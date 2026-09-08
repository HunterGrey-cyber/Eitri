//! The real agent-ui panel: a `WebView` hosting the embedded `agent-ui/web` frontend, a
//! `UserContentManager` script-message bridge (JS -> Rust), and a fast poll of
//! `AgentSession::pump()` pushed to the page via `evaluate_javascript` (Rust -> JS). See
//! docs/superpowers/specs/2026-09-07-agent-ui-design.md.
//!
//! `AgentSession` construction is lazy: this module starts with `None` and only calls
//! `AgentSession::start(...)` once the frontend sends `"start_session"` -- `PermissionMode` is a
//! construction-time-only choice on the real `agent` API (no live mode-switch exists), so the
//! frontend must choose before any real subprocess is spawned.

use crate::agent_bridge::{
    parse_inbound_message, serialize_error_for_js, serialize_event_for_js, serialize_snapshot_for_js, InboundMessage,
};
use agent::{AgentSession, CONSERVATIVE_DISALLOWED_TOOLS};
use gtk4::prelude::*;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
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
    session: Option<AgentSession>,
    project_dir: PathBuf,
}

/// A handle back into this panel's session state, held by `main.rs` alongside the `gtk4::Widget`
/// so a real window close can shut down whatever `AgentSession` exists -- without this, the
/// `Rc<RefCell<AgentPanelState>>` that owns the session is only ever reachable from closures
/// internal to this module (the script-message handler and the pump timer), neither of which
/// ever runs `AgentSession::shutdown()` on its own, so a normal window close would otherwise
/// leak the child process, its hook socket, and any in-memory-only settings backup.
pub(crate) struct AgentPanelHandle {
    state: Rc<RefCell<AgentPanelState>>,
}

impl AgentPanelHandle {
    /// No-op if no session was ever started -- a user who never leaves the mode-selector screen
    /// before closing the window has nothing to shut down.
    pub(crate) fn shutdown(&self) {
        if let Some(session) = self.state.borrow_mut().session.as_mut() {
            session.shutdown();
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

    let state = Rc::new(RefCell::new(AgentPanelState { session: None, project_dir }));

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

    let handle = AgentPanelHandle { state: state.clone() };
    (webview.upcast(), handle)
}

/// Starts the fast poll of `AgentSession::pump()`, pushing each drained event to the page via
/// `evaluate_javascript`. Called exactly once, from the `StartSession` success arm below -- not
/// from `build_agent_panel` -- so a panel that never gets a session (the user leaves the mode
/// selector up, or a Lua plugin replaces this whole Side-slot widget before a session is ever
/// started) never leaves a ticking timer running against an abandoned `WebView`.
fn start_pump_timer(state: Rc<RefCell<AgentPanelState>>, webview: WebView) {
    gtk4::glib::timeout_add_local(std::time::Duration::from_millis(PUMP_POLL_INTERVAL_MS), move || {
        let mut state_ref = state.borrow_mut();
        if let Some(session) = state_ref.session.as_mut() {
            for event in session.pump() {
                let payload = serialize_event_for_js(&event);
                evaluate_js_dispatch(&webview, &payload);
            }
        }
        gtk4::glib::ControlFlow::Continue
    });
}

fn handle_inbound_message(raw: &str, state: &Rc<RefCell<AgentPanelState>>, webview: &WebView) {
    let Some(message) = parse_inbound_message(raw) else {
        // Already logged inside parse_inbound_message -- an unparseable/unrecognized message
        // from the WebView must never crash the shell process.
        return;
    };

    match message {
        InboundMessage::Ready => {
            let state_ref = state.borrow();
            if let Some(session) = &state_ref.session {
                let payload = serialize_snapshot_for_js(&session.state);
                evaluate_js_dispatch(webview, &payload);
            }
            // No session yet is not an error -- the frontend's own App.tsx shows the mode
            // selector in that case and doesn't expect a snapshot until one exists.
        }
        InboundMessage::StartSession { mode } => {
            let mut state_ref = state.borrow_mut();
            if state_ref.session.is_some() {
                eprintln!("[agent_panel] start_session received but a session already exists -- ignoring");
                return;
            }
            let project_dir = state_ref.project_dir.clone();
            match AgentSession::start(&project_dir, mode.into(), CONSERVATIVE_DISALLOWED_TOOLS) {
                Ok(session) => {
                    state_ref.session = Some(session);
                    drop(state_ref);
                    // Only started on a real successful session start -- see start_pump_timer's
                    // own doc for why this can't live in build_agent_panel instead. The guard
                    // above (an existing session returns early) is also what keeps this a
                    // once-only call: StartSession can only ever reach this Ok branch once per
                    // panel lifetime.
                    start_pump_timer(state.clone(), webview.clone());
                }
                Err(e) => {
                    eprintln!("[agent_panel] failed to start AgentSession: {e}");
                    // The frontend set `sessionStarted` optimistically the moment the mode
                    // button was clicked, before Rust ever confirmed anything -- without this,
                    // a failed spawn (e.g. `claude` not on PATH) leaves it stranded on an empty
                    // conversation view with no session and no way back. `state_ref` must be
                    // dropped before this call: evaluate_js_dispatch doesn't need it, and
                    // holding a live borrow across it would be needless (if harmless here).
                    drop(state_ref);
                    evaluate_js_dispatch(webview, &serialize_error_for_js(&format!("failed to start session: {e}")));
                }
            }
        }
        InboundMessage::SendMessage { text } => {
            let mut state_ref = state.borrow_mut();
            match state_ref.session.as_mut().map(|session| session.send_turn(&text)) {
                Some(Ok(())) => {}
                Some(Err(e)) => {
                    eprintln!("[agent_panel] send_turn failed: {e}");
                    // A failed send_turn (BrokenPipe once the child has exited) would otherwise
                    // leave the frontend's optimistically-set turnInProgress stuck true forever
                    // with no recovery -- surface it and drop the dead session so a fresh
                    // "start_session" can actually succeed (the StartSession arm above ignores a
                    // request while `session` is still Some, even though the process behind it is
                    // already gone).
                    state_ref.session = None;
                    drop(state_ref);
                    evaluate_js_dispatch(webview, &serialize_error_for_js(&format!("failed to send message: {e}")));
                }
                None => {
                    eprintln!("[agent_panel] send_message received with no active session -- ignoring");
                }
            }
        }
        InboundMessage::Interrupt => {
            let mut state_ref = state.borrow_mut();
            match state_ref.session.as_mut().map(|session| session.interrupt()) {
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    eprintln!("[agent_panel] interrupt failed: {e}");
                    state_ref.session = None;
                    drop(state_ref);
                    evaluate_js_dispatch(webview, &serialize_error_for_js(&format!("failed to send interrupt: {e}")));
                }
                None => {
                    eprintln!("[agent_panel] interrupt received with no active session -- ignoring");
                }
            }
        }
        InboundMessage::PermissionResponse { request_id, allow, reason } => {
            let mut state_ref = state.borrow_mut();
            match state_ref.session.as_mut().map(|session| session.respond_permission(&request_id, allow, reason.as_deref())) {
                Some(Ok(())) => {}
                Some(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Benign: an already-answered or unknown request id (e.g. a duplicate click)
                    // -- not fatal to the session, nothing to surface to the frontend.
                    eprintln!("[agent_panel] respond_permission failed: {e}");
                }
                Some(Err(e)) => {
                    eprintln!("[agent_panel] respond_permission failed: {e}");
                    state_ref.session = None;
                    drop(state_ref);
                    evaluate_js_dispatch(webview, &serialize_error_for_js(&format!("failed to send permission response: {e}")));
                }
                None => {
                    eprintln!("[agent_panel] permission_response received with no active session -- ignoring");
                }
            }
        }
    }
}

fn evaluate_js_dispatch(webview: &WebView, json_payload: &str) {
    let script = format!("window.__neovibeDispatch({});", serde_json::to_string(json_payload).unwrap_or_default());
    webview.evaluate_javascript(&script, None, None, None::<&gtk4::gio::Cancellable>, |result| {
        if let Err(e) = result {
            eprintln!("[agent_panel] evaluate_javascript failed: {e}");
        }
    });
}
