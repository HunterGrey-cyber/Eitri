use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};

#[test]
fn agent_hook_relays_stdin_to_socket_and_prints_the_response() {
    let socket_path = agent::socket_path::in_dir(&std::env::temp_dir(), &format!("agent-hook-test-{}.sock", std::process::id()))
        .expect("the test socket path must fit the macOS socket-path limit");
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

/// The exit code IS the gate, so each failure path gets its own test.
///
/// Claude Code treats exit 2 as "block this tool call" and feeds stderr back to the model; exit 1
/// is a non-blocking error and exit 0 with empty stdout means "continue through the normal
/// permission flow". Both of the latter let the call RUN. Until 2026-09-18 every failure here took
/// one of those two paths, so a missing socket, a dropped connection or a host that died mid-
/// decision each ran the tool with no permission card and nothing recording that a gate had been
/// asked. A gate that fails open is worse than no gate, because the absence of a card reads as
/// "nothing needed approval".
///
/// These assert the code, not just "it failed": a test that accepted any non-zero status would
/// have passed against the old binary, which is exactly how this survived.
fn spawn_hook(socket: Option<&std::path::Path>, payload: &str) -> std::process::Output {
    let binary = env!("CARGO_BIN_EXE_agent-hook");
    let mut command = Command::new(binary);
    match socket {
        Some(path) => { command.env("NEOVIBE_AGENT_HOOK_SOCKET", path); }
        None => { command.env_remove("NEOVIBE_AGENT_HOOK_SOCKET"); }
    }
    let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn assert_blocked(output: &std::process::Output, because: &str) {
    assert_eq!(
        output.status.code(),
        Some(2),
        "a failure to reach the gate must BLOCK (exit 2). exit 1 and exit 0 both let the tool run.\n\
         stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty(), "a blocked call must print no decision");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(because), "stderr must name the cause, got: {stderr}");
}

#[test]
fn an_unset_socket_variable_blocks_rather_than_letting_the_tool_run() {
    let output = spawn_hook(None, "{}");
    assert_blocked(&output, "NEOVIBE_AGENT_HOOK_SOCKET");
}

#[test]
fn a_socket_that_is_not_there_blocks() {
    let missing = agent::socket_path::in_dir(&std::env::temp_dir(), &format!("ah-gone-{}.sock", uuid::Uuid::new_v4().simple())).unwrap();
    let output = spawn_hook(Some(&missing), "{}");
    assert_blocked(&output, "could not connect");
}

/// The path that used to be silent: the relay accepts, then closes without answering. `read_line`
/// returns `Ok(0)`, the old binary printed an empty string and exited 0, and the CLI read that as
/// "no opinion, carry on". `agent::process` already called a dropped connection a silent allow in a
/// comment; this makes the binary agree with it.
#[test]
fn a_host_that_closes_without_answering_blocks_instead_of_exiting_zero() {
    let socket_path = agent::socket_path::in_dir(&std::env::temp_dir(), &format!("ah-drop-{}.sock", uuid::Uuid::new_v4().simple())).expect("the test socket path must fit the macOS socket-path limit");
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).unwrap();
    let accepting = std::thread::spawn(move || {
        // Accept, read the request, then drop the stream without writing a decision.
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut received = String::new();
        let _ = reader.read_line(&mut received);
        let _ = stream.flush();
        drop(stream);
    });

    let output = spawn_hook(Some(&socket_path), "{\"tool_name\":\"Bash\"}");
    accepting.join().unwrap();
    assert_blocked(&output, "closed the connection without answering");
    let _ = std::fs::remove_file(&socket_path);
}

/// A host that answers with a blank line is the same hazard wearing different clothes: non-empty
/// read, empty decision, and the old binary would have printed nothing and exited 0.
#[test]
fn an_empty_decision_blocks() {
    let socket_path = agent::socket_path::in_dir(&std::env::temp_dir(), &format!("ah-blank-{}.sock", uuid::Uuid::new_v4().simple())).expect("the test socket path must fit the macOS socket-path limit");
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).unwrap();
    let accepting = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut received = String::new();
        let _ = reader.read_line(&mut received);
        let _ = stream.write_all(b"   \n");
    });

    let output = spawn_hook(Some(&socket_path), "{\"tool_name\":\"Bash\"}");
    accepting.join().unwrap();
    assert_blocked(&output, "empty decision");
    let _ = std::fs::remove_file(&socket_path);
}
