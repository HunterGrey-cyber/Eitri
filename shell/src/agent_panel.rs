//! The real agent-ui panel: a `WebView` hosting the embedded `agent-ui/web` frontend, a
//! `UserContentManager` script-message bridge (JS -> Rust), and a fast poll of the backend's
//! `pump()` pushed to the page via `evaluate_javascript` (Rust -> JS). See
//! docs/superpowers/specs/2026-09-07-agent-ui-design.md.
//!
//! Backend construction is lazy: this module starts with `None` and only builds one once the
//! frontend sends `"start_session"` -- `PermissionMode` is a construction-time-only choice on the
//! real `agent` API (no live mode-switch exists), so the frontend must choose before any real
//! subprocess is spawned. Which backend gets built is `neovibe_core::agent_backend`'s decision.
//!
//! **State is server-originated.** For the sidecar backend this module folds nothing of its own:
//! `active_turn_id`, tool calls and permissions all arrive as real events through `pump()`. A
//! command that succeeds returns no events at all; the UI updates on the next poll. Nothing here
//! synthesizes an optimistic `TurnStarted` to make the UI feel faster -- doing so would put the
//! panel's idea of "a turn is running" ahead of the server's, which is the exact shadow state the
//! runtime design forbids.

use agent::UiDelivery;
use gtk4::prelude::*;
use gtk4::Application;
use neovibe_core::agent_backend::{AgentBackend, BackendGreeting, BackendKind};
use neovibe_core::agent_bridge::{
    parse_inbound_message, serialize_command_result_for_js, serialize_error_for_js, serialize_events_for_js,
    serialize_hello_for_js, serialize_snapshot_for_js, InboundMessage, SnapshotView,
};
use std::cell::RefCell;
use std::path::PathBuf;
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

/// The backstop on a backend this panel is tearing down itself, once the window is gone.
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
    session: Option<AgentBackend>,
    backend_kind: BackendKind,
    project_dir: PathBuf,
    /// Where the user is, asked for at send time. Never stored across turns: what the editor was
    /// showing when the LAST turn went out is not context, it is a stale claim, and the composer
    /// has no way to tell the model which it is.
    editor_context: neovibe_core::editor_context::ContextSource,
    supervisor: Option<crate::supervisor_client::SupervisorClient>,
    /// Set only while a freshly-spawned `neovibe-supervisor` is still coming up. The pump drains
    /// it into `supervisor` above. A window that spawns the supervisor cannot connect to it
    /// synchronously -- see `PendingSupervisor` for the failure that taught us that.
    supervisor_pending: Option<std::sync::mpsc::Receiver<Option<crate::supervisor_client::SupervisorClient>>>,
    /// Set once a start-failure has been reported, so the 33ms tick reports it exactly once rather
    /// than every tick for as long as the dead session is installed.
    reported_start_failure: bool,
    /// Set by `AgentPanelHandle::shutdown`, i.e. only on the window-close path, and never cleared.
    ///
    /// **It exists because the main loop now outlives the window.** `shutdown` holds the
    /// application so the process can finish tearing its children down, which means every
    /// main-loop source this panel installed keeps firing for up to `SESSION_CLOSE_BACKSTOP`
    /// afterwards -- against a destroyed window and a `WebView` that is no longer in a widget
    /// tree. The pump reads this and stops; `poll_activate` reads it and stops answering, because
    /// its caller's reaction is `window.present()` on a window GTK has already destroyed.
    shutting_down: bool,
    /// Set while a backend is being constructed on a worker thread. Holds the requestId whose
    /// `command_result` is owed once that finishes -- the reply is deferred, not dropped, which is
    /// exactly what a requestId-addressed protocol is for.
    pending_start: Option<PendingStart>,
    /// Set while a "continue in a terminal" handoff is closing the session on a worker thread.
    /// Holds the command that is owed to the frontend once that close has actually finished.
    pending_handoff: Option<PendingHandoff>,
    /// The last handoff command this panel produced, kept in the Rust host rather than only in the
    /// WebView's React state.
    ///
    /// **This is the reason a reload cannot destroy it.** The panel ships a real reload affordance
    /// (`app.reload-agent-panel`, Ctrl+Shift+R and the top bar's `⟳`) that throws the document
    /// away on purpose, and the frontend's copy goes with it. On the default `legacy` backend the
    /// provider session id is then recoverable from nowhere at all: no `ConversationRecord` is
    /// written for that backend and `BackendGreeting::for_kind` returns `resumable: None` for it, so
    /// the only surviving copy of the id would have been a React `useState`. Holding it here is the
    /// project's own stated invariant -- agent state lives in the Rust host so the WebView can
    /// reload or crash without losing it -- applied to the one value this feature produces.
    ///
    /// Cleared when a new session is actually installed, not when one is merely requested: a start
    /// that fails must leave the previous conversation's command still recoverable.
    last_handoff: Option<agent::handoff::ClaudeResumeCommand>,
    /// The in-flight turn's latency marks, when `NEOVIBE_AGENT_TRACE=1`. `None` the rest of the
    /// time, which is every normal run -- this is a diagnostic, not a metrics pipeline.
    turn_trace: Option<neovibe_core::turn_trace::TurnTrace>,
    /// The panel's current theme. Starts on `ThemeTokens::fallback()` and is replaced by
    /// `AgentPanelHandle::set_theme`. Held as tokens rather than an envelope because it feeds three
    /// things: the `ready` batch, the `<style>` inlined into every `load_html`, and the WebView's
    /// own background colour.
    theme: neovibe_core::theme::ThemeTokens,
    /// Whether this panel's pane holds the window's keyboard focus, as `crate::pane_focus` last
    /// reported it. It is held here so that a freshly loaded document (first load or a
    /// `Ctrl+Shift+R` reload) is told on `ready`. A document is otherwise told only when focus
    /// changes, and a reload does not change focus.
    pane_focused: bool,
    /// Where the panel's two HINT messages (`hint_request`, `hint_targets`) go. `None` until
    /// `shell::hint::HintCoordinator` installs one via `AgentPanelHandle::on_hint`. An `Rc<dyn Fn>`
    /// rather than a plain closure field because the hook must be cloned out of the borrow before
    /// being called -- see `handle_inbound_message`'s HintRequest/HintTargets arms for why.
    hint_hook: Option<Rc<dyn Fn(HintInbound)>>,
    /// What the chat owes the user while it is not on screen (modules P2): the permission cards it
    /// holds and whether a turn finished unseen. Fed by the pump, which runs whether or not the
    /// panel is on screen; see `neovibe_core::attention` for why it counts deliveries.
    attention: neovibe_core::attention::AttentionTracker,
    /// Told whenever `attention` changes, with the value before and after: the tray's `agent ⚑N`,
    /// the toast, `on_permission`.
    attention_hook: Option<AttentionHook>,
}

/// [`AgentPanelState::attention_hook`]: called with the attention before a change and after it.
type AttentionHook = Rc<dyn Fn(neovibe_core::attention::Attention, neovibe_core::attention::Attention)>;

/// An in-flight backend construction. Constructing a sidecar backend spawns a real process, does a
/// real gRPC handshake, and on a cold Verdandi checkout runs `npm ci` + `npm run build` -- minutes
/// of blocking work. Doing that on the GTK main loop would freeze the whole editor, so it happens
/// on a worker thread and the result is collected by a poll on the main loop.
struct PendingStart {
    request_id: String,
    result_rx: mpsc::Receiver<Result<AgentBackend, neovibe_core::agent_backend::BackendError>>,
}

/// A handoff whose session close is still running on a worker thread.
///
/// The command is built BEFORE the session is taken -- it is derived from the session's own state,
/// which stops being readable the moment ownership moves to the worker -- and dispatched only after
/// `closed_rx` reports. That ordering is the whole reason this struct exists rather than a straight
/// reply: design doc §8.3 requires the session to be closed and flushed before the CLI starts, and
/// the user cannot start it before they have seen the command.
///
/// Closing runs off the GTK main loop for the same reason every other teardown here does: the
/// sidecar path's `close_session` is a unary RPC bounded at 10s, and blocking the main loop on it
/// would freeze the editor pane too.
struct PendingHandoff {
    request_id: String,
    command: agent::handoff::ClaudeResumeCommand,
    /// Reports when `AgentBackend::shutdown()` RETURNED, which is what the command waits on.
    closed_rx: mpsc::Receiver<()>,
    /// Reports when the backend has also been DROPPED -- on the sidecar path that is the child's
    /// actual death (`SpawnedSidecar::drop`), which `shutdown()` does not perform. Read only by
    /// `AgentPanelHandle::shutdown`, which holds the application until it arrives; the ordinary
    /// path ignores it and it is dropped with the rest of this struct when the command is
    /// dispatched, at which point the worker's send simply fails.
    dropped_rx: mpsc::Receiver<()>,
}

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
    /// Shuts down whatever backend exists, INCLUDING one still being constructed on a worker thread.
    ///
    /// The in-flight case is not hypothetical: since construction moved off the GTK main loop, a
    /// window closed during the connect leaves a fully-built backend -- a live sidecar process and a
    /// Tokio runtime thread -- owned by nothing but a channel nobody will ever read. Before that
    /// move, GTK's single thread made the window impossible to close while a backend existed outside
    /// `state.session`.
    ///
    /// Taking `pending_start` is load-bearing twice over: it lets a backend that already finished be
    /// shut down properly, and it makes any later `collect_pending_start` tick early-return instead
    /// of installing a session into a panel that has already torn down.
    ///
    /// **This function does not wait for anything, and that is its most important property.** It
    /// runs from `window.connect_close_request` on the GTK main thread, where a wait keeps the
    /// window mapped and frozen until it returns; two previous versions of this code waited there
    /// (3s, then 15s) while describing themselves as not doing so. All three branches below instead
    /// hand their receiver to `watch_off_the_main_loop`, which holds the application and polls.
    /// **This function** returns immediately and the process stays alive -- with no window on
    /// screen -- until the teardowns report or their backstops expire.
    ///
    /// **Scoped to the panel, deliberately, because the previous two versions of this doc were not
    /// and were false for it** (2026-09-21, third consecutive review finding on this path). The
    /// whole close handler does NOT return immediately: `main.rs`'s `connect_close_request` calls
    /// `pane.shutdown()` first, on the GTK thread, and that spins until nvim exits
    /// (`LiveHarness::shutdown` in the fork). So "the window disappears at once" is a claim about
    /// this half only, and a slow `:qa!` -- a hung plugin, a slow `BufWritePre` -- still delays the
    /// close with nothing here involved. That is pre-existing and out of this branch's scope; it is
    /// named so a verifier who sees a lingering window does not come looking here first.
    ///
    /// `app` is a parameter rather than a field for the same reason `install_reload_action` takes
    /// one: this handle is per-window, and the hold belongs to the application that owns the loop
    /// this panel's timers run on.
    pub(crate) fn shutdown(&self, app: &Application) {
        {
            let mut state = self.state.borrow_mut();
            // Read by the 33ms pump and by `poll_activate`, both of which keep ticking now that the
            // loop outlives the window. See the field's own doc.
            state.shutting_down = true;
        }
        let pending = self.state.borrow_mut().pending_start.take();
        let pending_handoff = self.state.borrow_mut().pending_handoff.take();
        // Taken out in its own statement, like the two above it, so the `RefMut` is dropped before
        // anything else runs. In edition 2021 an `if let` scrutinee's temporary lives to the end of
        // the block, which would have meant holding a `RefCell` borrow across the calls below --
        // and one of them (`tear_down_holding_the_application`) installs a main-loop source.
        let session = self.state.borrow_mut().session.take();
        if let Some(session) = session {
            // On a worker, held rather than waited for. The reasoning, and the other caller it is
            // shared with, are in `tear_down_holding_the_application`.
            tear_down_holding_the_application(app, "the session's teardown", session);
        }
        if let Some(handoff) = pending_handoff {
            // The worker already owns the backend and is already shutting it down -- there is
            // nothing to start here, only a reason to keep the process alive. Exiting first would
            // leave the `claude` child mid-close.
            //
            // **`dropped_rx`, not `closed_rx`.** The worker reports on `closed_rx` as soon as
            // `shutdown()` returns, because that is when the handoff command becomes true and may
            // be dispatched; on the sidecar path the child is still alive at that moment and is
            // killed by the backend's drop afterwards (`SpawnedSidecar::drop`: close stdin, poll up
            // to 3s, SIGKILL). `dropped_rx` reports after that drop, so what is held here is the
            // whole teardown rather than its first half.
            //
            // **The backstop is only generous for the default backend, and says so.** On `legacy`,
            // `AgentBackend::shutdown()` is ~0.8s of grace periods (`NATURAL_EXIT_GRACE_PERIOD`
            // 500ms + `GRACE_PERIOD` 300ms, agent/src/process.rs) plus three thread joins, so
            // `HANDOFF_CLOSE_BACKSTOP` covers it with room to spare. On `sidecar` it does not:
            // `close_session` alone is bounded at 10s, so this can expire with the close in flight
            // and leave the orphan class this repository keeps chasing with pid diffs. It is left
            // at 3s rather than raised to `SESSION_CLOSE_BACKSTOP` because nothing here has ever
            // measured this path -- the window has to be closed inside the second between
            // confirming a handoff and the card appearing -- and inventing a number for it would
            // read as evidence. See shell/MANUAL_VERIFICATION.md's owed check for this path.
            watch_off_the_main_loop(
                app,
                "the terminal handoff's close",
                handoff.dropped_rx,
                HANDOFF_CLOSE_BACKSTOP,
                |()| {},
            );
        }
        if let Some(pending) = pending {
            // A connect, not a teardown -- see `CONNECT_CLOSE_BACKSTOP`. The reason to hold the
            // process open for it at all is that a backend which finishes after the process is
            // gone is exactly the orphan described at the top of this function.
            let app_for_teardown = app.clone();
            watch_off_the_main_loop(
                app,
                "the connect that was still running",
                pending.result_rx,
                CONNECT_CLOSE_BACKSTOP,
                move |result| match result {
                    Ok(backend) => {
                        eprintln!(
                            "[agent_panel] window closed mid-connect; shutting down the backend that finished anyway"
                        );
                        // Held and bounded, exactly like the installed session above. This branch
                        // used to call `backend.shutdown()` inline and unbounded, which a review
                        // caught: it reaches the same `ConversationIngest::stop` thread join, and
                        // `fold` marks `history_dirty` on `SessionUnavailable` and `SessionClosed`
                        // as well as `TurnCompleted`, both of which can arrive before the panel ever
                        // installs the session -- so nothing here makes it safe. Narrow in practice
                        // (it also needs `record_written`), but the point of the declaration in
                        // `tear_down_holding_the_application` is that it holds without a caller-side
                        // argument.
                        tear_down_holding_the_application(
                            &app_for_teardown,
                            "the backend that finished mid-connect",
                            backend,
                        );
                    }
                    Err(e) => eprintln!(
                        "[agent_panel] window closed mid-connect; the backend had already failed: {}",
                        e.message
                    ),
                },
            );
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
        let vars = self.state.borrow().theme.css_vars();
        self.webview.load_html(&themed_document(&vars), None);
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

    /// Asks the panel to open its composer with the caret in it. Sent after `Ctrl+l` has moved GTK
    /// focus into the panel; see `serialize_enter_input_for_js`. Safe before the page loads (the
    /// dispatch is guarded), in which case there is no composer yet and nothing happens.
    pub(crate) fn enter_input(&self) {
        self.dispatch(
            neovibe_core::agent_bridge::serialize_enter_input_for_js(),
            "enter-input",
        );
    }

    /// `Ctrl+a Ctrl+a` with the panel focused; see `serialize_select_all_for_js`.
    pub(crate) fn select_all(&self) {
        self.dispatch(neovibe_core::agent_bridge::serialize_select_all_for_js(), "select-all");
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

    /// What the chat owes the user now (modules spec §3.3): its pending permission cards, and
    /// whether a turn finished while it was off screen.
    pub(crate) fn attention(&self) -> neovibe_core::attention::Attention {
        self.state.borrow().attention.attention()
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

    /// The chat was brought back to answer a card (its tray chip, `Ctrl+a a`): BROWSE, with the
    /// cursor on the oldest pending card. See `serialize_focus_permission_for_js`.
    pub(crate) fn focus_permission(&self) {
        self.dispatch(
            neovibe_core::agent_bridge::serialize_focus_permission_for_js(),
            "focus-permission",
        );
    }
}

/// Tells the attention hook, if the value changed. The hook is cloned out of the borrow first: it
/// reaches the layout and the top bar, and must never find this panel's state borrowed.
fn report_attention(state: &Rc<RefCell<AgentPanelState>>, before: neovibe_core::attention::Attention) {
    let (after, hook) = {
        let state = state.borrow();
        (state.attention.attention(), state.attention_hook.clone())
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
    let state = Rc::new(RefCell::new(AgentPanelState {
        session: None,
        editor_context,
        backend_kind,
        project_dir,
        supervisor,
        supervisor_pending,
        pending_start: None,
        pending_handoff: None,
        last_handoff: None,
        reported_start_failure: false,
        shutting_down: false,
        turn_trace: None,
        theme: neovibe_core::theme::ThemeTokens::fallback(),
        pane_focused: false,
        hint_hook: None,
        attention: Default::default(),
        attention_hook: None,
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
        webview.load_html(&themed_document(&tokens.css_vars()), None);
    }

    // Back on screen: whatever finished while the chat was away has been seen (the tray's `agent •`).
    {
        let state = state.clone();
        webview.connect_map(move |_| {
            let before = state.borrow().attention.attention();
            state.borrow_mut().attention.seen();
            report_attention(&state, before);
        });
    }

    // Started once, at construction, rather than when a session starts: it is also what collects a
    // finished background connect. Previously it was started inside the start_session handler,
    // which meant a second start attempt after a failure installed a SECOND timer on the same
    // state -- every later event would then be dispatched twice.
    start_pump_timer(state.clone(), webview.clone());

    let handle = AgentPanelHandle {
        state: state.clone(),
        webview: webview.clone(),
    };
    (webview.upcast(), handle)
}

/// The panel's single main-loop tick. Three jobs, in order:
///
/// 1. collect a backend that finished constructing on a worker thread, and answer the
///    `start_session` command that has been waiting on it;
/// 2. drain the backend's events and dispatch them as one batched
///    `events{fromRevision,throughRevision,events[]}` envelope (not one per event);
/// 3. report derived status to `neovibe-supervisor`.
///
/// `fromRevision`/`throughRevision` are read off the projection immediately before and after the
/// batch, since `pump()` folds each event into the projection before returning. An empty batch
/// sends nothing.
fn start_pump_timer(state: Rc<RefCell<AgentPanelState>>, webview: WebView) {
    gtk4::glib::timeout_add_local(std::time::Duration::from_millis(PUMP_POLL_INTERVAL_MS), move || {
        // The window has been closed and its session taken; everything below would be a no-op
        // against a destroyed window for as long as the close watch holds the application. Stop
        // instead of ticking 30 times a second at nothing. See `AgentPanelState::shutting_down`.
        if state.borrow().shutting_down {
            return gtk4::glib::ControlFlow::Break;
        }
        // Read before anything below changes it: a session installed this tick resets the count.
        let attention_before = state.borrow().attention.attention();
        collect_pending_start(&state, &webview);
        collect_pending_handoff(&state, &webview);

        // The payload is built under the borrow and dispatched OUTSIDE it. Calling into WebKit while
        // holding a `RefMut` on the panel's own state is a latent re-entrancy hazard: the script
        // message handler takes the same `RefCell`, and anything that let it run during the
        // dispatch would panic on an already-borrowed cell rather than fail gracefully. Nothing
        // today re-enters, which is exactly why it would stay latent until it didn't.
        report_a_session_that_never_opened(&state, &webview);

        // The chat's own host: unmapped while it is hidden or zoomed away.
        let on_screen = webview.is_mapped();
        let (payload, first_text_in_this_batch) = {
            let mut state_ref = state.borrow_mut();
            let AgentPanelState {
                session,
                turn_trace,
                supervisor,
                supervisor_pending,
                project_dir,
                attention,
                ..
            } = &mut *state_ref;
            // A supervisor this window had to start itself finishes connecting here rather than
            // during `build_ui`, where waiting for it would have delayed the window appearing.
            if supervisor.is_none() {
                if let Some(rx) = supervisor_pending.as_ref() {
                    match rx.try_recv() {
                        Ok(client) => {
                            *supervisor = client;
                            *supervisor_pending = None;
                        }
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            *supervisor_pending = None;
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    }
                }
            }
            // Folded from the SAME slice the bridge is about to serialize, so the trace describes
            // the events the user is actually about to see rather than a parallel accounting.
            let mut first_text_in_this_batch = false;
            let payload = session.as_mut().and_then(|session| {
                let from_revision = session.projection().last_revision;
                // The project root travels in because the permission policy judges
                // `Read`/`Grep`/`Glob` paths against it -- see
                // `AgentBackend::answer_what_needs_no_human`.
                let payload = match session.take_ui_delivery(project_dir) {
                    UiDelivery::Nothing => None,
                    UiDelivery::Events(events) => {
                        if let Some(trace) = turn_trace.as_mut() {
                            first_text_in_this_batch = trace.observe(&events);
                        }
                        // What the panel is handed is what it draws cards for (modules P2).
                        attention.observe(&events, on_screen);
                        let through_revision = session.projection().last_revision;
                        Some(serialize_events_for_js(from_revision, through_revision, &events))
                    }
                    // More happened than was worth queueing as individual events -- the UI was away
                    // long enough that the bounded queue overflowed. It reloads from canonical state
                    // instead, which is complete by construction rather than a degraded fallback:
                    // the projection saw every event, in order, while nobody was watching.
                    //
                    // The turn trace deliberately observes nothing here. Its "first presentation
                    // delta" mark is about how fast text reaches a LIVE reader; a window that was not
                    // being repainted has no such measurement to contribute, and recording one would
                    // report the stall as latency.
                    UiDelivery::Resync => {
                        attention.resync(session.projection().pending_permissions.keys().cloned());
                        let view = SnapshotView::of(session);
                        Some(serialize_snapshot_for_js(&view))
                    }
                };
                // A card answered from the panel is gone from the projection; on legacy no
                // delivery says so (`respond_permission` returns its resolution to the command).
                let projection = session.projection();
                attention.retain_pending(|id| projection.pending_permissions.contains_key(id));
                payload
            });
            // No session holds no card. Every place that takes one says so itself
            // (`AttentionTracker::session_ended`); this is the backstop for the next one that
            // does not (the whole-branch review's finding 2).
            if session.is_none() {
                attention.retain_pending(|_| false);
            }
            // Bound first: `projection()` returns a guard on the sidecar path, and passing it
            // inline would drop the ingestion lock before `derive_status` had read through it.
            let projection = session.as_ref().map(|s| s.projection());
            let status = crate::supervisor_client::derive_status(projection.as_deref());
            if let Some(supervisor) = supervisor.as_mut() {
                supervisor.send_status(status);
            }
            (payload, first_text_in_this_batch)
        };
        if let Some(payload) = payload {
            evaluate_js_dispatch(&webview, &payload);
            // Stamped after the dispatch call, which is where the WebView's own clock starts.
            let mut state_ref = state.borrow_mut();
            if let Some(trace) = state_ref.turn_trace.as_mut() {
                if first_text_in_this_batch {
                    trace.mark_first_text_dispatched();
                }
                if trace.is_complete() {
                    trace.emit();
                }
            }
        }
        // After the payload, not before it: a reaction that sends the panel an envelope -- one
        // that lands on a card, say -- must find the card it names already there (Task 10's
        // review, minor 3).
        report_attention(&state, attention_before);
        gtk4::glib::ControlFlow::Continue
    });
}

/// A session that reached a terminal state without ever opening never worked, and must not be
/// left on screen as an empty conversation the user cannot act on.
///
/// `AgentConversation::resume` catches the common case synchronously, within its own short window.
/// This is the backstop for everything slower than that window and for fresh sessions, which have
/// no equivalent check: either way the user is returned to the start screen with the provider's own
/// reason, where "start a new session" is available. It is never turned into a fresh session
/// automatically.
///
/// **`reason` used to be a fixed, generic string on the legacy backend** ("provider process
/// exited unexpectedly") for every non-zero exit, regardless of cause -- and that generic text
/// actively misdirected a real investigation (2026-09-18, work, production binary): the child
/// died before opening because a `PATH`-resolved multi-account launcher refused the gate-bearing
/// `--settings` flag outright, and the launcher's own one-line explanation had already gone past
/// on stderr with nowhere for this banner to recover it from. `agent::session`'s translation of
/// `AgentEvent::ProcessExited` now folds the child's own retained stderr tail into
/// `SessionUnavailable.reason` itself (see `agent/src/session.rs`), so this function needed no
/// change to benefit: `reason` below is already the specific one when the provider produced any
/// stderr at all before dying, and only falls back to the generic text when it produced none.
fn report_a_session_that_never_opened(state: &Rc<RefCell<AgentPanelState>>, webview: &WebView) {
    let failure = {
        let state_ref = state.borrow();
        if state_ref.reported_start_failure {
            return;
        }
        state_ref.session.as_ref().and_then(|s| s.terminated_before_opening())
    };
    let Some(reason) = failure else { return };

    let message = format!(
        "the session ended before it started ({reason}). If you were continuing a previous \
         conversation, it most likely no longer exists -- start a new session instead."
    );
    eprintln!("[agent_panel] {message}");
    let on_screen = webview.is_mapped();
    let dead = {
        let mut state_ref = state.borrow_mut();
        state_ref.reported_start_failure = true;
        // Its cards go with it (the pump reports the change).
        state_ref.attention.session_ended(on_screen);
        state_ref.session.take()
    };
    if let Some(mut backend) = dead {
        std::thread::spawn(move || backend.shutdown());
    }
    evaluate_js_dispatch(webview, &serialize_error_for_js(&message));
}

/// Non-blocking check on an in-flight backend construction. `try_recv`, never `recv`: this runs on
/// the GTK main loop 30 times a second and must never block it -- which is the entire reason the
/// construction was moved off this thread in the first place.
fn collect_pending_start(state: &Rc<RefCell<AgentPanelState>>, webview: &WebView) {
    let finished = {
        let mut state_ref = state.borrow_mut();
        let Some(pending) = state_ref.pending_start.as_ref() else {
            return;
        };
        match pending.result_rx.try_recv() {
            Ok(result) => {
                let request_id = pending.request_id.clone();
                state_ref.pending_start = None;
                Some((request_id, result))
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                // The worker died without sending anything -- a panic in backend construction. The
                // command would otherwise wait forever for a reply that is never coming.
                let request_id = pending.request_id.clone();
                state_ref.pending_start = None;
                Some((
                    request_id,
                    Err(neovibe_core::agent_backend::BackendError {
                        message: "the backend connect worker stopped without reporting a result".to_string(),
                        benign: false,
                        // Nothing was ever folded: this failure is the ABSENCE of a backend, so
                        // there is no projection it could have written into. See
                        // `BackendError::folded_events`.
                        folded_events: Vec::new(),
                    }),
                ))
            }
        }
    };

    let Some((request_id, result)) = finished else { return };
    match result {
        Ok(backend) => {
            let snapshot = {
                let mut state_ref = state.borrow_mut();
                state_ref.reported_start_failure = false;
                // The previous conversation's handoff command belongs to the previous conversation;
                // a reload from here must not put it above a live one. Cleared on a session
                // actually being INSTALLED, never on one merely being requested -- a start that
                // fails has to leave the command still recoverable, since on the legacy backend it
                // is the only surviving reference to that conversation.
                state_ref.last_handoff = None;
                // A new conversation holds no card yet; the old one's are gone with it.
                state_ref.attention = Default::default();
                state_ref.session = Some(backend);
                // The frontend has been showing a connecting state since it sent start_session; give
                // it the real projection immediately rather than making it wait for the first event.
                let view = SnapshotView::of(state_ref.session.as_ref().unwrap());
                serialize_snapshot_for_js(&view)
            };
            evaluate_js_dispatch(webview, &snapshot);
            evaluate_js_dispatch(webview, &serialize_command_result_for_js(&request_id, Ok(())));
        }
        Err(error) => {
            eprintln!("[agent_panel] backend failed to start: {}", error.message);
            evaluate_js_dispatch(
                webview,
                &serialize_command_result_for_js(&request_id, Err(&error.message)),
            );
            evaluate_js_dispatch(webview, &serialize_error_for_js(&error.message));
        }
    }
}

/// Non-blocking check on an in-flight terminal handoff, mirroring `collect_pending_start`.
///
/// The command is dispatched here and nowhere else, which is what makes the §8.3 ordering real: by
/// the time the frontend can show a user the line to run, the session's own `shutdown()` has
/// already returned on the worker thread.
fn collect_pending_handoff(state: &Rc<RefCell<AgentPanelState>>, webview: &WebView) {
    let finished = {
        let mut state_ref = state.borrow_mut();
        let Some(pending) = state_ref.pending_handoff.as_ref() else {
            return;
        };
        let outcome = classify_close_signal(&pending.closed_rx);
        if outcome == HandoffCloseOutcome::StillClosing {
            return;
        }
        let pending = state_ref.pending_handoff.take().expect("checked Some above");
        state_ref.reported_start_failure = false;
        state_ref.turn_trace = None;
        if outcome == HandoffCloseOutcome::Closed {
            // Into the Rust host, not only down the wire. See `AgentPanelState::last_handoff` for
            // why: the panel's own reload affordance would otherwise destroy the only copy, and on
            // the default legacy backend nothing else anywhere remembers this session id.
            state_ref.last_handoff = Some(pending.command.clone());
        }
        Some((pending, outcome))
    };

    let Some((pending, outcome)) = finished else { return };
    if outcome == HandoffCloseOutcome::CloseFailed {
        eprintln!("[agent_panel] {HANDOFF_CLOSE_FAILED_MESSAGE}");
    }
    for payload in handoff_payloads(&outcome, &pending.request_id, &pending.command) {
        evaluate_js_dispatch(webview, &payload);
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

/// The envelopes owed for a handoff whose close reached `outcome`, in dispatch order.
///
/// The property worth stating: only `Closed` ever produces a `handoff` envelope. A close that is
/// still running, or one that died partway, must not be followed by a card whose first line claims
/// the conversation is closed.
fn handoff_payloads(
    outcome: &HandoffCloseOutcome,
    request_id: &str,
    command: &agent::handoff::ClaudeResumeCommand,
) -> Vec<String> {
    match outcome {
        HandoffCloseOutcome::StillClosing => Vec::new(),
        HandoffCloseOutcome::Closed => vec![
            neovibe_core::agent_bridge::serialize_handoff_for_js(command),
            serialize_command_result_for_js(request_id, Ok(())),
        ],
        // The session is gone regardless -- it was handed to the worker before this. Saying so is
        // the only honest option: an `error` envelope returns the frontend to the start screen,
        // where a failed handoff reads as a dead session rather than as a conversation that is
        // still there.
        HandoffCloseOutcome::CloseFailed => vec![
            serialize_command_result_for_js(request_id, Err(HANDOFF_CLOSE_FAILED_MESSAGE)),
            serialize_error_for_js(HANDOFF_CLOSE_FAILED_MESSAGE),
        ],
    }
}

/// Everything a freshly-mounted document is owed, in the order it must arrive.
///
/// A pure function over plain data, for the same reason `SnapshotView` is one: the reload path has
/// no `WebView`-free test otherwise, and "what does a reloaded panel get back" is exactly the list
/// a later change loses something from without any test noticing.
///
/// `hello` comes first: it tells the frontend which backend it is talking to and what that backend
/// genuinely offers, which is what the start screen renders. `theme`, when given, comes second --
/// before the snapshot or handoff card that would be drawn with it. The `Ready` handler always
/// gives one, built from the panel's recorded tokens.
fn ready_payloads(
    mut greeting: BackendGreeting,
    snapshot: Option<String>,
    last_handoff: Option<&agent::handoff::ClaudeResumeCommand>,
    theme: Option<&str>,
) -> Vec<String> {
    // A session this panel just handed to a terminal must not also be offered for resume on the
    // start screen: nothing holds a lock on it, so taking the offer would put two writers on one
    // transcript -- which is the exact hazard the handoff card warns about. This has to happen here
    // rather than only in the frontend, because `hello` is recomputed on every mount and the
    // handed-over session has the greatest `updated_at`, so it would otherwise head the list.
    //
    // It removes exactly that one row. Clearing the whole offer would hide every other session the
    // workspace remembers, which is a different and worse bug than the one this prevents.
    if let Some(command) = last_handoff {
        greeting
            .resumable
            .retain(|r| r.provider_session_id != command.provider_session_id());
    }

    let mut payloads = vec![serialize_hello_for_js(&greeting)];
    // Colours before anything drawn with them.
    if let Some(theme) = theme {
        payloads.push(theme.to_string());
    }
    match snapshot {
        // A live session wins. `collect_pending_start` clears `last_handoff` when one is installed,
        // so a stale card and a live conversation cannot both be current -- but this ordering does
        // not depend on that being right, which is the point of writing it as an either/or.
        Some(snapshot) => payloads.push(snapshot),
        // No session yet is not an error: the frontend shows its start screen. If this panel closed
        // a conversation into a terminal, the command for it belongs on that screen -- including
        // after a reload, which is the case this whole path exists for.
        None => {
            if let Some(command) = last_handoff {
                payloads.push(neovibe_core::agent_bridge::serialize_handoff_for_js(command));
            }
        }
    }
    payloads
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

/// Applies one command's outcome to the panel and the frontend, uniformly.
///
/// The benign/fatal split is the whole point: a benign failure (a turn sent while one was running,
/// a permission answered twice) reports itself and leaves the session alone, while a fatal one
/// tears the session down and tells the frontend the whole session is gone. Before this existed the
/// same three-branch shape was copy-pasted at every call site, which is how the two classes drifted.
fn apply_command_outcome(
    state: &Rc<RefCell<AgentPanelState>>,
    webview: &WebView,
    request_id: &str,
    outcome: Result<Vec<agent::AgentDomainEvent>, neovibe_core::agent_backend::BackendError>,
) {
    let events = events_owed(&outcome);
    if !events.is_empty() {
        // Only the legacy backend ever gets here with a non-empty batch: it synthesizes events its
        // own wire protocol cannot provide. The sidecar backend returns an empty vec and its state
        // arrives through the pump, from the server.
        let state_ref = state.borrow();
        let through_revision = state_ref
            .session
            .as_ref()
            .map(|s| s.projection().last_revision)
            .unwrap_or(0);
        let from_revision = through_revision.saturating_sub(events.len() as u64);
        drop(state_ref);
        evaluate_js_dispatch(
            webview,
            &serialize_events_for_js(from_revision, through_revision, events),
        );
    }
    match outcome {
        Ok(_) => {
            evaluate_js_dispatch(webview, &serialize_command_result_for_js(request_id, Ok(())));
        }
        Err(error) if error.benign => {
            eprintln!(
                "[agent_panel] command rejected (session stays alive): {}",
                error.message
            );
            evaluate_js_dispatch(
                webview,
                &serialize_command_result_for_js(request_id, Err(&error.message)),
            );
        }
        Err(error) => {
            eprintln!("[agent_panel] command failed fatally: {}", error.message);
            // Retired on a worker thread, for the same reason construction runs on one. Two separate
            // problems with doing it here: `AgentConversation` has no `Drop`, so a bare
            // `session = None` never sends `close_session` at all and the sidecar is left holding a
            // session it thinks is live; and the drop chain that DOES run (SpawnedSidecar polls for
            // up to 3s before escalating to SIGKILL, then RuntimeThread joins its Tokio thread)
            // would block the whole shell -- editor pane included -- on the GTK main loop.
            //
            // Calling `shutdown()` inline instead would be worse, not better: it issues
            // `close_session` through a unary RPC bounded at 10s, which is precisely the timeout
            // case that gets here in the first place.
            //
            // Two statements, not `if let Some(..) = state.borrow_mut().session.take()`: the latter
            // keeps the `RefMut` alive across the whole body.
            let attention_before = state.borrow().attention.attention();
            let on_screen = webview.is_mapped();
            let dead = {
                let mut state_ref = state.borrow_mut();
                // The cards of a session that is gone can no longer be answered (the whole-branch
                // review's finding 2: a hidden chat kept reading `agent ⚑N`, and `Ctrl+a a` landed
                // on a card nobody could answer).
                state_ref.attention.session_ended(on_screen);
                state_ref.session.take()
            };
            if let Some(mut backend) = dead {
                std::thread::spawn(move || backend.shutdown());
            }
            evaluate_js_dispatch(
                webview,
                &serialize_command_result_for_js(request_id, Err(&error.message)),
            );
            evaluate_js_dispatch(webview, &serialize_error_for_js(&error.message));
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
    let request_id = message.request_id().to_string();

    match message {
        InboundMessage::Ready { .. } => {
            // Everything a fresh document is owed is decided by one pure function, so the reload
            // path is testable without a WebView -- see `ready_payloads`.
            let payloads = {
                let state_ref = state.borrow();
                let greeting = BackendGreeting::for_kind(state_ref.backend_kind, state_ref.project_dir.clone());
                let snapshot = state_ref
                    .session
                    .as_ref()
                    .map(|b| serialize_snapshot_for_js(&SnapshotView::of(b)));
                let theme = neovibe_core::agent_bridge::serialize_theme_for_js(&state_ref.theme);
                let mut payloads = ready_payloads(greeting, snapshot, state_ref.last_handoff.as_ref(), Some(&theme));
                // Last, so nothing the document draws from the payloads above can reset it.
                payloads.push(neovibe_core::agent_bridge::serialize_pane_focus_for_js(
                    state_ref.pane_focused,
                ));
                payloads
            };
            for payload in payloads {
                evaluate_js_dispatch(webview, &payload);
            }
            evaluate_js_dispatch(webview, &serialize_command_result_for_js(&request_id, Ok(())));
        }
        InboundMessage::StartSession { mode, resume, .. } => {
            let mut state_ref = state.borrow_mut();
            if state_ref.session.is_some() {
                drop(state_ref);
                evaluate_js_dispatch(
                    webview,
                    &serialize_command_result_for_js(&request_id, Err("a session already exists")),
                );
                return;
            }
            if state_ref.pending_start.is_some() {
                // A second start while one is already connecting. Rejecting is right: accepting
                // would spawn a second sidecar process whose result would overwrite the first,
                // leaking it.
                drop(state_ref);
                evaluate_js_dispatch(
                    webview,
                    &serialize_command_result_for_js(&request_id, Err("a session is already starting")),
                );
                return;
            }
            if state_ref.pending_handoff.is_some() {
                // `session` is already `None` at this point (the handoff took it), so nothing above
                // catches this. Starting here would spawn a second `claude` alongside the one still
                // being closed, in the same project.
                drop(state_ref);
                evaluate_js_dispatch(
                    webview,
                    &serialize_command_result_for_js(
                        &request_id,
                        Err("the previous session is still being handed off to a terminal"),
                    ),
                );
                return;
            }

            let backend_kind = state_ref.backend_kind;
            let project_dir = state_ref.project_dir.clone();
            let permission_mode: agent::PermissionMode = mode.into();

            // Off the GTK main loop. Constructing the sidecar backend spawns a real process, does a
            // real gRPC handshake, and on a cold Verdandi checkout runs `npm ci` + `npm run build`.
            // Doing that here would freeze the editor -- for minutes, in the cold case.
            let (result_tx, result_rx) = mpsc::channel();
            std::thread::spawn(move || {
                let result = AgentBackend::start(backend_kind, &project_dir, permission_mode, resume.as_deref());
                // The receiver is gone only if the panel was torn down mid-connect; dropping the
                // backend here is then the correct cleanup (its Drop shuts the sidecar down).
                let _ = result_tx.send(result);
            });
            state_ref.pending_start = Some(PendingStart { request_id, result_rx });
            // No command_result yet -- it is owed once the worker reports, and `collect_pending_start`
            // on the next tick is what pays it.
        }
        InboundMessage::SendMessage { text, .. } => {
            let outcome = {
                let mut state_ref = state.borrow_mut();
                // wire 1's one composition point. Above both backends on purpose: the turn's String
                // travels unmodified from here to `send_turn` on either path, so composing here
                // gives legacy the feature for free and changes no wire format. The context is read
                // NOW rather than remembered, and a `None` means the turn goes out exactly as the
                // user typed it.
                let composed =
                    neovibe_core::editor_context::compose_turn_text(&text, (state_ref.editor_context)().as_ref());
                // Stamped before the call, so the trace's zero is the user's action rather than the
                // moment the backend got around to accepting it.
                state_ref.turn_trace = neovibe_core::turn_trace::TurnTrace::start();
                match state_ref.session.as_mut() {
                    // `&text` second, and it is the user's own: the panel shows what was typed,
                    // never the composed wire text.
                    Some(session) => session.send_turn(&composed, &text),
                    None => Err(no_session_error()),
                }
            };
            apply_command_outcome(state, webview, &request_id, outcome);
        }
        InboundMessage::Interrupt { .. } => {
            let outcome = {
                let mut state_ref = state.borrow_mut();
                match state_ref.session.as_mut() {
                    Some(session) => session.interrupt(),
                    None => Err(no_session_error()),
                }
            };
            apply_command_outcome(state, webview, &request_id, outcome);
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
            let mut state_ref = state.borrow_mut();
            if let Some(trace) = state_ref.turn_trace.as_mut() {
                trace.mark_painted(receive_to_frame_ms);
                if trace.is_complete() {
                    trace.emit();
                }
            }
        }
        InboundMessage::HandoffToTerminal { .. } => {
            // The command is built from canonical state FIRST, while the session is still readable
            // -- ownership moves to the shutdown worker below and nothing here can read it after
            // that. Owned values rather than borrows because `prepare_handoff` is a pure function
            // and the `RefCell` borrow must not outlive this block.
            let (project_dir, session_facts, already_handing_off) = {
                let state_ref = state.borrow();
                let facts = state_ref.session.as_ref().map(|backend| {
                    // `provider_session_id()` BEFORE `projection()`, and the order is load-bearing
                    // rather than stylistic. On the sidecar path both reach for the SAME
                    // `Mutex<IngestState>` -- `projection()` returns a guard that holds it, and
                    // `provider_session_id()` locks it again. `std::sync::Mutex` is not reentrant,
                    // so calling the second inside the first's scope deadlocks the GTK main loop
                    // outright: no clicks, no keys, not even a compositor close request, with the
                    // last frame still painted so it looks alive. Reproduced 3/3 on 2026-09-15 and
                    // confirmed by backtrace; the legacy path never showed it because its
                    // `projection()` borrows a plain field and takes no lock at all.
                    let provider_session_id = backend.provider_session_id();
                    let projection = backend.projection();
                    (
                        provider_session_id,
                        projection.active_turn_id.clone(),
                        projection.cwd.clone(),
                        crate::terminal_handoff::ConversationLiveness::of(&projection.status),
                    )
                });
                (
                    state_ref.project_dir.clone(),
                    facts,
                    state_ref.pending_handoff.is_some(),
                )
            };
            if already_handing_off {
                evaluate_js_dispatch(
                    webview,
                    &serialize_command_result_for_js(&request_id, Err("this conversation is already being handed off")),
                );
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
                    evaluate_js_dispatch(
                        webview,
                        &serialize_command_result_for_js(&request_id, Err(&refusal.message())),
                    );
                    return;
                }
            };

            // Design doc §8.3 steps 1-5, in the only order that is safe: from here on this panel
            // holds no session, so no further turn can be sent, and `shutdown()` itself cancels
            // every pending permission (fail-closed) and closes the provider's session.
            // Two statements, not one `let ... else`: the same reason `apply_command_outcome`'s
            // fatal branch is written this way -- it keeps the `RefMut` from being alive during
            // whatever the failure branch does.
            let taken = state.borrow_mut().session.take();
            let Some(mut backend) = taken else {
                // `prepare_handoff` already returned `NoSession` for this, so it is unreachable
                // unless something took the session between that call and this line -- both run on
                // the GTK main loop, so nothing can. Reported rather than unwrapped anyway.
                evaluate_js_dispatch(
                    webview,
                    &serialize_command_result_for_js(
                        &request_id,
                        Err("the conversation ended before it could be handed off"),
                    ),
                );
                return;
            };
            let (closed_tx, closed_rx) = mpsc::channel();
            let (dropped_tx, dropped_rx) = mpsc::channel();
            std::thread::spawn(move || {
                backend.shutdown();
                // Sent after `shutdown()` returns, which is the whole signal: the command is not
                // dispatched until this arrives. A panic inside `shutdown()` drops the sender
                // instead, which `collect_pending_handoff` reads as a failed close.
                let _ = closed_tx.send(());
                // Then the drop, which is a separate and later event: `shutdown()` issues
                // `close_session` and joins ingestion, while it is `SpawnedSidecar::drop` that ends
                // the child. The command is deliberately NOT made to wait for it -- the user has
                // been told the conversation is closed in Neovibe, which `close_session` makes
                // true -- but a window closing at this moment is, so it gets its own signal.
                drop(backend);
                let _ = dropped_tx.send(());
            });
            state.borrow_mut().pending_handoff = Some(PendingHandoff {
                request_id,
                command,
                closed_rx,
                dropped_rx,
            });
            // No command_result yet -- `collect_pending_handoff` owes it once the close finishes.
        }
        InboundMessage::PermissionResponse {
            permission_id,
            decision,
            reason,
            ..
        } => {
            let decision = decision.into_decision(reason);
            let outcome = {
                let mut state_ref = state.borrow_mut();
                match state_ref.session.as_mut() {
                    Some(session) => session.respond_permission(&permission_id, decision),
                    None => Err(no_session_error()),
                }
            };
            apply_command_outcome(state, webview, &request_id, outcome);
        }
    }
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

/// Installs `app.reload-agent-panel` and binds Ctrl+Shift+R to it, for the panel `handle` owns.
///
/// The affordance deliberately lives OUTSIDE the WebView. A reload button drawn by the panel's own
/// page would be drawn by the very thing that has stopped responding -- missing exactly when it is
/// needed -- and until now a wedged panel could only be recovered by closing the window, which also
/// takes the editor, the nvim child and the agent session with it.
///
/// A `gio::SimpleAction` for the same two reasons `toggle-terminal-view` is one: an app-level accel
/// runs in the window's capture phase regardless of which pane holds focus, and a verification
/// harness can drive the identical path with `app.activate_action("reload-agent-panel", None)` and
/// no synthetic chord -- which matters given `wtype`'s documented trouble delivering
/// `Ctrl+Shift+<letter>`. The cost, stated plainly: an app accel never reaches Neovim, so
/// `<C-S-r>` stops being available to the user's own config -- the same trade already made for
/// Ctrl+Shift+S, and not yet checked against the owner's real config.
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
    app.set_accels_for_action("app.reload-agent-panel", &["<Control><Shift>r"]);
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let payloads = ready_payloads(legacy_greeting(), None, Some(&command), None);
        let hello: serde_json::Value = serde_json::from_str(&payloads[0]).unwrap();
        assert!(
            hello["resumableSession"].is_null(),
            "legacy has no other record of this session"
        );

        assert_eq!(kinds(&payloads), vec!["hello", "handoff"]);
        let handoff: serde_json::Value = serde_json::from_str(&payloads[1]).unwrap();
        assert_eq!(
            handoff["command"],
            "cd /home/user/project && claude --resume 1857dcd5-973b-46a2"
        );
        assert_eq!(handoff["providerSessionId"], "1857dcd5-973b-46a2");
    }

    /// The ordinary case is unchanged: a panel that never handed anything off sends `hello` alone.
    #[test]
    fn a_panel_that_handed_nothing_off_sends_only_the_greeting() {
        assert_eq!(
            kinds(&ready_payloads(legacy_greeting(), None, None, None)),
            vec!["hello"]
        );
    }

    /// A reloaded document must get its colours back before anything it would draw with them.
    #[test]
    fn the_theme_follows_the_greeting_and_precedes_everything_else() {
        let theme = neovibe_core::agent_bridge::serialize_theme_for_js(&neovibe_core::theme::ThemeTokens::fallback());
        assert_eq!(
            kinds(&ready_payloads(legacy_greeting(), None, None, Some(&theme))),
            vec!["hello", "theme"]
        );
        let payloads = ready_payloads(
            legacy_greeting(),
            Some(r#"{"kind":"snapshot","throughRevision":3,"state":{}}"#.to_string()),
            None,
            Some(&theme),
        );
        assert_eq!(kinds(&payloads), vec!["hello", "theme", "snapshot"]);
    }

    /// A live session wins. The card describes a conversation that is over; putting it above a
    /// running one would read as that one being closed.
    #[test]
    fn a_live_session_is_sent_instead_of_a_stale_handoff_card() {
        let command = a_command();
        let payloads = ready_payloads(
            legacy_greeting(),
            Some(r#"{"kind":"snapshot","throughRevision":3,"state":{}}"#.to_string()),
            Some(&command),
            None,
        );
        assert_eq!(kinds(&payloads), vec!["hello", "snapshot"]);
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
                },
                agent::ResumableSession {
                    provider: "claude".to_string(),
                    provider_session_id: "some-other-session".to_string(),
                    created_at: "1757500000000".to_string(),
                    updated_at: "1757600000000".to_string(),
                    title: None,
                },
            ],
        };
        let command = a_command();
        let payloads = ready_payloads(greeting, None, Some(&command), None);
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
        assert!(handoff_payloads(&HandoffCloseOutcome::StillClosing, "req-1", &a_command()).is_empty());

        closed_tx.send(()).unwrap();
        assert_eq!(classify_close_signal(&closed_rx), HandoffCloseOutcome::Closed);
        assert_eq!(
            kinds(&handoff_payloads(&HandoffCloseOutcome::Closed, "req-1", &a_command())),
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

        let payloads = handoff_payloads(&HandoffCloseOutcome::CloseFailed, "req-2", &a_command());
        assert_eq!(kinds(&payloads), vec!["command_result", "error"]);
        assert!(
            !payloads.iter().any(|p| p.contains("1857dcd5-973b-46a2")),
            "a failed close leaked the resume command anyway: {payloads:?}"
        );
    }
}
