//! The real agent-ui panel: a `WebView` hosting the embedded `agent-ui/web` frontend, a
//! `UserContentManager` script-message bridge (JS -> Rust), and a fast poll of every tab's backend
//! pushed to the page via `evaluate_javascript` (Rust -> JS). See
//! docs/superpowers/specs/2026-09-07-agent-ui-design.md and, for the tabs,
//! docs/superpowers/specs/2026-09-25-keymap-tabs-panel-design.md §3.
//!
//! **One backend per tab** (session tabs spec §3.1). The tabs themselves live in
//! `neovibe_core::tab_set::TabSet`; this module spawns the workers that set only holds receivers
//! for, drains every tab on one 33 ms tick, and routes each inbound command to the tab it names --
//! never to "the active one" (spec §3.8 point 2). Backend construction is lazy per tab: an empty
//! tab starts its backend on its first `send_message` (or a `resume`), with that tab's own mode --
//! `PermissionMode` is a construction-time-only choice on the real `agent` API (no live mode-switch
//! exists). Which backend gets built is `neovibe_core::agent_backend`'s decision.
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

use gtk4::prelude::*;
use gtk4::Application;
use neovibe_core::agent_backend::{AgentBackend, BackendError, BackendGreeting, BackendKind};
use neovibe_core::agent_bridge::{
    parse_inbound_message, serialize_command_result_for_js, serialize_error_for_js, serialize_events_for_js,
    serialize_hello_for_js, ChooserRecord, ChooserTab, DetailRow, InboundMessage, SessionModeChoice, TabVerbWire,
};
use neovibe_core::keymap::TabAction;
use neovibe_core::tab_set::{FirstTurn, PendingHandoff, PendingStart, ResumeRoute, StartCollected, Tab, TabBackend};
use neovibe_core::tabs::TabId;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use webkit6::prelude::*;
use webkit6::{UserContentManager, WebView};

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
/// three thread joins) and is knowingly shorter than the sidecar path's documented worst case;
/// `AgentPanelHandle::shutdown`'s handoff branch says what expiring costs.
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
/// anything: `shutdown()` issues `close_session`, a unary RPC bounded at 10s (`UNARY_RPC_TIMEOUT`,
/// agent/src/providers/claude_sidecar/mod.rs), and it is `SpawnedSidecar::drop` (close stdin, poll
/// up to 3s, then SIGKILL) plus `RuntimeThread::drop` that end the child. 15 seconds covers that
/// ~13s worst case with a little room; a round of this branch's review found an earlier version
/// deriving 15 from those same two numbers while reporting BEFORE the drop, so the escalation it
/// was sized for sat outside the window it bounded.
///
/// **What expiring costs**, since it is a bound and not a guarantee: one line on stderr naming what
/// is still outstanding, then the hold is released anyway, `Application::run` returns, and the
/// detached worker dies with the process -- i.e. exactly the orphaned `claude`/sidecar/`node` this
/// file keeps chasing with pid diffs, which is why the number is generous rather than tight.
const SESSION_CLOSE_BACKSTOP: std::time::Duration = std::time::Duration::from_secs(15);

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
    tabs: neovibe_core::tab_set::TabSet,
    backend_kind: BackendKind,
    project_dir: PathBuf,
    /// `project_dir.canonicalize()`, read once: the `canonical_cwd` a session lease is keyed on
    /// (`agent::lease::SessionLease::is_held`), for resume routing and the chooser.
    canonical_project_dir: String,
    /// `$XDG_STATE_HOME/neovibe/agent`, where the remembered mode is written (Task 3). `None`
    /// with no state directory.
    prefs_dir: Option<PathBuf>,
    /// Where the user is, asked for at send time. Never stored across turns: what the editor was
    /// showing when the LAST turn went out is not context, it is a stale claim, and the composer
    /// has no way to tell the model which it is.
    editor_context: neovibe_core::editor_context::ContextSource,
    supervisor: Option<crate::supervisor_client::SupervisorClient>,
    /// Set only while a freshly-spawned `neovibe-supervisor` is still coming up. The pump drains
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
    theme: neovibe_core::theme::ThemeTokens,
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
    /// D10 / ruling 16: `main.rs` says whether the chat is on screen at launch; consumed by the
    /// first `ready` of the process.
    launch_chooser_allowed: bool,
    launch_chooser_done: bool,
    launch_chooser_hook: Option<Rc<dyn Fn()>>,
    chooser_closed_hook: Option<Rc<dyn Fn(bool)>>,
    /// The per-window scratch directory (phase 3 ruling 18), `None` if it could not be made.
    scratch: Option<neovibe_core::scratch::ScratchDir>,
    /// Edits out in nvim, polled by the tick until their marker appears.
    pending_edits: Vec<neovibe_core::scratch::PendingEdit>,
    /// shows and focuses the editor, then hands it these keys; `Err` says why it could not.
    editor_request_hook: Option<EditorRequestHook>,
    /// An edit came back (or was discarded): the keys return to the chat, in INPUT.
    editor_done_hook: Option<Rc<dyn Fn()>>,
    /// Teardowns and connects of tabs that left while the window stays open, so the window-close
    /// backstop covers them too. See [`Retiring`].
    retiring: Retiring,
    /// `$XDG_STATE_HOME/neovibe/history` (C5) and `.../permissions` (D7); `None` with no state directory.
    history_dir: Option<PathBuf>,
    rules_dir: Option<PathBuf>,
    /// The project's prompts, oldest first, as last read or written (ruling 11).
    history: Vec<String>,
    /// Set by `ready`, cleared by `reload_document` (ruling 38).
    document_ready: bool,
    /// The last `editor_context` envelope sent (ruling 32).
    last_context_payload: Option<String>,
    /// Where a `tab_verb` message's mapped `TabAction` goes (panel round 2 plan Task 6). `None`
    /// until `main.rs`'s `AgentPanelHandle::on_tab_verb` installs one.
    tab_verb_hook: Option<Rc<dyn Fn(TabAction)>>,
}

/// [`AgentPanelState::attention_hook`]: called with the attention before a change and after it.
type AttentionHook = Rc<dyn Fn(neovibe_core::attention::Attention, neovibe_core::attention::Attention)>;

/// `AgentPanelHandle::on_editor_request`: the keys to hand nvim; `Err` says why it could not.
type EditorRequestHook = Rc<dyn Fn(&str) -> Result<(), String>>;

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
    webview: WebView,
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
                if let Err(e) = neovibe_core::prompt_history::append(&dir, &root, &texts) {
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
        eprintln!("[agent_panel] reloading the panel document; the session is left untouched");
        // The page that said `ready` is going; the tick holds its envelopes until the new one does.
        self.state.borrow_mut().document_ready = false;
        let vars = self.state.borrow().theme.css_vars();
        self.webview.load_html(&themed_document(&vars), Some(PANEL_BASE_URI));
    }

    /// Non-blocking: returns `true` if `neovibe-supervisor` asked this window to come to the
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
    pub(crate) fn set_theme(&self, tokens: &neovibe_core::theme::ThemeTokens) {
        let payload = neovibe_core::agent_bridge::serialize_theme_for_js(tokens);
        self.state.borrow_mut().theme = tokens.clone();
        paint_webview_background(&self.webview, tokens);
        let script = format!(
            "window.__neovibeDispatch && window.__neovibeDispatch({});",
            serde_json::to_string(&payload).unwrap_or_default()
        );
        self.webview
            .evaluate_javascript(&script, None, None, None::<&gtk4::gio::Cancellable>, |result| {
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
}

impl AgentPanelHandle {
    /// Dispatches one already-serialized envelope to the live document, guarded so it is safe to
    /// call before the page has loaded (in which case `ready`'s own batch re-sends whatever state
    /// mattered, and this call is simply a no-op). `what` is a short label for the error log only.
    fn dispatch(&self, payload: String, what: &'static str) {
        let script = format!(
            "window.__neovibeDispatch && window.__neovibeDispatch({});",
            serde_json::to_string(&payload).unwrap_or_default()
        );
        self.webview
            .evaluate_javascript(&script, None, None, None::<&gtk4::gio::Cancellable>, move |result| {
                if let Err(e) = result {
                    eprintln!("[agent_panel] {what} dispatch failed: {e}");
                }
            });
    }

    /// Records whether this panel's pane has keyboard focus and tells the live document, which
    /// dims its mode block when it does not. Safe before the page has loaded for the same reason
    /// `set_theme` is: the dispatch is guarded, and `ready` re-sends the recorded value.
    pub(crate) fn set_pane_focused(&self, focused: bool) {
        self.state.borrow_mut().pane_focused = focused;
        self.dispatch(
            neovibe_core::agent_bridge::serialize_pane_focus_for_js(focused),
            "pane focus",
        );
    }

    /// Asks the panel to open its composer with the caret in it. Sent only from the new-tab path
    /// now (panel round 2 plan Task 6, spec §8); see `serialize_enter_input_for_js`. Safe before
    /// the page loads (the dispatch is guarded), in which case there is no composer yet and nothing
    /// happens.
    pub(crate) fn enter_input(&self) {
        self.dispatch(
            neovibe_core::agent_bridge::serialize_enter_input_for_js(),
            "enter-input",
        );
    }

    /// Where every other keyboard arrival lands: BROWSE, on the last row, following resumed if it
    /// was, or the view restored as it was left (spec §8, Owner answers Q1). Sent after `Ctrl+l`/a
    /// tray chip/`prefix a`'s miss have moved GTK focus into the panel (panel round 2 plan Task 6,
    /// decision 4: reverses 2026-09-19's "control l直接闪cursor"). Mirrors `enter_input` exactly
    /// except for the envelope it dispatches; safe before the page loads for the same reason.
    pub(crate) fn arrive(&self) {
        self.dispatch(neovibe_core::agent_bridge::serialize_arrive_for_js(), "arrive");
    }

    /// The `?` overlay's rows (`serialize_keymap_for_js`): recorded for every later `ready`, and
    /// sent now to a document that is already up.
    pub(crate) fn set_keymap_help(&self, payload: String) {
        self.state.borrow_mut().keymap_help = Some(payload.clone());
        self.dispatch(payload, "keymap");
    }

    /// `send-prefix`/`send-keys` with the panel holding the keys (keymap spec §2.6).
    pub(crate) fn literal_key(&self, key: &neovibe_core::keymap::KeySpec) {
        self.dispatch(
            neovibe_core::agent_bridge::serialize_literal_key_for_js(&key.to_string()),
            "literal-key",
        );
    }

    /// `prefix ?`: open the `?` overlay.
    pub(crate) fn open_keymap(&self) {
        self.dispatch(
            neovibe_core::agent_bridge::serialize_open_keymap_for_js(),
            "open-keymap",
        );
    }

    /// Global `f` HINT (spec: docs/superpowers/specs/2026-09-19-global-hint-design.md §3.3). Each
    /// is a one-line dispatch of the matching `serialize_hint_*_for_js` envelope; `shell::hint`
    /// drives the session, this handle only relays it to the WebView.
    pub(crate) fn hint_collect(&self, session_id: u64) {
        self.dispatch(
            neovibe_core::agent_bridge::serialize_hint_collect_for_js(session_id),
            "hint collect",
        );
    }
    pub(crate) fn hint_show(&self, session_id: u64, labels: &[String]) {
        self.dispatch(
            neovibe_core::agent_bridge::serialize_hint_show_for_js(session_id, labels),
            "hint show",
        );
    }
    pub(crate) fn hint_prefix(&self, session_id: u64, typed: &str) {
        self.dispatch(
            neovibe_core::agent_bridge::serialize_hint_prefix_for_js(session_id, typed),
            "hint prefix",
        );
    }
    pub(crate) fn hint_land(&self, session_id: u64, index: usize) {
        self.dispatch(
            neovibe_core::agent_bridge::serialize_hint_land_for_js(session_id, index),
            "hint land",
        );
    }
    pub(crate) fn hint_end(&self, session_id: u64) {
        self.dispatch(
            neovibe_core::agent_bridge::serialize_hint_end_for_js(session_id),
            "hint end",
        );
    }

    /// Where the panel's two HINT messages go. `shell::hint::HintCoordinator` installs this once.
    pub(crate) fn on_hint(&self, hook: impl Fn(HintInbound) + 'static) {
        self.state.borrow_mut().hint_hook = Some(Rc::new(hook));
    }

    /// Where the scratch round trips (`Ctrl+g`, `gf`) hand nvim their keys. The hook shows and
    /// focuses the editor, then sends them; `Err` says why it could not. `main.rs` installs this once.
    pub(crate) fn on_editor_request(&self, hook: impl Fn(&str) -> Result<(), String> + 'static) {
        self.state.borrow_mut().editor_request_hook = Some(Rc::new(hook));
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
        hook: impl Fn(neovibe_core::attention::Attention, neovibe_core::attention::Attention) + 'static,
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
}

/// The session-tab verbs `main.rs` binds (session tabs plan Tasks 7 and 8). Each borrows, changes
/// the set, drops the borrow, then dispatches. Every one of them does nothing once the window is
/// closing (`AgentPanelState::shutting_down`: the set is empty then).
// Tasks 7 and 8 wire these to the keymap and the window close; until then some have no caller.
#[allow(dead_code)]
impl AgentPanelHandle {
    fn closing(&self) -> bool {
        self.state.borrow().shutting_down
    }

    /// Where the launch chooser's opening is reported (D10): `main.rs` gives it the keys.
    pub(crate) fn on_launch_chooser(&self, hook: impl Fn() + 'static) {
        self.state.borrow_mut().launch_chooser_hook = Some(Rc::new(hook));
    }

    /// Where a chooser's dismissal is reported, with its `launch` flag: dismissing the launch
    /// chooser hands the keys to the editor (D10).
    pub(crate) fn on_chooser_closed(&self, hook: impl Fn(bool) + 'static) {
        self.state.borrow_mut().chooser_closed_hook = Some(Rc::new(hook));
    }

    /// Whether the chat is on screen at launch (ruling 16). Read once, by the first `ready`.
    pub(crate) fn set_launch_chooser_allowed(&self, allowed: bool) {
        self.state.borrow_mut().launch_chooser_allowed = allowed;
    }

    /// `prefix c`: opens a tab, selects it, and sends `tabs` -- nothing else: an empty tab has no
    /// state yet.
    pub(crate) fn new_tab(&self) {
        if self.closing() {
            return;
        }
        self.state.borrow_mut().tabs.open();
        send_tabs(&self.state, &self.webview);
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

    fn switch_with(&self, select: impl FnOnce(&mut neovibe_core::tab_set::TabSet) -> Option<TabId>) -> bool {
        if self.closing() {
            return false;
        }
        let selected = select(&mut self.state.borrow_mut().tabs);
        if selected.is_none() {
            return false;
        }
        send_switch(&self.state, &self.webview);
        true
    }

    /// `prefix ,`: the inline rename field on the active tab's label.
    pub(crate) fn begin_rename(&self) {
        if self.closing() {
            return;
        }
        let payload = {
            let state = self.state.borrow();
            let tab = state.tabs.active_tab();
            neovibe_core::agent_bridge::serialize_begin_rename_for_js(tab.id, tab.name.as_deref())
        };
        self.dispatch(payload, "begin-rename");
    }

    /// `prefix &`: the footer's y/n prompt for the active tab.
    pub(crate) fn confirm_close(&self) {
        if self.closing() {
            return;
        }
        let payload = {
            let state = self.state.borrow();
            let active = state.tabs.active();
            let facts = state.tabs.close_facts(active).expect("the active tab exists");
            neovibe_core::agent_bridge::serialize_confirm_close_for_js(
                active,
                &neovibe_core::tabs::close_prompt(&facts),
            )
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
        let Some((tabs, prompt)) = self.state.borrow().tabs.close_others_plan() else {
            return;
        };
        let payload = neovibe_core::agent_bridge::serialize_confirm_close_others_for_js(&tabs, &[prompt]);
        self.dispatch(payload, "confirm-close-others");
    }

    /// `prefix w` (and D10's launch chooser, `launch: true`).
    pub(crate) fn open_chooser(&self, launch: bool) {
        if self.closing() {
            return;
        }
        let payload = chooser_payload(&self.state.borrow(), launch);
        self.dispatch(payload, "chooser");
    }

    /// `prefix i`: the active tab's detail popover.
    pub(crate) fn open_detail(&self) {
        if self.closing() {
            return;
        }
        let payload = {
            let state = self.state.borrow();
            detail_payload(&state, state.tabs.active())
        };
        self.dispatch(payload, "tab-detail");
    }

    /// The tray chip, `prefix a`: switches to the tab holding the oldest pending card and puts the
    /// panel's cursor on it. `false` when no tab holds a card.
    pub(crate) fn focus_oldest_card(&self) -> bool {
        if self.closing() {
            return false;
        }
        let target = {
            let mut state = self.state.borrow_mut();
            let Some(target) = state.tabs.oldest_card_tab() else {
                return false;
            };
            state.tabs.select(target);
            target
        };
        send_switch(&self.state, &self.webview);
        self.dispatch(
            neovibe_core::agent_bridge::serialize_focus_permission_for_js(target),
            "focus-permission",
        );
        true
    }

    /// What the chat owes the user now, summed over every tab (spec §3.7): its pending permission
    /// cards, and whether a turn finished while it was off screen.
    pub(crate) fn attention(&self) -> neovibe_core::attention::Attention {
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
        if self.closing() {
            return Ok(0);
        }
        let ids: Vec<TabId> = {
            let state = self.state.borrow();
            if state.tabs.tabs().iter().any(|t| t.pending_handoff.is_some()) {
                return Err("a tab is still being handed off to a terminal; kill the chat once that finishes");
            }
            state.tabs.tabs().iter().map(|t| t.id).collect()
        };
        for id in &ids {
            close_tab(&self.state, &self.webview, *id)?;
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
fn report_attention(state: &Rc<RefCell<AgentPanelState>>, before: neovibe_core::attention::Attention) {
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

pub(crate) fn build_agent_panel(
    project_dir: PathBuf,
    editor_context: neovibe_core::editor_context::ContextSource,
    scratch: Option<neovibe_core::scratch::ScratchDir>,
) -> (gtk4::Widget, AgentPanelHandle) {
    let content_manager = UserContentManager::new();
    let webview = WebView::builder().user_content_manager(&content_manager).build();
    webview.set_hexpand(true);
    webview.set_vexpand(true);

    // Security: never let this WebView itself navigate away from its one embedded document.
    // Even sanitized markdown can legitimately contain an external link (a click), and a script
    // that got past sanitization anyway (defense-in-depth) might try to redirect the page --
    // either way, navigating this WebView to a remote origin would hand that origin the same
    // UserContentManager and therefore the same `neovibeAgent` bridge this panel uses to relay
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
                            // UserContentManager/neovibeAgent bridge); hand it to the system
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
    let backend_kind = BackendKind::from_env();
    println!("[agent_panel] backend: {}", backend_kind.as_str());
    let state_home = std::env::var_os("XDG_STATE_HOME");
    let home = std::env::var_os("HOME");
    let prefs_dir = neovibe_core::agent_prefs::state_dir(state_home.as_deref(), home.as_deref());
    let (mode, notes) = neovibe_core::agent_prefs::startup_mode(prefs_dir.as_deref(), &project_dir);
    for note in notes {
        eprintln!("{note}");
    }
    let history_dir = neovibe_core::prompt_history::state_dir(state_home.as_deref(), home.as_deref());
    let rules_dir = neovibe_core::permission_store::state_dir(state_home.as_deref(), home.as_deref());
    let (history, notes) = neovibe_core::prompt_history::startup(history_dir.as_deref(), &project_dir);
    for note in notes {
        eprintln!("{note}");
    }
    let (rules, notes) = neovibe_core::permission_store::startup(rules_dir.as_deref(), &project_dir);
    for note in notes {
        eprintln!("{note}");
    }
    let mut tabs = neovibe_core::tab_set::TabSet::new(backend_kind, mode);
    tabs.set_rules(rules);
    // The key a session lease is taken under (`AgentConversation` canonicalizes its cwd the same
    // way). `main.rs` already canonicalized the root; this only makes the string explicit.
    let canonical_project_dir = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.clone())
        .to_string_lossy()
        .into_owned();
    let state = Rc::new(RefCell::new(AgentPanelState {
        tabs,
        editor_context,
        backend_kind,
        project_dir,
        canonical_project_dir,
        prefs_dir,
        supervisor,
        supervisor_pending,
        shutting_down: false,
        theme: neovibe_core::theme::ThemeTokens::fallback(),
        keymap_help: None,
        pane_focused: false,
        hint_hook: None,
        attention_hook: None,
        last_tabs_payload: None,
        last_open_ids: Vec::new(),
        launch_chooser_allowed: false,
        launch_chooser_done: false,
        launch_chooser_hook: None,
        chooser_closed_hook: None,
        scratch,
        pending_edits: Vec::new(),
        editor_request_hook: None,
        editor_done_hook: None,
        retiring: Retiring::default(),
        history_dir,
        rules_dir,
        history,
        document_ready: false,
        last_context_payload: None,
        tab_verb_hook: None,
    }));

    content_manager.register_script_message_handler("neovibeAgent", None);
    {
        let state = state.clone();
        let webview_for_handler = webview.clone();
        content_manager.connect_script_message_received(Some("neovibeAgent"), move |_manager, js_value| {
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
        webview: webview.clone(),
    };
    (webview.upcast(), handle)
}

/// The panel's single main-loop tick, over EVERY tab (spec §3.1). Three jobs, in order:
///
/// 1. collect every backend that finished constructing on a worker thread (and every handoff whose
///    close finished), and answer the command that has been waiting on it;
/// 2. drain every tab's backend (`TabSet::pump`): `take_ui_delivery` is where the permission policy
///    answers what needs no human, so a tab that was not pumped would stall. The active tab's batch
///    goes to the panel as one `events{tab,fromRevision,throughRevision,events[]}` envelope; a
///    background tab's feeds its attention and marks it stale;
/// 3. report the window's aggregated status to `neovibe-supervisor` (ruling 19).
///
/// Then the `tabs` envelope and `hello` go out if what they describe changed.
fn start_pump_timer(state: Rc<RefCell<AgentPanelState>>, webview: WebView) {
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
        let (payload, first_text, turn_ended, offers_changed) = {
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
            (out.active_payload, out.first_text, out.turn_ended, out.offers_changed)
        };
        if let Some(payload) = payload {
            evaluate_js_dispatch(&webview, &payload);
            // Stamped after the dispatch call, which is where the WebView's own clock starts.
            let mut state_ref = state.borrow_mut();
            if let Some(trace) = state_ref.tabs.active_tab_mut().turn_trace.as_mut() {
                if first_text {
                    trace.mark_first_text_dispatched();
                }
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
        // The active tab's offers changed (a card arrived or went): the panel's third buttons follow.
        if offers_changed.contains(&state.borrow().tabs.active()) && state.borrow().document_ready {
            let payload = {
                let state_ref = state.borrow();
                let tab = state_ref.tabs.active_tab();
                neovibe_core::agent_bridge::serialize_rule_offers_for_js(tab.id, &tab.rule_offers)
            };
            evaluate_js_dispatch(&webview, &payload);
        }
        send_context_if_changed(&state, &webview);
        poll_scratch_edits(&state, &webview);
        send_tabs_if_changed(&state, &webview);
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

/// Hands nvim `keys` through `main.rs`'s hook, which shows and focuses the editor first.
fn request_editor(state: &Rc<RefCell<AgentPanelState>>, keys: &str) -> Result<(), String> {
    let hook = state.borrow().editor_request_hook.clone();
    match hook {
        Some(hook) => hook(keys),
        None => Err("the editor is not connected to this panel".to_string()),
    }
}

/// C5's return half: every edit whose marker appeared goes back to the tab it came from (review
/// focus 4), and the keys return to the chat.
fn poll_scratch_edits(state: &Rc<RefCell<AgentPanelState>>, webview: &WebView) {
    let finished: Vec<(neovibe_core::scratch::PendingEdit, neovibe_core::scratch::EditDone)> = {
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
                evaluate_js_dispatch(webview, &neovibe_core::agent_bridge::serialize_draft_for_js(tab, text));
            }
            evaluate_js_dispatch(
                webview,
                &neovibe_core::agent_bridge::serialize_scratch_for_js(tab, false),
            );
        }
        if let neovibe_core::scratch::EditDone::Failed(why) = &result {
            evaluate_js_dispatch(
                webview,
                &neovibe_core::agent_bridge::serialize_notice_for_js(&format!("the nvim scratch buffer failed: {why}")),
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
            neovibe_core::agent_bridge::context_summary((state_ref.editor_context)().as_ref(), &state_ref.project_dir);
        let now = neovibe_core::agent_bridge::serialize_editor_context_for_js(summary.as_ref());
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
    match neovibe_core::prompt_history::append(&dir, &root, texts) {
        Ok(entries) => {
            let ready = {
                let mut s = state.borrow_mut();
                s.history = entries;
                s.document_ready
            };
            if ready {
                let payload = neovibe_core::agent_bridge::serialize_history_for_js(&state.borrow().history);
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
        neovibe_core::agent_bridge::serialize_queue_for_js(tab, &t.queue, t.queue_error.as_deref())
    };
    evaluate_js_dispatch(webview, &payload);
}

/// A flush's outcome (ruling 3): the same benign/fatal split a command gets, with no request to
/// answer. A refusal leaves the queue and its `error` line; the queue envelope says so.
fn apply_flush(
    state: &Rc<RefCell<AgentPanelState>>,
    webview: &WebView,
    tab: TabId,
    flush: neovibe_core::tab_set::Flush,
) {
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
        if open == state_ref.last_open_ids {
            return;
        }
        let mut greeting = BackendGreeting::for_kind(state_ref.backend_kind, state_ref.project_dir.clone());
        greeting.resumable.retain(|r| !open.contains(&r.provider_session_id));
        state_ref.last_open_ids = open;
        serialize_hello_for_js(&greeting)
    };
    evaluate_js_dispatch(webview, &payload);
}

/// The chooser's envelope (`prefix w`, D10): the open tabs in number order, then every record open
/// in no tab, each marked if another window holds its lease.
fn chooser_payload(state: &AgentPanelState, launch: bool) -> String {
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
                label: neovibe_core::tabs::label(t.number, &t.label_name()),
                marker: neovibe_core::tabs::marker(facts),
                pending: facts.pending,
                resumable,
            }
        })
        .collect();
    let canonical = state.canonical_project_dir.as_str();
    let records = chooser_records(&greeting.resumable, &state.tabs.open_session_ids(), |id| {
        agent::lease::SessionLease::is_held("claude", canonical, id).unwrap_or(false)
    });
    neovibe_core::agent_bridge::serialize_chooser_for_js(launch, &open, &records)
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
                .map(|d| neovibe_core::permission_store::path(d, &state.project_dir))
                .as_deref(),
        ),
        None => Vec::new(),
    };
    neovibe_core::agent_bridge::serialize_tab_detail_for_js(tab, &rows)
}

/// The backend a tab command acts on, or the benign "no active session" refusal (an empty,
/// starting or failed tab). Never another tab's.
fn backend_for(
    tabs: &mut neovibe_core::tab_set::TabSet,
    tab: TabId,
) -> Result<&mut AgentBackend, neovibe_core::agent_backend::BackendError> {
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
    let rows: [(&str, String); 18] = [
        ("name", known(tab.name.clone())),
        ("title", known(tab.title.clone())),
        ("state", tab.wire_state().as_str().to_string()),
        ("mode", tab.mode.as_str().to_string()),
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
    mode: SessionModeChoice,
    resume: Option<String>,
) -> mpsc::Receiver<Result<AgentBackend, BackendError>> {
    let (result_tx, result_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = AgentBackend::start(kind, &project_dir, mode.into(), resume.as_deref());
        // The receiver is gone only if the tab or the panel was torn down mid-connect; dropping
        // the backend here is then the cleanup (`Retiring` normally holds the receiver instead).
        let _ = result_tx.send(result);
    });
    result_rx
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
/// the specific one when the provider produced any stderr at all before dying.
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
        let message = format!(
            "the session ended before it started ({reason}). If you were continuing a previous \
             conversation, it most likely no longer exists -- start a new session instead."
        );
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
        match result {
            StartCollected::Installed {
                tab,
                request_id,
                first_turn,
            } => {
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
                match first_turn {
                    Some(turn) => send_first_turn(state, webview, tab, &request_id, turn),
                    None => evaluate_js_dispatch(webview, &serialize_command_result_for_js(&request_id, Ok(()))),
                }
            }
            StartCollected::Failed { tab, request_id, error } => {
                eprintln!(
                    "[agent_panel] tab {}: backend failed to start: {}",
                    tab.0, error.message
                );
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
            t.turn_trace = neovibe_core::turn_trace::TurnTrace::start();
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
/// "This conversation is closed in Neovibe", which is true only because the command is not released
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
            neovibe_core::agent_bridge::serialize_handoff_for_js(tab, command),
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
fn ready_payloads(
    mut greeting: BackendGreeting,
    open: &[String],
    window: Vec<String>,
    tabs: String,
    active: Vec<String>,
    theme: Option<&str>,
    keymap: Option<&str>,
) -> Vec<String> {
    // Ruling 17: no session open in any tab, or handed off from one, is offered for resume -- the
    // lease refuses a second driver, and a handed-off one has a CLI writing it.
    greeting.resumable.retain(|r| !open.contains(&r.provider_session_id));
    let mut payloads = vec![serialize_hello_for_js(&greeting)];
    payloads.extend(theme.map(str::to_string));
    payloads.extend(keymap.map(str::to_string));
    // Window-level state (the history, then the editor context), before the tabs that use it.
    payloads.extend(window);
    payloads.push(tabs);
    payloads.extend(active);
    payloads
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

/// D7's `remember` (ruling 16): the rule Rust offered for `permission_id`, only with an allow.
fn rule_to_remember(
    tab: &Tab,
    permission_id: &str,
    decision: neovibe_core::agent_bridge::DecisionChoice,
    remember: bool,
) -> Result<Option<agent::PrefixRule>, &'static str> {
    if !remember {
        return Ok(None);
    }
    if decision != neovibe_core::agent_bridge::DecisionChoice::Allow {
        return Err("only an allow can be remembered");
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
    outcome: &Result<Vec<agent::AgentDomainEvent>, neovibe_core::agent_backend::BackendError>,
) -> &[agent::AgentDomainEvent] {
    match outcome {
        Ok(events) => events,
        Err(error) => &error.folded_events,
    }
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
    if !events.is_empty() && active {
        // Only the legacy backend ever gets here with a non-empty batch: it synthesizes events its
        // own wire protocol cannot provide. The sidecar backend returns an empty vec and its state
        // arrives through the pump, from the server.
        let through_revision = {
            let state_ref = state.borrow();
            let revision = state_ref
                .tabs
                .get(tab)
                .and_then(|t| t.live())
                .map(|b| b.projection().last_revision)
                .unwrap_or(0);
            revision
        };
        let from_revision = through_revision.saturating_sub(events.len() as u64);
        evaluate_js_dispatch(
            webview,
            &serialize_events_for_js(tab, from_revision, through_revision, events),
        );
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
            // Everything a fresh document is owed is decided by one pure function, so the reload
            // path is testable without a WebView -- see `ready_payloads`.
            let (payloads, launch_chooser) = {
                let mut state_ref = state.borrow_mut();
                let state_ref = &mut *state_ref;
                let greeting = BackendGreeting::for_kind(state_ref.backend_kind, state_ref.project_dir.clone());
                let open = state_ref.tabs.open_session_ids();
                let offerable = greeting
                    .resumable
                    .iter()
                    .filter(|r| !open.contains(&r.provider_session_id))
                    .count();
                let theme = neovibe_core::agent_bridge::serialize_theme_for_js(&state_ref.theme);
                let context = neovibe_core::agent_bridge::context_summary(
                    (state_ref.editor_context)().as_ref(),
                    &state_ref.project_dir,
                );
                let context_payload = neovibe_core::agent_bridge::serialize_editor_context_for_js(context.as_ref());
                state_ref.last_context_payload = Some(context_payload.clone());
                let window = vec![
                    neovibe_core::agent_bridge::serialize_history_for_js(&state_ref.history),
                    context_payload,
                ];
                let tabs = tabs_payload_recorded(state_ref);
                let active = state_ref.tabs.active_state_payloads();
                let mut payloads = ready_payloads(
                    greeting,
                    &open,
                    window,
                    tabs,
                    active,
                    Some(&theme),
                    state_ref.keymap_help.as_deref(),
                );
                // Last, so nothing the document draws from the payloads above can reset it.
                payloads.push(neovibe_core::agent_bridge::serialize_pane_focus_for_js(
                    state_ref.pane_focused,
                ));
                state_ref.last_open_ids = open;
                // D10 / ruling 16: decided on the process's FIRST `ready` only, never on a reload.
                let launch_chooser = !state_ref.launch_chooser_done
                    && state_ref.launch_chooser_allowed
                    && neovibe_core::tabs::launch_opens_chooser(offerable, true);
                state_ref.launch_chooser_done = true;
                // Ruling 38: from here on the tick may send into this document.
                state_ref.document_ready = true;
                (payloads, launch_chooser)
            };
            dispatch_all(webview, payloads);
            ok(webview);
            if launch_chooser {
                let (payload, hook) = {
                    let state_ref = state.borrow();
                    (chooser_payload(&state_ref, true), state_ref.launch_chooser_hook.clone())
                };
                evaluate_js_dispatch(webview, &payload);
                if let Some(hook) = hook {
                    hook();
                }
            }
        }
        InboundMessage::SendMessage { text, .. } => {
            let tab = tab_of(target);
            // A sent prompt is history whether or not the backend takes it, as in Claude Code.
            remember_prompts(state, webview, std::slice::from_ref(&text));
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
                    neovibe_core::editor_context::compose_turn_text(&text, (state_ref.editor_context)().as_ref());
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
                            let trace = neovibe_core::turn_trace::TurnTrace::start();
                            // `&text` second, and it is the user's own: the panel shows what was
                            // typed, never the composed wire text.
                            let outcome = backend.send_turn(&composed, &text);
                            t.turn_trace = trace;
                            Plan::Sent(outcome)
                        }
                        TabBackend::NotStarted => {
                            let result_rx = spawn_connect(kind, project_dir, t.mode, None);
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
                        let result_rx = spawn_connect(kind, project_dir, t.mode, Some(provider_session_id.clone()));
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
        InboundMessage::PermissionResponse {
            permission_id,
            decision,
            reason,
            remember,
            ..
        } => {
            let tab = tab_of(target);
            let remembered = {
                let state_ref = state.borrow();
                let t = state_ref.tabs.get(tab).expect("resolved above");
                rule_to_remember(t, &permission_id, decision, remember)
            };
            match remembered {
                Err(why) => return refuse(webview, why),
                Ok(Some(rule)) => {
                    let (dir, root) = {
                        let s = state.borrow();
                        (s.rules_dir.clone(), s.project_dir.clone())
                    };
                    let Some(dir) = dir else {
                        return refuse(webview, "no state directory: the rule cannot be saved");
                    };
                    match neovibe_core::permission_store::add(&dir, &root, &rule) {
                        Ok(rules) => {
                            eprintln!(
                                "[permission] rule saved: {} ({})",
                                rule.to_rule_string(),
                                neovibe_core::permission_store::path(&dir, &root).display()
                            );
                            state.borrow_mut().tabs.set_rules(rules);
                        }
                        // Not answered: the card stays and `a` still works (ruling 16).
                        Err(e) => return refuse(webview, &format!("could not save the rule: {e}")),
                    }
                }
                Ok(None) => {}
            }
            let decision = decision.into_decision(reason);
            let outcome = backend_for(&mut state.borrow_mut().tabs, tab)
                .and_then(|backend| backend.respond_permission(&permission_id, decision));
            apply_command_outcome(state, webview, tab, &request_id, outcome);
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
        InboundMessage::CycleMode { .. } => {
            let tab = tab_of(target);
            let cycled = state.borrow_mut().tabs.cycle_mode(tab);
            match cycled {
                Some(mode) => {
                    let (prefs_dir, project_dir) = {
                        let state_ref = state.borrow();
                        (state_ref.prefs_dir.clone(), state_ref.project_dir.clone())
                    };
                    // Remembered for the next empty tab and the next launch (ruling 5). A failure
                    // to write is logged, not refused: the mode itself did change.
                    if let Some(dir) = prefs_dir {
                        if let Err(e) = neovibe_core::agent_prefs::save_mode(&dir, &project_dir, mode) {
                            eprintln!("[agent_panel] could not remember the permission mode: {e}");
                        }
                    }
                    send_tabs(state, webview);
                    ok(webview);
                }
                None => refuse(webview, "the mode is fixed for this session"),
            }
        }
        InboundMessage::OpenDetail { .. } => {
            let tab = tab_of(target);
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
                let wire =
                    neovibe_core::editor_context::compose_turn_text(&text, (state_ref.editor_context)().as_ref());
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
                &neovibe_core::agent_bridge::serialize_queue_taken_for_js(tab, &texts),
            );
            send_queue(state, webview, tab);
            ok(webview);
        }
        InboundMessage::SendNow { text, .. } => {
            let tab = tab_of(target);
            let result = {
                let mut state_ref = state.borrow_mut();
                let state_ref = &mut *state_ref;
                let wire =
                    neovibe_core::editor_context::compose_turn_text(&text, (state_ref.editor_context)().as_ref());
                state_ref.tabs.send_now(tab, &text, wire, now_ms())
            };
            if !text.trim().is_empty() {
                remember_prompts(state, webview, std::slice::from_ref(&text));
            }
            match result {
                Err(why) => refuse(webview, &why),
                Ok(neovibe_core::tab_set::SendNow::Interrupting(outcome)) => {
                    send_queue(state, webview, tab);
                    apply_command_outcome(state, webview, tab, &request_id, outcome);
                }
                Ok(neovibe_core::tab_set::SendNow::Flushed(flush)) => {
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
            let keys = neovibe_core::scratch::open_request(&resolved, line).input_keys();
            match request_editor(state, &keys) {
                Ok(()) => ok(webview),
                Err(why) => refuse(webview, &why),
            }
        }
        InboundMessage::ViewInEditor { title, text, .. } => {
            let prepared = match state.borrow_mut().scratch.as_mut() {
                Some(dir) => dir
                    .prepare_view(&title, &text)
                    .map_err(|e| format!("could not write the scratch file: {e}")),
                None => Err("the scratch directory could not be created at startup".to_string()),
            };
            match prepared.and_then(|request| request_editor(state, &request.input_keys())) {
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
            if let Err(why) = request_editor(state, &request.input_keys()) {
                let mut state_ref = state.borrow_mut();
                state_ref
                    .tabs
                    .finish_scratch_edit(edit.id, &neovibe_core::scratch::EditDone::Discarded);
                edit.cleanup();
                drop(state_ref);
                return refuse(webview, &why);
            }
            state.borrow_mut().tabs.set_draft(tab, &text);
            state.borrow_mut().pending_edits.push(edit);
            evaluate_js_dispatch(
                webview,
                &neovibe_core::agent_bridge::serialize_scratch_for_js(tab, true),
            );
            ok(webview);
        }
        InboundMessage::ChooserClosed { launch, .. } => {
            let hook = state.borrow().chooser_closed_hook.clone();
            if let Some(hook) = hook {
                hook(launch);
            }
            ok(webview);
        }
        InboundMessage::TabVerb { verb, .. } => match run_tab_verb(state, verb) {
            Ok(()) => ok(webview),
            Err(why) => refuse(webview, why),
        },
        InboundMessage::CycleDefaultMode { .. } => {
            // `Shift+Tab` on the chooser's `New session` row or a record (spec §6.3): the window's
            // remembered default, exactly as `CycleMode` remembers a tab's own mode above.
            cycle_default_mode(state);
            send_tabs(state, webview);
            ok(webview);
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
    }
}

/// `cycle_default_mode`: `TabSet::cycle_default_mode` plus remembering the new default, exactly as
/// `CycleMode`'s arm above does for a tab's own mode -- a failure to write is logged, not refused,
/// since the mode itself did change. Never touches the WebView, so it is tested directly; the arm
/// above owns `send_tabs`.
fn cycle_default_mode(state: &Rc<RefCell<AgentPanelState>>) -> SessionModeChoice {
    let mode = state.borrow_mut().tabs.cycle_default_mode();
    let (prefs_dir, project_dir) = {
        let state_ref = state.borrow();
        (state_ref.prefs_dir.clone(), state_ref.project_dir.clone())
    };
    if let Some(dir) = prefs_dir {
        if let Err(e) = neovibe_core::agent_prefs::save_mode(&dir, &project_dir, mode) {
            eprintln!("[agent_panel] could not remember the permission mode: {e}");
        }
    }
    mode
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
    for step in neovibe_core::tabs::close_steps(&facts) {
        match step {
            neovibe_core::tabs::CloseStep::Interrupt => {
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
            neovibe_core::tabs::CloseStep::QueueToHistory => {
                let texts = state.borrow().tabs.queue_texts(tab);
                remember_prompts(state, webview, &texts);
            }
            // Folded into `Remove`: `Retiring::tab` shuts the removed tab's backend down.
            neovibe_core::tabs::CloseStep::Shutdown => {}
            neovibe_core::tabs::CloseStep::Remove => {
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
fn no_session_error() -> neovibe_core::agent_backend::BackendError {
    // `folded_events` empty for the same reason: with no session there is no projection, so this
    // command changed nothing the frontend is owed. See `BackendError::folded_events`.
    neovibe_core::agent_backend::BackendError {
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
fn themed_document(vars: &[(String, String)]) -> String {
    let declarations: String = vars.iter().map(|(name, value)| format!("{name}:{value};")).collect();
    let style = format!("<style id=\"nv-theme\">:root{{{declarations}}}</style>");
    match AGENT_UI_HTML.find("<head>") {
        Some(at) => {
            let insert = at + "<head>".len();
            format!("{}{}{}", &AGENT_UI_HTML[..insert], style, &AGENT_UI_HTML[insert..])
        }
        None => {
            eprintln!("[agent_panel] the panel document has no <head>; loading it without an inline theme");
            AGENT_UI_HTML.to_string()
        }
    }
}

/// The WebView's own background, which shows before the web process has painted anything. WebKit
/// defaults it to opaque white.
fn paint_webview_background(webview: &WebView, tokens: &neovibe_core::theme::ThemeTokens) {
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
const PANEL_BASE_URI: &str = "https://neovibe.invalid/";

fn evaluate_js_dispatch(webview: &WebView, json_payload: &str) {
    let script = format!(
        "window.__neovibeDispatch({});",
        serde_json::to_string(json_payload).unwrap_or_default()
    );
    webview.evaluate_javascript(&script, None, None, None::<&gtk4::gio::Cancellable>, |result| {
        if let Err(e) = result {
            eprintln!("[agent_panel] evaluate_javascript failed: {e}");
        }
    });
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
    action.connect_activate(move |_, _| handle.reload_document());
    app.add_action(&action);
}

#[cfg(test)]
mod tests {
    use super::*;

    use neovibe_core::tab_set::{TabBackend, TabSet};
    use neovibe_core::tabs::TabId;
    use neovibe_core::test_providers::RecordingProvider;

    /// GUI pass, 2026-09-25: every `y` in the panel (a row, a code block, the handoff command, the
    /// `prefix i` popover's line) calls `navigator.clipboard.writeText`, and none of them ever
    /// reached the clipboard. `load_html(.., None)` gives the document an opaque origin, which is not
    /// a secure context, so WebKitGTK 2.52.6 has no `navigator.clipboard` and the optional chain
    /// did nothing. Measured in the sandbox with a bare WebKitGTK view: base `None`, nothing copied;
    /// base `https://neovibe.invalid/`, `wl-paste` printed the text. Every `load_html` here must
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

    fn live_backend(dir: &std::path::Path) -> (std::sync::Arc<RecordingProvider>, AgentBackend) {
        let provider = std::sync::Arc::new(RecordingProvider::default());
        let conversation =
            agent::AgentConversation::create(provider.clone(), dir, agent::PermissionMode::Auto).unwrap();
        (provider, AgentBackend::Sidecar(Box::new(conversation)))
    }

    /// Review focus 1 (spec §3.8 point 2): a permission answered after a switch reaches the tab
    /// that asked, not the one on screen.
    #[test]
    fn a_permission_answer_names_its_own_tab_not_the_active_one() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-permission-tab");
        let mut set = TabSet::new(
            BackendKind::Sidecar,
            neovibe_core::agent_bridge::SessionModeChoice::Auto,
        );
        let asking = set.active();
        let (asking_provider, backend) = live_backend(&dir);
        set.get_mut(asking).unwrap().backend = TabBackend::Live(backend);
        let other = set.open();
        let (other_provider, backend) = live_backend(&dir);
        set.get_mut(other).unwrap().backend = TabBackend::Live(backend);
        assert_eq!(set.active(), other, "the user has switched away");
        // `perm-1` must really be pending on the asking tab: a conversation refuses an answer to a
        // permission it never asked for. A `Write` needs a human, so the pump leaves it a card.
        asking_provider.queue(agent::AgentDomainEvent::PermissionRequested {
            permission_id: "perm-1".into(),
            tool_use_id: None,
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": "main.rs", "content": "" }),
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
        let outcome = backend_for(&mut set, tab)
            .and_then(|backend| backend.respond_permission("perm-1", agent::PermissionDecision::Allow));
        assert!(outcome.is_ok());
        assert_eq!(asking_provider.resolutions(), vec![("perm-1".to_string(), true)]);
        assert!(other_provider.resolutions().is_empty(), "never the active tab");
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
        let mut set = TabSet::new(
            BackendKind::Sidecar,
            neovibe_core::agent_bridge::SessionModeChoice::Auto,
        );
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
        let mut set = TabSet::new(
            BackendKind::Sidecar,
            neovibe_core::agent_bridge::SessionModeChoice::Auto,
        );
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
            theme: neovibe_core::theme::ThemeTokens::fallback(),
            keymap_help: None,
            pane_focused: false,
            hint_hook: None,
            attention_hook: None,
            last_tabs_payload: None,
            last_open_ids: Vec::new(),
            launch_chooser_allowed: false,
            launch_chooser_done: false,
            launch_chooser_hook: None,
            chooser_closed_hook: None,
            scratch: None,
            pending_edits: Vec::new(),
            editor_request_hook: None,
            editor_done_hook: None,
            retiring: Retiring::default(),
            history_dir: None,
            rules_dir: None,
            history: Vec::new(),
            document_ready: false,
            last_context_payload: None,
            tab_verb_hook: None,
        }))
    }

    /// Panel round 2 plan Task 6: `tab_verb` refuses naming why until `main.rs` installs a hook
    /// (`AgentPanelHandle::on_tab_verb`), and once one is installed it is called with the mapped
    /// `TabAction` -- every wire value, not just one, since a mapping this exhaustive is easy to
    /// get wrong silently for the one case nobody wrote a test for.
    #[test]
    fn tab_verb_refuses_with_no_hook_and_calls_the_installed_hook_with_the_mapped_action() {
        let state = state_for_hooks(TabSet::new(
            BackendKind::Sidecar,
            neovibe_core::agent_bridge::SessionModeChoice::Auto,
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
            neovibe_core::agent_bridge::SessionModeChoice::Auto,
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

    /// Panel round 2 plan Task 6, spec §6.3: `CycleDefaultMode` moves the window's remembered
    /// default (not any open tab's own mode -- `tab_set`'s own test already pins that half) and
    /// remembers it on disk exactly as `CycleMode`'s arm does, readable back by `load_mode`.
    #[test]
    fn cycle_default_mode_moves_the_default_and_remembers_it_on_disk() {
        let dir = agent::state_dirs::test_workspace_dir("panel-cycle-default-mode");
        let prefs_dir = dir.join("prefs");
        let project_dir = dir.join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let open_tab = set.active();
        let state = state_for_hooks(set);
        state.borrow_mut().prefs_dir = Some(prefs_dir.clone());
        state.borrow_mut().project_dir = project_dir.clone();

        let mode = cycle_default_mode(&state);
        assert_eq!(
            mode,
            SessionModeChoice::Bypass,
            "Auto -> Bypass is the only other implemented mode"
        );
        assert_eq!(state.borrow().tabs.default_mode(), mode);
        // Never the open tab's own mode (still `NotStarted`, so `cycle_mode` alone would move it;
        // `cycle_default_mode` must not).
        assert_eq!(set_mode(&state, open_tab), SessionModeChoice::Auto);
        assert_eq!(
            neovibe_core::agent_prefs::load_mode(&prefs_dir, &project_dir),
            neovibe_core::agent_prefs::LoadedMode::Remembered(mode)
        );
    }

    /// `cycle_default_mode`'s own reading of a tab's mode field, without going through
    /// `TabSet::cycle_mode` (which would change it): a thin accessor so the test above can assert
    /// on the untouched tab without reaching into a private field from two call sites.
    fn set_mode(state: &Rc<RefCell<AgentPanelState>>, tab: TabId) -> SessionModeChoice {
        state.borrow().tabs.get(tab).unwrap().mode
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
        let theme = neovibe_core::agent_bridge::serialize_theme_for_js(&neovibe_core::theme::ThemeTokens::fallback());
        let keymap = neovibe_core::agent_bridge::serialize_keymap_for_js(
            "Ctrl+b",
            &[],
            &[],
            &neovibe_core::keymap::panel::effective(&Default::default(), None).0,
            "Ctrl+b c",
        );
        let window = vec![
            neovibe_core::agent_bridge::serialize_history_for_js(&["earlier".into()]),
            neovibe_core::agent_bridge::serialize_editor_context_for_js(None),
        ];
        let payloads = ready_payloads(
            legacy_greeting(),
            &[],
            window,
            r#"{"kind":"tabs","active":1,"tabs":[]}"#.to_string(),
            vec![],
            Some(&theme),
            Some(&keymap),
        );
        assert_eq!(
            kinds(&payloads),
            vec!["hello", "theme", "keymap", "history", "editor_context", "tabs"]
        );
    }

    /// Ruling 16: the rule comes from what Rust offered for that card, never from the panel, and only
    /// with an allow.
    #[test]
    fn remember_saves_only_the_rule_rust_offered_and_only_with_an_allow() {
        use neovibe_core::agent_bridge::DecisionChoice;
        let mut set = TabSet::new(
            BackendKind::Sidecar,
            neovibe_core::agent_bridge::SessionModeChoice::Auto,
        );
        let tab = set.active();
        set.get_mut(tab)
            .unwrap()
            .rule_offers
            .insert("perm-1".into(), agent::PrefixRule::parse("Bash(git push *)").unwrap());
        let t = set.get(tab).unwrap();
        assert_eq!(
            rule_to_remember(t, "perm-1", DecisionChoice::Allow, false),
            Ok(None),
            "a plain allow"
        );
        assert_eq!(
            rule_to_remember(t, "perm-1", DecisionChoice::Allow, true)
                .unwrap()
                .unwrap()
                .display(),
            "git push *"
        );
        assert!(
            rule_to_remember(t, "perm-2", DecisionChoice::Allow, true).is_err(),
            "never offered"
        );
        assert!(
            rule_to_remember(t, "perm-1", DecisionChoice::Deny, true).is_err(),
            "remember is an allow"
        );
    }

    #[test]
    fn the_detail_popover_counts_the_queue_and_names_the_rules_file() {
        let mut set = TabSet::new(BackendKind::Legacy, neovibe_core::agent_bridge::SessionModeChoice::Auto);
        let tab = set.active();
        set.get_mut(tab).unwrap().queue.push(neovibe_core::tab_set::Queued {
            text: "a".into(),
            wire: "a".into(),
            queued_at_ms: 0,
        });
        let rules = std::path::Path::new("/s/neovibe/permissions/0123456789abcdef.json");
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
        let theme = neovibe_core::agent_bridge::serialize_theme_for_js(&neovibe_core::theme::ThemeTokens::fallback());
        let keymap = neovibe_core::agent_bridge::serialize_keymap_for_js(
            "Ctrl+b",
            &[],
            &[],
            &neovibe_core::keymap::panel::effective(&Default::default(), None).0,
            "Ctrl+b c",
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
        let set = TabSet::new(
            BackendKind::Legacy,
            neovibe_core::agent_bridge::SessionModeChoice::Bypass,
        );
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
        let refused: Result<Vec<agent::AgentDomainEvent>, _> = Err(neovibe_core::agent_backend::BackendError {
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
            AGENT_UI_HTML.contains("__neovibeDispatch"),
            "the embedded document never installs the global Rust pushes into -- every envelope would be dropped"
        );
        assert!(
            AGENT_UI_HTML.contains("neovibeAgent"),
            "the embedded document never posts through the script-message handler -- no command could reach Rust"
        );
    }

    /// The first frame of a freshly loaded document must already be in nvim's colours: the theme
    /// is parsed before the script that renders anything, and nothing else in the document moves.
    #[test]
    fn the_panel_document_carries_its_theme_before_its_script() {
        let tokens = neovibe_core::theme::ThemeTokens::fallback();
        let html = themed_document(&tokens.css_vars());
        let style_at = html
            .find("<style id=\"nv-theme\">")
            .expect("the theme block is inserted");
        let script_at = html.find("<script").expect("the single-file build inlines its script");
        assert!(
            style_at < script_at,
            "the theme must be parsed before the script that renders"
        );
        assert_eq!(html.matches("<style id=\"nv-theme\">").count(), 1);
        assert!(html.contains(&format!("--nv-bg:{};", tokens.bg.hex())));
        let end = style_at + html[style_at..].find("</style>").unwrap() + "</style>".len();
        assert_eq!(format!("{}{}", &html[..style_at], &html[end..]), AGENT_UI_HTML);
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
            vec![neovibe_core::agent_bridge::serialize_handoff_for_js(TabId(1), &command)],
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
                None
            )),
            vec!["hello", "tabs"]
        );
    }

    /// A reloaded document must get its colours back before anything it would draw with them.
    #[test]
    fn the_theme_follows_the_greeting_and_precedes_everything_else() {
        let theme = neovibe_core::agent_bridge::serialize_theme_for_js(&neovibe_core::theme::ThemeTokens::fallback());
        assert_eq!(
            kinds(&ready_payloads(
                legacy_greeting(),
                &[],
                vec![],
                "{\"kind\":\"tabs\"}".to_string(),
                vec![],
                Some(&theme),
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
        );
        assert_eq!(kinds(&payloads), vec!["hello", "theme", "tabs", "snapshot"]);
    }

    /// The `?` overlay's rows come with every `ready`, right after the colours, so a reloaded
    /// document never shows a keymap it was not told.
    #[test]
    fn the_keymap_follows_the_theme() {
        let theme = neovibe_core::agent_bridge::serialize_theme_for_js(&neovibe_core::theme::ThemeTokens::fallback());
        let keymap = neovibe_core::agent_bridge::serialize_keymap_for_js(
            "Ctrl+b",
            &[],
            &[],
            &neovibe_core::keymap::panel::effective(&Default::default(), None).0,
            "Ctrl+b c",
        );
        assert_eq!(
            kinds(&ready_payloads(
                legacy_greeting(),
                &[],
                vec![],
                "{\"kind\":\"tabs\"}".to_string(),
                vec![],
                Some(&theme),
                Some(&keymap)
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
            permission_modes: neovibe_core::agent_backend::CLIENT_IMPLEMENTED_PERMISSION_MODES,
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
        let payloads = ready_payloads(greeting, &open, vec![], "{}".to_string(), vec![], None, None);
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
}
