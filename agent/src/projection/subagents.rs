use super::{AgentDomainEvent, AgentSessionProjection, ContentKind, NoticeLevel, SessionNoticeKind, ToolCallRecord};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Content and calls belonging to one Agent call, independent of the main reply's open streams.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SubagentStream {
    pub transcript: Vec<super::TranscriptMessage>,
    pub thinking: Vec<super::ThinkingBlock>,
    pub tool_calls: Vec<ToolCallRecord>,
    pub assistant_message_open: bool,
    pub thinking_open: bool,
}

pub(super) fn find_call_mut<'a>(calls: &'a mut [ToolCallRecord], id: &str) -> Option<&'a mut ToolCallRecord> {
    for call in calls {
        if call.tool_use_id == id {
            return Some(call);
        }
        if let Some(stream) = &mut call.children {
            if let Some(found) = find_call_mut(&mut stream.tool_calls, id) {
                return Some(found);
            }
        }
    }
    None
}

const MAX_DEPTH: usize = 32;
const MAX_DESCENDANTS: usize = 10_000;
const DROPPED_ACTIVITY: &str = "Child activity was dropped because the subagent nesting or call limit was reached.";

pub(super) fn locate<'a>(calls: &'a [ToolCallRecord], id: &str, depth: usize) -> Option<(&'a ToolCallRecord, usize)> {
    for call in calls {
        if call.tool_use_id == id {
            return Some((call, depth));
        }
        if let Some(stream) = &call.children {
            if let Some(found) = locate(&stream.tool_calls, id, depth + 1) {
                return Some(found);
            }
        }
    }
    None
}

fn descendants(call: &ToolCallRecord) -> usize {
    call.children.as_ref().map_or(0, |stream| {
        stream.tool_calls.iter().map(|call| 1 + descendants(call)).sum()
    })
}

/// Checks the entire frame before inserting even synthetic parents. Existing trees are bounded,
/// and nested frame wrappers are walked without recursion.
pub(crate) fn subagent_frame_exceeds_limits(state: &AgentSessionProjection, event: &AgentDomainEvent) -> bool {
    let mut calls = state.tool_calls.as_slice();
    let mut depth = 0;
    let mut added = 0;
    let mut remaining = MAX_DESCENDANTS;
    let mut outer = true;
    let mut frame = event;
    while let AgentDomainEvent::SubagentFrame {
        parent_tool_use_id,
        frame: inner,
    } = frame
    {
        let found = calls.iter().find_map(|root| {
            locate(std::slice::from_ref(root), parent_tool_use_id, depth).map(|(parent, at)| (root, parent, at))
        });
        if outer {
            remaining = found.map_or(MAX_DESCENDANTS, |(root, _, _)| {
                MAX_DESCENDANTS.saturating_sub(descendants(root))
            });
            outer = false;
        }
        if let Some((_, parent, at)) = found {
            depth = at + 1;
            calls = parent
                .children
                .as_ref()
                .map_or(&[], |stream| stream.tool_calls.as_slice());
        } else {
            added += usize::from(depth > 0);
            depth += 1;
            calls = &[];
        }
        if depth > MAX_DEPTH || added > remaining {
            return true;
        }
        frame = inner;
    }
    !outer && matches!(frame, AgentDomainEvent::ToolCallStarted { .. }) && added + 1 > remaining
}

fn dropped_notice(state: &AgentSessionProjection) -> Option<AgentDomainEvent> {
    (!state
        .notices
        .iter()
        .any(|row| row.content.as_deref() == Some(DROPPED_ACTIVITY)))
    .then(|| AgentDomainEvent::SessionNotice {
        kind: SessionNoticeKind::Informational,
        level: NoticeLevel::Warning,
        content: Some(DROPPED_ACTIVITY.into()),
        scalars: Default::default(),
        task_id: None,
        tool_use_id: None,
        task_status: None,
    })
}

pub(super) fn fold(state: &mut AgentSessionProjection, event: &AgentDomainEvent) {
    #[cfg(test)]
    super::count_hook("subagents::fold");
    let AgentDomainEvent::SubagentFrame {
        parent_tool_use_id,
        frame,
    } = event
    else {
        return;
    };
    if subagent_frame_exceeds_limits(state, event) {
        if let Some(notice) = dropped_notice(state) {
            super::notices::fold(state, &notice);
        }
        return;
    }
    let mut leaf = frame.as_ref();
    while let AgentDomainEvent::SubagentFrame { frame, .. } = leaf {
        leaf = frame;
    }
    if !matches!(
        leaf,
        AgentDomainEvent::ContentDelta { .. }
            | AgentDomainEvent::ToolCallStarted { .. }
            | AgentDomainEvent::ToolCallCompleted { .. }
            | AgentDomainEvent::AssistantMessageBoundary { .. }
    ) {
        return;
    }
    let held = if let AgentDomainEvent::ToolCallStarted { tool_use_id, .. } = leaf {
        Some((
            tool_use_id.clone(),
            state.take_held_denial(tool_use_id),
            state.take_held_changed_input(tool_use_id),
        ))
    } else {
        None
    };
    let seq = state.last_revision;
    let active = state.active_turn_id.clone();
    if find_call_mut(&mut state.tool_calls, parent_tool_use_id).is_none() {
        state.tool_calls.push(ToolCallRecord {
            seq,
            turn_id: String::new(),
            tool_use_id: parent_tool_use_id.clone(),
            name: "background agent".into(),
            input: json!({}),
            result: None,
            denied: None,
            changed_input: None,
            children: None,
            background: true,
        });
    }
    let parent = find_call_mut(&mut state.tool_calls, parent_tool_use_id).unwrap();
    fold_stream(
        parent.children.get_or_insert_with(Default::default),
        frame,
        seq,
        active.as_deref(),
    );
    if let Some((id, denied, change)) = held {
        if let Some(call) = find_call_mut(&mut state.tool_calls, &id) {
            call.denied = denied;
            call.changed_input = change;
        }
    }
}

fn fold_stream(stream: &mut SubagentStream, event: &AgentDomainEvent, seq: u64, active: Option<&str>) {
    match event {
        AgentDomainEvent::ContentDelta { turn_id, kind, text } => {
            if text.is_empty() {
                return;
            }
            match kind {
                ContentKind::Text => {
                    if stream.assistant_message_open && !stream.transcript.is_empty() {
                        stream.transcript.last_mut().unwrap().text.push_str(text);
                    } else {
                        stream.transcript.push(super::TranscriptMessage {
                            seq,
                            text: text.clone(),
                        });
                        stream.assistant_message_open = true;
                    }
                }
                ContentKind::Thinking => {
                    if stream.thinking_open && !stream.thinking.is_empty() {
                        stream.thinking.last_mut().unwrap().text.push_str(text);
                    } else {
                        stream.thinking.push(super::ThinkingBlock {
                            seq,
                            turn_id: turn_id.clone(),
                            text: text.clone(),
                        });
                        stream.thinking_open = true;
                    }
                }
            }
        }
        AgentDomainEvent::AssistantMessageBoundary { .. } => {
            stream.assistant_message_open = false;
            stream.thinking_open = false;
        }
        AgentDomainEvent::ToolCallStarted {
            turn_id,
            tool_use_id,
            name,
            input,
        } => {
            stream.assistant_message_open = false;
            stream.thinking_open = false;
            stream.tool_calls.push(ToolCallRecord {
                seq,
                turn_id: turn_id.clone(),
                tool_use_id: tool_use_id.clone(),
                name: name.clone(),
                input: input.clone(),
                result: None,
                denied: None,
                changed_input: None,
                children: None,
                background: active != Some(turn_id),
            });
        }
        AgentDomainEvent::ToolCallCompleted {
            turn_id,
            tool_use_id,
            content,
            is_error,
        } => {
            if let Some(call) = find_call_mut(&mut stream.tool_calls, tool_use_id) {
                call.background |= active != Some(turn_id);
                call.result = Some(super::ToolCallResult {
                    content: content.clone(),
                    is_error: *is_error,
                });
            }
        }
        AgentDomainEvent::SubagentFrame {
            parent_tool_use_id,
            frame,
        } => {
            if find_call_mut(&mut stream.tool_calls, parent_tool_use_id).is_none() {
                stream.tool_calls.push(ToolCallRecord {
                    seq,
                    turn_id: String::new(),
                    tool_use_id: parent_tool_use_id.clone(),
                    name: "background agent".into(),
                    input: json!({}),
                    result: None,
                    denied: None,
                    changed_input: None,
                    children: None,
                    background: true,
                });
            }
            let parent = find_call_mut(&mut stream.tool_calls, parent_tool_use_id).unwrap();
            fold_stream(parent.children.get_or_insert_with(Default::default), frame, seq, active);
        }
        _ => {}
    }
}

fn stream_json(stream: &SubagentStream) -> Value {
    json!({
        "transcript": stream.transcript.iter().map(|row| json!({"seq": row.seq, "text": row.text})).collect::<Vec<_>>(),
        "thinking": stream.thinking.iter().map(|row| json!({"seq": row.seq, "turnId": row.turn_id, "text": row.text})).collect::<Vec<_>>(),
        "toolCalls": stream.tool_calls.iter().map(call_json).collect::<Vec<_>>(),
        "assistantMessageOpen": stream.assistant_message_open, "thinkingOpen": stream.thinking_open,
    })
}

fn call_json(call: &ToolCallRecord) -> Value {
    let mut value = json!({"seq": call.seq, "turnId": call.turn_id, "toolUseId": call.tool_use_id,
        "name": call.name, "input": call.input,
        "result": call.result.as_ref().map(|r| json!({"content": r.content, "isError": r.is_error}))});
    if let Some(denied) = &call.denied {
        value["denied"] = json!({"reasonType": denied.reason_type, "reason": denied.reason});
    }
    if let Some(change) = &call.changed_input {
        value["changedInput"] = json!(change);
    }
    if call.background {
        value["background"] = json!(true);
    }
    if let Some(children) = &call.children {
        value["children"] = stream_json(children);
    }
    value
}

pub(super) fn snapshot_json(state: &AgentSessionProjection, snapshot: &mut Value) {
    #[cfg(test)]
    super::count_hook("subagents::snapshot_json");
    if let Some(calls) = snapshot.get_mut("toolCalls").and_then(Value::as_array_mut) {
        for (entry, call) in calls.iter_mut().zip(&state.tool_calls) {
            if call.background {
                entry["background"] = json!(true);
            }
            if let Some(children) = &call.children {
                entry["children"] = stream_json(children);
            }
        }
    }
}

/// Refolds child rows through the same allocator as live events; saved sequence numbers are sorting keys only.
pub(crate) fn restore_children(state: &mut AgentSessionProjection, calls: &[ToolCallRecord]) {
    fn collect(
        parent: &ToolCallRecord,
        rows: &mut Vec<(u64, Vec<AgentDomainEvent>)>,
        depth: usize,
        remaining: &mut usize,
    ) -> bool {
        let Some(stream) = &parent.children else {
            return false;
        };
        if depth >= MAX_DEPTH {
            return !stream.transcript.is_empty() || !stream.thinking.is_empty() || !stream.tool_calls.is_empty();
        }
        let mut dropped = false;
        let wrap = |frame| AgentDomainEvent::SubagentFrame {
            parent_tool_use_id: parent.tool_use_id.clone(),
            frame: Box::new(frame),
        };
        for row in &stream.transcript {
            rows.push((
                row.seq,
                vec![
                    wrap(AgentDomainEvent::ContentDelta {
                        turn_id: parent.turn_id.clone(),
                        kind: ContentKind::Text,
                        text: row.text.clone(),
                    }),
                    wrap(AgentDomainEvent::AssistantMessageBoundary {
                        turn_id: parent.turn_id.clone(),
                    }),
                ],
            ));
        }
        for row in &stream.thinking {
            rows.push((
                row.seq,
                vec![
                    wrap(AgentDomainEvent::ContentDelta {
                        turn_id: row.turn_id.clone(),
                        kind: ContentKind::Thinking,
                        text: row.text.clone(),
                    }),
                    wrap(AgentDomainEvent::AssistantMessageBoundary {
                        turn_id: row.turn_id.clone(),
                    }),
                ],
            ));
        }
        for call in &stream.tool_calls {
            if *remaining == 0 {
                dropped = true;
                break;
            }
            *remaining -= 1;
            let mut events = vec![wrap(AgentDomainEvent::ToolCallStarted {
                turn_id: call.turn_id.clone(),
                tool_use_id: call.tool_use_id.clone(),
                name: call.name.clone(),
                input: call.input.clone(),
            })];
            if let Some(result) = &call.result {
                events.push(wrap(AgentDomainEvent::ToolCallCompleted {
                    turn_id: call.turn_id.clone(),
                    tool_use_id: call.tool_use_id.clone(),
                    content: result.content.clone(),
                    is_error: result.is_error,
                }));
            }
            rows.push((call.seq, events));
            dropped |= collect(call, rows, depth + 1, remaining);
        }
        dropped
    }
    fn restore_marks(target: &mut [ToolCallRecord], source: &[ToolCallRecord], depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        for saved in source {
            if let Some(call) = find_call_mut(target, &saved.tool_use_id) {
                call.background = saved.background;
                if depth > 0 && !saved.tool_use_id.is_empty() {
                    call.denied = saved.denied.clone();
                    call.changed_input = saved.changed_input.clone();
                }
                if let (Some(target), Some(source)) = (&mut call.children, &saved.children) {
                    restore_marks(&mut target.tool_calls, &source.tool_calls, depth + 1);
                }
            }
        }
    }
    let mut rows = Vec::new();
    let mut dropped = false;
    for call in calls {
        let mut remaining = MAX_DESCENDANTS;
        dropped |= collect(call, &mut rows, 0, &mut remaining);
    }
    if dropped {
        if let Some(notice) = dropped_notice(state) {
            state.apply(&notice);
        }
    }
    rows.sort_by_key(|(seq, _)| *seq);
    for (_, events) in rows {
        for event in events {
            state.apply(&event);
        }
    }
    restore_marks(&mut state.tool_calls, calls, 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(id: &str) -> AgentDomainEvent {
        AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: id.into(),
            name: "Agent".into(),
            input: json!({}),
        }
    }

    fn child(parent: &str, frame: AgentDomainEvent) -> AgentDomainEvent {
        AgentDomainEvent::SubagentFrame {
            parent_tool_use_id: parent.into(),
            frame: Box::new(frame),
        }
    }

    fn record(id: &str) -> ToolCallRecord {
        let mut state = AgentSessionProjection::default();
        state.apply(&start(id));
        state.tool_calls.pop().unwrap()
    }

    #[test]
    fn subagent_depth_is_capped_with_one_notice_and_a_safe_snapshot() {
        let mut state = AgentSessionProjection::default();
        state.apply(&start("p0"));
        for i in 1..=33 {
            state.apply(&child(&format!("p{}", i - 1), start(&format!("p{i}"))));
        }
        state.apply(&child("p32", start("again")));
        assert_eq!(descendants(&state.tool_calls[0]), 32);
        assert!(locate(&state.tool_calls, "p32", 0).is_some());
        assert!(locate(&state.tool_calls, "p33", 0).is_none());
        assert_eq!(state.notices.len(), 1);
        assert_eq!(state.notices[0].content.as_deref(), Some(DROPPED_ACTIVITY));
        assert_eq!(state.notices[0].seq, 33);
        assert_eq!(state.last_revision, 35);
        let mut snapshot = json!({ "toolCalls": [{}] });
        snapshot_json(&state, &mut snapshot);
        assert!(serde_json::to_string(&snapshot).unwrap().contains("p32"));
        drop(state.clone());
    }

    #[test]
    fn subagent_descendant_cap_is_per_root_and_keeps_existing_activity() {
        let mut state = AgentSessionProjection::default();
        let mut root = record("root");
        root.children = Some(SubagentStream {
            tool_calls: (0..MAX_DESCENDANTS - 1).map(|i| record(&format!("c{i}"))).collect(),
            ..Default::default()
        });
        state.tool_calls.push(root);
        state.apply(&child("root", child("missing", start("leaf"))));
        assert_eq!(descendants(&state.tool_calls[0]), MAX_DESCENDANTS - 1);
        assert!(locate(&state.tool_calls, "missing", 0).is_none());
        state.apply(&child("root", start("last")));
        state.apply(&child("root", start("excess")));
        state.apply(&child("root", start("excess2")));
        assert_eq!(descendants(&state.tool_calls[0]), MAX_DESCENDANTS);
        assert_eq!(state.notices.len(), 1);
        state.apply(&child(
            "root",
            AgentDomainEvent::ContentDelta {
                turn_id: "t1".into(),
                kind: ContentKind::Text,
                text: "still here".into(),
            },
        ));
        assert_eq!(
            state.tool_calls[0].children.as_ref().unwrap().transcript[0].text,
            "still here"
        );
        state.apply(&child("another", start("allowed")));
        assert_eq!(descendants(&state.tool_calls[1]), 1);
    }

    #[test]
    fn nested_frame_wrappers_are_checked_before_synthetic_parents_are_inserted() {
        let mut allowed = start("leaf");
        for i in 0..MAX_DEPTH {
            allowed = child(&format!("p{i}"), allowed);
        }
        let mut normal = AgentSessionProjection::default();
        normal.apply(&allowed);
        assert_eq!(descendants(&normal.tool_calls[0]), MAX_DEPTH);
        assert!(normal.notices.is_empty());

        let mut frame = start("leaf");
        for i in 0..=MAX_DEPTH {
            frame = child(&format!("p{i}"), frame);
        }
        let mut state = AgentSessionProjection::default();
        state.apply(&frame);
        assert!(state.tool_calls.is_empty());
        assert_eq!(state.notices.len(), 1);
    }

    #[test]
    fn restored_subagents_observe_depth_and_descendant_limits() {
        let mut root = record("p33");
        for i in (0..33).rev() {
            let mut parent = record(&format!("p{i}"));
            root.seq = i as u64 + 1;
            parent.children = Some(SubagentStream {
                tool_calls: vec![root],
                ..Default::default()
            });
            root = parent;
        }
        let mut wide = record("wide");
        wide.children = Some(SubagentStream {
            tool_calls: (0..=MAX_DESCENDANTS).map(|i| record(&format!("w{i}"))).collect(),
            ..Default::default()
        });
        let saved = vec![root, wide];
        let mut state = AgentSessionProjection::default();
        state.apply(&start("p0"));
        state.apply(&start("wide"));
        restore_children(&mut state, &saved);
        assert_eq!(descendants(&state.tool_calls[0]), MAX_DEPTH);
        assert_eq!(descendants(&state.tool_calls[1]), MAX_DESCENDANTS);
        assert_eq!(state.notices.len(), 1);
        assert!(locate(&state.tool_calls, "p33", 0).is_none());
        assert!(locate(&state.tool_calls, "w10000", 0).is_none());
    }

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../../agent-ui/web/src/fixtures/subagent-frames.json")).unwrap()
    }

    #[test]
    fn children_are_nested_and_never_change_main_state() {
        for case in fixture()["cases"].as_array().unwrap() {
            let events: Vec<AgentDomainEvent> = serde_json::from_value(case["events"].clone()).unwrap();
            let mut state = AgentSessionProjection::default();
            for event in &events {
                let mut before = serde_json::to_value(&state).unwrap();
                state.apply(event);
                if matches!(event, AgentDomainEvent::SubagentFrame { .. }) {
                    let mut after = serde_json::to_value(&state).unwrap();
                    for value in [&mut before, &mut after] {
                        value.as_object_mut().unwrap().remove("tool_calls");
                        value.as_object_mut().unwrap().remove("last_revision");
                        value.as_object_mut().unwrap().remove("denials_before_their_call");
                        value
                            .as_object_mut()
                            .unwrap()
                            .remove("changed_inputs_before_their_call");
                    }
                    assert_eq!(before, after, "{}: {event:?}", case["name"]);
                }
            }
            let calls: Vec<_> = state.tool_calls.iter().map(call_json).collect();
            assert_eq!(json!(calls), case["snapshot"]["state"]["toolCalls"], "{}", case["name"]);
        }
    }

    /// Two replies of one subagent are two stored rows, and restoring them gives two rows again: the
    /// refold closes the reply after each row it places.
    #[test]
    fn a_subagents_two_replies_restore_as_two_rows() {
        let frame = |frame| AgentDomainEvent::SubagentFrame {
            parent_tool_use_id: "agent-call".into(),
            frame: Box::new(frame),
        };
        let reply = |text: &str| {
            frame(AgentDomainEvent::ContentDelta {
                turn_id: "t1".into(),
                kind: ContentKind::Text,
                text: text.into(),
            })
        };
        let events = [
            AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
            AgentDomainEvent::ToolCallStarted {
                turn_id: "t1".into(),
                tool_use_id: "agent-call".into(),
                name: "Agent".into(),
                input: json!({}),
            },
            reply("First reply."),
            frame(AgentDomainEvent::AssistantMessageBoundary { turn_id: "t1".into() }),
            reply("Second reply."),
        ];
        let mut live = AgentSessionProjection::default();
        let mut restored = AgentSessionProjection::default();
        for event in &events[..2] {
            live.apply(event);
            restored.apply(event);
        }
        for event in &events[2..] {
            live.apply(event);
        }
        let rows = |state: &AgentSessionProjection| -> Vec<String> {
            state.tool_calls[0]
                .children
                .as_ref()
                .unwrap()
                .transcript
                .iter()
                .map(|row| row.text.clone())
                .collect()
        };
        assert_eq!(rows(&live), ["First reply.", "Second reply."]);
        restore_children(&mut restored, &live.tool_calls.clone());
        assert_eq!(rows(&restored), rows(&live));
    }

    #[test]
    fn a_cli_cycle_whose_only_output_is_a_background_childs_reply_keeps_its_start_row() {
        use super::super::{TurnEndDetail, TurnOrigin, TurnOutcome, NO_RESULT_REASON};
        let started = |id: &str| AgentDomainEvent::TurnStarted { turn_id: id.into() };
        let ended = |id: &str, reason: Option<&str>| AgentDomainEvent::TurnCompleted {
            turn_id: id.into(),
            outcome: TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: TurnEndDetail {
                reason: reason.map(str::to_string),
                ..TurnEndDetail::default()
            },
        };
        let cycle = |child_text: Option<&str>| {
            let mut state = AgentSessionProjection::default();
            let mut events = vec![
                started("t1"),
                AgentDomainEvent::ToolCallStarted {
                    turn_id: "t1".into(),
                    tool_use_id: "agent-call".into(),
                    name: "Agent".into(),
                    input: json!({ "run_in_background": true }),
                },
                ended("t1", None),
                started("c1"),
                AgentDomainEvent::TurnAttributed {
                    turn_id: "c1".into(),
                    origin: TurnOrigin::Cli,
                    cause: None,
                },
            ];
            if let Some(text) = child_text {
                events.push(AgentDomainEvent::SubagentFrame {
                    parent_tool_use_id: "agent-call".into(),
                    frame: Box::new(AgentDomainEvent::ContentDelta {
                        turn_id: "c1".into(),
                        kind: ContentKind::Text,
                        text: text.into(),
                    }),
                });
            }
            events.push(ended("c1", Some(NO_RESULT_REASON)));
            for event in &events {
                state.apply_with_derived(event);
            }
            state
        };
        assert_eq!(cycle(Some("child reply")).turn_starts.len(), 1);
        assert!(cycle(None).turn_starts.is_empty());
    }
}
