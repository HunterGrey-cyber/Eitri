//! The MCP messages: parsing what a client sends, the answers, the table of offered tools.
//!
//! Pure over bytes: no socket and no thread. A client is a process of the same user holding the token, and still
//! nothing it sends is trusted: a tool name goes back only inside a JSON string, and a pid is only ever logged.

use serde::Serialize;
use serde_json::{json, Map, Value};

use super::types::{OpenDiffArgs, RpcId, ToolAnswer, IDE_NAME};

/// The tools a client may call. `tools/list` holds exactly these.
pub const TOOL_NAMES: [&str; 4] = ["openDiff", "close_tab", "closeAllDiffTabs", "getDiagnostics"];

/// Oldest first.
const PROTOCOL_VERSIONS: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
const NEWEST_PROTOCOL_VERSION: &str = PROTOCOL_VERSIONS[PROTOCOL_VERSIONS.len() - 1];

const JSON_RPC: &str = "2.0";
const METHOD_NOT_FOUND: i64 = -32601;
const TOOL_ERROR: i64 = -32000;

/// The most of a tool name an answer repeats: a name is the client's own text, and escaping could carry one near the
/// message cap past the client's own cap on what it reads.
const NAME_SHOWN: usize = 128;

/// One message from a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    Initialize {
        id: RpcId,
        version: Option<String>,
    },
    ToolsList {
        id: RpcId,
    },
    GetDiagnostics {
        id: RpcId,
        uri: Option<String>,
    },
    OpenDiff {
        id: RpcId,
        args: OpenDiffArgs,
    },
    CloseTab {
        id: RpcId,
        tab_name: String,
    },
    CloseAllDiffTabs {
        id: RpcId,
    },
    /// A `tools/call` of any name not in [`TOOL_NAMES`].
    NotOffered {
        id: RpcId,
        name: String,
    },
    /// `server/discover` and every other request.
    UnknownMethod {
        id: RpcId,
    },
    BadParams {
        id: RpcId,
        why: String,
    },
    /// `notifications/initialized`.
    Initialized,
    /// `ide_connected`. The pid is the client's own word: only ever a log line.
    IdeConnected {
        pid: Option<i64>,
    },
    /// `notifications/cancelled`: the request it withdraws.
    Cancelled {
        request: RpcId,
    },
    /// Any other notification.
    Ignored,
}

/// What this server pushes to a client. A closed list, so nothing here can build a notification the CLI would book as
/// its own telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Push {
    SelectionChanged,
    AtMentioned,
}

impl Push {
    fn method(self) -> &'static str {
        match self {
            Push::SelectionChanged => "selection_changed",
            Push::AtMentioned => "at_mentioned",
        }
    }
}

/// One whole message. `Err` when it is no JSON object with a string `method`, which says nothing a client can be
/// answered on. The deepest it nests is the JSON reader's own limit, so a client cannot run the stack out.
///
/// An integer or string `id` makes it a request, which is always answered; any other `id` names no request, so the
/// message is a notification whatever its method.
pub fn parse(message: &[u8]) -> Result<Incoming, String> {
    let value: Value = serde_json::from_slice(message).map_err(|error| format!("not JSON: {error}"))?;
    let Value::Object(mut object) = value else {
        return Err("not a JSON object".to_owned());
    };
    let Some(Value::String(method)) = object.remove("method") else {
        return Err("no method".to_owned());
    };
    let params = object.remove("params").unwrap_or_default();
    Ok(match object.get("id").and_then(RpcId::from_json) {
        Some(id) => read_request(id, &method, params),
        None => read_notification(&method, &params),
    })
}

fn read_request(id: RpcId, method: &str, params: Value) -> Incoming {
    match method {
        "initialize" => {
            let version = params["protocolVersion"].as_str().map(str::to_owned);
            Incoming::Initialize { id, version }
        }
        "tools/list" => Incoming::ToolsList { id },
        "tools/call" => read_tool_call(id, params),
        _ => Incoming::UnknownMethod { id },
    }
}

fn read_notification(method: &str, params: &Value) -> Incoming {
    match method {
        "notifications/initialized" => Incoming::Initialized,
        "ide_connected" => Incoming::IdeConnected {
            pid: params["pid"].as_i64(),
        },
        "notifications/cancelled" => match RpcId::from_json(&params["requestId"]) {
            Some(request) => Incoming::Cancelled { request },
            None => Incoming::Ignored,
        },
        _ => Incoming::Ignored,
    }
}

fn read_tool_call(id: RpcId, params: Value) -> Incoming {
    let Value::Object(mut params) = params else {
        return Incoming::BadParams {
            id,
            why: "tools/call needs params".to_owned(),
        };
    };
    let Some(Value::String(name)) = params.remove("name") else {
        return Incoming::BadParams {
            id,
            why: "tools/call names no tool".to_owned(),
        };
    };
    let arguments = params.remove("arguments").unwrap_or_default();
    match name.as_str() {
        "openDiff" => or_bad_params(id, open_diff_args(arguments), |id, args| Incoming::OpenDiff {
            id,
            args,
        }),
        "close_tab" => or_bad_params(id, tab_name(arguments), |id, tab_name| Incoming::CloseTab {
            id,
            tab_name,
        }),
        "closeAllDiffTabs" => Incoming::CloseAllDiffTabs { id },
        "getDiagnostics" => or_bad_params(id, diagnostics_uri(arguments), |id, uri| Incoming::GetDiagnostics {
            id,
            uri,
        }),
        _ => Incoming::NotOffered { id, name },
    }
}

fn or_bad_params<T>(id: RpcId, read: Result<T, String>, call: impl FnOnce(RpcId, T) -> Incoming) -> Incoming {
    match read {
        Ok(value) => call(id, value),
        Err(why) => Incoming::BadParams { id, why },
    }
}

/// A call may leave `arguments` out, which is the same as none.
fn arguments_map(arguments: Value) -> Result<Map<String, Value>, String> {
    match arguments {
        Value::Null => Ok(Map::new()),
        Value::Object(map) => Ok(map),
        _ => Err("arguments must be an object".to_owned()),
    }
}

/// Moved out of the message: `new_file_contents` may be megabytes.
fn required_string(arguments: &mut Map<String, Value>, key: &str) -> Result<String, String> {
    match arguments.remove(key) {
        Some(Value::String(text)) => Ok(text),
        _ => Err(format!("{key} must be a string")),
    }
}

/// `old_file_path` is the same file as `new_file_path` and nothing reads it, but the call this server knows has all
/// four: one without it is some other call.
fn open_diff_args(arguments: Value) -> Result<OpenDiffArgs, String> {
    let mut arguments = arguments_map(arguments)?;
    required_string(&mut arguments, "old_file_path")?;
    Ok(OpenDiffArgs {
        new_file_path: required_string(&mut arguments, "new_file_path")?,
        new_file_contents: required_string(&mut arguments, "new_file_contents")?,
        tab_name: required_string(&mut arguments, "tab_name")?,
    })
}

fn tab_name(arguments: Value) -> Result<String, String> {
    required_string(&mut arguments_map(arguments)?, "tab_name")
}

fn diagnostics_uri(arguments: Value) -> Result<Option<String>, String> {
    let mut arguments = arguments_map(arguments)?;
    if arguments.contains_key("uri") {
        required_string(&mut arguments, "uri").map(Some)
    } else {
        Ok(None)
    }
}

/// The id of a message whose tail is all that was kept: the last `"id":` key, a number or a string, ending the object.
/// The CLI's encoder writes `id` last. In a tail no such key can sit inside a string, where its quotes are escaped, so
/// the last one is the real one or there is none: anything after it but the closing brace answers `None`.
pub fn tail_id(tail: &[u8]) -> Option<RpcId> {
    const KEY: &[u8] = b"\"id\":";
    let start = tail.windows(KEY.len()).rposition(|window| window == KEY)?;
    let value = tail[start + KEY.len()..].trim_ascii().strip_suffix(b"}")?;
    RpcId::from_json(&serde_json::from_slice(value).ok()?)
}

/// Writing `result` straight out, so a file's text in an answer is not copied on its way.
#[derive(Serialize)]
struct Answer<R> {
    jsonrpc: &'static str,
    id: Value,
    result: R,
}

#[derive(Serialize)]
struct Contents<'a> {
    content: Vec<TextContent<'a>>,
}

#[derive(Serialize)]
struct TextContent<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

#[derive(Serialize)]
struct Notification {
    jsonrpc: &'static str,
    method: &'static str,
    params: Value,
}

fn encode(message: &impl Serialize) -> Vec<u8> {
    serde_json::to_vec(message).expect("a message of strings and JSON values serialises")
}

fn answer(id: &RpcId, result: impl Serialize) -> Vec<u8> {
    encode(&Answer {
        jsonrpc: JSON_RPC,
        id: id.to_json(),
        result,
    })
}

fn answer_texts(id: &RpcId, texts: &[&str]) -> Vec<u8> {
    let content = texts.iter().map(|text| TextContent { kind: "text", text }).collect();
    answer(id, Contents { content })
}

fn answer_failure(id: &RpcId, code: i64, message: &str) -> Vec<u8> {
    encode(&json!({"jsonrpc": JSON_RPC, "id": id.to_json(), "error": {"code": code, "message": message}}))
}

/// The version asked when it is one this server knows, else the newest it knows.
pub fn answer_initialize(id: &RpcId, asked: Option<&str>) -> Vec<u8> {
    let version = asked
        .filter(|asked| PROTOCOL_VERSIONS.contains(asked))
        .unwrap_or(NEWEST_PROTOCOL_VERSION);
    answer(
        id,
        json!({
            "protocolVersion": version,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": IDE_NAME, "version": env!("CARGO_PKG_VERSION")},
        }),
    )
}

pub fn answer_tools_list(id: &RpcId) -> Vec<u8> {
    answer(
        id,
        json!({"tools": [
            {
                "name": "openDiff",
                "description": "Show a proposed edit in the editor beside the person's code and wait for an answer.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "old_file_path": {"type": "string"},
                        "new_file_path": {"type": "string"},
                        "new_file_contents": {"type": "string"},
                        "tab_name": {"type": "string"},
                    },
                    "required": ["old_file_path", "new_file_path", "new_file_contents", "tab_name"],
                },
            },
            {
                "name": "close_tab",
                "description": "Close the proposal view of this name. A name with no view is no error.",
                "inputSchema": {
                    "type": "object",
                    "properties": {"tab_name": {"type": "string"}},
                    "required": ["tab_name"],
                },
            },
            {
                "name": "closeAllDiffTabs",
                "description": "Close every proposal view this client has open.",
                "inputSchema": {"type": "object", "properties": {}},
            },
            {
                "name": "getDiagnostics",
                "description": "Get the diagnostics of the files loaded in the editor, or of the file `uri` names.",
                "inputSchema": {
                    "type": "object",
                    "properties": {"uri": {"type": "string", "description": "file:// and an absolute path"}},
                },
            },
        ]}),
    )
}

/// There is no way to write an answer that approves on closing: [`ToolAnswer`] has no such variant.
pub fn answer_tool(id: &RpcId, tool_answer: &ToolAnswer) -> Vec<u8> {
    match tool_answer {
        ToolAnswer::Empty => answer_texts(id, &[""]),
        ToolAnswer::DiagnosticsText(text) => answer_texts(id, &[text]),
        ToolAnswer::FileSaved(text) => answer_texts(id, &["FILE_SAVED", text]),
        ToolAnswer::DiffRejected => answer_texts(id, &["DIFF_REJECTED"]),
        ToolAnswer::Error(why) => answer_error(id, why),
    }
}

pub fn answer_method_not_found(id: &RpcId) -> Vec<u8> {
    answer_failure(id, METHOD_NOT_FOUND, "method not found")
}

pub fn answer_error(id: &RpcId, why: &str) -> Vec<u8> {
    answer_failure(id, TOOL_ERROR, why)
}

/// An error, so a CLI that starts calling a tool not listed here shows it, where an empty answer would hide it. The
/// CLI logs it and carries on.
pub fn answer_not_offered(id: &RpcId, name: &str) -> Vec<u8> {
    let shown: String = name.chars().take(NAME_SHOWN).collect();
    answer_error(id, &format!("{IDE_NAME} does not offer {shown}"))
}

pub fn notification(push: Push, params: Value) -> Vec<u8> {
    encode(&Notification {
        jsonrpc: JSON_RPC,
        method: push.method(),
        params,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const VERSIONS: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
    const NEWEST: &str = "2025-11-25";

    fn read(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).expect("an answer is JSON")
    }

    fn message(value: Value) -> Incoming {
        parse(value.to_string().as_bytes()).expect("a well-formed message")
    }

    fn request(method: &str) -> Incoming {
        message(json!({"jsonrpc": "2.0", "id": 7, "method": method}))
    }

    fn call(name: &str, arguments: Value) -> Incoming {
        message(json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        }))
    }

    fn open_diff_arguments() -> Map<String, Value> {
        let Value::Object(arguments) = json!({
            "old_file_path": "/p/a.rs",
            "new_file_path": "/p/a.rs",
            "new_file_contents": "fn main() {}\n",
            "tab_name": "a.rs (0f1e2d)",
        }) else {
            unreachable!("the literal is an object");
        };
        arguments
    }

    fn texts(answer: &[u8]) -> Vec<(String, String)> {
        read(answer)["result"]["content"]
            .as_array()
            .expect("a content array")
            .iter()
            .map(|item| {
                let field = |name: &str| item[name].as_str().expect("a string field").to_owned();
                (field("type"), field("text"))
            })
            .collect()
    }

    fn text(value: &str) -> (String, String) {
        ("text".to_owned(), value.to_owned())
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|window| window == needle)
    }

    #[test]
    fn initialize_says_eitri_and_echoes_a_known_version() {
        for version in VERSIONS {
            let incoming = message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {"protocolVersion": version},
            }));
            assert_eq!(
                incoming,
                Incoming::Initialize {
                    id: RpcId::Num(1),
                    version: Some(version.to_owned())
                }
            );
            let answer = read(&answer_initialize(&RpcId::Num(1), Some(version)));
            assert_eq!(answer["jsonrpc"], "2.0");
            assert_eq!(answer["id"], 1);
            assert_eq!(answer["result"]["protocolVersion"], version);
            assert_eq!(answer["result"]["capabilities"], json!({"tools": {}}));
            assert_eq!(answer["result"]["serverInfo"]["name"], "Eitri");
            assert_eq!(answer["result"]["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
        }
    }

    #[test]
    fn an_unknown_version_gets_the_newest() {
        for asked in [Some("1999-01-01"), Some(""), None] {
            let answer = read(&answer_initialize(&RpcId::Num(1), asked));
            assert_eq!(answer["result"]["protocolVersion"], NEWEST, "asked {asked:?}");
        }
        assert_eq!(
            message(json!({"id": 1, "method": "initialize", "params": {"protocolVersion": 20251125}})),
            Incoming::Initialize {
                id: RpcId::Num(1),
                version: None
            }
        );
        assert_eq!(
            message(json!({"id": 1, "method": "initialize"})),
            Incoming::Initialize {
                id: RpcId::Num(1),
                version: None
            }
        );
    }

    /// The CLI skips its diagnostics baselines for a server of that name.
    #[test]
    fn the_server_is_never_the_jetbrains_name() {
        assert_eq!(IDE_NAME, "Eitri");
        let answer = answer_initialize(&RpcId::Num(1), None);
        assert!(!contains(&answer, b"JetBrains"));
    }

    #[test]
    fn server_discover_answers_method_not_found() {
        let id = RpcId::Text("server-discover-probe-1".to_owned());
        let incoming = message(json!({"jsonrpc": "2.0", "id": "server-discover-probe-1", "method": "server/discover"}));
        assert_eq!(incoming, Incoming::UnknownMethod { id: id.clone() });
        assert_eq!(request("prompts/list"), Incoming::UnknownMethod { id: RpcId::Num(7) });
        let answer = read(&answer_method_not_found(&id));
        assert_eq!(answer["id"], "server-discover-probe-1");
        assert_eq!(answer["error"]["code"], -32601);
        assert!(answer.get("result").is_none());
    }

    #[test]
    fn tools_list_holds_exactly_the_four() {
        assert_eq!(request("tools/list"), Incoming::ToolsList { id: RpcId::Num(7) });
        let answer = read(&answer_tools_list(&RpcId::Num(7)));
        let tools = answer["result"]["tools"].as_array().expect("a tools array");
        let names: Vec<&str> = tools
            .iter()
            .map(|tool| tool["name"].as_str().expect("a name"))
            .collect();
        assert_eq!(names, ["openDiff", "close_tab", "closeAllDiffTabs", "getDiagnostics"]);
        assert_eq!(names, TOOL_NAMES);
        for tool in tools {
            assert!(!tool["description"].as_str().expect("a description").is_empty());
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
        let schema = |name: &str| &tools[names.iter().position(|one| *one == name).expect("listed")]["inputSchema"];
        assert_eq!(schema("getDiagnostics")["properties"]["uri"]["type"], "string");
        assert!(schema("getDiagnostics").get("required").is_none());
        assert_eq!(
            schema("openDiff")["required"],
            json!(["old_file_path", "new_file_path", "new_file_contents", "tab_name"])
        );
        assert_eq!(schema("close_tab")["required"], json!(["tab_name"]));
    }

    #[test]
    fn execute_code_open_file_and_save_document_are_not_offered() {
        for name in [
            "executeCode",
            "openFile",
            "saveDocument",
            "getOpenEditors",
            "getWorkspaceFolders",
            "getCurrentSelection",
            "getLatestSelection",
            "checkDocumentDirty",
        ] {
            assert_eq!(
                call(name, json!({"code": "print(1)"})),
                Incoming::NotOffered {
                    id: RpcId::Num(7),
                    name: name.to_owned()
                }
            );
            assert!(
                !contains(&answer_tools_list(&RpcId::Num(7)), name.as_bytes()),
                "{name} is listed"
            );
            let answer = read(&answer_not_offered(&RpcId::Num(7), name));
            assert_eq!(answer["id"], 7);
            assert_eq!(answer["error"]["code"], -32000);
            assert_eq!(answer["error"]["message"], format!("Eitri does not offer {name}"));
        }
    }

    /// A control character escapes to six bytes, which would carry a name near the message cap past the client's own
    /// frame cap.
    #[test]
    fn a_long_tool_name_is_cut_in_the_answer() {
        let answer = read(&answer_not_offered(&RpcId::Num(7), &"\u{1}".repeat(8 << 20)));
        let shown = answer["error"]["message"].as_str().expect("a message");
        assert!(shown.starts_with("Eitri does not offer "));
        assert!(shown.len() < 1024, "{} bytes", shown.len());
    }

    #[test]
    fn open_diff_needs_all_four_strings() {
        assert_eq!(
            call("openDiff", Value::Object(open_diff_arguments())),
            Incoming::OpenDiff {
                id: RpcId::Num(7),
                args: OpenDiffArgs {
                    new_file_path: "/p/a.rs".to_owned(),
                    new_file_contents: "fn main() {}\n".to_owned(),
                    tab_name: "a.rs (0f1e2d)".to_owned(),
                },
            }
        );
        for key in ["old_file_path", "new_file_path", "new_file_contents", "tab_name"] {
            let mut missing = open_diff_arguments();
            missing.remove(key);
            let mut not_a_string = open_diff_arguments();
            not_a_string.insert(key.to_owned(), json!(3));
            for arguments in [missing, not_a_string] {
                let Incoming::BadParams { id, why } = call("openDiff", Value::Object(arguments)) else {
                    panic!("{key} is required");
                };
                assert_eq!(id, RpcId::Num(7));
                assert!(why.contains(key), "{why} names {key}");
            }
        }
    }

    #[test]
    fn close_tab_needs_its_name() {
        assert_eq!(
            call("close_tab", json!({"tab_name": "a.rs"})),
            Incoming::CloseTab {
                id: RpcId::Num(7),
                tab_name: "a.rs".to_owned()
            }
        );
        for arguments in [json!({}), json!({"tab_name": 1})] {
            assert!(matches!(call("close_tab", arguments), Incoming::BadParams { .. }));
        }
    }

    #[test]
    fn close_all_diff_tabs_takes_nothing() {
        assert_eq!(
            call("closeAllDiffTabs", json!({})),
            Incoming::CloseAllDiffTabs { id: RpcId::Num(7) }
        );
    }

    #[test]
    fn get_diagnostics_takes_an_optional_string_uri() {
        assert_eq!(
            call("getDiagnostics", json!({})),
            Incoming::GetDiagnostics {
                id: RpcId::Num(7),
                uri: None
            }
        );
        assert_eq!(
            message(json!({"id": 7, "method": "tools/call", "params": {"name": "getDiagnostics"}})),
            Incoming::GetDiagnostics {
                id: RpcId::Num(7),
                uri: None
            }
        );
        assert_eq!(
            call("getDiagnostics", json!({"uri": "file:///p/a b%20c.rs"})),
            Incoming::GetDiagnostics {
                id: RpcId::Num(7),
                uri: Some("file:///p/a b%20c.rs".to_owned())
            }
        );
        for arguments in [json!({"uri": 1}), json!("file:///p/a.rs")] {
            assert!(matches!(call("getDiagnostics", arguments), Incoming::BadParams { .. }));
        }
    }

    #[test]
    fn a_tools_call_that_names_no_tool_is_bad_params() {
        for params in [json!({}), json!({"name": 1}), json!("openDiff")] {
            let incoming = message(json!({"id": 7, "method": "tools/call", "params": params}));
            assert!(matches!(incoming, Incoming::BadParams { .. }), "{incoming:?}");
        }
        assert!(matches!(request("tools/call"), Incoming::BadParams { .. }));
    }

    #[test]
    fn cancelled_reads_the_request_id() {
        assert_eq!(
            message(json!({"method": "notifications/cancelled", "params": {"requestId": 12, "reason": "x"}})),
            Incoming::Cancelled {
                request: RpcId::Num(12)
            }
        );
        assert_eq!(
            message(json!({"method": "notifications/cancelled", "params": {"requestId": "a"}})),
            Incoming::Cancelled {
                request: RpcId::Text("a".to_owned())
            }
        );
        assert_eq!(
            message(json!({"method": "notifications/cancelled", "params": {}})),
            Incoming::Ignored
        );
        assert_eq!(message(json!({"method": "notifications/cancelled"})), Incoming::Ignored);
    }

    #[test]
    fn the_notifications_read_as_themselves() {
        assert_eq!(
            message(json!({"method": "notifications/initialized"})),
            Incoming::Initialized
        );
        assert_eq!(
            message(json!({"method": "ide_connected", "params": {"pid": 4242}})),
            Incoming::IdeConnected { pid: Some(4242) }
        );
        assert_eq!(
            message(json!({"method": "ide_connected", "params": {"pid": "4242"}})),
            Incoming::IdeConnected { pid: None }
        );
        assert_eq!(
            message(json!({"method": "ide_connected"})),
            Incoming::IdeConnected { pid: None }
        );
        assert_eq!(
            message(json!({"method": "notifications/roots/list_changed"})),
            Incoming::Ignored
        );
    }

    /// An answer needs an id to go back on, so a call without one does not run.
    #[test]
    fn a_request_with_no_usable_id_is_ignored() {
        assert_eq!(message(json!({"method": "tools/list"})), Incoming::Ignored);
        assert_eq!(message(json!({"id": null, "method": "tools/list"})), Incoming::Ignored);
        assert_eq!(message(json!({"id": 1.5, "method": "tools/list"})), Incoming::Ignored);
    }

    #[test]
    fn a_notification_sent_with_an_id_is_a_request_and_gets_its_answer() {
        assert_eq!(
            request("notifications/initialized"),
            Incoming::UnknownMethod { id: RpcId::Num(7) }
        );
        assert_eq!(request("ide_connected"), Incoming::UnknownMethod { id: RpcId::Num(7) });
    }

    #[test]
    fn what_is_no_message_is_an_error() {
        let deep = "[".repeat(100_000);
        for bytes in [
            b"not json".as_slice(),
            b"",
            b"\xff\xfe",
            b"[1, 2]",
            b"\"tools/list\"",
            b"{\"id\": 1}",
            b"{\"id\": 1, \"result\": {}}",
            b"{\"id\": 1, \"method\": 3}",
            deep.as_bytes(),
        ] {
            assert!(
                parse(bytes).is_err(),
                "{:?}",
                String::from_utf8_lossy(&bytes[..bytes.len().min(32)])
            );
        }
    }

    #[test]
    fn tail_id_reads_the_encoders_last_key() {
        assert_eq!(
            tail_id(br#"ontents":"x"}},"jsonrpc":"2.0","id":42}"#),
            Some(RpcId::Num(42))
        );
        assert_eq!(tail_id(br#"},"id":-3}"#), Some(RpcId::Num(-3)));
        assert_eq!(tail_id(br#"},"id":"abc"}"#), Some(RpcId::Text("abc".to_owned())));
        assert_eq!(tail_id(br#"},"id":"a\"b"}"#), Some(RpcId::Text("a\"b".to_owned())));
        assert_eq!(tail_id(b"},\"id\":4}\n"), Some(RpcId::Num(4)));
        assert_eq!(tail_id(b"\xff\xfe\"},\"id\":4}"), Some(RpcId::Num(4)));
        assert_eq!(
            tail_id(br#"tents":"\"id\":9,\"id\":8}"},"other":{"id":7},"id":5}"#),
            Some(RpcId::Num(5)),
            "an `id` in the contents, or in a nested object, comes before the real last key"
        );
    }

    #[test]
    fn tail_id_is_none_with_no_id_ending_the_object() {
        for tail in [
            b"".as_slice(),
            b"}",
            br#"ontents":"x"}},"jsonrpc":"2.0"}"#,
            br#"tents":"\"id\":9}"}}"#,
            br#"params":{"id":3}}"#,
            br#""id":4,"jsonrpc":"2.0"}"#,
            br#""id":null}"#,
            br#""id":1.5}"#,
            br#""id":{}}"#,
            br#""id":4"#,
            br#""id":"open"#,
            b"\"id\":\xff}",
        ] {
            assert_eq!(tail_id(tail), None, "{:?}", String::from_utf8_lossy(tail));
        }
    }

    #[test]
    fn file_saved_is_two_text_contents() {
        let answer = answer_tool(&RpcId::Num(7), &ToolAnswer::FileSaved("the file\n".to_owned()));
        assert_eq!(texts(&answer), [text("FILE_SAVED"), text("the file\n")]);
        assert_eq!(read(&answer)["id"], 7);
    }

    #[test]
    fn a_rejection_is_one_text_content() {
        assert_eq!(
            texts(&answer_tool(&RpcId::Num(7), &ToolAnswer::DiffRejected)),
            [text("DIFF_REJECTED")]
        );
    }

    #[test]
    fn diagnostics_are_one_text_content() {
        let answer = answer_tool(&RpcId::Num(7), &ToolAnswer::DiagnosticsText("[]".to_owned()));
        assert_eq!(texts(&answer), [text("[]")]);
    }

    #[test]
    fn an_empty_answer_is_one_empty_text() {
        let answer = read(&answer_tool(&RpcId::Text("a".to_owned()), &ToolAnswer::Empty));
        assert_eq!(answer["id"], "a");
        assert_eq!(answer["result"], json!({"content": [{"type": "text", "text": ""}]}));
    }

    #[test]
    fn an_error_is_a_json_rpc_error() {
        let by_tool = read(&answer_tool(&RpcId::Num(7), &ToolAnswer::Error("no editor".to_owned())));
        let by_itself = read(&answer_error(&RpcId::Num(7), "no editor"));
        assert_eq!(by_tool, by_itself);
        assert_eq!(by_tool["jsonrpc"], "2.0");
        assert_eq!(by_tool["id"], 7);
        assert_eq!(by_tool["error"], json!({"code": -32000, "message": "no editor"}));
        assert!(by_tool.get("result").is_none());
    }

    #[test]
    fn a_notification_has_a_method_params_and_no_id() {
        for (push, method) in [
            (Push::SelectionChanged, "selection_changed"),
            (Push::AtMentioned, "at_mentioned"),
        ] {
            let sent = read(&notification(push, json!({"filePath": "/p/a.rs"})));
            assert_eq!(
                sent,
                json!({"jsonrpc": "2.0", "method": method, "params": {"filePath": "/p/a.rs"}})
            );
        }
    }

    /// Split, so the name stands nowhere in this module's text, tests included.
    #[test]
    fn nothing_built_here_names_the_telemetry_notification() {
        let id = RpcId::Num(7);
        let answers = [
            answer_initialize(&id, None),
            answer_tools_list(&id),
            answer_tool(&id, &ToolAnswer::Empty),
            answer_tool(&id, &ToolAnswer::DiffRejected),
            answer_method_not_found(&id),
            notification(Push::SelectionChanged, json!({})),
            notification(Push::AtMentioned, json!({})),
        ];
        for answer in answers {
            assert!(!contains(&answer, concat!("log_", "event").as_bytes()));
        }
    }
}
