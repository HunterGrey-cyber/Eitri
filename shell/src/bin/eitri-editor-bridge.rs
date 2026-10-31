//! `eitri-editor-bridge <socket> <key>`: the tool server an agent starts for a Codex or ACP tab. MCP over stdio on one
//! side, newline-delimited JSON-RPC; the bridge socket to the window serving the tab on the other. Nothing is decided
//! here: every tool's name, schema and answer is the window's, and this process only carries them. It links
//! `serde_json` and `std`, so it starts quickly and maps nothing of a window's.
//!
//! One loop owns stdout and every table, fed by a thread reading stdin and a thread holding the socket's reading end.
//! The connection comes and goes: a tab moving to the other window unbinds the socket here and binds it there, so a
//! lost connection is tried again for [`RECONNECT_LIMIT`] before the bridge gives up. A call that was open when the
//! connection dropped may or may not have run, and no later window knows of it, so it is answered then, once; a
//! call made while nothing is connected waits for the next connection.

// This binary uses the writing and blocking half; the non-blocking half is the window's.
#[allow(dead_code)]
#[path = "../../../core/src/editor_tools/bridge_wire.rs"]
mod bridge_wire;

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, BufRead, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

use bridge_wire::{encode_frame, read_frame, write_frame, Message, MAX_FRAME, PROTOCOL};
use serde_json::{json, Value};

/// How long the bridge goes on without a host: from its start, or from the moment a connection dropped.
const RECONNECT_LIMIT: Duration = Duration::from_secs(5);

const RETRY_EVERY: Duration = Duration::from_millis(100);

/// How long a write to the host may block. A window's worker drains its connections, so this only ends a host that
/// has stopped: past it the connection counts as lost, and the loop is free to read stdin again.
const WRITE_LIMIT: Duration = RECONNECT_LIMIT;

/// The longest line read from the client. Longer than a frame, so a call too long for a frame still has its id read
/// and is answered by it.
const MAX_LINE: u64 = 4 * MAX_FRAME as u64;

/// Events waiting for the loop. Bounded, so a client writing faster than the bridge answers waits at its own pipe.
const EVENTS_WAITING: usize = 64;

/// A call open when its connection dropped. Held to `words::MOVED` by this binary's test: it links no `eitri_core`.
const MOVED: &str = "the tab moved to the other window; try again";

const NO_HOST: &str = "no window is serving this tab";

const TOO_LONG: &str = "the call is too long for the editor's socket; send less";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let (Some(socket), Some(key), None) = (args.next(), args.next(), args.next()) else {
        eprintln!("usage: eitri-editor-bridge <socket> <key>");
        return ExitCode::from(2);
    };
    let Some(key) = key.to_str().map(str::to_owned) else {
        eprintln!("eitri-editor-bridge: the key is not text");
        return ExitCode::from(2);
    };
    let (events, incoming) = sync_channel(EVENTS_WAITING);
    let from_stdin = events.clone();
    std::thread::spawn(move || read_stdin(from_stdin));
    let socket = PathBuf::from(socket);
    let shown = socket.display().to_string();
    std::thread::spawn(move || hold_connection(&socket, &key, events));

    let stdout = io::stdout().lock();
    let code = Bridge::new(stdout).run(incoming);
    if code != 0 {
        eprintln!("eitri-editor-bridge: no host at {shown}");
    }
    ExitCode::from(code)
}

enum Event {
    Line(Line),
    StdinEnded,
    /// A connection the host welcomed. `writer` is the loop's to write on.
    Up {
        writer: UnixStream,
        tools: Vec<Value>,
    },
    Host(Message),
    /// The connection of the last `Up` is over; everything it delivered came before this.
    Down,
}

enum Line {
    Text(Vec<u8>),
    TooLong,
}

fn read_stdin(events: SyncSender<Event>) {
    let mut input = io::stdin().lock();
    while let Ok(Some(line)) = next_line(&mut input) {
        if events.send(Event::Line(line)).is_err() {
            return;
        }
    }
    let _ = events.send(Event::StdinEnded);
}

/// One line, or `None` at the end. A line past [`MAX_LINE`] is read through and kept no further.
fn next_line(input: &mut impl BufRead) -> io::Result<Option<Line>> {
    let mut line = Vec::new();
    if Read::take(&mut *input, MAX_LINE + 1).read_until(b'\n', &mut line)? == 0 {
        return Ok(None);
    }
    if line.len() as u64 <= MAX_LINE {
        return Ok(Some(Line::Text(line)));
    }
    if line.last() != Some(&b'\n') {
        input.skip_until(b'\n')?;
    }
    Ok(Some(Line::TooLong))
}

/// Holds one connection after another for as long as the process lives. Whether to go on without one is the loop's
/// to say: this thread can be stuck in a `connect` to a host that has stopped accepting.
fn hold_connection(socket: &Path, key: &str, events: SyncSender<Event>) {
    loop {
        let Some((mut reader, writer, tools)) = greet(socket, key) else {
            std::thread::sleep(RETRY_EVERY);
            continue;
        };
        if events.send(Event::Up { writer, tools }).is_err() {
            return;
        }
        // A frame out of shape ends the connection like its end does: a result that cannot be read leaves the same
        // call open as a result that never came.
        while let Ok(Some(frame)) = read_frame(&mut reader) {
            let Ok(message) = Message::parse(frame) else { break };
            if events.send(Event::Host(message)).is_err() {
                return;
            }
        }
        if events.send(Event::Down).is_err() {
            return;
        }
    }
}

/// A connection counts from its `welcome`: a host that closes on a `hello`, as it does for a wrong key, is no host.
fn greet(socket: &Path, key: &str) -> Option<(UnixStream, UnixStream, Vec<Value>)> {
    let mut stream = UnixStream::connect(socket).ok()?;
    stream.set_read_timeout(Some(RECONNECT_LIMIT)).ok()?;
    stream.set_write_timeout(Some(WRITE_LIMIT)).ok()?;
    let hello = Message::Hello {
        v: PROTOCOL,
        key: key.to_owned(),
    };
    write_frame(&mut stream, &hello.into_value()).ok()?;
    let Message::Welcome { tools } = Message::parse(read_frame(&mut stream).ok()??).ok()? else {
        return None;
    };
    stream.set_read_timeout(None).ok()?;
    let writer = stream.try_clone().ok()?;
    Some((stream, writer, tools))
}

/// A call that has a frame and is waiting for a connection.
struct Queued {
    number: u64,
    id: Value,
    frame: Vec<u8>,
}

struct Bridge<W> {
    out: W,
    /// The writing end of the live connection.
    link: Option<UnixStream>,
    /// When the bridge stops waiting for a host; `None` while one is connected.
    no_host_after: Option<Instant>,
    /// The latest welcome's tools. They outlast the connection: a window's tools do not change with its socket.
    tools: Option<Vec<Value>>,
    /// `tools/list` requests made before any welcome.
    lists: Vec<Value>,
    queued: VecDeque<Queued>,
    /// Calls sent and not answered: the frame's number to the client's id.
    open: BTreeMap<u64, Value>,
    next_number: u64,
}

impl<W: Write> Bridge<W> {
    fn new(out: W) -> Bridge<W> {
        Bridge {
            out,
            link: None,
            no_host_after: Some(Instant::now() + RECONNECT_LIMIT),
            tools: None,
            lists: Vec::new(),
            queued: VecDeque::new(),
            open: BTreeMap::new(),
            next_number: 1,
        }
    }

    /// Runs until stdin ends (0) or no host came in time (1).
    fn run(&mut self, events: Receiver<Event>) -> u8 {
        loop {
            let event = match self.no_host_after {
                None => events.recv().map_err(|_| RecvTimeoutError::Disconnected),
                // `recv_timeout` hands over what is queued before it looks at the time.
                Some(after) if Instant::now() >= after => Err(RecvTimeoutError::Timeout),
                Some(after) => events.recv_timeout(after.saturating_duration_since(Instant::now())),
            };
            match event {
                Ok(Event::Line(line)) => self.line(line),
                Ok(Event::Up { writer, tools }) => self.connected(writer, tools),
                Ok(Event::Host(message)) => self.host_said(message),
                Ok(Event::Down) => self.dropped(),
                Ok(Event::StdinEnded) | Err(RecvTimeoutError::Disconnected) => return 0,
                Err(RecvTimeoutError::Timeout) => {
                    self.give_up();
                    return 1;
                }
            }
        }
    }

    fn connected(&mut self, writer: UnixStream, tools: Vec<Value>) {
        self.link = Some(writer);
        self.no_host_after = None;
        self.set_tools(tools);
        self.send_queued();
    }

    fn set_tools(&mut self, tools: Vec<Value>) {
        self.tools = Some(tools);
        for id in std::mem::take(&mut self.lists) {
            self.answer_list(&id);
        }
    }

    fn dropped(&mut self) {
        self.link = None;
        self.no_host_after = Some(Instant::now() + RECONNECT_LIMIT);
        for (_, id) in std::mem::take(&mut self.open) {
            self.answer(&id, MOVED, true);
        }
    }

    fn give_up(&mut self) {
        for call in std::mem::take(&mut self.queued) {
            self.answer(&call.id, NO_HOST, true);
        }
        for id in std::mem::take(&mut self.lists) {
            self.refuse(&id, INTERNAL_ERROR, NO_HOST);
        }
    }

    fn host_said(&mut self, message: Message) {
        match message {
            Message::Result { id, text, is_error } => {
                if let Some(request) = self.open.remove(&id) {
                    self.answer(&request, &text, is_error);
                }
            }
            Message::Welcome { tools } => self.set_tools(tools),
            Message::Hello { .. } | Message::Call { .. } | Message::Cancel { .. } => {}
        }
    }

    fn line(&mut self, line: Line) {
        let text = match line {
            Line::Text(text) => text,
            Line::TooLong => return self.refuse(&Value::Null, PARSE_ERROR, "the line is too long"),
        };
        if text.trim_ascii().is_empty() {
            return;
        }
        let Ok(message) = serde_json::from_slice::<Value>(&text) else {
            return self.refuse(&Value::Null, PARSE_ERROR, "not JSON");
        };
        if !message.is_object() {
            return self.refuse(&Value::Null, INVALID_REQUEST, "not a request");
        }
        // A response has no method. This server asks the client nothing, so there is nothing it could answer.
        let Some(method) = message["method"].as_str() else {
            return;
        };
        match &message["id"] {
            Value::Null => self.notification(method, &message["params"]),
            id => self.request(method, id, &message["params"]),
        }
    }

    fn notification(&mut self, method: &str, params: &Value) {
        if method == "notifications/cancelled" {
            self.cancelled(&params["requestId"]);
        }
    }

    fn request(&mut self, method: &str, id: &Value, params: &Value) {
        match method {
            "initialize" => match params["protocolVersion"].as_str() {
                Some(version) => self.reply(
                    id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "eitri", "version": env!("CARGO_PKG_VERSION")},
                    }),
                ),
                None => self.refuse(id, INVALID_PARAMS, "no protocolVersion"),
            },
            "ping" => self.reply(id, json!({})),
            "tools/list" => match self.tools {
                Some(_) => self.answer_list(id),
                None => self.lists.push(id.clone()),
            },
            "tools/call" => self.call(id, params),
            _ => self.refuse(id, METHOD_NOT_FOUND, "method not found"),
        }
    }

    fn call(&mut self, id: &Value, params: &Value) {
        let Some(name) = params["name"].as_str() else {
            return self.refuse(id, INVALID_PARAMS, "no tool name");
        };
        let arguments = match &params["arguments"] {
            Value::Null => json!({}),
            arguments @ Value::Object(_) => arguments.clone(),
            _ => return self.refuse(id, INVALID_PARAMS, "arguments is not an object"),
        };
        let number = self.next_number;
        self.next_number += 1;
        let call = Message::Call {
            id: number,
            name: name.to_owned(),
            arguments,
            meta: params["_meta"].clone(),
        };
        let Ok(frame) = encode_frame(&call.into_value()) else {
            return self.answer(id, TOO_LONG, true);
        };
        self.queued.push_back(Queued {
            number,
            id: id.clone(),
            frame,
        });
        self.send_queued();
    }

    fn send_queued(&mut self) {
        while self.link.is_some() {
            let Some(call) = self.queued.pop_front() else {
                return;
            };
            self.open.insert(call.number, call.id);
            self.write_link(&call.frame);
        }
    }

    /// A client's cancel for a call the host has is passed on, and the call stays open: a result that comes is written
    /// all the same. One still waiting for a connection goes, since nothing of it has run.
    fn cancelled(&mut self, request: &Value) {
        if let Some(number) = self
            .open
            .iter()
            .find_map(|(number, id)| (id == request).then_some(*number))
        {
            if let Ok(frame) = encode_frame(&Message::Cancel { id: number }.into_value()) {
                self.write_link(&frame);
            }
            return;
        }
        self.queued.retain(|call| call.id != *request);
    }

    /// A write that fails or times out may have left part of a frame, so the connection cannot go on: shutting it
    /// down ends the reader's thread too, whose `Down` answers what was open.
    fn write_link(&mut self, frame: &[u8]) {
        let Some(link) = self.link.as_mut() else {
            return;
        };
        if link.write_all(frame).is_err() {
            let _ = link.shutdown(Shutdown::Both);
            self.link = None;
        }
    }

    fn answer_list(&mut self, id: &Value) {
        let tools = self.tools.as_deref().unwrap_or_default();
        let result = json!({ "tools": tools });
        self.reply(id, result);
    }

    fn answer(&mut self, id: &Value, text: &str, is_error: bool) {
        self.reply(
            id,
            json!({"content": [{"type": "text", "text": text}], "isError": is_error}),
        );
    }

    fn reply(&mut self, id: &Value, result: Value) {
        self.write(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    fn refuse(&mut self, id: &Value, code: i64, message: &str) {
        self.write(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}));
    }

    /// A client that stopped reading has left, as one that closed stdin has: there is nobody to serve.
    fn write(&mut self, message: &Value) {
        let mut line = message.to_string().into_bytes();
        line.push(b'\n');
        if self.out.write_all(&line).and_then(|()| self.out.flush()).is_err() {
            std::process::exit(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The limit is looked at before the queue: a client that never stops writing keeps it non-empty for ever.
    #[test]
    fn the_limit_holds_with_events_still_queued() {
        let (events, incoming) = sync_channel(EVENTS_WAITING);
        let ping = br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#.to_vec();
        events.send(Event::Line(Line::Text(ping))).expect("the queue has room");
        let mut out = Vec::new();
        let mut bridge = Bridge::new(&mut out);
        bridge.no_host_after = Some(Instant::now());
        assert_eq!(bridge.run(incoming), 1);
        assert!(out.is_empty(), "served a line past the limit: {out:?}");
    }
}
