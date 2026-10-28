//! Eitri's three records of a conversation, read into the normal form.
//!
//! - **E**, the domain event log with the final projection: the projection's rows (prompts, messages
//!   taken into a running turn, replies, thinking, tool calls), each in the turn of the event that wrote
//!   it, and the end of a turn that did not complete. A prompt opens a turn that the next unseen turn id
//!   then names.
//! - **A**, the stored history: prompts, replies and tool calls only, ordered by `seq`. It has no turn
//!   endings and no thinking. A prompt that was not delivered into a running turn opens a turn; a tool
//!   call's turn id names the turn it belongs to, so a call whose id differs from the running turn's opens
//!   a turn of its own (one the CLI started without a prompt). A reply row stays with the turn it follows:
//!   a turn the CLI started that holds only text cannot be told apart from the turn before it.
//! - **B**, Eitri's restore reader run on the CLI's files and folded into a projection, read like A. Its
//!   tool calls carry each line's own uuid as their turn id, so its turns come from its prompts alone
//!   unless asked otherwise.

use super::{Attribution, EitriForm, Entry, Omission, Origin, Pos, Source, Turn, Via};
use crate::history::transcript_jsonl::value_chars;
use crate::history::{read_transcript_history, HistoryLoadError, StoredHistory, HISTORY_FORMAT_VERSION};
use crate::projection::{
    AgentDomainEvent, AgentSessionProjection, ToolCallRecord, TranscriptMessage, TurnOutcome, UserPromptRecord,
};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// E: the domain event log of a session and its final projection.
///
/// The rows are the projection's own (the prompts, replies, thinking and tool calls a view draws), so a
/// fold that loses or cuts a row shows as a difference. The events give each row its turn: `events` are
/// every event the projection folded, derived ones included, in order, the last one at the projection's
/// newest revision; the event folded at revision `r` wrote the rows whose `seq` is `r`. Rows a resume
/// seeded before the first event are not read here (they are the restore reader's, B). A tool result sits
/// in the turn its own completion event names, and a turn that did not complete ends with the row its
/// `TurnCompleted` gives, since the projection keeps no row for the end of a turn.
pub fn read_e(events: &[AgentDomainEvent], projection: &AgentSessionProjection) -> EitriForm {
    let mut turns: Vec<Turn> = Vec::new();
    let mut by_turn_id: HashMap<String, usize> = HashMap::new();
    let mut unclaimed: Option<usize> = None;
    let mut changed_input = BTreeMap::new();
    // The turn each event belongs to, by event index.
    let mut event_turn: Vec<Option<usize>> = vec![None; events.len()];
    // Each completed call's turn and event index, by tool-use id: the first completion counts.
    let mut completion: HashMap<&str, (usize, usize)> = HashMap::new();
    // (turn, event index, rank, entry): the entries, ordered within a turn by event index, then rank.
    let mut placed: Vec<(usize, usize, u8, Entry)> = Vec::new();

    fn turn_for(
        id: &str,
        turns: &mut Vec<Turn>,
        by_turn_id: &mut HashMap<String, usize>,
        unclaimed: &mut Option<usize>,
    ) -> usize {
        if let Some(&at) = by_turn_id.get(id) {
            return at;
        }
        let at = match unclaimed.take() {
            Some(at) => at,
            None => {
                turns.push(Turn::new(turns.len() + 1, Origin::Unknown));
                turns.len() - 1
            }
        };
        by_turn_id.insert(id.to_owned(), at);
        at
    }

    for (index, event) in events.iter().enumerate() {
        event_turn[index] = match event {
            AgentDomainEvent::UserPromptSubmitted { .. } => {
                turns.push(Turn::new(turns.len() + 1, Origin::Human));
                unclaimed = Some(turns.len() - 1);
                unclaimed
            }
            // A delta with no text writes no row, and so claims no turn.
            AgentDomainEvent::ContentDelta { text, .. } if text.is_empty() => None,
            AgentDomainEvent::TurnStarted { turn_id }
            | AgentDomainEvent::UserMessageDelivered { turn_id, .. }
            | AgentDomainEvent::ContentDelta { turn_id, .. }
            | AgentDomainEvent::ToolCallStarted { turn_id, .. } => {
                Some(turn_for(turn_id, &mut turns, &mut by_turn_id, &mut unclaimed))
            }
            AgentDomainEvent::ToolCallCompleted {
                turn_id, tool_use_id, ..
            } => {
                let at = turn_for(turn_id, &mut turns, &mut by_turn_id, &mut unclaimed);
                completion.entry(tool_use_id.as_str()).or_insert((at, index));
                Some(at)
            }
            AgentDomainEvent::TurnCompleted { turn_id, outcome, .. } => {
                let at = turn_for(turn_id, &mut turns, &mut by_turn_id, &mut unclaimed);
                let ending = match outcome {
                    TurnOutcome::Completed => None,
                    TurnOutcome::Interrupted => Some(Entry::Interrupted),
                    TurnOutcome::Failed => Some(Entry::Ended {
                        reason: super::EndReason::Other("failed".to_owned()),
                    }),
                    TurnOutcome::LimitReached => Some(Entry::Ended {
                        reason: super::EndReason::Other("limit_reached".to_owned()),
                    }),
                };
                if let Some(entry) = ending {
                    placed.push((at, index, 4, entry));
                }
                Some(at)
            }
            AgentDomainEvent::ToolInputChanged { tool_use_id, change } => {
                changed_input.insert(tool_use_id.clone(), serde_json::to_value(change).unwrap_or(Value::Null));
                None
            }
            _ => None,
        };
    }

    // The event that wrote a row, by the row's `seq`; `None` for a row seeded before the first event.
    let base = projection.last_revision.saturating_sub(events.len() as u64);
    let writer = |seq: u64| -> Option<usize> {
        let index = usize::try_from(seq.checked_sub(base)?).ok()?;
        Some(index.min(events.len().saturating_sub(1)))
    };
    // A row whose event names no turn goes to the turn of the nearest event before it that does.
    let row_turn = |index: usize, turns: &mut Vec<Turn>| -> usize {
        if let Some(at) = event_turn
            .get(..=index)
            .and_then(|before| before.iter().rev().flatten().next())
        {
            return *at;
        }
        if turns.is_empty() {
            turns.push(Turn::new(1, Origin::Unknown));
        }
        0
    };
    for prompt in &projection.user_prompts {
        let Some(index) = writer(prompt.seq) else { continue };
        let at = row_turn(index, &mut turns);
        let via = if prompt.delivered { Via::Steer } else { Via::Prompt };
        placed.push((
            at,
            index,
            0,
            Entry::User {
                text: prompt.text.clone(),
                via,
            },
        ));
    }
    for message in &projection.transcript {
        let Some(index) = writer(message.seq) else { continue };
        let at = row_turn(index, &mut turns);
        placed.push((
            at,
            index,
            1,
            Entry::Assistant {
                text: message.text.clone(),
            },
        ));
    }
    for block in &projection.thinking {
        let Some(index) = writer(block.seq) else { continue };
        let at = row_turn(index, &mut turns);
        placed.push((
            at,
            index,
            1,
            Entry::Thinking {
                text: block.text.clone(),
            },
        ));
    }
    for call in &projection.tool_calls {
        let Some(index) = writer(call.seq) else { continue };
        let at = row_turn(index, &mut turns);
        placed.push((
            at,
            index,
            2,
            Entry::ToolUse {
                id: call.tool_use_id.clone(),
                name: call.name.clone(),
                input: call.input.clone(),
            },
        ));
        if let Some(result) = &call.result {
            let (result_at, result_index) = completion
                .get(call.tool_use_id.as_str())
                .copied()
                .unwrap_or((at, index));
            placed.push((
                result_at,
                result_index,
                3,
                Entry::ToolResult {
                    id: call.tool_use_id.clone(),
                    is_error: result.is_error,
                    content: result.content.clone(),
                },
            ));
        }
        if let Some(change) = &call.changed_input {
            changed_input
                .entry(call.tool_use_id.clone())
                .or_insert_with(|| serde_json::to_value(change).unwrap_or(Value::Null));
        }
    }
    placed.sort_by_key(|(turn, index, rank, _)| (*turn, *index, *rank));
    for (at, index, _, entry) in placed {
        turns[at].push(
            entry,
            Pos {
                input: 0,
                line: index + 1,
            },
        );
    }

    let omission = match &projection.history {
        None => Omission::None,
        Some(notice) => match notice.omitted_items {
            Some(0) => Omission::None,
            other => Omission::Recorded(other),
        },
    };
    EitriForm {
        source: Source::E,
        turns,
        attribution: Attribution::Full,
        omission,
        changed_input,
    }
}

/// A: a stored history.
pub fn read_a(stored: &StoredHistory) -> EitriForm {
    let (turns, changed_input) = from_collections(&stored.user_prompts, &stored.transcript, &stored.tool_calls, true);
    let chars = stored
        .user_prompts
        .iter()
        .map(|p| p.text.chars().count())
        .sum::<usize>()
        + stored.transcript.iter().map(|m| m.text.chars().count()).sum::<usize>()
        + stored.tool_calls.iter().map(call_chars).sum::<usize>();
    EitriForm {
        source: Source::A,
        turns,
        attribution: Attribution::ToolCallsAndMidTurnMessages,
        omission: Omission::Bounds {
            items: stored.item_count(),
            chars,
        },
        changed_input,
    }
}

/// A, from its file. A file of another format version is refused, as Eitri refuses it.
pub fn read_a_file(path: &Path) -> Result<EitriForm, HistoryLoadError> {
    let text = std::fs::read_to_string(path).map_err(HistoryLoadError::Io)?;
    let value: Value = serde_json::from_str(&text).map_err(HistoryLoadError::Malformed)?;
    let found = value.get("version").and_then(Value::as_u64).unwrap_or(0);
    if found != u64::from(HISTORY_FORMAT_VERSION) {
        return Err(HistoryLoadError::UnknownVersion {
            found: u32::try_from(found).unwrap_or(u32::MAX),
            expected: HISTORY_FORMAT_VERSION,
        });
    }
    let stored: StoredHistory = serde_json::from_value(value).map_err(HistoryLoadError::Malformed)?;
    Ok(read_a(&stored))
}

/// Whether B's turns are held against the CLI's (`read_b`'s `tool_turn_ids`): not while Eitri's restore
/// reader gives each tool call its own line's uuid as its turn id, so that the calls of one turn carry
/// different ids. A test fails once the reader gives a turn's calls one id while this is still false.
pub const B_TURNS_HELD: bool = false;

/// B: Eitri's restore reader on each file of the chain, folded in order into one projection.
///
/// With `tool_turn_ids` the turns also follow the tool calls' turn ids, as for A; without, B's turns come
/// from its prompts alone and no entry's turn can be held against the CLI's.
pub fn read_b(chain: &[PathBuf], tool_turn_ids: bool) -> std::io::Result<EitriForm> {
    let mut projection = AgentSessionProjection::default();
    let mut omitted: Option<usize> = Some(0);
    for path in chain {
        let history = read_transcript_history(path)?;
        for event in &history.events {
            projection.apply(event);
        }
        omitted = match (omitted, history.omitted_items) {
            (Some(total), Some(more)) => Some(total + more),
            _ => None,
        };
    }
    let (turns, changed_input) = from_collections(
        &projection.user_prompts,
        &projection.transcript,
        &projection.tool_calls,
        tool_turn_ids,
    );
    Ok(EitriForm {
        source: Source::B,
        turns,
        attribution: if tool_turn_ids {
            Attribution::ToolCallsAndMidTurnMessages
        } else {
            Attribution::None
        },
        omission: match omitted {
            Some(0) => Omission::None,
            other => Omission::Recorded(other),
        },
        changed_input,
    })
}

/// A tool call's size by the caps' measure: its name, its input and its result.
pub fn call_chars(call: &ToolCallRecord) -> usize {
    call.name.chars().count()
        + value_chars(&call.input)
        + call.result.as_ref().map(|r| value_chars(&r.content)).unwrap_or(0)
}

fn from_collections(
    prompts: &[UserPromptRecord],
    transcript: &[TranscriptMessage],
    calls: &[ToolCallRecord],
    tool_turn_ids: bool,
) -> (Vec<Turn>, BTreeMap<String, Value>) {
    enum Item<'a> {
        Prompt(&'a UserPromptRecord),
        Message(&'a TranscriptMessage),
        Call(&'a ToolCallRecord),
    }
    // The same order the store cuts by: `seq`, then prompt before reply before call.
    let mut items: Vec<(u64, u8, usize, Item)> = Vec::new();
    items.extend(prompts.iter().enumerate().map(|(i, p)| (p.seq, 0, i, Item::Prompt(p))));
    items.extend(
        transcript
            .iter()
            .enumerate()
            .map(|(i, m)| (m.seq, 1, i, Item::Message(m))),
    );
    items.extend(calls.iter().enumerate().map(|(i, c)| (c.seq, 2, i, Item::Call(c))));
    items.sort_by_key(|(seq, rank, index, _)| (*seq, *rank, *index));

    let mut turns: Vec<Turn> = Vec::new();
    // The turn id the running turn's tool calls carry, once one has been seen.
    let mut turn_ids: Vec<Option<String>> = Vec::new();
    let mut changed_input = BTreeMap::new();
    let open = |turns: &mut Vec<Turn>, turn_ids: &mut Vec<Option<String>>, origin: Origin, id: Option<String>| {
        turns.push(Turn::new(turns.len() + 1, origin));
        turn_ids.push(id);
    };
    for (seq, _, _, item) in items {
        let pos = Pos {
            input: 0,
            line: usize::try_from(seq).unwrap_or(usize::MAX),
        };
        match item {
            Item::Prompt(prompt) if !prompt.delivered => {
                open(&mut turns, &mut turn_ids, Origin::Human, None);
                turns.last_mut().expect("just opened").push(
                    Entry::User {
                        text: prompt.text.clone(),
                        via: Via::Prompt,
                    },
                    pos,
                );
            }
            Item::Prompt(prompt) => {
                if turns.is_empty() {
                    open(&mut turns, &mut turn_ids, Origin::Unknown, None);
                }
                turns.last_mut().expect("one exists").push(
                    Entry::User {
                        text: prompt.text.clone(),
                        via: Via::Steer,
                    },
                    pos,
                );
            }
            Item::Message(message) => {
                if turns.is_empty() {
                    open(&mut turns, &mut turn_ids, Origin::Unknown, None);
                }
                turns.last_mut().expect("one exists").push(
                    Entry::Assistant {
                        text: message.text.clone(),
                    },
                    pos,
                );
            }
            Item::Call(call) => {
                let same_turn = match turn_ids.last_mut() {
                    None => false,
                    Some(_) if !tool_turn_ids => true,
                    Some(slot @ None) => {
                        *slot = Some(call.turn_id.clone());
                        true
                    }
                    Some(Some(id)) => *id == call.turn_id,
                };
                if !same_turn {
                    open(&mut turns, &mut turn_ids, Origin::Unknown, Some(call.turn_id.clone()));
                }
                let turn = turns.last_mut().expect("one exists");
                turn.push(
                    Entry::ToolUse {
                        id: call.tool_use_id.clone(),
                        name: call.name.clone(),
                        input: call.input.clone(),
                    },
                    pos,
                );
                if let Some(result) = &call.result {
                    turn.push(
                        Entry::ToolResult {
                            id: call.tool_use_id.clone(),
                            is_error: result.is_error,
                            content: result.content.clone(),
                        },
                        pos,
                    );
                }
                if let Some(change) = &call.changed_input {
                    changed_input.insert(
                        call.tool_use_id.clone(),
                        serde_json::to_value(change).unwrap_or(Value::Null),
                    );
                }
            }
        }
    }
    (turns, changed_input)
}
