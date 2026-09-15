//! The real agent-ui panel: a `WebView` hosting the embedded `agent-ui/web` frontend, a
//! `UserContentManager` script-message bridge (JS -> Rust), and a fast poll of the backend's
//! `pump()` pushed to the page via `evaluate_javascript` (Rust -> JS). See
//! docs/superpowers/specs/2026-09-07-agent-ui-design.md.
//!
//! Backend construction is lazy: this module starts with `None` and only builds one once the
//! frontend sends `"start_session"` -- `PermissionMode` is a construction-time-only choice on the
//! real `agent` API (no live mode-switch exists), so the frontend must choose before any real
//! subprocess is spawned. Which backend gets built is `crate::agent_backend`'s decision.
//!
//! **State is server-originated.** For the sidecar backend this module folds nothing of its own:
//! `active_turn_id`, tool calls and permissions all arrive as real events through `pump()`. A
//! command that succeeds returns no events at all; the UI updates on the next poll. Nothing here
//! synthesizes an optimistic `TurnStarted` to make the UI feel faster -- doing so would put the
//! panel's idea of "a turn is running" ahead of the server's, which is the exact shadow state the
//! runtime design forbids.

use crate::agent_backend::{AgentBackend, BackendGreeting, BackendKind};
use agent::UiDelivery;
use crate::agent_bridge::{
    parse_inbound_message, serialize_command_result_for_js, serialize_error_for_js, serialize_events_for_js,
    serialize_hello_for_js, serialize_snapshot_for_js, InboundMessage, SnapshotView,
};
use gtk4::prelude::*;
use gtk4::Application;
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

struct AgentPanelState {
    session: Option<AgentBackend>,
    backend_kind: BackendKind,
    project_dir: PathBuf,
    supervisor: Option<crate::supervisor_client::SupervisorClient>,
    /// Set once a start-failure has been reported, so the 33ms tick reports it exactly once rather
    /// than every tick for as long as the dead session is installed.
    reported_start_failure: bool,
    /// Set while a backend is being constructed on a worker thread. Holds the requestId whose
    /// `command_result` is owed once that finishes -- the reply is deferred, not dropped, which is
    /// exactly what a requestId-addressed protocol is for.
    pending_start: Option<PendingStart>,
    /// The in-flight turn's latency marks, when `NEOVIBE_AGENT_TRACE=1`. `None` the rest of the
    /// time, which is every normal run -- this is a diagnostic, not a metrics pipeline.
    turn_trace: Option<crate::turn_trace::TurnTrace>,
}

/// An in-flight backend construction. Constructing a sidecar backend spawns a real process, does a
/// real gRPC handshake, and on a cold Verdandi checkout runs `npm ci` + `npm run build` -- minutes
/// of blocking work. Doing that on the GTK main loop would freeze the whole editor, so it happens
/// on a worker thread and the result is collected by a poll on the main loop.
struct PendingStart {
    request_id: String,
    result_rx: mpsc::Receiver<Result<AgentBackend, crate::agent_backend::BackendError>>,
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
    pub(crate) fn shutdown(&self) {
        let pending = self.state.borrow_mut().pending_start.take();
        if let Some(mut session) = self.state.borrow_mut().session.take() {
            session.shutdown();
        }
        if let Some(pending) = pending {
            // Bounded, and blocking on purpose: this runs on the window-close path, where waiting
            // for a clean teardown is the whole point. The worker is either about to finish or has
            // already failed; three seconds is well past a warm connect and well short of a hang.
            match pending.result_rx.recv_timeout(std::time::Duration::from_secs(3)) {
                Ok(Ok(mut backend)) => {
                    eprintln!("[agent_panel] window closed mid-connect; shutting down the backend that finished anyway");
                    backend.shutdown();
                }
                Ok(Err(e)) => eprintln!("[agent_panel] window closed mid-connect; the backend had already failed: {}", e.message),
                Err(_) => eprintln!(
                    "[agent_panel] window closed mid-connect and the worker did not report within 3s; \
                     any backend it produces will be dropped with its channel"
                ),
            }
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
        self.webview.load_html(AGENT_UI_HTML, None);
    }

    /// Non-blocking: returns `true` if `neovibe-supervisor` asked this window to come to the
    /// front since the last call. `main.rs` polls this on its own timer and calls
    /// `window.present()` in response -- this handle has no `Window` reference of its own (see
    /// `AgentPanelHandle`'s own top-level doc: window lifecycle stays owned by `main.rs`).
    pub(crate) fn poll_activate(&self) -> bool {
        if let Some(supervisor) = self.state.borrow_mut().supervisor.as_mut() {
            supervisor.poll_activate()
        } else {
            false
        }
    }
}

pub(crate) fn build_agent_panel(project_dir: PathBuf) -> (gtk4::Widget, AgentPanelHandle) {
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
                                gtk4::UriLauncher::new(&uri).launch(None::<&gtk4::Window>, None::<&gtk4::gio::Cancellable>, |_| {});
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
    let supervisor = crate::supervisor_client::SupervisorClient::connect_or_spawn(instance_id, project_name, &project_dir);
    let backend_kind = BackendKind::from_env();
    println!("[agent_panel] backend: {}", backend_kind.as_str());
    let state = Rc::new(RefCell::new(AgentPanelState {
        session: None,
        backend_kind,
        project_dir,
        supervisor,
        pending_start: None,
        reported_start_failure: false,
        turn_trace: None,
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

    webview.load_html(AGENT_UI_HTML, None);

    // Started once, at construction, rather than when a session starts: it is also what collects a
    // finished background connect. Previously it was started inside the start_session handler,
    // which meant a second start attempt after a failure installed a SECOND timer on the same
    // state -- every later event would then be dispatched twice.
    start_pump_timer(state.clone(), webview.clone());

    let handle = AgentPanelHandle { state: state.clone(), webview: webview.clone() };
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
        collect_pending_start(&state, &webview);

        // The payload is built under the borrow and dispatched OUTSIDE it. Calling into WebKit while
        // holding a `RefMut` on the panel's own state is a latent re-entrancy hazard: the script
        // message handler takes the same `RefCell`, and anything that let it run during the
        // dispatch would panic on an already-borrowed cell rather than fail gracefully. Nothing
        // today re-enters, which is exactly why it would stay latent until it didn't.
        report_a_session_that_never_opened(&state, &webview);

        let (payload, first_text_in_this_batch) = {
            let mut state_ref = state.borrow_mut();
            let AgentPanelState { session, turn_trace, supervisor, .. } = &mut *state_ref;
            // Folded from the SAME slice the bridge is about to serialize, so the trace describes
            // the events the user is actually about to see rather than a parallel accounting.
            let mut first_text_in_this_batch = false;
            let payload = session.as_mut().and_then(|session| {
                let from_revision = session.projection().last_revision;
                match session.take_ui_delivery() {
                    UiDelivery::Nothing => None,
                    UiDelivery::Events(events) => {
                        if let Some(trace) = turn_trace.as_mut() {
                            first_text_in_this_batch = trace.observe(&events);
                        }
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
                        let view = SnapshotView::of(session);
                        Some(serialize_snapshot_for_js(&view))
                    }
                }
            });
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
    let dead = {
        let mut state_ref = state.borrow_mut();
        state_ref.reported_start_failure = true;
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
        let Some(pending) = state_ref.pending_start.as_ref() else { return };
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
                    Err(crate::agent_backend::BackendError {
                        message: "the backend connect worker stopped without reporting a result".to_string(),
                        benign: false,
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
            evaluate_js_dispatch(webview, &serialize_command_result_for_js(&request_id, Err(&error.message)));
            evaluate_js_dispatch(webview, &serialize_error_for_js(&error.message));
        }
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
    outcome: Result<Vec<agent::AgentDomainEvent>, crate::agent_backend::BackendError>,
) {
    match outcome {
        Ok(events) => {
            if !events.is_empty() {
                // Only the legacy backend ever gets here with a non-empty batch: it synthesizes
                // events its own wire protocol cannot provide. The sidecar backend returns an empty
                // vec and its state arrives through the pump, from the server.
                let state_ref = state.borrow();
                let through_revision = state_ref.session.as_ref().map(|s| s.projection().last_revision).unwrap_or(0);
                let from_revision = through_revision.saturating_sub(events.len() as u64);
                drop(state_ref);
                evaluate_js_dispatch(webview, &serialize_events_for_js(from_revision, through_revision, &events));
            }
            evaluate_js_dispatch(webview, &serialize_command_result_for_js(request_id, Ok(())));
        }
        Err(error) if error.benign => {
            eprintln!("[agent_panel] command rejected (session stays alive): {}", error.message);
            evaluate_js_dispatch(webview, &serialize_command_result_for_js(request_id, Err(&error.message)));
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
            let dead = state.borrow_mut().session.take();
            if let Some(mut backend) = dead {
                std::thread::spawn(move || backend.shutdown());
            }
            evaluate_js_dispatch(webview, &serialize_command_result_for_js(request_id, Err(&error.message)));
            evaluate_js_dispatch(webview, &serialize_error_for_js(&error.message));
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
            let (greeting_payload, snapshot_payload) = {
                let state_ref = state.borrow();
                let greeting = BackendGreeting::for_kind(state_ref.backend_kind, state_ref.project_dir.clone());
                // No session yet is not an error -- the frontend shows its start screen in that
                // case and does not expect a snapshot until one exists.
                let snapshot = state_ref.session.as_ref().map(|b| serialize_snapshot_for_js(&SnapshotView::of(b)));
                (serialize_hello_for_js(&greeting), snapshot)
            };
            // `hello` FIRST: it tells the frontend which backend it is talking to and which
            // permission policies are genuinely on offer, which is what its start screen renders.
            evaluate_js_dispatch(webview, &greeting_payload);
            if let Some(payload) = snapshot_payload {
                evaluate_js_dispatch(webview, &payload);
            }
            evaluate_js_dispatch(webview, &serialize_command_result_for_js(&request_id, Ok(())));
        }
        InboundMessage::StartSession { mode, resume, .. } => {
            let mut state_ref = state.borrow_mut();
            if state_ref.session.is_some() {
                drop(state_ref);
                evaluate_js_dispatch(webview, &serialize_command_result_for_js(&request_id, Err("a session already exists")));
                return;
            }
            if state_ref.pending_start.is_some() {
                // A second start while one is already connecting. Rejecting is right: accepting
                // would spawn a second sidecar process whose result would overwrite the first,
                // leaking it.
                drop(state_ref);
                evaluate_js_dispatch(webview, &serialize_command_result_for_js(&request_id, Err("a session is already starting")));
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
                // Stamped before the call, so the trace's zero is the user's action rather than the
                // moment the backend got around to accepting it.
                state_ref.turn_trace = crate::turn_trace::TurnTrace::start();
                match state_ref.session.as_mut() {
                    Some(session) => session.send_turn(&text),
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
        InboundMessage::TurnRendered { receive_to_frame_ms, .. } => {
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
        InboundMessage::PermissionResponse { permission_id, decision, reason, .. } => {
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
fn no_session_error() -> crate::agent_backend::BackendError {
    crate::agent_backend::BackendError { message: "no active session".to_string(), benign: true }
}

fn evaluate_js_dispatch(webview: &WebView, json_payload: &str) {
    let script = format!("window.__neovibeDispatch({});", serde_json::to_string(json_payload).unwrap_or_default());
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
}
