//! The snapshot's camel-case shape, and the turn endings fixture.
#![cfg(test)]

use super::*;

#[test]
fn serialize_snapshot_for_js_produces_camel_case_matching_the_ts_shape() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::SessionOpened {
        session_id: "verdandi-1".into(),
        provider_session_id: "claude-1".into(),
        model: "claude-sonnet-5".into(),
        cwd: "/tmp".into(),
    });
    projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    projection.apply(&AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_1".into(),
        name: "Bash".into(),
        input: json!({"command": "echo hi"}),
    });
    projection.apply(&AgentDomainEvent::ToolCallCompleted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_1".into(),
        content: json!("boom"),
        is_error: true,
    });
    projection.apply(&AgentDomainEvent::SessionUnavailable {
        reason: "provider process exited unexpectedly".into(),
    });

    let provider = agent::ProviderInfo {
        host: None,
        account_name: None,
        account_config_dir: None,
        sidecar_version: "0.1.0".into(),
        claude_agent_sdk_version: "0.3.0".into(),
        actual_claude_code_version: "2.1.269".into(),
        protocol_major: 2,
        protocol_minor: 0,
        advertised_capabilities: vec!["handshake".into()],
        advertised_permission_modes: vec!["bypass".into()],
        event_buffer_policy: "bounded-1000".into(),
        build_description: Some("Verdandi checkout: /x @ eb70aa3 (via EITRI_VERDANDI_CHECKOUT)".into()),
        startup_diagnostics: vec!["claude CLI 2.1.269 is untested".into()],
        configuration_home: None,
        sidecar_update: None,
    };
    let view = SnapshotView {
        backend: "sidecar",
        conversation_id: Some("conv-hash"),
        session_id: Some("verdandi-1"),
        provider_session_id: Some("claude-1".to_string()),
        capabilities: agent::ProviderCapabilities {
            resume: false,
            fork: false,
            interrupt: true,
            bypass_permission_mode: true,
            interactive_permission_mode: true,
            cli_auto_mode: false,
            steer: false,
            ..Default::default()
        },
        provider: Some(&provider),
        projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
        hidden_pending: None,
    };

    let json_str = serialize_snapshot_for_js(TabId(1), &view, None);
    let parsed: Value = serde_json::from_str(&json_str).unwrap();
    assert_eq!(parsed["kind"], "snapshot");
    assert_eq!(parsed["tab"], 1);
    assert_eq!(parsed["throughRevision"], 5);
    assert_eq!(parsed["state"]["backend"], "sidecar");
    assert_eq!(parsed["state"]["pendingPermissions"], json!([]));
    assert_eq!(parsed["state"]["status"]["kind"], "unavailable");
    assert_eq!(
        parsed["state"]["status"]["reason"],
        "provider process exited unexpectedly"
    );
    assert_eq!(parsed["state"]["toolCalls"][0]["toolUseId"], "toolu_1");
    assert_eq!(parsed["state"]["toolCalls"][0]["result"]["isError"], true);
    // The turn it started in, so a panel rehydrated from this snapshot can tell a call of the
    // active turn from one its ended turn abandoned, as the live reducer can (sw-panel-render-6).
    assert_eq!(parsed["state"]["toolCalls"][0]["turnId"], "t1");

    // The three identities reach the frontend as three separate fields. If any two of these
    // ever collapse to the same source, a consumer will eventually send Claude's id where
    // Verdandi's belongs and get SESSION_NOT_FOUND.
    assert_eq!(parsed["state"]["conversationId"], "conv-hash");
    assert_eq!(parsed["state"]["sessionId"], "verdandi-1");
    assert_eq!(parsed["state"]["providerSessionId"], "claude-1");

    assert_eq!(parsed["state"]["capabilities"]["interrupt"], true);
    assert_eq!(
        parsed["state"]["capabilities"]["resume"], false,
        "resume must not be advertised in this milestone"
    );
    assert_eq!(parsed["state"]["provider"]["claudeCodeVersion"], "2.1.269");
    assert_eq!(parsed["state"]["provider"]["protocol"], "2.0");
    assert!(parsed["state"]["provider"]["buildDescription"]
        .as_str()
        .unwrap()
        .contains("eb70aa3"));
    // Warnings only -- a list that is never empty cannot drive a "something is wrong" glyph.
    assert!(parsed["state"]["provider"]["startupDiagnostics"][0]
        .as_str()
        .unwrap()
        .contains("untested"));
}

/// `agent-ui/web/src/fixtures/turn-endings.json`: for each way a turn can end other than by
/// completing, the events payload this side sends and the snapshot it sends after folding the
/// same events, which the panel's tests render. Compared on every run; `EITRI_WRITE_FIXTURES=1`
/// rewrites it.
#[test]
fn the_turn_endings_fixture_is_what_the_serializer_sends() {
    use agent::{TurnEndDetail, TurnOutcome};
    // Not recordings: only the 401 shape was ever recorded from the real CLI. Each case is the
    // shape the SDK's result types and the CLI's own classification of its terminal reasons
    // allow.
    let ended = |outcome: TurnOutcome,
                 is_error: bool,
                 result_text: &str,
                 reason: Option<&str>,
                 status: Option<u32>,
                 errors: &[&str]| {
        let errors: Vec<String> = errors.iter().map(ToString::to_string).collect();
        AgentDomainEvent::TurnCompleted {
            turn_id: "t1".into(),
            outcome,
            result_text: result_text.into(),
            stop_reason: None,
            usage: None,
            detail: TurnEndDetail::from_result(
                outcome,
                is_error,
                result_text,
                reason.map(str::to_string),
                status,
                &errors,
            ),
        }
    };
    let limits = |resets_at: i64| AgentDomainEvent::AccountLimits {
        used_percent: 100.0,
        window_minutes: Some(300),
        resets_at: Some(resets_at),
        plan: Some("pro".into()),
    };
    let cases: Vec<(&str, Vec<AgentDomainEvent>)> = vec![
        (
            "api_error",
            vec![ended(
                TurnOutcome::Failed,
                true,
                "API Error: 529 {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}",
                Some("api_error"),
                Some(529),
                &[],
            )],
        ),
        (
            "rate_limited",
            vec![ended(
                TurnOutcome::Failed,
                true,
                "API Error: 429 rate_limit_error",
                Some("api_error"),
                Some(429),
                &[],
            )],
        ),
        (
            "context_full",
            vec![ended(
                TurnOutcome::LimitReached,
                true,
                "Prompt is too long",
                Some("blocking_limit"),
                None,
                &[],
            )],
        ),
        (
            "max_turns",
            vec![ended(
                TurnOutcome::LimitReached,
                true,
                "",
                Some("max_turns"),
                None,
                &["Reached maximum number of turns (3)"],
            )],
        ),
        (
            "hook_stopped",
            vec![ended(
                TurnOutcome::Failed,
                false,
                "I stopped where the hook asked me to.",
                Some("hook_stopped"),
                None,
                &[],
            )],
        ),
        (
            "interrupt",
            vec![ended(
                TurnOutcome::Interrupted,
                true,
                "",
                Some("aborted_streaming"),
                None,
                &["[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=null"],
            )],
        ),
        (
            "cli_crash_closed",
            vec![
                ended(TurnOutcome::Failed, true, "", None, None, &[]),
                AgentDomainEvent::SessionClosed {
                    reason: "provider_failed".into(),
                },
            ],
        ),
        (
            "stream_lost",
            vec![AgentDomainEvent::SessionUnavailable {
                reason: "the sidecar's event stream ended without closing the session".into(),
            }],
        ),
        // An agent that reports its account's limits (Codex): the row keeps the reset time it
        // ended with, whatever a later report says.
        (
            "usage_limit",
            vec![
                limits(1_900_000_000),
                ended(
                    TurnOutcome::LimitReached,
                    true,
                    "You've hit your usage limit.",
                    Some("usage_limit"),
                    None,
                    &[],
                ),
                limits(1_900_086_400),
            ],
        ),
        // The report that explains the stop came just after it.
        (
            "usage_limit_reported_after",
            vec![
                ended(
                    TurnOutcome::LimitReached,
                    true,
                    "You've hit your usage limit.",
                    Some("usage_limit"),
                    None,
                    &[],
                ),
                limits(1_900_000_000),
            ],
        ),
    ];

    let mut written_cases = Vec::new();
    for (name, ending) in cases {
        let mut events = vec![
            AgentDomainEvent::SessionOpened {
                session_id: "sess-1".into(),
                provider_session_id: "prov-1".into(),
                model: "claude-sonnet-5".into(),
                cwd: "/project".into(),
            },
            AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
            AgentDomainEvent::UserPromptSubmitted {
                text: "fix the parser".into(),
            },
            AgentDomainEvent::ContentDelta {
                turn_id: "t1".into(),
                kind: agent::ContentKind::Text,
                text: "Looking at the parser".into(),
            },
        ];
        events.extend(ending);
        let mut projection = AgentSessionProjection::default();
        for event in &events {
            projection.apply(event);
        }
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: Some("sess-1"),
            provider_session_id: Some("prov-1".into()),
            capabilities: agent::ProviderCapabilities {
                resume: true,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
                cli_auto_mode: false,
                steer: false,
                ..Default::default()
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
            hidden_pending: None,
        };
        let events_payload = serialize_events_for_js(TabId(1), 0, projection.last_revision, &events);
        let snapshot_payload = serialize_snapshot_for_js(TabId(1), &view, None);
        written_cases.push(json!({
            "name": name,
            "events": serde_json::from_str::<Value>(&events_payload).unwrap(),
            "snapshot": serde_json::from_str::<Value>(&snapshot_payload).unwrap(),
        }));
    }
    let fixture = json!({
        "writtenBy": "core/src/agent_bridge.rs, tests::the_turn_endings_fixture_is_what_the_serializer_sends \
                      -- compared on every run; EITRI_WRITE_FIXTURES=1 rewrites it",
        "cases": written_cases,
    });
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/turn-endings.json");
    let written = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
    let committed = std::fs::read_to_string(&path).ok();
    if committed.as_deref() == Some(written.as_str()) {
        return;
    }
    if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
        std::fs::write(&path, &written).expect("write the turn endings fixture");
    } else {
        assert_eq!(
            committed.as_deref(),
            Some(written.as_str()),
            "the committed turn endings fixture is not what this test produces; if the change is \
             intended, regenerate it with EITRI_WRITE_FIXTURES=1"
        );
    }
}
