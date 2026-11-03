//! The GTK shell's side of the window contract: `GtkHost` carries out the controller's ops on real
//! widgets and reports what GTK did through the attached sink; `scheduler::GlibScheduler` arms its
//! timers, idles and watches on GLib's default context.
//!
//! **Re-entrancy.** GTK emits signals while an op is still running: `grab_focus` notifies
//! `focus-widget` before it returns, `Unmap` of the surface holding the keys clears the window's
//! focus (GTK itself would leave the keys on the hidden widget) and so notifies it, and `close()`
//! emits `close-request` from inside itself. Every handler here only posts to the sink, which queues
//! what arrives while a batch applies, and no handler or op holds a borrow of the host while it calls
//! into GTK or the sink, so a signal fired from inside an op finds the host free.
//!
//! **Closing.** `CloseWindow` means "close without asking": the host marks the window as closing by
//! op before `close()`, and its own `close-request` handler then lets the close proceed without
//! asking the sink. Only a close the user or the compositor asked for reaches `close_requested`
//! (GTK does not tell the two apart, so both arrive as the user's), and the handler decides nothing
//! else: the controller answers `Stop` and, once the close may go, tears the window down in a batch of
//! its own -- the layout saved, the pair told, nvim ended, the terminals hung up, the directories and
//! feeds removed, the plugins and chats ended -- whose last op is that `CloseWindow`. Each teardown step
//! is a service op ([`ServiceSet`], [`ServiceSet::cleanup`]), carried out in the batch's order.
//! A `CloseWindow` applied while the sink is answering that window's own close request cannot call
//! `close()` (GTK ignores a close from inside its close-request emission); it is noted instead,
//! and the handler lets the close under way proceed whatever the sink answered.
//!
//! **The editor's quit.** `Editor(Quit)` sends nvim the quit flow's `:confirm qall` as a watched request; one
//! that could not be sent is refused and also posted as `EditorEvent::QuitNotSent` with the quit's generation,
//! since a refusal names no call. `Editor(End)` ends nvim, `Editor(ReleaseExited)` lets an exited nvim's pane go.
//!
//! **The editor's restart.** The release is told as its own fact either way, `EditorEvent::Released` or
//! `ReleaseRefused` (nvim's process still lives), and `Editor(Restart)` starts a fresh nvim on the released pane with
//! the spawn it carries ([`ServiceSet::restart_editor`]); a pane that refused from the state it was in is refused as
//! any failed op is and told as `EditorEvent::RestartRefused` naming that state, since a refusal names no call. A
//! restart carried out is told only once its nvim is ready, by the shell's ready hook (`EditorEvent::Ready`).
//!
//! **Surfaces and services.** The host builds and drops module surfaces through a [`SurfaceMaker`]
//! and carries out the service ops the controller's module lifecycle emits (a terminal's start, a
//! plugin page's build, reload, visibility and stop, a chat view's chooser) through a
//! [`ServiceSet`]. Both are traits because what they act on (the grid, the terminal registry, the
//! plugin pages, the chat panel) belongs to the shell's binary, and this module is also compiled
//! alone by its conformance test, which hands it stubs. Until one is set, its ops are refused as
//! not carried out.
//!
//! **Starting a window.** `OpenWindow` builds the window (created, not shown) for the application
//! set with [`GtkHost::set_application`] and adopts it; `Present` shows it. `Lua` ops go to the
//! [`LuaRunner`] the shell sets (`init.lua` and the events it emits are the binary's).
//!
//! **The window's mode.** `SetMode` toggles the window's mode through the function the shell sets
//! ([`GtkHost::set_mode_toggle`]), as `prefix F11` does, rather than setting the state it names: the shell owns the
//! window's real mode, which a bare F11, nvim's `g:neovide_fullscreen` and the compositor change too.
//!
//! **Focus, keys and the prefix's chrome.** A module's `Focus` goes through the shell's own grab when
//! one is set ([`GtkHost::set_focus`]), since the shell knows what a bare grab does not: a page still
//! to be built, the editor's grab that also tells the input method; whether the module is on screen
//! is the controller's to know before it asks. Without one it is the bare grab. A top-bar item's
//! `Focus` goes through [`GtkHost::set_top_bar`], which brings a hidden bar out first, and the rename entry's
//! through [`GtkHost::set_rename_entry`], which does the same and shows the entry so it can take them. The
//! `select.number` digits and HINT's toggle go to the window's [`HintPort`]; a `Chrome` op reads the
//! window's model through [`GtkHost::set_chrome_model`] and redraws what changed on the widgets
//! [`GtkHost::set_chrome_widgets`] gives ([`chrome::reconcile`]), every flash and toast included. The cursor
//! signals, the literal keys and a chat page's claimed navigation key are service ops
//! ([`ServiceSet`]), and whether a chat page claims a key is the read [`GtkHost::set_chat_claims`]
//! gives.
//!
//! **The grid and the editor.** Each window's grid is reached through a [`GridPort`] and its editor
//! through [`EditorReads`] ([`GtkHost::set_grid`], [`GtkHost::set_editor`]): `Allocate` asks the grid
//! for a new allocation, `Restack` puts its float band in the op's order, and `Map`/`Unmap` leave
//! alone a host the grid's probe places by hand. The host also answers the controller's
//! [`HostReads`] from them, and with [`NoReads`]' values for a window that has given neither yet;
//! a chat view's attention is the read [`GtkHost::set_view_attention`] gives.
//!
//! **The pair.** `OpenLinkedWindow` starts or raises the project's second window through the [`LinkedWindows`] the
//! shell sets ([`GtkHost::set_linked_windows`]), with an activation token from the window's display; `RaisePeer` asks
//! that token and answers with `PeerEvent::Raised`, on which the controller asks the other window to come forward.
//! The controller's way onto the link itself is [`peer::LinkPort`], attached by the link's own code, not the host's.
//! `EndPeer` ends the other window through the same set, when this window started it.
//! `RestoreLinkedWindow` starts window 2 again at a launch through the same [`LinkedWindows`], with no token, and
//! `StartEditorWindow` starts the editor window from a window 2 left without one.
//! A module window of compositor mode adds five: `OpenModuleWindow` starts or raises a chat or page window through
//! [`LinkedWindows::open_module`] (a window the start opens takes no token and leaves the keys where they are),
//! `ForwardRaise` answers with the token a module window forwarded, `PeerRun` and `HubAsk` send `run_action` and
//! `hub_ask` over the link ([`LinkedWindows::send`], [`LinkedWindows::send_to_hub`]), and `CompositorFocus` is the
//! compositor's focus move past the window's edge, through [`PairRaise::move_focus`].
//! `PlaceWindow` applies this process's placement role, optionally moving to an output before focusing
//! ([`monitor_move`]), through the [`MonitorPlacer`] the shell sets, never on the GTK thread.
//!
//! **Moves.** An arriving module's `PrepareSurface` and `AdoptMoved` go to the [`SurfaceMaker`]: a terminal's place
//! is its pane, made off screen at the size the other window had it at, before anything there is frozen, and its
//! adoption takes the descriptors out of the op and builds the session over them; a numbered chat view's place is its
//! page, off screen, and a chat's adoption (into that view, or into the base view, which is never prepared) is the
//! panel's, from the record and the descriptors; a kind whose arrival the maker does not build is refused, so the
//! controller refuses the offer while its source still holds it whole. The terminal's freeze, forget and resume are
//! its service's ([`ServiceSet::terminal`]), a chat's the panel's ([`ServiceSet::chat`]).
//!
//! **Lifetime.** The host holds its windows weakly. GTK drops its own reference when a window
//! closes, and only then does the window dispose and emit `destroy`, which is where the host
//! forgets the window and its surfaces; a strong reference here would keep every closed window.

// The conformance test compiles this module alone, without the binary's callers of some of it.
#![allow(dead_code)]

pub(crate) mod chrome;
pub(crate) mod instance_pool;
pub(crate) mod keys;
pub(crate) mod onboarding;
pub(crate) mod peer;
pub(crate) mod prefix_panel;
pub(crate) mod project_chooser;
pub(crate) mod scheduler;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::rc::{Rc, Weak};

use eitri_core::attention::Attention;
use eitri_core::layout::{Direction, ModuleId, Rect, Size};
use eitri_core::tabs::ViewId;
use eitri_core::window_link::names::WindowName;
use eitri_core::window_link::Msg;
use eitri_core::wm::chrome::ChromeModel;
use eitri_core::wm::{
    ActivationToken, Applied, ChatKillCounts, ChatOp, ChooserEnv, CleanupOp, CloseBy, CloseVerdict, DropWhy,
    EditorCall, EditorEvent, FocusChain, FocusOwner, FocusTo, HintOp, Host, HostEvent, HostOp, HostReads, HostSink,
    LuaOp, Monitor, MonitorId, NoReads, OnboardingOp, OpTag, PeerEvent, PeerId, PluginOp, ProjectOp, SurfaceKind,
    TermOp, WinId, WindowSpec,
};
use gtk4::prelude::*;
use gtk4::{gdk, glib};

/// What toggles a window's mode the way `prefix F11` does: the window's real mode is the shell's, which also hears
/// a bare F11, the editor's own setting and the compositor.
type ModeToggleFn = dyn Fn(WinId);

/// What gives a module the keys when the shell knows more than a bare grab: whether it took.
type FocusFn = dyn Fn(&ModuleId) -> bool;

/// What gives a top-bar item the keys: the first item showing from that index on, the bar brought out
/// first if it is hidden. Whether one took them.
type TopBarFn = dyn Fn(WinId, u32) -> bool;

/// What gives the top bar's rename entry the keys: the bar brought out first if it is hidden, the entry shown so it
/// can take them, its text left to the `Chrome` op that follows. Whether it took them.
type RenameEntryFn = dyn Fn(WinId) -> bool;

/// What gives the first-start screen the keys: the screen shown first if it is hidden, since the `Focus` op comes
/// before the `Chrome` op that fills it. Whether it took them.
type OnboardingFocusFn = dyn Fn(WinId) -> bool;

/// What carries out the first-start screen's own ops (the checks, the detection, the write, the marker, the editor):
/// the shell's `OnboardingHost`. It answers each as a fact later, through the driver.
type OnboardingFn = dyn Fn(&OnboardingOp) -> Applied;

/// The native project chooser and its asynchronous work, supplied without coupling the host to the binary.
pub(crate) trait ProjectHost {
    fn apply(&self, op: &ProjectOp) -> Applied;
    /// Carries out [`ProjectOp::Open`], with the activation token the started window comes forward with.
    fn open(&self, win: WinId, target: &std::path::Path, token: Option<String>) -> Applied;
    fn focus(&self, win: WinId) -> bool;
    fn env(&self, win: WinId) -> ChooserEnv;
}

/// What reads a window's chrome model from the controller; nothing for a window it does not hold.
type ChromeModelFn = dyn Fn(WinId) -> Option<ChromeModel>;

/// What a chat view holds for the user, as its chip and its strip entry show it: read only, it never calls back.
type ViewAttentionFn = dyn Fn(ViewId) -> Attention;

/// Whether a chat page keeps a navigation key for itself: read only, it never calls back.
type ChatClaimsFn = dyn Fn(ViewId, Direction) -> bool;

/// The chat view holding the oldest card waiting for an answer: read only, it never calls back.
type OldestCardFn = dyn Fn(WinId) -> Option<ViewId>;

/// What a chat view's kill would end, and whether its tabs can close now: read only, it never calls back.
type KillCountsFn = dyn Fn(ViewId) -> ChatKillCounts;

/// Whether a new chat view can be given its first tab now (a break-out when the flag is set): read only, it never
/// calls back.
type FirstTabFn = dyn Fn(ViewId, bool) -> Result<(), String>;

/// The window's `select.number` digits and HINT's session, which stays the shell's.
pub(crate) trait HintPort {
    /// Draws each module's digit, the tenth module's as `0`.
    fn show_digits(&self, win: WinId, digits: &[(ModuleId, u8)]);
    /// Takes the digits down; nothing if none are up.
    fn hide_digits(&self, win: WinId);
    /// Starts or ends a HINT from the prefix; `keycode` is the key still held down that ran it.
    fn toggle(&self, win: WinId, keycode: u32);
}

/// The answer to every op this shell does not carry out through the host yet; its own code still
/// does those, so a controller that emits one learns it went nowhere.
pub(crate) const NOT_CARRIED_OUT: &str = "not carried out by this shell yet";

/// The compositor-aware presentation of a window of a pair or one that takes files from outside, supplied by the binary
/// so the host can also run alone.
pub(crate) trait PairRaise {
    fn raise(&self, window: &gtk4::Window, token: Option<&str>);
    /// Has the compositor move the keys one window `direction`; `false` with no adapter to do it, which `on_none` has
    /// then said once. Never waits: the adapter's command runs on its worker.
    fn move_focus(&self, direction: Direction, on_none: &dyn Fn(&str)) -> bool;
}

/// What starts the project's second window for `OpenLinkedWindow`: the pair's own code, which belongs to the shell's
/// binary. A host without it cannot start one.
pub(crate) trait LinkedWindows {
    /// Starts the window `name`, or raises it when it runs; `token` is the activation token this window's display
    /// gave, so the window that comes forward gets the keys. `Err` says why nothing was started.
    fn open(&self, win: WinId, name: &WindowName, token: Option<String>) -> Result<(), String>;
    /// Ends the process of the window `peer`, when this window started it. `Err` says why nothing was ended.
    fn end_peer(&self, win: WinId, peer: PeerId) -> Result<(), String> {
        let _ = (win, peer);
        Err(NOT_CARRIED_OUT.into())
    }
    /// Starts the window `name` again at this window's launch, as the pair last ended, with no activation token.
    /// `Err` says why it was not; what this window held for window 2 has then been given back here.
    fn restore(&self, win: WinId, name: &WindowName) -> Result<(), String>;
    /// Whether the module window `name` is open or starting, whoever started it.
    fn module_window_open(&self, name: &WindowName) -> bool {
        let _ = name;
        false
    }
    /// Window 1 opens the chat or page window `name`: starts it, or raises it with `token`. With `keep_keys` the window
    /// maps without taking the keys, and one that is open already is left as it is.
    fn open_module(&self, win: WinId, name: &WindowName, token: Option<String>, keep_keys: bool) -> Result<(), String> {
        let _ = (win, name, token, keep_keys);
        Err(NOT_CARRIED_OUT.into())
    }
    /// Sends `msg` to the window `peer` over its link. `Err` says why it was not sent.
    fn send(&self, win: WinId, peer: PeerId, msg: Msg) -> Result<(), String> {
        let _ = (win, peer, msg);
        Err(NOT_CARRIED_OUT.into())
    }
    /// Sends `msg` to window 1, from a window it started. `Err` says why it was not sent.
    fn send_to_hub(&self, win: WinId, msg: Msg) -> Result<(), String> {
        let _ = (win, msg);
        Err(NOT_CARRIED_OUT.into())
    }
    /// Hands the tab holding `agent`'s `session` from the module window `from` to the chat window `to`, which is started
    /// or raised with `token`. The window that asked is told itself when it cannot go; `Err` says why no window could be.
    fn hand_over(
        &self,
        win: WinId,
        from: PeerId,
        agent: &eitri_core::agent_backend::AgentId,
        session: &str,
        to: &WindowName,
        token: Option<String>,
    ) -> Result<(), String> {
        let _ = (win, from, agent, session, to, token);
        Err(NOT_CARRIED_OUT.into())
    }
    /// In a window 2 with no window 1, starts the editor window detached from this process; it links to this
    /// window once it is up. `Err` says why none was started.
    fn start_editor(&self, win: WinId) -> Result<(), String>;
}

/// Why `PlaceWindow` is refused where no window manager adapter can move a window.
pub(crate) const NO_OUTPUT_MOVE: &str = "this window manager cannot move a window to an output";

/// How this process's own window is moved to an output, per window manager. Each is carried out off the GTK thread:
/// a command runs on the adapter's worker, GNOME's call is asynchronous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MonitorMove {
    /// sway names the window by this process's pid: the command runs as it is.
    Command(Vec<String>),
    /// Hyprland names the window by this process's pid too, but its command is written in the form its config
    /// manager takes, which only the worker learns: it builds the command with
    /// `eitri_core::compositor::placement_argv`.
    Hyprland {
        pid: u32,
        connector: Option<String>,
        role: eitri_core::wm::PlacementRole,
    },
    /// No output move or compositor focus is needed.
    Unchanged,
    /// niri names a window by its id: the worker reads the window list for the first window of `pid`, then moves
    /// it with `eitri_core::compositor::move_to_output_argv`.
    ById { pid: u32, connector: String },
    /// GNOME: the Eitri extension moves the windows of the process that asks.
    Gnome { connector: String },
}

/// The move of the window of process `pid` to the output `connector` under `wm`; `Err` is why there is none.
pub(crate) fn monitor_move(
    wm: eitri_core::compositor::Wm,
    pid: u32,
    connector: Option<&str>,
    role: eitri_core::wm::PlacementRole,
) -> Result<MonitorMove, String> {
    use eitri_core::compositor::{placement_argv, valid_output_name, HyprForm, WindowRef, Wm};
    if connector.is_some_and(|connector| !valid_output_name(connector)) {
        return Err(format!("{:?} is not an output name", connector.unwrap_or_default()));
    }
    let own = WindowRef {
        pid,
        id: None,
        focused: false,
        visible: true,
        rect: None,
        ..Default::default()
    };
    if connector.is_none() && !matches!(wm, Wm::Sway | Wm::Hyprland) {
        return Ok(MonitorMove::Unchanged);
    }
    match wm {
        // sway has one form. For Hyprland the legacy build only shows that there is a command; the worker writes
        // the one it runs.
        Wm::Sway => placement_argv(wm, HyprForm::Legacy, &own, connector, role)
            .map(MonitorMove::Command)
            .ok_or_else(|| NO_OUTPUT_MOVE.to_string()),
        Wm::Hyprland => placement_argv(wm, HyprForm::Legacy, &own, connector, role)
            .map(|_| MonitorMove::Hyprland {
                pid,
                connector: connector.map(str::to_string),
                role,
            })
            .ok_or_else(|| NO_OUTPUT_MOVE.to_string()),
        Wm::Niri => Ok(MonitorMove::ById {
            pid,
            connector: connector.unwrap().to_string(),
        }),
        Wm::Gnome => Ok(MonitorMove::Gnome {
            connector: connector.unwrap().to_string(),
        }),
        Wm::None => Err(NO_OUTPUT_MOVE.to_string()),
    }
}

/// What carries out a [`MonitorMove`]: the window manager adapter the shell's binary has, which runs commands on its
/// worker thread and calls GNOME's extension.
pub(crate) trait MonitorPlacer {
    /// The window manager the moves are for.
    fn wm(&self) -> eitri_core::compositor::Wm;
    /// Starts `how`; never waits.
    fn place(&self, how: MonitorMove);
}

/// What builds a module's surface for `CreateSurface` and releases it for `DropSurface`.
/// Builds the window's base chat page when the controller says so (`HostOp::BuildBasePage`), which it does once and
/// never in a hub or a page window.
pub(crate) trait BasePageMaker {
    fn build(&self);
}

pub(crate) trait SurfaceMaker {
    /// Reveals a prepared page in the same allocation that applies its committed arrangement.
    fn mapped(&self, _module: &ModuleId, _mapped: bool) {}

    /// Builds `module`'s host for `kind` as the window's startup builds one, places it off screen,
    /// registers it with the host and wires its controllers. A module whose host already exists keeps
    /// it as it is: a host never gets a second set of controllers. `Err` says why nothing was built.
    fn create(&self, win: WinId, module: &ModuleId, kind: &SurfaceKind) -> Result<(), String>;
    /// Releases what `why` ends: a killed web module's web process, a retired chat view's page. The
    /// host widget stays in the window for the module's next instance.
    fn drop_surface(&self, win: WinId, module: &ModuleId, why: &DropWhy) -> Result<(), String>;
    /// Makes the place of `module`, arriving from the other window, off screen at `size` (logical pixels; zero when
    /// the other window could not tell), registered with the host, before anything there is frozen. `Err` for a kind
    /// whose arrival is not built here, or why nothing was made.
    fn prepare(
        &self,
        win: WinId,
        module: &ModuleId,
        shape: &eitri_core::window_link::Shape,
        size: (u32, u32),
    ) -> Result<(), String> {
        let _ = (win, module, shape, size);
        Err(NOT_CARRIED_OUT.into())
    }
    /// Builds the prepared `module` from what a committed move carried: `fds` are this call's to keep or close.
    fn adopt(
        &self,
        win: WinId,
        module: &ModuleId,
        kind: eitri_core::wm::moves::MoveKind,
        record: &serde_json::Value,
        fds: Vec<std::os::fd::OwnedFd>,
    ) -> Result<(), String> {
        let _ = (win, module, kind, record, fds);
        Err(NOT_CARRIED_OUT.into())
    }
}

/// The pane operations a restart in place needs, independent of a registry or widget implementation.
pub(crate) trait RespawnPane {
    fn shell_start(&self) -> eitri_core::wm::ShellStart;
    fn close(&self);
    fn set_start_dir(&self, dir: PathBuf);
    fn start(&self);
}

/// Checks the captured shell before touching the pane: the same shell, given no key or mouse report since the read
/// that found it idle, since a command typed after the read runs in that same shell. No registry borrow outlives the
/// lookup.
pub(crate) fn respawn_pane(
    pane: Option<impl RespawnPane>,
    expect: eitri_core::wm::ShellStart,
    dir: PathBuf,
) -> Result<(), String> {
    let pane = pane.ok_or_else(|| "the terminal changed".to_string())?;
    if !expect.untouched(Some(pane.shell_start())) {
        return Err("the terminal changed".into());
    }
    pane.close();
    pane.set_start_dir(dir);
    pane.start();
    Ok(())
}

/// The window's services the controller's effect ops call. `Err` says why an op was not carried out.
pub(crate) trait ServiceSet {
    fn page_built(&self, _module: &ModuleId) -> bool {
        false
    }

    fn plugin_keys_help(&self, _module: &ModuleId) -> Option<eitri_core::plugin::keys::PluginKeysHelp> {
        None
    }
    fn chat_views(&self) -> Vec<ViewId> {
        Vec::new()
    }
    fn can_break_out(&self) -> Result<(), String> {
        Err(NOT_CARRIED_OUT.into())
    }
    /// As [`HostReads::chat_move`] reads it, of this window's panel; with no panel, no chat moves.
    fn chat_move(&self, view: ViewId, session: Option<&str>) -> Result<eitri_core::wm::ChatMoveFacts, String> {
        let _ = (view, session);
        Err(NOT_CARRIED_OUT.into())
    }
    fn running_chats(&self) -> Vec<(ModuleId, Option<eitri_core::tabs::TabId>)> {
        Vec::new()
    }
    fn terminal_job_modules(&self) -> Vec<ModuleId> {
        Vec::new()
    }
    fn pages_with_tasks(&self) -> Vec<ModuleId> {
        Vec::new()
    }
    fn base_tabs(&self) -> Vec<eitri_core::tabs::TabId> {
        Vec::new()
    }
    fn active_base_tab(&self) -> Option<eitri_core::tabs::TabId> {
        None
    }
    fn arrived_tab(&self, _mv: eitri_core::window_link::moves::MoveId) -> Option<eitri_core::tabs::TabId> {
        None
    }
    fn tab_move(&self, _tab: eitri_core::tabs::TabId) -> Result<eitri_core::wm::ChatMoveFacts, String> {
        Err(NOT_CARRIED_OUT.into())
    }
    fn tab_hand_over(&self, _tab: eitri_core::tabs::TabId) -> Result<eitri_core::wm::TabHandOver, String> {
        Err(NOT_CARRIED_OUT.into())
    }
    fn moved_tab_key(&self, _tab: eitri_core::tabs::TabId) -> Option<eitri_core::window_link::base_tabs::MovedTabKey> {
        None
    }
    fn order_base_tabs(
        &self,
        _tabs: &[eitri_core::tabs::TabId],
        _active: Option<eitri_core::tabs::TabId>,
    ) -> Result<(), String> {
        Err(NOT_CARRIED_OUT.into())
    }
    /// As [`HostReads::view_of_session`] reads it, of this window's panel; with no panel, no view holds one.
    fn view_of_session(&self, session: &str) -> Option<ViewId> {
        let _ = session;
        None
    }
    /// The window's tab set, as [`HostReads::tab_can_step`] reads it: the set is this window's panel's, so the
    /// read names no window. With no panel, no step switches.
    fn tab_can_step(&self, delta: i32) -> bool {
        let _ = delta;
        false
    }
    /// As [`HostReads::tab_has_last`] reads it, of this window's panel; with no panel, there is no last tab.
    fn tab_has_last(&self) -> bool {
        false
    }
    /// As [`HostReads::tab_place`] reads it, of this window's panel; with no panel, there is no such tab.
    fn tab_place(&self, n: u16) -> Option<eitri_core::tab_set::TabPlace> {
        let _ = n;
        None
    }

    /// As [`HostReads::terminal_jobs`] reads it: this window's terminals running a job, counted without waiting for
    /// a child. With no terminals, none.
    fn terminal_jobs(&self) -> usize {
        0
    }

    fn editor(&self, op: &EditorCall) -> Result<(), String>;
    /// Starts a fresh nvim in the window's released editor pane with `spawn`, as its first start launched one. A set
    /// with no editor restarts none.
    fn restart_editor(&self, spawn: &eitri_core::wm::EditorSpawn) -> Result<(), RestartRefused> {
        let _ = spawn;
        Err(RestartRefused::Unavailable(NOT_CARRIED_OUT.into()))
    }
    fn terminal(&self, op: &TermOp) -> Result<(), String>;
    fn plugin(&self, op: &PluginOp) -> Result<(), String>;
    fn chat(&self, op: &ChatOp) -> Result<(), String>;
    /// The teardown's steps that end no module (the pair, the scratch directory, the pane-switch channel, the
    /// feeds). A set that holds none of them carries none out.
    fn cleanup(&self, op: &CleanupOp) -> Result<(), String> {
        let _ = op;
        Err(NOT_CARRIED_OUT.into())
    }
    /// What the window decided about the open or raise request `id`, for its client. A set that holds no open socket
    /// has no request to answer.
    fn open_answer(&self, id: u64, accepted: bool) -> Result<(), String> {
        let _ = (id, accepted);
        Err(NOT_CARRIED_OUT.into())
    }
    /// What the pair file and the pages are told once the controller took back what a window held for the other
    /// window. A set with no pair carries none out.
    fn pair(&self, op: &eitri_core::wm::PairOp) -> Result<(), String> {
        let _ = op;
        Err(NOT_CARRIED_OUT.into())
    }
}

/// Why the editor's service did not restart the pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RestartRefused {
    /// The pane was not released: `state` names the state it was in.
    Pane { state: String },
    /// Nothing here restarts an editor: the words say why.
    Unavailable(String),
}

/// What the `Lua` ops run on: the window's Lua kernel. `Err` says why an op was not carried out.
pub(crate) trait LuaRunner {
    fn run(&self, op: &LuaOp) -> Result<(), String>;
}

/// A window's grid, as the host reads and drives it. A trait because the grid belongs to the shell's
/// binary, and this module is also compiled alone by its conformance test, which hands it a stub.
pub(crate) trait GridPort {
    /// Attaches native pointer reports to the window's event sink. A stand-in grid has no gestures to wire.
    fn set_pointer_sink(&self, win: WinId, sink: Weak<dyn HostSink>) {
        let _ = (win, sink);
    }

    /// The grid's allocation, or the stand-in content size before its first one.
    fn size(&self) -> Size;
    /// The divider thickness, as the grid's dividers measure.
    fn handle_px(&self) -> i32;
    /// A container strip's row height, as the grid's strips measure.
    fn strip_px(&self) -> i32;
    /// `module`'s minimum size, as its host measures.
    fn module_min(&self, module: &ModuleId) -> Size;
    /// Asks for one new measure and allocation of the grid.
    fn queue_allocation(&self);
    /// Puts the floats' hosts in `order`, bottom first, above everything tiled; nothing else moves.
    fn restack_floats(&self, order: &[ModuleId]);
    /// Whether the grid's probe places `module`'s host by hand, so no map or unmap may touch it.
    fn probe_places(&self, module: &ModuleId) -> bool;
    /// `module`'s host as the grid last allocated it; nothing for a host not laid out.
    fn module_size(&self, module: &ModuleId) -> Option<Size> {
        let _ = module;
        None
    }
}

/// What the host reads of a window's editor.
pub(crate) trait EditorReads {
    /// The editor's cell size in logical pixels, when it can tell.
    fn cell(&self) -> Option<(f64, f64)>;
    /// Whether the editor's child is running.
    fn running(&self) -> bool;
    /// The pane's facts, copied without waiting for a quit request's worker.
    fn facts(&self) -> eitri_core::wm::EditorFacts {
        eitri_core::wm::EditorFacts {
            running: self.running(),
            ..Default::default()
        }
    }
}

/// The `Map` and `Unmap` ops of `ops` the grid carries out, as each module and whether it is to be shown:
/// a host the grid's probe places by hand is left where the probe put it, as the grid's own apply leaves it.
pub(crate) fn hosts_to_map(ops: &[HostOp], probe_places: impl Fn(&ModuleId) -> bool) -> Vec<(WinId, ModuleId, bool)> {
    ops.iter()
        .filter_map(|op| match op {
            HostOp::Map { win, module } => Some((*win, module.clone(), true)),
            HostOp::Unmap { win, module } => Some((*win, module.clone(), false)),
            _ => None,
        })
        .filter(|(_, module, _)| !probe_places(module))
        .collect()
}

/// A window's chrome as the host draws it: its widgets, and the model last drawn on them.
struct ChromeDrawn {
    widgets: chrome::ChromeWidgets,
    last: RefCell<Option<ChromeModel>>,
}

pub(crate) struct GtkHost {
    me: Weak<GtkHost>,
    sink: RefCell<Option<Weak<dyn HostSink>>>,
    windows: RefCell<BTreeMap<WinId, glib::WeakRef<gtk4::Window>>>,
    surfaces: RefCell<BTreeMap<(WinId, ModuleId), gtk4::Widget>>,
    /// Windows being closed by `CloseWindow`: their `close-request` proceeds without asking.
    closing: RefCell<BTreeSet<WinId>>,
    /// Windows whose close request the sink is answering now, each with whether a `CloseWindow`
    /// for it arrived meanwhile (then the close under way proceeds).
    asking: RefCell<BTreeMap<WinId, bool>>,
    mode_toggle: RefCell<Option<Rc<ModeToggleFn>>>,
    surface_maker: RefCell<Option<Rc<dyn SurfaceMaker>>>,
    base_page_maker: RefCell<Option<Rc<dyn BasePageMaker>>>,
    services: RefCell<Option<Rc<dyn ServiceSet>>>,
    lua: RefCell<Option<Rc<dyn LuaRunner>>>,
    /// The application `OpenWindow` builds windows for; held weakly, as GTK holds its windows.
    application: RefCell<Option<glib::WeakRef<gtk4::Application>>>,
    focus_fn: RefCell<Option<Rc<FocusFn>>>,
    top_bar_fn: RefCell<Option<Rc<TopBarFn>>>,
    rename_entry_fn: RefCell<Option<Rc<RenameEntryFn>>>,
    onboarding_focus_fn: RefCell<Option<Rc<OnboardingFocusFn>>>,
    onboarding_fn: RefCell<Option<Rc<OnboardingFn>>>,
    project_host: RefCell<Option<Rc<dyn ProjectHost>>>,
    chrome_model: RefCell<Option<Rc<ChromeModelFn>>>,
    /// Each window's chrome widgets, with the model last drawn on them.
    chrome: RefCell<BTreeMap<WinId, Rc<ChromeDrawn>>>,
    chat_claims_fn: RefCell<Option<Rc<ChatClaimsFn>>>,
    view_attention_fn: RefCell<Option<Rc<ViewAttentionFn>>>,
    oldest_card_fn: RefCell<Option<Rc<OldestCardFn>>>,
    kill_counts_fn: RefCell<Option<Rc<KillCountsFn>>>,
    summary_fn: RefCell<Option<Rc<dyn Fn(WinId) -> eitri_core::wm::ChatSummary>>>,
    first_tab_fn: RefCell<Option<Rc<FirstTabFn>>>,
    hints: RefCell<Option<Rc<dyn HintPort>>>,
    linked_windows: RefCell<Option<Rc<dyn LinkedWindows>>>,
    pair_raise: RefCell<Option<Rc<dyn PairRaise>>>,
    placer: RefCell<Option<Rc<dyn MonitorPlacer>>>,
    grids: RefCell<BTreeMap<WinId, Rc<dyn GridPort>>>,
    editors: RefCell<BTreeMap<WinId, Rc<dyn EditorReads>>>,
    /// How many native focus reports have been posted, so a grab that GTK did not notify (the
    /// widget already held the keys) can still report where they are.
    focus_reports: Cell<u64>,
}

impl GtkHost {
    pub(crate) fn new() -> Rc<GtkHost> {
        Rc::new_cyclic(|me| GtkHost {
            me: me.clone(),
            sink: RefCell::new(None),
            windows: RefCell::new(BTreeMap::new()),
            surfaces: RefCell::new(BTreeMap::new()),
            closing: RefCell::new(BTreeSet::new()),
            asking: RefCell::new(BTreeMap::new()),
            mode_toggle: RefCell::new(None),
            surface_maker: RefCell::new(None),
            base_page_maker: RefCell::new(None),
            services: RefCell::new(None),
            lua: RefCell::new(None),
            application: RefCell::new(None),
            focus_fn: RefCell::new(None),
            top_bar_fn: RefCell::new(None),
            rename_entry_fn: RefCell::new(None),
            onboarding_focus_fn: RefCell::new(None),
            onboarding_fn: RefCell::new(None),
            project_host: RefCell::new(None),
            chrome_model: RefCell::new(None),
            chrome: RefCell::new(BTreeMap::new()),
            chat_claims_fn: RefCell::new(None),
            view_attention_fn: RefCell::new(None),
            oldest_card_fn: RefCell::new(None),
            kill_counts_fn: RefCell::new(None),
            summary_fn: RefCell::new(None),
            first_tab_fn: RefCell::new(None),
            hints: RefCell::new(None),
            linked_windows: RefCell::new(None),
            pair_raise: RefCell::new(None),
            placer: RefCell::new(None),
            grids: RefCell::new(BTreeMap::new()),
            editors: RefCell::new(BTreeMap::new()),
            focus_reports: Cell::new(0),
        })
    }

    /// Where the host reports. Held weakly: the sink (the driver) owns the host.
    pub(crate) fn attach(&self, sink: Weak<dyn HostSink>) {
        *self.sink.borrow_mut() = Some(sink.clone());
        let grids: Vec<_> = self
            .grids
            .borrow()
            .iter()
            .map(|(win, grid)| (*win, grid.clone()))
            .collect();
        for (win, grid) in grids {
            grid.set_pointer_sink(win, sink.clone());
        }
    }

    fn sink(&self) -> Option<Rc<dyn HostSink>> {
        let sink = self.sink.borrow().clone();
        sink.and_then(|s| s.upgrade())
    }

    fn post(&self, event: HostEvent) {
        match self.sink() {
            Some(sink) => sink.post(event),
            None => eprintln!("[wm] the GTK host has no sink attached: {event:?} dropped"),
        }
    }

    /// Takes `window` as `win`: its close requests, focus changes and activation are reported from
    /// now on, and its entries are forgotten when it is destroyed.
    pub(crate) fn adopt_window(&self, win: WinId, window: &gtk4::Window) {
        self.windows.borrow_mut().insert(win, window.downgrade());
        let me = self.me.clone();
        window.connect_close_request(move |_| {
            let Some(host) = me.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if host.closing.borrow().contains(&win) {
                return glib::Propagation::Proceed;
            }
            // Without a sink there is no one to keep the window open, so GTK's own answer stands.
            let Some(sink) = host.sink() else {
                return glib::Propagation::Proceed;
            };
            host.asking.borrow_mut().insert(win, false);
            let verdict = sink.close_requested(win, CloseBy::User);
            let closed_by_op = host.asking.borrow_mut().remove(&win).unwrap_or(false);
            if closed_by_op {
                // A `CloseWindow` the sink applied while answering is carried out by this close.
                return glib::Propagation::Proceed;
            }
            match verdict {
                CloseVerdict::Stop => glib::Propagation::Stop,
                CloseVerdict::Proceed => glib::Propagation::Proceed,
            }
        });
        let me = self.me.clone();
        window.connect_focus_widget_notify(move |_| {
            if let Some(host) = me.upgrade() {
                host.report_focus(win);
            }
        });
        let me = self.me.clone();
        window.connect_is_active_notify(move |window| {
            if let Some(host) = me.upgrade() {
                host.post(HostEvent::Active {
                    win,
                    active: window.is_active(),
                });
            }
        });
        let me = self.me.clone();
        window.connect_destroy(move |_| {
            if let Some(host) = me.upgrade() {
                host.windows.borrow_mut().remove(&win);
                host.asking.borrow_mut().remove(&win);
                host.surfaces.borrow_mut().retain(|(w, _), _| *w != win);
                host.grids.borrow_mut().remove(&win);
                host.editors.borrow_mut().remove(&win);
                host.chrome.borrow_mut().remove(&win);
            }
        });
    }

    /// Names `widget` as `module`'s surface in `win`: the widget focus, map and unmap act on, and
    /// the one a focus report names when the keys are inside it.
    pub(crate) fn register_surface(&self, win: WinId, module: ModuleId, widget: &gtk4::Widget) {
        self.surfaces.borrow_mut().insert((win, module), widget.clone());
    }

    /// What `SetMode` toggles the window's mode through. Until it is set, a `SetMode` is not carried out.
    pub(crate) fn set_mode_toggle(&self, toggle: Rc<ModeToggleFn>) {
        *self.mode_toggle.borrow_mut() = Some(toggle);
    }

    /// What `CreateSurface` and `DropSurface` build and release through.
    pub(crate) fn set_surface_maker(&self, maker: Rc<dyn SurfaceMaker>) {
        *self.surface_maker.borrow_mut() = Some(maker);
    }

    /// What the service ops call. Set once, before the first batch that carries one.
    pub(crate) fn set_services(&self, services: Rc<dyn ServiceSet>) {
        *self.services.borrow_mut() = Some(services);
    }

    /// What the `Lua` ops run on. Set before the first batch that carries one.
    pub(crate) fn set_base_page_maker(&self, maker: Rc<dyn BasePageMaker>) {
        *self.base_page_maker.borrow_mut() = Some(maker);
    }

    pub(crate) fn set_lua(&self, lua: Rc<dyn LuaRunner>) {
        *self.lua.borrow_mut() = Some(lua);
    }

    /// The application whose windows `OpenWindow` builds. Set before the first `OpenWindow`.
    pub(crate) fn set_application(&self, app: &gtk4::Application) {
        *self.application.borrow_mut() = Some(app.downgrade());
    }

    /// What `Focus` gives a module the keys through. Set before the first `Focus` on a window that
    /// holds modules; until then a `Focus` is the bare grab of the module's surface.
    pub(crate) fn set_focus(&self, focus: Rc<FocusFn>) {
        *self.focus_fn.borrow_mut() = Some(focus);
    }

    /// What a top-bar item's `Focus` goes through. Until it is set, such a `Focus` is not carried out.
    pub(crate) fn set_top_bar(&self, top_bar: Rc<TopBarFn>) {
        *self.top_bar_fn.borrow_mut() = Some(top_bar);
    }

    /// What the rename entry's `Focus` goes through. Until it is set, such a `Focus` is not carried out.
    pub(crate) fn set_rename_entry(&self, rename: Rc<RenameEntryFn>) {
        *self.rename_entry_fn.borrow_mut() = Some(rename);
    }

    /// What the first-start screen's `Focus` goes through. Until it is set, such a `Focus` is not carried out.
    pub(crate) fn set_onboarding_focus(&self, focus: Rc<OnboardingFocusFn>) {
        *self.onboarding_focus_fn.borrow_mut() = Some(focus);
    }

    /// What carries out the first-start screen's ops. Until it is set, they are not carried out.
    pub(crate) fn set_onboarding(&self, carry: Rc<OnboardingFn>) {
        *self.onboarding_fn.borrow_mut() = Some(carry);
    }

    pub(crate) fn set_project_host(&self, host: Rc<dyn ProjectHost>) {
        *self.project_host.borrow_mut() = Some(host);
    }

    /// Where a `Chrome` op reads the window's model. Until it is set, a `Chrome` is not carried out.
    pub(crate) fn set_chrome_model(&self, model: Rc<ChromeModelFn>) {
        *self.chrome_model.borrow_mut() = Some(model);
    }

    /// The widgets `win`'s chrome is drawn on, nothing drawn on them yet. Until they are set, a `Chrome` for `win`
    /// is not carried out.
    pub(crate) fn set_chrome_widgets(&self, win: WinId, widgets: chrome::ChromeWidgets) {
        let drawn = Rc::new(ChromeDrawn {
            widgets,
            last: RefCell::new(None),
        });
        self.chrome.borrow_mut().insert(win, drawn);
    }

    /// Draws what changed in `win`'s chrome model since it was last drawn, as a `Chrome` op does: the first draw
    /// once the widgets are set, and every announcement after it. `Err` says why nothing was drawn.
    pub(crate) fn draw_chrome(&self, win: WinId) -> Result<(), &'static str> {
        // Cloned out first: the widgets' setters move widgets, and GTK may call back into the host meanwhile.
        let model = self.chrome_model.borrow().clone();
        let drawn = self.chrome.borrow().get(&win).cloned();
        let (Some(model), Some(drawn)) = (model, drawn) else {
            return Err(NOT_CARRIED_OUT);
        };
        let Some(now) = model(win) else {
            return Err("the controller holds no such window");
        };
        // Taken while it draws, so a draw that comes back here meanwhile draws everything rather than finding it
        // borrowed.
        let mut last = drawn.last.take();
        chrome::reconcile(&drawn.widgets, &mut last, &now);
        *drawn.last.borrow_mut() = last;
        Ok(())
    }

    /// What a chat view holds for the user, as the controller reads it for the chips and the strips. Until it is
    /// set, every view holds nothing.
    pub(crate) fn set_view_attention(&self, attention: Rc<ViewAttentionFn>) {
        *self.view_attention_fn.borrow_mut() = Some(attention);
    }

    /// Whether a chat page claims a navigation key, as the controller reads it while it decides that key.
    /// Until it is set, no page claims one.
    pub(crate) fn set_chat_claims(&self, claims: Rc<ChatClaimsFn>) {
        *self.chat_claims_fn.borrow_mut() = Some(claims);
    }

    /// Which chat view holds the oldest waiting card, as the chat's key reads it. Until it is set, none does.
    pub(crate) fn set_oldest_card(&self, oldest: Rc<OldestCardFn>) {
        *self.oldest_card_fn.borrow_mut() = Some(oldest);
    }

    /// What a chat view's kill would end, as its y/n names it, and whether its tabs can close now. Until it is set,
    /// nothing runs in any view and every tab can close.
    pub(crate) fn set_chat_summary(&self, read: Rc<dyn Fn(WinId) -> eitri_core::wm::ChatSummary>) {
        *self.summary_fn.borrow_mut() = Some(read);
    }

    pub(crate) fn set_chat_kill_counts(&self, counts: Rc<KillCountsFn>) {
        *self.kill_counts_fn.borrow_mut() = Some(counts);
    }

    /// Whether a new chat view can be given its first tab, asked before it is placed. Until it is set, it can.
    pub(crate) fn set_chat_first_tab(&self, first_tab: Rc<FirstTabFn>) {
        *self.first_tab_fn.borrow_mut() = Some(first_tab);
    }

    /// What the `Hint` ops draw and toggle through. Until it is set, they are not carried out.
    pub(crate) fn set_hints(&self, hints: Rc<dyn HintPort>) {
        *self.hints.borrow_mut() = Some(hints);
    }

    /// Presents a paired window through its compositor adapter, including its first show.
    pub(crate) fn set_pair_raise(&self, raise: Rc<dyn PairRaise>) {
        *self.pair_raise.borrow_mut() = Some(raise);
    }

    /// What `PlaceWindow` moves this window through. Until it is set, every move is refused.
    pub(crate) fn set_monitor_placer(&self, placer: Rc<dyn MonitorPlacer>) {
        *self.placer.borrow_mut() = Some(placer);
    }

    /// What `OpenLinkedWindow` starts the second window through. Until it is set, none can be started.
    pub(crate) fn set_linked_windows(&self, linked: Rc<dyn LinkedWindows>) {
        *self.linked_windows.borrow_mut() = Some(linked);
    }

    /// `win`'s grid: what `Allocate` and `Restack` act on and what the grid reads answer from.
    pub(crate) fn set_grid(&self, win: WinId, grid: Rc<dyn GridPort>) {
        let sink = self.sink.borrow().clone();
        if let Some(sink) = sink {
            grid.set_pointer_sink(win, sink);
        }
        self.grids.borrow_mut().insert(win, grid);
    }

    /// `win`'s editor: what the editor reads answer from.
    pub(crate) fn set_editor(&self, win: WinId, editor: Rc<dyn EditorReads>) {
        self.editors.borrow_mut().insert(win, editor);
    }

    fn grid(&self, win: WinId) -> Option<Rc<dyn GridPort>> {
        self.grids.borrow().get(&win).cloned()
    }

    fn editor(&self, win: WinId) -> Option<Rc<dyn EditorReads>> {
        self.editors.borrow().get(&win).cloned()
    }

    /// The window the host holds as `win`, while it exists.
    pub(crate) fn window(&self, win: WinId) -> Option<gtk4::Window> {
        self.windows.borrow().get(&win).and_then(|w| w.upgrade())
    }

    /// Whether the host still holds `win` or any surface of it: false once the window is gone.
    pub(crate) fn knows_window(&self, win: WinId) -> bool {
        self.windows.borrow().contains_key(&win) || self.surfaces.borrow().keys().any(|(w, _)| *w == win)
    }

    fn surface(&self, win: WinId, module: &ModuleId) -> Option<gtk4::Widget> {
        self.surfaces.borrow().get(&(win, module.clone())).cloned()
    }

    /// The registered surfaces of `win` around its focus widget, innermost first; empty for a window
    /// with no focus widget, or one in no module.
    fn focus_chain(&self, window: &gtk4::Window, win: WinId) -> Vec<ModuleId> {
        let surfaces: Vec<(ModuleId, gtk4::Widget)> = self
            .surfaces
            .borrow()
            .iter()
            .filter(|((w, _), _)| *w == win)
            .map(|((_, m), widget)| (m.clone(), widget.clone()))
            .collect();
        let mut chain = Vec::new();
        let mut at = GtkWindowExt::focus(window);
        while let Some(widget) = at {
            if let Some((module, _)) = surfaces.iter().find(|(_, s)| *s == widget) {
                chain.push(module.clone());
            }
            at = widget.parent();
        }
        chain
    }

    /// Posts where `win`'s keys are now: the registered surfaces around the focus widget, innermost
    /// first, the innermost one owning the keys.
    fn report_focus(&self, win: WinId) {
        let Some(window) = self.window(win) else {
            return;
        };
        let chain = self.focus_chain(&window, win);
        self.focus_reports.set(self.focus_reports.get() + 1);
        self.post(HostEvent::NativeFocus {
            win,
            owner: chain.first().cloned().map(FocusOwner::Module),
            chain: FocusChain(chain),
            active: window.is_active(),
            op_index: None,
        });
    }

    /// The window's mode toggled as `prefix F11` toggles it, not set to the state the op names: the controller asks
    /// for the state its own toggle leads to, and a bare F11, nvim's `g:neovide_fullscreen` or the compositor may
    /// have changed the real mode since without the controller hearing, so applying that state could make the press
    /// do nothing where the toggle still changes the mode.
    fn set_mode(&self, op: &HostOp, win: WinId) -> Applied {
        // Cloned out first: the toggle calls into GTK, whose signals post to the sink.
        let toggle = self.mode_toggle.borrow().clone();
        let Some(toggle) = toggle else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        if self.window(win).is_none() {
            return self.refuse(op, "no such window");
        }
        toggle(win);
        Applied::Done
    }

    fn refuse(&self, op: &HostOp, why: &str) -> Applied {
        self.post(HostEvent::Refused {
            what: OpTag::of(op),
            why: why.into(),
            surface: op.surface(),
        });
        Applied::Refused
    }

    fn focus(&self, op: &HostOp, win: WinId, to: &FocusTo) -> Applied {
        if self.window(win).is_none() {
            return self.refuse(op, "no such window");
        }
        let module = match to {
            FocusTo::Module(module) => module,
            FocusTo::TopBarItem(item) => return self.focus_top_bar(op, win, *item),
            FocusTo::RenameEntry => return self.focus_rename_entry(op, win),
            FocusTo::Onboarding => return self.focus_onboarding(op, win),
            FocusTo::ProjectChooser => return self.focus_project_chooser(op, win),
            FocusTo::Nothing => return self.refuse(op, NOT_CARRIED_OUT),
        };
        // Cloned out first: the function grabs, and GTK notifies the focus change from inside it.
        let focus_fn = self.focus_fn.borrow().clone();
        let reports = self.focus_reports.get();
        if let Some(focus_fn) = focus_fn {
            if !focus_fn(module) {
                return self.refuse(op, "the shell did not give the module the keys");
            }
        } else {
            let Some(widget) = self.surface(win, module) else {
                return self.refuse(op, "the module has no surface in this window");
            };
            if !widget.is_mapped() {
                return self.refuse(op, "the module's surface is not on screen");
            }
            if !widget.grab_focus() {
                return self.refuse(op, "GTK did not move the keys to the module's surface");
            }
        }
        // A grab on the widget that already holds the keys changes nothing GTK notifies; report
        // anyway, so every focus that took is reported while it applies, as on any host.
        if self.focus_reports.get() == reports {
            self.report_focus(win);
        }
        Applied::Done
    }

    fn focus_top_bar(&self, op: &HostOp, win: WinId, item: u32) -> Applied {
        // Cloned out first: the grab notifies the focus change from inside it.
        let top_bar = self.top_bar_fn.borrow().clone();
        let Some(top_bar) = top_bar else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        if top_bar(win, item) {
            Applied::Done
        } else {
            self.refuse(op, "no top-bar item took the keys")
        }
    }

    fn focus_rename_entry(&self, op: &HostOp, win: WinId) -> Applied {
        // Cloned out first: the grab notifies the focus change from inside it.
        let rename = self.rename_entry_fn.borrow().clone();
        let Some(rename) = rename else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        if rename(win) {
            Applied::Done
        } else {
            self.refuse(op, "the rename entry did not take the keys")
        }
    }

    fn focus_onboarding(&self, op: &HostOp, win: WinId) -> Applied {
        // Cloned out first: the grab notifies the focus change from inside it.
        let focus = self.onboarding_focus_fn.borrow().clone();
        let Some(focus) = focus else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        if focus(win) {
            Applied::Done
        } else {
            self.refuse(op, "the first-start screen did not take the keys")
        }
    }

    fn onboarding(&self, op: &HostOp, call: &OnboardingOp) -> Applied {
        // Cloned out first: the work it starts may post an event at once.
        let carry = self.onboarding_fn.borrow().clone();
        match carry {
            Some(carry) => match carry(call) {
                Applied::Done => Applied::Done,
                _ => self.refuse(op, "the first-start screen's work was not started"),
            },
            None => self.refuse(op, NOT_CARRIED_OUT),
        }
    }

    fn focus_project_chooser(&self, op: &HostOp, win: WinId) -> Applied {
        let host = self.project_host.borrow().clone();
        let reports = self.focus_reports.get();
        if host.is_some_and(|host| host.focus(win)) {
            if self.focus_reports.get() == reports {
                self.report_focus(win);
            }
            Applied::Done
        } else {
            self.refuse(op, "the project chooser did not take the keys")
        }
    }

    fn project(&self, op: &HostOp, call: &ProjectOp) -> Applied {
        let win = match call {
            ProjectOp::List { win, .. }
            | ProjectOp::Path { win, .. }
            | ProjectOp::Read { win, .. }
            | ProjectOp::FolderDialog { win, .. }
            | ProjectOp::Open { win, .. } => *win,
        };
        if self.window(win).is_none() {
            return self.refuse(op, "no such window");
        }
        let host = self.project_host.borrow().clone();
        let applied = match (host, call) {
            // The token names the input that asked, so it is taken here, before the start leaves this thread.
            (Some(host), ProjectOp::Open { target, .. }) => host.open(win, target, self.activation_token(win)),
            (Some(host), _) => host.apply(call),
            (None, _) => Applied::Refused,
        };
        if applied == Applied::Done {
            Applied::Done
        } else {
            self.refuse(op, NOT_CARRIED_OUT)
        }
    }

    fn chrome(&self, op: &HostOp, win: WinId) -> Applied {
        match self.draw_chrome(win) {
            Ok(()) => Applied::Done,
            Err(why) => self.refuse(op, why),
        }
    }

    fn hint(&self, op: &HostOp, call: &HintOp) -> Applied {
        // Cloned out first: the digits draw into the window, and a HINT adds its controllers.
        let hints = self.hints.borrow().clone();
        let Some(hints) = hints else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        match call {
            HintOp::ShowDigits(digits) => hints.show_digits(WinId::MAIN, digits),
            HintOp::Hide => hints.hide_digits(WinId::MAIN),
            HintOp::Toggle { keycode } => hints.toggle(WinId::MAIN, *keycode),
        }
        Applied::Done
    }

    /// A `Map` or an `Unmap`: nothing for a host the grid's probe places by hand.
    fn map_or_unmap(&self, op: &HostOp, win: WinId) -> Applied {
        // Cloned out first: the probe's answer reads the grid, and nothing it calls should find the host borrowed.
        let grid = self.grid(win);
        let probe_places = |module: &ModuleId| grid.as_ref().is_some_and(|grid| grid.probe_places(module));
        match hosts_to_map(std::slice::from_ref(op), probe_places).pop() {
            Some((win, module, mapped)) => self.set_mapped(op, win, &module, mapped),
            None => Applied::Done,
        }
    }

    /// Runs `f` on `win`'s grid; refused for a window that has given none.
    fn on_grid(&self, op: &HostOp, win: WinId, f: impl FnOnce(&dyn GridPort)) -> Applied {
        // Cloned out first: the grid allocates and restacks, which may call back into the host.
        let Some(grid) = self.grid(win) else {
            return self.refuse(op, "the window has no grid");
        };
        f(grid.as_ref());
        Applied::Done
    }

    fn set_mapped(&self, op: &HostOp, win: WinId, module: &ModuleId, mapped: bool) -> Applied {
        let Some(widget) = self.surface(win, module) else {
            return self.refuse(op, "the module has no surface in this window");
        };
        if !mapped {
            self.release_keys_inside(win, &widget);
        }
        if let Some(maker) = self.surface_maker.borrow().clone() {
            maker.mapped(module, mapped);
        }
        widget.set_child_visible(mapped);
        Applied::Done
    }

    /// Takes the keys from a surface about to be hidden. GTK leaves the window's focus widget on a
    /// widget made child-invisible, with no notify, so the keys would go on into a surface no one
    /// can see; clearing the focus first moves them, and the focus report says so while the op
    /// applies.
    fn release_keys_inside(&self, win: WinId, widget: &gtk4::Widget) {
        let Some(window) = self.window(win) else {
            return;
        };
        let holds_keys =
            GtkWindowExt::focus(&window).is_some_and(|focus| focus == *widget || focus.is_ancestor(widget));
        if holds_keys {
            GtkWindowExt::set_focus(&window, None::<&gtk4::Widget>);
        }
    }

    fn create_surface(&self, op: &HostOp, win: WinId, module: &ModuleId, kind: &SurfaceKind) -> Applied {
        // Cloned out first: the maker adds widgets, and GTK may call back into the host meanwhile.
        let maker = self.surface_maker.borrow().clone();
        let Some(maker) = maker else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        if let Err(why) = maker.create(win, module, kind) {
            return self.refuse(op, &why);
        }
        if self.surface(win, module).is_none() {
            return self.refuse(op, "the surface was built but never registered");
        }
        Applied::Done
    }

    fn drop_surface(&self, op: &HostOp, win: WinId, module: &ModuleId, why: &DropWhy) -> Applied {
        if self.surface(win, module).is_none() {
            return self.refuse(op, "the module has no surface in this window");
        }
        let maker = self.surface_maker.borrow().clone();
        let Some(maker) = maker else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        match maker.drop_surface(win, module, why) {
            Ok(()) => Applied::Done,
            Err(why) => self.refuse(op, &why),
        }
    }

    fn prepare_surface(
        &self,
        op: &HostOp,
        win: WinId,
        module: &ModuleId,
        shape: &eitri_core::window_link::Shape,
        size: (u32, u32),
    ) -> Applied {
        // Cloned out first: the maker adds widgets, and GTK may call back into the host meanwhile.
        let maker = self.surface_maker.borrow().clone();
        let Some(maker) = maker else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        if let Err(why) = maker.prepare(win, module, shape, size) {
            return self.refuse(op, &why);
        }
        if self.surface(win, module).is_none() {
            return self.refuse(op, "the arriving module's place was made but never registered");
        }
        Applied::Done
    }

    /// The descriptors are taken out of the op here, whatever comes of it: refused, they close as they drop.
    fn adopt_moved(
        &self,
        op: &HostOp,
        win: WinId,
        module: &ModuleId,
        kind: eitri_core::wm::moves::MoveKind,
        record: &serde_json::Value,
        fds: &eitri_core::wm::Descriptors,
    ) -> Applied {
        let fds = fds.take();
        if self.surface(win, module).is_none() {
            return self.refuse(op, "the arriving module has no place in this window");
        }
        let maker = self.surface_maker.borrow().clone();
        let Some(maker) = maker else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        match maker.adopt(win, module, kind, record, fds) {
            Ok(()) => Applied::Done,
            Err(why) => self.refuse(op, &why),
        }
    }

    fn end_peer(&self, op: &HostOp, win: WinId, peer: PeerId) -> Applied {
        let linked = self.linked_windows.borrow().clone();
        let Some(linked) = linked else {
            return self.refuse(op, eitri_core::wm::feedback::NO_SECOND_WINDOW_TEXT);
        };
        match linked.end_peer(win, peer) {
            Ok(()) => Applied::Done,
            Err(why) => self.refuse(op, &why),
        }
    }

    /// Builds `win`'s window, created and not shown, and adopts it. Undecorated: the window's own top
    /// bar is its only titlebar.
    fn open_window(&self, op: &HostOp, win: WinId, spec: &WindowSpec) -> Applied {
        if self.window(win).is_some() {
            return self.refuse(op, "the window is already open");
        }
        let app = self.application.borrow().as_ref().and_then(|app| app.upgrade());
        let Some(app) = app else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        let window = gtk4::ApplicationWindow::builder()
            .application(&app)
            .title(spec.title.as_str())
            .default_width(1280)
            .default_height(760)
            .decorated(false)
            .build();
        self.adopt_window(win, window.upcast_ref());
        Applied::Done
    }

    /// Shows `win`, with the launcher's activation token when the controller has one; without one GTK
    /// uses the token it read from the environment at startup, if any.
    fn present(&self, op: &HostOp, win: WinId, activation: Option<&ActivationToken>) -> Applied {
        let Some(window) = self.window(win) else {
            return self.refuse(op, "no such window");
        };
        let raise = self.pair_raise.borrow().clone();
        if let Some(raise) = raise {
            raise.raise(&window, activation.map(|token| token.0.as_str()));
        } else {
            if let Some(token) = activation {
                window.set_startup_id(&token.0);
            }
            window.present();
        }
        Applied::Done
    }

    /// An activation token from `win`'s display, naming the surface with the keys and the seat's last input, for the
    /// other window of the pair to come forward with. `None` when the compositor gives none.
    fn activation_token(&self, win: WinId) -> Option<String> {
        let window = self.window(win)?;
        let context = WidgetExt::display(&window).app_launch_context();
        context
            .startup_notify_id(None::<&gtk4::gio::AppInfo>, &[])
            .map(|token| token.to_string())
            .filter(|token| !token.is_empty())
    }

    fn open_linked_window(&self, op: &HostOp, win: WinId) -> Applied {
        if self.window(win).is_none() {
            return self.refuse(op, "no such window");
        }
        // Cloned out first: the start wires the new link and may call back into GTK.
        let linked = self.linked_windows.borrow().clone();
        let Some(linked) = linked else {
            return self.refuse(op, eitri_core::wm::feedback::NO_SECOND_WINDOW_TEXT);
        };
        match linked.open(win, &WindowName::Second, self.activation_token(win)) {
            Ok(()) => Applied::Done,
            Err(why) => self.refuse(op, &why),
        }
    }

    fn restore_linked_window(&self, op: &HostOp, win: WinId) -> Applied {
        if self.window(win).is_none() {
            return self.refuse(op, "no such window");
        }
        // Cloned out first: the start wires the new link and may call back into GTK.
        let linked = self.linked_windows.borrow().clone();
        let Some(linked) = linked else {
            return self.refuse(op, eitri_core::wm::feedback::NO_SECOND_WINDOW_TEXT);
        };
        match linked.restore(win, &WindowName::Second) {
            Ok(()) => Applied::Done,
            Err(why) => self.refuse(op, &why),
        }
    }

    /// Starts the module window `name`, or raises it when it runs. The window that comes forward takes the activation
    /// token of the key that asked, or of this window's surface when none came; a window the start opens takes none, and
    /// leaves the keys where they are.
    fn open_module_window(
        &self,
        op: &HostOp,
        win: WinId,
        name: &WindowName,
        token: Option<&str>,
        keep_keys: bool,
    ) -> Applied {
        // Cloned out first: the start wires the new link and may call back into GTK.
        let linked = self.linked_windows.borrow().clone();
        let Some(linked) = linked else {
            return self.refuse(op, eitri_core::wm::feedback::NO_SECOND_WINDOW_TEXT);
        };
        let token = if keep_keys {
            None
        } else {
            match token {
                Some(token) => Some(token.to_string()),
                None if self.window(win).is_some() => self.activation_token(win),
                None => None,
            }
        };
        match linked.open_module(win, name, token, keep_keys) {
            Ok(()) => Applied::Done,
            Err(why) => self.refuse(op, &why),
        }
    }

    /// A raise a module window asked for: the token that came with it is the one the window that comes forward gets,
    /// so it is answered as it would be had this window made one.
    fn forward_raise(&self, peer: PeerId, token: Option<&str>) -> Applied {
        self.post(HostEvent::Peer(PeerEvent::Raised {
            peer,
            token: token.map(str::to_string),
        }));
        Applied::Done
    }

    /// The key action `name` goes to the module window `peer`, which parses and runs it itself.
    fn peer_run(
        &self,
        op: &HostOp,
        win: WinId,
        peer: PeerId,
        name: &str,
        opts: &[(String, serde_json::Value)],
    ) -> Applied {
        let linked = self.linked_windows.borrow().clone();
        let Some(linked) = linked else {
            return self.refuse(op, eitri_core::wm::feedback::NO_SECOND_WINDOW_TEXT);
        };
        let msg = Msg::RunAction {
            name: name.to_string(),
            opts: opts.to_vec(),
        };
        match linked.send(win, peer, msg) {
            Ok(()) => Applied::Done,
            Err(why) => self.refuse(op, &why),
        }
    }

    /// A module window asks window 1 for another window, with an activation token from its own surface for the window
    /// that comes forward.
    fn hub_ask(&self, op: &HostOp, win: WinId, ask: &eitri_core::window_link::HubAsk) -> Applied {
        let linked = self.linked_windows.borrow().clone();
        let Some(linked) = linked else {
            return self.refuse(op, eitri_core::wm::feedback::NO_SECOND_WINDOW_TEXT);
        };
        let msg = Msg::HubAsk {
            ask: ask.clone(),
            token: self.activation_token(win),
        };
        match linked.send_to_hub(win, msg) {
            Ok(()) => Applied::Done,
            Err(why) => self.refuse(op, &why),
        }
    }

    /// Window 1 hands a tab between module windows; the links carry the rest.
    fn hand_over_tab(
        &self,
        op: &HostOp,
        win: WinId,
        from: PeerId,
        (agent, session): (&eitri_core::agent_backend::AgentId, &str),
        to: &WindowName,
        token: Option<&str>,
    ) -> Applied {
        let linked = self.linked_windows.borrow().clone();
        let Some(linked) = linked else {
            return self.refuse(op, eitri_core::wm::feedback::NO_SECOND_WINDOW_TEXT);
        };
        match linked.hand_over(win, from, agent, session, to, token.map(str::to_string)) {
            Ok(()) => Applied::Done,
            Err(why) => self.refuse(op, &why),
        }
    }

    /// Window 1 tells a module window that the tab it offered stays there.
    fn keep_tab(&self, op: &HostOp, win: WinId, peer: PeerId, session: &str, why: &str) -> Applied {
        let linked = self.linked_windows.borrow().clone();
        let Some(linked) = linked else {
            return self.refuse(op, eitri_core::wm::feedback::NO_SECOND_WINDOW_TEXT);
        };
        let msg = Msg::TabKept {
            session: session.to_string(),
            why: why.to_string(),
        };
        match linked.send(win, peer, msg) {
            Ok(()) => Applied::Done,
            Err(why) => self.refuse(op, &why),
        }
    }

    /// A focus move past this window's edge: the compositor's own tool moves it, on its worker. With none, the notice
    /// that says so is the refusal, shown once.
    fn compositor_focus(&self, op: &HostOp, direction: Direction) -> Applied {
        let raise = self.pair_raise.borrow().clone();
        let Some(raise) = raise else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        let notice = RefCell::new(None::<String>);
        if raise.move_focus(direction, &|text| *notice.borrow_mut() = Some(text.to_string())) {
            return Applied::Done;
        }
        match notice.into_inner() {
            Some(text) => self.refuse(op, &text),
            None => Applied::Refused,
        }
    }

    fn start_editor_window(&self, op: &HostOp, win: WinId) -> Applied {
        if self.window(win).is_none() {
            return self.refuse(op, "no such window");
        }
        let linked = self.linked_windows.borrow().clone();
        let Some(linked) = linked else {
            return self.refuse(op, eitri_core::wm::feedback::EDITOR_WINDOW_CLOSED_TEXT);
        };
        match linked.start_editor(win) {
            Ok(()) => Applied::Done,
            Err(why) => self.refuse(op, &why),
        }
    }

    /// Only this process's own window can be moved: the window manager is told its pid (or GNOME's extension finds
    /// the caller's windows), so `win` must be one of this host's.
    fn move_to_monitor(
        &self,
        op: &HostOp,
        win: WinId,
        connector: Option<&str>,
        role: eitri_core::wm::PlacementRole,
    ) -> Applied {
        if self.window(win).is_none() {
            return self.refuse(op, "no such window");
        }
        let placer = self.placer.borrow().clone();
        let Some(placer) = placer else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        match monitor_move(placer.wm(), std::process::id(), connector, role) {
            Ok(how) => {
                placer.place(how);
                Applied::Done
            }
            Err(why) => self.refuse(op, &why),
        }
    }

    /// The other window comes forward by its own raise once it hears `focus{window}` with this token: under GNOME
    /// the token lets it take the keys from this window, and under sway, Hyprland and niri, which turn a token into
    /// an urgency hint, it also has the compositor focus it by its pid. The controller sends that message on the
    /// answer posted here.
    fn raise_peer(&self, op: &HostOp, win: WinId, peer: PeerId) -> Applied {
        if self.window(win).is_none() {
            return self.refuse(op, "no such window");
        }
        let token = self.activation_token(win);
        self.post(HostEvent::Peer(PeerEvent::Raised { peer, token }));
        Applied::Done
    }

    fn lua(&self, op: &HostOp, call: &LuaOp) -> Applied {
        // Cloned out first: the kernel runs Lua, which may post to the sink or call into GTK.
        let lua = self.lua.borrow().clone();
        let Some(lua) = lua else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        match lua.run(call) {
            Ok(()) => Applied::Done,
            Err(why) => self.refuse(op, &why),
        }
    }

    fn service(&self, op: &HostOp, call: impl FnOnce(&dyn ServiceSet) -> Result<(), String>) -> Applied {
        // Cloned out first: a service may post to the sink or call into GTK.
        let services = self.services.borrow().clone();
        let Some(services) = services else {
            return self.refuse(op, NOT_CARRIED_OUT);
        };
        match call(services.as_ref()) {
            Ok(()) => Applied::Done,
            Err(why) => self.refuse(op, &why),
        }
    }

    /// The restart's refusal, as any failed op's and as its own fact naming the pane's state, so the editor can be
    /// shown and tried again; a window with nothing to restart says `unavailable`.
    fn restart_editor(&self, op: &HostOp, win: WinId, spawn: &eitri_core::wm::EditorSpawn) -> Applied {
        // Cloned out first: the pane's restart may queue a render, and a service may post to the sink.
        let services = self.services.borrow().clone();
        let refused = match services {
            Some(services) => match services.restart_editor(spawn) {
                Ok(()) => return Applied::Done,
                Err(refused) => refused,
            },
            None => RestartRefused::Unavailable(NOT_CARRIED_OUT.into()),
        };
        let (why, state) = match refused {
            RestartRefused::Pane { state } => (
                format!("the editor pane is {state}; only a released pane restarts"),
                state,
            ),
            RestartRefused::Unavailable(why) => (why, "unavailable".to_string()),
        };
        let applied = self.refuse(op, &why);
        self.post(HostEvent::Editor(EditorEvent::RestartRefused { win, state }));
        applied
    }

    fn close_window(&self, op: &HostOp, win: WinId) -> Applied {
        let Some(window) = self.window(win) else {
            return self.refuse(op, "no such window");
        };
        if let Some(noted) = self.asking.borrow_mut().get_mut(&win) {
            // Inside this window's own close request GTK ignores `close()`; the request's handler
            // lets the close under way proceed once the sink has answered.
            *noted = true;
            return Applied::Done;
        }
        if !window.is_realized() {
            // GTK ignores `close()` on a window that was never realized; nothing can have been
            // shown in it, so it is destroyed without a close request either way.
            window.destroy();
            return Applied::Done;
        }
        self.closing.borrow_mut().insert(win);
        window.close();
        self.closing.borrow_mut().remove(&win);
        Applied::Done
    }
}

impl Host for GtkHost {
    fn apply(&self, op: &HostOp) -> Applied {
        // Every variant is named, so a new op is a compile error here until this shell decides.
        match op {
            HostOp::Focus { win, to, on_grab: _ } => self.focus(op, *win, to),
            HostOp::Map { win, .. } | HostOp::Unmap { win, .. } => self.map_or_unmap(op, *win),
            HostOp::Allocate { win } => self.on_grid(op, *win, |grid| grid.queue_allocation()),
            // The floats only, bottom first: each move places its float after the one below it.
            HostOp::Restack { win, moves } => {
                let order: Vec<ModuleId> = moves.iter().map(|step| step.module.clone()).collect();
                self.on_grid(op, *win, |grid| grid.restack_floats(&order))
            }
            HostOp::CloseWindow { win } => self.close_window(op, *win),
            HostOp::CreateSurface { win, module, kind } => self.create_surface(op, *win, module, kind),
            HostOp::DropSurface { win, module, why } => self.drop_surface(op, *win, module, why),
            HostOp::Chrome { win, .. } => self.chrome(op, *win),
            HostOp::Hint(call) => self.hint(op, call),
            HostOp::Onboarding(call) => self.onboarding(op, call),
            HostOp::Project(call) => self.project(op, call),
            HostOp::Editor(call @ EditorCall::Quit { win, generation, .. }) => {
                let applied = self.service(op, |s| s.editor(call));
                // A refusal names no call, so the quit that sent nothing is told as its own fact, with its
                // generation, for the quit flow to clear it.
                if applied == Applied::Refused {
                    self.post(HostEvent::Editor(EditorEvent::QuitNotSent {
                        win: *win,
                        generation: *generation,
                    }));
                }
                applied
            }
            // Told as a fact either way: the restart a show asked for waits for the release.
            HostOp::Editor(call @ EditorCall::ReleaseExited { win }) => {
                let applied = self.service(op, |s| s.editor(call));
                self.post(HostEvent::Editor(match applied {
                    Applied::Done => EditorEvent::Released { win: *win },
                    _ => EditorEvent::ReleaseRefused { win: *win },
                }));
                applied
            }
            HostOp::Editor(EditorCall::Restart { win, spawn }) => self.restart_editor(op, *win, spawn),
            HostOp::Editor(call) => self.service(op, |s| s.editor(call)),
            HostOp::Terminal(call) => self.service(op, |s| s.terminal(call)),
            HostOp::Plugin(call) => self.service(op, |s| s.plugin(call)),
            HostOp::Chat(call) => self.service(op, |s| s.chat(call)),
            HostOp::OpenWindow { win, spec } => self.open_window(op, *win, spec),
            HostOp::Present { win, activation } => self.present(op, *win, activation.as_ref()),
            HostOp::OpenAnswer { id, accepted } => self.service(op, |s| s.open_answer(*id, *accepted)),
            HostOp::Lua(call) => self.lua(op, call),
            HostOp::OpenLinkedWindow { win } => self.open_linked_window(op, *win),
            HostOp::RestoreLinkedWindow { win } => self.restore_linked_window(op, *win),
            HostOp::StartEditorWindow { win } => self.start_editor_window(op, *win),
            HostOp::PlaceWindow { win, monitor, role } => {
                self.move_to_monitor(op, *win, monitor.as_ref().map(|m| m.0.as_str()), *role)
            }
            HostOp::RaisePeer { win, peer } => self.raise_peer(op, *win, *peer),
            // An arriving module is built by its kind's own code: a refused prepare makes this window refuse the offer
            // before the other window has frozen anything.
            HostOp::PrepareSurface {
                win,
                module,
                shape,
                size,
            } => self.prepare_surface(op, *win, module, shape, *size),
            HostOp::AdoptMoved {
                win,
                module,
                kind,
                record,
                fds,
            } => self.adopt_moved(op, *win, module, *kind, record, fds),
            HostOp::OrderBaseTabs { tabs, active, .. } => self.service(op, |s| s.order_base_tabs(tabs, *active)),
            HostOp::EndPeer { win, peer } => self.end_peer(op, *win, *peer),
            HostOp::Cleanup(call) => self.service(op, |s| s.cleanup(call)),
            HostOp::Pair(call) => self.service(op, |s| s.pair(call)),
            HostOp::SetMode { win, .. } => self.set_mode(op, *win),
            HostOp::OpenModuleWindow {
                win,
                name,
                token,
                keep_keys,
            } => self.open_module_window(op, *win, name, token.as_deref(), *keep_keys),
            HostOp::ForwardRaise { peer, token, .. } => self.forward_raise(*peer, token.as_deref()),
            HostOp::PeerRun { win, peer, name, opts } => self.peer_run(op, *win, *peer, name, opts),
            HostOp::CompositorFocus { direction, .. } => self.compositor_focus(op, *direction),
            HostOp::HubAsk { win, ask } => self.hub_ask(op, *win, ask),
            HostOp::BuildBasePage { .. } => {
                // Cloned out first: the page's build reaches the panel, and through it the host again.
                let maker = self.base_page_maker.borrow().clone();
                match maker {
                    Some(maker) => {
                        maker.build();
                        Applied::Done
                    }
                    None => self.refuse(op, NOT_CARRIED_OUT),
                }
            }
            HostOp::HandOverTab {
                win,
                from,
                agent,
                session,
                to,
                token,
            } => self.hand_over_tab(op, *win, *from, (agent, session), to, token.as_deref()),
            HostOp::KeepTab {
                win,
                peer,
                session,
                why,
            } => self.keep_tab(op, *win, *peer, session, why),
            HostOp::Quit => self.refuse(op, NOT_CARRIED_OUT),
        }
    }

    fn monitors(&self) -> Vec<Monitor> {
        let Some(display) = gdk::Display::default() else {
            return Vec::new();
        };
        let list = display.monitors();
        (0..list.n_items())
            .filter_map(|i| Some((i, list.item(i)?.downcast::<gdk::Monitor>().ok()?)))
            .map(|(i, monitor)| {
                let geometry = monitor.geometry();
                let id = monitor
                    .connector()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| format!("monitor-{i}"));
                Monitor {
                    name: monitor
                        .description()
                        .map(|d| d.to_string())
                        .unwrap_or_else(|| id.clone()),
                    id: MonitorId(id),
                    rect: Rect {
                        x: geometry.x(),
                        y: geometry.y(),
                        w: geometry.width(),
                        h: geometry.height(),
                    },
                    scale: monitor.scale(),
                }
            })
            .collect()
    }
}

/// Every read answers at once from the window's grid and editor, and calls nothing that could reach the
/// controller; a window that has given neither answers as [`NoReads`] does.
impl HostReads for GtkHost {
    fn project_chooser_env(&self, win: WinId) -> ChooserEnv {
        let host = self.project_host.borrow().clone();
        host.map_or_else(|| NoReads.project_chooser_env(win), |host| host.env(win))
    }

    /// The name of the window manager this window's moves go through, which `companion.wm` picks.
    fn compositor_name(&self) -> &'static str {
        let wm = self.placer.borrow().as_ref().map(|placer| placer.wm());
        eitri_core::compositor::mode::compositor_name(wm.unwrap_or(eitri_core::compositor::Wm::None))
    }
    fn module_window_open(&self, name: &WindowName) -> bool {
        let linked = self.linked_windows.borrow().clone();
        linked.is_some_and(|linked| linked.module_window_open(name))
    }
    fn page_built(&self, _win: WinId, module: &ModuleId) -> bool {
        self.services
            .borrow()
            .as_ref()
            .is_some_and(|services| services.page_built(module))
    }

    fn plugin_keys_help(&self, _win: WinId, module: &ModuleId) -> Option<eitri_core::plugin::keys::PluginKeysHelp> {
        let services = self.services.borrow().clone();
        services.and_then(|services| services.plugin_keys_help(module))
    }
    fn chat_views(&self) -> Vec<ViewId> {
        let services = self.services.borrow().clone();
        services.map_or_else(Vec::new, |services| services.chat_views())
    }
    fn can_break_out(&self, _win: WinId) -> Result<(), String> {
        let services = self.services.borrow().clone();
        services.map_or_else(|| Err(NOT_CARRIED_OUT.into()), |services| services.can_break_out())
    }
    fn chat_move(
        &self,
        _win: WinId,
        view: ViewId,
        session: Option<&str>,
    ) -> Result<eitri_core::wm::ChatMoveFacts, String> {
        let services = self.services.borrow().clone();
        services.map_or_else(
            || Err(NOT_CARRIED_OUT.into()),
            |services| services.chat_move(view, session),
        )
    }
    fn running_chats(&self, _win: WinId) -> Vec<(ModuleId, Option<eitri_core::tabs::TabId>)> {
        let services = self.services.borrow().clone();
        services.map_or_else(|| Vec::new(), |s| s.running_chats())
    }
    fn terminal_job_modules(&self, _win: WinId) -> Vec<ModuleId> {
        let services = self.services.borrow().clone();
        services.map_or_else(|| Vec::new(), |s| s.terminal_job_modules())
    }
    fn pages_with_tasks(&self, _win: WinId) -> Vec<ModuleId> {
        let services = self.services.borrow().clone();
        services.map_or_else(|| Vec::new(), |s| s.pages_with_tasks())
    }
    fn base_tabs(&self, _win: WinId) -> Vec<eitri_core::tabs::TabId> {
        let services = self.services.borrow().clone();
        services.map_or_else(|| Vec::new(), |s| s.base_tabs())
    }
    fn active_base_tab(&self, _win: WinId) -> Option<eitri_core::tabs::TabId> {
        let services = self.services.borrow().clone();
        services.map_or_else(|| None, |s| s.active_base_tab())
    }
    fn arrived_tab(&self, _win: WinId, mv: eitri_core::window_link::moves::MoveId) -> Option<eitri_core::tabs::TabId> {
        let services = self.services.borrow().clone();
        services.map_or_else(|| None, |s| s.arrived_tab(mv))
    }
    fn tab_move(&self, _win: WinId, tab: eitri_core::tabs::TabId) -> Result<eitri_core::wm::ChatMoveFacts, String> {
        let services = self.services.borrow().clone();
        services.map_or_else(|| Err(NOT_CARRIED_OUT.into()), |s| s.tab_move(tab))
    }
    fn tab_hand_over(&self, _win: WinId, tab: eitri_core::tabs::TabId) -> Result<eitri_core::wm::TabHandOver, String> {
        let services = self.services.borrow().clone();
        services.map_or_else(|| Err(NOT_CARRIED_OUT.into()), |s| s.tab_hand_over(tab))
    }
    fn moved_tab_key(
        &self,
        _win: WinId,
        tab: eitri_core::tabs::TabId,
    ) -> Option<eitri_core::window_link::base_tabs::MovedTabKey> {
        let services = self.services.borrow().clone();
        services.map_or_else(|| None, |s| s.moved_tab_key(tab))
    }
    fn view_of_session(&self, _win: WinId, session: &str) -> Option<ViewId> {
        let services = self.services.borrow().clone();
        services.and_then(|services| services.view_of_session(session))
    }
    fn tab_can_step(&self, _win: WinId, delta: i32) -> bool {
        let services = self.services.borrow().clone();
        services.is_some_and(|services| services.tab_can_step(delta))
    }
    fn tab_has_last(&self, _win: WinId) -> bool {
        let services = self.services.borrow().clone();
        services.is_some_and(|services| services.tab_has_last())
    }
    fn tab_place(&self, _win: WinId, n: u16) -> Option<eitri_core::tab_set::TabPlace> {
        let services = self.services.borrow().clone();
        services.and_then(|services| services.tab_place(n))
    }

    fn grid_size(&self, win: WinId) -> Size {
        match self.grid(win) {
            Some(grid) => grid.size(),
            None => NoReads.grid_size(win),
        }
    }
    fn handle_px(&self, win: WinId) -> i32 {
        match self.grid(win) {
            Some(grid) => grid.handle_px(),
            None => NoReads.handle_px(win),
        }
    }
    fn strip_px(&self, win: WinId) -> i32 {
        match self.grid(win) {
            Some(grid) => grid.strip_px(),
            None => NoReads.strip_px(win),
        }
    }
    fn module_min(&self, win: WinId, module: &ModuleId) -> Size {
        match self.grid(win) {
            Some(grid) => grid.module_min(module),
            None => NoReads.module_min(win, module),
        }
    }
    fn editor_cell(&self, win: WinId) -> Option<(f64, f64)> {
        self.editor(win).and_then(|editor| editor.cell())
    }
    /// The module's host as the grid last allocated it; nothing before its first allocation.
    fn module_size(&self, win: WinId, module: &ModuleId) -> Option<Size> {
        self.grid(win)?.module_size(module)
    }
    fn editor_facts(&self, win: WinId) -> eitri_core::wm::EditorFacts {
        self.editor(win).map_or_else(Default::default, |editor| editor.facts())
    }
    fn terminal_jobs(&self, _win: WinId) -> usize {
        let services = self.services.borrow().clone();
        services.map_or(0, |services| services.terminal_jobs())
    }
    fn editor_running(&self, win: WinId) -> bool {
        self.editor(win).is_some_and(|editor| editor.running())
    }
    /// The chat page's own answer, read from what the page last reported; none is claimed before the shell
    /// gives the read.
    fn chat_claims(&self, view: ViewId, direction: Direction) -> bool {
        let claims = self.chat_claims_fn.borrow().clone();
        match claims {
            Some(claims) => claims(view, direction),
            None => NoReads.chat_claims(view, direction),
        }
    }
    /// The panel's own count for the view; nothing held before the shell gives the read.
    fn view_attention(&self, view: ViewId) -> Attention {
        let attention = self.view_attention_fn.borrow().clone();
        match attention {
            Some(attention) => attention(view),
            None => NoReads.view_attention(view),
        }
    }
    /// The panel's own answer; none before the shell gives the read.
    fn oldest_card_view(&self, win: WinId) -> Option<ViewId> {
        let oldest = self.oldest_card_fn.borrow().clone();
        match oldest {
            Some(oldest) => oldest(win),
            None => NoReads.oldest_card_view(win),
        }
    }
    /// The panel's own counts for the view; nothing running before the shell gives the read.
    fn chat_summary(&self, win: WinId) -> eitri_core::wm::ChatSummary {
        self.summary_fn
            .borrow()
            .as_ref()
            .map_or_else(Default::default, |read| read(win))
    }
    fn chat_kill_counts(&self, view: ViewId) -> ChatKillCounts {
        let counts = self.kill_counts_fn.borrow().clone();
        match counts {
            Some(counts) => counts(view),
            None => NoReads.chat_kill_counts(view),
        }
    }
    /// The panel's own answer; a view can be given its tab before the shell gives the read.
    fn chat_first_tab(&self, view: ViewId, breaks: bool) -> Result<(), String> {
        let first_tab = self.first_tab_fn.borrow().clone();
        match first_tab {
            Some(first_tab) => first_tab(view, breaks),
            None => NoReads.chat_first_tab(view, breaks),
        }
    }
}

#[cfg(test)]
mod monitor_move_tests {
    use super::*;
    use eitri_core::compositor::Wm;
    use eitri_core::wm::PlacementRole;

    #[test]
    fn placement_focus_follows_the_role_on_each_compositor() {
        let restored = PlacementRole::RestoredSecond { first_pid: 101 };
        for (role, focus_pid) in [(PlacementRole::NewSecond, 4242), (restored, 101)] {
            assert_eq!(
                monitor_move(Wm::Sway, 4242, Some("DP-2"), role),
                Ok(MonitorMove::Command(vec![
                    "swaymsg".into(),
                    format!("[pid=4242] move container to output \"DP-2\"; [pid={focus_pid}] focus")
                ]))
            );
            assert_eq!(
                monitor_move(Wm::Hyprland, 4242, Some("DP-2"), role),
                Ok(MonitorMove::Hyprland {
                    pid: 4242,
                    connector: Some("DP-2".into()),
                    role
                })
            );
        }
        assert_eq!(
            monitor_move(Wm::Sway, 4242, None, restored),
            Ok(MonitorMove::Command(vec!["swaymsg".into(), "[pid=101] focus".into()]))
        );
        assert_eq!(
            monitor_move(Wm::Hyprland, 4242, None, restored),
            Ok(MonitorMove::Hyprland {
                pid: 4242,
                connector: None,
                role: restored
            })
        );
        for role in [PlacementRole::Main, PlacementRole::NewSecond, restored] {
            assert_eq!(
                monitor_move(Wm::Niri, 4242, Some("DP-2"), role),
                Ok(MonitorMove::ById {
                    pid: 4242,
                    connector: "DP-2".into()
                })
            );
            assert_eq!(
                monitor_move(Wm::Gnome, 4242, Some("DP-2"), role),
                Ok(MonitorMove::Gnome {
                    connector: "DP-2".into()
                })
            );
            for wm in [Wm::Niri, Wm::Gnome, Wm::None] {
                assert_eq!(monitor_move(wm, 4242, None, role), Ok(MonitorMove::Unchanged));
            }
            assert!(monitor_move(Wm::None, 4242, Some("DP-2"), role).is_err());
            for wm in [Wm::Sway, Wm::Hyprland, Wm::Niri, Wm::Gnome] {
                assert!(monitor_move(wm, 4242, Some("DP-2; focus"), role).is_err());
            }
        }
    }

    #[test]
    fn a_new_module_window_is_focused_by_its_pid_once_mapped_and_moved_nowhere() {
        let role = PlacementRole::NewModule;
        assert_eq!(
            monitor_move(Wm::Sway, 4242, None, role),
            Ok(MonitorMove::Command(vec!["swaymsg".into(), "[pid=4242] focus".into()]))
        );
        assert_eq!(
            monitor_move(Wm::Hyprland, 4242, None, role),
            Ok(MonitorMove::Hyprland {
                pid: 4242,
                connector: None,
                role
            })
        );
        for wm in [Wm::Niri, Wm::Gnome, Wm::None] {
            assert_eq!(monitor_move(wm, 4242, None, role), Ok(MonitorMove::Unchanged), "{wm:?}");
        }
    }

    #[test]
    fn the_placement_command_is_chosen_per_compositor() {
        assert_eq!(
            monitor_move(Wm::Sway, 4242, Some("DP-2"), PlacementRole::Main),
            Ok(MonitorMove::Command(vec![
                "swaymsg".into(),
                "[pid=4242] move container to output \"DP-2\"".into()
            ]))
        );
        assert_eq!(
            monitor_move(Wm::Hyprland, 4242, Some("DP-2"), PlacementRole::Main),
            Ok(MonitorMove::Hyprland {
                pid: 4242,
                connector: Some("DP-2".into()),
                role: PlacementRole::Main
            }),
            "Hyprland's command is written on the worker, in the form its config manager takes"
        );
        assert_eq!(
            monitor_move(Wm::Niri, 4242, Some("HDMI-A-1"), PlacementRole::Main),
            Ok(MonitorMove::ById {
                pid: 4242,
                connector: "HDMI-A-1".into()
            }),
            "niri's command needs the window's id, read on the worker"
        );
        assert_eq!(
            monitor_move(Wm::Gnome, 4242, Some("DP-2"), PlacementRole::Main),
            Ok(MonitorMove::Gnome {
                connector: "DP-2".into()
            }),
            "GNOME's extension is asked, by the caller's own pid"
        );
        assert_eq!(
            monitor_move(Wm::None, 4242, Some("DP-2"), PlacementRole::Main),
            Err(NO_OUTPUT_MOVE.to_string())
        );
        for bad in ["", "DP-1\" ; exit", "two words", &"x".repeat(65)] {
            for wm in [Wm::Sway, Wm::Hyprland, Wm::Niri, Wm::Gnome] {
                assert!(
                    monitor_move(wm, 4242, Some(bad), PlacementRole::Main).is_err(),
                    "{wm:?} refuses {bad:?}, which names no output"
                );
            }
        }
    }
}

#[cfg(test)]
mod editor_reads_tests {
    use super::*;
    use eitri_core::wm::{close::NvimAnswer, EditorFacts};
    use std::time::Instant;

    struct EditorSnapshot(RefCell<EditorFacts>);

    impl EditorReads for EditorSnapshot {
        fn cell(&self) -> Option<(f64, f64)> {
            None
        }
        fn running(&self) -> bool {
            self.0.borrow().running
        }
        fn facts(&self) -> EditorFacts {
            self.0.borrow().clone()
        }
    }

    #[test]
    fn editor_facts_copy_the_registered_panes_snapshot() {
        let host = GtkHost::new();
        let win = WinId::MAIN;
        assert_eq!(host.editor_facts(win), EditorFacts::default());
        let editor = Rc::new(EditorSnapshot(RefCell::new(EditorFacts {
            running: false,
            exited: false,
            exit_pending: true,
            starting: false,
            watch: Some(NvimAnswer {
                sent_at: Instant::now(),
                done: false,
                returned: false,
                last: None,
            }),
        })));
        host.set_editor(win, editor.clone());
        assert_eq!(host.editor_facts(win), *editor.0.borrow());
        assert_eq!(host.editor_facts(WinId(2)), EditorFacts::default());
        *editor.0.borrow_mut() = EditorFacts {
            exited: true,
            ..Default::default()
        };
        assert_eq!(host.editor_facts(win), *editor.0.borrow());
    }

    /// The editor's calls reach the window's editor service, the release of an exited nvim among them, which is told
    /// as its own fact. A quit the service could not send is refused as any failed op is, and told again as the
    /// quit's own fact with its generation, since the refusal names no call.
    #[test]
    fn a_quit_that_could_not_be_sent_is_told_with_its_generation() {
        use eitri_core::wm::conformance::RecordingSink;
        struct Services(RefCell<Vec<String>>);
        impl ServiceSet for Services {
            fn editor(&self, op: &EditorCall) -> Result<(), String> {
                self.0.borrow_mut().push(format!("{op:?}"));
                match op {
                    EditorCall::Quit { .. } => Err("nvim is not running; the quit was not sent".into()),
                    _ => Ok(()),
                }
            }
            fn terminal(&self, _: &TermOp) -> Result<(), String> {
                unreachable!()
            }
            fn plugin(&self, _: &PluginOp) -> Result<(), String> {
                unreachable!()
            }
            fn chat(&self, _: &ChatOp) -> Result<(), String> {
                unreachable!()
            }
        }
        let host = GtkHost::new();
        let sink = Rc::new(RecordingSink::default());
        let erased: Rc<dyn HostSink> = sink.clone();
        host.attach(Rc::downgrade(&erased));
        let services = Rc::new(Services(RefCell::default()));
        host.set_services(services.clone());

        let release = HostOp::Editor(EditorCall::ReleaseExited { win: WinId::MAIN });
        assert_eq!(
            host.apply(&release),
            Applied::Done,
            "the release reaches the editor service"
        );
        let events: Vec<HostEvent> = sink.events().into_iter().map(|(_, event)| event).collect();
        assert_eq!(events, [HostEvent::Editor(EditorEvent::Released { win: WinId::MAIN })]);

        let quit = HostOp::Editor(EditorCall::Quit {
            win: WinId::MAIN,
            lua: "quit".into(),
            generation: 7,
        });
        assert_eq!(host.apply(&quit), Applied::Refused);
        let events: Vec<HostEvent> = sink.events().into_iter().skip(1).map(|(_, event)| event).collect();
        assert!(matches!(
            events.as_slice(),
            [
                HostEvent::Refused {
                    what: OpTag::Editor,
                    ..
                },
                HostEvent::Editor(EditorEvent::QuitNotSent {
                    win: WinId::MAIN,
                    generation: 7
                })
            ]
        ));
        assert_eq!(services.0.borrow().len(), 2, "both calls reached the service");
    }

    /// An open's answer goes to the service that holds the socket; with none, it is refused as any op is.
    #[test]
    fn an_open_answer_reaches_the_service() {
        use eitri_core::wm::conformance::RecordingSink;
        #[derive(Default)]
        struct Services(RefCell<Vec<(u64, bool)>>);
        impl ServiceSet for Services {
            fn editor(&self, _: &EditorCall) -> Result<(), String> {
                unreachable!()
            }
            fn terminal(&self, _: &TermOp) -> Result<(), String> {
                unreachable!()
            }
            fn plugin(&self, _: &PluginOp) -> Result<(), String> {
                unreachable!()
            }
            fn chat(&self, _: &ChatOp) -> Result<(), String> {
                unreachable!()
            }
            fn open_answer(&self, id: u64, accepted: bool) -> Result<(), String> {
                self.0.borrow_mut().push((id, accepted));
                Ok(())
            }
        }
        let host = GtkHost::new();
        let sink = Rc::new(RecordingSink::default());
        let erased: Rc<dyn HostSink> = sink.clone();
        host.attach(Rc::downgrade(&erased));
        let answer = HostOp::OpenAnswer { id: 7, accepted: true };
        assert_eq!(host.apply(&answer), Applied::Refused, "no services yet");
        let services = Rc::new(Services::default());
        host.set_services(services.clone());
        assert_eq!(host.apply(&answer), Applied::Done);
        assert_eq!(*services.0.borrow(), [(7, true)]);
    }

    /// A release nvim's living process refused, and a restart the pane refused from the state it was in, are each
    /// refused as any failed op is and told as their own facts, the restart's with the state, since a refusal names
    /// no call. A restart carried out is told nothing until its nvim is ready.
    #[test]
    fn a_refused_release_and_a_refused_restart_are_told_as_their_own_facts() {
        use eitri_core::wm::conformance::RecordingSink;
        use eitri_core::wm::EditorSpawn;
        struct Pane {
            exited: Cell<bool>,
            refused_from: RefCell<Option<String>>,
            restarts: RefCell<Vec<EditorSpawn>>,
        }
        impl ServiceSet for Pane {
            fn editor(&self, op: &EditorCall) -> Result<(), String> {
                match op {
                    EditorCall::ReleaseExited { .. } if self.exited.get() => Ok(()),
                    EditorCall::ReleaseExited { .. } => Err("nvim has not exited; the pane was not released".into()),
                    _ => unreachable!("{op:?}"),
                }
            }
            fn restart_editor(&self, spawn: &EditorSpawn) -> Result<(), RestartRefused> {
                self.restarts.borrow_mut().push(spawn.clone());
                let refused_from = self.refused_from.borrow().clone();
                match refused_from {
                    Some(state) => Err(RestartRefused::Pane { state }),
                    None => Ok(()),
                }
            }
            fn terminal(&self, _: &TermOp) -> Result<(), String> {
                unreachable!()
            }
            fn plugin(&self, _: &PluginOp) -> Result<(), String> {
                unreachable!()
            }
            fn chat(&self, _: &ChatOp) -> Result<(), String> {
                unreachable!()
            }
        }
        let host = GtkHost::new();
        let sink = Rc::new(RecordingSink::default());
        let erased: Rc<dyn HostSink> = sink.clone();
        host.attach(Rc::downgrade(&erased));
        let pane = Rc::new(Pane {
            exited: Cell::new(false),
            refused_from: RefCell::new(Some("Ready".into())),
            restarts: RefCell::default(),
        });
        host.set_services(pane.clone());
        let events = || -> Vec<HostEvent> { sink.events().into_iter().map(|(_, event)| event).collect() };

        let release = HostOp::Editor(EditorCall::ReleaseExited { win: WinId::MAIN });
        assert_eq!(host.apply(&release), Applied::Refused);
        assert!(matches!(
            events().as_slice(),
            [
                HostEvent::Refused {
                    what: OpTag::Editor,
                    ..
                },
                HostEvent::Editor(EditorEvent::ReleaseRefused { win: WinId::MAIN })
            ]
        ));

        let spawn = EditorSpawn {
            env: vec![("TMUX".into(), "x".into())],
            args: vec!["--cmd".into(), "let g:x = 1".into()],
            cwd: None,
            clean: false,
            background: None,
            open: vec![],
        };
        let restart = HostOp::Editor(EditorCall::Restart {
            win: WinId::MAIN,
            spawn: spawn.clone(),
        });
        assert_eq!(host.apply(&restart), Applied::Refused);
        let told = events();
        let [_, _, HostEvent::Refused {
            what: OpTag::Editor,
            why,
            ..
        }, HostEvent::Editor(EditorEvent::RestartRefused {
            win: WinId::MAIN,
            state,
        })] = told.as_slice()
        else {
            panic!("the restart's refusal and its fact: {told:?}");
        };
        assert!(why.contains("Ready"), "the refusal says the state: {why}");
        assert_eq!(state, "Ready");

        *pane.refused_from.borrow_mut() = None;
        assert_eq!(host.apply(&restart), Applied::Done);
        assert_eq!(events().len(), 4, "a restart carried out is told nothing yet");
        assert_eq!(
            *pane.restarts.borrow(),
            [spawn.clone(), spawn],
            "each restart reached the pane with its spawn"
        );
    }
}

// Only the probe places hosts by hand, so only a build with it has a host to leave alone.
#[cfg(all(test, feature = "probe"))]
mod tests {
    use super::*;

    /// The probe's hosts stay where it put them: a map or an unmap of one reaches nothing, every other host's
    /// op is carried out as it came, and no other op is a map.
    #[test]
    fn map_and_unmap_skip_a_host_the_probe_places() {
        let placed = ModuleId::terminal();
        let laid_out = ModuleId::agent();
        let win = WinId::MAIN;
        let ops = vec![
            HostOp::Map {
                win,
                module: placed.clone(),
            },
            HostOp::Map {
                win,
                module: laid_out.clone(),
            },
            HostOp::Allocate { win },
            HostOp::Unmap {
                win,
                module: placed.clone(),
            },
            HostOp::Unmap {
                win,
                module: laid_out.clone(),
            },
        ];
        assert_eq!(
            hosts_to_map(&ops, |module| *module == placed),
            vec![(win, laid_out.clone(), true), (win, laid_out.clone(), false)]
        );
        assert_eq!(
            hosts_to_map(&ops, |_| false),
            vec![
                (win, placed.clone(), true),
                (win, laid_out.clone(), true),
                (win, placed, false),
                (win, laid_out, false),
            ],
            "without the probe every map and unmap is carried out"
        );
    }
}

#[cfg(test)]
mod terminal_at_tests {
    use super::*;
    use eitri_core::wm::conformance::RecordingSink;
    use eitri_core::wm::ShellStart;

    #[derive(Clone)]
    struct Pane {
        shell: ShellStart,
        calls: Rc<RefCell<Vec<String>>>,
    }

    impl RespawnPane for Pane {
        fn shell_start(&self) -> ShellStart {
            self.shell
        }
        fn close(&self) {
            self.calls.borrow_mut().push("close".into());
        }
        fn set_start_dir(&self, dir: PathBuf) {
            self.calls.borrow_mut().push(format!("dir {}", dir.display()));
        }
        fn start(&self) {
            self.calls.borrow_mut().push("start".into());
        }
    }

    struct Services(Option<Pane>);
    impl ServiceSet for Services {
        fn editor(&self, _: &EditorCall) -> Result<(), String> {
            unreachable!()
        }
        fn plugin(&self, _: &PluginOp) -> Result<(), String> {
            unreachable!()
        }
        fn chat(&self, _: &ChatOp) -> Result<(), String> {
            unreachable!()
        }
        fn terminal(&self, op: &TermOp) -> Result<(), String> {
            match op {
                TermOp::Respawn { expect, dir, .. } => respawn_pane(self.0.clone(), *expect, dir.clone()),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn a_respawn_for_a_pane_no_longer_running_expect_or_typed_into_since_is_refused_and_touches_nothing() {
        let expect = ShellStart::of(3, Some(42));
        let calls = Rc::new(RefCell::new(Vec::new()));
        let host = GtkHost::new();
        let sink = Rc::new(RecordingSink::default());
        let erased: Rc<dyn HostSink> = sink.clone();
        host.attach(Rc::downgrade(&erased));
        let op = HostOp::Terminal(TermOp::Respawn {
            win: WinId::MAIN,
            module: ModuleId::terminal(),
            dir: PathBuf::from("/new"),
            expect,
        });
        for shell in [
            None,
            Some(ShellStart::of(4, Some(42))),
            Some(ShellStart::of(3, Some(43))),
            Some(ShellStart::of(3, None)),
            Some(ShellStart::of(3, Some(42)).typed(1)),
        ] {
            host.set_services(Rc::new(Services(shell.map(|shell| Pane {
                shell,
                calls: calls.clone(),
            }))));
            assert_eq!(host.apply(&op), Applied::Refused);
            assert!(calls.borrow().is_empty());
            assert!(
                matches!(sink.events().last(), Some((_, HostEvent::Refused { what: OpTag::Respawn, surface: Some((WinId::MAIN, module)), .. })) if module == &ModuleId::terminal())
            );
        }
        host.set_services(Rc::new(Services(Some(Pane {
            shell: expect,
            calls: calls.clone(),
        }))));
        assert_eq!(host.apply(&op), Applied::Done);
        assert_eq!(*calls.borrow(), ["close", "dir /new", "start"]);
    }
}

#[cfg(test)]
mod module_window_ops_tests {
    use super::*;
    use eitri_core::window_link::HubAsk;
    use eitri_core::wm::conformance::RecordingSink;

    #[derive(Default)]
    struct Linked {
        opened: RefCell<Vec<(WindowName, Option<String>, bool)>>,
        sent: RefCell<Vec<(PeerId, Msg)>>,
        to_hub: RefCell<Vec<Msg>>,
        handed: RefCell<Vec<(PeerId, String, WindowName, Option<String>)>>,
        open_windows: RefCell<Vec<WindowName>>,
    }

    impl LinkedWindows for Linked {
        fn open(&self, _: WinId, _: &WindowName, _: Option<String>) -> Result<(), String> {
            unreachable!()
        }
        fn restore(&self, _: WinId, _: &WindowName) -> Result<(), String> {
            unreachable!()
        }
        fn start_editor(&self, _: WinId) -> Result<(), String> {
            unreachable!()
        }
        fn module_window_open(&self, name: &WindowName) -> bool {
            self.open_windows.borrow().contains(name)
        }
        fn open_module(&self, _: WinId, name: &WindowName, token: Option<String>, keep: bool) -> Result<(), String> {
            self.opened.borrow_mut().push((name.clone(), token, keep));
            Ok(())
        }
        fn send(&self, _: WinId, peer: PeerId, msg: Msg) -> Result<(), String> {
            self.sent.borrow_mut().push((peer, msg));
            Ok(())
        }
        fn send_to_hub(&self, _: WinId, msg: Msg) -> Result<(), String> {
            self.to_hub.borrow_mut().push(msg);
            Ok(())
        }
        fn hand_over(
            &self,
            _: WinId,
            from: PeerId,
            _: &eitri_core::agent_backend::AgentId,
            session: &str,
            to: &WindowName,
            token: Option<String>,
        ) -> Result<(), String> {
            let handed = (from, session.to_string(), to.clone(), token);
            self.handed.borrow_mut().push(handed);
            Ok(())
        }
    }

    struct Compositor(Option<&'static str>);
    impl PairRaise for Compositor {
        fn raise(&self, _: &gtk4::Window, _: Option<&str>) {
            unreachable!()
        }
        fn move_focus(&self, _: Direction, on_none: &dyn Fn(&str)) -> bool {
            match self.0 {
                Some(notice) => {
                    on_none(notice);
                    false
                }
                None => true,
            }
        }
    }

    fn host() -> (Rc<GtkHost>, Rc<RecordingSink>, Rc<Linked>) {
        let host = GtkHost::new();
        let sink = Rc::new(RecordingSink::default());
        let erased: Rc<dyn HostSink> = sink.clone();
        host.attach(Rc::downgrade(&erased));
        let linked = Rc::new(Linked::default());
        host.set_linked_windows(linked.clone());
        (host, sink, linked)
    }

    fn chat(n: u8) -> WindowName {
        WindowName::Chat(n)
    }

    #[test]
    fn opening_a_module_window_passes_the_key_s_token_and_a_start_s_window_takes_none() {
        let (host, _sink, linked) = host();
        for (token, keep_keys) in [(Some("key"), false), (Some("dropped"), true), (None, false)] {
            let op = HostOp::OpenModuleWindow {
                win: WinId::MAIN,
                name: chat(2),
                token: token.map(str::to_string),
                keep_keys,
            };
            assert_eq!(host.apply(&op), Applied::Done);
        }
        assert_eq!(
            *linked.opened.borrow(),
            vec![
                (chat(2), Some("key".to_string()), false),
                (chat(2), None, true),
                // No window of this host to make a token from: it goes without one.
                (chat(2), None, false),
            ]
        );
    }

    #[derive(Default)]
    struct Pages(Cell<u32>);

    impl BasePageMaker for Pages {
        fn build(&self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn the_base_page_is_built_by_the_maker_and_refused_without_one() {
        let (host, sink, _) = host();
        let op = HostOp::BuildBasePage { win: WinId::MAIN };
        assert_eq!(host.apply(&op), Applied::Refused);
        assert!(sink.events().iter().any(|(_, event)| matches!(
            event,
            HostEvent::Refused {
                what: OpTag::BuildBasePage,
                ..
            }
        )));
        let pages = Rc::new(Pages::default());
        host.set_base_page_maker(pages.clone());
        assert_eq!(host.apply(&op), Applied::Done);
        assert_eq!(pages.0.get(), 1);
    }

    #[test]
    fn a_host_with_no_pair_refuses_the_ops_that_reach_another_window() {
        let host = GtkHost::new();
        let sink = Rc::new(RecordingSink::default());
        let erased: Rc<dyn HostSink> = sink.clone();
        host.attach(Rc::downgrade(&erased));
        let ops = [
            HostOp::OpenModuleWindow {
                win: WinId::MAIN,
                name: chat(1),
                token: None,
                keep_keys: false,
            },
            HostOp::PeerRun {
                win: WinId::MAIN,
                peer: PeerId(16),
                name: "zoom".into(),
                opts: vec![],
            },
            HostOp::HubAsk {
                win: WinId::MAIN,
                ask: HubAsk::InstanceNew,
            },
            HostOp::CompositorFocus {
                win: WinId::MAIN,
                direction: Direction::Left,
            },
            HostOp::HandOverTab {
                win: WinId::MAIN,
                from: PeerId(16),
                agent: eitri_core::agent_backend::AgentId::Claude,
                session: "s".into(),
                to: chat(2),
                token: None,
            },
            HostOp::KeepTab {
                win: WinId::MAIN,
                peer: PeerId(16),
                session: "s".into(),
                why: "no".into(),
            },
        ];
        for op in &ops {
            assert_eq!(host.apply(op), Applied::Refused, "{op:?}");
        }
        let refused: Vec<OpTag> = sink
            .events()
            .into_iter()
            .filter_map(|(_, event)| match event {
                HostEvent::Refused { what, .. } => Some(what),
                _ => None,
            })
            .collect();
        assert_eq!(
            refused,
            vec![
                OpTag::OpenModuleWindow,
                OpTag::PeerRun,
                OpTag::HubAsk,
                OpTag::CompositorFocus,
                OpTag::HandOverTab,
                OpTag::KeepTab
            ]
        );
    }

    #[test]
    fn a_hand_over_goes_to_the_pair_and_a_kept_tab_over_the_askers_link() {
        let (host, _sink, linked) = host();
        let hand_over = HostOp::HandOverTab {
            win: WinId::MAIN,
            from: PeerId(16),
            agent: eitri_core::agent_backend::AgentId::Claude,
            session: "s".into(),
            to: chat(3),
            token: Some("theirs".into()),
        };
        assert_eq!(host.apply(&hand_over), Applied::Done);
        assert_eq!(
            *linked.handed.borrow(),
            vec![(PeerId(16), "s".to_string(), chat(3), Some("theirs".to_string()))]
        );
        let keep = HostOp::KeepTab {
            win: WinId::MAIN,
            peer: PeerId(16),
            session: "s".into(),
            why: "all nine chat windows are open".into(),
        };
        assert_eq!(host.apply(&keep), Applied::Done);
        assert_eq!(
            *linked.sent.borrow(),
            vec![(
                PeerId(16),
                Msg::TabKept {
                    session: "s".into(),
                    why: "all nine chat windows are open".into()
                }
            )]
        );
    }

    #[test]
    fn a_forwarded_raise_is_answered_with_the_token_it_carries() {
        let (host, sink, _linked) = host();
        let op = HostOp::ForwardRaise {
            win: WinId::MAIN,
            peer: PeerId(17),
            token: Some("theirs".into()),
        };
        assert_eq!(host.apply(&op), Applied::Done);
        assert_eq!(
            sink.events().into_iter().map(|(_, event)| event).collect::<Vec<_>>(),
            vec![HostEvent::Peer(PeerEvent::Raised {
                peer: PeerId(17),
                token: Some("theirs".into())
            })]
        );
    }

    #[test]
    fn a_run_and_an_ask_go_over_the_link_as_the_messages_the_other_window_reads() {
        let (host, _sink, linked) = host();
        let run = HostOp::PeerRun {
            win: WinId::MAIN,
            peer: PeerId(17),
            name: "tab.select".into(),
            opts: vec![("n".into(), 3.into())],
        };
        assert_eq!(host.apply(&run), Applied::Done);
        let ask = HostOp::HubAsk {
            win: WinId::MAIN,
            ask: HubAsk::InstanceNew,
        };
        assert_eq!(host.apply(&ask), Applied::Done);
        assert_eq!(
            *linked.sent.borrow(),
            vec![(
                PeerId(17),
                Msg::RunAction {
                    name: "tab.select".into(),
                    opts: vec![("n".into(), 3.into())]
                }
            )]
        );
        assert_eq!(
            *linked.to_hub.borrow(),
            vec![Msg::HubAsk {
                ask: HubAsk::InstanceNew,
                token: None
            }]
        );
    }

    #[test]
    fn the_off_key_hint_names_the_window_manager_this_window_moves_through() {
        struct Placer(eitri_core::compositor::Wm);
        impl MonitorPlacer for Placer {
            fn wm(&self) -> eitri_core::compositor::Wm {
                self.0
            }
            fn place(&self, _: MonitorMove) {
                unreachable!()
            }
        }
        let (host, _sink, _linked) = host();
        assert_eq!(host.compositor_name(), "the compositor");
        use eitri_core::compositor::Wm;
        for (wm, name) in [(Wm::Hyprland, "Hyprland"), (Wm::Sway, "sway"), (Wm::Niri, "niri")] {
            host.set_monitor_placer(Rc::new(Placer(wm)));
            assert_eq!(host.compositor_name(), name);
        }
    }

    #[test]
    fn a_window_a_hand_over_starts_reads_as_open_to_the_controller() {
        let (host, _sink, linked) = host();
        assert!(!host.module_window_open(&chat(2)));
        linked.open_windows.borrow_mut().push(chat(2));
        assert!(host.module_window_open(&chat(2)));
        assert!(!host.module_window_open(&chat(3)));
    }

    #[test]
    fn a_focus_move_past_the_edge_is_the_compositors_and_says_once_when_it_has_none() {
        let (host, sink, _linked) = host();
        let op = HostOp::CompositorFocus {
            win: WinId::MAIN,
            direction: Direction::Right,
        };
        host.set_pair_raise(Rc::new(Compositor(None)));
        assert_eq!(host.apply(&op), Applied::Done);
        host.set_pair_raise(Rc::new(Compositor(Some("no compositor tool"))));
        assert_eq!(host.apply(&op), Applied::Refused);
        assert!(sink.events().into_iter().any(|(_, event)| matches!(
            event,
            HostEvent::Refused { what: OpTag::CompositorFocus, why, .. } if why == "no compositor tool"
        )));
    }
}
