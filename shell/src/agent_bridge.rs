//! JSON message types and (de)serialization for the Rust<->JS bridge between `shell` and
//! `agent-ui/web`'s embedded frontend. `InboundMessage` is what the WebView's
//! `neovibeAgent.postMessage` calls deserialize into; `serialize_event_for_js`/
//! `serialize_snapshot_for_js` are what get passed to `evaluate_javascript`'s
//! `window.__neovibeDispatch` call. See agent-ui/web/src/types.ts for the exact TS-side shapes
//! these must match.

use agent::{AgentEvent, AgentSessionState, PermissionMode};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionModeChoice {
    Auto,
    Bypass,
}

impl From<SessionModeChoice> for PermissionMode {
    fn from(choice: SessionModeChoice) -> Self {
        match choice {
            SessionModeChoice::Auto => PermissionMode::Auto,
            SessionModeChoice::Bypass => PermissionMode::Bypass,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum InboundMessage {
    Ready,
    StartSession { mode: SessionModeChoice },
    SendMessage { text: String },
    Interrupt,
    PermissionResponse { request_id: String, allow: bool, reason: Option<String> },
}

/// Parses one JS->Rust message. Never panics -- an unrecognized `type` or malformed JSON
/// becomes `None`, and the caller (agent_panel.rs, Task 7) logs it rather than crashing the
/// whole shell process over one bad message from the WebView.
pub(crate) fn parse_inbound_message(json_str: &str) -> Option<InboundMessage> {
    match serde_json::from_str(json_str) {
        Ok(msg) => Some(msg),
        Err(e) => {
            eprintln!("[agent_bridge] failed to parse inbound message: {e} -- raw: {json_str}");
            None
        }
    }
}

/// `{"kind":"event","event":<tagged AgentEvent JSON>}` -- `AgentEvent`'s own `#[derive(Serialize)]`
/// (Step 1) produces the tagged shape directly; this just wraps it in the `kind` envelope
/// `agent-ui/web/src/bridge.ts`'s `installDispatch` demuxes on.
pub(crate) fn serialize_event_for_js(event: &AgentEvent) -> String {
    json!({ "kind": "event", "event": event }).to_string()
}

/// `{"kind":"snapshot","snapshot":<AgentUiState-shaped JSON>}` -- a hand-written re-shaping of
/// `AgentSessionState`'s snake_case Rust fields into the camelCase shape
/// `agent-ui/web/src/types.ts`'s `AgentUiState` expects (deliberately not a direct
/// `#[derive(Serialize)]` passthrough -- the TS and Rust naming conventions differ, and this
/// function is the one place that difference is bridged).
pub(crate) fn serialize_snapshot_for_js(state: &AgentSessionState) -> String {
    let tool_calls: Vec<Value> = state
        .tool_calls
        .iter()
        .map(|call| {
            json!({
                "id": call.id,
                "name": call.name,
                "input": call.input,
                "result": call.result.as_ref().map(|(content, is_error)| json!({ "content": content, "isError": is_error })),
            })
        })
        .collect();

    let pending_permissions: Vec<Value> = state
        .pending_permissions
        .iter()
        .map(|p| {
            json!({
                "requestId": p.request_id,
                "toolName": p.tool_name,
                "input": p.input,
                "source": p.source,
            })
        })
        .collect();

    // Hand-reshaped, same as tool_calls/pending_permissions above -- SessionStatus has no
    // Serialize impl at all (none is needed anywhere else), specifically so there's no
    // temptation to pass it through directly and accidentally ship its field as snake_case
    // `is_error` where the TS side's AgentUiState expects camelCase `isError`.
    let status = match &state.status {
        agent::SessionStatus::Starting => json!({ "kind": "starting" }),
        agent::SessionStatus::Running => json!({ "kind": "running" }),
        agent::SessionStatus::Finished { is_error } => json!({ "kind": "finished", "isError": is_error }),
    };

    let snapshot = json!({
        "sessionId": state.session_id,
        "model": state.model,
        "cwd": state.cwd,
        "transcript": state.transcript,
        "toolCalls": tool_calls,
        "status": status,
        "turnInProgress": state.turn_in_progress,
        "pendingPermissions": pending_permissions,
    });

    json!({ "kind": "snapshot", "snapshot": snapshot }).to_string()
}

/// `{"kind":"error","message":<message>}` -- a third bridge envelope kind alongside
/// `event`/`snapshot`, for a Rust-side failure that the frontend cannot otherwise detect (e.g.
/// `AgentSession::start` failing because `claude` isn't on `PATH`). `App.tsx`'s `sessionStarted`
/// flag is set optimistically the moment the mode button is clicked, before Rust ever confirms
/// the session actually started -- without this envelope a failed start left the frontend
/// stranded on an empty conversation view with no session and no way back.
pub(crate) fn serialize_error_for_js(message: &str) -> String {
    json!({ "kind": "error", "message": message }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ready() {
        let msg = parse_inbound_message(r#"{"type":"ready"}"#).unwrap();
        assert!(matches!(msg, InboundMessage::Ready));
    }

    #[test]
    fn parses_start_session_with_auto_mode() {
        let msg = parse_inbound_message(r#"{"type":"start_session","mode":"auto"}"#).unwrap();
        assert!(matches!(msg, InboundMessage::StartSession { mode: SessionModeChoice::Auto }));
    }

    #[test]
    fn parses_send_message() {
        let msg = parse_inbound_message(r#"{"type":"send_message","text":"hello"}"#).unwrap();
        match msg {
            InboundMessage::SendMessage { text } => assert_eq!(text, "hello"),
            other => panic!("expected SendMessage, got {other:?}"),
        }
    }

    #[test]
    fn parses_interrupt() {
        let msg = parse_inbound_message(r#"{"type":"interrupt"}"#).unwrap();
        assert!(matches!(msg, InboundMessage::Interrupt));
    }

    #[test]
    fn parses_start_session_with_bypass_mode_and_converts_to_the_right_permission_mode() {
        let msg = parse_inbound_message(r#"{"type":"start_session","mode":"bypass"}"#).unwrap();
        match msg {
            InboundMessage::StartSession { mode } => {
                assert!(matches!(mode, SessionModeChoice::Bypass));
                assert!(matches!(PermissionMode::from(mode), PermissionMode::Bypass));
            }
            other => panic!("expected StartSession, got {other:?}"),
        }
    }

    #[test]
    fn parses_permission_response_with_no_reason() {
        let msg = parse_inbound_message(r#"{"type":"permission_response","request_id":"r1","allow":true}"#).unwrap();
        match msg {
            InboundMessage::PermissionResponse { request_id, allow, reason } => {
                assert_eq!(request_id, "r1");
                assert!(allow);
                assert!(reason.is_none());
            }
            other => panic!("expected PermissionResponse, got {other:?}"),
        }
    }

    #[test]
    fn unparseable_json_returns_none_not_a_panic() {
        assert!(parse_inbound_message("not json at all {{{").is_none());
    }

    #[test]
    fn unrecognized_type_returns_none_not_a_panic() {
        assert!(parse_inbound_message(r#"{"type":"some_future_message_type"}"#).is_none());
    }

    #[test]
    fn serialize_event_for_js_wraps_in_kind_event_envelope() {
        let event = AgentEvent::AssistantText { text: "hello".into() };
        let json_str = serialize_event_for_js(&event);
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["kind"], "event");
        assert_eq!(parsed["event"]["type"], "assistant_text");
        assert_eq!(parsed["event"]["text"], "hello");
    }

    #[test]
    fn serialize_error_for_js_wraps_in_kind_error_envelope() {
        let json_str = serialize_error_for_js("failed to start session: no such file or directory");
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["kind"], "error");
        assert_eq!(parsed["message"], "failed to start session: no such file or directory");
    }

    #[test]
    fn serialize_snapshot_for_js_produces_camel_case_matching_the_ts_shape() {
        let mut state = AgentSessionState::default();
        state.apply(&AgentEvent::SessionStarted {
            session_id: "abc".into(), model: "claude-sonnet-5".into(), cwd: "/tmp".into(),
        });
        // Drive a real tool call to completion AND the session to Finished{is_error:true} --
        // both are the historically-risky isError sites (a naive derive on SessionStatus, or a
        // reintroduced snake_case field, would silently ship `is_error` instead of `isError` in
        // exactly these two spots without failing any test that only checks the empty/Running
        // case).
        state.apply(&AgentEvent::ToolStarted {
            id: "toolu_1".into(), name: "Bash".into(), input: json!({"command": "echo hi"}),
        });
        state.apply(&AgentEvent::ToolResult {
            id: "toolu_1".into(), content: json!("boom"), is_error: true,
        });
        state.apply(&AgentEvent::ProcessExited { success: false });

        let json_str = serialize_snapshot_for_js(&state);
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["kind"], "snapshot");
        assert_eq!(parsed["snapshot"]["sessionId"], "abc");
        assert_eq!(parsed["snapshot"]["turnInProgress"], false);
        assert_eq!(parsed["snapshot"]["pendingPermissions"], json!([]));
        assert_eq!(parsed["snapshot"]["status"]["kind"], "finished");
        assert_eq!(parsed["snapshot"]["status"]["isError"], true);
        assert_eq!(parsed["snapshot"]["toolCalls"][0]["id"], "toolu_1");
        assert_eq!(parsed["snapshot"]["toolCalls"][0]["result"]["isError"], true);
    }
}
