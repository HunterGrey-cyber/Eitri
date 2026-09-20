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

/// How long a window close waits for an in-flight terminal-handoff worker before abandoning it.
/// See `AgentPanelHandle::shutdown` for what this does and does not cover -- it is sized for the
/// legacy backend's real close time, and is knowingly short of the sidecar path's worst case.
const HANDOFF_CLOSE_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

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
}

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
    closed_rx: mpsc::Receiver<()>,
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
        let pending_handoff = self.state.borrow_mut().pending_handoff.take();
        if let Some(mut session) = self.state.borrow_mut().session.take() {
            session.shutdown();
        }
        if let Some(handoff) = pending_handoff {
            // The worker already owns the backend and is already shutting it down -- there is
            // nothing to start here, only a reason to wait. Exiting the process first would leave
            // the `claude` child mid-close.
            //
            // **A bounded wait, not a guarantee, and the bound is only honest for the default
            // backend.** On `legacy`, `AgentBackend::shutdown()` is ~0.8s of grace periods
            // (`NATURAL_EXIT_GRACE_PERIOD` 500ms + `GRACE_PERIOD` 300ms, agent/src/process.rs) plus
            // three thread joins, so this covers it with room to spare. On `sidecar` it does not:
            // `close_session` is a unary RPC bounded at 10s (`UNARY_RPC_TIMEOUT`,
            // agent/src/providers/claude_sidecar/mod.rs) and the spawned sidecar's own drop adds up
            // to 3s of SIGKILL escalation, so the worst case is ~13s and this wait can expire with
            // the close still in flight. When it does, `main` quits, the detached worker dies with
            // the process, and whatever the sidecar and `claude` were doing is abandoned -- the
            // orphan class this repository keeps having to chase with pid diffs. Raising the wait
            // to cover it would trade that for a window close that appears to hang for 13 seconds,
            // so the choice here is to stay short and say so rather than to claim a bound that is
            // not one. See shell/MANUAL_VERIFICATION.md's owed check for this path.
            match handoff.closed_rx.recv_timeout(HANDOFF_CLOSE_WAIT) {
                Ok(()) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => eprintln!(
                    "[agent_panel] window closed while a terminal handoff was closing the session, and \
                     the worker did not report within {}s; its close is being abandoned",
                    HANDOFF_CLOSE_WAIT.as_secs()
                ),
                // NOT a timeout, and it fires immediately: the worker's sender dropped, which means
                // it panicked inside `shutdown()`. Logging this as a timeout would send a future
                // debugger looking for a hang that never happened.
                Err(mpsc::RecvTimeoutError::Disconnected) => eprintln!(
                    "[agent_panel] window closed during a terminal handoff and the close worker died \
                     inside shutdown() without reporting"
                ),
            }
        }
        if let Some(pending) = pending {
            // Bounded, and blocking on purpose: this runs on the window-close path, where waiting
            // for a clean teardown is the whole point. The worker is either about to finish or has
            // already failed; three seconds is well past a warm connect and well short of a hang.
            match pending.result_rx.recv_timeout(std::time::Duration::from_secs(3)) {
                Ok(Ok(mut backend)) => {
                    eprintln!(
                        "[agent_panel] window closed mid-connect; shutting down the backend that finished anyway"
                    );
                    backend.shutdown();
                }
                Ok(Err(e)) => eprintln!(
                    "[agent_panel] window closed mid-connect; the backend had already failed: {}",
                    e.message
                ),
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
        let vars = self.state.borrow().theme.css_vars();
        self.webview.load_html(&themed_document(&vars), None);
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
        turn_trace: None,
        theme: neovibe_core::theme::ThemeTokens::fallback(),
        pane_focused: false,
        hint_hook: None,
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
        collect_pending_start(&state, &webview);
        collect_pending_handoff(&state, &webview);

        // The payload is built under the borrow and dispatched OUTSIDE it. Calling into WebKit while
        // holding a `RefMut` on the panel's own state is a latent re-entrancy hazard: the script
        // message handler takes the same `RefCell`, and anything that let it run during the
        // dispatch would panic on an already-borrowed cell rather than fail gracefully. Nothing
        // today re-enters, which is exactly why it would stay latent until it didn't.
        report_a_session_that_never_opened(&state, &webview);

        let (payload, first_text_in_this_batch) = {
            let mut state_ref = state.borrow_mut();
            let AgentPanelState {
                session,
                turn_trace,
                supervisor,
                supervisor_pending,
                project_dir,
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
                match session.take_ui_delivery(project_dir) {
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
            let dead = state.borrow_mut().session.take();
            if let Some(mut backend) = dead {
                std::thread::spawn(move || backend.shutdown());
            }
            evaluate_js_dispatch(
                webview,
                &serialize_command_result_for_js(request_id, Err(&error.message)),
            );
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
            std::thread::spawn(move || {
                backend.shutdown();
                // Sent after `shutdown()` returns, which is the whole signal: the command is not
                // dispatched until this arrives. A panic inside `shutdown()` drops the sender
                // instead, which `collect_pending_handoff` reads as a failed close.
                let _ = closed_tx.send(());
            });
            state.borrow_mut().pending_handoff = Some(PendingHandoff {
                request_id,
                command,
                closed_rx,
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
