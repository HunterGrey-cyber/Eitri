//! The tail-limited snapshot, pages, cut entries, `Ended`, `SendBlock` and the greeting.

use super::*;
use crate::remote_greeting::{greeting_value, GREETING_FIELDS};
use crate::remote_rv::{chunk_lines, frame_line, parse_rv_outbound, RvOutbound};

/// A conversation of `turns` turns, each a prompt naming its number and a reply of `reply_bytes`, all completed.
fn turns_of(turns: usize, reply_bytes: usize) -> Vec<AgentDomainEvent> {
    let reply = "r".repeat(reply_bytes);
    let mut events = Vec::with_capacity(turns * 4);
    for n in 0..turns {
        let turn = format!("turn-{n}");
        events.push(AgentDomainEvent::TurnStarted { turn_id: turn.clone() });
        events.push(AgentDomainEvent::UserPromptSubmitted {
            text: format!("prompt {n}"),
        });
        events.push(AgentDomainEvent::ContentDelta {
            turn_id: turn.clone(),
            kind: agent::ContentKind::Text,
            text: reply.clone(),
        });
        events.push(completed(&turn));
    }
    events
}

fn completed(turn: &str) -> AgentDomainEvent {
    AgentDomainEvent::TurnCompleted {
        turn_id: turn.into(),
        outcome: agent::TurnOutcome::Completed,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
        detail: Default::default(),
    }
}

/// An opened tab that has folded `events`, pumped.
fn tab_with(label: &str, events: Vec<AgentDomainEvent>) -> (PathBuf, TabSet, TabId, Arc<RecordingProvider>) {
    let (dir, mut set, tab, provider) = opened_tab(label);
    let through = revision(&set, tab) + events.len() as u64;
    provider.queue_all(events);
    folded(&set, tab, through);
    set.pump(&dir, true);
    (dir, set, tab, provider)
}

/// The snapshot frame's fields and its envelope, parsed.
fn snapshot_of(frames: &[ObserverFrame]) -> (u64, Vec<CutEntry>, usize, serde_json::Value) {
    let Some(ObserverFrame::Snapshot {
        first_turn,
        cut,
        json: text,
        ..
    }) = frames.first()
    else {
        panic!("a snapshot first: {frames:?}");
    };
    (*first_turn, cut.clone(), text.len(), json(text))
}

/// The texts of the prompts an envelope's state holds, in order.
fn prompts(payload: &serde_json::Value) -> Vec<String> {
    payload["state"]["userPrompts"]
        .as_array()
        .expect("the prompts")
        .iter()
        .map(|p| p["text"].as_str().unwrap().to_owned())
        .collect()
}

fn numbered(range: std::ops::Range<usize>) -> Vec<String> {
    range.map(|n| format!("prompt {n}")).collect()
}

fn generation(set: &TabSet, tab: TabId) -> u64 {
    set.get(tab).unwrap().backend_generation()
}

/// The page frame among `frames`: its first and last turn and its envelope.
fn page_of(frames: &[ObserverFrame]) -> (u64, u64, WorkMark, serde_json::Value) {
    let pages: Vec<_> = frames
        .iter()
        .filter_map(|f| match f {
            ObserverFrame::Page {
                from_turn,
                through_turn,
                mark,
                json: text,
                ..
            } => Some((*from_turn, *through_turn, *mark, json(text))),
            _ => None,
        })
        .collect();
    assert_eq!(pages.len(), 1, "one page: {frames:?}");
    pages.into_iter().next().unwrap()
}

#[test]
fn snapshot_holds_the_last_100_turns() {
    let (_dir, mut set, tab, _provider) = tab_with("observers-tail-100", turns_of(150, 200));
    let id = set.observe(&key(&set, tab)).unwrap();
    let (first_turn, cut, _, payload) = snapshot_of(&set.take_observer_frames(id, usize::MAX));
    assert_eq!(first_turn, 50, "the last {SNAPSHOT_TURNS} of 150 turns");
    assert!(cut.is_empty());
    assert_eq!(prompts(&payload), numbered(50..150));
    let replies = payload["state"]["transcript"].as_array().unwrap();
    assert_eq!(replies.len(), 100, "each kept turn's reply, and no older one");
    shut_down(&mut set);

    let (_dir, mut set, tab, _provider) = tab_with("observers-tail-short", turns_of(30, 200));
    let id = set.observe(&key(&set, tab)).unwrap();
    let (first_turn, cut, _, payload) = snapshot_of(&set.take_observer_frames(id, usize::MAX));
    assert_eq!((first_turn, cut.len()), (0, 0), "a short conversation is sent whole");
    assert_eq!(prompts(&payload), numbered(0..30));
    shut_down(&mut set);
}

#[test]
fn snapshot_stays_under_the_ceiling() {
    let (_dir, mut set, tab, _provider) = tab_with("observers-tail-ceiling", turns_of(300, 10_000));
    let id = set.observe(&key(&set, tab)).unwrap();
    let (first_turn, cut, bytes, payload) = snapshot_of(&set.take_observer_frames(id, usize::MAX));
    assert!(bytes <= SNAPSHOT_CEILING_BYTES, "{bytes} bytes");
    assert!(
        first_turn > 200,
        "100 turns of 10 kB do not fit, so fewer are sent: from {first_turn}"
    );
    assert!(first_turn < 220, "as many whole turns as fit: from {first_turn}");
    assert!(cut.is_empty(), "no entry is larger than the ceiling");
    assert_eq!(
        prompts(&payload),
        numbered(first_turn as usize..300),
        "whole turns, up to the last"
    );
    shut_down(&mut set);
}

#[test]
fn huge_entry_is_cut_and_fetched_whole_in_chunks() {
    let huge: String = (0..10 * 1024 * 1024)
        .map(|n| char::from(b'a' + (n % 26) as u8))
        .collect();
    let mut events = turns_of(3, 100);
    events.extend([
        AgentDomainEvent::TurnStarted {
            turn_id: "turn-huge".into(),
        },
        AgentDomainEvent::UserPromptSubmitted {
            text: "prompt huge".into(),
        },
        AgentDomainEvent::ContentDelta {
            turn_id: "turn-huge".into(),
            kind: agent::ContentKind::Text,
            text: huge.clone(),
        },
        completed("turn-huge"),
    ]);
    let after = turns_of(2, 100);
    events.extend(after);
    let (_dir, mut set, tab, _provider) = tab_with("observers-huge-entry", events);
    let id = set.observe(&key(&set, tab)).unwrap();
    let (first_turn, cut, bytes, payload) = snapshot_of(&set.take_observer_frames(id, usize::MAX));
    assert!(bytes <= SNAPSHOT_CEILING_BYTES, "{bytes} bytes");
    assert_eq!(first_turn, 0, "the other turns still fit beside the cut entry");
    let [CutEntry { entry_id, bytes: whole }] = &cut[..] else {
        panic!("one cut entry: {cut:?}");
    };
    assert!(*whole as usize >= huge.len(), "{whole}");
    let seq: u64 = entry_id
        .strip_prefix("transcript:")
        .expect("the reply is cut")
        .parse()
        .unwrap();
    let kept = payload["state"]["transcript"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["seq"] == seq)
        .expect("the cut entry is in the snapshot")["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(kept.len() <= CUT_KEEP_BYTES && huge.starts_with(&kept) && !kept.is_empty());

    // Its turn (the fourth) asked for whole: a page of that turn alone, longer than a line, sent as chunks.
    let generation = generation(&set, tab);
    set.observer_more(id, generation, 4)
        .expect("a page of the turns before 4");
    let frames = set.take_observer_frames(id, usize::MAX);
    let (from_turn, through_turn, _, _) = page_of(&frames);
    assert_eq!((from_turn, through_turn), (3, 3), "the large turn alone");
    let page = frames.iter().find(|f| matches!(f, ObserverFrame::Page { .. })).unwrap();
    let lines = chunk_lines(5, &frame_line(page));
    assert!(lines.len() > 1, "a page past the line cap goes in chunks");
    let mut joined = String::new();
    for line in &lines {
        let Ok(RvOutbound::Chunk { of: 5, data, .. }) = parse_rv_outbound(line.as_bytes()) else {
            panic!("a chunk of line 5");
        };
        joined.push_str(&data);
    }
    let Ok(RvOutbound::Page { frame, .. }) = parse_rv_outbound(joined.as_bytes()) else {
        panic!("the chunks make the page's line");
    };
    let whole_text = frame["state"]["transcript"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["seq"] == seq)
        .expect("the entry is in its page")["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(whole_text == huge, "the page holds the entry whole");
    assert_eq!(prompts(&frame), ["prompt huge"]);
    shut_down(&mut set);
}

#[test]
fn more_pages_back_without_gaps() {
    let (_dir, mut set, tab, _provider) = tab_with("observers-more-pages", turns_of(350, 200));
    let id = set.observe(&key(&set, tab)).unwrap();
    let (first_turn, _, _, payload) = snapshot_of(&set.take_observer_frames(id, usize::MAX));
    assert_eq!(first_turn, 250);
    let generation = generation(&set, tab);
    let mut held = prompts(&payload);
    let mut before = first_turn;
    let mut pages = 0;
    while before > 0 {
        set.observer_more(id, generation, before).expect("a page");
        assert_eq!(set.observers_with_frames(), [id], "the page waits to be taken");
        let (from, through, mark, page) = page_of(&set.take_observer_frames(id, usize::MAX));
        assert_eq!(through, before - 1, "a page ends where what the phone holds begins");
        assert!(before - from <= MORE_PAGE_TURNS as u64);
        assert_eq!(mark.generation, generation);
        let mut older = prompts(&page);
        assert_eq!(older.len() as u64, through - from + 1, "{older:?}");
        older.append(&mut held);
        held = older;
        before = from;
        pages += 1;
    }
    assert_eq!(pages, 3);
    assert_eq!(held, numbered(0..350), "every turn once, in order");
    assert_eq!(set.observer_more(id, generation, 0), Err(MoreRefusal::NoSuchTurn));
    assert_eq!(set.observer_more(id, generation, 351), Err(MoreRefusal::NoSuchTurn));
    assert!(set.observers_with_frames().is_empty());
    shut_down(&mut set);
}

#[test]
fn more_across_a_replaced_session_is_refused() {
    let (dir, mut set, tab, _provider) = tab_with("observers-more-replaced", turns_of(5, 100));
    let id = set.observe(&key(&set, tab)).unwrap();
    set.take_observer_frames(id, usize::MAX);
    let old = generation(&set, tab);
    // A page owed to the old session is not built for the new one.
    set.observer_more(id, old, 3).unwrap();

    let (second, backend) = conversation(&dir);
    if let TabBackend::Live(mut gone) = set.get_mut(tab).unwrap().set_backend(backend) {
        gone.shutdown();
    }
    let mut events = vec![opened(&dir)];
    events.extend(turns_of(4, 100));
    let through = events.len() as u64;
    second.queue_all(events);
    folded(&set, tab, through);
    set.pump(&dir, true);
    let new = generation(&set, tab);
    assert_ne!(old, new);
    assert_eq!(set.observer_more(id, old, 1), Err(MoreRefusal::WorkChanged));
    assert_eq!(
        set.observer_more(id, new, 1),
        Err(MoreRefusal::WorkChanged),
        "nothing of the new session before its snapshot"
    );
    let frames = set.take_observer_frames(id, usize::MAX);
    assert!(
        matches!(&frames[..], [ObserverFrame::Snapshot { .. }]),
        "the new session's snapshot and no page: {frames:?}"
    );
    assert_eq!(set.observer_more(id, old, 1), Err(MoreRefusal::WorkChanged));
    set.observer_more(id, new, 2)
        .expect("a page of the session it now follows");
    let (from, through, mark, _) = page_of(&set.take_observer_frames(id, usize::MAX));
    assert_eq!((from, through, mark.generation), (0, 1, new));
    shut_down(&mut set);
}

#[test]
fn second_more_while_one_is_owed_is_refused() {
    let (_dir, mut set, tab, _provider) = tab_with("observers-more-owed", turns_of(10, 100));
    let id = set.observe(&key(&set, tab)).unwrap();
    set.take_observer_frames(id, usize::MAX);
    let generation = generation(&set, tab);
    set.observer_more(id, generation, 5).unwrap();
    for _ in 0..1000 {
        assert_eq!(set.observer_more(id, generation, 9), Err(MoreRefusal::Outstanding));
    }
    let frames = set.take_observer_frames(id, usize::MAX);
    let (from, through, _, _) = page_of(&frames);
    assert_eq!((from, through), (0, 4), "the page first asked for, once");
    set.observer_more(id, generation, 9)
        .expect("the next page once the last was taken");
    assert_eq!(set.observer_more(id, generation, 10), Err(MoreRefusal::Outstanding));
    shut_down(&mut set);
}

#[test]
fn page_is_built_within_the_take_budget() {
    let (_dir, mut set, tab, _provider) = tab_with("observers-more-budget", turns_of(200, 2_000));
    let id = set.observe(&key(&set, tab)).unwrap();
    let (first_turn, _, _, _) = snapshot_of(&set.take_observer_frames(id, usize::MAX));
    assert_eq!(first_turn, 100);
    let generation = generation(&set, tab);
    set.observer_more(id, generation, first_turn).unwrap();
    let budget = 150_000;
    let frames = set.take_observer_frames(id, budget);
    let (from, through, _, _) = page_of(&frames);
    let bytes: usize = frames.iter().map(frame_bytes).sum();
    assert!(bytes <= budget, "{bytes} bytes for a take of {budget}");
    assert_eq!(through, 99);
    assert!(
        through - from + 1 < MORE_PAGE_TURNS as u64,
        "fewer turns, to fit: {from}..={through}"
    );
    shut_down(&mut set);
}

#[test]
fn failed_session_sends_ended() {
    let (dir, mut set, tab, provider) = opened_tab("observers-ended-failed");
    let id = set.observe(&key(&set, tab)).unwrap();
    let mut phone = Phone::default();
    phone.apply(set.take_observer_frames(id, usize::MAX));
    provider.queue_all(vec![started("t1"), delta("before the failure")]);
    folded(&set, tab, 3);
    set.pump(&dir, true);
    // A CLI out of the gate's control fails the tab at the next tick.
    provider.queue(AgentDomainEvent::UngatedCliMode {
        reported: "bypassPermissions".into(),
        detail: "SessionReady".into(),
    });
    folded(&set, tab, 4);
    set.pump(&dir, true);
    assert!(matches!(set.get(tab).unwrap().backend(), TabBackend::Failed { .. }));
    assert_eq!(set.observers_with_frames(), [id]);
    phone.apply(set.take_observer_frames(id, usize::MAX));
    assert_eq!(phone.revisions, [2, 3], "what was queued comes first");
    assert!(
        matches!(
            &phone.others[..],
            [
                ObserverFrame::Ended {
                    how: EndedHow::Failed,
                    ..
                },
                ObserverFrame::SendBlock {
                    block: Some(SendBlock::NotStarted),
                    ..
                }
            ]
        ),
        "{:?}",
        phone.others
    );
    assert!(phone.gone.is_none(), "the tab is still here");
    assert!(set.observers_with_frames().is_empty(), "said once");
    assert!(set.take_observer_frames(id, usize::MAX).is_empty());
    shut_down(&mut set);

    let (dir, mut set, tab, provider) = opened_tab("observers-ended-lost");
    let id = set.observe(&key(&set, tab)).unwrap();
    set.take_observer_frames(id, usize::MAX);
    provider.queue(AgentDomainEvent::SessionClosed {
        reason: "the CLI exited".into(),
    });
    folded(&set, tab, 2);
    set.pump(&dir, true);
    let frames = set.take_observer_frames(id, usize::MAX);
    assert!(
        matches!(
            frames.last(),
            Some(ObserverFrame::Ended {
                how: EndedHow::Lost,
                ..
            })
        ),
        "{frames:?}"
    );
    assert!(set.observers_with_frames().is_empty(), "said once");
    // A new session in the tab: its snapshot, and nothing ended.
    let (second, backend) = conversation(&dir);
    if let TabBackend::Live(mut gone) = set.get_mut(tab).unwrap().set_backend(backend) {
        gone.shutdown();
    }
    second.queue(opened(&dir));
    folded(&set, tab, 1);
    set.pump(&dir, true);
    let frames = set.take_observer_frames(id, usize::MAX);
    assert!(matches!(&frames[..], [ObserverFrame::Snapshot { .. }]), "{frames:?}");
    shut_down(&mut set);
}

#[test]
fn observer_turn_times_come_from_its_snapshot() {
    let (dir, mut set, tab, provider) = opened_tab("observers-turn-times");
    // Two turns the tick saw run and end, so the tab times them.
    for (n, turn) in ["t1", "t2"].into_iter().enumerate() {
        let at = revision(&set, tab);
        provider.queue_all(vec![
            started(turn),
            AgentDomainEvent::ContentDelta {
                turn_id: turn.into(),
                kind: agent::ContentKind::Text,
                text: format!("reply {n}"),
            },
        ]);
        folded(&set, tab, at + 2);
        set.pump(&dir, true);
        provider.queue(completed(turn));
        folded(&set, tab, at + 3);
        set.pump(&dir, true);
    }
    let timed: Vec<String> = set
        .get(tab)
        .unwrap()
        .notes
        .turn_times
        .iter()
        .map(|t| t.turn_id.clone())
        .collect();
    assert_eq!(timed, ["t1", "t2"]);
    let unsent = set.get(tab).unwrap().session.turn_times_unsent;
    let id = set.observe(&key(&set, tab)).unwrap();
    let (_, _, _, payload) = snapshot_of(&set.take_observer_frames(id, usize::MAX));
    let ids = |list: &serde_json::Value| -> Vec<String> {
        list.as_array()
            .map(|l| l.iter().map(|t| t["turnId"].as_str().unwrap().to_owned()).collect())
            .unwrap_or_default()
    };
    assert_eq!(
        ids(&payload["turnTimes"]),
        ["t1", "t2"],
        "the snapshot carries the tab's times"
    );
    assert_eq!(
        set.get(tab).unwrap().session.turn_times_unsent,
        unsent,
        "the desk's own count is untouched"
    );

    let at = revision(&set, tab);
    provider.queue(started("t3"));
    folded(&set, tab, at + 1);
    set.pump(&dir, true);
    provider.queue(completed("t3"));
    folded(&set, tab, at + 2);
    set.pump(&dir, true);
    let mut later = Vec::new();
    for frame in set.take_observer_frames(id, usize::MAX) {
        if let ObserverFrame::Events { json: text, .. } = frame {
            later.extend(ids(&json(&text)["turnTimes"]));
        }
    }
    assert_eq!(later, ["t3"], "only the times after those its snapshot carried");
    shut_down(&mut set);
}

#[test]
fn greeting_carries_exactly_its_fields() {
    let (_dir, mut set, tab, _provider) = opened_tab("observers-greeting");
    let key = key(&set, tab);
    let theme = crate::theme::ThemeTokens::fallback();
    let greeting = set
        .remote_greeting(&key, Some("the folder is your home"), &theme)
        .expect("the tab is here");
    let value = greeting_value(&greeting);
    let mut fields: Vec<&str> = value.as_object().unwrap().keys().map(String::as_str).collect();
    fields.sort_unstable();
    let mut expected = GREETING_FIELDS.to_vec();
    expected.sort_unstable();
    assert_eq!(fields, expected, "{value}");
    assert_eq!(value["root_note"], "the folder is your home");
    assert_eq!(
        value["send_block"],
        serde_json::Value::Null,
        "a live Claude tab in Auto takes messages"
    );
    assert_eq!(value["prompt"], serde_json::Value::Null);
    assert_eq!(value["shell_running"], false);
    assert_eq!(value["tab"]["agent"], "claude");
    assert_eq!(value["tab"]["state"], "live");
    assert!(value["tab"].get("questionDraft").is_none(), "{value}");
    let vars = value["theme"]["vars"].as_object().unwrap();
    assert!(
        !vars.is_empty() && vars.keys().all(|k| !k.starts_with("--nv-font")),
        "{value}"
    );
    let text = value.to_string();
    for absent in [
        "projectDir",
        "\"resumable\":[",
        "ruleOffers",
        "rule_offers",
        "pane_focus",
        "editor_context",
        "keymap",
    ] {
        assert!(!text.contains(absent), "{absent} in {value}");
    }
    assert!(set.remote_greeting(&RowKey::new(), None, &theme).is_none());
    set.get_mut(tab).unwrap().mode = SessionModeChoice::Bypass;
    let greeting = set.remote_greeting(&key, None, &theme).unwrap();
    assert_eq!(greeting.send_block, Some(SendBlock::Bypass));
    assert_eq!(greeting_value(&greeting)["root_note"], serde_json::Value::Null);
    shut_down(&mut set);
}

#[test]
fn mode_change_sends_send_block() {
    let (_dir, mut set, tab, _provider) = opened_tab("observers-send-block");
    let id = set.observe(&key(&set, tab)).unwrap();
    let frames = set.take_observer_frames(id, usize::MAX);
    assert!(
        matches!(&frames[..], [ObserverFrame::Snapshot { .. }]),
        "the greeting already said it may send: {frames:?}"
    );
    let mut said = Vec::new();
    for mode in [
        SessionModeChoice::Bypass,
        SessionModeChoice::Bypass,
        SessionModeChoice::Manual,
        SessionModeChoice::Auto,
    ] {
        set.get_mut(tab).unwrap().mode = mode;
        for frame in set.take_observer_frames(id, usize::MAX) {
            match frame {
                ObserverFrame::SendBlock { block, .. } => said.push(block),
                other => panic!("only the send block: {other:?}"),
            }
        }
    }
    assert_eq!(
        said,
        [Some(SendBlock::Bypass), None],
        "one frame for each change, none for the same mode again or Manual to Auto"
    );
    assert!(set.observers_with_frames().is_empty());
    shut_down(&mut set);
}

#[test]
fn new_session_failing_before_its_snapshot_sends_ended_once() {
    let (dir, mut set, tab, _provider) = opened_tab("observers-ended-once");
    let id = set.observe(&key(&set, tab)).unwrap();
    set.take_observer_frames(id, usize::MAX);
    // A new session lands and fails before the observer's next take, so no snapshot of it is ever built.
    let (second, backend) = conversation(&dir);
    if let TabBackend::Live(mut gone) = set.get_mut(tab).unwrap().set_backend(backend) {
        gone.shutdown();
    }
    second.queue_all(vec![
        opened(&dir),
        AgentDomainEvent::UngatedCliMode {
            reported: "bypassPermissions".into(),
            detail: "SessionReady".into(),
        },
    ]);
    folded(&set, tab, 2);
    set.pump(&dir, true);
    assert!(matches!(set.get(tab).unwrap().backend(), TabBackend::Failed { .. }));
    let mut ended = 0;
    for _ in 0..5 {
        for frame in set.take_observer_frames(id, usize::MAX) {
            if matches!(frame, ObserverFrame::Ended { .. }) {
                ended += 1;
            }
        }
    }
    assert_eq!(ended, 1, "the failed session is told once");
    assert!(
        set.observers_with_frames().is_empty(),
        "nothing left to take, so nothing wakes the socket again"
    );
    shut_down(&mut set);
}

#[test]
fn clear_owes_a_snapshot_and_drops_the_owed_page() {
    let (dir, mut set, tab, provider) = tab_with("observers-more-clear", turns_of(5, 100));
    let id = set.observe(&key(&set, tab)).unwrap();
    set.take_observer_frames(id, usize::MAX);
    let session = generation(&set, tab);
    set.observer_more(id, session, 3).unwrap();
    // `/clear`: the same session, a new conversation whose turns are numbered from 0 again.
    let mut events = vec![AgentDomainEvent::ConversationReset {
        new_conversation_id: "conversation-after-clear".into(),
        trigger: Some("clear".into()),
    }];
    events.extend(turns_of(2, 100).into_iter().map(|event| match event {
        AgentDomainEvent::UserPromptSubmitted { text } => AgentDomainEvent::UserPromptSubmitted {
            text: format!("after {text}"),
        },
        other => other,
    }));
    let through = revision(&set, tab) + events.len() as u64;
    provider.queue_all(events);
    folded(&set, tab, through);
    set.pump(&dir, true);
    assert_eq!(generation(&set, tab), session, "the same session");
    assert_eq!(
        set.observer_more(id, session, 1),
        Err(MoreRefusal::WorkChanged),
        "no turn of the new conversation before its snapshot"
    );
    let frames = set.take_observer_frames(id, usize::MAX);
    assert!(
        matches!(&frames[..], [ObserverFrame::Snapshot { .. }]),
        "the new conversation's snapshot, and no page of the old one: {frames:?}"
    );
    let (first_turn, _, _, payload) = snapshot_of(&frames);
    assert_eq!(first_turn, 0);
    assert_eq!(prompts(&payload), ["after prompt 0", "after prompt 1"]);
    set.observer_more(id, session, 1)
        .expect("a page of the conversation it now holds");
    let (from, through, _, page) = page_of(&set.take_observer_frames(id, usize::MAX));
    assert_eq!((from, through), (0, 0));
    assert_eq!(prompts(&page), ["after prompt 0"]);
    shut_down(&mut set);
}

#[test]
fn a_resync_that_hid_a_clear_drops_the_owed_page() {
    let (dir, mut set, tab, provider) = tab_with("observers-more-resync", turns_of(5, 100));
    let id = set.observe(&key(&set, tab)).unwrap();
    set.take_observer_frames(id, usize::MAX);
    let session = generation(&set, tab);
    set.observer_more(id, session, 3).unwrap();
    // A `/clear` and more of the new conversation than the UI queue holds: the tick sees a `Resync`, never the reset.
    let turns = agent::UI_EVENT_QUEUE_CAPACITY / 4 + 10;
    let mut events = vec![AgentDomainEvent::ConversationReset {
        new_conversation_id: "conversation-after-clear".into(),
        trigger: Some("clear".into()),
    }];
    events.extend(turns_of(turns, 10).into_iter().map(|event| match event {
        AgentDomainEvent::UserPromptSubmitted { text } => AgentDomainEvent::UserPromptSubmitted {
            text: format!("after {text}"),
        },
        other => other,
    }));
    let through = revision(&set, tab) + events.len() as u64;
    provider.queue_all(events);
    folded(&set, tab, through);
    let out = set.pump(&dir, true);
    let payload = json(&out.active_payload.expect("a payload"));
    assert_eq!(payload["kind"], "snapshot", "the overflow became a resync");
    assert_eq!(generation(&set, tab), session, "the same session");
    let frames = set.take_observer_frames(id, usize::MAX);
    assert!(
        matches!(&frames[..], [ObserverFrame::Snapshot { .. }]),
        "the new conversation's snapshot, and no page counted in the old one: {frames:?}"
    );
    let (first_turn, _, _, payload) = snapshot_of(&frames);
    assert_eq!(first_turn, 0);
    assert_eq!(prompts(&payload).first().map(String::as_str), Some("after prompt 0"));
    set.observer_more(id, session, 3)
        .expect("a page of the conversation it now holds");
    let (from, through, _, page) = page_of(&set.take_observer_frames(id, usize::MAX));
    assert_eq!((from, through), (0, 2));
    assert_eq!(prompts(&page), ["after prompt 0", "after prompt 1", "after prompt 2"]);
    shut_down(&mut set);
}

#[test]
fn more_before_the_first_snapshot_is_refused() {
    let (_dir, mut set, tab, _provider) = tab_with("observers-more-first", turns_of(5, 100));
    let id = set.observe(&key(&set, tab)).unwrap();
    let generation = generation(&set, tab);
    assert_eq!(set.observer_more(id, generation, 2), Err(MoreRefusal::WorkChanged));
    let frames = set.take_observer_frames(id, usize::MAX);
    assert!(matches!(&frames[..], [ObserverFrame::Snapshot { .. }]), "{frames:?}");
    set.observer_more(id, generation, 2)
        .expect("a page once the snapshot is had");
    shut_down(&mut set);
}

#[test]
fn snapshot_stays_under_the_ceiling_once_escaped() {
    // Every character of these replies takes two bytes in JSON: their raw length fits, their envelope would not.
    for (label, fill, turns) in [("observers-escaped-one", "\n", 1), ("observers-escaped-last", "\"", 4)] {
        let reply = fill.repeat(600_000);
        let mut events = turns_of(turns - 1, 100);
        events.extend([
            AgentDomainEvent::TurnStarted {
                turn_id: "turn-escaped".into(),
            },
            AgentDomainEvent::UserPromptSubmitted {
                text: "prompt escaped".into(),
            },
            AgentDomainEvent::ContentDelta {
                turn_id: "turn-escaped".into(),
                kind: agent::ContentKind::Text,
                text: reply.clone(),
            },
            completed("turn-escaped"),
        ]);
        let (_dir, mut set, tab, _provider) = tab_with(label, events);
        let id = set.observe(&key(&set, tab)).unwrap();
        let (_, cut, bytes, payload) = snapshot_of(&set.take_observer_frames(id, usize::MAX));
        assert!(bytes <= SNAPSHOT_CEILING_BYTES, "{label}: {bytes} bytes");
        assert!(
            matches!(&cut[..], [CutEntry { entry_id, .. }] if entry_id.starts_with("transcript:")),
            "{label}: the reply is named cut: {cut:?}"
        );
        assert_eq!(prompts(&payload).last().map(String::as_str), Some("prompt escaped"));
        shut_down(&mut set);
    }
}

#[test]
fn cut_largest_cuts_the_largest_entries_until_enough_is_saved() {
    let mut events = turns_of(1, 50_000);
    events.extend(turns_of(1, 20_000));
    let (_dir, mut set, tab, _provider) = tab_with("observers-cut-largest", events);
    {
        let projection = set.get(tab).unwrap().live().unwrap().projection();
        let turns = Turns::of(&projection);
        let whole = tail::choose_tail(&projection, &turns, SNAPSHOT_CEILING_BYTES);
        assert!(whole.is_whole());
        let replies: Vec<u64> = projection.transcript.iter().map(|m| m.seq).collect();
        let cut_seqs = |tail: &tail::Tail| -> Vec<u64> {
            tail.cut_entries()
                .iter()
                .map(|c| c.entry_id.strip_prefix("transcript:").unwrap().parse().unwrap())
                .collect()
        };
        let one = tail::cut_largest(whole.clone(), &projection, 10_000).expect("the larger reply goes");
        assert_eq!(cut_seqs(&one), [replies[0]]);
        let both = tail::cut_largest(whole, &projection, 60_000).expect("both replies go");
        assert_eq!(cut_seqs(&both), replies);
        assert!(
            tail::cut_largest(both, &projection, 1).is_none(),
            "nothing large is left to cut"
        );
    }
    shut_down(&mut set);
}
