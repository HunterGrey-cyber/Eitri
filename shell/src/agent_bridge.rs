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
    /// `resume` carries the Claude provider session id to continue. Absent for a fresh session.
    /// One message rather than two so there is exactly one path into backend construction -- a
    /// second entry point is how a "resume" would eventually acquire its own subtly different
    /// failure handling.
    StartSession {
        request_id: String,
        mode: SessionModeChoice,
        #[serde(default)]
        resume: Option<String>,
    },
    SendMessage { request_id: String, text: String },
    Interrupt { request_id: String },
    PermissionResponse {
        request_id: String,
        permission_id: String,
        decision: DecisionChoice,
        /// Only meaningful on a denial -- there is no field anywhere downstream that would show an
        /// approval's reason to the model. Carried flat rather than inside the variant because
        /// `InboundMessage` is already internally tagged on `type`.
        #[serde(default)]
        reason: Option<String>,
    },
}

/// The decision half of a `permission_response`, as a closed set rather than a bool.
///
/// Typed on the wire so an unrecognized value is a PARSE failure, not a value some later `match`
/// has to give a default to. The dangerous default here is obvious and one-directional: anything
/// that is not clearly a denial must never end up running the tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DecisionChoice {
    Allow,
    Deny,
}

impl DecisionChoice {
    /// Pairs the choice with its reason into the typed decision the backends take. An empty or
    /// whitespace-only reason becomes `None`: sending the model an empty string as its explanation
    /// is worse than sending it nothing.
    pub(crate) fn into_decision(self, reason: Option<String>) -> agent::PermissionDecision {
        match self {
            DecisionChoice::Allow => agent::PermissionDecision::Allow,
            DecisionChoice::Deny => agent::PermissionDecision::Deny {
                reason: reason.filter(|r| !r.trim().is_empty()),
            },
        }
    }
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

/// `{"kind":"hello","backend":...,"permissionModes":[...],"resumeAvailable":false,...}` -- sent once,
/// in reply to the frontend's `ready`, BEFORE any snapshot.
///
/// It exists because the frontend's start screen cannot be honest without it. The two backends
/// genuinely differ in what they can offer: the legacy backend has a real, tested interactive
/// permission gate, while the sidecar path ships BYPASS only in this milestone (its `interactive`
/// and `verdandi_rules` modes are confirmed to behave identically in the current sidecar, so
/// presenting them as distinct choices would be a lie). Hardcoding either shape into the frontend
/// would make it wrong for the other.
///
/// `resumableSession` is the whole resume gate: non-null only when the server advertised resume,
/// this client implements it, AND this workspace has a persisted provider session id. The frontend
/// renders "continue previous session" on exactly that field and nothing else, so the control
/// cannot appear for a workspace with nothing to continue.
pub(crate) fn serialize_hello_for_js(greeting: &crate::agent_backend::BackendGreeting) -> String {
    json!({
        "kind": "hello",
        "backend": greeting.kind.as_str(),
        "projectDir": greeting.project_dir.to_string_lossy(),
        "permissionModes": greeting.permission_modes,
        // The three-term intersection, already evaluated: server-advertised (last known) ∩
        // client-implemented ∩ this workspace has a persisted provider session. `null` when any
        // term fails, and the frontend renders the continue option on exactly that.
        "resumableSession": greeting.resumable.as_ref().map(|r| json!({
            "provider": r.provider,
            "providerSessionId": r.provider_session_id,
            "updatedAt": r.updated_at,
        })),
        "expectedVerdandiRevision": greeting.expected_verdandi_revision,
    })
    .to_string()
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

/// Everything one snapshot needs, gathered from wherever it actually lives.
///
/// Exists so the serializer stays a pure function of plain data: it can be unit-tested against a
/// hand-built view without constructing a real `AgentBackend`, which would mean spawning a real
/// sidecar process. `SnapshotView::of` is the one place the gathering happens, so the two can never
/// disagree about where a field comes from.
pub(crate) struct SnapshotView<'a> {
    pub(crate) backend: &'static str,
    pub(crate) conversation_id: Option<&'a str>,
    /// Verdandi's session id, from the backend rather than the projection -- the projection does not
    /// learn it until the first `SessionOpened`, which on the sidecar path is not until the first
    /// turn.
    pub(crate) session_id: Option<&'a str>,
    pub(crate) provider_session_id: Option<&'a str>,
    pub(crate) capabilities: agent::ProviderCapabilities,
    pub(crate) provider: Option<&'a agent::ProviderInfo>,
    pub(crate) projection: &'a AgentSessionProjection,
}

impl<'a> SnapshotView<'a> {
    pub(crate) fn of(backend: &'a crate::agent_backend::AgentBackend) -> Self {
        Self {
            backend: backend.kind().as_str(),
            conversation_id: backend.conversation_id(),
            session_id: backend.session_id(),
            provider_session_id: backend.provider_session_id(),
            capabilities: backend.capabilities(),
            provider: backend.provider_info(),
            projection: backend.projection(),
        }
    }
}

/// `{"kind":"snapshot","throughRevision":...,"state":<AgentUiState-shaped JSON>}` -- a
/// hand-written re-shaping of `AgentSessionProjection`'s snake_case Rust fields into the camelCase
/// shape `agent-ui/web/src/types.ts`'s `AgentUiState` expects (deliberately not a direct
/// `#[derive(Serialize)]` passthrough -- the TS and Rust naming conventions differ, and this
/// function is the one place that difference is bridged).
///
/// Takes a `SnapshotView` rather than the projection alone, because two of the three identities
/// live outside it: `conversationId` is Neovibe's own and `providerSessionId` is Claude's, while the
/// projection's `sessionId` is Verdandi's. Collapsing them would defeat the entire point of keeping
/// them apart -- and would eventually send Claude's id back as a session_id, which the sidecar
/// answers with SESSION_NOT_FOUND.
pub(crate) fn serialize_snapshot_for_js(view: &SnapshotView<'_>) -> String {
    let projection = view.projection;
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

    let capabilities = view.capabilities;
    let provider = view.provider.map(|info| {
        json!({
            "sidecarVersion": info.sidecar_version,
            "claudeAgentSdkVersion": info.claude_agent_sdk_version,
            "claudeCodeVersion": info.actual_claude_code_version,
            "protocol": format!("{}.{}", info.protocol_major, info.protocol_minor),
            "buildDescription": info.build_description,
            "startupDiagnostics": info.startup_diagnostics,
        })
    });

    let state = json!({
        "backend": view.backend,
        // Three identities, three fields. Never one.
        "conversationId": view.conversation_id,
        "sessionId": view.session_id,
        "providerSessionId": view.provider_session_id,
        "model": projection.model,
        "cwd": projection.cwd,
        "transcript": projection.transcript,
        "toolCalls": tool_calls,
        "status": status,
        "activeTurnId": projection.active_turn_id,
        "pendingPermissions": pending_permissions,
        "capabilities": {
            "resume": capabilities.resume,
            "fork": capabilities.fork,
            "interrupt": capabilities.interrupt,
            "bypassPermissionMode": capabilities.bypass_permission_mode,
        },
        "provider": provider,
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
    fn parses_an_approval_which_carries_no_reason() {
        let msg = parse_inbound_message(r#"{"type":"permission_response","request_id":"r5","permission_id":"p1","decision":"allow"}"#).unwrap();
        match msg {
            InboundMessage::PermissionResponse { permission_id, decision, reason, .. } => {
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
                    agent::PermissionDecision::Deny { reason: Some("not in this repo".into()) }
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
                assert_eq!(decision.into_decision(reason), agent::PermissionDecision::Deny { reason: None });
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
            parse_inbound_message(r#"{"type":"permission_response","request_id":"r5","permission_id":"p1","allow":true}"#).is_none(),
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
        projection.apply(&AgentDomainEvent::SessionOpened { session_id: "verdandi-1".into(), provider_session_id: "claude-1".into(), model: "claude-sonnet-5".into(), cwd: "/tmp".into() });
        projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        projection.apply(&AgentDomainEvent::ToolCallStarted { turn_id: "t1".into(), tool_use_id: "toolu_1".into(), name: "Bash".into(), input: json!({"command": "echo hi"}) });
        projection.apply(&AgentDomainEvent::ToolCallCompleted { turn_id: "t1".into(), tool_use_id: "toolu_1".into(), content: json!("boom"), is_error: true });
        projection.apply(&AgentDomainEvent::SessionUnavailable { reason: "provider process exited unexpectedly".into() });

        let provider = agent::ProviderInfo {
            sidecar_version: "0.1.0".into(),
            claude_agent_sdk_version: "0.3.0".into(),
            actual_claude_code_version: "2.1.269".into(),
            protocol_major: 1,
            protocol_minor: 0,
            advertised_capabilities: vec!["handshake".into()],
            advertised_permission_modes: vec!["bypass".into()],
            build_description: Some("Verdandi checkout: /x @ eb70aa3 (via NEOVIBE_VERDANDI_CHECKOUT)".into()),
            startup_diagnostics: vec!["claude CLI 2.1.269 is untested".into()],
        };
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: Some("conv-hash"),
            session_id: Some("verdandi-1"),
            provider_session_id: Some("claude-1"),
            capabilities: agent::ProviderCapabilities { resume: false, fork: false, interrupt: true, bypass_permission_mode: true, interactive_permission_mode: true },
            provider: Some(&provider),
            projection: &projection,
        };

        let json_str = serialize_snapshot_for_js(&view);
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["kind"], "snapshot");
        assert_eq!(parsed["throughRevision"], 5);
        assert_eq!(parsed["state"]["backend"], "sidecar");
        assert_eq!(parsed["state"]["pendingPermissions"], json!([]));
        assert_eq!(parsed["state"]["status"]["kind"], "unavailable");
        assert_eq!(parsed["state"]["status"]["reason"], "provider process exited unexpectedly");
        assert_eq!(parsed["state"]["toolCalls"][0]["toolUseId"], "toolu_1");
        assert_eq!(parsed["state"]["toolCalls"][0]["result"]["isError"], true);

        // The three identities reach the frontend as three separate fields. If any two of these
        // ever collapse to the same source, a consumer will eventually send Claude's id where
        // Verdandi's belongs and get SESSION_NOT_FOUND.
        assert_eq!(parsed["state"]["conversationId"], "conv-hash");
        assert_eq!(parsed["state"]["sessionId"], "verdandi-1");
        assert_eq!(parsed["state"]["providerSessionId"], "claude-1");

        assert_eq!(parsed["state"]["capabilities"]["interrupt"], true);
        assert_eq!(parsed["state"]["capabilities"]["resume"], false, "resume must not be advertised in this milestone");
        assert_eq!(parsed["state"]["provider"]["claudeCodeVersion"], "2.1.269");
        assert_eq!(parsed["state"]["provider"]["protocol"], "1.0");
        assert!(parsed["state"]["provider"]["buildDescription"].as_str().unwrap().contains("eb70aa3"));
        // Warnings only -- a list that is never empty cannot drive a "something is wrong" glyph.
        assert!(parsed["state"]["provider"]["startupDiagnostics"][0].as_str().unwrap().contains("untested"));
    }

    #[test]
    fn a_legacy_snapshot_carries_no_conversation_id_and_no_provider_block() {
        let projection = AgentSessionProjection::default();
        let view = SnapshotView {
            backend: "legacy",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities { resume: false, fork: false, interrupt: true, bypass_permission_mode: true, interactive_permission_mode: true },
            provider: None,
            projection: &projection,
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(&view)).unwrap();
        assert_eq!(parsed["state"]["backend"], "legacy");
        assert!(parsed["state"]["conversationId"].is_null(), "the legacy backend has no conversation identity");
        assert!(parsed["state"]["provider"].is_null());
    }

    #[test]
    fn the_sidecar_hello_carries_its_baseline_and_never_advertises_resume_without_a_record() {
        let greeting = crate::agent_backend::BackendGreeting::for_kind(
            crate::agent_backend::BackendKind::Sidecar,
            std::path::PathBuf::from("/tmp/project"),
        );
        let parsed: Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
        assert_eq!(parsed["kind"], "hello");
        assert_eq!(parsed["backend"], "sidecar");
        assert_eq!(
            parsed["permissionModes"],
            json!(crate::agent_backend::CLIENT_IMPLEMENTED_PERMISSION_MODES),
            "the hello envelope must carry the client's real offer, not a per-backend narrowing"
        );
        assert_eq!(parsed["projectDir"], "/tmp/project");
        assert!(parsed["expectedVerdandiRevision"].is_string());
    }

    #[test]
    fn hello_offers_nothing_to_continue_when_the_workspace_has_no_record() {
        let greeting = crate::agent_backend::BackendGreeting {
            kind: crate::agent_backend::BackendKind::Sidecar,
            project_dir: std::path::PathBuf::from("/tmp/project"),
            permission_modes: &["bypass"],
            expected_verdandi_revision: Some("abc1234"),
            resumable: None,
        };
        let parsed: Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
        assert!(parsed["resumableSession"].is_null());
    }

    #[test]
    fn hello_offers_the_claude_session_id_when_the_workspace_has_one() {
        // The CLAUDE id, not Verdandi's: resuming mints a new Verdandi session, so a record keyed
        // on that one would point at something that stops existing the moment it is used.
        let greeting = crate::agent_backend::BackendGreeting {
            kind: crate::agent_backend::BackendKind::Sidecar,
            project_dir: std::path::PathBuf::from("/tmp/project"),
            permission_modes: &["bypass"],
            expected_verdandi_revision: Some("abc1234"),
            resumable: Some(agent::ResumableSession {
                provider: "claude".into(),
                provider_session_id: "1857dcd5-973b-46a2".into(),
                updated_at: "1757700000000".into(),
            }),
        };
        let parsed: Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
        assert_eq!(parsed["resumableSession"]["providerSessionId"], "1857dcd5-973b-46a2");
        assert_eq!(parsed["resumableSession"]["provider"], "claude");
        assert_eq!(parsed["resumableSession"]["updatedAt"], "1757700000000");
    }

    #[test]
    fn start_session_parses_with_and_without_a_resume_id() {
        let fresh = parse_inbound_message(r#"{"type":"start_session","request_id":"r1","mode":"bypass"}"#).unwrap();
        match fresh {
            InboundMessage::StartSession { resume, .. } => assert!(resume.is_none()),
            other => panic!("expected StartSession, got {other:?}"),
        }
        let resumed = parse_inbound_message(
            r#"{"type":"start_session","request_id":"r2","mode":"bypass","resume":"claude-abc"}"#,
        )
        .unwrap();
        match resumed {
            InboundMessage::StartSession { resume, .. } => assert_eq!(resume.as_deref(), Some("claude-abc")),
            other => panic!("expected StartSession, got {other:?}"),
        }
    }

    #[test]
    fn the_legacy_hello_keeps_its_real_two_mode_choice() {
        let greeting = crate::agent_backend::BackendGreeting::for_kind(
            crate::agent_backend::BackendKind::Legacy,
            std::path::PathBuf::from("/tmp/project"),
        );
        let parsed: Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
        assert_eq!(parsed["backend"], "legacy");
        assert_eq!(parsed["permissionModes"], json!(["auto", "bypass"]));
        assert!(parsed["resumableSession"].is_null(), "the legacy backend can never offer a resume");
        assert!(parsed["expectedVerdandiRevision"].is_null());
    }
}
