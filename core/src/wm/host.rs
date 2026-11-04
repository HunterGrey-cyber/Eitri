//! The value contract between window decisions and native host services.

use super::moves::MoveKind;
use super::window::*;
use crate::agent_backend::AgentId;
use crate::keymap::{
    action::Action,
    key::{KeyName, KeySpec},
    prefix::Press,
};
use crate::layout::{kill::Reopen, Arrangement, Direction, Frame, ModuleId, Size};
use crate::onboarding::screen::{CheckWhy, MarkerEvent};
use crate::onboarding::{Found, Generated, Opening, WriteOutcome};
use crate::project_chooser::{LineEv, LinkKind, Listing, Row};
use crate::tabs::{TabId, ViewId};
use crate::window_link::moves::MoveId;
use crate::window_link::names::WindowName;
use crate::window_link::{FocusTarget, HubAsk, Msg, Shape, Summary};
use std::any::Any;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::os::fd::{OwnedFd, RawFd};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Native operations called on the UI thread by the driver.
pub trait Host {
    /// Performs an operation without blocking; refusals also post a refusal event.
    fn apply(&self, op: &HostOp) -> Applied;
    /// Returns the host's current outputs.
    fn monitors(&self) -> Vec<Monitor>;
}

/// Schedules later events without invoking callbacks synchronously.
pub trait Scheduler {
    /// Arms a timer at default priority.
    fn after(&self, delay: Duration, token: TimerToken);
    /// Arms an idle below painting.
    fn idle(&self, token: TimerToken);
    /// Cancels a timer or idle.
    fn cancel(&self, token: TimerToken);
    /// Watches a descriptor that remains open until unwatched.
    fn watch_fd(&self, fd: RawFd, interest: Interest, priority: Priority, token: WatchToken);
    /// Removes a descriptor watch.
    fn unwatch(&self, token: WatchToken);
    /// Reads the monotonic clock.
    fn now(&self) -> Instant;
    /// Supplies a thread-safe handle that requests a later wake event.
    fn waker(&self) -> Arc<dyn Waker>;
}

/// A worker's thread-safe route to the UI event queue.
pub trait Waker: Send + Sync {
    /// Requests a later wake event on the UI thread.
    fn wake(&self);
}

/// The callback interface held weakly by native hosts.
pub trait HostSink {
    /// Queues a native event until the active batch finishes.
    fn post(&self, event: HostEvent);
    /// Queues an event for a later idle.
    fn post_deferred(&self, event: HostEvent);
    /// Answers a native close request synchronously.
    fn close_requested(&self, win: WinId, by: CloseBy) -> CloseVerdict;
}

/// Pure decisions driven by the event queue.
pub trait Decide {
    /// Converts an event into a complete batch.
    fn handle(&mut self, event: HostEvent) -> Batch;
    /// Decides key routing at one capture stage.
    fn key(&mut self, win: WinId, stage: &KeyStage<'_>, press: &KeyPress) -> (Verdict, Batch);
    /// Decides whether a native close may proceed.
    fn close_requested(&mut self, win: WinId, by: CloseBy) -> (CloseVerdict, Batch);
    /// Runs an action through the shared action runner.
    fn run(&mut self, win: WinId, action: Action, origin: Origin) -> Batch;
    /// The driver's queue has drained: what is judged against the state every batch before it left (a chat page
    /// that came on screen during one, against the coverage they left). Asked each time the queue empties, so a
    /// decider with nothing to judge answers an empty batch and allocates nothing.
    fn after_drain(&mut self) -> Batch {
        Batch::default()
    }
}

/// The result of a native operation.
#[derive(Debug, Clone, PartialEq, Copy, Eq)]
pub enum Applied {
    /// The operation succeeded.
    Done,
    /// The host could not perform it.
    Refused,
}

/// The relative urgency of a descriptor watch.
#[derive(Debug, Clone, PartialEq)]
pub enum Priority {
    /// Input that should be handled promptly.
    Input,
    /// Ordinary service work.
    Default,
    /// Work that must yield to painting.
    BelowPaint,
}

/// The readiness condition for a descriptor watch.
#[derive(Debug, Clone, PartialEq)]
pub enum Interest {
    /// Readable input.
    Read,
    /// Writable output.
    Write,
}

/// An identity for a timer or idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimerToken(pub u64);

/// An identity for a descriptor watch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WatchToken(pub u64);

impl TimerToken {
    /// Reserved for draining the driver's deferred queue.
    pub const DRIVER_IDLE: Self = Self(u64::MAX);
}

/// A translated key with its prefix interpretation and native identity.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyPress {
    /// The hardware keycode.
    pub keycode: u32,
    /// The prefix machine interpretation.
    pub press: Press,
    /// The translated key before prefix folding, absent for unnamed keys.
    pub key: Option<KeyName>,
    /// The ordinary modifiers.
    pub mods: Mods,
    /// Whether a system modifier is held: Super or Hyper, which a page never sees on its own key events.
    pub super_or_hyper: bool,
    /// Whether Super or Meta is held: the system modifiers that keep `Ctrl+h/j/k/l` from moving the keys. Hyper
    /// alone does not.
    pub super_or_meta: bool,
}

/// The ordinary keyboard modifiers.
#[derive(Debug, Clone, PartialEq, Copy, Default)]
pub struct Mods {
    /// Control is held.
    pub ctrl: bool,
    /// Alt is held.
    pub alt: bool,
    /// Shift is held.
    pub shift: bool,
}

/// Where a captured key is being considered. A module's stage borrows the module's name, so a key on its way into
/// a module copies nothing, wherever the stage is built.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KeyStage<'a> {
    /// The window capture stage.
    Window,
    /// A module capture stage.
    Module(&'a ModuleId),
}

impl KeyStage<'_> {
    /// The module whose stage this is; `None` for the window's.
    pub fn module(&self) -> Option<&ModuleId> {
        match self {
            KeyStage::Window => None,
            KeyStage::Module(module) => Some(module),
        }
    }
}

/// How the host routes the key after deciding.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Stop delivery.
    Consumed,
    /// Continue ordinary delivery.
    PassThrough,
    /// Arm the page service for a second delivery.
    LetThrough,
}

/// The source of a native close request.
#[derive(Debug, Clone, PartialEq)]
pub enum CloseBy {
    /// An explicit user close.
    User,
    /// A close from the window manager.
    Compositor,
}

/// Whether a native close may continue.
#[derive(Debug, Clone, PartialEq)]
pub enum CloseVerdict {
    /// Allow the close.
    Proceed,
    /// Keep the window open.
    Stop,
}

/// The source of a shared runner action.
#[derive(Debug, Clone, PartialEq)]
pub enum Origin {
    /// A key with its native code.
    Key { keycode: u32 },
    /// A Lua call.
    Lua,
    /// A panel tab command.
    PanelTab,
    /// A chip activation.
    Chip,
    /// An automatic follow request.
    Follow,
    /// A linked process.
    Peer,
    /// An action window 1 sent over the link to a module window: it runs there as it is, never sent back.
    Hub,
}

/// The editor pane's liveness and last quit request, copied without waiting for its worker.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EditorFacts {
    pub running: bool,
    pub exited: bool,
    pub exit_pending: bool,
    pub starting: bool,
    pub watch: Option<super::close::NvimAnswer>,
}

/// Reads the controller makes of the shell directly, because each answers at once and none can call back into
/// the controller: the grid's size and measures, the editor's cell and whether it runs, and whether a chat page
/// claims a navigation key.
pub trait HostReads {
    /// Cached environment for the chooser; no environment or filesystem reads happen in the controller.
    fn project_chooser_env(&self, _win: WinId) -> ChooserEnv {
        ChooserEnv {
            home: None,
            lang: crate::onboarding::Lang::En,
        }
    }
    /// Terminals with a running foreground job, counted without waiting for a child.
    fn terminal_jobs(&self, win: WinId) -> usize {
        let _ = win;
        0
    }
    /// The running tabs and views a close can offer to the linked window.
    fn running_chats(&self, win: WinId) -> Vec<(ModuleId, Option<TabId>)> {
        let _ = win;
        Vec::new()
    }
    /// The terminal panes with foreground jobs, read without waiting for a child.
    fn terminal_job_modules(&self, win: WinId) -> Vec<ModuleId> {
        let _ = win;
        Vec::new()
    }
    /// The plugin pages whose guarded tasks still run.
    fn pages_with_tasks(&self, win: WinId) -> Vec<ModuleId> {
        let _ = win;
        Vec::new()
    }
    /// Base-view tabs in their displayed order, including tabs that currently cannot move.
    fn base_tabs(&self, win: WinId) -> Vec<TabId> {
        let _ = win;
        Vec::new()
    }
    /// The base view's active tab, which stays active in the other window when its workspace moves.
    fn active_base_tab(&self, win: WinId) -> Option<TabId> {
        let _ = win;
        None
    }
    /// The local tab adopted for a module move, after its host operation has run.
    fn arrived_tab(&self, win: WinId, mv: MoveId) -> Option<TabId> {
        let _ = (win, mv);
        None
    }
    /// A particular tab's move facts, so an unstarted tab can be named without a session id.
    fn tab_move(&self, win: WinId, tab: TabId) -> Result<ChatMoveFacts, String> {
        let _ = (win, tab);
        Err(NOT_CARRIED_OUT.into())
    }
    /// What a hand-over of `tab` to another window would take, which asks less of the tab than a move: its agent
    /// resumes in the other window, so it need not be able to move live. `Err` says why it cannot go.
    fn tab_hand_over(&self, win: WinId, tab: TabId) -> Result<TabHandOver, String> {
        let _ = (win, tab);
        Err(NOT_CARRIED_OUT.into())
    }
    /// The agent and provider session that name `tab` in the layout file's record of the tabs that came with a
    /// workspace; `None` for a tab that holds no session.
    fn moved_tab_key(&self, win: WinId, tab: TabId) -> Option<crate::window_link::base_tabs::MovedTabKey> {
        let _ = (win, tab);
        None
    }
    /// The pane's current liveness and quit watch, read without waiting for a worker.
    fn editor_facts(&self, win: WinId) -> EditorFacts {
        let _ = win;
        EditorFacts::default()
    }

    /// Whether stepping `delta` tabs along window `win`'s tab set would switch to another tab now, read before the
    /// step so a step with nowhere to go flashes instead. A host whose chats report none answers that it would not.
    fn tab_can_step(&self, win: WinId, delta: i32) -> bool {
        let _ = (win, delta);
        false
    }
    /// Whether window `win`'s tab set has a last tab to go back to that still belongs to its base view. A host
    /// whose chats report none answers that it has none.
    fn tab_has_last(&self, win: WinId) -> bool {
        let _ = win;
        false
    }
    /// What the compositor is called in a notice (`Hyprland`, `sway`, `niri`), or `the compositor` when none is known.
    fn compositor_name(&self) -> &'static str {
        "the compositor"
    }
    /// Whether the module window `name` is open or starting, whoever started it: a hand-over starts its target without
    /// the controller, so only the host knows of that one until it links. A host with no such windows has none open.
    fn module_window_open(&self, name: &crate::window_link::names::WindowName) -> bool {
        let _ = name;
        false
    }
    /// Where tab number `n` of window `win`'s tab set is now (the base view or the extra view showing it), or
    /// `None` when there is no such tab. A host whose chats report none answers none.
    fn tab_place(&self, win: WinId, n: u16) -> Option<crate::tab_set::TabPlace> {
        let _ = (win, n);
        None
    }

    /// The grid's allocation, or the stand-in content size before its first one.
    fn grid_size(&self, win: WinId) -> Size;
    /// The divider thickness as the grid's dividers measure.
    fn handle_px(&self, win: WinId) -> i32;
    /// A container strip's row height as the grid's strips measure.
    fn strip_px(&self, win: WinId) -> i32;
    /// A module's minimum size as its host measures.
    fn module_min(&self, win: WinId, module: &ModuleId) -> Size;
    /// The editor's cell size in logical pixels, when it can tell.
    fn editor_cell(&self, win: WinId) -> Option<(f64, f64)>;
    /// Whether the editor's child is running.
    fn editor_running(&self, win: WinId) -> bool;
    /// Whether the chat page `view` keeps a navigation key for itself.
    fn chat_claims(&self, view: ViewId, direction: Direction) -> bool;
    /// The size `module`'s host has on screen now, which a move offers so the other window lays it out at that
    /// size; `None` when the host cannot tell (no host, not laid out yet).
    fn module_size(&self, win: WinId, module: &ModuleId) -> Option<Size> {
        let _ = (win, module);
        None
    }
    /// What a move of a tab of chat view `view` would take: the view's one tab, or for the base view the tab
    /// holding `session` when one is named, else its active tab; with that tab's session, whether its agent keeps
    /// a running turn across a move, and whether a turn runs. The refusal's words when the tab cannot move now (a
    /// message steered into its turn unread, a session not bound to its review, an answer only this window can
    /// use still awaited, a move already under way), read before anything is offered. A host whose chats cannot
    /// move refuses.
    fn chat_move(&self, win: WinId, view: ViewId, session: Option<&str>) -> Result<ChatMoveFacts, String> {
        let _ = (win, view, session);
        Err(NOT_CARRIED_OUT.into())
    }
    /// The chat view of window `win` whose tabs hold the session `session`, if one does. A host whose chats
    /// report none answers none.
    fn view_of_session(&self, win: WinId, session: &str) -> Option<ViewId> {
        let _ = (win, session);
        None
    }
    /// What the chat view `view` holds for the user (cards waiting, a turn finished unseen), as its chip and its
    /// strip entry show it. A host whose chats report none answers nothing held.
    fn view_attention(&self, view: ViewId) -> crate::attention::Attention {
        let _ = view;
        crate::attention::Attention::default()
    }
    /// The chat view whose tabs hold the oldest card waiting for an answer, which the chat's key reaches when the
    /// keys are in no chat view. A host whose chats report none answers none.
    fn oldest_card_view(&self, win: WinId) -> Option<ViewId> {
        let _ = win;
        None
    }
    /// What killing the chat view `view` ends, as its y/n names it. A host whose chats report none answers that
    /// nothing runs.
    fn chat_kill_counts(&self, view: ViewId) -> ChatKillCounts {
        let _ = view;
        ChatKillCounts::default()
    }
    /// Whether the new chat view `view` can be given its first tab now -- a new empty one, or the base view's active
    /// tab brought out when `breaks` -- asked before the view is placed, so a refusal places nothing. The refusal's
    /// words when it cannot. A host whose chats report none answers that it can.
    fn chat_first_tab(&self, view: ViewId, breaks: bool) -> Result<(), String> {
        let _ = (view, breaks);
        Ok(())
    }
    /// Whether this window has actually built the plugin page; a deferred holding has nothing to save.
    fn page_built(&self, _win: WinId, _module: &ModuleId) -> bool {
        false
    }

    /// The `?` sheet's section for the plugin page `module`, when `module` is one that declared its keys; read
    /// before the chat takes the keys, so the sheet can show the keys of the page they came from. A host with no
    /// plugin pages answers none.
    fn plugin_keys_help(&self, win: WinId, module: &ModuleId) -> Option<crate::plugin::keys::PluginKeysHelp> {
        let _ = (win, module);
        None
    }
    /// The chat views that hold a tab now, the base view among them. A host whose chats report none answers none.
    fn chat_views(&self) -> Vec<ViewId> {
        Vec::new()
    }
    /// Whether the base view's active tab can go out into a view of its own now, asked before the view is made so
    /// a break that cannot happen makes nothing; the refusal's words when it cannot. A host that cannot tell
    /// refuses.
    fn can_break_out(&self, win: WinId) -> Result<(), String> {
        let _ = win;
        Err(NOT_CARRIED_OUT.into())
    }
    fn chat_summary(&self, win: WinId) -> ChatSummary {
        let _ = win;
        ChatSummary::default()
    }
}

/// What the controller needs of a chat tab to move it ([`HostReads::chat_move`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMoveFacts {
    /// The tab that moves.
    pub tab: TabId,
    /// Its agent's own session id, which the other window's chooser and the editor's review name it by; `None`
    /// for a tab that holds no session yet.
    pub session: Option<String>,
    /// Whether its agent keeps a running turn when the chat moves; when it does not, a running chat moves once
    /// its turn ends.
    pub live: bool,
    /// Whether a turn of it runs now.
    pub running: bool,
}

/// What the controller needs of a chat tab to hand it to another window ([`HostReads::tab_hand_over`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabHandOver {
    /// Its agent: session ids are each agent's own, so this and `session` together name the tab.
    pub agent: AgentId,
    /// The session the other window resumes; `None` for a tab that holds none.
    pub session: Option<String>,
    /// Whether a turn of it runs now.
    pub running: bool,
}

/// Window-wide chat counts, read without calling back into the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChatSummary {
    pub waiting: u32,
    pub unseen: u32,
    pub running: u32,
    pub queued: u32,
}

/// What a host answers for an operation or a read it does not carry out yet.
pub const NOT_CARRIED_OUT: &str = "not carried out by this shell yet";

/// What a chat view's kill ends: the turns running and the messages queued (for the base view, every tab's,
/// since its kill closes them all; for an extra view, its one tab's), an extra view's tab label, which its y/n
/// names, and whether its tabs can close now.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChatKillCounts {
    /// Tabs with live non-ambient background work.
    pub background: usize,
    pub running: usize,
    pub queued: usize,
    /// The window's tabs whose unsent text would go with it ([`crate::tab_set::TabSet::unsaved_draft_count`]). Only the
    /// base view's counts carry it: a close counts the whole window once.
    pub drafts: usize,
    pub tab: Option<String>,
    /// Why the view's tabs cannot close now (one is being handed off to a terminal, or moving to the other window),
    /// in the words the kill is refused with: a kill that left the view's tabs open would not be a kill, so the
    /// view stays where it is.
    pub blocked: Option<String>,
}

/// What a controller reads before the shell gives it its reads: the stand-in grid of a window not yet
/// allocated, the thinnest divider, the default strip, no minimums, no editor and no claims.
pub struct NoReads;

impl HostReads for NoReads {
    fn grid_size(&self, _: WinId) -> Size {
        super::grid::UNALLOCATED
    }
    fn handle_px(&self, _: WinId) -> i32 {
        1
    }
    fn strip_px(&self, _: WinId) -> i32 {
        super::grid::DEFAULT_STRIP_PX
    }
    fn module_min(&self, _: WinId, _: &ModuleId) -> Size {
        Size::default()
    }
    fn editor_cell(&self, _: WinId) -> Option<(f64, f64)> {
        None
    }
    fn editor_running(&self, _: WinId) -> bool {
        false
    }
    fn chat_claims(&self, _: ViewId, _: Direction) -> bool {
        false
    }
}

/// Runs `f` with the frame `win`'s geometry is computed in at `size`, its measures read from the shell.
pub fn frame_with<R>(reads: &dyn HostReads, win: WinId, size: Size, f: impl FnOnce(&Frame) -> R) -> R {
    let min = |module: &ModuleId| reads.module_min(win, module);
    f(&Frame {
        size,
        handle_px: reads.handle_px(win),
        strip_px: reads.strip_px(win),
        min: &min,
    })
}

/// The grid's way to its window's arrangement: the size the toolkit just gave it in, the arrangement out,
/// computed at once inside the allocation, and the minimum it reports when measured.
pub trait Allocator {
    /// Settles the pinned splits shown for the first time and arranges the layout at `size`.
    fn allocate(&self, win: WinId, size: Size) -> Rc<Arrangement>;
    /// What the layout needs at the least.
    fn min_size(&self, win: WinId) -> Size;
}

/// One complete native or service operation.
#[derive(Debug, Clone, PartialEq)]
pub enum HostOp {
    /// Work performed outside the project chooser's model.
    Project(ProjectOp),
    /// Creates a window without showing it.
    OpenWindow { win: WinId, spec: WindowSpec },
    /// Presents a window using optional native authorization.
    Present {
        win: WinId,
        activation: Option<ActivationToken>,
    },
    /// What the window says to the [`HostEvent::OpenFile`] or [`HostEvent::RaiseWindow`] of this `id`: `false` sends
    /// its client to a new window.
    OpenAnswer { id: u64, accepted: bool },
    /// Sets window presentation flags: the state the window's mode toggle (`window.immersive`) leads to. A host
    /// whose window also changes mode without the controller hearing (a bare F11, the editor's own setting, the
    /// compositor) carries it out as that toggle, since the state asked for may be out of date there.
    SetMode { win: WinId, mode: WindowMode },
    /// Places the mapped window on its remembered output, if needed, and applies its role's focus.
    PlaceWindow {
        win: WinId,
        monitor: Option<MonitorId>,
        role: PlacementRole,
    },
    /// Starts the project's second window again at this window's start, as it was when the pair last ended: with no
    /// activation token. Its placement role returns the keys here on sway and Hyprland after mapping. A refusal
    /// means window 2 is not coming back; the host has then given back here what it held for window 2.
    RestoreLinkedWindow { win: WinId },
    /// Starts the project's editor window (`eitri <root>`) from a window 2 that has no window 1, detached from this
    /// process: once up, it links to this window again. A refusal says why none was started.
    StartEditorWindow { win: WinId },
    /// Closes without asking again.
    CloseWindow { win: WinId },
    /// Ends the application.
    Quit,
    /// Creates a module surface. Carried out, the driver answers it with [`HostEvent::Created`]: a new instance is
    /// placed in the layout only once its surface exists.
    CreateSurface {
        win: WinId,
        module: ModuleId,
        kind: SurfaceKind,
    },
    /// Releases a module surface. A terminal's pane or a chat view's page is left idle in the host's pool under its
    /// id, its host kept, for the next instance of its number, which comes back with no create.
    DropSurface { win: WinId, module: ModuleId, why: DropWhy },
    /// Lays out a module arriving from the other window off screen, at the size and shape it has there, before
    /// anything of it is moved: made before it is mapped. Carried out, the driver answers it with
    /// [`HostEvent::Prepared`].
    PrepareSurface {
        win: WinId,
        module: ModuleId,
        shape: Shape,
        size: (u32, u32),
    },
    /// Starts the project's second window, or raises it when it runs.
    OpenLinkedWindow { win: WinId },
    /// Brings the other window of the pair forward through the compositor; answered with [`PeerEvent::Raised`]
    /// carrying the activation token the host got for it, if any.
    RaisePeer { win: WinId, peer: PeerId },
    /// Starts the module window `name`, which links back when it is up. `token` is the activation token of the window
    /// that asked for it (`None`: the host makes one from this window's surface); with `keep_keys` the new window maps
    /// without taking the keys.
    OpenModuleWindow {
        win: WinId,
        name: WindowName,
        token: Option<String>,
        keep_keys: bool,
    },
    /// A raise of `peer` that a module window asked for: it is carried out with that window's `token`, and answered
    /// with [`PeerEvent::Raised`] as [`HostOp::RaisePeer`] is.
    ForwardRaise {
        win: WinId,
        peer: PeerId,
        token: Option<String>,
    },
    /// Runs the key action `name` with `opts` in the module window `peer` (`Action::name` and
    /// `Action::wire_options`).
    PeerRun {
        win: WinId,
        peer: PeerId,
        name: String,
        opts: Vec<(String, serde_json::Value)>,
    },
    /// A focus move past this window's edge, which the compositor carries out.
    CompositorFocus { win: WinId, direction: Direction },
    /// A module window asks window 1 for another window of its own.
    HubAsk { win: WinId, ask: HubAsk },
    /// What a window beside the person's own nvim asks of the windows around it: it belongs to no group, so the
    /// host carries it out on its own, by the nvim the window follows.
    Beside(BesideOp),
    /// Builds the window's base chat page, once, at the start step the mode's facts allow. The only way one comes to
    /// exist: a window that shows no chat of its own never gets this op.
    BuildBasePage { win: WinId },
    /// Hands the tab holding `agent`'s `session` (session ids are each agent's own, so the two together name the tab)
    /// from the module window `from` to the chat window `to`, through this window: `to` is started or raised with the
    /// asking window's `token`, and once it has linked `from` is asked to let the tab go. The host answers everything
    /// that follows between the windows' links.
    HandOverTab {
        win: WinId,
        from: PeerId,
        agent: AgentId,
        session: String,
        to: WindowName,
        token: Option<String>,
    },
    /// Tells the module window `peer` that the tab holding `session` stays there, for the reason `why`.
    KeepTab {
        win: WinId,
        peer: PeerId,
        session: String,
        why: String,
    },
    /// Builds the prepared surface `module` from the descriptors and the record a committed move carried. The
    /// descriptors are the op's alone: the host takes them out of `fds` and owns them from then on.
    AdoptMoved {
        win: WinId,
        module: ModuleId,
        kind: MoveKind,
        record: serde_json::Value,
        fds: Descriptors,
    },
    /// Orders a workspace's arriving base tabs after the tabs this window already held.
    OrderBaseTabs {
        win: WinId,
        tabs: Vec<TabId>,
        active: Option<TabId>,
    },
    /// Ends the other window's process, which this window started: what the user asked when that window stopped
    /// answering in the middle of a move. The link's end then settles the move as any end does.
    EndPeer { win: WinId, peer: PeerId },
    /// Makes a module available on screen.
    Map { win: WinId, module: ModuleId },
    /// Hides a module after its keys have moved.
    Unmap { win: WinId, module: ModuleId },
    /// Requests allocation from the current layout.
    Allocate { win: WinId },
    /// Moves siblings in place without ever naming the editor.
    Restack { win: WinId, moves: Vec<RestackMove> },
    /// Transfers keys and runs the precomputed tail only on success.
    Focus {
        win: WinId,
        to: FocusTo,
        on_grab: Vec<HostOp>,
    },
    /// Reconciles chrome from the indicated model revision.
    Chrome { win: WinId, rev: u64 },
    /// Changes number hints.
    Hint(HintOp),
    /// Calls an editor service.
    Editor(EditorCall),
    /// Calls a chat service.
    Chat(ChatOp),
    /// Calls a terminal service.
    Terminal(TermOp),
    /// Calls a plugin service.
    Plugin(PluginOp),
    /// Removes process-wide resources.
    Cleanup(CleanupOp),
    /// Tells the pair file and the pages what a change of the pair left.
    Pair(PairOp),
    /// Asks for the first-start screen's work: the checks, the detection, the write, the marker.
    Onboarding(OnboardingOp),
    /// Runs Lua after all other operations.
    Lua(LuaOp),
}

/// What a window beside the person's own nvim asks the host for. Each is carried out off the UI thread, and a
/// refusal comes back as [`HostEvent::Refused`] with [`OpTag::Beside`], its text the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BesideOp {
    /// The project's agent window comes forward, or one is started for the nvim this window follows.
    Agent,
    /// The page window of this plugin id, for the same nvim, is started or comes forward.
    Page(String),
    /// The window that holds the nvim this window follows comes forward.
    RaiseEditor,
    /// This window comes forward.
    RaiseSelf,
}

/// What the first-start screen asks the host to do. Every op of a screen carries the generation the controller
/// opened it under, which the host hands back on its answer, so an answer that arrives after the screen closed
/// (or for an earlier screen) is dropped by the controller.
#[derive(Debug, Clone, PartialEq)]
pub enum OnboardingOp {
    /// Decides on a worker whether the screen opens, and answers [`OnboardingEvent::Open`] when it does. `requested`
    /// is true when the user asked (`window.setup`, `:Eitri setup`): a screen asked for opens whether or not the
    /// start would have shown it.
    Check {
        win: WinId,
        generation: u64,
        requested: bool,
    },
    /// Starts the detection rows; each answers [`OnboardingEvent::Found`].
    Detect { win: WinId, generation: u64 },
    /// Shows a one-time notice, after whatever is up has hidden.
    Notice { win: WinId, text: String },
    /// Reads Neovim's mappings again, probing `extra` as well for that one report; the answer is a
    /// [`OnboardingEvent::Found`].
    Rescan {
        win: WinId,
        generation: u64,
        extra: Vec<KeySpec>,
    },
    /// Moves the screen's text.
    Scroll { win: WinId, by: crate::onboarding::Scroll },
    /// Checks and writes `init.lua`; answers [`OnboardingEvent::Written`] or [`OnboardingEvent::CheckFailed`].
    Write {
        win: WinId,
        generation: u64,
        generated: Generated,
    },
    /// Records that the screen was left without writing, in the marker file; `today` is the date it carries.
    Marker {
        win: WinId,
        event: MarkerEvent,
        today: String,
    },
    /// Shows these lines in a scratch buffer in the editor.
    OpenLines { win: WinId, lines: Vec<String> },
    /// Starts the tutor in the editor.
    OpenTutor { win: WinId },
}

/// A plugin manifest identity, represented as in the plugin registry.
pub type PluginId = String;

/// The closed set of module surface descriptions.
#[derive(Debug, Clone, PartialEq)]
pub enum SurfaceKind {
    /// An editor with its complete launch settings.
    Editor { spawn: EditorSpawn },
    /// A chat view.
    Chat { view: ViewId },
    /// A registered Lua page.
    LuaPanel { url: String, title: String },
    /// A page from a loaded plugin.
    PluginPage { plugin: PluginId },
    /// A terminal starting in a directory.
    Terminal { cwd: PathBuf },
}

/// All inputs needed to launch an editor.
#[derive(Debug, Clone, PartialEq)]
pub struct EditorSpawn {
    /// The child environment entries.
    pub env: Vec<(String, String)>,
    /// The child arguments.
    pub args: Vec<String>,
    /// The optional working directory.
    pub cwd: Option<PathBuf>,
    /// Whether to start without user configuration.
    pub clean: bool,
    /// The colour the editor paints before nvim's UI is ready, from the remembered theme; `None` keeps the editor's
    /// own.
    pub background: Option<(u8, u8, u8)>,
    /// What nvim opens at start, as the arguments it takes after its options (`+N`, then absolute paths); empty
    /// opens nothing. Only the first start carries it: the controller keeps these apart from the spawn it restarts
    /// with, and puts them back only until the editor has been ready once.
    pub open: Vec<String>,
}

/// Why a module surface is released.
#[derive(Debug, Clone, PartialEq)]
pub enum DropWhy {
    /// The module was killed.
    Killed,
    /// The module is no longer needed.
    Retired,
    /// A move from the other window was given up before it committed: what was prepared for it goes.
    MoveGivenUp,
}

/// One sibling insertion that preserves native parentage.
#[derive(Debug, Clone, PartialEq)]
pub struct RestackMove {
    /// The sibling to move; never the editor.
    pub module: ModuleId,
    /// The preceding sibling, or the start of the band.
    pub after: Option<ModuleId>,
}

/// The possible destinations for keys.
#[derive(Debug, Clone, PartialEq)]
pub enum FocusTo {
    /// The project chooser's line, reported as non-module focus.
    ProjectChooser,
    /// A module surface.
    Module(ModuleId),
    /// An indexed top-bar item.
    TopBarItem(u32),
    /// The rename entry.
    RenameEntry,
    /// The first-start screen. No [`FocusOwner`] reports it: the host reports non-module focus as no owner.
    Onboarding,
    /// No destination.
    Nothing,
}

/// The number hints to display.
#[derive(Debug, Clone, PartialEq)]
pub enum HintOp {
    /// Shows a digit on each addressed module.
    ShowDigits(Vec<(ModuleId, u8)>),
    /// Hides the digits.
    Hide,
    /// Starts or ends the hint session from the prefix, with the code of the key still held down.
    Toggle { keycode: u32 },
}

/// Proof that the editor a call ends or restarts is one Eitri started. [`owned`] is its one constructor, and it
/// gives none for a person's own nvim, so no quit, end or restart can be built for an attached editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedEditor(());

/// The token for an editor of `kind`: `None` for an attached one, which nothing may quit, end or restart.
pub fn owned(kind: EditorKind) -> Option<OwnedEditor> {
    match kind {
        EditorKind::Owned => Some(OwnedEditor(())),
        EditorKind::Attached => None,
    }
}

/// A token for tests that build the calls it guards by hand.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn owned_editor_for_tests() -> OwnedEditor {
    owned(EditorKind::Owned).expect("an owned editor has a token")
}

/// How long a detach gives the glue's teardown in a person's nvim before it hangs up anyway.
pub const DETACH_TEARDOWN: Duration = Duration::from_millis(300);

/// How long a close waits for the waiting room to answer that it has taken the nvim back.
pub const DETACH_ROOM_WAIT: Duration = Duration::from_secs(1);

/// How long a close waits for [`EditorEvent::Detached`] before it tears the window down anyway: the detach's own
/// bounds, and half a second for the answer to come back.
pub const DETACHED_WITHIN: Duration = Duration::from_millis(1_800);

/// What a close has to ask about a person's own nvim before it lets go of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CloseAsks {
    /// nvim is waiting for a key or busy (`nvim_get_mode().blocking`): given back now, the terminal would show a
    /// blank screen until a key, which would answer whatever nvim is asking.
    pub blocking: bool,
    /// Drafts being edited in the editor with unsaved changes; they end with the window.
    pub drafts: usize,
    /// Agent proposals open in the editor; they end with the window.
    pub proposals: usize,
}

/// Commands sent to the editor service.
#[derive(Debug, Clone, PartialEq)]
pub enum EditorCall {
    /// Sets editor focus.
    SetFocused { win: WinId, focused: bool },
    /// Asks the editor to quit through Lua.
    Quit {
        win: WinId,
        lua: String,
        generation: u32,
        editor: OwnedEditor,
    },
    /// Releases an exited pane on the next turn of the loop, after its exit callback returns.
    ReleaseExited { win: WinId },
    /// Ends the editor session.
    End { win: WinId, editor: OwnedEditor },
    /// Shuts down the editor service.
    Shutdown { win: WinId },
    /// Sets editor scale.
    SetScale { win: WinId, scale: f64 },
    /// Reveals the editor.
    Reveal { win: WinId },
    /// Restarts with complete launch settings.
    Restart {
        win: WinId,
        spawn: EditorSpawn,
        editor: OwnedEditor,
    },
    /// Lets go of a person's own nvim: the glue's teardown, given at most [`DETACH_TEARDOWN`], then the hang-up,
    /// then the waiting room is told to take it back. Answered by [`EditorEvent::Detached`].
    Detach { win: WinId },
    /// Asks what a close has to ask about a person's own nvim, without waiting on it; answered by
    /// [`EditorEvent::CloseFacts`] carrying the same `generation`, so an answer to an earlier close that comes late
    /// is never taken for this one's.
    AskCloseFacts { win: WinId, generation: u32 },
    /// Types keys into the editor, in the editor's own key notation.
    SendKeys { win: WinId, keys: String },
    /// Opens a file in the running editor. `line` and `col` count from one, `col` in bytes; a line past the end is
    /// the last line. What it comes to is reported as [`EditorEvent::OpenFailed`] when it does not open.
    Open {
        win: WinId,
        path: std::path::PathBuf,
        line: Option<u32>,
        col: Option<u32>,
    },
    /// Runs the reopen capture in the running editor with `args` (the token, the project root and the prefixes to
    /// leave out) and reports its answer, however late, as one [`EditorEvent::Captured`] carrying `token`.
    Capture {
        win: WinId,
        token: String,
        args: rmpv::Value,
    },
    /// Has the window's record writer read the record the nvim of `token` left at its exit, waiting for it a short
    /// while, and hand it over with `seq`; the writer answers once per token, record or none.
    ReadHandoff { win: WinId, token: String, seq: u64 },
    /// Removes the input an earlier nvim of the window was started with from the channel directory, so a restart
    /// whose own input the record writer has not written in time starts with none (nvim then reopens nothing and
    /// keeps nothing) rather than reading another nvim's token and restore.
    DropInput { win: WinId },
}

/// Commands sent to the chat service.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatOp {
    /// Updates the chat font size in pixels.
    SetFontSize { win: WinId, px: f32 },
    /// One panel tab call, decided from the panel's read-only answers.
    Tab {
        win: WinId,
        call: super::verbs::tabs::TabCall,
    },

    /// Sets pane focus.
    PaneFocused { win: WinId, view: ViewId, focused: bool },
    /// Notifies a shown page.
    PageShown { win: WinId, view: ViewId },
    /// The keys are in the chat after the editor's "edit the selection" key: its shown tab opens the composer with
    /// the edit prompt ready, where an arrival would land in BROWSE.
    EditSelection { win: WinId, view: ViewId },
    /// Completes arrival in a view. With `card_first` (a module key, a strip entry, a HINT landing, the window's
    /// first focus) the view's oldest pending card takes the keys if one waits; without it (`Ctrl+h/j/k/l`,
    /// `select.last`, `select.next`, a digit) the view lands where its reader left it, card or not.
    Arrive { win: WinId, view: ViewId, card_first: bool },
    /// Delivers directional navigation.
    NavKey {
        win: WinId,
        view: ViewId,
        direction: Direction,
    },
    /// Closes tabs in a view.
    CloseTabs { win: WinId, view: ViewId },
    /// Opens a new empty tab of the window's default agent in a view: a new chat view's first tab, before the
    /// view is placed.
    OpenTab { win: WinId, view: ViewId },
    /// The extra chat views the window's workspaces still hold, after one left them: a restore still to come must
    /// not bring a tab back into a view that has gone.
    LayoutViews { win: WinId, views: Vec<ViewId> },
    /// Breaks a tab out of its view.
    BreakOut { win: WinId, view: ViewId },
    /// Brings the tab of extra view `view` back into the base view, which then arrives where its reader left it.
    /// The host carries it out whole; a refusal comes back as [`HostEvent::Refused`] naming `view`'s module, with the
    /// panel's words, which the chrome says.
    JoinBack { win: WinId, view: ViewId },
    /// Opens the `?` sheet in `view`'s page: `plugin` is the section of the plugin page that held the keys when the
    /// key was pressed, read before the chat took them, and `opening` the number its close quotes back
    /// ([`ChatEvent::KeymapClosed`]).
    OpenKeymap {
        win: WinId,
        view: ViewId,
        plugin: Option<crate::plugin::keys::PluginKeysHelp>,
        opening: u64,
    },
    /// Opens the command line.
    CommandLine { win: WinId, view: ViewId },
    /// Reloads a view.
    Reload { win: WinId, view: ViewId },
    /// Sets views whose answers are covered.
    SetCoverage { win: WinId, covered: Vec<ViewId> },
    /// Notifies views that became uncovered.
    TellUncovered { win: WinId, views: Vec<ViewId> },
    /// Shuts down all chats with optional hold.
    ShutdownAll { win: WinId, hold: bool },
    /// Opens the tab chooser.
    OpenChooser { win: WinId, view: ViewId },
    /// Delivers one key, as the key itself, to a view's page.
    LiteralKey { win: WinId, view: ViewId, key: KeySpec },
    /// Tells the panel whether the editor holds the keys of its typing window.
    EditorHasKeys { win: WinId, has_keys: bool },
    /// Freezes tab `tab` of view `view` (the view's one tab when `None`) for move `mv`: it takes no input and
    /// answers no card, its page is asked for its view state, and the host answers with [`ChatEvent::Frozen`]
    /// carrying the same `mv`, the record and the handoff, or [`ChatEvent::FreezeRefused`]. The id goes both ways
    /// because a tab may be frozen for a move that has ended and again for the next before the first answer comes.
    Freeze {
        win: WinId,
        view: ViewId,
        tab: Option<TabId>,
        mv: MoveId,
    },
    /// The other window holds the frozen tab now: it leaves this window without its session being ended or
    /// anything the handoff passed on being closed.
    Forget {
        win: WinId,
        view: ViewId,
        tab: Option<TabId>,
    },
    /// The move did not happen: the frozen tab takes input and answers cards again.
    Resume {
        win: WinId,
        view: ViewId,
        tab: Option<TabId>,
    },
    /// The editor's review events of session `session` are held (`hold`) while its chat moves, or, once the move
    /// has ended, delivered to the window that holds the session now: this one when `here`, else the other.
    ReviewHold {
        win: WinId,
        session: String,
        hold: bool,
        here: bool,
    },
    /// Before `move_done` of move `mv`: this window's presence counts the arriving session's running turn and
    /// confirms its file, answered with [`ChatEvent::PresenceHeld`] or [`ChatEvent::PresenceRefused`].
    HoldPresence { win: WinId, session: String, mv: MoveId },
    /// Move `mv`'s arrival was given up: what [`ChatOp::HoldPresence`] counted for `session` is let go.
    DropPresence { win: WinId, session: String, mv: MoveId },
}

/// Commands sent to terminal services.
#[derive(Debug, Clone, PartialEq)]
pub enum TermOp {
    /// Sets the text zoom of every terminal of the window, relative to its un-zoomed font; the
    /// host keeps it for terminals made later and makes an unchanged level a no-op.
    SetZoom { win: WinId, level: f32 },
    /// Starts a terminal.
    Start { win: WinId, module: ModuleId },
    /// Sets the directory the next shell of a terminal starts in: a new instance on a pane a kill left idle asks no
    /// create, which is what carries a new terminal's directory, so this does; a live pane with no shell running (the
    /// base terminal never started, or killed, which keeps its pane live, or its shell ended) only takes the directory,
    /// for its next start: a [`TermOp::Start`], a spawn that waited for the pane's size, or a restart on Enter.
    SetStartDir { win: WinId, module: ModuleId, dir: PathBuf },
    /// Sets terminal focus.
    SetFocused {
        win: WinId,
        module: ModuleId,
        focused: bool,
    },
    /// Hangs up the terminal.
    HangUp { win: WinId, module: ModuleId },
    /// Reads the directory the terminal's shell is in now, off the toolkit's thread, for a split that starts its
    /// new shell there; answered with [`TermEvent::Cwd`] carrying the same `read`, since reads of one module (a
    /// number reused meanwhile, two splits in a row) may come back in any order.
    ReadCwd { win: WinId, module: ModuleId, read: u64 },
    /// Reads, off the toolkit's thread, the directory the terminal's shell is in and whether any process but the
    /// shell runs in its session, after capturing which shell the pane runs ([`ShellStart`]); answered with
    /// [`TermEvent::Job`] carrying the same `read`. No fallback directory: a directory that cannot be read is an
    /// `Err`, since a guess would read as "elsewhere" and restart a shell whose directory is unknown.
    ReadJob { win: WinId, module: ModuleId, read: u64 },
    /// Ends the terminal's shell and starts a fresh one in `dir`, in the same pane, in one step: the pane, its place,
    /// its size and whether it holds the keys stay as they were. Only when the pane still runs the shell `expect`
    /// names and was given no key or mouse report for it since the capture ([`ShellStart::untouched`]); otherwise
    /// refused as [`OpTag::Respawn`] with nothing touched. The new shell reports [`TermEvent::Started`].
    Respawn {
        win: WinId,
        module: ModuleId,
        dir: PathBuf,
        expect: ShellStart,
    },
    /// Enters copy mode, scrolled up a page when `up`.
    CopyMode { win: WinId, module: ModuleId, up: bool },
    /// Sends terminal keys.
    SendKeys { win: WinId, module: ModuleId, keys: String },
    /// Delivers one key, as the key itself; the terminal's engine encodes it, or says it has no encoding.
    LiteralKey { win: WinId, module: ModuleId, key: KeySpec },
    /// Freezes a terminal for move `mv`: the session stops reading and writing, its pane stays on screen as it is,
    /// and the host answers with [`TermEvent::Detached`] carrying the same `mv`, its record and its handoff. The
    /// move's id goes both ways because one module may be frozen for a move that has ended and again for the next
    /// before the first answer arrives, and only the id tells the answers apart.
    Detach { win: WinId, module: ModuleId, mv: MoveId },
    /// The other window holds the terminal now: the local copy is dropped without hanging up its shell or
    /// closing anything the handoff passed on.
    Forget { win: WinId, module: ModuleId },
    /// The move did not happen: the frozen terminal reads and writes again.
    Resume { win: WinId, module: ModuleId },
    /// Shuts down every terminal.
    ShutdownAll { win: WinId },
}

/// Facts from a plugin page's move and its first restored frame.
#[derive(Debug, Clone, PartialEq)]
pub enum PluginEvent {
    /// The page was frozen for move `mv`: its tasks paused, `record` what moves with it (its saved state `null`
    /// after the one-second bound or for a page without a handoff) and `handoff` its tasks' descriptors.
    Frozen {
        win: WinId,
        module: ModuleId,
        mv: crate::window_link::moves::MoveId,
        record: serde_json::Value,
        handoff: Handoff,
    },
    /// The page could not be frozen for move `mv`; `why` says why. Only that move is given up.
    FreezeRefused {
        win: WinId,
        module: ModuleId,
        mv: crate::window_link::moves::MoveId,
        why: String,
    },
    /// The page that arrived here said `page.ready`: its restored document is drawn and it may be mapped.
    Ready { win: WinId, module: ModuleId },
}

/// Commands sent to plugin services.
#[derive(Debug, Clone, PartialEq)]
pub enum PluginOp {
    /// Pauses task readers and asks the page for state, bounded by one second; the move id matches its answer.
    Freeze {
        win: WinId,
        module: ModuleId,
        mv: crate::window_link::moves::MoveId,
    },
    /// Releases a page whose tasks are now held by the other window, without writing its store.
    Forget { win: WinId, module: ModuleId },
    /// Takes a paused page's tasks back after a move ends without sending it.
    Resume { win: WinId, module: ModuleId },
    /// Reopens a page's holding and its follow and command routing when it arrives or returns. A kept view
    /// reloads here; a page with no view is built on its first show.
    Hold { win: WinId, module: ModuleId },
    /// Updates the whole-page zoom of an attached view. The controller keeps every page that ever
    /// had a view in its zoom set (a page killed and rebuilt keeps its factor), so this can name a
    /// module whose view is gone: the host then does nothing, quietly, rather than refusing.
    SetZoom { win: WinId, module: ModuleId, level: f32 },
    /// Ends every plugin task and page owned by this window.
    ShutdownAll { win: WinId },
    /// Tells pages following the editor that it has closed.
    EditorClosed { win: WinId },
    /// Reinstalls editor-following hooks into the new nvim session.
    EditorBack { win: WinId },
    /// Reloads with optional guard reset.
    Reload {
        win: WinId,
        module: ModuleId,
        reset_guard: bool,
    },
    /// Terminates a plugin page.
    Terminate { win: WinId, module: ModuleId },
    /// Builds a deferred page.
    Build { win: WinId, module: ModuleId },
    /// Reports page visibility.
    OnScreen { win: WinId, module: ModuleId, on: bool },
    /// Tells a page whether it holds the keys.
    HasKeys {
        win: WinId,
        module: ModuleId,
        has_keys: bool,
    },
    /// The answer to plugin `plugin`'s `terminal.at` asked under `ticket`, for its host to give the page.
    TerminalAtAnswer {
        win: WinId,
        plugin: String,
        ticket: u64,
        answer: crate::plugin::host::TerminalAtAnswer,
    },
}

/// Process-wide resources to remove.
#[derive(Debug, Clone, PartialEq)]
pub enum CleanupOp {
    /// Window 1 in `compositor` mode closes its group: every module window hears the link's goodbye, and one that has
    /// not gone after a grace is ended.
    EndGroup,
    /// Window 1 in `compositor` mode was asked to close, whatever it answers: the module windows that go around this
    /// moment went with it, so what it keeps for them stays for the next start.
    GroupAsked,
    /// An end signal, not the user, closes this module window: its goodbye says so, and window 1 keeps its tabs and
    /// its place for the next start. Comes ahead of [`CleanupOp::PairOnClose`].
    KeepThisWindow,
    /// Flushes the pair's last state and sends the link's goodbye after the layout save.
    PairOnClose { layout_written: bool },
    /// The editor feeds.
    Feeds,
    /// The pane-switch shim directory.
    PaneSwitchDir,
    /// The process scratch directory.
    ScratchDir,
    /// The editor's record, after nvim was told to end and before the channel directory nvim leaves its last record
    /// in is removed: that record is read (when `token` is `Some`; a token whose read was asked already is `None`)
    /// and handed over with `seq`, then the record writer is waited for, bounded, so what it holds is on disk.
    EditorRecord { token: Option<String>, seq: u64 },
}

/// What the pair's glue carries out once the controller has taken back what a window held away for the other
/// window: the pair file's writes and the pages to load again, each after the layout ops that brought the modules
/// back.
#[derive(Debug, Clone, PartialEq)]
pub enum PairOp {
    /// Window 2, orphaned by window 1's end: the pair file names `held`, what this window holds now, and this window
    /// keeps the file from here on.
    Orphaned { win: WinId, held: BTreeSet<String> },
    /// Window 1, whose window 2 was to come back and does not: the pair file says window 2 is not open (still naming
    /// what it held), the saved tabs offered from now on add window 2's, and the launch's restore of tabs goes on.
    Unrestored { win: WinId },
    /// A page that came back is loaded again, from its store as it is now.
    ReloadPage { win: WinId, module: ModuleId },
}

/// Lua calls applied after native and service operations.
#[derive(Debug, Clone, PartialEq)]
pub enum LuaOp {
    /// Loads the user initialization.
    LoadInit,
    /// Emits a named event.
    Emit { event: String },
    /// Invokes a registered command.
    Invoke { command: String },
}

/// Text-size actions and page/base changes supplied by the shell.
#[derive(Debug, Clone, PartialEq)]
pub enum TextSizeEvent {
    /// An application accelerator affecting every pane that exists.
    Both {
        win: WinId,
        step: super::text_size::TextStep,
    },
    /// A wheel step addresses the pane under the pointer, regardless of where the keys are.
    Pane {
        win: WinId,
        module: ModuleId,
        step: super::text_size::TextStep,
    },
    /// A plugin page now has a view. The host posts this once per new view, not again for a view
    /// it already attached: each one sets the zoom and logs a line, and the controller cannot tell
    /// two posts for the same view apart.
    PageAttached { win: WinId, module: ModuleId },
    /// The unzoomed chat font size changed.
    Base { win: WinId, px: f32 },
    /// What the user's config left in `g:neovide_scale_factor`, read at nvim's VimEnter: where `Ctrl+0` returns to.
    Configured { win: WinId, scale: f64 },
}

/// Facts posted by the native host and services.
#[derive(Debug, Clone, PartialEq)]
pub enum HostEvent {
    /// Input and worker answers for the project chooser.
    Project(ProjectEvent),
    /// A text-size action or service fact.
    TextSize(TextSizeEvent),
    /// A watched hardware key was released.
    KeyReleased { win: WinId, keycode: u32 },
    /// A Control key went up while the prefix asked to hear it ([`crate::wm::Controller::watches_ctrl`]).
    CtrlReleased { win: WinId },
    /// A pointer event on a controller target.
    Pointer {
        win: WinId,
        on: PointerTarget,
        ev: PointerEv,
    },
    /// Native focus facts captured at signal time; the driver stamps the applying op.
    NativeFocus {
        win: WinId,
        owner: Option<FocusOwner>,
        chain: FocusChain,
        active: bool,
        op_index: Option<u32>,
    },
    /// Native window activation changed.
    Active { win: WinId, active: bool },
    /// Deferred content metrics changed.
    Resized { win: WinId, metrics: FrameMetrics },
    /// A deferred module minimum changed.
    MinSize { win: WinId, module: ModuleId, min: Size },
    /// Allocation settled its layout pins. The controller queues the tree as the settle left it: this event
    /// is heard later, from the driver's idle, and a change that comes in between (a resize of that very pin)
    /// must not pass for part of the settle.
    Settled { win: WinId },
    /// A `CreateSurface` op the host carried out: `module`'s surface exists now. Posted by the driver, which is
    /// the one other place besides a `Focus` op's tail where an op's answer is read.
    Created { win: WinId, module: ModuleId },
    /// A `PrepareSurface` op the host carried out: the arriving module's place exists without being revealed.
    /// Posted by the driver, as [`HostEvent::Created`] is.
    Prepared { win: WinId, module: ModuleId },
    /// The answer gate refused a card answer from a chat view (`Controller::answer_gate`); `text` is why, for the
    /// toast.
    AnswerRefused { win: WinId, text: String },
    /// A process termination signal arrived.
    EndSignal { name: &'static str },
    /// Window 1 said `bye` to this module window: the group is closing, and the window goes with it.
    GroupClosed { win: WinId },
    /// Another process asks this window to open a file. Always answered with one [`HostOp::OpenAnswer`] of this `id`.
    OpenFile {
        win: WinId,
        id: u64,
        path: std::path::PathBuf,
        line: Option<u32>,
        col: Option<u32>,
        /// The asking launcher's activation token, opaque.
        token: Option<String>,
    },
    /// Another process asks this window to come forward. Always answered with one [`HostOp::OpenAnswer`] of this
    /// `id`.
    RaiseWindow {
        win: WinId,
        id: u64,
        /// The asking launcher's activation token, opaque.
        token: Option<String>,
    },
    /// A module surface ended.
    SurfaceEnded { win: WinId, module: ModuleId, why: Ended },
    /// An editor service event.
    Editor(EditorEvent),
    /// A chat service event.
    Chat(ChatEvent),
    /// A plugin page's move or first restored frame.
    Plugin(PluginEvent),
    /// A terminal service event.
    Terminal(TermEvent),
    /// A Lua kernel event.
    Lua(LuaEvent),
    /// A hint coordinator event.
    Hint(HintEvent),
    /// An event from the rename entry.
    TopBarEntry { win: WinId, ev: EntryEv },
    /// The top bar gave the keys up by its own key: they go back to the module that last had them.
    TopBarLeft { win: WinId },
    /// A linked process event.
    Peer(PeerEvent),
    /// What the pair's glue saw of the link's end and of a window 2 that was to come back.
    Pair(PairEvent),
    /// The module window `name`, started by `win`, did not come up: its handshake was missed, it was refused or its
    /// link ended before it linked. Whatever waited for it is dropped, and the name is free to start again.
    ModuleWindowFailed { win: WinId, name: WindowName, why: String },
    /// The output list changed.
    MonitorsChanged { monitors: Vec<Monitor> },
    /// The output the window is on, or none after leaving every output.
    WindowMonitor { win: WinId, monitor: Option<MonitorId> },
    /// A fact about the first-start screen, from a worker or from the shell.
    Onboarding(OnboardingEvent),
    /// A scheduled timer or idle fired.
    Timer(TimerToken),
    /// A watched descriptor became ready.
    Fd(WatchToken),
    /// A worker requested UI processing.
    Wake,
    /// The host could not apply an operation. `surface` is the window and module of the refused op when it acts
    /// on one module's surface ([`HostOp::surface`]): a batch may create several surfaces and only the refused
    /// ones report back, so the kind alone cannot say which failed.
    Refused {
        what: OpTag,
        why: String,
        surface: Option<(WinId, ModuleId)>,
    },
}

/// The cached environment a host supplies when a project chooser opens.
#[derive(Debug, Clone, PartialEq)]
pub struct ChooserEnv {
    pub home: Option<PathBuf>,
    pub lang: crate::onboarding::Lang,
}

/// Project work the host carries out without blocking its event thread.
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectOp {
    List {
        win: WinId,
        serial: u64,
    },
    Read {
        win: WinId,
        serial: u64,
        /// Walked folder, preserved in the reply envelope.
        dir: PathBuf,
        filter: Option<String>,
        debounce: bool,
        near: Option<OsString>,
    },
    FolderDialog {
        win: WinId,
        at: Option<PathBuf>,
    },
    Open {
        win: WinId,
        target: PathBuf,
    },
}

/// Project input and facts. Request serials distinguish replies from an earlier opening or line.
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectEvent {
    Listing {
        win: WinId,
        serial: u64,
        /// Walked folder; the listing may name a different physical folder.
        dir: PathBuf,
        listing: Listing,
    },
    Later {
        win: WinId,
        serial: u64,
        git: Vec<OsString>,
        links: Vec<(OsString, LinkKind)>,
    },
    Slow {
        win: WinId,
        serial: u64,
    },
    Stuck {
        win: WinId,
        serial: u64,
    },
    Freed {
        win: WinId,
        mount: PathBuf,
    },
    Rows {
        win: WinId,
        serial: u64,
        rows: Vec<Row>,
    },
    Line {
        win: WinId,
        ev: LineEv,
    },
    Pick {
        win: WinId,
        index: usize,
        rows_rev: u64,
    },
    PickTwice {
        win: WinId,
        index: usize,
        rows_rev: u64,
    },
    Dismiss {
        win: WinId,
    },
    Folder {
        win: WinId,
        path: Option<PathBuf>,
    },
    /// A start of `path` that did not come to a window: `code` is the child's exit code, `None` when it never started or
    /// a signal ended it, and `gone` says the folder was not there once the child had ended.
    OpenFailed {
        win: WinId,
        path: PathBuf,
        code: Option<i32>,
        gone: bool,
    },
}

impl ProjectEvent {
    pub fn win(&self) -> WinId {
        match self {
            Self::Listing { win, .. }
            | Self::Later { win, .. }
            | Self::Slow { win, .. }
            | Self::Stuck { win, .. }
            | Self::Freed { win, .. }
            | Self::PickTwice { win, .. }
            | Self::Rows { win, .. }
            | Self::Line { win, .. }
            | Self::Pick { win, .. }
            | Self::Dismiss { win }
            | Self::Folder { win, .. }
            | Self::OpenFailed { win, .. } => *win,
        }
    }
}

/// What the first-start screen's workers and the shell report. Each carries the generation of the
/// [`OnboardingOp`] it answers; one that is not the screen's now is dropped. [`Found::TrustOpen`] may carry `0`,
/// which means whichever screen is up.
#[derive(Debug, Clone, PartialEq)]
pub enum OnboardingEvent {
    /// The check said the screen opens.
    Open { win: WinId, opening: Opening },
    /// A detection answer, or the trust question opening or closing.
    Found { win: WinId, generation: u64, found: Found },
    /// The write ended.
    Written {
        win: WinId,
        generation: u64,
        outcome: WriteOutcome,
    },
    /// The check before the write said no.
    CheckFailed { win: WinId, generation: u64, why: CheckWhy },
    /// The start's check said the screen stays closed.
    NoScreen { win: WinId },
}

/// The native owner of the keys.
#[derive(Debug, Clone, PartialEq)]
pub enum FocusOwner {
    /// A module.
    Module(ModuleId),
    /// The top bar.
    TopBar,
    /// The rename entry.
    RenameEntry,
}

/// Module hosts above the focus destination, innermost first.
#[derive(Debug, Clone, PartialEq)]
pub struct FocusChain(pub Vec<ModuleId>);

/// Why a surface ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Ended {
    /// The editor child exited.
    NvimExited,
    /// The terminal shell exited.
    ShellExited,
    /// A page could not continue.
    PageGaveUp,
    /// Killed with the retained reopening policy.
    Killed { reopen: Reopen },
    /// No longer needed.
    Retired,
}

/// The controller address beneath a pointer.
#[derive(Debug, Clone, PartialEq)]
pub enum PointerTarget {
    /// A container strip entry.
    StripEntry(ModuleId),
    /// A float title bar: the float its chrome drew when the press began.
    FloatBar(ModuleId),
    /// A float resize grip: the float its chrome drew when the press began.
    FloatGrip(ModuleId),
    /// A layout divider: the handle's index in the grid's pool of handles, which draws the arrangement's divider
    /// of the same index.
    Divider(u32),
    /// A top-bar item.
    TopBarItem(u32),
    /// A module chip.
    Chip(ModuleId),
    /// The window itself.
    Window,
}

/// Pointer coordinates and button transitions. A drag on a divider, a float's bar or its grip is posted by the
/// grid's gesture as one `Press`, its `Motion`s and a `Release` or `Cancel`; the gesture claims its sequence and
/// decides nothing, and nothing in the grid remembers the drag: the controller does.
#[derive(Debug, Clone, PartialEq)]
pub enum PointerEv {
    /// A button press; for a drag, its begin: where it began in the widget, and the button pressed.
    Press { x: f64, y: f64, button: u32 },
    /// Pointer movement; for a drag, the offset since its begin in the moving widget's own coordinates (what a
    /// toolkit's drag update reports): the widget moves under the pointer as the drag re-allocates it, so it is
    /// how far the pointer is from where that widget is drawn now.
    Motion { x: f64, y: f64 },
    /// A button release; for a drag, its end.
    Release { x: f64, y: f64 },
    /// The gesture lost its sequence; a release may never arrive.
    Cancel,
}

/// Rename entry input.
#[derive(Debug, Clone, PartialEq)]
pub enum EntryEv {
    /// Accepts the entered text.
    Enter(String),
    /// Cancels editing.
    Escape,
    /// The entry lost focus.
    LostKeys,
}

/// Facts reported by the editor service.
#[derive(Debug, Clone, PartialEq)]
pub enum EditorEvent {
    /// The pane's new nvim is running.
    Ready { win: WinId },
    /// The exited nvim's harness was released; the same pane can start again.
    Released { win: WinId },
    /// nvim's process still lives, so its harness cannot be released yet.
    ReleaseRefused { win: WinId },
    /// The pane could not restart from its current state.
    RestartRefused { win: WinId, state: String },
    /// The child exited.
    Exited {
        win: WinId,
        settled: Option<super::close::NvimAnswer>,
    },
    /// The launcher exited while nvim itself is still alive and unreachable.
    Unreachable { win: WinId },
    /// This nvim will say when it leaves: an exit that comes without [`Self::Leaving`] is a crash.
    Watched { win: WinId },
    /// nvim is quitting on purpose.
    Leaving { win: WinId },
    /// A start found `count` project files with unsaved changes in swap files, `newest` the latest.
    Recoverable { win: WinId, count: u32, newest: String },
    /// The embedded editor reported its reopen progress.
    Reopen {
        win: WinId,
        line: crate::pane_switch::ReopenLine,
    },
    /// The answer to the [`EditorCall::Capture`] asked with `token`: the record's JSON and whether it is the one the
    /// restore itself produced, nothing when the capture would not answer for this nvim yet, or why the call failed.
    Captured {
        win: WinId,
        token: String,
        reply: Result<Option<(String, bool)>, String>,
    },
    /// This generation's quit request could not be sent.
    QuitNotSent { win: WinId, generation: u32 },
    /// The editor reported its scale.
    ScaleReported { win: WinId, scale: f64 },
    /// A generation answered the quit request.
    QuitAnswered { win: WinId, mode: String, generation: u64 },
    /// Launching failed.
    StartFailed { win: WinId, why: String },
    /// The pane-switch feed requested navigation.
    Navigate { win: WinId, direction: Direction },
    /// A generation cancelled quitting.
    QuitCancelled { win: WinId, generation: u64 },
    /// An [`EditorCall::Open`] answered an error or has not answered in time. At a blocking prompt it stays queued
    /// in nvim and runs when the prompt ends, so this never means the file will not open.
    OpenFailed { win: WinId, why: String },
    /// The editor let go of a person's own nvim, which is back with its terminal or on its way there.
    Detached { win: WinId },
    /// The answer to the [`EditorCall::AskCloseFacts`] of the same `generation`.
    CloseFacts {
        win: WinId,
        generation: u32,
        asks: CloseAsks,
    },
    /// A person's own nvim went back to them, or must, for `why`: the window closes without asking about it.
    HandedBack { win: WinId, why: super::close::HandBackWhy },
}

/// Facts reported by chat services.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatEvent {
    /// A view requests attention.
    Attention { win: WinId, view: ViewId },
    /// A view changed its busy state.
    Busy { win: WinId, view: ViewId, busy: bool },
    /// A tab requested a shared action.
    TabVerb { win: WinId, view: ViewId, action: Action },
    /// A chooser row belongs to the linked peer, which should be raised.
    ChooserPartnerPick { win: WinId, view: ViewId, session: String },
    /// A tab this module window offered stays here, for the reason `why`.
    TabKept { win: WinId, why: String },
    /// A tab left this module window: it is open in the window it was handed to, or its last tab was closed.
    TabLeft { win: WinId },
    /// A view returned keys.
    KeysReturn { win: WinId, view: ViewId },
    /// A view reports coverage.
    Covered { win: WinId, view: ViewId },
    /// A card arrived in a view that is off screen, and the policy brings the view back: shown where it was hidden
    /// from and its tab drawn, the keys staying where they are.
    Reveal { win: WinId, view: ViewId },
    /// A page became visible; the driver stamps the applying op.
    PageShown {
        win: WinId,
        view: ViewId,
        op_index: Option<u32>,
    },
    /// A view asks to be shown.
    ShowView { win: WinId, view: ViewId },
    /// A view has no tabs.
    ViewEmpty { win: WinId, view: ViewId },
    /// Navigation left a view.
    NavFallthrough {
        win: WinId,
        view: ViewId,
        direction: Direction,
    },
    /// A review opened.
    ReviewOpen { win: WinId },
    /// An editor action finished.
    EditorDone { win: WinId, view: ViewId },
    /// The editor's "edit the selection" key was pressed: the chat takes the keys with its composer open.
    EditSelection { win: WinId },
    /// A chat supplied hint text.
    Hint { win: WinId, message: String },
    /// A chat supplied a toast: `Attention` for a hidden chat's waiting card, which a refusal does not erase.
    Toast {
        win: WinId,
        text: String,
        kind: super::toasts::ToastKind,
    },
    /// A keymap sheet closed; `opening` names the opening it closes, so a close of an earlier one is dropped.
    KeymapClosed {
        win: WinId,
        view: ViewId,
        opening: u64,
        forced: bool,
    },
    /// A tab froze for a move ([`ChatOp::Freeze`]): `record` is what the other window rebuilds the tab from, and
    /// `handoff` holds the descriptors, which nothing here reads.
    Frozen {
        win: WinId,
        view: ViewId,
        mv: MoveId,
        record: serde_json::Value,
        handoff: Handoff,
    },
    /// The tab could not freeze for move `mv`, in the words of `why`; the tab is as it was.
    FreezeRefused {
        win: WinId,
        view: ViewId,
        mv: MoveId,
        why: String,
    },
    /// The presence [`ChatOp::HoldPresence`] asked for move `mv` is confirmed.
    PresenceHeld { win: WinId, mv: MoveId },
    /// The presence for move `mv` could not be confirmed.
    PresenceRefused { win: WinId, mv: MoveId, why: String },
}

/// An opaque terminal transfer whose identity follows its shared allocation.
#[derive(Clone)]
pub struct Handoff(Rc<dyn Any>);

impl Handoff {
    /// Wraps what the terminal's host hands the link; nothing on the way reads it.
    pub fn new(inner: Rc<dyn Any>) -> Handoff {
        Handoff(inner)
    }

    /// What the terminal's host put in, for the link that sends it.
    pub fn inner(&self) -> &Rc<dyn Any> {
        &self.0
    }
}

impl fmt::Debug for Handoff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Handoff(..)")
    }
}

impl PartialEq for Handoff {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

/// Which shell of a terminal pane something was asked against: the pane's start count, and the shell's pid when
/// one was running. Captured by the host before a read off the toolkit's thread and handed back with its answer,
/// so whoever acts on the answer can tell whether the pane still runs that shell. `typed` is how many keys and mouse
/// reports the pane had been given for its shell at the capture: a read that found the shell idle says nothing of a
/// command typed after it, which leaves the shell and its pid as they were.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShellStart {
    pub generation: u64,
    pub pid: Option<u32>,
    pub typed: u64,
}

impl ShellStart {
    pub fn of(generation: u64, pid: Option<u32>) -> Self {
        Self {
            generation,
            pid,
            typed: 0,
        }
    }

    /// The same capture, with the pane's count of what it was given for its shell.
    pub fn typed(self, typed: u64) -> Self {
        Self { typed, ..self }
    }

    /// Whether `now` still runs the shell captured here ([`Self::still`]) and was given nothing for it since: what
    /// a restart decided on an idle read needs, since a command typed meanwhile may be running in the same shell.
    pub fn untouched(self, now: Option<ShellStart>) -> bool {
        self.still(now) && now.is_some_and(|now| now.typed == self.typed)
    }

    /// Whether `now` (the pane as it is today, `None` once it is idle or gone) still runs the shell captured here. A
    /// new start bumps the count, so a pane killed and reused under the same number fails; a shell that was running
    /// must be the same process, so a pane closed in place (the base terminal keeps its pane when killed) fails too.
    /// A shell not yet spawned at the capture may have spawned since, under the same start.
    pub fn still(self, now: Option<ShellStart>) -> bool {
        let Some(now) = now else {
            return false;
        };
        now.generation == self.generation && (self.pid.is_none() || now.pid == self.pid)
    }
}

/// Facts reported by terminal services.
#[derive(Debug, Clone, PartialEq)]
pub enum TermEvent {
    /// A terminal child started.
    Started { win: WinId, module: ModuleId, pid: u32 },
    /// A terminal child exited.
    Exited { win: WinId, module: ModuleId },
    /// A terminal rang its bell.
    Bell { win: WinId, module: ModuleId },
    /// The directory a [`TermOp::ReadCwd`] read: the shell's directory, or the project root where it could not be
    /// read; `Err` with the refusal's text when the terminal it was read for ended meanwhile. `read` is the
    /// request's own.
    Cwd {
        win: WinId,
        module: ModuleId,
        read: u64,
        dir: Result<PathBuf, String>,
    },
    /// What a [`TermOp::ReadJob`] read: `shell` is the pane's shell captured before the read, `cwd` that shell's
    /// directory (`Err` when it could not be read, the shell having ended meanwhile among the reasons), and `busy`
    /// whether any process but the shell runs in its session, in the foreground or not. `read` is the request's own.
    Job {
        win: WinId,
        module: ModuleId,
        read: u64,
        shell: ShellStart,
        cwd: Result<PathBuf, String>,
        busy: bool,
    },
    /// A terminal froze for a move ([`TermOp::Detach`]): `record` is what the destination rebuilds its screen and
    /// session from, and `handoff` holds the descriptors, which nothing here reads.
    Detached {
        win: WinId,
        module: ModuleId,
        /// The move the freeze was asked for ([`TermOp::Detach`]'s `mv`).
        mv: MoveId,
        record: serde_json::Value,
        handoff: Handoff,
    },
}

/// Facts reported by the Lua kernel.
#[derive(Debug, Clone, PartialEq)]
pub enum LuaEvent {
    /// Layout requests are queued for the controller to drain.
    LayoutRequests {
        win: WinId,
        requests: Vec<crate::lua::layout::LayoutRequest>,
    },
}

/// Facts reported by the hint coordinator.
#[derive(Debug, Clone, PartialEq)]
pub enum HintEvent {
    /// Hint mode changed.
    Active { win: WinId, active: bool },
    /// A hint selected a target.
    Landed { win: WinId, target: HintTarget },
}

/// The destinations selectable by a hint.
#[derive(Debug, Clone, PartialEq)]
pub enum HintTarget {
    /// A module.
    Module(ModuleId),
    /// A strip entry.
    StripEntry(ModuleId),
    /// A chip.
    Chip(ModuleId),
    /// A top-bar item.
    TopBarItem(u32),
}

/// What the other window of the pair said, as the link's thread decoded it, and the link's own news.
#[derive(Debug, Clone, PartialEq)]
pub enum PeerEvent {
    /// The other window offers a module.
    MoveOffer {
        peer: PeerId,
        id: MoveId,
        module: ModuleId,
        kind: MoveKind,
        shape: Shape,
        size: (u32, u32),
        session: Option<String>,
    },
    /// The destination has the module's place ready.
    MoveReady { peer: PeerId, id: MoveId },
    /// The destination will not take the module.
    MoveRefused { peer: PeerId, id: MoveId, why: String },
    /// The source's module, with its record and descriptors.
    Move {
        peer: PeerId,
        id: MoveId,
        record: serde_json::Value,
        fds: Descriptors,
    },
    /// The destination holds the module.
    MoveDone { peer: PeerId, id: MoveId },
    /// The other window gave a move up, as its source before `move` or as its destination after it.
    MoveAbort { peer: PeerId, id: MoveId, why: String },
    /// The other window asks this one to offer it a module this window holds: by its id here, or (a chat) by its
    /// session.
    MoveAsk {
        peer: PeerId,
        module: Option<ModuleId>,
        session: Option<String>,
        follow: bool,
    },
    /// The link's wake was drained: a confirmed send may have been answered.
    Woke { peer: PeerId },
    /// The host's answer to [`HostOp::RaisePeer`]: the activation token it got, if any.
    Raised { peer: PeerId, token: Option<String> },
    /// The other window's counts and workspace names.
    Summary { peer: PeerId, summary: Summary },
    /// The other window gives this one the keys.
    FocusIntent {
        peer: PeerId,
        target: FocusTarget,
        token: Option<String>,
    },
    /// The other window asks for a workspace already held here.
    Workspace {
        peer: PeerId,
        name: String,
        token: Option<String>,
    },
    /// The saved workspace and the module moves that decide when it may be shown.
    WorkspaceMove {
        peer: PeerId,
        group: u64,
        name: String,
        tree: serde_json::Value,
        moves: Vec<u64>,
        swap: Option<u64>,
    },
    /// Two workspace groups going in opposite directions.
    Swap { peer: PeerId, move_a: u64, move_b: u64 },
    /// The modules the other window currently holds.
    Held { peer: PeerId, ids: Vec<ModuleId> },
    /// The connector of the output the other window is on.
    Monitor { peer: PeerId, connector: String },
    /// The other window said it is closing.
    Bye { peer: PeerId },
    /// The link ended.
    LinkClosed { peer: PeerId },
    /// A module window asks this window (window 1) for another window; `token` is the asking window's activation
    /// token, for the window that comes forward.
    HubAsk {
        peer: PeerId,
        ask: HubAsk,
        token: Option<String>,
    },
    /// The module window's toplevel gained the keyboard.
    WindowFocused { peer: PeerId },
}

impl PeerEvent {
    /// The window of the pair the event came from.
    pub fn peer(&self) -> PeerId {
        match self {
            PeerEvent::MoveOffer { peer, .. }
            | PeerEvent::WorkspaceMove { peer, .. }
            | PeerEvent::Swap { peer, .. }
            | PeerEvent::MoveReady { peer, .. }
            | PeerEvent::MoveRefused { peer, .. }
            | PeerEvent::Move { peer, .. }
            | PeerEvent::MoveDone { peer, .. }
            | PeerEvent::MoveAbort { peer, .. }
            | PeerEvent::MoveAsk { peer, .. }
            | PeerEvent::Woke { peer }
            | PeerEvent::Raised { peer, .. }
            | PeerEvent::Summary { peer, .. }
            | PeerEvent::FocusIntent { peer, .. }
            | PeerEvent::Workspace { peer, .. }
            | PeerEvent::Held { peer, .. }
            | PeerEvent::Monitor { peer, .. }
            | PeerEvent::Bye { peer }
            | PeerEvent::HubAsk { peer, .. }
            | PeerEvent::WindowFocused { peer }
            | PeerEvent::LinkClosed { peer } => *peer,
        }
    }
}

/// What the pair's glue saw happen to the pair, for the window it names: the controller takes back what the window
/// held away for the other window, as the other window's end leaves it.
#[derive(Debug, Clone, PartialEq)]
pub enum PairEvent {
    /// The other window ended, cleanly or not, after this window had linked to it; `who` names it as the log does.
    LinkEnded { win: WinId, who: String },
    /// Window 2, started again at this window's launch, does not come back: its start was refused, or it ended or
    /// never finished its handshake.
    RestoreFailed { win: WinId },
}

/// The controller's way to the other window: pushing a message onto the link thread's channel, which never calls
/// back into the controller, so it is called directly.
pub trait PeerPort {
    /// Queues `msg`, with a terminal's handoff when it carries one; an error is the link's, as text.
    fn send(&self, msg: Msg, handoff: Option<Handoff>) -> Result<(), String>;
    /// `LinkHandle::send_confirmed`: the receiver answers once the packet is on the socket (or never will be).
    fn send_confirmed(&self, msg: Msg) -> Result<Receiver<io::Result<()>>, String>;
}

/// Descriptors that arrived with a move. One place owns them at a time -- the event, then the move's machine, then
/// the op that adopts them -- and each takes them out with [`Descriptors::take`]; a copy of the value is the same
/// descriptors, compared by identity.
#[derive(Clone, Default)]
pub struct Descriptors(Rc<RefCell<Vec<OwnedFd>>>);

impl Descriptors {
    pub fn new(fds: Vec<OwnedFd>) -> Descriptors {
        Descriptors(Rc::new(RefCell::new(fds)))
    }

    /// Takes the descriptors out, leaving none.
    pub fn take(&self) -> Vec<OwnedFd> {
        std::mem::take(&mut *self.0.borrow_mut())
    }

    /// Puts descriptors back in, after the ones already held.
    pub fn put(&self, fds: Vec<OwnedFd>) {
        self.0.borrow_mut().extend(fds);
    }

    /// How many descriptors are held now.
    pub fn len(&self) -> usize {
        self.0.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl fmt::Debug for Descriptors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Descriptors({})", self.len())
    }
}

impl PartialEq for Descriptors {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

/// The operation kind carried by a refusal.
#[derive(Debug, Clone, PartialEq)]
pub enum OpTag {
    /// Identifies project chooser work.
    Project,
    /// Identifies OpenWindow operations.
    OpenWindow,
    /// Identifies Present operations.
    Present,
    /// Identifies OpenAnswer operations.
    OpenAnswer,
    /// Identifies SetMode operations.
    SetMode,
    /// Identifies PlaceWindow operations.
    PlaceWindow,
    /// Identifies RestoreLinkedWindow operations.
    RestoreLinkedWindow,
    /// Identifies StartEditorWindow operations.
    StartEditorWindow,
    /// Identifies CloseWindow operations.
    CloseWindow,
    /// Identifies Quit operations.
    Quit,
    /// Identifies CreateSurface operations.
    CreateSurface,
    /// Identifies DropSurface operations.
    DropSurface,
    /// Identifies PrepareSurface operations.
    PrepareSurface,
    /// Identifies OpenLinkedWindow operations.
    OpenLinkedWindow,
    /// Identifies RaisePeer operations.
    RaisePeer,
    /// Identifies OpenModuleWindow operations.
    OpenModuleWindow,
    /// Identifies ForwardRaise operations.
    ForwardRaise,
    /// Identifies PeerRun operations.
    PeerRun,
    /// Identifies CompositorFocus operations.
    CompositorFocus,
    /// Identifies HubAsk operations.
    HubAsk,
    /// Identifies Beside operations.
    Beside,
    /// Identifies BuildBasePage operations.
    BuildBasePage,
    /// Identifies HandOverTab operations.
    HandOverTab,
    /// Identifies KeepTab operations.
    KeepTab,
    /// Identifies AdoptMoved operations.
    AdoptMoved,
    /// Identifies OrderBaseTabs operations.
    OrderBaseTabs,
    /// Identifies EndPeer operations.
    EndPeer,
    /// Identifies Map operations.
    Map,
    /// Identifies Unmap operations.
    Unmap,
    /// Identifies Allocate operations.
    Allocate,
    /// Identifies Restack operations.
    Restack,
    /// Identifies Focus operations.
    Focus,
    /// Identifies Chrome operations.
    Chrome,
    /// Identifies Hint operations.
    Hint,
    /// Identifies Editor operations.
    Editor,
    /// Identifies Chat operations.
    Chat,
    /// Identifies Terminal operations.
    Terminal,
    /// Identifies a terminal's restart in place ([`TermOp::Respawn`]), told apart from the terminal's other ops so
    /// its refusal reaches the restart that waits for it.
    Respawn,
    /// Identifies Plugin operations.
    Plugin,
    /// Identifies Cleanup operations.
    Cleanup,
    /// Identifies Pair operations.
    Pair,
    /// Identifies Onboarding operations.
    Onboarding,
    /// Identifies Lua operations.
    Lua,
}

impl OpTag {
    /// Identifies an operation without retaining its payload.
    pub fn of(op: &HostOp) -> Self {
        match op {
            HostOp::Project(_) => Self::Project,
            HostOp::OpenWindow { .. } => Self::OpenWindow,
            HostOp::Present { .. } => Self::Present,
            HostOp::OpenAnswer { .. } => Self::OpenAnswer,
            HostOp::SetMode { .. } => Self::SetMode,
            HostOp::PlaceWindow { .. } => Self::PlaceWindow,
            HostOp::RestoreLinkedWindow { .. } => Self::RestoreLinkedWindow,
            HostOp::StartEditorWindow { .. } => Self::StartEditorWindow,
            HostOp::CloseWindow { .. } => Self::CloseWindow,
            HostOp::Quit => Self::Quit,
            HostOp::CreateSurface { .. } => Self::CreateSurface,
            HostOp::DropSurface { .. } => Self::DropSurface,
            HostOp::PrepareSurface { .. } => Self::PrepareSurface,
            HostOp::OpenLinkedWindow { .. } => Self::OpenLinkedWindow,
            HostOp::RaisePeer { .. } => Self::RaisePeer,
            HostOp::OpenModuleWindow { .. } => Self::OpenModuleWindow,
            HostOp::ForwardRaise { .. } => Self::ForwardRaise,
            HostOp::PeerRun { .. } => Self::PeerRun,
            HostOp::CompositorFocus { .. } => Self::CompositorFocus,
            HostOp::HubAsk { .. } => Self::HubAsk,
            HostOp::Beside(..) => Self::Beside,
            HostOp::BuildBasePage { .. } => Self::BuildBasePage,
            HostOp::HandOverTab { .. } => Self::HandOverTab,
            HostOp::KeepTab { .. } => Self::KeepTab,
            HostOp::AdoptMoved { .. } => Self::AdoptMoved,
            HostOp::OrderBaseTabs { .. } => Self::OrderBaseTabs,
            HostOp::EndPeer { .. } => Self::EndPeer,
            HostOp::Map { .. } => Self::Map,
            HostOp::Unmap { .. } => Self::Unmap,
            HostOp::Allocate { .. } => Self::Allocate,
            HostOp::Restack { .. } => Self::Restack,
            HostOp::Focus { .. } => Self::Focus,
            HostOp::Chrome { .. } => Self::Chrome,
            HostOp::Hint(..) => Self::Hint,
            HostOp::Editor(..) => Self::Editor,
            HostOp::Chat(..) => Self::Chat,
            HostOp::Terminal(TermOp::Respawn { .. }) => Self::Respawn,
            HostOp::Terminal(..) => Self::Terminal,
            HostOp::Plugin(..) => Self::Plugin,
            HostOp::Cleanup(..) => Self::Cleanup,
            HostOp::Pair(..) => Self::Pair,
            HostOp::Onboarding(..) => Self::Onboarding,
            HostOp::Lua(..) => Self::Lua,
        }
    }
}

impl HostOp {
    /// The window and module whose surface the op creates, drops, prepares, adopts, maps, unmaps, gives the keys
    /// to, freezes for a move (`Terminal(Detach)`, `Chat(Freeze)`) or restarts in place (`Terminal(Respawn)`); `None`
    /// for every other op. A refused grab names its module, so
    /// its refusal withdraws that one request and no other the same batch made, and a refused freeze names the
    /// terminal whose move is given up.
    pub fn surface(&self) -> Option<(WinId, ModuleId)> {
        match self {
            HostOp::CreateSurface { win, module, .. }
            | HostOp::DropSurface { win, module, .. }
            | HostOp::PrepareSurface { win, module, .. }
            | HostOp::AdoptMoved { win, module, .. }
            | HostOp::Terminal(TermOp::Detach { win, module, .. } | TermOp::Respawn { win, module, .. })
            | HostOp::Map { win, module }
            | HostOp::Unmap { win, module }
            | HostOp::Focus {
                win,
                to: FocusTo::Module(module),
                ..
            } => Some((*win, module.clone())),
            // A join the panel refused is told apart from any other chat call by the view it names, and so is a
            // chat's freeze, whose refusal gives its move up.
            HostOp::Chat(ChatOp::JoinBack { win, view } | ChatOp::Freeze { win, view, .. }) => {
                Some((*win, view.module()))
            }
            _ => None,
        }
    }
}

/// A complete decision ordered as layout, effects, then Lua.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Batch {
    layout: Vec<HostOp>,
    effects: Vec<HostOp>,
    lua: Vec<LuaOp>,
}

impl Batch {
    /// Adds layout work, routing Lua into its final segment.
    pub fn push_layout(&mut self, op: HostOp) {
        match op {
            HostOp::Lua(op) => self.push_lua(op),
            op => self.layout.push(op),
        }
    }

    /// Adds service work, routing Lua into its final segment.
    pub fn push_effect(&mut self, op: HostOp) {
        match op {
            HostOp::Lua(op) => self.push_lua(op),
            op => self.effects.push(op),
        }
    }

    /// Adds a Lua call after all layout and service work.
    pub fn push_lua(&mut self, op: LuaOp) {
        self.lua.push(op);
    }

    /// Adds layout work ahead of the first grab of the keys in this batch, or at the end of its layout work when
    /// none is asked for: a window comes up before the keys land in it.
    pub fn push_layout_before_keys(&mut self, op: HostOp) {
        let at = self
            .layout
            .iter()
            .position(|op| matches!(op, HostOp::Focus { .. }))
            .unwrap_or(self.layout.len());
        self.layout.insert(at, op);
    }

    /// Reports whether every segment is empty.
    pub fn is_empty(&self) -> bool {
        self.layout.is_empty() && self.effects.is_empty() && self.lua.is_empty()
    }

    /// Iterates layout work before service effects.
    pub fn ops(&self) -> impl Iterator<Item = &HostOp> {
        self.layout.iter().chain(self.effects.iter())
    }

    /// Returns Lua calls in insertion order.
    pub fn lua(&self) -> &[LuaOp] {
        &self.lua
    }

    /// Appends each segment while preserving its internal order.
    pub fn append(&mut self, other: Batch) {
        self.layout.extend(other.layout);
        self.effects.extend(other.effects);
        self.lua.extend(other.lua);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::ModuleId;

    /// A split from a terminal, or a restart in place, goes ahead after its read only if the same shell is still
    /// there: not once the pane is idle, not after a new start under the same number, not after the running shell
    /// was closed in place.
    #[test]
    fn a_captured_shell_is_still_there_only_for_the_same_start_and_process() {
        let captured = ShellStart::of(3, Some(4100));
        assert!(captured.still(Some(ShellStart::of(3, Some(4100)))));
        assert!(!captured.still(None), "killed: the pane went idle");
        assert!(
            !captured.still(Some(ShellStart::of(4, Some(4200)))),
            "the number reused by a new terminal"
        );
        assert!(
            !captured.still(Some(ShellStart::of(4, Some(4100)))),
            "a new start, whatever its pid"
        );
        assert!(
            !captured.still(Some(ShellStart::of(3, None))),
            "the shell closed in place"
        );
        let unspawned = ShellStart::of(1, None);
        assert!(
            unspawned.still(Some(ShellStart::of(1, Some(4300)))),
            "spawned since, same start"
        );
        assert!(!unspawned.still(Some(ShellStart::of(2, Some(4300)))));
    }

    /// A restart decided on an idle read goes ahead only if nothing was typed into the shell after the read: a
    /// command typed meanwhile keeps the same shell and pid, and may be running by the time the restart comes.
    #[test]
    fn a_shell_given_keys_since_the_capture_is_not_untouched() {
        let captured = ShellStart::of(3, Some(4100)).typed(7);
        assert!(captured.untouched(Some(ShellStart::of(3, Some(4100)).typed(7))));
        assert!(captured.still(Some(ShellStart::of(3, Some(4100)).typed(8))));
        assert!(
            !captured.untouched(Some(ShellStart::of(3, Some(4100)).typed(8))),
            "a key after the read"
        );
        assert!(
            !captured.untouched(Some(ShellStart::of(4, Some(4100)).typed(7))),
            "another start"
        );
        assert!(!captured.untouched(None), "the pane went idle");
    }

    /// A refused restart in place names its terminal and is told apart from the terminal's other ops, whose
    /// refusal with a module is a move's freeze.
    #[test]
    fn a_respawn_is_tagged_apart_and_names_its_terminal() {
        let op = HostOp::Terminal(TermOp::Respawn {
            win: WinId::MAIN,
            module: ModuleId::terminal(),
            dir: "/w/work".into(),
            expect: ShellStart::of(1, Some(10)),
        });
        assert_eq!(OpTag::of(&op), OpTag::Respawn);
        assert_eq!(op.surface(), Some((WinId::MAIN, ModuleId::terminal())));
        let read = HostOp::Terminal(TermOp::ReadJob {
            win: WinId::MAIN,
            module: ModuleId::terminal(),
            read: 0,
        });
        assert_eq!(OpTag::of(&read), OpTag::Terminal);
        assert_eq!(read.surface(), None);
    }

    fn map(module: ModuleId) -> HostOp {
        HostOp::Map {
            win: WinId::MAIN,
            module,
        }
    }

    fn start() -> HostOp {
        HostOp::Terminal(TermOp::Start {
            win: WinId::MAIN,
            module: ModuleId::terminal(),
        })
    }

    fn emit(text: &str) -> LuaOp {
        LuaOp::Emit { event: text.into() }
    }

    #[test]
    fn a_lua_op_pushed_as_a_layout_op_lands_in_the_lua_segment() {
        let mut batch = Batch::default();
        batch.push_layout(map(ModuleId::agent()));
        batch.push_layout(HostOp::Lua(emit("changed")));
        batch.push_effect(start());
        assert_eq!(
            batch.ops().cloned().collect::<Vec<_>>(),
            vec![map(ModuleId::agent()), start()]
        );
        assert_eq!(batch.lua(), &[emit("changed")]);
    }

    #[test]
    fn effects_come_after_layout_ops_whatever_the_push_order() {
        let mut batch = Batch::default();
        batch.push_effect(start());
        batch.push_layout(map(ModuleId::agent()));
        batch.push_effect(HostOp::Quit);
        batch.push_layout(map(ModuleId::terminal()));
        assert_eq!(
            batch.ops().cloned().collect::<Vec<_>>(),
            vec![map(ModuleId::agent()), map(ModuleId::terminal()), start(), HostOp::Quit]
        );
    }

    #[test]
    fn append_keeps_each_segment_in_order() {
        let mut first = Batch::default();
        first.push_layout(map(ModuleId::agent()));
        first.push_effect(start());
        first.push_lua(emit("first"));
        let mut second = Batch::default();
        second.push_layout(map(ModuleId::terminal()));
        second.push_effect(HostOp::Quit);
        second.push_effect(HostOp::Lua(emit("second")));
        first.append(second);
        assert_eq!(
            first.ops().cloned().collect::<Vec<_>>(),
            vec![map(ModuleId::agent()), map(ModuleId::terminal()), start(), HostOp::Quit]
        );
        assert_eq!(first.lua(), &[emit("first"), emit("second")]);
        assert!(!first.is_empty());
        assert!(Batch::default().is_empty());
    }

    #[test]
    fn op_tag_names_every_op() {
        let win = WinId::MAIN;
        let module = ModuleId::terminal();
        macro_rules! tag {
            ($op:expr, $tag:ident) => {
                assert_eq!(OpTag::of(&$op), OpTag::$tag);
            };
        }
        tag!(
            HostOp::OpenWindow {
                win,
                spec: WindowSpec {
                    role: WindowRole::Main,
                    presence: EditorPresence::Owned,
                    title: "main".into()
                }
            },
            OpenWindow
        );
        tag!(HostOp::Present { win, activation: None }, Present);
        tag!(HostOp::OpenAnswer { id: 1, accepted: true }, OpenAnswer);
        tag!(
            HostOp::SetMode {
                win,
                mode: WindowMode::default()
            },
            SetMode
        );
        tag!(
            HostOp::PlaceWindow {
                win,
                monitor: Some(MonitorId("one".into())),
                role: PlacementRole::Main
            },
            PlaceWindow
        );
        tag!(HostOp::RestoreLinkedWindow { win }, RestoreLinkedWindow);
        tag!(HostOp::StartEditorWindow { win }, StartEditorWindow);
        tag!(HostOp::CloseWindow { win }, CloseWindow);
        tag!(HostOp::Quit, Quit);
        tag!(
            HostOp::CreateSurface {
                win,
                module: module.clone(),
                kind: SurfaceKind::Terminal { cwd: ".".into() }
            },
            CreateSurface
        );
        tag!(
            HostOp::DropSurface {
                win,
                module: module.clone(),
                why: DropWhy::Killed
            },
            DropSurface
        );
        tag!(
            HostOp::PrepareSurface {
                win,
                module: module.clone(),
                shape: Shape::Tiled,
                size: (640, 480),
            },
            PrepareSurface
        );
        tag!(HostOp::OpenLinkedWindow { win }, OpenLinkedWindow);
        tag!(HostOp::BuildBasePage { win }, BuildBasePage);
        tag!(HostOp::RaisePeer { win, peer: PeerId(2) }, RaisePeer);
        tag!(
            HostOp::OpenModuleWindow {
                win,
                name: WindowName::Chat(2),
                token: None,
                keep_keys: false
            },
            OpenModuleWindow
        );
        tag!(
            HostOp::ForwardRaise {
                win,
                peer: PeerId(16),
                token: Some("t".into())
            },
            ForwardRaise
        );
        tag!(
            HostOp::PeerRun {
                win,
                peer: PeerId(16),
                name: "tab.next".into(),
                opts: vec![]
            },
            PeerRun
        );
        tag!(
            HostOp::CompositorFocus {
                win,
                direction: Direction::Left
            },
            CompositorFocus
        );
        tag!(
            HostOp::HandOverTab {
                win,
                from: PeerId(16),
                agent: AgentId::Claude,
                session: "s".into(),
                to: WindowName::Chat(2),
                token: None
            },
            HandOverTab
        );
        tag!(
            HostOp::KeepTab {
                win,
                peer: PeerId(16),
                session: "s".into(),
                why: "no".into()
            },
            KeepTab
        );
        tag!(
            HostOp::HubAsk {
                win,
                ask: HubAsk::InstanceNew
            },
            HubAsk
        );
        tag!(
            HostOp::AdoptMoved {
                win,
                module: module.clone(),
                kind: MoveKind::Terminal,
                record: serde_json::Value::Null,
                fds: Descriptors::default(),
            },
            AdoptMoved
        );
        tag!(
            HostOp::OrderBaseTabs {
                win,
                tabs: vec![TabId(1)],
                active: Some(TabId(1))
            },
            OrderBaseTabs
        );
        tag!(HostOp::EndPeer { win, peer: PeerId(2) }, EndPeer);
        tag!(HostOp::Beside(BesideOp::RaiseSelf), Beside);
        tag!(map(module.clone()), Map);
        tag!(
            HostOp::Unmap {
                win,
                module: module.clone()
            },
            Unmap
        );
        tag!(HostOp::Allocate { win }, Allocate);
        tag!(HostOp::Restack { win, moves: vec![] }, Restack);
        tag!(
            HostOp::Focus {
                win,
                to: FocusTo::Module(module.clone()),
                on_grab: vec![]
            },
            Focus
        );
        tag!(HostOp::Chrome { win, rev: 2 }, Chrome);
        tag!(HostOp::Hint(HintOp::Hide), Hint);
        tag!(
            HostOp::Editor(EditorCall::End {
                win,
                editor: owned_editor_for_tests()
            }),
            Editor
        );
        tag!(
            HostOp::Chat(ChatOp::OpenChooser {
                win,
                view: crate::tabs::ViewId::BASE
            }),
            Chat
        );
        tag!(start(), Terminal);
        tag!(HostOp::Plugin(PluginOp::Build { win, module }), Plugin);
        tag!(HostOp::Cleanup(CleanupOp::Feeds), Cleanup);
        tag!(HostOp::Pair(PairOp::Unrestored { win }), Pair);
        tag!(HostOp::Onboarding(OnboardingOp::OpenTutor { win }), Onboarding);
        tag!(HostOp::Project(ProjectOp::List { win, serial: 1 }), Project);
        tag!(HostOp::Lua(emit("changed")), Lua);
    }

    #[test]
    fn owned_gives_none_for_attached() {
        assert!(
            owned(EditorKind::Attached).is_none(),
            "a person's own nvim gets no token"
        );
        assert_eq!(owned(EditorKind::Owned), Some(owned_editor_for_tests()));
        assert_eq!(
            EditorKind::default(),
            EditorKind::Owned,
            "a window owns its editor unless told otherwise"
        );
    }

    /// Where product code builds the token, or could get one without asking a window: `path:line` of each
    /// `OwnedEditor(` outside its definition and `owned`, and of each `EditorKind::Owned` outside `owned` and the
    /// test constructor, since `owned(EditorKind::Owned)` would give a token whatever the window's kind. Test
    /// modules, inline or in files of their own, are not product code.
    fn tokens_built_outside_owned(sources: &[crate::source_scan::Source]) -> Vec<String> {
        use crate::source_scan::{braced, code_only, functions, test_only_files, word_ends};
        let test_only = test_only_files(sources);
        let mut found = Vec::new();
        for source in sources.iter().filter(|source| !test_only.contains(&source.path)) {
            let code = code_only(&source.text);
            let mut tests: Vec<(usize, usize)> = Vec::new();
            for (at, _) in code.match_indices("#[cfg(test)]") {
                let rest = &code[at..];
                let Some(open) = rest.find(['{', ';']) else {
                    continue;
                };
                let head = &rest[..open];
                if rest.as_bytes()[open] == b'{' && word_ends(head, "mod").len() == 1 {
                    let body = braced(&code, at + open);
                    tests.push((at, at + open + body.len()));
                }
            }
            let spans = |name: &str| -> Vec<(usize, usize)> {
                functions(&code, name)
                    .into_iter()
                    .map(|body| {
                        let start = body.as_ptr() as usize - code.as_ptr() as usize;
                        (start, start + body.len())
                    })
                    .collect()
            };
            let owned = spans("owned");
            let for_tests = spans("owned_editor_for_tests");
            let line = |at: usize| code[..at].matches('\n').count() + 1;
            let inside =
                |spans: &[(usize, usize)], at: usize| spans.iter().any(|(from, to)| (*from..*to).contains(&at));
            for end in word_ends(&code, "EditorKind") {
                let rest = code[end..].trim_start();
                let Some(rest) = rest.strip_prefix("::") else {
                    continue;
                };
                let rest = rest.trim_start();
                let named = rest
                    .strip_prefix("Owned")
                    .is_some_and(|after| !after.starts_with(|c: char| c.is_alphanumeric() || c == '_'));
                let at = end - "EditorKind".len();
                if named && !inside(&owned, at) && !inside(&for_tests, at) && !inside(&tests, at) {
                    found.push(format!("{}:{}", source.path, line(at)));
                }
            }
            for end in word_ends(&code, "OwnedEditor") {
                if !code[end..].trim_start().starts_with('(') {
                    continue;
                }
                let at = end - "OwnedEditor".len();
                let defined = code[..at].trim_end().ends_with("struct");
                if defined || inside(&owned, at) || inside(&tests, at) {
                    continue;
                }
                found.push(format!("{}:{}", source.path, line(at)));
            }
        }
        found
    }

    #[test]
    fn owned_editor_is_built_only_by_owned() {
        use crate::source_scan::{require_anchors, rust_sources, Source};
        let sources = rust_sources(&["core/src", "shell/src", "panel/src", "agent/src"]);
        require_anchors(&sources, &["pub struct OwnedEditor(", "pub fn owned("]);
        assert_eq!(tokens_built_outside_owned(&sources), Vec::<String>::new());
        // The scan sees a token built anywhere else in product code, and leaves test modules and comments alone.
        let planted = vec![
            Source {
                path: "core/src/planted.rs".into(),
                text: "// OwnedEditor(()) in a comment\nfn quit() -> OwnedEditor {\n    OwnedEditor(())\n}\n".into(),
            },
            Source {
                path: "core/src/planted_tests.rs".into(),
                text: "#[cfg(test)]\nmod tests {\n    fn t() { let _ = OwnedEditor(()); let _ = owned(EditorKind::Owned); }\n}\n"
                    .into(),
            },
            Source {
                path: "core/src/planted_kind.rs".into(),
                text: "fn sneak() -> Option<OwnedEditor> {\n    owned(super::window::EditorKind :: Owned)\n}\nfn fine(w: &Window) -> bool {\n    w.editor_kind() == EditorKind::OwnedLater\n}\n"
                    .into(),
            },
        ];
        assert_eq!(
            tokens_built_outside_owned(&planted),
            vec![
                "core/src/planted.rs:3".to_owned(),
                "core/src/planted_kind.rs:2".to_owned()
            ]
        );
    }
}
