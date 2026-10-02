//! The per-project control socket: how a second `eitri-companion` for the same project finds the first
//! one, and how a panel tells an editor that is not its own to attach.
//!
//! One companion panel runs per project. Whoever binds `p-<16 hex>.sock` first owns it and writes
//! its pid beside it; everyone after connects, sends one JSON line (`{"v":1,"attach":"<nvim socket>"}`
//! or `{"v":1,"raise":true}`), reads one reply line (`ok` or `refused: <why>`) and leaves. The panel's
//! thread answers on its own and only then hands the request to the GTK side through a channel, so
//! a panel that is busy, or has not polled yet, still answers its callers within a second.
//!
//! This is a local trust boundary that also deletes files, so every step is deliberately narrow:
//!
//! - The directory is `$XDG_RUNTIME_DIR/eitri` or `<tmp>/eitri`, created 0700 and then required to
//!   be a real directory of this user that nobody else can write to. A directory this code did not
//!   create is never chmodded or removed; it is refused with a reason. When `XDG_RUNTIME_DIR` is
//!   unset and `<tmp>/eitri` belongs to someone else, that is an error, not a reason to pick another
//!   shared directory.
//! - Only a connection whose peer has this process's effective uid is served at all.
//! - A request is one line of at most 4096 bytes that must arrive within one second in total, and it
//!   must have exactly the shape of one of the two requests. Anything else is refused by name.
//!   Versioning goes through `v`, never through extra keys.
//! - An `attach` address must be an absolute path to a real Unix socket owned by this user, never a
//!   symbolic link and never a TCP address. The path is validated here, not opened: the panel
//!   connects later, and swapping the socket in between takes write access to nvim's own directory.
//! - A socket that is in the way is reclaimed only when it is demonstrably stale: it is a socket of
//!   this user, no live pid is recorded for it, nothing answers on it and a connect is refused
//!   (`ECONNREFUSED`/`ENOENT`; any other failure proves nothing and removes nothing). A live pid that
//!   does not answer is reported ([`Claim::Unresponsive`]) and nothing is removed; so is a socket
//!   that still has a listener but no readable pid. Reclaiming runs under an exclusive `flock` on
//!   `p-<key>.lock`, so two claimants cannot both reclaim and delete each other's new socket, and
//!   the removal is of the very socket (device and inode) that was probed. A recorded pid that
//!   was reused by an unrelated process keeps the claim unresponsive until that process exits, which
//!   is the price of never deleting a live panel's socket.

use crate::instance_dir::ConnectOutcome;
use std::cell::Cell;
use std::ffi::OsStr;
use std::fmt;
use std::fs::DirBuilder;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The only protocol version this build speaks. A request with another `v` is refused by name.
pub const PROTOCOL_VERSION: u64 = 1;

/// The longest request line, without its newline.
const MAX_LINE: usize = 4096;
/// How long one connection may take to deliver its request line, in total.
const REQUEST_DEADLINE: Duration = Duration::from_secs(1);
/// How long a claimant waits for the running panel's reply, connect included.
const FORWARD_DEADLINE: Duration = Duration::from_secs(2);
/// How long [`ControlServer::cleanup`] waits for the server thread before it stops waiting.
const CLEANUP_WAIT: Duration = Duration::from_millis(200);
/// How long a claimant waits for another claimant's reclaim before giving up. The holder spends at
/// most two forward deadlines and one recheck in its probe.
const RECLAIM_LOCK_WAIT: Duration = Duration::from_secs(8);
/// The most rounds of bind, probe and reclaim one claim goes through.
const MAX_CLAIM_ROUNDS: u32 = 6;
/// How long the server thread sleeps when nobody is connecting.
const IDLE_SLEEP: Duration = Duration::from_millis(50);

/// What a request asks the running panel to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Attach to the nvim listening on this socket path.
    Attach(PathBuf),
    /// Bring the panel to the front; there is nothing to attach.
    Raise,
}

/// What the running panel answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Ok,
    /// The panel understood the line and said no, with its reason.
    Refused(String),
}

/// The outcome of [`claim`].
#[derive(Debug)]
pub enum Claim {
    /// This process is the panel for the project. Act on the request yourself: a claim never
    /// delivers a request to its own owner.
    Ours(ControlServer),
    /// A panel is already running and answered.
    Forwarded(Reply),
    /// A panel's pid is alive (or something still listens) but nothing answered within the deadline.
    /// Nothing was removed. `pid` is 0 when it is not known.
    Unresponsive { pid: u32 },
}

/// The directory the control files live in: `<runtime dir>/eitri` when the runtime dir is set,
/// non-empty and absolute, else `<tmp>/eitri`.
pub fn control_dir(xdg_runtime_dir: Option<&OsStr>, tmp: &Path) -> PathBuf {
    match xdg_runtime_dir {
        Some(runtime) if Path::new(runtime).is_absolute() => Path::new(runtime).join("eitri"),
        _ => tmp.join("eitri"),
    }
}

/// The 16 hex digits that name this project's files, the same key the layout files use.
fn key(project_root: &Path) -> String {
    agent::conversation_id_for_cwd(project_root)[..16].to_owned()
}

/// `dir/p-<16 hex>.sock`, checked against the 103-byte socket path limit macOS has.
pub fn socket_path(dir: &Path, project_root: &Path) -> io::Result<PathBuf> {
    agent::socket_path::in_dir(dir, &format!("p-{}.sock", key(project_root)))
}

/// `dir/p-<16 hex>.pid`, the pid of the panel that owns the socket.
pub fn pid_path(dir: &Path, project_root: &Path) -> PathBuf {
    dir.join(format!("p-{}.pid", key(project_root)))
}

/// `dir/p-<16 hex>.lock`, the file claimants take an exclusive lock on while they reclaim.
fn lock_path(dir: &Path, project_root: &Path) -> PathBuf {
    dir.join(format!("p-{}.lock", key(project_root)))
}

/// This process's effective uid: what a socket file's owner and a peer's credentials both carry.
/// (`agent::private_fs::current_uid` is the real uid, which differs under setuid.)
fn euid() -> u32 {
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    unsafe { libc::geteuid() }
}

/// Creates `dir` 0700 if it is missing, then requires it to be a private directory of this user.
/// A directory that already existed is judged, never changed.
fn ensure_dir(dir: &Path) -> Result<(), String> {
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {
            // Created just now, so narrowing it is ours to do: a strict umask may have taken bits
            // the owner needs. On the open handle, and without following a link swapped in meanwhile.
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
                .open(dir)
                .and_then(|handle| handle.set_permissions(std::fs::Permissions::from_mode(0o700)))
                .map_err(|e| format!("could not set the mode of {}: {e}", dir.display()))?;
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("could not create {}: {e}", dir.display())),
    }
    let meta = dir
        .symlink_metadata()
        .map_err(|e| format!("could not read {}: {e}", dir.display()))?;
    let why = if meta.file_type().is_symlink() {
        Some("a symbolic link".to_owned())
    } else if !meta.is_dir() {
        Some("not a directory".to_owned())
    } else if meta.uid() != euid() {
        Some(format!("owned by uid {}", meta.uid()))
    } else if meta.mode() & 0o022 != 0 {
        Some(format!("mode 0{:o}", meta.mode() & 0o7777))
    } else {
        None
    };
    match why {
        None => Ok(()),
        Some(why) => Err(format!(
            "{} is not a private directory of this user ({why})",
            dir.display()
        )),
    }
}

/// The uid of the process on the other end of `stream`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    // SAFETY: `ucred` is plain old data and `len` is its exact size; `getsockopt` fills at most that.
    unsafe {
        let mut cred: libc::ucred = std::mem::zeroed();
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let rc = libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        );
        if rc == 0 {
            Ok(cred.uid)
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

/// The uid of the process on the other end of `stream`.
#[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let (mut uid, mut gid): (libc::uid_t, libc::gid_t) = (0, 0);
    // SAFETY: both out-pointers are valid for the call.
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0 {
        Ok(uid)
    } else {
        Err(io::Error::last_os_error())
    }
}

// A platform with no way to ask who is on the other end must not build: a stub that answered "us"
// would let any local user drive the panel.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
compile_error!("panel_control needs a way to read a Unix socket peer's uid on this platform");

/// Why [`read_line`] gave no line.
#[derive(Debug, PartialEq, Eq)]
enum LineFail {
    /// More than [`MAX_LINE`] bytes arrived before a newline.
    TooLong,
    /// The peer closed after sending something that never became a line.
    Partial,
    /// The peer closed having sent nothing.
    Empty,
    TimedOut,
    Io,
}

/// Reads one `\n`-terminated line of at most `max` bytes, all of it before `deadline`: a peer that
/// trickles one byte at a time cannot hold the caller longer than the deadline. Each read is given
/// only the time that is left, never a fresh allowance.
fn read_line(stream: &UnixStream, deadline: Instant, max: usize) -> Result<Vec<u8>, LineFail> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    let mut reader = stream;
    loop {
        if let Some(end) = buf.iter().position(|&b| b == b'\n') {
            buf.truncate(end);
            return if buf.len() > max {
                Err(LineFail::TooLong)
            } else {
                Ok(buf)
            };
        }
        if buf.len() > max {
            return Err(LineFail::TooLong);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(LineFail::TimedOut);
        }
        // A zero timeout is an error to set, and means "no timeout" to some platforms.
        if stream
            .set_read_timeout(Some(left.max(Duration::from_millis(1))))
            .is_err()
        {
            return Err(LineFail::Io);
        }
        match reader.read(&mut chunk) {
            Ok(0) if buf.is_empty() => return Err(LineFail::Empty),
            Ok(0) => return Err(LineFail::Partial),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                return Err(LineFail::TimedOut);
            }
            Err(_) => return Err(LineFail::Io),
        }
    }
}

/// `request` as the one line it is sent as.
fn encode(request: &Request) -> Result<String, String> {
    let value = match request {
        Request::Attach(path) => {
            let path = path
                .to_str()
                .ok_or_else(|| format!("{} is not valid UTF-8", path.display()))?;
            serde_json::json!({ "v": PROTOCOL_VERSION, "attach": path })
        }
        Request::Raise => serde_json::json!({ "v": PROTOCOL_VERSION, "raise": true }),
    };
    Ok(format!("{value}\n"))
}

/// The request one line says, or the reason it is refused (without the `refused: ` prefix).
fn parse_request(line: &[u8]) -> Result<Request, String> {
    let malformed = || "malformed request".to_owned();
    let value: serde_json::Value = serde_json::from_slice(line).map_err(|_| malformed())?;
    let object = value.as_object().ok_or_else(malformed)?;
    let version = object
        .get("v")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(malformed)?;
    if version != PROTOCOL_VERSION {
        return Err(format!("unsupported protocol version {version}"));
    }
    // Exactly one of the two shapes. Extra keys are refused too: this is a trust boundary, and a
    // new capability is a new version.
    match (object.len(), object.get("attach"), object.get("raise")) {
        (2, Some(serde_json::Value::String(path)), None) => Ok(Request::Attach(PathBuf::from(path))),
        (2, None, Some(serde_json::Value::Bool(true))) => Ok(Request::Raise),
        _ => Err(malformed()),
    }
}

/// Whether `addr` may be attached to: an absolute path to a Unix socket of `euid`, not a link.
/// The path is judged, not opened.
pub fn validate_nvim_addr(addr: &Path, euid: u32) -> Result<(), String> {
    let shown = addr.display();
    if !addr.is_absolute() {
        let text = addr.to_string_lossy();
        let looks_like_tcp = text.rsplit_once(':').is_some_and(|(host, port)| {
            !host.is_empty() && !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit())
        });
        return Err(if looks_like_tcp {
            format!("{shown} is not a socket path (TCP addresses are refused)")
        } else {
            format!("{shown} is not an absolute path")
        });
    }
    let meta = match addr.symlink_metadata() {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(format!("{shown} does not exist")),
        Err(e) => return Err(format!("{shown} cannot be read: {e}")),
    };
    if meta.file_type().is_symlink() {
        return Err(format!("{shown} is a symbolic link, not nvim's own socket"));
    }
    if !meta.file_type().is_socket() {
        return Err(format!("{shown} is not a Unix socket"));
    }
    if meta.uid() != euid {
        return Err(format!("{shown} belongs to another user"));
    }
    Ok(())
}

/// Sends `request` to the panel at `sock` and returns its reply. One deadline covers connect, write
/// and read together, so the worst case is `timeout`, not a sum.
fn send(sock: &Path, request: &Request, timeout: Duration) -> Result<Reply, String> {
    let line = encode(request)?;
    let deadline = Instant::now() + timeout;
    // A connect to a listener whose accept queue is full blocks on Linux, so it runs on a helper
    // thread that is simply abandoned when the deadline passes; it owns nothing but its stream.
    let (tx, rx) = mpsc::channel();
    let target = sock.to_owned();
    std::thread::spawn(move || {
        let _ = tx.send(UnixStream::connect(target));
    });
    let mut stream = match rx.recv_timeout(timeout) {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => return Err(format!("could not connect to {}: {e}", sock.display())),
        Err(_) => return Err(format!("{} did not accept a connection in time", sock.display())),
    };
    let left = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(1));
    stream
        .set_write_timeout(Some(left))
        .and_then(|()| stream.write_all(line.as_bytes()))
        .map_err(|e| format!("could not send to {}: {e}", sock.display()))?;
    let reply =
        read_line(&stream, deadline, MAX_LINE).map_err(|why| format!("no reply from {} ({why:?})", sock.display()))?;
    let reply = String::from_utf8_lossy(&reply);
    if reply == "ok" {
        Ok(Reply::Ok)
    } else if let Some(why) = reply.strip_prefix("refused: ") {
        Ok(Reply::Refused(why.to_owned()))
    } else {
        Err(format!("unexpected reply from {}: {reply}", sock.display()))
    }
}

/// The pid in `pid_file`, if it is a regular file of this user holding one. Never follows a link
/// and never blocks on a special file.
fn read_pid(pid_file: &Path) -> Option<u32> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(pid_file)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.uid() != euid() {
        return None;
    }
    let mut text = String::new();
    file.take(32).read_to_string(&mut text).ok()?;
    text.trim().parse::<u32>().ok().filter(|&pid| pid != 0)
}

/// `(device, inode)` of `path` itself, not of what a link there points at.
fn identity(path: &Path) -> Option<(u64, u64)> {
    path.symlink_metadata().ok().map(|meta| (meta.dev(), meta.ino()))
}

/// Removes `sock` if it is a socket of this user.
fn remove_own_socket(sock: &Path) {
    if matches!(sock.symlink_metadata(), Ok(meta) if meta.file_type().is_socket() && meta.uid() == euid()) {
        let _ = std::fs::remove_file(sock);
    }
}

/// Removes `pid_file` if it is a regular file of this user (a link or a directory there is left alone).
fn remove_own_pid_file(pid_file: &Path) {
    if matches!(pid_file.symlink_metadata(), Ok(meta) if meta.is_file() && meta.uid() == euid()) {
        let _ = std::fs::remove_file(pid_file);
    }
}

/// An exclusive `flock` on the reclaim lock file; closing the file releases it.
struct ReclaimLock(#[allow(dead_code)] std::fs::File);

/// Takes the reclaim lock, waiting up to [`RECLAIM_LOCK_WAIT`] for another claimant to finish. The
/// file is created 0600 if missing and must be a regular file of this user; it is never removed.
fn take_reclaim_lock(lock_file: &Path) -> Result<ReclaimLock, String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(lock_file)
        .map_err(|e| format!("could not open {}: {e}", lock_file.display()))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("could not read {}: {e}", lock_file.display()))?;
    if !meta.is_file() || meta.uid() != euid() {
        return Err(format!(
            "{} is not this user's regular file; not using it",
            lock_file.display()
        ));
    }
    let give_up = Instant::now() + RECLAIM_LOCK_WAIT;
    loop {
        // SAFETY: `file` owns a valid descriptor for the whole call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(ReclaimLock(file));
        }
        let err = io::Error::last_os_error();
        let busy = err.kind() == io::ErrorKind::WouldBlock;
        if !busy && err.kind() != io::ErrorKind::Interrupted {
            return Err(format!("could not lock {}: {err}", lock_file.display()));
        }
        if Instant::now() >= give_up {
            return Err(format!(
                "another claim has held {} for too long; not reclaiming",
                lock_file.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// What the probe of an occupied socket concluded.
enum Probe {
    Answered(Reply),
    Unresponsive(u32),
    /// A leftover, with the `(device, inode)` of the socket that was probed.
    Stale((u64, u64)),
}

/// Decides what an occupied `sock` is: a panel that answers, one that is alive but silent, or a
/// leftover. Removes nothing.
fn probe(sock: &Path, pid_file: &Path, request: Option<&Request>) -> Result<Probe, String> {
    let probed = match sock.symlink_metadata() {
        Ok(meta) if meta.file_type().is_socket() && meta.uid() == euid() => (meta.dev(), meta.ino()),
        _ => return Err(format!("{} is not this user's socket; not touching it", sock.display())),
    };
    let request = request.unwrap_or(&Request::Raise);
    let mut rechecked = false;
    loop {
        if let Ok(reply) = send(sock, request, FORWARD_DEADLINE) {
            return Ok(Probe::Answered(reply));
        }
        let pid = read_pid(pid_file);
        let connect = crate::instance_dir::nonblocking_connect_outcome(sock);
        if let Some(pid) = pid.filter(|&pid| agent::process_probe::pid_is_alive(pid)) {
            // Alive but silent, whether or not a connect would be refused right now.
            return Ok(Probe::Unresponsive(pid));
        }
        match connect {
            ConnectOutcome::NoListener => {}
            // Something accepts connections but no live pid in this namespace owns it; or the
            // connect failed for a reason that says nothing about a listener (a socket whose mode
            // forbids us, no descriptors left). Only a refusal proves a leftover.
            ConnectOutcome::Listener | ConnectOutcome::Unknown => return Ok(Probe::Unresponsive(pid.unwrap_or(0))),
        }
        if pid.is_none() && !rechecked {
            // A winner that has bound but not yet written its pid looks exactly like this; give it a
            // moment rather than deleting a live panel's socket and running two.
            rechecked = true;
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        return Ok(Probe::Stale(probed));
    }
}

/// Becomes the panel for `project_root`, or finds the one that already is.
///
/// `request` is what a caller that is not the owner wants the owner to do; `None` sends
/// [`Request::Raise`]. When this returns [`Claim::Ours`] nothing was sent: the caller acts on its
/// own request.
pub fn claim(dir: &Path, project_root: &Path, request: Option<&Request>) -> Result<Claim, String> {
    ensure_dir(dir)?;
    let sock = socket_path(dir, project_root).map_err(|e| e.to_string())?;
    let pid_file = pid_path(dir, project_root);
    let lock_file = lock_path(dir, project_root);
    // Taken the first time the socket is found occupied and held until this claim returns, so a
    // reclaim, its bind and its pid write are never interleaved with another claimant's.
    let mut lock = None;
    let mut reclaimed = false;
    for _ in 0..MAX_CLAIM_ROUNDS {
        match UnixListener::bind(&sock) {
            Ok(listener) => return serve(listener, sock, pid_file).map(Claim::Ours),
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => {}
            Err(e) => return Err(format!("could not bind {}: {e}", sock.display())),
        }
        if lock.is_none() {
            // Whoever held it may have reclaimed the socket meanwhile: bind again before probing.
            lock = Some(take_reclaim_lock(&lock_file)?);
            continue;
        }
        match probe(&sock, &pid_file, request)? {
            Probe::Answered(reply) => return Ok(Claim::Forwarded(reply)),
            Probe::Unresponsive(pid) => return Ok(Claim::Unresponsive { pid }),
            Probe::Stale(_) if reclaimed => {
                return Err(format!(
                    "the control socket {} came back stale after it was reclaimed",
                    sock.display()
                ));
            }
            Probe::Stale(probed) => {
                if identity(&sock) != Some(probed) {
                    // The socket was replaced while it was being probed: judge the new one.
                    continue;
                }
                remove_own_socket(&sock);
                remove_own_pid_file(&pid_file);
                reclaimed = true;
            }
        }
    }
    Err(format!(
        "the control socket {} kept changing; giving up",
        sock.display()
    ))
}

/// The bound socket's side of a won claim: finishes setting it up and starts the thread.
fn serve(listener: UnixListener, sock: PathBuf, pid_file: PathBuf) -> Result<ControlServer, String> {
    let bound = identity(&sock).ok_or_else(|| format!("could not read {}", sock.display()))?;
    let started = (|| -> Result<ControlServer, String> {
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("could not make {} non-blocking: {e}", sock.display()))?;
        // `bind` leaves the umask's mode on the file.
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("could not set the mode of {}: {e}", sock.display()))?;
        let tmp = pid_file.with_extension("pid.tmp");
        agent::private_fs::write_private(&tmp, format!("{}\n", std::process::id()).as_bytes())
            .and_then(|()| std::fs::rename(&tmp, &pid_file))
            .map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("could not write {}: {e}", pid_file.display())
            })?;
        let stop = Arc::new(AtomicBool::new(false));
        let (requests_tx, requests) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        let thread_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("eitri-panel-control".into())
            .spawn(move || {
                run(listener, &thread_stop, &requests_tx);
                let _ = done_tx.send(());
            })
            .map_err(|e| {
                let _ = std::fs::remove_file(&pid_file);
                format!("could not start the control thread: {e}")
            })?;
        Ok(ControlServer {
            stop,
            done,
            requests,
            socket: sock.clone(),
            pid_file: pid_file.clone(),
            bound,
            cleaned: Cell::new(false),
        })
    })();
    if started.is_err() && identity(&sock) == Some(bound) {
        let _ = std::fs::remove_file(&sock);
    }
    started
}

/// The server thread: accept, serve one connection at a time, stop when told to. It never waits for
/// the GTK side and never touches the files.
fn run(listener: UnixListener, stop: &AtomicBool, requests: &Sender<Request>) {
    let me = euid();
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => serve_one(stream, requests, me),
            // Nothing waiting, or a transient failure (descriptors exhausted, a connection that
            // went away): either way, never spin.
            Err(_) => std::thread::sleep(IDLE_SLEEP),
        }
    }
}

fn reply_line(stream: &UnixStream, line: &str) -> io::Result<()> {
    let mut writer = stream;
    writer.write_all(format!("{line}\n").as_bytes())
}

fn serve_one(stream: UnixStream, requests: &Sender<Request>, me: u32) {
    // A socket accepted from a non-blocking listener inherits that flag on macOS and the BSDs, and
    // every read below would then fail at once.
    if stream.set_nonblocking(false).is_err() || stream.set_write_timeout(Some(Duration::from_secs(1))).is_err() {
        return;
    }
    if !matches!(peer_uid(&stream), Ok(uid) if uid == me) {
        let _ = reply_line(&stream, "refused: not your process");
        return;
    }
    let line = match read_line(&stream, Instant::now() + REQUEST_DEADLINE, MAX_LINE) {
        Ok(line) => line,
        Err(LineFail::TooLong) => {
            let _ = reply_line(&stream, "refused: request too long");
            return;
        }
        Err(LineFail::Partial) => {
            let _ = reply_line(&stream, "refused: malformed request");
            return;
        }
        Err(_) => return,
    };
    let request = match parse_request(&line) {
        Ok(Request::Attach(path)) => match validate_nvim_addr(&path, me) {
            Ok(()) => Request::Attach(path),
            Err(why) => {
                let _ = reply_line(&stream, &format!("refused: {why}"));
                return;
            }
        },
        Ok(raise) => raise,
        Err(why) => {
            let _ = reply_line(&stream, &format!("refused: {why}"));
            return;
        }
    };
    // Deliver only what the caller was told succeeded, so both sides agree on whether it happened.
    // The receiver may already be gone; that is not this thread's problem.
    if reply_line(&stream, "ok").is_ok() {
        let _ = requests.send(request);
    }
}

/// The owner side of a won claim: a thread answering the control socket, and the requests it
/// accepted. Dropping it cleans up.
pub struct ControlServer {
    stop: Arc<AtomicBool>,
    done: Receiver<()>,
    requests: Receiver<Request>,
    socket: PathBuf,
    pid_file: PathBuf,
    bound: (u64, u64),
    cleaned: Cell<bool>,
}

impl ControlServer {
    /// The requests that were answered `ok`, in order. Poll it from the GTK thread; the server
    /// thread never waits on it.
    pub fn requests(&self) -> &Receiver<Request> {
        &self.requests
    }

    /// Stops the thread and removes the socket and the pid file, each only if it is still this
    /// server's: the pid file must still name this process and the socket must still be the very
    /// file this server bound, so a newer panel that reclaimed the path keeps its own. Idempotent.
    ///
    /// The wait for the thread is bounded: this runs on the GTK thread at window close, and a
    /// connection mid-request can hold the thread for about a second.
    pub fn cleanup(&self) {
        if self.cleaned.replace(true) {
            return;
        }
        self.stop.store(true, Ordering::Release);
        let _ = self.done.recv_timeout(CLEANUP_WAIT);
        if read_pid(&self.pid_file) != Some(std::process::id()) {
            return;
        }
        if identity(&self.socket) == Some(self.bound) {
            remove_own_socket(&self.socket);
        }
        remove_own_pid_file(&self.pid_file);
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.cleanup();
    }
}

impl fmt::Debug for ControlServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControlServer")
            .field("socket", &self.socket)
            .field("pid_file", &self.pid_file)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_scratch_dir::ScratchDir;
    use std::io::BufRead;
    use std::os::unix::fs::{symlink, FileTypeExt, MetadataExt, PermissionsExt};

    const MACOS_TMP: &str = "/var/folders/33/0tqfpnyn4z3c049gljzppdv00000gn/T/";

    fn root() -> PathBuf {
        PathBuf::from("/some/project")
    }

    /// A bound, never-accepting stand-in for nvim's own socket, at a path that is not a control path.
    fn fake_nvim(scratch: &Path) -> (PathBuf, UnixListener) {
        let path = scratch.join("nvim");
        let listener = UnixListener::bind(&path).unwrap();
        (path, listener)
    }

    fn ours(claim: Result<Claim, String>) -> ControlServer {
        match claim {
            Ok(Claim::Ours(server)) => server,
            other => panic!("expected Ours, got {other:?}"),
        }
    }

    fn mode(path: &Path) -> u32 {
        path.symlink_metadata().unwrap().mode() & 0o7777
    }

    #[test]
    fn the_control_dir_prefers_xdg_runtime_dir_and_falls_back_to_tmp() {
        let tmp = Path::new("/tmp");
        assert_eq!(
            control_dir(Some(OsStr::new("/run/user/1000")), tmp),
            Path::new("/run/user/1000/eitri")
        );
        for unusable in [Some(OsStr::new("")), Some(OsStr::new("rel/dir")), None] {
            assert_eq!(control_dir(unusable, tmp), Path::new("/tmp/eitri"));
        }
    }

    #[test]
    fn the_socket_path_is_78_bytes_under_macos_tmpdir() {
        let dir = control_dir(None, Path::new(MACOS_TMP));
        assert_eq!(socket_path(&dir, &root()).unwrap().as_os_str().len(), 78);
        assert!(pid_path(&dir, &root()).as_os_str().len() < 78);
    }

    #[test]
    fn a_first_claim_owns_the_socket_and_writes_its_pid_0600() {
        let scratch = ScratchDir::new("eitri-pc", "first");
        let dir = scratch.join("c");
        let server = ours(claim(&dir, &root(), None));
        assert_eq!(mode(&dir), 0o700);
        let sock = socket_path(&dir, &root()).unwrap();
        let pid_file = pid_path(&dir, &root());
        assert!(sock.symlink_metadata().unwrap().file_type().is_socket());
        assert_eq!(mode(&sock), 0o600);
        assert_eq!(
            std::fs::read_to_string(&pid_file).unwrap(),
            format!("{}\n", std::process::id())
        );
        assert_eq!(mode(&pid_file), 0o600);
        assert!(!pid_file.with_extension("pid.tmp").exists());
        server.cleanup();
        assert!(!sock.exists() && !pid_file.exists());
        server.cleanup();
    }

    #[test]
    fn a_second_claim_forwards_its_attach_and_the_first_receives_it() {
        let scratch = ScratchDir::new("eitri-pc", "attach");
        let dir = scratch.join("c");
        let first = ours(claim(&dir, &root(), None));
        let (nvim, _listener) = fake_nvim(&scratch);
        let second = claim(&dir, &root(), Some(&Request::Attach(nvim.clone())));
        assert!(matches!(second, Ok(Claim::Forwarded(Reply::Ok))), "{second:?}");
        assert_eq!(
            first.requests().recv_timeout(Duration::from_secs(2)),
            Ok(Request::Attach(nvim))
        );
    }

    #[test]
    fn a_second_claim_without_an_address_sends_raise() {
        let scratch = ScratchDir::new("eitri-pc", "raise");
        let dir = scratch.join("c");
        let first = ours(claim(&dir, &root(), None));
        let second = claim(&dir, &root(), None);
        assert!(matches!(second, Ok(Claim::Forwarded(Reply::Ok))), "{second:?}");
        assert_eq!(
            first.requests().recv_timeout(Duration::from_secs(2)),
            Ok(Request::Raise)
        );
    }

    #[test]
    fn a_claim_while_the_winner_is_still_starting_is_answered() {
        let scratch = ScratchDir::new("eitri-pc", "starting");
        let dir = scratch.join("c");
        // The winner's own side never polls `requests()`: the answer comes from the server thread.
        let _first = ours(claim(&dir, &root(), None));
        let started = Instant::now();
        let second = claim(&dir, &root(), None);
        assert!(matches!(second, Ok(Claim::Forwarded(Reply::Ok))), "{second:?}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_refused_attach_is_reported_back_and_never_delivered() {
        let scratch = ScratchDir::new("eitri-pc", "refused");
        let dir = scratch.join("c");
        let first = ours(claim(&dir, &root(), None));
        let second = claim(&dir, &root(), Some(&Request::Attach(PathBuf::from("localhost:6666"))));
        match second {
            Ok(Claim::Forwarded(Reply::Refused(why))) => assert!(why.contains("TCP"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(first.requests().recv_timeout(Duration::from_millis(300)).is_err());
    }

    #[test]
    fn a_stale_socket_and_dead_pid_are_reclaimed_once() {
        let scratch = ScratchDir::new("eitri-pc", "stale");
        let dir = scratch.join("c");
        ensure_dir(&dir).unwrap();
        let sock = socket_path(&dir, &root()).unwrap();
        drop(UnixListener::bind(&sock).unwrap());
        // A pid far above any real pid_max: `kill` answers ESRCH, and the probe does not pre-reject it.
        std::fs::write(pid_path(&dir, &root()), format!("{}\n", i32::MAX)).unwrap();
        let server = ours(claim(&dir, &root(), None));
        assert_eq!(
            std::fs::read_to_string(pid_path(&dir, &root())).unwrap(),
            format!("{}\n", std::process::id())
        );
        server.cleanup();

        // The same with no pid file at all: stale only after the recheck.
        drop(UnixListener::bind(&sock).unwrap());
        let server = ours(claim(&dir, &root(), None));
        server.cleanup();
    }

    #[test]
    fn a_live_pid_that_does_not_answer_is_unresponsive() {
        let scratch = ScratchDir::new("eitri-pc", "silent");
        let dir = scratch.join("c");
        ensure_dir(&dir).unwrap();
        let sock = socket_path(&dir, &root()).unwrap();
        let _listener = UnixListener::bind(&sock).unwrap();
        let pid_file = pid_path(&dir, &root());
        let own = std::process::id();
        std::fs::write(&pid_file, format!("{own}\n")).unwrap();
        let started = Instant::now();
        let claimed = claim(&dir, &root(), None);
        assert!(
            matches!(claimed, Ok(Claim::Unresponsive { pid }) if pid == own),
            "{claimed:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(sock.exists() && pid_file.exists());
    }

    #[test]
    fn a_connect_that_fails_for_another_reason_than_refusal_is_not_stale() {
        if euid() == 0 {
            return;
        }
        let scratch = ScratchDir::new("eitri-pc", "eacces");
        let dir = scratch.join("c");
        ensure_dir(&dir).unwrap();
        let sock = socket_path(&dir, &root()).unwrap();
        // A live listener with no pid file whose mode forbids even its owner: connect says EACCES.
        let _listener = UnixListener::bind(&sock).unwrap();
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o000)).unwrap();
        let claimed = claim(&dir, &root(), None);
        assert!(matches!(claimed, Ok(Claim::Unresponsive { pid: 0 })), "{claimed:?}");
        assert!(sock.symlink_metadata().unwrap().file_type().is_socket());
    }

    #[test]
    fn racing_claims_on_a_stale_socket_produce_exactly_one_panel() {
        let scratch = ScratchDir::new("eitri-pc", "race");
        let dir = scratch.join("c");
        ensure_dir(&dir).unwrap();
        let sock = socket_path(&dir, &root()).unwrap();
        for round in 0..5 {
            drop(UnixListener::bind(&sock).unwrap());
            std::fs::write(pid_path(&dir, &root()), format!("{}\n", i32::MAX)).unwrap();
            let barrier = Arc::new(std::sync::Barrier::new(6));
            let workers: Vec<_> = (0..6)
                .map(|_| {
                    let (dir, barrier) = (dir.clone(), Arc::clone(&barrier));
                    std::thread::spawn(move || {
                        barrier.wait();
                        claim(&dir, &root(), None)
                    })
                })
                .collect();
            let claims: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
            let owners = claims.iter().filter(|c| matches!(c, Ok(Claim::Ours(_)))).count();
            let forwarded = claims
                .iter()
                .filter(|c| matches!(c, Ok(Claim::Forwarded(Reply::Ok))))
                .count();
            assert!(owners == 1 && forwarded == 5, "round {round}: {claims:?}");
            for claim in claims {
                if let Ok(Claim::Ours(server)) = claim {
                    server.cleanup();
                }
            }
        }
    }

    #[test]
    fn a_symlinked_or_group_writable_dir_is_refused() {
        let scratch = ScratchDir::new("eitri-pc", "dirs");
        let real = scratch.join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = scratch.join("link");
        symlink(&real, &link).unwrap();
        let err = claim(&link, &root(), None).unwrap_err();
        assert!(
            err.contains("not a private directory") && err.contains("symbolic link"),
            "{err}"
        );
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());

        let open = scratch.join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o770)).unwrap();
        let err = claim(&open, &root(), None).unwrap_err();
        assert!(err.contains("not a private directory"), "{err}");
        assert_eq!(mode(&open), 0o770);
    }

    #[test]
    fn a_socket_owned_by_someone_else_is_not_touched() {
        if euid() == 0 {
            return;
        }
        let scratch = ScratchDir::new("eitri-pc", "foreign");
        let dir = scratch.join("c");
        ensure_dir(&dir).unwrap();
        // Not a socket at all, so `bind` meets `AddrInUse` and the probe must refuse to go on.
        let sock = socket_path(&dir, &root()).unwrap();
        let pid_file = pid_path(&dir, &root());
        std::fs::write(&sock, b"not a socket").unwrap();
        std::fs::write(&pid_file, b"1\n").unwrap();
        let err = claim(&dir, &root(), None).unwrap_err();
        assert!(err.contains("not this user's socket"), "{err}");
        assert!(sock.exists() && pid_file.exists());
    }

    /// One raw exchange with the control socket: `bytes` out, the reply line (or the error) back.
    fn raw(sock: &Path, bytes: &[u8], close_write: bool) -> String {
        let mut stream = UnixStream::connect(sock).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let _ = stream.write_all(bytes);
        if close_write {
            let _ = stream.shutdown(std::net::Shutdown::Write);
        }
        let mut reply = String::new();
        let _ = io::BufReader::new(&stream).read_line(&mut reply);
        reply
    }

    #[test]
    fn malformed_oversized_and_wrong_version_requests_are_refused_with_reasons() {
        let scratch = ScratchDir::new("eitri-pc", "badreq");
        let dir = scratch.join("c");
        let server = ours(claim(&dir, &root(), None));
        let sock = socket_path(&dir, &root()).unwrap();
        let cases: [(&[u8], &str); 6] = [
            (b"not json\n", "refused: malformed request\n"),
            (
                b"{\"v\":2,\"raise\":true}\n",
                "refused: unsupported protocol version 2\n",
            ),
            (b"{\"v\":1}\n", "refused: malformed request\n"),
            (b"{\"v\":1,\"raise\":false}\n", "refused: malformed request\n"),
            (
                b"{\"v\":1,\"attach\":\"/x\",\"raise\":true}\n",
                "refused: malformed request\n",
            ),
            (b"{\"v\":1,\"raise\":true,\"more\":1}\n", "refused: malformed request\n"),
        ];
        for (bytes, reply) in cases {
            assert_eq!(raw(&sock, bytes, false), reply, "{}", String::from_utf8_lossy(bytes));
        }
        assert_eq!(raw(&sock, &[b'x'; 5000], false), "refused: request too long\n");
        assert_eq!(raw(&sock, b"{\"v\":1", true), "refused: malformed request\n");
        // Whitespace and an attach to a path that is not nvim's are answered the same way as a TCP one.
        assert_eq!(
            raw(&sock, b"{\"v\":1,\"attach\":\"rel\"}\n", false),
            "refused: rel is not an absolute path\n"
        );
        assert!(server.requests().try_recv().is_err());
        assert_eq!(raw(&sock, b"{\"v\":1,\"raise\":true}\n", false), "ok\n");
        assert_eq!(
            server.requests().recv_timeout(Duration::from_secs(2)),
            Ok(Request::Raise)
        );
    }

    #[test]
    fn tcp_relative_missing_and_non_socket_addresses_are_refused() {
        let scratch = ScratchDir::new("eitri-pc", "addrs");
        let me = euid();
        let why = |addr: &Path| validate_nvim_addr(addr, me).unwrap_err();
        assert!(why(Path::new("localhost:6666")).contains("TCP addresses are refused"));
        assert!(why(Path::new("[::1]:6666")).contains("TCP addresses are refused"));
        assert!(why(Path::new("nvim.addr")).contains("is not an absolute path"));
        assert!(why(&scratch.join("absent")).contains("does not exist"));
        let plain = scratch.join("plain");
        std::fs::write(&plain, b"x").unwrap();
        assert!(why(&plain).contains("is not a Unix socket"));
        let (nvim, _listener) = fake_nvim(&scratch);
        let link = scratch.join("link");
        symlink(&nvim, &link).unwrap();
        assert!(why(&link).contains("symbolic link"));
        assert!(validate_nvim_addr(&nvim, me.wrapping_add(1))
            .unwrap_err()
            .contains("belongs to another user"));
        assert_eq!(validate_nvim_addr(&nvim, me), Ok(()));
    }

    #[test]
    fn peer_uid_is_ours_for_a_local_connection() {
        let (a, _b) = UnixStream::pair().unwrap();
        assert_eq!(peer_uid(&a).unwrap(), euid());
    }

    #[test]
    fn cleanup_leaves_files_a_newer_panel_wrote() {
        let scratch = ScratchDir::new("eitri-pc", "newer");
        let dir = scratch.join("c");
        let server = ours(claim(&dir, &root(), None));
        let pid_file = pid_path(&dir, &root());
        std::fs::write(&pid_file, b"1\n").unwrap();
        server.cleanup();
        assert!(socket_path(&dir, &root()).unwrap().exists());
        assert_eq!(std::fs::read_to_string(&pid_file).unwrap(), "1\n");
    }

    #[test]
    fn cleanup_leaves_a_socket_it_did_not_bind() {
        let scratch = ScratchDir::new("eitri-pc", "rebound");
        let dir = scratch.join("c");
        let server = ours(claim(&dir, &root(), None));
        let sock = socket_path(&dir, &root()).unwrap();
        // The server still holds its socket, so the replacement cannot reuse its inode number.
        std::fs::remove_file(&sock).unwrap();
        let _newer = UnixListener::bind(&sock).unwrap();
        let theirs = sock.symlink_metadata().unwrap().ino();
        server.cleanup();
        assert_eq!(sock.symlink_metadata().unwrap().ino(), theirs);
        assert!(!pid_path(&dir, &root()).exists());
    }

    #[test]
    fn a_server_dropped_without_cleanup_still_removes_its_files() {
        let scratch = ScratchDir::new("eitri-pc", "dropped");
        let dir = scratch.join("c");
        drop(ours(claim(&dir, &root(), None)));
        assert!(!socket_path(&dir, &root()).unwrap().exists());
        assert!(!pid_path(&dir, &root()).exists());
    }
}
