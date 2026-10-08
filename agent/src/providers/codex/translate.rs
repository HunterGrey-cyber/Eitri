//! Codex's words turned into Eitri's: the version in the user agent, how a turn ended, what a tool call
//! looked like going in and coming out, and the account's limit.
//!
//! Everything here is a pure function of one message, so each rule has a test that needs no process.
//! Nothing keeps the account's identity: the rate-limit snapshot has no field that names it, and the
//! types it is read into have none either.

use super::wire::{
    CollabAgentToolCallItem, CommandExecutionItem, FileChangeItem, ItemStatus, McpToolCallItem, PatchChangeKind,
    RateLimitSnapshot, RateLimitWindow, ThreadItem, TokenUsage, TurnError, TurnStatus,
};
use crate::projection::{TokenUsage as EitriTokens, TurnEndDetail, TurnOutcome, UsageInfo};
use serde_json::{json, Map, Value};

/// The most bytes of one tool result or one diff that reaches the transcript. A command can print
/// megabytes; the row shows the start and says it was cut.
pub const MAX_RESULT_BYTES: usize = 256 * 1024;

/// What a signed-out Codex tab says. The same words are the reason of an `unauthorized` turn.
pub const SIGNED_OUT: &str = "Codex is not signed in: run `codex login` in a terminal";

/// The version in `initialize.userAgent`, which reads `<originator>/<version> (<platform>) ...`.
/// A pre-release suffix (`0.161.0-alpha.3`) counts as its release. `None` when no version can be read, so
/// a caller refuses a binary whose version it cannot check.
pub fn parse_user_agent_version(ua: &str) -> Option<(u32, u32, u32)> {
    let first = ua.split_whitespace().next()?;
    let version = first.rsplit_once('/').map_or(first, |(_, v)| v);
    let mut parts = version.splitn(3, '.');
    let major = leading_number(parts.next()?)?;
    let minor = leading_number(parts.next()?)?;
    let patch = leading_number(parts.next()?)?;
    Some((major, minor, patch))
}

/// The digits a text starts with; `None` when it does not start with one.
fn leading_number(text: &str) -> Option<u32> {
    let end = text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len());
    text[..end].parse().ok()
}

pub fn version_text(version: (u32, u32, u32)) -> String {
    format!("{}.{}.{}", version.0, version.1, version.2)
}

/// Cuts a text to [`MAX_RESULT_BYTES`] at a character boundary and says so.
pub fn bounded(text: &str) -> String {
    if text.len() <= MAX_RESULT_BYTES {
        return text.to_string();
    }
    let mut cut = MAX_RESULT_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n[cut here: the rest was too long to show]", &text[..cut])
}

// ---------------------------------------------------------------- how a turn ended

/// `turn/completed` as Eitri's outcome and detail. `codexErrorInfo` decides a failed turn; an unknown
/// variant is a failure that names itself.
pub fn turn_ending(status: &TurnStatus, error: Option<&TurnError>) -> (TurnOutcome, TurnEndDetail) {
    match status {
        TurnStatus::Completed => (TurnOutcome::Completed, TurnEndDetail::default()),
        TurnStatus::Interrupted => (TurnOutcome::Interrupted, TurnEndDetail::default()),
        TurnStatus::Failed => failed_ending(error),
        TurnStatus::InProgress => ending(
            TurnOutcome::Failed,
            "Codex reported the turn as finished while still running".to_string(),
            Some("in_progress".to_string()),
            None,
        ),
        TurnStatus::Unknown(word) => ending(
            TurnOutcome::Failed,
            format!("Codex ended the turn with a status Eitri does not know: {word}"),
            Some(word.clone()),
            None,
        ),
    }
}

fn failed_ending(error: Option<&TurnError>) -> (TurnOutcome, TurnEndDetail) {
    let message = error.map(|e| e.message.trim()).unwrap_or_default();
    let Some(info) = error.and_then(|e| e.codex_error_info.as_ref()) else {
        let said = if message.is_empty() {
            "Codex ended the turn with an error".to_string()
        } else {
            message.to_string()
        };
        return ending(TurnOutcome::Failed, said, None, None);
    };
    let name = info.name();
    let status = info.http_status().map(u32::from);
    let verbatim = |fallback: &str| {
        if message.is_empty() {
            fallback.to_string()
        } else {
            message.to_string()
        }
    };
    match name {
        "usageLimitExceeded" => ending(
            TurnOutcome::LimitReached,
            verbatim("Codex's usage limit was reached"),
            Some("usage_limit".into()),
            status,
        ),
        "rateLimitExceeded" => ending(
            TurnOutcome::LimitReached,
            verbatim("Codex is rate limited"),
            Some("rate_limited".into()),
            status,
        ),
        "sessionBudgetExceeded" => ending(
            TurnOutcome::LimitReached,
            verbatim("Codex's spending limit was reached"),
            Some("session_budget".into()),
            status,
        ),
        "contextWindowExceeded" => ending(
            TurnOutcome::LimitReached,
            verbatim("Codex's context is full"),
            Some("context_window_exceeded".into()),
            status,
        ),
        "tooManyDenials" => ending(
            TurnOutcome::Failed,
            verbatim("Codex stopped after too many denied requests"),
            Some("too_many_denials".into()),
            status,
        ),
        "sandboxError" => ending(
            TurnOutcome::Failed,
            verbatim("Codex's sandbox failed"),
            Some("sandbox_error".into()),
            status,
        ),
        "unauthorized" => ending(
            TurnOutcome::Failed,
            SIGNED_OUT.to_string(),
            Some("auth_required".into()),
            status,
        ),
        // The connection variants, the response-stream variants and anything unknown: failed, named by
        // the variant, with the HTTP status when Codex gave one.
        other => ending(TurnOutcome::Failed, verbatim(other), Some(other.to_string()), status),
    }
}

fn ending(
    outcome: TurnOutcome,
    message: String,
    reason: Option<String>,
    api_error_status: Option<u32>,
) -> (TurnOutcome, TurnEndDetail) {
    let detail = TurnEndDetail::from_result(outcome, true, &message, reason, api_error_status, &[]);
    (outcome, detail)
}

// ---------------------------------------------------------------- usage and limits

/// The thread's running token total as the usage a finished turn reports. Codex reports no cost, so the
/// cost is marked absent, never zero. Negative counts, which no honest server sends, read as zero.
pub fn usage_of(usage: &TokenUsage, model: Option<&str>) -> UsageInfo {
    let count = |n: i64| u64::try_from(n).unwrap_or(0);
    let total = &usage.total;
    UsageInfo {
        total_cost_usd: 0.0,
        cost_reported: false,
        num_turns: None,
        tokens: Some(EitriTokens {
            input: count(total.input_tokens),
            output: count(total.output_tokens),
            cache_creation: count(total.cache_write_input_tokens),
            cache_read: count(total.cached_input_tokens),
        }),
        model: model.filter(|m| !m.is_empty()).map(str::to_string),
    }
}

/// What the rate-limit snapshot says, as the single figure the panel shows: the window that is closest
/// to its limit, since that is the one that will stop the user first. `None` when no window reports
/// how much is used.
pub fn account_limits(snapshot: &RateLimitSnapshot) -> Option<LimitFigure> {
    let windows: Vec<&RateLimitWindow> = [snapshot.primary.as_ref(), snapshot.secondary.as_ref()]
        .into_iter()
        .flatten()
        .filter(|w| w.used_percent.is_some_and(f64::is_finite))
        .collect();
    let mut best: Option<&RateLimitWindow> = None;
    for window in windows {
        let used = window.used_percent.unwrap_or(0.0);
        if best.is_none_or(|b| used > b.used_percent.unwrap_or(0.0)) {
            best = Some(window);
        }
    }
    let window = best?;
    Some(LimitFigure {
        used_percent: window.used_percent.unwrap_or(0.0),
        window_minutes: window.window_duration_mins.and_then(|m| u32::try_from(m).ok()),
        resets_at: window.resets_at,
        plan: snapshot.plan_type.clone().filter(|p| !p.is_empty()),
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct LimitFigure {
    pub used_percent: f64,
    pub window_minutes: Option<u32>,
    pub resets_at: Option<i64>,
    pub plan: Option<String>,
}

// ---------------------------------------------------------------- tool calls

/// The start of a row id built from a thread as well as an item id.
const THREAD_ROW_PREFIX: &str = "codex-thread:";
/// The start of every row id Eitri makes up itself (notes, history, rows for requests that had none).
const EITRI_ROW_PREFIX: &str = "eitri-";

/// The id a call's row goes under. Codex numbers items on each thread alone, so a parent and its subagent
/// can both run a `call-1`, while the transcript links a row, its result, its card and its notes by this id
/// alone. A call of the tab's own thread keeps Codex's id; one of another thread, and one of the tab's whose
/// id could be mistaken for a built or an Eitri id, is `codex-thread:<thread length>:<thread>:<item>`. The
/// length makes the split unambiguous whatever the two ids hold, so no two calls ever share a row.
pub fn row_id(tab_thread: &str, thread: &str, item: &str) -> String {
    if thread == tab_thread && !item.starts_with(THREAD_ROW_PREFIX) && !item.starts_with(EITRI_ROW_PREFIX) {
        return item.to_string();
    }
    format!("{THREAD_ROW_PREFIX}{}:{thread}:{item}", thread.len())
}

/// A tool call as the transcript names it: the item type Codex gave it, and its input in the shape the
/// panel draws. `None` for an item that is not a tool call (messages, reasoning, markers).
pub fn tool_start(item: &ThreadItem) -> Option<(&'static str, Value)> {
    match item {
        ThreadItem::CommandExecution(c) => Some(("commandExecution", command_input(c))),
        ThreadItem::FileChange(f) => Some(("fileChange", file_change_input(f))),
        ThreadItem::McpToolCall(m) => Some(("mcpToolCall", mcp_input(m))),
        ThreadItem::CollabAgentToolCall(c) => Some(("collabAgentToolCall", collab_input(c))),
        ThreadItem::Unknown { item_type, raw, .. } if item_type == "webSearch" => {
            Some(("webSearch", without_identity(raw)))
        }
        _ => None,
    }
}

/// `command` exactly as Codex will run it, shell wrapper included.
fn command_input(c: &CommandExecutionItem) -> Value {
    json!({ "command": c.command, "cwd": c.cwd })
}

fn file_change_input(f: &FileChangeItem) -> Value {
    let changes: Vec<Value> = f
        .changes
        .iter()
        .map(|change| {
            let mut entry = Map::new();
            entry.insert("path".into(), json!(change.path));
            let (kind, moved) = match &change.kind {
                PatchChangeKind::Add => ("add".to_string(), None),
                PatchChangeKind::Delete => ("delete".to_string(), None),
                PatchChangeKind::Update { move_path } => ("update".to_string(), move_path.clone()),
                PatchChangeKind::Unknown(other) => (other.clone(), None),
            };
            entry.insert("kind".into(), json!(kind));
            entry.insert("diff".into(), json!(bounded(&change.diff)));
            if let Some(path) = moved {
                entry.insert("movePath".into(), json!(path));
            }
            Value::Object(entry)
        })
        .collect();
    json!({ "changes": changes })
}

fn mcp_input(m: &McpToolCallItem) -> Value {
    json!({ "server": m.server, "tool": m.tool, "arguments": m.arguments })
}

fn collab_input(c: &CollabAgentToolCallItem) -> Value {
    json!({ "tool": c.tool, "prompt": c.prompt, "receiverThreadIds": c.receiver_thread_ids })
}

/// A raw item's own fields, without the identity the transcript already carries.
fn without_identity(raw: &Value) -> Value {
    let mut map = raw.as_object().cloned().unwrap_or_default();
    map.remove("id");
    map.remove("type");
    Value::Object(map)
}

/// The result text of a `fileChange` that Codex finished applying. Turn review names a changed file only
/// from a call whose result is this word, so the two must agree.
pub const FILE_CHANGE_APPLIED: &str = "applied";

/// What a finished tool call returned and whether it failed. `None` for an item that is not a tool call.
pub fn tool_result(item: &ThreadItem) -> Option<(Value, bool)> {
    match item {
        ThreadItem::CommandExecution(c) => {
            let failed = matches!(c.status, ItemStatus::Failed | ItemStatus::Declined)
                || c.exit_code.is_some_and(|code| code != 0);
            let output = c.aggregated_output.as_deref().unwrap_or_default();
            let content = if matches!(c.status, ItemStatus::Declined) && output.is_empty() {
                "declined".to_string()
            } else {
                bounded(output)
            };
            Some((Value::String(content), failed))
        }
        ThreadItem::FileChange(f) => {
            let failed = matches!(f.status, ItemStatus::Failed | ItemStatus::Declined);
            let word = match &f.status {
                ItemStatus::Completed => FILE_CHANGE_APPLIED.to_string(),
                ItemStatus::Declined => "declined".to_string(),
                ItemStatus::Failed => "failed".to_string(),
                ItemStatus::InProgress => "in progress".to_string(),
                ItemStatus::Unknown(other) => other.clone(),
            };
            Some((Value::String(word), failed))
        }
        ThreadItem::McpToolCall(m) => {
            let failed = matches!(m.status, ItemStatus::Failed | ItemStatus::Declined) || m.error.is_some();
            let content = match (&m.error, &m.result) {
                (Some(error), _) => error.clone(),
                (None, Some(result)) => result.clone(),
                (None, None) => Value::String(String::new()),
            };
            Some((bounded_value(content), failed))
        }
        ThreadItem::CollabAgentToolCall(c) => {
            let failed = matches!(c.status, ItemStatus::Failed | ItemStatus::Declined);
            let word = match &c.status {
                ItemStatus::Completed => "completed",
                ItemStatus::Failed => "failed",
                ItemStatus::Declined => "declined",
                ItemStatus::InProgress => "in progress",
                ItemStatus::Unknown(other) => other.as_str(),
            };
            Some((Value::String(format!("{}: {word}", c.tool)), failed))
        }
        ThreadItem::Unknown { item_type, .. } if item_type == "webSearch" => {
            Some((Value::String(String::new()), false))
        }
        _ => None,
    }
}

/// A result value that is no larger than [`MAX_RESULT_BYTES`] when written out; a larger one is replaced
/// by its text, cut.
fn bounded_value(value: Value) -> Value {
    let text = value.to_string();
    if text.len() <= MAX_RESULT_BYTES {
        value
    } else {
        Value::String(bounded(&text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_ids_never_collide_across_threads() {
        assert_eq!(row_id("tab", "tab", "call-1"), "call-1");
        let child = row_id("tab", "child", "call-1");
        assert_ne!(child, "call-1");
        // A tab id that looks built or made up by Eitri is built too, so it cannot pose as another row.
        assert_ne!(row_id("tab", "tab", &child), child);
        assert_ne!(row_id("tab", "tab", "eitri-answered-t1-1"), "eitri-answered-t1-1");
        // Ids holding the separator still split one way only.
        assert_ne!(row_id("tab", "a:b", "c"), row_id("tab", "a", "b:c"));
        let mut seen = std::collections::HashSet::new();
        for thread in ["tab", "child", "a:b", "a"] {
            for item in ["call-1", "c", "b:c", "codex-thread:3:tab:x", "eitri-x"] {
                assert!(seen.insert(row_id("tab", thread, item)), "{thread} {item}");
            }
        }
    }

    #[test]
    fn the_version_is_read_from_the_user_agent() {
        let ua = "eitri-probe/0.160.0 (Arch Linux Rolling Release; x86_64) tmux-256color (eitri-probe; 0)";
        assert_eq!(parse_user_agent_version(ua), Some((0, 160, 0)));
        assert_eq!(parse_user_agent_version("eitri/0.161.2-alpha.3 (x)"), Some((0, 161, 2)));
        assert_eq!(parse_user_agent_version("1.20.300"), Some((1, 20, 300)));
        for odd in ["", "codex", "codex/1.2", "codex/x.y.z (a)", "codex/1..3"] {
            assert_eq!(parse_user_agent_version(odd), None, "{odd:?}");
        }
    }

    #[test]
    fn a_long_result_is_cut_at_a_character_boundary() {
        let text = "é".repeat(MAX_RESULT_BYTES);
        let cut = bounded(&text);
        assert!(cut.len() < text.len());
        assert!(cut.ends_with("to show]"));
        assert_eq!(bounded("short"), "short");
    }
}
