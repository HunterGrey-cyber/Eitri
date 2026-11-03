//! The confirm_bypass envelope, turn_rendered, the hello and its session lists, and the hand-off to the terminal.
#![cfg(test)]

use super::*;

/// The bypass-confirmation wire contract must agree in both directions.
#[test]
fn the_confirm_bypass_envelope_matches_the_wire_contract() {
    let tab_plan = crate::tab_set::BypassPlan {
        scope: crate::tab_set::BypassScope::Tab(TabId(3)),
        nonce: 17,
        approve: vec!["p1".to_string(), "p2".to_string()],
        // The strings `TabSet` really sends, not a paraphrase that can drift from them.
        lines: vec![crate::tabs::bypass_prompt(crate::tabs::PromptScope::LiveTab, 2)],
        prompt: crate::tabs::PromptScope::LiveTab,
        page: crate::tabs::ViewId::BASE,
    };
    assert_eq!(
        serde_json::from_str::<Value>(&serialize_confirm_bypass_for_js(&tab_plan)).unwrap(),
        json!({
            "kind": "confirm_bypass",
            "tab": 3,
            "scope": "tab",
            "nonce": 17,
            "lines": ["Switch to bypass and approve the 2 waiting cards? (y/n)"],
        }),
        "approve must never reach the wire"
    );

    let default_plan = crate::tab_set::BypassPlan {
        scope: crate::tab_set::BypassScope::Default,
        nonce: 18,
        approve: Vec::new(),
        lines: vec![crate::tabs::bypass_prompt(crate::tabs::PromptScope::Default, 0)],
        prompt: crate::tabs::PromptScope::Default,
        page: crate::tabs::ViewId::BASE,
    };
    assert_eq!(
        serde_json::from_str::<Value>(&serialize_confirm_bypass_for_js(&default_plan)).unwrap(),
        json!({
            "kind": "confirm_bypass",
            "tab": null,
            "scope": "default",
            "nonce": 18,
            "lines": ["Start new sessions in bypass? (y/n)"],
        })
    );

    // Inbound: the exact contract line -- both fields, and the mapping to a real `BypassScope`.
    let msg = parse_inbound_message(r#"{"type":"confirm_bypass","request_id":"r7","tab":3,"scope":"tab","nonce":17}"#)
        .unwrap();
    assert_eq!(msg.tab_ref(), TabRef::WindowLevel);
    assert_eq!(msg.request_id(), "r7");
    match msg {
        InboundMessage::ConfirmBypass { tab, scope, nonce, .. } => {
            assert_eq!(scope.scope(tab), Ok(crate::tab_set::BypassScope::Tab(TabId(3))));
            assert_eq!(nonce, 17);
        }
        other => panic!("expected ConfirmBypass, got {other:?}"),
    }

    // A missing nonce fails to parse: an answer must identify the exact prompt shown,
    // never whichever prompt happens to be current.
    assert!(parse_inbound_message(r#"{"type":"confirm_bypass","request_id":"r7","tab":3,"scope":"tab"}"#).is_none());
    // A malformed nonce, and an unrecognized scope, are each their own parse failure.
    assert!(
        parse_inbound_message(r#"{"type":"confirm_bypass","request_id":"r7","tab":3,"scope":"tab","nonce":"17"}"#)
            .is_none()
    );
    assert!(parse_inbound_message(
        r#"{"type":"confirm_bypass","request_id":"r7","tab":3,"scope":"window","nonce":17}"#
    )
    .is_none());
}

/// `BypassScopeWire::scope` refuses a mismatched pair rather than guessing which the client
/// meant -- a well-formed client only ever sends `(Tab, Some)` or `(Default, None)`.
#[test]
fn bypass_scope_wire_refuses_a_mismatched_tab_default_pairing() {
    assert_eq!(
        BypassScopeWire::Tab.scope(Some(3)),
        Ok(crate::tab_set::BypassScope::Tab(TabId(3)))
    );
    assert_eq!(
        BypassScopeWire::Default.scope(None),
        Ok(crate::tab_set::BypassScope::Default)
    );
    assert!(BypassScopeWire::Tab.scope(None).is_err());
    assert!(BypassScopeWire::Default.scope(Some(3)).is_err());
}

/// A snapshot carries `user_prompts` so a reload or resync preserves prompts already delivered
/// by the live event stream. Like neighboring tests, this constructs its own `SnapshotView`
/// with the needed fields and defaults for the rest; there is no shared builder here.
#[test]
fn a_snapshot_carries_the_user_prompts_and_their_seqs() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::UserPromptSubmitted { text: "hello".into() });
    let view = SnapshotView {
        backend: "legacy",
        conversation_id: None,
        session_id: None,
        provider_session_id: None,
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
        provider: None,
        projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
        hidden_pending: None,
    };
    let json: serde_json::Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
    assert_eq!(json["state"]["userPrompts"][0]["text"], "hello");
    assert_eq!(json["state"]["userPrompts"][0]["seq"], 0);
}

/// The event path's own half of the same link, pinned here rather than assumed from the derive:
/// if `PermissionRequested` ever gained a `skip_serializing_if` on this field, the live path
/// would start disagreeing with the snapshot path and only one of them would be caught.
#[test]
fn the_event_path_carries_tool_use_id_too_so_the_two_paths_cannot_diverge() {
    let events = vec![AgentDomainEvent::PermissionRequested {
        permission_id: "perm-1".into(),
        tool_use_id: Some("toolu_01ABC".into()),
        tool_name: "Bash".into(),
        input: json!({}),
        provider_prompt: None,
    }];
    let parsed: Value = serde_json::from_str(&serialize_events_for_js(TabId(1), 0, 1, &events)).unwrap();
    assert_eq!(parsed["events"][0]["tool_use_id"], "toolu_01ABC");
}

/// The one inbound message with no `command_result` and nothing branching on it, which is
/// exactly why it had no test: a silent diagnostic that stops parsing costs a trace column and
/// nothing else, so nothing would ever report the break. `receive_to_frame_ms` is a SPAN in
/// milliseconds -- a float, never an instant, since JS `performance.now()` and Rust `Instant`
/// have unrelated epochs.
#[test]
fn parses_turn_rendered_as_a_float_span() {
    let msg =
        parse_inbound_message(r#"{"type":"turn_rendered","request_id":"r7","receive_to_frame_ms":18.5}"#).unwrap();
    assert_eq!(msg.request_id(), "r7");
    match msg {
        InboundMessage::TurnRendered {
            receive_to_frame_ms, ..
        } => assert_eq!(receive_to_frame_ms, 18.5),
        other => panic!("expected TurnRendered, got {other:?}"),
    }
    // An integer on the wire is still a valid span -- JSON has one number type and a whole
    // number of milliseconds is an ordinary measurement, not a different shape.
    let whole =
        parse_inbound_message(r#"{"type":"turn_rendered","request_id":"r8","receive_to_frame_ms":20}"#).unwrap();
    assert!(matches!(whole, InboundMessage::TurnRendered { receive_to_frame_ms, .. } if receive_to_frame_ms == 20.0));
    // Missing the measurement is a parse failure, not a defaulted zero: a zero-millisecond
    // render would be reported into a trace as a real, impossibly good number.
    assert!(parse_inbound_message(r#"{"type":"turn_rendered","request_id":"r9"}"#).is_none());
}

#[test]
fn a_legacy_snapshot_carries_no_conversation_id_and_no_provider_block() {
    let projection = AgentSessionProjection::default();
    let view = SnapshotView {
        backend: "legacy",
        conversation_id: None,
        session_id: None,
        provider_session_id: None,
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
        provider: None,
        projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
        hidden_pending: None,
    };
    let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
    assert_eq!(parsed["state"]["backend"], "legacy");
    assert!(
        parsed["state"]["conversationId"].is_null(),
        "the legacy backend has no conversation identity"
    );
    assert!(parsed["state"]["provider"].is_null());
}

#[test]
fn the_sidecar_hello_never_advertises_resume_without_a_record() {
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
    assert!(
        parsed.get("expectedVerdandiRevision").is_none(),
        "the hello no longer carries a revision to expect"
    );
}

#[test]
fn hello_carries_the_configured_account_or_null() {
    let mut greeting = greeting_with(Vec::new());
    let value: serde_json::Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
    assert!(value["account"].is_null(), "no account configured");
    greeting.account = Some("work".to_string());
    let value: serde_json::Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
    assert_eq!(value["account"], "work");
}

fn resumable(provider_session_id: &str, created_at: &str, updated_at: &str) -> agent::ResumableSession {
    agent::ResumableSession {
        provider: "claude".into(),
        provider_session_id: provider_session_id.into(),
        created_at: created_at.into(),
        updated_at: updated_at.into(),
        title: None,
        name: None,
    }
}

/// Every session the workspace can continue reaches the frontend, in the order Rust ranked
/// them. The frontend renders rows in array order and does no sorting of its own, so this array
/// IS the picker's order.
#[test]
fn hello_lists_every_session_the_workspace_can_continue_in_rank_order() {
    let greeting = greeting_with(vec![
        resumable("1857dcd5-973b-46a2", "1757600000000", "1757700000000"),
        resumable("99b2b206-0000-4000", "1757100000000", "1757200000000"),
    ]);
    let parsed: Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
    let sessions = parsed["resumableSessions"].as_array().expect("an array, always");
    assert_eq!(sessions.len(), 2);
    // The CLAUDE id, not Verdandi's: resuming mints a new Verdandi session, so a record keyed
    // on that one would point at something that stops existing the moment it is used.
    assert_eq!(sessions[0]["providerSessionId"], "1857dcd5-973b-46a2");
    assert_eq!(sessions[0]["provider"], "claude");
    assert_eq!(sessions[0]["createdAt"], "1757600000000");
    assert_eq!(sessions[0]["updatedAt"], "1757700000000");
    assert_eq!(sessions[1]["providerSessionId"], "99b2b206-0000-4000");
}

/// Empty, not null or absent: the frontend maps over this field unconditionally, and a workspace
/// with nothing to continue is a normal state rather than a missing one.
#[test]
fn hello_carries_an_empty_list_when_the_workspace_has_nothing_to_continue() {
    let parsed: Value = serde_json::from_str(&serialize_hello_for_js(&greeting_with(Vec::new()))).unwrap();
    assert_eq!(parsed["resumableSessions"], json!([]));
}

/// The title crosses as a string, and a session without one crosses as `null` -- present, so the
/// frontend reads one shape, and not an empty string it might render as a blank row.
#[test]
fn hello_carries_each_sessions_title_or_null() {
    let mut titled = resumable("prov-1", "1000", "9000");
    titled.title = Some("fix the picker".into());
    let greeting = greeting_with(vec![titled, resumable("prov-2", "1000", "8000")]);
    let parsed: Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
    assert_eq!(parsed["resumableSessions"][0]["title"], "fix the picker");
    assert_eq!(parsed["resumableSessions"][1]["title"], Value::Null);
}

/// Both stamps cross the bridge, because they answer different questions: `createdAt` is when
/// the conversation began and `updatedAt` is when it was last opened. The frontend shows the
/// first only when it differs from the second, which it cannot do if only one is sent.
#[test]
fn hello_carries_both_stamps_for_each_session() {
    let greeting = greeting_with(vec![resumable("prov-1", "1000", "9000")]);
    let parsed: Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
    assert_eq!(parsed["resumableSessions"][0]["createdAt"], "1000");
    assert_eq!(parsed["resumableSessions"][0]["updatedAt"], "9000");
}

#[test]
fn parses_handoff_to_terminal() {
    let msg = parse_inbound_message(r#"{"type":"handoff_to_terminal","request_id":"r10"}"#).unwrap();
    assert_eq!(msg.request_id(), "r10");
    assert!(matches!(msg, InboundMessage::HandoffToTerminal { .. }));
}

/// The command goes over the wire as one ready-to-run line PLUS its parts. The line is what a
/// user copies; the parts are what lets the frontend say which session and which directory
/// without re-parsing the line it was given.
///
/// Built from the real `agent::handoff::ClaudeResumeCommand` rather than a hand-written string,
/// so a change to what "continue this session" means reaches this envelope automatically.
#[test]
fn serialize_handoff_for_js_carries_the_runnable_line_and_its_parts() {
    let command = agent::handoff::ClaudeResumeCommand::for_session("/home/user/project", "1857dcd5-973b-46a2").unwrap();
    let parsed: Value = serde_json::from_str(&serialize_handoff_for_js(TabId(1), &command)).unwrap();
    assert_eq!(parsed["kind"], "handoff");
    assert_eq!(parsed["tab"], 1);
    assert_eq!(
        parsed["command"],
        "cd /home/user/project && claude --resume 1857dcd5-973b-46a2"
    );
    assert_eq!(parsed["cwd"], "/home/user/project");
    assert_eq!(parsed["providerSessionId"], "1857dcd5-973b-46a2");
}

/// The id the envelope carries is the ARGUMENT of the command, read back out of the argv --
/// never a second copy of the id passed in alongside it. Two sources for one value is how the
/// panel would eventually show one session and print a command resuming another.
#[test]
fn the_envelopes_session_id_is_the_one_the_command_actually_resumes() {
    let command = agent::handoff::ClaudeResumeCommand::for_session("/tmp/p", " padded-id ").unwrap();
    let parsed: Value = serde_json::from_str(&serialize_handoff_for_js(TabId(1), &command)).unwrap();
    assert_eq!(parsed["providerSessionId"], "padded-id");
    assert!(parsed["command"]
        .as_str()
        .unwrap()
        .ends_with("claude --resume padded-id"));
}

#[test]
fn the_legacy_hello_offers_its_three_modes() {
    let greeting = crate::agent_backend::BackendGreeting::for_kind(
        crate::agent_backend::BackendKind::Legacy,
        std::path::PathBuf::from("/tmp/project"),
    );
    let parsed: Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
    assert_eq!(parsed["backend"], "legacy");
    assert_eq!(parsed["permissionModes"], json!(["manual", "auto", "bypass"]));
    assert_eq!(
        parsed["resumableSessions"],
        json!([]),
        "the legacy backend can never offer a resume"
    );
}
