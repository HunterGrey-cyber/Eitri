use super::{AgentDomainEvent, AgentSessionProjection, ContentKind, ToolCallRecord};
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
    fn collect(parent: &ToolCallRecord, rows: &mut Vec<(u64, Vec<AgentDomainEvent>)>) {
        let Some(stream) = &parent.children else {
            return;
        };
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
            collect(call, rows);
        }
    }
    fn restore_marks(target: &mut [ToolCallRecord], source: &[ToolCallRecord], nested: bool) {
        for saved in source {
            if let Some(call) = find_call_mut(target, &saved.tool_use_id) {
                call.background = saved.background;
                if nested && !saved.tool_use_id.is_empty() {
                    call.denied = saved.denied.clone();
                    call.changed_input = saved.changed_input.clone();
                }
                if let (Some(target), Some(source)) = (&mut call.children, &saved.children) {
                    restore_marks(&mut target.tool_calls, &source.tool_calls, true);
                }
            }
        }
    }
    let mut rows = Vec::new();
    for call in calls {
        collect(call, &mut rows);
    }
    rows.sort_by_key(|(seq, _)| *seq);
    for (_, events) in rows {
        for event in events {
            state.apply(&event);
        }
    }
    restore_marks(&mut state.tool_calls, calls, false);
}

#[cfg(test)]
mod tests {
    use super::*;

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
