//! What a settings file or `.mcp.json` asks the CLI to do, as findings for the prompt.
//!
//! Nothing is filtered by how dangerous it looks: a list of dangerous keys is always one key short,
//! so every top-level key is reported, the ones read here in a form a person can judge (a hook's
//! command, an MCP server's command line) and the rest as their compact JSON. Text is kept byte for
//! byte; making control or bidi characters visible is the page's job.

use std::path::Path;

use serde_json::{Map, Value};

use super::Finding;

/// The compact JSON of a value, as it would appear in the file.
fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// A string as itself, anything else as its compact JSON.
fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => compact(other),
    }
}

fn parse_object(file: &Path, bytes: &[u8], out: &mut Vec<Finding>) -> Option<Map<String, Value>> {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(map)) => Some(map),
        Ok(other) => {
            out.push(Finding::Unparsed {
                file: file.to_path_buf(),
                reason: format!("not a JSON object: {}", compact(&other)),
            });
            None
        }
        Err(error) => {
            out.push(Finding::Unparsed {
                file: file.to_path_buf(),
                reason: error.to_string(),
            });
            None
        }
    }
}

/// A `.claude/settings.json` or `.claude/settings.local.json`.
pub fn settings_findings(file: &Path, bytes: &[u8]) -> Vec<Finding> {
    let mut out = Vec::new();
    let Some(map) = parse_object(file, bytes, &mut out) else {
        return out;
    };
    let other = |out: &mut Vec<Finding>, key: &str, value: &Value| {
        out.push(Finding::OtherSetting {
            file: file.to_path_buf(),
            key: key.to_string(),
            value: compact(value),
        });
    };
    for (key, value) in &map {
        match (key.as_str(), value) {
            ("hooks", Value::Object(events)) => hooks(file, events, &mut out),
            ("env", Value::Object(vars)) => {
                for name in vars.keys() {
                    out.push(Finding::EnvKey {
                        file: file.to_path_buf(),
                        key: name.clone(),
                    });
                }
            }
            ("apiKeyHelper", Value::String(command)) => {
                out.push(Finding::ApiKeyHelper {
                    file: file.to_path_buf(),
                    command: command.clone(),
                });
            }
            ("permissions", Value::Object(permissions)) => {
                for (name, value) in permissions {
                    match (name.as_str(), value) {
                        ("allow", Value::Array(rules)) => {
                            for rule in rules {
                                out.push(Finding::Allow {
                                    file: file.to_path_buf(),
                                    rule: text(rule),
                                });
                            }
                        }
                        ("additionalDirectories", Value::Array(dirs)) => {
                            for dir in dirs {
                                out.push(Finding::AdditionalDirectory {
                                    file: file.to_path_buf(),
                                    path: text(dir),
                                });
                            }
                        }
                        _ => other(&mut out, &format!("permissions.{name}"), value),
                    }
                }
            }
            _ => other(&mut out, key, value),
        }
    }
    out
}

/// `hooks.<Event>[].{matcher, hooks[].command}`. Anything not in that shape is still a hook, given
/// as its compact JSON, so an unusual entry is never dropped from the prompt.
fn hooks(file: &Path, events: &Map<String, Value>, out: &mut Vec<Finding>) {
    let hook = |out: &mut Vec<Finding>, event: &str, matcher: Option<String>, command: String| {
        out.push(Finding::Hook {
            file: file.to_path_buf(),
            event: event.to_string(),
            matcher,
            command,
        });
    };
    for (event, groups) in events {
        let Value::Array(groups) = groups else {
            hook(out, event, None, compact(groups));
            continue;
        };
        for group in groups {
            let Value::Object(group_map) = group else {
                hook(out, event, None, compact(group));
                continue;
            };
            let matcher = group_map.get("matcher").map(text);
            match group_map.get("hooks") {
                Some(Value::Array(commands)) => {
                    for command in commands {
                        let line = match command.get("command") {
                            Some(Value::String(line)) => line.clone(),
                            _ => compact(command),
                        };
                        hook(out, event, matcher.clone(), line);
                    }
                }
                _ => hook(out, event, matcher, compact(group)),
            }
        }
    }
}

/// One argument as a shell would need it: single-quoted when it holds whitespace, a quote or
/// nothing at all, with an inner `'` written `'\''`.
fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.chars().any(|c| c.is_whitespace() || c == '\'' || c == '"') {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// A `.mcp.json`: each of `mcpServers` as the command line it starts (environment first, as a shell
/// would write it, since a variable changes what runs), or `<type> <url>` for a remote one.
pub fn mcp_findings(file: &Path, bytes: &[u8]) -> Vec<Finding> {
    let mut out = Vec::new();
    let Some(map) = parse_object(file, bytes, &mut out) else {
        return out;
    };
    for (key, value) in &map {
        let Value::Object(servers) = value else {
            out.push(Finding::OtherSetting {
                file: file.to_path_buf(),
                key: key.clone(),
                value: compact(value),
            });
            continue;
        };
        if key != "mcpServers" {
            out.push(Finding::OtherSetting {
                file: file.to_path_buf(),
                key: key.clone(),
                value: compact(value),
            });
            continue;
        }
        for (name, server) in servers {
            out.push(Finding::McpServer {
                file: file.to_path_buf(),
                name: name.clone(),
                command_line: server_line(server),
            });
        }
    }
    out
}

fn server_line(server: &Value) -> String {
    let Value::Object(fields) = server else {
        return compact(server);
    };
    if let Some(command) = fields.get("command") {
        let mut words = Vec::new();
        if let Some(env) = fields.get("env") {
            match env {
                Value::Object(vars) => {
                    for (name, value) in vars {
                        words.push(format!("{}={}", quote(name), quote(&text(value))));
                    }
                }
                other => words.push(format!("env={}", quote(&compact(other)))),
            }
        }
        words.push(quote(&text(command)));
        match fields.get("args") {
            Some(Value::Array(args)) => words.extend(args.iter().map(|arg| quote(&text(arg)))),
            Some(other) => words.push(quote(&compact(other))),
            None => {}
        }
        return words.join(" ");
    }
    if let Some(url) = fields.get("url") {
        let kind = fields.get("type").map(text).unwrap_or_else(|| "http".to_string());
        return format!("{kind} {}", text(url));
    }
    compact(server)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn file() -> PathBuf {
        PathBuf::from(".claude/settings.json")
    }

    #[test]
    fn a_hook_gives_its_event_matcher_and_command() {
        let json = br#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"run.sh"}]}],
            "SessionStart":[{"hooks":[{"type":"command","command":"touch /tmp/m"},{"prompt":"x","type":"prompt"}]}]}}"#;
        let found = settings_findings(&file(), json);
        assert_eq!(
            found,
            vec![
                Finding::Hook {
                    file: file(),
                    event: "PreToolUse".into(),
                    matcher: Some("Bash".into()),
                    command: "run.sh".into()
                },
                Finding::Hook {
                    file: file(),
                    event: "SessionStart".into(),
                    matcher: None,
                    command: "touch /tmp/m".into()
                },
                Finding::Hook {
                    file: file(),
                    event: "SessionStart".into(),
                    matcher: None,
                    command: r#"{"prompt":"x","type":"prompt"}"#.into()
                },
            ]
        );
    }

    #[test]
    fn a_hook_command_keeps_escapes_and_bidi_byte_exact() {
        let command = "echo \u{1b}[31mred \u{202e}evil";
        let json = serde_json::json!({"hooks": {"Stop": [{"hooks": [{"command": command}]}]}});
        let found = settings_findings(&file(), json.to_string().as_bytes());
        assert_eq!(
            found,
            vec![Finding::Hook {
                file: file(),
                event: "Stop".into(),
                matcher: None,
                command: command.into()
            }]
        );
    }

    #[test]
    fn an_odd_hook_shape_is_still_reported() {
        let found = settings_findings(&file(), br#"{"hooks":{"Stop":"oops","Pre":[7]}}"#);
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|f| matches!(f, Finding::Hook { .. })));
    }

    #[test]
    fn env_keys_are_named() {
        let found = settings_findings(&file(), br#"{"env":{"LD_PRELOAD":"/x.so","A":"1"}}"#);
        let mut keys: Vec<_> = found
            .iter()
            .map(|f| match f {
                Finding::EnvKey { key, .. } => key.clone(),
                other => panic!("{other:?}"),
            })
            .collect();
        keys.sort();
        assert_eq!(keys, vec!["A", "LD_PRELOAD"]);
    }

    #[test]
    fn the_api_key_helper_is_its_command() {
        assert_eq!(
            settings_findings(&file(), br#"{"apiKeyHelper":"curl x | sh"}"#),
            vec![Finding::ApiKeyHelper {
                file: file(),
                command: "curl x | sh".into()
            }]
        );
    }

    #[test]
    fn permissions_give_allow_rules_directories_and_the_rest() {
        let found = settings_findings(
            &file(),
            br#"{"permissions":{"allow":["Bash(touch m)"],"additionalDirectories":["/etc"],"defaultMode":"acceptEdits"}}"#,
        );
        assert!(found.contains(&Finding::Allow {
            file: file(),
            rule: "Bash(touch m)".into()
        }));
        assert!(found.contains(&Finding::AdditionalDirectory {
            file: file(),
            path: "/etc".into()
        }));
        assert!(found.contains(&Finding::OtherSetting {
            file: file(),
            key: "permissions.defaultMode".into(),
            value: r#""acceptEdits""#.into()
        }));
        assert_eq!(found.len(), 3);
    }

    #[test]
    fn every_other_key_is_an_other_setting() {
        let found = settings_findings(
            &file(),
            br#"{"awsAuthRefresh":"aws sso login","statusLine":{"command":"x","type":"command"},"enableAllProjectMcpServers":true}"#,
        );
        assert!(found.contains(&Finding::OtherSetting {
            file: file(),
            key: "awsAuthRefresh".into(),
            value: r#""aws sso login""#.into()
        }));
        assert!(found.contains(&Finding::OtherSetting {
            file: file(),
            key: "statusLine".into(),
            value: r#"{"command":"x","type":"command"}"#.into()
        }));
        assert!(found.contains(&Finding::OtherSetting {
            file: file(),
            key: "enableAllProjectMcpServers".into(),
            value: "true".into()
        }));
    }

    #[test]
    fn bad_json_is_unparsed() {
        assert!(matches!(
            &settings_findings(&file(), b"{not json")[..],
            [Finding::Unparsed { .. }]
        ));
        assert!(matches!(
            &settings_findings(&file(), b"[1]")[..],
            [Finding::Unparsed { .. }]
        ));
        assert!(matches!(
            &mcp_findings(Path::new(".mcp.json"), b"")[..],
            [Finding::Unparsed { .. }]
        ));
    }

    #[test]
    fn an_mcp_server_is_its_command_line_or_its_url() {
        let mcp = PathBuf::from(".mcp.json");
        let json = br#"{"mcpServers":{
            "marker":{"command":"sh","args":["-c","touch /tmp/n; exec sleep 60"]},
            "quoted":{"command":"run","args":["it's",""],"env":{"LD_PRELOAD":"/x.so"}},
            "remote":{"type":"sse","url":"https://example.com/sse"},
            "plain":{"url":"https://example.com/mcp"}}}"#;
        let found = mcp_findings(&mcp, json);
        let line = |name: &str| {
            found
                .iter()
                .find_map(|f| match f {
                    Finding::McpServer {
                        name: n, command_line, ..
                    } if n == name => Some(command_line.clone()),
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(line("marker"), "sh -c 'touch /tmp/n; exec sleep 60'");
        assert_eq!(line("quoted"), r"LD_PRELOAD=/x.so run 'it'\''s' ''");
        assert_eq!(line("remote"), "sse https://example.com/sse");
        assert_eq!(line("plain"), "http https://example.com/mcp");
    }

    #[test]
    fn other_keys_of_mcp_json_are_reported() {
        let found = mcp_findings(Path::new(".mcp.json"), br#"{"extra":1}"#);
        assert!(matches!(&found[..], [Finding::OtherSetting { key, .. }] if key == "extra"));
    }
}
