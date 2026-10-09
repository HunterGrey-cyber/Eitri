//! The Agent Client Protocol (v1) wire, hand-written for exactly the methods Eitri uses.
//!
//! **Outbound** types serialize what Eitri sends. What Eitri tells the agent about itself is fixed here:
//! no file-system methods and no terminals are offered, so an agent has nothing to ask the client for
//! beyond a decision.
//!
//! **Inbound** values are read leniently, by hand, from `serde_json::Value`: agents add update kinds,
//! content types and fields between releases, and a field this build does not know must not end a turn.
//! An update kind Eitri does not know is [`Update::Unknown`]; a payload that lacks what a known kind must
//! carry is dropped by the caller with a log line, never guessed at.
//!
//! Nothing under `_meta` and none of the `_auth/*` notifications is read: they carry the account's
//! details, which are not Eitri's to keep or to log.

use serde::Serialize;
use serde_json::{json, Value};

/// The method names Eitri sends, and the ones it reads.
pub mod method {
    pub const INITIALIZE: &str = "initialize";
    pub const SESSION_NEW: &str = "session/new";
    pub const SESSION_LOAD: &str = "session/load";
    pub const SESSION_PROMPT: &str = "session/prompt";
    pub const SESSION_CANCEL: &str = "session/cancel";
    pub const SESSION_CLOSE: &str = "session/close";
    pub const SESSION_SET_MODE: &str = "session/set_mode";

    pub const SESSION_UPDATE: &str = "session/update";
    pub const REQUEST_PERMISSION: &str = "session/request_permission";
}

/// The one protocol version Eitri speaks.
pub const PROTOCOL_VERSION: u64 = 1;

// ------------------------------------------------------------------------------------ outbound

#[derive(Debug, Serialize)]
struct FsCapabilities {
    #[serde(rename = "readTextFile")]
    read_text_file: bool,
    #[serde(rename = "writeTextFile")]
    write_text_file: bool,
}

#[derive(Debug, Serialize)]
struct ClientCapabilities {
    fs: FsCapabilities,
    terminal: bool,
}

#[derive(Debug, Serialize)]
struct ClientInfo {
    name: &'static str,
    title: &'static str,
    version: String,
}

/// `initialize`: the capabilities are constants, so no caller can offer an agent a file or a terminal.
#[derive(Debug, Serialize)]
pub struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    protocol_version: u64,
    #[serde(rename = "clientCapabilities")]
    client_capabilities: ClientCapabilities,
    #[serde(rename = "clientInfo")]
    client_info: ClientInfo,
}

impl InitializeParams {
    pub fn eitri(version: &str) -> InitializeParams {
        InitializeParams {
            protocol_version: PROTOCOL_VERSION,
            client_capabilities: ClientCapabilities {
                fs: FsCapabilities {
                    read_text_file: false,
                    write_text_file: false,
                },
                terminal: false,
            },
            client_info: ClientInfo {
                name: "eitri",
                title: "Eitri",
                version: version.to_string(),
            },
        }
    }
}

/// `session/new`. No tool servers are passed: the agent runs the ones its own configuration names.
pub fn session_new_params(cwd: &str) -> Value {
    json!({ "cwd": cwd, "mcpServers": [] })
}

/// `session/load`.
pub fn session_load_params(session_id: &str, cwd: &str) -> Value {
    json!({ "sessionId": session_id, "cwd": cwd, "mcpServers": [] })
}

/// `session/prompt`: the user's text as the one text block of the turn.
pub fn session_prompt_params(session_id: &str, text: &str) -> Value {
    json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": text }] })
}

/// `session/cancel` and `session/close` name only the session.
pub fn session_only_params(session_id: &str) -> Value {
    json!({ "sessionId": session_id })
}

/// `session/set_mode`: the one mode change Eitri ever makes, to the mode the agent was registered with.
pub fn set_mode_params(session_id: &str, mode_id: &str) -> Value {
    json!({ "sessionId": session_id, "modeId": mode_id })
}

/// The answer to a permission request that selects one of the agent's own options.
pub fn selected_outcome(option_id: &str) -> Value {
    json!({ "outcome": { "outcome": "selected", "optionId": option_id } })
}

/// The answer to a permission request that nobody will decide any more.
pub fn cancelled_outcome() -> Value {
    json!({ "outcome": { "outcome": "cancelled" } })
}

// ------------------------------------------------------------------------------------ inbound

fn text_of(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

/// What `initialize` answered, as far as Eitri reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitializeResponse {
    /// `None` when the answer carried no number.
    pub protocol_version: Option<u64>,
    /// `loadSession`: the agent can replay a saved session.
    pub load_session: bool,
    /// `sessionCapabilities.close`: the agent takes `session/close`.
    pub close_session: bool,
    /// What the agent calls itself, for the panel: its title or name, with the version when it gave one.
    pub agent_label: Option<String>,
}

impl InitializeResponse {
    pub fn parse(value: &Value) -> InitializeResponse {
        let capabilities = value.get("agentCapabilities");
        let info = value.get("agentInfo");
        let name = info.and_then(|i| text_of(i, "title").or_else(|| text_of(i, "name")));
        let version = info.and_then(|i| text_of(i, "version"));
        InitializeResponse {
            protocol_version: value.get("protocolVersion").and_then(Value::as_u64),
            load_session: capabilities
                .and_then(|c| c.get("loadSession"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            close_session: capabilities
                .and_then(|c| c.get("sessionCapabilities"))
                .and_then(|c| c.get("close"))
                .is_some_and(|close| !close.is_null()),
            agent_label: match (name, version) {
                (Some(name), Some(version)) => Some(format!("{name} {version}")),
                (Some(name), None) => Some(name),
                _ => None,
            },
        }
    }
}

/// What `session/new` or `session/load` answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSetup {
    /// Present in a `session/new` answer; a load keeps the id it asked for.
    pub session_id: Option<String>,
    /// `modes.currentModeId`, when the agent reports modes.
    pub current_mode: Option<String>,
    /// `models.currentModelId`, for the panel's model line.
    pub current_model: Option<String>,
}

impl SessionSetup {
    pub fn parse(value: &Value) -> SessionSetup {
        SessionSetup {
            session_id: text_of(value, "sessionId"),
            current_mode: value
                .get("modes")
                .and_then(|m| text_of(m, "currentModeId"))
                .or_else(|| mode_from_options(value.get("configOptions"))),
            current_model: value.get("models").and_then(|m| text_of(m, "currentModelId")),
        }
    }
}

/// The current value of the option that holds the mode, in a list of config options. An option is the
/// mode's when its category or its id says `mode`.
pub fn mode_from_options(options: Option<&Value>) -> Option<String> {
    options?
        .as_array()?
        .iter()
        .find(|o| text_of(o, "category").as_deref() == Some("mode") || text_of(o, "id").as_deref() == Some("mode"))
        .and_then(|o| text_of(o, "currentValue"))
}

/// A tool call as an update or a permission request describes it. Every field but the id is optional,
/// because an update carries only what changed.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallFields {
    pub id: String,
    pub title: Option<String>,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub raw_input: Option<Value>,
    pub raw_output: Option<Value>,
    pub locations: Option<Value>,
    pub content: Vec<Value>,
}

impl ToolCallFields {
    /// `None` when the call names no id: nothing could tie later updates to it.
    pub fn parse(value: &Value) -> Option<ToolCallFields> {
        let id = text_of(value, "toolCallId").filter(|id| !id.is_empty())?;
        let non_null = |key: &str| value.get(key).filter(|v| !v.is_null()).cloned();
        Some(ToolCallFields {
            id,
            title: text_of(value, "title"),
            kind: text_of(value, "kind"),
            status: text_of(value, "status"),
            raw_input: non_null("rawInput"),
            raw_output: non_null("rawOutput"),
            locations: non_null("locations"),
            content: value
                .get("content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        })
    }

    /// Applies a later update about the same call: a field it carries replaces the earlier one, and one it
    /// leaves out stays. A `content` list that is not empty replaces the earlier list as a whole.
    pub fn merge(&mut self, update: &ToolCallFields) {
        fn replace<T: Clone>(slot: &mut Option<T>, with: &Option<T>) {
            if with.is_some() {
                slot.clone_from(with);
            }
        }
        replace(&mut self.title, &update.title);
        replace(&mut self.kind, &update.kind);
        replace(&mut self.status, &update.status);
        replace(&mut self.raw_input, &update.raw_input);
        replace(&mut self.raw_output, &update.raw_output);
        replace(&mut self.locations, &update.locations);
        if !update.content.is_empty() {
            self.content.clone_from(&update.content);
        }
    }

    /// Whether the call has named a file: a location, or a diff in its content.
    pub fn names_a_path(&self) -> bool {
        self.locations
            .as_ref()
            .and_then(Value::as_array)
            .is_some_and(|l| !l.is_empty())
            || !self.diffs().is_empty()
    }

    /// Whether the call has ended, one way or the other.
    pub fn is_terminal(&self) -> bool {
        matches!(self.status.as_deref(), Some("completed" | "failed"))
    }

    pub fn failed(&self) -> bool {
        self.status.as_deref() == Some("failed")
    }

    /// Each `diff` content item as `{path, oldText, newText}`.
    pub fn diffs(&self) -> Vec<Value> {
        self.content
            .iter()
            .filter(|c| text_of(c, "type").as_deref() == Some("diff"))
            .map(|c| {
                json!({
                    "path": c.get("path").cloned().unwrap_or(Value::Null),
                    "oldText": c.get("oldText").cloned().unwrap_or(Value::Null),
                    "newText": c.get("newText").cloned().unwrap_or(Value::Null),
                })
            })
            .collect()
    }

    /// The text of the call's `content` items, joined.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter(|c| text_of(c, "type").as_deref() == Some("content"))
            .filter_map(|c| c.get("content"))
            .filter(|inner| text_of(inner, "type").as_deref() == Some("text"))
            .filter_map(|inner| inner.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Which side of the conversation a message chunk is from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkKind {
    Agent,
    Thought,
    User,
}

/// One `session/update`, by kind.
#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    /// A piece of a message. `text` is `None` for content that is not text (an image, a resource).
    Chunk {
        kind: ChunkKind,
        message_id: Option<String>,
        text: Option<String>,
    },
    ToolCall(ToolCallFields),
    ToolCallUpdate(ToolCallFields),
    /// `usage_update`: how full the context window is. Not a count of tokens spent.
    Context {
        used: u64,
        size: u64,
    },
    /// `current_mode_update`.
    Mode(String),
    /// `config_option_update`: the mode, when the options carry one.
    Options {
        mode: Option<String>,
    },
    /// A kind Eitri knows and does not use: `plan`, `available_commands_update`, `session_info_update`.
    Ignored(&'static str),
    /// A kind this build does not know, or a known kind without what it must carry.
    Unknown(String),
}

/// `session/update` params as the session they are for and the update itself.
pub fn parse_update(params: &Value) -> Option<(String, Update)> {
    let session = text_of(params, "sessionId")?;
    let update = params.get("update")?;
    let kind = text_of(update, "sessionUpdate")?;
    let chunk = |kind: ChunkKind| {
        let content = update.get("content");
        let text = content
            .filter(|c| text_of(c, "type").as_deref() == Some("text"))
            .and_then(|c| text_of(c, "text"));
        Update::Chunk {
            kind,
            message_id: text_of(update, "messageId"),
            text,
        }
    };
    let parsed = match kind.as_str() {
        "agent_message_chunk" => chunk(ChunkKind::Agent),
        "agent_thought_chunk" => chunk(ChunkKind::Thought),
        "user_message_chunk" => chunk(ChunkKind::User),
        "tool_call" => ToolCallFields::parse(update).map_or_else(|| Update::Unknown(kind.clone()), Update::ToolCall),
        "tool_call_update" => {
            ToolCallFields::parse(update).map_or_else(|| Update::Unknown(kind.clone()), Update::ToolCallUpdate)
        }
        "usage_update" => match (
            update.get("used").and_then(Value::as_u64),
            update.get("size").and_then(Value::as_u64),
        ) {
            (Some(used), Some(size)) => Update::Context { used, size },
            _ => Update::Unknown(kind.clone()),
        },
        "current_mode_update" => {
            text_of(update, "currentModeId").map_or_else(|| Update::Unknown(kind.clone()), Update::Mode)
        }
        "config_option_update" => Update::Options {
            mode: mode_from_options(update.get("configOptions")),
        },
        "plan" => Update::Ignored("plan"),
        "available_commands_update" => Update::Ignored("available_commands_update"),
        "session_info_update" => Update::Ignored("session_info_update"),
        _ => Update::Unknown(kind),
    };
    Some((session, parsed))
}

/// The kind of one permission option, as the agent labels it. Eitri trusts the label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    Other,
}

impl OptionKind {
    fn parse(word: &str) -> OptionKind {
        match word {
            "allow_once" => OptionKind::AllowOnce,
            "allow_always" => OptionKind::AllowAlways,
            "reject_once" => OptionKind::RejectOnce,
            "reject_always" => OptionKind::RejectAlways,
            _ => OptionKind::Other,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    pub kind: OptionKind,
}

/// `session/request_permission`.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequest {
    pub session_id: String,
    pub tool_call: ToolCallFields,
    pub options: Vec<PermissionOption>,
}

impl PermissionRequest {
    /// `None` when the request does not name a session and a call.
    pub fn parse(params: &Value) -> Option<PermissionRequest> {
        let options = params
            .get("options")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|o| {
                        Some(PermissionOption {
                            id: text_of(o, "optionId")?,
                            name: text_of(o, "name").unwrap_or_default(),
                            kind: OptionKind::parse(&text_of(o, "kind").unwrap_or_default()),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(PermissionRequest {
            session_id: text_of(params, "sessionId")?,
            tool_call: ToolCallFields::parse(params.get("toolCall")?)?,
            options,
        })
    }
}

/// `PromptResponse.usage`. Counts a server could get wrong (negative) read as zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptUsage {
    pub input: u64,
    pub output: u64,
    pub cached_read: u64,
    pub cached_write: u64,
}

/// What a `session/prompt` answer said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptResponse {
    pub stop_reason: Option<String>,
    pub usage: Option<PromptUsage>,
}

impl PromptResponse {
    pub fn parse(value: &Value) -> PromptResponse {
        let usage = value.get("usage").filter(|u| u.is_object()).map(|u| {
            let count = |key: &str| {
                u.get(key)
                    .and_then(Value::as_i64)
                    .map_or(0, |n| u64::try_from(n).unwrap_or(0))
            };
            PromptUsage {
                input: count("inputTokens"),
                output: count("outputTokens"),
                cached_read: count("cachedReadTokens"),
                cached_write: count("cachedWriteTokens"),
            }
        });
        PromptResponse {
            stop_reason: text_of(value, "stopReason"),
            usage,
        }
    }
}
