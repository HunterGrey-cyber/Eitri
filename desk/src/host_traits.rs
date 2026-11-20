//! The traits a window's host is written against that do not name a toolkit: what the linked windows' code asks of
//! the host that carries it, which each host (`shell` on Linux, `eitri-mac` later) implements. It also holds the op
//! router's collaborator traits (surfaces, services, the grid, the editor, the hint digits, beside ops, the project
//! chooser), which `router` is written against.

// What the services' reads are documented against.
#[cfg(doc)]
use eitri_core::wm::HostReads;
use std::path::PathBuf;
use std::rc::Weak;

use eitri_core::layout::{ModuleId, Size};
use eitri_core::tabs::ViewId;
use eitri_core::window_link::names::WindowName;
use eitri_core::window_link::Msg;
use eitri_core::wm::{
    Applied, ChatOp, ChooserEnv, CleanupOp, DropWhy, EditorCall, HostSink, LuaOp, PeerId, PluginOp, ProjectOp,
    SurfaceKind, TermOp, WinId,
};

/// The answer to every op this shell does not carry out through the host yet; its own code still
/// does those, so a controller that emits one learns it went nowhere.
pub const NOT_CARRIED_OUT: &str = "not carried out by this shell yet";

/// What starts the project's second window for `OpenLinkedWindow`: the pair's own code, which belongs to the shell's
/// binary. A host without it cannot start one.
pub trait LinkedWindows {
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

/// Builds the window's base chat page when the controller says so (`HostOp::BuildBasePage`), which it does once and
/// never in a hub or a page window.
pub trait BasePageMaker {
    fn build(&self);
}

/// What builds a module's surface for `CreateSurface` and releases it for `DropSurface`, and makes and adopts the
/// place of a module arriving from the other window.
pub trait SurfaceMaker {
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
pub trait RespawnPane {
    fn shell_start(&self) -> eitri_core::wm::ShellStart;
    /// Opens a replacement server terminal in `dir`. The pane owns the asynchronous request.
    fn respawn_server(&self, _dir: PathBuf) -> Result<(), String> {
        Err("server terminal restart is not available yet".into())
    }
    fn close(&self);
    fn set_start_dir(&self, dir: PathBuf);
    fn start(&self);
}

/// Checks the captured shell before touching the pane: the same shell, given no key or mouse report since the read
/// that found it idle, since a command typed after the read runs in that same shell. No registry borrow outlives the
/// lookup.
pub fn respawn_pane(
    pane: Option<impl RespawnPane>,
    expect: eitri_core::wm::ShellStart,
    dir: PathBuf,
) -> Result<(), String> {
    let pane = pane.ok_or_else(|| "the terminal changed".to_string())?;
    if !expect.untouched(Some(pane.shell_start())) {
        return Err("the terminal changed".into());
    }
    if matches!(expect.identity, Some(eitri_core::wm::ShellIdentity::Server { .. })) {
        return pane.respawn_server(dir);
    }
    pane.close();
    pane.set_start_dir(dir);
    pane.start();
    Ok(())
}

/// The window's services the controller's effect ops call. `Err` says why an op was not carried out.
pub trait ServiceSet {
    fn view_of_tab(&self, _tab: eitri_core::tabs::TabId) -> Option<ViewId> {
        None
    }
    fn tab_moving(&self, _tab: eitri_core::tabs::TabId) -> bool {
        true
    }
    fn tab_has_work(&self, _tab: eitri_core::tabs::TabId) -> bool {
        false
    }
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
    fn terminal_job_names(&self) -> Vec<String> {
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
    /// Carries out a round key's step ([`HostOp::AgentsScan`]) off this thread, answered later as
    /// [`HostEvent::Agents`]; `at` is the module with the keys. A set with no agents layer carries none out.
    fn agents_scan(
        &self,
        win: WinId,
        step: eitri_core::wm::agents_round::RoundKey,
        at: Option<&ModuleId>,
    ) -> Result<(), String> {
        let _ = (win, step, at);
        Err(NOT_CARRIED_OUT.into())
    }
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
pub enum RestartRefused {
    /// The pane was not released: `state` names the state it was in.
    Pane { state: String },
    /// Nothing here restarts an editor: the words say why.
    Unavailable(String),
}

/// What the `Lua` ops run on: the window's Lua kernel. `Err` says why an op was not carried out.
pub trait LuaRunner {
    fn run(&self, op: &LuaOp) -> Result<(), String>;
}

/// A window's grid, as the host reads and drives it. A trait because the grid is each host's own widget; the
/// router's suites hand it a stand-in.
pub trait GridPort {
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
pub trait EditorReads {
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

/// The window's `select.number` digits and HINT's session, which stays the shell's.
pub trait HintPort {
    /// Draws each module's digit, the tenth module's as `0`.
    fn show_digits(&self, win: WinId, digits: &[(ModuleId, u8)]);
    /// Takes the digits down; nothing if none are up.
    fn hide_digits(&self, win: WinId);
    /// Starts or ends a HINT from the prefix; `keycode` is the key still held down that ran it.
    fn toggle(&self, win: WinId, keycode: u32);
}

/// What carries out a [`eitri_core::wm::BesideOp`] in a page window beside the person's own nvim: the shell binary's
/// page window, by the nvim it follows. A host without it carries none out.
pub trait BesidePort {
    /// Carries `op` out: an `Err` is the reason it was refused now; one a worker finds later is handed to `later`,
    /// on the main loop. Never waits.
    fn carry_out(&self, op: &eitri_core::wm::BesideOp, later: Box<dyn FnOnce(String)>) -> Result<(), String>;
}

/// The native project chooser and its asynchronous work, supplied without coupling the host to the binary.
pub trait ProjectHost {
    fn apply(&self, op: &ProjectOp) -> Applied;
    /// Carries out [`ProjectOp::Open`], with the activation token the started window comes forward with.
    fn open(&self, win: WinId, target: &std::path::Path, token: Option<String>) -> Applied;
    fn focus(&self, win: WinId) -> bool;
    fn env(&self, win: WinId) -> ChooserEnv;
}
