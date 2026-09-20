//! Pure, I/O-free translation from the sidecar's wire-level `SessionEvent` (proto) to this crate's
//! own provider-neutral `AgentDomainEvent` (design doc §10.2's event-flow layering: "Claude gRPC
//! event -> AgentDomainEvent"). No I/O, no async -- every case is covered by a plain unit test with
//! no sidecar process involved, matching this crate's own established preference for testing a
//! pure reducer/translator in isolation before any real-process integration test exercises it.

use crate::{AgentDomainEvent, ContentKind, PermissionOutcome, ResumeStatus, TurnOutcome};
use claude_runtime_protocol::v1::session_event::Event as ProtoEvent;
use claude_runtime_protocol::v1::{
    PermissionOutcome as ProtoPermissionOutcome, ResumeStatus as ProtoResumeStatus, SessionCloseReason,
    SessionEvent as ProtoSessionEvent, TurnOutcome as ProtoTurnOutcome,
};

/// Translates one proto `SessionEvent`'s wire fields into this crate's own domain event. Returns
/// `None` for a genuinely malformed message (unset `oneof`, or unparseable `input_json`/
/// `content_json`) -- logged via `eprintln!`, never panics (Global Constraints), and the caller
/// (Task 8's watch-loop) simply skips that one occurrence rather than tearing down the whole
/// stream over one bad event.
pub(crate) fn translate(event: ProtoSessionEvent) -> Option<AgentDomainEvent> {
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
    match event.event? {
        ProtoEvent::SessionReady(ready) => Some(AgentDomainEvent::SessionOpened {
            session_id: envelope_session_id,
            provider_session_id: ready.provider_session_id,
            model: ready.model,
            cwd: ready.cwd,
        }),
        ProtoEvent::TurnStarted(started) => Some(AgentDomainEvent::TurnStarted {
            turn_id: started.turn_id,
        }),
        ProtoEvent::TextDelta(delta) => Some(AgentDomainEvent::ContentDelta {
            turn_id: delta.turn_id,
            kind: ContentKind::Text,
            text: delta.text,
        }),
        ProtoEvent::ThinkingDelta(delta) => Some(AgentDomainEvent::ContentDelta {
            turn_id: delta.turn_id,
            kind: ContentKind::Thinking,
            text: delta.text,
        }),
        ProtoEvent::ToolCallStarted(started) => {
            match serde_json::from_str(&started.input_json) {
                Ok(input) => Some(AgentDomainEvent::ToolCallStarted {
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
            Ok(content) => Some(AgentDomainEvent::ToolCallCompleted {
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
            Ok(input) => Some(AgentDomainEvent::PermissionRequested {
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
            Some(AgentDomainEvent::PermissionResolved {
                permission_id: resolved.permission_id,
                outcome,
            })
        }
        ProtoEvent::TurnCompleted(completed) => {
            let outcome = translate_turn_outcome(completed.outcome());
            Some(AgentDomainEvent::TurnCompleted {
                turn_id: completed.turn_id,
                outcome,
                result_text: completed.result_text,
                stop_reason: completed.stop_reason,
                // No source data: the v1 sidecar proto's TurnCompleted carries no cost/usage fields
                // at all (confirmed by reading proto/verdandi/claude/runtime/v1/runtime.proto
                // directly -- the design doc's own §9.6 "suggested" UsageUpdated event was never
                // implemented this round). `None` says exactly that, and is the whole reason the
                // field is an `Option`: this site previously sent `0.0`/`0`, which the projection
                // stored as a measured value indistinguishable from a real free turn. A Verdandi
                // `TurnUsage` field is planned but does not exist, so do not reintroduce a
                // placeholder here in anticipation of it -- send `Some` when there is something to
                // put in it.
                usage: None,
            })
        }
        ProtoEvent::SessionClosed(closed) => Some(AgentDomainEvent::SessionClosed {
            reason: translate_close_reason(closed.reason()),
        }),
        ProtoEvent::ResumeOutcome(outcome) => {
            let status = translate_resume_status(outcome.status());
            Some(AgentDomainEvent::ResumeOutcome {
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
    }
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
        SessionCloseReason::Unspecified => "unspecified".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_runtime_protocol::v1::{
        PermissionOutcome as ProtoPermOutcome, PermissionRequested, PermissionResolved, ProviderNotice,
        SessionCloseReason, SessionClosed, SessionReady, TextDelta, ThinkingDelta, ToolCallCompleted, ToolCallStarted,
        TurnCompleted, TurnOutcome as ProtoTOutcome, TurnStarted,
    };

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
            // Nothing on this side reads it yet, so the fixture carries the empty string rather
            // than a value a reader could mistake for an assertion.
            permission_mode: String::new(),
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
        }));
        assert_eq!(
            translate(event),
            Some(AgentDomainEvent::PermissionRequested {
                permission_id: "perm-1".into(),
                tool_use_id: Some("tu-1".into()),
                tool_name: "Write".into(),
                input: serde_json::json!({}),
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
        }));
        assert_eq!(
            translate(event),
            Some(AgentDomainEvent::PermissionRequested {
                permission_id: "perm-1".into(),
                tool_use_id: None,
                tool_name: "Write".into(),
                input: serde_json::json!({}),
            })
        );
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
    /// the wire never sent. `None` is the honest translation of a message with no usage fields, and
    /// asserting it here is what stops a placeholder creeping back in ahead of a real
    /// Verdandi-side usage field.
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
            }));
            assert_eq!(
                translate(event),
                Some(AgentDomainEvent::TurnCompleted {
                    turn_id: "turn-1".into(),
                    outcome: expected,
                    result_text: "done".into(),
                    stop_reason: Some("end_turn".into()),
                    usage: None,
                })
            );
        }
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
}
