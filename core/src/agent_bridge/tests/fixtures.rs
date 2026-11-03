//! The fixtures the page's tests read, compared against what the serializer sends: Claude twins, Codex cards, steered messages, changed input, history records, unseen turns and CLI turns.
#![cfg(test)]

use super::*;

/// `agent-ui/web/src/fixtures/codex-cards.json`: what a Codex tab's permission cards, tool rows and
/// limit-stopped turn look like on the wire -- the events payload this side sends and the snapshot it
/// sends after folding the same events -- built from the provider's own card and tool-call
/// constructors, which the panel's tests render. Compared on every run; `EITRI_WRITE_FIXTURES=1`
/// rewrites it.
#[test]
fn the_codex_cards_fixture_is_what_the_serializer_sends() {
    use agent::providers::codex::approvals::{CardKind, FileChangeEntry, Wording};
    use agent::providers::codex::translate;
    use agent::providers::codex::wire::{ThreadItem, TurnError, TurnStatus};
    let card = |name: &str, kind: CardKind, subagent: Option<&str>| {
        let mut input = kind.input();
        if let (Some(label), Some(object)) = (subagent, input.as_object_mut()) {
            object.insert("subagent".into(), json!(label));
        }
        (
            name.to_string(),
            vec![AgentDomainEvent::PermissionRequested {
                permission_id: format!("perm-{name}"),
                tool_use_id: Some(format!("item-{name}")),
                tool_name: kind.tool_name().to_string(),
                input,
                provider_prompt: None,
            }],
        )
    };
    let row = |item: serde_json::Value, subagent: Option<&str>| {
        let item: ThreadItem = serde_json::from_value(item).expect("a thread item");
        let (name, mut input) = translate::tool_start(&item).expect("a tool call");
        if let (Some(label), Some(object)) = (subagent, input.as_object_mut()) {
            object.insert("subagent".into(), json!(label));
        }
        let (content, is_error) = translate::tool_result(&item).expect("a result");
        let id = item.id().expect("an id").to_string();
        vec![
            AgentDomainEvent::ToolCallStarted {
                turn_id: "t1".into(),
                tool_use_id: id.clone(),
                name: name.to_string(),
                input,
            },
            AgentDomainEvent::ToolCallCompleted {
                turn_id: "t1".into(),
                tool_use_id: id,
                content,
                is_error,
            },
        ]
    };
    let entry = |path: &str, kind: &str, diff: &str, move_path: Option<&str>| FileChangeEntry {
        path: path.into(),
        kind: kind.into(),
        diff: diff.into(),
        move_path: move_path.map(str::to_string),
    };
    let (limit_outcome, limit_detail) = translate::turn_ending(
        &TurnStatus::Failed,
        Some(
            &serde_json::from_value::<TurnError>(json!({
                "message": "You've hit your usage limit.",
                "codexErrorInfo": "usageLimitExceeded",
            }))
            .unwrap(),
        ),
    );
    let cases: Vec<(String, Vec<AgentDomainEvent>)> = vec![
        card(
            "command",
            CardKind::Command {
                command: Some("/bin/bash -lc 'git push origin main'".into()),
                cwd: Some("/project".into()),
                escalation: Wording::OutsideSandbox,
                reason: Some("The push needs the network".into()),
            },
            None,
        ),
        card(
            "hidden",
            CardKind::Command {
                command: Some("/bin/bash -lc 'echo \u{202e}dcba'".into()),
                cwd: Some("/project".into()),
                escalation: Wording::OutsideSandbox,
                reason: None,
            },
            None,
        ),
        card(
            "network",
            CardKind::Command {
                command: None,
                cwd: None,
                escalation: Wording::Network {
                    host: "registry.example.com".into(),
                    protocol: "https".into(),
                },
                reason: None,
            },
            None,
        ),
        card(
            "subagent",
            CardKind::Command {
                command: Some("/bin/bash -lc 'cargo test'".into()),
                cwd: Some("/project".into()),
                escalation: Wording::OutsideSandbox,
                reason: None,
            },
            Some("/root/tests"),
        ),
        card(
            "file_change",
            CardKind::FileChange {
                changes: vec![
                    entry("/project/new.txt", "add", "one\ntwo\n", None),
                    entry(
                        "/project/src/lib.rs",
                        "update",
                        "@@ -1,2 +1,2 @@\n fn a() {}\n-fn b() {}\n+fn c() {}\n",
                        Some("/project/src/main.rs"),
                    ),
                ],
                reason: Some("Rename b to c".into()),
            },
            None,
        ),
        card(
            "mcp",
            CardKind::Mcp {
                server: "docs".into(),
                tool: "lookup".into(),
                arguments: json!({"query": "tokio", "limit": 3}),
            },
            None,
        ),
        (
            "rows".to_string(),
            [
                row(
                    json!({"type": "commandExecution", "id": "r1", "command": "/bin/bash -lc 'ls'", "cwd": "/project",
                           "status": "completed", "aggregatedOutput": "Cargo.toml\n", "exitCode": 0}),
                    None,
                ),
                row(
                    json!({"type": "commandExecution", "id": "r2", "command": "/bin/bash -lc 'cargo build'",
                           "cwd": "/project", "status": "completed", "aggregatedOutput": "ok\n", "exitCode": 0}),
                    Some("/root/builder"),
                ),
                row(
                    json!({"type": "fileChange", "id": "r3", "status": "completed", "changes": [
                        {"path": "/project/a.txt", "kind": {"type": "add"}, "diff": "hello\n"}]}),
                    None,
                ),
                row(
                    json!({"type": "mcpToolCall", "id": "r4", "server": "docs", "tool": "lookup", "status": "completed",
                           "arguments": {"query": "tokio"}, "result": {"content": []}}),
                    None,
                ),
                row(json!({"type": "webSearch", "id": "r5", "query": "tokio runtime"}), None),
                row(
                    json!({"type": "collabAgentToolCall", "id": "r6", "tool": "spawnAgent", "status": "completed",
                           "receiverThreadIds": ["th-2"], "prompt": "Run the tests"}),
                    None,
                ),
            ]
            .concat(),
        ),
        (
            "usage_limit".to_string(),
            vec![
                AgentDomainEvent::AccountLimits {
                    used_percent: 100.0,
                    window_minutes: Some(300),
                    resets_at: Some(1_790_000_000),
                    plan: Some("pro".into()),
                },
                AgentDomainEvent::TurnCompleted {
                    turn_id: "t1".into(),
                    outcome: limit_outcome,
                    result_text: String::new(),
                    stop_reason: None,
                    usage: None,
                    detail: limit_detail,
                },
            ],
        ),
    ];

    let mut written_cases = Vec::new();
    for (name, tail) in cases {
        let mut events = vec![
            AgentDomainEvent::SessionOpened {
                session_id: "sess-1".into(),
                provider_session_id: "prov-1".into(),
                model: "gpt-5-codex".into(),
                cwd: "/project".into(),
            },
            AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
            AgentDomainEvent::UserPromptSubmitted {
                text: "fix the parser".into(),
            },
        ];
        events.extend(tail);
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
        "writtenBy": "core/src/agent_bridge.rs, tests::the_codex_cards_fixture_is_what_the_serializer_sends \
                      -- compared on every run; EITRI_WRITE_FIXTURES=1 rewrites it",
        "cases": written_cases,
    });
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/codex-cards.json");
    let written = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
    let committed = std::fs::read_to_string(&path).ok();
    if committed.as_deref() == Some(written.as_str()) {
        return;
    }
    if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
        std::fs::write(&path, &written).expect("write the codex cards fixture");
    } else {
        assert_eq!(
            committed.as_deref(),
            Some(written.as_str()),
            "the committed codex cards fixture is not what this test produces; if the change is \
             intended, regenerate it with EITRI_WRITE_FIXTURES=1"
        );
    }
}

/// `agent-ui/web/src/fixtures/steered-messages.json`: a Codex turn into which one message went and was
/// read (a user prompt where the agent read it) and another was dropped (nothing in the conversation),
/// as the events this side sends and the snapshot of the same projection, and the queue envelopes the
/// lines above the box are drawn from. The panel's tests fold the events with its own reducer and must
/// arrive at the snapshot. Compared on every run; `EITRI_WRITE_FIXTURES=1` rewrites it.
#[test]
fn the_steered_messages_fixture_is_what_the_serializer_sends() {
    use crate::tab_set::{Queued, QueuedState};
    let events = vec![
        AgentDomainEvent::SessionOpened {
            session_id: "codex-1".into(),
            provider_session_id: "thread-1".into(),
            model: "gpt-test".into(),
            cwd: "/project".into(),
        },
        AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
        AgentDomainEvent::UserPromptSubmitted {
            text: "fix the parser".into(),
        },
        AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: agent::ContentKind::Text,
            text: "Looking at the parser.".into(),
        },
        AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "item-1".into(),
            name: "commandExecution".into(),
            input: json!({ "command": "cargo test -p parser" }),
        },
        AgentDomainEvent::ToolCallCompleted {
            turn_id: "t1".into(),
            tool_use_id: "item-1".into(),
            content: json!("ok"),
            is_error: false,
        },
        AgentDomainEvent::UserMessageDelivered {
            client_message_id: "eitri-1-1".into(),
            turn_id: "t1".into(),
            text: "and the tests".into(),
        },
        AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: agent::ContentKind::Text,
            text: "Adding tests too.".into(),
        },
        AgentDomainEvent::UserMessageDropped {
            client_message_id: "eitri-1-2".into(),
            reason: "the turn ended before Codex read it".into(),
        },
        AgentDomainEvent::TurnCompleted {
            turn_id: "t1".into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: "Adding tests too.".into(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        },
    ];
    let mut projection = AgentSessionProjection::default();
    for event in &events {
        projection.apply(event);
    }
    let view = SnapshotView {
        backend: "external",
        conversation_id: None,
        session_id: Some("codex-1"),
        provider_session_id: Some("thread-1".into()),
        capabilities: agent::ProviderCapabilities {
            interrupt: true,
            steer: true,
            ..Default::default()
        },
        provider: None,
        projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
        hidden_pending: None,
    };
    let events_payload = serialize_events_for_js(TabId(1), 0, projection.last_revision, &events);
    let snapshot_payload = serialize_snapshot_for_js(TabId(1), &view, None);
    let mut steered = Queued::held("look at the lexer too", "ctx\n\nlook at the lexer too".into(), 20);
    steered.state = QueuedState::Steered {
        client_id: "eitri-1-3".into(),
        sent_at_ms: 21,
    };
    let running_queue = serialize_queue_for_js(
        TabId(1),
        &[steered, Queued::held("then commit", "ctx\n\nthen commit".into(), 22)],
        None,
        Some("held until the turn ends: the turn ended before Codex read it"),
        Some(true),
    );
    let steered_at = |text: &str, wire: &str, at: u64| {
        let mut item = Queued::held(text, wire.into(), at);
        item.state = QueuedState::Steered {
            client_id: format!("eitri-1-{at}"),
            sent_at_ms: at,
        };
        item
    };
    let commands_queue = serialize_queue_for_js(
        TabId(1),
        &[
            steered_at("look at the lexer too", "ctx\n\nlook at the lexer too", 20),
            Queued::held("/compact", "/compact".into(), 21),
            steered_at("then commit", "ctx\n\nthen commit", 22),
        ],
        None,
        Some("commands run when the turn ends"),
        Some(true),
    );
    let fixture = json!({
        "writtenBy": "core/src/agent_bridge.rs, tests::the_steered_messages_fixture_is_what_the_serializer_sends \
                      -- compared on every run; EITRI_WRITE_FIXTURES=1 rewrites it",
        "events": serde_json::from_str::<Value>(&events_payload).unwrap(),
        "snapshot": serde_json::from_str::<Value>(&snapshot_payload).unwrap(),
        "queue": serde_json::from_str::<Value>(&running_queue).unwrap(),
        "commandQueue": serde_json::from_str::<Value>(&commands_queue).unwrap(),
    });
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/steered-messages.json");
    let written = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
    let committed = std::fs::read_to_string(&path).ok();
    if committed.as_deref() == Some(written.as_str()) {
        return;
    }
    if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
        std::fs::write(&path, &written).expect("write the steered messages fixture");
    } else {
        assert_eq!(
            committed.as_deref(),
            Some(written.as_str()),
            "the committed steered messages fixture is not what this test produces; if the change is \
             intended, regenerate it with EITRI_WRITE_FIXTURES=1"
        );
    }
}

/// `agent-ui/web/src/fixtures/changed-input.json`: a call whose CLI prompt carried another input
/// than the gate saw, as the events payload this side sends and the snapshot after folding the
/// same events, which the panel's tests fold and render. Compared on every run;
/// `EITRI_WRITE_FIXTURES=1` rewrites it.
#[test]
fn the_changed_input_fixture_is_what_the_serializer_sends() {
    let ask = |permission_id: &str, input: Value, from_cli: bool| AgentDomainEvent::PermissionRequested {
        permission_id: permission_id.into(),
        tool_use_id: Some("toolu_1".into()),
        tool_name: "Bash".into(),
        input,
        provider_prompt: from_cli.then(agent::ProviderPrompt::default),
    };
    let events = vec![
        AgentDomainEvent::SessionOpened {
            session_id: "sess-1".into(),
            provider_session_id: "prov-1".into(),
            model: "claude-sonnet-5".into(),
            cwd: "/project".into(),
        },
        AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
        AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_1".into(),
            name: "Bash".into(),
            input: json!({ "command": "git push --force origin main" }),
        },
        ask("g1", json!({ "command": "git push --force origin main" }), false),
        AgentDomainEvent::PermissionResolved {
            permission_id: "g1".into(),
            outcome: agent::PermissionOutcome::Allowed,
        },
        // A hook rewrote the command, and a character the eye cannot see went in with it.
        ask(
            "c1",
            json!({ "command": "rtk git push --force origin main\u{202e}" }),
            true,
        ),
    ];
    // What ingestion does: fold each event, then the one the projection derives from it. And what
    // the host does before the page sees any of it: the gate request was answered without a card
    // (a deferral in an Auto tab), so it and its resolution never reach the page -- the page gets
    // the derived `tool_input_changed` and nothing to compare.
    let mut projection = AgentSessionProjection::default();
    let mut delivered = Vec::new();
    for event in events {
        let derived = projection.apply_with_derived(&event);
        let answered_by_the_host = matches!(&event, AgentDomainEvent::PermissionRequested { permission_id, .. } if permission_id == "g1")
            || matches!(&event, AgentDomainEvent::PermissionResolved { permission_id, .. } if permission_id == "g1");
        if !answered_by_the_host {
            delivered.push(event);
        }
        delivered.extend(derived);
    }
    let events = delivered;
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
    let fixture = json!({
        "writtenBy": "core/src/agent_bridge.rs, tests::the_changed_input_fixture_is_what_the_serializer_sends \
                      -- compared on every run; EITRI_WRITE_FIXTURES=1 rewrites it",
        "events": serde_json::from_str::<Value>(&events_payload).unwrap(),
        "snapshot": serde_json::from_str::<Value>(&snapshot_payload).unwrap(),
    });
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/changed-input.json");
    let written = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
    let committed = std::fs::read_to_string(&path).ok();
    if committed.as_deref() == Some(written.as_str()) {
        return;
    }
    if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
        std::fs::write(&path, &written).expect("write the changed input fixture");
    } else {
        assert_eq!(
            committed.as_deref(),
            Some(written.as_str()),
            "the committed changed input fixture is not what this test produces; if the change is \
             intended, regenerate it with EITRI_WRITE_FIXTURES=1"
        );
    }
}

/// `agent-ui/web/src/fixtures/history-records.json`: turns the CLI started with every kind of
/// cause (and one of each other origin), one notice of every kind, and question sets left waiting
/// and settled each way, as the events this side sends, the snapshot taken partway (a question
/// still waiting in it) and the snapshot after them all. The panel's tests fold the events with
/// its own reducer, from the start and from the middle snapshot, and must arrive at the last
/// snapshot. Compared on every run; `EITRI_WRITE_FIXTURES=1` rewrites it.
#[test]
fn the_history_records_fixture_is_what_the_serializer_sends() {
    use agent::{
        NoticeLevel, QuestionOptionSpec, QuestionOutcome, QuestionSpec, SessionNoticeKind, ShownAnswer, TurnCause,
        TurnCauseKind, TurnOrigin,
    };
    let completed = |turn_id: &str| AgentDomainEvent::TurnCompleted {
        turn_id: turn_id.into(),
        outcome: agent::TurnOutcome::Completed,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
        detail: Default::default(),
    };
    let cause = |kind: TurnCauseKind| {
        let mut cause = TurnCause::of_kind(kind);
        match kind {
            TurnCauseKind::Task => {
                cause.task_id = Some("task-1".into());
                cause.tool_use_id = Some("tool-1".into());
                cause.task_kind = Some("shell".into());
                cause.task_status = Some("completed".into());
                cause.description = Some("cargo build --release".into());
                cause.summary = Some("Background command \"cargo build\" completed (exit code 0)".into());
            }
            TurnCauseKind::Scheduled => {
                cause.wakeup_source = Some("loop_wakeup".into());
                cause.scheduled_for_ms = Some(1_790_000_000_000);
                cause.schedule_reason = Some("check the deploy".into());
                cause.cron_id = Some("cron-3".into());
                cause.prompt = Some("Is the deploy green?".into());
            }
            TurnCauseKind::Peer => {
                cause.peer_name = Some("reviewer".into());
                cause.text = Some("Look at \u{202e}line 4\u{200b} please".into());
            }
            TurnCauseKind::Other => cause.text = Some("coordinator".into()),
            TurnCauseKind::Unknown => {}
        }
        cause
    };
    let mut events = vec![AgentDomainEvent::SessionOpened {
        session_id: "sess-1".into(),
        provider_session_id: "prov-1".into(),
        model: "claude-sonnet-5".into(),
        cwd: "/project".into(),
    }];
    // A turn the user sent, attributed as theirs: no row.
    events.push(AgentDomainEvent::UserPromptSubmitted { text: "start".into() });
    events.push(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    events.push(AgentDomainEvent::TurnAttributed {
        turn_id: "t1".into(),
        origin: TurnOrigin::Client,
        cause: None,
    });
    events.push(completed("t1"));
    let kinds = [
        TurnCauseKind::Task,
        TurnCauseKind::Scheduled,
        TurnCauseKind::Peer,
        TurnCauseKind::Other,
        TurnCauseKind::Unknown,
    ];
    for (n, kind) in kinds.into_iter().enumerate() {
        let turn = format!("cli-{n}");
        events.push(AgentDomainEvent::TurnStarted { turn_id: turn.clone() });
        events.push(AgentDomainEvent::TurnAttributed {
            turn_id: turn.clone(),
            origin: TurnOrigin::Cli,
            cause: Some(cause(kind)),
        });
        events.push(AgentDomainEvent::ContentDelta {
            turn_id: turn.clone(),
            kind: agent::ContentKind::Text,
            text: format!("Reply {n}."),
        });
        events.push(completed(&turn));
    }
    // A follow-up turn, which names no cause.
    events.push(AgentDomainEvent::TurnStarted {
        turn_id: "follow-1".into(),
    });
    events.push(AgentDomainEvent::TurnAttributed {
        turn_id: "follow-1".into(),
        origin: TurnOrigin::FollowUp,
        cause: None,
    });
    events.push(completed("follow-1"));
    let notice = |kind: SessionNoticeKind, level: NoticeLevel, content: Option<&str>| AgentDomainEvent::SessionNotice {
        kind,
        level,
        content: content.map(str::to_string),
        scalars: Default::default(),
        task_id: None,
        tool_use_id: None,
        task_status: None,
    };
    events.push(AgentDomainEvent::SessionNotice {
        kind: SessionNoticeKind::Task,
        level: NoticeLevel::Notice,
        content: Some("Background command \"cargo build\" completed (exit code 0)".into()),
        scalars: [("exit_code".to_string(), json!(0))].into_iter().collect(),
        task_id: Some("task-1".into()),
        tool_use_id: Some("toolu_bg".into()),
        task_status: Some("completed".into()),
    });
    events.push(notice(
        SessionNoticeKind::ModelFallback,
        NoticeLevel::Warning,
        Some("Switched to claude-sonnet-5"),
    ));
    events.push(notice(
        SessionNoticeKind::ModelRefusal,
        NoticeLevel::Warning,
        Some("The model refused"),
    ));
    // No words: worded from its figures.
    events.push(AgentDomainEvent::SessionNotice {
        kind: SessionNoticeKind::Compaction,
        level: NoticeLevel::Info,
        content: None,
        scalars: [
            ("pre_tokens".to_string(), json!(182_000)),
            ("post_tokens".to_string(), json!(41_000)),
            ("trigger".to_string(), json!("auto")),
        ]
        .into_iter()
        .collect(),
        task_id: None,
        tool_use_id: None,
        task_status: None,
    });
    events.push(notice(
        SessionNoticeKind::Informational,
        NoticeLevel::Suggestion,
        Some("Try /compact"),
    ));
    events.push(notice(
        SessionNoticeKind::HookBlock,
        NoticeLevel::Warning,
        Some("blocked by policy"),
    ));
    events.push(AgentDomainEvent::SessionNotice {
        kind: SessionNoticeKind::PeerHold,
        level: NoticeLevel::Warning,
        content: None,
        scalars: [
            ("peer_name".to_string(), json!("reviewer")),
            ("hold_state".to_string(), json!("held")),
        ]
        .into_iter()
        .collect(),
        task_id: None,
        tool_use_id: None,
        task_status: None,
    });
    events.push(notice(
        SessionNoticeKind::RateLimit,
        NoticeLevel::Notice,
        Some("Usage limit reset"),
    ));
    events.push(notice(SessionNoticeKind::UnseenTurn, NoticeLevel::Notice, None));
    let questions = |text: &str, multi: bool| {
        vec![QuestionSpec {
            question: text.into(),
            header: "Target".into(),
            multi_select: multi,
            options: vec![
                QuestionOptionSpec {
                    label: "\"debug\"".into(),
                    description: "fast to build".into(),
                    preview: None,
                },
                QuestionOptionSpec {
                    label: "release\u{200b}".into(),
                    description: "slow to build".into(),
                    preview: Some("```\ncargo build --release\n```".into()),
                },
            ],
        }]
    };
    events.push(AgentDomainEvent::TurnStarted { turn_id: "t2".into() });
    events.push(AgentDomainEvent::QuestionAsked {
        interaction_id: "q-answered".into(),
        tool_use_id: Some("toolu_q1".into()),
        questions: questions("Which build?", false),
        unsupported_reason: None,
    });
    events.push(AgentDomainEvent::QuestionAsked {
        interaction_id: "q-waiting".into(),
        tool_use_id: Some("toolu_q2".into()),
        questions: questions("Which targets?", true),
        unsupported_reason: None,
    });
    // The middle snapshot is taken here: both questions wait in it.
    let split = events.len();
    events.push(AgentDomainEvent::QuestionSettled {
        interaction_id: "q-answered".into(),
        outcome: QuestionOutcome::Answered,
        answers: vec![ShownAnswer {
            question_index: 0,
            answer: "release\u{200b}".into(),
            notes: Some("ship it".into()),
        }],
        reason: None,
    });
    // A second settlement for the same id changes nothing.
    events.push(AgentDomainEvent::QuestionSettled {
        interaction_id: "q-answered".into(),
        outcome: QuestionOutcome::Expired,
        answers: vec![],
        reason: None,
    });
    events.push(AgentDomainEvent::QuestionAsked {
        interaction_id: "q-refused".into(),
        tool_use_id: None,
        questions: vec![],
        unsupported_reason: Some("five options in one question".into()),
    });
    events.push(AgentDomainEvent::QuestionSettled {
        interaction_id: "q-refused".into(),
        outcome: QuestionOutcome::HostRefused,
        answers: vec![],
        reason: Some("five options in one question".into()),
    });
    for (id, outcome) in [
        ("q-declined", QuestionOutcome::Declined),
        ("q-chat", QuestionOutcome::ChatRequested),
        ("q-interrupted", QuestionOutcome::Interrupted),
        ("q-expired", QuestionOutcome::Expired),
        ("q-closed", QuestionOutcome::SessionClosed),
        ("q-failed", QuestionOutcome::ProviderFailed),
    ] {
        events.push(AgentDomainEvent::QuestionAsked {
            interaction_id: id.into(),
            tool_use_id: Some(format!("toolu_{id}")),
            questions: questions("Go on?", false),
            unsupported_reason: None,
        });
        events.push(AgentDomainEvent::QuestionSettled {
            interaction_id: id.into(),
            outcome,
            answers: vec![],
            reason: (outcome == QuestionOutcome::ProviderFailed).then(|| "the sidecar exited".into()),
        });
    }
    events.push(completed("t2"));
    // Asked between turns and left waiting.
    events.push(AgentDomainEvent::QuestionAsked {
        interaction_id: "q-last".into(),
        tool_use_id: Some("toolu_last".into()),
        questions: questions("Still there?", false),
        unsupported_reason: None,
    });

    let view_of = |projection: &AgentSessionProjection| {
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: Some("sess-1"),
            provider_session_id: Some("prov-1".into()),
            capabilities: agent::ProviderCapabilities {
                interrupt: true,
                ..Default::default()
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(projection),
            hidden_pending: None,
        };
        serde_json::from_str::<Value>(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap()
    };
    let mut projection = AgentSessionProjection::default();
    let mut middle = Value::Null;
    for (at, event) in events.iter().enumerate() {
        if at == split {
            middle = view_of(&projection);
        }
        projection.apply(event);
    }
    assert_eq!(
        projection.turn_starts.len(),
        6,
        "five causes and the follow-up; the user's turn has none"
    );
    assert_eq!(projection.notices.len(), 9, "one notice of every kind");
    assert!(matches!(
        projection.questions[0].state,
        agent::QuestionState::Settled {
            outcome: QuestionOutcome::Answered,
            ..
        }
    ));
    assert_eq!(projection.questions[0].turn_id.as_deref(), Some("t2"));
    assert_eq!(projection.questions.last().unwrap().turn_id, None);
    let events_payload = serialize_events_for_js(TabId(1), 0, projection.last_revision, &events);
    let fixture = json!({
        "writtenBy": "core/src/agent_bridge.rs, tests::the_history_records_fixture_is_what_the_serializer_sends \
                      -- compared on every run; EITRI_WRITE_FIXTURES=1 rewrites it",
        "split": split,
        "events": serde_json::from_str::<Value>(&events_payload).unwrap(),
        "middle": middle,
        "snapshot": view_of(&projection),
    });
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/history-records.json");
    let written = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
    let committed = std::fs::read_to_string(&path).ok();
    if committed.as_deref() == Some(written.as_str()) {
        return;
    }
    if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
        std::fs::write(&path, &written).expect("write the history records fixture");
    } else {
        assert_eq!(
            committed.as_deref(),
            Some(written.as_str()),
            "the committed history records fixture is not what this test produces; if the change is \
             intended, regenerate it with EITRI_WRITE_FIXTURES=1"
        );
    }
}

/// A conversation with none of the three collections sends none of their keys, so every snapshot
/// written before they existed is still what this side sends for such a conversation.
#[test]
fn a_snapshot_without_turn_starts_notices_or_questions_sends_none_of_their_keys() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    projection.apply(&AgentDomainEvent::TurnAttributed {
        turn_id: "t1".into(),
        origin: agent::TurnOrigin::Client,
        cause: None,
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
    for key in ["turnStarts", "notices", "questions", "cliUnseenBusy"] {
        assert!(
            parsed["state"].get(key).is_none(),
            "{key} sent for a conversation with none"
        );
    }
}

/// `agent-ui/web/src/fixtures/unseen-turns.json`: what a turn's end does in every combination of
/// a turn running or not, result text or not, and its own turn or another; a prompt a hook
/// stopped and a slash command that answered without a reply; the note of work out of sight and
/// its end; and a prompt taken back. Each case is the events this side delivers (the ones the
/// projection derives included, as ingestion delivers them) and the snapshot after them, which
/// the panel's tests fold and load and must find equal. Compared on every run;
/// `EITRI_WRITE_FIXTURES=1` rewrites it.
#[test]
fn the_unseen_turns_fixture_is_what_the_serializer_sends() {
    use agent::{ContentKind, SessionNoticeKind, TurnOutcome};
    let opened = AgentDomainEvent::SessionOpened {
        session_id: "sess-1".into(),
        provider_session_id: "prov-1".into(),
        model: "claude-sonnet-5".into(),
        cwd: "/project".into(),
    };
    let prompt = |text: &str| AgentDomainEvent::UserPromptSubmitted { text: text.into() };
    let started = |turn: &str| AgentDomainEvent::TurnStarted { turn_id: turn.into() };
    let text = |turn: &str, text: &str| AgentDomainEvent::ContentDelta {
        turn_id: turn.into(),
        kind: ContentKind::Text,
        text: text.into(),
    };
    let ended = |turn: &str, outcome: TurnOutcome, result_text: &str| AgentDomainEvent::TurnCompleted {
        turn_id: turn.into(),
        outcome,
        result_text: result_text.into(),
        stop_reason: None,
        usage: None,
        detail: Default::default(),
    };
    // A turn of the user's own that is still running, and one that has ended.
    let running = vec![
        opened.clone(),
        prompt("fix the parser"),
        started("t1"),
        text("t1", "Looking at it"),
    ];
    let mut finished = running.clone();
    finished.push(ended("t1", TurnOutcome::Completed, "Looking at it"));
    let with = |base: &[AgentDomainEvent], more: Vec<AgentDomainEvent>| {
        let mut events = base.to_vec();
        events.extend(more);
        events
    };
    let cases: Vec<(&str, Vec<AgentDomainEvent>)> = vec![
        (
            "foreign_while_running_with_text",
            with(
                &running,
                vec![
                    ended(
                        "unknown-turn",
                        TurnOutcome::Completed,
                        "The tests passed in the background.",
                    ),
                    text("t1", " and the fix is in"),
                ],
            ),
        ),
        (
            "foreign_while_running_without_text",
            with(&running, vec![ended("unknown-turn", TurnOutcome::Failed, "")]),
        ),
        (
            "foreign_none_running_with_text",
            with(
                &finished,
                vec![ended("unknown-turn", TurnOutcome::Failed, "The build is green.")],
            ),
        ),
        (
            "foreign_none_running_without_text",
            with(&finished, vec![ended("unknown-turn", TurnOutcome::Failed, "")]),
        ),
        ("own_turn_with_text", finished.clone()),
        (
            "own_turn_without_text",
            with(&running, vec![ended("t1", TurnOutcome::Completed, "")]),
        ),
        (
            "own_turn_failed_with_text",
            with(&running, vec![ended("t1", TurnOutcome::Failed, "API Error: 500")]),
        ),
        (
            "hook_blocked_prompt",
            vec![
                opened.clone(),
                prompt("deploy to production"),
                started("t1"),
                ended(
                    "t1",
                    TurnOutcome::Completed,
                    "Blocked by the deploy guard: production deploys need a ticket\u{202e}.",
                ),
            ],
        ),
        (
            "compact_answers_without_a_reply",
            vec![
                opened.clone(),
                prompt("/compact"),
                started("t1"),
                ended("t1", TurnOutcome::Completed, "Compacted. ctrl+o to see full summary"),
            ],
        ),
        (
            "cli_work_unseen",
            with(&finished, vec![AgentDomainEvent::CliWorkUnseen]),
        ),
        (
            "cli_work_unseen_then_its_end",
            with(
                &finished,
                vec![
                    AgentDomainEvent::CliWorkUnseen,
                    ended("unknown-turn", TurnOutcome::Completed, ""),
                ],
            ),
        ),
        (
            "cli_work_unseen_then_the_session_closes",
            with(
                &finished,
                vec![
                    AgentDomainEvent::CliWorkUnseen,
                    AgentDomainEvent::SessionClosed {
                        reason: "closed".into(),
                    },
                ],
            ),
        ),
        (
            "cli_work_unseen_then_the_session_is_lost",
            with(
                &finished,
                vec![
                    AgentDomainEvent::CliWorkUnseen,
                    AgentDomainEvent::SessionUnavailable {
                        reason: "the sidecar exited".into(),
                    },
                ],
            ),
        ),
        (
            "cli_work_unseen_then_a_resume_that_did_not_attach",
            with(
                &finished,
                vec![
                    AgentDomainEvent::CliWorkUnseen,
                    AgentDomainEvent::ResumeOutcome {
                        requested_provider_session_id: "prov-0".into(),
                        status: agent::ResumeStatus::Rejected,
                        attached_provider_session_id: None,
                        forked: false,
                        detail: None,
                    },
                ],
            ),
        ),
        (
            "withdrawn_after_an_undelivered_prompt",
            with(
                &finished,
                vec![prompt("and the docs"), AgentDomainEvent::UserPromptWithdrawn],
            ),
        ),
        (
            "withdrawn_after_a_delivered_one",
            vec![
                opened.clone(),
                started("t1"),
                AgentDomainEvent::UserMessageDelivered {
                    client_message_id: "eitri-1-1".into(),
                    turn_id: "t1".into(),
                    text: "and the docs".into(),
                },
                AgentDomainEvent::UserPromptWithdrawn,
            ],
        ),
        (
            "withdrawn_with_none",
            vec![opened.clone(), AgentDomainEvent::UserPromptWithdrawn],
        ),
    ];

    let mut written_cases = Vec::new();
    for (name, events) in cases {
        let mut projection = AgentSessionProjection::default();
        let mut delivered = Vec::new();
        for event in events {
            let derived = projection.apply_with_derived(&event);
            delivered.push(event);
            delivered.extend(derived);
        }
        let kinds: Vec<_> = projection.notices.iter().map(|n| n.kind).collect();
        match name {
            "foreign_while_running_with_text" => {
                assert_eq!(projection.active_turn_id.as_deref(), Some("t1"), "{name}");
                assert_eq!(kinds, [SessionNoticeKind::UnseenTurn], "{name}");
                assert_eq!(projection.transcript.len(), 3, "{name}: the answer stands apart");
            }
            "foreign_while_running_without_text" => {
                assert_eq!(projection.active_turn_id.as_deref(), Some("t1"), "{name}");
                assert!(kinds.is_empty() && projection.turn_endings.is_empty(), "{name}");
            }
            "foreign_none_running_with_text" => {
                assert_eq!(kinds, [SessionNoticeKind::UnseenTurn], "{name}");
                assert!(projection.turn_endings.is_empty(), "{name}: no failed row");
            }
            "foreign_none_running_without_text" => {
                assert!(kinds.is_empty() && projection.turn_endings.is_empty(), "{name}");
            }
            "own_turn_failed_with_text" => {
                assert!(kinds.is_empty(), "{name}");
                assert_eq!(projection.turn_endings.len(), 1, "{name}");
            }
            "hook_blocked_prompt" => assert_eq!(kinds, [SessionNoticeKind::HookBlock], "{name}"),
            "cli_work_unseen" => assert!(projection.cli_unseen_busy, "{name}"),
            "cli_work_unseen_then_its_end"
            | "cli_work_unseen_then_the_session_closes"
            | "cli_work_unseen_then_the_session_is_lost"
            | "cli_work_unseen_then_a_resume_that_did_not_attach" => {
                assert!(!projection.cli_unseen_busy, "{name}")
            }
            "withdrawn_after_an_undelivered_prompt" => assert_eq!(projection.user_prompts.len(), 1, "{name}"),
            "withdrawn_after_a_delivered_one" => assert_eq!(projection.user_prompts.len(), 1, "{name}"),
            _ => assert!(kinds.is_empty(), "{name}"),
        }
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: Some("sess-1"),
            provider_session_id: Some("prov-1".into()),
            capabilities: agent::ProviderCapabilities {
                interrupt: true,
                ..Default::default()
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
            hidden_pending: None,
        };
        let events_payload = serialize_events_for_js(TabId(1), 0, projection.last_revision, &delivered);
        written_cases.push(json!({
            "name": name,
            "events": serde_json::from_str::<Value>(&events_payload).unwrap(),
            "snapshot": serde_json::from_str::<Value>(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap(),
        }));
    }
    let fixture = json!({
        "writtenBy": "core/src/agent_bridge.rs, tests::the_unseen_turns_fixture_is_what_the_serializer_sends \
                      -- compared on every run; EITRI_WRITE_FIXTURES=1 rewrites it",
        "cases": written_cases,
    });
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/unseen-turns.json");
    let written = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
    let committed = std::fs::read_to_string(&path).ok();
    if committed.as_deref() == Some(written.as_str()) {
        return;
    }
    if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
        std::fs::write(&path, &written).expect("write the unseen turns fixture");
    } else {
        assert_eq!(
            committed.as_deref(),
            Some(written.as_str()),
            "the committed unseen turns fixture is not what this test produces; if the change is \
             intended, regenerate it with EITRI_WRITE_FIXTURES=1"
        );
    }
}

/// `agent-ui/web/src/fixtures/cli-turns.json`: turns Claude Code started by itself, with each kind of
/// cause and a cause the close refined; a prompt Claude Code read only after work of its own, delivered
/// and dropped; endings without a result, empty and not. Each case is the events this side delivers
/// (derived ones included), a snapshot taken partway (`split` events in) and the snapshot after them
/// all. The panel's tests fold the events, from the start and from the middle snapshot, and load the
/// last snapshot, and must find the same page each way. Compared on every run;
/// `EITRI_WRITE_FIXTURES=1` rewrites it.
#[test]
fn the_cli_turns_fixture_is_what_the_serializer_sends() {
    use agent::{ContentKind, TurnCause, TurnCauseKind, TurnEndDetail, TurnOrigin, TurnOutcome};
    let opened = AgentDomainEvent::SessionOpened {
        session_id: "sess-1".into(),
        provider_session_id: "prov-1".into(),
        model: "claude-sonnet-5".into(),
        cwd: "/project".into(),
    };
    let prompt = |text: &str| AgentDomainEvent::UserPromptSubmitted { text: text.into() };
    let started = |turn: &str| AgentDomainEvent::TurnStarted { turn_id: turn.into() };
    let cli = |turn: &str, cause: Option<TurnCause>| AgentDomainEvent::TurnAttributed {
        turn_id: turn.into(),
        origin: TurnOrigin::Cli,
        cause,
    };
    let text = |turn: &str, text: &str| AgentDomainEvent::ContentDelta {
        turn_id: turn.into(),
        kind: ContentKind::Text,
        text: text.into(),
    };
    let ended = |turn: &str, outcome: TurnOutcome, reason: Option<&str>| AgentDomainEvent::TurnCompleted {
        turn_id: turn.into(),
        outcome,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
        detail: TurnEndDetail {
            reason: reason.map(str::to_string),
            origin_kind: Some("task-notification".into()),
            num_turns: Some(1),
            ..Default::default()
        },
    };
    let done = |turn: &str| ended(turn, TurnOutcome::Completed, None);
    let task = || {
        let mut cause = TurnCause::of_kind(TurnCauseKind::Task);
        cause.task_id = Some("bzng1ikb4".into());
        cause.task_kind = Some("shell".into());
        cause.task_status = Some("completed".into());
        cause.description = Some("cargo test".into());
        cause.summary = Some("Background command \"cargo test\" completed (exit code 0)".into());
        cause
    };
    let scheduled = |reason: Option<&str>| {
        let mut cause = TurnCause::of_kind(TurnCauseKind::Scheduled);
        cause.wakeup_source = Some("loop_wakeup".into());
        cause.scheduled_for_ms = Some(1_790_000_000_000);
        cause.schedule_reason = reason.map(str::to_string);
        cause
    };
    let peer = || {
        let mut cause = TurnCause::of_kind(TurnCauseKind::Peer);
        cause.peer_name = Some("reviewer".into());
        cause.text = Some("Please look at \u{202e}line 4\u{200b}".into());
        cause
    };
    let cli_turn = |turn: &str, cause: TurnCause, reply: &str| {
        vec![started(turn), cli(turn, Some(cause)), text(turn, reply), done(turn)]
    };
    let with = |base: Vec<AgentDomainEvent>, more: Vec<AgentDomainEvent>| {
        let mut events = base;
        events.extend(more);
        events
    };
    // The user's own turn, finished, before each case.
    let before = vec![
        opened.clone(),
        prompt("fix the parser"),
        started("t1"),
        text("t1", "Fixed."),
        done("t1"),
    ];
    let base = before.len();
    let delivered_prompt = AgentDomainEvent::UserMessageDelivered {
        client_message_id: "eitri-1-2".into(),
        turn_id: "t2".into(),
        text: "and the docs".into(),
    };
    let cycle = AgentDomainEvent::CliCycleStarted {
        turn_id: "t2".into(),
        cause: Some(task()),
    };
    // (name, events, split: how many events the middle snapshot has folded)
    let cases: Vec<(&str, Vec<AgentDomainEvent>, usize)> = vec![
        (
            "task_cause",
            with(before.clone(), cli_turn("c1", task(), "The tests passed.")),
            base + 2,
        ),
        (
            "unknown_cause_updated_at_the_close",
            with(
                before.clone(),
                vec![
                    started("c1"),
                    cli("c1", Some(TurnCause::of_kind(TurnCauseKind::Unknown))),
                    text("c1", "Done."),
                    AgentDomainEvent::TurnCauseUpdated {
                        turn_id: "c1".into(),
                        cause: Some(task()),
                    },
                    // No news: the cause stays.
                    AgentDomainEvent::TurnCauseUpdated {
                        turn_id: "c1".into(),
                        cause: None,
                    },
                    done("c1"),
                ],
            ),
            base + 3,
        ),
        (
            "scheduled_with_a_reason",
            with(
                before.clone(),
                cli_turn("c1", scheduled(Some("check the deploy")), "Green."),
            ),
            base + 2,
        ),
        (
            "scheduled_without_a_reason",
            with(before.clone(), cli_turn("c1", scheduled(None), "Green.")),
            base + 2,
        ),
        (
            "three_empty_scheduled_turns",
            with(
                before.clone(),
                ["c1", "c2", "c3"]
                    .into_iter()
                    .flat_map(|turn| vec![started(turn), cli(turn, Some(scheduled(None))), done(turn)])
                    .collect(),
            ),
            base + 4,
        ),
        (
            "peer_with_its_body",
            with(before.clone(), cli_turn("c1", peer(), "Looked.")),
            base + 2,
        ),
        (
            "prompt_read_after_work_of_its_own",
            with(
                before.clone(),
                vec![
                    prompt("and the docs"),
                    started("t2"),
                    cycle.clone(),
                    text("t2", "The build finished."),
                    delivered_prompt.clone(),
                    text("t2", "Docs updated."),
                    done("t2"),
                ],
            ),
            base + 2,
        ),
        (
            "prompt_dropped_after_work_of_its_own",
            with(
                before.clone(),
                vec![
                    prompt("and the docs"),
                    started("t2"),
                    cycle.clone(),
                    text("t2", "The build finished."),
                    AgentDomainEvent::UserMessageDropped {
                        client_message_id: "eitri-1-2".into(),
                        reason: "Claude Code cancelled it".into(),
                    },
                    done("t2"),
                ],
            ),
            base + 3,
        ),
        (
            "a_cli_turn_ending_off_screen",
            with(before.clone(), cli_turn("c1", task(), "The tests passed.")),
            base + 3,
        ),
        (
            "no_result_after_a_reply",
            with(
                before.clone(),
                vec![
                    started("c1"),
                    cli("c1", Some(task())),
                    text("c1", "Looking"),
                    ended("c1", TurnOutcome::Failed, Some(agent::NO_RESULT_REASON)),
                ],
            ),
            base + 3,
        ),
        (
            "empty_no_result",
            with(
                before.clone(),
                vec![
                    started("c1"),
                    cli("c1", Some(task())),
                    ended("c1", TurnOutcome::Failed, Some(agent::NO_RESULT_REASON)),
                ],
            ),
            base + 2,
        ),
    ];

    let view_of = |projection: &AgentSessionProjection| {
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: Some("sess-1"),
            provider_session_id: Some("prov-1".into()),
            capabilities: agent::ProviderCapabilities {
                interrupt: true,
                ..Default::default()
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(projection),
            hidden_pending: None,
        };
        serde_json::from_str::<Value>(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap()
    };
    let mut written_cases = Vec::new();
    for (name, events, split) in cases {
        let mut projection = AgentSessionProjection::default();
        let mut attention = crate::attention::AttentionTracker::default();
        let mut delivered = Vec::new();
        let mut middle = Value::Null;
        let mut middle_at = 0;
        for (at, event) in events.into_iter().enumerate() {
            if at == split {
                middle = view_of(&projection);
                middle_at = delivered.len();
            }
            // The user's own turn before the case was read.
            if at == base {
                attention.seen();
            }
            let derived = projection.apply_with_derived(&event);
            let mut batch = vec![event];
            batch.extend(derived);
            // Off screen throughout, so every end that is news marks the tab.
            attention.observe(&batch, false);
            delivered.extend(batch);
        }
        let prompts: Vec<_> = projection.user_prompts.iter().map(|p| p.text.as_str()).collect();
        let starts: Vec<_> = projection
            .turn_starts
            .iter()
            .map(|s| (s.turn_id.as_str(), s.cause.as_ref().map(|c| c.kind)))
            .collect();
        assert_eq!(projection.active_turn_id, None, "{name}: every case ends its turns");
        match name {
            "task_cause" | "a_cli_turn_ending_off_screen" | "peer_with_its_body" => {
                assert_eq!(starts.len(), 1, "{name}");
                assert!(attention.attention().unread, "{name}: its end off screen is news");
            }
            "unknown_cause_updated_at_the_close" => {
                assert_eq!(
                    starts,
                    [("c1", Some(TurnCauseKind::Task))],
                    "{name}: the close's cause wins"
                )
            }
            "three_empty_scheduled_turns" => {
                assert_eq!(starts.len(), 3, "{name}: an empty turn that completed keeps its row");
                assert!(projection.turn_endings.is_empty(), "{name}");
            }
            "prompt_read_after_work_of_its_own" => {
                assert_eq!(prompts, ["fix the parser", "and the docs"], "{name}");
                assert_eq!(starts, [("t2", Some(TurnCauseKind::Task))], "{name}");
                let moved = projection.user_prompts.last().unwrap();
                let woken = projection
                    .transcript
                    .iter()
                    .find(|m| m.text == "The build finished.")
                    .unwrap();
                assert!(
                    moved.delivered && moved.seq > woken.seq,
                    "{name}: read after the woken work"
                );
            }
            "prompt_dropped_after_work_of_its_own" => {
                assert_eq!(
                    prompts,
                    ["fix the parser"],
                    "{name}: the tab holds it, not the conversation"
                );
                assert_eq!(
                    projection.steer_outcome("eitri-1-2"),
                    Some(&agent::SteerOutcome::Dropped {
                        reason: "Claude Code cancelled it".into()
                    }),
                    "{name}"
                );
            }
            "no_result_after_a_reply" => {
                assert_eq!(starts.len(), 1, "{name}");
                assert_eq!(projection.turn_endings.len(), 1, "{name}");
                assert_eq!(
                    projection.turn_endings[0].detail.reason.as_deref(),
                    Some(agent::NO_RESULT_REASON),
                    "{name}"
                );
                assert_eq!(projection.turn_endings[0].detail.num_turns, Some(1), "{name}");
                assert!(attention.attention().unread, "{name}");
            }
            "empty_no_result" => {
                assert!(starts.is_empty(), "{name}: no row says why it started");
                assert!(projection.turn_endings.is_empty(), "{name}: no ending row");
                assert_eq!(projection.last_turn_ending, None, "{name}");
                assert!(!attention.attention().unread, "{name}: nothing to read");
            }
            _ => assert_eq!(starts.len(), 1, "{name}"),
        }
        let events_payload = serialize_events_for_js(TabId(1), 0, projection.last_revision, &delivered);
        written_cases.push(json!({
            "name": name,
            "split": middle_at,
            "events": serde_json::from_str::<Value>(&events_payload).unwrap(),
            "middle": middle,
            "snapshot": view_of(&projection),
        }));
    }
    let fixture = json!({
        "writtenBy": "core/src/agent_bridge.rs, tests::the_cli_turns_fixture_is_what_the_serializer_sends \
                      -- compared on every run; EITRI_WRITE_FIXTURES=1 rewrites it",
        "cases": written_cases,
    });
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/cli-turns.json");
    let written = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
    let committed = std::fs::read_to_string(&path).ok();
    if committed.as_deref() == Some(written.as_str()) {
        return;
    }
    if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
        std::fs::write(&path, &written).expect("write the CLI turns fixture");
    } else {
        assert_eq!(
            committed.as_deref(),
            Some(written.as_str()),
            "the committed CLI turns fixture is not what this test produces; if the change is \
             intended, regenerate it with EITRI_WRITE_FIXTURES=1"
        );
    }
}
