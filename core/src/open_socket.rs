//! The per-root open socket: how `eitri FILE` hands a file to a window that already holds that root,
//! instead of starting another window.
//!
//! A window that owns its editor binds `open-<16 hex project key>.sock` in the control directory. A
//! client sends one JSON line for each file and reads one reply line: `ok`, `busy` or
//! `refused: <why>`. `ok` means the window accepted the file, not that nvim has opened it: nvim may
//! be sitting at a prompt. On `busy`, a refusal or a timeout the client starts a new window, so
//! nothing here is ever fatal, to the window or to the command.
//!
//! The socket is a way for a local process to act on a window that holds an agent, so it stays narrow:
//!
//! - Only a peer with this process's effective uid is served, checked before its request is read. The
//!   client checks the server the same way before it writes a path.
//! - A request has one shape, one line of at most 4096 bytes within one second, and any other key is
//!   refused. It carries no text that is executed.
//! - The path must name a regular file inside the root, or a free name in a directory inside it, after
//!   every link is resolved. What goes on is the canonical path.
//! - Everything up to the hand-over happens on a thread of its own, parked in `poll(2)`, so a client
//!   never holds the GTK thread and an idle window takes no wakeups from this.
//!
//! One atomic decides who owns a handed-over request. The thread stops waiting for the GTK side after
//! [`ANSWER_WITHIN`] and says `busy`, and a request nobody took by then can no longer be taken. Nor
//! can one whose client hung up in the meantime: a client's deadline runs from its connect, so time
//! queued behind another client counts against it, and it can leave while its request waits here.
//! Once the GTK side has taken it, the request will run, so a thread that hears nothing more says
//! `ok`: `busy` could open the file twice.
//!
//! That leaves one way to open a file twice: the client's deadline passes after the take and before
//! the reply reaches it, so it starts a new window as well. Only the client could close that, by
//! waiting to hear the outcome past its deadline.
//!
//! A socket in the way is reclaimed only under `open-<key>.lock`, only when a connect is refused twice
//! a moment apart (a claimant between `bind` and `listen` refuses the first one), and only the very
//! socket that was probed. A socket with a listener is another window's. This window then binds
//! `open-<key>-<pid>.sock`, which nobody looks up by root: only its own terminals are told the name.
//! A crash leaves that file behind until the runtime directory goes, at logout.

use crate::instance_dir::{nonblocking_connect_outcome, ConnectOutcome};
use crate::panel_control::{
    ensure_dir, euid, identity, peer_uid, read_line, remove_own_socket, reply_line, take_reclaim_lock,
    validate_nvim_addr, LineFail, MAX_LINE, REQUEST_DEADLINE,
};
use crate::project_root::OpenTarget;
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::fmt;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
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
/// How long a client waits for each target, connect included. Longer than the server's three waits
/// together, so a request served as soon as it arrives gets its reply. It runs from the connect, and
/// those waits do not count time in the accept queue, so a client that queued can give up on a request
/// the server still holds: [`Incoming::take`] looks for that.
pub const CLIENT_DEADLINE: Duration = Duration::from_millis(4500);

/// The one protocol version. A new capability is a new version, never an extra key.
const PROTOCOL: u64 = 1;
/// The one operation.
const OP_OPEN: &str = "open";
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

/// What the window says to a request it took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Accepted,
    Busy,
}

/// One validated request, handed over at most once; dropping it unanswered answers `busy`.
#[derive(Debug)]
pub struct Incoming {
    open: Open,
    /// [`WAITING`] until the first of the GTK side and the thread moves it on.
    owner: Arc<AtomicU8>,
    answer: Sender<Answer>,
    /// The connection, to see from the GTK side whether its client is still there.
    client: UnixStream,
}

impl Incoming {
    /// The request and the way to answer it, or `None` when the thread already gave up on it or its
    /// client has left, which may mean a new window already holds the file. Never blocks.
    pub fn take(self) -> Option<(Open, Answering)> {
        if client_left(&self.client) {
            return None;
        }
        self.owner
            .compare_exchange(WAITING, TAKEN, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some((self.open, Answering { answer: self.answer }))
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
    if wire.v != PROTOCOL {
        return Err(format!("unsupported protocol version {}", wire.v));
    }
    if wire.op != OP_OPEN {
        return Err("unsupported op".to_owned());
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
            // SAFETY: `waiting` is valid for the call, its length is passed, and both descriptors
            // are borrowed for all of this function.
            let ready = unsafe { libc::poll(waiting.as_mut_ptr(), waiting.len() as libc::nfds_t, -1) };
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
            Ok(open) => {
                // It may have waited behind another client long enough to give up and start a new
                // window; opening the file here as well would open it twice.
                if !client_left(&stream) {
                    let reply = self.hand_over(open, &stream);
                    let _ = reply_line(&stream, reply);
                }
            }
            Err(why) => {
                let _ = reply_line(&stream, &format!("refused: {why}"));
            }
        }
    }

    fn judge(&self, line: &[u8]) -> Result<Open, String> {
        let wire = parse(line)?;
        Ok(Open {
            path: validated_path(Path::new(&wire.path), &self.root)?,
            line: wire.line,
            col: wire.col,
            token: wire.token,
        })
    }

    /// Gives `open` to the GTK side and returns the reply that settles it.
    fn hand_over(&self, open: Open, stream: &UnixStream) -> &'static str {
        // A request the GTK side could not check for a hangup might open after its client left.
        let Ok(client) = stream.try_clone() else {
            return "busy";
        };
        let owner = Arc::new(AtomicU8::new(WAITING));
        let (answer, answers) = mpsc::channel();
        let incoming = Incoming {
            open,
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
            // file twice.
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

/// Whether a connect to `sock` is refused, the one proof that nothing listens there.
fn refused(sock: &Path) -> bool {
    nonblocking_connect_outcome(sock) == ConnectOutcome::NoListener
}

/// Replaces `sock` if it is a leftover; `None` when something else holds it. A link or a file that is
/// not this user's socket is an error: it is left, and a connect would go through the link. A socket
/// that goes away meanwhile was a window closing, and its name is free to bind.
fn reclaim(sock: &Path) -> Result<Option<UnixListener>, String> {
    let probed = match sock.symlink_metadata() {
        Ok(meta) if meta.file_type().is_socket() && meta.uid() == euid() => (meta.dev(), meta.ino()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return try_bind(sock),
        _ => return Err(format!("{} is not this user's socket; not touching it", sock.display())),
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
    // A claimant needs no lock to bind a free name, so one may have got there first.
    try_bind(sock)
}

/// `bind`, with the name in use reported as `None`.
fn try_bind(sock: &Path) -> Result<Option<UnixListener>, String> {
    match UnixListener::bind(sock) {
        Ok(listener) => Ok(Some(listener)),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => Ok(None),
        Err(e) => Err(format!("could not bind {}: {e}", sock.display())),
    }
}

/// Binds the root's socket, or this window's own when another window holds that one.
fn bind(dir: &Path, root: &Path) -> Result<(UnixListener, PathBuf), String> {
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
        let (listener, socket) = bind(dir, root)?;
        let bound = identity(&socket).ok_or_else(|| format!("could not read {}", socket.display()))?;
        let started = OpenServer::start(listener, socket.clone(), bound, root, waits);
        if started.is_err() && identity(&socket) == Some(bound) {
            remove_own_socket(&socket);
        }
        started
    }

    fn start(
        listener: UnixListener,
        socket: PathBuf,
        bound: (u64, u64),
        root: &Path,
        waits: Waits,
    ) -> Result<OpenServer, String> {
        let failed = |what: &str, e: io::Error| format!("could not {what} for {}: {e}", socket.display());
        listener
            .set_nonblocking(true)
            .map_err(|e| failed("set up the listener", e))?;
        // `bind` leaves the umask's mode on the file.
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| failed("set the mode", e))?;
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
    /// never a refusal it would take for a leftover. Nothing waits for the thread, because this runs
    /// on the GTK thread: it ends within its own bounds. Idempotent.
    pub fn close(&self) {
        let Some(stop) = self.stop.take() else {
            return;
        };
        if identity(&self.socket) == Some(self.bound) {
            remove_own_socket(&self.socket);
        }
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
    /// A window took every target.
    Opened,
    /// A window took the one that decides, and these failed after it, with the reason.
    Partly { failed: Vec<(PathBuf, String)> },
    /// No window took anything: start a new one.
    NotHere,
}

/// Connects to `sock`, checks that the process behind it is `me`, sends `line` and returns the reply
/// line. One deadline covers all of it.
fn exchange(sock: &Path, line: &str, me: u32, within: Duration) -> Result<String, String> {
    let deadline = Instant::now() + within;
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
    let left = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(1));
    stream
        .set_write_timeout(Some(left))
        .and_then(|()| (&stream).write_all(line.as_bytes()))
        .map_err(|e| format!("could not send to {}: {e}", sock.display()))?;
    let reply = read_line(&stream, deadline, MAX_LINE).map_err(|why| format!("no reply ({why:?})"))?;
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
        v: PROTOCOL,
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

/// Tries to give `open` to a running window: `hint` first, then the root's socket when every target
/// is inside the root. Targets go last to first, one connection each, so the first ends up shown.
/// `root` must be canonical, as for [`OpenServer::claim`].
pub fn deliver(open: &[OpenTarget], root: &Path, dir: &Path, hint: Option<&Path>, token: Option<&str>) -> Delivered {
    deliver_with(open, root, dir, hint, token, euid(), CLIENT_DEADLINE)
}

fn deliver_with(
    open: &[OpenTarget],
    root: &Path,
    dir: &Path,
    hint: Option<&Path>,
    token: Option<&str>,
    me: u32,
    within: Duration,
) -> Delivered {
    let Some((last, rest)) = open.split_last() else {
        return Delivered::NotHere;
    };
    let mut candidates: Vec<PathBuf> = hint.map(Path::to_owned).into_iter().collect();
    if open.iter().all(|target| target.path.starts_with(root)) {
        candidates.extend(socket_path(dir, root).ok().filter(|sock| Some(sock.as_path()) != hint));
    }
    for sock in candidates {
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
            Delivered::Opened
        } else {
            Delivered::Partly { failed }
        };
    }
    Delivered::NotHere
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

    /// Sends one line on a new connection and reads the reply, if the server gives one.
    fn ask(sock: &Path, line: &str) -> String {
        let mut stream = UnixStream::connect(sock).unwrap();
        stream.set_read_timeout(Some(PATIENCE)).unwrap();
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

    /// `line` through a fresh server, accepted: what the window got and what the client read.
    fn handed_over(fixture: &Fixture, line: String) -> (Open, String) {
        let server = fixture.server();
        let client = ask_later(server.socket(), line);
        let (open, answering) = next_request(&server).take().expect("nobody gave up on it");
        answering.answer(Answer::Accepted);
        (open, client.join().unwrap())
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
            refusal(&server, &json!({ "v": 2, "op": "open", "path": file }).to_string()),
            "refused: unsupported protocol version 2"
        );
        assert_eq!(
            refusal(&server, &json!({ "v": 1, "op": "close", "path": file }).to_string()),
            "refused: unsupported op"
        );

        // Half a line, then the sender goes.
        let stream = UnixStream::connect(server.socket()).unwrap();
        (&stream).write_all(b"{\"v\":1").unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        stream.set_read_timeout(Some(PATIENCE)).unwrap();
        assert_eq!(read_reply(&stream), "refused: malformed request");

        // Half a line, then the sender stays silent past the deadline.
        let stream = UnixStream::connect(server.socket()).unwrap();
        (&stream).write_all(b"{\"v\":1").unwrap();
        stream.set_read_timeout(Some(PATIENCE)).unwrap();
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
        {
            let gone = UnixStream::connect(server.socket()).unwrap();
            (&gone).write_all(format!("{}\n", request(&left)).as_bytes()).unwrap();
        }
        let waiting = ask_later(server.socket(), request(&last));
        let (_, answering) = first.take().unwrap();
        answering.answer(Answer::Accepted);
        assert_eq!(client.join().unwrap(), "ok");
        let (open, answering) = next_request(&server).take().unwrap();
        assert_eq!(open.path, last, "the request of the client that left is skipped");
        answering.answer(Answer::Accepted);
        assert_eq!(waiting.join().unwrap(), "ok");
    }

    #[test]
    fn a_client_that_left_after_the_hand_over_cannot_be_taken() {
        let fixture = fixture();
        let file = fixture.file("a.rs");
        let server = fixture.server();
        let client = UnixStream::connect(server.socket()).unwrap();
        (&client).write_all(format!("{}\n", request(&file)).as_bytes()).unwrap();
        let incoming = next_request(&server);
        drop(client);
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
        let server = fixture.server();
        assert_eq!(server.socket(), fixture.root_socket());
        assert_ne!(identity(&fixture.root_socket()).unwrap(), before);
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
    fn a_leftover_pid_named_socket_is_reclaimed() {
        let fixture = fixture();
        let first = fixture.server();
        drop(UnixListener::bind(fixture.pid_socket()).unwrap());
        let second = fixture.server();
        assert_eq!(second.socket(), fixture.pid_socket());
        drop(first);
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

        // A newer window took the path meanwhile.
        let server = fixture.server();
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
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(server.watch.polls.load(Ordering::SeqCst), 0);
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
        std::thread::spawn(move || deliver(&open, &root, &dir, hint.as_deref(), None))
    }

    /// What a drain on the GTK side would open.
    fn taken(server: &OpenServer) -> Vec<PathBuf> {
        let incoming = server.drain().into_iter();
        incoming.filter_map(Incoming::take).map(|(open, _)| open.path).collect()
    }

    #[test]
    fn a_silent_window_opens_nothing_for_clients_that_gave_up_in_the_queue() {
        let fixture = fixture();
        let files = [fixture.file("a"), fixture.file("b"), fixture.file("c")];
        let server = fixture.server();
        let clients: Vec<_> = files
            .iter()
            .map(|file| deliver_later(&fixture, vec![target(file.clone())], None))
            .collect();
        // Two waits of two seconds put the third hand-over past four seconds, so its client gives up
        // with the request still in hand.
        for client in clients {
            assert_eq!(client.join().unwrap(), Delivered::NotHere);
        }
        assert_eq!(
            taken(&server),
            Vec::<PathBuf>::new(),
            "these clients went to a new window"
        );
    }

    #[test]
    fn a_client_that_gave_up_behind_a_held_request_gets_nothing_opened() {
        let fixture = fixture();
        let (first_file, second_file) = (fixture.file("a"), fixture.file("b"));
        let server = fixture.server();
        let first = deliver_later(&fixture, vec![target(first_file)], None);
        let (_, _held) = next_request(&server).take().unwrap();
        let second = deliver_later(&fixture, vec![target(second_file)], None);
        // The held request keeps the thread for three seconds, which leaves the second hand-over less
        // time than its two-second wait before the client's deadline.
        assert_eq!(first.join().unwrap(), Delivered::Opened);
        assert_eq!(second.join().unwrap(), Delivered::NotHere);
        assert_eq!(
            taken(&server),
            Vec::<PathBuf>::new(),
            "this client went to a new window"
        );
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
        assert_eq!(client.join().unwrap(), Delivered::Opened);
        assert!(at_root.drain().is_empty());

        // A busy hint moves on to the root's socket.
        let client = deliver_later(&fixture, vec![target(file.clone())], Some(hint));
        let (_, answering) = next_request(&at_hint).take().unwrap();
        answering.answer(Answer::Busy);
        let (_, answering) = next_request(&at_root).take().unwrap();
        answering.answer(Answer::Accepted);
        assert_eq!(client.join().unwrap(), Delivered::Opened);

        // A hint nothing listens on, and one that is not a socket at all.
        for hint in [fixture.base.join("gone"), file.clone()] {
            let client = deliver_later(&fixture, vec![target(file.clone())], Some(hint));
            let (_, answering) = next_request(&at_root).take().unwrap();
            answering.answer(Answer::Accepted);
            assert_eq!(client.join().unwrap(), Delivered::Opened);
        }
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
            deliver_with(&open, &root, &dir, Some(&silent), None, euid(), Duration::from_secs(1))
        });
        let (_, answering) = next_request(&server).take().unwrap();
        answering.answer(Answer::Accepted);
        assert_eq!(client.join().unwrap(), Delivered::Opened);
    }

    #[test]
    fn deliver_refuses_a_server_of_another_uid() {
        let fixture = fixture();
        let file = fixture.file("a.rs");
        let server = fixture.server();
        let open = [target(file)];
        let other = euid().wrapping_add(1);
        let delivered = deliver_with(&open, &fixture.root, &fixture.dir, None, None, other, CLIENT_DEADLINE);
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
        let client = std::thread::spawn(move || deliver(&open, &root, &dir, None, Some("token")));
        let mut seen = Vec::new();
        for _ in 0..3 {
            let (open, answering) = next_request(&server).take().unwrap();
            answering.answer(Answer::Accepted);
            seen.push(open);
        }
        assert_eq!(client.join().unwrap(), Delivered::Opened);
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
        let (open, answering) = next_request(&server).take().unwrap();
        assert_eq!(open.path, a);
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
            deliver(&open, &fixture.root, &fixture.dir, None, None),
            Delivered::NotHere
        );
        assert_eq!(
            server.watch.polls.load(Ordering::SeqCst),
            0,
            "nothing connected to the root's socket"
        );
    }

    #[test]
    fn nothing_listening_is_not_here() {
        let fixture = fixture();
        let open = [target(fixture.file("a"))];
        assert_eq!(
            deliver(&open, &fixture.root, &fixture.dir, None, None),
            Delivered::NotHere
        );
        // A crashed window's socket: the file is there and the connect is refused.
        drop(UnixListener::bind(fixture.root_socket()).unwrap());
        assert_eq!(
            deliver(&open, &fixture.root, &fixture.dir, None, None),
            Delivered::NotHere
        );
        assert_eq!(
            deliver(&[], &fixture.root, &fixture.dir, None, None),
            Delivered::NotHere
        );
    }
}
