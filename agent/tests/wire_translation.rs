use agent::{translate_line, AgentEvent, PermissionSource};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR")))
        .unwrap_or_else(|e| panic!("reading fixture {name}: {e}"))
}

#[test]
fn init_line_becomes_session_started() {
    let events = translate_line(fixture("init.json").trim());
    assert_eq!(events.len(), 1);
    match &events[0] {
        AgentEvent::SessionStarted { session_id, model, cwd } => {
            assert_eq!(session_id, "acaea690-c694-492f-979d-1d7e3621d99f");
            assert_eq!(model, "claude-sonnet-5");
            assert_eq!(cwd, "/home/user/src/neovibe");
        }
        other => panic!("expected SessionStarted, got {other:?}"),
    }
}

#[test]
fn assistant_text_block_becomes_assistant_text() {
    let events = translate_line(fixture("assistant_text.json").trim());
    assert_eq!(events.len(), 1);
    assert_eq!(events[0], AgentEvent::AssistantText { text: "pong".to_string() });
}

#[test]
fn assistant_tool_use_block_becomes_tool_started() {
    let events = translate_line(fixture("assistant_tool_use.json").trim());
    assert_eq!(events.len(), 1);
    match &events[0] {
        AgentEvent::ToolStarted { id, name, input } => {
            assert_eq!(id, "toolu_01BNd1uugPz7giUUePqgCeyD");
            assert_eq!(name, "Bash");
            assert_eq!(input["command"], "echo hi");
        }
        other => panic!("expected ToolStarted, got {other:?}"),
    }
}

#[test]
fn user_tool_result_becomes_tool_result() {
    let events = translate_line(fixture("user_tool_result.json").trim());
    assert_eq!(events.len(), 1);
    match &events[0] {
        AgentEvent::ToolResult { id, content, is_error } => {
            assert_eq!(id, "toolu_01BNd1uugPz7giUUePqgCeyD");
            assert_eq!(content, "hi");
            assert!(!is_error);
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
}

#[test]
fn result_line_becomes_turn_finished() {
    let events = translate_line(fixture("result_success.json").trim());
    assert_eq!(events.len(), 1);
    match &events[0] {
        AgentEvent::TurnFinished { result_text, is_error, num_turns, .. } => {
            assert_eq!(result_text, "pong");
            assert!(!is_error);
            assert_eq!(*num_turns, 1);
        }
        other => panic!("expected TurnFinished, got {other:?}"),
    }
}

#[test]
fn rate_limit_event_is_passed_through_raw() {
    let events = translate_line(fixture("rate_limit_event.json").trim());
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], AgentEvent::RateLimit { .. }));
}

#[test]
fn unrecognized_event_type_becomes_unknown_not_a_panic() {
    let events = translate_line(r#"{"type": "some_future_event_type", "subtype": "whatever"}"#);
    assert_eq!(events.len(), 1);
    match &events[0] {
        AgentEvent::Unknown { kind, subtype, .. } => {
            assert_eq!(kind, "some_future_event_type");
            assert_eq!(subtype.as_deref(), Some("whatever"));
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}

#[test]
fn garbage_that_is_not_even_json_becomes_unknown_not_a_panic() {
    let events = translate_line("this is not json at all {{{");
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], AgentEvent::Unknown { .. }));
}

#[test]
fn unrecognized_content_block_kind_preserves_its_raw_json() {
    // A future content-block kind this crate doesn't yet model (e.g. `redacted_thinking`,
    // `server_tool_use`, `image`) must still be recoverable from the `Unknown` event's `raw`
    // field, not discarded as `Value::Null` -- every other `Unknown` path in this file preserves
    // the real value it couldn't fully parse, and this one must too.
    let events = translate_line(
        r#"{"type": "assistant", "message": {"content": [{"type": "redacted_thinking", "data": "abc123"}]}}"#,
    );
    assert_eq!(events.len(), 1);
    match &events[0] {
        AgentEvent::Unknown { kind, subtype, raw } => {
            assert_eq!(kind, "assistant_content_block");
            assert_eq!(subtype.as_deref(), Some("redacted_thinking"));
            assert_eq!(raw, &serde_json::json!({"type": "redacted_thinking", "data": "abc123"}));
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}

#[test]
fn a_line_with_a_hook_subtype_this_crate_does_not_model_becomes_unknown() {
    // Real observed shape this crate deliberately does not special-case in v1.
    let events = translate_line(
        r#"{"type": "system", "subtype": "hook_started", "hook_id": "x", "session_id": "y"}"#,
    );
    assert_eq!(events.len(), 1);
    match &events[0] {
        AgentEvent::Unknown { kind, subtype, .. } => {
            assert_eq!(kind, "system");
            assert_eq!(subtype.as_deref(), Some("hook_started"));
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}

#[test]
fn control_response_becomes_control_response_event() {
    let events = translate_line(fixture("v2_control_response_interrupt.json").trim());
    assert_eq!(events.len(), 1);
    match &events[0] {
        AgentEvent::ControlResponse { request_id, subtype, .. } => {
            assert_eq!(request_id, "int-1");
            assert_eq!(subtype, "success");
        }
        other => panic!("expected ControlResponse, got {other:?}"),
    }
}

#[test]
fn interrupted_result_still_becomes_turn_finished_with_error() {
    let events = translate_line(fixture("v2_result_interrupted.json").trim());
    assert_eq!(events.len(), 1);
    match &events[0] {
        AgentEvent::TurnFinished { is_error, .. } => assert!(*is_error),
        other => panic!("expected TurnFinished, got {other:?}"),
    }
}

#[test]
fn can_use_tool_control_request_becomes_permission_request_with_can_use_tool_source() {
    let events = translate_line(fixture("v2_control_request_can_use_tool.json").trim());
    assert_eq!(events.len(), 1);
    match &events[0] {
        AgentEvent::PermissionRequest { request_id, tool_name, source, .. } => {
            assert_eq!(request_id, "ctu-1");
            assert_eq!(tool_name, "Bash");
            assert_eq!(*source, PermissionSource::CanUseTool);
        }
        other => panic!("expected PermissionRequest, got {other:?}"),
    }
}
