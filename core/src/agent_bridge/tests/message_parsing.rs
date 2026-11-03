//! The hint envelopes, and how the page's plain messages parse: ready, send, interrupt, approvals and denials, malformed input, command results, events and the first snapshot.
#![cfg(test)]

use super::*;

#[test]
fn serializes_the_five_hint_envelopes() {
    let v = |s: String| serde_json::from_str::<serde_json::Value>(&s).unwrap();
    assert_eq!(
        v(serialize_hint_collect_for_js(7)),
        serde_json::json!({ "kind": "hint_collect", "sessionId": 7 })
    );
    assert_eq!(
        v(serialize_hint_show_for_js(7, &["a".into(), "s".into()])),
        serde_json::json!({ "kind": "hint_show", "sessionId": 7, "labels": ["a", "s"] })
    );
    assert_eq!(
        v(serialize_hint_prefix_for_js(7, "a")),
        serde_json::json!({ "kind": "hint_prefix", "sessionId": 7, "typed": "a" })
    );
    assert_eq!(
        v(serialize_hint_land_for_js(7, 2)),
        serde_json::json!({ "kind": "hint_land", "sessionId": 7, "index": 2 })
    );
    assert_eq!(
        v(serialize_hint_end_for_js(7)),
        serde_json::json!({ "kind": "hint_end", "sessionId": 7 })
    );
}

#[test]
fn parses_the_two_hint_messages() {
    let m = parse_inbound_message(r#"{"type":"hint_request","request_id":"r1"}"#).unwrap();
    assert_eq!(m.request_id(), "r1");
    assert!(matches!(m, InboundMessage::HintRequest { .. }));
    let m = parse_inbound_message(r#"{"type":"hint_targets","request_id":"r2","session_id":7,"count":3}"#).unwrap();
    assert!(matches!(
        m,
        InboundMessage::HintTargets {
            session_id: 7,
            count: 3,
            ..
        }
    ));
}

#[test]
fn parses_ready_with_request_id() {
    let msg = parse_inbound_message(r#"{"type":"ready","request_id":"r1"}"#).unwrap();
    // .request_id() (a &self borrow) is checked before the by-value `matches!` pattern below,
    // which binds `request_id: String` out of `msg` by value (InboundMessage isn't Copy) --
    // reversing this order would partial-move `msg` and fail to borrow-check on the next line.
    assert_eq!(msg.request_id(), "r1");
    assert!(matches!(msg, InboundMessage::Ready { request_id } if request_id == "r1"));
}

#[test]
fn parses_send_message() {
    let msg = parse_inbound_message(r#"{"type":"send_message","request_id":"r3","text":"hello"}"#).unwrap();
    match msg {
        InboundMessage::SendMessage { text, .. } => assert_eq!(text, "hello"),
        other => panic!("expected SendMessage, got {other:?}"),
    }
}

#[test]
fn parses_interrupt() {
    let msg = parse_inbound_message(r#"{"type":"interrupt","request_id":"r4"}"#).unwrap();
    assert!(matches!(msg, InboundMessage::Interrupt { .. }));
}

#[test]
fn parses_an_approval_which_carries_no_reason() {
    let msg = parse_inbound_message(
        r#"{"type":"permission_response","request_id":"r5","permission_id":"p1","decision":"allow"}"#,
    )
    .unwrap();
    match msg {
        InboundMessage::PermissionResponse {
            permission_id,
            decision,
            reason,
            ..
        } => {
            assert_eq!(permission_id, "p1");
            assert_eq!(decision, DecisionChoice::Allow);
            assert_eq!(decision.into_decision(reason), agent::PermissionDecision::Allow);
        }
        other => panic!("expected PermissionResponse, got {other:?}"),
    }
}

#[test]
fn parses_a_denial_and_carries_its_reason_to_the_model() {
    let msg = parse_inbound_message(
        r#"{"type":"permission_response","request_id":"r5","permission_id":"p1","decision":"deny","reason":"not in this repo"}"#,
    )
    .unwrap();
    match msg {
        InboundMessage::PermissionResponse { decision, reason, .. } => {
            assert_eq!(
                decision.into_decision(reason),
                agent::PermissionDecision::Deny {
                    reason: Some("not in this repo".into())
                }
            );
        }
        other => panic!("expected PermissionResponse, got {other:?}"),
    }
}

/// An empty reason box is not a reason. Sending the model "" as its explanation is worse than
/// sending it nothing, because nothing at least reads as "no reason given".
#[test]
fn a_blank_deny_reason_becomes_no_reason_at_all() {
    let msg = parse_inbound_message(
        r#"{"type":"permission_response","request_id":"r5","permission_id":"p1","decision":"deny","reason":"   "}"#,
    )
    .unwrap();
    match msg {
        InboundMessage::PermissionResponse { decision, reason, .. } => {
            assert_eq!(
                decision.into_decision(reason),
                agent::PermissionDecision::Deny { reason: None }
            );
        }
        other => panic!("expected PermissionResponse, got {other:?}"),
    }
}

/// The failure direction that matters: a decision this build does not recognize must be
/// REJECTED, never defaulted. A `bool allow` field could not express this -- any unknown value
/// would have had to become one of the two, and the tempting default is the one that runs the
/// tool.
#[test]
fn an_unrecognized_decision_is_rejected_rather_than_defaulted() {
    assert!(parse_inbound_message(
        r#"{"type":"permission_response","request_id":"r5","permission_id":"p1","decision":"allow_for_session"}"#
    )
    .is_none());
    assert!(
        parse_inbound_message(r#"{"type":"permission_response","request_id":"r5","permission_id":"p1","allow":true}"#)
            .is_none(),
        "the old bool shape must not still be accepted"
    );
}

#[test]
fn unparseable_json_returns_none_not_a_panic() {
    assert!(parse_inbound_message("not json at all {{{").is_none());
}

#[test]
fn unrecognized_type_returns_none_not_a_panic() {
    assert!(parse_inbound_message(r#"{"type":"some_future_message_type","request_id":"r6"}"#).is_none());
}

#[test]
fn missing_request_id_returns_none_not_a_panic() {
    assert!(parse_inbound_message(r#"{"type":"ready"}"#).is_none());
}

#[test]
fn serialize_command_result_ok() {
    let json_str = serialize_command_result_for_js("r1", Ok(()));
    let parsed: Value = serde_json::from_str(&json_str).unwrap();
    assert_eq!(parsed["kind"], "command_result");
    assert_eq!(parsed["requestId"], "r1");
    assert_eq!(parsed["ok"], true);
}

#[test]
fn serialize_command_result_error() {
    let json_str = serialize_command_result_for_js("r1", Err("boom"));
    let parsed: Value = serde_json::from_str(&json_str).unwrap();
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["error"], "boom");
}

#[test]
fn serialize_events_for_js_carries_revision_range_and_tagged_events() {
    let events = vec![AgentDomainEvent::PermissionResolved {
        permission_id: "p1".into(),
        outcome: PermissionOutcome::Allowed,
    }];
    let json_str = serialize_events_for_js(TabId(1), 3, 4, &events);
    let parsed: Value = serde_json::from_str(&json_str).unwrap();
    assert_eq!(parsed["kind"], "events");
    assert_eq!(parsed["tab"], 1);
    assert_eq!(parsed["fromRevision"], 3);
    assert_eq!(parsed["throughRevision"], 4);
    assert_eq!(parsed["events"][0]["type"], "permission_resolved");
    assert_eq!(parsed["events"][0]["permission_id"], "p1");
}

#[test]
fn serialize_events_preserves_optional_fold_revisions_and_untagged_bytes() {
    let events = vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }];
    let notes = CallNotes::default();
    let plain = serialize_events_for_js(TabId(1), 3, 5, &events);
    assert_eq!(
        plain,
        serialize_events_with_notes_for_js(TabId(1), 3, 5, &events, &notes)
    );
    assert_eq!(
        plain,
        serialize_events_with_notes_and_revisions_for_js(TabId(1), 3, 5, &events, &notes, None)
    );
    let mut expected: Value = serde_json::from_str(&plain).unwrap();
    assert!(expected.get("revisions").is_none());
    expected["revisions"] = json!([5]);
    let tagged = serialize_events_with_notes_and_revisions_for_js(TabId(1), 3, 5, &events, &notes, Some(&[5]));
    assert_eq!(serde_json::from_str::<Value>(&tagged).unwrap(), expected);
}

#[test]
fn serialize_error_for_js_wraps_in_kind_error_envelope() {
    let json_str = serialize_error_for_js(TabId(1), "failed to start session: no such file or directory");
    let parsed: Value = serde_json::from_str(&json_str).unwrap();
    assert_eq!(parsed["kind"], "error");
    assert_eq!(parsed["tab"], 1);
    assert_eq!(parsed["message"], "failed to start session: no such file or directory");
}
