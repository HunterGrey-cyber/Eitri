//! The window's plugin pages. Each loaded plugin becomes a Lua panel module `lua:<id>` whose
//! content is the plugin's page, with its host pumped from a GLib watch on a wake its threads raise, plus a
//! one-shot timer only when the host asks for one: a plugin with nothing running costs no wake-up. Kill,
//! window close, the layout and the window's editor are connected here.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use eitri_core::editor_rpc::EditorRpc;
use eitri_core::keymap::Keymap;
use eitri_core::layout::{ModuleId, OpenRule};
use eitri_core::lua::panel::PanelSlot;
use eitri_core::lua::plugin::PluginRegistration;
use eitri_core::plugin::doc_route::DocSlot;
use eitri_core::plugin::follow_feed::{FollowBatch, FollowCmd, Ordered};
use eitri_core::plugin::host::{
    AgentsDone, AgentsFeed, AgentsPort, AgentsRefusal, AgentsServices, GestureClock, LayoutPort, PagePort, PluginHost,
    SharedWrite, TerminalAtAnswer, WaitShow,
};
use eitri_core::plugin::keys::EffectiveRow;
use eitri_core::plugin::kit_settings::KeysSettings;
use eitri_core::plugin::load::LoadedPlugin;
use eitri_core::plugin::manifest::{Capability, ShownWhile};
use eitri_core::plugin::page_policy;
use eitri_core::plugin::protocol::FocusTarget;
use eitri_core::plugin::state::PluginStore;
use eitri_core::plugin::tasks::{RunsWaiter, GRACE};
use eitri_core::plugin::theme::PageTheme;
use eitri_core::theme::ThemeTokens;
use eitri_panel::panel_wake::PanelWake;
use gtk4::glib;
use gtk4::prelude::*;

use crate::lua::{install_crash_recovery_into, LuaEngine, PanelEntry};
use crate::plugin_page::{self, PageHooks, PageSpec, WebKitPluginPage};
use crate::webview_crash_guard::WebViewCrashGuard;

/// Resolves the bounded save question; a page may decline state while its guarded tasks still move.
fn saved_state(outcome: eitri_core::plugin::host::SaveOutcome) -> Option<serde_json::Value> {
    use eitri_core::plugin::host::SaveOutcome;
    match outcome {
        SaveOutcome::Waiting => None,
        SaveOutcome::Saved(state) => Some(state),
        SaveOutcome::TimedOut | SaveOutcome::NotAsked | SaveOutcome::NotHandoff => Some(serde_json::Value::Null),
    }
}

/// A probe build's trace line about the page of the plugin module `module`.
fn trace_page(module: &ModuleId, event: &str, detail: &str) {
    let id = module.as_str().strip_prefix("lua:").unwrap_or(module.as_str());
    eitri_core::plugin::trace::line(id, event, detail);
}

fn trace_uri(uri: Option<&str>) -> &str {
    uri.filter(|u| !u.is_empty()).unwrap_or("none")
}

struct FrozenRuns {
    runner: eitri_core::plugin::tasks::TaskRunner,
    paused: Vec<eitri_core::plugin::tasks::TaskHandoff>,
    records: serde_json::Value,
    fds: Vec<std::os::fd::OwnedFd>,
}

/// Pausing readers, duplicating descriptors and serialising their held output all run on a worker.
fn freeze_runs(
    mut runner: eitri_core::plugin::tasks::TaskRunner,
) -> Result<FrozenRuns, (eitri_core::plugin::tasks::TaskRunner, String)> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let mut paused = Vec::new();
    let mut records = Vec::new();
    let mut fds = Vec::new();
    let result = (|| -> Result<(), String> {
        for run in runner.runs() {
            let handoff = runner
                .pause_for_move(run.run)
                .map_err(|e| format!("task {}: {e:?}", run.run))?;
            paused.push(handoff);
            let task = paused.last().unwrap();
            records.push(serde_json::to_value(&task.record).map_err(|e| e.to_string())?);
            for fd in task
                .fds
                .stdout
                .iter()
                .chain(task.fds.stderr.iter())
                .chain([&task.fds.status, &task.fds.lifetime])
            {
                // The outgoing duplicates keep the task alive while the source retains its resume copy.
                // SAFETY: fcntl only duplicates the live borrowed descriptor, with close-on-exec set atomically.
                let copy = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
                if copy < 0 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                // SAFETY: a successful duplication returned a new descriptor owned only by this handoff.
                fds.push(unsafe { OwnedFd::from_raw_fd(copy) });
            }
        }
        Ok(())
    })();
    if let Err(why) = result {
        for task in paused {
            if let Err(e) = runner.adopt(task) {
                return Err((runner, format!("{why}; resume: {:?}", e.why)));
            }
        }
        return Err((runner, why));
    }
    Ok(FrozenRuns {
        runner,
        paused,
        records: serde_json::Value::Array(records),
        fds,
    })
}

/// The record's stream flags define the descriptor order; missing or surplus descriptors refuse the whole record.
fn unpack_runs(
    records: &serde_json::Value,
    fds: Vec<std::os::fd::OwnedFd>,
) -> Result<Vec<eitri_core::plugin::tasks::TaskHandoff>, String> {
    use eitri_core::plugin::tasks::{MovedRun, MovedRunFds, TaskHandoff};
    let records: Vec<MovedRun> = serde_json::from_value(records.clone()).map_err(|e| e.to_string())?;
    let expected: usize = records
        .iter()
        .map(|r| 2 + usize::from(r.stdout.is_some()) + usize::from(r.stderr.is_some()))
        .sum();
    if fds.len() != expected {
        return Err(format!("task descriptors: got {}, expected {expected}", fds.len()));
    }
    let mut fds = fds.into_iter();
    Ok(records
        .into_iter()
        .map(|record| {
            let stdout = record.stdout.as_ref().map(|_| fds.next().unwrap());
            let stderr = record.stderr.as_ref().map(|_| fds.next().unwrap());
            TaskHandoff {
                record,
                fds: MovedRunFds {
                    stdout,
                    stderr,
                    status: fds.next().unwrap(),
                    lifetime: fds.next().unwrap(),
                },
            }
        })
        .collect())
}

type RunsAnswer = Result<FrozenRuns, (eitri_core::plugin::tasks::TaskRunner, String)>;
struct FreezeAsk {
    win: eitri_core::wm::WinId,
    module: ModuleId,
    mv: eitri_core::window_link::moves::MoveId,
    answer: std::sync::mpsc::Receiver<RunsAnswer>,
    frozen: Option<FrozenRuns>,
    state: Option<serde_json::Value>,
    cancelled: bool,
    deadline: Instant,
}

/// Runs one host's `pump` when its wake is raised and when the host asked for a time.
struct Pump {
    host: Rc<PluginHost>,
    wake: PanelWake,
    timer: RefCell<Option<glib::SourceId>>,
    me: Weak<Pump>,
    saving: RefCell<Option<FreezeAsk>>,
    paused: RefCell<Vec<eitri_core::plugin::tasks::TaskHandoff>>,
    restoring: RefCell<Option<std::sync::mpsc::Receiver<Result<eitri_core::plugin::tasks::TaskRunner, String>>>>,
    after_restore: RefCell<Option<Box<dyn FnOnce()>>>,
    events: RefCell<Option<Rc<dyn Fn(eitri_core::wm::HostEvent)>>>,
    adopting: Cell<bool>,
    /// The page's view while it has one, whose frames a ready waits for.
    view: Rc<RefCell<Option<webkit6::WebView>>>,
}

/// How many of its view's frames a page's `page.ready` waits for before it is posted.
const READY_FRAMES: u32 = 3;

impl Pump {
    fn start(host: Rc<PluginHost>, wake: PanelWake, view: Rc<RefCell<Option<webkit6::WebView>>>) -> Rc<Pump> {
        let pump = Rc::new_cyclic(|me| Pump {
            host,
            wake,
            timer: RefCell::new(None),
            me: me.clone(),
            saving: RefCell::default(),
            paused: RefCell::default(),
            restoring: RefCell::default(),
            after_restore: RefCell::default(),
            events: RefCell::default(),
            adopting: Cell::new(false),
            view,
        });
        let watched = {
            let pump = pump.clone();
            neovide_editor::fd_watch::add_local(
                pump.wake.fd(),
                glib::IOCondition::IN | glib::IOCondition::ERR | glib::IOCondition::HUP,
                glib::ffi::G_PRIORITY_DEFAULT,
                move |_| {
                    pump.wake.take();
                    pump.kick();
                    glib::ControlFlow::Continue
                },
            )
        };
        if watched.is_err() {
            eprintln!("[plugin] its wake cannot be watched on this thread; output arrives with the page's next call");
        }
        pump
    }

    fn kick(&self) {
        let now = Instant::now();
        let moving = self.moving(now);
        let next = self.host.pump(now);
        if self.host.take_ready() {
            self.post_ready();
        }
        self.arm(next.into_iter().chain(moving).min());
    }

    fn committed(&self) {
        // The returned runner stays frozen until the document reloaded after adoption commits.
        if self.restoring.borrow().is_none() && self.adopting.replace(false) {
            self.host.resume_moved();
        }
        self.host.page_committed();
    }

    /// Posts the page's `page.ready` once its view has drawn a few more frames. A page says it from an animation
    /// frame callback, which WebKit runs before it paints that frame; the painted frame then reaches the view
    /// asynchronously and is shown at a later snapshot. Revealed at once, the view would show the picture from before
    /// the page was ready for one frame. A view that is not mapped draws no frames, and its ready is posted at once.
    /// The ready is about the document that said it, which a commit, an arrival, a reload after a crash or the page
    /// leaving may replace within those frames; the ready of a document that is gone is dropped, or the controller
    /// would reveal a document that never said it was ready.
    fn post_ready(&self) {
        let Some(document) = self.host.hello_document() else {
            return;
        };
        let view = self.view.borrow().clone();
        let Some(view) = view.filter(|view| view.is_mapped()) else {
            self.post_ready_of(document);
            return;
        };
        let me = self.me.clone();
        let left = Cell::new(READY_FRAMES);
        view.add_tick_callback(move |_, _| {
            left.set(left.get() - 1);
            if left.get() > 0 {
                return glib::ControlFlow::Continue;
            }
            if let Some(pump) = me.upgrade() {
                pump.post_ready_of(document);
            }
            glib::ControlFlow::Break
        });
    }

    /// Posts the `page.ready` that `document` said, if it is still the document running; drops it otherwise.
    fn post_ready_of(&self, document: u64) {
        let id = &self.host.plugin().manifest.id;
        if self.host.hello_document() != Some(document) {
            eitri_core::plugin::trace::line(id, "ready dropped", "its document was replaced");
            return;
        }
        eitri_core::plugin::trace::line(id, "ready posted", "");
        self.post(eitri_core::wm::PluginEvent::Ready {
            win: eitri_core::wm::WinId::MAIN,
            module: ModuleId::lua(id),
        });
    }

    fn freeze(
        &self,
        win: eitri_core::wm::WinId,
        module: ModuleId,
        mv: eitri_core::window_link::moves::MoveId,
    ) -> Result<(), String> {
        if self.saving.borrow().is_some() || !self.paused.borrow().is_empty() || self.restoring.borrow().is_some() {
            return Err("the page already has a move in flight".into());
        }
        let runner = self.host.take_move_runner()?;
        let deadline = Instant::now() + Duration::from_secs(1);
        self.host.request_save(deadline);
        let (tx, rx) = std::sync::mpsc::channel();
        let wake = self.wake.waker();
        std::thread::spawn(move || {
            let _ = tx.send(freeze_runs(runner));
            wake();
        });
        *self.saving.borrow_mut() = Some(FreezeAsk {
            win,
            module,
            mv,
            answer: rx,
            frozen: None,
            state: None,
            cancelled: false,
            deadline,
        });
        self.kick();
        Ok(())
    }

    fn resume(&self) {
        if let Some(ask) = self.saving.borrow_mut().as_mut() {
            ask.cancelled = true;
            return;
        }
        let tasks = std::mem::take(&mut *self.paused.borrow_mut());
        if tasks.is_empty() {
            if self.restoring.borrow().is_none() {
                self.host.resume_moved();
            }
            return;
        }
        self.restore_tasks(tasks);
    }

    fn restore_tasks(&self, tasks: Vec<eitri_core::plugin::tasks::TaskHandoff>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let wake = self.wake.waker();
        *self.restoring.borrow_mut() = Some(rx);
        std::thread::spawn(move || {
            let mut runner = eitri_core::plugin::tasks::TaskRunner::new(wake.clone());
            let result = tasks.into_iter().try_for_each(|task| {
                runner
                    .adopt(task)
                    .map(drop)
                    .map_err(|e| format!("task adoption failed: {:?}", e.why))
            });
            let _ = tx.send(result.map(|()| runner));
            wake();
        });
    }

    fn moving(&self, now: Instant) -> Option<Instant> {
        let restored = self.restoring.borrow().as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some(result) = restored {
            self.restoring.borrow_mut().take();
            let hook = self.after_restore.borrow_mut().take();
            match result {
                Ok(runner) => {
                    self.host.return_move_runner(runner, hook.is_some());
                    if let Some(hook) = hook {
                        hook();
                    }
                }
                Err(why) => {
                    self.adopting.set(false);
                    let events = self.events.borrow().clone();
                    if let Some(events) = events {
                        events(eitri_core::wm::HostEvent::Refused {
                            what: eitri_core::wm::OpTag::AdoptMoved,
                            why,
                            surface: Some((
                                eitri_core::wm::WinId::MAIN,
                                ModuleId::lua(&self.host.plugin().manifest.id),
                            )),
                        });
                    }
                }
            }
        }
        let mut ask = self.saving.borrow_mut().take()?;
        if ask.state.is_none() {
            ask.state = saved_state(self.host.take_saved(now));
        }
        if ask.frozen.is_none() {
            match ask.answer.try_recv() {
                Ok(Ok(frozen)) => ask.frozen = Some(frozen),
                Ok(Err((runner, why))) => {
                    self.host.return_move_runner(runner, false);
                    self.post(eitri_core::wm::PluginEvent::FreezeRefused {
                        win: ask.win,
                        module: ask.module,
                        mv: ask.mv,
                        why,
                    });
                    return None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.post(eitri_core::wm::PluginEvent::FreezeRefused {
                        win: ask.win,
                        module: ask.module,
                        mv: ask.mv,
                        why: "the task move worker ended".into(),
                    });
                    return None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        if ask.frozen.is_some() && (ask.state.is_some() || ask.cancelled) {
            let frozen = ask.frozen.take().unwrap();
            self.host.return_move_runner(frozen.runner, true);
            if ask.cancelled {
                self.restore_tasks(frozen.paused);
            } else {
                *self.paused.borrow_mut() = frozen.paused;
                self.post(eitri_core::wm::PluginEvent::Frozen {
                    win: ask.win,
                    module: ask.module,
                    mv: ask.mv,
                    record: serde_json::json!({ "built": true, "state": ask.state.unwrap(), "tasks": frozen.records }),
                    handoff: crate::gtk_host::peer::handoff_of(frozen.fds),
                });
            }
            return None;
        }
        let next = (now < ask.deadline).then_some(ask.deadline);
        *self.saving.borrow_mut() = Some(ask);
        next
    }

    fn post(&self, event: eitri_core::wm::PluginEvent) {
        let hook = self.events.borrow().clone();
        if let Some(hook) = hook {
            hook(eitri_core::wm::HostEvent::Plugin(event));
        }
    }

    /// Replaces the pending timer with one for `next`, rounded up to a whole millisecond (GLib counts in them).
    fn arm(&self, next: Option<Instant>) {
        let old = self.timer.borrow_mut().take();
        if let Some(old) = old {
            old.remove();
        }
        let (Some(at), Some(me)) = (next, self.me.upgrade()) else {
            return;
        };
        let nanos = at.saturating_duration_since(Instant::now()).as_nanos();
        let wait = Duration::from_millis(u64::try_from(nanos.div_ceil(1_000_000)).unwrap_or(u64::MAX));
        let id = glib::timeout_add_local_once(wait, move || {
            // Fired: its id is spent and must not be removed again.
            me.timer.borrow_mut().take();
            me.kick();
        });
        *self.timer.borrow_mut() = Some(id);
    }
}

/// The user's download directory (XDG's), else `~/Downloads`, else the current directory.
fn downloads_dir() -> std::path::PathBuf {
    glib::user_special_dir(glib::UserDirectory::Downloads)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join("Downloads")))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// Shows a module if it is hidden, then gives it the keys.
pub(crate) type ShowAndFocus = Rc<dyn Fn(&ModuleId) -> Result<(), String>>;

/// Asks for the window's base terminal in a directory on behalf of a plugin (its id), under the host's ticket.
type TerminalAt = Rc<dyn Fn(&str, PathBuf, u64) -> Result<(), String>>;

/// The window's layout as a plugin sees it: the editor, or its own module, each shown first if hidden; and the
/// window's base terminal.
struct WindowLayout {
    own: ModuleId,
    plugin: String,
    show_and_focus: ShowAndFocus,
    terminal_at: TerminalAt,
}

impl LayoutPort for WindowLayout {
    fn focus(&self, target: FocusTarget) -> Result<(), String> {
        match target {
            FocusTarget::Editor => (self.show_and_focus)(&ModuleId::editor()),
            FocusTarget::Page => (self.show_and_focus)(&self.own),
        }
    }

    fn terminal_at(&self, dir: PathBuf, ticket: u64) -> Result<(), String> {
        (self.terminal_at)(&self.plugin, dir, ticket)
    }
}

/// The window as the agents verbs of its plugin pages reach it: its own tabs through its agents host (when the window
/// shows chats and the layer started), any other window through that window's control socket, and the shared files
/// of the control directory. Every request returns at once; a socket or a file is worked on a thread, and `done` is
/// called when it finished. It brings a tab forward, opens an empty one or writes the shared selection or round:
/// nothing here answers anything in a chat.
pub(crate) struct WindowAgents {
    control_dir: PathBuf,
    host: RefCell<Option<Rc<crate::agents_host::AgentsHost>>>,
    /// An activation token from this window's display, for a request to another window.
    token: Rc<dyn Fn() -> Option<String>>,
    /// The window's address when it has no host of its own (a page window): a round started here names it.
    here: String,
    /// Gives a module of this window the keys, as a page's `layout.focus` does.
    show_and_focus: ShowAndFocus,
}

impl WindowAgents {
    pub(crate) fn new(
        control_dir: PathBuf,
        token: Rc<dyn Fn() -> Option<String>>,
        show_and_focus: ShowAndFocus,
    ) -> WindowAgents {
        WindowAgents {
            control_dir,
            host: RefCell::new(None),
            token,
            here: eitri_core::agents::new_instance(),
            show_and_focus,
        }
    }

    /// This window's own agents host, once it started; a window without one sends every request to a socket.
    pub(crate) fn set_host(&self, host: Option<Rc<crate::agents_host::AgentsHost>>) {
        *self.host.borrow_mut() = host;
    }

    fn own(&self) -> Option<Rc<crate::agents_host::AgentsHost>> {
        self.host.borrow().clone()
    }

    /// `req` to the window `instance`: through this window's host when it has one, else straight to the socket.
    fn send(&self, instance: &str, req: eitri_core::agents::control::Request, done: AgentsDone) {
        let answered = move |reply| done(reply_result(reply));
        match self.own() {
            Some(host) => host.focus_remote(instance, req, answered),
            None => crate::agents_host::send_to(&self.control_dir, &*self.token, instance, req, answered),
        }
    }
}

/// What a control socket's reply is to a page.
fn reply_result(reply: eitri_core::agents::control::Reply) -> Result<(), AgentsRefusal> {
    match reply {
        eitri_core::agents::control::Reply::Ok => Ok(()),
        eitri_core::agents::control::Reply::Refused(words) => Err(AgentsRefusal::of_words(&words)),
    }
}

impl AgentsPort for WindowAgents {
    fn own_instance(&self) -> Option<String> {
        self.own().map(|host| host.instance().to_owned())
    }

    fn origin(&self) -> String {
        self.own_instance().unwrap_or_else(|| self.here.clone())
    }

    fn is_here(&self, instance: &str) -> bool {
        instance == self.here || self.own().is_some_and(|host| host.instance() == instance)
    }

    fn focus_module(&self, module: &str, done: AgentsDone) {
        let shown = match ModuleId::parse(module) {
            Ok(module) => (self.show_and_focus)(&module).map_err(AgentsRefusal::Refused),
            Err(_) => Err(AgentsRefusal::Gone),
        };
        done(shown);
    }

    fn focus(
        &self,
        instance: &str,
        tab: Option<u64>,
        card_first: bool,
        intent: eitri_core::agents::Intent,
        done: AgentsDone,
    ) {
        if let Some(host) = self.own().filter(|host| host.instance() == instance) {
            let req = eitri_core::wm::RevealRequest {
                tab: tab.map(eitri_core::tabs::TabId),
                card_first,
                intent,
            };
            return done(reply_result(host.focus_local(req)));
        }
        let req = eitri_core::agents::control::Request::Focus {
            v: eitri_core::agents::control::PROTOCOL_V,
            tab,
            card_first,
            intent,
            token: None,
        };
        self.send(instance, req, done);
    }

    fn new_in(&self, project_key: &str, done: AgentsDone) {
        // Every running window of the project, the hub that holds no tab included, read from the files on a
        // worker; the target is then asked from this thread.
        let control_dir = self.control_dir.clone();
        let project_key = project_key.to_owned();
        let host = self.own();
        let token = self.token.clone();
        glib::spawn_future_local(async move {
            let dir = control_dir.clone();
            let scanned =
                gtk4::gio::spawn_blocking(move || eitri_core::agents::scan_windows(&dir, eitri_core::agents::SCHEMA_V))
                    .await;
            let windows = match scanned {
                Ok(Ok(windows)) => windows,
                Ok(Err(e)) => {
                    return done(Err(AgentsRefusal::Refused(format!(
                        "the agents directory could not be read: {e}"
                    ))))
                }
                Err(_) => return done(Err(AgentsRefusal::Refused("the agents directory was not read".into()))),
            };
            let Some(instance) = crate::agents_host::new_target(&windows, &project_key).map(str::to_owned) else {
                return done(Err(AgentsRefusal::NoWindow));
            };
            if let Some(host) = host.as_ref().filter(|host| host.instance() == instance) {
                return done(reply_result(host.new_local()));
            }
            let req = eitri_core::agents::control::Request::New {
                v: eitri_core::agents::control::PROTOCOL_V,
                token: None,
            };
            let answered = move |reply| done(reply_result(reply));
            match host {
                Some(host) => host.focus_remote(&instance, req, answered),
                None => crate::agents_host::send_to(&control_dir, &*token, &instance, req, answered),
            }
        });
    }

    fn write_selection(&self, selection: eitri_core::agents::shared::Selection, done: AgentsDone) {
        eitri_core::plugin::host::write_shared_on_thread(
            self.control_dir.clone(),
            SharedWrite::Selection(selection),
            done,
        );
    }

    fn write_round(&self, round: Option<eitri_core::agents::shared::Round>, done: AgentsDone) {
        eitri_core::plugin::host::write_shared_on_thread(self.control_dir.clone(), SharedWrite::Round(round), done);
    }
}

/// The page port of a plugin whose page is not built yet: what the host sends before then is dropped (the page
/// that is built later says hello and gets the current state), and from the build on it is the page's own port.
#[derive(Default)]
struct LazyPort(RefCell<Option<Rc<dyn PagePort>>>);

impl LazyPort {
    /// Cloned out, so a page that answers inside a send finds the port free.
    fn page(&self) -> Option<Rc<dyn PagePort>> {
        self.0.borrow().clone()
    }
}

impl PagePort for LazyPort {
    fn send(&self, message: &str) {
        if let Some(page) = self.page() {
            page.send(message);
        }
    }

    fn set_theme(&self, theme: &PageTheme) {
        if let Some(page) = self.page() {
            page.set_theme(theme);
        }
    }
}

/// Builds a new view of the plugin's page into its module's slot and returns the page's port; `None` when WebKit
/// cannot start here. Runs whenever the module has no page: its first show, and each time it comes back to this
/// window after its view was dropped here.
type BuildPage = Rc<dyn Fn(&PageTheme) -> Option<Rc<dyn PagePort>>>;

/// Navigates the existing view to an entry, or reloads its current address when none is given.
type LoadPage = Rc<dyn Fn(Option<&str>) -> bool>;

/// One plugin's module: its host (none where WebKit cannot start), the port its host sends through, and the page
/// build, which runs whenever the module has no page here.
pub(crate) struct Panel {
    module: ModuleId,
    pump: Option<Rc<Pump>>,
    port: Rc<LazyPort>,
    build: Option<BuildPage>,
    /// The page's entry, which a page loaded again from its store navigates to.
    url: String,
    hidden_start: bool,
    away: Cell<bool>,
    /// A build or reactivation since departure already read the store, so the return must not load it again.
    fresh_page: Cell<bool>,
    load: LoadPage,
    own_rule: Option<OpenRule>,
    /// Filled by the build.
    webview: Rc<RefCell<Option<webkit6::WebView>>>,
    /// Whether the host was last told its module is drawn; `None` until it is first told. A layout change that
    /// leaves it where it was tells the host nothing, so a divider drag wakes no plugin.
    on_screen: Cell<Option<bool>>,
    /// For a plugin that declares `agents`: when its view last had a key or button press, recorded by every view
    /// built for it and read by its host's agents verbs.
    gesture: Option<Rc<GestureClock>>,
}

/// One plugin registration, kept independently of the Lua registry's borrow.
struct Registered {
    plugin: Rc<LoadedPlugin>,
    key: Option<String>,
    slot: PanelSlot,
    hidden_start: bool,
    own_rule: Option<OpenRule>,
}

impl Registered {
    fn of(registration: &PluginRegistration) -> Registered {
        Registered {
            plugin: registration.plugin.clone(),
            key: registration.key.clone(),
            slot: registration.slot,
            hidden_start: registration.starts_hidden(),
            own_rule: registration.own_rule(),
        }
    }
}

impl Panel {
    /// The panel of `registered`, whose start and open rule it answers with.
    fn new(
        registered: &Registered,
        pump: Option<Rc<Pump>>,
        port: Rc<LazyPort>,
        build: Option<BuildPage>,
        webview: Rc<RefCell<Option<webkit6::WebView>>>,
    ) -> Panel {
        let manifest = &registered.plugin.manifest;
        let load: LoadPage = {
            let webview = webview.clone();
            Rc::new(move |entry| {
                use webkit6::prelude::*;
                let Some(view) = webview.borrow().clone() else {
                    return false;
                };
                let uri = entry
                    .map(str::to_owned)
                    .or_else(|| view.uri().map(|uri| uri.to_string()));
                let Some(uri) = uri else { return false };
                view.load_uri(&uri);
                true
            })
        };
        Panel {
            module: ModuleId::lua(&manifest.id),
            pump,
            port,
            build,
            url: page_policy::entry_uri(&manifest.id, &manifest.entry),
            hidden_start: registered.hidden_start,
            away: Cell::new(false),
            fresh_page: Cell::new(false),
            load,
            own_rule: registered.own_rule.clone(),
            webview,
            on_screen: Cell::new(None),
            gesture: None,
        }
    }
}

/// The place every plugin page holds in the grid, whose one child is the page's current view, if any: empty until
/// the first build of a plugin that starts hidden, and again while the page is in the other window. The child is
/// laid out at its own origin as a web module's host lays out its view, and a grab of the keys is passed to it,
/// since the module's focus target is this widget and GTK refuses a grab on a widget that is not focusable itself.
mod page_slot {
    use gtk4::glib;
    use gtk4::prelude::*;
    use gtk4::subclass::prelude::*;

    mod imp {
        use super::*;

        #[derive(Default)]
        pub struct PageSlot;

        #[glib::object_subclass]
        impl ObjectSubclass for PageSlot {
            const NAME: &'static str = "EitriPluginPageSlot";
            type Type = super::PageSlot;
            type ParentType = gtk4::Widget;

            fn class_init(klass: &mut Self::Class) {
                klass.set_layout_manager_type::<gtk4::BinLayout>();
            }
        }

        impl ObjectImpl for PageSlot {
            fn dispose(&self) {
                while let Some(child) = self.obj().first_child() {
                    child.unparent();
                }
            }
        }

        impl WidgetImpl for PageSlot {
            fn grab_focus(&self) -> bool {
                self.obj().first_child().is_some_and(|child| child.grab_focus())
            }
        }
    }

    glib::wrapper! {
        pub struct PageSlot(ObjectSubclass<imp::PageSlot>)
            @extends gtk4::Widget,
            @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
    }

    impl PageSlot {
        pub(crate) fn new() -> PageSlot {
            let slot: PageSlot = glib::Object::new();
            slot.set_hexpand(true);
            slot.set_vexpand(true);
            slot
        }

        /// The page, filling the slot from its origin: a margin or an alignment would move the view inside its
        /// parent, which WebKit adds to the caret rectangle it gives the input method.
        pub(crate) fn set_page(&self, page: &impl IsA<gtk4::Widget>) {
            let page = page.as_ref();
            page.set_margin_start(0);
            page.set_margin_end(0);
            page.set_margin_top(0);
            page.set_margin_bottom(0);
            page.set_halign(gtk4::Align::Fill);
            page.set_valign(gtk4::Align::Fill);
            page.set_hexpand(true);
            page.set_vexpand(true);
            page.set_parent(self);
        }
    }
}

/// What the window does with a plugin's nvim commands; `main` implements it over the grid, a test records.
pub(crate) trait FollowWindow {
    fn is_shown(&self, module: &ModuleId) -> bool;
    /// Shown and drawn: not behind a zoom, not on a tab its container does not draw.
    fn is_on_screen(&self, module: &ModuleId) -> bool;
    /// Opens or shows the module with the keys where they are (its page built the first time), and brings it on
    /// screen as far as that leaves the module with the keys drawn.
    fn show(&self, module: &ModuleId);
    /// Hides it; if it held the keys they go where a hide sends them.
    fn hide(&self, module: &ModuleId);
    /// Gives the controller the command as nvim sent it, sorted by nothing here: what a window beside nvim is given,
    /// since only the controller knows what each command means there. Group windows use
    /// [`apply_follow_commands`] instead.
    fn command(&self, module: &ModuleId, cmd: FollowCmd);
}

/// Applies the commands in order, each reading the window at its own turn, so several in one batch end where as
/// many key presses would. `Show` brings a module that is not on screen there (a hidden one, or one behind a zoom
/// or another tab) and does nothing to one on screen; `Toggle` hides one on screen and otherwise shows it, so the
/// command never hides a preview the reader cannot see; `Hide` hides a shown one wherever it is. An `Auto` that
/// reached here shows only a hidden module: it fires on every new buffer, and must not end a zoom or switch a tab
/// the user chose.
pub(crate) fn apply_follow_commands(window: &dyn FollowWindow, commands: Vec<(ModuleId, FollowCmd)>) {
    for (module, cmd) in commands {
        match cmd {
            FollowCmd::Show | FollowCmd::Toggle if !window.is_on_screen(&module) => window.show(&module),
            FollowCmd::Toggle | FollowCmd::Hide if window.is_shown(&module) => window.hide(&module),
            FollowCmd::Auto if !window.is_shown(&module) => window.show(&module),
            FollowCmd::Show | FollowCmd::Toggle | FollowCmd::Hide | FollowCmd::Auto => {}
        }
    }
}

/// One module shown while agents wait: its watch over the feed, and the one timer that pumps it again when a running
/// round on screen will have stood idle too long (an expiry changes no file, so no reader event comes).
struct WaitFollow {
    show: WaitShow,
    panels: std::rc::Weak<PluginPanels>,
    window: Rc<dyn FollowWindow>,
    module: ModuleId,
    recheck: RefCell<Option<glib::SourceId>>,
}

impl WaitFollow {
    fn pump(this: &Rc<WaitFollow>) {
        let now_ms = wall_clock_ms();
        if let Some(want) = this.show.pump_at(now_ms) {
            let here = this
                .panels
                .upgrade()
                .is_some_and(|panels| panels.panel(&this.module).is_some_and(|panel| !panel.away.get()));
            if here {
                apply_wait_show(&*this.window, &this.module, want);
            }
        }
        if let Some(old) = this.recheck.borrow_mut().take() {
            old.remove();
        }
        if let Some(after) = this.show.recheck_in(now_ms) {
            let weak = Rc::downgrade(this);
            let id = glib::timeout_add_local_once(after + Duration::from_millis(1), move || {
                if let Some(this) = weak.upgrade() {
                    this.recheck.borrow_mut().take();
                    WaitFollow::pump(&this);
                }
            });
            *this.recheck.borrow_mut() = Some(id);
        }
    }
}

fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A module shown while agents wait, told whether anything waits: a hidden one is shown with the keys where they
/// are, and one shown is hidden wherever it is. One shown but off screen (behind a zoom, on a tab not drawn) is
/// left there: a card arriving must not end the person's zoom or switch their tab.
pub(crate) fn apply_wait_show(window: &dyn FollowWindow, module: &ModuleId, want: bool) {
    match (want, window.is_shown(module)) {
        (true, false) => window.show(module),
        (false, true) => window.hide(module),
        _ => {}
    }
}

/// How many follow lines the window has read, for the probe build's trace.
#[derive(Default)]
struct FollowTotals {
    views: u64,
    cmds: u64,
    /// The totals the last trace line printed, and when.
    printed: Option<(u64, u64, Instant)>,
}

impl FollowTotals {
    /// Adds a batch; the line to print when the totals moved since the last one and that was a second ago or more.
    fn add(&mut self, views: u64, cmds: u64, now: Instant) -> Option<String> {
        self.views += views;
        self.cmds += cmds;
        if let Some((views, cmds, at)) = self.printed {
            let moved = (views, cmds) != (self.views, self.cmds);
            if !moved || now.saturating_duration_since(at) < Duration::from_secs(1) {
                return None;
            }
        } else if self.views == 0 && self.cmds == 0 {
            return None;
        }
        self.printed = Some((self.views, self.cmds, now));
        Some(format!("[follow] trace: views={} cmds={}", self.views, self.cmds))
    }
}

type MoveEvents = Rc<dyn Fn(eitri_core::wm::HostEvent)>;
type HolderHook = Rc<dyn Fn(&str, bool)>;
type GridTop = Rc<dyn Fn() -> Option<f64>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fact {
    MoveEvents,
    LinkEditor,
    WindowRoot,
    OwnWindow,
    ScratchPrefix,
    FollowRefused,
    Editor,
    GridTop,
    Layout,
    Agents,
    WaitWindow,
    Timeoutlen,
    PageBuilt,
    Holder,
    Theme,
    FollowFeed,
    EditorGone,
}

/// The latest window settings, in the order they were applied. Replacing a setting moves it to the end,
/// so a direct editor and an editor proxy retain the window's choice of which was installed last.
#[derive(Clone)]
struct WindowFacts {
    order: Vec<Fact>,
    move_events: Option<MoveEvents>,
    link_editor: Option<Rc<crate::linked_window::editor::EditorOps>>,
    window_root: Option<PathBuf>,
    own_window: Option<Option<String>>,
    scratch_prefix: Option<Option<PathBuf>>,
    follow_refused: Option<Rc<dyn Fn()>>,
    editor: Option<Rc<dyn EditorRpc>>,
    grid_top: Option<GridTop>,
    layout: Option<(ShowAndFocus, TerminalAt)>,
    agents: Option<Option<Rc<WindowAgents>>>,
    agents_feed: Option<Rc<AgentsFeed>>,
    wait_window: Option<(Weak<PluginPanels>, Rc<dyn FollowWindow>)>,
    timeoutlen: Option<u32>,
    page_built: Option<PageBuilt>,
    holder: Option<HolderHook>,
    theme: PageTheme,
    text_px: Option<f32>,
    follow_feed: Option<(PathBuf, Option<PathBuf>)>,
    editor_gone: bool,
}

impl WindowFacts {
    fn new(theme: PageTheme) -> Self {
        Self {
            order: vec![Fact::Theme],
            move_events: None,
            link_editor: None,
            window_root: None,
            own_window: None,
            scratch_prefix: None,
            follow_refused: None,
            editor: None,
            grid_top: None,
            layout: None,
            agents: None,
            agents_feed: None,
            wait_window: None,
            timeoutlen: None,
            page_built: None,
            holder: None,
            theme,
            text_px: None,
            follow_feed: None,
            editor_gone: false,
        }
    }
}

pub(crate) struct PluginPanels {
    panels: RefCell<Vec<Rc<Panel>>>,
    facts: RefCell<WindowFacts>,
    follow_totals: RefCell<FollowTotals>,
}

/// What [`PluginPanels::set_page_built`] calls with a module and its new view.
pub(crate) type PageBuilt = Rc<dyn Fn(&ModuleId, &webkit6::WebView)>;

impl PluginPanels {
    /// Registers every loaded plugin as a Lua panel entry, each host checking its page's keys against the
    /// window's prefix and applying the user's `plugin:<id>` overrides, and each page starting in `theme`.
    /// Hosts exist now; pages are built after the layout decision, with a hidden module's waiting for first show
    /// ([`PluginPanels::build_page`]); its entry's widget is the empty slot the page is built into.
    /// `Err` is a startup failure's text.
    pub(crate) fn install(engine: &LuaEngine, keymap: &Keymap, theme: &PageTheme) -> Result<PluginPanels, String> {
        let loaded: Vec<Registered> = engine.plugins.borrow().plugins().iter().map(Registered::of).collect();
        let mut panels = Vec::new();
        let keys_settings =
            eitri_core::plugin::kit_settings::from_config(|key| engine.config.borrow().get(key).map(str::to_owned))?;
        for registration in loaded {
            panels.push(Self::make_panel(engine, keymap, theme, &registration, &keys_settings)?);
        }
        Ok(PluginPanels {
            panels: RefCell::new(panels),
            facts: RefCell::new(WindowFacts::new(theme.clone())),
            follow_totals: RefCell::default(),
        })
    }

    fn make_panel(
        engine: &LuaEngine,
        keymap: &Keymap,
        theme: &PageTheme,
        registration: &Registered,
        keys_settings: &KeysSettings,
    ) -> Result<Rc<Panel>, String> {
        let store_root = PluginStore::eitri_root_from_env();
        let plugin = registration.plugin.clone();
        let key = registration.key.clone();
        let slot = registration.slot;
        let id = plugin.manifest.id.clone();
        let clash = engine.panels.borrow().entries().iter().any(|e| e.id == id);
        if clash {
            return Err(format!("eitri.plugin.load: plugin {id} has the id of a Lua panel"));
        }
        let url = page_policy::entry_uri(&id, &plugin.manifest.entry);
        let title = plugin.manifest.title.clone();
        let wake = if crate::webkit_sandbox::decision().allows_webviews() {
            PanelWake::new()
                .map_err(|e| eprintln!("[plugin {id}] cannot start: {e}"))
                .ok()
        } else {
            eprintln!("[plugin {id}] not loading its page: WebKit's sandbox cannot start here");
            None
        };
        let port = Rc::new(LazyPort::default());
        let gesture = plugin
            .manifest
            .has(Capability::Agents)
            .then(|| Rc::new(GestureClock::default()));
        let webview: Rc<RefCell<Option<webkit6::WebView>>> = Rc::default();
        let crash_guard = Rc::new(RefCell::new(WebViewCrashGuard::with_defaults()));
        let (widget, pump, build) = match wake {
            Some(wake) => {
                let cell: Rc<OnceCell<Rc<Pump>>> = Rc::new(OnceCell::new());
                let hooks = PageHooks {
                    on_message: {
                        let cell = cell.clone();
                        Rc::new(move |raw: &str| {
                            if let Some(pump) = cell.get() {
                                pump.host.page_message(raw, Instant::now());
                                pump.kick();
                            }
                        })
                    },
                    on_committed: {
                        let cell = cell.clone();
                        Rc::new(move || {
                            if let Some(pump) = cell.get() {
                                pump.committed();
                            }
                        })
                    },
                    open_outside: Rc::new(plugin_page::open_outside),
                };
                let doc_slot = plugin
                    .manifest
                    .has(Capability::EditorFollow)
                    .then(|| Rc::new(RefCell::new(DocSlot::default())));
                let spec = PageSpec {
                    id: id.clone(),
                    site_root: plugin.site_root.clone(),
                    entry: plugin.manifest.entry.clone(),
                    origins: plugin.manifest.origins.clone(),
                    downloads: downloads_dir(),
                    doc_slot: doc_slot.clone(),
                    remote_images: plugin.remote_images,
                    console_to_stdout: eitri_core::plugin::trace::enabled(),
                    agents_kit: plugin.manifest.has(Capability::Agents),
                };
                let store = store_root.clone().map(|root| PluginStore::new(root, &id));
                let overrides = keymap.plugin_user(&id).to_vec();
                let host = Rc::new(PluginHost::new(
                    plugin.clone(),
                    port.clone(),
                    wake.waker(),
                    store,
                    overrides,
                ));
                host.set_prefix_key(keymap.prefix());
                host.set_keys_settings(keys_settings.clone());
                // A view built now or later wears `theme`; the host records it so it sends nothing for the
                // same set.
                host.set_theme(theme);
                host.set_module_key(key.as_deref());
                // The host fills the slot with the followed document's project; the page's scheme handler
                // reads the same slot.
                if let Some(slot) = &doc_slot {
                    host.set_doc_slot(slot.clone());
                }
                let pump = Pump::start(host, wake, webview.clone());
                let _ = cell.set(pump.clone());
                // The grid holds the slot, never the view: a view is dropped when its page leaves for the
                // other window and a new one is built into the same slot when it comes back, so the module's
                // grid host stays as it is.
                let slot_widget = page_slot::PageSlot::new();
                let build: BuildPage = {
                    let id = id.clone();
                    let url = url.clone();
                    let crash_guard = crash_guard.clone();
                    let webview = webview.clone();
                    let slot_widget = slot_widget.clone();
                    let gesture = gesture.clone();
                    Rc::new(move |theme: &PageTheme| {
                        let view = plugin_page::build(&spec, hooks.clone(), theme);
                        if let Some(clock) = &gesture {
                            plugin_page::observe_gestures(&view, clock.clone());
                        }
                        slot_widget.set_page(&view);
                        install_crash_recovery_into(
                            &view,
                            id.clone(),
                            url.clone(),
                            Some(page_policy::host_page_base(&id)),
                            crash_guard.clone(),
                        );
                        let page: Rc<dyn PagePort> = Rc::new(WebKitPluginPage(view.downgrade()));
                        *webview.borrow_mut() = Some(view);
                        eitri_core::plugin::trace::line(&id, "page built", "");
                        Some(page)
                    })
                };
                // Page creation waits until the window's layout and ownership have been decided.
                (slot_widget.upcast::<gtk4::Widget>(), Some(pump), Some(build))
            }
            None => (crate::webkit_sandbox::lua_panel_placeholder(&title), None, None),
        };
        engine.panels.borrow_mut().register(PanelEntry {
            id,
            title,
            slot,
            key,
            url,
            widget,
            crash_guard,
        });
        let mut panel = Panel::new(registration, pump, port, build, webview);
        panel.gesture = gesture;
        Ok(Rc::new(panel))
    }

    /// Registers a page for this run. Its slot is placed by the caller after the window's facts reach its host.
    pub(crate) fn join(
        &self,
        engine: &LuaEngine,
        keymap: &Keymap,
        registration: &PluginRegistration,
    ) -> Result<Rc<Panel>, String> {
        let registration = Registered::of(registration);
        self.join_with(&registration, || {
            let keys_settings = eitri_core::plugin::kit_settings::from_config(|key| {
                engine.config.borrow().get(key).map(str::to_owned)
            })?;
            Self::make_panel(engine, keymap, &self.theme(), &registration, &keys_settings)
        })
    }

    /// The factory also lets display-free tests supply a host without constructing a GTK slot.
    fn join_with(
        &self,
        registration: &Registered,
        make: impl FnOnce() -> Result<Rc<Panel>, String>,
    ) -> Result<Rc<Panel>, String> {
        let module = ModuleId::lua(&registration.plugin.manifest.id);
        if self.panel(&module).is_some() {
            return Err(format!("plugin {} is already loaded", registration.plugin.manifest.id));
        }
        let panel = make()?;
        let facts = self.facts.borrow().clone();
        for fact in &facts.order {
            self.apply_fact(&panel, &facts, *fact, true);
        }
        if let Some(pump) = &panel.pump {
            pump.host.set_own_window(false);
        }
        self.panels.borrow_mut().push(panel.clone());
        if facts.editor.is_some() || facts.link_editor.is_some() {
            if let Some(pump) = &panel.pump {
                pump.host.editor_session(Instant::now());
            }
        }
        Ok(panel)
    }

    /// No collection borrow crosses a host call, a hook or GTK: any of those may enter this window again.
    fn snapshot(&self) -> Vec<Rc<Panel>> {
        self.panels.borrow().clone()
    }

    fn record(&self, fact: Fact, update: impl FnOnce(&mut WindowFacts)) {
        let facts = {
            let mut facts = self.facts.borrow_mut();
            update(&mut facts);
            facts.order.retain(|old| *old != fact);
            facts.order.push(fact);
            facts.clone()
        };
        for panel in self.snapshot() {
            self.apply_fact(&panel, &facts, fact, false);
        }
    }

    fn apply_fact(&self, panel: &Rc<Panel>, facts: &WindowFacts, fact: Fact, joined: bool) {
        match fact {
            Fact::Holder => {}
            Fact::PageBuilt => {
                let view = panel.webview.borrow().clone();
                if let (Some(hook), Some(view)) = (&facts.page_built, view) {
                    hook(&panel.module, &view);
                }
            }
            Fact::WaitWindow => {
                if let (Some(feed), Some((panels, window))) = (&facts.agents_feed, &facts.wait_window) {
                    Self::watch_agents_wait(panel, feed, panels, window, joined);
                }
            }
            _ => {
                let Some(pump) = &panel.pump else { return };
                let host = &pump.host;
                match fact {
                    Fact::MoveEvents => *pump.events.borrow_mut() = facts.move_events.clone(),
                    Fact::LinkEditor => {
                        if let Some(ops) = &facts.link_editor {
                            host.set_editor(Rc::new(crate::linked_window::editor::PluginEditorRpc::new(
                                host.plugin().manifest.id.clone(),
                                host.plugin().roots.clone(),
                                ops.clone(),
                            )));
                        }
                    }
                    Fact::WindowRoot => {
                        if let Some(root) = &facts.window_root {
                            host.set_window_root(root.clone());
                        }
                    }
                    Fact::OwnWindow => host.set_own_window(
                        !joined
                            && facts.own_window.as_ref().and_then(|owner| owner.as_deref())
                                == Some(host.plugin().manifest.id.as_str()),
                    ),
                    Fact::ScratchPrefix if host.wants_follow_feed() => {
                        if let Some(prefix) = &facts.scratch_prefix {
                            host.set_scratch_prefix(prefix.clone());
                        }
                    }
                    Fact::FollowRefused => {
                        if let Some(hook) = &facts.follow_refused {
                            host.on_follow_refused(hook.clone());
                        }
                    }
                    Fact::Editor => {
                        if let Some(editor) = &facts.editor {
                            host.set_editor(editor.clone());
                        }
                    }
                    Fact::GridTop => {
                        if let Some(source) = &facts.grid_top {
                            host.set_grid_top(source.clone());
                        }
                    }
                    Fact::Layout => {
                        if let Some((show_and_focus, terminal_at)) = &facts.layout {
                            host.set_layout(Rc::new(WindowLayout {
                                own: panel.module.clone(),
                                plugin: host.plugin().manifest.id.clone(),
                                show_and_focus: show_and_focus.clone(),
                                terminal_at: terminal_at.clone(),
                            }));
                        }
                    }
                    Fact::Agents => match facts.agents.as_ref() {
                        Some(Some(window)) => {
                            if let (Some(feed), Some(gesture)) = (&facts.agents_feed, &panel.gesture) {
                                host.set_agents(Some(AgentsServices {
                                    feed: feed.clone(),
                                    port: window.clone(),
                                    gesture: gesture.clone(),
                                }));
                            }
                        }
                        Some(None) => host.set_agents(None),
                        None => {}
                    },
                    Fact::Timeoutlen => {
                        if let Some(ms) = facts.timeoutlen {
                            host.set_timeoutlen(ms);
                        }
                    }
                    Fact::Theme if !panel.away.get() => host.set_theme(&facts.theme),
                    Fact::FollowFeed if host.wants_follow_feed() => {
                        if let Some((socket, scratch)) = &facts.follow_feed {
                            host.set_follow_feed(socket.clone(), scratch.clone());
                        }
                    }
                    Fact::EditorGone if facts.editor_gone && !panel.away.get() => {
                        host.editor_closed(Instant::now());
                        if !joined {
                            pump.kick();
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Routes lifecycle facts to the controller; the pump consumes each ready indication once.
    pub(crate) fn set_move_events(&self, hook: Rc<dyn Fn(eitri_core::wm::HostEvent)>) {
        self.record(Fact::MoveEvents, |facts| facts.move_events = Some(hook));
    }

    pub(crate) fn freeze(
        &self,
        win: eitri_core::wm::WinId,
        module: &ModuleId,
        mv: eitri_core::window_link::moves::MoveId,
    ) -> Result<(), String> {
        let pump = self
            .panel(module)
            .and_then(|p| p.pump.clone())
            .ok_or("the page has no running host")?;
        pump.freeze(win, module.clone(), mv)
    }

    pub(crate) fn resume(&self, module: &ModuleId) {
        if let Some(pump) = self.panel(module).and_then(|p| p.pump.clone()) {
            pump.resume();
        }
    }

    /// Ownership changes also retarget follow lines in the window that holds the editor.
    pub(crate) fn set_holder_hook(&self, hook: Rc<dyn Fn(&str, bool)>) {
        self.record(Fact::Holder, |facts| facts.holder = Some(hook));
    }

    fn holder_changed(&self, panel: &Panel, held: bool) {
        panel.away.set(!held);
        let hook = self.facts.borrow().holder.clone();
        if let (Some(hook), Some(pump)) = (hook, &panel.pump) {
            hook(&pump.host.plugin().manifest.id, held);
        }
    }

    /// An away holding must never start tasks or answer follow commands in this process.
    pub(crate) fn mark_away(&self, module: &ModuleId) {
        if let Some(panel) = self.panel(module) {
            panel.away.set(true);
            panel.fresh_page.set(false);
            if let Some(pump) = &panel.pump {
                pump.host.forget_moved();
            }
        }
    }

    pub(crate) fn forget(&self, module: &ModuleId) {
        let Some(panel) = self.panel(module) else { return };
        trace_page(module, "forget", "");
        // Whether the page is drawn here is told afresh when it comes back: its host forgot it with the document.
        panel.on_screen.set(None);
        panel.fresh_page.set(false);
        self.holder_changed(&panel, false);
        if let Some(pump) = &panel.pump {
            pump.paused.borrow_mut().clear();
            pump.host.forget_moved();
        }
        self.drop_view(&panel);
    }

    /// The page left this window, so its view goes: its web process ends (a deliberate end, which the crash guard
    /// does not count), the view leaves its slot, and the host's port is empty until a new view is built. A view
    /// kept here would be reused by the page's return, and a view whose web process was ended keeps no address to
    /// load again.
    fn drop_view(&self, panel: &Panel) {
        let view = panel.webview.borrow_mut().take();
        *panel.port.0.borrow_mut() = None;
        if let Some(view) = view {
            use webkit6::prelude::*;
            view.terminate_web_process();
            view.unparent();
        }
    }

    /// Restoring and adopting happen before navigation, so hello sees the saved state and every moved run.
    pub(crate) fn adopt(
        &self,
        module: &ModuleId,
        record: &serde_json::Value,
        fds: Vec<std::os::fd::OwnedFd>,
    ) -> Result<(), String> {
        let panel = self.panel(module).ok_or("the arriving plugin is not registered here")?;
        let pump = panel.pump.as_ref().ok_or("the plugin host is unavailable")?;
        if pump.restoring.borrow().is_some() {
            return Err("the page is already adopting tasks".into());
        }
        self.holder_changed(&panel, true);
        pump.host.set_theme(&self.theme());
        pump.adopting.set(true);
        pump.host.accept_moved();
        pump.host
            .set_restored(record.get("state").cloned().unwrap_or(serde_json::Value::Null));
        let records = record.get("tasks").cloned().unwrap_or_else(|| serde_json::json!([]));
        let (tx, rx) = std::sync::mpsc::channel();
        let wake = pump.wake.waker();
        *pump.restoring.borrow_mut() = Some(rx);
        std::thread::spawn(move || {
            let result = unpack_runs(&records, fds).and_then(|tasks| {
                let mut runner = eitri_core::plugin::tasks::TaskRunner::new(wake.clone());
                for task in tasks {
                    runner
                        .adopt(task)
                        .map_err(|e| format!("task adoption failed: {:?}", e.why))?;
                }
                Ok(runner)
            });
            let _ = tx.send(result);
            wake();
        });
        self.build_page(module, &self.theme());
        if let Some(view) = panel.webview.borrow().clone() {
            use webkit6::prelude::*;
            trace_page(module, "adopt", &format!("uri={}", trace_uri(view.uri().as_deref())));
            let module = module.clone();
            *pump.after_restore.borrow_mut() = Some(Box::new(move || {
                let uri = view.uri().map(|s| s.to_string());
                trace_page(&module, "reload", &format!("uri={}", trace_uri(uri.as_deref())));
                if let Some(uri) = uri {
                    view.load_uri(&uri);
                }
            }));
        } else {
            trace_page(module, "adopt", "uri=none");
        }
        Ok(())
    }

    pub(crate) fn follow_config(
        &self,
        owner: &str,
        socket: &Path,
        scratch: Option<&Path>,
    ) -> Option<eitri_core::window_link::editor_ops::FollowConfig> {
        let panel = self.panel(&ModuleId::lua(owner))?;
        let host = &panel.pump.as_ref()?.host;
        if !host.wants_follow_feed() {
            return None;
        }
        let filetypes = host
            .plugin()
            .manifest
            .follow
            .as_ref()
            .map(|follow| follow.filetypes.clone())
            .unwrap_or_default();
        Some(eitri_core::window_link::editor_ops::FollowConfig {
            socket: socket.to_owned(),
            scratch: scratch.map(Path::to_owned),
            filetypes,
            commands: host
                .plugin()
                .manifest
                .commands
                .iter()
                .map(|cmd| (cmd.name.clone(), cmd.does.as_str()))
                .collect(),
            auto: host.auto_show(),
        })
    }

    pub(crate) fn plugin_roots(&self, owner: &str) -> Option<BTreeMap<String, std::path::PathBuf>> {
        let roots: BTreeMap<_, _> = self
            .panel(&ModuleId::lua(owner))?
            .pump
            .as_ref()?
            .host
            .plugin()
            .roots
            .iter()
            .map(|(name, path)| (name.clone(), path.clone()))
            .collect();
        (!roots.is_empty()).then_some(roots)
    }

    pub(crate) fn set_link_editor(&self, ops: Rc<crate::linked_window::editor::EditorOps>) {
        self.record(Fact::LinkEditor, |facts| facts.link_editor = Some(ops));
    }

    fn panel(&self, module: &ModuleId) -> Option<Rc<Panel>> {
        self.snapshot().into_iter().find(|p| p.module == *module)
    }

    /// Whether `module` is a plugin's that starts hidden: the window never places it by itself.
    pub(crate) fn starts_hidden(&self, module: &ModuleId) -> bool {
        self.panel(module).is_some_and(|p| p.hidden_start)
    }

    /// The plugin's own open rule (its `position`, else its `[module]` table), counted where a Lua panel's
    /// `position` counts.
    pub(crate) fn own_rule(&self, module: &ModuleId) -> Option<OpenRule> {
        self.panel(module).and_then(|p| p.own_rule.clone())
    }

    /// Whether the plugin's page exists. A plugin with no page here (WebKit cannot start) never has one.
    pub(crate) fn is_built(&self, module: &ModuleId) -> bool {
        self.panel(module).is_some_and(|p| p.port.page().is_some())
    }

    /// `module`'s holding taken back where it was away: this window answers its follow lines, commands and tasks
    /// again, and a page already built loads afresh. Nothing is built here, and a holding not away is left as it is.
    pub(crate) fn activate_holding(&self, module: &ModuleId) {
        if let Some(panel) = self.panel(module) {
            if panel.away.get() {
                self.holder_changed(&panel, true);
                if let Some(pump) = &panel.pump {
                    pump.host.accept_moved();
                    pump.host.set_theme(&self.theme());
                    pump.host.resume_moved();
                }
                panel.fresh_page.set((panel.load)(None));
            }
        }
    }

    /// Builds the page when the module has none here (its first show, or its return after its view was dropped);
    /// with a page, nothing. The host's port sends to the page from then on, and the page says hello and gets the
    /// host's current state. Whether it built the page now.
    pub(crate) fn build_page(&self, module: &ModuleId, theme: &PageTheme) -> bool {
        let Some(panel) = self.panel(module) else {
            return false;
        };
        if panel.port.page().is_some() {
            return false;
        }
        let Some(build) = panel.build.clone() else { return false };
        let page = build(theme);
        panel.fresh_page.set(page.is_some());
        *panel.port.0.borrow_mut() = page;
        let (hook, view) = (self.facts.borrow().page_built.clone(), panel.webview.borrow().clone());
        if let (Some(hook), Some(view)) = (hook, view) {
            hook(&panel.module, &view);
        }
        true
    }

    /// The page loaded again from its store as it is now: built afresh when it has no view here, else its view
    /// navigated to its entry, unless it was already built or reactivated for this return. Whether it is available.
    pub(crate) fn reload_back(&self, module: &ModuleId) -> bool {
        let Some(panel) = self.panel(module) else {
            return false;
        };
        if panel.port.page().is_none() {
            self.activate_holding(module);
            return self.build_page(module, &self.theme());
        }
        if panel.away.get() {
            self.activate_holding(module);
            return true;
        }
        if panel.fresh_page.replace(false) {
            return true;
        }
        (panel.load)(Some(&panel.url))
    }

    /// An arriving page's view, built before the commit when it has none.
    pub(crate) fn prepare(&self, module: &ModuleId) {
        let fresh = self.build_page(module, &self.theme());
        trace_page(module, "prepare", if fresh { "built=fresh" } else { "built=reused" });
    }

    /// `hook` hears of every page's view: now, of each built already (a page that starts shown is built at
    /// startup, before the window's text size exists), and later of each built at its first show.
    pub(crate) fn set_page_built(&self, hook: PageBuilt) {
        self.record(Fact::PageBuilt, |facts| facts.page_built = Some(hook));
    }

    /// The window's theme as the pages last got it (what a page built later starts with).
    pub(crate) fn theme(&self) -> PageTheme {
        self.facts.borrow().theme.clone()
    }

    /// The module's built view, for a kill (its web process ended) and a revive (loaded again).
    pub(crate) fn webview(&self, module: &ModuleId) -> Option<webkit6::WebView> {
        self.panel(module).and_then(|p| p.webview.borrow().clone())
    }

    fn hosts(&self) -> impl Iterator<Item = (ModuleId, Rc<Pump>)> {
        self.snapshot()
            .into_iter()
            .filter_map(|panel| panel.pump.clone().map(|pump| (panel.module.clone(), pump)))
    }

    fn held_hosts(&self) -> impl Iterator<Item = (ModuleId, Rc<Pump>)> {
        self.snapshot()
            .into_iter()
            .filter(|panel| !panel.away.get())
            .filter_map(|panel| panel.pump.clone().map(|pump| (panel.module.clone(), pump)))
    }

    /// Every plugin's module, in load order.
    pub(crate) fn modules(&self) -> Vec<ModuleId> {
        self.snapshot().into_iter().map(|p| p.module.clone()).collect()
    }

    /// The host of the plugin `owner` names, when it is loaded here and wants the follow socket's lines.
    fn follower(&self, owner: &str) -> Option<Rc<Pump>> {
        self.held_hosts()
            .map(|(_, pump)| pump)
            .find(|pump| pump.host.plugin().manifest.id == owner && pump.host.wants_follow_feed())
    }

    /// Whether any loaded plugin follows the editor or defines nvim commands: only then is the follow socket bound.
    pub(crate) fn wants_follow_feed(&self) -> bool {
        self.hosts().any(|(_, pump)| pump.host.wants_follow_feed())
    }

    /// The window's follow socket and Eitri's scratch directory, to every host that wants them.
    pub(crate) fn set_follow_feed(&self, socket: &Path, scratch_dir: Option<&Path>) {
        self.record(Fact::FollowFeed, |facts| {
            facts.follow_feed = Some((socket.to_owned(), scratch_dir.map(Path::to_owned)))
        });
    }

    /// Views and gone lines to their hosts (each host pumped once after); commands back in order for the window:
    /// `Show`, `Hide` and `Toggle` as sent, `Auto` as sent only when that host's `auto_show` is on (still `Auto`, so
    /// the window shows only a hidden module and leaves a zoom or a tab the user chose alone). A line for an
    /// owner that is not a loaded plugin wanting the feed is dropped. Views go first: the reader lets a gone line
    /// out only when it is newer than every view of its plugin let out so far, so this is the order nvim sent
    /// them in. After the editor's end and before a new session, views and gone lines are dropped: they describe
    /// buffers of an editor that is no more, and a view would bring its page back to one.
    pub(crate) fn route_follow(&self, batch: FollowBatch, now: Instant) -> Vec<(ModuleId, FollowCmd)> {
        let cmd_lines = batch
            .ordered
            .iter()
            .filter(|line| matches!(line, Ordered::Cmd { .. }))
            .count() as u64;
        let trace = self
            .follow_totals
            .borrow_mut()
            .add(batch.views_received, cmd_lines, now);
        if let (Some(line), true) = (trace, eitri_core::plugin::trace::enabled()) {
            println!("{line}");
        }
        let mut touched: Vec<Rc<Pump>> = Vec::new();
        let mut touch = |pump: &Rc<Pump>| {
            if !touched.iter().any(|p| Rc::ptr_eq(p, pump)) {
                touched.push(pump.clone());
            }
        };
        let editor_gone = self.facts.borrow().editor_gone;
        for view in batch.views {
            if editor_gone {
                break;
            }
            if let Some(pump) = self.follower(&view.owner) {
                pump.host.follow_view(view, now);
                touch(&pump);
            }
        }
        let mut commands = Vec::new();
        for line in batch.ordered {
            match line {
                Ordered::Gone { owner, buf } => {
                    if editor_gone {
                        continue;
                    }
                    if let Some(pump) = self.follower(&owner) {
                        pump.host.follow_gone(buf, now);
                        touch(&pump);
                    }
                }
                Ordered::Cmd { owner, cmd, .. } => {
                    let Some(pump) = self.follower(&owner) else { continue };
                    let module = ModuleId::lua(&owner);
                    match cmd {
                        FollowCmd::Auto if pump.host.auto_show() => commands.push((module, FollowCmd::Auto)),
                        FollowCmd::Auto => {}
                        _ => commands.push((module, cmd)),
                    }
                }
            }
        }
        for pump in touched {
            pump.kick();
        }
        commands
    }

    /// Whether the plugin's module is drawn now, from the window's layout. Told to its host only when it changed.
    pub(crate) fn set_on_screen(&self, module: &ModuleId, on: bool) {
        let Some(panel) = self.panel(module) else { return };
        let Some(pump) = &panel.pump else { return };
        if panel.on_screen.replace(Some(on)) == Some(on) {
            return;
        }
        pump.host.set_on_screen(on, Instant::now());
    }

    /// The window's project root: a followed document inside it has it as its project.
    pub(crate) fn set_window_root(&self, root: &Path) {
        self.record(Fact::WindowRoot, |facts| facts.window_root = Some(root.to_owned()));
    }

    /// Exactly the host whose declared root owns this window receives the own-window grant.
    pub(crate) fn set_own_window(&self, owner: Option<&str>) {
        self.record(Fact::OwnWindow, |facts| {
            facts.own_window = Some(owner.map(str::to_owned))
        });
    }

    /// The window's editor's nvim ended: every following page is told, once, until a new nvim runs.
    pub(crate) fn editor_closed(&self) {
        if self.facts.borrow().editor_gone {
            return;
        }
        self.record(Fact::EditorGone, |facts| facts.editor_gone = true);
    }

    /// The window's editor runs a new nvim, in the pane whose nvim ended: what [`editor_closed`](Self::editor_closed)
    /// stopped starts again, so every page that follows the editor has its follow snippet installed into the new
    /// nvim once (its session is a new one) and follows it from there.
    pub(crate) fn editor_back(&self) {
        self.editor_session();
    }

    /// Each host looks at the editor's session now (nvim may have just started).
    pub(crate) fn editor_session(&self) {
        self.facts.borrow_mut().editor_gone = false;
        let now = Instant::now();
        for (_, pump) in self.held_hosts() {
            pump.host.editor_session(now);
        }
    }

    /// Excludes every Eitri scratch directory under `prefix` from following, the agent window's drafts among them, for
    /// every host that follows the editor.
    pub(crate) fn set_scratch_prefix(&self, prefix: Option<PathBuf>) {
        self.record(Fact::ScratchPrefix, |facts| facts.scratch_prefix = Some(prefix));
    }

    /// What runs when the editor refuses a host's follow snippet: the hosts never try it again themselves.
    pub(crate) fn on_follow_refused(&self, hook: Rc<dyn Fn()>) {
        self.record(Fact::FollowRefused, |facts| facts.follow_refused = Some(hook));
    }

    /// The window's one editor handle, shared with the chat.
    pub(crate) fn set_editor(&self, editor: Rc<dyn EditorRpc>) {
        self.record(Fact::Editor, |facts| facts.editor = Some(editor));
    }

    /// Where the window's editor draws nvim's grid (the share of its height above the first row), for views.
    pub(crate) fn set_grid_top(&self, source: Rc<dyn Fn() -> Option<f64>>) {
        self.record(Fact::GridTop, |facts| facts.grid_top = Some(source));
    }

    pub(crate) fn set_layout(&self, show_and_focus: ShowAndFocus, terminal_at: TerminalAt) {
        self.record(Fact::Layout, |facts| facts.layout = Some((show_and_focus, terminal_at)));
    }

    /// The agents layer for the pages that declare `agents`: one feed for the whole window, so every such page
    /// reads the same reader, and the window's port. `None` (the layer is off) leaves each of them answering
    /// `disabled`.
    pub(crate) fn set_agents(&self, window: Option<Rc<WindowAgents>>) {
        let feed = window
            .as_ref()
            .map(|window| AgentsFeed::of_dir(window.control_dir.clone()));
        self.record(Fact::Agents, |facts| {
            facts.agents = Some(window);
            facts.agents_feed = feed;
        });
    }

    /// Shows and hides each module declared `[module] shown_while = "agents-wait"` from the window's agents feed
    /// (after [`Self::set_agents`]; with the layer off nothing does): shown while an agent waits or a round is at a
    /// stop, hidden when neither holds, each time the answer changes, so a show or hide the person makes in between
    /// stands until the next change. A module handed to another window is left alone.
    pub(crate) fn show_while_agents_wait(self: &Rc<Self>, window: Rc<dyn FollowWindow>) {
        let panels = Rc::downgrade(self);
        self.record(Fact::WaitWindow, |facts| facts.wait_window = Some((panels, window)));
    }

    fn watch_agents_wait(
        panel: &Panel,
        feed: &Rc<AgentsFeed>,
        panels: &Weak<PluginPanels>,
        window: &Rc<dyn FollowWindow>,
        joined: bool,
    ) {
        let declared = panel
            .pump
            .as_ref()
            .and_then(|pump| pump.host.plugin().manifest.module.as_ref().and_then(|m| m.shown_while));
        if declared != Some(ShownWhile::AgentsWait) {
            return;
        }
        let module = panel.module.clone();
        let wake = match PanelWake::new() {
            Ok(wake) => wake,
            Err(e) => {
                eprintln!("[plugin] {module:?} cannot follow the agents that wait: {e}");
                return;
            }
        };
        let follow = Rc::new(WaitFollow {
            show: WaitShow::new(feed.clone(), wake.waker()),
            panels: panels.clone(),
            window: window.clone(),
            module,
            recheck: RefCell::new(None),
        });
        if joined {
            // The panel enters the collection and the controller declares its module after these facts are
            // replayed. Wait until idle so the first pump can show it from the feed's cached state.
            let weak = Rc::downgrade(&follow);
            glib::idle_add_local_once(move || {
                if let Some(follow) = weak.upgrade() {
                    WaitFollow::pump(&follow);
                }
            });
        }
        let fd = wake.fd();
        let named = format!("{:?}", follow.module);
        let watched = neovide_editor::fd_watch::add_local(
            fd,
            glib::IOCondition::IN | glib::IOCondition::ERR | glib::IOCondition::HUP,
            glib::ffi::G_PRIORITY_DEFAULT,
            move |_| {
                wake.take();
                WaitFollow::pump(&follow);
                glib::ControlFlow::Continue
            },
        );
        if watched.is_err() {
            eprintln!("[plugin] {named} cannot follow the agents that wait: its wake cannot be watched here");
        }
    }

    /// The window's answer to plugin `plugin`'s `terminal.at`, for its host to give the page that asked.
    pub(crate) fn terminal_at_answered(&self, plugin: &str, ticket: u64, answer: TerminalAtAnswer) {
        let pump = self
            .held_hosts()
            .map(|(_, pump)| pump)
            .find(|pump| pump.host.plugin().manifest.id == plugin);
        match pump {
            Some(pump) => pump.host.terminal_at_answered(ticket, answer, Instant::now()),
            None => eprintln!("[plugin {plugin}] terminal.at {ticket}: answered for a plugin not held here; dropped"),
        }
    }

    /// Whether a plugin's module holds the keys, from the window's focus tracking. Any other module: nothing.
    pub(crate) fn set_has_keys(&self, module: &ModuleId, has_keys: bool) {
        if let Some((_, pump)) = self.held_hosts().find(|(m, _)| m == module) {
            pump.host.set_has_keys(has_keys);
        }
    }

    /// Every page follows the window's colorscheme. A change that leaves the page's set as it was (a size
    /// alone, say) sends nothing.
    pub(crate) fn set_theme(&self, tokens: &ThemeTokens) {
        let text_px = self.facts.borrow().text_px;
        self.send_theme(PageTheme::from_tokens(tokens).with_text_size(text_px));
    }

    /// Remembers the editor's text size without sending anything: for a caller that calls [`Self::set_theme`] next, so
    /// a payload that moves the size and the colourscheme together reaches the pages as one theme event, not two.
    pub(crate) fn store_text_size(&self, css_px: Option<f32>) {
        self.facts.borrow_mut().text_px = css_px;
    }

    /// The editor's text size in the pages' CSS px (`EditorFont::css_px`), which a page that wants its prose the
    /// editor's size reads as `--nv-text-size`; `None` takes it away. Sent on a change only, like any theme.
    pub(crate) fn set_text_size(&self, css_px: Option<f32>) {
        self.store_text_size(css_px);
        let theme = self.facts.borrow().theme.clone().with_text_size(css_px);
        self.send_theme(theme);
    }

    fn send_theme(&self, theme: PageTheme) {
        self.record(Fact::Theme, |facts| facts.theme = theme);
    }

    /// The editor's `timeoutlen`, which every page's multi-key sequences wait for.
    pub(crate) fn set_timeoutlen(&self, ms: u32) {
        self.record(Fact::Timeoutlen, |facts| facts.timeoutlen = Some(ms));
    }

    /// The plugin's title and the key table its current page declared (`None` inside: not declared yet), or
    /// `None` when `module` is not a plugin page with a running host.
    pub(crate) fn declared_rows(&self, module: &ModuleId) -> Option<(String, Option<Vec<EffectiveRow>>)> {
        let (_, pump) = self.held_hosts().find(|(m, _)| m == module)?;
        Some((pump.host.plugin().manifest.title.clone(), pump.host.declared_rows()))
    }

    /// The pages whose guarded runs are still held by this window, without waiting for their children.
    pub(crate) fn pages_with_tasks(&self) -> Vec<ModuleId> {
        self.held_hosts()
            .filter(|(_, pump)| !pump.host.runs().is_empty())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// `prefix x` on a plugin's module: its runs stop and its assist is put back.
    pub(crate) fn stop(&self, module: &ModuleId) {
        if let Some((_, pump)) = self.held_hosts().find(|(m, _)| m == module) {
            pump.host.stop(Instant::now());
            pump.kick();
        }
    }

    /// The window is closing: every run is stopped, and the application is held until they have ended (at most
    /// the grace and a second), so no task outlives the window.
    pub(crate) fn shutdown(&self, app: &gtk4::Application) {
        let waiters: Vec<RunsWaiter> = self
            .held_hosts()
            .map(|(_, pump)| pump.host.shutdown(Instant::now()))
            .collect();
        if waiters.iter().all(RunsWaiter::finished) {
            return;
        }
        let mut hold = Some(app.hold());
        let deadline = Instant::now() + GRACE + Duration::from_secs(1);
        glib::timeout_add_local(Duration::from_millis(50), move || {
            if waiters.iter().all(RunsWaiter::finished) || Instant::now() >= deadline {
                hold.take();
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::plugin::load::load_plugin;
    use eitri_core::plugin::test_support::{demo_plugin, Scratch};
    use eitri_core::wm::{Batch, Decide, HostEvent, HostOp, PairEvent, PairOp, PluginEvent, PluginOp, WinId};

    #[derive(Default)]
    struct FakeWindow;

    impl FollowWindow for FakeWindow {
        fn is_shown(&self, _: &ModuleId) -> bool {
            false
        }
        fn is_on_screen(&self, _: &ModuleId) -> bool {
            false
        }
        fn show(&self, _: &ModuleId) {}
        fn hide(&self, _: &ModuleId) {}
        fn command(&self, _: &ModuleId, _: FollowCmd) {}
    }

    #[test]
    fn joined_panel_inherits_window_facts() {
        let _turn = crate::layout_state::default_context_turn();
        let context = glib::MainContext::default();
        let _owner = context.acquire().unwrap();
        let scratch = Scratch::new("panels-join-facts");
        let registration = registered(&scratch, "demo", true, None);
        let before = Rc::new(Recorded::default());
        let after = Rc::new(Recorded::default());
        let make = |page: Rc<Recorded>| {
            let port = Rc::new(LazyPort::default());
            *port.0.borrow_mut() = Some(page);
            panel(&registration, port, None)
        };
        let installed = Rc::new(plugin_panels(vec![make(before.clone())]));
        let joined = Rc::new(plugin_panels(Vec::new()));
        let editor = Rc::new(RecordingEditor::default());
        editor.target.set(Some(7));
        let events: Rc<dyn Fn(HostEvent)> = Rc::new(|_| {});
        let holding_calls = Rc::new(RefCell::new(Vec::new()));
        let holders: Rc<dyn Fn(&str, bool)> = {
            let calls = holding_calls.clone();
            Rc::new(move |id, held| calls.borrow_mut().push((id.to_owned(), held)))
        };
        let built: PageBuilt = Rc::new(|_, _| {});
        let refused: Rc<dyn Fn()> = Rc::new(|| {});
        let link = crate::linked_window::editor::EditorOps::new(Rc::new(|_, _| {}));
        let focused = Rc::new(RefCell::new(Vec::new()));
        let focus: ShowAndFocus = {
            let focused = focused.clone();
            Rc::new(move |module| {
                focused.borrow_mut().push(module.clone());
                Ok(())
            })
        };
        let grid_reads = Rc::new(Cell::new(0));
        let grid: GridTop = {
            let reads = grid_reads.clone();
            Rc::new(move || {
                reads.set(reads.get() + 1);
                Some(0.125)
            })
        };
        for panels in [&installed, &joined] {
            panels.set_move_events(events.clone());
            panels.set_text_size(Some(19.0));
            panels.set_scratch_prefix(Some(scratch.0.join("drafts")));
            panels.on_follow_refused(refused.clone());
            panels.set_holder_hook(holders.clone());
            panels.set_follow_feed(&scratch.0.join("follow"), Some(&scratch.0));
            panels.set_timeoutlen(317);
            panels.set_window_root(&scratch.0);
            panels.set_own_window(Some("demo"));
            panels.set_theme(&tokens("light", 0x575279, 0xfaf4ed));
            panels.set_page_built(built.clone());
            panels.set_layout(focus.clone(), Rc::new(|_, _, _| Ok(())));
            panels.set_link_editor(link.clone());
            panels.set_editor(editor.clone());
            panels.set_grid_top(grid.clone());
            panels.set_agents(None);
            panels.show_while_agents_wait(Rc::new(FakeWindow::default()));
            panels.editor_closed();
        }
        let added = joined
            .join_with(&registration, || Ok(Rc::new(make(after.clone()))))
            .unwrap();
        let host = &added.pump.as_ref().unwrap().host;
        let original = installed.panel(&ModuleId::lua("demo")).unwrap();
        assert!(original.pump.as_ref().unwrap().host.own_window());
        assert_eq!(host.theme(), original.pump.as_ref().unwrap().host.theme());
        assert!(!host.own_window());
        assert!(Rc::ptr_eq(
            added.pump.as_ref().unwrap().events.borrow().as_ref().unwrap(),
            &events
        ));
        before.0.borrow_mut().clear();
        after.0.borrow_mut().clear();
        before.2.borrow_mut().clear();
        after.2.borrow_mut().clear();
        for (panels, panel) in [(&installed, &original), (&joined, &added)] {
            let host = &panel.pump.as_ref().unwrap().host;
            for (id, verb, args) in [
                (1, "host.hello", serde_json::json!({})),
                (2, "editor.follow", serde_json::json!({})),
                (3, "layout.focus", serde_json::json!({"target": "self"})),
            ] {
                host.page_message(
                    &serde_json::json!({"v": 1, "id": id, "verb": verb, "args": args}).to_string(),
                    Instant::now(),
                );
            }
            panels.set_on_screen(&panel.module, true);
            host.follow_view(view("demo"), Instant::now());
            panels.mark_away(&panel.module);
            panels.activate_holding(&panel.module);
        }
        assert_eq!(grid_reads.get(), 2, "both hosts read the window's grid geometry");
        assert_eq!(*focused.borrow(), [ModuleId::lua("demo"), ModuleId::lua("demo")]);
        assert_eq!(
            *holding_calls.borrow(),
            [("demo".to_owned(), true), ("demo".to_owned(), true)]
        );
        assert_eq!(*before.0.borrow(), *after.0.borrow());
        before.1.borrow_mut()[0]["result"]["own_window"] = serde_json::json!(false);
        assert_eq!(*before.1.borrow(), *after.1.borrow());
        before.2.borrow_mut()[0]["result"]["own_window"] = serde_json::json!(false);
        assert_eq!(*before.2.borrow(), *after.2.borrow());
        assert_eq!(after.1.borrow()[0]["result"]["timeoutlen"], 317);
        assert!(after
            .2
            .borrow()
            .iter()
            .any(|message| message["event"] == "editor.closed"));
        assert_eq!(joined.modules(), [ModuleId::lua("demo")]);
    }

    #[test]
    fn joining_checks_the_running_editor_session() {
        let scratch = Scratch::new("panels-join-editor");
        let registration = registered(&scratch, "demo", true, None);
        let panels = plugin_panels(Vec::new());
        let editor = Rc::new(RecordingEditor::default());
        editor.target.set(Some(9));
        panels.set_follow_feed(&scratch.0.join("follow"), Some(&scratch.0));
        panels.set_scratch_prefix(Some(scratch.0.join("drafts")));
        panels.set_editor(editor.clone());
        let wakes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let joined = panels
            .join_with(&registration, || {
                let wakes = wakes.clone();
                Ok(Rc::new(panel_waking(
                    &registration,
                    Rc::default(),
                    None,
                    std::sync::Arc::new(move || {
                        wakes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }),
                )))
            })
            .unwrap();
        assert_eq!(editor.targets.get(), 1, "join checks the editor before any pump runs");
        assert!(
            wakes.load(std::sync::atomic::Ordering::Relaxed) > 0,
            "the session check wakes the follow host"
        );
        let pump = joined.pump.as_ref().unwrap();
        pump.host.pump(Instant::now());
        let installs = editor.installs();
        assert_eq!(installs.len(), 1, "the running nvim gets the joined plugin's snippet");
        let args = installs[0][1].as_map().unwrap();
        for (key, path) in [
            ("socket", scratch.0.join("follow")),
            ("scratch_dir", scratch.0.clone()),
            ("scratch_prefix", scratch.0.join("drafts")),
        ] {
            let value = &args.iter().find(|(name, _)| name.as_str() == Some(key)).unwrap().1;
            assert_eq!(
                value,
                &rmpv::Value::Binary(path.as_os_str().as_encoded_bytes().to_vec()),
                "{key}"
            );
        }
    }

    #[test]
    fn joining_without_an_editor_does_not_check_a_session() {
        let scratch = Scratch::new("panels-join-no-editor");
        let registration = registered(&scratch, "demo", true, None);
        let panels = plugin_panels(Vec::new());
        panels.set_follow_feed(&scratch.0.join("follow"), None);
        let wakes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        panels
            .join_with(&registration, || {
                let wakes = wakes.clone();
                Ok(Rc::new(panel_waking(
                    &registration,
                    Rc::default(),
                    None,
                    std::sync::Arc::new(move || {
                        wakes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }),
                )))
            })
            .unwrap();
        assert_eq!(
            wakes.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "only setting the follow feed wakes the host"
        );
    }

    #[test]
    fn joining_refuses_a_duplicate_before_making_a_panel() {
        let scratch = Scratch::new("panels-join-duplicate");
        let registration = registered(&scratch, "demo", true, None);
        let panels = plugin_panels(Vec::new());
        panels
            .join_with(&registration, || Ok(Rc::new(panel(&registration, Rc::default(), None))))
            .unwrap();
        let result = panels.join_with(&registration, || {
            panic!("a duplicate must not build or register anything")
        });
        assert!(result.is_err());
        assert_eq!(panels.modules(), [ModuleId::lua("demo")]);
    }

    #[test]
    fn a_setter_hook_can_read_and_join_panels() {
        struct Reentrant(Rc<dyn Fn()>);
        impl PagePort for Reentrant {
            fn send(&self, _: &str) {}
            fn set_theme(&self, _: &PageTheme) {
                (self.0)();
            }
        }
        let scratch = Scratch::new("panels-join-reentrant");
        let first = registered(&scratch, "first", false, None);
        let second = registered(&scratch, "second", false, None);
        let port = Rc::new(LazyPort::default());
        let panels = Rc::new(plugin_panels(vec![panel(&first, port.clone(), None)]));
        let weak = Rc::downgrade(&panels);
        *port.0.borrow_mut() = Some(Rc::new(Reentrant(Rc::new(move || {
            let panels = weak.upgrade().unwrap();
            assert_eq!(panels.modules(), [ModuleId::lua("first")]);
            panels
                .join_with(&second, || Ok(Rc::new(panel(&second, Rc::default(), None))))
                .unwrap();
        }))));
        panels.set_theme(&tokens("light", 0x575279, 0xfaf4ed));
        assert_eq!(panels.modules().len(), 2);
    }

    fn saved_page(
        handoff: bool,
    ) -> (
        Scratch,
        Panel,
        Rc<Recorded>,
        Rc<RefCell<Vec<eitri_core::wm::HostEvent>>>,
        Instant,
    ) {
        let scratch = Scratch::new("page-move-save");
        let registration = registered_with(&scratch, "demo", true, None, |text| {
            if handoff {
                format!("{text}\n[page]\nhandoff = true\n")
            } else {
                text
            }
        });
        let page = Rc::new(Recorded::default());
        let port = Rc::new(LazyPort::default());
        *port.0.borrow_mut() = Some(page.clone());
        let panel = panel(&registration, port, None);
        let pump = panel.pump.as_ref().unwrap();
        let events = Rc::new(RefCell::new(Vec::new()));
        let received = events.clone();
        *pump.events.borrow_mut() = Some(Rc::new(move |event| received.borrow_mut().push(event)));
        let now = Instant::now();
        pump.host
            .page_message(r#"{"v":1,"id":1,"verb":"host.hello","args":{}}"#, now);
        page.0.borrow_mut().clear();
        let runner = pump.host.take_move_runner().unwrap();
        pump.host.request_save(now + Duration::from_secs(1));
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(tx.send(freeze_runs(runner)).is_ok());
        *pump.saving.borrow_mut() = Some(FreezeAsk {
            win: eitri_core::wm::WinId::MAIN,
            module: panel.module.clone(),
            mv: eitri_core::window_link::moves::MoveId(7),
            answer: rx,
            frozen: None,
            state: None,
            cancelled: false,
            deadline: now + Duration::from_secs(1),
        });
        (scratch, panel, page, events, now)
    }

    #[test]
    fn a_page_without_handoff_is_reopened_and_its_tasks_still_move() {
        let (_scratch, panel, page, events, now) = saved_page(false);
        assert!(
            page.0.borrow().is_empty(),
            "a page without handoff gets no save request"
        );
        panel.pump.as_ref().unwrap().moving(now);
        assert!(matches!(
            &events.borrow()[0],
            HostEvent::Plugin(PluginEvent::Frozen { record, mv, .. })
                if record["state"].is_null() && record["built"] == true && mv.0 == 7
        ));
        task_survives_move();
    }

    #[test]
    fn a_save_timing_out_moves_the_page_without_state() {
        let (_scratch, panel, page, events, now) = saved_page(true);
        assert_eq!(*page.0.borrow(), ["host.save"]);
        let pump = panel.pump.as_ref().unwrap();
        pump.moving(now + Duration::from_millis(999));
        assert!(events.borrow().is_empty());
        pump.moving(now + Duration::from_secs(1));
        assert!(matches!(
            &events.borrow()[0],
            HostEvent::Plugin(PluginEvent::Frozen { record, mv, .. })
                if record["state"].is_null() && record["tasks"] == serde_json::json!([]) && mv.0 == 7
        ));
        pump.moving(now + Duration::from_secs(2));
        assert_eq!(events.borrow().len(), 1);
    }

    #[test]
    fn a_saved_page_keeps_its_state_and_a_cancelled_save_resumes_without_sending_it() {
        let (_scratch, panel, _page, events, now) = saved_page(true);
        let pump = panel.pump.as_ref().unwrap();
        pump.host.page_message(
            r#"{"v":1,"id":2,"verb":"page.saved","args":{"state":{"scroll":12}}}"#,
            now,
        );
        pump.moving(now);
        assert!(matches!(
            &events.borrow()[0],
            HostEvent::Plugin(PluginEvent::Frozen { record, .. }) if record["state"]["scroll"] == 12
        ));
        let (_scratch, panel, _page, events, now) = saved_page(true);
        let pump = panel.pump.as_ref().unwrap();
        pump.resume();
        pump.moving(now);
        assert!(events.borrow().is_empty(), "a cancelled save never sends a move");
    }

    #[test]
    fn a_task_moved_with_its_page_keeps_reporting_and_survives_window_1() {
        task_survives_move();
    }

    #[test]
    fn guard_entry() {
        if std::env::var_os(eitri_core::run_guard::HELPER_VAR).is_some() {
            eitri_core::run_guard::serve_inherited();
        }
    }

    fn task_survives_move() {
        use eitri_core::plugin::tasks::{ExitKind, Launch, TaskEvent, TaskRunner};
        use std::sync::Arc;
        let scratch = Scratch::new("page-moved-task");
        let go = scratch.0.join("go");
        let guard = eitri_core::run_guard::test_binary_guard("plugin_panels::tests::guard_entry");
        let mut runner = TaskRunner::with_guard(Arc::new(|| {}), Some(guard));
        let run = runner
            .start(Launch {
                task: "worker".into(),
                vars: Default::default(),
                program: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    "echo before; while [ ! -f go ]; do sleep 0.01; done; echo after".into(),
                ],
                cwd: scratch.0.clone(),
                timeout: None,
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let frozen = loop {
            match freeze_runs(runner) {
                Ok(frozen) => break frozen,
                Err((back, _)) => runner = back,
            }
            assert!(Instant::now() < deadline, "the guarded task did not start");
            std::thread::yield_now();
        };
        let FrozenRuns {
            runner,
            paused,
            records,
            fds,
        } = frozen;
        for fd in &fds {
            use std::os::fd::AsRawFd;
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
            assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
        }
        let tasks = unpack_runs(&records, fds).unwrap();
        drop(paused);
        drop(runner);
        let mut destination = TaskRunner::new(Arc::new(|| {}));
        for task in tasks {
            destination.adopt(task).unwrap();
        }
        std::fs::write(go, "").unwrap();
        let mut lines = Vec::new();
        loop {
            let (events, _) = destination.poll(Instant::now() + Duration::from_secs(1));
            for event in events {
                match event {
                    TaskEvent::Output { lines: got, .. } => lines.extend(got),
                    TaskEvent::Exit { run: ended, kind, .. } => {
                        assert_eq!(ended, run);
                        assert_eq!(kind, ExitKind::Exited(0));
                        assert_eq!(lines, ["before", "after"]);
                        return;
                    }
                }
            }
            assert!(Instant::now() < deadline, "the moved task did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_plugin_moves_the_keys_to_the_editor_or_to_its_own_module() {
        let asked: Rc<RefCell<Vec<ModuleId>>> = Rc::default();
        let layout = WindowLayout {
            own: ModuleId::lua("demo"),
            plugin: "demo".into(),
            show_and_focus: {
                let asked = asked.clone();
                Rc::new(move |id: &ModuleId| {
                    asked.borrow_mut().push(id.clone());
                    Ok(())
                })
            },
            terminal_at: Rc::new(|_: &str, _: PathBuf, _: u64| Err("unused".into())),
        };
        layout.focus(FocusTarget::Editor).unwrap();
        layout.focus(FocusTarget::Page).unwrap();
        assert_eq!(*asked.borrow(), [ModuleId::editor(), ModuleId::lua("demo")]);
    }

    #[test]
    fn a_plugins_terminal_at_goes_to_the_window_with_its_id_and_ticket() {
        let asked: Rc<RefCell<Vec<(String, PathBuf, u64)>>> = Rc::default();
        let layout = WindowLayout {
            own: ModuleId::lua("demo"),
            plugin: "demo".into(),
            show_and_focus: Rc::new(|_: &ModuleId| Ok(())),
            terminal_at: {
                let asked = asked.clone();
                Rc::new(move |plugin: &str, dir: PathBuf, ticket: u64| {
                    asked.borrow_mut().push((plugin.to_owned(), dir, ticket));
                    Err("the controller is deciding".into())
                })
            },
        };
        assert_eq!(
            layout.terminal_at("/w/work".into(), 9),
            Err("the controller is deciding".to_string()),
            "a refusal reaches the plugin host as it was given"
        );
        assert_eq!(*asked.borrow(), [("demo".to_string(), PathBuf::from("/w/work"), 9)]);
    }

    /// What a host did to its page: theme updates and event names, with replies recorded separately.
    #[derive(Default)]
    struct Recorded(
        RefCell<Vec<String>>,
        RefCell<Vec<serde_json::Value>>,
        RefCell<Vec<serde_json::Value>>,
    );

    impl eitri_core::plugin::host::PagePort for Recorded {
        fn send(&self, message: &str) {
            let value: serde_json::Value = serde_json::from_str(message).expect("the host sends JSON");
            self.2.borrow_mut().push(value.clone());
            let event = value["event"].as_str().unwrap_or("(not an event)").to_owned();
            self.0.borrow_mut().push(event);
            if value.get("re").is_some() {
                self.1.borrow_mut().push(value);
            }
        }

        fn set_theme(&self, _theme: &PageTheme) {
            self.0.borrow_mut().push("set_theme".to_owned());
        }
    }

    /// A demo plugin as `init.lua` registers it, with `[module]` cut from its manifest when `module` is false.
    fn registered(scratch: &Scratch, name: &str, module: bool, position: Option<PanelSlot>) -> Registered {
        registered_with(scratch, name, module, position, |text| text)
    }

    /// [`registered`], its manifest's text passed through `edit` first.
    fn registered_with(
        scratch: &Scratch,
        name: &str,
        module: bool,
        position: Option<PanelSlot>,
        edit: impl Fn(String) -> String,
    ) -> Registered {
        let (dir, settings) = demo_plugin(&scratch.0.join(name));
        let manifest = dir.join("eitri-plugin.toml");
        // Each plugin its own id, so each has its own module.
        let text = edit(std::fs::read_to_string(&manifest).unwrap());
        let mut text = text.replacen("id = \"demo\"", &format!("id = \"{name}\""), 1);
        if !module {
            // The follow capabilities need the `[follow]` table, which follows `[module]` and goes with it.
            text = text[..text.find("\n[module]\n").unwrap() + 1].replacen(
                ", \"editor.follow\", \"editor.commands\"]",
                "]",
                1,
            );
        }
        std::fs::write(&manifest, text).unwrap();
        let plugin = Rc::new(load_plugin(dir.to_str().unwrap(), &settings, None).unwrap());
        assert_eq!(plugin.manifest.id, name);
        Registered::of(&PluginRegistration {
            plugin,
            key: None,
            slot: position.unwrap_or(PanelSlot::Side),
            position,
        })
    }

    /// A panel over `registered` whose host sends through `port`, its pump made directly rather than through
    /// `Pump::start`: no wake is watched, so nothing runs on a main loop.
    fn panel(registered: &Registered, port: Rc<LazyPort>, build: Option<BuildPage>) -> Panel {
        panel_waking(registered, port, build, std::sync::Arc::new(|| {}))
    }

    /// [`panel`], its host raising `wake` where the window's watch would run its pump.
    fn panel_waking(
        registered: &Registered,
        port: Rc<LazyPort>,
        build: Option<BuildPage>,
        wake: agent::UiWake,
    ) -> Panel {
        let host = Rc::new(PluginHost::new(
            registered.plugin.clone(),
            port.clone(),
            wake,
            None,
            Vec::new(),
        ));
        let pump = Rc::new_cyclic(|me| Pump {
            host,
            wake: PanelWake::new().unwrap(),
            timer: RefCell::new(None),
            me: me.clone(),
            saving: RefCell::default(),
            paused: RefCell::default(),
            restoring: RefCell::default(),
            after_restore: RefCell::default(),
            events: RefCell::default(),
            adopting: Cell::new(false),
            view: Rc::default(),
        });
        Panel::new(registered, Some(pump), port, build, Rc::default())
    }

    fn plugin_panels(panels: Vec<Panel>) -> PluginPanels {
        PluginPanels {
            panels: RefCell::new(panels.into_iter().map(Rc::new).collect()),
            facts: RefCell::new(WindowFacts::new(PageTheme::fallback())),
            follow_totals: RefCell::default(),
        }
    }

    #[test]
    fn the_own_windows_first_hello_says_own_window_true() {
        let scratch = Scratch::new("panels-own-window");
        let demo = registered(&scratch, "demo", false, None);
        let other = registered(&scratch, "other", false, None);
        let (demo_panel, demo_pages, _) = counting_panel(&demo);
        let (other_panel, other_pages, _) = counting_panel(&other);
        let panels = plugin_panels(vec![demo_panel, other_panel]);
        panels.set_own_window(Some("demo"));
        for (id, pages, own) in [("demo", demo_pages, true), ("other", other_pages, false)] {
            let module = ModuleId::lua(id);
            panels.build_page(&module, &panels.theme());
            let host = panels.panel(&module).unwrap().pump.as_ref().unwrap().host.clone();
            host.page_committed();
            host.page_message(r#"{"v":1,"id":1,"verb":"host.hello","args":{}}"#, Instant::now());
            assert_eq!(pages.borrow()[0].1.borrow()[0]["result"]["own_window"], own);
        }
        panels.set_own_window(None);
        assert!(panels.hosts().all(|(_, pump)| !pump.host.own_window()));
    }

    /// A set of colours that differs from the fallback the hosts start with, so setting it sends something.
    fn tokens(background: &str, fg: u32, bg: u32) -> ThemeTokens {
        use eitri_core::theme::payload::{HlAttrs, NvimOptions, NvimThemePayload, PAYLOAD_VERSION};
        ThemeTokens::derive(&NvimThemePayload {
            v: PAYLOAD_VERSION,
            groups: [(
                "Normal".to_owned(),
                HlAttrs {
                    fg: Some(fg),
                    bg: Some(bg),
                    reverse: false,
                },
            )]
            .into_iter()
            .collect(),
            options: NvimOptions {
                background: background.to_owned(),
                guifont: String::new(),
                colors_name: format!("{background}-test"),
                cmdheight: None,
                start_scale: None,
            },
        })
    }

    #[test]
    fn set_theme_reaches_every_host() {
        let scratch = Scratch::new("panels-theme");
        let mut panels = Vec::new();
        let mut pages = Vec::new();
        for name in ["one", "two"] {
            let page = Rc::new(Recorded::default());
            let port = Rc::new(LazyPort::default());
            *port.0.borrow_mut() = Some(page.clone());
            panels.push(panel(&registered(&scratch, name, false, None), port, None));
            pages.push(page);
        }
        let plugin_panels = plugin_panels(panels);
        // A light set: the hosts start from the dark fallback, which would send nothing.
        let light = tokens("light", 0x575279, 0xfaf4ed);
        plugin_panels.set_theme(&light);
        for page in &pages {
            assert_eq!(*page.0.borrow(), ["set_theme", "theme"]);
        }
        assert_eq!(plugin_panels.theme(), PageTheme::from_tokens(&light));
    }

    /// The editor's text size reaches every host as `--nv-text-size`, survives a colourscheme change, goes away
    /// again on `None`, and an unchanged size sends nothing.
    #[test]
    fn the_editors_text_size_rides_every_theme_and_only_a_change_is_sent() {
        let scratch = Scratch::new("panels-text-size");
        let page = Rc::new(Recorded::default());
        let port = Rc::new(LazyPort::default());
        *port.0.borrow_mut() = Some(page.clone());
        let registration = registered(&scratch, "one", false, None);
        let panel = panel(&registration, port, None);
        let host = panel.pump.as_ref().unwrap().host.clone();
        let panels = plugin_panels(vec![panel]);
        let size = |theme: &PageTheme| {
            theme
                .vars()
                .iter()
                .find(|(name, _)| name == "--nv-text-size")
                .map(|(_, value)| value.clone())
        };
        assert_eq!(size(&host.theme()), None, "no size until the window gives one");

        panels.set_text_size(Some(56.0 / 3.0 / 1.5));
        assert_eq!(size(&host.theme()).as_deref(), Some("12.44px"));
        assert_eq!(
            size(&panels.theme()).as_deref(),
            Some("12.44px"),
            "a page built later starts with it"
        );
        assert_eq!(*page.0.borrow(), ["set_theme", "theme"]);

        panels.set_text_size(Some(56.0 / 3.0 / 1.5));
        assert_eq!(*page.0.borrow(), ["set_theme", "theme"], "the same size sends nothing");

        let light = tokens("light", 0x575279, 0xfaf4ed);
        panels.set_theme(&light);
        assert_eq!(
            size(&host.theme()).as_deref(),
            Some("12.44px"),
            "a colourscheme change keeps it"
        );
        assert_eq!(
            host.theme(),
            PageTheme::from_tokens(&light).with_text_size(Some(56.0 / 3.0 / 1.5))
        );

        panels.set_text_size(None);
        assert_eq!(host.theme(), PageTheme::from_tokens(&light), "taken away again");
        panels.set_theme(&light);
        assert_eq!(size(&host.theme()), None);
    }

    /// A payload that moves the guifont and the colourscheme together is one `theme` event to a page, not two (each
    /// one re-renders the page and its diagrams): the size is stored, and the colour call sends both.
    #[test]
    fn a_size_stored_before_a_theme_goes_out_with_it_in_one_event() {
        let scratch = Scratch::new("panels-text-size-one-event");
        let page = Rc::new(Recorded::default());
        let port = Rc::new(LazyPort::default());
        *port.0.borrow_mut() = Some(page.clone());
        let registration = registered(&scratch, "one", false, None);
        let panel = panel(&registration, port, None);
        let host = panel.pump.as_ref().unwrap().host.clone();
        let panels = plugin_panels(vec![panel]);
        let light = tokens("light", 0x575279, 0xfaf4ed);
        panels.store_text_size(Some(24.0));
        assert!(page.0.borrow().is_empty(), "storing sends nothing");
        panels.set_theme(&light);
        assert_eq!(*page.0.borrow(), ["set_theme", "theme"], "one event for both");
        assert_eq!(host.theme(), PageTheme::from_tokens(&light).with_text_size(Some(24.0)));
    }

    #[test]
    fn an_away_holding_gets_the_current_theme_before_its_first_hello_here() {
        let scratch = Scratch::new("away-page-theme");
        let registration = registered(&scratch, "demo", true, None);
        let panel = panel(&registration, Rc::default(), None);
        let host = panel.pump.as_ref().unwrap().host.clone();
        let panels = plugin_panels(vec![panel]);
        panels.mark_away(&demo());
        let light = tokens("light", 0x575279, 0xfaf4ed);
        panels.set_theme(&light);
        panels.activate_holding(&demo());
        assert_eq!(host.theme(), PageTheme::from_tokens(&light));
    }

    #[test]
    fn a_page_that_arrives_never_built_answers_its_commands_here_and_stays_unbuilt() {
        let scratch = Scratch::new("unbuilt-page-held");
        let registration = registered(&scratch, "demo", true, None);
        let builds = Rc::new(Cell::new(0));
        let build: BuildPage = {
            let builds = builds.clone();
            Rc::new(move |_: &PageTheme| {
                builds.set(builds.get() + 1);
                None
            })
        };
        let panels = plugin_panels(vec![panel(&registration, Rc::default(), Some(build))]);
        let heard: Rc<RefCell<Vec<(String, bool)>>> = Rc::default();
        {
            let heard = heard.clone();
            panels.set_holder_hook(Rc::new(move |owner, held| {
                heard.borrow_mut().push((owner.to_owned(), held))
            }));
        }
        // It left for the other window and comes back still never shown.
        panels.forget(&demo());
        assert!(panels.follower("demo").is_none(), "the other window holds it");
        panels.activate_holding(&demo());
        assert!(
            panels.follower("demo").is_some(),
            "its follow lines and commands are answered here again"
        );
        assert_eq!(
            *heard.borrow(),
            [("demo".to_owned(), false), ("demo".to_owned(), true)],
            "the editor's window routes its lines to the holder each time"
        );
        assert_eq!(builds.get(), 0, "it is built on its first show, not by being held");
        assert!(!panels.is_built(&demo()));
    }

    /// A build that records each page it makes, and how many it made.
    fn counting_build() -> (BuildPage, Rc<RefCell<Vec<Rc<Recorded>>>>) {
        let pages: Rc<RefCell<Vec<Rc<Recorded>>>> = Rc::default();
        let build: BuildPage = {
            let pages = pages.clone();
            Rc::new(move |_: &PageTheme| {
                let page = Rc::new(Recorded::default());
                pages.borrow_mut().push(page.clone());
                Some(page as Rc<dyn PagePort>)
            })
        };
        (build, pages)
    }

    #[test]
    fn a_page_that_left_holds_no_view_and_is_built_afresh_on_its_return() {
        let scratch = Scratch::new("panels-built-afresh");
        let registered = registered(&scratch, "demo", false, None);
        let (build, pages) = counting_build();
        let panels = plugin_panels(vec![panel(&registered, Rc::default(), Some(build))]);
        panels.build_page(&demo(), &panels.theme());
        assert_eq!(pages.borrow().len(), 1);

        panels.set_on_screen(&demo(), true);
        panels.forget(&demo());
        assert!(!panels.is_built(&demo()), "the window it left holds no page");
        assert!(panels.webview(&demo()).is_none());
        assert_eq!(
            panels.panel(&demo()).unwrap().on_screen.get(),
            None,
            "whether it is drawn here is told afresh when it comes back"
        );
        let heard = pages.borrow()[0].0.borrow().len();
        panels.set_theme(&tokens("light", 0x575279, 0xfaf4ed));
        assert_eq!(
            pages.borrow()[0].0.borrow().len(),
            heard,
            "the page that left hears nothing more from this window"
        );

        panels.prepare(&demo());
        assert_eq!(
            pages.borrow().len(),
            2,
            "its return builds a new page, as a first arrival does"
        );
        assert!(panels.is_built(&demo()));
        assert_eq!(
            *pages.borrow()[1].0.borrow(),
            Vec::<String>::new(),
            "the new page starts from its own hello"
        );
        panels.prepare(&demo());
        panels.build_page(&demo(), &panels.theme());
        assert_eq!(pages.borrow().len(), 2, "a page that has its view is not built again");
    }

    #[test]
    fn reload_page_back_builds_a_page_that_has_no_view() {
        let scratch = Scratch::new("panels-reload-back");
        let registered = registered(&scratch, "demo", true, None);
        let (build, pages) = counting_build();
        let panels = plugin_panels(vec![panel(&registered, Rc::default(), Some(build))]);
        let lua_pages: crate::gtk_host_services::LuaPages = Rc::default();
        assert!(
            crate::linked_window::reload_page_back(&panels, &lua_pages, &demo()),
            "a page never built here is built from its store as it is"
        );
        assert_eq!(pages.borrow().len(), 1);
        panels.forget(&demo());
        assert!(panels.follower("demo").is_none());
        assert!(crate::linked_window::reload_page_back(&panels, &lua_pages, &demo()));
        assert_eq!(pages.borrow().len(), 2, "a page whose view was dropped is built again");
        assert!(panels.is_built(&demo()));
        assert!(
            panels.follower("demo").is_some(),
            "the returned page is held here again"
        );
        let host = panels.panel(&demo()).unwrap().pump.as_ref().unwrap().host.clone();
        host.page_committed();
        host.page_message(r#"{"v":1,"id":7,"verb":"host.hello","args":{}}"#, Instant::now());
        let pages = pages.borrow();
        let replies = pages[1].1.borrow();
        assert_eq!(replies.len(), 1, "the returned page's hello is answered once");
        assert_eq!(replies[0]["re"], 7);
        assert_eq!(replies[0]["ok"], true);
        assert!(
            !crate::linked_window::reload_page_back(&panels, &lua_pages, &ModuleId::lua("other")),
            "a module that is no plugin's and no Lua panel's loads nothing"
        );
    }

    fn counting_panel(registered: &Registered) -> (Panel, Rc<RefCell<Vec<Rc<Recorded>>>>, Rc<Cell<usize>>) {
        let (build, pages) = counting_build();
        let loads = Rc::new(Cell::new(0));
        let port = Rc::new(LazyPort::default());
        let mut panel = panel(
            registered,
            port.clone(),
            Some({
                let loads = loads.clone();
                Rc::new(move |theme| {
                    loads.set(loads.get() + 1);
                    build(theme)
                })
            }),
        );
        panel.load = {
            let loads = loads.clone();
            Rc::new(move |_| {
                if port.page().is_none() {
                    return false;
                }
                loads.set(loads.get() + 1);
                true
            })
        };
        (panel, pages, loads)
    }

    #[test]
    fn pair_returns_reopen_follow_commands_and_hello_before_show() {
        for event in pair_returns() {
            for retained in [false, true] {
                let scratch = Scratch::new("panels-pair-return");
                let registered = registered(&scratch, "demo", true, None);
                let (panel, pages, loads) = counting_panel(&registered);
                let panels = plugin_panels(vec![panel]);
                panels.build_page(&demo(), &panels.theme());
                if retained {
                    panels.mark_away(&demo());
                } else {
                    panels.forget(&demo());
                }
                assert!(panels
                    .route_follow(batch(vec![cmd("demo", FollowCmd::Show)]), Instant::now())
                    .is_empty());
                let before = loads.get();
                let (mut controller, returned) = pair_return(event.clone(), true, retained);
                apply_return_holds(&panels, &returned);
                assert_eq!(
                    panels.route_follow(batch(vec![cmd("demo", FollowCmd::Show)]), Instant::now()),
                    [(demo(), FollowCmd::Show)],
                    "{event:?}, retained={retained}: commands work before the page is shown"
                );
                assert_eq!(loads.get(), before + usize::from(retained));
                assert_eq!(panels.is_built(&demo()), retained, "Hold never builds a missing view");

                let hello = || {
                    let host = panels.panel(&demo()).unwrap().pump.as_ref().unwrap().host.clone();
                    host.page_committed();
                    host.page_message(r#"{"v":1,"id":9,"verb":"host.hello","args":{}}"#, Instant::now());
                    let pages = pages.borrow();
                    let replies = pages.last().unwrap().1.borrow();
                    assert_eq!(
                        replies.len(),
                        1,
                        "{event:?}, retained={retained}: hello is answered once"
                    );
                    assert_eq!(replies[0]["re"], 9);
                    assert_eq!(replies[0]["ok"], true);
                };
                if retained {
                    hello();
                }
                let shown = controller.lua_request(WinId::MAIN, eitri_core::lua::layout::LayoutRequest::Show(demo()));
                apply_return_loads(&panels, &returned);
                apply_return_loads(&panels, &shown);
                assert_eq!(loads.get(), before + 1, "one load for the return and show together");
                if !retained {
                    hello();
                }
            }
        }
    }

    fn pair_returns() -> [PairEvent; 2] {
        [
            PairEvent::LinkEnded {
                win: WinId::MAIN,
                who: "process 7".into(),
            },
            PairEvent::RestoreFailed { win: WinId::MAIN },
        ]
    }

    fn pair_return(event: PairEvent, hidden: bool, built: bool) -> (eitri_core::wm::Controller, Batch) {
        use eitri_core::layout::{hide, Frame, Layout, ModuleDecl, Placement, Size, Workspaces};
        use eitri_core::wm::{fake::FakeScheduler, module::SurfaceDecl, Controller, EditorPresence, SurfaceKind};
        use eitri_core::wm::{WindowParts, WindowRole, WindowSpec};
        let mut layout = Layout::initial(&[ModuleDecl {
            id: demo(),
            placement: Placement::RightOfRoot,
        }])
        .unwrap();
        if hidden {
            hide(&mut layout, &demo(), &Frame::new(Size { w: 1200, h: 800 }, 1)).unwrap();
        }
        layout.set_away(&demo(), true).unwrap();
        let mut controller = Controller::new(FakeScheduler::new());
        let host = eitri_core::wm::fake::FakeHost::new();
        host.set_grid_size(Size { w: 1200, h: 800 });
        host.set_page_built(demo(), built);
        controller.set_reads(host);
        controller.open_window(
            WinId::MAIN,
            WindowSpec {
                role: WindowRole::Main,
                presence: EditorPresence::Owned,
                title: "project".into(),
            },
            WindowParts::new(layout, Workspaces::single(), None),
        );
        controller.first_apply(
            WinId::MAIN,
            &[SurfaceDecl {
                module: demo(),
                kind: SurfaceKind::PluginPage { plugin: "demo".into() },
                deferred: hidden,
            }],
        );
        let returned = controller.handle(HostEvent::Pair(event));
        (controller, returned)
    }

    fn apply_return_holds(panels: &PluginPanels, returned: &Batch) {
        for op in returned.ops() {
            if let HostOp::Plugin(PluginOp::Hold { module, .. }) = op {
                panels.activate_holding(module);
            }
        }
    }

    fn apply_return_loads(panels: &PluginPanels, returned: &Batch) {
        for op in returned.ops() {
            match op {
                HostOp::Plugin(PluginOp::Build { module, .. }) => {
                    panels.build_page(module, &panels.theme());
                }
                HostOp::Pair(PairOp::ReloadPage { module, .. }) => {
                    assert!(crate::linked_window::reload_page_back(panels, &Rc::default(), module));
                }
                _ => {}
            }
        }
    }

    #[test]
    fn pair_returns_hold_a_never_shown_page_without_building_it() {
        for event in pair_returns() {
            let scratch = Scratch::new("panels-pair-unbuilt");
            let registered = registered(&scratch, "demo", true, None);
            let (panel, pages, loads) = counting_panel(&registered);
            let panels = plugin_panels(vec![panel]);
            panels.mark_away(&demo());
            let (mut controller, returned) = pair_return(event.clone(), true, false);
            apply_return_holds(&panels, &returned);
            assert_eq!(
                panels.route_follow(batch(vec![cmd("demo", FollowCmd::Show)]), Instant::now()),
                [(demo(), FollowCmd::Show)],
                "{event:?}: a page can receive its first show command as soon as it returns"
            );
            assert_eq!(loads.get(), 0);
            assert!(pages.borrow().is_empty());
            assert!(!panels.is_built(&demo()));
            apply_return_loads(&panels, &returned);
            assert_eq!(loads.get(), 0, "{event:?}: a hidden return keeps its page unbuilt");
            assert!(!panels.is_built(&demo()));
            let shown = controller.lua_request(WinId::MAIN, eitri_core::lua::layout::LayoutRequest::Show(demo()));
            apply_return_loads(&panels, &shown);
            assert_eq!(loads.get(), 1);
        }
    }

    #[test]
    fn pair_returns_hold_loads_a_kept_view_once_even_with_reload_at_show() {
        for event in pair_returns() {
            for hidden in [false, true] {
                let scratch = Scratch::new("panels-pair-reload-show");
                let registered = registered(&scratch, "demo", true, None);
                let (panel, pages, loads) = counting_panel(&registered);
                let panels = plugin_panels(vec![panel]);
                panels.build_page(&demo(), &panels.theme());
                panels.mark_away(&demo());
                let (mut controller, returned) = pair_return(event.clone(), hidden, true);
                apply_return_holds(&panels, &returned);
                assert_eq!(loads.get(), 2, "{event:?}: Hold reloads the kept view at return");
                let shown = controller.lua_request(WinId::MAIN, eitri_core::lua::layout::LayoutRequest::Show(demo()));
                assert!(returned.ops().chain(shown.ops()).any(|op| matches!(op,
                    HostOp::Pair(PairOp::ReloadPage { module, .. }) if module == &demo()
                )));
                apply_return_loads(&panels, &returned);
                apply_return_loads(&panels, &shown);
                assert_eq!(
                    loads.get(),
                    2,
                    "{event:?}: ReloadPage reuses the document Hold just loaded"
                );
                assert_eq!(pages.borrow().len(), 1, "the kept view is reused");
            }
        }
    }

    #[test]
    fn reload_page_back_reopens_a_built_page_marked_away_at_startup() {
        let scratch = Scratch::new("panels-built-away-back");
        let registered = registered_with(&scratch, "demo", true, None, |text| {
            text.replace("start = \"hidden\"", "start = \"shown\"")
        });
        assert!(!registered.hidden_start);
        let (panel, pages, loads) = counting_panel(&registered);
        let panels = plugin_panels(vec![panel]);
        panels.build_page(&demo(), &panels.theme());
        panels.mark_away(&demo());
        assert!(panels.follower("demo").is_none());
        assert!(crate::linked_window::reload_page_back(&panels, &Rc::default(), &demo()));
        assert_eq!(loads.get(), 2, "one initial load and one return load");
        assert_eq!(pages.borrow().len(), 1, "the retained view is reused");
        let host = panels.panel(&demo()).unwrap().pump.as_ref().unwrap().host.clone();
        host.page_committed();
        host.page_message(r#"{"v":1,"id":8,"verb":"host.hello","args":{}}"#, Instant::now());
        let pages = pages.borrow();
        let replies = pages[0].1.borrow();
        assert_eq!(replies.len(), 1, "the returned document's hello is answered once");
        assert_eq!(replies[0]["re"], 8);
        assert_eq!(replies[0]["ok"], true);
        assert!(panels.follower("demo").is_some());
    }

    #[test]
    fn reload_page_back_does_not_load_again_after_the_grab_built_or_reactivated_it() {
        for retained in [false, true] {
            let scratch = Scratch::new("panels-grab-reload-back");
            let registered = registered(&scratch, "demo", false, None);
            let (panel, _pages, loads) = counting_panel(&registered);
            let panels = plugin_panels(vec![panel]);
            panels.build_page(&demo(), &panels.theme());
            if retained {
                panels.mark_away(&demo());
            } else {
                panels.forget(&demo());
            }
            let before = loads.get();
            panels.activate_holding(&demo());
            panels.build_page(&demo(), &panels.theme());
            assert_eq!(loads.get(), before + 1);
            assert!(crate::linked_window::reload_page_back(&panels, &Rc::default(), &demo()));
            assert_eq!(loads.get(), before + 1, "the same change loads the page only once");
        }
    }

    #[test]
    fn a_commit_during_adoption_leaves_the_reload_to_resume_the_host() {
        let scratch = Scratch::new("panels-commit-during-adoption");
        let registered = registered(&scratch, "demo", false, None);
        let panel = panel(&registered, Rc::default(), None);
        let pump = panel.pump.as_ref().unwrap();
        pump.host.accept_moved();
        pump.adopting.set(true);
        let (tx, rx) = std::sync::mpsc::channel();
        *pump.restoring.borrow_mut() = Some(rx);
        let reloaded = Rc::new(Cell::new(false));
        {
            let reloaded = reloaded.clone();
            *pump.after_restore.borrow_mut() = Some(Box::new(move || reloaded.set(true)));
        }

        pump.committed();
        let runner = eitri_core::plugin::tasks::TaskRunner::new(std::sync::Arc::new(|| {}));
        assert!(tx.send(Ok(runner)).is_ok());
        pump.moving(Instant::now());
        assert!(reloaded.get());
        pump.committed();
        pump.host
            .page_message(r#"{"v":1,"id":1,"verb":"host.hello","args":{}}"#, Instant::now());
        assert!(pump.host.hello_document().is_some(), "the reload's hello must succeed");
    }

    #[test]
    fn a_ready_is_posted_only_while_the_document_that_said_it_runs() {
        let scratch = Scratch::new("panels-ready-document");
        let registered = registered(&scratch, "demo", false, None);
        let panel = panel(&registered, Rc::default(), None);
        let pump = panel.pump.clone().unwrap();
        let posted: Rc<RefCell<Vec<ModuleId>>> = Rc::default();
        {
            let posted = posted.clone();
            *pump.events.borrow_mut() = Some(Rc::new(move |event| {
                if let eitri_core::wm::HostEvent::Plugin(eitri_core::wm::PluginEvent::Ready { module, .. }) = event {
                    posted.borrow_mut().push(module);
                }
            }));
        }
        let hello = || {
            let text = serde_json::json!({"v": 1, "id": 1, "verb": "host.hello", "args": {}}).to_string();
            pump.host.page_message(&text, Instant::now());
        };
        pump.post_ready();
        assert!(
            posted.borrow().is_empty(),
            "a page that has not said hello is not ready"
        );

        pump.host.page_committed();
        hello();
        let first = pump.host.hello_document().unwrap();
        pump.post_ready();
        assert_eq!(
            *posted.borrow(),
            [demo()],
            "a view that draws no frames posts its ready at once"
        );

        // The ready a document said, posted after the document was replaced: by a commit, by an arrival, by the
        // page leaving.
        pump.host.page_committed();
        pump.post_ready_of(first);
        hello();
        pump.post_ready_of(first);
        let second = pump.host.hello_document().unwrap();
        pump.host.accept_moved();
        pump.post_ready_of(second);
        // The arriving document answers once the move is over.
        pump.host.resume_moved();
        hello();
        let third = pump.host.hello_document().unwrap();
        pump.host.forget_moved();
        pump.post_ready_of(third);
        assert_eq!(
            posted.borrow().len(),
            1,
            "no ready of a document that is gone is posted"
        );

        // Back again: the document that arrives says hello, and its own ready is posted.
        pump.host.accept_moved();
        pump.host.resume_moved();
        hello();
        pump.post_ready_of(pump.host.hello_document().unwrap());
        assert_eq!(posted.borrow().len(), 2, "the running document's ready is posted");
    }

    #[test]
    fn every_plugin_page_is_hosted_in_a_page_slot() {
        use eitri_core::source_scan as scan;
        let source = scan::read_required("shell/src/plugin_panels.rs");
        let code = scan::code_only(&source.text);
        let code = scan::without(&code, &scan::modules(&code, "tests"));
        let install = scan::functions(&code, "install");
        assert_eq!(install.len(), 1);
        let install: String = install[0].split_whitespace().collect();
        assert!(install.contains("Self::make_panel(engine,keymap,theme,&registration,&keys_settings)?"));
        let join = scan::functions(&code, "join");
        assert_eq!(join.len(), 1);
        let join: String = join[0].split_whitespace().collect();
        assert!(join.contains("Self::make_panel(engine,keymap,&self.theme(),&registration,&keys_settings)"));
        let factory = scan::functions(&code, "make_panel");
        assert_eq!(factory.len(), 1);
        let install: String = factory[0].split_whitespace().collect();
        assert_eq!(
            install.matches("page_slot::PageSlot::new()").count(),
            1,
            "one slot per plugin, whatever its start: {install}"
        );
        assert!(install.contains("letslot_widget=page_slot::PageSlot::new();"));
        assert!(install.contains("slot_widget.set_page(&view);"));
        assert!(
            install.contains("(slot_widget.upcast::<gtk4::Widget>(),Some(pump),Some(build))"),
            "the grid is given the slot and the panel keeps its build"
        );
        assert!(
            !install.contains("view.upcast::<gtk4::Widget>()"),
            "never the view itself"
        );
    }

    #[test]
    fn a_start_hidden_plugin_has_no_webview_until_its_first_show() {
        let scratch = Scratch::new("panels-lazy");
        let registered = registered(&scratch, "demo", true, None);
        assert!(registered.hidden_start);
        let page = Rc::new(Recorded::default());
        let builds: Rc<RefCell<Vec<PageTheme>>> = Rc::default();
        let build: BuildPage = {
            let page = page.clone();
            let builds = builds.clone();
            Rc::new(move |theme: &PageTheme| {
                builds.borrow_mut().push(theme.clone());
                Some(page.clone() as Rc<dyn PagePort>)
            })
        };
        let module = ModuleId::lua("demo");
        let panels = plugin_panels(vec![panel(&registered, Rc::default(), Some(build))]);
        assert!(!panels.is_built(&module));
        assert!(panels.webview(&module).is_none());

        let light = tokens("light", 0x575279, 0xfaf4ed);
        panels.set_theme(&light);
        assert!(
            page.0.borrow().is_empty(),
            "what the host sent before the page existed went nowhere"
        );
        assert!(builds.borrow().is_empty());

        panels.build_page(&module, &panels.theme());
        assert_eq!(
            *builds.borrow(),
            [PageTheme::from_tokens(&light)],
            "built once, in the window's theme"
        );
        assert!(panels.is_built(&module));

        panels.set_theme(&tokens("dark", 0xe0def4, 0x191724));
        assert_eq!(
            *page.0.borrow(),
            ["set_theme", "theme"],
            "the built page hears the host"
        );

        panels.build_page(&module, &panels.theme());
        assert_eq!(builds.borrow().len(), 1, "the page is built once");
        // A module that is not a plugin's: nothing to build, and nothing breaks.
        panels.build_page(&ModuleId::lua("other"), &panels.theme());
        assert!(!panels.is_built(&ModuleId::lua("other")));
    }

    #[test]
    fn a_start_hidden_plugin_answers_its_rule_and_kind() {
        let scratch = Scratch::new("panels-rule");
        let hidden = registered(&scratch, "hidden", true, None);
        let placed = registered(&scratch, "placed", true, Some(PanelSlot::Bottom));
        let plain = registered(&scratch, "plain", false, None);
        let side = registered(&scratch, "side", false, Some(PanelSlot::Main));
        let expected: Vec<(bool, Option<OpenRule>)> = [&hidden, &placed, &plain, &side]
            .iter()
            .map(|r| (r.hidden_start, r.own_rule.clone()))
            .collect();
        let panels = plugin_panels(
            [hidden, placed, plain, side]
                .iter()
                .map(|r| panel(r, Rc::default(), None))
                .collect(),
        );
        let answered: Vec<(bool, Option<OpenRule>)> = ["hidden", "placed", "plain", "side"]
            .iter()
            .map(|name| {
                let id = ModuleId::lua(name);
                (panels.starts_hidden(&id), panels.own_rule(&id))
            })
            .collect();
        assert_eq!(answered, expected, "as the registration says");
        use eitri_core::layout::rules::At;
        use eitri_core::layout::{Direction, Placement};
        assert_eq!(
            answered,
            [
                (
                    true,
                    Some(OpenRule {
                        at: Some(At::Beside(Direction::Right)),
                        size: Some(eitri_core::layout::rules::RuleSize::Share(0.4)),
                        focus: false,
                        float: false,
                        scratchpad: false,
                    })
                ),
                (
                    true,
                    Some(OpenRule {
                        at: Some(At::Placement(Placement::BelowRoot)),
                        size: None,
                        focus: false,
                        float: false,
                        scratchpad: false,
                    })
                ),
                (false, None),
                (
                    false,
                    Some(OpenRule {
                        at: Some(At::Placement(Placement::InPlaceOfEditor)),
                        size: None,
                        focus: true,
                        float: false,
                        scratchpad: false,
                    })
                ),
            ]
        );
        // Not a plugin's module at all.
        assert!(!panels.starts_hidden(&ModuleId::lua("ghost")));
        assert_eq!(panels.own_rule(&ModuleId::lua("ghost")), None);
    }

    fn cmd(owner: &str, cmd: FollowCmd) -> Ordered {
        Ordered::Cmd {
            owner: owner.to_owned(),
            cmd,
            buf: (cmd == FollowCmd::Auto).then_some(3),
        }
    }

    fn batch(ordered: Vec<Ordered>) -> FollowBatch {
        FollowBatch {
            ordered_received: ordered.len() as u64,
            ordered,
            ..FollowBatch::default()
        }
    }

    fn demo() -> ModuleId {
        ModuleId::lua("demo")
    }

    /// `demo` (follows, defines commands, starts hidden) and `other` (neither), each with a page that records.
    fn demo_and_other(scratch: &Scratch, auto: bool) -> (PluginPanels, Rc<Recorded>) {
        let demo = registered_with(scratch, "demo", true, None, |text| {
            if auto {
                text.replacen(
                    "auto = { default = false, kind = \"bool\" }",
                    "auto = { default = true, kind = \"bool\" }",
                    1,
                )
            } else {
                text
            }
        });
        let other = registered(scratch, "other", false, None);
        let page = Rc::new(Recorded::default());
        let port = Rc::new(LazyPort::default());
        *port.0.borrow_mut() = Some(page.clone());
        let panels = plugin_panels(vec![panel(&demo, port, None), panel(&other, Rc::default(), None)]);
        (panels, page)
    }

    #[test]
    fn commands_come_back_in_order_and_auto_only_where_its_setting_is_on() {
        let lines = || {
            batch(vec![
                cmd("demo", FollowCmd::Toggle),
                cmd("ghost", FollowCmd::Show),
                cmd("demo", FollowCmd::Auto),
                cmd("other", FollowCmd::Show),
                cmd("demo", FollowCmd::Hide),
                Ordered::Gone {
                    owner: "demo".into(),
                    buf: 3,
                },
            ])
        };
        let now = Instant::now();

        let scratch = Scratch::new("panels-follow-cmds");
        let (panels, _) = demo_and_other(&scratch, false);
        assert!(panels.wants_follow_feed());
        assert_eq!(
            panels.route_follow(lines(), now),
            [(demo(), FollowCmd::Toggle), (demo(), FollowCmd::Hide)],
            "an unknown owner, an owner without the capability and an auto show that is off are dropped"
        );

        let scratch = Scratch::new("panels-follow-auto");
        let (panels, _) = demo_and_other(&scratch, true);
        assert_eq!(
            panels.route_follow(lines(), now),
            [
                (demo(), FollowCmd::Toggle),
                (demo(), FollowCmd::Auto),
                (demo(), FollowCmd::Hide)
            ],
            "an automatic show passes where its setting is on, still marked automatic"
        );

        // A window whose plugins neither follow nor define commands binds no socket.
        let scratch = Scratch::new("panels-follow-none");
        let other = registered(&scratch, "other", false, None);
        let panels = plugin_panels(vec![panel(&other, Rc::default(), None)]);
        assert!(!panels.wants_follow_feed());
    }

    /// The window as the commands see it: a start-hidden module whose show builds its page, as the grid's change
    /// hook does.
    struct RecordingWindow {
        panels: Weak<PluginPanels>,
        shown: Cell<bool>,
        /// Shown but kept off screen (behind a zoom, on a tab not drawn) until a show brings it forward.
        covered: Cell<bool>,
        calls: RefCell<Vec<&'static str>>,
    }

    impl FollowWindow for RecordingWindow {
        fn is_shown(&self, module: &ModuleId) -> bool {
            *module == demo() && self.shown.get()
        }

        fn is_on_screen(&self, module: &ModuleId) -> bool {
            self.is_shown(module) && !self.covered.get()
        }

        fn show(&self, module: &ModuleId) {
            self.calls.borrow_mut().push("show");
            self.shown.set(true);
            self.covered.set(false);
            let panels = self.panels.upgrade().expect("the window still holds its panels");
            panels.build_page(module, &panels.theme());
        }

        fn hide(&self, _module: &ModuleId) {
            self.calls.borrow_mut().push("hide");
            self.shown.set(false);
        }

        fn command(&self, _module: &ModuleId, _cmd: FollowCmd) {
            self.calls.borrow_mut().push("command");
        }
    }

    #[test]
    fn toggles_in_one_batch_apply_in_order_and_build_one_page() {
        let scratch = Scratch::new("panels-follow-toggles");
        let registered = registered(&scratch, "demo", true, None);
        assert!(registered.hidden_start);
        let builds = Rc::new(Cell::new(0));
        let build: BuildPage = {
            let builds = builds.clone();
            Rc::new(move |_: &PageTheme| {
                builds.set(builds.get() + 1);
                Some(Rc::new(Recorded::default()) as Rc<dyn PagePort>)
            })
        };
        let panels = Rc::new(plugin_panels(vec![panel(&registered, Rc::default(), Some(build))]));
        let window = RecordingWindow {
            panels: Rc::downgrade(&panels),
            shown: Cell::new(false),
            covered: Cell::new(false),
            calls: RefCell::default(),
        };
        let toggles = panels.route_follow(batch(vec![cmd("demo", FollowCmd::Toggle); 5]), Instant::now());
        assert_eq!(toggles.len(), 5);
        apply_follow_commands(&window, toggles);
        assert_eq!(*window.calls.borrow(), ["show", "hide", "show", "hide", "show"]);
        assert!(window.is_shown(&demo()), "an odd count leaves it shown");
        assert_eq!(builds.get(), 1, "one page");
        assert!(panels.is_built(&demo()));

        window.calls.borrow_mut().clear();
        apply_follow_commands(&window, vec![(demo(), FollowCmd::Show)]);
        assert!(window.calls.borrow().is_empty(), "already shown");
        apply_follow_commands(&window, vec![(demo(), FollowCmd::Hide), (demo(), FollowCmd::Hide)]);
        assert_eq!(*window.calls.borrow(), ["hide"]);
        assert_eq!(builds.get(), 1);
    }

    #[test]
    fn a_module_shown_while_agents_wait_is_shown_when_hidden_and_hidden_when_shown() {
        let scratch = Scratch::new("panels-wait-show");
        let registered = registered(&scratch, "demo", true, None);
        let build: BuildPage = Rc::new(|_: &PageTheme| Some(Rc::new(Recorded::default()) as Rc<dyn PagePort>));
        let panels = Rc::new(plugin_panels(vec![panel(&registered, Rc::default(), Some(build))]));
        let window = RecordingWindow {
            panels: Rc::downgrade(&panels),
            shown: Cell::new(false),
            covered: Cell::new(false),
            calls: RefCell::default(),
        };
        apply_wait_show(&window, &demo(), false);
        assert!(window.calls.borrow().is_empty(), "a hidden module stays as it is");
        apply_wait_show(&window, &demo(), true);
        assert_eq!(*window.calls.borrow(), ["show"]);
        assert!(panels.is_built(&demo()), "the first show builds the page");

        // Shown behind a zoom or on a tab not drawn: a show leaves the person's zoom and tab alone.
        window.calls.borrow_mut().clear();
        window.covered.set(true);
        apply_wait_show(&window, &demo(), true);
        assert!(window.calls.borrow().is_empty());
        apply_wait_show(&window, &demo(), false);
        assert_eq!(*window.calls.borrow(), ["hide"], "hidden wherever it is");
    }

    #[test]
    fn only_a_joined_wait_panel_shows_the_cached_wait_on_idle() {
        use eitri_core::agents::{Change, Class, ReaderEvent, Row, View};
        use eitri_core::plugin::host::{AgentsSource, ReaderSink};

        struct WaitingSource;

        impl AgentsSource for WaitingSource {
            fn start(&self, sink: ReaderSink) -> Box<dyn std::any::Any> {
                sink(ReaderEvent::Change(Change::Reset {
                    view: View {
                        rows: vec![Row {
                            class: Class::Needs,
                            ..Row::default()
                        }],
                        ..View::default()
                    },
                }));
                Box::new(())
            }
        }

        let _turn = crate::layout_state::default_context_turn();
        let context = glib::MainContext::default();
        let _owner = context.acquire().unwrap();
        let scratch = Scratch::new("panels-join-wait");
        let registration = registered_with(&scratch, "demo", true, None, |text| {
            text.replace("\"net\"", "\"agents\"")
                .replace(
                    "[net]\norigins = [\"https://learn.example.org\"]\nimages = \"${images}\"\n",
                    "",
                )
                .replace("[module]\n", "[module]\nshown_while = \"agents-wait\"\n")
        });

        for joined in [false, true] {
            let feed = AgentsFeed::new(Box::new(WaitingSource));
            let keep_reading = WaitShow::new(feed.clone(), std::sync::Arc::new(|| {}));
            assert_eq!(keep_reading.pump_at(wall_clock_ms()), Some(true));
            let initial = if joined {
                Vec::new()
            } else {
                vec![panel(&registration, Rc::default(), None)]
            };
            let panels = Rc::new(plugin_panels(initial));
            let window = Rc::new(RecordingWindow {
                panels: Rc::downgrade(&panels),
                shown: Cell::new(false),
                covered: Cell::new(false),
                calls: RefCell::default(),
            });
            panels.record(Fact::Agents, |facts| facts.agents_feed = Some(feed));
            panels.show_while_agents_wait(window.clone());
            if joined {
                panels
                    .join_with(&registration, || Ok(Rc::new(panel(&registration, Rc::default(), None))))
                    .unwrap();
            }
            assert!(window.calls.borrow().is_empty(), "showing must wait for the next idle");
            while context.pending() {
                context.iteration(false);
            }
            if joined {
                assert_eq!(
                    *window.calls.borrow(),
                    ["show"],
                    "the joined module sees the cached wait"
                );
                assert!(window.is_shown(&demo()));
            } else {
                assert!(window.calls.borrow().is_empty(), "startup adds no idle show");
            }
        }
    }

    #[test]
    fn a_shown_module_off_screen_is_brought_on_screen_by_show_and_toggle_but_not_by_auto() {
        let scratch = Scratch::new("panels-follow-covered");
        let registered = registered(&scratch, "demo", true, None);
        let panels = Rc::new(plugin_panels(vec![panel(&registered, Rc::default(), None)]));
        let window = RecordingWindow {
            panels: Rc::downgrade(&panels),
            shown: Cell::new(true),
            covered: Cell::new(true),
            calls: RefCell::default(),
        };
        apply_follow_commands(&window, vec![(demo(), FollowCmd::Auto)]);
        assert!(
            window.calls.borrow().is_empty(),
            "an automatic show leaves a zoom or a tab alone"
        );
        apply_follow_commands(&window, vec![(demo(), FollowCmd::Show)]);
        assert_eq!(*window.calls.borrow(), ["show"]);
        assert!(window.is_on_screen(&demo()));

        window.calls.borrow_mut().clear();
        window.covered.set(true);
        apply_follow_commands(&window, vec![(demo(), FollowCmd::Toggle), (demo(), FollowCmd::Toggle)]);
        assert_eq!(
            *window.calls.borrow(),
            ["show", "hide"],
            "a toggle shows what the reader cannot see"
        );

        window.calls.borrow_mut().clear();
        window.shown.set(true);
        window.covered.set(true);
        apply_follow_commands(&window, vec![(demo(), FollowCmd::Hide)]);
        assert_eq!(*window.calls.borrow(), ["hide"], "a hide hides it wherever it is");
    }

    #[test]
    fn an_automatic_show_routed_from_the_feed_leaves_a_zoomed_preview_alone() {
        let scratch = Scratch::new("panels-follow-auto-zoom");
        let (panels, _) = demo_and_other(&scratch, true);
        let panels = Rc::new(panels);
        let window = RecordingWindow {
            panels: Rc::downgrade(&panels),
            shown: Cell::new(true),
            covered: Cell::new(true),
            calls: RefCell::default(),
        };
        let routed = panels.route_follow(batch(vec![cmd("demo", FollowCmd::Auto)]), Instant::now());
        apply_follow_commands(&window, routed);
        assert!(
            window.calls.borrow().is_empty(),
            "a new buffer must not end the zoom or switch the tab over a preview already shown"
        );

        window.shown.set(false);
        window.covered.set(false);
        let routed = panels.route_follow(batch(vec![cmd("demo", FollowCmd::Auto)]), Instant::now());
        apply_follow_commands(&window, routed);
        assert_eq!(
            *window.calls.borrow(),
            ["show"],
            "a hidden preview opens on a new buffer"
        );
    }

    fn view(owner: &str) -> eitri_core::plugin::follow_feed::ViewLine {
        eitri_core::plugin::follow_feed::ViewLine {
            owner: owner.into(),
            seq: 1,
            buf: 3,
            name: "/w/notes/a.md".into(),
            filetype: "markdown".into(),
            tick: 41,
            current: true,
            line_count: 100,
            top: 1,
            top_fraction: 0.0,
            bottom: 40,
            cursor_line: 1,
            cursor_col: 0,
            cause: eitri_core::plugin::protocol::ViewCause::User,
            token: None,
            scrolloff: None,
            rows: None,
            cursor_row: None,
        }
    }

    /// An editor that counts what it is asked and answers that it is not there.
    #[derive(Default)]
    struct CountingEditor(Cell<usize>);

    impl EditorRpc for CountingEditor {
        fn exec_lua(&self, _code: &'static str, _args: Vec<rmpv::Value>) -> eitri_core::nvim_rpc::Pending {
            self.0.set(self.0.get() + 1);
            eitri_core::nvim_rpc::Pending::failed(eitri_core::nvim_rpc::RpcError::Unavailable("test".into()))
        }

        fn target(&self) -> Option<u64> {
            Some(1)
        }
    }

    #[test]
    fn views_reach_their_host_and_wake_it() {
        // The follow feed adds its reader to glib's default main context, which other tests in this crate own
        // while they run, so this one takes its turn and holds the context for its whole run.
        let _turn = crate::layout_state::default_context_turn();
        let context = glib::MainContext::default();
        let _owner = context
            .acquire()
            .expect("glib's default main context, which the other tests on it leave to this one");
        let scratch = Scratch::new("panels-follow-views");
        let registered = registered(&scratch, "demo", true, None);
        let page = Rc::new(Recorded::default());
        let port = Rc::new(LazyPort::default());
        *port.0.borrow_mut() = Some(page.clone());
        let wakes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let wake: agent::UiWake = {
            let wakes = wakes.clone();
            std::sync::Arc::new(move || {
                wakes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            })
        };
        let panels = plugin_panels(vec![panel_waking(&registered, port, None, wake)]);
        panels.set_follow_feed(&scratch.0.join("f-socket"), None);
        let editor = Rc::new(CountingEditor::default());
        panels.set_editor(editor.clone());
        panels.editor_session();
        let host = panels.follower("demo").expect("demo follows").host.clone();
        let request = |id: u64, verb: &str| {
            let text = serde_json::json!({"v": 1, "id": id, "verb": verb, "args": {}}).to_string();
            host.page_message(&text, Instant::now());
        };
        request(1, "host.hello");
        request(2, "editor.follow");
        panels.set_on_screen(&demo(), true);
        page.0.borrow_mut().clear();
        let before = wakes.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(editor.0.get(), 0, "nothing has run the host's pump yet");

        let mut lines = batch(Vec::new());
        lines.views = vec![view("ghost"), view("demo")];
        lines.views_received = 2;
        assert!(panels.route_follow(lines, Instant::now()).is_empty());
        assert_eq!(
            page.0.borrow().iter().filter(|e| *e == "editor.view").count(),
            1,
            "the view for demo reached its page; the one for ghost went nowhere"
        );
        assert!(
            wakes.load(std::sync::atomic::Ordering::Relaxed) > before,
            "the host was woken"
        );
        // The wake alone would run the pump on the main loop's next turn; the routing runs it now, which
        // installs the follow snippet in the editor.
        assert!(editor.0.get() > 0, "the routing ran the host's pump");
    }

    #[test]
    fn on_screen_reaches_the_host_only_when_it_changes() {
        // The follow feed adds its reader to glib's default main context, which other tests in this crate own
        // while they run, so this one takes its turn and holds the context for its whole run.
        let _turn = crate::layout_state::default_context_turn();
        let context = glib::MainContext::default();
        let _owner = context
            .acquire()
            .expect("glib's default main context, which the other tests on it leave to this one");
        let scratch = Scratch::new("panels-follow-screen");
        let registered = registered(&scratch, "demo", true, None);
        let wakes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let wake: agent::UiWake = {
            let wakes = wakes.clone();
            std::sync::Arc::new(move || {
                wakes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            })
        };
        let panels = plugin_panels(vec![panel_waking(&registered, Rc::default(), None, wake)]);
        panels.set_follow_feed(&scratch.0.join("f-socket"), None);
        let count = || wakes.load(std::sync::atomic::Ordering::Relaxed);
        let start = count();
        panels.set_on_screen(&demo(), true);
        let told = count();
        assert!(told > start, "the first report reaches the host");
        panels.set_on_screen(&demo(), true);
        panels.set_on_screen(&demo(), true);
        assert_eq!(count(), told, "the same answer again tells it nothing");
        panels.set_on_screen(&demo(), false);
        assert!(count() > told);
        // Not a plugin's module: nothing.
        panels.set_on_screen(&ModuleId::lua("ghost"), true);
    }

    #[test]
    fn the_editors_end_is_told_once_until_a_new_session() {
        // The follow feed adds its reader to glib's default main context, which other tests in this crate own
        // while they run, so this one takes its turn and holds the context for its whole run.
        let _turn = crate::layout_state::default_context_turn();
        let context = glib::MainContext::default();
        let _owner = context
            .acquire()
            .expect("glib's default main context, which the other tests on it leave to this one");
        let scratch = Scratch::new("panels-follow-end");
        let registered = registered(&scratch, "demo", true, None);
        let page = Rc::new(Recorded::default());
        let port = Rc::new(LazyPort::default());
        *port.0.borrow_mut() = Some(page.clone());
        let panels = plugin_panels(vec![panel(&registered, port, None)]);
        panels.set_follow_feed(&scratch.0.join("f-socket"), None);
        let host = panels.follower("demo").expect("demo follows").host.clone();
        for (id, verb) in [(1, "host.hello"), (2, "editor.follow")] {
            let text = serde_json::json!({"v": 1, "id": id, "verb": verb, "args": {}}).to_string();
            host.page_message(&text, Instant::now());
        }
        panels.set_on_screen(&demo(), true);
        let closed = || page.0.borrow().iter().filter(|e| *e == "editor.closed").count();
        panels.editor_closed();
        assert_eq!(closed(), 1);
        panels.editor_closed();
        assert_eq!(closed(), 1, "an exit reported twice is told once");
        // A view the dead editor sent before it ended, read only now, does not bring the page back to it.
        let views = || page.0.borrow().iter().filter(|e| *e == "editor.view").count();
        let late = || {
            let mut lines = batch(vec![Ordered::Gone {
                owner: "demo".into(),
                buf: 3,
            }]);
            lines.views = vec![view("demo")];
            lines.views_received = 1;
            lines
        };
        assert!(panels.route_follow(late(), Instant::now()).is_empty());
        assert_eq!(views(), 0);
        assert_eq!(closed(), 1, "nor does its gone line tell the page anything");
        panels.editor_session();
        assert!(panels.route_follow(late(), Instant::now()).is_empty());
        assert_eq!(views(), 1, "a new session's views reach it");
        panels.editor_closed();
        assert_eq!(closed(), 3, "the gone line, then a later editor's end, are told again");
    }

    /// An editor that records every call it is asked, answers none of them, and runs the session `target` names.
    #[derive(Default)]
    struct RecordingEditor {
        calls: RefCell<Vec<(&'static str, Vec<rmpv::Value>)>>,
        target: Cell<Option<u64>>,
        targets: Cell<usize>,
    }

    impl RecordingEditor {
        /// The follow snippet's installs asked so far, as the arguments each was sent with.
        fn installs(&self) -> Vec<Vec<rmpv::Value>> {
            self.calls
                .borrow()
                .iter()
                .filter(|(code, args)| {
                    std::ptr::eq(*code, eitri_core::plugin::editor::PLUGIN_FOLLOW_LUA)
                        && args.first().and_then(rmpv::Value::as_str) == Some("install")
                })
                .map(|(_, args)| args.clone())
                .collect()
        }
    }

    impl EditorRpc for RecordingEditor {
        fn exec_lua(&self, code: &'static str, args: Vec<rmpv::Value>) -> eitri_core::nvim_rpc::Pending {
            self.calls.borrow_mut().push((code, args));
            eitri_core::nvim_rpc::Pending::failed(eitri_core::nvim_rpc::RpcError::Unavailable("test".into()))
        }

        fn target(&self) -> Option<u64> {
            self.targets.set(self.targets.get() + 1);
            self.target.get()
        }
    }

    #[test]
    fn a_restarted_editor_gets_the_follow_snippet_again() {
        // The follow feed adds its reader to glib's default main context, which other tests in this crate own
        // while they run, so this one takes its turn and holds the context for its whole run.
        let _turn = crate::layout_state::default_context_turn();
        let context = glib::MainContext::default();
        let _owner = context
            .acquire()
            .expect("glib's default main context, which the other tests on it leave to this one");
        let scratch = Scratch::new("panels-follow-back");
        let registered = registered(&scratch, "demo", true, None);
        let page = Rc::new(Recorded::default());
        let port = Rc::new(LazyPort::default());
        *port.0.borrow_mut() = Some(page.clone());
        let panels = plugin_panels(vec![panel(&registered, port, None)]);
        panels.set_follow_feed(&scratch.0.join("f-socket"), None);
        let editor = Rc::new(RecordingEditor::default());
        editor.target.set(Some(1));
        panels.set_editor(editor.clone());
        panels.editor_session();
        let pump = panels.follower("demo").expect("demo follows").clone();
        for (id, verb) in [(1, "host.hello"), (2, "editor.follow")] {
            let text = serde_json::json!({"v": 1, "id": id, "verb": verb, "args": {}}).to_string();
            pump.host.page_message(&text, Instant::now());
        }
        panels.set_on_screen(&demo(), true);
        pump.kick();
        let first = editor.installs();
        assert_eq!(first.len(), 1, "the first nvim gets the snippet");

        // nvim ended, and the same pane runs a new nvim: a session of its own.
        panels.editor_closed();
        editor.target.set(Some(2));
        panels.editor_back();
        pump.kick();
        pump.kick();
        let installs = editor.installs();
        assert_eq!(installs.len(), 2, "the new nvim gets the snippet once: {installs:?}");
        assert_eq!(installs[1], first[0], "with the options the first nvim got");
    }

    #[test]
    fn the_follow_trace_prints_totals_at_most_once_a_second_and_only_when_they_moved() {
        let mut totals = FollowTotals::default();
        let t0 = Instant::now();
        assert_eq!(totals.add(0, 0, t0), None, "nothing read yet");
        assert_eq!(totals.add(3, 1, t0).as_deref(), Some("[follow] trace: views=3 cmds=1"));
        assert_eq!(
            totals.add(2, 0, t0 + Duration::from_millis(500)),
            None,
            "within the second"
        );
        assert_eq!(
            totals.add(0, 0, t0 + Duration::from_secs(1)).as_deref(),
            Some("[follow] trace: views=5 cmds=1")
        );
        assert_eq!(totals.add(0, 0, t0 + Duration::from_secs(5)), None, "unchanged");
    }
}
