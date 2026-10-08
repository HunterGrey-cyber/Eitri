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
use crate::projection::{AgentDomainEvent, ContentKind, TurnEndDetail, TurnOutcome};
use serde_json::{json, Value};
use std::time::Instant;

/// The turn id the notes about the history as a whole sit in.
const NOTE_TURN: &str = "eitri-history";

/// What a note about missing history says when a page was larger than the link reads.
const TOO_LARGE: &str = "too large to show";

/// One call on the link: the method, its params, and the answer.
pub type Ask<'a> = dyn FnMut(&str, Value) -> Result<Value, LinkError> + 'a;

/// The events of a thread's history, oldest turn first. Anything that cannot be read becomes a note row in
/// the turn it belongs to, or in a turn of its own when the list of turns is what failed. Stops at
/// `deadline` with a note, since a very long thread must not hold the start for ever.
pub fn page_history(thread_id: &str, deadline: Instant, ask: &mut Ask<'_>) -> Vec<AgentDomainEvent> {
    let mut events = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        if Instant::now() >= deadline {
            events.extend(note_turn(
                "the rest of this conversation was not loaded: reading it took too long",
            ));
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
                    events.extend(note_turn("the history of this conversation could not be read"));
                    break;
                }
            },
            Err(LinkError::TooLarge) => {
                eprintln!("agent: Codex's list of turns was larger than 16 MiB; skipped");
                events.extend(note_turn(&format!(
                    "a page of this conversation's history was {TOO_LARGE}"
                )));
                break;
            }
            // The link is gone; the pump reports why.
            Err(LinkError::Closed) => break,
            Err(other) => {
                eprintln!("agent: Codex's list of turns failed: {other}");
                events.extend(note_turn("the history of this conversation could not be loaded"));
                break;
            }
        };
        for turn in &page.data {
            let (items, complete) = load_items(thread_id, turn, ask);
            events.extend(turn_events(thread_id, turn, &items, complete));
        }
        match page.next_cursor {
            // A cursor that does not move would ask for the same page for ever.
            Some(next) if cursor.as_deref() != Some(next.as_str()) && !page.data.is_empty() => cursor = Some(next),
            _ => break,
        }
    }
    events
}

/// How a turn's items were read: what arrived, and whether every page did.
fn load_items(thread_id: &str, turn: &Turn, ask: &mut Ask<'_>) -> (Vec<ThreadItem>, Complete) {
    let mut items = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
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
            Err(LinkError::TooLarge) => {
                eprintln!("agent: a page of a turn's items from Codex was larger than 16 MiB; skipped");
                return (items, Complete::TooLarge);
            }
            Err(other) => {
                eprintln!("agent: Codex's items of a turn failed: {other}");
                return (items, Complete::No);
            }
        };
        let empty = page.data.is_empty();
        items.extend(page.data.into_iter().filter(|e| e.turn_id == turn.id).map(|e| e.item));
        match page.next_cursor {
            Some(next) if cursor.as_deref() != Some(next.as_str()) && !empty => cursor = Some(next),
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
    /// A page could not be had for another reason.
    No,
}

/// The text of a user message's parts.
fn user_text(content: &[Value]) -> String {
    content
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The events of one past turn: the prompt, the start, what it said and ran, a note for what could not be
/// read, and the end.
fn turn_events(thread_id: &str, turn: &Turn, items: &[ThreadItem], complete: Complete) -> Vec<AgentDomainEvent> {
    let mut events = Vec::new();
    for item in items {
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
    for item in items {
        match item {
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
                let text = parts.join("\n\n");
                if !text.is_empty() {
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
        Complete::No => events.extend(note_row(&turn.id, "part of this turn could not be loaded")),
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
