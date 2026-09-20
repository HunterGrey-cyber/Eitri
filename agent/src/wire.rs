//! Deserialization types for the specific real wire-JSON shapes this crate models, plus the
//! translation into `AgentEvent`. Deliberately does NOT attempt to model the CLI's entire
//! protocol -- see this plan's Global Constraint on `Unknown` as the fallback for everything
//! else. Every shape below was captured from a real `claude -p --output-format stream-json
//! --verbose` invocation (version 2.1.263), not hand-guessed from documentation -- see
//! `agent/tests/fixtures/*.json` for the exact (trimmed but real) captured lines this module's
//! own tests parse.

use crate::event::{AgentEvent, PermissionSource};
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
    #[serde(default)]
    result: String,
    is_error: bool,
    stop_reason: Option<String>,
    total_cost_usd: f64,
    num_turns: u32,
}

#[derive(Deserialize)]
struct ControlRequestLine {
    request_id: String,
    request: ControlRequestBody,
}

#[derive(Deserialize)]
struct ControlRequestBody {
    subtype: String,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    input: Option<Value>,
    /// The id of the tool call this request is asking about. The Claude Agent SDK's own
    /// declaration of this message (`SDKControlPermissionRequest`, checked against `sdk.d.ts` from
    /// package `0.3.263` -- see `agent/CAPTURE_NOTES.md` step 6) marks it `tool_use_id: string`,
    /// required. It is read through `#[serde(default)]` anyway, so a message that turns out not to
    /// carry it parses to `None` rather than failing: this project has never observed this message
    /// on a real wire at all, so the declaration is the only evidence there is about its shape, and
    /// a permission request that cannot be linked is much better than one that cannot be answered.
    #[serde(default)]
    tool_use_id: Option<String>,
}

#[derive(Deserialize)]
struct ControlResponseLine {
    response: ControlResponseBody,
}

#[derive(Deserialize)]
struct ControlResponseBody {
    request_id: String,
    subtype: String,
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
            return vec![AgentEvent::Unknown {
                kind: "parse_error".into(),
                subtype: None,
                raw: Value::String(line.to_string()),
            }];
        }
    };
    let envelope: Envelope = match serde_json::from_value(raw.clone()) {
        Ok(e) => e,
        Err(_) => {
            return vec![AgentEvent::Unknown {
                kind: "no_type_field".into(),
                subtype: None,
                raw,
            }];
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
                Err(_) => vec![AgentEvent::Unknown {
                    kind: envelope.kind,
                    subtype: envelope.subtype,
                    raw,
                }],
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
                        "text" => AgentEvent::AssistantText {
                            text: block.text.unwrap_or_default(),
                        },
                        "thinking" => AgentEvent::Thinking {
                            text: block.thinking.unwrap_or_default(),
                        },
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
                Err(_) => vec![AgentEvent::Unknown {
                    kind: envelope.kind,
                    subtype: envelope.subtype,
                    raw,
                }],
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
            Err(_) => vec![AgentEvent::Unknown {
                kind: envelope.kind,
                subtype: envelope.subtype,
                raw,
            }],
        },
        "result" => match serde_json::from_value::<ResultLine>(raw.clone()) {
            Ok(r) => vec![AgentEvent::TurnFinished {
                result_text: r.result,
                is_error: r.is_error,
                stop_reason: r.stop_reason,
                total_cost_usd: r.total_cost_usd,
                num_turns: r.num_turns,
            }],
            Err(_) => vec![AgentEvent::Unknown {
                kind: envelope.kind,
                subtype: envelope.subtype,
                raw,
            }],
        },
        "rate_limit_event" => vec![AgentEvent::RateLimit { raw }],
        "control_request" => match serde_json::from_value::<ControlRequestLine>(raw.clone()) {
            Ok(cr) if cr.request.subtype == "can_use_tool" => {
                vec![AgentEvent::PermissionRequest {
                    request_id: cr.request_id,
                    // The message's OWN `tool_use_id`, and never `request_id`: that one belongs to
                    // the control_request envelope (`"ctu-1"` in the checked-in fixture) and names
                    // nothing in the transcript, so passing it on as a tool-use id would produce a
                    // permission card pointing at a tool call that does not exist. Verbatim, not
                    // judged -- `Some("")` is decided against once, at the domain boundary
                    // (`projection::tool_use_link`), the same way the hook-relay path's id is.
                    //
                    // This being `Some` does not make this path a gate. `can_use_tool` is the
                    // secondary, confirmed-leaky channel (a real `Bash` call has been observed
                    // running to completion with no `can_use_tool` in the stream at all); the
                    // `PreToolUse` hook relay is the gate. What the id affects is only whether a
                    // request that DID arrive can name its call.
                    tool_use_id: cr.request.tool_use_id,
                    tool_name: cr.request.tool_name.unwrap_or_default(),
                    input: cr.request.input.unwrap_or(Value::Null),
                    source: PermissionSource::CanUseTool,
                }]
            }
            // Every other control_request subtype (interrupt acks are control_RESPONSEs, not
            // requests the CLI sends us; this arm is for CLI-initiated control_requests other
            // than can_use_tool, e.g. hook_callback) is preserved raw, never acted on.
            Ok(cr) => vec![AgentEvent::Unknown {
                kind: "control_request".into(),
                subtype: Some(cr.request.subtype),
                raw,
            }],
            Err(_) => vec![AgentEvent::Unknown {
                kind: envelope.kind,
                subtype: envelope.subtype,
                raw,
            }],
        },
        "control_response" => match serde_json::from_value::<ControlResponseLine>(raw.clone()) {
            Ok(cr) => vec![AgentEvent::ControlResponse {
                request_id: cr.response.request_id,
                subtype: cr.response.subtype,
                raw,
            }],
            Err(_) => vec![AgentEvent::Unknown {
                kind: envelope.kind,
                subtype: envelope.subtype,
                raw,
            }],
        },
        other => vec![AgentEvent::Unknown {
            kind: other.to_string(),
            subtype: envelope.subtype,
            raw,
        }],
    }
}
