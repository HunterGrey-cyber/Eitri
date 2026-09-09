use agent::{
    AgentDomainEvent, AgentSessionProjection, ContentKind, PermissionOutcome, ProjectionStatus,
    TurnOutcome,
};
use serde_json::json;

#[test]
fn session_opened_populates_identity_and_sets_running() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::SessionOpened {
        session_id: "sess-1".into(),
        model: "claude-sonnet-5".into(),
        cwd: "/tmp/project".into(),
    });
    assert_eq!(projection.session_id, Some("sess-1".into()));
    assert_eq!(projection.model, Some("claude-sonnet-5".into()));
    assert_eq!(projection.cwd, Some("/tmp/project".into()));
    assert_eq!(projection.status, ProjectionStatus::Running);
    assert_eq!(projection.last_revision, 1);
}

#[test]
fn turn_started_sets_active_turn_id() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "turn-1".into() });
    assert_eq!(projection.active_turn_id, Some("turn-1".into()));
}

#[test]
fn content_delta_text_appends_to_transcript_in_order() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::ContentDelta { turn_id: "turn-1".into(), kind: ContentKind::Text, text: "hello".into() });
    projection.apply(&AgentDomainEvent::ContentDelta { turn_id: "turn-1".into(), kind: ContentKind::Text, text: "world".into() });
    assert_eq!(projection.transcript, vec!["hello".to_string(), "world".to_string()]);
}

#[test]
fn content_delta_thinking_has_no_transcript_effect_but_still_bumps_revision() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::ContentDelta { turn_id: "turn-1".into(), kind: ContentKind::Thinking, text: "hmm".into() });
    assert!(projection.transcript.is_empty());
    assert_eq!(projection.last_revision, 1);
}

#[test]
fn tool_call_started_then_completed_links_by_tool_use_id() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::ToolCallStarted {
        turn_id: "turn-1".into(), tool_use_id: "toolu_1".into(), name: "Bash".into(), input: json!({"command": "echo hi"}),
    });
    projection.apply(&AgentDomainEvent::ToolCallCompleted {
        turn_id: "turn-1".into(), tool_use_id: "toolu_1".into(), content: json!("hi\n"), is_error: false,
    });
    assert_eq!(projection.tool_calls.len(), 1);
    let call = &projection.tool_calls[0];
    assert_eq!(call.tool_use_id, "toolu_1");
    let result = call.result.as_ref().expect("result should be set");
    assert_eq!(result.content, json!("hi\n"));
    assert!(!result.is_error);
}

#[test]
fn tool_call_completed_for_unknown_tool_use_id_is_a_harmless_no_op() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::ToolCallCompleted {
        turn_id: "turn-1".into(), tool_use_id: "does-not-exist".into(), content: json!("x"), is_error: false,
    });
    assert!(projection.tool_calls.is_empty());
}

#[test]
fn permission_requested_populates_pending_permissions_map() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::PermissionRequested {
        permission_id: "perm-1".into(), tool_name: "Bash".into(), input: json!({}),
    });
    assert!(projection.pending_permissions.contains_key("perm-1"));
    assert_eq!(projection.pending_permissions["perm-1"].tool_name, "Bash");
}

/// The scenario `agent/BACKEND_BASELINE.md` cites by name as pinning concurrent-permission
/// handling: two simultaneously-pending requests must both be retained and independently
/// resolvable in EITHER order, not just the order they arrived in.
#[test]
fn two_concurrent_permission_requests_are_both_retained_and_independently_resolvable_in_either_order() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::PermissionRequested {
        permission_id: "perm-1".into(), tool_name: "Bash".into(), input: json!({}),
    });
    projection.apply(&AgentDomainEvent::PermissionRequested {
        permission_id: "perm-2".into(), tool_name: "Write".into(), input: json!({}),
    });
    assert_eq!(projection.pending_permissions.len(), 2);

    // Resolve the SECOND request first -- proves order-independence, not just that two can coexist.
    projection.apply(&AgentDomainEvent::PermissionResolved { permission_id: "perm-2".into(), outcome: PermissionOutcome::Allowed });
    assert!(!projection.pending_permissions.contains_key("perm-2"));
    assert!(projection.pending_permissions.contains_key("perm-1"));

    projection.apply(&AgentDomainEvent::PermissionResolved { permission_id: "perm-1".into(), outcome: PermissionOutcome::Denied });
    assert!(projection.pending_permissions.is_empty());
}

#[test]
fn permission_resolved_for_unknown_id_is_a_harmless_no_op() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::PermissionResolved { permission_id: "does-not-exist".into(), outcome: PermissionOutcome::Allowed });
    assert!(projection.pending_permissions.is_empty());
}

#[test]
fn turn_completed_clears_active_turn_id_and_updates_usage_for_every_outcome() {
    for outcome in [TurnOutcome::Completed, TurnOutcome::Interrupted, TurnOutcome::Failed, TurnOutcome::LimitReached] {
        let mut projection = AgentSessionProjection::default();
        projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "turn-1".into() });
        projection.apply(&AgentDomainEvent::TurnCompleted {
            turn_id: "turn-1".into(), outcome, result_text: "done".into(), stop_reason: None,
            total_cost_usd: 0.01, num_turns: 1,
        });
        assert_eq!(projection.active_turn_id, None, "outcome {outcome:?} must clear active_turn_id");
        assert_eq!(projection.usage.total_cost_usd, 0.01);
        assert_eq!(projection.usage.num_turns, 1);
    }
}

#[test]
fn session_unavailable_and_session_closed_set_distinct_statuses() {
    let mut unavailable = AgentSessionProjection::default();
    unavailable.apply(&AgentDomainEvent::SessionUnavailable { reason: "crashed".into() });
    assert_eq!(unavailable.status, ProjectionStatus::Unavailable { reason: "crashed".into() });

    let mut closed = AgentSessionProjection::default();
    closed.apply(&AgentDomainEvent::SessionClosed { reason: "closed_by_host".into() });
    assert_eq!(closed.status, ProjectionStatus::Closed { reason: "closed_by_host".into() });
}

#[test]
fn every_apply_call_bumps_last_revision_by_exactly_one() {
    let mut projection = AgentSessionProjection::default();
    let events = vec![
        AgentDomainEvent::SessionOpened { session_id: "s".into(), model: "m".into(), cwd: "/".into() },
        AgentDomainEvent::TurnStarted { turn_id: "t".into() },
        AgentDomainEvent::ContentDelta { turn_id: "t".into(), kind: ContentKind::Thinking, text: "".into() },
    ];
    for (i, event) in events.iter().enumerate() {
        projection.apply(event);
        assert_eq!(projection.last_revision, (i + 1) as u64);
    }
}
