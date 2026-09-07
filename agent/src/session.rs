//! Folds the stream of `AgentEvent`s from one `AgentProcess` into a queryable
//! `AgentSessionState`, and keeps an in-memory, append-only log of every event seen (v1: no
//! disk persistence, see this plan's Global Constraints). This is the "session state" stage of
//! the wire -> event -> session-state -> (future) UI pipeline this plan follows from the
//! early design assessment's recommended shape.

use crate::event::AgentEvent;
use crate::process::{AgentProcess, SpawnMode};
use uuid::Uuid;

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

/// Mirrors the architecture doc's original `AgentSessionState` shape (session id, messages,
/// tool calls, cwd, status) -- "task" and "permissions" fields from that original sketch are
/// deliberately not yet present: there's no multi-task queue and no real permission
/// request/response round-trip in v1 (see this plan's Global Constraints on both).
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
}

impl AgentSessionState {
    /// The reducer: applies one event's effect to this state. Pure and side-effect-free (no I/O)
    /// so it's trivially testable without spawning anything -- see this task's own tests, which
    /// feed a hand-assembled sequence of `AgentEvent`s (built from the same real fixture data
    /// Task 1 introduced) and assert on the resulting state, not on any live process.
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
            AgentEvent::TurnFinished { is_error, .. } => {
                self.status = SessionStatus::Finished { is_error: *is_error };
            }
            AgentEvent::ProcessExited { success } => {
                // Only meaningful if the process died WITHOUT ever having emitted a
                // `TurnFinished` (abnormal/failed exit -- crash, bad `--resume` id, auth
                // failure, spawn-then-die). If `TurnFinished` already arrived and set
                // `Finished`, this is redundant -- leave the existing (possibly
                // `is_error: false`) status alone rather than let a late `ProcessExited`
                // overwrite a normal finish.
                if matches!(self.status, SessionStatus::Starting | SessionStatus::Running) {
                    self.status = SessionStatus::Finished { is_error: !*success };
                }
            }
            AgentEvent::Thinking { .. }
            | AgentEvent::RateLimit { .. }
            | AgentEvent::Unknown { .. }
            | AgentEvent::ProcessStderr { .. } => {
                // No state effect in v1 -- these are observable via the raw event log
                // (`AgentSession::event_log`) if a future caller needs them.
            }
        }
    }
}

pub struct AgentSession {
    process: AgentProcess,
    pub state: AgentSessionState,
    event_log: Vec<AgentEvent>,
    /// The session id `agent` itself asked the CLI to use (via `--session-id`/`--resume`),
    /// recorded from the `SpawnMode` passed to `start` -- in both `New` and `Resume` cases we
    /// already know what UUID we asked for. Used only for the defensive cross-check in `pump()`;
    /// never treated as anything other than a diagnostic (see this plan's Global Constraint:
    /// session identity is caller-assigned, never scraped from the CLI's own output).
    expected_session_id: Uuid,
}

impl AgentSession {
    pub fn start(prompt: &str, mode: SpawnMode, disallowed_tools: &[&str]) -> std::io::Result<Self> {
        let expected_session_id = mode.session_id();
        let process = AgentProcess::spawn(prompt, mode, disallowed_tools)?;
        Ok(Self {
            process,
            state: AgentSessionState::default(),
            event_log: Vec::new(),
            expected_session_id,
        })
    }

    /// Convenience constructor for starting a brand-new session: generates a fresh
    /// `Uuid::new_v4()` internally and returns it alongside the session, since the caller needs
    /// that UUID later to `--resume`. The lower-level `start` remains available for tests wanting
    /// a specific, deterministic UUID, and is still required for `SpawnMode::Resume` (which
    /// always needs an explicit prior UUID).
    pub fn start_new(prompt: &str, disallowed_tools: &[&str]) -> std::io::Result<(Self, Uuid)> {
        let session_id = Uuid::new_v4();
        let session = Self::start(prompt, SpawnMode::New { session_id }, disallowed_tools)?;
        Ok((session, session_id))
    }

    /// Drains whatever's newly arrived from the underlying process, folds each event into
    /// `state`, appends it to the in-memory log, and returns an owned copy of just the events
    /// that were new this call (empty if nothing arrived).
    ///
    /// Returns an owned `Vec` rather than a slice borrowing `self`: the natural consumer pattern
    /// (`for ev in sess.pump() { render(ev, &sess.state) }`, reading `sess.state` -- already
    /// updated by `apply()` -- while iterating the events that produced it) fails to borrow-check
    /// against `&[AgentEvent]`, since that slice keeps `self` borrowed for as long as it's alive,
    /// conflicting with the second access to `self.state` in the same scope. Every event is still
    /// appended to `self.event_log` as before for the durable history.
    pub fn pump(&mut self) -> Vec<AgentEvent> {
        let mut new_events = Vec::new();
        for event in self.process.poll_events() {
            if let AgentEvent::SessionStarted { session_id, .. } = &event {
                if session_id != &self.expected_session_id.to_string() {
                    eprintln!(
                        "[agent] session id mismatch: requested {}, CLI reported {session_id}",
                        self.expected_session_id
                    );
                }
            }
            self.state.apply(&event);
            self.event_log.push(event.clone());
            new_events.push(event);
        }
        new_events
    }

    pub fn event_log(&self) -> &[AgentEvent] {
        &self.event_log
    }

    /// The underlying `claude` child process's OS pid -- delegates to
    /// [`AgentProcess::pid`](crate::process::AgentProcess::pid). Exists so a caller holding only
    /// an `AgentSession` handle (e.g. a future `agent-ui`) can check process liveness/identity
    /// without reaching into a private field.
    pub fn pid(&mut self) -> u32 {
        self.process.pid()
    }

    /// True once the underlying `claude` child process has exited -- delegates to
    /// [`AgentProcess::has_exited`](crate::process::AgentProcess::has_exited).
    pub fn has_exited(&mut self) -> bool {
        self.process.has_exited()
    }

    pub fn shutdown(&mut self) {
        self.process.shutdown();
    }
}
