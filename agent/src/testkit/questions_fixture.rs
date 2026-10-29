use super::*;
use crate::{QuestionOptionSpec, QuestionSpec, ShownAnswer};

fn asked(id: &str) -> AgentDomainEvent {
    AgentDomainEvent::QuestionAsked {
        interaction_id: id.into(),
        tool_use_id: Some(format!("tool-{id}")),
        questions: vec![
            QuestionSpec {
                question: "Which targets?".into(),
                header: "Targets".into(),
                multi_select: true,
                options: vec![
                    QuestionOptionSpec {
                        label: "\"fast\", local".into(),
                        description: "A quoted item".into(),
                        preview: None,
                    },
                    QuestionOptionSpec {
                        label: "remote\u{202e}\u{200b}".into(),
                        description: "Hidden characters remain text".into(),
                        preview: None,
                    },
                ],
            },
            QuestionSpec {
                question: "Which patch?".into(),
                header: "Preview".into(),
                multi_select: false,
                options: vec![
                    QuestionOptionSpec {
                        label: "Apply".into(),
                        description: "Use this patch".into(),
                        preview: Some("```diff\n+keep this\n```".into()),
                    },
                    QuestionOptionSpec {
                        label: "Leave".into(),
                        description: "Keep the current file".into(),
                        preview: Some("No edits".into()),
                    },
                ],
            },
        ],
        unsupported_reason: None,
    }
}

fn settled(id: &str, outcome: QuestionOutcome) -> AgentDomainEvent {
    AgentDomainEvent::QuestionSettled {
        interaction_id: id.into(),
        outcome,
        answers: if matches!(outcome, QuestionOutcome::Answered | QuestionOutcome::ChatRequested) {
            vec![
                ShownAnswer {
                    question_index: 0,
                    answer: "\"fast\", local, remote\u{202e}\u{200b}".into(),
                    notes: None,
                },
                ShownAnswer {
                    question_index: 1,
                    answer: "Apply".into(),
                    notes: Some("Keep \"this\" line\u{200b}".into()),
                },
            ]
        } else {
            vec![]
        },
        reason: Some(format!("ended: {outcome:?}")),
    }
}

fn case(name: &str, actions: Vec<Value>) -> Value {
    let mut projection = AgentSessionProjection::default();
    let mut steps = vec![];
    for action in actions {
        if let Some(event) = action.get("event") {
            projection.apply(&serde_json::from_value::<AgentDomainEvent>(event.clone()).unwrap());
        } else {
            let id = action["interactionId"].as_str().unwrap();
            let changed = match action["send"].as_str().unwrap() {
                "answering" => projection.mark_question_answering(id),
                "failed" => projection.question_send_failed(id),
                other => panic!("unknown send action: {other}"),
            };
            assert_eq!(changed, action["changed"].as_bool().unwrap());
        }
        let mut snapshot = json!({ "activeTurnId": projection.active_turn_id });
        snapshot_json(&projection, &mut snapshot);
        steps.push(json!({
            "action": action, "snapshot": snapshot, "throughRevision": projection.last_revision,
            "waitingIds": projection.waiting_questions().map(|record| &record.interaction_id).collect::<Vec<_>>(),
        }));
    }
    json!({ "name": name, "steps": steps })
}

fn event(event: AgentDomainEvent) -> Value {
    json!({ "event": event })
}
fn send(id: &str, action: &str, changed: bool) -> Value {
    json!({ "interactionId": id, "send": action, "changed": changed })
}

#[test]
fn questions_fixture() {
    let outcomes = [
        QuestionOutcome::Answered,
        QuestionOutcome::Declined,
        QuestionOutcome::ChatRequested,
        QuestionOutcome::Interrupted,
        QuestionOutcome::Expired,
        QuestionOutcome::SessionClosed,
        QuestionOutcome::ProviderFailed,
        QuestionOutcome::HostRefused,
    ];
    let mut cases = vec![];
    for outcome in outcomes {
        cases.push(case(
            &format!("outcome-{outcome:?}"),
            vec![
                event(AgentDomainEvent::TurnStarted { turn_id: "turn".into() }),
                event(asked("q")),
                send("q", "answering", true),
                event(settled("q", outcome)),
                event(settled("q", QuestionOutcome::Answered)),
                send("q", "failed", false),
                send("q", "answering", false),
            ],
        ));
    }
    let mut replacement = asked("first");
    if let AgentDomainEvent::QuestionAsked {
        tool_use_id, questions, ..
    } = &mut replacement
    {
        *tool_use_id = Some("replacement-tool".into());
        questions[0].question = "Which revised targets?".into();
    }
    cases.push(case(
        "send-retry-and-arrival-order",
        vec![
            event(asked("first")),
            event(asked("second")),
            send("first", "answering", true),
            send("first", "answering", false),
            send("first", "failed", true),
            send("first", "failed", false),
            send("missing", "answering", false),
            send("missing", "failed", false),
            event(settled("missing", QuestionOutcome::Expired)),
            event(replacement),
            send("first", "answering", true),
            event(asked("first")),
            event(settled("first", QuestionOutcome::Answered)),
            event(asked("first")),
        ],
    ));
    let mut refused = asked("refused");
    if let AgentDomainEvent::QuestionAsked {
        questions,
        unsupported_reason,
        ..
    } = &mut refused
    {
        questions.clear();
        *unsupported_reason = Some("This question set has too many options".into());
    }
    cases.push(case(
        "refused-set-waits-for-settlement",
        vec![event(refused), event(settled("refused", QuestionOutcome::HostRefused))],
    ));
    for ending in [
        AgentDomainEvent::SessionClosed {
            reason: "closed".into(),
        },
        AgentDomainEvent::SessionUnavailable { reason: "lost".into() },
    ] {
        cases.push(case(
            &format!("{ending:?}"),
            vec![
                event(asked("waiting")),
                event(asked("answering")),
                send("answering", "answering", true),
                event(asked("settled")),
                event(settled("settled", QuestionOutcome::Declined)),
                event(ending),
                event(settled("waiting", QuestionOutcome::Answered)),
            ],
        ));
    }
    cases.push(case(
        "turn-end-leaves-question-live",
        vec![
            event(AgentDomainEvent::TurnStarted { turn_id: "turn".into() }),
            event(asked("q")),
            event(AgentDomainEvent::TurnCompleted {
                turn_id: "turn".into(),
                outcome: crate::TurnOutcome::Completed,
                usage: None,
                result_text: String::new(),
                stop_reason: None,
                detail: crate::TurnEndDetail::default(),
            }),
        ],
    ));
    let turn_completed = |turn: &str| AgentDomainEvent::TurnCompleted {
        turn_id: turn.into(),
        outcome: crate::TurnOutcome::Completed,
        usage: None,
        result_text: String::new(),
        stop_reason: None,
        detail: crate::TurnEndDetail::default(),
    };
    let woken = |turn: &str| {
        event(AgentDomainEvent::TurnAttributed {
            turn_id: turn.into(),
            origin: crate::TurnOrigin::Cli,
            cause: None,
        })
    };
    let ask_call = |turn: &str| {
        event(AgentDomainEvent::ToolCallStarted {
            turn_id: turn.into(),
            tool_use_id: "tool-q".into(),
            name: "AskUserQuestion".into(),
            input: json!({}),
        })
    };
    // The CLI asks before it announces the turn it woke itself for: the question is that turn's, whether
    // the turn before is still running when it arrives or has ended.
    cases.push(case(
        "question-asked-before-the-woken-turn-is-announced",
        vec![
            event(AgentDomainEvent::TurnStarted {
                turn_id: "first".into(),
            }),
            event(asked("q")),
            event(turn_completed("first")),
            event(AgentDomainEvent::TurnStarted {
                turn_id: "woken".into(),
            }),
            woken("woken"),
            ask_call("woken"),
            event(settled("q", QuestionOutcome::Answered)),
        ],
    ));
    cases.push(case(
        "question-asked-between-turns-before-the-woken-turn-is-announced",
        vec![
            event(AgentDomainEvent::TurnStarted {
                turn_id: "first".into(),
            }),
            event(turn_completed("first")),
            event(asked("q")),
            event(AgentDomainEvent::TurnStarted {
                turn_id: "woken".into(),
            }),
            woken("woken"),
            ask_call("woken"),
            event(settled("q", QuestionOutcome::Answered)),
        ],
    ));
    cases.push(case(
        "question-asked-after-the-woken-turn-is-announced",
        vec![
            event(AgentDomainEvent::TurnStarted {
                turn_id: "first".into(),
            }),
            event(turn_completed("first")),
            event(AgentDomainEvent::TurnStarted {
                turn_id: "woken".into(),
            }),
            woken("woken"),
            ask_call("woken"),
            event(asked("q")),
            event(settled("q", QuestionOutcome::Answered)),
        ],
    ));
    // Questions that wait in arrival order keep it when one of them is moved to its turn.
    cases.push(case(
        "a-moved-question-keeps-the-record-in-seq-order",
        vec![
            event(asked("q")),
            event(asked("other")),
            event(AgentDomainEvent::TurnStarted {
                turn_id: "woken".into(),
            }),
            woken("woken"),
            ask_call("woken"),
        ],
    ));
    for (index, outcome) in outcomes.into_iter().enumerate() {
        let steps = &cases[index]["steps"];
        assert_eq!(
            steps[1]["snapshot"]["questions"][0]["state"],
            json!({ "kind": "waiting" })
        );
        assert_eq!(
            steps[2]["snapshot"]["questions"][0]["state"],
            json!({ "kind": "answering" })
        );
        let AgentDomainEvent::QuestionSettled { answers, reason, .. } = settled("q", outcome) else {
            unreachable!()
        };
        assert_eq!(
            steps[3]["snapshot"]["questions"][0]["state"],
            json!({
                "kind": "settled", "outcome": outcome,
                "answers": answers.iter().map(|answer| json!({
                    "questionIndex": answer.question_index, "answer": answer.answer, "notes": answer.notes,
                })).collect::<Vec<_>>(), "reason": reason,
            })
        );
        assert_eq!(steps[3]["snapshot"], steps[4]["snapshot"]);
    }
    let retry = &cases[8]["steps"];
    assert_eq!(
        retry[4]["snapshot"]["questions"][0]["state"],
        json!({ "kind": "waiting" })
    );
    assert_eq!(retry[9]["waitingIds"], json!(["second", "first"]));
    assert_eq!(retry[9]["snapshot"]["questions"][1]["toolUseId"], "replacement-tool");
    assert_eq!(
        retry[9]["snapshot"]["questions"][1]["questions"][0]["question"],
        "Which revised targets?"
    );
    assert_eq!(retry[9]["snapshot"]["questions"][1]["seq"], 3);
    assert_eq!(retry[11]["snapshot"]["questions"].as_array().unwrap().len(), 2);
    assert_eq!(cases[12]["steps"][2]["waitingIds"], json!(["q"]));
    let running = &cases[13]["steps"];
    let between = &cases[14]["steps"];
    let after = &cases[15]["steps"];
    assert_eq!(running[1]["snapshot"]["questions"][0]["turnId"], "first");
    assert_eq!(between[2]["snapshot"]["questions"][0]["turnId"], Value::Null);
    // Folded after the announcement, as the CLI's file has it: the question is in the woken turn, at its call.
    for (steps, seq) in [(running, 5), (between, 5), (after, 5)] {
        assert_eq!(steps[5]["snapshot"]["questions"][0]["turnId"], "woken");
        assert_eq!(steps[5]["snapshot"]["questions"][0]["seq"], seq);
    }
    assert_eq!(running[5]["snapshot"]["questions"], after[5]["snapshot"]["questions"]);
    let moved = &cases[16]["steps"][4]["snapshot"]["questions"];
    assert_eq!(moved[0]["interactionId"], "other");
    assert_eq!(moved[1]["interactionId"], "q");
    assert_eq!(moved[1]["turnId"], "woken");
    for closed in &cases[10..12] {
        let records = closed["steps"][5]["snapshot"]["questions"].as_array().unwrap();
        assert_eq!(records[0]["state"]["outcome"], "session_closed");
        assert_eq!(records[1]["state"]["outcome"], "session_closed");
        assert_eq!(records[2]["state"]["outcome"], "declined");
    }
    let fixture = json!({ "writtenBy": "agent/src/testkit/questions_fixture.rs::questions_fixture", "cases": cases });
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/questions.json");
    let written = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
    if std::env::var("EITRI_WRITE_FIXTURES").as_deref() == Ok("1") {
        std::fs::write(&path, &written).unwrap();
    } else {
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            written,
            "regenerate with EITRI_WRITE_FIXTURES=1"
        );
    }
}
