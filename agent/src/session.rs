//! The reducer stage sitting above `agent::process::AgentProcess`: pumps the real Claude-CLI
//! wire events (`agent::event::AgentEvent`) through a per-session translation step into the
//! provider-neutral `agent::projection::AgentDomainEvent` vocabulary, then folds each into a
//! queryable `AgentSessionProjection` -- this crate's real public API for a whole conversation
//! (`start`/`send_turn`/`interrupt`/`respond_permission`/`pump`/`shutdown`).
//!
//! `AgentEvent`/`PermissionSource` (the Claude-wire-specific types) never cross this module's own
//! public boundary as such: `translate_event` (private) is the one place they get turned into
//! `AgentDomainEvent`s, matching design doc §10.2's "these types are not domain/UI events" list.
//! The one piece of Claude-wire-specific state this module still has to keep -- which channel
//! (`PermissionSource::HookRelay` vs `CanUseTool`) a given pending permission arrived on, needed
//! to route `AgentProcess::respond_permission`'s answer back correctly -- lives in
//! `pending_permission_sources`, a private map never exposed through `AgentSessionProjection`.

use crate::event::{AgentEvent, PermissionSource};
use crate::process::{AgentProcess, PermissionMode};
use crate::provider::PermissionDecision;
use crate::projection::{AgentDomainEvent, AgentSessionProjection, ContentKind, PermissionOutcome, TurnOutcome};
use std::collections::HashMap;
use std::path::Path;

/// A whole live conversation: owns the long-lived `AgentProcess`, translates and folds its events
/// through the pure `AgentSessionProjection` reducer, and keeps a full raw `event_log` of every
/// domain event alongside the reduced projection (a future consumer may want the raw stream, e.g.
/// to render every text delta as it arrived rather than just the reducer's flattened view).
pub struct AgentSession {
    process: AgentProcess,
    pub projection: AgentSessionProjection,
    event_log: Vec<AgentDomainEvent>,
    /// Which real channel each currently-pending permission arrived on -- needed by
    /// `respond_permission` to route the answer, never exposed via `AgentSessionProjection` (see
    /// this module's own header doc). Entries are inserted alongside the matching
    /// `PermissionRequested` domain event and removed alongside the matching `PermissionResolved`
    /// one, so this map's keys are always exactly `self.projection.pending_permissions`'s keys.
    pending_permission_sources: HashMap<String, PermissionSource>,
    /// Set by `interrupt()`, consumed and cleared by the next translated `TurnFinished` --
    /// distinguishes a real, CLI-driven `TurnOutcome::Interrupted` from `Completed`/`Failed`. The
    /// CLI still emits a genuine terminal `result` line for an interrupted turn (confirmed by
    /// Phase 0's own real conformance test,
    /// `agent::tests::backend_conformance::real_interrupt_mid_permission_denies_pending_requests_without_ending_the_session`)
    /// -- `interrupt()` itself must NOT synthesize a premature `TurnCompleted` before that real
    /// terminal event arrives, or the session would produce two terminal events for one turn.
    interrupt_requested: bool,
}

impl AgentSession {
    /// Spawns the underlying `AgentProcess` for a whole conversation. See
    /// `AgentProcess::spawn`'s own doc for what `project_dir`/`mode`/`disallowed_tools` mean.
    ///
    /// **Nothing here claims the directory.** Two `AgentSession`s in one project directory is a
    /// supported shape, not a hazard to be locked out: each one's `PreToolUse` hook config travels
    /// in its own CLI process's argv (`--settings`, see `agent::settings`), so neither can read,
    /// overwrite or delete the other's. `backend_conformance`'s
    /// `real_two_sessions_in_the_same_project_dir_each_see_only_their_own_permission_hooks`
    /// asserts exactly that against the real CLI.
    pub fn start(project_dir: &Path, mode: PermissionMode, disallowed_tools: &[&str]) -> std::io::Result<Self> {
        let process = AgentProcess::spawn(project_dir, mode, disallowed_tools)?;
        Ok(Self {
            process,
            projection: AgentSessionProjection::default(),
            event_log: Vec::new(),
            pending_permission_sources: HashMap::new(),
            interrupt_requested: false,
        })
    }

    /// Writes one user turn to the underlying process, synthesizes and folds `TurnStarted`
    /// immediately on success, and returns it. Global Constraint: one active turn per session --
    /// rejects (does not queue) a second call while `self.projection.active_turn_id` is `Some`.
    ///
    /// `TurnStarted` is only folded once the write has actually succeeded: setting it first meant
    /// a failed write (a dead process, a closed stdin) left `active_turn_id` stuck `Some` forever,
    /// since nothing but a `TurnCompleted` for a turn that never started could ever clear it again
    /// -- the same reasoning the pre-Phase-1 `AgentSession::send_turn` already used for
    /// `turn_in_progress`.
    pub fn send_turn(&mut self, text: &str) -> std::io::Result<Vec<AgentDomainEvent>> {
        if self.projection.active_turn_id.is_some() {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "a turn is already in progress on this session"));
        }
        self.process.send_turn(text)?;
        let turn_id = uuid::Uuid::new_v4().to_string();
        let event = AgentDomainEvent::TurnStarted { turn_id };
        self.fold(event.clone());
        Ok(vec![event])
    }

    /// Sends a real interrupt control_request (`AgentProcess::interrupt` already releases every
    /// pending hook connection at the transport level, denying each -- see its own doc). Folds and
    /// returns a `PermissionResolved { outcome: CancelledByInterrupt }` for every permission that
    /// was pending at the moment of the call, draining `pending_permission_sources` to match.
    /// Does NOT fold a `TurnCompleted` here -- see `interrupt_requested`'s own doc for why that
    /// must wait for the turn's real terminal event.
    pub fn interrupt(&mut self) -> std::io::Result<Vec<AgentDomainEvent>> {
        self.process.interrupt()?;
        if self.projection.active_turn_id.is_some() {
            self.interrupt_requested = true;
        }

        let pending_ids: Vec<String> = self.projection.pending_permissions.keys().cloned().collect();
        let mut events = Vec::with_capacity(pending_ids.len());
        for permission_id in pending_ids {
            self.pending_permission_sources.remove(&permission_id);
            let event = AgentDomainEvent::PermissionResolved { permission_id, outcome: PermissionOutcome::CancelledByInterrupt };
            self.fold(event.clone());
            events.push(event);
        }
        Ok(events)
    }

    /// Answers one specific pending permission request, looked up by `permission_id` in
    /// `pending_permission_sources` to find which real channel to route the answer over. Folds
    /// and returns a `PermissionResolved` event only once the decision has genuinely been written
    /// -- a failed write leaves the request pending and still answerable, matching the pre-Phase-1
    /// `AgentSession::respond_permission`'s identical reasoning. Returns `ErrorKind::NotFound` if
    /// no pending request carries that id (an already-answered or unknown id) -- callers must
    /// treat that as a benign no-op, per this plan's Global Constraint on duplicate/unknown ids.
    /// Answers one pending permission request.
    ///
    /// Unlike the sidecar provider, this backend has no event source for the answer: the Claude CLI
    /// emits nothing when a `PreToolUse` hook is replied to, so the `PermissionResolved` below is
    /// folded locally because it is the ONLY place the fact exists. That is a property of this
    /// backend's wire, not a pattern to copy -- on a path where the provider does report the
    /// resolution, the provider's event is the authority and this side must not pre-empt it.
    pub fn respond_permission(&mut self, permission_id: &str, decision: PermissionDecision) -> std::io::Result<Vec<AgentDomainEvent>> {
        let Some(source) = self.pending_permission_sources.get(permission_id).copied() else {
            return Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("no pending permission request with id {permission_id}")));
        };
        self.process.respond_permission(permission_id, source, decision.allows(), decision.reason())?;
        self.pending_permission_sources.remove(permission_id);
        let outcome = if decision.allows() { PermissionOutcome::Allowed } else { PermissionOutcome::Denied };
        let event = AgentDomainEvent::PermissionResolved { permission_id: permission_id.to_string(), outcome };
        self.fold(event.clone());
        Ok(vec![event])
    }

    /// Non-blocking drain: pulls whatever real wire events have arrived from the underlying
    /// process since the last call, translates each into zero or more `AgentDomainEvent`s (see
    /// `translate_event`), folds each into `self.projection` immediately (so
    /// `self.projection.active_turn_id` is always current for the NEXT event this same call
    /// translates -- e.g. a `ToolStarted` arriving right after the `TurnStarted` this same batch
    /// already folded), appends each to `event_log`, and returns the whole batch.
    pub fn pump(&mut self) -> Vec<AgentDomainEvent> {
        let mut produced = Vec::new();
        for wire_event in self.process.poll_events() {
            for domain_event in self.translate_event(wire_event) {
                self.fold(domain_event.clone());
                produced.push(domain_event);
            }
        }
        produced
    }

    pub fn event_log(&self) -> &[AgentDomainEvent] {
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
        // Fail-closed on shutdown, mirroring interrupt()'s identical reasoning: a permission request
        // from a session that's ending can never be genuinely answered. Design doc §7.3 requires this
        // specifically for a Rust shell/window close -- the pre-Phase-1 AgentSession::shutdown already
        // did the equivalent (state.pending_permissions.clear()); this restores that behavior under
        // the new domain-event vocabulary, giving PermissionOutcome::CancelledBySessionClose its first
        // real producer.
        let pending_ids: Vec<String> = self.projection.pending_permissions.keys().cloned().collect();
        for permission_id in pending_ids {
            self.pending_permission_sources.remove(&permission_id);
            self.fold(AgentDomainEvent::PermissionResolved { permission_id, outcome: PermissionOutcome::CancelledBySessionClose });
        }
        self.fold(AgentDomainEvent::SessionClosed { reason: "closed_by_host".to_string() });
    }

    /// Folds one domain event into `self.projection` and appends it to `self.event_log` -- the one
    /// place either happens, so the two never drift apart.
    fn fold(&mut self, event: AgentDomainEvent) {
        self.projection.apply(&event);
        self.event_log.push(event);
    }

    /// Translates one real Claude-wire `AgentEvent` into zero or more `AgentDomainEvent`s, using
    /// `self.projection.active_turn_id` (already current -- see `pump`'s own doc) to stamp
    /// `turn_id` onto events that need one, and `self.interrupt_requested` to decide a
    /// `TurnFinished`'s outcome. Global Constraint: an `AgentEvent` with no domain-event mapping
    /// (`ControlResponse`, `ProcessStderr`, `RateLimit`, `Unknown`) always returns an empty `Vec`
    /// -- never a fabricated event, never a panic.
    fn translate_event(&mut self, event: AgentEvent) -> Vec<AgentDomainEvent> {
        match event {
            AgentEvent::SessionStarted { session_id, model, cwd } => {
                // The legacy CLI backend never distinguishes a sidecar-internal id from the real
                // provider session id -- `session_id` here already *is* the real Claude CLI UUID
                // (it comes straight from the CLI's own `init` line), so both fields get the same
                // value.
                vec![AgentDomainEvent::SessionOpened { session_id: session_id.clone(), provider_session_id: session_id, model, cwd }]
            }
            AgentEvent::AssistantText { text } => {
                let Some(turn_id) = self.projection.active_turn_id.clone() else { return vec![] };
                vec![AgentDomainEvent::ContentDelta { turn_id, kind: ContentKind::Text, text }]
            }
            AgentEvent::Thinking { text } => {
                let Some(turn_id) = self.projection.active_turn_id.clone() else { return vec![] };
                vec![AgentDomainEvent::ContentDelta { turn_id, kind: ContentKind::Thinking, text }]
            }
            AgentEvent::ToolStarted { id, name, input } => {
                let Some(turn_id) = self.projection.active_turn_id.clone() else { return vec![] };
                vec![AgentDomainEvent::ToolCallStarted { turn_id, tool_use_id: id, name, input }]
            }
            AgentEvent::ToolResult { id, content, is_error } => {
                let Some(turn_id) = self.projection.active_turn_id.clone() else { return vec![] };
                vec![AgentDomainEvent::ToolCallCompleted { turn_id, tool_use_id: id, content, is_error }]
            }
            AgentEvent::PermissionRequest { request_id, tool_name, input, source } => {
                self.pending_permission_sources.insert(request_id.clone(), source);
                // `tool_use_id: None` on BOTH sources. On the primary one that is a choice, not a
                // limit -- the answer, since the frontend comments used to imply otherwise:
                //
                //   HookRelay (the documented primary gate) -- `request_id` IS the real Claude
                //     `toolu_*` id. `process.rs`'s hook listener sets it from the `PreToolUse`
                //     payload's own `tool_use_id` (a required field on
                //     `hook_protocol::PreToolUseHookInput`; real capture in
                //     `tests/fixtures/v2_hook_pretooluse_stdin.json`), and that is the same id
                //     `ToolCallStarted` carries out of `wire.rs`'s `tool_use` block. So a
                //     source-aware `Some(request_id.clone())` here would genuinely link a
                //     permission card to the call it gates.
                //
                //   CanUseTool (secondary, best-effort) -- `request_id` is NOT a tool id. It is the
                //     control_request envelope's own id (`wire.rs`'s `ControlRequestLine.request_id`,
                //     e.g. "ctu-1"). Whether the real CLI also puts a `tool_use_id` inside the
                //     request body is UNVERIFIED: the checked-in fixture carries one, but its value
                //     is a hand-written placeholder, the protocol spec describes this message as
                //     carrying `tool_name` and `input` only, and `ControlRequestBody` does not
                //     deserialize the field at all.
                //
                // Left unwired deliberately. Doing it honestly means a source-aware value (so the
                // CanUseTool path keeps saying `None` rather than passing off an envelope id as a
                // tool id), matching changes in the two layers that render it, and one real-turn
                // check that the hook's id and the `tool_use` block's id do match in a live
                // conversation -- a change with its own verification, not a one-liner. Until then
                // the frontend renders `null` honestly rather than guessing at the most recent call.
                vec![AgentDomainEvent::PermissionRequested { permission_id: request_id, tool_use_id: None, tool_name, input }]
            }
            AgentEvent::TurnFinished { result_text, is_error, stop_reason, total_cost_usd, num_turns } => {
                let turn_id = self.projection.active_turn_id.clone().unwrap_or_else(|| "unknown-turn".to_string());
                let outcome = if self.interrupt_requested {
                    self.interrupt_requested = false;
                    TurnOutcome::Interrupted
                } else if is_error {
                    TurnOutcome::Failed
                } else {
                    TurnOutcome::Completed
                };
                // `Some` unconditionally, and correctly so: both figures came off the CLI's own
                // terminal `result` line, where `wire.rs::ResultLine` declares them as REQUIRED
                // (no `serde(default)`) -- a `result` line missing either one fails to deserialize
                // and becomes `AgentEvent::Unknown`, never a `TurnFinished`. So every value
                // reaching here was genuinely reported. This backend has real session-cumulative
                // figures; the sidecar backend has none, and sends `None` rather than a zero
                // standing in for them.
                let usage = Some(crate::UsageInfo { total_cost_usd, num_turns });
                vec![AgentDomainEvent::TurnCompleted { turn_id, outcome, result_text, stop_reason, usage }]
            }
            AgentEvent::ProcessExited { success: true } => {
                vec![AgentDomainEvent::SessionClosed { reason: "provider_exited".to_string() }]
            }
            AgentEvent::ProcessExited { success: false } => {
                vec![AgentDomainEvent::SessionUnavailable { reason: "provider process exited unexpectedly".to_string() }]
            }
            AgentEvent::ProcessStderr { line } => {
                eprintln!("[agent] claude stderr: {line}");
                vec![]
            }
            AgentEvent::RateLimit { raw } => {
                eprintln!("[agent] rate limit notice: {raw}");
                vec![]
            }
            AgentEvent::ControlResponse { .. } | AgentEvent::Unknown { .. } => {
                vec![]
            }
        }
    }
}
