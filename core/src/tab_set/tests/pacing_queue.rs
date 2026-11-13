//! Payload pacing, the queue and send-now, the turn's times, and how a turn ends.
#![cfg(test)]

use super::*;

/// Pumps until the active tab has a payload, returning it with its class.
fn pump_until_payload(set: &mut TabSet, dir: &Path) -> (String, crate::panel_cadence::EnvelopeClass) {
    let mut got = None;
    until("a payload for the active tab", || {
        let out = set.pump(dir, true);
        if let Some(payload) = out.active_payload {
            got = Some((payload, out.active_class));
        }
        got.is_some()
    });
    got.unwrap()
}

#[test]
fn the_pump_says_which_payloads_may_wait_for_the_typing_cadence() {
    use crate::panel_cadence::EnvelopeClass::{Immediate, Stream};
    let dir = workspace("tabs-pump-class");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

    // A turn starting is state the chrome hangs on.
    provider.queue(started("t1"));
    let (payload, class) = pump_until_payload(&mut set, &dir);
    assert!(payload.contains("turn_started"), "{payload}");
    assert_eq!(class, Immediate);

    // Plain streamed text is what the cadence exists to pace.
    provider.queue(text_delta("t1", "hello"));
    let (payload, class) = pump_until_payload(&mut set, &dir);
    assert_eq!(events_text(Some(&payload)), "hello");
    assert_eq!(class, Stream);

    // A card is never held behind a slot.
    provider.queue(write("perm-1"));
    let (payload, class) = pump_until_payload(&mut set, &dir);
    assert!(payload.contains("permission_requested"), "{payload}");
    assert_eq!(class, Immediate);

    // Nor is the end of the turn.
    provider.queue(completed("t1", agent::TurnOutcome::Completed));
    let (payload, class) = pump_until_payload(&mut set, &dir);
    assert!(payload.contains("turn_completed"), "{payload}");
    assert_eq!(class, Immediate);
    shut_down_all(&mut set);
}

#[test]
fn a_resync_snapshot_is_never_paced() {
    use crate::panel_cadence::EnvelopeClass::Immediate;
    let dir = workspace("tabs-pump-class-resync");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    // Without pumping in between: enough to overflow the sidecar's own UI queue.
    provider.queue(started("t1"));
    for i in 0..300 {
        provider.queue(text_delta("t1", &format!("chunk {i}")));
    }
    until("the whole stream folded", || {
        let backend = set.get(tab).unwrap().live().unwrap();
        let projection = backend.projection();
        projection.transcript.iter().any(|m| m.text.contains("chunk 299"))
    });
    let (payload, class) = pump_until_payload(&mut set, &dir);
    assert!(
        payload.contains("\"kind\":\"snapshot\""),
        "an overflow resyncs: {payload:.120}"
    );
    assert_eq!(class, Immediate);
    shut_down_all(&mut set);
}

fn interruptible_live(dir: &Path) -> (Arc<RecordingProvider>, AgentBackend) {
    let provider = Arc::new(RecordingProvider::interruptible());
    let conversation =
        AgentConversation::create(provider.clone(), dir, agent::setting_sources::ProjectTrust::Untrusted).unwrap();
    (
        provider,
        AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        },
    )
}

#[test]
fn queued_messages_go_out_as_one_turn_when_the_turn_completes() {
    let dir = workspace("tabs-queue-flush");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(started("t1"));
    pump_until_running(&mut set, &dir, tab, true);

    assert_eq!(
        set.queue_message(
            tab,
            "and the tests",
            "ctx A\n\nand the tests".into(),
            1,
            true,
            agent::PromptOrigin::Typed
        ),
        Ok(1)
    );
    assert_eq!(
        set.queue_message(
            tab,
            "then commit",
            "ctx B\n\nthen commit".into(),
            2,
            true,
            agent::PromptOrigin::Typed
        ),
        Ok(2)
    );
    assert!(set.flush_queue(tab).is_none(), "a running turn holds the queue");
    assert_eq!(set.queued_count(), 2);

    provider.queue(completed("t1", agent::TurnOutcome::Completed));
    let ended = pump_until_running(&mut set, &dir, tab, false);
    assert_eq!(ended, vec![tab], "the pump reports the end of the turn once");
    let flush = set.flush_queue(tab).expect("the queue goes out");
    assert!(flush.outcome.is_ok());
    assert_eq!(flush.typed, "and the tests\n\nthen commit");
    assert_eq!(
        provider.turns(),
        vec!["ctx A\n\nand the tests\n\nctx B\n\nthen commit".to_string()],
        "one turn, each item with the context captured when it was queued"
    );
    assert!(set.get(tab).unwrap().session.queue.is_empty());
    shut_down_all(&mut set);
}

/// Like vim's `:bd` (E89), `<leader>bd` asks only when closing would lose something.
#[test]
fn closing_asks_only_for_a_running_turn_a_connect_or_a_queue() {
    let dir = workspace("tabs-close-needs-confirm");
    let mut set = set();
    let empty = set.active();
    assert!(!set.close_needs_confirm(empty), "an empty tab has nothing to lose");
    assert!(!set.close_needs_confirm(TabId(999)), "no such tab");

    let connecting = set.open(AgentId::Claude);
    let (_tx, result_rx) = mpsc::channel();
    set.get_mut(connecting).unwrap().backend = TabBackend::Starting(PendingStart {
        request_id: "req-1".into(),
        result_rx,
        first_turn: None,
        resume: None,
        resumed_title: None,
        resumed_name: None,
    });
    assert!(
        set.close_needs_confirm(connecting),
        "a connect in flight (it counts as running)"
    );

    let (provider, backend) = live(&dir);
    set.get_mut(empty).unwrap().backend = TabBackend::Live(backend);
    assert!(!set.close_needs_confirm(empty), "an idle live tab: nothing to lose");
    provider.queue(started("t1"));
    pump_until_running(&mut set, &dir, empty, true);
    assert!(set.close_needs_confirm(empty), "a running turn");
    assert_eq!(
        set.queue_message(empty, "later", "later".into(), 1, true, agent::PromptOrigin::Typed),
        Ok(1)
    );
    assert!(set.close_needs_confirm(empty), "a running turn with a queue");
    shut_down_all(&mut set);
}

/// Seen in a GUI pass on 2026-09-25: the elapsed clock read `0s+` after a tab switch, because
/// the panel holds only the active tab's state and restarted the clock at the snapshot. The tab
/// now keeps when its running turn started, from the first tick that delivered it, and the
/// snapshot a switch (or a reload) sends carries it.
#[test]
fn a_running_turn_keeps_its_start_time_across_ticks_and_the_snapshot_carries_it() {
    let clock = TestWallClock::new();
    let dir = workspace("tabs-turn-clock");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    assert_eq!(set.get(tab).unwrap().turn_started_at_ms(), None);
    let before = wall_clock_ms();
    provider.queue(started("t1"));
    pump_until_running(&mut set, &dir, tab, true);
    let stamp = set
        .get(tab)
        .unwrap()
        .turn_started_at_ms()
        .expect("stamped when the turn is delivered");
    assert!(stamp >= before && stamp <= wall_clock_ms());
    clock.advance(20);
    set.pump(&dir, true);
    assert_eq!(
        set.get(tab).unwrap().turn_started_at_ms(),
        Some(stamp),
        "a later tick keeps it"
    );

    let other = set.open(AgentId::Claude);
    set.pump(&dir, true);
    set.select(tab);
    let payloads = set.active_state_payloads();
    let snapshot: serde_json::Value = payloads
        .iter()
        .map(|p| serde_json::from_str::<serde_json::Value>(p).unwrap())
        .find(|v| v["kind"] == "snapshot")
        .expect("a live tab's snapshot");
    assert_eq!(snapshot["turnStartedAtMs"], serde_json::json!(stamp));
    set.remove(other).unwrap();

    provider.queue(completed("t1", agent::TurnOutcome::Completed));
    pump_until_running(&mut set, &dir, tab, false);
    until("the clock to clear", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().turn_started_at_ms().is_none()
    });
    shut_down_all(&mut set);
}

/// Pumps `tab` until a payload for the active tab carries `turnTimes`, returning that payload's
/// `turnTimes`. Every payload the pump hands back is looked at, since the entry rides on whichever
/// events envelope the tick that saw the turn end builds.
fn pump_until_turn_times(set: &mut TabSet, dir: &Path) -> serde_json::Value {
    let mut found = None;
    until("an events payload carrying turnTimes", || {
        if let Some(payload) = set.pump(dir, true).active_payload {
            let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
            if value["kind"] == "events" && value.get("turnTimes").is_some() {
                found = Some(value["turnTimes"].clone());
            }
        }
        found.is_some()
    });
    found.unwrap()
}

fn snapshot_turn_times(set: &mut TabSet) -> serde_json::Value {
    let snapshot: serde_json::Value = serde_json::from_str(&the_snapshot(set.active_state_payloads())).unwrap();
    snapshot["turnTimes"].clone()
}

/// A turn that ends records how long it ran, on the events envelope of the batch that ended it
/// and in every later snapshot; a second end is a second entry.
#[test]
fn a_completed_turn_records_its_duration_in_the_events_and_the_snapshot() {
    let clock = TestWallClock::new();
    let dir = workspace("tabs-turn-times-completed");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    assert_eq!(
        snapshot_turn_times(&mut set),
        serde_json::json!([]),
        "nothing has ended yet"
    );

    provider.queue(started("t1"));
    pump_until_running(&mut set, &dir, tab, true);
    let started_at = set.get(tab).unwrap().turn_started_at_ms().expect("t1 stamped");
    clock.advance(25);
    provider.queue(text_delta("t1", "done"));
    provider.queue(completed("t1", agent::TurnOutcome::Completed));
    let times = pump_until_turn_times(&mut set, &dir);
    let times = times.as_array().unwrap();
    assert_eq!(times.len(), 1, "{times:?}");
    assert_eq!(times[0]["turnId"], "t1");
    assert_eq!(times[0]["startedAtMs"], serde_json::json!(started_at));
    let ended_at = times[0]["endedAtMs"].as_u64().unwrap();
    assert!(
        ended_at >= started_at + 25,
        "the turn includes the advanced time: {started_at} -> {ended_at}"
    );
    assert_eq!(
        set.get(tab).unwrap().session.turn_times_unsent,
        0,
        "the events carried it"
    );
    let newest = {
        let snapshot: serde_json::Value = serde_json::from_str(&the_snapshot(set.active_state_payloads())).unwrap();
        let transcript = snapshot["state"]["transcript"].as_array().unwrap().clone();
        transcript.last().expect("the reply is on the page")["seq"]
            .as_u64()
            .unwrap()
    };
    assert_eq!(
        times[0]["anchorSeq"],
        serde_json::json!(newest),
        "the duration line stands under the newest row"
    );

    // A tick with nothing new does not send it again.
    assert!(set.pump(&dir, true).active_payload.is_none());

    provider.queue(started("t2"));
    pump_until_running(&mut set, &dir, tab, true);
    provider.queue(completed("t2", agent::TurnOutcome::Completed));
    let second = pump_until_turn_times(&mut set, &dir);
    assert_eq!(
        second
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["turnId"].clone())
            .collect::<Vec<_>>(),
        vec![serde_json::json!("t2")],
        "only the turn that ended in this batch rides on the events envelope"
    );

    let all = snapshot_turn_times(&mut set);
    assert_eq!(
        all.as_array()
            .unwrap()
            .iter()
            .map(|t| t["turnId"].clone())
            .collect::<Vec<_>>(),
        vec![serde_json::json!("t1"), serde_json::json!("t2")],
        "the snapshot carries every recorded turn, oldest first"
    );
    assert_eq!(all[0]["endedAtMs"].as_u64().unwrap(), ended_at);
    shut_down_all(&mut set);
}

#[test]
fn an_auto_answered_request_keeps_the_reply_revision_and_duration_anchor() {
    let dir = workspace("tabs-turn-times-revision-gap");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(started("t1"));
    pump_until_running(&mut set, &dir, tab, true);
    // Leave a folded event queued while its snapshot is drawn: only the suffix needs tags.
    provider.queue(text_delta("t1", "covered"));
    wait_for_fold(&set, tab, "covered");
    set.active_state_payloads();
    let base = set.get(tab).unwrap().live().unwrap().projection().last_revision;
    provider.queue(call_started(
        "read",
        "Read",
        serde_json::json!({ "file_path": "main.rs" }),
    ));
    provider.queue(read("perm-read"));
    provider.queue(text_delta("t1", "done"));
    provider.queue(completed("t1", agent::TurnOutcome::Completed));
    until("the whole batch to fold before delivery", || {
        set.get(tab).unwrap().live().unwrap().projection().last_revision >= base + 4
    });
    let payload = set.pump(&dir, true).active_payload.expect("the completed batch");
    let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
    let events = value["events"].as_array().unwrap();
    let revisions = value["revisions"].as_array().unwrap();
    assert_eq!(events.len(), revisions.len());
    assert_eq!(
        revisions,
        &vec![
            serde_json::json!(base + 1),
            serde_json::json!(base + 3),
            serde_json::json!(base + 4)
        ]
    );
    assert!(events.iter().all(|event| event["type"] != "permission_requested"));
    assert_eq!(provider.resolutions(), vec![("perm-read".into(), true)]);
    let reply = events
        .iter()
        .position(|event| event["type"] == "content_delta")
        .unwrap();
    assert_eq!(
        value["turnTimes"][0]["anchorSeq"],
        revisions[reply].as_u64().unwrap() - 1
    );
    shut_down_all(&mut set);
}

/// A window can end with no chance to say anything, so a saved tab has to say a turn is running while it runs.
#[test]
fn a_saved_tab_says_a_turn_is_running_for_as_long_as_it_runs() {
    let dir = workspace("tabs-saved-turn");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(AgentDomainEvent::SessionOpened {
        session_id: "verdandi-1".into(),
        provider_session_id: "s-1".into(),
        model: "m".into(),
        cwd: dir.to_string_lossy().into_owned(),
    });
    until("the session", || !set.saved_snapshot(PROJECT).unwrap().is_empty());
    let running = |set: &TabSet| set.saved_snapshot(PROJECT).unwrap().tabs[0].turn_running;
    assert!(!running(&set));

    provider.queue(started("t1"));
    pump_until_running(&mut set, &dir, tab, true);
    assert!(running(&set));

    provider.queue(completed("t1", agent::TurnOutcome::Completed));
    pump_until_running(&mut set, &dir, tab, false);
    assert!(!running(&set));
    shut_down_all(&mut set);
}

/// A turn that ends badly still ran for some time, and the turn-ending row says how long.
#[test]
fn an_interrupted_turn_records_its_duration_too() {
    let dir = workspace("tabs-turn-times-interrupted");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(started("t1"));
    pump_until_running(&mut set, &dir, tab, true);
    provider.queue(completed("t1", agent::TurnOutcome::Interrupted));
    let times = pump_until_turn_times(&mut set, &dir);
    assert_eq!(times[0]["turnId"], "t1");
    shut_down_all(&mut set);
}

/// A turn no tick saw running (it began and ended between two ticks) has no stamped start, so it
/// has no duration: the panel shows nothing rather than a guess.
#[test]
fn a_turn_no_tick_saw_running_records_no_duration() {
    let dir = workspace("tabs-turn-times-unstamped");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let base = set.get(tab).unwrap().live().unwrap().projection().last_revision;
    provider.queue(started("t1"));
    provider.queue(completed("t1", agent::TurnOutcome::Completed));
    until("both events to fold, with no tick", || {
        set.get(tab).unwrap().live().unwrap().projection().last_revision >= base + 2
    });
    let mut carried = false;
    for _ in 0..5 {
        if let Some(payload) = set.pump(&dir, true).active_payload {
            carried |= payload.contains("turnTimes");
        }
    }
    assert!(!carried, "no payload names a duration");
    assert_eq!(snapshot_turn_times(&mut set), serde_json::json!([]));
    shut_down_all(&mut set);
}

/// Each tab keeps its own durations; a background tab's finished turn waits for the snapshot a
/// switch sends instead of going out as an envelope for a tab nobody is looking at.
#[test]
fn a_background_tabs_turn_durations_wait_for_its_snapshot() {
    let dir = workspace("tabs-turn-times-background");
    let mut set = set();
    let background = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(background).unwrap().backend = TabBackend::Live(backend);
    provider.queue(started("t1"));
    pump_until_running(&mut set, &dir, background, true);
    let foreground = set.open(AgentId::Claude);
    provider.queue(completed("t1", agent::TurnOutcome::Completed));
    until("the background tab's turn to end", || {
        set.pump(&dir, true);
        !set.get(background).unwrap().session.was_running
    });
    assert_eq!(set.get(background).unwrap().session.turn_times_unsent, 1);
    set.select(background);
    let times = snapshot_turn_times(&mut set);
    assert_eq!(times[0]["turnId"], "t1");
    assert_eq!(set.get(background).unwrap().session.turn_times_unsent, 0);
    set.remove(foreground).unwrap();
    shut_down_all(&mut set);
}

#[test]
fn the_turn_log_drops_its_oldest_entries_past_the_cap() {
    let mut clock = None;
    let mut times = Vec::new();
    let mut unsent = 0;
    let mut projection = agent::AgentSessionProjection::default();
    let mut observe = |active: Option<&str>, now: u64| {
        projection.active_turn_id = active.map(str::to_string);
        observe_turn_clock(&mut clock, &mut times, &mut unsent, &projection, now);
    };
    for n in 0..(MAX_TURN_TIMES + 5) {
        observe(Some(&format!("t{n}")), 1_000 + n as u64);
        observe(None, 1_500 + n as u64);
    }
    // The next turn id replacing a running one also ends it, but names no row to stand under.
    observe(Some("a"), 9_000);
    observe(Some("b"), 9_400);
    assert_eq!(times.len(), MAX_TURN_TIMES);
    assert_eq!(
        times[0].turn_id, "t6",
        "the oldest went, one per entry added past the cap"
    );
    assert_eq!(unsent, MAX_TURN_TIMES);
    let last = times.last().unwrap();
    assert_eq!(
        (last.turn_id.as_str(), last.ended_at_ms, last.anchor_seq),
        ("a", 9_400, None)
    );
}

#[test]
fn a_turn_ends_on_the_agents_last_row_even_with_a_prompt_already_after_it() {
    let mut clock = None;
    let mut times = Vec::new();
    let mut unsent = 0;
    let mut projection = agent::AgentSessionProjection::default();
    projection.active_turn_id = Some("t1".to_string());
    observe_turn_clock(&mut clock, &mut times, &mut unsent, &projection, 1_000);
    projection.transcript.push(agent::TranscriptMessage {
        seq: 4,
        text: "done".to_string(),
    });
    projection.user_prompts.push(agent::UserPromptRecord {
        seq: 5,
        text: "next".to_string(),
        delivered: false,
        attachments: Vec::new(),
    });
    projection.active_turn_id = None;
    observe_turn_clock(&mut clock, &mut times, &mut unsent, &projection, 3_000);
    assert_eq!(times[0].anchor_seq, Some(4));
}

/// A refused queue flush stays queued with the refusal reason and is not retried every tick.
#[test]
fn a_refused_flush_keeps_the_queue_and_is_not_retried_every_tick() {
    let dir = workspace("tabs-queue-refused");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(started("t1"));
    pump_until_running(&mut set, &dir, tab, true);
    set.queue_message(tab, "later", "later".into(), 1, true, agent::PromptOrigin::Typed)
        .unwrap();
    provider.refuse_sends(true);
    provider.queue(completed("t1", agent::TurnOutcome::Completed));
    pump_until_running(&mut set, &dir, tab, false);
    let flush = set.flush_queue(tab).unwrap();
    assert!(flush.outcome.is_err());
    let t = set.get(tab).unwrap();
    assert_eq!(t.session.queue.len(), 1, "never dropped");
    assert!(t.session.queue_error.is_some(), "the line saying why");
    for _ in 0..10 {
        assert!(
            set.pump(&dir, true).turn_ended.is_empty(),
            "no new turn end, so no retry"
        );
    }
    assert_eq!(provider.turns().len(), 1, "one attempt");
    shut_down_all(&mut set);
}

/// A pending card holds the queue: a queued message waits until the card is answered.
#[test]
fn the_queue_waits_while_a_card_is_pending() {
    let dir = workspace("tabs-queue-card");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(started("t1"));
    provider.queue(write("perm-w"));
    until("the card", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 1
    });
    set.queue_message(
        tab,
        "after the card",
        "after the card".into(),
        1,
        true,
        agent::PromptOrigin::Typed,
    )
    .unwrap();
    assert!(set.flush_queue(tab).is_none());
    assert!(provider.turns().is_empty());
    shut_down_all(&mut set);
}

/// After an interrupt the queue still goes out, including what `Ctrl+Enter` added.
#[test]
fn send_now_interrupts_and_the_queue_goes_out_when_the_turn_ends() {
    let dir = workspace("tabs-send-now");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = interruptible_live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(started("t1"));
    pump_until_running(&mut set, &dir, tab, true);
    set.queue_message(tab, "one", "one".into(), 1, true, agent::PromptOrigin::Typed)
        .unwrap();
    match set.send_now(tab, "two", "two".into(), 2).unwrap() {
        SendNow::Interrupting(outcome) => assert!(outcome.is_ok()),
        SendNow::Flushed(_) => panic!("a running turn is interrupted first"),
    }
    assert_eq!(provider.interrupts(), 1);
    assert!(
        provider.turns().is_empty(),
        "nothing is sent before the interrupt lands"
    );
    provider.queue(completed("t1", agent::TurnOutcome::Interrupted));
    assert_eq!(pump_until_running(&mut set, &dir, tab, false), vec![tab]);
    set.flush_queue(tab).unwrap().outcome.unwrap();
    assert_eq!(provider.turns(), vec!["one\n\ntwo".to_string()]);
    shut_down_all(&mut set);
}

#[test]
fn send_now_on_an_idle_tab_sends_at_once() {
    let dir = workspace("tabs-send-now-idle");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    match set.send_now(tab, "now", "ctx\n\nnow".into(), 1).unwrap() {
        SendNow::Flushed(Some(flush)) => assert_eq!(flush.typed, "now"),
        _ => panic!("idle: flushed at once"),
    }
    assert_eq!(provider.turns(), vec!["ctx\n\nnow".to_string()]);
    shut_down_all(&mut set);
}

#[test]
fn take_back_returns_every_item_in_order_and_empties_the_queue() {
    let dir = workspace("tabs-take-back");
    let mut set = set();
    let tab = set.active();
    let (_provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    set.queue_message(tab, "a", "a".into(), 1, true, agent::PromptOrigin::Typed)
        .unwrap();
    set.queue_message(tab, "b", "b".into(), 2, true, agent::PromptOrigin::Typed)
        .unwrap();
    assert_eq!(set.take_back_queue(tab), vec!["a".to_string(), "b".to_string()]);
    assert!(set.get(tab).unwrap().session.queue.is_empty());
    assert!(set.take_back_queue(tab).is_empty());
    shut_down_all(&mut set);
}

/// Queueing requires a starting or live session; empty and failed tabs refuse it.
#[test]
fn queueing_needs_a_session_that_is_starting_or_alive() {
    let mut set = set();
    let tab = set.active();
    assert!(
        set.queue_message(tab, "x", "x".into(), 1, true, agent::PromptOrigin::Typed)
            .is_err(),
        "an empty tab sends, it does not queue"
    );
    let (_tx, result_rx) = mpsc::channel();
    set.get_mut(tab).unwrap().backend = TabBackend::Starting(PendingStart {
        request_id: "r".into(),
        result_rx,
        first_turn: None,
        resume: None,
        resumed_title: None,
        resumed_name: None,
    });
    assert_eq!(
        set.queue_message(tab, "x", "x".into(), 1, true, agent::PromptOrigin::Typed),
        Ok(1),
        "the box is live while it starts"
    );
    set.get_mut(tab).unwrap().backend = TabBackend::Failed { reason: "no".into() };
    assert!(set
        .queue_message(tab, "y", "y".into(), 2, true, agent::PromptOrigin::Typed)
        .is_err());
}

/// `r` on an ended tab restores its queue to the draft.
#[test]
fn reset_folds_the_queue_and_the_old_draft_into_the_draft() {
    let mut set = set();
    let tab = set.active();
    {
        let t = set.get_mut(tab).unwrap();
        t.session.queue.push(Queued::held("q1", "q1".into(), 1));
        t.session.queue.push(Queued::held("q2", "q2".into(), 2));
        t.replace_draft("half".into());
        t.backend = TabBackend::Failed { reason: "gone".into() };
    }
    set.reset(tab).unwrap();
    let t = set.get(tab).unwrap();
    assert_eq!(t.session.draft, "q1\n\nq2\n\nhalf");
    assert!(t.session.queue.is_empty());
    assert_eq!(set.close_facts(tab).unwrap().queued, 0);
}

#[test]
fn close_facts_and_the_window_count_the_queue_and_a_close_hands_the_texts_back() {
    let mut set = set();
    let one = set.active();
    let two = set.open(AgentId::Claude);
    for (tab, text) in [(one, "a"), (two, "b"), (two, "c")] {
        set.get_mut(tab)
            .unwrap()
            .session
            .queue
            .push(Queued::held(text, text.into(), 0));
    }
    assert_eq!(set.close_facts(two).unwrap().queued, 2);
    assert_eq!(set.queued_count(), 3);
    assert_eq!(
        set.take_every_queued_text(),
        vec!["a".to_string(), "b".into(), "c".into()]
    );
    assert_eq!(set.queued_count(), 0);
}
