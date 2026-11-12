//! The per-root open socket: how `eitri FILE` hands a file to a window that already holds that root,
//! instead of starting another window, and how a launch asks that window to come forward ([`raise`]).
//!
//! A window that owns its editor binds `open-<16 hex project key>.sock` in the control directory. A
//! client connects for each file and waits for `ready`, which the thread says when it turns to that
//! connection; it then sends one JSON line and reads one reply line: `ok`, `busy` or `refused: <why>`.
//! `ok` means the window accepted the file, not that nvim has opened it: nvim may be sitting at a
//! prompt. On `busy`, a refusal or a timeout the client starts a new window, so nothing here is ever
//! fatal, to the window or to the command.
//!
//! The socket is a way for a local process to act on a window that holds an agent, so it stays narrow:
//!
//! - Only a peer with this process's effective uid is served, checked before its request is read. The
//!   client checks the server the same way before it writes a path.
//! - A request has one shape, one line of at most 4096 bytes within one second, and any other key is
//!   refused. It carries no text that is executed.
//! - An open's path must name a regular file inside the root, or a free name in a directory inside it,
//!   after every link is resolved. What goes on is the canonical path.
//! - A raise's path must be the root itself, spelt as this window spells it. Nothing on disk is looked
//!   at for one, and it changes nothing in the window: it asks for less than an open.
//! - Everything up to the hand-over happens on a thread of its own, parked in `poll(2)`, so a client
//!   never holds the GTK thread and an idle window takes no wakeups from this.
//!
//! One atomic decides who owns a handed-over request. The thread stops waiting for the GTK side after
//! [`ANSWER_WITHIN`] and says `busy`, and a request nobody took by then can no longer be taken. Nor
//! can one whose client was killed or interrupted in the meantime. Once the GTK side has taken it, the
//! request will run, so a thread that hears nothing more says `ok`: `busy` could open the file twice.
//!
//! The client settles the rest by holding its request back until `ready`. Connections wait behind each
//! other, and a client that gives up in that queue has sent nothing, so its new window is the only
//! place the file opens. One that has sent gets [`CLIENT_DEADLINE`] afresh, longer than everything the
//! thread can still spend on the request, so it hears how the request ended.
//!
//! That leaves one way to open a file twice: a window stopped or starved for longer than that margin
//! between a take and its reply, which no bounded wait on the client's side can tell from a dead one.
//!
//! A socket in the way is reclaimed only under `open-<key>.lock`, only when a connect is refused twice
//! a moment apart (a claimant between `bind` and `listen` refuses the first one), and only the very
//! socket that was probed. Each server also holds its socket's `.alive` lock from before bind until
//! close: a refused connect can mean a full accept queue, so reclaim must take that lock too.
//! A socket with a listener or a held liveness lock is another window's. This window then binds
//! `open-<key>-<pid>.sock`, which nobody looks up by root: only its own terminals are told the name.
//! A crash leaves that file behind until the runtime directory goes, at logout.

use crate::instance_dir::{nonblocking_connect_outcome, ConnectOutcome};
use crate::panel_control::{
    bind_unfollowed, ensure_dir, euid, file_identity, identity, make_control_socket_private, peer_uid, read_line,
    remove_own_socket, reply_line, take_reclaim_lock, validate_nvim_addr, LineFail, MAX_LINE, REQUEST_DEADLINE,
};
use crate::project_root::OpenTarget;
use agent::private_fs::PrivateLock;
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::fmt;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long one connection may take to deliver its request line.
pub const READ_WITHIN: Duration = REQUEST_DEADLINE;
/// How long a request waits for the GTK side to take it.
pub const ANSWER_WITHIN: Duration = Duration::from_secs(2);
/// How much longer a taken request waits for its answer.
pub const TAKEN_WITHIN: Duration = Duration::from_secs(1);
/// How long a client waits at each of its two steps on a target: from the connect to [`READY`], and
/// from the request to the reply. The first is what time in the accept queue counts against, and a
/// client that gives up there has sent nothing. The second is longer than the server's three waits
/// together, so a client that has sent a request hears how it ended.
pub const CLIENT_DEADLINE: Duration = Duration::from_millis(4500);

/// What the thread says when it turns to a connection, before anything else is read or written.
const READY: &str = "ready";
/// The protocol versions, one operation to each. A new capability is a new version, never an extra key, so a window
/// older than a request refuses it by number, and `eitri FILE` still speaks the version such a window knows.
const PROTOCOL_OPEN: u64 = 1;
const PROTOCOL_RAISE: u64 = 2;
/// The operation of [`PROTOCOL_OPEN`].
const OP_OPEN: &str = "open";
/// The operation of [`PROTOCOL_RAISE`].
const OP_RAISE: &str = "raise";
/// The longest activation token, in bytes.
const MAX_TOKEN: usize = 512;
/// How long a claim waits for another claimant's reclaim. With two [`RECHECK_AFTER`] pauses, one for
/// each name it may have to look at twice, this is what the GTK thread can spend in a claim, before
/// the window shows.
const LOCK_WAIT: Duration = Duration::from_millis(60);
/// How long after a refused connect the socket is looked at again. A claimant that has bound but
/// not yet listened refuses connections for this long at most.
const RECHECK_AFTER: Duration = Duration::from_millis(20);
/// How long a reply may take to write to a client that does not read.
const WRITE_WITHIN: Duration = Duration::from_secs(1);
/// How long the thread rests after an `accept` that failed (descriptors exhausted, say). The
/// listener stays readable meanwhile, so without it the thread would spin.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// Who owns a handed-over request.
const WAITING: u8 = 0;
const TAKEN: u8 = 1;
const ABANDONED: u8 = 2;

/// `dir/open-<16 hex>.sock`, the one socket a client finds by root.
pub fn socket_path(dir: &Path, root: &Path) -> io::Result<PathBuf> {
    let key = crate::state_paths::project_key(root);
    agent::socket_path::in_dir(dir, &format!("open-{key}.sock"))
}

/// `dir/open-<16 hex>-<pid>.sock`, for a window that found the root's socket taken.
fn pid_socket_path(dir: &Path, root: &Path) -> io::Result<PathBuf> {
    let key = crate::state_paths::project_key(root);
    let pid = std::process::id();
    agent::socket_path::in_dir(dir, &format!("open-{key}-{pid}.sock"))
}

/// `dir/open-<16 hex>.lock`, the file claimants take an exclusive lock on while they reclaim.
fn lock_path(dir: &Path, root: &Path) -> PathBuf {
    let key = crate::state_paths::project_key(root);
    dir.join(format!("open-{key}.lock"))
}

/// A file to open, validated on the listener thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Open {
    /// Canonical and inside the root; for a file that does not exist yet, its parent is canonical.
    pub path: PathBuf,
    /// [`crate::project_root::LAST_LINE`] is the end of the buffer.
    pub line: Option<u32>,
    pub col: Option<u32>,
    /// The launcher's activation token, opaque.
    pub token: Option<String>,
}

/// What a client asks of the window, validated on the listener thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Open(Open),
    /// Come forward, and nothing else: no file, no editor, no module changes.
    Raise {
        /// The launcher's activation token, opaque.
        token: Option<String>,
    },
}

/// What the window says to a request it took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Accepted,
    Busy,
}

/// One validated request, handed over at most once; dropping it unanswered answers `busy`.
#[derive(Debug)]
pub struct Incoming {
    request: Request,
    /// [`WAITING`] until the first of the GTK side and the thread moves it on.
    owner: Arc<AtomicU8>,
    answer: Sender<Answer>,
    /// The connection, to see from the GTK side whether its client is still there.
    client: UnixStream,
}

impl Incoming {
    /// The request and the way to answer it, or `None` when the thread already gave up on it or its
    /// client has left, which may mean a new window already holds the file. Never blocks.
    pub fn take(self) -> Option<(Request, Answering)> {
        if client_left(&self.client) {
            return None;
        }
        self.owner
            .compare_exchange(WAITING, TAKEN, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some((self.request, Answering { answer: self.answer }))
    }
}

/// The answer a taken request owes; dropping it unanswered answers `busy`.
#[derive(Debug)]
pub struct Answering {
    answer: Sender<Answer>,
}

impl Answering {
    pub fn answer(self, answer: Answer) {
        // The thread may have stopped waiting; the client then has its reply already.
        let _ = self.answer.send(answer);
    }
}

/// How long the thread gives each stage of a request.
#[derive(Debug, Clone, Copy)]
struct Waits {
    read: Duration,
    answer: Duration,
    taken: Duration,
}

impl Waits {
    const STANDARD: Waits = Waits {
        read: READ_WITHIN,
        answer: ANSWER_WITHIN,
        taken: TAKEN_WITHIN,
    };
}

/// One line on the wire, in both directions.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    v: u64,
    op: String,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    col: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<String>,
}

/// The request one line says, or the reason it is refused (without the `refused: ` prefix).
fn parse(line: &[u8]) -> Result<Wire, String> {
    let wire: Wire = serde_json::from_slice(line).map_err(|_| "malformed request".to_owned())?;
    if !matches!(wire.v, PROTOCOL_OPEN | PROTOCOL_RAISE) {
        return Err(format!("unsupported protocol version {}", wire.v));
    }
    match (wire.v, wire.op.as_str()) {
        (PROTOCOL_OPEN, OP_OPEN) => {}
        // A window has no position to go to.
        (PROTOCOL_RAISE, OP_RAISE) if wire.line.is_some() || wire.col.is_some() => {
            return Err("malformed request".to_owned());
        }
        (PROTOCOL_RAISE, OP_RAISE) => {}
        _ => return Err("unsupported op".to_owned()),
    }
    if wire.token.as_ref().is_some_and(|token| token.len() > MAX_TOKEN) {
        return Err("token too long".to_owned());
    }
    Ok(wire)
}

/// The file `path` names, or why it may not be opened. Inside `root` is judged by components, after
/// every link is resolved, so a symlink that leaves the root is refused. Nothing is opened here: a
/// FIFO is judged by its metadata and refused. No reason quotes the path, because a file name may
/// hold a newline and a reply is one line.
fn validated_path(path: &Path, root: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("the path is not absolute".to_owned());
    }
    let resolved = match path.symlink_metadata() {
        Ok(_) => {
            // The name exists, so a failure here is a link that leads nowhere. Writing through one
            // would create a file wherever it points.
            let canonical = path
                .canonicalize()
                .map_err(|_| "the path is a dangling link".to_owned())?;
            let is_file = canonical.metadata().is_ok_and(|meta| meta.is_file());
            if !is_file {
                return Err("the path is not a regular file".to_owned());
            }
            canonical
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => new_file_path(path)?,
        Err(_) => return Err("the path cannot be read".to_owned()),
    };
    if !resolved.starts_with(root) {
        return Err("the path is outside the root".to_owned());
    }
    if resolved.to_str().is_none() {
        return Err("the path is not valid UTF-8".to_owned());
    }
    Ok(resolved)
}

/// Where a file that does not exist yet would be: its canonical parent and its name. A name that can
/// only be a directory (`foo/`, `foo/.`) is not a new file.
fn new_file_path(path: &Path) -> Result<PathBuf, String> {
    let bytes = path.as_os_str().as_bytes();
    let last = bytes.rsplit(|byte| *byte == b'/').next().unwrap_or(bytes);
    if matches!(last, b"" | b"." | b"..") {
        return Err("the path names no file".to_owned());
    }
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err("the path names no file".to_owned());
    };
    let parent = parent
        .canonicalize()
        .map_err(|_| "the parent directory does not exist".to_owned())?;
    if !parent.is_dir() {
        return Err("the parent is not a directory".to_owned());
    }
    Ok(parent.join(name))
}

/// Whether the client on `stream` has gone: closed, or shut its writing side after the request.
fn client_left(stream: &UnixStream) -> bool {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const HANGUP: libc::c_short = libc::POLLHUP | libc::POLLRDHUP;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    const HANGUP: libc::c_short = libc::POLLHUP;
    let mut pollfd = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: HANGUP,
        revents: 0,
    };
    // SAFETY: one valid `pollfd`, and the count says one; a zero timeout returns at once.
    let ready = unsafe { libc::poll(&mut pollfd, 1, 0) };
    ready > 0 && pollfd.revents & HANGUP != 0
}

/// What the listener thread needs.
struct Serving {
    root: PathBuf,
    me: u32,
    waits: Waits,
    requests: Sender<Incoming>,
    wake: UnixStream,
    #[cfg(test)]
    watch: Arc<ThreadWatch>,
}

impl Serving {
    fn poll_listener(&self, waiting: &mut [libc::pollfd], timeout_ms: libc::c_int) -> libc::c_int {
        #[cfg(test)]
        {
            self.watch.timeout_ms.store(timeout_ms, Ordering::SeqCst);
            self.watch.entered.store(true, Ordering::SeqCst);
        }
        // SAFETY: the slice supplies its actual length and owns valid borrowed descriptors.
        unsafe { libc::poll(waiting.as_mut_ptr(), waiting.len() as libc::nfds_t, timeout_ms) }
    }

    /// Parks in `poll` on the listener and the stop descriptor, and serves one connection at a time.
    fn run(&self, listener: &UnixListener, stop: &UnixStream) {
        let mut waiting = [
            libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stop.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            let timeout_ms = -1;
            let ready = self.poll_listener(&mut waiting, timeout_ms);
            #[cfg(test)]
            self.watch.polls.fetch_add(1, Ordering::SeqCst);
            if ready < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
            // The other end of the stop pair closing is a hangup here.
            if waiting[1].revents != 0 {
                return;
            }
            match listener.accept() {
                Ok((stream, _)) => self.serve_one(stream),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => std::thread::sleep(ACCEPT_BACKOFF),
            }
        }
    }

    fn serve_one(&self, stream: UnixStream) {
        // A socket accepted from a non-blocking listener inherits that flag on macOS and the BSDs,
        // and every read below would then fail at once.
        if stream.set_nonblocking(false).is_err() || stream.set_write_timeout(Some(WRITE_WITHIN)).is_err() {
            return;
        }
        if !matches!(peer_uid(&stream), Ok(uid) if uid == self.me) {
            let _ = reply_line(&stream, "refused: not your process");
            return;
        }
        // Nothing comes back from a client that gave up in the accept queue: it has not sent yet.
        if reply_line(&stream, READY).is_err() {
            return;
        }
        let line = match read_line(&stream, Instant::now() + self.waits.read, MAX_LINE) {
            Ok(line) => line,
            Err(LineFail::TooLong) => {
                let _ = reply_line(&stream, "refused: request too long");
                return;
            }
            Err(LineFail::Partial) => {
                let _ = reply_line(&stream, "refused: malformed request");
                return;
            }
            Err(LineFail::TimedOut) => {
                let _ = reply_line(&stream, "refused: request timed out");
                return;
            }
            Err(LineFail::Empty | LineFail::Io) => return,
        };
        match self.judge(&line) {
            Ok(request) => {
                // A client killed or interrupted since it sent this is no longer asking.
                if !client_left(&stream) {
                    let reply = self.hand_over(request, &stream);
                    let _ = reply_line(&stream, reply);
                }
            }
            Err(why) => {
                let _ = reply_line(&stream, &format!("refused: {why}"));
            }
        }
    }

    fn judge(&self, line: &[u8]) -> Result<Request, String> {
        let wire = parse(line)?;
        if wire.op == OP_RAISE {
            if Path::new(&wire.path) != self.root {
                return Err("the path is not this window's root".to_owned());
            }
            return Ok(Request::Raise { token: wire.token });
        }
        Ok(Request::Open(Open {
            path: validated_path(Path::new(&wire.path), &self.root)?,
            line: wire.line,
            col: wire.col,
            token: wire.token,
        }))
    }

    /// Gives `request` to the GTK side and returns the reply that settles it.
    fn hand_over(&self, request: Request, stream: &UnixStream) -> &'static str {
        // A request the GTK side could not check for a hangup might open after its client left.
        let Ok(client) = stream.try_clone() else {
            return "busy";
        };
        let owner = Arc::new(AtomicU8::new(WAITING));
        let (answer, answers) = mpsc::channel();
        let incoming = Incoming {
            request,
            owner: Arc::clone(&owner),
            answer,
            client,
        };
        if self.requests.send(incoming).is_err() {
            return "busy";
        }
        // A full buffer is a wake already pending, and the descriptor stays readable until a drain
        // that also finds this request.
        let _ = (&self.wake).write(&[1]);
        let settled = match answers.recv_timeout(self.waits.answer) {
            Err(RecvTimeoutError::Timeout) => {
                let abandoned = owner
                    .compare_exchange(WAITING, ABANDONED, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok();
                if abandoned {
                    return "busy";
                }
                answers.recv_timeout(self.waits.taken)
            }
            settled => settled,
        };
        match settled {
            Ok(Answer::Accepted) => "ok",
            // No answer: the controller has the request and it will run, so `busy` could open the
            // file twice, or start a second window of this root beside the one that comes forward.
            Err(RecvTimeoutError::Timeout) => "ok",
            // Dropped unanswered: the window is going.
            Ok(Answer::Busy) | Err(RecvTimeoutError::Disconnected) => "busy",
        }
    }
}

/// What a test can see of the thread.
#[cfg(test)]
#[derive(Default)]
struct ThreadWatch {
    entered: std::sync::atomic::AtomicBool,
    timeout_ms: std::sync::atomic::AtomicI32,
    polls: std::sync::atomic::AtomicU64,
    finished: std::sync::atomic::AtomicBool,
}

/// A test's look at the two moments in between: a claim's two looks at a socket, and a close's unlink
/// and stop.
#[cfg(test)]
mod test_hook {
    use std::cell::RefCell;

    thread_local! {
        static HOOK: RefCell<Option<Box<dyn FnMut()>>> = RefCell::new(None);
        pub(super) static REFUSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    pub(super) fn set(hook: Box<dyn FnMut()>) {
        HOOK.with(|cell| *cell.borrow_mut() = Some(hook));
    }

    pub(super) fn run() {
        let hook = HOOK.with(|cell| cell.borrow_mut().take());
        if let Some(mut hook) = hook {
            hook();
        }
    }
}

/// Whether a connect to `sock` is refused. A full accept queue can also refuse on macOS.
fn refused(sock: &Path) -> bool {
    #[cfg(test)]
    if test_hook::REFUSED.with(|forced| forced.get()) {
        return true;
    }
    nonblocking_connect_outcome(sock) == ConnectOutcome::NoListener
}

struct BoundSocket {
    listener: UnixListener,
    alive: PrivateLock,
}

/// The root's file stays in place across owners, so every claimant locks the same inode. A pid-named
/// one is removed by its holder's `close`; only this process ever claims that name, and its claims and
/// closes run one after another on one thread, so no claim of it can lock the file being removed.
fn liveness_lock(sock: &Path) -> Result<Option<PrivateLock>, String> {
    let path = sock.with_extension("alive");
    let lock = PrivateLock::open(&path).map_err(|e| format!("could not open {}: {e}", path.display()))?;
    match lock.try_lock() {
        Ok(true) => Ok(Some(lock)),
        Ok(false) => Ok(None),
        Err(e) => Err(format!("could not lock {}: {e}", path.display())),
    }
}

/// Replaces `sock` if it is a leftover; `None` when something else holds it. A link or a file that is
/// not this user's socket is an error: it is left, and a connect would go through the link. A socket
/// that goes away meanwhile was a window closing, and its name is free to bind.
fn reclaim(sock: &Path) -> Result<Option<BoundSocket>, String> {
    let probed = match sock.symlink_metadata() {
        Ok(meta) if meta.file_type().is_socket() && meta.uid() == euid() => (meta.dev(), meta.ino()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return try_bind(sock),
        _ => return Err(format!("{} is not this user's socket; not touching it", sock.display())),
    };
    // A missing file belongs to the older layout. Creating and holding it excludes new servers
    // while the two probes below still decide whether that older socket is stale.
    let Some(alive) = liveness_lock(sock)? else {
        return Ok(None);
    };
    if !refused(sock) {
        return Ok(None);
    }
    std::thread::sleep(RECHECK_AFTER);
    #[cfg(test)]
    test_hook::run();
    if !refused(sock) {
        return Ok(None);
    }
    match identity(sock) {
        Some(found) if found == probed => remove_own_socket(sock),
        Some(_) => return Ok(None),
        None => {}
    }
    bind_locked(sock, alive)
}

/// `bind`, never through a link at the name, with the name in use reported as `None`.
fn try_bind(sock: &Path) -> Result<Option<BoundSocket>, String> {
    let Some(alive) = liveness_lock(sock)? else {
        return Ok(None);
    };
    bind_locked(sock, alive)
}

fn bind_locked(sock: &Path, alive: PrivateLock) -> Result<Option<BoundSocket>, String> {
    match bind_unfollowed(sock) {
        Ok(listener) => Ok(Some(BoundSocket { listener, alive })),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => Ok(None),
        Err(e) => Err(format!("could not bind {}: {e}", sock.display())),
    }
}

/// Binds the root's socket, or this window's own when another window holds that one.
fn bind(dir: &Path, root: &Path) -> Result<(BoundSocket, PathBuf), String> {
    let sock = socket_path(dir, root).map_err(|e| e.to_string())?;
    if let Some(listener) = try_bind(&sock)? {
        return Ok((listener, sock));
    }
    let _lock = take_reclaim_lock(&lock_path(dir, root), LOCK_WAIT)?;
    // Whoever held the lock may have reclaimed the socket, or the window that had it may have closed.
    if let Some(listener) = try_bind(&sock)? {
        return Ok((listener, sock));
    }
    if let Some(listener) = reclaim(&sock)? {
        return Ok((listener, sock));
    }
    let own = pid_socket_path(dir, root).map_err(|e| e.to_string())?;
    if let Some(listener) = try_bind(&own)? {
        return Ok((listener, own));
    }
    // A pid comes round again, and a crash leaves this name behind.
    match reclaim(&own)? {
        Some(listener) => Ok((listener, own)),
        None => Err(format!("{} is held by a live window", own.display())),
    }
}

/// The GTK side of a claimed socket: its thread, and the requests the thread hands over. Dropping it
/// closes it.
pub struct OpenServer {
    socket: PathBuf,
    alive: Cell<Option<PrivateLock>>,
    /// The control directory, for removing a pid-named socket's `.alive` file on close.
    dir: std::fs::File,
    /// Whether `socket` is this window's pid-named one rather than the root's.
    pid_named: bool,
    /// `(device, inode)` of the socket this server bound, so a close leaves a newer window's alone.
    bound: (u64, u64),
    requests: Receiver<Incoming>,
    wake: UnixStream,
    /// The thread's end of the wake pair, kept open here as well: when the thread ends the read end
    /// must stay quiet, because a closed peer reads as readable to the watcher for good.
    _wake_writer: UnixStream,
    /// Closing it stops the thread; `None` once closed.
    stop: Cell<Option<UnixStream>>,
    #[cfg(test)]
    watch: Arc<ThreadWatch>,
}

impl OpenServer {
    /// Claims `root`'s socket in `dir`. An `Err` is a reason to log: the window runs without reuse.
    /// `root` must be canonical: a request's canonical path is compared with it by components, and
    /// its bytes make the key a client looks the socket up by.
    pub fn claim(dir: &Path, root: &Path) -> Result<OpenServer, String> {
        OpenServer::claim_with(dir, root, Waits::STANDARD)
    }

    fn claim_with(dir: &Path, root: &Path, waits: Waits) -> Result<OpenServer, String> {
        ensure_dir(dir)?;
        let dir_handle =
            agent::private_fs::open_dir(dir).map_err(|e| format!("could not open {}: {e}", dir.display()))?;
        let (bound_socket, socket) = bind(dir, root)?;
        let bound = identity(&socket).ok_or_else(|| format!("could not read {}", socket.display()))?;
        let pid_named = pid_socket_path(dir, root).is_ok_and(|own| own == socket);
        let started = OpenServer::start(dir_handle, bound_socket, socket.clone(), bound, pid_named, root, waits);
        if started.is_err() && identity(&socket) == Some(bound) {
            remove_own_socket(&socket);
        }
        started
    }

    fn start(
        dir: std::fs::File,
        bound_socket: BoundSocket,
        socket: PathBuf,
        bound: (u64, u64),
        pid_named: bool,
        root: &Path,
        waits: Waits,
    ) -> Result<OpenServer, String> {
        let BoundSocket { listener, alive } = bound_socket;
        let failed = |what: &str, e: io::Error| format!("could not {what} for {}: {e}", socket.display());
        listener
            .set_nonblocking(true)
            .map_err(|e| failed("set up the listener", e))?;
        #[cfg(test)]
        crate::panel_control::before_socket_chmod::run();
        // `bind` leaves the umask's mode on the file.
        make_control_socket_private(&dir, &socket, file_identity(bound)).map_err(|e| failed("set the mode", e))?;
        let (wake, wake_writer) = UnixStream::pair().map_err(|e| failed("make the wake socket", e))?;
        let (stop, thread_stop) = UnixStream::pair().map_err(|e| failed("make the stop socket", e))?;
        wake.set_nonblocking(true)
            .and_then(|()| wake_writer.set_nonblocking(true))
            .and_then(|()| stop.set_nonblocking(true))
            .map_err(|e| failed("set up the wake socket", e))?;
        let (requests_tx, requests) = mpsc::channel();
        #[cfg(test)]
        let watch = Arc::new(ThreadWatch::default());
        let serving = Serving {
            root: root.to_owned(),
            me: euid(),
            waits,
            requests: requests_tx,
            wake: wake_writer
                .try_clone()
                .map_err(|e| failed("clone the wake socket", e))?,
            #[cfg(test)]
            watch: Arc::clone(&watch),
        };
        std::thread::Builder::new()
            .name("eitri-open-socket".into())
            .spawn(move || {
                serving.run(&listener, &thread_stop);
                #[cfg(test)]
                serving.watch.finished.store(true, Ordering::SeqCst);
            })
            .map_err(|e| failed("start the thread", e))?;
        Ok(OpenServer {
            socket,
            alive: Cell::new(Some(alive)),
            dir,
            pid_named,
            bound,
            requests,
            wake,
            _wake_writer: wake_writer,
            stop: Cell::new(Some(stop)),
            #[cfg(test)]
            watch,
        })
    }

    /// The socket this window holds: the root's, or its own pid-named one.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Readable when requests wait.
    pub fn wake_fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }

    /// The requests that wait, oldest first. Never blocks.
    pub fn drain(&self) -> Vec<Incoming> {
        let mut bytes = [0u8; 64];
        let mut wake = &self.wake;
        // The thread sends before it writes the byte, so every byte read here has its request queued.
        loop {
            match wake.read(&mut bytes) {
                Ok(read) if read == bytes.len() => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                _ => break,
            }
        }
        self.requests.try_iter().collect()
    }

    /// Removes the socket if it is still the one bound, then stops the thread. The socket goes first,
    /// while the listener can still answer: a claimant probing the path finds a listener or nothing,
    /// never a refusal it would take for a leftover. A pid-named socket's `.alive` file goes next, while
    /// its lock is still held (only a window of this pid ever claims that name); the root's stays, the
    /// one inode every claimant of the root locks. Nothing waits for the thread, because this runs on
    /// the GTK thread: it ends within its own bounds. Idempotent.
    pub fn close(&self) {
        let Some(stop) = self.stop.take() else {
            return;
        };
        if identity(&self.socket) == Some(self.bound) {
            remove_own_socket(&self.socket);
        }
        let alive = self.alive.take();
        if let (true, Some(lock)) = (self.pid_named, &alive) {
            let path = self.socket.with_extension("alive");
            if let (Some(name), Ok(meta)) = (path.file_name(), lock.as_file().metadata()) {
                let _ = agent::private_fs::remove_if_same(&self.dir, name, file_identity((meta.dev(), meta.ino())));
            }
        }
        drop(alive);
        #[cfg(test)]
        test_hook::run();
        drop(stop);
        // Whoever waits on these gets `busy` now rather than at the thread's own bound.
        self.requests.try_iter().for_each(drop);
    }
}

impl Drop for OpenServer {
    fn drop(&mut self) {
        self.close();
    }
}

impl fmt::Debug for OpenServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenServer")
            .field("socket", &self.socket)
            .finish_non_exhaustive()
    }
}

/// What became of a client's targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivered {
    /// A window took every target. `root` is that window's when its own socket was the one asked; a hint can be any
    /// window's, and a client is not told which.
    Opened { root: Option<PathBuf> },
    /// A window took the one that decides, and these failed after it, with the reason.
    Partly { failed: Vec<(PathBuf, String)> },
    /// No window took anything: start a new one.
    NotHere,
}

/// Connects to `sock`, checks that the process behind it is `me`, waits to be served, sends `line` and
/// returns the reply line. `within` covers the wait to be served, and again the request and its reply:
/// a window can run a request once it is sent, so that is the point after which giving up costs a
/// second open.
fn exchange(sock: &Path, line: &str, me: u32, within: Duration) -> Result<String, String> {
    exchange_with(sock, line, me, within, &Instant::now, &mut read_line)
}

fn exchange_with(
    sock: &Path,
    line: &str,
    me: u32,
    within: Duration,
    now: &dyn Fn() -> Instant,
    read: &mut dyn FnMut(&UnixStream, Instant, usize) -> Result<Vec<u8>, LineFail>,
) -> Result<String, String> {
    let deadline = now() + within;
    // A connect to a listener whose accept queue is full blocks on Linux, so it runs on a helper
    // thread that is simply abandoned when the deadline passes; it owns nothing but its stream.
    let (tx, rx) = mpsc::channel();
    let target = sock.to_owned();
    std::thread::spawn(move || {
        let _ = tx.send(UnixStream::connect(target));
    });
    let stream = match rx.recv_timeout(within) {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => return Err(format!("could not connect to {}: {e}", sock.display())),
        Err(_) => return Err(format!("{} did not accept a connection in time", sock.display())),
    };
    // Before a byte goes out: the socket file can be swapped after its owner was checked.
    match peer_uid(&stream) {
        Ok(uid) if uid == me => {}
        Ok(_) => return Err(format!("{} is served by another user", sock.display())),
        Err(e) => return Err(format!("could not read the peer of {}: {e}", sock.display())),
    }
    let greeting = read(&stream, deadline, MAX_LINE).map_err(|why| format!("not served ({why:?})"))?;
    if greeting != READY.as_bytes() {
        return Err(format!("{} did not say it was ready", sock.display()));
    }
    stream
        .set_write_timeout(Some(within))
        .and_then(|()| (&stream).write_all(line.as_bytes()))
        .map_err(|e| format!("could not send to {}: {e}", sock.display()))?;
    let reply = read(&stream, now() + within, MAX_LINE).map_err(|why| format!("no reply ({why:?})"))?;
    Ok(String::from_utf8_lossy(&reply).into_owned())
}

/// Sends one target to `sock`: `Ok` when the window accepted it, else why not.
fn send(sock: &Path, target: &OpenTarget, token: Option<&str>, me: u32, within: Duration) -> Result<(), String> {
    validate_nvim_addr(sock, me)?;
    let path = target
        .path
        .to_str()
        .ok_or_else(|| format!("{} is not valid UTF-8", target.path.display()))?;
    let wire = Wire {
        v: PROTOCOL_OPEN,
        op: OP_OPEN.to_owned(),
        path: path.to_owned(),
        line: target.line,
        col: target.col,
        token: token.map(str::to_owned),
    };
    let line = serde_json::to_string(&wire).map_err(|e| e.to_string())?;
    let reply = exchange(sock, &format!("{line}\n"), me, within)?;
    match reply.as_str() {
        "ok" => Ok(()),
        "busy" => Err(reply),
        other => Err(other.strip_prefix("refused: ").unwrap_or(other).to_owned()),
    }
}

/// Tries to give `open` to a running window: `hint` first, then the socket of `root`, then those of `wider`, nearest
/// first, each only when every target is inside that root. Targets go last to first, one connection each, so the first
/// ends up shown. Every root must be canonical, as for [`OpenServer::claim`].
pub fn deliver(
    open: &[OpenTarget],
    root: &Path,
    wider: &[PathBuf],
    dir: &Path,
    hint: Option<&Path>,
    token: Option<&str>,
) -> Delivered {
    deliver_with(open, root, wider, dir, hint, token, euid(), CLIENT_DEADLINE)
}

#[allow(clippy::too_many_arguments)]
fn deliver_with(
    open: &[OpenTarget],
    root: &Path,
    wider: &[PathBuf],
    dir: &Path,
    hint: Option<&Path>,
    token: Option<&str>,
    me: u32,
    within: Duration,
) -> Delivered {
    let Some((last, rest)) = open.split_last() else {
        return Delivered::NotHere;
    };
    let mut candidates: Vec<(PathBuf, Option<&Path>)> = hint.map(|hint| (hint.to_owned(), None)).into_iter().collect();
    let roots = std::iter::once(root).chain(wider.iter().map(PathBuf::as_path));
    for root in roots.filter(|root| open.iter().all(|target| target.path.starts_with(root))) {
        let Ok(sock) = socket_path(dir, root) else { continue };
        match candidates.iter_mut().find(|(asked, _)| *asked == sock) {
            Some(already) => already.1 = Some(root),
            None => candidates.push((sock, Some(root))),
        }
    }
    for (sock, root) in candidates {
        // The window that takes this one has opened something, so from here on there is no new
        // window to fall back to.
        if send(&sock, last, token, me, within).is_err() {
            continue;
        }
        let failed: Vec<(PathBuf, String)> = rest
            .iter()
            .rev()
            .filter_map(|target| {
                let why = send(&sock, target, token, me, within).err()?;
                Some((target.path.clone(), why))
            })
            .collect();
        return if failed.is_empty() {
            Delivered::Opened {
                root: root.map(Path::to_owned),
            }
        } else {
            Delivered::Partly { failed }
        };
    }
    Delivered::NotHere
}

/// What became of a raise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Raised {
    /// The window of the root took it and is coming forward.
    Done,
    /// No window of that root will: none runs, it is ending, it is of a build that has no raise, or it did not answer
    /// in time. The launch becomes that window.
    NotHere,
}

/// Asks the window of `root` to come forward. `root` must be canonical, as for [`OpenServer::claim`]. Only the root's
/// own socket is asked: a window that found it taken is not looked up by root, for a raise as for a file.
pub fn raise(root: &Path, dir: &Path, token: Option<&str>) -> Raised {
    raise_with(root, dir, token, euid(), CLIENT_DEADLINE)
}

fn raise_with(root: &Path, dir: &Path, token: Option<&str>, me: u32, within: Duration) -> Raised {
    match ask_to_raise(root, dir, token, me, within).as_deref() {
        Ok("ok") => Raised::Done,
        _ => Raised::NotHere,
    }
}

/// The reply of the window of `root` to a raise. A root that is not text cannot be named on the wire.
fn ask_to_raise(root: &Path, dir: &Path, token: Option<&str>, me: u32, within: Duration) -> Result<String, String> {
    let sock = socket_path(dir, root).map_err(|e| e.to_string())?;
    validate_nvim_addr(&sock, me)?;
    let path = root
        .to_str()
        .ok_or_else(|| format!("{} is not valid UTF-8", root.display()))?;
    let wire = Wire {
        v: PROTOCOL_RAISE,
        op: OP_RAISE.to_owned(),
        path: path.to_owned(),
        line: None,
        col: None,
        token: token.map(str::to_owned),
    };
    let line = serde_json::to_string(&wire).map_err(|e| e.to_string())?;
    exchange(&sock, &format!("{line}\n"), me, within)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel_control::ensure_dir;
    use crate::project_root::LAST_LINE;
    use crate::test_scratch_dir::ScratchDir;
    use serde_json::json;
    use std::io::BufRead;
    use std::os::unix::fs::{symlink, MetadataExt};
    use std::sync::atomic::Ordering;
    use std::thread::JoinHandle;

    /// How long any one step may take on a loaded machine; a test that passes never waits it out.
    const PATIENCE: Duration = Duration::from_secs(20);

    struct Fixture {
        _scratch: ScratchDir,
        base: PathBuf,
        dir: PathBuf,
        root: PathBuf,
    }

    /// A control directory and a canonical root side by side, with short names: a pid-named socket
    /// has to stay under the socket path limit inside them.
    fn fixture() -> Fixture {
        let scratch = ScratchDir::new("eo", "s");
        let base = scratch.canonicalize().unwrap();
        let root = base.join("root");
        std::fs::create_dir(&root).unwrap();
        let dir = base.join("run");
        ensure_dir(&dir).unwrap();
        Fixture {
            _scratch: scratch,
            base,
            dir,
            root,
        }
    }

    impl Fixture {
        fn server(&self) -> OpenServer {
            self.server_with(Waits::STANDARD)
        }

        fn server_with(&self, waits: Waits) -> OpenServer {
            OpenServer::claim_with(&self.dir, &self.root, waits).unwrap()
        }

        fn file(&self, name: &str) -> PathBuf {
            let path = self.root.join(name);
            std::fs::write(&path, "text\n").unwrap();
            path
        }

        fn outside(&self, name: &str) -> PathBuf {
            let path = self.base.join(name);
            std::fs::write(&path, "text\n").unwrap();
            path
        }

        /// What a client hears when the window of this fixture's root takes everything.
        fn opened(&self) -> Delivered {
            Delivered::Opened {
                root: Some(self.root.clone()),
            }
        }

        /// A folder under the root, for a launch whose root is narrower than a running window's.
        fn below(&self, name: &str) -> PathBuf {
            let path = self.root.join(name);
            std::fs::create_dir(&path).unwrap();
            path
        }

        fn root_socket(&self) -> PathBuf {
            socket_path(&self.dir, &self.root).unwrap()
        }

        fn pid_socket(&self) -> PathBuf {
            pid_socket_path(&self.dir, &self.root).unwrap()
        }
    }

    fn request(path: &Path) -> String {
        json!({ "v": 1, "op": "open", "path": path }).to_string()
    }

    /// A new connection the thread has turned to.
    fn served(sock: &Path) -> UnixStream {
        let stream = UnixStream::connect(sock).unwrap();
        stream.set_read_timeout(Some(PATIENCE)).unwrap();
        assert_eq!(read_reply(&stream), READY);
        stream
    }

    /// Sends one line on a new connection and reads the reply, if the server gives one.
    fn ask(sock: &Path, line: &str) -> String {
        let mut stream = served(sock);
        // A refused oversize line may be answered before it has all been written.
        let _ = stream.write_all(format!("{line}\n").as_bytes());
        read_reply(&stream)
    }

    fn read_reply(stream: &UnixStream) -> String {
        let mut reply = String::new();
        let _ = io::BufReader::new(stream).read_line(&mut reply);
        reply.trim_end().to_owned()
    }

    fn ask_later(sock: &Path, line: String) -> JoinHandle<String> {
        let sock = sock.to_owned();
        std::thread::spawn(move || ask(&sock, &line))
    }

    fn readable(fd: RawFd, within: Duration) -> bool {
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid `pollfd`, and the count says one.
        unsafe { libc::poll(&mut pollfd, 1, within.as_millis() as libc::c_int) > 0 }
    }

    /// What the GTK side does: wait for the wake descriptor, then drain.
    fn next_request(server: &OpenServer) -> Incoming {
        assert!(
            readable(server.wake_fd(), PATIENCE),
            "no request reached the wake socket"
        );
        let mut waiting = server.drain();
        assert_eq!(waiting.len(), 1, "the thread serves one connection at a time");
        waiting.remove(0)
    }

    /// The file a request names; a test that reads one has sent an open.
    fn file_of(request: Request) -> Open {
        match request {
            Request::Open(open) => open,
            Request::Raise { .. } => panic!("a raise where an open was sent"),
        }
    }

    /// `line` through a fresh server, accepted: what the window got and what the client read.
    fn handed_over(fixture: &Fixture, line: String) -> (Open, String) {
        let server = fixture.server();
        let client = ask_later(server.socket(), line);
        let (request, answering) = next_request(&server).take().expect("nobody gave up on it");
        answering.answer(Answer::Accepted);
        (file_of(request), client.join().unwrap())
    }

    /// The reply to `line`, which must not reach the window.
    fn refusal(server: &OpenServer, line: &str) -> String {
        let reply = ask(server.socket(), line);
        assert!(
            server.drain().is_empty(),
            "a refused request reached the window: {line}"
        );
        reply
    }

    fn wait_until(what: &str, done: impl Fn() -> bool) {
        let give_up = Instant::now() + PATIENCE;
        while !done() {
            assert!(Instant::now() < give_up, "never happened: {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Waits until `sock`'s liveness lock can be taken. A child forked by another test keeps a dropped
    /// holder's descriptor until it execs, so a lock just let go can look held for a moment; one that
    /// was never let go is still held at the end of the wait. The probe is unlocked before it goes, so
    /// a fork's copy of the probe cannot hold the lock past its drop either.
    fn liveness_comes_free(sock: &Path) {
        let give_up = Instant::now() + PATIENCE;
        let probe = loop {
            if let Some(lock) = liveness_lock(sock).unwrap() {
                break lock;
            }
            assert!(Instant::now() < give_up, "never happened: the liveness lock comes free");
            std::thread::sleep(Duration::from_millis(5));
        };
        loop {
            // SAFETY: the lock owns the descriptor for this call; flock takes no pointers.
            if unsafe { libc::flock(probe.as_file().as_raw_fd(), libc::LOCK_UN) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            assert_eq!(error.kind(), std::io::ErrorKind::Interrupted, "{error}");
        }
    }

    #[test]
    fn an_open_inside_the_root_is_handed_over_canonically() {
        let fixture = fixture();
        let file = fixture.file("a.rs");
        std::fs::create_dir(fixture.root.join("sub")).unwrap();
        symlink(&file, fixture.root.join("link")).unwrap();
        let line = json!({
            "v": 1,
            "op": "open",
            "path": fixture.root.join("sub/../link"),
            "line": 42,
            "col": 7,
            "token": "from the launcher",
        });
        let (open, reply) = handed_over(&fixture, line.to_string());
        assert_eq!(reply, "ok");
        assert_eq!(
            open,
            Open {
                path: file,
                line: Some(42),
                col: Some(7),
                token: Some("from the launcher".to_owned()),
            }
        );
    }

    #[test]
    fn the_last_line_of_a_bare_plus_is_carried_as_it_is() {
        let fixture = fixture();
        let file = fixture.file("a.rs");
        let line = json!({ "v": 1, "op": "open", "path": file, "line": LAST_LINE });
        let (open, _) = handed_over(&fixture, line.to_string());
        assert_eq!(open.line, Some(LAST_LINE));
    }

    #[test]
    fn the_wake_descriptor_is_quiet_once_drained() {
        let fixture = fixture();
        let file = fixture.file("a.rs");
        let server = fixture.server();
        assert!(!readable(server.wake_fd(), Duration::ZERO));
        let client = ask_later(server.socket(), request(&file));
        let incoming = next_request(&server);
        assert!(!readable(server.wake_fd(), Duration::ZERO));
        drop(incoming);
        assert_eq!(client.join().unwrap(), "busy");
    }

    #[test]
    fn a_new_file_in_an_existing_directory_is_handed_over() {
        let fixture = fixture();
        std::fs::create_dir(fixture.root.join("sub")).unwrap();
        let new = fixture.root.join("sub/new file.rs");
        let (open, reply) = handed_over(&fixture, request(&new));
        assert_eq!(reply, "ok");
        assert_eq!(open.path, new);
        assert!(!new.exists(), "validating a new file does not create it");
    }

    #[test]
    fn outside_the_root_is_refused() {
        let fixture = fixture();
        let server = fixture.server();
        let outside = fixture.outside("elsewhere.rs");
        // Inside by bytes, outside by components.
        let sibling = fixture.base.join("root-2");
        std::fs::create_dir(&sibling).unwrap();
        std::fs::write(sibling.join("a.rs"), "text\n").unwrap();
        for path in [
            outside,
            sibling.join("a.rs"),
            sibling.join("new.rs"),
            fixture.root.join("../elsewhere.rs"),
        ] {
            assert_eq!(
                refusal(&server, &request(&path)),
                "refused: the path is outside the root",
                "{}",
                path.display()
            );
        }
    }

    #[test]
    fn a_symlink_out_of_the_root_is_refused() {
        let fixture = fixture();
        let server = fixture.server();
        let outside = fixture.outside("secret");
        symlink(&outside, fixture.root.join("out")).unwrap();
        symlink(&fixture.base, fixture.root.join("up")).unwrap();
        for path in [
            fixture.root.join("out"),
            fixture.root.join("up/secret"),
            fixture.root.join("up/new.rs"),
        ] {
            assert_eq!(
                refusal(&server, &request(&path)),
                "refused: the path is outside the root",
                "{}",
                path.display()
            );
        }
    }

    #[test]
    fn a_dangling_link_is_refused() {
        let fixture = fixture();
        let server = fixture.server();
        // Writing through this link would create a file, even outside the root.
        symlink(fixture.base.join("not there"), fixture.root.join("dangling")).unwrap();
        assert_eq!(
            refusal(&server, &request(&fixture.root.join("dangling"))),
            "refused: the path is a dangling link"
        );
    }

    #[test]
    fn a_directory_is_refused() {
        let fixture = fixture();
        let server = fixture.server();
        std::fs::create_dir(fixture.root.join("sub")).unwrap();
        for path in [fixture.root.join("sub"), fixture.root.clone()] {
            assert_eq!(
                refusal(&server, &request(&path)),
                "refused: the path is not a regular file"
            );
        }
    }

    #[test]
    fn a_new_file_needs_a_parent_that_is_a_directory() {
        let fixture = fixture();
        let server = fixture.server();
        let file = fixture.file("a.rs");
        for path in [fixture.root.join("missing/new.rs"), file.join("new.rs")] {
            assert!(
                refusal(&server, &request(&path)).starts_with("refused: "),
                "{}",
                path.display()
            );
            assert!(!path.exists());
        }
    }

    #[test]
    fn a_name_that_can_only_be_a_directory_is_refused() {
        let fixture = fixture();
        let server = fixture.server();
        for tail in ["new/", "new/.", "new/.."] {
            let line = request(Path::new(&format!("{}/{tail}", fixture.root.display())));
            assert_eq!(refusal(&server, &line), "refused: the path names no file", "{tail}");
        }
    }

    #[test]
    fn a_relative_path_is_refused() {
        let fixture = fixture();
        let server = fixture.server();
        fixture.file("a.rs");
        assert_eq!(
            refusal(&server, &request(Path::new("a.rs"))),
            "refused: the path is not absolute"
        );
    }

    #[test]
    fn a_path_with_a_nul_is_refused() {
        let fixture = fixture();
        let server = fixture.server();
        let line = json!({ "v": 1, "op": "open", "path": format!("{}/a\0b", fixture.root.display()) });
        assert_eq!(refusal(&server, &line.to_string()), "refused: the path cannot be read");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_link_to_a_name_that_is_not_text_is_refused() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let fixture = fixture();
        let server = fixture.server();
        let bytes = fixture.root.join(OsStr::from_bytes(b"not-text-\xff"));
        std::fs::write(&bytes, "text\n").unwrap();
        symlink(&bytes, fixture.root.join("link")).unwrap();
        assert_eq!(
            refusal(&server, &request(&fixture.root.join("link"))),
            "refused: the path is not valid UTF-8"
        );
    }

    #[test]
    fn oversize_slow_and_malformed_requests_are_refused() {
        let fixture = fixture();
        let file = fixture.file("a.rs");
        let server = fixture.server_with(Waits {
            read: Duration::from_millis(100),
            ..Waits::STANDARD
        });
        assert_eq!(refusal(&server, &"x".repeat(MAX_LINE + 1)), "refused: request too long");
        for line in [
            "not json".to_owned(),
            "[]".to_owned(),
            json!({ "v": 1, "op": "open" }).to_string(),
            json!({ "op": "open", "path": file }).to_string(),
        ] {
            assert_eq!(refusal(&server, &line), "refused: malformed request", "{line}");
        }
        assert_eq!(
            refusal(&server, &json!({ "v": 3, "op": "open", "path": file }).to_string()),
            "refused: unsupported protocol version 3"
        );
        assert_eq!(
            refusal(&server, &json!({ "v": 1, "op": "close", "path": file }).to_string()),
            "refused: unsupported op"
        );

        // Half a line, then the sender goes.
        let stream = served(server.socket());
        (&stream).write_all(b"{\"v\":1").unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        assert_eq!(read_reply(&stream), "refused: malformed request");

        // Half a line, then the sender stays silent past the deadline.
        let stream = served(server.socket());
        (&stream).write_all(b"{\"v\":1").unwrap();
        assert_eq!(read_reply(&stream), "refused: request timed out");
        assert!(server.drain().is_empty());
    }

    #[test]
    fn an_unknown_key_is_refused() {
        let fixture = fixture();
        let file = fixture.file("a.rs");
        let server = fixture.server();
        let line = json!({ "v": 1, "op": "open", "path": file, "run": "!sh" });
        assert_eq!(refusal(&server, &line.to_string()), "refused: malformed request");
    }

    #[test]
    fn a_negative_or_fractional_line_is_refused() {
        let fixture = fixture();
        let file = fixture.file("a.rs");
        let server = fixture.server();
        for line in [
            json!({ "v": 1, "op": "open", "path": file, "line": -1 }),
            json!({ "v": 1, "op": "open", "path": file, "line": 1.5 }),
            json!({ "v": 1, "op": "open", "path": file, "line": 4294967296u64 }),
            json!({ "v": 1, "op": "open", "path": file, "col": -1 }),
            json!({ "v": 1, "op": "open", "path": file, "col": 1.5 }),
        ] {
            assert_eq!(
                refusal(&server, &line.to_string()),
                "refused: malformed request",
                "{line}"
            );
        }
    }

    #[test]
    fn a_token_over_512_bytes_is_refused() {
        let fixture = fixture();
        let file = fixture.file("a.rs");
        let longest = json!({ "v": 1, "op": "open", "path": file, "token": "t".repeat(MAX_TOKEN) });
        let (open, reply) = handed_over(&fixture, longest.to_string());
        assert_eq!(reply, "ok");
        assert_eq!(open.token.map(|token| token.len()), Some(512));

        let server = fixture.server();
        let over = json!({ "v": 1, "op": "open", "path": file, "token": "t".repeat(MAX_TOKEN + 1) });
        assert_eq!(refusal(&server, &over.to_string()), "refused: token too long");
    }

    #[test]
    fn a_peer_of_another_uid_is_refused() {
        let fixture = fixture();
        let file = fixture.file("a.rs");
        let (requests, waiting) = mpsc::channel();
        let (wake, _quiet) = UnixStream::pair().unwrap();
        let serving = Serving {
            root: fixture.root.clone(),
            me: euid().wrapping_add(1),
            waits: Waits::STANDARD,
            requests,
            wake,
            watch: Arc::default(),
        };
        let (client, served) = UnixStream::pair().unwrap();
        (&client).write_all(format!("{}\n", request(&file)).as_bytes()).unwrap();
        serving.serve_one(served);
        assert_eq!(read_reply(&client), "refused: not your process");
        assert!(waiting.try_recv().is_err());
    }

    #[test]
    fn no_answer_within_two_seconds_replies_busy_and_a_late_take_is_none() {
        let fixture = fixture();
        let file = fixture.file("a.rs");
        let server = fixture.server_with(Waits {
            answer: Duration::from_millis(100),
            ..Waits::STANDARD
        });
        // Nothing on this side moves until the thread has given up.
        assert_eq!(ask(server.socket(), &request(&file)), "busy");
        assert!(next_request(&server).take().is_none());
    }

    #[test]
    fn a_dropped_incoming_answers_busy() {
        let fixture = fixture();
        let file = fixture.file("a.rs");
        let server = fixture.server();
        let client = ask_later(server.socket(), request(&file));
        drop(next_request(&server));
        assert_eq!(client.join().unwrap(), "busy");
    }

    #[test]
    fn an_answered_busy_replies_busy() {
        let fixture = fixture();
        let file = fixture.file("a.rs");
        let server = fixture.server();
        let client = ask_later(server.socket(), request(&file));
        let (_, answering) = next_request(&server).take().unwrap();
        answering.answer(Answer::Busy);
        assert_eq!(client.join().unwrap(), "busy");
    }

    #[test]
    fn a_taken_request_with_no_answer_replies_ok() {
        let fixture = fixture();
        let file = fixture.file("a.rs");
        let server = fixture.server_with(Waits {
            answer: Duration::from_secs(1),
            taken: Duration::from_millis(50),
            ..Waits::STANDARD
        });
        let client = ask_later(server.socket(), request(&file));
        // Held to the end: the controller has the request and has not answered yet.
        let _taken = next_request(&server).take().expect("taken before the wait ran out");
        assert_eq!(client.join().unwrap(), "ok");
    }

    #[test]
    fn an_answering_dropped_after_the_take_replies_busy() {
        let fixture = fixture();
        let file = fixture.file("a.rs");
        let server = fixture.server();
        let client = ask_later(server.socket(), request(&file));
        drop(next_request(&server).take().unwrap());
        assert_eq!(client.join().unwrap(), "busy");
    }

    #[test]
    fn a_queued_request_whose_client_left_is_dropped() {
        let fixture = fixture();
        let (served, left, last) = (fixture.file("a"), fixture.file("b"), fixture.file("c"));
        let server = fixture.server();
        let client = ask_later(server.socket(), request(&served));
        // The thread now waits on the first request, so the next connection stays in the accept queue.
        let first = next_request(&server);
        // Half closed and not closed, so `ready` goes out and it is the hangup check that drops this.
        let gone = UnixStream::connect(server.socket()).unwrap();
        (&gone).write_all(format!("{}\n", request(&left)).as_bytes()).unwrap();
        gone.shutdown(std::net::Shutdown::Write).unwrap();
        let waiting = ask_later(server.socket(), request(&last));
        let (_, answering) = first.take().unwrap();
        answering.answer(Answer::Accepted);
        assert_eq!(client.join().unwrap(), "ok");
        let (request, answering) = next_request(&server).take().unwrap();
        assert_eq!(
            file_of(request).path,
            last,
            "the request of the client that left is skipped"
        );
        answering.answer(Answer::Accepted);
        assert_eq!(waiting.join().unwrap(), "ok");
    }

    #[test]
    fn a_client_that_left_after_the_hand_over_cannot_be_taken() {
        let fixture = fixture();
        let file = fixture.file("a.rs");
        // The thread outwaits the hangup below: a request it had given up on would answer `None` for that reason instead.
        let server = fixture.server_with(Waits {
            answer: PATIENCE * 2,
            ..Waits::STANDARD
        });
        let client = UnixStream::connect(server.socket()).unwrap();
        (&client).write_all(format!("{}\n", request(&file)).as_bytes()).unwrap();
        let incoming = next_request(&server);
        drop(client);
        // A child another test thread is starting holds a copy of every descriptor until it execs, so the hangup comes
        // when the last copy of the client's end closes.
        assert!(readable(incoming.client.as_raw_fd(), PATIENCE), "the hangup never came");
        assert_eq!(
            incoming.owner.load(Ordering::SeqCst),
            WAITING,
            "still the taker's to take"
        );
        assert!(incoming.take().is_none(), "its client is on its way to a new window");
    }

    #[test]
    fn the_client_outlasts_the_server() {
        assert_eq!(READ_WITHIN, Duration::from_secs(1));
        assert_eq!(ANSWER_WITHIN, Duration::from_secs(2));
        assert_eq!(TAKEN_WITHIN, Duration::from_secs(1));
        assert_eq!(CLIENT_DEADLINE, Duration::from_millis(4500));
        assert!(CLIENT_DEADLINE > READ_WITHIN + ANSWER_WITHIN + TAKEN_WITHIN);
    }

    #[test]
    fn a_claim_waits_at_most_a_tenth_of_a_second() {
        assert!(LOCK_WAIT + RECHECK_AFTER * 2 <= Duration::from_millis(100));
    }

    #[test]
    fn a_claim_behind_a_held_lock_gives_up_and_removes_nothing() {
        let fixture = fixture();
        let sock = fixture.root_socket();
        drop(UnixListener::bind(&sock).unwrap());
        let _held = take_reclaim_lock(&lock_path(&fixture.dir, &fixture.root), LOCK_WAIT).unwrap();
        let why = OpenServer::claim(&fixture.dir, &fixture.root).unwrap_err();
        assert!(why.contains("not reclaiming"), "{why}");
        assert!(sock.symlink_metadata().is_ok());
    }

    #[test]
    fn a_stale_socket_is_reclaimed() {
        let fixture = fixture();
        let file = fixture.file("a.rs");
        drop(UnixListener::bind(fixture.root_socket()).unwrap());
        let before = identity(&fixture.root_socket()).unwrap();
        // A second name keeps the stale socket's inode alive, so the file system cannot hand the same number to the
        // new one (ext4 reuses a freed inode in the same directory at once) and a reclaim always shows.
        let kept = fixture.dir.join("stale-socket-kept");
        std::fs::hard_link(fixture.root_socket(), &kept).unwrap();
        let server = fixture.server();
        assert_eq!(server.socket(), fixture.root_socket());
        assert_ne!(identity(&fixture.root_socket()).unwrap(), before);
        let _ = std::fs::remove_file(&kept);
        let client = ask_later(server.socket(), request(&file));
        let (_, answering) = next_request(&server).take().unwrap();
        answering.answer(Answer::Accepted);
        assert_eq!(client.join().unwrap(), "ok");
    }

    #[test]
    fn a_socket_that_comes_alive_during_the_recheck_is_kept() {
        let fixture = fixture();
        let sock = fixture.root_socket();
        drop(UnixListener::bind(&sock).unwrap());
        let live = std::rc::Rc::new(std::cell::RefCell::new(None));
        let (hook_live, hook_sock) = (live.clone(), sock.clone());
        test_hook::set(Box::new(move || {
            std::fs::remove_file(&hook_sock).unwrap();
            *hook_live.borrow_mut() = Some(UnixListener::bind(&hook_sock).unwrap());
        }));
        let server = fixture.server();
        assert_eq!(server.socket(), fixture.pid_socket());
        assert!(live.borrow().is_some(), "the recheck did not run");
        assert!(UnixStream::connect(&sock).is_ok(), "the live socket was removed");
    }

    #[test]
    fn a_socket_that_vanishes_during_the_recheck_leaves_its_name_to_the_claim() {
        let fixture = fixture();
        let sock = fixture.root_socket();
        drop(UnixListener::bind(&sock).unwrap());
        let hook_sock = sock.clone();
        test_hook::set(Box::new(move || std::fs::remove_file(&hook_sock).unwrap()));
        let server = fixture.server();
        assert_eq!(
            server.socket(),
            sock,
            "a window that closed leaves the root's name free"
        );
    }

    #[test]
    fn a_socket_gone_by_the_first_look_leaves_its_name_to_the_claim() {
        let fixture = fixture();
        let sock = fixture.root_socket();
        let listener = reclaim(&sock).unwrap();
        assert!(listener.is_some());
        assert!(UnixStream::connect(&sock).is_ok());
    }

    #[test]
    fn a_live_socket_is_left_and_the_claim_gets_its_own_pid_name() {
        let fixture = fixture();
        let file = fixture.file("a.rs");
        let first = fixture.server();
        let second = fixture.server();
        assert_eq!(first.socket(), fixture.root_socket());
        assert_eq!(second.socket(), fixture.pid_socket());
        for server in [&first, &second] {
            let meta = server.socket().symlink_metadata().unwrap();
            assert_eq!(meta.mode() & 0o777, 0o600);
            let client = ask_later(server.socket(), request(&file));
            let (_, answering) = next_request(server).take().unwrap();
            answering.answer(Answer::Accepted);
            assert_eq!(client.join().unwrap(), "ok");
        }
    }

    #[test]
    fn refused_probes_do_not_reclaim_a_live_servers_root_or_pid_socket() {
        let fixture = fixture();
        let first = fixture.server();
        let first_bound = identity(first.socket());
        // Force the refusal a full accept queue returns on macOS, independent of scheduling.
        test_hook::REFUSED.with(|forced| forced.set(true));
        let second = OpenServer::claim(&fixture.dir, &fixture.root);
        test_hook::REFUSED.with(|forced| forced.set(false));
        let second = second.unwrap();
        assert_eq!(second.socket(), fixture.pid_socket());
        assert_eq!(identity(first.socket()), first_bound);
        let second_bound = identity(second.socket());
        test_hook::REFUSED.with(|forced| forced.set(true));
        let third = OpenServer::claim(&fixture.dir, &fixture.root);
        test_hook::REFUSED.with(|forced| forced.set(false));
        assert!(third.is_err());
        assert_eq!(identity(first.socket()), first_bound);
        assert_eq!(identity(second.socket()), second_bound);
    }

    #[test]
    fn a_dead_holders_socket_with_a_released_liveness_lock_is_reclaimed() {
        let fixture = fixture();
        let sock = fixture.root_socket();
        let alive = liveness_lock(&sock).unwrap().unwrap();
        drop(UnixListener::bind(&sock).unwrap());
        // A second name prevents the reclaimed inode from being reused for the new socket.
        let kept = fixture.dir.join("dead-socket-kept");
        std::fs::hard_link(&sock, &kept).unwrap();
        let before = identity(&sock);
        drop(alive);
        liveness_comes_free(&sock);
        let server = fixture.server();
        assert_eq!(server.socket(), sock);
        assert_ne!(identity(&sock), before);
    }

    #[test]
    fn the_liveness_lock_is_private_exclusive_and_reusable_after_release() {
        let fixture = fixture();
        let sock = fixture.root_socket();
        let alive = liveness_lock(&sock).unwrap().unwrap();
        assert_eq!(sock.with_extension("alive").metadata().unwrap().mode() & 0o777, 0o600);
        assert!(liveness_lock(&sock).unwrap().is_none());
        drop(alive);
        liveness_comes_free(&sock);
        assert!(
            liveness_lock(&sock).unwrap().is_some(),
            "the next claimant can take the lock"
        );
    }

    #[test]
    fn a_dead_socket_whose_liveness_lock_is_held_is_not_reclaimed() {
        let fixture = fixture();
        let sock = fixture.root_socket();
        drop(UnixListener::bind(&sock).unwrap());
        let before = identity(&sock).unwrap();
        let held = liveness_lock(&sock).unwrap().expect("nobody holds it yet");
        let server = fixture.server();
        assert_eq!(server.socket(), fixture.pid_socket());
        assert_eq!(identity(&sock), Some(before));
        // Reclaim checks liveness itself, whatever its two connects say.
        assert!(reclaim(&sock).unwrap().is_none());
        assert_eq!(identity(&sock), Some(before));
        drop(held);
    }

    #[test]
    fn a_link_at_a_liveness_file_is_refused_without_touching_its_target() {
        let fixture = fixture();
        let target = fixture.outside("target");
        for sock in [fixture.root_socket(), fixture.pid_socket()] {
            symlink(&target, sock.with_extension("alive")).unwrap();
            assert!(liveness_lock(&sock).is_err());
            assert_eq!(std::fs::read(&target).unwrap(), b"text\n");
        }
    }

    #[test]
    fn a_server_holds_its_liveness_lock_until_close() {
        let fixture = fixture();
        let server = fixture.server();
        assert_eq!(server.socket(), fixture.root_socket());
        let other = PrivateLock::open(&server.socket().with_extension("alive")).unwrap();
        let meta = other.as_file().metadata().unwrap();
        assert!(meta.is_file());
        assert_eq!(meta.mode() & 0o777, 0o600);
        // SAFETY: the lock's file owns this descriptor for the call; F_GETFD takes no pointer.
        let flags = unsafe { libc::fcntl(other.as_file().as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
        assert!(!other.try_lock().unwrap(), "the open server holds its lock");
        drop(other);
        server.close();
        liveness_comes_free(server.socket());
        server.close();
        assert!(!server.socket().exists());
    }

    #[test]
    fn a_closed_server_leaves_the_root_socket_free_before_it_is_dropped() {
        let fixture = fixture();
        let first = fixture.server();
        assert_eq!(first.socket(), fixture.root_socket());
        first.close();
        liveness_comes_free(first.socket());

        let second = fixture.server();
        assert_eq!(second.socket(), fixture.root_socket());
        first.close();
        assert!(UnixStream::connect(second.socket()).is_ok());
    }

    #[test]
    fn a_leftover_pid_named_socket_is_reclaimed() {
        let fixture = fixture();
        let first = fixture.server();
        drop(UnixListener::bind(fixture.pid_socket()).unwrap());
        let second = fixture.server();
        assert_eq!(second.socket(), fixture.pid_socket());
        drop(first);
    }

    #[test]
    fn a_link_put_at_the_name_between_bind_and_chmod_is_refused_and_its_target_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = fixture();
        let target = fixture.outside("target");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        let sock = fixture.root_socket();
        let (planted_sock, planted_target) = (sock.clone(), target.clone());
        crate::panel_control::before_socket_chmod::set(Box::new(move || {
            std::fs::remove_file(&planted_sock).unwrap();
            symlink(&planted_target, &planted_sock).unwrap();
        }));
        let claimed = OpenServer::claim(&fixture.dir, &fixture.root);
        assert_eq!(target.metadata().unwrap().mode() & 0o777, 0o640);
        assert!(claimed.is_err());
        assert!(sock.symlink_metadata().unwrap().file_type().is_symlink());
    }

    #[test]
    fn closing_a_pid_named_server_removes_its_alive_file_and_keeps_the_roots() {
        let fixture = fixture();
        let first = fixture.server();
        let second = fixture.server();
        assert_eq!(second.socket(), fixture.pid_socket());
        let pid_alive = fixture.pid_socket().with_extension("alive");
        let root_alive = fixture.root_socket().with_extension("alive");
        assert!(pid_alive.exists());
        drop(second);
        assert!(
            pid_alive.symlink_metadata().is_err(),
            "the pid-named .alive file was left behind"
        );
        assert!(root_alive.exists());
        drop(first);
        assert!(
            root_alive.exists(),
            "the root's .alive file is shared by every claimant and stays"
        );
        liveness_comes_free(&fixture.root_socket());
        // The name is free again for a later window of this pid.
        let third = fixture.server();
        let fourth = fixture.server();
        assert_eq!(fourth.socket(), fixture.pid_socket());
        drop((third, fourth));
    }

    #[test]
    fn a_symlink_at_the_socket_path_is_not_followed() {
        let fixture = fixture();
        let target = fixture.base.join("target");
        symlink(&target, fixture.root_socket()).unwrap();
        let why = OpenServer::claim(&fixture.dir, &fixture.root).unwrap_err();
        assert!(why.contains("not this user's socket"), "{why}");
        assert!(fixture
            .root_socket()
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(target.symlink_metadata().is_err(), "the bind went through the link");

        // A link to a live listener is not a window of this root either.
        std::fs::remove_file(fixture.root_socket()).unwrap();
        let _listener = UnixListener::bind(&target).unwrap();
        symlink(&target, fixture.root_socket()).unwrap();
        assert!(OpenServer::claim(&fixture.dir, &fixture.root).is_err());
        assert!(target.exists(), "the listener's socket was removed through the link");
    }

    #[test]
    fn close_unlinks_first_and_removes_only_our_socket() {
        let fixture = fixture();
        let sock = fixture.root_socket();
        let server = fixture.server();
        let watch = Arc::clone(&server.watch);
        let (hook_sock, hook_watch) = (sock.clone(), Arc::clone(&watch));
        let looked = std::rc::Rc::new(std::cell::Cell::new(false));
        let hook_looked = looked.clone();
        test_hook::set(Box::new(move || {
            assert!(hook_sock.symlink_metadata().is_err(), "the socket goes first");
            assert!(
                !hook_watch.finished.load(Ordering::SeqCst),
                "the listener is still up when the socket goes"
            );
            hook_looked.set(true);
        }));
        server.close();
        assert!(looked.get(), "the close never reached the stop");
        wait_until("the thread ends", || watch.finished.load(Ordering::SeqCst));
        server.close();
        liveness_comes_free(&sock);

        // A newer window took the path meanwhile.
        let server = fixture.server();
        assert_eq!(server.socket(), sock, "the closed server left the root name free");
        std::fs::remove_file(&sock).unwrap();
        let _newer = UnixListener::bind(&sock).unwrap();
        server.close();
        assert!(sock.symlink_metadata().is_ok(), "a newer window's socket was removed");
    }

    #[test]
    fn close_does_not_wait_for_the_thread() {
        let fixture = fixture();
        let server = fixture.server_with(Waits {
            read: Duration::from_secs(120),
            ..Waits::STANDARD
        });
        let watch = Arc::clone(&server.watch);
        let client = UnixStream::connect(server.socket()).unwrap();
        // Past its `poll` the thread is on this connection, and that silent read outlasts the test.
        wait_until("the thread sees the connection", || {
            watch.polls.load(Ordering::SeqCst) > 0
        });
        server.close();
        assert!(
            !watch.finished.load(Ordering::SeqCst),
            "the close returned with the thread still reading"
        );
        drop(client);
        wait_until("the thread ends", || watch.finished.load(Ordering::SeqCst));
    }

    #[test]
    fn dropping_the_server_closes_it() {
        let fixture = fixture();
        let server = fixture.server();
        let watch = Arc::clone(&server.watch);
        drop(server);
        assert!(fixture.root_socket().symlink_metadata().is_err());
        wait_until("the thread ends", || watch.finished.load(Ordering::SeqCst));
    }

    #[test]
    fn the_thread_parks_with_no_wakeups() {
        let fixture = fixture();
        let server = fixture.server();
        wait_until("the listener enters its poll", || {
            server.watch.entered.load(Ordering::SeqCst)
        });
        assert_eq!(
            server.watch.timeout_ms.load(Ordering::SeqCst),
            -1,
            "an idle listener waits for descriptors only"
        );
        server.close();
        wait_until("the stop wakes the listener", || {
            server.watch.finished.load(Ordering::SeqCst)
        });
        assert_eq!(
            server.watch.timeout_ms.load(Ordering::SeqCst),
            -1,
            "the completed poll received an infinite timeout"
        );
        assert_eq!(
            server.watch.polls.load(Ordering::SeqCst),
            1,
            "after shutdown every wakeup has been counted, and stop is the only one"
        );
    }

    fn target(path: PathBuf) -> OpenTarget {
        OpenTarget {
            path,
            line: None,
            col: None,
        }
    }

    fn deliver_later(fixture: &Fixture, open: Vec<OpenTarget>, hint: Option<PathBuf>) -> JoinHandle<Delivered> {
        let (root, dir) = (fixture.root.clone(), fixture.dir.clone());
        std::thread::spawn(move || deliver(&open, &root, &[], &dir, hint.as_deref(), None))
    }

    /// What a drain on the GTK side would open.
    fn taken(server: &OpenServer) -> Vec<PathBuf> {
        let incoming = server.drain().into_iter();
        incoming
            .filter_map(Incoming::take)
            .map(|(request, _)| file_of(request).path)
            .collect()
    }

    #[test]
    fn a_silent_window_opens_nothing_for_clients_that_gave_up_in_the_queue() {
        let fixture = fixture();
        let files = ["a", "b", "c", "d"].map(|name| fixture.file(name));
        let server = fixture.server();
        let clients: Vec<_> = files
            .iter()
            .map(|file| deliver_later(&fixture, vec![target(file.clone())], None))
            .collect();
        // Each wait of two seconds ends in `busy`. Three of them keep the fourth client queued past its
        // deadline, so it leaves without having sent anything.
        for client in clients {
            assert_eq!(client.join().unwrap(), Delivered::NotHere);
        }
        assert_eq!(
            taken(&server),
            Vec::<PathBuf>::new(),
            "these clients went to a new window"
        );
    }

    /// The request that opens the file twice if its client gives up on it: taken late, with the answer
    /// coming later still.
    #[test]
    fn a_client_behind_a_held_request_hears_what_became_of_it() {
        let fixture = fixture();
        let listener = UnixListener::bind(fixture.root_socket()).unwrap();
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(PATIENCE)).unwrap();
            reply_line(&stream, READY).unwrap();
            let mut line = [0u8; 2];
            stream.read_exact(&mut line).unwrap();
            assert_eq!(&line, b"x\n");
            reply_line(&stream, "ok").unwrap();
        });
        let clock = Cell::new(Instant::now());
        let connected = clock.get();
        let mut reads = Vec::new();
        let answer = exchange_with(
            &fixture.root_socket(),
            "x\n",
            euid(),
            PATIENCE,
            &|| clock.get(),
            &mut |stream, deadline, cap| {
                reads.push(deadline);
                let line = read_line(stream, Instant::now() + PATIENCE, cap)?;
                if reads.len() == 1 {
                    clock.set(connected + PATIENCE + Duration::from_secs(1));
                }
                Ok(line)
            },
        )
        .unwrap();
        assert_eq!(answer, "ok");
        assert_eq!(
            reads,
            [connected + PATIENCE, clock.get() + PATIENCE],
            "the reply gets its whole budget after ready, even after a queued wait"
        );
        peer.join().unwrap();
    }

    #[test]
    fn a_client_sends_nothing_until_it_is_served() {
        let fixture = fixture();
        let listener = UnixListener::bind(fixture.root_socket()).unwrap();
        let open = vec![target(fixture.file("a.rs"))];
        let within = Duration::from_millis(100);
        let delivered = deliver_with(&open, &fixture.root, &[], &fixture.dir, None, None, euid(), within);
        assert_eq!(delivered, Delivered::NotHere);
        let (stream, _) = listener.accept().unwrap();
        let mut sent = Vec::new();
        (&stream).read_to_end(&mut sent).unwrap();
        assert_eq!(sent, b"", "a window that turned to this connection late would run it");
    }

    #[test]
    fn deliver_tries_the_hint_then_the_root() {
        let fixture = fixture();
        let file = fixture.file("a.rs");
        let at_root = fixture.server();
        let at_hint = fixture.server();
        let hint = at_hint.socket().to_owned();

        // A hint that takes it: the root's window never hears of it.
        let client = deliver_later(&fixture, vec![target(file.clone())], Some(hint.clone()));
        let (_, answering) = next_request(&at_hint).take().unwrap();
        answering.answer(Answer::Accepted);
        assert_eq!(client.join().unwrap(), Delivered::Opened { root: None });
        assert!(at_root.drain().is_empty());

        // A busy hint moves on to the root's socket.
        let client = deliver_later(&fixture, vec![target(file.clone())], Some(hint));
        let (_, answering) = next_request(&at_hint).take().unwrap();
        answering.answer(Answer::Busy);
        let (_, answering) = next_request(&at_root).take().unwrap();
        answering.answer(Answer::Accepted);
        assert_eq!(client.join().unwrap(), fixture.opened());

        // A hint nothing listens on, and one that is not a socket at all.
        for hint in [fixture.base.join("gone"), file.clone()] {
            let client = deliver_later(&fixture, vec![target(file.clone())], Some(hint));
            let (_, answering) = next_request(&at_root).take().unwrap();
            answering.answer(Answer::Accepted);
            assert_eq!(client.join().unwrap(), fixture.opened());
        }
    }

    /// `cd repo/crate; eitri src/x.rs`: the launch's root is `crate`, the running window's is `repo`.
    #[test]
    fn a_window_on_a_wider_root_takes_the_file() {
        let fixture = fixture();
        let wider = fixture.root.clone();
        let root = fixture.below("crate");
        let file = root.join("x.rs");
        std::fs::write(&file, "text\n").unwrap();
        let server = fixture.server();
        let (open, dir) = (vec![target(file.clone())], fixture.dir.clone());
        let client = {
            let wider = [wider.clone()];
            std::thread::spawn(move || deliver(&open, &root, &wider, &dir, None, None))
        };
        let (request, answering) = next_request(&server).take().unwrap();
        answering.answer(Answer::Accepted);
        assert_eq!(file_of(request).path, file);
        assert_eq!(client.join().unwrap(), Delivered::Opened { root: Some(wider) });
    }

    #[test]
    fn the_narrowest_window_of_the_project_goes_first_and_a_busy_one_moves_on() {
        let fixture = fixture();
        let top = fixture.root.clone();
        let middle = fixture.below("a");
        let root = middle.join("b");
        std::fs::create_dir(&root).unwrap();
        let file = root.join("x.rs");
        std::fs::write(&file, "text\n").unwrap();
        let at_top = OpenServer::claim(&fixture.dir, &top).unwrap();
        let at_middle = OpenServer::claim(&fixture.dir, &middle).unwrap();
        let at_root = OpenServer::claim(&fixture.dir, &root).unwrap();
        let run = || {
            let (open, dir) = (vec![target(file.clone())], fixture.dir.clone());
            let (root, wider) = (root.clone(), [middle.clone(), top.clone()]);
            std::thread::spawn(move || deliver(&open, &root, &wider, &dir, None, None))
        };

        let client = run();
        let (_, answering) = next_request(&at_root).take().unwrap();
        answering.answer(Answer::Accepted);
        assert_eq!(
            client.join().unwrap(),
            Delivered::Opened {
                root: Some(root.clone())
            }
        );
        assert!(at_middle.drain().is_empty() && at_top.drain().is_empty());

        let client = run();
        let (_, answering) = next_request(&at_root).take().unwrap();
        answering.answer(Answer::Busy);
        let (_, answering) = next_request(&at_middle).take().unwrap();
        answering.answer(Answer::Accepted);
        assert_eq!(
            client.join().unwrap(),
            Delivered::Opened {
                root: Some(middle.clone())
            }
        );
        assert!(at_top.drain().is_empty());
    }

    /// The window checks too, but a window that cannot hold every target must not be given the first of them: from
    /// then on there is no new window to fall back to.
    #[test]
    fn a_wider_root_that_does_not_hold_every_target_is_not_asked() {
        let fixture = fixture();
        let root = fixture.below("crate");
        let inside = root.join("x.rs");
        std::fs::write(&inside, "text\n").unwrap();
        let outside = fixture.outside("y.rs");
        let server = fixture.server();
        let open = vec![target(inside), target(outside)];
        let wider = [fixture.root.clone()];
        let delivered = deliver_with(&open, &root, &wider, &fixture.dir, None, None, euid(), CLIENT_DEADLINE);
        assert_eq!(delivered, Delivered::NotHere);
        assert!(server.drain().is_empty());
    }

    /// `eitri x.rs ../y.rs` from a crate: the second file is outside the launch's root and inside the repository.
    #[test]
    fn a_wider_root_can_hold_what_the_root_does_not() {
        let fixture = fixture();
        let root = fixture.below("crate");
        let inside = root.join("x.rs");
        std::fs::write(&inside, "text\n").unwrap();
        let beside = fixture.file("y.rs");
        let server = fixture.server();
        let at_crate = OpenServer::claim(&fixture.dir, &root).unwrap();
        let (open, dir, wider) = (
            vec![target(inside), target(beside)],
            fixture.dir.clone(),
            [fixture.root.clone()],
        );
        let client = std::thread::spawn(move || deliver(&open, &root, &wider, &dir, None, None));
        for _ in 0..2 {
            let (_, answering) = next_request(&server).take().unwrap();
            answering.answer(Answer::Accepted);
        }
        assert_eq!(client.join().unwrap(), fixture.opened());
        assert!(at_crate.drain().is_empty(), "this window cannot hold the second file");
    }

    #[test]
    fn a_hint_goes_before_every_root_and_a_hint_that_is_a_wider_root_is_named_and_asked_once() {
        let fixture = fixture();
        let root = fixture.below("crate");
        let file = root.join("x.rs");
        std::fs::write(&file, "text\n").unwrap();
        let at_root = OpenServer::claim(&fixture.dir, &root).unwrap();
        let at_wider = fixture.server();
        let run = || {
            let hint = at_wider.socket().to_owned();
            let (open, dir) = (vec![target(file.clone())], fixture.dir.clone());
            let (root, wider) = (root.clone(), [fixture.root.clone()]);
            std::thread::spawn(move || deliver(&open, &root, &wider, &dir, Some(&hint), None))
        };

        let client = run();
        let (_, answering) = next_request(&at_wider).take().unwrap();
        answering.answer(Answer::Accepted);
        assert_eq!(client.join().unwrap(), fixture.opened());
        assert!(at_root.drain().is_empty(), "the terminal's own window comes first");

        // Turned down by the window behind both names, it goes to the root's and not to that window again.
        let client = run();
        let (_, answering) = next_request(&at_wider).take().unwrap();
        answering.answer(Answer::Busy);
        let (_, answering) = next_request(&at_root).take().unwrap();
        answering.answer(Answer::Busy);
        assert_eq!(client.join().unwrap(), Delivered::NotHere);
        assert!(at_wider.drain().is_empty());
    }

    #[test]
    fn a_hint_that_is_the_root_socket_is_asked_once() {
        let fixture = fixture();
        let file = fixture.file("a.rs");
        let server = fixture.server();
        let client = deliver_later(&fixture, vec![target(file)], Some(fixture.root_socket()));
        let (_, answering) = next_request(&server).take().unwrap();
        answering.answer(Answer::Busy);
        assert_eq!(client.join().unwrap(), Delivered::NotHere);
        assert!(server.drain().is_empty());
    }

    #[test]
    fn a_silent_hint_moves_on_at_the_deadline() {
        let fixture = fixture();
        let file = fixture.file("a.rs");
        let server = fixture.server();
        let silent = fixture.base.join("silent");
        let _never_accepts = UnixListener::bind(&silent).unwrap();
        let (root, dir, open) = (fixture.root.clone(), fixture.dir.clone(), vec![target(file)]);
        let client = std::thread::spawn(move || {
            deliver_with(
                &open,
                &root,
                &[],
                &dir,
                Some(&silent),
                None,
                euid(),
                Duration::from_secs(1),
            )
        });
        let (_, answering) = next_request(&server).take().unwrap();
        answering.answer(Answer::Accepted);
        assert_eq!(client.join().unwrap(), fixture.opened());
    }

    #[test]
    fn deliver_refuses_a_server_of_another_uid() {
        let fixture = fixture();
        let file = fixture.file("a.rs");
        let server = fixture.server();
        let open = [target(file)];
        let other = euid().wrapping_add(1);
        let delivered = deliver_with(
            &open,
            &fixture.root,
            &[],
            &fixture.dir,
            None,
            None,
            other,
            CLIENT_DEADLINE,
        );
        assert_eq!(delivered, Delivered::NotHere);
        assert!(server.drain().is_empty());

        // The file can be swapped after the path check: the connected peer is checked as well, and
        // it hears nothing.
        let listening = fixture.base.join("listening");
        let listener = UnixListener::bind(&listening).unwrap();
        let why = exchange(&listening, "secret\n", other, CLIENT_DEADLINE).unwrap_err();
        assert!(why.contains("another user"), "{why}");
        let (mut accepted, _) = listener.accept().unwrap();
        let mut heard = Vec::new();
        accepted.read_to_end(&mut heard).unwrap();
        assert!(heard.is_empty());
    }

    #[test]
    fn targets_go_last_to_first() {
        let fixture = fixture();
        let (a, b, c) = (fixture.file("a"), fixture.file("b"), fixture.file("c"));
        let server = fixture.server();
        let open = vec![
            OpenTarget {
                path: a.clone(),
                line: Some(3),
                col: Some(9),
            },
            target(b.clone()),
            target(c.clone()),
        ];
        let (root, dir) = (fixture.root.clone(), fixture.dir.clone());
        let client = std::thread::spawn(move || deliver(&open, &root, &[], &dir, None, Some("token")));
        let mut seen = Vec::new();
        for _ in 0..3 {
            let (request, answering) = next_request(&server).take().unwrap();
            answering.answer(Answer::Accepted);
            seen.push(file_of(request));
        }
        assert_eq!(client.join().unwrap(), fixture.opened());
        let token = Some("token".to_owned());
        let plain = |path: &Path| Open {
            path: path.to_owned(),
            line: None,
            col: None,
            token: token.clone(),
        };
        assert_eq!(
            seen,
            [
                plain(&c),
                plain(&b),
                Open {
                    line: Some(3),
                    col: Some(9),
                    ..plain(&a)
                },
            ]
        );
    }

    #[test]
    fn a_later_target_failing_is_partly() {
        let fixture = fixture();
        let (a, b, c) = (fixture.file("a"), fixture.file("b"), fixture.file("c"));
        let server = fixture.server();
        let open = vec![target(a.clone()), target(b.clone()), target(c)];
        let client = deliver_later(&fixture, open, None);
        let (_, answering) = next_request(&server).take().unwrap();
        answering.answer(Answer::Accepted);
        let (_, answering) = next_request(&server).take().unwrap();
        answering.answer(Answer::Busy);
        // One failure does not stop the rest: the window may be free again.
        let (request, answering) = next_request(&server).take().unwrap();
        assert_eq!(file_of(request).path, a);
        answering.answer(Answer::Accepted);
        assert_eq!(
            client.join().unwrap(),
            Delivered::Partly {
                failed: vec![(b, "busy".to_owned())]
            }
        );
    }

    #[test]
    fn a_target_outside_the_root_skips_the_root_socket() {
        let fixture = fixture();
        let inside = fixture.file("a");
        let outside = fixture.outside("b");
        let server = fixture.server();
        let open = [target(inside), target(outside)];
        assert_eq!(
            deliver(&open, &fixture.root, &[], &fixture.dir, None, None),
            Delivered::NotHere
        );
        assert_eq!(
            server.watch.polls.load(Ordering::SeqCst),
            0,
            "nothing connected to the root's socket"
        );
    }

    /// A window's end of the protocol, as a window of some other build speaks it: says `greeting`, reads one line,
    /// says `reply`. The thread returns the line.
    fn stub_window(fixture: &Fixture, greeting: &'static str, reply: &'static str) -> JoinHandle<String> {
        let listener = UnixListener::bind(fixture.root_socket()).unwrap();
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            let give_up = Instant::now() + PATIENCE;
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < give_up => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => return format!("no client came: {e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(PATIENCE)).unwrap();
            reply_line(&stream, greeting).unwrap();
            let line = read_reply(&stream);
            // A client that did not like the greeting has gone.
            let _ = reply_line(&stream, reply);
            line
        })
    }

    fn raise_line(path: &Path) -> String {
        json!({ "v": 2, "op": "raise", "path": path }).to_string()
    }

    fn raise_later(fixture: &Fixture, token: Option<&'static str>) -> JoinHandle<Raised> {
        let (root, dir) = (fixture.root.clone(), fixture.dir.clone());
        std::thread::spawn(move || raise(&root, &dir, token))
    }

    #[test]
    fn version_2_raise_is_taken_and_answered() {
        let fixture = fixture();
        let server = fixture.server();
        let client = raise_later(&fixture, Some("token"));
        let (request, answering) = next_request(&server).take().expect("nobody gave up on it");
        assert_eq!(
            request,
            Request::Raise {
                token: Some("token".to_owned())
            }
        );
        answering.answer(Answer::Accepted);
        assert_eq!(client.join().unwrap(), Raised::Done);

        // With no token, and as the wire has it.
        let client = ask_later(server.socket(), raise_line(&fixture.root));
        let (request, answering) = next_request(&server).take().unwrap();
        assert_eq!(request, Request::Raise { token: None });
        answering.answer(Answer::Accepted);
        assert_eq!(client.join().unwrap(), "ok");
    }

    #[test]
    fn a_version_1_open_is_still_served() {
        let fixture = fixture();
        let file = fixture.file("a.rs");
        let (open, reply) = handed_over(&fixture, request(&file));
        assert_eq!((open.path, reply.as_str()), (file.clone(), "ok"));

        // And it is what a client still sends, so a window of an older build takes the file.
        let window = stub_window(&fixture, READY, "ok");
        let delivered = deliver(&[target(file)], &fixture.root, &[], &fixture.dir, None, None);
        assert_eq!(delivered, fixture.opened());
        let heard: serde_json::Value = serde_json::from_str(&window.join().unwrap()).unwrap();
        assert_eq!((&heard["v"], &heard["op"]), (&json!(1), &json!("open")));
    }

    #[test]
    fn each_version_has_its_one_operation() {
        let fixture = fixture();
        let file = fixture.file("a.rs");
        let server = fixture.server();
        for line in [
            json!({ "v": 2, "op": "open", "path": file }),
            json!({ "v": 1, "op": "raise", "path": fixture.root }),
        ] {
            assert_eq!(refusal(&server, &line.to_string()), "refused: unsupported op", "{line}");
        }
        assert_eq!(
            refusal(
                &server,
                &json!({ "v": 3, "op": "raise", "path": fixture.root }).to_string()
            ),
            "refused: unsupported protocol version 3"
        );
    }

    #[test]
    fn a_raise_carries_no_position() {
        let fixture = fixture();
        let server = fixture.server();
        for line in [
            json!({ "v": 2, "op": "raise", "path": fixture.root, "line": 1 }),
            json!({ "v": 2, "op": "raise", "path": fixture.root, "col": 1 }),
            json!({ "v": 2, "op": "raise", "path": fixture.root, "run": "!sh" }),
        ] {
            assert_eq!(
                refusal(&server, &line.to_string()),
                "refused: malformed request",
                "{line}"
            );
        }
    }

    #[test]
    fn raise_of_another_root_is_refused() {
        let fixture = fixture();
        let server = fixture.server();
        let below = fixture.below("sub");
        let beside = fixture.base.join("root-2");
        std::fs::create_dir(&beside).unwrap();
        let spelt_otherwise = fixture.base.join("link");
        symlink(&fixture.root, &spelt_otherwise).unwrap();
        let file = fixture.file("a.rs");
        for path in [below, beside, spelt_otherwise, file, PathBuf::from("root")] {
            assert_eq!(
                refusal(&server, &raise_line(&path)),
                "refused: the path is not this window's root",
                "{}",
                path.display()
            );
        }
    }

    #[test]
    fn a_raise_is_busy_when_the_window_ends() {
        let fixture = fixture();
        let server = fixture.server();

        let client = raise_later(&fixture, None);
        let (_, answering) = next_request(&server).take().unwrap();
        answering.answer(Answer::Busy);
        assert_eq!(client.join().unwrap(), Raised::NotHere);

        let client = raise_later(&fixture, None);
        drop(next_request(&server).take().unwrap());
        assert_eq!(
            client.join().unwrap(),
            Raised::NotHere,
            "a window going answers nothing"
        );

        let client = raise_later(&fixture, None);
        drop(next_request(&server));
        assert_eq!(client.join().unwrap(), Raised::NotHere);
    }

    #[test]
    fn an_old_shape_server_reads_as_not_here() {
        let fixture = fixture();
        let window = stub_window(&fixture, READY, "refused: unsupported protocol version 2");
        assert_eq!(raise(&fixture.root, &fixture.dir, None), Raised::NotHere);
        let heard: serde_json::Value = serde_json::from_str(&window.join().unwrap()).unwrap();
        assert_eq!(heard, json!({ "v": 2, "op": "raise", "path": fixture.root }));
        std::fs::remove_file(fixture.root_socket()).unwrap();

        // Nor does a server that never said it was ready get a request.
        let window = stub_window(&fixture, "ok", "ok");
        assert_eq!(raise(&fixture.root, &fixture.dir, None), Raised::NotHere);
        assert_eq!(window.join().unwrap(), "", "nothing went to it");
    }

    #[test]
    fn with_nothing_listening_a_raise_is_not_here() {
        let fixture = fixture();
        assert_eq!(raise(&fixture.root, &fixture.dir, None), Raised::NotHere);
        // A crashed window's socket.
        drop(UnixListener::bind(fixture.root_socket()).unwrap());
        assert_eq!(raise(&fixture.root, &fixture.dir, None), Raised::NotHere);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_root_that_is_not_text_gets_a_window_of_its_own() {
        use std::ffi::OsStr;

        let fixture = fixture();
        let root = fixture.base.join(OsStr::from_bytes(b"not-text-\xff"));
        std::fs::create_dir(&root).unwrap();
        let server = OpenServer::claim(&fixture.dir, &root).unwrap();
        assert_eq!(raise(&root, &fixture.dir, None), Raised::NotHere);
        assert_eq!(server.watch.polls.load(Ordering::SeqCst), 0, "nothing connected");
    }

    #[test]
    fn a_token_past_the_cap_is_refused() {
        let fixture = fixture();
        let server = fixture.server();
        let longest = json!({ "v": 2, "op": "raise", "path": fixture.root, "token": "t".repeat(MAX_TOKEN) });
        let client = ask_later(server.socket(), longest.to_string());
        let (request, answering) = next_request(&server).take().unwrap();
        answering.answer(Answer::Accepted);
        assert_eq!(client.join().unwrap(), "ok");
        assert!(matches!(request, Request::Raise { token: Some(token) } if token.len() == MAX_TOKEN));

        let over = json!({ "v": 2, "op": "raise", "path": fixture.root, "token": "t".repeat(MAX_TOKEN + 1) });
        assert_eq!(refusal(&server, &over.to_string()), "refused: token too long");
        let token = "t".repeat(MAX_TOKEN + 1);
        assert_eq!(
            raise(&fixture.root, &fixture.dir, Some(&token)),
            Raised::NotHere,
            "the launch gets a window of its own"
        );
        assert!(server.drain().is_empty());
    }

    #[test]
    fn the_peer_uid_rule_holds_for_raise() {
        let fixture = fixture();
        let (requests, waiting) = mpsc::channel();
        let (wake, _quiet) = UnixStream::pair().unwrap();
        let serving = Serving {
            root: fixture.root.clone(),
            me: euid().wrapping_add(1),
            waits: Waits::STANDARD,
            requests,
            wake,
            watch: Arc::default(),
        };
        let (client, served) = UnixStream::pair().unwrap();
        (&client)
            .write_all(format!("{}\n", raise_line(&fixture.root)).as_bytes())
            .unwrap();
        serving.serve_one(served);
        assert_eq!(read_reply(&client), "refused: not your process");
        assert!(waiting.try_recv().is_err());

        // The client looks at the server the same way, before it writes anything.
        let server = fixture.server();
        let other = euid().wrapping_add(1);
        assert_eq!(
            raise_with(&fixture.root, &fixture.dir, None, other, CLIENT_DEADLINE),
            Raised::NotHere
        );
        assert_eq!(server.watch.polls.load(Ordering::SeqCst), 0, "nothing connected");
    }

    /// A look at whether the socket is alive, such as a claim's, is a connection that says nothing.
    #[test]
    fn a_probe_connect_that_sends_nothing_leaves_the_server_serving() {
        let fixture = fixture();
        let server = fixture.server_with(Waits {
            read: Duration::from_millis(100),
            ..Waits::STANDARD
        });
        drop(UnixStream::connect(server.socket()).unwrap());
        // One that stays silent past the read deadline, then the next client.
        let silent = served(server.socket());
        assert_eq!(read_reply(&silent), "refused: request timed out");
        let client = raise_later(&fixture, None);
        let (request, answering) = next_request(&server).take().unwrap();
        assert_eq!(request, Request::Raise { token: None });
        answering.answer(Answer::Accepted);
        assert_eq!(client.join().unwrap(), Raised::Done);
    }

    #[test]
    fn nothing_listening_is_not_here() {
        let fixture = fixture();
        let open = [target(fixture.file("a"))];
        assert_eq!(
            deliver(&open, &fixture.root, &[], &fixture.dir, None, None),
            Delivered::NotHere
        );
        // A crashed window's socket: the file is there and the connect is refused.
        drop(UnixListener::bind(fixture.root_socket()).unwrap());
        assert_eq!(
            deliver(&open, &fixture.root, &[], &fixture.dir, None, None),
            Delivered::NotHere
        );
        assert_eq!(
            deliver(&[], &fixture.root, &[], &fixture.dir, None, None),
            Delivered::NotHere
        );
    }
}
