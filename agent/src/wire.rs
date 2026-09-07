//! Deserialization types for the specific real wire-JSON shapes this crate models, plus the
//! translation into `AgentEvent`. Deliberately does NOT attempt to model the CLI's entire
//! protocol -- see this plan's Global Constraint on `Unknown` as the fallback for everything
//! else. Every shape below was captured from a real `claude -p --output-format stream-json
//! --verbose` invocation (version 2.1.263), not hand-guessed from documentation -- see
//! `agent/tests/fixtures/*.json` for the exact (trimmed but real) captured lines this module's
//! own tests parse.

use crate::event::AgentEvent;
use serde::Deserialize;
use serde_json::Value;

/// The minimal envelope every wire line shares -- used to peek at `type`/`subtype` before
/// deciding which strongly-typed shape (if any) to parse the rest of the line as.
#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    subtype: Option<String>,
}

#[derive(Deserialize)]
struct InitLine {
    session_id: String,
    model: String,
    cwd: String,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
    thinking: Option<String>,
    id: Option<String>,
    name: Option<String>,
    input: Option<Value>,
}

#[derive(Deserialize)]
struct AssistantMessage {
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
struct AssistantLine {
    message: AssistantMessage,
}

#[derive(Deserialize)]
struct ToolResultBlock {
    tool_use_id: String,
    content: Value,
    #[serde(default)]
    is_error: bool,
}

#[derive(Deserialize)]
struct UserMessage {
    content: Vec<ToolResultBlock>,
}

#[derive(Deserialize)]
struct UserLine {
    message: UserMessage,
}

#[derive(Deserialize)]
struct ResultLine {
    result: String,
    is_error: bool,
    stop_reason: Option<String>,
    total_cost_usd: f64,
    num_turns: u32,
}

/// Parses one line of the CLI's newline-delimited `stream-json` stdout into zero or more
/// `AgentEvent`s. Never panics or returns an error to the caller -- a line that fails to parse
/// entirely, or whose `type`/`subtype` this crate doesn't model, becomes a single
/// `AgentEvent::Unknown` carrying whatever raw JSON could be recovered (or an empty object if
/// even that failed), so the calling stdout-reader loop (`AgentProcess`, Task 2) can keep
/// running regardless of what the CLI ever sends.
pub fn translate_line(line: &str) -> Vec<AgentEvent> {
    let raw: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => {
            return vec![AgentEvent::Unknown { kind: "parse_error".into(), subtype: None, raw: Value::String(line.to_string()) }];
        }
    };
    let envelope: Envelope = match serde_json::from_value(raw.clone()) {
        Ok(e) => e,
        Err(_) => {
            return vec![AgentEvent::Unknown { kind: "no_type_field".into(), subtype: None, raw }];
        }
    };

    match envelope.kind.as_str() {
        "system" if envelope.subtype.as_deref() == Some("init") => {
            match serde_json::from_value::<InitLine>(raw.clone()) {
                Ok(init) => vec![AgentEvent::SessionStarted {
                    session_id: init.session_id,
                    model: init.model,
                    cwd: init.cwd,
                }],
                Err(_) => vec![AgentEvent::Unknown { kind: envelope.kind, subtype: envelope.subtype, raw }],
            }
        }
        "assistant" => {
            // Parsed alongside the original raw `Value` array (not just the typed
            // `Vec<ContentBlock>`) so the `Unknown` fallback below can carry a content block's
            // real JSON instead of discarding it -- every other `Unknown` path in this file
            // preserves the raw value it couldn't fully parse; this one used to be the sole
            // exception.
            let raw_blocks: Vec<Value> = raw
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();
            match serde_json::from_value::<AssistantLine>(raw.clone()) {
                Ok(line) => line
                    .message
                    .content
                    .into_iter()
                    .zip(raw_blocks.into_iter().map(Some).chain(std::iter::repeat(None)))
                    .map(|(block, raw_block)| match block.kind.as_str() {
                        "text" => AgentEvent::AssistantText { text: block.text.unwrap_or_default() },
                        "thinking" => AgentEvent::Thinking { text: block.thinking.unwrap_or_default() },
                        "tool_use" => AgentEvent::ToolStarted {
                            id: block.id.unwrap_or_default(),
                            name: block.name.unwrap_or_default(),
                            input: block.input.unwrap_or(Value::Null),
                        },
                        other => AgentEvent::Unknown {
                            kind: "assistant_content_block".into(),
                            subtype: Some(other.to_string()),
                            raw: raw_block.unwrap_or(Value::Null),
                        },
                    })
                    .collect(),
                Err(_) => vec![AgentEvent::Unknown { kind: envelope.kind, subtype: envelope.subtype, raw }],
            }
        }
        "user" => match serde_json::from_value::<UserLine>(raw.clone()) {
            Ok(line) => line
                .message
                .content
                .into_iter()
                .map(|block| AgentEvent::ToolResult {
                    id: block.tool_use_id,
                    content: block.content,
                    is_error: block.is_error,
                })
                .collect(),
            Err(_) => vec![AgentEvent::Unknown { kind: envelope.kind, subtype: envelope.subtype, raw }],
        },
        "result" => match serde_json::from_value::<ResultLine>(raw.clone()) {
            Ok(r) => vec![AgentEvent::TurnFinished {
                result_text: r.result,
                is_error: r.is_error,
                stop_reason: r.stop_reason,
                total_cost_usd: r.total_cost_usd,
                num_turns: r.num_turns,
            }],
            Err(_) => vec![AgentEvent::Unknown { kind: envelope.kind, subtype: envelope.subtype, raw }],
        },
        "rate_limit_event" => vec![AgentEvent::RateLimit { raw }],
        other => vec![AgentEvent::Unknown { kind: other.to_string(), subtype: envelope.subtype, raw }],
    }
}
