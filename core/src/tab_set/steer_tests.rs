//! Enter while a turn runs: a message goes into the running turn when the tab's agent can take it, and
//! is held for the turn's end otherwise.
#![cfg(test)]

use super::*;
use crate::test_providers::{empty_trust_plan, ExternalRecordingProvider, RecordingProvider};
use agent::{AgentConversation, ProviderErrorCode};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn workspace(label: &str) -> PathBuf {
    agent::state_dirs::redirect_state_to_a_test_root();
    let dir = agent::state_dirs::test_workspace_dir(label);
    std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
    dir
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn pump_until_running(set: &mut TabSet, dir: &Path, tab: TabId, running: bool) -> PumpOutput {
    let mut all = None::<PumpOutput>;
    until("the turn state", || {
        let out = set.pump(dir, true);
        match all.as_mut() {
            None => all = Some(out),
            Some(all) => {
                all.turn_ended.extend(out.turn_ended);
                all.queue_changed.extend(out.queue_changed);
            }
        }
        set.get(tab).unwrap().session.was_running == running
    });
    all.unwrap()
}

fn turn_completed(turn: &str, outcome: agent::TurnOutcome) -> AgentDomainEvent {
    AgentDomainEvent::TurnCompleted {
        turn_id: turn.into(),
        outcome,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
        detail: Default::default(),
    }
}

/// A Codex tab on a scripted provider, with `turn-1` running and delivered.
fn running_codex(label: &str, steering: bool) -> (PathBuf, TabSet, TabId, Arc<ExternalRecordingProvider>) {
    let dir = workspace(label);
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let tab = set.open(AgentId::Codex);
    let provider = Arc::new(if steering {
        ExternalRecordingProvider::steering_codex()
    } else {
        ExternalRecordingProvider::codex()
    });
    let backend = AgentBackend::start_external_with(
        AgentId::Codex,
        provider.clone(),
        &dir,
        empty_trust_plan(),
        SessionModeChoice::Auto,
        None,
    )
    .unwrap();
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.inner.queue(AgentDomainEvent::TurnStarted {
        turn_id: "turn-1".into(),
    });
    pump_until_running(&mut set, &dir, tab, true);
    (dir, set, tab, provider)
}

/// Enter on `text` while the turn runs, with the editor context of the moment in front of it.
fn enter(set: &mut TabSet, tab: TabId, text: &str) {
    set.queue_message(tab, text, format!("ctx\n\n{text}"), 1, true, PromptOrigin::Typed)
        .unwrap();
    set.steer_held(tab, 2, Some("turn-1"));
}

/// The queue as `(typed text, sent into the turn)`.
fn queue(set: &TabSet, tab: TabId) -> Vec<(String, bool)> {
    set.get(tab)
        .unwrap()
        .session
        .queue
        .iter()
        .map(|q| (q.text.clone(), !q.is_held()))
        .collect()
}

fn note(set: &TabSet, tab: TabId) -> Option<String> {
    set.get(tab).unwrap().queue_line()
}

fn prompts(set: &TabSet, tab: TabId) -> Vec<String> {
    set.get(tab)
        .unwrap()
        .live()
        .unwrap()
        .projection()
        .user_prompts
        .iter()
        .map(|p| p.text.clone())
        .collect()
}

fn shut_down(set: &mut TabSet) {
    for tab in set.take_all() {
        if let Some(retired) = tab.retired {
            retired.into_backend().shutdown();
        }
    }
}

#[test]
fn enter_puts_the_message_into_the_turn_and_its_row_comes_when_the_agent_reads_it() {
    let (dir, mut set, tab, provider) = running_codex("steer-delivered", true);
    enter(&mut set, tab, "look at the parser too");
    let sent = provider.steers();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].expected_turn_id, "turn-1", "the turn the user saw running");
    assert_eq!(
        sent[0].text, "ctx\n\nlook at the parser too",
        "the wire text carries the context"
    );
    assert_eq!(sent[0].as_typed, "look at the parser too");
    assert_eq!(sent[0].client_message_id, format!("eitri-{}-1", tab.0));
    assert_eq!(queue(&set, tab), [("look at the parser too".to_string(), true)]);
    assert_eq!(note(&set, tab), None);
    assert!(prompts(&set, tab).is_empty(), "no row before the agent read it");

    provider.inner.queue(AgentDomainEvent::UserMessageDelivered {
        client_message_id: sent[0].client_message_id.clone(),
        turn_id: "turn-1".into(),
        text: "look at the parser too".into(),
    });
    let mut changed = Vec::new();
    until("the message leaves the queue", || {
        changed.extend(set.pump(&dir, true).queue_changed);
        set.get(tab).unwrap().session.queue.is_empty()
    });
    assert_eq!(changed, [tab], "the panel is sent the queue again");
    assert_eq!(prompts(&set, tab), ["look at the parser too"]);
    assert!(set.get(tab).unwrap().turn_running(), "the turn was never interrupted");
    assert_eq!(provider.inner.interrupts(), 0);
    shut_down(&mut set);
}

#[test]
fn a_message_the_agent_did_not_read_is_held_again_and_goes_out_after_the_turn() {
    let (dir, mut set, tab, provider) = running_codex("steer-dropped", true);
    enter(&mut set, tab, "and the tests");
    let id = provider.steers()[0].client_message_id.clone();
    provider.inner.queue_all(vec![
        AgentDomainEvent::UserMessageDropped {
            client_message_id: id,
            reason: "the turn ended before Codex read it".into(),
        },
        turn_completed("turn-1", agent::TurnOutcome::Completed),
    ]);
    let out = pump_until_running(&mut set, &dir, tab, false);
    assert_eq!(queue(&set, tab), [("and the tests".to_string(), false)]);
    assert_eq!(
        note(&set, tab).as_deref(),
        Some("held until the turn ends: the turn ended before Codex read it")
    );
    assert!(prompts(&set, tab).is_empty(), "a dropped message has no row");
    assert_eq!(out.turn_ended, [tab]);
    let flush = set.flush_queue(tab).expect("it goes out now");
    assert!(flush.outcome.is_ok());
    assert_eq!(flush.typed, "and the tests");
    assert_eq!(provider.inner.turns(), ["ctx\n\nand the tests"]);
    assert!(set.get(tab).unwrap().session.queue.is_empty());
    assert_eq!(note(&set, tab), None);
    shut_down(&mut set);
}

/// The agent runs a message it could not fold into the turn as a follow-up turn of its own (Claude after
/// a plain-text answer or an interrupt): the provider returns the turn's end and the follow-up's start
/// from one `pump()`, so the host never sees the turn over with the message unreported, sends nothing,
/// and the message leaves the queue when the follow-up reads it. Had the end been folded on its own,
/// the flush would have sent the message again while the agent ran it too.
#[test]
fn a_message_run_in_a_follow_up_turn_is_never_sent_again() {
    let (dir, mut set, tab, provider) = running_codex("steer-follow-up", true);
    enter(&mut set, tab, "and the tests");
    let id = provider.steers()[0].client_message_id.clone();
    provider.inner.queue_all(vec![
        turn_completed("turn-1", agent::TurnOutcome::Completed),
        AgentDomainEvent::TurnStarted {
            turn_id: "turn-2".into(),
        },
    ]);
    let mut ended = Vec::new();
    until("the follow-up turn is folded", || {
        ended.extend(set.pump(&dir, true).turn_ended);
        set.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .active_turn_id
            .as_deref()
            == Some("turn-2")
    });
    assert!(ended.is_empty(), "a turn runs, so nothing is flushed: {ended:?}");
    assert!(set.flush_queue(tab).is_none(), "the follow-up runs");
    assert_eq!(
        queue(&set, tab),
        [("and the tests".to_string(), true)],
        "still in the agent's hands"
    );

    provider.inner.queue_all(vec![
        AgentDomainEvent::UserMessageDelivered {
            client_message_id: id,
            turn_id: "turn-2".into(),
            text: "and the tests".into(),
        },
        turn_completed("turn-2", agent::TurnOutcome::Completed),
    ]);
    let out = pump_until_running(&mut set, &dir, tab, false);
    assert_eq!(out.turn_ended, [tab]);
    assert!(set.get(tab).unwrap().session.queue.is_empty());
    assert!(set.flush_queue(tab).is_none());
    assert!(provider.inner.turns().is_empty(), "never sent as a turn of its own");
    assert_eq!(
        prompts(&set, tab),
        ["and the tests"],
        "one row, where the agent read it"
    );
    shut_down(&mut set);
}

/// The provider gave up waiting on a message (its follow-up turn came too late) and reported it
/// dropped, so the host held it again; the flush was refused while the late follow-up ran, and then
/// the delivery came after all under the old id. The message leaves the queue and is never sent a
/// second time.
#[test]
fn a_message_reported_dropped_and_then_read_late_is_not_sent_again() {
    let (dir, mut set, tab, provider) = running_codex("steer-read-late", true);
    enter(&mut set, tab, "and the tests");
    let id = provider.steers()[0].client_message_id.clone();
    provider.inner.queue_all(vec![
        AgentDomainEvent::UserMessageDropped {
            client_message_id: id.clone(),
            reason: "Claude did not say whether it read it".into(),
        },
        turn_completed("turn-1", agent::TurnOutcome::Completed),
    ]);
    let out = pump_until_running(&mut set, &dir, tab, false);
    assert_eq!(out.turn_ended, [tab]);
    assert_eq!(queue(&set, tab), [("and the tests".to_string(), false)], "held again");
    // The agent is already running the message as a turn of its own, so the send is refused.
    provider.inner.refuse_sends(true);
    let flush = set.flush_queue(tab).expect("a flush is tried");
    assert!(flush.outcome.is_err());
    provider.inner.refuse_sends(false);
    assert_eq!(queue(&set, tab), [("and the tests".to_string(), false)]);

    provider.inner.queue_all(vec![
        AgentDomainEvent::TurnStarted {
            turn_id: "turn-2".into(),
        },
        AgentDomainEvent::UserMessageDelivered {
            client_message_id: id,
            turn_id: "turn-2".into(),
            text: "and the tests".into(),
        },
    ]);
    until("the late delivery takes the message out of the queue", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().session.queue.is_empty()
    });
    assert_eq!(note(&set, tab), None);
    provider
        .inner
        .queue(turn_completed("turn-2", agent::TurnOutcome::Completed));
    let out = pump_until_running(&mut set, &dir, tab, false);
    assert_eq!(out.turn_ended, [tab]);
    assert!(set.flush_queue(tab).is_none(), "nothing left to send");
    assert_eq!(
        provider.inner.turns(),
        ["ctx\n\nand the tests"],
        "only the refused attempt; the agent ran it once"
    );
    // The refused send still shows its prompt (a rejected turn does, by design, with its error), and
    // the delivery is the row where the agent read it.
    assert_eq!(prompts(&set, tab), ["and the tests", "and the tests"]);
    shut_down(&mut set);
}

/// The provider gave up on a message and reported it dropped, and the agent's follow-up turn started
/// before the host could flush, so the message is held again while a turn runs. Enter does not put it
/// into that turn under a new id, since the agent may still read the first copy and would then read
/// it twice: it waits for the turn's end with what was typed after it, and the late delivery under its
/// old id takes it out of the queue.
#[test]
fn a_message_reported_dropped_is_not_put_into_the_turn_again() {
    const WAITS: &str = "held until the turn ends: it went into the turn once already, so it waits for the \
             turn's end, and what follows it waits too";
    let (dir, mut set, tab, provider) = running_codex("steer-given-up-enter", true);
    enter(&mut set, tab, "and the tests");
    let id = provider.steers()[0].client_message_id.clone();
    provider.inner.queue_all(vec![
        AgentDomainEvent::UserMessageDropped {
            client_message_id: id.clone(),
            reason: "Claude did not say whether it read it".into(),
        },
        turn_completed("turn-1", agent::TurnOutcome::Completed),
        AgentDomainEvent::TurnStarted {
            turn_id: "turn-2".into(),
        },
    ]);
    until("the follow-up turn is folded and pumped", || {
        // Read before the pump, not after: the conversation folds on its own thread, so a turn seen
        // started after a pump returned may have been folded after that pump read the projection,
        // and the drop folded in the same batch would not have reached the queue yet.
        let folded = set
            .get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .active_turn_id
            .as_deref()
            == Some("turn-2");
        set.pump(&dir, true);
        folded
    });
    assert_eq!(queue(&set, tab), [("and the tests".to_string(), false)], "held again");
    assert!(set.flush_queue(tab).is_none(), "the follow-up runs");

    set.queue_message(
        tab,
        "and the docs",
        "ctx\n\nand the docs".into(),
        3,
        true,
        PromptOrigin::Typed,
    )
    .unwrap();
    set.steer_held(tab, 4, Some("turn-2"));
    assert_eq!(provider.steers().len(), 1, "nothing more went into a turn");
    assert_eq!(
        queue(&set, tab),
        [
            ("and the tests".to_string(), false),
            ("and the docs".to_string(), false)
        ]
    );
    assert_eq!(note(&set, tab).as_deref(), Some(WAITS));

    provider.inner.queue(AgentDomainEvent::UserMessageDelivered {
        client_message_id: id,
        turn_id: "turn-2".into(),
        text: "and the tests".into(),
    });
    until("the late delivery takes the message out of the queue", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().session.queue.len() == 1
    });
    assert_eq!(queue(&set, tab), [("and the docs".to_string(), false)]);
    provider
        .inner
        .queue(turn_completed("turn-2", agent::TurnOutcome::Completed));
    let out = pump_until_running(&mut set, &dir, tab, false);
    assert_eq!(out.turn_ended, [tab]);
    let flush = set.flush_queue(tab).expect("what was typed after it goes out");
    assert!(flush.outcome.is_ok());
    assert_eq!(
        provider.inner.turns(),
        ["ctx\n\nand the docs"],
        "the agent read the first message once, and it was never sent again"
    );
    assert_eq!(provider.steers().len(), 1);
    shut_down(&mut set);
}

/// A message read under an id it was reported dropped from, while a newer copy is still with the
/// agent, stays in the queue until the newer copy's outcome comes, or until no turn runs; then it
/// leaves without being sent again, whatever became of the newer copy.
#[test]
fn a_late_delivery_of_an_old_copy_waits_for_the_newer_copy() {
    let steered = Queued {
        state: QueuedState::Steered {
            client_id: "new".into(),
            sent_at_ms: 2,
        },
        dropped_as: vec!["old".into()],
        ..Queued::held("x", "x".into(), 1)
    };
    let delivered = |id: &str| AgentDomainEvent::UserMessageDelivered {
        client_message_id: id.into(),
        turn_id: "turn-2".into(),
        text: "x".into(),
    };
    let mut projection = agent::AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::TurnStarted {
        turn_id: "turn-2".into(),
    });
    projection.apply(&delivered("old"));
    let mut queue = vec![steered.clone()];
    let mut note = None;
    assert!(!settle_against(&AgentId::Codex, &mut queue, &mut note, &projection));
    assert_eq!(queue, [steered.clone()], "the newer copy is still open");

    let mut dropped = projection.clone();
    dropped.apply(&AgentDomainEvent::UserMessageDropped {
        client_message_id: "new".into(),
        reason: "Claude discarded it".into(),
    });
    assert!(settle_against(&AgentId::Codex, &mut queue, &mut note, &dropped));
    assert!(queue.is_empty(), "the copy the agent read is enough");
    assert_eq!(note, None, "not held again");

    let mut ended = projection.clone();
    ended.apply(&turn_completed("turn-2", agent::TurnOutcome::Completed));
    let mut queue = vec![steered.clone()];
    assert!(settle_against(&AgentId::Codex, &mut queue, &mut note, &ended));
    assert!(queue.is_empty(), "no turn runs, and the agent read it");

    let mut read_twice = projection;
    read_twice.apply(&delivered("new"));
    let mut queue = vec![steered];
    assert!(settle_against(&AgentId::Codex, &mut queue, &mut note, &read_twice));
    assert!(queue.is_empty());
}

/// An agent that listed its commands: the tab reads a leading `/` as one.
fn lists_commands(set: &mut TabSet, dir: &Path, tab: TabId, provider: &ExternalRecordingProvider) {
    provider.inner.queue(AgentDomainEvent::CommandsAvailable {
        commands: vec![agent::SlashCommand {
            name: "compact".into(),
            description: String::new(),
            hint: None,
        }],
    });
    until("the list of commands is folded", || {
        set.pump(dir, true);
        set.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .available_commands
            .is_some()
    });
}

fn typed(set: &mut TabSet, tab: TabId, text: &str) {
    set.queue_message(tab, text, text.into(), 1, true, PromptOrigin::Typed)
        .unwrap();
}

/// Ends `turn`, and sends what the tab's queue sends then, as the tick does.
fn turn_ends_and_flushes(set: &mut TabSet, dir: &Path, tab: TabId, provider: &ExternalRecordingProvider, turn: &str) {
    provider
        .inner
        .queue(turn_completed(turn, agent::TurnOutcome::Completed));
    assert_eq!(pump_until_running(set, dir, tab, false).turn_ended, [tab]);
    set.flush_queue(tab).expect("the queue goes").outcome.unwrap();
}

fn turn_runs(set: &mut TabSet, dir: &Path, tab: TabId, provider: &ExternalRecordingProvider, turn: &str) {
    provider
        .inner
        .queue(AgentDomainEvent::TurnStarted { turn_id: turn.into() });
    pump_until_running(set, dir, tab, true);
}

/// Only the command waits: on an agent that reads commands it is never put into the turn, the message
/// typed behind it is, and the command stays in its place. On Codex, which lists none, `/status` goes into
/// the turn as typed.
#[test]
fn a_message_behind_a_command_goes_into_the_turn_and_the_command_waits_in_its_place() {
    let (dir, mut set, tab, provider) = running_codex("steer-command-listed", true);
    lists_commands(&mut set, &dir, tab, &provider);
    typed(&mut set, tab, "/compact");
    typed(&mut set, tab, "then this");
    assert!(set.steer_held(tab, 2, Some("turn-1")), "the message went in");
    let texts: Vec<String> = provider.steers().into_iter().map(|s| s.as_typed).collect();
    assert_eq!(texts, ["then this"], "the command is not sent as text");
    assert_eq!(
        queue(&set, tab),
        [("/compact".to_string(), false), ("then this".to_string(), true)]
    );
    assert_eq!(note(&set, tab).as_deref(), Some(COMMANDS_NOTE));
    assert_eq!(
        set.get(tab).unwrap().session.queue_note,
        None,
        "the note is the held command's, and goes with it"
    );

    // The agent reads the message out of order with the queue; the command stays, and says why.
    let id = provider.steers()[0].client_message_id.clone();
    provider.inner.queue(AgentDomainEvent::UserMessageDelivered {
        client_message_id: id,
        turn_id: "turn-1".into(),
        text: "then this".into(),
    });
    until("the message leaves the queue", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().session.queue.len() == 1
    });
    assert_eq!(queue(&set, tab), [("/compact".to_string(), false)]);
    assert_eq!(note(&set, tab).as_deref(), Some(COMMANDS_NOTE));
    shut_down(&mut set);

    // Claude reads commands whatever it listed; an ACP agent that withdrew its list (an empty one) reads none.
    let mut none_listed = agent::AgentSessionProjection::default();
    assert!(reads_slash_commands(&AgentId::Claude, &none_listed));
    assert!(!reads_slash_commands(&AgentId::Codex, &none_listed));
    none_listed.available_commands = Some(Vec::new());
    assert!(!reads_slash_commands(&AgentId::Acp("x".into()), &none_listed));

    // Codex lists none: the text goes in as typed, and a slash later in a message is no command.
    let (_dir, mut set, tab, provider) = running_codex("steer-command-codex", true);
    enter(&mut set, tab, "/status");
    let texts: Vec<String> = provider.steers().into_iter().map(|s| s.as_typed).collect();
    assert_eq!(texts, ["/status"]);
    assert_eq!(note(&set, tab), None);
    shut_down(&mut set);
    assert!(is_slash_command("/x") && is_slash_command("  /x") && !is_slash_command("a/b"));
}

/// A command does not stop what comes after it, and a message does not stop a command behind it; the
/// messages reach the agent in the order typed.
#[test]
fn messages_around_commands_go_into_the_turn_in_order_and_the_commands_stay() {
    let (dir, mut set, tab, provider) = running_codex("steer-around-commands", true);
    lists_commands(&mut set, &dir, tab, &provider);
    for text in ["one", "/a", "two", "/b", "three"] {
        typed(&mut set, tab, text);
    }
    set.steer_held(tab, 2, Some("turn-1"));
    let texts: Vec<String> = provider.steers().into_iter().map(|s| s.as_typed).collect();
    assert_eq!(texts, ["one", "two", "three"]);
    assert_eq!(
        queue(&set, tab),
        [
            ("one".to_string(), true),
            ("/a".to_string(), false),
            ("two".to_string(), true),
            ("/b".to_string(), false),
            ("three".to_string(), true),
        ]
    );
    assert_eq!(note(&set, tab).as_deref(), Some(COMMANDS_NOTE));
    shut_down(&mut set);
}

/// The other stops still stop: what follows them waits, and the line says both things.
#[test]
fn a_refusal_behind_a_command_holds_what_follows_it_and_says_both() {
    let (dir, mut set, tab, provider) = running_codex("steer-command-refused", true);
    lists_commands(&mut set, &dir, tab, &provider);
    provider.refuse_steers(Some(ProviderErrorCode::NoActiveTurn));
    typed(&mut set, tab, "/compact");
    typed(&mut set, tab, "then this");
    typed(&mut set, tab, "and this");
    set.steer_held(tab, 2, Some("turn-1"));
    assert!(provider.steers().is_empty());
    assert_eq!(
        note(&set, tab).as_deref(),
        Some("commands run when the turn ends; held until the turn ends: the scripted agent took no message")
    );

    // The refused message is steered at the next Enter, and only the command's words stay.
    provider.refuse_steers(None);
    set.steer_held(tab, 3, Some("turn-1"));
    let texts: Vec<String> = provider.steers().into_iter().map(|s| s.as_typed).collect();
    assert_eq!(texts, ["then this", "and this"]);
    assert_eq!(note(&set, tab).as_deref(), Some(COMMANDS_NOTE));
    shut_down(&mut set);
}

/// A message dropped beside a held command: the line keeps both, and loses the drop's reason once the
/// message has left, while the command still waits.
#[test]
fn a_dropped_message_leaves_the_commands_words_and_goes_on_its_own() {
    let (dir, mut set, tab, provider) = running_codex("steer-command-dropped", true);
    lists_commands(&mut set, &dir, tab, &provider);
    typed(&mut set, tab, "/compact");
    typed(&mut set, tab, "then this");
    set.steer_held(tab, 2, Some("turn-1"));
    let id = provider.steers()[0].client_message_id.clone();
    provider.inner.queue(AgentDomainEvent::UserMessageDropped {
        client_message_id: id.clone(),
        reason: "gave up waiting".into(),
    });
    until("the message is held again", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().session.queue.iter().all(Queued::is_held)
    });
    assert_eq!(
        note(&set, tab).as_deref(),
        Some("commands run when the turn ends; held until the turn ends: gave up waiting")
    );
    provider.inner.queue(AgentDomainEvent::UserMessageDelivered {
        client_message_id: id,
        turn_id: "turn-1".into(),
        text: "then this".into(),
    });
    until("the late delivery takes it out", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().session.queue.len() == 1
    });
    assert_eq!(note(&set, tab).as_deref(), Some(COMMANDS_NOTE));
    shut_down(&mut set);
}

/// A command the CLI refused stays until it is taken back, and the line goes on saying so after the message
/// behind it has left: it is not a held command, which would run at the turn's end.
#[test]
fn a_refused_command_keeps_its_words_when_the_message_behind_it_leaves() {
    let (dir, mut set, tab, provider) = running_codex("steer-refused-command", true);
    lists_commands(&mut set, &dir, tab, &provider);
    typed(&mut set, tab, "/unknown");
    typed(&mut set, tab, "then this");
    {
        let session = &mut set.get_mut(tab).unwrap().session;
        session.queue[0].refused = true;
        session.queue_note = Some(refused_note(agent::CLI_REFUSED_DROP));
    }
    set.steer_held(tab, 2, Some("turn-1"));
    let words = refused_note(agent::CLI_REFUSED_DROP);
    assert_eq!(note(&set, tab).as_deref(), Some(words.as_str()));
    let id = provider.steers()[0].client_message_id.clone();
    provider.inner.queue(AgentDomainEvent::UserMessageDelivered {
        client_message_id: id,
        turn_id: "turn-1".into(),
        text: "then this".into(),
    });
    until("the message leaves the queue", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().session.queue.len() == 1
    });
    assert_eq!(note(&set, tab).as_deref(), Some(words.as_str()));
    shut_down(&mut set);
}

/// What a turn's end finds in the queue goes out in the order typed, a command alone: read with a message
/// behind it, the message would be the command's arguments and never reach the agent.
#[test]
fn at_the_turns_end_a_command_goes_alone_and_what_follows_it_waits_for_its_end() {
    let (dir, mut set, tab, provider) = running_codex("flush-commands", false);
    lists_commands(&mut set, &dir, tab, &provider);
    for text in ["one", "two", "/a", "/b", "three", "four"] {
        typed(&mut set, tab, text);
    }
    set.steer_held(tab, 2, Some("turn-1"));
    assert_eq!(
        note(&set, tab).as_deref(),
        Some("commands run when the turn ends; held until the turn ends: Codex cannot take messages mid-turn")
    );
    turn_ends_and_flushes(&mut set, &dir, tab, &provider, "turn-1");
    assert_eq!(provider.inner.turns(), ["one\n\ntwo"]);
    assert_eq!(note(&set, tab).as_deref(), Some(COMMANDS_NOTE));
    turn_runs(&mut set, &dir, tab, &provider, "turn-2");
    turn_ends_and_flushes(&mut set, &dir, tab, &provider, "turn-2");
    assert_eq!(provider.inner.turns(), ["one\n\ntwo", "/a"]);
    turn_runs(&mut set, &dir, tab, &provider, "turn-3");
    turn_ends_and_flushes(&mut set, &dir, tab, &provider, "turn-3");
    assert_eq!(provider.inner.turns(), ["one\n\ntwo", "/a", "/b"]);
    assert_eq!(
        queue(&set, tab),
        [("three".to_string(), false), ("four".to_string(), false)]
    );
    assert_eq!(note(&set, tab), None, "no command is held any more");
    turn_runs(&mut set, &dir, tab, &provider, "turn-4");
    turn_ends_and_flushes(&mut set, &dir, tab, &provider, "turn-4");
    assert_eq!(provider.inner.turns(), ["one\n\ntwo", "/a", "/b", "three\n\nfour"]);
    assert!(set.get(tab).unwrap().session.queue.is_empty());
    shut_down(&mut set);
}

/// Codex lists no commands: the whole queue is one turn, as typed. A message the CLI refused stays put, and
/// a review is a message whatever it starts with.
#[test]
fn next_turn_splits_only_at_a_command_the_agent_reads() {
    let item = |text: &str| Queued::held(text, text.into(), 1);
    let mut refused = item("/refused");
    refused.refused = true;
    let mut review = item("/a review");
    review.steerable = false;
    let queue = [refused, item("one"), review, item("/a"), item("two")];
    assert_eq!(next_turn(&queue, false), [1, 2, 3, 4]);
    assert_eq!(next_turn(&queue, true), [1, 2]);
    assert_eq!(next_turn(&queue[3..], true), [0]);
    assert_eq!(next_turn(&queue[4..], true), [0]);
    assert!(next_turn(&queue[..1], true).is_empty());
}

#[test]
fn a_refusal_holds_the_message_and_every_one_after_it_keeping_their_order() {
    let (dir, mut set, tab, provider) = running_codex("steer-refused", true);
    provider.refuse_steers(Some(ProviderErrorCode::NoActiveTurn));
    set.queue_message(tab, "first", "first".into(), 1, true, PromptOrigin::Typed)
        .unwrap();
    set.queue_message(tab, "second", "second".into(), 1, true, PromptOrigin::Typed)
        .unwrap();
    assert!(set.steer_held(tab, 2, Some("turn-1")), "the note changed");
    assert_eq!(
        queue(&set, tab),
        [("first".to_string(), false), ("second".to_string(), false)]
    );
    assert_eq!(
        note(&set, tab).as_deref(),
        Some("held until the turn ends: the scripted agent took no message")
    );
    assert!(provider.steers().is_empty(), "nothing was sent");

    // The next Enter tries again, oldest first, and the note goes once nothing is held.
    provider.refuse_steers(None);
    enter(&mut set, tab, "third");
    let texts: Vec<String> = provider.steers().into_iter().map(|s| s.as_typed).collect();
    assert_eq!(texts, ["first", "second", "third"]);
    assert!(queue(&set, tab).iter().all(|(_, steered)| *steered));
    assert_eq!(note(&set, tab), None);
    let ids: std::collections::BTreeSet<String> = provider.steers().into_iter().map(|s| s.client_message_id).collect();
    assert_eq!(ids.len(), 3, "every message has its own id");
    let _ = dir;
    shut_down(&mut set);
}

#[test]
fn an_agent_that_cannot_take_messages_mid_turn_holds_them_and_says_so() {
    let (_dir, mut set, tab, provider) = running_codex("steer-unsupported-codex", false);
    enter(&mut set, tab, "later");
    assert_eq!(queue(&set, tab), [("later".to_string(), false)]);
    assert_eq!(
        note(&set, tab).as_deref(),
        Some("held until the turn ends: Codex cannot take messages mid-turn")
    );
    assert!(provider.steers().is_empty());
    shut_down(&mut set);

    // A Claude tab: the sidecar does not take them yet either.
    let dir = workspace("steer-unsupported-claude");
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let tab = set.active();
    let provider = Arc::new(RecordingProvider::default());
    let conversation =
        AgentConversation::create(provider.clone(), &dir, agent::setting_sources::ProjectTrust::Untrusted).unwrap();
    set.get_mut(tab).unwrap().backend = TabBackend::Live(AgentBackend::Conversation {
        agent: AgentId::Claude,
        conversation: Box::new(conversation),
    });
    provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    pump_until_running(&mut set, &dir, tab, true);
    enter(&mut set, tab, "later");
    assert_eq!(queue(&set, tab), [("later".to_string(), false)]);
    assert_eq!(
        note(&set, tab).as_deref(),
        Some("held until the turn ends: Claude cannot take messages mid-turn")
    );
    shut_down(&mut set);
}

#[test]
fn nothing_is_put_into_a_turn_when_none_runs() {
    let dir = workspace("steer-idle");
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let tab = set.open(AgentId::Codex);
    let provider = Arc::new(ExternalRecordingProvider::steering_codex());
    let backend = AgentBackend::start_external_with(
        AgentId::Codex,
        provider.clone(),
        &dir,
        empty_trust_plan(),
        SessionModeChoice::Auto,
        None,
    )
    .unwrap();
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    set.queue_message(tab, "x", "x".into(), 1, true, PromptOrigin::Typed)
        .unwrap();
    assert!(!set.steer_held(tab, 2, Some("turn-1")));
    assert!(provider.steers().is_empty());
    shut_down(&mut set);
}

#[test]
fn taking_the_queue_back_leaves_what_was_sent_into_the_turn() {
    let (_dir, mut set, tab, provider) = running_codex("steer-take-back", true);
    enter(&mut set, tab, "sent");
    provider.refuse_steers(Some(ProviderErrorCode::NoActiveTurn));
    enter(&mut set, tab, "held");
    assert_eq!(
        queue(&set, tab),
        [("sent".to_string(), true), ("held".to_string(), false)]
    );
    assert_eq!(set.take_back_queue(tab), ["held"]);
    assert_eq!(queue(&set, tab), [("sent".to_string(), true)]);
    assert_eq!(note(&set, tab), None, "nothing is held any more");
    assert!(set.take_back_queue(tab).is_empty());
    shut_down(&mut set);
}

/// Codex forgets a message it had not read when the turn is interrupted (measured on 0.162.1), and says
/// so as a drop before the turn's end: `Ctrl+Enter` then sends it ahead of what was typed with the key.
#[test]
fn send_now_with_a_message_in_the_turn_sends_it_again_once_the_agent_drops_it() {
    let (dir, mut set, tab, provider) = running_codex("steer-send-now", true);
    enter(&mut set, tab, "first");
    let id = provider.steers()[0].client_message_id.clone();
    match set.send_now(tab, "now", "now".into(), 3).unwrap() {
        SendNow::Interrupting(Ok(_)) => {}
        _ => panic!("a running turn is interrupted"),
    }
    assert_eq!(provider.inner.interrupts(), 1);
    assert_eq!(
        queue(&set, tab),
        [("first".to_string(), true), ("now".to_string(), false)]
    );
    provider.inner.queue_all(vec![
        AgentDomainEvent::UserMessageDropped {
            client_message_id: id,
            reason: "the turn ended before Codex read it".into(),
        },
        turn_completed("turn-1", agent::TurnOutcome::Interrupted),
    ]);
    let out = pump_until_running(&mut set, &dir, tab, false);
    assert_eq!(out.turn_ended, [tab]);
    let flush = set.flush_queue(tab).expect("the queue goes out after the interrupt");
    assert_eq!(flush.typed, "first\n\nnow");
    assert_eq!(provider.inner.turns(), ["ctx\n\nfirst\n\nnow"]);
    shut_down(&mut set);
}

/// A provider that never said what became of a message: it goes out with the next turn rather than
/// staying in the queue for good.
#[test]
fn a_message_no_outcome_was_reported_for_goes_out_with_the_next_turn() {
    let (dir, mut set, tab, provider) = running_codex("steer-no-outcome", true);
    enter(&mut set, tab, "unanswered");
    provider
        .inner
        .queue(turn_completed("turn-1", agent::TurnOutcome::Completed));
    pump_until_running(&mut set, &dir, tab, false);
    let flush = set.flush_queue(tab).expect("sent");
    assert_eq!(flush.typed, "unanswered");
    shut_down(&mut set);
}

/// What the agent did with a message is read off the projection, so a path that never sees the events
/// (a resync, or a take-back before the next pump) still finds it.
#[test]
fn the_outcome_is_read_from_the_projection_not_only_from_a_delivered_batch() {
    let (_dir, mut set, tab, provider) = running_codex("steer-from-projection", true);
    enter(&mut set, tab, "read");
    let id = provider.steers()[0].client_message_id.clone();
    provider.inner.queue(AgentDomainEvent::UserMessageDelivered {
        client_message_id: id.clone(),
        turn_id: "turn-1".into(),
        text: "read".into(),
    });
    until("the projection folds it", || {
        set.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .steer_outcome(&id)
            .is_some()
    });
    assert!(set.take_back_queue(tab).is_empty());
    assert!(set.get(tab).unwrap().session.queue.is_empty(), "settled without a pump");
    shut_down(&mut set);
}

#[test]
fn closing_says_a_message_in_the_turn_may_not_have_reached_the_agent() {
    let (_dir, mut set, tab, provider) = running_codex("steer-close", true);
    enter(&mut set, tab, "sent");
    provider.refuse_steers(Some(ProviderErrorCode::NoActiveTurn));
    enter(&mut set, tab, "held");
    let facts = set.close_facts(tab).unwrap();
    assert_eq!((facts.queued, facts.steered), (1, 1));
    let lines = tabs::close_prompt(&facts);
    assert!(
        lines.contains(&"1 queued message goes to history".to_string()),
        "{lines:?}"
    );
    assert!(
        lines.contains(&"1 message sent into the turn may not have reached the agent; it goes to history".to_string()),
        "{lines:?}"
    );
    assert_eq!(set.queue_texts(tab), ["sent", "held"], "both go to history");
    assert_eq!(set.queued_count(), 2, "the window counts both");
    assert!(tabs::close_steps(&facts).contains(&tabs::CloseStep::QueueToHistory));
    shut_down(&mut set);
}

/// The flush reads what the agent reported and whether a turn runs in one read: a delivery and the turn's
/// end folded after the last pump and before the flush are seen together, and the message the agent read
/// is not sent again.
#[test]
fn a_delivery_folded_just_before_the_flush_is_not_sent_again() {
    let (_dir, mut set, tab, provider) = running_codex("steer-fold-before-flush", true);
    enter(&mut set, tab, "read just now");
    let id = provider.steers()[0].client_message_id.clone();
    provider.inner.queue_all(vec![
        AgentDomainEvent::UserMessageDelivered {
            client_message_id: id,
            turn_id: "turn-1".into(),
            text: "read just now".into(),
        },
        turn_completed("turn-1", agent::TurnOutcome::Completed),
    ]);
    until("the projection folds both, with no pump", || {
        set.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .active_turn_id
            .is_none()
    });
    assert_eq!(
        queue(&set, tab),
        [("read just now".to_string(), true)],
        "not settled yet"
    );
    assert!(set.flush_queue(tab).is_none(), "nothing left to send");
    assert!(
        provider.inner.turns().is_empty(),
        "the message the agent read was not sent again"
    );
    assert!(set.get(tab).unwrap().session.queue.is_empty());
    shut_down(&mut set);
}

/// A delivery folded with the turn still running: the flush sends nothing, and the message leaves the
/// queue as read.
#[test]
fn a_flush_during_the_turn_settles_and_sends_nothing() {
    let (_dir, mut set, tab, provider) = running_codex("steer-flush-running", true);
    enter(&mut set, tab, "mid-turn");
    let id = provider.steers()[0].client_message_id.clone();
    provider.inner.queue(AgentDomainEvent::UserMessageDelivered {
        client_message_id: id.clone(),
        turn_id: "turn-1".into(),
        text: "mid-turn".into(),
    });
    until("the delivery is folded", || {
        set.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .steer_outcome(&id)
            .is_some()
    });
    assert!(set.flush_queue(tab).is_none());
    assert!(set.get(tab).unwrap().session.queue.is_empty());
    assert!(provider.inner.turns().is_empty());
    shut_down(&mut set);
}

/// Only into the turn the page showed: one typed in a turn that has ended since, or with none showing,
/// is held.
#[test]
fn a_message_goes_only_into_the_turn_the_page_showed() {
    let (_dir, mut set, tab, provider) = running_codex("steer-shown-turn", true);
    set.queue_message(
        tab,
        "for the old turn",
        "for the old turn".into(),
        1,
        true,
        PromptOrigin::Typed,
    )
    .unwrap();
    set.steer_held(tab, 2, Some("turn-0"));
    assert!(provider.steers().is_empty());
    assert_eq!(
        note(&set, tab).as_deref(),
        Some("held until the turn ends: the turn it was typed in has ended")
    );
    set.steer_held(tab, 3, None);
    assert!(provider.steers().is_empty());
    assert_eq!(
        note(&set, tab).as_deref(),
        Some("held until the turn ends: no turn was running when it was typed")
    );
    set.steer_held(tab, 4, Some("turn-1"));
    assert_eq!(provider.steers().len(), 1);
    assert_eq!(note(&set, tab), None);
    shut_down(&mut set);
}

/// A confirmed review held behind the turn waits for its end, and so does what is typed after it.
#[test]
fn a_review_waits_for_the_turns_end_and_keeps_what_follows_behind_it() {
    let (_dir, mut set, tab, provider) = running_codex("steer-review", true);
    assert!(set.send_review(tab, "the review", 1).unwrap().is_none(), "it waits");
    enter(&mut set, tab, "typed after it");
    assert!(
        provider.steers().is_empty(),
        "nothing goes into the turn ahead of the review"
    );
    assert_eq!(
        queue(&set, tab),
        [("the review".to_string(), false), ("typed after it".to_string(), false)]
    );
    assert_eq!(
        note(&set, tab).as_deref(),
        Some("held until the turn ends: a review waits for the turn's end, and what follows it waits too")
    );
    shut_down(&mut set);
}

/// A tripwire takes the session away: a message still in the turn is held again with the reason, not
/// left as `↪` for good.
#[test]
fn a_tab_failed_by_a_tripwire_holds_what_was_in_the_turn() {
    let (dir, mut set, tab, provider) = running_codex("steer-tripwire", true);
    enter(&mut set, tab, "in the turn");
    provider.inner.queue(AgentDomainEvent::PolicyMismatch {
        requested: "on-request".into(),
        reported: "never".into(),
    });
    let mut changed = Vec::new();
    until("the tab fails", || {
        let mut out = set.pump(&dir, true);
        changed.append(&mut out.queue_changed);
        for tripped in out.tripped {
            let mut backend = tripped.retired.into_backend();
            backend.shutdown();
        }
        matches!(set.get(tab).unwrap().backend, TabBackend::Failed { .. })
    });
    assert_eq!(queue(&set, tab), [("in the turn".to_string(), false)]);
    assert_eq!(
        note(&set, tab).as_deref(),
        Some("held until the turn ends: the session was closed before the agent read it")
    );
    assert_eq!(changed, [tab], "the panel is sent the queue again");
    shut_down(&mut set);
}

/// A command that fails fatally takes the session away like a tripwire: a message still in the turn
/// is held again with the reason, and `↑` takes it back.
#[test]
fn a_fatal_command_holds_a_steered_message_again() {
    let (_dir, mut set, tab, provider) = running_codex("steer-fatal", true);
    enter(&mut set, tab, "in the turn");
    assert_eq!(provider.steers().len(), 1, "it went into the turn");
    let mut backend = set
        .fail_session(tab, "gone".into(), FailedHow::AfterStart, true)
        .expect("the live backend is handed back to be shut down")
        .into_backend();
    backend.shutdown();
    assert!(matches!(
        &set.get(tab).unwrap().backend,
        TabBackend::Failed { reason } if reason == "gone"
    ));
    assert!(set.get(tab).unwrap().failed_after_start);
    assert_eq!(queue(&set, tab), [("in the turn".to_string(), false)]);
    // Read before the take-back, which clears the note.
    assert_eq!(
        note(&set, tab),
        Some(held_note("the session was closed before the agent read it"))
    );
    assert_eq!(set.take_back_queue(tab), ["in the turn"]);
    set.retire_done(tab);
    shut_down(&mut set);
}

/// What the agent reported before its session went is applied first: the message it read leaves the
/// queue, and only the one it did not is held again, where it stood.
#[test]
fn only_the_steered_messages_the_agent_did_not_take_are_held_again_in_order() {
    let (_dir, mut set, tab, provider) = running_codex("steer-fatal-partly-read", true);
    enter(&mut set, tab, "read");
    enter(&mut set, tab, "not read");
    let ids: Vec<String> = provider.steers().into_iter().map(|s| s.client_message_id).collect();
    assert_eq!(ids.len(), 2, "both went into the turn");
    provider.inner.queue(AgentDomainEvent::UserMessageDelivered {
        client_message_id: ids[0].clone(),
        turn_id: "turn-1".into(),
        text: "read".into(),
    });
    until("the delivery is folded", || {
        set.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .steer_outcome(&ids[0])
            .is_some()
    });
    assert_eq!(
        queue(&set, tab),
        [("read".to_string(), true), ("not read".to_string(), true)],
        "not settled before the failure"
    );
    let mut backend = set
        .fail_session(tab, "gone".into(), FailedHow::AfterStart, false)
        .unwrap()
        .into_backend();
    backend.shutdown();
    assert_eq!(queue(&set, tab), [("not read".to_string(), false)]);
    assert_eq!(set.take_back_queue(tab), ["not read"]);
    set.retire_done(tab);
    shut_down(&mut set);
}

#[test]
fn the_queue_envelope_says_whether_enter_steers_now() {
    let (_dir, mut set, tab, _provider) = running_codex("steer-envelope", true);
    let payloads = set.active_state_payloads();
    let queue: serde_json::Value = payloads
        .iter()
        .map(|p| serde_json::from_str::<serde_json::Value>(p).unwrap())
        .find(|v| v["kind"] == "queue")
        .unwrap();
    assert_eq!(queue["enterSteers"], true);
    assert_eq!(set.get(tab).unwrap().steers(), Some(true));
    shut_down(&mut set);
    let (_dir, mut set, tab, _provider) = running_codex("steer-envelope-no", false);
    assert_eq!(set.get(tab).unwrap().steers(), Some(false));
    shut_down(&mut set);
}

#[test]
fn ordering_base_tabs_keeps_the_numbers_of_tabs_already_here_and_numbers_arrivals_after_them() {
    let mut tabs = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Manual);
    let one = tabs.active();
    let two = tabs.open(AgentId::Claude);
    let three = tabs.open(AgentId::Claude);
    tabs.remove(two).unwrap();
    // The lowest free number is the gap, so the first arrival lands between the tabs that were here.
    let a = tabs.open(AgentId::Claude);
    let b = tabs.open(AgentId::Claude);
    assert_eq!(tabs.get(a).unwrap().number, 2);
    assert!(tabs.order_base_tabs(&[one, three, a, b], &[a, b]));
    let numbers: Vec<_> = [one, three, a, b]
        .iter()
        .map(|t| tabs.get(*t).unwrap().number)
        .collect();
    assert_eq!(numbers, vec![1, 3, 4, 5]);
    assert_eq!(tabs.base_tabs(), vec![one, three, a, b]);
    assert!(
        !tabs.order_base_tabs(&[one, three, a], &[]),
        "an order missing a tab is refused"
    );
}
