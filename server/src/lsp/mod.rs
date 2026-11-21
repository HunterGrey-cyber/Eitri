//! The daemon's language servers.
//!
//! Each `OpenServer` starts one language server in its root with the login environment, as a child of the daemon in
//! a session of its own; it lives until it exits, is stopped, the daemon stops, or no bridge has been attached for its
//! grace period. One bridge is attached at a time. Its messages go to the server's stdin in the order they came; the
//! server's go back to it in chunks.
//!
//! The helper reads only enough of each message to keep the link honest across a detach: which requests each side
//! has not answered yet, and what the editor answered to `workspace/configuration` and `workspace/workspaceFolders`.
//! When the bridge goes away, its unanswered requests are cancelled at the server (and their answers dropped), and the
//! server's unanswered requests are answered by the detached table (`detached`). While detached, the server's requests
//! are answered by that table and its notifications queue (`queue`) for the next attach, which gets `LspAttached`
//! first, then the queue in order, then live traffic.
//!
//! Every buffer is bounded: the queue (`LSP_DETACHED_QUEUE_MAX`, past it the server is stopped), the registrations
//! kept for the next attach, the remembered answers, the ids of unanswered requests (past `PENDING_MAX` the oldest is
//! let go, and an unanswered server request let go is answered by the table; a request with an id longer than
//! `message::ID_MAX` is answered with an error and not passed on) and the keys and params they keep, what a bridge
//! that has not spoken was handed, the writes a detach makes, the stderr tail, one header (8 KiB) and one message
//! (`LSP_MESSAGE_MAX`; a larger one is read past, and a request or response among them is answered with an error).
//!
//! On the way to the server's stdin each attach's messages pass its orderer (`save_hold`): a save the bridge sent
//! with the laptop file's hash waits until the server's file hashes the same (at most `SAVE_HOLD_MAX`), and the
//! messages for its document wait behind it. The saves a detach drops are listed in the next `LspAttached`.

pub mod detached;
pub mod message;
pub mod process;
pub mod queue;
pub mod read_path;
pub mod resolve;
pub mod save_hold;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::os::fd::OwnedFd;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime};

use eitri_server_proto::lsp_server_frame::Frame;
use eitri_server_proto::*;
use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use tonic::Status;

use self::message::{Kind, TopLevelScan};
use self::process::FromServer;
use self::queue::{Collapse, Queue, Registrations};
use super::sidecars::Sidecars;
use crate::run_dir::Paths;

/// The grace an open gets when it names none.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(1800);
/// Unanswered request ids remembered each way, and answers dropped late; past it the oldest is let go.
pub const PENDING_MAX: usize = 4096;
/// How long a replaced bridge has to take the frame that says so; the server's task does not wait for it.
const REPLACED_WAIT: Duration = Duration::from_secs(10);
/// An ended server's log is removed this long after its last write, or sooner when more than `LOGS_KEPT` ended
/// servers' logs are kept; a log written in the last `LOG_FRESH` is never removed (its server may be starting).
const LOG_RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);
pub const LOGS_KEPT: usize = 64;
const LOG_FRESH: Duration = Duration::from_secs(60);
/// How long a stopped server has between SIGTERM and SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(2);
/// How long a server whose output ended has to exit by itself.
const EXIT_WAIT: Duration = Duration::from_secs(1);
const REAP_WAIT: Duration = Duration::from_secs(2);
/// How long the stderr tail waits for the last of the server's stderr once it has exited.
const STDERR_WAIT: Duration = Duration::from_millis(500);
/// Frames queued toward a bridge (each at most `LSP_CHUNK_MAX`).
const FRAMES: usize = 8;

/// Why a server ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    Exited,
    Grace,
    QueueFull,
    Stopped,
    Daemon,
}

impl EndReason {
    fn wire(self) -> LspEndReason {
        match self {
            EndReason::Exited => LspEndReason::Exited,
            EndReason::Grace => LspEndReason::Grace,
            EndReason::QueueFull => LspEndReason::QueueFull,
            EndReason::Stopped => LspEndReason::Stopped,
            EndReason::Daemon => LspEndReason::Daemon,
        }
    }

    fn word(self) -> &'static str {
        match self {
            EndReason::Exited => "exited",
            EndReason::Grace => "grace",
            EndReason::QueueFull => "queue-full",
            EndReason::Stopped => "stopped",
            EndReason::Daemon => "daemon",
        }
    }
}

/// One whole message from the bridge, on its way to the server.
pub struct ClientMessage {
    pub bytes: Vec<u8>,
    /// The laptop file's SHA-256 on a `textDocument/didSave` (64 lowercase hex), when the bridge sent one: the save
    /// is held until the server's file hashes the same.
    pub saved_sha256: Option<String>,
}

/// Ids with a value each, in the order they were last inserted; past `PENDING_MAX` the oldest is evicted and handed
/// back to the caller, so a side that never answers cannot stop new ids from being kept.
struct Bounded<V> {
    next: u64,
    order: BTreeMap<u64, String>,
    entries: HashMap<String, (u64, V)>,
}

impl<V> Default for Bounded<V> {
    fn default() -> Self {
        Bounded {
            next: 0,
            order: BTreeMap::new(),
            entries: HashMap::new(),
        }
    }
}

impl<V> Bounded<V> {
    /// Keeps `id` as the newest; answers the entry evicted to make room, if any.
    fn insert(&mut self, id: String, value: V) -> Option<(String, V)> {
        if let Some((sequence, _)) = self.entries.remove(&id) {
            self.order.remove(&sequence);
        }
        let sequence = self.next;
        self.next += 1;
        self.order.insert(sequence, id.clone());
        self.entries.insert(id, (sequence, value));
        if self.entries.len() <= PENDING_MAX {
            return None;
        }
        let (_, oldest) = self.order.pop_first()?;
        let (_, value) = self.entries.remove(&oldest)?;
        Some((oldest, value))
    }

    fn get(&self, id: &str) -> Option<&V> {
        self.entries.get(id).map(|(_, value)| value)
    }

    fn remove(&mut self, id: &str) -> Option<V> {
        let (sequence, value) = self.entries.remove(id)?;
        self.order.remove(&sequence);
        Some(value)
    }

    /// Removes every entry `pick` chooses, oldest first.
    fn drain_where(&mut self, pick: impl Fn(&V) -> bool) -> Vec<(String, V)> {
        let picked: Vec<(u64, String)> = self
            .order
            .iter()
            .filter(|(_, id)| self.entries.get(*id).is_some_and(|(_, value)| pick(value)))
            .map(|(sequence, id)| (*sequence, id.clone()))
            .collect();
        picked
            .into_iter()
            .filter_map(|(sequence, id)| {
                self.order.remove(&sequence);
                let (_, value) = self.entries.remove(&id)?;
                Some((id, value))
            })
            .collect()
    }
}

/// Ids remembered in arrival order; the oldest is forgotten past `PENDING_MAX`.
#[derive(Default)]
struct Recent(Bounded<()>);

impl Recent {
    fn insert(&mut self, id: String) {
        self.0.insert(id, ());
    }

    fn remove(&mut self, id: &str) -> bool {
        self.0.remove(id).is_some()
    }
}

struct ServerRequest {
    /// The attach that was sent it.
    generation: u64,
    method: String,
    config_key: Option<String>,
    /// A registration's params, kept for the next attach if the helper answers it.
    registration: Option<Vec<u8>>,
}

/// Server requests a bridge was sent and has not answered, with the bytes their keys and params hold. Past
/// `LSP_REGISTRATIONS_PARAMS_MAX` of registration params a new registration keeps none; if it is still unanswered at
/// a detach, that detach stops the server (`QUEUE_FULL`) rather than keep it for the next attach without its params.
/// Past `CONFIG_BYTES_MAX` of configuration keys a new request keeps none and is answered as an unknown key is.
#[derive(Default)]
struct ServerPending {
    requests: Bounded<ServerRequest>,
    registration_bytes: usize,
    config_bytes: usize,
}

impl ServerPending {
    /// Keeps `request` as the newest; answers the request evicted to make room, if any.
    fn insert(&mut self, id: String, mut request: ServerRequest) -> Option<(String, ServerRequest)> {
        if let Some(replaced) = self.requests.remove(&id) {
            self.forget(&replaced);
        }
        if request
            .registration
            .as_ref()
            .is_some_and(|params| self.registration_bytes + params.len() > LSP_REGISTRATIONS_PARAMS_MAX)
        {
            request.registration = None;
        }
        if request
            .config_key
            .as_ref()
            .is_some_and(|key| self.config_bytes + key.len() > detached::CONFIG_BYTES_MAX)
        {
            request.config_key = None;
        }
        self.registration_bytes += request.registration.as_ref().map_or(0, Vec::len);
        self.config_bytes += request.config_key.as_ref().map_or(0, String::len);
        let (oldest, evicted) = self.requests.insert(id, request)?;
        self.forget(&evicted);
        Some((oldest, evicted))
    }

    fn remove(&mut self, id: &str) -> Option<ServerRequest> {
        let request = self.requests.remove(id)?;
        self.forget(&request);
        Some(request)
    }

    fn drain_where(&mut self, pick: impl Fn(&ServerRequest) -> bool) -> Vec<(String, ServerRequest)> {
        let drained = self.requests.drain_where(pick);
        for (_, request) in &drained {
            self.forget(request);
        }
        drained
    }

    fn forget(&mut self, request: &ServerRequest) {
        self.registration_bytes -= request.registration.as_ref().map_or(0, Vec::len);
        self.config_bytes -= request.config_key.as_ref().map_or(0, String::len);
    }
}

/// What settling a detach left for the actor: the registrations among the requests the table answered, how many it
/// answered, and the most queued for the server's stdin after one of its writes.
#[derive(Default)]
struct Settled {
    registrations: Vec<(String, Option<Vec<u8>>)>,
    answered: u64,
    queued: usize,
}

/// What both directions read and change, under one short lock.
#[derive(Default)]
struct Book {
    /// The attach whose messages are taken; a replaced or lost attach's late messages are dropped.
    generation: u64,
    /// The newest attach that has sent a message: it read the `LspAttached` it was handed.
    spoke: u64,
    /// Requests from a bridge the server has not answered: id, and the attach that sent it. Only the cancellation at
    /// a detach reads it, so an evicted id costs that cancellation and nothing else.
    bridge_pending: Bounded<u64>,
    /// Requests whose bridge went away and that were cancelled at the server: their answers are dropped.
    cancelled: Recent,
    /// Server requests a bridge was sent and has not answered.
    server_pending: ServerPending,
    /// Server requests the helper answered itself: a bridge's late answer to one is dropped.
    table_answered: Recent,
    remembered: detached::Remembered,
    /// Saves held for their file, by the attach holding them and the hold's number in it: the uri each names.
    held_saves: BTreeMap<(u64, u64), String>,
    /// The saves the detaches dropped while they were held, for the next attach's `LspAttached`.
    dropped_saves: save_hold::DroppedSaves,
}

impl Book {
    /// Records a save the attach `generation` holds for its file, as a message of that attach (it has spoken);
    /// `false` when that attach was replaced or lost, and its save is not held.
    fn hold_save(&mut self, generation: u64, number: u64, uri: &str) -> bool {
        if self.generation != generation {
            return false;
        }
        self.spoke = self.spoke.max(generation);
        self.held_saves.insert((generation, number), uri.to_owned());
        true
    }

    /// Whether `id` is a `workspace/applyEdit` the server sent the bridge and that is still unanswered.
    fn applies_edit(&self, id: &str) -> bool {
        self.server_pending
            .requests
            .get(id)
            .is_some_and(|request| request.method == save_hold::APPLY_EDIT)
    }

    fn bridge_request(&mut self, id: String, generation: u64) {
        self.cancelled.remove(&id);
        self.bridge_pending.insert(id, generation);
    }

    /// Records the editor's answer to a server request; `false` when the helper already answered it.
    fn bridge_response(&mut self, id: &str, bytes: &[u8]) -> bool {
        if self.table_answered.remove(id) {
            return false;
        }
        let Some(request) = self.server_pending.remove(id) else {
            return true;
        };
        match request.method.as_str() {
            detached::CONFIGURATION => {
                if let (Some(key), Some(result)) = (request.config_key, message::result(bytes)) {
                    self.remembered
                        .config
                        .insert(key, message::canonical(&result).into_bytes());
                }
            }
            detached::WORKSPACE_FOLDERS => {
                if let Some(result) = message::result(bytes) {
                    let text = message::canonical(&result);
                    if text.len() <= detached::FOLDERS_MAX {
                        self.remembered.folders = Some(text.into_bytes());
                    }
                }
            }
            _ => {}
        }
        true
    }

    /// Settles what the attaches up to `generation` left: the saves they still held are listed for the next attach;
    /// the bridge's requests are cancelled at the server and the server's answered by the table, handing each write
    /// to `send` (which answers how much is queued for the server's stdin after it). It stops at the first write past
    /// `process::WRITE_MAX`: the server is not reading and is stopped, and the rest would only pile up in memory.
    fn settle(&mut self, generation: u64, mut send: impl FnMut(Vec<u8>) -> usize) -> Settled {
        let mut settled = Settled::default();
        if self.generation == generation {
            self.generation += 1;
        }
        let later = self.held_saves.split_off(&(generation + 1, 0));
        for (_, uri) in std::mem::replace(&mut self.held_saves, later) {
            self.dropped_saves.add(uri);
        }
        for (id, _) in self.bridge_pending.drain_where(|sent_by| *sent_by <= generation) {
            settled.queued = settled.queued.max(send(message::cancel(&id)));
            self.cancelled.insert(id);
            if settled.queued > process::WRITE_MAX {
                return settled;
            }
        }
        for (id, request) in self
            .server_pending
            .drain_where(|request| request.generation <= generation)
        {
            let answer = detached::answer(
                &id,
                &request.method,
                request.config_key.as_deref(),
                &self.remembered,
                detached::NOT_CONNECTED,
            );
            settled.queued = settled.queued.max(send(answer));
            if detached::is_registration(&request.method) {
                settled.registrations.push((request.method, request.registration));
            }
            self.table_answered.insert(id);
            settled.answered += 1;
            if settled.queued > process::WRITE_MAX {
                return settled;
            }
        }
        settled
    }
}

#[derive(Clone, Default)]
struct Info {
    attached: bool,
    detached_since_unix_ms: u64,
    queued_bytes: u64,
}

struct Handle {
    id: String,
    root: String,
    /// The root as the open named it and as it resolves: the saves held are of paths under either.
    roots: Vec<PathBuf>,
    command: String,
    pid: u32,
    /// The bridge that opened or last attached it.
    client_id: Mutex<String>,
    book: Mutex<Book>,
    writer: process::Writer,
    log: Arc<process::Log>,
    commands: mpsc::Sender<Command>,
    /// The newest attach asked for: a send to an older bridge gives way to it.
    latest: watch::Sender<u64>,
    stop: watch::Sender<Option<EndReason>>,
    done: watch::Receiver<bool>,
    info: Mutex<Info>,
    /// The looks at held saves' files running for this server, over all its attaches.
    looks: Arc<save_hold::Looks>,
}

impl Handle {
    fn info(&self) -> Info {
        self.info.lock().expect("server info").clone()
    }

    /// Hands one of the bridge's messages to the server's stdin, in the order the attach's orderer releases them. It
    /// waits for room first, then decides under the book's lock, where every attach and detach changes the generation,
    /// whether it still goes: a replaced or lost attach's message never lands after the next attach's first one, or
    /// after the cancellations and answers its own detach wrote, and a held save that goes is no longer listed as
    /// dropped. `false` when the attach was replaced or lost.
    async fn to_server(&self, generation: u64, pending: save_hold::Pending) -> bool {
        self.writer.room().await;
        let mut book = self.book.lock().expect("lsp book");
        if book.generation != generation {
            // This attach was replaced or lost: its late messages are not the editor's any more.
            return false;
        }
        book.spoke = book.spoke.max(generation);
        if let Some(number) = pending.hold {
            book.held_saves.remove(&(generation, number));
        }
        let forward = match pending.kind {
            Some(Kind::Request { id, .. }) => {
                book.bridge_request(id, generation);
                true
            }
            Some(Kind::Response { id }) => book.bridge_response(&id, &pending.message.bytes),
            _ => true,
        };
        if forward {
            self.writer.send_now(pending.message.bytes);
        }
        true
    }

    /// Asks the server to end for `reason` (the first reason asked wins) and waits until it has.
    async fn end(&self, reason: EndReason) {
        self.stop.send_if_modified(|asked| {
            let first = asked.is_none();
            if first {
                *asked = Some(reason);
            }
            first
        });
        let mut done = self.done.clone();
        let _ = done.wait_for(|done| *done).await;
    }
}

#[derive(Default)]
struct Registry {
    servers: BTreeMap<String, Arc<Handle>>,
    /// The answer to each open of a live server, by `command_id`: a retried open is the same server.
    opened: HashMap<String, OpenServerResponse>,
}

/// The daemon's language servers.
pub struct Servers {
    paths: Paths,
    sidecars: Arc<Sidecars>,
    registry: Arc<Mutex<Registry>>,
    starts: Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
    stopping: AtomicBool,
    min_grace: Duration,
    save_hold: save_hold::Timing,
}

/// An attach: the frames for the bridge, and where its messages go (`None` when the server is gone: the frames are
/// one `LspEnded` with `GONE`). Dropping the sink detaches the bridge.
pub struct Attachment {
    pub frames: mpsc::Receiver<Result<LspServerFrame, Status>>,
    pub sink: Option<Sink>,
}

fn refused(refusal: String) -> OpenServerResponse {
    OpenServerResponse {
        refusal,
        ..Default::default()
    }
}

fn ended_frame(ended: LspEnded) -> LspServerFrame {
    LspServerFrame {
        frame: Some(Frame::Ended(ended)),
    }
}

fn gone_frame() -> LspServerFrame {
    ended_frame(LspEnded {
        reason: LspEndReason::Gone as i32,
        ..Default::default()
    })
}

/// A bridge's id: 1..128 safe ASCII bytes, as command ids are.
fn check_client_id(value: &str) -> Result<(), Status> {
    if value.is_empty()
        || value.len() > 128
        || !value.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
    {
        return Err(Status::invalid_argument(
            "invalid client_id: expected 1..128 safe ASCII bytes",
        ));
    }
    Ok(())
}

/// The argv and root bounds of an open; the text says what was wrong.
fn check_open(request: &OpenServerRequest) -> Result<(), String> {
    let Some(argv0) = request.argv.first() else {
        return Err("no language server command was given".to_owned());
    };
    if argv0.is_empty() {
        return Err("the language server command is empty".to_owned());
    }
    if request.argv.len() > LSP_ARGV_MAX {
        return Err(format!(
            "the language server command has more than {LSP_ARGV_MAX} arguments"
        ));
    }
    if request
        .argv
        .iter()
        .any(|arg| arg.len() > LSP_ARGV_ENTRY_MAX || arg.contains('\0'))
    {
        return Err(format!(
            "an argument of the language server command is longer than {LSP_ARGV_ENTRY_MAX} bytes or holds a NUL"
        ));
    }
    if request.root.len() > LSP_PATH_MAX {
        return Err(format!("the root is longer than {LSP_PATH_MAX} bytes"));
    }
    Ok(())
}

async fn off_runtime<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Result<T, Status> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| Status::internal(error.to_string()))
}

impl Servers {
    pub fn new(paths: Paths, sidecars: Arc<Sidecars>) -> Servers {
        Servers {
            paths,
            sidecars,
            registry: Arc::default(),
            starts: Mutex::default(),
            stopping: AtomicBool::new(false),
            min_grace: Duration::from_millis(LSP_GRACE_MIN_MS),
            save_hold: save_hold::Timing::default(),
        }
    }

    /// Sets how long a save waits for its file, so tests can tell a release by the file from one at the deadline.
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn with_save_hold(mut self, hold: Duration) -> Servers {
        self.save_hold.hold = hold;
        self
    }

    /// Makes every look at a held save's file take `delay` first, so tests can tell that a slow look holds up
    /// nothing else.
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn with_look_delay(mut self, delay: Duration) -> Servers {
        self.save_hold.look_delay = delay;
        self
    }

    /// How many looks at held saves' files the server `id` has running.
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn looks_running(&self, id: &str) -> usize {
        let registry = self.registry.lock().expect("lsp registry");
        registry.servers.get(id).map_or(0, |handle| handle.looks.running())
    }

    /// Lowers the shortest grace an open may ask for, so tests need not wait a minute.
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn with_min_grace(mut self, min: Duration) -> Servers {
        self.min_grace = min;
        self
    }

    fn grace_of(&self, asked_ms: u64) -> Duration {
        if asked_ms == 0 {
            return DEFAULT_GRACE;
        }
        Duration::from_millis(asked_ms).clamp(self.min_grace, Duration::from_millis(LSP_GRACE_MAX_MS))
    }

    /// Where the servers' logs are: `<run dir>/lsp/<server id>.log`.
    pub fn log_dir(&self) -> PathBuf {
        self.paths.metadata_run().join("lsp")
    }

    fn start_lock(&self, command_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut starts = self.starts.lock().expect("lsp starts");
        starts.retain(|_, weak| weak.strong_count() != 0);
        let serial = starts
            .get(command_id)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| Arc::new(tokio::sync::Mutex::new(())));
        starts.insert(command_id.to_owned(), Arc::downgrade(&serial));
        serial
    }

    /// Starts a language server, or answers the server an earlier open with the same `command_id` started.
    pub async fn open(&self, request: OpenServerRequest) -> Result<OpenServerResponse, Status> {
        if self.stopping.load(Ordering::SeqCst) {
            return Err(Status::unavailable("daemon is stopping"));
        }
        super::sidecars::validate_command_id(&request.command_id)?;
        check_client_id(&request.client_id)?;
        if let Err(refusal) = check_open(&request) {
            return Ok(refused(refusal));
        }
        let serial = self.start_lock(&request.command_id);
        let _serial = serial.lock().await;
        {
            let registry = self.registry.lock().expect("lsp registry");
            if let Some(answer) = registry.opened.get(&request.command_id) {
                return Ok(answer.clone());
            }
            let attached = registry.servers.values().find(|handle| {
                *handle.client_id.lock().expect("client id") == request.client_id && handle.info().attached
            });
            if let Some(handle) = attached {
                return Ok(refused(format!(
                    "already open: this editor is attached to language server {}",
                    handle.id
                )));
            }
        }
        let captured = match self.sidecars.login_environment().await {
            Ok(captured) => captured,
            Err(status) if self.stopping.load(Ordering::SeqCst) => return Err(status),
            Err(status) => {
                return Ok(refused(format!(
                    "the login environment is unavailable: {}",
                    status.message()
                )))
            }
        };
        let root_text = request.root.clone();
        let opened = off_runtime(move || {
            let root = super::sidecars::open_root(&root_text)?;
            let canonical = super::sidecars::canonical_root(&root)?;
            Ok::<_, Status>((root, canonical))
        })
        .await?;
        let (root, canonical_root) = match opened {
            Ok(opened) => opened,
            Err(status) => return Ok(refused(status.message().to_owned())),
        };
        if canonical_root.len() > LSP_PATH_MAX {
            return Ok(refused(format!("the root is longer than {LSP_PATH_MAX} bytes")));
        }
        let env = captured.env.clone();
        let argv0 = request.argv[0].clone();
        let resolved = off_runtime(move || {
            let path: Vec<PathBuf> = env
                .get(OsStr::new("PATH"))
                .map(|path| std::env::split_paths(path).collect())
                .unwrap_or_default();
            let home = env.get(OsStr::new("HOME")).map(PathBuf::from).unwrap_or_default();
            let data_home = env.get(OsStr::new("XDG_DATA_HOME")).map(PathBuf::from);
            resolve::command(&argv0, &path, &home, data_home.as_deref())
        })
        .await?;
        let program = match resolved {
            Ok(program) => program,
            Err(missing) => {
                return Ok(OpenServerResponse {
                    missing_command: missing.name,
                    ..Default::default()
                })
            }
        };
        let command = match program.to_str() {
            Some(command) if command.len() <= LSP_PATH_MAX => command.to_owned(),
            _ => {
                return Ok(refused(format!(
                    "the resolved command {:?} is not UTF-8 or longer than {LSP_PATH_MAX} bytes",
                    program
                )))
            }
        };
        let id = super::terminals::new_instance_id().map_err(|error| Status::internal(error.to_string()))?;
        let log = match process::Log::open(&self.paths.state, &self.log_dir(), &id, LSP_STDERR_TAIL) {
            Ok(log) => log,
            Err(error) => return Ok(refused(format!("the language server's log could not be made: {error}"))),
        };
        let started = match process::start(
            &program,
            &request.argv[1..],
            &captured.env,
            &root,
            &canonical_root,
            log.clone(),
        ) {
            Ok(started) => started,
            Err(error) => return Ok(refused(format!("{command} could not start: {error}"))),
        };
        drop(root);
        self.launch(request, id, command, canonical_root, started, log)
    }

    /// Registers a started server and its actor.
    fn launch(
        &self,
        request: OpenServerRequest,
        id: String,
        command: String,
        root: String,
        started: process::Started,
        log: Arc<process::Log>,
    ) -> Result<OpenServerResponse, Status> {
        let process::Started {
            child,
            pid,
            pidfd,
            stdin,
            stdout,
            stderr_done,
        } = started;
        let fail = |child: tokio::process::Child, why: String| {
            process::signal_group(pid, libc::SIGKILL);
            drop(child);
            why
        };
        let pidfd = match AsyncFd::with_interest(pidfd, tokio::io::Interest::READABLE) {
            Ok(pidfd) => pidfd,
            Err(error) => return Err(Status::internal(fail(child, error.to_string()))),
        };
        let (commands, command_receiver) = mpsc::channel(4);
        let (latest, latest_receiver) = watch::channel(0);
        let (stop, stop_receiver) = watch::channel(None);
        let (done_sender, done) = watch::channel(false);
        let (messages, message_receiver) = mpsc::channel(1);
        let opened_at = crate::run_dir::unix_ms();
        let mut roots = vec![PathBuf::from(&request.root)];
        if request.root != root {
            roots.push(PathBuf::from(&root));
        }
        let handle = Arc::new(Handle {
            id: id.clone(),
            root: root.clone(),
            roots,
            command: command.clone(),
            pid,
            client_id: Mutex::new(request.client_id.clone()),
            book: Mutex::default(),
            writer: process::Writer::start(stdin),
            log: log.clone(),
            commands,
            latest,
            stop,
            done,
            info: Mutex::new(Info {
                attached: false,
                detached_since_unix_ms: opened_at,
                queued_bytes: 0,
            }),
            looks: Arc::default(),
        });
        let answer = OpenServerResponse {
            server_id: id.clone(),
            resolved_command: command.clone(),
            ..Default::default()
        };
        {
            // Stopping is checked under the registry lock that `stop_all` takes, so no server is left behind it.
            let mut registry = self.registry.lock().expect("lsp registry");
            if self.stopping.load(Ordering::SeqCst) {
                drop(registry);
                fail(child, String::new());
                return Err(Status::unavailable("daemon is stopping"));
            }
            registry.servers.insert(id.clone(), handle.clone());
            registry.opened.insert(request.command_id.clone(), answer.clone());
        }
        tokio::spawn(process::read_stdout(stdout, messages, log.clone()));
        let actor = Actor {
            handle,
            registry: self.registry.clone(),
            state: self.paths.state.clone(),
            child,
            pidfd,
            stdout: message_receiver,
            commands: command_receiver,
            latest: latest_receiver,
            stop: stop_receiver,
            done: done_sender,
            stderr_done: Some(stderr_done),
            grace: self.grace_of(request.grace_ms),
            attached: None,
            detached_at: Some(Instant::now()),
            detached_since_unix_ms: opened_at,
            leader_exited: None,
            reattached: false,
            queue: Queue::default(),
            registrations: Registrations::default(),
            answered: 0,
            handed: None,
        };
        let basename = Path::new(&command)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let _ = crate::audit::record(
            &self.paths.state,
            "lsp-start",
            &format!(
                "{} root={} id={id} pid={pid}",
                crate::daemon::sync::audit_word(&basename),
                crate::daemon::sync::audit_word(&root)
            ),
        );
        log.line(&format!("started {command} in {root} (pid {pid})"));
        tokio::spawn(actor.run());
        Ok(answer)
    }

    /// Attaches a bridge to `server_id`, replacing the one attached.
    pub async fn attach(&self, server_id: &str, client_id: &str) -> Attachment {
        let (frames_sender, frames) = mpsc::channel(FRAMES);
        let handle = self
            .registry
            .lock()
            .expect("lsp registry")
            .servers
            .get(server_id)
            .cloned();
        let Some(handle) = handle else {
            let _ = frames_sender.try_send(Ok(gone_frame()));
            return Attachment { frames, sink: None };
        };
        let generation = {
            let mut book = handle.book.lock().expect("lsp book");
            book.generation += 1;
            // Under the book's lock, so the newest attach is never overwritten by an older one.
            handle.latest.send_replace(book.generation);
            book.generation
        };
        *handle.client_id.lock().expect("client id") = client_id.to_owned();
        let (closed_sender, closed) = oneshot::channel();
        let bridge = Bridge {
            generation,
            frames: frames_sender,
            closed,
        };
        match handle.commands.send(Command::Attach(bridge)).await {
            Ok(()) => Attachment {
                frames,
                sink: Some(Sink {
                    orderer: save_hold::Orderer::spawn(
                        handle.clone(),
                        generation,
                        self.paths.state.clone(),
                        self.save_hold,
                    ),
                    handle,
                    generation,
                    partial: Vec::new(),
                    oversize: None,
                    _closed: closed_sender,
                }),
            },
            Err(error) => {
                // The server ended between the lookup and the attach.
                if let Command::Attach(bridge) = error.0 {
                    let _ = bridge.frames.try_send(Ok(gone_frame()));
                }
                Attachment { frames, sink: None }
            }
        }
    }

    pub fn list(&self) -> ListServersResponse {
        let registry = self.registry.lock().expect("lsp registry");
        let servers = registry
            .servers
            .values()
            .take(LSP_LIST_MAX)
            .map(|handle| {
                let info = handle.info();
                LspServerInfo {
                    server_id: handle.id.clone(),
                    root: handle.root.clone(),
                    command: handle.command.clone(),
                    pid: handle.pid,
                    attached: info.attached,
                    detached_since_unix_ms: if info.attached { 0 } else { info.detached_since_unix_ms },
                    queued_bytes: info.queued_bytes,
                }
            })
            .collect();
        ListServersResponse {
            servers,
            truncated: registry.servers.len() > LSP_LIST_MAX,
        }
    }

    /// Stops a server and waits until it has ended; an unknown id is already stopped.
    pub async fn stop(&self, server_id: &str) {
        let handle = self
            .registry
            .lock()
            .expect("lsp registry")
            .servers
            .get(server_id)
            .cloned();
        if let Some(handle) = handle {
            handle.end(EndReason::Stopped).await;
        }
    }

    /// Stops every server for the daemon's stop, together, and refuses opens from now on.
    pub async fn stop_all(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        let handles: Vec<_> = self
            .registry
            .lock()
            .expect("lsp registry")
            .servers
            .values()
            .cloned()
            .collect();
        futures_util::future::join_all(handles.iter().map(|handle| handle.end(EndReason::Daemon))).await;
    }

    /// Whether any server lives: the daemon is not idle while one does.
    pub fn any_live(&self) -> bool {
        !self.registry.lock().expect("lsp registry").servers.is_empty()
    }

    /// Removes the logs of ended servers past their retention: each editor start leaves one, of up to
    /// `process::LOG_CAP`. Blocking: run it off the runtime.
    pub fn trim_logs(&self) {
        let live: HashSet<String> = self
            .registry
            .lock()
            .expect("lsp registry")
            .servers
            .keys()
            .cloned()
            .collect();
        let Ok(entries) = std::fs::read_dir(self.log_dir()) else {
            return;
        };
        let mut ended: Vec<(SystemTime, PathBuf)> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let id = name.to_str()?.strip_suffix(".log")?;
                if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()) || live.contains(id) {
                    return None;
                }
                // Not followed: only the logs themselves are removed.
                let metadata = entry.metadata().ok().filter(std::fs::Metadata::is_file)?;
                Some((metadata.modified().ok()?, entry.path()))
            })
            .collect();
        ended.sort_by(|a, b| b.0.cmp(&a.0));
        let now = SystemTime::now();
        for (at, (modified, path)) in ended.into_iter().enumerate() {
            let age = now.duration_since(modified).unwrap_or_default();
            if age >= LOG_FRESH && (at >= LOGS_KEPT || age >= LOG_RETENTION) {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// Where an attached bridge's messages go. Messages are reassembled from chunks (up to `LSP_MESSAGE_MAX`; a larger
/// one is read past and, when it is a request, answered with an error) and pass the attach's orderer, which writes
/// them to the server's stdin in order around the saves it holds.
pub struct Sink {
    handle: Arc<Handle>,
    generation: u64,
    partial: Vec<u8>,
    oversize: Option<TopLevelScan>,
    orderer: save_hold::Inbox,
    /// Dropped with the sink, which tells the server's actor the bridge went away.
    _closed: oneshot::Sender<()>,
}

fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Sink {
    pub async fn chunk(&mut self, chunk: LspChunk) {
        if let Some(scan) = self.oversize.as_mut() {
            scan.feed(&chunk.data);
        } else if self.partial.len() + chunk.data.len() > LSP_MESSAGE_MAX {
            let mut scan = TopLevelScan::default();
            scan.feed(&self.partial);
            scan.feed(&chunk.data);
            self.partial = Vec::new();
            self.oversize = Some(scan);
        } else {
            self.partial.extend_from_slice(&chunk.data);
        }
        if !chunk.last {
            return;
        }
        if let Some(scan) = self.oversize.take() {
            return self.too_large(scan).await;
        }
        let bytes = std::mem::take(&mut self.partial);
        let saved_sha256 = Some(chunk.saved_sha256).filter(|hash| is_sha256(hash));
        self.deliver(ClientMessage { bytes, saved_sha256 }).await;
    }

    async fn too_large(&mut self, scan: TopLevelScan) {
        let why = format!("the editor's message was larger than {LSP_MESSAGE_MAX} bytes");
        self.handle
            .log
            .line(&format!("dropped a message from the bridge: {why}"));
        match scan.kind() {
            Some(Kind::Request { id, .. }) => self.fail_request(&id, &why).await,
            // The editor's answer to a server request is replaced by an error for it, as any answer is passed on.
            Some(Kind::Response { id }) => {
                let answer = ClientMessage {
                    bytes: message::error_response(&id, detached::REQUEST_FAILED, &why),
                    saved_sha256: None,
                };
                self.to_server(Some(Kind::Response { id }), answer).await;
            }
            _ => {
                self.take_part();
            }
        }
    }

    /// Whether this attach's messages are still taken; marks it as one that has spoken.
    fn take_part(&self) -> bool {
        let mut book = self.handle.book.lock().expect("lsp book");
        if book.generation != self.generation {
            return false;
        }
        book.spoke = book.spoke.max(self.generation);
        true
    }

    /// Answers one of the editor's requests with `RequestFailed` instead of passing it to the server: the helper
    /// answers it itself, through the actor. The lock is not held across the wait.
    async fn fail_request(&mut self, id: &str, why: &str) {
        if !self.take_part() {
            return;
        }
        let reply = message::error_response(id, detached::REQUEST_FAILED, why);
        let _ = self
            .handle
            .commands
            .send(Command::Reply {
                generation: self.generation,
                bytes: reply,
            })
            .await;
    }

    async fn deliver(&mut self, message: ClientMessage) {
        let kind = message::classify(&message.bytes);
        if let Some(Kind::Request { id, .. }) = &kind {
            if id.len() > message::ID_MAX {
                let why = format!("the request's id is longer than {} bytes", message::ID_MAX);
                self.handle
                    .log
                    .line(&format!("dropped a request from the bridge: {why}"));
                return self.fail_request(id, &why).await;
            }
        }
        self.to_server(kind, message).await;
    }

    /// Hands one of the bridge's messages to the attach's orderer and waits until it has gone on or is held, so the
    /// next is taken only then and the messages keep the order the bridge sent them in.
    async fn to_server(&mut self, kind: Option<Kind>, message: ClientMessage) {
        let (taken, wait) = oneshot::channel();
        let pending = save_hold::Pending::new(message, kind);
        if self.orderer.send((pending, taken)).await.is_ok() {
            let _ = wait.await;
        }
    }
}

struct Bridge {
    generation: u64,
    frames: mpsc::Sender<Result<LspServerFrame, Status>>,
    /// Resolves when the bridge's sink is dropped.
    closed: oneshot::Receiver<()>,
}

enum Command {
    Attach(Bridge),
    /// A message for the bridge of this attach (the helper's answer to a request it could not carry).
    Reply {
        generation: u64,
        bytes: Vec<u8>,
    },
}

enum Event {
    Stop,
    Exited,
    Drained,
    BridgeClosed,
    Command(Option<Command>),
    Server(Option<FromServer>),
    Grace,
}

/// What an attach was handed: given back to the next attach when this one goes without having sent a message, since
/// it may never have read its `LspAttached` or the queue after it.
struct Handed {
    generation: u64,
    /// `reattached` as it was before this attach.
    reattached: bool,
    registrations: Vec<LspRegistration>,
    answered: u64,
    dropped: u64,
    /// The notifications it was handed whole, queued and live, in order, and their bytes.
    replay: Vec<Vec<u8>>,
    replay_bytes: usize,
    /// Notifications it was handed that are no longer kept: past `LSP_DETACHED_QUEUE_MAX` of them the replay is let
    /// go. They count as dropped only if the bridge goes without having spoken.
    replay_dropped: u64,
    dropped_saves: Vec<String>,
}

/// Tells a replaced bridge so, without making the server's task wait for a bridge that is not reading.
fn tell_replaced(frames: mpsc::Sender<Result<LspServerFrame, Status>>) {
    let frame = ended_frame(LspEnded {
        reason: LspEndReason::Replaced as i32,
        ..Default::default()
    });
    tokio::spawn(async move {
        let _ = tokio::time::timeout(REPLACED_WAIT, frames.send(Ok(frame))).await;
    });
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// The one task that owns a server's process, its output toward the bridge, and its detached state.
struct Actor {
    handle: Arc<Handle>,
    registry: Arc<Mutex<Registry>>,
    state: PathBuf,
    child: tokio::process::Child,
    pidfd: AsyncFd<OwnedFd>,
    stdout: mpsc::Receiver<FromServer>,
    commands: mpsc::Receiver<Command>,
    latest: watch::Receiver<u64>,
    stop: watch::Receiver<Option<EndReason>>,
    done: watch::Sender<bool>,
    stderr_done: Option<oneshot::Receiver<()>>,
    grace: Duration,
    attached: Option<Bridge>,
    /// When the last bridge went away (or the server opened), while none is attached.
    detached_at: Option<Instant>,
    detached_since_unix_ms: u64,
    /// When the server's own process exited: its output is still read for a moment, then it ends.
    leader_exited: Option<Instant>,
    /// A bridge has been attached before.
    reattached: bool,
    queue: Queue,
    registrations: Registrations,
    /// Server requests answered by the table since the last attach.
    answered: u64,
    /// What the attached bridge was handed, until it sends a message.
    handed: Option<Handed>,
}

impl Actor {
    async fn run(mut self) {
        let reason = match self.serve().await {
            Ok(()) => EndReason::Exited,
            Err(reason) => reason,
        };
        self.end(reason).await;
    }

    async fn serve(&mut self) -> Result<(), EndReason> {
        loop {
            if let Some(reason) = *self.stop.borrow_and_update() {
                return Err(reason);
            }
            if self.handed.as_ref().is_some_and(|handed| self.spoke(handed.generation)) {
                self.handed = None;
            }
            let deadline = self.detached_at.map(|at| at + self.grace);
            let drained = self.leader_exited.map(|at| at + EXIT_WAIT);
            let watch_exit = self.leader_exited.is_none();
            let event = {
                let (stop, pidfd, commands, stdout) =
                    (&mut self.stop, &self.pidfd, &mut self.commands, &mut self.stdout);
                let attached = &mut self.attached;
                let closed = async move {
                    match attached.as_mut() {
                        Some(bridge) => {
                            let _ = (&mut bridge.closed).await;
                        }
                        None => std::future::pending().await,
                    }
                };
                let grace = sleep_until(deadline);
                let drained = sleep_until(drained);
                tokio::select! {
                    _ = stop.changed() => Event::Stop,
                    _ = pidfd.readable(), if watch_exit => Event::Exited,
                    _ = drained => Event::Drained,
                    _ = closed => Event::BridgeClosed,
                    command = commands.recv() => Event::Command(command),
                    message = stdout.recv() => Event::Server(message),
                    _ = grace => Event::Grace,
                }
            };
            match event {
                Event::Stop => {}
                // The rest of its output is still delivered: the server ends when its output does, or a moment after.
                Event::Exited => self.leader_exited = Some(Instant::now()),
                Event::Drained => return Err(EndReason::Exited),
                Event::BridgeClosed => self.detach()?,
                Event::Command(Some(Command::Attach(bridge))) => self.attach(bridge).await?,
                Event::Command(Some(Command::Reply { generation, bytes })) => {
                    if self
                        .attached
                        .as_ref()
                        .is_some_and(|bridge| bridge.generation == generation)
                    {
                        self.to_bridge(&bytes).await?;
                    }
                }
                // The handle keeps a sender for as long as this task runs.
                Event::Command(None) => return Err(EndReason::Daemon),
                Event::Server(Some(message)) => self.from_server(message).await?,
                Event::Server(None) => return Err(EndReason::Exited),
                Event::Grace => return Err(EndReason::Grace),
            }
        }
    }

    fn publish(&self) {
        *self.handle.info.lock().expect("server info") = Info {
            attached: self.attached.is_some(),
            detached_since_unix_ms: self.detached_since_unix_ms,
            queued_bytes: self.queue.bytes() as u64,
        };
    }

    fn stop_reason(&self) -> EndReason {
        self.stop.borrow().unwrap_or(EndReason::Daemon)
    }

    /// Sends a frame to the attached bridge; a bridge that is gone is detached. A send that waits for a slow bridge
    /// ends at a stop, or when a newer attach is asked for: that bridge is replaced, so it is detached and told so,
    /// and the newer attach is not held up behind it.
    async fn send_frame(&mut self, frame: Frame) -> Result<(), EndReason> {
        let Some((frames, generation)) = self
            .attached
            .as_ref()
            .map(|bridge| (bridge.frames.clone(), bridge.generation))
        else {
            return Ok(());
        };
        // Checked in this order, so a frame never goes to a bridge already replaced or to a server already stopping
        // when the send could also go ahead.
        let sent = tokio::select! {
            biased;
            _ = self.stop.wait_for(Option::is_some) => None,
            _ = self.latest.wait_for(|latest| *latest > generation) => {
                tell_replaced(frames);
                Some(false)
            }
            sent = frames.send(Ok(LspServerFrame { frame: Some(frame) })) => Some(sent.is_ok()),
        };
        match sent {
            Some(true) => Ok(()),
            Some(false) => self.detach(),
            None => Err(self.stop_reason()),
        }
    }

    /// One message to the bridge, in chunks of at most `LSP_CHUNK_MAX`; `true` when the bridge attached at the call
    /// took every chunk (`false` when none was attached, or it went or was replaced on the way).
    async fn to_bridge(&mut self, bytes: &[u8]) -> Result<bool, EndReason> {
        let Some(generation) = self.attached.as_ref().map(|bridge| bridge.generation) else {
            return Ok(false);
        };
        let count = bytes.len().div_ceil(LSP_CHUNK_MAX).max(1);
        for at in 0..count {
            let data = bytes[at * LSP_CHUNK_MAX..((at + 1) * LSP_CHUNK_MAX).min(bytes.len())].to_vec();
            self.send_frame(Frame::Chunk(LspChunk {
                data,
                last: at + 1 == count,
                saved_sha256: String::new(),
            }))
            .await?;
            if self.attached.as_ref().map(|bridge| bridge.generation) != Some(generation) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Keeps a notification the bridge of `generation` took whole while it has not spoken, so a bridge that goes
    /// without speaking gives back the news it was sent live too, after its replay. Past `LSP_DETACHED_QUEUE_MAX` the
    /// notifications kept so far are let go and only counted; the registrations and counters it was handed are kept
    /// whatever it was sent, since the frames it took may still sit unread in the buffers between it and the helper.
    fn hand(&mut self, generation: u64, bytes: Vec<u8>) {
        let Some(handed) = self.handed.as_mut().filter(|handed| handed.generation == generation) else {
            return;
        };
        handed.replay_bytes += bytes.len();
        handed.replay.push(bytes);
        if handed.replay_bytes > LSP_DETACHED_QUEUE_MAX {
            handed.replay_dropped += handed.replay.len() as u64;
            handed.replay.clear();
            handed.replay_bytes = 0;
        }
    }

    fn write_now(&self, bytes: Vec<u8>) -> Result<(), EndReason> {
        self.written(self.handle.writer.send_now(bytes))
    }

    /// Stops a server that is not reading its input, given how much was queued for it after a write.
    fn written(&self, queued: usize) -> Result<(), EndReason> {
        if queued > process::WRITE_MAX {
            self.handle
                .log
                .line("the language server stopped reading its input; it is stopped");
            return Err(EndReason::QueueFull);
        }
        Ok(())
    }

    /// Whether the attach `generation` has sent a message.
    fn spoke(&self, generation: u64) -> bool {
        self.handle.book.lock().expect("lsp book").spoke >= generation
    }

    /// The bridge went away: its unanswered requests are cancelled at the server, the server's unanswered requests
    /// are answered by the table, and the grace period starts.
    fn detach(&mut self) -> Result<(), EndReason> {
        let Some(bridge) = self.attached.take() else {
            return Ok(());
        };
        self.detached_at = Some(Instant::now());
        self.detached_since_unix_ms = crate::run_dir::unix_ms();
        self.give_back(bridge.generation)?;
        self.publish();
        self.release(bridge.generation)
    }

    /// An attach that goes without having sent a message may not have read what it was handed: the registrations,
    /// counters and replayed notifications go back for the next attach, ahead of anything newer.
    fn give_back(&mut self, generation: u64) -> Result<(), EndReason> {
        let Some(handed) = self.handed.take() else {
            return Ok(());
        };
        if handed.generation != generation || self.spoke(generation) {
            return Ok(());
        }
        self.reattached = handed.reattached;
        self.answered += handed.answered;
        self.queue.add_dropped(handed.dropped + handed.replay_dropped);
        self.handle
            .book
            .lock()
            .expect("lsp book")
            .dropped_saves
            .give_back(handed.dropped_saves);
        self.requeue(handed.replay);
        for registration in handed.registrations {
            self.keep_registration(&registration.method, Some(registration.params))?;
        }
        Ok(())
    }

    /// Puts notifications back in the queue, in order, as if they had just arrived.
    fn requeue(&mut self, messages: impl IntoIterator<Item = Vec<u8>>) {
        for bytes in messages {
            let collapse = match message::classify(&bytes) {
                Some(Kind::Notification { method }) => Collapse::of(&method, &bytes),
                _ => Collapse::Plain,
            };
            self.queue.push(bytes, collapse);
        }
        self.publish();
    }

    /// Settles what the attaches up to `generation` left unanswered. The writes are made under the book's lock, so
    /// none lands after a message of the next attach (which passes the same lock).
    fn release(&mut self, generation: u64) -> Result<(), EndReason> {
        let settled = {
            let writer = &self.handle.writer;
            let mut book = self.handle.book.lock().expect("lsp book");
            book.settle(generation, |bytes| writer.send_now(bytes))
        };
        self.answered += settled.answered;
        self.written(settled.queued)?;
        for (method, params) in settled.registrations {
            self.keep_registration(&method, params)?;
        }
        Ok(())
    }

    fn keep_registration(&mut self, method: &str, params: Option<Vec<u8>>) -> Result<(), EndReason> {
        let accepted = match params {
            Some(params) => self.registrations.push(method, params).is_ok(),
            None => false,
        };
        if !accepted {
            self.handle
                .log
                .line("the registrations kept for the editor passed their bound; the language server is stopped");
            return Err(EndReason::QueueFull);
        }
        Ok(())
    }

    async fn attach(&mut self, bridge: Bridge) -> Result<(), EndReason> {
        if bridge.generation < self.handle.book.lock().expect("lsp book").generation {
            // A newer attach was asked for before this one was taken: this one is already replaced.
            tell_replaced(bridge.frames);
            return Ok(());
        }
        if let Some(old) = self.attached.take() {
            tell_replaced(old.frames);
            self.give_back(old.generation)?;
            self.release(old.generation)?;
        }
        let generation = bridge.generation;
        let (queued, dropped) = self.queue.take();
        let registrations = self.registrations.take();
        let answered = std::mem::take(&mut self.answered);
        let dropped_saves = self.handle.book.lock().expect("lsp book").dropped_saves.take();
        let attached = LspAttached {
            reattached: self.reattached,
            registrations: registrations.clone(),
            requests_answered: answered,
            notifications_dropped: dropped,
            dropped_saves: dropped_saves.clone(),
        };
        self.handed = Some(Handed {
            generation,
            reattached: self.reattached,
            registrations,
            answered,
            dropped,
            replay: Vec::new(),
            replay_bytes: 0,
            replay_dropped: 0,
            dropped_saves,
        });
        self.reattached = true;
        self.detached_at = None;
        self.attached = Some(bridge);
        self.publish();
        self.send_frame(Frame::Attached(attached)).await?;
        let mut tail = queued.into_iter();
        let mut whole = 0;
        while let Some(message) = tail.next() {
            self.to_bridge(&message).await?;
            if !self
                .attached
                .as_ref()
                .is_some_and(|bridge| bridge.generation == generation)
            {
                // The bridge went during the replay. What it was not handed whole goes back to the queue after what
                // the detach gave back; when it had spoken, what it may not have taken from its frames is counted.
                if self.spoke(generation) {
                    self.queue.add_dropped(whole.min(FRAMES) as u64);
                }
                self.requeue(std::iter::once(message).chain(tail));
                break;
            }
            whole += 1;
            self.hand(generation, message);
        }
        Ok(())
    }

    async fn from_server(&mut self, message: FromServer) -> Result<(), EndReason> {
        let bytes = match message {
            FromServer::Message(bytes) => bytes,
            FromServer::TooLarge { scan, len } => return self.too_large(scan, len).await,
        };
        match message::classify(&bytes) {
            Some(Kind::Response { id }) => {
                if self.answer_for_bridge(&id) {
                    self.to_bridge(&bytes).await?;
                }
            }
            Some(Kind::Request { id, .. }) if id.len() > message::ID_MAX => {
                let why = format!("the request's id is longer than {} bytes", message::ID_MAX);
                self.handle
                    .log
                    .line(&format!("dropped a request from the server: {why}"));
                self.write_now(message::error_response(&id, detached::REQUEST_FAILED, &why))?;
            }
            Some(Kind::Request { id, method }) => match self.attached.as_ref().map(|bridge| bridge.generation) {
                Some(generation) => {
                    self.remember_request(id, method, &bytes, generation)?;
                    self.to_bridge(&bytes).await?;
                }
                None => self.answer_detached(id, method, &bytes)?,
            },
            Some(Kind::Notification { method }) => {
                if let Some(generation) = self.attached.as_ref().map(|bridge| bridge.generation) {
                    if self.to_bridge(&bytes).await? {
                        self.hand(generation, bytes);
                        return Ok(());
                    }
                }
                // No bridge is attached, or the one attached went or was replaced before it took the whole message:
                // it waits for the next attach.
                let collapse = Collapse::of(&method, &bytes);
                self.queue.push(bytes, collapse);
                self.publish();
                if self.queue.bytes() > LSP_DETACHED_QUEUE_MAX {
                    self.handle.log.line(&format!(
                        "more than {LSP_DETACHED_QUEUE_MAX} bytes queued for the editor; the language server is stopped"
                    ));
                    return Err(EndReason::QueueFull);
                }
            }
            None => {
                if self.attached.is_some() {
                    self.to_bridge(&bytes).await?;
                } else {
                    self.handle
                        .log
                        .line("dropped a message that is not JSON-RPC while no editor was attached");
                }
            }
        }
        Ok(())
    }

    /// Whether a response from the server goes to the attached bridge. The helper sends the server no requests of
    /// its own, so every response answers a bridge's request: all go but those whose request was cancelled at a
    /// detach (a later attach's bridge never asked for them), including those whose id was no longer kept.
    fn answer_for_bridge(&self, id: &str) -> bool {
        let mut book = self.handle.book.lock().expect("lsp book");
        book.bridge_pending.remove(id);
        !book.cancelled.remove(id)
    }

    fn remember_request(&mut self, id: String, method: String, bytes: &[u8], generation: u64) -> Result<(), EndReason> {
        let params = (method == detached::CONFIGURATION || detached::is_registration(&method))
            .then(|| message::params(bytes))
            .flatten();
        let config_key = (method == detached::CONFIGURATION)
            .then(|| params.as_ref().map(message::canonical))
            .flatten()
            .filter(|key| key.len() <= detached::CONFIG_KEY_MAX);
        let registration = detached::is_registration(&method)
            .then(|| message::canonical(params.as_ref().unwrap_or(&serde_json::Value::Null)).into_bytes())
            .filter(|params| params.len() <= LSP_REGISTRATIONS_PARAMS_MAX);
        let mut book = self.handle.book.lock().expect("lsp book");
        book.table_answered.remove(&id);
        let request = ServerRequest {
            generation,
            method,
            config_key,
            registration,
        };
        let Some((oldest, evicted)) = book.server_pending.insert(id, request) else {
            return Ok(());
        };
        // Past the bound the oldest request the editor has not answered is answered by the table now, so the server
        // is never left waiting on it; the editor's late answer to it is dropped.
        let answer = detached::answer(
            &oldest,
            &evicted.method,
            evicted.config_key.as_deref(),
            &book.remembered,
            detached::TOO_MANY_UNANSWERED,
        );
        let queued = self.handle.writer.send_now(answer);
        book.table_answered.insert(oldest);
        drop(book);
        self.answered += 1;
        self.handle.log.line(&format!(
            "more than {PENDING_MAX} of the server's requests were unanswered by the editor; the oldest was answered \
             by the helper"
        ));
        self.written(queued)
    }

    fn answer_detached(&mut self, id: String, method: String, bytes: &[u8]) -> Result<(), EndReason> {
        self.answered += 1;
        let params = (method == detached::CONFIGURATION || detached::is_registration(&method))
            .then(|| message::params(bytes))
            .flatten();
        if detached::is_registration(&method) {
            let kept = message::canonical(params.as_ref().unwrap_or(&serde_json::Value::Null)).into_bytes();
            self.keep_registration(&method, Some(kept))?;
        }
        let key = (method == detached::CONFIGURATION)
            .then(|| params.as_ref().map(message::canonical))
            .flatten();
        let answer = {
            let book = self.handle.book.lock().expect("lsp book");
            detached::answer(&id, &method, key.as_deref(), &book.remembered, detached::NOT_CONNECTED)
        };
        self.write_now(answer)
    }

    async fn too_large(&mut self, scan: TopLevelScan, len: u64) -> Result<(), EndReason> {
        let why = format!("the language server's message was {len} bytes, more than the {LSP_MESSAGE_MAX} carried");
        self.handle
            .log
            .line(&format!("dropped a message from the server: {why}"));
        match scan.kind() {
            Some(Kind::Response { id }) => {
                if self.answer_for_bridge(&id) {
                    let error = message::error_response(&id, detached::REQUEST_FAILED, &why);
                    self.to_bridge(&error).await?;
                }
            }
            Some(Kind::Request { id, .. }) => {
                self.write_now(message::error_response(&id, detached::REQUEST_FAILED, &why))?;
            }
            // Counted for the next attach as a dropped notification, as a collapsed one is.
            Some(Kind::Notification { .. }) if self.attached.is_none() => self.queue.add_dropped(1),
            _ => {}
        }
        Ok(())
    }

    /// Ends the server: stops its process group if it still runs, reaps it, and tells the attached bridge why.
    async fn end(mut self, reason: EndReason) {
        let pid = self.handle.pid;
        if !process::exited(self.pidfd.get_ref()) {
            if reason == EndReason::Exited {
                let _ = tokio::time::timeout(EXIT_WAIT, self.pidfd.readable()).await;
            }
            if !process::exited(self.pidfd.get_ref()) {
                process::signal_group(pid, libc::SIGTERM);
                let _ = tokio::time::timeout(STOP_GRACE, self.pidfd.readable()).await;
            }
        }
        // What the server started goes with it, while its unreaped leader still reserves the group's id.
        process::signal_group(pid, libc::SIGKILL);
        let status = tokio::time::timeout(REAP_WAIT, self.child.wait())
            .await
            .ok()
            .and_then(Result::ok);
        {
            let mut registry = self.registry.lock().expect("lsp registry");
            registry.servers.remove(&self.handle.id);
            registry.opened.retain(|_, answer| answer.server_id != self.handle.id);
        }
        self.commands.close();
        if let Some(done) = self.stderr_done.take() {
            let _ = tokio::time::timeout(STDERR_WAIT, done).await;
        }
        let ended = LspEnded {
            code: status.and_then(|status| status.code()),
            signal: status.and_then(|status| status.signal()),
            reason: reason.wire() as i32,
            stderr_tail: self.handle.log.tail(),
        };
        if let Some(bridge) = self.attached.take() {
            let _ = tokio::time::timeout(
                Duration::from_secs(1),
                bridge.frames.send(Ok(ended_frame(ended.clone()))),
            )
            .await;
        }
        while let Ok(command) = self.commands.try_recv() {
            if let Command::Attach(bridge) = command {
                let _ = bridge.frames.try_send(Ok(gone_frame()));
            }
        }
        let outcome = match (ended.code, ended.signal) {
            (Some(code), _) => format!(" exit={code}"),
            (None, Some(signal)) => format!(" signal={signal}"),
            (None, None) => String::new(),
        };
        self.handle.log.line(&format!("ended: {}{outcome}", reason.word()));
        let _ = crate::audit::record(
            &self.state,
            "lsp-end",
            &format!("{} {}{outcome}", self.handle.id, reason.word()),
        );
        self.publish();
        let _ = self.done.send(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, config_key: Option<String>, registration: Option<Vec<u8>>) -> ServerRequest {
        ServerRequest {
            generation: 1,
            method: method.to_owned(),
            config_key,
            registration,
        }
    }

    #[test]
    fn pending_server_requests_keep_params_and_keys_within_their_byte_budgets() {
        let mut pending = ServerPending::default();
        let quarter = LSP_REGISTRATIONS_PARAMS_MAX / 4;
        for id in 0..5 {
            pending.insert(
                format!("r{id}"),
                request(detached::REGISTER, None, Some(vec![b' '; quarter])),
            );
        }
        let key_quarter = detached::CONFIG_BYTES_MAX / 4;
        for id in 0..5 {
            pending.insert(
                format!("c{id}"),
                request(detached::CONFIGURATION, Some("k".repeat(key_quarter)), None),
            );
        }
        assert_eq!(pending.registration_bytes, LSP_REGISTRATIONS_PARAMS_MAX);
        assert_eq!(pending.config_bytes, detached::CONFIG_BYTES_MAX);
        assert!(pending.remove("r4").unwrap().registration.is_none());
        assert!(pending.remove("c4").unwrap().config_key.is_none());
        // A request answered makes room for the next.
        assert!(pending.remove("r0").unwrap().registration.is_some());
        pending.insert(
            "r5".into(),
            request(detached::REGISTER, None, Some(vec![b' '; quarter])),
        );
        assert!(pending.remove("r5").unwrap().registration.is_some());
        // An id kept again replaces the older request's bytes.
        pending.insert("c0".into(), request(detached::CONFIGURATION, Some("k".into()), None));
        assert_eq!(pending.config_bytes, 3 * key_quarter + 1);
        pending.drain_where(|_| true);
        assert_eq!((pending.registration_bytes, pending.config_bytes), (0, 0));
        // The request evicted past the bound gives back its bytes.
        for id in 0..PENDING_MAX + 1 {
            pending.insert(id.to_string(), request(detached::REGISTER, None, Some(vec![b' '])));
        }
        assert_eq!(pending.registration_bytes, PENDING_MAX);
    }

    #[test]
    fn settling_stops_at_the_first_write_past_the_bound() {
        let book_with_requests = || {
            let mut book = Book {
                generation: 1,
                ..Book::default()
            };
            for id in 0..10 {
                book.server_pending
                    .insert(id.to_string(), request(detached::CONFIGURATION, None, None));
            }
            book
        };
        let mut book = book_with_requests();
        let (mut writes, mut queued) = (0, 0);
        let settled = book.settle(1, |_| {
            writes += 1;
            queued += process::WRITE_MAX / 3 + 1;
            queued
        });
        assert_eq!(writes, 3);
        assert_eq!(settled.answered, 3);
        assert!(settled.queued > process::WRITE_MAX);
        assert_eq!(book.generation, 2);
        // A server that reads its input gets every answer.
        let mut book = book_with_requests();
        let settled = book.settle(1, |bytes| bytes.len());
        assert_eq!(settled.answered, 10);
    }
}
