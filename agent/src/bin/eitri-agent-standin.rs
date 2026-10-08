//! A scripted stand-in for an agent child (`codex app-server`, an ACP agent) in the `JsonRpcLink`
//! tests. The one argument is a JSON array of steps, run in order:
//!
//! - `{"write": "<line>"}`: the text and a newline on stdout.
//! - `{"write_raw": "<text>"}`: the text alone, to split a line across reads.
//! - `{"write_big": {"prefix": "<text>", "bytes": N, "suffix": "<text>"}}`: the prefix, `N` filler
//!   bytes, the optional suffix and a newline: one line of any size.
//! - `{"stderr": "<text>"}`: the text and a newline on stderr.
//! - `{"expect": "<regex>"}`: reads one line from stdin; exit 3 if it does not match.
//! - `{"sleep_ms": N}`: does not read or write for that long.
//! - `{"ignore_eof": true}`: after the last step, never exit on its own and never notice stdin closing.
//! - `{"exit": code}`: exits at once.
//!
//! After the last step it reads stdin until EOF and exits 0, as a well-behaved child does.
//!
//! With `--mcp` instead of a script it is a minimal MCP server on stdio (newline-delimited JSON-RPC
//! 2.0) offering one tool, `probe_write`, annotated destructive so an agent asks before calling it. The
//! tool appends `line` and a newline to the file named by `path` and answers `wrote`.

use std::io::{self, BufRead, Write};
use std::time::Duration;

use serde_json::Value;

fn fail(code: i32, message: &str) -> ! {
    eprintln!("eitri-agent-standin: {message}");
    std::process::exit(code);
}

fn main() {
    let Some(script) = std::env::args().nth(1) else {
        fail(2, "no script argument")
    };
    if script == "--mcp" {
        mcp_server();
        return;
    }
    let steps: Vec<Value> = match serde_json::from_str(&script) {
        Ok(steps) => steps,
        Err(e) => fail(2, &format!("script is not a JSON array: {e}")),
    };
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut ignore_eof = false;

    for step in &steps {
        if let Some(line) = step.get("write").and_then(Value::as_str) {
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        } else if let Some(text) = step.get("write_raw").and_then(Value::as_str) {
            let _ = out.write_all(text.as_bytes());
            let _ = out.flush();
        } else if let Some(big) = step.get("write_big") {
            let prefix = big.get("prefix").and_then(Value::as_str).unwrap_or("");
            let suffix = big.get("suffix").and_then(Value::as_str).unwrap_or("");
            let mut left = big.get("bytes").and_then(Value::as_u64).unwrap_or(0) as usize;
            let _ = out.write_all(prefix.as_bytes());
            let chunk = vec![b'x'; 64 * 1024];
            while left > 0 {
                let n = left.min(chunk.len());
                if out.write_all(&chunk[..n]).is_err() {
                    break;
                }
                left -= n;
            }
            let _ = out.write_all(suffix.as_bytes());
            let _ = out.write_all(b"\n");
            let _ = out.flush();
        } else if let Some(text) = step.get("stderr").and_then(Value::as_str) {
            eprintln!("{text}");
        } else if let Some(pattern) = step.get("expect").and_then(Value::as_str) {
            let re = match regex::Regex::new(pattern) {
                Ok(re) => re,
                Err(e) => fail(2, &format!("bad regex {pattern:?}: {e}")),
            };
            let mut line = String::new();
            match input.read_line(&mut line) {
                Ok(0) => fail(4, &format!("stdin closed while expecting {pattern:?}")),
                Ok(_) => {
                    let line = line.trim_end_matches(['\r', '\n']);
                    if !re.is_match(line) {
                        fail(3, &format!("line {line:?} does not match {pattern:?}"));
                    }
                }
                Err(e) => fail(4, &format!("stdin read failed: {e}")),
            }
        } else if let Some(ms) = step.get("sleep_ms").and_then(Value::as_u64) {
            std::thread::sleep(Duration::from_millis(ms));
        } else if step.get("ignore_eof").and_then(Value::as_bool) == Some(true) {
            ignore_eof = true;
        } else if let Some(code) = step.get("exit").and_then(Value::as_i64) {
            std::process::exit(code as i32);
        } else {
            fail(2, &format!("unknown step {step}"));
        }
    }

    if ignore_eof {
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    let mut sink = String::new();
    while matches!(input.read_line(&mut sink), Ok(n) if n > 0) {
        sink.clear();
    }
}

/// The one tool the MCP mode offers, as `tools/list` describes it.
fn probe_write_tool() -> Value {
    serde_json::json!({
        "name": "probe_write",
        "description": "Appends a line to a file.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "The file to append to."},
                "line": {"type": "string", "description": "The line to append."}
            },
            "required": ["path", "line"]
        },
        "annotations": {"destructiveHint": true, "readOnlyHint": false, "openWorldHint": false}
    })
}

fn probe_write(arguments: &Value) -> Result<Value, String> {
    let path = arguments.get("path").and_then(Value::as_str).ok_or("no path")?;
    let line = arguments.get("line").and_then(Value::as_str).unwrap_or("");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("cannot open {path}: {e}"))?;
    writeln!(file, "{line}").map_err(|e| format!("cannot write {path}: {e}"))?;
    Ok(serde_json::json!({"content": [{"type": "text", "text": "wrote"}], "isError": false}))
}

fn mcp_server() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // A notification needs no answer.
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let result = match message.get("method").and_then(Value::as_str).unwrap_or("") {
            "initialize" => Ok(serde_json::json!({
                "protocolVersion": params.get("protocolVersion").cloned().unwrap_or(Value::from("2025-06-18")),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "eitri-probe", "version": "0"}
            })),
            "ping" => Ok(serde_json::json!({})),
            "tools/list" => Ok(serde_json::json!({"tools": [probe_write_tool()]})),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                if name == "probe_write" {
                    probe_write(params.get("arguments").unwrap_or(&Value::Null))
                        .or_else(|e| Ok(serde_json::json!({"content": [{"type": "text", "text": e}], "isError": true})))
                } else {
                    Err((-32602, format!("no tool {name}")))
                }
            }
            other => Err((-32601, format!("method not found: {other}"))),
        };
        let answer = match result {
            Ok(result) => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => {
                serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        };
        if writeln!(out, "{answer}").and_then(|_| out.flush()).is_err() {
            break;
        }
    }
}
