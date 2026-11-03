//! The review settings and display envelopes, thinking blocks, and the events/snapshot twins.
#![cfg(test)]

use super::*;

#[test]
fn the_review_settings_envelope_carries_both_switches_by_name() {
    for (enabled, hint) in [(false, false), (true, false), (false, true), (true, true)] {
        let parsed: Value = serde_json::from_str(&serialize_review_settings_for_js(enabled, hint)).unwrap();
        assert_eq!(
            parsed,
            json!({ "kind": "review_settings", "enabled": enabled, "hint": hint })
        );
    }
}

#[test]
fn the_display_envelope_names_the_thinking_level_in_the_words_init_lua_uses() {
    use crate::agent_prefs::ThinkingDisplay;
    for (display, word) in [
        (ThinkingDisplay::Collapsed, "collapsed"),
        (ThinkingDisplay::Preview, "preview"),
        (ThinkingDisplay::Expanded, "expanded"),
        (ThinkingDisplay::Hidden, "hidden"),
    ] {
        let parsed: Value = serde_json::from_str(&serialize_display_for_js(display)).unwrap();
        assert_eq!(parsed, json!({ "kind": "display", "thinking": word }));
    }
}

/// A projection in the middle of a thought sends the block and says it is still open, so a panel
/// rebuilt from it neither splits the thought nor draws it as finished.
#[test]
fn a_snapshot_mid_thought_carries_the_block_and_says_it_is_open() {
    let state_of = |projection: &AgentSessionProjection| -> Value {
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: Some("sess-1"),
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities::default(),
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(projection),
            hidden_pending: None,
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        parsed["state"].clone()
    };
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    for chunk in ["First ", "the parser."] {
        projection.apply(&AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: agent::ContentKind::Thinking,
            text: chunk.into(),
        });
    }
    let state = state_of(&projection);
    assert_eq!(
        state["thinking"],
        json!([{ "seq": 1, "turnId": "t1", "text": "First the parser." }])
    );
    assert_eq!(state["thinkingOpen"], true);
    // Thinking is not a reply: nothing of it is in the transcript.
    assert_eq!(state["transcript"], json!([]));

    projection.apply(&AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: agent::ContentKind::Text,
        text: "Done.".into(),
    });
    let state = state_of(&projection);
    assert_eq!(state["thinkingOpen"], false);
    assert_eq!(state["thinking"][0]["text"], "First the parser.");
}

/// `agent-ui/web/src/fixtures/thinking.json`: for each way thinking arrives, the events payload this
/// side sends, the snapshot after a prefix of them (`split`), and the snapshot after all of them. The
/// panel's reducer must fold the events to what the last snapshot carries, from nothing and from the
/// middle one. Compared on every run; `EITRI_WRITE_FIXTURES=1` rewrites it.
#[test]
fn the_thinking_fixture_is_what_the_serializer_sends() {
    use agent::{ContentKind, TurnEndDetail, TurnOutcome};
    let thought = |text: &str| AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: ContentKind::Thinking,
        text: text.into(),
    };
    let said = |text: &str| AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: ContentKind::Text,
        text: text.into(),
    };
    let call = |id: &str| AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: id.into(),
        name: "Read".into(),
        input: json!({ "file_path": "/project/src/parser.rs" }),
    };
    let done = |id: &str| AgentDomainEvent::ToolCallCompleted {
        turn_id: "t1".into(),
        tool_use_id: id.into(),
        content: json!("fn parse() {}"),
        is_error: false,
    };
    let ended = AgentDomainEvent::TurnCompleted {
        turn_id: "t1".into(),
        outcome: TurnOutcome::Completed,
        result_text: "Fixed.".into(),
        stop_reason: None,
        usage: None,
        detail: TurnEndDetail::default(),
    };
    // (name, events after the turn starts, how many of ALL the events the middle snapshot folds)
    let cases: Vec<(&str, Vec<AgentDomainEvent>, usize)> = vec![
        // Thinking between calls and before the reply, streamed in pieces; the middle snapshot lands
        // inside the second thought.
        (
            "interleaved",
            vec![
                thought("Read the "),
                thought("parser first."),
                call("toolu_1"),
                done("toolu_1"),
                thought("The loop "),
                thought("never ends.\nFix the bound."),
                said("Fixed."),
                ended,
            ],
            7,
        ),
        // The turn is still thinking when the middle snapshot is taken, and goes on after it.
        (
            "open_at_the_snapshot",
            vec![thought("One"), thought(" two"), thought(" three")],
            3,
        ),
        // A delta with no text is no block and ends none.
        (
            "empty_delta",
            vec![thought(""), thought("a"), thought(""), thought("b"), said("ok")],
            4,
        ),
        // A thought arriving in the middle of a reply does not cut the reply in two.
        (
            "thought_inside_a_reply",
            vec![said("Half "), thought("hmm"), said("and half."), thought("after")],
            4,
        ),
    ];

    let view_of = |projection: &AgentSessionProjection| -> Value {
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: Some("sess-1"),
            provider_session_id: Some("prov-1".into()),
            capabilities: agent::ProviderCapabilities::default(),
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(projection),
            hidden_pending: None,
        };
        serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap()
    };
    let mut written_cases = Vec::new();
    for (name, rest, split) in cases {
        let mut events = vec![
            AgentDomainEvent::UserPromptSubmitted {
                text: "fix the parser".into(),
            },
            AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
        ];
        events.extend(rest);
        assert!(split <= events.len(), "{name}");
        let mut projection = AgentSessionProjection::default();
        let mut middle = Value::Null;
        for (at, event) in events.iter().enumerate() {
            if at == split {
                middle = view_of(&projection);
            }
            projection.apply(event);
        }
        if split == events.len() {
            middle = view_of(&projection);
        }
        let events_payload = serialize_events_for_js(TabId(1), 0, projection.last_revision, &events);
        written_cases.push(json!({
            "name": name,
            "split": split,
            "events": serde_json::from_str::<Value>(&events_payload).unwrap(),
            "middle": middle,
            "snapshot": view_of(&projection),
        }));
    }
    let fixture = json!({
        "writtenBy": "core/src/agent_bridge.rs, tests::the_thinking_fixture_is_what_the_serializer_sends \
                      -- compared on every run; EITRI_WRITE_FIXTURES=1 rewrites it",
        "cases": written_cases,
    });
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/thinking.json");
    let written = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
    let committed = std::fs::read_to_string(&path).ok();
    if committed.as_deref() == Some(written.as_str()) {
        return;
    }
    if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
        std::fs::write(&path, &written).expect("write the thinking fixture");
    } else {
        assert_eq!(
            committed.as_deref(),
            Some(written.as_str()),
            "the committed thinking fixture is not what this test produces; if the change is \
             intended, regenerate it with EITRI_WRITE_FIXTURES=1"
        );
    }
}

#[test]
fn session_notices_events_and_snapshot_are_twins() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/session-notices.json");
    let mut fixture: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let original = fixture.clone();
    for case in fixture["cases"].as_array_mut().unwrap() {
        let events: Vec<AgentDomainEvent> = serde_json::from_value(case["domain_events"].clone()).unwrap();
        let mut projection = AgentSessionProjection::default();
        for event in &events {
            projection.apply(event);
        }
        // History A keeps rows, independently of live activity and band state.
        let stored = agent::history::StoredHistory::from_parts(
            "notices".into(),
            "session".into(),
            agent::history::HistoryParts {
                notices: projection.notices.clone(),
                ..Default::default()
            },
        );
        let restored: agent::history::StoredHistory =
            serde_json::from_str(&serde_json::to_string(&stored).unwrap()).unwrap();
        assert_eq!(restored.notices, projection.notices);
        let mut refolded = AgentSessionProjection::default();
        for record in restored.notices {
            refolded.apply(&AgentDomainEvent::SessionNotice {
                kind: record.kind,
                level: record.level,
                content: record.content,
                scalars: record.scalars,
                task_id: record.task_id,
                tool_use_id: record.tool_use_id,
                task_status: record.task_status,
            });
        }
        assert_eq!(refolded.notices.len(), projection.notices.len());
        for (restored, original) in refolded.notices.iter().zip(&projection.notices) {
            assert_eq!(restored.kind, original.kind);
            assert_eq!(restored.content, original.content);
            assert_eq!(restored.scalars, original.scalars);
        }
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
        case["snapshot"] = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        case["events"] =
            serde_json::from_str(&serialize_events_for_js(TabId(1), 0, projection.last_revision, &events)).unwrap();
    }
    if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
        std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap())).unwrap();
    } else {
        assert_eq!(original, fixture);
    }
}

/// `fixtures/conversation-reset.json`: each case's events folded here, the events payload this side
/// sends for them and the snapshot it sends after them, which the panel's reducer must agree with
/// (`reducer.test.ts`). The snapshot is taken as it is once the new conversation's session opened,
/// with that session's id. `EITRI_WRITE_FIXTURES=1` rewrites the generated parts.
#[test]
fn conversation_reset_events_and_snapshot_are_twins() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/conversation-reset.json");
    let mut fixture: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let original = fixture.clone();
    for case in fixture["cases"].as_array_mut().unwrap() {
        let events: Vec<AgentDomainEvent> = serde_json::from_value(case["domain_events"].clone()).unwrap();
        let mut projection = AgentSessionProjection::default();
        for event in &events {
            projection.apply(event);
        }
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: projection.session_id.as_deref(),
            provider_session_id: projection.provider_session_id.clone(),
            capabilities: agent::ProviderCapabilities::default(),
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
            hidden_pending: None,
        };
        case["snapshot"] = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        case["events"] =
            serde_json::from_str(&serialize_events_for_js(TabId(1), 0, projection.last_revision, &events)).unwrap();
    }
    if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
        std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap())).unwrap();
    } else {
        assert_eq!(original, fixture);
    }
}

#[test]
fn questions_fixture_matches_events_and_canonical_snapshots() {
    let fixture: Value =
        serde_json::from_str(include_str!("../../../../agent-ui/web/src/fixtures/questions.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let mut projection = AgentSessionProjection::default();
        for step in case["steps"].as_array().unwrap() {
            let action = &step["action"];
            if let Some(event) = action.get("event") {
                let event: AgentDomainEvent = serde_json::from_value(event.clone()).unwrap();
                let payload: Value = serde_json::from_str(&serialize_events_for_js(
                    TabId(1),
                    projection.last_revision,
                    projection.last_revision + 1,
                    std::slice::from_ref(&event),
                ))
                .unwrap();
                assert_eq!(payload["events"][0], action["event"]);
                projection.apply(&event);
            } else {
                let id = action["interactionId"].as_str().unwrap();
                let changed = match action["send"].as_str().unwrap() {
                    "answering" => projection.mark_question_answering(id),
                    "failed" => projection.question_send_failed(id),
                    other => panic!("unknown send action: {other}"),
                };
                assert_eq!(changed, action["changed"].as_bool().unwrap());
            }
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
            let canonical: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
            assert_eq!(canonical["throughRevision"], step["throughRevision"]);
            assert_eq!(
                canonical["state"]["questions"], step["snapshot"]["questions"],
                "{}",
                case["name"]
            );
            assert_eq!(canonical["state"]["activeTurnId"], step["snapshot"]["activeTurnId"]);
            assert_eq!(
                serde_json::to_value(
                    projection
                        .waiting_questions()
                        .map(|q| &q.interaction_id)
                        .collect::<Vec<_>>()
                )
                .unwrap(),
                step["waitingIds"]
            );
        }
    }
}

#[test]
fn background_tasks_events_and_snapshots_are_twins() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/background-tasks.json");
    let mut fixture: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let write = std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1");
    for case in fixture["cases"].as_array_mut().unwrap() {
        let name = case["name"].as_str().unwrap().to_owned();
        let source: Vec<AgentDomainEvent> = serde_json::from_value(case["domain_events"].clone()).unwrap();
        let mut projection = AgentSessionProjection::default();
        let mut delivered = Vec::new();
        let mut snapshots = Vec::new();
        let mut batches = Vec::new();
        for (index, event) in source.iter().enumerate() {
            let from = projection.last_revision;
            let old_count = projection.background_tasks.running();
            let derived = projection.apply_with_derived(event);
            let count = projection.background_tasks.running();
            assert_eq!(
                count as u64,
                case["expected_running"][index].as_u64().unwrap(),
                "{name} event {index}"
            );
            assert_eq!(projection.background_calls, count, "{name} event {index}");
            assert_eq!(derived.len(), usize::from(old_count != count), "{name} event {index}");
            let mut batch = vec![event.clone()];
            batch.extend(derived);
            delivered.extend(batch.iter().cloned());
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
            let snapshot: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
            snapshots.push(snapshot);
            let payload: Value = serde_json::from_str(&serialize_events_for_js(
                TabId(1),
                from,
                projection.last_revision,
                &batch,
            ))
            .unwrap();
            batches.push(payload);
        }
        assert_eq!(
            projection.notices.len() as u64,
            case["expected_notice_count"].as_u64().unwrap(),
            "{name}"
        );
        let rows = serde_json::to_value(&projection.background_tasks.rows).unwrap();
        let expected_rows = case["expected_rows"].as_array().unwrap();
        assert_eq!(rows.as_array().unwrap().len(), expected_rows.len(), "{name}");
        for (row, expected) in rows.as_array().unwrap().iter().zip(expected_rows) {
            for (key, value) in expected.as_object().unwrap() {
                assert_eq!(&row[key], value, "{name} {key}");
            }
        }
        let payload: Value = serde_json::from_str(&serialize_events_for_js(
            TabId(1),
            0,
            projection.last_revision,
            &delivered,
        ))
        .unwrap();
        if write {
            case["events"] = payload;
            case["batches"] = json!(batches);
            case["snapshots"] = json!(snapshots);
        } else {
            assert_eq!(case["events"], payload, "{name}");
            assert_eq!(case["batches"], json!(batches), "{name}");
            assert_eq!(case["snapshots"], json!(snapshots), "{name}");
        }
    }
    if write {
        std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap())).unwrap();
    }
}

#[test]
fn subagent_frames_events_and_snapshots_are_twins() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/subagent-frames.json");
    let mut fixture: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let write = std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1");
    for case in fixture["cases"].as_array_mut().unwrap() {
        let events: Vec<AgentDomainEvent> = serde_json::from_value(case["events"].clone()).unwrap();
        let mut projection = AgentSessionProjection::default();
        for event in &events {
            projection.apply(event);
        }
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
        let snapshot: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        if write {
            case["snapshot"] = snapshot;
        } else {
            assert_eq!(case["snapshot"], snapshot, "{}", case["name"]);
        }
    }
    if write {
        std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap())).unwrap();
    }
}

#[test]
fn protocol_322_events_and_snapshot_are_twins() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/protocol-322.json");
    let committed = std::fs::read_to_string(&path).unwrap();
    let fixture: Value = serde_json::from_str(&committed).unwrap();
    let events: Vec<AgentDomainEvent> = serde_json::from_value(fixture["domain_events"].clone()).unwrap();
    let mut projection = AgentSessionProjection::default();
    for event in &events {
        projection.apply(event);
    }
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
    let snapshot: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
    let payload: Value =
        serde_json::from_str(&serialize_events_for_js(TabId(1), 0, projection.last_revision, &events)).unwrap();
    let expected = json!({ "domain_events": fixture["domain_events"], "events": payload, "snapshot": snapshot });
    if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
        std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(&expected).unwrap())).unwrap();
    } else {
        assert_eq!(fixture, expected);
    }
}

/// `agent-ui/web/src/fixtures/claude-scripted-<scenario>.json`: the events of a scripted Claude scenario, as
/// the harness records them with every string the CLI wrote reduced to a digest (`domain_events`), the
/// events payload this side sends for them and the snapshot it sends after folding them. The panel's
/// reducer must fold the payload to what the snapshot carries (`claudeScripted.test.ts`). The harness
/// writes `domain_events` and `scenario`; this test adds the rest. Compared on every run;
/// `EITRI_WRITE_FIXTURES=1` rewrites each file.
#[test]
fn the_claude_scripted_twins_are_what_the_serializer_sends() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures");
    let mut twins: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .expect("the fixtures directory")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("claude-scripted-") && name.ends_with(".json"))
        })
        .collect();
    twins.sort();
    assert!(!twins.is_empty(), "no claude-scripted twin under {}", dir.display());
    let write = std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1");
    let mut stale = Vec::new();
    for path in twins {
        let committed = std::fs::read_to_string(&path).expect("a twin");
        let twin: Value = serde_json::from_str(&committed).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let source = twin["domain_events"]
            .as_array()
            .unwrap_or_else(|| panic!("{}: no domain_events", path.display()));
        let events: Vec<AgentDomainEvent> = source
            .iter()
            .enumerate()
            .map(|(n, event)| {
                serde_json::from_value(event.clone())
                    .unwrap_or_else(|e| panic!("{}: domain event {n} does not parse: {e}", path.display()))
            })
            .collect();
        let mut projection = AgentSessionProjection::default();
        for event in &events {
            projection.apply(event);
        }
        let snapshot = {
            let view = SnapshotView {
                backend: "sidecar",
                conversation_id: None,
                session_id: Some("sess-1"),
                provider_session_id: Some("prov-1".into()),
                capabilities: agent::ProviderCapabilities::default(),
                provider: None,
                projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
                hidden_pending: None,
            };
            serde_json::from_str::<Value>(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap()
        };
        let payload =
            serde_json::from_str::<Value>(&serialize_events_for_js(TabId(1), 0, projection.last_revision, &events))
                .unwrap();
        let fixture = json!({
            "writtenBy": "core/src/agent_bridge.rs, tests::the_claude_scripted_twins_are_what_the_serializer_sends \
                          -- compared on every run; EITRI_WRITE_FIXTURES=1 rewrites it",
            "scenario": twin["scenario"],
            "domain_events": twin["domain_events"],
            "events": payload,
            "snapshot": snapshot,
        });
        let written = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
        if written == committed {
            continue;
        }
        if write {
            std::fs::write(&path, &written).expect("write a twin");
        } else {
            stale.push(path.display().to_string());
        }
    }
    assert!(
        stale.is_empty(),
        "these twins are not what this test produces; if the change is intended, regenerate them with \
         EITRI_WRITE_FIXTURES=1: {stale:?}"
    );
}
