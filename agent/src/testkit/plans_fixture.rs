//! Writes (or, without `EITRI_WRITE_FIXTURES=1`, compares) `agent-ui/web/src/fixtures/plans.json`: what the
//! projection's plan records and plan phase look like to a page after each step of a case, so the page's
//! own fold is held to the same answers.

use super::*;
use crate::{AgentSessionProjection, CliExpectation, RequestedCliMode};
use serde_json::json;

const T0: u64 = 1_800_000_000_000;

fn proposed(id: &str) -> AgentDomainEvent {
    let plan = "# Plan\n\n1. Read `src/a.rs`\n2. Change it\u{200b}\n\n<script>alert(1)</script>".to_string();
    AgentDomainEvent::PlanProposed {
        interaction_id: id.into(),
        tool_use_id: Some(format!("tool-{id}")),
        plan_sha256: "0".repeat(64),
        plan,
        allowed_prompts: vec![
            PlanAllowedPrompt {
                tool: "Bash".into(),
                prompt: "run the tests".into(),
            },
            PlanAllowedPrompt {
                tool: "Bash".into(),
                prompt: "remote\u{202e}\u{200b}".into(),
            },
        ],
        unsupported_reason: None,
    }
}

fn settled(id: &str, outcome: PlanOutcome) -> AgentDomainEvent {
    AgentDomainEvent::PlanSettled {
        interaction_id: id.into(),
        outcome,
        mode_after: (outcome == PlanOutcome::Approved).then_some(PlanExecutionMode::Auto),
        feedback: (outcome == PlanOutcome::KeptPlanning).then(|| "Keep \"this\" shorter\u{200b}".to_string()),
        reason: Some(format!("ended: {outcome:?}")),
    }
}

fn event(event: AgentDomainEvent) -> Value {
    json!({ "event": event })
}

fn send(id: &str, action: &str, changed: bool) -> Value {
    json!({ "interactionId": id, "send": action, "changed": changed })
}

fn cli(reported: &str) -> Value {
    event(AgentDomainEvent::CliPermissionMode {
        reported: reported.into(),
    })
}

fn plan_report() -> Value {
    event(AgentDomainEvent::CliPlanMode {
        source: "CliModeReported".into(),
    })
}

fn enter_call() -> Value {
    event(AgentDomainEvent::ToolCallStarted {
        turn_id: "turn".into(),
        tool_use_id: "tool-enter".into(),
        name: "EnterPlanMode".into(),
        input: json!({}),
    })
}

fn case(name: &str, requested: RequestedCliMode, actions: Vec<Value>) -> Value {
    let mut projection = AgentSessionProjection::default();
    projection.offer_plan_mode(CliExpectation {
        requested,
        plan_offered: true,
    });
    let mut steps = vec![];
    for (at, action) in actions.into_iter().enumerate() {
        if let Some(event) = action.get("event") {
            let event = serde_json::from_value::<AgentDomainEvent>(event.clone()).unwrap();
            projection.apply_at(&event, T0 + at as u64);
        } else {
            let id = action["interactionId"].as_str().unwrap();
            let changed = match action["send"].as_str().unwrap() {
                "answering" => projection.mark_plan_answering(id),
                "failed" => projection.plan_send_failed(id),
                other => panic!("unknown send action: {other}"),
            };
            assert_eq!(changed, action["changed"].as_bool().unwrap());
        }
        let mut snapshot = json!({ "activeTurnId": projection.active_turn_id });
        snapshot_json(&projection, &mut snapshot);
        steps.push(json!({
            "action": action, "snapshot": snapshot, "throughRevision": projection.last_revision,
            "waitingId": projection.waiting_plan().map(|record| &record.interaction_id),
            "isPlanning": projection.is_planning(), "planGateApplies": projection.plan_gate_applies(),
        }));
    }
    json!({ "name": name, "requested": requested, "steps": steps })
}

#[test]
fn plans_fixture() {
    let outcomes = [
        PlanOutcome::Approved,
        PlanOutcome::KeptPlanning,
        PlanOutcome::Interrupted,
        PlanOutcome::Expired,
        PlanOutcome::SessionClosed,
        PlanOutcome::ProviderFailed,
        PlanOutcome::HostRefused,
    ];
    let mut cases = vec![];
    for outcome in outcomes {
        cases.push(case(
            &format!("outcome-{outcome:?}"),
            RequestedCliMode::Default,
            vec![
                event(AgentDomainEvent::TurnStarted { turn_id: "turn".into() }),
                enter_call(),
                plan_report(),
                event(proposed("p")),
                send("p", "answering", true),
                event(settled("p", outcome)),
                event(settled("p", PlanOutcome::Approved)),
                send("p", "failed", false),
                send("p", "answering", false),
            ],
        ));
    }
    // Both directions of a move, the model's and the host's, on each creation mode; an auto session's
    // CLI steps through `default` on its way back.
    cases.push(case(
        "model-entry-approval-and-return-to-default",
        RequestedCliMode::Default,
        vec![
            cli("default"),
            enter_call(),
            plan_report(),
            event(proposed("p")),
            send("p", "answering", true),
            event(AgentDomainEvent::PlanTransitionStarted {
                enter: false,
                cause: PlanCause::Tool {
                    tool_use_id: "tool-p".into(),
                },
                at_ms: T0,
            }),
            event(settled("p", PlanOutcome::Approved)),
            cli("default"),
        ],
    ));
    cases.push(case(
        "host-entry-and-leave-on-auto-through-default",
        // From the test module: this file is read by the wire guard as production source.
        super::tests::auto(),
        vec![
            cli("auto"),
            event(AgentDomainEvent::PlanTransitionStarted {
                enter: true,
                cause: PlanCause::Host,
                at_ms: T0,
            }),
            plan_report(),
            event(AgentDomainEvent::PlanTransitionStarted {
                enter: false,
                cause: PlanCause::Host,
                at_ms: T0,
            }),
            cli("default"),
            cli("auto"),
        ],
    ));
    let refused = AgentDomainEvent::PlanProposed {
        interaction_id: "refused".into(),
        tool_use_id: Some("tool-refused".into()),
        plan: String::new(),
        plan_sha256: String::new(),
        allowed_prompts: Vec::new(),
        unsupported_reason: Some("The plan is longer than 256 KiB, so it was not shown".into()),
    };
    cases.push(case(
        "refused-plan-waits-for-settlement",
        RequestedCliMode::Default,
        vec![
            enter_call(),
            plan_report(),
            event(refused),
            event(settled("refused", PlanOutcome::HostRefused)),
        ],
    ));
    for ending in [
        AgentDomainEvent::SessionClosed {
            reason: "closed".into(),
        },
        AgentDomainEvent::SessionUnavailable { reason: "lost".into() },
    ] {
        cases.push(case(
            &format!("{ending:?}"),
            RequestedCliMode::Default,
            vec![
                enter_call(),
                plan_report(),
                event(proposed("settled")),
                event(settled("settled", PlanOutcome::KeptPlanning)),
                event(proposed("answering")),
                send("answering", "answering", true),
                event(ending),
                event(settled("answering", PlanOutcome::Approved)),
            ],
        ));
    }

    for (index, outcome) in outcomes.into_iter().enumerate() {
        let steps = &cases[index]["steps"];
        assert_eq!(steps[3]["snapshot"]["plans"][0]["state"], json!({ "kind": "waiting" }));
        assert_eq!(
            steps[4]["snapshot"]["plans"][0]["state"],
            json!({ "kind": "answering" })
        );
        let AgentDomainEvent::PlanSettled {
            mode_after,
            feedback,
            reason,
            ..
        } = settled("p", outcome)
        else {
            unreachable!()
        };
        assert_eq!(
            steps[5]["snapshot"]["plans"][0]["state"],
            json!({ "kind": "settled", "outcome": outcome, "modeAfter": mode_after, "feedback": feedback, "reason": reason })
        );
        assert_eq!(steps[5]["snapshot"]["plans"], steps[6]["snapshot"]["plans"]);
        assert_eq!(steps[2]["snapshot"]["planning"], json!({ "phase": "planning" }));
        // Only an approval starts the move out.
        let after = if outcome == PlanOutcome::Approved {
            "leaving"
        } else {
            "planning"
        };
        assert_eq!(steps[5]["snapshot"]["planning"]["phase"], after, "{outcome:?}");
    }
    let model = &cases[7]["steps"];
    let phases: Vec<_> = model
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["snapshot"]["planning"]["phase"].as_str().unwrap())
        .collect();
    assert_eq!(
        phases,
        [
            "executing",
            "entering",
            "planning",
            "planning",
            "planning",
            "leaving",
            "leaving",
            "executing"
        ]
    );
    let gate: Vec<_> = model
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["planGateApplies"].as_bool().unwrap())
        .collect();
    assert_eq!(gate, [false, false, true, true, true, true, true, false]);
    let host = &cases[8]["steps"];
    let phases: Vec<_> = host
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["snapshot"]["planning"]["phase"].as_str().unwrap())
        .collect();
    assert_eq!(
        phases,
        ["executing", "entering", "planning", "leaving", "leaving", "executing"]
    );
    assert_eq!(
        host[4]["planGateApplies"], false,
        "the CLI reported the move out of plan"
    );
    assert_eq!(cases[9]["steps"][2]["snapshot"]["plans"][0]["plan"], "");
    for closed in &cases[10..12] {
        let records = closed["steps"][6]["snapshot"]["plans"].as_array().unwrap();
        assert_eq!(records[0]["state"]["outcome"], "kept_planning");
        assert_eq!(records[1]["state"]["outcome"], "session_closed");
        assert_eq!(closed["steps"][6]["snapshot"]["planning"]["phase"], "executing");
        assert_eq!(closed["steps"][7]["snapshot"], closed["steps"][6]["snapshot"]);
    }
    let fixture = json!({ "writtenBy": "agent/src/testkit/plans_fixture.rs::plans_fixture", "cases": cases });
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/plans.json");
    let written = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
    if std::env::var("EITRI_WRITE_FIXTURES").as_deref() == Ok("1") {
        std::fs::write(&path, &written).unwrap();
    } else {
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            written,
            "regenerate with EITRI_WRITE_FIXTURES=1"
        );
    }
}
