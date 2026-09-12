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
use crate::agent_bridge::{
    parse_inbound_message, serialize_command_result_for_js, serialize_error_for_js, serialize_events_for_js,
    serialize_hello_for_js, serialize_snapshot_for_js, InboundMessage, SnapshotView,
};
use gtk4::prelude::*;
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

    let handle = AgentPanelHandle { state: state.clone() };
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

        let payload = {
            let mut state_ref = state.borrow_mut();
            let payload = state_ref.session.as_mut().and_then(|session| {
                let from_revision = session.projection().last_revision;
                let events = session.pump();
                if events.is_empty() {
                    return None;
                }
                let through_revision = session.projection().last_revision;
                Some(serialize_events_for_js(from_revision, through_revision, &events))
            });
            let status = crate::supervisor_client::derive_status(state_ref.session.as_ref().map(|s| s.projection()));
            if let Some(supervisor) = state_ref.supervisor.as_mut() {
                supervisor.send_status(status);
            }
            payload
        };
        if let Some(payload) = payload {
            evaluate_js_dispatch(&webview, &payload);
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
                serialize_snapshot_for_js(&SnapshotView::of(state_ref.session.as_ref().unwrap()))
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
