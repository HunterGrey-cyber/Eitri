//! Pumps Claude CLI `AgentEvent`s through `translate_wire_event` into provider-neutral
//! `AgentDomainEvent`s, then folds them into a queryable `AgentSessionProjection`.
//! The session exposes `start`, `send_turn`, `interrupt`, `respond_permission`, `pump` and `shutdown`.
//! Wire-specific types stay behind this module's public boundary. The translator is
//! `pub(crate)` so process tests can exercise translation of relayed events.
//! The private `pending_permission_sources` map retains each request's channel so permission
//! answers can be routed through `AgentProcess::respond_permission` without exposing that
//! detail in the projection.

use crate::event::{AgentEvent, PermissionSource};
use crate::process::AgentProcess;
use crate::projection::{AgentDomainEvent, AgentSessionProjection, ContentKind, PermissionOutcome, TurnOutcome};
use crate::provider::PermissionDecision;
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
    /// The `message.id` of the last assistant text block translated, so a text block of a
    /// DIFFERENT message closes the one before it (`AgentDomainEvent::AssistantMessageBoundary`).
    last_text_message_id: Option<String>,
}

impl AgentSession {
    /// Spawns the underlying `AgentProcess` for a whole conversation.
    /// `AgentProcess::spawn` documents `project_dir` and `disallowed_tools`.
    /// The CLI always runs gated in `default`; no permission mode is passed here.
    ///
    /// Two `AgentSession`s can share a project directory. Each process receives its own
    /// `PreToolUse` hook configuration through `--settings` in argv, so neither session can
    /// read, overwrite or delete the other's hook configuration.
    /// The real-CLI test in `backend_conformance` verifies this isolation.
    pub fn start(project_dir: &Path, disallowed_tools: &[&str]) -> std::io::Result<Self> {
        let process = AgentProcess::spawn(project_dir, disallowed_tools)?;
        Ok(Self {
            process,
            projection: AgentSessionProjection::default(),
            event_log: Vec::new(),
            pending_permission_sources: HashMap::new(),
            interrupt_requested: false,
            last_text_message_id: None,
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
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a turn is already in progress on this session",
            ));
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
            let event = AgentDomainEvent::PermissionResolved {
                permission_id,
                outcome: PermissionOutcome::CancelledByInterrupt,
            };
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
    pub fn respond_permission(
        &mut self,
        permission_id: &str,
        decision: PermissionDecision,
    ) -> std::io::Result<Vec<AgentDomainEvent>> {
        let Some(source) = self.pending_permission_sources.get(permission_id).copied() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no pending permission request with id {permission_id}"),
            ));
        };
        self.process
            .respond_permission(permission_id, source, decision.allows(), decision.reason())?;
        self.pending_permission_sources.remove(permission_id);
        let outcome = if decision.allows() {
            PermissionOutcome::Allowed
        } else {
            PermissionOutcome::Denied
        };
        let event = AgentDomainEvent::PermissionResolved {
            permission_id: permission_id.to_string(),
            outcome,
        };
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

    /// Folds an event this side produced rather than the provider, and hands it back for the UI.
    ///
    /// The legacy twin of `AgentConversation::fold_locally`. Only for facts this side genuinely
    /// owns -- what the user typed is the case it exists for -- never for provider state, which is
    /// the provider's to report.
    ///
    /// Routed through the same private `fold` every provider-translated event uses, rather than
    /// calling `self.projection.apply` directly: `fold`'s own doc claims to be "the one place
    /// either [the projection or `event_log`] happens", and a locally-produced event is still a
    /// real, ordered occurrence of this conversation -- `event_log()` is documented as "a full raw
    /// log of every domain event", and a caller replaying it to reconstruct the transcript must see
    /// the prompt too, not just what the provider said back.
    pub fn fold_locally(&mut self, event: AgentDomainEvent) -> AgentDomainEvent {
        self.fold(event.clone());
        event
    }

    pub fn pid(&self) -> u32 {
        self.process.pid()
    }

    pub fn has_exited(&mut self) -> bool {
        self.process.has_exited()
    }

    pub fn shutdown(&mut self) {
        self.process.shutdown();
        // Cancel pending permissions when the session ends, as on interrupt: no answer can be
        // routed to an ended session. Emit `PermissionOutcome::CancelledBySessionClose` for each
        // request and remove its routing source before closing the projection.
        let pending_ids: Vec<String> = self.projection.pending_permissions.keys().cloned().collect();
        for permission_id in pending_ids {
            self.pending_permission_sources.remove(&permission_id);
            self.fold(AgentDomainEvent::PermissionResolved {
                permission_id,
                outcome: PermissionOutcome::CancelledBySessionClose,
            });
        }
        self.fold(AgentDomainEvent::SessionClosed {
            reason: "closed_by_host".to_string(),
        });
    }

    /// Folds one domain event into `self.projection` and appends it to `self.event_log` -- the one
    /// place either happens, so the two never drift apart.
    fn fold(&mut self, event: AgentDomainEvent) {
        self.projection.apply(&event);
        self.event_log.push(event);
    }

    /// Hands one wire event to `translate_wire_event` with this session's three pieces of
    /// translation state borrowed out of `self`. Deliberately holds no logic of its own: the whole
    /// match lives in the free function so every arm of it is reachable from a test without
    /// spawning a real `claude`. If you are about to add a branch here, add it there instead.
    fn translate_event(&mut self, event: AgentEvent) -> Vec<AgentDomainEvent> {
        translate_wire_event(
            event,
            self.projection.active_turn_id.as_deref(),
            &mut self.interrupt_requested,
            &mut self.pending_permission_sources,
            &mut self.last_text_message_id,
        )
    }
}

/// Translates one real Claude-wire `AgentEvent` into zero or more `AgentDomainEvent`s.
///
/// A free function rather than a method, and the reason is a real regression this crate already
/// shipped once: the `PermissionRequest` arm below is the single line that decides whether a
/// permission card can name the call it gates, and while this match hung off `&mut AgentSession`
/// -- a struct owning a live `AgentProcess` -- no test could reach it. The arm was written
/// `tool_use_id: None` for weeks with a fully green workspace. Everything it needs from the
/// session is passed in explicitly instead:
///
/// - `active_turn_id`: the projection's current turn, already folded (see `pump`'s own doc), used
///   to stamp `turn_id` onto the events that carry one.
/// - `interrupt_requested`: read and cleared when deciding a `TurnFinished`'s outcome.
/// - `pending_permission_sources`: written, so `respond_permission` can later route an answer back
///   over the channel its request arrived on.
/// - `last_text_message_id`: the `message.id` of the last text block, read and written, so a text
///   block of a different message starts a new transcript entry.
///
/// Global Constraint: an `AgentEvent` with no domain-event mapping (`ControlResponse`,
/// `ProcessStderr`, `RateLimit`, `Unknown`) always returns an empty `Vec` -- never a fabricated
/// event, never a panic.
pub(crate) fn translate_wire_event(
    event: AgentEvent,
    active_turn_id: Option<&str>,
    interrupt_requested: &mut bool,
    pending_permission_sources: &mut HashMap<String, PermissionSource>,
    last_text_message_id: &mut Option<String>,
) -> Vec<AgentDomainEvent> {
    match event {
        AgentEvent::SessionStarted { session_id, model, cwd } => {
            // The legacy CLI backend never distinguishes a sidecar-internal id from the real
            // provider session id -- `session_id` here already *is* the real Claude CLI UUID
            // (it comes straight from the CLI's own `init` line), so both fields get the same
            // value.
            vec![AgentDomainEvent::SessionOpened {
                session_id: session_id.clone(),
                provider_session_id: session_id,
                model,
                cwd,
            }]
        }
        AgentEvent::AssistantText { text, message_id } => {
            let Some(turn_id) = active_turn_id.map(|t| t.to_string()) else {
                return vec![];
            };
            let mut out = Vec::with_capacity(2);
            // A text block of a different message than the last one: that message is over, even
            // with no tool call between (which would have closed it anyway).
            if let Some(id) = message_id {
                if last_text_message_id.as_ref().is_some_and(|last| *last != id) {
                    out.push(AgentDomainEvent::AssistantMessageBoundary {
                        turn_id: turn_id.clone(),
                    });
                }
                *last_text_message_id = Some(id);
            }
            out.push(AgentDomainEvent::ContentDelta {
                turn_id,
                kind: ContentKind::Text,
                text,
            });
            out
        }
        AgentEvent::Thinking { text } => {
            let Some(turn_id) = active_turn_id.map(|t| t.to_string()) else {
                return vec![];
            };
            vec![AgentDomainEvent::ContentDelta {
                turn_id,
                kind: ContentKind::Thinking,
                text,
            }]
        }
        AgentEvent::ToolStarted { id, name, input } => {
            let Some(turn_id) = active_turn_id.map(|t| t.to_string()) else {
                return vec![];
            };
            vec![AgentDomainEvent::ToolCallStarted {
                turn_id,
                tool_use_id: id,
                name,
                input,
            }]
        }
        AgentEvent::ToolResult { id, content, is_error } => {
            let Some(turn_id) = active_turn_id.map(|t| t.to_string()) else {
                return vec![];
            };
            vec![AgentDomainEvent::ToolCallCompleted {
                turn_id,
                tool_use_id: id,
                content,
                is_error,
            }]
        }
        AgentEvent::PermissionRequest {
            request_id,
            tool_use_id,
            tool_name,
            input,
            source,
        } => {
            pending_permission_sources.insert(request_id.clone(), source);
            vec![permission_requested_event(request_id, tool_use_id, tool_name, input)]
        }
        AgentEvent::TurnFinished {
            result_text,
            is_error,
            stop_reason,
            total_cost_usd,
            num_turns,
        } => {
            let turn_id = active_turn_id
                .map(|t| t.to_string())
                .unwrap_or_else(|| "unknown-turn".to_string());
            let outcome = if *interrupt_requested {
                *interrupt_requested = false;
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
            let usage = Some(crate::UsageInfo {
                total_cost_usd,
                num_turns,
            });
            vec![AgentDomainEvent::TurnCompleted {
                turn_id,
                outcome,
                result_text,
                stop_reason,
                usage,
            }]
        }
        AgentEvent::ProcessExited { success: true, .. } => {
            vec![AgentDomainEvent::SessionClosed {
                reason: "provider_exited".to_string(),
            }]
        }
        AgentEvent::ProcessExited {
            success: false,
            stderr_tail,
        } => {
            // The generic text alone actively misdirected a real investigation (2026-09-18): a
            // multi-account launcher on this host's `PATH` refused the gate-bearing `--settings`
            // flag and the child died before ever emitting `system`/`init`, so nothing upstream of
            // this arm had any more specific signal to offer than "exited unexpectedly" -- even
            // though the launcher's own one-line refusal had already gone past as a
            // `ProcessStderr` event moments earlier. Folding the retained tail into the reason
            // itself (rather than adding a second field nothing downstream reads) means every
            // existing consumer of `SessionUnavailable.reason` -- in particular
            // `AgentBackend::terminated_before_opening` and the panel banner built from it --
            // gets the improvement with no further change on their part.
            let reason = if stderr_tail.is_empty() {
                "provider process exited unexpectedly".to_string()
            } else {
                format!("provider process exited unexpectedly: {}", stderr_tail.join(" | "))
            };
            vec![AgentDomainEvent::SessionUnavailable { reason }]
        }
        // The hook call itself was already denied on its connection (`process::spawn_hook_listener`);
        // what is left is the close, which is the host's.
        AgentEvent::UngatedCliMode { reported } => vec![AgentDomainEvent::UngatedCliMode {
            reported,
            detail: "a PreToolUse hook call".to_string(),
        }],
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

/// Builds the domain event for a legacy-backend permission request. Keeping this separate
/// from session translation lets tests check id handling without spawning a live process.
/// The producer supplies the tool-use id; this function does not inspect `PermissionSource`.
/// `projection::tool_use_link` rejects an empty id, while `permission_id` remains the routing
/// key used by `respond_permission`. The hook relay uses the tool-use id as its routing key,
/// but the fields stay separate so changing either role does not silently change the other.
fn permission_requested_event(
    request_id: String,
    tool_use_id: Option<String>,
    tool_name: String,
    input: serde_json::Value,
) -> AgentDomainEvent {
    AgentDomainEvent::PermissionRequested {
        permission_id: request_id,
        tool_use_id: tool_use_id.and_then(crate::projection::tool_use_link),
        tool_name,
        input,
        // Always the gate's own request on legacy: this backend never passes
        // `--permission-prompt-tool`, so the CLI's own prompts never reach it and are refused
        // headlessly, as before. The `CanUseTool` source here is that channel's never-observed
        // secondary signal, kept as it was.
        provider_prompt: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const REAL_ID: &str = "toolu_01CtdezhmhUCrBaswxW5HYmC";

    /// Folds real stream-json lines through `translate_line` and `translate_wire_event` into a
    /// projection, the way `pump` does.
    fn fold_lines(lines: &[serde_json::Value]) -> AgentSessionProjection {
        let mut projection = AgentSessionProjection::default();
        let mut interrupt_requested = false;
        let mut sources = HashMap::new();
        let mut last_text = None;
        projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        for line in lines {
            for event in crate::wire::translate_line(&line.to_string()) {
                for domain in translate_wire_event(
                    event,
                    projection.active_turn_id.as_deref(),
                    &mut interrupt_requested,
                    &mut sources,
                    &mut last_text,
                ) {
                    projection.apply(&domain);
                }
            }
        }
        projection
    }

    fn assistant_text(message_id: &str, text: &str) -> serde_json::Value {
        json!({"type": "assistant", "message": {"id": message_id, "role": "assistant",
            "content": [{"type": "text", "text": text}]}})
    }

    /// The phase-3 GUI pass (2026-09-25): two assistant messages with no tool call between them
    /// rendered as one, `After the table.TURN-1-DONE`. The CLI emits one `assistant` line per
    /// content block, and the blocks of ONE message share its `message.id`; a different id is a
    /// different message and must be its own transcript entry.
    #[test]
    fn two_assistant_messages_with_nothing_between_are_two_transcript_entries() {
        let projection = fold_lines(&[
            assistant_text("msg_1", "After the table."),
            assistant_text("msg_2", "TURN-1-DONE"),
        ]);
        let texts: Vec<&str> = projection.transcript.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["After the table.", "TURN-1-DONE"]);
    }

    #[test]
    fn two_text_blocks_of_one_message_still_merge() {
        let projection = fold_lines(&[assistant_text("msg_1", "one "), assistant_text("msg_1", "message")]);
        let texts: Vec<&str> = projection.transcript.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["one message"]);
    }

    /// **The test across the seam.** Everything else in this module tests
    /// `permission_requested_event` in isolation, which is one side of a call; the arm that
    /// actually decides what gets passed into it is the other. This drives the real
    /// `translate_wire_event` match, so the argument at that call site is what is under test.
    ///
    /// Reverting that argument to `tool_use_id: None` -- the literal shape this crate shipped for
    /// weeks -- makes this test fail and (as far as any test in this workspace was concerned
    /// before it existed) nothing else.
    #[test]
    fn the_permission_arm_passes_the_wire_events_tool_use_id_through_rather_than_dropping_it() {
        let mut interrupt_requested = false;
        let mut sources = HashMap::new();

        let produced = translate_wire_event(
            AgentEvent::PermissionRequest {
                request_id: REAL_ID.to_string(),
                tool_use_id: Some(REAL_ID.to_string()),
                tool_name: "Bash".to_string(),
                input: json!({"command": "echo hello"}),
                source: PermissionSource::HookRelay,
            },
            None,
            &mut interrupt_requested,
            &mut sources,
            &mut None,
        );

        assert_eq!(produced.len(), 1);
        match &produced[0] {
            AgentDomainEvent::PermissionRequested {
                permission_id,
                tool_use_id,
                tool_name,
                ..
            } => {
                assert_eq!(
                    tool_use_id.as_deref(),
                    Some(REAL_ID),
                    "the arm must forward the event's own tool_use_id; a hardcoded None here is \
                     exactly the regression this test exists to catch"
                );
                assert_eq!(permission_id, REAL_ID);
                assert_eq!(tool_name, "Bash");
            }
            other => panic!("expected PermissionRequested, got {other:?}"),
        }
        assert_eq!(
            sources.get(REAL_ID),
            Some(&PermissionSource::HookRelay),
            "the arm must also file the source, or respond_permission cannot route the answer"
        );
    }

    /// The same seam, driven with a source that supplied nothing: the arm must not invent a link
    /// out of `request_id`, which is the only other string it has in scope.
    #[test]
    fn the_permission_arm_does_not_substitute_the_request_id_when_no_tool_use_id_was_supplied() {
        let mut interrupt_requested = false;
        let mut sources = HashMap::new();

        let produced = translate_wire_event(
            AgentEvent::PermissionRequest {
                request_id: "ctu-1".to_string(),
                tool_use_id: None,
                tool_name: "Bash".to_string(),
                input: json!({}),
                source: PermissionSource::CanUseTool,
            },
            None,
            &mut interrupt_requested,
            &mut sources,
            &mut None,
        );

        match &produced[0] {
            AgentDomainEvent::PermissionRequested {
                permission_id,
                tool_use_id,
                ..
            } => {
                assert_eq!(permission_id, "ctu-1");
                assert_eq!(*tool_use_id, None);
            }
            other => panic!("expected PermissionRequested, got {other:?}"),
        }
        assert_eq!(sources.get("ctu-1"), Some(&PermissionSource::CanUseTool));
    }

    /// The hook-relay shape, which is the one this backend actually gates tools with: the real
    /// `toolu_*` id from the `PreToolUse` payload survives translation and becomes the domain
    /// event's link. Fixture-shaped id on purpose -- this is the exact value
    /// `agent/tests/fixtures/v2_hook_pretooluse_stdin.json` carries.
    #[test]
    fn a_hook_relay_request_keeps_the_real_tool_use_id_as_its_link() {
        let event = permission_requested_event(
            "toolu_01CtdezhmhUCrBaswxW5HYmC".to_string(),
            Some("toolu_01CtdezhmhUCrBaswxW5HYmC".to_string()),
            "Bash".to_string(),
            json!({"command": "echo hello"}),
        );
        match event {
            AgentDomainEvent::PermissionRequested {
                permission_id,
                tool_use_id,
                tool_name,
                ..
            } => {
                assert_eq!(permission_id, "toolu_01CtdezhmhUCrBaswxW5HYmC");
                assert_eq!(tool_use_id.as_deref(), Some("toolu_01CtdezhmhUCrBaswxW5HYmC"));
                assert_eq!(tool_name, "Bash");
            }
            other => panic!("expected PermissionRequested, got {other:?}"),
        }
    }

    /// The `can_use_tool` shape: a permission request whose source supplied no tool-use id at all
    /// stays unlinked. `permission_id` is still the envelope's `request_id`, because that is what
    /// the answer has to be routed back with -- it is simply never promoted into a link.
    #[test]
    fn a_request_with_no_supplied_id_stays_unlinked_rather_than_borrowing_its_permission_id() {
        let event = permission_requested_event("ctu-1".to_string(), None, "Bash".to_string(), json!({}));
        match event {
            AgentDomainEvent::PermissionRequested {
                permission_id,
                tool_use_id,
                ..
            } => {
                assert_eq!(permission_id, "ctu-1");
                assert_eq!(tool_use_id, None);
            }
            other => panic!("expected PermissionRequested, got {other:?}"),
        }
    }

    /// An empty id is not a link, even though the type would happily hold one -- see
    /// `projection::tool_use_link`, whose single definition of that rule this path shares with the
    /// sidecar provider's own translation step.
    #[test]
    fn an_empty_supplied_id_does_not_become_a_link() {
        let event =
            permission_requested_event("perm-1".to_string(), Some(String::new()), "Bash".to_string(), json!({}));
        match event {
            AgentDomainEvent::PermissionRequested { tool_use_id, .. } => assert_eq!(tool_use_id, None),
            other => panic!("expected PermissionRequested, got {other:?}"),
        }
    }
}
