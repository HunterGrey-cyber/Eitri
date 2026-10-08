//! What Codex's own project files ask it to do, as findings for the prompt: a `.codex/config.toml`
//! (its MCP servers with their approval modes, its hooks, the sandbox and approval keys Eitri
//! replaces, and every other key), a `.codex/hooks.json`, and the exec-policy rule files under
//! `.codex/rules/`. As for Claude's files, nothing is filtered by how dangerous it looks.
//!
//! Also the one key Eitri reads from the user's own Codex configuration: the alternate names of
//! `AGENTS.md`, which discovery must fingerprint in every walked directory.

use std::path::Path;

use serde_json::{Map, Value};

use super::findings::{compact, hooks, parse_object, server_line, text};
use super::nofollow::{read_regular_nofollow, Owner};
use super::Finding;

/// The most of the user's `config.toml` that is read for the alternate names.
const MAX_USER_CONFIG_BYTES: u64 = 4 << 20;

/// The key of the user's Codex configuration that names alternates for `AGENTS.md`.
const FALLBACK_NAMES_KEY: &str = "project_doc_fallback_filenames";

/// Top-level keys of a project's `config.toml` that every session Eitri starts sets itself, so the
/// project's value never applies.
const OVERRIDDEN_KEYS: [&str; 4] = [
    "sandbox_mode",
    "sandbox_workspace_write",
    "approval_policy",
    "approvals_reviewer",
];

/// Features every session Eitri starts turns off itself.
const OVERRIDDEN_FEATURES: [&str; 2] = ["exec_permission_approvals", "request_permissions_tool"];

/// The calls of an exec-policy file that let something run: each is shown as written.
const RULE_CALLS: [&str; 3] = ["prefix_rule", "network_rule", "host_executable"];

/// The alternate names of `AGENTS.md` from `<codex_home>/config.toml`: read without following a
/// link, at most 4 MiB, parsed as TOML, and only that one key kept (its string items, in order,
/// once each). Anything that keeps the key from being read -- no file, a link, a FIFO, bad TOML, a
/// value that is not a list -- gives no names. Nothing else from the file is kept or logged.
pub fn codex_fallback_names(codex_home: &Path) -> Vec<String> {
    let Ok(bytes) = read_regular_nofollow(&codex_home.join("config.toml"), MAX_USER_CONFIG_BYTES, Owner::Anyone) else {
        return Vec::new();
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Vec::new();
    };
    let Ok(mut table) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let Some(toml::Value::Array(items)) = table.remove(FALLBACK_NAMES_KEY) else {
        return Vec::new();
    };
    let mut names: Vec<String> = Vec::new();
    for item in items {
        if let toml::Value::String(name) = item {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

/// A TOML value as the JSON the prompt shows; a date-time becomes its text.
fn json_of(value: &toml::Value) -> Value {
    match value {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => Value::from(*i),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or_else(|| Value::String(f.to_string())),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(items) => Value::Array(items.iter().map(json_of).collect()),
        toml::Value::Table(table) => Value::Object(table.iter().map(|(k, v)| (k.clone(), json_of(v))).collect()),
    }
}

fn parse_toml(file: &Path, bytes: &[u8], out: &mut Vec<Finding>) -> Option<toml::Table> {
    let unparsed = |out: &mut Vec<Finding>, reason: String| {
        out.push(Finding::Unparsed {
            file: file.to_path_buf(),
            reason,
        })
    };
    let Ok(text) = std::str::from_utf8(bytes) else {
        unparsed(out, "not UTF-8".to_string());
        return None;
    };
    match text.parse::<toml::Table>() {
        Ok(table) => Some(table),
        Err(error) => {
            unparsed(out, error.message().to_string());
            None
        }
    }
}

/// How an MCP server's tools are approved, as the file sets it: the server's
/// `default_tools_approval_mode`, then each tool's own `approval_mode` as `<tool>: <mode>`.
fn approval_mode(server: &Map<String, Value>) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(mode) = server.get("default_tools_approval_mode") {
        parts.push(text(mode));
    }
    if let Some(Value::Object(tools)) = server.get("tools") {
        for (tool, settings) in tools {
            if let Some(mode) = settings.get("approval_mode") {
                parts.push(format!("{tool}: {}", text(mode)));
            }
        }
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// A project's `.codex/config.toml`.
pub fn codex_findings(file: &Path, bytes: &[u8]) -> Vec<Finding> {
    let mut out = Vec::new();
    let Some(table) = parse_toml(file, bytes, &mut out) else {
        return out;
    };
    let other = |out: &mut Vec<Finding>, key: String, value: &Value| {
        out.push(Finding::OtherSetting {
            file: file.to_path_buf(),
            key,
            value: compact(value),
        });
    };
    let overridden = |out: &mut Vec<Finding>, key: String, value: &Value| {
        out.push(Finding::Overridden {
            file: file.to_path_buf(),
            key,
            value: compact(value),
        });
    };
    for (key, value) in &table {
        let value = json_of(value);
        match (key.as_str(), &value) {
            ("mcp_servers", Value::Object(servers)) => {
                for (name, server) in servers {
                    let mode = match server {
                        Value::Object(fields) => approval_mode(fields),
                        _ => None,
                    };
                    out.push(Finding::McpServer {
                        file: file.to_path_buf(),
                        name: name.clone(),
                        command_line: server_line(server),
                        approval_mode: mode,
                    });
                }
            }
            ("hooks", Value::Object(events)) => {
                // The hooks table also holds each hook's saved trust, which is not a hook.
                let mut events = events.clone();
                if let Some(state) = events.remove("state") {
                    other(&mut out, "hooks.state".to_string(), &state);
                }
                hooks(file, &events, &mut out);
            }
            ("features", Value::Object(features)) => {
                for (name, setting) in features {
                    let key = format!("features.{name}");
                    if OVERRIDDEN_FEATURES.contains(&name.as_str()) {
                        overridden(&mut out, key, setting);
                    } else {
                        other(&mut out, key, setting);
                    }
                }
            }
            (key, value) if OVERRIDDEN_KEYS.contains(&key) => overridden(&mut out, key.to_string(), value),
            (key, value) => other(&mut out, key.to_string(), value),
        }
    }
    out
}

/// A project's `.codex/hooks.json`: `{"hooks": {<Event>: [...]}}`, the same shape as Claude's.
pub fn hooks_json_findings(file: &Path, bytes: &[u8]) -> Vec<Finding> {
    let mut out = Vec::new();
    let Some(map) = parse_object(file, bytes, &mut out) else {
        return out;
    };
    for (key, value) in &map {
        match (key.as_str(), value) {
            ("hooks", Value::Object(events)) => hooks(file, events, &mut out),
            _ => out.push(Finding::OtherSetting {
                file: file.to_path_buf(),
                key: key.clone(),
                value: compact(value),
            }),
        }
    }
    out
}

/// A rule file under `.codex/rules/`: the file, with every rule call in it as written, and the whole
/// text as well when the calls are not all there is to it.
pub(super) fn rule_file_findings(file: &Path, bytes: &[u8]) -> Vec<Finding> {
    match std::str::from_utf8(bytes) {
        Ok(text) => {
            let scan = scan_rules(text);
            vec![Finding::RuleFile {
                path: file.to_path_buf(),
                calls: scan.calls,
                text: (!scan.only_calls).then(|| text.to_string()),
            }]
        }
        Err(_) => vec![Finding::Unparsed {
            file: file.to_path_buf(),
            reason: "not UTF-8".to_string(),
        }],
    }
}

fn is_name_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_name(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The index just past the comment starting at `at` (a `#`): the end of its line.
fn skip_comment(bytes: &[u8], at: usize) -> usize {
    bytes[at..]
        .iter()
        .position(|b| *b == b'\n')
        .map_or(bytes.len(), |n| at + n)
}

/// The index just past the string literal starting at `at` (a quote), single or triple quoted, a
/// backslash escaping the byte after it. An unterminated string runs to its line's end (single) or
/// the text's end (triple).
fn skip_string(bytes: &[u8], at: usize) -> usize {
    let quote = bytes[at];
    let triple = bytes.len() >= at + 3 && bytes[at + 1] == quote && bytes[at + 2] == quote;
    let mut i = at + if triple { 3 } else { 1 };
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'\n' if !triple => return i,
            b if b == quote => {
                if !triple {
                    return i + 1;
                }
                if bytes.len() >= i + 3 && bytes[i + 1] == quote && bytes[i + 2] == quote {
                    return i + 3;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    bytes.len()
}

/// The index just past the `)` matching the `(` at `open`, skipping strings and comments; the
/// text's end when it is never closed.
fn call_end(bytes: &[u8], open: usize) -> usize {
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
                if depth == 0 {
                    return i;
                }
            }
            b'#' => i = skip_comment(bytes, i),
            b'"' | b'\'' => i = skip_string(bytes, i),
            _ => i += 1,
        }
    }
    bytes.len()
}

/// Every `prefix_rule(...)`, `network_rule(...)` and `host_executable(...)` call in a rule file,
/// exactly as written (from the name to its closing parenthesis, or to the end of an unclosed one),
/// outside comments and strings.
pub fn rule_calls(text: &str) -> Vec<String> {
    scan_rules(text).calls
}

/// What a rule file holds, as far as a scan that does not evaluate it can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleScan {
    /// Each rule call spelt by its own name, as written.
    pub calls: Vec<String>,
    /// Whether those calls are all the file does: nothing outside them but comments, whitespace and
    /// `;`. A rule file is a program, so anything else -- a name bound to a rule function and
    /// called under that name, a function wrapping one, a `load` -- can make rules no call above
    /// spells out, and the file has to be shown as it is.
    pub only_calls: bool,
}

/// Scans a rule file for its rule calls and for anything else in it.
pub fn scan_rules(text: &str) -> RuleScan {
    let bytes = text.as_bytes();
    let mut calls = Vec::new();
    let mut only_calls = true;
    let mut i = 0;
    // Every byte the scan stops at is ASCII, so each slice below falls on a character boundary.
    while i < bytes.len() {
        match bytes[i] {
            b'#' => i = skip_comment(bytes, i),
            b';' => i += 1,
            b if b.is_ascii_whitespace() => i += 1,
            b'"' | b'\'' => {
                only_calls = false;
                i = skip_string(bytes, i);
            }
            b if is_name_start(b) => {
                let start = i;
                while i < bytes.len() && is_name(bytes[i]) {
                    i += 1;
                }
                if !RULE_CALLS.contains(&&text[start..i]) {
                    only_calls = false;
                    continue;
                }
                let mut open = i;
                while open < bytes.len() && bytes[open].is_ascii_whitespace() {
                    open += 1;
                }
                if open < bytes.len() && bytes[open] == b'(' {
                    let end = call_end(bytes, open);
                    calls.push(text[start..end].to_string());
                    i = end;
                } else {
                    // The rule function's name used as a value, not called here.
                    only_calls = false;
                }
            }
            b if b.is_ascii_digit() => {
                // A number, and any name glued to it, is not a call.
                only_calls = false;
                while i < bytes.len() && is_name(bytes[i]) {
                    i += 1;
                }
            }
            _ => {
                only_calls = false;
                i += 1;
            }
        }
    }
    RuleScan { calls, only_calls }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn rule_calls_skip_comments_and_strings() {
        let text = "# prefix_rule(pattern=[\"no\"])\nx = \"prefix_rule(\"\nprefix_rule(\n  pattern = [\"git\", \")\"],  # )\n  decision = \"allow\",\n)\nnetwork_rule(host=\"h\", protocol=\"https\", decision=\"allow\")\nprefix_rules = 1\n";
        assert_eq!(
            rule_calls(text),
            vec![
                "prefix_rule(\n  pattern = [\"git\", \")\"],  # )\n  decision = \"allow\",\n)".to_string(),
                "network_rule(host=\"h\", protocol=\"https\", decision=\"allow\")".to_string(),
            ]
        );
    }

    #[test]
    fn only_calls_comments_and_whitespace_are_accounted_for() {
        let plain = "# header\nprefix_rule(pattern=[\"ls\"]);\n\tnetwork_rule(host=\"h\")  # tail\n";
        assert!(scan_rules(plain).only_calls);
        assert!(scan_rules("").only_calls);
        for other in [
            "r = prefix_rule\nr(pattern=[\"rm\"], decision=\"allow\")\n",
            "def f():\n    prefix_rule(pattern=[\"rm\"])\nf()\n",
            "load(\"x.star\", \"y\")\n",
            "\"a string\"\nprefix_rule(pattern=[\"ls\"])\n",
            "x.prefix_rule(pattern=[\"ls\"])\n",
            "[prefix_rule(pattern=[c]) for c in [\"rm\"]]\n",
            "1\n",
            "prefix_rule\n",
        ] {
            assert!(!scan_rules(other).only_calls, "{other:?}");
        }
    }

    #[test]
    fn a_rule_file_with_more_than_calls_carries_its_text() {
        let text = "r = prefix_rule\nr(pattern=[\"rm\"], decision=\"allow\")\n";
        assert_eq!(
            rule_file_findings(Path::new("a.rules"), text.as_bytes()),
            vec![Finding::RuleFile {
                path: PathBuf::from("a.rules"),
                calls: vec![],
                text: Some(text.to_string()),
            }]
        );
    }

    #[test]
    fn an_unclosed_call_runs_to_the_end() {
        assert_eq!(
            rule_calls("prefix_rule(pattern=[\"a\""),
            vec!["prefix_rule(pattern=[\"a\"".to_string()]
        );
    }

    #[test]
    fn triple_quoted_strings_hide_their_text() {
        assert!(rule_calls("'''\nprefix_rule(x)\n'''\n").is_empty());
    }

    #[test]
    fn a_datetime_is_its_text() {
        let found = codex_findings(Path::new(".codex/config.toml"), b"when = 1979-05-27T07:32:00Z\n");
        assert_eq!(
            found,
            vec![Finding::OtherSetting {
                file: PathBuf::from(".codex/config.toml"),
                key: "when".into(),
                value: "\"1979-05-27T07:32:00Z\"".into(),
            }]
        );
    }

    #[test]
    fn bad_toml_is_unparsed() {
        assert!(matches!(
            &codex_findings(Path::new("c.toml"), b"[unclosed")[..],
            [Finding::Unparsed { .. }]
        ));
        assert!(matches!(
            &codex_findings(Path::new("c.toml"), b"\xff")[..],
            [Finding::Unparsed { .. }]
        ));
    }
}
