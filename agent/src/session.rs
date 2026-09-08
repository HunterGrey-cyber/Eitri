//! The reducer stage of the wire -> event -> session-state -> (future) UI pipeline this plan
//! follows from the early design assessment's recommended shape: folds a stream of `AgentEvent`s into
//! a queryable `AgentSessionState`. The reducer (`AgentSessionState::apply`) is pure and
//! side-effect-free (no I/O, no dependency on `AgentProcess`) so it stays trivially testable
//! without spawning anything.
//!
//! This file also owns `AgentSession` (v2, reintroduced by Task 6): a thin, process-owning
//! wrapper around `agent::process::AgentProcess` that pumps its events through the reducer and
//! exposes the crate's real public API for a whole conversation -- `send_turn`/`interrupt`/
//! `respond_permission`/`pump`. Task 5 removed the v1 `AgentSession` (built around the
//! now-deleted `SpawnMode`/per-turn `AgentProcess::spawn`) rather than adapt it, since Task 5's
//! own scope was `agent::process` only; this is that real replacement, built around
//! `AgentProcess`'s new long-lived, full-duplex shape.

use crate::event::{AgentEvent, PermissionSource};
use crate::process::{AgentProcess, PermissionMode};
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub enum SessionStatus {
    Starting,
    Running,
    Finished { is_error: bool },
}

impl Default for SessionStatus {
    fn default() -> Self {
        SessionStatus::Starting
    }
}

#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
    /// `None` until the matching `ToolResult` event arrives.
    pub result: Option<(serde_json::Value, bool)>,
}

/// One unanswered permission request, recording which channel (`PermissionSource`) it arrived on
/// so `AgentSession::respond_permission` can route the answer back correctly without the caller
/// having to separately track that.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequestRecord {
    pub request_id: String,
    pub tool_name: String,
    pub input: serde_json::Value,
    pub source: PermissionSource,
}

/// Mirrors the architecture doc's original `AgentSessionState` shape (session id, messages,
/// tool calls, cwd, status) -- "task" from that original sketch is deliberately not yet present:
/// there's no multi-task queue in v2 either. `turn_in_progress` and `pending_permissions` are new
/// in v2, tracking the conversation-lifecycle state a long-lived, multi-turn process needs that a
/// v1 one-shot-per-turn process never had to.
#[derive(Debug, Clone, Default)]
pub struct AgentSessionState {
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<String>,
    /// Plain assistant text, in arrival order -- a minimal transcript projection. Does not
    /// interleave tool calls/results into this list; consult `tool_calls` separately. A richer
    /// unified transcript view is left for whenever `agent-ui` actually needs one.
    pub transcript: Vec<String>,
    pub tool_calls: Vec<ToolCallRecord>,
    pub status: SessionStatus,
    /// True from `AgentSession::send_turn` until the matching `TurnFinished` arrives. A separate,
    /// orthogonal flag from `status` -- the conversation can be `Running` with no turn in flight.
    pub turn_in_progress: bool,
    /// Every unanswered permission request, in arrival order. A collection, not a single slot:
    /// one assistant message can genuinely contain several `tool_use` blocks, and the generated
    /// hook matcher is `"*"` (every tool), so several `agent-hook` connections can legitimately be
    /// live at once, each blocking its own tool call. A single-slot `Option` silently stranded
    /// every request but the newest -- its connection stayed open with nothing in the public API
    /// able to answer it, so its `agent-hook` blocked for the CLI's full 600s hook timeout.
    /// Entries are removed one at a time by `AgentSession::respond_permission` -- see its own doc
    /// for why nothing else ever removes one (in particular, not an incoming `ControlResponse`).
    pub pending_permissions: Vec<PermissionRequestRecord>,
}

impl AgentSessionState {
    /// The reducer: applies one event's effect to this state. Pure and side-effect-free (no I/O)
    /// so it's trivially testable without spawning anything -- see this task's own tests, which
    /// feed a hand-assembled sequence of `AgentEvent`s and assert on the resulting state, not on
    /// any live process.
    pub fn apply(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::SessionStarted { session_id, model, cwd } => {
                self.session_id = Some(session_id.clone());
                self.model = Some(model.clone());
                self.cwd = Some(cwd.clone());
                self.status = SessionStatus::Running;
            }
            AgentEvent::AssistantText { text } => {
                self.transcript.push(text.clone());
            }
            AgentEvent::ToolStarted { id, name, input } => {
                self.tool_calls.push(ToolCallRecord {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    result: None,
                });
            }
            AgentEvent::ToolResult { id, content, is_error } => {
                if let Some(call) = self.tool_calls.iter_mut().find(|c| &c.id == id) {
                    call.result = Some((content.clone(), *is_error));
                }
            }
            AgentEvent::TurnFinished { .. } => {
                // v2 semantic change from v1: a finished turn does NOT end the conversation --
                // the process stays alive for more turns. Only ProcessExited ends the session
                // now. This also correctly handles an interrupted turn's error_during_execution
                // result: it clears turn_in_progress without flipping the whole session to
                // Finished, since the conversation is still alive.
                self.turn_in_progress = false;
            }
            AgentEvent::PermissionRequest { request_id, tool_name, input, source } => {
                // Push, never overwrite: concurrent requests (several `tool_use` blocks in one
                // assistant message) must all stay answerable -- see `pending_permissions`' doc.
                self.pending_permissions.push(PermissionRequestRecord {
                    request_id: request_id.clone(),
                    tool_name: tool_name.clone(),
                    input: input.clone(),
                    source: *source,
                });
            }
            AgentEvent::ControlResponse { .. } => {
                // Purely observational in v2: a control_response only ever acknowledges a
                // control_request THIS crate itself initiated (e.g. `interrupt`) -- the CLI
                // never sends one back for OUR OWN answer to ITS can_use_tool request (that
                // answer IS a control_response we send, not something acknowledged in turn).
                // So there is no case where an incoming ControlResponse should remove an entry
                // from pending_permissions -- that removal happens immediately inside
                // AgentSession::respond_permission instead, for both PermissionSource variants
                // alike. No state effect here, same as Thinking/RateLimit/Unknown.
            }
            AgentEvent::ProcessExited { success } => {
                // Only meaningful if the process died WITHOUT ever having emitted a
                // `TurnFinished` that ended things (which no `TurnFinished` does anymore in v2)
                // -- this is genuinely the only path that ends a session now.
                if matches!(self.status, SessionStatus::Starting | SessionStatus::Running) {
                    self.status = SessionStatus::Finished { is_error: !*success };
                }
            }
            AgentEvent::Thinking { .. }
            | AgentEvent::RateLimit { .. }
            | AgentEvent::Unknown { .. }
            | AgentEvent::ProcessStderr { .. } => {
                // No state effect -- these are observable via `AgentSession::event_log` if
                // needed.
            }
        }
    }

    /// The unanswered permission request with this `request_id`, if it is still pending. What
    /// `AgentSession::respond_permission` uses to find the channel a given request arrived on;
    /// also the natural way for a UI to redraw one specific approve/deny card.
    pub fn find_pending_permission(&self, request_id: &str) -> Option<&PermissionRequestRecord> {
        self.pending_permissions.iter().find(|p| p.request_id == request_id)
    }

    /// Removes and returns the unanswered permission request with this `request_id`, leaving every
    /// other pending request untouched. Returns `None` if it was already answered or never
    /// existed. This is the removal half of `AgentSession::respond_permission` -- which only calls
    /// it once the decision has genuinely been written, so a failed write leaves the request
    /// pending and still answerable rather than silently dropping it.
    pub fn take_pending_permission(&mut self, request_id: &str) -> Option<PermissionRequestRecord> {
        let index = self.pending_permissions.iter().position(|p| p.request_id == request_id)?;
        Some(self.pending_permissions.remove(index))
    }
}

/// A whole live conversation: owns the long-lived `AgentProcess`, pumps its events through the
/// pure `AgentSessionState` reducer, and keeps a full raw `event_log` alongside the reduced
/// state (a future `agent-ui` may want the raw stream, e.g. to render every assistant text chunk
/// as it arrived, not just the reducer's flattened projection).
pub struct AgentSession {
    process: AgentProcess,
    pub state: AgentSessionState,
    event_log: Vec<AgentEvent>,
}

impl AgentSession {
    /// Spawns the underlying `AgentProcess` for a whole conversation. See
    /// `AgentProcess::spawn`'s own doc for what `project_dir`/`mode`/`disallowed_tools` mean.
    pub fn start(project_dir: &Path, mode: PermissionMode, disallowed_tools: &[&str]) -> std::io::Result<Self> {
        let process = AgentProcess::spawn(project_dir, mode, disallowed_tools)?;
        Ok(Self { process, state: AgentSessionState::default(), event_log: Vec::new() })
    }

    /// Writes one user turn to the underlying process and marks `state.turn_in_progress`. See
    /// `AgentProcess::send_turn`'s own doc for the caller's responsibility around turn ordering.
    ///
    /// `turn_in_progress` is only set once the write has actually succeeded: setting it first
    /// meant a failed write (a dead process, a closed stdin) left the flag stuck `true` forever,
    /// since nothing but a `TurnFinished` for a turn that never started could ever clear it again.
    pub fn send_turn(&mut self, text: &str) -> std::io::Result<()> {
        self.process.send_turn(text)?;
        self.state.turn_in_progress = true;
        Ok(())
    }

    /// Sends a real interrupt control_request. See `AgentProcess::interrupt`'s own doc.
    pub fn interrupt(&mut self) -> std::io::Result<uuid::Uuid> {
        let result = self.process.interrupt();
        // AgentProcess::interrupt now releases every pending hook connection (denying each) as
        // part of stopping the turn -- clear the state-level records to match, mirroring
        // shutdown()'s identical reasoning: a request that can no longer ever be genuinely
        // answered must not keep rendering an approve/deny card that would silently no-op if
        // clicked.
        self.state.pending_permissions.clear();
        result
    }

    /// Answers one specific pending permission request, looked up by `request_id` in
    /// `self.state.pending_permissions` and routed via that request's own recorded `source` --
    /// callers don't need to separately track which channel a request arrived on, unlike
    /// `AgentProcess::respond_permission`. Removes *only* that entry once the answer is
    /// successfully sent, leaving any other concurrently-pending request (a second `tool_use`
    /// block in the same assistant message, say) exactly as answerable as it was before. Returns
    /// `ErrorKind::NotFound` if no pending request carries that id.
    ///
    /// Removal happens immediately on a successful send, for BOTH sources alike: unlike
    /// `interrupt`, there is no later incoming event that ever acknowledges this crate's own
    /// answer to a permission request (a `HookRelay` answer goes out over that request's socket
    /// connection and the CLI just proceeds; a `CanUseTool` answer IS itself the control_response
    /// -- nothing comes back to confirm it landed). An earlier draft of this reducer wrongly tried
    /// to clear the pending request from an incoming `ControlResponse` event instead -- that event
    /// never arrives for a `CanUseTool` answer, which would have left it pending forever; see
    /// `AgentSessionState::apply`'s `ControlResponse` arm for the corrected reasoning.
    pub fn respond_permission(&mut self, request_id: &str, allow: bool, reason: Option<&str>) -> std::io::Result<()> {
        let Some(source) = self.state.find_pending_permission(request_id).map(|p| p.source) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no pending permission request with id {request_id}"),
            ));
        };
        self.process.respond_permission(request_id, source, allow, reason)?;
        // Only after the decision genuinely went out -- a failed write leaves it pending and
        // answerable again, rather than dropping a request nothing ever answered.
        self.state.take_pending_permission(request_id);
        Ok(())
    }

    /// Non-blocking drain: pulls whatever events have arrived from the underlying process since
    /// the last call, folds each into `state` via the reducer, appends each to `event_log`, and
    /// returns the same batch to the caller (mirroring `AgentProcess::poll_events`'s shape, so a
    /// future GTK tick callback can call this every frame with no redesign needed).
    pub fn pump(&mut self) -> Vec<AgentEvent> {
        let mut new_events = Vec::new();
        for event in self.process.poll_events() {
            self.state.apply(&event);
            self.event_log.push(event.clone());
            new_events.push(event);
        }
        new_events
    }

    pub fn event_log(&self) -> &[AgentEvent] {
        &self.event_log
    }

    pub fn pid(&self) -> u32 {
        self.process.pid()
    }

    pub fn has_exited(&mut self) -> bool {
        self.process.has_exited()
    }

    pub fn shutdown(&mut self) {
        self.process.shutdown();
        // `AgentProcess::shutdown` already released every live hook connection (denying and
        // dropping each); clear the state-level records to match, so a caller (e.g. a future
        // agent-ui) doesn't keep rendering approve/deny cards for requests that can no longer
        // ever be genuinely answered -- answering one now would silently no-op (the transport
        // side is already gone) rather than deliver anything.
        self.state.pending_permissions.clear();
    }
}
