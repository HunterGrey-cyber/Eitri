//! The save hold: a `textDocument/didSave` reaches the language server only once the server's copy of the file is
//! the one the editor saved.
//!
//! The bridge sends a save with the laptop file's SHA-256 beside it. Sync brings the file a moment later, so such a
//! save is held until the server's file at the save's path hashes the same as it or as a later save of the same
//! document waiting behind it (sync carries only the newest of quick saves; the saves up to the newest that matches
//! go together). The file is looked at once, again whenever its directory reports a write, a rename into place, a
//! change of attributes or a new entry for that name (a directory not made yet is reached through the deepest one
//! that exists; a watched directory moved or deleted is watched again by its path), again when a later save of the
//! document joins the queue, and a last time at the deadline (`SAVE_HOLD_MAX`), when it goes anyway with a line in
//! the server's log and the audit log once it has been written to the server. A later save's match counts only once
//! the hold has seen its file change (a watch event, or a look hashing other than its first): the file it already
//! had may be one a reverted save came back to, with the earlier save's content still on its way. Looks run off the
//! orderer and are never waited on for long, so a slow one holds up no other document and no deadline; at most
//! `LOOKS_MAX` run at once for a server, so a hung file system parks no more blocking threads than that. Every later
//! message naming the same document
//! (`params.textDocument.uri` or `params.uri`) waits behind the held save, requests included, so a change never
//! overtakes the save before it; so does a `$/cancelRequest` for a request waiting there, and the editor's answer to
//! the server's `workspace/applyEdit` waits behind every document held when it came. Messages for other documents,
//! and those naming none, go on. A save is not held when its uri is not a `file:` uri of a path under the server's
//! root (a path with `..` included), is longer than `LSP_URI_MAX`, carries no hash, or names something other than a
//! regular file (or one larger than `LOOK_MAX`): sync carries only what the hold waits for.
//!
//! One task per attach (the `Orderer`) takes the bridge's messages in order, holds and releases them, and hands them
//! to the server's stdin through `Handle::to_server`, whose generation check drops whatever a replaced or lost attach
//! still had. Each held save is recorded in the book under the attach's generation; a detach moves the ones still held
//! to the uris the next `LspAttached.dropped_saves` lists, under the same lock, so a save is either passed on or
//! listed, never both and never neither. A held request was never passed on: the server is sent no cancellation for
//! it, and the bridge answers it itself when its link drops.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{CString, OsString};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use eitri_server_proto::{LSP_DETACHED_QUEUE_MAX, LSP_DROPPED_SAVES_MAX, LSP_URI_MAX};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use super::message::{self, Kind};
use super::{ClientMessage, Handle};

/// How long a save waits for its file before it goes anyway.
pub const SAVE_HOLD_MAX: Duration = Duration::from_secs(5);
/// How long the look started at the deadline is waited for; past it the save goes as one whose file did not match.
const LAST_LOOK_WAIT: Duration = Duration::from_secs(1);
/// How long the first look at a save's file may keep the bridge's next message waiting, so a save whose file is
/// already there keeps its place among the messages for other documents; past it the save stays held and the rest
/// go on.
const FIRST_LOOK_WAIT: Duration = Duration::from_millis(100);
/// Bytes of messages waiting behind held saves; past it no more is taken from the bridge until a save goes.
const WAITING_MAX: usize = LSP_DETACHED_QUEUE_MAX;
/// Documents held at once; a save past it goes at once. A detach lists at most this many, as `LspAttached` may.
const HOLDS_MAX: usize = LSP_DROPPED_SAVES_MAX;
/// The largest file a hold hashes; a save of a larger one goes at once.
const LOOK_MAX: u64 = 256 * 1024 * 1024;
/// Looks running at once for one language server, over all its attaches. Past it no look starts: a look that hangs
/// (a stuck mount) keeps its blocking thread, and the blocking pool is shared with the rest of the daemon. A hold that
/// found no place looks when another look ends, or lets its deadline decide.
pub const LOOKS_MAX: usize = 8;
/// What a watched directory reports: for a name in it, a write finished, a rename into place, a change of
/// attributes and a new entry (a directory on the way to the file); for itself, a move or a deletion (its path may
/// name another directory then).
const WATCH_EVENTS: u32 = libc::IN_CLOSE_WRITE
    | libc::IN_MOVED_TO
    | libc::IN_ATTRIB
    | libc::IN_CREATE
    | libc::IN_MOVE_SELF
    | libc::IN_DELETE_SELF
    | libc::IN_ONLYDIR;
const DID_SAVE: &str = "textDocument/didSave";
const CANCEL: &str = "$/cancelRequest";
/// The server request whose answer waits behind the held documents, where the edit's changes wait.
pub(super) const APPLY_EDIT: &str = "workspace/applyEdit";

/// How long the holds of an attach wait.
#[derive(Clone, Copy)]
pub(super) struct Timing {
    pub hold: Duration,
    /// Slept before each look; zero but in tests.
    pub look_delay: Duration,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            hold: SAVE_HOLD_MAX,
            look_delay: Duration::ZERO,
        }
    }
}

/// One of the bridge's messages on its way to the server, with what the hold read of it.
pub(super) struct Pending {
    pub message: ClientMessage,
    pub kind: Option<Kind>,
    /// The hold's number in its attach while this is a held save.
    pub hold: Option<u64>,
    /// The editor's answer to a `workspace/applyEdit` the server sent: it waits behind every held document.
    pub answers_edit: bool,
}

impl Pending {
    pub fn new(message: ClientMessage, kind: Option<Kind>) -> Pending {
        Pending {
            message,
            kind,
            hold: None,
            answers_edit: false,
        }
    }

    /// The laptop file's hash when this is a `textDocument/didSave` that came with one.
    fn hashed_save(&self) -> Option<&str> {
        match &self.kind {
            Some(Kind::Notification { method }) if method == DID_SAVE => self.message.saved_sha256.as_deref(),
            _ => None,
        }
    }
}

/// A save that waits for its file.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Hold {
    pub number: u64,
    pub uri: String,
    pub path: PathBuf,
    /// The root the path is under: the watch never climbs above it.
    pub root: PathBuf,
}

/// The document a message names, if any.
fn document(bytes: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct Uri {
        uri: Option<String>,
    }
    #[derive(Deserialize)]
    struct Params {
        #[serde(rename = "textDocument")]
        text_document: Option<Uri>,
        uri: Option<String>,
    }
    #[derive(Deserialize)]
    struct Message {
        params: Option<Params>,
    }
    let params = serde_json::from_slice::<Message>(bytes).ok()?.params?;
    params.text_document.and_then(|document| document.uri).or(params.uri)
}

/// `%XX` escapes decoded (two hex digits each); `None` for a malformed one.
fn percent_decoded(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let pair = bytes.get(at + 1..at + 3)?;
            if !pair.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            decoded.push(u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?);
            at += 3;
        } else {
            decoded.push(bytes[at]);
            at += 1;
        }
    }
    Some(decoded)
}

/// The path of a `file:` uri under one of `roots`, and that root; `None` for any other uri.
fn path_under<'a>(uri: &str, roots: &'a [PathBuf]) -> Option<(PathBuf, &'a Path)> {
    if uri.len() > LSP_URI_MAX {
        return None;
    }
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') || rest.contains(['?', '#']) {
        return None;
    }
    let bytes = percent_decoded(rest)?;
    if bytes.contains(&0) {
        return None;
    }
    let path = PathBuf::from(OsString::from_vec(bytes));
    if path
        .components()
        .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
    {
        return None;
    }
    let root = roots.iter().find(|root| path.starts_with(root) && path != **root)?;
    Some((path, root))
}

/// A place in a held document's queue.
enum Entry {
    Message(Pending),
    /// Where a message waiting behind several held documents stands in this one's queue, by its number.
    Behind(u64),
}

/// What `SaveHold::submit` did with a message.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Submitted {
    /// It is a save that starts a hold: the caller watches for its file and looks at it.
    Hold(Hold),
    /// It is a save with a hash that joined the queue of this held document: its file may be there already.
    SaveQueued(String),
    /// It may go now or waits behind held saves.
    Taken,
}

/// Orders the bridge's messages around held saves; it reads no file and keeps no clock: the `Orderer` decides when a
/// save may go and calls `release_through`.
pub(super) struct SaveHold {
    roots: Vec<PathBuf>,
    /// Each held document's queue, the held save first.
    held: HashMap<String, VecDeque<Entry>>,
    /// The requests waiting in a held document's queue, by id, and that document: a cancellation of one waits there.
    held_requests: HashMap<String, String>,
    /// Messages waiting behind several held documents, by number, with how many of those queues they still wait in.
    behind: HashMap<u64, (usize, Pending)>,
    next_behind: u64,
    ready: VecDeque<Pending>,
    waiting_bytes: usize,
    next: u64,
}

impl SaveHold {
    pub fn new(roots: Vec<PathBuf>) -> SaveHold {
        SaveHold {
            roots,
            held: HashMap::new(),
            held_requests: HashMap::new(),
            behind: HashMap::new(),
            next_behind: 0,
            ready: VecDeque::new(),
            waiting_bytes: 0,
            next: 0,
        }
    }

    pub fn holding(&self) -> bool {
        !self.held.is_empty()
    }

    /// Takes the bridge's next message. It goes to `poll` at once, waits behind held saves, or is a save that starts
    /// a hold.
    pub fn submit(&mut self, pending: Pending) -> Submitted {
        if pending.answers_edit && !self.held.is_empty() {
            let number = self.next_behind;
            self.next_behind += 1;
            for waiting in self.held.values_mut() {
                waiting.push_back(Entry::Behind(number));
            }
            self.waiting_bytes += pending.message.bytes.len();
            self.behind.insert(number, (self.held.len(), pending));
            return Submitted::Taken;
        }
        let saves = pending.hashed_save().is_some();
        let names_one = matches!(pending.kind, Some(Kind::Request { .. } | Kind::Notification { .. }));
        let uri = (names_one && (saves || !self.held.is_empty()))
            .then(|| document(&pending.message.bytes).or_else(|| self.cancelled_document(&pending)))
            .flatten();
        if let Some(uri) = uri.as_ref().filter(|uri| self.held.contains_key(*uri)) {
            self.queue(uri, pending);
            return if saves {
                Submitted::SaveQueued(uri.clone())
            } else {
                Submitted::Taken
            };
        }
        match uri {
            Some(uri) if saves && self.held.len() < HOLDS_MAX => {
                self.waiting_bytes += pending.message.bytes.len();
                match self.start(uri, VecDeque::from([Entry::Message(pending)])) {
                    Some(hold) => Submitted::Hold(hold),
                    None => Submitted::Taken,
                }
            }
            _ => {
                self.ready.push_back(pending);
                Submitted::Taken
            }
        }
    }

    /// The held document of the request a `$/cancelRequest` names, if that request waits in one's queue.
    fn cancelled_document(&self, pending: &Pending) -> Option<String> {
        if !matches!(&pending.kind, Some(Kind::Notification { method }) if method == CANCEL) {
            return None;
        }
        let id = message::id_key(message::params(&pending.message.bytes)?.get("id")?)?;
        self.held_requests.get(&id).cloned()
    }

    fn queue(&mut self, uri: &str, pending: Pending) {
        let Some(waiting) = self.held.get_mut(uri) else {
            return self.ready.push_back(pending);
        };
        if let Some(Kind::Request { id, .. }) = &pending.kind {
            self.held_requests.insert(id.clone(), uri.to_owned());
        }
        self.waiting_bytes += pending.message.bytes.len();
        waiting.push_back(Entry::Message(pending));
    }

    /// Makes the first of `waiting` (all for `uri`, counted in `waiting_bytes`) a held save when it is one that may
    /// be held; otherwise it and what follows go, up to the next save that may be held.
    fn start(&mut self, uri: String, mut waiting: VecDeque<Entry>) -> Option<Hold> {
        while let Some(entry) = waiting.pop_front() {
            let entry = match entry {
                Entry::Message(mut first) => match self.holdable(&uri, &first) {
                    Some(mut hold) => {
                        hold.number = self.next;
                        self.next += 1;
                        first.hold = Some(hold.number);
                        waiting.push_front(Entry::Message(first));
                        self.held.insert(uri, waiting);
                        return Some(hold);
                    }
                    None => Entry::Message(first),
                },
                behind => behind,
            };
            self.pass(entry);
        }
        None
    }

    fn holdable(&self, uri: &str, pending: &Pending) -> Option<Hold> {
        pending.hashed_save()?;
        let (path, root) = path_under(uri, &self.roots)?;
        Some(Hold {
            number: 0,
            uri: uri.to_owned(),
            path,
            root: root.to_owned(),
        })
    }

    /// An entry leaves a held document's queue: a message goes; a message waiting behind several goes once it has
    /// left the last of them.
    fn pass(&mut self, entry: Entry) {
        let pending = match entry {
            Entry::Message(pending) => pending,
            Entry::Behind(number) => {
                let Some((left, _)) = self.behind.get_mut(&number) else {
                    return;
                };
                *left -= 1;
                if *left > 0 {
                    return;
                }
                match self.behind.remove(&number) {
                    Some((_, pending)) => pending,
                    None => return,
                }
            }
        };
        self.waiting_bytes -= pending.message.bytes.len();
        if let Some(Kind::Request { id, .. }) = &pending.kind {
            self.held_requests.remove(id);
        }
        self.ready.push_back(pending);
    }

    /// Where in `uri`'s queue the newest save whose hash is `sha256` stands (0: the held save itself). With `later`
    /// false only the held save itself is asked about.
    pub fn matching(&self, uri: &str, sha256: &str, later: bool) -> Option<usize> {
        let waiting = self.held.get(uri)?;
        let saved = |entry: &Entry| matches!(entry, Entry::Message(pending) if pending.hashed_save() == Some(sha256));
        if later {
            waiting.iter().rposition(saved)
        } else {
            waiting.front().filter(|entry| saved(entry)).map(|_| 0)
        }
    }

    /// The held save of `uri` goes, with everything behind it up to and including the entry at `last`, and then up
    /// to the next save that may be held, which is held in turn and answered.
    pub fn release_through(&mut self, uri: &str, last: usize) -> Option<Hold> {
        let mut waiting = self.held.remove(uri)?;
        let rest = waiting.split_off((last + 1).min(waiting.len()));
        for entry in waiting {
            self.pass(entry);
        }
        self.start(uri.to_owned(), rest)
    }

    /// Everything held goes, holding nothing more: the attach was replaced or lost, and `to_server` drops it all.
    pub fn release_all(&mut self) {
        for (_, waiting) in std::mem::take(&mut self.held) {
            for entry in waiting {
                self.pass(entry);
            }
        }
    }

    /// The messages that may go now, in order.
    pub fn poll(&mut self) -> Vec<Pending> {
        self.ready.drain(..).collect()
    }

    pub fn waiting_bytes(&self) -> usize {
        self.waiting_bytes
    }
}

/// What a look at a held save's file found.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Look {
    /// A regular file with this SHA-256 (lowercase hex).
    Hashed(String),
    /// Missing: sync has not brought it yet.
    Missing,
    /// Not something sync brings (not a regular file, too large, unreadable): the save is not held.
    Never,
    /// `/proc/self/fd` is not there to read the file through: the save is not held.
    NoProc,
}

/// The looks a language server's holds have running, shared by its attaches.
#[derive(Default)]
pub(super) struct Looks {
    running: AtomicUsize,
    /// The server's log has said that held files cannot be read through `/proc/self/fd`.
    no_proc_logged: AtomicBool,
}

impl Looks {
    /// A place for one more look, or `None` while `LOOKS_MAX` run.
    fn take(self: &Arc<Looks>) -> Option<LookSlot> {
        self.running
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |running| {
                (running < LOOKS_MAX).then_some(running + 1)
            })
            .ok()
            .map(|_| LookSlot(self.clone()))
    }

    #[cfg(test)]
    pub fn running(&self) -> usize {
        self.running.load(Ordering::SeqCst)
    }
}

/// A running look's place, given back when it is dropped.
struct LookSlot(Arc<Looks>);

impl Drop for LookSlot {
    fn drop(&mut self) {
        self.0.running.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Hashes the file at `path` (not following a link at its end). It is opened `O_PATH` first, which a FIFO's writer
/// or a device's driver never notices, and opened to read through `/proc/self/fd` only once it is known to be a
/// regular file; that reopens the very inode looked at. Blocking.
fn look(path: &Path) -> Look {
    let held = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW)
        .open(path);
    let held = match held {
        Ok(held) => held,
        Err(error) if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => return Look::Missing,
        Err(_) => return Look::Never,
    };
    match held.metadata() {
        Ok(metadata) if metadata.is_file() && metadata.len() <= LOOK_MAX => {}
        _ => return Look::Never,
    }
    let mut file = match std::fs::File::open(format!("/proc/self/fd/{}", held.as_raw_fd())) {
        Ok(file) => file,
        // The descriptor is open, so its entry is missing only when `/proc` is.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Look::NoProc,
        Err(_) => return Look::Never,
    };
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut read = 0u64;
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                read += n as u64;
                if read > LOOK_MAX {
                    return Look::Never;
                }
                hasher.update(&buffer[..n]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Look::Never,
        }
    }
    Look::Hashed(hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

/// The watch that leads to a held save's file: on the deepest directory between the root and the file that exists,
/// and the name in it that leads on (the file's own name when it is the file's directory).
struct Armed {
    wd: i32,
    next: OsString,
}

/// One inotify instance for an attach's held saves; holds in one directory share its watch.
struct Watcher {
    fd: AsyncFd<OwnedFd>,
    uses: HashMap<i32, usize>,
}

/// What a read of the queued events found.
#[derive(Default)]
struct Events {
    /// `(watch, name)` for events about a name in a watched directory.
    named: HashSet<(i32, Vec<u8>)>,
    /// Watched directories moved or deleted: their paths may name other directories now.
    moved: HashSet<i32>,
    /// Watches the kernel removed (their directory went).
    gone: HashSet<i32>,
    /// The queue overflowed, or could not be read: every hold looks again.
    overflowed: bool,
}

impl Watcher {
    fn new() -> std::io::Result<Watcher> {
        // SAFETY: inotify_init1 takes no pointers; a new descriptor that nothing else owns is returned.
        let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh descriptor owned only here.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        Ok(Watcher {
            fd: AsyncFd::with_interest(fd, tokio::io::Interest::READABLE)?,
            uses: HashMap::new(),
        })
    }

    /// Watches the deepest directory on the way from `root` to `path` that exists now.
    fn arm(&mut self, path: &Path, root: &Path) -> std::io::Result<Armed> {
        let mut next = path.file_name().unwrap_or_default().to_owned();
        let mut dir = path.parent().unwrap_or(root);
        loop {
            let c_dir = CString::new(dir.as_os_str().as_bytes())?;
            // SAFETY: `c_dir` is NUL-terminated and outlives the call; the inotify descriptor is open.
            let wd = unsafe { libc::inotify_add_watch(self.fd.get_ref().as_raw_fd(), c_dir.as_ptr(), WATCH_EVENTS) };
            if wd >= 0 {
                *self.uses.entry(wd).or_default() += 1;
                return Ok(Armed { wd, next });
            }
            let error = std::io::Error::last_os_error();
            let missing = matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR));
            match dir.parent() {
                Some(parent) if missing && dir != root => {
                    next = dir.file_name().unwrap_or_default().to_owned();
                    dir = parent;
                }
                _ => return Err(error),
            }
        }
    }

    fn disarm(&mut self, armed: Armed) {
        let Some(uses) = self.uses.get_mut(&armed.wd) else {
            return;
        };
        *uses -= 1;
        if *uses == 0 {
            self.uses.remove(&armed.wd);
            // SAFETY: removing a watch takes no pointers; a watch already gone is an error that changes nothing.
            unsafe { libc::inotify_rm_watch(self.fd.get_ref().as_raw_fd(), armed.wd) };
        }
    }

    /// Reads every event queued.
    async fn events(&self) -> Events {
        let mut events = Events::default();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let Ok(mut guard) = self.fd.readable().await else {
                events.overflowed = true;
                return events;
            };
            let read = guard.try_io(|fd| {
                // SAFETY: `buffer` is valid and writable for its length; the descriptor is open.
                let read = unsafe { libc::read(fd.get_ref().as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
                if read < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(read as usize)
                }
            });
            match read {
                Ok(Ok(read)) => parse_events(&buffer[..read], &mut events),
                Ok(Err(error)) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Ok(Err(_)) => {
                    events.overflowed = true;
                    return events;
                }
                // Everything queued has been read; readiness is cleared.
                Err(_would_block) => return events,
            }
        }
    }
}

/// Sorts the raw `events` into `into`.
fn parse_events(events: &[u8], into: &mut Events) {
    let header = std::mem::size_of::<libc::inotify_event>();
    let mut offset = 0;
    while offset + header <= events.len() {
        // SAFETY: the kernel wrote whole events; the header is read unaligned from inside the buffer.
        let event: libc::inotify_event = unsafe { std::ptr::read_unaligned(events[offset..].as_ptr().cast()) };
        let name_start = offset + header;
        let name_end = (name_start + event.len as usize).min(events.len());
        let name = &events[name_start..name_end];
        let name = &name[..name.iter().position(|b| *b == 0).unwrap_or(name.len())];
        into.overflowed |= event.mask & libc::IN_Q_OVERFLOW != 0;
        if event.mask & libc::IN_IGNORED != 0 {
            into.gone.insert(event.wd);
        } else if event.mask & (libc::IN_MOVE_SELF | libc::IN_DELETE_SELF) != 0 {
            into.moved.insert(event.wd);
        } else {
            into.named.insert((event.wd, name.to_vec()));
        }
        offset = name_end;
    }
}

/// The inotify instance of an attach's held saves, open only while one of its watches is in use: an attach holding
/// nothing keeps no instance (an account has few of them, 128 by default).
#[derive(Default)]
struct Watches(Option<Watcher>);

impl Watches {
    fn arm(&mut self, path: &Path, root: &Path) -> std::io::Result<Armed> {
        if self.0.is_none() {
            self.0 = Some(Watcher::new()?);
        }
        let armed = self.0.as_mut().expect("watcher").arm(path, root);
        self.close_if_unused();
        armed
    }

    fn disarm(&mut self, armed: Armed) {
        if let Some(watcher) = self.0.as_mut() {
            watcher.disarm(armed);
        }
        self.close_if_unused();
    }

    /// The kernel removed these watches with their directories: they are not removed again.
    fn forget(&mut self, gone: &HashSet<i32>) {
        if let Some(watcher) = self.0.as_mut() {
            watcher.uses.retain(|wd, _| !gone.contains(wd));
        }
        self.close_if_unused();
    }

    fn close_if_unused(&mut self) {
        if self.0.as_ref().is_some_and(|watcher| watcher.uses.is_empty()) {
            self.0 = None;
        }
    }
}

/// A held save the orderer is waiting on.
struct Wait {
    /// The held save's number.
    number: u64,
    path: PathBuf,
    root: PathBuf,
    /// The hold's deadline; once `due`, when the last look is no longer waited for.
    deadline: Instant,
    /// The deadline passed: the next look's answer decides.
    due: bool,
    /// `None` once the kernel removed the watch and nothing on the way could be watched again: the looks at the
    /// deadline decide.
    armed: Option<Armed>,
    /// What the first look started for this save found.
    first: Option<Look>,
    /// The file changed since the hold began (a watch event for it, or a look that found other than the first): only
    /// then may it match a later save.
    changed: bool,
}

/// A look's answer, for the save of `uri` numbered `number`.
struct Looked {
    uri: String,
    number: u64,
    found: Look,
}

/// The channel from an attach's sink to its orderer: each message with a reply sent once the message has gone on or
/// is held, so the sink takes the next only then.
pub(super) type Inbox = mpsc::Sender<(Pending, oneshot::Sender<()>)>;

enum Event {
    Message(Option<(Pending, oneshot::Sender<()>)>),
    Looked(Option<Looked>),
    Watch,
    Deadline,
}

/// The reply for the message last taken, while the save it started has its first look.
struct Taken {
    reply: oneshot::Sender<()>,
    uri: Option<String>,
    until: Instant,
}

/// The task that orders one attach's messages before the server's stdin.
pub(super) struct Orderer {
    handle: Arc<Handle>,
    generation: u64,
    state: PathBuf,
    timing: Timing,
    hold: SaveHold,
    waits: HashMap<String, Wait>,
    watches: Watches,
    inbox: mpsc::Receiver<(Pending, oneshot::Sender<()>)>,
    /// Looks running, by uri, and whether another was asked for meanwhile; one runs per uri at a time.
    looking: HashMap<String, bool>,
    /// Holds that asked for a look while the server had no place for one: they look when a look ends.
    wanting: HashSet<String>,
    /// Saves let go at their deadline, by hold number, with their uri: logged as late once written to the server.
    late: HashMap<u64, String>,
    looks: mpsc::UnboundedSender<Looked>,
    looked: mpsc::UnboundedReceiver<Looked>,
    taken: Option<Taken>,
}

impl Orderer {
    /// Starts the orderer of the attach `generation` and answers where its messages go; it ends when the sink that
    /// holds that sender is dropped, and what it still holds goes with it.
    pub fn spawn(handle: Arc<Handle>, generation: u64, state: PathBuf, timing: Timing) -> Inbox {
        let (sender, inbox) = mpsc::channel(1);
        let (looks, looked) = mpsc::unbounded_channel();
        let roots = handle.roots.clone();
        let orderer = Orderer {
            handle,
            generation,
            state,
            timing,
            hold: SaveHold::new(roots),
            waits: HashMap::new(),
            watches: Watches::default(),
            inbox,
            looking: HashMap::new(),
            wanting: HashSet::new(),
            late: HashMap::new(),
            looks,
            looked,
            taken: None,
        };
        tokio::spawn(orderer.run());
        sender
    }

    async fn run(mut self) {
        loop {
            self.forward().await;
            self.answer_taken().await;
            let take = self.taken.is_none() && self.hold.waiting_bytes() <= WAITING_MAX;
            let deadline = self
                .waits
                .values()
                .map(|wait| wait.deadline)
                .chain(self.taken.as_ref().map(|taken| taken.until))
                .min();
            let event = {
                let watcher = self.watches.0.as_ref();
                let watch = async move {
                    match watcher {
                        Some(watcher) => {
                            let _ = watcher.fd.readable().await;
                        }
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    message = self.inbox.recv(), if take => Event::Message(message),
                    looked = self.looked.recv() => Event::Looked(looked),
                    _ = watch => Event::Watch,
                    _ = super::sleep_until(deadline) => Event::Deadline,
                }
            };
            match event {
                Event::Message(None) => return,
                Event::Message(Some((pending, reply))) => self.take(pending, reply),
                Event::Looked(Some(looked)) => {
                    self.looked(looked);
                    self.look_wanted();
                }
                Event::Looked(None) => {}
                Event::Watch => self.watched().await,
                Event::Deadline => self.deadline(),
            }
        }
    }

    /// Takes one of the bridge's messages. Its reply waits for the first look at a save it started, a moment at most.
    fn take(&mut self, mut pending: Pending, reply: oneshot::Sender<()>) {
        if let Some(Kind::Response { id }) = &pending.kind {
            pending.answers_edit = self.hold.holding() && self.handle.book.lock().expect("lsp book").applies_edit(id);
        }
        let uri = match self.hold.submit(pending) {
            Submitted::Hold(hold) => {
                let uri = hold.uri.clone();
                self.start(hold);
                Some(uri)
            }
            Submitted::SaveQueued(uri) => {
                self.look_at(&uri);
                None
            }
            Submitted::Taken => None,
        };
        self.taken = Some(Taken {
            reply,
            uri,
            until: Instant::now() + FIRST_LOOK_WAIT,
        });
    }

    /// Answers the message last taken once what may go has gone and the first look at a save it started has
    /// answered or taken too long.
    async fn answer_taken(&mut self) {
        let Some(taken) = self.taken.as_ref() else {
            return;
        };
        let looking = taken
            .uri
            .as_ref()
            .is_some_and(|uri| self.looking.contains_key(uri) && self.waits.contains_key(uri));
        if looking && Instant::now() < taken.until {
            return;
        }
        self.forward().await;
        if let Some(taken) = self.taken.take() {
            let _ = taken.reply.send(());
        }
    }

    /// Hands what may go to the server's stdin, in order. Once the attach turns out replaced or lost, everything it
    /// holds goes the same way and nothing more is watched.
    async fn forward(&mut self) {
        let mut ready = self.hold.poll();
        while !ready.is_empty() {
            let mut lost = false;
            for pending in ready {
                let late = pending.hold.and_then(|number| self.late.remove(&number));
                if !self.handle.to_server(self.generation, pending).await {
                    lost = true;
                } else if let Some(uri) = late {
                    self.went_late(&uri);
                }
            }
            if lost {
                self.abandon();
            }
            ready = self.hold.poll();
        }
    }

    /// The attach was replaced or lost: what it holds goes to `to_server`, which drops it, and its watches end.
    fn abandon(&mut self) {
        self.hold.release_all();
        self.waits.clear();
        self.wanting.clear();
        self.late.clear();
        self.watches = Watches::default();
    }

    /// Holds `first`, and each save released at once after it: recorded in the book, its directory watched, then
    /// looked at (in that order, so a file landing between the two is not missed).
    fn start(&mut self, first: Hold) {
        let mut next = Some(first);
        while let Some(hold) = next.take() {
            if !self
                .handle
                .book
                .lock()
                .expect("lsp book")
                .hold_save(self.generation, hold.number, &hold.uri)
            {
                return self.abandon();
            }
            match self.watches.arm(&hold.path, &hold.root) {
                Ok(armed) => {
                    let wait = Wait {
                        number: hold.number,
                        path: hold.path,
                        root: hold.root,
                        deadline: Instant::now() + self.timing.hold,
                        due: false,
                        armed: Some(armed),
                        first: None,
                        changed: false,
                    };
                    self.waits.insert(hold.uri.clone(), wait);
                    self.look_at(&hold.uri);
                }
                Err(error) => {
                    self.handle.log.line(&format!(
                        "cannot watch {:?} for a save: {error}; the save goes without waiting",
                        hold.path
                    ));
                    next = self.hold.release_through(&hold.uri, 0);
                }
            }
        }
    }

    /// Looks at the held save's file of `uri` off the runtime; when a look of it is running, another follows it, and
    /// while the server has `LOOKS_MAX` running it waits for one of them to end.
    fn look_at(&mut self, uri: &str) {
        let Some(wait) = self.waits.get(uri) else {
            return;
        };
        if let Some(again) = self.looking.get_mut(uri) {
            *again = true;
            return;
        }
        let Some(slot) = self.handle.looks.take() else {
            self.wanting.insert(uri.to_owned());
            return;
        };
        self.looking.insert(uri.to_owned(), false);
        let (path, delay, looks) = (wait.path.clone(), self.timing.look_delay, self.looks.clone());
        let (uri, number) = (uri.to_owned(), wait.number);
        tokio::task::spawn_blocking(move || {
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            let found = look(&path);
            // Given back before the answer, so the holds waiting for a place find it free.
            drop(slot);
            let _ = looks.send(Looked { uri, number, found });
        });
    }

    /// A look ended: the holds that found no place for theirs look now.
    fn look_wanted(&mut self) {
        for uri in std::mem::take(&mut self.wanting) {
            self.look_at(&uri);
        }
    }

    /// A look answered. A file that hashes as a save waiting for it lets that save go, with every message before it
    /// (a later save only once the file has changed since the hold began); one that is not a regular file lets the
    /// held save go unheld. A look started for an earlier save of the document counts only when it matches.
    fn looked(&mut self, Looked { uri, number, found }: Looked) {
        let again = self.looking.remove(&uri).unwrap_or(false);
        if found == Look::NoProc && !self.handle.looks.no_proc_logged.swap(true, Ordering::SeqCst) {
            self.handle.log.line(
                "held files cannot be read through /proc/self/fd: saves go to the language server without waiting",
            );
        }
        let Some(wait) = self.waits.get_mut(&uri) else {
            return;
        };
        let (current, due) = (wait.number == number, wait.due);
        if current {
            match &wait.first {
                None => wait.first = Some(found.clone()),
                Some(first) => wait.changed |= *first != found,
            }
        }
        let later = wait.changed;
        match found {
            Look::Hashed(sha256) => {
                if let Some(last) = self.hold.matching(&uri, &sha256, later) {
                    return self.release_through(&uri, last);
                }
            }
            Look::Never | Look::NoProc if current => return self.release_through(&uri, 0),
            _ => {}
        }
        if again || !current {
            self.look_at(&uri);
        } else if due {
            self.late(&uri);
        }
    }

    /// The held save of `uri` goes, with everything up to the queue's entry `last`; the next save behind it, if
    /// any, is held in turn.
    fn release_through(&mut self, uri: &str, last: usize) {
        if let Some(armed) = self.waits.remove(uri).and_then(|wait| wait.armed) {
            self.watches.disarm(armed);
        }
        if let Some(next) = self.hold.release_through(uri, last) {
            self.start(next);
        }
    }

    /// The held save of `uri` goes although its file does not match: logged as late once it is written.
    fn late(&mut self, uri: &str) {
        if let Some(wait) = self.waits.get(uri) {
            self.late.insert(wait.number, uri.to_owned());
        }
        self.release_through(uri, 0);
    }

    /// A save let go at its deadline was written to the server.
    fn went_late(&self, uri: &str) {
        self.handle.log.line(&format!(
            "a save of {uri:?} went to the language server before its file matched the editor's, after {:?}",
            self.timing.hold
        ));
        let _ = crate::audit::record(&self.state, "lsp-save-late", &self.handle.id);
    }

    /// A watched directory reported something: each hold it may concern is armed again (a directory on the way may
    /// have appeared, or the watched one moved) and looked at.
    async fn watched(&mut self) {
        let Some(watcher) = self.watches.0.as_ref() else {
            return;
        };
        let events = watcher.events().await;
        self.watches.forget(&events.gone);
        let concerned: Vec<String> = self
            .waits
            .iter()
            .filter(|(_, wait)| match &wait.armed {
                Some(armed) => {
                    events.overflowed
                        || events.gone.contains(&armed.wd)
                        || events.moved.contains(&armed.wd)
                        || events.named.contains(&(armed.wd, armed.next.as_bytes().to_vec()))
                }
                None => events.overflowed,
            })
            .map(|(uri, _)| uri.clone())
            .collect();
        for uri in concerned {
            let Some(wait) = self.waits.get_mut(&uri) else {
                continue;
            };
            wait.changed = true;
            // A watch the kernel removed is not counted any more (its number may be reused): it is only let go.
            let old = wait.armed.take().filter(|old| !events.gone.contains(&old.wd));
            match self.watches.arm(&wait.path, &wait.root) {
                Ok(armed) => {
                    wait.armed = Some(armed);
                    if let Some(old) = old {
                        self.watches.disarm(old);
                    }
                }
                // When nothing on the way can be watched any more, the looks at the deadline decide.
                Err(_) => wait.armed = old,
            }
            self.look_at(&uri);
        }
    }

    /// A hold whose deadline passed gets a last look, waited for `LAST_LOOK_WAIT` at most; past that it goes late.
    fn deadline(&mut self) {
        let now = Instant::now();
        let due: Vec<(String, bool)> = self
            .waits
            .iter()
            .filter(|(_, wait)| wait.deadline <= now)
            .map(|(uri, wait)| (uri.clone(), wait.due))
            .collect();
        for (uri, already) in due {
            if already {
                self.late(&uri);
                continue;
            }
            if let Some(wait) = self.waits.get_mut(&uri) {
                wait.due = true;
                wait.deadline = now + LAST_LOOK_WAIT;
            }
            self.look_at(&uri);
        }
    }
}

/// The uris of held saves a detach dropped, in the order they were held, each once and at most
/// `LSP_DROPPED_SAVES_MAX`; past it the newer are not listed.
#[derive(Default)]
pub(super) struct DroppedSaves {
    uris: Vec<String>,
    listed: HashSet<String>,
}

impl DroppedSaves {
    pub fn add(&mut self, uri: String) {
        if self.uris.len() < LSP_DROPPED_SAVES_MAX && self.listed.insert(uri.clone()) {
            self.uris.push(uri);
        }
    }

    pub fn take(&mut self) -> Vec<String> {
        self.listed.clear();
        std::mem::take(&mut self.uris)
    }

    /// Puts back what an attach was handed and may not have read, ahead of anything listed since.
    pub fn give_back(&mut self, handed: Vec<String>) {
        let newer = self.take();
        for uri in handed.into_iter().chain(newer) {
            self.add(uri);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> Vec<PathBuf> {
        vec![PathBuf::from("/srv/project"), PathBuf::from("/data/project")]
    }

    #[test]
    fn only_file_uris_under_a_root_are_held_paths() {
        let roots = roots();
        let under = |uri: &str| path_under(uri, &roots).map(|(path, root)| (path, root.to_owned()));
        assert_eq!(
            under("file:///srv/project/src/a%20b.rs"),
            Some((PathBuf::from("/srv/project/src/a b.rs"), PathBuf::from("/srv/project")))
        );
        assert_eq!(
            under("file://localhost/data/project/x.rs").map(|(_, root)| root),
            Some(PathBuf::from("/data/project"))
        );
        for outside in [
            "file:///srv/project",
            "file:///srv/projectx/a.rs",
            "file:///srv/project/../etc/passwd",
            "file:///srv/project/a/%2E%2E/%2E%2E/etc",
            "file://other-host/srv/project/a.rs",
            "file:///srv/project/a%2",
            "file:///srv/project/a%00b",
            "file:///srv/project/a%+fb",
            "untitled:Untitled-1",
        ] {
            assert_eq!(under(outside), None, "{outside}");
        }
        let long = format!("file:///srv/project/{}", "n".repeat(LSP_URI_MAX));
        assert_eq!(under(&long), None);
    }

    #[test]
    fn an_escape_is_two_hex_digits() {
        assert_eq!(percent_decoded("a%2fb%2F"), Some(b"a/b/".to_vec()));
        for malformed in ["%+f", "%-1", "% f", "%f", "%g0"] {
            assert_eq!(percent_decoded(malformed), None, "{malformed}");
        }
    }

    fn pending(value: serde_json::Value, saved: Option<&str>) -> Pending {
        let bytes = serde_json::to_vec(&value).unwrap();
        let kind = super::super::message::classify(&bytes);
        Pending::new(
            ClientMessage {
                bytes,
                saved_sha256: saved.map(str::to_owned),
            },
            kind,
        )
    }

    fn hash(of: char) -> String {
        of.to_string().repeat(64)
    }

    fn save_hashed(uri: &str, of: char) -> Pending {
        pending(
            serde_json::json!({"jsonrpc": "2.0", "method": DID_SAVE, "params": {"textDocument": {"uri": uri}}}),
            Some(&hash(of)),
        )
    }

    fn save(uri: &str) -> Pending {
        save_hashed(uri, 'a')
    }

    fn change(uri: &str) -> Pending {
        pending(
            serde_json::json!({"jsonrpc": "2.0", "method": "textDocument/didChange", "params": {"textDocument": {"uri": uri}}}),
            None,
        )
    }

    fn methods(ready: Vec<Pending>) -> Vec<String> {
        ready
            .into_iter()
            .map(|pending| match pending.kind {
                Some(Kind::Notification { method } | Kind::Request { method, .. }) => method,
                _ => "response".to_owned(),
            })
            .collect()
    }

    fn held(submitted: Submitted) -> Hold {
        match submitted {
            Submitted::Hold(hold) => hold,
            other => panic!("expected a hold, got {other:?}"),
        }
    }

    #[test]
    fn a_held_document_keeps_its_order_and_a_second_save_is_held_in_turn() {
        let a = "file:///srv/project/a.rs";
        let b = "file:///srv/project/b.rs";
        let mut hold = SaveHold::new(roots());
        let first = held(hold.submit(save(a)));
        assert_eq!(
            (first.number, first.path.as_path()),
            (0, Path::new("/srv/project/a.rs"))
        );
        assert_eq!(hold.submit(change(a)), Submitted::Taken);
        assert_eq!(hold.submit(save(a)), Submitted::SaveQueued(a.to_owned()));
        assert_eq!(hold.submit(change(a)), Submitted::Taken);
        assert_eq!(hold.submit(change(b)), Submitted::Taken);
        let response = pending(serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": null}), None);
        assert_eq!(hold.submit(response), Submitted::Taken);
        assert_eq!(methods(hold.poll()), ["textDocument/didChange", "response"]);
        assert!(hold.waiting_bytes() > 0);
        let second = hold.release_through(a, 0).unwrap();
        assert_eq!(second.number, 1);
        assert_eq!(methods(hold.poll()), [DID_SAVE, "textDocument/didChange"]);
        assert!(hold.release_through(a, 0).is_none());
        assert_eq!(methods(hold.poll()), [DID_SAVE, "textDocument/didChange"]);
        assert_eq!(hold.waiting_bytes(), 0);
        // A save without a hash is not held.
        let mut unhashed = save(a);
        unhashed.message.saved_sha256 = None;
        assert_eq!(hold.submit(unhashed), Submitted::Taken);
        assert_eq!(methods(hold.poll()), [DID_SAVE]);
    }

    #[test]
    fn a_file_matching_a_later_save_lets_every_save_up_to_it_go() {
        let a = "file:///srv/project/a.rs";
        let mut hold = SaveHold::new(roots());
        held(hold.submit(save_hashed(a, 'x')));
        hold.submit(change(a));
        assert_eq!(hold.submit(save_hashed(a, 'y')), Submitted::SaveQueued(a.to_owned()));
        hold.submit(change(a));
        hold.submit(save_hashed(a, 'z'));
        assert_eq!(hold.matching(a, &hash('x'), true), Some(0));
        assert_eq!(hold.matching(a, &hash('y'), true), Some(2));
        assert_eq!(hold.matching(a, &hash('q'), true), None);
        // Asked about the held save alone, a later save's match is not seen.
        assert_eq!(hold.matching(a, &hash('y'), false), None);
        assert_eq!(hold.matching(a, &hash('x'), false), Some(0));
        // The newest save that matches goes with everything before it; the next save is held in turn.
        let next = hold.release_through(a, 2).unwrap();
        assert_eq!(next.number, 1);
        assert_eq!(
            methods(hold.poll()),
            [DID_SAVE, "textDocument/didChange", DID_SAVE, "textDocument/didChange"]
        );
        assert_eq!(hold.matching(a, &hash('z'), false), Some(0));
    }

    #[test]
    fn a_cancellation_and_an_edit_answer_wait_behind_the_held_documents() {
        let (a, b) = ("file:///srv/project/a.rs", "file:///srv/project/b.rs");
        let mut hold = SaveHold::new(roots());
        held(hold.submit(save(a)));
        held(hold.submit(save(b)));
        let hover = serde_json::json!({"jsonrpc": "2.0", "id": 5, "method": "textDocument/hover",
            "params": {"textDocument": {"uri": a}, "position": {"line": 0, "character": 0}}});
        hold.submit(pending(hover, None));
        let cancel = |id: i64| {
            pending(
                serde_json::json!({"jsonrpc": "2.0", "method": CANCEL, "params": {"id": id}}),
                None,
            )
        };
        hold.submit(cancel(5));
        // A cancellation of a request that does not wait goes on.
        hold.submit(cancel(9));
        let mut answer = pending(
            serde_json::json!({"jsonrpc": "2.0", "id": "edit-1", "result": {"applied": true}}),
            None,
        );
        answer.answers_edit = true;
        hold.submit(answer);
        assert_eq!(methods(hold.poll()), [CANCEL]);
        hold.release_through(a, 0);
        assert_eq!(methods(hold.poll()), [DID_SAVE, "textDocument/hover", CANCEL]);
        hold.release_through(b, 0);
        assert_eq!(methods(hold.poll()), [DID_SAVE, "response"]);
        assert_eq!(hold.waiting_bytes(), 0);
        // With nothing held, the answer goes at once.
        let mut answer = pending(serde_json::json!({"jsonrpc": "2.0", "id": 2, "result": {}}), None);
        answer.answers_edit = true;
        hold.submit(answer);
        assert_eq!(methods(hold.poll()), ["response"]);
    }

    #[tokio::test]
    async fn the_inotify_instance_lives_only_while_a_watch_is_in_use() {
        let dir = std::env::temp_dir().join(format!("save-hold-watches-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let mut watches = Watches::default();
        let first = watches.arm(&dir.join("sub/a.rs"), &dir).unwrap();
        let second = watches.arm(&dir.join("sub/b.rs"), &dir).unwrap();
        assert_eq!(first.wd, second.wd);
        // A file whose directory is not made yet is watched through the deepest one that is, for the next name.
        let deeper = watches.arm(&dir.join("new/deeper/c.rs"), &dir).unwrap();
        assert_eq!(deeper.next, OsString::from("new"));
        watches.disarm(first);
        watches.disarm(deeper);
        assert!(watches.0.is_some());
        watches.disarm(second);
        assert!(watches.0.is_none());
        // Nothing to watch on the way: no instance is kept either.
        assert!(watches.arm(&dir.join("gone/x.rs"), &dir.join("gone")).is_err());
        assert!(watches.0.is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dropped_saves_are_listed_once_and_what_was_handed_comes_first() {
        let mut dropped = DroppedSaves::default();
        dropped.add("b".into());
        dropped.add("b".into());
        dropped.give_back(vec!["a".into(), "b".into()]);
        assert_eq!(dropped.take(), ["a", "b"]);
        for n in 0..LSP_DROPPED_SAVES_MAX + 5 {
            dropped.add(n.to_string());
        }
        assert_eq!(dropped.take().len(), LSP_DROPPED_SAVES_MAX);
    }
}
