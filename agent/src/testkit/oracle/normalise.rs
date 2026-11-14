//! What counts as the same, and what each Eitri record can hold.
//!
//! - Text: CRLF becomes LF and trailing spaces are trimmed, per line and at the end. On user text, the
//!   editor block Eitri composes into a prompt is stripped first (`history::strip_composed_block`), on both
//!   sides, since the CLI stores the composed text.
//! - Unicode normalisation is not applied: both sides carry the bytes the CLI and the model produced, and
//!   no Unicode normalisation crate is in this crate's dependencies.
//! - Tool inputs compare as canonical JSON (keys sorted at every level).
//! - Tool results compare as their text: a string as is, text blocks joined by newlines, any other block as
//!   `[<type>]`.
//! - Adjacent assistant text in one turn is one entry, and so is adjacent thinking: Eitri coalesces deltas,
//!   the CLI writes one line per block. The reader has already put the paragraph separator in front of a
//!   text block that follows a text block of the same message, as Eitri's records do. A record may also keep two messages of one turn as two rows; the
//!   alignment joins those where the CLI holds the joined text.
//! - A question set compares by its key alone ([`super::QuestionKey`]): how many questions, which carry an
//!   answer, and how it ended. Eitri's own ends map onto the oracle's words in [`question_end`]: an interrupt,
//!   an expiry and a closed session are all `cancelled`, a host that could not show the form and a failed
//!   provider are `other`. A set still waiting has no key: the CLI writes no result for it either.
//! - A plan compares by its key alone ([`super::PlanKey`]): the digest of its text and how it ended. Eitri's own
//!   ends map onto the oracle's words in [`plan_end`]: an approval and a keep are their own, every other end
//!   (an interrupt, an expiry, a closed session, a plan the client could not carry) is `other`. A plan still
//!   waiting has no key: the CLI writes no result for it either.
//! - The CLI form is projected onto what each record holds before it is compared: A and B store no
//!   thinking and no turn endings, and B does not open subagent transcripts. These are differences of
//!   kind, counted by [`project`], never compared entry by entry.

use super::{Entry, PlanEnd, PlanKey, QuestionEnd, QuestionKey, Source, Turn};
use crate::history::strip_composed_block;
use crate::projection::{PlanOutcome, PlanRecord, PlanState, QuestionOutcome, QuestionRecord, QuestionState};
use serde_json::Value;
use std::collections::BTreeMap;

/// `text` with CRLF as LF and trailing spaces trimmed from each line and from the end.
pub fn text(raw: &str) -> String {
    let unified = raw.replace("\r\n", "\n");
    let lines: Vec<&str> = unified
        .split('\n')
        .map(|line| line.trim_end_matches([' ', '\t']))
        .collect();
    lines.join("\n").trim_end().to_owned()
}

/// A user's words, without the editor block Eitri composes into a prompt.
pub fn user_text(raw: &str) -> String {
    text(strip_composed_block(raw))
}

/// JSON with object keys sorted at every level, whatever map order the parser kept.
pub fn canonical_json(value: &Value) -> String {
    fn write(value: &Value, out: &mut String) {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                out.push('{');
                for (i, key) in keys.into_iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&Value::String(key.clone()).to_string());
                    out.push(':');
                    write(&map[key], out);
                }
                out.push('}');
            }
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write(item, out);
                }
                out.push(']');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut out = String::new();
    write(value, &mut out);
    out
}

/// A tool result's content flattened to text.
pub fn result_text(content: &Value) -> String {
    match content {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") => block.get("text").and_then(Value::as_str).unwrap_or_default().to_owned(),
                Some(kind) => format!("[{kind}]"),
                None => canonical_json(block),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => canonical_json(other),
    }
}

/// An end Eitri records, in the oracle's words.
pub fn question_end(outcome: QuestionOutcome) -> QuestionEnd {
    match outcome {
        QuestionOutcome::Answered => QuestionEnd::Answered,
        QuestionOutcome::Declined => QuestionEnd::Declined,
        QuestionOutcome::ChatRequested => QuestionEnd::Chat,
        QuestionOutcome::Interrupted | QuestionOutcome::Expired | QuestionOutcome::SessionClosed => {
            QuestionEnd::Cancelled
        }
        QuestionOutcome::ProviderFailed | QuestionOutcome::HostRefused => QuestionEnd::Other,
    }
}

/// A question record of Eitri's as an entry, once it has settled; `None` while it waits.
pub fn question_entry(record: &QuestionRecord) -> Option<Entry> {
    let QuestionState::Settled { outcome, answers, .. } = &record.state else {
        return None;
    };
    let answered = answers
        .iter()
        .filter_map(|answer| usize::try_from(answer.question_index).ok())
        .collect();
    Some(Entry::Question {
        id: record
            .tool_use_id
            .clone()
            .unwrap_or_else(|| record.interaction_id.clone()),
        key: QuestionKey::new(record.questions.len(), answered, question_end(*outcome)),
    })
}

/// An end Eitri records for a plan, in the oracle's words.
pub fn plan_end(outcome: PlanOutcome) -> PlanEnd {
    match outcome {
        PlanOutcome::Approved => PlanEnd::Approved,
        PlanOutcome::KeptPlanning => PlanEnd::KeptPlanning,
        PlanOutcome::Interrupted
        | PlanOutcome::Expired
        | PlanOutcome::SessionClosed
        | PlanOutcome::ProviderFailed
        | PlanOutcome::HostRefused => PlanEnd::Other,
    }
}

/// A plan record of Eitri's as an entry, once it has settled; `None` while it waits.
pub fn plan_entry(record: &PlanRecord) -> Option<Entry> {
    let PlanState::Settled { outcome, .. } = &record.state else {
        return None;
    };
    Some(Entry::Plan {
        id: record
            .tool_use_id
            .clone()
            .unwrap_or_else(|| record.interaction_id.clone()),
        key: PlanKey {
            plan_sha256: record.plan_sha256.clone(),
            end: plan_end(*outcome),
        },
    })
}

/// Joins adjacent assistant text, and adjacent thinking, within each turn (and in nested subagent turns).
pub fn join_adjacent(turns: &mut [Turn]) {
    join_counted(turns);
}

/// [`join_adjacent`], saying per turn and entry how many entries each joined entry holds.
fn join_counted(turns: &mut [Turn]) -> Vec<Vec<usize>> {
    let mut counts = Vec::with_capacity(turns.len());
    for turn in turns.iter_mut() {
        let mut entries: Vec<Entry> = Vec::with_capacity(turn.entries.len());
        let mut positions = Vec::with_capacity(turn.positions.len());
        let mut pieces: Vec<usize> = Vec::with_capacity(turn.entries.len());
        for (entry, pos) in turn.entries.drain(..).zip(turn.positions.drain(..)) {
            match (entries.last_mut(), entry) {
                (Some(Entry::Assistant { text: last }), Entry::Assistant { text })
                | (Some(Entry::Thinking { text: last }), Entry::Thinking { text }) => {
                    last.push_str(&text);
                    *pieces.last_mut().expect("one per entry") += 1;
                }
                (
                    _,
                    Entry::Subagent {
                        parent_tool_use_id,
                        mut turns,
                    },
                ) => {
                    join_adjacent(&mut turns);
                    entries.push(Entry::Subagent {
                        parent_tool_use_id,
                        turns,
                    });
                    positions.push(pos);
                    pieces.push(1);
                }
                (_, entry) => {
                    entries.push(entry);
                    positions.push(pos);
                    pieces.push(1);
                }
            }
        }
        turn.entries = entries;
        turn.positions = positions;
        counts.push(pieces);
    }
    counts
}

/// Whether a record can hold an entry of this kind at all.
pub fn holds(source: Source, entry: &Entry) -> bool {
    match entry {
        // A stores no thinking, and B drops it on purpose.
        Entry::Thinking { .. } => source == Source::E,
        // A and B have no row for the end of a turn.
        Entry::Interrupted | Entry::Ended { .. } => source == Source::E,
        // B does not open subagent transcripts, as the CLI's own resume does not show them either.
        Entry::Subagent { .. } => source != Source::B,
        _ => true,
    }
}

/// The CLI form as `source` could hold it, with adjacent text joined, and how many entries of each kind
/// were left out.
pub fn project(turns: &[Turn], source: Source) -> (Vec<Turn>, BTreeMap<&'static str, usize>) {
    let (out, removed, _) = project_counted(turns, source);
    (out, removed)
}

/// [`project`], also saying per turn and entry how many of the CLI's entries each joined entry holds.
pub fn project_counted(turns: &[Turn], source: Source) -> (Vec<Turn>, BTreeMap<&'static str, usize>, Vec<Vec<usize>>) {
    let mut removed: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut out = project_into(turns, source, &mut removed);
    let pieces = join_counted(&mut out);
    (out, removed, pieces)
}

fn project_into(turns: &[Turn], source: Source, removed: &mut BTreeMap<&'static str, usize>) -> Vec<Turn> {
    turns
        .iter()
        .map(|turn| {
            let mut kept = Turn::new(turn.index, turn.origin.clone());
            for (entry, pos) in turn.entries.iter().zip(&turn.positions) {
                if !holds(source, entry) {
                    *removed.entry(entry.kind()).or_default() += 1;
                    continue;
                }
                let entry = match entry {
                    Entry::Subagent {
                        parent_tool_use_id,
                        turns,
                    } => Entry::Subagent {
                        parent_tool_use_id: parent_tool_use_id.clone(),
                        turns: project_into(turns, source, removed),
                    },
                    other => other.clone(),
                };
                kept.push(entry, *pos);
            }
            kept
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(state: QuestionState, tool_use_id: Option<&str>) -> QuestionRecord {
        QuestionRecord {
            seq: 1,
            interaction_id: "interaction".into(),
            tool_use_id: tool_use_id.map(str::to_owned),
            turn_id: None,
            questions: vec![
                crate::projection::QuestionSpec {
                    question: "First?".into(),
                    header: String::new(),
                    multi_select: false,
                    options: Vec::new(),
                };
                2
            ],
            state,
            unsupported_reason: None,
        }
    }

    fn settled(outcome: QuestionOutcome, indexes: &[u32]) -> QuestionState {
        QuestionState::Settled {
            outcome,
            answers: indexes
                .iter()
                .map(|&question_index| crate::projection::ShownAnswer {
                    question_index,
                    answer: "words".into(),
                    notes: None,
                })
                .collect(),
            reason: None,
        }
    }

    #[test]
    fn every_end_eitri_records_maps_to_one_word_of_the_oracle_s_list() {
        use QuestionOutcome::*;
        let words: Vec<(QuestionOutcome, &str)> = [
            Answered,
            Declined,
            ChatRequested,
            Interrupted,
            Expired,
            SessionClosed,
            ProviderFailed,
            HostRefused,
        ]
        .into_iter()
        .map(|outcome| (outcome, question_end(outcome).word()))
        .collect();
        assert_eq!(
            words,
            vec![
                (Answered, "answered"),
                (Declined, "declined"),
                (ChatRequested, "chat"),
                (Interrupted, "cancelled"),
                (Expired, "cancelled"),
                (SessionClosed, "cancelled"),
                (ProviderFailed, "other"),
                (HostRefused, "other"),
            ]
        );
    }

    fn plan_record(state: PlanState, tool_use_id: Option<&str>) -> PlanRecord {
        PlanRecord {
            seq: 1,
            interaction_id: "interaction".into(),
            tool_use_id: tool_use_id.map(str::to_owned),
            turn_id: None,
            plan: "# Plan\n".into(),
            plan_sha256: "abc123".into(),
            allowed_prompts: Vec::new(),
            state,
            unsupported_reason: None,
        }
    }

    fn plan_settled(outcome: PlanOutcome) -> PlanState {
        PlanState::Settled {
            outcome,
            mode_after: None,
            feedback: Some("words".into()),
            reason: Some("words".into()),
        }
    }

    #[test]
    fn a_plan_is_its_digest_and_one_word_of_the_oracle_s_list() {
        use PlanOutcome::*;
        let words: Vec<_> = [
            Approved,
            KeptPlanning,
            Interrupted,
            Expired,
            SessionClosed,
            ProviderFailed,
            HostRefused,
        ]
        .into_iter()
        .map(|outcome| plan_end(outcome).word())
        .collect();
        assert_eq!(
            words,
            ["approved", "kept_planning", "other", "other", "other", "other", "other"]
        );
        assert_eq!(
            plan_entry(&plan_record(plan_settled(Approved), Some("toolu"))),
            Some(Entry::Plan {
                id: "toolu".into(),
                key: PlanKey {
                    plan_sha256: "abc123".into(),
                    end: PlanEnd::Approved
                },
            })
        );
        let by_interaction = plan_entry(&plan_record(plan_settled(KeptPlanning), None));
        assert!(matches!(by_interaction, Some(Entry::Plan { id, .. }) if id == "interaction"));
        assert_eq!(plan_entry(&plan_record(PlanState::Waiting, Some("toolu"))), None);
        assert_eq!(plan_entry(&plan_record(PlanState::Answering, Some("toolu"))), None);
    }

    #[test]
    fn a_settled_record_is_its_key_and_a_waiting_one_is_nothing_yet() {
        assert_eq!(
            question_entry(&record(settled(QuestionOutcome::Answered, &[1, 0, 1]), Some("toolu"))),
            Some(Entry::Question {
                id: "toolu".into(),
                key: QuestionKey::new(2, vec![0, 1], QuestionEnd::Answered),
            }),
            "answered indexes are sorted and counted once"
        );
        assert_eq!(
            question_entry(&record(settled(QuestionOutcome::SessionClosed, &[]), None)),
            Some(Entry::Question {
                id: "interaction".into(),
                key: QuestionKey::new(2, Vec::new(), QuestionEnd::Cancelled),
            }),
            "a record without a tool-use id pairs by its interaction id"
        );
        assert_eq!(question_entry(&record(QuestionState::Waiting, Some("toolu"))), None);
        assert_eq!(question_entry(&record(QuestionState::Answering, Some("toolu"))), None);
    }

    #[test]
    fn text_unifies_line_endings_and_trims_trailing_spaces() {
        assert_eq!(text("a  \r\nb\t\r\n\n"), "a\nb");
        assert_eq!(text("  lead kept"), "  lead kept");
    }

    #[test]
    fn canonical_json_sorts_keys_at_every_level() {
        let a = json!({"b": 1, "a": {"y": [1, {"d": 0, "c": 1}], "x": null}});
        assert_eq!(canonical_json(&a), r#"{"a":{"x":null,"y":[1,{"c":1,"d":0}]},"b":1}"#);
    }

    #[test]
    fn result_text_joins_text_blocks_and_names_other_blocks() {
        let content =
            json!([{"type": "text", "text": "one"}, {"type": "image", "source": {}}, {"type": "text", "text": "two"}]);
        assert_eq!(result_text(&content), "one\n[image]\ntwo");
        assert_eq!(result_text(&json!("plain")), "plain");
        assert_eq!(result_text(&Value::Null), "");
    }
}
