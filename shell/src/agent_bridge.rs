//! JSON message types and (de)serialization for the Rust<->JS bridge between `shell` and
//! `agent-ui/web`'s embedded frontend, v2 protocol
//! (docs/superpowers/specs/2026-09-09-claude-runtime-provider-refactor-design.md §11): every
//! JS->Rust command carries a `requestId`; Rust replies with exactly one `command_result` per
//! command, and pushes a revisioned `events`/`snapshot` envelope independently of any specific
//! command. See agent-ui/web/src/types.ts for the exact TS-side shapes these must match.

use agent::{AgentDomainEvent, AgentSessionProjection, PermissionMode};
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
    Ready { request_id: String },
    StartSession { request_id: String, mode: SessionModeChoice },
    SendMessage { request_id: String, text: String },
    Interrupt { request_id: String },
    PermissionResponse { request_id: String, permission_id: String, allow: bool, reason: Option<String> },
}

impl InboundMessage {
    pub(crate) fn request_id(&self) -> &str {
        match self {
            InboundMessage::Ready { request_id }
            | InboundMessage::StartSession { request_id, .. }
            | InboundMessage::SendMessage { request_id, .. }
            | InboundMessage::Interrupt { request_id }
            | InboundMessage::PermissionResponse { request_id, .. } => request_id,
        }
    }
}

/// Parses one JS->Rust message. Never panics -- an unrecognized `type` or malformed JSON becomes
/// `None`, and the caller (`agent_panel.rs`) logs it rather than crashing the whole shell process
/// over one bad message from the WebView. A message that fails to parse has no known `requestId`
/// to reply to, so no `command_result` is possible for it -- this mirrors the pre-v2 behavior
/// exactly (an unparseable message was already silently logged-and-dropped, not surfaced to the
/// frontend).
pub(crate) fn parse_inbound_message(json_str: &str) -> Option<InboundMessage> {
    match serde_json::from_str(json_str) {
        Ok(msg) => Some(msg),
        Err(e) => {
            eprintln!("[agent_bridge] failed to parse inbound message: {e} -- raw: {json_str}");
            None
        }
    }
}

/// `{"kind":"command_result","requestId":...,"ok":true}` or
/// `{"kind":"command_result","requestId":...,"ok":false,"error":"..."}`.
pub(crate) fn serialize_command_result_for_js(request_id: &str, result: Result<(), &str>) -> String {
    match result {
        Ok(()) => json!({ "kind": "command_result", "requestId": request_id, "ok": true }).to_string(),
        Err(error) => json!({ "kind": "command_result", "requestId": request_id, "ok": false, "error": error }).to_string(),
    }
}

/// `{"kind":"events","fromRevision":...,"throughRevision":...,"events":[<tagged AgentDomainEvent JSON>, ...]}`.
/// `AgentDomainEvent`'s own `#[derive(Serialize)]` produces the tagged shape directly for each
/// element. `from_revision` is the projection's `last_revision` BEFORE this batch was folded;
/// `through_revision` is `last_revision` AFTER. Both are currently informational only -- this
/// phase implements no gap detection, no resync request, and no `RequestSnapshot`-style message
/// type; the frontend never reads `fromRevision`/`throughRevision` at all. The one real reload
/// path that exists today (`shell/MANUAL_VERIFICATION.md`'s "agent-ui verification, protocol v2"
/// section, check 4) works by re-loading the panel's document from scratch and consuming a fresh
/// `snapshot`, not by asking for events from a prior revision -- a real revisioned resync
/// consumer, if one is ever built, is future work, not something already wired up here.
pub(crate) fn serialize_events_for_js(from_revision: u64, through_revision: u64, events: &[AgentDomainEvent]) -> String {
    json!({ "kind": "events", "fromRevision": from_revision, "throughRevision": through_revision, "events": events }).to_string()
}

/// `{"kind":"snapshot","throughRevision":...,"state":<AgentUiState-shaped JSON>}` -- a
/// hand-written re-shaping of `AgentSessionProjection`'s snake_case Rust fields into the camelCase
/// shape `agent-ui/web/src/types.ts`'s `AgentUiState` expects (deliberately not a direct
/// `#[derive(Serialize)]` passthrough -- the TS and Rust naming conventions differ, and this
/// function is the one place that difference is bridged).
pub(crate) fn serialize_snapshot_for_js(projection: &AgentSessionProjection) -> String {
    let tool_calls: Vec<Value> = projection
        .tool_calls
        .iter()
        .map(|call| {
            json!({
                "toolUseId": call.tool_use_id,
                "name": call.name,
                "input": call.input,
                "result": call.result.as_ref().map(|r| json!({ "content": r.content, "isError": r.is_error })),
            })
        })
        .collect();

    let pending_permissions: Vec<Value> = projection
        .pending_permissions
        .values()
        .map(|p| json!({ "permissionId": p.permission_id, "toolName": p.tool_name, "input": p.input }))
        .collect();

    let status = match &projection.status {
        agent::ProjectionStatus::Starting => json!({ "kind": "starting" }),
        agent::ProjectionStatus::Running => json!({ "kind": "running" }),
        agent::ProjectionStatus::Unavailable { reason } => json!({ "kind": "unavailable", "reason": reason }),
        agent::ProjectionStatus::Closed { reason } => json!({ "kind": "closed", "reason": reason }),
    };

    let state = json!({
        "sessionId": projection.session_id,
        "model": projection.model,
        "cwd": projection.cwd,
        "transcript": projection.transcript,
        "toolCalls": tool_calls,
        "status": status,
        "activeTurnId": projection.active_turn_id,
        "pendingPermissions": pending_permissions,
    });

    json!({ "kind": "snapshot", "throughRevision": projection.last_revision, "state": state }).to_string()
}

/// `{"kind":"error","message":<message>}` -- a fatal, session-ending failure the frontend cannot
/// otherwise detect (e.g. `AgentSession::start` failing because `claude` isn't on `PATH`).
/// Distinct from `command_result`'s `ok:false` (which reports one command's own failure without
/// ending the session) -- this envelope means the whole `AgentSession` is gone.
pub(crate) fn serialize_error_for_js(message: &str) -> String {
    json!({ "kind": "error", "message": message }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::PermissionOutcome;

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
    fn parses_start_session_with_auto_mode() {
        let msg = parse_inbound_message(r#"{"type":"start_session","request_id":"r2","mode":"auto"}"#).unwrap();
        assert!(matches!(msg, InboundMessage::StartSession { mode: SessionModeChoice::Auto, .. }));
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
    fn parses_permission_response_with_no_reason() {
        let msg = parse_inbound_message(r#"{"type":"permission_response","request_id":"r5","permission_id":"p1","allow":true}"#).unwrap();
        match msg {
            InboundMessage::PermissionResponse { permission_id, allow, reason, .. } => {
                assert_eq!(permission_id, "p1");
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
        let events = vec![AgentDomainEvent::PermissionResolved { permission_id: "p1".into(), outcome: PermissionOutcome::Allowed }];
        let json_str = serialize_events_for_js(3, 4, &events);
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["kind"], "events");
        assert_eq!(parsed["fromRevision"], 3);
        assert_eq!(parsed["throughRevision"], 4);
        assert_eq!(parsed["events"][0]["type"], "permission_resolved");
        assert_eq!(parsed["events"][0]["permission_id"], "p1");
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
        let mut projection = AgentSessionProjection::default();
        projection.apply(&AgentDomainEvent::SessionOpened { session_id: "abc".into(), model: "claude-sonnet-5".into(), cwd: "/tmp".into() });
        projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        projection.apply(&AgentDomainEvent::ToolCallStarted { turn_id: "t1".into(), tool_use_id: "toolu_1".into(), name: "Bash".into(), input: json!({"command": "echo hi"}) });
        projection.apply(&AgentDomainEvent::ToolCallCompleted { turn_id: "t1".into(), tool_use_id: "toolu_1".into(), content: json!("boom"), is_error: true });
        projection.apply(&AgentDomainEvent::SessionUnavailable { reason: "provider process exited unexpectedly".into() });

        let json_str = serialize_snapshot_for_js(&projection);
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["kind"], "snapshot");
        assert_eq!(parsed["throughRevision"], 5);
        assert_eq!(parsed["state"]["sessionId"], "abc");
        assert_eq!(parsed["state"]["pendingPermissions"], json!([]));
        assert_eq!(parsed["state"]["status"]["kind"], "unavailable");
        assert_eq!(parsed["state"]["status"]["reason"], "provider process exited unexpectedly");
        assert_eq!(parsed["state"]["toolCalls"][0]["toolUseId"], "toolu_1");
        assert_eq!(parsed["state"]["toolCalls"][0]["result"]["isError"], true);
    }
}
