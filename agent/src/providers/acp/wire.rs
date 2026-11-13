//! The Agent Client Protocol (v1) wire, hand-written for exactly the methods Eitri uses.
//!
//! **Outbound** types serialize what Eitri sends. What Eitri tells the agent about itself is fixed here:
//! no file-system methods and no terminals are offered, so an agent has nothing to ask the client for
//! beyond a decision.
//!
//! **Inbound** values are read leniently, by hand, from `serde_json::Value`: agents add update kinds,
//! content types and fields between releases, and a field this build does not know must not end a turn.
//! An update kind Eitri does not know is [`Update::Unknown`]; a payload that lacks what a known kind must
//! carry is dropped by the caller with a log line, never guessed at. An unreadable mode report is kept
//! as [`Update::UnreadableMode`], so the caller can enforce the registered mode.
//!
//! Nothing under `_meta` and none of the `_auth/*` notifications is read: they carry the account's
//! details, which are not Eitri's to keep or to log.

use crate::external::{EditorServerSpec, ListedSession};
use crate::projection::SlashCommand;
use serde::{Deserialize, Serialize};
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
    pub const SESSION_SET_CONFIG_OPTION: &str = "session/set_config_option";
    pub const SESSION_LIST: &str = "session/list";

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

/// The `mcpServers` of `session/new` and `session/load`: the editor's tool server as one stdio entry, or no entry
/// when the session is given none. Eitri starts nothing itself; the agent runs the command. `env` stays empty: the
/// server reads its socket and key from its arguments.
fn mcp_servers(editor: Option<&EditorServerSpec>) -> Value {
    match editor {
        Some(server) => json!([{
            "name": server.name,
            "command": server.command.to_string_lossy(),
            "args": server.args,
            "env": [],
        }]),
        None => json!([]),
    }
}

/// `session/new`. The agent runs the tool servers its own configuration names, and the editor's if one is given.
pub fn session_new_params(cwd: &str, editor: Option<&EditorServerSpec>) -> Value {
    json!({ "cwd": cwd, "mcpServers": mcp_servers(editor) })
}

/// `session/load`; the tool servers go out again, as on `session/new`.
pub fn session_load_params(session_id: &str, cwd: &str, editor: Option<&EditorServerSpec>) -> Value {
    json!({ "sessionId": session_id, "cwd": cwd, "mcpServers": mcp_servers(editor) })
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

/// `session/set_config_option`: one of the agent's own selectable options set to one of the values it listed. Only
/// the model's and the effort's are ever sent; the mode is never set this way.
pub fn set_config_option_params(session_id: &str, config_id: &str, value: &str) -> Value {
    json!({ "sessionId": session_id, "configId": config_id, "value": value })
}

/// `session/list`, for the sessions of one project directory.
pub fn session_list_params(cwd: &str) -> Value {
    json!({ "cwd": cwd })
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
    /// `sessionCapabilities.list`: the agent lists its sessions (`session/list`).
    pub list_sessions: bool,
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
            list_sessions: capabilities
                .and_then(|c| c.get("sessionCapabilities"))
                .and_then(|c| c.get("list"))
                .is_some_and(|list| !list.is_null()),
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
    /// `modes.currentModeId` or the mode option, including a report that cannot be read.
    pub current_mode: Option<ModeReport>,
    /// `models.currentModelId`, for the panel's model line.
    pub current_model: Option<String>,
    /// The agent's selectable options, as it listed them; `None` when the answer carried no list.
    pub config_options: Option<Vec<ConfigOption>>,
}

impl SessionSetup {
    pub fn parse(value: &Value) -> SessionSetup {
        SessionSetup {
            session_id: text_of(value, "sessionId"),
            current_mode: setup_mode(
                value
                    .get("modes")
                    .filter(|m| !m.is_null())
                    .map(|m| text_of(m, "currentModeId").map_or(ModeReport::Unreadable, ModeReport::Known)),
                mode_from_options(value.get("configOptions")),
            ),
            current_model: value.get("models").and_then(|m| text_of(m, "currentModelId")),
            config_options: parse_config_options(value.get("configOptions")),
        }
    }
}

/// The mode a session's setup reports, from its `modes` and its config options: a readable report from
/// either wins over an unreadable one from the other, since an agent may fill only one of the two.
fn setup_mode(modes: Option<ModeReport>, options: Option<ModeReport>) -> Option<ModeReport> {
    match (modes, options) {
        (Some(known @ ModeReport::Known(_)), _) | (_, Some(known @ ModeReport::Known(_))) => Some(known),
        (modes, options) => modes.or(options),
    }
}

/// The current value of the option that holds the mode, in a list of config options. An option is the
/// mode's when its category or its id says `mode`.
pub fn mode_from_options(options: Option<&Value>) -> Option<ModeReport> {
    options?
        .as_array()?
        .iter()
        .find(|o| text_of(o, "category").as_deref() == Some("mode") || text_of(o, "id").as_deref() == Some("mode"))
        .map(|o| text_of(o, "currentValue").map_or(ModeReport::Unreadable, ModeReport::Known))
}

/// A reported mode that can be compared with the registration, or one that cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeReport {
    Known(String),
    Unreadable,
}

/// The most sessions one list keeps.
pub const MAX_LISTED_SESSIONS: usize = 50;
/// The longest title kept, in characters; a longer one is cut with `…`.
pub const MAX_SESSION_TITLE_CHARS: usize = 120;
/// The most entries of one answer that are read at all; the newest [`MAX_LISTED_SESSIONS`] of them stay.
pub const MAX_SESSIONS_READ: usize = 500;

/// The sessions in a `session/list` answer, newest first: `None` when it carries no list. Only a session whose id
/// Eitri can keep a record under is kept, one that names another directory than `cwd` is dropped (an agent may
/// ignore the directory it was asked for), and at most [`MAX_LISTED_SESSIONS`] stay.
pub fn parse_session_list(value: &Value, cwd: &str) -> Option<Vec<ListedSession>> {
    let list = value.get("sessions")?.as_array()?;
    let same_dir = |theirs: &str| theirs.trim_end_matches('/') == cwd.trim_end_matches('/');
    let mut out: Vec<ListedSession> = Vec::new();
    // Only the first entries are looked at, so a list of any length costs the same.
    for item in list.iter().take(MAX_SESSIONS_READ) {
        let Some(session_id) = text_of(item, "sessionId").filter(|id| {
            !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        }) else {
            continue;
        };
        if text_of(item, "cwd").is_some_and(|theirs| !same_dir(&theirs))
            || out.iter().any(|s| s.session_id == session_id)
        {
            continue;
        }
        out.push(ListedSession {
            session_id,
            // The chooser draws a title as it stands, so a character that changes how a line reads goes.
            title: text_of(item, "title")
                .map(|t| {
                    one_line(
                        &t.chars().filter(|c| !is_invisible_char(*c)).collect::<String>(),
                        MAX_SESSION_TITLE_CHARS,
                    )
                })
                .filter(|t| !t.is_empty()),
            updated_at_ms: text_of(item, "updatedAt").and_then(|t| rfc3339_ms(&t)),
        });
    }
    out.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
    out.truncate(MAX_LISTED_SESSIONS);
    Some(out)
}

/// An RFC 3339 time (`2026-11-02T05:00:00.000Z`, or with an offset) as milliseconds since the epoch; `None` for
/// anything else or a time before 1970.
pub fn rfc3339_ms(text: &str) -> Option<u64> {
    let (date, rest) = text.split_once(['T', 't'])?;
    let mut ymd = date.split('-');
    let (year, month, day): (i64, i64, i64) = (
        ymd.next()?.parse().ok()?,
        ymd.next()?.parse().ok()?,
        ymd.next()?.parse().ok()?,
    );
    // Years the epoch can count in milliseconds without the arithmetic below going out of range.
    if ymd.next().is_some() || !(1970..=9999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day)
    {
        return None;
    }
    let split = rest.find(['Z', 'z', '+', '-']).unwrap_or(rest.len());
    let (clock, zone) = rest.split_at(split);
    let mut hms = clock.split(':');
    let hour: i64 = hms.next()?.parse().ok()?;
    let minute: i64 = hms.next()?.parse().ok()?;
    let seconds = hms.next().unwrap_or("0");
    let (whole, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    if !fraction.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let second: i64 = whole.parse().ok()?;
    let millis: i64 = format!("{fraction:0<3}")[..3].parse().ok()?;
    if hms.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let offset_minutes = match zone {
        "" | "Z" | "z" => 0,
        _ => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let (h, m) = zone[1..].split_once(':')?;
            let (h, m): (i64, i64) = (h.parse().ok()?, m.parse().ok()?);
            if !(0..=23).contains(&h) || !(0..=59).contains(&m) {
                return None;
            }
            sign * (h * 60 + m)
        }
    };
    // Days from the civil date (proleptic Gregorian), as in Howard Hinnant's algorithm.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let total = ((days * 24 + hour) * 60 + minute - offset_minutes) * 60 + second;
    u64::try_from(total * 1000 + millis).ok()
}

/// The most options one list keeps.
pub const MAX_CONFIG_OPTIONS: usize = 32;
/// The most choices one option keeps; the rest are dropped.
pub const MAX_CONFIG_CHOICES: usize = 200;
/// The longest option id or choice value kept, in characters. A longer one is dropped, not cut: a cut value
/// would be sent back as a value the agent never listed.
pub const MAX_CONFIG_VALUE_CHARS: usize = 128;
/// The longest choice name kept, in characters; a longer one is cut with `…`.
pub const MAX_CONFIG_NAME_CHARS: usize = 80;

/// One choice of a selectable option, in the agent's words: untrusted text, folded to one line and bounded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigChoice {
    /// What `session/set_config_option` takes back.
    pub value: String,
    /// The name shown; the value when the agent gave none.
    pub name: String,
    pub description: String,
}

/// One selectable option the agent reports (`configOptions`), as far as Eitri reads it: its id, its category,
/// the value it holds and the values it offers. An option that is not a select with a string value is not kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigOption {
    pub id: String,
    pub category: Option<String>,
    pub current: String,
    pub choices: Vec<ConfigChoice>,
}

impl ConfigOption {
    /// The option that holds the mode, which Eitri sets once and never through this list
    /// ([`mode_from_options`] names it the same way).
    pub fn is_mode(&self) -> bool {
        self.category.as_deref() == Some("mode") || self.id == "mode"
    }

    /// The option that holds the model.
    pub fn is_model(&self) -> bool {
        !self.is_mode() && (self.category.as_deref() == Some("model") || self.id == "model")
    }

    /// The option that holds the reasoning effort.
    pub fn is_effort(&self) -> bool {
        !self.is_mode()
            && (self.category.as_deref() == Some("thought_level")
                || matches!(self.id.as_str(), "thought_level" | "reasoning_effort" | "effort"))
    }

    /// The choice that `wanted` names: its value exactly, else the one choice whose value or name matches in
    /// any letter case. `None` when nothing matches or several do.
    pub fn choice_named(&self, wanted: &str) -> Option<&ConfigChoice> {
        if let Some(exact) = self.choices.iter().find(|c| c.value == wanted) {
            return Some(exact);
        }
        let mut loose = self
            .choices
            .iter()
            .filter(|c| c.value.eq_ignore_ascii_case(wanted) || c.name.eq_ignore_ascii_case(wanted));
        match (loose.next(), loose.next()) {
            (Some(only), None) => Some(only),
            _ => None,
        }
    }
}

/// A value or id Eitri will compare and send back: non-empty, short and showing nothing hidden.
fn plain_token(text: &str) -> bool {
    !text.is_empty() && text.chars().count() <= MAX_CONFIG_VALUE_CHARS && !text.chars().any(is_hidden_char)
}

/// The choices of one option's `options` array, a flat list or groups of them, bounded and without repeats.
fn parse_choices(list: &[Value], out: &mut Vec<ConfigChoice>) {
    for item in list {
        if out.len() >= MAX_CONFIG_CHOICES {
            return;
        }
        if let Some(group) = item.get("options").and_then(Value::as_array) {
            parse_choices(group, out);
            continue;
        }
        let Some(value) = text_of(item, "value").filter(|v| plain_token(v)) else {
            continue;
        };
        if out.iter().any(|c| c.value == value) {
            continue;
        }
        let name = text_of(item, "name")
            .map(|n| one_line(&n, MAX_CONFIG_NAME_CHARS))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| value.clone());
        let description = text_of(item, "description")
            .map(|d| one_line(&d, MAX_COMMAND_TEXT_CHARS))
            .unwrap_or_default();
        out.push(ConfigChoice {
            value,
            name,
            description,
        });
    }
}

/// The selectable options in a `configOptions` value: `None` when it is not a list, so an answer that carries
/// none leaves what was known as it was. At most [`MAX_CONFIG_OPTIONS`] are kept.
pub fn parse_config_options(options: Option<&Value>) -> Option<Vec<ConfigOption>> {
    let list = options?.as_array()?;
    let is_mode = |item: &Value| {
        text_of(item, "category").as_deref() == Some("mode") || text_of(item, "id").as_deref() == Some("mode")
    };
    // `session/set_config_option` names an option by its id alone, so an id the agent also gives its mode option
    // is never offered for anything else: setting it could set the mode.
    let mode_ids: Vec<String> = list
        .iter()
        .filter(|item| is_mode(item))
        .filter_map(|item| text_of(item, "id"))
        .collect();
    let mut out: Vec<ConfigOption> = Vec::new();
    for item in list {
        if out.len() >= MAX_CONFIG_OPTIONS {
            break;
        }
        if item.get("type").is_some_and(|t| t.as_str() != Some("select")) {
            continue;
        }
        let Some(id) = text_of(item, "id").filter(|id| plain_token(id)) else {
            continue;
        };
        if !is_mode(item) && mode_ids.contains(&id) {
            continue;
        }
        let Some(current) = text_of(item, "currentValue") else {
            continue;
        };
        if out.iter().any(|o| o.id == id) {
            continue;
        }
        let mut choices = Vec::new();
        if let Some(offered) = item.get("options").and_then(Value::as_array) {
            parse_choices(offered, &mut choices);
        }
        out.push(ConfigOption {
            id,
            category: text_of(item, "category"),
            current,
            choices,
        });
    }
    Some(out)
}

/// A tool call as an update or a permission request describes it. Every field but the id is optional,
/// because an update carries only what changed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallFields {
    pub id: String,
    pub title: Option<String>,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub raw_input: Option<Value>,
    pub raw_output: Option<Value>,
    pub locations: Option<Value>,
    pub content: Vec<Value>,
    #[serde(default)]
    pub content_given: bool,
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
            // Only a list replaces the earlier content: a `null` (as serializers write an absent field) or any
            // other value leaves it as it was, as an absent field does.
            content_given: value.get("content").is_some_and(Value::is_array),
        })
    }

    /// Applies a later update about the same call: a field it carries replaces the earlier one, and one it
    /// leaves out stays. A carried `content` list replaces the earlier list as a whole, even when empty.
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
        if update.content_given {
            self.content.clone_from(&update.content);
            self.content_given = true;
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
    /// A mode update that carries no readable mode id.
    UnreadableMode,
    /// `config_option_update`: the mode, when the options carry one, and the whole list of options.
    Options {
        mode: Option<String>,
        options: Option<Vec<ConfigOption>>,
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
        "current_mode_update" => text_of(update, "currentModeId").map_or(Update::UnreadableMode, Update::Mode),
        "config_option_update" => match mode_from_options(update.get("configOptions")) {
            Some(ModeReport::Known(mode)) => Update::Options {
                mode: Some(mode),
                options: parse_config_options(update.get("configOptions")),
            },
            Some(ModeReport::Unreadable) => Update::UnreadableMode,
            None => Update::Options {
                mode: None,
                options: parse_config_options(update.get("configOptions")),
            },
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
/// the table of format and default-ignorable ranges.
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
        (0x2060, 0x206F),
        (0xFEFF, 0xFEFF),
        (0xFFF0, 0xFFFB),
        (0x110BD, 0x110BD),
        (0x110CD, 0x110CD),
        (0x13430, 0x1343F),
        (0x1BCA0, 0x1BCA3),
        (0x1D173, 0x1D17A),
        (0xE0000, 0xE0FFF),
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
    ];
    let code = c as u32;
    let within = |table: &[(u32, u32)]| table.iter().any(|&(from, to)| (from..=to).contains(&code));
    within(FORMAT) || within(BLANK)
}

/// Cuts `text` to `max` characters with `…`, with control characters (newlines included) turned into
/// spaces and runs of whitespace folded, so a description is always one plain line. Other invisible
/// characters are kept: the page draws them as visible escapes (`revealHidden`) rather than hiding them.
pub fn one_line(text: &str, max: usize) -> String {
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
    #[test]
    fn a_null_modes_falls_back_to_the_mode_option() {
        let setup = SessionSetup::parse(&json!({
            "sessionId": "s1",
            "modes": null,
            "configOptions": [{"id": "mode", "currentValue": "workspace-write"}]
        }));
        assert_eq!(
            setup.current_mode,
            Some(ModeReport::Known("workspace-write".to_string()))
        );
        // A readable report from either source wins over an unreadable one from the other.
        let setup = SessionSetup::parse(&json!({
            "sessionId": "s1",
            "modes": {"availableModes": []},
            "configOptions": [{"id": "mode", "currentValue": "workspace-write"}]
        }));
        assert_eq!(
            setup.current_mode,
            Some(ModeReport::Known("workspace-write".to_string()))
        );
        let setup = SessionSetup::parse(&json!({"sessionId": "s1", "modes": {"availableModes": []}}));
        assert_eq!(setup.current_mode, Some(ModeReport::Unreadable));
        let setup = SessionSetup::parse(&json!({"sessionId": "s1", "modes": null}));
        assert_eq!(setup.current_mode, None);
    }

    #[test]
    fn a_null_content_is_not_a_carried_list() {
        let mut kept = ToolCallFields::parse(&json!({
            "toolCallId": "c1",
            "content": [{"type": "content", "content": {"type": "text", "text": "working"}}]
        }))
        .unwrap();
        let update =
            ToolCallFields::parse(&json!({"toolCallId": "c1", "status": "completed", "content": null})).unwrap();
        kept.merge(&update);
        assert_eq!(kept.content.len(), 1, "a null content keeps the earlier list");
    }

    use super::*;

    #[test]
    fn mode_updates_distinguish_unreadable_reports_from_absent_options() {
        for report in [
            json!({"sessionUpdate": "current_mode_update"}),
            json!({"sessionUpdate": "current_mode_update", "currentModeId": 5}),
            json!({"sessionUpdate": "config_option_update", "configOptions": [
                {"id": "mode", "currentValue": 5}
            ]}),
            json!({"sessionUpdate": "config_option_update", "configOptions": [
                {"id": "agent-mode", "category": "mode"}
            ]}),
        ] {
            let (_, update) = parse_update(&json!({"sessionId": "s", "update": report})).unwrap();
            assert_eq!(update, Update::UnreadableMode);
        }
        let (_, update) = parse_update(&json!({"sessionId": "s", "update": {
            "sessionUpdate": "config_option_update", "configOptions": [
                {"id": "model", "currentValue": 5}
            ]
        }}))
        .unwrap();
        assert_eq!(
            update,
            Update::Options {
                mode: None,
                options: Some(vec![])
            },
            "an option without a readable value is not kept"
        );
    }

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
    fn unassigned_default_ignorables_are_invisible_and_refused_in_names() {
        for code in [
            0x2065, 0xfff0, 0xfff8, 0xe0000, 0xe0002, 0xe001f, 0xe0080, 0xe00ff, 0xe01f0, 0xe0fff,
        ] {
            let c = char::from_u32(code).unwrap();
            assert!(is_invisible_char(c), "U+{code:X}");
            assert!(is_hidden_char(c));
            let list = json!([{"name": format!("a{c}b")}]);
            assert!(parse_commands(list.as_array().unwrap()).is_empty());
        }
    }

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

    #[test]
    fn rfc3339_times_read_as_epoch_milliseconds() {
        assert_eq!(rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_ms("2026-11-02T05:00:00.250Z"), Some(1_793_595_600_250));
        assert_eq!(rfc3339_ms("2026-11-02T13:00:00+08:00"), Some(1_793_595_600_000));
        assert_eq!(rfc3339_ms("2026-11-02T05:00:00-01:30"), Some(1_793_601_000_000));
        assert_eq!(rfc3339_ms("2000-02-29T00:00:00Z"), Some(951_782_400_000), "a leap day");
        for bad in [
            "",
            "yesterday",
            "2026-13-01T00:00:00Z",
            "2026-11-02",
            "2026-11-02T25:00:00Z",
            "1969-12-31T23:59:59Z",
            "2026-11-02T05:00:00.é9Z",
        ] {
            assert_eq!(rfc3339_ms(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_time_out_of_range_is_no_time_and_never_overflows() {
        for bad in [
            "2026-11-02T05:00:00+9223372036854775807:00",
            "2026-11-02T05:00:00+24:00",
            "2026-11-02T05:00:00+01:60",
            "99999999999999999-11-02T05:00:00Z",
            "10000-01-01T00:00:00Z",
        ] {
            assert_eq!(rfc3339_ms(bad), None, "{bad:?}");
        }
        assert_eq!(rfc3339_ms("9999-12-31T23:59:59Z"), Some(253_402_300_799_000));
    }

    #[test]
    fn a_huge_session_list_is_read_in_bounded_work() {
        let many: Vec<Value> = (0..100_000)
            .map(|n| json!({"sessionId": format!("s{n}"), "updatedAt": "2026-11-02T00:00:00Z"}))
            .collect();
        let listed = parse_session_list(&json!({"sessions": many}), "/work").unwrap();
        assert_eq!(listed.len(), MAX_LISTED_SESSIONS);
    }

    #[test]
    fn an_id_the_agent_also_gives_its_mode_is_not_offered_for_anything_else() {
        let options = json!([
            {"id": "shared", "category": "model", "type": "select", "currentValue": "a", "options": [{"value": "a"}]},
            {"id": "shared", "category": "mode", "type": "select", "currentValue": "ask", "options": [{"value": "ask"}]},
            {"id": "model", "category": "model", "type": "select", "currentValue": "m", "options": [{"value": "m"}]}
        ]);
        let parsed = parse_config_options(Some(&options)).unwrap();
        let ids: Vec<_> = parsed
            .iter()
            .map(|o| (o.id.as_str(), o.is_mode(), o.is_model()))
            .collect();
        assert_eq!(ids, [("shared", true, false), ("model", false, true)]);
    }
}
