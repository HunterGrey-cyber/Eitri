//! The params of a `selection_changed` push.

use crate::editor_context::feed::PushFacts;
use crate::editor_context::EditorContext;
use serde_json::{json, Value};
use std::path::Path;

/// `selection_changed`'s params. `inside` is the feed's file, judged and resolved; absent outside the project
/// or when there is no file. A path that is not UTF-8 is written lossily: `json!` would panic on it, and this runs
/// on the judge thread that serves every client.
pub fn selection_params(context: Option<&EditorContext>, facts: &PushFacts, inside: Option<&Path>) -> Value {
    let (Some(context), Some(inside)) = (context, inside) else {
        return json!({
            "selection": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
            "text": ""
        });
    };
    let (start, end, text) = if let Some(selection) = &context.selection {
        let start_col = facts.start_col.unwrap_or(0);
        let end_col = facts.end_col.unwrap_or(0);
        let start = json!({"line": selection.start_line.saturating_sub(1), "character": start_col});
        // A zero-column end includes the whole last line in the CLI's line count.
        let end_line = if end_col == 0 {
            selection.end_line
        } else {
            selection.end_line.saturating_sub(1)
        };
        let end = json!({"line": end_line, "character": end_col});
        (start, end, selection.text.as_str())
    } else {
        let cursor = json!({"line": facts.cursor_line.unwrap_or(1).saturating_sub(1), "character": 0});
        (cursor.clone(), cursor, "")
    };
    json!({"selection": {"start": start, "end": end}, "text": text, "filePath": inside.to_string_lossy()})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_context::compose::Selection;
    use crate::editor_context::feed::PushFacts;
    use crate::editor_context::EditorContext;
    use serde_json::{json, Value};
    use std::path::Path;

    fn context(start_line: u32, end_line: u32, text: &str) -> EditorContext {
        EditorContext {
            file: "/project/a.rs".into(),
            selection: Some(Selection {
                start_line,
                end_line,
                text: text.into(),
            }),
        }
    }

    fn params(context: &EditorContext, start_col: Option<u32>, end_col: Option<u32>) -> Value {
        selection_params(
            Some(context),
            &PushFacts {
                cursor_line: Some(8),
                start_col,
                end_col,
            },
            Some(Path::new("/resolved/a.rs")),
        )
    }

    #[test]
    fn a_one_line_v_selection_keeps_its_columns() {
        assert_eq!(
            params(&context(3, 3, "rav"), Some(1), Some(4)),
            json!({
                "selection": {"start": {"line": 2, "character": 1}, "end": {"line": 2, "character": 4}},
                "text": "rav", "filePath": "/resolved/a.rs"
            })
        );
    }

    #[test]
    fn a_column_zero_end_moves_to_the_next_line() {
        let value = params(&context(2, 4, "a\nb\n"), Some(1), Some(0));
        assert_eq!(value["selection"]["end"], json!({"line": 4, "character": 0}));
    }

    #[test]
    fn a_line_without_columns_reads_as_linewise() {
        let value = params(&context(2, 4, "a\nb\nc"), None, None);
        assert_eq!(
            value["selection"],
            json!({"start": {"line": 1, "character": 0}, "end": {"line": 4, "character": 0}})
        );
    }

    #[test]
    fn the_cli_count_comes_out_right() {
        for (context, start_col, end_col, expected) in [
            (context(3, 3, "rav"), 1, 4, 1),
            (context(2, 4, "a\nb\nc"), 0, 0, 3),
            (context(2, 3, "a\n"), 1, 0, 2),
        ] {
            let value = params(&context, Some(start_col), Some(end_col));
            let start = &value["selection"]["start"];
            let end = &value["selection"]["end"];
            let count =
                end["line"].as_u64().unwrap() - start["line"].as_u64().unwrap() + 1 - u64::from(end["character"] == 0);
            assert_eq!(count, expected);
        }
    }

    fn empty() -> Value {
        json!({"selection": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "text": ""})
    }

    #[test]
    fn a_selection_outside_is_the_empty_form_with_no_file() {
        assert_eq!(
            selection_params(Some(&context(2, 4, "private")), &PushFacts::default(), None),
            empty()
        );
    }

    #[test]
    fn no_selection_inside_keeps_the_file() {
        let context = EditorContext {
            file: "/project/a.rs".into(),
            selection: None,
        };
        assert_eq!(
            params(&context, None, None),
            json!({
                "selection": {"start": {"line": 7, "character": 0}, "end": {"line": 7, "character": 0}},
                "text": "", "filePath": "/resolved/a.rs"
            })
        );
    }

    #[test]
    fn a_non_utf8_resolved_path_is_written_lossily() {
        use std::os::unix::ffi::OsStrExt;
        let context = EditorContext {
            file: "/project/a.rs".into(),
            selection: None,
        };
        let inside = Path::new(std::ffi::OsStr::from_bytes(b"/p/caf\xe9.rs"));
        let value = selection_params(Some(&context), &PushFacts::default(), Some(inside));
        assert_eq!(value["filePath"], json!("/p/caf\u{fffd}.rs"));
    }

    #[test]
    fn the_empty_form_names_no_outside_path() {
        let facts = PushFacts {
            cursor_line: Some(99),
            start_col: Some(4),
            end_col: Some(7),
        };
        let context = EditorContext {
            file: "/outside/secret".into(),
            selection: None,
        };
        assert_eq!(selection_params(Some(&context), &facts, None), empty());
        assert_eq!(
            selection_params(None, &facts, Some(Path::new("/resolved/a.rs"))),
            empty()
        );
        assert_eq!(selection_params(None, &facts, None), empty());
    }
}
