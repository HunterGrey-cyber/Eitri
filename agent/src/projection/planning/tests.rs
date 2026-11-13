use super::*;
use crate::{AgentSessionProjection, CliExpectation, QuestionDeclineKind, RequestedCliMode};
use serde_json::json;

const T0: u64 = 1_800_000_000_000;

/// The CLI's own auto mode, for the fixture beside this module, which the wire guard reads as production
/// source (it is reached through a `#[path]`).
pub(super) fn auto() -> RequestedCliMode {
    RequestedCliMode::Auto
}
const ENTER: &str = "toolu_enter";
const EXIT: &str = "toolu_exit";

fn session(requested: RequestedCliMode) -> AgentSessionProjection {
    let mut projection = AgentSessionProjection::default();
    projection.offer_plan_mode(CliExpectation {
        requested,
        plan_offered: true,
    });
    projection
}

fn call(name: &str, tool_use_id: &str) -> AgentDomainEvent {
    AgentDomainEvent::ToolCallStarted {
        turn_id: "t".into(),
        tool_use_id: tool_use_id.into(),
        name: name.into(),
        input: json!({}),
    }
}

fn completed(tool_use_id: &str, is_error: bool) -> AgentDomainEvent {
    AgentDomainEvent::ToolCallCompleted {
        turn_id: "t".into(),
        tool_use_id: tool_use_id.into(),
        content: json!("done"),
        is_error,
    }
}

fn plan_report(source: &str) -> AgentDomainEvent {
    AgentDomainEvent::CliPlanMode { source: source.into() }
}

fn mode(reported: &str) -> AgentDomainEvent {
    AgentDomainEvent::CliPermissionMode {
        reported: reported.into(),
    }
}

fn transition(enter: bool, cause: PlanCause, at_ms: u64) -> AgentDomainEvent {
    AgentDomainEvent::PlanTransitionStarted { enter, cause, at_ms }
}

fn proposed(id: &str) -> AgentDomainEvent {
    AgentDomainEvent::PlanProposed {
        interaction_id: id.into(),
        tool_use_id: Some(EXIT.into()),
        plan: format!("# Plan {id}\n\n1. Do it"),
        plan_sha256: "0".repeat(64),
        allowed_prompts: vec![PlanAllowedPrompt {
            tool: "Bash".into(),
            prompt: "run tests".into(),
        }],
        unsupported_reason: None,
    }
}

fn settled(id: &str, outcome: PlanOutcome, mode_after: Option<PlanExecutionMode>) -> AgentDomainEvent {
    AgentDomainEvent::PlanSettled {
        interaction_id: id.into(),
        outcome,
        mode_after,
        feedback: (outcome == PlanOutcome::KeptPlanning).then(|| "shorter, please".to_string()),
        reason: None,
    }
}

fn tool(id: &str) -> PlanCause {
    PlanCause::Tool { tool_use_id: id.into() }
}

/// A session of `requested` in plan mode, entered by the model at `T0`, a plan waiting.
fn planning(requested: RequestedCliMode) -> AgentSessionProjection {
    let mut p = session(requested);
    p.apply_at(
        &mode(if requested == RequestedCliMode::Default {
            "default"
        } else {
            "auto"
        }),
        T0,
    );
    p.apply_at(&call("EnterPlanMode", ENTER), T0);
    p.apply_at(&plan_report("CliModeReported"), T0 + 10);
    assert_eq!(p.planning.phase, PlanPhase::Planning);
    p.apply_at(&proposed("p1"), T0 + 20);
    p
}

/// `planning`, then the plan approved here at `T0 + 100`: the record is answering and the move out
/// started before anything could be sent.
fn leaving(requested: RequestedCliMode) -> AgentSessionProjection {
    let mut p = planning(requested);
    assert!(p.mark_plan_answering("p1"));
    p.apply_at(&transition(false, tool(EXIT), T0 + 100), T0 + 100);
    p
}

fn tripped(p: &AgentSessionProjection) -> Option<(&str, &str)> {
    p.ungated_cli_mode
        .as_ref()
        .map(|record| (record.reported.as_str(), record.detail.as_str()))
}

#[test]
fn a_model_entry_is_planning_only_after_the_cli_says_so() {
    let mut p = session(RequestedCliMode::Default);
    p.apply_at(&call("EnterPlanMode", ENTER), T0);
    assert_eq!(
        p.planning.phase,
        PlanPhase::Entering {
            cause: tool(ENTER),
            since_ms: T0
        }
    );
    assert!(p.is_planning());
    assert!(!p.plan_gate_applies(), "the CLI still runs default");
    p.apply_at(&plan_report("CliModeReported"), T0 + 5);
    assert_eq!(p.planning.phase, PlanPhase::Planning);
    assert!(p.planning.cli_reports_plan && p.plan_gate_applies());
    assert_eq!(tripped(&p), None);
    // Another entry call while planning starts nothing new.
    p.apply_at(&call("EnterPlanMode", "toolu_again"), T0 + 6);
    assert_eq!(p.planning.phase, PlanPhase::Planning);
}

#[test]
fn a_host_entry_waits_for_the_report() {
    let mut p = session(RequestedCliMode::Auto);
    p.apply_at(&mode("auto"), T0);
    p.apply_at(&transition(true, PlanCause::Host, T0 + 1), T0 + 50);
    assert_eq!(
        p.planning.phase,
        PlanPhase::Entering {
            cause: PlanCause::Host,
            since_ms: T0 + 1
        }
    );
    // The CLI's earlier mode, reported before it took the move in, changes nothing.
    p.apply_at(&mode("auto"), T0 + 2);
    assert!(matches!(p.planning.phase, PlanPhase::Entering { .. }));
    p.apply_at(&plan_report("SessionReady"), T0 + 3);
    assert_eq!(p.planning.phase, PlanPhase::Planning);
    // A leave asked for while not planning, or an entry while planning, starts nothing.
    p.apply_at(&transition(true, PlanCause::Host, T0 + 4), T0 + 4);
    assert_eq!(p.planning.phase, PlanPhase::Planning);
    let mut idle = session(RequestedCliMode::Default);
    idle.apply_at(&transition(false, PlanCause::Host, T0), T0);
    assert_eq!(idle.planning.phase, PlanPhase::Executing);
}

#[test]
fn an_approval_moves_to_leaving_before_the_cli_can_report() {
    let p = leaving(RequestedCliMode::Default);
    assert_eq!(p.plan("p1").unwrap().state, PlanState::Answering);
    assert_eq!(
        p.planning.phase,
        PlanPhase::Leaving {
            cause: tool(EXIT),
            since_ms: T0 + 100,
            saw_default: false
        }
    );
    assert!(p.plan_gate_applies(), "the CLI has not reported the move yet");
    // An approval another window sent moves the same way, from the settlement.
    let mut other = planning(RequestedCliMode::Default);
    other.apply_at(
        &settled("p1", PlanOutcome::Approved, Some(PlanExecutionMode::Auto)),
        T0 + 200,
    );
    assert_eq!(
        other.planning.phase,
        PlanPhase::Leaving {
            cause: tool(EXIT),
            since_ms: T0 + 200,
            saw_default: false
        }
    );
    // Any other settlement leaves the phase where it was.
    for outcome in [
        PlanOutcome::KeptPlanning,
        PlanOutcome::Interrupted,
        PlanOutcome::Expired,
        PlanOutcome::ProviderFailed,
        PlanOutcome::HostRefused,
    ] {
        let mut p = planning(RequestedCliMode::Default);
        p.apply_at(&settled("p1", outcome, Some(PlanExecutionMode::Bypass)), T0 + 200);
        assert_eq!(p.planning.phase, PlanPhase::Planning, "{outcome:?}");
    }
}

#[test]
fn the_creation_mode_report_ends_leaving() {
    for (requested, word) in [(RequestedCliMode::Default, "default"), (RequestedCliMode::Auto, "auto")] {
        let mut p = leaving(requested);
        p.apply_at(&mode(word), T0 + 200);
        assert_eq!(p.planning.phase, PlanPhase::Executing, "{word}");
        assert!(!p.planning.cli_reports_plan && !p.plan_gate_applies() && !p.is_planning());
        assert!(!p.cli_left_auto);
        assert_eq!(tripped(&p), None);
        assert_eq!(p.cli_mode_reported.as_deref(), Some(word));
    }
}

#[test]
fn a_plan_report_after_leaving_closes_the_session() {
    let mut p = leaving(RequestedCliMode::Default);
    p.apply_at(&mode("default"), T0 + 200);
    p.apply_at(&plan_report("SessionReady"), T0 + 300);
    assert_eq!(tripped(&p), Some(("plan", "plan mode the host did not expect")));
}

#[test]
fn a_plan_session_ready_while_planning_is_accepted() {
    let mut p = planning(RequestedCliMode::Auto);
    for at in 0..3 {
        p.apply_at(&plan_report("SessionReady"), T0 + 500 + at);
    }
    assert_eq!(p.planning.phase, PlanPhase::Planning);
    assert_eq!(tripped(&p), None);
}

#[test]
fn an_unsolicited_plan_report_closes_the_session() {
    let mut p = session(RequestedCliMode::Default);
    p.apply_at(&mode("default"), T0);
    p.apply_at(&plan_report("CliModeReported"), T0 + 1);
    assert_eq!(tripped(&p), Some(("plan", "plan mode the host did not expect")));
    assert_eq!(p.planning.phase, PlanPhase::Executing);
    // A session that never offered plan mode is never sent one; one that arrives closes it too.
    let mut never = AgentSessionProjection::default();
    never.apply_at(&plan_report("SessionReady"), T0);
    assert!(tripped(&never).is_some());
}

#[test]
fn leaving_plan_without_an_approval_closes_the_session() {
    for (requested, reports) in [
        (RequestedCliMode::Default, &["default"][..]),
        (RequestedCliMode::Auto, &["auto", "default"][..]),
    ] {
        for reported in reports {
            let mut p = planning(requested);
            p.apply_at(&mode(reported), T0 + 300);
            assert_eq!(
                tripped(&p),
                Some((*reported, "left plan mode without an approval")),
                "{reported} on {requested:?}"
            );
        }
    }
}

#[test]
fn an_intermediate_default_on_an_auto_session_waits_for_auto() {
    let mut p = leaving(RequestedCliMode::Auto);
    p.apply_at(&mode("default"), T0 + 200);
    assert_eq!(
        p.planning.phase,
        PlanPhase::Leaving {
            cause: tool(EXIT),
            since_ms: T0 + 100,
            saw_default: true
        }
    );
    assert!(!p.cli_left_auto, "a step the CLI may still correct");
    assert!(!p.plan_gate_applies(), "the CLI reported the move out of plan");
    assert!(!p.cli_runs_auto());
    assert_eq!(tripped(&p), None);
    p.apply_at(&mode("auto"), T0 + 300);
    assert_eq!(p.planning.phase, PlanPhase::Executing);
    assert!(!p.cli_left_auto && p.cli_runs_auto());
}

#[test]
fn a_default_that_stays_past_the_guard_is_the_auto_fallback() {
    let mut p = leaving(RequestedCliMode::Auto);
    p.apply_at(&mode("default"), T0 + 200);
    assert_eq!(p.plan_transition_overdue(T0 + 100 + PLAN_TRANSITION_GUARD_MS), None);
    assert!(p.is_planning(), "not past the guard yet");
    assert_eq!(p.plan_transition_overdue(T0 + 101 + PLAN_TRANSITION_GUARD_MS), None);
    assert_eq!(p.planning.phase, PlanPhase::Executing);
    assert!(p.cli_left_auto, "the tab follows out of auto, as for any fallback");
    assert_eq!(tripped(&p), None);
}

#[test]
fn a_transition_past_15_s_is_overdue() {
    assert_eq!(PLAN_TRANSITION_GUARD_MS, 15_000);
    let mut entering = session(RequestedCliMode::Default);
    entering.apply_at(&transition(true, PlanCause::Host, T0), T0);
    // The host's request was answered at once.
    assert!(!entering.planning_reply_read(true, None, T0));
    assert_eq!(entering.plan_transition_overdue(T0 + 15_000), None);
    assert!(entering
        .plan_transition_overdue(T0 + 15_001)
        .unwrap()
        .contains("did not enter plan mode within 15 s"));
    let mut leaving = leaving(RequestedCliMode::Default);
    assert!(leaving
        .plan_transition_overdue(T0 + 100 + 15_001)
        .unwrap()
        .contains("did not leave plan mode"));
    // An auto session that never reported anything on the way out has no fallback to end in.
    let mut silent = self::leaving(RequestedCliMode::Auto);
    assert!(silent.plan_transition_overdue(T0 + 100 + 15_001).is_some());
    for settled_phase in [PlanPhase::Executing, PlanPhase::Planning] {
        let mut p = session(RequestedCliMode::Default);
        p.planning.phase = settled_phase;
        assert_eq!(p.plan_transition_overdue(u64::MAX), None);
    }
}

#[test]
fn a_failed_enter_tool_returns_to_executing() {
    let mut p = session(RequestedCliMode::Default);
    p.apply_at(&call("EnterPlanMode", ENTER), T0);
    p.apply_at(&completed("toolu_other", true), T0 + 1);
    p.apply_at(&completed(ENTER, false), T0 + 2);
    assert!(matches!(p.planning.phase, PlanPhase::Entering { .. }));
    p.apply_at(&completed(ENTER, true), T0 + 3);
    assert_eq!(p.planning.phase, PlanPhase::Executing);
    // A host entry is not ended by any tool's failure.
    let mut host = session(RequestedCliMode::Default);
    host.apply_at(&transition(true, PlanCause::Host, T0), T0);
    host.apply_at(&completed(ENTER, true), T0 + 1);
    assert!(matches!(host.planning.phase, PlanPhase::Entering { .. }));
}

/// The phase rules never run on a session that did not offer plan mode: the model's entry call moves
/// nothing, a move this side folds is ignored, and a fallback out of auto is followed as before.
#[test]
fn a_session_without_plan_mode_keeps_its_phase_and_today_s_folds() {
    let mut p = AgentSessionProjection::default();
    p.apply_at(&mode("auto"), T0);
    p.apply_at(&call("EnterPlanMode", ENTER), T0);
    p.apply_at(&transition(true, PlanCause::Host, T0), T0);
    assert_eq!(p.planning, PlanningState::default());
    p.apply_at(&mode("default"), T0);
    assert!(p.cli_left_auto);
    p.apply_at(&mode("auto"), T0);
    assert!(!p.cli_left_auto);
    assert!(!p.is_planning() && !p.plan_gate_applies());
}

#[test]
fn close_settles_a_waiting_plan() {
    for ending in [
        AgentDomainEvent::SessionClosed {
            reason: "closed".into(),
        },
        AgentDomainEvent::SessionUnavailable { reason: "lost".into() },
    ] {
        let mut p = planning(RequestedCliMode::Default);
        p.apply_at(&settled("p1", PlanOutcome::KeptPlanning, None), T0 + 30);
        p.apply_at(&proposed("p2"), T0 + 40);
        assert!(p.mark_plan_answering("p2"));
        p.apply_at(&ending, T0 + 50);
        assert!(matches!(
            p.plan("p1").unwrap().state,
            PlanState::Settled {
                outcome: PlanOutcome::KeptPlanning,
                ..
            }
        ));
        assert_eq!(
            p.plan("p2").unwrap().state,
            PlanState::Settled {
                outcome: PlanOutcome::SessionClosed,
                mode_after: None,
                feedback: None,
                reason: None
            }
        );
        assert_eq!(p.waiting_plan(), None);
        assert_eq!(p.planning.phase, PlanPhase::Executing);
        // A settlement after the close changes nothing.
        p.apply_at(
            &settled("p2", PlanOutcome::Approved, Some(PlanExecutionMode::Auto)),
            T0 + 60,
        );
        assert_eq!(p.latest_approval(), None);
    }
}

#[test]
fn the_gate_applies_only_once_the_cli_reports_plan() {
    let mut p = session(RequestedCliMode::Default);
    p.apply_at(&call("EnterPlanMode", ENTER), T0);
    assert!(!p.plan_gate_applies(), "Entering");
    p.apply_at(&plan_report("CliModeReported"), T0 + 1);
    assert!(p.plan_gate_applies(), "Planning");
    p.apply_at(&proposed("p1"), T0 + 2);
    p.apply_at(&transition(false, tool(EXIT), T0 + 3), T0 + 3);
    assert!(p.plan_gate_applies(), "Leaving, before the report");
    p.apply_at(&mode("default"), T0 + 4);
    assert!(!p.plan_gate_applies(), "after the report");
    // Not offered: never.
    let mut never = AgentSessionProjection::default();
    never.planning.phase = PlanPhase::Planning;
    assert!(!never.plan_gate_applies());
}

#[test]
fn a_plan_report_while_leaving_waits_for_the_guard() {
    let mut p = leaving(RequestedCliMode::Default);
    p.apply_at(&plan_report("SessionReady"), T0 + 150);
    assert_eq!(tripped(&p), None);
    assert!(matches!(p.planning.phase, PlanPhase::Leaving { .. }));
    assert!(p.plan_gate_applies());
    assert!(p.plan_transition_overdue(T0 + 100 + 15_001).is_some());
}

#[test]
fn approve_auto_with_an_intermediate_default_lands_in_auto() {
    let mut p = planning(RequestedCliMode::Auto);
    assert!(p.cli_runs_auto());
    p.apply_at(
        &settled("p1", PlanOutcome::Approved, Some(PlanExecutionMode::Auto)),
        T0 + 100,
    );
    p.apply_at(&mode("default"), T0 + 110);
    p.apply_at(&mode("auto"), T0 + 120);
    assert_eq!(p.planning.phase, PlanPhase::Executing);
    assert!(!p.cli_left_auto);
    assert!(p.cli_runs_auto());
    let seq = p.plan("p1").unwrap().seq;
    assert_eq!(p.latest_approval(), Some((seq, Some(PlanExecutionMode::Auto))));
}

#[test]
fn a_default_outside_leaving_on_a_plan_session_still_follows_out_of_auto() {
    let mut p = session(RequestedCliMode::Auto);
    p.apply_at(&mode("auto"), T0);
    p.apply_at(&mode("default"), T0 + 1);
    assert!(p.cli_left_auto);
    assert!(!p.is_planning());
    p.apply_at(&mode("auto"), T0 + 2);
    assert!(!p.cli_left_auto);
    // A fallback reported first, with no `auto` before it, is followed too.
    let mut first = session(RequestedCliMode::Auto);
    first.apply_at(&mode("default"), T0);
    assert!(first.cli_left_auto);
    // A session created in `default` has no auto to leave.
    let mut manual = session(RequestedCliMode::Default);
    manual.apply_at(&mode("default"), T0);
    assert!(!manual.cli_left_auto);
}

#[test]
fn a_plan_report_leaves_cli_runs_auto_as_it_was() {
    let p = planning(RequestedCliMode::Auto);
    assert!(p.plan_gate_applies());
    assert_eq!(p.cli_mode_reported.as_deref(), Some("auto"));
    assert!(p.cli_runs_auto(), "the session runs auto when it executes");
    let p = planning(RequestedCliMode::Default);
    assert_eq!(p.cli_mode_reported.as_deref(), Some("default"));
    assert!(!p.cli_runs_auto());
}

#[test]
fn the_latest_approval_carries_mode_after_from_any_window() {
    let mut p = planning(RequestedCliMode::Default);
    assert_eq!(p.latest_approval(), None);
    p.apply_at(
        &settled("p1", PlanOutcome::Approved, Some(PlanExecutionMode::Manual)),
        T0 + 100,
    );
    let first = p.plan("p1").unwrap().seq;
    assert_eq!(p.latest_approval(), Some((first, Some(PlanExecutionMode::Manual))));
    p.apply_at(&mode("default"), T0 + 110);
    p.apply_at(&call("EnterPlanMode", "toolu_enter_2"), T0 + 120);
    p.apply_at(&plan_report("CliModeReported"), T0 + 121);
    p.apply_at(&proposed("p2"), T0 + 130);
    p.apply_at(
        &settled("p2", PlanOutcome::KeptPlanning, Some(PlanExecutionMode::Bypass)),
        T0 + 140,
    );
    assert_eq!(p.latest_approval(), Some((first, Some(PlanExecutionMode::Manual))));
    assert!(matches!(
        &p.plan("p2").unwrap().state,
        PlanState::Settled { mode_after: None, feedback: Some(f), .. } if f == "shorter, please"
    ));
    p.apply_at(&proposed("p3"), T0 + 150);
    p.apply_at(
        &settled("p3", PlanOutcome::Approved, Some(PlanExecutionMode::Bypass)),
        T0 + 160,
    );
    let third = p.plan("p3").unwrap().seq;
    assert!(third > first);
    assert_eq!(p.latest_approval(), Some((third, Some(PlanExecutionMode::Bypass))));
    // A second settlement of a settled plan changes nothing.
    p.apply_at(
        &settled("p3", PlanOutcome::Approved, Some(PlanExecutionMode::Manual)),
        T0 + 170,
    );
    assert_eq!(p.latest_approval(), Some((third, Some(PlanExecutionMode::Bypass))));
}

#[test]
fn a_plan_takes_no_question_answer_and_only_while_waiting() {
    let mut p = planning(RequestedCliMode::Default);
    let answer = InteractionResolution::Answers(Vec::new());
    let decline = InteractionResolution::Decline {
        kind: QuestionDeclineKind::Cancel,
        so_far: Vec::new(),
    };
    for resolution in [&answer, &decline] {
        assert!(p
            .plan("p1")
            .unwrap()
            .check_resolution(resolution)
            .unwrap_err()
            .contains("never answered as a question"));
    }
    assert!(p.mark_plan_answering("p1"));
    assert!(!p.mark_plan_answering("p1"));
    assert!(p
        .plan("p1")
        .unwrap()
        .check_resolution(&answer)
        .unwrap_err()
        .contains("already on its way"));
    assert_eq!(p.waiting_plan().map(|r| r.interaction_id.as_str()), Some("p1"));
    assert!(p.plan_send_failed("p1"));
    assert!(!p.plan_send_failed("p1"));
    assert!(!p.mark_plan_answering("missing"));
    p.apply_at(&settled("p1", PlanOutcome::Expired, None), T0 + 50);
    assert!(p
        .plan("p1")
        .unwrap()
        .check_resolution(&answer)
        .unwrap_err()
        .contains("no longer waiting"));
    p.apply_at(
        &AgentDomainEvent::PlanProposed {
            interaction_id: "refused".into(),
            tool_use_id: None,
            plan: String::new(),
            plan_sha256: String::new(),
            allowed_prompts: Vec::new(),
            unsupported_reason: Some("too long".into()),
        },
        T0 + 60,
    );
    assert!(p
        .plan("refused")
        .unwrap()
        .check_resolution(&answer)
        .unwrap_err()
        .contains("could not be shown"));
}

/// `/clear` keeps what the process holds: a plan still waiting and the phase; settled plans go with the
/// conversation.
#[test]
fn a_reset_keeps_the_waiting_plan_and_the_phase() {
    let mut p = planning(RequestedCliMode::Default);
    p.apply_at(&settled("p1", PlanOutcome::KeptPlanning, None), T0 + 30);
    p.apply_at(&proposed("p2"), T0 + 40);
    p.apply_at(
        &AgentDomainEvent::ConversationReset {
            new_conversation_id: "new".into(),
            trigger: None,
        },
        T0 + 50,
    );
    assert_eq!(
        p.plans.iter().map(|r| r.interaction_id.as_str()).collect::<Vec<_>>(),
        ["p2"]
    );
    assert_eq!(p.planning.phase, PlanPhase::Planning);
}

/// A plan proposed before the frames of the turn it belongs to takes that turn and its place when its
/// call arrives, as a question does.
#[test]
fn a_plan_proposed_before_its_call_moves_to_the_call_s_turn() {
    let mut p = session(RequestedCliMode::Default);
    p.apply_at(&proposed("p1"), T0);
    assert_eq!(p.plan("p1").unwrap().turn_id, None);
    p.apply_at(&AgentDomainEvent::TurnStarted { turn_id: "t".into() }, T0);
    p.apply_at(&call("ExitPlanMode", EXIT), T0);
    let record = p.plan("p1").unwrap();
    assert_eq!(record.turn_id.as_deref(), Some("t"));
    assert_eq!(record.seq, p.tool_calls[0].seq);
}

#[test]
fn a_move_carries_plans_and_the_plan_phase() {
    let mut p = planning(RequestedCliMode::Auto);
    p.apply_at(&settled("p1", PlanOutcome::KeptPlanning, None), T0 + 30);
    let core = crate::move_seed::ChatMoveCore {
        lifecycle: Vec::new(),
        seed: p.export_settled(),
        running_turn: Vec::new(),
        folded_through: 0,
        call_marks: Default::default(),
    };
    let adopted = AgentSessionProjection::adopt(&core).unwrap();
    assert_eq!(adopted.plans, p.plans);
    assert_eq!(adopted.planning, p.planning);
    assert!(adopted.plan_gate_applies());
    assert_eq!(adopted.last_revision, p.last_revision);
}

/// A move's records cross as JSON, whose seed leaves the call marks out; the folds rebuild only those of the calls
/// the running turn starts. The marks cross beside the records, and each is placed where the folds left the call
/// unmarked; a mark the folds rebuilt stays as they made it.
#[test]
fn a_move_keeps_the_call_marks_its_folds_cannot_rebuild() {
    let mut p = planning(RequestedCliMode::Auto);
    p.apply_at(&call("Bash", "toolu_earlier"), T0 + 30);
    assert_eq!(p.call_mark("toolu_earlier"), Some(CallPlanMark::Planning));
    let core = |call_marks: BTreeMap<String, CallPlanMark>| {
        let core = crate::move_seed::ChatMoveCore {
            lifecycle: Vec::new(),
            seed: p.export_settled(),
            running_turn: vec![call("Bash", "toolu_running")],
            folded_through: 0,
            call_marks,
        };
        serde_json::from_value::<crate::move_seed::ChatMoveCore>(serde_json::to_value(core).unwrap()).unwrap()
    };
    let unmarked = AgentSessionProjection::adopt(&core(BTreeMap::new())).unwrap();
    assert!(unmarked.call_seen("toolu_earlier"), "the call crosses in the seed");
    assert_eq!(unmarked.call_mark("toolu_earlier"), None, "and its mark does not");
    assert_eq!(unmarked.call_mark("toolu_running"), Some(CallPlanMark::Planning));
    let carried = BTreeMap::from([
        ("toolu_earlier".to_string(), CallPlanMark::Planning),
        ("toolu_running".to_string(), CallPlanMark::Moving),
    ]);
    let adopted = AgentSessionProjection::adopt(&core(carried)).unwrap();
    assert_eq!(adopted.call_mark("toolu_earlier"), Some(CallPlanMark::Planning));
    assert_eq!(adopted.call_mark("toolu_running"), Some(CallPlanMark::Planning));
}

#[test]
fn the_snapshot_names_the_phase_only_on_a_plan_session() {
    let mut snapshot = json!({});
    snapshot_json(&AgentSessionProjection::default(), &mut snapshot);
    assert_eq!(snapshot, json!({}));
    for (phase, word) in [
        (PlanPhase::Executing, "executing"),
        (
            PlanPhase::Entering {
                cause: PlanCause::Host,
                since_ms: 0,
            },
            "entering",
        ),
        (PlanPhase::Planning, "planning"),
        (
            PlanPhase::Leaving {
                cause: PlanCause::Host,
                since_ms: 0,
                saw_default: true,
            },
            "leaving",
        ),
    ] {
        let mut p = session(RequestedCliMode::Default);
        p.planning.phase = phase;
        let mut snapshot = json!({});
        snapshot_json(&p, &mut snapshot);
        assert_eq!(snapshot, json!({ "planning": { "phase": word } }));
    }
}

fn undone(enter: bool, cause: PlanCause) -> AgentDomainEvent {
    AgentDomainEvent::PlanTransitionUndone { enter, cause }
}

fn asked_for(permission_id: &str, tool_use_id: &str) -> AgentDomainEvent {
    AgentDomainEvent::PermissionRequested {
        permission_id: permission_id.into(),
        tool_use_id: Some(tool_use_id.into()),
        tool_name: "EnterPlanMode".into(),
        input: json!({}),
        provider_prompt: None,
    }
}

fn answered(permission_id: &str) -> AgentDomainEvent {
    AgentDomainEvent::PermissionResolved {
        permission_id: permission_id.into(),
        outcome: crate::PermissionOutcome::Allowed,
    }
}

fn turn_ended(outcome: crate::TurnOutcome) -> AgentDomainEvent {
    AgentDomainEvent::TurnCompleted {
        turn_id: "t".into(),
        outcome,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
        detail: Default::default(),
    }
}

/// A move the agent refused goes back to where the session stood: an entry to executing, an approval to
/// planning. Only the move that was refused, and only while the CLI has not made it.
#[test]
fn a_refused_move_goes_back_to_where_it_started() {
    let mut entering = session(RequestedCliMode::Default);
    entering.apply_at(&transition(true, PlanCause::Host, T0), T0);
    entering.apply_at(&undone(true, PlanCause::Host), T0 + 1);
    assert_eq!(entering.planning.phase, PlanPhase::Executing);
    assert_eq!(entering.plan_transition_overdue(u64::MAX), None);

    let mut p = leaving(RequestedCliMode::Default);
    assert!(p.plan_send_failed("p1"));
    p.apply_at(&undone(false, tool(EXIT)), T0 + 101);
    assert_eq!(p.planning.phase, PlanPhase::Planning);
    assert!(p.plan_gate_applies());
    assert_eq!(p.plan_transition_overdue(u64::MAX), None, "nothing waits for the guard");
    assert_eq!(p.waiting_plan().unwrap().state, PlanState::Waiting);
    // Still planning: the CLI's next report of plan is accepted.
    p.apply_at(&plan_report("SessionReady"), T0 + 102);
    assert_eq!(tripped(&p), None);

    // Another move's refusal changes nothing; nor does a refusal once the CLI reported the move.
    let mut other = leaving(RequestedCliMode::Default);
    other.apply_at(&undone(false, PlanCause::Host), T0 + 101);
    other.apply_at(&undone(true, tool(EXIT)), T0 + 101);
    assert!(matches!(other.planning.phase, PlanPhase::Leaving { .. }));
    let mut moved = leaving(RequestedCliMode::Default);
    moved.apply_at(&mode("default"), T0 + 150);
    assert_eq!(moved.planning.phase, PlanPhase::Executing);
    moved.apply_at(&undone(false, tool(EXIT)), T0 + 151);
    assert_eq!(moved.planning.phase, PlanPhase::Executing);
    let mut entered = session(RequestedCliMode::Default);
    entered.apply_at(&transition(true, PlanCause::Host, T0), T0);
    entered.apply_at(&plan_report("CliModeReported"), T0 + 1);
    entered.apply_at(&undone(true, PlanCause::Host), T0 + 2);
    assert_eq!(entered.planning.phase, PlanPhase::Planning);
}

/// The guard does not count the time a person spends on the entry call's card: it starts when the card
/// is answered.
#[test]
fn the_entry_card_s_time_is_not_counted() {
    let mut p = session(RequestedCliMode::Default);
    p.apply_at(&call("EnterPlanMode", ENTER), T0);
    p.apply_at(&asked_for("perm-1", ENTER), T0 + 1);
    assert_eq!(
        p.plan_transition_overdue(T0 + 60_000),
        None,
        "the person is reading the card"
    );
    p.apply_at(&answered("perm-1"), T0 + 60_000);
    assert_eq!(
        p.planning.phase,
        PlanPhase::Entering {
            cause: tool(ENTER),
            since_ms: T0 + 60_000
        }
    );
    assert_eq!(p.plan_transition_overdue(T0 + 60_000 + PLAN_TRANSITION_GUARD_MS), None);
    assert!(p
        .plan_transition_overdue(T0 + 60_001 + PLAN_TRANSITION_GUARD_MS)
        .is_some());
    // Another call's card holds nothing back.
    let mut q = session(RequestedCliMode::Default);
    q.apply_at(&call("EnterPlanMode", ENTER), T0);
    q.apply_at(&asked_for("perm-2", "toolu_other"), T0 + 1);
    assert!(q.plan_transition_overdue(T0 + 15_001).is_some());
}

/// An entry that ends without the CLI entering reads the report of the mode it set aside meanwhile.
#[test]
fn a_failed_entry_reads_the_default_reported_meanwhile() {
    let mut p = session(RequestedCliMode::Auto);
    p.apply_at(&mode("auto"), T0);
    p.apply_at(&call("EnterPlanMode", ENTER), T0 + 1);
    p.apply_at(&mode("default"), T0 + 2);
    assert!(!p.cli_left_auto, "set aside while entering");
    p.apply_at(&completed(ENTER, true), T0 + 3);
    assert_eq!(p.planning.phase, PlanPhase::Executing);
    assert!(p.cli_left_auto, "the CLI's last report is default");
}

/// A turn that ends before the entry call had its result never entered plan mode.
#[test]
fn an_interrupt_before_the_entry_result_returns_to_executing() {
    let mut p = session(RequestedCliMode::Default);
    p.apply_at(&call("EnterPlanMode", ENTER), T0);
    p.apply_at(&turn_ended(crate::TurnOutcome::Interrupted), T0 + 1);
    assert_eq!(p.planning.phase, PlanPhase::Executing);
    assert_eq!(p.plan_transition_overdue(u64::MAX), None);
    // With its result in, the turn's end leaves the move to the CLI's report.
    let mut q = session(RequestedCliMode::Default);
    q.apply_at(&call("EnterPlanMode", ENTER), T0);
    q.apply_at(&completed(ENTER, false), T0 + 1);
    q.apply_at(&turn_ended(crate::TurnOutcome::Completed), T0 + 2);
    assert!(matches!(q.planning.phase, PlanPhase::Entering { .. }));
}

/// A `plan` report after a `default` on the way out is plan again: the default was a step, not the
/// fallback, and the gate still applies.
#[test]
fn a_plan_report_after_a_default_on_the_way_out_is_plan_again() {
    let mut p = leaving(RequestedCliMode::Auto);
    p.apply_at(&mode("default"), T0 + 200);
    assert!(!p.plan_gate_applies());
    p.apply_at(&plan_report("CliModeReported"), T0 + 300);
    assert!(p.planning.cli_reports_plan && p.plan_gate_applies());
    assert!(
        matches!(p.planning.phase, PlanPhase::Leaving { saw_default: false, .. }),
        "{:?}",
        p.planning.phase
    );
    assert!(p.plan_transition_overdue(T0 + 101 + PLAN_TRANSITION_GUARD_MS).is_some());
    assert!(!p.cli_left_auto);
}

/// The plan an approval was sent for ended some other way first (an interrupt, an expiry, "keep planning"
/// from another window): the CLI never left plan mode, so the move out is over, whichever of the
/// settlement and the approval's refusal arrives first. An approval's own settlement still leaves.
#[test]
fn a_plan_settled_another_way_ends_leaving() {
    for outcome in [
        PlanOutcome::Interrupted,
        PlanOutcome::Expired,
        PlanOutcome::ProviderFailed,
        PlanOutcome::KeptPlanning,
    ] {
        // The approval's reply (the plan is no longer there) first, then the settlement.
        let mut reply_first = leaving(RequestedCliMode::Default);
        assert!(reply_first.plan_send_failed("p1"));
        assert!(matches!(reply_first.planning.phase, PlanPhase::Leaving { .. }));
        reply_first.apply_at(&settled("p1", outcome, None), T0 + 200);
        assert_eq!(reply_first.planning.phase, PlanPhase::Planning, "{outcome:?}");
        assert!(reply_first.plan_gate_applies());
        assert_eq!(reply_first.plan_transition_overdue(u64::MAX), None, "{outcome:?}");
        reply_first.apply_at(&plan_report("SessionReady"), T0 + 300);
        assert_eq!(tripped(&reply_first), None);

        // The settlement first, then the reply, which finds the plan settled and changes nothing.
        let mut settled_first = leaving(RequestedCliMode::Default);
        settled_first.apply_at(&settled("p1", outcome, None), T0 + 200);
        assert_eq!(settled_first.planning.phase, PlanPhase::Planning, "{outcome:?}");
        assert!(!settled_first.plan_send_failed("p1"));
        assert_eq!(settled_first.planning.phase, PlanPhase::Planning);
    }
    // The CLI already reported its way out: its report decides, not the settlement.
    let mut out = leaving(RequestedCliMode::Auto);
    out.apply_at(&mode("default"), T0 + 150);
    out.apply_at(&settled("p1", PlanOutcome::Interrupted, None), T0 + 200);
    assert!(matches!(out.planning.phase, PlanPhase::Leaving { .. }));
    // Another plan's settlement leaves this move alone.
    let mut other = leaving(RequestedCliMode::Default);
    let mut second = proposed("p2");
    if let AgentDomainEvent::PlanProposed { tool_use_id, .. } = &mut second {
        *tool_use_id = Some("toolu_other_exit".into());
    }
    other.apply_at(&second, T0 + 150);
    other.apply_at(&settled("p2", PlanOutcome::Interrupted, None), T0 + 200);
    assert!(matches!(other.planning.phase, PlanPhase::Leaving { .. }));
}

/// The host's own move is not overdue while the reply to its request is unread: the sidecar's deadline is
/// as long as the guard, and a CLI that moved late still reports it. The guard counts from the reply.
#[test]
fn a_host_move_waits_for_its_reply_before_the_guard_counts() {
    let deadline = crate::ProviderError::Provider {
        code: crate::ProviderErrorCode::DeadlineExceeded,
        message: String::new(),
    };
    let mut p = session(RequestedCliMode::Default);
    p.apply_at(&transition(true, PlanCause::Host, T0), T0);
    assert_eq!(p.plan_transition_overdue(T0 + 60_000), None, "the reply is unread");
    assert!(!p.planning_reply_read(true, Some(&deadline), T0 + 15_000));
    assert!(matches!(p.planning.phase, PlanPhase::Entering { since_ms, .. } if since_ms == T0 + 15_000));
    assert_eq!(p.plan_transition_overdue(T0 + 15_000 + PLAN_TRANSITION_GUARD_MS), None);
    // The CLI moved late: its report confirms the move.
    p.apply_at(&plan_report("CliModeReported"), T0 + 20_000);
    assert_eq!(p.planning.phase, PlanPhase::Planning);
    // Without a report, the guard counts from the reply.
    let mut q = session(RequestedCliMode::Default);
    q.apply_at(&transition(true, PlanCause::Host, T0), T0);
    q.planning_reply_read(true, Some(&deadline), T0 + 15_000);
    assert!(q
        .plan_transition_overdue(T0 + 15_001 + PLAN_TRANSITION_GUARD_MS)
        .is_some());
    // The same holds on the way out.
    let mut r = session(RequestedCliMode::Default);
    r.planning.phase = PlanPhase::Planning;
    r.planning.cli_reports_plan = true;
    r.apply_at(&transition(false, PlanCause::Host, T0), T0);
    assert_eq!(r.plan_transition_overdue(T0 + 60_000), None);
    r.planning_reply_read(false, None, T0 + 60_000);
    assert!(r
        .plan_transition_overdue(T0 + 60_001 + PLAN_TRANSITION_GUARD_MS)
        .is_some());
}

/// A request for the host's move that timed out may still be carried out: a resend's refusal then undoes
/// nothing, so the CLI's late report of the move is not taken for one nobody expected. A fresh move's
/// refusal still undoes it.
#[test]
fn a_refusal_after_a_lost_request_leaves_the_move_open() {
    let busy = crate::ProviderError::Provider {
        code: crate::ProviderErrorCode::TurnAlreadyActive,
        message: "planning_turn_running".into(),
    };
    let mut fresh = session(RequestedCliMode::Default);
    fresh.apply_at(&transition(true, PlanCause::Host, T0), T0);
    assert!(
        fresh.planning_reply_read(true, Some(&busy), T0 + 1),
        "a fresh refusal undoes the move"
    );

    let mut p = session(RequestedCliMode::Default);
    p.apply_at(&transition(true, PlanCause::Host, T0), T0);
    assert!(!p.planning_reply_read(true, Some(&crate::ProviderError::Timeout), T0 + 15_000));
    p.planning_resent();
    assert_eq!(
        p.plan_transition_overdue(u64::MAX),
        None,
        "the resend's reply is unread"
    );
    assert!(!p.planning_reply_read(true, Some(&busy), T0 + 15_100));
    assert!(matches!(p.planning.phase, PlanPhase::Entering { .. }));
    p.apply_at(&plan_report("CliModeReported"), T0 + 16_000);
    assert_eq!(p.planning.phase, PlanPhase::Planning);
    assert_eq!(tripped(&p), None);
    // A new move starts with nothing uncertain.
    p.apply_at(&transition(false, PlanCause::Host, T0 + 20_000), T0 + 20_000);
    assert!(p.planning_reply_read(false, Some(&busy), T0 + 20_001));
}

/// Each call carries where the session stood when it began: nothing while executing, a move while entering (a call
/// beside `EnterPlanMode`), plan mode once the CLI reported it, and the same for a subagent's call. A session without
/// plan mode marks nothing.
#[test]
fn a_call_is_marked_with_the_phase_it_began_in() {
    let mut p = session(RequestedCliMode::Default);
    p.apply_at(&call("Read", "toolu_before"), T0);
    p.apply_at(&call("EnterPlanMode", ENTER), T0);
    p.apply_at(&call("Write", "toolu_beside"), T0);
    p.apply_at(&plan_report("CliModeReported"), T0);
    p.apply_at(&call("Write", "toolu_planned"), T0);
    p.apply_at(&call("Agent", "toolu_agent"), T0);
    p.apply_at(
        &AgentDomainEvent::SubagentFrame {
            parent_tool_use_id: "toolu_agent".into(),
            frame: Box::new(call("Bash", "toolu_child")),
        },
        T0,
    );
    assert_eq!(p.call_mark("toolu_before"), None);
    assert_eq!(
        p.call_mark(ENTER),
        None,
        "the entry call begins while the session executes"
    );
    assert_eq!(p.call_mark("toolu_beside"), Some(CallPlanMark::Moving));
    assert_eq!(p.call_mark("toolu_planned"), Some(CallPlanMark::Planning));
    assert_eq!(p.call_mark("toolu_child"), Some(CallPlanMark::Planning));
    assert!(p.call_seen("toolu_child"), "a subagent's call is seen");
    assert!(!p.call_seen("toolu_never"));
    assert!(p.began_outside_executing(Some("toolu_beside")));
    assert!(!p.began_outside_executing(Some("toolu_before")));
    assert!(!p.began_outside_executing(None));

    let mut plain = AgentSessionProjection::default();
    plain.apply(&call("Write", "toolu_w"));
    assert!(plain.planning.call_marks.is_empty());
}
