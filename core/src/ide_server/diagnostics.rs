//! The shape and the JSON of a `getDiagnostics` answer.

use std::collections::HashMap;

use crate::editor_tools::tools::diagnostics::{DiagnosticEntry, Severity};
use serde_json::{json, Value};

const ENTRY_LIMIT: usize = 500;
const TEXT_LIMIT: usize = 4 * 1024 * 1024;

/// The CLI's severity spelling.
pub const fn severity_word(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "Error",
        Severity::Warning => "Warning",
        Severity::Info => "Information",
        Severity::Hint => "Hint",
    }
}

/// Groups entries under each URI's first appearance, preserving their incoming severity order.
/// Lines become zero-based; the chunk's zero-based UTF-16 characters are kept unchanged.
/// Missing source and code are omitted. The answer keeps a prefix of at most 500 entries and 4 MiB.
pub fn shape(entries: &[(String, DiagnosticEntry)]) -> String {
    let mut groups: Vec<Value> = Vec::new();
    let mut indices: HashMap<&str, usize> = HashMap::new();
    let mut bytes = 2; // The outer array's brackets.
    for (uri, entry) in entries.iter().take(ENTRY_LIMIT) {
        let mut diagnostic = json!({
            "message": entry.message,
            "severity": severity_word(entry.severity),
            "range": {
                "start": {"line": entry.line.saturating_sub(1), "character": entry.start_character},
                "end": {"line": entry.end_line.saturating_sub(1), "character": entry.end_character}
            }
        });
        if let Some(source) = &entry.source {
            diagnostic["source"] = json!(source);
        }
        if let Some(code) = &entry.code {
            diagnostic["code"] = json!(code);
        }
        let index = indices.get(uri.as_str()).copied();
        let new_group = index.is_none().then(|| json!({"uri": uri, "diagnostics": []}));
        // Count the escaped JSON, including each new group's wrapper and the array separators.
        let added = diagnostic.to_string().len()
            + new_group
                .as_ref()
                .map_or(1, |group| group.to_string().len() + usize::from(!groups.is_empty()));
        if added > TEXT_LIMIT - bytes {
            break;
        }
        bytes += added;
        let index = index.unwrap_or_else(|| {
            let index = groups.len();
            groups.push(new_group.unwrap());
            indices.insert(uri, index);
            index
        });
        groups[index]["diagnostics"].as_array_mut().unwrap().push(diagnostic);
    }
    let text = serde_json::to_string(&groups).unwrap();
    debug_assert_eq!(text.len(), bytes);
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_tools::tools::diagnostics::{DiagnosticEntry, Severity};
    use serde_json::{json, Value};

    fn entry(severity: Severity) -> DiagnosticEntry {
        DiagnosticEntry {
            path: "/p/a.py".into(),
            line: 3,
            column: 9,
            end_line: 4,
            end_column: 12,
            severity,
            source: None,
            code: None,
            message: "untrusted <b>text</b>\u{202e}".into(),
            start_character: Some(6),
            end_character: Some(8),
        }
    }

    fn read(entries: &[(String, DiagnosticEntry)]) -> Value {
        serde_json::from_str(&shape(entries)).unwrap()
    }

    #[test]
    fn the_json_shape() {
        let mut first = entry(Severity::Error);
        first.source = Some("checker".into());
        first.code = Some("E1".into());
        let other = entry(Severity::Warning);
        let entries = vec![
            ("file:///p/b.py".into(), first.clone()),
            ("file:///p/a.py".into(), other.clone()),
            ("file:///p/b.py".into(), other.clone()),
        ];
        let diagnostic = |d: &DiagnosticEntry| {
            let mut value = json!({
                "message": d.message, "severity": severity_word(d.severity),
                "range": {"start": {"line": 2, "character": 6}, "end": {"line": 3, "character": 8}}
            });
            if let Some(source) = &d.source {
                value["source"] = json!(source);
            }
            if let Some(code) = &d.code {
                value["code"] = json!(code);
            }
            value
        };
        assert_eq!(
            read(&entries),
            json!([
                {"uri": "file:///p/b.py", "diagnostics": [diagnostic(&first), diagnostic(&other)]},
                {"uri": "file:///p/a.py", "diagnostics": [diagnostic(&other)]}
            ])
        );
        assert_eq!(shape(&[]), "[]");
    }

    #[test]
    fn lines_are_zero_based() {
        let mut d = entry(Severity::Info);
        d.line = 1;
        d.end_line = 7;
        let answer = read(&[("file:///p/a.py".into(), d)]);
        assert_eq!(
            answer[0]["diagnostics"][0]["range"],
            json!({
                "start": {"line": 0, "character": 6}, "end": {"line": 6, "character": 8}
            })
        );
    }

    #[test]
    fn severity_words() {
        assert_eq!(severity_word(Severity::Error), "Error");
        assert_eq!(severity_word(Severity::Warning), "Warning");
        assert_eq!(severity_word(Severity::Info), "Information");
        assert_eq!(severity_word(Severity::Hint), "Hint");
    }

    #[test]
    fn the_most_severe_stay_when_it_is_cut() {
        let mut entries = vec![("file:///p/a.py".into(), entry(Severity::Error)); 500];
        entries.push(("file:///p/b.py".into(), entry(Severity::Hint)));
        let answer = read(&entries);
        assert_eq!(answer.as_array().unwrap().len(), 1);
        let diagnostics = answer[0]["diagnostics"].as_array().unwrap();
        assert_eq!(diagnostics.len(), 500);
        assert!(diagnostics.iter().all(|d| d["severity"] == "Error"));
    }

    #[test]
    fn a_text_over_4_mib_loses_its_last_entries() {
        let mut entries = Vec::new();
        for i in 0..5 {
            let mut d = entry(Severity::Error);
            d.message = "\0".repeat(180_000);
            entries.push((format!("file:///p/{i}.py"), d));
        }
        let text = shape(&entries);
        assert!(text.len() <= 4 * 1024 * 1024);
        assert_eq!(read(&entries), read(&entries[..3]));
        let mut too_big = entry(Severity::Error);
        too_big.message = "x".repeat(4 * 1024 * 1024);
        assert_eq!(shape(&[("file:///p/a.py".into(), too_big)]), "[]");
    }

    #[test]
    fn a_uri_is_written_as_given() {
        let uri = "file:///p/a b%20c.py";
        let answer = read(&[(uri.into(), entry(Severity::Hint))]);
        assert_eq!(answer[0]["uri"], uri);
    }
}
