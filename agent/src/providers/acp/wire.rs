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

use crate::projection::SlashCommand;
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
    /// `available_commands_update`: the whole list of commands the agent accepts, bounded and cleaned
    /// (`parse_commands`).
    Commands(Vec<SlashCommand>),
    /// A kind Eitri knows and does not use: `plan`, `session_info_update`.
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
        "available_commands_update" => match update.get("availableCommands").and_then(Value::as_array) {
            Some(list) => Update::Commands(parse_commands(list)),
            None => Update::Unknown(kind.clone()),
        },
        "session_info_update" => Update::Ignored("session_info_update"),
        _ => Update::Unknown(kind),
    };
    Some((session, parsed))
}

/// The most commands one list keeps; the rest are dropped.
pub const MAX_COMMANDS: usize = 200;
/// The longest command name kept, in characters. A longer name is dropped, not cut: a cut name would
/// complete into a command the agent does not have.
pub const MAX_COMMAND_NAME_CHARS: usize = 64;
/// The longest description kept, in characters; a longer one is cut with `…`.
pub const MAX_COMMAND_TEXT_CHARS: usize = 200;
/// The longest argument hint kept, in characters; a longer one is cut with `…`. The menu draws a hint
/// beside the name on one line, so a long one would only push the description out of view.
pub const MAX_COMMAND_HINT_CHARS: usize = 60;

/// Whether `c` is a character that changes how a line reads without showing as anything itself: a
/// control, a format character (bidi overrides and isolates, zero-width characters, the byte-order mark,
/// the soft hyphen, tag characters), any space, or a character that renders blank. The same set as the
/// page's `revealHidden.ts`, which both `cleanCommands` there and this module's name check follow, so a
/// name one side drops the other drops too.
fn is_hidden_char(c: char) -> bool {
    c.is_control() || c.is_whitespace() || is_invisible_char(c)
}

/// The part of [`is_hidden_char`] that is neither a control nor a space: a format character or one that
/// renders blank. Public so another check of text a user reads (one that allows ordinary spaces) uses the
/// same table rather than a copy that drifts. Rust's standard library has no format-character test, hence
/// the table (Unicode general category `Cf`).
pub fn is_invisible_char(c: char) -> bool {
    const FORMAT: &[(u32, u32)] = &[
        (0xAD, 0xAD),
        (0x600, 0x605),
        (0x61C, 0x61C),
        (0x6DD, 0x6DD),
        (0x70F, 0x70F),
        (0x890, 0x891),
        (0x8E2, 0x8E2),
        (0x180E, 0x180E),
        (0x200B, 0x200F),
        (0x202A, 0x202E),
        (0x2060, 0x2064),
        (0x2066, 0x206F),
        (0xFEFF, 0xFEFF),
        (0xFFF9, 0xFFFB),
        (0x110BD, 0x110BD),
        (0x110CD, 0x110CD),
        (0x13430, 0x1343F),
        (0x1BCA0, 0x1BCA3),
        (0x1D173, 0x1D17A),
        (0xE0001, 0xE0001),
        (0xE0020, 0xE007F),
    ];
    // Blank-rendering characters outside the classes above: the combining grapheme joiner, the Hangul and
    // Braille fillers, the Khmer inherent vowels, the Mongolian and ordinary variation selectors.
    const BLANK: &[(u32, u32)] = &[
        (0x34F, 0x34F),
        (0x115F, 0x1160),
        (0x17B4, 0x17B5),
        (0x180B, 0x180F),
        (0x2800, 0x2800),
        (0x3164, 0x3164),
        (0xFE00, 0xFE0F),
        (0xFFA0, 0xFFA0),
        (0xE0100, 0xE01EF),
    ];
    let code = c as u32;
    let within = |table: &[(u32, u32)]| table.iter().any(|&(from, to)| (from..=to).contains(&code));
    within(FORMAT) || within(BLANK)
}

/// Cuts `text` to `max` characters with `…`, with control characters (newlines included) turned into
/// spaces and runs of whitespace folded, so a description is always one plain line. Other invisible
/// characters are kept: the page draws them as visible escapes (`revealHidden`) rather than hiding them.
fn one_line(text: &str, max: usize) -> String {
    let folded = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if folded.chars().count() <= max {
        return folded;
    }
    let mut cut: String = folded.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// The `availableCommands` of an `available_commands_update`, as the panel may draw them. The agent's
/// text is untrusted: a name that is empty, holds whitespace, a control character or an invisible or
/// direction-changing one (`is_hidden_char`), or is over [`MAX_COMMAND_NAME_CHARS`] is dropped (one leading
/// `/` is tolerated, as some agents send it, and a name still starting with `/` after that is dropped, so
/// the page, which strips one `/` again, never reads a name differently); a repeated name keeps its first
/// entry; descriptions are folded to one line and cut at [`MAX_COMMAND_TEXT_CHARS`], hints at
/// [`MAX_COMMAND_HINT_CHARS`]; at most [`MAX_COMMANDS`] are kept.
pub fn parse_commands(list: &[Value]) -> Vec<SlashCommand> {
    bound_commands(list.iter().filter_map(|item| {
        Some(RawCommand {
            name: text_of(item, "name")?,
            description: text_of(item, "description"),
            hint: item.get("input").and_then(|input| text_of(input, "hint")),
        })
    }))
}

/// One slash command as an agent sent it, before [`bound_commands`] has read it.
pub struct RawCommand {
    pub name: String,
    pub description: Option<String>,
    pub hint: Option<String>,
}

/// An agent's command list held to the rules [`parse_commands`] states, whichever wire it came on: the
/// one place those rules are written, so every agent's list reaches the page bounded the same way.
pub fn bound_commands(list: impl IntoIterator<Item = RawCommand>) -> Vec<SlashCommand> {
    let mut out: Vec<SlashCommand> = Vec::new();
    for item in list {
        if out.len() >= MAX_COMMANDS {
            break;
        }
        let raw = item.name;
        let name = raw.strip_prefix('/').unwrap_or(&raw);
        if name.is_empty()
            || name.starts_with('/')
            || name.chars().count() > MAX_COMMAND_NAME_CHARS
            || name
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || is_hidden_char(c))
            || out.iter().any(|c| c.name == name)
        {
            continue;
        }
        let hint = item
            .hint
            .map(|hint| one_line(&hint, MAX_COMMAND_HINT_CHARS))
            .filter(|hint| !hint.is_empty());
        out.push(SlashCommand {
            name: name.to_string(),
            description: item
                .description
                .map(|d| one_line(&d, MAX_COMMAND_TEXT_CHARS))
                .unwrap_or_default(),
            hint,
        });
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_read_bounded_and_cleaned() {
        let long_name = "n".repeat(MAX_COMMAND_NAME_CHARS + 1);
        let long_text = "d".repeat(MAX_COMMAND_TEXT_CHARS + 50);
        let list = json!([
            {"name": "web", "description": "Search the web", "input": {"hint": "query"}},
            {"name": "/test", "description": "Run\ntests\u{7}  now"},
            {"name": "web", "description": "a repeat"},
            {"name": "two words", "description": "dropped"},
            {"name": "", "description": "dropped"},
            {"name": "tab\there", "description": "dropped"},
            {"name": long_name, "description": "dropped"},
            {"description": "no name"},
            {"name": "long", "description": long_text, "input": {"hint": 3}},
            {"name": "bare"},
        ]);
        let commands = parse_commands(list.as_array().unwrap());
        let names: Vec<_> = commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["web", "test", "long", "bare"]);
        assert_eq!(commands[0].hint.as_deref(), Some("query"));
        assert_eq!(
            commands[1].description, "Run tests now",
            "control characters and runs of space fold to one line"
        );
        assert_eq!(commands[2].description.chars().count(), MAX_COMMAND_TEXT_CHARS);
        assert!(commands[2].description.ends_with('…'));
        assert_eq!(commands[2].hint, None, "a hint that is not text is none");
        assert_eq!(commands[3].description, "");
    }

    /// The characters the page's `slashCommands.test.ts` checks for the same drop: a name holding any of them
    /// is gone on both sides, so a list one side keeps is never one the other drops.
    const HIDDEN_NAME_CHARS: &[char] = &[
        '\u{202E}',
        '\u{200B}',
        '\u{2066}',
        '\u{2067}',
        '\u{2068}',
        '\u{2069}',
        '\u{00AD}',
        '\u{FEFF}',
        '\u{E0041}',
        '\u{200D}',
        '\u{2060}',
        '\u{061C}',
        '\u{3164}',
        '\u{FE0F}',
        '\u{00A0}',
    ];

    #[test]
    fn a_name_with_an_invisible_or_direction_character_is_dropped() {
        for c in HIDDEN_NAME_CHARS {
            let list = json!([{"name": format!("te{c}st")}, {"name": format!("{c}lead")}, {"name": "ok"}]);
            let names: Vec<_> = parse_commands(list.as_array().unwrap())
                .into_iter()
                .map(|c| c.name)
                .collect();
            assert_eq!(names, ["ok"], "U+{:04X}", *c as u32);
        }
        // Letters of a right-to-left script are ordinary text.
        let list = json!([{"name": "שלום"}]);
        assert_eq!(parse_commands(list.as_array().unwrap())[0].name, "שלום");
    }

    #[test]
    fn a_name_holding_whitespace_or_a_control_character_is_dropped() {
        let list = json!([
            {"name": "two words"},
            {"name": " lead"},
            {"name": "trail "},
            {"name": "tab\there"},
            {"name": "line\nbreak"},
            {"name": "wide\u{3000}space"},
            {"name": "bell\u{7}"},
            {"name": "ok"}
        ]);
        let names: Vec<_> = parse_commands(list.as_array().unwrap())
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["ok"]);
    }

    #[test]
    fn one_leading_slash_is_stripped_and_a_second_drops_the_name() {
        let list = json!([{"name": "//x"}, {"name": "/y"}, {"name": "/"}]);
        let names: Vec<_> = parse_commands(list.as_array().unwrap())
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["y"]);
    }

    #[test]
    fn a_hint_is_cut_shorter_than_a_description() {
        let list = json!([{"name": "a", "description": "d".repeat(100), "input": {"hint": "h".repeat(100)}}]);
        let command = &parse_commands(list.as_array().unwrap())[0];
        assert_eq!(command.hint.as_deref().unwrap().chars().count(), MAX_COMMAND_HINT_CHARS);
        assert!(command.hint.as_deref().unwrap().ends_with('…'));
        assert_eq!(command.description.chars().count(), 100);
    }

    #[test]
    fn the_list_is_capped() {
        let list: Vec<Value> = (0..MAX_COMMANDS + 20)
            .map(|i| json!({"name": format!("c{i}")}))
            .collect();
        assert_eq!(parse_commands(&list).len(), MAX_COMMANDS);
    }

    #[test]
    fn an_update_without_a_list_is_unknown_and_with_one_is_commands() {
        let params = |update: Value| json!({"sessionId": "s", "update": update});
        let (_, bare) = parse_update(&params(json!({"sessionUpdate": "available_commands_update"}))).unwrap();
        assert_eq!(bare, Update::Unknown("available_commands_update".into()));
        let (_, empty) = parse_update(&params(
            json!({"sessionUpdate": "available_commands_update", "availableCommands": []}),
        ))
        .unwrap();
        assert_eq!(empty, Update::Commands(vec![]));
    }
}
