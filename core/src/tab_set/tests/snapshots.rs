//! A snapshot and the events after it never deliver the same text twice: the watermark across ticks, switches and resyncs.
#![cfg(test)]

use super::*;

/// Arms the seam (`TabSet::after_drain`): `pump`'s next drain of a live tab's queue is followed,
/// before anything else, by `text` folding into that tab -- the provider streams it and this
/// waits for the ingestion thread to fold it. The fold lands right after a drain and before the
/// next snapshot read, where the race is, not wherever a timing probe happens to.
fn fold_after_next_drain(set: &mut TabSet, provider: &Arc<RecordingProvider>, text: &'static str) {
    let provider = Arc::clone(provider);
    set.after_drain = Some(Box::new(move |backend: &AgentBackend| {
        provider.queue(text_delta("t1", text));
        until("the fold right after the drain", || {
            backend.projection().transcript.iter().any(|m| m.text.contains(text))
        });
    }));
}

/// One run of the double-delivery race: the snapshot a switch or a reload sent (`active_state_payloads`), then the
/// active payload of each of the two ticks after it.
struct P1a2Run {
    snapshot: String,
    next: Option<String>,
    after: Option<String>,
}

/// The race as it first showed: the sidecar's ingestion thread folds on
/// its own thread, so a turn's text folds with NO tick in between -- the real gap between a tick
/// and a switch or reload -- and the switch/reload snapshot carries it while the tab's queue
/// still holds it. `redeliver` turns the `covered` filter off (`TabSet::redeliver_covered`).
fn p1a2_mid_stream(redeliver: bool) -> P1a2Run {
    let dir = workspace("tabs-p1a2-mid-stream");
    let mut set = set();
    set.redeliver_covered = redeliver;
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(started("t1"));
    provider.queue(text_delta("t1", "P1-unique-text"));
    wait_for_fold(&set, tab, "P1-unique-text");
    let run = P1a2Run {
        snapshot: the_snapshot(set.active_state_payloads()),
        next: set.pump(&dir, true).active_payload,
        after: set.pump(&dir, true).active_payload,
    };
    shut_down_all(&mut set);
    run
}

/// The fold-after-a-drain shape, driven by the seam: a tick has delivered the turn's first text, `<between>`
/// folds right after the next tick's drain, the switch/reload snapshot carries it, and `<after>`
/// folds after that snapshot. `redeliver` as for [`p1a2_mid_stream`].
fn p1a2_fold_after_a_drain(redeliver: bool) -> P1a2Run {
    let dir = workspace("tabs-p1a2-fold-after-drain");
    let mut set = set();
    set.redeliver_covered = redeliver;
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(started("t1"));
    provider.queue(text_delta("t1", "<first>"));
    until("the turn's first text to reach the panel", || {
        events_text(set.pump(&dir, true).active_payload.as_deref()).contains("<first>")
    });
    fold_after_next_drain(&mut set, &provider, "<between>");
    assert_eq!(
        set.pump(&dir, true).active_payload,
        None,
        "the tick the fold follows had drained nothing new"
    );
    assert!(set.after_drain.is_none(), "the seam ran, right after that tick's drain");
    let snapshot = the_snapshot(set.active_state_payloads());
    provider.queue(text_delta("t1", "<after>"));
    wait_for_fold(&set, tab, "<after>");
    let run = P1a2Run {
        snapshot,
        next: set.pump(&dir, true).active_payload,
        after: set.pump(&dir, true).active_payload,
    };
    shut_down_all(&mut set);
    run
}

/// A switch or reload taken mid-stream must not be followed by a tick
/// delivering the snapshot's own text again, or the panel renders the reply twice. The
/// snapshot carries the text; the ticks after it send nothing, the whole batch having been in it.
#[test]
fn a_mid_stream_snapshots_text_is_never_delivered_again() {
    let run = p1a2_mid_stream(false);
    assert!(
        snapshot_text(&run.snapshot).contains("P1-unique-text"),
        "the snapshot carries the text: {}",
        snapshot_text(&run.snapshot)
    );
    assert_eq!(
        run.next, None,
        "the next tick must not redeliver what the snapshot carried"
    );
    assert_eq!(run.after, None);
}

/// A drain and a later snapshot read are separate lock
/// acquisitions, and an event folded between them is in the snapshot AND still queued, so the
/// next tick would deliver it again. Driven by the seam, not by timing. Also the no-loss direction:
/// what folds after the snapshot reaches the panel on the next tick, once, and nothing is left.
#[test]
fn a_fold_between_a_ticks_drain_and_a_switchs_snapshot_is_never_delivered_again() {
    let run = p1a2_fold_after_a_drain(false);
    assert!(
        snapshot_text(&run.snapshot).contains("<first><between>"),
        "the snapshot carries the fold: {}",
        snapshot_text(&run.snapshot)
    );
    let delivered = events_text(run.next.as_deref());
    assert!(
        !delivered.contains("<between>"),
        "the next tick must never redeliver what the snapshot carried: {delivered:?}"
    );
    assert_eq!(
        delivered, "<after>",
        "a fold after the snapshot reaches the panel, once, and nothing else does"
    );
    assert_eq!(events_text(run.after.as_deref()), "");
}

/// Both shapes with the `covered` filter off -- the semantics before it -- repeat the snapshot's
/// text on the next tick: the filter, and nothing else, keeps it out. And the fixture
/// `agent-ui/web/src/reducer.p1a2.test.ts` replays through the real `reducer.ts`: each shape's
/// real snapshot and the real `events` of the tick after it, with the filter and without --
/// never a hand-typed guess at their shape.
///
/// Compared on every run, rewritten only under `EITRI_WRITE_FIXTURES=1` and only when it
/// differs: a test in this crate writing into `../agent-ui/web/src` dirtied a tracked file
/// whenever it went red, and a newer file under `src` sets off `shell/build.rs`'s npm rebuild. A
/// checkout without the web tree skips the comparison, having no reducer test to feed.
#[test]
fn with_the_filter_off_the_tick_repeats_the_snapshot_and_the_reducer_fixture_records_both() {
    let events_of = |payload: Option<&str>| -> serde_json::Value {
        payload
            .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
            .map_or(serde_json::json!([]), |v| v["events"].clone())
    };
    let record = |text: &str, filtered: P1a2Run, unfiltered: P1a2Run| {
        let snapshot: serde_json::Value = serde_json::from_str(&filtered.snapshot).unwrap();
        let unfiltered_snapshot: serde_json::Value = serde_json::from_str(&unfiltered.snapshot).unwrap();
        // `conversationId` is `AgentConversation::create`'s own random uuid, which the reducer
        // test never reads: pinned so the bytes are the same on every run.
        let mut state = snapshot["state"].clone();
        state["conversationId"] = serde_json::json!("fixture-conversation-id");
        let mut unfiltered_state = unfiltered_snapshot["state"].clone();
        unfiltered_state["conversationId"] = serde_json::json!("fixture-conversation-id");
        assert_eq!(state, unfiltered_state, "the two runs send the same snapshot");
        assert_eq!(snapshot["throughRevision"], unfiltered_snapshot["throughRevision"]);
        assert!(
            snapshot_text(&filtered.snapshot).contains(text),
            "the snapshot carries {text:?}"
        );
        assert!(
            !events_text(filtered.next.as_deref()).contains(text),
            "with the filter the next tick leaves {text:?} out: {:?}",
            filtered.next
        );
        assert!(
            events_text(unfiltered.next.as_deref()).contains(text),
            "without the filter the next tick sends {text:?} again: {:?}",
            unfiltered.next
        );
        serde_json::json!({
            "text": text,
            "snapshot": state,
            "throughRevision": snapshot["throughRevision"],
            "nextEvents": events_of(filtered.next.as_deref()),
            "nextEventsUnfiltered": events_of(unfiltered.next.as_deref()),
        })
    };
    let fixture = serde_json::json!({
        "writtenBy": "core/src/tab_set.rs, tests::with_the_filter_off_the_tick_repeats_the_snapshot_and_the_reducer_fixture_records_both \
                      -- compared on every run; EITRI_WRITE_FIXTURES=1 rewrites it",
        "midStream": record("P1-unique-text", p1a2_mid_stream(false), p1a2_mid_stream(true)),
        "foldAfterADrain": record("<between>", p1a2_fold_after_a_drain(false), p1a2_fold_after_a_drain(true)),
    });
    let fixture_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/p1a2-mid-stream.json");
    let fixture_json = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
    let committed = std::fs::read_to_string(&fixture_path).ok();
    if committed.as_deref() == Some(fixture_json.as_str()) {
        return;
    }
    if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
        std::fs::write(&fixture_path, &fixture_json).expect("write the p1a2 reducer fixture");
    } else if let Some(committed) = committed {
        assert_eq!(
            committed, fixture_json,
            "the committed p1a2 reducer fixture is not what this test produces; if the change is \
             intended, regenerate it with EITRI_WRITE_FIXTURES=1"
        );
    }
}

/// An earlier catch-up drain at a switch or reload lost turn ends:
/// a `TurnCompleted` a switch or reload took off the queue never reached
/// `PumpOutput::turn_ended` -- the only trigger `eitri_panel::agent_panel`'s queue flush has -- so
/// a message queued behind the turn sat there until something else poked the tab. That drain is
/// gone; what this holds now is that a `TurnCompleted` the snapshot carried,
/// and the next tick therefore leaves out of its payload, is still folded by that tick into
/// `turn_ended`, exactly once. The completion folds with NO tick in between, as in the real race.
#[test]
fn a_turn_end_a_switch_snapshot_carried_still_flushes_the_queue() {
    let dir = workspace("tabs-p1a2-round1-turn-end");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

    provider.queue(started("t1"));
    until("the turn to be delivered as running", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().turn_running()
    });

    set.queue_message(tab, "follow-up", "follow-up-wire".into(), 0, true, PromptOrigin::Typed)
        .expect("queuing behind a running turn");

    provider.queue(completed("t1", agent::TurnOutcome::Completed));
    until("the completion to fold into the projection", || {
        set.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .active_turn_id
            .is_none()
    });

    // The switch/reload snapshot carries the completion; the queue still holds it.
    let snapshot: serde_json::Value = serde_json::from_str(&the_snapshot(set.active_state_payloads())).unwrap();
    assert_eq!(snapshot["state"]["activeTurnId"], serde_json::Value::Null);

    let mut turn_ended_reported = Vec::new();
    for _ in 0..20 {
        let out = set.pump(&dir, true);
        assert!(
            !out.active_payload
                .as_deref()
                .unwrap_or_default()
                .contains("turn_completed"),
            "the snapshot carried the completion; no tick sends it again"
        );
        turn_ended_reported.extend(out.turn_ended);
    }
    assert_eq!(
        turn_ended_reported,
        vec![tab],
        "the turn's end must still be reported exactly once after a snapshot carried it"
    );
    assert_eq!(
        set.get(tab).unwrap().session.queue.len(),
        1,
        "the queued message must still be waiting for the flush this enables"
    );
    let flush = set
        .flush_queue(tab)
        .expect("the turn having ended must let the queue flush");
    assert_eq!(flush.typed, "follow-up");
    shut_down_all(&mut set);
}

/// Without a drain ahead of the snapshot, its turn clock must still match its own turn: a turn
/// that started since the last tick is named by the snapshot, and the snapshot says when it
/// started -- never the start of the turn before it, which the panel would take as exact.
#[test]
fn a_snapshot_names_the_start_of_the_turn_it_carries_even_before_a_tick_saw_it() {
    let dir = workspace("tabs-p1a2-turn-clock");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(started("t1"));
    pump_until_running(&mut set, &dir, tab, true);
    let first = set.get(tab).unwrap().turn_started_at_ms().expect("t1 stamped");
    std::thread::sleep(Duration::from_millis(20));

    provider.queue(completed("t1", agent::TurnOutcome::Completed));
    provider.queue(started("t2"));
    until("t2 to fold, with no tick", || {
        set.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .active_turn_id
            .as_deref()
            == Some("t2")
    });
    let before = wall_clock_ms();
    let snapshot: serde_json::Value = serde_json::from_str(&the_snapshot(set.active_state_payloads())).unwrap();
    assert_eq!(snapshot["state"]["activeTurnId"], "t2");
    let at = snapshot["turnStartedAtMs"]
        .as_u64()
        .expect("the snapshot says when t2 started");
    assert!(at > first && at >= before, "t2's start, not t1's ({first}): {at}");
    set.pump(&dir, true);
    assert_eq!(
        set.get(tab).unwrap().turn_started_at_ms(),
        Some(at),
        "the tick keeps the clock the snapshot sent"
    );
    shut_down_all(&mut set);
}

/// `pump`'s own `Resync` arm has the same shape -- the drain returns
/// `Resync`, and the snapshot it sends is read after further lock cycles (and a bypass sweep) --
/// so a fold in between would be in that snapshot and queued for the next tick. Driven the same way:
/// an overflow owes the tab a resync, and the seam folds a unique delta right after that drain.
#[test]
fn a_fold_between_a_resyncs_drain_and_its_snapshot_read_is_never_delivered_again() {
    let dir = workspace("tabs-p1a2-round2-resync");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

    let base = set.get(tab).unwrap().live().unwrap().projection().last_revision;
    let overflow = agent::UI_EVENT_QUEUE_CAPACITY + 10;
    provider.queue(started("t1"));
    for _ in 0..overflow {
        provider.queue(text_delta("t1", "."));
    }
    until("the overflow to fold with nothing draining it", || {
        set.get(tab).unwrap().live().unwrap().projection().last_revision > base + overflow as u64
    });

    fold_after_next_drain(&mut set, &provider, "<between>");
    let payload = set
        .pump(&dir, true)
        .active_payload
        .expect("a resync sends the active tab a snapshot");
    assert!(
        set.after_drain.is_none(),
        "the seam ran, between the drain and the read"
    );
    assert!(
        snapshot_text(&payload).ends_with("<between>"),
        "the drain was a Resync, and its snapshot carries the fold"
    );

    provider.queue(text_delta("t1", "<after>"));
    wait_for_fold(&set, tab, "<after>");
    let delivered = events_text(set.pump(&dir, true).active_payload.as_deref());
    assert!(
        !delivered.contains("<between>"),
        "the next pump must never redeliver what the resync's snapshot carried: {delivered:?}"
    );
    assert_eq!(
        delivered, "<after>",
        "a fold after the snapshot reaches the panel, once"
    );
    shut_down_all(&mut set);
}

/// The `Resync` arm's own early turn-clock read -- taken right after the
/// drain, before this arm's further lock cycles (its one read of the projection and
/// `approve_pending`'s bypass sweep) -- was used to stamp the payload this arm actually sends,
/// which is read only afterward, from a fresh `SnapshotView::of`. If a turn ends and the next
/// starts in that gap, the snapshot names the new turn while the payload still carries the old
/// one's start time, and the panel takes that stamp as exact. Driven by a seam positioned exactly
/// where `after_drain` (fired right after the drain, before any of this arm's own lock cycles)
/// cannot reach: right before this arm's own snapshot read.
#[test]
fn a_resyncs_snapshot_names_the_start_of_the_turn_it_carries_even_when_the_race_ends_it_first() {
    let dir = workspace("tabs-p1a2-round4-resync-clock");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

    provider.queue(started("t1"));
    pump_until_running(&mut set, &dir, tab, true);
    let first = set.get(tab).unwrap().turn_started_at_ms().expect("t1 stamped");
    std::thread::sleep(Duration::from_millis(20));

    // Overflows the queue so this tick's drain is a `Resync`, with nothing draining it meanwhile.
    let base = set.get(tab).unwrap().live().unwrap().projection().last_revision;
    let overflow = agent::UI_EVENT_QUEUE_CAPACITY + 10;
    for _ in 0..overflow {
        provider.queue(text_delta("t1", "."));
    }
    until("the overflow to fold with nothing draining it", || {
        set.get(tab).unwrap().live().unwrap().projection().last_revision >= base + overflow as u64
    });

    let seam_provider = Arc::clone(&provider);
    set.before_resync_snapshot = Some(Box::new(move |backend: &AgentBackend| {
        seam_provider.queue(completed("t1", agent::TurnOutcome::Completed));
        seam_provider.queue(started("t2"));
        until("t1 to end and t2 to start, right before the snapshot read", || {
            backend.projection().active_turn_id.as_deref() == Some("t2")
        });
    }));

    let before = wall_clock_ms();
    let payload = set
        .pump(&dir, true)
        .active_payload
        .expect("a resync sends the active tab a snapshot");
    assert!(
        set.before_resync_snapshot.is_none(),
        "the seam ran, right before the snapshot read"
    );

    let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(
        value["state"]["activeTurnId"], "t2",
        "the resync's own snapshot names t2, not t1"
    );
    let at = value["turnStartedAtMs"]
        .as_u64()
        .expect("the snapshot says when t2 started");
    assert!(at > first && at >= before, "t2's start, not t1's ({first}): {at}");

    shut_down_all(&mut set);
}

/// A snapshot's revision belongs to its session. A tab whose session is replaced
/// right after a snapshot, with no pump in between, must not measure the new session's events
/// -- whose revisions start again from its own seed -- against the old one's snapshot. Here
/// through `collect_starts`; the next test replaces it the way `shell` does.
#[test]
fn a_new_sessions_events_are_never_measured_against_the_old_sessions_snapshot() {
    let dir = workspace("tabs-p1a2-round2-install");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    let base = backend.projection().last_revision;
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(started("t1"));
    for _ in 0..5 {
        provider.queue(text_delta("t1", "."));
    }
    provider.queue(text_delta("t1", "<old>"));
    wait_for_fold(&set, tab, "<old>");
    let snapshot: serde_json::Value = serde_json::from_str(&the_snapshot(set.active_state_payloads())).unwrap();
    assert!(
        snapshot["throughRevision"].as_u64().is_some_and(|r| r >= base + 7),
        "the old session's snapshot covers more revisions than the new one will reach below"
    );

    let old = std::mem::replace(&mut set.get_mut(tab).unwrap().backend, TabBackend::NotStarted);
    if let TabBackend::Live(mut old) = old {
        old.shutdown();
    }
    let tx = starting(&mut set, tab, None, None);
    let (new_provider, new_backend) = live(&dir);
    tx.send(Ok(new_backend)).unwrap();
    assert_eq!(set.collect_starts().len(), 1, "installed");
    new_provider.queue(started("t2"));
    new_provider.queue(text_delta("t2", "<new-session>"));
    // Folded before the first drain, so that drain holds revisions the old snapshot would cover.
    wait_for_fold(&set, tab, "<new-session>");
    let delivered = events_text(set.pump(&dir, true).active_payload.as_deref());
    assert_eq!(
        delivered, "<new-session>",
        "the new session's first text reaches the panel"
    );
    shut_down_all(&mut set);
}

/// A snapshot's watermark belongs to the session it was read
/// from and goes with it, whatever replaces the backend. `shell`'s fatal-command and
/// never-opened paths swap in `Failed` themselves, passing through neither `collect_starts` nor
/// the pump; here the next session is installed straight after, with no tick in between either.
#[test]
fn a_replaced_backend_takes_its_snapshot_watermark_with_it() {
    let dir = workspace("tabs-p1a2-watermark-replaced");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    let base = backend.projection().last_revision;
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(started("t1"));
    for _ in 0..5 {
        provider.queue(text_delta("t1", "."));
    }
    provider.queue(text_delta("t1", "<old>"));
    wait_for_fold(&set, tab, "<old>");
    let snapshot: serde_json::Value = serde_json::from_str(&the_snapshot(set.active_state_payloads())).unwrap();
    assert!(
        snapshot["throughRevision"].as_u64().is_some_and(|r| r >= base + 7),
        "the old session's snapshot covers more revisions than the new one will reach below"
    );

    // As `eitri_panel::agent_panel` retires a session whose command failed fatally.
    let old = std::mem::replace(
        &mut set.get_mut(tab).unwrap().backend,
        TabBackend::Failed {
            reason: "a fatal command".into(),
        },
    );
    if let TabBackend::Live(mut old) = old {
        old.shutdown();
    }
    let (new_provider, new_backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(new_backend);
    new_provider.queue(started("t2"));
    new_provider.queue(text_delta("t2", "<new-session>"));
    wait_for_fold(&set, tab, "<new-session>");
    assert_eq!(
        events_text(set.pump(&dir, true).active_payload.as_deref()),
        "<new-session>",
        "the new session's text is measured against no snapshot of the old one's"
    );
    shut_down_all(&mut set);
}

/// `shell` reports the tray's attention around the tick and a
/// switch, never around a panel reload (`InboundMessage::Ready`) -- so reading the active tab's
/// state must not move it. A card folded since the last tick is drawn by the snapshot and
/// counted by the next pump, which the tick reports; the pump does not send its request again.
#[test]
fn reading_the_active_tabs_state_leaves_the_windows_attention_alone() {
    let dir = workspace("tabs-p1a2-attention");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(write("perm-write"));
    until("the card to fold, with no pump", || {
        let backend = set.get(tab).unwrap().live().unwrap();
        let projection = backend.projection();
        projection.pending_permissions.contains_key("perm-write")
    });

    let before = set.attention();
    let snapshot: serde_json::Value = serde_json::from_str(&set.active_state_payloads()[0]).unwrap();
    assert_eq!(
        set.attention(),
        before,
        "reading a tab's state reports nothing to the tray"
    );
    assert_eq!(snapshot["state"]["pendingPermissions"][0]["permissionId"], "perm-write");

    let payload = set.pump(&dir, true).active_payload;
    assert_eq!(set.attention().pending, 1, "the tick counts the card");
    assert!(
        !payload.as_deref().unwrap_or_default().contains("permission_requested"),
        "the snapshot drew the card; the tick does not send it again: {payload:?}"
    );
    shut_down_all(&mut set);
}

#[test]
fn view_activity_counts_running_and_queued_per_view() {
    let dir = workspace("tabs-view-activity");
    let mut set = set();
    let base = set.active();
    let queued = set.open(AgentId::Claude);
    let two = ViewId::new(2).unwrap();
    assert_eq!(set.break_out(two), Ok(queued));
    let (provider, backend) = live(&dir);
    set.get_mut(base).unwrap().backend = TabBackend::Live(backend);
    let (_queued_provider, backend) = live(&dir);
    set.get_mut(queued).unwrap().backend = TabBackend::Live(backend);
    set.queue_message(queued, "later", "later".into(), 1, false, PromptOrigin::Typed)
        .unwrap();
    provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    until("the turn starts", || set.running_count() == 1);
    assert_eq!(
        set.view_activity(),
        vec![
            (ViewId::BASE, ViewActivity { running: 1, queued: 0 }),
            (two, ViewActivity { running: 0, queued: 1 }),
        ]
    );
    assert_eq!(set.queued_count(), 1);
    shut_down_all(&mut set);
}

#[test]
fn the_active_tabs_events_are_dispatched_with_its_tab() {
    let dir = workspace("tabs-active-pump");
    let mut set = set();
    let active = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(active).unwrap().backend = TabBackend::Live(backend);
    provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    let mut payload = None;
    until("the active tab's events", || {
        payload = set.pump(&dir, true).active_payload;
        payload.is_some()
    });
    let value: serde_json::Value = serde_json::from_str(&payload.unwrap()).unwrap();
    assert_eq!(value["kind"], "events");
    assert_eq!(value["tab"], active.0);
    assert!(!set.get(active).unwrap().session.stale);
    assert_eq!(set.running_count(), 1, "a turn is running");
    shut_down_all(&mut set);
}
