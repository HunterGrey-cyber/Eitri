//! Pure, I/O-free translation from the sidecar's wire-level `SessionEvent` (proto) to this crate's
//! own provider-neutral `AgentDomainEvent` (design doc §10.2's event-flow layering: "Claude gRPC
//! event -> AgentDomainEvent"). No I/O, no async -- every case is covered by a plain unit test with
//! no sidecar process involved, matching this crate's own established preference for testing a
//! pure reducer/translator in isolation before any real-process integration test exercises it.

use crate::process::{classify_cli_mode, classify_reported_cli_mode, CliModeReport, RequestedCliMode};
use crate::{
    AgentDomainEvent, ContentKind, PermissionOutcome, ResumeStatus, TokenUsage, TurnEndDetail, TurnOutcome, UsageInfo,
};
use claude_runtime_protocol::v1::session_event::Event as ProtoEvent;
use claude_runtime_protocol::v1::{
    PermissionMode as ProtoPermissionMode, PermissionOrigin as ProtoPermissionOrigin,
    PermissionOutcome as ProtoPermissionOutcome, ResumeStatus as ProtoResumeStatus, SessionCloseReason,
    SessionEvent as ProtoSessionEvent, TurnOutcome as ProtoTurnOutcome,
};

/// Translates one proto `SessionEvent`'s wire fields into this crate's own domain events. Returns
/// nothing for a genuinely malformed message (unset `oneof`, or unparseable `input_json`/
/// `content_json`) -- logged via `eprintln!`, never panics (Global Constraints), and the caller
/// (Task 8's watch-loop) simply skips that one occurrence rather than tearing down the whole
/// stream over one bad event.
///
/// Usually one event. Two when a `SessionReady` reports a CLI permission mode that is not one the
/// session accepts (spec §2.3, D12): the session did open, AND it must be closed -- so both are said,
/// in that order, rather than one hiding the other. Two also on every `SessionReady` of a session
/// that asked for the CLI's auto mode and got a mode it accepts: the opening, then the mode
/// (`CliPermissionMode`), which the host's answer path reads.
///
/// `requested` is what this session asked the CLI to run in (`build_create_request`), and is what
/// the CLI's report is judged against (`classify_reported_cli_mode`).
pub(crate) fn translate(event: ProtoSessionEvent, requested: RequestedCliMode) -> Vec<AgentDomainEvent> {
    translate_one(event, requested).into_iter().flatten().collect()
}

/// `translate`'s body: `None` is nothing, and `Some` is one or two events.
fn translate_one(event: ProtoSessionEvent, requested: RequestedCliMode) -> Option<Vec<AgentDomainEvent>> {
    // The ENVELOPE's session_id, captured before the oneof is destructured. This is the sidecar's
    // own session id -- the value `CreateSession` returned and the one every subsequent RPC must be
    // addressed with.
    //
    // Do not use `SessionReady.session_id` (the inner field) for this, however much its name
    // suggests otherwise: Verdandi's kernel populates BOTH inner fields from the SDK's own
    // `system`/`init` message, so `SessionReady.session_id == SessionReady.provider_session_id ==
    // the Claude session UUID`, and the sidecar's id appears nowhere inside the message. Measured
    // against a real sidecar, 2026-09-11: CreateSessionResponse returned
    // 0124c913-eeb7-4eef-9436-9714eb1f596c while SessionReady carried
    // a9fd9d1a-92e2-4d0f-8b1f-56bb8c92436c in both inner fields. Trusting the inner field collapses
    // two identities into one and would send Claude's id back as a session_id, which the sidecar
    // answers with SESSION_NOT_FOUND. Reported to Verdandi as a wire-level naming defect; this
    // client does not depend on it being fixed.
    let envelope_session_id = event.session_id;
    let envelope_turn_id = event.turn_id;
    let one = |event: AgentDomainEvent| Some(vec![event]);
    match event.event? {
        // `permission_mode` is the CLI's own `system/init` report, re-sent every turn (read by
        // nothing before R07). On a session that asked for `default`, anything less restrictive
        // than `default` must close the session (D12), and stricter and unreported values are noted
        // by `CliModeNote`. On a session that asked for the CLI's auto mode, `auto` and its
        // `default` fallback are reported on as `CliPermissionMode`, and anything else closes it.
        ProtoEvent::SessionReady(ready) => {
            let reported = ready.permission_mode;
            let mut out = vec![AgentDomainEvent::SessionOpened {
                session_id: envelope_session_id,
                provider_session_id: ready.provider_session_id,
                model: ready.model,
                cwd: ready.cwd,
            }];
            match classify_reported_cli_mode(&reported, requested) {
                CliModeReport::Ungated => out.push(AgentDomainEvent::UngatedCliMode {
                    reported,
                    detail: "SessionReady".to_string(),
                }),
                CliModeReport::Auto | CliModeReport::AutoUnavailable => {
                    out.push(AgentDomainEvent::CliPermissionMode { reported })
                }
                CliModeReport::Default | CliModeReport::Stricter | CliModeReport::Unreported => {}
            }
            Some(out)
        }
        ProtoEvent::TurnStarted(started) => one(AgentDomainEvent::TurnStarted {
            turn_id: started.turn_id,
        }),
        ProtoEvent::TextDelta(delta) => one(AgentDomainEvent::ContentDelta {
            turn_id: delta.turn_id,
            kind: ContentKind::Text,
            text: delta.text,
        }),
        ProtoEvent::ThinkingDelta(delta) => one(AgentDomainEvent::ContentDelta {
            turn_id: delta.turn_id,
            kind: ContentKind::Thinking,
            text: delta.text,
        }),
        ProtoEvent::ToolCallStarted(started) => {
            match serde_json::from_str(&started.input_json) {
                Ok(input) => one(AgentDomainEvent::ToolCallStarted {
                    turn_id: started.turn_id,
                    tool_use_id: started.tool_use_id,
                    name: started.name,
                    input,
                }),
                Err(e) => {
                    eprintln!("agent: ClaudeSidecarProvider: unparseable ToolCallStarted.input_json, dropping this event: {e}");
                    None
                }
            }
        }
        ProtoEvent::ToolCallCompleted(completed) => match serde_json::from_str(&completed.content_json) {
            Ok(content) => one(AgentDomainEvent::ToolCallCompleted {
                turn_id: completed.turn_id,
                tool_use_id: completed.tool_use_id,
                content,
                is_error: completed.is_error,
            }),
            Err(e) => {
                eprintln!("agent: ClaudeSidecarProvider: unparseable ToolCallCompleted.content_json, dropping this event: {e}");
                None
            }
        },
        ProtoEvent::PermissionRequested(requested) => match serde_json::from_str(&requested.input_json) {
            Ok(input) => one(AgentDomainEvent::PermissionRequested {
                // Read before the fields below are moved out of `requested`.
                provider_prompt: provider_prompt_of(&requested),
                permission_id: requested.permission_id,
                // Through the shared rule rather than straight into `Some`: proto3 has no absent
                // string, so an id the provider never set arrives here as `""`, and `Some("")`
                // would compare equal to any other record whose id is also `""`.
                //
                // **What is NOT known:** whether a real sidecar ever sends one. Until 2026-09-15
                // this line wrapped unconditionally, which made
                // `claude_sidecar_conformance.rs`'s `assert!(tool_use_id.is_some(), ..)` true for
                // every possible wire value including `""` -- so that assertion has never
                // discriminated anything, and it is `#[ignore]`d and has not been run since this
                // change made it able to. Treat this as a hole being closed on principle, not as a
                // behaviour anyone has observed.
                tool_use_id: crate::projection::tool_use_link(requested.tool_use_id),
                tool_name: requested.tool_name,
                input,
            }),
            Err(e) => {
                eprintln!("agent: ClaudeSidecarProvider: unparseable PermissionRequested.input_json, dropping this event: {e}");
                None
            }
        },
        ProtoEvent::PermissionResolved(resolved) => {
            let outcome = translate_permission_outcome(resolved.outcome());
            one(AgentDomainEvent::PermissionResolved {
                permission_id: resolved.permission_id,
                outcome,
            })
        }
        ProtoEvent::TurnCompleted(completed) => {
            let outcome = translate_turn_outcome(completed.outcome());
            // Why it ended, for the row the panel draws where a turn did not complete. The text of
            // an error result is in `errors` (its `result_text` is empty); an API error arrives as a
            // "success" result whose `result_text` is the error. A negative status is not one.
            let detail = TurnEndDetail::from_result(
                outcome,
                completed.is_error,
                &completed.result_text,
                completed.terminal_reason,
                completed.api_error_status.and_then(|status| u32::try_from(status).ok()),
                &completed.errors,
            );
            one(AgentDomainEvent::TurnCompleted {
                turn_id: completed.turn_id,
                outcome,
                detail,
                result_text: completed.result_text,
                stop_reason: completed.stop_reason,
                // Verdandi's `TurnUsage` (capability `turn_usage`, `TurnCompleted.usage`), when the
                // SDK's result had a usable `modelUsage`. Unset -- a turn the kernel synthesized, or a
                // sidecar older than the field -- stays `None`, which is unknown and is the whole
                // reason the domain field is an `Option`: this site once sent `0.0`/`0` for every
                // turn, which the projection stored as a measured value indistinguishable from a real
                // free turn. Never put a placeholder here: send `Some` only for a figure that arrived.
                usage: completed.usage.and_then(usage_from_proto),
            })
        }
        ProtoEvent::SessionClosed(closed) => one(AgentDomainEvent::SessionClosed {
            reason: translate_close_reason(closed.reason()),
        }),
        ProtoEvent::ResumeOutcome(outcome) => {
            let status = translate_resume_status(outcome.status());
            one(AgentDomainEvent::ResumeOutcome {
                requested_provider_session_id: outcome.requested_provider_session_id,
                status,
                // The wire uses an empty string for "not set" on a plain string field. An empty id
                // is not an id, so it becomes None rather than travelling inward as `Some("")` and
                // failing an equality check for the wrong reason.
                attached_provider_session_id: Some(outcome.attached_provider_session_id).filter(|id| !id.is_empty()),
                forked: outcome.forked,
                detail: Some(outcome.detail).filter(|d| !d.trim().is_empty()),
            })
        }
        ProtoEvent::ProviderNotice(notice) => {
            // Diagnostics only, per design doc §3.2/§5.2 -- never surfaced as a domain event.
            eprintln!(
                "agent: ClaudeSidecarProvider: ProviderNotice kind={} subtype={:?} (turn {})",
                notice.kind,
                notice.subtype,
                envelope_turn_id.unwrap_or_default()
            );
            None
        }
        // Capability 'set_permission_mode' (Verdandi 133dc03). Emitted once per accepted switch, to
        // every watcher. **Since R07 this client never asks for a switch** (spec §6, D5), so any
        // report is unsolicited: one naming `default` is folded as a no-op, and one naming anything
        // less restrictive -- or the BYPASS enum at all -- closes the session (D12). The
        // `Bypass =>` pattern below is the wire guard's one allowlisted entry: it decodes a report
        // the sidecar sends; it never constructs a request.
        //
        // INTERACTIVE and VERDANDI_RULES are both Eitri's `Auto` (`capabilities_from_handshake`'s
        // note on verdandi_rules); UNSPECIFIED is not a mode anybody chose, so it is dropped, loudly
        // -- after the provider-mode check, which never depends on it.
        ProtoEvent::PermissionModeChanged(changed) => {
            // A session created with the CLI's auto mode refuses every switch (the proto's
            // `SetPermissionModeRequest`), so a report of one is a peer breaking that rule, and what
            // the CLI runs in afterwards is unknown to the answer path: closed, whatever it names.
            if requested == RequestedCliMode::Auto {
                return one(AgentDomainEvent::UngatedCliMode {
                    reported: if changed.permission_mode.is_empty() {
                        "a mode switch".to_string()
                    } else {
                        changed.permission_mode
                    },
                    detail: "PermissionModeChanged".to_string(),
                });
            }
            let mode = match changed.mode() {
                ProtoPermissionMode::Bypass => Some(crate::PermissionMode::Bypass),
                ProtoPermissionMode::Interactive | ProtoPermissionMode::VerdandiRules => {
                    Some(crate::PermissionMode::Auto)
                }
                ProtoPermissionMode::Unspecified => None,
            };
            let ungated_by_name = classify_cli_mode(&changed.permission_mode) == CliModeReport::Ungated;
            if ungated_by_name || mode == Some(crate::PermissionMode::Bypass) {
                return one(AgentDomainEvent::UngatedCliMode {
                    // The CLI's own word when it gave one that trips; `BYPASS` when only the enum
                    // did (a contradictory report still fails closed).
                    reported: if ungated_by_name {
                        changed.permission_mode
                    } else {
                        "BYPASS".to_string()
                    },
                    detail: "PermissionModeChanged".to_string(),
                });
            }
            let Some(mode) = mode else {
                eprintln!("agent: ClaudeSidecarProvider: PermissionModeChanged with no mode, dropping it");
                return None;
            };
            if changed.bypass_default_deny_applied {
                // Eitri states `unrestricted` on every session (owner, 2026-09-20: bypass denies
                // nothing), so this must never happen. If it does, Verdandi now denies
                // Bash/Write/Edit/NotebookEdit.
                eprintln!("agent: ClaudeSidecarProvider: entering bypass applied Verdandi's conservative floor");
            }
            one(AgentDomainEvent::PermissionModeChanged {
                mode,
                provider_mode: changed.permission_mode,
                floor_applied: changed.bypass_default_deny_applied,
            })
        }
        // Capability 'permission_denied_events', on a session that stated a CLI permission mode --
        // which this client does only when it asks for auto. Informational: the CLI has already
        // refused the call and the model already has the refusal as its error result. The strings
        // are the CLI's, carried verbatim; an empty one is proto3's "unset", so it is no id.
        ProtoEvent::PermissionDenied(denied) => one(AgentDomainEvent::PermissionDenied {
            tool_use_id: crate::projection::tool_use_link(denied.tool_use_id),
            tool_name: denied.tool_name,
            reason_type: denied.reason_type,
            reason: denied.reason,
        }),
    }
}

/// Which kind of request this is (O3; Verdandi b3aa188): `None` for the gate's own, `Some` with the
/// CLI's own words for a prompt the CLI raised itself after the gate answered.
///
/// UNSPECIFIED is what a sidecar older than the field sends, and the proto says to read it as HOOK;
/// provider fields on a HOOK request (which the proto says never happens) are not believed. A value
/// this client does not know fails toward a provider prompt the permission policy never judges (O3
/// ruling 3) AND that nothing automatic answers (`unrecognized_origin`, `needs_a_human`; review #3):
/// an unknown kind of ask is a card in every mode, bypass included.
fn provider_prompt_of(requested: &claude_runtime_protocol::v1::PermissionRequested) -> Option<crate::ProviderPrompt> {
    let unrecognized_origin = match ProtoPermissionOrigin::try_from(requested.origin) {
        Ok(ProtoPermissionOrigin::Unspecified) | Ok(ProtoPermissionOrigin::Hook) => return None,
        Ok(ProtoPermissionOrigin::ProviderPrompt) => None,
        Err(_) => {
            eprintln!(
                "agent: ClaudeSidecarProvider: PermissionRequested with unrecognized origin {}, treated as \
                 a CLI prompt only a human answers (a card in every mode)",
                requested.origin
            );
            Some(requested.origin)
        }
    };
    Some(crate::ProviderPrompt {
        unrecognized_origin,
        reason: requested.provider_reason.clone(),
        description: requested.provider_description.clone(),
        blocked_path: requested.provider_blocked_path.clone(),
        matched_ask_rule: requested
            .provider_matched_ask_rule
            .as_ref()
            .map(|rule| crate::MatchedAskRule {
                source: rule.source.clone(),
                tool_name: rule.tool_name.clone(),
                rule_content: rule.rule_content.clone(),
            }),
    })
}

/// Verdandi's `TurnUsage` (capability `turn_usage`): session-cumulative, summed over every model
/// (`runtime.proto:844-869`). A non-finite or negative cost is dropped whole -- never half a figure.
fn usage_from_proto(u: claude_runtime_protocol::v1::TurnUsage) -> Option<UsageInfo> {
    (u.total_cost_usd.is_finite() && u.total_cost_usd >= 0.0).then(|| UsageInfo {
        total_cost_usd: u.total_cost_usd,
        num_turns: None,
        tokens: Some(TokenUsage {
            input: u.input_tokens,
            output: u.output_tokens,
            cache_creation: u.cache_creation_input_tokens,
            cache_read: u.cache_read_input_tokens,
        }),
        model: (!u.model.is_empty()).then_some(u.model),
    })
}

/// An unrecognized or unset status becomes `InitializationFailed`, never `Attached`.
///
/// The asymmetry is deliberate: the only dangerous default here is the one that says a resume
/// succeeded. "Something went wrong and I do not know what" is a true, if vague, statement; "your
/// conversation continued" would not be.
fn translate_resume_status(status: ProtoResumeStatus) -> ResumeStatus {
    match status {
        ProtoResumeStatus::Attached => ResumeStatus::Attached,
        ProtoResumeStatus::Rejected => ResumeStatus::Rejected,
        ProtoResumeStatus::InitializationFailed => ResumeStatus::InitializationFailed,
        ProtoResumeStatus::Unspecified => {
            eprintln!("agent: ClaudeSidecarProvider: ResumeStatus::Unspecified from the wire, treating as a failure");
            ResumeStatus::InitializationFailed
        }
    }
}

fn translate_permission_outcome(outcome: ProtoPermissionOutcome) -> PermissionOutcome {
    match outcome {
        ProtoPermissionOutcome::Allowed => PermissionOutcome::Allowed,
        ProtoPermissionOutcome::Denied => PermissionOutcome::Denied,
        ProtoPermissionOutcome::CancelledByInterrupt => PermissionOutcome::CancelledByInterrupt,
        ProtoPermissionOutcome::CancelledBySessionClose => PermissionOutcome::CancelledBySessionClose,
        ProtoPermissionOutcome::ProviderFailed => PermissionOutcome::ProviderFailed,
        ProtoPermissionOutcome::Expired => PermissionOutcome::Expired,
        ProtoPermissionOutcome::Deferred => PermissionOutcome::Deferred,
        ProtoPermissionOutcome::Unspecified => {
            eprintln!(
                "agent: ClaudeSidecarProvider: PermissionOutcome::Unspecified from the wire, treating as Expired"
            );
            PermissionOutcome::Expired
        }
    }
}

fn translate_turn_outcome(outcome: ProtoTurnOutcome) -> TurnOutcome {
    match outcome {
        ProtoTurnOutcome::Completed => TurnOutcome::Completed,
        ProtoTurnOutcome::Interrupted => TurnOutcome::Interrupted,
        ProtoTurnOutcome::Failed => TurnOutcome::Failed,
        ProtoTurnOutcome::LimitReached => TurnOutcome::LimitReached,
        ProtoTurnOutcome::Unspecified => {
            eprintln!("agent: ClaudeSidecarProvider: TurnOutcome::Unspecified from the wire, treating as Failed");
            TurnOutcome::Failed
        }
    }
}

/// `AgentDomainEvent::SessionClosed` carries a plain `String` reason (unchanged by this plan --
/// Task 2 did not touch it), so the proto's typed `SessionCloseReason` enum collapses to a short,
/// stable diagnostic string here rather than propagating a second parallel typed reason.
fn translate_close_reason(reason: SessionCloseReason) -> String {
    match reason {
        SessionCloseReason::ClosedByHost => "closed_by_host".to_string(),
        SessionCloseReason::ProviderExited => "provider_exited".to_string(),
        SessionCloseReason::ProviderFailed => "provider_failed".to_string(),
        SessionCloseReason::ToolPolicyViolation => "tool_policy_violation".to_string(),
        SessionCloseReason::Unspecified => "unspecified".to_string(),
    }
}

/// Splits consecutive sidecar assistant messages at a `TextDelta.message_id` change, the same rule
/// the legacy backend applies to the CLI's `message.id` (`session.rs`'s `AssistantText` arm).
/// `ProtoEvent::TextDelta` carries no message identity on the wire until capability
/// `text_delta_message_id` (Verdandi 133dc03), so `before` is a no-op when `enabled` is `false` --
/// the capability this provider actually connected against, not a compile-time constant, since an
/// older sidecar must keep concatenating exactly as it always has.
///
/// Called once per raw proto event, before that event's own translation: only a `TextDelta` with
/// `Some(message_id)` can close the message before it, and only a `TurnStarted` clears the tracked
/// id (a new turn is already a new message, so there is nothing left to compare against). Every
/// other event -- `ThinkingDelta` included, despite carrying the same `message_id` field -- leaves
/// the tracked id untouched and never splits: thinking is not assistant text.
pub(crate) struct MessageSplit {
    enabled: bool,
    last_text: Option<String>,
}

impl MessageSplit {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            last_text: None,
        }
    }

    pub(crate) fn before(&mut self, event: &ProtoSessionEvent) -> Option<AgentDomainEvent> {
        if !self.enabled {
            return None;
        }
        match event.event.as_ref()? {
            ProtoEvent::TurnStarted(_) => {
                self.last_text = None;
                None
            }
            ProtoEvent::TextDelta(delta) => {
                let id = delta.message_id.as_ref()?;
                let boundary = self.last_text.as_ref().is_some_and(|last| last != id).then(|| {
                    AgentDomainEvent::AssistantMessageBoundary {
                        turn_id: delta.turn_id.clone(),
                    }
                });
                self.last_text = Some(id.clone());
                boundary
            }
            _ => None,
        }
    }
}

/// Notes, once per session, a CLI permission mode that is stricter than `default` or not reported
/// at all (spec §2.3). Neither closes the session -- a stricter mode still denies on a non-answer,
/// and an empty field is a sidecar older than it, neither a downgrade nor an all-clear -- but both
/// are worth one line, and `SessionReady` arrives every turn, so the line would otherwise repeat.
///
/// Per session, alongside `MessageSplit`, in the watch loop; `translate` itself stays pure.
///
/// On a session that asked for the CLI's auto mode the one line due is the fallback: the CLI runs
/// `default` instead, so Eitri's own policy answers its requests and none is deferred.
#[derive(Default)]
pub(crate) struct CliModeNote {
    requested: RequestedCliMode,
    noted: bool,
}

impl CliModeNote {
    pub(crate) fn new(requested: RequestedCliMode) -> Self {
        Self {
            requested,
            noted: false,
        }
    }

    /// The line to print for this event, the first time one is due.
    pub(crate) fn observe(&mut self, event: &ProtoSessionEvent) -> Option<String> {
        let (reported, source) = match event.event.as_ref()? {
            ProtoEvent::SessionReady(ready) => (ready.permission_mode.as_str(), "SessionReady"),
            ProtoEvent::PermissionModeChanged(changed) => (changed.permission_mode.as_str(), "PermissionModeChanged"),
            _ => return None,
        };
        let line = match classify_reported_cli_mode(reported, self.requested) {
            CliModeReport::Stricter => {
                format!("[permission] the CLI reports permission mode '{reported}' in {source}, not 'default'")
            }
            CliModeReport::Unreported => format!(
                "[permission] {source} reports no CLI permission mode (a sidecar older than the field): \
                 neither a downgrade nor an all-clear"
            ),
            CliModeReport::AutoUnavailable => format!(
                "[permission] the CLI reports permission mode '{reported}' in {source} although this session \
                 asked for its auto mode (unavailable to it): Eitri's own policy answers, nothing is deferred"
            ),
            CliModeReport::Default | CliModeReport::Auto | CliModeReport::Ungated => return None,
        };
        if std::mem::replace(&mut self.noted, true) {
            return None;
        }
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_runtime_protocol::v1::{
        MatchedAskRule, PermissionModeChanged, PermissionOrigin as ProtoPermissionOrigin,
        PermissionOutcome as ProtoPermOutcome, PermissionRequested, PermissionResolved, ProviderNotice,
        SessionCloseReason, SessionClosed, SessionReady, TextDelta, ThinkingDelta, ToolCallCompleted, ToolCallStarted,
        TurnCompleted, TurnOutcome as ProtoTOutcome, TurnStarted,
    };

    /// The single event the old `Option`-returning `translate` gave, for the tests written against
    /// it. Every event but a tripping `SessionReady` still translates to at most one; that one is
    /// tested through `super::translate` itself.
    fn translate(event: ProtoSessionEvent) -> Option<AgentDomainEvent> {
        let mut out = super::translate(event, RequestedCliMode::Default);
        assert!(out.len() <= 1, "expected at most one event, got {out:?}");
        out.pop()
    }

    fn wrap(event: ProtoEvent) -> ProtoSessionEvent {
        ProtoSessionEvent {
            session_id: "sess-1".into(),
            sequence: 1,
            occurred_at: 0,
            turn_id: None,
            event: Some(event),
        }
    }

    #[test]
    fn session_opened_takes_its_session_id_from_the_envelope_not_the_inner_message() {
        // The inner field is deliberately given a DIFFERENT (and, per the real sidecar, wrong)
        // value here: Verdandi's kernel fills both inner fields with the Claude session UUID, so
        // trusting `SessionReady.session_id` collapses the sidecar's identity into Claude's. The
        // envelope is the authority for which session an event belongs to.
        let event = wrap(ProtoEvent::SessionReady(SessionReady {
            session_id: "claude-uuid-not-the-sidecars".into(),
            provider_session_id: "claude-uuid-not-the-sidecars".into(),
            model: "claude-sonnet-5".into(),
            cwd: "/tmp".into(),
            // New in the protocol bump of 2026-09-20: the sidecar now reports the permission mode
            // actually in force, rather than leaving the caller to assume the one it asked for.
            // Read since R07 by the D12 tripwire (the tests below); empty here, which is
            // "unreported" -- noted, never a trip, so this test still sees exactly one event.
            permission_mode: String::new(),
            // account_identity, init_fingerprint: added at 133dc03, read by nothing in this test.
            ..Default::default()
        }));
        assert_eq!(
            translate(event),
            Some(AgentDomainEvent::SessionOpened {
                session_id: "sess-1".into(), // the envelope's, set by `wrap`
                provider_session_id: "claude-uuid-not-the-sidecars".into(),
                model: "claude-sonnet-5".into(),
                cwd: "/tmp".into(),
            })
        );
    }

    #[test]
    fn turn_started_translates() {
        let event = wrap(ProtoEvent::TurnStarted(TurnStarted {
            turn_id: "turn-1".into(),
        }));
        assert_eq!(
            translate(event),
            Some(AgentDomainEvent::TurnStarted {
                turn_id: "turn-1".into()
            })
        );
    }

    #[test]
    fn text_delta_translates_with_text_kind() {
        let event = wrap(ProtoEvent::TextDelta(TextDelta {
            turn_id: "turn-1".into(),
            text: "hi".into(),
            // message_id: capability 'text_delta_message_id' (133dc03), read by nothing yet --
            // Task 3 of the wave-5 plan is the message-split consumer.
            ..Default::default()
        }));
        assert_eq!(
            translate(event),
            Some(AgentDomainEvent::ContentDelta {
                turn_id: "turn-1".into(),
                kind: ContentKind::Text,
                text: "hi".into()
            })
        );
    }

    #[test]
    fn thinking_delta_translates_with_thinking_kind() {
        let event = wrap(ProtoEvent::ThinkingDelta(ThinkingDelta {
            turn_id: "turn-1".into(),
            text: "hmm".into(),
            // message_id: as TextDelta.message_id, read by nothing yet.
            ..Default::default()
        }));
        assert_eq!(
            translate(event),
            Some(AgentDomainEvent::ContentDelta {
                turn_id: "turn-1".into(),
                kind: ContentKind::Thinking,
                text: "hmm".into()
            })
        );
    }

    #[test]
    fn tool_call_started_parses_input_json() {
        let event = wrap(ProtoEvent::ToolCallStarted(ToolCallStarted {
            turn_id: "turn-1".into(),
            tool_use_id: "tu-1".into(),
            name: "Bash".into(),
            input_json: r#"{"command":"echo hi"}"#.into(),
        }));
        assert_eq!(
            translate(event),
            Some(AgentDomainEvent::ToolCallStarted {
                turn_id: "turn-1".into(),
                tool_use_id: "tu-1".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "echo hi"}),
            })
        );
    }

    #[test]
    fn tool_call_started_drops_the_event_on_unparseable_input_json() {
        let event = wrap(ProtoEvent::ToolCallStarted(ToolCallStarted {
            turn_id: "turn-1".into(),
            tool_use_id: "tu-1".into(),
            name: "Bash".into(),
            input_json: "not json".into(),
        }));
        assert_eq!(translate(event), None);
    }

    #[test]
    fn tool_call_completed_drops_the_event_on_unparseable_content_json() {
        let event = wrap(ProtoEvent::ToolCallCompleted(ToolCallCompleted {
            turn_id: "turn-1".into(),
            tool_use_id: "tu-1".into(),
            content_json: "not json".into(),
            is_error: false,
        }));
        assert_eq!(translate(event), None);
    }

    #[test]
    fn tool_call_completed_parses_content_json() {
        let event = wrap(ProtoEvent::ToolCallCompleted(ToolCallCompleted {
            turn_id: "turn-1".into(),
            tool_use_id: "tu-1".into(),
            content_json: r#""hi""#.into(),
            is_error: false,
        }));
        assert_eq!(
            translate(event),
            Some(AgentDomainEvent::ToolCallCompleted {
                turn_id: "turn-1".into(),
                tool_use_id: "tu-1".into(),
                content: serde_json::json!("hi"),
                is_error: false,
            })
        );
    }

    #[test]
    fn permission_requested_carries_tool_use_id_and_parses_input_json() {
        let event = wrap(ProtoEvent::PermissionRequested(PermissionRequested {
            permission_id: "perm-1".into(),
            tool_use_id: "tu-1".into(),
            tool_name: "Write".into(),
            input_json: "{}".into(),
            // `origin` UNSPECIFIED and no provider fields: what a sidecar older than b3aa188 sends.
            ..Default::default()
        }));
        assert_eq!(
            translate(event),
            Some(AgentDomainEvent::PermissionRequested {
                permission_id: "perm-1".into(),
                tool_use_id: Some("tu-1".into()),
                tool_name: "Write".into(),
                input: serde_json::json!({}),
                provider_prompt: None,
            })
        );
    }

    /// proto3 has no absent string, so a `tool_use_id` the provider never set arrives here as
    /// `""` rather than as a missing field -- and `Some("")` downstream is a link that matches any
    /// other record whose id is also `""`. The conformance test against the real sidecar asserts
    /// it always sends a genuine id, so this arm is not expected to fire in practice; it exists
    /// because the wire type cannot rule it out and "no link" is the honest reading if it ever
    /// does. See `projection::tool_use_link`, the single definition both backends share.
    #[test]
    fn an_unset_proto3_tool_use_id_arrives_as_an_empty_string_and_is_not_treated_as_a_link() {
        let event = wrap(ProtoEvent::PermissionRequested(PermissionRequested {
            permission_id: "perm-1".into(),
            tool_use_id: String::new(),
            tool_name: "Write".into(),
            input_json: "{}".into(),
            ..Default::default()
        }));
        assert_eq!(
            translate(event),
            Some(AgentDomainEvent::PermissionRequested {
                permission_id: "perm-1".into(),
                tool_use_id: None,
                tool_name: "Write".into(),
                input: serde_json::json!({}),
                provider_prompt: None,
            })
        );
    }

    /// A `PermissionRequested` the CLI itself raised (O3; Verdandi b3aa188): origin PROVIDER_PROMPT,
    /// with the CLI's own words carried verbatim into `provider_prompt` -- the reason sentence the
    /// card shows, the subject, the blocked path, and the user's ask rule when one forced it.
    fn provider_prompt_wire(origin: i32) -> PermissionRequested {
        PermissionRequested {
            permission_id: "perm-2".into(),
            tool_use_id: "toolu_01NqRA82mkgduq2VkbswcvQQ".into(),
            tool_name: "Write".into(),
            input_json: r#"{"file_path":"/p/.git/probe","content":"o3 2"}"#.into(),
            origin,
            provider_reason: Some(
                "Claude requested permissions to edit /p/.git/probe which is a sensitive file.".into(),
            ),
            provider_description: Some(".git/probe".into()),
            provider_blocked_path: Some("/p/.git/probe".into()),
            provider_matched_ask_rule: Some(MatchedAskRule {
                source: "projectSettings".into(),
                tool_name: "Write".into(),
                rule_content: Some(".git/**".into()),
            }),
        }
    }

    #[test]
    fn a_provider_prompt_carries_the_clis_own_words_verbatim() {
        let event = wrap(ProtoEvent::PermissionRequested(provider_prompt_wire(
            ProtoPermissionOrigin::ProviderPrompt as i32,
        )));
        assert_eq!(
            translate(event),
            Some(AgentDomainEvent::PermissionRequested {
                permission_id: "perm-2".into(),
                tool_use_id: Some("toolu_01NqRA82mkgduq2VkbswcvQQ".into()),
                tool_name: "Write".into(),
                input: serde_json::json!({ "file_path": "/p/.git/probe", "content": "o3 2" }),
                provider_prompt: Some(crate::ProviderPrompt {
                    reason: Some(
                        "Claude requested permissions to edit /p/.git/probe which is a sensitive file.".into()
                    ),
                    description: Some(".git/probe".into()),
                    blocked_path: Some("/p/.git/probe".into()),
                    matched_ask_rule: Some(crate::MatchedAskRule {
                        source: "projectSettings".into(),
                        tool_name: "Write".into(),
                        rule_content: Some(".git/**".into()),
                    }),
                    unrecognized_origin: None,
                }),
            })
        );
    }

    /// Every provider field is optional on the wire; a provider prompt that carries none of them is
    /// still the CLI's own prompt, never the gate's -- origin alone decides.
    #[test]
    fn a_provider_prompt_without_any_provider_field_is_still_one() {
        let event = wrap(ProtoEvent::PermissionRequested(PermissionRequested {
            permission_id: "perm-3".into(),
            tool_use_id: "tu-3".into(),
            tool_name: "Write".into(),
            input_json: "{}".into(),
            origin: ProtoPermissionOrigin::ProviderPrompt as i32,
            ..Default::default()
        }));
        match translate(event) {
            Some(AgentDomainEvent::PermissionRequested { provider_prompt, .. }) => {
                assert_eq!(provider_prompt, Some(crate::ProviderPrompt::default()));
            }
            other => panic!("expected a PermissionRequested, got {other:?}"),
        }
    }

    /// The gate's own request, from a sidecar with the capability (HOOK) or from one older than the
    /// field (UNSPECIFIED, which the proto says to read as HOOK), is never a provider prompt -- and
    /// provider fields a HOOK request should not carry are not believed if it does.
    #[test]
    fn a_hook_request_and_an_older_sidecars_unspecified_origin_are_the_gates_own() {
        for origin in [ProtoPermissionOrigin::Hook, ProtoPermissionOrigin::Unspecified] {
            let event = wrap(ProtoEvent::PermissionRequested(provider_prompt_wire(origin as i32)));
            match translate(event) {
                Some(AgentDomainEvent::PermissionRequested { provider_prompt, .. }) => {
                    assert_eq!(provider_prompt, None, "{origin:?}");
                }
                other => panic!("{origin:?}: expected a PermissionRequested, got {other:?}"),
            }
        }
    }

    /// A value this client does not know (a newer sidecar's) fails toward the CLI's own prompt: that
    /// kind is never judged by the permission policy, and in Auto it is a card, so an unknown origin
    /// can never be auto-allowed by the classifier as though the gate had asked.
    #[test]
    fn an_unrecognized_origin_is_treated_as_the_clis_own_prompt() {
        let event = wrap(ProtoEvent::PermissionRequested(PermissionRequested {
            permission_id: "perm-4".into(),
            tool_use_id: "tu-4".into(),
            tool_name: "Read".into(),
            input_json: r#"{"file_path":"main.rs"}"#.into(),
            origin: 7,
            ..Default::default()
        }));
        match translate(event) {
            Some(AgentDomainEvent::PermissionRequested { provider_prompt, .. }) => {
                let prompt = provider_prompt.expect("an unknown origin is never the gate's own request");
                assert_eq!(prompt.unrecognized_origin, Some(7));
                // O3 review #3: it is a card in every mode, bypass included -- the same standing
                // as the user's own ask rule, not the plain CLI prompt bypass allows.
                assert!(prompt.needs_a_human());
            }
            other => panic!("expected a PermissionRequested, got {other:?}"),
        }
    }

    /// The shared link rule applies to a provider prompt too: an empty proto3 id is no link, so a
    /// prompt with one can never be matched against another request's empty id (O3 ruling 5).
    #[test]
    fn a_provider_prompt_with_an_empty_tool_use_id_carries_no_link() {
        let mut wire = provider_prompt_wire(ProtoPermissionOrigin::ProviderPrompt as i32);
        wire.tool_use_id = String::new();
        match translate(wrap(ProtoEvent::PermissionRequested(wire))) {
            Some(AgentDomainEvent::PermissionRequested {
                tool_use_id,
                provider_prompt,
                ..
            }) => {
                assert_eq!(tool_use_id, None);
                assert!(provider_prompt.is_some());
            }
            other => panic!("expected a PermissionRequested, got {other:?}"),
        }
    }

    #[test]
    fn permission_resolved_translates_every_outcome_value() {
        let cases = [
            (ProtoPermOutcome::Allowed, PermissionOutcome::Allowed),
            (ProtoPermOutcome::Denied, PermissionOutcome::Denied),
            (
                ProtoPermOutcome::CancelledByInterrupt,
                PermissionOutcome::CancelledByInterrupt,
            ),
            (
                ProtoPermOutcome::CancelledBySessionClose,
                PermissionOutcome::CancelledBySessionClose,
            ),
            (ProtoPermOutcome::ProviderFailed, PermissionOutcome::ProviderFailed),
            (ProtoPermOutcome::Expired, PermissionOutcome::Expired),
            (ProtoPermOutcome::Deferred, PermissionOutcome::Deferred),
        ];
        for (proto_outcome, expected) in cases {
            let event = wrap(ProtoEvent::PermissionResolved(PermissionResolved {
                permission_id: "perm-1".into(),
                outcome: proto_outcome as i32,
            }));
            assert_eq!(
                translate(event),
                Some(AgentDomainEvent::PermissionResolved {
                    permission_id: "perm-1".into(),
                    outcome: expected
                })
            );
        }
    }

    /// The usage half of this used to be named "...and_zeroes_usage", and asserted a `0.0`/`0` that
    /// the wire never sent. `None` is the honest translation of a `TurnCompleted` whose `usage` is
    /// unset (a kernel-synthesized one, or a result with no usable `modelUsage`), and asserting it
    /// here is what stops a placeholder creeping back in. The mapped case, now that Verdandi's
    /// `TurnUsage` exists, is `turn_completed_maps_turn_usage_and_keeps_an_absent_one_unknown`.
    #[test]
    fn turn_completed_translates_every_outcome_value_and_reports_usage_as_unknown() {
        let cases = [
            (ProtoTOutcome::Completed, TurnOutcome::Completed),
            (ProtoTOutcome::Interrupted, TurnOutcome::Interrupted),
            (ProtoTOutcome::Failed, TurnOutcome::Failed),
            (ProtoTOutcome::LimitReached, TurnOutcome::LimitReached),
        ];
        for (proto_outcome, expected) in cases {
            let event = wrap(ProtoEvent::TurnCompleted(TurnCompleted {
                turn_id: "turn-1".into(),
                outcome: proto_outcome as i32,
                result_text: "done".into(),
                is_error: false,
                stop_reason: Some("end_turn".into()),
                // The rest of the result's fields: `turn_completed_carries_why_a_turn_did_not_complete`.
                ..Default::default()
            }));
            assert_eq!(
                translate(event),
                Some(AgentDomainEvent::TurnCompleted {
                    turn_id: "turn-1".into(),
                    outcome: expected,
                    result_text: "done".into(),
                    stop_reason: Some("end_turn".into()),
                    usage: None,
                    detail: TurnEndDetail::from_result(expected, false, "done", None, None, &[]),
                })
            );
        }
    }

    /// The shapes the SDK ends a turn with, as Verdandi relays them: what each leaves for the row the
    /// panel draws where the turn ended.
    #[test]
    fn turn_completed_carries_why_a_turn_did_not_complete() {
        let detail_of = |completed: TurnCompleted| match translate(wrap(ProtoEvent::TurnCompleted(completed))) {
            Some(AgentDomainEvent::TurnCompleted { detail, .. }) => detail,
            other => panic!("{other:?}"),
        };
        let detail = |reason: Option<&str>, api_error_status: Option<u32>, message: Option<&str>| TurnEndDetail {
            reason: reason.map(str::to_string),
            api_error_status,
            message: message.map(str::to_string),
        };

        // A rejected token, as recorded from the real CLI: a "success" result whose text is the error.
        assert_eq!(
            detail_of(TurnCompleted {
                outcome: ProtoTOutcome::Failed as i32,
                result_text: "Failed to authenticate. API Error: 401 OAuth access token is invalid.".into(),
                is_error: true,
                terminal_reason: Some("api_error".into()),
                api_error_status: Some(401),
                result_subtype: Some("success".into()),
                ..Default::default()
            }),
            detail(
                Some("api_error"),
                Some(401),
                Some("Failed to authenticate. API Error: 401 OAuth access token is invalid.")
            )
        );
        // An error result: no text, the words are in `errors`.
        assert_eq!(
            detail_of(TurnCompleted {
                outcome: ProtoTOutcome::LimitReached as i32,
                is_error: true,
                terminal_reason: Some("max_turns".into()),
                result_subtype: Some("error_max_turns".into()),
                errors: vec!["Reached maximum number of turns (3)".into()],
                ..Default::default()
            }),
            detail(Some("max_turns"), None, Some("Reached maximum number of turns (3)"))
        );
        assert_eq!(
            detail_of(TurnCompleted {
                outcome: ProtoTOutcome::LimitReached as i32,
                is_error: true,
                terminal_reason: Some("blocking_limit".into()),
                errors: vec!["usage limit reached".into(), "  ".into(), "try again later".into()],
                ..Default::default()
            }),
            detail(
                Some("blocking_limit"),
                None,
                Some("usage limit reached\ntry again later")
            )
        );
        // A turn a hook stopped is not an error, and its text is the reply, not a reason.
        assert_eq!(
            detail_of(TurnCompleted {
                outcome: ProtoTOutcome::Failed as i32,
                is_error: false,
                result_text: "I stopped where the hook asked me to.".into(),
                terminal_reason: Some("hook_stopped".into()),
                ..Default::default()
            }),
            detail(Some("hook_stopped"), None, None)
        );
        // An interrupt is the user's own doing: its diagnostics are not shown.
        assert_eq!(
            detail_of(TurnCompleted {
                outcome: ProtoTOutcome::Interrupted as i32,
                is_error: true,
                terminal_reason: Some("aborted_streaming".into()),
                result_subtype: Some("error_during_execution".into()),
                errors: vec!["[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=null".into()],
                ..Default::default()
            }),
            detail(Some("aborted_streaming"), None, None)
        );
        // The end Verdandi synthesizes when the CLI died mid-turn says nothing more.
        assert_eq!(
            detail_of(TurnCompleted {
                outcome: ProtoTOutcome::Failed as i32,
                is_error: true,
                ..Default::default()
            }),
            TurnEndDetail::default()
        );
        // A negative status is not an HTTP status.
        assert_eq!(
            detail_of(TurnCompleted {
                outcome: ProtoTOutcome::Failed as i32,
                api_error_status: Some(-1),
                ..Default::default()
            })
            .api_error_status,
            None
        );
        // A long message is cut, on a character boundary.
        let long = "é".repeat(crate::MAX_TURN_END_MESSAGE_CHARS + 50);
        let message = detail_of(TurnCompleted {
            outcome: ProtoTOutcome::Failed as i32,
            errors: vec![long],
            ..Default::default()
        })
        .message
        .unwrap();
        assert_eq!(message.chars().count(), crate::MAX_TURN_END_MESSAGE_CHARS + 1);
        assert!(message.ends_with('…'), "{message}");
    }

    /// Verdandi's `TurnUsage` (capability `turn_usage`) reaches the domain event whole: every token
    /// count, the cost, and the model that did the work. An unset `usage` stays unknown -- never a
    /// zero -- and a cost that is not a measurement (NaN) drops the whole figure.
    #[test]
    fn turn_completed_maps_turn_usage_and_keeps_an_absent_one_unknown() {
        use claude_runtime_protocol::v1::TurnUsage as ProtoTurnUsage;
        let with = |usage| {
            translate(wrap(ProtoEvent::TurnCompleted(TurnCompleted {
                turn_id: "t".into(),
                usage,
                ..Default::default()
            })))
        };
        let Some(AgentDomainEvent::TurnCompleted { usage, .. }) = with(Some(ProtoTurnUsage {
            input_tokens: 10,
            output_tokens: 20,
            cache_creation_input_tokens: 300,
            cache_read_input_tokens: 4000,
            total_cost_usd: 0.0421,
            model: "claude-sonnet-5".into(),
        })) else {
            panic!()
        };
        assert_eq!(
            usage,
            Some(UsageInfo {
                total_cost_usd: 0.0421,
                num_turns: None,
                tokens: Some(TokenUsage {
                    input: 10,
                    output: 20,
                    cache_creation: 300,
                    cache_read: 4000
                }),
                model: Some("claude-sonnet-5".into()),
            })
        );
        let Some(AgentDomainEvent::TurnCompleted { usage, .. }) = with(None) else {
            panic!()
        };
        assert_eq!(usage, None, "no TurnUsage is unknown, never zero");
        let Some(AgentDomainEvent::TurnCompleted { usage, .. }) = with(Some(ProtoTurnUsage {
            total_cost_usd: f64::NAN,
            ..Default::default()
        })) else {
            panic!()
        };
        assert_eq!(usage, None, "a non-finite cost is not a measurement");
    }

    /// The rest of `usage_from_proto`'s rule: a bad cost is dropped whole (no half a figure with the
    /// tokens kept), and an honest report is kept whole -- a zero cost is a cost, and proto3's empty
    /// `model` string is "no model", not a model called "".
    #[test]
    fn turn_usage_with_a_bad_cost_is_dropped_whole_and_an_honest_one_is_kept_whole() {
        use claude_runtime_protocol::v1::TurnUsage as ProtoTurnUsage;
        let usage_of = |u: ProtoTurnUsage| match translate(wrap(ProtoEvent::TurnCompleted(TurnCompleted {
            turn_id: "t".into(),
            usage: Some(u),
            ..Default::default()
        }))) {
            Some(AgentDomainEvent::TurnCompleted { usage, .. }) => usage,
            other => panic!("expected a TurnCompleted, got {other:?}"),
        };
        for bad in [-0.01, f64::NEG_INFINITY, f64::INFINITY, f64::NAN] {
            assert_eq!(
                usage_of(ProtoTurnUsage {
                    input_tokens: 5,
                    output_tokens: 6,
                    total_cost_usd: bad,
                    model: "claude-sonnet-5".into(),
                    ..Default::default()
                }),
                None,
                "cost {bad} must drop the whole figure, tokens and model included"
            );
        }
        assert_eq!(
            usage_of(ProtoTurnUsage {
                input_tokens: 7,
                cache_read_input_tokens: 9,
                total_cost_usd: 0.0,
                ..Default::default()
            }),
            Some(UsageInfo {
                total_cost_usd: 0.0,
                num_turns: None,
                tokens: Some(TokenUsage {
                    input: 7,
                    output: 0,
                    cache_creation: 0,
                    cache_read: 9
                }),
                model: None,
            }),
            "a zero cost is not a bad cost, and an empty model is None"
        );
    }

    #[test]
    fn session_closed_translates_every_reason_value_to_a_stable_string() {
        let cases = [
            (SessionCloseReason::ClosedByHost, "closed_by_host"),
            (SessionCloseReason::ProviderExited, "provider_exited"),
            (SessionCloseReason::ProviderFailed, "provider_failed"),
        ];
        for (proto_reason, expected) in cases {
            let event = wrap(ProtoEvent::SessionClosed(SessionClosed {
                reason: proto_reason as i32,
            }));
            assert_eq!(
                translate(event),
                Some(AgentDomainEvent::SessionClosed {
                    reason: expected.to_string()
                })
            );
        }
    }

    #[test]
    fn provider_notice_never_becomes_a_domain_event() {
        let event = wrap(ProtoEvent::ProviderNotice(ProviderNotice {
            kind: "diagnostic".into(),
            subtype: None,
        }));
        assert_eq!(translate(event), None);
    }

    fn ready(permission_mode: &str) -> ProtoSessionEvent {
        wrap(ProtoEvent::SessionReady(SessionReady {
            session_id: "claude-1".into(),
            provider_session_id: "claude-1".into(),
            model: "m".into(),
            cwd: "/tmp".into(),
            permission_mode: permission_mode.into(),
            ..Default::default()
        }))
    }

    fn opened() -> AgentDomainEvent {
        AgentDomainEvent::SessionOpened {
            session_id: "sess-1".into(),
            provider_session_id: "claude-1".into(),
            model: "m".into(),
            cwd: "/tmp".into(),
        }
    }

    fn mode_changed(mode: ProtoPermissionMode, provider_mode: &str) -> ProtoSessionEvent {
        wrap(ProtoEvent::PermissionModeChanged(PermissionModeChanged {
            mode: mode as i32,
            permission_mode: provider_mode.into(),
            bypass_default_deny_applied: false,
        }))
    }

    /// D12: a `SessionReady` whose CLI reports a mode less restrictive than `default` -- a project's
    /// own `permissions.defaultMode`, most likely -- opened a session that must be closed. Both are
    /// said, the opening first. An unknown value resolves toward stopping.
    #[test]
    fn a_session_ready_reporting_a_less_restrictive_mode_opens_and_trips() {
        for reported in ["bypassPermissions", "acceptEdits", "auto", "weird"] {
            assert_eq!(
                super::translate(ready(reported), RequestedCliMode::Default),
                vec![
                    opened(),
                    AgentDomainEvent::UngatedCliMode {
                        reported: reported.into(),
                        detail: "SessionReady".into(),
                    },
                ],
                "{reported}"
            );
        }
    }

    #[test]
    fn a_session_ready_in_default_a_stricter_mode_or_none_only_opens() {
        for reported in ["default", "plan", "dontAsk", ""] {
            assert_eq!(
                super::translate(ready(reported), RequestedCliMode::Default),
                vec![opened()],
                "{reported:?}"
            );
        }
    }

    /// Nothing asks for a switch since R07, so a `PermissionModeChanged` is unsolicited: `default` is
    /// the no-op it always folded to, and anything less restrictive -- by name, or the BYPASS enum
    /// alone -- closes the session (D12).
    #[test]
    fn a_permission_mode_change_trips_unless_it_reports_default() {
        assert_eq!(
            super::translate(
                mode_changed(ProtoPermissionMode::Bypass, "bypassPermissions"),
                RequestedCliMode::Default
            ),
            vec![AgentDomainEvent::UngatedCliMode {
                reported: "bypassPermissions".into(),
                detail: "PermissionModeChanged".into(),
            }]
        );
        assert_eq!(
            super::translate(
                mode_changed(ProtoPermissionMode::Interactive, "acceptEdits"),
                RequestedCliMode::Default
            ),
            vec![AgentDomainEvent::UngatedCliMode {
                reported: "acceptEdits".into(),
                detail: "PermissionModeChanged".into(),
            }]
        );
        // A contradictory report fails closed on the enum.
        assert_eq!(
            super::translate(
                mode_changed(ProtoPermissionMode::Bypass, "default"),
                RequestedCliMode::Default
            ),
            vec![AgentDomainEvent::UngatedCliMode {
                reported: "BYPASS".into(),
                detail: "PermissionModeChanged".into(),
            }]
        );
        assert_eq!(
            super::translate(
                mode_changed(ProtoPermissionMode::Interactive, "default"),
                RequestedCliMode::Default
            ),
            vec![AgentDomainEvent::PermissionModeChanged {
                mode: crate::PermissionMode::Auto,
                provider_mode: "default".into(),
                floor_applied: false,
            }]
        );
        // UNSPECIFIED with nothing ungated about it is still dropped, loudly.
        let none = wrap(ProtoEvent::PermissionModeChanged(PermissionModeChanged::default()));
        assert_eq!(super::translate(none, RequestedCliMode::Default), vec![]);
        // And UNSPECIFIED never hides an ungated name.
        assert_eq!(
            super::translate(
                mode_changed(ProtoPermissionMode::Unspecified, "bypassPermissions"),
                RequestedCliMode::Default
            ),
            vec![AgentDomainEvent::UngatedCliMode {
                reported: "bypassPermissions".into(),
                detail: "PermissionModeChanged".into(),
            }]
        );
    }

    /// Stricter and unreported modes are one line per session, not one per turn's `SessionReady`;
    /// `default` and the tripping ones earn none here (the trip is `translate`'s).
    #[test]
    fn a_stricter_or_unreported_mode_is_noted_once_per_session() {
        let mut note = CliModeNote::default();
        assert_eq!(note.observe(&ready("default")), None);
        assert_eq!(note.observe(&ready("bypassPermissions")), None);
        let line = note
            .observe(&ready("plan"))
            .expect("the first stricter report is noted");
        assert!(line.contains("'plan'") && line.contains("SessionReady"), "{line}");
        assert_eq!(note.observe(&ready("plan")), None, "once per session");
        assert_eq!(note.observe(&ready("")), None, "once per session, whatever the reason");

        let mut fresh = CliModeNote::default();
        let line = fresh.observe(&ready("")).expect("an unreported mode is noted too");
        assert!(line.contains("no CLI permission mode"), "{line}");
        let mut other = CliModeNote::default();
        assert!(other
            .observe(&mode_changed(ProtoPermissionMode::Interactive, "dontAsk"))
            .is_some_and(|l| l.contains("PermissionModeChanged")));
    }

    /// D12 on a session that asked for the CLI's own auto mode: `auto` and the `default` fallback
    /// open the session and say which (`CliPermissionMode`, read by the answer path); everything
    /// else opens and trips -- the stricter modes and an empty report included, which a `default`
    /// session would only note.
    #[test]
    fn a_session_that_asked_for_auto_accepts_auto_or_default_and_trips_on_anything_else() {
        let auto = RequestedCliMode::Auto;
        for accepted in ["auto", "default"] {
            assert_eq!(
                super::translate(ready(accepted), auto),
                vec![
                    opened(),
                    AgentDomainEvent::CliPermissionMode {
                        reported: accepted.into()
                    }
                ],
                "{accepted}"
            );
        }
        for reported in ["bypassPermissions", "acceptEdits", "plan", "dontAsk", "", "weird"] {
            assert_eq!(
                super::translate(ready(reported), auto),
                vec![
                    opened(),
                    AgentDomainEvent::UngatedCliMode {
                        reported: reported.into(),
                        detail: "SessionReady".into(),
                    },
                ],
                "{reported:?}"
            );
        }
    }

    /// The other half: a session that did not ask for auto never says `CliPermissionMode`, so the
    /// answer path can never read an `auto` it did not ask for -- and `auto` there still trips (the
    /// tests above, on `RequestedCliMode::Default`).
    #[test]
    fn a_session_that_asked_for_default_never_reports_a_cli_permission_mode() {
        for reported in ["default", "plan", "dontAsk", "", "auto", "bypassPermissions"] {
            assert!(
                !super::translate(ready(reported), RequestedCliMode::Default)
                    .iter()
                    .any(|e| matches!(e, AgentDomainEvent::CliPermissionMode { .. })),
                "{reported:?}"
            );
        }
    }

    /// A session created with the CLI's auto mode refuses every switch, so any report of one closes
    /// it -- `default` included, which a `default` session folds as a no-op.
    #[test]
    fn any_permission_mode_change_on_an_auto_session_trips() {
        for (mode, name, expected) in [
            (ProtoPermissionMode::Interactive, "default", "default"),
            (ProtoPermissionMode::Bypass, "bypassPermissions", "bypassPermissions"),
            (ProtoPermissionMode::Unspecified, "", "a mode switch"),
        ] {
            assert_eq!(
                super::translate(mode_changed(mode, name), RequestedCliMode::Auto),
                vec![AgentDomainEvent::UngatedCliMode {
                    reported: expected.into(),
                    detail: "PermissionModeChanged".into(),
                }],
                "{mode:?} {name:?}"
            );
        }
    }

    /// The CLI's own refusal reaches the domain verbatim, tied to its call; proto3's empty strings
    /// are no id (the shared link rule), and the optional fields stay absent when the CLI gave none.
    #[test]
    fn a_permission_denied_carries_the_clis_words_and_its_call() {
        use claude_runtime_protocol::v1::PermissionDenied as ProtoPermissionDenied;
        let event = wrap(ProtoEvent::PermissionDenied(ProtoPermissionDenied {
            tool_use_id: "toolu_017CZ9vk9wjuHQmght9KZgQo".into(),
            tool_name: "Bash".into(),
            reason_type: Some("classifier".into()),
            reason: Some("[Git Destructive]".into()),
        }));
        assert_eq!(
            translate(event),
            Some(AgentDomainEvent::PermissionDenied {
                tool_use_id: Some("toolu_017CZ9vk9wjuHQmght9KZgQo".into()),
                tool_name: "Bash".into(),
                reason_type: Some("classifier".into()),
                reason: Some("[Git Destructive]".into()),
            })
        );
        let bare = wrap(ProtoEvent::PermissionDenied(ProtoPermissionDenied {
            tool_name: "Bash".into(),
            ..Default::default()
        }));
        assert_eq!(
            translate(bare),
            Some(AgentDomainEvent::PermissionDenied {
                tool_use_id: None,
                tool_name: "Bash".into(),
                reason_type: None,
                reason: None,
            })
        );
    }

    /// On an auto session the fallback to `default` is the one line due, once; `auto` itself earns
    /// none.
    #[test]
    fn an_auto_session_notes_its_fallback_to_default_once() {
        let mut note = CliModeNote::new(RequestedCliMode::Auto);
        assert_eq!(note.observe(&ready("auto")), None);
        let line = note.observe(&ready("default")).expect("the fallback is noted");
        assert!(
            line.contains("asked for its auto mode") && line.contains("'default'"),
            "{line}"
        );
        assert_eq!(note.observe(&ready("default")), None, "once per session");
    }

    #[test]
    fn a_tool_policy_violation_close_has_its_own_reason() {
        assert_eq!(
            translate_close_reason(SessionCloseReason::ToolPolicyViolation),
            "tool_policy_violation"
        );
    }

    fn text(turn: &str, id: Option<&str>, text: &str) -> ProtoSessionEvent {
        wrap(ProtoEvent::TextDelta(TextDelta {
            turn_id: turn.into(),
            text: text.into(),
            message_id: id.map(Into::into),
        }))
    }

    fn boundary(turn: &str) -> Option<AgentDomainEvent> {
        Some(AgentDomainEvent::AssistantMessageBoundary { turn_id: turn.into() })
    }

    #[test]
    fn a_new_message_id_closes_the_message_before_it() {
        let mut split = MessageSplit::new(true);
        assert_eq!(
            split.before(&text("t1", Some("msg_a"), "After the table.")),
            None,
            "the first text opens"
        );
        assert_eq!(
            split.before(&text("t1", Some("msg_a"), " more")),
            None,
            "same message continues"
        );
        assert_eq!(split.before(&text("t1", Some("msg_b"), "TURN-1-DONE")), boundary("t1"));
        assert_eq!(split.before(&text("t1", None, "x")), None, "no id says nothing");
    }

    #[test]
    fn a_turn_starts_fresh_and_thinking_never_splits() {
        let mut split = MessageSplit::new(true);
        split.before(&text("t1", Some("msg_a"), "a"));
        assert_eq!(
            split.before(&wrap(ProtoEvent::TurnStarted(TurnStarted { turn_id: "t2".into() }))),
            None
        );
        assert_eq!(
            split.before(&text("t2", Some("msg_c"), "b")),
            None,
            "a new turn is already a new message"
        );
        let thinking = wrap(ProtoEvent::ThinkingDelta(ThinkingDelta {
            turn_id: "t2".into(),
            text: "…".into(),
            message_id: Some("msg_d".into()),
        }));
        assert_eq!(split.before(&thinking), None);
    }

    #[test]
    fn without_the_capability_nothing_splits() {
        let mut split = MessageSplit::new(false);
        split.before(&text("t1", Some("msg_a"), "a"));
        assert_eq!(split.before(&text("t1", Some("msg_b"), "b")), None);
    }
}
