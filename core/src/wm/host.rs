//! The value contract between window decisions and native host services.

use super::window::*;
use crate::keymap::{
    action::{Action, SplitCwd},
    key::{KeyName, KeySpec},
    prefix::{Press, WorkspacePick},
};
use crate::layout::{kill::Reopen, Arrangement, Axis, Con, Direction, Frame, ModuleId, Size};
use crate::tabs::ViewId;
use std::any::Any;
use std::fmt;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Native operations called on the UI thread by the driver.
pub trait Host {
    /// Performs an operation without blocking; refusals also post a refusal event.
    fn apply(&self, op: &HostOp) -> Applied;
    /// Runs a remaining shell call with no batch applying or controller borrow held.
    fn run_shell_verb(&self, win: WinId, call: &ShellCall, keycode: u32);
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
    fn key(&mut self, win: WinId, stage: &KeyStage, press: &KeyPress) -> (Verdict, Batch);
    /// Decides whether a native close may proceed.
    fn close_requested(&mut self, win: WinId, by: CloseBy) -> (CloseVerdict, Batch);
    /// Runs an action through the shared action runner.
    fn run(&mut self, win: WinId, action: Action, origin: Origin) -> Batch;
    /// Plans a focus transfer to a module.
    fn focus(&mut self, win: WinId, module: &ModuleId) -> Batch;
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

/// Where a captured key is being considered.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyStage {
    /// The window capture stage.
    Window,
    /// A module capture stage.
    Module(ModuleId),
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
}

/// A call still carried out by the shell after the batch.
#[derive(Debug, Clone, PartialEq)]
pub struct ShellVerb {
    /// The target window.
    pub win: WinId,
    /// What the shell's arm is asked to do.
    pub call: ShellCall,
    /// The originating hardware code, or zero.
    pub keycode: u32,
}

/// What a shell verb asks the shell's arm to do. The prefix's split and chooser commands are not actions, and
/// they still need the shell's arms once the prefix is decided here; each of those two goes when its family's
/// verbs move into the controller.
#[derive(Debug, Clone, PartialEq)]
pub enum ShellCall {
    /// An action whose family the shell still carries out.
    Action(Action),
    /// A split key's placement: a new module of the key's kind beside `module` along `axis`.
    Place {
        module: ModuleId,
        axis: Axis,
        cwd: SplitCwd,
    },
    /// A digit or command answered to the workspace chooser.
    WorkspacePick(WorkspacePick),
}

/// Reads the controller makes of the shell directly, because each answers at once and none can call back into
/// the controller: the grid's size and measures, the editor's cell and whether it runs, and whether a chat page
/// claims a navigation key.
pub trait HostReads {
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
    /// Creates a window without showing it.
    OpenWindow { win: WinId, spec: WindowSpec },
    /// Presents a window using optional native authorization.
    Present {
        win: WinId,
        activation: Option<ActivationToken>,
    },
    /// Sets window presentation flags.
    SetMode { win: WinId, mode: WindowMode },
    /// Moves the window to an output.
    MoveToMonitor { win: WinId, monitor: MonitorId },
    /// Closes without asking again.
    CloseWindow { win: WinId },
    /// Ends the application.
    Quit,
    /// Creates a module surface.
    CreateSurface {
        win: WinId,
        module: ModuleId,
        kind: SurfaceKind,
    },
    /// Releases a module surface.
    DropSurface { win: WinId, module: ModuleId, why: DropWhy },
    /// Lays out a destination off screen.
    PrepareSurface { win: WinId, module: ModuleId },
    /// Flashes the window's name and shows `text` as a refusal's toast; an empty `text` flashes alone. It
    /// stays until the chrome model carries the flash and the toast itself.
    Refuse { win: WinId, text: String },
    /// Runs the shell's remaining change hooks (selection, tray, mark and workspace text) after a change the
    /// controller made; it goes with those hooks, once the chrome model draws what they draw.
    ChromeHooks { win: WinId },
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
    /// Runs Lua after all other operations.
    Lua(LuaOp),
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
}

/// Why a module surface is released.
#[derive(Debug, Clone, PartialEq)]
pub enum DropWhy {
    /// The module was killed.
    Killed,
    /// The module is no longer needed.
    Retired,
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
    /// A module surface.
    Module(ModuleId),
    /// An indexed top-bar item.
    TopBarItem(u32),
    /// The rename entry.
    RenameEntry,
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

/// Commands sent to the editor service.
#[derive(Debug, Clone, PartialEq)]
pub enum EditorCall {
    /// Sets editor focus.
    SetFocused { win: WinId, focused: bool },
    /// Asks the editor to quit through Lua.
    Quit { win: WinId, lua: String },
    /// Ends the editor session.
    End { win: WinId },
    /// Shuts down the editor service.
    Shutdown { win: WinId },
    /// Sets editor scale.
    SetScale { win: WinId, scale: f64 },
    /// Reveals the editor.
    Reveal { win: WinId },
    /// Restarts with complete launch settings.
    Restart { win: WinId, spawn: EditorSpawn },
    /// Types keys into the editor, in the editor's own key notation.
    SendKeys { win: WinId, keys: String },
}

/// Commands sent to the chat service.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatOp {
    /// Sets pane focus.
    PaneFocused { win: WinId, view: ViewId, focused: bool },
    /// Notifies a shown page.
    PageShown { win: WinId, view: ViewId },
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
    /// Breaks a tab out of its view.
    BreakOut { win: WinId, view: ViewId },
    /// Joins a tab back.
    JoinBack { win: WinId, view: ViewId },
    /// Opens the keymap sheet.
    OpenKeymap { win: WinId, view: ViewId },
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
}

/// Commands sent to terminal services.
#[derive(Debug, Clone, PartialEq)]
pub enum TermOp {
    /// Starts a terminal.
    Start { win: WinId, module: ModuleId },
    /// Sets terminal focus.
    SetFocused {
        win: WinId,
        module: ModuleId,
        focused: bool,
    },
    /// Hangs up the terminal.
    HangUp { win: WinId, module: ModuleId },
    /// Enters copy mode, scrolled up a page when `up`.
    CopyMode { win: WinId, module: ModuleId, up: bool },
    /// Sends terminal keys.
    SendKeys { win: WinId, module: ModuleId, keys: String },
    /// Delivers one key, as the key itself; the terminal's engine encodes it, or says it has no encoding.
    LiteralKey { win: WinId, module: ModuleId, key: KeySpec },
    /// Detaches for a handoff.
    Detach { win: WinId, module: ModuleId },
    /// Shuts down every terminal.
    ShutdownAll { win: WinId },
}

/// Commands sent to plugin services.
#[derive(Debug, Clone, PartialEq)]
pub enum PluginOp {
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
}

/// Process-wide resources to remove.
#[derive(Debug, Clone, PartialEq)]
pub enum CleanupOp {
    /// The editor feeds.
    Feeds,
    /// The pane-switch shim directory.
    PaneSwitchDir,
    /// The process scratch directory.
    ScratchDir,
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

/// Facts posted by the native host and services.
#[derive(Debug, Clone, PartialEq)]
pub enum HostEvent {
    /// A watched hardware key was released.
    KeyReleased { win: WinId, keycode: u32 },
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
    /// Allocation settled its layout pins. `root` is the current tree as the settle left it, copied inside the
    /// allocation: the event is heard later, from the driver's idle, and a change that comes in between (a
    /// resize of that very pin) must not pass for part of the settle.
    Settled { win: WinId, root: Con },
    /// A remaining shell action changed the layout.
    LayoutChangedByShell { win: WinId },
    /// Reports a surface built by a shell arm until the runner owns creation.
    Declared {
        win: WinId,
        module: ModuleId,
        kind: SurfaceKind,
    },
    /// A process termination signal arrived.
    EndSignal { name: &'static str },
    /// A module surface ended.
    SurfaceEnded { win: WinId, module: ModuleId, why: Ended },
    /// An editor service event.
    Editor(EditorEvent),
    /// A chat service event.
    Chat(ChatEvent),
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
    /// The output list changed.
    MonitorsChanged,
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
    /// A float title bar.
    FloatBar(ModuleId),
    /// A float resize grip.
    FloatGrip(ModuleId),
    /// A layout divider.
    Divider(u32),
    /// A top-bar item.
    TopBarItem(u32),
    /// A module chip.
    Chip(ModuleId),
    /// The window itself.
    Window,
}

/// Pointer coordinates and button transitions.
#[derive(Debug, Clone, PartialEq)]
pub enum PointerEv {
    /// A button press.
    Press { x: f64, y: f64, button: u32 },
    /// Pointer movement.
    Motion { x: f64, y: f64 },
    /// A button release.
    Release { x: f64, y: f64 },
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
    /// The child exited.
    Exited { win: WinId },
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
    /// A view returned keys.
    KeysReturn { win: WinId, view: ViewId },
    /// A view reports coverage.
    Covered { win: WinId, view: ViewId },
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
    /// A chat supplied hint text.
    Hint { win: WinId, message: String },
    /// A chat supplied a toast.
    Toast { win: WinId, text: String },
    /// A keymap sheet closed; `opening` names the opening it closes, so a close of an earlier one is dropped.
    KeymapClosed {
        win: WinId,
        view: ViewId,
        opening: u64,
        forced: bool,
    },
    /// A view requested editor focus.
    EditorRequest { win: WinId, view: ViewId },
}

/// An opaque terminal transfer whose identity follows its shared allocation.
#[derive(Clone)]
pub struct Handoff(Rc<dyn Any>);

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

/// Facts reported by terminal services.
#[derive(Debug, Clone, PartialEq)]
pub enum TermEvent {
    /// A terminal child started.
    Started { win: WinId, module: ModuleId, pid: u32 },
    /// A terminal child exited.
    Exited { win: WinId, module: ModuleId },
    /// A terminal rang its bell.
    Bell { win: WinId, module: ModuleId },
    /// A terminal detached for transfer.
    Detached {
        win: WinId,
        module: ModuleId,
        handoff: Handoff,
    },
}

/// Facts reported by the Lua kernel.
#[derive(Debug, Clone, PartialEq)]
pub enum LuaEvent {
    /// Layout requests are queued for the controller to drain.
    LayoutRequests,
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

/// Notifications from linked processes.
#[derive(Debug, Clone, PartialEq)]
pub enum PeerEvent {
    /// A peer is arriving.
    Arriving { peer: PeerId },
    /// A peer completed a move.
    MoveDone { peer: PeerId },
    /// A peer could not move.
    MoveFailed { peer: PeerId },
    /// A peer sent its summary.
    Summary { peer: PeerId },
    /// A peer intends to focus.
    FocusIntent { peer: PeerId },
    /// A peer disconnected.
    Bye { peer: PeerId },
}

/// The operation kind carried by a refusal.
#[derive(Debug, Clone, PartialEq)]
pub enum OpTag {
    /// Identifies OpenWindow operations.
    OpenWindow,
    /// Identifies Present operations.
    Present,
    /// Identifies SetMode operations.
    SetMode,
    /// Identifies MoveToMonitor operations.
    MoveToMonitor,
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
    /// Identifies Refuse operations.
    Refuse,
    /// Identifies ChromeHooks operations.
    ChromeHooks,
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
    /// Identifies Plugin operations.
    Plugin,
    /// Identifies Cleanup operations.
    Cleanup,
    /// Identifies Lua operations.
    Lua,
}

impl OpTag {
    /// Identifies an operation without retaining its payload.
    pub fn of(op: &HostOp) -> Self {
        match op {
            HostOp::OpenWindow { .. } => Self::OpenWindow,
            HostOp::Present { .. } => Self::Present,
            HostOp::SetMode { .. } => Self::SetMode,
            HostOp::MoveToMonitor { .. } => Self::MoveToMonitor,
            HostOp::CloseWindow { .. } => Self::CloseWindow,
            HostOp::Quit => Self::Quit,
            HostOp::CreateSurface { .. } => Self::CreateSurface,
            HostOp::DropSurface { .. } => Self::DropSurface,
            HostOp::PrepareSurface { .. } => Self::PrepareSurface,
            HostOp::Refuse { .. } => Self::Refuse,
            HostOp::ChromeHooks { .. } => Self::ChromeHooks,
            HostOp::Map { .. } => Self::Map,
            HostOp::Unmap { .. } => Self::Unmap,
            HostOp::Allocate { .. } => Self::Allocate,
            HostOp::Restack { .. } => Self::Restack,
            HostOp::Focus { .. } => Self::Focus,
            HostOp::Chrome { .. } => Self::Chrome,
            HostOp::Hint(..) => Self::Hint,
            HostOp::Editor(..) => Self::Editor,
            HostOp::Chat(..) => Self::Chat,
            HostOp::Terminal(..) => Self::Terminal,
            HostOp::Plugin(..) => Self::Plugin,
            HostOp::Cleanup(..) => Self::Cleanup,
            HostOp::Lua(..) => Self::Lua,
        }
    }
}

impl HostOp {
    /// The window and module whose surface the op creates, drops, prepares, maps, unmaps or gives the keys to;
    /// `None` for every other op. A refused grab names its module, so its refusal withdraws that one request and
    /// no other the same batch made.
    pub fn surface(&self) -> Option<(WinId, ModuleId)> {
        match self {
            HostOp::CreateSurface { win, module, .. }
            | HostOp::DropSurface { win, module, .. }
            | HostOp::PrepareSurface { win, module }
            | HostOp::Map { win, module }
            | HostOp::Unmap { win, module }
            | HostOp::Focus {
                win,
                to: FocusTo::Module(module),
                ..
            } => Some((*win, module.clone())),
            _ => None,
        }
    }
}

/// A complete decision ordered as layout, effects, Lua, then one remaining shell action.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Batch {
    layout: Vec<HostOp>,
    effects: Vec<HostOp>,
    lua: Vec<LuaOp>,
    shell_verb: Option<ShellVerb>,
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

    /// Sets the final shell action, replacing a previous one with a debug diagnostic.
    pub fn set_shell_verb(&mut self, verb: ShellVerb) {
        let previous = self.shell_verb.replace(verb);
        debug_assert!(previous.is_none(), "a batch has at most one shell action");
    }

    /// Reports whether every segment is empty.
    pub fn is_empty(&self) -> bool {
        self.layout.is_empty() && self.effects.is_empty() && self.lua.is_empty() && self.shell_verb.is_none()
    }

    /// Iterates layout work before service effects.
    pub fn ops(&self) -> impl Iterator<Item = &HostOp> {
        self.layout.iter().chain(self.effects.iter())
    }

    /// Returns Lua calls in insertion order.
    pub fn lua(&self) -> &[LuaOp] {
        &self.lua
    }

    /// Returns the optional final shell action.
    pub fn shell_verb(&self) -> Option<&ShellVerb> {
        self.shell_verb.as_ref()
    }

    /// Appends each segment while preserving its internal order.
    pub fn append(&mut self, other: Batch) {
        self.layout.extend(other.layout);
        self.effects.extend(other.effects);
        self.lua.extend(other.lua);
        if let Some(verb) = other.shell_verb {
            self.set_shell_verb(verb);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::ModuleId;

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
        let verb = ShellVerb {
            win: WinId::MAIN,
            call: ShellCall::Action(crate::keymap::Action::FloatToggle),
            keycode: 4,
        };
        second.set_shell_verb(verb.clone());
        first.append(second);
        assert_eq!(
            first.ops().cloned().collect::<Vec<_>>(),
            vec![map(ModuleId::agent()), map(ModuleId::terminal()), start(), HostOp::Quit]
        );
        assert_eq!(first.lua(), &[emit("first"), emit("second")]);
        assert_eq!(first.shell_verb(), Some(&verb));
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
        tag!(
            HostOp::SetMode {
                win,
                mode: WindowMode::default()
            },
            SetMode
        );
        tag!(
            HostOp::MoveToMonitor {
                win,
                monitor: MonitorId("one".into())
            },
            MoveToMonitor
        );
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
                module: module.clone()
            },
            PrepareSurface
        );
        tag!(map(module.clone()), Map);
        tag!(
            HostOp::Unmap {
                win,
                module: module.clone()
            },
            Unmap
        );
        tag!(HostOp::Refuse { win, text: "no".into() }, Refuse);
        tag!(HostOp::ChromeHooks { win }, ChromeHooks);
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
        tag!(HostOp::Editor(EditorCall::End { win }), Editor);
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
        tag!(HostOp::Lua(emit("changed")), Lua);
    }
}
