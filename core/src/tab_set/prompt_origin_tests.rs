//! What a prompt is stamped with when it is sent, queued, steered or restored (typed by the person,
//! composed by Eitri, or a CLI turn's own), and the scan that holds every send and steer caller to naming it.
#![cfg(test)]

use super::*;
use crate::test_providers::RecordingProvider;
use std::time::{Duration, Instant};

struct Fixture {
    dir: PathBuf,
    set: TabSet,
    tab: TabId,
    provider: Arc<RecordingProvider>,
}

impl Fixture {
    fn new(label: &str) -> Self {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir(label);
        let provider = Arc::new(RecordingProvider::steering());
        let conversation =
            agent::AgentConversation::create(provider.clone(), &dir, agent::setting_sources::ProjectTrust::Untrusted)
                .unwrap();
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = set.active();
        set.get_mut(tab).unwrap().backend = TabBackend::Live(AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        });
        Self {
            dir,
            set,
            tab,
            provider,
        }
    }

    fn wait(&mut self, mut done: impl FnMut(&Tab) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            self.set.pump(&self.dir, true);
            if done(self.set.get(self.tab).unwrap()) {
                return;
            }
            assert!(Instant::now() < deadline, "the tab did not reach the expected state");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn prompt(&mut self, text: &str, origin: PromptOrigin, first: bool) {
        let turn = FirstTurn {
            wire: format!("editor context\n\n{text}"),
            typed: text.into(),
            typed_by_person: origin == PromptOrigin::Typed,
            shell_line: false,
            attachments: Vec::new(),
        };
        let sent = if first {
            self.set.send_first_turn(self.tab, "first", turn)
        } else {
            self.set.send_prompt(self.tab, "prompt", ViewId::BASE, turn)
        };
        assert!(matches!(
            sent,
            Some(Sent::First { outcome: Ok(()), .. } | Sent::Prompt { outcome: Ok(()), .. })
        ));
    }

    fn start(&mut self, cli: bool) {
        let mut events = vec![AgentDomainEvent::TurnStarted {
            turn_id: "turn-1".into(),
        }];
        if cli {
            events.push(AgentDomainEvent::TurnAttributed {
                turn_id: "turn-1".into(),
                origin: agent::TurnOrigin::Cli,
                cause: None,
            });
        }
        self.provider.queue_all(events);
        self.wait(Tab::turn_running);
    }

    fn end(&mut self) {
        self.provider.queue(AgentDomainEvent::TurnCompleted {
            turn_id: "turn-1".into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        });
        self.wait(|tab| !tab.turn_running());
    }

    fn cycle(&mut self) -> String {
        let id = self
            .provider
            .turn_stamps()
            .last()
            .unwrap()
            .client_message_id
            .clone()
            .unwrap();
        self.provider.queue(AgentDomainEvent::CliCycleStarted {
            turn_id: "turn-1".into(),
            cause: None,
        });
        self.wait(|tab| tab.session.queue.len() == 1);
        id
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for removed in self.set.take_all() {
            if let Some(retired) = removed.retired {
                retired.into_backend().shutdown();
            }
        }
    }
}

#[test]
fn a_held_steer_goes_into_a_cli_turn() {
    let mut f = Fixture::new("held-steer-cli-turn");
    f.provider.refuse_sends(true);
    f.prompt("and the tests", PromptOrigin::Typed, false);
    assert!(f.set.get(f.tab).unwrap().session.queue[0].steer_when_running);
    f.start(true);
    f.wait(|tab| tab.steered_unconfirmed() == 1);
    let sent = f.provider.steers();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].expected_turn_id, "turn-1");
    assert_eq!(sent[0].as_typed, "and the tests");
    assert!(f.provider.steer_stamps()[0].typed_by_person);
}

#[test]
fn a_case_2_prompt_becomes_a_steered_queue_line() {
    let mut f = Fixture::new("opening-prompt-cli-cycle");
    f.prompt("fix the parser", PromptOrigin::Typed, false);
    f.start(false);
    assert_eq!(
        f.set
            .get(f.tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .user_prompts
            .len(),
        1
    );
    let id = f.cycle();
    let tab = f.set.get(f.tab).unwrap();
    let queue: serde_json::Value = serde_json::from_str(&crate::agent_bridge::serialize_queue_for_js(
        f.tab,
        &tab.session.queue,
        None,
        None,
        tab.steers(),
    ))
    .unwrap();
    assert_eq!(queue["items"].as_array().unwrap().len(), 1);
    assert_eq!(queue["items"][0]["steered"], true);
    assert_eq!(queue["items"][0]["text"], "fix the parser");
    assert!(tab.live().unwrap().projection().user_prompts.is_empty());
    assert!(matches!(&tab.session.queue[0].state, QueuedState::Steered { client_id, .. } if client_id == &id));
    f.provider.queue(AgentDomainEvent::ContentDelta {
        turn_id: "turn-1".into(),
        kind: agent::ContentKind::Text,
        text: "background work first".into(),
    });
    f.provider.queue(AgentDomainEvent::UserMessageDelivered {
        client_message_id: id,
        turn_id: "turn-1".into(),
        text: "fix the parser".into(),
    });
    f.wait(|tab| tab.session.queue.is_empty());
    let projection = f.set.get(f.tab).unwrap().live().unwrap().projection();
    assert_eq!(projection.user_prompts.len(), 1);
    assert_eq!(projection.user_prompts[0].text, "fix the parser");
    assert!(projection.user_prompts[0].delivered);
    assert!(projection.user_prompts[0].seq > projection.transcript[0].seq);
}

#[test]
fn a_dropped_prompt_returns_to_the_queue_with_its_reason() {
    let mut f = Fixture::new("opening-prompt-dropped");
    f.prompt("fix it", PromptOrigin::Typed, false);
    f.start(false);
    let id = f.cycle();
    f.provider.queue(AgentDomainEvent::UserMessageDropped {
        client_message_id: id.clone(),
        reason: "Claude cancelled it".into(),
    });
    f.wait(|tab| tab.session.queue[0].is_held());
    let tab = f.set.get(f.tab).unwrap();
    assert_eq!(tab.session.queue[0].text, "fix it");
    assert_eq!(tab.session.queue[0].dropped_as, [id.clone()]);
    assert_eq!(
        tab.session.queue_note.as_deref(),
        Some("held until the turn ends: Claude cancelled it")
    );
    f.set.steer_held(f.tab, 2, Some("turn-1"));
    assert!(f.provider.steers().is_empty());
    f.provider.queue(AgentDomainEvent::UserMessageDelivered {
        client_message_id: id,
        turn_id: "turn-1".into(),
        text: "fix it".into(),
    });
    f.wait(|tab| tab.session.queue.is_empty());
}

#[test]
fn an_idle_prompt_the_cli_refuses_stays_one_row_and_is_never_sent_again() {
    let mut f = Fixture::new("opening-prompt-refused-idle");
    f.prompt("/unknown-command", PromptOrigin::Typed, false);
    f.start(false);
    let id = f.provider.turn_stamps()[0].client_message_id.clone().unwrap();
    f.provider.queue_all(vec![
        AgentDomainEvent::UserMessageDropped {
            client_message_id: id,
            reason: agent::CLI_REFUSED_DROP.into(),
        },
        AgentDomainEvent::TurnCompleted {
            turn_id: "turn-1".into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        },
    ]);
    let mut ended = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    while ended.is_empty() {
        ended.extend(f.set.pump(&f.dir, true).turn_ended);
        assert!(Instant::now() < deadline, "the turn's end was never reported");
        std::thread::sleep(Duration::from_millis(5));
    }
    let tab = f.set.get(f.tab).unwrap();
    assert!(
        tab.session.queue.is_empty(),
        "a prompt still shown as a row is not also a queue line"
    );
    assert_eq!(tab.live().unwrap().projection().user_prompts.len(), 1);
    assert!(f.set.flush_queue(f.tab).is_none(), "nothing is left to send");
    assert_eq!(f.provider.turn_stamps().len(), 1, "the refused prompt went out once");
}

#[test]
fn a_prompt_the_cli_refuses_after_a_cycle_waits_for_the_person() {
    let mut f = Fixture::new("opening-prompt-refused-cycle");
    f.prompt("/unknown-command", PromptOrigin::Typed, false);
    f.start(false);
    let id = f.cycle();
    f.provider.queue(AgentDomainEvent::UserMessageDropped {
        client_message_id: id,
        reason: agent::CLI_REFUSED_DROP.into(),
    });
    f.wait(|tab| tab.session.queue[0].is_held());
    f.end();
    let tab = f.set.get(f.tab).unwrap();
    assert!(tab.session.queue[0].refused);
    assert_eq!(
        tab.session.queue_note.as_deref(),
        Some("Claude refused it; it is not sent again until you take it back")
    );
    assert!(
        f.set.flush_queue(f.tab).is_none(),
        "a refused message is not sent again"
    );
    assert_eq!(f.provider.turn_stamps().len(), 1);
    assert_eq!(f.set.take_back_queue(f.tab), ["/unknown-command"]);
    assert!(f.set.get(f.tab).unwrap().session.queue.is_empty());
}

#[test]
fn a_refused_message_stays_behind_when_the_rest_of_the_queue_goes() {
    let mut f = Fixture::new("opening-prompt-refused-rest");
    f.prompt("/unknown-command", PromptOrigin::Typed, false);
    f.start(false);
    let id = f.cycle();
    f.provider.queue(AgentDomainEvent::UserMessageDropped {
        client_message_id: id,
        reason: agent::CLI_REFUSED_DROP.into(),
    });
    f.wait(|tab| tab.session.queue[0].is_held());
    f.end();
    f.set
        .queue_message(
            f.tab,
            "and the docs",
            "and the docs".into(),
            2,
            true,
            PromptOrigin::Typed,
        )
        .unwrap();
    let flush = f.set.flush_queue(f.tab).expect("the new message goes out");
    assert_eq!(flush.typed, "and the docs");
    let queue = &f.set.get(f.tab).unwrap().session.queue;
    assert_eq!(queue.len(), 1);
    assert!(queue[0].refused);
}

#[test]
fn a_refused_message_does_not_hold_back_a_steer_typed_after_it() {
    let mut f = Fixture::new("opening-prompt-refused-steer");
    f.prompt("/unknown-command", PromptOrigin::Typed, false);
    f.start(false);
    let id = f.cycle();
    f.provider.queue(AgentDomainEvent::UserMessageDropped {
        client_message_id: id,
        reason: agent::CLI_REFUSED_DROP.into(),
    });
    f.wait(|tab| tab.session.queue[0].is_held());
    assert!(f.set.get(f.tab).unwrap().turn_running());
    f.set
        .queue_message(
            f.tab,
            "and the docs",
            "and the docs".into(),
            2,
            true,
            PromptOrigin::Typed,
        )
        .unwrap();
    f.set.steer_held(f.tab, 3, Some("turn-1"));
    let steers = f.provider.steers();
    assert_eq!(
        steers.len(),
        1,
        "the message typed after the refused one goes into the turn"
    );
    assert_eq!(steers[0].as_typed, "and the docs");
    let tab = f.set.get(f.tab).unwrap();
    assert_eq!(tab.session.queue.len(), 2);
    assert!(tab.session.queue[0].refused && tab.session.queue[0].is_held());
    assert!(matches!(tab.session.queue[1].state, QueuedState::Steered { .. }));
    assert_eq!(
        tab.session.queue_note.as_deref(),
        Some("Claude refused it; it is not sent again until you take it back"),
        "the note still says why the refused message stays"
    );
}

#[test]
fn an_opening_prompt_survives_a_cycle_snapshot_and_resync_without_a_second_copy() {
    for overflow in [false, true] {
        let mut f = Fixture::new("opening-prompt-snapshot");
        f.prompt("keep this prompt", PromptOrigin::Typed, false);
        f.start(false);
        let id = f.provider.turn_stamps()[0].client_message_id.clone().unwrap();
        let mut batch = vec![AgentDomainEvent::CliCycleStarted {
            turn_id: "turn-1".into(),
            cause: None,
        }];
        if overflow {
            batch.extend(
                (0..agent::UI_EVENT_QUEUE_CAPACITY + 10)
                    .map(|_| AgentDomainEvent::CommandsAvailable { commands: vec![] }),
            );
        }
        let through = f.set.get(f.tab).unwrap().live().unwrap().projection().last_revision + batch.len() as u64;
        f.provider.queue_all(batch);
        let deadline = Instant::now() + Duration::from_secs(60);
        while f.set.get(f.tab).unwrap().live().unwrap().projection().last_revision < through {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        if overflow {
            let output = f.set.pump(&f.dir, true);
            let payload: serde_json::Value = serde_json::from_str(output.active_payload.as_deref().unwrap()).unwrap();
            assert_eq!(payload["kind"], "snapshot");
            assert!(output.queue_changed.contains(&f.tab));
        }
        let payloads: Vec<serde_json::Value> = f
            .set
            .active_state_payloads()
            .iter()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        let snapshot = payloads.iter().find(|v| v["kind"] == "snapshot").unwrap();
        let queue = payloads.iter().find(|v| v["kind"] == "queue").unwrap();
        assert!(snapshot["state"]["userPrompts"].as_array().unwrap().is_empty());
        assert_eq!(queue["items"].as_array().unwrap().len(), 1);
        assert_eq!(queue["items"][0]["steered"], true);
        f.set.pump(&f.dir, true);
        assert_eq!(f.set.get(f.tab).unwrap().session.queue.len(), 1);
        f.provider.queue(AgentDomainEvent::UserMessageDelivered {
            client_message_id: id,
            turn_id: "turn-1".into(),
            text: "keep this prompt".into(),
        });
        f.wait(|tab| tab.session.queue.is_empty());
        assert_eq!(
            f.set
                .get(f.tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .user_prompts
                .len(),
            1
        );
    }
}

#[test]
fn composer_send_and_steer_are_typed() {
    for first in [false, true] {
        let mut f = Fixture::new("typed-composer-opening");
        f.prompt("hello", PromptOrigin::Typed, first);
        let stamp = &f.provider.turn_stamps()[0];
        assert!(stamp.typed_by_person);
        assert_eq!(stamp.as_typed.as_deref(), Some("hello"));
        assert!(stamp.client_message_id.is_some());
        f.start(false);
        f.set
            .queue_message(
                f.tab,
                "also this",
                "ctx: also this".into(),
                1,
                true,
                PromptOrigin::Typed,
            )
            .unwrap();
        assert!(f.set.steer_held(f.tab, 2, Some("turn-1")));
        assert!(f.provider.steer_stamps()[0].typed_by_person);
    }
}

#[test]
fn send_now_and_delayed_composer_queue_are_typed() {
    for running in [false, true] {
        let mut f = Fixture::new("typed-send-now");
        if running {
            f.start(true);
        }
        f.set.send_now(f.tab, "send now", "ctx: send now".into(), 1).unwrap();
        if running {
            assert!(f.provider.turn_stamps().is_empty());
            f.end();
            assert!(f.set.flush_queue(f.tab).unwrap().outcome.is_ok());
        }
        assert!(f.provider.turn_stamps()[0].typed_by_person);
    }
    let mut f = Fixture::new("typed-delayed-queue");
    f.start(true);
    f.set
        .queue_message(f.tab, "later", "ctx: later".into(), 1, true, PromptOrigin::Typed)
        .unwrap();
    f.end();
    assert!(f.set.flush_queue(f.tab).unwrap().outcome.is_ok());
    assert!(f.provider.turn_stamps()[0].typed_by_person);
}

#[test]
fn turn_review_and_plugin_sends_are_composed() {
    let mut f = Fixture::new("composed-review");
    assert!(f
        .set
        .send_review(f.tab, "review of the changes", 1)
        .unwrap()
        .unwrap()
        .outcome
        .is_ok());
    assert!(!f.provider.turn_stamps()[0].typed_by_person);
    let mut f = Fixture::new("composed-plugin-send");
    f.prompt("a plugin's prepared prompt", PromptOrigin::Composed, false);
    assert!(!f.provider.turn_stamps()[0].typed_by_person);
    f.start(true);
    f.set
        .queue_message(
            f.tab,
            "prepared follow-up",
            "wire follow-up".into(),
            1,
            true,
            PromptOrigin::Composed,
        )
        .unwrap();
    f.set.steer_held(f.tab, 2, Some("turn-1"));
    assert!(!f.provider.steer_stamps()[0].typed_by_person);
}

#[test]
fn a_queue_containing_a_review_is_not_stamped_as_the_persons_text() {
    let mut f = Fixture::new("mixed-origin-queue");
    f.start(true);
    assert!(f.set.send_review(f.tab, "generated review", 1).unwrap().is_none());
    f.set
        .queue_message(
            f.tab,
            "my follow-up",
            "my follow-up".into(),
            2,
            true,
            PromptOrigin::Typed,
        )
        .unwrap();
    f.end();
    let flush = f.set.flush_queue(f.tab).unwrap();
    assert!(flush.outcome.is_ok());
    assert_eq!(flush.typed, "generated review\n\nmy follow-up");
    assert!(!f.provider.turn_stamps()[0].typed_by_person);
}

#[test]
fn a_restored_queue_is_not_typed() {
    let mut f = Fixture::new("composed-restored-queue");
    f.set
        .queue_message(
            f.tab,
            "a saved message",
            "ctx: saved".into(),
            1,
            true,
            PromptOrigin::Typed,
        )
        .unwrap();
    let json = serde_json::to_string(&f.set.get(f.tab).unwrap().session.queue).unwrap();
    f.set.get_mut(f.tab).unwrap().session.queue = serde_json::from_str(&json).unwrap();
    assert!(f.set.flush_queue(f.tab).unwrap().outcome.is_ok());
    assert!(!f.provider.turn_stamps()[0].typed_by_person);
}

#[test]
fn a_restored_message_the_person_resends_is_typed() {
    let mut f = Fixture::new("restored-queue-resend");
    f.set
        .get_mut(f.tab)
        .unwrap()
        .session
        .queue
        .push(Queued::held("saved", "old context".into(), 1));
    let text = f.set.take_back_queue(f.tab);
    assert_eq!(text, ["saved"]);
    f.prompt(&text[0].text, PromptOrigin::Typed, false);
    assert!(f.provider.turn_stamps()[0].typed_by_person);
}

#[test]
fn a_composer_message_held_behind_a_card_keeps_its_origin() {
    for origin in [PromptOrigin::Typed, PromptOrigin::Composed] {
        let mut f = Fixture::new("prompt-origin-card-wait");
        f.provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: "card".into(),
            tool_use_id: Some("call".into()),
            tool_name: "Write".into(),
            input: serde_json::json!({"file_path": ".git/probe", "content": "x"}),
            provider_prompt: None,
        });
        f.wait(|tab| tab.card_is_waiting("card"));
        f.prompt("after the card", origin, false);
        assert!(f.provider.turn_stamps().is_empty());
        f.provider.queue(AgentDomainEvent::PermissionResolved {
            permission_id: "card".into(),
            outcome: agent::PermissionOutcome::Allowed,
        });
        f.wait(|tab| !tab.card_is_waiting("card"));
        let sent = f.set.release_after_card();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].1.outcome.is_ok());
        assert_eq!(
            f.provider.turn_stamps()[0].typed_by_person,
            origin == PromptOrigin::Typed
        );
    }
}

#[test]
fn ctrl_c_interrupts_a_cli_turn() {
    let mut f = Fixture::new("interrupt-cli-turn");
    f.start(true);
    f.set.get_mut(f.tab).unwrap().live_mut().unwrap().interrupt().unwrap();
    assert_eq!(f.provider.interrupts(), 1);
}

#[test]
fn close_asks_during_a_cli_turn() {
    let mut f = Fixture::new("close-cli-turn");
    f.start(true);
    assert!(f.set.close_needs_confirm(f.tab));
    assert!(f.set.close_facts(f.tab).unwrap().turn_running);
}

#[test]
fn handoff_refused_during_a_cli_turn() {
    let mut f = Fixture::new("handoff-cli-turn");
    f.start(true);
    assert!(f.set.take_for_handoff(f.tab).is_none());
    assert!(f.set.get(f.tab).unwrap().turn_running());
    assert_eq!(f.provider.closes(), 0);
}

#[test]
fn the_queue_holds_during_a_cli_turn() {
    let mut f = Fixture::new("queue-cli-turn");
    f.start(true);
    f.set
        .queue_message(f.tab, "after this", "after this".into(), 1, true, PromptOrigin::Typed)
        .unwrap();
    assert!(f.set.flush_queue(f.tab).is_none());
    assert!(f.provider.turns().is_empty());
    assert_eq!(f.set.queue_texts(f.tab), ["after this"]);
}

fn send_calls(code: &str) -> Vec<String> {
    use crate::source_scan::word_ends;
    let mut found = Vec::new();
    for name in [
        "send_turn",
        "send_turn_in",
        "send_turn_as",
        "send_turn_with",
        "send_turn_stamped",
        "send_turn_spawned",
        "send_turn_as_spawned",
        "steer",
        "steer_turn",
        "steer_turn_stamped",
    ] {
        for end in word_ends(code, name) {
            let before = code[..end - name.len()].trim_end();
            if !(before.ends_with('.') || before.ends_with("::")) || !code[end..].trim_start().starts_with('(') {
                continue;
            }
            let open = end + code[end..].find('(').unwrap();
            let mut depth = 0;
            let close = code[open..]
                .char_indices()
                .find_map(|(at, c)| {
                    match c {
                        '(' => depth += 1,
                        ')' => depth -= 1,
                        _ => {}
                    }
                    (depth == 0).then_some(open + at)
                })
                .expect("balanced call arguments");
            found.push(
                code[end - name.len()..=close]
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect(),
            );
        }
    }
    found.sort();
    found
}

#[test]
fn every_send_caller_names_its_origin() {
    use crate::source_scan::{code_only, modules, require_anchors, rust_sources, without, word_ends};
    let sources = rust_sources(&["core/src", "panel/src", "shell/src"]);
    require_anchors(
        &sources,
        &[
            "fn send_one",
            "fn release_turn_send",
            "fn send_turn_in",
            "fn steer_held",
        ],
    );
    let test_only = crate::source_scan::test_only_files(&sources);
    for file in ["core/src/tab_set/move_tests.rs", "core/src/tab_set/tests/bypass.rs"] {
        assert!(
            test_only.contains(file),
            "{file} is no longer a test module, so it is read as product code"
        );
    }
    let mut actual = Vec::new();
    for source in &sources {
        if source.path == "core/src/test_providers.rs" || test_only.contains(&source.path) {
            continue;
        }
        let code = code_only(&source.text);
        let names: Vec<&str> = word_ends(&code, "mod")
            .iter()
            .map(|end| {
                code[*end..]
                    .trim_start()
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .next()
                    .unwrap()
            })
            .filter(|name| name.ends_with("tests"))
            .collect();
        let bodies: Vec<&str> = names.iter().flat_map(|name| modules(&code, name)).collect();
        let production = without(&code, &bodies);
        actual.extend(
            send_calls(&production)
                .into_iter()
                .map(|call| (source.path.as_str(), call)),
        );
    }
    let mut expected: Vec<(&str, String)> = [
        ("core/src/agent_backend.rs", "send_turn(wire_text)"),
        (
            "core/src/agent_backend.rs",
            "send_turn(wire_text,as_typed,origin,images)",
        ),
        (
            "core/src/agent_backend.rs",
            "send_turn_as(wire_text,as_typed,origin,&client_message_id,images)",
        ),
        ("core/src/agent_backend.rs", "send_turn_with(wire_text,policy)"),
        (
            "core/src/agent_backend.rs",
            "send_turn(&turn.wire,&turn.typed,turn.origin,Vec::new())",
        ),
        (
            "core/src/agent_backend.rs",
            "send_turn_as_spawned(&turn.wire,&turn.typed,turn.origin,&client_message_id,answered)",
        ),
        // A phone's turn: its request names `PromptOrigin::Composed`.
        (
            "core/src/tab_set/remote_turns.rs",
            "send_turn_spawned(request,answered)",
        ),
        // The desk's turn of a remote tab: its request carries the origin its queue or composer named.
        ("core/src/tab_set/desk_sends.rs", "send_turn_spawned(request,answered)"),
        (
            "core/src/agent_backend.rs",
            "steer_turn(shown_turn,client_message_id,wire_text,as_typed,origin)",
        ),
        (
            "core/src/tab_set.rs",
            "send_turn_in(&wire,&typed,mode,root.as_deref(),origin,images)",
        ),
        (
            "core/src/tab_set.rs",
            "send_turn_in(&turn.wire,&turn.typed,mode,root.as_deref(),turn.origin(),images)",
        ),
        (
            "core/src/tab_set.rs",
            "steer(turn,&client_id,&item.wire,&item.text,item.origin())",
        ),
    ]
    .into_iter()
    .map(|(path, call)| (path, call.to_string()))
    .collect();
    actual.sort();
    expected.sort();
    assert_eq!(
        actual, expected,
        "every new send or steer caller must name how its prompt was written"
    );
    assert_eq!(
        send_calls("backend\n . send_turn_in (\n wire, typed, mode,\n origin\n )"),
        ["send_turn_in(wire,typed,mode,origin)"]
    );
    assert_eq!(
        send_calls("AgentBackend::send_turn(&mut backend, wire, typed)"),
        ["send_turn(&mutbackend,wire,typed)"]
    );
    assert_eq!(
        send_calls("provider.send_turn_stamped(request, stamp)"),
        ["send_turn_stamped(request,stamp)"]
    );
}
