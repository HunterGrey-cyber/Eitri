//! Reading a resumed thread's history back in small pages.
//!
//! `thread/resume` is sent without the thread's turns, because one answer holding a long conversation can
//! pass the 16 MiB line cap and lose the whole resume. The history is asked for afterwards instead: the
//! turns, ten at a time and without their items, then each turn's items, fifty at a time. A page that is
//! still too large to read is skipped, and the turn it belongs to says so; it never fails the resume.
//!
//! The pager only asks questions and folds the answers into events. It does not know about locks or the
//! child: the caller hands it a function that makes one call, which is where the bounded wait and the
//! holding of live messages happen.

use super::translate;
use super::wire::{
    method, ThreadItem, ThreadItemsListParams, ThreadItemsListResponse, ThreadTurnsListParams, ThreadTurnsListResponse,
    Turn, TurnStatus,
};
use crate::jsonrpc_link::LinkError;
use crate::projection::{thinking_open_after, AgentDomainEvent, ContentKind, TurnEndDetail, TurnOutcome};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::Instant;

/// The turn id the notes about the history as a whole sit in.
const NOTE_TURN: &str = "eitri-history";

/// What a note about missing history says when a page was larger than the link reads.
const TOO_LARGE: &str = "too large to show";

/// Pages of one turn's items read at most. A turn longer than that is shown in part, so a Codex that hands
/// out a new cursor for ever cannot hold the start.
const ITEM_PAGES_MAX: usize = 200;

/// Items of one turn kept at most, for the same reason.
const ITEMS_MAX: usize = 10_000;

/// Why one call of the pager got no answer.
#[derive(Debug)]
pub enum AskError {
    /// The link's own failure.
    Link(LinkError),
    /// Codex sent more live messages while the history was read than can be kept for after it. Reading on
    /// would leave the link's reader blocked behind them, with every later answer stuck there too.
    Flooded,
}

impl From<LinkError> for AskError {
    fn from(e: LinkError) -> AskError {
        AskError::Link(e)
    }
}

/// One call on the link: the method, its params, and the answer.
pub type Ask<'a> = dyn FnMut(&str, Value) -> Result<Value, AskError> + 'a;

/// Why the whole history stopped being read: the rest is not asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// The deadline passed, or Codex did not answer a page in time. One page that hangs would hang the next
    /// one too, so nothing more is asked.
    Slow,
    Flooded,
    /// The link is gone; the pump reports why.
    Closed,
}

impl Stop {
    fn of(e: &AskError) -> Option<Stop> {
        match e {
            AskError::Link(LinkError::Timeout) => Some(Stop::Slow),
            AskError::Link(LinkError::Closed) => Some(Stop::Closed),
            AskError::Flooded => Some(Stop::Flooded),
            AskError::Link(_) => None,
        }
    }

    /// The note in a turn of its own that says the rest of the history is missing, if one is said. `kept` is
    /// whether any turn of the history is shown below it.
    fn note(self, kept: bool) -> Vec<AgentDomainEvent> {
        match self {
            Stop::Slow => history_gap(kept, "could not be loaded: reading it took too long"),
            Stop::Flooded => history_gap(kept, "could not be loaded: Codex sent too much while it was being read"),
            Stop::Closed => Vec::new(),
        }
    }
}

/// A note in a turn of its own for history that is missing. With turns shown below it, what is missing is
/// the turns before them; with none, it is the whole history.
fn history_gap(kept: bool, problem: &str) -> Vec<AgentDomainEvent> {
    let subject = if kept {
        "earlier turns of this conversation"
    } else {
        "the history of this conversation"
    };
    note_turn(&format!("{subject} {problem}"))
}

/// The events of a thread's history, oldest turn first. Anything that cannot be read becomes a note row in
/// the turn it belongs to, or in a turn of its own when the list of turns is what failed. Stops at
/// `deadline` with a note, since a very long thread must not hold the start for ever; a page that is not
/// answered in time stops it the same way. A cursor that comes back, and a turn listed twice, are not
/// followed again. Turns are read newest first so the budget leaves older history out, then their event
/// groups are reversed into chronological order with any missing-history note before them.
pub fn page_history(thread_id: &str, deadline: Instant, ask: &mut Ask<'_>) -> Vec<AgentDomainEvent> {
    let mut events = Vec::new();
    let mut turn_groups = Vec::new();
    let mut cursor: Option<String> = None;
    let mut cursors_seen: HashSet<String> = HashSet::new();
    let mut turns_seen: HashSet<String> = HashSet::new();
    'pages: loop {
        if Instant::now() >= deadline {
            events.extend(Stop::Slow.note(!turn_groups.is_empty()));
            break;
        }
        let params = match serde_json::to_value(ThreadTurnsListParams::new(thread_id.to_string(), cursor.clone())) {
            Ok(params) => params,
            Err(_) => break,
        };
        let page = match ask(method::THREAD_TURNS_LIST, params) {
            Ok(value) => match serde_json::from_value::<ThreadTurnsListResponse>(value) {
                Ok(page) => page,
                Err(e) => {
                    eprintln!("agent: Codex's list of turns could not be read: {e}");
                    events.extend(history_gap(!turn_groups.is_empty(), "could not be read"));
                    break;
                }
            },
            Err(AskError::Link(LinkError::TooLarge)) => {
                eprintln!("agent: Codex's list of turns was larger than 16 MiB; skipped");
                events.extend(note_turn(&format!(
                    "a page of this conversation's history was {TOO_LARGE}"
                )));
                break;
            }
            Err(e) => {
                eprintln!("agent: Codex's list of turns failed: {e:?}");
                match Stop::of(&e) {
                    Some(stop) => events.extend(stop.note(!turn_groups.is_empty())),
                    None => events.extend(history_gap(!turn_groups.is_empty(), "could not be loaded")),
                }
                break;
            }
        };
        let last_of_page = page.data.len().saturating_sub(1);
        for (at, turn) in page.data.iter().enumerate() {
            if !turns_seen.insert(turn.id.clone()) {
                eprintln!("agent: Codex listed a turn of the history twice; shown once");
                continue;
            }
            let (items, complete) = load_items(thread_id, turn, deadline, ask);
            if let Complete::Stopped(stop) = complete {
                // A turn that stopped before any of its items arrived would show as an empty turn; it is left
                // out and the note covers it. One with items that is the last turn Codex has is the oldest and
                // already says it is partial, so a note about earlier turns would name turns that do not exist.
                let left_out = items.is_empty();
                if !left_out {
                    turn_groups.push(turn_events(thread_id, turn, &items, complete));
                }
                if left_out || at < last_of_page || page.next_cursor.is_some() {
                    events.extend(stop.note(!turn_groups.is_empty()));
                }
                break 'pages;
            }
            turn_groups.push(turn_events(thread_id, turn, &items, complete));
        }
        match page.next_cursor {
            // A cursor already followed would ask for pages already read, perhaps for ever.
            Some(next) if !page.data.is_empty() && cursors_seen.insert(next.clone()) => cursor = Some(next),
            _ => break,
        }
    }
    events.extend(turn_groups.into_iter().rev().flatten());
    events
}

/// How a turn's items were read: what arrived, and whether every page did.
fn load_items(thread_id: &str, turn: &Turn, deadline: Instant, ask: &mut Ask<'_>) -> (Vec<ThreadItem>, Complete) {
    let mut items = Vec::new();
    let mut cursor: Option<String> = None;
    let mut cursors_seen: HashSet<String> = HashSet::new();
    let mut pages = 0;
    loop {
        if Instant::now() >= deadline {
            return (items, Complete::Stopped(Stop::Slow));
        }
        if pages >= ITEM_PAGES_MAX || items.len() >= ITEMS_MAX {
            eprintln!("agent: a turn of the history from Codex has more items than are read; shown in part");
            items.truncate(ITEMS_MAX);
            return (items, Complete::No);
        }
        pages += 1;
        let params = match serde_json::to_value(ThreadItemsListParams::new(
            thread_id.to_string(),
            turn.id.clone(),
            cursor.clone(),
        )) {
            Ok(params) => params,
            Err(_) => return (items, Complete::No),
        };
        let page = match ask(method::THREAD_ITEMS_LIST, params) {
            Ok(value) => match serde_json::from_value::<ThreadItemsListResponse>(value) {
                Ok(page) => page,
                Err(e) => {
                    eprintln!("agent: Codex's items of a turn could not be read: {e}");
                    return (items, Complete::No);
                }
            },
            Err(AskError::Link(LinkError::TooLarge)) => {
                eprintln!("agent: a page of a turn's items from Codex was larger than 16 MiB; skipped");
                return (items, Complete::TooLarge);
            }
            Err(e) => {
                eprintln!("agent: Codex's items of a turn failed: {e:?}");
                return match Stop::of(&e) {
                    Some(stop) => (items, Complete::Stopped(stop)),
                    None => (items, Complete::No),
                };
            }
        };
        let empty = page.data.is_empty();
        items.extend(page.data.into_iter().filter(|e| e.turn_id == turn.id).map(|e| e.item));
        match page.next_cursor {
            Some(next) if !empty && cursors_seen.insert(next.clone()) => cursor = Some(next),
            _ => break,
        }
    }
    // A Codex that listed no items but sent them with the turn is still read.
    if items.is_empty() && !turn.items.is_empty() {
        items = turn.items.clone();
    }
    (items, Complete::Yes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Complete {
    Yes,
    /// A page was skipped for its size.
    TooLarge,
    /// A page could not be had for another reason, or the turn had more than is read.
    No,
    /// Reading stopped in this turn, and the rest of the history is not asked for.
    Stopped(Stop),
}

/// The text of a user message's parts.
fn user_text(content: &[Value]) -> String {
    content
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The client id a message read inside a past turn is reported under: never one Eitri hands out live
/// (`eitri-<tab>-<n>`), so the projection's record of live messages' outcomes is not answered by history.
fn history_message_id(item_id: &str) -> String {
    format!("{NOTE_TURN}-message-{item_id}")
}

/// The events of one past turn: the prompt, the start, what it said and ran, a note for what could not be
/// read, and the end.
///
/// The user messages the turn opens with are its prompt, before the start. A user message after anything
/// else of the turn was put into it while it ran (`turn/steer`): it is reported where it stands, as the
/// live stream reports it (`UserMessageDelivered`), so it reads as part of the turn, below what came
/// before it, and ends the reply and the thought before it exactly as it did live.
fn turn_events(thread_id: &str, turn: &Turn, items: &[ThreadItem], complete: Complete) -> Vec<AgentDomainEvent> {
    let mut events = Vec::new();
    let opening = items
        .iter()
        .take_while(|item| matches!(item, ThreadItem::UserMessage(_)))
        .count();
    for item in &items[..opening] {
        if let ThreadItem::UserMessage(m) = item {
            events.push(AgentDomainEvent::UserPromptSubmitted {
                text: user_text(&m.content),
            });
        }
    }
    events.push(AgentDomainEvent::TurnStarted {
        turn_id: turn.id.clone(),
    });
    let mut last_message = String::new();
    // Whether the thinking block is open after `events[..thought_scanned]`, as the projection folds them.
    let (mut thought_open, mut thought_scanned) = (false, 0);
    for item in &items[opening..] {
        match item {
            ThreadItem::UserMessage(m) => {
                events.push(AgentDomainEvent::UserMessageDelivered {
                    client_message_id: history_message_id(&m.id),
                    turn_id: turn.id.clone(),
                    text: user_text(&m.content),
                });
            }
            ThreadItem::AgentMessage(m) => {
                events.push(AgentDomainEvent::AssistantMessageBoundary {
                    turn_id: turn.id.clone(),
                });
                if !m.text.is_empty() {
                    events.push(AgentDomainEvent::ContentDelta {
                        turn_id: turn.id.clone(),
                        kind: ContentKind::Text,
                        text: m.text.clone(),
                    });
                    last_message = m.text.clone();
                }
            }
            ThreadItem::Reasoning(r) => {
                let parts = if r.summary.is_empty() { &r.content } else { &r.summary };
                let mut text = parts.join("\n\n");
                if !text.is_empty() {
                    // A reasoning item straight after another lands in the same block; the blank line
                    // between them is the one the live stream puts there (`provider::thought_text`).
                    for event in &events[thought_scanned..] {
                        thought_open = thinking_open_after(thought_open, event);
                    }
                    thought_scanned = events.len();
                    if thought_open {
                        text.insert_str(0, "\n\n");
                    }
                    events.push(AgentDomainEvent::ContentDelta {
                        turn_id: turn.id.clone(),
                        kind: ContentKind::Thinking,
                        text,
                    });
                }
            }
            other => {
                if let (Some((name, input)), Some((content, is_error)), Some(id)) =
                    (translate::tool_start(other), translate::tool_result(other), other.id())
                {
                    // Under the id the live call had, so a row never shares an id with another.
                    let row = translate::row_id(thread_id, thread_id, id);
                    events.push(AgentDomainEvent::ToolCallStarted {
                        turn_id: turn.id.clone(),
                        tool_use_id: row.clone(),
                        name: name.to_string(),
                        input,
                    });
                    events.push(AgentDomainEvent::ToolCallCompleted {
                        turn_id: turn.id.clone(),
                        tool_use_id: row,
                        content,
                        is_error,
                    });
                }
            }
        }
    }
    match complete {
        Complete::Yes => {}
        Complete::TooLarge => events.extend(note_row(&turn.id, &format!("part of this turn was {TOO_LARGE}"))),
        Complete::No | Complete::Stopped(_) => {
            events.extend(note_row(&turn.id, "part of this turn could not be loaded"))
        }
    }
    // A past turn is over whatever Codex last wrote on it: one still marked running was cut off.
    let (outcome, detail) = match turn.status {
        TurnStatus::InProgress => (TurnOutcome::Interrupted, TurnEndDetail::default()),
        _ => translate::turn_ending(&turn.status, turn.error.as_ref()),
    };
    events.push(AgentDomainEvent::TurnCompleted {
        turn_id: turn.id.clone(),
        outcome,
        result_text: last_message,
        stop_reason: None,
        usage: None,
        detail,
    });
    events
}

/// A row in turn `turn_id` that says something about the history rather than about a call.
fn note_row(turn_id: &str, note: &str) -> Vec<AgentDomainEvent> {
    let id = format!("eitri-history-note-{turn_id}");
    vec![
        AgentDomainEvent::ToolCallStarted {
            turn_id: turn_id.to_string(),
            tool_use_id: id.clone(),
            name: "history".to_string(),
            input: json!({ "note": note }),
        },
        AgentDomainEvent::ToolCallCompleted {
            turn_id: turn_id.to_string(),
            tool_use_id: id,
            content: Value::String(note.to_string()),
            is_error: true,
        },
    ]
}

/// A turn of its own for a note that belongs to no past turn.
fn note_turn(note: &str) -> Vec<AgentDomainEvent> {
    let mut events = vec![AgentDomainEvent::TurnStarted {
        turn_id: NOTE_TURN.to_string(),
    }];
    events.extend(note_row(NOTE_TURN, note));
    events.push(AgentDomainEvent::TurnCompleted {
        turn_id: NOTE_TURN.to_string(),
        outcome: TurnOutcome::Completed,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
        detail: TurnEndDetail::default(),
    });
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn turn(id: &str) -> Value {
        json!({"id": id, "items": [], "itemsView": "notLoaded", "status": "completed", "error": null})
    }

    fn turns_page(ids: &[&str], next: Option<&str>) -> Value {
        json!({"data": ids.iter().map(|id| turn(id)).collect::<Vec<_>>(), "nextCursor": next})
    }

    fn items_page(turn: &str, item: &str, next: Option<&str>) -> Value {
        json!({"data": [{"turnId": turn, "item": {"type": "agentMessage", "id": item, "text": item}}],
               "nextCursor": next})
    }

    fn cursor_of(params: &Value) -> Option<String> {
        params.get("cursor").and_then(Value::as_str).map(str::to_string)
    }

    fn notes(events: &[AgentDomainEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                AgentDomainEvent::ToolCallCompleted { content, .. } => content.as_str().map(str::to_string),
                _ => None,
            })
            .collect()
    }

    fn started(events: &[AgentDomainEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                AgentDomainEvent::TurnStarted { turn_id } => Some(turn_id.clone()),
                _ => None,
            })
            .collect()
    }

    /// The same reasoning items `tests/codex_provider.rs`'s `reasoning_streams_as_it_resumes` streams live,
    /// read back as a past turn: the text is the text the live stream gave.
    #[test]
    fn reasoning_reads_as_it_streamed() {
        let turn: Turn = serde_json::from_value(turn("t1")).expect("turn");
        let reasoning = |id: &str, summary: Value, content: Value| json!({"type": "reasoning", "id": id, "summary": summary, "content": content});
        let items: Vec<ThreadItem> = [
            reasoning("r1", json!(["weigh options", "then act"]), json!([])),
            reasoning("r2", json!([]), json!(["raw one", "raw two"])),
            reasoning("r3", json!(["whole a", "whole b"]), json!([])),
            json!({"type": "agentMessage", "id": "m1", "text": "done"}),
            reasoning("r4", json!(["after"]), json!([])),
        ]
        .into_iter()
        .map(|item| serde_json::from_value(item).expect("item"))
        .collect();
        let mut projection = crate::AgentSessionProjection::default();
        for event in turn_events("th", &turn, &items, Complete::Yes) {
            projection.apply(&event);
        }
        let blocks: Vec<&str> = projection.thinking.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(
            blocks,
            [
                "weigh options\n\nthen act\n\nraw one\n\nraw two\n\nwhole a\n\nwhole b",
                "after"
            ]
        );
    }

    /// A message put into the turn while it ran is read back where it stood: after the thought and before
    /// the command it came between, marked as read inside the turn, and ending the thought before it as it
    /// did live (the next thought is a block of its own, with no separator in front). The turn's own
    /// opening message stays its prompt, before the start.
    #[test]
    fn a_message_put_into_the_turn_is_read_back_where_it_stood() {
        let turn: Turn = serde_json::from_value(turn("t1")).expect("turn");
        let user = |id: &str, text: &str| json!({"type": "userMessage", "id": id, "content": [{"type": "text", "text": text}]});
        let reasoning = |id: &str, text: &str| json!({"type": "reasoning", "id": id, "summary": [text], "content": []});
        let items: Vec<ThreadItem> = [
            user("u1", "fix the parser"),
            reasoning("r1", "look first"),
            user("u2", "and the tests"),
            reasoning("r2", "tests too"),
            json!({"type": "commandExecution", "id": "c1", "command": "cargo test", "status": "completed",
                   "aggregatedOutput": "ok", "exitCode": 0}),
            json!({"type": "agentMessage", "id": "m1", "text": "done"}),
        ]
        .into_iter()
        .map(|item| serde_json::from_value(item).expect("item"))
        .collect();
        let events = turn_events("th", &turn, &items, Complete::Yes);
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| match e {
                AgentDomainEvent::UserPromptSubmitted { .. } => "prompt",
                AgentDomainEvent::TurnStarted { .. } => "start",
                AgentDomainEvent::ContentDelta {
                    kind: ContentKind::Thinking,
                    ..
                } => "thought",
                AgentDomainEvent::UserMessageDelivered { .. } => "delivered",
                AgentDomainEvent::ToolCallStarted { .. } => "call",
                _ => "other",
            })
            .filter(|k| *k != "other")
            .collect();
        assert_eq!(kinds, ["prompt", "start", "thought", "delivered", "thought", "call"]);
        let ids: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                AgentDomainEvent::UserMessageDelivered { client_message_id, .. } => Some(client_message_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, ["eitri-history-message-u2"], "never an id handed out live");

        let mut projection = crate::AgentSessionProjection::default();
        for event in &events {
            projection.apply(event);
        }
        let prompts: Vec<(&str, bool)> = projection
            .user_prompts
            .iter()
            .map(|p| (p.text.as_str(), p.delivered))
            .collect();
        assert_eq!(prompts, [("fix the parser", false), ("and the tests", true)]);
        let blocks: Vec<&str> = projection.thinking.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(blocks, ["look first", "tests too"]);
        let delivered = projection.user_prompts[1].seq;
        assert!(projection.thinking[0].seq < delivered && delivered < projection.thinking[1].seq);
        assert!(delivered < projection.tool_calls[0].seq);
        assert_eq!(projection.transcript.len(), 1);
    }

    /// A turn that opens with something other than a message (one the agent opened itself) has no prompt:
    /// a message in it was put into it while it ran.
    #[test]
    fn a_message_after_the_turns_first_item_is_never_its_prompt() {
        let turn: Turn = serde_json::from_value(turn("t1")).expect("turn");
        let items: Vec<ThreadItem> = [
            json!({"type": "agentMessage", "id": "m1", "text": "on it"}),
            json!({"type": "userMessage", "id": "u1", "content": [{"type": "text", "text": "stop"}]}),
        ]
        .into_iter()
        .map(|item| serde_json::from_value(item).expect("item"))
        .collect();
        let events = turn_events("th", &turn, &items, Complete::Yes);
        assert!(matches!(events[0], AgentDomainEvent::TurnStarted { .. }));
        assert!(!events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::UserPromptSubmitted { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::UserMessageDelivered { text, .. } if text == "stop")));
    }

    /// Item cursors that go round in a circle end the turn's read instead of asking for ever.
    #[test]
    fn an_item_cursor_that_comes_back_ends_the_turn() {
        let mut item_asks = 0;
        let mut ask = |name: &str, params: Value| -> Result<Value, AskError> {
            if name == method::THREAD_TURNS_LIST {
                return Ok(turns_page(&["t1"], None));
            }
            item_asks += 1;
            if item_asks > 50 {
                return Err(LinkError::Closed.into());
            }
            let next = match cursor_of(&params).as_deref() {
                None | Some("B") => "A",
                _ => "B",
            };
            Ok(items_page("t1", &format!("i{item_asks}"), Some(next)))
        };
        let events = page_history("th", Instant::now() + Duration::from_secs(30), &mut ask);
        assert!(item_asks < 5, "{item_asks} item pages were asked for");
        assert_eq!(started(&events), vec!["t1"]);
    }

    /// A page that times out ends the whole history: the next turns are not asked about one by one.
    #[test]
    fn a_timed_out_page_ends_the_history() {
        let ids: Vec<String> = (0..10).map(|n| format!("t{n}")).collect();
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        let mut item_asks = 0;
        let mut ask = |name: &str, _: Value| -> Result<Value, AskError> {
            if name == method::THREAD_TURNS_LIST {
                return Ok(turns_page(&ids, None));
            }
            item_asks += 1;
            Err(LinkError::Timeout.into())
        };
        let events = page_history("th", Instant::now() + Duration::from_secs(30), &mut ask);
        assert_eq!(item_asks, 1);
        assert!(
            notes(&events).iter().any(|n| n.contains("took too long")),
            "{:#?}",
            notes(&events)
        );
    }

    #[test]
    fn a_timed_out_turn_page_keeps_newest_turns_after_the_note() {
        let mut turn_asks = 0;
        let mut directions = Vec::new();
        let mut ask = |name: &str, params: Value| -> Result<Value, AskError> {
            if name == method::THREAD_TURNS_LIST {
                turn_asks += 1;
                let direction = params["sortDirection"].as_str().expect("direction");
                directions.push(direction.to_string());
                return match (cursor_of(&params).as_deref(), direction) {
                    (None, "desc") => Ok(turns_page(&["t6", "t5"], Some("P"))),
                    (Some("P"), "desc") => Ok(turns_page(&["t4", "t3"], Some("Q"))),
                    (None, "asc") => Ok(turns_page(&["t1", "t2"], Some("P"))),
                    (Some("P"), "asc") => Ok(turns_page(&["t3", "t4"], Some("Q"))),
                    (Some("Q"), _) => Err(LinkError::Timeout.into()),
                    other => panic!("unexpected turn page: {other:?}"),
                };
            }
            assert_eq!(name, method::THREAD_ITEMS_LIST);
            let turn = params["turnId"].as_str().expect("turn id");
            assert!(params.get("sortDirection").is_none());
            Ok(match cursor_of(&params).as_deref() {
                None => items_page(turn, &format!("{turn}-first"), Some("items-2")),
                Some("items-2") => items_page(turn, &format!("{turn}-second"), None),
                other => panic!("unexpected item page: {other:?}"),
            })
        };
        let events = page_history("th", Instant::now() + Duration::from_secs(30), &mut ask);
        assert_eq!(turn_asks, 3);
        assert_eq!(started(&events), [NOTE_TURN, "t3", "t4", "t5", "t6"]);
        assert_eq!(directions, ["desc", "desc", "desc"]);
        assert!(notes(&events)[0].contains("took too long"));
        let text: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                AgentDomainEvent::ContentDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            text,
            [
                "t3-first",
                "t3-second",
                "t4-first",
                "t4-second",
                "t5-first",
                "t5-second",
                "t6-first",
                "t6-second",
            ]
        );
    }

    /// A list of turns whose pages go round in a circle, or repeat a turn, shows each turn once.
    #[test]
    fn a_turn_listed_twice_is_shown_once() {
        let mut turn_asks = 0;
        let mut ask = |name: &str, params: Value| -> Result<Value, AskError> {
            if name == method::THREAD_ITEMS_LIST {
                return Ok(json!({"data": [], "nextCursor": null}));
            }
            turn_asks += 1;
            if turn_asks > 50 {
                return Err(LinkError::Closed.into());
            }
            Ok(match cursor_of(&params).as_deref() {
                None => turns_page(&["t3", "t2"], Some("P")),
                Some("P") => turns_page(&["t2", "t1"], Some("Q")),
                _ => turns_page(&["t1"], Some("P")),
            })
        };
        let events = page_history("th", Instant::now() + Duration::from_secs(30), &mut ask);
        assert_eq!(started(&events), vec!["t1", "t2", "t3"]);
        assert!(turn_asks <= 3, "{turn_asks} pages of turns were asked for");
    }

    /// The overall deadline is kept while a turn's items are read, not only between pages of turns. The test
    /// asserts what was read and asked, never how long it took.
    #[test]
    fn the_deadline_holds_inside_a_turns_items() {
        let deadline = Instant::now() + Duration::from_millis(200);
        let mut item_asks = 0;
        let mut ask = |name: &str, _: Value| -> Result<Value, AskError> {
            if name == method::THREAD_TURNS_LIST {
                return Ok(turns_page(&["t1"], None));
            }
            item_asks += 1;
            if item_asks > 100 {
                return Err(LinkError::Closed.into());
            }
            std::thread::sleep(Duration::from_millis(50));
            Ok(items_page(
                "t1",
                &format!("i{item_asks}"),
                Some(&format!("c{item_asks}")),
            ))
        };
        let events = page_history("th", deadline, &mut ask);
        assert!(
            notes(&events)
                .iter()
                .any(|n| n == "part of this turn could not be loaded"),
            "{:#?}",
            notes(&events)
        );
        assert!(
            !notes(&events).iter().any(|n| n.starts_with("earlier turns")),
            "the only turn is the oldest, so no turns before it are missing: {:#?}",
            notes(&events)
        );
        assert!(
            item_asks < 100,
            "the cap ended it, not the deadline: {item_asks} item asks"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentDomainEvent::ContentDelta { text, .. } if text == "i1")),
            "the first item read before the deadline is kept: {events:#?}"
        );
    }

    #[test]
    fn an_expired_deadline_stops_before_asking_for_history() {
        let mut ask = |_: &str, _: Value| -> Result<Value, AskError> {
            panic!("history requested after the deadline");
        };
        let events = page_history("th", Instant::now(), &mut ask);
        assert_eq!(started(&events), [NOTE_TURN]);
        assert_eq!(
            notes(&events),
            ["the history of this conversation could not be loaded: reading it took too long"],
            "no turn is shown, so the note is about the whole history"
        );
    }

    /// Items of a turn: a first page with a cursor and then `second`, which stands for the failure under test.
    fn items_then(second: impl Fn() -> Result<Value, AskError>) -> impl FnMut(&str, Value) -> Result<Value, AskError> {
        move |_, params| match cursor_of(&params).as_deref() {
            None => Ok(items_page(
                params["turnId"].as_str().expect("turn id"),
                "first",
                Some("more"),
            )),
            Some("more") => second(),
            other => panic!("unexpected item page: {other:?}"),
        }
    }

    /// The oldest turn is cut short while its items are read and Codex has no turns before it: the turn's own
    /// row says it is partial, and no note claims earlier turns are missing.
    #[test]
    fn a_cut_short_oldest_turn_gets_no_note_about_earlier_turns() {
        let mut items = items_then(|| Err(LinkError::Timeout.into()));
        let mut ask = |name: &str, params: Value| -> Result<Value, AskError> {
            if name == method::THREAD_TURNS_LIST {
                return Ok(turns_page(&["t2", "t1"], None));
            }
            if params["turnId"] == "t2" {
                return Ok(items_page("t2", "t2-only", None));
            }
            items(name, params)
        };
        let events = page_history("th", Instant::now() + Duration::from_secs(30), &mut ask);
        assert_eq!(started(&events), ["t1", "t2"]);
        assert_eq!(notes(&events), ["part of this turn could not be loaded"]);
    }

    /// The same cut with more turns behind it (a cursor): they are missing, and the note says so.
    #[test]
    fn a_cut_short_turn_with_older_turns_behind_it_gets_the_note() {
        let mut items = items_then(|| Err(LinkError::Timeout.into()));
        let mut ask = |name: &str, params: Value| -> Result<Value, AskError> {
            if name == method::THREAD_TURNS_LIST {
                return Ok(turns_page(&["t2", "t1"], Some("P")));
            }
            items(name, params)
        };
        let events = page_history("th", Instant::now() + Duration::from_secs(30), &mut ask);
        assert_eq!(started(&events), [NOTE_TURN, "t2"]);
        assert_eq!(
            notes(&events),
            [
                "earlier turns of this conversation could not be loaded: reading it took too long",
                "part of this turn could not be loaded"
            ]
        );
    }

    /// A page of turns after the first fails with turns already kept: the note is about the turns before
    /// them, not the whole conversation, whether the body does not parse or the link fails otherwise.
    #[test]
    fn a_failing_later_turn_page_says_earlier_turns_are_missing() {
        type Failure = fn() -> Result<Value, AskError>;
        let failures: [(Failure, &str); 2] = [
            (|| Ok(json!({"data": "not a list"})), "could not be read"),
            (
                || {
                    Err(LinkError::Rpc {
                        code: -1,
                        message: "no".into(),
                    }
                    .into())
                },
                "could not be loaded",
            ),
        ];
        for (failure, wording) in failures {
            let mut ask = |name: &str, params: Value| -> Result<Value, AskError> {
                if name == method::THREAD_ITEMS_LIST {
                    return Ok(items_page("t2", "kept", None));
                }
                match cursor_of(&params).as_deref() {
                    None => Ok(turns_page(&["t2"], Some("P"))),
                    _ => failure(),
                }
            };
            let events = page_history("th", Instant::now() + Duration::from_secs(30), &mut ask);
            assert_eq!(started(&events), [NOTE_TURN, "t2"]);
            assert_eq!(
                notes(&events),
                [format!("earlier turns of this conversation {wording}")]
            );
        }
    }

    /// The first page of turns fails: nothing of the conversation is shown, and the note says the history.
    #[test]
    fn a_failing_first_turn_page_says_the_history_is_missing() {
        let mut ask = |_: &str, _: Value| -> Result<Value, AskError> { Ok(json!({"data": "not a list"})) };
        let events = page_history("th", Instant::now() + Duration::from_secs(30), &mut ask);
        assert_eq!(notes(&events), ["the history of this conversation could not be read"]);
    }

    /// The deadline passes between two turns: the next older turn was never read, so it is not shown as an
    /// empty turn marked partial; only the note stands for it.
    #[test]
    fn a_turn_the_deadline_reached_before_its_items_is_left_out() {
        let deadline = Instant::now() + Duration::from_millis(100);
        let mut ask = |name: &str, _: Value| -> Result<Value, AskError> {
            if name == method::THREAD_TURNS_LIST {
                return Ok(turns_page(&["t2", "t1"], None));
            }
            // Answering takes longer than the budget left.
            std::thread::sleep(Duration::from_millis(250));
            Ok(items_page("t2", "t2-only", None))
        };
        let events = page_history("th", deadline, &mut ask);
        assert_eq!(started(&events), [NOTE_TURN, "t2"]);
        assert_eq!(
            notes(&events),
            ["earlier turns of this conversation could not be loaded: reading it took too long"]
        );
    }

    #[test]
    fn an_empty_turn_page_ends_history_even_with_a_cursor() {
        let mut turn_asks = 0;
        let mut ask = |name: &str, _: Value| -> Result<Value, AskError> {
            assert_eq!(name, method::THREAD_TURNS_LIST);
            turn_asks += 1;
            assert_eq!(turn_asks, 1);
            Ok(turns_page(&[], Some("P")))
        };
        let events = page_history("th", Instant::now() + Duration::from_secs(30), &mut ask);
        assert_eq!(turn_asks, 1);
        assert!(events.is_empty());
    }

    /// A turn with more pages of items than are read at most is shown in part, with a note.
    #[test]
    fn a_turn_with_too_many_pages_is_shown_in_part() {
        let mut item_asks = 0;
        let mut ask = |name: &str, _: Value| -> Result<Value, AskError> {
            if name == method::THREAD_TURNS_LIST {
                return Ok(turns_page(&["t2", "t1"], None));
            }
            item_asks += 1;
            if item_asks > 5000 {
                return Err(LinkError::Closed.into());
            }
            Ok(items_page(
                "t1",
                &format!("i{item_asks}"),
                Some(&format!("c{item_asks}")),
            ))
        };
        let events = page_history("th", Instant::now() + Duration::from_secs(30), &mut ask);
        assert!(item_asks <= 2 * 200 + 1, "{item_asks} item pages were asked for");
        assert_eq!(started(&events), vec!["t1", "t2"]);
        assert!(
            notes(&events).iter().any(|n| n.contains("could not be loaded")),
            "{:#?}",
            notes(&events)
        );
    }
}
