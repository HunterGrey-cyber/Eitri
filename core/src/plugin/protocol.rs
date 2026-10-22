//! The page bridge, version 1. A page message parses into a closed set of verbs; an unknown key, verb or
//! version is a `bad_request`, and what the host sends back is written by the types below, so both
//! directions share one convention: `v`, snake_case keys.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::manifest::Capability;

pub const VERSION: u32 = 1;
/// The largest page message the host acts on.
pub const MAX_PAGE_MESSAGE: usize = 256 * 1024;
/// Past this a message is not even parsed for its id; it is logged and dropped.
pub const MAX_READ_FOR_ID: usize = 4 * 1024 * 1024;

/// Installed at document start in the page's main world; defines `window.eitriPlugin`.
pub const BRIDGE_JS: &str = include_str!("bridge.js");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    NotDeclared,
    OutsideRoots,
    NoSuchTask,
    BadVar,
    Busy,
    EditorUnavailable,
    EditorTimeout,
    TooLarge,
    Io,
}

/// `{}`, and nothing else.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoArgs {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnOff {
    On,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FocusTarget {
    #[serde(rename = "editor")]
    Editor,
    /// The plugin's own module.
    #[serde(rename = "self")]
    Page,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskStart {
    pub task: String,
    pub vars: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCancel {
    pub run: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorOpen {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub col: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorAssist {
    pub root: String,
    pub diagnostics: OnOff,
    pub inlay_hints: OnOff,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutFocus {
    pub target: FocusTarget,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateRead {
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateWrite {
    pub key: String,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogAppend {
    pub stream: String,
    pub items: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogRead {
    pub stream: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

/// One row of a page's key table: an action of its own, the keys that run it, and the text the `?` sheet
/// shows for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyRowArg {
    pub action: String,
    pub keys: Vec<String>,
    pub context: String,
    pub desc: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeysDeclareArgs {
    pub rows: Vec<KeyRowArg>,
}

/// The longest link target a page may hand to `editor.open_link`.
pub const MAX_LINK_TARGET: usize = 4096;

/// `editor.reveal`: put a source line of the followed buffer at the top of its window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorReveal {
    pub buf: u64,
    pub line: u32,
    pub token: u64,
}

/// `editor.jump`: move the followed window's cursor, and with `focus` the keys, to a source line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorJump {
    pub buf: u64,
    pub line: u32,
    pub col: u32,
    pub focus: bool,
}

/// `editor.open_link`: a link of the followed document, as written, to open in the editor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorOpenLink {
    pub buf: u64,
    pub target: String,
}

fn line_ok(line: u32) -> Result<(), String> {
    if line < 1 {
        Err("line must be at least 1".to_string())
    } else {
        Ok(())
    }
}

impl EditorReveal {
    pub fn check(&self) -> Result<(), String> {
        line_ok(self.line)
    }
}

impl EditorJump {
    pub fn check(&self) -> Result<(), String> {
        line_ok(self.line)
    }
}

impl EditorOpenLink {
    pub fn check(&self) -> Result<(), String> {
        if self.target.len() > MAX_LINK_TARGET {
            return Err(format!("target is over {MAX_LINK_TARGET} bytes"));
        }
        if self.target.chars().any(char::is_control) {
            return Err("target holds a control character".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verb", content = "args")]
pub enum Verb {
    #[serde(rename = "host.hello")]
    HostHello(NoArgs),
    #[serde(rename = "task.start")]
    TaskStart(TaskStart),
    #[serde(rename = "task.cancel")]
    TaskCancel(TaskCancel),
    #[serde(rename = "editor.open")]
    EditorOpen(EditorOpen),
    #[serde(rename = "editor.assist")]
    EditorAssist(EditorAssist),
    #[serde(rename = "editor.assist_clear")]
    EditorAssistClear(NoArgs),
    #[serde(rename = "layout.focus")]
    LayoutFocus(LayoutFocus),
    #[serde(rename = "state.read")]
    StateRead(StateRead),
    #[serde(rename = "state.write")]
    StateWrite(StateWrite),
    #[serde(rename = "log.append")]
    LogAppend(LogAppend),
    #[serde(rename = "log.read")]
    LogRead(LogRead),
    #[serde(rename = "keys.declare")]
    KeysDeclare(KeysDeclareArgs),
    #[serde(rename = "editor.follow")]
    EditorFollow(NoArgs),
    #[serde(rename = "editor.reveal")]
    EditorReveal(EditorReveal),
    #[serde(rename = "editor.jump")]
    EditorJump(EditorJump),
    #[serde(rename = "editor.open_link")]
    EditorOpenLink(EditorOpenLink),
}

impl Verb {
    /// The capability a verb needs. `host.hello` needs none, and neither does `keys.declare`: it grants no
    /// power, only records text the window shows.
    pub fn capability(&self) -> Option<Capability> {
        match self {
            Verb::HostHello(_) | Verb::KeysDeclare(_) => None,
            Verb::EditorFollow(_) | Verb::EditorReveal(_) | Verb::EditorJump(_) | Verb::EditorOpenLink(_) => {
                Some(Capability::EditorFollow)
            }
            Verb::TaskStart(_) | Verb::TaskCancel(_) => Some(Capability::Tasks),
            Verb::EditorOpen(_) => Some(Capability::EditorOpen),
            Verb::EditorAssist(_) | Verb::EditorAssistClear(_) => Some(Capability::EditorAssist),
            Verb::LayoutFocus(_) => Some(Capability::LayoutFocus),
            Verb::StateRead(_) | Verb::StateWrite(_) | Verb::LogAppend(_) | Verb::LogRead(_) => Some(Capability::State),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Verb::HostHello(_) => "host.hello",
            Verb::TaskStart(_) => "task.start",
            Verb::TaskCancel(_) => "task.cancel",
            Verb::EditorOpen(_) => "editor.open",
            Verb::EditorAssist(_) => "editor.assist",
            Verb::EditorAssistClear(_) => "editor.assist_clear",
            Verb::LayoutFocus(_) => "layout.focus",
            Verb::StateRead(_) => "state.read",
            Verb::StateWrite(_) => "state.write",
            Verb::LogAppend(_) => "log.append",
            Verb::LogRead(_) => "log.read",
            Verb::KeysDeclare(_) => "keys.declare",
            Verb::EditorFollow(_) => "editor.follow",
            Verb::EditorReveal(_) => "editor.reveal",
            Verb::EditorJump(_) => "editor.jump",
            Verb::EditorOpenLink(_) => "editor.open_link",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub id: u64,
    pub verb: Verb,
}

impl Request {
    /// The message as the page sent it.
    pub fn to_value(&self) -> Value {
        let mut value = serde_json::to_value(&self.verb).unwrap_or(Value::Null);
        if let Some(map) = value.as_object_mut() {
            map.insert("v".into(), Value::from(VERSION));
            map.insert("id".into(), Value::from(self.id));
        }
        value
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Inbound {
    Request(Request),
    /// Not acted on. With `re`, the page is told why; without one, there is nothing to answer.
    Refused {
        re: Option<u64>,
        code: ErrorCode,
        message: String,
    },
}

fn refused(re: Option<u64>, code: ErrorCode, message: impl Into<String>) -> Inbound {
    Inbound::Refused {
        re,
        code,
        message: message.into(),
    }
}

pub fn parse_request(raw: &str) -> Inbound {
    if raw.len() > MAX_READ_FOR_ID {
        return refused(
            None,
            ErrorCode::TooLarge,
            format!("a page message of {} bytes", raw.len()),
        );
    }
    let value: Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(e) => return refused(None, ErrorCode::BadRequest, format!("not JSON: {e}")),
    };
    let Value::Object(mut map) = value else {
        return refused(None, ErrorCode::BadRequest, "not a JSON object");
    };
    let re = map.get("id").and_then(Value::as_u64);
    if raw.len() > MAX_PAGE_MESSAGE {
        return refused(
            re,
            ErrorCode::TooLarge,
            format!("{} bytes; a message may be at most 256 KiB", raw.len()),
        );
    }
    let Some(id) = re else {
        return refused(None, ErrorCode::BadRequest, "id must be a non-negative integer");
    };
    if let Some(key) = map.keys().find(|k| !matches!(k.as_str(), "v" | "id" | "verb" | "args")) {
        return refused(re, ErrorCode::BadRequest, format!("unknown key {key:?}"));
    }
    if map.get("v").and_then(Value::as_u64) != Some(u64::from(VERSION)) {
        return refused(
            re,
            ErrorCode::BadRequest,
            format!("the protocol version must be {VERSION}"),
        );
    }
    let verb = map.remove("verb").unwrap_or(Value::Null);
    let args = map.remove("args").unwrap_or(Value::Null);
    match serde_json::from_value::<Verb>(serde_json::json!({ "verb": verb, "args": args })) {
        Ok(verb) => Inbound::Request(Request { id, verb }),
        Err(e) => refused(re, ErrorCode::BadRequest, e.to_string()),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireError {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub v: u32,
    pub re: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<WireError>,
}

impl Reply {
    pub fn ok(re: u64, result: Value) -> Reply {
        Reply {
            v: VERSION,
            re,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(re: u64, code: ErrorCode, message: &str) -> Reply {
        Reply {
            v: VERSION,
            re,
            ok: false,
            result: None,
            error: Some(WireError {
                code,
                message: message.to_owned(),
            }),
        }
    }

    pub fn to_text(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostEvent {
    pub v: u32,
    pub event: String,
    pub data: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamName {
    Stdout,
    Stderr,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// `task.output`. The two counters are what was dropped since the previous `task.output` of this run, and
/// are left out when zero.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskOutput {
    pub run: u64,
    pub stream: StreamName,
    pub lines: Vec<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub dropped_lines: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub dropped_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitStatus {
    Exited,
    Killed,
    Timeout,
    Cancelled,
    SpawnFailed,
}

/// `task.exit`. The counters are the run's totals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskExit {
    pub run: u64,
    pub status: ExitStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    pub dropped_lines: u64,
    pub dropped_bytes: u64,
}

/// `focus`: whether the page's module holds the keys of a window that is itself active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FocusEvent {
    pub has_keys: bool,
}

/// `keys_config`: the page's keyboard settings, complete each time any of them changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeysConfig {
    /// How long, in milliseconds, a key sequence waits for its next key.
    pub timeoutlen: u32,
    /// The window's prefix key as a person reads it (`Ctrl+b`), for the page's own key help.
    pub prefix: String,
    /// The key that shows this plugin's module after the prefix, or `null` when it has none. Required on
    /// the wire: a page must never mistake an older message without it for "no key".
    #[serde(deserialize_with = "Option::deserialize")]
    pub module_key: Option<String>,
}

/// `theme`: the complete set of appearance variables for the page's root, each a checked `--nv-*` value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeEvent {
    pub vars: BTreeMap<String, String>,
}

pub const EVENT_EDITOR_VIEW: &str = "editor.view";
pub const EVENT_EDITOR_TEXT: &str = "editor.text";
pub const EVENT_EDITOR_CLOSED: &str = "editor.closed";

/// nvim's cursor in the followed window: `line` 1-based, `col` nvim's 0-based byte column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    pub line: u64,
    pub col: u64,
}

/// Why the followed window's view changed: the user, or the echo of a page's own `editor.reveal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewCause {
    User,
    Reveal,
}

/// The URL prefixes of the document route: the document's project directory and its own directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocPrefixes {
    pub root: String,
    pub base: String,
}

/// `editor.view`: where the followed buffer's window is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorView {
    pub buf: u64,
    /// The buffer's absolute path, or `""` for an unnamed buffer.
    pub name: String,
    pub filetype: String,
    pub tick: u64,
    /// Whether the followed window is nvim's current window.
    pub current: bool,
    pub line_count: u64,
    /// The buffer line at the window's top, 1-based; a closed fold's first line.
    pub top: u64,
    /// With a partly hidden wrapped top line, the hidden share of its rows; 0 otherwise.
    pub top_fraction: f64,
    pub bottom: u64,
    pub cursor: Cursor,
    pub cause: ViewCause,
    /// The page's own token, on the echo of its reveal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<u64>,
    /// Required on the wire, `null` for an unnamed buffer or while the project is still being looked up.
    #[serde(deserialize_with = "Option::deserialize")]
    pub doc: Option<DocPrefixes>,
}

/// Why a document was not sent: its size and the caps it went over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TooLarge {
    pub bytes: u64,
    pub lines: u64,
    pub max_bytes: u64,
    pub max_lines: u64,
}

/// `editor.text`: the followed buffer's whole text at `tick`, or why it is not sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorText {
    pub buf: u64,
    pub tick: u64,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub too_large: Option<TooLarge>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosedReason {
    /// The followed buffer was wiped.
    Wiped,
    /// The editor quit for good.
    Editor,
}

/// `editor.closed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorClosed {
    pub buf: u64,
    pub reason: ClosedReason,
}

pub fn event_text<T: Serialize>(event: &str, data: &T) -> String {
    let data = serde_json::to_value(data).unwrap_or(Value::Null);
    serde_json::to_string(&HostEvent {
        v: VERSION,
        event: event.to_owned(),
        data,
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn fixtures() -> Vec<(String, String)> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/plugin/fixtures");
        let mut all: Vec<(String, String)> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .map(|p| {
                (
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                    std::fs::read_to_string(&p).unwrap(),
                )
            })
            .collect();
        all.sort();
        assert!(all.len() >= 27, "the shared fixtures are missing");
        all
    }

    /// Optional counters the host leaves out when they are zero; a fixture may spell them out.
    fn without_zero_counters(mut v: Value) -> Value {
        if let Some(map) = v.as_object_mut() {
            map.retain(|k, x| !(k.starts_with("dropped_") && x.as_u64() == Some(0)));
        }
        v
    }

    #[test]
    fn every_request_fixture_parses_and_writes_back_the_same() {
        for (name, text) in fixtures().into_iter().filter(|(n, _)| n.starts_with("req-")) {
            let Inbound::Request(request) = parse_request(&text) else {
                panic!("{name} was refused")
            };
            let fixture: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(request.to_value(), fixture, "{name}");
        }
    }

    #[test]
    fn every_invalid_fixture_is_refused() {
        for (name, text) in fixtures().into_iter().filter(|(n, _)| n.starts_with("bad-")) {
            assert!(
                matches!(parse_request(&text), Inbound::Refused { .. }),
                "{name} was accepted"
            );
        }
    }

    #[test]
    fn every_reply_fixture_is_a_reply_the_host_writes_the_same_way() {
        for (name, text) in fixtures().into_iter().filter(|(n, _)| n.starts_with("rep-")) {
            let reply: Reply = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(reply.v, VERSION, "{name}");
            assert_eq!(reply.ok, reply.result.is_some() && reply.error.is_none(), "{name}");
            let fixture: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&reply.to_text()).unwrap(),
                fixture,
                "{name}"
            );
        }
    }

    #[test]
    fn every_event_fixture_has_the_shape_the_host_writes() {
        for (name, text) in fixtures().into_iter().filter(|(n, _)| n.starts_with("ev-")) {
            let event: HostEvent = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
            let written = match event.event.as_str() {
                "task.output" => {
                    serde_json::to_value(serde_json::from_value::<TaskOutput>(event.data.clone()).unwrap())
                }
                "task.exit" => serde_json::to_value(serde_json::from_value::<TaskExit>(event.data.clone()).unwrap()),
                "focus" => serde_json::to_value(serde_json::from_value::<FocusEvent>(event.data.clone()).unwrap()),
                "keys_config" => {
                    serde_json::to_value(serde_json::from_value::<KeysConfig>(event.data.clone()).unwrap())
                }
                "theme" => serde_json::to_value(serde_json::from_value::<ThemeEvent>(event.data.clone()).unwrap()),
                "editor.view" => {
                    serde_json::to_value(serde_json::from_value::<EditorView>(event.data.clone()).unwrap())
                }
                "editor.text" => {
                    serde_json::to_value(serde_json::from_value::<EditorText>(event.data.clone()).unwrap())
                }
                "editor.closed" => {
                    serde_json::to_value(serde_json::from_value::<EditorClosed>(event.data.clone()).unwrap())
                }
                other => panic!("{name}: unknown event {other}"),
            }
            .unwrap();
            assert_eq!(
                without_zero_counters(written),
                without_zero_counters(event.data),
                "{name}"
            );
        }
    }

    fn fixture(name: &str) -> Value {
        let text = fixtures()
            .into_iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("{name}"))
            .1;
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn the_theme_fixture_names_are_the_appearance_names() {
        let event = fixture("ev-theme.json");
        assert_eq!(event["event"], "theme");
        let theme: ThemeEvent = serde_json::from_value(event["data"].clone()).unwrap();
        let mut want: Vec<String> = crate::theme::ThemeTokens::fallback()
            .appearance_vars()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        want.sort();
        assert_eq!(theme.vars.keys().cloned().collect::<Vec<_>>(), want);
        for (name, value) in &theme.vars {
            assert!(crate::plugin::theme::page_var_ok(name, value), "{name}: {value}");
        }
    }

    #[test]
    fn keys_config_carries_three_fields_and_needs_module_key() {
        let event = fixture("ev-keys-config.json");
        let config: KeysConfig = serde_json::from_value(event["data"].clone()).unwrap();
        assert_eq!(
            config,
            KeysConfig {
                timeoutlen: 300,
                prefix: "Ctrl+b".into(),
                module_key: None,
            }
        );
        let mut without = event["data"].clone();
        without.as_object_mut().unwrap().remove("module_key");
        assert!(serde_json::from_value::<KeysConfig>(without).is_err());
        let with_key: KeysConfig =
            serde_json::from_value(serde_json::json!({"timeoutlen": 1, "prefix": "Ctrl+a", "module_key": "y"}))
                .unwrap();
        assert_eq!(with_key.module_key.as_deref(), Some("y"));
        assert_eq!(
            serde_json::to_value(&config).unwrap()["module_key"],
            Value::Null,
            "written as null, never left out"
        );
    }

    #[test]
    fn caps_versions_ids_and_unknown_keys() {
        let ok = r#"{"v":1,"id":3,"verb":"state.read","args":{"key":"k"}}"#;
        assert!(matches!(
            parse_request(ok),
            Inbound::Request(Request {
                id: 3,
                verb: Verb::StateRead(_)
            })
        ));
        let big = format!(
            r#"{{"v":1,"id":4,"verb":"state.write","args":{{"key":"k","value":"{}"}}}}"#,
            "x".repeat(MAX_PAGE_MESSAGE)
        );
        assert!(matches!(
            parse_request(&big),
            Inbound::Refused {
                re: Some(4),
                code: ErrorCode::TooLarge,
                ..
            }
        ));
        let huge = "x".repeat(MAX_READ_FOR_ID + 1);
        assert!(matches!(
            parse_request(&huge),
            Inbound::Refused {
                re: None,
                code: ErrorCode::TooLarge,
                ..
            }
        ));
        let v2 = r#"{"v":2,"id":5,"verb":"host.hello","args":{}}"#;
        assert!(matches!(
            parse_request(v2),
            Inbound::Refused {
                re: Some(5),
                code: ErrorCode::BadRequest,
                ..
            }
        ));
        let extra_arg = r#"{"v":1,"id":6,"verb":"host.hello","args":{"x":1}}"#;
        assert!(matches!(
            parse_request(extra_arg),
            Inbound::Refused {
                re: Some(6),
                code: ErrorCode::BadRequest,
                ..
            }
        ));
        let float_id = r#"{"v":1,"id":1.5,"verb":"host.hello","args":{}}"#;
        assert!(matches!(parse_request(float_id), Inbound::Refused { re: None, .. }));
        assert!(matches!(
            parse_request("[1]"),
            Inbound::Refused {
                re: None,
                code: ErrorCode::BadRequest,
                ..
            }
        ));
    }

    #[test]
    fn the_keys_declare_fixture_parses_to_its_verb() {
        let text = fixtures()
            .into_iter()
            .find(|(n, _)| n == "req-keys-declare.json")
            .expect("req-keys-declare.json")
            .1;
        let Inbound::Request(Request {
            verb: Verb::KeysDeclare(args),
            ..
        }) = parse_request(&text)
        else {
            panic!("not keys.declare")
        };
        assert!(!args.rows.is_empty());
        assert!(args.rows.iter().all(|r| !r.keys.is_empty()));
    }

    #[test]
    fn a_keys_declare_row_with_an_unknown_or_missing_key_is_a_bad_request() {
        for raw in [
            r#"{"v":1,"id":9,"verb":"keys.declare","args":{"rows":[{"action":"a","keys":["j"],"context":"c","desc":"d","when":"x"}]}}"#,
            r#"{"v":1,"id":9,"verb":"keys.declare","args":{"rows":[{"action":"a","keys":["j"],"context":"c"}]}}"#,
            r#"{"v":1,"id":9,"verb":"keys.declare","args":{"rows":[],"extra":1}}"#,
            r#"{"v":1,"id":9,"verb":"keys.declare","args":{"rows":[{"action":"a","keys":"j","context":"c","desc":"d"}]}}"#,
        ] {
            assert!(
                matches!(
                    parse_request(raw),
                    Inbound::Refused {
                        re: Some(9),
                        code: ErrorCode::BadRequest,
                        ..
                    }
                ),
                "{raw}"
            );
        }
    }

    #[test]
    fn each_verb_names_its_capability() {
        let cap = |raw: &str| match parse_request(raw) {
            Inbound::Request(r) => r.verb.capability(),
            Inbound::Refused { message, .. } => panic!("{message}"),
        };
        assert_eq!(cap(r#"{"v":1,"id":1,"verb":"host.hello","args":{}}"#), None);
        assert_eq!(cap(r#"{"v":1,"id":1,"verb":"keys.declare","args":{"rows":[]}}"#), None);
        assert_eq!(
            cap(r#"{"v":1,"id":1,"verb":"task.cancel","args":{"run":2}}"#),
            Some(Capability::Tasks)
        );
        assert_eq!(
            cap(r#"{"v":1,"id":1,"verb":"editor.assist_clear","args":{}}"#),
            Some(Capability::EditorAssist)
        );
        assert_eq!(
            cap(r#"{"v":1,"id":1,"verb":"layout.focus","args":{"target":"self"}}"#),
            Some(Capability::LayoutFocus)
        );
        assert_eq!(
            cap(r#"{"v":1,"id":1,"verb":"log.read","args":{"stream":"events"}}"#),
            Some(Capability::State)
        );
        for raw in [
            r#"{"v":1,"id":1,"verb":"editor.follow","args":{}}"#,
            r#"{"v":1,"id":1,"verb":"editor.reveal","args":{"buf":1,"line":1,"token":1}}"#,
            r#"{"v":1,"id":1,"verb":"editor.jump","args":{"buf":1,"line":1,"col":0,"focus":false}}"#,
            r#"{"v":1,"id":1,"verb":"editor.open_link","args":{"buf":1,"target":"a.md"}}"#,
        ] {
            assert_eq!(cap(raw), Some(Capability::EditorFollow), "{raw}");
        }
    }

    #[test]
    fn the_bridge_script_is_plain_ascii_and_builds_no_code_from_text() {
        assert!(BRIDGE_JS.is_ascii());
        for banned in ["eval(", "Function(", "innerHTML", "document.write", "setTimeout(\""] {
            assert!(!BRIDGE_JS.contains(banned), "{banned}");
        }
        assert!(BRIDGE_JS.contains("messageHandlers") && BRIDGE_JS.contains("handlers.eitriPlugin"));
        assert!(BRIDGE_JS.contains("__eitriPluginDispatch"));
    }

    /// Run by hand after the page side changes its fixtures:
    /// `EITRI_TEST_PLUGIN_FIXTURES_FROM=<page fixtures dir> cargo test -p eitri-core --lib -- --ignored fixtures_match`.
    #[test]
    #[ignore = "reads the page side's checkout, named by EITRI_TEST_PLUGIN_FIXTURES_FROM"]
    fn fixtures_match_the_page_side_byte_for_byte() {
        let from = std::env::var_os("EITRI_TEST_PLUGIN_FIXTURES_FROM").expect("set EITRI_TEST_PLUGIN_FIXTURES_FROM");
        let ours = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/plugin/fixtures");
        for e in std::fs::read_dir(&from).unwrap() {
            let p = e.unwrap().path();
            if p.extension().is_some_and(|x| x == "json") {
                let name = p.file_name().unwrap();
                assert_eq!(
                    std::fs::read(&p).unwrap(),
                    std::fs::read(ours.join(name)).unwrap(),
                    "{name:?}"
                );
            }
        }
    }

    #[test]
    fn the_follow_fixtures_parse_to_their_verbs_and_name_them() {
        let verb = |name: &str| match parse_request(&fixture_text(name)) {
            Inbound::Request(r) => r.verb,
            Inbound::Refused { message, .. } => panic!("{name}: {message}"),
        };
        assert_eq!(verb("req-editor-follow.json"), Verb::EditorFollow(NoArgs {}));
        assert_eq!(
            verb("req-editor-reveal.json"),
            Verb::EditorReveal(EditorReveal {
                buf: 3,
                line: 120,
                token: 7
            })
        );
        assert_eq!(
            verb("req-editor-jump.json"),
            Verb::EditorJump(EditorJump {
                buf: 3,
                line: 42,
                col: 0,
                focus: true
            })
        );
        assert_eq!(
            verb("req-editor-open-link.json"),
            Verb::EditorOpenLink(EditorOpenLink {
                buf: 3,
                target: "../docs/setup.md#L10".into()
            })
        );
        assert_eq!(verb("req-editor-open-link.json").name(), "editor.open_link");
        assert!(matches!(
            parse_request(&fixture_text("bad-editor-reveal-extra-key.json")),
            Inbound::Refused {
                re: Some(34),
                code: ErrorCode::BadRequest,
                ..
            }
        ));
    }

    fn fixture_text(name: &str) -> String {
        fixtures()
            .into_iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("{name}"))
            .1
    }

    #[test]
    fn a_line_below_one_or_a_bad_link_target_is_refused_by_its_check() {
        let reveal = |line| EditorReveal { buf: 1, line, token: 1 };
        assert_eq!(reveal(0).check(), Err("line must be at least 1".to_string()));
        assert_eq!(reveal(1).check(), Ok(()));
        let jump = |line| EditorJump {
            buf: 1,
            line,
            col: 0,
            focus: true,
        };
        assert_eq!(jump(0).check(), Err("line must be at least 1".to_string()));
        assert_eq!(jump(7).check(), Ok(()));
        let link = |target: String| EditorOpenLink { buf: 1, target };
        assert_eq!(link("../x.md#L3".into()).check(), Ok(()));
        assert_eq!(link("a".repeat(MAX_LINK_TARGET)).check(), Ok(()));
        assert_eq!(
            link("a".repeat(MAX_LINK_TARGET + 1)).check(),
            Err("target is over 4096 bytes".to_string())
        );
        for bad in ["a\nb.md", "a\u{7}.md", "a\u{7f}.md", "a\u{85}.md"] {
            assert_eq!(
                link(bad.into()).check(),
                Err("target holds a control character".to_string()),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_view_must_carry_doc_and_writes_null_and_no_token_when_absent() {
        let mut data = fixture("ev-editor-view-reveal.json")["data"].clone();
        let view: EditorView = serde_json::from_value(data.clone()).unwrap();
        assert_eq!(
            (view.cause, view.token, view.doc.clone()),
            (ViewCause::Reveal, Some(7), None)
        );
        assert_eq!(view.top_fraction, 0.25);
        let written = serde_json::to_value(&view).unwrap();
        assert_eq!(written["doc"], Value::Null, "written as null, never left out");
        let user = EditorView {
            cause: ViewCause::User,
            token: None,
            ..view
        };
        let written = serde_json::to_value(&user).unwrap();
        assert!(written.get("token").is_none(), "no token on a user view");
        data.as_object_mut().unwrap().remove("doc");
        assert!(serde_json::from_value::<EditorView>(data).is_err(), "doc is required");
        let view: EditorView = serde_json::from_value(fixture("ev-editor-view.json")["data"].clone()).unwrap();
        assert_eq!(
            view.doc,
            Some(DocPrefixes {
                root: "eitri-plugin://markdown/.eitri/doc/4/".into(),
                base: "eitri-plugin://markdown/.eitri/doc/4/docs/".into()
            })
        );
        assert_eq!(view.cursor, Cursor { line: 125, col: 4 });
    }

    #[test]
    fn editor_text_carries_the_text_or_why_not_and_closed_its_reason() {
        let text: EditorText = serde_json::from_value(fixture("ev-editor-text.json")["data"].clone()).unwrap();
        assert_eq!(text.text.as_deref(), Some("# Notes\n\nA paragraph.\n"));
        assert_eq!(text.too_large, None);
        let big: EditorText = serde_json::from_value(fixture("ev-editor-text-too-large.json")["data"].clone()).unwrap();
        assert_eq!(big.text, None);
        assert_eq!(
            big.too_large,
            Some(TooLarge {
                bytes: 5_452_595,
                lines: 61_000,
                max_bytes: 4 * 1024 * 1024,
                max_lines: 50_000
            })
        );
        let written = serde_json::to_value(&big).unwrap();
        assert!(written.get("text").is_none());
        let closed: EditorClosed = serde_json::from_value(fixture("ev-editor-closed.json")["data"].clone()).unwrap();
        assert_eq!(
            closed,
            EditorClosed {
                buf: 3,
                reason: ClosedReason::Wiped
            }
        );
        assert_eq!(
            serde_json::to_value(ClosedReason::Editor).unwrap(),
            Value::String("editor".into())
        );
        assert_eq!(
            event_text(EVENT_EDITOR_CLOSED, &closed),
            r#"{"v":1,"event":"editor.closed","data":{"buf":3,"reason":"wiped"}}"#
        );
    }

    #[test]
    fn the_settings_hello_fixture_has_the_keys_of_the_plain_one() {
        let plain = fixture("rep-hello.json");
        let with = fixture("rep-hello-settings.json");
        assert_eq!(
            plain["result"].as_object().unwrap().keys().collect::<Vec<_>>(),
            with["result"].as_object().unwrap().keys().collect::<Vec<_>>()
        );
        assert_eq!(plain["result"]["settings"], serde_json::json!({}));
        assert_eq!(
            with["result"]["settings"],
            serde_json::json!({"auto": false, "remote_images": "https", "sync": "both"})
        );
        assert_eq!(
            with["result"]["capabilities"],
            serde_json::json!(["editor.follow", "editor.commands", "net"])
        );
    }
}
