use super::tests::{background, end, filled, turn, Scratch};
use super::*;
use crate::history::{store, HistoryParts};
use crate::projection::*;
use serde_json::json;

fn with_tool(st: &mut IngestState, n: usize) {
    st.fold(AgentDomainEvent::UserPromptSubmitted {
        text: format!("prompt {n}"),
    });
    st.fold(AgentDomainEvent::TurnStarted {
        turn_id: format!("t{n}"),
    });
    st.fold(AgentDomainEvent::ToolCallStarted {
        turn_id: format!("t{n}"),
        tool_use_id: format!("tool-{n}"),
        name: "Read".into(),
        input: json!({"n":n}),
    });
    st.fold(AgentDomainEvent::ToolCallCompleted {
        turn_id: format!("t{n}"),
        tool_use_id: format!("tool-{n}"),
        content: json!(format!("result {n}")),
        is_error: false,
    });
    st.fold(AgentDomainEvent::ContentDelta {
        turn_id: format!("t{n}"),
        kind: ContentKind::Text,
        text: format!("reply {n}"),
    });
    st.fold(end(n));
}

fn run(budget: usize, fail: bool, move_at: Option<usize>, turns: usize) -> HistoryParts {
    let scratch = Scratch::new();
    let mut state = filled(0);
    let context = AdoptionContext {
        conversation_id: uuid::Uuid::new_v4().simple().to_string(),
        ..super::tests::context()
    };
    let slot = Mutex::new(Some("session".into()));
    state.lock().unwrap().record_written = true;
    for n in 0..turns {
        with_tool(&mut state.lock().unwrap(), n);
        flush_in(
            &state,
            &slot,
            &context,
            budget,
            if fail {
                Err(std::io::Error::other("archive unavailable"))
            } else {
                Ok(scratch.0.clone())
            },
            || {},
        );
        flush_history(&state, &slot, &context);
        let tail = state.lock().unwrap().history_tail.clone();
        let bounded = crate::history::StoredHistory::from_parts("check".into(), "check".into(), tail.clone());
        assert_eq!(
            bounded.into_parts(),
            tail,
            "tail always obeys the saved copy's own caps"
        );
        if move_at == Some(n) {
            let (core, handoff) = {
                let mut st = state.lock().unwrap();
                st.take_revised_delivery();
                st.take_evicted_calls_through(u64::MAX);
                let (core, mut handoff) = st.take_move_record(Some("session".into())).unwrap();
                handoff.history_tail =
                    serde_json::from_slice(&serde_json::to_vec(&handoff.history_tail).unwrap()).unwrap();
                (core, handoff)
            };
            let projection = AgentSessionProjection::adopt(&core).unwrap();
            let (destination, _) = IngestState::moved(MovedIngest {
                projection,
                core,
                state: handoff,
                lease: None,
            });
            state = Mutex::new(destination);
        }
    }
    let st = state.lock().unwrap();
    if budget == 10 {
        assert!(st.projection.evicted.is_some());
        assert!(Turns::of(&st.projection).count() < 20);
        assert_eq!(st.stats.lost_history_turns > 0, fail);
        if !fail {
            assert!(
                std::fs::read_dir(scratch.0.join(&context.conversation_id))
                    .unwrap()
                    .count()
                    > 0
            );
        }
    }
    canonical(store::load(&context.conversation_id, "session").unwrap().into_parts())
}

#[test]
fn saved_copy_is_independent_of_display_cap_archive_failure_and_move() {
    let expected = run(2000, false, None, 60);
    assert_eq!(expected.user_prompts.len(), 60);
    assert_eq!(expected.tool_calls.len(), 60);
    for (fail, move_at) in [(false, None), (true, None), (false, Some(31)), (true, Some(31))] {
        assert_eq!(run(10, fail, move_at, 60), expected, "fail={fail}, move={move_at:?}");
    }
}

#[test]
fn saved_copy_still_obeys_its_own_item_cap() {
    let expected = run(2000, false, None, 170);
    assert_eq!(
        expected.user_prompts.len() + expected.tool_calls.len() + expected.transcript.len(),
        400
    );
    assert_eq!(run(10, false, Some(91), 170), expected);
}

#[test]
fn a_full_live_saved_copy_leaves_no_eviction_tail() {
    for character_cap in [false, true] {
        let scratch = Scratch::new();
        let state = filled(221);
        let expected = {
            let mut st = state.lock().unwrap();
            if character_cap {
                st.projection.user_prompts.last_mut().unwrap().text = "x".repeat(crate::history::HISTORY_MAX_CHARS);
            }
            store::newest_that_fit(store::HistoryView::of(&st.projection))
        };
        assert!(flush_in(
            &state,
            &Mutex::new(Some("session".into())),
            &super::tests::context(),
            200,
            Ok(scratch.0.clone()),
            || {},
        ));
        let st = state.lock().unwrap();
        assert_eq!(history_snapshot(&st), expected);
        assert_eq!(
            st.history_tail,
            HistoryParts::default(),
            "character_cap={character_cap}"
        );
    }
}

#[test]
fn a_small_live_cap_keeps_only_the_selected_saved_tail() {
    let scratch = Scratch::new();
    let state = filled(221);
    let expected = store::newest_that_fit(store::HistoryView::of(&state.lock().unwrap().projection));
    assert!(super::tests::flush(&state, &scratch.0));
    let st = state.lock().unwrap();
    assert_eq!(history_snapshot(&st), expected);
    let mut selected_tail = expected;
    let through_seq = st.projection.evicted.as_ref().unwrap().through_seq;
    selected_tail.user_prompts.retain(|r| r.seq < through_seq);
    selected_tail.transcript.retain(|r| r.seq < through_seq);
    assert_eq!(selected_tail.user_prompts.len() + selected_tail.transcript.len(), 380);
    assert_eq!(st.history_tail, selected_tail);
}

#[test]
fn reset_flushes_the_old_copy_before_discarding_its_evicted_tail() {
    let scratch = Scratch::new();
    let state = filled(12);
    assert!(super::tests::flush(&state, &scratch.0));
    let mut st = state.lock().unwrap();
    st.record_written = true;
    let cleared = detach_for_reset(&mut st, &Mutex::new(Some("session".into()))).unwrap();
    assert_eq!(cleared.history.unwrap().user_prompts.len(), 12);
    st.fold(AgentDomainEvent::ConversationReset {
        new_conversation_id: "next".into(),
        trigger: None,
    });
    assert!(history_snapshot(&st).user_prompts.is_empty());
}

#[test]
fn expired_lifted_calls_remain_in_the_saved_copy() {
    let scratch = Scratch::new();
    let state = filled(12);
    background(&mut state.lock().unwrap());
    assert!(super::tests::flush(&state, &scratch.0));
    {
        let mut st = state.lock().unwrap();
        st.projection.tool_calls[0].background = false;
        st.projection.tool_calls[0].result =
            Some(serde_json::from_value(json!({"content":"done", "is_error":false})).unwrap());
        turn(&mut st, 12);
        turn(&mut st, 13);
    }
    assert!(super::tests::flush(&state, &scratch.0));
    let st = state.lock().unwrap();
    assert!(st.projection.tool_calls.is_empty());
    // The saved copy must still contain the finished lifted tree and the ordinary dead call.
    let parts = history_snapshot(&st);
    assert_eq!(
        parts
            .tool_calls
            .iter()
            .map(|c| c.tool_use_id.as_str())
            .collect::<Vec<_>>(),
        ["bg", "dead"]
    );
}

// Administrative folds allocate revisions too. Compare every stored value and the global row
// order independently of those gaps in sequence numbers.
fn canonical(mut parts: HistoryParts) -> HistoryParts {
    let mut seqs: Vec<_> = parts
        .user_prompts
        .iter()
        .map(|r| r.seq)
        .chain(parts.transcript.iter().map(|r| r.seq))
        .chain(parts.tool_calls.iter().map(|r| r.seq))
        .collect();
    seqs.sort_unstable();
    for r in &mut parts.user_prompts {
        r.seq = seqs.binary_search(&r.seq).unwrap() as u64;
    }
    for r in &mut parts.transcript {
        r.seq = seqs.binary_search(&r.seq).unwrap() as u64;
    }
    for r in &mut parts.tool_calls {
        r.seq = seqs.binary_search(&r.seq).unwrap() as u64;
    }
    parts
}

#[test]
fn eviction_preserves_all_seven_saved_kinds_without_thinking_or_endings() {
    let scratch = Scratch::new();
    let state = filled(12);
    let expected = {
        let mut st = state.lock().unwrap();
        st.projection
            .turn_starts
            .push(serde_json::from_value(json!({"seq":2,"turn_id":"cli","origin":"cli"})).unwrap());
        st.projection.notices.push(
            serde_json::from_value(json!({"seq":3,"kind":"informational","level":"info","content":"notice"})).unwrap(),
        );
        st.projection.questions.push(
            serde_json::from_value(
                json!({"seq":5,"interaction_id":"q","state":{"kind":"settled","outcome":"answered","answers":[]}}),
            )
            .unwrap(),
        );
        st.projection.plans.push(
            serde_json::from_value(
                json!({"seq":4,"interaction_id":"p","plan":"# Plan\n","state":{"kind":"settled","outcome":"approved"}}),
            )
            .unwrap(),
        );
        background(&mut st);
        crate::history::store::newest_that_fit(crate::history::store::HistoryView::of(&st.projection))
    };
    assert!(super::tests::flush(&state, &scratch.0));
    let st = state.lock().unwrap();
    assert_eq!(history_snapshot(&st), expected);
    assert_eq!(st.history_tail.turn_starts.len(), 1);
    assert_eq!(st.history_tail.notices.len(), 1);
    assert_eq!(st.history_tail.questions.len(), 1);
    assert_eq!(st.history_tail.plans.len(), 1);
    assert!(!serde_json::to_value(&st.history_tail)
        .unwrap()
        .as_object()
        .unwrap()
        .contains_key("thinking"));
    assert_eq!(
        serde_json::to_value(&st.history_tail)
            .unwrap()
            .as_object()
            .unwrap()
            .len(),
        7
    );
}

#[test]
fn saved_copy_character_cap_keeps_the_same_stopping_item_and_oversized_newest_row() {
    let scratch = Scratch::new();
    let state = filled(12);
    {
        let mut st = state.lock().unwrap();
        st.projection.user_prompts[2].text = "x".repeat(crate::history::HISTORY_MAX_CHARS);
    }
    let expected = crate::history::store::newest_that_fit(crate::history::store::HistoryView::of(
        &state.lock().unwrap().projection,
    ));
    assert!(super::tests::flush(&state, &scratch.0));
    {
        let st = state.lock().unwrap();
        assert_eq!(history_snapshot(&st), expected);
        assert_eq!(
            st.history_tail.user_prompts.len(),
            0,
            "older candidates cannot bypass the oversized live prompt or remain in the tail"
        );
    }
    let mut st = state.lock().unwrap();
    st.projection.transcript.last_mut().unwrap().text = "z".repeat(crate::history::HISTORY_MAX_CHARS + 1);
    let kept = history_snapshot(&st);
    assert!(kept.user_prompts.is_empty());
    assert_eq!(kept.transcript.len(), 1);
    assert_eq!(kept.transcript[0], *st.projection.transcript.last().unwrap());
    // A legacy seed can arrive out of sequence; keep the original selector's fallback too.
    st.projection.user_prompts.reverse();
    assert_eq!(history_snapshot(&st), kept);
}

#[test]
fn refused_cut_never_remembers_rows_that_are_still_live() {
    let scratch = Scratch::new();
    let state = filled(12);
    assert!(!flush_in(
        &state,
        &Mutex::new(Some("session".into())),
        &super::tests::context(),
        10,
        Ok(scratch.0.clone()),
        || {
            state.lock().unwrap().fold(AgentDomainEvent::UserPromptSubmitted {
                text: "crossing".into(),
            });
        }
    ));
    let st = state.lock().unwrap();
    assert_eq!(st.history_tail, HistoryParts::default());
    assert_eq!(
        history_snapshot(&st),
        store::newest_that_fit(store::HistoryView::of(&st.projection))
    );
}

#[test]
fn a_direct_reset_discards_the_saved_tail() {
    let scratch = Scratch::new();
    let state = filled(12);
    assert!(super::tests::flush(&state, &scratch.0));
    let mut st = state.lock().unwrap();
    assert!(!st.history_tail.user_prompts.is_empty());
    st.fold(AgentDomainEvent::ConversationReset {
        new_conversation_id: "next".into(),
        trigger: None,
    });
    assert_eq!(st.history_tail, HistoryParts::default());
    assert_eq!(history_snapshot(&st), HistoryParts::default());
}

#[test]
fn tail_character_storage_is_bounded_even_when_the_newest_removed_item_is_oversized() {
    let scratch = Scratch::new();
    let state = filled(12);
    let control = filled(12);
    for state in [&state, &control] {
        state.lock().unwrap().projection.transcript[1].text = "x".repeat(crate::history::HISTORY_MAX_CHARS + 1);
    }
    assert!(super::tests::flush(&state, &scratch.0));
    {
        let st = state.lock().unwrap();
        let tail = crate::history::StoredHistory::from_parts("check".into(), "check".into(), st.history_tail.clone());
        assert_eq!(tail.item_count(), 0);
        assert_eq!(tail.char_count(), 0);
        assert_eq!(
            history_snapshot(&st),
            store::newest_that_fit(store::HistoryView::of(&control.lock().unwrap().projection))
        );
    }
    for state in [&state, &control] {
        let mut st = state.lock().unwrap();
        turn(&mut st, 12);
        turn(&mut st, 13);
    }
    assert!(super::tests::flush(&state, &scratch.0));
    let st = state.lock().unwrap();
    let tail = crate::history::StoredHistory::from_parts("check".into(), "check".into(), st.history_tail.clone());
    assert!(tail.char_count() <= crate::history::HISTORY_MAX_CHARS);
    assert_eq!(tail.item_count(), 4);
    // Compare values and global order, allowing the revision gap introduced by eviction.
    assert_eq!(
        canonical(history_snapshot(&st)),
        canonical(store::newest_that_fit(store::HistoryView::of(
            &control.lock().unwrap().projection
        )))
    );
}
