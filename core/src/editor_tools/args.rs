//! Reading a call's arguments. Nothing upstream checks them against the declared schema, so these are the
//! only check there is: each reader takes one argument by name and by type, and its error is the sentence
//! the agent gets. A JSON `null` in an optional argument counts as leaving it out.

use serde_json::Value;

use super::step::ToolError;
use super::tools::diagnostics::Severity;
use super::tools::lsp::UsagePart;

/// The most a symbol may be: it is searched for on one line, and no language's name comes near this.
const SYMBOL_MAX: usize = 256;

fn given<'a>(args: &'a Value, key: &str) -> Option<&'a Value> {
    args.get(key).filter(|value| !value.is_null())
}

/// A required path, as the agent wrote it. The path's meaning is `paths::judge`'s.
pub fn path<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    optional_path(args, key)?.ok_or_else(|| ToolError(format!("{key} is required")))
}

/// An optional path, as the agent wrote it.
pub fn optional_path<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, ToolError> {
    let Some(value) = given(args, key) else {
        return Ok(None);
    };
    match value.as_str() {
        Some("") => Err(ToolError(format!("{key} may not be empty"))),
        Some(text) => Ok(Some(text)),
        None => Err(ToolError(format!("{key} must be a string"))),
    }
}

/// An optional line number, from 1.
pub fn line(args: &Value, key: &str) -> Result<Option<u32>, ToolError> {
    let Some(value) = given(args, key) else {
        return Ok(None);
    };
    let Some(number) = value.as_u64() else {
        return Err(ToolError(format!("{key} must be a whole number from 1")));
    };
    match u32::try_from(number) {
        Ok(0) => Err(ToolError(format!("{key} starts at 1"))),
        Ok(line) => Ok(Some(line)),
        Err(_) => Err(ToolError(format!("{key} is too large"))),
    }
}

/// Two optional line numbers, the second not before the first.
pub fn line_range(args: &Value, start_key: &str, end_key: &str) -> Result<(Option<u32>, Option<u32>), ToolError> {
    let start = line(args, start_key)?;
    let end = line(args, end_key)?;
    match (start, end) {
        (Some(first), Some(last)) if last < first => Err(ToolError(format!("{end_key} may not precede {start_key}"))),
        _ => Ok((start, end)),
    }
}

/// A required symbol: a non-empty name on one line.
pub fn symbol<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    let Some(value) = given(args, key) else {
        return Err(ToolError(format!("{key} is required")));
    };
    let Some(text) = value.as_str() else {
        return Err(ToolError(format!("{key} must be a string")));
    };
    if text.is_empty() {
        return Err(ToolError(format!("{key} may not be empty")));
    }
    if text.len() > SYMBOL_MAX {
        return Err(ToolError(format!("{key} is over {SYMBOL_MAX} bytes")));
    }
    if text.chars().any(char::is_control) {
        return Err(ToolError(format!("{key} may not hold a control character")));
    }
    Ok(text)
}

/// An optional list of a symbol's usage parts. Left out, or empty, is all of them.
pub fn usage_parts(args: &Value, key: &str) -> Result<Vec<UsagePart>, ToolError> {
    let Some(value) = given(args, key) else {
        return Ok(Vec::new());
    };
    let refused = || {
        ToolError(format!(
            "{key} must be a list of definitions, references, implementations"
        ))
    };
    value
        .as_array()
        .ok_or_else(refused)?
        .iter()
        .map(|part| part.as_str().and_then(UsagePart::from_word).ok_or_else(refused))
        .collect()
}

/// An optional severity word.
pub fn severity(args: &Value, key: &str) -> Result<Option<Severity>, ToolError> {
    let Some(value) = given(args, key) else {
        return Ok(None);
    };
    match value.as_str().and_then(Severity::from_word) {
        Some(severity) => Ok(Some(severity)),
        None => Err(ToolError(format!("{key} must be one of error, warning, info, hint"))),
    }
}

/// An optional `since`, whose one value is `"turn"`: whether it was given.
pub fn since(args: &Value, key: &str) -> Result<bool, ToolError> {
    match given(args, key) {
        None => Ok(false),
        Some(value) if value.as_str() == Some("turn") => Ok(true),
        Some(_) => Err(ToolError(format!("{key} must be \"turn\""))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn error<T: std::fmt::Debug>(result: Result<T, ToolError>) -> String {
        result.expect_err("refused").0
    }

    #[test]
    fn a_line_starts_at_one() {
        assert_eq!(line(&json!({"line": 1}), "line").unwrap(), Some(1));
        assert_eq!(line(&json!({}), "line").unwrap(), None);
        assert_eq!(line(&json!({"line": null}), "line").unwrap(), None);
        assert_eq!(error(line(&json!({"line": 0}), "line")), "line starts at 1");
    }

    #[test]
    fn end_line_may_not_precede_line() {
        let range = |args| line_range(&args, "line", "end_line");
        assert_eq!(range(json!({"line": 3, "end_line": 3})).unwrap(), (Some(3), Some(3)));
        assert_eq!(range(json!({"line": 3, "end_line": 9})).unwrap(), (Some(3), Some(9)));
        assert_eq!(range(json!({"end_line": 9})).unwrap(), (None, Some(9)));
        assert_eq!(range(json!({"line": 3})).unwrap(), (Some(3), None));
        assert_eq!(
            error(range(json!({"line": 3, "end_line": 2}))),
            "end_line may not precede line"
        );
    }

    #[test]
    fn a_path_must_be_a_string() {
        assert_eq!(path(&json!({"path": "a.rs"}), "path").unwrap(), "a.rs");
        assert_eq!(error(path(&json!({}), "path")), "path is required");
        assert_eq!(error(path(&json!({"path": null}), "path")), "path is required");
        assert_eq!(error(path(&json!({"path": ""}), "path")), "path may not be empty");
        for bad in [json!(1), json!(true), json!(["a.rs"]), json!({"a": 1})] {
            assert_eq!(error(path(&json!({"path": bad}), "path")), "path must be a string");
        }
        assert_eq!(optional_path(&json!({}), "path").unwrap(), None);
    }

    #[test]
    fn a_line_of_the_wrong_type_is_refused() {
        for bad in [json!("12"), json!(1.5), json!(1.0), json!(-1), json!(true), json!([1])] {
            assert_eq!(
                error(line(&json!({"line": bad}), "line")),
                "line must be a whole number from 1",
                "{bad}"
            );
        }
    }

    #[test]
    fn a_number_too_large_for_u32_is_refused() {
        let largest = u64::from(u32::MAX);
        assert_eq!(line(&json!({"line": largest}), "line").unwrap(), Some(u32::MAX));
        assert_eq!(error(line(&json!({"line": largest + 1}), "line")), "line is too large");
    }

    #[test]
    fn a_severity_is_one_of_four_words() {
        let read = |args| severity(&args, "min_severity");
        assert_eq!(
            read(json!({"min_severity": "warning"})).unwrap(),
            Some(Severity::Warning)
        );
        assert_eq!(read(json!({})).unwrap(), None);
        for bad in [json!("fatal"), json!("ERROR"), json!(1)] {
            let args = json!({ "min_severity": bad });
            assert_eq!(
                error(read(args)),
                "min_severity must be one of error, warning, info, hint"
            );
        }
    }

    #[test]
    fn since_is_turn_or_nothing() {
        assert!(!since(&json!({}), "since").unwrap());
        assert!(!since(&json!({"since": null}), "since").unwrap());
        assert!(since(&json!({"since": "turn"}), "since").unwrap());
        for bad in [
            json!("session"),
            json!("Turn"),
            json!(""),
            json!(1),
            json!(true),
            json!(["turn"]),
        ] {
            assert_eq!(
                error(since(&json!({ "since": bad }), "since")),
                "since must be \"turn\""
            );
        }
    }
}
