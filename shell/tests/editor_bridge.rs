//! `eitri-editor-bridge` between an MCP client and a window: the test is the client on the bridge's stdio and the
//! host on a socket of its own, so every promise here is about bytes the real binary read and wrote.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eitri_core::editor_tools::bridge_wire::{read_frame, write_frame, Message, PROTOCOL};
use eitri_core::editor_tools::words::MOVED;
use serde_json::{json, Value};

/// How long a test waits for something that must happen. Only a failing test waits this long.
const PATIENCE: Duration = Duration::from_secs(30);

const KEY: &str = "the-key";

/// A private directory for one test, gone when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Scratch {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let name = format!("br-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
        std::fs::create_dir_all(&dir).expect("the scratch directory");
        Scratch(dir)
    }

    fn socket(&self) -> PathBuf {
        agent::socket_path::in_dir(&self.0, "s.sock").expect("a short enough path")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The window's side: a listener on the socket.
struct Host {
    listener: UnixListener,
    path: PathBuf,
}

impl Host {
    fn bind(path: PathBuf) -> Host {
        let listener = UnixListener::bind(&path).expect("binds");
        listener.set_nonblocking(true).expect("non-blocking");
        Host { listener, path }
    }

    /// Takes the socket away, as a window does when its tab leaves.
    fn unbind(self) {
        std::fs::remove_file(&self.path).expect("the socket file");
    }

    fn accept(&self) -> Link {
        let deadline = Instant::now() + PATIENCE;
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => return Link::new(stream),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "the bridge never connected");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept: {error}"),
            }
        }
    }

    /// Accepts the bridge, checks its hello and answers with `tools`.
    fn greet(&self, tools: Vec<Value>) -> Link {
        let mut link = self.accept();
        assert_eq!(
            link.read(),
            Message::Hello {
                v: PROTOCOL,
                key: KEY.to_string()
            }
        );
        link.send(Message::Welcome { tools });
        link
    }
}

struct Link(UnixStream);

impl Link {
    fn new(stream: UnixStream) -> Link {
        stream.set_nonblocking(false).expect("blocking");
        stream.set_read_timeout(Some(PATIENCE)).expect("a timeout");
        Link(stream)
    }

    fn read(&mut self) -> Message {
        let frame = read_frame(&mut self.0).expect("a frame").expect("not the end");
        Message::parse(frame).expect("a message")
    }

    fn read_call(&mut self) -> u64 {
        match self.read() {
            Message::Call { id, .. } => id,
            other => panic!("not a call: {other:?}"),
        }
    }

    fn send(&mut self, message: Message) {
        write_frame(&mut self.0, &message.into_value()).expect("writes");
    }
}

/// The bridge as a client holds it. Dropping it kills the child, so a failing test leaves nothing running.
struct Bridge {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Value>,
}

impl Bridge {
    fn start(socket: &Path, key: &str) -> Bridge {
        let mut child = Command::new(env!("CARGO_BIN_EXE_eitri-editor-bridge"))
            .arg(socket)
            .arg(key)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the bridge starts");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("piped");
        let (send, lines) = channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.expect("a line");
                let value = serde_json::from_str(&line).expect("each line is one JSON value");
                if send.send(value).is_err() {
                    return;
                }
            }
        });
        Bridge { child, stdin, lines }
    }

    fn say(&mut self, message: &Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.say_raw(line.as_bytes());
    }

    fn say_raw(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        stdin.write_all(bytes).expect("writes");
        stdin.flush().expect("flushes");
    }

    fn hear(&self) -> Value {
        self.lines.recv_timeout(PATIENCE).expect("an answer")
    }

    fn ask(&mut self, message: &Value) -> Value {
        self.say(message);
        self.hear()
    }

    /// Asks a ping and returns once it is answered, so a line heard before it would fail the check.
    fn nothing_else_came(&mut self) {
        let pong = self.ask(&json!({"jsonrpc": "2.0", "id": "ping", "method": "ping"}));
        assert_eq!(pong, json!({"jsonrpc": "2.0", "id": "ping", "result": {}}));
    }

    fn call(&mut self, id: u64, name: &str) {
        self.say(&call_request(id, name, json!({"path": "a"})));
    }

    fn assert_reconnect_budget_exhausted(&mut self, socket: &Path) {
        assert_eq!(self.wait().code(), Some(1));
        let mut stderr = String::new();
        self.child.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
        assert!(
            stderr
                .lines()
                .any(|line| line == format!("eitri-editor-bridge: no host at {}", socket.display())),
            "missing no-host diagnostic: {stderr}"
        );
        #[cfg(feature = "probe")]
        assert!(
            stderr
                .lines()
                .any(|line| line == "eitri-editor-bridge: reconnect budget 5s exhausted"),
            "missing reconnect-budget diagnostic: {stderr}"
        );
    }

    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            assert!(Instant::now() < deadline, "the bridge did not exit");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn call_request(id: u64, name: &str, arguments: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": arguments}})
}

fn tools() -> Vec<Value> {
    vec![json!({
        "name": "open_file",
        "description": "Show a file",
        "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}},
        "annotations": {"readOnlyHint": true},
    })]
}

/// A bridge greeted by a host, ready for calls.
fn connected(scratch: &Scratch) -> (Bridge, Host, Link) {
    let host = Host::bind(scratch.socket());
    let bridge = Bridge::start(&host.path, KEY);
    let link = host.greet(tools());
    (bridge, host, link)
}

fn text_result(text: &str, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

#[test]
fn initialize_echoes_the_clients_version_and_server_discover_is_method_not_found() {
    let scratch = Scratch::new();
    let (mut bridge, _host, _link) = connected(&scratch);
    let answer = bridge.ask(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2099-01-01", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}},
    }));
    assert_eq!(answer["jsonrpc"], "2.0");
    assert_eq!(answer["id"], 1);
    assert_eq!(answer["result"]["protocolVersion"], "2099-01-01");
    assert_eq!(answer["result"]["capabilities"], json!({"tools": {}}));
    assert_eq!(answer["result"]["serverInfo"]["name"], "eitri");
    assert!(
        answer["result"]["serverInfo"]["version"]
            .as_str()
            .is_some_and(|v| !v.is_empty()),
        "{answer}"
    );

    bridge.say(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    bridge.nothing_else_came();

    let answer = bridge.ask(&json!({"jsonrpc": "2.0", "id": 2, "method": "server/discover"}));
    assert_eq!(answer["id"], 2);
    assert_eq!(answer["error"]["code"], -32601);
    let answer = bridge.ask(&json!({"jsonrpc": "2.0", "id": 3, "method": "resources/list", "params": {}}));
    assert_eq!(answer["error"]["code"], -32601);
}

#[test]
fn initialize_with_no_version_is_invalid_params() {
    let scratch = Scratch::new();
    let (mut bridge, _host, _link) = connected(&scratch);
    let answer = bridge.ask(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}));
    assert_eq!(answer["error"]["code"], -32602);
}

#[test]
fn tools_list_is_the_welcomes() {
    let scratch = Scratch::new();
    let (mut bridge, _host, _link) = connected(&scratch);
    let answer = bridge.ask(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}));
    assert_eq!(answer["result"], json!({"tools": tools()}));
}

#[test]
fn a_tools_list_before_the_welcome_waits_for_it() {
    let scratch = Scratch::new();
    let host = Host::bind(scratch.socket());
    let mut bridge = Bridge::start(&host.path, KEY);
    bridge.say(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}));
    bridge.nothing_else_came();
    let _link = host.greet(tools());
    assert_eq!(bridge.hear()["result"], json!({"tools": tools()}));
}

#[test]
fn a_call_goes_through_and_its_result_comes_back() {
    let scratch = Scratch::new();
    let (mut bridge, _host, mut link) = connected(&scratch);

    bridge.say(&call_request(5, "open_file", json!({"path": "src/a.rs", "line": 3})));
    let Message::Call {
        id,
        name,
        arguments,
        meta,
    } = link.read()
    else {
        panic!("not a call");
    };
    assert_eq!(name, "open_file");
    assert_eq!(arguments, json!({"path": "src/a.rs", "line": 3}));
    assert_eq!(meta, Value::Null);
    link.send(Message::Result {
        id,
        text: "shown".into(),
        is_error: false,
    });
    let answer = bridge.hear();
    assert_eq!(answer["id"], 5);
    assert_eq!(answer["result"], text_result("shown", false));

    bridge.say(&call_request(6, "open_file", json!({})));
    let id = link.read_call();
    link.send(Message::Result {
        id,
        text: "no such file".into(),
        is_error: true,
    });
    let answer = bridge.hear();
    assert_eq!(answer["id"], 6);
    assert_eq!(answer["result"], text_result("no such file", true));
}

#[test]
fn a_call_with_no_arguments_sends_an_empty_object() {
    let scratch = Scratch::new();
    let (mut bridge, _host, mut link) = connected(&scratch);
    bridge.say(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "editor_state"}}));
    let Message::Call { arguments, .. } = link.read() else {
        panic!("not a call");
    };
    assert_eq!(arguments, json!({}));
}

#[test]
fn a_call_out_of_shape_is_invalid_params_and_reaches_no_host() {
    let scratch = Scratch::new();
    let (mut bridge, _host, mut link) = connected(&scratch);
    for (id, params) in [
        (1, json!({})),
        (2, json!({"name": 7})),
        (3, json!({"name": "open_file", "arguments": [1]})),
    ] {
        let request = json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params});
        let answer = bridge.ask(&request);
        assert_eq!(answer["id"], id);
        assert_eq!(answer["error"]["code"], -32602);
    }
    bridge.call(4, "open_file");
    let id = link.read_call();
    link.send(Message::Result {
        id,
        text: "ok".into(),
        is_error: false,
    });
    assert_eq!(bridge.hear()["id"], 4);
}

#[test]
fn a_string_id_comes_back_as_it_was() {
    let scratch = Scratch::new();
    let (mut bridge, _host, mut link) = connected(&scratch);
    let mut request = call_request(0, "open_file", json!({}));
    request["id"] = json!("an id");
    bridge.say(&request);
    let id = link.read_call();
    link.send(Message::Result {
        id,
        text: "ok".into(),
        is_error: false,
    });
    assert_eq!(bridge.hear()["id"], "an id");
}

#[test]
fn a_call_too_long_for_a_frame_is_answered_and_the_bridge_goes_on() {
    let scratch = Scratch::new();
    let (mut bridge, _host, mut link) = connected(&scratch);
    let answer = bridge.ask(&call_request(1, "open_file", json!({"path": "x".repeat(1024 * 1024)})));
    assert_eq!(answer["id"], 1);
    assert_eq!(answer["result"]["isError"], true, "{}", answer["result"]);
    bridge.call(2, "open_file");
    let id = link.read_call();
    link.send(Message::Result {
        id,
        text: "ok".into(),
        is_error: false,
    });
    assert_eq!(bridge.hear()["id"], 2);
}

#[test]
fn a_line_past_every_limit_is_refused_whole_and_the_next_line_is_served() {
    let scratch = Scratch::new();
    let (mut bridge, _host, _link) = connected(&scratch);
    let mut line = vec![b'x'; 5 * 1024 * 1024];
    line.push(b'\n');
    bridge.say_raw(&line);
    let answer = bridge.hear();
    assert_eq!(answer["id"], Value::Null);
    assert_eq!(answer["error"]["code"], -32700);
    bridge.nothing_else_came();
}

#[test]
fn lines_that_are_no_request_are_refused_and_the_bridge_goes_on() {
    let scratch = Scratch::new();
    let (mut bridge, _host, _link) = connected(&scratch);
    bridge.say_raw(b"not json\n");
    let answer = bridge.hear();
    assert_eq!(answer["id"], Value::Null);
    assert_eq!(answer["error"]["code"], -32700);
    bridge.say_raw(b"[1]\n");
    let answer = bridge.hear();
    assert_eq!(answer["id"], Value::Null);
    assert_eq!(answer["error"]["code"], -32600);
    // An empty line, an unknown notification and a stray response want no answer.
    bridge.say_raw(b"\n");
    bridge.say(&json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {}}));
    bridge.say(&json!({"jsonrpc": "2.0", "id": 9, "result": {}}));
    bridge.nothing_else_came();
}

#[test]
fn cancelled_is_passed_on() {
    let scratch = Scratch::new();
    let (mut bridge, _host, mut link) = connected(&scratch);
    bridge.call(7, "open_file");
    let id = link.read_call();
    bridge.say(&json!({
        "jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 7, "reason": "stopped"},
    }));
    assert_eq!(link.read(), Message::Cancel { id });

    // A cancel leaves the call open at the bridge: a result that comes is written, and a client ignores it.
    link.send(Message::Result {
        id,
        text: "late".into(),
        is_error: false,
    });
    let answer = bridge.hear();
    assert_eq!(answer["id"], 7);
    assert_eq!(answer["result"], text_result("late", false));
}

#[test]
fn a_cancel_for_nothing_open_sends_nothing() {
    let scratch = Scratch::new();
    let (mut bridge, _host, mut link) = connected(&scratch);
    bridge.say(&json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 99}}));
    bridge.call(1, "open_file");
    link.read_call();
}

#[test]
fn a_wrong_key_closed_by_the_host_exits() {
    let scratch = Scratch::new();
    let host = Host::bind(scratch.socket());
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let closing = thread::spawn(move || {
        let mut hellos = 0;
        while !stopped.load(Ordering::Relaxed) {
            match host.listener.accept() {
                Ok((stream, _)) => {
                    // Closing is the refusal. A connection the bridge dropped as it exited has no hello to read.
                    if let Ok(Some(frame)) = read_frame(&mut Link::new(stream).0) {
                        let hello = Message::parse(frame).expect("a message");
                        assert_eq!(
                            hello,
                            Message::Hello {
                                v: PROTOCOL,
                                key: "wrong".to_string()
                            }
                        );
                        hellos += 1;
                    }
                }
                Err(_) => thread::sleep(Duration::from_millis(5)),
            }
        }
        hellos
    });

    let mut bridge = Bridge::start(&scratch.socket(), "wrong");
    let status = bridge.wait();
    stop.store(true, Ordering::Relaxed);
    let hellos = closing.join().expect("the host thread");
    assert!(hellos >= 1, "the bridge never said hello");
    assert_eq!(status.code(), Some(1));
}

#[test]
fn a_reconnect_across_an_unbind_and_rebind_keeps_serving() {
    let scratch = Scratch::new();
    let (mut bridge, host, mut link) = connected(&scratch);

    // A call open at the drop: its answer proves the bridge has seen the drop before the next call goes in.
    bridge.call(1, "open_file");
    link.read_call();
    drop(link);
    host.unbind();
    assert_eq!(bridge.hear()["id"], 1);

    bridge.call(2, "open_file");
    let host = Host::bind(scratch.socket());
    let mut link = host.greet(tools());
    let id = link.read_call();
    link.send(Message::Result {
        id,
        text: "after".into(),
        is_error: false,
    });
    let answer = bridge.hear();
    assert_eq!(answer["id"], 2);
    assert_eq!(answer["result"], text_result("after", false));
}

#[test]
fn a_call_open_across_the_drop_is_answered_once() {
    let scratch = Scratch::new();
    let (mut bridge, host, mut link) = connected(&scratch);

    bridge.call(1, "open_file");
    let answered = link.read_call();
    link.send(Message::Result {
        id: answered,
        text: "done".into(),
        is_error: false,
    });
    bridge.call(2, "open_file");
    let open = link.read_call();
    assert_ne!(answered, open);
    assert_eq!(bridge.hear()["result"], text_result("done", false));

    drop(link);
    host.unbind();
    let answer = bridge.hear();
    assert_eq!(answer["id"], 2);
    assert_eq!(answer["result"], text_result(MOVED, true));

    // A result reaching the bridge late, on the next connection, is for a call already over.
    let host = Host::bind(scratch.socket());
    let mut link = host.greet(tools());
    link.send(Message::Result {
        id: open,
        text: "late".into(),
        is_error: false,
    });
    link.send(Message::Result {
        id: answered,
        text: "late".into(),
        is_error: false,
    });
    bridge.call(3, "open_file");
    let id = link.read_call();
    link.send(Message::Result {
        id,
        text: "ok".into(),
        is_error: false,
    });
    assert_eq!(bridge.hear()["id"], 3);
    bridge.nothing_else_came();
}

#[test]
fn a_frame_out_of_shape_from_the_host_counts_as_a_drop() {
    let scratch = Scratch::new();
    let (mut bridge, host, mut link) = connected(&scratch);
    bridge.call(1, "open_file");
    link.read_call();
    write_frame(&mut link.0, &json!({"t": "result"})).expect("writes");
    assert_eq!(bridge.hear()["result"], text_result(MOVED, true));
    let _link = host.greet(tools());
    bridge.nothing_else_came();
}

#[test]
fn no_host_exhausts_the_reconnect_budget_and_exits_1() {
    let scratch = Scratch::new();
    let mut bridge = Bridge::start(&scratch.socket(), KEY);
    bridge.assert_reconnect_budget_exhausted(&scratch.socket());
}

#[test]
fn a_dropped_host_exhausts_the_reconnect_budget_and_exits_1() {
    let scratch = Scratch::new();
    let (mut bridge, host, link) = connected(&scratch);
    // A ping answered after the welcome shows nothing of the connection is in doubt when it goes.
    bridge.nothing_else_came();
    drop(link);
    host.unbind();
    bridge.assert_reconnect_budget_exhausted(&scratch.socket());
}

#[test]
fn what_still_waits_when_no_host_comes_is_answered_before_the_exit() {
    let scratch = Scratch::new();
    let mut bridge = Bridge::start(&scratch.socket(), KEY);
    bridge.call(1, "open_file");
    bridge.say(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let answer = bridge.hear();
    assert_eq!(answer["id"], 1);
    assert_eq!(answer["result"]["isError"], true, "{answer}");
    let answer = bridge.hear();
    assert_eq!(answer["id"], 2);
    assert!(answer["error"]["code"].is_i64(), "{answer}");
    assert_eq!(bridge.wait().code(), Some(1));
}

#[test]
fn stdin_ending_exits_0() {
    let scratch = Scratch::new();
    let (mut bridge, _host, _link) = connected(&scratch);
    drop(bridge.stdin.take());
    assert_eq!(bridge.wait().code(), Some(0));
}

#[test]
fn meta_is_passed_through_whole() {
    let scratch = Scratch::new();
    let (mut bridge, _host, mut link) = connected(&scratch);
    let meta = json!({"callId": "call_1", "itemId": "fc_2", "nested": {"a": [1, 2, {"b": null}]}});
    bridge.say(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "open_file", "arguments": {}, "_meta": meta},
    }));
    let Message::Call { meta: passed, .. } = link.read() else {
        panic!("not a call");
    };
    assert_eq!(passed, meta);
}

#[test]
fn a_bad_command_line_exits_2_before_anything() {
    let status = Command::new(env!("CARGO_BIN_EXE_eitri-editor-bridge"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("the bridge starts");
    assert_eq!(status.code(), Some(2));
}
