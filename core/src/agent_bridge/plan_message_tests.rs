//! The page's plan messages on the wire: a decision names its plan, its choice from a closed set and the
//! digest the person read, and takes no other field; a switch says which way; a feedback draft is a mirror.
//! Host to page: the snapshot's `planning` with its origin, `planMode` in the capabilities, and the events
//! envelope's `planningChanged`.
#![cfg(test)]
use super::*;
use crate::tab_set::PlanChoice;
use crate::tabs::TabId;

const SHA: &str = "00000000000000000000000000000000000000000000000000000000000000aa";

fn resolve(choice: &str, extra: &str) -> String {
    format!(
        r#"{{"type":"plan_resolve","request_id":"r1","tab":3,"interaction_id":"p1","choice":"{choice}","plan_sha256":"{SHA}"{extra}}}"#
    )
}

fn decision_of(raw: &str) -> Result<PlanDecision, String> {
    match parse_inbound_message(raw).expect("parses") {
        InboundMessage::PlanResolve(message) => message.decision(),
        other => panic!("not a plan_resolve: {other:?}"),
    }
}

#[test]
fn a_plan_resolve_parses_each_choice_and_refuses_unknown_fields() {
    for (word, expected) in [
        ("approve_manual", PlanDecision::Approve(PlanChoice::ApproveManual)),
        ("approve_auto", PlanDecision::Approve(PlanChoice::ApproveAuto)),
        ("approve_bypass", PlanDecision::Approve(PlanChoice::ApproveBypass)),
        (
            "keep_planning",
            PlanDecision::KeepPlanning {
                feedback: String::new(),
            },
        ),
    ] {
        let message = parse_inbound_message(&resolve(word, "")).unwrap_or_else(|| panic!("{word} parses"));
        assert_eq!(message.request_id(), "r1");
        assert_eq!(message.tab_ref(), TabRef::Named(TabId(3)));
        let InboundMessage::PlanResolve(resolve) = message else {
            panic!("a plan_resolve");
        };
        assert_eq!(resolve.interaction_id, "p1");
        assert_eq!(resolve.plan_sha256, SHA);
        assert_eq!(resolve.decision(), Ok(expected), "{word}");
    }
    // No tab is a protocol error at resolution, never "the active one".
    let untabbed = parse_inbound_message(&resolve("approve_manual", "").replace(r#","tab":3"#, "")).unwrap();
    assert_eq!(untabbed.tab_ref(), TabRef::Missing);
    // Only the four choices, and only the named fields: no mode, no rule, no input the sidecar did not build.
    for raw in [
        resolve("approve", ""),
        resolve("approve_accept_edits", ""),
        resolve("Approve_manual", ""),
        resolve("APPROVE_AUTO", ""),
        resolve("", ""),
        resolve("approve_auto", r#","mode":"bypass""#),
        resolve("approve_auto", r#","updatedInput":{}"#),
        resolve("approve_auto", r#","remember":true"#),
        resolve("approve_auto", r#","allowedPrompts":[]"#),
        resolve("keep_planning", r#","feedback":7"#),
        r#"{"type":"plan_resolve","request_id":"r1","tab":3,"choice":"approve_auto","plan_sha256":"x"}"#.into(),
        r#"{"type":"plan_resolve","request_id":"r1","tab":3,"interaction_id":"p1","choice":"approve_auto"}"#.into(),
        r#"{"type":"plan_resolve","request_id":"r1","tab":3,"interaction_id":"p1","plan_sha256":"x"}"#.into(),
        r#"{"type":"plan_resolve","tab":3,"interaction_id":"p1","choice":"approve_auto","plan_sha256":"x"}"#.into(),
    ] {
        assert!(parse_inbound_message(&raw).is_none(), "{raw}");
    }
}

#[test]
fn keep_planning_carries_its_feedback_and_an_approval_none() {
    assert_eq!(
        decision_of(&resolve("keep_planning", r#","feedback":"Keep it shorter""#)),
        Ok(PlanDecision::KeepPlanning {
            feedback: "Keep it shorter".into()
        })
    );
    // Absent and null are the same: an empty feedback, which the CLI words as its plain rejection.
    assert_eq!(
        decision_of(&resolve("keep_planning", r#","feedback":null"#)),
        Ok(PlanDecision::KeepPlanning {
            feedback: String::new()
        })
    );
    for choice in ["approve_manual", "approve_auto", "approve_bypass"] {
        assert!(decision_of(&resolve(choice, r#","feedback":null"#)).is_ok(), "{choice}");
        let refused = decision_of(&resolve(choice, r#","feedback":"and also run the tests""#));
        assert!(
            matches!(&refused, Err(why) if why.starts_with("protocol:")),
            "{choice}: {refused:?}"
        );
        // An empty string is still feedback the person never gave with an approval.
        assert!(decision_of(&resolve(choice, r#","feedback":"""#)).is_err(), "{choice}");
    }
}

#[test]
fn set_planning_parses() {
    for enter in [true, false] {
        let message = parse_inbound_message(&format!(
            r#"{{"type":"set_planning","request_id":"s1","tab":4,"enter":{enter}}}"#
        ))
        .expect("parses");
        assert_eq!(message.request_id(), "s1");
        assert_eq!(message.tab_ref(), TabRef::Named(TabId(4)));
        assert!(matches!(message, InboundMessage::SetPlanning { enter: e, .. } if e == enter));
    }
    // `enter` has no default: a switch that does not say which way is no switch.
    for raw in [
        r#"{"type":"set_planning","request_id":"s1","tab":4}"#,
        r#"{"type":"set_planning","request_id":"s1","tab":4,"enter":"yes"}"#,
        r#"{"type":"set_planning","request_id":"s1","tab":4,"enter":null}"#,
    ] {
        assert!(parse_inbound_message(raw).is_none(), "{raw}");
    }
}

#[test]
fn plan_feedback_draft_parses_as_a_mirror() {
    let message = parse_inbound_message(
        r#"{"type":"plan_feedback_draft","request_id":"d1","tab":2,"interaction_id":"p1","text":"shorter"}"#,
    )
    .expect("parses");
    assert_eq!(message.request_id(), "d1");
    assert_eq!(message.tab_ref(), TabRef::Named(TabId(2)));
    assert!(
        matches!(message, InboundMessage::PlanFeedbackDraft { interaction_id, text, .. } if interaction_id == "p1" && text == "shorter")
    );
    assert!(parse_inbound_message(r#"{"type":"plan_feedback_draft","request_id":"d1","tab":2,"text":"x"}"#).is_none());
}

fn snapshot_of(projection: &agent::AgentSessionProjection, plan_mode_v1: bool, notes: &CallNotes) -> Value {
    let view = SnapshotView {
        backend: "sidecar",
        conversation_id: None,
        session_id: None,
        provider_session_id: None,
        capabilities: agent::ProviderCapabilities {
            plan_mode_v1,
            ..agent::ProviderCapabilities::default()
        },
        provider: None,
        projection: crate::agent_backend::ProjectionRef::Borrowed(projection),
        hidden_pending: None,
    };
    serde_json::from_str(&serialize_snapshot_with_notes_for_js(TabId(1), &view, None, notes, &[])).unwrap()
}

fn offered(plan_offered: bool) -> agent::AgentSessionProjection {
    let mut projection = agent::AgentSessionProjection::default();
    projection.offer_plan_mode(agent::CliExpectation {
        requested: agent::RequestedCliMode::Default,
        plan_offered,
    });
    projection
}

#[test]
fn the_page_capabilities_say_plan_mode_only_with_the_capability_and_the_opt_in() {
    for (capable, opted_in) in [(false, false), (true, false), (false, true), (true, true)] {
        let snapshot = snapshot_of(&offered(opted_in), capable, &CallNotes::default());
        let says = snapshot["state"]["capabilities"].get("planMode").cloned();
        if capable && opted_in {
            assert_eq!(says, Some(json!(true)));
        } else {
            assert_eq!(says, None, "capable {capable}, opted in {opted_in}");
        }
    }
}

#[test]
fn the_snapshot_says_the_phase_and_the_origin_only_on_a_plan_session() {
    assert!(snapshot_of(&offered(false), true, &CallNotes::default())["state"]
        .get("planning")
        .is_none());
    // No note yet: the phase from the projection, no origin.
    assert_eq!(
        snapshot_of(&offered(true), true, &CallNotes::default())["state"]["planning"],
        json!({ "phase": "executing", "origin": null })
    );
    let mut projection = offered(true);
    projection.apply(&agent::AgentDomainEvent::PlanTransitionStarted {
        enter: true,
        cause: agent::PlanCause::Host,
        at_ms: 1,
    });
    projection.apply(&agent::AgentDomainEvent::CliPlanMode {
        source: "CliModeReported".into(),
    });
    let notes = CallNotes {
        planning: Some(PlanningNote {
            phase: "planning",
            origin: Some(SessionModeChoice::Bypass),
        }),
        ..CallNotes::default()
    };
    assert_eq!(
        snapshot_of(&projection, true, &notes)["state"]["planning"],
        json!({ "phase": "planning", "origin": "bypass" })
    );
}

#[test]
fn planning_changed_rides_the_events_envelope_only_when_noted() {
    let quiet: Value = serde_json::from_str(&serialize_events_with_notes_for_js(
        TabId(1),
        0,
        0,
        &[],
        &CallNotes::default(),
    ))
    .unwrap();
    assert!(quiet.get("planningChanged").is_none());
    for (phase, origin, wire) in [
        ("entering", Some(SessionModeChoice::Manual), json!("manual")),
        ("planning", Some(SessionModeChoice::Auto), json!("auto")),
        ("leaving", Some(SessionModeChoice::Bypass), json!("bypass")),
        ("executing", None, Value::Null),
    ] {
        let notes = CallNotes {
            planning: Some(PlanningNote { phase, origin }),
            ..CallNotes::default()
        };
        assert!(!notes.is_empty(), "a change alone is worth an envelope");
        let payload: Value =
            serde_json::from_str(&serialize_events_with_notes_for_js(TabId(1), 4, 4, &[], &notes)).unwrap();
        assert_eq!(payload["planningChanged"], json!({ "phase": phase, "origin": wire }));
        // Not an event: the page's numbering of the conversation is Rust's, one per folded event.
        assert_eq!(payload["events"], json!([]));
    }
}

#[test]
fn the_tab_row_carries_the_feedback_drafts_and_a_start_in_plan_mode() {
    let row = |drafts: BTreeMap<String, String>, plan_on_start: bool| -> Value {
        let view = TabView {
            following: false,
            question_draft: BTreeMap::new(),
            plan_feedback_draft: drafts,
            plan_on_start,
            id: TabId(1),
            number: 1,
            label: "1".into(),
            name: None,
            state: TabStateWire::NotStarted,
            mode: SessionModeChoice::Manual,
            marker: None,
            pending: 0,
            resumable: false,
            failure: None,
            failed_after_start: false,
            resumes_on_r: false,
            title: None,
            agent: "claude".into(),
        };
        let wire: Value =
            serde_json::from_str(&serialize_tabs_for_js(TabId(1), &[view], SessionModeChoice::Manual)).unwrap();
        wire["tabs"][0].clone()
    };
    let bare = row(BTreeMap::new(), false);
    assert!(bare.get("planFeedbackDraft").is_none() && bare.get("planOnStart").is_none());
    let drafted = row(BTreeMap::from([("p1".to_string(), "shorter".to_string())]), true);
    assert_eq!(drafted["planFeedbackDraft"], json!({ "p1": "shorter" }));
    assert_eq!(drafted["planOnStart"], json!(true));
}
