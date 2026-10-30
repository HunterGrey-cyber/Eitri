//! An ACP agent's words as Eitri's events: tool calls, message chunks, how a turn ended and what it used.
//!
//! [`Fold`] keeps the bookkeeping of one turn (which calls are open, which have ended, what the last
//! message said) and turns updates into events through a callback, so the live stream and the replay of a
//! loaded session share one translation.

use super::wire::{ChunkKind, PromptUsage, ToolCallFields, Update};
use crate::projection::{
    AgentDomainEvent, ContentKind, TokenUsage, TurnEndDetail, TurnOutcome, UsageInfo, MAX_TURN_END_MESSAGE_CHARS,
};
use crate::providers::codex::translate::{bounded, MAX_RESULT_BYTES};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

/// The name a tool call is shown under: `acp:` and the kind the agent gave it.
pub fn tool_name(kind: Option<&str>) -> String {
    match kind.filter(|k| !k.is_empty()) {
        Some(kind) => format!("acp:{kind}"),
        None => "acp:other".to_string(),
    }
}

/// A value that is no larger than [`MAX_RESULT_BYTES`] written out; a larger one is replaced by its text,
/// cut.
fn bounded_value(value: Value) -> Value {
    let text = value.to_string();
    if text.len() <= MAX_RESULT_BYTES {
        value
    } else {
        Value::String(bounded(&text))
    }
}

/// Whether a card cannot show the whole call: [`tool_input`] has to cut its raw input, its diffs together
/// are larger than a card draws in full, or a diff carries a path or a text that is not a string, which the
/// card would draw as empty. A card for such a call must not be approvable: what would be approved is more
/// than the card shows.
pub fn input_is_cut(fields: &ToolCallFields) -> bool {
    let raw_cut = fields
        .raw_input
        .as_ref()
        .is_some_and(|input| input.to_string().len() > MAX_RESULT_BYTES);
    raw_cut || diffs_are_unshowable(&fields.diffs())
}

/// Whether diffs, as [`ToolCallFields::diffs`] gives them, cannot all be drawn whole on a card.
fn diffs_are_unshowable(diffs: &[Value]) -> bool {
    let mut total = 0usize;
    for diff in diffs {
        if !diff["path"].is_string() || !diff["newText"].is_string() {
            return true;
        }
        // No old text is a new file; anything else must be text to be drawn.
        let old = match &diff["oldText"] {
            Value::Null => 0,
            Value::String(text) => text.len(),
            _ => return true,
        };
        total = total
            .saturating_add(old)
            .saturating_add(diff["newText"].as_str().map_or(0, str::len));
        if total > MAX_RESULT_BYTES {
            return true;
        }
    }
    false
}

/// What a call's row and its permission card show of it.
pub fn tool_input(fields: &ToolCallFields) -> Value {
    json!({
        "title": fields.title,
        "rawInput": fields.raw_input.clone().map(bounded_value),
        "locations": fields.locations,
        "diffs": fields.diffs(),
    })
}

/// A call's result: the text it produced, else the raw output the agent attached, else nothing.
fn tool_result(fields: &ToolCallFields) -> Value {
    let text = fields.text();
    if !text.is_empty() {
        return Value::String(bounded(&text));
    }
    match &fields.raw_output {
        Some(output) => bounded_value(output.clone()),
        None => Value::String(String::new()),
    }
}

/// A message cut to what a turn's ending keeps.
fn cut_message(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= MAX_TURN_END_MESSAGE_CHARS {
        return text.to_string();
    }
    let kept: String = text
        .chars()
        .take(MAX_TURN_END_MESSAGE_CHARS.saturating_sub(1))
        .collect();
    format!("{kept}\u{2026}")
}

fn ending(outcome: TurnOutcome, reason: &str, message: Option<String>) -> (TurnOutcome, TurnEndDetail) {
    (
        outcome,
        TurnEndDetail {
            origin_kind: None,
            num_turns: None,
            reason: Some(reason.to_string()),
            api_error_status: None,
            message: message.map(|m| cut_message(&m)).filter(|m| !m.is_empty()),
        },
    )
}

/// How a turn ended, from the `stopReason` of its `session/prompt` answer.
pub fn stop_ending(stop: Option<&str>) -> (TurnOutcome, TurnEndDetail) {
    match stop {
        Some("end_turn") => (TurnOutcome::Completed, TurnEndDetail::default()),
        Some("cancelled") => (TurnOutcome::Interrupted, TurnEndDetail::default()),
        Some("max_tokens") => ending(TurnOutcome::LimitReached, "max_tokens", None),
        Some("max_turn_requests") => ending(TurnOutcome::LimitReached, "max_turns", None),
        Some("refusal") => ending(
            TurnOutcome::Failed,
            "refusal",
            Some("the agent refused to continue".to_string()),
        ),
        Some(other) => ending(
            TurnOutcome::Failed,
            other,
            Some(format!(
                "the agent ended the turn with a reason Eitri does not know: {other}"
            )),
        ),
        None => ending(
            TurnOutcome::Failed,
            "no_stop_reason",
            Some("the agent ended the turn without saying why".to_string()),
        ),
    }
}

/// A turn that ended in an error: the agent's own message, or Eitri's account of why there was none.
pub fn failure_ending(reason: &str, message: &str) -> (TurnOutcome, TurnEndDetail) {
    ending(TurnOutcome::Failed, reason, Some(message.to_string()))
}

/// What a finished turn used. ACP reports no cost, so the cost is marked absent, never zero. Thought tokens
/// are the agent's own accounting of a part of its output and are not added to it.
pub fn usage_of(usage: &PromptUsage, model: Option<&str>) -> UsageInfo {
    UsageInfo {
        total_cost_usd: 0.0,
        cost_reported: false,
        num_turns: None,
        tokens: Some(TokenUsage {
            input: usage.input,
            output: usage.output,
            cache_creation: usage.cached_write,
            cache_read: usage.cached_read,
        }),
        model: model.filter(|m| !m.is_empty()).map(str::to_string),
    }
}

/// Whether a call changes files, so that the files it names decide which changes are the agent's.
fn names_files(kind: Option<&str>) -> bool {
    matches!(kind, Some("edit" | "delete" | "move"))
}

/// The bookkeeping of one turn's stream.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Fold {
    /// Calls started and not ended.
    open: HashSet<String>,
    /// Calls that ended: a repeated or late update about one draws no second row.
    done: HashSet<String>,
    /// What the updates so far said about each call that has not ended, merged. A row's input is fixed
    /// when it starts and its result is made when the call ends, so both read this.
    calls: HashMap<String, ToolCallFields>,
    /// The text of the last whole message, which is the turn's result.
    pub last_message: String,
    message_open: bool,
    message_id: Option<String>,
}

impl Fold {
    /// Forgets the turn's calls, as its end does.
    pub fn clear(&mut self) {
        *self = Fold::default();
    }

    /// Whether a call's row is open.
    pub fn is_open(&self, id: &str) -> bool {
        self.open.contains(id)
    }

    /// Applies one update of the turn `turn_id`. A user's chunk, a mode report and a context figure are not
    /// the turn's stream and are left to the caller.
    pub fn apply(&mut self, turn_id: &str, update: &Update, out: &mut dyn FnMut(AgentDomainEvent)) {
        match update {
            Update::Chunk {
                kind: ChunkKind::Agent,
                message_id,
                text: Some(text),
            } => {
                let new_message = !self.message_open || (message_id.is_some() && *message_id != self.message_id);
                if self.message_open
                    && message_id.is_some()
                    && self.message_id.is_some()
                    && *message_id != self.message_id
                {
                    out(AgentDomainEvent::AssistantMessageBoundary {
                        turn_id: turn_id.to_string(),
                    });
                }
                if new_message {
                    self.last_message.clear();
                }
                self.message_open = true;
                if message_id.is_some() {
                    self.message_id = message_id.clone();
                }
                if text.is_empty() {
                    return;
                }
                self.last_message.push_str(text);
                out(AgentDomainEvent::ContentDelta {
                    turn_id: turn_id.to_string(),
                    kind: ContentKind::Text,
                    text: text.clone(),
                });
            }
            Update::Chunk {
                kind: ChunkKind::Thought,
                text: Some(text),
                ..
            } if !text.is_empty() => out(AgentDomainEvent::ContentDelta {
                turn_id: turn_id.to_string(),
                kind: ContentKind::Thinking,
                text: text.clone(),
            }),
            Update::ToolCall(fields) | Update::ToolCallUpdate(fields) => {
                self.message_open = false;
                self.tool(turn_id, fields, out);
            }
            _ => {}
        }
    }

    /// Remembers what `fields` say about their call next to what earlier updates said, and returns the
    /// whole. `None` when the call has ended: nothing about it is kept.
    fn merge(&mut self, fields: &ToolCallFields) -> Option<&mut ToolCallFields> {
        if self.done.contains(&fields.id) {
            return None;
        }
        let kept = self.calls.entry(fields.id.clone()).or_insert_with(|| fields.clone());
        kept.merge(fields);
        Some(kept)
    }

    /// Opens a call's row if it has none and has not ended, and returns what is known of the call: the
    /// updates so far merged with `fields`. A call that ended gives `fields` back as they are.
    pub fn ensure_started(
        &mut self,
        turn_id: &str,
        fields: &ToolCallFields,
        out: &mut dyn FnMut(AgentDomainEvent),
    ) -> ToolCallFields {
        let Some(whole) = self.merge(fields).map(|kept| kept.clone()) else {
            return fields.clone();
        };
        self.start(turn_id, &whole, out);
        whole
    }

    fn start(&mut self, turn_id: &str, whole: &ToolCallFields, out: &mut dyn FnMut(AgentDomainEvent)) {
        if !self.open.insert(whole.id.clone()) {
            return;
        }
        out(AgentDomainEvent::ToolCallStarted {
            turn_id: turn_id.to_string(),
            tool_use_id: whole.id.clone(),
            name: tool_name(whole.kind.as_deref()),
            input: tool_input(whole),
        });
    }

    fn tool(&mut self, turn_id: &str, fields: &ToolCallFields, out: &mut dyn FnMut(AgentDomainEvent)) {
        let Some(whole) = self.merge(fields).map(|kept| kept.clone()) else {
            return;
        };
        // A change that has not said which file it touches waits for that (or for its first sign of
        // progress): a row's input is fixed when it starts, and turn review reads the files from it.
        let waiting = names_files(whole.kind.as_deref())
            && !whole.names_a_path()
            && matches!(whole.status.as_deref(), None | Some("pending"));
        if waiting {
            return;
        }
        self.start(turn_id, &whole, out);
        if whole.is_terminal() {
            self.open.remove(&whole.id);
            self.calls.remove(&whole.id);
            self.done.insert(whole.id.clone());
            out(AgentDomainEvent::ToolCallCompleted {
                turn_id: turn_id.to_string(),
                tool_use_id: whole.id.clone(),
                content: tool_result(&whole),
                is_error: whole.failed(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reasons_have_one_outcome_each() {
        assert_eq!(stop_ending(Some("end_turn")).0, TurnOutcome::Completed);
        assert_eq!(stop_ending(Some("cancelled")).0, TurnOutcome::Interrupted);
        let (outcome, detail) = stop_ending(Some("max_tokens"));
        assert_eq!(outcome, TurnOutcome::LimitReached);
        assert_eq!(detail.reason.as_deref(), Some("max_tokens"));
        let (outcome, detail) = stop_ending(Some("max_turn_requests"));
        assert_eq!(outcome, TurnOutcome::LimitReached);
        assert_eq!(detail.reason.as_deref(), Some("max_turns"));
        assert_eq!(stop_ending(Some("refusal")).0, TurnOutcome::Failed);
        // A reason added later, or none, fails the turn and says so.
        let (outcome, detail) = stop_ending(Some("tool_budget"));
        assert_eq!(outcome, TurnOutcome::Failed);
        assert_eq!(detail.reason.as_deref(), Some("tool_budget"));
        assert_eq!(stop_ending(None).0, TurnOutcome::Failed);
    }

    fn with_diffs(diffs: Vec<Value>) -> ToolCallFields {
        let content = diffs
            .into_iter()
            .map(|mut d| {
                d["type"] = json!("diff");
                d
            })
            .collect();
        ToolCallFields {
            id: "c1".to_string(),
            title: None,
            kind: Some("edit".to_string()),
            status: None,
            raw_input: None,
            raw_output: None,
            locations: None,
            content,
            content_given: true,
        }
    }

    #[test]
    fn a_carried_empty_content_list_clears_earlier_text() {
        let start = ToolCallFields::parse(&json!({
            "toolCallId": "c1", "status": "in_progress",
            "content": [{"type": "content", "content": {"type": "text", "text": "working"}}]
        }))
        .unwrap();
        let update = ToolCallFields::parse(&json!({
            "toolCallId": "c1", "status": "completed", "content": [], "rawOutput": "final"
        }))
        .unwrap();
        let mut fold = Fold::default();
        let mut events = Vec::new();
        fold.apply("t1", &Update::ToolCall(start), &mut |event| events.push(event));
        fold.apply("t1", &Update::ToolCallUpdate(update), &mut |event| events.push(event));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentDomainEvent::ToolCallCompleted { tool_use_id, content, is_error: false, .. }
                if tool_use_id == "c1" && content == "final"
        )));
    }

    #[test]
    fn an_update_without_content_keeps_earlier_text() {
        let start = ToolCallFields::parse(&json!({
            "toolCallId": "c1", "status": "in_progress",
            "content": [{"type": "content", "content": {"type": "text", "text": "working"}}]
        }))
        .unwrap();
        let update = ToolCallFields::parse(&json!({
            "toolCallId": "c1", "status": "completed", "rawOutput": "final"
        }))
        .unwrap();
        let mut fold = Fold::default();
        let mut events = Vec::new();
        fold.apply("t1", &Update::ToolCall(start), &mut |event| events.push(event));
        fold.apply("t1", &Update::ToolCallUpdate(update), &mut |event| events.push(event));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentDomainEvent::ToolCallCompleted { tool_use_id, content, is_error: false, .. }
                if tool_use_id == "c1" && content == "working"
        )));
    }

    #[test]
    fn a_diff_a_card_cannot_draw_whole_makes_the_input_cut() {
        let small = json!({"path": "/w/a", "oldText": "a\n", "newText": "b\n"});
        assert!(!input_is_cut(&with_diffs(vec![small.clone()])));
        // A new file has no old text.
        assert!(!input_is_cut(&with_diffs(vec![
            json!({"path": "/w/a", "oldText": null, "newText": "b"})
        ])));
        // Many lines are still drawn whole while the texts stay within the bound.
        let lines = "line\n".repeat(401);
        assert!(!input_is_cut(&with_diffs(vec![
            json!({"path": "/w/a", "oldText": lines, "newText": ""})
        ])));
        // Over the bound, one diff or several together.
        let half = "x".repeat(MAX_RESULT_BYTES / 2 + 1);
        assert!(input_is_cut(&with_diffs(vec![
            json!({"path": "/w/a", "oldText": half, "newText": half})
        ])));
        assert!(input_is_cut(&with_diffs(vec![
            json!({"path": "/w/a", "oldText": null, "newText": half}),
            json!({"path": "/w/b", "oldText": null, "newText": half}),
        ])));
        // A path or a text the card would draw as empty.
        assert!(input_is_cut(&with_diffs(vec![
            json!({"path": 1, "oldText": "a", "newText": "b"})
        ])));
        assert!(input_is_cut(&with_diffs(vec![
            json!({"path": "/w/a", "oldText": {"a": 1}, "newText": "b"})
        ])));
        assert!(input_is_cut(&with_diffs(vec![json!({"path": "/w/a", "oldText": "a"})])));
        assert!(input_is_cut(&with_diffs(vec![
            json!({"path": "/w/a", "oldText": "a", "newText": ["b"]})
        ])));
    }

    #[test]
    fn a_long_failure_message_is_cut_to_what_an_ending_keeps() {
        let (_, detail) = failure_ending("agent_error", &"x".repeat(5000));
        let message = detail.message.unwrap_or_default();
        assert_eq!(message.chars().count(), MAX_TURN_END_MESSAGE_CHARS);
    }
}
