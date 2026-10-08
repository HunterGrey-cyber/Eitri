//! One child agent process and its newline-delimited JSON-RPC conversation over stdio.
//!
//! The link owns the child and three threads: a reader (stdout), a writer (stdin) and a stderr
//! drain. No caller ever blocks on the child: requests and answers go through a bounded queue to
//! the writer thread, and incoming notifications and server requests are drained with
//! [`JsonRpcLink::try_recv`]. Only [`Pending::wait`] blocks, and only until its own deadline.
//!
//! A line longer than [`MAX_LINE`] is not buffered. The reader keeps its first
//! [`PREFIX_LEN`] bytes, discards the rest up to the newline, and classifies the line from the keys
//! at the top level of that prefix, because the agent's ids and ours are separate number spaces and
//! the class decides which one an `"id"` belongs to. Memory stays bounded and the link stays up for
//! every class but one that cannot be told.

use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

/// The longest line the reader keeps whole. Newline framing lets it skip a longer one without
/// losing the stream; the figure is the one `nvim_rpc` caps a frame at.
pub const MAX_LINE: usize = 16 << 20;
/// How much of an over-long line is kept to classify it.
pub const PREFIX_LEN: usize = 256;
/// Requests and answers waiting for the writer thread; a full queue fails the call, never blocks it.
const WRITE_QUEUE: usize = 1024;
/// Messages from the agent waiting for the owner to drain them. The reader waits when it is full.
const INCOMING_QUEUE: usize = 4096;
/// The stderr tail kept for notices and logs.
const STDERR_TAIL: usize = 8 * 1024;

/// How a message is framed: ACP carries the `"jsonrpc":"2.0"` member, the Codex app-server omits it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    WithJsonrpcMember,
    Bare,
}

#[derive(Clone, Debug)]
pub struct LinkSpec {
    /// Absolute: a relative one would resolve against the untrusted project root the child runs in.
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub framing: Framing,
    /// Variables set on the child, over what it inherits.
    pub env: Vec<(OsString, OsString)>,
    /// Variables taken away from what the child inherits.
    pub env_remove: Vec<OsString>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkError {
    /// The writer queue is full.
    Busy,
    /// The link ended before an answer.
    Closed,
    /// The deadline passed.
    Timeout,
    /// The answer was larger than [`MAX_LINE`] and was discarded.
    TooLarge,
    /// The agent answered with an error.
    Rpc { code: i64, message: String },
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkError::Busy => write!(f, "the agent is not reading its input"),
            LinkError::Closed => write!(f, "the agent's connection closed"),
            LinkError::Timeout => write!(f, "the agent did not answer in time"),
            LinkError::TooLarge => write!(f, "the agent's answer was larger than 16 MiB"),
            LinkError::Rpc { code, message } => write!(f, "the agent answered with an error ({code}): {message}"),
        }
    }
}

impl std::error::Error for LinkError {}

/// What an over-long line was, as far as its prefix says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Skipped {
    /// A server request that was answered inside the link (a decline, or an error).
    RequestAnswered { method: String },
    /// A response; only the request with this id failed.
    ResponseFailed { id: u64 },
    /// A notification about one item that is too large to show.
    ItemTooLarge {
        method: String,
        item_id: Option<String>,
        thread_id: Option<String>,
    },
    /// A turn notification that is too large: the turn cannot be followed.
    TurnTooLarge { method: String, thread_id: Option<String> },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Incoming {
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Skipped(Skipped),
    /// Always the last message.
    Closed {
        reason: String,
    },
}

/// The program to run for `name`: an absolute name as it is, otherwise the first regular file found
/// in the absolute `PATH` entries. Empty and relative entries are skipped, as `eitri split` does, so
/// the project root can never supply the program.
pub fn resolve_program(name: &OsStr, path_var: Option<&OsStr>, is_file: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let path = Path::new(name);
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }
    if name.is_empty() || path.components().count() != 1 {
        return None;
    }
    std::env::split_paths(path_var?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| is_file(candidate))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Where one answer lands.
struct Slot {
    value: Mutex<Option<Result<Value, LinkError>>>,
    ready: Condvar,
}

impl Slot {
    fn new() -> Arc<Slot> {
        Arc::new(Slot {
            value: Mutex::new(None),
            ready: Condvar::new(),
        })
    }

    fn fill(&self, result: Result<Value, LinkError>) {
        let mut value = lock(&self.value);
        if value.is_none() {
            *value = Some(result);
        }
        self.ready.notify_all();
    }
}

#[derive(Default)]
struct PendingMap {
    closed: bool,
    slots: HashMap<u64, Arc<Slot>>,
}

struct Shared {
    framing: Framing,
    next_id: AtomicU64,
    pending: Mutex<PendingMap>,
    /// `None` once the link is shut down; the writer thread ends and the child sees EOF.
    writer: Mutex<Option<SyncSender<Vec<u8>>>>,
    stderr: Mutex<VecDeque<u8>>,
}

impl Shared {
    fn send_line(&self, line: Vec<u8>) -> Result<(), LinkError> {
        match lock(&self.writer).as_ref() {
            None => Err(LinkError::Closed),
            Some(tx) => tx.try_send(line).map_err(|e| match e {
                TrySendError::Full(_) => LinkError::Busy,
                TrySendError::Disconnected(_) => LinkError::Closed,
            }),
        }
    }

    fn fail_all(&self) {
        let slots: Vec<Arc<Slot>> = {
            let mut pending = lock(&self.pending);
            pending.closed = true;
            pending.slots.drain().map(|(_, slot)| slot).collect()
        };
        for slot in slots {
            slot.fill(Err(LinkError::Closed));
        }
    }

    fn take_slot(&self, id: u64) -> Option<Arc<Slot>> {
        lock(&self.pending).slots.remove(&id)
    }

    /// One message as a line: our own members in a fixed order, so the bytes are predictable.
    fn line(&self, members: &[(&str, String)]) -> Vec<u8> {
        let mut line = String::from("{");
        if self.framing == Framing::WithJsonrpcMember {
            line.push_str("\"jsonrpc\":\"2.0\",");
        }
        for (i, (key, json)) in members.iter().enumerate() {
            if i > 0 {
                line.push(',');
            }
            line.push('"');
            line.push_str(key);
            line.push_str("\":");
            line.push_str(json);
        }
        line.push_str("}\n");
        line.into_bytes()
    }

    fn respond(&self, id: &Value, result: &Value) -> Result<(), LinkError> {
        self.send_line(self.line(&[("id", json_text(id)), ("result", json_text(result))]))
    }

    fn respond_error(&self, id: &Value, code: i64, message: &str) -> Result<(), LinkError> {
        let error = serde_json::json!({ "code": code, "message": message });
        self.send_line(self.line(&[("id", json_text(id)), ("error", json_text(&error))]))
    }
}

fn json_text(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

/// An answer on its way. Dropping it forgets the request: a late answer is discarded.
pub struct Pending {
    id: u64,
    slot: Arc<Slot>,
    shared: Arc<Shared>,
}

impl Pending {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Waits for the answer until `deadline`.
    pub fn wait(self, deadline: Instant) -> Result<Value, LinkError> {
        let mut value = lock(&self.slot.value);
        loop {
            if let Some(result) = value.take() {
                return result;
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(LinkError::Timeout);
            }
            value = match self.slot.ready.wait_timeout(value, deadline - now) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    /// The answer if it has arrived.
    pub fn try_take(&mut self) -> Option<Result<Value, LinkError>> {
        lock(&self.slot.value).take()
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        lock(&self.shared.pending).slots.remove(&self.id);
    }
}

pub struct JsonRpcLink {
    shared: Arc<Shared>,
    incoming: Receiver<Incoming>,
    child: Option<Child>,
    pid: u32,
}

impl JsonRpcLink {
    /// Starts the child with piped stdio in `spec.cwd`. The child is not put in a process group of
    /// its own, and its environment is the caller's except for what `spec.env` and `spec.env_remove` say.
    pub fn spawn(spec: LinkSpec) -> io::Result<JsonRpcLink> {
        if !spec.program.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the agent's program must be an absolute path",
            ));
        }
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in &spec.env_remove {
            command.env_remove(name);
        }
        for (name, value) in &spec.env {
            command.env(name, value);
        }
        let mut child = command.spawn()?;
        let pid = child.id();
        let (Some(stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other("the agent's pipes were not created"));
        };

        let (write_tx, write_rx) = sync_channel::<Vec<u8>>(WRITE_QUEUE);
        let (in_tx, in_rx) = sync_channel::<Incoming>(INCOMING_QUEUE);
        let shared = Arc::new(Shared {
            framing: spec.framing,
            next_id: AtomicU64::new(1),
            pending: Mutex::new(PendingMap::default()),
            writer: Mutex::new(Some(write_tx)),
            stderr: Mutex::new(VecDeque::new()),
        });

        thread::Builder::new()
            .name("eitri-agent-write".into())
            .spawn(move || write_loop(stdin, write_rx))?;
        let reader_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("eitri-agent-read".into())
            .spawn(move || read_loop(stdout, reader_shared, in_tx))?;
        let stderr_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("eitri-agent-stderr".into())
            .spawn(move || drain_stderr(stderr, stderr_shared))?;

        Ok(JsonRpcLink {
            shared,
            incoming: in_rx,
            child: Some(child),
            pid,
        })
    }

    /// The child's pid, captured at spawn.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Sends a request and returns at once. A failure to queue it is the answer.
    pub fn request(&self, method: &str, params: Value) -> Pending {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let slot = Slot::new();
        let pending = Pending {
            id,
            slot: Arc::clone(&slot),
            shared: Arc::clone(&self.shared),
        };
        {
            let mut map = lock(&self.shared.pending);
            if map.closed {
                slot.fill(Err(LinkError::Closed));
                return pending;
            }
            map.slots.insert(id, Arc::clone(&slot));
        }
        let mut members = vec![("id", id.to_string()), ("method", json_text(&Value::from(method)))];
        if !params.is_null() {
            members.push(("params", json_text(&params)));
        }
        if let Err(e) = self.shared.send_line(self.shared.line(&members)) {
            lock(&self.shared.pending).slots.remove(&id);
            slot.fill(Err(e));
        }
        pending
    }

    pub fn notify(&self, method: &str, params: Value) {
        let mut members = vec![("method", json_text(&Value::from(method)))];
        if !params.is_null() {
            members.push(("params", json_text(&params)));
        }
        if let Err(e) = self.shared.send_line(self.shared.line(&members)) {
            eprintln!("a notification to the agent was not sent: {e}");
        }
    }

    /// Answers a request the agent made, by its own id.
    pub fn respond(&self, id: Value, result: Value) {
        if let Err(e) = self.shared.respond(&id, &result) {
            eprintln!("an answer to the agent was not sent: {e}");
        }
    }

    pub fn respond_error(&self, id: Value, code: i64, message: &str) {
        if let Err(e) = self.shared.respond_error(&id, code, message) {
            eprintln!("an error answer to the agent was not sent: {e}");
        }
    }

    /// The next message from the agent, if one is waiting.
    pub fn try_recv(&self) -> Option<Incoming> {
        self.incoming.try_recv().ok()
    }

    /// The end of what the child wrote to stderr, bounded, with email addresses replaced.
    pub fn stderr_tail(&self) -> String {
        let bytes: Vec<u8> = lock(&self.shared.stderr).iter().copied().collect();
        redact_emails(&String::from_utf8_lossy(&bytes))
    }

    /// Closes the child's stdin and waits up to `grace` for it to exit; then SIGKILL to the captured pid.
    pub fn shutdown(mut self, grace: Duration) {
        self.finish(grace);
    }

    fn finish(&mut self, grace: Duration) {
        // Dropping the sender ends the writer thread, which drops stdin: the child sees EOF.
        lock(&self.shared.writer).take();
        let Some(mut child) = self.child.take() else { return };
        let end = Instant::now() + grace;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < end => thread::sleep(Duration::from_millis(10)),
                _ => break,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for JsonRpcLink {
    fn drop(&mut self) {
        self.finish(Duration::ZERO);
    }
}

fn write_loop(mut stdin: ChildStdin, queue: Receiver<Vec<u8>>) {
    for line in queue {
        if stdin.write_all(&line).and_then(|_| stdin.flush()).is_err() {
            return;
        }
    }
}

fn drain_stderr(mut stderr: impl Read, shared: Arc<Shared>) {
    let mut buf = [0u8; 4096];
    loop {
        match stderr.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                let mut tail = lock(&shared.stderr);
                tail.extend(&buf[..n]);
                let excess = tail.len().saturating_sub(STDERR_TAIL);
                tail.drain(..excess);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

fn read_loop(stdout: ChildStdout, shared: Arc<Shared>, tx: SyncSender<Incoming>) {
    let mut out = BufReader::with_capacity(64 * 1024, stdout);
    let mut line: Vec<u8> = Vec::new();
    // Set once the line in progress passed the cap: only its first bytes are kept.
    let mut over_cap: Option<Vec<u8>> = None;
    let reason = loop {
        let buf = match out.fill_buf() {
            Ok(buf) => buf,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => break format!("reading from the agent failed: {e}"),
        };
        if buf.is_empty() {
            break "the agent closed its output".to_string();
        }
        let newline = buf.iter().position(|b| *b == b'\n');
        let chunk = &buf[..newline.unwrap_or(buf.len())];
        let used = chunk.len() + usize::from(newline.is_some());
        match over_cap.as_mut() {
            Some(_) => {}
            None if line.len() + chunk.len() <= MAX_LINE => line.extend_from_slice(chunk),
            None => {
                let mut prefix = std::mem::take(&mut line);
                let want = PREFIX_LEN.saturating_sub(prefix.len()).min(chunk.len());
                prefix.extend_from_slice(&chunk[..want]);
                prefix.truncate(PREFIX_LEN);
                prefix.shrink_to_fit();
                over_cap = Some(prefix);
            }
        }
        out.consume(used);
        if newline.is_none() {
            continue;
        }
        let verdict = match over_cap.take() {
            Some(prefix) => handle_over_cap(&prefix, &shared, &tx),
            None => {
                let whole = std::mem::take(&mut line);
                handle_line(&whole, &shared, &tx)
            }
        };
        line.clear();
        if let Err(reason) = verdict {
            break reason;
        }
    };
    shared.fail_all();
    let _ = tx.send(Incoming::Closed { reason });
}

fn push(tx: &SyncSender<Incoming>, item: Incoming) -> Result<(), String> {
    tx.send(item).map_err(|_| "the link was dropped".to_string())
}

/// A bounded, escaped excerpt for a log line.
fn excerpt(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(160)]);
    format!("{:?}", text.as_ref())
}

fn handle_line(bytes: &[u8], shared: &Shared, tx: &SyncSender<Incoming>) -> Result<(), String> {
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    let Ok(Value::Object(mut map)) = serde_json::from_slice::<Value>(bytes) else {
        eprintln!(
            "a line from the agent was not a JSON object and was dropped: {}",
            excerpt(bytes)
        );
        return Ok(());
    };
    let method = match map.get("method") {
        None => None,
        Some(Value::String(m)) => Some(m.clone()),
        Some(_) => {
            eprintln!("a message from the agent had a method that is not a string and was dropped");
            return Ok(());
        }
    };
    let id = map.remove("id").filter(|id| !id.is_null());
    match (method, id) {
        (Some(method), Some(id)) => {
            let params = map.remove("params").unwrap_or(Value::Null);
            push(tx, Incoming::Request { id, method, params })
        }
        (Some(method), None) => {
            let params = map.remove("params").unwrap_or(Value::Null);
            push(tx, Incoming::Notification { method, params })
        }
        (None, Some(id)) => {
            deliver_response(shared, &id, &mut map);
            Ok(())
        }
        (None, None) => {
            eprintln!("a message from the agent had neither a method nor an id and was dropped");
            Ok(())
        }
    }
}

fn deliver_response(shared: &Shared, id: &Value, map: &mut serde_json::Map<String, Value>) {
    let Some(slot) = id.as_u64().and_then(|id| shared.take_slot(id)) else {
        return;
    };
    let result = match map.remove("error") {
        Some(error) => Err(LinkError::Rpc {
            code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: error.get("message").and_then(Value::as_str).unwrap_or("").to_string(),
        }),
        None => Ok(map.remove("result").unwrap_or(Value::Null)),
    };
    slot.fill(result);
}

fn handle_over_cap(prefix: &[u8], shared: &Shared, tx: &SyncSender<Incoming>) -> Result<(), String> {
    let found = scan_prefix(prefix);
    eprintln!("a message from the agent was larger than 16 MiB");
    match (found.has_method, found.has_id) {
        (true, true) => {
            let (Some(id), method) = (found.id, found.method.unwrap_or_default()) else {
                return Err("a request from the agent was too large and its id could not be read".to_string());
            };
            // The agent treats a failed approval as a denial, and an unanswered request hangs it.
            let answered = if method == "item/permissions/requestApproval" {
                let grant = serde_json::json!({ "permissions": {}, "scope": "turn" });
                shared.respond(&id, &grant)
            } else if method.ends_with("requestApproval") {
                shared.respond(&id, &serde_json::json!({ "decision": "decline" }))
            } else {
                shared.respond_error(&id, -32600, "too large")
            };
            if let Err(e) = answered {
                eprintln!("the answer to an over-sized request was not sent: {e}");
            }
            push(tx, Incoming::Skipped(Skipped::RequestAnswered { method }))
        }
        (false, true) => match found.id.as_ref().and_then(Value::as_u64) {
            Some(id) => {
                if let Some(slot) = shared.take_slot(id) {
                    slot.fill(Err(LinkError::TooLarge));
                }
                push(tx, Incoming::Skipped(Skipped::ResponseFailed { id }))
            }
            None => {
                eprintln!("an over-sized response with an unreadable id was dropped");
                Ok(())
            }
        },
        (true, false) => {
            let method = found.method.unwrap_or_default();
            let thread_id = found.thread_id;
            if method == "turn/started" || method == "turn/completed" {
                push(tx, Incoming::Skipped(Skipped::TurnTooLarge { method, thread_id }))
            } else if method.starts_with("item/") {
                push(
                    tx,
                    Incoming::Skipped(Skipped::ItemTooLarge {
                        method,
                        item_id: found.item_id,
                        thread_id,
                    }),
                )
            } else {
                eprintln!("an over-sized notification was dropped");
                Ok(())
            }
        }
        (false, false) => Err("a message from the agent was too large and could not be classified".to_string()),
    }
}

/// What the first bytes of a line say about it.
#[derive(Default, Debug)]
struct Prefix {
    has_method: bool,
    method: Option<String>,
    has_id: bool,
    id: Option<Value>,
    thread_id: Option<String>,
    item_id: Option<String>,
}

/// Reads as much of a truncated JSON object as the bytes hold, recording the keys the link cares
/// about. It never fails: where the bytes stop making sense it stops, and keeps what it found.
fn scan_prefix(bytes: &[u8]) -> Prefix {
    let mut scan = Scan {
        b: bytes,
        i: 0,
        found: Prefix::default(),
    };
    scan.ws();
    if scan.b.get(scan.i) == Some(&b'{') {
        let _ = scan.object(&mut Vec::new());
    }
    scan.found
}

struct Scan<'a> {
    b: &'a [u8],
    i: usize,
    found: Prefix,
}

/// The bytes ended or stopped being JSON.
struct Cut;

impl Scan<'_> {
    fn ws(&mut self) {
        while self.b.get(self.i).is_some_and(u8::is_ascii_whitespace) {
            self.i += 1;
        }
    }

    fn next(&mut self) -> Result<u8, Cut> {
        let c = *self.b.get(self.i).ok_or(Cut)?;
        self.i += 1;
        Ok(c)
    }

    /// A string whose opening quote is next.
    fn string(&mut self) -> Result<String, Cut> {
        self.i += 1;
        let mut out = Vec::new();
        loop {
            match self.next()? {
                b'"' => return Ok(String::from_utf8_lossy(&out).into_owned()),
                b'\\' => match self.next()? {
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'r' => out.push(b'\r'),
                    b'u' => {
                        for _ in 0..4 {
                            self.next()?;
                        }
                        out.push(b'?');
                    }
                    other => out.push(other),
                },
                c => out.push(c),
            }
        }
    }

    fn object(&mut self, path: &mut Vec<String>) -> Result<(), Cut> {
        self.i += 1;
        loop {
            self.ws();
            match *self.b.get(self.i).ok_or(Cut)? {
                b'}' => {
                    self.i += 1;
                    return Ok(());
                }
                b',' => self.i += 1,
                b'"' => {
                    let key = self.string()?;
                    self.ws();
                    if self.next()? != b':' {
                        return Err(Cut);
                    }
                    if path.is_empty() {
                        match key.as_str() {
                            "method" => self.found.has_method = true,
                            "id" => self.found.has_id = true,
                            _ => {}
                        }
                    }
                    path.push(key);
                    let done = self.value(path);
                    path.pop();
                    done?;
                }
                _ => return Err(Cut),
            }
        }
    }

    fn array(&mut self, path: &mut Vec<String>) -> Result<(), Cut> {
        self.i += 1;
        loop {
            self.ws();
            match *self.b.get(self.i).ok_or(Cut)? {
                b']' => {
                    self.i += 1;
                    return Ok(());
                }
                b',' => self.i += 1,
                _ => {
                    path.push(String::new());
                    let done = self.value(path);
                    path.pop();
                    done?;
                }
            }
        }
    }

    fn value(&mut self, path: &mut Vec<String>) -> Result<(), Cut> {
        self.ws();
        match *self.b.get(self.i).ok_or(Cut)? {
            b'{' => self.object(path),
            b'[' => self.array(path),
            b'"' => {
                let text = self.string()?;
                self.record_string(path, text);
                Ok(())
            }
            _ => {
                let start = self.i;
                while self
                    .b
                    .get(self.i)
                    .is_some_and(|c| !matches!(c, b',' | b'}' | b']') && !c.is_ascii_whitespace())
                {
                    self.i += 1;
                }
                // A token that runs to the end of the bytes may have been cut in the middle.
                if self.i >= self.b.len() {
                    return Err(Cut);
                }
                if path.len() == 1 && path[0] == "id" {
                    self.found.id = serde_json::from_slice(&self.b[start..self.i]).ok();
                }
                Ok(())
            }
        }
    }

    fn record_string(&mut self, path: &[String], text: String) {
        let path: Vec<&str> = path.iter().map(String::as_str).collect();
        match path.as_slice() {
            ["method"] => self.found.method = Some(text),
            ["id"] => self.found.id = Some(Value::String(text)),
            ["params", "threadId"] => self.found.thread_id = Some(text),
            ["params", "itemId"] | ["params", "item", "id"] => self.found.item_id = Some(text),
            _ => {}
        }
    }
}

/// Replaces anything shaped like an email address with `<email>`.
pub fn redact_emails(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let local = |c: char| c.is_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-');
    let domain = |c: char| c.is_alphanumeric() || matches!(c, '.' | '-');
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if !local(chars[i]) {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        // A run of local-part characters, then possibly '@' and a domain.
        let start = i;
        while i < chars.len() && local(chars[i]) {
            i += 1;
        }
        if chars.get(i) == Some(&'@') {
            let mut end = i + 1;
            while end < chars.len() && domain(chars[end]) {
                end += 1;
            }
            while end > i + 1 && matches!(chars[end - 1], '.' | '-') {
                end -= 1;
            }
            if chars[i + 1..end].contains(&'.') {
                out.push_str("<email>");
                i = end;
                continue;
            }
        }
        out.extend(&chars[start..i]);
    }
    out
}
