use agent::{AgentEvent, AgentSessionState, PermissionSource, SessionStatus};
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
    // In v2 the session id is the CLI's own -- it generates one per process and reports it in its
    // `system`/`init` line, which is where `AgentEvent::SessionStarted` comes from. Nothing in
    // this crate assigns or requests a session id anymore (v1's caller-assigned
    // `--session-id`/`--resume` path, and the `AgentSession::start_new`/`SpawnMode` API around it,
    // were deleted with the v2 rewrite). A uuid-shaped id is used here purely because that's the
    // real shape the CLI emits; the event is hand-built, without spawning a real process, to keep
    // this a pure reducer test.
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
fn turn_finished_no_longer_ends_the_whole_session_only_clears_turn_in_progress() {
    // v2 semantic change from v1: TurnFinished means "this turn is over", NOT "the conversation
    // is over" -- the process (and session) keeps running for more turns.
    let mut state = AgentSessionState::default();
    state.apply(&AgentEvent::SessionStarted {
        session_id: "abc".into(),
        model: "claude-sonnet-5".into(),
        cwd: "/tmp".into(),
    });
    state.turn_in_progress = true; // set by AgentSession::send_turn in real usage
    state.apply(&AgentEvent::TurnFinished {
        result_text: "pong".into(),
        is_error: false,
        stop_reason: Some("end_turn".into()),
        total_cost_usd: 0.01,
        num_turns: 1,
    });
    assert!(!state.turn_in_progress);
    assert!(
        matches!(state.status, SessionStatus::Running),
        "session must still be Running after one turn finishes, got {:?}",
        state.status
    );
}

#[test]
fn interrupted_turn_result_clears_turn_in_progress_without_marking_session_finished() {
    let mut state = AgentSessionState::default();
    state.apply(&AgentEvent::SessionStarted {
        session_id: "abc".into(),
        model: "claude-sonnet-5".into(),
        cwd: "/tmp".into(),
    });
    state.turn_in_progress = true;
    state.apply(&AgentEvent::TurnFinished {
        result_text: String::new(),
        is_error: true,
        stop_reason: None,
        total_cost_usd: 0.0,
        num_turns: 3,
    });
    assert!(!state.turn_in_progress);
    assert!(
        matches!(state.status, SessionStatus::Running),
        "an interrupted turn must not end the conversation"
    );
}

#[test]
fn process_exited_is_now_the_only_thing_that_ends_the_session() {
    let mut state = AgentSessionState::default();
    state.apply(&AgentEvent::SessionStarted {
        session_id: "abc".into(),
        model: "claude-sonnet-5".into(),
        cwd: "/tmp".into(),
    });
    state.apply(&AgentEvent::ProcessExited { success: true });
    assert!(matches!(state.status, SessionStatus::Finished { is_error: false }));
}

#[test]
fn permission_request_populates_pending_permissions() {
    let mut state = AgentSessionState::default();
    state.apply(&AgentEvent::PermissionRequest {
        request_id: "r1".into(),
        tool_name: "Bash".into(),
        input: json!({"command": "echo hi"}),
        source: PermissionSource::HookRelay,
    });
    assert_eq!(state.pending_permissions.len(), 1);
    let pending = state.find_pending_permission("r1").expect("pending permission should be recorded");
    assert_eq!(pending.request_id, "r1");
    assert_eq!(pending.tool_name, "Bash");
    assert_eq!(pending.source, PermissionSource::HookRelay);
}

/// The regression test for the real stranding bug a final review found: `pending_permission` used
/// to be a single `Option`, so a second concurrent `PermissionRequest` (entirely possible -- one
/// assistant message can carry several `tool_use` blocks, and the generated hook matcher is `"*"`)
/// silently overwrote the first. The first request's live `agent-hook` connection stayed open with
/// nothing in the public API able to answer it, so it blocked for the CLI's full 600s hook
/// timeout. Proves both requests survive, in arrival order, and that answering either one (the
/// removal half of `AgentSession::respond_permission`) leaves the other exactly as answerable as
/// before -- including when they're answered out of order.
#[test]
fn two_concurrent_permission_requests_are_both_retained_and_independently_answerable() {
    let mut state = AgentSessionState::default();
    state.apply(&AgentEvent::PermissionRequest {
        request_id: "toolu_first".into(),
        tool_name: "Bash".into(),
        input: json!({"command": "echo one"}),
        source: PermissionSource::HookRelay,
    });
    state.apply(&AgentEvent::PermissionRequest {
        request_id: "toolu_second".into(),
        tool_name: "Read".into(),
        input: json!({"file_path": "/tmp/x"}),
        source: PermissionSource::CanUseTool,
    });

    assert_eq!(state.pending_permissions.len(), 2, "a second request must not overwrite the first");
    assert_eq!(state.pending_permissions[0].request_id, "toolu_first", "arrival order is preserved");
    assert_eq!(state.pending_permissions[1].request_id, "toolu_second");
    // Each request keeps its OWN source -- that's what routes its answer to the right channel.
    assert_eq!(state.find_pending_permission("toolu_first").unwrap().source, PermissionSource::HookRelay);
    assert_eq!(state.find_pending_permission("toolu_second").unwrap().source, PermissionSource::CanUseTool);

    // Answer the SECOND one first (out of order, the interesting case).
    let answered = state.take_pending_permission("toolu_second").expect("second must be answerable");
    assert_eq!(answered.tool_name, "Read");
    assert_eq!(state.pending_permissions.len(), 1);
    assert!(state.find_pending_permission("toolu_second").is_none(), "an answered request is removed");

    // The first must still be there, untouched, and still answerable.
    let still_pending = state.find_pending_permission("toolu_first").expect("first must NOT have been stranded");
    assert_eq!(still_pending.tool_name, "Bash");
    let answered = state.take_pending_permission("toolu_first").expect("first must be answerable afterwards");
    assert_eq!(answered.request_id, "toolu_first");
    assert!(state.pending_permissions.is_empty());
}

#[test]
fn taking_an_unknown_or_already_answered_permission_request_returns_none() {
    let mut state = AgentSessionState::default();
    state.apply(&AgentEvent::PermissionRequest {
        request_id: "r1".into(),
        tool_name: "Bash".into(),
        input: json!({}),
        source: PermissionSource::HookRelay,
    });
    assert!(state.take_pending_permission("no-such-request").is_none());
    assert_eq!(state.pending_permissions.len(), 1, "a miss must not disturb what is pending");
    assert!(state.take_pending_permission("r1").is_some());
    assert!(state.take_pending_permission("r1").is_none(), "answering twice must not find it again");
}

#[test]
fn control_response_has_no_state_effect_including_on_pending_permissions() {
    // A control_response never removes a pending permission request (see
    // AgentSession::respond_permission in session.rs, which is the ONLY thing that removes one) --
    // an incoming ControlResponse only ever acknowledges a control_request THIS crate itself
    // initiated, never the CLI's own can_use_tool request we answered.
    let mut state = AgentSessionState::default();
    state.apply(&AgentEvent::PermissionRequest {
        request_id: "r1".into(),
        tool_name: "Bash".into(),
        input: json!({"command": "echo hi"}),
        source: PermissionSource::CanUseTool,
    });
    state.apply(&AgentEvent::ControlResponse {
        request_id: "r1".into(),
        subtype: "success".into(),
        raw: json!({}),
    });
    assert!(
        state.find_pending_permission("r1").is_some(),
        "ControlResponse must not remove a pending permission request by itself"
    );
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
fn process_exited_is_a_no_op_once_already_finished_by_a_prior_process_exited() {
    // v2: TurnFinished no longer ends the session (see
    // `turn_finished_no_longer_ends_the_whole_session_only_clears_turn_in_progress` above) --
    // ProcessExited is now the only thing that sets `Finished`. This protects the same invariant
    // the old (v1) version of this test covered via TurnFinished: a redundant second
    // `ProcessExited` must not flip an already-recorded `is_error: false` to `true`, or vice
    // versa.
    let mut state = AgentSessionState::default();
    state.apply(&AgentEvent::SessionStarted {
        session_id: "abc-123".into(),
        model: "claude-sonnet-5".into(),
        cwd: "/tmp".into(),
    });
    state.apply(&AgentEvent::ProcessExited { success: true });
    assert!(matches!(state.status, SessionStatus::Finished { is_error: false }));

    state.apply(&AgentEvent::ProcessExited { success: false });
    assert!(
        matches!(state.status, SessionStatus::Finished { is_error: false }),
        "a redundant ProcessExited after the session is already Finished must not overwrite status"
    );
}
