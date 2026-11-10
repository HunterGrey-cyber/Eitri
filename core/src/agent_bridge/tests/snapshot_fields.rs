//! What a snapshot carries, field by field: the running turn's prompt, held changes, usage, limits, commands, permissions and their tool calls, history, and the order of its collections.
#![cfg(test)]

use super::*;

/// A snapshot taken mid-turn names the prompt that turn opened with and whether Claude Code started
/// it; once the turn ends it names neither.
#[test]
fn the_running_turns_opening_prompt_and_origin_ride_the_snapshot_only_while_set() {
    let snapshot = |projection: &AgentSessionProjection| {
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities::default(),
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(projection),
            hidden_pending: None,
        };
        serde_json::from_str::<Value>(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap()["state"].clone()
    };
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::UserPromptSubmitted { text: "go".into() });
    projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    let state = snapshot(&projection);
    assert_eq!(state["openingPromptSeq"], json!(0));
    assert!(state.get("runningTurnCli").is_none());
    projection.apply(&AgentDomainEvent::TurnCompleted {
        turn_id: "t1".into(),
        outcome: agent::TurnOutcome::Completed,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
        detail: Default::default(),
    });
    projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "c1".into() });
    projection.apply(&AgentDomainEvent::TurnAttributed {
        turn_id: "c1".into(),
        origin: agent::TurnOrigin::Cli,
        cause: None,
    });
    let state = snapshot(&projection);
    assert!(state.get("openingPromptSeq").is_none());
    assert_eq!(state["runningTurnCli"], json!(true));
}

/// A change held for a call that has not started rides the snapshot, so a page rebuilt from it
/// attaches the mark when the call starts, as Rust does; with nothing held the key is absent.
#[test]
fn a_change_held_for_its_call_rides_the_snapshot_and_nothing_held_sends_no_key() {
    let mut projection = AgentSessionProjection::default();
    let view = |projection: &AgentSessionProjection| {
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities::default(),
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(projection),
            hidden_pending: None,
        };
        serde_json::from_str::<Value>(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap()
    };
    assert!(view(&projection)["state"].get("changedInputsBeforeTheirCall").is_none());
    projection.apply(&AgentDomainEvent::ToolInputChanged {
        tool_use_id: "toolu_9".into(),
        change: agent::InputChange {
            fields: json!({ "command": "b" }).as_object().unwrap().clone(),
            removed: vec![],
            clipped: false,
        },
    });
    let held = &view(&projection)["state"]["changedInputsBeforeTheirCall"];
    assert_eq!(held[0]["toolUseId"], "toolu_9");
    assert_eq!(held[0]["change"]["fields"]["command"], "b");
}

/// A tab's last usage figure rides its snapshot so a tab switch or panel reload shows it
/// immediately. It is a present `null` key until a provider reports usage, then replaced whole
/// by each report. Its snake_case fields match `turn_completed` and `agent::UsageInfo`, even
/// though the surrounding state uses camelCase.
#[test]
fn a_snapshot_carries_the_last_reported_usage_and_null_before_any() {
    let usage_in_snapshot = |projection: &AgentSessionProjection| -> Value {
        let view = SnapshotView {
            backend: "sidecar",
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
            projection: crate::agent_backend::ProjectionRef::Borrowed(projection),
            hidden_pending: None,
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        assert!(
            parsed["state"].as_object().unwrap().contains_key("usage"),
            "the key is always sent: `null` says nothing was reported, an absent key says this build predates it"
        );
        parsed["state"]["usage"].clone()
    };
    let completed = |usage: Option<agent::UsageInfo>| AgentDomainEvent::TurnCompleted {
        turn_id: "t1".into(),
        outcome: agent::TurnOutcome::Completed,
        result_text: String::new(),
        stop_reason: None,
        usage,
        detail: Default::default(),
    };

    let mut projection = AgentSessionProjection::default();
    assert_eq!(
        usage_in_snapshot(&projection),
        Value::Null,
        "a fresh session has reported nothing"
    );

    // A turn that reports nothing (interrupted, synthesized) is unknown, not a zero.
    projection.apply(&completed(None));
    assert_eq!(
        usage_in_snapshot(&projection),
        Value::Null,
        "silence is not a measurement"
    );

    // The sidecar's shape: tokens and a model, no turn count.
    projection.apply(&completed(Some(agent::UsageInfo {
        cost_reported: true,
        total_cost_usd: 0.42,
        num_turns: None,
        tokens: Some(agent::TokenUsage {
            input: 1,
            output: 2,
            cache_creation: 3,
            cache_read: 4,
        }),
        model: Some("claude-sonnet-5".into()),
    })));
    assert_eq!(
        usage_in_snapshot(&projection),
        json!({
            "total_cost_usd": 0.42,
            "num_turns": null,
            "tokens": {"input": 1, "output": 2, "cache_creation": 3, "cache_read": 4},
            "model": "claude-sonnet-5",
        })
    );

    // A later turn that reports nothing leaves the last figure standing...
    projection.apply(&completed(None));
    assert_eq!(usage_in_snapshot(&projection)["total_cost_usd"], 0.42);

    // ...and one that does replaces it whole, a lower figure included (`/clear` resets the SDK's
    // running total). Legacy's shape: a turn count, no tokens and no model.
    projection.apply(&completed(Some(agent::UsageInfo {
        cost_reported: true,
        total_cost_usd: 0.01,
        num_turns: Some(3),
        tokens: None,
        model: None,
    })));
    assert_eq!(
        usage_in_snapshot(&projection),
        json!({"total_cost_usd": 0.01, "num_turns": 3, "tokens": null, "model": null})
    );
}

/// The account's usage limit rides the snapshot like the usage figure does, so a tab switch or a
/// panel reload shows it without waiting for the agent's next report. An explicit `null` until one
/// arrived; camelCase inside, unlike `usage`, because the panel folds it from a hand-shaped object.
/// How many commands the agent left running rides the snapshot, so a tab switch or a reload shows
/// the band's count; `0` when there are none.
#[test]
fn the_background_count_reaches_the_ui_snapshot() {
    let count_in_snapshot = |projection: &AgentSessionProjection| -> Value {
        let view = SnapshotView {
            backend: "external",
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
            projection: crate::agent_backend::ProjectionRef::Borrowed(projection),
            hidden_pending: None,
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        parsed["state"]["backgroundCalls"].clone()
    };
    let mut projection = AgentSessionProjection::default();
    assert_eq!(count_in_snapshot(&projection), json!(0));
    projection.apply(&AgentDomainEvent::BackgroundCalls { running: 2 });
    assert_eq!(count_in_snapshot(&projection), json!(2));
    projection.apply(&AgentDomainEvent::BackgroundCalls { running: 0 });
    assert_eq!(count_in_snapshot(&projection), json!(0));
}

#[test]
fn account_limits_reach_the_ui_snapshot() {
    let limits_in_snapshot = |projection: &AgentSessionProjection| -> Value {
        let view = SnapshotView {
            backend: "sidecar",
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
            projection: crate::agent_backend::ProjectionRef::Borrowed(projection),
            hidden_pending: None,
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        assert!(
            parsed["state"].as_object().unwrap().contains_key("accountLimits"),
            "the key is always sent: `null` says no report yet"
        );
        parsed["state"]["accountLimits"].clone()
    };
    let mut projection = AgentSessionProjection::default();
    assert_eq!(limits_in_snapshot(&projection), Value::Null);

    projection.apply(&AgentDomainEvent::AccountLimits {
        used_percent: 42.5,
        window_minutes: Some(300),
        resets_at: Some(1_790_000_000),
        plan: Some("pro".into()),
    });
    assert_eq!(
        limits_in_snapshot(&projection),
        json!({"usedPercent": 42.5, "windowMinutes": 300, "resetsAt": 1_790_000_000i64, "plan": "pro"})
    );

    // The latest report replaces the whole figure; fields the agent left out are `null`, not kept.
    projection.apply(&AgentDomainEvent::AccountLimits {
        used_percent: 61.0,
        window_minutes: None,
        resets_at: None,
        plan: None,
    });
    assert_eq!(
        limits_in_snapshot(&projection),
        json!({"usedPercent": 61.0, "windowMinutes": null, "resetsAt": null, "plan": null})
    );
}

#[test]
fn available_commands_reach_the_ui_snapshot() {
    let commands_in_snapshot = |projection: &AgentSessionProjection| -> Value {
        let view = SnapshotView {
            backend: "sidecar",
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
            projection: crate::agent_backend::ProjectionRef::Borrowed(projection),
            hidden_pending: None,
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        assert!(
            parsed["state"].as_object().unwrap().contains_key("availableCommands"),
            "the key is always sent: `null` says the agent listed none"
        );
        parsed["state"]["availableCommands"].clone()
    };
    let mut projection = AgentSessionProjection::default();
    assert_eq!(commands_in_snapshot(&projection), Value::Null);

    projection.apply(&AgentDomainEvent::CommandsAvailable {
        commands: vec![
            agent::SlashCommand {
                name: "web".into(),
                description: "Search the web".into(),
                hint: Some("query".into()),
            },
            agent::SlashCommand {
                name: "test".into(),
                description: String::new(),
                hint: None,
            },
        ],
    });
    assert_eq!(
        commands_in_snapshot(&projection),
        json!([
            {"name": "web", "description": "Search the web", "hint": "query"},
            {"name": "test", "description": ""},
        ])
    );

    // The latest list replaces the whole earlier one, and an empty one withdraws them.
    projection.apply(&AgentDomainEvent::CommandsAvailable { commands: vec![] });
    assert_eq!(commands_in_snapshot(&projection), json!([]));
}

/// sw-panel-render-2 (2026-09-27): a snapshot taken mid-reply used to say nothing at all about
/// whether the last transcript entry was still open -- `applySnapshot` forced its own copy of
/// the bit to `false` regardless, splitting a streaming reply into two rows on every snapshot
/// landing mid-stream. This projection's own bit (`assistant_message_open`) is genuinely `true`
/// here (a `ContentDelta` with no tool call or turn boundary after it), and the wire must say so.
#[test]
fn a_snapshot_mid_reply_carries_the_open_message_bit() {
    let capabilities = agent::ProviderCapabilities {
        resume: false,
        fork: false,
        interrupt: true,
        bypass_permission_mode: true,
        interactive_permission_mode: true,
        cli_auto_mode: false,
        steer: false,
        ..Default::default()
    };
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::SessionOpened {
        session_id: "verdandi-1".into(),
        provider_session_id: "claude-1".into(),
        model: "claude-sonnet-5".into(),
        cwd: "/tmp".into(),
    });
    projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    projection.apply(&AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: agent::ContentKind::Text,
        text: "Use **strong".into(),
    });
    assert!(
        projection.assistant_message_open,
        "the projection's own bit must be true here"
    );
    {
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities,
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
            hidden_pending: None,
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        assert_eq!(parsed["state"]["assistantMessageOpen"], true);
    }

    // And the other direction: a tool call closes it, on both sides, and the wire says so too.
    projection.apply(&AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_1".into(),
        name: "Bash".into(),
        input: json!({}),
    });
    assert!(!projection.assistant_message_open);
    let view = SnapshotView {
        backend: "sidecar",
        conversation_id: None,
        session_id: None,
        provider_session_id: None,
        capabilities,
        provider: None,
        projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
        hidden_pending: None,
    };
    let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
    assert_eq!(parsed["state"]["assistantMessageOpen"], false);
}

/// The link from a permission card back to the tool call it gates.
///
/// The EVENT path has always carried it (`AgentDomainEvent`'s own derived `Serialize` emits
/// every field of `PermissionRequested`); this serializer dropped it, so a panel that
/// rehydrated from a snapshot -- a reload, or a bounded-queue overflow -- lost a link the live
/// path had. With several calls of one tool in flight, "Bash wants to run" identifies nothing.
#[test]
fn a_pending_permission_snapshot_carries_the_tool_call_it_gates() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::PermissionRequested {
        permission_id: "perm-1".into(),
        tool_use_id: Some("toolu_01ABC".into()),
        tool_name: "Bash".into(),
        input: json!({"command": "rm -rf /"}),
        provider_prompt: None,
    });
    let view = SnapshotView {
        backend: "sidecar",
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
    let pending = &parsed["state"]["pendingPermissions"][0];
    assert_eq!(pending["permissionId"], "perm-1");
    assert_eq!(pending["toolUseId"], "toolu_01ABC");
    assert_eq!(pending["toolName"], "Bash");
}

/// The CLI's own prompt for a call is a card of its own for the SAME call: its snapshot entry and its
/// event both carry the call's id next to the provider's words, so the panel can hang it under the
/// call and `a`/`d` on that call's row reach it. Without the id the card would be drawn by
/// sequence only and no key on the tool row could answer it.
#[test]
fn a_provider_prompt_keeps_the_tool_call_it_names_in_snapshot_and_event() {
    let event = AgentDomainEvent::PermissionRequested {
        permission_id: "perm-cli".into(),
        tool_use_id: Some("toolu_01ABC".into()),
        tool_name: "Bash".into(),
        input: json!({"command": "echo hi"}),
        provider_prompt: Some(agent::ProviderPrompt {
            reason: None,
            description: Some("Print hi".into()),
            blocked_path: None,
            matched_ask_rule: Some(agent::MatchedAskRule {
                source: "projectSettings".into(),
                tool_name: "Bash".into(),
                rule_content: Some("echo:*".into()),
            }),
            unrecognized_origin: None,
        }),
    };
    let on_the_wire = serde_json::to_value(&event).unwrap();
    assert_eq!(on_the_wire["tool_use_id"], "toolu_01ABC");
    assert!(on_the_wire["provider_prompt"].is_object());

    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_01ABC".into(),
        name: "Bash".into(),
        input: json!({"command": "echo hi"}),
    });
    projection.apply(&event);
    let view = SnapshotView {
        backend: "sidecar",
        conversation_id: None,
        session_id: None,
        provider_session_id: None,
        capabilities: agent::ProviderCapabilities {
            resume: false,
            fork: false,
            interrupt: true,
            bypass_permission_mode: true,
            interactive_permission_mode: true,
            cli_auto_mode: true,
            steer: false,
            ..Default::default()
        },
        provider: None,
        projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
        hidden_pending: None,
    };
    let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
    let pending = &parsed["state"]["pendingPermissions"][0];
    assert_eq!(pending["toolUseId"], parsed["state"]["toolCalls"][0]["toolUseId"]);
    assert_eq!(pending["toolUseId"], "toolu_01ABC");
    assert_eq!(pending["providerPrompt"]["matchedAskRule"]["ruleContent"], "echo:*");
}

/// The CLI's own refusal of a call reaches a rebuilt panel too: `denied` on that call's entry,
/// camelCase like the rest of the snapshot, the CLI's words verbatim -- and no such key on a call
/// nobody refused, so a snapshot without refusals reads as it did before the field existed.
#[test]
fn a_snapshot_carries_the_clis_refusal_on_the_call_it_refused() {
    let mut projection = AgentSessionProjection::default();
    for id in ["toolu_ok", "toolu_push"] {
        projection.apply(&AgentDomainEvent::ToolCallStarted {
            turn_id: "t".into(),
            tool_use_id: id.into(),
            name: "Bash".into(),
            input: json!({ "command": "git push --force origin main" }),
        });
    }
    projection.apply(&AgentDomainEvent::PermissionDenied {
        tool_use_id: Some("toolu_push".into()),
        tool_name: "Bash".into(),
        reason_type: Some("classifier".into()),
        reason: Some("[Git Destructive]".into()),
    });
    let view = SnapshotView {
        backend: "sidecar",
        conversation_id: None,
        session_id: None,
        provider_session_id: None,
        capabilities: agent::ProviderCapabilities::default(),
        provider: None,
        projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
        hidden_pending: None,
    };
    let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
    let calls = &parsed["state"]["toolCalls"];
    assert!(calls[0].get("denied").is_none(), "{calls}");
    assert_eq!(
        calls[1]["denied"],
        json!({ "reasonType": "classifier", "reason": "[Git Destructive]" })
    );
}

/// A reload or a tab switch rebuilds the panel from this snapshot, so the CLI's own prompt
/// must arrive here with its words (`providerPrompt`, camelCase like the rest of the snapshot)
/// or its card would lose the reason it was drawn for. A gate request carries no such key.
#[test]
fn a_snapshot_carries_the_clis_own_prompt_and_its_words() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::PermissionRequested {
        permission_id: "perm-gate".into(),
        tool_use_id: Some("toolu_1".into()),
        tool_name: "Write".into(),
        input: json!({}),
        provider_prompt: None,
    });
    projection.apply(&AgentDomainEvent::PermissionRequested {
        permission_id: "perm-cli".into(),
        tool_use_id: Some("toolu_1".into()),
        tool_name: "Write".into(),
        input: json!({}),
        provider_prompt: Some(agent::ProviderPrompt {
            reason: Some("Claude requested permissions to edit /p/.git/probe which is a sensitive file.".into()),
            description: Some(".git/probe".into()),
            blocked_path: Some("/p/.git/probe".into()),
            matched_ask_rule: Some(agent::MatchedAskRule {
                source: "projectSettings".into(),
                tool_name: "Write".into(),
                rule_content: None,
            }),
            unrecognized_origin: None,
        }),
    });
    let view = SnapshotView {
        backend: "sidecar",
        conversation_id: None,
        session_id: None,
        provider_session_id: None,
        capabilities: agent::ProviderCapabilities::default(),
        provider: None,
        projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
        hidden_pending: None,
    };
    let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
    let cards = parsed["state"]["pendingPermissions"].as_array().unwrap();
    assert_eq!(cards.len(), 2, "both requests for the one call are cards of their own");
    assert_eq!(cards[0]["permissionId"], "perm-gate");
    assert!(!cards[0].as_object().unwrap().contains_key("providerPrompt"));
    assert_eq!(cards[1]["permissionId"], "perm-cli");
    let prompt = &cards[1]["providerPrompt"];
    assert_eq!(
        prompt["reason"],
        "Claude requested permissions to edit /p/.git/probe which is a sensitive file."
    );
    assert_eq!(prompt["description"], ".git/probe");
    assert_eq!(prompt["blockedPath"], "/p/.git/probe");
    assert_eq!(prompt["matchedAskRule"]["source"], "projectSettings");
    assert_eq!(prompt["matchedAskRule"]["toolName"], "Write");
    assert!(prompt["matchedAskRule"]["ruleContent"].is_null());
}

/// `null`, not an omitted key. The TS side declares the field as `string | null`, and an absent
/// key would arrive as `undefined` -- which reads as "this build is too old to send it" rather
/// than "no id was sent for this request". This pins the serializer's handling of `None`; when
/// a request genuinely has no id to send is settled upstream, at `agent/src/session.rs`'s
/// `permission_requested_event` and `agent/src/wire.rs`'s `can_use_tool` arm.
#[test]
fn a_permission_with_no_tool_use_id_says_null_rather_than_omitting_the_key() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::PermissionRequested {
        permission_id: "perm-1".into(),
        tool_use_id: None,
        tool_name: "Bash".into(),
        input: json!({}),
        provider_prompt: None,
    });
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
    let pending = &parsed["state"]["pendingPermissions"][0];
    assert!(pending["toolUseId"].is_null());
    assert!(
        pending.as_object().unwrap().contains_key("toolUseId"),
        "the key must be present and null, not absent"
    );
}

/// The notice a restored history puts on the wire. Every key is checked, because
/// the panel's four wordings each read a different one and a missing key renders as
/// `undefined` -- which is not a state the TS type admits.
#[test]
fn a_restored_history_reaches_the_frontend_as_one_notice_object() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::UserPromptSubmitted {
        text: "what did we say?".into(),
    });
    projection.history = Some(agent::HistoryNotice {
        source: agent::HistorySource::EitriCopy,
        restored_items: 314,
        omitted_items: Some(1431),
        omitted_turns: None,
        upto_seq: projection.last_revision,
        source_path: "/state/eitri/history/conv/sess.json".into(),
        attempted_transcript_path: Some("/claude/projects/p/sess.jsonl".into()),
        fallback_reason: Some("transcript file not found".into()),
        writer_version: None,
    });
    let view = SnapshotView {
        backend: "sidecar",
        conversation_id: None,
        session_id: None,
        provider_session_id: None,
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
    let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
    let history = &parsed["state"]["history"];
    assert_eq!(history["source"], "eitri_copy");
    assert_eq!(history["restoredItems"], 314);
    assert_eq!(history["omittedItems"], 1431);
    assert_eq!(history["uptoSeq"], 1);
    assert_eq!(history["sourcePath"], "/state/eitri/history/conv/sess.json");
    assert_eq!(history["attemptedTranscriptPath"], "/claude/projects/p/sess.jsonl");
    assert_eq!(history["fallbackReason"], "transcript file not found");
    assert!(history["writerVersion"].is_null());
    // Every restored item sits below `uptoSeq`, which is what lets the panel tell a historical
    // tool call that can never complete from a live one that is still running.
    assert!(parsed["state"]["userPrompts"][0]["seq"].as_u64().unwrap() < history["uptoSeq"].as_u64().unwrap());
}

#[test]
fn acp_replay_omitted_turns_reach_the_snapshot_with_the_original_sequence_boundary() {
    use agent::providers::acp::history::{History, HISTORY_TURN_LIMIT};
    use agent::providers::acp::wire::{ChunkKind, Update};

    let mut history = History::default();
    for n in 0..3 * HISTORY_TURN_LIMIT {
        for (kind, text) in [
            (ChunkKind::User, format!("prompt {n}")),
            (ChunkKind::Agent, "reply".into()),
        ] {
            history.apply(&Update::Chunk {
                kind,
                text: Some(text),
                message_id: None,
            });
        }
    }
    let mut projection = AgentSessionProjection::default();
    for event in history.finish() {
        projection.apply(&event);
    }
    let boundary = projection.history.as_ref().unwrap().upto_seq;
    assert!(projection.user_prompts[0].seq > 0);
    assert!(projection.transcript.last().unwrap().seq < boundary);
    projection.apply(&AgentDomainEvent::UserPromptSubmitted { text: "live".into() });
    assert!(projection.user_prompts.last().unwrap().seq >= boundary);
    let view = SnapshotView {
        backend: "external",
        conversation_id: None,
        session_id: None,
        provider_session_id: None,
        capabilities: agent::ProviderCapabilities::default(),
        provider: None,
        projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
        hidden_pending: None,
    };
    let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
    let notice = &parsed["state"]["history"];
    assert_eq!(notice["source"], "acp_replay");
    assert_eq!(notice["omittedTurns"], 2 * HISTORY_TURN_LIMIT);
    assert!(notice["omittedItems"].is_null());
    assert_eq!(notice["restoredItems"], 2 * HISTORY_TURN_LIMIT);
    assert_eq!(notice["uptoSeq"], boundary);
    assert_eq!(notice["sourcePath"], "");
}

/// A session that restored nothing -- every fresh one -- sends an explicit `null`. Omitting the
/// key would make "this session has no history" indistinguishable from "this build predates the
/// field", the same distinction the `toolUseId` test above exists for.
#[test]
fn a_session_with_no_restored_history_says_null_rather_than_omitting_the_key() {
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
    let state = parsed["state"].as_object().unwrap();
    assert!(
        state.contains_key("history"),
        "the key must be present and null, not absent"
    );
    assert!(state["history"].is_null());
}

/// The legacy backend's own shape, which the two tests above do not cover between them: on its
/// `PreToolUse` hook-relay path the permission id and the tool-use id are the SAME string,
/// because the hook payload's `tool_use_id` is both the identity of the gated call and the key
/// the live relay socket is filed under. Serializing them as two separate keys with one value
/// is correct and must stay that way -- the frontend keys cards on `permissionId` and matches
/// tool calls on `toolUseId`, and collapsing either into the other would tie the routing key to
/// the rendering link.
#[test]
fn a_legacy_hook_relay_permission_sends_the_same_id_under_both_keys() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::PermissionRequested {
        permission_id: "toolu_01CtdezhmhUCrBaswxW5HYmC".into(),
        tool_use_id: Some("toolu_01CtdezhmhUCrBaswxW5HYmC".into()),
        tool_name: "Bash".into(),
        input: json!({"command": "echo hello"}),
        provider_prompt: None,
    });
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
    let pending = &parsed["state"]["pendingPermissions"][0];
    assert_eq!(pending["permissionId"], "toolu_01CtdezhmhUCrBaswxW5HYmC");
    assert_eq!(pending["toolUseId"], "toolu_01CtdezhmhUCrBaswxW5HYmC");
}

/// The snapshot's half of interleaved ordering (2026-09-15).
///
/// The frontend rendered `transcript`, then `toolCalls`, then `pendingPermissions` as three
/// sequential lists, so every tool card sat below every assistant message. It could not have
/// done better from this payload: before `seq`, nothing here said how the three collections
/// interleave. Reconstructing it from the live event stream alone would have been lost on every
/// reload and every `UiDelivery::Resync`, which both rebuild the whole frontend state from
/// exactly this snapshot.
#[test]
fn a_snapshot_carries_the_order_of_the_three_collections_against_each_other() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: agent::ContentKind::Text,
        text: "I'll check.".into(),
    });
    projection.apply(&AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_1".into(),
        name: "Bash".into(),
        input: json!({}),
    });
    projection.apply(&AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: agent::ContentKind::Text,
        text: "And now this.".into(),
    });
    projection.apply(&AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_2".into(),
        name: "Read".into(),
        input: json!({}),
    });
    projection.apply(&AgentDomainEvent::PermissionRequested {
        permission_id: "perm-1".into(),
        tool_use_id: Some("toolu_2".into()),
        tool_name: "Read".into(),
        input: json!({}),
        provider_prompt: None,
    });

    let view = SnapshotView {
        backend: "sidecar",
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
    let state = &parsed["state"];

    let mut merged: Vec<(u64, String)> = Vec::new();
    for m in state["transcript"].as_array().unwrap() {
        merged.push((
            m["seq"].as_u64().unwrap(),
            format!("text:{}", m["text"].as_str().unwrap()),
        ));
    }
    for c in state["toolCalls"].as_array().unwrap() {
        merged.push((
            c["seq"].as_u64().unwrap(),
            format!("tool:{}", c["toolUseId"].as_str().unwrap()),
        ));
    }
    for p in state["pendingPermissions"].as_array().unwrap() {
        merged.push((
            p["seq"].as_u64().unwrap(),
            format!("perm:{}", p["permissionId"].as_str().unwrap()),
        ));
    }
    merged.sort_by_key(|(seq, _)| *seq);

    assert_eq!(
        merged.into_iter().map(|(_, label)| label).collect::<Vec<_>>(),
        vec![
            "text:I'll check.",
            "tool:toolu_1",
            "text:And now this.",
            "tool:toolu_2",
            "perm:perm-1"
        ],
    );
    // Every seq is below the revision the same envelope reports, so the frontend can seed its
    // own counter from `throughRevision` and never collide with an item this snapshot carried.
    assert_eq!(parsed["throughRevision"], 5);
}

/// `pending_permissions` is a `HashMap`, and `HashMap::values()` order is unspecified -- two
/// snapshots of the same state could emit two different card orders, and a reader sorting by
/// `seq` would still be at the mercy of whatever order the array happened to arrive in for
/// anything it could not sort. Emitting them already ordered makes the payload itself
/// deterministic.
///
/// SIZED AND REPEATED DELIBERATELY, because the thing under test is nondeterminism and a
/// careless version of this test inherits it. One `HashMap` iterates in one fixed order for its
/// whole life, and a fresh one draws a new `RandomState`, so what decides whether an UNSORTED
/// emission passes is how often a fresh map happens to iterate in insertion order. Measured on
/// this machine, 200,000 fresh maps each: **5 keys reproduce insertion order 2.53% of the
/// time** -- so the five-key version of this test used to pass roughly one run in forty with
/// the sort deleted. At 12 keys that fell to 0.001%, and at 16 and 24 keys it was 0 in 200,000.
///
/// So: 16 permissions, and `PROJECTIONS` independently built ones, all of which must agree.
/// This is not literally deterministic -- nothing that reads a `HashMap` can be -- but a false
/// pass now needs every one of those independent maps to draw insertion order at a rate already
/// below what 200,000 trials could measure for a single one. Stated as a measurement rather
/// than as a guarantee, because that is what it is.
#[test]
fn pending_permissions_are_emitted_in_the_order_they_were_requested() {
    const PERMISSIONS: usize = 16;
    const PROJECTIONS: usize = 8;

    let expected: Vec<String> = (0..PERMISSIONS).map(|i| format!("perm-{i:02}")).collect();

    for attempt in 0..PROJECTIONS {
        let mut projection = AgentSessionProjection::default();
        for id in &expected {
            projection.apply(&AgentDomainEvent::PermissionRequested {
                permission_id: id.clone(),
                tool_use_id: None,
                tool_name: "Bash".into(),
                input: json!({}),
                provider_prompt: None,
            });
        }
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
        let cards = parsed["state"]["pendingPermissions"].as_array().unwrap();

        let ids: Vec<&str> = cards.iter().map(|p| p["permissionId"].as_str().unwrap()).collect();
        assert_eq!(
            ids, expected,
            "projection {attempt} emitted its cards out of request order"
        );

        // The request order IS seq order; asserted separately so a future change that kept the
        // ids lined up while emitting some other key's order still fails here.
        let seqs: Vec<u64> = cards.iter().map(|p| p["seq"].as_u64().unwrap()).collect();
        assert!(
            seqs.windows(2).all(|w| w[0] < w[1]),
            "projection {attempt} emitted seqs {seqs:?}"
        );
    }
}

/// `hidden_pending` (`Tab::host_answered`) removes exactly the named ids from
/// `pendingPermissions`. This test builds no tool calls or transcript, so their absence
/// cannot demonstrate whether they were preserved.
#[test]
fn a_snapshot_hides_the_ids_it_is_given() {
    let mut projection = AgentSessionProjection::default();
    for id in ["shown", "hidden"] {
        projection.apply(&AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: None,
            tool_name: "Bash".into(),
            input: json!({}),
            provider_prompt: None,
        });
    }
    let hidden: std::collections::BTreeSet<String> = ["hidden".to_string()].into_iter().collect();
    let view = SnapshotView {
        backend: "sidecar",
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
        hidden_pending: Some(&hidden),
    };
    let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
    let cards = parsed["state"]["pendingPermissions"].as_array().unwrap();
    let ids: Vec<&str> = cards.iter().map(|p| p["permissionId"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["shown"], "{ids:?}");
}
