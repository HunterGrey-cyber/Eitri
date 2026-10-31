//! The window's services behind the controller's effect ops: what a terminal's start, a plugin page's
//! build, reload, visibility and stop, and a chat view's chooser call in the shell. The controller's
//! module lifecycle decides when each happens, from the layout after a change; this only carries the
//! op out on the module's own service, so no change hook in `build_ui` decides any of them.
//!
//! The keys' effects come here too: every module's "has the keys" signal (the editor's and each
//! terminal's cursor, each chat page's mode block, a plugin page's `focus` event), derived by the
//! controller from where GTK reports the keys; the literal key `send-prefix` and `send-keys` hand to
//! the module with the keys; the terminal's copy mode; and the navigation key a chat page claimed.
//!
//! A terminal's move to the other window is carried out here too: the freeze, whose answer is read off the GTK thread
//! and posted with the move's own id; the forget once the other window holds it, with a watch of the shell this
//! process started, whose exit only this process can read and the other window is told of; and the resume when the
//! move did not happen (`crate::terminal::moving`). So is a chat's: its freeze, forget and resume are the panel's
//! (answered later through its move hook), the arriving session's presence is held before the commit, and the
//! editor's review events of a moving session are held and released through the pair's review.
//!
//! A module's kill and a new instance end and begin here as well: a terminal's shell hung up, its directory read for
//! a split, an idle pane's next start directory; a chat view's tabs closed, its first tab opened or broken out of the
//! base view, and the views the workspaces still hold told to the panel.
//!
//! So do the editor's quit and the window's end. The quit's `:confirm qall` is sent as a watched request, nvim is
//! ended or its exited pane released when the controller says so, a released pane starts a fresh nvim on its own
//! widget with the start's spawn (`EditorCall::Restart`), and the teardown's steps run here in the order its
//! batch gives them: the pair's last write and the link's goodbye, nvim ended, every terminal hung up, the scratch
//! directory, the pane-switch channel and the editor feeds removed, the plugin tasks and the chat sessions ended with
//! the application held for what they still finish. What those last steps act on is made late in the window's build,
//! so it is handed over once, as [`Teardown`].
//!
//! A file another process asked this window to open arrives here twice: its client's answer (`open_answer`, from
//! [`Teardown`]'s requests) and, in the same batch or once nvim is ready, the call to nvim, followed from a bounded
//! timer by `crate::open_requests::watch_open`.
//!
//! Given to the GTK host once, before the startup layout's first batch (`GtkHost::set_services`).
//! An op this shell does not carry out yet is answered with the host's refusal.
//!
//! [`LuaKernel`] is what the host's `Lua` ops run on: the window's `init.lua`, loaded at the start's
//! kernel phase, and the events the start emits.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::{Rc, Weak};

use eitri_core::layout::ModuleId;
use eitri_core::tabs::ViewId;
use eitri_core::window_link::moves::MoveId;
use eitri_core::wm::{
    ChatOp, CleanupOp, EditorCall, HostEvent, HostSink, LuaOp, PairOp, PluginOp, TermEvent, TermOp, WinId,
};
use gtk4::prelude::*;
use neovide_editor::NeovideEditorPane;

use crate::agent_panel::AgentPanelHandle;
use crate::gtk_host::instance_pool::{InstancePool, Retire, Reused};
use crate::gtk_host::{respawn_pane, LuaRunner, RespawnPane, RestartRefused, ServiceSet, NOT_CARRIED_OUT};
use crate::lua::LuaEngine;
use crate::plugin_panels::PluginPanels;
use crate::terminal::registry::Pane;
use crate::terminal::TerminalPane;
use crate::webview_crash_guard::WebViewCrashGuard;

use eitri_core::wm::verbs::tabs::TabCall;

trait TabCalls {
    fn new_tab(&self);
    fn enter_input(&self, view: ViewId);
    fn step(&self, delta: i32) -> bool;
    fn select_last(&self) -> bool;
    fn select_number(&self, n: u16) -> bool;
    fn begin_rename(&self, view: ViewId);
    fn confirm_close(&self, view: ViewId);
    fn confirm_close_others(&self);
    fn open_chooser(&self, view: ViewId);
    fn open_detail(&self, view: ViewId);
}

impl TabCalls for AgentPanelHandle {
    fn new_tab(&self) {
        self.new_tab();
    }
    fn enter_input(&self, view: ViewId) {
        self.enter_input(view);
    }
    fn step(&self, delta: i32) -> bool {
        self.step(delta)
    }
    fn select_last(&self) -> bool {
        self.select_last()
    }
    fn select_number(&self, n: u16) -> bool {
        self.select_number(n).is_some()
    }
    fn begin_rename(&self, view: ViewId) {
        self.begin_rename(view);
    }
    fn confirm_close(&self, view: ViewId) {
        self.confirm_close(view);
    }
    fn confirm_close_others(&self) {
        self.confirm_close_others();
    }
    fn open_chooser(&self, view: ViewId) {
        self.open_chooser(view);
    }
    fn open_detail(&self, view: ViewId) {
        self.open_detail(view);
    }
}

fn tab_call(agent: &impl TabCalls, call: &TabCall) -> bool {
    match *call {
        TabCall::New => {
            agent.new_tab();
            agent.enter_input(ViewId::BASE);
        }
        TabCall::Step(delta) => return agent.step(delta),
        TabCall::Last => return agent.select_last(),
        TabCall::Select(n) => return agent.select_number(n),
        TabCall::Rename(view) => agent.begin_rename(view),
        TabCall::Close(view) => agent.confirm_close(view),
        TabCall::CloseOthers => agent.confirm_close_others(),
        TabCall::Choose(view) => agent.open_chooser(view),
        TabCall::Info(view) => agent.open_detail(view),
    }
    true
}

pub(crate) fn apply_tab_call(agent: &AgentPanelHandle, call: &TabCall) -> bool {
    tab_call(agent, call)
}

/// Every Lua panel's module, its view as `init.lua` registered it, its URL and its crash guard: what
/// a reload loads again and what a kill ends. A plugin page built after startup has its own view in
/// `PluginPanels`, which is asked first.
pub(crate) type LuaPages = Rc<Vec<(ModuleId, gtk4::Widget, String, Rc<RefCell<WebViewCrashGuard>>)>>;

/// One of the window's pools of panes or pages -- the terminals, the extra chat views' pages -- shared by the
/// services, the surface constructors and the shell's hooks. Every read hands out clones and every write is one call,
/// each holding the pool's borrow inside itself only: a pane or page is built, retired, started or asked anything only
/// once nothing holds the pool, so a widget made, a shell hung up or a signal GTK emits from inside finds it free.
pub(crate) struct Registry<P> {
    pool: RefCell<InstancePool<P>>,
}

impl<P: Clone> Registry<P> {
    pub(crate) fn new() -> Self {
        Self {
            pool: RefCell::new(InstancePool::new()),
        }
    }

    /// The live entry under `id`, cloned.
    pub(crate) fn get(&self, id: &ModuleId) -> Option<P> {
        self.pool.borrow().get(id).cloned()
    }

    /// Whether a live entry is held under `id`.
    pub(crate) fn holds(&self, id: &ModuleId) -> bool {
        self.pool.borrow().get(id).is_some()
    }

    /// Every live entry with its id, cloned.
    pub(crate) fn live(&self) -> Vec<(ModuleId, P)> {
        self.pool
            .borrow()
            .live()
            .map(|(id, entry)| (id.clone(), entry.clone()))
            .collect()
    }

    /// Every entry, live and idle, cloned.
    pub(crate) fn each(&self) -> Vec<P> {
        self.pool.borrow().each().cloned().collect()
    }
}

impl<P: Retire + Clone> Registry<P> {
    /// The entry under `id`, live or idle (made live again), else one `make` builds with nothing held, made live.
    pub(crate) fn take_or_make(&self, id: &ModuleId, make: impl FnOnce() -> P) -> (P, Reused) {
        let found = self.pool.borrow_mut().revive(id);
        if let Some(entry) = found {
            return (entry, Reused::Yes);
        }
        let made = make();
        self.pool.borrow_mut().insert(id, made.clone());
        (made, Reused::No)
    }

    /// The entry under `id` made live again (`InstancePool::revive`).
    pub(crate) fn revive(&self, id: &ModuleId) -> Option<P> {
        self.pool.borrow_mut().revive(id)
    }

    /// The live entry under `id` moved to its idle slot, its kind's retirement hook run once nothing holds the pool;
    /// whether there was one.
    pub(crate) fn retire(&self, id: &ModuleId) -> bool {
        let idle = self.pool.borrow_mut().set_idle(id);
        let Some(entry) = idle else {
            return false;
        };
        entry.on_retire();
        true
    }
}

/// What a terminal's start needs of a pane, so the decision can be tested without a display.
pub(crate) trait ShellPane: Pane + Clone {
    /// A shell is running in the pane.
    fn running(&self) -> bool;
    /// Wants a shell: spawned now if the pane has been laid out, else at its first allocation.
    fn start(&self);
}

impl ShellPane for TerminalPane {
    fn running(&self) -> bool {
        self.shell_pid().is_some()
    }
    fn start(&self) {
        TerminalPane::start(self);
    }
}

impl RespawnPane for TerminalPane {
    fn shell_start(&self) -> eitri_core::wm::ShellStart {
        eitri_core::wm::ShellStart::of(self.generation(), self.shell_pid()).typed(self.typed())
    }
    fn close(&self) {
        TerminalPane::close(self);
    }
    fn set_start_dir(&self, dir: PathBuf) {
        TerminalPane::set_start_dir(self, dir);
    }
    fn start(&self) {
        TerminalPane::start(self);
    }
}

/// Both filesystem reads run together on the worker; an unreadable cwd remains an error.
fn read_job_with(
    pid: Option<u32>,
    cwd: impl FnOnce(u32) -> std::io::Result<PathBuf>,
    busy: impl FnOnce(u32) -> std::io::Result<bool>,
) -> (Result<PathBuf, String>, bool) {
    let Some(pid) = pid else {
        return (Err("its shell is not running".into()), true);
    };
    (cwd(pid).map_err(|error| error.to_string()), busy(pid).unwrap_or(true))
}

/// Starts `module`'s pane unless a shell already runs in it; whether it started one. The pane is a clone the
/// registry handed out, so the start, which may spawn and report from inside, finds the registry free.
pub(crate) fn start_pane<P: ShellPane>(terminals: &Registry<P>, module: &ModuleId) -> Result<bool, String> {
    let Some(pane) = terminals.get(module) else {
        return Err(format!("{module} has no live pane"));
    };
    if pane.running() {
        return Ok(false);
    }
    pane.start();
    Ok(true)
}

/// Takes `module`'s idle pane back live, its next shell to start in `dir`: a new instance under a number a kill left
/// idle asks no create, so this is where its directory comes from. A live pane only gets the directory.
pub(crate) fn revive_pane(terminals: &Registry<TerminalPane>, module: &ModuleId, dir: PathBuf) -> Result<(), String> {
    let pane = terminals
        .revive(module)
        .ok_or_else(|| format!("{module} has no pane to take back"))?;
    pane.set_start_dir(dir);
    Ok(())
}

/// What the window's teardown ends that is no module's: made late in the window's build and handed over once
/// (`Services::teardown`). Held here, each lives as long as the window: the feeds' and the channel's own `Drop` would
/// remove the sockets nvim still writes to, and GTK may never drop what held them before, so each is removed
/// explicitly by its teardown step.
pub(crate) struct Teardown {
    /// What the plugin tasks and the chat sessions keep running while they finish, after the window has gone.
    pub(crate) app: gtk4::Application,
    pub(crate) pair: Rc<crate::linked_window::spawn::Pair>,
    /// The panel's scratch directory: removed here, since GTK may never drop the `ScratchDir` that owns it.
    pub(crate) scratch_path: Option<PathBuf>,
    pub(crate) pane_switch: Option<crate::pane_switch::PaneSwitchChannel>,
    pub(crate) theme_feed: Option<crate::theme::feed::ThemeFeed>,
    pub(crate) context_feed: Option<eitri_core::editor_context::feed::EditorContextFeed>,
    pub(crate) nvim_keys_feed: Option<eitri_core::nvim_keys::feed::NvimKeysFeed>,
    pub(crate) follow_socket: Option<eitri_core::plugin::follow_feed::FollowSocket>,
    /// The tutorial's lessons, written for this window's nvim; the next window's sweep is only a backstop.
    pub(crate) tutor: Option<Rc<crate::tutor::Tutor>>,
    /// The window's open socket and what waits on it; `None` in a window that holds none.
    pub(crate) open_requests: Option<Rc<crate::open_requests::OpenRequests>>,
    /// Nothing else holds the window's Lua engine once its build returns; this keeps it for the window's life, and
    /// is never read.
    #[allow(dead_code)]
    pub(crate) lua_engine: Rc<LuaEngine>,
    /// The terminal `claude`'s listener; `None` with `terminal.claude_ide` off and in a window with no editor.
    #[cfg(target_os = "linux")]
    pub(crate) ide: Option<Rc<crate::ide_wiring::IdeWiring>>,
}

impl Teardown {
    /// The pair file's last write, waited for briefly (a small file renamed into place). Window 2 closing while it
    /// keeps the file (window 1 closed first) leaves the pair open, so the next launch starts both windows again.
    /// Once window 1's layout file holds what it took back at its launch, the pair file forgets what window 2 held;
    /// while it does not (a save that failed), the pair file keeps naming them, so the next launch takes them back
    /// again. Then the other window hears this one is closing, rather than finding the link gone.
    fn pair_on_close(&self, layout_written: bool) {
        let pair = &self.pair;
        pair.with_pair_writer(|writer| {
            crate::linked_window::pair_files::on_close(writer, pair.is_second(), layout_written);
            if !writer.flush(std::time::Duration::from_secs(1)) {
                eprintln!("[window] the pair file was still being written; not waiting longer");
            }
        });
        if let Some(link) = pair.link() {
            link.close(&pair.closed_words());
        }
    }

    /// The editor's feeds, the plugins' follow socket, the open socket, the tutorial's lessons and the terminal
    /// `claude`'s listener: each removes its socket, directory or lock file now, not at a `Drop` that may never come (a directory a close leaves behind is swept by the
    /// next launch, as a backstop only).
    fn feeds(&self) {
        if let Some(feed) = &self.theme_feed {
            feed.cleanup();
        }
        if let Some(feed) = &self.context_feed {
            feed.cleanup();
        }
        if let Some(feed) = &self.nvim_keys_feed {
            feed.cleanup();
        }
        if let Some(socket) = &self.follow_socket {
            socket.cleanup();
        }
        if let Some(requests) = &self.open_requests {
            requests.close();
        }
        if let Some(tutor) = &self.tutor {
            tutor.cleanup();
        }
        #[cfg(target_os = "linux")]
        if let Some(ide) = &self.ide {
            ide.end();
        }
    }
}

/// The window's services, as the host's ops reach them.
pub(crate) struct Services {
    /// `None` in a window with no editor (window 2), where an editor op has nothing to reach and is
    /// refused. Every full window has the editor (a Lua `main` panel hides it), so `main.rs` passes
    /// `Some`, and a write of `g:neovide_scale_factor` before nvim has started is buffered by the pane.
    pub(crate) editor: Option<Rc<NeovideEditorPane>>,
    /// The handle every question to this window's nvim goes through; in a window with no editor it answers that.
    pub(crate) editor_rpc: Rc<dyn eitri_core::editor_rpc::EditorRpc>,
    pub(crate) terminals: Rc<Registry<TerminalPane>>,
    pub(crate) plugin_panels: Rc<PluginPanels>,
    pub(crate) agent: AgentPanelHandle,
    /// The panel's live px, shared with `main.rs`'s colorscheme listener (`panel_tokens`, via
    /// `crate::text_size::live_panel_font_size_px`) so a colorscheme change re-derives every other token but never
    /// resets a zoom. This struct writes it; `main.rs` only ever reads it.
    pub(crate) panel_px: Rc<Cell<f32>>,
    /// The window's terminal text zoom, shared with `main.rs`, whose new terminal panes start at it. This struct
    /// writes it.
    pub(crate) terminal_zoom: Rc<Cell<f32>>,
    pub(crate) lua_pages: LuaPages,
    /// Where a terminal's shell starts when its directory cannot be read.
    pub(crate) project_dir: PathBuf,
    /// Where a directory read off the toolkit's thread is reported; weak, as every host's sink is.
    pub(crate) sink: Weak<dyn HostSink>,
    /// What the teardown ends beyond the modules, once the window's build has handed it over: written once, and read
    /// only through [`Services::teardown`], which hands out the `Rc`.
    pub(crate) teardown: Rc<RefCell<Option<Rc<Teardown>>>>,
    /// The freeze each terminal was last asked for by a move: only that one's refusal gives a move up.
    pub(crate) freezes: Rc<crate::terminal::moving::Freezes>,
    /// Holds or releases the editor's review events of a moving chat's session (`GroupReview::hold_session`):
    /// `None` in a window that is no pair's, where no chat moves.
    pub(crate) review_hold: Option<ReviewHold>,
}

/// [`Services::review_hold`]: the session, whether to hold, and whether this window holds the session after a
/// release.
pub(crate) type ReviewHold = Rc<dyn Fn(&str, bool, bool) -> Result<(), String>>;

impl Services {
    /// The application the teardown holds while what it ended finishes; `Err` before the window's build handed it
    /// over.
    fn app(&self) -> Result<gtk4::Application, String> {
        Ok(self.teardown()?.app.clone())
    }

    /// What the teardown ends, handed out so nothing holds its slot while a step runs; `Err` before the window's
    /// build handed it over.
    fn teardown(&self) -> Result<Rc<Teardown>, String> {
        let teardown = self.teardown.borrow().clone();
        teardown.ok_or_else(|| NOT_CARRIED_OUT.to_string())
    }

    /// `module`'s live pane, a clone the registry handed out, so nothing holds the registry while it is asked.
    fn pane(&self, module: &ModuleId) -> Result<TerminalPane, String> {
        self.terminals
            .get(module)
            .ok_or_else(|| format!("{module} has no live pane"))
    }

    /// Captures the pane's start before one worker reads its cwd and the whole session. The controller
    /// compares that capture with later start/exit events and bounds the request with its own deadline.
    fn read_job(&self, win: WinId, module: &ModuleId, read: u64) -> Result<(), String> {
        let pane = self.pane(module)?;
        let shell = pane.shell_start();
        let sink = self.sink.clone();
        let module = module.clone();
        gtk4::glib::spawn_future_local(async move {
            let (cwd, busy) = gtk4::gio::spawn_blocking(move || {
                read_job_with(
                    shell.pid,
                    |pid| crate::terminal::usable_dir(std::fs::read_link(format!("/proc/{pid}/cwd"))),
                    crate::terminal::jobs::session_has_other_process,
                )
            })
            .await
            .unwrap_or_else(|_| (Err("the reading thread panicked".into()), true));
            if let Some(sink) = sink.upgrade() {
                sink.post(HostEvent::Terminal(TermEvent::Job {
                    win,
                    module,
                    read,
                    shell,
                    cwd,
                    busy,
                }));
            }
        });
        Ok(())
    }

    /// Reads the directory `module`'s shell is in now, on a worker thread (a slow or hung filesystem must never
    /// stall the window), and reports it as [`TermEvent::Cwd`]: the project root, with one log line, where it cannot
    /// be read or no shell runs; a refusal when the terminal was killed while it was read, since its number may
    /// name a new terminal by then.
    fn read_cwd(&self, win: WinId, module: &ModuleId, read: u64) -> Result<(), String> {
        let pane = self.pane(module)?;
        let captured = eitri_core::wm::ShellStart::of(pane.generation(), pane.shell_pid());
        let terminals = Rc::downgrade(&self.terminals);
        let root = self.project_dir.clone();
        let sink = self.sink.clone();
        let module = module.clone();
        gtk4::glib::spawn_future_local(async move {
            let found = match captured.pid {
                // The stat that tells a removed directory runs here too, off the GTK thread.
                Some(pid) => gtk4::gio::spawn_blocking(move || {
                    crate::terminal::usable_dir(std::fs::read_link(format!("/proc/{pid}/cwd")))
                })
                .await
                .unwrap_or_else(|_| Err(std::io::Error::other("the reading thread panicked"))),
                None => Err(std::io::Error::other("its shell is not running")),
            };
            let now = terminals.upgrade().and_then(|terminals| {
                terminals
                    .get(&module)
                    .map(|pane| eitri_core::wm::ShellStart::of(pane.generation(), pane.shell_pid()))
            });
            let dir = if captured.still(now) {
                let (dir, note) = crate::terminal::start_dir(found, &root);
                if let Some(note) = note {
                    println!("[terminal] {note}");
                }
                Ok(dir)
            } else {
                Err(format!(
                    "{module} closed while its directory was read; no terminal was opened"
                ))
            };
            if let Some(sink) = sink.upgrade() {
                sink.post(HostEvent::Terminal(TermEvent::Cwd { win, module, read, dir }));
            }
        });
        Ok(())
    }
}

impl Services {
    /// Freezes `module` for move `mv`. The session's answer is waited for and written down off the GTK thread, then
    /// posted with `mv` itself: the terminal's record and handoff, or, for the freeze still asked for only, its
    /// refusal naming the module.
    fn detach(&self, win: WinId, module: &ModuleId, mv: MoveId) -> Result<(), String> {
        use crate::terminal::moving::{self, Frozen};
        let answer = self.pane(module)?.detach()?;
        self.freezes.asked(module, mv);
        let freezes = self.freezes.clone();
        let sink = self.sink.clone();
        let module = module.clone();
        gtk4::glib::spawn_future_local(async move {
            let frozen = gtk4::gio::spawn_blocking(move || moving::wait_frozen(answer, moving::ANSWER_WAIT))
                .await
                .unwrap_or_else(|_| Frozen::Refused("the thread reading the terminal failed".into()));
            let answer = moving::answer_for(&freezes, &module, mv, frozen);
            if let (Some(event), Some(sink)) = (moving::event_for(win, module, mv, answer), sink.upgrade()) {
                sink.post(event);
            }
        });
        Ok(())
    }

    /// The other window holds `module`'s terminal now: the pane lets it go, and the shell, if this process started
    /// it, is watched until it ends, so how it ended reaches whichever window runs it then.
    fn forget(&self, module: &ModuleId) -> Result<(), String> {
        let pane = self.pane(module)?;
        self.freezes.forget(module);
        let pid = pane.shell_pid();
        pane.forget();
        if let Some(pid) = pid {
            self.watch_moved(pid);
        }
        Ok(())
    }

    /// Watches the shell `pid`, which went to the other window, off the GTK thread. Its exit is given to this
    /// window's own terminal when it came back here, else sent to the other window as `child_exited`.
    fn watch_moved(&self, pid: u32) {
        let (report, reported) = futures_channel::oneshot::channel();
        let watched = crate::linked_window::spawn::watch_moved_child(pid, move |start, status| {
            let _ = report.send((start, status));
        });
        if let Err(err) = watched {
            eprintln!("[terminal] the shell {pid} that moved cannot be watched: {err}");
            return;
        }
        let terminals = Rc::downgrade(&self.terminals);
        let teardown = Rc::downgrade(&self.teardown);
        gtk4::glib::spawn_future_local(async move {
            let Ok((start, status)) = reported.await else {
                return;
            };
            let here = terminals
                .upgrade()
                .is_some_and(|terminals| child_exited_here(&terminals, pid, start, status));
            if here {
                return;
            }
            let link = teardown.upgrade().and_then(|slot| {
                let teardown = slot.borrow().clone();
                teardown.and_then(|teardown| teardown.pair.link())
            });
            match link {
                Some(link) => {
                    let msg = eitri_core::window_link::Msg::ChildExited { pid, start, status };
                    if let Err(err) = link.send(msg, Vec::new()) {
                        eprintln!("[terminal] the end of the shell {pid} could not be told: {err}");
                    }
                }
                None => println!("[terminal] the shell {pid} that moved ended, with no other window to tell"),
            }
        });
    }
}

/// Gives the end of the shell `pid` started at `start` (a raw wait status, `None` when it could not be read) to the
/// terminal here that runs it, a shell this window took over from the other one; whether one did.
pub(crate) fn child_exited_here(terminals: &Registry<TerminalPane>, pid: u32, start: u64, status: Option<i32>) -> bool {
    use std::os::unix::process::ExitStatusExt;
    let Some(status) = status else {
        return false;
    };
    // The registry hands out clones, so nothing holds it while any pane is asked.
    let panes = terminals.each();
    let status = std::process::ExitStatus::from_raw(status);
    panes.iter().any(|pane| pane.child_exited(pid, start, status))
}

impl ServiceSet for Services {
    fn page_built(&self, module: &ModuleId) -> bool {
        self.plugin_panels.is_built(module)
    }

    fn tab_can_step(&self, delta: i32) -> bool {
        self.agent.can_step(delta)
    }
    fn tab_has_last(&self) -> bool {
        self.agent.has_last()
    }
    fn tab_place(&self, n: u16) -> Option<eitri_core::tab_set::TabPlace> {
        self.agent.tab_place(n)
    }
    fn terminal_job_modules(&self) -> Vec<ModuleId> {
        self.terminals
            .live()
            .into_iter()
            .filter(|(_, pane)| pane.shell_pid().is_some_and(crate::terminal::jobs::has_foreground_job))
            .map(|(id, _)| id)
            .collect()
    }
    fn pages_with_tasks(&self) -> Vec<ModuleId> {
        self.plugin_panels.pages_with_tasks()
    }
    fn terminal_jobs(&self) -> usize {
        crate::terminal::jobs::count_terminal_jobs(&self.terminals.each())
    }
    fn editor(&self, op: &EditorCall) -> Result<(), String> {
        match op {
            // Neovide draws the cursor, or none, and tells nvim `FocusGained`/`FocusLost`.
            EditorCall::SetFocused { focused, .. } => {
                if let Some(editor) = &self.editor {
                    editor.set_focused(*focused);
                }
                Ok(())
            }
            EditorCall::SendKeys { keys, .. } => {
                if let Some(editor) = &self.editor {
                    editor.send_keys(keys);
                }
                Ok(())
            }
            // nvim's own `:confirm qall`, as a request whose answers the pane watches: never typed keys, which a key
            // nvim waits for (after `f`) would swallow. Refused when nvim is not running, so nothing was sent.
            EditorCall::Quit { lua, .. } => match &self.editor {
                Some(editor) if editor.exec_lua_watched(lua) => Ok(()),
                Some(_) => Err("nvim is not running; the quit was not sent".into()),
                None => Err("this window has no editor".into()),
            },
            // stdin closed and SIGTERM, SIGKILL by its pid after the pane's patience; its exit is reported as any
            // exit is, once the process is gone. Never `:qa!`: the swap files keep the unsaved changes.
            EditorCall::End { .. } => match &self.editor {
                Some(editor) if editor.end_nvim() => Ok(()),
                Some(_) => Err("nvim has no session to end".into()),
                None => Err("this window has no editor".into()),
            },
            // At the close: ends nvim by its pid if it has not exited within the fork's wait. nvim has normally
            // exited by then (the close waited for its own `:confirm qall`), so this returns at once.
            EditorCall::Shutdown { .. } => {
                if let Some(editor) = &self.editor {
                    editor.shutdown();
                }
                Ok(())
            }
            // An exited nvim's pane let go, so nothing reaches the closed connection; refused while nvim's process
            // still lives. A pane let go already has nothing left to release, and is answered as released.
            EditorCall::ReleaseExited { .. } => match &self.editor {
                Some(editor) if editor.release_exited() => Ok(()),
                Some(editor) if editor.nvim_exited() && !editor.is_ready() => Ok(()),
                Some(_) => Err("nvim has not exited; the pane was not released".into()),
                None => Err("this window has no editor".into()),
            },
            EditorCall::Restart { spawn, .. } => self.restart_editor(spawn).map_err(|refused| match refused {
                RestartRefused::Pane { state } => format!("the editor pane is {state}"),
                RestartRefused::Unavailable(why) => why,
            }),
            EditorCall::SetScale { scale, .. } => {
                let editor = self.editor.as_ref().ok_or("this window has no editor")?;
                editor.set_scale_factor_setting(*scale as f32);
                Ok(())
            }
            // Carried out either way: a call that could not be made is an error in its answer, reported as any is.
            EditorCall::Open { win, path, line, col } => {
                let pending = eitri_core::editor_open::open(self.editor_rpc.as_ref(), path, *line, *col);
                crate::open_requests::watch_open(pending, *win, self.sink.clone());
                Ok(())
            }
            EditorCall::Reveal { .. } => Err(NOT_CARRIED_OUT.into()),
        }
    }

    fn open_answer(&self, id: u64, accepted: bool) -> Result<(), String> {
        let teardown = self.teardown()?;
        let requests = teardown
            .open_requests
            .as_ref()
            .ok_or("this window holds no open socket")?;
        requests.answer(id, accepted);
        Ok(())
    }

    // The same pane and widget, so every clone the window holds of it reaches the new nvim; the callbacks it has
    // registered report on that nvim too.
    fn restart_editor(&self, spawn: &eitri_core::wm::EditorSpawn) -> Result<(), RestartRefused> {
        let Some(editor) = &self.editor else {
            return Err(RestartRefused::Unavailable("this window has no editor".into()));
        };
        editor
            .restart(crate::gtk_host_surfaces::editor_options(spawn))
            .map_err(|refused| RestartRefused::Pane {
                state: refused.state.to_string(),
            })
    }

    fn terminal(&self, op: &TermOp) -> Result<(), String> {
        match op {
            TermOp::Start { module, .. } => start_pane(&self.terminals, module).map(drop),
            TermOp::SetFocused { module, focused, .. } => {
                self.pane(module)?.set_focused(*focused);
                Ok(())
            }
            TermOp::CopyMode { module, up, .. } => {
                self.pane(module)?.enter_copy_mode(*up);
                Ok(())
            }
            // The terminal's engine encodes the key, or the key has no encoding there: said, and nothing sent.
            TermOp::LiteralKey { module, key, .. } => {
                let pane = self.pane(module)?;
                match crate::terminal::keys::literal(key) {
                    Some(inputs) => inputs.into_iter().for_each(|input| pane.send(input)),
                    None => println!("[prefix] {key}: no key event can express it"),
                }
                Ok(())
            }
            // The shell is hung up and the pane forgets it, so its next start is a fresh shell on a blank pane.
            TermOp::HangUp { module, .. } => {
                self.pane(module)?.close();
                Ok(())
            }
            TermOp::ReadCwd { win, module, read } => self.read_cwd(*win, module, *read),
            TermOp::SetStartDir { module, dir, .. } => revive_pane(&self.terminals, module, dir.clone()),
            // Every terminal's shell hung up without waiting for it (`TerminalSession`'s `Drop`); should the process
            // exit first, the kernel closing each PTY master hangs it up anyway.
            TermOp::ShutdownAll { .. } => {
                for terminal in self.terminals.each() {
                    terminal.shutdown();
                }
                Ok(())
            }
            TermOp::Detach { win, module, mv } => self.detach(*win, module, *mv),
            TermOp::Forget { module, .. } => self.forget(module),
            TermOp::Resume { module, .. } => {
                self.freezes.forget(module);
                self.pane(module)?.resume();
                Ok(())
            }
            TermOp::SendKeys { .. } => Err(NOT_CARRIED_OUT.into()),
            // Every terminal of the window, idle ones too, so one reused later is already at it; and the zoom a
            // pane made later starts at. A pane at the level already does nothing.
            TermOp::SetZoom { level, .. } => {
                self.terminal_zoom.set(*level);
                for pane in self.terminals.each() {
                    pane.set_zoom(*level);
                }
                Ok(())
            }
            TermOp::ReadJob { win, module, read } => self.read_job(*win, module, *read),
            TermOp::Respawn {
                module, dir, expect, ..
            } => respawn_pane(self.terminals.get(module), *expect, dir.clone()),
        }
    }

    fn plugin(&self, op: &PluginOp) -> Result<(), String> {
        match op {
            PluginOp::Freeze { win, module, mv } => {
                if let Err(why) = self.plugin_panels.freeze(*win, module, *mv) {
                    if let Some(sink) = self.sink.upgrade() {
                        sink.post(HostEvent::Plugin(eitri_core::wm::PluginEvent::FreezeRefused {
                            win: *win,
                            module: module.clone(),
                            mv: *mv,
                            why,
                        }));
                    }
                }
                Ok(())
            }
            PluginOp::Forget { module, .. } => {
                self.plugin_panels.forget(module);
                Ok(())
            }
            PluginOp::Resume { module, .. } => {
                self.plugin_panels.resume(module);
                Ok(())
            }
            // Reopen its holding and reload a kept view; a missing view waits for its first show and `Build`.
            PluginOp::Hold { module, .. } => {
                self.plugin_panels.activate_holding(module);
                Ok(())
            }
            PluginOp::SetZoom { module, level, .. } => {
                let view = self
                    .plugin_panels
                    .webview(module)
                    .or_else(|| {
                        self.lua_pages
                            .iter()
                            .find(|(id, _, _, _)| id == module)
                            .and_then(|(_, widget, _, _)| widget.downcast_ref::<webkit6::WebView>().cloned())
                    })
                    .ok_or_else(|| format!("{module}: no page view"))?;
                crate::plugin_page::set_zoom(&view, *level);
                Ok(())
            }
            // A new nvim runs in the editor's pane: a page following the editor follows it, its snippet installed
            // there again.
            PluginOp::EditorBack { .. } => {
                self.plugin_panels.editor_back();
                Ok(())
            }
            // Every plugin task ends with the window; the application waits for them, at most the grace.
            PluginOp::ShutdownAll { .. } => {
                self.plugin_panels.shutdown(&self.app()?);
                Ok(())
            }
            // No editor is coming back to this window: a page following one says so.
            PluginOp::EditorClosed { .. } => {
                self.plugin_panels.editor_closed();
                Ok(())
            }
            // Any page with no view here is built: one that waits for its first show, or one whose view was
            // dropped when it left for the other window. A page the startup or a grab already built is left as it
            // is.
            PluginOp::Build { module, .. } => {
                if !self.plugin_panels.is_built(module) {
                    self.plugin_panels.activate_holding(module);
                    self.plugin_panels.build_page(module, &self.plugin_panels.theme());
                }
                Ok(())
            }
            PluginOp::Reload {
                module, reset_guard, ..
            } => {
                let Some((_, widget, url, crash_guard)) = self.lua_pages.iter().find(|(m, _, _, _)| m == module) else {
                    return Err(format!("{module} is not a Lua panel"));
                };
                println!("[modules] {module}: shown again after a kill, starting it fresh");
                let webview = self
                    .plugin_panels
                    .webview(module)
                    .or_else(|| widget.downcast_ref::<webkit6::WebView>().cloned());
                if let Some(webview) = webview {
                    use webkit6::prelude::*;
                    webview.load_uri(url);
                }
                // The manual recovery a give-up message points the user at (`prefix x`, then this panel's own
                // key): the guard is armed again, so a later, unrelated crash is caught fresh rather than
                // dropped as already given up.
                if *reset_guard {
                    crash_guard.borrow_mut().reset();
                }
                Ok(())
            }
            PluginOp::OnScreen { module, on, .. } => {
                self.plugin_panels.set_on_screen(module, *on);
                Ok(())
            }
            // A no-op for a Lua panel that is not a plugin.
            PluginOp::Terminate { module, .. } => {
                self.plugin_panels.stop(module);
                Ok(())
            }
            // A page hears this as its `focus` event; a Lua page that is no plugin's has no page to tell.
            PluginOp::HasKeys { module, has_keys, .. } => {
                self.plugin_panels.set_has_keys(module, *has_keys);
                Ok(())
            }
            PluginOp::TerminalAtAnswer {
                plugin, ticket, answer, ..
            } => {
                self.plugin_panels.terminal_at_answered(plugin, *ticket, answer.clone());
                Ok(())
            }
        }
    }

    fn chat(&self, op: &ChatOp) -> Result<(), String> {
        match op {
            ChatOp::Tab { call, .. } => {
                apply_tab_call(&self.agent, call);
                Ok(())
            }
            ChatOp::OpenChooser { view, .. } => {
                println!(
                    "[modules] {}: shown again after a kill, starting it fresh",
                    view.module()
                );
                self.agent.open_chooser(*view);
                Ok(())
            }
            // Into a chat view: when asked, the oldest card among that view's tabs (the base view switches to the tab
            // holding it first), else the view's own arrival.
            // Into a chat view: the oldest card among that view's tabs (the base view switches to the tab holding it
            // first), else BROWSE, as every keyboard arrival there is -- on the last row if the reader was following
            // it, else on the row and scroll they left (the page decides, `App.tsx`'s `arrive` effect).
            ChatOp::Arrive { view, card_first, .. } => {
                if !(*card_first && self.agent.focus_oldest_card(*view)) {
                    self.agent.arrive(*view);
                }
                Ok(())
            }
            // Each chat view's page is told for itself, so exactly one is told it has the keys.
            ChatOp::PaneFocused { view, focused, .. } => {
                self.agent.set_pane_focused(*view, *focused);
                Ok(())
            }
            ChatOp::NavKey { view, direction, .. } => {
                self.agent.nav_key(*view, *direction);
                Ok(())
            }
            ChatOp::LiteralKey { view, key, .. } => {
                self.agent.literal_key(*view, key);
                Ok(())
            }
            // The typing window that paces the panel's stream only counts while the editor holds the keys.
            ChatOp::EditorHasKeys { has_keys, .. } => {
                self.agent.set_editor_has_keys(*has_keys);
                Ok(())
            }
            // A float left these views. Each page is told it is on screen once the change that moved the float is
            // over, never inside it: the panel's attention hook reads the layout and the grid again, and checks then
            // that the page is mapped and clear, so a view that left the screen with its float sees nothing.
            ChatOp::TellUncovered { views, .. } => {
                let agent = self.agent.clone();
                let views = views.clone();
                gtk4::glib::idle_add_local_once(move || {
                    for view in views {
                        agent.page_shown(view);
                    }
                });
                Ok(())
            }
            // A kill's: the base view's own tabs, or an extra view's one tab. A tab shown in another view is that
            // view's.
            ChatOp::CloseTabs { view, .. } => {
                let closed = self.agent.close_view_tabs(*view)?;
                println!("[modules] {}: {closed} session tab(s) closed", view.module());
                Ok(())
            }
            // A new chat view's first tab: a new empty tab of the window's default agent, or the base view's active
            // tab brought out of it.
            ChatOp::OpenTab { view, .. } => self.agent.open_in(*view).map(drop),
            ChatOp::BreakOut { view, .. } => self.agent.break_out(*view).map(drop),
            // A restore still to come must not bring a tab back into a view that has left: its page is idle, so the
            // tab would show nowhere.
            ChatOp::LayoutViews { views, .. } => {
                self.agent.set_layout_views(views.clone());
                Ok(())
            }
            ChatOp::SetFontSize { px, .. } => {
                self.panel_px.set(*px);
                self.agent.set_panel_font_size_px(*px);
                Ok(())
            }
            ChatOp::PageShown { .. }
            | ChatOp::JoinBack { .. }
            | ChatOp::OpenKeymap { .. }
            | ChatOp::CommandLine { .. }
            | ChatOp::Reload { .. }
            | ChatOp::SetCoverage { .. } => Err(NOT_CARRIED_OUT.into()),
            // A chat's move: the panel freezes the tab and its agent and answers later with the move's own id, lets
            // it go once the other window holds it, or takes it back.
            ChatOp::Freeze { view, tab, mv, .. } => {
                self.agent.freeze_for_move(*view, *tab, mv.0);
                Ok(())
            }
            ChatOp::Forget { view, tab, .. } => self.agent.forget_moved(*view, *tab),
            ChatOp::Resume { view, tab, .. } => {
                self.agent.resume_moved(*view, *tab);
                Ok(())
            }
            ChatOp::ReviewHold {
                session, hold, here, ..
            } => match &self.review_hold {
                Some(hold_session) => hold_session(session, *hold, *here),
                None => Err(NOT_CARRIED_OUT.into()),
            },
            ChatOp::HoldPresence { session, mv, .. } => {
                self.agent.hold_presence(session, mv.0);
                Ok(())
            }
            ChatOp::DropPresence { session, mv, .. } => {
                self.agent.drop_presence(session, mv.0);
                Ok(())
            }
            // Every session the panel started ends (settings restored, hook sockets removed, children sent
            // SIGTERM) on workers, so nothing here waits for a child; held, the application stays alive with no
            // window on screen until they report.
            ChatOp::ShutdownAll { hold, .. } => {
                let left = self.agent.shutdown();
                if *hold {
                    crate::agent_panel::hold_until_done(&self.app()?, left);
                }
                Ok(())
            }
        }
    }

    fn cleanup(&self, op: &CleanupOp) -> Result<(), String> {
        let teardown = self.teardown()?;
        match op {
            CleanupOp::EndGroup => teardown.pair.end_group(),
            CleanupOp::GroupAsked => teardown.pair.group_asked(),
            CleanupOp::KeepThisWindow => teardown.pair.keep_this_window(),
            CleanupOp::PairOnClose { layout_written } => {
                if let Some(write) = teardown.pair.finish_group_tabs() {
                    self.agent.hold_close_for_tabs(write);
                }
                teardown.pair_on_close(*layout_written);
            }
            CleanupOp::ScratchDir => {
                if let Some(path) = &teardown.scratch_path {
                    let _ = std::fs::remove_dir_all(path);
                }
            }
            // Explicitly, not by `Drop`: GTK does not reliably free what held the channel before the process exits,
            // which left the shim directory behind after every close.
            CleanupOp::PaneSwitchDir => {
                if let Some(channel) = &teardown.pane_switch {
                    channel.cleanup();
                }
            }
            CleanupOp::Feeds => teardown.feeds(),
        }
        Ok(())
    }

    fn pair(&self, op: &PairOp) -> Result<(), String> {
        use crate::linked_window::pair_files;
        match op {
            PairOp::Orphaned { held, .. } => self.teardown()?.pair.with_pair_writer(|writer| {
                writer.window2_holds(held);
                writer.take_over();
            }),
            PairOp::Unrestored { .. } => {
                let pair = self.teardown()?.pair.clone();
                let alone = pair_files::alone_on_project(
                    eitri_core::turn_review::review_dir_for(
                        std::env::var_os("XDG_STATE_HOME").as_deref(),
                        std::env::var_os("HOME").as_deref(),
                        &self.project_dir,
                    )
                    .as_deref(),
                );
                let mut files = None;
                pair.with_pair_writer(|writer| files = Some(pair_files::take_back_unrestored(Some(writer), alone)));
                self.agent
                    .set_tab_files(files.unwrap_or_else(|| pair_files::take_back_unrestored(None, alone)));
                self.agent.release_launch_restore();
            }
            PairOp::ReloadPage { module, .. } => {
                if crate::linked_window::reload_page_back(&self.plugin_panels, &self.lua_pages, module) {
                    println!("[window] {module}: loaded again, from its store as it is now");
                }
            }
        }
        Ok(())
    }
}

/// The window's Lua kernel, as the start's `Lua` ops reach it.
pub(crate) struct LuaKernel {
    pub(crate) engine: Rc<LuaEngine>,
    /// The `init.lua` the kernel phase loads.
    pub(crate) init_file: PathBuf,
}

impl LuaRunner for LuaKernel {
    fn run(&self, op: &LuaOp) -> Result<(), String> {
        match op {
            // A file that fails ends the process, naming it: `init.lua`'s refusals are startup failures.
            LuaOp::LoadInit => {
                self.engine.load_init_file(&self.init_file);
                Ok(())
            }
            LuaOp::Emit { event } => {
                self.engine.emit(event);
                Ok(())
            }
            LuaOp::Invoke { .. } => Err(NOT_CARRIED_OUT.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::layout::ModuleKind;
    use std::cell::Cell;

    use eitri_core::tabs::TabVerb;

    #[derive(Default)]
    struct RecordingTabs {
        calls: RefCell<Vec<String>>,
    }
    impl TabCalls for RecordingTabs {
        fn new_tab(&self) {
            self.calls.borrow_mut().push("new".into());
        }
        fn enter_input(&self, view: ViewId) {
            self.calls.borrow_mut().push(format!("input {view:?}"));
        }
        fn step(&self, delta: i32) -> bool {
            self.calls.borrow_mut().push(format!("step {delta}"));
            true
        }
        fn select_last(&self) -> bool {
            self.calls.borrow_mut().push("last".into());
            false
        }
        fn select_number(&self, n: u16) -> bool {
            self.calls.borrow_mut().push(format!("select {n}"));
            n <= 2
        }
        fn begin_rename(&self, view: ViewId) {
            self.calls.borrow_mut().push(format!("rename {view:?}"));
        }
        fn confirm_close(&self, view: ViewId) {
            self.calls.borrow_mut().push(format!("close {view:?}"));
        }
        fn confirm_close_others(&self) {
            self.calls.borrow_mut().push("close others".into());
        }
        fn open_chooser(&self, view: ViewId) {
            self.calls.borrow_mut().push(format!("choose {view:?}"));
        }
        fn open_detail(&self, view: ViewId) {
            self.calls.borrow_mut().push(format!("info {view:?}"));
        }
    }
    #[test]
    fn the_companion_and_the_main_window_map_a_tab_verb_to_the_same_panel_call() {
        for (verb, expected) in [
            (TabVerb::New, vec!["new", "input ViewId(1)"]),
            (TabVerb::Step(1), vec!["step 1"]),
            (TabVerb::Last, vec!["last"]),
            (TabVerb::Select(9), vec!["select 9"]),
            (TabVerb::Rename, vec!["rename ViewId(1)"]),
            (TabVerb::Close, vec!["close ViewId(1)"]),
            (TabVerb::CloseOthers, vec!["close others"]),
            (TabVerb::Choose, vec!["choose ViewId(1)"]),
            (TabVerb::Info, vec!["info ViewId(1)"]),
            (TabVerb::Flash, vec![]),
            (TabVerb::Nothing, vec![]),
            (TabVerb::Sibling(1), vec![]),
            (TabVerb::Terminal(1), vec![]),
        ] {
            let panel = RecordingTabs::default();
            if let Some(call) = eitri_core::wm::verbs::tabs::base_call(&verb) {
                tab_call(&panel, &call);
            }
            assert_eq!(*panel.calls.borrow(), expected);
        }
        let source = eitri_core::source_scan::read_required("shell/src/companion/window.rs");
        let (_, body) = eitri_core::source_scan::function_in(&[source], "run_tab");
        let body: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(body.contains("apply_tab_call("));
        for forbidden in [
            "agent.step(",
            "select_last(",
            "select_number(",
            "new_tab(",
            "begin_rename(",
            "confirm_close",
            "open_chooser(",
            "open_detail(",
        ] {
            assert!(!body.contains(forbidden), "{forbidden}");
        }
    }
    #[test]
    fn the_companion_toasts_no_such_tab_when_the_switch_finds_none() {
        let panel = RecordingTabs::default();
        let mut toasts = vec![];
        for (verb, flashes) in [
            (TabVerb::Select(9), true),
            (TabVerb::Last, true),
            (TabVerb::Rename, false),
            (TabVerb::Flash, true),
            (TabVerb::Nothing, false),
        ] {
            let answer = eitri_core::wm::verbs::tabs::base_call(&verb).map(|call| tab_call(&panel, &call));
            if crate::companion::window::tab_failed(&verb, answer) {
                toasts.push("no such tab");
            }
            assert_eq!(toasts.last().is_some(), flashes);
            toasts.clear();
        }
    }

    #[test]
    fn read_job_reports_an_unreadable_cwd_as_err_not_the_root() {
        let (cwd, busy) = read_job_with(
            Some(42),
            |_| Err(std::io::Error::from_raw_os_error(libc::EACCES)),
            |_| Ok(false),
        );
        assert!(cwd.is_err());
        assert!(!busy);
        let (cwd, busy) = read_job_with(None, |_| unreachable!(), |_| unreachable!());
        assert!(cwd.is_err());
        assert!(busy);
        let (_, busy) = read_job_with(
            Some(42),
            |_| Ok(PathBuf::from("/")),
            |_| Err(std::io::Error::from_raw_os_error(libc::EACCES)),
        );
        assert!(busy, "a failed session scan leaves the terminal alone");
    }

    /// A pane whose shell runs from its first start on; counts its starts and closes.
    #[derive(Clone, Default)]
    struct FakePane {
        starts: Rc<Cell<u32>>,
        running: Rc<Cell<bool>>,
    }

    impl Pane for FakePane {
        fn close(&self) {
            self.running.set(false);
        }
    }

    impl ShellPane for FakePane {
        fn running(&self) -> bool {
            self.running.get()
        }
        fn start(&self) {
            self.starts.set(self.starts.get() + 1);
            self.running.set(true);
        }
    }

    #[test]
    fn a_start_op_starts_a_shown_pane_once() {
        let terminals = Registry::new();
        let id = ModuleId::instance(ModuleKind::Terminal, 2).unwrap();
        let (pane, _) = terminals.take_or_make(&id, FakePane::default);
        assert_eq!(start_pane(&terminals, &id), Ok(true));
        assert_eq!(
            start_pane(&terminals, &id),
            Ok(false),
            "a second start while its shell runs starts nothing"
        );
        assert_eq!(pane.starts.get(), 1);
        // A killed pane's shell is gone: the next start is a fresh one.
        pane.close();
        assert_eq!(start_pane(&terminals, &id), Ok(true));
        assert_eq!(pane.starts.get(), 2);
        // An idle pane, retired with its instance, is no one's to start.
        terminals.retire(&id);
        assert!(start_pane(&terminals, &id).is_err());
        assert_eq!(pane.starts.get(), 2);
    }

    /// A pane that asks the registry from inside what the registry calls on it: the shell it starts, the hook its
    /// retirement runs, the constructor that builds it.
    #[derive(Clone)]
    struct Nosy {
        registry: Rc<RefCell<Option<std::rc::Weak<Registry<Nosy>>>>>,
        seen: Rc<RefCell<Vec<String>>>,
    }

    impl Nosy {
        /// Reads and writes the registry, as a signal GTK emits from inside a call might.
        fn ask(&self, from: &str) {
            let registry = self.registry.borrow().as_ref().and_then(std::rc::Weak::upgrade);
            if let Some(registry) = registry {
                let live = registry.live().len();
                registry.revive(&ModuleId::terminal());
                self.seen.borrow_mut().push(format!("{from}: {live} live"));
            }
        }
    }

    impl Pane for Nosy {
        fn close(&self) {
            self.ask("retire");
        }
    }

    impl ShellPane for Nosy {
        fn running(&self) -> bool {
            false
        }
        fn start(&self) {
            self.ask("start");
        }
    }

    #[test]
    fn a_registry_hands_out_clones() {
        let registry = Rc::new(Registry::<Nosy>::new());
        let slot = Rc::new(RefCell::new(Some(Rc::downgrade(&registry))));
        let seen: Rc<RefCell<Vec<String>>> = Rc::default();
        let nosy = Nosy {
            registry: slot,
            seen: seen.clone(),
        };
        let id = ModuleId::instance(ModuleKind::Terminal, 2).unwrap();
        let (made, reused) = registry.take_or_make(&id, || {
            nosy.ask("make");
            nosy.clone()
        });
        assert_eq!(reused, Reused::No);
        let (again, reused) = registry.take_or_make(&id, || panic!("the live pane is reused"));
        assert_eq!(reused, Reused::Yes);
        assert!(Rc::ptr_eq(&made.seen, &again.seen), "the same pane, handed out again");
        assert_eq!(start_pane(&registry, &id), Ok(true));
        assert!(registry.retire(&id));
        assert!(
            !registry.holds(&id) && registry.get(&id).is_none(),
            "retired, it is idle"
        );
        assert_eq!(registry.each().len(), 1, "an idle pane is still held");
        assert_eq!(
            *seen.borrow(),
            vec!["make: 0 live", "start: 1 live", "retire: 0 live"],
            "every call made into a pane found the registry free"
        );
        assert!(
            registry.revive(&id).is_some(),
            "the idle pane comes back under its own id"
        );
        assert_eq!(registry.live().len(), 1);
    }
}
