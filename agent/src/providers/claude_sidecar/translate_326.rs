//! Protocol 3.26's plan approvals (`plan_mode_v1`): a plan the model proposed (`InteractionRequested` of
//! kind `PLAN_APPROVAL`) and its settlement (`InteractionResolved` naming that kind), into provider-neutral
//! events. Only a session that opted into plan mode is sent either.
//!
//! **A plan is carried whole or refused, never cut.** The sidecar holds a plan to its bounds and refuses
//! one past them (no payload, `unsupported_reason`, then `HOST_REFUSED`); the same bounds are checked here
//! again, with the plan's digest, so a plan this side shows is exactly the one an approval names. A plan
//! that fails a check arrives with its reason and no text, and takes no decision.

use super::translate_322::{bound, fits};
use crate::projection::{
    planning::{PLAN_ALLOWED_PROMPTS_MAX, PLAN_ALLOWED_PROMPT_UNITS_MAX, PLAN_FEEDBACK_MAX_BYTES, PLAN_MAX_BYTES},
    AgentDomainEvent as Domain, PlanAllowedPrompt, PlanExecutionMode, PlanOutcome,
};
use claude_runtime_protocol::v1 as wire;
use sha2::{Digest, Sha256};

/// The longest reason kept from the wire, as for a question.
const REASON_CHARS: usize = 2000;

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The plan and its prompts, when every bound holds and the digest is the plan's; otherwise why not.
fn checked(proposal: wire::PlanProposal) -> Result<(String, String, Vec<PlanAllowedPrompt>), String> {
    let wire::PlanProposal {
        plan,
        plan_sha256,
        allowed_prompts,
    } = proposal;
    if plan.is_empty() {
        return Err("The plan is empty".into());
    }
    if plan.len() > PLAN_MAX_BYTES {
        return Err(format!(
            "The plan is longer than {} KiB, so it was not shown",
            PLAN_MAX_BYTES / 1024
        ));
    }
    if plan_sha256 != sha256_hex(&plan) {
        return Err("The plan's digest does not match its text".into());
    }
    if allowed_prompts.len() > PLAN_ALLOWED_PROMPTS_MAX
        || allowed_prompts.iter().any(|prompt| {
            !fits(&prompt.tool, PLAN_ALLOWED_PROMPT_UNITS_MAX) || !fits(&prompt.prompt, PLAN_ALLOWED_PROMPT_UNITS_MAX)
        })
    {
        return Err("The prompts the plan lists exceed supported bounds".into());
    }
    let allowed_prompts = allowed_prompts
        .into_iter()
        .map(|wire::PlanAllowedPrompt { tool, prompt }| PlanAllowedPrompt { tool, prompt })
        .collect();
    Ok((plan, plan_sha256, allowed_prompts))
}

/// A plan the model proposed. The ids are refused rather than cut, as a question's are: an approval names
/// them back.
pub(super) fn plan_requested(value: wire::InteractionRequested) -> Domain {
    let wire::InteractionRequested {
        interaction_id,
        tool_use_id,
        kind: _,
        payload,
        unsupported_reason,
    } = value;
    let mut unsupported = unsupported_reason;
    let proposal = match payload {
        Some(wire::interaction_requested::Payload::Plan(proposal)) => Some(proposal),
        // A question set on a plan approval is no plan at all.
        Some(wire::interaction_requested::Payload::Questions(_)) | None => {
            unsupported.get_or_insert_with(|| "Plan payload is missing".into());
            None
        }
    };
    let mut carried = None;
    // A plan the sidecar itself refused is not read, whatever arrived beside the reason.
    if let (Some(proposal), None) = (proposal, &unsupported) {
        match checked(proposal) {
            Ok(plan) => carried = Some(plan),
            Err(why) => unsupported = Some(why),
        }
    }
    let ids_fit = fits(&interaction_id, 128) && fits(&tool_use_id, 128);
    if !ids_fit {
        unsupported = Some("Plan identifier exceeds supported bounds".into());
        carried = None;
    }
    let (plan, plan_sha256, allowed_prompts) = carried.unwrap_or_default();
    Domain::PlanProposed {
        interaction_id: if ids_fit { interaction_id } else { String::new() },
        tool_use_id: if ids_fit {
            crate::projection::tool_use_link(tool_use_id)
        } else {
            None
        },
        plan,
        plan_sha256,
        allowed_prompts,
        unsupported_reason: unsupported.map(|reason| bound(&reason, REASON_CHARS)),
    }
}

/// How a plan ended. An outcome that is unset, unknown or a question's still ends the plan, as a failure,
/// never as an approval: dropping it would leave the plan waiting for good.
fn plan_outcome(value: i32) -> PlanOutcome {
    match wire::InteractionOutcome::try_from(value) {
        Ok(wire::InteractionOutcome::Approved) => PlanOutcome::Approved,
        Ok(wire::InteractionOutcome::KeptPlanning) => PlanOutcome::KeptPlanning,
        Ok(wire::InteractionOutcome::Interrupted) => PlanOutcome::Interrupted,
        Ok(wire::InteractionOutcome::Expired) => PlanOutcome::Expired,
        Ok(wire::InteractionOutcome::SessionClosed) => PlanOutcome::SessionClosed,
        Ok(wire::InteractionOutcome::ProviderFailed) => PlanOutcome::ProviderFailed,
        Ok(wire::InteractionOutcome::HostRefused) => PlanOutcome::HostRefused,
        Ok(
            wire::InteractionOutcome::Unspecified
            | wire::InteractionOutcome::Answered
            | wire::InteractionOutcome::Declined
            | wire::InteractionOutcome::ChatRequested,
        )
        | Err(_) => {
            eprintln!("agent: ClaudeSidecarProvider: plan settled with outcome {value}, read as provider_failed");
            PlanOutcome::ProviderFailed
        }
    }
}

/// The mode an approval named; unset or unknown is none, which the host reads as Manual.
fn execution_mode(value: i32) -> Option<PlanExecutionMode> {
    match wire::PlanExecutionMode::try_from(value) {
        Ok(wire::PlanExecutionMode::Manual) => Some(PlanExecutionMode::Manual),
        Ok(wire::PlanExecutionMode::Auto) => Some(PlanExecutionMode::Auto),
        Ok(wire::PlanExecutionMode::Bypass) => Some(PlanExecutionMode::Bypass),
        Ok(wire::PlanExecutionMode::Unspecified) | Err(_) => None,
    }
}

/// A plan's settlement. `mode_after` is read only from an approval and `feedback` only from "keep
/// planning"; feedback past the sidecar's own bound is left out (the outcome stands) and the reason says
/// so.
pub(super) fn plan_resolved(value: wire::InteractionResolved) -> Option<Domain> {
    let wire::InteractionResolved {
        interaction_id,
        outcome,
        answers,
        reason,
        kind: _,
        mode_after,
        feedback,
    } = value;
    if !fits(&interaction_id, 128) {
        eprintln!("agent: ClaudeSidecarProvider: plan settlement with an identifier past its bound, dropped");
        return None;
    }
    if !answers.is_empty() {
        eprintln!("agent: ClaudeSidecarProvider: plan settlement carrying question answers, which are ignored");
    }
    let outcome = plan_outcome(outcome);
    let mut reason = reason;
    let feedback = match (outcome, feedback) {
        (PlanOutcome::KeptPlanning, Some(feedback)) if feedback.len() <= PLAN_FEEDBACK_MAX_BYTES => Some(feedback),
        (PlanOutcome::KeptPlanning, Some(_)) => {
            reason = Some("The feedback exceeds supported bounds".into());
            None
        }
        _ => None,
    };
    Some(Domain::PlanSettled {
        interaction_id,
        outcome,
        mode_after: if outcome == PlanOutcome::Approved {
            mode_after.and_then(execution_mode)
        } else {
            None
        },
        feedback,
        reason: reason.map(|reason| bound(&reason, REASON_CHARS)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{CliExpectation, RequestedCliMode};
    use wire::session_event::Event;

    fn translate(event: Event, expect: CliExpectation) -> Vec<Domain> {
        super::super::translate::translate(
            wire::SessionEvent {
                event: Some(event),
                ..Default::default()
            },
            expect,
        )
    }

    fn proposal(plan: &str) -> wire::InteractionRequested {
        wire::InteractionRequested {
            interaction_id: "p1".into(),
            tool_use_id: "toolu_exit".into(),
            kind: wire::InteractionKind::PlanApproval as i32,
            payload: Some(wire::interaction_requested::Payload::Plan(wire::PlanProposal {
                plan: plan.into(),
                plan_sha256: sha256_hex(plan),
                allowed_prompts: vec![wire::PlanAllowedPrompt {
                    tool: "Bash".into(),
                    prompt: "run the tests".into(),
                }],
            })),
            unsupported_reason: None,
        }
    }

    fn edit(value: &mut wire::InteractionRequested) -> &mut wire::PlanProposal {
        let Some(wire::interaction_requested::Payload::Plan(plan)) = value.payload.as_mut() else {
            panic!("a plan")
        };
        plan
    }

    fn refused(value: wire::InteractionRequested) -> Option<String> {
        let Domain::PlanProposed {
            plan,
            plan_sha256,
            allowed_prompts,
            unsupported_reason,
            ..
        } = plan_requested(value)
        else {
            panic!("a plan")
        };
        if unsupported_reason.is_some() {
            assert!(plan.is_empty() && plan_sha256.is_empty() && allowed_prompts.is_empty());
        }
        unsupported_reason
    }

    fn settlement(outcome: wire::InteractionOutcome) -> wire::InteractionResolved {
        wire::InteractionResolved {
            interaction_id: "p1".into(),
            outcome: outcome as i32,
            kind: wire::InteractionKind::PlanApproval as i32,
            ..Default::default()
        }
    }

    /// A plan reaches the projection byte for byte, its digest and prompts with it; one past 256 KiB is
    /// refused with its reason and no text at all, never cut to fit, and so is one whose digest is not its
    /// text's.
    #[test]
    fn a_plan_interaction_is_lossless_and_refused_not_cut_over_256_kib() {
        let text = "# Plan\n\n1. Read `src/a.rs`\u{200b}\n2. <script>x</script>\n\u{1F600}";
        assert_eq!(
            plan_requested(proposal(text)),
            Domain::PlanProposed {
                interaction_id: "p1".into(),
                tool_use_id: Some("toolu_exit".into()),
                plan: text.into(),
                plan_sha256: sha256_hex(text),
                allowed_prompts: vec![PlanAllowedPrompt {
                    tool: "Bash".into(),
                    prompt: "run the tests".into(),
                }],
                unsupported_reason: None,
            }
        );
        // Exactly at the bound, counted in UTF-8 bytes: carried whole.
        let at_bound = "é".repeat(PLAN_MAX_BYTES / 2);
        assert_eq!(at_bound.len(), PLAN_MAX_BYTES);
        let Domain::PlanProposed { plan, .. } = plan_requested(proposal(&at_bound)) else {
            panic!("a plan")
        };
        assert_eq!(plan, at_bound);
        let over = format!("{at_bound}x");
        assert!(refused(proposal(&over)).unwrap().contains("256 KiB"));
        let mut wrong_digest = proposal(text);
        edit(&mut wrong_digest).plan_sha256 = sha256_hex("another plan");
        assert!(refused(wrong_digest).unwrap().contains("digest"));
        let mut upper = proposal(text);
        let digest = edit(&mut upper).plan_sha256.to_uppercase();
        edit(&mut upper).plan_sha256 = digest;
        assert!(refused(upper).is_some(), "the digest is lower-case hex");
        assert!(refused(proposal("")).is_some());
        // The sidecar's own refusal: no payload, a reason, and nothing read beside it.
        let mut by_sidecar = proposal(text);
        by_sidecar.payload = None;
        by_sidecar.unsupported_reason = Some("plan_too_long".into());
        assert_eq!(refused(by_sidecar).as_deref(), Some("plan_too_long"));
        let mut both = proposal(text);
        both.unsupported_reason = Some("refused".into());
        assert_eq!(refused(both).as_deref(), Some("refused"));
        let mut missing = proposal(text);
        missing.payload = None;
        assert!(refused(missing).unwrap().contains("missing"));
        // Identifiers are refused rather than cut: an approval names them back.
        let mut long_id = proposal(text);
        long_id.interaction_id = "i".repeat(129);
        assert!(matches!(
            plan_requested(long_id),
            Domain::PlanProposed { interaction_id, plan, unsupported_reason: Some(_), .. }
                if interaction_id.is_empty() && plan.is_empty()
        ));
    }

    #[test]
    fn allowed_prompts_over_bounds_refuse_the_plan() {
        let prompt = |tool: &str, prompt: &str| wire::PlanAllowedPrompt {
            tool: tool.into(),
            prompt: prompt.into(),
        };
        let mut at_bounds = proposal("# Plan");
        edit(&mut at_bounds).allowed_prompts = (0..PLAN_ALLOWED_PROMPTS_MAX)
            .map(|_| prompt(&"t".repeat(1000), &"\u{1F600}".repeat(500)))
            .collect();
        assert_eq!(refused(at_bounds), None);
        let mut too_many = proposal("# Plan");
        edit(&mut too_many).allowed_prompts = (0..=PLAN_ALLOWED_PROMPTS_MAX).map(|_| prompt("Bash", "x")).collect();
        assert!(refused(too_many).unwrap().contains("prompts"));
        let mut long_tool = proposal("# Plan");
        edit(&mut long_tool).allowed_prompts = vec![prompt(&"t".repeat(1001), "x")];
        assert!(refused(long_tool).is_some());
        // Counted in UTF-16 units: a character outside the BMP is two.
        let mut long_prompt = proposal("# Plan");
        edit(&mut long_prompt).allowed_prompts = vec![prompt("Bash", &"\u{1F600}".repeat(501))];
        assert!(refused(long_prompt).is_some());
    }

    #[test]
    fn a_plan_settlement_maps_every_outcome_and_mode() {
        for (outcome, expected) in [
            (wire::InteractionOutcome::Approved, PlanOutcome::Approved),
            (wire::InteractionOutcome::KeptPlanning, PlanOutcome::KeptPlanning),
            (wire::InteractionOutcome::Interrupted, PlanOutcome::Interrupted),
            (wire::InteractionOutcome::Expired, PlanOutcome::Expired),
            (wire::InteractionOutcome::SessionClosed, PlanOutcome::SessionClosed),
            (wire::InteractionOutcome::ProviderFailed, PlanOutcome::ProviderFailed),
            (wire::InteractionOutcome::HostRefused, PlanOutcome::HostRefused),
        ] {
            let mut value = settlement(outcome);
            value.mode_after = Some(wire::PlanExecutionMode::Auto as i32);
            value.feedback = Some("keep going".into());
            value.reason = Some("why".into());
            let approved = expected == PlanOutcome::Approved;
            let kept = expected == PlanOutcome::KeptPlanning;
            assert_eq!(
                plan_resolved(value),
                Some(Domain::PlanSettled {
                    interaction_id: "p1".into(),
                    outcome: expected,
                    mode_after: approved.then_some(PlanExecutionMode::Auto),
                    feedback: kept.then(|| "keep going".to_string()),
                    reason: Some("why".into()),
                }),
                "{outcome:?}"
            );
        }
        for (mode, expected) in [
            (
                Some(wire::PlanExecutionMode::Manual as i32),
                Some(PlanExecutionMode::Manual),
            ),
            (
                Some(wire::PlanExecutionMode::Auto as i32),
                Some(PlanExecutionMode::Auto),
            ),
            (
                Some(wire::PlanExecutionMode::Bypass as i32),
                Some(PlanExecutionMode::Bypass),
            ),
            (Some(wire::PlanExecutionMode::Unspecified as i32), None),
            (Some(42), None),
            (None, None),
        ] {
            let mut value = settlement(wire::InteractionOutcome::Approved);
            value.mode_after = mode;
            assert!(
                matches!(plan_resolved(value), Some(Domain::PlanSettled { outcome: PlanOutcome::Approved, mode_after, .. }) if mode_after == expected),
                "{mode:?}"
            );
        }
        // Feedback is carried exactly, empty included; past the sidecar's bound it is left out, said why.
        let mut empty = settlement(wire::InteractionOutcome::KeptPlanning);
        empty.feedback = Some(String::new());
        assert!(matches!(plan_resolved(empty), Some(Domain::PlanSettled { feedback: Some(f), .. }) if f.is_empty()));
        let mut long = settlement(wire::InteractionOutcome::KeptPlanning);
        long.feedback = Some("f".repeat(PLAN_FEEDBACK_MAX_BYTES + 1));
        assert!(matches!(
            plan_resolved(long),
            Some(Domain::PlanSettled {
                outcome: PlanOutcome::KeptPlanning,
                feedback: None,
                reason: Some(_),
                ..
            })
        ));
        // Through the translator: a settlement naming the plan kind is a plan's.
        assert!(matches!(
            translate(
                Event::InteractionResolved(settlement(wire::InteractionOutcome::Approved)),
                RequestedCliMode::Default.into()
            )
            .as_slice(),
            [Domain::PlanSettled {
                outcome: PlanOutcome::Approved,
                ..
            }]
        ));
        let offered = CliExpectation {
            requested: RequestedCliMode::Default,
            plan_offered: true,
        };
        assert!(matches!(
            translate(Event::InteractionRequested(proposal("# Plan")), offered).as_slice(),
            [Domain::PlanProposed {
                unsupported_reason: None,
                ..
            }]
        ));
        // Without the offer the request is refused as an unknown kind: no plan waits that nothing could settle.
        assert!(matches!(
            translate(
                Event::InteractionRequested(proposal("# Plan")),
                RequestedCliMode::Default.into()
            )
            .as_slice(),
            [Domain::QuestionAsked {
                unsupported_reason: Some(_),
                ..
            }]
        ));
    }

    #[test]
    fn an_unknown_outcome_settles_as_provider_failed() {
        for outcome in [
            wire::InteractionOutcome::Unspecified as i32,
            wire::InteractionOutcome::Answered as i32,
            wire::InteractionOutcome::Declined as i32,
            wire::InteractionOutcome::ChatRequested as i32,
            77,
        ] {
            let mut value = settlement(wire::InteractionOutcome::Approved);
            value.outcome = outcome;
            value.mode_after = Some(wire::PlanExecutionMode::Bypass as i32);
            assert!(
                matches!(
                    plan_resolved(value),
                    Some(Domain::PlanSettled {
                        outcome: PlanOutcome::ProviderFailed,
                        mode_after: None,
                        ..
                    })
                ),
                "{outcome}"
            );
        }
    }

    /// A sidecar older than the field sends no kind, and its settlements are questions'; so is a kind this
    /// build does not know. Neither can settle a plan.
    #[test]
    fn a_resolved_without_kind_is_a_question() {
        for kind in [
            wire::InteractionKind::Unspecified as i32,
            wire::InteractionKind::Question as i32,
            7,
        ] {
            let mut value = settlement(wire::InteractionOutcome::Declined);
            value.kind = kind;
            assert!(
                matches!(
                    translate(Event::InteractionResolved(value), RequestedCliMode::Default.into()).as_slice(),
                    [Domain::QuestionSettled { .. }]
                ),
                "{kind}"
            );
        }
    }

    fn ready(mode: &str) -> Event {
        Event::SessionReady(wire::SessionReady {
            permission_mode: mode.into(),
            ..Default::default()
        })
    }

    fn reported(mode: &str) -> Event {
        Event::CliModeReported(wire::CliModeReported {
            permission_mode: mode.into(),
        })
    }

    /// The events of `event` that say something about the CLI's mode.
    fn mode_events(event: Event, expect: CliExpectation) -> Vec<Domain> {
        translate(event, expect)
            .into_iter()
            .filter(|event| !matches!(event, Domain::SessionOpened { .. }))
            .collect()
    }

    /// On a session that offered plan mode, `plan` is its own report, and every mode the session accepts
    /// besides is reported as itself, `CliLeftAuto` never: the projection tells a step out of plan mode
    /// from a fallback out of auto.
    #[test]
    fn a_plan_session_reports_plan_and_every_accepted_mode() {
        for requested in [RequestedCliMode::Default, RequestedCliMode::Auto] {
            let expect = CliExpectation {
                requested,
                plan_offered: true,
            };
            assert_eq!(
                mode_events(ready("plan"), expect),
                [Domain::CliPlanMode {
                    source: "SessionReady".into()
                }]
            );
            assert_eq!(
                mode_events(reported("plan"), expect),
                [Domain::CliPlanMode {
                    source: "CliModeReported".into()
                }]
            );
            for event in [ready("default"), reported("default")] {
                assert_eq!(
                    mode_events(event, expect),
                    [Domain::CliPermissionMode {
                        reported: "default".into()
                    }]
                );
            }
            for mode in ["bypassPermissions", "acceptEdits", "dontAsk", "weird"] {
                assert!(
                    matches!(
                        mode_events(reported(mode), expect).as_slice(),
                        [Domain::UngatedCliMode { .. }]
                    ) || (mode == "dontAsk" && requested == RequestedCliMode::Default),
                    "{mode} on {requested:?}"
                );
            }
        }
        let auto = CliExpectation {
            requested: RequestedCliMode::Auto,
            plan_offered: true,
        };
        assert_eq!(
            mode_events(reported("auto"), auto),
            [Domain::CliPermissionMode {
                reported: "auto".into()
            }]
        );
        // A kind that contradicts the string still closes the session.
        let contradiction = Event::SessionReady(wire::SessionReady {
            permission_mode: "plan".into(),
            cli_permission_mode_kind: wire::CliPermissionModeKind::Auto as i32,
            ..Default::default()
        });
        assert!(matches!(
            mode_events(contradiction, auto).as_slice(),
            [Domain::UngatedCliMode { .. }]
        ));
        let agreeing = Event::SessionReady(wire::SessionReady {
            permission_mode: "plan".into(),
            cli_permission_mode_kind: wire::CliPermissionModeKind::Plan as i32,
            ..Default::default()
        });
        assert!(matches!(
            mode_events(agreeing, auto).as_slice(),
            [Domain::CliPlanMode { .. }]
        ));
    }

    /// A session that did not offer plan mode is judged exactly as before: `plan` on an auto session
    /// closes it, on a default session it is a stricter mode noted once, and a fallback out of auto
    /// still says `CliLeftAuto`.
    #[test]
    fn a_session_without_plan_mode_keeps_today_s_tripwire() {
        let auto = RequestedCliMode::Auto.into();
        let default = RequestedCliMode::Default.into();
        for event in [ready("plan"), reported("plan")] {
            assert!(matches!(
                mode_events(event, auto).as_slice(),
                [Domain::UngatedCliMode { reported, .. }] if reported == "plan"
            ));
        }
        assert_eq!(mode_events(ready("plan"), default), []);
        assert_eq!(mode_events(reported("plan"), default), []);
        assert_eq!(mode_events(ready("default"), default), []);
        assert_eq!(
            mode_events(reported("default"), auto),
            [
                Domain::CliPermissionMode {
                    reported: "default".into()
                },
                Domain::CliLeftAuto
            ]
        );
    }
}
