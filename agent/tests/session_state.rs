use agent::{AgentEvent, AgentSessionState, SessionStatus};
use serde_json::json;

#[test]
fn session_started_populates_identity_and_sets_running() {
    let mut state = AgentSessionState::default();
    state.apply(&AgentEvent::SessionStarted {
        session_id: "abc-123".into(),
        model: "claude-sonnet-5".into(),
        cwd: "/tmp".into(),
    });
    assert_eq!(state.session_id.as_deref(), Some("abc-123"));
    assert_eq!(state.model.as_deref(), Some("claude-sonnet-5"));
    assert!(matches!(state.status, SessionStatus::Running));
}

#[test]
fn session_started_with_a_freshly_generated_uuid_populates_session_id() {
    // Mirrors what `AgentSession::start_new` does internally (`Uuid::new_v4()` fed into
    // `SpawnMode::New`, which the CLI then echoes back in its own `system`/`init` line) --
    // constructed by hand here, without spawning a real process, to keep this a pure reducer
    // test.
    let generated = uuid::Uuid::new_v4();
    let mut state = AgentSessionState::default();
    state.apply(&AgentEvent::SessionStarted {
        session_id: generated.to_string(),
        model: "claude-sonnet-5".into(),
        cwd: "/tmp".into(),
    });
    assert_eq!(state.session_id.as_deref(), Some(generated.to_string().as_str()));
}

#[test]
fn assistant_text_appends_to_transcript_in_order() {
    let mut state = AgentSessionState::default();
    state.apply(&AgentEvent::AssistantText { text: "first".into() });
    state.apply(&AgentEvent::AssistantText { text: "second".into() });
    assert_eq!(state.transcript, vec!["first".to_string(), "second".to_string()]);
}

#[test]
fn tool_started_then_tool_result_links_by_id() {
    let mut state = AgentSessionState::default();
    state.apply(&AgentEvent::ToolStarted {
        id: "toolu_1".into(),
        name: "Bash".into(),
        input: json!({"command": "echo hi"}),
    });
    assert_eq!(state.tool_calls.len(), 1);
    assert!(state.tool_calls[0].result.is_none());

    state.apply(&AgentEvent::ToolResult {
        id: "toolu_1".into(),
        content: json!("hi"),
        is_error: false,
    });
    assert_eq!(state.tool_calls.len(), 1);
    assert_eq!(state.tool_calls[0].result, Some((json!("hi"), false)));
}

#[test]
fn tool_result_for_unknown_id_is_a_harmless_no_op() {
    let mut state = AgentSessionState::default();
    state.apply(&AgentEvent::ToolResult {
        id: "no-such-tool-call".into(),
        content: json!("whatever"),
        is_error: false,
    });
    assert!(state.tool_calls.is_empty());
}

#[test]
fn turn_finished_sets_finished_status_with_error_flag() {
    let mut state = AgentSessionState::default();
    state.apply(&AgentEvent::TurnFinished {
        result_text: "done".into(),
        is_error: true,
        stop_reason: Some("end_turn".into()),
        total_cost_usd: 0.01,
        num_turns: 1,
    });
    assert!(matches!(state.status, SessionStatus::Finished { is_error: true }));
}

#[test]
fn unknown_and_thinking_and_rate_limit_events_have_no_state_effect() {
    // Start from non-trivial, populated state (not `AgentSessionState::default()`) -- a
    // from-default baseline can't catch an "overwrite-to-default" regression, e.g. an accidental
    // `self.session_id = None` inside one of these arms would be invisible since the field is
    // already `None` at default. Populate first, snapshot, then apply the supposedly-no-op
    // events and assert byte-identical `Debug` output against that populated snapshot.
    let mut state = AgentSessionState::default();
    state.apply(&AgentEvent::SessionStarted {
        session_id: "abc-123".into(),
        model: "claude-sonnet-5".into(),
        cwd: "/tmp".into(),
    });
    state.apply(&AgentEvent::AssistantText { text: "hello".into() });
    state.apply(&AgentEvent::ToolStarted {
        id: "toolu_1".into(),
        name: "Bash".into(),
        input: json!({"command": "echo hi"}),
    });

    let before = format!("{state:?}");
    state.apply(&AgentEvent::Thinking { text: "".into() });
    state.apply(&AgentEvent::RateLimit { raw: json!({}) });
    state.apply(&AgentEvent::Unknown { kind: "x".into(), subtype: None, raw: json!({}) });
    state.apply(&AgentEvent::ProcessStderr { line: "some diagnostic line".into() });
    assert_eq!(format!("{state:?}"), before);
}

#[test]
fn process_exited_sets_finished_with_is_error_when_status_still_starting() {
    let mut state = AgentSessionState::default();
    assert!(matches!(state.status, SessionStatus::Starting));
    state.apply(&AgentEvent::ProcessExited { success: false });
    assert!(matches!(state.status, SessionStatus::Finished { is_error: true }));
}

#[test]
fn process_exited_sets_finished_without_error_on_clean_exit_from_running() {
    let mut state = AgentSessionState::default();
    state.apply(&AgentEvent::SessionStarted {
        session_id: "abc-123".into(),
        model: "claude-sonnet-5".into(),
        cwd: "/tmp".into(),
    });
    assert!(matches!(state.status, SessionStatus::Running));
    state.apply(&AgentEvent::ProcessExited { success: true });
    assert!(matches!(state.status, SessionStatus::Finished { is_error: false }));
}

#[test]
fn process_exited_is_a_no_op_once_turn_already_finished() {
    // A `TurnFinished` already arrived (normal completion) before `ProcessExited` is observed --
    // the redundant `ProcessExited` must not flip an already-recorded `is_error: false` to
    // `true`, or vice versa.
    let mut state = AgentSessionState::default();
    state.apply(&AgentEvent::TurnFinished {
        result_text: "done".into(),
        is_error: false,
        stop_reason: Some("end_turn".into()),
        total_cost_usd: 0.01,
        num_turns: 1,
    });
    assert!(matches!(state.status, SessionStatus::Finished { is_error: false }));

    state.apply(&AgentEvent::ProcessExited { success: false });
    assert!(
        matches!(state.status, SessionStatus::Finished { is_error: false }),
        "a redundant ProcessExited after a normal TurnFinished must not overwrite status"
    );
}
