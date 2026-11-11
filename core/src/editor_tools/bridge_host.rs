//! The window's end of the bridge socket: what an `eitri-editor-bridge` a Codex or ACP agent started connects to.
//!
//! Each tab with an external session has one listener, `ed-<16 hex>.sock`, in the control directory the companion
//! panel uses and with its posture: the directory is private to this user, the socket is 0600, only a peer of this
//! process's effective uid is served, and a socket in the way is reclaimed only when a connect to it is refused, and
//! only the very socket probed. The key in the first frame selects the tab and makes a bridge left over from a closed
//! session fail. It is no secret from this uid: it is on the agent's command line, and the boundary against other users
//! is the directory and the peer check.
//!
//! One worker thread per window does every accept, read and write, in `poll` on the listeners, the connections and a
//! command eventfd, so a peer that stops reading stalls nothing: the GTK side binds (a few syscalls) and queues. What a
//! peer sends reaches the GTK side as [`BridgeNews`] through a channel and the wake. Both directions are bounded: a
//! peer that will not read is closed at [`MAX_QUEUED`], and connections are not read past [`MAX_NEWS`] waiting news.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::bridge_wire::{encode_frame, FrameBuf, Message, PROTOCOL};
use super::step::{BridgeCallId, ToolResult};
use super::words;
use crate::panel_control;
use crate::window_link::fdpass;

/// How long a new connection has to say `hello`, from its accept; slow bytes do not extend it.
const HELLO_WITHIN: Duration = Duration::from_secs(5);

/// How long a closed tab's connection keeps being written to. The bridge reads all the time, so only a peer that has
/// stopped reading uses this up.
const CLOSE_WITHIN: Duration = Duration::from_secs(2);

/// The most bytes waiting for one connection. A bridge reads as they come, so past this the peer has stopped, and
/// its connection is closed rather than kept growing.
const MAX_QUEUED: usize = 8 * super::bridge_wire::MAX_FRAME;

/// News not yet taken above which no connection is read: the socket's own buffer then holds a peer back, and a greeting
/// that waits out [`HELLO_WITHIN`] is closed.
const MAX_NEWS: usize = 256;

/// Connections one tab may have at a time. A tab's agent runs one bridge; the room is for a reconnect that finds the
/// old connection not yet noticed as gone.
const MAX_CONNECTIONS: usize = 16;

/// How long a listener rests after an accept fails for lack of descriptors or memory. The connection stays queued, so
/// `poll` would report it at once and for ever.
const ACCEPT_REST: Duration = Duration::from_millis(100);

const BACKLOG: libc::c_int = 8;
const READ_CHUNK: usize = 64 * 1024;

/// A tab's socket by name. The same name binds again in the other window when the tab moves there.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeName {
    pub hex: String,
    pub key: String,
}

impl BridgeName {
    fn fresh() -> BridgeName {
        BridgeName {
            hex: uuid::Uuid::new_v4().simple().to_string()[..16].to_owned(),
            key: uuid::Uuid::new_v4().simple().to_string(),
        }
    }

    /// A name from the other window goes into a file name, so its shape is checked before it is used.
    pub fn check(&self) -> io::Result<()> {
        let lower_hex = |text: &str, len: usize| {
            text.len() == len && text.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        };
        if lower_hex(&self.hex, 16) && lower_hex(&self.key, 32) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a bridge name is 16 and 32 lowercase hex digits",
            ))
        }
    }
}

/// `BridgeCallId` is `step.rs`'s. `socket`: the `BridgeName.hex` the call came through, which only this session has: a
/// call can still wait here when its tab has started another. `row_id`: `meta.callId`, the model's call id and Codex's
/// item id; never `meta.itemId`, which is another id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BridgeNews {
    Call {
        id: BridgeCallId,
        socket: String,
        tool: String,
        arguments: String,
        row_id: Option<String>,
    },
    Cancelled {
        id: BridgeCallId,
    },
}

/// The sockets of one window's tabs and the worker serving them. Dropping it removes the sockets and lets the worker
/// finish what is queued, as [`BridgeHost::close`] does for each tab.
pub struct BridgeHost {
    dir: PathBuf,
    commands: Sender<Command>,
    /// Made readable after every command, and when taken news let reading go on.
    commands_ready: Arc<OwnedFd>,
    news: Receiver<BridgeNews>,
    pending: Arc<AtomicUsize>,
    /// A tab's socket file, removed here so that a bind of the same name right after a close finds nothing in the way,
    /// whenever the worker gets to the listener.
    bound: RefCell<HashMap<u64, BoundPath>>,
}

impl BridgeHost {
    pub fn start(dir: PathBuf, wake: agent::UiWake) -> io::Result<BridgeHost> {
        BridgeHost::start_for(dir, wake, panel_control::euid())
    }

    /// As [`BridgeHost::start`], serving `uid` in place of the euid, so a test can be a peer of another uid.
    #[cfg(test)]
    fn start_expecting_uid(dir: PathBuf, wake: agent::UiWake, uid: u32) -> io::Result<BridgeHost> {
        BridgeHost::start_for(dir, wake, uid)
    }

    fn start_for(dir: PathBuf, wake: agent::UiWake, uid: u32) -> io::Result<BridgeHost> {
        let (commands, command_queue) = channel();
        let (news_sender, news) = channel();
        let commands_ready = Arc::new(eventfd()?);
        let pending = Arc::new(AtomicUsize::new(0));
        let worker = Worker {
            commands: command_queue,
            commands_ready: commands_ready.clone(),
            uid,
            listeners: Vec::new(),
            connections: Vec::new(),
            connections_made: 0,
            news: news_sender,
            pending: pending.clone(),
            wake,
            stopping: false,
        };
        std::thread::Builder::new()
            .name("eitri-bridge".to_owned())
            .spawn(move || worker.run())?;
        Ok(BridgeHost {
            dir,
            commands,
            commands_ready,
            news,
            pending,
            bound: RefCell::new(HashMap::new()),
        })
    }

    /// Binds a new socket for `tab`, a new session's. A socket the tab already had closes first: a tab has one.
    pub fn open(&self, tab: u64, tools: Vec<Value>) -> io::Result<(BridgeName, PathBuf)> {
        let name = BridgeName::fresh();
        let path = self.bind(tab, &name, tools)?;
        Ok((name, path))
    }

    /// Binds `name`, which another window or this one bound before, for `tab`.
    pub fn adopt(&self, tab: u64, name: &BridgeName, tools: Vec<Value>) -> io::Result<PathBuf> {
        name.check()?;
        self.bind(tab, name, tools)
    }

    fn bind(&self, tab: u64, name: &BridgeName, tools: Vec<Value>) -> io::Result<PathBuf> {
        let welcome = encode_frame(&Message::Welcome { tools }.into_value())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
        let path = socket_path(&self.dir, &name.hex)?;
        panel_control::ensure_dir(&self.dir).map_err(|why| io::Error::new(io::ErrorKind::PermissionDenied, why))?;
        self.close(tab);
        let (fd, bound) = claim(&self.dir, &name.hex, path.clone())?;
        let listener = Listener {
            fd,
            door: Arc::new(Door {
                tab,
                hex: name.hex.clone(),
                key: name.key.clone(),
                welcome,
            }),
            rest_until: None,
        };
        if self.commands.send(Command::Listen(listener)).is_err() {
            bound.remove();
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the bridge worker has stopped",
            ));
        }
        signal(self.commands_ready.as_fd());
        self.bound.borrow_mut().insert(tab, bound);
        Ok(path)
    }

    /// Ends `tab`'s socket: the name is free when this returns, and what is queued for its connections is written
    /// before they close.
    pub fn close(&self, tab: u64) {
        let Some(bound) = self.bound.borrow_mut().remove(&tab) else {
            return;
        };
        bound.remove();
        self.command(Command::Close { tab });
    }

    pub fn take_news(&self) -> Vec<BridgeNews> {
        let news: Vec<BridgeNews> = self.news.try_iter().collect();
        let before = self.pending.fetch_sub(news.len(), Ordering::Relaxed);
        if before >= MAX_NEWS && before - news.len() < MAX_NEWS {
            signal(self.commands_ready.as_fd());
        }
        news
    }

    /// Queues the answer for the call's connection. A connection that has gone takes nothing with it: the bridge has
    /// already told its client.
    pub fn answer(&self, id: &BridgeCallId, result: &ToolResult) {
        self.command(Command::Answer {
            id: id.clone(),
            text: result.text.clone(),
            is_error: result.is_error,
        });
    }

    fn command(&self, command: Command) {
        // A worker that has stopped has no connection left to act on.
        let _ = self.commands.send(command);
        signal(self.commands_ready.as_fd());
    }
}

impl Drop for BridgeHost {
    fn drop(&mut self) {
        for bound in self.bound.borrow().values() {
            bound.remove();
        }
        self.command(Command::Stop);
    }
}

/// `dir/ed-<16 hex>.sock`, checked against the socket path limit.
fn socket_path(dir: &Path, hex: &str) -> io::Result<PathBuf> {
    agent::socket_path::in_dir(dir, &format!("ed-{hex}.sock"))
}

/// `dir/ed-<16 hex>.lock`, held while a socket in the way is judged and reclaimed.
fn lock_path(dir: &Path, hex: &str) -> PathBuf {
    dir.join(format!("ed-{hex}.lock"))
}

/// `(device, inode)` of `path` itself, never of what a link there points at.
fn identity(path: &Path) -> Option<(u64, u64)> {
    path.symlink_metadata().ok().map(|meta| (meta.dev(), meta.ino()))
}

/// A socket file this host made. Removed only while it is still that file: the other window may have bound the name.
struct BoundPath {
    path: PathBuf,
    identity: (u64, u64),
}

impl BoundPath {
    fn remove(&self) {
        if identity(&self.path) == Some(self.identity) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The `sockaddr_un` for `path`, or why it cannot name one.
fn address(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: a zeroed `sockaddr_un` is a valid empty address.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.is_empty() || bytes.len() >= addr.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} cannot name a socket", path.display()),
        ));
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in addr.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let len = (std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1) as libc::socklen_t;
    Ok((addr, len))
}

/// A new stream socket, close-on-exec and non-blocking.
fn stream_socket() -> io::Result<OwnedFd> {
    // SAFETY: plain socket creation; the descriptor is owned below.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just created, owned by nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn listen_at(path: &Path) -> io::Result<OwnedFd> {
    let (addr, len) = address(path)?;
    let fd = stream_socket()?;
    // SAFETY: `addr` is a valid `sockaddr_un` of `len` bytes for the call.
    if unsafe { libc::bind(fd.as_raw_fd(), (&addr as *const libc::sockaddr_un).cast(), len) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on a bound socket this process owns.
    if unsafe { libc::listen(fd.as_raw_fd(), BACKLOG) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// Whether nothing listens on `path`. A non-blocking connect: a listener whose queue is full answers at once too, and
/// this runs on the caller's thread. Only a refusal proves anything; any other answer leaves the socket alone.
fn nothing_listens(path: &Path) -> io::Result<bool> {
    let (addr, len) = address(path)?;
    let fd = stream_socket()?;
    // SAFETY: `addr` is a valid `sockaddr_un` of `len` bytes for the call.
    if unsafe { libc::connect(fd.as_raw_fd(), (&addr as *const libc::sockaddr_un).cast(), len) } == 0 {
        return Ok(false);
    }
    let error = io::Error::last_os_error();
    Ok(matches!(
        error.raw_os_error(),
        Some(libc::ECONNREFUSED) | Some(libc::ENOENT)
    ))
}

/// An exclusive `flock` on the reclaim lock file, released when it is dropped. Taken without waiting: a lock held by
/// another means something else is at work on this name, and nothing is reclaimed.
struct ReclaimLock(#[allow(dead_code)] std::fs::File);

impl ReclaimLock {
    fn take(path: &Path) -> io::Result<ReclaimLock> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.uid() != panel_control::euid() {
            return Err(in_the_way(format!(
                "{} is not this user's regular file; not using it",
                path.display()
            )));
        }
        // The umask took its bits from the mode at `open`, and the next claim needs to open this file again.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        // SAFETY: `file` owns a valid descriptor for the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(in_the_way(format!(
                "{} is held by another claim; not reclaiming",
                path.display()
            )));
        }
        Ok(ReclaimLock(file))
    }
}

fn in_the_way(why: String) -> io::Error {
    io::Error::new(io::ErrorKind::AddrInUse, why)
}

/// Binds `path`, taking a socket in the way only when nothing listens on it, and leaves the socket 0600.
fn claim(dir: &Path, hex: &str, path: PathBuf) -> io::Result<(OwnedFd, BoundPath)> {
    let handle = agent::private_fs::open_dir(dir)?;
    let busy = |error: &io::Error| error.kind() == io::ErrorKind::AddrInUse;
    match listen_at(&path) {
        Ok(fd) => return own(&handle, fd, path),
        Err(error) if busy(&error) => {}
        Err(error) => return Err(error),
    }
    // Held until the new socket is bound, so a reclaim and its bind are never split by another claim.
    let _lock = ReclaimLock::take(&lock_path(dir, hex))?;
    match listen_at(&path) {
        Ok(fd) => return own(&handle, fd, path),
        Err(error) if busy(&error) => {}
        Err(error) => return Err(error),
    }
    let probed = match path.symlink_metadata() {
        Ok(meta) if meta.file_type().is_socket() && meta.uid() == panel_control::euid() => (meta.dev(), meta.ino()),
        _ => {
            return Err(in_the_way(format!(
                "{} is not this user's socket; not touching it",
                path.display()
            )))
        }
    };
    if !nothing_listens(&path)? {
        return Err(in_the_way(format!("{} is in use; not reclaiming it", path.display())));
    }
    if identity(&path) != Some(probed) {
        return Err(in_the_way(format!(
            "{} changed while it was judged; not reclaiming it",
            path.display()
        )));
    }
    std::fs::remove_file(&path)?;
    own(&handle, listen_at(&path)?, path)
}

/// Makes the socket just bound at `path` private through `handle`, the directory it is in, and only while the name is
/// still that socket. When that fails, only that socket is removed, never what a link put at the name meanwhile
/// points at.
fn own(handle: &std::fs::File, fd: OwnedFd, path: PathBuf) -> io::Result<(OwnedFd, BoundPath)> {
    let identity = identity(&path).ok_or_else(|| io::Error::other("the socket file went as it was bound"))?;
    let bound = panel_control::file_identity(identity);
    #[cfg(test)]
    panel_control::before_socket_chmod::run();
    // `bind` leaves the umask's mode on the file.
    if let Err(error) = panel_control::make_control_socket_private(handle, &path, bound) {
        if let Some(name) = path.file_name() {
            let _ = agent::private_fs::remove_if_same(handle, name, bound);
        }
        return Err(error);
    }
    Ok((fd, BoundPath { path, identity }))
}

fn eventfd() -> io::Result<OwnedFd> {
    // SAFETY: `eventfd` takes no pointers; a negative return is an error.
    let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a new descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Makes an eventfd readable. The only failure is a counter at its maximum, which is readable already.
fn signal(fd: BorrowedFd<'_>) {
    let one: u64 = 1;
    // SAFETY: writes the 8 bytes of `one`, which lives for the call.
    unsafe { libc::write(fd.as_raw_fd(), (&one as *const u64).cast(), 8) };
}

/// Makes an eventfd unreadable until the next `signal`.
fn clear(fd: BorrowedFd<'_>) {
    let mut count: u64 = 0;
    // SAFETY: reads at most 8 bytes into `count`, which lives for the call; a non-blocking eventfd at zero fails at once.
    unsafe { libc::read(fd.as_raw_fd(), (&mut count as *mut u64).cast(), 8) };
}

fn poll(fds: &mut [libc::pollfd], timeout_ms: libc::c_int) -> io::Result<()> {
    loop {
        // SAFETY: `fds` is a valid array of `fds.len()` entries for the call.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
        if rc >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn pollfd(fd: BorrowedFd<'_>, events: libc::c_short) -> libc::pollfd {
    libc::pollfd {
        fd: fd.as_raw_fd(),
        events,
        revents: 0,
    }
}

/// What the GTK side asks of the worker, in the order it asked.
enum Command {
    Listen(Listener),
    Close {
        tab: u64,
    },
    Answer {
        id: BridgeCallId,
        text: String,
        is_error: bool,
    },
    Stop,
}

/// What a connection needs of the socket it came through.
struct Door {
    tab: u64,
    hex: String,
    key: String,
    /// The `welcome` frame, made as the socket was bound, so a list too long to send refuses the bind.
    welcome: Vec<u8>,
}

struct Listener {
    fd: OwnedFd,
    door: Arc<Door>,
    rest_until: Option<Instant>,
}

enum Stage {
    Greeting { by: Instant },
    Serving,
    Closing { by: Instant },
}

struct Connection {
    id: u64,
    door: Arc<Door>,
    stream: UnixStream,
    frames: FrameBuf,
    out: Vec<u8>,
    sent: usize,
    stage: Stage,
}

impl Connection {
    fn deadline(&self) -> Option<Instant> {
        match self.stage {
            Stage::Greeting { by } | Stage::Closing { by } => Some(by),
            Stage::Serving => None,
        }
    }

    fn drained(&self) -> bool {
        self.sent == self.out.len()
    }

    fn is_closing(&self) -> bool {
        matches!(self.stage, Stage::Closing { .. })
    }

    fn interest(&self, reading: bool) -> libc::c_short {
        let read = match self.stage {
            // A greeting counts: everything behind its `hello` in one read is served, and a connection that goes
            // straight after leaves its news here and takes its place in the tab's count with it.
            Stage::Greeting { .. } | Stage::Serving => reading,
            Stage::Closing { .. } => false,
        };
        let mut events = 0;
        if read {
            events |= libc::POLLIN;
        }
        if !self.drained() {
            events |= libc::POLLOUT;
        }
        events
    }

    /// Whether the connection stays, once `poll` said `ready` of it.
    fn on_ready(&mut self, ready: libc::c_short, chunk: &mut [u8], news: &mut Vec<BridgeNews>) -> bool {
        if ready & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return false;
        }
        if ready & libc::POLLOUT != 0 && !self.write() {
            return false;
        }
        if ready & libc::POLLIN != 0 {
            if !self.read(chunk, news) {
                return false;
            }
        } else if ready & libc::POLLHUP != 0 {
            return false;
        }
        !(self.is_closing() && self.drained())
    }

    /// Writes what the socket takes. False when the peer is gone.
    fn write(&mut self) -> bool {
        let rest = &self.out[self.sent..];
        // SAFETY: `rest` is valid for `rest.len()` bytes for the call. `MSG_NOSIGNAL`: a peer that went is an error
        // here, never a signal to the process.
        let count = unsafe {
            libc::send(
                self.stream.as_raw_fd(),
                rest.as_ptr().cast(),
                rest.len(),
                libc::MSG_NOSIGNAL,
            )
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            return matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted);
        }
        self.sent += count as usize;
        if self.drained() {
            self.out = Vec::new();
            self.sent = 0;
        }
        true
    }

    /// One read: `poll` reports what is left, and the other connections get their turn. False when it ended.
    fn read(&mut self, chunk: &mut [u8], news: &mut Vec<BridgeNews>) -> bool {
        match (&self.stream).read(chunk) {
            Ok(0) => false,
            Ok(count) => match self.frames.push(&chunk[..count]) {
                Ok(frames) => frames.into_iter().all(|frame| self.take_frame(frame, news)),
                Err(error) => {
                    eprintln!("[editor bridge] connection {} closed: {error}", self.id);
                    false
                }
            },
            Err(error) => matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted),
        }
    }

    /// Takes one frame. False when it closes the connection: anything but a `hello` first, a wrong version or key, or
    /// something a host never receives.
    fn take_frame(&mut self, frame: Value, news: &mut Vec<BridgeNews>) -> bool {
        let serving = matches!(self.stage, Stage::Serving);
        let tab = self.door.tab;
        let id_of = |call: u64, connection: u64| BridgeCallId { tab, connection, call };
        match Message::parse(frame) {
            Ok(Message::Hello { v, key }) if v == PROTOCOL && key == self.door.key => {
                self.stage = Stage::Serving;
                let door = self.door.clone();
                self.queue(&door.welcome)
            }
            Ok(Message::Call {
                id,
                name,
                arguments,
                meta,
            }) if serving => {
                news.push(BridgeNews::Call {
                    id: id_of(id, self.id),
                    socket: self.door.hex.clone(),
                    tool: name,
                    arguments: arguments.to_string(),
                    row_id: meta.get("callId").and_then(Value::as_str).map(str::to_owned),
                });
                true
            }
            Ok(Message::Cancel { id }) if serving => {
                news.push(BridgeNews::Cancelled { id: id_of(id, self.id) });
                true
            }
            Ok(_) => {
                eprintln!("[editor bridge] connection {} closed: an unexpected frame", self.id);
                false
            }
            Err(error) => {
                eprintln!("[editor bridge] connection {} closed: {error}", self.id);
                false
            }
        }
    }

    /// Queues a frame. False when that would pass [`MAX_QUEUED`]: the peer is not reading.
    fn queue(&mut self, frame: &[u8]) -> bool {
        self.out.drain(..self.sent);
        self.sent = 0;
        if self.out.len() + frame.len() > MAX_QUEUED {
            eprintln!(
                "[editor bridge] connection {} closed: its peer stopped reading",
                self.id
            );
            return false;
        }
        self.out.extend_from_slice(frame);
        true
    }
}

struct Worker {
    commands: Receiver<Command>,
    commands_ready: Arc<OwnedFd>,
    uid: u32,
    listeners: Vec<Listener>,
    connections: Vec<Connection>,
    connections_made: u64,
    news: Sender<BridgeNews>,
    pending: Arc<AtomicUsize>,
    wake: agent::UiWake,
    stopping: bool,
}

impl Worker {
    fn run(mut self) {
        let mut chunk = vec![0u8; READ_CHUNK];
        while self.turn(Instant::now(), &mut chunk, poll, Instant::now) {}
    }

    fn turn(
        &mut self,
        now: Instant,
        chunk: &mut [u8],
        polling: impl FnOnce(&mut [libc::pollfd], libc::c_int) -> io::Result<()>,
        accepted_now: impl Fn() -> Instant,
    ) -> bool {
        // Cleared before the queue is read, so a command sent after the last read leaves it readable.
        clear(self.commands_ready.as_fd());
        while let Ok(command) = self.commands.try_recv() {
            self.obey(command, now);
        }
        self.connections
            .retain(|connection| connection.deadline().is_none_or(|by| by > now));
        for listener in &mut self.listeners {
            listener.rest_until = listener.rest_until.filter(|until| *until > now);
        }
        if self.stopping && self.connections.is_empty() {
            return false;
        }

        let reading = self.pending.load(Ordering::Relaxed) < MAX_NEWS;
        let mut polled = Vec::with_capacity(1 + self.listeners.len() + self.connections.len());
        polled.push(pollfd(self.commands_ready.as_fd(), libc::POLLIN));
        for listener in &self.listeners {
            let events = if listener.rest_until.is_some() { 0 } else { libc::POLLIN };
            polled.push(pollfd(listener.fd.as_fd(), events));
        }
        for connection in &self.connections {
            polled.push(pollfd(connection.stream.as_fd(), connection.interest(reading)));
        }
        if let Err(error) = polling(&mut polled, self.timeout_ms(now)) {
            eprintln!("[editor bridge] the worker stopped: {error}");
            return false;
        }

        let (listeners, connections) = polled[1..].split_at(self.listeners.len());
        let mut news = Vec::new();
        let mut ready = connections.iter().map(|polled| polled.revents);
        self.connections.retain_mut(|connection| {
            let revents = ready.next().unwrap_or(0);
            connection.on_ready(revents, chunk, &mut news)
        });
        self.publish(news);
        for (index, polled) in listeners.iter().enumerate() {
            if polled.revents & libc::POLLIN != 0 {
                self.accept(index, accepted_now());
            }
        }
        true
    }

    fn obey(&mut self, command: Command, now: Instant) {
        match command {
            Command::Listen(listener) => self.listeners.push(listener),
            Command::Close { tab } => {
                self.listeners.retain(|listener| listener.door.tab != tab);
                self.connections.retain_mut(|connection| {
                    if connection.door.tab != tab {
                        return true;
                    }
                    connection.stage = Stage::Closing { by: now + CLOSE_WITHIN };
                    !connection.drained()
                });
            }
            Command::Answer { id, text, is_error } => self.answer(id, text, is_error),
            Command::Stop => {
                self.stopping = true;
                self.listeners.clear();
                self.connections.retain_mut(|connection| {
                    connection.stage = Stage::Closing { by: now + CLOSE_WITHIN };
                    !connection.drained()
                });
            }
        }
    }

    /// Queues an answer for its connection; one that is closing or gone gets nothing.
    fn answer(&mut self, id: BridgeCallId, text: String, is_error: bool) {
        let place = self.connections.iter().position(|connection| {
            connection.id == id.connection && connection.door.tab == id.tab && !connection.is_closing()
        });
        let Some(place) = place else {
            return;
        };
        let call = id.call;
        let result = |text: String, is_error: bool| {
            encode_frame(
                &Message::Result {
                    id: call,
                    text,
                    is_error,
                }
                .into_value(),
            )
        };
        let frame = result(text, is_error).or_else(|_| result(words::TOO_LARGE.to_owned(), true));
        let Ok(frame) = frame else {
            return;
        };
        if !self.connections[place].queue(&frame) {
            self.connections.swap_remove(place);
        }
    }

    fn publish(&self, news: Vec<BridgeNews>) {
        if news.is_empty() {
            return;
        }
        // Counted before it is sent, so `take_news` never takes more than was counted.
        self.pending.fetch_add(news.len(), Ordering::Relaxed);
        for one in news {
            let _ = self.news.send(one);
        }
        (self.wake)();
    }

    /// One connection, if one waits: `poll` reports the next. Everything is decided before a byte is read.
    fn accept(&mut self, index: usize, now: Instant) {
        let listener = &mut self.listeners[index];
        // SAFETY: accepting on a listening socket this process owns; the address is not asked for.
        let fd = unsafe {
            libc::accept4(
                listener.fd.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            )
        };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if !matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
            ) {
                eprintln!("[editor bridge] could not accept: {error}");
                listener.rest_until = Some(now + ACCEPT_REST);
            }
            return;
        }
        // SAFETY: a new descriptor the kernel just handed this process.
        let stream = UnixStream::from(unsafe { OwnedFd::from_raw_fd(fd) });
        let door = listener.door.clone();
        match fdpass::peer_cred(stream.as_fd()) {
            Ok((_, uid)) if uid == self.uid => {}
            Ok((pid, uid)) => {
                eprintln!("[editor bridge] process {pid} of uid {uid} connected; closed");
                return;
            }
            Err(error) => {
                eprintln!("[editor bridge] a connection names no peer ({error}); closed");
                return;
            }
        }
        let open = self
            .connections
            .iter()
            .filter(|connection| connection.door.tab == door.tab && !connection.is_closing());
        if open.count() >= MAX_CONNECTIONS {
            eprintln!("[editor bridge] tab {} has too many connections; closed", door.tab);
            return;
        }
        self.connections_made += 1;
        self.connections.push(Connection {
            id: self.connections_made,
            door,
            stream,
            frames: FrameBuf::new(),
            out: Vec::new(),
            sent: 0,
            stage: Stage::Greeting { by: now + HELLO_WITHIN },
        });
    }

    /// Until the nearest deadline; nothing with no deadline, so an idle worker only sleeps.
    fn timeout_ms(&self, now: Instant) -> libc::c_int {
        let deadlines = self.connections.iter().filter_map(Connection::deadline);
        let rests = self.listeners.iter().filter_map(|listener| listener.rest_until);
        match deadlines.chain(rests).min() {
            None => -1,
            // Rounded up: a wait cut short would come straight back with nothing to do.
            Some(at) => {
                let wait = at.saturating_duration_since(now).as_millis().saturating_add(1);
                wait.min(libc::c_int::MAX as u128) as libc::c_int
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::bridge_wire::{read_frame, write_frame, MAX_FRAME};
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc::sync_channel;

    /// How long a test waits for something that must happen. Only a failing test waits this long.
    const PATIENCE: Duration = Duration::from_secs(30);

    /// A private directory under the target dir (the test binary's own place), removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Scratch {
            use std::os::unix::fs::DirBuilderExt;
            let exe = std::env::current_exe().expect("the test binary has a path");
            let build = exe.ancestors().nth(2).expect("target/<profile>/deps/<binary>");
            let dir = build.join(format!("bh-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&dir)
                .expect("makes a directory");
            Scratch(dir)
        }

        fn control(&self) -> PathBuf {
            self.0.join("c")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn counted_wake() -> (agent::UiWake, Arc<AtomicUsize>) {
        let raised = Arc::new(AtomicUsize::new(0));
        let counter = raised.clone();
        let wake: agent::UiWake = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        (wake, raised)
    }

    fn host(scratch: &Scratch) -> BridgeHost {
        BridgeHost::start(scratch.control(), counted_wake().0).expect("starts")
    }

    fn tools() -> Vec<Value> {
        vec![json!({"name": "open_file", "description": "opens", "inputSchema": {"type": "object"}})]
    }

    fn connect(path: &Path) -> UnixStream {
        let stream = UnixStream::connect(path).expect("connects");
        stream.set_read_timeout(Some(PATIENCE)).expect("a timeout");
        stream.set_write_timeout(Some(PATIENCE)).expect("a timeout");
        stream
    }

    fn say(stream: &mut UnixStream, message: Message) {
        write_frame(stream, &message.into_value()).expect("writes");
    }

    fn hear(stream: &mut UnixStream) -> Message {
        let frame = read_frame(stream).expect("reads").expect("a frame");
        Message::parse(frame).expect("a message")
    }

    fn welcomed(path: &Path, key: &str) -> UnixStream {
        let mut stream = connect(path);
        let hello = Message::Hello {
            v: PROTOCOL,
            key: key.to_owned(),
        };
        say(&mut stream, hello);
        assert!(matches!(hear(&mut stream), Message::Welcome { .. }));
        stream
    }

    /// Reads to the end of the stream; a reset is an end too, since a host closing on unread bytes resets its peer.
    fn read_to_the_end(stream: &mut UnixStream) -> usize {
        let mut chunk = [0u8; 64 * 1024];
        let mut total = 0;
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => return total,
                Ok(count) => total += count,
                Err(error) if error.kind() == io::ErrorKind::ConnectionReset => return total,
                Err(error) => panic!("the host kept the connection open: {error}"),
            }
        }
    }

    fn closes_unanswered(stream: &mut UnixStream) {
        assert_eq!(read_to_the_end(stream), 0, "the host answered");
    }

    fn call(id: u64, meta: Value) -> Message {
        Message::Call {
            id,
            name: "open_file".to_owned(),
            arguments: json!({"path": "a.rs"}),
            meta,
        }
    }

    fn news_from(host: &BridgeHost) -> Vec<BridgeNews> {
        let until = Instant::now() + PATIENCE;
        loop {
            let news = host.take_news();
            if !news.is_empty() {
                return news;
            }
            assert!(Instant::now() < until, "no news came");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn call_id(news: &BridgeNews) -> BridgeCallId {
        match news {
            BridgeNews::Call { id, .. } | BridgeNews::Cancelled { id } => id.clone(),
        }
    }

    /// A served connection with one call open, and that call's id.
    fn one_call(host: &BridgeHost, tab: u64) -> (UnixStream, BridgeCallId) {
        let (name, path) = host.open(tab, tools()).expect("opens");
        let mut stream = welcomed(&path, &name.key);
        say(&mut stream, call(1, Value::Null));
        let news = news_from(host);
        (stream, call_id(&news[0]))
    }

    fn file_name(hex: &str, control: &Path) -> PathBuf {
        socket_path(control, hex).expect("a short path")
    }

    fn name() -> BridgeName {
        BridgeName {
            hex: "0123456789abcdef".to_owned(),
            key: "0123456789abcdef0123456789abcdef".to_owned(),
        }
    }

    /// The real worker, driven on this thread with nonblocking polls and a logical clock.
    struct Driven {
        host: BridgeHost,
        worker: Worker,
        now: Instant,
        chunk: Vec<u8>,
        turn_count: usize,
    }

    impl std::ops::Deref for Driven {
        type Target = BridgeHost;
        fn deref(&self) -> &BridgeHost {
            &self.host
        }
    }

    impl Driven {
        fn new(scratch: &Scratch) -> Self {
            let (commands, command_queue) = channel();
            let (news_sender, news) = channel();
            let commands_ready = Arc::new(eventfd().unwrap());
            let pending = Arc::new(AtomicUsize::new(0));
            let host = BridgeHost {
                dir: scratch.control(),
                commands,
                commands_ready: commands_ready.clone(),
                news,
                pending: pending.clone(),
                bound: RefCell::new(HashMap::new()),
            };
            let worker = Worker {
                commands: command_queue,
                commands_ready,
                uid: panel_control::euid(),
                listeners: Vec::new(),
                connections: Vec::new(),
                connections_made: 0,
                news: news_sender,
                pending,
                wake: Arc::new(|| {}),
                stopping: false,
            };
            Self {
                host,
                worker,
                now: Instant::now(),
                chunk: vec![0; READ_CHUNK],
                turn_count: 0,
            }
        }

        fn turn(&mut self) {
            self.turn_at(self.now + Duration::from_micros(1));
        }

        fn turn_at(&mut self, now: Instant) {
            self.turn_count += 1;
            assert!(
                self.turn_count <= 1_000_000,
                "bridge did not finish within 1,000,000 turns"
            );
            self.now = now;
            assert!(self.worker.turn(now, &mut self.chunk, |fds, _| poll(fds, 0), || now));
        }

        fn turns(&mut self, n: usize) {
            for _ in 0..n {
                self.turn();
            }
        }

        fn welcomed(&mut self, path: &Path, key: &str) -> UnixStream {
            let mut stream = connect(path);
            say(
                &mut stream,
                Message::Hello {
                    v: PROTOCOL,
                    key: key.to_owned(),
                },
            );
            self.turns(4);
            assert!(matches!(hear(&mut stream), Message::Welcome { .. }));
            stream
        }

        fn one_call(&mut self) -> (UnixStream, BridgeCallId) {
            let (name, path) = self.open(1, tools()).unwrap();
            self.turn();
            let mut stream = self.welcomed(&path, &name.key);
            let size: libc::c_int = 4096;
            // SAFETY: the socket is owned here and size is a valid integer option value.
            assert_eq!(
                unsafe {
                    libc::setsockopt(
                        self.worker.connections[0].stream.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_SNDBUF,
                        (&size as *const libc::c_int).cast(),
                        std::mem::size_of_val(&size) as libc::socklen_t,
                    )
                },
                0
            );
            say(&mut stream, call(1, Value::Null));
            self.turn();
            let BridgeNews::Call { id, .. } = self.take_news().remove(0) else {
                panic!("not a call")
            };
            (stream, id)
        }
    }

    #[test]
    fn the_socket_is_0600_in_a_private_directory() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        let (name, path) = host.open(1, tools()).expect("opens");
        assert_eq!(path.parent(), Some(scratch.control().as_path()));
        let file_name = path.file_name().expect("a file").to_string_lossy().into_owned();
        assert_eq!(file_name.len(), 24, "{file_name}");
        assert!(file_name.starts_with("ed-"), "{file_name}");
        assert_eq!(name.hex.len(), 16);
        assert_eq!(name.key.len(), 32);

        let meta = path.symlink_metadata().expect("bound");
        assert!(meta.file_type().is_socket());
        assert_eq!(meta.mode() & 0o777, 0o600);
        let dir = scratch.control().symlink_metadata().expect("made");
        assert_eq!(dir.mode() & 0o777, 0o700);

        let open = scratch.0.join("open");
        std::fs::create_dir(&open).expect("makes a directory");
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).expect("widens it");
        let in_a_shared_directory = BridgeHost::start(open, counted_wake().0).expect("starts");
        let error = in_a_shared_directory.open(1, tools()).expect_err("not private");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
    }

    #[test]
    fn a_listener_is_close_on_exec_and_non_blocking() {
        let scratch = Scratch::new();
        panel_control::ensure_dir(&scratch.control()).expect("makes it");
        let fd = listen_at(&file_name("0123456789abcdef", &scratch.control())).expect("binds");
        // SAFETY: reading the flags of a descriptor this test owns.
        let (descriptor, status) = unsafe {
            (
                libc::fcntl(fd.as_raw_fd(), libc::F_GETFD),
                libc::fcntl(fd.as_raw_fd(), libc::F_GETFL),
            )
        };
        assert_ne!(descriptor & libc::FD_CLOEXEC, 0);
        assert_ne!(status & libc::O_NONBLOCK, 0);
    }

    #[test]
    fn a_peer_of_another_uid_is_closed_before_a_byte_is_read() {
        let scratch = Scratch::new();
        let another_uid = panel_control::euid().wrapping_add(1);
        let host = BridgeHost::start_expecting_uid(scratch.control(), counted_wake().0, another_uid).expect("starts");
        let (name, path) = host.open(1, tools()).expect("opens");
        let mut stream = connect(&path);
        // The host may already have closed, so a failed write is the same outcome.
        let hello = Message::Hello {
            v: PROTOCOL,
            key: name.key.clone(),
        };
        let _ = write_frame(&mut stream, &hello.into_value());
        let _ = write_frame(&mut stream, &call(1, Value::Null).into_value());
        closes_unanswered(&mut stream);
        assert!(host.take_news().is_empty());
    }

    #[test]
    fn a_wrong_key_closes_the_connection() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        let (_, path) = host.open(1, tools()).expect("opens");
        let mut stream = connect(&path);
        let hello = Message::Hello {
            v: PROTOCOL,
            key: "not the key".to_owned(),
        };
        say(&mut stream, hello);
        closes_unanswered(&mut stream);
    }

    #[test]
    fn a_wrong_version_closes_it() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        let (name, path) = host.open(1, tools()).expect("opens");
        let mut stream = connect(&path);
        say(
            &mut stream,
            Message::Hello {
                v: PROTOCOL + 1,
                key: name.key,
            },
        );
        closes_unanswered(&mut stream);
    }

    #[test]
    fn no_hello_within_5_s_closes_it() {
        assert_eq!(HELLO_WITHIN, Duration::from_secs(5));
        let scratch = Scratch::new();
        let mut host = Driven::new(&scratch);
        let (_, path) = host.open(1, tools()).expect("opens");
        host.turn();
        let mut stream = connect(&path);
        stream.write_all(&[0, 0]).expect("writes");
        host.turn();
        let accepted = host.now;
        host.turn();
        let by = host.worker.connections[0].deadline().unwrap();
        assert_eq!(by, accepted + HELLO_WITHIN);
        stream.write_all(&[0]).expect("another partial byte");
        host.turn_at(by - Duration::from_millis(1));
        assert_eq!(
            host.worker.connections.len(),
            1,
            "a partial hello stays until its deadline"
        );
        host.turn_at(by);
        assert!(host.worker.connections.is_empty());
        closes_unanswered(&mut stream);
    }

    #[test]
    fn an_oversized_frame_closes_it() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        let (name, path) = host.open(1, tools()).expect("opens");
        let mut before_hello = connect(&path);
        before_hello
            .write_all(&((MAX_FRAME + 1) as u32).to_be_bytes())
            .expect("writes");
        closes_unanswered(&mut before_hello);

        let mut after_hello = welcomed(&path, &name.key);
        after_hello
            .write_all(&((MAX_FRAME + 1) as u32).to_be_bytes())
            .expect("writes");
        closes_unanswered(&mut after_hello);
    }

    #[test]
    fn anything_but_a_hello_first_closes_it() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        let (_, path) = host.open(1, tools()).expect("opens");
        for first in [call(1, Value::Null), Message::Cancel { id: 1 }] {
            let mut stream = connect(&path);
            say(&mut stream, first);
            closes_unanswered(&mut stream);
        }
        assert!(host.take_news().is_empty());
    }

    #[test]
    fn a_frame_a_host_never_receives_closes_it() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        let (name, path) = host.open(1, tools()).expect("opens");
        for frame in [
            json!({"t": "welcome", "tools": []}),
            json!({"t": "result", "id": 1, "text": "x", "is_error": false}),
            json!({"t": "nothing"}),
            json!(1),
        ] {
            let mut stream = welcomed(&path, &name.key);
            write_frame(&mut stream, &frame).expect("writes");
            closes_unanswered(&mut stream);
        }
    }

    #[test]
    fn the_welcome_carries_the_tools() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        let (name, path) = host.open(1, tools()).expect("opens");
        let mut stream = connect(&path);
        let hello = Message::Hello {
            v: PROTOCOL,
            key: name.key,
        };
        say(&mut stream, hello);
        assert_eq!(hear(&mut stream), Message::Welcome { tools: tools() });
    }

    #[test]
    fn a_list_of_tools_too_long_for_a_frame_refuses_the_bind() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        let long = vec![json!({"name": "x".repeat(MAX_FRAME)})];
        let error = host.open(1, long).expect_err("over the cap");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{error}");
    }

    #[test]
    fn a_call_reaches_the_news_and_raises_the_wake() {
        let scratch = Scratch::new();
        let (wake, raised) = counted_wake();
        let host = BridgeHost::start(scratch.control(), wake).expect("starts");
        let (name, path) = host.open(7, tools()).expect("opens");
        let mut stream = welcomed(&path, &name.key);
        assert_eq!(raised.load(Ordering::SeqCst), 0, "a hello is no news");
        say(&mut stream, call(9, json!({"callId": "row"})));
        let news = news_from(&host);
        assert_eq!(news.len(), 1);
        let BridgeNews::Call {
            id,
            tool,
            arguments,
            row_id,
            ..
        } = &news[0]
        else {
            panic!("{news:?}");
        };
        assert_eq!((id.tab, id.call), (7, 9));
        assert_eq!(tool, "open_file");
        let arguments: Value = serde_json::from_str(arguments).expect("JSON text");
        assert_eq!(arguments, json!({"path": "a.rs"}));
        assert_eq!(row_id.as_deref(), Some("row"));
        assert!(raised.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn a_cancel_reaches_the_news_on_the_calls_id() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        let (mut stream, id) = one_call(&host, 1);
        say(&mut stream, Message::Cancel { id: 1 });
        assert_eq!(news_from(&host), vec![BridgeNews::Cancelled { id }]);
    }

    #[test]
    fn row_id_is_call_id_not_item_id() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        let (name, path) = host.open(1, tools()).expect("opens");
        let mut stream = welcomed(&path, &name.key);
        for (meta, expected) in [
            (json!({"callId": "call_1", "itemId": "fc_2"}), Some("call_1")),
            (json!({"itemId": "fc_2"}), None),
            (json!({"callId": 3}), None),
            (Value::Null, None),
        ] {
            say(&mut stream, call(1, meta));
            let news = news_from(&host);
            let BridgeNews::Call { row_id, .. } = &news[0] else {
                panic!("{news:?}");
            };
            assert_eq!(row_id.as_deref(), expected);
        }
    }

    #[test]
    fn an_answer_reaches_the_connection() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        let (mut stream, id) = one_call(&host, 1);
        let result = ToolResult {
            text: "opened".to_owned(),
            is_error: false,
        };
        host.answer(&id, &result);
        let expected = Message::Result {
            id: 1,
            text: "opened".to_owned(),
            is_error: false,
        };
        assert_eq!(hear(&mut stream), expected);
    }

    #[test]
    fn an_answer_for_another_tab_or_connection_goes_nowhere() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        let (mut stream, id) = one_call(&host, 1);
        let result = |text: &str| ToolResult {
            text: text.to_owned(),
            is_error: false,
        };
        let another_tab = BridgeCallId { tab: 2, ..id.clone() };
        let another_connection = BridgeCallId {
            connection: id.connection + 1,
            ..id.clone()
        };
        host.answer(&another_tab, &result("wrong"));
        host.answer(&another_connection, &result("wrong"));
        host.answer(&id, &result("right"));
        let Message::Result { text, .. } = hear(&mut stream) else {
            panic!("not a result");
        };
        assert_eq!(text, "right");
    }

    #[test]
    fn an_answer_over_the_frame_cap_says_so() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        let (mut stream, id) = one_call(&host, 1);
        let result = ToolResult {
            text: "x".repeat(MAX_FRAME),
            is_error: false,
        };
        host.answer(&id, &result);
        let expected = Message::Result {
            id: 1,
            text: words::TOO_LARGE.to_owned(),
            is_error: true,
        };
        assert_eq!(hear(&mut stream), expected);
    }

    #[test]
    fn close_writes_what_is_queued_first() {
        let scratch = Scratch::new();
        let mut host = Driven::new(&scratch);
        let (mut stream, worker_stream) = UnixStream::pair().unwrap();
        worker_stream.set_nonblocking(true).unwrap();
        // Keep the same extra reference a concurrent fork can retain until its exec.
        let inherited = worker_stream.try_clone().unwrap();
        host.worker.connections.push(Connection {
            id: 1,
            door: Arc::new(Door {
                tab: 1,
                hex: name().hex,
                key: name().key,
                welcome: Vec::new(),
            }),
            stream: worker_stream,
            frames: FrameBuf::new(),
            out: Vec::new(),
            sent: 0,
            stage: Stage::Serving,
        });
        let id = BridgeCallId {
            tab: 1,
            connection: 1,
            call: 1,
        };
        let result = ToolResult {
            text: "x".repeat(900 * 1024),
            is_error: false,
        };
        host.answer(&id, &result);
        host.command(Command::Close { tab: 1 });
        stream.set_nonblocking(true).unwrap();
        let mut bytes = Vec::new();
        for _ in 0..10_000 {
            host.turn();
            match stream.read_to_end(&mut bytes) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("reading queued output: {error}"),
            }
            if host.worker.connections.is_empty() {
                break;
            }
        }
        assert!(
            host.worker.connections.is_empty(),
            "queued output never drained within 10,000 turns"
        );
        drop(inherited);
        // A concurrent fork can hold the closed socket until exec, after the worker has drained it.
        let until = Instant::now() + Duration::from_secs(60);
        loop {
            match stream.read_to_end(&mut bytes) {
                Ok(_) => break,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < until, "the drained socket never reached EOF");
                    std::thread::yield_now();
                }
                Err(error) => panic!("reading drained output: {error}"),
            }
        }
        let expected = Message::Result {
            id: 1,
            text: result.text,
            is_error: false,
        };
        assert_eq!(bytes, encode_frame(&expected.into_value()).unwrap());
    }

    #[test]
    fn close_does_not_wait_for_ever_for_a_peer_that_never_reads() {
        let scratch = Scratch::new();
        let mut host = Driven::new(&scratch);
        let (mut stream, id) = host.one_call();
        let frame_text = 900 * 1024;
        host.answer(
            &id,
            &ToolResult {
                text: "x".repeat(frame_text),
                is_error: false,
            },
        );
        host.close(1);
        host.turn();
        assert_eq!(
            host.worker.connections.len(),
            1,
            "output is still queued for the nonreader"
        );
        let by = host.worker.connections[0].deadline().unwrap();
        assert_eq!(by, host.now + CLOSE_WITHIN);
        host.turn_at(by - Duration::from_millis(1));
        assert_eq!(host.worker.connections.len(), 1, "the grace period has not ended");
        host.turn_at(by);
        assert!(
            host.worker.connections.is_empty(),
            "the close deadline frees the connection"
        );
        assert!(read_to_the_end(&mut stream) < frame_text);
    }

    #[test]
    fn close_frees_the_name_at_once() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        let (name, path) = host.open(1, tools()).expect("opens");
        let _stream = welcomed(&path, &name.key);
        host.close(1);
        assert!(path.symlink_metadata().is_err());
        assert_eq!(host.adopt(1, &name, tools()).expect("binds again"), path);
    }

    #[test]
    fn a_reconnect_across_close_and_adopt_keeps_the_name() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        let (name, path) = host.open(1, tools()).expect("opens");
        let mut before = welcomed(&path, &name.key);
        host.close(1);
        closes_unanswered(&mut before);

        let adopted = host.adopt(2, &name, tools()).expect("binds the same name");
        assert_eq!(adopted, path);
        let mut after = welcomed(&path, &name.key);
        say(&mut after, call(1, Value::Null));
        assert_eq!(call_id(&news_from(&host)[0]).tab, 2);
    }

    #[test]
    fn a_tab_has_one_socket() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        let (name, first) = host.open(1, tools()).expect("opens");
        let mut stream = welcomed(&first, &name.key);
        let (_, second) = host.open(1, tools()).expect("opens again");
        assert_ne!(first, second);
        assert!(first.symlink_metadata().is_err());
        closes_unanswered(&mut stream);
    }

    #[test]
    fn dropping_the_host_removes_its_sockets_and_ends_its_connections() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        let (name, path) = host.open(1, tools()).expect("opens");
        let mut stream = welcomed(&path, &name.key);
        drop(host);
        assert!(path.symlink_metadata().is_err());
        closes_unanswered(&mut stream);
    }

    #[test]
    fn a_close_leaves_a_socket_the_other_window_has_bound_under_the_name() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        let (_, path) = host.open(1, tools()).expect("opens");
        std::fs::remove_file(&path).expect("removes");
        let other_window = UnixListener::bind(&path).expect("binds the name");
        host.close(1);
        assert!(path.symlink_metadata().is_ok(), "that socket is not this host's");
        drop(other_window);
    }

    #[test]
    fn a_live_listener_in_the_way_is_not_reclaimed() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        panel_control::ensure_dir(&scratch.control()).expect("makes it");
        let path = file_name(&name().hex, &scratch.control());
        let live = UnixListener::bind(&path).expect("binds");
        let error = host.adopt(1, &name(), tools()).expect_err("in use");
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse, "{error}");
        let _stream = UnixStream::connect(&path).expect("the listener is still there");
        live.set_nonblocking(true).expect("non-blocking");
        live.accept().expect("the connection reached it");
    }

    #[test]
    fn a_dead_socket_in_the_way_is_reclaimed() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        panel_control::ensure_dir(&scratch.control()).expect("makes it");
        let path = file_name(&name().hex, &scratch.control());
        drop(UnixListener::bind(&path).expect("binds"));
        assert!(path.symlink_metadata().is_ok(), "the file outlives its listener");
        assert_eq!(host.adopt(1, &name(), tools()).expect("reclaims"), path);
        welcomed(&path, &name().key);
    }

    #[test]
    fn anything_else_in_the_way_is_left_alone() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        panel_control::ensure_dir(&scratch.control()).expect("makes it");
        let path = file_name(&name().hex, &scratch.control());
        std::fs::write(&path, "a file").expect("writes");
        let error = host.adopt(1, &name(), tools()).expect_err("not a socket");
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse, "{error}");
        assert_eq!(std::fs::read_to_string(&path).expect("reads"), "a file");

        std::fs::remove_file(&path).expect("removes");
        std::os::unix::fs::symlink(scratch.0.join("elsewhere"), &path).expect("links");
        host.adopt(1, &name(), tools()).expect_err("a link");
        assert!(path.symlink_metadata().expect("still").file_type().is_symlink());
    }

    #[test]
    fn a_held_reclaim_lock_stops_a_reclaim() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        panel_control::ensure_dir(&scratch.control()).expect("makes it");
        let path = file_name(&name().hex, &scratch.control());
        drop(UnixListener::bind(&path).expect("binds"));
        let lock = ReclaimLock::take(&lock_path(&scratch.control(), &name().hex)).expect("locks");
        let error = host.adopt(1, &name(), tools()).expect_err("another claim");
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse, "{error}");
        drop(lock);
        host.adopt(1, &name(), tools()).expect("reclaims");
    }

    #[test]
    fn a_link_put_at_the_socket_between_bind_and_chmod_is_refused_and_its_target_keeps_its_mode() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        let path = file_name(&name().hex, &scratch.control());
        let target = scratch.0.join("target");
        std::fs::write(&target, "kept").expect("writes");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).expect("sets it");
        let (planted_path, planted_target) = (path.clone(), target.clone());
        panel_control::before_socket_chmod::set(Box::new(move || {
            std::fs::remove_file(&planted_path).expect("removes the socket");
            std::os::unix::fs::symlink(&planted_target, &planted_path).expect("links");
        }));
        let adopted = host.adopt(1, &name(), tools());
        assert_eq!(std::fs::metadata(&target).expect("still there").mode() & 0o777, 0o640);
        assert!(adopted.is_err(), "{adopted:?}");
        assert!(path.symlink_metadata().expect("still").file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&target).expect("reads"), "kept");
    }

    /// The file outlives the claim, and a strict umask takes bits from the mode asked for when it is made: a later
    /// claim opens it to read and write, so what mode it ends with cannot be left to the umask.
    #[test]
    fn the_reclaim_lock_is_left_0600_whatever_it_was_made_with() {
        let scratch = Scratch::new();
        panel_control::ensure_dir(&scratch.control()).expect("makes it");
        let path = lock_path(&scratch.control(), &name().hex);
        std::fs::File::create(&path).expect("makes it");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("sets it");
        drop(ReclaimLock::take(&path).expect("locks"));
        let meta = std::fs::metadata(&path).expect("still there");
        assert_eq!(meta.mode() & 0o777, 0o600);
    }

    #[test]
    fn a_path_over_the_limit_is_refused() {
        let scratch = Scratch::new();
        let host_dir = scratch.0.join("d".repeat(agent::socket_path::MAX_SOCKET_PATH_BYTES));
        std::fs::create_dir(&host_dir).expect("makes a directory");
        let host = BridgeHost::start(host_dir.clone(), counted_wake().0).expect("starts");
        let error = host.open(1, tools()).expect_err("too long");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{error}");
        assert_eq!(std::fs::read_dir(&host_dir).expect("lists").count(), 0);
    }

    #[test]
    fn a_name_of_another_shape_is_refused() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        let good = name();
        let bad_names = [
            BridgeName {
                hex: "../../../../etc/x".to_owned(),
                ..good.clone()
            },
            BridgeName {
                hex: "0123456789ABCDEF".to_owned(),
                ..good.clone()
            },
            BridgeName {
                hex: good.hex[1..].to_owned(),
                ..good.clone()
            },
            BridgeName {
                key: "short".to_owned(),
                ..good.clone()
            },
        ];
        for bad in bad_names {
            let error = host.adopt(1, &bad, tools()).expect_err("no such name");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{bad:?}");
        }
    }

    #[test]
    fn nothing_on_the_gtk_side_blocks() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        let (mut peer, id) = one_call(&host, 1);
        let answers = 3 * MAX_QUEUED / MAX_FRAME;
        let result = ToolResult {
            text: "x".repeat(MAX_FRAME / 2),
            is_error: false,
        };
        let (returned, all_returned) = sync_channel(1);
        std::thread::spawn(move || {
            for _ in 0..answers {
                host.answer(&id, &result);
                host.take_news();
            }
            host.close(1);
            let _ = returned.send(host);
        });
        let _host = all_returned.recv_timeout(PATIENCE).expect("every call returned");
        // Closed at the cap: what was written is what the socket held, well under everything answered.
        assert!(read_to_the_end(&mut peer) < answers * MAX_FRAME / 2);
    }

    /// A hello, calls behind it in one write, and a frame that ends the connection. What the calls left behind is all such
    /// a connection is for, and connecting again has no limit of its own.
    fn hello_calls_and_an_end(key: &str, calls: u64) -> Vec<u8> {
        let hello = Message::Hello {
            v: PROTOCOL,
            key: key.to_owned(),
        };
        let mut bytes = encode_frame(&hello.into_value()).expect("encodes");
        for id in 0..calls {
            bytes.extend(encode_frame(&call(id, Value::Null).into_value()).expect("encodes"));
        }
        let end = Message::Welcome { tools: Vec::new() };
        bytes.extend(encode_frame(&end.into_value()).expect("encodes"));
        bytes
    }

    #[test]
    fn a_new_connection_is_not_read_past_what_the_news_can_hold() {
        let scratch = Scratch::new();
        let mut host = Driven::new(&scratch);
        let (name, path) = host.open(1, tools()).expect("opens");
        host.turn();
        let mut filler = host.welcomed(&path, &name.key);
        for id in 0..MAX_NEWS as u64 {
            say(&mut filler, call(id, Value::Null));
        }
        host.turns(64);
        assert_eq!(host.pending.load(Ordering::Relaxed), MAX_NEWS);

        for _ in 0..8 {
            let mut stream = connect(&path);
            stream.write_all(&hello_calls_and_an_end(&name.key, 8)).expect("writes");
        }
        for _ in 0..64 {
            host.turn();
            assert_eq!(host.pending.load(Ordering::Relaxed), MAX_NEWS);
        }

        let mut waiting = connect(&path);
        let hello = Message::Hello {
            v: PROTOCOL,
            key: name.key.clone(),
        };
        say(&mut waiting, hello);
        for _ in 0..64 {
            host.turn();
            assert_eq!(host.pending.load(Ordering::Relaxed), MAX_NEWS);
        }
        waiting.set_nonblocking(true).unwrap();
        assert_eq!(waiting.read(&mut [0]).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        waiting.set_nonblocking(false).unwrap();
        assert_eq!(host.take_news().len(), MAX_NEWS);
        host.turns(64);
        assert!(
            matches!(hear(&mut waiting), Message::Welcome { .. }),
            "taken, the rest are read"
        );
    }

    #[test]
    fn calls_are_not_read_past_what_the_news_can_hold() {
        let scratch = Scratch::new();
        let mut host = Driven::new(&scratch);
        let (mut stream, worker_stream) = UnixStream::pair().unwrap();
        worker_stream.set_nonblocking(true).unwrap();
        host.worker.connections.push(Connection {
            id: 1,
            door: Arc::new(Door {
                tab: 1,
                hex: name().hex,
                key: name().key,
                welcome: Vec::new(),
            }),
            stream: worker_stream,
            frames: FrameBuf::new(),
            out: Vec::new(),
            sent: 0,
            stage: Stage::Serving,
        });
        // Feed one frame per turn so the queue reaches the exact limit without a read-chunk overshoot.
        for id in 0..MAX_NEWS {
            say(&mut stream, call(id as u64, Value::Null));
            host.turn();
            assert_eq!(host.pending.load(Ordering::Relaxed), id + 1);
        }
        say(&mut stream, call(MAX_NEWS as u64, Value::Null));
        let mut ready = [pollfd(host.worker.connections[0].stream.as_fd(), libc::POLLIN)];
        poll(&mut ready, 0).unwrap();
        assert_ne!(ready[0].revents & libc::POLLIN, 0, "another call is on the socket");
        for _ in 0..64 {
            host.turn();
            assert_eq!(host.pending.load(Ordering::Relaxed), MAX_NEWS);
            assert_eq!(host.worker.connections.len(), 1);
            assert_eq!(host.worker.connections[0].interest(false) & libc::POLLIN, 0);
        }
        assert_eq!(host.take_news().len(), MAX_NEWS);
        host.turns(64);
        let news = host.take_news();
        assert_eq!(news.len(), 1, "draining lets the waiting call through");
        assert_eq!(call_id(&news[0]).call, MAX_NEWS as u64);
    }
}
