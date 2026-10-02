//! The real agent-ui panel: a `WebView` hosting the embedded `agent-ui/web` frontend, a
//! `UserContentManager` script-message bridge (JS -> Rust), and a fast poll of every tab's backend
//! pushed to the page via `evaluate_javascript` (Rust -> JS). See
//! docs/superpowers/specs/2026-09-07-agent-ui-design.md and, for the tabs,
//! docs/superpowers/specs/2026-09-25-keymap-tabs-panel-design.md §3.
//!
//! **One backend per tab** (session tabs spec §3.1). The tabs themselves live in
//! `eitri_core::tab_set::TabSet`; this module spawns the workers that set only holds receivers
//! for, drains every tab on one 33 ms tick, and routes each inbound command to the tab it names --
//! never to "the active one" (spec §3.8 point 2). Backend construction is lazy per tab: an empty
//! tab starts its backend on its first `send_message` (or a `resume`). No mode goes into it (R07,
//! 2026-09-27): every session is gated, and the tab's own mode is read where requests are answered,
//! never sent to the CLI. Which backend gets built is `eitri_core::agent_backend`'s decision.
//!
//! **State is server-originated.** For the sidecar backend this module folds nothing of its own:
//! `active_turn_id`, tool calls and permissions all arrive as real events through `pump()`. A
//! command that succeeds returns no events at all; the UI updates on the next poll. Nothing here
//! synthesizes an optimistic `TurnStarted` to make the UI feel faster -- doing so would put the
//! panel's idea of "a turn is running" ahead of the server's, which is the exact shadow state the
//! runtime design forbids.
//!
//! **The WebView draws only the active tab.** A background tab's events feed its attention and mark
//! it stale (`TabSet::pump`); a switch always sends the new active tab's full snapshot.

use eitri_core::agent_backend::{AgentBackend, BackendError, BackendGreeting, BackendKind};
use eitri_core::agent_bridge::{
    parse_inbound_message, serialize_command_result_for_js, serialize_confirm_restore_for_js, serialize_error_for_js,
    serialize_events_for_js, serialize_hello_with_restore_for_js, serialize_nav_key_for_js, ChooserRecord, ChooserTab,
    DetailRow, InboundMessage, NavKeyDirection, PaneNavDirection, PanelKeys, SessionModeChoice, TabVerbWire,
};
use eitri_core::keymap::TabAction;
use eitri_core::layout::Direction;
use eitri_core::saved_tabs::{SavedTabs, TabMemory};
use eitri_core::tab_restore::{RestoreOffer, RestorePolicy, RestoreRun};
use eitri_core::tab_set::{
    BypassPolicy, FirstTurn, PendingHandoff, PendingStart, RestoreGo, RestorePrompt, RestoreStep, ResumeRoute,
    StartCollected, Tab, TabBackend,
};
use eitri_core::tabs::TabId;
use gtk4::prelude::*;
use gtk4::Application;
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use webkit6::prelude::*;
use webkit6::{NetworkProxyMode, NetworkProxySettings, NetworkSession, UserContentManager, WebView};

/// The embedded, single-file `agent-ui/web` production build -- `shell/build.rs` (Task 5)
/// guarantees this file exists and is current by the time `shell` itself compiles.
const AGENT_UI_HTML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../agent-ui/web/dist/index.html"));

/// Poll interval for draining `AgentSession::pump()` -- fast enough that streaming text feels
/// live (a ~30-60fps cadence), not the coarse multi-second poll `poc/shell_composed` used for a
/// much less latency-sensitive readout. See the agent-ui spec's "Push mechanism, corrected" note
/// for why this is a poll at all (agent's own reader-thread channel is intentionally private).
const PUMP_POLL_INTERVAL_MS: u64 = 33;

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
/// this one span (`tear_down_all_holding_the_application`).
///
/// **Nothing waits on this. No thread blocks on it at all, and the GTK main loop least of all.**
/// It is a deadline observed by a `CLOSE_WATCH_POLL` tick on the main loop (`close_watch_step`),
/// which does a `try_recv` and returns. The window is already destroyed and the teardown is on a
/// worker; what keeps the process alive meanwhile is a `gio` application hold, not a wait. Read
/// `watch_off_the_main_loop` for the mechanism before changing this number.
///
/// **What is inside the span it bounds:** the whole of `AgentBackend::shutdown()` AND the drop of
/// the backend afterwards -- `tear_down_holding_the_application` drops it explicitly before the
/// worker reports. On the sidecar path those are two different waits and only the second one kills
/// anything: `shutdown()` issues `close_session`, a unary RPC bounded at `agent::UNARY_RPC_TIMEOUT`
/// (agent/src/providers/claude_sidecar/mod.rs), and it is `SpawnedSidecar::drop` (close stdin, poll
/// up to `agent::SIDECAR_EXIT_GRACE`, then SIGKILL) plus `RuntimeThread::drop` that end the child.
/// 18 seconds covers that ~16 s worst case with a little room; a round of this branch's review
/// found an earlier version deriving 15 from those same two numbers while reporting BEFORE the
/// drop, so the escalation it was sized for sat outside the window it bounded.
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
const CONNECT_CLOSE_BACKSTOP: std::time::Duration = std::time::Duration::from_secs(3);

/// How often the main loop looks at a close that is running on a worker. A `try_recv` and an
/// `Instant::elapsed`, so the interval only sets how long the process lingers past a teardown that
/// has already finished -- not how much work the main loop does.
const CLOSE_WATCH_POLL: std::time::Duration = std::time::Duration::from_millis(50);

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

/// Keeps the process alive until `rx` reports, WITHOUT anything waiting for it.
///
/// **The mechanism, because the previous two versions of this code got its premise wrong.** A
/// window close that has teardown left to do has two bad options if it insists on a wait: return
/// from `connect_close_request` immediately and let the process exit with `claude`, the sidecar and
/// `node` still running, or block the GTK main thread -- which keeps the window mapped and
/// unresponsive for as long as the wait lasts, because `window.connect_close_request` runs ON that
/// thread and nothing is destroyed until it returns. The first round of this branch moved the WORK
/// to a worker and kept a 3s wait on the main thread; the second raised that wait to 15s while
/// declaring the main loop no longer waited. It did. That is a 15-second frozen window, not a
/// bound.
///
/// There is a third option and it is the one GLib is built for: `g_application_hold`
/// (`gio::prelude::ApplicationExtManual::hold`) raises the application's use count, so
/// `Application::run` keeps iterating the main loop after the last window is gone instead of
/// returning. **This function's caller** returns at once, the main loop keeps running, and this
/// function's `CLOSE_WATCH_POLL` tick drops the guard once the worker reports or the backstop
/// expires. Releasing it is what lets `run` return and the process exit --
/// once nothing else holds the application, that is: the close path can install up to three of
/// these watches, and `run` returns when the last one releases. Nothing anywhere blocks.
///
/// Verified rather than declared, in two places, because this doc is the third attempt at it:
/// `a_hold_keeps_the_application_running_after_its_last_window` (this module, `#[ignore]`d --
/// `Application::run` wants the default main context and must not race the rest of the suite for
/// it) drives a real `gio::Application` with no window at all and shows `run` returning only after
/// the guard drops; `close_watch_step`'s own tests pin what each tick decides.
///
/// `what` names the branch in the log: the callers fail for different reasons and a debugger
/// reading one line needs to know which one it is looking at.
fn watch_off_the_main_loop<T: 'static>(
    app: &Application,
    what: &'static str,
    rx: mpsc::Receiver<T>,
    backstop: std::time::Duration,
    mut on_value: impl FnMut(T) + 'static,
) {
    // Taken BEFORE this function returns, i.e. before `connect_close_request` returns and GTK
    // destroys the last window -- which is the moment the application would otherwise release
    // itself and `run` would return.
    let mut hold = Some(app.hold());
    let started = std::time::Instant::now();
    gtk4::glib::timeout_add_local(CLOSE_WATCH_POLL, move || {
        let reported = rx.try_recv();
        match close_watch_step(&reported, started.elapsed() >= backstop) {
            CloseWatch::KeepHolding => return gtk4::glib::ControlFlow::Continue,
            // `on_value` runs BEFORE the release below, which matters for the one caller that has
            // one: the mid-connect branch starts a teardown here, and that teardown takes a hold of
            // its own. Taking it while this one is still held is what keeps the use count off zero
            // between the two.
            CloseWatch::Finished => {
                if let Ok(value) = reported {
                    on_value(value);
                }
            }
            CloseWatch::WorkerDied => {
                eprintln!("[agent_panel] {what}: its worker died without reporting")
            }
            CloseWatch::Expired => eprintln!(
                "[agent_panel] {what} had still not finished {}s after the window closed; the \
                 process is exiting anyway and whatever is left of it -- possibly a live `claude`, \
                 sidecar or `node` -- is being abandoned",
                backstop.as_secs()
            ),
        }
        // Dropping the guard IS the release, and it has to happen here rather than be left to the
        // closure's captures falling when GLib frees the removed source: the release is the thing
        // that lets `Application::run` return, so leaving it to the source's own teardown would
        // make process exit depend on GLib freeing a closure whose last act was to ask for exit.
        hold.take();
        gtk4::glib::ControlFlow::Break
    });
}

/// Tears a backend down on a worker thread while the application is held, and never waits for it.
///
/// **One mechanism for every backend this panel shuts down itself**, which is the point of it
/// being a function: `AgentPanelHandle::shutdown` has two such backends (the installed session,
/// and one that finished connecting while the window was closing) and a review found the second
/// still being torn down inline on the GTK main loop, 60 lines under a comment claiming that never
/// happens. A comment true of one branch and false of its sibling is worse than none.
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
fn tear_down_holding_the_application(app: &Application, what: &'static str, mut backend: AgentBackend) {
    let (closed_tx, closed_rx) = mpsc::channel();
    std::thread::spawn(move || {
        backend.shutdown();
        // Explicit, and BEFORE the report below rather than at the end of this closure: on the
        // sidecar path this line is where the child actually dies. See this function's doc.
        drop(backend);
        // The receiver is gone if the watch already released; that is the abandoned case, not an
        // error.
        let _ = closed_tx.send(());
    });
    watch_off_the_main_loop(app, what, closed_rx, SESSION_CLOSE_BACKSTOP, |()| {});
}

/// Every tab's backend at once, one worker each, under ONE backstop (spec §3.5 risk): serial
/// sidecar `CloseSession`s are bounded at 10 s each, so N tabs torn down in turn could outlive any
/// backstop. The workers report to a collector, which reports once to the watch.
///
/// Each worker drops its backend before it reports, for the reason
/// `tear_down_holding_the_application` gives: on the sidecar path the drop is where the child dies.
fn tear_down_all_holding_the_application(app: &Application, what: &'static str, backends: Vec<AgentBackend>) {
    if backends.is_empty() {
        return;
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
    watch_off_the_main_loop(app, what, all_rx, SESSION_CLOSE_BACKSTOP, move |finished| {
        if finished < expected {
            eprintln!(
                "[agent_panel] {what}: {} of {expected} workers died without reporting",
                expected - finished
            );
        }
    });
}

/// A connect still running when the window closed -- a tab's, or a closed tab's (`Retiring`). A
/// connect, not a teardown: see `CONNECT_CLOSE_BACKSTOP`. A backend that finishes after the process
/// is gone is exactly the orphan `AgentPanelHandle::shutdown` exists to prevent, so one that
/// finishes inside the backstop is torn down, held and bounded like the installed sessions: its
/// shutdown reaches the same ingestion-thread join.
fn watch_connect_holding_the_application(app: &Application, rx: mpsc::Receiver<Result<AgentBackend, BackendError>>) {
    let app_for_teardown = app.clone();
    watch_off_the_main_loop(
        app,
        "a connect that was still running",
        rx,
        CONNECT_CLOSE_BACKSTOP,
        move |result| match result {
            Ok(backend) => {
                eprintln!("[agent_panel] window closed mid-connect; shutting down the backend that finished anyway");
                tear_down_holding_the_application(&app_for_teardown, "a backend that finished mid-connect", backend);
            }
            Err(e) => eprintln!(
                "[agent_panel] window closed mid-connect; the backend had already failed: {}",
                e.message
            ),
        },
    );
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
/// **Held in panel state so the window-close backstop sees it** (the session-tabs whole-branch
/// review). These used to run on bare threads nobody recorded, so a window closed a few seconds
/// after `prefix &` y let `Application::run` return with the worker halfway through `shutdown()`:
/// `SpawnedSidecar::drop` never ran and the sidecar outlived the process, and a connect finishing
/// later was never shut down at all. `AgentPanelHandle::shutdown` now watches every entry here under
/// the same holds and backstops as the tabs it takes; the pump forgets an entry once it finishes.
#[derive(Default)]
struct Retiring {
    /// Connects of tabs that are gone. Whatever one produces is shut down (`poll`).
    connects: Vec<mpsc::Receiver<Result<AgentBackend, BackendError>>>,
    /// One per teardown worker, sent after the backend was shut down AND dropped -- the drop is
    /// where the sidecar child actually dies. A worker that panicked drops its sender instead.
    teardowns: Vec<mpsc::Receiver<()>>,
}

impl Retiring {
    /// One backend, shut down and dropped on a worker thread, never on the GTK main loop (the same
    /// reasons as `tear_down_holding_the_application`).
    fn backend(&mut self, mut backend: AgentBackend) {
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            backend.shutdown();
            drop(backend);
            let _ = done_tx.send(());
        });
        self.teardowns.push(done_rx);
    }

    /// A tab removed from the set: its backend is shut down, its connect is waited for, and a
    /// handoff's own worker (which already owns that backend) is waited for too. Returns the
    /// requests the tab still owed a `command_result`, for the caller to refuse -- a connect's
    /// `send_message`/`resume`, and a handoff's `handoff_to_terminal`. The record is never deleted
    /// (spec §3.5).
    fn tab(&mut self, tab: Tab) -> Vec<String> {
        let mut owed = Vec::new();
        if let Some(handoff) = tab.pending_handoff {
            owed.push(handoff.request_id);
            self.teardowns.push(handoff.dropped_rx);
        }
        match tab.backend {
            TabBackend::Live(backend) => self.backend(backend),
            TabBackend::Starting(pending) => {
                owed.push(pending.request_id);
                self.connects.push(pending.result_rx);
            }
            TabBackend::NotStarted | TabBackend::Failed { .. } => {}
        }
        owed
    }

    /// Each tick: a connect that finished has its backend shut down; a teardown that reported, or
    /// whose worker died, is forgotten. `try_recv` only -- this runs on the GTK main loop.
    fn poll(&mut self) {
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
            self.backend(backend);
        }
        self.teardowns
            .retain(|rx| matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.connects.is_empty() && self.teardowns.is_empty()
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
pub(crate) enum HintInbound {
    /// `f` in the panel's BROWSE: start a global HINT (or cancel one already active, same as the
    /// window-level toggle).
    Request,
    /// The panel's answer to `hint_collect`: how many visible targets it froze for `session_id`.
    Targets { session_id: u64, count: usize },
}

struct AgentPanelState {
    /// Every tab and which is active (session tabs spec §3.1). Rust owns the tabs; the WebView
    /// draws the active one.
    tabs: eitri_core::tab_set::TabSet,
    backend_kind: BackendKind,
    project_dir: PathBuf,
    /// `project_dir.canonicalize()`, read once: the `canonical_cwd` a session lease is keyed on
    /// (`agent::lease::SessionLease::is_held`), for resume routing and the chooser.
    canonical_project_dir: String,
    /// `$XDG_STATE_HOME/eitri/agent`, where the remembered mode is written (Task 3). `None`
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
    /// The panel's current theme. Starts on `ThemeTokens::fallback()` and is replaced by
    /// `AgentPanelHandle::set_theme`. Held as tokens rather than an envelope because it feeds three
    /// things: the `ready` batch, the `<style>` inlined into every `load_html`, and the WebView's
    /// own background colour.
    theme: eitri_core::theme::ThemeTokens,
    /// The `?` overlay's rows (`serialize_keymap_for_js`), sent with the theme on every `ready` so
    /// a reloaded document is told the keymap it needs, not the one baked into a hand list.
    keymap_help: Option<String>,
    /// Whether this panel's pane holds the window's keyboard focus, as `crate::pane_focus` last
    /// reported it. It is held here so that a freshly loaded document (first load or a
    /// `prefix r` reload) is told on `ready`. A document is otherwise told only when focus
    /// changes, and a reload does not change focus.
    pane_focused: bool,
    /// Where the panel's two HINT messages (`hint_request`, `hint_targets`) go. `None` until
    /// `shell::hint::HintCoordinator` installs one via `AgentPanelHandle::on_hint`. An `Rc<dyn Fn>`
    /// rather than a plain closure field because the hook must be cloned out of the borrow before
    /// being called -- see `handle_inbound_message`'s HintRequest/HintTargets arms for why.
    hint_hook: Option<Rc<dyn Fn(HintInbound)>>,
    /// Told whenever the window's attention (every tab's, summed: `TabSet::attention`) changes,
    /// with the value before and after: the tray's `agent ⚑N`, the toast, `on_permission`.
    attention_hook: Option<AttentionHook>,
    /// The last `tabs` envelope sent, so the tick sends one only when it changed.
    last_tabs_payload: Option<String>,
    /// The open provider sessions `hello` was last computed against (ruling 17).
    last_open_ids: Vec<String>,
    /// The per-window scratch directory (phase 3 ruling 18), `None` if it could not be made.
    scratch: Option<eitri_core::scratch::ScratchDir>,
    /// Edits out in nvim, polled by the tick until their marker appears.
    pending_edits: Vec<eitri_core::scratch::PendingEdit>,
    /// Reviews being worked out on worker threads, polled by the tick: git never runs on the GTK
    /// thread, so the answer comes back through a channel the tick drains.
    review_jobs: Vec<PendingReview>,
    /// Status-band hints not yet shown: the page was not ready (loading, or reloading) when they
    /// arrived. At most one per tab, the newest.
    review_hints: Vec<eitri_core::turn_review::ReviewHint>,
    /// shows and focuses the editor, then hands it these keys; `Err` says why it could not.
    editor_request_hook: Option<EditorRequestHook>,
    /// An edit came back (or was discarded): the keys return to the chat, in INPUT.
    editor_done_hook: Option<Rc<dyn Fn()>>,
    /// Teardowns and connects of tabs that left while the window stays open, so the window-close
    /// backstop covers them too. See [`Retiring`].
    retiring: Retiring,
    /// `$XDG_STATE_HOME/eitri/history` (C5) and `.../permissions` (D7); `None` with no state directory.
    history_dir: Option<PathBuf>,
    rules_dir: Option<PathBuf>,
    /// The project's prompts, oldest first, as last read or written (ruling 11).
    history: Vec<String>,
    /// Set by `ready`, cleared by `reload_document` (ruling 38).
    document_ready: bool,
    /// The last `editor_context` envelope sent (ruling 32).
    last_context_payload: Option<String>,
    /// The last `editor_link` envelope (companion mode), kept so a fresh document is told on
    /// `ready`. `None` in a window with no companion, which never sends one.
    editor_link: Option<String>,
    /// Where a `tab_verb` message's mapped `TabAction` goes (panel round 2 plan Task 6). `None`
    /// until `main.rs`'s `AgentPanelHandle::on_tab_verb` installs one.
    tab_verb_hook: Option<Rc<dyn Fn(TabAction)>>,
    /// C1's mirror (spec §3.5): the page's last-reported `panel_keys` mode. Starts, and is reset to,
    /// `Other` on every `ready` (`PanelKeys::Other`'s own doc says why, `reset_nav_mode_for_ready`
    /// is where the reset happens) -- see `AgentPanelHandle::panel_keys`.
    nav_mode: Cell<PanelKeys>,
    /// Where a `nav_fallthrough` message's direction goes (C1, spec §3.5), and a `pane_nav`'s (v1 picks,
    /// R11: `Ctrl+w h/j/k/l` -- the same move, from a key the page read itself): `None` until `main.rs`'s
    /// `AgentPanelHandle::on_nav_fallthrough` installs one, which is why `InboundMessage::NavFallthrough`
    /// and `InboundMessage::PaneNav` can be refused.
    nav_fallthrough_hook: Option<Rc<dyn Fn(Direction)>>,
    /// R1-6: how many times this WebView's own web process has crashed recently, and whether
    /// automatic recovery has given up on it -- see `AgentPanelHandle::on_web_process_terminated`.
    /// Re-armed by a manual recovery, never by the automatic path itself --
    /// `AgentPanelHandle::reload_document_by_hand`'s own doc says why.
    crash_guard: crate::webview_crash_guard::WebViewCrashGuard,
    /// Keeps `$XDG_STATE_HOME/eitri/tabs/<project>.json` in step with the tabs, once per tick.
    tab_memory: TabMemory,
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
    /// A restore whose resumes have not all returned yet.
    restore_run: Option<RestoreRun>,
    /// Whether the hello last sent could carry the restore offer, so the tick sends a new one only
    /// when that changes.
    last_restore_offered: bool,
    /// Where a restore's one-line result goes: `main.rs`'s window toast.
    toast_hook: Option<ToastHook>,
}

/// [`AgentPanelState::toast_hook`]: shows one line in the window's toast.
type ToastHook = Rc<dyn Fn(&str)>;

/// [`AgentPanelState::attention_hook`]: called with the attention before a change and after it.
type AttentionHook = Rc<dyn Fn(eitri_core::attention::Attention, eitri_core::attention::Attention)>;

/// `AgentPanelHandle::on_editor_request`: the request to hand nvim; `Err` says why it could not.
type EditorRequestHook = Rc<dyn Fn(&eitri_core::scratch::ScratchRequest) -> Result<(), String>>;

/// A handle back into this panel's session state, held by `main.rs` alongside the `gtk4::Widget`
/// so a real window close can shut down whatever `AgentSession` exists -- without this, the
/// `Rc<RefCell<AgentPanelState>>` that owns the session is only ever reachable from closures
/// internal to this module (the script-message handler and the pump timer), neither of which
/// ever runs `AgentSession::shutdown()` on its own, so a normal window close would otherwise
/// leak the child process, its hook socket, and any in-memory-only settings backup.
#[derive(Clone)]
pub(crate) struct AgentPanelHandle {
    state: Rc<RefCell<AgentPanelState>>,
    /// Held so the panel's document can be re-injected without going back through the widget tree.
    /// Cheap: a `WebView` is a GObject and cloning it is a refcount bump on the same object.
    ///
    /// `None` when WebKit's sandbox cannot start in this process (`webkit_sandbox::Decision::
    /// Unavailable`, v1-dist sub-plan 2026-09-28-v1-dist-ubuntu-userns): no `WebView` is built, the
    /// panel's place is a GTK notice, and nothing ever starts a session -- sessions start from the
    /// page's own messages, and there is no page. Every method below still runs against the inert
    /// state `main.rs`'s focus, HINT, prefix and tab-verb paths call into; what would reach a page
    /// does nothing, and the tab verbs refuse (`closing`), so `main.rs` flashes rather than opening
    /// tabs nobody can see.
    webview: Option<WebView>,
}

impl AgentPanelHandle {
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
    /// **Every live backend is torn down at once** (`tear_down_all_holding_the_application`): one
    /// worker per tab, one collector, one backstop. Serial sidecar closes are bounded at 10 s each,
    /// so N tabs torn down in turn could outlive any backstop (spec §3.5).
    ///
    /// **This function does not wait for anything, and that is its most important property.** It
    /// runs from `window.connect_close_request` on the GTK main thread, where a wait keeps the
    /// window mapped and frozen until it returns; two previous versions of this code waited there
    /// (3s, then 15s) while describing themselves as not doing so. Every branch below instead hands
    /// its receiver to `watch_off_the_main_loop`, which holds the application and polls.
    /// **This function** returns immediately and the process stays alive -- with no window on
    /// screen -- until the teardowns report or their backstops expire.
    ///
    /// **Scoped to the panel, deliberately, because the previous two versions of this doc were not
    /// and were false for it** (2026-09-21, third consecutive review finding on this path). The
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
    /// `app` is a parameter rather than a field for the same reason `install_reload_action` takes
    /// one: this handle is per-window, and the hold belongs to the application that owns the loop
    /// this panel's timers run on.
    pub(crate) fn shutdown(&self, app: &Application) {
        // Ruling 10: queued words outlive the window as history.
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
        let tabs = {
            let mut state = self.state.borrow_mut();
            // Read by the 33ms pump, by `poll_activate` and by every path into the tab set, all of
            // which keep running now that the loop outlives the window. See the field's own doc.
            state.shutting_down = true;
            state.tabs.take_all()
        };
        let mut backends = Vec::new();
        for tab in tabs {
            if let Some(handoff) = tab.pending_handoff {
                watch_off_the_main_loop(
                    app,
                    "a terminal handoff's close",
                    handoff.dropped_rx,
                    HANDOFF_CLOSE_BACKSTOP,
                    |()| {},
                );
            }
            match tab.backend {
                TabBackend::Live(backend) => backends.push(backend),
                TabBackend::Starting(pending) => watch_connect_holding_the_application(app, pending.result_rx),
                TabBackend::NotStarted | TabBackend::Failed { .. } => {}
            }
        }
        tear_down_all_holding_the_application(app, "every session tab's teardown", backends);
        // Tabs that left earlier, while the window stayed open: their teardowns and connects are
        // still running on workers nothing else would keep alive (the session-tabs whole-branch
        // review). Same holds and backstops as the tabs just taken.
        let retiring = std::mem::take(&mut self.state.borrow_mut().retiring);
        for rx in retiring.connects {
            watch_connect_holding_the_application(app, rx);
        }
        for rx in retiring.teardowns {
            watch_off_the_main_loop(app, "a closed tab's teardown", rx, SESSION_CLOSE_BACKSTOP, |()| {});
        }
    }

    /// Reloads the panel's frontend from scratch, deliberately WITHOUT touching the session.
    ///
    /// The frontend is a pure view layer over `AgentSessionProjection`, which lives here in Rust --
    /// so a WebView that has wedged (a frontend panic leaving a blank pane, a bridge that stopped
    /// dispatching) can be thrown away and rebuilt while the real conversation keeps running. The
    /// fresh document sends `ready` on mount, and `Ready`'s handler answers with `hello` plus a
    /// snapshot of the still-live session, which is the rehydration path already verified end to
    /// end in `shell/MANUAL_VERIFICATION.md`'s 2026-09-11 check 4.
    ///
    /// `load_html`, NOT `WebViewExt::reload()`. That same check found reload() reproducibly leaves
    /// this WebView permanently blank with zero further page activity, because the panel's document
    /// is a substitute-data load rather than a fetchable URI -- so the recovery action would itself
    /// be the wedge it exists to undo.
    ///
    /// **The invariant this rests on, stated because it is not obvious and a later change could
    /// break it silently:** `take_ui_delivery()` CONSUMES events. Every envelope the 33ms pump
    /// dispatches between this `load_html` and the new document's own `ready` is therefore gone --
    /// the old document is being torn down and the new one has no listener yet. Nothing is lost
    /// only because `Ready`'s handler answers from canonical state with a full snapshot rather than
    /// a replay, and `serialize_snapshot_for_js` already carries tool-call results. A pump that
    /// ever stopped being the sole consumer, or a snapshot that stopped being complete, would turn
    /// this into a quiet data-loss path with no test failing.
    pub(crate) fn reload_document(&self) {
        let Some(webview) = &self.webview else {
            eprintln!("[agent_panel] no panel document to reload: WebKit's sandbox cannot start here");
            return;
        };
        eprintln!("[agent_panel] reloading the panel document; the session is left untouched");
        // The page that said `ready` is going; the tick holds its envelopes until the new one does.
        self.state.borrow_mut().document_ready = false;
        // What the typing cadence still holds is in the projection, hence in the new page's snapshot.
        let discarded = crate::panel_pacer::pacer_for(webview).borrow_mut().discard_pending();
        trace_discarded_first_texts(&self.state, discarded);
        // And so is the mode it reported: until the new page's `ready`, nothing is BROWSE or INPUT.
        forget_the_pages_nav_mode(&self.state.borrow());
        let vars = self.state.borrow().theme.css_vars();
        webview.load_html(&themed_document(&vars), Some(PANEL_BASE_URI));
    }

    /// The user's OWN recovery action for a wedged or crashed panel -- `prefix r`
    /// (`Action::PanelReload`) or the top-bar `\u{21bb}` button (`install_reload_action`) -- as
    /// distinct from the AUTOMATIC reload `on_web_process_terminated` performs while still within
    /// `crash_guard`'s own budget.
    ///
    /// Re-arms `crash_guard` (`WebViewCrashGuard::reset`) before doing exactly what
    /// [`reload_document`](Self::reload_document) does. **Why a separate method rather than putting
    /// the reset inside `reload_document` itself (fix round 1, v1 hardening review, the
    /// guard-permanence finding on R1-6):** `on_web_process_terminated`'s own `Reload` branch calls
    /// `reload_document` directly, on every crash still within budget -- if that shared path reset
    /// the guard too, every automatic reload would erase its own crash history and `GiveUp` could
    /// never be reached, defeating the loop guard entirely. Only a genuinely manual recovery may
    /// re-arm it; the automatic path must keep counting toward the very loop it exists to detect.
    pub(crate) fn reload_document_by_hand(&self) {
        self.state.borrow_mut().crash_guard.reset();
        self.reload_document();
    }

    /// `web-process-terminated` (R1-6, v1 hardening review): the panel's WebView process died --
    /// a real WebKit crash, or the host genuinely out of memory. Until now nothing reconnected
    /// this signal, so the chat stayed blank until the user found `\u{21bb}`/`prefix r` by hand.
    /// Recovery is exactly [`reload_document`](Self::reload_document): the session lives in Rust,
    /// so a fresh document rehydrates from its own `ready`/`hello` round trip with nothing lost.
    ///
    /// Guarded by `crash_guard` (`webview_crash_guard::WebViewCrashGuard`) so a page that cannot
    /// even finish loading does not turn into a reload/crash loop; see that module's own doc for
    /// why `TerminatedByApi` is skipped before the guard is ever asked (nothing on this WebView
    /// calls `terminate_web_process()` today, but the check costs nothing and keeps this call site
    /// consistent with the Lua-panel one, which does call it).
    pub(crate) fn on_web_process_terminated(&self, reason: webkit6::WebProcessTerminationReason) {
        if reason == webkit6::WebProcessTerminationReason::TerminatedByApi {
            return;
        }
        // The page that reported BROWSE or INPUT is dead, and the give-up document never sends
        // `ready`: left as it was, `main.rs` would keep claiming `Ctrl+j`/`Ctrl+k` for a page that
        // cannot answer (the whole-branch review, R2-11's second half).
        forget_the_pages_nav_mode(&self.state.borrow());
        let response = self.state.borrow_mut().crash_guard.on_crash();
        match response {
            crate::webview_crash_guard::CrashResponse::Reload => {
                eprintln!("[agent_panel] the WebView's web process terminated ({reason:?}); reloading it");
                self.reload_document();
            }
            crate::webview_crash_guard::CrashResponse::GiveUp => {
                eprintln!("[agent_panel] the WebView's web process kept terminating; giving up on automatic reload");
                self.state.borrow_mut().document_ready = false;
                let html = crate::webview_crash_guard::crash_message_html(
                    "Reload it by hand with \u{21bb} in the top bar, or prefix r.",
                );
                if let Some(webview) = &self.webview {
                    let discarded = crate::panel_pacer::pacer_for(webview).borrow_mut().discard_pending();
                    trace_discarded_first_texts(&self.state, discarded);
                    webview.load_html(&html, Some(PANEL_BASE_URI));
                }
            }
            crate::webview_crash_guard::CrashResponse::AlreadyGivenUp => {}
        }
    }

    /// Non-blocking: returns `true` if `eitri-supervisor` asked this window to come to the
    /// front since the last call. `main.rs` polls this on its own timer and calls
    /// `window.present()` in response -- this handle has no `Window` reference of its own (see
    /// `AgentPanelHandle`'s own top-level doc: window lifecycle stays owned by `main.rs`).
    pub(crate) fn poll_activate(&self) -> bool {
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

    /// Records `tokens` as the panel's current theme, repaints the WebView's own background, and
    /// pushes the variables to the live document.
    ///
    /// Safe to call before the page has loaded: the dispatch is guarded, and the `ready` handshake
    /// sends the recorded envelope anyway, so neither ordering loses it.
    pub(crate) fn set_theme(&self, tokens: &eitri_core::theme::ThemeTokens) {
        let payload = eitri_core::agent_bridge::serialize_theme_for_js(tokens);
        self.state.borrow_mut().theme = tokens.clone();
        let Some(webview) = &self.webview else { return };
        paint_webview_background(webview, tokens);
        let script = format!(
            "window.__eitriDispatch && window.__eitriDispatch({});",
            serde_json::to_string(&payload).unwrap_or_default()
        );
        webview.evaluate_javascript(&script, None, None, None::<&gtk4::gio::Cancellable>, |result| {
            if let Err(e) = result {
                eprintln!("[agent_panel] theme dispatch failed: {e}");
            }
        });
    }

    /// Updates only the recorded theme's `font_size_px` and re-sends it -- the zoom-together
    /// design's path (`text_size::TextSizeController`) for pushing a new panel scale without
    /// re-deriving any other themed colour or token. Built on `set_theme`, so both a panel reload
    /// (`reload_document`, which reads straight from the recorded theme) and a fresh `ready`
    /// handshake (whose `hello` theme payload comes from the same recorded field) pick the new
    /// size up automatically -- neither has a separate path that could fall out of sync with it.
    pub(crate) fn set_panel_font_size_px(&self, font_size_px: f32) {
        let tokens = {
            let mut tokens = self.state.borrow().theme.clone();
            tokens.font_size_px = font_size_px;
            tokens
        };
        self.set_theme(&tokens);
    }

    /// The twin of [`set_panel_font_size_px`](Self::set_panel_font_size_px), for
    /// `--nv-editor-row` (wave 4, R5): updates only the recorded theme's `editor_row_px` and
    /// re-sends it, so both a panel reload and a fresh `ready` handshake pick the editor's cell
    /// height up the same way `font_size_px` already does, with no new envelope. `main.rs` calls
    /// this from `NeovideEditorPane::connect_cell_size_changed` and on a `gtk-xft-dpi` change, in
    /// the panel's CSS px -- already converted from the editor's GTK logical px by
    /// `webkit_zoom::EditorRow`.
    pub(crate) fn set_editor_row_px(&self, editor_row_px: f32) {
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
    /// mattered, and this call is simply a no-op). `what` is a short label for the error log only.
    fn dispatch(&self, payload: String, what: &'static str) {
        // No page at all (`webview`'s own doc): nothing is listening, as before a page loads.
        let Some(webview) = &self.webview else { return };
        // Behind whatever the typing cadence is holding, as `evaluate_js_dispatch` is.
        crate::panel_pacer::flush(webview);
        let script = format!(
            "window.__eitriDispatch && window.__eitriDispatch({});",
            serde_json::to_string(&payload).unwrap_or_default()
        );
        webview.evaluate_javascript(&script, None, None, None::<&gtk4::gio::Cancellable>, move |result| {
            if let Err(e) = result {
                eprintln!("[agent_panel] {what} dispatch failed: {e}");
            }
        });
    }

    /// The user pressed a key in the editor and it reached nvim (`NeovideEditorPane::
    /// connect_key_activity`). Starts, or extends, the typing window that paces the stream
    /// (`crate::panel_pacer`). O(1): it runs in the press's path.
    pub(crate) fn note_editor_key(&self) {
        if let Some(webview) = &self.webview {
            crate::panel_pacer::pacer_for(webview)
                .borrow_mut()
                .note_editor_key(std::time::Instant::now());
        }
    }

    /// Whether the editor holds the window's keys (`pane_focus`). Losing them ends the typing window
    /// at once: the next tick releases what is held.
    pub(crate) fn set_editor_has_keys(&self, has_keys: bool) {
        if let Some(webview) = &self.webview {
            crate::panel_pacer::pacer_for(webview)
                .borrow_mut()
                .set_editor_has_keys(has_keys);
        }
    }

    /// `agent.typing_cadence_hz` as `init.lua` left it (`None`: `"off"`), applied once after it ran.
    pub(crate) fn set_typing_cadence(&self, cadence_hz: Option<u32>) {
        if let Some(webview) = &self.webview {
            crate::panel_pacer::pacer_for(webview)
                .borrow_mut()
                .set_cadence(cadence_hz);
        }
    }

    /// `agent.restore` as `init.lua` left it, applied once after it ran. `Off` also stops the tabs
    /// being remembered at all and forgets what the last window left.
    pub(crate) fn set_restore_policy(&self, policy: RestorePolicy) {
        let mut state = self.state.borrow_mut();
        state.restore_policy = policy;
        if policy == RestorePolicy::Off {
            state.tab_memory.disable();
            state.restore_source = None;
        }
    }

    /// `agent.default_mode` as `init.lua` left it (`None`: unset), applied once after it ran. A mode
    /// Shift+Tab left for this project keeps winning; otherwise the empty tab the window already has,
    /// and every new one, start in it. Naming bypass here is the user's own answer to the question
    /// that is otherwise asked every time, so saved bypass tabs also come back in bypass unasked.
    pub(crate) fn set_default_mode(&self, mode: Option<SessionModeChoice>) {
        let mut state = self.state.borrow_mut();
        state.bypass_by_config = mode == Some(SessionModeChoice::Bypass);
        if let Some(mode) = mode {
            if !state.mode_remembered {
                state.tabs.apply_configured_default(mode);
            }
        }
    }

    /// Where a restore's one-line result goes. `main.rs` installs this once.
    pub(crate) fn on_toast(&self, hook: impl Fn(&str) + 'static) {
        self.state.borrow_mut().toast_hook = Some(Rc::new(hook));
    }

    /// Records whether this panel's pane has keyboard focus and tells the live document, which
    /// dims its mode block when it does not. Safe before the page has loaded for the same reason
    /// `set_theme` is: the dispatch is guarded, and `ready` re-sends the recorded value.
    pub(crate) fn set_pane_focused(&self, focused: bool) {
        self.state.borrow_mut().pane_focused = focused;
        if !focused {
            // Spec §3.3/§3.4: a pending bypass prompt is about the tab the user was just looking
            // at. Losing the keys entirely (focus moved to another module, e.g. the editor or the
            // terminal) must drop it, the same way switching tabs already does (`TabSet::select`),
            // so a `y` that slips through afterwards lands on nothing.
            self.state.borrow_mut().tabs.drop_bypass_prompt();
        }
        self.dispatch(
            eitri_core::agent_bridge::serialize_pane_focus_for_js(focused),
            "pane focus",
        );
    }

    /// Asks the panel to open its composer with the caret in it. Sent only from the new-tab path
    /// now (panel round 2 plan Task 6, spec §8); see `serialize_enter_input_for_js`. Safe before
    /// the page loads (the dispatch is guarded), in which case there is no composer yet and nothing
    /// happens.
    pub(crate) fn enter_input(&self) {
        self.drop_bypass_prompt();
        self.dispatch(eitri_core::agent_bridge::serialize_enter_input_for_js(), "enter-input");
    }

    /// Where every other keyboard arrival lands: BROWSE -- on the last row with following resumed if
    /// the reader was following it, else on the row and scroll they left (owner decision #22,
    /// 2026-09-29; the page decides, from what it parked when the keys left). Sent after `Ctrl+l`/a
    /// tray chip/`prefix a`'s miss have moved GTK focus into the panel (panel round 2 plan Task 6,
    /// decision 4: reverses 2026-09-19's "control l直接闪cursor"). Mirrors `enter_input` exactly
    /// except for the envelope it dispatches; safe before the page loads for the same reason.
    pub(crate) fn arrive(&self) {
        self.drop_bypass_prompt();
        self.dispatch(eitri_core::agent_bridge::serialize_arrive_for_js(), "arrive");
    }

    /// Spec §3.3/§3.4: every keyboard route GTK claims before the WebView sees a key is a route away
    /// from a bypass prompt. The panel cancels its own copy on the matching envelope; this drops
    /// Rust's, so a `y` that slipped through is refused by the nonce. v1-mode fix round 1 (the
    /// whole-branch review, blocking): HINT (`prefix f`) and `prefix ,` left the prompt open, and the
    /// next ordinary key -- the `y` that copies a code block HINT landed on, a name starting with
    /// `y` -- entered bypass and approved the waiting cards. The same now holds for `arrive`,
    /// `enter_input`, `nav_key`, `literal_key`, `prefix ?`, `prefix i`, `prefix &`, `<leader>bo` and
    /// a card landing, besides `pane_focus false` and the tab switches that already dropped it.
    fn drop_bypass_prompt(&self) {
        self.state.borrow_mut().tabs.drop_bypass_prompt();
    }

    /// C1's mirror (spec §3.5): the page's last-reported effective mode, for `main.rs`'s
    /// `Ctrl+j`/`Ctrl+k` intercept to decide against (`claims`). `Other` before the page has ever
    /// posted one, and again after every `ready` -- see `PanelKeys::Other`'s own doc.
    pub(crate) fn panel_keys(&self) -> PanelKeys {
        self.state.borrow().nav_mode.get()
    }

    /// C1's decision, dispatched to the page (spec §3.1, §3.5): `Ctrl+j` claimed in BROWSE, or
    /// `Ctrl+k` claimed in INPUT. Safe before the page loads for the same reason `arrive` is --
    /// there is no live mirror yet, so `main.rs`'s intercept never calls this before one exists.
    pub(crate) fn nav_key(&self, direction: NavKeyDirection) {
        self.drop_bypass_prompt();
        self.dispatch(serialize_nav_key_for_js(direction), "nav-key");
    }

    /// The `?` overlay's rows (`serialize_keymap_for_js`): recorded for every later `ready`, and
    /// sent now to a document that is already up.
    pub(crate) fn set_keymap_help(&self, payload: String) {
        self.state.borrow_mut().keymap_help = Some(payload.clone());
        self.dispatch(payload, "keymap");
    }

    /// `send-prefix`/`send-keys` with the panel holding the keys (keymap spec §2.6).
    pub(crate) fn literal_key(&self, key: &eitri_core::keymap::KeySpec) {
        self.drop_bypass_prompt();
        self.dispatch(
            eitri_core::agent_bridge::serialize_literal_key_for_js(&key.to_string()),
            "literal-key",
        );
    }

    /// `prefix ?`: open the `?` overlay.
    pub(crate) fn open_keymap(&self) {
        self.drop_bypass_prompt();
        self.dispatch(eitri_core::agent_bridge::serialize_open_keymap_for_js(), "open-keymap");
    }

    /// `prefix :` (owner decision #28, K16): open the panel's `:` command line, which runs nothing.
    pub(crate) fn open_command_line(&self) {
        self.drop_bypass_prompt();
        self.dispatch(
            eitri_core::agent_bridge::serialize_open_command_line_for_js(),
            "open-command-line",
        );
    }

    /// Global `f` HINT (spec: docs/superpowers/specs/2026-09-19-global-hint-design.md §3.3). Each
    /// is a one-line dispatch of the matching `serialize_hint_*_for_js` envelope; `shell::hint`
    /// drives the session, this handle only relays it to the WebView.
    pub(crate) fn hint_collect(&self, session_id: u64) {
        self.drop_bypass_prompt();
        self.dispatch(
            eitri_core::agent_bridge::serialize_hint_collect_for_js(session_id),
            "hint collect",
        );
    }
    pub(crate) fn hint_show(&self, session_id: u64, labels: &[String]) {
        self.dispatch(
            eitri_core::agent_bridge::serialize_hint_show_for_js(session_id, labels),
            "hint show",
        );
    }
    pub(crate) fn hint_prefix(&self, session_id: u64, typed: &str) {
        self.dispatch(
            eitri_core::agent_bridge::serialize_hint_prefix_for_js(session_id, typed),
            "hint prefix",
        );
    }
    pub(crate) fn hint_land(&self, session_id: u64, index: usize) {
        self.dispatch(
            eitri_core::agent_bridge::serialize_hint_land_for_js(session_id, index),
            "hint land",
        );
    }
    pub(crate) fn hint_end(&self, session_id: u64) {
        self.dispatch(
            eitri_core::agent_bridge::serialize_hint_end_for_js(session_id),
            "hint end",
        );
    }

    /// Where the panel's two HINT messages go. `shell::hint::HintCoordinator` installs this once.
    pub(crate) fn on_hint(&self, hook: impl Fn(HintInbound) + 'static) {
        self.state.borrow_mut().hint_hook = Some(Rc::new(hook));
    }

    /// Where the scratch round trips (`Ctrl+g`, `gf`) hand nvim their request. The hook shows and
    /// focuses the editor, then sends its keys; `Err` says why it could not. `main.rs` installs this once.
    pub(crate) fn on_editor_request(
        &self,
        hook: impl Fn(&eitri_core::scratch::ScratchRequest) -> Result<(), String> + 'static,
    ) {
        self.state.borrow_mut().editor_request_hook = Some(Rc::new(hook));
    }

    /// Where the panel stands with the editor beside it (companion mode), kept so a document that
    /// loads or reloads is told, and sent when it changes. Never called by the one-window mode.
    pub(crate) fn set_editor_link(&self, link: eitri_core::companion::attach::BandLink) {
        let payload = eitri_core::agent_bridge::serialize_editor_link_for_js(link.state, &link.text);
        {
            let mut state = self.state.borrow_mut();
            if state.editor_link.as_ref() == Some(&payload) {
                return;
            }
            state.editor_link = Some(payload.clone());
        }
        self.dispatch(payload, "editor link");
    }

    /// The editor went away or was swapped: every draft edit still out in it ends (a body saved in
    /// nvim becomes the draft). The active tab is told now, the others when they are switched to.
    /// Says so once if any edit was cut, and returns how many. It does not return the keys to the
    /// chat: the editor left, so nothing should take the focus.
    pub(crate) fn editor_detached(&self) -> usize {
        let (ended, active) = {
            let mut state = self.state.borrow_mut();
            let ended = end_pending_edits(&mut state);
            (ended, state.tabs.active())
        };
        for (tab, draft) in &ended {
            if *tab != active {
                continue;
            }
            if let Some(text) = draft {
                self.dispatch(eitri_core::agent_bridge::serialize_draft_for_js(*tab, text), "draft");
            }
            self.dispatch(
                eitri_core::agent_bridge::serialize_scratch_for_js(*tab, false),
                "scratch",
            );
        }
        if !ended.is_empty() {
            self.dispatch(
                eitri_core::agent_bridge::serialize_notice_for_js("draft editing stopped: the editor went away"),
                "notice",
            );
        }
        ended.len()
    }

    /// One line in the panel's band, in the notice's own style. Used by the companion window for what
    /// it cannot say any other way (a desktop it cannot move focus on).
    pub(crate) fn show_notice(&self, text: &str) {
        self.dispatch(eitri_core::agent_bridge::serialize_notice_for_js(text), "notice");
    }

    /// Called when a draft edited in nvim came back (or was discarded): the keys return to the
    /// chat. `main.rs` installs this once.
    pub(crate) fn on_editor_done(&self, hook: impl Fn() + 'static) {
        self.state.borrow_mut().editor_done_hook = Some(Rc::new(hook));
    }

    /// Called with the value before and after whenever [`AgentPanelHandle::attention`] changes --
    /// the one place that knows both, so `main.rs` keeps no copy of its own to drift (Task 10's
    /// review, minor 4). `main.rs` installs this once.
    pub(crate) fn on_attention(
        &self,
        hook: impl Fn(eitri_core::attention::Attention, eitri_core::attention::Attention) + 'static,
    ) {
        self.state.borrow_mut().attention_hook = Some(Rc::new(hook));
    }

    /// Where a `tab_verb` message's mapped `TabAction` goes: the same code the prefix's own
    /// `Action::Tab` arm runs (`main.rs`'s `run_tab_action`, panel round 2 plan Task 6). `None`
    /// until `main.rs` installs one, which is why `InboundMessage::TabVerb` can be refused.
    /// `main.rs` installs this once.
    pub(crate) fn on_tab_verb(&self, hook: impl Fn(TabAction) + 'static) {
        self.state.borrow_mut().tab_verb_hook = Some(Rc::new(hook));
    }

    /// Where a `nav_fallthrough` message's direction goes (C1, spec §3.5): `main.rs` wires this to
    /// `move_focus(&ModuleId::agent(), dir)`, i.e. exactly what the chord would have done had the
    /// panel never intercepted it. A `pane_nav` (v1 picks, R11: `Ctrl+w h/j/k/l`) goes through the
    /// same hook, for it is the same move. `main.rs` installs this once.
    pub(crate) fn on_nav_fallthrough(&self, hook: impl Fn(Direction) + 'static) {
        self.state.borrow_mut().nav_fallthrough_hook = Some(Rc::new(hook));
    }
}

/// The session-tab verbs `main.rs` binds (session tabs plan Tasks 7 and 8). Each borrows, changes
/// the set, drops the borrow, then dispatches. Every one of them does nothing once the window is
/// closing (`AgentPanelState::shutting_down`: the set is empty then).
// Tasks 7 and 8 wire these to the keymap and the window close; until then some have no caller.
#[allow(dead_code)]
impl AgentPanelHandle {
    /// The window is closing (`AgentPanelState::shutting_down`), or there is no page at all
    /// (`webview`'s own doc): every verb below then does nothing, and says so to its caller the way
    /// it does for a closing window.
    fn closing(&self) -> bool {
        self.live_page().is_none()
    }

    /// The page, while there is one and the window is not closing.
    fn live_page(&self) -> Option<&WebView> {
        if self.state.borrow().shutting_down {
            return None;
        }
        self.webview.as_ref()
    }

    /// `prefix c`: opens a tab, selects it, and sends `tabs` -- nothing else: an empty tab has no
    /// state yet.
    pub(crate) fn new_tab(&self) {
        let Some(webview) = self.live_page() else {
            return;
        };
        self.state.borrow_mut().tabs.open();
        send_tabs(&self.state, webview);
    }

    /// `prefix <digit>`. `false` when no tab has that number (the caller flashes, ruling 10).
    pub(crate) fn select_number(&self, n: u16) -> bool {
        self.switch_with(|tabs| tabs.select_number(n))
    }

    /// `prefix n` (+1) / `p` (-1), wrapping.
    pub(crate) fn step(&self, delta: i32) -> bool {
        self.switch_with(|tabs| tabs.step(delta))
    }

    /// `prefix l`: the previously active tab.
    pub(crate) fn select_last(&self) -> bool {
        self.switch_with(|tabs| tabs.select_last())
    }

    fn switch_with(&self, select: impl FnOnce(&mut eitri_core::tab_set::TabSet) -> Option<TabId>) -> bool {
        let Some(webview) = self.live_page() else {
            return false;
        };
        let selected = select(&mut self.state.borrow_mut().tabs);
        if selected.is_none() {
            return false;
        }
        send_switch(&self.state, webview);
        true
    }

    /// `prefix ,`: the inline rename field on the active tab's label.
    pub(crate) fn begin_rename(&self) {
        if self.closing() {
            return;
        }
        self.drop_bypass_prompt();
        let payload = {
            let state = self.state.borrow();
            let tab = state.tabs.active_tab();
            eitri_core::agent_bridge::serialize_begin_rename_for_js(tab.id, tab.name.as_deref())
        };
        self.dispatch(payload, "begin-rename");
    }

    /// `prefix &`: the footer's y/n prompt for the active tab.
    pub(crate) fn confirm_close(&self) {
        if self.closing() {
            return;
        }
        // The panel's close prompt replaces a bypass prompt on screen; Rust's copy goes with it.
        self.drop_bypass_prompt();
        let payload = {
            let state = self.state.borrow();
            let active = state.tabs.active();
            let facts = state.tabs.close_facts(active).expect("the active tab exists");
            eitri_core::agent_bridge::serialize_confirm_close_for_js(active, &eitri_core::tabs::close_prompt(&facts))
        };
        self.dispatch(payload, "confirm-close");
    }

    /// `<leader>bo` (Owner answers Q2): the footer's y/n prompt over every tab but the active one.
    /// Nothing to dispatch, and no flash, when the active tab is the only one open -- `<leader>bo`
    /// with one tab is simply nothing to do, the same as `prefix n`/`p` with one tab (ruling 10 is
    /// about a verb naming a tab that does not exist, not about this).
    pub(crate) fn confirm_close_others(&self) {
        if self.closing() {
            return;
        }
        self.drop_bypass_prompt();
        let Some((tabs, prompt)) = self.state.borrow().tabs.close_others_plan() else {
            return;
        };
        let payload = eitri_core::agent_bridge::serialize_confirm_close_others_for_js(&tabs, &[prompt]);
        self.dispatch(payload, "confirm-close-others");
    }

    /// `prefix w`.
    pub(crate) fn open_chooser(&self) {
        if self.closing() {
            return;
        }
        // Spec §3.4: opening the chooser is one of the routes that cancels an outstanding bypass
        // prompt -- it was about the tab the user is leaving to see this list. A fresh one raised
        // from inside the chooser (its own Shift+Tab) makes its own plan afterwards, unaffected.
        self.state.borrow_mut().tabs.drop_bypass_prompt();
        let payload = chooser_payload(&self.state.borrow());
        self.dispatch(payload, "chooser");
    }

    /// `prefix i`: the active tab's detail popover.
    pub(crate) fn open_detail(&self) {
        if self.closing() {
            return;
        }
        self.drop_bypass_prompt();
        let payload = {
            let state = self.state.borrow();
            detail_payload(&state, state.tabs.active())
        };
        self.dispatch(payload, "tab-detail");
    }

    /// The tray chip, `prefix a`: switches to the tab holding the oldest pending card and puts the
    /// panel's cursor on it. `false` when no tab holds a card.
    pub(crate) fn focus_oldest_card(&self) -> bool {
        let Some(webview) = self.live_page() else {
            return false;
        };
        let target = {
            let mut state = self.state.borrow_mut();
            let Some(target) = state.tabs.oldest_card_tab() else {
                return false;
            };
            state.tabs.select(target);
            // A landing on a card is a route away (spec §3.4) even when it names the tab already on
            // screen, which `select` alone does not treat as a move.
            state.tabs.drop_bypass_prompt();
            target
        };
        send_switch(&self.state, webview);
        self.dispatch(
            eitri_core::agent_bridge::serialize_focus_permission_for_js(target),
            "focus-permission",
        );
        true
    }

    /// What the chat owes the user now, summed over every tab (spec §3.7): its pending permission
    /// cards, and whether a turn finished while it was off screen.
    pub(crate) fn attention(&self) -> eitri_core::attention::Attention {
        let state = self.state.borrow();
        if state.shutting_down {
            return Default::default();
        }
        state.tabs.attention()
    }

    /// The tab holding the newest pending card, as `(number, label name)`: the toast names it.
    pub(crate) fn newest_card_label(&self) -> Option<(u16, String)> {
        let state = self.state.borrow();
        if state.shutting_down {
            return None;
        }
        let id = state.tabs.newest_card_tab()?;
        state.tabs.get(id).map(|t| (t.number, t.label_name()))
    }

    /// `prefix x` on the chat (2026-09-26): every session tab is closed exactly as `y` to `prefix &`
    /// closes one (`close_tab`: a running turn interrupted, queued words kept as history, the backend
    /// shut down and dropped on a worker the window-close backstop still covers, the record never
    /// deleted, so each session stays resumable). The set is left with one fresh empty tab, as closing
    /// its last tab always leaves it. How many tabs were closed.
    ///
    /// Refused, closing nothing, while a tab is being handed off to a terminal: `close_tab` refuses
    /// that tab, and a kill that closed the others and left it would not be a kill.
    pub(crate) fn close_every_tab(&self) -> Result<usize, &'static str> {
        let Some(webview) = self.live_page() else {
            return Ok(0);
        };
        let ids: Vec<TabId> = {
            let state = self.state.borrow();
            if state.tabs.tabs().iter().any(|t| t.pending_handoff.is_some()) {
                return Err("a tab is still being handed off to a terminal; kill the chat once that finishes");
            }
            state.tabs.tabs().iter().map(|t| t.id).collect()
        };
        for id in &ids {
            close_tab(&self.state, webview, *id)?;
        }
        eprintln!(
            "[agent_panel] killed: {} session tab(s) closed, records kept",
            ids.len()
        );
        Ok(ids.len())
    }

    /// Ruling 15: tabs whose turn is running or whose session is still connecting.
    pub(crate) fn running_count(&self) -> usize {
        let state = self.state.borrow();
        if state.shutting_down {
            return 0;
        }
        state.tabs.running_count()
    }

    /// Phase 3 ruling 10: every tab's queued messages, summed (the window-close prompt).
    pub(crate) fn queued_count(&self) -> usize {
        let state = self.state.borrow();
        if state.shutting_down {
            return 0;
        }
        state.tabs.queued_count()
    }
}

/// Tells the attention hook, if the value changed. The hook is cloned out of the borrow first: it
/// reaches the layout and the top bar, and must never find this panel's state borrowed.
fn report_attention(state: &Rc<RefCell<AgentPanelState>>, before: eitri_core::attention::Attention) {
    let (after, hook) = {
        let state = state.borrow();
        if state.shutting_down {
            return;
        }
        (state.tabs.attention(), state.attention_hook.clone())
    };
    if after != before {
        if let Some(hook) = hook {
            hook(before, after);
        }
    }
}

/// `backend_kind` is `main()`'s own choice (v1-dist plan Task 5, spec §10): `BackendKind::from_env`
/// is called there, before any window exists, so `--legacy`/`EITRI_AGENT_BACKEND=legacy` on a
/// release build can exit 1 naming the reason before touching GTK at all. This function no longer
/// makes that choice itself -- it only prints it.
///
/// `unavailable`: `Some(notice)` when WebKit's sandbox cannot start in this process
/// (`webkit_sandbox::Decision::Unavailable`). Then no `WebView` -- nor anything else of WebKit's --
/// is built: the panel's place is `webkit_sandbox::notice_widget(notice)`, and the handle carries the
/// same state with no page (`AgentPanelHandle::webview`'s own doc), no pump, no supervisor client.
pub(crate) fn build_agent_panel(
    project_dir: PathBuf,
    editor_context: eitri_core::editor_context::ContextSource,
    scratch: Option<eitri_core::scratch::ScratchDir>,
    backend_kind: BackendKind,
    unavailable: Option<&str>,
) -> (gtk4::Widget, AgentPanelHandle) {
    if let Some(notice) = unavailable {
        println!(
            "[agent_panel] backend: {} (no panel: WebKit's sandbox cannot start here)",
            backend_kind.as_str()
        );
        let state = Rc::new(RefCell::new(panel_state(
            project_dir,
            editor_context,
            scratch,
            backend_kind,
            None,
            None,
        )));
        let handle = AgentPanelHandle { state, webview: None };
        return (crate::webkit_sandbox::notice_widget(notice), handle);
    }
    let content_manager = UserContentManager::new();
    // Finding 3 (panel-content review): a CSP floor the page cannot loosen, plus a network backstop
    // behind it -- see `PANEL_CONTENT_SECURITY_POLICY` and `PANEL_NETWORK_PROXY_URI`'s doc comments.
    let network_session = NetworkSession::new_ephemeral();
    network_session.set_proxy_settings(
        NetworkProxyMode::Custom,
        Some(&NetworkProxySettings::new(Some(PANEL_NETWORK_PROXY_URI), &[])),
    );
    let webview = WebView::builder()
        .user_content_manager(&content_manager)
        .network_session(&network_session)
        .default_content_security_policy(PANEL_CONTENT_SECURITY_POLICY)
        .build();
    webview.set_hexpand(true);
    webview.set_vexpand(true);

    // Security: never let this WebView itself navigate away from its one embedded document.
    // Even sanitized markdown can legitimately contain an external link (a click), and a script
    // that got past sanitization anyway (defense-in-depth) might try to redirect the page --
    // either way, navigating this WebView to a remote origin would hand that origin the same
    // UserContentManager and therefore the same `eitriAgent` bridge this panel uses to relay
    // permission decisions. Hand link clicks to the system browser instead and keep this WebView
    // on its embedded document for the panel's whole lifetime.
    webview.connect_decide_policy(|_webview, decision, decision_type| {
        if decision_type == webkit6::PolicyDecisionType::NavigationAction {
            if let Some(nav_decision) = decision.downcast_ref::<webkit6::NavigationPolicyDecision>() {
                if let Some(action) = nav_decision.navigation_action() {
                    match action.navigation_type() {
                        webkit6::NavigationType::LinkClicked => {
                            // A markdown link the model rendered -- never let the embedded panel
                            // itself navigate there (that origin would inherit the same
                            // UserContentManager/eitriAgent bridge); hand it to the system
                            // browser instead.
                            if let Some(uri) = action.request().and_then(|r| r.uri()) {
                                gtk4::UriLauncher::new(&uri).launch(
                                    None::<&gtk4::Window>,
                                    None::<&gtk4::gio::Cancellable>,
                                    |_| {},
                                );
                            }
                            nav_decision.ignore();
                            return true;
                        }
                        webkit6::NavigationType::FormSubmitted | webkit6::NavigationType::FormResubmitted => {
                            // DOMPurify's default config deliberately keeps <form action="...">
                            // intact -- a sanitized-but-attacker-styled form submit button is a
                            // real, click-required (not zero-click) way to navigate this panel to
                            // a remote origin that would inherit the same bridge. Found by the
                            // final whole-branch review's own re-review of the LinkClicked-only
                            // guard above. There is no legitimate reason this panel's own
                            // embedded document would ever submit a form anywhere.
                            nav_decision.ignore();
                            return true;
                        }
                        _ => {}
                    }
                }
            }
        }
        false
    });

    let instance_id = uuid::Uuid::new_v4().to_string();
    let project_name = project_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| project_dir.to_string_lossy().to_string());
    let (supervisor, supervisor_pending) =
        match crate::supervisor_client::SupervisorClient::connect_or_spawn(instance_id, project_name, &project_dir) {
            crate::supervisor_client::PendingSupervisor::Ready(client) => (client, None),
            crate::supervisor_client::PendingSupervisor::Connecting(rx) => (None, Some(rx)),
        };
    println!("[agent_panel] backend: {}", backend_kind.as_str());
    let state = Rc::new(RefCell::new(panel_state(
        project_dir,
        editor_context,
        scratch,
        backend_kind,
        supervisor,
        supervisor_pending,
    )));

    content_manager.register_script_message_handler("eitriAgent", None);
    {
        let state = state.clone();
        let webview_for_handler = webview.clone();
        content_manager.connect_script_message_received(Some("eitriAgent"), move |_manager, js_value| {
            let raw = js_value.to_str();
            handle_inbound_message(&raw, &state, &webview_for_handler);
        });
    }

    {
        let tokens = state.borrow().theme.clone();
        paint_webview_background(&webview, &tokens);
        webview.load_html(&themed_document(&tokens.css_vars()), Some(PANEL_BASE_URI));
    }

    // Back on screen: whatever finished in the active tab while the chat was away has been seen
    // (the tray's `agent •`).
    {
        let state = state.clone();
        webview.connect_map(move |_| {
            if state.borrow().shutting_down {
                return;
            }
            let before = state.borrow().tabs.attention();
            state.borrow_mut().tabs.mark_seen();
            report_attention(&state, before);
        });
    }

    // Started once, at construction, rather than when a session starts: it is also what collects a
    // finished background connect. Previously it was started inside the handler that constructs the
    // backend, which meant a second start attempt after a failure installed a SECOND timer on the
    // same state -- every later event would then be dispatched twice.
    start_pump_timer(state.clone(), webview.clone());

    let handle = AgentPanelHandle {
        state: state.clone(),
        webview: Some(webview.clone()),
    };

    // R1-6: recover this WebView's own crash automatically (guarded) instead of leaving the chat
    // blank until the user finds `\u{21bb}`/`prefix r` by hand. See
    // `AgentPanelHandle::on_web_process_terminated`'s own doc.
    {
        let handle = handle.clone();
        webview.connect_web_process_terminated(move |_webview, reason| {
            handle.on_web_process_terminated(reason);
        });
    }

    (webview.upcast(), handle)
}

/// The panel's state, the same with a page or without one (`build_agent_panel`'s `unavailable`):
/// the tab set in the project's remembered mode, its prompt history and saved permission rules.
fn panel_state(
    project_dir: PathBuf,
    editor_context: eitri_core::editor_context::ContextSource,
    scratch: Option<eitri_core::scratch::ScratchDir>,
    backend_kind: BackendKind,
    supervisor: Option<crate::supervisor_client::SupervisorClient>,
    supervisor_pending: Option<std::sync::mpsc::Receiver<Option<crate::supervisor_client::SupervisorClient>>>,
) -> AgentPanelState {
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
    let mut tabs = eitri_core::tab_set::TabSet::new(backend_kind, mode);
    tabs.set_rules(rules);
    // The key a session lease is taken under (`AgentConversation` canonicalizes its cwd the same
    // way). `main.rs` already canonicalized the root; this only makes the string explicit.
    let canonical_root = project_dir.canonicalize().unwrap_or_else(|_| project_dir.clone());
    // Turn review follows every tab's turns from here on; the legacy backend has no turn
    // boundaries to follow and says so.
    if let Err(why) = tabs.install_turn_review(eitri_core::turn_review::TurnReview::new(
        state_home.as_deref(),
        home.as_deref(),
        &canonical_root,
    )) {
        eprintln!("[agent_panel] {why}");
    }
    let canonical_project_dir = canonical_root.to_string_lossy().into_owned();
    let tabs_dir = eitri_core::saved_tabs::state_dir(state_home.as_deref(), home.as_deref());
    let (restore_source, notes) = eitri_core::saved_tabs::startup(tabs_dir.as_deref(), &project_dir);
    for note in notes {
        eprintln!("{note}");
    }
    AgentPanelState {
        tabs,
        tab_memory: TabMemory::new(tabs_dir, project_dir.clone()),
        conversation_id: agent::conversation_id_for_cwd(&canonical_root),
        restore_source,
        restore_policy: RestorePolicy::Offer,
        bypass_by_config: false,
        mode_remembered,
        restore_run: None,
        last_restore_offered: false,
        toast_hook: None,
        editor_context,
        backend_kind,
        project_dir,
        canonical_project_dir,
        prefs_dir,
        supervisor,
        supervisor_pending,
        shutting_down: false,
        theme: eitri_core::theme::ThemeTokens::fallback(),
        keymap_help: None,
        pane_focused: false,
        hint_hook: None,
        attention_hook: None,
        last_tabs_payload: None,
        last_open_ids: Vec::new(),
        scratch,
        pending_edits: Vec::new(),
        review_jobs: Vec::new(),
        review_hints: Vec::new(),
        editor_request_hook: None,
        editor_done_hook: None,
        retiring: Retiring::default(),
        history_dir,
        rules_dir,
        history,
        document_ready: false,
        last_context_payload: None,
        editor_link: None,
        tab_verb_hook: None,
        nav_mode: Cell::new(PanelKeys::Other),
        nav_fallthrough_hook: None,
        crash_guard: crate::webview_crash_guard::WebViewCrashGuard::with_defaults(),
    }
}

/// The panel's single main-loop tick, over EVERY tab (spec §3.1). Three jobs, in order:
///
/// 1. collect every backend that finished constructing on a worker thread (and every handoff whose
///    close finished), and answer the command that has been waiting on it;
/// 2. drain every tab's backend (`TabSet::pump`): `take_ui_delivery` is where the permission policy
///    answers what needs no human, so a tab that was not pumped would stall. The active tab's batch
///    goes to the panel as one `events{tab,fromRevision,throughRevision,events[]}` envelope; a
///    background tab's feeds its attention and marks it stale;
/// 3. report the window's aggregated status to `eitri-supervisor` (ruling 19).
///
/// Then the `tabs` envelope and `hello` go out if what they describe changed.
fn start_pump_timer(state: Rc<RefCell<AgentPanelState>>, webview: WebView) {
    let pacer = crate::panel_pacer::pacer_for(&webview);
    gtk4::glib::timeout_add_local(std::time::Duration::from_millis(PUMP_POLL_INTERVAL_MS), move || {
        // The window has been closed and its tabs taken; everything below would be a no-op
        // against a destroyed window for as long as the close watch holds the application. Stop
        // instead of ticking 30 times a second at nothing. See `AgentPanelState::shutting_down`.
        if state.borrow().shutting_down {
            return gtk4::glib::ControlFlow::Break;
        }
        // The chat's own host: unmapped while it is hidden or zoomed away.
        let panel_mapped = webview.is_mapped();
        // Read before anything below changes it: a session installed this tick resets its count.
        let attention_before = state.borrow().tabs.attention();
        collect_pending_starts(&state, &webview);
        collect_pending_handoffs(&state, &webview);
        report_sessions_that_never_opened(&state, &webview);
        state.borrow_mut().retiring.poll();

        // The payload is built under the borrow and dispatched OUTSIDE it. Calling into WebKit while
        // holding a `RefMut` on the panel's own state is a latent re-entrancy hazard: the script
        // message handler takes the same `RefCell`, and anything that let it run during the
        // dispatch would panic on an already-borrowed cell rather than fail gracefully.
        let (payload, class, first_text, turn_ended, offers_changed, tripped, review_hints) = {
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
            // paths against it -- see `AgentBackend::answer_what_needs_no_human`.
            let project_dir = state_ref.project_dir.clone();
            let out = state_ref.tabs.pump(&project_dir, panel_mapped);
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
            // Whose first text the payload carries, kept with it: the cadence may release it after
            // the active tab, or that tab's turn, has changed (`panel_pacer::FirstText`).
            let first_text = if out.first_text {
                let active = state_ref.tabs.active();
                state_ref
                    .tabs
                    .active_tab()
                    .turn_trace
                    .as_ref()
                    .map(|trace| crate::panel_pacer::FirstText {
                        tab: active,
                        turn: trace.submitted_at(),
                    })
            } else {
                None
            };
            (
                out.active_payload,
                out.active_class,
                first_text,
                out.turn_ended,
                out.offers_changed,
                out.tripped,
                out.review_hints,
            )
        };
        // The CLI-mode tripwire failed these tabs (spec §2.3, D12): each session is shut down off
        // this thread, as a closed tab's is, and the shutdown denies what is still pending in it.
        // The active one also says why, as a session that never opened does.
        for tripped in tripped {
            let active = state.borrow().tabs.active() == tripped.tab;
            state.borrow_mut().retiring.backend(tripped.backend);
            if active {
                evaluate_js_dispatch(&webview, &serialize_error_for_js(tripped.tab, &tripped.reason));
            }
        }
        // The stream goes through the typing cadence (`crate::panel_pacer`): held for the next
        // slot while the user types in the editor, if it is plain streamed content; at once
        // otherwise, and always at once into a page that has not said `ready` (its snapshot
        // follows), as before. Whatever a slot or the end of the typing window releases goes out
        // in this same tick.
        let page_ready = state.borrow().document_ready;
        {
            let sink = crate::panel_pacer::WebViewSink(&webview);
            let mut pacer = pacer.borrow_mut();
            let now = std::time::Instant::now();
            if let Some(payload) = payload {
                let class = if page_ready {
                    class
                } else {
                    eitri_core::panel_cadence::EnvelopeClass::Immediate
                };
                pacer.send_stream(&sink, payload, first_text, class, now);
            }
            pacer.poll(&sink, now, page_ready);
        }
        let sent = pacer.borrow_mut().take_sent();
        // Stamped at the dispatch call that really sent it -- where the WebView's own clock starts,
        // which under the cadence may be a later tick or a flush between ticks -- for the tab and
        // turn it was pumped for.
        stamp_first_text_dispatches(&state, sent.first_text);
        if sent.any {
            let mut state_ref = state.borrow_mut();
            if let Some(trace) = state_ref.tabs.active_tab_mut().turn_trace.as_mut() {
                if trace.is_complete() {
                    trace.emit();
                }
            }
        }
        // Ruling 3a: a turn ended in these tabs; their queues go out now.
        for tab in turn_ended {
            let flush = state.borrow_mut().tabs.flush_queue(tab);
            if let Some(flush) = flush {
                apply_flush(&state, &webview, tab, flush);
            }
        }
        // A turn's end snapshot landed: the status band's hint. A page that has not said `ready` (still
        // loading, or reloading) has nothing to show it, so it waits here for the page; the review
        // itself is asked for by `c`, never by this.
        let hints = {
            let mut state_ref = state.borrow_mut();
            let state_ref = &mut *state_ref;
            keep_newest_hint_per_tab(&mut state_ref.review_hints, review_hints);
            if state_ref.document_ready {
                std::mem::take(&mut state_ref.review_hints)
            } else {
                Vec::new()
            }
        };
        for hint in &hints {
            evaluate_js_dispatch(&webview, &eitri_core::agent_bridge::serialize_review_hint_for_js(hint));
        }
        // The active tab's offers changed (a card arrived or went): the panel's third buttons follow.
        if offers_changed.contains(&state.borrow().tabs.active()) && state.borrow().document_ready {
            let payload = {
                let state_ref = state.borrow();
                let tab = state_ref.tabs.active_tab();
                eitri_core::agent_bridge::serialize_rule_offers_for_js(tab.id, &tab.rule_offers)
            };
            evaluate_js_dispatch(&webview, &payload);
        }
        send_context_if_changed(&state, &webview);
        poll_scratch_edits(&state, &webview);
        poll_review_jobs(&state, &webview);
        send_tabs_if_changed(&state, &webview);
        remember_tabs(&state);
        send_hello_if_open_sessions_changed(&state, &webview);
        // After the payload, not before it: a reaction that sends the panel an envelope -- one
        // that lands on a card, say -- must find the card it names already there (Task 10's
        // review, minor 3).
        report_attention(&state, attention_before);
        gtk4::glib::ControlFlow::Continue
    });
}

/// Dispatches each payload, in order.
fn dispatch_all(webview: &WebView, payloads: Vec<String>) {
    for payload in payloads {
        evaluate_js_dispatch(webview, &payload);
    }
}

/// The `tabs` envelope, recorded as the last one sent.
fn tabs_payload_recorded(state: &mut AgentPanelState) -> String {
    let payload = state.tabs.tabs_payload();
    state.last_tabs_payload = Some(payload.clone());
    payload
}

/// Sends `tabs` now: the set changed in a way the panel must see at once.
fn send_tabs(state: &Rc<RefCell<AgentPanelState>>, webview: &WebView) {
    let payload = tabs_payload_recorded(&mut state.borrow_mut());
    evaluate_js_dispatch(webview, &payload);
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
    /// The finished envelope, or why there is none.
    rx: mpsc::Receiver<Result<String, String>>,
}

/// Runs `work` on a worker thread; the tick sends what it returns (see [`poll_review_jobs`]).
/// Refused while [`MAX_REVIEW_JOBS`] are already out.
fn start_review_job(
    state: &Rc<RefCell<AgentPanelState>>,
    request_id: &str,
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
        rx,
    });
    Ok(())
}

/// What the finished jobs of `jobs` owe the panel, in order: an envelope for a review that was
/// worked out, a failing `command_result` for one that could not be (or whose worker died). The
/// finished ones leave `jobs`.
fn finished_reviews(jobs: &mut Vec<PendingReview>) -> Vec<String> {
    let mut payloads = Vec::new();
    jobs.retain(|job| match job.rx.try_recv() {
        Ok(Ok(envelope)) => {
            payloads.push(envelope);
            false
        }
        Ok(Err(why)) => {
            payloads.push(serialize_command_result_for_js(&job.request_id, Err(&why)));
            false
        }
        Err(mpsc::TryRecvError::Disconnected) => {
            payloads.push(serialize_command_result_for_js(
                &job.request_id,
                Err("the review stopped before it finished"),
            ));
            false
        }
        Err(mpsc::TryRecvError::Empty) => true,
    });
    payloads
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

/// The tick's half of a review: whatever a worker finished goes to the panel.
fn poll_review_jobs(state: &Rc<RefCell<AgentPanelState>>, webview: &WebView) {
    let payloads = {
        let mut state_ref = state.borrow_mut();
        if state_ref.review_jobs.is_empty() {
            return;
        }
        finished_reviews(&mut state_ref.review_jobs)
    };
    dispatch_all(webview, payloads);
}

/// C5's return half: every edit whose marker appeared goes back to the tab it came from (review
/// focus 4), and the keys return to the chat.
fn poll_scratch_edits(state: &Rc<RefCell<AgentPanelState>>, webview: &WebView) {
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
    for (edit, result) in finished {
        let returned = state.borrow_mut().tabs.finish_scratch_edit(edit.id, &result);
        edit.cleanup();
        let Some((tab, draft)) = returned else { continue };
        let active = state.borrow().tabs.active() == tab;
        if active {
            if let Some(text) = &draft {
                evaluate_js_dispatch(webview, &eitri_core::agent_bridge::serialize_draft_for_js(tab, text));
            }
            evaluate_js_dispatch(webview, &eitri_core::agent_bridge::serialize_scratch_for_js(tab, false));
        }
        if let eitri_core::scratch::EditDone::Failed(why) = &result {
            evaluate_js_dispatch(
                webview,
                &eitri_core::agent_bridge::serialize_notice_for_js(&format!("the nvim scratch buffer failed: {why}")),
            );
        }
    }
    let hook = state.borrow().editor_done_hook.clone();
    if let Some(hook) = hook {
        hook();
    }
}

/// The tick's `tabs` envelope: sent only when it differs from the last one sent (a marker, a
/// label, a state -- whatever `TabSet::tabs_payload` says).
///
/// Held back while no document has said `ready` (ruling 38), and then not recorded as sent, so it
/// goes out once one has.
fn send_tabs_if_changed(state: &Rc<RefCell<AgentPanelState>>, webview: &WebView) {
    let payload = {
        let mut state_ref = state.borrow_mut();
        let state_ref = &mut *state_ref;
        let now = state_ref.tabs.tabs_payload();
        changed_envelope(&mut state_ref.last_tabs_payload, now, state_ref.document_ready)
    };
    if let Some(payload) = payload {
        evaluate_js_dispatch(webview, &payload);
    }
}

/// V1 (ruling 32): the editor context's summary, when it changed.
fn send_context_if_changed(state: &Rc<RefCell<AgentPanelState>>, webview: &WebView) {
    let payload = {
        let mut state_ref = state.borrow_mut();
        let state_ref = &mut *state_ref;
        let summary =
            eitri_core::agent_bridge::context_summary((state_ref.editor_context)().as_ref(), &state_ref.project_dir);
        let now = eitri_core::agent_bridge::serialize_editor_context_for_js(summary.as_ref());
        changed_envelope(&mut state_ref.last_context_payload, now, state_ref.document_ready)
    };
    if let Some(payload) = payload {
        evaluate_js_dispatch(webview, &payload);
    }
}

/// Appends `texts` to the project's history and tells the panel (ruling 11). Best-effort: a failed
/// write is logged and the panel keeps the list it had.
fn remember_prompts(state: &Rc<RefCell<AgentPanelState>>, webview: &WebView, texts: &[String]) {
    let (dir, root) = {
        let s = state.borrow();
        (s.history_dir.clone(), s.project_dir.clone())
    };
    let Some(dir) = dir else { return };
    match eitri_core::prompt_history::append(&dir, &root, texts) {
        Ok(entries) => {
            let ready = {
                let mut s = state.borrow_mut();
                s.history = entries;
                s.document_ready
            };
            if ready {
                let payload = eitri_core::agent_bridge::serialize_history_for_js(&state.borrow().history);
                evaluate_js_dispatch(webview, &payload);
            }
        }
        Err(e) => eprintln!("[history] could not append: {e}"),
    }
}

/// The active tab's queue, after anything changed it.
fn send_queue(state: &Rc<RefCell<AgentPanelState>>, webview: &WebView, tab: TabId) {
    let payload = {
        let s = state.borrow();
        if s.tabs.active() != tab || !s.document_ready {
            return;
        }
        let t = s.tabs.get(tab).expect("the active tab exists");
        eitri_core::agent_bridge::serialize_queue_for_js(tab, &t.queue, t.queue_error.as_deref())
    };
    evaluate_js_dispatch(webview, &payload);
}

/// A flush's outcome (ruling 3): the same benign/fatal split a command gets, with no request to
/// answer. A refusal leaves the queue and its `error` line; the queue envelope says so.
fn apply_flush(state: &Rc<RefCell<AgentPanelState>>, webview: &WebView, tab: TabId, flush: eitri_core::tab_set::Flush) {
    if flush.outcome.is_ok() {
        title_from_first_prompt(state, tab, &flush.typed);
    }
    apply_outcome(state, webview, tab, None, flush.outcome);
    send_queue(state, webview, tab);
}

/// `tabs`, then the active tab's own state (ruling 18: always, not only when stale).
fn switch_payloads(state: &mut AgentPanelState) -> Vec<String> {
    let mut payloads = vec![tabs_payload_recorded(state)];
    payloads.extend(state.tabs.active_state_payloads());
    payloads
}

/// After the active tab changed: `mark_seen` if the panel is on screen, then `tabs` and the new
/// active tab's state.
fn send_switch(state: &Rc<RefCell<AgentPanelState>>, webview: &WebView) {
    let before = state.borrow().tabs.attention();
    let payloads = {
        let mut state_ref = state.borrow_mut();
        if webview.is_mapped() {
            state_ref.tabs.mark_seen();
        }
        switch_payloads(&mut state_ref)
    };
    dispatch_all(webview, payloads);
    report_attention(state, before);
}

/// Ruling 17: `hello` is window-level and re-sent, recomputed, whenever the set of open provider
/// sessions changes -- a session adopted, a resume installed, a tab closed or reset.
fn send_hello_if_open_sessions_changed(state: &Rc<RefCell<AgentPanelState>>, webview: &WebView) {
    let payload = {
        let mut state_ref = state.borrow_mut();
        // Ruling 38: `ready` sends `hello` itself, and records what it was computed against.
        if !state_ref.document_ready {
            return;
        }
        let open = state_ref.tabs.open_session_ids();
        // The saved tabs are on offer only while nothing here has started: when that stops being
        // true the dashboard's line has to go, though no session id changed with it.
        let offered = restore_is_on_offer(&state_ref);
        if open == state_ref.last_open_ids && offered == state_ref.last_restore_offered {
            return;
        }
        let mut greeting = BackendGreeting::for_kind(state_ref.backend_kind, state_ref.project_dir.clone());
        greeting.resumable.retain(|r| !open.contains(&r.provider_session_id));
        let offer = restore_offer(&state_ref, &greeting);
        state_ref.last_open_ids = open;
        state_ref.last_restore_offered = offered;
        serialize_hello_with_restore_for_js(&greeting, offer.as_ref())
    };
    evaluate_js_dispatch(webview, &payload);
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
fn restore_offer(state: &AgentPanelState, greeting: &BackendGreeting) -> Option<RestoreOffer> {
    if !restore_is_on_offer(state) {
        return None;
    }
    restore_plan(state, greeting).offer()
}

/// Which of the saved tabs can come back, judged against the records and leases as they are now.
fn restore_plan(state: &AgentPanelState, greeting: &BackendGreeting) -> eitri_core::tab_restore::RestorePlan {
    let canonical = state.canonical_project_dir.as_str();
    let saved = state.restore_source.clone().unwrap_or_default();
    eitri_core::tab_restore::plan(&saved, &greeting.resumable, &state.tabs.open_session_ids(), |id| {
        agent::lease::SessionLease::is_held("claude", canonical, id).unwrap_or(false)
    })
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
}

/// Begins one resume: given a session and the lease already taken for it, the receiver its result
/// will arrive on.
type SpawnResume<'a> =
    &'a mut dyn FnMut(&str, agent::lease::SessionLease) -> mpsc::Receiver<Result<AgentBackend, BackendError>>;

/// A restore's resumes, as the real workers start them.
fn resume_spawner(
    state: &AgentPanelState,
) -> impl FnMut(&str, agent::lease::SessionLease) -> mpsc::Receiver<Result<AgentBackend, BackendError>> {
    let (kind, project_dir) = (state.backend_kind, state.project_dir.clone());
    move |id, lease| spawn_connect_holding(kind, project_dir.clone(), Some(id.to_string()), Some(lease))
}

/// Asks for the last window's tabs back, into `from` (the empty tab the key was pressed in) and new
/// tabs. Nothing is started unless this window has started nothing yet.
fn ask_for_restore(state: &mut AgentPanelState, from: TabId, ask: RestoreAsk, spawn: SpawnResume<'_>) -> RestoreResult {
    if state.backend_kind != BackendKind::Sidecar {
        return RestoreResult::Refused("only the sidecar backend can resume sessions".to_string());
    }
    if state.restore_run.is_some() {
        return RestoreResult::Refused("the last session's tabs are already being restored".to_string());
    }
    if !state.tabs.is_pristine() {
        return RestoreResult::Refused(
            "the last session's tabs can be restored only before a session has started".to_string(),
        );
    }
    if state.restore_source.is_none() {
        return RestoreResult::Refused("there are no saved tabs to restore".to_string());
    }
    let greeting = BackendGreeting::for_kind(state.backend_kind, state.project_dir.clone());
    let plan = restore_plan(state, &greeting);
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
        RestoreStep::Go(go) => start_restore(state, from, go, spawn),
    }
}

/// `y` or `n` to the question [`ask_for_restore`] raised.
fn answer_restore_question(
    state: &mut AgentPanelState,
    nonce: u64,
    keep_bypass: bool,
    spawn: SpawnResume<'_>,
) -> RestoreResult {
    match state.tabs.answer_restore(nonce, keep_bypass) {
        Err(why) => RestoreResult::Refused(why),
        Ok(go) => {
            let from = state.tabs.active();
            start_restore(state, from, go, spawn)
        }
    }
}

/// Takes every lease first and only then starts the resumes, so that nothing -- a record pruned to
/// make room for another session, another window -- can claim a saved session between the decision
/// and its own resume. A session whose lease cannot be taken is skipped with the reason.
fn start_restore(state: &mut AgentPanelState, from: TabId, go: RestoreGo, spawn: SpawnResume<'_>) -> RestoreResult {
    let canonical = state.canonical_project_dir.clone();
    let mut leases: std::collections::HashMap<String, Result<agent::lease::SessionLease, String>> = go
        .session_ids()
        .into_iter()
        .map(|id| {
            let lease = match agent::lease::SessionLease::try_acquire("claude", &canonical, &id) {
                Ok(lease) => Ok(lease),
                Err(agent::lease::LeaseError::AlreadyHeld) => Err(eitri_core::tab_restore::HELD_ELSEWHERE.to_string()),
                Err(other) => Err(format!("its session lease could not be taken ({other})")),
            };
            (id, lease)
        })
        .collect();
    let (downgraded, asked) = (go.downgraded, go.asked);
    let run = state.tabs.start_restore(from, go, |id| match leases.remove(id) {
        Some(Ok(lease)) => Ok(spawn(id, lease)),
        Some(Err(reason)) => Err(reason),
        None => Err("it was listed twice".to_string()),
    });
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
        let outcome = state_ref.restore_run.as_ref().and_then(|run| run.outcome());
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

/// What a [`RestoreResult`] owes the page; `Err` is a refusal for the caller to report.
fn deliver_restore(
    state: &Rc<RefCell<AgentPanelState>>,
    webview: &WebView,
    result: RestoreResult,
) -> Result<(), String> {
    match result {
        RestoreResult::Refused(why) => Err(why),
        RestoreResult::Question(prompt) => {
            evaluate_js_dispatch(webview, &serialize_confirm_restore_for_js(&prompt));
            Ok(())
        }
        RestoreResult::Started { notice } => {
            send_switch(state, webview);
            if let Some(notice) = notice {
                evaluate_js_dispatch(webview, &eitri_core::agent_bridge::serialize_notice_for_js(&notice));
            }
            finish_restore(state);
            Ok(())
        }
    }
}

/// `agent.restore = "auto"`: on the first page that says `ready`, bring the last window's tabs back
/// with no key pressed. Used up on its first go, so a reload of the page does not do it again.
fn restore_at_launch(state: &Rc<RefCell<AgentPanelState>>, webview: &WebView) {
    let result = {
        let mut state_ref = state.borrow_mut();
        if state_ref.restore_policy != RestorePolicy::Auto || state_ref.restore_source.is_none() {
            return;
        }
        let from = state_ref.tabs.active();
        let mut spawn = resume_spawner(&state_ref);
        ask_for_restore(&mut state_ref, from, RestoreAsk::Launch, &mut spawn)
    };
    if let Err(why) = deliver_restore(state, webview, result) {
        eprintln!("[restore] not restored: {why}");
    }
}

/// Keeps the saved-tabs file in step with the tabs: once per tick, cheap when nothing changed.
fn remember_tabs(state: &Rc<RefCell<AgentPanelState>>) {
    let mut state_ref = state.borrow_mut();
    let state_ref = &mut *state_ref;
    let snapshot = state_ref.tabs.saved_snapshot(&state_ref.conversation_id);
    match state_ref.tab_memory.observe(snapshot) {
        Some(Err(e)) => eprintln!("[tabs] could not save the open tabs: {e}"),
        Some(Ok(written)) => {
            if let Some(aside) = written.set_aside {
                eprintln!(
                    "[tabs] an unusable saved-tabs file was set aside as {}",
                    aside.display()
                );
            }
        }
        None => {}
    }
}

/// The chooser's envelope (`prefix w`): the open tabs in number order, then every record open
/// in no tab, each marked if another window holds its lease.
fn chooser_payload(state: &AgentPanelState) -> String {
    let greeting = BackendGreeting::for_kind(state.backend_kind, state.project_dir.clone());
    let resumable = state.backend_kind == BackendKind::Sidecar;
    let open: Vec<ChooserTab> = state
        .tabs
        .tabs()
        .iter()
        .map(|t| {
            let facts = t.facts();
            ChooserTab {
                tab: t.id,
                label: eitri_core::tabs::label(t.number, &t.label_name()),
                marker: eitri_core::tabs::marker(facts),
                pending: facts.pending,
                resumable,
            }
        })
        .collect();
    let canonical = state.canonical_project_dir.as_str();
    let records = chooser_records(&greeting.resumable, &state.tabs.open_session_ids(), |id| {
        agent::lease::SessionLease::is_held("claude", canonical, id).unwrap_or(false)
    });
    eitri_core::agent_bridge::serialize_chooser_for_js(&open, &records)
}

/// The detail popover's envelope for `tab` (`prefix i`, `open_detail{tab}`).
fn detail_payload(state: &AgentPanelState, tab: TabId) -> String {
    let rows = match state.tabs.get(tab) {
        Some(t) => detail_rows(
            t,
            state.backend_kind,
            agent::account::configured().map(|a| a.name()),
            &state.project_dir,
            state
                .rules_dir
                .as_ref()
                .map(|d| eitri_core::permission_store::path(d, &state.project_dir))
                .as_deref(),
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

/// `prefix i`'s rows for one tab, in spec §3.3's order, `—` for anything unknown.
///
/// `provider_session_id()` is read into a local BEFORE `projection()` is taken: on the sidecar path
/// both lock the same ingestion mutex (the 2026-09-15 GTK freeze; `tab_set`'s module doc).
fn detail_rows(
    tab: &Tab,
    kind: BackendKind,
    account: Option<&str>,
    project_dir: &Path,
    rules_file: Option<&Path>,
) -> Vec<DetailRow> {
    const UNKNOWN: &str = "—";
    let known = |value: Option<String>| value.filter(|v| !v.is_empty()).unwrap_or_else(|| UNKNOWN.to_string());
    let backend = tab.live();
    let provider_session_id = backend.and_then(|b| b.provider_session_id());
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
    let resumable = match kind {
        BackendKind::Sidecar => "yes",
        BackendKind::Legacy => "no (legacy backend)",
    };
    let rows: [(&str, String); 19] = [
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
        // R13: a fact about every session on either backend, so it is drawn on an empty tab too.
        ("settings", agent::setting_sources::note().to_string()),
        ("Verdandi revision", known(revision)),
        ("resumable", resumable.to_string()),
        ("created", known(record.as_ref().map(|r| r.created_at.clone()))),
        ("updated", known(record.as_ref().map(|r| r.updated_at.clone()))),
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

/// The connect worker, shared by `send_message` on an empty tab and `resume`. Off the GTK main
/// loop: constructing the sidecar backend spawns a real process, does a real gRPC handshake, and on
/// a cold Verdandi checkout runs `npm ci` + `npm run build` -- minutes, in the cold case.
fn spawn_connect(
    kind: BackendKind,
    project_dir: PathBuf,
    resume: Option<String>,
) -> mpsc::Receiver<Result<AgentBackend, BackendError>> {
    spawn_connect_holding(kind, project_dir, resume, None)
}

/// [`spawn_connect`] for a resume whose session lease is already held: the worker hands it to the
/// resume, which keeps it for as long as the session lives.
fn spawn_connect_holding(
    kind: BackendKind,
    project_dir: PathBuf,
    resume: Option<String>,
    lease: Option<agent::lease::SessionLease>,
) -> mpsc::Receiver<Result<AgentBackend, BackendError>> {
    let (result_tx, result_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = AgentBackend::start_holding(kind, &project_dir, resume.as_deref(), lease);
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
/// it. `agent-ui/web/src/problems.ts`'s `classifySidecarStopped`/`failureEvidence` already read
/// this reason correctly on the panel side (2026-09-27, "v1 polish", item 6); this closes the
/// wrapper's own half of the same item (owner decision (b), dated record 2026-09-27, "v1 polish"):
/// a `reason` naming a stopped sidecar gets its own wording, with no guess appended, instead of
/// being folded into the generic case.
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
/// no equivalent check: either way the tab becomes `Failed` with the provider's own reason (ruling
/// 14), where `r` starts it over. It is never turned into a fresh session automatically. The
/// `error{tab}` envelope goes out only if that tab is active; the tab keeps the reason for when it
/// is shown, and no other tab is touched.
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
fn report_sessions_that_never_opened(state: &Rc<RefCell<AgentPanelState>>, webview: &WebView) {
    let failed: Vec<(TabId, String)> = {
        let state_ref = state.borrow();
        state_ref
            .tabs
            .tabs()
            .iter()
            .filter(|t| !t.reported_start_failure)
            .filter_map(|t| t.live().and_then(|b| b.terminated_before_opening()).map(|r| (t.id, r)))
            .collect()
    };
    for (tab, reason) in failed {
        let message = never_opened_message(&reason);
        eprintln!("[agent_panel] tab {}: {message}", tab.0);
        let (dead, active) = {
            let mut state_ref = state.borrow_mut();
            let active = state_ref.tabs.active() == tab;
            let on_screen = active && webview.is_mapped();
            let Some(t) = state_ref.tabs.get_mut(tab) else { continue };
            t.reported_start_failure = true;
            // Its cards go with it (the tick reports the change).
            t.attention.session_ended(on_screen);
            let dead = std::mem::replace(
                &mut t.backend,
                TabBackend::Failed {
                    reason: message.clone(),
                },
            );
            (dead, active)
        };
        if let TabBackend::Live(backend) = dead {
            state.borrow_mut().retiring.backend(backend);
        }
        if active {
            evaluate_js_dispatch(webview, &serialize_error_for_js(tab, &message));
        }
    }
}

/// Every connect that finished since the last tick (`TabSet::collect_starts`), answered. `try_recv`
/// only, inside the set: this runs on the GTK main loop 30 times a second and must never block it --
/// which is the entire reason construction was moved off this thread in the first place.
fn collect_pending_starts(state: &Rc<RefCell<AgentPanelState>>, webview: &WebView) {
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
                // The panel has been showing this tab as starting; give it the real projection at
                // once rather than making it wait for the first event.
                let payloads = {
                    let mut state_ref = state.borrow_mut();
                    if state_ref.tabs.active() == tab {
                        switch_payloads(&mut state_ref)
                    } else {
                        Vec::new()
                    }
                };
                dispatch_all(webview, payloads);
                if !restored {
                    match first_turn {
                        Some(turn) => send_first_turn(state, webview, tab, &request_id, turn),
                        None => evaluate_js_dispatch(webview, &serialize_command_result_for_js(&request_id, Ok(()))),
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
                        send_switch(state, webview);
                    }
                    continue;
                }
                evaluate_js_dispatch(
                    webview,
                    &serialize_command_result_for_js(&request_id, Err(&error.message)),
                );
                // Ruling 14: only if that tab is on screen; it keeps the reason for later.
                if state.borrow().tabs.active() == tab {
                    evaluate_js_dispatch(webview, &serialize_error_for_js(tab, &error.message));
                }
            }
        }
    }
    finish_restore(state);
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
    let AgentPanelState { restore_run, tabs, .. } = state;
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
            tabs.discard_failed_restore(*tab, back_to);
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

/// The first turn of a session started by it (ruling 4), sent once its backend is installed. Its
/// `command_result` is the one the `send_message` that started the session was owed.
fn send_first_turn(
    state: &Rc<RefCell<AgentPanelState>>,
    webview: &WebView,
    tab: TabId,
    request_id: &str,
    turn: FirstTurn,
) {
    let outcome = {
        let mut state_ref = state.borrow_mut();
        if let Some(t) = state_ref.tabs.get_mut(tab) {
            t.turn_trace = eitri_core::turn_trace::TurnTrace::start();
        }
        backend_for(&mut state_ref.tabs, tab).and_then(|backend| backend.send_turn(&turn.wire, &turn.typed))
    };
    if outcome.is_ok() {
        title_from_first_prompt(state, tab, &turn.typed);
    }
    apply_command_outcome(state, webview, tab, request_id, outcome);
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
/// The command is dispatched here and nowhere else, which is what makes the §8.3 ordering real: by
/// the time the frontend can show a user the line to run, the session's own `shutdown()` has
/// already returned on the worker thread. The handoff card goes out only if its tab is active
/// (the tab keeps it in `last_handoff` for when it is shown, ruling 13); the `command_result` is
/// owed either way, and the tick's `tabs` envelope reports the state change.
fn collect_pending_handoffs(state: &Rc<RefCell<AgentPanelState>>, webview: &WebView) {
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
            let active = state_ref.tabs.active() == tab;
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
            (pending, outcome, active)
        };
        let (pending, outcome, active) = finished;
        if outcome == HandoffCloseOutcome::CloseFailed {
            eprintln!("[agent_panel] tab {}: {HANDOFF_CLOSE_FAILED_MESSAGE}", tab.0);
        }
        if active {
            dispatch_all(
                webview,
                handoff_payloads(tab, &outcome, &pending.request_id, &pending.command),
            );
        } else {
            let result = match outcome {
                HandoffCloseOutcome::CloseFailed => Err(HANDOFF_CLOSE_FAILED_MESSAGE),
                _ => Ok(()),
            };
            evaluate_js_dispatch(webview, &serialize_command_result_for_js(&pending.request_id, result));
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
/// is on screen.
///
/// The property worth stating: only `Closed` ever produces a `handoff` envelope. A close that is
/// still running, or one that died partway, must not be followed by a card whose first line claims
/// the conversation is closed.
fn handoff_payloads(
    tab: TabId,
    outcome: &HandoffCloseOutcome,
    request_id: &str,
    command: &agent::handoff::ClaudeResumeCommand,
) -> Vec<String> {
    match outcome {
        HandoffCloseOutcome::StillClosing => Vec::new(),
        HandoffCloseOutcome::Closed => vec![
            eitri_core::agent_bridge::serialize_handoff_for_js(tab, command),
            serialize_command_result_for_js(request_id, Ok(())),
        ],
        // The session is gone regardless -- it was handed to the worker before this. Saying so is
        // the only honest option: an `error` envelope returns the tab to its empty screen, where a
        // failed handoff reads as a dead session rather than as a conversation that is still there.
        HandoffCloseOutcome::CloseFailed => vec![
            serialize_command_result_for_js(request_id, Err(HANDOFF_CLOSE_FAILED_MESSAGE)),
            serialize_error_for_js(tab, HANDOFF_CLOSE_FAILED_MESSAGE),
        ],
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
    // Ruling 17: no session open in any tab, or handed off from one, is offered for resume -- the
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

/// C1's mirror (spec §3.5, `AgentPanelState::nav_mode`'s own doc): called from the `Ready` arm on
/// every `ready` -- the first one and every reload's alike -- so the mirror never survives a
/// reload holding whatever mode the torn-down document last reported.
///
/// Without this, a reload while the composer is open (`nav_mode == Input`) leaves the mirror at
/// `Input` even though the fresh document starts in BROWSE. Before that document posts its own
/// first `panel_keys` message, `main.rs`'s `claims(Input, Direction::Down)` is `false`, so a
/// `Ctrl+j` the user presses to enter the composer is not claimed and falls through to
/// `move_focus` instead -- the opposite of what BROWSE + `Ctrl+j` must do (spec §3.1).
///
/// A plain field write rather than a bigger function only because it is one: kept apart from the
/// rest of `Ready`'s payload-building so it has its own doc and its own test
/// (`ready_resets_the_nav_mode_mirror_to_other`), the same reason `ready_payloads` and
/// `changed_envelope` are pulled out of that arm.
fn reset_nav_mode_for_ready(state: &AgentPanelState) {
    state.nav_mode.set(PanelKeys::Other);
}

/// The other half of the same rule: when the page that reported a mode goes -- its web process
/// died (`on_web_process_terminated`, both the reload and the give-up path) or it is being
/// replaced (`reload_document`) -- the mirror is `Other` until a new page says otherwise. The
/// give-up document (`webview_crash_guard::crash_message_html`) never sends `ready`, so the reset
/// on `ready` alone left `Ctrl+j` claimed for a page that could not take it.
fn forget_the_pages_nav_mode(state: &AgentPanelState) {
    state.nav_mode.set(PanelKeys::Other);
}

/// A window-level envelope for the tick: `Some` when the page can take it and it changed since the
/// last one sent (phase 3 ruling 38). Recorded as sent only when it is returned.
fn changed_envelope(last: &mut Option<String>, now: String, document_ready: bool) -> Option<String> {
    if !document_ready || last.as_deref() == Some(now.as_str()) {
        return None;
    }
    *last = Some(now.clone());
    Some(now)
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

/// Everything a `permission_response` does, with no WebView, so its route is tested whole (O3 review
/// #4): the rule the card offered is saved first when the user chose "Always allow" (never taken
/// from the panel, ruling 16), then the answer goes through `TabSet::answer_card` -- the one route
/// that records a human approval, which is what lets the CLI's own prompt for the same call through
/// without a second card (O3 ruling 5). `Err` is a refusal the panel shows (the card stays and `a`
/// still works); `Ok` is the answer's own outcome.
fn answer_permission_response(
    tabs: &mut eitri_core::tab_set::TabSet,
    rules_dir: Option<&Path>,
    project_dir: &Path,
    tab: TabId,
    answer: PermissionAnswer,
) -> Result<Result<Vec<agent::AgentDomainEvent>, BackendError>, String> {
    let remembered = {
        let t = tabs.get(tab).ok_or_else(|| format!("protocol: no tab {}", tab.0))?;
        // Panel-content review finding 4: read right before the save, so an id that is no longer
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
            // Not answered: the card stays and `a` still works (ruling 16).
            Err(e) => return Err(format!("could not save the rule: {e}")),
        }
    }
    let decision = answer.decision.into_decision(answer.reason);
    Ok(tabs.answer_card(tab, &answer.permission_id, decision))
}

/// D7's `remember` (ruling 16): the rule Rust offered for `permission_id`, only with an allow, and
/// only while that id is still a card waiting for the user (panel-content review finding 4).
/// `still_pending` is the caller's own read (`Tab::card_is_waiting`), taken right before this runs
/// -- passed in rather than read here so this stays testable without a live backend, the same
/// reason `changed_envelope` takes `document_ready` as a parameter.
///
/// The offer itself is gone once the first answer to that id succeeds, whatever the decision, and
/// never comes back (`TabSet::answer_card`, the tab's `user_answered`): on the sidecar path the
/// projection still lists the id as pending until the provider's real `PermissionResolved`
/// arrives, and `refresh_offers` recomputes offers from that projection whenever another id
/// arrives, so a one-time removal alone was undone by the next pump (the whole-branch review).
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
/// prompt, and -- since v1-mode fix round 1 -- the `PermissionResolved`s entering bypass produced on
/// legacy (`ConfirmOutcome::Entered::resolved`), which no pump ever carries. `None` for an empty
/// batch, or for a tab that is not the active one: the WebView draws only that tab, and a background
/// tab catches up with a snapshot when it is switched to. The revisions are the projection's, as the
/// events were folded into it one each, just now, on this thread.
fn returned_events_payload(state: &AgentPanelState, tab: TabId, events: &[agent::AgentDomainEvent]) -> Option<String> {
    if events.is_empty() || state.tabs.active() != tab {
        return None;
    }
    let through_revision = state
        .tabs
        .get(tab)
        .and_then(|t| t.live())
        .map(|b| b.projection().last_revision)
        .unwrap_or(0);
    let from_revision = through_revision.saturating_sub(events.len() as u64);
    Some(serialize_events_for_js(tab, from_revision, through_revision, events))
}

/// Applies one command's outcome to its tab and the frontend, uniformly.
///
/// The benign/fatal split is the whole point: a benign failure (a turn sent while one was running,
/// a permission answered twice) reports itself and leaves the session alone, while a fatal one
/// retires the tab's backend and marks the tab `Failed` (ruling 14). Events and the `error`
/// envelope go out only if `tab` is the active one -- the WebView draws only that tab, and a
/// background tab catches up with a snapshot when it is switched to. The `command_result` is owed
/// either way.
fn apply_command_outcome(
    state: &Rc<RefCell<AgentPanelState>>,
    webview: &WebView,
    tab: TabId,
    request_id: &str,
    outcome: Result<Vec<agent::AgentDomainEvent>, BackendError>,
) {
    apply_outcome(state, webview, tab, Some(request_id), outcome);
}

/// [`apply_command_outcome`] for an outcome no request is waiting on (a queue flush, ruling 3):
/// `request_id` is `None` and no `command_result` goes out.
fn apply_outcome(
    state: &Rc<RefCell<AgentPanelState>>,
    webview: &WebView,
    tab: TabId,
    request_id: Option<&str>,
    outcome: Result<Vec<agent::AgentDomainEvent>, BackendError>,
) {
    let events = events_owed(&outcome);
    let active = state.borrow().tabs.active() == tab;
    // Only the legacy backend ever gets here with a non-empty batch: it synthesizes events its own
    // wire protocol cannot provide. The sidecar backend returns an empty vec and its state arrives
    // through the pump, from the server.
    let payload = returned_events_payload(&state.borrow(), tab, events);
    if let Some(payload) = payload {
        evaluate_js_dispatch(webview, &payload);
    }
    match outcome {
        Ok(_) => {
            if let Some(id) = request_id {
                evaluate_js_dispatch(webview, &serialize_command_result_for_js(id, Ok(())));
            }
        }
        Err(error) if error.benign => {
            eprintln!(
                "[agent_panel] tab {}: command rejected (session stays alive): {}",
                tab.0, error.message
            );
            if let Some(id) = request_id {
                evaluate_js_dispatch(webview, &serialize_command_result_for_js(id, Err(&error.message)));
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
            let on_screen = active && webview.is_mapped();
            let dead = {
                let mut state_ref = state.borrow_mut();
                state_ref.tabs.get_mut(tab).map(|t| {
                    // The cards of a session that is gone can no longer be answered.
                    t.attention.session_ended(on_screen);
                    std::mem::replace(
                        &mut t.backend,
                        TabBackend::Failed {
                            reason: error.message.clone(),
                        },
                    )
                })
            };
            if let Some(TabBackend::Live(backend)) = dead {
                state.borrow_mut().retiring.backend(backend);
            }
            if let Some(id) = request_id {
                evaluate_js_dispatch(webview, &serialize_command_result_for_js(id, Err(&error.message)));
            }
            if active {
                evaluate_js_dispatch(webview, &serialize_error_for_js(tab, &error.message));
            }
            report_attention(state, attention_before);
        }
    }
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

fn handle_inbound_message(raw: &str, state: &Rc<RefCell<AgentPanelState>>, webview: &WebView) {
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
    // Spec §3.8 point 2 and ruling 2: a tab command names its tab, or it is refused. Never "the
    // active one" -- a permission answered just after a switch must reach the tab that asked.
    let target = match state.borrow().tabs.resolve(message.tab_ref()) {
        Ok(target) => target,
        Err(protocol) => {
            eprintln!("[agent_panel] refused {request_id}: {protocol}");
            evaluate_js_dispatch(webview, &serialize_command_result_for_js(&request_id, Err(&protocol)));
            return;
        }
    };
    let tab_of = |target: Option<TabId>| target.expect("a tab-scoped message resolved to a tab");
    let ok = |webview: &WebView| evaluate_js_dispatch(webview, &serialize_command_result_for_js(&request_id, Ok(())));
    let refuse = |webview: &WebView, why: &str| {
        evaluate_js_dispatch(webview, &serialize_command_result_for_js(&request_id, Err(why)));
    };

    match message {
        InboundMessage::Ready { .. } => {
            // A stream envelope the typing cadence still holds is already in the snapshot below
            // (it was drained from the queue and folded into the projection before it was held);
            // sent after that snapshot it would draw the same text twice.
            let discarded = crate::panel_pacer::pacer_for(webview).borrow_mut().discard_pending();
            trace_discarded_first_texts(state, discarded);
            // Everything a fresh document is owed is decided by one pure function, so the reload
            // path is testable without a WebView -- see `ready_payloads`. Wave 4 R2 (owner: "默认
            // 界面是new session这个界面，不用默认弹到all session界面"): a `ready` never opens the
            // chooser any more, on the first one or on a reload -- the empty tab's dashboard is
            // what a fresh document sees.
            let payloads = {
                let mut state_ref = state.borrow_mut();
                let state_ref = &mut *state_ref;
                // C1's mirror (spec §3.5): "it starts other and returns to other on every ready
                // (a reload)". A fresh document starts BROWSE-or-dashboard, never whatever mode
                // the torn-down document last reported, so the mirror must not carry the old
                // value across -- see `reset_nav_mode_for_ready`'s own doc for the failure this
                // closes.
                reset_nav_mode_for_ready(state_ref);
                let greeting = BackendGreeting::for_kind(state_ref.backend_kind, state_ref.project_dir.clone());
                let open = state_ref.tabs.open_session_ids();
                let theme = eitri_core::agent_bridge::serialize_theme_for_js(&state_ref.theme);
                let context = eitri_core::agent_bridge::context_summary(
                    (state_ref.editor_context)().as_ref(),
                    &state_ref.project_dir,
                );
                let context_payload = eitri_core::agent_bridge::serialize_editor_context_for_js(context.as_ref());
                state_ref.last_context_payload = Some(context_payload.clone());
                let window = window_payloads(
                    eitri_core::agent_bridge::serialize_history_for_js(&state_ref.history),
                    context_payload,
                    state_ref.editor_link.as_ref(),
                );
                let tabs = tabs_payload_recorded(state_ref);
                let active = state_ref.tabs.active_state_payloads();
                // What the last window left open, if the dashboard may offer it: a reloaded page
                // is told again, like everything else in the greeting.
                let mut offered_greeting = greeting;
                offered_greeting
                    .resumable
                    .retain(|r| !open.contains(&r.provider_session_id));
                let offer = restore_offer(state_ref, &offered_greeting);
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
                // Last, so nothing the document draws from the payloads above can reset it.
                payloads.push(eitri_core::agent_bridge::serialize_pane_focus_for_js(
                    state_ref.pane_focused,
                ));
                state_ref.last_open_ids = open;
                state_ref.last_restore_offered = restore_is_on_offer(state_ref);
                // Ruling 38: from here on the tick may send into this document.
                state_ref.document_ready = true;
                payloads
            };
            dispatch_all(webview, payloads);
            ok(webview);
            // `agent.restore = "auto"`: only once the page can take what it starts.
            restore_at_launch(state, webview);
        }
        InboundMessage::RestoreLast { .. } => {
            let tab = tab_of(target);
            let result = {
                let mut state_ref = state.borrow_mut();
                let mut spawn = resume_spawner(&state_ref);
                ask_for_restore(&mut state_ref, tab, RestoreAsk::Dashboard, &mut spawn)
            };
            match deliver_restore(state, webview, result) {
                Ok(()) => ok(webview),
                Err(why) => refuse(webview, &why),
            }
        }
        InboundMessage::RestoreAnswer { nonce, keep_bypass, .. } => {
            let result = {
                let mut state_ref = state.borrow_mut();
                let mut spawn = resume_spawner(&state_ref);
                answer_restore_question(&mut state_ref, nonce, keep_bypass, &mut spawn)
            };
            match deliver_restore(state, webview, result) {
                Ok(()) => ok(webview),
                Err(why) => refuse(webview, &why),
            }
        }
        InboundMessage::SendMessage { text, .. } => {
            let tab = tab_of(target);
            // A sent prompt is history whether or not the backend takes it, as in Claude Code.
            remember_prompts(state, webview, std::slice::from_ref(&text));
            // ...and no longer the tab's draft (a refusal puts it back in the box panel-side).
            state.borrow_mut().tabs.note_sent(tab);
            enum Plan {
                Sent(Result<Vec<agent::AgentDomainEvent>, BackendError>),
                Starting,
                Refused(&'static str),
            }
            let plan = {
                let mut state_ref = state.borrow_mut();
                let state_ref = &mut *state_ref;
                // wire 1's one composition point, read when Enter is pressed (ruling 4). Above both
                // backends on purpose: the turn's String travels unmodified from here to
                // `send_turn` on either path. A `None` context means the turn goes out exactly as
                // the user typed it.
                let composed =
                    eitri_core::editor_context::compose_turn_text(&text, (state_ref.editor_context)().as_ref());
                let kind = state_ref.backend_kind;
                let project_dir = state_ref.project_dir.clone();
                let t = state_ref.tabs.get_mut(tab).expect("resolved above");
                if t.pending_handoff.is_some() {
                    // Starting here would spawn a second `claude` alongside the one still being
                    // closed, in the same project.
                    Plan::Refused("the previous session is still being handed off to a terminal")
                } else {
                    match &mut t.backend {
                        TabBackend::Live(backend) => {
                            // Stamped before the call, so the trace's zero is the user's action
                            // rather than the moment the backend got around to accepting it.
                            let trace = eitri_core::turn_trace::TurnTrace::start();
                            // `&text` second, and it is the user's own: the panel shows what was
                            // typed, never the composed wire text.
                            let outcome = backend.send_turn(&composed, &text);
                            t.turn_trace = trace;
                            Plan::Sent(outcome)
                        }
                        TabBackend::NotStarted => {
                            let result_rx = spawn_connect(kind, project_dir, None);
                            t.backend = TabBackend::Starting(PendingStart {
                                request_id: request_id.clone(),
                                result_rx,
                                first_turn: Some(FirstTurn {
                                    wire: composed,
                                    typed: text.clone(),
                                }),
                                resume: None,
                                resumed_title: None,
                                resumed_name: None,
                            });
                            Plan::Starting
                        }
                        TabBackend::Starting(_) => Plan::Refused("the session is still starting"),
                        TabBackend::Failed { .. } => Plan::Refused("this tab's session failed; press r to start over"),
                    }
                }
            };
            match plan {
                Plan::Sent(outcome) => {
                    if outcome.is_ok() {
                        title_from_first_prompt(state, tab, &text);
                    }
                    apply_command_outcome(state, webview, tab, &request_id, outcome);
                }
                // The `command_result` is owed once the connect finishes and the first turn is
                // sent (`collect_pending_starts`).
                Plan::Starting => send_tabs(state, webview),
                Plan::Refused(why) => refuse(webview, why),
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
                refuse(webview, "the previous session is still being handed off to a terminal");
                return;
            }
            let canonical = state.borrow().canonical_project_dir.clone();
            let held = agent::lease::SessionLease::is_held("claude", &canonical, &provider_session_id).unwrap_or(false);
            let route = state.borrow_mut().tabs.route_resume(tab, &provider_session_id, held);
            match route {
                ResumeRoute::SwitchTo(_) => {
                    send_switch(state, webview);
                    ok(webview);
                }
                ResumeRoute::Refuse(why) => refuse(webview, &why),
                ResumeRoute::StartIn(target_tab) => {
                    {
                        let mut state_ref = state.borrow_mut();
                        let state_ref = &mut *state_ref;
                        let greeting = BackendGreeting::for_kind(state_ref.backend_kind, state_ref.project_dir.clone());
                        // The rename and the title separately (the whole-branch review): the
                        // label puts a rename first (spec §3.2), and folding the two into the
                        // title lost the rename whenever the record had both.
                        let (resumed_title, resumed_name) = greeting
                            .resumable
                            .iter()
                            .find(|r| r.provider_session_id == provider_session_id)
                            .map(|r| (r.title.clone(), r.name.clone()))
                            .unwrap_or_default();
                        let kind = state_ref.backend_kind;
                        let project_dir = state_ref.project_dir.clone();
                        let t = state_ref
                            .tabs
                            .get_mut(target_tab)
                            .expect("route_resume names a tab it has");
                        let result_rx = spawn_connect(kind, project_dir, Some(provider_session_id.clone()));
                        t.backend = TabBackend::Starting(PendingStart {
                            request_id: request_id.clone(),
                            result_rx,
                            first_turn: None,
                            resume: Some(provider_session_id),
                            resumed_title,
                            resumed_name,
                        });
                        // A new tab is already selected by `route_resume`; this is for the record.
                        state_ref.tabs.select(target_tab);
                    }
                    // The command_result is owed once the connect finishes.
                    send_switch(state, webview);
                }
            }
        }
        InboundMessage::Interrupt { .. } => {
            let tab = tab_of(target);
            let outcome = backend_for(&mut state.borrow_mut().tabs, tab).and_then(|backend| backend.interrupt());
            apply_command_outcome(state, webview, tab, &request_id, outcome);
        }
        message @ InboundMessage::PermissionResponse { .. } => {
            let tab = tab_of(target);
            let answer = PermissionAnswer::of(message).expect("matched as a permission_response above");
            let (rules_dir, project_dir) = {
                let s = state.borrow();
                (s.rules_dir.clone(), s.project_dir.clone())
            };
            let answered = answer_permission_response(
                &mut state.borrow_mut().tabs,
                rules_dir.as_deref(),
                &project_dir,
                tab,
                answer,
            );
            match answered {
                Err(why) => refuse(webview, &why),
                Ok(outcome) => apply_command_outcome(state, webview, tab, &request_id, outcome),
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
            let stamps = crate::panel_pacer::pacer_for(webview)
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
                refuse(webview, "this conversation is already being handed off");
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
                    refuse(webview, &refusal.message());
                    return;
                }
            };

            // Design doc §8.3 steps 1-5, in the only order that is safe: from here on this tab
            // holds no session (`NotStarted` at once, ruling 13), so no further turn can be sent,
            // and `shutdown()` itself cancels every pending permission (fail-closed) and closes the
            // provider's session.
            let taken = {
                let mut state_ref = state.borrow_mut();
                // The whole-branch review (gate): the tab becomes `NotStarted` under whatever bypass
                // prompt was open for it -- a live tab's `Switch to bypass?` whose `y` would then also
                // have moved the window default (spec §7.2). Reached by clicks that never pass
                // through the panel's y/n, so it is dropped here; `confirm_bypass`'s own re-check of
                // which question was asked is the other half.
                state_ref.tabs.drop_bypass_prompt();
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
                refuse(webview, "the conversation ended before it could be handed off");
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
            if let Some(t) = state.borrow_mut().tabs.get_mut(tab) {
                t.pending_handoff = Some(PendingHandoff {
                    request_id,
                    command,
                    closed_rx,
                    dropped_rx,
                });
            }
            // No command_result yet -- `collect_pending_handoffs` owes it once the close finishes.
            send_tabs(state, webview);
        }
        InboundMessage::SelectTab { .. } => {
            let tab = tab_of(target);
            state.borrow_mut().tabs.select(tab);
            send_switch(state, webview);
            ok(webview);
        }
        InboundMessage::RenameTab { name, .. } => {
            let tab = tab_of(target);
            state.borrow_mut().tabs.rename(tab, &name);
            send_tabs(state, webview);
            ok(webview);
        }
        InboundMessage::CloseTab { .. } => {
            let tab = tab_of(target);
            match close_tab(state, webview, tab) {
                Ok(()) => ok(webview),
                Err(why) => refuse(webview, why),
            }
        }
        InboundMessage::ResetTab { .. } => {
            let tab = tab_of(target);
            let before = state.borrow().tabs.attention();
            let reset = state.borrow_mut().tabs.reset(tab);
            match reset {
                Ok(old) => {
                    if let Some(old) = old {
                        state.borrow_mut().retiring.backend(old);
                    }
                    let payloads = {
                        let mut state_ref = state.borrow_mut();
                        if state_ref.tabs.active() == tab {
                            // `tabs` and the (empty) tab's own state.
                            switch_payloads(&mut state_ref)
                        } else {
                            vec![tabs_payload_recorded(&mut state_ref)]
                        }
                    };
                    dispatch_all(webview, payloads);
                    send_hello_if_open_sessions_changed(state, webview);
                    report_attention(state, before);
                    ok(webview);
                }
                Err(why) => refuse(webview, &why),
            }
        }
        // R07/S2, Task 2: `TabSet::cycle_mode` now returns a `Result<ModeCycle, String>` -- a live,
        // active tab can toggle too (D6), not only an empty one, so `Ok(Changed(_))` here no longer
        // means "was empty". `was_empty` is read BEFORE the call so the persisted "next launch"
        // default (ruling 5) is written only for the tab whose move that preference is actually
        // about. Task 3, D2: `Confirm(plan)` dispatches the real y/n prompt rather than refusing --
        // `apply_confirm_bypass`, below, is what a later `confirm_bypass` message answers it with.
        InboundMessage::CycleMode { .. } => {
            let tab = tab_of(target);
            let was_empty = matches!(
                state.borrow().tabs.get(tab).map(|t| &t.backend),
                Some(TabBackend::NotStarted)
            );
            // Bound as its own statement, not the match scrutinee: edition 2021 keeps a match
            // scrutinee's temporaries alive for the whole match, so `state.borrow_mut()` here would
            // otherwise still hold the `RefCell`'s mutable borrow while the `Changed` arm below calls
            // `send_tabs`, which itself needs `state.borrow_mut()` -- a reproducible panic ("already
            // borrowed") once a tab could actually reach `Changed` by leaving bypass.
            let outcome = state.borrow_mut().tabs.cycle_mode(tab);
            match outcome {
                Ok(eitri_core::tab_set::ModeCycle::Changed(mode)) => {
                    if was_empty {
                        let (prefs_dir, project_dir) = {
                            let state_ref = state.borrow();
                            (state_ref.prefs_dir.clone(), state_ref.project_dir.clone())
                        };
                        // Remembered for the next empty tab and the next launch (ruling 5). A
                        // failure to write is logged, not refused: the mode itself did change.
                        if let Some(dir) = prefs_dir {
                            if let Err(e) = eitri_core::agent_prefs::save_mode(&dir, &project_dir, mode) {
                                eprintln!("[agent_panel] could not remember the permission mode: {e}");
                            }
                        }
                    }
                    send_tabs(state, webview);
                    ok(webview);
                }
                Ok(eitri_core::tab_set::ModeCycle::Confirm(plan)) => {
                    evaluate_js_dispatch(
                        webview,
                        &eitri_core::agent_bridge::serialize_confirm_bypass_for_js(&plan),
                    );
                    ok(webview);
                }
                Err(why) => refuse(webview, &why),
            }
        }
        InboundMessage::OpenDetail { .. } => {
            let tab = tab_of(target);
            // A route away from a bypass prompt (spec §3.4; the panel cancels its own on `tab_detail`).
            state.borrow_mut().tabs.drop_bypass_prompt();
            let payload = detail_payload(&state.borrow(), tab);
            evaluate_js_dispatch(webview, &payload);
            ok(webview);
        }
        InboundMessage::QueueMessage { text, .. } => {
            let tab = tab_of(target);
            let queued = {
                let mut state_ref = state.borrow_mut();
                let state_ref = &mut *state_ref;
                // Captured now (ruling 1): what the editor shows when Enter is pressed.
                let wire = eitri_core::editor_context::compose_turn_text(&text, (state_ref.editor_context)().as_ref());
                state_ref.tabs.queue_message(tab, &text, wire, now_ms())
            };
            match queued {
                Ok(_) => {
                    remember_prompts(state, webview, std::slice::from_ref(&text));
                    // The turn may have ended between the panel's Enter and this handler (ruling 3b).
                    let flush = state.borrow_mut().tabs.flush_queue(tab);
                    match flush {
                        Some(flush) => apply_flush(state, webview, tab, flush),
                        None => send_queue(state, webview, tab),
                    }
                    ok(webview);
                }
                Err(why) => refuse(webview, &why),
            }
        }
        InboundMessage::TakeBackQueue { .. } => {
            let tab = tab_of(target);
            let texts = state.borrow_mut().tabs.take_back_queue(tab);
            evaluate_js_dispatch(
                webview,
                &eitri_core::agent_bridge::serialize_queue_taken_for_js(tab, &texts),
            );
            send_queue(state, webview, tab);
            ok(webview);
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
                remember_prompts(state, webview, std::slice::from_ref(&text));
            }
            match result {
                Err(why) => refuse(webview, &why),
                Ok(eitri_core::tab_set::SendNow::Interrupting(outcome)) => {
                    send_queue(state, webview, tab);
                    apply_command_outcome(state, webview, tab, &request_id, outcome);
                }
                Ok(eitri_core::tab_set::SendNow::Flushed(flush)) => {
                    if let Some(flush) = flush {
                        apply_flush(state, webview, tab, flush);
                    }
                    ok(webview);
                }
            }
        }
        InboundMessage::Draft { text, .. } => {
            // Ruling 6: a mirror, answered with nothing.
            state.borrow_mut().tabs.set_draft(tab_of(target), &text);
        }
        InboundMessage::HistoryPush { text, .. } => {
            remember_prompts(state, webview, std::slice::from_ref(&text));
            ok(webview);
        }
        InboundMessage::OpenPath { path, line, .. } => {
            let project_dir = state.borrow().project_dir.clone();
            let candidate = std::path::Path::new(&path);
            let resolved = if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                project_dir.join(candidate)
            };
            if !resolved.exists() {
                refuse(webview, &format!("no such file: {path}"));
                return;
            }
            let request = eitri_core::scratch::open_request(&resolved, line);
            match request_editor(state, &request) {
                Ok(()) => ok(webview),
                Err(why) => refuse(webview, &why),
            }
        }
        // `c` in BROWSE: the jobs are built here, from this tab's own session and edit calls, and run
        // on a worker; the answer is an envelope carrying the request id, sent by the tick.
        InboundMessage::ReviewRequest { turn, scope, .. } => {
            let tab = tab_of(target);
            let job = state.borrow().tabs.review_overview_job(tab, turn.into(), scope.into());
            let started = job.and_then(|job| {
                let id = request_id.clone();
                start_review_job(state, &request_id, move || {
                    job.run()
                        .map(|overview| eitri_core::agent_bridge::serialize_review_for_js(&id, tab, &overview))
                        .map_err(|why| why.to_string())
                })
            });
            if let Err(why) = started {
                refuse(webview, &why);
            }
        }
        InboundMessage::ReviewDiffRequest { turn, scope, path, .. } => {
            let tab = tab_of(target);
            let job = state.borrow().tabs.review_diff_job(tab, turn, scope.into(), &path);
            let started = job.and_then(|job| {
                let id = request_id.clone();
                start_review_job(state, &request_id, move || {
                    job.run()
                        .map(|diff| eitri_core::agent_bridge::serialize_review_diff_for_js(&id, tab, &diff))
                        .map_err(|why| why.to_string())
                })
            });
            if let Err(why) = started {
                refuse(webview, &why);
            }
        }
        InboundMessage::ViewInEditor { title, text, .. } => {
            let prepared = match state.borrow_mut().scratch.as_mut() {
                Some(dir) => dir
                    .prepare_view(&title, &text)
                    .map_err(|e| format!("could not write the scratch file: {e}")),
                None => Err("the scratch directory could not be created at startup".to_string()),
            };
            match prepared.and_then(|request| request_editor(state, &request)) {
                Ok(()) => ok(webview),
                Err(why) => refuse(webview, &why),
            }
        }
        InboundMessage::EditDraft { text, .. } => {
            let tab = tab_of(target);
            let prepared = match state.borrow_mut().scratch.as_mut() {
                Some(dir) => dir
                    .prepare_edit(&text)
                    .map_err(|e| format!("could not write the scratch file: {e}")),
                None => Err("the scratch directory could not be created at startup".to_string()),
            };
            let (request, edit) = match prepared {
                Ok(pair) => pair,
                Err(why) => return refuse(webview, &why),
            };
            if let Err(why) = state.borrow_mut().tabs.begin_scratch_edit(tab, edit.id) {
                edit.cleanup();
                return refuse(webview, &why);
            }
            if let Err(why) = request_editor(state, &request) {
                let mut state_ref = state.borrow_mut();
                state_ref
                    .tabs
                    .finish_scratch_edit(edit.id, &eitri_core::scratch::EditDone::Discarded);
                edit.cleanup();
                drop(state_ref);
                return refuse(webview, &why);
            }
            state.borrow_mut().tabs.set_draft(tab, &text);
            state.borrow_mut().pending_edits.push(edit);
            evaluate_js_dispatch(webview, &eitri_core::agent_bridge::serialize_scratch_for_js(tab, true));
            ok(webview);
        }
        // `<leader>bd` (wave 4, R1): quiet unless `close_needs_confirm`; `prefix &` never comes through here.
        InboundMessage::TabVerb {
            verb: TabVerbWire::Close,
            ..
        } if !{
            let state_ref = state.borrow();
            state_ref.tabs.close_needs_confirm(state_ref.tabs.active())
        } =>
        {
            let active = state.borrow().tabs.active();
            match close_tab(state, webview, active) {
                Ok(()) => ok(webview),
                Err(why) => refuse(webview, why),
            }
        }
        InboundMessage::TabVerb { verb, .. } => match run_tab_verb(state, verb) {
            Ok(()) => ok(webview),
            Err(why) => refuse(webview, why),
        },
        InboundMessage::CycleDefaultMode { .. } => {
            // `Shift+Tab` on the chooser's `New session` row or a record (spec §6.3): the window's
            // remembered default, exactly as `CycleMode` remembers a tab's own mode above. `Confirm`
            // dispatches the same y/n prompt (D2), answered by a later `confirm_bypass` message.
            match cycle_default_mode(state) {
                eitri_core::tab_set::ModeCycle::Changed(_) => {
                    send_tabs(state, webview);
                    ok(webview);
                }
                eitri_core::tab_set::ModeCycle::Confirm(plan) => {
                    evaluate_js_dispatch(
                        webview,
                        &eitri_core::agent_bridge::serialize_confirm_bypass_for_js(&plan),
                    );
                    ok(webview);
                }
            }
        }
        InboundMessage::CloseOthers { .. } => {
            // `y` to `confirm_close_others` (Owner answers Q2): recomputed fresh rather than
            // trusting the set the prompt was shown with, then `close_tab` -- the same close path
            // `prefix &`'s own `y` runs -- once per tab.
            let ids = state
                .borrow()
                .tabs
                .close_others_plan()
                .map_or_else(Vec::new, |(ids, _)| ids);
            let mut first_err = None;
            for id in ids {
                if let Err(why) = close_tab(state, webview, id) {
                    first_err.get_or_insert(why);
                }
            }
            match first_err {
                None => ok(webview),
                Some(why) => refuse(webview, why),
            }
        }
        // C1's mirror (spec §3.5): recorded so `main.rs`'s intercept can read it back through
        // `AgentPanelHandle::panel_keys`. Never refused -- an unrecognised `mode` is a parse
        // failure in `parse_inbound_message`, not something that reaches here.
        InboundMessage::PanelKeys { mode, .. } => {
            state.borrow().nav_mode.set(mode);
            ok(webview);
        }
        InboundMessage::NavFallthrough { direction, .. } => match run_nav_fallthrough(state, direction) {
            Ok(()) => ok(webview),
            Err(why) => refuse(webview, why),
        },
        // v1 picks, Task 6 (R11): `Ctrl+w h/j/k/l` in the panel's BROWSE -- the move `Ctrl+h/j/k/l` make.
        InboundMessage::PaneNav { direction, .. } => match run_pane_nav(state, direction) {
            Ok(()) => ok(webview),
            Err(why) => refuse(webview, why),
        },
        // v1 picks, Task 8 (R6): `gx` -- a web link out to the system browser. The page's own check
        // (`nav.ts#webUrl`) is not trusted with what reaches the launcher: `web_url` decides again, and a
        // refusal means a page/Rust mismatch or a page that should not be sending this, so it is logged
        // (the address through `{:?}`, which escapes anything a terminal would act on).
        InboundMessage::OpenUrl { url, .. } => match web_url(&url) {
            Some(url) => {
                let shown = url.to_string();
                gtk4::UriLauncher::new(url).launch(
                    None::<&gtk4::Window>,
                    None::<&gtk4::gio::Cancellable>,
                    move |result| {
                        if let Err(error) = result {
                            eprintln!("[agent_panel] open_url {shown}: {error}");
                        }
                    },
                );
                ok(webview);
            }
            None => {
                eprintln!("[agent_panel] open_url refused: {url:?}");
                refuse(webview, "not a web link");
            }
        },
        // R07/S2, Task 3: `y`/`Y` to the band's bypass prompt. D11's typed-input guard (the 250ms
        // `TYPING_GUARD_MS` rule, cancelling on every route away) is enforced client-side (spec
        // §3.4) -- this only re-validates the facts §3.3 names (the nonce, the active tab, exactly
        // the delivered cards still pending) via `apply_confirm_bypass`/`TabSet::confirm_bypass`,
        // which is where all of that is actually checked.
        InboundMessage::ConfirmBypass { tab, scope, nonce, .. } => match scope.scope(tab) {
            Err(why) => refuse(webview, &why),
            Ok(scope) => {
                let before = state.borrow().tabs.attention();
                match apply_confirm_bypass(state, scope, nonce) {
                    Ok(eitri_core::tab_set::ConfirmOutcome::Entered { resolved, .. }) => {
                        // Codex v1-mode finding 1: on legacy the approved cards' resolutions exist
                        // only here (its CLI reports nothing for a hook reply), so without this the
                        // cards stayed drawn and counted as waiting until the next snapshot. A tab
                        // scope is always the active tab (`confirm_bypass` refuses any other); the
                        // window-default scope approves nothing, so `resolved` is empty there.
                        let payload = {
                            let state_ref = state.borrow();
                            let active = state_ref.tabs.active();
                            returned_events_payload(&state_ref, active, &resolved)
                        };
                        if let Some(payload) = payload {
                            evaluate_js_dispatch(webview, &payload);
                        }
                        send_tabs(state, webview);
                        // The cards `confirm_bypass` just approved leave the tray at once, rather
                        // than waiting for a later poll to notice `attention()` dropped.
                        report_attention(state, before);
                        ok(webview);
                    }
                    Ok(eitri_core::tab_set::ConfirmOutcome::AlreadyBypass) => {
                        send_tabs(state, webview);
                        ok(webview);
                    }
                    Ok(eitri_core::tab_set::ConfirmOutcome::Reprompt(plan)) => {
                        // D7: a card arrived while the prompt was up -- nothing changed, and the
                        // panel is shown the fresh count under a fresh nonce instead.
                        evaluate_js_dispatch(
                            webview,
                            &eitri_core::agent_bridge::serialize_confirm_bypass_for_js(&plan),
                        );
                        ok(webview);
                    }
                    Err(why) => refuse(webview, &why),
                }
            }
        },
    }
}

/// `cycle_default_mode`: `TabSet::cycle_default_mode` plus remembering the new default on
/// `Changed(Auto)` only -- D3: bypass is never written, and a `Confirm` is not persisted here
/// either (only actually entering bypass, via `apply_confirm_bypass` below, ever could -- and D3
/// forbids that too). A failure to write is logged, not refused, since the mode itself did change.
/// Never touches the WebView, so it is tested directly; the arm above owns `send_tabs` and
/// dispatching the prompt.
fn cycle_default_mode(state: &Rc<RefCell<AgentPanelState>>) -> eitri_core::tab_set::ModeCycle {
    let outcome = state.borrow_mut().tabs.cycle_default_mode();
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
/// gets above. `TabSet::confirm_bypass` does all of the checking (D7's re-check against exactly the
/// delivered cards, the nonce, the active tab, ended/failed) and D3 forbids ever writing bypass to
/// disk, so there is nothing else for this wrapper to do -- it exists so a test can drive the exact
/// call the arm makes without a WebView, the same way `cycle_default_mode` can.
fn apply_confirm_bypass(
    state: &Rc<RefCell<AgentPanelState>>,
    scope: eitri_core::tab_set::BypassScope,
    nonce: u64,
) -> Result<eitri_core::tab_set::ConfirmOutcome, String> {
    state.borrow_mut().tabs.confirm_bypass(scope, nonce)
}

/// `tab_verb`: the hook `main.rs` installs via `on_tab_verb` (`run_tab_action`, panel round 2 plan
/// Task 6), mapped from the wire shape and called -- or a refusal naming why there is none yet (a
/// legacy backend at startup, before `main.rs` has wired it). Never touches the WebView, so it is
/// tested directly.
fn run_tab_verb(state: &Rc<RefCell<AgentPanelState>>, verb: TabVerbWire) -> Result<(), &'static str> {
    // Cloned out in its own statement: a `state.borrow()` in the `match` scrutinee would live to
    // the end of the `match` (edition 2021), and the hook -- `main.rs`'s `run_tab_action` -- goes
    // back through this same state with `borrow_mut` (`switch_with`, `new_tab`), which panics.
    let hook = state.borrow().tab_verb_hook.clone();
    match hook {
        Some(hook) => {
            hook(tab_action_for(verb));
            Ok(())
        }
        None => Err("tab verbs are not wired"),
    }
}

/// `nav_fallthrough`: the hook `main.rs` installs via `on_nav_fallthrough` (C1, spec §3.5), called
/// with the direction the page could not apply -- or a refusal naming why there is none yet. Never
/// touches the WebView, so it is tested directly, the same shape as `run_tab_verb`.
fn run_nav_fallthrough(state: &Rc<RefCell<AgentPanelState>>, direction: NavKeyDirection) -> Result<(), &'static str> {
    let hook = state.borrow().nav_fallthrough_hook.clone();
    match hook {
        Some(hook) => {
            let direction = match direction {
                NavKeyDirection::Down => Direction::Down,
                NavKeyDirection::Up => Direction::Up,
            };
            hook(direction);
            Ok(())
        }
        None => Err("nav fallthrough is not wired"),
    }
}

/// `pane_nav` (R11): the same hook `nav_fallthrough` uses -- `main.rs` installs it as
/// `move_focus(&ModuleId::agent(), direction)`, exactly what `Ctrl+h/j/k/l` do from the panel.
fn run_pane_nav(state: &Rc<RefCell<AgentPanelState>>, direction: PaneNavDirection) -> Result<(), &'static str> {
    let hook = state.borrow().nav_fallthrough_hook.clone();
    let hook = hook.ok_or("pane nav is not wired")?;
    hook(match direction {
        PaneNavDirection::Left => Direction::Left,
        PaneNavDirection::Down => Direction::Down,
        PaneNavDirection::Up => Direction::Up,
        PaneNavDirection::Right => Direction::Right,
    });
    Ok(())
}

/// `open_url` (R6): what a panel key may hand `UriLauncher`. The page sends a WHATWG-normalized
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

/// `tab_verb`'s wire shape to the `TabAction` the prefix's own `Action::Tab` arm runs (spec §10.2).
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

/// `y` to `prefix &` (spec §3.5): `tabs::close_steps`, in order. The record is never deleted.
///
/// **Refused while the tab is handing off** (the session-tabs whole-branch review): the close takes
/// at most the sidecar's 10 s `close_session`, and the only thing it produces is the command the
/// user asked to be shown. Closing the tab under it would drop the command and the
/// `command_result` the panel is still waiting on.
///
/// A tab closed while it is still connecting has its `send_message`/`resume` answered with an
/// error here, so the panel's in-flight record is closed and a first message comes back with its
/// text (ruling 3); its connect is kept in `Retiring` and shut down when it finishes.
fn close_tab(state: &Rc<RefCell<AgentPanelState>>, webview: &WebView, tab: TabId) -> Result<(), &'static str> {
    let before = state.borrow().tabs.attention();
    let Some(facts) = state.borrow().tabs.close_facts(tab) else {
        return Ok(());
    };
    if state
        .borrow()
        .tabs
        .get(tab)
        .is_some_and(|t| t.pending_handoff.is_some())
    {
        return Err("this tab is still being handed off to a terminal; close it once that finishes");
    }
    let mut owed = Vec::new();
    for step in eitri_core::tabs::close_steps(&facts) {
        match step {
            eitri_core::tabs::CloseStep::Interrupt => {
                let interrupted = backend_for(&mut state.borrow_mut().tabs, tab).and_then(|b| b.interrupt());
                if let Err(e) = interrupted {
                    // The close goes on: the shutdown below ends the turn anyway.
                    eprintln!(
                        "[agent_panel] tab {}: interrupt before close failed: {}",
                        tab.0, e.message
                    );
                }
            }
            // Ruling 10: the queued words outlive the tab as history.
            eitri_core::tabs::CloseStep::QueueToHistory => {
                let texts = state.borrow().tabs.queue_texts(tab);
                remember_prompts(state, webview, &texts);
            }
            // Folded into `Remove`: `Retiring::tab` shuts the removed tab's backend down.
            eitri_core::tabs::CloseStep::Shutdown => {}
            eitri_core::tabs::CloseStep::Remove => {
                let mut state_ref = state.borrow_mut();
                let state_ref = &mut *state_ref;
                if let Some(removed) = state_ref.tabs.remove(tab) {
                    owed = state_ref.retiring.tab(removed);
                }
            }
        }
    }
    let payloads = switch_payloads(&mut state.borrow_mut());
    dispatch_all(webview, payloads);
    // After the switch, not before: the panel restores a refused send into the composer only while
    // its tab is the active one, and the switch that follows would then replace it with the next
    // tab's draft. Answered once the closed tab is no longer active, the refusal is reported with
    // the message's full text instead (ruling 3), and the switch cannot clear that notice.
    for request_id in owed {
        evaluate_js_dispatch(
            webview,
            &serialize_command_result_for_js(&request_id, Err(TAB_CLOSED_MESSAGE)),
        );
    }
    send_hello_if_open_sessions_changed(state, webview);
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

/// The panel document with `vars` already inlined, so the first frame WebKit paints -- on a cold
/// start or after `reload_document` -- is in nvim's colours instead of an unstyled white page.
/// `applyTheme` later sets the same variables inline on `:root`, which overrides this block for
/// live colorscheme changes.
///
/// Inserted directly after the opening `<head>`, not before `</head>`: the single-file build
/// inlines its script into `<head>`, and that script contains the literal `<head></head>`
/// (DOMPurify), so the first `</head>` in the file is not the document's.
///
/// The CSP `<meta>` goes in first, ahead of even the theme `<style>` -- a `<meta
/// http-equiv="Content-Security-Policy">` only governs what loads *after* it in the document, so it
/// must be the very first thing `<head>` contains, before the single-file build's own inlined
/// `<script>`/`<style>`. See `PANEL_CONTENT_SECURITY_POLICY`.
fn themed_document(vars: &[(String, String)]) -> String {
    let declarations: String = vars.iter().map(|(name, value)| format!("{name}:{value};")).collect();
    let style = format!("<style id=\"nv-theme\">:root{{{declarations}}}</style>");
    let csp_meta = format!("<meta http-equiv=\"Content-Security-Policy\" content=\"{PANEL_CONTENT_SECURITY_POLICY}\">");
    match AGENT_UI_HTML.find("<head>") {
        Some(at) => {
            let insert = at + "<head>".len();
            format!(
                "{}{}{}{}",
                &AGENT_UI_HTML[..insert],
                csp_meta,
                style,
                &AGENT_UI_HTML[insert..]
            )
        }
        None => {
            eprintln!("[agent_panel] the panel document has no <head>; loading it without an inline theme");
            AGENT_UI_HTML.to_string()
        }
    }
}

/// The WebView's own background, which shows before the web process has painted anything. WebKit
/// defaults it to opaque white.
fn paint_webview_background(webview: &WebView, tokens: &eitri_core::theme::ThemeTokens) {
    let bg = tokens.bg;
    webview.set_background_color(&gtk4::gdk::RGBA::new(
        f32::from(bg.r) / 255.0,
        f32::from(bg.g) / 255.0,
        f32::from(bg.b) / 255.0,
        1.0,
    ));
}

/// The panel document's base URI. A secure context, so `navigator.clipboard` exists and every `y`
/// reaches the clipboard -- with no base the document's origin is opaque and it does not (GUI pass,
/// 2026-09-25). `.invalid` never resolves (RFC 2606): nothing is ever fetched from it, and a
/// relative link is still a `LinkClicked` that `connect_decide_policy` hands to the browser.
const PANEL_BASE_URI: &str = "https://eitri.invalid/";

/// Ruling R1 (rendered model content is untrusted) plus finding 3 of
/// `docs/superpowers/reviews/2026-09-27-v1-hardening/codex-sec-panel-content-verdicts.md`: a CSP
/// floor the page cannot loosen, applied two ways -- as the `WebView`'s own
/// `default-content-security-policy` (`build_agent_panel`) and as the first element of `<head>`
/// (`themed_document`), so it is in force before the single-file build's own inlined `<script>`/
/// `<style>` run. This is defense in depth behind Task 1's sanitizer, for whatever gets past it.
///
/// Checked what the panel legitimately loads before picking each directive: `agent-ui/web/src` has
/// no `fetch`/`XMLHttpRequest`/`WebSocket`/`eval`/`new Function`, and `index.css` has no
/// `@font-face`/`url(` -- every font is a locally-installed one named by `font-family`, resolved by
/// WebKit's own font matching, never loaded as a resource. So `connect-src 'none'` and `media-src
/// 'none'` cost the panel nothing today. `img-src data:`/`font-src data:` stay open only for a
/// future inline `data:` use, never a remote load. `script-src`/`style-src 'unsafe-inline'` are what
/// the single-file build's inlined `<script>`/`<style>` need (`vite-plugin-singlefile` leaves no
/// hash or nonce to pin instead). `frame-src`/`object-src 'none'` and `form-action`/`base-uri 'none'`
/// match `connect_decide_policy`'s existing navigation guard just above, which already refuses any
/// `FormSubmitted`/`LinkClicked` navigation of this WebView itself.
///
/// `evaluate_javascript` from Rust (`evaluate_js_dispatch`) bypasses CSP by design -- it is this
/// panel's own push channel, not page-originated content -- so the dispatch path this panel relies
/// on is unaffected.
const PANEL_CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; font-src data:; media-src 'none'; connect-src 'none'; frame-src 'none'; object-src 'none'; form-action 'none'; base-uri 'none'";

/// Finding 3's backstop, the same technique the (unbuilt) canvas design names for its own sandbox
/// (`docs/superpowers/specs/2026-09-23-modules-and-canvas-design.md` §9.1): an ephemeral
/// `NetworkSession` (per `WebKitWebsiteDataManager:is-ephemeral`, all data -- cookies, cache, storage
/// -- is held in memory for the session's lifetime and never written to disk; it is non-persistent,
/// not absent) whose proxy is the "discard" port on loopback, so any request that gets past the CSP
/// above and Task 1's sanitizer dies at a closed port. **This is a backstop, not the boundary** --
/// the CSP is.
const PANEL_NETWORK_PROXY_URI: &str = "http://127.0.0.1:9";

/// Sends one envelope to the page, never delayed -- and never ahead of a stream envelope the typing
/// cadence is still holding (`crate::panel_pacer`): whatever waits goes first, so the page sees
/// things in the order they happened. The only way this module puts an envelope in the page.
fn evaluate_js_dispatch(webview: &WebView, json_payload: &str) {
    crate::panel_pacer::send_immediate(webview, json_payload);
}

/// Installs `app.reload-agent-panel`, which the top bar's `↻` and `prefix r` (keymap spec §2.1)
/// fire, for the panel `handle` owns.
///
/// The affordance deliberately lives OUTSIDE the WebView. A reload button drawn by the panel's own
/// page would be drawn by the very thing that has stopped responding -- missing exactly when it is
/// needed -- and until now a wedged panel could only be recovered by closing the window, which also
/// takes the editor, the nvim child and the agent session with it.
///
/// It binds no accelerator: `Ctrl+Shift+R` was removed with every other `Ctrl+Shift` chord (keymap
/// spec §2.2).
///
/// **Not verified, and stated here so nobody reads a constraint into the call site:** `main.rs`
/// calls this before building the top bar, whose `⟳` button is a `GtkActionable` pointed at this
/// action. An earlier version of this comment claimed the order was load-bearing -- that a button
/// pointed at an action that does not exist yet renders permanently insensitive. That claim was
/// never tested here, and upstream GTK4's own `gtk/gtkactionhelper.c` appears to contradict it:
/// `GtkActionHelper` implements `GtkActionObserver` (`action_added`, `action_removed`,
/// `action_enabled_changed`) and calls `gtk_widget_set_sensitive` when an action appears. That is
/// read from the GNOME repository's `main`, not from the GTK actually installed here, and it is not
/// the same thing as having watched a late-registered action light the button up. So the ordering
/// is most likely free, on documentary evidence rather than observation. It is kept as it is because it is the conservative one, not
/// because anything in this repository has demonstrated that the other order breaks. Whether the
/// button is in fact sensitive on screen is a GUI-pass item -- see
/// `shell/MANUAL_VERIFICATION.md`'s 2026-09-15 section.
///
/// **Known limitation while `shell` can still run two windows in one process:** the action is
/// process-global (`app.add_action`) and this handle is per-window, so a second `activate` running
/// `build_ui` again silently REPLACES the first window's action -- after which window 1's `⟳` and
/// Ctrl+Shift+R reload window 2's panel. `ApplicationFlags::NON_UNIQUE` (task A1 of the same plan)
/// makes that unreachable by giving each launch its own process; until it lands this is real, and
/// it is the same class of bug A1 exists to close rather than a new one introduced here.
pub(crate) fn install_reload_action(app: &Application, handle: &AgentPanelHandle) {
    let action = gtk4::gio::SimpleAction::new("reload-agent-panel", None);
    // Cloned because the caller still needs its own handle afterwards -- `connect_close_request`
    // takes it by move.
    let handle = handle.clone();
    action.connect_activate(move |_, _| handle.reload_document_by_hand());
    app.add_action(&action);
}

#[cfg(test)]
mod tests {
    use super::*;

    use eitri_core::agent_bridge::SessionModeChoice;
    use eitri_core::tab_set::{TabBackend, TabSet};
    use eitri_core::tabs::TabId;
    use eitri_core::test_providers::RecordingProvider;

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

    /// Added by the local review of item 6 of the 2026-09-27 "v1 polish" task (dated record, owner
    /// decision (b)): a `reason` naming a stopped sidecar must not carry the "most likely no longer
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

    /// GUI pass, 2026-09-25: every `y` in the panel (a row, a code block, the handoff command, the
    /// `prefix i` popover's line) calls `navigator.clipboard.writeText`, and none of them ever
    /// reached the clipboard. `load_html(.., None)` gives the document an opaque origin, which is not
    /// a secure context, so WebKitGTK 2.52.6 has no `navigator.clipboard` and the optional chain
    /// did nothing. Measured in the sandbox with a bare WebKitGTK view: base `None`, nothing copied;
    /// base `https://eitri.invalid/`, `wl-paste` printed the text. Every `load_html` here must
    /// pass the secure base, and the base must stay one that never resolves (RFC 2606 `.invalid`):
    /// a relative link is still a `LinkClicked` the navigation guard hands to the browser.
    #[test]
    fn the_panel_document_is_loaded_in_a_secure_context_that_never_resolves() {
        assert!(PANEL_BASE_URI.starts_with("https://"), "{PANEL_BASE_URI}");
        let host = PANEL_BASE_URI.trim_start_matches("https://").trim_end_matches('/');
        assert!(host.ends_with(".invalid"), "{host}");
        let source = include_str!("agent_panel.rs");
        let calls: Vec<&str> = source
            // Every product call passes a borrowed document (`&themed_document(..)`); this test's own
            // needle does not, which is what keeps it out of the list.
            .match_indices(concat!(".load_html", "(&"))
            .map(|(i, _)| {
                let rest = &source[i..];
                &rest[..rest.find(';').unwrap_or(rest.len())]
            })
            .collect();
        assert!(calls.len() >= 2, "{calls:?}");
        for call in calls {
            assert!(
                call.contains("Some(PANEL_BASE_URI)"),
                "a load_html without the secure base: {call}"
            );
        }
    }

    /// Ruling W6: `SESSION_CLOSE_BACKSTOP` must cover both the `close_session` unary RPC and the
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
        let conversation = agent::AgentConversation::create(provider.clone(), dir).unwrap();
        (provider, AgentBackend::Sidecar(Box::new(conversation)))
    }

    /// Review focus 1 (spec §3.8 point 2): a permission answered after a switch reaches the tab
    /// that asked, not the one on screen.
    #[test]
    fn a_permission_answer_names_its_own_tab_not_the_active_one() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-permission-tab");
        let mut set = TabSet::new(BackendKind::Sidecar, eitri_core::agent_bridge::SessionModeChoice::Auto);
        let asking = set.active();
        let (asking_provider, backend) = live_backend(&dir);
        set.get_mut(asking).unwrap().backend = TabBackend::Live(backend);
        let other = set.open();
        let (other_provider, backend) = live_backend(&dir);
        set.get_mut(other).unwrap().backend = TabBackend::Live(backend);
        assert_eq!(set.active(), other, "the user has switched away");
        // `perm-1` must really be pending on the asking tab: a conversation refuses an answer to a
        // permission it never asked for. A `Write` needs a human, so the pump leaves it a card --
        // `.git/main.rs` rather than plain `main.rs` since fix round 1 (2026-09-28, v1 trial item
        // 4A): an ordinary in-project `Write` no longer cards at all under the acceptEdits fast path.
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
        let outcome = answer_permission_response(&mut set, None, &dir, tab, PermissionAnswer::of(message).unwrap());
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

    /// O3 review #4: the panel's own `permission_response` route records the human's approval, so the
    /// CLI's own prompt for the same call that follows it is answered without a second card (ruling
    /// 5). Were the route to answer the backend directly, this would be a second card.
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
        let outcome = answer_permission_response(&mut set, None, &dir, tab, PermissionAnswer::of(message).unwrap());
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

    /// The whole-branch review: a tab closed while the window stays open is kept in panel state
    /// until its teardown or connect finishes, so the window-close backstop can see it; and the
    /// command a connecting tab still owed is handed back to be answered.
    #[test]
    fn a_closed_tabs_teardown_and_connect_are_kept_until_they_finish() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-retiring");
        let mut set = TabSet::new(BackendKind::Sidecar, eitri_core::agent_bridge::SessionModeChoice::Auto);
        let live = set.active();
        set.get_mut(live).unwrap().backend = TabBackend::Live(live_backend(&dir).1);
        let connecting = set.open();
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

    /// A minimal `AgentPanelState` for tests that only care about window-level hooks (`tab_verb`)
    /// and never touch the WebView, which lives on `AgentPanelHandle` and not on this struct --
    /// so no display is required to run them.
    fn state_for_hooks(tabs: TabSet) -> Rc<RefCell<AgentPanelState>> {
        Rc::new(RefCell::new(AgentPanelState {
            tabs,
            editor_context: Rc::new(|| None),
            backend_kind: BackendKind::Sidecar,
            project_dir: PathBuf::new(),
            canonical_project_dir: String::new(),
            prefs_dir: None,
            supervisor: None,
            supervisor_pending: None,
            shutting_down: false,
            theme: eitri_core::theme::ThemeTokens::fallback(),
            keymap_help: None,
            pane_focused: false,
            hint_hook: None,
            attention_hook: None,
            last_tabs_payload: None,
            last_open_ids: Vec::new(),
            scratch: None,
            pending_edits: Vec::new(),
            review_jobs: Vec::new(),
            review_hints: Vec::new(),
            editor_request_hook: None,
            editor_done_hook: None,
            retiring: Retiring::default(),
            history_dir: None,
            rules_dir: None,
            history: Vec::new(),
            document_ready: false,
            last_context_payload: None,
            editor_link: None,
            tab_verb_hook: None,
            nav_mode: Cell::new(PanelKeys::Other),
            nav_fallthrough_hook: None,
            crash_guard: crate::webview_crash_guard::WebViewCrashGuard::with_defaults(),
            tab_memory: TabMemory::new(None, PathBuf::new()),
            conversation_id: String::new(),
            restore_source: None,
            restore_policy: RestorePolicy::Offer,
            bypass_by_config: false,
            mode_remembered: false,
            restore_run: None,
            last_restore_offered: false,
            toast_hook: None,
        }))
    }

    /// v1-dist sub-plan 2026-09-28-v1-dist-ubuntu-userns: with no page (WebKit's sandbox cannot
    /// start, so no `WebView` exists), every call `main.rs`'s focus, HINT, prefix, tab-verb, theme,
    /// zoom and close paths make still runs -- none panics, none needs a display -- and nothing that
    /// only a page could act on happens: no tab is opened or switched to (the verbs refuse, so the
    /// prefix flashes), `panel_keys` never leaves `Other` (so `Ctrl+j`/`Ctrl+k` are never claimed for
    /// a composer that does not exist and always move focus), and the chat owes no attention.
    #[test]
    fn a_panel_with_no_page_answers_every_window_call_and_does_nothing_a_page_would() {
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        let handle = AgentPanelHandle {
            state: state.clone(),
            webview: None,
        };
        let tabs_before: Vec<TabId> = state.borrow().tabs.tabs().iter().map(|t| t.id).collect();

        // Focus and arrival (`pane_focus`, `move_focus`, `arrive`), the prefix's panel keys.
        handle.set_pane_focused(true);
        handle.arrive();
        handle.enter_input();
        handle.nav_key(NavKeyDirection::Down);
        handle.literal_key(&eitri_core::keymap::KeySpec::parse("C-b").unwrap());
        handle.open_keymap();
        handle.open_command_line();
        handle.set_keymap_help("{}".to_string());
        assert!(state.borrow().pane_focused);
        assert_eq!(handle.panel_keys(), PanelKeys::Other);
        handle.set_pane_focused(false);

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
        handle.reload_document();
        handle.reload_document_by_hand();
        handle.on_web_process_terminated(webkit6::WebProcessTerminationReason::Crashed);

        // The tab verbs refuse: nothing a user could not see is opened, closed or switched to.
        handle.new_tab();
        assert!(!handle.step(1));
        assert!(!handle.select_last());
        assert!(!handle.select_number(1));
        handle.begin_rename();
        handle.confirm_close();
        handle.confirm_close_others();
        handle.open_chooser();
        handle.open_detail();
        assert!(!handle.focus_oldest_card());
        assert_eq!(handle.close_every_tab(), Ok(0));
        let tabs_after: Vec<TabId> = state.borrow().tabs.tabs().iter().map(|t| t.id).collect();
        assert_eq!(tabs_after, tabs_before);

        // What the tray, the toast and the window-close prompt ask.
        assert_eq!(handle.attention(), Default::default());
        assert_eq!(handle.newest_card_label(), None);
        assert_eq!(handle.running_count(), 0);
        assert_eq!(handle.queued_count(), 0);
        assert!(!handle.poll_activate());
    }

    /// Panel round 2 plan Task 6: `tab_verb` refuses naming why until `main.rs` installs a hook
    /// (`AgentPanelHandle::on_tab_verb`), and once one is installed it is called with the mapped
    /// `TabAction` -- every wire value, not just one, since a mapping this exhaustive is easy to
    /// get wrong silently for the one case nobody wrote a test for.
    #[test]
    fn tab_verb_refuses_with_no_hook_and_calls_the_installed_hook_with_the_mapped_action() {
        let state = state_for_hooks(TabSet::new(
            BackendKind::Sidecar,
            eitri_core::agent_bridge::SessionModeChoice::Auto,
        ));
        assert_eq!(run_tab_verb(&state, TabVerbWire::Close), Err("tab verbs are not wired"));

        let seen: Rc<RefCell<Vec<TabAction>>> = Rc::new(RefCell::new(Vec::new()));
        {
            let seen = seen.clone();
            state.borrow_mut().tab_verb_hook = Some(Rc::new(move |action| seen.borrow_mut().push(action)));
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
            assert_eq!(run_tab_verb(&state, verb), Ok(()));
            assert_eq!(seen.borrow_mut().pop(), Some(action), "{verb:?} -> {action:?}");
        }
    }

    /// The whole-branch review: the product hook (`main.rs`'s `run_tab_action`) re-enters this
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
            state.borrow_mut().tab_verb_hook = Some(Rc::new(move |_action| {
                let state = weak.upgrade().expect("state alive");
                let _guard = state.borrow_mut();
                *ran.borrow_mut() += 1;
            }));
        }
        assert_eq!(run_tab_verb(&state, TabVerbWire::Next), Ok(()));
        assert_eq!(*ran.borrow(), 1);
    }

    /// C1's mirror (spec §3.5): `handle_inbound_message`'s `PanelKeys` arm is what
    /// `AgentPanelHandle::panel_keys` reads back, and it starts `Other`.
    #[test]
    fn a_panel_keys_message_updates_the_mirror_handle_inbound_message_records() {
        let state = state_for_hooks(TabSet::new(
            BackendKind::Sidecar,
            eitri_core::agent_bridge::SessionModeChoice::Auto,
        ));
        assert_eq!(state.borrow().nav_mode.get(), PanelKeys::Other);
        for mode in [PanelKeys::Browse, PanelKeys::Input, PanelKeys::Other] {
            state.borrow().nav_mode.set(mode);
            assert_eq!(state.borrow().nav_mode.get(), mode);
        }
    }

    /// C1's mirror (spec §3.5): "it starts other and returns to other on every ready (a reload)".
    /// A round trip through `handle_inbound_message`'s `Ready` arm needs a real `WebView`
    /// (`dispatch_all`/`ok` both take one), so this pins the one-line mutation the arm calls,
    /// `reset_nav_mode_for_ready`, directly -- the same shape as
    /// `nav_fallthrough_refuses_with_no_hook_and_calls_the_installed_hook_with_the_mapped_direction`
    /// pinning `run_nav_fallthrough` apart from the WebView it is also called from.
    ///
    /// Before this fix, nothing reset `nav_mode` on `ready`: a reload while `mode == Input` left
    /// the mirror at `Input` even though the fresh document starts in BROWSE, so `main.rs`'s
    /// `claims(PanelKeys::Input, Direction::Down)` was `false` and a `Ctrl+j` meant to open the
    /// composer fell through to `move_focus` instead.
    #[test]
    fn ready_resets_the_nav_mode_mirror_to_other() {
        let state = state_for_hooks(TabSet::new(
            BackendKind::Sidecar,
            eitri_core::agent_bridge::SessionModeChoice::Auto,
        ));
        for stale in [PanelKeys::Input, PanelKeys::Browse] {
            state.borrow().nav_mode.set(stale);
            reset_nav_mode_for_ready(&state.borrow());
            assert_eq!(
                state.borrow().nav_mode.get(),
                PanelKeys::Other,
                "stale {stale:?} must clear"
            );
        }
    }

    /// The whole-branch review (R2-11, second half): a crashed or replaced page's BROWSE/INPUT is
    /// forgotten at once, not at a `ready` the give-up document never sends. `on_web_process_terminated`
    /// and `reload_document` need a real `WebView`, so this pins the call they both make.
    #[test]
    fn a_dead_pages_nav_mode_is_forgotten_before_any_ready() {
        let state = state_for_hooks(TabSet::new(
            BackendKind::Sidecar,
            eitri_core::agent_bridge::SessionModeChoice::Auto,
        ));
        for stale in [PanelKeys::Input, PanelKeys::Browse] {
            state.borrow().nav_mode.set(stale);
            forget_the_pages_nav_mode(&state.borrow());
            assert_eq!(state.borrow().nav_mode.get(), PanelKeys::Other, "stale {stale:?}");
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
            run_nav_fallthrough(&state, NavKeyDirection::Down),
            Err("nav fallthrough is not wired")
        );

        let seen: Rc<RefCell<Vec<Direction>>> = Rc::new(RefCell::new(Vec::new()));
        {
            let seen = seen.clone();
            state.borrow_mut().nav_fallthrough_hook = Some(Rc::new(move |direction| seen.borrow_mut().push(direction)));
        }
        for (wire, direction) in [
            (NavKeyDirection::Down, Direction::Down),
            (NavKeyDirection::Up, Direction::Up),
        ] {
            assert_eq!(run_nav_fallthrough(&state, wire), Ok(()));
            assert_eq!(seen.borrow_mut().pop(), Some(direction), "{wire:?} -> {direction:?}");
        }
    }

    /// v1 picks, Task 6 (R11): `pane_nav` runs the very hook `nav_fallthrough` does -- `main.rs`'s
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
            assert_eq!(run_pane_nav(&state, wire), Err("pane nav is not wired"), "{wire:?}");
        }

        let seen: Rc<RefCell<Vec<Direction>>> = Rc::new(RefCell::new(Vec::new()));
        {
            let seen = seen.clone();
            state.borrow_mut().nav_fallthrough_hook = Some(Rc::new(move |direction| seen.borrow_mut().push(direction)));
        }
        for (wire, direction) in [
            (PaneNavDirection::Left, Direction::Left),
            (PaneNavDirection::Down, Direction::Down),
            (PaneNavDirection::Up, Direction::Up),
            (PaneNavDirection::Right, Direction::Right),
        ] {
            assert_eq!(run_pane_nav(&state, wire), Ok(()));
            assert_eq!(seen.borrow_mut().pop(), Some(direction), "{wire:?} -> {direction:?}");
            assert!(
                seen.borrow().is_empty(),
                "{wire:?}: the hook is called once per message"
            );
        }
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
        state.borrow_mut().nav_fallthrough_hook = Some(Rc::new(move |_| {
            let state = reentrant.upgrade().expect("the state outlives the call");
            state.borrow_mut().pane_focused = true;
        }));
        assert_eq!(run_pane_nav(&state, PaneNavDirection::Right), Ok(()));
        assert!(state.borrow().pane_focused);
    }

    /// v1 picks, Task 8 (R6): the addresses `gx` may hand `UriLauncher`. The page sends a normalized
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

    /// **Correction (R07/S2, D3):** the old version of this test asserted bypass remembered on
    /// disk, which D3 forbids and which `save_mode` now refuses outright. Auto -> bypass
    /// always asks first (D2) and writes nothing either way; only leaving bypass (always to Auto,
    /// which D3 allows) is ever remembered.
    #[test]
    fn cycle_default_mode_moves_the_default_and_remembers_auto_only_on_disk() {
        let dir = agent::state_dirs::test_workspace_dir("panel-cycle-default-mode");
        let prefs_dir = dir.join("prefs");
        let project_dir = dir.join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let open_tab = set.active();
        let state = state_for_hooks(set);
        state.borrow_mut().prefs_dir = Some(prefs_dir.clone());
        state.borrow_mut().project_dir = project_dir.clone();

        let plan = match cycle_default_mode(&state) {
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

        // Confirming is `apply_confirm_bypass`'s job (Task 3), which is `TabSet::confirm_bypass`
        // and nothing else -- it moves the in-memory default and still writes nothing (D3).
        assert_eq!(
            apply_confirm_bypass(&state, plan.scope, plan.nonce),
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

        // Leaving bypass moves at once and IS remembered -- it is Auto, which D3 allows.
        assert_eq!(
            cycle_default_mode(&state),
            eitri_core::tab_set::ModeCycle::Changed(SessionModeChoice::Auto)
        );
        assert_eq!(state.borrow().tabs.default_mode(), SessionModeChoice::Auto);
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

    /// D2/D3: `apply_confirm_bypass` -- the `ConfirmBypass` arm's WebView-free body -- moves a
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

        let plan = match state.borrow_mut().tabs.cycle_mode(tab) {
            Ok(eitri_core::tab_set::ModeCycle::Confirm(plan)) => plan,
            other => panic!("an empty auto tab must ask first: {other:?}"),
        };
        assert_eq!(
            apply_confirm_bypass(&state, plan.scope, plan.nonce),
            Ok(eitri_core::tab_set::ConfirmOutcome::Entered {
                approved: 0,
                resolved: vec![]
            })
        );
        assert_eq!(set_mode(&state, tab), SessionModeChoice::Bypass);
        assert_eq!(
            eitri_core::agent_prefs::load_mode(&prefs_dir, &project_dir),
            eitri_core::agent_prefs::LoadedMode::Missing,
            "D3: bypass is never written, even the one apply_confirm_bypass itself enters"
        );
    }

    /// Codex v1-mode finding 1: entering bypass on a legacy tab approves its waiting cards through
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
        let background = set.open();
        set.select(active);
        let state = state_for_hooks(set);
        let resolved = vec![agent::AgentDomainEvent::PermissionResolved {
            permission_id: "perm-1".into(),
            outcome: agent::PermissionOutcome::Allowed,
        }];

        let payload = returned_events_payload(&state.borrow(), active, &resolved)
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

    /// Spec §3.3/§3.4: the pane losing the keys entirely -- the WebView-free half of
    /// `set_pane_focused(false)`, `TabSet::drop_bypass_prompt` -- drops an outstanding bypass
    /// prompt the same way switching tabs already does, so a `y` that slips through afterwards is
    /// refused by the nonce rather than landing on a tab no longer on screen.
    #[test]
    fn a_bypass_prompt_dropped_by_a_focus_loss_refuses_a_later_confirm() {
        let set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = set.active();
        let state = state_for_hooks(set);

        let plan = match state.borrow_mut().tabs.cycle_mode(tab) {
            Ok(eitri_core::tab_set::ModeCycle::Confirm(plan)) => plan,
            other => panic!("an empty auto tab must ask first: {other:?}"),
        };

        // `AgentPanelHandle::set_pane_focused(false)` calls exactly this on the state it holds.
        state.borrow_mut().tabs.drop_bypass_prompt();

        assert_eq!(
            apply_confirm_bypass(&state, plan.scope, plan.nonce),
            Err("that prompt is no longer current".to_string())
        );
        assert_eq!(set_mode(&state, tab), SessionModeChoice::Auto, "nothing changed");
    }

    /// Review focus 3 (spec §3.5): every worker is counted, and a worker that died (dropped its
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

    /// Defect 7 (phase 2's sandbox pass): the first tick dispatched `tabs` into a page that had not
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
                rx: done_rx,
            },
            PendingReview {
                request_id: "b".into(),
                rx: failed_rx,
            },
            PendingReview {
                request_id: "c".into(),
                rx: died_rx,
            },
            PendingReview {
                request_id: "d".into(),
                rx: running_rx,
            },
        ];
        let payloads = finished_reviews(&mut jobs);
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
        assert!(finished_reviews(&mut jobs).is_empty(), "nothing is sent twice");
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
        let unsaved = set.open();
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

    /// Ruling 16: the rule comes from what Rust offered for that card, never from the panel, and only
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

    /// Panel-content review finding 4: a request that is no longer pending -- because it was
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

    /// Panel-content review finding 4, the sidecar's own longer window, through the production route
    /// (`answer_permission_response`, the whole `permission_response` arm): `respond_permission`'s
    /// own resolution is not folded into the projection until the provider's real event arrives
    /// (`AgentConversation::respond_permission`'s own doc), so a projection read alone cannot catch
    /// a second response landing before that. The first successful answer, whatever its decision,
    /// drops the offer and marks the card answered (`TabSet::answer_card`), and -- the whole-branch
    /// review's finding -- the next pump, which recomputes offers because another id arrived, does
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
        let outcome =
            answer_permission_response(&mut set, Some(&rules_dir), &dir, tab, response("perm-1", "deny", false));
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
        let replay =
            answer_permission_response(&mut set, Some(&rules_dir), &dir, tab, response("perm-1", "allow", true));
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
        set.get_mut(tab).unwrap().queue.push(eitri_core::tab_set::Queued {
            text: "a".into(),
            wire: "a".into(),
            queued_at_ms: 0,
        });
        let rules = std::path::Path::new("/s/eitri/permissions/0123456789abcdef.json");
        let rows = detail_rows(
            set.active_tab(),
            BackendKind::Legacy,
            None,
            std::path::Path::new("/p"),
            Some(rules),
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

    /// Ruling 17 and spec §3.8 point 5: `hello` offers no session that is open in a tab, and still
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
        assert_eq!(settings.value, agent::setting_sources::note());
        let position = |label: &str| labels.iter().position(|l| *l == label).unwrap();
        assert_eq!(position("settings"), position("CLI") + 1, "{labels:?}");
    }

    /// The `settings` row is a fact about every session, so it does not depend on there being one:
    /// an empty tab (no backend, no provider info) says it too, and on either backend.
    #[test]
    fn the_settings_row_is_the_same_on_an_empty_tab_and_on_either_backend() {
        for kind in [BackendKind::Legacy, BackendKind::Sidecar] {
            let set = TabSet::new(kind, eitri_core::agent_bridge::SessionModeChoice::Auto);
            let rows = detail_rows(set.active_tab(), kind, None, std::path::Path::new("/p"), None);
            let settings = rows
                .iter()
                .find(|r| r.label == "settings")
                .expect("the row is always drawn");
            assert_eq!(settings.value, agent::setting_sources::note(), "{}", kind.as_str());
        }
    }

    /// What each tick of the close watch decides, including the two orderings that are judgements
    /// rather than mechanics.
    ///
    /// This is the whole of what `shell`'s own suite can say about the close path: it cannot open a
    /// window, so `AgentPanelHandle::shutdown` itself, the hold, and the fact that GTK destroys the
    /// window as soon as the handler returns are all owed to a human
    /// (`shell/MANUAL_VERIFICATION.md`, the resume-history section's item 8). What a test CAN pin is
    /// that no tick misreports what it saw.
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

    /// The premise the whole close path now rests on: a `gio` application hold keeps
    /// `Application::run` iterating the main loop with no window on screen, and dropping the guard
    /// is what lets `run` return.
    ///
    /// **`#[ignore]`d, and the reason is not flakiness.** `g_application_run` iterates the DEFAULT
    /// main context; two of these in two test threads would fight over it, and `cargo test` runs
    /// this crate's tests in parallel. Run it alone:
    /// `cargo test -p shell a_hold_keeps_the_application_running -- --ignored --exact ...`.
    ///
    /// It drives a plain `gio::Application` rather than a `gtk4::Application` deliberately -- the
    /// latter needs a display, which is what stops `shell` from testing any of this -- and
    /// `gtk4::Application` IS a `gio::Application`, which is where `hold` is defined and what
    /// `AgentPanelHandle::shutdown` calls it through.
    ///
    /// The first half is a control: without the hold, `run` returns as soon as `activate` does. Its
    /// point is that the second half's 300ms is the hold's doing and not the loop's own latency.
    #[test]
    #[ignore = "runs a real gio main loop on the default main context; must not race the rest of the suite"]
    fn a_hold_keeps_the_application_running_after_its_last_window() {
        use gtk4::gio;

        let control = gio::Application::new(None, gio::ApplicationFlags::empty());
        control.connect_activate(|_| {});
        let started = std::time::Instant::now();
        control.run_with_args(&["shell-hold-control"]);
        let without_a_hold = started.elapsed();
        assert!(
            without_a_hold < std::time::Duration::from_millis(100),
            "an application with nothing holding it must not stay in its main loop at all; it took {without_a_hold:?}"
        );

        let held = gio::Application::new(None, gio::ApplicationFlags::empty());
        held.connect_activate(|app| {
            let mut hold = Some(app.hold());
            // The shape `watch_off_the_main_loop` uses: a main-loop source, and a release from
            // inside it once its work is done.
            gtk4::glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
                if started_long_enough_ago() {
                    hold.take();
                    return gtk4::glib::ControlFlow::Break;
                }
                gtk4::glib::ControlFlow::Continue
            });
        });
        let started = std::time::Instant::now();
        RELEASE_AT.with(|cell| cell.set(Some(started + std::time::Duration::from_millis(300))));
        held.run_with_args(&["shell-hold"]);
        let with_a_hold = started.elapsed();
        assert!(
            with_a_hold >= std::time::Duration::from_millis(290),
            "the hold did not keep the loop running: run() returned after {with_a_hold:?}"
        );
        assert!(
            with_a_hold < std::time::Duration::from_secs(5),
            "releasing the hold did not let run() return: it took {with_a_hold:?}"
        );
    }

    thread_local! {
        /// When the ignored hold test's source should release. A thread-local rather than a capture
        /// so the closure above stays the same shape as the real one.
        static RELEASE_AT: std::cell::Cell<Option<std::time::Instant>> = const { std::cell::Cell::new(None) };
    }

    fn started_long_enough_ago() -> bool {
        RELEASE_AT.with(|cell| cell.get().is_some_and(|at| std::time::Instant::now() >= at))
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

    /// The document this panel loads -- at startup, and again on every `reload_document()` -- has to
    /// actually carry the frontend and the global Rust dispatches into. A stale or truncated
    /// `agent-ui/web/dist/index.html` (build.rs's mtime freshness check going wrong, a half-written
    /// build output) would produce a WebView that loads without error and can never receive a single
    /// envelope -- which presents exactly as the wedged panel `reload_document` exists to recover
    /// from, so the recovery would silently reproduce the fault.
    ///
    /// What this does NOT prove: that the reload itself works. Re-injection is a real `load_html`
    /// call on a real `WebView`, and it is verified in a sandbox -- notably `WebViewExt::reload()`
    /// does NOT work for a substitute-data load like this one, per
    /// `shell/MANUAL_VERIFICATION.md`'s 2026-09-11 reload check.
    #[test]
    fn the_embedded_panel_document_carries_the_frontend_and_its_dispatch_entry_point() {
        assert!(
            AGENT_UI_HTML.contains("__eitriDispatch"),
            "the embedded document never installs the global Rust pushes into -- every envelope would be dropped"
        );
        assert!(
            AGENT_UI_HTML.contains("eitriAgent"),
            "the embedded document never posts through the script-message handler -- no command could reach Rust"
        );
    }

    /// The first frame of a freshly loaded document must already be in nvim's colours: the theme
    /// is parsed before the script that renders anything, and nothing else in the document moves.
    #[test]
    fn the_panel_document_carries_its_theme_before_its_script() {
        let tokens = eitri_core::theme::ThemeTokens::fallback();
        let html = themed_document(&tokens.css_vars());
        let csp_at = html
            .find("<meta http-equiv=\"Content-Security-Policy\"")
            .expect("the CSP meta is inserted");
        let style_at = html
            .find("<style id=\"nv-theme\">")
            .expect("the theme block is inserted");
        let script_at = html.find("<script").expect("the single-file build inlines its script");
        assert!(
            csp_at < style_at,
            "the CSP must be in force before the theme style, or it would not cover it"
        );
        assert!(
            style_at < script_at,
            "the theme must be parsed before the script that renders"
        );
        assert_eq!(html.matches("<style id=\"nv-theme\">").count(), 1);
        assert!(html.contains(&format!("--nv-bg:{};", tokens.bg.hex())));
        let end = style_at + html[style_at..].find("</style>").unwrap() + "</style>".len();
        // The CSP meta and the theme style are inserted back to back at the same point `<head>`
        // used to sit -- removing both must reconstruct the untouched embedded document.
        assert_eq!(format!("{}{}", &html[..csp_at], &html[end..]), AGENT_UI_HTML);
    }

    /// Ruling R1 / finding 3 (see `PANEL_CONTENT_SECURITY_POLICY`'s own doc comment): the CSP meta
    /// is the very first thing inside `<head>` -- nothing (not even the theme style) may load before
    /// it, and its `content` must be exactly the policy this test pins so a future edit to one copy
    /// cannot silently drift from the other (`build_agent_panel`'s builder-level CSP).
    #[test]
    fn the_panel_documents_csp_meta_is_first_in_head_and_matches_the_policy_constant() {
        let tokens = eitri_core::theme::ThemeTokens::fallback();
        let html = themed_document(&tokens.css_vars());
        let head_at = html.find("<head>").expect("the document has a <head>");
        let after_head = head_at + "<head>".len();
        assert!(
            html[after_head..].starts_with("<meta http-equiv=\"Content-Security-Policy\""),
            "the CSP meta must be the very first thing after <head>, before even the theme style"
        );
        let expected_meta =
            format!("<meta http-equiv=\"Content-Security-Policy\" content=\"{PANEL_CONTENT_SECURITY_POLICY}\">");
        assert!(html.contains(&expected_meta), "{html}");
        assert_eq!(html.matches("Content-Security-Policy").count(), 1);
    }

    /// Pins the policy string itself against silent drift, and each directive's presence against
    /// the reasoning in `PANEL_CONTENT_SECURITY_POLICY`'s doc comment: `default-src 'none'` closes
    /// everything not named, and nothing here reopens network access for the panel (`connect-src`,
    /// `frame-src`, `object-src`, `media-src` all `'none'`; images/fonts limited to `data:`).
    #[test]
    fn the_panel_csp_closes_every_directive_it_does_not_explicitly_reopen() {
        assert_eq!(
            PANEL_CONTENT_SECURITY_POLICY,
            "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; \
             font-src data:; media-src 'none'; connect-src 'none'; frame-src 'none'; object-src 'none'; \
             form-action 'none'; base-uri 'none'"
        );
        assert!(PANEL_CONTENT_SECURITY_POLICY.starts_with("default-src 'none';"));
        for directive in [
            "connect-src 'none'",
            "frame-src 'none'",
            "object-src 'none'",
            "media-src 'none'",
        ] {
            assert!(
                PANEL_CONTENT_SECURITY_POLICY.contains(directive),
                "{directive} missing from {PANEL_CONTENT_SECURITY_POLICY}"
            );
        }
        // No directive allows an https:/http: remote load -- every source list is 'none', an inline
        // keyword, or `data:`.
        assert!(!PANEL_CONTENT_SECURITY_POLICY.contains("https:"));
        assert!(!PANEL_CONTENT_SECURITY_POLICY.contains("http:"));
        assert!(!PANEL_CONTENT_SECURITY_POLICY.contains('*'));
    }

    /// The `WebView` itself must be built with the same policy as a floor the page cannot loosen
    /// (finding 3's "No CSP" half) -- source-scanned the same way
    /// `the_panel_document_is_loaded_in_a_secure_context_that_never_resolves` pins `load_html`,
    /// since constructing a real `WebView` needs a running WebKitGTK display this crate's tests do
    /// not have.
    #[test]
    fn the_webview_builder_applies_the_content_security_policy() {
        let source = include_str!("agent_panel.rs");
        let builder_at = source
            .find("WebView::builder()")
            .expect("the panel's WebView is built through WebView::builder()");
        let build_at = source[builder_at..]
            .find(".build()")
            .map(|i| builder_at + i)
            .expect("the builder chain ends in .build()");
        let chain = &source[builder_at..build_at];
        assert!(
            chain.contains(".default_content_security_policy(PANEL_CONTENT_SECURITY_POLICY)"),
            "the WebView builder chain never applies the panel's CSP: {chain}"
        );
    }

    /// Finding 3's backstop (`PANEL_NETWORK_PROXY_URI`'s own doc comment): the panel's `WebView` is
    /// built with an ephemeral `NetworkSession` whose proxy is the closed loopback "discard" port, so
    /// a request that gets past the CSP and the sanitizer still cannot leave. Source-scanned for the
    /// same reason the CSP test above is.
    #[test]
    fn the_webview_builder_applies_the_network_proxy_backstop() {
        assert_eq!(PANEL_NETWORK_PROXY_URI, "http://127.0.0.1:9");
        let source = include_str!("agent_panel.rs");
        // Scoped to `build_agent_panel`'s own body, ending at the WebView `.build()` call -- never
        // the whole file. `source` (the whole file, via `include_str!`) includes this very test's own
        // string literals, so searching all of `source` for e.g. "NetworkSession::new_ephemeral()"
        // would always find a match in the assertion below regardless of what the production code
        // does: a mutation that deleted the real call, or swapped in a persistent `NetworkSession`,
        // would still leave this test green. `fn_at` keeps the needles below restricted to code that
        // actually ran before the WebView was built.
        let fn_at = source
            .find("fn build_agent_panel(")
            .expect("the panel is built by build_agent_panel");
        let builder_at = source[fn_at..]
            .find("WebView::builder()")
            .map(|i| fn_at + i)
            .expect("the panel's WebView is built through WebView::builder()");
        let build_at = source[builder_at..]
            .find(".build()")
            .map(|i| builder_at + i)
            .expect("the builder chain ends in .build()");
        let scope = &source[fn_at..build_at];
        let chain = &source[builder_at..build_at];
        assert!(
            chain.contains(".network_session(&network_session)"),
            "the WebView builder chain never installs the ephemeral network session: {chain}"
        );
        assert!(
            scope.contains("NetworkSession::new_ephemeral()"),
            "the backstop session must be ephemeral: no cookies, cache or storage"
        );
        assert!(
            scope.contains("NetworkProxyMode::Custom")
                && scope.contains("NetworkProxySettings::new(Some(PANEL_NETWORK_PROXY_URI)"),
            "the ephemeral session must be pointed at the unreachable proxy, not left on WebKit's default"
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

    /// Wave 4 R2 (owner, 2026-09-26: "默认界面是new session这个界面，不用默认弹到all session界面"): the first
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
        // The tab set's `open_session_ids` includes a handed-off session (ruling 13).
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
        assert!(handoff_payloads(TabId(1), &HandoffCloseOutcome::StillClosing, "req-1", &a_command()).is_empty());

        closed_tx.send(()).unwrap();
        assert_eq!(classify_close_signal(&closed_rx), HandoffCloseOutcome::Closed);
        assert_eq!(
            kinds(&handoff_payloads(
                TabId(1),
                &HandoffCloseOutcome::Closed,
                "req-1",
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

        let payloads = handoff_payloads(TabId(1), &HandoffCloseOutcome::CloseFailed, "req-2", &a_command());
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
            s.restore_source = Some(SavedTabs {
                tabs: saved
                    .iter()
                    .map(|(id, name, mode)| eitri_core::saved_tabs::SavedTab {
                        conversation_id: conversation_id.clone(),
                        provider_session_id: id.to_string(),
                        name: name.map(str::to_string),
                        mode: *mode,
                    })
                    .collect(),
                active: 0,
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
        senders: Vec<mpsc::Sender<Result<AgentBackend, BackendError>>>,
        held_when_first_asked: Option<Vec<bool>>,
        all: Vec<String>,
    }

    impl FakeResumes {
        fn spawner(
            &mut self,
            canonical: String,
        ) -> impl FnMut(&str, agent::lease::SessionLease) -> mpsc::Receiver<Result<AgentBackend, BackendError>> + '_
        {
            move |id, lease| {
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
                let (tx, rx) = mpsc::channel();
                self.senders.push(tx);
                rx
            }
        }
    }

    fn ask(state: &Rc<RefCell<AgentPanelState>>, resumes: &mut FakeResumes, how: RestoreAsk) -> RestoreResult {
        let mut s = state.borrow_mut();
        let from = s.tabs.active();
        let canonical = s.canonical_project_dir.clone();
        let mut spawn = resumes.spawner(canonical);
        ask_for_restore(&mut s, from, how, &mut spawn)
    }

    fn answer(
        state: &Rc<RefCell<AgentPanelState>>,
        resumes: &mut FakeResumes,
        nonce: u64,
        keep: bool,
    ) -> RestoreResult {
        let mut s = state.borrow_mut();
        let canonical = s.canonical_project_dir.clone();
        let mut spawn = resumes.spawner(canonical);
        answer_restore_question(&mut s, nonce, keep, &mut spawn)
    }

    fn started(result: RestoreResult) -> Option<String> {
        match result {
            RestoreResult::Started { notice } => notice,
            RestoreResult::Refused(why) => panic!("refused: {why}"),
            RestoreResult::Question(prompt) => panic!("asked: {:?}", prompt.lines),
        }
    }

    fn mode(state: &Rc<RefCell<AgentPanelState>>, session: &str) -> SessionModeChoice {
        let s = state.borrow();
        s.tabs.get(s.tabs.tab_with_session(session).unwrap()).unwrap().mode()
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
        let greeting = BackendGreeting::for_kind(s.backend_kind, s.project_dir.clone());
        assert!(restore_is_on_offer(&s));
        let offer = restore_offer(&s, &greeting).expect("two tabs can come back");
        assert_eq!(offer.labels, vec!["title of s-a".to_string(), "api".to_string()]);
        assert_eq!(offer.bypass, 1);
    }

    #[test]
    fn nothing_is_offered_after_a_session_started_under_another_policy_or_when_nothing_would_come_back() {
        let (state, _dir) = restore_panel("restore-not-offered", &[("s-a", None, Auto)], &["s-a"]);
        let greeting = {
            let s = state.borrow();
            BackendGreeting::for_kind(s.backend_kind, s.project_dir.clone())
        };
        assert!(restore_offer(&state.borrow(), &greeting).is_some());

        state.borrow_mut().restore_policy = RestorePolicy::Auto;
        assert!(
            restore_offer(&state.borrow(), &greeting).is_none(),
            "auto restores at launch, it does not offer"
        );
        state.borrow_mut().restore_policy = RestorePolicy::Off;
        assert!(restore_offer(&state.borrow(), &greeting).is_none());
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
        assert!(restore_offer(&state.borrow(), &greeting).is_none());
        state.borrow_mut().tabs.get_mut(started_tab).unwrap().backend = TabBackend::NotStarted;
        assert!(restore_offer(&state.borrow(), &greeting).is_some());

        // Every saved tab skipped: a key that restores nothing is not offered.
        let (nothing, _dir) = restore_panel("restore-nothing", &[("s-x", None, Auto)], &[]);
        let greeting = {
            let s = nothing.borrow();
            BackendGreeting::for_kind(s.backend_kind, s.project_dir.clone())
        };
        assert!(restore_offer(&nothing.borrow(), &greeting).is_none());
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
        assert!(state.borrow().restore_source.is_none(), "used up");
        let numbers: Vec<Option<String>> = state
            .borrow()
            .tabs
            .tabs()
            .iter()
            .map(|t| t.provider_session_id())
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
        let second = state.borrow().tabs.tab_with_session("s-2").unwrap();
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
        let first = state.borrow().tabs.tab_with_session("s-1").unwrap();
        let second = state.borrow().tabs.tab_with_session("s-2").unwrap();
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
        state.borrow_mut().tab_memory = TabMemory::new(Some(files.clone()), dir.clone());
        remember_tabs(&state);
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
        let loaded = match eitri_core::saved_tabs::load(&files, &dir) {
            eitri_core::saved_tabs::Loaded::Saved(saved) => saved,
            other => panic!("{other:?}"),
        };
        assert_eq!(loaded.tabs.len(), 1);
        assert_eq!(loaded.tabs[0].provider_session_id, "s-live");
        assert_eq!(loaded.tabs[0].conversation_id, state.borrow().conversation_id);

        let taken = state.borrow_mut().tabs.take_all();
        remember_tabs(&state);
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

    fn handle_for(state: &Rc<RefCell<AgentPanelState>>) -> AgentPanelHandle {
        AgentPanelHandle {
            state: state.clone(),
            webview: None,
        }
    }

    /// `agent.restore = "off"` is the whole feature off: nothing is offered, nothing is remembered.
    #[test]
    fn restore_off_offers_nothing_and_remembers_nothing() {
        let (state, dir) = restore_panel("restore-off", &[("s-1", None, Auto)], &["s-1"]);
        let files = agent::state_dirs::test_workspace_dir("restore-off-files");
        state.borrow_mut().tab_memory = TabMemory::new(Some(files.clone()), dir.clone());
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
}
