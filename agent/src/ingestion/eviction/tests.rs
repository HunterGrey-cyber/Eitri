use super::*;
use crate::projection::*;
use serde_json::json;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

pub(super) struct Scratch(pub(super) std::path::PathBuf);
impl Scratch {
    pub(super) fn new() -> Self {
        let path = crate::state_dirs::test_workspace_dir("history-cap").join("archive");
        std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
    }
}
pub(super) fn context() -> AdoptionContext {
    AdoptionContext {
        provider_name: "claude",
        conversation_id: "conversation".into(),
        canonical_cwd: "/project".into(),
        provider_advertises_resume: true,
    }
}
pub(super) fn end(n: usize) -> AgentDomainEvent {
    AgentDomainEvent::TurnCompleted {
        turn_id: format!("t{n}"),
        outcome: TurnOutcome::Completed,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
        detail: Default::default(),
    }
}
pub(super) fn turn(st: &mut IngestState, n: usize) {
    st.fold(AgentDomainEvent::UserPromptSubmitted {
        text: format!("prompt {n}"),
    });
    st.fold(AgentDomainEvent::TurnStarted {
        turn_id: format!("t{n}"),
    });
    st.fold(AgentDomainEvent::ContentDelta {
        turn_id: format!("t{n}"),
        kind: ContentKind::Text,
        text: format!("reply {n}"),
    });
    st.fold(end(n));
}
pub(super) fn filled(n: usize) -> Mutex<IngestState> {
    let mut st = crate::ingestion::tests::state();
    for i in 0..n {
        turn(&mut st, i);
    }
    Mutex::new(st)
}
pub(super) fn flush(state: &Mutex<IngestState>, dir: &Path) -> bool {
    flush_in(
        state,
        &Mutex::new(Some("session".into())),
        &context(),
        10,
        Ok(dir.to_owned()),
        || {},
    )
}
fn page(dir: &Path, id: &ArchiveId, before: u64) -> crate::history::archive::Page {
    crate::history::archive::read_page(dir, id, before, 100, usize::MAX, &AtomicBool::new(false))
        .unwrap()
        .unwrap()
}
pub(super) fn background(st: &mut IngestState) {
    st.projection.tool_calls.push(serde_json::from_value(json!({
        "seq": 1, "turn_id":"t0", "tool_use_id":"bg", "name":"Agent", "input":{}, "result":null,"background":true,
        "children":{"transcript":[{"seq":1,"text":"child reply"}], "thinking":[],
            "tool_calls":[{"seq":2,"turn_id":"child","tool_use_id":"nested","name":"Read","input":{},"result":null}],
            "assistant_message_open":false,"thinking_open":false}
    })).unwrap());
    st.projection.tool_calls.push(
        serde_json::from_value(json!({
            "seq":3,"turn_id":"t0","tool_use_id":"dead","name":"Read","input":{},"result":null
        }))
        .unwrap(),
    );
}

#[test]
fn settled_history_evicts_only_durable_unpinned_turns() {
    let scratch = Scratch::new();
    let state = filled(30);
    background(&mut state.lock().unwrap());
    assert!(flush(&state, &scratch.0));
    let id = {
        let st = state.lock().unwrap();
        assert_eq!(Turns::of(&st.projection).count(), 10);
        assert_eq!(
            st.projection.evicted,
            Some(Evicted {
                through_seq: 80,
                turns: 20,
                lifted: vec![1]
            })
        );
        assert_eq!(st.projection.tool_calls.len(), 1);
        assert_eq!(
            st.projection.tool_calls[0].children.as_ref().unwrap().tool_calls.len(),
            1
        );
        assert!(!st.eviction_due);
        assert!(st.running_turn.is_none());
        st.archive_id.clone().unwrap()
    };
    let archived = page(&scratch.0, &id, 20);
    assert_eq!(
        archived.turns.iter().map(|t| t.turn).collect::<Vec<_>>(),
        (0..20).collect::<Vec<_>>()
    );
    assert_eq!(archived.turns[0].records.len(), 3); // prompt, reply and the dead foreground call
    assert!(archived
        .turns
        .iter()
        .flat_map(|t| &t.records)
        .all(|r| !matches!(r, ArchivedRecord::ToolCall(c) if c.tool_use_id == "bg")));
    {
        let mut st = state.lock().unwrap();
        turn(&mut st, 30);
        turn(&mut st, 31);
    }
    assert!(flush(&state, &scratch.0));
    assert_eq!(
        state.lock().unwrap().projection.evicted.as_ref().unwrap().lifted,
        vec![1]
    );
    {
        let mut st = state.lock().unwrap();
        st.fold(AgentDomainEvent::ToolCallCompleted {
            turn_id: "t0".into(),
            tool_use_id: "bg".into(),
            content: json!("done"),
            is_error: false,
        });
        turn(&mut st, 32);
        turn(&mut st, 33);
    }
    assert!(flush(&state, &scratch.0));
    let st = state.lock().unwrap();
    assert_eq!(st.projection.evicted.as_ref().unwrap().turns, 24);
    assert!(st.projection.evicted.as_ref().unwrap().lifted.is_empty());
    assert!(st.projection.tool_calls.is_empty());
    assert_eq!(page(&scratch.0, &id, 24).turns.len(), 24);
}

#[test]
fn questions_client_calls_reload_and_opening_prompts_defer_eviction() {
    let scratch = Scratch::new();
    for blocker in 0..6 {
        let state = filled(12);
        {
            let mut st = state.lock().unwrap();
            match blocker {
                0 | 1 => st.projection.questions.push(serde_json::from_value(json!({"seq":3,"interaction_id":"q","state":{"kind":if blocker == 0 {"waiting"} else {"answering"}}})).unwrap()),
                2 => st.fold(AgentDomainEvent::ClientToolCall { call_id:"c".into(), tool_use_id:None,tool:"edit".into(),arguments_json:"{}".into(),sequence:1 }),
                3 => st.reload_held = true,
                4 => st.projection.opening_prompt = Some(48),
                5 => st.fold(AgentDomainEvent::UserPromptSubmitted { text:"next".into() }),
                _ => unreachable!(),
            }
        }
        assert!(!flush(&state, &scratch.0), "blocker {blocker}");
        let st = state.lock().unwrap();
        assert!(st.projection.evicted.is_none());
        assert!(st.eviction_due);
        if blocker == 5 {
            assert!(st.running_turn.is_some());
        }
        drop(st);
        {
            let mut st = state.lock().unwrap();
            st.projection.questions.clear();
            st.projection.open_client_calls.clear();
            st.reload_held = false;
            st.projection.opening_prompt = None;
            if blocker == 5 {
                st.fold(AgentDomainEvent::TurnStarted { turn_id: "t12".into() });
                st.fold(end(12));
            }
        }
        assert!(flush(&state, &scratch.0), "cleared blocker {blocker}");
    }
}

#[test]
fn reload_defers_below_ceiling_and_stays_reserved_until_an_eligible_fold() {
    let state = filled(19);
    let mut st = state.lock().unwrap();
    st.reload_held = true;
    let generation = st.reload_generation;
    assert!(!ready(&mut st, 10).0);
    assert!(st.reload_held);
    assert_eq!(st.reload_generation, generation);
    turn(&mut st, 19);
    st.projection.opening_prompt = Some(80);
    assert!(!ready(&mut st, 10).0);
    assert!(st.reload_held);
    assert_eq!(st.reload_generation, generation);
    assert!(st.eviction_ceiling_logged);
    st.projection.opening_prompt = None;
    assert!(ready(&mut st, 10).0);
}

#[test]
fn ceiling_blocker_warning_rearms_after_a_successful_cut() {
    let scratch = Scratch::new();
    let state = filled(20);
    {
        let mut st = state.lock().unwrap();
        st.projection.opening_prompt = Some(80);
        let (eligible, warning) = ready(&mut st, 10);
        assert!(!eligible);
        assert!(warning.unwrap().ends_with("eviction deferred by opening prompt"));
        assert!(st.eviction_ceiling_logged);
        assert_eq!(ready(&mut st, 10), (false, None));
        assert!(st.eviction_ceiling_logged);
        st.projection.opening_prompt = None;
    }
    assert!(flush(&state, &scratch.0));
    let mut st = state.lock().unwrap();
    assert!(!st.eviction_ceiling_logged);
    for n in 20..30 {
        turn(&mut st, n);
    }
    st.projection.opening_prompt = Some(120);
    let (eligible, warning) = ready(&mut st, 10);
    assert!(!eligible);
    assert!(warning.is_some());
    assert!(st.eviction_ceiling_logged);
}

#[test]
fn reset_fold_releases_reload_and_invalidates_its_generation() {
    let state = filled(12);
    let mut st = state.lock().unwrap();
    st.reload_held = true;
    let generation = st.reload_generation;
    st.fold(AgentDomainEvent::ConversationReset {
        new_conversation_id: "next".into(),
        trigger: None,
    });
    assert!(!st.reload_held);
    assert!(st.reload_generation > generation);
}

#[test]
fn prompt_between_archive_copy_and_fold_keeps_running_turn_and_retries_idempotently() {
    let scratch = Scratch::new();
    let state = filled(12);
    let slot = Mutex::new(Some("session".into()));
    assert!(!flush_in(&state, &slot, &context(), 10, Ok(scratch.0.clone()), || {
        // This hook runs with the state lock released, at the same seam a local prompt can use.
        state
            .try_lock()
            .expect("archive write released the state lock")
            .fold(AgentDomainEvent::UserPromptSubmitted {
                text: "crossing".into(),
            });
    }));
    {
        let mut st = state.lock().unwrap();
        assert!(st.projection.evicted.is_none());
        assert!(st.evicted_calls.is_empty(), "an abandoned cut queues no calls");
        assert!(st.running_turn.is_some());
        st.fold(AgentDomainEvent::TurnStarted { turn_id: "t12".into() });
        st.fold(end(12));
    }
    assert!(flush(&state, &scratch.0));
    let st = state.lock().unwrap();
    let archived = page(&scratch.0, st.archive_id.as_ref().unwrap(), 3);
    assert_eq!(archived.turns.len(), 3);
    assert_eq!(archived.turns[0].records.len(), 2);
    assert_eq!(st.projection.evicted.as_ref().unwrap().turns, 3);
}

#[test]
fn reload_requested_after_copy_also_prevents_the_fold() {
    let scratch = Scratch::new();
    let state = filled(12);
    assert!(!flush_in(
        &state,
        &Mutex::new(Some("session".into())),
        &context(),
        10,
        Ok(scratch.0.clone()),
        || {
            state
                .try_lock()
                .expect("archive write released the state lock")
                .reload_held = true;
        }
    ));
    assert!(state.lock().unwrap().projection.evicted.is_none());
    assert!(state.lock().unwrap().reload_held);
    {
        let mut st = state.lock().unwrap();
        st.reload_held = false;
        turn(&mut st, 12);
    }
    assert!(flush(&state, &scratch.0));
}

#[test]
fn failed_archive_waits_for_next_turn_end_and_at_twice_budget_counts_loss() {
    let scratch = Scratch::new();
    let readonly = scratch.0.join("readonly");
    std::fs::DirBuilder::new().mode(0o000).create(&readonly).unwrap();
    let state = filled(12);
    background(&mut state.lock().unwrap());
    assert!(!flush(&state, &readonly));
    {
        let st = state.lock().unwrap();
        assert_eq!(Turns::of(&st.projection).count(), 12);
        assert!(st.eviction_due);
        assert!(st.eviction_retry);
        assert!(st.evicted_calls.is_empty(), "a deferred cut queues no calls");
    }
    // Even a repaired destination is not retried on every quiet pump.
    assert!(!flush(&state, &scratch.0));
    for i in 12..20 {
        turn(&mut state.lock().unwrap(), i);
        let evicted = flush(&state, &readonly);
        assert_eq!(evicted, i == 19);
    }
    let st = state.lock().unwrap();
    assert_eq!(Turns::of(&st.projection).count(), 10);
    assert_eq!(st.stats.lost_history_turns, 10);
    assert_eq!(st.projection.evicted.as_ref().unwrap().turns, 10);
    assert_eq!(
        st.evicted_calls[0].1[0].tool_use_id, "dead",
        "a hard-ceiling cut retains attribution even without an archive"
    );
    drop(st);
    std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn moved_tail_keeps_boundary_lifted_children_and_archive_identity() {
    let scratch = Scratch::new();
    let source = filled(12);
    background(&mut source.lock().unwrap());
    assert!(flush(&source, &scratch.0));
    let (core, handoff) = {
        let mut st = source.lock().unwrap();
        st.take_revised_delivery();
        assert!(matches!(
            st.take_move_record(Some("session".into())),
            Err(HandoffRefused::Undelivered)
        ));
        assert_eq!(st.take_evicted_calls_through(u64::MAX).len(), 1);
        st.take_move_record(Some("session".into())).unwrap()
    };
    let id = handoff.archive_id.clone().unwrap();
    // Both identities survive the serialized move payload too.
    let id = serde_json::from_str::<ArchiveId>(&serde_json::to_string(&id).unwrap()).unwrap();
    let projection = AgentSessionProjection::adopt(&core).unwrap();
    assert_eq!(projection.evicted, source.lock().unwrap().projection.evicted);
    assert_eq!(projection.tool_calls, source.lock().unwrap().projection.tool_calls);
    let (mut destination, _) = IngestState::moved(MovedIngest {
        projection,
        core,
        state: handoff,
        lease: None,
    });
    turn(&mut destination, 12);
    turn(&mut destination, 13);
    let destination = Mutex::new(destination);
    assert!(flush(&destination, &scratch.0));
    assert_eq!(destination.lock().unwrap().archive_id.as_ref(), Some(&id));
    assert_eq!(page(&scratch.0, &id, 4).turns.len(), 4);
    assert_eq!(std::fs::read_dir(scratch.0.join("conversation")).unwrap().count(), 1);
}

#[test]
fn settled_record_counts_plateau_after_three_budgets() {
    let scratch = Scratch::new();
    let state = filled(0);
    for i in 0..30 {
        turn(&mut state.lock().unwrap(), i);
        flush(&state, &scratch.0);
        let st = state.lock().unwrap();
        assert!(st.projection.user_prompts.len() <= 11);
        assert!(st.projection.transcript.len() <= 11);
        assert!(Turns::of(&st.projection).count() <= 11);
    }
    let st = state.lock().unwrap();
    assert_eq!(st.projection.evicted.as_ref().unwrap().turns, 20);
    assert_eq!(Turns::of(&st.projection).count(), 10);
    assert_eq!(st.projection.user_prompts[0].text, "prompt 20");
}

#[test]
fn running_roster_lifts_its_launcher_even_after_a_launch_result() {
    let scratch = Scratch::new();
    let state = filled(12);
    {
        let mut st = state.lock().unwrap();
        st.projection.tool_calls.push(
            serde_json::from_value(json!({
                "seq":1,"turn_id":"t0","tool_use_id":"launcher","name":"Bash","input":{},
                "result":{"content":"running in background","is_error":false}
            }))
            .unwrap(),
        );
        st.projection.apply(&serde_json::from_value(json!({
            "type":"background_tasks_changed","tasks":[{"task_id":"task","run_id":null,"kind":"shell",
            "task_type":"shell","description":"work","subagent_type":null,"parent_task_id":null,"tool_use_id":"launcher",
            "ambient":false,"workflow_name":null}]
        })).unwrap());
    }
    assert!(flush(&state, &scratch.0));
    let st = state.lock().unwrap();
    assert_eq!(st.projection.evicted.as_ref().unwrap().lifted, vec![1]);
    assert_eq!(st.projection.tool_calls[0].tool_use_id, "launcher");
}

#[test]
fn nested_running_roster_lifts_completed_ancestors_and_expires_after_completion() {
    let scratch = Scratch::new();
    let state = filled(12);
    {
        let mut st = state.lock().unwrap();
        background(&mut st);
        let root = &mut st.projection.tool_calls[0];
        root.background = false;
        root.result = Some(serde_json::from_value(json!({"content":"done", "is_error":false})).unwrap());
        let nested = &mut root.children.as_mut().unwrap().tool_calls[0];
        nested.result = root.result.clone();
        nested.children = Some(
            serde_json::from_value(json!({
                "transcript":[],"thinking":[],"tool_calls":[{
                    "seq":3,"turn_id":"grandchild","tool_use_id":"launcher","name":"Bash","input":{},
                    "result":{"content":"launched","is_error":false}
                }],"assistant_message_open":false,"thinking_open":false
            }))
            .unwrap(),
        );
        st.fold(serde_json::from_value(json!({"type":"background_tasks_changed","tasks":[{
            "task_id":"task","kind":"shell","task_type":"shell","description":"work","ambient":false,"tool_use_id":"launcher"
        }]})).unwrap());
    }
    assert!(flush(&state, &scratch.0));
    {
        let mut st = state.lock().unwrap();
        assert_eq!(st.projection.evicted.as_ref().unwrap().lifted, vec![1]);
        assert_eq!(
            st.projection.tool_calls[0].children.as_ref().unwrap().tool_calls[0]
                .children
                .as_ref()
                .unwrap()
                .tool_calls[0]
                .tool_use_id,
            "launcher"
        );
        st.fold(
            serde_json::from_value(json!({"type":"task_event","edge":"updated","task_id":"task",
            "run_id":null,"task":null,"status":"completed","summary":"done"}))
            .unwrap(),
        );
        turn(&mut st, 12);
        turn(&mut st, 13);
    }
    assert!(flush(&state, &scratch.0));
    let st = state.lock().unwrap();
    assert!(st.projection.tool_calls.is_empty());
    assert!(st.projection.evicted.as_ref().unwrap().lifted.is_empty());
}

#[test]
fn synthetic_background_launcher_expires_when_its_roster_entry_completes() {
    let scratch = Scratch::new();
    let state = filled(0);
    {
        let mut st = state.lock().unwrap();
        st.fold(AgentDomainEvent::SubagentFrame {
            parent_tool_use_id: "unknown".into(),
            frame: Box::new(AgentDomainEvent::ContentDelta {
                turn_id: "child".into(),
                kind: ContentKind::Text,
                text: "work".into(),
            }),
        });
        st.fold(serde_json::from_value(json!({"type":"background_tasks_changed","tasks":[{
            "task_id":"task","kind":"local_agent","task_type":"agent","description":"work","ambient":false,"tool_use_id":"unknown"
        }]})).unwrap());
        for n in 0..12 {
            turn(&mut st, n);
        }
    }
    assert!(flush(&state, &scratch.0));
    let id = {
        let mut st = state.lock().unwrap();
        assert_eq!(st.projection.evicted.as_ref().unwrap().lifted, vec![0]);
        assert!(st.projection.tool_calls[0].background);
        assert!(st.projection.tool_calls[0].result.is_none());
        st.fold(
            serde_json::from_value(json!({"type":"task_event","edge":"updated","task_id":"task",
            "run_id":null,"task":null,"status":"completed","summary":"done"}))
            .unwrap(),
        );
        turn(&mut st, 12);
        turn(&mut st, 13);
        st.archive_id.clone().unwrap()
    };
    assert!(flush(&state, &scratch.0));
    let st = state.lock().unwrap();
    assert!(st.projection.tool_calls.is_empty());
    assert!(st.projection.evicted.as_ref().unwrap().lifted.is_empty());
    assert!(page(&scratch.0, &id, 4)
        .turns
        .iter()
        .flat_map(|t| &t.records)
        .all(|r| !matches!(r, ArchivedRecord::ToolCall(c) if c.tool_use_id == "unknown")));
}

#[test]
fn evicted_calls_keep_children_exclude_lifted_and_drain_once() {
    let scratch = Scratch::new();
    let state = filled(12);
    {
        let mut st = state.lock().unwrap();
        background(&mut st);
        let children = st.projection.tool_calls[0].children.clone();
        st.projection.tool_calls[1].children = children;
    }
    assert!(flush(&state, &scratch.0));
    let mut st = state.lock().unwrap();
    let calls = st.take_evicted_calls_through(u64::MAX);
    assert_eq!(
        calls.iter().map(|c| c.tool_use_id.as_str()).collect::<Vec<_>>(),
        ["dead"]
    );
    assert_eq!(calls[0].children.as_ref().unwrap().tool_calls[0].tool_use_id, "nested");
    assert!(st.take_evicted_calls_through(u64::MAX).is_empty());
    assert_eq!(st.projection.tool_calls[0].tool_use_id, "bg");
}

#[test]
fn evicted_calls_keep_chunks_until_their_revision_is_delivered() {
    let scratch = Scratch::new();
    let state = filled(12);
    background(&mut state.lock().unwrap());
    assert!(flush(&state, &scratch.0));
    {
        let mut st = state.lock().unwrap();
        assert_eq!(st.evicted_calls[0].1[0].tool_use_id, "dead");
        let mut newer = st.evicted_calls[0].1[0].clone();
        newer.seq = st.projection.user_prompts[0].seq;
        newer.tool_use_id = "newer".into();
        st.projection.tool_calls.push(newer);
        turn(&mut st, 12);
        turn(&mut st, 13);
    }
    assert!(flush(&state, &scratch.0));
    let mut st = state.lock().unwrap();
    let first = st.evicted_calls[0].0;
    let second = st.evicted_calls[1].0;
    let cuts: Vec<_> = st
        .pending_ui
        .iter()
        .filter_map(|(revision, event)| matches!(event, AgentDomainEvent::HistoryEvicted { .. }).then_some(*revision))
        .collect();
    assert_eq!(
        cuts,
        [first, second],
        "each chunk carries its own eviction fold revision"
    );
    assert!(first < second);
    assert!(st.take_evicted_calls_through(first - 1).is_empty());
    assert_eq!(
        st.take_evicted_calls_through(first)
            .iter()
            .map(|c| c.tool_use_id.as_str())
            .collect::<Vec<_>>(),
        ["dead"]
    );
    assert!(st.take_evicted_calls_through(second - 1).is_empty());
    assert_eq!(
        st.take_evicted_calls_through(second)
            .iter()
            .map(|c| c.tool_use_id.as_str())
            .collect::<Vec<_>>(),
        ["newer"]
    );
    assert!(st.take_evicted_calls_through(u64::MAX).is_empty());
}

#[test]
fn reset_discards_undelivered_evicted_calls() {
    let scratch = Scratch::new();
    let state = filled(12);
    background(&mut state.lock().unwrap());
    assert!(flush(&state, &scratch.0));
    let mut st = state.lock().unwrap();
    assert!(!st.evicted_calls.is_empty());
    detach_for_reset(&mut st, &Mutex::new(Some("session".into())));
    assert!(st.take_evicted_calls_through(u64::MAX).is_empty());
}

#[test]
fn expired_lifted_calls_are_captured_without_archiving_them() {
    let scratch = Scratch::new();
    let state = filled(12);
    background(&mut state.lock().unwrap());
    assert!(flush(&state, &scratch.0));
    let id = {
        let mut st = state.lock().unwrap();
        st.take_evicted_calls_through(u64::MAX);
        st.projection.tool_calls[0].background = false;
        st.projection.tool_calls[0].result =
            Some(serde_json::from_value(json!({"content":"ok", "is_error":false})).unwrap());
        turn(&mut st, 12);
        turn(&mut st, 13);
        st.archive_id.clone().unwrap()
    };
    assert!(flush(&state, &scratch.0));
    let mut st = state.lock().unwrap();
    assert!(st.projection.tool_calls.is_empty());
    let calls = st.take_evicted_calls_through(u64::MAX);
    assert_eq!(calls.iter().map(|c| c.tool_use_id.as_str()).collect::<Vec<_>>(), ["bg"]);
    assert_eq!(calls[0].children.as_ref().unwrap().tool_calls[0].tool_use_id, "nested");
    assert!(page(&scratch.0, &id, 4)
        .turns
        .iter()
        .flat_map(|t| &t.records)
        .all(|r| !matches!(r, ArchivedRecord::ToolCall(c) if c.tool_use_id == "bg")));
}

#[test]
fn evicted_call_queue_keeps_the_newest_chunks_at_its_bound() {
    let state = filled(0);
    let mut st = state.lock().unwrap();
    background(&mut st);
    let mut call = st.projection.tool_calls[1].clone();
    st.queue_evicted_calls(Vec::new());
    assert!(st.evicted_calls.is_empty());
    for i in 0..EVICTED_CALL_CHUNKS + 2 {
        st.fold(AgentDomainEvent::EffortReported { effort: None });
        call.tool_use_id = format!("call-{i}");
        st.queue_evicted_calls(vec![call.clone()]);
        assert!(st.evicted_calls.len() <= EVICTED_CALL_CHUNKS);
    }
    let through = st.projection.last_revision;
    assert_eq!(
        st.take_evicted_calls_through(through)
            .iter()
            .map(|c| c.tool_use_id.as_str())
            .collect::<Vec<_>>(),
        (2..EVICTED_CALL_CHUNKS + 2)
            .map(|i| format!("call-{i}"))
            .collect::<Vec<_>>()
    );
    assert!(st.evicted_calls.is_empty());
}

#[test]
fn background_turns_ignore_fifty_turns_without_calls() {
    for with_call in [false, true] {
        let mut st = crate::ingestion::tests::state();
        if with_call {
            background(&mut st);
        }
        st.fold(AgentDomainEvent::BackgroundCalls { running: 1 });
        for n in 0..50 {
            turn(&mut st, n);
            let expected = if with_call {
                BTreeSet::from(["t0".into()])
            } else {
                BTreeSet::new()
            };
            assert_eq!(st.background_turns, expected, "after turn {n}");
        }
    }
}

#[test]
fn background_turns_require_an_unfinished_non_denied_call_of_the_completed_turn() {
    for nested in [false, true] {
        for outcome in ["unfinished", "finished", "denied"] {
            let mut st = crate::ingestion::tests::state();
            background(&mut st);
            st.projection.tool_calls.truncate(1);
            let root = &mut st.projection.tool_calls[0];
            let call = if nested {
                root.turn_id = "ancestor".into();
                &mut root.children.as_mut().unwrap().tool_calls[0]
            } else {
                root
            };
            call.turn_id = "t0".into();
            match outcome {
                "finished" => {
                    call.result = Some(ToolCallResult {
                        content: json!("done"),
                        is_error: false,
                    })
                }
                "denied" => {
                    call.denied = Some(ToolCallDenial {
                        reason_type: None,
                        reason: None,
                    })
                }
                _ => {}
            }
            st.fold(AgentDomainEvent::BackgroundCalls { running: 1 });
            turn(&mut st, 0);
            assert_eq!(
                st.background_turns.contains("t0"),
                outcome == "unfinished",
                "nested={nested}, outcome={outcome}"
            );
        }
    }
}

#[test]
fn eviction_bounds_background_turns_to_live_and_lifted_calls() {
    let scratch = Scratch::new();
    let state = filled(0);
    {
        let mut st = state.lock().unwrap();
        st.fold(AgentDomainEvent::BackgroundCalls { running: 1 });
        background(&mut st);
        let root = &mut st.projection.tool_calls[0];
        root.turn_id = "ancestor".into();
        root.background = false;
        root.result = Some(ToolCallResult {
            content: json!("launched"),
            is_error: false,
        });
        root.children.as_mut().unwrap().tool_calls[0].turn_id = "t0".into();
        st.projection.tool_calls[1].turn_id = "t1".into();
        for n in 0..14 {
            turn(&mut st, n);
        }
        assert_eq!(st.background_turns, BTreeSet::from(["t0".into(), "t1".into()]));
        // The old call finishes while other background work keeps the count nonzero.
        st.projection.tool_calls[1].result = Some(ToolCallResult {
            content: json!("done"),
            is_error: false,
        });
    }
    let slot = Mutex::new(Some("session".into()));
    let context = AdoptionContext {
        provider_name: "codex",
        ..context()
    };
    assert!(flush_in(&state, &slot, &context, 10, Ok(scratch.0.clone()), || {}));
    {
        let mut st = state.lock().unwrap();
        assert_eq!(st.background_turns, BTreeSet::from(["t0".into()]));
        assert_eq!(st.projection.tool_calls.len(), 1);
        assert_eq!(st.projection.evicted.as_ref().unwrap().lifted.len(), 1);
        st.projection.tool_calls[0].children.as_mut().unwrap().tool_calls[0].result = Some(ToolCallResult {
            content: json!("done"),
            is_error: false,
        });
        // Keep an unfinished call in the live tail while the lifted child expires.
        st.fold(AgentDomainEvent::UserPromptSubmitted { text: "live".into() });
        st.fold(AgentDomainEvent::TurnStarted { turn_id: "t14".into() });
        st.fold(AgentDomainEvent::ToolCallStarted {
            turn_id: "t14".into(),
            tool_use_id: "live".into(),
            name: "Read".into(),
            input: json!({}),
        });
        st.fold(end(14));
        turn(&mut st, 15);
    }
    assert!(flush_in(&state, &slot, &context, 10, Ok(scratch.0.clone()), || {}));
    {
        let mut st = state.lock().unwrap();
        assert_eq!(st.background_turns, BTreeSet::from(["t14".into()]));
        assert_eq!(st.projection.tool_calls.len(), 1);
        assert!(st.projection.evicted.as_ref().unwrap().lifted.is_empty());
        st.projection.tool_calls[0].result = Some(ToolCallResult {
            content: json!("done"),
            is_error: false,
        });
        for n in 16..18 {
            turn(&mut st, n);
        }
    }
    assert!(flush_in(&state, &slot, &context, 10, Ok(scratch.0.clone()), || {}));
    {
        let mut st = state.lock().unwrap();
        assert_eq!(st.background_turns, BTreeSet::from(["t14".into()]));
        assert_eq!(st.projection.tool_calls.len(), 1);
        for n in 18..30 {
            turn(&mut st, n);
        }
    }
    assert!(flush_in(&state, &slot, &context, 10, Ok(scratch.0.clone()), || {}));
    let st = state.lock().unwrap();
    assert!(st.projection.tool_calls.is_empty());
    assert!(st.background_turns.is_empty());
    assert_eq!(st.projection.background_calls, 1);
}

#[test]
fn codex_background_count_keeps_unfinished_calls_until_their_results_and_expiry() {
    for running in [0, 1] {
        let scratch = Scratch::new();
        let state = filled(0);
        {
            let mut st = state.lock().unwrap();
            st.fold(AgentDomainEvent::UserPromptSubmitted { text: "first".into() });
            st.fold(AgentDomainEvent::TurnStarted { turn_id: "t0".into() });
            st.fold(AgentDomainEvent::ToolCallStarted {
                turn_id: "t0".into(),
                tool_use_id: "long".into(),
                name: "Write".into(),
                input: json!({"file_path":"/project/long.rs"}),
            });
            st.fold(AgentDomainEvent::BackgroundCalls { running });
            st.fold(end(0));
            for n in 1..14 {
                turn(&mut st, n);
            }
        }
        assert!(flush_in(
            &state,
            &Mutex::new(Some("session".into())),
            &AdoptionContext {
                provider_name: if running == 0 { "claude" } else { "codex" },
                ..context()
            },
            10,
            Ok(scratch.0.clone()),
            || {}
        ));
        if running == 0 {
            assert!(
                state.lock().unwrap().projection.tool_calls.is_empty(),
                "Claude has no count and keeps its dead-call rule"
            );
            continue;
        }
        let id = {
            let mut st = state.lock().unwrap();
            assert_eq!(st.projection.tool_calls.len(), 1);
            assert_eq!(st.projection.evicted.as_ref().unwrap().lifted.len(), 1);
            assert!(!st.projection.tool_calls[0].background);
            assert!(st.background_turns.contains("t0"));
            st.take_revised_delivery();
            st.take_evicted_calls_through(u64::MAX);
            let (core, handoff) = st.take_move_record(Some("session".into())).unwrap();
            assert_eq!(handoff.background_turns, st.background_turns);
            let (mut moved, _) = IngestState::moved(MovedIngest {
                projection: AgentSessionProjection::adopt(&core).unwrap(),
                core,
                state: handoff,
                lease: None,
            });
            assert_eq!(moved.background_turns, st.background_turns);
            moved.fold(AgentDomainEvent::ConversationReset {
                new_conversation_id: "new".into(),
                trigger: None,
            });
            assert!(moved.background_turns.is_empty());
            st.fold(AgentDomainEvent::ToolCallCompleted {
                turn_id: "t0".into(),
                tool_use_id: "long".into(),
                content: json!("saved"),
                is_error: false,
            });
            assert!(st.projection.tool_calls[0].result.is_some());
            st.fold(AgentDomainEvent::BackgroundCalls { running: 0 });
            assert!(st.background_turns.is_empty());
            for n in 14..16 {
                turn(&mut st, n);
            }
            st.archive_id.clone().unwrap()
        };
        assert!(flush_in(
            &state,
            &Mutex::new(Some("session".into())),
            &AdoptionContext {
                provider_name: if running == 0 { "claude" } else { "codex" },
                ..context()
            },
            10,
            Ok(scratch.0.clone()),
            || {}
        ));
        let mut st = state.lock().unwrap();
        assert!(st.projection.tool_calls.is_empty());
        assert!(st
            .history_tail
            .tool_calls
            .iter()
            .any(|c| c.tool_use_id == "long" && c.result.is_some()));
        assert!(st
            .take_evicted_calls_through(u64::MAX)
            .iter()
            .any(|c| c.tool_use_id == "long" && c.result.is_some()));
        assert!(page(&scratch.0, &id, 6)
            .turns
            .iter()
            .flat_map(|t| &t.records)
            .all(|r| !matches!(r, ArchivedRecord::ToolCall(c) if c.tool_use_id == "long")));
    }
}

#[test]
fn codex_background_lift_excludes_unrelated_ended_turns() {
    let scratch = Scratch::new();
    let state = filled(0);
    {
        let mut st = state.lock().unwrap();
        for n in 0..2 {
            st.fold(AgentDomainEvent::UserPromptSubmitted {
                text: format!("prompt {n}"),
            });
            st.fold(AgentDomainEvent::TurnStarted {
                turn_id: format!("t{n}"),
            });
            st.fold(AgentDomainEvent::ToolCallStarted {
                turn_id: format!("t{n}"),
                tool_use_id: format!("call-{n}"),
                name: "Read".into(),
                input: json!({}),
            });
            st.fold(AgentDomainEvent::BackgroundCalls { running: n });
            st.fold(end(n as usize));
        }
        for n in 2..14 {
            turn(&mut st, n);
        }
    }
    assert!(flush_in(
        &state,
        &Mutex::new(Some("session".into())),
        &AdoptionContext {
            provider_name: "codex",
            ..context()
        },
        10,
        Ok(scratch.0.clone()),
        || {}
    ));
    let st = state.lock().unwrap();
    assert_eq!(
        st.projection
            .tool_calls
            .iter()
            .map(|c| c.tool_use_id.as_str())
            .collect::<Vec<_>>(),
        vec!["call-1"]
    );
    assert!(page(&scratch.0, st.archive_id.as_ref().unwrap(), 4)
        .turns
        .iter()
        .flat_map(|t| &t.records)
        .any(|r| matches!(r, ArchivedRecord::ToolCall(c) if c.tool_use_id == "call-0")));
}

#[test]
fn ceiling_keeps_reload_valid_during_write_and_on_abandoned_cut() {
    for abandon in [false, true] {
        let scratch = Scratch::new();
        let ingest = ConversationIngest::start(
            Arc::new(super::super::idle_wakeups::Quiet::default()),
            "claude",
            "conversation".into(),
            "/project".into(),
            false,
            None,
            None,
            AgentSessionProjection::default(),
        );
        *ingest.state.lock().unwrap() = filled(12).into_inner().unwrap();
        let slot = Mutex::new(Some("session".into()));
        assert!(flush_in(
            &ingest.state,
            &slot,
            &context(),
            10,
            Ok(scratch.0.clone()),
            || {}
        ));
        let first = ingest.begin_reload(None).unwrap();
        {
            let mut st = ingest.state.lock().unwrap();
            for n in 12..22 {
                turn(&mut st, n);
            }
        }
        let mut during = None;
        let folded = flush_in(&ingest.state, &slot, &context(), 10, Ok(scratch.0.clone()), || {
            assert!(
                ingest.finish_reload(&first, true, || {}),
                "poll must still see a valid held page"
            );
            assert_eq!(ingest.reload_was_released(&first), None);
            let next = ingest.begin_reload(None).unwrap();
            assert!(
                ingest.finish_reload(&next, true, || {}),
                "older during write can receive its page"
            );
            assert!(!page(&scratch.0, &next.id, next.before).turns.is_empty());
            during = Some(next);
            if abandon {
                ingest
                    .state
                    .lock()
                    .unwrap()
                    .fold(AgentDomainEvent::EffortReported { effort: None });
            }
        });
        assert_eq!(folded, !abandon);
        let next = during.unwrap();
        assert_eq!(ingest.finish_reload(&next, true, || {}), abandon);
        assert_eq!(ingest.reload_was_released(&next), if abandon { None } else { Some(12) });
    }
}

#[test]
fn poisoned_lost_chunk_limits_reload_and_the_limit_survives_a_move() {
    let scratch = Scratch::new();
    let ingest = ConversationIngest::start(
        Arc::new(super::super::idle_wakeups::Quiet::default()),
        "claude",
        "conversation".into(),
        "/project".into(),
        false,
        None,
        None,
        AgentSessionProjection::default(),
    );
    *ingest.state.lock().unwrap() = filled(12).into_inner().unwrap();
    let slot = Mutex::new(Some("session".into()));
    assert!(flush_in(
        &ingest.state,
        &slot,
        &context(),
        10,
        Ok(scratch.0.clone()),
        || {}
    ));
    {
        let mut st = ingest.state.lock().unwrap();
        for n in 12..22 {
            turn(&mut st, n);
        }
        let writer = st.archive_writer.as_mut().unwrap();
        writer.write_fault = Some((2, false));
        writer.fail_rollback = true;
    }
    assert!(flush_in(
        &ingest.state,
        &slot,
        &context(),
        10,
        Ok(scratch.0.clone()),
        || {}
    ));
    let ticket = ingest.begin_reload(None).unwrap();
    assert_eq!((ticket.base, ticket.before), (12, 2));
    assert_eq!(
        page(&scratch.0, &ticket.id, ticket.before).turns.last().unwrap().turn,
        1
    );
    {
        let mut st = ingest.state.lock().unwrap();
        assert!(st.archive_writer.as_ref().unwrap().is_poisoned());
        st.take_revised_delivery();
        st.take_evicted_calls_through(u64::MAX);
        let (core, handoff) = st.take_move_record(Some("session".into())).unwrap();
        assert_eq!(handoff.archive_readable_limit, Some(2));
        let (moved, _) = IngestState::moved(MovedIngest {
            projection: AgentSessionProjection::adopt(&core).unwrap(),
            core,
            state: handoff,
            lease: None,
        });
        assert_eq!(moved.archive_readable_limit, Some(2));
        let moved = Mutex::new(moved);
        {
            let mut dest = moved.lock().unwrap();
            for n in 22..24 {
                turn(&mut dest, n);
            }
        }
        assert!(flush_in(&moved, &slot, &context(), 10, Ok(scratch.0.clone()), || {}));
        assert_eq!(
            moved.lock().unwrap().archive_readable_limit,
            Some(2),
            "reopening cannot roll back complete lines from the lost chunk"
        );
        st.archive_readable_limit = Some(0);
    }
    assert!(ingest.begin_reload(None).is_none());
    ingest.state.lock().unwrap().archive_readable_limit = Some(2);
    ingest.clear_reload();
    {
        let mut st = ingest.state.lock().unwrap();
        for n in 22..24 {
            turn(&mut st, n);
        }
    }
    assert!(flush_in(
        &ingest.state,
        &slot,
        &context(),
        10,
        Ok(scratch.0.clone()),
        || {}
    ));
    assert_eq!(ingest.state.lock().unwrap().archive_readable_limit, None);
    let recovered = ingest.begin_reload(None).unwrap();
    assert_eq!((recovered.base, recovered.before), (14, 14));
    let loaded = page(&scratch.0, &recovered.id, recovered.before);
    assert_eq!(loaded.turns.iter().map(|t| t.turn).collect::<Vec<_>>(), vec![12, 13]);
    let mut st = ingest.state.lock().unwrap();
    st.archive_readable_limit = Some(2);
    st.fold(AgentDomainEvent::ConversationReset {
        new_conversation_id: "new".into(),
        trigger: None,
    });
    assert_eq!(st.archive_readable_limit, None);
}

#[test]
fn background_count_lifts_nested_unfinished_calls_but_not_denied_children() {
    let mut st = crate::ingestion::tests::state();
    background(&mut st);
    let root = &mut st.projection.tool_calls[0];
    root.background = false;
    root.result = Some(serde_json::from_value(json!({"content":"launched", "is_error":false})).unwrap());
    assert!(!running_background(root, &BTreeMap::new(), &BTreeSet::new()));
    assert!(running_background(
        root,
        &BTreeMap::new(),
        &BTreeSet::from(["child".into()])
    ));
    root.children.as_mut().unwrap().tool_calls[0].denied = Some(ToolCallDenial {
        reason_type: None,
        reason: None,
    });
    assert!(!running_background(
        root,
        &BTreeMap::new(),
        &BTreeSet::from(["child".into()])
    ));
}
