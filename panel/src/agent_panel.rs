//! The real agent-ui panel: the page a host supplies (`panel_page::PageSurface`) running the embedded
//! `agent-ui/web` frontend, its messages in (JS -> Rust, `AgentPanelHandle::page_message`), and a fast poll
//! of every tab's backend pushed to the page (Rust -> JS).
//!
//! **One backend per tab**. The tabs themselves live in
//! `eitri_core::tab_set::TabSet`; this module spawns the workers that set only holds receivers
//! for, drains every tab on one tick (run when due, `crate::panel_wake`), and routes each inbound command to the tab it names --
//! never to "the active one". Backend construction is lazy per tab: an empty
//! tab starts its backend on its first `send_message` (or a `resume`). No mode goes into it:
//! every session is gated, and the tab's own mode is read where requests are answered,
//! never sent to the CLI. Which backend gets built is `eitri_core::agent_backend`'s decision.
//!
//! **State is server-originated.** For the sidecar backend this module folds nothing of its own:
//! `active_turn_id`, tool calls and permissions all arrive as real events through `pump()`. A
//! command that succeeds returns no events at all; the UI updates on the next poll. Nothing here
//! synthesizes an optimistic `TurnStarted` to make the UI feel faster -- doing so would put the
//! panel's idea of "a turn is running" ahead of the server's, which is the exact shadow state the
//! runtime design forbids.
//!
//! **Each page draws only the tab it shows.** The panel keeps one page per chat view: the base view's
//! page draws its active tab, an extra view's page its one bound tab, and every payload about a tab
//! goes to the page that shows it (`AgentPanelState::page_of`). A background base-view tab's events
//! feed its attention and mark it stale (`TabSet::pump_views`); a switch always sends the new active
//! tab's full snapshot. A message from a page is handled as that page's: its view comes from the
//! handler the page was built with, never from what the page says.

use crate::panel_document::{PanelDocument, IDLE_DOCUMENT, PANEL_BASE_URI};
use crate::panel_page::{PanelHost, PanelPage};
use crate::trust_gate::{AnswerOutcome, Asked, CommandAnswer, GateEvent, Route, TrustGate};
use agent::setting_sources::ProjectTrust;
use eitri_core::agent_backend::{AgentBackend, BackendError, BackendGreeting, BackendKind};
use eitri_core::agent_bridge::{
    parse_inbound_message, serialize_command_result_for_js, serialize_confirm_restore_for_js, serialize_error_for_js,
    serialize_events_for_js, serialize_hello_with_restore_for_js, serialize_nav_key_for_js, ChooserRecord, ChooserTab,
    DetailRow, InboundMessage, NavKeyDirection, PaneNavDirection, PanelKeys, SessionModeChoice, TabVerbWire,
};
use eitri_core::keymap::TabAction;
use eitri_core::layout::Direction;
use eitri_core::lua::agent::AgentRegistration;
use eitri_core::project_trust::Profile;
use eitri_core::saved_tabs::{SavedTabs, TabWriter};
use eitri_core::tab_restore::{RestoreOffer, RestorePolicy, RestoreRun};
use eitri_core::tab_set::{
    AgentId, AwaitingTrust, BypassPolicy, DeferredStart, FirstTurn, PendingHandoff, PendingStart, RestoreConnect,
    RestoreGo, RestorePrompt, RestoreStep, ResumeRoute, StartCollected, Tab, TabBackend,
};
pub use eitri_core::tab_set::{TabPlace, ViewId};
use eitri_core::tabs::TabId;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;

/// The backstop on a terminal-handoff worker's close, once the window that started it is gone.
///
/// A separate number from `SESSION_CLOSE_BACKSTOP` although the two now bound the same SHAPE of
/// work: this one bounds a worker somebody else already started, that one a teardown started on
/// the close path itself, so a future reason to move either has nothing to do with the other.
/// Sharing one number would make the next change to it silently change the other.
///
/// It is the legacy backend's real close time with room to spare (~0.8s of grace periods plus
/// three thread joins) and is knowingly shorter than the sidecar path's documented worst case --
/// this backstop's expiry only releases the application hold, it does not kill anything, so
/// "knowingly short" here means Eitri's own process may exit before a still-reaping sidecar
/// finishes its cleanup, not that anything gets SIGKILLed early; `AgentPanelHandle::shutdown`'s
/// handoff branch says what expiring costs.
const HANDOFF_CLOSE_BACKSTOP: std::time::Duration = std::time::Duration::from_secs(3);

/// The backstop on a backend this panel is tearing down itself, once the window is gone. Since
/// session tabs it bounds every tab's teardown at once: they run in parallel, one worker each, under
/// this one span (`tear_down_all`).
///
/// **Nothing waits on this. No thread blocks on it at all, and the GTK main loop least of all.**
/// It is a deadline observed by a `CLOSE_WATCH_POLL` tick on the main loop (`close_watch_step`),
/// which does a `try_recv` and returns. The window is already destroyed and the teardown is on a
/// worker; what keeps the process alive meanwhile is a `gio` application hold, not a wait. Read
/// `hold_until_done` for the mechanism before changing this number.
///
/// **What is inside the span it bounds:** the whole of `AgentBackend::shutdown()` AND the drop of
/// the backend afterwards -- `tear_down` drops it explicitly before the
/// worker reports. On the sidecar path those are two different waits and only the second one kills
/// anything: `shutdown()` issues `close_session`, a unary RPC bounded at `agent::UNARY_RPC_TIMEOUT`
/// (agent/src/providers/claude_sidecar/mod.rs), and it is `SpawnedSidecar::drop` (close stdin, poll
/// up to `agent::SIDECAR_EXIT_GRACE`, then SIGKILL) plus `RuntimeThread::drop` that end the child.
/// 18 seconds covers that ~16 s worst case with a little room. The bound must include the drop,
/// because it is the step that escalates to stop the child process.
///
/// **What expiring costs**, since it is a bound and not a guarantee: one line on stderr naming what
/// is still outstanding, then the hold is released anyway, `Application::run` returns, and the
/// detached worker dies with the process -- i.e. exactly the orphaned `claude`/sidecar/`node` this
/// file keeps chasing with pid diffs, which is why the number is generous rather than tight.
const SESSION_CLOSE_BACKSTOP: std::time::Duration = std::time::Duration::from_secs(18);

/// How long a window close keeps the process alive for a backend that is still CONNECTING.
///
/// Not a teardown budget like the two above: what this waits out is `AgentBackend::start`
/// finishing, so that a backend which finishes after the window is gone can be shut down rather
/// than left running with nothing owning it. Three seconds is well past a warm connect and well
/// short of a cold one (`npm ci` in a fresh Verdandi checkout is minutes), so it deliberately does
/// not cover every case; what it must not do is cover none of them by exiting instantly.
///
/// The sidecar's own bind wait (`SIDECAR_BIND_BUDGET` in `agent`, 15 s) is longer than this on
/// purpose. A window closed while a cold sidecar is still starting does not wait out that budget:
/// the hold ends here and the process exits, the connect worker with it. The sidecar is left with
/// its stdin pipe closed, which it reads as the host being gone, so no one waits on it. A longer
/// hold would keep an invisible process alive for up to 15 s after the window closed, to save a
/// session nobody is looking at.
const CONNECT_CLOSE_BACKSTOP: std::time::Duration = std::time::Duration::from_secs(3);

/// How long a close waits for the saved tabs' last write. A write is a small file renamed over the old one,
/// milliseconds even on a network file system; a disk that hangs longer is left, and since the rename comes
/// last the previous file stays whole.
const SAVED_TABS_BACKSTOP: std::time::Duration = std::time::Duration::from_secs(5);

/// How often the main loop looks at a close that is running on a worker. A `try_recv` and an
/// `Instant::elapsed`, so the interval only sets how long the process lingers past a teardown that
/// has already finished -- not how much work the main loop does.
pub const CLOSE_WATCH_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// What one tick of the close watch decides.
///
/// A value rather than control flow so the decision -- specifically that a report observed on the
/// same tick as the deadline counts as a report -- is assertable without a GTK main loop, which
/// `shell`'s tests cannot start.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum CloseWatch {
    /// Still running, still inside the backstop: keep holding the application.
    KeepHolding,
    /// The worker reported. Everything the span covers is done.
    Finished,
    /// The sender dropped without a report: the worker panicked inside the teardown. Immediate,
    /// and NOT a timeout -- calling it one would send a future debugger looking for a hang that
    /// never happened.
    WorkerDied,
    /// The backstop expired with the teardown still outstanding.
    Expired,
}

fn close_watch_step<T>(reported: &Result<T, mpsc::TryRecvError>, past_backstop: bool) -> CloseWatch {
    match reported {
        // Checked first on purpose: a `try_recv` that finds the value also finds the sender gone on
        // the NEXT call, and a teardown that finished in the same tick as the deadline finished.
        Ok(_) => CloseWatch::Finished,
        Err(mpsc::TryRecvError::Disconnected) => CloseWatch::WorkerDied,
        Err(mpsc::TryRecvError::Empty) if past_backstop => CloseWatch::Expired,
        Err(mpsc::TryRecvError::Empty) => CloseWatch::KeepHolding,
    }
}

/// One worker a window close left running: what to look at, and what to start when it reports.
/// Built by [`watched`]; looked at by [`ShutdownWatch::step`].
struct Watched {
    /// One look at `now`: `None` while the worker is still inside its backstop, else the entries its
    /// end started (usually none).
    look: Box<dyn FnMut(std::time::Instant) -> Option<Vec<Watched>>>,
}

/// Watches `rx` for one report, with nothing waiting for it. Its backstop runs from this call.
///
/// `what` names the branch in the log: the callers fail for different reasons and a debugger
/// reading one line needs to know which one it is looking at. `on_value` runs when the report
/// arrives and returns the entries it starts (the mid-connect branch starts a teardown there); they
/// join the same watch, so the process is never let go between the two.
fn watched<T: 'static>(
    what: &'static str,
    rx: mpsc::Receiver<T>,
    backstop: std::time::Duration,
    on_value: impl FnMut(T) -> Vec<Watched> + 'static,
) -> Watched {
    watched_abandoning(
        what,
        rx,
        backstop,
        "whatever is left of it -- possibly a live `claude`, sidecar or `node` -- is being abandoned",
        on_value,
    )
}

/// [`watched`], saying `abandoned` (what is left behind when the backstop passes) in its log line.
fn watched_abandoning<T: 'static>(
    what: &'static str,
    rx: mpsc::Receiver<T>,
    backstop: std::time::Duration,
    abandoned: &'static str,
    mut on_value: impl FnMut(T) -> Vec<Watched> + 'static,
) -> Watched {
    let started = std::time::Instant::now();
    Watched {
        look: Box::new(move |now| {
            let reported = rx.try_recv();
            match close_watch_step(&reported, now.saturating_duration_since(started) >= backstop) {
                CloseWatch::KeepHolding => return None,
                CloseWatch::Finished => {
                    if let Ok(value) = reported {
                        return Some(on_value(value));
                    }
                }
                CloseWatch::WorkerDied => {
                    eprintln!("[agent_panel] {what}: its worker died without reporting");
                }
                CloseWatch::Expired => eprintln!(
                    "[agent_panel] {what} had still not finished {}s after the window closed; the \
                     process is exiting anyway and {abandoned}",
                    backstop.as_secs()
                ),
            }
            Some(Vec::new())
        }),
    }
}

/// Teardowns and connects a window close left on worker threads, each with its backstop. The host
/// polls [`ShutdownWatch::step`] every [`CLOSE_WATCH_POLL`] while it keeps its process alive, and lets
/// the process go once `step` returns false. Nothing waits on a worker.
#[must_use]
pub struct ShutdownWatch {
    entries: Vec<Watched>,
}

impl ShutdownWatch {
    /// Nothing to keep the process alive for.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// One look at every entry at `now`: a report, a dead worker or a passed backstop lets it go (logged
    /// as it always was); a connect that reports a backend adds that backend's teardown. Returns whether
    /// anything is still held.
    pub fn step(&mut self, now: std::time::Instant) -> bool {
        let mut started = Vec::new();
        self.entries.retain_mut(|entry| match (entry.look)(now) {
            None => true,
            Some(more) => {
                started.extend(more);
                false
            }
        });
        self.entries.extend(started);
        !self.entries.is_empty()
    }
}

/// Tears a backend down on a worker thread and never waits for it; the entry reports when it is done.
///
/// **One mechanism for every backend this panel shuts down itself**, which is the point of it
/// being a function: `AgentPanelHandle::shutdown` has two such backends (the installed session,
/// and one that finished connecting while the window was closing). Both must be torn down on a
/// worker rather than the GTK main loop.
///
/// Why it may not happen inline: `AgentBackend::shutdown` stops ingestion first, and
/// `ConversationIngest::stop` JOINS the ingestion thread, which since the resume-history branch
/// may be inside `history::store::save` -- serializing up to `HISTORY_MAX_CHARS`, an `sync_all`, a
/// rename and a directory fsync, plus (on a first write) a `read_dir` and up to 17 record parses
/// to sweep orphans. That is a disk wait, and this repository already has one recorded whole-window
/// freeze from exactly the family "the GTK thread waits on something slow" (the projection-guard
/// deadlock, 2026-09-15); a frozen window during close is the same symptom whether the cause is a
/// mutex or an fsync on a full disk.
///
/// **`drop(backend)` is explicit and its position is the point.** On the sidecar path `shutdown()`
/// only issues `close_session` and joins ingestion; the child is killed by `SpawnedSidecar::drop`
/// (close stdin, poll up to 3s, SIGKILL) and its Tokio thread joined by `RuntimeThread::drop`. A
/// version of this function reported to the watcher first and let the backend fall at the end of
/// the closure, which put the only part of the teardown that actually ends the process AFTER the
/// thing that decides the process may exit -- so the escalation raced process exit and the backstop
/// did not cover what its own doc derived it from.
fn tear_down(what: &'static str, mut backend: AgentBackend) -> Watched {
    let (closed_tx, closed_rx) = mpsc::channel();
    std::thread::spawn(move || {
        backend.shutdown();
        // Explicit, and BEFORE the report below rather than at the end of this closure: on the
        // sidecar path this line is where the child actually dies. See this function's doc.
        drop(backend);
        // The receiver is gone if the watch already let go; that is the abandoned case, not an
        // error.
        let _ = closed_tx.send(());
    });
    watched(what, closed_rx, SESSION_CLOSE_BACKSTOP, |()| Vec::new())
}

/// Every tab's backend at once, one worker each, under ONE backstop: serial sidecar `CloseSession`s
/// are bounded at 10 s each, so N tabs torn down in turn could outlive any backstop. The workers
/// report to a collector, which reports once to the watch. `None` when there is no backend: nothing is
/// held then.
///
/// Each worker drops its backend before it reports, for the reason `tear_down` gives: on the sidecar
/// path the drop is where the child dies.
fn tear_down_all(what: &'static str, backends: Vec<AgentBackend>) -> Option<Watched> {
    if backends.is_empty() {
        return None;
    }
    let expected = backends.len();
    let (each_tx, each_rx) = mpsc::channel();
    for mut backend in backends {
        let tx = each_tx.clone();
        std::thread::spawn(move || {
            backend.shutdown();
            drop(backend);
            let _ = tx.send(());
        });
    }
    drop(each_tx);
    let (all_tx, all_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = all_tx.send(collect_reports(each_rx, expected));
    });
    Some(watched(what, all_rx, SESSION_CLOSE_BACKSTOP, move |finished| {
        if finished < expected {
            eprintln!(
                "[agent_panel] {what}: {} of {expected} workers died without reporting",
                expected - finished
            );
        }
        Vec::new()
    }))
}

/// A connect still running when the window closed -- a tab's, or a closed tab's (`Retiring`). A
/// connect, not a teardown: see `CONNECT_CLOSE_BACKSTOP`. A backend that finishes after the process
/// is gone is exactly the orphan `AgentPanelHandle::shutdown` exists to prevent, so one that
/// finishes inside the backstop is torn down, held and bounded like the installed sessions: its
/// shutdown reaches the same ingestion-thread join.
fn watch_connect(rx: mpsc::Receiver<Result<AgentBackend, BackendError>>) -> Watched {
    watched(
        "a connect that was still running",
        rx,
        CONNECT_CLOSE_BACKSTOP,
        move |result| match result {
            Ok(backend) => {
                eprintln!("[agent_panel] window closed mid-connect; shutting down the backend that finished anyway");
                vec![tear_down("a backend that finished mid-connect", backend)]
            }
            Err(e) => {
                eprintln!(
                    "[agent_panel] window closed mid-connect; the backend had already failed: {}",
                    e.message
                );
                Vec::new()
            }
        },
    )
}

/// Waits for `expected` teardown workers' reports and says how many arrived. A worker that died
/// drops its sender without one; once every sender is gone `recv` fails and this returns.
fn collect_reports(rx: mpsc::Receiver<()>, expected: usize) -> usize {
    let mut finished = 0;
    while finished < expected {
        match rx.recv() {
            Ok(()) => finished += 1,
            Err(_) => break,
        }
    }
    finished
}

/// What tabs that left while the window stays open are still doing: a backend being shut down on
/// a worker, or a connect that has not finished (a closed tab, `r` reset, a fatal command, a
/// session that never opened).
///
/// **Held in panel state so the window-close backstop sees it.** These used to run on bare threads nobody recorded, so a window closed a few seconds
/// after `prefix &` y let `Application::run` return with the worker halfway through `shutdown()`:
/// `SpawnedSidecar::drop` never ran and the sidecar outlived the process, and a connect finishing
/// later was never shut down at all. `AgentPanelHandle::shutdown` now watches every entry here under
/// the same holds and backstops as the tabs it takes; the pump forgets an entry once it finishes.
#[derive(Default)]
struct Retiring {
    /// Connects of tabs that are gone. Whatever one produces is shut down (`poll`).
    connects: Vec<mpsc::Receiver<Result<AgentBackend, BackendError>>>,
    /// One per teardown worker, sent after the backend was shut down AND dropped -- the drop is
    /// where the sidecar child actually dies. A worker that panicked drops its sender instead. Each
    /// names the tab whose presence the set keeps until it reports ([`TabSet::note_retiring`]), when
    /// it is one the set counted.
    teardowns: Vec<(Option<TabId>, mpsc::Receiver<()>)>,
}

impl Retiring {
    /// One backend, shut down and dropped on a worker thread, never on the GTK main loop (the same
    /// reasons as `tear_down`). `tab` is the tab the set counted it as retiring for, which `poll`
    /// names once the shutdown has returned; `None` for a backend the set never held.
    fn backend(&mut self, backend: AgentBackend, tab: Option<TabId>) {
        let _ = self.backend_watched(backend, tab);
    }

    /// [`backend`](Self::backend), and a receiver told when it is shut down and dropped (or its worker died):
    /// for a start that must not begin before the old session lets go of its lease.
    fn backend_watched(&mut self, mut backend: AgentBackend, tab: Option<TabId>) -> mpsc::Receiver<()> {
        let (done_tx, done_rx) = mpsc::channel();
        let (gone_tx, gone_rx) = mpsc::channel();
        std::thread::spawn(move || {
            backend.shutdown();
            drop(backend);
            let _ = done_tx.send(());
            let _ = gone_tx.send(());
        });
        self.teardowns.push((tab, done_rx));
        gone_rx
    }

    /// A tab removed from the set: its backend is shut down, its connect is waited for, and a
    /// handoff's own worker (which already owns that backend) is waited for too. Returns the
    /// requests the tab still owed a `command_result`, for the caller to refuse -- a connect's
    /// `send_message`/`resume`, and a handoff's `handoff_to_terminal`. The record is never deleted.
    fn tab(&mut self, tab: Tab) -> Vec<String> {
        let mut owed = Vec::new();
        if let Some(handoff) = tab.pending_handoff {
            owed.push(handoff.request_id);
            self.teardowns.push((None, handoff.dropped_rx));
        }
        match tab.backend {
            // `TabSet::remove` counted it as retiring.
            TabBackend::Live(backend) => self.backend(backend, Some(tab.id)),
            TabBackend::Starting(pending) => {
                owed.push(pending.request_id);
                self.connects.push(pending.result_rx);
            }
            // Nothing was spawned, so there is nothing to stop; the command that asked is owed an answer.
            TabBackend::AwaitingTrust(waiting) => owed.extend(waiting.start.request_id().map(str::to_owned)),
            TabBackend::NotStarted | TabBackend::Failed { .. } => {}
        }
        owed
    }

    /// Each tick: a connect that finished has its backend shut down; a teardown that reported, or
    /// whose worker died, is forgotten, and the tab it was counted for is returned for
    /// [`TabSet::retire_done`]. `try_recv` only -- this runs on the GTK main loop.
    fn poll(&mut self) -> Vec<TabId> {
        let mut finished = Vec::new();
        self.connects.retain(|rx| match rx.try_recv() {
            Ok(Ok(backend)) => {
                finished.push(backend);
                false
            }
            Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected) => false,
            Err(mpsc::TryRecvError::Empty) => true,
        });
        for backend in finished {
            eprintln!("[agent_panel] a closed tab's connect finished; shutting its backend down");
            self.backend(backend, None);
        }
        let mut done = Vec::new();
        self.teardowns.retain(|(tab, rx)| {
            // A worker that died has dropped its backend on the way out, which ends the child too.
            let finished = !matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty));
            if finished {
                done.extend(*tab);
            }
            !finished
        });
        done
    }

    fn is_empty(&self) -> bool {
        self.connects.is_empty() && self.teardowns.is_empty()
    }
}

#[cfg(any(test, feature = "test-support"))]
static HOST_TEST_DOCUMENT: std::sync::LazyLock<PanelDocument> = std::sync::LazyLock::new(|| {
    PanelDocument::new("<!doctype html><html><head></head><body><script type=\"module\">1</script></body></html>")
});

/// A host that opens nothing, for [`AgentPanelHandle::for_host_test`].
#[cfg(any(test, feature = "test-support"))]
struct NoHost;

#[cfg(any(test, feature = "test-support"))]
impl PanelHost for NoHost {
    fn open_external(&self, _url: &str) {}
}

#[cfg(any(test, feature = "test-support"))]
impl AgentPanelHandle {
    /// A panel over `page` with one empty tab and no supervisor, disk, trust worker or review: what a
    /// host's own tests drive, to see how its loop runs the tick.
    pub fn for_host_test(page: Rc<dyn crate::panel_page::PageSurface>) -> Self {
        let tabs =
            eitri_core::tab_set::TabSet::new(BackendKind::Sidecar, eitri_core::agent_bridge::SessionModeChoice::Auto);
        let state = bare_state(&HOST_TEST_DOCUMENT, Rc::new(NoHost), tabs);
        AgentPanelHandle::over(Rc::new(RefCell::new(state)), Some(Rc::new(PanelPage::new(page))))
    }

    /// [`for_host_test`](Self::for_host_test) with its tabs kept in `files` for `project`: one tab that keeps
    /// the session `session` (a resume that failed, so the close has nothing to tear down and owes only the
    /// write), and `before_write` run on the writer thread before each write. A host's test of its close holds
    /// the last write with it and sees the close wait for it.
    pub fn for_host_test_with_saved_tabs(
        page: Rc<dyn crate::panel_page::PageSurface>,
        files: PathBuf,
        project: PathBuf,
        session: &str,
        before_write: std::sync::Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let handle = Self::for_host_test(page);
        {
            let mut state = handle.state.borrow_mut();
            let state = &mut *state;
            state.canonical_project_dir = project.to_string_lossy().into_owned();
            state.conversation_id = agent::conversation_id_for_cwd(&project);
            let mut writer = TabWriter::new(Some(files), project.clone());
            writer.set_before_write(before_write);
            state.tab_memory = writer;
            state.project_dir = project;
            let tab = state.tabs.active();
            let tab = state.tabs.get_mut(tab).expect("the active tab");
            tab.failed_resume = Some(session.to_owned());
            tab.backend = TabBackend::Failed {
                reason: "stopped".to_owned(),
            };
        }
        handle
    }

    /// Hands the panel a supervisor connection over `stream`, as a host's test plays the supervisor.
    pub fn connect_test_supervisor(&self, stream: std::os::unix::net::UnixStream) {
        self.state.borrow_mut().supervisor = crate::supervisor_client::SupervisorClient::over_stream(stream);
    }

    /// How many ticks have run.
    pub fn ticks_run(&self) -> u64 {
        self.state.borrow().ticks_run
    }
}

/// What a command still owed by a tab that was closed is answered with.
const TAB_CLOSED_MESSAGE: &str = "the tab was closed before this finished";

/// What the frontend is told when the close worker died inside `shutdown()`. One constant because
/// it is both logged and dispatched, and the two drifting apart would make a log line unmatchable
/// to what the user saw.
const HANDOFF_CLOSE_FAILED_MESSAGE: &str =
    "the handoff could not finish closing this session; it is no longer running here";

/// The panel's two HINT messages, demuxed out of `InboundMessage` for `shell::hint::HintCoordinator`
/// -- it never sees the wire's `request_id`, only what the coordinator itself needs to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintInbound {
    /// `f` in the panel's BROWSE: start a global HINT (or cancel one already active, same as the
    /// window-level toggle).
    Request,
    /// The panel's answer to `hint_collect`: how many visible targets it froze for `session_id`.
    Targets { session_id: u64, count: usize },
}

struct AgentPanelState {
    /// The bundle every document load starts from (`PanelDocument::themed`).
    document: &'static PanelDocument,
    /// The process around the panel: where a checked link or address is opened.
    host: Rc<dyn PanelHost>,
    /// Every tab and which is active. Rust owns the tabs; the WebView
    /// draws the active one.
    tabs: eitri_core::tab_set::TabSet,
    backend_kind: BackendKind,
    project_dir: PathBuf,
    /// `project_dir.canonicalize()`, read once: the `canonical_cwd` a session lease is keyed on
    /// (`agent::lease::SessionLease::is_held`), for resume routing and the chooser.
    canonical_project_dir: String,
    /// `$XDG_STATE_HOME/eitri/agent`, where the remembered mode is written. `None`
    /// with no state directory.
    prefs_dir: Option<PathBuf>,
    /// Where the user is, asked for at send time. Never stored across turns: what the editor was
    /// showing when the LAST turn went out is not context, it is a stale claim, and the composer
    /// has no way to tell the model which it is.
    editor_context: eitri_core::editor_context::ContextSource,
    supervisor: Option<crate::supervisor_client::SupervisorClient>,
    /// Set only while a freshly-spawned `eitri-supervisor` is still coming up. The pump drains
    /// it into `supervisor` above. A window that spawns the supervisor cannot connect to it
    /// synchronously -- see `PendingSupervisor` for the failure that taught us that.
    supervisor_pending: Option<std::sync::mpsc::Receiver<Option<crate::supervisor_client::SupervisorClient>>>,
    /// Set by `AgentPanelHandle::shutdown`, i.e. only on the window-close path, and never cleared.
    ///
    /// **It exists because the main loop now outlives the window.** `shutdown` holds the
    /// application so the process can finish tearing its children down, which means every
    /// main-loop source this panel installed keeps firing for up to `SESSION_CLOSE_BACKSTOP`
    /// afterwards -- against a destroyed window and a `WebView` that is no longer in a widget
    /// tree. The pump reads this and stops; `poll_activate` reads it and stops answering, because
    /// its caller's reaction is `window.present()` on a window GTK has already destroyed.
    ///
    /// **Also the guard on the tab set:** `shutdown` empties it with `TabSet::take_all`, after which
    /// `active_tab()` would panic, so every path that reaches the set checks this first.
    shutting_down: bool,
    /// What tells the host to run the tick: raised by backends' folds and by every call into the panel.
    /// `None` when the socket pair could not be made, and then the tick runs on its old steady cadence.
    wake: Option<crate::panel_wake::PanelWake>,
    /// When the last tick started: a wake runs the next one no sooner than [`AgentPanelHandle::TICK`] after.
    last_tick: Option<std::time::Instant>,
    /// Ticks that ran (past the close check), for a test counting how often an idle panel wakes.
    #[cfg(any(test, feature = "test-support"))]
    ticks_run: u64,
    /// The panel's current theme. Starts on `ThemeTokens::fallback()` and is replaced by
    /// `AgentPanelHandle::set_theme`. Held as tokens rather than an envelope because it feeds three
    /// things: the `ready` batch, the `<style>` inlined into every `load_html`, and the WebView's
    /// own background colour.
    theme: eitri_core::theme::ThemeTokens,
    /// The `?` overlay's rows (`serialize_keymap_for_js`), sent with the theme on every `ready` so
    /// a reloaded document is told the keymap it needs, not the one baked into a hand list.
    keymap_help: Option<String>,
    /// One record per chat page that is attached and live, by the view it shows, and always one for
    /// the base view. A page that goes idle loses its record, so nothing here is ever sent to it again
    /// until it is attached anew.
    pages: BTreeMap<ViewId, PageState>,
    /// Told once per extra view left holding no tab (`AgentPanelHandle::on_view_empty`).
    view_empty_hook: Option<Rc<dyn Fn(ViewId)>>,
    /// Told when a pick asks the window to show a view and give it the keys
    /// (`AgentPanelHandle::on_show_view`).
    show_view_hook: Option<Rc<dyn Fn(ViewId)>>,
    /// Extra views whose tab left since the hook was last told, in the order they emptied.
    emptied_views: Vec<ViewId>,
    /// Whether the views left empty by a settled restore have been judged (once per window).
    empty_views_judged: bool,
    /// The page the last chooser was opened in: its newer titles go there.
    chooser_view: ViewId,
    /// The review requests the flow has not finished answering, each with the page that sent it: what
    /// the flow answers later goes there. Two pages may not have the same request id out at once.
    review_requests: Vec<(String, ViewId)>,
    /// The `:trust` and `:untrust` commands the gate is still working on, by the id the gate knows
    /// each by. Two pages number their requests alike, so the gate is given ids unique in the window.
    trust_commands: BTreeMap<String, TrustCommand>,
    /// The number in the next command's gate id.
    next_trust_command: u64,
    /// The page each starting tab's start was asked from, with that command's request id: its
    /// `command_result` is owed there even if the tab has moved to another view since.
    start_pages: BTreeMap<TabId, (String, ViewId)>,
    /// `agent.typing_cadence_hz` as last set, for a page attached after it was.
    typing_cadence: Option<u32>,
    /// Whether the editor holds the keys, for a page attached after it was last said.
    editor_has_keys: bool,
    /// How the page shows the agent's thinking (`init.lua`'s `agent.thinking`), sent on every `ready`
    /// so a reloaded document draws thinking the way the first one did.
    thinking_display: eitri_core::agent_prefs::ThinkingDisplay,
    /// Where the panel's two HINT messages (`hint_request`, `hint_targets`) go. `None` until
    /// `shell::hint::HintCoordinator` installs one via `AgentPanelHandle::on_hint`. An `Rc<dyn Fn>`
    /// rather than a plain closure field because the hook must be cloned out of the borrow before
    /// being called -- see `handle_inbound_message`'s HintRequest/HintTargets arms for why.
    hint_hook: Option<Rc<dyn Fn(HintInbound)>>,
    /// The window's check on every answer from a page (`AgentPanelHandle::set_answer_gate`), asked
    /// with that page's view: `Err(why)` refuses the answer and leaves the card or question waiting.
    /// Cloned out of the borrow before it is called, as `hint_hook` is, so a gate that reads the panel
    /// cannot find the state held.
    answer_gate: Option<AnswerGate>,
    /// The window's word on whether a float covers the chat now (`AgentPanelHandle::set_coverage`). A
    /// covered chat is off screen for attention: what finishes under a float is not seen. Cloned out
    /// of the borrow before it is called, as `answer_gate` is.
    coverage: Option<Coverage>,
    /// Told whenever the window's attention (every tab's, summed: `TabSet::attention`) or one tab's
    /// own changes, with the window's value before and after and the views handed a card: each chat
    /// chip's `⚑N`, the toast, `on_permission`.
    attention_hook: Option<AttentionHook>,
    /// Every tab's attention as the hook was last told it (`report_attention`).
    reported_tabs: Vec<eitri_core::tabs::TabAttention>,
    /// The per-window scratch directory, `None` if it could not be made.
    scratch: Option<eitri_core::scratch::ScratchDir>,
    /// Edits out in nvim, polled by the tick until their marker appears.
    pending_edits: Vec<eitri_core::scratch::PendingEdit>,
    /// Reviews being worked out on worker threads, polled by the tick: git never runs on the GTK
    /// thread, so the answer comes back through a channel the tick drains.
    review_jobs: Vec<PendingReview>,
    /// Status-band hints not yet shown: the page was not ready (loading, or reloading) when they
    /// arrived. At most one per tab, the newest.
    review_hints: Vec<eitri_core::turn_review::ReviewHint>,
    /// The hints the page has been sent and has not since been told to drop, at most one per tab:
    /// the page is only a view, so a document that replaces it (`prefix r`, a crashed WebView) is
    /// owed these again. Dropped when the tab's next turn starts, when that tab's review is answered
    /// with a review, and when a newer hint for the tab arrives.
    shown_hints: Vec<eitri_core::turn_review::ReviewHint>,
    /// Whether a finished turn's hint reaches the status band (`review.hint`). Off until
    /// `set_review` says otherwise: the review is still one key away.
    review_hint: bool,
    /// Whether a turn review is installed, so `c` can open one: `review.enabled` and a backend that
    /// has turn boundaries. Off until `set_review` installs one. Told to the page with `review_hint`
    /// (`review_settings`), which draws nothing about the turn's changes unless both are on.
    review_enabled: bool,
    /// What `set_review` builds the turn review from: the state and home directories and the
    /// canonical project root, read once when the state is made.
    review_roots: ReviewRoots,
    /// shows and focuses the editor, then hands it these keys; `Err` says why it could not.
    editor_request_hook: Option<EditorRequestHook>,
    /// How to ask the editor something and poll the answer: the embedded pane or the companion's
    /// link. `None` until the window hands one over.
    editor_rpc: Option<Rc<dyn eitri_core::editor_rpc::EditorRpc>>,
    /// The review's reverts, undos, comments, sends and recoveries in flight; the tick drives it.
    review_flow: eitri_core::turn_review::ReviewFlow,
    /// What the window gives the flow's editor overlay: who owns the module in its nvim, and what to
    /// do once a file opened there.
    review_editor: crate::review_editor::ReviewEditor,
    /// An edit came back (or was discarded): the keys return to the chat, in INPUT, in the view of the
    /// edit's tab.
    editor_done_hook: Option<Rc<dyn Fn(ViewId)>>,
    /// Teardowns and connects of tabs that left while the window stays open, so the window-close
    /// backstop covers them too. See [`Retiring`].
    retiring: Retiring,
    /// Per tab, the session `r` is continuing in it and its old backend's teardown: that session's lease is
    /// let go only when the old backend is gone, so a start resuming that session waits for it. Taken by the
    /// tab's next external start whatever it is, and dropped when the start is put off or the tab closes.
    resume_after: std::collections::HashMap<TabId, (String, mpsc::Receiver<()>)>,
    /// `$XDG_STATE_HOME/eitri/history` and `.../permissions`; `None` with no state directory.
    history_dir: Option<PathBuf>,
    rules_dir: Option<PathBuf>,
    /// The project's prompts, oldest first, as last read or written.
    history: Vec<String>,
    /// The last `editor_link` envelope (companion mode), kept so a fresh document is told on
    /// `ready`. `None` in a window with no companion, which never sends one.
    editor_link: Option<String>,
    /// Where a `tab_verb` message's mapped `TabAction` goes, with the view of the page that sent it.
    /// `None` until `main.rs`'s `AgentPanelHandle::on_tab_verb` installs one.
    tab_verb_hook: Option<Rc<dyn Fn(ViewId, TabAction)>>,
    /// Where a `nav_fallthrough` message's direction goes, and a `pane_nav`'s (`Ctrl+w h/j/k/l` --
    /// the same move, from a key the page read itself), with the view of the page that sent it:
    /// `None` until `main.rs`'s `AgentPanelHandle::on_nav_fallthrough` installs one, which is why
    /// `InboundMessage::NavFallthrough` and `InboundMessage::PaneNav` can be refused.
    nav_fallthrough_hook: Option<Rc<dyn Fn(ViewId, Direction)>>,
    /// Where a `keymap_closed` message goes (the opening it closes, and whether the overlay's own keys
    /// closed it): `None` in a window that never gives the keys back (the companion window, `eitri-mac`),
    /// where the message is accepted and does nothing.
    keymap_closed_hook: Option<Rc<dyn Fn(u64, bool)>>,
    /// Keeps `$XDG_STATE_HOME/eitri/tabs/<project>.json` in step with the tabs, once per tick.
    tab_memory: TabWriter,
    /// The CLI's own session titles, read on a worker; a title that changed raises the wake and `hello`
    /// is sent again.
    titles: eitri_core::transcript_titles::TitleCache,
    /// A title read since the last `hello`: it is sent again.
    hello_titles_stale: bool,
    /// The records the last chooser listed, each with the title it showed, so a title read after it was
    /// sent reaches the rows of a chooser still open.
    chooser_listed: Vec<(String, Option<String>)>,
    /// A title read since the chooser's titles were last compared with the cache.
    chooser_titles_stale: bool,
    /// This project's conversation id (`agent::conversation_id_for_cwd` of its canonical root), which
    /// every saved tab carries.
    conversation_id: String,
    /// What the last window on this project left open, read once at launch. The launch dashboard
    /// offers it, or `agent.restore = "auto"` brings it back; either way it is used up. `None` when
    /// there was nothing, it was unusable, or `agent.restore` is `"off"`.
    restore_source: Option<SavedTabs>,
    /// `agent.restore`, as `init.lua` left it (applied once, after it ran).
    restore_policy: RestorePolicy,
    /// Whether `init.lua` made bypass the default mode: the one user whose saved bypass tabs come
    /// back in bypass without being asked.
    bypass_by_config: bool,
    /// Whether this project remembers a mode Shift+Tab left, which the configured default yields to.
    mode_remembered: bool,
    /// The agent a tab made without naming one runs (`agent.default`).
    default_agent: AgentId,
    /// `agent.codex.command` as `init.lua` left it; `EITRI_CODEX` still outranks it.
    codex_command: Option<String>,
    /// The ACP agents `init.lua` registered: what `:tabnew <name>` accepts, the files each one's trust
    /// question is about, and the command its start runs.
    acp_agents: Vec<AgentRegistration>,
    /// What a connect worker has to say while it works (a first Codex start's self-check): sent to
    /// the band by the tick.
    start_notes: StartNotes,
    /// A restore whose resumes have not all returned yet.
    restore_run: Option<RestoreRun>,
    /// Where a restore's one-line result goes: `main.rs`'s window toast.
    toast_hook: Option<ToastHook>,
    /// Whether a session about to start may load the project's own Claude configuration, and the
    /// question that decides it. Every start goes through it (`start_deferred`).
    trust: TrustGate,
    /// What starts a session's backend: the real connect workers, or a test's stand-in.
    spawner: Spawner,
}

/// What the panel keeps for one live chat page. Everything here is about the document in that page:
/// a reload or a crash of one page leaves every other page's record as it was.
struct PageState {
    /// Set by `ready`, cleared by `reload_document`: until then nothing that can wait is sent.
    document_ready: bool,
    /// The page's last-reported `panel_keys` mode, mirrored here. Starts, and is reset to,
    /// `Other` on every `ready` (`PanelKeys::Other`'s own doc says why, `reset_nav_mode_for_ready`
    /// is where the reset happens) -- see `AgentPanelHandle::panel_keys`.
    nav_mode: Cell<PanelKeys>,
    /// Whether this page's pane holds the window's keyboard focus, as `crate::pane_focus` last
    /// reported it. It is held here so that a freshly loaded document (first load or a
    /// `prefix r` reload) is told on `ready`. A document is otherwise told only when focus
    /// changes, and a reload does not change focus.
    pane_focused: bool,
    /// The last `tabs` envelope sent to this page, so the tick sends one only when it changed.
    last_tabs_payload: Option<String>,
    /// The open provider sessions this page's `hello` was last computed against.
    last_open_ids: Vec<String>,
    /// Whether this page's last hello could carry the restore offer (only the base page's can), so
    /// the tick sends a new one only when that changes.
    last_restore_offered: bool,
    /// The agent of the tab this page's last hello was computed for: its sessions are the ones the
    /// greeting offers, so the tick sends a new one when that tab runs another agent.
    last_hello_agent: AgentId,
    /// The last `editor_context` envelope sent to this page.
    last_context_payload: Option<String>,
    /// How many times this page's own web process has crashed recently, and whether
    /// automatic recovery has given up on it -- see `AgentPanelHandle::page_gone`.
    /// Re-armed by a manual recovery, never by the automatic path itself --
    /// `AgentPanelHandle::reload_document_by_hand`'s own doc says why.
    crash_guard: crate::webview_crash_guard::WebViewCrashGuard,
}

impl Default for PageState {
    fn default() -> Self {
        PageState {
            document_ready: false,
            nav_mode: Cell::new(PanelKeys::Other),
            pane_focused: false,
            last_tabs_payload: None,
            last_open_ids: Vec::new(),
            last_restore_offered: false,
            last_hello_agent: AgentId::Claude,
            last_context_payload: None,
            crash_guard: crate::webview_crash_guard::WebViewCrashGuard::with_defaults(),
        }
    }
}

impl AgentPanelState {
    /// The record of `view`'s page, while it is live.
    fn page(&self, view: ViewId) -> Option<&PageState> {
        self.pages.get(&view)
    }

    /// The record of `view`'s page, made if there is none.
    fn page_mut(&mut self, view: ViewId) -> &mut PageState {
        self.pages.entry(view).or_default()
    }

    /// Whether `view`'s page is live and its document said `ready`.
    fn ready(&self, view: ViewId) -> bool {
        self.page(view).is_some_and(|page| page.document_ready)
    }

    /// The tab `view`'s page shows: the base view's active tab, or an extra view's bound tab (none
    /// before one is opened in it).
    fn tab_of_page(&self, view: ViewId) -> Option<TabId> {
        if view.is_base() {
            Some(self.tabs.active())
        } else {
            self.tabs.tab_in(view)
        }
    }

    /// The view whose page shows `tab` now: the base view for its active tab, an extra view for its
    /// bound tab while that view's page is live. `None` for a background tab, and for a bound tab
    /// whose page is idle or not attached yet. The base page is always counted as there: with no page
    /// at all, what is sent to it goes nowhere.
    fn shown_in(&self, tab: TabId) -> Option<ViewId> {
        if !self.tabs.is_shown(tab) {
            return None;
        }
        let view = self.tabs.view_of(tab);
        (view.is_base() || self.pages.contains_key(&view)).then_some(view)
    }

    /// Where a payload about `tab` that must wait for a ready document goes: [`Self::shown_in`] once
    /// that page's document is ready, else `None` (the payload waits for that page's `ready` or the
    /// tab's next switch, as a background tab's does).
    fn page_of(&self, tab: TabId) -> Option<ViewId> {
        self.shown_in(tab).filter(|view| self.ready(*view))
    }
}

/// One chat page the panel can reach, and whether it is routed to. An idle page still takes the
/// theme and the font, and nothing else.
#[derive(Clone)]
struct PageSlot {
    page: Rc<PanelPage>,
    live: bool,
}

/// Every chat page the window built, by the view it shows; shared by every clone of the handle. A
/// page is never moved to another view: a view's page is replaced only by attaching one for it.
#[derive(Clone, Default)]
struct Pages(Rc<RefCell<BTreeMap<ViewId, PageSlot>>>);

impl Pages {
    /// `view`'s page while it is live (routed to).
    fn live(&self, view: ViewId) -> Option<Rc<PanelPage>> {
        self.0
            .borrow()
            .get(&view)
            .filter(|slot| slot.live)
            .map(|slot| slot.page.clone())
    }

    /// `view`'s page, live or idle.
    fn any(&self, view: ViewId) -> Option<Rc<PanelPage>> {
        self.0.borrow().get(&view).map(|slot| slot.page.clone())
    }

    /// Every live page with its view, base first.
    fn live_views(&self) -> Vec<(ViewId, Rc<PanelPage>)> {
        self.0
            .borrow()
            .iter()
            .filter(|(_, slot)| slot.live)
            .map(|(view, slot)| (*view, slot.page.clone()))
            .collect()
    }

    /// Every page, live and idle.
    fn every(&self) -> Vec<Rc<PanelPage>> {
        self.0.borrow().values().map(|slot| slot.page.clone()).collect()
    }

    /// Sends `payload` to `view`'s page if it is live. The map is not borrowed while the page runs.
    fn send(&self, view: ViewId, payload: &str) {
        if let Some(page) = self.live(view) {
            page.send(payload);
        }
    }

    /// Each payload, in order, to `view`'s page if it is live.
    fn send_all(&self, view: ViewId, payloads: Vec<String>) {
        if let Some(page) = self.live(view) {
            dispatch_all(&page, payloads);
        }
    }
}

/// How [`start_deferred`] begins a session.
enum Spawner {
    /// The connect workers (`spawn_connect`, `resume_spawner`).
    Real,
    /// A test's stand-in, told what would have been spawned.
    #[cfg(test)]
    Fake(Rc<RefCell<FakeSpawn>>),
}

/// A test's stand-in spawner: given what would have been started, the receiver its result arrives on.
#[cfg(test)]
type FakeSpawn = dyn FnMut(SpawnCall) -> mpsc::Receiver<Result<AgentBackend, BackendError>>;

/// What a test's [`Spawner::Fake`] is asked to start.
#[cfg(test)]
enum SpawnCall {
    /// A fresh session, or a resume by hand (`resume`).
    Connect {
        resume: Option<String>,
        project: ProjectTrust,
    },
    /// One tab of a restore, with the lease already taken for it.
    Resume {
        id: String,
        lease: agent::lease::SessionLease,
        project: ProjectTrust,
    },
    /// A tab of another agent, fresh or resumed, with the trust plan it was given and, for an ACP
    /// agent, its registration.
    External {
        agent: AgentId,
        plan: agent::TrustPlan,
        mode: SessionModeChoice,
        resume: Option<String>,
        acp: Option<agent::providers::acp::AcpAgentSpec>,
        /// The teardown of the tab's previous backend, which the start waits for (`r` continuing a session).
        after_old: Option<mpsc::Receiver<()>>,
    },
}

/// [`AgentPanelState::toast_hook`]: shows one line in the window's toast.
type ToastHook = Rc<dyn Fn(&str)>;

/// [`AgentPanelState::attention_hook`]: called with the window's attention before a change and after
/// it, and the views handed a card since the last call (`tabs::views_handed_a_card`).
type AttentionHook = Rc<dyn Fn(eitri_core::attention::Attention, eitri_core::attention::Attention, &[ViewId])>;

/// `AgentPanelHandle::on_editor_request`: the request to hand nvim; `Err` says why it could not.
type EditorRequestHook = Rc<dyn Fn(&eitri_core::scratch::ScratchRequest) -> Result<(), String>>;

/// A handle back into this panel's session state, held by `main.rs` alongside the `gtk4::Widget`
/// so a real window close can shut down whatever `AgentSession` exists -- without this, the
/// `Rc<RefCell<AgentPanelState>>` that owns the session is only ever reachable from closures
/// internal to this module (the script-message handler and the pump timer), neither of which
/// ever runs `AgentSession::shutdown()` on its own, so a normal window close would otherwise
/// leak the child process, its hook socket, and any in-memory-only settings backup.
#[derive(Clone)]
pub struct AgentPanelHandle {
    state: Rc<RefCell<AgentPanelState>>,
    /// Every chat page, by the view it shows, held so a document can be re-injected without going
    /// back through the widget tree. Each page and the pacer in front of it are one value, shared
    /// with the pump and the page's own message handler. Pages are attached after the handle is
    /// built ([`AgentPanelHandle::attach_page`]).
    pages: Pages,
    /// `false` when WebKit's sandbox cannot start in this process (`webkit_sandbox::Decision::
    /// Unavailable`): no `WebView` is built, the
    /// panel's place is a GTK notice, and nothing ever starts a session -- sessions start from the
    /// page's own messages, and there is no page. Every method below still runs against the inert
    /// state `main.rs`'s focus, HINT, prefix and tab-verb paths call into; what would reach a page
    /// does nothing, and the tab verbs refuse (`closing`), so `main.rs` flashes rather than opening
    /// tabs nobody can see.
    with_pages: bool,
    /// The wake's raise, held outside the state so raising it never needs the state's cell.
    raise: Option<agent::UiWake>,
}

/// Where the window's link to `eitri-supervisor` stands ([`AgentPanelHandle::supervisor_link`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisorLink {
    /// The supervisor is being started and connected to, off the window's thread: ask again later.
    Connecting,
    /// Connected: an activation arrives on this descriptor, which stays open while it is reported.
    Socket(std::os::fd::RawFd),
    /// None now and none to come (none running, its connection gone, or the window closing).
    Absent,
}

/// What `AgentPanelHandle::new` builds the panel's state from.
pub struct PanelInit {
    pub project_dir: PathBuf,
    pub editor_context: eitri_core::editor_context::ContextSource,
    pub scratch: Option<eitri_core::scratch::ScratchDir>,
    pub backend_kind: BackendKind,
    pub document: &'static PanelDocument,
}

impl AgentPanelHandle {
    /// A handle over `state`, holding the state's wake, with `page` attached as the base view's when
    /// there is one; with none, the handle has no pages at all.
    #[cfg(any(test, feature = "test-support"))]
    fn over(state: Rc<RefCell<AgentPanelState>>, page: Option<Rc<PanelPage>>) -> Self {
        let raise = state.borrow().wake.as_ref().map(crate::panel_wake::PanelWake::waker);
        let handle = AgentPanelHandle {
            state,
            pages: Pages::default(),
            with_pages: page.is_some(),
            raise,
        };
        if let Some(page) = page {
            handle.attach_page(ViewId::BASE, page);
        }
        handle
    }

    /// With pages (`with_pages`): connects to (or starts) the supervisor and builds the state; the pages
    /// are attached afterwards ([`attach_page`](Self::attach_page)). Without (WebKit's sandbox cannot
    /// start): the state alone, no pump and no supervisor client, and every page verb does nothing.
    pub fn new(init: PanelInit, with_pages: bool, host: Rc<dyn PanelHost>) -> Self {
        let backend_kind = init.backend_kind;
        let (supervisor, supervisor_pending) = if with_pages {
            let project_dir = &init.project_dir;
            let instance_id = uuid::Uuid::new_v4().to_string();
            let project_name = project_dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| project_dir.to_string_lossy().to_string());
            let connected = match crate::supervisor_client::SupervisorClient::connect_or_spawn(
                instance_id,
                project_name,
                project_dir,
            ) {
                crate::supervisor_client::PendingSupervisor::Ready(client) => (client, None),
                crate::supervisor_client::PendingSupervisor::Connecting(rx) => (None, Some(rx)),
            };
            println!("[agent_panel] backend: {}", backend_kind.as_str());
            connected
        } else {
            println!(
                "[agent_panel] backend: {} (no panel: WebKit's sandbox cannot start here)",
                backend_kind.as_str()
            );
            (None, None)
        };
        let state = Rc::new(RefCell::new(panel_state(init, supervisor, supervisor_pending, host)));
        let raise = state.borrow().wake.as_ref().map(crate::panel_wake::PanelWake::waker);
        AgentPanelHandle {
            state,
            pages: Pages::default(),
            with_pages,
            raise,
        }
    }

    /// `page` becomes `view`'s live page, starting with a fresh record (no document ready yet): a new
    /// page, or the idle page that showed `view` before, attached again. Whatever page `view` had is
    /// replaced; a page is never moved to another view. The page is given the typing cadence and the
    /// editor's keys as they stand; its document is loaded by [`load_document`](Self::load_document).
    pub fn attach_page(&self, view: ViewId, page: Rc<PanelPage>) {
        self.wake();
        if !self.with_pages {
            return;
        }
        let (cadence, editor_has_keys) = {
            let mut state = self.state.borrow_mut();
            // Whether the view's pane has the keys is the pane's, not the document's: it carries over.
            let pane_focused = state.page(view).is_some_and(|record| record.pane_focused);
            state.pages.insert(
                view,
                PageState {
                    pane_focused,
                    ..PageState::default()
                },
            );
            (state.typing_cadence, state.editor_has_keys)
        };
        {
            let mut pacer = page.pacer().borrow_mut();
            pacer.set_cadence(cadence);
            pacer.set_editor_has_keys(editor_has_keys);
        }
        self.pages.0.borrow_mut().insert(view, PageSlot { page, live: true });
    }

    /// `view`'s page leaves routing: its record is dropped, what its pacer held is let go, the prompts
    /// it showed go, and it is given a blank document at the panel's base ([`IDLE_DOCUMENT`]), so it
    /// shows nothing of the tab it showed and holds no chat while no one sees it; reusing the page
    /// loads the panel again ([`Self::load_document`]). Envelopes sent to it meanwhile are dropped by
    /// the blank page, and its background still follows the theme. The base view's page never goes
    /// idle.
    pub fn idle_page(&self, view: ViewId) {
        self.wake();
        if view.is_base() {
            eprintln!("[agent_panel] the base view's page cannot go idle");
            return;
        }
        let Some(page) = self.pages.any(view) else { return };
        if let Some(slot) = self.pages.0.borrow_mut().get_mut(&view) {
            slot.live = false;
        }
        {
            let mut state = self.state.borrow_mut();
            state.pages.remove(&view);
            if !state.shutting_down {
                state.tabs.drop_bypass_prompt_in(view);
            }
        }
        let discarded = page.pacer().borrow_mut().discard_pending();
        trace_discarded_first_texts(&self.state, discarded);
        page.load_document(IDLE_DOCUMENT, PANEL_BASE_URI);
    }

    /// Paints `view`'s page's background and loads the themed document into it, live or idle; called
    /// once the host listens for the page's messages, and again when an idle page is reused.
    pub fn load_document(&self, view: ViewId) {
        self.wake();
        let Some(page) = self.pages.any(view) else { return };
        let (tokens, document) = {
            let state = self.state.borrow();
            (state.theme.clone(), state.document)
        };
        page.set_background([tokens.bg.r, tokens.bg.g, tokens.bg.b]);
        page.load_document(&document.themed(&tokens.css_vars()), PANEL_BASE_URI);
    }

    /// A message `view`'s page posted (the `eitriAgent` handler that page was built with). A page that
    /// is not live -- idle, or never attached -- has nothing to say: its message is dropped, save the
    /// one late mirror of the tab that just left it (`late_draft_from_idle_page`).
    pub fn page_message(&self, view: ViewId, raw: &str) {
        self.wake();
        match self.pages.live(view) {
            Some(page) => handle_inbound_message(raw, &self.state, &self.pages, view, &page),
            None if late_draft_from_idle_page(raw, &self.state, &self.pages, view) => {}
            None => eprintln!(
                "[agent_panel] view {}: a message from a page that is not live was dropped",
                view.number()
            ),
        }
    }

    /// `view`'s page came on screen, or a float stopped covering it: whatever finished in the tab it
    /// shows while the page was away has been seen (the tray's `agent •`). Nothing is seen while the
    /// page is unmapped or a float covers it (`set_coverage`).
    pub fn page_shown(&self, view: ViewId) {
        self.wake();
        let Some(page) = self.pages.live(view) else { return };
        if self.state.borrow().shutting_down {
            return;
        }
        // Mapped under a float is not seen: the window calls this again once the float is off it.
        if !on_screen(&self.state, view, &page) {
            return;
        }
        let before = self.state.borrow().tabs.attention();
        {
            let mut state = self.state.borrow_mut();
            if let Some(tab) = state.tab_of_page(view) {
                state.tabs.mark_seen(tab);
            }
        }
        report_attention(&self.state, before);
    }

    /// A link in the page was clicked. A markdown link the model rendered must never navigate the
    /// panel itself (that origin would inherit the same message bridge the panel uses to relay
    /// permission decisions), so the host keeps the page on its document and calls this instead. A web
    /// page or a mail draft goes to the system's handler; nothing else is launched: `sms:`, `tel:`,
    /// `xmpp:`, `ftp:` and the like hand the link's data to another program on one click, and a
    /// relative link resolves to the panel's own address, which goes nowhere. A refusal is logged (the
    /// address through `{:?}`, which escapes anything a terminal would act on) and flashed, so the
    /// click is not silently lost.
    pub fn link_clicked(&self, view: ViewId, uri: Option<&str>) {
        self.wake();
        let Some(page) = self.pages.live(view) else { return };
        match uri.and_then(clicked_link_url) {
            Some(url) => {
                // Cloned out of the borrow first: the host runs arbitrary code.
                let host = self.state.borrow().host.clone();
                host.open_external(url);
            }
            None => {
                eprintln!("[agent_panel] link click refused: {uri:?}");
                page.send(&eitri_core::agent_bridge::serialize_notice_for_js(
                    "not opened: this link's address is not one Eitri opens",
                ));
            }
        }
    }
}

impl AgentPanelHandle {
    /// The tick's steady cadence: fast enough that streaming text feels live (a ~30-60fps cadence). The
    /// host runs the tick when [`run_tick`](Self::run_tick) and the wake say so, never sooner than this after
    /// the last one, and on this cadence only while something it acts on cannot say when it is ready
    /// (`crate::panel_wake`).
    pub const TICK: std::time::Duration = std::time::Duration::from_millis(33);

    /// The descriptor the host watches for reading; when it is readable the host calls
    /// [`take_wake`](Self::take_wake). `None`: there is no wake, and the host runs the tick every
    /// [`TICK`](Self::TICK) as [`run_tick`](Self::run_tick) says.
    pub fn wake_fd(&self) -> Option<std::os::fd::RawFd> {
        if !self.with_pages {
            return None;
        }
        self.state.borrow().wake.as_ref().map(crate::panel_wake::PanelWake::fd)
    }

    /// Asks the host to run the tick soon: whatever a call into the panel changed is sent by that tick.
    /// Cheap and idempotent until the host takes it.
    pub fn wake(&self) {
        if let Some(raise) = &self.raise {
            raise();
        }
    }

    /// The wake's descriptor was readable: takes what was raised and says when the tick is due -- now, or
    /// one [`TICK`](Self::TICK) after the last tick if that is later, so a stream of folds is taken in the
    /// batches a steady tick made. `None` when nothing was raised.
    pub fn take_wake(&self, now: std::time::Instant) -> Option<std::time::Instant> {
        let state = self.state.borrow();
        if !state.wake.as_ref().is_some_and(crate::panel_wake::PanelWake::take) {
            return None;
        }
        Some(state.last_tick.map_or(now, |last| (last + Self::TICK).max(now)))
    }

    /// Runs the tick at `now` and says when the host runs it next: on the steady cadence while anything
    /// it acts on is pending (and once more after that, for what a finished worker sent just before it
    /// was seen to finish), at the typing cadence's next release, or only on the wake.
    pub fn run_tick(&self, now: std::time::Instant) -> crate::panel_wake::NextTick {
        use crate::panel_wake::NextTick;
        let polled_before = needs_polling(&self.state.borrow());
        self.state.borrow_mut().last_tick = Some(now);
        if !self.tick() {
            return NextTick::Stop;
        }
        let state = self.state.borrow();
        let mut next = (polled_before || needs_polling(&state)).then_some(now + Self::TICK);
        // Every live page's typing cadence, each against its own document.
        for (view, page) in self.pages.live_views() {
            let paced = page
                .pacer()
                .borrow()
                .next_deadline(std::time::Instant::now(), state.ready(view))
                // Never at the moment this tick started: a deadline that would not move cannot spin the loop.
                .map(|at| at.max(now + std::time::Duration::from_millis(1)));
            next = match (next, paced) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
        next.map_or(NextTick::OnWake, NextTick::At)
    }

    /// The panel's single main-loop tick, over EVERY tab. Three jobs, in order:
    ///
    /// 1. collect every backend that finished constructing on a worker thread (and every handoff whose
    ///    close finished), and answer the command that has been waiting on it;
    /// 2. drain every tab's backend (`TabSet::pump_views`): `take_ui_delivery` is where the permission
    ///    policy answers what needs no human, so a tab that was not pumped would stall. Each shown tab's
    ///    batch -- the base view's active tab's, and each bound tab's -- goes to the page that shows it
    ///    as one `events{tab,fromRevision,throughRevision,events[]}` envelope; a background tab's feeds
    ///    its attention and marks it stale;
    /// 3. report the window's aggregated status to `eitri-supervisor`.
    ///
    /// Then each page's `tabs` envelope and `hello` go out if what they describe changed, and the
    /// extra views left holding no tab are told to the window.
    pub fn tick(&self) -> bool {
        let state = &self.state;
        let pages = &self.pages;
        // No page, no session ever starts and nothing could be shown: nothing to drive.
        if !self.with_pages {
            return false;
        }
        // The window has been closed and its tabs taken; everything below would be a no-op
        // against a destroyed window for as long as the close watch holds the application. Stop
        // instead of ticking 30 times a second at nothing. See `AgentPanelState::shutting_down`.
        if state.borrow().shutting_down {
            return false;
        }
        #[cfg(any(test, feature = "test-support"))]
        {
            state.borrow_mut().ticks_run += 1;
        }
        // Each chat view's host is unmapped while it is hidden or zoomed away; mapped under a float,
        // it is off screen too. Asked before the state is borrowed: the window's probe may read it.
        let live = pages.live_views();
        let mapped: Vec<ViewId> = live
            .iter()
            .filter(|(view, page)| on_screen(state, *view, page))
            .map(|(view, _)| *view)
            .collect();
        // Read before anything below changes it: a session installed this tick resets its count.
        let attention_before = state.borrow().tabs.attention();
        let notes = state.borrow().start_notes.drain();
        pages.send_all(ViewId::BASE, notes);
        collect_pending_starts(state, pages);
        collect_pending_handoffs(state, pages);
        report_sessions_that_never_opened(state, pages);
        let retired = state.borrow_mut().retiring.poll();
        for tab in retired {
            state.borrow_mut().tabs.retire_done(tab);
        }

        // The payloads are built under the borrow and dispatched OUTSIDE it. Calling into WebKit while
        // holding a `RefMut` on the panel's own state is a latent re-entrancy hazard: the script
        // message handler takes the same `RefCell`, and anything that let it run during the
        // dispatch would panic on an already-borrowed cell rather than fail gracefully.
        let (streams, turn_ended, offers_changed, queue_changed, tripped, review_hints) = {
            let mut state_ref = state.borrow_mut();
            let state_ref = &mut *state_ref;
            // A supervisor this window had to start itself finishes connecting here rather than
            // during `build_ui`, where waiting for it would have delayed the window appearing.
            if state_ref.supervisor.is_none() {
                if let Some(rx) = state_ref.supervisor_pending.as_ref() {
                    match rx.try_recv() {
                        Ok(client) => {
                            state_ref.supervisor = client;
                            state_ref.supervisor_pending = None;
                        }
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            state_ref.supervisor_pending = None;
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    }
                }
            }
            // The project root travels in because the permission policy judges `Read`/`Grep`/`Glob`
            // paths against it -- see `AgentBackend::answer_what_needs_no_human`. A view counts as
            // mapped only while its page is live and on screen.
            let project_dir = state_ref.project_dir.clone();
            let out = state_ref.tabs.pump_views(&project_dir, &|view| mapped.contains(&view));
            let statuses: Vec<supervisor::AgentStatus> = state_ref
                .tabs
                .tabs()
                .iter()
                .map(|t| {
                    // Bound first: `projection()` returns a guard on the sidecar path, and passing
                    // it inline would drop the ingestion lock before `derive_status` had read
                    // through it.
                    let projection = t.live().map(|b| b.projection());
                    crate::supervisor_client::derive_status(projection.as_deref())
                })
                .collect();
            let status = crate::supervisor_client::aggregate_status(statuses);
            if let Some(supervisor) = state_ref.supervisor.as_mut() {
                supervisor.send_status(status);
            }
            // Whose first text each payload carries, kept with it: the cadence may release it after
            // the tab, or that tab's turn, has changed (`panel_pacer::FirstText`).
            let mut streams: Vec<(ViewId, Stream)> = Vec::new();
            if let Some(payload) = out.active_payload {
                let active = state_ref.tabs.active();
                let first_text = if out.first_text {
                    first_text_of(&state_ref.tabs, active)
                } else {
                    None
                };
                streams.push((ViewId::BASE, (payload, first_text, out.active_class)));
            }
            for shown in out.view_payloads {
                let first_text = if shown.first_text {
                    first_text_of(&state_ref.tabs, shown.tab)
                } else {
                    None
                };
                streams.push((shown.view, (shown.payload, first_text, shown.class)));
            }
            (
                streams,
                out.turn_ended,
                out.offers_changed,
                out.queue_changed,
                out.tripped,
                out.review_hints,
            )
        };
        // The CLI-mode tripwire failed these tabs: each session is shut down off
        // this thread, as a closed tab's is, and the shutdown denies what is still pending in it.
        // A tab on screen in some page also says why there, as a session that never opened does.
        for tripped in tripped {
            let shown = state.borrow().shown_in(tripped.tab);
            // The pump counted it as retiring for its tab.
            state.borrow_mut().retiring.backend(tripped.backend, Some(tripped.tab));
            if let Some(view) = shown {
                pages.send(view, &serialize_error_for_js(tripped.tab, &tripped.reason));
            }
        }
        // Each stream goes through its own page's typing cadence (`crate::panel_pacer`): held for the
        // next slot while the user types in the editor, if it is plain streamed content; at once
        // otherwise, and always at once into a page that has not said `ready` (its snapshot
        // follows), as before. Whatever a slot or the end of the typing window releases goes out
        // in this same tick. A payload for a view with no live page has nowhere to go: that page's
        // `ready` sends the tab's whole state.
        for (view, page) in &live {
            let page_ready = state.borrow().ready(*view);
            {
                let sink = page.sink();
                let mut pacer = page.pacer().borrow_mut();
                let now = std::time::Instant::now();
                for (_, (payload, first_text, class)) in streams.iter().filter(|(to, _)| to == view) {
                    let class = if page_ready {
                        *class
                    } else {
                        eitri_core::panel_cadence::EnvelopeClass::Immediate
                    };
                    pacer.send_stream(&sink, payload.clone(), *first_text, class, now);
                }
                pacer.poll(&sink, now, page_ready);
            }
            let sent = page.pacer().borrow_mut().take_sent();
            // Stamped at the dispatch call that really sent it -- where the WebView's own clock starts,
            // which under the cadence may be a later tick or a flush between ticks -- for the tab and
            // turn it was pumped for.
            stamp_first_text_dispatches(state, sent.first_text);
            if sent.any {
                let mut state_ref = state.borrow_mut();
                let shows = state_ref.tab_of_page(*view);
                if let Some(trace) = shows
                    .and_then(|tab| state_ref.tabs.get_mut(tab))
                    .and_then(|t| t.turn_trace.as_mut())
                {
                    if trace.is_complete() {
                        trace.emit();
                    }
                }
            }
        }
        // A message put into a turn was read (its row went out above) or is held again.
        for tab in queue_changed {
            send_queue(state, pages, tab);
        }
        // A turn ended in these tabs; their queued messages go out now.
        for tab in turn_ended {
            let flush = state.borrow_mut().tabs.flush_queue(tab);
            if let Some(flush) = flush {
                apply_flush(state, pages, tab, flush);
            }
        }
        // A turn's end snapshot landed: the status band's hint, in the page of the hint's tab's view. A
        // page that has not said `ready` (still loading, or reloading) has nothing to show it, so it
        // waits here for the page; the review itself is asked for by `c`, never by this.
        let hints = {
            let mut state_ref = state.borrow_mut();
            let state_ref = &mut *state_ref;
            forget_hints_of_running_or_closed_tabs(state_ref);
            let review_hints = hints_to_keep(state_ref.review_hint, review_hints);
            keep_newest_hint_per_tab(&mut state_ref.review_hints, review_hints);
            take_hints_for_ready_pages(state_ref)
        };
        if !hints.is_empty() {
            let sent: Vec<_> = hints.iter().map(|(_, hint)| hint.clone()).collect();
            keep_shown_hints(&mut state.borrow_mut().shown_hints, &sent);
        }
        for (view, hint) in &hints {
            pages.send(*view, &eitri_core::agent_bridge::serialize_review_hint_for_js(hint));
        }
        // A shown tab's offers changed (a card arrived or went): its page's third buttons follow.
        for tab in offers_changed {
            let payload = {
                let state_ref = state.borrow();
                state_ref.page_of(tab).and_then(|view| {
                    state_ref.tabs.get(tab).map(|t| {
                        (
                            view,
                            eitri_core::agent_bridge::serialize_rule_offers_for_js(t.id, &t.rule_offers),
                        )
                    })
                })
            };
            if let Some((view, payload)) = payload {
                pages.send(view, &payload);
            }
        }
        send_context_if_changed(state, pages);
        poll_scratch_edits(state, pages);
        poll_review_jobs(state, pages);
        poll_review_flow(state, pages);
        // Before the tabs envelope, so a session the gate started this tick shows as starting at once.
        poll_trust(state, pages);
        apply_title_reads(state);
        send_chooser_titles_if_changed(state, pages);
        send_tabs_if_changed(state, pages);
        remember_tabs(state);
        send_hello_if_open_sessions_changed(state, pages);
        // After the payload, not before it: a reaction that sends the panel an envelope -- one
        // that lands on a card, say -- must find the card it names already there.
        report_attention(state, attention_before);
        judge_empty_views(state);
        tell_emptied_views(state);
        true
    }

    /// Shuts down every tab's backend, INCLUDING one still being constructed on a worker thread,
    /// all of them in parallel under one backstop.
    ///
    /// The in-flight case is not hypothetical: since construction moved off the GTK main loop, a
    /// window closed during a connect leaves a fully-built backend -- a live sidecar process and a
    /// Tokio runtime thread -- owned by nothing but a channel nobody will ever read.
    ///
    /// Taking every tab (`TabSet::take_all`) is load-bearing twice over: it lets a backend that
    /// already finished be shut down properly, and it leaves nothing for a later tick to install a
    /// session into -- the pump stops on `shutting_down` anyway.
    ///
    /// **Every live backend is torn down at once** (`tear_down_all`): one worker per tab, one
    /// collector, one backstop. Serial sidecar closes are bounded at 10 s each, so N tabs torn down
    /// in turn could outlive any backstop.
    ///
    /// **This function does not wait for anything, and that is its most important property.** It
    /// runs from `window.connect_close_request` on the GTK main thread, where a wait keeps the
    /// window mapped and frozen until it returns; two previous versions of this code waited there
    /// (3s, then 15s) while describing themselves as not doing so. Every branch below instead puts
    /// its receiver in the returned [`ShutdownWatch`], which the host hands to `hold_until_done`:
    /// **this function** returns immediately and the process stays alive -- with no window on
    /// screen -- until the teardowns report or their backstops expire.
    ///
    /// **Scoped to the panel deliberately:** the
    /// whole close handler does NOT return immediately: `main.rs`'s `connect_close_request` calls
    /// `pane.shutdown()` first, on the GTK thread, and that spins until nvim exits
    /// (`LiveHarness::shutdown` in the fork). So "the window disappears at once" is a claim about
    /// this half only, and a slow `:qa!` -- a hung plugin, a slow `BufWritePre` -- still delays the
    /// close with nothing here involved.
    ///
    /// **A handoff's close** is already running on its own worker, which owns that backend: there
    /// is nothing to start, only a reason to keep the process alive. It is held on `dropped_rx`,
    /// not `closed_rx` -- `closed_rx` reports when `shutdown()` returned, while on the sidecar path
    /// the child dies in the backend's drop afterwards. `HANDOFF_CLOSE_BACKSTOP` is generous for
    /// the legacy backend and knowingly short of the sidecar's worst case; see its own doc.
    ///
    /// The hold belongs to the host (`hold_until_done`): this handle is per-window, and the process
    /// the watch keeps alive belongs to the application that owns the loop this panel's timers run on.
    pub fn shutdown(&self) -> ShutdownWatch {
        self.wake();
        // Queued words outlive the window as history.
        let texts = self.state.borrow_mut().tabs.take_every_queued_text();
        if !texts.is_empty() {
            let (dir, root) = {
                let s = self.state.borrow();
                (s.history_dir.clone(), s.project_dir.clone())
            };
            if let Some(dir) = dir {
                if let Err(e) = eitri_core::prompt_history::append(&dir, &root, &texts) {
                    eprintln!("[history] the queue could not be kept: {e}");
                }
            }
        }
        // The tabs as they are now, whatever the last tick saw: the writer's last set, written before the
        // close lets the process go.
        remember_tabs(&self.state);
        let last_write = self.state.borrow_mut().tab_memory.finish();
        let tabs = {
            let mut state = self.state.borrow_mut();
            // Read by the tick, by `poll_activate` and by every path into the tab set, all of
            // which keep running now that the loop outlives the window. See the field's own doc.
            state.shutting_down = true;
            state.tabs.take_all()
        };
        let mut entries = Vec::new();
        let mut backends = Vec::new();
        for tab in tabs {
            if let Some(handoff) = tab.pending_handoff {
                entries.push(watched(
                    "a terminal handoff's close",
                    handoff.dropped_rx,
                    HANDOFF_CLOSE_BACKSTOP,
                    |()| Vec::new(),
                ));
            }
            match tab.backend {
                TabBackend::Live(backend) => backends.push(backend),
                TabBackend::Starting(pending) => entries.push(watch_connect(pending.result_rx)),
                // A start still waiting for the trust question has spawned nothing.
                TabBackend::NotStarted | TabBackend::AwaitingTrust(_) | TabBackend::Failed { .. } => {}
            }
        }
        entries.extend(tear_down_all("every session tab's teardown", backends));
        if let Some(rx) = last_write {
            entries.push(watched_abandoning(
                "the saved tabs' last write",
                rx,
                SAVED_TABS_BACKSTOP,
                "the write is left unfinished: the saved-tabs file stays the one before it, whole, without this \
                 window's last change to its tabs",
                |()| Vec::new(),
            ));
        }
        // Tabs that left earlier, while the window stayed open: their teardowns and connects are
        // still running on workers nothing else would keep alive. Same holds and backstops as the tabs just taken.
        let retiring = std::mem::take(&mut self.state.borrow_mut().retiring);
        entries.extend(retiring.connects.into_iter().map(watch_connect));
        for (_, rx) in retiring.teardowns {
            entries.push(watched("a closed tab's teardown", rx, SESSION_CLOSE_BACKSTOP, |()| {
                Vec::new()
            }));
        }
        ShutdownWatch { entries }
    }

    /// Records whether `view`'s page's document said `ready` (nothing for a page with no record).
    fn set_document_ready(&self, view: ViewId, ready: bool) {
        let mut state = self.state.borrow_mut();
        if let Some(record) = state.pages.get_mut(&view) {
            record.document_ready = ready;
        }
    }

    /// Reloads the panel's frontend from scratch, deliberately WITHOUT touching the session.
    ///
    /// The frontend is a pure view layer over `AgentSessionProjection`, which lives here in Rust --
    /// so a WebView that has wedged (a frontend panic leaving a blank pane, a bridge that stopped
    /// dispatching) can be thrown away and rebuilt while the real conversation keeps running. The
    /// fresh document sends `ready` on mount, and `Ready`'s handler answers with `hello` plus a
    /// snapshot of the still-live session, which rehydrates the replacement document.
    ///
    /// `PageSurface::load_document` (WebKit's `load_html`), NOT `WebViewExt::reload()`. The latter
    /// reproducibly leaves this WebView permanently blank with zero further page activity, because the panel's document
    /// is a substitute-data load rather than a fetchable URI -- so the recovery action would itself
    /// be the wedge it exists to undo.
    ///
    /// **The invariant this rests on, stated because it is not obvious and a later change could
    /// break it silently:** `take_ui_delivery()` CONSUMES events. Every envelope the tick
    /// dispatches between this document load and the new document's own `ready` is therefore gone --
    /// the old document is being torn down and the new one has no listener yet. Nothing is lost
    /// only because `Ready`'s handler answers from canonical state with a full snapshot rather than
    /// a replay, and `serialize_snapshot_for_js` already carries tool-call results. A pump that
    /// ever stopped being the sole consumer, or a snapshot that stopped being complete, would turn
    /// this into a quiet data-loss path with no test failing.
    pub(crate) fn reload_document(&self, view: ViewId) {
        let Some(page) = self.pages.live(view) else {
            eprintln!(
                "[agent_panel] no panel document to reload in view {}: no live page shows it",
                view.number()
            );
            return;
        };
        eprintln!(
            "[agent_panel] reloading the panel document; the session is left untouched (view {})",
            view.number()
        );
        // The page that said `ready` is going; the tick holds its envelopes until the new one does.
        self.set_document_ready(view, false);
        // What the typing cadence still holds is in the projection, hence in the new page's snapshot.
        let discarded = page.pacer().borrow_mut().discard_pending();
        trace_discarded_first_texts(&self.state, discarded);
        // And so is the mode it reported: until the new page's `ready`, nothing is BROWSE or INPUT.
        forget_the_pages_nav_mode(&self.state.borrow(), view);
        let (vars, document) = {
            let state = self.state.borrow();
            (state.theme.css_vars(), state.document)
        };
        page.load_document(&document.themed(&vars), PANEL_BASE_URI);
    }

    /// The user's OWN recovery action for a wedged or crashed panel -- `prefix r`
    /// (`Action::PanelReload`) or the top-bar `\u{21bb}` button (`install_reload_action`) -- as
    /// distinct from the AUTOMATIC reload `page_gone` performs while still within
    /// `crash_guard`'s own budget.
    ///
    /// Re-arms `crash_guard` (`WebViewCrashGuard::reset`) before doing exactly what
    /// [`reload_document`](Self::reload_document) does. **Why a separate method rather than putting
    /// the reset inside `reload_document` itself:** `page_gone`'s own `Reload` branch calls
    /// `reload_document` directly, on every crash still within budget -- if that shared path reset
    /// the guard too, every automatic reload would erase its own crash history and `GiveUp` could
    /// never be reached, defeating the loop guard entirely. Only a genuinely manual recovery may
    /// re-arm it; the automatic path must keep counting toward the very loop it exists to detect.
    pub fn reload_document_by_hand(&self, view: ViewId) {
        self.wake();
        {
            let mut state = self.state.borrow_mut();
            if let Some(record) = state.pages.get_mut(&view) {
                record.crash_guard.reset();
            }
        }
        self.reload_document(view);
    }

    /// The page's content process ended: a real WebKit crash, or the host
    /// genuinely out of memory. Until the signal was connected the chat stayed blank until the user
    /// found `\u{21bb}`/`prefix r` by hand.
    /// Recovery is exactly [`reload_document`](Self::reload_document): the session lives in Rust,
    /// so a fresh document rehydrates from its own `ready`/`hello` round trip with nothing lost.
    ///
    /// Guarded by `crash_guard` (`webview_crash_guard::WebViewCrashGuard`) so a page that cannot
    /// even finish loading does not turn into a reload/crash loop. The host leaves out an end it asked
    /// for itself (WebKit's `TerminatedByApi`): that is no crash and must not count toward the guard's
    /// budget. Nothing on this panel's view ends its process today, but the Lua-panel's call site does.
    pub fn page_gone(&self, view: ViewId, reason: &str) {
        self.wake();
        // The page that reported BROWSE or INPUT is dead, and the give-up document never sends
        // `ready`: left as it was, `main.rs` would keep claiming `Ctrl+j`/`Ctrl+k` for a page that
        // cannot answer.
        forget_the_pages_nav_mode(&self.state.borrow(), view);
        // An idle page's crash is reloaded by its next attach; nothing is routed to it meanwhile.
        let response = {
            let mut state = self.state.borrow_mut();
            state.pages.get_mut(&view).map(|record| record.crash_guard.on_crash())
        };
        let Some(response) = response else { return };
        match response {
            crate::webview_crash_guard::CrashResponse::Reload => {
                eprintln!(
                    "[agent_panel] the WebView's web process terminated ({reason}); reloading it (view {})",
                    view.number()
                );
                self.reload_document(view);
            }
            crate::webview_crash_guard::CrashResponse::GiveUp => {
                eprintln!(
                    "[agent_panel] the WebView's web process kept terminating; giving up on automatic reload (view {})",
                    view.number()
                );
                self.set_document_ready(view, false);
                let html = crate::webview_crash_guard::crash_message_html(
                    "Reload it by hand with \u{21bb} in the top bar, or prefix r.",
                );
                if let Some(page) = self.pages.live(view) {
                    let discarded = page.pacer().borrow_mut().discard_pending();
                    trace_discarded_first_texts(&self.state, discarded);
                    page.load_document(&html, PANEL_BASE_URI);
                }
            }
            crate::webview_crash_guard::CrashResponse::AlreadyGivenUp => {}
        }
    }

    /// Non-blocking: returns `true` if `eitri-supervisor` asked this window to come to the
    /// front since the last call. `main.rs` polls this on its own timer and calls
    /// `window.present()` in response -- this handle has no `Window` reference of its own (see
    /// `AgentPanelHandle`'s own top-level doc: window lifecycle stays owned by `main.rs`).
    pub fn poll_activate(&self) -> bool {
        let mut state = self.state.borrow_mut();
        // The window this would raise is gone; see `AgentPanelState::shutting_down`.
        if state.shutting_down {
            return false;
        }
        if let Some(supervisor) = state.supervisor.as_mut() {
            supervisor.poll_activate()
        } else {
            false
        }
    }

    /// Where the window's link to `eitri-supervisor` stands, for a host that waits on its socket instead
    /// of polling [`poll_activate`](Self::poll_activate).
    pub fn supervisor_link(&self) -> SupervisorLink {
        let state = self.state.borrow();
        if state.shutting_down {
            return SupervisorLink::Absent;
        }
        match (&state.supervisor, &state.supervisor_pending) {
            (Some(client), _) => client
                .activate_fd()
                .map_or(SupervisorLink::Absent, SupervisorLink::Socket),
            (None, Some(_)) => SupervisorLink::Connecting,
            (None, None) => SupervisorLink::Absent,
        }
    }

    /// Records `tokens` as the panel's current theme, repaints the WebView's own background, and
    /// pushes the variables to the live document.
    ///
    /// Safe to call before the page has loaded: the dispatch is guarded, and the `ready` handshake
    /// sends the recorded envelope anyway, so neither ordering loses it.
    pub fn set_theme(&self, tokens: &eitri_core::theme::ThemeTokens) {
        self.wake();
        let payload = eitri_core::agent_bridge::serialize_theme_for_js(tokens);
        self.state.borrow_mut().theme = tokens.clone();
        // Every page, idle ones too: a reused page must not show the old colours for a frame.
        for page in self.pages.every() {
            page.set_background([tokens.bg.r, tokens.bg.g, tokens.bg.b]);
            page.send_direct(&payload);
        }
    }

    /// Updates only the recorded theme's `font_size_px` and re-sends it -- the zoom-together
    /// design's path (`text_size::TextSizeController`) for pushing a new panel scale without
    /// re-deriving any other themed colour or token. Built on `set_theme`, so both a panel reload
    /// (`reload_document`, which reads straight from the recorded theme) and a fresh `ready`
    /// handshake (whose `hello` theme payload comes from the same recorded field) pick the new
    /// size up automatically -- neither has a separate path that could fall out of sync with it.
    pub fn set_panel_font_size_px(&self, font_size_px: f32) {
        self.wake();
        let tokens = {
            let mut tokens = self.state.borrow().theme.clone();
            tokens.font_size_px = font_size_px;
            tokens
        };
        self.set_theme(&tokens);
    }

    /// The twin of [`set_panel_font_size_px`](Self::set_panel_font_size_px), for
    /// `--nv-editor-row`: updates only the recorded theme's `editor_row_px` and
    /// re-sends it, so both a panel reload and a fresh `ready` handshake pick the editor's cell
    /// height up the same way `font_size_px` already does, with no new envelope. `main.rs` calls
    /// this from `NeovideEditorPane::connect_cell_size_changed` and on a `gtk-xft-dpi` change, in
    /// the panel's CSS px -- already converted from the editor's GTK logical px by
    /// `webkit_zoom::EditorRow`.
    pub fn set_editor_row_px(&self, editor_row_px: f32) {
        self.wake();
        let tokens = {
            let mut tokens = self.state.borrow().theme.clone();
            tokens.editor_row_px = Some(editor_row_px);
            tokens
        };
        self.set_theme(&tokens);
    }
}

impl AgentPanelHandle {
    /// Dispatches one already-serialized envelope to the live document, guarded so it is safe to
    /// call before the page has loaded (in which case `ready`'s own batch re-sends whatever state
    /// mattered, and this call is simply a no-op).
    fn dispatch(&self, view: ViewId, payload: String) {
        // No live page for the view: nothing is listening, as before a page loads.
        let Some(page) = self.pages.live(view) else { return };
        Self::dispatch_to(&page, &payload);
    }

    /// [`dispatch`](Self::dispatch) to every page, idle ones included (the keymap, how thinking is
    /// shown, the editor link). An idle page's blank document runs nothing and drops it.
    fn dispatch_every(&self, payload: String) {
        for page in self.pages.every() {
            Self::dispatch_to(&page, &payload);
        }
    }

    /// [`dispatch`](Self::dispatch) to every live page.
    fn dispatch_live(&self, payload: String) {
        for (_, page) in self.pages.live_views() {
            Self::dispatch_to(&page, &payload);
        }
    }

    /// The one send past the pacer for a dispatch: behind whatever the typing cadence is holding, as
    /// `PanelPage::send` is.
    fn dispatch_to(page: &PanelPage, payload: &str) {
        page.flush();
        page.send_direct(payload);
    }

    /// The user pressed a key in the editor and it reached nvim (`NeovideEditorPane::
    /// connect_key_activity`). Starts, or extends, the typing window that paces the stream
    /// (`crate::panel_pacer`). O(1): it runs in the press's path.
    ///
    /// Wakes the tick only when the key began a typing window: a key inside one moves only the window's
    /// end, which the tick already waits for, and a wake per keystroke would cost a tick per keystroke.
    pub fn note_editor_key(&self) {
        let now = std::time::Instant::now();
        let mut began = false;
        for page in self.pages.every() {
            began |= page.pacer().borrow_mut().note_editor_key(now);
        }
        if began {
            self.wake();
        }
    }

    /// Whether the editor holds the window's keys (`pane_focus`). Losing them ends the typing window
    /// at once: the next tick releases what is held.
    pub fn set_editor_has_keys(&self, has_keys: bool) {
        self.wake();
        self.state.borrow_mut().editor_has_keys = has_keys;
        for page in self.pages.every() {
            page.pacer().borrow_mut().set_editor_has_keys(has_keys);
        }
    }

    /// `agent.typing_cadence_hz` as `init.lua` left it (`None`: `"off"`), applied once after it ran, to
    /// every page and to each page attached later.
    pub fn set_typing_cadence(&self, cadence_hz: Option<u32>) {
        self.wake();
        self.state.borrow_mut().typing_cadence = cadence_hz;
        for page in self.pages.every() {
            page.pacer().borrow_mut().set_cadence(cadence_hz);
        }
    }

    /// `agent.restore` as `init.lua` left it, applied once after it ran. `Off` also stops the tabs
    /// being remembered at all and forgets what the last window left.
    pub fn set_restore_policy(&self, policy: RestorePolicy) {
        self.wake();
        let mut state = self.state.borrow_mut();
        state.restore_policy = policy;
        if policy == RestorePolicy::Off {
            state.tab_memory.disable();
            state.restore_source = None;
        }
    }

    /// `agent.default_mode` as `init.lua` left it (`None`: unset), applied once after it ran. A mode
    /// Shift+Tab left for this project keeps winning; otherwise the empty tab the window already has,
    /// and every new one, start in it (an agent that offers no manual mode starts in auto instead).
    /// Naming bypass here is the user's own answer to the question that is otherwise asked every time,
    /// so saved bypass tabs also come back in bypass unasked.
    pub fn set_default_mode(&self, mode: Option<SessionModeChoice>) {
        self.wake();
        let mut state = self.state.borrow_mut();
        state.bypass_by_config = mode == Some(SessionModeChoice::Bypass);
        let Some(mode) = mode else { return };
        let remembered = state.mode_remembered.then(|| state.tabs.default_mode());
        if remembered.is_none() {
            state.tabs.apply_configured_default(mode);
        }
        // Said here, where it is decided, so the log never claims a mode new tabs do not start in.
        eprintln!("{}", default_mode_said(mode, remembered));
    }

    /// `agent.default` as `init.lua` left it, applied once after it ran: every tab made without naming
    /// an agent, and the empty tab the window already has, run it.
    pub fn set_default_agent(&self, agent: AgentId) {
        self.wake();
        let mut state = self.state.borrow_mut();
        state.tabs.apply_configured_agent(agent.clone());
        state.default_agent = agent;
    }

    /// `agent.codex.command` as `init.lua` left it (`None`: unset), read by every Codex start.
    pub fn set_codex_command(&self, command: Option<String>) {
        self.wake();
        self.state.borrow_mut().codex_command = command;
    }

    /// The ACP agents `eitri.agent.register` accepted, applied once after `init.lua` ran.
    pub fn set_acp_agents(&self, agents: Vec<AgentRegistration>) {
        self.wake();
        self.state.borrow_mut().acp_agents = agents;
    }

    /// `review.enabled` and `review.hint` as `init.lua` left them, applied once after it ran. Enabled
    /// installs the turn review (the legacy backend has no turn boundaries to follow and says so);
    /// disabled leaves none, and a request for one says why. The hint only decides whether a
    /// finished turn writes to the status band.
    pub fn set_review(&self, review: ReviewConfig) {
        self.wake();
        let payload = {
            let mut state = self.state.borrow_mut();
            state.apply_review(review);
            review_settings_payload(&state)
        };
        self.dispatch_live(payload);
    }

    /// Where a restore's one-line result goes. `main.rs` installs this once.
    pub fn on_toast(&self, hook: impl Fn(&str) + 'static) {
        self.wake();
        self.state.borrow_mut().toast_hook = Some(Rc::new(hook));
    }

    /// Records whether `view`'s pane has keyboard focus and tells its live document, which
    /// dims its mode block when it does not. Safe before the page has loaded for the same reason
    /// `set_theme` is: the dispatch is guarded, and `ready` re-sends the recorded value.
    pub fn set_pane_focused(&self, view: ViewId, focused: bool) {
        self.wake();
        {
            let mut state = self.state.borrow_mut();
            if let Some(record) = state.pages.get_mut(&view) {
                record.pane_focused = focused;
            }
            if !focused && !state.shutting_down {
                // A pending bypass prompt is about the tab the user was just looking at in this
                // page. Losing the keys (focus moved to another module, e.g. the editor, the
                // terminal or another chat view) must drop it, the same way switching tabs already
                // does (`TabSet::select`), so a `y` that slips through afterwards lands on nothing.
                // Another page's prompts are still on screen there, and stay.
                state.tabs.drop_bypass_prompt_in(view);
            }
        }
        self.dispatch(view, eitri_core::agent_bridge::serialize_pane_focus_for_js(focused));
    }

    /// `agent.thinking` as `init.lua` left it, applied once after it ran. Recorded for every later
    /// `ready` and told to a document that is already listening (the dispatch is guarded, as
    /// `set_pane_focused`'s is).
    pub fn set_thinking_display(&self, display: eitri_core::agent_prefs::ThinkingDisplay) {
        self.wake();
        self.state.borrow_mut().thinking_display = display;
        self.dispatch_every(eitri_core::agent_bridge::serialize_display_for_js(display));
    }

    /// Asks the panel to open its composer with the caret in it. Sent only from the new-tab path
    /// now; see `serialize_enter_input_for_js`. Safe before
    /// the page loads (the dispatch is guarded), in which case there is no composer yet and nothing
    /// happens.
    pub fn enter_input(&self, view: ViewId) {
        self.wake();
        self.drop_bypass_prompt_in(view);
        self.dispatch(view, eitri_core::agent_bridge::serialize_enter_input_for_js());
    }

    /// Where every other keyboard arrival lands: BROWSE -- on the last row with following resumed if
    /// the reader was following it, else on the row and scroll they left (2026-09-29; the page decides, from what it parked when the keys left). Sent after `Ctrl+l`/a
    /// tray chip/`prefix a`'s miss have moved GTK focus into the panel (this reverses the 2026-09-19
    /// behaviour of `Ctrl+l` going straight into the composer). Mirrors `enter_input` exactly
    /// except for the envelope it dispatches; safe before the page loads for the same reason.
    pub fn arrive(&self, view: ViewId) {
        self.wake();
        self.drop_bypass_prompt_in(view);
        self.dispatch(view, eitri_core::agent_bridge::serialize_arrive_for_js());
    }

    /// Every keyboard route GTK claims before the WebView sees a key is a route away
    /// from a bypass prompt. The panel cancels its own copy on the matching envelope; this drops
    /// Rust's, so a `y` that slipped through is refused by the nonce. HINT (`prefix f`) and `prefix ,` left the prompt open, and the
    /// next ordinary key -- the `y` that copies a code block HINT landed on, a name starting with
    /// `y` -- entered bypass and approved the waiting cards. The same now holds for `arrive`,
    /// `enter_input`, `nav_key`, `literal_key`, `prefix ?`, `prefix i`, `prefix &`, `<leader>bo` and
    /// a card landing, besides `pane_focus false` and the tab switches that already dropped it. Only
    /// the prompts of the page the route is in (`view`): another page's are still on its screen.
    fn drop_bypass_prompt_in(&self, view: ViewId) {
        let mut state = self.state.borrow_mut();
        if !state.shutting_down {
            state.tabs.drop_bypass_prompt_in(view);
        }
    }

    /// `view`'s page's last-reported effective mode, for `main.rs`'s
    /// `Ctrl+j`/`Ctrl+k` intercept to decide against (`claims`). `Other` before the page has ever
    /// posted one, and again after every `ready` -- see `PanelKeys::Other`'s own doc.
    pub fn panel_keys(&self, view: ViewId) -> PanelKeys {
        self.state
            .borrow()
            .page(view)
            .map_or(PanelKeys::Other, |record| record.nav_mode.get())
    }

    /// Tells the page that `Ctrl+j` was claimed in BROWSE, or
    /// `Ctrl+k` claimed in INPUT. Safe before the page loads for the same reason `arrive` is --
    /// there is no live mirror yet, so `main.rs`'s intercept never calls this before one exists.
    pub fn nav_key(&self, view: ViewId, direction: NavKeyDirection) {
        self.wake();
        self.drop_bypass_prompt_in(view);
        self.dispatch(view, serialize_nav_key_for_js(direction));
    }

    /// The agent host's `Ctrl+j`/`Ctrl+k` intercept for a module-navigation controller: a chord the
    /// panel's own `panel_keys` mirror claims goes to the panel as a nav key and the closure answers
    /// `true`; every other direction answers `false`, so the host moves focus as it would without one.
    pub fn nav_intercept(&self, view: ViewId) -> Rc<dyn Fn(Direction) -> bool> {
        let agent = self.clone();
        Rc::new(move |direction: Direction| {
            if !claims(agent.panel_keys(view), direction) {
                return false;
            }
            let direction = match direction {
                Direction::Down => NavKeyDirection::Down,
                Direction::Up => NavKeyDirection::Up,
                Direction::Left | Direction::Right => {
                    unreachable!("claims() only claims Down (in Browse) or Up (in Input)")
                }
            };
            agent.nav_key(view, direction);
            true
        })
    }

    /// The `?` overlay's rows (`serialize_keymap_for_js`): recorded for every later `ready`, and
    /// sent now to a document that is already up.
    pub fn set_keymap_help(&self, payload: String) {
        self.wake();
        self.state.borrow_mut().keymap_help = Some(payload.clone());
        self.dispatch_every(payload);
    }

    /// `send-prefix`/`send-keys` with `view`'s page holding the keys.
    pub fn literal_key(&self, view: ViewId, key: &eitri_core::keymap::KeySpec) {
        self.wake();
        self.drop_bypass_prompt_in(view);
        self.dispatch(
            view,
            eitri_core::agent_bridge::serialize_literal_key_for_js(&key.to_string()),
        );
    }

    /// `prefix ?`: open the `?` overlay in `view`'s page, with the section of the plugin page that held the
    /// keys, if one did. `opening` numbers this opening; the page quotes it back when the overlay closes,
    /// so a close that arrives after a newer opening can be told apart.
    pub fn open_keymap(&self, view: ViewId, plugin: Option<eitri_core::plugin::keys::PluginKeysHelp>, opening: u64) {
        self.wake();
        self.drop_bypass_prompt_in(view);
        self.dispatch(
            view,
            eitri_core::agent_bridge::serialize_open_keymap_for_js(plugin.as_ref(), opening),
        );
    }

    /// `prefix :`: open `view`'s page's `:` command line, which runs nothing.
    pub fn open_command_line(&self, view: ViewId) {
        self.wake();
        self.drop_bypass_prompt_in(view);
        self.dispatch(view, eitri_core::agent_bridge::serialize_open_command_line_for_js());
    }

    /// Global `f` HINT, in the base page only: an extra view is labelled as a module, its rows are
    /// not. Each is a one-line dispatch of the matching `serialize_hint_*_for_js` envelope;
    /// `shell::hint` drives the session, this handle only relays it to the WebView.
    pub fn hint_collect(&self, session_id: u64) {
        self.wake();
        self.drop_bypass_prompt_in(ViewId::BASE);
        self.dispatch(
            ViewId::BASE,
            eitri_core::agent_bridge::serialize_hint_collect_for_js(session_id),
        );
    }
    pub fn hint_show(&self, session_id: u64, labels: &[String]) {
        self.wake();
        self.dispatch(
            ViewId::BASE,
            eitri_core::agent_bridge::serialize_hint_show_for_js(session_id, labels),
        );
    }
    pub fn hint_prefix(&self, session_id: u64, typed: &str) {
        self.wake();
        self.dispatch(
            ViewId::BASE,
            eitri_core::agent_bridge::serialize_hint_prefix_for_js(session_id, typed),
        );
    }
    pub fn hint_land(&self, session_id: u64, index: usize) {
        self.wake();
        self.dispatch(
            ViewId::BASE,
            eitri_core::agent_bridge::serialize_hint_land_for_js(session_id, index),
        );
    }
    pub fn hint_end(&self, session_id: u64) {
        self.wake();
        self.dispatch(
            ViewId::BASE,
            eitri_core::agent_bridge::serialize_hint_end_for_js(session_id),
        );
    }

    /// Every card answer, bypass `y` and trust answer from a page passes this, with the page's view, after
    /// the page check and before anything is answered; `Err(why)` refuses it and leaves the card or question
    /// waiting, and `why` is what the page shows. The window sets it once.
    pub fn set_answer_gate(&self, gate: Rc<dyn Fn(ViewId) -> Result<(), String>>) {
        self.wake();
        self.state.borrow_mut().answer_gate = Some(gate);
    }

    /// Whether a float covers a chat view's module now, asked with the view whenever the panel decides
    /// if that view's page is on screen for attention (a turn finishing, a tab switch, the page coming
    /// on screen). The window sets it once, and calls [`AgentPanelHandle::page_shown`] when a float
    /// stops covering a view.
    pub fn set_coverage(&self, covered: Rc<dyn Fn(ViewId) -> bool>) {
        self.wake();
        self.state.borrow_mut().coverage = Some(covered);
    }

    /// Where the panel's two HINT messages go. `shell::hint::HintCoordinator` installs this once.
    pub fn on_hint(&self, hook: impl Fn(HintInbound) + 'static) {
        self.wake();
        self.state.borrow_mut().hint_hook = Some(Rc::new(hook));
    }

    /// Where the scratch round trips (`Ctrl+g`, `gf`) hand nvim their request. The hook shows and
    /// focuses the editor, then sends its keys; `Err` says why it could not. `main.rs` installs this once.
    pub fn on_editor_request(
        &self,
        hook: impl Fn(&eitri_core::scratch::ScratchRequest) -> Result<(), String> + 'static,
    ) {
        self.wake();
        self.state.borrow_mut().editor_request_hook = Some(Rc::new(hook));
    }

    /// The window's way to ask its editor something (the embedded pane, or the companion's link).
    /// `main.rs` and the companion window install this once.
    pub fn set_editor_rpc(&self, rpc: Rc<dyn eitri_core::editor_rpc::EditorRpc>) {
        self.wake();
        self.state.borrow_mut().editor_rpc = Some(rpc);
    }

    /// Who owns the review module in this window's editor, asked before every review request: the
    /// integrated window's own nvim, or the companion panel's channel. `None` from it: no editor.
    pub fn set_review_owner(&self, owner: impl Fn() -> Option<eitri_core::review_editor::Owner> + 'static) {
        self.wake();
        self.state.borrow_mut().review_editor.set_owner(Rc::new(owner));
    }

    /// Called after the editor opened a file for the review: the window brings the editor to the
    /// user (shows and focuses it, or raises the user's own window). `main.rs` and the companion
    /// window install this once.
    pub fn on_review_open(&self, hook: impl Fn() + 'static) {
        self.wake();
        self.state.borrow_mut().review_editor.set_opened(Rc::new(hook));
    }

    /// The editor went away, or its drafts were dropped: what the review overlay installed and
    /// drew there is forgotten, and an open still waiting on it is told it will not be answered.
    pub fn review_editor_lost(&self) {
        self.wake();
        let outs = self.state.borrow_mut().review_flow.editor_lost();
        dispatch_review_outs(&self.state, &self.pages, outs);
    }

    /// Where the panel stands with the editor beside it (companion mode), kept so a document that
    /// loads or reloads is told, and sent when it changes. Never called by the one-window mode.
    pub fn set_editor_link(&self, link: eitri_core::companion::attach::BandLink) {
        self.wake();
        let payload = eitri_core::agent_bridge::serialize_editor_link_for_js(link.state, &link.text);
        {
            let mut state = self.state.borrow_mut();
            if state.editor_link.as_ref() == Some(&payload) {
                return;
            }
            state.editor_link = Some(payload.clone());
        }
        self.dispatch_every(payload);
    }

    /// The editor went away or was swapped: every draft edit still out in it ends (a body saved in
    /// nvim becomes the draft). Each tab on screen is told now, in its own page; the others when they
    /// are switched to. Says so once if any edit was cut, and returns how many. It does not return
    /// the keys to the chat: the editor left, so nothing should take the focus.
    pub fn editor_detached(&self) -> usize {
        self.wake();
        let ended = {
            let mut state = self.state.borrow_mut();
            let ended = end_pending_edits(&mut state);
            ended
                .into_iter()
                .map(|(tab, draft)| (tab, draft, state.shown_in(tab)))
                .collect::<Vec<_>>()
        };
        for (tab, draft, shown) in &ended {
            let Some(view) = shown else { continue };
            if let Some(text) = draft {
                self.dispatch(*view, eitri_core::agent_bridge::serialize_draft_for_js(*tab, text));
            }
            self.dispatch(*view, eitri_core::agent_bridge::serialize_scratch_for_js(*tab, false));
        }
        if !ended.is_empty() {
            self.dispatch(
                ViewId::BASE,
                eitri_core::agent_bridge::serialize_notice_for_js("draft editing stopped: the editor went away"),
            );
        }
        ended.len()
    }

    /// One line in the base page's band, in the notice's own style. Used by the companion window for
    /// what it cannot say any other way (a desktop it cannot move focus on).
    pub fn show_notice(&self, text: &str) {
        self.wake();
        self.dispatch(ViewId::BASE, eitri_core::agent_bridge::serialize_notice_for_js(text));
    }

    /// Called when a draft edited in nvim came back (or was discarded), with the view of the edit's
    /// tab: the keys return to that chat view. `main.rs` installs this once.
    pub fn on_editor_done(&self, hook: impl Fn(ViewId) + 'static) {
        self.wake();
        self.state.borrow_mut().editor_done_hook = Some(Rc::new(hook));
    }

    /// Called with the value before and after whenever [`AgentPanelHandle::attention`] changes, or
    /// one tab's own attention does ([`AgentPanelHandle::attention_of`] for its view) -- the one place
    /// that knows both, so `main.rs` keeps no copy of its own to drift -- with the views handed a card
    /// since the last call, the view holding the newest of them last. `main.rs` installs this once.
    pub fn on_attention(
        &self,
        hook: impl Fn(eitri_core::attention::Attention, eitri_core::attention::Attention, &[ViewId]) + 'static,
    ) {
        self.wake();
        self.state.borrow_mut().attention_hook = Some(Rc::new(hook));
    }

    /// Where a `tab_verb` message's mapped `TabAction` goes: the same code the prefix's own
    /// `Action::Tab` arm runs (`main.rs`'s `run_tab_action`). `None`
    /// until `main.rs` installs one, which is why `InboundMessage::TabVerb` can be refused.
    /// The hook is told the view of the page the verb came from. `main.rs` installs this once.
    pub fn on_tab_verb(&self, hook: impl Fn(ViewId, TabAction) + 'static) {
        self.wake();
        self.state.borrow_mut().tab_verb_hook = Some(Rc::new(hook));
    }

    /// Where a `nav_fallthrough` message's direction goes: `main.rs` wires this to
    /// `move_focus(&ModuleId::agent(), dir)`, i.e. exactly what the chord would have done had the
    /// panel never intercepted it. A `pane_nav` (`Ctrl+w h/j/k/l`) goes through the
    /// same hook, for it is the same move. The hook is told the view of the page the move came from, the
    /// module it moves from. `main.rs` installs this once.
    pub fn on_nav_fallthrough(&self, hook: impl Fn(ViewId, Direction) + 'static) {
        self.wake();
        self.state.borrow_mut().nav_fallthrough_hook = Some(Rc::new(hook));
    }

    /// Where a `keymap_closed` message goes: the opening it closes, and whether the overlay's own keys (or a
    /// click outside it) closed it. `main.rs` installs this once; without it the message does nothing.
    pub fn on_keymap_closed(&self, hook: impl Fn(u64, bool) + 'static) {
        self.wake();
        self.state.borrow_mut().keymap_closed_hook = Some(Rc::new(hook));
    }
}

/// The session-tab verbs `main.rs` binds. Each borrows, changes
/// the set, drops the borrow, then dispatches. Every one of them does nothing once the window is
/// closing (`AgentPanelState::shutting_down`: the set is empty then). A verb about "the tab on
/// screen" names the view whose page has the keys and acts on that page's tab.
// The keymap and the window close will call these; until they do, some have no caller.
#[allow(dead_code)]
impl AgentPanelHandle {
    /// The window is closing (`AgentPanelState::shutting_down`), or there are no pages at all
    /// (`with_pages`'s own doc): every verb below then does nothing, and says so to its caller the way
    /// it does for a closing window.
    fn closing(&self) -> bool {
        !self.with_pages || self.state.borrow().shutting_down
    }

    /// `prefix c`: opens a tab running the window's default agent (`agent.default`) in the base view,
    /// selects it, and sends `tabs` -- nothing else: an empty tab has no state yet.
    pub fn new_tab(&self) {
        self.wake();
        let agent = self.state.borrow().default_agent.clone();
        self.new_tab_for(agent);
    }

    /// [`new_tab`](Self::new_tab) for a named agent (`:tabnew <agent>`).
    pub fn new_tab_for(&self, agent: AgentId) {
        self.wake();
        if self.closing() {
            return;
        }
        self.state.borrow_mut().tabs.open(agent);
        send_tabs(&self.state, &self.pages);
    }

    /// `prefix <digit>`: a base-view tab is selected there; a tab bound to an extra view asks the window
    /// to show that view (`on_show_view`). Where the tab is, or `None` when no tab has that number (the
    /// caller flashes).
    pub fn select_number(&self, n: u16) -> Option<TabPlace> {
        self.wake();
        if self.closing() {
            return None;
        }
        let place = self.state.borrow_mut().tabs.select_number(n)?;
        match place {
            TabPlace::Base(_) => send_switch(&self.state, &self.pages),
            TabPlace::View(view, _) => show_view(&self.state, view),
        }
        Some(place)
    }

    /// `prefix n` (+1) / `p` (-1), wrapping, over the base view's tabs.
    pub fn step(&self, delta: i32) -> bool {
        self.wake();
        self.switch_with(|tabs| tabs.step(delta))
    }

    /// `prefix l`: the base view's previously active tab.
    pub fn select_last(&self) -> bool {
        self.wake();
        self.switch_with(eitri_core::tab_set::TabSet::select_last)
    }

    fn switch_with(&self, select: impl FnOnce(&mut eitri_core::tab_set::TabSet) -> Option<TabId>) -> bool {
        if self.closing() {
            return false;
        }
        let selected = select(&mut self.state.borrow_mut().tabs);
        if selected.is_none() {
            return false;
        }
        send_switch(&self.state, &self.pages);
        true
    }

    /// `prefix ,`: the inline rename field on the label of the tab `view`'s page shows.
    pub fn begin_rename(&self, view: ViewId) {
        self.wake();
        if self.closing() {
            return;
        }
        self.drop_bypass_prompt_in(view);
        let payload = {
            let state = self.state.borrow();
            let Some(tab) = state.tab_of_page(view).and_then(|tab| state.tabs.get(tab)) else {
                return;
            };
            eitri_core::agent_bridge::serialize_begin_rename_for_js(tab.id, tab.name.as_deref())
        };
        self.dispatch(view, payload);
    }

    /// `prefix &`: the footer's y/n prompt for the tab `view`'s page shows.
    pub fn confirm_close(&self, view: ViewId) {
        self.wake();
        if self.closing() {
            return;
        }
        // The panel's close prompt replaces a bypass prompt on screen; Rust's copy goes with it.
        self.drop_bypass_prompt_in(view);
        let payload = {
            let state = self.state.borrow();
            let Some((tab, facts)) = state
                .tab_of_page(view)
                .and_then(|tab| state.tabs.close_facts(tab).map(|facts| (tab, facts)))
            else {
                return;
            };
            eitri_core::agent_bridge::serialize_confirm_close_for_js(tab, &eitri_core::tabs::close_prompt(&facts))
        };
        self.dispatch(view, payload);
    }

    /// `<leader>bo` in the base view: the footer's y/n prompt over every base-view tab but the active
    /// one. Nothing to dispatch, and no flash, when the active tab is the only one -- `<leader>bo`
    /// with one tab is simply nothing to do, the same as `prefix n`/`p` with one tab (unlike a verb
    /// naming a tab that does not exist, which flashes).
    pub fn confirm_close_others(&self) {
        self.wake();
        if self.closing() {
            return;
        }
        self.drop_bypass_prompt_in(ViewId::BASE);
        let Some((tabs, prompt)) = self.state.borrow().tabs.close_others_plan() else {
            return;
        };
        let payload = eitri_core::agent_bridge::serialize_confirm_close_others_for_js(&tabs, &[prompt]);
        self.dispatch(ViewId::BASE, payload);
    }

    /// `prefix w` in `view`'s page: every open tab, and the records of that page's tab's agent.
    pub fn open_chooser(&self, view: ViewId) {
        self.wake();
        if self.closing() {
            return;
        }
        // Opening the chooser is one of the routes that cancels an outstanding bypass
        // prompt in this page -- it was about the tab the user is leaving to see this list. A fresh
        // one raised from inside the chooser (its own Shift+Tab) makes its own plan afterwards.
        self.drop_bypass_prompt_in(view);
        let (payload, listed) = chooser_payload(&self.state.borrow(), view);
        {
            let mut state = self.state.borrow_mut();
            state.chooser_listed = listed;
            state.chooser_view = view;
        }
        self.dispatch(view, payload);
    }

    /// `prefix i`: the detail popover of the tab `view`'s page shows.
    pub fn open_detail(&self, view: ViewId) {
        self.wake();
        if self.closing() {
            return;
        }
        self.drop_bypass_prompt_in(view);
        let payload = {
            let state = self.state.borrow();
            let Some(tab) = state.tab_of_page(view) else { return };
            detail_payload(&state, tab)
        };
        self.dispatch(view, payload);
    }

    /// The tray chip, `prefix a`: the oldest pending card among `view`'s tabs, with `view`'s page's
    /// cursor put on it. The base view switches to the tab holding it; an extra view shows its one
    /// tab already. `false` when none of `view`'s tabs holds a card.
    pub fn focus_oldest_card(&self, view: ViewId) -> bool {
        self.wake();
        if self.closing() {
            return false;
        }
        let target = {
            let mut state = self.state.borrow_mut();
            let Some(target) = oldest_card_in(&state.tabs, view) else {
                return false;
            };
            if view.is_base() {
                state.tabs.select(target);
            }
            // A landing on a card is a route away even when it names the tab already on
            // screen, which `select` alone does not treat as a move.
            state.tabs.drop_bypass_prompt_in(view);
            target
        };
        if view.is_base() {
            send_switch(&self.state, &self.pages);
        }
        self.dispatch(
            view,
            eitri_core::agent_bridge::serialize_focus_permission_for_js(target),
        );
        true
    }

    /// The view whose tab holds the window's oldest pending card.
    pub fn oldest_card_view(&self) -> Option<ViewId> {
        self.wake();
        let state = self.state.borrow();
        if state.shutting_down {
            return None;
        }
        state.tabs.oldest_card_place().map(|place| match place {
            TabPlace::Base(_) => ViewId::BASE,
            TabPlace::View(view, _) => view,
        })
    }

    /// The view whose tab holds the window's newest pending card (where attention points).
    pub fn newest_card_view(&self) -> Option<ViewId> {
        self.wake();
        let state = self.state.borrow();
        if state.shutting_down {
            return None;
        }
        state.tabs.newest_card_tab().map(|tab| state.tabs.view_of(tab))
    }

    /// What `view`'s tabs owe the user: the base view's summed, or an extra view's one tab's.
    pub fn attention_of(&self, view: ViewId) -> eitri_core::attention::Attention {
        self.wake();
        let state = self.state.borrow();
        if state.shutting_down {
            return Default::default();
        }
        state.tabs.attention_of(view)
    }

    /// What the chat owes the user now, summed over every tab: its pending permission
    /// cards, and whether a turn finished while it was off screen.
    pub fn attention(&self) -> eitri_core::attention::Attention {
        let state = self.state.borrow();
        if state.shutting_down {
            return Default::default();
        }
        state.tabs.attention()
    }

    /// The tab holding the newest pending card, as `(number, label name)`: the toast names it.
    pub fn newest_card_label(&self) -> Option<(u16, String)> {
        let state = self.state.borrow();
        if state.shutting_down {
            return None;
        }
        let id = state.tabs.newest_card_tab()?;
        state.tabs.get(id).map(|t| (t.number, t.label_name()))
    }

    /// [`AgentPanelHandle::newest_card_label`] among `view`'s tabs only.
    pub fn newest_card_label_in(&self, view: ViewId) -> Option<(u16, String)> {
        let state = self.state.borrow();
        if state.shutting_down {
            return None;
        }
        let id = state.tabs.newest_card_tab_in(view)?;
        state.tabs.get(id).map(|t| (t.number, t.label_name()))
    }

    /// `prefix x` on a chat view (2026-09-26): every tab of `view` is closed exactly as `y` to
    /// `prefix &` closes one (`close_tab`: a running turn interrupted, queued words kept as history,
    /// the backend shut down and dropped on a worker the window-close backstop still covers, the
    /// record never deleted, so each session stays resumable). The base view is left with one fresh
    /// empty tab, as closing its last tab always leaves it, and every extra view's tab keeps running;
    /// an extra view loses its one tab. How many tabs were closed.
    ///
    /// Refused, closing nothing, while one of them is being handed off to a terminal: `close_tab`
    /// refuses that tab, and a kill that closed the others and left it would not be a kill.
    pub fn close_view_tabs(&self, view: ViewId) -> Result<usize, &'static str> {
        self.wake();
        if self.closing() {
            return Ok(0);
        }
        let ids: Vec<TabId> = {
            let state = self.state.borrow();
            let ids: Vec<TabId> = if view.is_base() {
                state.tabs.base_tabs()
            } else {
                state.tabs.tab_in(view).into_iter().collect()
            };
            if ids
                .iter()
                .any(|id| state.tabs.get(*id).is_some_and(|t| t.pending_handoff.is_some()))
            {
                return Err("a tab is still being handed off to a terminal; kill the chat once that finishes");
            }
            ids
        };
        for id in &ids {
            close_tab(&self.state, &self.pages, *id)?;
        }
        eprintln!(
            "[agent_panel] killed: {} session tab(s) closed, records kept (view {})",
            ids.len(),
            view.number()
        );
        tell_emptied_views(&self.state);
        Ok(ids.len())
    }

    /// `prefix !` (tmux's `break-pane`) with the keys in the base view: its active tab moves to the
    /// empty extra view `view`, and the base page shows the tab it falls back to. The moved tab.
    pub fn break_out(&self, view: ViewId) -> Result<TabId, String> {
        self.wake();
        if self.closing() {
            return Err("the window is closing".to_string());
        }
        let moved = self
            .state
            .borrow_mut()
            .tabs
            .break_out(view)
            .map_err(|e| e.to_string())?;
        send_switch(&self.state, &self.pages);
        send_view(&self.state, &self.pages, view);
        send_moved_hint(&self.state, &self.pages, moved, view);
        Ok(moved)
    }

    /// The tab `view` shows comes back into the base view as its active tab; `view` is left empty,
    /// which the window is told (`on_view_empty`). The tab that came back.
    pub fn join_back(&self, view: ViewId) -> Result<TabId, String> {
        self.wake();
        if self.closing() {
            return Err("the window is closing".to_string());
        }
        let tab = self
            .state
            .borrow_mut()
            .tabs
            .join_back(view)
            .ok_or_else(|| format!("agent {} shows no tab", view.number()))?;
        send_switch(&self.state, &self.pages);
        send_moved_hint(&self.state, &self.pages, tab, ViewId::BASE);
        self.state.borrow_mut().emptied_views.push(view);
        tell_emptied_views(&self.state);
        Ok(tab)
    }

    /// A new chat view's tab: a new empty tab of the window's default agent, bound to the empty extra
    /// view `view`; the base view's active tab is unchanged. The new tab.
    pub fn open_in(&self, view: ViewId) -> Result<TabId, String> {
        self.wake();
        if self.closing() {
            return Err("the window is closing".to_string());
        }
        let agent = self.state.borrow().default_agent.clone();
        let tab = self
            .state
            .borrow_mut()
            .tabs
            .open_in(agent, view)
            .map_err(|e| e.to_string())?;
        send_view(&self.state, &self.pages, view);
        Ok(tab)
    }

    /// The extra views that hold a tab, ascending.
    pub fn views(&self) -> Vec<ViewId> {
        self.wake();
        let state = self.state.borrow();
        if state.shutting_down {
            return Vec::new();
        }
        state.tabs.views()
    }

    /// The label of the tab the extra view `view` shows, as its strip names it (`<n> <name>`): what
    /// `prefix x` on that view says it closes. `None` when `view` holds no tab, and for the base view,
    /// whose kill closes every base-view tab.
    pub fn view_tab_label(&self, view: ViewId) -> Option<String> {
        self.wake();
        let state = self.state.borrow();
        if state.shutting_down {
            return None;
        }
        let tab = state.tabs.tab_in(view).and_then(|tab| state.tabs.get(tab))?;
        Some(eitri_core::tabs::label(tab.number, &tab.label_name()))
    }

    /// What closing the extra view `view`'s tab would end, as the window-close prompt counts it: whether
    /// its turn is running or its session still connecting (0 or 1), and its queued messages. `(0, 0)`
    /// when `view` holds no tab, and for the base view.
    pub fn view_running_queued(&self, view: ViewId) -> (usize, usize) {
        self.wake();
        let state = self.state.borrow();
        if state.shutting_down {
            return (0, 0);
        }
        let Some(tab) = state.tabs.tab_in(view).and_then(|tab| state.tabs.get(tab)) else {
            return (0, 0);
        };
        let running = matches!(tab.backend, TabBackend::Starting(_)) || tab.turn_running();
        (usize::from(running), tab.queue.len())
    }

    /// The extra chat views the window's layout holds, in any workspace, told before any restore so a
    /// restored tab saved in one of them comes back into it.
    pub fn set_layout_views(&self, views: Vec<ViewId>) {
        self.wake();
        self.state.borrow_mut().tabs.set_layout_views(views);
    }

    /// Whether the last window's tabs are settled: none on offer, no restore question open and no
    /// restore run in flight. Until then an extra view holding no tab may still be given one.
    pub fn restore_settled(&self) -> bool {
        self.wake();
        restore_settled(&self.state.borrow())
    }

    /// Told once per extra view left holding no tab: its tab closed, joined back or a failed restore
    /// discarded it, and, once the restore has settled, each attached extra view that never got one.
    /// The window removes the view. `main.rs` installs this once.
    pub fn on_view_empty(&self, hook: impl Fn(ViewId) + 'static) {
        self.wake();
        self.state.borrow_mut().view_empty_hook = Some(Rc::new(hook));
    }

    /// Told when a pick asks the window to show a view and give it the keys: a chooser pick, a number
    /// key or a resume naming a tab bound to an extra view, or a resume from an extra page that opened
    /// in a new base-view tab. `main.rs` installs this once.
    pub fn on_show_view(&self, hook: impl Fn(ViewId) + 'static) {
        self.wake();
        self.state.borrow_mut().show_view_hook = Some(Rc::new(hook));
    }

    /// Tabs whose turn is running or whose session is still connecting.
    pub fn running_count(&self) -> usize {
        let state = self.state.borrow();
        if state.shutting_down {
            return 0;
        }
        state.tabs.running_count()
    }

    /// Every tab's queued messages, summed (the window-close prompt).
    pub fn queued_count(&self) -> usize {
        let state = self.state.borrow();
        if state.shutting_down {
            return 0;
        }
        state.tabs.queued_count()
    }
}

/// The oldest pending card among `view`'s tabs.
fn oldest_card_in(tabs: &eitri_core::tab_set::TabSet, view: ViewId) -> Option<TabId> {
    let stamps: Vec<(TabId, Option<u64>)> = tabs
        .tabs()
        .iter()
        .filter(|t| t.view_id() == view)
        .map(|t| (t.id, t.attention.oldest_pending_stamp()))
        .collect();
    eitri_core::tabs::oldest_card(&stamps)
}

/// Asks the window to show `view` and give it the keys. The hook is cloned out of the borrow first:
/// it moves the layout's focus, which reaches back into the panel.
fn show_view(state: &Rc<RefCell<AgentPanelState>>, view: ViewId) {
    let hook = state.borrow().show_view_hook.clone();
    if let Some(hook) = hook {
        hook(view);
    }
}

/// Whether the last window's tabs are settled ([`AgentPanelHandle::restore_settled`]). `agent.restore
/// = "auto"` keeps its source until the base page's first `ready` uses it up.
fn restore_settled(state: &AgentPanelState) -> bool {
    let on_offer = restore_is_on_offer(state)
        || (state.restore_policy == RestorePolicy::Auto
            && state.restore_source.is_some()
            && !state.ready(ViewId::BASE));
    !on_offer && !state.tabs.restore_question_open() && !restore_waits_for_trust(state) && state.restore_run.is_none()
}

/// Once the restore has settled, every attached extra view still holding no tab is owed to the
/// window as empty, once per window.
fn judge_empty_views(state: &Rc<RefCell<AgentPanelState>>) {
    let mut state = state.borrow_mut();
    if state.empty_views_judged || state.shutting_down || !restore_settled(&state) {
        return;
    }
    state.empty_views_judged = true;
    let empty: Vec<ViewId> = state
        .pages
        .keys()
        .copied()
        .filter(|view| !view.is_base() && state.tabs.tab_in(*view).is_none())
        .collect();
    for view in empty {
        if !state.emptied_views.contains(&view) {
            state.emptied_views.push(view);
        }
    }
}

/// Tells the window of each extra view left empty since it was last told, once each, with the state
/// released: the window removes the view and idles its page, which comes back into the panel.
fn tell_emptied_views(state: &Rc<RefCell<AgentPanelState>>) {
    let (views, hook) = {
        let mut state = state.borrow_mut();
        let mut views = std::mem::take(&mut state.emptied_views);
        views.dedup();
        // A view given a tab again since (a restore, a new tab) is not empty any more.
        views.retain(|view| state.tabs.tab_in(*view).is_none());
        (views, state.view_empty_hook.clone())
    };
    for view in views {
        eprintln!("[agent_panel] view {}: no tab left", view.number());
        if let Some(hook) = &hook {
            hook(view);
        }
    }
}

/// Tells the attention hook, if the window's value changed or any tab's own did (or a tab moved to
/// another view): each chat view's chip shows only its own tabs, so a view that turns unread while
/// another already is must still be redrawn, and two views handed a card in one pump must both be
/// named. The hook is cloned out of the borrow first: it reaches the layout and the top bar, and must
/// never find this panel's state borrowed.
fn report_attention(state: &Rc<RefCell<AgentPanelState>>, before: eitri_core::attention::Attention) {
    let (after, hook, handed) = {
        let mut state = state.borrow_mut();
        if state.shutting_down {
            return;
        }
        let after = state.tabs.attention();
        let now = state.tabs.tab_attentions();
        if after == before && now == state.reported_tabs {
            return;
        }
        let handed = eitri_core::tabs::views_handed_a_card(&state.reported_tabs, &now);
        state.reported_tabs = now;
        (after, state.attention_hook.clone(), handed)
    };
    if let Some(hook) = hook {
        hook(before, after, &handed);
    }
}

/// The `review_settings` envelope for the state as it is: whether `c` can open a review, and whether
/// the user asked to be told about a turn's changes.
fn review_settings_payload(state: &AgentPanelState) -> String {
    eitri_core::agent_bridge::serialize_review_settings_for_js(state.review_enabled, state.review_hint)
}

/// The three values a turn review is built from.
struct ReviewRoots {
    state_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
    canonical_root: PathBuf,
}

/// `init.lua`'s two review keys, applied once after it ran.
pub struct ReviewConfig {
    pub enabled: bool,
    pub hint: bool,
}

impl AgentPanelState {
    /// The state half of [`AgentPanelHandle::set_review`], which needs no page.
    fn apply_review(&mut self, review: ReviewConfig) {
        self.review_hint = review.hint;
        self.review_enabled = false;
        if review.enabled {
            let roots = &self.review_roots;
            let installed = eitri_core::turn_review::TurnReview::new(
                roots.state_home.as_deref(),
                roots.home.as_deref(),
                &roots.canonical_root,
            );
            match self.tabs.install_turn_review(installed) {
                Ok(()) => self.review_enabled = true,
                Err(why) => eprintln!("[agent_panel] {why}"),
            }
        } else {
            self.tabs.turn_review_off();
            eprintln!("[review] turn review is off (init.lua's review.enabled = false)");
        }
    }
}

/// Holds this window's presence for every other window's reverts: its window file, and a file per
/// running turn, kept by a thread of its own. It runs for every backend (the legacy one installs
/// no review) and with the review switched off, because a window that reverts must be able to see
/// this one either way. With no state directory nothing can be held, which is said once; every
/// revert in this window is then refused as not holding the presence lock.
fn hold_window_presence(
    tabs: &mut eitri_core::tab_set::TabSet,
    state_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
    root: &Path,
    pid: u32,
) {
    match eitri_core::turn_review::review_dir_for(state_home, home, root) {
        Some(dir) => tabs.hold_presence(eitri_core::turn_review::PresenceHolder::start(dir, pid)),
        None => eprintln!("[review] no state directory: this window holds no presence lock, so reverts are refused"),
    }
}

/// The panel's state, the same with a page or without one (`build_agent_panel`'s `unavailable`):
/// the tab set in the project's remembered mode, its prompt history and saved permission rules.
fn panel_state(
    init: PanelInit,
    supervisor: Option<crate::supervisor_client::SupervisorClient>,
    supervisor_pending: Option<std::sync::mpsc::Receiver<Option<crate::supervisor_client::SupervisorClient>>>,
    host: Rc<dyn PanelHost>,
) -> AgentPanelState {
    let PanelInit {
        project_dir,
        editor_context,
        scratch,
        backend_kind,
        document,
    } = init;
    let state_home = std::env::var_os("XDG_STATE_HOME");
    let home = std::env::var_os("HOME");
    let prefs_dir = eitri_core::agent_prefs::state_dir(state_home.as_deref(), home.as_deref());
    let (mode, notes) = eitri_core::agent_prefs::startup_mode(prefs_dir.as_deref(), &project_dir);
    for note in notes {
        eprintln!("{note}");
    }
    let mode_remembered = eitri_core::agent_prefs::remembers_a_choice(prefs_dir.as_deref(), &project_dir);
    let history_dir = eitri_core::prompt_history::state_dir(state_home.as_deref(), home.as_deref());
    let rules_dir = eitri_core::permission_store::state_dir(state_home.as_deref(), home.as_deref());
    let (history, notes) = eitri_core::prompt_history::startup(history_dir.as_deref(), &project_dir);
    for note in notes {
        eprintln!("{note}");
    }
    let (rules, notes) = eitri_core::permission_store::startup(rules_dir.as_deref(), &project_dir);
    for note in notes {
        eprintln!("{note}");
    }
    // The key a session lease is taken under (`AgentConversation` canonicalizes its cwd the same
    // way). `main.rs` already canonicalized the root; this only makes the string explicit.
    let canonical_root = project_dir.canonicalize().unwrap_or_else(|_| project_dir.clone());
    let mut tabs = eitri_core::tab_set::TabSet::new(backend_kind, mode);
    tabs.set_rules(rules);
    let wake = panel_wake(&mut tabs);
    let titles = eitri_core::transcript_titles::TitleCache::new(wake.as_ref().map(crate::panel_wake::PanelWake::waker));
    // Before anything review-related, and whatever the backend or `review.enabled` say: another
    // window's revert has to see this window, and this window's running turns.
    hold_window_presence(
        &mut tabs,
        state_home.as_deref(),
        home.as_deref(),
        &canonical_root,
        std::process::id(),
    );
    // Turn review is installed (or switched off) by `set_review`, once `init.lua` has run.
    let review_roots = ReviewRoots {
        state_home: state_home.clone(),
        home: home.clone(),
        canonical_root: canonical_root.clone(),
    };
    let canonical_project_dir = canonical_root.to_string_lossy().into_owned();
    let tabs_dir = eitri_core::saved_tabs::state_dir(state_home.as_deref(), home.as_deref());
    let (restore_source, notes) = eitri_core::saved_tabs::startup(tabs_dir.as_deref(), &project_dir);
    for note in notes {
        eprintln!("{note}");
    }
    let trust = trust_gate_for(state_home.as_deref(), home.as_deref(), &canonical_root);
    AgentPanelState {
        document,
        host,
        trust,
        spawner: Spawner::Real,
        tabs,
        tab_memory: TabWriter::new(tabs_dir, project_dir.clone()),
        titles,
        hello_titles_stale: false,
        chooser_listed: Vec::new(),
        chooser_titles_stale: false,
        conversation_id: agent::conversation_id_for_cwd(&canonical_root),
        restore_source,
        restore_policy: RestorePolicy::Offer,
        bypass_by_config: false,
        mode_remembered,
        default_agent: AgentId::Claude,
        codex_command: None,
        acp_agents: Vec::new(),
        start_notes: StartNotes::default(),
        restore_run: None,
        toast_hook: None,
        editor_context,
        backend_kind,
        project_dir,
        canonical_project_dir,
        prefs_dir,
        supervisor,
        supervisor_pending,
        shutting_down: false,
        wake,
        last_tick: None,
        #[cfg(any(test, feature = "test-support"))]
        ticks_run: 0,
        theme: eitri_core::theme::ThemeTokens::fallback(),
        keymap_help: None,
        // The base view's record is there from the start: its page is the window's, attached or not.
        pages: BTreeMap::from([(ViewId::BASE, PageState::default())]),
        view_empty_hook: None,
        show_view_hook: None,
        emptied_views: Vec::new(),
        empty_views_judged: false,
        chooser_view: ViewId::BASE,
        review_requests: Vec::new(),
        trust_commands: BTreeMap::new(),
        next_trust_command: 0,
        start_pages: BTreeMap::new(),
        typing_cadence: Some(eitri_core::panel_cadence::DEFAULT_CADENCE_HZ),
        editor_has_keys: false,
        thinking_display: eitri_core::agent_prefs::ThinkingDisplay::Collapsed,
        hint_hook: None,
        answer_gate: None,
        coverage: None,
        attention_hook: None,
        reported_tabs: Vec::new(),
        scratch,
        pending_edits: Vec::new(),
        review_jobs: Vec::new(),
        review_hints: Vec::new(),
        shown_hints: Vec::new(),
        review_hint: false,
        review_enabled: false,
        review_roots,
        editor_request_hook: None,
        editor_rpc: None,
        review_flow: eitri_core::turn_review::ReviewFlow::new(),
        review_editor: crate::review_editor::ReviewEditor::default(),
        editor_done_hook: None,
        retiring: Retiring::default(),
        resume_after: std::collections::HashMap::new(),
        history_dir,
        rules_dir,
        history,
        editor_link: None,
        tab_verb_hook: None,
        nav_fallthrough_hook: None,
        keymap_closed_hook: None,
    }
}

/// A panel state with no supervisor, no disk, no trust worker and nothing installed: what the tests here,
/// and a host's own tests through [`AgentPanelHandle::for_host_test`], build on.
#[cfg(any(test, feature = "test-support"))]
fn bare_state(
    document: &'static PanelDocument,
    host: Rc<dyn PanelHost>,
    mut tabs: eitri_core::tab_set::TabSet,
) -> AgentPanelState {
    let wake = panel_wake(&mut tabs);
    let titles = eitri_core::transcript_titles::TitleCache::new(wake.as_ref().map(crate::panel_wake::PanelWake::waker));
    AgentPanelState {
        document,
        host,
        tabs,
        editor_context: Rc::new(|| None),
        backend_kind: BackendKind::Sidecar,
        project_dir: PathBuf::new(),
        canonical_project_dir: String::new(),
        prefs_dir: None,
        supervisor: None,
        supervisor_pending: None,
        shutting_down: false,
        wake,
        last_tick: None,
        #[cfg(any(test, feature = "test-support"))]
        ticks_run: 0,
        theme: eitri_core::theme::ThemeTokens::fallback(),
        keymap_help: None,
        // The base view's record is there from the start: its page is the window's, attached or not.
        pages: BTreeMap::from([(ViewId::BASE, PageState::default())]),
        view_empty_hook: None,
        show_view_hook: None,
        emptied_views: Vec::new(),
        empty_views_judged: false,
        chooser_view: ViewId::BASE,
        review_requests: Vec::new(),
        trust_commands: BTreeMap::new(),
        next_trust_command: 0,
        start_pages: BTreeMap::new(),
        typing_cadence: Some(eitri_core::panel_cadence::DEFAULT_CADENCE_HZ),
        editor_has_keys: false,
        thinking_display: eitri_core::agent_prefs::ThinkingDisplay::Collapsed,
        hint_hook: None,
        answer_gate: None,
        coverage: None,
        attention_hook: None,
        reported_tabs: Vec::new(),
        scratch: None,
        pending_edits: Vec::new(),
        review_jobs: Vec::new(),
        review_hints: Vec::new(),
        shown_hints: Vec::new(),
        review_hint: false,
        review_enabled: false,
        review_roots: ReviewRoots {
            state_home: None,
            home: None,
            canonical_root: PathBuf::new(),
        },
        editor_request_hook: None,
        editor_rpc: None,
        review_flow: eitri_core::turn_review::ReviewFlow::new(),
        review_editor: crate::review_editor::ReviewEditor::default(),
        editor_done_hook: None,
        retiring: Retiring::default(),
        resume_after: std::collections::HashMap::new(),
        history_dir: None,
        rules_dir: None,
        history: Vec::new(),
        editor_link: None,
        tab_verb_hook: None,
        nav_fallthrough_hook: None,
        keymap_closed_hook: None,
        tab_memory: TabWriter::new(None, PathBuf::new()),
        titles,
        hello_titles_stale: false,
        chooser_listed: Vec::new(),
        chooser_titles_stale: false,
        conversation_id: String::new(),
        restore_source: None,
        restore_policy: RestorePolicy::Offer,
        bypass_by_config: false,
        mode_remembered: false,
        default_agent: AgentId::Claude,
        codex_command: None,
        acp_agents: Vec::new(),
        start_notes: StartNotes::default(),
        restore_run: None,
        toast_hook: None,
        // No worker and no disk: every start goes on untrusted, as for a root with nothing to
        // trust. A test about the question puts a real gate in its place.
        trust: TrustGate::without_worker(PathBuf::new()),
        spawner: Spawner::Real,
    }
}

/// The window's wake, handed to every backend the tabs hold. A pair that cannot be made is logged, and the
/// tick then runs on its steady cadence as it always did.
fn panel_wake(tabs: &mut eitri_core::tab_set::TabSet) -> Option<crate::panel_wake::PanelWake> {
    match crate::panel_wake::PanelWake::new() {
        Ok(wake) => {
            tabs.set_ui_wake(wake.waker());
            Some(wake)
        }
        Err(e) => {
            eprintln!(
                "[agent_panel] the panel's wake could not be made ({e}); it ticks every {:?}",
                AgentPanelHandle::TICK
            );
            None
        }
    }
}

/// Whether something the tick acts on can become ready without raising the wake, so the tick has to run on
/// its steady cadence: a worker reporting through a channel (a connect, a teardown, the supervisor's
/// connect, the trust check, a review or scratch job), a backend that does not announce its folds, or the
/// review flow waiting on the editor.
fn needs_polling(state: &AgentPanelState) -> bool {
    state.wake.is_none()
        || state.tabs.needs_polling()
        || !state.retiring.is_empty()
        || state.supervisor_pending.is_some()
        || !state.pending_edits.is_empty()
        || !state.review_jobs.is_empty()
        || !state.review_flow.is_idle()
        || state.trust.is_busy()
}

/// The window's trust gate over `root` (canonical), its record kept under the state home by the rule
/// every other store follows. Touches no disk: the gate's worker does all of that.
fn trust_gate_for(state_home: Option<&std::ffi::OsStr>, home: Option<&std::ffi::OsStr>, root: &Path) -> TrustGate {
    let store = eitri_core::project_trust::TrustStore::new(state_home, home);
    let home = home.map(PathBuf::from).filter(|home| home.is_absolute());
    TrustGate::new(store, root.to_path_buf(), home)
}

/// Dispatches each payload, in order.
fn dispatch_all(page: &PanelPage, payloads: Vec<String>) {
    for payload in payloads {
        page.send(&payload);
    }
}

/// The `tabs` envelope for `view`'s page (`TabSet::tabs_payload_for`: the base page lists the base
/// view's tabs, an extra page its one tab), recorded as the last one sent to that page.
fn tabs_payload_recorded(state: &mut AgentPanelState, view: ViewId) -> String {
    let payload = state.tabs.tabs_payload_for(view);
    if let Some(record) = state.pages.get_mut(&view) {
        record.last_tabs_payload = Some(payload.clone());
    }
    payload
}

/// Sends `tabs` now: the set changed in a way the pages must see at once. The base page is always
/// sent its strip; an extra page only when its own changed and it is ready.
fn send_tabs(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages) {
    let payload = tabs_payload_recorded(&mut state.borrow_mut(), ViewId::BASE);
    pages.send(ViewId::BASE, &payload);
    send_extra_tabs_if_changed(state, pages);
    send_mode_note(state, pages);
}

/// Each live extra page's `tabs`, when its own changed since the last one it was sent.
fn send_extra_tabs_if_changed(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages) {
    for (view, _) in pages.live_views() {
        if view.is_base() {
            continue;
        }
        let payload = {
            let mut state_ref = state.borrow_mut();
            let now = state_ref.tabs.tabs_payload_for(view);
            let ready = state_ref.ready(view);
            state_ref
                .pages
                .get_mut(&view)
                .and_then(|record| changed_envelope(&mut record.last_tabs_payload, now, ready))
        };
        if let Some(payload) = payload {
            pages.send(view, &payload);
        }
    }
}

/// What the tab set noted about a tab that could not take the window's mode (an agent with no
/// manual mode started in auto), said once in the base page's transient slot, after the `tabs` that
/// shows the tab. Kept until the base page is ready to show it.
fn send_mode_note(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages) {
    let note = {
        let mut state_ref = state.borrow_mut();
        if !state_ref.ready(ViewId::BASE) {
            return;
        }
        state_ref.tabs.take_mode_note()
    };
    if let Some(note) = note {
        eprintln!("[permission] {note}");
        pages.send(ViewId::BASE, &eitri_core::agent_bridge::serialize_notice_for_js(&note));
    }
}

/// Hands nvim `request` through `main.rs`'s hook, which shows and focuses the editor first.
fn request_editor(
    state: &Rc<RefCell<AgentPanelState>>,
    request: &eitri_core::scratch::ScratchRequest,
) -> Result<(), String> {
    let hook = state.borrow().editor_request_hook.clone();
    match hook {
        Some(hook) => hook(request),
        None => Err("the editor is not connected to this panel".to_string()),
    }
}

/// The most reviews worked out at once. Each one runs git over the shadow repository, and a panel
/// that asks again before an answer arrives is told to wait rather than queueing without bound.
const MAX_REVIEW_JOBS: usize = 4;

/// One review being worked out on a worker thread.
struct PendingReview {
    request_id: String,
    /// The page that asked: the answer goes there.
    view: ViewId,
    /// The finished envelope, or why there is none.
    rx: mpsc::Receiver<Result<String, String>>,
    /// The tab and turn whose status-band hint the page drops when this review's envelope arrives:
    /// Rust forgets the remembered hint only then, since a refused or failed review leaves the hint
    /// on the page.
    clears_hint: Option<(u64, eitri_core::agent_bridge::ReviewTurnWire)>,
}

/// Runs `work` on a worker thread; the tick sends what it returns (see [`poll_review_jobs`]).
/// Refused while [`MAX_REVIEW_JOBS`] are already out.
fn start_review_job(
    state: &Rc<RefCell<AgentPanelState>>,
    view: ViewId,
    request_id: &str,
    clears_hint: Option<(u64, eitri_core::agent_bridge::ReviewTurnWire)>,
    work: impl FnOnce() -> Result<String, String> + Send + 'static,
) -> Result<(), String> {
    if state.borrow().review_jobs.len() >= MAX_REVIEW_JOBS {
        return Err("reviews are still being worked out; try again in a moment".to_string());
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // A window closed in the meantime has dropped the receiver; the answer has nowhere to go.
        let _ = tx.send(work());
    });
    state.borrow_mut().review_jobs.push(PendingReview {
        request_id: request_id.to_string(),
        view,
        rx,
        clears_hint,
    });
    Ok(())
}

/// What the finished jobs of `jobs` owe the panel, in order: an envelope for a review that was
/// worked out, a failing `command_result` for one that could not be (or whose worker died). The
/// finished ones leave `jobs`. The second list is the hints those envelopes clear on the page.
#[cfg(test)]
fn finished_reviews(
    jobs: &mut Vec<PendingReview>,
) -> (Vec<String>, Vec<(u64, eitri_core::agent_bridge::ReviewTurnWire)>) {
    let (payloads, cleared) = finished_reviews_by_page(jobs);
    (payloads.into_iter().map(|(_, payload)| payload).collect(), cleared)
}

/// What [`finished_reviews_by_page`] found: each payload with the page it goes to, and the hints the
/// reviews among them clear.
type ReviewsDone = (
    Vec<(ViewId, String)>,
    Vec<(u64, eitri_core::agent_bridge::ReviewTurnWire)>,
);

/// [`finished_reviews`], each payload with the view of the page that asked for it.
fn finished_reviews_by_page(jobs: &mut Vec<PendingReview>) -> ReviewsDone {
    let mut payloads = Vec::new();
    let mut cleared = Vec::new();
    jobs.retain(|job| match job.rx.try_recv() {
        Ok(Ok(envelope)) => {
            payloads.push((job.view, envelope));
            cleared.extend(job.clears_hint);
            false
        }
        Ok(Err(why)) => {
            payloads.push((job.view, serialize_command_result_for_js(&job.request_id, Err(&why))));
            false
        }
        Err(mpsc::TryRecvError::Disconnected) => {
            payloads.push((
                job.view,
                serialize_command_result_for_js(&job.request_id, Err("the review stopped before it finished")),
            ));
            false
        }
        Err(mpsc::TryRecvError::Empty) => true,
    });
    (payloads, cleared)
}

/// The hints a finished turn may show: all of them with `review.hint` on, none otherwise, so a
/// quiet window never queues or dispatches one.
fn hints_to_keep(
    hint_on: bool,
    hints: Vec<eitri_core::turn_review::ReviewHint>,
) -> Vec<eitri_core::turn_review::ReviewHint> {
    if hint_on {
        hints
    } else {
        Vec::new()
    }
}

/// Adds `new` to the hints still waiting for the page, in order; a tab's older hint is dropped
/// because the newer one (even `files: 0`, which clears) supersedes it.
fn keep_newest_hint_per_tab(
    waiting: &mut Vec<eitri_core::turn_review::ReviewHint>,
    new: Vec<eitri_core::turn_review::ReviewHint>,
) {
    for hint in new {
        waiting.retain(|old| old.tab != hint.tab);
        waiting.push(hint);
    }
}

/// Records what the page was just sent, so a later document can be told again: a hint with files
/// replaces its tab's, one with none clears it.
fn keep_shown_hints(
    shown: &mut Vec<eitri_core::turn_review::ReviewHint>,
    sent: &[eitri_core::turn_review::ReviewHint],
) {
    for hint in sent {
        shown.retain(|old| old.tab != hint.tab);
        if hint.files > 0 {
            shown.push(hint.clone());
        }
    }
}

/// The page clears `tab`'s hint on receiving that tab's review of a turn at least as new as the
/// hint's, so a reload must not bring it back.
fn forget_hint_for_review(
    shown: &mut Vec<eitri_core::turn_review::ReviewHint>,
    tab: u64,
    asked: eitri_core::agent_bridge::ReviewTurnWire,
) {
    use eitri_core::agent_bridge::ReviewTurnWire;
    shown.retain(|hint| {
        hint.tab != tab
            || match asked {
                ReviewTurnWire::Latest => false,
                ReviewTurnWire::N(turn) => turn < hint.turn,
            }
    });
}

/// Forgets the shown hints of a tab that closed or is running a turn: the page drops a tab's hint
/// when a turn starts, and a turn that ends without a hint of its own would otherwise leave this
/// record holding one the page no longer has.
fn forget_hints_of_running_or_closed_tabs(state: &mut AgentPanelState) {
    let tabs = &state.tabs;
    state.shown_hints.retain(|hint| {
        tabs.get(eitri_core::tabs::TabId(hint.tab))
            .is_some_and(|tab| !tab.turn_running())
    });
}

/// The status-band hint `tab` showed in the page it left, to the ready page of `view` it moved to:
/// that page was never sent it, and the tick does not send a shown hint again. A page not ready yet is
/// sent it with the rest on its `ready` ([`hints_for_new_document`]).
fn send_moved_hint(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages, tab: TabId, view: ViewId) {
    let hint = {
        let mut state_ref = state.borrow_mut();
        if !state_ref.ready(view) {
            return;
        }
        forget_hints_of_running_or_closed_tabs(&mut state_ref);
        state_ref
            .shown_hints
            .iter()
            .find(|hint| hint.tab == tab.0)
            .map(eitri_core::agent_bridge::serialize_review_hint_for_js)
    };
    if let Some(hint) = hint {
        pages.send(view, &hint);
    }
}

/// What a new document in `view`'s page is owed of the hints the last one showed: only those of the
/// tabs in that view.
fn hints_for_new_document(state: &mut AgentPanelState, view: ViewId) -> Vec<eitri_core::turn_review::ReviewHint> {
    forget_hints_of_running_or_closed_tabs(state);
    state
        .shown_hints
        .iter()
        .filter(|hint| state.tabs.view_of(TabId(hint.tab)) == view)
        .cloned()
        .collect()
}

/// The waiting hints whose tab's view has a ready page, each with that view; the others keep waiting.
fn take_hints_for_ready_pages(state: &mut AgentPanelState) -> Vec<(ViewId, eitri_core::turn_review::ReviewHint)> {
    let waiting = std::mem::take(&mut state.review_hints);
    let mut ready = Vec::new();
    for hint in waiting {
        let view = state.tabs.view_of(TabId(hint.tab));
        if state.ready(view) {
            ready.push((view, hint));
        } else {
            state.review_hints.push(hint);
        }
    }
    ready
}

/// The tick's half of a review: whatever a worker finished goes to the page that asked.
fn poll_review_jobs(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages) {
    let payloads = {
        let mut state_ref = state.borrow_mut();
        if state_ref.review_jobs.is_empty() {
            return;
        }
        let (payloads, cleared) = finished_reviews_by_page(&mut state_ref.review_jobs);
        for (tab, turn) in cleared {
            forget_hint_for_review(&mut state_ref.shown_hints, tab, turn);
        }
        payloads
    };
    for (view, payload) in payloads {
        pages.send(view, &payload);
    }
}

/// What the review flow owes the panel: an answer goes to the page that sent its request, a tab's
/// draft to the page of the tab's view, the window's interrupted reverts to the base page, and a
/// confirmed review that was sent is applied as any flush of a tab's queue is. Called with no borrow
/// of the state held, because applying a flush takes it. The requests the flow no longer owes an
/// answer are forgotten afterwards.
fn dispatch_review_outs(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages, outs: Vec<eitri_core::turn_review::Out>) {
    use eitri_core::turn_review::Out;
    for out in outs {
        match out {
            Out::Envelope(payload) => pages.send(ViewId::BASE, &payload),
            Out::Answer { request_id, payload } => {
                let asked_from = state
                    .borrow()
                    .review_requests
                    .iter()
                    .find(|(id, _)| *id == request_id)
                    .map(|(_, view)| *view);
                match asked_from {
                    Some(view) => pages.send(view, &payload),
                    None => eprintln!("[agent_panel] review answer for {request_id}: no page asked for it"),
                }
            }
            Out::Draft(tab, payload) => send_to_tabs_page(state, pages, tab, &payload),
            Out::Flushed(tab, flush) => apply_flush(state, pages, tab, flush),
            Out::QueueChanged(tab) => send_queue(state, pages, tab),
            Out::EditorOpened => {
                let hook = state.borrow().review_editor.opened_hook();
                if let Some(hook) = hook {
                    hook();
                }
            }
        }
    }
    let mut state_ref = state.borrow_mut();
    let state_ref = &mut *state_ref;
    let flow = &state_ref.review_flow;
    state_ref.review_requests.retain(|(id, _)| flow.owes_answer(id));
}

/// What a page is told when its review request has the id of another page's that is still out: the
/// answers could not be told apart. Its next request has another id.
const REVIEW_ID_IN_USE: &str = "another view's review request is still being worked out; try again in a moment";

/// One review message for the flow from `view`'s page, with the window's editor handle if it has one.
/// What the flow answers, now or on a later tick, goes to that page.
fn run_review_flow(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages, view: ViewId, message: InboundMessage) {
    let request_id = message.request_id().to_string();
    let in_use = state
        .borrow()
        .review_requests
        .iter()
        .any(|(id, asked_from)| *id == request_id && *asked_from != view);
    if in_use {
        pages.send(
            view,
            &serialize_command_result_for_js(&request_id, Err(REVIEW_ID_IN_USE)),
        );
        return;
    }
    let outs = {
        let mut state_ref = state.borrow_mut();
        let state_ref = &mut *state_ref;
        if !state_ref.review_requests.iter().any(|(id, _)| *id == request_id) {
            state_ref.review_requests.push((request_id, view));
        }
        state_ref.review_editor.sync(&mut state_ref.review_flow);
        state_ref.review_flow.handle(
            message,
            &mut state_ref.tabs,
            state_ref.editor_rpc.as_deref(),
            std::time::Instant::now(),
        )
    };
    dispatch_review_outs(state, pages, outs);
}

/// The tick's half of the review flow: editor answers, workers that finished and tickets whose tab
/// went away, each answer for the page that sent its request.
fn poll_review_flow(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages) {
    let outs = {
        let mut state_ref = state.borrow_mut();
        let state_ref = &mut *state_ref;
        if state_ref.review_flow.is_idle() {
            return;
        }
        state_ref.review_editor.sync(&mut state_ref.review_flow);
        state_ref.review_flow.tick(
            &mut state_ref.tabs,
            state_ref.editor_rpc.as_deref(),
            std::time::Instant::now(),
        )
    };
    dispatch_review_outs(state, pages, outs);
}

/// The return half of a scratch edit: every edit whose marker appeared goes back to the tab it came from,
/// told to the page that shows it, and the keys return to the chat view of the last such tab.
fn poll_scratch_edits(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages) {
    let finished: Vec<(eitri_core::scratch::PendingEdit, eitri_core::scratch::EditDone)> = {
        let mut state_ref = state.borrow_mut();
        if state_ref.pending_edits.is_empty() {
            return;
        }
        let mut done = Vec::new();
        state_ref.pending_edits.retain(|edit| match edit.poll() {
            Some(result) => {
                done.push((edit.clone(), result));
                false
            }
            None => true,
        });
        done
    };
    if finished.is_empty() {
        return;
    }
    let mut keys_to = ViewId::BASE;
    for (edit, result) in finished {
        let returned = state.borrow_mut().tabs.finish_scratch_edit(edit.id, &result);
        edit.cleanup();
        let Some((tab, draft)) = returned else { continue };
        let (shown, view) = {
            let state_ref = state.borrow();
            (state_ref.shown_in(tab), state_ref.tabs.view_of(tab))
        };
        keys_to = view;
        if let Some(shown) = shown {
            if let Some(text) = &draft {
                pages.send(shown, &eitri_core::agent_bridge::serialize_draft_for_js(tab, text));
            }
            pages.send(shown, &eitri_core::agent_bridge::serialize_scratch_for_js(tab, false));
        }
        if let eitri_core::scratch::EditDone::Failed(why) = &result {
            pages.send(
                view,
                &eitri_core::agent_bridge::serialize_notice_for_js(&format!("the nvim scratch buffer failed: {why}")),
            );
        }
    }
    let hook = state.borrow().editor_done_hook.clone();
    if let Some(hook) = hook {
        hook(keys_to);
    }
}

/// Titles the worker read: `hello` is owed again after a change, and a tab resumed with its record's title
/// takes the CLI's once it is known (the `tabs` envelope that follows carries it).
fn apply_title_reads(state: &Rc<RefCell<AgentPanelState>>) {
    let mut state_ref = state.borrow_mut();
    let state_ref = &mut *state_ref;
    if state_ref.titles.take_changed() {
        state_ref.hello_titles_stale = true;
        state_ref.chooser_titles_stale = true;
    }
    // Every tick, not only after a read: a read can land before the tab it titles is installed or knows
    // its session, and a tab with no record's title left is skipped at no cost.
    let cwd = state_ref.canonical_project_dir.clone();
    let titles = &state_ref.titles;
    state_ref.tabs.retitle_from(|session| titles.known_title(&cwd, session));
}

/// The titles of the records the last chooser listed that the cache knows better since it was sent, to the
/// page it was opened in. The page applies them only to a chooser still open, so sending them after it
/// closed reopens nothing.
fn send_chooser_titles_if_changed(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages) {
    let (view, payload) = {
        let mut state_ref = state.borrow_mut();
        let state_ref = &mut *state_ref;
        if !std::mem::take(&mut state_ref.chooser_titles_stale) {
            return;
        }
        let cwd = state_ref.canonical_project_dir.as_str();
        let titles = &state_ref.titles;
        let mut changed = Vec::new();
        for (id, shown) in &mut state_ref.chooser_listed {
            if let Some(title) = titles.known_title(cwd, id) {
                if shown.as_deref() != Some(title.as_str()) {
                    *shown = Some(title.clone());
                    changed.push((id.clone(), title));
                }
            }
        }
        if changed.is_empty() {
            return;
        }
        (
            state_ref.chooser_view,
            eitri_core::agent_bridge::serialize_chooser_titles_for_js(&changed),
        )
    };
    pages.send(view, &payload);
}

/// The tick's `tabs` envelopes, one per live page: each sent only when it differs from the last one
/// that page was sent (a marker, a label, a state -- whatever `TabSet::tabs_payload_for` says).
///
/// Held back while a page's document has not said `ready`, and then not recorded as sent, so it
/// goes out once it has.
fn send_tabs_if_changed(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages) {
    let mut base_sent = false;
    for (view, page) in pages.live_views() {
        let payload = {
            let mut state_ref = state.borrow_mut();
            let now = state_ref.tabs.tabs_payload_for(view);
            let ready = state_ref.ready(view);
            state_ref
                .pages
                .get_mut(&view)
                .and_then(|record| changed_envelope(&mut record.last_tabs_payload, now, ready))
        };
        if let Some(payload) = payload {
            page.send(&payload);
            base_sent |= view.is_base();
        }
    }
    if base_sent {
        send_mode_note(state, pages);
    }
}

/// V1: the editor context's summary, to each live page whose last one differs.
fn send_context_if_changed(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages) {
    let views = pages.live_views();
    if views.is_empty() {
        return;
    }
    let now = {
        let state_ref = state.borrow();
        let summary =
            eitri_core::agent_bridge::context_summary((state_ref.editor_context)().as_ref(), &state_ref.project_dir);
        eitri_core::agent_bridge::serialize_editor_context_for_js(summary.as_ref())
    };
    for (view, page) in views {
        let payload = {
            let mut state_ref = state.borrow_mut();
            let ready = state_ref.ready(view);
            state_ref
                .pages
                .get_mut(&view)
                .and_then(|record| changed_envelope(&mut record.last_context_payload, now.clone(), ready))
        };
        if let Some(payload) = payload {
            page.send(&payload);
        }
    }
}

/// Appends `texts` to the project's history and tells every ready page (the history is the
/// window's). Best-effort: a failed write is logged and the pages keep the list they had.
fn remember_prompts(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages, texts: &[String]) {
    let (dir, root) = {
        let s = state.borrow();
        (s.history_dir.clone(), s.project_dir.clone())
    };
    let Some(dir) = dir else { return };
    match eitri_core::prompt_history::append(&dir, &root, texts) {
        Ok(entries) => {
            let payload = {
                let mut s = state.borrow_mut();
                s.history = entries;
                eitri_core::agent_bridge::serialize_history_for_js(&s.history)
            };
            for (view, page) in pages.live_views() {
                if state.borrow().ready(view) {
                    page.send(&payload);
                }
            }
        }
        Err(e) => eprintln!("[history] could not append: {e}"),
    }
}

/// The queue of a tab on screen, after anything changed it, to the ready page that shows it.
fn send_queue(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages, tab: TabId) {
    let (view, payload) = {
        let s = state.borrow();
        let Some(view) = s.page_of(tab) else { return };
        let t = s.tabs.get(tab).expect("a shown tab exists");
        (
            view,
            eitri_core::agent_bridge::serialize_queue_for_js(
                tab,
                &t.queue,
                t.queue_error.as_deref(),
                t.queue_note.as_deref(),
                t.steers(),
            ),
        )
    };
    pages.send(view, &payload);
}

/// A flush's outcome: the same benign/fatal split a command gets, with no request to
/// answer. A refusal leaves the queue and its `error` line; the queue envelope says so.
fn apply_flush(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages, tab: TabId, flush: eitri_core::tab_set::Flush) {
    if flush.outcome.is_ok() {
        title_from_first_prompt(state, tab, &flush.typed);
    }
    apply_outcome(state, pages, tab, None, flush.outcome);
    send_queue(state, pages, tab);
}

/// Whether a move or a reload in `view`'s page ends the `:trust` prompt: it was typed in that page,
/// or its tab has left the view of the page it was typed in, which can no longer answer it (or no
/// command is recorded, which leaves the gate's own record to decide).
fn command_prompt_is_in(state: &AgentPanelState, view: ViewId) -> bool {
    let Some(command) = state.trust.command_prompt().and_then(|id| state.trust_commands.get(id)) else {
        return true;
    };
    command.typed_in == view
        || state
            .tabs
            .get(command.tab)
            .is_none_or(|t| t.view_id() != command.typed_in)
}

/// The base page's `tabs`, then the active tab's own state (always, not only when stale), and the
/// trust question again when the tab on screen waits for one: routing away took the old one off. A
/// `:trust` question typed in the base page goes with any move there, answered as not trusted, and so
/// does one whose tab has left the view it was typed in.
fn switch_payloads(state: &mut AgentPanelState) -> Vec<String> {
    if command_prompt_is_in(state, ViewId::BASE) {
        state.trust.drop_command_prompt();
    }
    let mut payloads = vec![tabs_payload_recorded(state, ViewId::BASE)];
    payloads.extend(state.tabs.active_state_payloads());
    let active = state.tabs.active();
    if state.trust.current_route(&waiting_profile(state, active)) == Some(Route::Ask) {
        payloads.extend(reroute_or_ask(state, active).payloads_for(state, ViewId::BASE));
    }
    payloads
}

/// What `view`'s page is owed when the tab it shows arrived or changed state: for the base view,
/// [`switch_payloads`]; for an extra view, its `tabs`, its tab's own state and its tab's trust question
/// asked again. An extra view with no tab gets only its (empty) `tabs`.
fn view_payloads(state: &mut AgentPanelState, view: ViewId) -> Vec<String> {
    if view.is_base() {
        return switch_payloads(state);
    }
    let mut payloads = vec![tabs_payload_recorded(state, view)];
    let Some(tab) = state.tabs.tab_in(view) else {
        return payloads;
    };
    payloads.extend(state.tabs.state_payloads(tab));
    if state.trust.current_route(&waiting_profile(state, tab)) == Some(Route::Ask) {
        payloads.extend(reroute_or_ask(state, tab).payloads_for(state, view));
    }
    payloads
}

/// After the base view's active tab changed: `mark_seen` if the base page is on screen, then its
/// `tabs` and the new active tab's state.
fn send_switch(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages) {
    send_view(state, pages, ViewId::BASE);
}

/// [`send_switch`] for any view's page: `mark_seen` of the tab it shows if the page is on screen, then
/// [`view_payloads`]. An extra page is sent nothing until its document is ready: its `ready` sends
/// all of it.
fn send_view(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages, view: ViewId) {
    let page = pages.live(view);
    if !view.is_base() && !state.borrow().ready(view) {
        return;
    }
    let before = state.borrow().tabs.attention();
    let shown = page.as_ref().is_some_and(|page| on_screen(state, view, page));
    let payloads = {
        let mut state_ref = state.borrow_mut();
        if shown {
            if let Some(tab) = state_ref.tab_of_page(view) {
                state_ref.tabs.mark_seen(tab);
            }
        }
        view_payloads(&mut state_ref, view)
    };
    if let Some(page) = page {
        dispatch_all(&page, payloads);
    }
    report_attention(state, before);
}

/// The payloads a page shows for tab `tab` once it changed state: the whole view's when the tab is on
/// screen in some page ([`view_payloads`]), else only the `tabs` of its view, to that view's page.
fn shown_payloads(state: &mut AgentPanelState, tab: TabId) -> (ViewId, Vec<String>) {
    let view = state.tabs.view_of(tab);
    if state.tabs.is_shown(tab) && (view.is_base() || state.ready(view)) {
        (view, view_payloads(state, view))
    } else {
        (view, vec![tabs_payload_recorded(state, view)])
    }
}

/// `hello` is per page and re-sent, recomputed, whenever what it describes changes -- a session
/// adopted, a resume installed, a tab closed or reset, the page's tab now running another agent.
fn send_hello_if_open_sessions_changed(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages) {
    // A title the worker found since: the rows it names are out of date, in every page.
    let titles_changed = std::mem::take(&mut state.borrow_mut().hello_titles_stale);
    for (view, page) in pages.live_views() {
        let payload = {
            let mut state_ref = state.borrow_mut();
            let state_ref = &mut *state_ref;
            // `ready` sends `hello` itself, and records what it was computed against.
            if !state_ref.ready(view) {
                continue;
            }
            let open = state_ref.tabs.open_session_ids();
            // The saved tabs are on offer only in the base page, and only while nothing here has
            // started: when that stops being true the dashboard's line has to go, though no session id
            // changed with it.
            let offered = view.is_base() && restore_is_on_offer(state_ref);
            // The greeting lists the page's tab's agent's sessions, so a switch to a tab running another
            // agent changes it too, though no session id changed with it.
            let agent = hello_agent(state_ref, view);
            let record = state_ref.pages.get(&view).expect("a ready page has its record");
            if open == record.last_open_ids
                && offered == record.last_restore_offered
                && agent == record.last_hello_agent
                && !titles_changed
            {
                continue;
            }
            let mut greeting = BackendGreeting::for_agent_titled(
                state_ref.backend_kind,
                &agent,
                state_ref.project_dir.clone(),
                eitri_core::agent_backend::Titles::Cached(&state_ref.titles),
            );
            greeting.resumable.retain(|r| !open.contains(&r.provider_session_id));
            let offer = if view.is_base() { restore_offer(state_ref) } else { None };
            let record = state_ref.pages.get_mut(&view).expect("a ready page has its record");
            record.last_open_ids = open;
            record.last_restore_offered = offered;
            record.last_hello_agent = agent;
            serialize_hello_with_restore_for_js(&greeting, offer.as_ref())
        };
        page.send(&payload);
    }
}

/// The agent whose records `view`'s page offers: the agent of the tab it shows, or the window's
/// default for an extra page that shows none yet.
fn hello_agent(state: &AgentPanelState, view: ViewId) -> AgentId {
    state
        .tab_of_page(view)
        .and_then(|tab| state.tabs.get(tab))
        .map_or_else(|| state.default_agent.clone(), |t| t.agent().clone())
}

/// Whether the launch dashboard may offer to bring the last window's tabs back at all: the policy is
/// to offer, there is something saved, no tab here has started a session and no restore is running.
fn restore_is_on_offer(state: &AgentPanelState) -> bool {
    state.restore_policy == RestorePolicy::Offer
        && state.restore_source.is_some()
        && state.restore_run.is_none()
        && state.tabs.is_pristine()
}

/// What the dashboard says it can bring back: the saved tabs that still can be, `None` when that is
/// none (a key that restores nothing is never offered).
fn restore_offer(state: &AgentPanelState) -> Option<RestoreOffer> {
    if !restore_is_on_offer(state) {
        return None;
    }
    restore_plan(state).offer()
}

/// Which of the saved tabs can come back, judged against the records and leases as they are now.
fn restore_plan(state: &AgentPanelState) -> eitri_core::tab_restore::RestorePlan {
    let canonical = state.canonical_project_dir.as_str();
    let saved = state.restore_source.clone().unwrap_or_default();
    // Read here rather than taken from a greeting: the greeting lists the active tab's agent's
    // records, and the saved Claude tabs are matched with Claude's whichever agent that tab runs. The
    // other agents' records are read beside them, each saved tab being matched only with a record its
    // own agent wrote.
    let mut records = BackendGreeting::claude_sessions_titled(
        state.backend_kind,
        &state.project_dir,
        eitri_core::agent_backend::Titles::Cached(&state.titles),
    );
    if !saved.external_tabs.is_empty() {
        records.extend(eitri_core::agent_backend::external_sessions(&state.project_dir));
    }
    let mut plan = eitri_core::tab_restore::plan(&saved, &records, &state.tabs.open_session_ids(), |agent, id| {
        agent::lease::SessionLease::is_held(&agent.record_name(), canonical, id).unwrap_or(false)
    });
    // A saved tab of an ACP agent `init.lua` no longer registers has no command to run: it is skipped
    // and said to be, and the next save of the open tabs leaves it out of the file.
    plan.skip_unregistered(|agent| match agent {
        AgentId::Acp(name) => state.acp_agents.iter().any(|a| a.name == *name),
        AgentId::Claude | AgentId::Codex => true,
    });
    plan
}

/// Where a restore was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreAsk {
    /// `s` on the launch dashboard: a saved bypass tab is asked about first.
    Dashboard,
    /// `agent.restore = "auto"`: nobody pressed a key, so nothing can be asked.
    Launch,
}

/// What asking for, or answering, a restore did.
enum RestoreResult {
    /// Why nothing was started.
    Refused(String),
    /// A saved tab was in bypass; the page shows the question and answers it.
    Question(RestorePrompt),
    /// Every saved tab that could be has been started or skipped. `notice` is what the band says.
    Started { notice: Option<String> },
    /// Cleared to start, and waiting for the trust question: `start_deferred` starts it.
    Deferred,
}

/// Begins one resume: given a session and the lease already taken for it, the receiver its result
/// will arrive on.
type SpawnResume<'a> = &'a mut dyn FnMut(
    &str,
    agent::lease::SessionLease,
    ProjectTrust,
) -> mpsc::Receiver<Result<AgentBackend, BackendError>>;

/// A restore's resumes, as the real workers start them. Only [`start_deferred`] calls it.
fn resume_spawner(
    state: &AgentPanelState,
) -> impl FnMut(&str, agent::lease::SessionLease, ProjectTrust) -> mpsc::Receiver<Result<AgentBackend, BackendError>> {
    let (kind, project_dir) = (state.backend_kind, state.project_dir.clone());
    move |id, lease, project| {
        spawn_connect_holding(kind, project_dir.clone(), Some(id.to_string()), Some(lease), project)
    }
}

/// Asks for the last window's tabs back, into `from` (the empty tab the key was pressed in) and new
/// tabs. Nothing is started unless this window has started nothing yet; what is cleared waits for the
/// trust question before anything is leased or started.
fn ask_for_restore(state: &mut AgentPanelState, from: TabId, ask: RestoreAsk) -> RestoreResult {
    // Another agent's saved tab does not need the sidecar; a window with only Claude tabs saved does.
    let saved_external = state
        .restore_source
        .as_ref()
        .is_some_and(|s| !s.external_tabs.is_empty());
    if state.backend_kind != BackendKind::Sidecar && !saved_external {
        return RestoreResult::Refused("only the sidecar backend can resume sessions".to_string());
    }
    if state.restore_run.is_some() {
        return RestoreResult::Refused("the last session's tabs are already being restored".to_string());
    }
    if restore_waits_for_trust(state) {
        return RestoreResult::Refused("the last session's tabs are waiting for the trust question".to_string());
    }
    if !state.tabs.is_pristine() {
        return RestoreResult::Refused(
            "the last session's tabs can be restored only before a session has started".to_string(),
        );
    }
    if state.restore_source.is_none() {
        return RestoreResult::Refused("there are no saved tabs to restore".to_string());
    }
    let plan = restore_plan(state);
    // A launch has one go at it, whatever comes of it: a reload of the page must not try again.
    if ask == RestoreAsk::Launch {
        state.restore_source = None;
    }
    let policy = if state.bypass_by_config {
        BypassPolicy::Keep
    } else if ask == RestoreAsk::Launch {
        BypassPolicy::Downgrade
    } else {
        BypassPolicy::Ask
    };
    match state.tabs.begin_restore(plan, policy) {
        RestoreStep::Confirm(prompt) => RestoreResult::Question(prompt),
        RestoreStep::Go(go) => gate_restore(state, from, go),
    }
}

/// Whether a cleared restore is holding a tab while the trust question is asked.
fn restore_waits_for_trust(state: &AgentPanelState) -> bool {
    state.tabs.tabs().iter().any(|t| {
        matches!(
            t.backend,
            TabBackend::AwaitingTrust(AwaitingTrust {
                start: DeferredStart::Restore { .. },
                ..
            })
        )
    })
}

/// `y` or `n` to the question [`ask_for_restore`] raised.
fn answer_restore_question(state: &mut AgentPanelState, nonce: u64, keep_bypass: bool) -> RestoreResult {
    match state.tabs.answer_restore(nonce, keep_bypass) {
        Err(why) => RestoreResult::Refused(why),
        Ok(go) => {
            let from = state.tabs.active();
            gate_restore(state, from, go)
        }
    }
}

/// A restore the bypass question (if any) has cleared waits in `from` for the trust question. It holds
/// no lease while it waits: those are taken when it starts, so a question left unanswered blocks no
/// other window.
fn gate_restore(state: &mut AgentPanelState, from: TabId, mut go: RestoreGo) -> RestoreResult {
    // Still held by the window (a dashboard restore): put off while the window saves nothing else, it stays on offer.
    go.reoffer = state.restore_source.is_some();
    match gate_start(state, from, DeferredStart::Restore { go }) {
        Ok(()) => RestoreResult::Deferred,
        Err(why) => RestoreResult::Refused(why),
    }
}

/// Takes every lease first and only then starts the resumes, so that nothing -- a record pruned to
/// make room for another session, another window -- can claim a saved session between the decision
/// and its own resume. A session whose lease cannot be taken is skipped with the reason. Every resumed
/// tab is given `project`: they all share the window's root, so one answer covers them.
fn start_restore(
    state: &mut AgentPanelState,
    from: TabId,
    go: RestoreGo,
    spawn: SpawnResume<'_>,
    project: ProjectTrust,
) -> RestoreResult {
    let canonical = state.canonical_project_dir.clone();
    // Only Claude tabs are started here, on this answer. Another agent's tab comes back holding its
    // resume and waits for its own agent's trust answer, which starts it as a resume by hand; its
    // lease is taken when it starts, so none is taken here.
    let mut leases: std::collections::HashMap<String, Result<agent::lease::SessionLease, String>> = go
        .sessions()
        .into_iter()
        .filter(|(agent, _)| *agent == AgentId::Claude)
        .map(|(_, id)| {
            let lease = match agent::lease::SessionLease::try_acquire("claude", &canonical, &id) {
                Ok(lease) => Ok(lease),
                Err(agent::lease::LeaseError::AlreadyHeld) => Err(eitri_core::tab_restore::HELD_ELSEWHERE.to_string()),
                Err(other) => Err(format!("its session lease could not be taken ({other})")),
            };
            (id, lease)
        })
        .collect();
    let (downgraded, asked) = (go.downgraded, go.asked);
    let run = state.tabs.start_restore(from, go, |agent, id| {
        if agent.is_external() {
            return Ok(RestoreConnect::AwaitTrust);
        }
        match leases.remove(id) {
            Some(Ok(lease)) => Ok(RestoreConnect::Started(spawn(id, lease, project))),
            Some(Err(reason)) => Err(reason),
            None => Err("it was listed twice".to_string()),
        }
    });
    // Each waiting tab is checked for its own agent now; the tick asks the question once the tab is
    // on screen, or starts it at once when the window already has that agent's answer.
    for &tab in run.awaiting_trust() {
        let profile = waiting_profile(state, tab);
        state.trust.wait_for(tab, &profile);
        state.trust.begin_check(&profile);
    }
    // The tabs this restore just set connecting are the only starting ones without their tiers:
    // every other start names them as it begins.
    for tab in state.tabs.tabs().iter().map(|t| t.id).collect::<Vec<_>>() {
        if let Some(t) = state.tabs.get_mut(tab) {
            if matches!(t.backend, TabBackend::Starting(_)) && t.project_tiers.is_none() {
                t.project_tiers = Some(project);
            }
        }
    }
    state.restore_source = None;
    state.restore_run = Some(run);
    let notice = (downgraded > 0 && !asked).then(|| {
        let tabs = if downgraded == 1 { "tab" } else { "tabs" };
        format!("{downgraded} bypass {tabs} came back in auto; Shift+Tab then y switches")
    });
    RestoreResult::Started { notice }
}

/// Once every resume of a restore has returned, its one message: to the log and to the window's toast.
fn finish_restore(state: &Rc<RefCell<AgentPanelState>>) {
    let message = {
        let mut state_ref = state.borrow_mut();
        let AgentPanelState { restore_run, tabs, .. } = &mut *state_ref;
        // A tab the user closed while it was still connecting will never report: it is not waited for.
        if let Some(run) = restore_run.as_mut() {
            run.forget_closed(|tab| tabs.get(tab).is_some());
        }
        let outcome = state_ref
            .restore_run
            .as_ref()
            .and_then(eitri_core::tab_restore::RestoreRun::outcome);
        outcome.map(|outcome| {
            state_ref.restore_run = None;
            outcome.message()
        })
    };
    let Some(message) = message else { return };
    eprintln!("[restore] {message}");
    let hook = state.borrow().toast_hook.clone();
    if let Some(hook) = hook {
        hook(&message);
    }
}

/// What a [`RestoreResult`] owes the base page, the only one that offers or asks about a restore;
/// `Err` is a refusal for the caller to report.
fn deliver_restore(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages, result: RestoreResult) -> Result<(), String> {
    match result {
        RestoreResult::Refused(why) => Err(why),
        RestoreResult::Question(prompt) => {
            pages.send(ViewId::BASE, &serialize_confirm_restore_for_js(&prompt));
            Ok(())
        }
        RestoreResult::Started { notice } => {
            send_switch(state, pages);
            // A restored tab that came back into its extra view is shown there.
            send_extra_views(state, pages);
            if let Some(notice) = notice {
                pages.send(
                    ViewId::BASE,
                    &eitri_core::agent_bridge::serialize_notice_for_js(&notice),
                );
            }
            finish_restore(state);
            Ok(())
        }
        // The tab shows as starting while the question is asked; the tick's trust events go on.
        RestoreResult::Deferred => {
            send_tabs(state, pages);
            Ok(())
        }
    }
}

/// [`send_view`] for every live extra page.
fn send_extra_views(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages) {
    for (view, _) in pages.live_views() {
        if !view.is_base() {
            send_view(state, pages, view);
        }
    }
}

/// `agent.restore = "auto"`: on the base page's first `ready`, bring the last window's tabs back
/// with no key pressed. Used up on its first go, so a reload of the page does not do it again.
fn restore_at_launch(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages) {
    let Some(result) = launch_restore(&mut state.borrow_mut()) else {
        return;
    };
    if let Err(why) = deliver_restore(state, pages, result) {
        eprintln!("[restore] not restored: {why}");
    }
}

/// The state half of [`restore_at_launch`]: `None` when the launch restores nothing.
fn launch_restore(state: &mut AgentPanelState) -> Option<RestoreResult> {
    if state.restore_policy != RestorePolicy::Auto || state.restore_source.is_none() {
        return None;
    }
    let from = state.tabs.active();
    Some(ask_for_restore(state, from, RestoreAsk::Launch))
}

/// Which page a [`TrustOut`] payload goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dest {
    /// This view's page: an answer to a request it sent, or what that page shows.
    Page(ViewId),
    /// The page of this tab's view, decided when it is delivered: what is said about the tab.
    Tab(TabId),
}

/// What a trust step owes the pages and the window: payloads to dispatch in order, each with the
/// page it goes to, a toast, and what a restore it started has to say.
#[derive(Default)]
struct TrustOut {
    /// The payloads in order. `dests` holds the page of each, at the same index.
    payloads: Vec<String>,
    dests: Vec<Dest>,
    toast: Option<String>,
    /// A restore was started: its one-line result for the base page's band (`None` when it has
    /// nothing to say), sent after the payloads, and then whether it is already over is checked.
    restored: Option<Option<String>>,
}

impl TrustOut {
    fn extend(&mut self, other: TrustOut) {
        self.payloads.extend(other.payloads);
        self.dests.extend(other.dests);
        if other.toast.is_some() {
            self.toast = other.toast;
        }
        if other.restored.is_some() {
            self.restored = other.restored;
        }
    }

    fn push(&mut self, dest: Dest, payload: String) {
        self.payloads.push(payload);
        self.dests.push(dest);
    }

    fn push_all(&mut self, dest: Dest, payloads: Vec<String>) {
        for payload in payloads {
            self.push(dest, payload);
        }
    }

    /// The payloads bound for `view`'s page, in order; the others, and the toast, are dropped. For a
    /// caller that already sends to that page alone.
    fn payloads_for(self, state: &AgentPanelState, view: ViewId) -> Vec<String> {
        self.payloads
            .into_iter()
            .zip(self.dests)
            .filter(|(_, dest)| dest_view(state, *dest) == view)
            .map(|(payload, _)| payload)
            .collect()
    }
}

/// The view whose page a [`Dest`] names now.
fn dest_view(state: &AgentPanelState, dest: Dest) -> ViewId {
    match dest {
        Dest::Page(view) => view,
        Dest::Tab(tab) => state.tabs.view_of(tab),
    }
}

/// Dispatches a [`TrustOut`] once the state is no longer borrowed, each payload to its page.
fn deliver_trust(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages, out: TrustOut) {
    for (payload, dest) in out.payloads.into_iter().zip(out.dests) {
        let view = dest_view(&state.borrow(), dest);
        pages.send(view, &payload);
    }
    if let Some(toast) = out.toast {
        let hook = state.borrow().toast_hook.clone();
        if let Some(hook) = hook {
            hook(&toast);
        }
    }
    if let Some(notice) = out.restored {
        // A restored tab that came back into its extra view is shown there.
        send_extra_views(state, pages);
        if let Some(notice) = notice {
            pages.send(
                ViewId::BASE,
                &eitri_core::agent_bridge::serialize_notice_for_js(&notice),
            );
        }
        finish_restore(state);
    }
}

/// Seconds since the epoch, for when a project was trusted.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The discovery profile of an agent: whose project files its trust question is about. A registered
/// agent's are the files its registration names. One no registration names (a tab saved while it was
/// registered) finds nothing; it still starts only after a `y`, which its start then refuses for want
/// of a command.
fn trust_profile(registered: &[AgentRegistration], agent: &AgentId) -> Profile {
    match agent {
        AgentId::Claude => Profile::Claude,
        AgentId::Codex => Profile::Codex,
        AgentId::Acp(name) => Profile::Acp {
            name: name.clone(),
            files: registered
                .iter()
                .find(|a| a.name == *name)
                .map(|a| a.project_files.clone())
                .unwrap_or_default(),
        },
    }
}

/// The ACP command a registered agent's tab starts with, or `None` when nothing registered that name.
fn acp_spec(registered: &[AgentRegistration], name: &str) -> Option<agent::providers::acp::AcpAgentSpec> {
    let registration = registered.iter().find(|a| a.name == name)?;
    Some(agent::providers::acp::AcpAgentSpec {
        name: registration.name.clone(),
        argv: registration.command.iter().map(std::ffi::OsString::from).collect(),
        auto_mode: registration.auto_mode.clone(),
    })
}

/// The profile a tab's start waits on. A restore starts on its answer only the tabs of the agent it
/// waits for (Claude's, whenever it brings a Claude tab back), whichever agent the tab it waits in
/// runs; its other agents' tabs come back waiting for their own answers.
fn waiting_profile(state: &AgentPanelState, tab: TabId) -> Profile {
    match state.tabs.get(tab) {
        Some(t) => match &t.backend {
            // A restore waits for the answer its own tabs start on: Claude's when it brings any
            // Claude tab back, else its first tab's agent's.
            TabBackend::AwaitingTrust(AwaitingTrust {
                start: DeferredStart::Restore { go },
                ..
            }) => trust_profile(&state.acp_agents, &go.trust_agent().unwrap_or(AgentId::Claude)),
            _ => trust_profile(&state.acp_agents, t.agent()),
        },
        None => Profile::Claude,
    }
}

/// Puts the resume of `provider_session_id` into the empty tab `tab`, and selects the tab when `select`. It
/// waits for the trust check like a first message (a resumed session loads the project's configuration only
/// as a new one would); `request_id` is answered once the connect finishes or the question is put off.
fn resume_in(
    state: &mut AgentPanelState,
    tab: TabId,
    request_id: &str,
    provider_session_id: String,
    select: bool,
) -> Result<(), String> {
    let agent = state.tabs.get(tab).map_or(AgentId::Claude, |t| t.agent().clone());
    let greeting = BackendGreeting::for_agent_titled(
        state.backend_kind,
        &agent,
        state.project_dir.clone(),
        eitri_core::agent_backend::Titles::Cached(&state.titles),
    );
    // The rename and the title separately: the label puts a rename first, and folding the two into the
    // title lost the rename whenever the record had both.
    let (resumed_title, resumed_name) = greeting
        .resumable
        .iter()
        .find(|r| r.provider_session_id == provider_session_id)
        .map(|r| (r.title.clone(), r.name.clone()))
        .unwrap_or_default();
    let start = DeferredStart::Resume {
        request_id: request_id.to_string(),
        provider_session_id,
        resumed_title,
        resumed_name,
    };
    let deferred = gate_start(state, tab, start);
    if select {
        // A new tab is already selected by `route_resume`; this is for the record.
        state.tabs.select(tab);
    }
    deferred
}

/// Puts a session start off until the gate has checked the project for the tab's agent: the tab holds
/// what was asked for, and nothing is spawned or leased until the check (and, when it must, the user)
/// says how.
fn gate_start(state: &mut AgentPanelState, tab: TabId, start: DeferredStart) -> Result<(), String> {
    state.tabs.defer_start(tab, start)?;
    let profile = waiting_profile(state, tab);
    state.trust.wait_for(tab, &profile);
    state.trust.begin_check(&profile);
    Ok(())
}

/// Starts a session the gate let through, with the tiers it was given. The one place a session's
/// backend is begun: a first message, a resume by hand and a restore all come here, after the trust
/// check, so none of them can load the project's own configuration unasked.
fn start_deferred(state: &mut AgentPanelState, tab: TabId, start: DeferredStart, project: ProjectTrust) -> TrustOut {
    let mut out = TrustOut::default();
    if state.tabs.get(tab).is_none() {
        return out;
    }
    // A question the tab may still have had on screen is spent: the tab is starting.
    state.trust.hide(tab);
    let agent = state.tabs.get(tab).expect("checked above").agent().clone();
    if agent.is_external() && !matches!(start, DeferredStart::Restore { .. }) {
        return start_external_tab(state, tab, agent, start, project);
    }
    let (kind, project_dir) = (state.backend_kind, state.project_dir.clone());
    match start {
        DeferredStart::FirstSend { request_id, first_turn } => {
            let result_rx = match &state.spawner {
                Spawner::Real => spawn_connect(kind, project_dir, None, project),
                #[cfg(test)]
                Spawner::Fake(fake) => (fake.borrow_mut())(SpawnCall::Connect { resume: None, project }),
            };
            let t = state.tabs.get_mut(tab).expect("checked above");
            t.project_tiers = Some(project);
            t.backend = TabBackend::Starting(PendingStart {
                request_id,
                result_rx,
                first_turn: Some(first_turn),
                resume: None,
                resumed_title: None,
                resumed_name: None,
            });
            // The `command_result` is owed once the connect finishes and the first turn is sent
            // (`collect_pending_starts`).
            let view = state.tabs.view_of(tab);
            out.push(Dest::Page(view), tabs_payload_recorded(state, view));
        }
        DeferredStart::Resume {
            request_id,
            provider_session_id,
            resumed_title,
            resumed_name,
        } => {
            let resume = Some(provider_session_id.clone());
            let result_rx = match &state.spawner {
                Spawner::Real => spawn_connect(kind, project_dir, resume, project),
                #[cfg(test)]
                Spawner::Fake(fake) => (fake.borrow_mut())(SpawnCall::Connect { resume, project }),
            };
            let t = state.tabs.get_mut(tab).expect("checked above");
            t.project_tiers = Some(project);
            t.backend = TabBackend::Starting(PendingStart {
                request_id,
                result_rx,
                first_turn: None,
                resume: Some(provider_session_id),
                resumed_title,
                resumed_name,
            });
            // The command_result is owed once the connect finishes.
            let (view, payloads) = shown_payloads(state, tab);
            out.push_all(Dest::Page(view), payloads);
        }
        DeferredStart::Restore { go } => {
            let result = match &state.spawner {
                Spawner::Real => {
                    let mut spawn = resume_spawner(state);
                    start_restore(state, tab, go, &mut spawn, project)
                }
                #[cfg(test)]
                Spawner::Fake(fake) => {
                    let fake = fake.clone();
                    let mut spawn = move |id: &str, lease, project| {
                        (fake.borrow_mut())(SpawnCall::Resume {
                            id: id.to_string(),
                            lease,
                            project,
                        })
                    };
                    start_restore(state, tab, go, &mut spawn, project)
                }
            };
            out.push_all(Dest::Page(ViewId::BASE), switch_payloads(state));
            out.restored = Some(match result {
                RestoreResult::Started { notice } => notice,
                _ => None,
            });
        }
    }
    out
}

/// [`start_deferred`] for a tab of another agent: its trust plan is built from the discovery the
/// gate's answer is bound to, and the agent's own connect worker is begun with it. Without such a
/// discovery (the trust check could not run) or with a root no agent configuration can name, the
/// start is dropped and says why. So is an untrusted start of an agent that has no way to leave the
/// project's configuration out, whatever routed it here: such an agent starts only after a `y`.
fn start_external_tab(
    state: &mut AgentPanelState,
    tab: TabId,
    agent: AgentId,
    start: DeferredStart,
    project: ProjectTrust,
) -> TrustOut {
    // Taken whatever happens to this start, so a wait left for it never holds a later one.
    let wait = state.resume_after.remove(&tab);
    let profile = trust_profile(&state.acp_agents, &agent);
    if project == ProjectTrust::Untrusted && profile.requires_explicit_yes() {
        return put_off_start(state, tab, start, &crate::trust_gate::refused_without_yes(&profile));
    }
    // A registered agent starts with the command its registration names; a tab of a name nothing
    // registers (saved while it was) has none to run.
    let acp = match &agent {
        AgentId::Acp(name) => match acp_spec(&state.acp_agents, name) {
            Some(spec) => Some(spec),
            None => {
                let why = format!("{name} was not started: no agent of that name is registered in init.lua");
                return put_off_start(state, tab, start, &why);
            }
        },
        AgentId::Claude | AgentId::Codex => None,
    };
    let answer = match project {
        ProjectTrust::Trusted => agent::DirTrust::Trusted,
        ProjectTrust::Untrusted => agent::DirTrust::Untrusted,
    };
    let plan = match state.trust.bound_discovery(&profile) {
        Some(discovery) => {
            eitri_core::project_trust::trust_plan(discovery, answer, &discovery.root).map_err(|e| e.to_string())
        }
        None => Err(format!(
            "{} was not started: the trust check of this project has not run",
            agent.label()
        )),
    };
    let plan = match plan {
        Ok(plan) => plan,
        Err(why) => return put_off_start(state, tab, start, &why),
    };
    let (request_id, resume, resumed_title, resumed_name, first_turn) = match start {
        DeferredStart::FirstSend { request_id, first_turn } => (request_id, None, None, None, Some(first_turn)),
        DeferredStart::Resume {
            request_id,
            provider_session_id,
            resumed_title,
            resumed_name,
        } => (request_id, Some(provider_session_id), resumed_title, resumed_name, None),
        // A restore is started by `start_deferred` itself.
        DeferredStart::Restore { .. } => return TrustOut::default(),
    };
    let mode = state.tabs.get(tab).map_or(SessionModeChoice::Auto, |t| t.mode());
    let project_dir = state.project_dir.clone();
    let codex_command = state.codex_command.clone();
    // Only the resume of the session whose old backend it waits for.
    let after_old = wait
        .filter(|(id, _)| resume.as_deref() == Some(id.as_str()))
        .map(|(_, gone)| gone);
    let result_rx = match &state.spawner {
        Spawner::Real => spawn_external(
            agent,
            project_dir,
            plan,
            mode,
            resume.clone(),
            codex_command,
            acp,
            state.start_notes.sender(),
            after_old,
        ),
        #[cfg(test)]
        Spawner::Fake(fake) => (fake.borrow_mut())(SpawnCall::External {
            agent,
            plan,
            mode,
            resume: resume.clone(),
            acp,
            after_old,
        }),
    };
    let Some(t) = state.tabs.get_mut(tab) else {
        return TrustOut::default();
    };
    t.project_tiers = Some(project);
    let resuming = resume.is_some();
    t.backend = TabBackend::Starting(PendingStart {
        request_id,
        result_rx,
        first_turn,
        resume,
        resumed_title,
        resumed_name,
    });
    let mut out = TrustOut::default();
    if resuming {
        let (view, payloads) = shown_payloads(state, tab);
        out.push_all(Dest::Page(view), payloads);
    } else {
        let view = state.tabs.view_of(tab);
        out.push(Dest::Page(view), tabs_payload_recorded(state, view));
    }
    out
}

/// A start that will not happen, taken back as an `Escape` on its question would be: a first message
/// returns to the composer, a resume or a restore is dropped with a toast. `why` is said in the panel
/// and is the refused command's error.
fn put_off_start(state: &mut AgentPanelState, tab: TabId, start: DeferredStart, why: &str) -> TrustOut {
    state.resume_after.remove(&tab);
    let mut out = TrustOut::default();
    let view = state.tabs.view_of(tab);
    out.push(Dest::Page(view), tabs_payload_recorded(state, view));
    out.push(Dest::Tab(tab), eitri_core::agent_bridge::serialize_notice_for_js(why));
    match start {
        DeferredStart::FirstSend {
            request_id: sent,
            first_turn,
        } => return_first_message(state, tab, &sent, &first_turn, why, &mut out),
        DeferredStart::Resume { request_id: asked, .. } => {
            let asked_from = start_answer_dest(state, tab, &asked);
            out.push(asked_from, serialize_command_result_for_js(&asked, Err(why)));
            out.toast = Some(why.to_string());
        }
        DeferredStart::Restore { go } => {
            put_off_restore(state, tab, go, why, &mut out);
            out.toast = Some(why.to_string());
        }
    }
    out
}

/// A restore whose start will not happen keeps its sessions as failed tabs
/// ([`TabSet::keep_put_off_restore`]). The base page is shown its tabs again, and a view a kept tab
/// came back into is shown that tab.
fn put_off_restore(state: &mut AgentPanelState, from: TabId, go: RestoreGo, why: &str, out: &mut TrustOut) {
    let held_before = state.tabs.views();
    if !state.tabs.keep_put_off_restore(from, go, why) {
        state.restore_source = None;
    }
    out.push_all(Dest::Page(ViewId::BASE), switch_payloads(state));
    for view in state.tabs.views() {
        if held_before.contains(&view) {
            continue;
        }
        if let Some(tab) = state.tabs.tab_in(view) {
            let (view, payloads) = shown_payloads(state, tab);
            out.push_all(Dest::Page(view), payloads);
        }
    }
}

/// A first message whose start will not happen goes back to the composer, `why` being the refusal.
/// The refused send comes first: the page puts the message back in its box itself, keeping what was
/// typed there since and putting the message before it. Rust's copy of the draft is that same merge, so
/// the `draft` that follows (which replaces the box) holds both and loses neither.
fn return_first_message(
    state: &mut AgentPanelState,
    tab: TabId,
    sent: &str,
    first_turn: &FirstTurn,
    why: &str,
    out: &mut TrustOut,
) {
    let typed_since = state.tabs.get(tab).map(|t| t.draft.clone()).unwrap_or_default();
    let draft = if typed_since.is_empty() {
        first_turn.typed.clone()
    } else {
        format!("{}\n{typed_since}", first_turn.typed)
    };
    state.tabs.set_draft(tab, &draft);
    // The refusal to the page that sent the message, which reports it with its text if it no longer
    // shows the tab; the draft to the page that shows the tab now, whose box takes the message back.
    let sent_from = start_answer_dest(state, tab, sent);
    out.push(sent_from, serialize_command_result_for_js(sent, Err(why)));
    out.push(
        Dest::Tab(tab),
        eitri_core::agent_bridge::serialize_refused_draft_for_js(tab, &draft, sent),
    );
}

/// Records that the start of `tab`, owed a `command_result` under `request_id`, was asked from `view`'s
/// page: that page holds the request, wherever the tab is when the start ends.
fn note_start_page(state: &mut AgentPanelState, tab: TabId, request_id: &str, view: ViewId) {
    state.start_pages.insert(tab, (request_id.to_string(), view));
}

/// Where the `command_result` owed under `request_id` for `tab`'s start goes: the page it was asked
/// from, or the page of the tab's view when none is recorded. The record is spent.
fn start_answer_dest(state: &mut AgentPanelState, tab: TabId, request_id: &str) -> Dest {
    match state.start_pages.remove(&tab) {
        Some((asked, view)) if asked == request_id => Dest::Page(view),
        Some(other) => {
            state.start_pages.insert(tab, other);
            Dest::Tab(tab)
        }
        None => Dest::Tab(tab),
    }
}

/// A tab waiting for the trust question, given what the gate knows now: started if the answer is
/// already known, asked if it is on screen and its question is not, left waiting otherwise. Does
/// nothing while its check or its record is still out.
fn reroute_or_ask(state: &mut AgentPanelState, tab: TabId) -> TrustOut {
    let mut out = TrustOut::default();
    let asking = match state.tabs.get(tab).map(|t| &t.backend) {
        Some(TabBackend::AwaitingTrust(AwaitingTrust {
            recording: false,
            asking,
            ..
        })) => *asking,
        _ => return out,
    };
    if state.trust.is_waiting(tab) {
        return out;
    }
    let profile = waiting_profile(state, tab);
    match state.trust.current_route(&profile) {
        None => {}
        Some(Route::Start(project)) => {
            if let Some(start) = state.tabs.take_deferred(tab) {
                out.extend(start_deferred(state, tab, start, project));
            }
        }
        Some(Route::Ask) => {
            if asking.is_some_and(|nonce| state.trust.is_shown(tab, nonce)) {
                return out;
            }
            if state.page_of(tab).is_none() {
                // Asked when it is next on screen, in the page that shows it then.
                state.trust.hide(tab);
                return out;
            }
            let Ok(nonce) = state.tabs.ask_trust(tab) else {
                return out;
            };
            match state.trust.ask(tab, nonce, &profile) {
                Some(Asked::Prompt(message)) => out.push(Dest::Tab(tab), message),
                // Nothing in a prompt may be cut, so one too large to show refuses its start.
                Some(asked @ (Asked::TooLarge { .. } | Asked::Unavailable)) => {
                    if let Some(start) = state.tabs.take_deferred(tab) {
                        let why = match asked {
                            Asked::TooLarge { agent, bytes } => crate::trust_gate::too_large_to_start(&agent, bytes),
                            _ => crate::trust_gate::UNAVAILABLE_START.to_string(),
                        };
                        out.extend(put_off_start(state, tab, start, &why));
                    }
                }
                None => {}
            }
        }
    }
    out
}

/// [`reroute_or_ask`] for every waiting tab, in tab order.
fn sweep_awaiting(state: &mut AgentPanelState) -> TrustOut {
    let mut out = TrustOut::default();
    for tab in state.tabs.awaiting_trust() {
        out.extend(reroute_or_ask(state, tab));
    }
    out
}

/// The tick's half of the gate: what its worker finished, acted on. Never blocks.
fn trust_events(state: &mut AgentPanelState) -> TrustOut {
    use eitri_core::agent_bridge::serialize_notice_for_js;
    let mut out = TrustOut::default();
    for event in state.trust.poll() {
        match event {
            GateEvent::Route(tab, Route::Start(project)) => {
                if let Some(start) = state.tabs.take_deferred(tab) {
                    out.extend(start_deferred(state, tab, start, project));
                }
            }
            // Asked by the sweep below, which knows whether the tab is on screen.
            GateEvent::Route(_, Route::Ask) | GateEvent::Reask(_) => {}
            GateEvent::Recorded { tab, notice } => {
                if let Some(start) = state.tabs.take_recorded(tab) {
                    out.extend(start_deferred(state, tab, start, ProjectTrust::Trusted));
                    if let Some(notice) = notice {
                        out.push(Dest::Tab(tab), serialize_notice_for_js(&notice));
                    }
                }
            }
            GateEvent::RecordRefused { tab, notice } => {
                out.push(Dest::Tab(tab), serialize_notice_for_js(&notice));
                // The `y` is spent; the tab waits for a new answer, asked by the sweep below.
                if state.tabs.ask_trust(tab).is_ok() {
                    state.trust.hide(tab);
                }
            }
            GateEvent::CommandAsk {
                request_id: command,
                tab,
            } => {
                // Shown only in the page the command was typed in, while that page still shows the
                // tab: a tab that moved to another page while the check ran must not take the prompt
                // with it, since that page never typed the command and could answer it.
                let in_its_page = match state.trust_commands.get(&command) {
                    Some(record) => state.page_of(tab) == Some(record.typed_in),
                    None => state.page_of(tab).is_some(),
                };
                let shown = in_its_page.then(|| state.trust.ask_command(tab, &command)).flatten();
                match shown {
                    Some(Asked::Prompt(message)) => out.push(Dest::Tab(tab), message),
                    // Too large to show, or not ready: the command is over.
                    Some(asked @ (Asked::TooLarge { .. } | Asked::Unavailable)) => {
                        let why = match asked {
                            Asked::TooLarge { agent, bytes } => crate::trust_gate::too_large(&agent, bytes),
                            _ => crate::trust_gate::UNAVAILABLE_COMMAND.to_string(),
                        };
                        if let Some((typed_in, request_id)) = end_trust_command(state, &command) {
                            out.push(typed_in, serialize_notice_for_js(&why));
                            out.push(typed_in, serialize_command_result_for_js(&request_id, Err(&why)));
                        }
                    }
                    // The tab is not on screen in the page that typed it any more: `:trust` ends as
                    // not trusted on the next poll, in that page.
                    None => state.trust.drop_command_prompt(),
                }
            }
            GateEvent::CommandDone {
                request_id: command,
                ok,
                notice,
            } => {
                let Some((typed_in, request_id)) = end_trust_command(state, &command) else {
                    eprintln!("[agent_panel] {command} ended, but no page's :trust is recorded for it");
                    continue;
                };
                if let Some(notice) = &notice {
                    out.push(typed_in, serialize_notice_for_js(notice));
                }
                let error = notice.as_deref().unwrap_or(crate::trust_gate::NOT_TRUSTED);
                out.push(
                    typed_in,
                    serialize_command_result_for_js(&request_id, if ok { Ok(()) } else { Err(error) }),
                );
            }
        }
    }
    out.extend(sweep_awaiting(state));
    out
}

/// One `:trust` or `:untrust` the gate is working on.
struct TrustCommand {
    /// The id the page that typed it knows it by.
    request_id: String,
    /// The tab whose `:` line it was typed in.
    tab: TabId,
    /// The page it was typed in: its result goes there.
    typed_in: ViewId,
}

/// The command the gate knows as `command` is over: its record goes, and its result is owed to the
/// page it was typed in, under that page's own request id. `None` for a command no record names.
fn end_trust_command(state: &mut AgentPanelState, command: &str) -> Option<(Dest, String)> {
    state
        .trust_commands
        .remove(command)
        .map(|c| (Dest::Page(c.typed_in), c.request_id))
}

/// `:trust` / `:untrust` from the `:` line of `view`'s page, about the agent of the tab that page
/// shows. `agent` is the agent the page believed that tab runs; a command typed for another agent's
/// tab (the page was behind a switch) is refused rather than applied to whichever agent is on screen
/// now. An extra page that shows no tab has no agent to trust.
fn trust_command(
    state: &mut AgentPanelState,
    view: ViewId,
    request_id: &str,
    action: eitri_core::agent_bridge::TrustAction,
    agent: &str,
) -> Result<(), String> {
    let Some(shown) = state.tab_of_page(view).and_then(|tab| state.tabs.get(tab)) else {
        return Err(NO_TAB_HERE.to_string());
    };
    let (tab, running) = (shown.id, shown.agent().clone());
    if running.record_name() != agent {
        let command = match action {
            eitri_core::agent_bridge::TrustAction::Trust => ":trust",
            eitri_core::agent_bridge::TrustAction::Untrust => ":untrust",
        };
        let asked = AgentId::from_record_name(agent).map_or_else(|| format!("{agent:?}"), |a| a.label());
        return Err(format!(
            "that {command} was for a {asked} tab; this tab runs {}",
            running.label()
        ));
    }
    let command = format!("trust-command-{}", state.next_trust_command);
    state.next_trust_command += 1;
    state.trust_commands.insert(
        command.clone(),
        TrustCommand {
            request_id: request_id.to_string(),
            tab,
            typed_in: view,
        },
    );
    state
        .trust
        .command(command, action, tab, &trust_profile(&state.acp_agents, &running));
    Ok(())
}

/// Drains the gate from the tick.
fn poll_trust(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages) {
    let out = trust_events(&mut state.borrow_mut());
    deliver_trust(state, pages, out);
}

/// `y` or `n` to a trust question, a tab's or `:trust`'s. Only checks and queues: a `y` starts its
/// session when the worker says the disk is unchanged since the prompt (and the record written, when
/// one is kept), never here.
// One parameter per field of the answer, and the view of the page that sent it.
#[allow(clippy::too_many_arguments)]
fn answer_trust(
    state: &mut AgentPanelState,
    request_id: &str,
    tab: TabId,
    nonce: u64,
    fingerprint: &str,
    findings_digest: &str,
    trust: bool,
    view: ViewId,
) -> TrustOut {
    use eitri_core::agent_bridge::serialize_notice_for_js;
    let mut out = TrustOut::default();
    let here = Dest::Page(view);
    let now = now_unix();
    if state.tabs.check_trust_answer(tab, nonce, view).is_ok() {
        match state.trust.answer(tab, nonce, fingerprint, findings_digest, trust, now) {
            AnswerOutcome::Start(project, notice) => {
                let Ok(start) = state.tabs.take_answered(tab, nonce, view) else {
                    out.push(
                        here,
                        serialize_command_result_for_js(request_id, Err(crate::trust_gate::NOT_CURRENT)),
                    );
                    return out;
                };
                out.extend(start_deferred(state, tab, start, project));
                if let Some(notice) = notice {
                    out.push(here, serialize_notice_for_js(&notice));
                }
                out.push(here, serialize_command_result_for_js(request_id, Ok(())));
                // The window's answer may cover the other tabs waiting for it.
                out.extend(sweep_awaiting(state));
            }
            AnswerOutcome::Recording => {
                let marked = state.tabs.mark_recording(tab, nonce, view);
                out.push(
                    here,
                    serialize_command_result_for_js(request_id, marked.as_ref().map(|_| ()).map_err(String::as_str)),
                );
            }
            AnswerOutcome::Refused(why) => {
                out.push(here, serialize_notice_for_js(&why));
                out.push(here, serialize_command_result_for_js(request_id, Err(&why)));
                // Asked again with what is on disk now, or started if there is nothing left to ask.
                out.extend(reroute_or_ask(state, tab));
            }
            AnswerOutcome::NotStarted(why) => {
                let Ok(start) = state.tabs.take_answered(tab, nonce, view) else {
                    out.push(
                        here,
                        serialize_command_result_for_js(request_id, Err(crate::trust_gate::NOT_CURRENT)),
                    );
                    return out;
                };
                out.extend(put_off_start(state, tab, start, &why));
                // The answer itself was taken; it is the start that is refused.
                out.push(here, serialize_command_result_for_js(request_id, Ok(())));
            }
        }
        return out;
    }
    if state.tab_of_page(view) == Some(tab) && state.trust.is_command_prompt(tab, nonce) {
        match state
            .trust
            .answer_command(tab, nonce, fingerprint, findings_digest, trust, now)
        {
            Some(CommandAnswer::Done {
                request_id: command,
                ok,
                notice,
            }) => {
                if let Some(notice) = &notice {
                    out.push(here, serialize_notice_for_js(notice));
                }
                let error = notice.as_deref().unwrap_or(crate::trust_gate::NOT_TRUSTED);
                if let Some((typed_in, command)) = end_trust_command(state, &command) {
                    out.push(
                        typed_in,
                        serialize_command_result_for_js(&command, if ok { Ok(()) } else { Err(error) }),
                    );
                }
                out.push(here, serialize_command_result_for_js(request_id, Ok(())));
            }
            Some(CommandAnswer::Recording) => out.push(here, serialize_command_result_for_js(request_id, Ok(()))),
            None => out.push(
                here,
                serialize_command_result_for_js(request_id, Err(crate::trust_gate::NOT_CURRENT)),
            ),
        }
        return out;
    }
    out.push(
        here,
        serialize_command_result_for_js(request_id, Err(crate::trust_gate::NOT_CURRENT)),
    );
    out
}

/// Which trust question an answer is to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrustAsked {
    /// The tab's own question, which holds its start.
    Tab,
    /// The prompt a `:trust` typed in the page asked.
    Command,
}

/// Which trust question an answer from `view`'s page is to, if that page may answer it: the tab's own
/// question it shows, or the `:trust` prompt typed in it. `None` is an answer [`answer_trust`] refuses
/// as no longer current. Changes nothing.
fn trust_question_answered(state: &AgentPanelState, tab: TabId, nonce: u64, view: ViewId) -> Option<TrustAsked> {
    if state.tabs.check_trust_answer(tab, nonce, view).is_ok() {
        Some(TrustAsked::Tab)
    } else if state.tab_of_page(view) == Some(tab) && state.trust.is_command_prompt(tab, nonce) {
        Some(TrustAsked::Command)
    } else {
        None
    }
}

/// The refusal for a `:trust` prompt's answer the window's gate turned down: the command is over, and
/// the user typed it, so typing it again is its way back.
fn trust_command_refused(why: &str) -> String {
    format!("{why} — :trust was not answered; type :trust again once it is uncovered")
}

/// A trust answer the window's gate refused (`why`, said to the page). A tab's own question stays
/// open: the page closed it when the key was pressed, so it is asked again in that page under a fresh
/// nonce, and the tab keeps waiting for an answer from an uncovered page. A `:trust` prompt ends the
/// command unrecorded (`not trusted`, on the next poll).
// One parameter per fact of the refused answer, and the view of the page that sent it.
#[allow(clippy::too_many_arguments)]
fn refuse_trust_answer(
    state: &mut AgentPanelState,
    request_id: &str,
    tab: TabId,
    nonce: u64,
    asked: TrustAsked,
    why: &str,
    view: ViewId,
) -> TrustOut {
    use eitri_core::agent_bridge::serialize_notice_for_js;
    let mut out = TrustOut::default();
    let here = Dest::Page(view);
    match asked {
        TrustAsked::Tab => {
            out.push(here, serialize_notice_for_js(why));
            out.push(here, serialize_command_result_for_js(request_id, Err(why)));
            if state.tabs.check_trust_answer(tab, nonce, view).is_ok() {
                state.trust.hide(tab);
                out.extend(reroute_or_ask(state, tab));
            }
        }
        TrustAsked::Command => {
            let text = trust_command_refused(why);
            if state.trust.is_command_prompt(tab, nonce) {
                state.trust.drop_command_prompt();
            }
            out.push(here, serialize_notice_for_js(&text));
            out.push(here, serialize_command_result_for_js(request_id, Err(&text)));
        }
    }
    out
}

/// `Escape` on a trust question: the start is put off. A first message goes back to the composer; a
/// resume is dropped and the toast says so; a restore keeps its sessions (as failed tabs holding their
/// resume ids, or still on offer in an otherwise empty window). Nothing was leased, so nothing is released.
fn cancel_trust_question(
    state: &mut AgentPanelState,
    request_id: &str,
    tab: TabId,
    nonce: u64,
    view: ViewId,
) -> TrustOut {
    use crate::trust_gate::{NOT_CURRENT, NOT_STARTED, NOT_TRUSTED, PUT_OFF};
    let mut out = TrustOut::default();
    let here = Dest::Page(view);
    if state.tabs.check_trust_answer(tab, nonce, view).is_err() {
        if state.tab_of_page(view) == Some(tab) {
            if let Some(command) = state.trust.cancel_command(tab, nonce) {
                if let Some((typed_in, command)) = end_trust_command(state, &command) {
                    out.push(typed_in, serialize_command_result_for_js(&command, Err(NOT_TRUSTED)));
                }
                out.push(here, serialize_command_result_for_js(request_id, Ok(())));
                return out;
            }
        }
        out.push(here, serialize_command_result_for_js(request_id, Err(NOT_CURRENT)));
        return out;
    }
    let Ok(start) = state.tabs.cancel_trust(tab, nonce, view) else {
        out.push(here, serialize_command_result_for_js(request_id, Err(NOT_CURRENT)));
        return out;
    };
    state.trust.hide(tab);
    let tabs_view = state.tabs.view_of(tab);
    out.push(Dest::Page(tabs_view), tabs_payload_recorded(state, tabs_view));
    match start {
        DeferredStart::FirstSend {
            request_id: sent,
            first_turn,
        } => return_first_message(state, tab, &sent, &first_turn, NOT_STARTED, &mut out),
        DeferredStart::Resume { request_id: asked, .. } => {
            let asked_from = start_answer_dest(state, tab, &asked);
            out.push(asked_from, serialize_command_result_for_js(&asked, Err(PUT_OFF)));
            out.toast = Some(PUT_OFF.to_string());
        }
        DeferredStart::Restore { go } => {
            put_off_restore(state, tab, go, PUT_OFF, &mut out);
            out.toast = Some(PUT_OFF.to_string());
        }
    }
    out.push(here, serialize_command_result_for_js(request_id, Ok(())));
    out
}

/// Keeps the saved-tabs file in step with the tabs: once per tick, cheap when nothing changed. A change is
/// handed to the writer thread (`TabWriter`); nothing here touches the disk.
fn remember_tabs(state: &Rc<RefCell<AgentPanelState>>) {
    let mut state_ref = state.borrow_mut();
    let state_ref = &mut *state_ref;
    // A dashboard restore still waiting is listed from the first write that would otherwise leave it out.
    if state_ref.tabs.settle_waiting_restore() {
        state_ref.restore_source = None;
    }
    let snapshot = state_ref.tabs.saved_snapshot(&state_ref.conversation_id);
    state_ref.tab_memory.observe(snapshot);
}

/// The chooser's envelope (`prefix w` in `view`'s page): every open tab in number order, a tab bound
/// to an extra view naming it, then every record open in no tab, each marked if another window holds
/// its lease. Also each record listed with the title shown.
fn chooser_payload(state: &AgentPanelState, view: ViewId) -> (String, Vec<(String, Option<String>)>) {
    // The records offered are the agent's of the tab the opening page shows, as the resume that
    // follows starts in it.
    let agent = hello_agent(state, view);
    let greeting = BackendGreeting::for_agent_titled(
        state.backend_kind,
        &agent,
        state.project_dir.clone(),
        eitri_core::agent_backend::Titles::Cached(&state.titles),
    );
    let lease_name = agent.record_name();
    let open: Vec<ChooserTab> = state
        .tabs
        .tabs()
        .iter()
        .map(|t| {
            let facts = t.facts();
            ChooserTab {
                tab: t.id,
                label: ChooserTab::label(t.number, &t.label_name(), t.view_id()),
                marker: eitri_core::tabs::marker(facts),
                pending: facts.pending,
                resumable: t.resumable(state.backend_kind),
            }
        })
        .collect();
    let canonical = state.canonical_project_dir.as_str();
    let records = chooser_records(&greeting.resumable, &state.tabs.open_session_ids(), |id| {
        agent::lease::SessionLease::is_held(&lease_name, canonical, id).unwrap_or(false)
    });
    let listed = records
        .iter()
        .map(|r| (r.provider_session_id.clone(), r.title.clone()))
        .collect();
    (
        eitri_core::agent_bridge::serialize_chooser_for_js(&open, &records),
        listed,
    )
}

/// The detail popover's envelope for `tab` (`prefix i`, `open_detail{tab}`).
fn detail_payload(state: &AgentPanelState, tab: TabId) -> String {
    let rows = match state.tabs.get(tab) {
        Some(t) => detail_rows(
            t,
            state.backend_kind,
            agent::account::configured().map(agent::account::ClaudeAccount::name),
            &state.project_dir,
            state
                .rules_dir
                .as_ref()
                .map(|d| eitri_core::permission_store::path(d, &state.project_dir))
                .as_deref(),
            &state.trust.row(&trust_profile(&state.acp_agents, t.agent())),
            state
                .trust
                .next_session_trust(&trust_profile(&state.acp_agents, t.agent())),
            &state.acp_agents,
            stop_background_keys(state.keymap_help.as_deref()).as_deref(),
        ),
        None => Vec::new(),
    };
    eitri_core::agent_bridge::serialize_tab_detail_for_js(tab, &rows)
}

/// The backend a tab command acts on, or the benign "no active session" refusal (an empty,
/// starting or failed tab). Never another tab's.
fn backend_for(
    tabs: &mut eitri_core::tab_set::TabSet,
    tab: TabId,
) -> Result<&mut AgentBackend, eitri_core::agent_backend::BackendError> {
    tabs.get_mut(tab)
        .and_then(|t| t.live_mut())
        .ok_or_else(no_session_error)
}

/// The chooser's records: every resumable session open in no tab, newest first as `hello` ranks
/// them, each marked if another window holds its lease.
fn chooser_records(
    sessions: &[agent::ResumableSession],
    open: &[String],
    held: impl Fn(&str) -> bool,
) -> Vec<ChooserRecord> {
    sessions
        .iter()
        .filter(|s| !open.contains(&s.provider_session_id))
        .map(|s| ChooserRecord {
            provider_session_id: s.provider_session_id.clone(),
            name: s.name.clone(),
            title: s.title.clone(),
            created_at: s.created_at.clone(),
            updated_at: s.updated_at.clone(),
            held_elsewhere: held(&s.provider_session_id),
        })
        .collect()
}

/// `prefix i`'s rows for one tab, in the order they are shown, `—` for anything unknown. `trust_row` is the
/// window's latest trust check in words, and `next` the tiers a session started now would get. A tab
/// that runs another agent than Claude gets that agent's rows (`external_detail_rows`); the Claude rows
/// below are the same for a Claude tab on either backend.
///
/// `provider_session_id()` is read into a local BEFORE `projection()` is taken: on the sidecar path
/// both lock the same ingestion mutex (the 2026-09-15 GTK freeze; `tab_set`'s module doc).
fn detail_rows(
    tab: &Tab,
    kind: BackendKind,
    account: Option<&str>,
    project_dir: &Path,
    rules_file: Option<&Path>,
    trust_row: &str,
    next: ProjectTrust,
    registered: &[AgentRegistration],
    stop_keys: Option<&str>,
) -> Vec<DetailRow> {
    if tab.agent().is_external() {
        return external_detail_rows(tab, kind, project_dir, trust_row, next, registered, stop_keys);
    }
    const UNKNOWN: &str = "—";
    let known = |value: Option<String>| value.filter(|v| !v.is_empty()).unwrap_or_else(|| UNKNOWN.to_string());
    let backend = tab.live();
    let provider_session_id = backend.and_then(AgentBackend::provider_session_id);
    let (model, cwd) = match backend {
        Some(b) => {
            let projection = b.projection();
            (projection.model.clone(), projection.cwd.clone())
        }
        None => (None, None),
    };
    let conversation_id = backend.and_then(|b| b.conversation_id().map(str::to_string));
    let session_id = backend.and_then(|b| b.session_id().map(str::to_string));
    let info = backend.and_then(|b| b.provider_info());
    let cli = info.map(|i| {
        let mut line = if i.actual_claude_code_version.is_empty() {
            UNKNOWN.to_string()
        } else {
            i.actual_claude_code_version.clone()
        };
        for diagnostic in &i.startup_diagnostics {
            line.push_str(" · ");
            line.push_str(diagnostic);
        }
        line
    });
    let revision = info.map(|i| {
        format!(
            "{} · expected {}",
            i.build_description.as_deref().unwrap_or(UNKNOWN),
            agent::EXPECTED_VERDANDI_REVISION
        )
    });
    let record = match (&conversation_id, &provider_session_id) {
        (Some(conversation), Some(session)) => agent::persistence::load_conversation_record(conversation, session).ok(),
        _ => None,
    };
    let resumable = if tab.resumable(kind) {
        "yes"
    } else {
        "no (legacy backend)"
    };
    let rows: [(&str, String); 20] = [
        ("name", known(tab.name.clone())),
        ("title", known(tab.title.clone())),
        ("state", tab.wire_state().as_str().to_string()),
        ("mode", tab.mode().as_str().to_string()),
        ("model", known(model)),
        ("backend", kind.as_str().to_string()),
        ("account", known(account.map(str::to_string))),
        (
            "cwd",
            known(cwd.or_else(|| Some(project_dir.to_string_lossy().into_owned()))),
        ),
        ("conversation", known(conversation_id)),
        ("verdandi session", known(session_id)),
        ("claude session", known(provider_session_id)),
        ("CLI", known(cli)),
        // A fact about every session on either backend, so it is drawn on an empty tab too: a
        // started tab says what its session was given, an empty one what its next would get.
        (
            "settings",
            agent::setting_sources::note_for_session(
                agent::setting_sources::loads_user_settings(),
                tab.project_tiers.unwrap_or(next),
            )
            .to_string(),
        ),
        ("trust", trust_row.to_string()),
        ("Verdandi revision", known(revision)),
        ("resumable", resumable.to_string()),
        (
            "created",
            record_stamp_row(record.as_ref().map(|r| r.created_at.as_str())),
        ),
        (
            "updated",
            record_stamp_row(record.as_ref().map(|r| r.updated_at.as_str())),
        ),
        ("queued", tab.queue.len().to_string()),
        ("permission rules", known(rules_file.map(|p| p.display().to_string()))),
    ];
    rows.into_iter()
        .map(|(label, value)| DetailRow {
            label: label.to_string(),
            value,
        })
        .collect()
}

/// What Codex does without asking, for `prefix i`: the sandbox's reach and every route around it, as
/// they were measured, not softened. `root` is the project the sandbox is rooted at; the second half
/// of the sentence follows the tab's mode: in auto Codex's reviewer decides, in manual every such
/// request is a card, and in bypass Eitri allows it.
fn codex_runs_without_asking(root: &Path, mode: SessionModeChoice) -> String {
    let who_decides = match mode {
        SessionModeChoice::Bypass => "Eitri allows anything else",
        SessionModeChoice::Manual => "you decide anything else on a card",
        SessionModeChoice::Auto => "its reviewer decides anything else",
    };
    format!(
        "Codex runs commands and edits inside {} without asking, reads anywhere, no network or sockets; \
         your exec-policy allow rules run unsandboxed; {who_decides}, which then runs with your full \
         rights; MCP tools follow their servers' approval modes",
        root.display()
    )
}

/// What a mode change does to a Codex tab and when: Eitri's own answers follow it at once, Codex's
/// reviewer only at the next turn. Codex's reviewer is the user's in manual and bypass, its own in auto.
const CODEX_MODE_LAG: &str = "a mode change reaches Codex's reviewer at the next turn; a running turn keeps the \
     reviewer it started with (a turn started in manual or bypass: Codex asks about everything outside its \
     sandbox for the rest of it, and Eitri draws a card, or accepts in bypass; a turn started in auto: Codex's \
     reviewer still decides for the rest of it, and Eitri draws a card for what still arrives, or accepts it \
     in bypass)";

/// What an ACP agent runs without a question. Eitri sets only the mode the registration names, once at the
/// start, and does not know what any mode of the agent lets through; without one the agent's own default
/// stays and Eitri cannot say even which mode that is.
fn acp_runs_without_asking(auto_mode: Option<&str>) -> String {
    match auto_mode {
        Some(mode) => format!(
            "whatever the agent's mode {mode} lets through, set at the start; Eitri shows every request it receives"
        ),
        None => "Eitri does not know what this agent runs without asking (no auto_mode registered, so its own \
                 default mode); Eitri shows every request it receives"
            .to_string(),
    }
}

/// What a mode change does to an ACP tab: Eitri answers by the tab's mode at once, and the agent's own mode,
/// set once at the start, never changes with it.
const ACP_MODE_LAG: &str = "none: Eitri answers by the tab's mode at once (entering bypass approves the cards \
     waiting); the agent's own mode is not changed";

/// How full an agent last said its context window is, in words.
fn context_row(gauge: agent::external::ContextGauge) -> String {
    if gauge.size == 0 {
        return format!("{} used", gauge.used);
    }
    let percent = gauge.used.saturating_mul(100) / gauge.size;
    format!("{percent}% full ({} of {})", gauge.used, gauge.size)
}

/// The account limit an agent last reported, in words.
fn limits_row(limits: &agent::AccountLimitsRecord) -> String {
    let mut line = format!("{:.0}% used", limits.used_percent);
    if let Some(minutes) = limits.window_minutes {
        line.push_str(&format!(" of the {minutes}-minute window"));
    }
    if let Some(resets) = limits.resets_at.and_then(|t| u64::try_from(t).ok()) {
        line.push_str(&format!(
            " · resets {} {:02}:{:02} UTC",
            eitri_core::project_trust::civil_date(resets),
            resets % 86_400 / 3_600,
            resets % 3_600 / 60
        ));
    }
    if let Some(plan) = limits.plan.as_deref().filter(|p| !p.is_empty()) {
        line.push_str(&format!(" · plan {plan}"));
    }
    line
}

/// A `created`/`updated` row of `prefix i`: the record's stamp as local date and time, or a dash for a tab
/// with no record.
fn record_stamp_row(raw: Option<&str>) -> String {
    raw.map_or_else(|| "\u{2014}".to_string(), eitri_core::tabs::record_stamp)
}

/// The `settings` row of a Codex tab: Codex's own configuration directory, the one it reported (a
/// `CODEX_HOME` the user set, not always `~/.codex`), and what of the project is loaded. Before a session
/// has reported one it says where Codex looks.
fn codex_settings_row(configuration_home: Option<&str>, project_loaded: &str) -> String {
    let whose = match configuration_home {
        Some(home) => format!("your Codex configuration in {home} (always;"),
        None => "your Codex configuration (CODEX_HOME, else ~/.codex; always;".to_string(),
    };
    format!("{whose} skills off while this project is untrusted); this project's: {project_loaded}")
}

/// `prefix i`'s rows for a tab that runs another agent than Claude. The Claude-only rows (Verdandi
/// session, Claude session, CLI, Verdandi revision, permission rules) are not shown: they describe
/// things such a tab does not have.
fn external_detail_rows(
    tab: &Tab,
    kind: BackendKind,
    project_dir: &Path,
    trust_row: &str,
    next: ProjectTrust,
    registered: &[AgentRegistration],
    stop_keys: Option<&str>,
) -> Vec<DetailRow> {
    const UNKNOWN: &str = "—";
    let known = |value: Option<String>| value.filter(|v| !v.is_empty()).unwrap_or_else(|| UNKNOWN.to_string());
    let agent = tab.agent();
    let is_codex = matches!(agent, AgentId::Codex);
    let backend = tab.live();
    // As on the Claude rows: the session id is read before the projection is taken.
    let provider_session_id = backend.and_then(|b| b.provider_session_id());
    let (cwd, limits, background) = match backend {
        Some(b) => {
            let projection = b.projection();
            (
                projection.cwd.clone(),
                projection.account_limits.clone(),
                projection.background_calls,
            )
        }
        None => (None, None, 0),
    };
    let conversation_id = backend.and_then(|b| b.conversation_id().map(str::to_string));
    let version = backend.and_then(|b| b.provider_info()).map(|info| {
        let mut line = info.build_description.clone().unwrap_or_else(|| UNKNOWN.to_string());
        for diagnostic in &info.startup_diagnostics {
            line.push_str(" · ");
            line.push_str(diagnostic);
        }
        line
    });
    let configuration_home = backend
        .and_then(|b| b.provider_info())
        .and_then(|info| info.configuration_home.clone());
    let record = match (&conversation_id, &provider_session_id) {
        (Some(conversation), Some(session)) => agent::persistence::load_conversation_record(conversation, session).ok(),
        _ => None,
    };
    let project_loaded = match tab.project_tiers.unwrap_or(next) {
        ProjectTrust::Trusted => "loaded",
        ProjectTrust::Untrusted => "not loaded",
    };
    let (backend_name, session_label, settings, runs_without_asking, mode_lag, limits_label, limits_value) = if is_codex
    {
        (
            "codex app-server".to_string(),
            "thread",
            codex_settings_row(configuration_home.as_deref(), project_loaded),
            codex_runs_without_asking(project_dir, tab.mode()),
            CODEX_MODE_LAG.to_string(),
            "limits",
            limits.as_ref().map(limits_row),
        )
    } else {
        let auto_mode = match agent {
            AgentId::Acp(name) => registered
                .iter()
                .find(|a| a.name == *name)
                .and_then(|a| a.auto_mode.clone()),
            _ => None,
        };
        let context = match backend {
            Some(AgentBackend::External(external)) => external.conversation().context_gauge(),
            _ => None,
        };
        (
            "acp".to_string(),
            "session",
            format!("the agent's own configuration (always); this project's: {project_loaded}"),
            acp_runs_without_asking(auto_mode.as_deref()),
            ACP_MODE_LAG.to_string(),
            "context",
            context.map(context_row),
        )
    };
    let rows: [(&str, String); 18] = [
        ("name", known(tab.name.clone())),
        ("title", known(tab.title.clone())),
        ("state", tab.wire_state().as_str().to_string()),
        ("mode", tab.mode().as_str().to_string()),
        ("agent", known(version.or_else(|| Some(agent.label())))),
        ("backend", backend_name),
        (
            "cwd",
            known(cwd.or_else(|| Some(project_dir.to_string_lossy().into_owned()))),
        ),
        ("conversation", known(conversation_id)),
        (session_label, known(provider_session_id)),
        ("settings", settings),
        ("trust", trust_row.to_string()),
        ("runs without asking", runs_without_asking),
        ("mode lag", mode_lag),
        (limits_label, known(limits_value)),
        ("resumable", if tab.resumable(kind) { "yes" } else { "no" }.to_string()),
        (
            "created",
            record_stamp_row(record.as_ref().map(|r| r.created_at.as_str())),
        ),
        (
            "updated",
            record_stamp_row(record.as_ref().map(|r| r.updated_at.as_str())),
        ),
        ("queued", tab.queue.len().to_string()),
    ];
    let mut rows: Vec<DetailRow> = rows
        .into_iter()
        .map(|(label, value)| DetailRow {
            label: label.to_string(),
            value,
        })
        .collect();
    // Only Codex counts what it leaves running after a turn, and only its tab can stop it.
    if is_codex {
        rows.push(DetailRow {
            label: "background".to_string(),
            value: background_row(background, stop_keys),
        });
    }
    rows
}

/// The `background` row of `prefix i`: how many commands the agent left running after its turns ended,
/// and the key that stops them (`stop_keys`, as the user presses it; `None` when no key is bound).
fn background_row(running: u32, stop_keys: Option<&str>) -> String {
    if running == 0 {
        return "none".to_string();
    }
    let them = if running == 1 { "it" } else { "them" };
    match stop_keys {
        Some(keys) => format!("{running} running ({keys} stops {them} with the session)"),
        None => {
            format!("{running} running (no key is bound to tab.stop-background, which stops {them} with the session)")
        }
    }
}

/// How the user presses `tab.stop-background`, read from the panel table the window last sent (the
/// leader by its own name): the default `Space k` before any was sent, `None` when no key is bound.
fn stop_background_keys(keymap_help: Option<&str>) -> Option<String> {
    let Some(help) = keymap_help else {
        return Some("Space k".to_string());
    };
    let help: serde_json::Value = serde_json::from_str(help).ok()?;
    let panel = &help["panel"];
    let leader = panel["leaderLabel"].as_str().unwrap_or("Space");
    let binding = panel["bindings"]
        .as_array()?
        .iter()
        .find(|b| b["action"] == "tab.stop-background")?;
    let keys: Vec<String> = binding["keys"]
        .as_array()?
        .iter()
        .filter_map(|k| k.as_str())
        .map(|k| match k {
            "<leader>" => leader.to_string(),
            " " => "Space".to_string(),
            other => other.to_string(),
        })
        .collect();
    (!keys.is_empty()).then(|| keys.join(" "))
}

/// The connect worker, shared by `send_message` on an empty tab and `resume`. Off the GTK main
/// loop: constructing the sidecar backend spawns a real process, does a real gRPC handshake, and on
/// a cold Verdandi checkout runs `npm ci` + `npm run build` -- minutes, in the cold case.
fn spawn_connect(
    kind: BackendKind,
    project_dir: PathBuf,
    resume: Option<String>,
    project: ProjectTrust,
) -> mpsc::Receiver<Result<AgentBackend, BackendError>> {
    spawn_connect_holding(kind, project_dir, resume, None, project)
}

/// What the band says while a Codex binary Eitri has not checked yet goes through its self-check.
const CHECKING_CODEX: &str = "checking this Codex\u{2026} (once for this binary, a few seconds)";

/// Where the self-check keeps a verdict, under Eitri's state directory: `codex-check/<key>/verdict.json`,
/// written only for a pass.
const CODEX_CHECK_DIR: &str = "codex-check";

/// Whether starting `program` will run the self-check first: no passing verdict is kept for this binary
/// under `state_home`. Asked only to say so in the band; the start decides for itself. A binary whose
/// key cannot be read, or one too old to be checked, is refused by the start before any check runs, so
/// it says nothing here. Runs a `--version` of the binary: worker threads only.
fn codex_check_pending(program: &Path, state_home: Option<&Path>) -> bool {
    let Some(state_home) = state_home else { return false };
    let Ok(key) = agent::providers::codex::BinaryKey::of(program) else {
        return false;
    };
    if key
        .version_numbers()
        .is_some_and(|found| found < agent::providers::codex::MIN_CODEX)
    {
        return false;
    }
    !state_home
        .join(CODEX_CHECK_DIR)
        .join(key.id())
        .join("verdict.json")
        .is_file()
}

/// The notes connect workers send while they work, drained by the tick.
struct StartNotes {
    tx: mpsc::Sender<String>,
    rx: mpsc::Receiver<String>,
}

impl Default for StartNotes {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        StartNotes { tx, rx }
    }
}

impl StartNotes {
    fn sender(&self) -> mpsc::Sender<String> {
        self.tx.clone()
    }

    /// Every note sent since the last call, as `notice` payloads for the band. Never blocks.
    fn drain(&self) -> Vec<String> {
        self.rx
            .try_iter()
            .map(|note| eitri_core::agent_bridge::serialize_notice_for_js(&note))
            .collect()
    }
}

/// How long a session continued in its own tab (`r`) waits for the tab's old backend to let go of the lease.
const OLD_SESSION_LET_GO: std::time::Duration = std::time::Duration::from_secs(10);

/// The connect worker of another agent's tab: locates its binary (Codex) or takes its registered
/// command (`acp`, an ACP agent) and starts (or resumes) its session with `plan`, off the GTK main loop,
/// since the agent's own check and handshake take seconds. With `after_old`, it first waits (at most
/// [`OLD_SESSION_LET_GO`]) for the tab's old backend to be dropped.
#[allow(clippy::too_many_arguments)]
fn spawn_external(
    agent: AgentId,
    project_dir: PathBuf,
    plan: agent::TrustPlan,
    mode: SessionModeChoice,
    resume: Option<String>,
    codex_command: Option<String>,
    acp: Option<agent::providers::acp::AcpAgentSpec>,
    notes: mpsc::Sender<String>,
    after_old: Option<mpsc::Receiver<()>>,
) -> mpsc::Receiver<Result<AgentBackend, BackendError>> {
    use eitri_core::agent_backend::ExternalLaunch;
    let (result_tx, result_rx) = mpsc::channel();
    std::thread::spawn(move || {
        // The session's previous backend holds its lease until it is dropped; the resume would find it held.
        // Bounded: a teardown that hangs leaves the resume to fail on the lease and say so.
        if let Some(after_old) = after_old {
            let _ = after_old.recv_timeout(OLD_SESSION_LET_GO);
        }
        let launch = match &agent {
            AgentId::Codex => {
                // A relative home, or one inside the project, is refused before anything runs, the version
                // probe of the binary included.
                let located = agent::providers::codex::refuse_codex_home(&project_dir)
                    .map_err(|e| e.to_string())
                    .and_then(|()| {
                        agent::providers::codex::locate_codex(codex_command.as_deref()).map_err(|e| e.to_string())
                    })
                    .map_err(|e| BackendError {
                        message: format!("Codex could not start: {e}"),
                        benign: false,
                        folded_events: Vec::new(),
                    });
                // The start is about to run the self-check: the tab would otherwise sit in "starting"
                // for seconds with nothing said.
                if let Ok(program) = &located {
                    let state_home = eitri_core::layout::persist::state_subdir(
                        std::env::var_os("XDG_STATE_HOME").as_deref(),
                        std::env::var_os("HOME").as_deref(),
                        "",
                    );
                    if codex_check_pending(program, state_home.as_deref()) {
                        let _ = notes.send(CHECKING_CODEX.to_string());
                    }
                }
                located.map(ExternalLaunch::Codex)
            }
            AgentId::Acp(name) => acp
                .ok_or_else(|| BackendError {
                    message: format!("{name} was not started: no agent of that name is registered in init.lua"),
                    benign: false,
                    folded_events: Vec::new(),
                })
                .map(ExternalLaunch::Acp),
            AgentId::Claude => Err(BackendError {
                message: "a Claude tab is not started as another agent's".to_string(),
                benign: false,
                folded_events: Vec::new(),
            }),
        };
        let result = launch.and_then(|launch| {
            AgentBackend::start_external(agent, launch, &project_dir, plan, mode, resume.as_deref())
        });
        // As in `spawn_connect_holding`: a receiver dropped mid-connect makes this drop the cleanup.
        let _ = result_tx.send(result);
    });
    result_rx
}

/// [`spawn_connect`] for a resume whose session lease is already held: the worker hands it to the
/// resume, which keeps it for as long as the session lives.
fn spawn_connect_holding(
    kind: BackendKind,
    project_dir: PathBuf,
    resume: Option<String>,
    lease: Option<agent::lease::SessionLease>,
    project: ProjectTrust,
) -> mpsc::Receiver<Result<AgentBackend, BackendError>> {
    let (result_tx, result_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = AgentBackend::start_holding(kind, &project_dir, resume.as_deref(), lease, project);
        // The receiver is gone only if the tab or the panel was torn down mid-connect; dropping
        // the backend here is then the cleanup (`Retiring` normally holds the receiver instead).
        let _ = result_tx.send(result);
    });
    result_rx
}

/// `agent-ui/web/src/problems.ts`'s own `SIDECAR_STOPPED_MARKER`, matched the same way (a plain
/// substring check) and pinned to the same source: `stream_ended_early_reason`'s opening sentence
/// in `agent/src/providers/claude_sidecar/watch.rs` -- see
/// `the_sidecar_stopped_marker_is_pinned_to_watch_rs` below. A `reason` carrying this means the
/// provider's event stream ended under a session that really was open, which is not what
/// `never_opened_message` below guesses for every other reason.
const SIDECAR_STOPPED_MARKER: &str = "the connection to the provider ended before this session did";

/// The text `report_sessions_that_never_opened` shows for a `reason`. Split out so it can be
/// tested without a live tab.
///
/// **Most `reason`s here mean the session never opened at all**, so the generic guess ("it most
/// likely no longer exists") is right for them. **A resumed sidecar session is the exception**: it
/// reports itself open only at its first new turn (`AgentConversation` never re-plays
/// `SessionReady` on attach), so a sidecar that stops before that first turn lands here too even
/// though the conversation and its transcript are completely fine -- the guess is simply wrong for
/// it. `agent-ui/web/src/problems.ts`'s `classifySidecarStopped`/`failureEvidence` read this
/// reason correctly on the panel side, so this wrapper gives a `reason` naming a stopped sidecar
/// its own wording with no guess appended instead of folding it into the generic case.
fn never_opened_message(reason: &str) -> String {
    if reason.contains(SIDECAR_STOPPED_MARKER) {
        format!("the agent sidecar stopped ({reason})")
    } else {
        format!(
            "the session ended before it started ({reason}). If you were continuing a previous \
             conversation, it most likely no longer exists -- start a new session instead."
        )
    }
}

/// A session that reached a terminal state without ever opening never worked, and must not be
/// left on screen as an empty conversation the user cannot act on. Checked for every tab.
///
/// `AgentConversation::resume` catches the common case synchronously, within its own short window.
/// This is the backstop for everything slower than that window and for fresh sessions, which have
/// no equivalent check: either way the tab becomes `Failed` with the provider's own reason, where `r` starts it over. It is never turned into a fresh session automatically. The
/// `error{tab}` envelope goes out only if that tab is on screen, to the page that shows it; the tab
/// keeps the reason for when it is shown, and no other tab is touched.
///
/// **`reason` used to be a fixed, generic string on the legacy backend** ("provider process
/// exited unexpectedly") for every non-zero exit, regardless of cause -- and that generic text
/// actively misdirected a real investigation (2026-09-18, work, production binary): the child
/// died before opening because a `PATH`-resolved multi-account launcher refused the gate-bearing
/// `--settings` flag outright, and the launcher's own one-line explanation had already gone past
/// on stderr with nowhere for this banner to recover it from. `agent::session`'s translation of
/// `AgentEvent::ProcessExited` now folds the child's own retained stderr tail into
/// `SessionUnavailable.reason` itself (see `agent/src/session.rs`), so `reason` below is already
/// the specific one when the provider produced any stderr at all before dying. See
/// `never_opened_message` for the one exception to the guess it otherwise wraps `reason` in.
fn report_sessions_that_never_opened(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages) {
    let failed: Vec<(TabId, String)> = {
        let state_ref = state.borrow();
        state_ref
            .tabs
            .tabs()
            .iter()
            .filter(|t| !t.reported_start_failure)
            .filter_map(|t| {
                t.live()
                    .and_then(AgentBackend::terminated_before_opening)
                    .map(|r| (t.id, r))
            })
            .collect()
    };
    for (tab, reason) in failed {
        let message = never_opened_message(&reason);
        eprintln!("[agent_panel] tab {}: {message}", tab.0);
        // Asked before the state is borrowed: the window's coverage probe may read the panel.
        let shown_in = state.borrow().shown_in(tab);
        let shown = shown_in.is_some_and(|view| pages.live(view).is_some_and(|page| on_screen(state, view, &page)));
        let dead = {
            let mut state_ref = state.borrow_mut();
            let on_screen = shown;
            let Some(t) = state_ref.tabs.get_mut(tab) else { continue };
            t.reported_start_failure = true;
            t.failed_after_start = false;
            // The session that never opened is still the one the file should keep for this tab.
            t.failed_resume = t.live().and_then(AgentBackend::provider_session_id);
            // Its cards go with it (the tick reports the change).
            t.attention.session_ended(on_screen);
            std::mem::replace(
                &mut t.backend,
                TabBackend::Failed {
                    reason: message.clone(),
                },
            )
        };
        if let TabBackend::Live(backend) = dead {
            let mut state_ref = state.borrow_mut();
            state_ref.tabs.note_retiring(tab);
            state_ref.retiring.backend(backend, Some(tab));
        }
        if let Some(view) = shown_in {
            pages.send(view, &serialize_error_for_js(tab, &message));
        }
    }
}

/// Every connect that finished since the last tick (`TabSet::collect_starts`), answered. `try_recv`
/// only, inside the set: this runs on the GTK main loop 30 times a second and must never block it --
/// which is the entire reason construction was moved off this thread in the first place.
///
/// What the page showing a tab is owed goes to that page; a command's answer to the page it was sent
/// from (`start_answer_dest`), which need not show the tab any more.
fn collect_pending_starts(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages) {
    let collected = state.borrow_mut().tabs.collect_starts();
    for result in collected {
        let part_of_a_restore = account_for_restore(&mut state.borrow_mut(), &result);
        match result {
            StartCollected::Installed {
                tab,
                request_id,
                first_turn,
            } => {
                // A tab a restore started: nobody asked for this connect by name, so there is no
                // `command_result` owed -- the restore reports once, when its last resume is back.
                let restored = part_of_a_restore != RestoreStart::NotPartOfOne;
                // The page has been showing this tab as starting; give it the real projection at
                // once rather than making it wait for the first event.
                let shown = {
                    let mut state_ref = state.borrow_mut();
                    match state_ref.shown_in(tab) {
                        Some(view) if view.is_base() || state_ref.ready(view) => {
                            Some((view, view_payloads(&mut state_ref, view)))
                        }
                        _ => None,
                    }
                };
                if let Some((view, payloads)) = shown {
                    pages.send_all(view, payloads);
                }
                if !restored {
                    match first_turn {
                        Some(turn) => send_first_turn(state, pages, tab, &request_id, turn),
                        None => send_start_answer(
                            state,
                            pages,
                            tab,
                            &request_id,
                            &serialize_command_result_for_js(&request_id, Ok(())),
                        ),
                    }
                }
            }
            StartCollected::Failed { tab, request_id, error } => {
                eprintln!(
                    "[agent_panel] tab {}: backend failed to start: {}",
                    tab.0, error.message
                );
                // A tab a restore started is reported with the rest of the restore, and was tidied
                // away by `account_for_restore`, rather than left on screen as a dead tab each.
                if let RestoreStart::Failed { was_active } = part_of_a_restore {
                    if was_active {
                        send_switch(state, pages);
                    }
                    continue;
                }
                send_start_answer(
                    state,
                    pages,
                    tab,
                    &request_id,
                    &serialize_command_result_for_js(&request_id, Err(&error.message)),
                );
                // Only if that tab is on screen; it keeps the reason for later.
                let shown = state.borrow().shown_in(tab);
                if let Some(view) = shown {
                    pages.send(view, &serialize_error_for_js(tab, &error.message));
                }
            }
        }
    }
    {
        // A start that ended another way (its tab closed, or was reset into a new one) owes nothing.
        let mut state_ref = state.borrow_mut();
        let state_ref = &mut *state_ref;
        let tabs = &state_ref.tabs;
        state_ref.start_pages.retain(|tab, _| {
            tabs.get(*tab)
                .is_some_and(|t| matches!(t.backend, TabBackend::Starting(_) | TabBackend::AwaitingTrust(_)))
        });
    }
    finish_restore(state);
}

/// Sends `payload`, the answer owed under `request_id` when `tab`'s start ended, to the page that asked
/// for the start.
fn send_start_answer(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages, tab: TabId, request_id: &str, payload: &str) {
    let view = {
        let mut state_ref = state.borrow_mut();
        let asked_from = start_answer_dest(&mut state_ref, tab, request_id);
        dest_view(&state_ref, asked_from)
    };
    pages.send(view, payload);
}

/// Sends `payload` to the page of `tab`'s view, if it is live: a command's answer, or a notice about
/// the tab, wherever it is in that view.
fn send_to_tabs_page(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages, tab: TabId, payload: &str) {
    let view = state.borrow().tabs.view_of(tab);
    pages.send(view, payload);
}

/// How one collected connect relates to the restore in progress, if there is one.
#[derive(Debug, PartialEq, Eq)]
enum RestoreStart {
    /// An ordinary connect (a resume by hand, a first message): handled as it always was.
    NotPartOfOne,
    Installed,
    /// A resume the restore started was refused. The tab is already tidied away; `was_active` says
    /// the page was showing it, so it must be told which tab it is showing now.
    Failed {
        was_active: bool,
    },
}

/// Counts a collected connect into the restore that started it. A refused one is also taken off the
/// screen: an empty tab again if the restore began in it, gone if the restore made it.
fn account_for_restore(state: &mut AgentPanelState, collected: &StartCollected) -> RestoreStart {
    let AgentPanelState {
        restore_run,
        tabs,
        emptied_views,
        ..
    } = state;
    let Some(run) = restore_run.as_mut() else {
        return RestoreStart::NotPartOfOne;
    };
    match collected {
        StartCollected::Installed { tab, .. } if run.owns(*tab) => {
            run.note_installed(*tab);
            RestoreStart::Installed
        }
        StartCollected::Failed { tab, error, .. } if run.owns(*tab) => {
            let back_to = run.back_to(*tab);
            run.note_failed(*tab, &error.message);
            let was_active = tabs.active() == *tab;
            let view = tabs.view_of(*tab);
            tabs.discard_failed_restore(*tab, back_to);
            // A restored tab that came back into an extra view and is discarded leaves that view.
            if !view.is_base() && tabs.tab_in(view).is_none() {
                emptied_views.push(view);
            }
            // The tab on screen did not come back: the first that did (or is coming) takes its place,
            // rather than leaving an empty dashboard over tabs that restored.
            if was_active {
                if let Some(survivor) = run.first_survivor() {
                    tabs.select(survivor);
                }
            }
            RestoreStart::Failed { was_active }
        }
        _ => RestoreStart::NotPartOfOne,
    }
}

/// The first turn of a session started by it, sent once its backend is installed. Its
/// `command_result` is the one the `send_message` that started the session was owed.
fn send_first_turn(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages, tab: TabId, request_id: &str, turn: FirstTurn) {
    let outcome = {
        let mut state_ref = state.borrow_mut();
        let mut mode = eitri_core::agent_bridge::SessionModeChoice::Auto;
        if let Some(t) = state_ref.tabs.get_mut(tab) {
            t.turn_trace = eitri_core::turn_trace::TurnTrace::start();
            mode = t.mode();
        }
        backend_for(&mut state_ref.tabs, tab).and_then(|backend| backend.send_turn_in(&turn.wire, &turn.typed, mode))
    };
    if outcome.is_ok() {
        title_from_first_prompt(state, tab, &turn.typed);
    }
    // The `send_message` is answered in the page it came from.
    let view = {
        let mut state_ref = state.borrow_mut();
        let sent_from = start_answer_dest(&mut state_ref, tab, request_id);
        dest_view(&state_ref, sent_from)
    };
    apply_outcome(state, pages, tab, Some((view, request_id)), outcome);
}

/// A tab with no title takes its first prompt's (`agent::persistence::title_from_prompt`, the rule
/// the sidecar's record uses too).
fn title_from_first_prompt(state: &Rc<RefCell<AgentPanelState>>, tab: TabId, typed: &str) {
    let mut state_ref = state.borrow_mut();
    if let Some(t) = state_ref.tabs.get_mut(tab) {
        if t.title.is_none() {
            t.title = agent::persistence::title_from_prompt(typed);
        }
    }
}

/// Every tab's in-flight terminal handoff, checked without blocking.
///
/// The command is dispatched here and nowhere else, which is what makes the handoff ordering real: by
/// the time the frontend can show a user the line to run, the session's own `shutdown()` has
/// already returned on the worker thread. The handoff card goes out only if its tab is on screen, to
/// the page that shows it (the tab keeps it in `last_handoff` for when it is shown); the
/// `command_result` is owed either way, and the tick's `tabs` envelope reports the state change.
fn collect_pending_handoffs(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages) {
    let handing_off: Vec<TabId> = state
        .borrow()
        .tabs
        .tabs()
        .iter()
        .filter(|t| t.pending_handoff.is_some())
        .map(|t| t.id)
        .collect();
    for tab in handing_off {
        let finished = {
            let mut state_ref = state.borrow_mut();
            let shown = state_ref.shown_in(tab);
            let Some(t) = state_ref.tabs.get_mut(tab) else { continue };
            let Some(pending) = t.pending_handoff.as_ref() else {
                continue;
            };
            let outcome = classify_close_signal(&pending.closed_rx);
            if outcome == HandoffCloseOutcome::StillClosing {
                continue;
            }
            let pending = t.pending_handoff.take().expect("checked Some above");
            t.reported_start_failure = false;
            t.turn_trace = None;
            if outcome == HandoffCloseOutcome::Closed {
                // Into the Rust host, not only down the wire: the panel's own reload affordance
                // would otherwise destroy the only copy, and on the default legacy backend nothing
                // else anywhere remembers this session id.
                t.last_handoff = Some(pending.command.clone());
            }
            (pending, outcome, shown)
        };
        let (pending, outcome, shown) = finished;
        if outcome == HandoffCloseOutcome::CloseFailed {
            eprintln!("[agent_panel] tab {}: {HANDOFF_CLOSE_FAILED_MESSAGE}", tab.0);
        }
        match shown {
            Some(view) if view == pending.page => pages.send_all(
                view,
                handoff_payloads(tab, &outcome, Some(&pending.request_id), &pending.command),
            ),
            // The tab moved, or is in the background: the page showing it gets the card or the error,
            // and the page that asked gets the answer, which ends its wait for the close wherever the
            // tab is now. Without it that page would keep the tab's composer disabled as closing.
            _ => {
                if let Some(view) = shown {
                    pages.send_all(view, handoff_payloads(tab, &outcome, None, &pending.command));
                }
                let result = match outcome {
                    HandoffCloseOutcome::CloseFailed => Err(HANDOFF_CLOSE_FAILED_MESSAGE),
                    _ => Ok(()),
                };
                pages.send(
                    pending.page,
                    &serialize_command_result_for_js(&pending.request_id, result),
                );
            }
        }
    }
}

/// What one tick of the handoff collector found, as a value rather than as control flow.
///
/// Split out because this is the sequencing the whole feature's honesty rests on -- the card says
/// "This conversation is closed in Eitri", which is true only because the command is not released
/// until `AgentBackend::shutdown()` has returned -- and a property that load-bearing should be
/// assertable without a `WebView`, a GTK loop or a real backend.
#[derive(Debug, PartialEq, Eq)]
enum HandoffCloseOutcome {
    /// `shutdown()` has not returned yet. Nothing at all is dispatched in this state.
    StillClosing,
    /// The worker sent, which it does only after `shutdown()` returned.
    Closed,
    /// The worker's sender dropped without a send: it panicked inside `shutdown()`.
    CloseFailed,
}

fn classify_close_signal(closed_rx: &mpsc::Receiver<()>) -> HandoffCloseOutcome {
    match closed_rx.try_recv() {
        Err(mpsc::TryRecvError::Empty) => HandoffCloseOutcome::StillClosing,
        Ok(()) => HandoffCloseOutcome::Closed,
        Err(mpsc::TryRecvError::Disconnected) => HandoffCloseOutcome::CloseFailed,
    }
}

/// The envelopes owed for a handoff whose close reached `outcome`, in dispatch order, when its tab
/// is on screen; the `command_result` only when `request_id` is given (the page showing the tab is
/// the one that asked).
///
/// The property worth stating: only `Closed` ever produces a `handoff` envelope. A close that is
/// still running, or one that died partway, must not be followed by a card whose first line claims
/// the conversation is closed.
fn handoff_payloads(
    tab: TabId,
    outcome: &HandoffCloseOutcome,
    request_id: Option<&str>,
    command: &agent::handoff::ClaudeResumeCommand,
) -> Vec<String> {
    match outcome {
        HandoffCloseOutcome::StillClosing => Vec::new(),
        HandoffCloseOutcome::Closed => {
            let mut payloads = vec![eitri_core::agent_bridge::serialize_handoff_for_js(tab, command)];
            payloads.extend(request_id.map(|id| serialize_command_result_for_js(id, Ok(()))));
            payloads
        }
        // The session is gone regardless -- it was handed to the worker before this. Saying so is
        // the only honest option: an `error` envelope returns the tab to its empty screen, where a
        // failed handoff reads as a dead session rather than as a conversation that is still there.
        HandoffCloseOutcome::CloseFailed => {
            let mut payloads: Vec<String> = request_id
                .map(|id| serialize_command_result_for_js(id, Err(HANDOFF_CLOSE_FAILED_MESSAGE)))
                .into_iter()
                .collect();
            payloads.push(serialize_error_for_js(tab, HANDOFF_CLOSE_FAILED_MESSAGE));
            payloads
        }
    }
}

/// Everything a freshly-mounted document is owed, in the order it must arrive.
///
/// A pure function over plain data, for the same reason `SnapshotView` is one: the reload path has
/// no `WebView`-free test otherwise, and "what does a reloaded panel get back" is exactly the list
/// a later change loses something from without any test noticing.
///
/// `hello` first, then the theme and the keymap (colours before anything drawn with them), then the
/// window-level envelopes in `window` (the history, then the editor context), then the
/// `tabs` envelope, then the active tab's own state. The handoff card is now one of the active
/// tab's own payloads (`TabSet::active_state_payloads`), as its snapshot is.
// One parameter per thing the page is owed, in the order it receives them.
#[allow(clippy::too_many_arguments)]
fn ready_payloads(
    mut greeting: BackendGreeting,
    open: &[String],
    window: Vec<String>,
    tabs: String,
    active: Vec<String>,
    theme: Option<&str>,
    keymap: Option<&str>,
    restore: Option<&RestoreOffer>,
) -> Vec<String> {
    // No session open in any tab, or handed off from one, is offered for resume -- the
    // lease refuses a second driver, and a handed-off one has a CLI writing it.
    greeting.resumable.retain(|r| !open.contains(&r.provider_session_id));
    let mut payloads = vec![serialize_hello_with_restore_for_js(&greeting, restore)];
    payloads.extend(theme.map(str::to_string));
    payloads.extend(keymap.map(str::to_string));
    // Window-level state (the history, then the editor context), before the tabs that use it.
    payloads.extend(window);
    payloads.push(tabs);
    payloads.extend(active);
    payloads
}

/// The window-level envelopes of a fresh document, in order: the history, the editor context and,
/// only in a window that has an editor to follow, where the panel stands with it. The one-window
/// mode never sets a link, so its list is the two it always was.
fn window_payloads(history: String, context: String, link: Option<&String>) -> Vec<String> {
    let mut payloads = vec![history, context];
    payloads.extend(link.cloned());
    payloads
}

/// Ends every draft edit still out in nvim, for an editor that went away: the body file's text
/// becomes the draft when it differs from what was handed out, else the draft keeps its own text.
/// A marker nvim wrote wins; without one the body is read as it stands. Each edit's files go after
/// it is read. Returns the tabs that had an edit out, oldest edit first, with the new draft where
/// there is one.
fn end_pending_edits(state: &mut AgentPanelState) -> Vec<(TabId, Option<String>)> {
    use eitri_core::scratch::EditDone;
    let edits = std::mem::take(&mut state.pending_edits);
    let results: Vec<(u64, EditDone)> = edits
        .iter()
        .map(|edit| {
            let done = edit
                .poll()
                .unwrap_or_else(|| match std::fs::read_to_string(&edit.body) {
                    Ok(raw) => EditDone::Written(raw.strip_suffix('\n').unwrap_or(&raw).to_string()),
                    Err(_) => EditDone::Discarded,
                });
            (edit.id, done)
        })
        .collect();
    let ended = state.tabs.cancel_scratch_edits(&results);
    for edit in &edits {
        edit.cleanup();
    }
    ended
}

/// Resets the nav-mode mirror (`AgentPanelState::nav_mode`'s own doc): called from the `Ready` arm on
/// every `ready` -- the first one and every reload's alike -- so the mirror never survives a
/// reload holding whatever mode the torn-down document last reported.
///
/// Without this, a reload while the composer is open (`nav_mode == Input`) leaves the mirror at
/// `Input` even though the fresh document starts in BROWSE. Before that document posts its own
/// first `panel_keys` message, the host's `claims(Input, Direction::Down)` is `false`, so a
/// `Ctrl+j` the user presses to enter the composer is not claimed and falls through to
/// `move_focus` instead -- the opposite of what BROWSE + `Ctrl+j` must do.
///
/// A plain field write rather than a bigger function only because it is one: kept apart from the
/// rest of `Ready`'s payload-building so it has its own doc and its own test
/// (`ready_resets_the_nav_mode_mirror_to_other`), the same reason `ready_payloads` and
/// `changed_envelope` are pulled out of that arm.
fn reset_nav_mode_for_ready(state: &AgentPanelState, view: ViewId) {
    if let Some(record) = state.page(view) {
        record.nav_mode.set(PanelKeys::Other);
    }
}

/// The other half of the same rule: when the page that reported a mode goes -- its web process
/// died (`page_gone`, both the reload and the give-up path) or it is being
/// replaced (`reload_document`) -- the mirror is `Other` until a new page says otherwise. The
/// give-up document (`webview_crash_guard::crash_message_html`) never sends `ready`, so the reset
/// on `ready` alone left `Ctrl+j` claimed for a page that could not take it.
fn forget_the_pages_nav_mode(state: &AgentPanelState, view: ViewId) {
    if let Some(record) = state.page(view) {
        record.nav_mode.set(PanelKeys::Other);
    }
}

/// A window-level envelope for the tick: `Some` when the page can take it and it changed since the
/// last one sent. Recorded as sent only when it is returned.
fn changed_envelope(last: &mut Option<String>, now: String, document_ready: bool) -> Option<String> {
    if !document_ready || last.as_deref() == Some(now.as_str()) {
        return None;
    }
    *last = Some(now.clone());
    Some(now)
}

/// The window's check on an answer from a page (`AgentPanelHandle::set_answer_gate`), asked with that
/// page's view.
type AnswerGate = Rc<dyn Fn(ViewId) -> Result<(), String>>;

/// An answer from `view`'s page, through the window's gate first: a refusal returns the gate's words
/// and never runs `answer`, so the card stays waiting. The gate is asked at the moment of the answer
/// and with the panel's state released, so what it reads is the window as it is now.
fn through_answer_gate<T>(
    gate: Option<&AnswerGate>,
    view: ViewId,
    answer: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    if let Some(gate) = gate {
        gate(view)?;
    }
    answer()
}

/// Whether `view`'s page may answer about `tab`: refused, naming the tab, when the tab is in another
/// view. Asked before the window's gate, so an answer from a page that does not show the tab never
/// reaches the window, whatever covers it.
fn page_may_answer(tabs: &eitri_core::tab_set::TabSet, tab: TabId, view: ViewId) -> Result<(), String> {
    if tabs.may_answer(tab, view) {
        return Ok(());
    }
    Err(match tabs.get(tab) {
        Some(t) => format!("tab {} is not shown in this page", t.number),
        None => format!("no tab {}", tab.0),
    })
}

/// The window's word on whether a float covers a chat view's module (`AgentPanelHandle::set_coverage`).
type Coverage = Rc<dyn Fn(ViewId) -> bool>;

/// Whether `view`'s page is on screen for attention: mapped and no float over its view. The window's
/// probe is asked with the panel's state released, so it may read the panel.
fn on_screen(state: &Rc<RefCell<AgentPanelState>>, view: ViewId, page: &PanelPage) -> bool {
    if !page.is_visible() {
        return false;
    }
    let coverage = state.borrow().coverage.clone();
    !coverage.is_some_and(|covered| covered(view))
}

/// A `permission_response`'s own fields, out of the message (see `answer_permission_response`).
struct PermissionAnswer {
    permission_id: String,
    decision: eitri_core::agent_bridge::DecisionChoice,
    reason: Option<String>,
    remember: bool,
}

impl PermissionAnswer {
    fn of(message: InboundMessage) -> Option<Self> {
        match message {
            InboundMessage::PermissionResponse {
                permission_id,
                decision,
                reason,
                remember,
                ..
            } => Some(PermissionAnswer {
                permission_id,
                decision,
                reason,
                remember,
            }),
            _ => None,
        }
    }
}

/// Everything a `permission_response` does, with no WebView, so its route is tested whole: the rule the card offered is saved first when the user chose "Always allow" (never taken
/// from the panel), then the answer goes through `TabSet::answer_card` -- the one route
/// that records a human approval, which is what lets the CLI's own prompt for the same call through
/// without a second card. `Err` is a refusal the panel shows (the card stays and `a`
/// still works); `Ok` is the answer's own outcome.
fn answer_permission_response(
    tabs: &mut eitri_core::tab_set::TabSet,
    rules_dir: Option<&Path>,
    project_dir: &Path,
    tab: TabId,
    page: ViewId,
    answer: PermissionAnswer,
) -> Result<Result<Vec<agent::AgentDomainEvent>, BackendError>, String> {
    let remembered = {
        let t = tabs.get(tab).ok_or_else(|| format!("protocol: no tab {}", tab.0))?;
        // Read right before the save, so an id that is no longer
        // pending -- or that this tab's user already answered, which the sidecar's projection still
        // lists until its `PermissionResolved` arrives -- saves nothing (`Tab::card_is_waiting`).
        let still_pending = t.card_is_waiting(&answer.permission_id);
        rule_to_remember(
            t,
            &answer.permission_id,
            answer.decision,
            answer.remember,
            still_pending,
        )
        .map_err(str::to_string)?
    };
    if let Some(rule) = remembered {
        let Some(dir) = rules_dir else {
            return Err("no state directory: the rule cannot be saved".to_string());
        };
        match eitri_core::permission_store::add(dir, project_dir, &rule) {
            Ok(rules) => {
                eprintln!(
                    "[permission] rule saved: {} ({})",
                    rule.to_rule_string(),
                    eitri_core::permission_store::path(dir, project_dir).display()
                );
                tabs.set_rules(rules);
            }
            // Not answered: the card stays and `a` still works.
            Err(e) => return Err(format!("could not save the rule: {e}")),
        }
    }
    let decision = answer.decision.into_decision(answer.reason);
    Ok(tabs.answer_card(tab, page, &answer.permission_id, decision))
}

/// The `remember` choice: the rule Rust offered for `permission_id`, only with an allow, and
/// only while that id is still a card waiting for the user.
/// `still_pending` is the caller's own read (`Tab::card_is_waiting`), taken right before this runs
/// -- passed in rather than read here so this stays testable without a live backend, the same
/// reason `changed_envelope` takes `document_ready` as a parameter.
///
/// The offer itself is gone once the first answer to that id succeeds, whatever the decision, and
/// never comes back (`TabSet::answer_card`, the tab's `user_answered`): on the sidecar path the
/// projection still lists the id as pending until the provider's real `PermissionResolved`
/// arrives, and `refresh_offers` recomputes offers from that projection whenever another id
/// arrives, so a one-time removal alone was undone by the next pump.
fn rule_to_remember(
    tab: &Tab,
    permission_id: &str,
    decision: eitri_core::agent_bridge::DecisionChoice,
    remember: bool,
    still_pending: bool,
) -> Result<Option<agent::PrefixRule>, &'static str> {
    if !remember {
        return Ok(None);
    }
    if decision != eitri_core::agent_bridge::DecisionChoice::Allow {
        return Err("only an allow can be remembered");
    }
    if !still_pending {
        return Err("this request was already answered");
    }
    tab.rule_offers
        .get(permission_id)
        .cloned()
        .map(Some)
        .ok_or("no rule can be offered for this request")
}

/// Milliseconds since the epoch, for a queued item's `queuedAt`; 0 on a clock before 1970.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The events a command's outcome owes the frontend, SUCCESS OR FAILURE.
///
/// A failure carries any the backend had already folded into its own projection before it failed
/// (`BackendError::folded_events`) -- today that is the legacy arm's user prompt, which `send_turn`
/// folds before it even attempts the send, deliberately. Dispatching only on the `Ok` arm is what
/// let a refused send show no prompt row live while the next snapshot carried one: the same message
/// read as unsent until a panel reload and as sent afterwards, which is precisely the
/// snapshot-vs-live indistinguishability this panel rests on.
///
/// A free function, not three arms of the `match` below deciding separately: separate arms is how
/// the success and failure paths diverged in the first place, and it makes the rule testable
/// without a WebView.
fn events_owed(
    outcome: &Result<Vec<agent::AgentDomainEvent>, eitri_core::agent_backend::BackendError>,
) -> &[agent::AgentDomainEvent] {
    match outcome {
        Ok(events) => events,
        Err(error) => &error.folded_events,
    }
}

/// The `events` envelope for a batch a backend RETURNED (folded into its own projection already)
/// rather than delivered through the pump: legacy's synthesized events, a refused send's folded
/// prompt, and the `PermissionResolved`s entering bypass produced on
/// legacy (`ConfirmOutcome::Entered::resolved`), which no pump ever carries. `None` for an empty
/// batch, or for a tab no page shows: each page draws only its own tab, and a background tab catches
/// up with a snapshot when it is switched to. With the view whose page shows the tab. The revisions
/// are the projection's, as the events were folded into it one each, just now, on this thread.
fn returned_events_payload(
    state: &AgentPanelState,
    tab: TabId,
    events: &[agent::AgentDomainEvent],
) -> Option<(ViewId, String)> {
    if events.is_empty() {
        return None;
    }
    let view = state.shown_in(tab)?;
    let through_revision = state
        .tabs
        .get(tab)
        .and_then(|t| t.live())
        .map(|b| b.projection().last_revision)
        .unwrap_or(0);
    let from_revision = through_revision.saturating_sub(events.len() as u64);
    Some((
        view,
        serialize_events_for_js(tab, from_revision, through_revision, events),
    ))
}

/// Applies one command's outcome to its tab and the frontend, uniformly.
///
/// The benign/fatal split is the whole point: a benign failure (a turn sent while one was running,
/// a permission answered twice) reports itself and leaves the session alone, while a fatal one
/// retires the tab's backend and marks the tab `Failed`. Events and the `error`
/// envelope go out only if `tab` is on screen, to the page that shows it -- each page draws only its
/// own tab, and a background tab catches up with a snapshot when it is switched to. The
/// `command_result` is owed either way, to `view`'s page, which sent the command.
fn apply_command_outcome(
    state: &Rc<RefCell<AgentPanelState>>,
    pages: &Pages,
    view: ViewId,
    tab: TabId,
    request_id: &str,
    outcome: Result<Vec<agent::AgentDomainEvent>, BackendError>,
) {
    apply_outcome(state, pages, tab, Some((view, request_id)), outcome);
}

/// [`apply_command_outcome`] for an outcome no request is waiting on (a queue flush):
/// `request` is `None` and no `command_result` goes out.
fn apply_outcome(
    state: &Rc<RefCell<AgentPanelState>>,
    pages: &Pages,
    tab: TabId,
    request: Option<(ViewId, &str)>,
    outcome: Result<Vec<agent::AgentDomainEvent>, BackendError>,
) {
    let events = events_owed(&outcome);
    let shown_in = state.borrow().shown_in(tab);
    // Only the legacy backend ever gets here with a non-empty batch: it synthesizes events its own
    // wire protocol cannot provide. The sidecar backend returns an empty vec and its state arrives
    // through the pump, from the server.
    let payload = returned_events_payload(&state.borrow(), tab, events);
    if let Some((view, payload)) = payload {
        pages.send(view, &payload);
    }
    match outcome {
        Ok(_) => {
            if let Some((view, id)) = request {
                pages.send(view, &serialize_command_result_for_js(id, Ok(())));
            }
        }
        Err(error) if error.benign => {
            eprintln!(
                "[agent_panel] tab {}: command rejected (session stays alive): {}",
                tab.0, error.message
            );
            if let Some((view, id)) = request {
                pages.send(view, &serialize_command_result_for_js(id, Err(&error.message)));
            }
        }
        Err(error) => {
            eprintln!("[agent_panel] tab {}: command failed fatally: {}", tab.0, error.message);
            // Retired on a worker thread, for the same reason construction runs on one. Two separate
            // problems with doing it here: `AgentConversation` has no `Drop`, so a bare drop never
            // sends `close_session` at all and the sidecar is left holding a session it thinks is
            // live; and the drop chain that DOES run (SpawnedSidecar polls for up to 3s before
            // escalating to SIGKILL, then RuntimeThread joins its Tokio thread) would block the
            // whole shell -- editor pane included -- on the GTK main loop.
            let attention_before = state.borrow().tabs.attention();
            let shown = shown_in.is_some_and(|view| pages.live(view).is_some_and(|page| on_screen(state, view, &page)));
            let dead = {
                let mut state_ref = state.borrow_mut();
                state_ref.tabs.get_mut(tab).map(|t| {
                    // The cards of a session that is gone can no longer be answered.
                    t.attention.session_ended(shown);
                    t.failed_after_start = true;
                    t.failed_resume = None;
                    std::mem::replace(
                        &mut t.backend,
                        TabBackend::Failed {
                            reason: error.message.clone(),
                        },
                    )
                })
            };
            if let Some(TabBackend::Live(backend)) = dead {
                let mut state_ref = state.borrow_mut();
                state_ref.tabs.note_retiring(tab);
                state_ref.retiring.backend(backend, Some(tab));
            }
            if let Some((view, id)) = request {
                pages.send(view, &serialize_command_result_for_js(id, Err(&error.message)));
            }
            if let Some(view) = shown_in {
                pages.send(view, &serialize_error_for_js(tab, &error.message));
            }
            report_attention(state, attention_before);
        }
    }
}

/// One stream payload for a page's pacer: the envelope, whose first text it carries, and its class.
type Stream = (
    String,
    Option<crate::panel_pacer::FirstText>,
    eitri_core::panel_cadence::EnvelopeClass,
);

/// The first text `tab`'s turn trace waits for, as the pacer stamps it: that tab and turn, never
/// whichever tab is on screen when the stamp lands.
fn first_text_of(tabs: &eitri_core::tab_set::TabSet, tab: TabId) -> Option<crate::panel_pacer::FirstText> {
    tabs.get(tab)
        .and_then(|t| t.turn_trace.as_ref())
        .map(|trace| crate::panel_pacer::FirstText {
            tab,
            turn: trace.submitted_at(),
        })
}

/// Applies the pacer's first-text stamps (`panel_pacer::Sent::first_text`): each to the tab and turn
/// its envelope was pumped for, at the moment it was handed to the page -- never to whichever tab
/// is active when this runs, and never to a later turn of the same tab
/// (`TurnTrace::mark_first_text_dispatched_for`). Called by the pump tick and before a
/// `turn_rendered` report is folded.
fn stamp_first_text_dispatches(
    state: &Rc<RefCell<AgentPanelState>>,
    stamps: Vec<(crate::panel_pacer::FirstText, std::time::Instant)>,
) {
    if stamps.is_empty() {
        return;
    }
    let mut state_ref = state.borrow_mut();
    for (first, at) in stamps {
        if let Some(trace) = state_ref.tabs.get_mut(first.tab).and_then(|t| t.turn_trace.as_mut()) {
            if trace.mark_first_text_dispatched_for(first.turn, at) && trace.is_complete() {
                trace.emit();
            }
        }
    }
}

/// The first texts the pacer threw away (`Pacer::discard_pending`: a reload, a fresh `ready`, the
/// give-up page) reach the panel inside a snapshot, which it reports no paint for: each turn's
/// trace stops waiting for one, and prints now if that leaves nothing else to wait for -- no later
/// dispatch of this turn is coming to print it if the turn has already ended.
fn trace_discarded_first_texts(state: &Rc<RefCell<AgentPanelState>>, discarded: Vec<crate::panel_pacer::FirstText>) {
    if discarded.is_empty() {
        return;
    }
    let mut state_ref = state.borrow_mut();
    for first in discarded {
        if let Some(trace) = state_ref.tabs.get_mut(first.tab).and_then(|t| t.turn_trace.as_mut()) {
            if trace.mark_first_text_in_snapshot_for(first.turn) && trace.is_complete() {
                trace.emit();
            }
        }
    }
}

/// What an extra page that shows no tab answers a command about a tab.
const NO_TAB_HERE: &str = "protocol: this page shows no tab";

/// What an extra page is told when it asks about the last window's tabs: only the base page offers them.
const RESTORE_IS_THE_BASE_PAGES: &str = "protocol: the last session's tabs are restored from the base view";

/// What an extra page is told when it asks to close the other tabs: it shows one.
const ONE_TAB_HERE: &str = "this view shows one tab: there are no others to close";

/// The one late mirror of `tab` from the page it was just moved out of (`TabSet::take_late_draft`,
/// already taken): it is newer than the copy the tab's new page was given, so the tab keeps it and that
/// page is offered it, marked late, so that a box edited there since keeps its own newer text.
fn deliver_late_draft(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages, tab: TabId, text: &str) {
    let shown = {
        let mut state = state.borrow_mut();
        state.tabs.set_draft(tab, text);
        state.shown_in(tab)
    };
    if let Some(shown) = shown {
        pages.send(shown, &eitri_core::agent_bridge::serialize_late_draft_for_js(tab, text));
    }
}

/// A message from `view`'s page after it stopped being live. A view emptied by its tab leaving is
/// retired on the loop's next turn, which can come before its document has delivered the mirror its
/// composer flushed as the keys went; that mirror is still the tab's newest text, so it is taken as the
/// late draft it is. Nothing else from such a page is acted on, and nothing is answered: its document
/// is being replaced. Whether the message was taken.
fn late_draft_from_idle_page(raw: &str, state: &Rc<RefCell<AgentPanelState>>, pages: &Pages, view: ViewId) -> bool {
    if view.is_base() || state.borrow().shutting_down {
        return false;
    }
    let Some(message) = parse_inbound_message(raw) else {
        return false;
    };
    if !matches!(message, InboundMessage::Draft { .. }) {
        return false;
    }
    let resolved = state.borrow().tabs.resolve(message.tab_ref());
    let (Ok(Some(tab)), InboundMessage::Draft { text, .. }) = (resolved, message) else {
        return false;
    };
    if !state.borrow_mut().tabs.take_late_draft(tab, view) {
        return false;
    }
    deliver_late_draft(state, pages, tab, &text);
    true
}

/// One message from `view`'s page (`page`). Every answer goes back to that page; what changes for a
/// tab goes to the page that shows it.
fn handle_inbound_message(
    raw: &str,
    state: &Rc<RefCell<AgentPanelState>>,
    pages: &Pages,
    view: ViewId,
    page: &PanelPage,
) {
    let Some(message) = parse_inbound_message(raw) else {
        // Already logged inside parse_inbound_message -- an unparseable/unrecognized message
        // from the WebView must never crash the shell process. No requestId is known for it, so
        // no command_result is possible (see parse_inbound_message's own doc).
        return;
    };
    // The window is closing and its tabs are gone; a document still posting has nothing to reach.
    if state.borrow().shutting_down {
        return;
    }
    let request_id = message.request_id().to_string();
    // A tab command names its tab, or it is refused. Never "the
    // active one" -- a permission answered just after a switch must reach the tab that asked.
    // Bound before the `match`: a scrutinee's temporaries live to the end of the whole match.
    let resolved = state.borrow().tabs.resolve(message.tab_ref());
    // Whether this is the one `draft` the page a tab just left may still post for it.
    let mut late_draft = false;
    let target = match resolved {
        // An extra page that shows no tab has no tab to command, but the tab that just left it may be
        // owed the mirror its composer flushed as the keys went.
        Ok(Some(tab)) if !view.is_base() && state.borrow().tabs.tab_in(view).is_none() => {
            late_draft =
                matches!(message, InboundMessage::Draft { .. }) && state.borrow_mut().tabs.take_late_draft(tab, view);
            if !late_draft {
                eprintln!("[agent_panel] refused {request_id}: {NO_TAB_HERE}");
                page.send(&serialize_command_result_for_js(&request_id, Err(NO_TAB_HERE)));
                return;
            }
            Some(tab)
        }
        Ok(target) => target,
        Err(protocol) => {
            eprintln!("[agent_panel] refused {request_id}: {protocol}");
            page.send(&serialize_command_result_for_js(&request_id, Err(&protocol)));
            return;
        }
    };
    let tab_of = |target: Option<TabId>| target.expect("a tab-scoped message resolved to a tab");
    let ok = |page: &PanelPage| page.send(&serialize_command_result_for_js(&request_id, Ok(())));
    let refuse = |page: &PanelPage, why: &str| {
        page.send(&serialize_command_result_for_js(&request_id, Err(why)));
    };

    match message {
        InboundMessage::Ready { .. } => {
            // A stream envelope the typing cadence still holds is already in the snapshot below
            // (it was drained from the queue and folded into the projection before it was held);
            // sent after that snapshot it would draw the same text twice.
            let discarded = page.pacer().borrow_mut().discard_pending();
            trace_discarded_first_texts(state, discarded);
            // Everything a fresh document is owed is decided by one pure function, so the reload
            // path is testable without a WebView -- see `ready_payloads`. A `ready` never opens the
            // chooser,  on the first one or on a reload -- the empty tab's dashboard is
            // what a fresh document sees. Only this page's tab is in it: another page's tab and its
            // cards are that page's.
            let payloads = {
                let mut state_ref = state.borrow_mut();
                let state_ref = &mut *state_ref;
                // The nav-mode mirror starts as `Other` and returns to it on every ready
                // (a reload). A fresh document starts BROWSE-or-dashboard, never whatever mode
                // the torn-down document last reported, so the mirror must not carry the old
                // value across -- see `reset_nav_mode_for_ready`'s own doc for the failure this
                // closes.
                reset_nav_mode_for_ready(state_ref, view);
                let agent = hello_agent(state_ref, view);
                let greeting = BackendGreeting::for_agent_titled(
                    state_ref.backend_kind,
                    &agent,
                    state_ref.project_dir.clone(),
                    eitri_core::agent_backend::Titles::Cached(&state_ref.titles),
                );
                let open = state_ref.tabs.open_session_ids();
                let theme = eitri_core::agent_bridge::serialize_theme_for_js(&state_ref.theme);
                let context = eitri_core::agent_bridge::context_summary(
                    (state_ref.editor_context)().as_ref(),
                    &state_ref.project_dir,
                );
                let context_payload = eitri_core::agent_bridge::serialize_editor_context_for_js(context.as_ref());
                state_ref.page_mut(view).last_context_payload = Some(context_payload.clone());
                let mut window = window_payloads(
                    eitri_core::agent_bridge::serialize_history_for_js(&state_ref.history),
                    context_payload,
                    state_ref.editor_link.as_ref(),
                );
                // How thinking is shown, before the tabs and the snapshot it applies to.
                window.insert(
                    0,
                    eitri_core::agent_bridge::serialize_display_for_js(state_ref.thinking_display),
                );
                // Whether the page may speak of a turn's changes, before the tabs it would speak of.
                window.insert(1, review_settings_payload(state_ref));
                let tabs = tabs_payload_recorded(state_ref, view);
                // The base page's active tab, as always; an extra page's one tab, or nothing for an
                // extra page that shows none yet.
                let active = if view.is_base() {
                    state_ref.tabs.active_state_payloads()
                } else {
                    match state_ref.tabs.tab_in(view) {
                        Some(tab) => state_ref.tabs.state_payloads(tab),
                        None => Vec::new(),
                    }
                };
                // What the last window left open, if the dashboard may offer it: a reloaded page
                // is told again, like everything else in the greeting. Only the base page offers it.
                let mut offered_greeting = greeting;
                offered_greeting
                    .resumable
                    .retain(|r| !open.contains(&r.provider_session_id));
                let offer = if view.is_base() { restore_offer(state_ref) } else { None };
                let mut payloads = ready_payloads(
                    offered_greeting,
                    &open,
                    window,
                    tabs,
                    active,
                    Some(&theme),
                    state_ref.keymap_help.as_deref(),
                    offer.as_ref(),
                );
                // The status-band hints the replaced document was showing, of this view's tabs.
                for hint in hints_for_new_document(state_ref, view) {
                    payloads.push(eitri_core::agent_bridge::serialize_review_hint_for_js(&hint));
                }
                // Last, so nothing the document draws from the payloads above can reset it.
                let focused = state_ref.page(view).is_some_and(|record| record.pane_focused);
                payloads.push(eitri_core::agent_bridge::serialize_pane_focus_for_js(focused));
                let offered = view.is_base() && restore_is_on_offer(state_ref);
                let record = state_ref.page_mut(view);
                record.last_open_ids = open;
                record.last_restore_offered = offered;
                record.last_hello_agent = agent;
                // From here on the tick may send into this document.
                record.document_ready = true;
                payloads
            };
            eprintln!("[agent_panel] view {}: ready", view.number());
            dispatch_all(page, payloads);
            // A mode note set before any page was ready (the configured agent clamping the empty
            // tab) is held back by `send_mode_note`; this is the first moment it can be said, after
            // the `tabs` that shows the tab. Taking it clears it, so a later `tabs` does not repeat it.
            send_mode_note(state, pages);
            ok(page);
            // The interrupted reverts, offered once the base page can show them (and again after a
            // reload). They are the window's, so only the base page offers them.
            if view.is_base() {
                let owed = {
                    let mut state_ref = state.borrow_mut();
                    let state_ref = &mut *state_ref;
                    state_ref.review_flow.document_ready(&state_ref.tabs)
                };
                dispatch_review_outs(state, pages, owed);
            }
            // A fresh document has no trust question on screen: the tab it shows, if that tab waits
            // for one, is asked again, and a fresh check gives `prefix i` its row. A `:trust` prompt
            // ends only when it was this page's.
            let asked = {
                let mut state_ref = state.borrow_mut();
                let state_ref = &mut *state_ref;
                if command_prompt_is_in(state_ref, view) {
                    state_ref.trust.drop_command_prompt();
                }
                match state_ref.tab_of_page(view) {
                    Some(tab) => {
                        state_ref.trust.hide(tab);
                        let profile = waiting_profile(state_ref, tab);
                        state_ref.trust.begin_check(&profile);
                        reroute_or_ask(state_ref, tab)
                    }
                    None => TrustOut::default(),
                }
            };
            deliver_trust(state, pages, asked);
            // `agent.restore = "auto"`: only once the base page can take what it starts.
            if view.is_base() {
                restore_at_launch(state, pages);
            }
        }
        // The last window's tabs are offered, and asked about, in the base page only.
        InboundMessage::RestoreLast { .. } | InboundMessage::RestoreAnswer { .. } if !view.is_base() => {
            refuse(page, RESTORE_IS_THE_BASE_PAGES);
        }
        InboundMessage::RestoreLast { .. } => {
            let tab = tab_of(target);
            let result = ask_for_restore(&mut state.borrow_mut(), tab, RestoreAsk::Dashboard);
            match deliver_restore(state, pages, result) {
                Ok(()) => ok(page),
                Err(why) => refuse(page, &why),
            }
        }
        InboundMessage::RestoreAnswer { nonce, keep_bypass, .. } => {
            // A `y` resumes the saved bypass tabs in bypass, so it meets the window's gate like any
            // answer that loosens permissions. Refused, the question is left as it was: the same
            // nonce can still be answered and the restore key asks it again. An `n` loosens nothing.
            let gate = state.borrow().answer_gate.clone();
            let gated = if keep_bypass {
                through_answer_gate(gate.as_ref(), view, || Ok(()))
            } else {
                Ok(())
            };
            match gated.and_then(|()| {
                let result = answer_restore_question(&mut state.borrow_mut(), nonce, keep_bypass);
                deliver_restore(state, pages, result)
            }) {
                Ok(()) => ok(page),
                Err(why) => refuse(page, &why),
            }
        }
        InboundMessage::SendMessage { text, .. } => {
            let tab = tab_of(target);
            // A sent prompt is history whether or not the backend takes it, as in Claude Code.
            remember_prompts(state, pages, std::slice::from_ref(&text));
            // ...and no longer the tab's draft (a refusal puts it back in the box panel-side).
            state.borrow_mut().tabs.note_sent(tab);
            enum Plan {
                Sent(Result<Vec<agent::AgentDomainEvent>, BackendError>),
                Defer(FirstTurn),
                Refused(&'static str),
            }
            let plan = {
                let mut state_ref = state.borrow_mut();
                let state_ref = &mut *state_ref;
                // wire 1's one composition point, read when Enter is pressed. Above both
                // backends on purpose: the turn's String travels unmodified from here to
                // `send_turn` on either path. A `None` context means the turn goes out exactly as
                // the user typed it.
                let composed =
                    eitri_core::editor_context::compose_turn_text(&text, (state_ref.editor_context)().as_ref());
                let t = state_ref.tabs.get_mut(tab).expect("resolved above");
                if t.pending_handoff.is_some() {
                    // Starting here would spawn a second `claude` alongside the one still being
                    // closed, in the same project.
                    Plan::Refused("the previous session is still being handed off to a terminal")
                } else {
                    let mode = t.mode();
                    match &mut t.backend {
                        TabBackend::Live(backend) => {
                            // Stamped before the call, so the trace's zero is the user's action
                            // rather than the moment the backend got around to accepting it.
                            let trace = eitri_core::turn_trace::TurnTrace::start();
                            // `&text` second, and it is the user's own: the panel shows what was
                            // typed, never the composed wire text. The tab's mode goes with it: an
                            // external agent's turn runs under it.
                            let outcome = backend.send_turn_in(&composed, &text, mode);
                            t.turn_trace = trace;
                            Plan::Sent(outcome)
                        }
                        // The session waits for the trust check (and, when it must, the question)
                        // before anything is spawned.
                        TabBackend::NotStarted => Plan::Defer(FirstTurn {
                            wire: composed,
                            typed: text.clone(),
                        }),
                        TabBackend::Starting(_) => Plan::Refused("the session is still starting"),
                        TabBackend::AwaitingTrust(_) => Plan::Refused("waiting for the trust question"),
                        TabBackend::Failed { .. } => Plan::Refused("this tab's session failed; press r to start over"),
                    }
                }
            };
            match plan {
                Plan::Sent(outcome) => {
                    if outcome.is_ok() {
                        title_from_first_prompt(state, tab, &text);
                    }
                    apply_command_outcome(state, pages, view, tab, &request_id, outcome);
                }
                // The `command_result` is owed once the connect finishes and the first turn is
                // sent (`collect_pending_starts`), or when the question is put off.
                Plan::Defer(first_turn) => {
                    let start = DeferredStart::FirstSend {
                        request_id: request_id.clone(),
                        first_turn,
                    };
                    // Bound before the `match`: the borrow would otherwise stay held through the
                    // arms, and `send_tabs` borrows the state again, which panics inside a WebKit
                    // signal handler and aborts the window.
                    let started = gate_start(&mut state.borrow_mut(), tab, start);
                    match started {
                        Ok(()) => {
                            note_start_page(&mut state.borrow_mut(), tab, &request_id, view);
                            send_tabs(state, pages)
                        }
                        Err(why) => refuse(page, &why),
                    }
                }
                Plan::Refused(why) => refuse(page, why),
            }
        }
        InboundMessage::Resume {
            provider_session_id, ..
        } => {
            let tab = tab_of(target);
            if state
                .borrow()
                .tabs
                .get(tab)
                .is_some_and(|t| t.pending_handoff.is_some())
            {
                refuse(page, "the previous session is still being handed off to a terminal");
                return;
            }
            let canonical = state.borrow().canonical_project_dir.clone();
            // The session is the tab's agent's: its lease is under that agent's name.
            let agent = state
                .borrow()
                .tabs
                .get(tab)
                .map_or(AgentId::Claude, |t| t.agent().clone());
            let lease_name = agent.record_name();
            let held =
                agent::lease::SessionLease::is_held(&lease_name, &canonical, &provider_session_id).unwrap_or(false);
            let route = state
                .borrow_mut()
                .tabs
                .route_resume(tab, &agent, &provider_session_id, held);
            match route {
                // Already open: a bound tab is never selected, its view is shown instead; a base-view
                // tab is the base view's now, and the base view is shown if the pick was elsewhere.
                ResumeRoute::SwitchTo(open) => {
                    let open_in = state.borrow().tabs.view_of(open);
                    if open_in.is_base() {
                        send_switch(state, pages);
                    }
                    if open_in != view {
                        show_view(state, open_in);
                    }
                    ok(page);
                }
                ResumeRoute::Refuse(why) => refuse(page, &why),
                ResumeRoute::StartIn(target_tab) => {
                    let deferred = resume_in(
                        &mut state.borrow_mut(),
                        target_tab,
                        &request_id,
                        provider_session_id,
                        true,
                    );
                    // An extra page's empty tab takes the resume itself; any other opens in a new
                    // base-view tab, and the base view takes the keys.
                    let started_in = state.borrow().tabs.view_of(target_tab);
                    match deferred {
                        // The command_result is owed once the connect finishes, or when the question
                        // is put off.
                        Ok(()) => {
                            note_start_page(&mut state.borrow_mut(), target_tab, &request_id, view);
                            send_view(state, pages, started_in);
                            if started_in != view {
                                show_view(state, started_in);
                            }
                        }
                        Err(why) => refuse(page, &why),
                    }
                }
            }
        }
        InboundMessage::Interrupt { .. } => {
            let tab = tab_of(target);
            let outcome = backend_for(&mut state.borrow_mut().tabs, tab).and_then(AgentBackend::interrupt);
            apply_command_outcome(state, pages, view, tab, &request_id, outcome);
        }
        InboundMessage::StopBackground { running, .. } => {
            // The tab the question was asked in, never the one on screen now. A refusal is benign and
            // leaves the session alone; a taken stop ends the session through the pump, as a lost one.
            let tab = tab_of(target);
            let outcome = state.borrow_mut().tabs.stop_background(tab, running);
            apply_command_outcome(state, pages, view, tab, &request_id, outcome);
        }
        message @ InboundMessage::PermissionResponse { .. } => {
            let tab = tab_of(target);
            let answer = PermissionAnswer::of(message).expect("matched as a permission_response above");
            let (rules_dir, project_dir, gate) = {
                let s = state.borrow();
                (s.rules_dir.clone(), s.project_dir.clone(), s.answer_gate.clone())
            };
            // `Ctrl+y`, BROWSE's answer keys and a card's own buttons all arrive here, so one gate
            // covers every way a card is answered. Another page's card is refused before the window
            // is asked; `answer_card` checks the page again, so a later change here cannot skip it.
            let shown_here = page_may_answer(&state.borrow().tabs, tab, view);
            let answered = shown_here.and_then(|()| {
                through_answer_gate(gate.as_ref(), view, || {
                    answer_permission_response(
                        &mut state.borrow_mut().tabs,
                        rules_dir.as_deref(),
                        &project_dir,
                        tab,
                        view,
                        answer,
                    )
                })
            });
            match answered {
                Err(why) => refuse(page, &why),
                Ok(outcome) => apply_command_outcome(state, pages, view, tab, &request_id, outcome),
            }
        }
        InboundMessage::HintRequest { .. } => {
            // Cloned out of the borrow before calling: the hook calls back into this handle,
            // which borrows `state` again -- calling it while the borrow above is still held
            // would panic with a double borrow (`HintCoordinator::toggle`/`on_panel` both do).
            let hook = state.borrow().hint_hook.clone();
            if let Some(hook) = hook {
                hook(HintInbound::Request);
            }
        }
        InboundMessage::HintTargets { session_id, count, .. } => {
            let hook = state.borrow().hint_hook.clone();
            if let Some(hook) = hook {
                hook(HintInbound::Targets { session_id, count });
            }
        }
        InboundMessage::TurnRendered {
            receive_to_frame_ms, ..
        } => {
            // Purely a diagnostic: no command_result, and nothing downstream reads it. A WebView
            // that never sends one costs only a missing column in a trace line.
            let tab = tab_of(target);
            // A flush between ticks (a focus change, a tab switch) may have released the first
            // text after the last tick: its stamp is applied first, or this report -- which the
            // page sends only once -- would find no dispatch to measure from and be dropped.
            let stamps = page
                .pacer()
                .try_borrow_mut()
                .map(|mut pacer| pacer.take_first_text())
                .unwrap_or_default();
            stamp_first_text_dispatches(state, stamps);
            let mut state_ref = state.borrow_mut();
            if let Some(trace) = state_ref.tabs.get_mut(tab).and_then(|t| t.turn_trace.as_mut()) {
                trace.mark_painted(receive_to_frame_ms);
                if trace.is_complete() {
                    trace.emit();
                }
            }
        }
        InboundMessage::HandoffToTerminal { .. } => {
            let tab = tab_of(target);
            // The command is built from canonical state FIRST, while the session is still readable
            // -- ownership moves to the shutdown worker below and nothing here can read it after
            // that. Owned values rather than borrows because `prepare_handoff` is a pure function
            // and the `RefCell` borrow must not outlive this block.
            let (project_dir, session_facts, already_handing_off) = {
                let state_ref = state.borrow();
                let t = state_ref.tabs.get(tab).expect("resolved above");
                let facts = t.live().map(|backend| {
                    // `provider_session_id()` BEFORE `projection()`, and the order is load-bearing
                    // rather than stylistic. On the sidecar path both reach for the SAME
                    // `Mutex<IngestState>` -- `projection()` returns a guard that holds it, and
                    // `provider_session_id()` locks it again. `std::sync::Mutex` is not reentrant,
                    // so calling the second inside the first's scope deadlocks the GTK main loop
                    // outright: no clicks, no keys, not even a compositor close request, with the
                    // last frame still painted so it looks alive. Reproduced 3/3 on 2026-09-15 and
                    // confirmed by backtrace.
                    let provider_session_id = backend.provider_session_id();
                    let projection = backend.projection();
                    (
                        provider_session_id,
                        projection.active_turn_id.clone(),
                        projection.cwd.clone(),
                        crate::terminal_handoff::ConversationLiveness::of(&projection.status),
                    )
                });
                (state_ref.project_dir.clone(), facts, t.pending_handoff.is_some())
            };
            if already_handing_off {
                refuse(page, "this conversation is already being handed off");
                return;
            }
            let prepared = crate::terminal_handoff::prepare_handoff(
                &project_dir,
                session_facts
                    .as_ref()
                    .map(|(provider_session_id, active_turn_id, reported_cwd, liveness)| {
                        crate::terminal_handoff::HandoffFacts {
                            provider_session_id: provider_session_id.as_deref(),
                            active_turn_id: active_turn_id.as_deref(),
                            reported_cwd: reported_cwd.as_deref(),
                            liveness: *liveness,
                        }
                    }),
            );
            let command = match prepared {
                Ok(command) => command,
                Err(refusal) => {
                    // The frontend disables the control for exactly these states, so reaching here
                    // means its view was stale (a turn started between the render and the click).
                    // The session is untouched.
                    refuse(page, &refusal.message());
                    return;
                }
            };

            // The handoff's steps, in the only order that is safe: from here on this tab
            // holds no session (`NotStarted` at once), so no further turn can be sent,
            // and `shutdown()` itself cancels every pending permission (fail-closed) and closes the
            // provider's session.
            let taken = {
                let mut state_ref = state.borrow_mut();
                // The tab becomes `NotStarted` under whatever bypass
                // prompt was open for it -- a live tab's `Switch to bypass?` whose `y` would then also
                // have moved the window default. Reached by clicks that never pass
                // through the panel's y/n, so it is dropped here; `confirm_bypass`'s own re-check of
                // which question was asked is the other half. Only this page's: the click was here.
                state_ref.tabs.drop_bypass_prompt_in(view);
                let t = state_ref.tabs.get_mut(tab).expect("resolved above");
                match std::mem::replace(&mut t.backend, TabBackend::NotStarted) {
                    TabBackend::Live(backend) => Some(backend),
                    other => {
                        t.backend = other;
                        None
                    }
                }
            };
            let Some(mut backend) = taken else {
                // `prepare_handoff` already returned `NoSession` for this, so it is unreachable
                // unless something took the session between that call and this line -- both run on
                // the GTK main loop, so nothing can. Reported rather than unwrapped anyway.
                refuse(page, "the conversation ended before it could be handed off");
                return;
            };
            let (closed_tx, closed_rx) = mpsc::channel();
            let (dropped_tx, dropped_rx) = mpsc::channel();
            std::thread::spawn(move || {
                backend.shutdown();
                // Sent after `shutdown()` returns, which is the whole signal: the command is not
                // dispatched until this arrives. A panic inside `shutdown()` drops the sender
                // instead, which `collect_pending_handoffs` reads as a failed close.
                let _ = closed_tx.send(());
                // Then the drop, which is a separate and later event: `shutdown()` issues
                // `close_session` and joins ingestion, while it is `SpawnedSidecar::drop` that ends
                // the child. The command is deliberately NOT made to wait for it, but a window
                // closing at this moment is, so it gets its own signal.
                drop(backend);
                let _ = dropped_tx.send(());
            });
            {
                let mut state_ref = state.borrow_mut();
                if let Some(t) = state_ref.tabs.get_mut(tab) {
                    t.pending_handoff = Some(PendingHandoff {
                        request_id,
                        command,
                        closed_rx,
                        dropped_rx,
                        page: view,
                    });
                }
            }
            // No command_result yet -- `collect_pending_handoffs` owes it once the close finishes.
            send_tabs(state, pages);
        }
        // A pick in the chooser, which lists every tab: a base-view tab is selected there, and the
        // base view shown if the pick was in another page; a tab bound to an extra view is never
        // selected, its view is shown instead.
        InboundMessage::SelectTab { .. } => {
            let tab = tab_of(target);
            let place = state.borrow_mut().tabs.select_place(tab);
            match place {
                Some(TabPlace::View(shown_in, _)) => {
                    if shown_in != view {
                        show_view(state, shown_in);
                    }
                }
                _ => {
                    send_switch(state, pages);
                    if !view.is_base() {
                        show_view(state, ViewId::BASE);
                    }
                }
            }
            ok(page);
        }
        InboundMessage::RenameTab { name, .. } => {
            let tab = tab_of(target);
            state.borrow_mut().tabs.rename(tab, &name);
            send_tabs(state, pages);
            ok(page);
        }
        InboundMessage::CloseTab { .. } => {
            let tab = tab_of(target);
            match close_tab(state, pages, tab) {
                Ok(()) => ok(page),
                Err(why) => refuse(page, why),
            }
        }
        InboundMessage::ResetTab { .. } => {
            let tab = tab_of(target);
            let before = state.borrow().tabs.attention();
            // A start put off for the trust question owes its command an answer; read before the
            // reset drops it.
            let owed = state.borrow().tabs.get(tab).and_then(|t| match &t.backend {
                TabBackend::AwaitingTrust(waiting) => waiting.start.request_id().map(str::to_owned),
                _ => None,
            });
            // A session its agent stopped with the conversation whole is continued in the same tab, not
            // left for a second `r` on the start screen.
            let continued = state.borrow().tabs.resumes_on_r(tab);
            let reset = state.borrow_mut().tabs.reset(tab);
            match reset {
                Ok(old) => {
                    {
                        let mut state_ref = state.borrow_mut();
                        // A wait an earlier `r` left for this tab is over: its start did not happen.
                        state_ref.resume_after.remove(&tab);
                        if let Some(old) = old {
                            // `TabSet::reset` counted it as retiring.
                            if continued.is_some() {
                                let gone = state_ref.retiring.backend_watched(old, Some(tab));
                                let id = continued.clone().unwrap_or_default();
                                state_ref.resume_after.insert(tab, (id, gone));
                            } else {
                                state_ref.retiring.backend(old, Some(tab));
                            }
                        }
                        state_ref.trust.forget_tab(tab);
                    }
                    // The put-off start's command is answered in the page it was sent from.
                    let owed = owed.map(|owed| {
                        let asked_from = start_answer_dest(&mut state.borrow_mut(), tab, &owed);
                        let asked_from = match asked_from {
                            Dest::Page(asked_from) => asked_from,
                            Dest::Tab(_) => view,
                        };
                        (owed, asked_from)
                    });
                    // No lease check first: the tab's own old backend holds it until its teardown, which the
                    // start waits for. The command is answered when the connect finishes. The tab is selected
                    // only if it was on screen: a reset never moves the window to another tab.
                    let resumed = continued.map(|provider_session_id| {
                        let on_screen = state.borrow().shown_in(tab).is_some();
                        resume_in(
                            &mut state.borrow_mut(),
                            tab,
                            &request_id,
                            provider_session_id,
                            on_screen,
                        )
                    });
                    match &resumed {
                        Some(Ok(())) => note_start_page(&mut state.borrow_mut(), tab, &request_id, view),
                        Some(Err(_)) => {
                            state.borrow_mut().resume_after.remove(&tab);
                        }
                        None => {}
                    }
                    // Before the draft the reset carries: the page puts a refused message back in
                    // its box itself, and the draft that follows then only confirms it.
                    if let Some((owed, asked_from)) = owed {
                        pages.send(
                            asked_from,
                            &serialize_command_result_for_js(&owed, Err(crate::trust_gate::NOT_STARTED)),
                        );
                    }
                    // `tabs` and the (empty) tab's own state, in the page that shows it.
                    let (shown_in, payloads) = shown_payloads(&mut state.borrow_mut(), tab);
                    pages.send_all(shown_in, payloads);
                    send_hello_if_open_sessions_changed(state, pages);
                    report_attention(state, before);
                    match resumed {
                        None => ok(page),
                        Some(Ok(())) => {}
                        Some(Err(why)) => refuse(page, &why),
                    }
                }
                Err(why) => refuse(page, &why),
            }
        }
        // `TabSet::cycle_mode` returns a `Result<ModeCycle, String>` -- a live,
        // active tab can toggle too, not only an empty one, so `Ok(Changed(_))` here no longer
        // means "was empty". `was_empty` is read BEFORE the call so the persisted "next launch"
        // default is written only for the tab whose move that preference is actually
        // about. `Confirm(plan)` dispatches the real y/n prompt rather than refusing --
        // `apply_confirm_bypass`, below, is what a later `confirm_bypass` message answers it with.
        InboundMessage::CycleMode { .. } => {
            let tab = tab_of(target);
            // Bound as its own statement, not the match scrutinee: edition 2021 keeps a match
            // scrutinee's temporaries alive for the whole match, and the `Changed` arm below calls
            // `send_tabs`, which needs `state.borrow_mut()`.
            let outcome = cycle_tab_mode(state, tab, view);
            match outcome {
                Ok(eitri_core::tab_set::ModeCycle::Changed(_)) => {
                    send_tabs(state, pages);
                    ok(page);
                }
                Ok(eitri_core::tab_set::ModeCycle::Confirm(plan)) => {
                    page.send(&eitri_core::agent_bridge::serialize_confirm_bypass_for_js(&plan));
                    ok(page);
                }
                Err(why) => refuse(page, &why),
            }
        }
        InboundMessage::OpenDetail { .. } => {
            let tab = tab_of(target);
            // A route away from this page's bypass prompt (the panel cancels its own on `tab_detail`).
            state.borrow_mut().tabs.drop_bypass_prompt_in(view);
            let payload = detail_payload(&state.borrow(), tab);
            page.send(&payload);
            ok(page);
        }
        InboundMessage::QueueMessage { text, turn_id, .. } => {
            let tab = tab_of(target);
            let queued = {
                let mut state_ref = state.borrow_mut();
                let state_ref = &mut *state_ref;
                // Captured now: what the editor shows when Enter is pressed.
                let wire = eitri_core::editor_context::compose_turn_text(&text, (state_ref.editor_context)().as_ref());
                state_ref.tabs.queue_message(tab, &text, wire, now_ms(), true)
            };
            match queued {
                Ok(_) => {
                    remember_prompts(state, pages, std::slice::from_ref(&text));
                    // The turn may have ended between the panel's Enter and this handler.
                    let flush = state.borrow_mut().tabs.flush_queue(tab);
                    match flush {
                        Some(flush) => apply_flush(state, pages, tab, flush),
                        None => {
                            // Into the running turn when its agent can take it; held otherwise. Never
                            // waits on the agent: the outcome arrives through the pump.
                            state.borrow_mut().tabs.steer_held(tab, now_ms(), turn_id.as_deref());
                            send_queue(state, pages, tab);
                        }
                    }
                    ok(page);
                }
                Err(why) => refuse(page, &why),
            }
        }
        InboundMessage::TakeBackQueue { .. } => {
            let tab = tab_of(target);
            let texts = state.borrow_mut().tabs.take_back_queue(tab);
            page.send(&eitri_core::agent_bridge::serialize_queue_taken_for_js(tab, &texts));
            send_queue(state, pages, tab);
            ok(page);
        }
        InboundMessage::SendNow { text, .. } => {
            let tab = tab_of(target);
            let result = {
                let mut state_ref = state.borrow_mut();
                let state_ref = &mut *state_ref;
                let wire = eitri_core::editor_context::compose_turn_text(&text, (state_ref.editor_context)().as_ref());
                state_ref.tabs.send_now(tab, &text, wire, now_ms())
            };
            if !text.trim().is_empty() {
                remember_prompts(state, pages, std::slice::from_ref(&text));
            }
            match result {
                Err(why) => refuse(page, &why),
                Ok(eitri_core::tab_set::SendNow::Interrupting(outcome)) => {
                    send_queue(state, pages, tab);
                    apply_command_outcome(state, pages, view, tab, &request_id, outcome);
                }
                Ok(eitri_core::tab_set::SendNow::Flushed(flush)) => {
                    if let Some(flush) = flush {
                        apply_flush(state, pages, tab, flush);
                    }
                    ok(page);
                }
            }
        }
        InboundMessage::Draft { text, .. } => {
            // A mirror, answered with nothing, from the page whose view holds the tab (a base-view tab
            // in the background included: the base page flushes as it switches away from it).
            let tab = tab_of(target);
            let mut state_ref = state.borrow_mut();
            let held_here = page_may_answer(&state_ref.tabs, tab, view);
            if held_here.is_ok() {
                state_ref.tabs.set_draft(tab, &text);
            } else if late_draft || state_ref.tabs.take_late_draft(tab, view) {
                drop(state_ref);
                deliver_late_draft(state, pages, tab, &text);
            } else if let Err(why) = held_here {
                drop(state_ref);
                eprintln!("[agent_panel] refused {request_id}: {why}");
                refuse(page, &why);
            }
        }
        InboundMessage::HistoryPush { text, .. } => {
            remember_prompts(state, pages, std::slice::from_ref(&text));
            ok(page);
        }
        InboundMessage::OpenPath { path, line, .. } => {
            let project_dir = state.borrow().project_dir.clone();
            let resolved = match open_path_target(&project_dir, &path) {
                Ok(resolved) => resolved,
                Err(why) => return refuse(page, &why),
            };
            let request = eitri_core::scratch::open_request(&resolved, line);
            match request_editor(state, &request) {
                Ok(()) => ok(page),
                Err(why) => refuse(page, &why),
            }
        }
        // `c` in BROWSE: the jobs are built here, from this tab's own session and edit calls, and run
        // on a worker; the answer is an envelope carrying the request id, sent by the tick.
        InboundMessage::ReviewRequest { turn, scope, .. } => {
            let tab = tab_of(target);
            let (job, draft) = {
                let state_ref = state.borrow();
                // Memory only: the draft is cloned here, on the GTK thread, and moves to the worker.
                (
                    state_ref.tabs.review_overview_job(tab, turn.into(), scope.into()),
                    state_ref.tabs.review_draft(tab).cloned(),
                )
            };
            let started = job.and_then(|job| {
                let draft = draft?;
                let id = request_id.clone();
                start_review_job(state, view, &request_id, Some((tab.0, turn)), move || {
                    job.run()
                        .map(|overview| eitri_core::agent_bridge::serialize_review_for_js(&id, tab, &overview, &draft))
                        .map_err(|why| why.to_string())
                })
            });
            if let Err(why) = started {
                refuse(page, &why);
            }
        }
        // The panel's revert, undo, comment, send and recovery messages belong to the review flow,
        // which checks, asks the editor and does the writing without ever waiting here. Its answers
        // come back now or on a later tick.
        InboundMessage::ReviewRevert { .. }
        | InboundMessage::ReviewUndo { .. }
        | InboundMessage::ReviewCommentAdd { .. }
        | InboundMessage::ReviewCommentRemove { .. }
        | InboundMessage::ReviewSend { .. }
        | InboundMessage::ReviewRecover { .. }
        | InboundMessage::OpenInEditor { .. } => run_review_flow(state, pages, view, message),
        InboundMessage::ReviewDiffRequest { turn, scope, path, .. } => {
            let tab = tab_of(target);
            let job = state.borrow().tabs.review_diff_job(tab, turn, scope.into(), &path);
            let started = job.and_then(|job| {
                let id = request_id.clone();
                start_review_job(state, view, &request_id, None, move || {
                    job.run()
                        .map(|diff| eitri_core::agent_bridge::serialize_review_diff_for_js(&id, tab, &diff))
                        .map_err(|why| why.to_string())
                })
            });
            match started {
                Ok(()) => {
                    // A file the editor draws moves to this turn with the panel.
                    let mut state_ref = state.borrow_mut();
                    let state_ref = &mut *state_ref;
                    state_ref.review_editor.sync(&mut state_ref.review_flow);
                    state_ref
                        .review_flow
                        .panel_shows(&state_ref.tabs, tab, turn, scope.into(), &path);
                }
                Err(why) => refuse(page, &why),
            }
        }
        InboundMessage::ViewInEditor { title, text, .. } => {
            let prepared = state
                .borrow_mut()
                .scratch
                .as_mut()
                .map(|dir| {
                    dir.prepare_view(&title, &text)
                        .map_err(|e| format!("could not write the scratch file: {e}"))
                })
                .unwrap_or_else(|| Err("the scratch directory could not be created at startup".to_string()));
            match prepared.and_then(|request| request_editor(state, &request)) {
                Ok(()) => ok(page),
                Err(why) => refuse(page, &why),
            }
        }
        InboundMessage::EditDraft { text, .. } => {
            let tab = tab_of(target);
            let prepared = state
                .borrow_mut()
                .scratch
                .as_mut()
                .map(|dir| {
                    dir.prepare_edit(&text)
                        .map_err(|e| format!("could not write the scratch file: {e}"))
                })
                .unwrap_or_else(|| Err("the scratch directory could not be created at startup".to_string()));
            let (request, edit) = match prepared {
                Ok(pair) => pair,
                Err(why) => return refuse(page, &why),
            };
            let begun = state.borrow_mut().tabs.begin_scratch_edit(tab, edit.id);
            if let Err(why) = begun {
                edit.cleanup();
                return refuse(page, &why);
            }
            if let Err(why) = request_editor(state, &request) {
                let mut state_ref = state.borrow_mut();
                state_ref
                    .tabs
                    .finish_scratch_edit(edit.id, &eitri_core::scratch::EditDone::Discarded);
                edit.cleanup();
                drop(state_ref);
                return refuse(page, &why);
            }
            state.borrow_mut().tabs.set_draft(tab, &text);
            state.borrow_mut().pending_edits.push(edit);
            page.send(&eitri_core::agent_bridge::serialize_scratch_for_js(tab, true));
            ok(page);
        }
        // `<leader>bd`: quiet unless `close_needs_confirm`, about the tab this page shows; `prefix &`
        // never comes through here.
        InboundMessage::TabVerb {
            verb: TabVerbWire::Close,
            ..
        } if {
            let state_ref = state.borrow();
            state_ref
                .tab_of_page(view)
                .is_some_and(|tab| !state_ref.tabs.close_needs_confirm(tab))
        } =>
        {
            let shown = state.borrow().tab_of_page(view).expect("checked in the guard");
            match close_tab(state, pages, shown) {
                Ok(()) => ok(page),
                Err(why) => refuse(page, why),
            }
        }
        // `:tabnew <agent>`: the page names the agent, so the hook the prefix's `c` goes through (which
        // carries none) is not used. Keys are in the panel already, since the word was typed there.
        InboundMessage::TabVerb {
            verb: TabVerbWire::New,
            agent: Some(word),
            ..
        } => match open_tab_named(state, view, &word) {
            Ok(()) => {
                send_tabs(state, pages);
                // The new tab is the base view's: typed in an extra page, the base view is shown and
                // takes the keys first.
                if !view.is_base() {
                    show_view(state, ViewId::BASE);
                }
                pages.send(ViewId::BASE, &eitri_core::agent_bridge::serialize_enter_input_for_js());
                ok(page);
            }
            Err(why) => refuse(page, &why),
        },
        InboundMessage::TabVerb { verb, .. } => match run_tab_verb(state, view, verb) {
            Ok(()) => ok(page),
            Err(why) => refuse(page, why),
        },
        InboundMessage::CycleDefaultMode { .. } => {
            // `Shift+Tab` on the chooser's `New session` row or a record: the window's
            // remembered default, exactly as `CycleMode` remembers a tab's own mode above. `Confirm`
            // dispatches the same y/n prompt, answered by a later `confirm_bypass` message.
            match cycle_default_mode(state, view) {
                eitri_core::tab_set::ModeCycle::Changed(_) => {
                    send_tabs(state, pages);
                    ok(page);
                }
                eitri_core::tab_set::ModeCycle::Confirm(plan) => {
                    page.send(&eitri_core::agent_bridge::serialize_confirm_bypass_for_js(&plan));
                    ok(page);
                }
            }
        }
        // An extra view shows one tab: there are no others to close from its page.
        InboundMessage::CloseOthers { .. } if !view.is_base() => refuse(page, ONE_TAB_HERE),
        InboundMessage::CloseOthers { .. } => {
            // `y` to `confirm_close_others`: recomputed fresh rather than
            // trusting the set the prompt was shown with, then `close_tab` -- the same close path
            // `prefix &`'s own `y` runs -- once per tab. Only the base view's tabs: another view's
            // tab is not one of the strip's others.
            let ids = state
                .borrow()
                .tabs
                .close_others_plan()
                .map_or_else(Vec::new, |(ids, _)| ids);
            let mut first_err = None;
            for id in ids {
                if let Err(why) = close_tab(state, pages, id) {
                    first_err.get_or_insert(why);
                }
            }
            match first_err {
                None => ok(page),
                Some(why) => refuse(page, why),
            }
        }
        // The nav-mode mirror: recorded so `main.rs`'s intercept can read it back through
        // `AgentPanelHandle::panel_keys`. Never refused -- an unrecognised `mode` is a parse
        // failure in `parse_inbound_message`, not something that reaches here.
        InboundMessage::PanelKeys { mode, .. } => {
            {
                let state_ref = state.borrow();
                if let Some(record) = state_ref.page(view) {
                    record.nav_mode.set(mode);
                }
            }
            ok(page);
        }
        InboundMessage::NavFallthrough { direction, .. } => match run_nav_fallthrough(state, view, direction) {
            Ok(()) => ok(page),
            Err(why) => refuse(page, why),
        },
        // `Ctrl+w h/j/k/l` in the panel's BROWSE -- the move `Ctrl+h/j/k/l` make.
        InboundMessage::PaneNav { direction, .. } => match run_pane_nav(state, view, direction) {
            Ok(()) => ok(page),
            Err(why) => refuse(page, why),
        },
        // The `?` overlay `prefix ?` opened was closed. With no hook (the companion window, `eitri-mac`) there is
        // nothing to give back, and the message is accepted all the same.
        InboundMessage::KeymapClosed {
            opening, return_keys, ..
        } => {
            run_keymap_closed(state, opening, return_keys);
            ok(page);
        }
        // `gx` -- a web link out to the system browser. The page's own check
        // (`nav.ts#webUrl`) is not trusted with what reaches the launcher: `web_url` decides again, and a
        // refusal means a page/Rust mismatch or a page that should not be sending this, so it is logged
        // (the address through `{:?}`, which escapes anything a terminal would act on).
        InboundMessage::OpenUrl { url, .. } => match web_url(&url) {
            Some(url) => {
                // Cloned out of the borrow first: the host runs arbitrary code.
                let host = state.borrow().host.clone();
                host.open_external(url);
                ok(page);
            }
            None => {
                eprintln!("[agent_panel] open_url refused: {url:?}");
                refuse(page, "not a web link");
            }
        },
        // `y`/`n` to a trust question. Only checked and queued here: no disk is touched, and a `y`
        // starts its session from the tick, once the worker found the disk unchanged (and wrote the
        // record, when one is kept).
        InboundMessage::TrustAnswer {
            nonce,
            fingerprint,
            findings_digest,
            trust,
            ..
        } => {
            let tab = tab_of(target);
            // Only an answer this page may give meets the window's gate, asked with the state
            // released; an `n` too, since a covered page's question may not be the one the user read.
            let asked = trust_question_answered(&state.borrow(), tab, nonce, view);
            let gate = state.borrow().answer_gate.clone();
            let refused = match (asked, gate) {
                (Some(_), Some(gate)) => gate(view).err(),
                _ => None,
            };
            let out = match (asked, refused) {
                (Some(asked), Some(why)) => {
                    refuse_trust_answer(&mut state.borrow_mut(), &request_id, tab, nonce, asked, &why, view)
                }
                _ => answer_trust(
                    &mut state.borrow_mut(),
                    &request_id,
                    tab,
                    nonce,
                    &fingerprint,
                    &findings_digest,
                    trust,
                    view,
                ),
            };
            deliver_trust(state, pages, out);
        }
        InboundMessage::TrustCancel { nonce, .. } => {
            let tab = tab_of(target);
            let out = cancel_trust_question(&mut state.borrow_mut(), &request_id, tab, nonce, view);
            deliver_trust(state, pages, out);
        }
        // `:trust` / `:untrust`: queued for the gate's worker; the notice and this command's
        // `command_result` come from the tick when it is done.
        InboundMessage::TrustCommand { action, agent, .. } => {
            let refused = trust_command(&mut state.borrow_mut(), view, &request_id, action, &agent);
            if let Err(why) = refused {
                refuse(page, &why);
            }
        }
        // `y`/`Y` to the band's bypass prompt. The typed-input guard (the 250ms
        // `TYPING_GUARD_MS` rule, cancelling on every route away) is enforced client-side
        // -- this only re-validates the facts (the nonce, the active tab, exactly
        // the delivered cards still pending) via `apply_confirm_bypass`/`TabSet::confirm_bypass`,
        // which is where all of that is actually checked.
        //
        // A tab's `y` approves every card still waiting in it, and the prompt itself may be what a
        // float hides, so it meets the window's card-answer gate first, with the state released, as a
        // card answer does: refused from a covered chat, nothing changes, and Shift+Tab asks again
        // (the page has closed the prompt already). A tab's prompt is answered only from the page
        // that shows the tab, checked before the window is asked.
        InboundMessage::ConfirmBypass { tab, scope, nonce, .. } => {
            let gate = state.borrow().answer_gate.clone();
            let checked = scope.scope(tab).and_then(|scope| match scope {
                eitri_core::tab_set::BypassScope::Tab(tab) => {
                    page_may_answer(&state.borrow().tabs, tab, view).map(|()| scope)
                }
                eitri_core::tab_set::BypassScope::Default => Ok(scope),
            });
            match checked.and_then(|scope| through_answer_gate(gate.as_ref(), view, || Ok(scope))) {
                Err(why) => refuse(page, &why),
                Ok(scope) => {
                    let before = state.borrow().tabs.attention();
                    match apply_confirm_bypass(state, scope, nonce, view) {
                        Ok(eitri_core::tab_set::ConfirmOutcome::Entered { resolved, .. }) => {
                            // On legacy the approved cards' resolutions exist
                            // only here (its CLI reports nothing for a hook reply), so without this the
                            // cards stayed drawn and counted as waiting until the next snapshot. A tab
                            // scope is always a tab this page shows (`confirm_bypass` refuses any
                            // other); the window-default scope approves nothing, so `resolved` is
                            // empty there.
                            let payload = match scope {
                                eitri_core::tab_set::BypassScope::Tab(tab) => {
                                    returned_events_payload(&state.borrow(), tab, &resolved)
                                }
                                eitri_core::tab_set::BypassScope::Default => None,
                            };
                            if let Some((shown_in, payload)) = payload {
                                pages.send(shown_in, &payload);
                            }
                            send_tabs(state, pages);
                            // The cards `confirm_bypass` just approved leave the tray at once, rather
                            // than waiting for a later poll to notice `attention()` dropped.
                            report_attention(state, before);
                            ok(page);
                        }
                        Ok(eitri_core::tab_set::ConfirmOutcome::AlreadyBypass) => {
                            send_tabs(state, pages);
                            ok(page);
                        }
                        Ok(eitri_core::tab_set::ConfirmOutcome::Reprompt(plan)) => {
                            // A card arrived while the prompt was up -- nothing changed, and the
                            // panel is shown the fresh count under a fresh nonce instead.
                            page.send(&eitri_core::agent_bridge::serialize_confirm_bypass_for_js(&plan));
                            ok(page);
                        }
                        Err(why) => refuse(page, &why),
                    }
                }
            }
        }
    }
}

/// The log line for `init.lua`'s `agent.default_mode` = `configured`, given the mode this project
/// remembers from Shift+Tab, if any, which beats it.
fn default_mode_said(configured: SessionModeChoice, remembered: Option<SessionModeChoice>) -> String {
    match remembered {
        Some(kept) => format!(
            "[agent] init.lua's agent.default_mode is {}, but this project remembers {} (Shift+Tab on an empty \
             tab), which wins: new tabs start in {}",
            configured.as_str(),
            kept.as_str(),
            kept.as_str()
        ),
        None => format!(
            "[agent] new tabs start in {} (init.lua's agent.default_mode){}",
            configured.as_str(),
            if configured == SessionModeChoice::Manual {
                "; a tab of an agent with no manual mode starts in auto"
            } else {
                ""
            }
        ),
    }
}

/// The `CycleMode` arm's WebView-free body: `TabSet::cycle_mode` on `tab`, plus remembering the mode an
/// empty tab moved to, for the next empty tab and the next launch. `TabSet::cycle_mode` can reach
/// `Changed` on a live tab too, so whether the tab was empty is read before the call. Only a mode the
/// user moved to on a tab that offers every mode is remembered: a tab whose agent has no manual takes
/// auto in its place when it leaves bypass, which nobody chose, and remembered it would beat
/// `init.lua`'s default on every later launch. A failure to write is logged, not refused: the mode
/// itself did change.
fn cycle_tab_mode(
    state: &Rc<RefCell<AgentPanelState>>,
    tab: TabId,
    page: ViewId,
) -> Result<eitri_core::tab_set::ModeCycle, String> {
    let remembered_if_moved = state
        .borrow()
        .tabs
        .get(tab)
        .is_some_and(|t| matches!(t.backend, TabBackend::NotStarted) && !t.agent().mode_fixed_at_start());
    let outcome = state.borrow_mut().tabs.cycle_mode(tab, page);
    if let Ok(eitri_core::tab_set::ModeCycle::Changed(mode)) = &outcome {
        if remembered_if_moved {
            let (prefs_dir, project_dir) = {
                let state_ref = state.borrow();
                (state_ref.prefs_dir.clone(), state_ref.project_dir.clone())
            };
            if let Some(dir) = prefs_dir {
                if let Err(e) = eitri_core::agent_prefs::save_mode(&dir, &project_dir, *mode) {
                    eprintln!("[agent_panel] could not remember the permission mode: {e}");
                }
            }
        }
    }
    outcome
}

/// `cycle_default_mode`: `TabSet::cycle_default_mode` plus remembering the new default on
/// `Changed(Auto)` only -- bypass is never written, and a `Confirm` is not persisted here
/// either (only actually entering bypass, via `apply_confirm_bypass` below, ever could -- and that is
/// forbidden too). A failure to write is logged, not refused, since the mode itself did change.
/// Never touches the WebView, so it is tested directly; the arm above owns `send_tabs` and
/// dispatching the prompt.
fn cycle_default_mode(state: &Rc<RefCell<AgentPanelState>>, page: ViewId) -> eitri_core::tab_set::ModeCycle {
    let outcome = state.borrow_mut().tabs.cycle_default_mode(page);
    if let eitri_core::tab_set::ModeCycle::Changed(mode) = outcome {
        let (prefs_dir, project_dir) = {
            let state_ref = state.borrow();
            (state_ref.prefs_dir.clone(), state_ref.project_dir.clone())
        };
        if let Some(dir) = prefs_dir {
            if let Err(e) = eitri_core::agent_prefs::save_mode(&dir, &project_dir, mode) {
                eprintln!("[agent_panel] could not remember the permission mode: {e}");
            }
        }
    }
    outcome
}

/// `confirm_bypass`: the `ConfirmBypass` arm's WebView-free body, the same split `cycle_default_mode`
/// gets above. `TabSet::confirm_bypass` does all of the checking (the re-check against exactly the
/// delivered cards, the nonce, the active tab, ended/failed) and bypass is never written to
/// disk, so there is nothing else for this wrapper to do -- it exists so a test can drive the exact
/// call the arm makes without a WebView, the same way `cycle_default_mode` can.
fn apply_confirm_bypass(
    state: &Rc<RefCell<AgentPanelState>>,
    scope: eitri_core::tab_set::BypassScope,
    nonce: u64,
    page: ViewId,
) -> Result<eitri_core::tab_set::ConfirmOutcome, String> {
    state.borrow_mut().tabs.confirm_bypass(scope, nonce, page)
}

/// `tab_verb`: the hook `main.rs` installs via `on_tab_verb` (`run_tab_action`), mapped from the wire shape and called -- or a refusal naming why there is none yet (a
/// legacy backend at startup, before `main.rs` has wired it). Never touches the WebView, so it is
/// tested directly.
fn run_tab_verb(state: &Rc<RefCell<AgentPanelState>>, view: ViewId, verb: TabVerbWire) -> Result<(), &'static str> {
    // Cloned out in its own statement: a `state.borrow()` in the `match` scrutinee would live to
    // the end of the `match` (edition 2021), and the hook -- `main.rs`'s `run_tab_action` -- goes
    // back through this same state with `borrow_mut` (`switch_with`, `new_tab`), which panics.
    let hook = state.borrow().tab_verb_hook.clone();
    match hook {
        Some(hook) => {
            hook(view, tab_action_for(verb));
            Ok(())
        }
        None => Err("tab verbs are not wired"),
    }
}

/// `tab_verb new` with an agent word (`:tabnew <agent>`, typed in `view`'s page): opens and selects a
/// base-view tab that runs the agent the word names, or refuses naming the words that are accepted.
/// Touches no page, so it is tested directly.
fn open_tab_named(state: &Rc<RefCell<AgentPanelState>>, view: ViewId, word: &str) -> Result<(), String> {
    let mut state = state.borrow_mut();
    let registered: Vec<&str> = state.acp_agents.iter().map(|a| a.name.as_str()).collect();
    let Some(agent) = eitri_core::agent_prefs::agent_named(word, &registered) else {
        let mut accepted = vec!["claude", "codex"];
        accepted.extend(registered);
        return Err(format!(
            "unknown agent {word} — :tabnew accepts {}",
            accepted.join(", ")
        ));
    };
    state.tabs.drop_bypass_prompt_in(view);
    state.tabs.open(agent);
    Ok(())
}

/// `nav_fallthrough`: the hook `main.rs` installs via `on_nav_fallthrough`, called
/// with the direction the page could not apply -- or a refusal naming why there is none yet. Never
/// touches the WebView, so it is tested directly, the same shape as `run_tab_verb`.
fn run_nav_fallthrough(
    state: &Rc<RefCell<AgentPanelState>>,
    view: ViewId,
    direction: NavKeyDirection,
) -> Result<(), &'static str> {
    let hook = state.borrow().nav_fallthrough_hook.clone();
    match hook {
        Some(hook) => {
            let direction = match direction {
                NavKeyDirection::Down => Direction::Down,
                NavKeyDirection::Up => Direction::Up,
            };
            hook(view, direction);
            Ok(())
        }
        None => Err("nav fallthrough is not wired"),
    }
}

/// `pane_nav`: the same hook `nav_fallthrough` uses -- `main.rs` installs it as
/// `move_focus(&ModuleId::agent(), direction)`, exactly what `Ctrl+h/j/k/l` do from the panel.
fn run_pane_nav(
    state: &Rc<RefCell<AgentPanelState>>,
    view: ViewId,
    direction: PaneNavDirection,
) -> Result<(), &'static str> {
    let hook = state.borrow().nav_fallthrough_hook.clone();
    let hook = hook.ok_or("pane nav is not wired")?;
    hook(
        view,
        match direction {
            PaneNavDirection::Left => Direction::Left,
            PaneNavDirection::Down => Direction::Down,
            PaneNavDirection::Up => Direction::Up,
            PaneNavDirection::Right => Direction::Right,
        },
    );
    Ok(())
}

/// `keymap_closed`: the hook `main.rs` installs via `on_keymap_closed`, cloned out of the state first because
/// what it does (moving the keys) re-enters the panel through `pane_focus`. With no hook it does nothing.
fn run_keymap_closed(state: &Rc<RefCell<AgentPanelState>>, opening: u64, return_keys: bool) {
    let hook = state.borrow().keymap_closed_hook.clone();
    if let Some(hook) = hook {
        hook(opening, return_keys);
    }
}

/// The file `gf` opens for a path the page sent: relative paths are under the project root. A path
/// holding a control character is refused outright: the path reaches the editor, where a newline
/// in a name has been able to end one command and start another, and no real file name the agent
/// shows needs one.
fn open_path_target(project_dir: &std::path::Path, path: &str) -> Result<std::path::PathBuf, String> {
    if path.chars().any(|c| c.is_ascii_control()) {
        return Err(format!("refused to open a path holding a control character: {path:?}"));
    }
    let candidate = std::path::Path::new(path);
    let resolved = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        project_dir.join(candidate)
    };
    if !resolved.exists() {
        return Err(format!("no such file: {path}"));
    }
    Ok(resolved)
}

/// `open_url`: what a panel key may hand `UriLauncher`. The page sends a WHATWG-normalized
/// `URL.href` (`nav.ts#webUrl`); no crate here parses the way a browser does (`url` is not a
/// dependency), so this is a strict re-check instead of a parse: ASCII only, no whitespace, control
/// character or backslash anywhere, an `http://` or `https://` scheme, and an authority that is a plain
/// host -- letters, digits, `.` and `-` only, no trailing dot, not the panel's own `eitri.invalid`
/// (`PANEL_BASE_URI`, where every relative link resolves) -- with an optional all-digit port. The scheme
/// and host are compared lower-cased, and the input comes back unchanged: this only ever says yes or no.
/// Anything a browser would re-normalize (`%`, `@`, `[`, non-ASCII, userinfo) is refused, not guessed at.
fn web_url(url: &str) -> Option<&str> {
    if url
        .chars()
        .any(|c| !c.is_ascii() || c.is_ascii_whitespace() || c.is_ascii_control() || c == '\\')
    {
        return None;
    }
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let (host, port) = authority.split_once(':').unwrap_or((authority, ""));
    let host_ok = !host.is_empty()
        && !host.ends_with('.')
        && host != "eitri.invalid"
        && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    (host_ok && port.chars().all(|c| c.is_ascii_digit())).then_some(url)
}

/// What a click on a link in the panel may hand `UriLauncher`: a web address `web_url` accepts, or a
/// `mailto:` one. Every other scheme is refused, and so is a relative link (it resolves to the panel's
/// own base, which `web_url` refuses). The input comes back unchanged.
pub fn clicked_link_url(uri: &str) -> Option<&str> {
    web_url(uri).or_else(|| mail_url(uri))
}

/// A `mailto:` address with something after the scheme: ASCII only, no whitespace, control character or
/// backslash anywhere. `%`, `?`, `&` and `=` stay allowed, since a real mail link carries
/// `?subject=a%20b`; the mail client parses the rest. Its query may name only the headers a draft
/// needs ([`MAIL_HEADERS`]): some mail clients read `attach=<path>` as a local file to attach, which
/// would put a file of the user's into a draft addressed by a reply.
fn mail_url(uri: &str) -> Option<&str> {
    if uri
        .chars()
        .any(|c| !c.is_ascii() || c.is_ascii_whitespace() || c.is_ascii_control() || c == '\\')
    {
        return None;
    }
    let scheme = uri.get(..7)?;
    if !(scheme.eq_ignore_ascii_case("mailto:") && uri.len() > 7) {
        return None;
    }
    let query = uri[7..].split_once('?').map_or("", |(_, query)| query);
    // `;` is split on too: a client that reads it as a separator must not find a key hidden in a value.
    let headers_known = query.split(['&', ';']).filter(|piece| !piece.is_empty()).all(|piece| {
        let key = piece.split_once('=').map_or(piece, |(key, _)| key);
        percent_decoded(key).is_some_and(|key| {
            MAIL_HEADERS
                .iter()
                .any(|known| key.eq_ignore_ascii_case(known.as_bytes()))
        })
    });
    headers_known.then_some(uri)
}

/// The `mailto:` query keys a clicked mail link may carry.
const MAIL_HEADERS: [&str; 6] = ["to", "cc", "bcc", "subject", "body", "in-reply-to"];

/// `text` with every `%XX` replaced by its byte; `None` for a `%` not followed by two hex digits.
fn percent_decoded(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text
                .get(i + 1..i + 3)
                .filter(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

/// `tab_verb`'s wire shape to the `TabAction` the prefix's own `Action::Tab` arm runs.
fn tab_action_for(verb: TabVerbWire) -> TabAction {
    match verb {
        TabVerbWire::Next => TabAction::Next,
        TabVerbWire::Prev => TabAction::Prev,
        TabVerbWire::Last => TabAction::Last,
        TabVerbWire::New => TabAction::New,
        TabVerbWire::Close => TabAction::Close,
        TabVerbWire::CloseOthers => TabAction::CloseOthers,
        TabVerbWire::Choose => TabAction::Choose,
        TabVerbWire::Info => TabAction::Info,
    }
}

/// `y` to `prefix &`: `tabs::close_steps`, in order. The record is never deleted.
///
/// **Refused while the tab is handing off**: the close takes
/// at most the sidecar's 10 s `close_session`, and the only thing it produces is the command the
/// user asked to be shown. Closing the tab under it would drop the command and the
/// `command_result` the panel is still waiting on.
///
/// A tab closed while it is still connecting has its `send_message`/`resume` answered with an
/// error here, so the panel's in-flight record is closed and a first message comes back with its
/// text; its connect is kept in `Retiring` and shut down when it finishes.
///
/// A tab bound to an extra view leaves that view empty: its page is sent its empty strip, and the
/// window is told by the next tick (`on_view_empty`), never from inside a page's own message.
fn close_tab(state: &Rc<RefCell<AgentPanelState>>, pages: &Pages, tab: TabId) -> Result<(), &'static str> {
    let before = state.borrow().tabs.attention();
    let Some(facts) = state.borrow().tabs.close_facts(tab) else {
        return Ok(());
    };
    let view = state.borrow().tabs.view_of(tab);
    if state
        .borrow()
        .tabs
        .get(tab)
        .is_some_and(|t| t.pending_handoff.is_some())
    {
        return Err("this tab is still being handed off to a terminal; close it once that finishes");
    }
    // A wait for the tab's old backend goes with the tab; nothing will start in it.
    state.borrow_mut().resume_after.remove(&tab);
    // Taken before the removal: a start asked from a page the tab has since left is answered there.
    let start_page = state.borrow_mut().start_pages.remove(&tab);
    let mut owed = Vec::new();
    for step in eitri_core::tabs::close_steps(&facts) {
        match step {
            eitri_core::tabs::CloseStep::Interrupt => {
                let interrupted = backend_for(&mut state.borrow_mut().tabs, tab).and_then(AgentBackend::interrupt);
                if let Err(e) = interrupted {
                    // The close goes on: the shutdown below ends the turn anyway.
                    eprintln!(
                        "[agent_panel] tab {}: interrupt before close failed: {}",
                        tab.0, e.message
                    );
                }
            }
            // The queued words outlive the tab as history.
            eitri_core::tabs::CloseStep::QueueToHistory => {
                let texts = state.borrow().tabs.queue_texts(tab);
                remember_prompts(state, pages, &texts);
            }
            // Folded into `Remove`: `Retiring::tab` shuts the removed tab's backend down.
            eitri_core::tabs::CloseStep::Shutdown => {}
            eitri_core::tabs::CloseStep::Remove => {
                let mut state_ref = state.borrow_mut();
                let state_ref = &mut *state_ref;
                if let Some(removed) = state_ref.tabs.remove(tab) {
                    owed = state_ref.retiring.tab(removed);
                }
                // Its question goes with it: a record still being written for it starts nothing.
                state_ref.trust.forget_tab(tab);
            }
        }
    }
    if view.is_base() {
        let payloads = switch_payloads(&mut state.borrow_mut());
        pages.send_all(ViewId::BASE, payloads);
    } else {
        let payload = tabs_payload_recorded(&mut state.borrow_mut(), view);
        pages.send(view, &payload);
        state.borrow_mut().emptied_views.push(view);
    }
    // After the switch, not before: the panel restores a refused send into the composer only while
    // its tab is the active one, and the switch that follows would then replace it with the next
    // tab's draft. Answered once the closed tab is no longer active, the refusal is reported with
    // the message's full text instead, and the switch cannot clear that notice.
    // The page that sent a request holds it, and it may not be the page that showed the tab: a first
    // message sent in the base view, its tab broken out while it waited, is still the base page's to
    // report (with its text) and to take back.
    for request_id in owed {
        let asked_from = match &start_page {
            Some((asked, page)) if *asked == request_id => *page,
            _ => view,
        };
        pages.send(
            asked_from,
            &serialize_command_result_for_js(&request_id, Err(TAB_CLOSED_MESSAGE)),
        );
    }
    send_hello_if_open_sessions_changed(state, pages);
    report_attention(state, before);
    Ok(())
}

/// A command arriving with no session. Benign: the frontend's start screen is showing, or the
/// session just ended -- either way the panel is healthy and there is nothing to tear down.
fn no_session_error() -> eitri_core::agent_backend::BackendError {
    // `folded_events` empty for the same reason: with no session there is no projection, so this
    // command changed nothing the frontend is owed. See `BackendError::folded_events`.
    eitri_core::agent_backend::BackendError {
        message: "no active session".to_string(),
        benign: true,
        folded_events: Vec::new(),
    }
}

/// Which `Ctrl+j`/`Ctrl+k` the panel takes for its composer: only `Ctrl+j` (`Direction::Down`) while the
/// `panel_keys` mirror reports `Browse`, or `Ctrl+k` (`Direction::Up`) while it reports `Input`; every
/// other combination goes to the host's focus move exactly as it would without an intercept. Pure, so
/// the whole table is a unit test with no display.
fn claims(mirror: PanelKeys, dir: Direction) -> bool {
    matches!(
        (dir, mirror),
        (Direction::Down, PanelKeys::Browse) | (Direction::Up, PanelKeys::Input)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The full decision table: the two claimed cells (`Down`+`Browse`, `Up`+`Input`) and every other
    /// combination, which must fall through to the host's focus move unclaimed.
    #[test]
    fn claims_only_down_in_browse_and_up_in_input() {
        for dir in [Direction::Left, Direction::Down, Direction::Up, Direction::Right] {
            for mirror in [PanelKeys::Browse, PanelKeys::Input, PanelKeys::Other] {
                let expected = matches!(
                    (dir, mirror),
                    (Direction::Down, PanelKeys::Browse) | (Direction::Up, PanelKeys::Input)
                );
                assert_eq!(claims(mirror, dir), expected, "{dir:?} + {mirror:?}");
            }
        }
    }

    use crate::panel_page::PageSurface;
    use eitri_core::agent_bridge::SessionModeChoice;
    use eitri_core::tab_set::{TabBackend, TabSet};
    use eitri_core::tabs::TabId;
    use eitri_core::test_providers::RecordingProvider;

    /// A path `gf` sends the editor is a file name, and no file name the agent shows needs a control
    /// character; one holding a newline is refused before nvim sees it, whatever exists on disk.
    #[test]
    fn open_path_refuses_a_control_character_before_looking_at_the_disk() {
        let dir = std::env::temp_dir().join(format!("eitri-open-path-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("plain.rs"), "").unwrap();
        let hostile = "notes\nlua(io.open)(string.char(47),string.char(119))";
        std::fs::write(dir.join(hostile), "").unwrap();

        assert_eq!(open_path_target(&dir, "plain.rs"), Ok(dir.join("plain.rs")));
        let absolute = dir.join("plain.rs");
        assert_eq!(
            open_path_target(&dir, &absolute.to_string_lossy()),
            Ok(absolute.clone())
        );
        let refused = open_path_target(&dir, hostile).unwrap_err();
        assert!(refused.contains("control character"), "{refused}");
        for c in ['\0', '\r', '\t', '\x1b', '\x7f'] {
            let path = format!("a{c}b.rs");
            assert!(open_path_target(&dir, &path).is_err(), "{path:?}");
        }
        assert!(open_path_target(&dir, "missing.rs")
            .unwrap_err()
            .contains("no such file"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Pins `SIDECAR_STOPPED_MARKER` against the Rust that actually produces it, the same way
    /// `agent-ui/web/src/problems.test.ts`'s own "matches watch.rs's own wording" test pins the
    /// frontend's copy: if `stream_ended_early_reason`'s opening sentence ever drifts, this fails
    /// loudly instead of `never_opened_message` silently going back to guessing on every stopped
    /// sidecar. The marker sits entirely on `stream_ended_early_reason`'s first source line, before
    /// its `\`-continuation, so no line-join normalisation is needed here (contrast the vitest
    /// test, which normalises defensively).
    #[test]
    fn the_sidecar_stopped_marker_is_pinned_to_watch_rs() {
        let watch_rs = include_str!("../../agent/src/providers/claude_sidecar/watch.rs");
        assert!(
            watch_rs.contains(SIDECAR_STOPPED_MARKER),
            "watch.rs's stream_ended_early_reason no longer contains {SIDECAR_STOPPED_MARKER:?}"
        );
    }

    /// A `reason` naming a stopped sidecar must not carry the "most likely no longer
    /// exists" guess, which is wrong for it -- the conversation and its transcript are fine.
    #[test]
    fn a_stopped_sidecar_reason_gets_no_never_opened_guess() {
        let reason = format!(
            "{SIDECAR_STOPPED_MARKER}, so anything after this point never arrived and the reply \
             above may be incomplete (stream closed)"
        );
        let message = never_opened_message(&reason);
        assert!(message.contains("the agent sidecar stopped"), "{message}");
        assert!(message.contains(&reason), "{message}");
        assert!(!message.contains("most likely no longer exists"), "{message}");
        assert!(!message.contains("start a new session instead"), "{message}");
    }

    /// The unwrapped counterpart: every other `reason` keeps the generic guess unchanged, so a
    /// session that really never opened still tells the reader to start over.
    #[test]
    fn a_generic_reason_keeps_the_never_opened_guess() {
        let reason = "provider process exited unexpectedly";
        let message = never_opened_message(reason);
        assert!(message.contains(reason), "{message}");
        assert!(message.contains("most likely no longer exists"), "{message}");
        assert!(message.contains("start a new session instead"), "{message}");
    }

    /// Every `y` in the panel (a row, a code block, the handoff command, the `prefix i` popover's
    /// line) calls `navigator.clipboard.writeText`, and none of them can reach the clipboard.
    /// `load_html(.., None)` gives the document an opaque origin, which is not
    /// a secure context, so WebKitGTK 2.52.6 has no `navigator.clipboard` and the optional chain
    /// did nothing. Measured in the sandbox with a bare WebKitGTK view: base `None`, nothing copied;
    /// base `https://eitri.invalid/`, `wl-paste` printed the text. Every `load_html` here must
    /// pass the secure base, and the base must stay one that never resolves (RFC 2606 `.invalid`):
    /// a relative link is still a `LinkClicked` the navigation guard hands to the browser. The host's one
    /// `load_html` passes on the base it is given; `shell` checks that half
    /// (`the_webkit_page_hands_load_html_the_base_it_is_given`).
    #[test]
    fn the_panel_document_is_loaded_in_a_secure_context_that_never_resolves() {
        use eitri_core::source_scan as scan;
        assert!(PANEL_BASE_URI.starts_with("https://"), "{PANEL_BASE_URI}");
        let host = PANEL_BASE_URI.trim_start_matches("https://").trim_end_matches('/');
        assert!(host.ends_with(".invalid"), "{host}");
        let sources = scan::rust_sources(&["shell/src", "panel/src"]);
        let code = scan::code_only(&scan::file_with(&sources, "fn handle_inbound_message(").text);
        let code = scan::without(&code, &scan::modules(&code, "tests"));
        // Every document load the panel asks for -- the first, a reload and the give-up page -- names
        // the secure base.
        let loads: Vec<&str> = code
            .match_indices(".load_document(")
            .map(|(i, _)| {
                let rest = &code[i..];
                &rest[..rest.find(';').unwrap_or(rest.len())]
            })
            .collect();
        assert!(loads.len() >= 3, "{loads:?}");
        for load in loads {
            assert!(
                load.contains("PANEL_BASE_URI"),
                "a load_document without the secure base: {load}"
            );
        }
    }

    /// The braced body of every `impl` whose header, with the whitespace removed, is `header_flat`.
    fn impl_blocks<'a>(code: &'a str, header_flat: &str) -> Vec<&'a str> {
        use eitri_core::source_scan as scan;
        code.match_indices("impl")
            .filter(|(at, _)| {
                !code[..*at]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_')
            })
            .filter_map(|(at, _)| {
                let open = at + code[at..].find('{')?;
                let flat: String = code[at..open].split_whitespace().collect();
                (flat == header_flat).then(|| scan::braced(code, open))
            })
            .collect()
    }

    /// `SESSION_CLOSE_BACKSTOP` must cover both the `close_session` unary RPC and the
    /// sidecar's own reap of the `claude` CLI it owns, with margin -- so the drop normally finishes
    /// inside this backstop rather than racing it.
    #[test]
    fn the_close_backstop_covers_a_close_rpc_and_the_sidecars_reap() {
        assert!(
            SESSION_CLOSE_BACKSTOP
                >= agent::UNARY_RPC_TIMEOUT + agent::SIDECAR_EXIT_GRACE + std::time::Duration::from_secs(1)
        );
    }

    fn live_backend(dir: &std::path::Path) -> (std::sync::Arc<RecordingProvider>, AgentBackend) {
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation = agent::AgentConversation::create(provider.clone(), dir, ProjectTrust::Untrusted).unwrap();
        (provider, AgentBackend::Sidecar(Box::new(conversation)))
    }

    /// A permission answered after a switch reaches the tab
    /// that asked, not the one on screen.
    #[test]
    fn a_permission_answer_names_its_own_tab_not_the_active_one() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-permission-tab");
        let mut set = TabSet::new(BackendKind::Sidecar, eitri_core::agent_bridge::SessionModeChoice::Auto);
        let asking = set.active();
        let (asking_provider, backend) = live_backend(&dir);
        set.get_mut(asking).unwrap().backend = TabBackend::Live(backend);
        let other = set.open(eitri_core::tab_set::AgentId::Claude);
        let (other_provider, backend) = live_backend(&dir);
        set.get_mut(other).unwrap().backend = TabBackend::Live(backend);
        assert_eq!(set.active(), other, "the user has switched away");
        // `perm-1` must really be pending on the asking tab: a conversation refuses an answer to a
        // permission it never asked for. A `Write` needs a human, so the pump leaves it a card --
        // `.git/main.rs` rather than plain `main.rs`:
        // an ordinary in-project `Write` no longer cards at all under the acceptEdits fast path.
        asking_provider.queue(agent::AgentDomainEvent::PermissionRequested {
            permission_id: "perm-1".into(),
            tool_use_id: None,
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }),
            provider_prompt: None,
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while set.get(asking).unwrap().attention.attention().pending != 1 {
            assert!(std::time::Instant::now() < deadline, "timed out waiting for the card");
            set.pump(&dir, true);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let message = parse_inbound_message(&format!(
            r#"{{"type":"permission_response","request_id":"r","tab":{},"permission_id":"perm-1","decision":"allow"}}"#,
            asking.0
        ))
        .unwrap();
        let tab = set.resolve(message.tab_ref()).unwrap().unwrap();
        // The production route itself (`InboundMessage::PermissionResponse`'s whole body).
        let outcome = answer_permission_response(
            &mut set,
            None,
            &dir,
            tab,
            ViewId::BASE,
            PermissionAnswer::of(message).unwrap(),
        );
        assert!(
            matches!(outcome, Ok(Ok(_))),
            "{:?}",
            outcome.map(|r| r.map_err(|e| e.message))
        );
        assert_eq!(asking_provider.resolutions(), vec![("perm-1".to_string(), true)]);
        assert!(other_provider.resolutions().is_empty(), "never the active tab");
        for mut tab in set.take_all() {
            if let TabBackend::Live(backend) = &mut tab.backend {
                backend.shutdown();
            }
        }
    }

    /// A card answer from the page meets the window's gate at the moment it arrives: refused while the
    /// gate says a float covers the chat -- the card still waiting, nothing sent to the agent, the page
    /// told why -- and the same answer taken once it says the chat is clear. With no gate, answers are
    /// taken as they always were.
    #[test]
    fn a_card_answer_is_refused_only_while_a_float_covers_the_chat_now() {
        const COVERED: &str = "agent is under terminal 2: C-b M-- from terminal 2 stows it";
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-answer-gate");
        let (handle, page, _host) = handle_with_recording_page();
        let state = handle.state.clone();
        let tab = state.borrow().tabs.active();
        let (provider, backend) = live_backend(&dir);
        state.borrow_mut().tabs.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        // A `Write` into `.git` needs a human, so each is left a card.
        for id in ["perm-1", "perm-2"] {
            provider.queue(agent::AgentDomainEvent::PermissionRequested {
                permission_id: id.into(),
                tool_use_id: None,
                tool_name: "Write".into(),
                input: serde_json::json!({ "file_path": format!(".git/{id}"), "content": "" }),
                provider_prompt: None,
            });
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.borrow().tabs.get(tab).unwrap().attention.attention().pending != 2 {
            assert!(std::time::Instant::now() < deadline, "timed out waiting for the cards");
            state.borrow_mut().tabs.pump(&dir, true);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let covered = Rc::new(Cell::new(true));
        let asked = Rc::new(Cell::new(0));
        handle.set_answer_gate(Rc::new({
            let covered = covered.clone();
            let asked = asked.clone();
            move |view: ViewId| {
                assert_eq!(view, ViewId::BASE, "asked with the view of the page that answered");
                asked.set(asked.get() + 1);
                if covered.get() {
                    Err(COVERED.to_string())
                } else {
                    Ok(())
                }
            }
        }));
        let answer = |request: &str, permission: &str| {
            handle.page_message(ViewId::BASE, &format!(
                r#"{{"type":"permission_response","request_id":"{request}","tab":{},"permission_id":"{permission}","decision":"allow"}}"#,
                tab.0
            ));
        };

        answer("r1", "perm-1");
        assert_eq!(result_for(&page.sent.borrow(), "r1"), Some(Err(COVERED.to_string())));
        assert!(
            state.borrow().tabs.get(tab).unwrap().card_is_waiting("perm-1"),
            "a refused answer leaves the card waiting"
        );
        assert!(provider.resolutions().is_empty(), "nothing reached the agent");

        // A tab's bypass `y` approves every card waiting in it, so from a covered chat it meets the
        // same gate before anything about the prompt is checked, and both cards stay.
        handle.page_message(
            ViewId::BASE,
            &format!(
                r#"{{"type":"confirm_bypass","request_id":"b1","tab":{},"scope":"tab","nonce":1}}"#,
                tab.0
            ),
        );
        assert_eq!(result_for(&page.sent.borrow(), "b1"), Some(Err(COVERED.to_string())));
        for id in ["perm-1", "perm-2"] {
            assert!(
                state.borrow().tabs.get(tab).unwrap().card_is_waiting(id),
                "{id} still waits"
            );
        }
        assert!(provider.resolutions().is_empty(), "bypass approved nothing");

        // The float moved off: the gate is asked again, at this answer, and lets it through.
        covered.set(false);
        answer("r2", "perm-1");
        assert_eq!(result_for(&page.sent.borrow(), "r2"), Some(Ok(())));
        assert_eq!(provider.resolutions(), vec![("perm-1".to_string(), true)]);
        assert_eq!(asked.get(), 3, "asked once per answer");

        state.borrow_mut().answer_gate = None;
        answer("r3", "perm-2");
        assert_eq!(
            result_for(&page.sent.borrow(), "r3"),
            Some(Ok(())),
            "no gate: taken as before"
        );
        assert_eq!(
            provider.resolutions(),
            vec![("perm-1".to_string(), true), ("perm-2".to_string(), true)]
        );
        for mut t in state.borrow_mut().tabs.take_all() {
            if let TabBackend::Live(backend) = &mut t.backend {
                backend.shutdown();
            }
        }
    }

    /// A chat a float covers is off screen for attention: what finished in it stays unread while the
    /// page is mapped under the float, and is seen once the window says the float is off it
    /// (`page_shown` again). With no probe set, a mapped page is on screen as before.
    #[test]
    fn a_chat_under_a_float_leaves_what_finished_unread_until_the_float_is_off_it() {
        let (handle, page, _host) = handle_with_recording_page();
        let state = handle.state.clone();
        let tab = state.borrow().tabs.active();
        let unread = || state.borrow().tabs.get(tab).unwrap().attention.attention().unread;
        page.visible.set(true);
        state
            .borrow_mut()
            .tabs
            .get_mut(tab)
            .unwrap()
            .attention
            .session_ended(false);
        assert!(unread());

        let covered = Rc::new(Cell::new(true));
        handle.set_coverage(Rc::new({
            let covered = covered.clone();
            move |_view| covered.get()
        }));
        handle.page_shown(ViewId::BASE);
        assert!(unread(), "mapped under a float is not seen");
        assert!(!on_screen(
            &state,
            ViewId::BASE,
            &handle.pages.live(ViewId::BASE).unwrap()
        ));

        covered.set(false);
        handle.page_shown(ViewId::BASE);
        assert!(!unread(), "seen once the float is off it");
        assert!(on_screen(
            &state,
            ViewId::BASE,
            &handle.pages.live(ViewId::BASE).unwrap()
        ));

        page.visible.set(false);
        assert!(
            !on_screen(&state, ViewId::BASE, &handle.pages.live(ViewId::BASE).unwrap()),
            "an unmapped page is never on screen"
        );
        state.borrow_mut().coverage = None;
        page.visible.set(true);
        assert!(
            on_screen(&state, ViewId::BASE, &handle.pages.live(ViewId::BASE).unwrap()),
            "no probe: mapped is on screen"
        );
    }

    /// The panel's own `permission_response` route records the human's approval, so the
    /// CLI's own prompt for the same call that follows it is answered without a second card. Were the route to answer the backend directly, this would be a second card.
    #[test]
    fn the_panels_approve_route_lets_the_clis_own_prompt_for_that_call_through() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-o3-approve-route");
        let mut set = TabSet::new(BackendKind::Sidecar, eitri_core::agent_bridge::SessionModeChoice::Auto);
        let tab = set.active();
        let (provider, backend) = live_backend(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let input = serde_json::json!({ "file_path": ".git/probe2", "content": "o3" });
        provider.queue(agent::AgentDomainEvent::PermissionRequested {
            permission_id: "perm-hook".into(),
            tool_use_id: Some("toolu_route".into()),
            tool_name: "Write".into(),
            input: input.clone(),
            provider_prompt: None,
        });
        let pump_until = |set: &mut TabSet, what: &str, done: &dyn Fn(&TabSet) -> bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !done(set) {
                assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
                set.pump(&dir, true);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        pump_until(&mut set, "the gate's card", &|set| {
            set.get(tab).unwrap().attention.attention().pending == 1
        });

        let message = parse_inbound_message(&format!(
            r#"{{"type":"permission_response","request_id":"r","tab":{},"permission_id":"perm-hook","decision":"allow"}}"#,
            tab.0
        ))
        .unwrap();
        let tab = set.resolve(message.tab_ref()).unwrap().unwrap();
        let outcome = answer_permission_response(
            &mut set,
            None,
            &dir,
            tab,
            ViewId::BASE,
            PermissionAnswer::of(message).unwrap(),
        );
        assert!(matches!(outcome, Ok(Ok(_))));

        provider.queue(agent::AgentDomainEvent::PermissionResolved {
            permission_id: "perm-hook".into(),
            outcome: agent::PermissionOutcome::Allowed,
        });
        provider.queue(agent::AgentDomainEvent::PermissionRequested {
            permission_id: "perm-cli".into(),
            tool_use_id: Some("toolu_route".into()),
            tool_name: "Write".into(),
            input,
            provider_prompt: Some(agent::ProviderPrompt {
                reason: Some("Claude requested permissions to edit .git/probe2 which is a sensitive file.".into()),
                ..Default::default()
            }),
        });
        let answered = |_: &TabSet| provider.resolutions().len() == 2;
        pump_until(&mut set, "the CLI's own prompt answered", &answered);
        assert_eq!(provider.resolutions()[1], ("perm-cli".to_string(), true));
        assert_eq!(
            set.get(tab).unwrap().attention.attention().arrived,
            1,
            "one card for the call"
        );
        for mut tab in set.take_all() {
            if let TabBackend::Live(backend) = &mut tab.backend {
                backend.shutdown();
            }
        }
    }

    /// A tab closed while the window stays open is kept in panel state
    /// until its teardown or connect finishes, so the window-close backstop can see it; and the
    /// command a connecting tab still owed is handed back to be answered.
    #[test]
    fn a_closed_tabs_teardown_and_connect_are_kept_until_they_finish() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-retiring");
        let mut set = TabSet::new(BackendKind::Sidecar, eitri_core::agent_bridge::SessionModeChoice::Auto);
        let live = set.active();
        set.get_mut(live).unwrap().backend = TabBackend::Live(live_backend(&dir).1);
        let connecting = set.open(eitri_core::tab_set::AgentId::Claude);
        let (connect_tx, result_rx) = mpsc::channel();
        set.get_mut(connecting).unwrap().backend = TabBackend::Starting(PendingStart {
            request_id: "req-send".into(),
            result_rx,
            first_turn: None,
            resume: None,
            resumed_title: None,
            resumed_name: None,
        });

        let mut retiring = Retiring::default();
        assert!(
            retiring.tab(set.remove(live).unwrap()).is_empty(),
            "a live tab owes nothing"
        );
        assert_eq!(
            retiring.tab(set.remove(connecting).unwrap()),
            vec!["req-send".to_string()]
        );
        assert_eq!((retiring.connects.len(), retiring.teardowns.len()), (1, 1));

        // The connect finishes after its tab is gone: what it built is shut down, and held too.
        connect_tx.send(Ok(live_backend(&dir).1)).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !retiring.is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for the teardowns"
            );
            retiring.poll();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn a_command_to_a_tab_with_no_session_is_a_benign_refusal() {
        let mut set = TabSet::new(BackendKind::Sidecar, eitri_core::agent_bridge::SessionModeChoice::Auto);
        let tab = set.active();
        let error = backend_for(&mut set, tab).err().expect("an empty tab has no backend");
        assert!(error.benign);
        assert_eq!(error.message, "no active session");
    }

    /// A page that records what the panel asked of it.
    #[derive(Default)]
    struct RecordingPage {
        sent: RefCell<Vec<String>>,
        documents: RefCell<Vec<(String, String)>>,
        backgrounds: RefCell<Vec<[u8; 3]>>,
        visible: Cell<bool>,
    }

    impl PageSurface for RecordingPage {
        fn send(&self, envelope_json: &str) {
            self.sent.borrow_mut().push(envelope_json.to_owned());
        }
        fn is_visible(&self) -> bool {
            self.visible.get()
        }
        fn load_document(&self, html: &str, base_uri: &str) {
            self.documents.borrow_mut().push((html.to_owned(), base_uri.to_owned()));
        }
        fn set_background(&self, rgb: [u8; 3]) {
            self.backgrounds.borrow_mut().push(rgb);
        }
    }

    /// A host that records the addresses it was asked to open.
    #[derive(Default)]
    struct RecordingHost {
        opened: RefCell<Vec<String>>,
    }

    impl PanelHost for RecordingHost {
        fn open_external(&self, url: &str) {
            self.opened.borrow_mut().push(url.to_owned());
        }
    }

    fn kind_of(envelope: &str) -> String {
        serde_json::from_str::<serde_json::Value>(envelope).unwrap()["kind"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// A small stand-in for the bundle: a `<head>` for the policy and the theme, and one inline script.
    static TEST_DOCUMENT: std::sync::LazyLock<PanelDocument> = std::sync::LazyLock::new(|| {
        PanelDocument::new("<!doctype html><html><head></head><body><script type=\"module\">1</script></body></html>")
    });

    /// A handle over a recording page and a recording host. Not built through `AgentPanelHandle::new`,
    /// which would connect to (or start) the supervisor.
    fn handle_with_recording_page() -> (AgentPanelHandle, Rc<RecordingPage>, Rc<RecordingHost>) {
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        let page = Rc::new(RecordingPage::default());
        let host = Rc::new(RecordingHost::default());
        state.borrow_mut().host = host.clone() as Rc<dyn PanelHost>;
        let handle = AgentPanelHandle::over(
            state,
            Some(Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>))),
        );
        (handle, page, host)
    }

    /// A new tab of an agent that offers no manual mode, under a manual window default, starts in
    /// auto, and the page is told so once, in its transient slot, after the `tabs` that shows it.
    #[test]
    fn a_new_acp_tab_under_a_manual_default_is_announced_once() {
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Manual));
        let page = Rc::new(RecordingPage::default());
        let handle = AgentPanelHandle::over(
            state.clone(),
            Some(Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>))),
        );
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        let notices = || {
            page.sent
                .borrow()
                .iter()
                .filter(|e| kind_of(e) == "notice")
                .map(|e| {
                    serde_json::from_str::<serde_json::Value>(e).unwrap()["text"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                })
                .collect::<Vec<_>>()
        };
        assert!(notices().is_empty(), "{:?}", notices());
        handle.new_tab_for(AgentId::Codex);
        assert!(notices().is_empty(), "Codex offers manual: {:?}", notices());
        handle.new_tab_for(AgentId::Acp("gem".into()));
        let said = notices();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("gem tabs have no manual mode"), "{said:?}");
        let sent = page.sent.borrow().clone();
        let last_tabs = sent.iter().rposition(|e| kind_of(e) == "tabs").unwrap();
        let notice = sent.iter().rposition(|e| kind_of(e) == "notice").unwrap();
        assert!(last_tabs < notice, "the tab is on screen before it is spoken of");
        assert_eq!(state.borrow().tabs.active_tab().mode(), SessionModeChoice::Auto);
        handle.new_tab_for(AgentId::Claude);
        assert_eq!(notices().len(), 1, "once");
        assert_eq!(state.borrow().tabs.active_tab().mode(), SessionModeChoice::Manual);
    }

    /// A note the tab set made before any page was ready (the configured ACP agent clamping the
    /// empty tab out of manual, as `init.lua` does at startup) is said once the page says `ready`,
    /// after the `tabs` that shows the tab, and not again on a later `tabs`.
    #[test]
    fn a_mode_note_made_before_the_page_was_ready_is_said_once_at_ready() {
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Manual));
        let page = Rc::new(RecordingPage::default());
        let handle = AgentPanelHandle::over(
            state.clone(),
            Some(Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>))),
        );
        handle.set_default_agent(AgentId::Acp("gem".into()));
        assert_eq!(state.borrow().tabs.active_tab().mode(), SessionModeChoice::Auto);
        let notices = || {
            page.sent
                .borrow()
                .iter()
                .filter(|e| kind_of(e) == "notice")
                .map(|e| {
                    serde_json::from_str::<serde_json::Value>(e).unwrap()["text"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                })
                .filter(|text| text.contains("have no manual mode"))
                .collect::<Vec<_>>()
        };
        assert!(notices().is_empty(), "nothing is said to a page that is not ready");
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        let said = notices();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("gem tabs have no manual mode"), "{said:?}");
        let sent = page.sent.borrow().clone();
        let last_tabs = sent.iter().rposition(|e| kind_of(e) == "tabs").unwrap();
        let notice = sent.iter().rposition(|e| kind_of(e) == "notice").unwrap();
        assert!(last_tabs < notice, "the tab is on screen before it is spoken of");
        handle.new_tab_for(AgentId::Claude);
        assert_eq!(notices().len(), 1, "a later tabs change does not say it again");
    }

    /// A page that panics if a send happens inside another send: the contract every host keeps.
    #[derive(Default)]
    struct StrictPage {
        inner: RecordingPage,
        sending: Cell<bool>,
    }

    impl PageSurface for StrictPage {
        fn send(&self, envelope_json: &str) {
            assert!(!self.sending.replace(true), "a send re-entered the page");
            self.inner.send(envelope_json);
            self.sending.set(false);
        }
        fn is_visible(&self) -> bool {
            self.inner.is_visible()
        }
        fn load_document(&self, html: &str, base_uri: &str) {
            self.inner.load_document(html, base_uri);
        }
        fn set_background(&self, rgb: [u8; 3]) {
            self.inner.set_background(rgb);
        }
    }

    /// `handle_with_recording_page` over a [`StrictPage`].
    fn handle_with_strict_page() -> (AgentPanelHandle, Rc<StrictPage>) {
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        let page = Rc::new(StrictPage::default());
        let handle = AgentPanelHandle::over(
            state,
            Some(Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>))),
        );
        (handle, page)
    }

    #[test]
    fn a_watch_holds_until_its_worker_reports() {
        let (tx, rx) = mpsc::channel::<()>();
        let mut watch = ShutdownWatch {
            entries: vec![watched("t", rx, SESSION_CLOSE_BACKSTOP, |()| Vec::new())],
        };
        let now = std::time::Instant::now();
        assert!(watch.step(now));
        tx.send(()).unwrap();
        assert!(!watch.step(now));
        assert!(watch.is_empty());
    }

    #[test]
    fn a_watch_lets_go_at_the_backstop_or_when_the_worker_dies() {
        let (_tx, rx) = mpsc::channel::<()>();
        let mut watch = ShutdownWatch {
            entries: vec![watched("t", rx, SESSION_CLOSE_BACKSTOP, |()| Vec::new())],
        };
        let start = std::time::Instant::now();
        assert!(watch.step(start));
        assert!(!watch.step(start + SESSION_CLOSE_BACKSTOP + std::time::Duration::from_millis(1)));
        let (tx, rx) = mpsc::channel::<()>();
        let mut watch = ShutdownWatch {
            entries: vec![watched("t", rx, SESSION_CLOSE_BACKSTOP, |()| Vec::new())],
        };
        drop(tx);
        assert!(!watch.step(std::time::Instant::now()));
    }

    #[test]
    fn a_connect_that_finishes_during_the_close_is_held_through_its_teardown() {
        let (connect_tx, connect_rx) = mpsc::channel::<u32>();
        let (teardown_tx, teardown_rx) = mpsc::channel::<()>();
        let teardown_rx = RefCell::new(Some(teardown_rx));
        let mut watch = ShutdownWatch {
            entries: vec![watched(
                "connect",
                connect_rx,
                CONNECT_CLOSE_BACKSTOP,
                move |_backend: u32| {
                    vec![watched(
                        "teardown",
                        teardown_rx.borrow_mut().take().unwrap(),
                        SESSION_CLOSE_BACKSTOP,
                        |()| Vec::new(),
                    )]
                },
            )],
        };
        let now = std::time::Instant::now();
        connect_tx.send(7).unwrap();
        assert!(watch.step(now), "the teardown it started is held");
        teardown_tx.send(()).unwrap();
        assert!(!watch.step(now));
    }

    #[test]
    fn ready_gets_its_envelopes_in_their_fixed_order() {
        let (handle, page) = handle_with_strict_page();
        page.inner.visible.set(true);
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        let kinds: Vec<String> = page.inner.sent.borrow().iter().map(|e| kind_of(e)).collect();
        let at = |k: &str| {
            kinds
                .iter()
                .position(|x| x == k)
                .unwrap_or_else(|| panic!("no {k} in {kinds:?}"))
        };
        assert_eq!(kinds[0], "hello");
        assert!(
            at("history") < at("tabs") && at("editor_context") < at("tabs"),
            "{kinds:?}"
        );
        assert!(at("display") < at("tabs"), "{kinds:?}");
        assert!(kinds.iter().any(|k| k == "command_result"), "{kinds:?}");
    }

    /// `agent.thinking` reaches the page: at once to a page already listening, and again on every
    /// `ready`, so a reloaded document shows thinking the way the first one did.
    #[test]
    fn the_thinking_level_is_told_at_once_and_again_on_every_ready() {
        use eitri_core::agent_prefs::ThinkingDisplay;
        let (handle, page) = handle_with_strict_page();
        page.inner.visible.set(true);
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        let displays = |page: &std::cell::RefCell<Vec<String>>| -> Vec<String> {
            page.borrow()
                .iter()
                .filter(|e| kind_of(e) == "display")
                .map(|e| {
                    serde_json::from_str::<serde_json::Value>(e).unwrap()["thinking"]
                        .as_str()
                        .unwrap()
                        .to_string()
                })
                .collect()
        };
        assert_eq!(displays(&page.inner.sent), vec!["collapsed"]);
        handle.set_thinking_display(ThinkingDisplay::Hidden);
        assert_eq!(displays(&page.inner.sent), vec!["collapsed", "hidden"]);
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r2"}"#);
        assert_eq!(displays(&page.inner.sent), vec!["collapsed", "hidden", "hidden"]);
    }

    /// The review switches reach the page: off before `init.lua`'s values are applied, at once to a
    /// page already listening when they are, before the tabs on every `ready`, and again to a document
    /// that replaces the first.
    #[test]
    fn the_review_settings_are_told_at_once_and_again_on_every_ready() {
        let (handle, page) = handle_with_strict_page();
        page.inner.visible.set(true);
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        let settings = |page: &std::cell::RefCell<Vec<String>>| -> Vec<(bool, bool)> {
            page.borrow()
                .iter()
                .filter(|e| kind_of(e) == "review_settings")
                .map(|e| {
                    let v = serde_json::from_str::<serde_json::Value>(e).unwrap();
                    (v["enabled"].as_bool().unwrap(), v["hint"].as_bool().unwrap())
                })
                .collect()
        };
        let kinds: Vec<String> = page.inner.sent.borrow().iter().map(|e| kind_of(e)).collect();
        let at = |k: &str| kinds.iter().position(|x| x == k).unwrap();
        assert!(at("review_settings") < at("tabs"), "{kinds:?}");
        assert_eq!(settings(&page.inner.sent), vec![(false, false)]);
        handle.set_review(ReviewConfig {
            enabled: true,
            hint: true,
        });
        assert_eq!(settings(&page.inner.sent), vec![(false, false), (true, true)]);
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r2"}"#);
        assert_eq!(
            settings(&page.inner.sent),
            vec![(false, false), (true, true), (true, true)]
        );
        handle.set_review(ReviewConfig {
            enabled: false,
            hint: true,
        });
        assert_eq!(settings(&page.inner.sent).last(), Some(&(false, true)));
    }

    #[test]
    fn the_tick_runs_until_the_close_and_never_after() {
        let (handle, page) = handle_with_strict_page();
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        assert!(handle.tick());
        assert!(handle.tick());
        let watch = handle.shutdown();
        assert!(watch.is_empty(), "no tab ever started a backend");
        assert!(!handle.tick());
        let before = page.inner.sent.borrow().len();
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r2"}"#);
        assert_eq!(
            page.inner.sent.borrow().len(),
            before,
            "a closing panel answers nothing"
        );
    }

    /// A closed tab's agent may still be writing while its backend shuts down (a subagent, during
    /// the shutdown's grace), so the window's presence stays as if a turn ran until that shutdown has
    /// returned: no revert in this window or another one can write meanwhile.
    #[test]
    fn closed_tab_keeps_presence_until_its_backend_has_shut_down() {
        use eitri_core::test_providers::{empty_trust_plan, ExternalRecordingProvider};
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-retiring-presence");
        let (handle, page, _host) = handle_with_recording_page();
        let provider = std::sync::Arc::new(ExternalRecordingProvider::codex());
        let release = provider.hold_close();
        let (codex, survivor, guard) = {
            let mut state_ref = handle.state.borrow_mut();
            let tabs = &mut state_ref.tabs;
            let codex = tabs.open(AgentId::Codex);
            tabs.get_mut(codex).unwrap().backend = TabBackend::Live(
                AgentBackend::start_external_with(
                    AgentId::Codex,
                    provider.clone(),
                    &dir,
                    empty_trust_plan(),
                    SessionModeChoice::Auto,
                    None,
                )
                .unwrap(),
            );
            let survivor = tabs.tabs().iter().find(|t| t.id != codex).unwrap().id;
            tabs.hold_presence(eitri_core::turn_review::PresenceHolder::start(
                dir.join(".review-state"),
                std::process::id(),
            ));
            (codex, survivor, tabs.presence_guard().unwrap())
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while matches!(guard.check(), Err(eitri_core::turn_review::Blocked::NotHeld(_))) {
            assert!(std::time::Instant::now() < deadline, "the presence was never held");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(guard.check(), Ok(()), "nothing runs yet");

        close_tab(&handle.state, &handle.pages, codex).unwrap();
        assert!(handle.state.borrow().tabs.get(codex).is_none());
        assert!(handle.state.borrow().tabs.get(survivor).is_some());
        page.visible.set(true);
        for _ in 0..5 {
            handle.tick();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            guard.check(),
            Err(eitri_core::turn_review::Blocked::TurnRunningHere),
            "the closed tab's backend is still shutting down"
        );
        assert_eq!(provider.inner.closes(), 0, "its close is held");

        release.send(()).unwrap();
        while guard.check() != Ok(()) {
            assert!(
                std::time::Instant::now() < deadline,
                "the presence never let go: {:?}",
                guard.check()
            );
            handle.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(provider.inner.closes(), 1, "released only after the shutdown returned");
    }

    /// The confirmed `<leader>k` reaches the conversation of the tab it names, never the one on screen.
    /// One that names a closed tab, or none, is refused; a refusal because of when it was asked is said
    /// and leaves the tab live; a taken stop ends the session through the pump, as a lost one that can
    /// be resumed. One `r` on it resumes it in the same tab, and the start waits until the old backend has
    /// let go of the session.
    #[test]
    fn a_stop_of_the_background_reaches_the_tab_it_names_and_ends_its_session() {
        use eitri_core::test_providers::{empty_trust_plan, ExternalRecordingProvider};
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-stop-background");
        let (handle, page, _host) = handle_with_recording_page();
        page.visible.set(true);
        let provider = std::sync::Arc::new(ExternalRecordingProvider::codex());
        let (codex, on_screen) = {
            let mut state_ref = handle.state.borrow_mut();
            let tabs = &mut state_ref.tabs;
            let codex = tabs.open(AgentId::Codex);
            tabs.get_mut(codex).unwrap().backend = TabBackend::Live(
                AgentBackend::start_external_with(
                    AgentId::Codex,
                    provider.clone(),
                    &dir,
                    empty_trust_plan(),
                    SessionModeChoice::Auto,
                    None,
                )
                .unwrap(),
            );
            let on_screen = tabs.tabs().iter().find(|t| t.id != codex).unwrap().id;
            tabs.select(on_screen);
            (codex, on_screen)
        };
        provider
            .inner
            .queue(agent::AgentDomainEvent::BackgroundCalls { running: 1 });
        let count = || {
            handle
                .state
                .borrow()
                .tabs
                .get(codex)
                .and_then(|t| t.live())
                .map(|b| b.projection().background_calls)
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while count() != Some(1) {
            assert!(std::time::Instant::now() < deadline, "the count never arrived");
            handle.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let result = |request: &str| -> Option<Result<(), String>> {
            page.sent.borrow().iter().find_map(|envelope| {
                let v: serde_json::Value = serde_json::from_str(envelope).unwrap();
                (v["kind"] == "command_result" && v["requestId"] == request).then(|| {
                    if v["ok"] == true {
                        Ok(())
                    } else {
                        Err(v["error"].as_str().unwrap_or("").to_string())
                    }
                })
            })
        };
        let stop = |request: &str, tab: Option<TabId>| {
            let tab = tab.map_or(String::new(), |t| format!(r#","tab":{}"#, t.0));
            handle.page_message(
                ViewId::BASE,
                &format!(r#"{{"type":"stop_background","request_id":"{request}"{tab},"running":1}}"#),
            );
        };

        stop("s0", Some(TabId(9_999)));
        assert_eq!(result("s0"), Some(Err("protocol: no tab 9999".to_string())));
        stop("s1", None);
        assert_eq!(
            result("s1"),
            Some(Err("protocol: this command names no tab".to_string()))
        );
        stop("s2", Some(on_screen));
        assert_eq!(result("s2"), Some(Err("no active session".to_string())));
        provider.refuse_next_stop(agent::ProviderErrorCode::NoActiveTurn);
        stop("s3", Some(codex));
        assert!(matches!(result("s3"), Some(Err(_))), "{:?}", result("s3"));
        assert!(
            handle.state.borrow().tabs.get(codex).unwrap().live().is_some(),
            "a refusal leaves the session alone"
        );
        assert_eq!(provider.stops(), 0);

        stop("s4", Some(codex));
        assert_eq!(result("s4"), Some(Ok(())));
        assert_eq!(provider.stops(), 1, "the tab it named, not the one on screen");
        let ended = || handle.state.borrow().tabs.get(codex).unwrap().wire_state().as_str() == "ended";
        while !ended() {
            assert!(std::time::Instant::now() < deadline, "the session never ended");
            handle.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(count(), Some(0));
        assert!(handle.state.borrow().tabs.get(on_screen).is_some());

        // The row says `r` resumes, and one `r` does.
        let wire = |tab: TabId| {
            let tabs: serde_json::Value = serde_json::from_str(&handle.state.borrow().tabs.tabs_payload()).unwrap();
            tabs["tabs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["id"] == tab.0)
                .unwrap()
                .clone()
        };
        assert_eq!(wire(codex)["resumesOnR"], true);
        assert_eq!(wire(on_screen)["resumesOnR"], false);
        let release = provider.hold_close();
        handle.page_message(
            ViewId::BASE,
            &format!(r#"{{"type":"reset_tab","request_id":"r1","tab":{}}}"#, codex.0),
        );
        {
            let state_ref = handle.state.borrow();
            let tab = state_ref.tabs.get(codex).unwrap();
            assert!(
                matches!(
                    &tab.backend,
                    TabBackend::AwaitingTrust(AwaitingTrust {
                        start: DeferredStart::Resume { provider_session_id, .. },
                        ..
                    }) if provider_session_id == "thread-1"
                ),
                "the session is resumed in place, not left for a start screen"
            );
            assert_eq!(
                state_ref.tabs.active(),
                on_screen,
                "a reset never moves the window to its tab"
            );
        }
        assert_eq!(result("r1"), None, "answered once the resume connects");
        assert_eq!(wire(codex)["resumesOnR"], false);
        // Its old backend keeps the lease until it is dropped; the start must not begin before. (This panel's
        // gate has no worker, so the start itself is put off on the next tick: the wait is read here.)
        let (session, gone) = handle
            .state
            .borrow_mut()
            .resume_after
            .remove(&codex)
            .expect("the start waits for the old backend");
        assert_eq!(session, "thread-1");
        for _ in 0..5 {
            handle.tick();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            gone.try_recv(),
            Err(mpsc::TryRecvError::Empty),
            "the old backend is still closing"
        );
        release.send(()).unwrap();
        assert_eq!(gone.recv_timeout(std::time::Duration::from_secs(10)), Ok(()));
        assert_eq!(provider.inner.closes(), 1);
    }

    /// `r` on an ended tab whose agent did not stop it itself still empties it to a start screen.
    #[test]
    fn r_on_a_session_that_simply_ended_starts_over() {
        use eitri_core::test_providers::{empty_trust_plan, ExternalRecordingProvider};
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-r-starts-over");
        let (handle, page, _host) = handle_with_recording_page();
        page.visible.set(true);
        let provider = std::sync::Arc::new(ExternalRecordingProvider::codex());
        let codex = {
            let mut state_ref = handle.state.borrow_mut();
            let tabs = &mut state_ref.tabs;
            let codex = tabs.open(AgentId::Codex);
            tabs.get_mut(codex).unwrap().backend = TabBackend::Live(
                AgentBackend::start_external_with(
                    AgentId::Codex,
                    provider.clone(),
                    &dir,
                    empty_trust_plan(),
                    SessionModeChoice::Auto,
                    None,
                )
                .unwrap(),
            );
            tabs.select(codex);
            codex
        };
        provider.inner.queue(agent::AgentDomainEvent::SessionUnavailable {
            reason: "the stream ended".into(),
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while handle.state.borrow().tabs.get(codex).unwrap().wire_state().as_str() != "ended" {
            assert!(std::time::Instant::now() < deadline, "the session never ended");
            handle.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(handle.state.borrow().tabs.resumes_on_r(codex), None);
        handle.page_message(
            ViewId::BASE,
            &format!(r#"{{"type":"reset_tab","request_id":"r1","tab":{}}}"#, codex.0),
        );
        assert!(matches!(
            handle.state.borrow().tabs.get(codex).unwrap().backend,
            TabBackend::NotStarted
        ));
        assert!(handle.state.borrow().resume_after.is_empty());
    }

    /// What one tick of a settled panel costs: a ready page, one empty tab, a selection in the
    /// editor, nothing happening. For a measurement run by hand (`--ignored --nocapture`).
    #[test]
    #[ignore]
    fn measure_an_idle_tick() {
        let (handle, page) = handle_with_strict_page();
        page.inner.visible.set(true);
        let context = eitri_core::editor_context::EditorContext {
            file: "/p/src/main.rs".into(),
            selection: Some(eitri_core::editor_context::Selection {
                start_line: 10,
                end_line: 60,
                text: "x".repeat(2000),
            }),
        };
        handle.state.borrow_mut().editor_context = Rc::new(move || Some(context.clone()));
        handle.state.borrow_mut().project_dir = PathBuf::from("/p");
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        for _ in 0..10 {
            handle.tick();
        }
        let sent = page.inner.sent.borrow().len();
        let ticks = 1000;
        let started = std::time::Instant::now();
        let (_, bytes) = eitri_core::alloc_count::bytes_allocated_by(|| {
            for _ in 0..ticks {
                handle.tick();
            }
        });
        let elapsed = started.elapsed();
        assert_eq!(page.inner.sent.borrow().len(), sent, "an idle tick sends nothing");
        eprintln!(
            "idle tick: {:?} and {} bytes allocated each",
            elapsed / ticks,
            bytes / ticks as usize
        );
    }

    /// A ready panel with one empty tab and the page on screen, its first ticks run.
    fn settled_panel() -> (AgentPanelHandle, Rc<RecordingPage>) {
        let (handle, page, _host) = handle_with_recording_page();
        page.visible.set(true);
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        let now = std::time::Instant::now();
        handle.take_wake(now);
        assert_eq!(
            handle.run_tick(now),
            crate::panel_wake::NextTick::OnWake,
            "a settled panel waits"
        );
        (handle, page)
    }

    /// Waits up to 5 s for the wake's descriptor to have been raised, and takes it.
    fn wait_for_wake(handle: &AgentPanelHandle) -> std::time::Instant {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(due) = handle.take_wake(std::time::Instant::now()) {
                return due;
            }
            assert!(std::time::Instant::now() < deadline, "the wake never came");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// Idle, the tick waits for the wake; every call into the panel and every page message raises it, and
    /// the tick it brings sends what the call changed.
    #[test]
    fn a_settled_panel_waits_and_a_call_into_it_wakes_the_tick() {
        let (handle, page) = settled_panel();
        let now = std::time::Instant::now();
        assert_eq!(handle.take_wake(now), None, "nothing raised");
        let sent = page.sent.borrow().len();
        handle.new_tab();
        let due = handle.take_wake(now).expect("a call into the panel wakes it");
        assert_eq!(handle.run_tick(due), crate::panel_wake::NextTick::OnWake);
        assert!(
            page.sent.borrow()[sent..].iter().any(|p| kind_of(p) == "tabs"),
            "the new tab reached the page"
        );
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r2"}"#);
        assert!(handle.take_wake(now).is_some(), "a page message wakes it");
    }

    /// A wake runs the tick no sooner than one `TICK` after the last one, so a stream of folds is taken in
    /// the batches a steady tick made; after a quiet stretch it runs at once.
    #[test]
    fn a_wake_runs_the_tick_no_sooner_than_one_tick_after_the_last() {
        let (handle, _page) = settled_panel();
        let at = std::time::Instant::now();
        handle.run_tick(at);
        handle.wake();
        assert_eq!(
            handle.take_wake(at + std::time::Duration::from_millis(5)),
            Some(at + AgentPanelHandle::TICK)
        );
        handle.wake();
        let later = at + std::time::Duration::from_secs(2);
        assert_eq!(handle.take_wake(later), Some(later));
    }

    /// An agent's events wake the tick: the backend's fold raises the wake, and the tick it brings sends
    /// them to the page. Then it waits again.
    #[test]
    fn a_backends_fold_wakes_the_tick_and_reaches_the_page() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-fold-wakes");
        let (handle, page) = settled_panel();
        let (provider, backend) = live_backend(&dir);
        {
            let mut state = handle.state.borrow_mut();
            let tab = state.tabs.active();
            state.tabs.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        }
        // A backend put in place without the wake is polled; the tick that finds it tells it about the
        // wake (which raises it once), and runs once more.
        let at = std::time::Instant::now();
        assert_eq!(
            handle.run_tick(at),
            crate::panel_wake::NextTick::At(at + AgentPanelHandle::TICK)
        );
        let due = wait_for_wake(&handle);
        assert_eq!(handle.run_tick(due), crate::panel_wake::NextTick::OnWake);
        let sent = page.sent.borrow().len();
        provider.queue(agent::AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        let due = wait_for_wake(&handle);
        assert_eq!(
            handle.run_tick(due),
            crate::panel_wake::NextTick::OnWake,
            "nothing left to poll"
        );
        assert!(
            page.sent.borrow()[sent..].iter().any(|p| kind_of(p) == "events"),
            "the fold reached the page: {:?}",
            &page.sent.borrow()[sent..]
        );
        let tabs = handle.state.borrow_mut().tabs.take_all();
        for mut tab in tabs {
            if let TabBackend::Live(backend) = &mut tab.backend {
                backend.shutdown();
            }
        }
    }

    /// A connect reports through a channel nobody is told about: the tick runs on its steady cadence while
    /// it is pending, collects it, runs once more, and then waits.
    #[test]
    fn a_pending_connect_is_polled_until_collected_and_once_more() {
        use crate::panel_wake::NextTick;
        let (handle, _page) = settled_panel();
        let (connect_tx, result_rx) = mpsc::channel::<Result<AgentBackend, BackendError>>();
        {
            let mut state = handle.state.borrow_mut();
            let tab = state.tabs.active();
            state.tabs.get_mut(tab).unwrap().backend = TabBackend::Starting(PendingStart {
                request_id: "req-send".into(),
                result_rx,
                first_turn: None,
                resume: None,
                resumed_title: None,
                resumed_name: None,
            });
        }
        let at = std::time::Instant::now();
        assert_eq!(
            handle.run_tick(at),
            NextTick::At(at + AgentPanelHandle::TICK),
            "pending: polled"
        );
        drop(connect_tx);
        let at = at + AgentPanelHandle::TICK;
        assert_eq!(
            handle.run_tick(at),
            NextTick::At(at + AgentPanelHandle::TICK),
            "collected: once more"
        );
        let at = at + AgentPanelHandle::TICK;
        assert_eq!(handle.run_tick(at), NextTick::OnWake, "then it waits");
    }

    /// Each pending thing the tick polls keeps it on its steady cadence: a closed tab's teardown, the
    /// supervisor's connect, the trust check, a review job, a scratch edit. None of them, nothing to poll.
    #[test]
    fn every_unannounced_pending_job_keeps_the_tick_polling() {
        let (handle, _page) = settled_panel();
        let polls = |handle: &AgentPanelHandle| needs_polling(&handle.state.borrow());
        assert!(!polls(&handle));

        let (done_tx, done_rx) = mpsc::channel();
        handle.state.borrow_mut().retiring.teardowns.push((None, done_rx));
        assert!(polls(&handle), "a teardown");
        done_tx.send(()).unwrap();
        handle.state.borrow_mut().retiring.poll();
        assert!(!polls(&handle));

        let (supervisor_tx, supervisor_rx) = mpsc::channel();
        handle.state.borrow_mut().supervisor_pending = Some(supervisor_rx);
        assert!(polls(&handle), "the supervisor's connect");
        drop(supervisor_tx);
        handle.state.borrow_mut().supervisor_pending = None;

        {
            let mut state = handle.state.borrow_mut();
            state.trust.wait_for(TabId(1), &Profile::Claude);
            state.trust.begin_check(&Profile::Claude);
        }
        assert!(polls(&handle), "the trust check's answer");
        let _ = handle.state.borrow_mut().trust.poll();
        assert!(!polls(&handle));

        let (_review_tx, review_rx) = mpsc::channel();
        handle.state.borrow_mut().review_jobs.push(PendingReview {
            request_id: "r".into(),
            view: ViewId::BASE,
            rx: review_rx,
            clears_hint: None,
        });
        assert!(polls(&handle), "a review job");
        handle.state.borrow_mut().review_jobs.clear();

        let mut scratch = eitri_core::scratch::ScratchDir::new().expect("a scratch directory");
        let (_request, edit) = scratch.prepare_edit("draft").unwrap();
        handle.state.borrow_mut().pending_edits.push(edit);
        assert!(polls(&handle), "a scratch edit");
        for edit in std::mem::take(&mut handle.state.borrow_mut().pending_edits) {
            edit.cleanup();
        }
        assert!(!polls(&handle));
    }

    /// The review flow's work (here: the recovery journal read it starts once a document is ready and a
    /// review exists) reports through a worker nobody is told about: the tick runs on its steady cadence
    /// until the flow is idle, and then waits.
    #[test]
    fn a_busy_review_flow_keeps_the_tick_polling_until_it_is_idle() {
        use crate::panel_wake::NextTick;
        agent::state_dirs::redirect_state_to_a_test_root();
        let project = agent::state_dirs::test_workspace_dir("panel-review-flow-polls");
        let review_dir = agent::state_dirs::test_workspace_dir("panel-review-flow-polls-store");
        let (handle, _page) = settled_panel();
        {
            let mut state = handle.state.borrow_mut();
            let state = &mut *state;
            let options = eitri_core::turn_review::ReviewOptions {
                excludes: Some(None),
                trim_every: std::time::Duration::MAX,
                ..eitri_core::turn_review::ReviewOptions::default()
            };
            state
                .tabs
                .install_turn_review(eitri_core::turn_review::TurnReview::with_options(
                    Some(review_dir),
                    &project,
                    options,
                ))
                .unwrap();
            let _ = state.review_flow.document_ready(&state.tabs);
            assert!(!state.review_flow.is_idle(), "the journal read is out");
        }
        assert!(needs_polling(&handle.state.borrow()), "a busy review flow is polled");
        let mut at = std::time::Instant::now();
        let deadline = at + std::time::Duration::from_secs(10);
        loop {
            let next = handle.run_tick(at);
            if handle.state.borrow().review_flow.is_idle() && !needs_polling(&handle.state.borrow()) {
                break;
            }
            assert_eq!(next, NextTick::At(at + AgentPanelHandle::TICK), "polled while busy");
            assert!(std::time::Instant::now() < deadline, "the review flow never went idle");
            std::thread::sleep(std::time::Duration::from_millis(5));
            at += AgentPanelHandle::TICK;
        }
        // The tick that saw it go idle asked for one more; the one after waits.
        let next = handle.run_tick(at + AgentPanelHandle::TICK);
        assert_eq!(next, NextTick::OnWake);
    }

    /// The typing cadence's deadlines: the first key of a typing window wakes the tick, which tells the
    /// page and is due again at the window's end; later keys wake nothing.
    #[test]
    fn the_typing_cadence_names_when_the_tick_is_due() {
        use crate::panel_wake::NextTick;
        let (handle, page) = settled_panel();
        handle.set_typing_cadence(Some(2));
        handle.set_editor_has_keys(true);
        let at = std::time::Instant::now();
        handle.take_wake(at);
        handle.run_tick(at);
        handle.note_editor_key();
        let due = handle
            .take_wake(std::time::Instant::now())
            .expect("the first key wakes the tick");
        handle.note_editor_key();
        assert_eq!(
            handle.take_wake(std::time::Instant::now()),
            None,
            "a later key does not"
        );
        let sent = page.sent.borrow().len();
        let NextTick::At(end) = handle.run_tick(due) else {
            panic!("the window's end is due")
        };
        assert!(
            page.sent.borrow()[sent..].iter().any(|p| p.contains("\"typing\":true")),
            "the page was told"
        );
        assert!(end > due && end <= std::time::Instant::now() + eitri_core::panel_cadence::TYPING_WINDOW);
        std::thread::sleep(end.saturating_duration_since(std::time::Instant::now()));
        let next = handle.run_tick(end);
        assert!(
            page.sent.borrow().iter().any(|p| p.contains("\"typing\":false")),
            "the end was told"
        );
        assert_eq!(next, NextTick::OnWake);
    }

    /// The close stops the tick for good.
    #[test]
    fn the_close_stops_the_tick() {
        let (handle, _page) = settled_panel();
        let _ = handle.shutdown();
        assert!(
            handle.take_wake(std::time::Instant::now()).is_some(),
            "the close wakes it"
        );
        assert_eq!(
            handle.run_tick(std::time::Instant::now()),
            crate::panel_wake::NextTick::Stop
        );
    }

    /// Every call a host makes into the panel raises the wake, so what it changed is sent; only reads and
    /// the scheduling calls themselves do not. A new `pub fn` on the handle must choose.
    #[test]
    fn every_call_into_the_panel_wakes_the_tick() {
        use eitri_core::source_scan as scan;
        const QUIET: &[&str] = &[
            "new",
            "tick",
            "run_tick",
            "wake",
            "wake_fd",
            "take_wake",
            "poll_activate",
            "supervisor_link",
            "panel_keys",
            "nav_intercept",
            "attention",
            "newest_card_label",
            "newest_card_label_in",
            "running_count",
            "queued_count",
        ];
        // Calls that wake only when what they changed needs the tick: a key that starts a typing window.
        const WAKES_WHEN: &[&str] = &["note_editor_key"];
        let sources = scan::rust_sources(&["panel/src"]);
        let code = scan::code_only(&scan::file_with(&sources, "fn handle_inbound_message(").text);
        let code = scan::without(&code, &scan::modules(&code, "tests"));
        let mut checked = 0;
        for (at, _) in code.match_indices("impl AgentPanelHandle {") {
            // The test-support block builds handles for tests; it is not a call a host makes.
            let line_before = code[..at].trim_end().rsplit('\n').next().unwrap_or("");
            if line_before.trim_start().starts_with("#[cfg(any(test, feature") {
                continue;
            }
            let body = &code[at..];
            let end = body.find("\n}").expect("the impl block ends");
            let body = &body[..end];
            for (fn_at, _) in body.match_indices("\n    pub fn ") {
                let rest = &body[fn_at + "\n    pub fn ".len()..];
                let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                let next = rest.find("\n    pub fn ").unwrap_or(rest.len());
                let fn_body = &rest[..next];
                checked += 1;
                if QUIET.contains(&name.as_str()) {
                    continue;
                }
                if WAKES_WHEN.contains(&name.as_str()) {
                    assert!(
                        fn_body.contains("self.wake()"),
                        "`AgentPanelHandle::{name}` never wakes the tick"
                    );
                    continue;
                }
                // The first statement, so no early return can skip it.
                let opened = fn_body.split_once('{').map_or("", |(_, body)| body);
                assert!(
                    opened.trim_start().starts_with("self.wake();"),
                    "`AgentPanelHandle::{name}` does not wake the tick before anything else"
                );
            }
        }
        assert!(checked > 50, "only {checked} methods found");
    }

    #[test]
    fn a_panel_with_no_page_never_ticks() {
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        assert!(!handle_for(&state).tick());
    }

    #[test]
    fn a_clicked_web_link_goes_to_the_host_and_anything_else_is_refused_on_the_page() {
        let (handle, page, host) = handle_with_recording_page();
        handle.link_clicked(ViewId::BASE, Some("https://example.com/a"));
        handle.link_clicked(ViewId::BASE, Some("tel:123"));
        handle.link_clicked(ViewId::BASE, None);
        assert_eq!(*host.opened.borrow(), vec!["https://example.com/a".to_owned()]);
        let kinds: Vec<String> = page.sent.borrow().iter().map(|e| kind_of(e)).collect();
        assert_eq!(kinds, vec!["notice", "notice"]);
    }

    #[test]
    fn the_first_document_is_the_themed_bundle_at_the_panel_base() {
        let (handle, page, _host) = handle_with_recording_page();
        handle.load_document(ViewId::BASE);
        let documents = page.documents.borrow();
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].1, PANEL_BASE_URI);
        assert!(documents[0].0.contains("<style id=\"nv-theme\">"));
        assert_eq!(page.backgrounds.borrow().len(), 1);
    }

    #[test]
    fn a_crash_reloads_the_bundle_and_repeated_crashes_give_up() {
        let (handle, page, _host) = handle_with_recording_page();
        handle.page_gone(ViewId::BASE, "Crashed");
        assert_eq!(page.documents.borrow().len(), 1, "one reload");
        for _ in 0..10 {
            handle.page_gone(ViewId::BASE, "Crashed");
        }
        let last = page.documents.borrow().last().cloned().unwrap();
        assert_eq!(last.1, PANEL_BASE_URI);
        assert!(
            !last.0.contains("<style id=\"nv-theme\">"),
            "the give-up page is not the bundle"
        );
    }

    #[test]
    fn open_url_goes_to_the_host_only_for_a_web_address() {
        let (handle, page, host) = handle_with_recording_page();
        handle.page_message(
            ViewId::BASE,
            r#"{"type":"open_url","request_id":"a","url":"https://example.com/"}"#,
        );
        handle.page_message(
            ViewId::BASE,
            r#"{"type":"open_url","request_id":"b","url":"file:///etc/passwd"}"#,
        );
        assert_eq!(*host.opened.borrow(), vec!["https://example.com/".to_owned()]);
        assert_eq!(
            page.sent
                .borrow()
                .iter()
                .filter(|e| kind_of(e) == "command_result")
                .count(),
            2
        );
    }

    /// A minimal `AgentPanelState` for tests that only care about window-level hooks (`tab_verb`)
    /// and never touch the WebView, which lives on `AgentPanelHandle` and not on this struct --
    /// so no display is required to run them.
    fn state_for_hooks(tabs: TabSet) -> Rc<RefCell<AgentPanelState>> {
        let mut state = bare_state(&TEST_DOCUMENT, Rc::new(RecordingHost::default()), tabs);
        state.spawner = spawn_nothing();
        Rc::new(RefCell::new(state))
    }

    /// A spawner whose every start fails at once (its sender is gone), so no test starts a process.
    fn spawn_nothing() -> Spawner {
        Spawner::Fake(Rc::new(RefCell::new(|_call: SpawnCall| mpsc::channel().1)))
    }

    /// Each chat view's chip shows its own tabs, so the hook hears a tab's own change even when the
    /// window's sum stays where it was, and both views handed a card in one pump are named.
    #[test]
    fn the_attention_hook_hears_each_views_change_and_names_every_view_handed_a_card() {
        use agent::AgentDomainEvent;
        use eitri_core::attention::Attention;
        let mut tabs = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let base = tabs.active();
        let moved = tabs.open(eitri_core::tab_set::AgentId::Claude);
        let two = ViewId::new(2).unwrap();
        assert_eq!(tabs.break_out(two), Ok(moved));
        let state = state_for_hooks(tabs);
        // Each call: whether the window's value changed, and the views handed a card.
        type Heard = Rc<RefCell<Vec<(bool, Vec<ViewId>)>>>;
        let heard: Heard = Rc::default();
        {
            let heard = heard.clone();
            state.borrow_mut().attention_hook = Some(Rc::new(
                move |before: Attention, after: Attention, handed: &[ViewId]| {
                    heard.borrow_mut().push((before != after, handed.to_vec()));
                },
            ));
        }
        let finished = AgentDomainEvent::TurnCompleted {
            turn_id: "t".into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        };
        let card = |id: &str| AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: None,
            tool_name: "Write".into(),
            input: serde_json::json!({}),
            provider_prompt: None,
        };
        let observe = |tab: TabId, event: &AgentDomainEvent| {
            let before = state.borrow().tabs.attention();
            state
                .borrow_mut()
                .tabs
                .get_mut(tab)
                .unwrap()
                .attention
                .observe(std::slice::from_ref(event), false);
            report_attention(&state, before);
        };

        observe(base, &finished);
        assert_eq!(heard.borrow_mut().pop(), Some((true, vec![])));
        // The window was already unread: only agent 2's chip changes.
        observe(moved, &finished);
        assert_eq!(heard.borrow_mut().pop(), Some((false, vec![])));
        // Nothing changed: not told.
        observe(moved, &finished);
        assert_eq!(heard.borrow_mut().pop(), None);

        // agent 2 is handed a card first, the base view the newest, before one report.
        let before = state.borrow().tabs.attention();
        {
            let mut state = state.borrow_mut();
            state
                .tabs
                .get_mut(moved)
                .unwrap()
                .attention
                .observe(&[card("p1")], false);
            state
                .tabs
                .get_mut(base)
                .unwrap()
                .attention
                .observe(&[card("p2")], false);
        }
        report_attention(&state, before);
        assert_eq!(heard.borrow_mut().pop(), Some((true, vec![two, ViewId::BASE])));
    }

    /// `review.enabled` and `review.hint` reach the panel's state: enabled installs a review and the hint
    /// follows its key; disabled leaves none and a request for one says why.
    #[test]
    fn the_review_keys_reach_the_panel_state() {
        use eitri_core::turn_review::{Scope, TurnRef};
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        state.borrow_mut().apply_review(ReviewConfig {
            enabled: true,
            hint: false,
        });
        assert!(state.borrow().tabs.turn_review().is_some());
        assert!(state.borrow().review_enabled);
        assert!(!state.borrow().review_hint);
        state.borrow_mut().apply_review(ReviewConfig {
            enabled: true,
            hint: true,
        });
        assert!(state.borrow().review_hint);

        // A backend without turn boundaries installs none, so the page is not told `c` would open one.
        let state = state_for_hooks(TabSet::new(BackendKind::Legacy, SessionModeChoice::Auto));
        state.borrow_mut().apply_review(ReviewConfig {
            enabled: true,
            hint: true,
        });
        assert!(state.borrow().tabs.turn_review().is_none());
        assert!(!state.borrow().review_enabled);

        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        state.borrow_mut().apply_review(ReviewConfig {
            enabled: false,
            hint: true,
        });
        let state = state.borrow();
        assert!(state.tabs.turn_review().is_none());
        assert!(!state.review_enabled);
        let tab = state.tabs.active();
        assert_eq!(
            state
                .tabs
                .review_overview_job(tab, TurnRef::Latest, Scope::Turn)
                .err()
                .as_deref(),
            Some(eitri_core::tab_set::TURN_REVIEW_OFF)
        );
    }

    /// A scratch directory under the build's target directory, which is where tests of this crate
    /// keep what they write.
    fn scratch_under_target(label: &str) -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        let dir = exe
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .join("review-flow-tests")
            .join(format!(
                "{label}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
        std::fs::create_dir_all(dir.join("project")).unwrap();
        dir.canonicalize().unwrap()
    }

    /// What another window (a pid no real one here has) sees of the window that holds `dir`.
    fn seen_from_another_window(dir: &Path) -> Option<eitri_core::turn_review::Busy> {
        eitri_core::turn_review::busy_elsewhere(dir, 999_999).unwrap()
    }

    /// A window holds its presence from the moment its state is made, on every backend and with the
    /// review off: the legacy backend installs no review at all, and `review.enabled = false`
    /// installs none either, yet another window's revert must still see both.
    #[test]
    fn presence_is_held_from_window_start_on_every_backend() {
        use eitri_core::turn_review::{review_dir_for, Busy, TurnReview};
        let scratch = scratch_under_target("presence-every-backend");
        let state_home = scratch.join("state");
        let pid = std::process::id();
        let wait_for_window = |dir: &Path| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            while seen_from_another_window(dir) != Some(Busy::OtherWindow { pid }) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the window never showed in {dir:?}"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };

        let legacy_root = scratch.join("project");
        let mut legacy = TabSet::new(BackendKind::Legacy, SessionModeChoice::Auto);
        let refused = TurnReview::with_options(None, &legacy_root, Default::default());
        assert!(
            legacy.install_turn_review(refused).is_err(),
            "the legacy backend installs no review"
        );
        hold_window_presence(&mut legacy, Some(state_home.as_os_str()), None, &legacy_root, pid);
        assert!(legacy.presence_guard().is_some());
        wait_for_window(&review_dir_for(Some(state_home.as_os_str()), None, &legacy_root).unwrap());

        let off_root = scratch.join("off-project");
        std::fs::create_dir_all(&off_root).unwrap();
        let mut off = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        off.turn_review_off();
        hold_window_presence(&mut off, Some(state_home.as_os_str()), None, &off_root, pid);
        assert!(off.turn_review().is_none());
        wait_for_window(&review_dir_for(Some(state_home.as_os_str()), None, &off_root).unwrap());

        // With no state directory there is nothing to hold, and a revert is refused for it.
        let mut nowhere = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        hold_window_presence(&mut nowhere, None, None, &legacy_root, pid);
        assert!(nowhere.presence_guard().is_none());
        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// With no page (WebKit's sandbox cannot
    /// start, so no `WebView` exists), every call `main.rs`'s focus, HINT, prefix, tab-verb, theme,
    /// zoom and close paths make still runs -- none panics, none needs a display -- and nothing that
    /// only a page could act on happens: no tab is opened or switched to (the verbs refuse, so the
    /// prefix flashes), `panel_keys` never leaves `Other` (so `Ctrl+j`/`Ctrl+k` are never claimed for
    /// a composer that does not exist and always move focus), and the chat owes no attention.
    #[test]
    fn a_panel_with_no_page_answers_every_window_call_and_does_nothing_a_page_would() {
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        let host = Rc::new(RecordingHost::default());
        state.borrow_mut().host = host.clone() as Rc<dyn PanelHost>;
        let handle = AgentPanelHandle::over(state.clone(), None);
        let tabs_before: Vec<TabId> = state.borrow().tabs.tabs().iter().map(|t| t.id).collect();

        // Focus and arrival (`pane_focus`, `move_focus`, `arrive`), the prefix's panel keys.
        handle.set_pane_focused(ViewId::BASE, true);
        handle.arrive(ViewId::BASE);
        handle.enter_input(ViewId::BASE);
        handle.nav_key(ViewId::BASE, NavKeyDirection::Down);
        handle.literal_key(ViewId::BASE, &eitri_core::keymap::KeySpec::parse("C-b").unwrap());
        handle.open_keymap(ViewId::BASE, None, 0);
        handle.open_command_line(ViewId::BASE);
        handle.set_keymap_help("{}".to_string());
        assert!(state.borrow().page(ViewId::BASE).unwrap().pane_focused);
        assert_eq!(handle.panel_keys(ViewId::BASE), PanelKeys::Other);
        handle.set_pane_focused(ViewId::BASE, false);

        // HINT (`hint::HintCoordinator`): every envelope is a no-op.
        handle.hint_collect(1);
        handle.hint_show(1, &["a".to_string()]);
        handle.hint_prefix(1, "a");
        handle.hint_land(1, 0);
        handle.hint_end(1);

        // Theme, zoom, reload and a crash report: recorded where there is state, nothing else.
        let mut tokens = eitri_core::theme::ThemeTokens::fallback();
        tokens.font_size_px = 17.0;
        handle.set_theme(&tokens);
        assert_eq!(state.borrow().theme.font_size_px, 17.0);
        handle.set_panel_font_size_px(19.0);
        handle.set_editor_row_px(21.0);
        assert_eq!(state.borrow().theme.font_size_px, 19.0);
        handle.reload_document(ViewId::BASE);
        handle.reload_document_by_hand(ViewId::BASE);
        handle.page_gone(ViewId::BASE, "Crashed");
        handle.load_document(ViewId::BASE);
        handle.page_shown(ViewId::BASE);
        handle.link_clicked(ViewId::BASE, Some("https://example.com/"));
        // A page message is the one way a command reaches the host, and there is no page to post one.
        handle.page_message(
            ViewId::BASE,
            r#"{"type":"open_url","request_id":"a","url":"https://example.com/"}"#,
        );
        assert!(host.opened.borrow().is_empty());

        // The tab verbs refuse: nothing a user could not see is opened, closed or switched to.
        handle.new_tab();
        assert!(!handle.step(1));
        assert!(!handle.select_last());
        assert!(handle.select_number(1).is_none());
        handle.begin_rename(ViewId::BASE);
        handle.confirm_close(ViewId::BASE);
        handle.confirm_close_others();
        handle.open_chooser(ViewId::BASE);
        handle.open_detail(ViewId::BASE);
        assert!(!handle.focus_oldest_card(ViewId::BASE));
        assert_eq!(handle.close_view_tabs(ViewId::BASE), Ok(0));
        let tabs_after: Vec<TabId> = state.borrow().tabs.tabs().iter().map(|t| t.id).collect();
        assert_eq!(tabs_after, tabs_before);

        // What the tray, the toast and the window-close prompt ask.
        assert_eq!(handle.attention(), Default::default());
        assert_eq!(handle.newest_card_label(), None);
        assert_eq!(handle.running_count(), 0);
        assert_eq!(handle.queued_count(), 0);
        assert!(!handle.poll_activate());
    }

    /// `tab_verb` refuses naming why until `main.rs` installs a hook
    /// (`AgentPanelHandle::on_tab_verb`), and once one is installed it is called with the mapped
    /// `TabAction` -- every wire value, not just one, since a mapping this exhaustive is easy to
    /// get wrong silently for the one case nobody wrote a test for.
    #[test]
    fn tab_verb_refuses_with_no_hook_and_calls_the_installed_hook_with_the_mapped_action() {
        let state = state_for_hooks(TabSet::new(
            BackendKind::Sidecar,
            eitri_core::agent_bridge::SessionModeChoice::Auto,
        ));
        assert_eq!(
            run_tab_verb(&state, ViewId::BASE, TabVerbWire::Close),
            Err("tab verbs are not wired")
        );

        let seen: Rc<RefCell<Vec<TabAction>>> = Rc::new(RefCell::new(Vec::new()));
        {
            let seen = seen.clone();
            state.borrow_mut().tab_verb_hook = Some(Rc::new(move |_view, action| seen.borrow_mut().push(action)));
        }
        for (verb, action) in [
            (TabVerbWire::Next, TabAction::Next),
            (TabVerbWire::Prev, TabAction::Prev),
            (TabVerbWire::Last, TabAction::Last),
            (TabVerbWire::New, TabAction::New),
            (TabVerbWire::Close, TabAction::Close),
            (TabVerbWire::CloseOthers, TabAction::CloseOthers),
            (TabVerbWire::Choose, TabAction::Choose),
            (TabVerbWire::Info, TabAction::Info),
        ] {
            assert_eq!(run_tab_verb(&state, ViewId::BASE, verb), Ok(()));
            assert_eq!(seen.borrow_mut().pop(), Some(action), "{verb:?} -> {action:?}");
        }
    }

    /// The product hook (`main.rs`'s `run_tab_action`) re-enters this
    /// same state with `borrow_mut` (`switch_with`, `new_tab`). `run_tab_verb` must not still hold
    /// its own borrow while the hook runs, or every panel tab key panics on the GTK thread.
    #[test]
    fn tab_verb_hook_may_borrow_the_panel_state_mutably() {
        let state = state_for_hooks(TabSet::new(
            BackendKind::Sidecar,
            eitri_core::agent_bridge::SessionModeChoice::Auto,
        ));
        let ran = Rc::new(RefCell::new(0usize));
        {
            let weak = Rc::downgrade(&state);
            let ran = ran.clone();
            state.borrow_mut().tab_verb_hook = Some(Rc::new(move |_view, _action| {
                let state = weak.upgrade().expect("state alive");
                let _guard = state.borrow_mut();
                *ran.borrow_mut() += 1;
            }));
        }
        assert_eq!(run_tab_verb(&state, ViewId::BASE, TabVerbWire::Next), Ok(()));
        assert_eq!(*ran.borrow(), 1);
    }

    /// The nav-mode mirror: `handle_inbound_message`'s `PanelKeys` arm is what
    /// `AgentPanelHandle::panel_keys` reads back, and it starts `Other`.
    #[test]
    fn a_panel_keys_message_updates_the_mirror_handle_inbound_message_records() {
        let state = state_for_hooks(TabSet::new(
            BackendKind::Sidecar,
            eitri_core::agent_bridge::SessionModeChoice::Auto,
        ));
        assert_eq!(
            state.borrow().page(ViewId::BASE).unwrap().nav_mode.get(),
            PanelKeys::Other
        );
        for mode in [PanelKeys::Browse, PanelKeys::Input, PanelKeys::Other] {
            state.borrow().page(ViewId::BASE).unwrap().nav_mode.set(mode);
            assert_eq!(state.borrow().page(ViewId::BASE).unwrap().nav_mode.get(), mode);
        }
    }

    /// The navigation-mode mirror starts at `Other` and resets to `Other` on every `ready`.
    /// Test reset_nav_mode_for_ready directly because the Ready handler needs a real WebView.
    /// A stale `Input` value after reload would make `claims(PanelKeys::Input, Direction::Down)`
    /// return false, sending Ctrl+j to move_focus instead of opening the composer.
    #[test]
    fn ready_resets_the_nav_mode_mirror_to_other() {
        let state = state_for_hooks(TabSet::new(
            BackendKind::Sidecar,
            eitri_core::agent_bridge::SessionModeChoice::Auto,
        ));
        for stale in [PanelKeys::Input, PanelKeys::Browse] {
            state.borrow().page(ViewId::BASE).unwrap().nav_mode.set(stale);
            reset_nav_mode_for_ready(&state.borrow(), ViewId::BASE);
            assert_eq!(
                state.borrow().page(ViewId::BASE).unwrap().nav_mode.get(),
                PanelKeys::Other,
                "stale {stale:?} must clear"
            );
        }
    }

    /// A crashed or replaced page's BROWSE/INPUT is
    /// forgotten at once, not at a `ready` the give-up document never sends. `page_gone`
    /// and `reload_document` are exercised against a recording page elsewhere; this pins the call they both make.
    #[test]
    fn a_dead_pages_nav_mode_is_forgotten_before_any_ready() {
        let state = state_for_hooks(TabSet::new(
            BackendKind::Sidecar,
            eitri_core::agent_bridge::SessionModeChoice::Auto,
        ));
        for stale in [PanelKeys::Input, PanelKeys::Browse] {
            state.borrow().page(ViewId::BASE).unwrap().nav_mode.set(stale);
            forget_the_pages_nav_mode(&state.borrow(), ViewId::BASE);
            assert_eq!(
                state.borrow().page(ViewId::BASE).unwrap().nav_mode.get(),
                PanelKeys::Other,
                "stale {stale:?}"
            );
        }
    }

    /// The same shape as `tab_verb_refuses_with_no_hook_and_calls_the_installed_hook_with_the_mapped_action`:
    /// a refusal naming why until `main.rs` installs a hook, then called with the mapped
    /// `eitri_core::layout::Direction` -- both wire values, not just one.
    #[test]
    fn nav_fallthrough_refuses_with_no_hook_and_calls_the_installed_hook_with_the_mapped_direction() {
        let state = state_for_hooks(TabSet::new(
            BackendKind::Sidecar,
            eitri_core::agent_bridge::SessionModeChoice::Auto,
        ));
        assert_eq!(
            run_nav_fallthrough(&state, ViewId::BASE, NavKeyDirection::Down),
            Err("nav fallthrough is not wired")
        );

        let seen: Rc<RefCell<Vec<Direction>>> = Rc::new(RefCell::new(Vec::new()));
        {
            let seen = seen.clone();
            state.borrow_mut().nav_fallthrough_hook =
                Some(Rc::new(move |_view, direction| seen.borrow_mut().push(direction)));
        }
        for (wire, direction) in [
            (NavKeyDirection::Down, Direction::Down),
            (NavKeyDirection::Up, Direction::Up),
        ] {
            assert_eq!(run_nav_fallthrough(&state, ViewId::BASE, wire), Ok(()));
            assert_eq!(seen.borrow_mut().pop(), Some(direction), "{wire:?} -> {direction:?}");
        }
    }

    /// `pane_nav` runs the very hook `nav_fallthrough` does -- `main.rs`'s
    /// `move_focus(&ModuleId::agent(), direction)` -- so `Ctrl+w h/j/k/l` moves the keys exactly as
    /// `Ctrl+h/j/k/l` do from the panel. A refusal naming why until `main.rs` installs it, then called
    /// with the mapped `Direction`, once per message, for all four wire values (the move is left to
    /// the hook: a side with no module is its business, not this function's).
    #[test]
    fn pane_nav_refuses_with_no_hook_and_calls_the_installed_hook_with_the_mapped_direction() {
        let state = state_for_hooks(TabSet::new(
            BackendKind::Sidecar,
            eitri_core::agent_bridge::SessionModeChoice::Auto,
        ));
        for wire in [
            PaneNavDirection::Left,
            PaneNavDirection::Down,
            PaneNavDirection::Up,
            PaneNavDirection::Right,
        ] {
            assert_eq!(
                run_pane_nav(&state, ViewId::BASE, wire),
                Err("pane nav is not wired"),
                "{wire:?}"
            );
        }

        let seen: Rc<RefCell<Vec<Direction>>> = Rc::new(RefCell::new(Vec::new()));
        {
            let seen = seen.clone();
            state.borrow_mut().nav_fallthrough_hook =
                Some(Rc::new(move |_view, direction| seen.borrow_mut().push(direction)));
        }
        for (wire, direction) in [
            (PaneNavDirection::Left, Direction::Left),
            (PaneNavDirection::Down, Direction::Down),
            (PaneNavDirection::Up, Direction::Up),
            (PaneNavDirection::Right, Direction::Right),
        ] {
            assert_eq!(run_pane_nav(&state, ViewId::BASE, wire), Ok(()));
            assert_eq!(seen.borrow_mut().pop(), Some(direction), "{wire:?} -> {direction:?}");
            assert!(
                seen.borrow().is_empty(),
                "{wire:?}: the hook is called once per message"
            );
        }
    }

    /// `keymap_closed` reaches the installed hook with the opening and the `return_keys` the page sent, once
    /// per message, and holds no borrow of the state while the hook runs (the hook moves the keys, which
    /// re-enters the panel).
    #[test]
    fn a_keymap_closed_reaches_the_hook_with_its_opening_and_return_keys() {
        let state = state_for_hooks(TabSet::new(
            BackendKind::Sidecar,
            eitri_core::agent_bridge::SessionModeChoice::Auto,
        ));
        let seen: Rc<RefCell<Vec<(u64, bool)>>> = Rc::new(RefCell::new(Vec::new()));
        let reentrant = Rc::downgrade(&state);
        {
            let seen = seen.clone();
            state.borrow_mut().keymap_closed_hook = Some(Rc::new(move |opening, return_keys| {
                let state = reentrant.upgrade().expect("the state outlives the call");
                state.borrow_mut().page_mut(ViewId::BASE).pane_focused = true;
                seen.borrow_mut().push((opening, return_keys));
            }));
        }
        run_keymap_closed(&state, 7, true);
        run_keymap_closed(&state, 8, false);
        assert_eq!(*seen.borrow(), vec![(7, true), (8, false)]);
        assert!(state.borrow().page(ViewId::BASE).is_some_and(|page| page.pane_focused));
    }

    /// The companion window and `eitri-mac` install no hook: the message is accepted and nothing happens.
    #[test]
    fn a_keymap_closed_without_a_hook_does_nothing() {
        let state = state_for_hooks(TabSet::new(
            BackendKind::Sidecar,
            eitri_core::agent_bridge::SessionModeChoice::Auto,
        ));
        assert!(state.borrow().keymap_closed_hook.is_none());
        run_keymap_closed(&state, 3, true);
        assert!(!state.borrow().page(ViewId::BASE).is_some_and(|page| page.pane_focused));
    }

    /// A raw `keymap_closed` through the inbound dispatch is answered with an ok `command_result` for its
    /// request (never an error toast) with or without a hook, and the hook gets the opening and
    /// `return_keys` the message carried.
    #[test]
    fn a_raw_keymap_closed_is_answered_ok_with_or_without_a_hook() {
        let (handle, page, _host) = handle_with_recording_page();
        let seen: Rc<RefCell<Vec<(u64, bool)>>> = Rc::new(RefCell::new(Vec::new()));
        let result = |request: &str| -> Option<(bool, String)> {
            page.sent.borrow().iter().find_map(|envelope| {
                let v: serde_json::Value = serde_json::from_str(envelope).unwrap();
                (v["kind"] == "command_result" && v["requestId"] == request)
                    .then(|| (v["ok"] == true, v["error"].as_str().unwrap_or("").to_string()))
            })
        };

        handle.page_message(
            ViewId::BASE,
            r#"{"type":"keymap_closed","request_id":"k1","opening":4,"return_keys":true}"#,
        );
        assert_eq!(result("k1"), Some((true, String::new())), "no hook: accepted, no error");

        {
            let seen = seen.clone();
            handle.on_keymap_closed(move |opening, return_keys| seen.borrow_mut().push((opening, return_keys)));
        }
        handle.page_message(
            ViewId::BASE,
            r#"{"type":"keymap_closed","request_id":"k2","opening":5,"return_keys":false}"#,
        );
        assert_eq!(result("k2"), Some((true, String::new())), "a hook: accepted, no error");
        assert_eq!(
            *seen.borrow(),
            vec![(5, false)],
            "only the message sent with a hook reaches it"
        );
    }

    /// The hook `main.rs` installs goes back through the panel's state (`move_focus` reads and writes
    /// it), so `run_pane_nav` must not hold a borrow of the state across the call -- the same reason
    /// `run_tab_verb` clones its hook out first. A hook that borrows the state mutably panics here if
    /// it does.
    #[test]
    fn pane_nav_does_not_hold_the_state_borrowed_while_the_hook_runs() {
        let state = state_for_hooks(TabSet::new(
            BackendKind::Sidecar,
            eitri_core::agent_bridge::SessionModeChoice::Auto,
        ));
        let reentrant = Rc::downgrade(&state);
        state.borrow_mut().nav_fallthrough_hook = Some(Rc::new(move |_, _| {
            let state = reentrant.upgrade().expect("the state outlives the call");
            state.borrow_mut().page_mut(ViewId::BASE).pane_focused = true;
        }));
        assert_eq!(run_pane_nav(&state, ViewId::BASE, PaneNavDirection::Right), Ok(()));
        assert!(state.borrow().page(ViewId::BASE).unwrap().pane_focused);
    }

    /// The addresses `gx` may hand `UriLauncher`. The page sends a normalized
    /// `URL.href`, so these are what a browser writes: a query, a fragment, a port, a numeric host, a
    /// punycode one, an `@` after the host's slash. An upper-case spelling (which only a page that did not
    /// normalize would send) is judged as its lower-case self and returned unchanged, never rewritten.
    #[test]
    fn web_url_accepts_plain_http_and_https_links() {
        for url in [
            "https://example.com/a?b#c",
            "http://x.y:8080/p",
            "https://xn--r8jz45g.jp/",
            "https://example.com/@user",
            "http://127.0.0.1:3000/",
            "HTTPS://EXAMPLE.COM/A",
        ] {
            assert_eq!(web_url(url), Some(url), "{url}");
        }
    }

    /// Every way a link can be something other than a web page a reader was shown: another scheme, a
    /// relative link, the panel's own address (spelled any way a browser resolves it), a backslash or
    /// userinfo that moves the real host, whitespace, a bracketed or non-ASCII host, and hosts a browser
    /// would re-normalize. Backslash cases are raw strings so the backslash reaches the function.
    #[test]
    fn web_url_refuses_every_non_web_or_disguised_link() {
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "mailto:a@b",
            "docs/a.md",
            "https://eitri.invalid/x",
            "https://EITRI.invalid/x",
            "https://eitri.invalid./x",
            "https://%65itri.invalid/x",
            r"https:\\eitri.invalid\x",
            r"https://example.com\@evil/",
            r"https://example.com/?a\b",
            "https://a b",
            "https://user@evil",
            "https://user:pw@example.com/",
            "https://例え.jp/",
            "https://[::1]/",
            "https://example.com./",
            "https://a$b.com/",
            "https://my_host.x/",
            "https://",
            "http://x.y:8080:9/p",
            "https://example.com:80@evil.example/",
            "https://example.com/a\nb",
            // The same rules past the host, where the host check cannot help: a space, a control character
            // and a non-ASCII character in the path or query (a browser percent-encodes all three).
            "https://example.com/a b",
            "https://example.com/a\u{7f}b",
            "https://example.com/?q=例え",
            "",
        ] {
            assert_eq!(web_url(url), None, "{url:?}");
        }
    }

    /// The panel document's own base (`PANEL_BASE_URI`) is where every relative link in a reply resolves;
    /// opening it would launch a browser at an address that never resolves. Tied to the constant, so a
    /// change of the base fails here until `web_url` follows it.
    #[test]
    fn web_url_refuses_the_panels_own_base() {
        assert_eq!(web_url(PANEL_BASE_URI), None);
        assert_eq!(web_url(&format!("{PANEL_BASE_URI}docs/a.md")), None);
    }

    /// A click (or Enter on a focused link) launches a web page or a mail draft and nothing else: any
    /// other scheme hands its data to another program on one click, and a relative link resolves to the
    /// panel's own address, which goes nowhere.
    #[test]
    fn a_clicked_link_launches_only_web_and_mail_addresses() {
        for uri in [
            "https://example.com/a",
            "http://x.y:8080/p",
            "mailto:a@b.example",
            "MAILTO:a@b",
            "mailto:a@b?subject=a%20b",
        ] {
            assert_eq!(clicked_link_url(uri), Some(uri), "{uri}");
        }
        for uri in [
            "ftp://x.example/",
            "sms:123",
            "tel:123",
            "callto:x",
            "xmpp:a@b",
            "matrix:r/x:y",
            "cid:x",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://eitri.invalid/docs/a.md",
            "mailto:",
            "mailto:a b",
            "mailto:a@b\n",
            r"mailto:a\b",
            "mailto:例@x",
            "",
        ] {
            assert_eq!(clicked_link_url(uri), None, "{uri:?}");
        }
    }

    /// A mail link may carry only the headers a draft needs. Any other key -- `attach` above all,
    /// which some mail clients honour by attaching a local file to the draft -- refuses the whole
    /// link, however its name is spelled or encoded.
    #[test]
    fn a_mail_link_carries_only_the_headers_a_draft_needs() {
        for uri in [
            "mailto:a@b?subject=hi",
            "mailto:a@b?to=c@d&cc=e@f&bcc=g@h",
            "mailto:a@b?SUBJECT=x&Body=y%0Az",
            "mailto:a@b?in-reply-to=%3Cid@x%3E",
            "mailto:a@b?%73ubject=x",
            "mailto:?to=a@b",
            "mailto:a@b?",
            "mailto:a@b?subject",
            "mailto:a@b?subject=x&",
        ] {
            assert_eq!(mail_url(uri), Some(uri), "{uri}");
        }
        for uri in [
            "mailto:a@b?attach=/etc/passwd",
            "mailto:a@b?subject=x&attach=/etc/passwd",
            "mailto:a@b?Attachment=/etc/passwd",
            "mailto:a@b?Attach%6Dent=/etc/passwd",
            "mailto:a@b?%61ttach=/etc/passwd",
            "mailto:a@b?subject=x;attach=/etc/passwd",
            "mailto:a@b?x-attach=y",
            "mailto:a@b?from=x",
            "mailto:a@b?=x",
            "mailto:a@b?%zz=x",
            "mailto:a@b?%+5ubject=x",
            "mailto:a@b?sub%00ject=x",
        ] {
            assert_eq!(mail_url(uri), None, "{uri:?}");
        }
    }

    /// Bypass is never remembered on disk: `save_mode` refuses it outright. Auto -> bypass
    /// always asks first and writes nothing either way; leaving bypass (to manual) and manual ->
    /// auto are remembered.
    #[test]
    fn cycle_default_mode_moves_the_default_and_never_remembers_bypass_on_disk() {
        let dir = agent::state_dirs::test_workspace_dir("panel-cycle-default-mode");
        let prefs_dir = dir.join("prefs");
        let project_dir = dir.join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let open_tab = set.active();
        let state = state_for_hooks(set);
        state.borrow_mut().prefs_dir = Some(prefs_dir.clone());
        state.borrow_mut().project_dir = project_dir.clone();

        let plan = match cycle_default_mode(&state, ViewId::BASE) {
            eitri_core::tab_set::ModeCycle::Confirm(plan) => plan,
            other => panic!("auto -> bypass must ask first: {other:?}"),
        };
        assert_eq!(
            state.borrow().tabs.default_mode(),
            SessionModeChoice::Auto,
            "nothing moved yet"
        );
        assert_eq!(
            eitri_core::agent_prefs::load_mode(&prefs_dir, &project_dir),
            eitri_core::agent_prefs::LoadedMode::Missing,
            "nothing on disk yet"
        );

        // Confirming is `apply_confirm_bypass`'s job, which is `TabSet::confirm_bypass`
        // and nothing else -- it moves the in-memory default and still writes nothing.
        assert_eq!(
            apply_confirm_bypass(&state, plan.scope, plan.nonce, ViewId::BASE),
            Ok(eitri_core::tab_set::ConfirmOutcome::Entered {
                approved: 0,
                resolved: vec![]
            })
        );
        assert_eq!(state.borrow().tabs.default_mode(), SessionModeChoice::Bypass);
        // Never the open tab's own mode (still `NotStarted`, so `cycle_mode` alone would move it;
        // `confirm_bypass(Default, ..)` must not).
        assert_eq!(set_mode(&state, open_tab), SessionModeChoice::Auto);
        assert_eq!(
            eitri_core::agent_prefs::load_mode(&prefs_dir, &project_dir),
            eitri_core::agent_prefs::LoadedMode::Missing,
            "still nothing on disk while bypass"
        );

        // Leaving bypass moves at once, to manual, and IS remembered -- manual is allowed.
        assert_eq!(
            cycle_default_mode(&state, ViewId::BASE),
            eitri_core::tab_set::ModeCycle::Changed(SessionModeChoice::Manual)
        );
        assert_eq!(state.borrow().tabs.default_mode(), SessionModeChoice::Manual);
        assert_eq!(
            eitri_core::agent_prefs::load_mode(&prefs_dir, &project_dir),
            eitri_core::agent_prefs::LoadedMode::Remembered(SessionModeChoice::Manual)
        );
        // Manual -> auto moves at once and is remembered too.
        assert_eq!(
            cycle_default_mode(&state, ViewId::BASE),
            eitri_core::tab_set::ModeCycle::Changed(SessionModeChoice::Auto)
        );
        assert_eq!(
            eitri_core::agent_prefs::load_mode(&prefs_dir, &project_dir),
            eitri_core::agent_prefs::LoadedMode::Remembered(SessionModeChoice::Auto)
        );
    }

    /// The log says what new tabs start in: init.lua's mode, or the remembered one that beats it.
    #[test]
    fn the_default_mode_log_names_the_mode_that_wins() {
        let said = default_mode_said(SessionModeChoice::Manual, None);
        assert!(said.starts_with("[agent] new tabs start in manual (init.lua"), "{said}");
        let said = default_mode_said(SessionModeChoice::Manual, Some(SessionModeChoice::Auto));
        assert!(said.ends_with("new tabs start in auto"), "{said}");
        assert!(said.contains("agent.default_mode is manual"), "{said}");
    }

    /// Shift+Tab on an empty tab remembers the mode it moved to, except where the tab's agent has no
    /// manual: leaving bypass there lands on auto only because that agent offers nothing stricter, so
    /// nothing is written and init.lua's manual default is not beaten on the next launch. The window
    /// default stays manual.
    #[test]
    fn leaving_bypass_on_an_empty_acp_tab_remembers_nothing() {
        let dir = agent::state_dirs::test_workspace_dir("panel-cycle-acp-tab");
        let prefs_dir = dir.join("prefs");
        let project_dir = dir.join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Manual);
        let claude = set.active();
        let acp = set.open(eitri_core::tab_set::AgentId::Acp("gem".into()));
        let state = state_for_hooks(set);
        state.borrow_mut().prefs_dir = Some(prefs_dir.clone());
        state.borrow_mut().project_dir = project_dir.clone();
        assert_eq!(set_mode(&state, acp), SessionModeChoice::Auto);

        let plan = match cycle_tab_mode(&state, acp, ViewId::BASE) {
            Ok(eitri_core::tab_set::ModeCycle::Confirm(plan)) => plan,
            other => panic!("auto -> bypass asks: {other:?}"),
        };
        apply_confirm_bypass(&state, plan.scope, plan.nonce, ViewId::BASE).unwrap();
        assert_eq!(set_mode(&state, acp), SessionModeChoice::Bypass);
        assert_eq!(
            cycle_tab_mode(&state, acp, ViewId::BASE),
            Ok(eitri_core::tab_set::ModeCycle::Changed(SessionModeChoice::Auto))
        );
        assert_eq!(state.borrow().tabs.default_mode(), SessionModeChoice::Manual);
        assert_eq!(
            eitri_core::agent_prefs::load_mode(&prefs_dir, &project_dir),
            eitri_core::agent_prefs::LoadedMode::Missing,
            "a stand-in mode is not remembered"
        );

        // The same move on an empty Claude tab is remembered, as before.
        state.borrow_mut().tabs.select(claude);
        assert_eq!(
            cycle_tab_mode(&state, claude, ViewId::BASE),
            Ok(eitri_core::tab_set::ModeCycle::Changed(SessionModeChoice::Auto))
        );
        assert_eq!(
            eitri_core::agent_prefs::load_mode(&prefs_dir, &project_dir),
            eitri_core::agent_prefs::LoadedMode::Remembered(SessionModeChoice::Auto)
        );
    }

    /// `cycle_default_mode`'s own reading of a tab's mode field, without going through
    /// `TabSet::cycle_mode` (which would change it): a thin accessor so the test above can assert
    /// on the untouched tab without reaching into a private field from two call sites.
    fn set_mode(state: &Rc<RefCell<AgentPanelState>>, tab: TabId) -> SessionModeChoice {
        state.borrow().tabs.get(tab).unwrap().mode()
    }

    /// `apply_confirm_bypass` -- the `ConfirmBypass` arm's WebView-free body -- moves a
    /// TAB's own mode (not only the window default the test above covers) and, like every path into
    /// bypass, never writes it to the prefs dir either way.
    #[test]
    fn apply_confirm_bypass_moves_a_live_tabs_mode_and_never_touches_prefs() {
        let dir = agent::state_dirs::test_workspace_dir("panel-apply-confirm-bypass-tab");
        let prefs_dir = dir.join("prefs");
        let project_dir = dir.join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = set.active();
        let state = state_for_hooks(set);
        state.borrow_mut().prefs_dir = Some(prefs_dir.clone());
        state.borrow_mut().project_dir = project_dir.clone();

        let cycled = state.borrow_mut().tabs.cycle_mode(tab, ViewId::BASE);
        let plan = match cycled {
            Ok(eitri_core::tab_set::ModeCycle::Confirm(plan)) => plan,
            other => panic!("an empty auto tab must ask first: {other:?}"),
        };
        assert_eq!(
            apply_confirm_bypass(&state, plan.scope, plan.nonce, ViewId::BASE),
            Ok(eitri_core::tab_set::ConfirmOutcome::Entered {
                approved: 0,
                resolved: vec![]
            })
        );
        assert_eq!(set_mode(&state, tab), SessionModeChoice::Bypass);
        assert_eq!(
            eitri_core::agent_prefs::load_mode(&prefs_dir, &project_dir),
            eitri_core::agent_prefs::LoadedMode::Missing,
            "bypass is never written, even the one apply_confirm_bypass itself enters"
        );
    }

    /// Entering bypass on a legacy tab approves its waiting cards through
    /// `respond_permission`, whose `PermissionResolved` exists nowhere but in what it returns (the CLI
    /// reports nothing for a hook reply). `ConfirmOutcome::Entered::resolved` now carries it, and the
    /// `ConfirmBypass` arm hands it to the panel through this function -- the only event that takes a
    /// card off the panel's screen (`reducer.ts`'s `permission_resolved`). A real
    /// `AgentBackend::Legacy` needs a real `claude` process, so the half that FILLS `resolved` is
    /// `eitri-core`'s `approve_pending_keeps_the_resolutions_a_legacy_answer_returns`.
    #[test]
    fn a_resolution_entering_bypass_returned_reaches_the_panel_payload() {
        let mut set = TabSet::new(BackendKind::Legacy, SessionModeChoice::Auto);
        let active = set.active();
        let background = set.open(eitri_core::tab_set::AgentId::Claude);
        set.select(active);
        let state = state_for_hooks(set);
        let resolved = vec![agent::AgentDomainEvent::PermissionResolved {
            permission_id: "perm-1".into(),
            outcome: agent::PermissionOutcome::Allowed,
        }];

        let (_, payload) = returned_events_payload(&state.borrow(), active, &resolved)
            .expect("the tab on screen is owed the resolution");
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["kind"], "events");
        assert_eq!(value["tab"], active.0);
        assert_eq!(value["events"][0]["type"], "permission_resolved");
        assert_eq!(value["events"][0]["permission_id"], "perm-1");
        assert_eq!(value["events"][0]["outcome"], "allowed");

        assert!(
            returned_events_payload(&state.borrow(), background, &resolved).is_none(),
            "a background tab catches up with a snapshot on its switch, never a stray batch"
        );
        assert!(returned_events_payload(&state.borrow(), active, &[]).is_none());
    }

    /// The pane losing the keys entirely -- the WebView-free half of
    /// `set_pane_focused(false)`, `TabSet::drop_bypass_prompt` -- drops an outstanding bypass
    /// prompt the same way switching tabs already does, so a `y` that slips through afterwards is
    /// refused by the nonce rather than landing on a tab no longer on screen.
    #[test]
    fn a_bypass_prompt_dropped_by_a_focus_loss_refuses_a_later_confirm() {
        let set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = set.active();
        let state = state_for_hooks(set);

        let cycled = state.borrow_mut().tabs.cycle_mode(tab, ViewId::BASE);
        let plan = match cycled {
            Ok(eitri_core::tab_set::ModeCycle::Confirm(plan)) => plan,
            other => panic!("an empty auto tab must ask first: {other:?}"),
        };

        // `AgentPanelHandle::set_pane_focused(false)` calls exactly this on the state it holds.
        state.borrow_mut().tabs.drop_bypass_prompt();

        assert_eq!(
            apply_confirm_bypass(&state, plan.scope, plan.nonce, ViewId::BASE),
            Err("that prompt is no longer current".to_string())
        );
        assert_eq!(set_mode(&state, tab), SessionModeChoice::Auto, "nothing changed");
    }

    /// Every worker is counted, and a worker that died (dropped its
    /// sender without reporting) neither hangs the collector nor counts as finished.
    #[test]
    fn the_close_collector_counts_every_worker_and_survives_one_dying() {
        let (tx, rx) = mpsc::channel();
        for n in 0..3 {
            let tx = tx.clone();
            std::thread::spawn(move || {
                if n == 1 {
                    drop(tx);
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(20 * n));
                let _ = tx.send(());
            });
        }
        drop(tx);
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done_tx.send(collect_reports(rx, 3));
        });
        let finished = done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the collector must not hang on a dead worker");
        assert_eq!(finished, 2);
    }

    /// The first tick dispatched `tabs` into a page that had not
    /// loaded. A window-level envelope goes out only once the document said `ready`, and one held
    /// back is not recorded as sent, so it still goes out once it is.
    #[test]
    fn a_window_envelope_waits_for_the_document_and_goes_out_once_per_change() {
        let mut last = None;
        assert_eq!(changed_envelope(&mut last, "tabs-1".into(), false), None);
        assert_eq!(last, None, "not recorded while the page cannot take it");
        assert_eq!(
            changed_envelope(&mut last, "tabs-1".into(), true),
            Some("tabs-1".into())
        );
        assert_eq!(changed_envelope(&mut last, "tabs-1".into(), true), None, "unchanged");
        assert_eq!(
            changed_envelope(&mut last, "tabs-2".into(), true),
            Some("tabs-2".into())
        );
    }

    #[test]
    fn ready_sends_the_history_and_the_editor_context_before_the_tabs() {
        let theme = eitri_core::agent_bridge::serialize_theme_for_js(&eitri_core::theme::ThemeTokens::fallback());
        let keymap = eitri_core::agent_bridge::serialize_keymap_for_js(
            "Ctrl+b",
            &[],
            &[],
            &eitri_core::keymap::panel::effective(&Default::default(), None).0,
            "Ctrl+b c",
            &[],
        );
        let window = vec![
            eitri_core::agent_bridge::serialize_history_for_js(&["earlier".into()]),
            eitri_core::agent_bridge::serialize_editor_context_for_js(None),
        ];
        let payloads = ready_payloads(
            legacy_greeting(),
            &[],
            window,
            r#"{"kind":"tabs","active":1,"tabs":[]}"#.to_string(),
            vec![],
            Some(&theme),
            Some(&keymap),
            None,
        );
        assert_eq!(
            kinds(&payloads),
            vec!["hello", "theme", "keymap", "history", "editor_context", "tabs"]
        );
    }

    #[test]
    fn ready_sends_the_editor_link_only_when_one_was_set() {
        let history = || eitri_core::agent_bridge::serialize_history_for_js(&[]);
        let context = || eitri_core::agent_bridge::serialize_editor_context_for_js(None);
        let none = window_payloads(history(), context(), None);
        assert_eq!(
            kinds(&none),
            vec!["history", "editor_context"],
            "the one-window list is unchanged"
        );
        let link = eitri_core::agent_bridge::serialize_editor_link_for_js("none", "no editor attached");
        let some = window_payloads(history(), context(), Some(&link));
        assert_eq!(kinds(&some), vec!["history", "editor_context", "editor_link"]);
    }

    /// Without `review.hint` a finished turn's hint is dropped before it is queued, so nothing is
    /// ever dispatched; with it, every hint passes.
    #[test]
    fn hints_are_dropped_unless_opted_in() {
        use eitri_core::turn_review::ReviewHint;
        let hints = || {
            vec![
                ReviewHint {
                    tab: 1,
                    turn: 1,
                    files: 3,
                },
                ReviewHint {
                    tab: 2,
                    turn: 1,
                    files: 0,
                },
            ]
        };
        assert!(hints_to_keep(false, hints()).is_empty());
        assert_eq!(hints_to_keep(true, hints()), hints());
    }

    /// A hint that arrives while the page is not ready is kept for it, one per tab, the newest.
    #[test]
    fn a_waiting_review_hint_is_replaced_only_by_the_same_tabs_newer_one() {
        use eitri_core::turn_review::ReviewHint;
        let hint = |tab, turn, files| ReviewHint { tab, turn, files };
        let mut waiting = Vec::new();
        keep_newest_hint_per_tab(&mut waiting, vec![hint(1, 1, 3), hint(2, 1, 1)]);
        keep_newest_hint_per_tab(&mut waiting, vec![hint(1, 2, 0)]);
        assert_eq!(waiting, vec![hint(2, 1, 1), hint(1, 2, 0)]);
        keep_newest_hint_per_tab(&mut waiting, Vec::new());
        assert_eq!(waiting.len(), 2);
    }

    /// What the page was sent is kept for the document that replaces it: a hint with files stands for its
    /// tab until a newer one, and one with none clears it.
    #[test]
    fn a_shown_review_hint_is_kept_until_a_newer_one_for_its_tab_replaces_or_clears_it() {
        use eitri_core::turn_review::ReviewHint;
        let hint = |tab, turn, files| ReviewHint { tab, turn, files };
        let mut shown = Vec::new();
        keep_shown_hints(&mut shown, &[hint(1, 1, 2), hint(2, 1, 5)]);
        keep_shown_hints(&mut shown, &[hint(1, 2, 3)]);
        assert_eq!(shown, vec![hint(2, 1, 5), hint(1, 2, 3)]);
        keep_shown_hints(&mut shown, &[hint(2, 2, 0)]);
        assert_eq!(shown, vec![hint(1, 2, 3)]);
        keep_shown_hints(&mut shown, &[]);
        assert_eq!(shown, vec![hint(1, 2, 3)]);
    }

    /// Opening a tab's review of the turn the hint is about, or a newer one, is what clears the hint on the
    /// page; a review of an older turn is not.
    #[test]
    fn asking_for_a_tabs_review_forgets_its_shown_hint_only_when_the_page_would_clear_it() {
        use eitri_core::agent_bridge::ReviewTurnWire;
        use eitri_core::turn_review::ReviewHint;
        let hint = |tab, turn, files| ReviewHint { tab, turn, files };
        let fresh = || vec![hint(1, 4, 2), hint(2, 4, 2)];
        let mut shown = fresh();
        forget_hint_for_review(&mut shown, 1, ReviewTurnWire::Latest);
        assert_eq!(shown, vec![hint(2, 4, 2)]);
        let mut shown = fresh();
        forget_hint_for_review(&mut shown, 1, ReviewTurnWire::N(3));
        assert_eq!(shown, fresh());
        forget_hint_for_review(&mut shown, 1, ReviewTurnWire::N(4));
        assert_eq!(shown, vec![hint(2, 4, 2)]);
    }

    /// A reloaded document is told again the hints the last one showed, for the tabs that still exist; a
    /// closed tab's is forgotten, and asking twice (two reloads) gives the same answer.
    #[test]
    fn a_new_document_is_owed_the_hints_the_last_one_showed() {
        use eitri_core::turn_review::ReviewHint;
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        let tab = state.borrow().tabs.active().0;
        let live = ReviewHint { tab, turn: 2, files: 3 };
        keep_shown_hints(
            &mut state.borrow_mut().shown_hints,
            &[
                live.clone(),
                ReviewHint {
                    tab: tab + 100,
                    turn: 1,
                    files: 1,
                },
            ],
        );
        assert_eq!(
            hints_for_new_document(&mut state.borrow_mut(), ViewId::BASE),
            vec![live.clone()]
        );
        assert_eq!(
            hints_for_new_document(&mut state.borrow_mut(), ViewId::BASE),
            vec![live]
        );
    }

    /// A review's answer is the worker's envelope as it is; a job that failed or whose worker died
    /// is a failing `command_result` for its own request; one still running is left in the list.
    #[test]
    fn finished_reviews_answer_each_request_once_and_leave_the_running_one() {
        let (done_tx, done_rx) = mpsc::channel();
        let (failed_tx, failed_rx) = mpsc::channel();
        let (died_tx, died_rx) = mpsc::channel::<Result<String, String>>();
        let (_running_tx, running_rx) = mpsc::channel::<Result<String, String>>();
        done_tx
            .send(Ok(r#"{"kind":"review","requestId":"a"}"#.to_string()))
            .unwrap();
        failed_tx.send(Err("no such turn".to_string())).unwrap();
        drop(died_tx);
        let mut jobs = vec![
            PendingReview {
                request_id: "a".into(),
                view: ViewId::BASE,
                rx: done_rx,
                clears_hint: Some((7, eitri_core::agent_bridge::ReviewTurnWire::Latest)),
            },
            PendingReview {
                request_id: "b".into(),
                view: ViewId::BASE,
                rx: failed_rx,
                clears_hint: Some((7, eitri_core::agent_bridge::ReviewTurnWire::Latest)),
            },
            PendingReview {
                request_id: "c".into(),
                view: ViewId::BASE,
                rx: died_rx,
                clears_hint: Some((7, eitri_core::agent_bridge::ReviewTurnWire::Latest)),
            },
            PendingReview {
                request_id: "d".into(),
                view: ViewId::BASE,
                rx: running_rx,
                clears_hint: Some((7, eitri_core::agent_bridge::ReviewTurnWire::Latest)),
            },
        ];
        let (payloads, cleared) = finished_reviews(&mut jobs);
        // Only the review that was worked out clears a hint; a failed or dead one leaves it on the page.
        assert_eq!(cleared, vec![(7, eitri_core::agent_bridge::ReviewTurnWire::Latest)]);
        assert_eq!(payloads.len(), 3);
        assert_eq!(payloads[0], r#"{"kind":"review","requestId":"a"}"#);
        let failed: serde_json::Value = serde_json::from_str(&payloads[1]).unwrap();
        assert_eq!(
            failed,
            serde_json::json!({ "kind": "command_result", "requestId": "b", "ok": false, "error": "no such turn" })
        );
        let died: serde_json::Value = serde_json::from_str(&payloads[2]).unwrap();
        assert_eq!(
            (died["requestId"].as_str(), died["ok"].as_bool()),
            (Some("c"), Some(false))
        );
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].request_id, "d");
        let (again, cleared_again) = finished_reviews(&mut jobs);
        assert!(again.is_empty() && cleared_again.is_empty(), "nothing is sent twice");
    }

    /// An editor that went away takes its pending draft edits with it: a body saved in nvim becomes
    /// the draft, an unsaved one leaves it alone, and the files are gone afterwards.
    #[test]
    fn ending_pending_edits_keeps_a_saved_body_and_removes_the_files() {
        let dir = std::env::temp_dir().join(format!("ap-{}-edits", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut set = TabSet::new(BackendKind::Sidecar, eitri_core::agent_bridge::SessionModeChoice::Auto);
        let saved = set.active();
        let unsaved = set.open(eitri_core::tab_set::AgentId::Claude);
        set.set_draft(saved, "old");
        set.set_draft(unsaved, "kept\n");
        let edit = |id: u64, body: &str| {
            let edit = eitri_core::scratch::PendingEdit {
                id,
                body: dir.join(format!("{id}-draft.md")),
                done: dir.join(format!("{id}-draft.done")),
            };
            std::fs::write(&edit.body, body).unwrap();
            edit
        };
        let (first, second) = (edit(1, "new text\n"), edit(2, "kept\n"));
        set.begin_scratch_edit(saved, 1).unwrap();
        set.begin_scratch_edit(unsaved, 2).unwrap();
        let state = state_for_hooks(set);
        state.borrow_mut().pending_edits = vec![first.clone(), second.clone()];

        let ended = end_pending_edits(&mut state.borrow_mut());

        assert_eq!(ended, vec![(saved, Some("new text".to_string())), (unsaved, None)]);
        let state = state.borrow();
        assert!(state.pending_edits.is_empty());
        assert_eq!(state.tabs.get(saved).unwrap().draft, "new text");
        assert_eq!(state.tabs.get(unsaved).unwrap().draft, "kept\n");
        assert!(
            !first.body.exists() && !second.body.exists(),
            "the scratch files are removed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The rule comes from what Rust offered for that card, never from the panel, and only
    /// with an allow.
    #[test]
    fn remember_saves_only_the_rule_rust_offered_and_only_with_an_allow() {
        use eitri_core::agent_bridge::DecisionChoice;
        let mut set = TabSet::new(BackendKind::Sidecar, eitri_core::agent_bridge::SessionModeChoice::Auto);
        let tab = set.active();
        set.get_mut(tab)
            .unwrap()
            .rule_offers
            .insert("perm-1".into(), agent::PrefixRule::parse("Bash(git push *)").unwrap());
        let t = set.get(tab).unwrap();
        assert_eq!(
            rule_to_remember(t, "perm-1", DecisionChoice::Allow, false, true),
            Ok(None),
            "a plain allow"
        );
        assert_eq!(
            rule_to_remember(t, "perm-1", DecisionChoice::Allow, true, true)
                .unwrap()
                .unwrap()
                .display(),
            "git push *"
        );
        assert!(
            rule_to_remember(t, "perm-2", DecisionChoice::Allow, true, true).is_err(),
            "never offered"
        );
        assert!(
            rule_to_remember(t, "perm-1", DecisionChoice::Deny, true, true).is_err(),
            "remember is an allow"
        );
    }

    /// A request that is no longer pending -- because it was
    /// already answered -- saves no rule, even though the offer Rust made for it may still be
    /// sitting in `rule_offers` (the offer is only recomputed once per pump tick).
    #[test]
    fn a_no_longer_pending_request_saves_no_rule_even_if_still_offered() {
        use eitri_core::agent_bridge::DecisionChoice;
        let mut set = TabSet::new(BackendKind::Sidecar, eitri_core::agent_bridge::SessionModeChoice::Auto);
        let tab = set.active();
        set.get_mut(tab)
            .unwrap()
            .rule_offers
            .insert("perm-1".into(), agent::PrefixRule::parse("Bash(git push *)").unwrap());
        let t = set.get(tab).unwrap();
        assert!(
            rule_to_remember(t, "perm-1", DecisionChoice::Allow, true, false).is_err(),
            "not pending any more, whatever the offer still says"
        );
    }

    /// The sidecar's own longer window, through the production route
    /// (`answer_permission_response`, the whole `permission_response` arm): `respond_permission`'s
    /// own resolution is not folded into the projection until the provider's real event arrives
    /// (`AgentConversation::respond_permission`'s own doc), so a projection read alone cannot catch
    /// a second response landing before that. The first successful answer, whatever its decision,
    /// drops the offer and marks the card answered (`TabSet::answer_card`), and the next pump, which recomputes offers because another id arrived, does
    /// not bring it back.
    #[test]
    fn an_answered_cards_offer_never_comes_back_and_a_replayed_remember_saves_nothing() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-permission-remember-race");
        let rules_dir = dir.join("rules");
        let mut set = TabSet::new(BackendKind::Sidecar, eitri_core::agent_bridge::SessionModeChoice::Auto);
        let tab = set.active();
        let (provider, backend) = live_backend(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let pump_until = |set: &mut TabSet, what: &str, done: &dyn Fn(&TabSet) -> bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !done(set) {
                assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
                set.pump(&dir, true);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        let bash = |id: &str, command: &str| agent::AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: None,
            tool_name: "Bash".into(),
            input: serde_json::json!({ "command": command }),
            provider_prompt: None,
        };
        let response = |id: &str, decision: &str, remember: bool| {
            let message = parse_inbound_message(&format!(
                r#"{{"type":"permission_response","request_id":"r","tab":{},"permission_id":"{id}","decision":"{decision}","remember":{remember}}}"#,
                tab.0
            ))
            .unwrap();
            PermissionAnswer::of(message).unwrap()
        };
        provider.queue(bash("perm-1", "git push origin main"));
        pump_until(&mut set, "the offer", &|set| {
            !set.get(tab).unwrap().rule_offers.is_empty()
        });
        assert_eq!(set.get(tab).unwrap().rule_offers["perm-1"].display(), "git push *");
        assert!(set.get(tab).unwrap().card_is_waiting("perm-1"));

        // The first response: a deny, never remembered, through the production route.
        let outcome = answer_permission_response(
            &mut set,
            Some(&rules_dir),
            &dir,
            tab,
            ViewId::BASE,
            response("perm-1", "deny", false),
        );
        assert!(matches!(outcome, Ok(Ok(_))));
        assert_eq!(provider.resolutions(), vec![("perm-1".to_string(), false)]);
        assert!(
            !set.get(tab).unwrap().rule_offers.contains_key("perm-1"),
            "the route itself drops the offer"
        );

        // A parallel call's card arrives before the provider's `PermissionResolved` for perm-1, so
        // the pump recomputes the offers from a projection that still lists perm-1 as pending.
        provider.queue(bash("perm-2", "git push origin other"));
        pump_until(&mut set, "the second card's offer", &|set| {
            set.get(tab).unwrap().rule_offers.contains_key("perm-2")
        });
        assert!(
            set.get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .pending_permissions
                .contains_key("perm-1"),
            "the premise: the sidecar has not folded perm-1's resolution yet"
        );
        let t = set.get(tab).unwrap();
        assert!(
            !t.rule_offers.contains_key("perm-1"),
            "the recompute must not bring back an answered card's offer"
        );
        assert!(!t.card_is_waiting("perm-1"), "answered, whatever the projection says");
        assert!(t.card_is_waiting("perm-2"));

        // A replayed "Always allow" for perm-1 (a tab switch or a reload re-drew the card): refused,
        // and no rule reaches the file.
        let replay = answer_permission_response(
            &mut set,
            Some(&rules_dir),
            &dir,
            tab,
            ViewId::BASE,
            response("perm-1", "allow", true),
        );
        assert_eq!(replay.map(|_| ()), Err("this request was already answered".to_string()));
        assert_eq!(
            eitri_core::permission_store::load(&rules_dir, &dir),
            eitri_core::permission_store::LoadedRules::Missing,
            "no rule was saved"
        );
        assert_eq!(provider.resolutions().len(), 1, "and nothing more reached the provider");
        for mut tab in set.take_all() {
            if let TabBackend::Live(backend) = &mut tab.backend {
                backend.shutdown();
            }
        }
    }

    #[test]
    fn the_detail_popover_counts_the_queue_and_names_the_rules_file() {
        let mut set = TabSet::new(BackendKind::Legacy, eitri_core::agent_bridge::SessionModeChoice::Auto);
        let tab = set.active();
        set.get_mut(tab)
            .unwrap()
            .queue
            .push(eitri_core::tab_set::Queued::held("a", "a".into(), 0));
        let rules = std::path::Path::new("/s/eitri/permissions/0123456789abcdef.json");
        let rows = detail_rows(
            set.active_tab(),
            BackendKind::Legacy,
            None,
            std::path::Path::new("/p"),
            Some(rules),
            "not checked yet",
            ProjectTrust::Untrusted,
            &[],
            Some("Space k"),
        );
        let value = |label: &str| rows.iter().find(|r| r.label == label).unwrap().value.clone();
        assert_eq!(value("queued"), "1");
        assert_eq!(value("permission rules"), rules.display().to_string());
        let rows = detail_rows(
            set.active_tab(),
            BackendKind::Legacy,
            None,
            std::path::Path::new("/p"),
            None,
            "not checked yet",
            ProjectTrust::Untrusted,
            &[],
            Some("Space k"),
        );
        assert_eq!(rows.iter().find(|r| r.label == "permission rules").unwrap().value, "—");
    }

    #[test]
    fn ready_sends_hello_theme_keymap_tabs_then_the_active_tabs_state() {
        let theme = eitri_core::agent_bridge::serialize_theme_for_js(&eitri_core::theme::ThemeTokens::fallback());
        let keymap = eitri_core::agent_bridge::serialize_keymap_for_js(
            "Ctrl+b",
            &[],
            &[],
            &eitri_core::keymap::panel::effective(&Default::default(), None).0,
            "Ctrl+b c",
            &[],
        );
        let snapshot = r#"{"kind":"snapshot","tab":1,"throughRevision":3,"state":{}}"#.to_string();
        let payloads = ready_payloads(
            legacy_greeting(),
            &[],
            vec![],
            r#"{"kind":"tabs","active":1,"tabs":[]}"#.to_string(),
            vec![snapshot],
            Some(&theme),
            Some(&keymap),
            None,
        );
        assert_eq!(kinds(&payloads), vec!["hello", "theme", "keymap", "tabs", "snapshot"]);
    }

    /// `hello` offers no session that is open in a tab, and still
    /// drops a session a tab handed off (the older rule, now per tab).
    #[test]
    fn hello_offers_no_session_that_is_open_in_a_tab() {
        let mut greeting = legacy_greeting();
        greeting.resumable = ["open-here", "free"]
            .iter()
            .map(|id| agent::ResumableSession {
                provider: "claude".into(),
                provider_session_id: id.to_string(),
                created_at: "1".into(),
                updated_at: "2".into(),
                title: None,
                name: None,
            })
            .collect();
        let payloads = ready_payloads(
            greeting,
            &["open-here".to_string()],
            vec![],
            "{}".to_string(),
            vec![],
            None,
            None,
            None,
        );
        let hello: serde_json::Value = serde_json::from_str(&payloads[0]).unwrap();
        let offered: Vec<&str> = hello["resumableSessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["providerSessionId"].as_str().unwrap())
            .collect();
        assert_eq!(offered, vec!["free"]);
    }

    #[test]
    fn the_chooser_lists_records_open_in_no_tab_and_marks_those_held_elsewhere() {
        let sessions: Vec<agent::ResumableSession> = ["open-here", "held", "free"]
            .iter()
            .map(|id| agent::ResumableSession {
                provider: "claude".into(),
                provider_session_id: id.to_string(),
                created_at: "1".into(),
                updated_at: "2".into(),
                title: Some(format!("about {id}")),
                name: None,
            })
            .collect();
        let records = chooser_records(&sessions, &["open-here".to_string()], |id| id == "held");
        let ids: Vec<(&str, bool)> = records
            .iter()
            .map(|r| (r.provider_session_id.as_str(), r.held_elsewhere))
            .collect();
        assert_eq!(ids, vec![("held", true), ("free", false)]);
    }

    #[test]
    fn the_detail_popover_names_every_identity_and_the_account() {
        let set = TabSet::new(BackendKind::Legacy, eitri_core::agent_bridge::SessionModeChoice::Bypass);
        let rows = detail_rows(
            set.active_tab(),
            BackendKind::Legacy,
            Some("work"),
            std::path::Path::new("/p"),
            None,
            "not checked yet",
            ProjectTrust::Untrusted,
            &[],
            Some("Space k"),
        );
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        for want in [
            "name",
            "title",
            "state",
            "mode",
            "model",
            "backend",
            "account",
            "cwd",
            "conversation",
            "verdandi session",
            "claude session",
            "CLI",
            "settings",
            "Verdandi revision",
            "resumable",
            "created",
            "updated",
            "queued",
            "permission rules",
        ] {
            assert!(labels.contains(&want), "missing {want}: {labels:?}");
        }
        let resumable = rows.iter().find(|r| r.label == "resumable").unwrap();
        assert_eq!(resumable.value, "no (legacy backend)");
        let account = rows.iter().find(|r| r.label == "account").unwrap();
        assert_eq!(account.value, "work");

        // What every session loads. The value is the sentence `build_create_request` and the legacy
        // spawn are held to, not a copy of it, and the row sits right after the CLI it describes.
        let settings = rows.iter().find(|r| r.label == "settings").unwrap();
        assert_eq!(
            settings.value,
            agent::setting_sources::note_for_session(true, ProjectTrust::Untrusted)
        );
        let position = |label: &str| labels.iter().position(|l| *l == label).unwrap();
        assert_eq!(position("settings"), position("CLI") + 1, "{labels:?}");
    }

    /// The `settings` row is a fact about every session, so it does not depend on there being one:
    /// an empty tab (no backend, no provider info) says it too, and on either backend.
    #[test]
    fn the_settings_row_is_the_same_on_an_empty_tab_and_on_either_backend() {
        for kind in [BackendKind::Legacy, BackendKind::Sidecar] {
            let set = TabSet::new(kind, eitri_core::agent_bridge::SessionModeChoice::Auto);
            let rows = detail_rows(
                set.active_tab(),
                kind,
                None,
                std::path::Path::new("/p"),
                None,
                "not checked yet",
                ProjectTrust::Untrusted,
                &[],
                Some("Space k"),
            );
            let settings = rows
                .iter()
                .find(|r| r.label == "settings")
                .expect("the row is always drawn");
            assert_eq!(
                settings.value,
                agent::setting_sources::note_for_session(true, ProjectTrust::Untrusted),
                "{}",
                kind.as_str()
            );
        }
    }

    /// What each tick of the close watch decides, including the two orderings that are judgements
    /// rather than mechanics.
    ///
    /// This is the whole of what `shell`'s own suite can say about the close path: it cannot open a
    /// window, so it cannot exercise `AgentPanelHandle::shutdown`, the application hold, or GTK's
    /// window destruction. What a test can pin is that no tick misreports what it saw.
    #[test]
    fn a_close_watch_tick_reports_what_it_saw() {
        // Still running, still inside the backstop: the application stays held.
        assert_eq!(
            close_watch_step::<()>(&Err(mpsc::TryRecvError::Empty), false),
            CloseWatch::KeepHolding
        );
        // Still running, past the deadline: the process gives up on it.
        assert_eq!(
            close_watch_step::<()>(&Err(mpsc::TryRecvError::Empty), true),
            CloseWatch::Expired
        );
        // A report ARRIVING on the same tick as the deadline is a report. A teardown that finished
        // must never be logged as an abandoned one -- that line names a possible orphaned `claude`.
        assert_eq!(close_watch_step(&Ok(()), true), CloseWatch::Finished);
        assert_eq!(close_watch_step(&Ok(()), false), CloseWatch::Finished);
        // A dead worker is a dead worker at any point in the window, never a timeout: the two send
        // a debugger to completely different places (a panic in `shutdown()` vs. a hang in it).
        assert_eq!(
            close_watch_step::<()>(&Err(mpsc::TryRecvError::Disconnected), true),
            CloseWatch::WorkerDied
        );
        assert_eq!(
            close_watch_step::<()>(&Err(mpsc::TryRecvError::Disconnected), false),
            CloseWatch::WorkerDied
        );
    }

    /// A refused command still delivers what the backend already folded.
    ///
    /// **Scope, stated because the name would otherwise overclaim it.** This pins `events_owed`,
    /// the rule `apply_command_outcome` now reads the events off -- not a real refused legacy send.
    /// It cannot: the only way to get an `AgentBackend::Legacy` is `AgentSession::start` ->
    /// `AgentProcess::spawn`, which spawns a real `claude` process, and there is no injectable
    /// provider on that path the way `AgentConversation` has one (`core/src/agent_backend.rs`'s own
    /// test doc records that asymmetry, and that the Legacy arm of `send_turn` is dead under
    /// `cargo test` for the same reason). So the arm that POPULATES `folded_events` is unverified
    /// by any automated test in this workspace, on purpose rather than by oversight; what is
    /// verified is that a populated one is delivered instead of dropped, which is the half that was
    /// broken.
    #[test]
    fn a_failed_command_still_owes_the_events_its_backend_already_folded() {
        let folded = vec![agent::AgentDomainEvent::UserPromptSubmitted {
            text: "what does this do?".into(),
        }];
        let refused: Result<Vec<agent::AgentDomainEvent>, _> = Err(eitri_core::agent_backend::BackendError {
            message: "a turn is already in progress".into(),
            benign: true,
            folded_events: folded.clone(),
        });
        assert_eq!(
            events_owed(&refused),
            folded.as_slice(),
            "a refusal must not swallow a folded prompt"
        );

        // The other two shapes, so this is a rule rather than one case: a success delivers its own
        // events, and a failure that folded nothing delivers nothing.
        assert_eq!(events_owed(&Ok(folded.clone())), folded.as_slice());
        assert!(events_owed(&Err(no_session_error())).is_empty());
    }

    /// No source that builds the panel document or its policy may allow inline script by keyword:
    /// the Rust sources of this crate, and the web frontend's own `index.html` and sources. Matches
    /// on content only, so where a line ends does not matter; the needle is split so this test does
    /// not find itself.
    #[test]
    fn no_shell_source_allows_inline_script_by_keyword() {
        use eitri_core::source_scan as scan;
        const SCANNED: &[&str] = &["shell/src", "panel/src"];
        const WEB: &[&str] = &["agent-ui/web/src"];
        let needle = concat!("script-src ", "'unsafe-inline'");
        let rust = scan::rust_sources(SCANNED);
        scan::require_anchors(&rust, &["fn content_security_policy_for(", "impl PanelDocument"]);
        let web = scan::sources(WEB, &["ts", "tsx", "js", "jsx", "css", "html"]);
        let page = scan::read_required("agent-ui/web/index.html");
        // The raw text, not the code: the policy is a string literal.
        let offenders: Vec<&str> = rust
            .iter()
            .chain(&web)
            .chain(std::iter::once(&page))
            .filter(|source| source.text.contains(needle))
            .map(|source| source.path.as_str())
            .collect();
        assert!(
            offenders.is_empty(),
            "inline script allowed by keyword in: {offenders:?}"
        );
    }

    fn a_command() -> agent::handoff::ClaudeResumeCommand {
        agent::handoff::ClaudeResumeCommand::for_session("/home/user/project", "1857dcd5-973b-46a2").unwrap()
    }

    fn legacy_greeting() -> BackendGreeting {
        BackendGreeting::for_kind(BackendKind::Legacy, PathBuf::from("/home/user/project"))
    }

    fn kinds(payloads: &[String]) -> Vec<String> {
        payloads
            .iter()
            .map(|p| {
                serde_json::from_str::<serde_json::Value>(p).unwrap()["kind"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    /// The whole point of holding the command in Rust. A document that reloads -- Ctrl+Shift+R, the
    /// top bar's `⟳`, or a WebKit crash -- gets the command back, because the panel still has it.
    ///
    /// The backend here is `legacy` deliberately: it is the default, and it is the case where the
    /// loss was unrecoverable. The first assert states why, from the greeting's own payload -- legacy
    /// offers no resume, so `hello` carries no copy of this id and nothing else in the process does
    /// either.
    #[test]
    fn a_reloaded_document_is_handed_back_the_command_the_old_one_was_showing() {
        let command = a_command();
        let payloads = ready_payloads(
            legacy_greeting(),
            &[],
            vec![],
            "{\"kind\":\"tabs\"}".to_string(),
            vec![eitri_core::agent_bridge::serialize_handoff_for_js(TabId(1), &command)],
            None,
            None,
            None,
        );
        let hello: serde_json::Value = serde_json::from_str(&payloads[0]).unwrap();
        assert!(
            hello["resumableSession"].is_null(),
            "legacy has no other record of this session"
        );

        assert_eq!(kinds(&payloads), vec!["hello", "tabs", "handoff"]);
        let handoff: serde_json::Value = serde_json::from_str(&payloads[2]).unwrap();
        assert_eq!(
            handoff["command"],
            "cd /home/user/project && claude --resume 1857dcd5-973b-46a2"
        );
        assert_eq!(handoff["providerSessionId"], "1857dcd5-973b-46a2");
    }

    /// The ordinary case: a panel that handed nothing off sends `hello` and the tab bar alone.
    #[test]
    fn a_panel_that_handed_nothing_off_sends_only_the_greeting_and_the_tabs() {
        assert_eq!(
            kinds(&ready_payloads(
                legacy_greeting(),
                &[],
                vec![],
                "{\"kind\":\"tabs\"}".to_string(),
                vec![],
                None,
                None,
                None
            )),
            vec!["hello", "tabs"]
        );
    }

    /// The first
    /// `ready` draws the empty tab's dashboard, never the chooser -- `ready_payloads` is all a `ready` sends now.
    #[test]
    fn a_ready_sends_no_chooser() {
        assert!(!kinds(&ready_payloads(
            legacy_greeting(),
            &[],
            vec![],
            "{\"kind\":\"tabs\"}".to_string(),
            vec![],
            None,
            None,
            None
        ))
        .contains(&"chooser".to_string()));
    }

    /// A reloaded document must get its colours back before anything it would draw with them.
    #[test]
    fn the_theme_follows_the_greeting_and_precedes_everything_else() {
        let theme = eitri_core::agent_bridge::serialize_theme_for_js(&eitri_core::theme::ThemeTokens::fallback());
        assert_eq!(
            kinds(&ready_payloads(
                legacy_greeting(),
                &[],
                vec![],
                "{\"kind\":\"tabs\"}".to_string(),
                vec![],
                Some(&theme),
                None,
                None
            )),
            vec!["hello", "theme", "tabs"]
        );
        let payloads = ready_payloads(
            legacy_greeting(),
            &[],
            vec![],
            "{\"kind\":\"tabs\"}".to_string(),
            vec![r#"{"kind":"snapshot","tab":1,"throughRevision":3,"state":{}}"#.to_string()],
            Some(&theme),
            None,
            None,
        );
        assert_eq!(kinds(&payloads), vec!["hello", "theme", "tabs", "snapshot"]);
    }

    /// The `?` overlay's rows come with every `ready`, right after the colours, so a reloaded
    /// document never shows a keymap it was not told.
    #[test]
    fn the_keymap_follows_the_theme() {
        let theme = eitri_core::agent_bridge::serialize_theme_for_js(&eitri_core::theme::ThemeTokens::fallback());
        let keymap = eitri_core::agent_bridge::serialize_keymap_for_js(
            "Ctrl+b",
            &[],
            &[],
            &eitri_core::keymap::panel::effective(&Default::default(), None).0,
            "Ctrl+b c",
            &[],
        );
        assert_eq!(
            kinds(&ready_payloads(
                legacy_greeting(),
                &[],
                vec![],
                "{\"kind\":\"tabs\"}".to_string(),
                vec![],
                Some(&theme),
                Some(&keymap),
                None
            )),
            vec!["hello", "theme", "keymap", "tabs"]
        );
    }

    /// A session handed to a terminal must not also be offered for resume on the start screen --
    /// accepting that offer would put two writers on one transcript, which is exactly what the card
    /// beside it warns about. This is the case the frontend's own suppression cannot cover: `hello`
    /// is recomputed here on every mount, and `agent::resumable_session` picks the most recently
    /// updated record, which right after a handoff is the handed-over session itself.
    #[test]
    fn a_handoff_drops_exactly_the_session_it_gave_away_and_no_other() {
        // One list, two rows: the session just handed to a terminal, and an unrelated one the
        // workspace also remembers. Before the picker landed, `resumable` held at most one record
        // and these were two tests that could not tell "dropped the right row" from "cleared the
        // offer" -- the distinction only becomes testable when the list can hold both at once.
        let greeting = BackendGreeting {
            kind: BackendKind::Sidecar,
            project_dir: PathBuf::from("/home/user/project"),
            permission_modes: eitri_core::agent_backend::CLIENT_IMPLEMENTED_PERMISSION_MODES,
            expected_verdandi_revision: None,
            resumable: vec![
                agent::ResumableSession {
                    provider: "claude".to_string(),
                    provider_session_id: "1857dcd5-973b-46a2".to_string(),
                    created_at: "1757600000000".to_string(),
                    updated_at: "1757700000000".to_string(),
                    title: None,
                    name: None,
                },
                agent::ResumableSession {
                    provider: "claude".to_string(),
                    provider_session_id: "some-other-session".to_string(),
                    created_at: "1757500000000".to_string(),
                    updated_at: "1757600000000".to_string(),
                    title: None,
                    name: None,
                },
            ],
            account: None,
        };
        let command = a_command();
        // The tab set's `open_session_ids` includes a handed-off session.
        let open = vec![command.provider_session_id().to_string()];
        let payloads = ready_payloads(greeting, &open, vec![], "{}".to_string(), vec![], None, None, None);
        let hello: serde_json::Value = serde_json::from_str(&payloads[0]).unwrap();
        let offered: Vec<&str> = hello["resumableSessions"]
            .as_array()
            .expect("the offer is a list")
            .iter()
            .map(|s| s["providerSessionId"].as_str().unwrap())
            .collect();
        assert_eq!(
            offered,
            vec!["some-other-session"],
            "a handoff must drop the session it gave away -- and only that one. Nothing holds a \
             lock on it, so re-offering it would put two writers on one transcript; clearing the \
             whole list instead would hide a conversation nobody gave away."
        );
    }

    /// The sequencing the whole feature's honesty rests on: while the close worker has not reported,
    /// the tick decides `StillClosing` -- and `handoff_payloads` dispatches nothing at all for it.
    /// A real channel across a real send, not a mocked one.
    #[test]
    fn the_command_is_not_released_until_the_close_worker_has_reported() {
        let (closed_tx, closed_rx) = mpsc::channel();
        assert_eq!(classify_close_signal(&closed_rx), HandoffCloseOutcome::StillClosing);
        assert!(handoff_payloads(
            TabId(1),
            &HandoffCloseOutcome::StillClosing,
            Some("req-1"),
            &a_command()
        )
        .is_empty());

        closed_tx.send(()).unwrap();
        assert_eq!(classify_close_signal(&closed_rx), HandoffCloseOutcome::Closed);
        assert_eq!(
            kinds(&handoff_payloads(
                TabId(1),
                &HandoffCloseOutcome::Closed,
                Some("req-1"),
                &a_command()
            )),
            vec!["handoff", "command_result"]
        );
    }

    /// A worker that panicked inside `shutdown()` drops its sender without sending. That is a failed
    /// close, not a finished one -- and a failed close must NOT be followed by a card whose first
    /// line says the conversation is closed.
    #[test]
    fn a_close_that_died_partway_reports_a_dead_session_rather_than_the_command() {
        let (closed_tx, closed_rx) = mpsc::channel::<()>();
        drop(closed_tx);
        assert_eq!(classify_close_signal(&closed_rx), HandoffCloseOutcome::CloseFailed);

        let payloads = handoff_payloads(TabId(1), &HandoffCloseOutcome::CloseFailed, Some("req-2"), &a_command());
        assert_eq!(kinds(&payloads), vec!["command_result", "error"]);
        assert!(
            !payloads.iter().any(|p| p.contains("1857dcd5-973b-46a2")),
            "a failed close leaked the resume command anyway: {payloads:?}"
        );
    }

    // ---- bringing the last window's tabs back --------------------------------------------------

    /// A project directory with resumable records for `records` and a panel state whose last window
    /// left `saved` open.
    fn restore_panel(
        label: &str,
        saved: &[(&str, Option<&str>, SessionModeChoice)],
        records: &[&str],
    ) -> (Rc<RefCell<AgentPanelState>>, PathBuf) {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir(label);
        let conversation_id = agent::conversation_id_for_cwd(&dir);
        for id in records {
            agent::persistence::save_conversation_record(&agent::persistence::ConversationRecord {
                conversation_id: conversation_id.clone(),
                provider: "claude".to_string(),
                provider_session_id: id.to_string(),
                canonical_cwd: dir.to_string_lossy().into_owned(),
                created_at: "1".to_string(),
                updated_at: "2".to_string(),
                provider_advertised_resume: true,
                title: Some(format!("title of {id}")),
                name: None,
            })
            .unwrap();
        }
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        {
            let mut s = state.borrow_mut();
            s.project_dir = dir.clone();
            s.canonical_project_dir = dir.to_string_lossy().into_owned();
            s.conversation_id = conversation_id.clone();
            // A real gate over the project, its home the directory above it and its record kept
            // beside it, so the restore waits for the check as it does in a window.
            let workspaces = dir.parent().unwrap().to_path_buf();
            let records = workspaces.join(format!("{}.trust", dir.file_name().unwrap().to_string_lossy()));
            s.trust = TrustGate::new(
                eitri_core::project_trust::TrustStore::at(Some(records)),
                dir.clone(),
                Some(workspaces),
            );
            s.restore_source = Some(SavedTabs {
                tabs: saved
                    .iter()
                    .map(|(id, name, mode)| eitri_core::saved_tabs::SavedTab {
                        conversation_id: conversation_id.clone(),
                        provider_session_id: id.to_string(),
                        name: name.map(str::to_string),
                        mode: *mode,
                        view: None,
                    })
                    .collect(),
                active: 0,
                ..SavedTabs::default()
            });
        }
        (state, dir)
    }

    /// Spawns nothing: records each session it is asked to resume and the lease it was handed, and
    /// keeps the sender so the test decides how that resume ends.
    #[derive(Default)]
    struct FakeResumes {
        asked: Vec<String>,
        leases: Vec<agent::lease::SessionLease>,
        trusts: Vec<ProjectTrust>,
        senders: Vec<mpsc::Sender<Result<AgentBackend, BackendError>>>,
        held_when_first_asked: Option<Vec<bool>>,
        all: Vec<String>,
    }

    impl FakeResumes {
        fn spawner(
            &mut self,
            canonical: String,
        ) -> impl FnMut(
            &str,
            agent::lease::SessionLease,
            ProjectTrust,
        ) -> mpsc::Receiver<Result<AgentBackend, BackendError>>
               + '_ {
            move |id, lease, project| {
                if self.held_when_first_asked.is_none() {
                    self.held_when_first_asked = Some(
                        self.all
                            .iter()
                            .map(|other| {
                                agent::lease::SessionLease::is_held("claude", &canonical, other).unwrap_or(false)
                            })
                            .collect(),
                    );
                }
                self.asked.push(id.to_string());
                self.leases.push(lease);
                self.trusts.push(project);
                let (tx, rx) = mpsc::channel();
                self.senders.push(tx);
                rx
            }
        }
    }

    fn ask(state: &Rc<RefCell<AgentPanelState>>, resumes: &mut FakeResumes, how: RestoreAsk) -> RestoreResult {
        let result = {
            let mut s = state.borrow_mut();
            let from = s.tabs.active();
            ask_for_restore(&mut s, from, how)
        };
        settle(state, resumes, result)
    }

    fn answer(
        state: &Rc<RefCell<AgentPanelState>>,
        resumes: &mut FakeResumes,
        nonce: u64,
        keep: bool,
    ) -> RestoreResult {
        let result = answer_restore_question(&mut state.borrow_mut(), nonce, keep);
        settle(state, resumes, result)
    }

    /// Puts `resumes` in as the panel's spawner for as long as `f` runs, and takes it back after.
    fn with_resumes<T>(state: &Rc<RefCell<AgentPanelState>>, resumes: &mut FakeResumes, f: impl FnOnce() -> T) -> T {
        let canonical = state.borrow().canonical_project_dir.clone();
        let shared = Rc::new(RefCell::new(std::mem::take(resumes)));
        let fake = shared.clone();
        state.borrow_mut().spawner = Spawner::Fake(Rc::new(RefCell::new(move |call: SpawnCall| match call {
            SpawnCall::Resume { id, lease, project } => {
                let mut fake = fake.borrow_mut();
                let mut spawn = fake.spawner(canonical.clone());
                spawn(&id, lease, project)
            }
            SpawnCall::Connect { .. } => panic!("a restore resumes; it never connects afresh"),
            SpawnCall::External { .. } => panic!("a restore starts only Claude tabs"),
        })));
        let result = f();
        state.borrow_mut().spawner = spawn_nothing();
        *resumes = Rc::try_unwrap(shared)
            .ok()
            .expect("the spawner was put back")
            .into_inner();
        result
    }

    /// The gate's tick, run until `done` says so (at most two seconds), with every step's output.
    fn tick_until(state: &Rc<RefCell<AgentPanelState>>, done: impl Fn(&[TrustOut]) -> bool) -> Vec<TrustOut> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut seen = Vec::new();
        loop {
            seen.push(trust_events(&mut state.borrow_mut()));
            if done(&seen) {
                return seen;
            }
            assert!(std::time::Instant::now() < deadline, "the gate never got there");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// A restore the gate holds is let through once its check comes back (these roots have nothing to
    /// trust), with `resumes` as the spawner; what `start_restore` said is the result.
    fn settle(state: &Rc<RefCell<AgentPanelState>>, resumes: &mut FakeResumes, result: RestoreResult) -> RestoreResult {
        if !matches!(result, RestoreResult::Deferred) {
            return result;
        }
        let seen = with_resumes(state, resumes, || {
            tick_until(state, |seen| seen.iter().any(|o| o.restored.is_some()))
        });
        let notice = seen.into_iter().find_map(|o| o.restored).expect("restored");
        RestoreResult::Started { notice }
    }

    fn started(result: RestoreResult) -> Option<String> {
        match result {
            RestoreResult::Started { notice } => notice,
            RestoreResult::Refused(why) => panic!("refused: {why}"),
            RestoreResult::Question(prompt) => panic!("asked: {:?}", prompt.lines),
            RestoreResult::Deferred => panic!("still waiting for the trust question"),
        }
    }

    fn mode(state: &Rc<RefCell<AgentPanelState>>, session: &str) -> SessionModeChoice {
        let s = state.borrow();
        let tab = s
            .tabs
            .tabs()
            .iter()
            .find(|t| t.provider_session_id().as_deref() == Some(session))
            .map(|t| t.id)
            .unwrap();
        s.tabs.get(tab).unwrap().mode()
    }

    use SessionModeChoice::{Auto, Bypass};

    #[test]
    fn the_dashboard_offers_the_saved_tabs_that_can_still_come_back() {
        let (state, _dir) = restore_panel(
            "restore-offer",
            &[
                ("s-a", None, Auto),
                ("s-b", Some("api"), Bypass),
                ("s-gone", None, Auto),
            ],
            &["s-a", "s-b"],
        );
        let s = state.borrow();
        assert!(restore_is_on_offer(&s));
        let offer = restore_offer(&s).expect("two tabs can come back");
        assert_eq!(offer.labels, vec!["title of s-a".to_string(), "api".to_string()]);
        assert_eq!(offer.bypass, 1);
    }

    /// The greeting lists the active tab's agent's sessions, and a switch to a tab running another
    /// agent sends a new one though no session id changed; the restore offer still matches the saved
    /// Claude tabs with Claude's records whichever agent the active tab runs.
    #[test]
    fn hello_follows_the_active_tabs_agent_and_the_offer_keeps_the_claude_tabs() {
        let (state, dir) = restore_panel("hello-agent", &[("s-a", None, Auto)], &["s-a"]);
        agent::persistence::save_conversation_record(&agent::persistence::ConversationRecord {
            conversation_id: agent::conversation_id_for_cwd(&dir),
            provider: AgentId::Codex.record_name(),
            provider_session_id: "thread-1".to_string(),
            canonical_cwd: dir.to_string_lossy().into_owned(),
            created_at: "1".to_string(),
            updated_at: "3".to_string(),
            provider_advertised_resume: true,
            title: None,
            name: None,
        })
        .unwrap();
        let page = Rc::new(RecordingPage::default());
        let handle = AgentPanelHandle::over(
            state.clone(),
            Some(Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>))),
        );
        let last_hello = || {
            let sent = page.sent.borrow();
            let hello = sent.iter().rev().find(|e| kind_of(e) == "hello").expect("a hello");
            let hello: serde_json::Value = serde_json::from_str(hello).unwrap();
            let ids: Vec<String> = hello["resumableSessions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["providerSessionId"].as_str().unwrap().to_string())
                .collect();
            (ids, hello["restore"]["labels"].clone())
        };
        let hellos = || page.sent.borrow().iter().filter(|e| kind_of(e) == "hello").count();
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        assert_eq!(
            last_hello(),
            (vec!["s-a".to_string()], serde_json::json!(["title of s-a"]))
        );

        let before = hellos();
        send_hello_if_open_sessions_changed(&state, &handle.pages);
        assert_eq!(hellos(), before, "nothing changed, nothing is sent");

        let claude_tab = state.borrow().tabs.active();
        state.borrow_mut().tabs.open(AgentId::Codex);
        send_hello_if_open_sessions_changed(&state, &handle.pages);
        assert_eq!(hellos(), before + 1);
        assert_eq!(
            last_hello(),
            (vec!["thread-1".to_string()], serde_json::json!(["title of s-a"])),
            "the Codex tab offers Codex's sessions, and the saved Claude tab can still come back"
        );

        state.borrow_mut().tabs.select(claude_tab);
        send_hello_if_open_sessions_changed(&state, &handle.pages);
        assert_eq!(hellos(), before + 2);
        assert_eq!(
            last_hello(),
            (vec!["s-a".to_string()], serde_json::json!(["title of s-a"]))
        );
    }

    #[test]
    fn nothing_is_offered_after_a_session_started_under_another_policy_or_when_nothing_would_come_back() {
        let (state, _dir) = restore_panel("restore-not-offered", &[("s-a", None, Auto)], &["s-a"]);
        assert!(restore_offer(&state.borrow()).is_some());

        state.borrow_mut().restore_policy = RestorePolicy::Auto;
        assert!(
            restore_offer(&state.borrow()).is_none(),
            "auto restores at launch, it does not offer"
        );
        state.borrow_mut().restore_policy = RestorePolicy::Off;
        assert!(restore_offer(&state.borrow()).is_none());
        state.borrow_mut().restore_policy = RestorePolicy::Offer;

        // A tab that has started a session: this is no longer a launch.
        let started_tab = state.borrow().tabs.active();
        let (_tx, rx) = mpsc::channel();
        state.borrow_mut().tabs.get_mut(started_tab).unwrap().backend = TabBackend::Starting(PendingStart {
            request_id: "r".into(),
            result_rx: rx,
            first_turn: None,
            resume: None,
            resumed_title: None,
            resumed_name: None,
        });
        assert!(restore_offer(&state.borrow()).is_none());
        state.borrow_mut().tabs.get_mut(started_tab).unwrap().backend = TabBackend::NotStarted;
        assert!(restore_offer(&state.borrow()).is_some());

        // Every saved tab skipped: a key that restores nothing is not offered.
        let (nothing, _dir) = restore_panel("restore-nothing", &[("s-x", None, Auto)], &[]);
        assert!(restore_offer(&nothing.borrow()).is_none());
        // And no file, no offer.
        nothing.borrow_mut().restore_source = None;
        assert!(!restore_is_on_offer(&nothing.borrow()));
    }

    /// Every lease is held before the first resume is begun, so nothing can claim a saved session
    /// -- or prune its record -- in between.
    #[test]
    fn every_lease_is_taken_before_the_first_resume_begins() {
        let (state, dir) = restore_panel(
            "restore-leases",
            &[("s-1", None, Auto), ("s-2", None, Auto), ("s-3", None, Auto)],
            &["s-1", "s-2", "s-3"],
        );
        let canonical = dir.to_string_lossy().into_owned();
        let mut resumes = FakeResumes {
            all: vec!["s-1".into(), "s-2".into(), "s-3".into()],
            ..Default::default()
        };
        assert_eq!(started(ask(&state, &mut resumes, RestoreAsk::Dashboard)), None);
        assert_eq!(resumes.asked, ["s-1", "s-2", "s-3"], "in order");
        assert_eq!(resumes.held_when_first_asked, Some(vec![true, true, true]));
        assert_eq!(resumes.leases.len(), 3);
        assert_eq!(
            resumes.trusts,
            [ProjectTrust::Untrusted; 3],
            "no restored session loads the project's own configuration"
        );
        assert!(state.borrow().restore_source.is_none(), "used up");
        let numbers: Vec<Option<String>> = state
            .borrow()
            .tabs
            .tabs()
            .iter()
            .map(eitri_core::tab_set::Tab::provider_session_id)
            .collect();
        assert_eq!(numbers, ["s-1", "s-2", "s-3"].map(|s| Some(s.to_string())));
        drop(resumes.leases);
        assert!(!agent::lease::SessionLease::is_held("claude", &canonical, "s-1").unwrap());
    }

    #[test]
    fn a_session_another_window_takes_after_the_offer_is_skipped_and_said() {
        let (state, dir) = restore_panel(
            "restore-late-lease",
            &[("s-1", None, Bypass), ("s-2", None, Auto)],
            &["s-1", "s-2"],
        );
        let canonical = dir.to_string_lossy().into_owned();
        let mut resumes = FakeResumes::default();
        let RestoreResult::Question(prompt) = ask(&state, &mut resumes, RestoreAsk::Dashboard) else {
            panic!("a bypass tab is asked about")
        };
        // Another window opens s-2 while the question is on screen.
        let theirs = agent::lease::SessionLease::try_acquire("claude", &canonical, "s-2").unwrap();
        started(answer(&state, &mut resumes, prompt.nonce, true));
        assert_eq!(resumes.asked, ["s-1"], "only the one that could be leased was begun");

        let toasts = Rc::new(RefCell::new(Vec::<String>::new()));
        state.borrow_mut().toast_hook = Some({
            let toasts = toasts.clone();
            Rc::new(move |text: &str| toasts.borrow_mut().push(text.to_string()))
        });
        let (_provider, backend) = live_backend(&dir);
        resumes.senders[0].send(Ok(backend)).unwrap();
        let collected = state.borrow_mut().tabs.collect_starts();
        for result in &collected {
            account_for_restore(&mut state.borrow_mut(), result);
        }
        finish_restore(&state);
        assert_eq!(
            *toasts.borrow(),
            ["Restored 1 of 2 tabs; 1 could not be: title of s-2 (open in another window)"]
        );
        drop(theirs);
        for mut tab in state.borrow_mut().tabs.take_all() {
            if let TabBackend::Live(backend) = &mut tab.backend {
                backend.shutdown();
            }
        }
    }

    #[test]
    fn a_saved_bypass_tab_asks_first_and_each_answer_has_its_own_result() {
        let (state, _dir) = restore_panel(
            "restore-bypass-ask",
            &[("s-1", None, Auto), ("s-2", None, Bypass)],
            &["s-1", "s-2"],
        );
        let mut resumes = FakeResumes::default();
        let RestoreResult::Question(prompt) = ask(&state, &mut resumes, RestoreAsk::Dashboard) else {
            panic!("asked")
        };
        assert_eq!(prompt.lines[0], "Restore 2 tabs (1 in bypass)? y/n");
        assert!(resumes.asked.is_empty(), "nothing begins until the answer");
        assert!(
            state.borrow().restore_source.is_some(),
            "and the offer is still there if it is cancelled"
        );
        assert!(matches!(
            answer(&state, &mut resumes, prompt.nonce + 1, true),
            RestoreResult::Refused(_)
        ));

        assert_eq!(
            started(answer(&state, &mut resumes, prompt.nonce, false)),
            None,
            "an answered n needs no second telling"
        );
        assert_eq!(mode(&state, "s-2"), Auto, "n: the tab comes back in auto");
        assert_eq!(state.borrow().tabs.default_mode(), Auto);

        let (yes, _dir) = restore_panel(
            "restore-bypass-yes",
            &[("s-1", None, Auto), ("s-2", None, Bypass)],
            &["s-1", "s-2"],
        );
        let mut resumes = FakeResumes::default();
        let RestoreResult::Question(prompt) = ask(&yes, &mut resumes, RestoreAsk::Dashboard) else {
            panic!("asked")
        };
        started(answer(&yes, &mut resumes, prompt.nonce, true));
        assert_eq!(mode(&yes, "s-2"), Bypass, "y: as it was");
        assert_eq!(mode(&yes, "s-1"), Auto);
        assert_eq!(
            yes.borrow().tabs.default_mode(),
            Auto,
            "the window's own default never moves"
        );
    }

    #[test]
    fn a_restore_at_launch_asks_nothing_downgrades_bypass_and_says_so() {
        let (state, _dir) = restore_panel(
            "restore-launch",
            &[("s-1", None, Bypass), ("s-2", None, Bypass)],
            &["s-1", "s-2"],
        );
        state.borrow_mut().restore_policy = RestorePolicy::Auto;
        let mut resumes = FakeResumes::default();
        let notice = started(ask(&state, &mut resumes, RestoreAsk::Launch)).expect("the band names it");
        assert_eq!(notice, "2 bypass tabs came back in auto; Shift+Tab then y switches");
        assert_eq!(mode(&state, "s-1"), Auto);
        assert_eq!(mode(&state, "s-2"), Auto);
        assert_eq!(resumes.asked.len(), 2);
    }

    #[test]
    fn a_launch_has_one_go_whatever_comes_of_it() {
        let (state, _dir) = restore_panel("restore-launch-once", &[("s-x", None, Auto)], &[]);
        state.borrow_mut().restore_policy = RestorePolicy::Auto;
        let mut resumes = FakeResumes::default();
        started(ask(&state, &mut resumes, RestoreAsk::Launch));
        assert!(
            state.borrow().restore_source.is_none(),
            "a reload of the page does not try again"
        );
        assert!(resumes.asked.is_empty(), "the record is gone, so nothing was begun");
        // And its message is already due: nothing is left to wait for.
        let toasts = Rc::new(RefCell::new(Vec::<String>::new()));
        state.borrow_mut().toast_hook = Some({
            let toasts = toasts.clone();
            Rc::new(move |text: &str| toasts.borrow_mut().push(text.to_string()))
        });
        finish_restore(&state);
        assert_eq!(
            *toasts.borrow(),
            ["Restored 0 of 1 tab; 1 could not be: s-x (its saved record is gone)"]
        );
        finish_restore(&state);
        assert_eq!(toasts.borrow().len(), 1, "said once");
    }

    #[test]
    fn the_users_own_bypass_default_restores_bypass_tabs_without_asking() {
        let (state, _dir) = restore_panel("restore-config-bypass", &[("s-1", None, Bypass)], &["s-1"]);
        state.borrow_mut().bypass_by_config = true;
        let mut resumes = FakeResumes::default();
        assert_eq!(started(ask(&state, &mut resumes, RestoreAsk::Dashboard)), None);
        assert_eq!(mode(&state, "s-1"), Bypass);
    }

    /// Only the sidecar resumes: a window on the legacy backend neither offers nor attempts a restore,
    /// and says why if asked.
    #[test]
    fn the_legacy_backend_is_never_asked_to_restore() {
        let (state, _dir) = restore_panel("restore-legacy", &[("s-1", None, Auto)], &["s-1"]);
        state.borrow_mut().backend_kind = BackendKind::Legacy;
        let mut resumes = FakeResumes::default();
        assert!(matches!(
            ask(&state, &mut resumes, RestoreAsk::Launch),
            RestoreResult::Refused(_)
        ));
        assert!(resumes.asked.is_empty());
    }

    #[test]
    fn a_restore_is_refused_when_it_would_be_stale() {
        let (state, _dir) = restore_panel("restore-stale", &[("s-1", None, Auto)], &["s-1"]);
        let mut resumes = FakeResumes::default();
        started(ask(&state, &mut resumes, RestoreAsk::Dashboard));
        assert!(
            matches!(
                ask(&state, &mut resumes, RestoreAsk::Dashboard),
                RestoreResult::Refused(_)
            ),
            "a second press: the tabs are already coming back"
        );
        let (none, _dir) = restore_panel("restore-none", &[], &[]);
        none.borrow_mut().restore_source = None;
        assert!(matches!(
            ask(&none, &mut FakeResumes::default(), RestoreAsk::Dashboard),
            RestoreResult::Refused(_)
        ));
    }

    /// The glue the tick runs: each result counts into the restore, a refused tab is tidied away,
    /// and one message ends it once the last resume is back.
    #[test]
    fn a_restore_ends_in_one_toast_once_the_last_resume_has_returned() {
        let (state, dir) = restore_panel(
            "restore-toast",
            &[("s-1", None, Auto), ("s-2", None, Auto), ("s-3", None, Auto)],
            &["s-1", "s-2", "s-3"],
        );
        let toasts = Rc::new(RefCell::new(Vec::<String>::new()));
        state.borrow_mut().toast_hook = Some({
            let toasts = toasts.clone();
            Rc::new(move |text: &str| toasts.borrow_mut().push(text.to_string()))
        });
        let mut resumes = FakeResumes::default();
        started(ask(&state, &mut resumes, RestoreAsk::Dashboard));
        finish_restore(&state);
        assert!(toasts.borrow().is_empty(), "three resumes are still connecting");

        let (_provider, backend) = live_backend(&dir);
        resumes.senders[0].send(Ok(backend)).unwrap();
        resumes.senders[1]
            .send(Err(BackendError {
                message: "could not continue the previous session: no such session".to_string(),
                benign: false,
                folded_events: Vec::new(),
            }))
            .unwrap();
        let collected = state.borrow_mut().tabs.collect_starts();
        let mut accounted = Vec::new();
        for result in &collected {
            accounted.push(account_for_restore(&mut state.borrow_mut(), result));
        }
        finish_restore(&state);
        assert!(
            accounted.contains(&RestoreStart::Installed)
                && accounted.iter().any(|a| matches!(a, RestoreStart::Failed { .. })),
            "{accounted:?}"
        );
        assert!(toasts.borrow().is_empty(), "the third is still connecting");
        assert_eq!(state.borrow().tabs.tabs().len(), 2, "the refused tab is gone");

        let (_provider, backend) = live_backend(&dir);
        resumes.senders[2].send(Ok(backend)).unwrap();
        let collected = state.borrow_mut().tabs.collect_starts();
        for result in &collected {
            account_for_restore(&mut state.borrow_mut(), result);
        }
        finish_restore(&state);
        assert_eq!(
            *toasts.borrow(),
            ["Restored 2 of 3 tabs; 1 could not be: title of s-2 (could not continue the previous session: no such session)"]
        );
        for mut tab in state.borrow_mut().tabs.take_all() {
            if let TabBackend::Live(backend) = &mut tab.backend {
                backend.shutdown();
            }
        }
    }

    /// A restored tab the user closes before its resume returns is never going to report; the restore
    /// must still end, and say nothing about it.
    #[test]
    fn closing_a_restored_tab_while_it_connects_does_not_hold_the_restore_open() {
        let (state, dir) = restore_panel(
            "restore-closed",
            &[("s-1", None, Auto), ("s-2", None, Auto)],
            &["s-1", "s-2"],
        );
        let toasts = Rc::new(RefCell::new(Vec::<String>::new()));
        state.borrow_mut().toast_hook = Some({
            let toasts = toasts.clone();
            Rc::new(move |text: &str| toasts.borrow_mut().push(text.to_string()))
        });
        let mut resumes = FakeResumes::default();
        started(ask(&state, &mut resumes, RestoreAsk::Dashboard));
        let second = state.borrow().tabs.tab_with_session(&AgentId::Claude, "s-2").unwrap();
        let (_provider, backend) = live_backend(&dir);
        resumes.senders[0].send(Ok(backend)).unwrap();
        let collected = state.borrow_mut().tabs.collect_starts();
        for result in &collected {
            account_for_restore(&mut state.borrow_mut(), result);
        }
        finish_restore(&state);
        assert!(toasts.borrow().is_empty(), "s-2 is still connecting");
        let closed = state.borrow_mut().tabs.remove(second);
        assert!(closed.is_some());
        finish_restore(&state);
        assert_eq!(*toasts.borrow(), ["Restored 1 tab"]);
        assert!(state.borrow().restore_run.is_none(), "so the restore is over");
        for mut tab in state.borrow_mut().tabs.take_all() {
            if let TabBackend::Live(backend) = &mut tab.backend {
                backend.shutdown();
            }
        }
    }

    /// A restore that fails in the tab it began in leaves that tab as it found it: in the mode it had,
    /// not in one only the restored session was cleared for.
    /// The saved active tab is the one that does not come back: the screen goes to a tab that did, not
    /// to an empty dashboard over them.
    #[test]
    fn when_the_tab_on_screen_fails_the_first_that_restored_takes_its_place() {
        let (state, dir) = restore_panel(
            "restore-survivor",
            &[("s-1", None, Auto), ("s-2", None, Auto)],
            &["s-1", "s-2"],
        );
        let mut resumes = FakeResumes::default();
        started(ask(&state, &mut resumes, RestoreAsk::Dashboard));
        let first = state.borrow().tabs.tab_with_session(&AgentId::Claude, "s-1").unwrap();
        let second = state.borrow().tabs.tab_with_session(&AgentId::Claude, "s-2").unwrap();
        assert_eq!(state.borrow().tabs.active(), first, "the saved active tab is the first");
        resumes.senders[0]
            .send(Err(BackendError {
                message: "refused".to_string(),
                benign: false,
                folded_events: Vec::new(),
            }))
            .unwrap();
        let (_provider, backend) = live_backend(&dir);
        resumes.senders[1].send(Ok(backend)).unwrap();
        let collected = state.borrow_mut().tabs.collect_starts();
        for result in &collected {
            account_for_restore(&mut state.borrow_mut(), result);
        }
        assert_eq!(state.borrow().tabs.active(), second);
        for mut tab in state.borrow_mut().tabs.take_all() {
            if let TabBackend::Live(backend) = &mut tab.backend {
                backend.shutdown();
            }
        }
    }

    #[test]
    fn a_failed_restore_leaves_the_tab_it_began_in_in_its_old_mode() {
        let (state, _dir) = restore_panel("restore-mode-back", &[("s-1", None, Bypass)], &["s-1"]);
        state.borrow_mut().bypass_by_config = true; // the saved bypass tab comes back as saved
        let mut resumes = FakeResumes::default();
        started(ask(&state, &mut resumes, RestoreAsk::Dashboard));
        let tab = state.borrow().tabs.active();
        assert_eq!(
            state.borrow().tabs.get(tab).unwrap().mode(),
            Bypass,
            "while it connects"
        );
        resumes.senders[0]
            .send(Err(BackendError {
                message: "refused".to_string(),
                benign: false,
                folded_events: Vec::new(),
            }))
            .unwrap();
        let collected = state.borrow_mut().tabs.collect_starts();
        for result in &collected {
            account_for_restore(&mut state.borrow_mut(), result);
        }
        let s = state.borrow();
        assert!(matches!(s.tabs.get(tab).unwrap().backend, TabBackend::NotStarted));
        assert_eq!(s.tabs.get(tab).unwrap().mode(), Auto, "an empty tab, in auto again");
    }

    #[test]
    fn a_connect_that_is_not_part_of_a_restore_is_left_to_its_own_handling() {
        let (state, _dir) = restore_panel("restore-unrelated", &[("s-1", None, Auto)], &["s-1"]);
        let collected = StartCollected::Failed {
            tab: state.borrow().tabs.active(),
            request_id: "r".to_string(),
            error: BackendError {
                message: "x".to_string(),
                benign: false,
                folded_events: Vec::new(),
            },
        };
        assert_eq!(
            account_for_restore(&mut state.borrow_mut(), &collected),
            RestoreStart::NotPartOfOne
        );
    }

    /// Once per tick, the tabs are shown to the memory: a session adopted is written, and so is
    /// nothing more until something changes.
    #[test]
    fn the_tick_keeps_the_saved_file_in_step_and_a_closing_window_leaves_it_alone() {
        let (state, dir) = restore_panel("restore-remember", &[], &[]);
        let files = agent::state_dirs::test_workspace_dir("restore-remember-files");
        state.borrow_mut().tab_memory = TabWriter::new(Some(files.clone()), dir.clone());
        remember_tabs(&state);
        assert!(state
            .borrow()
            .tab_memory
            .wait_written(std::time::Duration::from_secs(5)));
        assert!(
            std::fs::read_dir(&files).unwrap().next().is_none(),
            "an untouched launch writes nothing"
        );

        let tab = state.borrow().tabs.active();
        let (provider, backend) = live_backend(&dir);
        state.borrow_mut().tabs.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.open_session("s-live", &dir);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.borrow().tabs.get(tab).unwrap().provider_session_id().is_none() {
            assert!(std::time::Instant::now() < deadline, "the session id never arrived");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        remember_tabs(&state);
        assert!(state
            .borrow()
            .tab_memory
            .wait_written(std::time::Duration::from_secs(5)));
        let loaded = match eitri_core::saved_tabs::load(&files, &dir) {
            eitri_core::saved_tabs::Loaded::Saved(saved) => saved,
            other => panic!("{other:?}"),
        };
        assert_eq!(loaded.tabs.len(), 1);
        assert_eq!(loaded.tabs[0].provider_session_id, "s-live");
        assert_eq!(loaded.tabs[0].conversation_id, state.borrow().conversation_id);

        let taken = state.borrow_mut().tabs.take_all();
        remember_tabs(&state);
        assert!(state
            .borrow()
            .tab_memory
            .wait_written(std::time::Duration::from_secs(5)));
        assert_eq!(
            eitri_core::saved_tabs::load(&files, &dir),
            eitri_core::saved_tabs::Loaded::Saved(loaded)
        );
        for mut tab in taken {
            if let TabBackend::Live(backend) = &mut tab.backend {
                backend.shutdown();
            }
        }
    }

    /// The close hands the writer the tabs as they are and holds the process until that write is on disk:
    /// a session adopted after the last tick is still kept.
    #[test]
    fn the_close_waits_for_the_last_saved_tabs_write() {
        let (state, dir) = restore_panel("restore-close", &[], &[]);
        let files = agent::state_dirs::test_workspace_dir("restore-close-files");
        state.borrow_mut().tab_memory = TabWriter::new(Some(files.clone()), dir.clone());
        let tab = state.borrow().tabs.active();
        let (provider, backend) = live_backend(&dir);
        state.borrow_mut().tabs.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.open_session("s-closing", &dir);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.borrow().tabs.get(tab).unwrap().provider_session_id().is_none() {
            assert!(std::time::Instant::now() < deadline, "the session id never arrived");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // No tick ran since: the close itself hands the set over.
        let mut watch = handle_for(&state).shutdown();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while watch.step(std::time::Instant::now()) {
            assert!(std::time::Instant::now() < deadline, "the close never let go");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let loaded = match eitri_core::saved_tabs::load(&files, &dir) {
            eitri_core::saved_tabs::Loaded::Saved(saved) => saved,
            other => panic!("{other:?}"),
        };
        assert_eq!(loaded.tabs.len(), 1);
        assert_eq!(loaded.tabs[0].provider_session_id, "s-closing");
    }

    /// The close holds the process while the writer is inside the last write and lets go only once it is on
    /// disk: with the write held, every look keeps holding; released, the close ends with the tab saved.
    #[test]
    fn the_close_holds_while_the_last_saved_tabs_write_is_under_way() {
        let (state, dir) = restore_panel("restore-close-held", &[], &[]);
        let files = agent::state_dirs::test_workspace_dir("restore-close-held-files");
        let mut writer = TabWriter::new(Some(files.clone()), dir.clone());
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx);
        writer.set_before_write(std::sync::Arc::new(move || {
            let _ = release_rx
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(10));
        }));
        state.borrow_mut().tab_memory = writer;
        {
            // A tab that keeps its session's id with no backend to tear down: the write is all the close owes.
            let mut s = state.borrow_mut();
            let tab = s.tabs.active();
            let t = s.tabs.get_mut(tab).unwrap();
            t.failed_resume = Some("s-held".to_string());
            t.backend = TabBackend::Failed {
                reason: "stopped".to_string(),
            };
        }
        let mut watch = handle_for(&state).shutdown();
        let held_until = std::time::Instant::now() + std::time::Duration::from_millis(300);
        while std::time::Instant::now() < held_until {
            assert!(
                watch.step(std::time::Instant::now()),
                "let go while the write was under way"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        release_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while watch.step(std::time::Instant::now()) {
            assert!(std::time::Instant::now() < deadline, "the close never let go");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let loaded = match eitri_core::saved_tabs::load(&files, &dir) {
            eitri_core::saved_tabs::Loaded::Saved(saved) => saved,
            other => panic!("{other:?}"),
        };
        let ids: Vec<_> = loaded.tabs.iter().map(|t| t.provider_session_id.as_str()).collect();
        assert_eq!(ids, ["s-held"]);
    }

    /// Points `CLAUDE_CONFIG_DIR` at one disposable root for this whole test binary, and returns it.
    ///
    /// A process-wide variable set from a multi-threaded test binary, which this project normally
    /// refuses (`agent::state_dirs`'s own module doc says so). It is set exactly ONCE, to a value that
    /// never changes, from inside a `OnceLock` initializer, so no thread can observe it being two different
    /// things; `eitri-core`'s test binary does the same for the same reason. Nothing else in this binary
    /// reads a transcript, so any other test that reaches a transcript path finds an empty directory,
    /// which is what it would have found anyway. The root lives under the agent crate's test state root,
    /// which the next run prunes once this process is gone, so nothing is left behind for good.
    fn a_claude_config_root() -> PathBuf {
        static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        ROOT.get_or_init(|| {
            let root = agent::state_dirs::redirect_state_to_a_test_root().join("claude-config");
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            std::env::set_var("CLAUDE_CONFIG_DIR", &root);
            root
        })
        .clone()
    }

    /// `hello` names a row by its record's own title at once, and the CLI's own title, read on the
    /// title worker, arrives with a wake and a second `hello`; the window never read the transcript.
    #[test]
    fn the_clis_title_reaches_hello_after_the_worker_read_it() {
        a_claude_config_root();
        let (state, dir) = restore_panel("titles-later", &[], &["s-titled"]);
        let transcript = agent::transcript::transcript_path(&dir.to_string_lossy(), "s-titled").unwrap();
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "{\"type\":\"ai-title\",\"aiTitle\":\"the CLI's title\"}\n").unwrap();
        let page = Rc::new(RecordingPage::default());
        page.visible.set(true);
        let handle = AgentPanelHandle::over(
            state.clone(),
            Some(Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>))),
        );
        let titles = |page: &RecordingPage| -> Vec<String> {
            page.sent
                .borrow()
                .iter()
                .filter(|p| kind_of(p) == "hello")
                .map(|p| {
                    let hello: serde_json::Value = serde_json::from_str(p).unwrap();
                    hello["resumableSessions"][0]["title"].as_str().unwrap_or("").to_owned()
                })
                .collect()
        };
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        assert_eq!(
            titles(&page),
            vec!["title of s-titled".to_string()],
            "the record's own, at once"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while titles(&page).len() < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "no second hello: {:?}",
                titles(&page)
            );
            if let Some(due) = handle.take_wake(std::time::Instant::now()) {
                handle.run_tick(due);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(titles(&page)[1], "the CLI's title");
        let _ = std::fs::remove_file(transcript);
    }

    /// A chooser sent before the CLI's title of one of its records was read gets that title once the worker
    /// has read it: a `chooser_titles` envelope naming the record, and nothing more for it afterwards.
    #[test]
    fn a_title_read_after_the_chooser_opened_is_sent_to_it() {
        a_claude_config_root();
        let (state, dir) = restore_panel("titles-chooser", &[], &["s-listed"]);
        let transcript = agent::transcript::transcript_path(&dir.to_string_lossy(), "s-listed").unwrap();
        let _ = std::fs::remove_file(&transcript);
        let page = Rc::new(RecordingPage::default());
        page.visible.set(true);
        let handle = AgentPanelHandle::over(
            state.clone(),
            Some(Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>))),
        );
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        // The greeting's read finds no transcript yet; the title is written after.
        assert!(state.borrow().titles.wait_read(std::time::Duration::from_secs(5)));
        let tick_pending = |handle: &AgentPanelHandle| {
            if let Some(due) = handle.take_wake(std::time::Instant::now()) {
                handle.run_tick(due);
            }
        };
        tick_pending(&handle);
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(
            &transcript,
            "{\"type\":\"ai-title\",\"aiTitle\":\"the CLI's late title\"}\n",
        )
        .unwrap();
        handle.open_chooser(ViewId::BASE);
        let chooser = page
            .sent
            .borrow()
            .iter()
            .rfind(|p| kind_of(p) == "chooser")
            .cloned()
            .expect("the chooser was sent");
        assert!(
            chooser.contains("title of s-listed"),
            "the record's own title: {chooser}"
        );
        let retitled = |page: &RecordingPage| {
            page.sent
                .borrow()
                .iter()
                .filter(|p| kind_of(p) == "chooser_titles")
                .cloned()
                .collect::<Vec<_>>()
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while retitled(&page).is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "the chooser was never told the title"
            );
            tick_pending(&handle);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let sent: serde_json::Value = serde_json::from_str(&retitled(&page)[0]).unwrap();
        assert_eq!(
            sent["titles"],
            serde_json::json!([{ "providerSessionId": "s-listed", "title": "the CLI's late title" }])
        );
        // Told once: a later title read that changes nothing for it sends nothing more.
        handle.state.borrow_mut().chooser_titles_stale = true;
        handle.run_tick(std::time::Instant::now() + std::time::Duration::from_secs(1));
        assert_eq!(retitled(&page).len(), 1);
        let _ = std::fs::remove_file(transcript);
    }

    /// A tab restored with its record's title (the CLI's not read yet at launch) takes the CLI's title once
    /// the worker has read it, and the `tabs` envelope carries it.
    #[test]
    fn a_restored_tab_takes_the_clis_title_when_it_is_read() {
        a_claude_config_root();
        let (state, dir) = restore_panel("titles-tab", &[], &["s-tabbed"]);
        let transcript = agent::transcript::transcript_path(&dir.to_string_lossy(), "s-tabbed").unwrap();
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "{\"type\":\"ai-title\",\"aiTitle\":\"the CLI's own\"}\n").unwrap();
        let tab = state.borrow().tabs.active();
        let (provider, backend) = live_backend(&dir);
        {
            let mut s = state.borrow_mut();
            let t = s.tabs.get_mut(tab).unwrap();
            t.backend = TabBackend::Live(backend);
            t.title = Some("title of s-tabbed".into());
            t.title_from_record = true;
        }
        let page = Rc::new(RecordingPage::default());
        page.visible.set(true);
        let handle = AgentPanelHandle::over(
            state.clone(),
            Some(Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>))),
        );
        // The greeting asks the worker to read the session's transcript, which may land before the session
        // has opened, as at a restore.
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        assert!(state.borrow().titles.wait_read(std::time::Duration::from_secs(5)));
        provider.open_session("s-tabbed", &dir);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.borrow().tabs.get(tab).unwrap().title.as_deref() != Some("the CLI's own") {
            assert!(std::time::Instant::now() < deadline, "the tab kept the record's title");
            if let Some(due) = handle.take_wake(std::time::Instant::now()) {
                handle.run_tick(due);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            page.sent
                .borrow()
                .iter()
                .any(|p| kind_of(p) == "tabs" && p.contains("the CLI's own")),
            "the tabs envelope carries it"
        );
        let _ = std::fs::remove_file(transcript);
        for mut t in state.borrow_mut().tabs.take_all() {
            if let TabBackend::Live(backend) = &mut t.backend {
                backend.shutdown();
            }
        }
    }

    fn handle_for(state: &Rc<RefCell<AgentPanelState>>) -> AgentPanelHandle {
        AgentPanelHandle::over(state.clone(), None)
    }

    /// `agent.restore = "off"` is the whole feature off: nothing is offered, nothing is remembered.
    #[test]
    fn restore_off_offers_nothing_and_remembers_nothing() {
        let (state, dir) = restore_panel("restore-off", &[("s-1", None, Auto)], &["s-1"]);
        let files = agent::state_dirs::test_workspace_dir("restore-off-files");
        state.borrow_mut().tab_memory = TabWriter::new(Some(files.clone()), dir.clone());
        handle_for(&state).set_restore_policy(RestorePolicy::Off);
        assert!(state.borrow().restore_source.is_none());
        assert!(!state.borrow().tab_memory.is_enabled());
        assert!(!restore_is_on_offer(&state.borrow()));

        let tab = state.borrow().tabs.active();
        let (provider, backend) = live_backend(&dir);
        state.borrow_mut().tabs.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.open_session("s-live", &dir);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.borrow().tabs.get(tab).unwrap().provider_session_id().is_none() {
            assert!(std::time::Instant::now() < deadline, "the session id never arrived");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        remember_tabs(&state);
        assert!(state
            .borrow()
            .tab_memory
            .wait_written(std::time::Duration::from_secs(5)));
        assert!(std::fs::read_dir(&files).unwrap().next().is_none(), "nothing written");
        for mut tab in state.borrow_mut().tabs.take_all() {
            if let TabBackend::Live(backend) = &mut tab.backend {
                backend.shutdown();
            }
        }
    }

    /// `agent.default_mode`: applied to the window's default and its empty tab, unless the project
    /// remembers a mode Shift+Tab left; naming bypass is also what lets saved bypass tabs come back
    /// unasked; naming nothing changes nothing.
    #[test]
    fn the_configured_default_mode_applies_unless_the_project_remembers_one() {
        let (state, _dir) = restore_panel("default-mode", &[], &[]);
        let handle = handle_for(&state);
        handle.set_default_mode(None);
        assert_eq!(state.borrow().tabs.default_mode(), Auto);
        assert!(!state.borrow().bypass_by_config);

        handle.set_default_mode(Some(Bypass));
        assert_eq!(state.borrow().tabs.default_mode(), Bypass);
        let tab = state.borrow().tabs.active();
        assert_eq!(
            state.borrow().tabs.get(tab).unwrap().mode(),
            Bypass,
            "the empty tab it already has"
        );
        assert!(state.borrow().bypass_by_config);

        // A project that remembers a mode: the setting does not override it.
        let (remembered, _dir) = restore_panel("default-mode-remembered", &[], &[]);
        remembered.borrow_mut().mode_remembered = true;
        handle_for(&remembered).set_default_mode(Some(Bypass));
        assert_eq!(remembered.borrow().tabs.default_mode(), Auto);
        let tab = remembered.borrow().tabs.active();
        assert_eq!(remembered.borrow().tabs.get(tab).unwrap().mode(), Auto);
    }

    // ---- the trust gate on every session path ----------------------------------------------------

    /// A project with its own home and state home, all under one scratch directory the test owns.
    struct TrustFx {
        outer: PathBuf,
        state: PathBuf,
        root: PathBuf,
        /// Every start the panel's spawner was asked for: the session it resumes, if any, and the
        /// tiers it was given.
        spawned: Rc<RefCell<Vec<Spawned>>>,
        /// Every start of another agent's tab: the agent, its trust plan, its mode and the session
        /// it resumes.
        external: Rc<RefCell<Vec<ExternalSpawned>>>,
    }

    /// One start of another agent's tab, as the counting spawner saw it.
    #[derive(Debug, Clone)]
    struct ExternalSpawned {
        agent: AgentId,
        plan: agent::TrustPlan,
        mode: SessionModeChoice,
        resume: Option<String>,
        /// The command and mode a registered agent's start was given.
        acp: Option<(Vec<std::ffi::OsString>, Option<String>)>,
        /// Whether the start waits for the tab's old backend to be gone (`r` continuing its session).
        waits_for_old: bool,
    }

    /// One start a counting spawner was asked for: the session it resumes, if any, and its tiers.
    type Spawned = (Option<String>, ProjectTrust);

    impl TrustFx {
        fn write(&self, relative: &str, bytes: &[u8]) {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }

        fn record_file(&self) -> PathBuf {
            self.state
                .join("eitri/trust")
                .join(format!("{}.json", &agent::conversation_id_for_cwd(&self.root)[..16]))
        }
    }

    impl Drop for TrustFx {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.outer);
        }
    }

    const PROJECT_SETTINGS: &str = ".claude/settings.json";

    fn session_start_hook(command: &str) -> Vec<u8> {
        format!(r#"{{"hooks":{{"SessionStart":[{{"hooks":[{{"type":"command","command":"{command}"}}]}}]}}}}"#)
            .into_bytes()
    }

    /// A panel over a fresh project below a fake home, with a real gate and a spawner that only
    /// counts. The page is ready, so a question can be shown.
    fn trust_panel(label: &str) -> (Rc<RefCell<AgentPanelState>>, TrustFx) {
        let outer = agent::state_dirs::test_workspace_dir(label);
        let home = outer.join("home");
        let state_home = outer.join("state");
        let root = home.join("p");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&state_home).unwrap();
        let spawned = Rc::new(RefCell::new(Vec::new()));
        let external = Rc::new(RefCell::new(Vec::new()));
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        {
            let mut s = state.borrow_mut();
            s.project_dir = root.clone();
            s.canonical_project_dir = root.to_string_lossy().into_owned();
            s.page_mut(ViewId::BASE).document_ready = true;
            s.trust = trust_gate_for(Some(state_home.as_os_str()), Some(home.as_os_str()), &root);
            let counted = spawned.clone();
            let counted_external = external.clone();
            s.spawner = Spawner::Fake(Rc::new(RefCell::new(move |call: SpawnCall| {
                match call {
                    SpawnCall::Connect { resume, project } => counted.borrow_mut().push((resume, project)),
                    SpawnCall::Resume { id, project, .. } => counted.borrow_mut().push((Some(id), project)),
                    SpawnCall::External {
                        agent,
                        plan,
                        mode,
                        resume,
                        acp,
                        after_old,
                    } => counted_external.borrow_mut().push(ExternalSpawned {
                        agent,
                        plan,
                        mode,
                        resume,
                        acp: acp.map(|spec| (spec.argv, spec.auto_mode)),
                        waits_for_old: after_old.is_some(),
                    }),
                }
                mpsc::channel().1
            })));
        }
        let fx = TrustFx {
            outer,
            state: state_home,
            root,
            spawned,
            external,
        };
        (state, fx)
    }

    /// One trust prompt as the page received it.
    #[derive(Debug, Clone)]
    struct Prompt {
        tab: TabId,
        nonce: u64,
        fingerprint: String,
        digest: String,
        remember: String,
        state: String,
    }

    fn json_of(payload: &str) -> serde_json::Value {
        serde_json::from_str(payload).unwrap()
    }

    fn prompts(payloads: &[String]) -> Vec<Prompt> {
        payloads
            .iter()
            .map(|p| json_of(p))
            .filter(|v| v["kind"] == "trust_prompt")
            .map(|v| Prompt {
                tab: TabId(v["tab"].as_u64().unwrap()),
                nonce: v["nonce"].as_u64().unwrap(),
                fingerprint: v["fingerprint"].as_str().unwrap().to_string(),
                digest: v["findingsDigest"].as_str().unwrap().to_string(),
                remember: v["remember"].as_str().unwrap().to_string(),
                state: v["state"].as_str().unwrap().to_string(),
            })
            .collect()
    }

    fn all_payloads(seen: &[TrustOut]) -> Vec<String> {
        seen.iter().flat_map(|o| o.payloads.iter().cloned()).collect()
    }

    /// The last prompt the tick sent, waiting for one.
    fn next_prompt(state: &Rc<RefCell<AgentPanelState>>) -> Prompt {
        let seen = tick_until(state, |seen| !prompts(&all_payloads(seen)).is_empty());
        prompts(&all_payloads(&seen)).pop().unwrap()
    }

    /// The `command_result` for `request_id` among `payloads`: `Ok` or the error text.
    fn result_for(payloads: &[String], request_id: &str) -> Option<Result<(), String>> {
        payloads.iter().map(|p| json_of(p)).find_map(|v| {
            (v["kind"] == "command_result" && v["requestId"] == request_id).then(|| {
                if v["ok"] == true {
                    Ok(())
                } else {
                    Err(v["error"].as_str().unwrap_or("").to_string())
                }
            })
        })
    }

    fn notices(payloads: &[String]) -> Vec<String> {
        payloads
            .iter()
            .map(|p| json_of(p))
            .filter(|v| v["kind"] == "notice")
            .map(|v| v["text"].as_str().unwrap().to_string())
            .collect()
    }

    fn first_send(request_id: &str, text: &str) -> DeferredStart {
        DeferredStart::FirstSend {
            request_id: request_id.to_string(),
            first_turn: FirstTurn {
                wire: format!("wire: {text}"),
                typed: text.to_string(),
            },
        }
    }

    fn answer_prompt(state: &Rc<RefCell<AgentPanelState>>, request_id: &str, p: &Prompt, trust: bool) -> TrustOut {
        answer_trust(
            &mut state.borrow_mut(),
            request_id,
            p.tab,
            p.nonce,
            &p.fingerprint,
            &p.digest,
            trust,
            ViewId::BASE,
        )
    }

    /// Outside `start_deferred` (and the connect workers it calls), nothing in the panel begins a
    /// session: no other code calls the spawners or `start_restore`, sets a tab connecting, or
    /// starts a backend. Another agent's tab is started by `start_external_tab`, which only
    /// `start_deferred` calls, through `spawn_external`, the one worker that calls
    /// `AgentBackend::start_external`. Read as text, formatting-insensitive.
    #[test]
    fn every_session_start_goes_through_the_trust_gate() {
        use eitri_core::source_scan as scan;
        const SCANNED: &[&str] = &["shell/src", "panel/src"];
        const SPAWNERS: &[&str] = &[
            "spawn_connect",
            "spawn_connect_holding",
            "resume_spawner",
            "start_restore",
            "spawn_external",
            "start_external_tab",
            "start_external",
            "start_external_with",
        ];
        let read = scan::rust_sources(SCANNED);
        let panel = scan::file_with(&read, "fn start_deferred(");
        let code = scan::code_only(&panel.text);
        let tests = scan::modules(&code, "tests");
        assert_eq!(tests.len(), 1, "the tests module not found");
        let gate = scan::functions(&code, "start_deferred");
        assert_eq!(gate.len(), 1, "start_deferred not found");
        for spawner in ["spawn_connect", "resume_spawner", "start_restore", "start_external_tab"] {
            assert!(
                scan::calls(gate[0], spawner) > 0,
                "start_deferred no longer calls {spawner}; the scan would prove nothing"
            );
        }
        assert!(gate[0].contains("TabBackend::Starting(PendingStart"));
        let external = scan::functions(&code, "start_external_tab");
        assert_eq!(external.len(), 1, "start_external_tab not found");
        assert!(
            scan::calls(external[0], "spawn_external") > 0,
            "start_external_tab no longer calls spawn_external; the scan would prove nothing"
        );
        assert!(
            scan::calls(external[0], "trust_plan") > 0,
            "an external start no longer builds its trust plan"
        );
        let worker = scan::functions(&code, "spawn_external");
        assert_eq!(worker.len(), 1, "spawn_external not found");
        let flat_worker: String = worker[0].split_whitespace().collect();
        assert!(
            flat_worker.contains("AgentBackend::start_external("),
            "spawn_external no longer starts the backend; the scan would prove nothing"
        );
        // Of the two, only `start_deferred` may call `start_external_tab`, and only it may call
        // `spawn_external`.
        let without_gate = scan::without(&code, &[gate[0], tests[0]]);
        assert_eq!(
            scan::calls(&without_gate, "start_external_tab"),
            0,
            "start_external_tab is called outside start_deferred"
        );
        let without_external = scan::without(&code, &[external[0], tests[0]]);
        assert_eq!(
            scan::calls(&without_external, "spawn_external"),
            0,
            "spawn_external is called outside start_external_tab"
        );

        let mut cut: Vec<&str> = tests;
        cut.extend(gate);
        cut.extend(external);
        cut.extend(worker);
        for worker in ["spawn_connect", "resume_spawner", "spawn_connect_holding"] {
            let found = scan::functions(&code, worker);
            assert_eq!(found.len(), 1, "{worker} not found");
            cut.extend(found);
        }
        let rest = scan::without(&code, &cut);
        for name in SPAWNERS {
            assert_eq!(scan::calls(&rest, name), 0, "{name} is called outside start_deferred");
        }
        let flat: String = rest.split_whitespace().collect();
        assert!(
            !flat.contains("TabBackend::Starting(PendingStart"),
            "a tab is set connecting outside start_deferred"
        );
        assert!(
            !flat.contains("AgentBackend::start"),
            "a backend is started outside the connect worker"
        );

        // No other file of the crate starts a session either.
        for other in read.iter().filter(|source| source.path != panel.path) {
            let code = scan::code_only(&other.text);
            let rest = scan::without(&code, &scan::modules(&code, "tests"));
            for name in SPAWNERS {
                assert_eq!(scan::calls(&rest, name), 0, "{} calls {name}", other.path);
            }
            let flat: String = rest.split_whitespace().collect();
            assert!(
                !flat.contains("TabBackend::Starting(PendingStart"),
                "{} sets a tab connecting",
                other.path
            );
            assert!(!flat.contains("AgentBackend::start"), "{} starts a backend", other.path);
        }
    }

    /// A Codex tab, opened and on screen.
    fn codex_tab(state: &Rc<RefCell<AgentPanelState>>) -> TabId {
        let mut s = state.borrow_mut();
        let tab = s.tabs.open(AgentId::Codex);
        s.tabs.select(tab);
        tab
    }

    fn items_of_prompt(payloads: &[String]) -> Vec<String> {
        payloads
            .iter()
            .map(|p| json_of(p))
            .filter(|v| v["kind"] == "trust_prompt")
            .flat_map(|v| {
                v["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|item| item["file"].as_str().unwrap().to_string())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// `:trust` and `:untrust` act on the agent of the tab on screen: in a Codex tab they ask about,
    /// record and forget Codex's configuration, never Claude's; one the page typed for another agent's
    /// tab is refused.
    #[test]
    fn trust_command_acts_on_the_active_tab_agent() {
        use eitri_core::agent_bridge::TrustAction;
        let (state, fx) = trust_panel("trust-command-agent");
        fx.write(PROJECT_SETTINGS, &session_start_hook("touch /tmp/claude"));
        fx.write("AGENTS.md", b"codex instructions\n");
        let tab = codex_tab(&state);
        let codex_record = PathBuf::from(format!("{}.codex", fx.record_file().display()));

        let refused = trust_command(
            &mut state.borrow_mut(),
            ViewId::BASE,
            "c0",
            TrustAction::Trust,
            "claude",
        );
        assert_eq!(
            refused,
            Err("that :trust was for a Claude tab; this tab runs Codex".to_string())
        );
        let refused = trust_command(
            &mut state.borrow_mut(),
            ViewId::BASE,
            "c0",
            TrustAction::Untrust,
            "claude",
        );
        assert_eq!(
            refused,
            Err("that :untrust was for a Claude tab; this tab runs Codex".to_string())
        );

        trust_command(&mut state.borrow_mut(), ViewId::BASE, "c1", TrustAction::Trust, "codex").unwrap();
        let seen = tick_until(&state, |seen| !prompts(&all_payloads(seen)).is_empty());
        let payloads = all_payloads(&seen);
        assert_eq!(
            items_of_prompt(&payloads),
            ["AGENTS.md"],
            "the Codex files, not Claude's"
        );
        let prompt = prompts(&payloads).pop().unwrap();
        assert_eq!(prompt.tab, tab);
        let out = answer_prompt(&state, "a1", &prompt, true);
        assert_eq!(result_for(&out.payloads, "a1"), Some(Ok(())));
        let seen = tick_until(&state, |seen| result_for(&all_payloads(seen), "c1").is_some());
        assert_eq!(result_for(&all_payloads(&seen), "c1"), Some(Ok(())));
        assert!(codex_record.is_file());
        assert!(!fx.record_file().exists(), "Claude's record is untouched");
        assert!(state.borrow().trust.row(&Profile::Codex).starts_with("trusted since "));
        assert_eq!(state.borrow().trust.row(&Profile::Claude), "not checked yet");

        trust_command(
            &mut state.borrow_mut(),
            ViewId::BASE,
            "c2",
            TrustAction::Untrust,
            "codex",
        )
        .unwrap();
        let seen = tick_until(&state, |seen| result_for(&all_payloads(seen), "c2").is_some());
        assert_eq!(result_for(&all_payloads(&seen), "c2"), Some(Ok(())));
        assert!(!codex_record.exists());
    }

    /// The external start is handed the trust plan of the discovery its answer was bound to: a `y`
    /// trusts the directories that discovery walked and nothing above them, names the project's own
    /// tool servers, and keeps skills on; an `n` in another tab untrusts every directory up to `/`
    /// and turns skills off. Nothing goes through the Claude spawner.
    #[test]
    fn external_start_gets_the_plan_of_the_bound_discovery() {
        let (state, fx) = trust_panel("trust-external-plan");
        fx.write("AGENTS.md", b"codex instructions\n");
        fx.write(".codex/config.toml", b"[mcp_servers.srv]\ncommand = \"srv\"\n");
        let tab = codex_tab(&state);
        gate_start(&mut state.borrow_mut(), tab, first_send("r1", "hello codex")).unwrap();
        let prompt = next_prompt(&state);
        assert_eq!(prompt.tab, tab);
        assert!(fx.external.borrow().is_empty(), "nothing before the answer");
        answer_prompt(&state, "a1", &prompt, true);
        tick_until(&state, |_| !fx.external.borrow().is_empty());

        let started = fx.external.borrow()[0].clone();
        assert_eq!(started.agent, AgentId::Codex);
        assert_eq!(started.mode, SessionModeChoice::Auto);
        assert_eq!(started.resume, None);
        let plan = &started.plan;
        assert_eq!(
            plan.fingerprint, prompt.fingerprint,
            "the plan is the shown discovery's"
        );
        assert_eq!(plan.project_only, ["srv"]);
        assert!(!plan.skills_off);
        let root = fx.root.canonicalize().unwrap();
        let expected: Vec<(PathBuf, agent::DirTrust)> = root
            .ancestors()
            .map(|dir| {
                let level = if dir == root {
                    agent::DirTrust::Trusted
                } else {
                    agent::DirTrust::Untrusted
                };
                (dir.to_path_buf(), level)
            })
            .collect();
        assert_eq!(plan.dirs, expected);
        assert_eq!(plan.dirs.last().unwrap().0, PathBuf::from("/"));
        assert!(fx.spawned.borrow().is_empty(), "the Claude spawner is not used");
        {
            let s = state.borrow();
            match &s.tabs.get(tab).unwrap().backend {
                TabBackend::Starting(pending) => {
                    assert_eq!(pending.request_id, "r1");
                    assert_eq!(pending.first_turn.as_ref().unwrap().typed, "hello codex");
                }
                _ => panic!("the Codex tab connects once the record is written"),
            }
        }

        // `:untrust`, then an `n` in a second Codex tab: every directory untrusted, skills off.
        trust_command(
            &mut state.borrow_mut(),
            ViewId::BASE,
            "u1",
            eitri_core::agent_bridge::TrustAction::Untrust,
            "codex",
        )
        .unwrap();
        tick_until(&state, |seen| result_for(&all_payloads(seen), "u1").is_some());
        let second = codex_tab(&state);
        gate_start(&mut state.borrow_mut(), second, first_send("r2", "again")).unwrap();
        let prompt = next_prompt(&state);
        assert_eq!(prompt.tab, second);
        answer_prompt(&state, "n1", &prompt, false);
        assert_eq!(fx.external.borrow().len(), 2);
        let plan = fx.external.borrow()[1].plan.clone();
        assert_eq!(plan.fingerprint, prompt.fingerprint);
        assert!(plan.skills_off);
        assert!(plan.dirs.iter().all(|(_, level)| *level == agent::DirTrust::Untrusted));
        assert_eq!(plan.dirs.len(), root.ancestors().count());
    }

    /// A resume of a Codex session runs the gate again and is given a plan built from what is on disk
    /// now, not the one its first start had.
    #[test]
    fn resume_reasks_and_rebuilds_the_plan() {
        let (state, fx) = trust_panel("trust-external-resume");
        fx.write("AGENTS.md", b"first\n");
        let first = codex_tab(&state);
        gate_start(&mut state.borrow_mut(), first, first_send("r1", "hello")).unwrap();
        let prompt = next_prompt(&state);
        answer_prompt(&state, "a1", &prompt, true);
        tick_until(&state, |_| !fx.external.borrow().is_empty());
        let before = fx.external.borrow()[0].plan.clone();

        fx.write("AGENTS.md", b"changed since\n");
        let second = codex_tab(&state);
        let resume = DeferredStart::Resume {
            request_id: "r2".into(),
            provider_session_id: "thread-1".into(),
            resumed_title: None,
            resumed_name: None,
        };
        gate_start(&mut state.borrow_mut(), second, resume).unwrap();
        let again = next_prompt(&state);
        assert_eq!(again.tab, second, "the changed configuration is asked about again");
        assert_ne!(again.fingerprint, prompt.fingerprint);
        assert_eq!(fx.external.borrow().len(), 1, "nothing resumes before the answer");
        answer_prompt(&state, "a2", &again, true);
        tick_until(&state, |_| fx.external.borrow().len() == 2);
        let resumed = fx.external.borrow()[1].clone();
        assert_eq!(resumed.resume.as_deref(), Some("thread-1"));
        assert_eq!(resumed.plan.fingerprint, again.fingerprint);
        assert_ne!(resumed.plan.fingerprint, before.fingerprint);
        {
            let s = state.borrow();
            match &s.tabs.get(second).unwrap().backend {
                TabBackend::Starting(pending) => assert_eq!(pending.resume.as_deref(), Some("thread-1")),
                _ => panic!("the resume connects once the record is written"),
            }
        }

        // Unchanged since, a later resume is let through by the record without a question, and is
        // still given a plan of its own from a fresh check.
        let third = codex_tab(&state);
        let resume = DeferredStart::Resume {
            request_id: "r3".into(),
            provider_session_id: "thread-1".into(),
            resumed_title: None,
            resumed_name: None,
        };
        gate_start(&mut state.borrow_mut(), third, resume).unwrap();
        let seen = tick_until(&state, |_| fx.external.borrow().len() == 3);
        assert!(prompts(&all_payloads(&seen)).is_empty());
        assert_eq!(fx.external.borrow()[2].plan.fingerprint, again.fingerprint);
    }

    /// A resume `r` continues in the tab its session stopped in is started holding the wait for that tab's old
    /// backend, which keeps the session's lease until it is gone; a fresh start in a tab is never held by a
    /// wait left there.
    #[test]
    fn a_resume_continued_by_r_waits_for_the_old_backend() {
        let (state, fx) = trust_panel("trust-resume-waits");
        let started = |n: usize| {
            let seen = tick_until(&state, |seen| {
                fx.external.borrow().len() == n || !prompts(&all_payloads(seen)).is_empty()
            });
            if let Some(p) = prompts(&all_payloads(&seen)).pop() {
                answer_prompt(&state, "a", &p, true);
                tick_until(&state, |_| fx.external.borrow().len() == n);
            }
        };
        let tab = codex_tab(&state);
        let (_old_gone, gone) = mpsc::channel::<()>();
        state.borrow_mut().resume_after.insert(tab, ("thread-1".into(), gone));
        resume_in(&mut state.borrow_mut(), tab, "r1", "thread-1".into(), true).unwrap();
        started(1);
        let spawned = fx.external.borrow()[0].clone();
        assert_eq!(spawned.resume.as_deref(), Some("thread-1"));
        assert!(spawned.waits_for_old);
        assert!(state.borrow().resume_after.is_empty());

        let other = codex_tab(&state);
        let (_never, left) = mpsc::channel::<()>();
        state.borrow_mut().resume_after.insert(other, ("thread-1".into(), left));
        gate_start(&mut state.borrow_mut(), other, first_send("r2", "hello")).unwrap();
        started(2);
        assert!(!fx.external.borrow()[1].waits_for_old);
        assert!(state.borrow().resume_after.is_empty());

        // A resume of another session than the one the wait is for does not wait either.
        let third = codex_tab(&state);
        let (_never, left) = mpsc::channel::<()>();
        state.borrow_mut().resume_after.insert(third, ("thread-1".into(), left));
        resume_in(&mut state.borrow_mut(), third, "r3", "thread-9".into(), true).unwrap();
        started(3);
        assert_eq!(fx.external.borrow()[2].resume.as_deref(), Some("thread-9"));
        assert!(!fx.external.borrow()[2].waits_for_old);
        assert!(state.borrow().resume_after.is_empty());
    }

    /// No trust check result for the tab's agent (its worker never ran): the external start is
    /// dropped with the reason and the typed text goes back to the composer; nothing is spawned.
    #[test]
    fn an_external_start_without_a_bound_discovery_is_refused() {
        let (state, fx) = trust_panel("trust-external-unbound");
        state.borrow_mut().trust = TrustGate::without_worker(fx.root.clone());
        let tab = codex_tab(&state);
        gate_start(&mut state.borrow_mut(), tab, first_send("r1", "hello")).unwrap();
        let seen = tick_until(&state, |seen| result_for(&all_payloads(seen), "r1").is_some());
        let payloads = all_payloads(&seen);
        let why = "Codex was not started: the trust check of this project has not run";
        assert_eq!(result_for(&payloads, "r1"), Some(Err(why.to_string())));
        assert!(notices(&payloads).contains(&why.to_string()));
        assert!(fx.external.borrow().is_empty());
        assert!(matches!(
            state.borrow().tabs.get(tab).unwrap().backend,
            TabBackend::NotStarted
        ));
    }

    /// A project whose trust question would be larger than the page is ever sent (here one hook of
    /// 3 MiB) is not shown in part. Claude starts without the project's configuration on an `n`, so it is
    /// asked a short question that has no `y`: a `y` is refused and the question asked again, nothing is
    /// started or recorded, and an `n` starts the session untrusted. `:trust` is told the configuration
    /// is too large and shown nothing.
    #[test]
    fn a_trust_prompt_too_large_to_show_is_a_short_question_with_no_y() {
        let (state, fx) = trust_panel("trust-too-large");
        fx.write(PROJECT_SETTINGS, &session_start_hook(&"x".repeat(3 << 20)));
        let tab = state.borrow().tabs.active();
        gate_start(&mut state.borrow_mut(), tab, first_send("r1", "hello")).unwrap();
        let seen = tick_until(&state, |seen| !prompts(&all_payloads(seen)).is_empty());
        let raw = all_payloads(&seen)
            .into_iter()
            .find(|p| json_of(p)["kind"] == "trust_prompt")
            .unwrap();
        assert!(
            raw.len() < 4096,
            "no part of the configuration is sent: {} bytes",
            raw.len()
        );
        let prompt = prompts(&all_payloads(&seen)).pop().unwrap();
        assert_eq!(
            (prompt.state.as_str(), prompt.remember.as_str()),
            ("too_large", "window")
        );

        // A `y` is refused and the question asked again; nothing started or recorded.
        let out = answer_prompt(&state, "y1", &prompt, true);
        assert_eq!(
            result_for(&out.payloads, "y1"),
            Some(Err(crate::trust_gate::Y_NOT_TAKEN.to_string()))
        );
        let again = prompts(&out.payloads);
        assert_eq!(again.len(), 1, "asked again: {:?}", out.payloads);
        assert_ne!(again[0].nonce, prompt.nonce);
        assert!(fx.spawned.borrow().is_empty() && !fx.record_file().exists());
        assert!(matches!(
            state.borrow().tabs.get(tab).unwrap().backend,
            TabBackend::AwaitingTrust(_)
        ));

        // An `n` starts the session without the project's configuration.
        let out = answer_prompt(&state, "n1", &again[0], false);
        assert_eq!(result_for(&out.payloads, "n1"), Some(Ok(())));
        assert!(notices(&out.payloads).contains(&crate::trust_gate::NOT_LOADED.to_string()));
        assert_eq!(*fx.spawned.borrow(), [(None, ProjectTrust::Untrusted)]);
        assert!(!fx.record_file().exists());

        // `:trust` has nothing to trust: told, with nothing shown.
        use eitri_core::agent_bridge::TrustAction;
        trust_command(
            &mut state.borrow_mut(),
            ViewId::BASE,
            "c1",
            TrustAction::Trust,
            "claude",
        )
        .unwrap();
        let seen = tick_until(&state, |seen| result_for(&all_payloads(seen), "c1").is_some());
        let payloads = all_payloads(&seen);
        assert!(prompts(&payloads).is_empty());
        let refused = result_for(&payloads, "c1").unwrap().unwrap_err();
        assert!(refused.contains("too large to show whole"), "{refused}");
        assert!(!fx.record_file().exists());
    }

    /// The trust worker dies after a check of a registered agent's configuration came back: the next
    /// start is routed untrusted with that check's discovery still held, and is refused because such
    /// an agent starts only after a `y`. Nothing is spawned.
    #[test]
    fn an_untrusted_start_of_an_agent_that_needs_a_yes_is_refused() {
        let (state, fx) = trust_panel("trust-external-explicit-yes");
        state.borrow_mut().acp_agents = vec![gem_registration()];
        let agent = AgentId::Acp("gem".into());
        let profile = trust_profile(&state.borrow().acp_agents, &agent);
        state.borrow_mut().trust.begin_check(&profile);
        tick_until(&state, |_| state.borrow().trust.bound_discovery(&profile).is_some());
        state.borrow_mut().trust.drop_worker();
        let tab = {
            let mut s = state.borrow_mut();
            let tab = s.tabs.open(agent);
            s.tabs.select(tab);
            tab
        };
        gate_start(&mut state.borrow_mut(), tab, first_send("r1", "hello")).unwrap();
        let seen = tick_until(&state, |seen| result_for(&all_payloads(seen), "r1").is_some());
        let payloads = all_payloads(&seen);
        let why = crate::trust_gate::refused_without_yes(&profile);
        assert_eq!(result_for(&payloads, "r1"), Some(Err(why.clone())));
        assert!(notices(&payloads).contains(&why));
        assert!(fx.external.borrow().is_empty());
        assert!(fx.spawned.borrow().is_empty());
        assert!(matches!(
            state.borrow().tabs.get(tab).unwrap().backend,
            TabBackend::NotStarted
        ));
    }

    /// A registered agent as `init.lua` would give it: a command, two project files and a mode.
    fn gem_registration() -> AgentRegistration {
        AgentRegistration {
            name: "gem".into(),
            command: vec!["gem-acp".into(), "--stdio".into()],
            project_files: vec![".gem/".into(), "GEM.md".into()],
            auto_mode: Some("auto_edit".into()),
        }
    }

    /// A tab of the registered `gem`, opened and on screen.
    fn gem_tab(state: &Rc<RefCell<AgentPanelState>>) -> TabId {
        let mut s = state.borrow_mut();
        s.acp_agents = vec![gem_registration()];
        let tab = s.tabs.open(AgentId::Acp("gem".into()));
        s.tabs.select(tab);
        tab
    }

    fn prompt_json(payloads: &[String]) -> serde_json::Value {
        payloads
            .iter()
            .map(|p| json_of(p))
            .rfind(|v| v["kind"] == "trust_prompt")
            .expect("a trust prompt")
    }

    /// `:tabnew <name>` opens a tab of a registered agent; a name nothing registered is refused with
    /// the accepted words, the registered ones among them.
    #[test]
    fn registered_agent_offered_by_tabnew() {
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        let refused = open_tab_named(&state, ViewId::BASE, "gem").unwrap_err();
        assert_eq!(refused, "unknown agent gem — :tabnew accepts claude, codex");
        state.borrow_mut().acp_agents = vec![gem_registration()];
        open_tab_named(&state, ViewId::BASE, "gem").unwrap();
        assert_eq!(state.borrow().tabs.active_tab().agent(), &AgentId::Acp("gem".into()));
        let refused = open_tab_named(&state, ViewId::BASE, "other").unwrap_err();
        assert_eq!(refused, "unknown agent other — :tabnew accepts claude, codex, gem");
        assert!(open_tab_named(&state, ViewId::BASE, "acp:gem").is_err());
    }

    /// A registered agent in a root with none of its files is still asked about, with the question
    /// saying there are none and that `n` does not start it; a `y` records the empty configuration and
    /// starts the agent with its registered command and mode.
    #[test]
    fn acp_root_with_nothing_found_asks_and_records_the_empty_fingerprint() {
        let (state, fx) = trust_panel("acp-nothing-found");
        fx.write("CLAUDE.md", b"another agent's file, not gem's\n");
        let tab = gem_tab(&state);
        gate_start(&mut state.borrow_mut(), tab, first_send("r1", "hello")).unwrap();
        let seen = tick_until(&state, |seen| !prompts(&all_payloads(seen)).is_empty());
        let payloads = all_payloads(&seen);
        let shown = prompt_json(&payloads);
        assert_eq!(shown["agent"], "gem");
        assert_eq!(shown["state"], "nothing");
        assert_eq!(shown["needsYes"], true);
        assert_eq!(shown["items"], serde_json::json!([]));
        let prompt = prompts(&payloads).pop().unwrap();
        assert_eq!(prompt.tab, tab);
        assert!(fx.external.borrow().is_empty(), "nothing before the answer");

        let out = answer_prompt(&state, "a1", &prompt, true);
        assert_eq!(result_for(&out.payloads, "a1"), Some(Ok(())));
        tick_until(&state, |_| !fx.external.borrow().is_empty());
        let record = PathBuf::from(format!("{}.acp-gem", fx.record_file().display()));
        let written: serde_json::Value = serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
        assert_eq!(written["fingerprint"], prompt.fingerprint.as_str());
        assert!(!fx.record_file().exists(), "Claude's record is untouched");
        let started = fx.external.borrow()[0].clone();
        assert_eq!(started.agent, AgentId::Acp("gem".into()));
        assert_eq!(
            started.acp,
            Some((
                vec![std::ffi::OsString::from("gem-acp"), std::ffi::OsString::from("--stdio")],
                Some("auto_edit".to_string())
            ))
        );
        assert!(
            fx.spawned.borrow().is_empty(),
            "nothing went through the Claude spawner"
        );
    }

    /// An `n` to a registered agent's question starts nothing and says why; it is not remembered, so the
    /// next start asks again.
    #[test]
    fn acp_n_refuses_with_the_notice() {
        let (state, fx) = trust_panel("acp-n-refuses");
        fx.write("GEM.md", b"gem instructions\n");
        let tab = gem_tab(&state);
        gate_start(&mut state.borrow_mut(), tab, first_send("r1", "hello")).unwrap();
        let seen = tick_until(&state, |seen| !prompts(&all_payloads(seen)).is_empty());
        let payloads = all_payloads(&seen);
        assert_eq!(prompt_json(&payloads)["state"], "untrusted");
        assert_eq!(prompt_json(&payloads)["needsYes"], true);
        assert_eq!(items_of_prompt(&payloads), ["GEM.md"]);
        let prompt = prompts(&payloads).pop().unwrap();

        let out = answer_prompt(&state, "a1", &prompt, false);
        assert_eq!(
            result_for(&out.payloads, "a1"),
            Some(Ok(())),
            "the answer is taken; the start is refused"
        );
        let payloads = out.payloads;
        let why = "gem would load this project's own configuration, which you have not trusted; Eitri cannot \
                   start it without loading that";
        let profile = trust_profile(&state.borrow().acp_agents, &AgentId::Acp("gem".into()));
        assert_eq!(crate::trust_gate::refused_without_yes(&profile), why);
        assert_eq!(result_for(&payloads, "r1"), Some(Err(why.to_string())));
        assert!(notices(&payloads).contains(&why.to_string()), "{payloads:?}");
        assert!(fx.external.borrow().is_empty());
        assert!(fx.spawned.borrow().is_empty());

        gate_start(&mut state.borrow_mut(), tab, first_send("r2", "again")).unwrap();
        let again = next_prompt(&state);
        assert_eq!(again.tab, tab);
        assert!(fx.external.borrow().is_empty());
    }

    /// Trusted with nothing found, a registered agent starts without asking; once one of its project
    /// files appears, the fingerprint no longer matches and the next start asks again, naming it.
    #[test]
    fn a_file_appearing_later_reasks() {
        let (state, fx) = trust_panel("acp-file-later");
        let tab = gem_tab(&state);
        gate_start(&mut state.borrow_mut(), tab, first_send("r1", "hello")).unwrap();
        let prompt = next_prompt(&state);
        answer_prompt(&state, "a1", &prompt, true);
        tick_until(&state, |_| fx.external.borrow().len() == 1);

        let second = {
            let mut s = state.borrow_mut();
            let second = s.tabs.open(AgentId::Acp("gem".into()));
            s.tabs.select(second);
            second
        };
        gate_start(&mut state.borrow_mut(), second, first_send("r2", "hello")).unwrap();
        tick_until(&state, |_| fx.external.borrow().len() == 2);

        fx.write(".gem/settings.json", b"{\"tools\": \"all\"}\n");
        let third = {
            let mut s = state.borrow_mut();
            let third = s.tabs.open(AgentId::Acp("gem".into()));
            s.tabs.select(third);
            third
        };
        gate_start(&mut state.borrow_mut(), third, first_send("r3", "hello")).unwrap();
        let seen = tick_until(&state, |seen| !prompts(&all_payloads(seen)).is_empty());
        let payloads = all_payloads(&seen);
        assert_eq!(prompts(&payloads).pop().unwrap().tab, third);
        assert_eq!(prompt_json(&payloads)["state"], "changed");
        assert_eq!(fx.external.borrow().len(), 2, "the third start waits for the answer");
    }

    #[test]
    fn a_first_send_in_an_untrusted_root_spawns_nothing_until_answered() {
        let (state, fx) = trust_panel("trust-first-send");
        fx.write(PROJECT_SETTINGS, &session_start_hook("touch /tmp/never"));
        let tab = state.borrow().tabs.active();
        gate_start(&mut state.borrow_mut(), tab, first_send("r1", "hello")).unwrap();
        let prompt = next_prompt(&state);
        assert_eq!(prompt.tab, tab);
        assert_eq!(prompt.remember, "yes");
        assert!(fx.spawned.borrow().is_empty(), "nothing before the answer");

        let out = answer_prompt(&state, "a1", &prompt, true);
        assert_eq!(result_for(&out.payloads, "a1"), Some(Ok(())));
        assert!(fx.spawned.borrow().is_empty(), "nothing before the record is written");
        assert!(matches!(
            state.borrow().tabs.get(tab).unwrap().backend,
            TabBackend::AwaitingTrust(AwaitingTrust { recording: true, .. })
        ));

        tick_until(&state, |_| !fx.spawned.borrow().is_empty());
        assert_eq!(*fx.spawned.borrow(), [(None, ProjectTrust::Trusted)]);
        assert!(fx.record_file().is_file());
        let s = state.borrow();
        let t = s.tabs.get(tab).unwrap();
        assert_eq!(t.project_tiers, Some(ProjectTrust::Trusted));
        match &t.backend {
            TabBackend::Starting(pending) => {
                assert_eq!(pending.request_id, "r1");
                let first = pending
                    .first_turn
                    .as_ref()
                    .expect("the first message goes once connected");
                assert_eq!((first.wire.as_str(), first.typed.as_str()), ("wire: hello", "hello"));
            }
            _ => panic!("the tab connects once the record is written"),
        }
    }

    /// A resume by hand waits for the question like a first message, and an `n` resumes it without
    /// the project's tiers.
    #[test]
    fn a_resume_waits_for_the_question_and_n_resumes_it_untrusted() {
        let (state, fx) = trust_panel("trust-resume");
        fx.write(PROJECT_SETTINGS, &session_start_hook("touch /tmp/never"));
        let tab = state.borrow().tabs.active();
        let resume = DeferredStart::Resume {
            request_id: "r1".into(),
            provider_session_id: "s-1".into(),
            resumed_title: Some("about s-1".into()),
            resumed_name: None,
        };
        gate_start(&mut state.borrow_mut(), tab, resume).unwrap();
        let prompt = next_prompt(&state);
        assert!(fx.spawned.borrow().is_empty());
        let out = answer_prompt(&state, "n1", &prompt, false);
        assert_eq!(result_for(&out.payloads, "n1"), Some(Ok(())));
        assert_eq!(
            *fx.spawned.borrow(),
            [(Some("s-1".to_string()), ProjectTrust::Untrusted)]
        );
        let s = state.borrow();
        match &s.tabs.get(tab).unwrap().backend {
            TabBackend::Starting(pending) => {
                assert_eq!(pending.resume.as_deref(), Some("s-1"));
                assert_eq!(pending.resumed_title.as_deref(), Some("about s-1"));
            }
            _ => panic!("the resume connects on the answer"),
        }
    }

    /// A check made for tab B (the hook changed meanwhile) takes A's prompt off; A's answer to what it
    /// was shown before is refused and A is asked about what is there now. Nothing is recorded and
    /// nothing starts.
    #[test]
    fn two_tabs_an_older_prompt_cannot_record_a_newer_discovery() {
        let (state, fx) = trust_panel("trust-two-tabs");
        fx.write(PROJECT_SETTINGS, &session_start_hook("touch /tmp/one"));
        let (a, b) = {
            let mut s = state.borrow_mut();
            let a = s.tabs.active();
            let b = s.tabs.open(eitri_core::tab_set::AgentId::Claude);
            s.tabs.select(a);
            (a, b)
        };
        gate_start(&mut state.borrow_mut(), a, first_send("ra", "to a")).unwrap();
        let first = next_prompt(&state);
        assert_eq!(first.tab, a);

        fx.write(PROJECT_SETTINGS, &session_start_hook("touch /tmp/two"));
        gate_start(&mut state.borrow_mut(), b, first_send("rb", "to b")).unwrap();
        let second = next_prompt(&state);
        assert_eq!(second.tab, a, "the tab on screen is asked again at once");
        assert_ne!(second.fingerprint, first.fingerprint);
        assert!(!state.borrow().trust.is_waiting(b));

        // The old prompt's nonce is spent.
        let out = answer_prompt(&state, "x1", &first, true);
        assert_eq!(
            result_for(&out.payloads, "x1"),
            Some(Err(crate::trust_gate::NOT_CURRENT.to_string()))
        );
        // The current nonce, echoing what the old prompt showed: refused, and asked again.
        let stale_echo = Prompt {
            nonce: second.nonce,
            ..first.clone()
        };
        let out = answer_prompt(&state, "x2", &stale_echo, true);
        assert_eq!(
            result_for(&out.payloads, "x2"),
            Some(Err(crate::trust_gate::OUT_OF_DATE.to_string()))
        );
        let again = prompts(&out.payloads);
        assert_eq!(again.len(), 1, "{:?}", out.payloads);
        assert_eq!(again[0].fingerprint, second.fingerprint);

        std::thread::sleep(std::time::Duration::from_millis(50));
        trust_events(&mut state.borrow_mut());
        assert!(!fx.record_file().exists());
        assert!(fx.spawned.borrow().is_empty());
        for tab in [a, b] {
            assert!(matches!(
                state.borrow().tabs.get(tab).unwrap().backend,
                TabBackend::AwaitingTrust(_)
            ));
        }
    }

    #[test]
    fn a_failed_record_is_said_in_the_panel_and_starts_nothing() {
        let (state, fx) = trust_panel("trust-record-fails");
        fx.write(PROJECT_SETTINGS, &session_start_hook("touch /tmp/never"));
        std::fs::create_dir_all(fx.state.join("eitri")).unwrap();
        std::fs::write(fx.state.join("eitri/trust"), b"not a directory\n").unwrap();
        let tab = state.borrow().tabs.active();
        gate_start(&mut state.borrow_mut(), tab, first_send("r1", "hello")).unwrap();
        let prompt = next_prompt(&state);

        let began = std::time::Instant::now();
        let out = answer_prompt(&state, "a1", &prompt, true);
        assert!(
            began.elapsed() < std::time::Duration::from_millis(50),
            "the handler only queues"
        );
        assert_eq!(result_for(&out.payloads, "a1"), Some(Ok(())));

        let seen = tick_until(&state, |seen| !prompts(&all_payloads(seen)).is_empty());
        let payloads = all_payloads(&seen);
        let said = notices(&payloads);
        assert!(
            said.iter().any(|n| n.starts_with("trust could not be recorded (")
                && n.ends_with("); asking again for this window only")),
            "{said:?}"
        );
        let asked = prompts(&payloads).pop().unwrap();
        assert_eq!(asked.remember, "window");
        assert!(fx.spawned.borrow().is_empty());
        assert!(!fx.record_file().exists());

        // The window-only `y` also waits for the worker to find the disk unchanged: the handler
        // starts nothing, the tick does, and says the trust lasts this window only.
        let out = answer_prompt(&state, "a2", &asked, true);
        assert_eq!(result_for(&out.payloads, "a2"), Some(Ok(())));
        assert!(fx.spawned.borrow().is_empty(), "not started from the handler");
        let seen = tick_until(&state, |_| !fx.spawned.borrow().is_empty());
        assert_eq!(*fx.spawned.borrow(), vec![(None, ProjectTrust::Trusted)]);
        assert!(
            notices(&all_payloads(&seen))
                .iter()
                .any(|n| n.starts_with("trusted for this window only: ")),
            "{seen:?}",
            seen = all_payloads(&seen)
        );
        assert!(!fx.record_file().exists());
    }

    /// At launch nothing of an untrusted root starts and no saved session is leased while the
    /// question waits; an `n` then brings every saved tab back without the project's tiers.
    #[test]
    fn automatic_restore_never_starts_an_untrusted_session() {
        let (state, dir) = restore_panel(
            "trust-restore-launch",
            &[("s-1", None, Auto), ("s-2", None, Auto)],
            &["s-1", "s-2"],
        );
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        let canonical = dir.to_string_lossy().into_owned();
        {
            let mut s = state.borrow_mut();
            s.restore_policy = RestorePolicy::Auto;
            s.page_mut(ViewId::BASE).document_ready = true;
        }
        let mut resumes = FakeResumes::default();
        let result = launch_restore(&mut state.borrow_mut());
        assert!(matches!(result, Some(RestoreResult::Deferred)));
        let prompt = with_resumes(&state, &mut resumes, || next_prompt(&state));
        assert!(resumes.asked.is_empty(), "nothing is resumed while the question waits");
        for id in ["s-1", "s-2"] {
            let lease = agent::lease::SessionLease::try_acquire("claude", &canonical, id);
            assert!(lease.is_ok(), "{id} is not leased while the question waits");
        }
        assert!(
            state.borrow().restore_source.is_none(),
            "a launch has its one go, even while it waits"
        );
        assert!(
            matches!(
                ask_for_restore(&mut state.borrow_mut(), prompt.tab, RestoreAsk::Dashboard),
                RestoreResult::Refused(_)
            ),
            "a second restore waits for the first"
        );

        let out = with_resumes(&state, &mut resumes, || answer_prompt(&state, "n1", &prompt, false));
        assert!(out.restored.is_some(), "the restore starts on the answer");
        assert_eq!(resumes.asked, ["s-1", "s-2"]);
        assert_eq!(resumes.trusts, [ProjectTrust::Untrusted; 2]);
        assert!(notices(&out.payloads).contains(&crate::trust_gate::NOT_LOADED.to_string()));
        for t in state.borrow().tabs.tabs() {
            assert_eq!(t.project_tiers, Some(ProjectTrust::Untrusted));
        }
    }

    /// A restore waiting for the trust question keeps its sessions in the saved-tabs file, and so does
    /// one put off with `Escape`: they come back as failed tabs holding their resume ids, names and
    /// modes, so a session started afterwards does not write over them.
    #[test]
    fn a_restore_put_off_by_escape_keeps_its_sessions_in_the_saved_file() {
        let (state, dir) = restore_panel(
            "trust-restore-escape",
            &[("s-1", Some("api"), Auto), ("s-2", None, Auto)],
            &["s-1", "s-2"],
        );
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        {
            let mut s = state.borrow_mut();
            s.restore_policy = RestorePolicy::Auto;
            s.page_mut(ViewId::BASE).document_ready = true;
        }
        let mut resumes = FakeResumes::default();
        assert!(matches!(
            launch_restore(&mut state.borrow_mut()),
            Some(RestoreResult::Deferred)
        ));
        let prompt = with_resumes(&state, &mut resumes, || next_prompt(&state));
        let files = agent::state_dirs::test_workspace_dir("trust-restore-escape-files");
        state.borrow_mut().tab_memory = TabWriter::new(Some(files.clone()), dir.clone());
        let both = vec![("s-1".to_string(), Some("api".to_string())), ("s-2".to_string(), None)];
        remember_tabs_written(&state);
        assert_eq!(saved_file_sessions(&files, &dir), both, "kept while the question waits");

        let out = cancel_trust_question(&mut state.borrow_mut(), "c1", prompt.tab, prompt.nonce, ViewId::BASE);
        assert_eq!(result_for(&out.payloads, "c1"), Some(Ok(())));
        assert_eq!(out.toast.as_deref(), Some(crate::trust_gate::PUT_OFF));
        assert!(resumes.asked.is_empty(), "nothing was resumed");
        // A session started afterwards is written beside them, not over them.
        let (other, _provider) = start_session_in_new_tab(&state, &dir, "s-new");
        remember_tabs_written(&state);
        let mut with_new = both.clone();
        with_new.push(("s-new".to_string(), None));
        assert_eq!(saved_file_sessions(&files, &dir), with_new, "kept after it is put off");
        let mut closed = state.borrow_mut().tabs.remove(other).unwrap();
        if let TabBackend::Live(backend) = &mut closed.backend {
            backend.shutdown();
        }
        let s = state.borrow();
        let failed: Vec<_> = s
            .tabs
            .tabs()
            .iter()
            .filter(|t| matches!(&t.backend, TabBackend::Failed { reason } if reason == crate::trust_gate::PUT_OFF))
            .map(|t| t.failed_resume.clone())
            .collect();
        assert_eq!(failed, [Some("s-1".to_string()), Some("s-2".to_string())]);
    }

    /// A put-off restore's tab saved in an extra view is kept there, as a failed tab, and that view's
    /// page is shown it; the saved file still names the view.
    #[test]
    fn a_restore_put_off_keeps_a_tab_in_its_saved_view_and_shows_it_there() {
        let (state, dir) = restore_panel(
            "trust-restore-escape-view",
            &[("s-1", None, Auto), ("s-2", None, Auto)],
            &["s-1", "s-2"],
        );
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        {
            let mut s = state.borrow_mut();
            s.restore_source.as_mut().unwrap().tabs[0].view = Some(2);
            s.restore_policy = RestorePolicy::Auto;
            s.page_mut(ViewId::BASE).document_ready = true;
            s.page_mut(view(2)).document_ready = true;
            s.tabs.set_layout_views(vec![view(2)]);
        }
        let mut resumes = FakeResumes::default();
        assert!(matches!(
            launch_restore(&mut state.borrow_mut()),
            Some(RestoreResult::Deferred)
        ));
        let prompt = with_resumes(&state, &mut resumes, || next_prompt(&state));
        let out = cancel_trust_question(&mut state.borrow_mut(), "c1", prompt.tab, prompt.nonce, ViewId::BASE);
        assert_eq!(result_for(&out.payloads, "c1"), Some(Ok(())));
        assert!(resumes.asked.is_empty(), "nothing was resumed");

        let s = state.borrow();
        let kept = s.tabs.tab_in(view(2)).expect("the tab saved in agent#2 is kept there");
        assert_eq!(s.tabs.get(kept).unwrap().failed_resume.as_deref(), Some("s-1"));
        assert!(s.tabs.view_of(s.tabs.active()).is_base());
        let to_view: Vec<serde_json::Value> = out
            .payloads
            .iter()
            .zip(&out.dests)
            .filter(|(_, dest)| dest_view(&s, **dest) == view(2))
            .map(|(payload, _)| json_of(payload))
            .collect();
        assert!(
            to_view.iter().any(|v| v["kind"] == "tabs" && v["active"] == kept.0),
            "its page is shown the kept tab: {to_view:?}"
        );
        let saved = s.tabs.saved_snapshot(&s.conversation_id).unwrap();
        let mut views: Vec<(String, Option<u8>)> = saved
            .tabs
            .iter()
            .map(|t| (t.provider_session_id.clone(), t.view))
            .collect();
        views.sort();
        assert_eq!(views, [("s-1".to_string(), Some(2)), ("s-2".to_string(), None)]);
    }

    /// A restore asked for from the launch dashboard leaves the saved tabs on offer: put off with
    /// `Escape`, its tab is empty again, the offer stands (the same key asks again) and the file is not
    /// touched, as before restores kept their sessions as failed tabs.
    #[test]
    fn a_dashboard_restore_put_off_by_escape_stays_on_offer() {
        let (state, dir) = restore_panel(
            "trust-restore-dashboard-escape",
            &[("s-1", None, Auto), ("s-2", None, Auto)],
            &["s-1", "s-2"],
        );
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        let files = agent::state_dirs::test_workspace_dir("trust-restore-dashboard-escape-files");
        state.borrow_mut().tab_memory = TabWriter::new(Some(files.clone()), dir.clone());
        state.borrow_mut().page_mut(ViewId::BASE).document_ready = true;
        let from = state.borrow().tabs.active();
        let mut resumes = FakeResumes::default();
        let result = ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard);
        assert!(
            matches!(result, RestoreResult::Deferred),
            "no bypass tab, so no question"
        );
        let prompt = with_resumes(&state, &mut resumes, || next_prompt(&state));
        remember_tabs_written(&state);
        assert!(
            std::fs::read_dir(&files).unwrap().next().is_none(),
            "the file is not written while it waits with nothing else to save"
        );
        let out = cancel_trust_question(&mut state.borrow_mut(), "c1", prompt.tab, prompt.nonce, ViewId::BASE);
        assert_eq!(result_for(&out.payloads, "c1"), Some(Ok(())));
        remember_tabs_written(&state);
        assert!(std::fs::read_dir(&files).unwrap().next().is_none(), "nor after");
        let s = state.borrow();
        assert!(s.tabs.is_pristine(), "the tab is empty again");
        assert!(restore_is_on_offer(&s), "the same key can ask again");
    }

    /// `remember_tabs`, then waits until the writer thread has written what it was handed, so the test
    /// can read the file.
    fn remember_tabs_written(state: &Rc<RefCell<AgentPanelState>>) {
        remember_tabs(state);
        assert!(state
            .borrow()
            .tab_memory
            .wait_written(std::time::Duration::from_secs(5)));
    }

    /// Runs the tick on its own cadence until it says it waits for the wake (at most ten seconds of real
    /// time: workers answer meanwhile); the moment of the last tick.
    fn tick_until_quiet(handle: &AgentPanelHandle, mut at: std::time::Instant) -> std::time::Instant {
        use crate::panel_wake::NextTick;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match handle.run_tick(at) {
                NextTick::OnWake => return at,
                NextTick::At(next) => at = next,
                NextTick::Stop => panic!("the tick stopped"),
            }
            assert!(std::time::Instant::now() < deadline, "the tick never went quiet");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// The trust worker builds the prompt beside its check and tells nobody when it is done: the prompt
    /// reaches the page only through the tick that polls the gate. Held on the worker, the tick keeps its
    /// steady cadence; let go, the prompt is sent and the tick waits for the answer.
    #[test]
    fn a_prompt_built_on_the_trust_worker_is_polled_for_until_it_reaches_the_page() {
        use crate::panel_wake::NextTick;
        let (handle, page) = settled_panel();
        let outer = agent::state_dirs::test_workspace_dir("panel-trust-polled");
        let home = outer.join("home");
        let state_home = outer.join("state");
        let root = home.join("p");
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::create_dir_all(&state_home).unwrap();
        std::fs::write(root.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        let hold = crate::trust_gate::WorkerHold::closed();
        {
            let mut s = handle.state.borrow_mut();
            s.project_dir = root.clone();
            s.canonical_project_dir = root.to_string_lossy().into_owned();
            let store =
                eitri_core::project_trust::TrustStore::new(Some(state_home.as_os_str()), Some(home.as_os_str()));
            s.trust = TrustGate::new_held(store, root.clone(), Some(home.clone()), hold.clone());
            let tab = s.tabs.active();
            gate_start(&mut s, tab, first_send("r1", "hello")).unwrap();
        }
        let sent = page.sent.borrow().len();
        let prompt_sent = || page.sent.borrow()[sent..].iter().any(|p| kind_of(p) == "trust_prompt");
        let mut at = std::time::Instant::now();
        for _ in 0..5 {
            assert_eq!(
                handle.run_tick(at),
                NextTick::At(at + AgentPanelHandle::TICK),
                "the worker owes an answer: polled"
            );
            at += AgentPanelHandle::TICK;
        }
        assert!(!prompt_sent());
        hold.open();
        tick_until_quiet(&handle, at);
        assert!(
            prompt_sent(),
            "the prompt reached the page: {:?}",
            &page.sent.borrow()[sent..]
        );
    }

    /// A dashboard restore waits for the trust question while another tab's session opens. That session's
    /// id arrives on the backend's own thread, whose fold wakes the tick; the tick it brings settles the
    /// waiting restore and hands the writer a set with the waiting sessions in it.
    #[test]
    fn a_waiting_restore_is_settled_and_written_by_the_tick_a_fold_wakes() {
        let (state, dir) = restore_panel(
            "trust-restore-settle-wake",
            &[("s-1", Some("api"), Auto), ("s-2", None, Auto)],
            &["s-1", "s-2"],
        );
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        let files = agent::state_dirs::test_workspace_dir("trust-restore-settle-wake-files");
        let page = Rc::new(RecordingPage::default());
        page.visible.set(true);
        let handle = AgentPanelHandle::over(
            state.clone(),
            Some(Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>))),
        );
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        state.borrow_mut().tab_memory = TabWriter::new(Some(files.clone()), dir.clone());
        let from = state.borrow().tabs.active();
        assert!(matches!(
            ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard),
            RestoreResult::Deferred
        ));
        let at = tick_until_quiet(&handle, std::time::Instant::now());
        assert!(
            page.sent.borrow().iter().any(|p| kind_of(p) == "trust_prompt"),
            "the restore waits for the question"
        );
        assert!(state.borrow().restore_source.is_some(), "still on offer");

        // Another tab's backend; the tick that finds it hands it the wake.
        let other = state.borrow_mut().tabs.open(AgentId::Claude);
        state.borrow_mut().tabs.select(from);
        let (provider, backend) = live_backend(&dir);
        state.borrow_mut().tabs.get_mut(other).unwrap().backend = TabBackend::Live(backend);
        let at = tick_until_quiet(&handle, at + AgentPanelHandle::TICK);
        handle.take_wake(at);

        provider.open_session("s-new", &dir);
        let due = wait_for_wake(&handle);
        tick_until_quiet(&handle, due);
        assert!(state.borrow().restore_source.is_none(), "the offer is spent");
        assert!(state
            .borrow()
            .tab_memory
            .wait_written(std::time::Duration::from_secs(5)));
        let ids: Vec<String> = saved_file_sessions(&files, &dir)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, ["s-new", "s-1", "s-2"], "written with the waiting sessions");
        let mut closed = state.borrow_mut().tabs.remove(other).unwrap();
        if let TabBackend::Live(backend) = &mut closed.backend {
            backend.shutdown();
        }
    }

    /// The saved file as the window last wrote it: each Claude session with its name.
    fn saved_file_sessions(files: &std::path::Path, dir: &std::path::Path) -> Vec<(String, Option<String>)> {
        match eitri_core::saved_tabs::load(files, dir) {
            eitri_core::saved_tabs::Loaded::Saved(saved) => saved
                .tabs
                .into_iter()
                .map(|t| (t.provider_session_id, t.name))
                .collect(),
            other => panic!("{other:?}"),
        }
    }

    /// Starts a session in a new tab of the window, as a typed first message would, and waits for its id.
    /// The tab on screen stays on screen.
    fn start_session_in_new_tab(
        state: &Rc<RefCell<AgentPanelState>>,
        dir: &std::path::Path,
        session: &str,
    ) -> (TabId, std::sync::Arc<RecordingProvider>) {
        let back = state.borrow().tabs.active();
        let tab = state.borrow_mut().tabs.open(AgentId::Claude);
        state.borrow_mut().tabs.select(back);
        let (provider, backend) = live_backend(dir);
        state.borrow_mut().tabs.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.open_session(session, dir);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.borrow().tabs.get(tab).unwrap().provider_session_id().is_none() {
            assert!(std::time::Instant::now() < deadline, "the session id never arrived");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        (tab, provider)
    }

    /// A dashboard restore waits for the trust question while another tab starts a session: the file
    /// the window then writes keeps the waiting sessions, also once that other tab is closed again, and
    /// put off with `Escape` they come back as failed tabs instead of being left out of every tab.
    #[test]
    fn a_waiting_dashboard_restore_stays_in_the_file_when_another_tab_starts_a_session() {
        let (state, dir) = restore_panel(
            "trust-restore-dashboard-other",
            &[("s-1", Some("api"), Auto), ("s-2", None, Auto)],
            &["s-1", "s-2"],
        );
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        let files = agent::state_dirs::test_workspace_dir("trust-restore-dashboard-other-files");
        state.borrow_mut().tab_memory = TabWriter::new(Some(files.clone()), dir.clone());
        state.borrow_mut().page_mut(ViewId::BASE).document_ready = true;
        let from = state.borrow().tabs.active();
        let mut resumes = FakeResumes::default();
        assert!(matches!(
            ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard),
            RestoreResult::Deferred
        ));
        let _ = with_resumes(&state, &mut resumes, || next_prompt(&state));

        let (other, _provider) = start_session_in_new_tab(&state, &dir, "s-new");
        remember_tabs_written(&state);
        let waiting = vec![("s-1".to_string(), Some("api".to_string())), ("s-2".to_string(), None)];
        let mut all = vec![("s-new".to_string(), None)];
        all.extend(waiting.clone());
        assert_eq!(
            saved_file_sessions(&files, &dir),
            all,
            "written with the waiting sessions"
        );
        assert!(state.borrow().restore_source.is_none(), "the offer is spent");

        // Closing the other tab again does not take the waiting sessions back out of the file.
        let mut closed = state.borrow_mut().tabs.remove(other).unwrap();
        if let TabBackend::Live(backend) = &mut closed.backend {
            backend.shutdown();
        }
        remember_tabs_written(&state);
        assert_eq!(
            saved_file_sessions(&files, &dir),
            waiting,
            "still listed while they wait"
        );

        let prompt = with_resumes(&state, &mut resumes, || next_prompt(&state));
        let out = cancel_trust_question(&mut state.borrow_mut(), "c1", prompt.tab, prompt.nonce, ViewId::BASE);
        assert_eq!(result_for(&out.payloads, "c1"), Some(Ok(())));
        assert!(resumes.asked.is_empty(), "nothing was resumed");
        remember_tabs_written(&state);
        assert_eq!(saved_file_sessions(&files, &dir), waiting, "kept after it is put off");
        let s = state.borrow();
        let failed: Vec<_> = s
            .tabs
            .tabs()
            .iter()
            .filter(|t| matches!(&t.backend, TabBackend::Failed { reason } if reason == crate::trust_gate::PUT_OFF))
            .map(|t| t.failed_resume.clone())
            .collect();
        assert_eq!(failed, [Some("s-1".to_string()), Some("s-2".to_string())]);
    }

    /// A put-off while another tab's session is still starting (no id yet, so nothing of it is saved)
    /// keeps the sessions as failed tabs: that tab writes the file once its id arrives, and the offer
    /// could not be shown again beside it.
    #[test]
    fn a_dashboard_restore_put_off_beside_a_starting_session_keeps_its_sessions() {
        let (state, dir) = restore_panel(
            "trust-restore-dashboard-starting",
            &[("s-1", None, Auto), ("s-2", None, Auto)],
            &["s-1", "s-2"],
        );
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        let files = agent::state_dirs::test_workspace_dir("trust-restore-dashboard-starting-files");
        state.borrow_mut().tab_memory = TabWriter::new(Some(files.clone()), dir.clone());
        state.borrow_mut().page_mut(ViewId::BASE).document_ready = true;
        let from = state.borrow().tabs.active();
        let mut resumes = FakeResumes::default();
        assert!(matches!(
            ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard),
            RestoreResult::Deferred
        ));
        let prompt = with_resumes(&state, &mut resumes, || next_prompt(&state));
        // Another tab starts a session whose id has not arrived.
        let other = state.borrow_mut().tabs.open(AgentId::Claude);
        state.borrow_mut().tabs.select(from);
        let (provider, backend) = live_backend(&dir);
        state.borrow_mut().tabs.get_mut(other).unwrap().backend = TabBackend::Live(backend);
        let nonce = state.borrow_mut().tabs.ask_trust(from).unwrap();
        let out = cancel_trust_question(&mut state.borrow_mut(), "c1", prompt.tab, nonce, ViewId::BASE);
        assert_eq!(result_for(&out.payloads, "c1"), Some(Ok(())));
        assert!(state.borrow().restore_source.is_none(), "no longer on offer");
        remember_tabs_written(&state);

        provider.open_session("s-new", &dir);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.borrow().tabs.get(other).unwrap().provider_session_id().is_none() {
            assert!(std::time::Instant::now() < deadline, "the session id never arrived");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        remember_tabs_written(&state);
        let ids: Vec<String> = saved_file_sessions(&files, &dir)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, ["s-1", "s-new", "s-2"]);
        let mut closed = state.borrow_mut().tabs.remove(other).unwrap();
        if let TabBackend::Live(backend) = &mut closed.backend {
            backend.shutdown();
        }
    }

    /// A put-off that comes before the tick has written anything (the other session started in the same
    /// tick) still keeps the sessions, since the file is about to be written without them.
    #[test]
    fn a_dashboard_restore_put_off_beside_another_session_keeps_its_sessions() {
        let (state, dir) = restore_panel(
            "trust-restore-dashboard-same-tick",
            &[("s-1", None, Auto), ("s-2", None, Auto)],
            &["s-1", "s-2"],
        );
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        let files = agent::state_dirs::test_workspace_dir("trust-restore-dashboard-same-tick-files");
        state.borrow_mut().tab_memory = TabWriter::new(Some(files.clone()), dir.clone());
        state.borrow_mut().page_mut(ViewId::BASE).document_ready = true;
        let from = state.borrow().tabs.active();
        let mut resumes = FakeResumes::default();
        assert!(matches!(
            ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard),
            RestoreResult::Deferred
        ));
        let _ = with_resumes(&state, &mut resumes, || next_prompt(&state));
        let (other, _provider) = start_session_in_new_tab(&state, &dir, "s-new");
        // Asked again with no tick between (a tick would write the file first).
        let nonce = state.borrow_mut().tabs.ask_trust(from).unwrap();
        let out = cancel_trust_question(&mut state.borrow_mut(), "c1", from, nonce, ViewId::BASE);
        assert_eq!(result_for(&out.payloads, "c1"), Some(Ok(())));
        assert!(state.borrow().restore_source.is_none(), "no longer on offer");
        remember_tabs_written(&state);
        let ids: Vec<String> = saved_file_sessions(&files, &dir)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, ["s-1", "s-new", "s-2"], "the first in the tab it waited in");
        let mut closed = state.borrow_mut().tabs.remove(other).unwrap();
        if let TabBackend::Live(backend) = &mut closed.backend {
            backend.shutdown();
        }
    }

    /// A session of a waiting restore that a failed tab already holds (a resume by hand that failed) is
    /// listed once.
    #[test]
    fn a_waiting_restore_does_not_list_a_session_a_failed_tab_holds() {
        let (state, dir) = restore_panel(
            "trust-restore-no-twice",
            &[("s-1", None, Auto), ("s-2", None, Auto)],
            &["s-1", "s-2"],
        );
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        {
            let mut s = state.borrow_mut();
            s.restore_policy = RestorePolicy::Auto;
            s.page_mut(ViewId::BASE).document_ready = true;
        }
        let mut resumes = FakeResumes::default();
        assert!(matches!(
            launch_restore(&mut state.borrow_mut()),
            Some(RestoreResult::Deferred)
        ));
        let _ = with_resumes(&state, &mut resumes, || next_prompt(&state));
        {
            let mut s = state.borrow_mut();
            let other = s.tabs.open(AgentId::Claude);
            let t = s.tabs.get_mut(other).unwrap();
            t.backend = TabBackend::Failed { reason: "x".into() };
            t.failed_resume = Some("s-1".into());
        }
        let saved = state.borrow().tabs.saved_snapshot("conv").unwrap();
        let ids: Vec<_> = saved.tabs.iter().map(|t| t.provider_session_id.as_str()).collect();
        assert_eq!(ids.iter().filter(|id| **id == "s-1").count(), 1, "{ids:?}");
        assert!(ids.contains(&"s-2"), "{ids:?}");
    }

    /// A restore whose start is refused outright (here by the panel's own put-off, as an agent that
    /// needs a `y` gets after an `n`) keeps its sessions the same way, saying why on the failed tabs.
    #[test]
    fn a_restore_whose_start_is_refused_keeps_its_sessions_in_the_saved_file() {
        let (state, dir) = restore_panel(
            "trust-restore-refused",
            &[("s-1", None, Auto), ("s-2", Some("two"), Auto)],
            &["s-1", "s-2"],
        );
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        {
            let mut s = state.borrow_mut();
            s.restore_policy = RestorePolicy::Auto;
            s.page_mut(ViewId::BASE).document_ready = true;
        }
        let mut resumes = FakeResumes::default();
        assert!(matches!(
            launch_restore(&mut state.borrow_mut()),
            Some(RestoreResult::Deferred)
        ));
        let prompt = with_resumes(&state, &mut resumes, || next_prompt(&state));
        let why = "not started: for the test";
        {
            let mut s = state.borrow_mut();
            let start = s.tabs.take_deferred(prompt.tab).expect("the restore waits in its tab");
            assert!(matches!(start, DeferredStart::Restore { .. }));
            let out = put_off_start(&mut s, prompt.tab, start, why);
            assert_eq!(out.toast.as_deref(), Some(why));
        }
        let s = state.borrow();
        let saved = s.tabs.saved_snapshot("conv").unwrap();
        let ids: Vec<_> = saved.tabs.iter().map(|t| t.provider_session_id.as_str()).collect();
        assert_eq!(ids, ["s-1", "s-2"]);
        assert_eq!(saved.tabs[1].name.as_deref(), Some("two"));
        for t in s.tabs.tabs() {
            assert!(
                matches!(&t.backend, TabBackend::Failed { reason } if reason == why),
                "{:?}",
                t.id
            );
        }
        assert!(resumes.asked.is_empty());
    }

    /// A stand-in `codex` that prints `version` for `--version`, in a fresh scratch directory.
    fn fake_codex(label: &str, version: &str) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = agent::state_dirs::test_workspace_dir(label);
        let program = dir.join("codex");
        std::fs::write(&program, format!("#!/bin/sh\necho 'codex-cli {version}'\n")).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        (dir, program)
    }

    /// The band says the self-check is running exactly when the start is about to run it: a binary with no
    /// passing verdict kept. A kept verdict, a binary too old to check, or no state directory say nothing.
    #[test]
    fn a_first_codex_start_says_it_is_checking_this_codex() {
        let (dir, program) = fake_codex("codex-check-note", "0.161.0");
        let state_home = dir.join("state");
        assert!(codex_check_pending(&program, Some(&state_home)));
        assert!(!codex_check_pending(&program, None));
        let key = agent::providers::codex::BinaryKey::of(&program).unwrap();
        let verdict = state_home.join(CODEX_CHECK_DIR).join(key.id()).join("verdict.json");
        std::fs::create_dir_all(verdict.parent().unwrap()).unwrap();
        std::fs::write(&verdict, b"{}").unwrap();
        assert!(
            !codex_check_pending(&program, Some(&state_home)),
            "checked once is checked"
        );

        let (old_dir, old) = fake_codex("codex-check-note-old", "0.100.0");
        assert!(!codex_check_pending(&old, Some(&old_dir.join("state"))));

        let notes = StartNotes::default();
        notes.sender().send(CHECKING_CODEX.to_string()).unwrap();
        let sent = notes.drain();
        assert_eq!(sent.len(), 1);
        assert_eq!(json_of(&sent[0])["kind"], "notice");
        assert_eq!(
            json_of(&sent[0])["text"],
            "checking this Codex\u{2026} (once for this binary, a few seconds)"
        );
        assert!(notes.drain().is_empty());
    }

    /// The band's guess reads the self-check's own verdict file: the two names must stay the same.
    #[test]
    fn the_check_note_reads_where_the_self_check_writes() {
        let source = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent/src/providers/codex/self_check.rs"),
        )
        .unwrap();
        assert!(source.contains(&format!("const CHECK_DIR: &str = \"{CODEX_CHECK_DIR}\";")));
        assert!(source.contains("dir.join(\"verdict.json\")"));
    }

    /// A panel from [`restore_panel`] whose last window also left a Codex tab (`thread-9`, the one on
    /// screen), with its record, and a spawner that records what it was asked to start.
    #[allow(clippy::type_complexity)]
    fn restore_panel_with_codex(
        label: &str,
    ) -> (
        Rc<RefCell<AgentPanelState>>,
        PathBuf,
        Rc<RefCell<Vec<String>>>,
        Rc<RefCell<Vec<ExternalSpawned>>>,
    ) {
        restore_panel_with_external(label, AgentId::Codex, "thread-9")
    }

    /// [`restore_panel_with_codex`] for a tab of `agent` holding `session`.
    #[allow(clippy::type_complexity)]
    fn restore_panel_with_external(
        label: &str,
        agent: AgentId,
        session: &str,
    ) -> (
        Rc<RefCell<AgentPanelState>>,
        PathBuf,
        Rc<RefCell<Vec<String>>>,
        Rc<RefCell<Vec<ExternalSpawned>>>,
    ) {
        let (state, dir) = restore_panel(label, &[("s-1", None, Auto)], &["s-1"]);
        let conversation_id = agent::conversation_id_for_cwd(&dir);
        agent::persistence::save_conversation_record(&agent::persistence::ConversationRecord {
            conversation_id: conversation_id.clone(),
            provider: agent.record_name(),
            provider_session_id: session.to_string(),
            canonical_cwd: dir.to_string_lossy().into_owned(),
            created_at: "1".to_string(),
            updated_at: "2".to_string(),
            provider_advertised_resume: true,
            title: Some(format!("title of {session}")),
            name: None,
        })
        .unwrap();
        let resumed = Rc::new(RefCell::new(Vec::new()));
        let external = Rc::new(RefCell::new(Vec::new()));
        {
            let mut s = state.borrow_mut();
            s.page_mut(ViewId::BASE).document_ready = true;
            let saved = s.restore_source.as_mut().unwrap();
            saved.external_tabs = vec![eitri_core::saved_tabs::SavedExternalTab {
                agent,
                conversation_id,
                provider_session_id: session.to_string(),
                name: None,
                mode: Auto,
                view: None,
            }];
            saved.active_external = Some(0);
            let (resumed, external) = (resumed.clone(), external.clone());
            s.spawner = Spawner::Fake(Rc::new(RefCell::new(move |call: SpawnCall| {
                match call {
                    SpawnCall::Resume { id, .. } => resumed.borrow_mut().push(id),
                    SpawnCall::Connect { .. } => panic!("a restore resumes; it never connects afresh"),
                    SpawnCall::External {
                        agent,
                        plan,
                        mode,
                        resume,
                        acp,
                        after_old,
                    } => external.borrow_mut().push(ExternalSpawned {
                        agent,
                        plan,
                        mode,
                        resume,
                        acp: acp.map(|spec| (spec.argv, spec.auto_mode)),
                        waits_for_old: after_old.is_some(),
                    }),
                }
                mpsc::channel().1
            })));
        }
        (state, dir, resumed, external)
    }

    /// A saved Codex tab comes back after the Claude tabs' answer, holding its resume for Codex's own
    /// answer (here nothing to trust, so given at once), and is resumed as a resume by hand would be.
    #[test]
    fn a_saved_codex_tab_is_restored_through_its_own_trust_answer() {
        let (state, _dir, resumed, external) = restore_panel_with_codex("restore-codex");
        let from = state.borrow().tabs.active();
        let result = ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard);
        assert!(matches!(result, RestoreResult::Deferred));
        tick_until(&state, |_| !external.borrow().is_empty());
        assert_eq!(*resumed.borrow(), ["s-1"]);
        let spawned = external.borrow();
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].agent, AgentId::Codex);
        assert_eq!(spawned[0].resume.as_deref(), Some("thread-9"));
        assert_eq!(spawned[0].mode, Auto);
        assert!(spawned[0].plan.skills_off, "nothing to trust starts untrusted");
        let s = state.borrow();
        let codex = s
            .tabs
            .tab_with_session(&AgentId::Codex, "thread-9")
            .expect("the Codex tab is back");
        assert_eq!(s.tabs.get(codex).unwrap().agent(), &AgentId::Codex);
        assert_eq!(s.tabs.active(), codex, "the saved active tab is on screen");
        let saved = s.tabs.saved_snapshot("conv").unwrap();
        assert_eq!(saved.tabs.len(), 1);
        assert_eq!(saved.external_tabs.len(), 1, "the Codex tab is still saved");
    }

    /// A saved tab of a registered ACP agent comes back holding its resume and waits for that agent's own
    /// question, which it is asked even with nothing found; a `y` resumes it with the registered command,
    /// and an `n` starts nothing.
    #[test]
    fn a_saved_acp_tab_is_restored_only_after_its_own_yes() {
        for yes in [true, false] {
            let label = if yes { "restore-acp-yes" } else { "restore-acp-no" };
            let gem = AgentId::Acp("gem".into());
            let (state, _dir, resumed, external) = restore_panel_with_external(label, gem.clone(), "gem-s");
            state.borrow_mut().acp_agents = vec![gem_registration()];
            let from = state.borrow().tabs.active();
            let result = ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard);
            assert!(matches!(result, RestoreResult::Deferred));
            let prompt = next_prompt(&state);
            assert_eq!(*resumed.borrow(), ["s-1"], "Claude has nothing to trust and started");
            assert!(external.borrow().is_empty(), "nothing of gem starts while it is asked");
            let waiting = state
                .borrow()
                .tabs
                .tab_with_session(&AgentId::Acp("gem".into()), "gem-s")
                .expect("the gem tab is back");
            assert_eq!(prompt.tab, waiting);
            let saved = state.borrow().tabs.saved_snapshot("conv").unwrap();
            assert_eq!(saved.external_tabs.len(), 1, "kept in the saved file while it waits");
            assert_eq!(saved.external_tabs[0].agent, gem);

            answer_prompt(&state, "a1", &prompt, yes);
            if yes {
                tick_until(&state, |_| !external.borrow().is_empty());
                let spawned = external.borrow();
                assert_eq!(spawned.len(), 1);
                assert_eq!(spawned[0].agent, gem);
                assert_eq!(spawned[0].resume.as_deref(), Some("gem-s"));
                assert_eq!(
                    spawned[0].acp,
                    Some((
                        vec![std::ffi::OsString::from("gem-acp"), std::ffi::OsString::from("--stdio")],
                        Some("auto_edit".to_string())
                    ))
                );
            } else {
                for _ in 0..20 {
                    trust_events(&mut state.borrow_mut());
                }
                assert!(external.borrow().is_empty(), "an n starts nothing");
            }
        }
    }

    /// A saved tab of an ACP agent `init.lua` no longer registers is skipped, said to be, and never
    /// asked about; the Claude tabs come back as usual, and the next save leaves it out.
    #[test]
    fn a_saved_tab_of_an_unregistered_acp_agent_is_skipped() {
        let gem = AgentId::Acp("gem".into());
        let (state, _dir, resumed, external) = restore_panel_with_external("restore-acp-gone", gem, "gem-s");
        assert!(state.borrow().acp_agents.is_empty());
        let plan = restore_plan(&state.borrow());
        assert_eq!(plan.items.len(), 1, "only the Claude tab");
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].reason, eitri_core::tab_restore::NOT_REGISTERED);
        let from = state.borrow().tabs.active();
        let result = ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard);
        assert!(matches!(result, RestoreResult::Deferred));
        tick_until(&state, |_| !resumed.borrow().is_empty());
        assert_eq!(*resumed.borrow(), ["s-1"]);
        assert!(external.borrow().is_empty());
        let s = state.borrow();
        assert!(
            s.tabs.tab_with_session(&AgentId::Acp("gem".into()), "gem-s").is_none(),
            "no tab is made for it"
        );
        let saved = s.tabs.saved_snapshot("conv").unwrap();
        assert!(saved.external_tabs.is_empty(), "the next save leaves it out");
    }

    /// When the project has something for Codex to load, the restored Codex tab waits for that
    /// question without starting, and stays in the saved file meanwhile; the answer starts it.
    #[test]
    fn a_restored_codex_tab_waiting_for_its_question_stays_saved() {
        let (state, dir, resumed, external) = restore_panel_with_codex("restore-codex-asks");
        std::fs::write(dir.join("AGENTS.md"), b"be careful\n").unwrap();
        let from = state.borrow().tabs.active();
        let result = ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard);
        assert!(matches!(result, RestoreResult::Deferred));
        let prompt = next_prompt(&state);
        assert_eq!(*resumed.borrow(), ["s-1"], "Claude has nothing to trust and started");
        assert!(
            external.borrow().is_empty(),
            "nothing of Codex starts while it is asked"
        );
        let codex = state
            .borrow()
            .tabs
            .tab_with_session(&AgentId::Codex, "thread-9")
            .expect("the Codex tab is back");
        assert_eq!(prompt.tab, codex);
        assert!(matches!(
            state.borrow().tabs.get(codex).unwrap().backend,
            TabBackend::AwaitingTrust(_)
        ));
        let saved = state.borrow().tabs.saved_snapshot("conv").unwrap();
        assert_eq!(saved.external_tabs.len(), 1, "kept in the saved file while it waits");
        assert_eq!(saved.external_tabs[0].provider_session_id, "thread-9");

        let out = answer_prompt(&state, "n1", &prompt, false);
        assert_eq!(result_for(&out.payloads, "n1"), Some(Ok(())));
        let spawned = external.borrow();
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].resume.as_deref(), Some("thread-9"));
        assert!(spawned[0].plan.skills_off, "an n starts it untrusted");
    }

    /// A restore of Codex tabs alone asks Codex's question, once, and never Claude's.
    #[test]
    fn a_restore_of_codex_tabs_alone_asks_only_codexs_question() {
        let (state, dir, resumed, external) = restore_panel_with_codex("restore-codex-only");
        state.borrow_mut().restore_source.as_mut().unwrap().tabs.clear();
        // Claude would have something to ask about too; it is not asked.
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        std::fs::write(dir.join("AGENTS.md"), b"be careful\n").unwrap();
        let from = state.borrow().tabs.active();
        let result = ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard);
        assert!(matches!(result, RestoreResult::Deferred));
        let seen = tick_until(&state, |seen| !prompts(&all_payloads(seen)).is_empty());
        let asked: Vec<serde_json::Value> = all_payloads(&seen)
            .iter()
            .map(|p| json_of(p))
            .filter(|v| v["kind"] == "trust_prompt")
            .collect();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0]["agent"], "Codex");
        let prompt = prompts(&all_payloads(&seen)).pop().unwrap();
        let out = answer_prompt(&state, "n1", &prompt, false);
        assert_eq!(result_for(&out.payloads, "n1"), Some(Ok(())));
        let seen = tick_until(&state, |_| !external.borrow().is_empty());
        assert!(
            prompts(&all_payloads(&seen)).is_empty(),
            "the window's answer covers the tab"
        );
        assert!(resumed.borrow().is_empty());
        assert_eq!(external.borrow()[0].resume.as_deref(), Some("thread-9"));
    }

    /// A saved bypass tab's question is answered first; only then is the trust question asked, and
    /// still nothing has been leased.
    #[test]
    fn the_restore_bypass_question_comes_before_the_trust_question() {
        let (state, dir) = restore_panel("trust-restore-bypass", &[("s-1", None, Bypass)], &["s-1"]);
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        state.borrow_mut().page_mut(ViewId::BASE).document_ready = true;
        let from = state.borrow().tabs.active();
        let RestoreResult::Question(question) = ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard)
        else {
            panic!("a bypass tab is asked about first")
        };
        assert!(state.borrow().tabs.awaiting_trust().is_empty());
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(prompts(&trust_events(&mut state.borrow_mut()).payloads).is_empty());

        let answered = answer_restore_question(&mut state.borrow_mut(), question.nonce, true);
        assert!(matches!(answered, RestoreResult::Deferred));
        let mut resumes = FakeResumes::default();
        let prompt = with_resumes(&state, &mut resumes, || next_prompt(&state));
        assert_eq!(prompt.tab, from);
        assert!(resumes.asked.is_empty());
        let canonical = dir.to_string_lossy().into_owned();
        assert!(agent::lease::SessionLease::try_acquire("claude", &canonical, "s-1").is_ok());
    }

    /// A restore question's `y` resumes the saved bypass tabs in bypass, so from a chat a float covers
    /// it meets the same gate as a card answer: refused, nothing is started and the question is kept,
    /// so the same `y` goes through once the float is off the chat.
    #[test]
    fn a_bypass_restore_y_from_a_covered_chat_is_refused_and_the_question_stays() {
        const COVERED: &str = "agent is under terminal 2: C-b M-- from terminal 2 stows it";
        let (state, dir) = restore_panel("gate-restore-y", &[("s-1", None, Bypass)], &["s-1"]);
        let page = Rc::new(RecordingPage::default());
        let handle = AgentPanelHandle::over(
            state.clone(),
            Some(Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>))),
        );
        // Attaching the page starts its record afresh, so it is ready only from here.
        state.borrow_mut().page_mut(ViewId::BASE).document_ready = true;
        let from = state.borrow().tabs.active();
        let RestoreResult::Question(question) = ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard)
        else {
            panic!("a bypass tab is asked about first")
        };
        let covered = Rc::new(Cell::new(true));
        let asked = Rc::new(Cell::new(0));
        handle.set_answer_gate(Rc::new({
            let covered = covered.clone();
            let asked = asked.clone();
            move |view: ViewId| {
                assert_eq!(view, ViewId::BASE, "asked with the view of the page that answered");
                asked.set(asked.get() + 1);
                if covered.get() {
                    Err(COVERED.to_string())
                } else {
                    Ok(())
                }
            }
        }));
        let answer = |request: &str| {
            handle.page_message(
                ViewId::BASE,
                &format!(
                    r#"{{"type":"restore_answer","request_id":"{request}","nonce":{},"keep_bypass":true}}"#,
                    question.nonce
                ),
            );
        };

        answer("r1");
        assert_eq!(result_for(&page.sent.borrow(), "r1"), Some(Err(COVERED.to_string())));
        assert_eq!(asked.get(), 1, "the gate is asked once, with the base view");
        assert!(
            state.borrow().tabs.awaiting_trust().is_empty(),
            "nothing waits to start"
        );
        let canonical = dir.to_string_lossy().into_owned();
        assert!(
            agent::lease::SessionLease::try_acquire("claude", &canonical, "s-1").is_ok(),
            "nothing was started"
        );

        covered.set(false);
        answer("r2");
        assert_eq!(
            result_for(&page.sent.borrow(), "r2"),
            Some(Ok(())),
            "the question was kept for the same nonce"
        );
        assert_eq!(asked.get(), 2);
    }

    /// A restore question's `n` resumes nothing in bypass, so it is not asked of the gate.
    #[test]
    fn a_restore_n_is_not_asked_of_the_gate() {
        let (state, _dir) = restore_panel("gate-restore-n", &[("s-1", None, Bypass)], &["s-1"]);
        let page = Rc::new(RecordingPage::default());
        let handle = AgentPanelHandle::over(
            state.clone(),
            Some(Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>))),
        );
        // Attaching the page starts its record afresh, so it is ready only from here.
        state.borrow_mut().page_mut(ViewId::BASE).document_ready = true;
        let from = state.borrow().tabs.active();
        let RestoreResult::Question(question) = ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard)
        else {
            panic!("a bypass tab is asked about first")
        };
        let asked = Rc::new(Cell::new(0));
        handle.set_answer_gate(Rc::new({
            let asked = asked.clone();
            move |_view: ViewId| {
                asked.set(asked.get() + 1);
                Err("covered".to_string())
            }
        }));
        handle.page_message(
            ViewId::BASE,
            &format!(
                r#"{{"type":"restore_answer","request_id":"n1","nonce":{},"keep_bypass":false}}"#,
                question.nonce
            ),
        );
        assert_eq!(result_for(&page.sent.borrow(), "n1"), Some(Ok(())));
        assert_eq!(asked.get(), 0, "an `n` is not asked of the gate");
    }

    #[test]
    fn an_answer_for_another_tab_is_refused() {
        let (state, fx) = trust_panel("trust-other-tab");
        fx.write(PROJECT_SETTINGS, &session_start_hook("touch /tmp/never"));
        let (a, b) = {
            let mut s = state.borrow_mut();
            let a = s.tabs.active();
            let b = s.tabs.open(eitri_core::tab_set::AgentId::Claude);
            s.tabs.select(a);
            (a, b)
        };
        gate_start(&mut state.borrow_mut(), a, first_send("ra", "to a")).unwrap();
        let prompt = next_prompt(&state);
        // Named for the other tab: refused.
        let other = Prompt {
            tab: b,
            ..prompt.clone()
        };
        let out = answer_prompt(&state, "x1", &other, true);
        assert!(matches!(result_for(&out.payloads, "x1"), Some(Err(_))));
        // The right tab, but no longer on screen: refused too.
        state.borrow_mut().tabs.select(b);
        let out = answer_prompt(&state, "x2", &prompt, true);
        assert!(matches!(result_for(&out.payloads, "x2"), Some(Err(_))));
        std::thread::sleep(std::time::Duration::from_millis(50));
        trust_events(&mut state.borrow_mut());
        assert!(fx.spawned.borrow().is_empty());
        assert!(!fx.record_file().exists());
    }

    /// A first message put off while the user typed on: the composer's text since the send (Rust's
    /// mirrored copy of the box) is kept after the message, as the page merges it, so the `draft` that
    /// follows the refusal replaces the box with both and loses neither.
    #[test]
    fn a_put_off_first_message_comes_back_with_what_was_typed_since() {
        let (state, fx) = trust_panel("trust-escape-typed-since");
        fx.write(PROJECT_SETTINGS, &session_start_hook("touch /tmp/never"));
        let tab = state.borrow().tabs.active();
        gate_start(&mut state.borrow_mut(), tab, first_send("r1", "hello")).unwrap();
        let prompt = next_prompt(&state);
        state.borrow_mut().tabs.set_draft(tab, "and more");
        let out = cancel_trust_question(&mut state.borrow_mut(), "c1", tab, prompt.nonce, ViewId::BASE);
        let drafts: Vec<_> = out
            .payloads
            .iter()
            .map(|p| json_of(p))
            .filter(|v| v["kind"] == "draft")
            .collect();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0]["text"], "hello\nand more");
        assert_eq!(
            drafts[0]["refused"], "r1",
            "it names the refusal it follows, so the page can skip it"
        );
        assert_eq!(state.borrow().tabs.get(tab).unwrap().draft, "hello\nand more");
    }

    /// `Escape` on the question for a first message: the tab is empty again and the message is back in
    /// the box, once: the refused send comes before the draft, so the page does not put it back twice.
    #[test]
    fn escape_returns_the_first_message_to_the_composer() {
        let (state, fx) = trust_panel("trust-escape");
        fx.write(PROJECT_SETTINGS, &session_start_hook("touch /tmp/never"));
        let tab = state.borrow().tabs.active();
        gate_start(&mut state.borrow_mut(), tab, first_send("r1", "hello")).unwrap();
        let prompt = next_prompt(&state);
        let out = cancel_trust_question(&mut state.borrow_mut(), "c1", tab, prompt.nonce, ViewId::BASE);
        assert_eq!(result_for(&out.payloads, "c1"), Some(Ok(())));
        assert_eq!(
            result_for(&out.payloads, "r1"),
            Some(Err(crate::trust_gate::NOT_STARTED.to_string()))
        );
        let at = |kind: &str| out.payloads.iter().position(|p| json_of(p)["kind"] == kind).unwrap();
        let refused = out
            .payloads
            .iter()
            .position(|p| json_of(p)["requestId"] == "r1")
            .unwrap();
        assert!(refused < at("draft"), "{:?}", out.payloads);
        let drafts: Vec<_> = out
            .payloads
            .iter()
            .map(|p| json_of(p))
            .filter(|v| v["kind"] == "draft")
            .collect();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0]["text"], "hello");
        assert_eq!(drafts[0]["refused"], "r1");
        let s = state.borrow();
        let t = s.tabs.get(tab).unwrap();
        assert!(matches!(t.backend, TabBackend::NotStarted));
        assert_eq!(t.draft, "hello");
        drop(s);

        // A resume put off is dropped with a toast.
        gate_start(
            &mut state.borrow_mut(),
            tab,
            DeferredStart::Resume {
                request_id: "r2".into(),
                provider_session_id: "s-1".into(),
                resumed_title: None,
                resumed_name: None,
            },
        )
        .unwrap();
        let prompt = next_prompt(&state);
        let out = cancel_trust_question(&mut state.borrow_mut(), "c2", tab, prompt.nonce, ViewId::BASE);
        assert_eq!(out.toast.as_deref(), Some(crate::trust_gate::PUT_OFF));
        assert_eq!(
            result_for(&out.payloads, "r2"),
            Some(Err(crate::trust_gate::PUT_OFF.to_string()))
        );
        assert!(fx.spawned.borrow().is_empty());
    }

    /// `settings` says what the tab's own session was given once it has one, and what the next would
    /// get before; `trust` (the window's latest check) sits right after it.
    #[test]
    fn the_settings_and_trust_rows_are_per_tab() {
        let user = agent::setting_sources::loads_user_settings();
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let rows_for = |set: &TabSet, next: ProjectTrust| {
            detail_rows(
                set.active_tab(),
                BackendKind::Sidecar,
                None,
                std::path::Path::new("/p"),
                None,
                "trusted since 2026-10-02",
                next,
                &[],
                Some("Space k"),
            )
        };
        let value = |rows: &[DetailRow], label: &str| rows.iter().find(|r| r.label == label).unwrap().value.clone();

        let rows = rows_for(&set, ProjectTrust::Trusted);
        assert_eq!(
            value(&rows, "settings"),
            agent::setting_sources::note_for_session(user, ProjectTrust::Trusted),
            "an empty tab: what its next session would get"
        );
        assert_eq!(value(&rows, "trust"), "trusted since 2026-10-02");
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        let position = |label: &str| labels.iter().position(|l| *l == label).unwrap();
        assert_eq!(position("settings"), position("CLI") + 1, "{labels:?}");
        assert_eq!(position("trust"), position("settings") + 1, "{labels:?}");
        assert_eq!(rows.len(), 20);

        set.active_tab_mut().project_tiers = Some(ProjectTrust::Untrusted);
        let rows = rows_for(&set, ProjectTrust::Trusted);
        assert_eq!(
            value(&rows, "settings"),
            agent::setting_sources::note_for_session(user, ProjectTrust::Untrusted),
            "a started tab keeps what its session was given"
        );
    }

    fn codex_rows(mode: SessionModeChoice, next: ProjectTrust) -> Vec<DetailRow> {
        // A window never starts in bypass; only the user's own `agent.default_mode` puts new tabs there.
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        set.apply_configured_default(mode);
        let tab = set.open(AgentId::Codex);
        detail_rows(
            set.get(tab).unwrap(),
            BackendKind::Sidecar,
            Some("work"),
            std::path::Path::new("/p/root"),
            Some(std::path::Path::new("/s/eitri/permissions/x.json")),
            "trusted since 2026-10-02",
            next,
            &[],
            Some("Space k"),
        )
    }

    /// A Codex tab says, in `prefix i`, what its sandbox lets through without a question and how a mode
    /// change reaches its reviewer; and shows none of the rows that describe Claude's machinery.
    #[test]
    fn detail_rows_for_a_codex_tab_state_what_runs_without_asking_and_the_mode_lag() {
        let rows = codex_rows(SessionModeChoice::Auto, ProjectTrust::Untrusted);
        let value = |label: &str| rows.iter().find(|r| r.label == label).unwrap().value.clone();
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "name",
                "title",
                "state",
                "mode",
                "agent",
                "backend",
                "cwd",
                "conversation",
                "thread",
                "settings",
                "trust",
                "runs without asking",
                "mode lag",
                "limits",
                "resumable",
                "created",
                "updated",
                "queued",
                "background"
            ]
        );
        for claude_only in [
            "verdandi session",
            "claude session",
            "CLI",
            "Verdandi revision",
            "permission rules",
            "account",
            "model",
        ] {
            assert!(
                !labels.contains(&claude_only),
                "{claude_only} is a Claude row: {labels:?}"
            );
        }
        assert_eq!(value("backend"), "codex app-server");
        assert_eq!(value("agent"), "Codex", "no version until a session reports one");
        assert_eq!(value("trust"), "trusted since 2026-10-02");
        assert_eq!(
            value("settings"),
            "your Codex configuration (CODEX_HOME, else ~/.codex; always; skills off while this project is untrusted); \
             this project's: not loaded"
        );
        assert_eq!(
            value("runs without asking"),
            "Codex runs commands and edits inside /p/root without asking, reads anywhere, no network or sockets; \
             your exec-policy allow rules run unsandboxed; its reviewer decides anything else, which then runs with \
             your full rights; MCP tools follow their servers' approval modes"
        );
        assert!(value("mode lag").contains("next turn"), "{}", value("mode lag"));
        assert!(value("mode lag").contains("keeps the reviewer it started with"));
        assert_eq!(value("limits"), "—");
        assert_eq!(value("resumable"), "yes");
        assert_eq!(value("background"), "none");

        // Bypass keeps the sandbox and answers for the reviewer; the project's tier follows the answer.
        let rows = codex_rows(SessionModeChoice::Bypass, ProjectTrust::Trusted);
        let value = |label: &str| rows.iter().find(|r| r.label == label).unwrap().value.clone();
        assert!(value("runs without asking").contains("; Eitri allows anything else, which then runs"));
        assert!(!value("runs without asking").contains("reviewer"));
        assert!(value("settings").ends_with("this project's: loaded"));

        // Manual hands every review to the user: a card, never Codex's reviewer.
        let rows = codex_rows(SessionModeChoice::Manual, ProjectTrust::Trusted);
        let value = |label: &str| rows.iter().find(|r| r.label == label).unwrap().value.clone();
        assert_eq!(value("mode"), "manual");
        assert!(value("runs without asking").contains("; you decide anything else on a card, which then runs"));
        assert!(!value("runs without asking").contains("reviewer"));
        assert!(value("mode lag").contains("a turn started in manual or bypass"));
    }

    /// An ACP tab says what Eitri knows of what the agent runs unasked (only the mode it registered, or
    /// nothing at all without one), that a mode change applies at once, and how full its context is.
    #[test]
    fn the_settings_row_names_the_directory_codex_reported_and_the_stamps_read_as_dates() {
        assert_eq!(
            codex_settings_row(Some("/home/user/.config/codex"), "loaded"),
            "your Codex configuration in /home/user/.config/codex (always; skills off while this project is \
             untrusted); this project's: loaded"
        );
        assert!(
            codex_settings_row(None, "not loaded").starts_with("your Codex configuration (CODEX_HOME, else ~/.codex;")
        );
        assert_eq!(record_stamp_row(None), "\u{2014}");
        let shown = record_stamp_row(Some("1791522750000"));
        assert!(shown.starts_with("2026-10-0") && shown.len() == 16, "{shown}");
        assert!(!shown.contains("1791522750000"));
    }

    #[test]
    fn detail_rows_for_an_acp_tab_follow_its_registration() {
        let registration = |auto_mode: Option<&str>| AgentRegistration {
            name: "gem".into(),
            command: vec!["gem-acp".into()],
            project_files: vec![".gem/".into()],
            auto_mode: auto_mode.map(str::to_string),
        };
        let rows_for = |registered: &[AgentRegistration]| {
            let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
            let tab = set.open(AgentId::Acp("gem".into()));
            detail_rows(
                set.get(tab).unwrap(),
                BackendKind::Sidecar,
                None,
                std::path::Path::new("/p/root"),
                None,
                "not checked yet",
                ProjectTrust::Untrusted,
                registered,
                Some("Space k"),
            )
        };
        let value = |rows: &[DetailRow], label: &str| rows.iter().find(|r| r.label == label).unwrap().value.clone();

        let rows = rows_for(&[registration(None)]);
        assert_eq!(rows.len(), 18);
        assert_eq!(value(&rows, "backend"), "acp");
        assert!(
            value(&rows, "runs without asking").starts_with("Eitri does not know what this agent runs without asking"),
            "{}",
            value(&rows, "runs without asking")
        );
        let lag = value(&rows, "mode lag");
        assert!(
            lag.starts_with("none: Eitri answers by the tab's mode at once"),
            "{lag}"
        );
        assert!(!lag.contains("next turn"), "{lag}");
        assert_eq!(value(&rows, "context"), "—", "nothing reported before a session");

        let rows = rows_for(&[registration(Some("auto_edit"))]);
        let runs = value(&rows, "runs without asking");
        assert!(runs.contains("auto_edit"), "{runs}");
        assert!(!runs.contains("does not know what this agent"), "{runs}");

        // An agent init.lua no longer registers is one Eitri knows nothing about.
        let rows = rows_for(&[]);
        assert!(value(&rows, "runs without asking").starts_with("Eitri does not know"));
    }

    /// The `background` row says how many commands a Codex tab's agent left running and the key that
    /// stops them, as the user presses it: the leader by its own name, a rebinding followed, a deleted
    /// binding said.
    #[test]
    fn the_background_row_counts_and_names_the_key_that_stops_them() {
        assert_eq!(background_row(0, Some("Space k")), "none");
        assert_eq!(
            background_row(1, Some("Space k")),
            "1 running (Space k stops it with the session)"
        );
        assert_eq!(
            background_row(2, Some(", k")),
            "2 running (, k stops them with the session)"
        );
        assert!(background_row(3, None).contains("no key is bound to tab.stop-background"));

        assert_eq!(stop_background_keys(None).as_deref(), Some("Space k"));
        let help = |leader: &str, bindings: serde_json::Value| {
            serde_json::json!({"kind": "keymap", "panel": {"leaderLabel": leader, "bindings": bindings}}).to_string()
        };
        let bound = help(
            ",",
            serde_json::json!([{"keys": ["<leader>", "m"], "action": "mode.cycle"},
                               {"keys": ["<leader>", "x", "k"], "action": "tab.stop-background"}]),
        );
        assert_eq!(stop_background_keys(Some(&bound)).as_deref(), Some(", x k"));
        let unbound = help(
            "Space",
            serde_json::json!([{"keys": ["<leader>", "m"], "action": "mode.cycle"}]),
        );
        assert_eq!(stop_background_keys(Some(&unbound)), None);
    }

    /// A Codex tab's `background` row reads the count its agent last reported.
    #[test]
    fn a_codex_tab_background_row_reads_its_count() {
        use eitri_core::test_providers::{empty_trust_plan, ExternalRecordingProvider};
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-background-row");
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = set.open(AgentId::Codex);
        let provider = std::sync::Arc::new(ExternalRecordingProvider::codex());
        set.get_mut(tab).unwrap().backend = TabBackend::Live(
            AgentBackend::start_external_with(
                AgentId::Codex,
                provider.clone(),
                &dir,
                empty_trust_plan(),
                SessionModeChoice::Auto,
                None,
            )
            .unwrap(),
        );
        provider
            .inner
            .queue(agent::AgentDomainEvent::BackgroundCalls { running: 2 });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while set.get(tab).unwrap().live().unwrap().projection().background_calls != 2 {
            assert!(std::time::Instant::now() < deadline, "the count never arrived");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let rows = detail_rows(
            set.get(tab).unwrap(),
            BackendKind::Sidecar,
            None,
            &dir,
            None,
            "not checked yet",
            ProjectTrust::Untrusted,
            &[],
            Some("Space k"),
        );
        let row = rows.iter().find(|r| r.label == "background").unwrap();
        assert_eq!(row.value, "2 running (Space k stops them with the session)");
    }

    #[test]
    fn the_context_row_is_a_gauge_in_words() {
        let gauge = |used, size| agent::external::ContextGauge { used, size };
        assert_eq!(context_row(gauge(50_000, 200_000)), "25% full (50000 of 200000)");
        assert_eq!(context_row(gauge(7, 0)), "7 used");
    }

    /// The limit an agent reported is in the `limits` row of its tab, in words.
    #[test]
    fn the_limits_row_names_the_window_and_the_reset() {
        let limits = agent::AccountLimitsRecord {
            used_percent: 42.5,
            window_minutes: Some(300),
            resets_at: Some(1_790_000_000),
            plan: Some("pro".into()),
        };
        assert_eq!(
            limits_row(&limits),
            "42% used of the 300-minute window · resets 2026-09-21 14:13 UTC · plan pro"
        );
        let bare = agent::AccountLimitsRecord {
            used_percent: 3.0,
            window_minutes: None,
            resets_at: None,
            plan: None,
        };
        assert_eq!(limits_row(&bare), "3% used");
    }

    /// A Claude tab's rows are the same twenty, in the same order, whatever else the window can run.
    #[test]
    fn claude_detail_rows_unchanged() {
        let set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let rows = detail_rows(
            set.active_tab(),
            BackendKind::Sidecar,
            Some("work"),
            std::path::Path::new("/p"),
            Some(std::path::Path::new("/s/rules.json")),
            "not checked yet",
            ProjectTrust::Untrusted,
            &[],
            Some("Space k"),
        );
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "name",
                "title",
                "state",
                "mode",
                "model",
                "backend",
                "account",
                "cwd",
                "conversation",
                "verdandi session",
                "claude session",
                "CLI",
                "settings",
                "trust",
                "Verdandi revision",
                "resumable",
                "created",
                "updated",
                "queued",
                "permission rules"
            ]
        );
        let value = |label: &str| rows.iter().find(|r| r.label == label).unwrap().value.clone();
        assert_eq!(value("backend"), "sidecar");
        assert_eq!(value("resumable"), "yes");
        assert_eq!(value("permission rules"), "/s/rules.json");
    }

    /// `:tabnew <agent>` opens a tab of the agent the word names and refuses any other word, saying
    /// which are accepted; the new-tab key's tab runs `agent.default`.
    #[test]
    fn tabnew_opens_the_named_agent_and_refuses_other_words() {
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        assert_eq!(open_tab_named(&state, ViewId::BASE, "codex"), Ok(()));
        let active = state.borrow().tabs.active();
        assert_eq!(state.borrow().tabs.get(active).unwrap().agent(), &AgentId::Codex);
        assert_eq!(open_tab_named(&state, ViewId::BASE, "claude"), Ok(()));
        let active = state.borrow().tabs.active();
        assert_eq!(state.borrow().tabs.get(active).unwrap().agent(), &AgentId::Claude);
        let before = state.borrow().tabs.tabs().len();
        let refused = open_tab_named(&state, ViewId::BASE, "gemini").unwrap_err();
        assert!(
            refused.contains("gemini") && refused.contains("claude, codex"),
            "{refused}"
        );
        assert_eq!(state.borrow().tabs.tabs().len(), before, "a refused word opens nothing");
    }

    /// `agent.default` reaches the tab the window already has (empty, unstarted) and every later one;
    /// a started tab keeps its agent.
    #[test]
    fn the_default_agent_applies_to_empty_tabs_only() {
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let first = set.active();
        let named = set.open(AgentId::Claude);
        set.get_mut(named).unwrap().name = Some("docs".into());
        set.apply_configured_agent(AgentId::Codex);
        assert_eq!(set.get(first).unwrap().agent(), &AgentId::Codex);
        assert_eq!(
            set.get(named).unwrap().agent(),
            &AgentId::Claude,
            "a tab the user named is theirs"
        );
    }

    /// The companion window builds its panel with `build_agent_panel`, which builds the state with
    /// `panel_state`, which puts in the gate over the window's own canonical root; and that gate is
    /// about the root it was given.
    #[test]
    fn companion_mode_builds_the_same_gate() {
        use eitri_core::source_scan as scan;
        const SCANNED: &[&str] = &["shell/src", "panel/src"];
        let companion = scan::code_only(&scan::read_required("shell/src/companion/window.rs").text);
        let flat: String = companion.split_whitespace().collect();
        assert!(flat.contains("crate::agent_panel::build_agent_panel("));
        let read = scan::rust_sources(SCANNED);
        let panel = scan::code_only(&scan::file_with(&read, "fn panel_state(").text);
        let builder = scan::code_only(&scan::file_with(&read, "fn build_agent_panel(").text);
        let build = scan::functions(&builder, "build_agent_panel");
        assert_eq!(build.len(), 1);
        // The builder goes through `AgentPanelHandle::new` (once for the page, once for none), and
        // that is the one place the state is built.
        let flat_build: String = build[0].split_whitespace().collect();
        assert_eq!(flat_build.matches("AgentPanelHandle::new(").count(), 2);
        assert_eq!(scan::calls(build[0], "panel_state"), 0);
        let handle_impls = impl_blocks(&panel, "implAgentPanelHandle");
        let new: Vec<&str> = handle_impls
            .iter()
            .flat_map(|block| scan::functions(block, "new"))
            .collect();
        assert_eq!(new.len(), 1);
        assert!(scan::calls(new[0], "panel_state") > 0);
        let state = scan::functions(&panel, "panel_state");
        assert_eq!(state.len(), 1);
        let flat: String = state[0].split_whitespace().collect();
        assert!(
            flat.contains("trust:trust") || flat.contains("trust,"),
            "the state holds the gate"
        );
        assert!(flat.contains("trust_gate_for(state_home.as_deref(),home.as_deref(),&canonical_root)"));

        let outer = agent::state_dirs::test_workspace_dir("trust-companion");
        let root = outer.join("home/p");
        std::fs::create_dir_all(&root).unwrap();
        let gate = trust_gate_for(
            Some(outer.join("state").as_os_str()),
            Some(outer.join("home").as_os_str()),
            &root,
        );
        assert_eq!(gate.root(), root.as_path());
        let _ = std::fs::remove_dir_all(&outer);
    }

    // ---- Chat views: one page per view, each told only of the tab it shows. ----

    const READY: &str = r#"{"type":"ready","request_id":"r-ready"}"#;

    fn view(n: u8) -> ViewId {
        ViewId::new(n).unwrap()
    }

    /// A recording page attached to `handle` as `view`'s, its document not loaded.
    fn attach_recording_page(handle: &AgentPanelHandle, view: ViewId) -> Rc<RecordingPage> {
        let page = Rc::new(RecordingPage::default());
        handle.attach_page(view, Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>)));
        page
    }

    /// [`handle_with_recording_page`] with a second recording page attached as view 2, both past `ready`.
    fn handle_with_two_pages() -> (AgentPanelHandle, Rc<RecordingPage>, Rc<RecordingPage>) {
        let (handle, base, _host) = handle_with_recording_page();
        let second = attach_recording_page(&handle, view(2));
        handle.page_message(ViewId::BASE, READY);
        handle.page_message(view(2), READY);
        (handle, base, second)
    }

    /// Every tab the envelopes `page` was sent name, for those that name one.
    fn tabs_named(page: &RecordingPage) -> Vec<u64> {
        page.sent
            .borrow()
            .iter()
            .filter_map(|e| json_of(e)["tab"].as_u64())
            .collect()
    }

    /// The ids and the active tab of the last `tabs` envelope `page` was sent.
    fn last_strip(page: &RecordingPage) -> (Vec<u64>, u64) {
        let sent = page.sent.borrow();
        let strip = sent
            .iter()
            .rev()
            .map(|e| json_of(e))
            .find(|v| v["kind"] == "tabs")
            .expect("a tabs envelope");
        let ids = strip["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_u64().unwrap())
            .collect();
        (ids, strip["active"].as_u64().unwrap())
    }

    /// Whether `page` was sent an envelope of `kind` about `tab`.
    fn sent_about(page: &RecordingPage, kind: &str, tab: TabId) -> bool {
        page.sent
            .borrow()
            .iter()
            .map(|e| json_of(e))
            .any(|v| v["kind"] == kind && v["tab"] == tab.0)
    }

    /// A new tab fed by a provider of its own, opened (so selected) in the base view.
    fn live_tab(handle: &AgentPanelHandle, dir: &Path) -> (TabId, std::sync::Arc<RecordingProvider>) {
        let (provider, backend) = live_backend(dir);
        let mut state = handle.state.borrow_mut();
        let tab = state.tabs.open(AgentId::Claude);
        state.tabs.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        (tab, provider)
    }

    /// Ticks until `done` says so, at most five seconds.
    fn tick_handle_until(handle: &AgentPanelHandle, what: &str, done: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
            handle.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn stream(provider: &RecordingProvider, turn: &str, text: &str) {
        provider.queue(agent::AgentDomainEvent::TurnStarted { turn_id: turn.into() });
        provider.queue(agent::AgentDomainEvent::ContentDelta {
            turn_id: turn.into(),
            kind: agent::ContentKind::Text,
            text: text.into(),
        });
    }

    /// A `Write` that needs a human, so the pump leaves it a card.
    fn write_card(permission_id: &str) -> agent::AgentDomainEvent {
        agent::AgentDomainEvent::PermissionRequested {
            permission_id: permission_id.into(),
            tool_use_id: None,
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }),
            provider_prompt: None,
        }
    }

    fn clear(pages: &[&Rc<RecordingPage>]) {
        for page in pages {
            page.sent.borrow_mut().clear();
        }
    }

    #[test]
    fn break_out_and_join_route_every_payload_to_the_right_page() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("views-break-join");
        let (handle, base, second) = handle_with_two_pages();
        let first = handle.state.borrow().tabs.active();
        let (moved, provider) = live_tab(&handle, &dir);
        assert_eq!(handle.break_out(view(2)), Ok(moved));
        assert_eq!(handle.state.borrow().tabs.active(), first, "the base view falls back");
        let (ids, active) = last_strip(&base);
        assert!(!ids.contains(&moved.0), "the base strip no longer lists it: {ids:?}");
        assert_eq!(active, first.0);
        assert_eq!(last_strip(&second), (vec![moved.0], moved.0));

        clear(&[&base, &second]);
        stream(&provider, "t1", "to the extra view");
        tick_handle_until(&handle, "the moved tab's events in page 2", || {
            sent_about(&second, "events", moved)
        });
        assert!(
            !tabs_named(&base).contains(&moved.0),
            "the base page is told nothing of it: {:?}",
            base.sent.borrow()
        );

        assert_eq!(handle.join_back(view(2)), Ok(moved));
        let (ids, active) = last_strip(&base);
        assert!(ids.contains(&moved.0), "{ids:?}");
        assert_eq!(active, moved.0, "it comes back as the base view's active tab");
        clear(&[&base, &second]);
        provider.queue(agent::AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: agent::ContentKind::Text,
            text: "back in the base view".into(),
        });
        tick_handle_until(&handle, "the joined tab's events in the base page", || {
            sent_about(&base, "events", moved)
        });
        assert!(
            !tabs_named(&second).contains(&moved.0),
            "page 2 shows it no longer: {:?}",
            second.sent.borrow()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn post_draft(handle: &AgentPanelHandle, from: ViewId, request: &str, tab: TabId, text: &str) {
        let message = serde_json::json!({ "type": "draft", "request_id": request, "tab": tab.0, "text": text });
        handle.page_message(from, &message.to_string());
    }

    #[test]
    fn a_late_draft_from_the_page_a_tab_just_left_reaches_it() {
        let (handle, base, second) = handle_with_two_pages();
        let moved = handle.state.borrow_mut().tabs.open(AgentId::Claude);
        assert_eq!(handle.break_out(view(2)), Ok(moved));
        post_draft(&handle, view(2), "d-1", moved, "fix the te");
        assert_eq!(handle.state.borrow().tabs.get(moved).unwrap().draft, "fix the te");

        assert_eq!(handle.join_back(view(2)), Ok(moved));
        clear(&[&base, &second]);
        post_draft(&handle, view(2), "d-2", moved, "fix the test");
        assert_eq!(handle.state.borrow().tabs.get(moved).unwrap().draft, "fix the test");
        assert_eq!(
            result_for(&second.sent.borrow(), "d-2"),
            None,
            "a mirror is answered with nothing"
        );
        let told: Vec<serde_json::Value> = base.sent.borrow().iter().map(|e| json_of(e)).collect();
        assert!(
            told.contains(
                &serde_json::json!({ "kind": "draft", "tab": moved.0, "text": "fix the test", "late": true })
            ),
            "the base page's box is offered it: {told:?}"
        );

        post_draft(&handle, view(2), "d-3", moved, "fix the test again");
        assert_eq!(
            result_for(&second.sent.borrow(), "d-3"),
            Some(Err(NO_TAB_HERE.to_string())),
            "taken once"
        );
        assert_eq!(handle.state.borrow().tabs.get(moved).unwrap().draft, "fix the test");

        let third = attach_recording_page(&handle, view(3));
        handle.page_message(view(3), READY);
        handle.open_in(view(3)).unwrap();
        post_draft(&handle, view(3), "d-4", moved, "not mine");
        let number = handle.state.borrow().tabs.get(moved).unwrap().number;
        assert_eq!(
            result_for(&third.sent.borrow(), "d-4"),
            Some(Err(format!("tab {number} is not shown in this page")))
        );
        assert_eq!(handle.state.borrow().tabs.get(moved).unwrap().draft, "fix the test");
    }

    #[test]
    fn a_late_draft_reaches_the_tab_after_the_emptied_view_s_page_went_idle() {
        let (handle, base, second) = handle_with_two_pages();
        let moved = handle.state.borrow_mut().tabs.open(AgentId::Claude);
        assert_eq!(handle.break_out(view(2)), Ok(moved));
        post_draft(&handle, view(2), "d-1", moved, "fix the te");
        let emptied = Rc::new(RefCell::new(Vec::<ViewId>::new()));
        handle.on_view_empty({
            let emptied = emptied.clone();
            move |view| emptied.borrow_mut().push(view)
        });
        assert_eq!(handle.join_back(view(2)), Ok(moved));
        assert_eq!(*emptied.borrow(), vec![view(2)]);
        // The window retires the emptied view on the loop's next turn, which can come before the
        // document delivers what its composer flushed as the keys went.
        handle.idle_page(view(2));
        assert!(handle.pages.live(view(2)).is_none(), "the emptied view's page is idle");
        clear(&[&base, &second]);

        post_draft(&handle, view(2), "d-2", moved, "fix the test");
        assert_eq!(handle.state.borrow().tabs.get(moved).unwrap().draft, "fix the test");
        let told: Vec<serde_json::Value> = base.sent.borrow().iter().map(|e| json_of(e)).collect();
        assert!(
            told.contains(
                &serde_json::json!({ "kind": "draft", "tab": moved.0, "text": "fix the test", "late": true })
            ),
            "the base page's box is offered it: {told:?}"
        );
        assert_eq!(
            result_for(&second.sent.borrow(), "d-2"),
            None,
            "an idle page is answered nothing"
        );

        // Taken once; nothing else an idle page posts is acted on.
        post_draft(&handle, view(2), "d-3", moved, "fix the test again");
        assert_eq!(handle.state.borrow().tabs.get(moved).unwrap().draft, "fix the test");
        handle.page_message(view(2), READY);
        assert!(
            second.sent.borrow().is_empty(),
            "an idle page is sent nothing: {:?}",
            second.sent.borrow()
        );
    }

    #[test]
    fn the_receiving_page_gets_the_tabs_snapshot_with_its_pending_card() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("views-break-card");
        let (handle, _base, second) = handle_with_two_pages();
        let (tab, provider) = live_tab(&handle, &dir);
        provider.queue(write_card("perm-1"));
        tick_handle_until(&handle, "the card", || {
            handle
                .state
                .borrow()
                .tabs
                .get(tab)
                .unwrap()
                .attention
                .attention()
                .pending
                == 1
        });
        clear(&[&second]);
        assert_eq!(handle.break_out(view(2)), Ok(tab));
        let sent = second.sent.borrow().clone();
        let strip = sent.iter().position(|e| kind_of(e) == "tabs").expect("its strip");
        let snapshot = sent
            .iter()
            .position(|e| kind_of(e) == "snapshot" && json_of(e)["tab"] == tab.0)
            .expect("its tab's snapshot");
        assert!(strip < snapshot, "the strip before the tab it shows");
        assert_eq!(last_strip(&second), (vec![tab.0], tab.0));
        assert!(
            sent[snapshot].contains("perm-1"),
            "with its pending card: {}",
            sent[snapshot]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_page_has_its_own_document_ready() {
        let (handle, _base, _host) = handle_with_recording_page();
        let second = attach_recording_page(&handle, view(2));
        handle.page_message(ViewId::BASE, READY);
        handle.state.borrow_mut().tabs.open(AgentId::Claude);
        let moved = handle.break_out(view(2)).unwrap();
        handle.tick();
        assert!(
            second.sent.borrow().is_empty(),
            "nothing goes to a page before its ready: {:?}",
            second.sent.borrow()
        );
        handle.page_message(ViewId::BASE, READY);
        handle.state.borrow_mut().tabs.rename(moved, "renamed");
        handle.tick();
        assert!(
            second.sent.borrow().is_empty(),
            "the base page's ready releases nothing of page 2's: {:?}",
            second.sent.borrow()
        );
        handle.page_message(view(2), READY);
        assert_eq!(last_strip(&second), (vec![moved.0], moved.0));
        assert!(second.sent.borrow().iter().any(|e| kind_of(e) == "hello"));
        clear(&[&second]);
        handle.state.borrow_mut().tabs.rename(moved, "again");
        handle.tick();
        assert!(
            second.sent.borrow().iter().any(|e| kind_of(e) == "tabs"),
            "from its own ready on, its strip follows"
        );
    }

    #[test]
    fn page_shown_from_an_extra_view_marks_its_own_tab_seen_not_active() {
        let (handle, base, second) = handle_with_two_pages();
        let active = handle.state.borrow().tabs.active();
        handle.state.borrow_mut().tabs.open(AgentId::Claude);
        let bound = handle.break_out(view(2)).unwrap();
        assert_eq!(handle.state.borrow().tabs.active(), active);
        base.visible.set(true);
        second.visible.set(true);
        for tab in [active, bound] {
            handle
                .state
                .borrow_mut()
                .tabs
                .get_mut(tab)
                .unwrap()
                .attention
                .session_ended(false);
        }
        let unread = |tab: TabId| {
            handle
                .state
                .borrow()
                .tabs
                .get(tab)
                .unwrap()
                .attention
                .attention()
                .unread
        };
        assert!(unread(active) && unread(bound));
        handle.page_shown(view(2));
        assert!(!unread(bound), "the page's own tab is seen");
        assert!(unread(active), "the base view's active tab is not");
    }

    #[test]
    fn prefix_x_on_the_base_view_leaves_an_extra_views_session_running_and_opens_a_fresh_base_tab() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("views-kill-base");
        let (handle, _base, _second) = handle_with_two_pages();
        let (bound, _bound_provider) = live_tab(&handle, &dir);
        assert_eq!(handle.break_out(view(2)), Ok(bound));
        let (closed, _closed_provider) = live_tab(&handle, &dir);
        let before = handle.state.borrow().tabs.base_tabs();
        assert_eq!(before.len(), 2);
        assert_eq!(handle.close_view_tabs(ViewId::BASE), Ok(2));
        let state = handle.state.borrow();
        assert!(state.tabs.get(closed).is_none());
        let now = state.tabs.base_tabs();
        assert_eq!(now.len(), 1, "the base view is left one tab");
        assert!(!before.contains(&now[0]), "a fresh one");
        assert_eq!(state.tabs.active(), now[0]);
        let kept = state.tabs.get(bound).expect("the extra view's tab is kept");
        assert!(matches!(kept.backend, TabBackend::Live(_)), "its session runs on");
        assert_eq!(state.tabs.view_of(bound), view(2));
        drop(state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_idle_page_is_told_nothing_and_holds_a_blank_document() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("views-idle");
        let (handle, _base, second) = handle_with_two_pages();
        let (_bound, provider) = live_tab(&handle, &dir);
        handle.break_out(view(2)).unwrap();
        provider.queue(write_card("perm-1"));
        tick_handle_until(&handle, "its card in page 2", || {
            second.sent.borrow().iter().any(|e| e.contains("perm-1"))
        });
        let documents = second.documents.borrow().len();
        handle.idle_page(view(2));
        assert_eq!(second.documents.borrow().len(), documents + 1, "a document is loaded");
        let panel = {
            let state = handle.state.borrow();
            state.document.themed(&state.theme.css_vars())
        };
        {
            let loaded = second.documents.borrow();
            let (html, base) = loaded.last().unwrap();
            assert_eq!(
                (html.as_str(), base.as_str()),
                (IDLE_DOCUMENT, PANEL_BASE_URI),
                "a blank document at the panel's secure base"
            );
            assert!(!html.contains("<script"), "nothing to run: {html}");
            assert_ne!(*html, panel, "not the panel");
        }
        assert!(handle.state.borrow().page(view(2)).is_none(), "its record is gone");
        clear(&[&second]);
        provider.queue(write_card("perm-2"));
        for _ in 0..20 {
            handle.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        handle.set_pane_focused(view(2), true);
        handle.arrive(view(2));
        handle.page_message(view(2), READY);
        assert!(
            second.sent.borrow().is_empty(),
            "an idle page is sent nothing, and its own message is dropped: {:?}",
            second.sent.borrow()
        );
        assert!(handle.state.borrow().page(view(2)).is_none());

        // Reused, as the window reuses a pooled page: attached again, then the panel is loaded.
        handle.attach_page(view(2), Rc::new(PanelPage::new(second.clone() as Rc<dyn PageSurface>)));
        handle.load_document(view(2));
        assert_eq!(
            second.documents.borrow().last().cloned(),
            Some((panel, PANEL_BASE_URI.to_string())),
            "the panel again once reused"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_restore_that_asks_keeps_an_extra_view_until_its_tab_arrives() {
        let (state, dir) = restore_panel("views-restore", &[("s-1", None, Bypass)], &["s-1"]);
        state.borrow_mut().restore_source.as_mut().unwrap().tabs[0].view = Some(2);
        let base = Rc::new(RecordingPage::default());
        let handle = AgentPanelHandle::over(
            state.clone(),
            Some(Rc::new(PanelPage::new(base.clone() as Rc<dyn PageSurface>))),
        );
        let _second = attach_recording_page(&handle, view(2));
        let _third = attach_recording_page(&handle, view(3));
        handle.set_layout_views(vec![view(2), view(3)]);
        let emptied = Rc::new(RefCell::new(Vec::<ViewId>::new()));
        handle.on_view_empty({
            let emptied = emptied.clone();
            move |view| emptied.borrow_mut().push(view)
        });
        handle.page_message(ViewId::BASE, READY);
        handle.tick();
        assert!(!handle.restore_settled(), "on offer");
        assert!(emptied.borrow().is_empty());

        // The saved bypass tab is asked about first.
        let from = state.borrow().tabs.active();
        let asked = ask_for_restore(&mut state.borrow_mut(), from, RestoreAsk::Dashboard);
        let RestoreResult::Question(prompt) = asked else {
            panic!("a saved bypass tab is asked about");
        };
        handle.tick();
        assert!(!handle.restore_settled(), "the restore's question is open");
        assert!(emptied.borrow().is_empty());

        // Answered, it waits for the trust check, then resumes into its view.
        let answered = answer_restore_question(&mut state.borrow_mut(), prompt.nonce, false);
        assert!(matches!(answered, RestoreResult::Deferred));
        assert!(!handle.restore_settled(), "waiting for the trust question");
        let mut resumes = FakeResumes::default();
        with_resumes(&state, &mut resumes, || {
            tick_handle_until(&handle, "the restore to start", || state.borrow().restore_run.is_some());
            assert!(emptied.borrow().is_empty());
        });
        assert_eq!(resumes.asked, vec!["s-1".to_string()]);
        let restored = state
            .borrow()
            .tabs
            .tab_in(view(2))
            .expect("the saved tab comes back into its view");
        handle.tick();
        assert!(!handle.restore_settled(), "its resume is still out");
        assert!(emptied.borrow().is_empty());

        let (_provider, backend) = live_backend(&dir);
        resumes.senders[0].send(Ok(backend)).unwrap();
        tick_handle_until(&handle, "the restore to settle", || handle.restore_settled());
        handle.tick();
        assert_eq!(state.borrow().tabs.tab_in(view(2)), Some(restored));
        assert_eq!(
            *emptied.borrow(),
            vec![view(3)],
            "only the view the restore left empty goes, once"
        );
        handle.tick();
        assert_eq!(emptied.borrow().len(), 1);
    }

    #[test]
    fn the_theme_reaches_every_page_idle_ones_included() {
        let (handle, base, second) = handle_with_two_pages();
        let third = attach_recording_page(&handle, view(3));
        handle.idle_page(view(3));
        for page in [&base, &second, &third] {
            page.sent.borrow_mut().clear();
            page.backgrounds.borrow_mut().clear();
        }
        let mut tokens = eitri_core::theme::ThemeTokens::fallback();
        tokens.font_size_px = 23.0;
        handle.set_theme(&tokens);
        handle.set_panel_font_size_px(25.0);
        for page in [&base, &second, &third] {
            assert_eq!(page.backgrounds.borrow().len(), 2);
            let themes = page.sent.borrow().iter().filter(|e| kind_of(e) == "theme").count();
            assert_eq!(themes, 2, "{:?}", page.sent.borrow());
        }
    }

    #[test]
    fn a_reload_of_an_extra_page_gets_its_own_tabs_snapshot_and_no_base_card() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("views-reload");
        let (handle, base, second) = handle_with_two_pages();
        let (bound, _bound_provider) = live_tab(&handle, &dir);
        handle.break_out(view(2)).unwrap();
        let (base_tab, base_provider) = live_tab(&handle, &dir);
        base_provider.queue(write_card("perm-base"));
        tick_handle_until(&handle, "the base tab's card", || {
            handle
                .state
                .borrow()
                .tabs
                .get(base_tab)
                .unwrap()
                .attention
                .attention()
                .pending
                == 1
        });
        clear(&[&base, &second]);
        handle.page_message(view(2), READY);
        let sent = second.sent.borrow().clone();
        assert_eq!(last_strip(&second), (vec![bound.0], bound.0));
        assert!(sent_about(&second, "snapshot", bound), "{sent:?}");
        let hello = sent
            .iter()
            .map(|e| json_of(e))
            .find(|v| v["kind"] == "hello")
            .expect("a hello");
        assert!(hello["restore"].is_null(), "no restore offer outside the base page");
        for envelope in &sent {
            assert!(!envelope.contains("perm-base"), "{envelope}");
            assert_ne!(json_of(envelope)["tab"].as_u64(), Some(base_tab.0), "{envelope}");
        }
        assert!(
            base.sent.borrow().is_empty(),
            "the base page is told nothing of it: {:?}",
            base.sent.borrow()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_shown_tabs_streaming_in_one_tick_reach_their_own_pages() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("views-two-streams");
        let (handle, base, second) = handle_with_two_pages();
        let (bound, bound_provider) = live_tab(&handle, &dir);
        handle.break_out(view(2)).unwrap();
        let (shown, shown_provider) = live_tab(&handle, &dir);
        clear(&[&base, &second]);
        stream(&bound_provider, "tb", "the bound tab's text");
        stream(&shown_provider, "ts", "the base tab's text");
        tick_handle_until(&handle, "both pages' events", || {
            sent_about(&second, "events", bound) && sent_about(&base, "events", shown)
        });
        assert!(
            tabs_named(&base).iter().all(|tab| *tab == shown.0),
            "{:?}",
            base.sent.borrow()
        );
        assert!(
            tabs_named(&second).iter().all(|tab| *tab == bound.0),
            "{:?}",
            second.sent.borrow()
        );
        assert!(base.sent.borrow().iter().all(|e| !e.contains("the bound tab's text")));
        assert!(second.sent.borrow().iter().all(|e| !e.contains("the base tab's text")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keys_and_overlays_in_the_base_page_leave_an_extra_views_prompts() {
        let (handle, _base, _second) = handle_with_two_pages();
        let state = handle.state.clone();
        state.borrow_mut().tabs.open(AgentId::Claude);
        let bound = handle.break_out(view(2)).unwrap();
        // The mode key in page 2 asks first (a fresh press there takes page 2's own questions off,
        // so its tab's trust question is asked after it).
        let default_prompt = || match cycle_default_mode(&state, view(2)) {
            eitri_core::tab_set::ModeCycle::Confirm(plan) => plan,
            other => panic!("auto -> bypass asks first: {other:?}"),
        };
        let plan = default_prompt();
        assert_eq!(plan.page, view(2));
        let nonce = {
            let mut s = state.borrow_mut();
            s.tabs.defer_start(bound, first_send("r1", "hello")).unwrap();
            s.tabs.ask_trust(bound).unwrap()
        };
        handle.arrive(ViewId::BASE);
        handle.open_keymap(ViewId::BASE, None, 0);
        handle.hint_collect(1);
        handle.set_pane_focused(ViewId::BASE, false);
        assert_eq!(state.borrow().tabs.check_trust_answer(bound, nonce, view(2)), Ok(()));
        assert!(matches!(
            state.borrow_mut().tabs.confirm_bypass(plan.scope, plan.nonce, view(2)),
            Ok(eitri_core::tab_set::ConfirmOutcome::Entered { .. })
        ));

        // Page 2 losing the keys drops both of its own.
        state
            .borrow_mut()
            .tabs
            .apply_configured_default(SessionModeChoice::Auto);
        let plan = default_prompt();
        let nonce = state.borrow_mut().tabs.ask_trust(bound).unwrap();
        assert_eq!(state.borrow().tabs.check_trust_answer(bound, nonce, view(2)), Ok(()));
        handle.set_pane_focused(view(2), false);
        assert!(state.borrow().tabs.check_trust_answer(bound, nonce, view(2)).is_err());
        assert!(state
            .borrow_mut()
            .tabs
            .confirm_bypass(plan.scope, plan.nonce, view(2))
            .is_err());
    }

    #[test]
    fn the_chooser_lists_every_tab_and_names_a_bound_tabs_view() {
        let (handle, base, second) = handle_with_two_pages();
        handle.state.borrow_mut().tabs.open(AgentId::Claude);
        let bound = handle.break_out(view(2)).unwrap();
        let other = handle.state.borrow_mut().tabs.open(AgentId::Claude);
        let every: Vec<u64> = handle.state.borrow().tabs.tabs().iter().map(|t| t.id.0).collect();
        for (shown_in, page) in [(ViewId::BASE, &base), (view(2), &second)] {
            clear(&[page]);
            handle.open_chooser(shown_in);
            let sent = page.sent.borrow();
            let chooser = sent
                .iter()
                .map(|e| json_of(e))
                .find(|v| v["kind"] == "chooser")
                .expect("the chooser, in the page it was opened in");
            let rows = chooser["open"].as_array().unwrap();
            let listed: Vec<u64> = rows.iter().map(|r| r["tab"].as_u64().unwrap()).collect();
            assert_eq!(listed, every, "every tab, from any page");
            for row in rows {
                let label = row["label"].as_str().unwrap();
                assert_eq!(label.ends_with(" · agent 2"), row["tab"] == bound.0, "{label}");
            }
        }
        assert!(handle.state.borrow().tabs.get(other).is_some());
    }

    #[test]
    fn a_chooser_pick_of_a_bound_tab_asks_the_window_to_show_its_view() {
        let (handle, base, _second) = handle_with_two_pages();
        let first = handle.state.borrow().tabs.active();
        handle.state.borrow_mut().tabs.open(AgentId::Claude);
        let bound = handle.break_out(view(2)).unwrap();
        let shown = Rc::new(RefCell::new(Vec::<ViewId>::new()));
        handle.on_show_view({
            let shown = shown.clone();
            move |view| shown.borrow_mut().push(view)
        });
        clear(&[&base]);
        handle.page_message(
            ViewId::BASE,
            &format!(r#"{{"type":"select_tab","request_id":"p1","tab":{}}}"#, bound.0),
        );
        assert_eq!(*shown.borrow(), vec![view(2)]);
        assert_eq!(
            handle.state.borrow().tabs.active(),
            first,
            "a bound tab is never selected"
        );
        assert_eq!(result_for(&base.sent.borrow(), "p1"), Some(Ok(())));
        let number = handle.state.borrow().tabs.get(bound).unwrap().number;
        assert_eq!(handle.select_number(number), Some(TabPlace::View(view(2), bound)));
        assert_eq!(*shown.borrow(), vec![view(2), view(2)]);
    }

    // ---- Answers per page: each from the page that shows its tab, through the window's gate. ----

    /// A gate that records the view of every answer it is asked about and refuses those of `refused`
    /// views while `covered` is set.
    fn recording_gate(
        handle: &AgentPanelHandle,
        refused: &'static [u8],
        covered: Rc<Cell<bool>>,
    ) -> Rc<RefCell<Vec<ViewId>>> {
        const COVERED: &str = "agent 2 is under terminal 2: C-b M-- from terminal 2 stows it";
        let asked = Rc::new(RefCell::new(Vec::new()));
        handle.set_answer_gate(Rc::new({
            let asked = asked.clone();
            move |view: ViewId| {
                asked.borrow_mut().push(view);
                if covered.get() && refused.contains(&view.number()) {
                    Err(COVERED.to_string())
                } else {
                    Ok(())
                }
            }
        }));
        asked
    }

    fn permission_response(handle: &AgentPanelHandle, from: ViewId, request: &str, tab: TabId, permission: &str) {
        handle.page_message(
            from,
            &format!(
                r#"{{"type":"permission_response","request_id":"{request}","tab":{},"permission_id":"{permission}","decision":"allow"}}"#,
                tab.0
            ),
        );
    }

    fn waiting(handle: &AgentPanelHandle, tab: TabId, permission: &str) -> bool {
        handle.state.borrow().tabs.get(tab).unwrap().card_is_waiting(permission)
    }

    fn pending_cards(handle: &AgentPanelHandle, tab: TabId) -> usize {
        handle
            .state
            .borrow()
            .tabs
            .get(tab)
            .unwrap()
            .attention
            .attention()
            .pending
    }

    fn shut_down_every_tab(handle: &AgentPanelHandle) {
        for mut t in handle.state.borrow_mut().tabs.take_all() {
            if let TabBackend::Live(backend) = &mut t.backend {
                backend.shutdown();
            }
        }
    }

    #[test]
    fn a_permission_answer_is_taken_only_from_the_page_that_shows_its_tab() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("views-answer-per-page");
        let (handle, base, second) = handle_with_two_pages();
        let (bound, bound_provider) = live_tab(&handle, &dir);
        assert_eq!(handle.break_out(view(2)), Ok(bound));
        // A base-view tab left in the background: still the base page's to answer.
        let (background, background_provider) = live_tab(&handle, &dir);
        handle.state.borrow_mut().tabs.open(AgentId::Claude);
        assert_ne!(handle.state.borrow().tabs.active(), background);
        bound_provider.queue(write_card("perm-2"));
        background_provider.queue(write_card("perm-b"));
        tick_handle_until(&handle, "both cards", || {
            pending_cards(&handle, bound) == 1 && pending_cards(&handle, background) == 1
        });
        let asked = recording_gate(&handle, &[], Rc::new(Cell::new(false)));
        let number = handle.state.borrow().tabs.get(bound).unwrap().number;

        permission_response(&handle, ViewId::BASE, "r1", bound, "perm-2");
        assert_eq!(
            result_for(&base.sent.borrow(), "r1"),
            Some(Err(format!("tab {number} is not shown in this page")))
        );
        assert!(waiting(&handle, bound, "perm-2"), "still waiting");
        assert!(bound_provider.resolutions().is_empty());
        assert!(asked.borrow().is_empty(), "refused before the window is asked");

        permission_response(&handle, view(2), "r2", bound, "perm-2");
        assert_eq!(result_for(&second.sent.borrow(), "r2"), Some(Ok(())));
        assert_eq!(bound_provider.resolutions(), vec![("perm-2".to_string(), true)]);

        // Page 2 cannot answer the base view's card, whichever tab the base view shows.
        permission_response(&handle, view(2), "r3", background, "perm-b");
        assert!(matches!(result_for(&second.sent.borrow(), "r3"), Some(Err(_))));
        assert!(waiting(&handle, background, "perm-b"));
        permission_response(&handle, ViewId::BASE, "r4", background, "perm-b");
        assert_eq!(result_for(&base.sent.borrow(), "r4"), Some(Ok(())));
        assert_eq!(background_provider.resolutions(), vec![("perm-b".to_string(), true)]);
        assert_eq!(
            *asked.borrow(),
            vec![view(2), ViewId::BASE],
            "asked with each answering page's view"
        );
        shut_down_every_tab(&handle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_permission_answer_from_the_page_a_tab_left_is_refused_and_the_new_page_answers_it() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("views-answer-left-page");
        let (handle, base, second) = handle_with_two_pages();
        let (tab, provider) = live_tab(&handle, &dir);
        provider.queue(write_card("perm-1"));
        tick_handle_until(&handle, "the card", || pending_cards(&handle, tab) == 1);
        // The base page drew the card; the tab moves to view 2 before its answer arrives.
        assert_eq!(handle.break_out(view(2)), Ok(tab));
        permission_response(&handle, ViewId::BASE, "r1", tab, "perm-1");
        assert!(
            matches!(result_for(&base.sent.borrow(), "r1"), Some(Err(why)) if why.contains("not shown in this page"))
        );
        assert!(
            waiting(&handle, tab, "perm-1"),
            "the card waits for the page that shows it"
        );
        assert!(provider.resolutions().is_empty());

        permission_response(&handle, view(2), "r2", tab, "perm-1");
        assert_eq!(result_for(&second.sent.borrow(), "r2"), Some(Ok(())));
        assert_eq!(provider.resolutions(), vec![("perm-1".to_string(), true)]);
        shut_down_every_tab(&handle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_card_answer_from_a_covered_view_is_refused_and_the_card_stays() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("views-answer-covered");
        let (handle, _base, second) = handle_with_two_pages();
        let (tab, provider) = live_tab(&handle, &dir);
        assert_eq!(handle.break_out(view(2)), Ok(tab));
        provider.queue(write_card("perm-1"));
        tick_handle_until(&handle, "the card", || pending_cards(&handle, tab) == 1);
        let covered = Rc::new(Cell::new(true));
        let asked = recording_gate(&handle, &[2], covered.clone());

        permission_response(&handle, view(2), "r1", tab, "perm-1");
        assert!(matches!(result_for(&second.sent.borrow(), "r1"), Some(Err(why)) if why.contains("under terminal 2")));
        assert!(
            waiting(&handle, tab, "perm-1"),
            "a refused answer leaves the card waiting"
        );
        assert!(provider.resolutions().is_empty(), "nothing reached the agent");
        assert_eq!(*asked.borrow(), vec![view(2)]);

        covered.set(false);
        permission_response(&handle, view(2), "r2", tab, "perm-1");
        assert_eq!(result_for(&second.sent.borrow(), "r2"), Some(Ok(())));
        assert_eq!(provider.resolutions(), vec![("perm-1".to_string(), true)]);
        shut_down_every_tab(&handle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The last `confirm_bypass` prompt `page` was sent: its scope's tab and its nonce.
    fn last_bypass_prompt(page: &RecordingPage) -> Option<(Option<u64>, u64)> {
        page.sent
            .borrow()
            .iter()
            .rev()
            .map(|e| json_of(e))
            .find(|v| v["kind"] == "confirm_bypass")
            .map(|v| (v["tab"].as_u64(), v["nonce"].as_u64().unwrap()))
    }

    fn confirm_bypass_from(handle: &AgentPanelHandle, from: ViewId, request: &str, tab: TabId, nonce: u64) {
        handle.page_message(
            from,
            &format!(
                r#"{{"type":"confirm_bypass","request_id":"{request}","tab":{},"scope":"tab","nonce":{nonce}}}"#,
                tab.0
            ),
        );
    }

    fn cycle_mode_from(handle: &AgentPanelHandle, from: ViewId, request: &str, tab: TabId) {
        handle.page_message(
            from,
            &format!(r#"{{"type":"cycle_mode","request_id":"{request}","tab":{}}}"#, tab.0),
        );
    }

    /// What `prefix x` on an extra view names: the label of the one tab it shows, as its strip does, and
    /// nothing for a view with no tab or for the base view, whose kill closes every base-view tab.
    #[test]
    fn an_extra_views_kill_names_its_one_tab() {
        let (handle, _base, _second) = handle_with_two_pages();
        assert_eq!(handle.view_tab_label(view(2)), None, "no tab yet");
        assert_eq!(handle.view_running_queued(view(2)), (0, 0));
        let tab = handle.open_in(view(2)).unwrap();
        let number = handle.state.borrow().tabs.get(tab).unwrap().number;
        assert_eq!(handle.view_tab_label(view(2)), Some(format!("{number} new")));
        assert_eq!(handle.view_running_queued(view(2)), (0, 0), "an empty tab runs nothing");
        assert_eq!(handle.view_tab_label(ViewId::BASE), None);
        assert_eq!(handle.view_tab_label(view(3)), None);
    }

    fn mode_of(handle: &AgentPanelHandle, tab: TabId) -> SessionModeChoice {
        handle.state.borrow().tabs.get(tab).unwrap().mode()
    }

    #[test]
    fn cycle_mode_into_bypass_from_an_extra_view_shows_the_prompt_there_and_takes_its_confirmation() {
        let (handle, base, second) = handle_with_two_pages();
        let tab = handle.open_in(view(2)).unwrap();
        assert_eq!(mode_of(&handle, tab), SessionModeChoice::Auto);
        let asked = recording_gate(&handle, &[], Rc::new(Cell::new(false)));
        clear(&[&base, &second]);
        cycle_mode_from(&handle, view(2), "m1", tab);
        assert_eq!(result_for(&second.sent.borrow(), "m1"), Some(Ok(())));
        let (prompt_tab, nonce) = last_bypass_prompt(&second).expect("the prompt, in page 2");
        assert_eq!(prompt_tab, Some(tab.0));
        assert_eq!(last_bypass_prompt(&base), None, "the base page is shown nothing");

        // The base page's `y` with the same tab and nonce is not page 2's answer, and leaves its prompt.
        confirm_bypass_from(&handle, ViewId::BASE, "y1", tab, nonce);
        assert!(
            matches!(result_for(&base.sent.borrow(), "y1"), Some(Err(why)) if why.contains("not shown in this page"))
        );
        assert_eq!(mode_of(&handle, tab), SessionModeChoice::Auto);
        assert!(asked.borrow().is_empty(), "refused before the window is asked");

        confirm_bypass_from(&handle, view(2), "y2", tab, nonce);
        assert_eq!(result_for(&second.sent.borrow(), "y2"), Some(Ok(())));
        assert_eq!(mode_of(&handle, tab), SessionModeChoice::Bypass);
        assert_eq!(*asked.borrow(), vec![view(2)]);

        // A press in the base page about page 2's tab is refused, so no prompt is built there.
        clear(&[&base]);
        cycle_mode_from(&handle, ViewId::BASE, "m2", tab);
        assert!(matches!(result_for(&base.sent.borrow(), "m2"), Some(Err(_))));
        assert_eq!(last_bypass_prompt(&base), None);
    }

    #[test]
    fn a_bypass_prompt_is_dropped_when_its_page_loses_the_keys() {
        let (handle, base, second) = handle_with_two_pages();
        let first = handle.state.borrow().tabs.active();
        let tab = handle.open_in(view(2)).unwrap();
        cycle_mode_from(&handle, view(2), "m1", tab);
        let (_, nonce) = last_bypass_prompt(&second).expect("page 2's prompt");
        handle.set_pane_focused(view(2), false);
        confirm_bypass_from(&handle, view(2), "y1", tab, nonce);
        assert!(
            matches!(result_for(&second.sent.borrow(), "y1"), Some(Err(_))),
            "dropped with the keys"
        );
        assert_eq!(mode_of(&handle, tab), SessionModeChoice::Auto);

        // A prompt built in the base page stays when page 2 loses the keys.
        cycle_mode_from(&handle, ViewId::BASE, "m2", first);
        let (prompt_tab, nonce) = last_bypass_prompt(&base).expect("the base page's prompt");
        assert_eq!(prompt_tab, Some(first.0));
        handle.set_pane_focused(view(2), false);
        confirm_bypass_from(&handle, ViewId::BASE, "y2", first, nonce);
        assert_eq!(result_for(&base.sent.borrow(), "y2"), Some(Ok(())));
        assert_eq!(mode_of(&handle, first), SessionModeChoice::Bypass);
    }

    /// An untrusted project ([`trust_panel`] with a project hook) behind a handle with two recording
    /// pages past `ready` and a new empty tab bound to view 2. The gate's worker waits on `hold`
    /// before each job; it starts open.
    struct TrustViews {
        handle: AgentPanelHandle,
        base: Rc<RecordingPage>,
        second: Rc<RecordingPage>,
        tab: TabId,
        fx: TrustFx,
        hold: crate::trust_gate::WorkerHold,
    }

    fn trust_views(label: &str) -> TrustViews {
        let (state, fx) = trust_panel(label);
        fx.write(PROJECT_SETTINGS, &session_start_hook("touch /tmp/never"));
        let hold = crate::trust_gate::WorkerHold::default();
        {
            let mut s = state.borrow_mut();
            let home = fx.root.parent().unwrap().to_path_buf();
            let store = eitri_core::project_trust::TrustStore::new(Some(fx.state.as_os_str()), Some(home.as_os_str()));
            s.trust = TrustGate::new_held(store, fx.root.clone(), Some(home), hold.clone());
        }
        let base = Rc::new(RecordingPage::default());
        let handle = AgentPanelHandle::over(
            state,
            Some(Rc::new(PanelPage::new(base.clone() as Rc<dyn PageSurface>))),
        );
        let second = attach_recording_page(&handle, view(2));
        handle.page_message(ViewId::BASE, READY);
        handle.page_message(view(2), READY);
        let tab = handle.open_in(view(2)).unwrap();
        TrustViews {
            handle,
            base,
            second,
            tab,
            fx,
            hold,
        }
    }

    /// The gate's tick, delivering to the pages, until `done` says so (at most five seconds).
    fn poll_trust_until(handle: &AgentPanelHandle, what: &str, done: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            poll_trust(&handle.state, &handle.pages);
            if done() {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// The gate's tick until it owes nothing.
    fn poll_trust_until_idle(handle: &AgentPanelHandle) {
        poll_trust_until(handle, "the gate to go idle", || !handle.state.borrow().trust.is_busy());
    }

    fn trust_answer_from(handle: &AgentPanelHandle, from: ViewId, request: &str, p: &Prompt, trust: bool) {
        handle.page_message(
            from,
            &format!(
                r#"{{"type":"trust_answer","request_id":"{request}","tab":{},"nonce":{},"fingerprint":"{}","findings_digest":"{}","trust":{trust}}}"#,
                p.tab.0, p.nonce, p.fingerprint, p.digest
            ),
        );
    }

    fn awaiting_trust(handle: &AgentPanelHandle, tab: TabId) -> bool {
        matches!(
            handle.state.borrow().tabs.get(tab).unwrap().backend,
            TabBackend::AwaitingTrust(AwaitingTrust { recording: false, .. })
        )
    }

    #[test]
    fn an_extra_views_trust_question_survives_a_base_view_switch_and_its_y_starts_the_turn() {
        let TrustViews {
            handle,
            base,
            second,
            tab,
            fx,
            hold: _hold,
        } = trust_views("views-trust-switch");
        let first = handle.state.borrow().tabs.active();
        handle.state.borrow_mut().tabs.open(AgentId::Claude);
        let asked = recording_gate(&handle, &[], Rc::new(Cell::new(false)));
        clear(&[&base, &second]);
        handle.page_message(
            view(2),
            &format!(
                r#"{{"type":"send_message","request_id":"s1","tab":{},"text":"hello"}}"#,
                tab.0
            ),
        );
        poll_trust_until(&handle, "the question in page 2", || {
            !prompts(&second.sent.borrow()).is_empty()
        });
        let prompt = prompts(&second.sent.borrow()).pop().unwrap();
        assert_eq!(prompt.tab, tab);
        assert!(
            prompts(&base.sent.borrow()).is_empty(),
            "asked only in the page that shows it"
        );

        trust_answer_from(&handle, ViewId::BASE, "a1", &prompt, true);
        assert!(matches!(result_for(&base.sent.borrow(), "a1"), Some(Err(_))));
        assert!(awaiting_trust(&handle, tab));
        assert!(
            asked.borrow().is_empty(),
            "another page's answer never reaches the window"
        );

        // A switch in the base view drops the base view's questions only.
        handle.page_message(
            ViewId::BASE,
            &format!(r#"{{"type":"select_tab","request_id":"p1","tab":{}}}"#, first.0),
        );
        assert_eq!(handle.state.borrow().tabs.active(), first);
        assert!(handle
            .state
            .borrow()
            .tabs
            .check_trust_answer(tab, prompt.nonce, view(2))
            .is_ok());

        trust_answer_from(&handle, view(2), "a2", &prompt, true);
        assert_eq!(result_for(&second.sent.borrow(), "a2"), Some(Ok(())));
        assert_eq!(*asked.borrow(), vec![view(2)]);
        poll_trust_until(&handle, "the start", || !fx.spawned.borrow().is_empty());
        assert_eq!(*fx.spawned.borrow(), [(None, ProjectTrust::Trusted)]);
        let state = handle.state.borrow();
        match &state.tabs.get(tab).unwrap().backend {
            TabBackend::Starting(pending) => {
                assert_eq!(pending.request_id, "s1");
                assert_eq!(pending.first_turn.as_ref().unwrap().typed, "hello");
            }
            _ => panic!("page 2's tab connects once the record is written"),
        };
    }

    #[test]
    fn a_trust_answer_from_a_covered_page_is_refused_and_asked_again() {
        let TrustViews {
            handle,
            second,
            tab,
            fx,
            ..
        } = trust_views("views-trust-covered");
        let covered = Rc::new(Cell::new(true));
        let asked = recording_gate(&handle, &[2], covered.clone());
        handle.page_message(
            view(2),
            &format!(
                r#"{{"type":"send_message","request_id":"s1","tab":{},"text":"hello"}}"#,
                tab.0
            ),
        );
        poll_trust_until(&handle, "the question", || !prompts(&second.sent.borrow()).is_empty());
        let first = prompts(&second.sent.borrow()).pop().unwrap();
        let mut shown = first.clone();

        // Both a `y` and an `n` are refused from a covered page.
        for (request, trust) in [("a1", true), ("n1", false)] {
            clear(&[&second]);
            trust_answer_from(&handle, view(2), request, &shown, trust);
            let sent = second.sent.borrow().clone();
            assert!(
                matches!(result_for(&sent, request), Some(Err(why)) if why.contains("under terminal 2")),
                "{sent:?}"
            );
            assert!(
                notices(&sent).iter().any(|n| n.contains("under terminal 2")),
                "the page is told why"
            );
            poll_trust_until(&handle, "the question again", || {
                !prompts(&second.sent.borrow()).is_empty()
            });
            let again = prompts(&second.sent.borrow()).pop().unwrap();
            assert_eq!(again.tab, tab);
            assert_ne!(again.nonce, shown.nonce, "asked again under a fresh nonce");
            assert!(awaiting_trust(&handle, tab), "the tab still waits for its answer");
            assert!(fx.spawned.borrow().is_empty(), "nothing started");
            shown = again;
        }
        assert_eq!(*asked.borrow(), vec![view(2), view(2)]);

        covered.set(false);
        trust_answer_from(&handle, view(2), "a2", &first, true);
        assert!(
            matches!(result_for(&second.sent.borrow(), "a2"), Some(Err(_))),
            "the question first shown is no longer current"
        );
        trust_answer_from(&handle, view(2), "a3", &shown, true);
        assert_eq!(result_for(&second.sent.borrow(), "a3"), Some(Ok(())));
        poll_trust_until(&handle, "the start", || !fx.spawned.borrow().is_empty());
        assert!(fx.record_file().is_file());
    }

    #[test]
    fn a_trust_command_answer_from_a_covered_page_ends_the_command_unrecorded() {
        let TrustViews {
            handle,
            second,
            fx,
            hold,
            ..
        } = trust_views("views-trust-command-covered");
        let covered = Rc::new(Cell::new(true));
        recording_gate(&handle, &[2], covered.clone());
        let trust_command_from = |request: &str| {
            handle.page_message(
                view(2),
                &format!(r#"{{"type":"trust_command","request_id":"{request}","action":"trust","agent":"claude"}}"#),
            );
        };
        trust_command_from("c1");
        poll_trust_until(&handle, "the :trust prompt", || {
            !prompts(&second.sent.borrow()).is_empty()
        });
        let prompt = prompts(&second.sent.borrow()).pop().unwrap();
        poll_trust_until_idle(&handle);
        // From here a job the answer queued would stay queued, and the gate would say it is busy.
        hold.close();
        clear(&[&second]);
        trust_answer_from(&handle, view(2), "a1", &prompt, true);
        let sent = second.sent.borrow().clone();
        assert!(
            notices(&sent)
                .iter()
                .any(|n| n.contains("under terminal 2") && n.contains(":trust")),
            "{sent:?}"
        );
        assert!(matches!(result_for(&sent, "a1"), Some(Err(why)) if why.contains(":trust")));
        poll_trust_until(&handle, "the command's result", || {
            result_for(&second.sent.borrow(), "c1").is_some()
        });
        assert!(matches!(result_for(&second.sent.borrow(), "c1"), Some(Err(_))));
        assert!(
            !handle.state.borrow().trust.is_busy(),
            "no Record or Confirm job was queued"
        );
        assert!(!fx.record_file().exists(), "nothing recorded");
        hold.open();

        covered.set(false);
        clear(&[&second]);
        trust_command_from("c2");
        poll_trust_until(&handle, "the :trust prompt again", || {
            !prompts(&second.sent.borrow()).is_empty()
        });
        assert_ne!(prompts(&second.sent.borrow()).pop().unwrap().nonce, prompt.nonce);
    }

    fn trust_command_from(handle: &AgentPanelHandle, from: ViewId, request: &str, action: &str) {
        handle.page_message(
            from,
            &format!(r#"{{"type":"trust_command","request_id":"{request}","action":"{action}","agent":"claude"}}"#),
        );
    }

    /// A `:trust` typed in the base page whose tab breaks out ends: the base page no longer shows that
    /// tab and cannot answer its prompt. One typed in page 2 whose tab joins back ends the same way.
    #[test]
    fn a_trust_command_ends_when_its_tab_leaves_the_page_it_was_typed_in() {
        let TrustViews {
            handle,
            base,
            second,
            tab,
            fx: _fx,
            hold: _hold,
        } = trust_views("views-trust-command-moves");
        let first = handle.state.borrow().tabs.active();
        handle.state.borrow_mut().tabs.open(AgentId::Claude);
        handle.state.borrow_mut().tabs.select(first);
        let third = attach_recording_page(&handle, view(3));
        handle.page_message(view(3), READY);

        trust_command_from(&handle, ViewId::BASE, "c1", "trust");
        poll_trust_until(&handle, "the :trust prompt in the base page", || {
            !prompts(&base.sent.borrow()).is_empty()
        });
        assert_eq!(handle.break_out(view(3)), Ok(first));
        poll_trust_until(&handle, "the command's end", || {
            result_for(&base.sent.borrow(), "c1").is_some()
        });
        assert!(matches!(result_for(&base.sent.borrow(), "c1"), Some(Err(_))));
        assert_eq!(result_for(&third.sent.borrow(), "c1"), None);

        clear(&[&base, &second, &third]);
        trust_command_from(&handle, view(2), "c2", "trust");
        poll_trust_until(&handle, "the :trust prompt in page 2", || {
            !prompts(&second.sent.borrow()).is_empty()
        });
        assert_eq!(handle.join_back(view(2)), Ok(tab));
        poll_trust_until(&handle, "the second command's end", || {
            result_for(&second.sent.borrow(), "c2").is_some()
        });
        assert!(matches!(result_for(&second.sent.borrow(), "c2"), Some(Err(_))));
        assert_eq!(result_for(&base.sent.borrow(), "c2"), None);
    }

    /// A `:trust` whose tab breaks out of the base page while the gate is still checking the project
    /// is never shown in the page the tab moved to: that page never typed it. It ends, unrecorded, in
    /// the page it was typed in.
    #[test]
    fn a_trust_prompt_whose_tab_broke_out_before_the_check_returned_is_never_shown() {
        let TrustViews {
            handle,
            base,
            second: _second,
            tab: _tab,
            fx,
            hold,
        } = trust_views("views-trust-command-moves-early");
        let first = handle.state.borrow().tabs.active();
        handle.state.borrow_mut().tabs.open(AgentId::Claude);
        handle.state.borrow_mut().tabs.select(first);
        let third = attach_recording_page(&handle, view(3));
        handle.page_message(view(3), READY);

        // The worker cannot finish the command's check until the tab has moved.
        hold.close();
        trust_command_from(&handle, ViewId::BASE, "c1", "trust");
        assert_eq!(handle.break_out(view(3)), Ok(first));
        hold.open();
        poll_trust_until(&handle, "c1's end", || result_for(&base.sent.borrow(), "c1").is_some());

        assert!(matches!(result_for(&base.sent.borrow(), "c1"), Some(Err(_))));
        assert!(prompts(&third.sent.borrow()).is_empty(), "{:?}", third.sent.borrow());
        assert_eq!(result_for(&third.sent.borrow(), "c1"), None);
        assert!(!fx.record_file().exists(), "nothing recorded");
    }

    /// Two pages number their requests alike: a `:untrust` from page 2 and a `:trust` from the base
    /// page, both `c1` and both out at once, are each answered in the page that typed it.
    #[test]
    fn trust_commands_out_at_once_from_two_pages_are_each_answered_in_their_own_page() {
        let TrustViews {
            handle,
            base,
            second,
            fx: _fx,
            hold,
            ..
        } = trust_views("views-trust-two-commands");
        poll_trust_until_idle(&handle);
        hold.close();
        trust_command_from(&handle, view(2), "c1", "untrust");
        trust_command_from(&handle, ViewId::BASE, "c1", "trust");
        hold.open();
        poll_trust_until(&handle, "the :untrust's result and the :trust prompt", || {
            result_for(&second.sent.borrow(), "c1").is_some() && !prompts(&base.sent.borrow()).is_empty()
        });
        assert!(result_for(&second.sent.borrow(), "c1").is_some());
        assert_eq!(
            result_for(&base.sent.borrow(), "c1"),
            None,
            "the base page's :trust waits for its answer"
        );

        let prompt = prompts(&base.sent.borrow()).pop().unwrap();
        clear(&[&base, &second]);
        trust_answer_from(&handle, ViewId::BASE, "n1", &prompt, false);
        poll_trust_until(&handle, "the :trust's result", || {
            result_for(&base.sent.borrow(), "c1").is_some()
        });
        assert!(matches!(result_for(&base.sent.borrow(), "c1"), Some(Err(_))));
        assert_eq!(
            result_for(&second.sent.borrow(), "c1"),
            None,
            "page 2's c1 was answered once"
        );
        assert!(
            handle.state.borrow().trust_commands.is_empty(),
            "no record is left behind"
        );
    }

    /// A first message sent in the base page, its tab broken out to view 3 while it waits and then
    /// starting there: the start's failure is answered in the base page, which holds the request and
    /// the text, and page 3 (which shows the tab now) is told the tab's error.
    #[test]
    fn a_start_is_answered_in_the_page_it_was_asked_from_after_its_tab_moved() {
        let TrustViews {
            handle,
            base,
            fx: _fx,
            hold: _hold,
            ..
        } = trust_views("views-start-moves");
        let first = handle.state.borrow().tabs.active();
        handle.state.borrow_mut().tabs.open(AgentId::Claude);
        handle.state.borrow_mut().tabs.select(first);
        let third = attach_recording_page(&handle, view(3));
        handle.page_message(view(3), READY);
        handle.page_message(
            ViewId::BASE,
            &format!(
                r#"{{"type":"send_message","request_id":"s1","tab":{},"text":"hello"}}"#,
                first.0
            ),
        );
        poll_trust_until(&handle, "the question in the base page", || {
            !prompts(&base.sent.borrow()).is_empty()
        });
        assert_eq!(handle.break_out(view(3)), Ok(first));
        poll_trust_until(&handle, "the question in page 3", || {
            !prompts(&third.sent.borrow()).is_empty()
        });
        let prompt = prompts(&third.sent.borrow()).pop().unwrap();
        assert_eq!(prompt.tab, first);
        clear(&[&base, &third]);
        trust_answer_from(&handle, view(3), "a1", &prompt, true);
        assert_eq!(result_for(&third.sent.borrow(), "a1"), Some(Ok(())));
        // The fake connect's channel is closed: the start fails on the tick that collects it.
        tick_handle_until(&handle, "the start's answer", || {
            result_for(&base.sent.borrow(), "s1").is_some()
        });
        assert!(matches!(result_for(&base.sent.borrow(), "s1"), Some(Err(_))));
        assert_eq!(result_for(&third.sent.borrow(), "s1"), None);
        assert!(sent_about(&third, "error", first), "{:?}", third.sent.borrow());
        assert!(handle.state.borrow().start_pages.is_empty());
    }

    /// A first message sent in the base page, its tab broken out to view 3 while it waits for the trust
    /// answer, then closed there: the cancellation goes to the base page, which holds the request and
    /// reports it with the message's text, not to page 3, which never sent it.
    #[test]
    fn a_closed_tabs_start_is_refused_in_the_page_it_was_asked_from_after_its_tab_moved() {
        let TrustViews {
            handle,
            base,
            fx: _fx,
            hold: _hold,
            ..
        } = trust_views("views-start-closed");
        let first = handle.state.borrow().tabs.active();
        handle.state.borrow_mut().tabs.open(AgentId::Claude);
        handle.state.borrow_mut().tabs.select(first);
        let third = attach_recording_page(&handle, view(3));
        handle.page_message(view(3), READY);
        handle.page_message(
            ViewId::BASE,
            &format!(
                r#"{{"type":"send_message","request_id":"s1","tab":{},"text":"hello"}}"#,
                first.0
            ),
        );
        poll_trust_until(&handle, "the question in the base page", || {
            !prompts(&base.sent.borrow()).is_empty()
        });
        assert_eq!(handle.break_out(view(3)), Ok(first));
        clear(&[&base, &third]);
        assert_eq!(handle.close_view_tabs(view(3)), Ok(1));
        assert_eq!(
            result_for(&base.sent.borrow(), "s1"),
            Some(Err(TAB_CLOSED_MESSAGE.to_string()))
        );
        assert_eq!(result_for(&third.sent.borrow(), "s1"), None);
        assert!(handle.state.borrow().start_pages.is_empty());
    }

    /// A terminal handoff asked from the base page, its tab broken out to view 2 before the close
    /// finished: page 2, which shows the tab, is sent the card (or the failed close's error), and the
    /// base page, which holds the request and shows the tab as closing until it is answered, gets the
    /// answer.
    #[test]
    fn a_handoff_is_answered_in_the_page_it_was_asked_from_after_its_tab_moved() {
        agent::state_dirs::redirect_state_to_a_test_root();
        for failed in [false, true] {
            let (handle, base, second) = handle_with_two_pages();
            let moved = handle.state.borrow_mut().tabs.open(AgentId::Claude);
            let (closed_tx, closed_rx) = mpsc::channel();
            let (_dropped_tx, dropped_rx) = mpsc::channel();
            handle.state.borrow_mut().tabs.get_mut(moved).unwrap().pending_handoff = Some(PendingHandoff {
                request_id: "h1".into(),
                command: a_command(),
                closed_rx,
                dropped_rx,
                page: ViewId::BASE,
            });
            assert_eq!(handle.break_out(view(2)), Ok(moved));
            clear(&[&base, &second]);
            if failed {
                drop(closed_tx);
            } else {
                closed_tx.send(()).unwrap();
            }
            tick_handle_until(&handle, "the handoff's answer", || {
                result_for(&base.sent.borrow(), "h1").is_some()
            });
            let answer = result_for(&base.sent.borrow(), "h1").unwrap();
            assert_eq!(answer.is_ok(), !failed, "{answer:?}");
            assert_eq!(result_for(&second.sent.borrow(), "h1"), None);
            let card = if failed { "error" } else { "handoff" };
            assert!(sent_about(&second, card, moved), "{:?}", second.sent.borrow());
            assert!(!sent_about(&base, card, moved), "{:?}", base.sent.borrow());
            assert!(handle.state.borrow().tabs.get(moved).unwrap().pending_handoff.is_none());
        }
    }

    /// The review flow's answers go to the page whose request they answer, a tab's draft to the page
    /// that shows the tab, and the window's interrupted reverts to the base page; a request the flow
    /// no longer owes is forgotten.
    #[test]
    fn review_flow_answers_go_to_the_page_that_asked() {
        use eitri_core::turn_review::Out;
        let (handle, base, second) = handle_with_two_pages();
        let tab = handle.open_in(view(2)).unwrap();
        handle.state.borrow_mut().review_requests = vec![("req-1".into(), view(2)), ("req-2".into(), ViewId::BASE)];
        clear(&[&base, &second]);
        let answer = |id: &str| Out::Answer {
            request_id: id.to_string(),
            payload: serialize_command_result_for_js(id, Ok(())),
        };
        dispatch_review_outs(
            &handle.state,
            &handle.pages,
            vec![
                answer("req-1"),
                answer("req-2"),
                Out::Draft(tab, format!(r#"{{"kind":"review_draft","tab":{}}}"#, tab.0)),
                Out::Envelope(r#"{"kind":"review_recovery","entries":[]}"#.to_string()),
            ],
        );
        assert_eq!(result_for(&second.sent.borrow(), "req-1"), Some(Ok(())));
        assert_eq!(result_for(&second.sent.borrow(), "req-2"), None);
        assert_eq!(result_for(&base.sent.borrow(), "req-2"), Some(Ok(())));
        assert_eq!(result_for(&base.sent.borrow(), "req-1"), None);
        assert!(sent_about(&second, "review_draft", tab));
        assert!(!sent_about(&base, "review_draft", tab));
        let kinds = |page: &RecordingPage| -> Vec<String> {
            page.sent
                .borrow()
                .iter()
                .map(|e| json_of(e)["kind"].as_str().unwrap_or("").to_string())
                .collect()
        };
        assert!(kinds(&base).contains(&"review_recovery".to_string()));
        assert!(!kinds(&second).contains(&"review_recovery".to_string()));
        assert!(
            handle.state.borrow().review_requests.is_empty(),
            "the flow owes neither any more"
        );
    }

    /// A review request whose id another page's request still out has is refused, not mixed up with it.
    #[test]
    fn a_review_request_with_another_pages_id_out_is_refused() {
        let (handle, base, second) = handle_with_two_pages();
        let tab = handle.open_in(view(2)).unwrap();
        handle.state.borrow_mut().review_requests = vec![("req-7".into(), ViewId::BASE)];
        clear(&[&base, &second]);
        handle.page_message(
            view(2),
            &format!(r#"{{"type":"review_undo","request_id":"req-7","tab":{}}}"#, tab.0),
        );
        assert_eq!(
            result_for(&second.sent.borrow(), "req-7"),
            Some(Err(REVIEW_ID_IN_USE.to_string()))
        );
        assert_eq!(result_for(&base.sent.borrow(), "req-7"), None);
        assert_eq!(
            handle.state.borrow().review_requests,
            vec![("req-7".to_string(), ViewId::BASE)],
            "the base page's request keeps its record"
        );
    }

    /// A tab whose turn left a review hint in page 2 brings it to the base page when it joins back, and
    /// takes it to page 2 when it breaks out again: the page it arrives in was never sent it.
    #[test]
    fn a_moved_tab_brings_its_review_hint_to_its_new_page() {
        let (handle, base, second) = handle_with_two_pages();
        let tab = handle.open_in(view(2)).unwrap();
        let hint = eitri_core::turn_review::ReviewHint {
            tab: tab.0,
            turn: 1,
            files: 2,
        };
        handle.state.borrow_mut().shown_hints.push(hint.clone());
        clear(&[&base, &second]);
        assert_eq!(handle.join_back(view(2)), Ok(tab));
        assert!(sent_about(&base, "review_hint", tab), "{:?}", base.sent.borrow());
        clear(&[&base, &second]);
        assert_eq!(handle.break_out(view(2)), Ok(tab));
        assert!(sent_about(&second, "review_hint", tab), "{:?}", second.sent.borrow());
        assert!(!sent_about(&base, "review_hint", tab));
    }
}
