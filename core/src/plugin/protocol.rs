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
}

impl Verb {
    /// The capability a verb needs; `host.hello` needs none.
    pub fn capability(&self) -> Option<Capability> {
        match self {
            Verb::HostHello(_) => None,
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
        assert!(all.len() >= 23, "the shared fixtures are missing");
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
    fn each_verb_names_its_capability() {
        let cap = |raw: &str| match parse_request(raw) {
            Inbound::Request(r) => r.verb.capability(),
            Inbound::Refused { message, .. } => panic!("{message}"),
        };
        assert_eq!(cap(r#"{"v":1,"id":1,"verb":"host.hello","args":{}}"#), None);
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
}
