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
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
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
/// How much the reader asks the agent's stdout for at a time.
const READ_CHUNK: usize = 64 * 1024;
/// How long a reader facing a full incoming queue waits on its stop signal before it tries again.
const QUEUE_RETRY: Duration = Duration::from_millis(10);
/// The longest a stop for a move waits for the agent to take what was written to it, and for an answer
/// a blocked caller is waiting on. A JSON-RPC answer has no acknowledgement, so an empty writer queue
/// whose last write returned is the most the link can know.
const MOVE_WAIT: Duration = Duration::from_secs(2);

/// Writes one log line and ignores a failure to write it. `eprintln!` panics when stderr is a pipe
/// whose reader has gone, and these lines are written from the link's own threads.
fn write_log(out: &mut impl Write, args: fmt::Arguments<'_>) {
    let _ = writeln!(out, "{args}");
}

macro_rules! link_log {
    ($($arg:tt)*) => {
        write_log(&mut io::stderr(), format_args!($($arg)*))
    };
}

/// How a message is framed: ACP carries the `"jsonrpc":"2.0"` member, the Codex app-server omits it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Framing {
    WithJsonrpcMember,
    Bare,
}

/// The JSON-RPC error a request too large to read is refused with, where its protocol has no refusal of
/// its own: "invalid request".
pub const REQUEST_TOO_LARGE: i64 = -32600;

/// What the link writes back, at once, to a request from the agent larger than [`MAX_LINE`]. Nothing past
/// the request's method could be read, so the answer must refuse whatever was asked.
#[derive(Clone, Debug, PartialEq)]
pub enum OverCapReply {
    Result(Value),
    Error { code: i64, message: &'static str },
}

/// Picks the [`OverCapReply`] for a request by its method, in the provider's own protocol. The link
/// answers inside its reader rather than leave it to the owner, because an unanswered request hangs the
/// agent and an owner may not be reading.
pub type OverCapAnswer = fn(method: &str) -> OverCapReply;

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
    /// How a request too large to read is answered.
    pub over_cap: OverCapAnswer,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Skipped {
    /// A server request that was answered inside the link with what [`LinkSpec::over_cap`] picked. The
    /// ids are the ones its first bytes named, for the owner to say on the call's row what happened.
    RequestAnswered {
        method: String,
        /// `params.threadId` (Codex).
        thread_id: Option<String>,
        /// `params.sessionId` (ACP).
        session_id: Option<String>,
        /// `params.itemId` (Codex) or `params.toolCall.toolCallId` (ACP).
        item_id: Option<String>,
    },
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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

/// Everything a stopped link held that the agent has not seen answered or the owner has not been
/// given, as data another process can take the conversation up from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinkHandoff {
    /// Read from the agent's stdout and not looked at yet.
    pub unread: Vec<u8>,
    /// The line in progress: its bytes so far, or the kept prefix of a line past [`MAX_LINE`].
    pub partial_line: Vec<u8>,
    /// Whether `partial_line` is the prefix of a line past [`MAX_LINE`].
    pub over_cap: bool,
    /// The messages the owner had not drained, oldest first; one the reader held because the queue
    /// was full comes last, since it was read after all of them.
    pub incoming: Vec<Value>,
    /// Requests of ours the agent has not answered yet.
    pub pending: Vec<PendingRecord>,
    /// Answers that came for requests this link had itself taken over and whose [`Pending`] was not
    /// rebound yet. The next link keeps them under their ids, for [`JsonRpcLink::rebind`].
    pub answered: Vec<AnsweredRecord>,
    pub next_id: u64,
    /// The stderr tail up to its last newline, and what follows it.
    pub stderr_tail: Vec<u8>,
    pub stderr_partial: Vec<u8>,
    pub pid: u32,
    /// The child's start time, in clock ticks since boot: with `pid` it names the process for good.
    pub start: u64,
    pub framing: Framing,
}

/// One of our requests still waiting for the agent's answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingRecord {
    pub id: u64,
    pub method: String,
    pub kind: PendingKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PendingKind {
    /// Its owner takes the answer when it comes ([`Pending::try_take`]), so the request can move.
    Polled,
    /// A caller is blocked in [`Pending::wait`] for it. Only that caller can use the answer, so a stop
    /// waits for it and is refused if it does not come.
    Waited,
}

/// An answer that had already arrived, as data that can move.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnsweredRecord {
    pub id: u64,
    pub result: Result<Value, RpcErrorRecord>,
}

/// How a request failed, as data: the agent's own error, or the link's (an answer too large to read,
/// a request that never left).
pub type RpcErrorRecord = LinkError;

/// A stop for a move that did not happen, and why. The link is handed back running, as it was; boxed
/// so the refusal stays small beside the stop's success.
pub struct StopRefused {
    pub link: Box<JsonRpcLink>,
    pub reason: String,
}

impl fmt::Debug for StopRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StopRefused").field("reason", &self.reason).finish()
    }
}

impl fmt::Display for StopRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for StopRefused {}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Where one answer lands.
struct Slot {
    value: Mutex<Option<Result<Value, LinkError>>>,
    ready: Condvar,
    /// Set once a caller blocks in [`Pending::wait`] on it.
    waited: AtomicBool,
}

impl Slot {
    fn new() -> Arc<Slot> {
        Arc::new(Slot {
            value: Mutex::new(None),
            ready: Condvar::new(),
            waited: AtomicBool::new(false),
        })
    }

    fn fill(&self, result: Result<Value, LinkError>) {
        let mut value = lock(&self.value);
        if value.is_none() {
            *value = Some(result);
        }
        self.ready.notify_all();
    }

    fn is_filled(&self) -> bool {
        lock(&self.value).is_some()
    }
}

struct Entry {
    slot: Arc<Slot>,
    method: String,
    /// False for a request taken over from another process until its [`Pending`] is rebound: an
    /// answer that comes first stays here for [`JsonRpcLink::rebind`] to hand out.
    bound: bool,
}

#[derive(Default)]
struct PendingMap {
    closed: bool,
    slots: HashMap<u64, Entry>,
}

enum WriteItem {
    Line(Vec<u8>),
    /// Answered once every line queued before it has been written.
    Flush(SyncSender<()>),
    /// Ends the writer, which hands back the agent's stdin instead of closing it.
    Release(SyncSender<File>),
}

struct Shared {
    framing: Framing,
    over_cap: OverCapAnswer,
    next_id: AtomicU64,
    pending: Mutex<PendingMap>,
    /// `None` once the link is shut down; the writer thread ends and the child sees EOF.
    writer: Mutex<Option<SyncSender<WriteItem>>>,
    stderr: Mutex<VecDeque<u8>>,
    /// True while the reader holds a message and waits for room in the incoming queue.
    reader_waiting: AtomicBool,
    /// Bytes the reader has taken from the agent's stdout.
    bytes_read: AtomicU64,
    /// The close of a conversation whose queue was full when its reader ended. Waiting for room
    /// would hold the reader up for as long as nobody drains, a stop for a move included, so the
    /// close waits here and is handed out once the queue is empty.
    held_closed: Mutex<Option<Incoming>>,
}

impl Shared {
    fn new(framing: Framing, over_cap: OverCapAnswer, next_id: u64, writer: Option<SyncSender<WriteItem>>) -> Shared {
        Shared {
            framing,
            over_cap,
            next_id: AtomicU64::new(next_id),
            pending: Mutex::new(PendingMap::default()),
            writer: Mutex::new(writer),
            stderr: Mutex::new(VecDeque::new()),
            reader_waiting: AtomicBool::new(false),
            bytes_read: AtomicU64::new(0),
            held_closed: Mutex::new(None),
        }
    }

    /// Tells the owner the conversation ended. Never waits: a full queue keeps the close aside.
    fn queue_closed(&self, tx: &SyncSender<Incoming>, reason: String) {
        if let Err(TrySendError::Full(closed)) = tx.try_send(Incoming::Closed { reason }) {
            *lock(&self.held_closed) = Some(closed);
        }
    }

    fn send_line(&self, line: Vec<u8>) -> Result<(), LinkError> {
        match lock(&self.writer).as_ref() {
            None => Err(LinkError::Closed),
            Some(tx) => tx.try_send(WriteItem::Line(line)).map_err(|e| match e {
                TrySendError::Full(_) => LinkError::Busy,
                TrySendError::Disconnected(_) => LinkError::Closed,
            }),
        }
    }

    /// Fails every open request. A request taken over and not rebound yet keeps its entry, with the
    /// answer that came before the end or else the failure, so that [`JsonRpcLink::rebind`] still
    /// hands it out.
    fn fail_all(&self) {
        let slots: Vec<Arc<Slot>> = {
            let mut pending = lock(&self.pending);
            pending.closed = true;
            let mut slots = Vec::new();
            pending.slots.retain(|_, entry| {
                slots.push(Arc::clone(&entry.slot));
                !entry.bound
            });
            slots
        };
        // A slot that already holds an answer keeps it.
        for slot in slots {
            slot.fill(Err(LinkError::Closed));
        }
    }

    /// Gives request `id` its answer. A request taken over and not rebound yet keeps its entry, so
    /// the answer is still there when its owner asks for it.
    fn resolve(&self, id: u64, result: Result<Value, LinkError>) {
        let slot = {
            let mut map = lock(&self.pending);
            match map.slots.get(&id) {
                None => return,
                Some(entry) if entry.bound => map.slots.remove(&id).map(|entry| entry.slot),
                Some(entry) => Some(Arc::clone(&entry.slot)),
            }
        };
        if let Some(slot) = slot {
            slot.fill(result);
        }
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

    fn push_stderr(&self, bytes: &[u8]) {
        let mut tail = lock(&self.stderr);
        tail.extend(bytes);
        let excess = tail.len().saturating_sub(STDERR_TAIL);
        tail.drain(..excess);
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
        self.slot.waited.store(true, Ordering::SeqCst);
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

    /// The answer if it has arrived, as data that can move to another process. The answer stays:
    /// this `Pending` still reads as answered.
    pub fn answered_record(&mut self) -> Option<AnsweredRecord> {
        lock(&self.slot.value).as_ref().map(|result| AnsweredRecord {
            id: self.id,
            result: result.clone(),
        })
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        let mut map = lock(&self.shared.pending);
        // An answer carried in from another process has no entry of its own, and must not take away
        // one of the same id that belongs to someone else.
        if map
            .slots
            .get(&self.id)
            .is_some_and(|entry| Arc::ptr_eq(&entry.slot, &self.slot))
        {
            map.slots.remove(&self.id);
        }
    }
}

/// Asks one of the link's threads to stop, and wakes it wherever it waits: a flag for the loop's own
/// checks, and a socket whose end of file its `poll` sees.
struct StopSignal {
    raised: AtomicBool,
    wake: UnixStream,
    watch: UnixStream,
}

impl StopSignal {
    fn new() -> io::Result<Arc<StopSignal>> {
        let (wake, watch) = UnixStream::pair()?;
        Ok(Arc::new(StopSignal {
            raised: AtomicBool::new(false),
            wake,
            watch,
        }))
    }

    fn raise(&self) {
        self.raised.store(true, Ordering::SeqCst);
        let _ = self.wake.shutdown(Shutdown::Write);
    }

    fn raised(&self) -> bool {
        self.raised.load(Ordering::SeqCst)
    }

    /// Waits until `fd` has something to read (or hung up): true, or false once the stop is raised,
    /// which wins when both are ready.
    fn wait_readable(&self, fd: RawFd) -> bool {
        let mut fds = [pollin(fd), pollin(self.watch.as_raw_fd())];
        if poll(&mut fds, -1).is_err() {
            // The read that follows reports whatever is wrong with the descriptor.
            return !self.raised();
        }
        fds[1].revents == 0 && !self.raised()
    }

    /// Waits up to `timeout` for the stop; true once it is raised.
    fn wait_for(&self, timeout: Duration) -> bool {
        let mut fds = [pollin(self.watch.as_raw_fd())];
        let ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        let _ = poll(&mut fds, ms);
        self.raised()
    }
}

fn pollin(fd: RawFd) -> libc::pollfd {
    libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }
}

fn poll(fds: &mut [libc::pollfd], timeout_ms: i32) -> io::Result<()> {
    loop {
        // SAFETY: `fds` is a valid, writable array of `fds.len()` `pollfd`s for the call.
        let r = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
        if r >= 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

pub struct JsonRpcLink {
    shared: Arc<Shared>,
    incoming: Receiver<Incoming>,
    /// Kept to start a reader again when a stop is refused after the reader had stopped.
    incoming_tx: SyncSender<Incoming>,
    /// Present when this process started the child.
    child: Option<Child>,
    pid: u32,
    start: u64,
    /// Whether finishing the link ends the child; false once the child was handed to someone else.
    owns_child: bool,
    reader: Option<(JoinHandle<Option<ReaderStopped>>, Arc<StopSignal>)>,
    stderr_drain: Option<(JoinHandle<File>, Arc<StopSignal>)>,
    /// An adopted link's descriptors and reader state until its move is committed.
    parked: Option<Parked>,
}

/// What an adopted link holds of the agent until its move is committed. No thread runs on it, so
/// nothing is read from or written to the agent while the move can still be given back.
struct Parked {
    stdin: File,
    write_rx: Receiver<WriteItem>,
    stdout: File,
    state: ReaderState,
    stderr: File,
}

impl JsonRpcLink {
    /// Starts the child with piped stdio in `spec.cwd`, leading a process group of its own so that
    /// shutting down can end what it started too. Its environment is the caller's except for what `spec.env` and `spec.env_remove` say.
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
            .stderr(Stdio::piped())
            .process_group(0);
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
        let start = proc_stat(pid).map(|stat| stat.start).unwrap_or(0);
        let (write_tx, write_rx) = sync_channel::<WriteItem>(WRITE_QUEUE);
        let shared = Arc::new(Shared::new(spec.framing, spec.over_cap, 1, Some(write_tx)));
        let (in_tx, in_rx) = sync_channel::<Incoming>(INCOMING_QUEUE);
        let mut link = JsonRpcLink::new(shared, (in_tx, in_rx), Some(child), pid, start, true);
        link.run(Parked {
            stdin: File::from(OwnedFd::from(stdin)),
            write_rx,
            stdout: File::from(OwnedFd::from(stdout)),
            state: ReaderState::default(),
            stderr: File::from(OwnedFd::from(stderr)),
        })?;
        Ok(link)
    }

    /// The source's way back when a move is abandoned: the same conversation, read again at once from
    /// what [`JsonRpcLink::stop_for_move`] handed over. `over_cap` is the provider's, as at spawn.
    ///
    /// The `Pending`s the source held before the stop belong to the stopped link and never resolve:
    /// the source takes its requests up again as a destination does, [`JsonRpcLink::rebind`] for each
    /// open id and [`JsonRpcLink::answered`] for each [`Pending::answered_record`].
    pub fn resume_after_move(
        handoff: LinkHandoff,
        fds: [OwnedFd; 3],
        over_cap: OverCapAnswer,
    ) -> io::Result<JsonRpcLink> {
        let (mut link, parked) = JsonRpcLink::from_handoff(handoff, fds, over_cap)?;
        // The source never stopped holding the agent: it is this link's to end, also when a thread
        // cannot be started.
        link.owns_child = true;
        link.run(parked)?;
        Ok(link)
    }

    /// Takes up a conversation another link stopped for a move. The link starts parked: it reads
    /// nothing, writes nothing and ends nothing until [`JsonRpcLink::start_after_commit`], so a move
    /// given back before its commit leaves the agent exactly as the source handed it over, and
    /// dropping the parked link only closes this process's copies of the descriptors.
    ///
    /// The requests the source had sent wait under their ids for [`JsonRpcLink::rebind`]; the owner
    /// rebinds every one it was handed, since an id never rebound keeps its entry until the link ends.
    pub fn adopt(handoff: LinkHandoff, fds: [OwnedFd; 3], over_cap: OverCapAnswer) -> io::Result<JsonRpcLink> {
        let (mut link, parked) = JsonRpcLink::from_handoff(handoff, fds, over_cap)?;
        link.parked = Some(parked);
        Ok(link)
    }

    /// Starts an adopted link once its move is committed: the carried messages come first, then the
    /// unread bytes, then the agent's stdout, and what was queued for the agent meanwhile is written.
    /// From here on the agent is this link's to end, also when a thread cannot be started. Nothing to
    /// do for a link that is already running.
    pub fn start_after_commit(&mut self) -> io::Result<()> {
        let Some(parked) = self.parked.take() else {
            return Ok(());
        };
        self.owns_child = true;
        self.run(parked)
    }

    /// The link a handoff describes, not started, and owning nothing.
    fn from_handoff(
        handoff: LinkHandoff,
        [stdin, stdout, stderr]: [OwnedFd; 3],
        over_cap: OverCapAnswer,
    ) -> io::Result<(JsonRpcLink, Parked)> {
        let LinkHandoff {
            unread,
            partial_line,
            over_cap: line_over_cap,
            incoming,
            pending,
            answered,
            next_id,
            stderr_tail,
            stderr_partial,
            pid,
            start,
            framing,
        } = handoff;
        let incoming = incoming
            .into_iter()
            .map(serde_json::from_value::<Incoming>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("a carried message is unreadable: {e}"),
                )
            })?;
        let (write_tx, write_rx) = sync_channel::<WriteItem>(WRITE_QUEUE);
        let shared = Arc::new(Shared::new(framing, over_cap, next_id, Some(write_tx)));
        {
            let mut map = lock(&shared.pending);
            for record in pending {
                map.slots.insert(
                    record.id,
                    Entry {
                        slot: Slot::new(),
                        method: record.method,
                        bound: false,
                    },
                );
            }
            for record in answered {
                let slot = Slot::new();
                slot.fill(record.result);
                map.slots.insert(
                    record.id,
                    Entry {
                        slot,
                        method: String::new(),
                        bound: false,
                    },
                );
            }
        }
        shared.push_stderr(&stderr_tail);
        shared.push_stderr(&stderr_partial);
        // Room for everything carried: the stopped reader may have held one more than the queue holds.
        let (in_tx, in_rx) = sync_channel::<Incoming>(INCOMING_QUEUE.max(incoming.len()));
        for item in incoming {
            if in_tx.try_send(item).is_err() {
                return Err(io::Error::other("the carried messages did not fit the queue"));
            }
        }
        let parked = Parked {
            stdin: File::from(stdin),
            write_rx,
            stdout: File::from(stdout),
            state: ReaderState {
                unread,
                line: partial_line,
                over_cap: line_over_cap,
                blocked: None,
            },
            stderr: File::from(stderr),
        };
        let link = JsonRpcLink::new(shared, (in_tx, in_rx), None, pid, start, false);
        Ok((link, parked))
    }

    fn new(
        shared: Arc<Shared>,
        (incoming_tx, incoming): (SyncSender<Incoming>, Receiver<Incoming>),
        child: Option<Child>,
        pid: u32,
        start: u64,
        owns_child: bool,
    ) -> JsonRpcLink {
        JsonRpcLink {
            shared,
            incoming,
            incoming_tx,
            child,
            pid,
            start,
            owns_child,
            reader: None,
            stderr_drain: None,
            parked: None,
        }
    }

    /// Starts the writer, the reader and the stderr drain on the agent's descriptors.
    fn run(&mut self, parked: Parked) -> io::Result<()> {
        let Parked {
            stdin,
            write_rx,
            stdout,
            state,
            stderr,
        } = parked;
        thread::Builder::new()
            .name("eitri-agent-write".into())
            .spawn(move || write_loop(stdin, write_rx))?;
        self.start_reader(stdout, state)?;
        self.start_stderr_drain(stderr)?;
        Ok(())
    }

    fn start_reader(&mut self, stdout: File, state: ReaderState) -> io::Result<()> {
        let stop = StopSignal::new()?;
        let shared = Arc::clone(&self.shared);
        let tx = self.incoming_tx.clone();
        let reader_stop = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("eitri-agent-read".into())
            .spawn(move || read_loop(stdout, state, shared, tx, reader_stop))?;
        self.reader = Some((handle, stop));
        Ok(())
    }

    fn start_stderr_drain(&mut self, stderr: File) -> io::Result<()> {
        let stop = StopSignal::new()?;
        let shared = Arc::clone(&self.shared);
        let drain_stop = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("eitri-agent-stderr".into())
            .spawn(move || drain_stderr(stderr, shared, drain_stop))?;
        self.stderr_drain = Some((handle, stop));
        Ok(())
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
            map.slots.insert(
                id,
                Entry {
                    slot: Arc::clone(&slot),
                    method: method.to_string(),
                    bound: true,
                },
            );
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

    /// The [`Pending`] for a request another link sent and this one took over, by its id. `None` for
    /// an id this link was not handed, or one already rebound.
    pub fn rebind(&self, id: u64) -> Option<Pending> {
        let mut map = lock(&self.shared.pending);
        let entry = map.slots.get_mut(&id)?;
        if entry.bound {
            return None;
        }
        entry.bound = true;
        let slot = Arc::clone(&entry.slot);
        if slot.is_filled() {
            map.slots.remove(&id);
        }
        Some(Pending {
            id,
            slot,
            shared: Arc::clone(&self.shared),
        })
    }

    /// A [`Pending`] for an answer that arrived before the link was stopped: it resolves at once,
    /// with that answer, and only once.
    pub fn answered(&self, record: AnsweredRecord) -> Pending {
        let slot = Slot::new();
        slot.fill(record.result);
        Pending {
            id: record.id,
            slot,
            shared: Arc::clone(&self.shared),
        }
    }

    /// Stops the link so that another process can take the conversation up: the reader stops at once,
    /// wherever it is (waiting for the agent, or holding a message for a full queue), the writer's queue
    /// is written out (bounded, 2 s), the stderr drain stops, and no pending request fails and no close
    /// is queued. Returns the record and the agent's stdin, stdout and stderr; the child is not ended,
    /// now or when what is left of this link is dropped.
    ///
    /// Refused, with the link handed back running, when a caller blocked in [`Pending::wait`] has no
    /// answer within 2 s, the agent does not take what was written to it within 2 s, or the conversation
    /// has already ended.
    pub fn stop_for_move(mut self) -> Result<(LinkHandoff, [OwnedFd; 3]), StopRefused> {
        if self.parked.is_some() {
            return Err(StopRefused {
                link: Box::new(self),
                reason: "the link was never started after it was taken up".to_string(),
            });
        }
        let deadline = Instant::now() + MOVE_WAIT;
        if let Err(reason) = self.wait_for_waiters(deadline) {
            return Err(StopRefused {
                link: Box::new(self),
                reason,
            });
        }
        let stopped = match self.stop_reader() {
            Ok(stopped) => stopped,
            Err(reason) => {
                return Err(StopRefused {
                    link: Box::new(self),
                    reason,
                })
            }
        };
        if let Err(reason) = self.flush_writer(deadline) {
            return Err(self.refuse(reason, stopped, None));
        }
        let stderr = match self.stop_stderr_drain() {
            Ok(stderr) => stderr,
            Err(reason) => return Err(self.refuse(reason, stopped, None)),
        };
        let stdin = match self.release_writer() {
            Ok(stdin) => stdin,
            Err(reason) => return Err(self.refuse(reason, stopped, Some(stderr))),
        };

        let ReaderStopped {
            state:
                ReaderState {
                    unread,
                    line,
                    over_cap,
                    blocked,
                },
            stdout,
        } = stopped;
        let mut incoming = Vec::new();
        while let Ok(item) = self.incoming.try_recv() {
            if !matches!(item, Incoming::Closed { .. }) {
                incoming.push(incoming_value(&item));
            }
        }
        if let Some(item) = blocked {
            incoming.push(incoming_value(&item));
        }
        let mut pending = Vec::new();
        let mut answered = Vec::new();
        for (id, entry) in &lock(&self.shared.pending).slots {
            // A request whose answer came keeps its entry only while it was taken over and not
            // rebound; that answer moves on as data.
            match lock(&entry.slot.value).clone() {
                Some(result) => answered.push(AnsweredRecord { id: *id, result }),
                None => pending.push(PendingRecord {
                    id: *id,
                    method: entry.method.clone(),
                    kind: if entry.slot.waited.load(Ordering::SeqCst) {
                        PendingKind::Waited
                    } else {
                        PendingKind::Polled
                    },
                }),
            }
        }
        pending.sort_by_key(|record| record.id);
        answered.sort_by_key(|record| record.id);
        let (stderr_tail, stderr_partial) = {
            let tail: Vec<u8> = lock(&self.shared.stderr).iter().copied().collect();
            let cut = tail.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            (tail[..cut].to_vec(), tail[cut..].to_vec())
        };
        let handoff = LinkHandoff {
            unread,
            partial_line: line,
            over_cap,
            incoming,
            pending,
            answered,
            next_id: self.shared.next_id.load(Ordering::SeqCst),
            stderr_tail,
            stderr_partial,
            pid: self.pid,
            start: self.start,
            framing: self.shared.framing,
        };
        // The child now belongs to whoever takes the handoff. Dropping a std `Child` neither kills nor
        // waits for it, so once the agent exits it stays a zombie of this process until this process
        // exits or takes it back with `resume_after_move`. That keeps its pid from naming another
        // process while the holder ends it by pid and start time.
        self.owns_child = false;
        drop(self.child.take());
        Ok((
            handoff,
            [OwnedFd::from(stdin), OwnedFd::from(stdout), OwnedFd::from(stderr)],
        ))
    }

    /// Waits until no caller blocked in [`Pending::wait`] is still without its answer.
    fn wait_for_waiters(&self, deadline: Instant) -> Result<(), String> {
        loop {
            let waiting = lock(&self.shared.pending)
                .slots
                .values()
                .any(|entry| entry.slot.waited.load(Ordering::SeqCst) && !entry.slot.is_filled());
            if !waiting {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("a request a caller is waiting on was not answered within 2 s".to_string());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn stop_reader(&mut self) -> Result<ReaderStopped, String> {
        let Some((handle, stop)) = self.reader.take() else {
            return Err("the agent's output is no longer read".to_string());
        };
        stop.raise();
        match handle.join() {
            Ok(Some(stopped)) => Ok(stopped),
            // The reader had ended on its own, and the owner has been told through the queue.
            Ok(None) => Err("the agent's connection closed".to_string()),
            Err(_) => Err("the agent's reader stopped unexpectedly".to_string()),
        }
    }

    /// Waits until every line queued so far has been written and its write returned.
    fn flush_writer(&self, deadline: Instant) -> Result<(), String> {
        let (done_tx, done_rx) = sync_channel::<()>(1);
        self.queue_for_writer(WriteItem::Flush(done_tx), deadline)?;
        let left = deadline.saturating_duration_since(Instant::now());
        done_rx
            .recv_timeout(left)
            .map_err(|_| "the agent did not take what was written to it within 2 s".to_string())
    }

    fn stop_stderr_drain(&mut self) -> Result<File, String> {
        let Some((handle, stop)) = self.stderr_drain.take() else {
            return Err("the agent's stderr is no longer read".to_string());
        };
        stop.raise();
        handle
            .join()
            .map_err(|_| "the agent's stderr drain stopped unexpectedly".to_string())
    }

    /// Ends the writer, taking back the agent's stdin. Only after a flush: the queue is then empty,
    /// and nothing else adds to it once the reader has stopped, so the writer answers at once.
    fn release_writer(&mut self) -> Result<File, String> {
        let (back_tx, back_rx) = sync_channel::<File>(1);
        self.queue_for_writer(WriteItem::Release(back_tx), Instant::now() + MOVE_WAIT)?;
        let stdin = back_rx
            .recv_timeout(MOVE_WAIT)
            .map_err(|_| "the agent's input could not be taken back".to_string())?;
        lock(&self.shared.writer).take();
        Ok(stdin)
    }

    fn queue_for_writer(&self, item: WriteItem, deadline: Instant) -> Result<(), String> {
        let Some(tx) = lock(&self.shared.writer).clone() else {
            return Err("the agent's input is closed".to_string());
        };
        let mut item = item;
        loop {
            match tx.try_send(item) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Disconnected(_)) => return Err("the agent's input is closed".to_string()),
                Err(TrySendError::Full(back)) => {
                    if Instant::now() >= deadline {
                        return Err("the agent did not take what was written to it within 2 s".to_string());
                    }
                    item = back;
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    /// Puts back what a refused stop had stopped, so the link goes on as it was.
    fn refuse(mut self, reason: String, reader: ReaderStopped, stderr: Option<File>) -> StopRefused {
        if let Err(e) = self.start_reader(reader.stdout, reader.state) {
            self.lose_reader(&format!("the agent's reader could not be started again: {e}"));
        }
        if let Some(stderr) = stderr {
            if let Err(e) = self.start_stderr_drain(stderr) {
                link_log!("the agent's stderr drain could not be started again: {e}");
            }
        }
        StopRefused {
            link: Box::new(self),
            reason,
        }
    }

    /// What the reader's end would have done, for a reader that could not be started.
    fn lose_reader(&self, reason: &str) {
        self.shared.fail_all();
        self.shared.queue_closed(&self.incoming_tx, reason.to_string());
    }

    pub fn notify(&self, method: &str, params: Value) {
        let mut members = vec![("method", json_text(&Value::from(method)))];
        if !params.is_null() {
            members.push(("params", json_text(&params)));
        }
        if let Err(e) = self.shared.send_line(self.shared.line(&members)) {
            link_log!("a notification to the agent was not sent: {e}");
        }
    }

    /// Answers a request the agent made, by its own id.
    pub fn respond(&self, id: Value, result: Value) {
        if let Err(e) = self.shared.respond(&id, &result) {
            link_log!("an answer to the agent was not sent: {e}");
        }
    }

    pub fn respond_error(&self, id: Value, code: i64, message: &str) {
        if let Err(e) = self.shared.respond_error(&id, code, message) {
            link_log!("an error answer to the agent was not sent: {e}");
        }
    }

    /// The next message from the agent, if one is waiting.
    pub fn try_recv(&self) -> Option<Incoming> {
        if let Ok(item) = self.incoming.try_recv() {
            return Some(item);
        }
        let mut held = lock(&self.shared.held_closed);
        // A close is held back only after the reader's last message went in, so once it is there a
        // second look at the queue sees everything that came before it.
        if held.is_some() {
            if let Ok(item) = self.incoming.try_recv() {
                return Some(item);
            }
        }
        held.take()
    }

    /// The end of what the child wrote to stderr, bounded, with email addresses replaced.
    pub fn stderr_tail(&self) -> String {
        let bytes: Vec<u8> = lock(&self.shared.stderr).iter().copied().collect();
        redact_emails(&String::from_utf8_lossy(&bytes))
    }

    /// Closes the child's stdin and waits up to `grace` for it to exit; then SIGKILL to the captured pid's process group.
    pub fn shutdown(mut self, grace: Duration) {
        self.finish(grace);
    }

    fn finish(&mut self, grace: Duration) {
        // Dropping the sender ends the writer thread, which drops stdin: the child sees EOF.
        lock(&self.shared.writer).take();
        // A child handed over for a move is the taker's to end.
        if !std::mem::replace(&mut self.owns_child, false) {
            return;
        }
        let child = self.child.take();
        let end = Instant::now() + grace;
        let exited = loop {
            match child_state(self.pid) {
                ChildState::Running if Instant::now() < end => thread::sleep(Duration::from_millis(10)),
                ChildState::Running => break false,
                ChildState::Exited => break true,
                // Not an unreaped child of this process.
                ChildState::Gone => {
                    match child {
                        // Reaped by someone else (SIGCHLD ignored): the pid may already name another
                        // process, so nothing is signalled.
                        Some(mut child) => {
                            let _ = child.wait();
                        }
                        None => end_foreign_child(self.pid, self.start, end),
                    }
                    return;
                }
            }
        };
        // The child is not reaped yet, alive or a zombie, so its pid still names its process group
        // and neither can belong to anything else. The group is killed even when the child exited
        // on its own: a grandchild left in it would keep our pipes open, and with them the reader
        // and stderr threads, for good.
        kill_group(self.pid);
        match child {
            Some(mut child) => {
                if !exited {
                    let _ = child.kill();
                }
                let _ = child.wait();
            }
            // Taken back after a move that did not happen: still this process's child, without a
            // `Child` to wait through.
            None => reap(self.pid),
        }
    }
}

/// Ends a child another process started and handed over. It cannot be waited for here, so it is
/// named by its pid and start time, and signalled through a pidfd opened while that start time still
/// matched: the pidfd keeps naming that process, and its group signal reaches the group it led even
/// after its parent has reaped it, so a pid reused meanwhile is never signalled.
#[cfg(target_os = "linux")]
fn end_foreign_child(pid: u32, start: u64, end: Instant) {
    if start == 0 {
        return;
    }
    let Ok(leader) = crate::child::PidFd::open(pid, start) else {
        return;
    };
    while Instant::now() < end && !leader.exited() {
        thread::sleep(Duration::from_millis(10));
    }
    let _ = leader.signal_group(libc::SIGKILL);
}

/// Ends a child another process started and handed over, named by its pid and start time: the group
/// is killed only while that same process is alive or a zombie, because once its parent has reaped
/// it the pid may name something else.
#[cfg(not(target_os = "linux"))]
fn end_foreign_child(pid: u32, start: u64, end: Instant) {
    let same = || start != 0 && proc_stat(pid).is_some_and(|stat| stat.start == start);
    while Instant::now() < end && proc_stat(pid).is_some_and(|stat| stat.start == start && stat.state != 'Z') {
        thread::sleep(Duration::from_millis(10));
    }
    if same() {
        kill_group(pid);
    }
}

fn reap(pid: u32) {
    let Ok(pid) = libc::pid_t::try_from(pid) else { return };
    let mut status = 0;
    loop {
        // SAFETY: `status` lives for the call; the pid is an unreaped child of this process.
        let r = unsafe { libc::waitpid(pid, &mut status, 0) };
        if r >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return;
        }
    }
}

struct ProcStat {
    // Read where no pidfd stands in for it, and by tests.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    state: char,
    start: u64,
}

/// A process's state letter and start time (field 22, clock ticks since boot) from `/proc`. `None`
/// for a process that is gone, and off Linux.
fn proc_stat(pid: u32) -> Option<ProcStat> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is in parentheses and may hold anything, spaces and parentheses included.
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let start = fields.nth(18)?.parse().ok()?;
    Some(ProcStat { state, start })
}

fn incoming_value(item: &Incoming) -> Value {
    serde_json::to_value(item).unwrap_or(Value::Null)
}

enum ChildState {
    Running,
    /// Exited and not reaped: its pid stays reserved until `wait`.
    Exited,
    /// Not our unreaped child any more.
    Gone,
}

/// Whether the child has exited, asked without reaping it, so its pid keeps naming its group.
fn child_state(pid: u32) -> ChildState {
    let pid: libc::id_t = pid;
    // SAFETY: an all-zero `siginfo_t` is a valid value, and `waitid` writes only into it.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `info` lives for the call; WNOWAIT leaves the child waitable.
    let r = unsafe {
        libc::waitid(
            libc::P_PID,
            pid,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if r != 0 {
        return if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            ChildState::Running
        } else {
            ChildState::Gone
        };
    }
    // With WNOHANG and nothing to report the pid field stays as zeroed above.
    // SAFETY: `waitid` succeeded, so `info` holds a child state or the zeroes it started with.
    if unsafe { info.si_pid() } == 0 {
        ChildState::Running
    } else {
        ChildState::Exited
    }
}

fn kill_group(pid: u32) {
    let Ok(pid) = libc::pid_t::try_from(pid) else { return };
    // SAFETY: `kill` takes no pointers; the group is the one `spawn` made the child the leader of.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

impl Drop for JsonRpcLink {
    fn drop(&mut self) {
        self.finish(Duration::ZERO);
    }
}

fn write_loop(mut stdin: File, queue: Receiver<WriteItem>) {
    for item in queue {
        match item {
            WriteItem::Line(line) => {
                if stdin.write_all(&line).and_then(|_| stdin.flush()).is_err() {
                    return;
                }
            }
            WriteItem::Flush(done) => {
                let _ = done.send(());
            }
            WriteItem::Release(back) => {
                let _ = back.send(stdin);
                return;
            }
        }
    }
}

/// Keeps the stderr tail until the agent closes stderr or the drain is stopped, and gives the
/// descriptor back either way, so that a stop for a move can hand it on.
fn drain_stderr(mut stderr: File, shared: Arc<Shared>, stop: Arc<StopSignal>) -> File {
    let mut buf = [0u8; 4096];
    loop {
        if !stop.wait_readable(stderr.as_raw_fd()) {
            return stderr;
        }
        match stderr.read(&mut buf) {
            Ok(0) => return stderr,
            Ok(n) => shared.push_stderr(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return stderr,
        }
    }
}

/// Ends the reader however it ends: pending calls fail and the owner is told, also when the loop
/// unwinds, which would otherwise leave every caller waiting out its deadline. A reader stopped for
/// a move disarms it: the conversation goes on elsewhere, and its requests are still open there.
struct ReaderEnd {
    shared: Arc<Shared>,
    tx: SyncSender<Incoming>,
    reason: String,
    armed: bool,
}

impl ReaderEnd {
    fn new(shared: Arc<Shared>, tx: SyncSender<Incoming>) -> ReaderEnd {
        ReaderEnd {
            shared,
            tx,
            reason: "the reader stopped unexpectedly".to_string(),
            armed: true,
        }
    }
}

impl Drop for ReaderEnd {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.shared.fail_all();
        self.shared.queue_closed(&self.tx, std::mem::take(&mut self.reason));
    }
}

/// What the reader holds of the agent's output that the owner has not been given.
#[derive(Default)]
struct ReaderState {
    /// Read from the agent, not looked at yet.
    unread: Vec<u8>,
    /// The line in progress, or the kept prefix of an over-long one.
    line: Vec<u8>,
    over_cap: bool,
    /// A message read whole that was waiting for room in the incoming queue.
    blocked: Option<Incoming>,
}

/// A reader stopped on request, with its state and the agent's stdout.
struct ReaderStopped {
    state: ReaderState,
    stdout: File,
}

/// Bytes read from the agent and not looked at yet. A buffer of its own rather than a `BufReader`, so
/// it can start with bytes another reader had read, and give back what is left when it stops.
struct InBuf {
    data: Vec<u8>,
    start: usize,
    end: usize,
}

impl InBuf {
    fn new(carried: Vec<u8>) -> InBuf {
        let end = carried.len();
        let mut data = carried;
        if data.len() < READ_CHUNK {
            data.resize(READ_CHUNK, 0);
        }
        InBuf { data, start: 0, end }
    }

    fn is_empty(&self) -> bool {
        self.start == self.end
    }

    fn available(&self) -> &[u8] {
        &self.data[self.start..self.end]
    }

    fn consume(&mut self, n: usize) {
        self.start = (self.start + n).min(self.end);
    }

    /// Reads into an empty buffer.
    fn fill(&mut self, from: &mut File) -> io::Result<usize> {
        self.start = 0;
        self.end = 0;
        let n = from.read(&mut self.data)?;
        self.end = n;
        Ok(n)
    }
}

/// Why a step of the reader did not go on.
enum Halt {
    /// The conversation ended.
    Failed(String),
    /// The stop was raised while this message waited for room in the queue.
    Stopped(Incoming),
}

fn read_loop(
    mut stdout: File,
    state: ReaderState,
    shared: Arc<Shared>,
    tx: SyncSender<Incoming>,
    stop: Arc<StopSignal>,
) -> Option<ReaderStopped> {
    let mut end = ReaderEnd::new(shared, tx);
    let ReaderState {
        unread,
        line: carried_line,
        over_cap: carried_over_cap,
        blocked,
    } = state;
    let mut buf = InBuf::new(unread);
    // Set once the line in progress passed the cap: only its first bytes are kept.
    let (mut line, mut over_cap) = if carried_over_cap {
        (Vec::new(), Some(carried_line))
    } else {
        (carried_line, None)
    };
    fn stopped(
        end: &mut ReaderEnd,
        buf: &InBuf,
        line: Vec<u8>,
        over_cap: Option<Vec<u8>>,
        blocked: Option<Incoming>,
        stdout: File,
    ) -> Option<ReaderStopped> {
        end.armed = false;
        Some(ReaderStopped {
            state: ReaderState {
                unread: buf.available().to_vec(),
                over_cap: over_cap.is_some(),
                line: over_cap.unwrap_or(line),
                blocked,
            },
            stdout,
        })
    }
    if let Some(item) = blocked {
        match push(&end.tx, item, &stop, &end.shared) {
            Ok(()) => {}
            Err(Halt::Stopped(item)) => return stopped(&mut end, &buf, line, over_cap, Some(item), stdout),
            Err(Halt::Failed(reason)) => {
                end.reason = reason;
                return None;
            }
        }
    }
    let reason = loop {
        if stop.raised() {
            return stopped(&mut end, &buf, line, over_cap, None, stdout);
        }
        if buf.is_empty() {
            // An idle agent never writes again, so the stop has to be able to end this wait.
            if !stop.wait_readable(stdout.as_raw_fd()) {
                return stopped(&mut end, &buf, line, over_cap, None, stdout);
            }
            match buf.fill(&mut stdout) {
                Ok(0) => break "the agent closed its output".to_string(),
                Ok(n) => {
                    end.shared.bytes_read.fetch_add(n as u64, Ordering::SeqCst);
                    continue;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => break format!("reading from the agent failed: {e}"),
            }
        }
        let avail = buf.available();
        let newline = avail.iter().position(|b| *b == b'\n');
        let chunk = &avail[..newline.unwrap_or(avail.len())];
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
        buf.consume(used);
        if newline.is_none() {
            continue;
        }
        let verdict = match over_cap.take() {
            Some(prefix) => handle_over_cap(&prefix, &end.shared, &end.tx, &stop),
            None => {
                let whole = std::mem::take(&mut line);
                handle_line(&whole, &end.shared, &end.tx, &stop)
            }
        };
        line.clear();
        match verdict {
            Ok(()) => {}
            Err(Halt::Failed(reason)) => break reason,
            // The line was read whole: the message is all that is left of it.
            Err(Halt::Stopped(item)) => return stopped(&mut end, &buf, line, None, Some(item), stdout),
        }
    };
    end.reason = reason;
    None
}

/// Queues a message for the owner. A full queue is waited on in short steps, so that a stop is seen
/// while the owner is not draining: the message then goes back to the caller.
fn push(tx: &SyncSender<Incoming>, item: Incoming, stop: &StopSignal, shared: &Shared) -> Result<(), Halt> {
    let mut item = item;
    loop {
        match tx.try_send(item) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Disconnected(_)) => return Err(Halt::Failed("the link was dropped".to_string())),
            Err(TrySendError::Full(back)) => {
                item = back;
                shared.reader_waiting.store(true, Ordering::SeqCst);
                let stopped = stop.wait_for(QUEUE_RETRY);
                shared.reader_waiting.store(false, Ordering::SeqCst);
                if stopped {
                    return Err(Halt::Stopped(item));
                }
            }
        }
    }
}

/// What a log line may say about a line it dropped: its size and what kind of byte it starts with,
/// never its content, which can carry account data.
fn describe_dropped(bytes: &[u8]) -> String {
    let class = match bytes.first() {
        None => "empty",
        Some(b'[') => "starts like an array",
        Some(b'"') => "starts like a string",
        Some(b) if b.is_ascii_digit() || *b == b'-' => "starts like a number",
        Some(b) if b.is_ascii_alphabetic() => "starts with a letter",
        Some(b) if b.is_ascii() => "starts with punctuation",
        Some(_) => "starts with a non-ASCII byte",
    };
    format!("{} bytes, {class}", bytes.len())
}

fn handle_line(bytes: &[u8], shared: &Shared, tx: &SyncSender<Incoming>, stop: &StopSignal) -> Result<(), Halt> {
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    let Ok(Value::Object(mut map)) = serde_json::from_slice::<Value>(bytes) else {
        link_log!(
            "a line from the agent was not a JSON object and was dropped ({})",
            describe_dropped(bytes)
        );
        return Ok(());
    };
    let method = match map.get("method") {
        None => None,
        Some(Value::String(m)) => Some(m.clone()),
        Some(_) => {
            link_log!("a message from the agent had a method that is not a string and was dropped");
            return Ok(());
        }
    };
    let id = map.remove("id").filter(|id| !id.is_null());
    match (method, id) {
        (Some(method), Some(id)) => {
            let params = map.remove("params").unwrap_or(Value::Null);
            push(tx, Incoming::Request { id, method, params }, stop, shared)
        }
        (Some(method), None) => {
            let params = map.remove("params").unwrap_or(Value::Null);
            push(tx, Incoming::Notification { method, params }, stop, shared)
        }
        (None, Some(id)) => {
            deliver_response(shared, &id, &mut map);
            Ok(())
        }
        (None, None) => {
            link_log!("a message from the agent had neither a method nor an id and was dropped");
            Ok(())
        }
    }
}

fn deliver_response(shared: &Shared, id: &Value, map: &mut serde_json::Map<String, Value>) {
    let Some(id) = id.as_u64() else {
        return;
    };
    let result = match map.remove("error") {
        Some(error) => Err(LinkError::Rpc {
            code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: error.get("message").and_then(Value::as_str).unwrap_or("").to_string(),
        }),
        None => Ok(map.remove("result").unwrap_or(Value::Null)),
    };
    shared.resolve(id, result);
}

fn handle_over_cap(prefix: &[u8], shared: &Shared, tx: &SyncSender<Incoming>, stop: &StopSignal) -> Result<(), Halt> {
    let found = scan_prefix(prefix);
    link_log!("a message from the agent was larger than 16 MiB");
    match (found.has_method, found.has_id) {
        (true, true) => {
            let (Some(id), method) = (found.id, found.method.unwrap_or_default()) else {
                return Err(Halt::Failed(
                    "a request from the agent was too large and its id could not be read".to_string(),
                ));
            };
            // An unanswered request hangs the agent; the provider's answer refuses what it asked.
            let answered = match (shared.over_cap)(&method) {
                OverCapReply::Result(result) => shared.respond(&id, &result),
                OverCapReply::Error { code, message } => shared.respond_error(&id, code, message),
            };
            if let Err(e) = answered {
                link_log!("the answer to an over-sized request was not sent: {e}");
            }
            push(
                tx,
                Incoming::Skipped(Skipped::RequestAnswered {
                    method,
                    thread_id: found.thread_id,
                    session_id: found.session_id,
                    item_id: found.item_id,
                }),
                stop,
                shared,
            )
        }
        (false, true) => match found.id.as_ref().and_then(Value::as_u64) {
            Some(id) => {
                shared.resolve(id, Err(LinkError::TooLarge));
                push(tx, Incoming::Skipped(Skipped::ResponseFailed { id }), stop, shared)
            }
            None => {
                link_log!("an over-sized response with an unreadable id was dropped");
                Ok(())
            }
        },
        (true, false) => {
            let method = found.method.unwrap_or_default();
            let thread_id = found.thread_id;
            if method == "turn/started" || method == "turn/completed" {
                push(
                    tx,
                    Incoming::Skipped(Skipped::TurnTooLarge { method, thread_id }),
                    stop,
                    shared,
                )
            } else if method.starts_with("item/") {
                push(
                    tx,
                    Incoming::Skipped(Skipped::ItemTooLarge {
                        method,
                        item_id: found.item_id,
                        thread_id,
                    }),
                    stop,
                    shared,
                )
            } else {
                link_log!("an over-sized notification was dropped");
                Ok(())
            }
        }
        (false, false) => Err(Halt::Failed(
            "a message from the agent was too large and could not be classified".to_string(),
        )),
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
    session_id: Option<String>,
    item_id: Option<String>,
}

/// Reads as much of a truncated JSON object as the bytes hold, recording the keys the link cares
/// about. It never fails: where the bytes stop making sense it stops, and keeps what it found.
fn scan_prefix(bytes: &[u8]) -> Prefix {
    let mut scan = Scan {
        b: bytes,
        i: 0,
        found: Prefix::default(),
        steps: 0,
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
    /// Loop turns and values taken so far, against [`Scan::STEP_BUDGET`].
    steps: usize,
}

/// The bytes ended or stopped being JSON.
struct Cut;

impl Scan<'_> {
    /// Far more than any prefix of [`PREFIX_LEN`] bytes can need: every turn but a malformed one
    /// consumes a byte, so this only ever ends a scan that has stopped advancing.
    const STEP_BUDGET: usize = 4 * PREFIX_LEN;

    fn spend(&mut self) -> Result<(), Cut> {
        self.steps += 1;
        if self.steps > Self::STEP_BUDGET {
            return Err(Cut);
        }
        Ok(())
    }

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
    /// `None` when the string is not exactly what a JSON parser would read (an invalid escape, a lone
    /// surrogate, bytes that are not UTF-8): a key or an id read approximately could name the wrong thing.
    fn string(&mut self) -> Result<Option<String>, Cut> {
        let start = self.i;
        self.i += 1;
        loop {
            match self.next()? {
                b'"' => return Ok(serde_json::from_slice::<String>(&self.b[start..self.i]).ok()),
                b'\\' => {
                    self.next()?;
                }
                _ => {}
            }
        }
    }

    fn object(&mut self, path: &mut Vec<String>) -> Result<(), Cut> {
        self.i += 1;
        loop {
            self.spend()?;
            self.ws();
            match *self.b.get(self.i).ok_or(Cut)? {
                b'}' => {
                    self.i += 1;
                    return Ok(());
                }
                b',' => self.i += 1,
                b'"' => {
                    // A key not read exactly matches none of the paths recorded.
                    let key = self.string()?.unwrap_or_else(|| "\u{0}unread".to_string());
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
            self.spend()?;
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
        self.spend()?;
        self.ws();
        match *self.b.get(self.i).ok_or(Cut)? {
            b'{' => self.object(path),
            b'[' => self.array(path),
            b'"' => {
                if let Some(text) = self.string()? {
                    self.record_string(path, text);
                }
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
                // No token at all is a closer of the wrong kind or a stray comma: the bytes stopped
                // being JSON here, and an array would otherwise ask for the same byte forever.
                if self.i == start {
                    return Err(Cut);
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
            ["params", "sessionId"] => self.found.session_id = Some(text),
            ["params", "itemId"] | ["params", "item", "id"] | ["params", "toolCall", "toolCallId"] => {
                self.found.item_id = Some(text)
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Runs the scan on a thread so a scan that never returns fails the test instead of hanging it.
    fn scan_within(bytes: &'static [u8]) -> Prefix {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(scan_prefix(bytes));
        });
        rx.recv_timeout(Duration::from_secs(2))
            .expect("scan_prefix did not return")
    }

    #[test]
    fn a_closing_brace_where_an_array_element_should_be_ends_the_scan() {
        let cut_inside_list = scan_within(br#"{"method":"item/started","params":{"threadId":"t1","list":[1}"#);
        assert!(cut_inside_list.has_method);
        assert_eq!(cut_inside_list.method.as_deref(), Some("item/started"));
        assert_eq!(cut_inside_list.thread_id.as_deref(), Some("t1"));
        for bytes in [
            &br#"{"method":"x","params":[}"#[..],
            br#"{"method":"x","params":[ }"#,
            br#"{"a":[1,2}"#,
            br#"{"a":[,}"#,
            br#"{"a":[{"b":1}}"#,
            br#"{"a":[[}]]"#,
            br#"{"a":[1}"#,
        ] {
            // Each leaks into a `'static` slice only because the helper moves it to a thread.
            let leaked: &'static [u8] = Box::leak(bytes.to_vec().into_boxed_slice());
            let found = scan_within(leaked);
            assert!(!found.has_id, "{:?}", String::from_utf8_lossy(bytes));
        }
    }

    #[test]
    fn well_formed_prefixes_are_classified_as_before() {
        let request = scan_within(br#"{"method":"x","id":3,"params":[1,[2],{"k":[]}]"#);
        assert!(request.has_method && request.has_id);
        assert_eq!(request.id, Some(Value::from(3)));
        assert_eq!(request.method.as_deref(), Some("x"));

        let permission = scan_within(
            br#"{"jsonrpc":"2.0","id":7,"method":"session/request_permission","params":{"sessionId":"s1","toolCall":{"toolCallId":"c9","rawInput":{"#,
        );
        assert_eq!(permission.session_id.as_deref(), Some("s1"));
        // An id is recorded as a JSON parser reads it, or not at all.
        let escaped =
            scan_within(br#"{"id":7,"method":"m","params":{"sessionId":"s\u0031","toolCall":{"toolCallId":"c\q9"}"#);
        assert_eq!(escaped.session_id.as_deref(), Some("s1"));
        assert!(escaped.item_id.is_none(), "an invalid escape was read as an id");
        let key = scan_within(br#"{"id":7,"method":"m","params":{"\u0069temId":"i1","item\d":"i2"}"#);
        assert_eq!(key.item_id.as_deref(), Some("i1"));
        assert_eq!(permission.item_id.as_deref(), Some("c9"));
        assert!(permission.thread_id.is_none());

        let item =
            scan_within(br#"{"method":"item/completed","params":{"item":{"id":"i9","list":[1,2,3]},"threadId":"t2"#);
        assert_eq!(item.item_id.as_deref(), Some("i9"));
        assert!(
            item.thread_id.is_none(),
            "a string cut before its quote is not recorded"
        );

        let response = scan_within(br#"{"id":"abc","result":{"a":[true,null,"s"]}"#);
        assert!(response.has_id && !response.has_method);
        assert_eq!(response.id, Some(Value::String("abc".into())));
    }

    #[test]
    fn the_step_budget_bounds_a_scan_whatever_the_bytes_are() {
        let mut scan = Scan {
            b: b"",
            i: 0,
            found: Prefix::default(),
            steps: 0,
        };
        for _ in 0..=Scan::STEP_BUDGET {
            let _ = scan.spend();
        }
        assert!(scan.spend().is_err());
    }

    fn shared_for_test() -> Arc<Shared> {
        Arc::new(Shared::new(Framing::Bare, refuse_over_cap, 1, None))
    }

    fn pending_in(shared: &Arc<Shared>, id: u64) -> Pending {
        let slot = Slot::new();
        lock(&shared.pending).slots.insert(
            id,
            Entry {
                slot: Arc::clone(&slot),
                method: "m".to_string(),
                bound: true,
            },
        );
        Pending {
            id,
            slot,
            shared: Arc::clone(shared),
        }
    }

    #[test]
    fn a_reader_that_unwinds_still_fails_pending_calls_and_reports_closed() {
        let shared = shared_for_test();
        let pending = pending_in(&shared, 7);
        let (tx, rx) = sync_channel::<Incoming>(4);
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _end = ReaderEnd::new(Arc::clone(&shared), tx);
            panic!("the reader body failed");
        }));
        assert!(unwound.is_err());
        assert_eq!(
            pending.wait(Instant::now() + Duration::from_secs(1)),
            Err(LinkError::Closed)
        );
        assert!(lock(&shared.pending).closed);
        match rx.try_recv() {
            Ok(Incoming::Closed { reason }) => assert_eq!(reason, "the reader stopped unexpectedly"),
            other => panic!("expected Closed, got {other:?}"),
        }
    }

    #[test]
    fn a_reader_that_ends_normally_reports_its_own_reason() {
        let shared = shared_for_test();
        let (tx, rx) = sync_channel::<Incoming>(4);
        {
            let mut end = ReaderEnd::new(Arc::clone(&shared), tx);
            end.reason = "the agent closed its output".to_string();
        }
        assert_eq!(
            rx.try_recv().ok(),
            Some(Incoming::Closed {
                reason: "the agent closed its output".to_string()
            })
        );
    }

    #[test]
    fn a_log_line_to_a_broken_stream_is_dropped_without_a_panic() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
        }
        write_log(&mut Broken, format_args!("a line {}", 1));
    }

    #[test]
    fn a_dropped_line_is_described_without_its_content() {
        let note = describe_dropped(b"user@example.com {\"token\":\"secret\"} not json");
        assert!(!note.contains("example"), "{note}");
        assert!(!note.contains("secret"), "{note}");
        assert!(note.starts_with("44 bytes"), "{note}");
        assert_eq!(describe_dropped(b""), "0 bytes, empty");
        assert_eq!(describe_dropped(b"[1,2]"), "5 bytes, starts like an array");
    }

    fn refuse_over_cap(_: &str) -> OverCapReply {
        OverCapReply::Error {
            code: REQUEST_TOO_LARGE,
            message: "too large",
        }
    }

    /// A stand-in agent: a shell script on the link's stdio.
    fn stand_in(script: &str) -> JsonRpcLink {
        JsonRpcLink::spawn(LinkSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), script.into()],
            cwd: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            framing: Framing::Bare,
            env: Vec::new(),
            env_remove: Vec::new(),
            over_cap: refuse_over_cap,
        })
        .expect("the stand-in starts")
    }

    /// Long enough for a loaded machine; it only turns a hang into a failure.
    const PATIENCE: Duration = Duration::from_secs(30);

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let end = Instant::now() + PATIENCE;
        while !done() {
            assert!(Instant::now() < end, "still waiting for {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Stops `link` on a thread, so a reader that cannot be stopped fails the test instead of hanging it.
    fn stop(link: JsonRpcLink) -> (LinkHandoff, [OwnedFd; 3]) {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(link.stop_for_move());
        });
        match rx.recv_timeout(PATIENCE).expect("the stop returned") {
            Ok(stopped) => stopped,
            Err(refused) => panic!("the stop was refused: {}", refused.reason),
        }
    }

    /// Takes a stopped link up as a destination does once its move is committed.
    fn adopt(handoff: LinkHandoff, fds: [OwnedFd; 3]) -> JsonRpcLink {
        let mut link = JsonRpcLink::adopt(handoff, fds, refuse_over_cap).expect("the link is taken up");
        link.start_after_commit().expect("the adopted link starts");
        link
    }

    /// A shell prelude that waits, reading nothing, until the test sends SIGUSR1: the test decides
    /// when the agent goes on, whatever the scheduler does. It writes `armed` to stderr once the trap
    /// is set; a signal sent before that would take SIGUSR1's default action and kill the agent, so
    /// call [`until_armed`] before [`signal`].
    const UNTIL_SIGNALLED: &str =
        r#"trap 'go=1' USR1; printf 'armed\n' >&2; while [ -z "$go" ]; do sleep 0.02; done; "#;

    /// Waits until an agent started with [`UNTIL_SIGNALLED`] has set its trap.
    fn until_armed(link: &JsonRpcLink) {
        wait_until("the agent to set its signal trap", || {
            link.stderr_tail().lines().any(|line| line == "armed")
        });
    }

    fn signal(pid: u32) {
        // SAFETY: `kill` takes no pointers; the pid is a stand-in this test started and has not reaped.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGUSR1);
        }
    }

    /// Whether `fd` has bytes to read within `within`.
    fn readable(fd: &OwnedFd, within: Duration) -> bool {
        let mut fds = [pollin(fd.as_raw_fd())];
        let ms = i32::try_from(within.as_millis()).unwrap_or(i32::MAX);
        poll(&mut fds, ms).is_ok() && fds[0].revents & libc::POLLIN != 0
    }

    fn index_of(item: &Incoming) -> u64 {
        match item {
            Incoming::Notification { params, .. } => params["i"].as_u64().expect("a numbered message"),
            other => panic!("not a numbered message: {other:?}"),
        }
    }

    fn carried_index(value: &Value) -> u64 {
        index_of(&serde_json::from_value::<Incoming>(value.clone()).expect("a carried message"))
    }

    /// Takes numbered messages from `link` into `seen` until it holds `count`.
    fn drain_numbered(link: &JsonRpcLink, seen: &mut Vec<u64>, count: usize) {
        wait_until(&format!("{count} messages"), || {
            while let Some(item) = link.try_recv() {
                seen.push(index_of(&item));
            }
            seen.len() >= count
        });
    }

    /// An agent that writes `count` numbered notifications as fast as it can, then reads its input.
    fn numbered_flood(count: u64) -> String {
        format!(
            r#"i=0; while [ $i -lt {count} ]; do printf '{{"method":"n","params":{{"i":%d}}}}\n' $i; i=$((i+1)); done; while read -r l; do :; done"#
        )
    }

    /// Whether the agent the handoff names is still running.
    fn running(handoff: &LinkHandoff) -> bool {
        proc_stat(handoff.pid).is_some_and(|stat| stat.start == handoff.start && stat.state != 'Z')
    }

    fn big_line_payload() -> Value {
        Value::from("x".repeat(8192))
    }

    #[test]
    fn an_idle_reader_stops_on_its_stop_fd_with_no_input_pending() {
        let link = stand_in("while read -r l; do :; done");
        let pid = link.pid();
        let (handoff, fds) = stop(link);
        assert_eq!(handoff.pid, pid);
        assert!(handoff.unread.is_empty() && handoff.partial_line.is_empty() && !handoff.over_cap);
        assert!(handoff.incoming.is_empty(), "{:?}", handoff.incoming);
        assert!(handoff.pending.is_empty());
        if cfg!(target_os = "linux") {
            assert!(running(&handoff), "stopping the link ended the agent");
        }
        drop(adopt(handoff, fds));
    }

    #[test]
    fn a_reader_blocked_on_a_full_queue_stops_and_hands_over_the_blocked_message_last() {
        let total = INCOMING_QUEUE as u64 + 500;
        let link = stand_in(&numbered_flood(total));
        let shared = Arc::clone(&link.shared);
        wait_until("the reader to wait for room in the queue", || {
            shared.reader_waiting.load(Ordering::SeqCst)
        });
        let (handoff, fds) = stop(link);
        let carried: Vec<u64> = handoff.incoming.iter().map(carried_index).collect();
        assert_eq!(
            carried,
            (0..=INCOMING_QUEUE as u64).collect::<Vec<_>>(),
            "the queue in order, then the message that waited for room in it"
        );
        let adopted = adopt(handoff, fds);
        let mut seen = Vec::new();
        drain_numbered(&adopted, &mut seen, total as usize);
        assert_eq!(seen, (0..total).collect::<Vec<_>>());
    }

    #[test]
    fn a_line_half_read_at_the_stop_arrives_whole_after_adoption() {
        let head = br#"{"method":"half","par"#;
        let link = stand_in(
            r#"printf '%s' '{"method":"half","par'; read go; printf '%s\n' 'ams":{"x":1}}'; while read -r l; do :; done"#,
        );
        let shared = Arc::clone(&link.shared);
        wait_until("the first half to be read", || {
            shared.bytes_read.load(Ordering::SeqCst) >= head.len() as u64
        });
        let (handoff, fds) = stop(link);
        let mut held = handoff.partial_line.clone();
        held.extend_from_slice(&handoff.unread);
        assert_eq!(held, head.to_vec(), "the half line is carried, as read so far");
        assert!(handoff.incoming.is_empty());
        let adopted = adopt(handoff, fds);
        adopted.notify("go", Value::Null);
        let mut got = None;
        wait_until("the rest of the line", || {
            got = adopted.try_recv();
            got.is_some()
        });
        assert_eq!(
            got,
            Some(Incoming::Notification {
                method: "half".to_string(),
                params: serde_json::json!({ "x": 1 }),
            })
        );
    }

    #[test]
    fn no_closed_is_queued_and_no_pending_request_fails_on_a_stop() {
        let link = stand_in("while read -r l; do :; done");
        let mut asked = link.request("ask", serde_json::json!({}));
        let (handoff, fds) = stop(link);
        assert_eq!(asked.try_take(), None, "a request failed when the link stopped");
        assert_eq!(
            handoff.pending,
            vec![PendingRecord {
                id: asked.id(),
                method: "ask".to_string(),
                kind: PendingKind::Polled,
            }]
        );
        assert!(handoff.incoming.is_empty(), "the stop queued {:?}", handoff.incoming);
        if cfg!(target_os = "linux") {
            assert!(running(&handoff), "what was left of the stopped link ended the agent");
        }
        drop(asked);
        drop(adopt(handoff, fds));
    }

    #[test]
    fn a_pending_request_is_answered_after_adoption_and_its_rebound_pending_resolves() {
        let link = stand_in(
            r#"read req; echo got >&2; read go; printf '{"id":1,"result":"ok"}\n'; while read -r l; do :; done"#,
        );
        let mut asked = link.request("ask", Value::Null);
        assert_eq!(asked.id(), 1);
        wait_until("the agent to read the request", || link.stderr_tail().contains("got"));
        let (handoff, fds) = stop(link);
        assert_eq!(handoff.next_id, 2);
        let adopted = adopt(handoff, fds);
        let rebound = adopted
            .rebind(1)
            .expect("the request is open in the link that took it up");
        assert!(adopted.rebind(1).is_none(), "one request was rebound twice");
        assert!(adopted.rebind(9).is_none(), "an id never sent was rebound");
        adopted.notify("go", Value::Null);
        assert_eq!(rebound.wait(Instant::now() + PATIENCE), Ok(Value::from("ok")));
        assert_eq!(asked.try_take(), None, "the stopped link's copy saw the answer");
        assert_eq!(
            adopted.request("next", Value::Null).id(),
            2,
            "ids go on from the stopped link's"
        );
    }

    #[test]
    fn an_answer_read_after_the_last_pump_and_before_the_stop_is_carried_and_resolves_in_the_destination() {
        let link = stand_in(r#"read req; read now; printf '{"id":1,"result":"late"}\n'; while read -r l; do :; done"#);
        let mut asked = link.request("ask", Value::Null);
        // The owner's last look: the agent holds its answer until told.
        assert_eq!(asked.try_take(), None);
        link.notify("now", Value::Null);
        wait_until("the answer to arrive", || asked.answered_record().is_some());
        let (handoff, fds) = stop(link);
        assert!(
            handoff.pending.is_empty(),
            "an answered request is still open: {:?}",
            handoff.pending
        );
        let record = asked.answered_record().expect("the answer is held");
        assert_eq!(
            record,
            AnsweredRecord {
                id: 1,
                result: Ok(Value::from("late")),
            }
        );
        assert!(
            asked.answered_record().is_some(),
            "asking for the record took the answer away"
        );
        let adopted = adopt(handoff, fds);
        assert!(adopted.rebind(1).is_none(), "an answered request was rebound");
        let mut answered = adopted.answered(record);
        assert_eq!(answered.id(), 1);
        assert_eq!(answered.try_take(), Some(Ok(Value::from("late"))));
        assert_eq!(answered.try_take(), None, "the answer resolved twice");
    }

    #[test]
    fn adopt_writes_nothing_to_the_agent() {
        let link = stand_in(r#"while read -r l; do printf '%s\n' "$l" >&2; done"#);
        let _asked = link.request("ask", Value::Null);
        link.notify("told", Value::Null);
        let (handoff, fds) = stop(link);
        let adopted = adopt(handoff, fds);
        adopted.notify("marker", Value::Null);
        wait_until("the agent to echo the marker", || {
            adopted.stderr_tail().contains("marker")
        });
        let echoed: Vec<String> = adopted.stderr_tail().lines().map(str::to_string).collect();
        assert_eq!(
            echoed,
            vec![
                r#"{"id":1,"method":"ask"}"#,
                r#"{"method":"told"}"#,
                r#"{"method":"marker"}"#
            ],
            "the agent read something between the stop and the next message"
        );
    }

    #[test]
    fn a_reader_stopped_mid_flood_then_resumed_by_the_source_loses_no_line() {
        let total = 20_000u64;
        let link = stand_in(&numbered_flood(total));
        let mut seen = Vec::new();
        drain_numbered(&link, &mut seen, 1000);
        let (handoff, fds) = stop(link);
        let resumed = JsonRpcLink::resume_after_move(handoff, fds, refuse_over_cap).expect("the link resumes");
        drain_numbered(&resumed, &mut seen, total as usize);
        assert!(
            seen.iter().copied().eq(0..total),
            "lines were lost, doubled or reordered"
        );
    }

    #[test]
    fn the_writer_is_flushed_before_the_stop_returns() {
        const LINES: usize = 20;
        let payload = big_line_payload();
        let one = shared_for_test()
            .line(&[
                ("method", json_text(&Value::from("big"))),
                ("params", json_text(&payload)),
            ])
            .len();
        // More than a pipe holds, to an agent that starts reading late: the writer is blocked when
        // the stop begins.
        let link = stand_in(&format!(
            "sleep 0.3; head -c {} >/dev/null; echo all-read >&2; while read -r l; do :; done",
            one * LINES
        ));
        for _ in 0..LINES {
            link.notify("big", payload.clone());
        }
        let (handoff, fds) = stop(link);
        let resumed = JsonRpcLink::resume_after_move(handoff, fds, refuse_over_cap).expect("the link resumes");
        wait_until("the agent to read every line queued before the stop", || {
            resumed.stderr_tail().contains("all-read")
        });
    }

    #[test]
    fn a_stop_the_agent_does_not_read_for_is_refused_and_the_link_runs_on() {
        let link = stand_in(&format!(
            r#"{UNTIL_SIGNALLED}printf '{{"method":"after"}}\n'; while read -r l; do :; done"#
        ));
        let payload = big_line_payload();
        for _ in 0..20 {
            link.notify("big", payload.clone());
        }
        let mut asked = link.request("ask", Value::Null);
        until_armed(&link);
        let refused = match link.stop_for_move() {
            Ok(_) => panic!("an agent that reads nothing let the stop flush"),
            Err(refused) => refused,
        };
        assert!(refused.reason.contains("did not take"), "{}", refused.reason);
        let link = refused.link;
        signal(link.pid());
        let mut got = None;
        wait_until("the agent's next message", || {
            got = link.try_recv();
            got.is_some()
        });
        assert_eq!(
            got,
            Some(Incoming::Notification {
                method: "after".to_string(),
                params: Value::Null,
            })
        );
        assert_eq!(asked.try_take(), None, "a refused stop failed a request");
    }

    #[test]
    fn a_caller_blocked_on_an_answer_that_does_not_come_refuses_the_stop() {
        let link = stand_in("while read -r l; do :; done");
        let asked = link.request("ask", Value::Null);
        let waiter = thread::spawn(move || asked.wait(Instant::now() + Duration::from_secs(10)));
        let shared = Arc::clone(&link.shared);
        wait_until("the caller to block", || {
            lock(&shared.pending)
                .slots
                .values()
                .any(|entry| entry.slot.waited.load(Ordering::SeqCst))
        });
        let refused = match link.stop_for_move() {
            Ok(_) => panic!("the stop left a blocked caller without its answer"),
            Err(refused) => refused,
        };
        assert!(refused.reason.contains("waiting"), "{}", refused.reason);
        drop(refused.link);
        assert_eq!(waiter.join().expect("the caller returns"), Err(LinkError::Closed));
    }

    #[test]
    fn a_reader_that_ends_on_a_full_queue_does_not_hold_up_a_stop_and_its_close_comes_last() {
        let total = INCOMING_QUEUE as u64;
        // Exactly a queue's worth, then the agent closes its output and lives on.
        let link = stand_in(&format!(
            r#"i=0; while [ $i -lt {total} ]; do printf '{{"method":"n","params":{{"i":%d}}}}\n' $i; i=$((i+1)); done; exec >&-; while read -r l; do :; done"#
        ));
        let shared = Arc::clone(&link.shared);
        wait_until("the reader to end", || lock(&shared.held_closed).is_some());
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(link.stop_for_move());
        });
        let refused = match rx.recv_timeout(PATIENCE).expect("the stop returned") {
            Ok(_) => panic!("a link whose agent closed its output was stopped for a move"),
            Err(refused) => refused,
        };
        assert!(refused.reason.contains("closed"), "{}", refused.reason);
        let link = refused.link;
        let mut items = Vec::new();
        while let Some(item) = link.try_recv() {
            items.push(item);
        }
        let last = items.pop();
        assert_eq!(
            last,
            Some(Incoming::Closed {
                reason: "the agent closed its output".to_string()
            })
        );
        assert!(
            items.iter().map(index_of).eq(0..total),
            "the messages before the close were lost or reordered"
        );
    }

    #[test]
    fn an_answer_and_the_end_that_come_before_the_rebind_are_kept_for_it() {
        let link = stand_in(
            r#"read a; read b; read go; printf '{"id":1,"result":"ok"}\n'; exec >&-; while read -r l; do :; done"#,
        );
        let _first = link.request("first", Value::Null);
        let _second = link.request("second", Value::Null);
        let (handoff, fds) = stop(link);
        let adopted = adopt(handoff, fds);
        adopted.notify("go", Value::Null);
        wait_until("the end of the conversation", || {
            matches!(adopted.try_recv(), Some(Incoming::Closed { .. }))
        });
        let mut answered = adopted.rebind(1).expect("the answered request is still handed out");
        assert_eq!(answered.try_take(), Some(Ok(Value::from("ok"))));
        let mut failed = adopted.rebind(2).expect("the open request is still handed out");
        assert_eq!(failed.try_take(), Some(Err(LinkError::Closed)));
    }

    #[test]
    fn an_answer_to_a_request_not_rebound_yet_moves_on_with_a_second_stop() {
        let link = stand_in(r#"read req; read go; printf '{"id":1,"result":"ok"}\n'; while read -r l; do :; done"#);
        let _asked = link.request("ask", Value::Null);
        let (handoff, fds) = stop(link);
        let adopted = adopt(handoff, fds);
        adopted.notify("go", Value::Null);
        let shared = Arc::clone(&adopted.shared);
        wait_until("the answer to arrive", || {
            lock(&shared.pending)
                .slots
                .get(&1)
                .is_some_and(|entry| entry.slot.is_filled())
        });
        let (handoff, fds) = stop(adopted);
        assert!(
            handoff.pending.is_empty(),
            "an answered request is open: {:?}",
            handoff.pending
        );
        assert_eq!(
            handoff.answered,
            vec![AnsweredRecord {
                id: 1,
                result: Ok(Value::from("ok")),
            }]
        );
        let again = adopt(handoff, fds);
        let mut rebound = again.rebind(1).expect("the answered request is handed out");
        assert_eq!(rebound.try_take(), Some(Ok(Value::from("ok"))));
        assert!(again.rebind(1).is_none(), "one request was rebound twice");
    }

    #[test]
    fn an_adopted_link_reads_and_writes_nothing_before_its_commit() {
        let link = stand_in(&format!(
            r#"{UNTIL_SIGNALLED}printf '\n'; while read -r l; do printf '%s\n' "$l" >&2; done"#
        ));
        until_armed(&link);
        let (mut handoff, fds) = stop(link);
        // The source had read the start of a request too long to keep; the agent writes its end later.
        handoff.partial_line = br#"{"id":5,"method":"big","params":{"#.to_vec();
        handoff.over_cap = true;
        let probe = fds[1].try_clone().expect("a second stdout descriptor");
        signal(handoff.pid);
        assert!(
            readable(&probe, PATIENCE),
            "the agent did not write the end of the request"
        );
        let mut adopted = JsonRpcLink::adopt(handoff, fds, refuse_over_cap).expect("the link is taken up");
        assert!(
            adopted.reader.is_none() && adopted.stderr_drain.is_none() && adopted.parked.is_some(),
            "a thread runs on the agent before the commit"
        );
        adopted.notify("queued", Value::Null);
        assert!(
            readable(&probe, Duration::ZERO),
            "the end of the request was read before the commit"
        );
        assert_eq!(adopted.try_recv(), None);
        adopted.start_after_commit().expect("the link starts");
        let mut got = None;
        wait_until("the over-long request to be answered", || {
            got = adopted.try_recv();
            got.is_some()
        });
        assert!(
            matches!(&got, Some(Incoming::Skipped(Skipped::RequestAnswered { method, .. })) if method == "big"),
            "{got:?}"
        );
        wait_until("the agent to read what was queued and the answer", || {
            adopted.stderr_tail().lines().count() >= 3
        });
        let echoed: Vec<String> = adopted.stderr_tail().lines().map(str::to_string).collect();
        assert_eq!(
            echoed,
            vec![
                "armed",
                r#"{"method":"queued"}"#,
                r#"{"id":5,"error":{"code":-32600,"message":"too large"}}"#
            ],
            "what was queued before the commit is written first, after it"
        );
    }

    #[test]
    fn dropping_an_adopted_link_before_its_commit_leaves_the_agent_to_the_source() {
        let link = stand_in(r#"while read -r l; do printf '%s\n' "$l" >&2; done"#);
        let (handoff, fds) = stop(link);
        let kept = fds
            .each_ref()
            .map(|fd| fd.try_clone().expect("a copy of the descriptor"));
        let carried = handoff.clone();
        drop(JsonRpcLink::adopt(handoff, fds, refuse_over_cap).expect("the link is taken up"));
        if cfg!(target_os = "linux") {
            assert!(running(&carried), "dropping a link before its commit ended the agent");
        }
        let resumed = JsonRpcLink::resume_after_move(carried, kept, refuse_over_cap).expect("the link resumes");
        resumed.notify("still-here", Value::Null);
        wait_until("the agent to read on", || resumed.stderr_tail().contains("still-here"));
    }

    /// A shell that starts a grandchild sharing its stdout in a group of its own, prints once both run,
    /// and then either waits or exits; with its pid, start time and stdout.
    #[cfg(target_os = "linux")]
    fn group_with_grandchild(leader_exits: bool) -> (std::process::Child, u32, u64, std::process::ChildStdout) {
        use std::io::BufRead;
        use std::os::unix::process::CommandExt;
        let script = if leader_exits {
            "sleep 30 & echo started"
        } else {
            "sleep 30 & echo started; exec sleep 30"
        };
        let mut child = std::process::Command::new("sh")
            .args(["-c", script])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let start = crate::child::start_time(pid).unwrap();
        let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        out.read_line(&mut line).unwrap();
        assert_eq!(line, "started\n");
        (child, pid, start, out.into_inner())
    }

    /// Whether every holder of the pipe's write end is gone within a few seconds.
    #[cfg(target_os = "linux")]
    fn pipe_closed_soon(mut out: std::process::ChildStdout) -> bool {
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut rest = Vec::new();
            let _ = tx.send(std::io::Read::read_to_end(&mut out, &mut rest).is_ok());
        });
        rx.recv_timeout(Duration::from_secs(10)).unwrap_or(false)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_foreign_child_is_signalled_only_as_the_process_its_start_time_names() {
        let (mut child, pid, start, out) = group_with_grandchild(false);
        end_foreign_child(pid, start + 1, Instant::now());
        thread::sleep(Duration::from_millis(100));
        assert!(
            child.try_wait().unwrap().is_none(),
            "another start time names another process: nothing signalled"
        );
        end_foreign_child(pid, start, Instant::now());
        assert!(
            std::os::unix::process::ExitStatusExt::signal(&child.wait().unwrap()).is_some(),
            "the leader is killed"
        );
        assert!(pipe_closed_soon(out), "the grandchild in its group is killed too");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_foreign_childs_group_is_ended_after_the_leader_exited_on_its_own() {
        let (mut child, pid, start, out) = group_with_grandchild(true);
        end_foreign_child(pid, start, Instant::now() + Duration::from_secs(5));
        assert!(
            pipe_closed_soon(out),
            "the grandchild the exited leader left in its group is killed"
        );
        let _ = child.wait();
    }
}
