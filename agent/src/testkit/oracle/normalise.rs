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
//!   the CLI writes one line per block. A record may also keep two messages of one turn as two rows; the
//!   alignment joins those where the CLI holds the joined text.
//! - The CLI form is projected onto what each record holds before it is compared: A and B store no
//!   thinking and no turn endings, and B does not open subagent transcripts. These are differences of
//!   kind, counted by [`project`], never compared entry by entry.

use super::{Entry, Source, Turn};
use crate::history::strip_composed_block;
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
