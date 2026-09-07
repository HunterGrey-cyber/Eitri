use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};

#[test]
fn agent_hook_relays_stdin_to_socket_and_prints_the_response() {
    let socket_path = std::env::temp_dir().join(format!("agent-hook-test-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).unwrap();

    let hook_input = r#"{"session_id":"s1","transcript_path":"/tmp/t","cwd":"/tmp","prompt_id":"p1","permission_mode":"default","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo hi"},"tool_use_id":"toolu_1"}"#;

    // Spawn agent-hook as a real subprocess, matching exactly how the CLI itself will invoke it.
    let binary = env!("CARGO_BIN_EXE_agent-hook");
    let mut child = Command::new(binary)
        .env("NEOVIBE_AGENT_HOOK_SOCKET", &socket_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(hook_input.as_bytes()).unwrap();

    // Act as the parent process: accept the connection, read what agent-hook forwarded, verify
    // it's the real payload (not just the parsed subset), then send back a decision.
    let (mut stream, _) = listener.accept().unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut received = String::new();
    reader.read_line(&mut received).unwrap();
    let received_json: serde_json::Value = serde_json::from_str(received.trim()).unwrap();
    assert_eq!(received_json["tool_name"], "Bash");
    assert_eq!(received_json["tool_use_id"], "toolu_1");

    stream.write_all(b"{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"allow\"}}\n").unwrap();

    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let printed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(printed["hookSpecificOutput"]["permissionDecision"], "allow");

    let _ = std::fs::remove_file(&socket_path);
}
