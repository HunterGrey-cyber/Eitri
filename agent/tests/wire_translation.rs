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
    assert_eq!(
        events[0],
        AgentEvent::AssistantText {
            text: "pong".to_string(),
            message_id: Some("msg_011Cep1VPpoKuerNr9Wjt2gL".to_string()),
        }
    );
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
        AgentEvent::TurnFinished {
            result_text,
            is_error,
            num_turns,
            ..
        } => {
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
    let events = translate_line(r#"{"type": "system", "subtype": "hook_started", "hook_id": "x", "session_id": "y"}"#);
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
        AgentEvent::ControlResponse {
            request_id, subtype, ..
        } => {
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
        AgentEvent::PermissionRequest {
            request_id,
            tool_name,
            source,
            ..
        } => {
            assert_eq!(request_id, "ctu-1");
            assert_eq!(tool_name, "Bash");
            assert_eq!(*source, PermissionSource::CanUseTool);
        }
        other => panic!("expected PermissionRequest, got {other:?}"),
    }
}

/// The guard that keeps the two permission sources from being confused with each other. On the
/// hook-relay path `request_id` genuinely IS the `toolu_*` id of the gated call; here it is the
/// control_request ENVELOPE's own id (`"ctu-1"` in this fixture), which names nothing in the
/// transcript. Passing it off as a tool-use id would produce a permission card claiming to point
/// at a tool call that does not exist.
///
/// The fixture's two ids differ on purpose, so "the envelope id was substituted" and "the real
/// field was read" cannot both satisfy this test.
///
/// **On the fixture's own value:** `toolu_01CTUexamplePlaceholder01` is synthetic. This project
/// has never observed a `can_use_tool` control_request on a real wire; the field is in the fixture
/// because the SDK's declared type marks it required (`agent/CAPTURE_NOTES.md` step 6). So this
/// test pins how `wire.rs` treats the declared shape -- it is not evidence about what the CLI
/// really sends.
#[test]
fn can_use_tool_never_passes_its_envelope_request_id_off_as_a_tool_use_id() {
    let events = translate_line(fixture("v2_control_request_can_use_tool.json").trim());
    match &events[0] {
        AgentEvent::PermissionRequest {
            request_id,
            tool_use_id,
            ..
        } => {
            assert_eq!(request_id, "ctu-1");
            assert_eq!(
                tool_use_id.as_deref(),
                Some("toolu_01CTUexamplePlaceholder01"),
                "the message's own tool_use_id field is what gets read"
            );
            assert_ne!(
                tool_use_id.as_deref(),
                Some(request_id.as_str()),
                "the envelope's request_id must never be promoted into a tool-use id"
            );
        }
        other => panic!("expected PermissionRequest, got {other:?}"),
    }
}

/// The same message with the declared field absent. Written inline rather than checked in as a
/// fixture precisely because it is NOT a capture of anything -- it is the degraded shape that
/// `#[serde(default)]` exists for, and pinning it stops an absent field from being answered with
/// the envelope id or with a parse failure that would drop a real permission request on the floor.
#[test]
fn a_can_use_tool_request_without_the_declared_tool_use_id_is_unlinked_rather_than_unparseable() {
    let line = r#"{"type":"control_request","request_id":"ctu-9","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"echo hi"}}}"#;
    let events = translate_line(line);
    assert_eq!(
        events.len(),
        1,
        "an absent optional field must not turn this into an Unknown"
    );
    match &events[0] {
        AgentEvent::PermissionRequest {
            request_id,
            tool_use_id,
            tool_name,
            source,
            ..
        } => {
            assert_eq!(request_id, "ctu-9");
            assert_eq!(*tool_use_id, None);
            assert_eq!(tool_name, "Bash");
            assert_eq!(*source, PermissionSource::CanUseTool);
        }
        other => panic!("expected PermissionRequest, got {other:?}"),
    }
}
