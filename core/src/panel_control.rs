//! The per-project control socket: how a second `eitri-companion` for the same project finds the first
//! one, and how a panel tells an editor that is not its own to attach.
//!
//! One companion panel runs per project. Whoever binds `p-<16 hex>.sock` first owns it and writes
//! its pid beside it; everyone after connects, sends one JSON line (`{"v":1,"attach":"<nvim socket>"}`
//! or `{"v":1,"raise":true}`), reads one reply line (`ok` or `refused: <why>`) and leaves. The panel's
//! thread answers on its own and only then hands the request to the GTK side through a channel, so
//! a panel that is busy, or has not polled yet, still answers its callers within a second.
//!
//! Version 2 adds one request, an attach that also names the editor process whose exit should close
//! the panel: `{"v":2,"attach":"<nvim socket>","close_with":{"pid":N,"start":N}}`, `start` being that
//! process's start time (field 22 of `/proc/<pid>/stat`). It is only sent when there is such a
//! process, so every other request stays v1 and an older panel still understands it. The named
//! process must be a child of the sender, checked before the reply: otherwise any process of this
//! user could tie the panel to an arbitrary application.
//!
//! This is a local trust boundary that also deletes files, so every step is deliberately narrow:
//!
//! - The directory is `$XDG_RUNTIME_DIR/eitri` or `<tmp>/eitri-<uid>`, created 0700 and then required to
//!   be a real directory of this user that nobody else can write to. A directory this code did not
//!   create is never chmodded or removed; it is refused with a reason. When `XDG_RUNTIME_DIR` is
//!   unset and `<tmp>/eitri-<uid>` belongs to someone else, that is an error, not a reason to pick another
//!   shared directory. So is a directory above it that another user could swap it out of.
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
//!   the removal is of the very socket (device and inode) that was probed. The pid file names the
//!   panel by its pid and its start time (field 22 of `/proc/<pid>/stat`), so a pid that was reused
//!   by an unrelated process does not count as the panel. Where no start time can be read (off
//!   Linux), or in the file of a panel that wrote only its pid, a reused pid keeps the claim
//!   unresponsive until that process exits, which is the price of never deleting a live panel's
//!   socket. A panel that reads only a pid finds none in the two-field form and falls back to its
//!   connect probe, which removes nothing that still has a listener.

use crate::instance_dir::ConnectOutcome;
use std::cell::Cell;
use std::ffi::OsStr;
use std::fmt;
use std::fs::DirBuilder;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The version of every request that carries no `close_with`; a panel that predates `close_with`
/// speaks only this one.
pub const PROTOCOL_V1: u64 = 1;
/// The version of an attach that carries `close_with`. A request with a `v` other than these two is
/// refused by name.
pub const PROTOCOL_V2: u64 = 2;
/// A page remains attached to the editor that opened it.
pub const PAGE_ATTACH_REFUSED: &str = "refused: a page window follows the nvim it started with";
/// How many ancestors of a sender are recorded with its attach. A process tree deeper than this is
/// not one an editor window is found in.
const SENDER_CHAIN_MAX: usize = 32;

/// The longest request line, without its newline.
pub(crate) const MAX_LINE: usize = 4096;
/// How long one connection may take to deliver its request line, in total.
pub(crate) const REQUEST_DEADLINE: Duration = Duration::from_secs(1);
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

/// A process named by its pid and its start time, so a pid that was reused names nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloseWith {
    pub pid: u32,
    /// Field 22 of `/proc/<pid>/stat`: clock ticks since boot.
    pub start: u64,
}

/// What a request asks the running panel to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Attach to the nvim listening on `addr`. With `close_with`, the panel closes when that process
    /// exits; the process was verified to be the sender's child before the request was accepted.
    Attach {
        addr: PathBuf,
        close_with: Option<CloseWith>,
    },
    /// Bring the panel to the front; there is nothing to attach.
    Raise,
}

/// A request the control thread accepted, with what it learnt about the sender while the sender
/// was still connected: by the time the GTK side looks, a forwarding process has usually exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    pub request: Request,
    /// For an attach, the process whose window belongs to the attached editor, nearest first: the
    /// verified `close_with` pid alone when there is one, else the sender and its ancestors. Empty
    /// for a raise, and when the platform does not say who the sender is.
    pub sender_chain: Vec<u32>,
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
/// non-empty and absolute, else `<tmp>/eitri-<uid>`. The runtime dir already belongs to one user;
/// `<tmp>` is shared, so the fallback carries the uid and another account creating the name first
/// cannot lock this one out.
pub fn control_dir(xdg_runtime_dir: Option<&OsStr>, tmp: &Path) -> PathBuf {
    control_dir_for(xdg_runtime_dir, tmp, euid())
}

/// [`control_dir`] for an explicit uid.
fn control_dir_for(xdg_runtime_dir: Option<&OsStr>, tmp: &Path, uid: u32) -> PathBuf {
    match xdg_runtime_dir {
        Some(runtime) if Path::new(runtime).is_absolute() => Path::new(runtime).join("eitri"),
        _ => tmp.join(format!("eitri-{uid}")),
    }
}

/// The 16 hex digits that name this project's files, the same key the layout files use.
fn key(project_root: &Path) -> String {
    crate::state_paths::project_key(project_root)
}

/// A companion is shared by project; a page is shared only by editor address and plugin id.
#[derive(Debug, Clone, Copy)]
pub enum ControlKey<'a> {
    Project(&'a Path),
    Page { addr: &'a Path, id: &'a str },
}

/// A checked socket path in the project's or page's namespace. Address aliases name the same page.
pub fn socket_path_for(dir: &Path, control_key: &ControlKey<'_>) -> io::Result<PathBuf> {
    match control_key {
        ControlKey::Project(root) => agent::socket_path::in_dir(dir, &format!("p-{}.sock", key(root))),
        ControlKey::Page { addr, id } => {
            use sha2::{Digest, Sha256};
            let canonical = std::fs::canonicalize(addr)?;
            let mut hash = Sha256::new();
            hash.update(canonical.as_os_str().as_bytes());
            hash.update(b"\0");
            hash.update(id.as_bytes());
            agent::socket_path::in_dir(dir, &format!("g-{}.sock", &format!("{:x}", hash.finalize())[..16]))
        }
    }
}

/// `dir/p-<16 hex>.sock`, checked against the 103-byte socket path limit macOS has.
pub fn socket_path(dir: &Path, project_root: &Path) -> io::Result<PathBuf> {
    socket_path_for(dir, &ControlKey::Project(project_root))
}

/// `dir/split-<16 hex>-<pid>-<8 hex nonce>.sock`, where `eitri split` asks the editor it starts to
/// listen, checked against the same limit. The nonce keeps a later split whose pid was reused from
/// meeting a socket an earlier editor still listens on.
pub fn split_socket_path(dir: &Path, project_root: &Path, pid: u32, nonce: u32) -> io::Result<PathBuf> {
    let project = key(project_root);
    agent::socket_path::in_dir(dir, &format!("split-{project}-{pid}-{nonce:08x}.sock"))
}

/// `dir/p-<16 hex>.pid`, the pid (and, where it can be read, the start time) of the panel that owns
/// the socket.
pub fn pid_path(dir: &Path, project_root: &Path) -> PathBuf {
    dir.join(format!("p-{}.pid", key(project_root)))
}

/// This process's effective uid: what a socket file's owner and a peer's credentials both carry.
/// (`agent::private_fs::current_uid` is the real uid, which differs under setuid.)
pub(crate) fn euid() -> u32 {
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    unsafe { libc::geteuid() }
}

/// Creates `dir` 0700 if it is missing, then requires it to be a private directory of this user.
/// A directory that already existed is judged, never changed. What holds it is judged first, whichever
/// of [`control_dir`]'s two places it is: a directory is only as private as its parent lets it stay, and
/// `<tmp>` can be anything `TMPDIR` names.
pub fn ensure_dir(dir: &Path) -> Result<(), String> {
    if let Some(parent) = dir.parent() {
        agent::private_fs::require_safe_parent(parent, "Eitri's control sockets").map_err(|e| e.to_string())?;
    }
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

/// Who is on the other end of a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Peer {
    uid: u32,
    /// `None` where the platform does not say, and for a peer in a pid namespace this process
    /// cannot see (the kernel reports pid 0 then).
    pid: Option<u32>,
}

/// The uid of the process on the other end of `stream`, for a caller that needs no more of it.
pub fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    peer(stream).map(|peer| peer.uid)
}

/// The pid of the process on the other end of `stream`, where the platform says (Linux does; the
/// BSDs and macOS give only a uid here) and the process is visible from this pid namespace.
pub(crate) fn peer_pid(stream: &UnixStream) -> Option<u32> {
    peer(stream).ok().and_then(|peer| peer.pid)
}

/// The credentials of the process on the other end of `stream`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer(stream: &UnixStream) -> io::Result<Peer> {
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
            let pid = u32::try_from(cred.pid).ok().filter(|&pid| pid != 0);
            Ok(Peer { uid: cred.uid, pid })
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

/// A pidfd for the process that connected (`SO_PEERPIDFD`, Linux 6.5 and later), or `None` where the
/// kernel has none. The pid `SO_PEERCRED` reports is a number taken at connect time: once that
/// process exits and the number is reused, it names a stranger. A pidfd keeps naming the process
/// that connected, and says when it is gone.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn peer_pidfd(stream: &UnixStream) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    let mut fd: libc::c_int = -1;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `fd` and `len` are valid out-pointers sized for the one int the option returns.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERPIDFD,
            &mut fd as *mut libc::c_int as *mut libc::c_void,
            &mut len,
        )
    };
    // SAFETY: on success the kernel handed this process a new descriptor it now owns.
    (rc == 0 && fd >= 0).then(|| unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) fn peer_pidfd(_stream: &UnixStream) -> Option<std::os::fd::OwnedFd> {
    None
}

/// Whether the process behind `pidfd` is still alive and still has `pid`: its `fdinfo` says
/// `Pid:\t-1` once it has exited.
pub(crate) fn pidfd_still_names(pidfd: &std::os::fd::OwnedFd, pid: u32) -> bool {
    use std::os::fd::AsRawFd;
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd())).unwrap_or_default();
    pidfd_info_pid(&info) == Some(i64::from(pid))
}

/// The `Pid:` field of a pidfd's `fdinfo`, `-1` for a process that has exited.
fn pidfd_info_pid(info: &str) -> Option<i64> {
    info.lines()
        .find_map(|line| line.strip_prefix("Pid:"))
        .and_then(|value| value.trim().parse().ok())
}

/// The credentials of the process on the other end of `stream`. `getpeereid` gives no pid, so a
/// `close_with` is never accepted here and an attach carries no sender chain.
#[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
fn peer(stream: &UnixStream) -> io::Result<Peer> {
    let (mut uid, mut gid): (libc::uid_t, libc::gid_t) = (0, 0);
    // SAFETY: both out-pointers are valid for the call.
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0 {
        Ok(Peer { uid, pid: None })
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
pub(crate) enum LineFail {
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
pub(crate) fn read_line(stream: &UnixStream, deadline: Instant, max: usize) -> Result<Vec<u8>, LineFail> {
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
        Request::Attach { addr, close_with } => {
            let path = addr
                .to_str()
                .ok_or_else(|| format!("{} is not valid UTF-8", addr.display()))?;
            match close_with {
                None => serde_json::json!({ "v": PROTOCOL_V1, "attach": path }),
                Some(CloseWith { pid, start }) => serde_json::json!({
                    "v": PROTOCOL_V2,
                    "attach": path,
                    "close_with": { "pid": pid, "start": start },
                }),
            }
        }
        Request::Raise => serde_json::json!({ "v": PROTOCOL_V1, "raise": true }),
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
    // Exactly one shape per version. Extra keys are refused too: this is a trust boundary, and a
    // new capability is a new version.
    match version {
        PROTOCOL_V1 => match (object.len(), object.get("attach"), object.get("raise")) {
            (2, Some(serde_json::Value::String(path)), None) => Ok(Request::Attach {
                addr: PathBuf::from(path),
                close_with: None,
            }),
            (2, None, Some(serde_json::Value::Bool(true))) => Ok(Request::Raise),
            _ => Err(malformed()),
        },
        PROTOCOL_V2 => match (object.len(), object.get("attach"), object.get("close_with")) {
            (3, Some(serde_json::Value::String(path)), Some(serde_json::Value::Object(close_with)))
                if close_with.len() == 2 =>
            {
                let pid = close_with
                    .get("pid")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|pid| u32::try_from(pid).ok());
                let start = close_with.get("start").and_then(serde_json::Value::as_u64);
                match (pid, start) {
                    (Some(pid), Some(start)) => Ok(Request::Attach {
                        addr: PathBuf::from(path),
                        close_with: Some(CloseWith { pid, start }),
                    }),
                    _ => Err(malformed()),
                }
            }
            _ => Err(malformed()),
        },
        _ => Err(format!("unsupported protocol version {version}")),
    }
}

/// Field 4 (the parent pid) and field 22 (the start time) of one `/proc/<pid>/stat` text. The
/// fields are counted from the last `)`, because the command name before it may itself hold spaces
/// and parentheses.
pub(crate) fn stat_parent_and_start(stat: &str) -> Option<(u32, u64)> {
    let (_, rest) = stat.rsplit_once(')')?;
    // `rest` starts at field 3, the state.
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let parent = fields.get(1)?.parse().ok()?;
    let start = fields.get(19)?.parse().ok()?;
    Some((parent, start))
}

/// Whether `close_with` names a live child of the peer, by pid and start time. Without the peer's
/// pid nothing can be proved, so the answer is no.
fn check_close_with(
    close_with: CloseWith,
    peer_pid: Option<u32>,
    read_stat: &dyn Fn(u32) -> Option<String>,
) -> Result<(), ()> {
    let peer_pid = peer_pid.ok_or(())?;
    let (parent, start) = read_stat(close_with.pid)
        .as_deref()
        .and_then(stat_parent_and_start)
        .ok_or(())?;
    if parent == peer_pid && start == close_with.start {
        Ok(())
    } else {
        Err(())
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

/// What a pid file says about the panel that wrote it: its pid and, when it was known, its start
/// time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PidRecord {
    pid: u32,
    start: Option<u64>,
}

impl PidRecord {
    /// This process, as its own pid file names it.
    fn own() -> PidRecord {
        let pid = std::process::id();
        PidRecord {
            pid,
            start: crate::split::proc_start_time(pid),
        }
    }

    fn line(self) -> String {
        match self.start {
            Some(start) => format!("{} {start}\n", self.pid),
            None => format!("{}\n", self.pid),
        }
    }

    /// `<pid>` (what a panel that knew no start time, or an older one, wrote) or `<pid> <start>`.
    fn parse(text: &str) -> Option<PidRecord> {
        let mut words = text.split_ascii_whitespace();
        let pid = words.next()?.parse::<u32>().ok().filter(|&pid| pid != 0)?;
        let start = match words.next() {
            Some(word) => Some(word.parse::<u64>().ok()?),
            None => None,
        };
        if words.next().is_some() {
            return None;
        }
        Some(PidRecord { pid, start })
    }

    /// Whether the process this record names is still running. A recorded start time that the
    /// current process of that pid does not have means the pid was reused. When the current start
    /// time cannot be read, the pid alone decides, as it does for a record without a start time:
    /// a live panel's socket is never taken for a leftover.
    fn is_alive(self, start_of: &dyn Fn(u32) -> Option<u64>) -> bool {
        if !agent::process_probe::pid_is_alive(self.pid) {
            return false;
        }
        match (self.start, start_of(self.pid)) {
            (Some(recorded), Some(now)) => recorded == now,
            _ => true,
        }
    }
}

/// The pid in `pid_file`, if it is a regular file of this user holding one.
fn read_pid(pid_file: &Path) -> Option<u32> {
    read_pid_record(pid_file).map(|record| record.pid)
}

/// The record in `pid_file`, if it is a regular file of this user holding one. Never follows a link
/// and never blocks on a special file.
fn read_pid_record(pid_file: &Path) -> Option<PidRecord> {
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
    file.take(64).read_to_string(&mut text).ok()?;
    PidRecord::parse(&text)
}

/// `(device, inode)` of `path` itself, not of what a link there points at.
pub(crate) fn identity(path: &Path) -> Option<(u64, u64)> {
    path.symlink_metadata().ok().map(|meta| (meta.dev(), meta.ino()))
}

/// Removes `sock` if it is a socket of this user.
pub(crate) fn remove_own_socket(sock: &Path) {
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
pub(crate) struct ReclaimLock(#[allow(dead_code)] std::fs::File);

/// Takes the reclaim lock, waiting up to `wait` for another claimant to finish. The file is created
/// 0600 if missing and must be a regular file of this user; it is never removed.
pub(crate) fn take_reclaim_lock(lock_file: &Path, wait: Duration) -> Result<ReclaimLock, String> {
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
    let give_up = Instant::now() + wait;
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
        let record = read_pid_record(pid_file);
        let pid = record.map(|record| record.pid);
        let connect = crate::instance_dir::nonblocking_connect_outcome(sock);
        if let Some(record) = record.filter(|record| record.is_alive(&crate::split::proc_start_time)) {
            // Alive but silent, whether or not a connect would be refused right now.
            return Ok(Probe::Unresponsive(record.pid));
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
/// `request` is what a caller that does not hold the claim wants the holder to do; `None` sends
/// [`Request::Raise`]. When this returns [`Claim::Ours`] nothing was sent: the caller acts on its
/// own request.
pub fn claim(dir: &Path, project_root: &Path, request: Option<&Request>) -> Result<Claim, String> {
    claim_for(dir, &ControlKey::Project(project_root), request)
}

/// What [`forward`] found at a control socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Forwarded {
    /// A running holder answered.
    Answered(Reply),
    /// No socket, or one nothing listens on: there is no holder to ask.
    NoneListening,
}

/// Sends `request` to whoever holds `control_key`'s socket, never binding, reclaiming or removing anything: for a
/// caller that wants a running window to act and starts one itself when none runs. A socket that is not this user's is
/// refused, and one that has a listener which does not answer is an `Err`.
pub fn forward(dir: &Path, control_key: &ControlKey<'_>, request: &Request) -> Result<Forwarded, String> {
    let sock = socket_path_for(dir, control_key).map_err(|e| e.to_string())?;
    match sock.symlink_metadata() {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Forwarded::NoneListening),
        Ok(meta) if meta.file_type().is_socket() && meta.uid() == euid() => {}
        _ => return Err(format!("{} is not this user's socket; not touching it", sock.display())),
    }
    match send(&sock, request, FORWARD_DEADLINE) {
        Ok(reply) => Ok(Forwarded::Answered(reply)),
        Err(why) => match crate::instance_dir::nonblocking_connect_outcome(&sock) {
            ConnectOutcome::NoListener => Ok(Forwarded::NoneListening),
            ConnectOutcome::Listener | ConnectOutcome::Unknown => Err(why),
        },
    }
}

/// Claims or forwards to a companion or page, with identical ownership and stale-socket checks.
pub fn claim_for(dir: &Path, control_key: &ControlKey<'_>, request: Option<&Request>) -> Result<Claim, String> {
    ensure_dir(dir)?;
    let sock = socket_path_for(dir, control_key).map_err(|e| e.to_string())?;
    let pid_file = sock.with_extension("pid");
    let lock_file = sock.with_extension("lock");
    let page = matches!(control_key, ControlKey::Page { .. });
    // Taken the first time the socket is found occupied and held until this claim returns, so a
    // reclaim, its bind and its pid write are never interleaved with another claimant's.
    let mut lock = None;
    let mut reclaimed = false;
    for _ in 0..MAX_CLAIM_ROUNDS {
        match UnixListener::bind(&sock) {
            Ok(listener) => return serve(listener, sock, pid_file, page).map(Claim::Ours),
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => {}
            Err(e) => return Err(format!("could not bind {}: {e}", sock.display())),
        }
        if lock.is_none() {
            // Whoever held it may have reclaimed the socket meanwhile: bind again before probing.
            lock = Some(take_reclaim_lock(&lock_file, RECLAIM_LOCK_WAIT)?);
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
                // The pid file goes first. Only a claimant that has bound the socket writes one,
                // and nobody can bind until the socket is gone; the other order would delete the
                // pid file of a claimant that won the path between the two removals.
                remove_own_pid_file(&pid_file);
                #[cfg(test)]
                between_removals::run();
                remove_own_socket(&sock);
                reclaimed = true;
            }
        }
    }
    Err(format!(
        "the control socket {} kept changing; giving up",
        sock.display()
    ))
}

/// A test's look at the moment between a reclaim's two removals, when another claimant could act.
#[cfg(test)]
mod between_removals {
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

/// The bound socket's side of a won claim: finishes setting it up and starts the thread.
fn serve(listener: UnixListener, sock: PathBuf, pid_file: PathBuf, page: bool) -> Result<ControlServer, String> {
    let bound = identity(&sock).ok_or_else(|| format!("could not read {}", sock.display()))?;
    let started = (|| -> Result<ControlServer, String> {
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("could not make {} non-blocking: {e}", sock.display()))?;
        // `bind` leaves the umask's mode on the file.
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("could not set the mode of {}: {e}", sock.display()))?;
        let tmp = pid_file.with_extension("pid.tmp");
        agent::private_fs::write_private(&tmp, PidRecord::own().line().as_bytes())
            .and_then(|()| std::fs::rename(&tmp, &pid_file))
            .map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("could not write {}: {e}", pid_file.display())
            })?;
        let stop = Arc::new(AtomicBool::new(false));
        let (requests_tx, requests) = mpsc::sync_channel(64);
        let raise_queued = Arc::new(AtomicBool::new(false));
        let requests = ControlRequests {
            receiver: requests,
            raise_queued: raise_queued.clone(),
        };
        let (done_tx, done) = mpsc::channel();
        let accepted = Arc::new(AtomicU64::new(0));
        let thread_stop = Arc::clone(&stop);
        let thread_accepted = Arc::clone(&accepted);
        std::thread::Builder::new()
            .name("eitri-panel-control".into())
            .spawn(move || {
                run(
                    listener,
                    &thread_stop,
                    &requests_tx,
                    &thread_accepted,
                    &raise_queued,
                    page,
                );
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
            accepted,
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
fn run(
    listener: UnixListener,
    stop: &AtomicBool,
    requests: &SyncSender<Received>,
    accepted: &AtomicU64,
    raise_queued: &AtomicBool,
    page: bool,
) {
    let me = euid();
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => serve_one(stream, requests, accepted, raise_queued, me, page),
            // Nothing waiting, or a transient failure (descriptors exhausted, a connection that
            // went away): either way, never spin.
            Err(_) => std::thread::sleep(IDLE_SLEEP),
        }
    }
}

pub(crate) fn reply_line(stream: &UnixStream, line: &str) -> io::Result<()> {
    let mut writer = stream;
    writer.write_all(format!("{line}\n").as_bytes())
}

fn serve_one(
    stream: UnixStream,
    requests: &SyncSender<Received>,
    accepted: &AtomicU64,
    raise_queued: &AtomicBool,
    me: u32,
    page: bool,
) {
    // A socket accepted from a non-blocking listener inherits that flag on macOS and the BSDs, and
    // every read below would then fail at once.
    if stream.set_nonblocking(false).is_err() || stream.set_write_timeout(Some(Duration::from_secs(1))).is_err() {
        return;
    }
    let peer = match peer(&stream) {
        Ok(peer) if peer.uid == me => peer,
        _ => {
            let _ = reply_line(&stream, "refused: not your process");
            return;
        }
    };
    // Taken before the request is read, so it names the process that connected even if that one
    // exits while its socket lives on in another process.
    let peer_pidfd = peer_pidfd(&stream);
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
        Ok(request) => request,
        Err(why) => {
            let _ = reply_line(&stream, &format!("refused: {why}"));
            return;
        }
    };
    if page && matches!(&request, Request::Attach { .. }) {
        let _ = reply_line(&stream, PAGE_ATTACH_REFUSED);
        return;
    }
    let sender_chain = match &request {
        Request::Raise => Vec::new(),
        Request::Attach { addr, close_with } => {
            if let Err(why) = validate_nvim_addr(addr, me) {
                let _ = reply_line(&stream, &format!("refused: {why}"));
                return;
            }
            // Read before the reply: the sender exits as soon as it has one, and once it is reaped
            // its pid and its place in the process tree say nothing any more.
            let chain = match close_with {
                Some(close_with) => {
                    if check_close_with(*close_with, peer.pid, &crate::proc_chain::proc_stat).is_err() {
                        let _ = reply_line(&stream, "refused: close_with is not the sender's child");
                        return;
                    }
                    vec![close_with.pid]
                }
                None => peer
                    .pid
                    .map(|pid| crate::proc_chain::ppid_chain(pid, &crate::proc_chain::proc_stat, SENDER_CHAIN_MAX))
                    .unwrap_or_default(),
            };
            // The process tree was read by pid. If the process that connected is still alive now,
            // that pid was its own the whole time; if it is gone, the pid may already belong to
            // someone else, and what was read proves nothing about the sender.
            let sender_gone = match (&peer_pidfd, peer.pid) {
                (Some(pidfd), Some(pid)) => !pidfd_still_names(pidfd, pid),
                _ => false,
            };
            if sender_gone {
                if close_with.is_some() {
                    let _ = reply_line(&stream, "refused: close_with is not the sender's child");
                    return;
                }
                Vec::new()
            } else {
                chain
            }
        }
    };
    let reply = admit(requests, accepted, raise_queued, Received { request, sender_chain });
    let _ = reply_line(&stream, reply);
}

fn admit(
    requests: &SyncSender<Received>,
    accepted: &AtomicU64,
    raise_queued: &AtomicBool,
    received: Received,
) -> &'static str {
    let raise = matches!(received.request, Request::Raise);
    if raise && raise_queued.swap(true, Ordering::SeqCst) {
        return "ok";
    }
    // Count before publishing: the consumer must see every admitted request in the count.
    // Once published it may be acted on, even if the caller disconnects before reading its reply.
    accepted.fetch_add(1, Ordering::SeqCst);
    if requests.try_send(received).is_ok() {
        "ok"
    } else {
        accepted.fetch_sub(1, Ordering::SeqCst);
        if raise {
            raise_queued.store(false, Ordering::SeqCst);
        }
        "refused: busy"
    }
}

/// Admitted work. Taking a raise lets the producer queue the next presentation.
pub struct ControlRequests {
    receiver: Receiver<Received>,
    raise_queued: Arc<AtomicBool>,
}

impl Drop for ControlRequests {
    fn drop(&mut self) {
        self.raise_queued.store(false, Ordering::SeqCst);
    }
}

impl ControlRequests {
    fn taken(&self, received: Received) -> Received {
        if matches!(received.request, Request::Raise) {
            self.raise_queued.store(false, Ordering::SeqCst);
        }
        received
    }

    pub fn try_recv(&self) -> Result<Received, mpsc::TryRecvError> {
        self.receiver.try_recv().map(|received| self.taken(received))
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Received, mpsc::RecvTimeoutError> {
        self.receiver.recv_timeout(timeout).map(|received| self.taken(received))
    }

    pub fn try_iter(&self) -> impl Iterator<Item = Received> + '_ {
        std::iter::from_fn(|| self.try_recv().ok())
    }
}

/// The owner side of a won claim: a thread answering the control socket, and the requests it
/// accepted. Dropping it cleans up.
pub struct ControlServer {
    stop: Arc<AtomicBool>,
    done: Receiver<()>,
    requests: ControlRequests,
    accepted: Arc<AtomicU64>,
    socket: PathBuf,
    pid_file: PathBuf,
    bound: (u64, u64),
    cleaned: Cell<bool>,
}

impl ControlServer {
    /// The requests admitted for an `ok` reply, in order. Poll it from the GTK thread; the server
    /// thread never waits on it.
    pub fn requests(&self) -> &ControlRequests {
        &self.requests
    }

    /// How many requests were admitted, including one being published before its reply is written.
    /// A full queue rolls the count back; a caller disconnecting after admission cannot withdraw work
    /// the consumer may already have taken. A consumer behind this count must defer its exit decision.
    pub fn accepted(&self) -> u64 {
        self.accepted.load(Ordering::SeqCst)
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
    #[test]
    fn a_raise_burst_leaves_room_for_attach_and_counts_only_queued_work() {
        let (tx, receiver) = mpsc::sync_channel(64);
        let raise_queued = Arc::new(AtomicBool::new(false));
        let rx = ControlRequests {
            receiver,
            raise_queued: raise_queued.clone(),
        };
        let accepted = AtomicU64::new(0);
        let raise = || Received {
            request: Request::Raise,
            sender_chain: vec![],
        };
        for _ in 0..1000 {
            assert_eq!(admit(&tx, &accepted, &raise_queued, raise()), "ok");
        }
        let attach = Received {
            request: Request::Attach {
                addr: "/nvim".into(),
                close_with: None,
            },
            sender_chain: vec![],
        };
        assert_eq!(admit(&tx, &accepted, &raise_queued, attach), "ok");
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
        assert_eq!(rx.try_recv().unwrap().request, Request::Raise);
        assert_eq!(admit(&tx, &accepted, &raise_queued, raise()), "ok");
        assert!(matches!(rx.try_recv().unwrap().request, Request::Attach { .. }));
        assert_eq!(rx.try_recv().unwrap().request, Request::Raise);
        assert!(rx.try_recv().is_err());
        assert_eq!(accepted.load(Ordering::SeqCst), 3);
        assert_eq!(admit(&tx, &accepted, &raise_queued, raise()), "ok");
        drop(rx);
        assert_eq!(admit(&tx, &accepted, &raise_queued, raise()), "refused: busy");
        assert!(!raise_queued.load(Ordering::SeqCst));
        assert_eq!(accepted.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn the_65th_queued_request_is_refused_without_counting_or_acknowledging_it() {
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let accepted = std::sync::atomic::AtomicU64::new(0);
        let raise_queued = AtomicBool::new(false);
        for _ in 0..64 {
            assert_eq!(
                super::admit(
                    &tx,
                    &accepted,
                    &raise_queued,
                    super::Received {
                        request: super::Request::Attach {
                            addr: "/nvim".into(),
                            close_with: None
                        },
                        sender_chain: vec![],
                    }
                ),
                "ok"
            );
        }
        assert_eq!(
            super::admit(
                &tx,
                &accepted,
                &raise_queued,
                super::Received {
                    request: super::Request::Attach {
                        addr: "/nvim".into(),
                        close_with: None
                    },
                    sender_chain: vec![],
                }
            ),
            "refused: busy"
        );
        assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 64);
        assert_eq!(rx.try_iter().count(), 64);
        drop(rx);
        assert_eq!(
            super::admit(
                &tx,
                &accepted,
                &raise_queued,
                super::Received {
                    request: super::Request::Attach {
                        addr: "/nvim".into(),
                        close_with: None
                    },
                    sender_chain: vec![],
                }
            ),
            "refused: busy"
        );
        assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 64);
    }

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
    fn control_key_hashes_canonical_bytes() {
        use sha2::{Digest, Sha256};
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let scratch = ScratchDir::new("eitri-pc", "keys");
        let real = scratch.join("编辑 器");
        std::fs::create_dir(&real).unwrap();
        let link = scratch.join("alias");
        symlink(&real, &link).unwrap();
        let addr = real.join(std::ffi::OsString::from_vec(b"nvim-\xff".to_vec()));
        std::fs::write(&addr, b"").unwrap();
        let canonical = std::fs::canonicalize(&addr).unwrap();
        let mut hash = Sha256::new();
        hash.update(canonical.as_os_str().as_bytes());
        hash.update(b"\0notes");
        let hex = format!("{:x}", hash.finalize());
        let dir = Path::new("/runtime/eitri");
        let page = ControlKey::Page {
            addr: &addr,
            id: "notes",
        };
        let sock = socket_path_for(&dir, &page).unwrap();
        assert_eq!(sock.file_stem().unwrap(), OsStr::new(&format!("g-{}", &hex[..16])));
        assert_eq!(sock.extension(), Some(OsStr::new("sock")));
        let alias_addr = link.join(addr.file_name().unwrap());
        assert_eq!(
            socket_path_for(
                &dir,
                &ControlKey::Page {
                    addr: &alias_addr,
                    id: "notes"
                }
            )
            .unwrap(),
            sock
        );
        assert_ne!(
            socket_path_for(
                &dir,
                &ControlKey::Page {
                    addr: &addr,
                    id: "other"
                }
            )
            .unwrap(),
            sock
        );
        let other_addr = real.join("another");
        std::fs::write(&other_addr, b"").unwrap();
        assert_ne!(
            socket_path_for(
                &dir,
                &ControlKey::Page {
                    addr: &other_addr,
                    id: "notes"
                }
            )
            .unwrap(),
            sock
        );
        assert_eq!(
            socket_path_for(&dir, &ControlKey::Project(&root())).unwrap(),
            socket_path(&dir, &root()).unwrap()
        );
        let long_dir = PathBuf::from(format!("/{}", "x".repeat(103)));
        assert!(socket_path_for(&long_dir, &page).is_err());
    }

    #[test]
    fn a_page_claim_refuses_attach_and_answers_raise() {
        let scratch = ScratchDir::new("eitri-pc", "page");
        let dir = scratch.join("c");
        let (addr, _listener) = fake_nvim(&scratch);
        let key = ControlKey::Page {
            addr: &addr,
            id: "notes",
        };
        let server = ours(claim_for(&dir, &key, None));
        let sock = socket_path_for(&dir, &key).unwrap();
        assert_eq!(mode(&sock), 0o600);
        assert_eq!(mode(&sock.with_extension("pid")), 0o600);
        let inode = identity(&sock);
        for request in [
            attach(&addr),
            attach(Path::new("relative")),
            Request::Attach {
                addr: addr.clone(),
                close_with: Some(CloseWith { pid: 1, start: 0 }),
            },
        ] {
            assert_eq!(
                raw(&sock, encode(&request).unwrap().as_bytes(), false),
                format!("{PAGE_ATTACH_REFUSED}\n")
            );
            assert!(server.requests().try_recv().is_err());
            assert_eq!(server.accepted(), 0);
        }
        assert!(matches!(claim_for(&dir, &key, Some(&attach(&addr))),
            Ok(Claim::Forwarded(Reply::Refused(why))) if format!("refused: {why}") == PAGE_ATTACH_REFUSED));
        assert_eq!(identity(&sock), inode);
        assert!(matches!(claim_for(&dir, &key, None), Ok(Claim::Forwarded(Reply::Ok))));
        assert_eq!(
            server.requests().recv_timeout(Duration::from_secs(2)).unwrap().request,
            Request::Raise
        );
        assert_eq!(server.accepted(), 1);
        assert_eq!(
            raw(
                &sock,
                br#"{"v":2,"raise":true}
"#,
                false
            ),
            "refused: malformed request\n"
        );
        let other = ours(claim_for(
            &dir,
            &ControlKey::Page {
                addr: &addr,
                id: "other",
            },
            None,
        ));
        other.cleanup();
        server.cleanup();
        assert!(!sock.exists() && !sock.with_extension("pid").exists());
        assert!(sock.with_extension("lock").exists());
        // A dead holder's files use the same reclaim checks as a project's.
        drop(UnixListener::bind(&sock).unwrap());
        std::fs::write(sock.with_extension("pid"), format!("{}\n", i32::MAX)).unwrap();
        let reclaimed = ours(claim_for(&dir, &key, None));
        reclaimed.cleanup();
    }

    #[test]
    fn claim_for_project_is_todays_claim() {
        let scratch = ScratchDir::new("eitri-pc", "project-key");
        let dir = scratch.join("c");
        let server = ours(claim_for(&dir, &ControlKey::Project(&root()), None));
        assert!(matches!(claim(&dir, &root(), None), Ok(Claim::Forwarded(Reply::Ok))));
        assert_eq!(
            server.requests().recv_timeout(Duration::from_secs(2)).unwrap().request,
            Request::Raise
        );
        assert_eq!(server.socket, socket_path(&dir, &root()).unwrap());
        assert_eq!(server.pid_file, pid_path(&dir, &root()));
        server.cleanup();
        let server = ours(claim(&dir, &root(), None));
        assert!(matches!(
            claim_for(&dir, &ControlKey::Project(&root()), None),
            Ok(Claim::Forwarded(Reply::Ok))
        ));
        server.cleanup();
    }

    #[test]
    fn the_control_dir_prefers_xdg_runtime_dir_and_falls_back_to_tmp() {
        let tmp = Path::new("/tmp");
        assert_eq!(
            control_dir(Some(OsStr::new("/run/user/1000")), tmp),
            Path::new("/run/user/1000/eitri")
        );
        let fallback = tmp.join(format!("eitri-{}", euid()));
        for unusable in [Some(OsStr::new("")), Some(OsStr::new("rel/dir")), None] {
            assert_eq!(control_dir(unusable, tmp), fallback);
        }
    }

    /// Two users on one machine get two different fallback directories, so one of them creating
    /// the name first cannot lock the other out of companion mode.
    #[test]
    fn the_fallback_directory_name_holds_the_uid() {
        let tmp = Path::new("/tmp");
        assert_eq!(control_dir_for(None, tmp, 1000), Path::new("/tmp/eitri-1000"));
        assert_eq!(control_dir_for(None, tmp, 501), Path::new("/tmp/eitri-501"));
        assert_ne!(control_dir_for(None, tmp, 1000), control_dir_for(None, tmp, 1001));
        assert_eq!(
            control_dir_for(Some(OsStr::new("/run/user/1000")), tmp, 1000),
            Path::new("/run/user/1000/eitri"),
            "a runtime dir is already per user"
        );
    }

    #[test]
    fn the_socket_path_is_82_bytes_under_macos_tmpdir_for_the_first_macos_user() {
        // 501 is the first account macOS creates; every extra digit of the uid costs one byte.
        let dir = control_dir_for(None, Path::new(MACOS_TMP), 501);
        assert_eq!(socket_path(&dir, &root()).unwrap().as_os_str().len(), 82);
        assert!(pid_path(&dir, &root()).as_os_str().len() < 82);
        // A uid of the widest width (`uid_t` is 32 bits: ten digits) still fits under the cap.
        let widest = control_dir_for(None, Path::new(MACOS_TMP), u32::MAX);
        assert!(socket_path(&widest, &root()).is_ok());
    }

    #[test]
    fn a_first_claim_owns_the_socket_and_writes_its_pid_0600() {
        let scratch = ScratchDir::new("eitri-pc", "first");
        let dir = scratch.join("c");
        let server = ours(claim_for(&dir, &ControlKey::Project(&root()), None));
        assert_eq!(mode(&dir), 0o700);
        let sock = socket_path(&dir, &root()).unwrap();
        let pid_file = pid_path(&dir, &root());
        assert!(sock.symlink_metadata().unwrap().file_type().is_socket());
        assert_eq!(mode(&sock), 0o600);
        assert_eq!(std::fs::read_to_string(&pid_file).unwrap(), PidRecord::own().line());
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
        let first = ours(claim_for(&dir, &ControlKey::Project(&root()), None));
        let (nvim, _listener) = fake_nvim(&scratch);
        let second = claim_for(&dir, &ControlKey::Project(&root()), Some(&attach(&nvim)));
        assert!(matches!(second, Ok(Claim::Forwarded(Reply::Ok))), "{second:?}");
        assert_eq!(
            first
                .requests()
                .recv_timeout(Duration::from_secs(2))
                .map(|received| received.request),
            Ok(attach(&nvim))
        );
    }

    #[test]
    fn a_request_is_counted_before_its_sender_can_read_ok() {
        let scratch = ScratchDir::new("eitri-pc", "counted");
        let dir = scratch.join("c");
        let first = ours(claim_for(&dir, &ControlKey::Project(&root()), None));
        assert_eq!(first.accepted(), 0);
        let (nvim, _listener) = fake_nvim(&scratch);
        // `claim` returns once the reply was read: nothing of the request has been taken yet, and
        // the count must already say it is coming.
        let second = claim_for(&dir, &ControlKey::Project(&root()), Some(&attach(&nvim)));
        assert!(matches!(second, Ok(Claim::Forwarded(Reply::Ok))), "{second:?}");
        assert_eq!(first.accepted(), 1);
        assert!(first.requests().recv_timeout(Duration::from_secs(2)).is_ok());
        assert_eq!(first.accepted(), 1, "taking it does not change the count");
        // A refused request is never counted.
        let refused = claim_for(
            &dir,
            &ControlKey::Project(&root()),
            Some(&attach(Path::new("localhost:6666"))),
        );
        assert!(
            matches!(refused, Ok(Claim::Forwarded(Reply::Refused(_)))),
            "{refused:?}"
        );
        assert_eq!(first.accepted(), 1);
    }

    #[test]
    fn a_second_claim_without_an_address_sends_raise() {
        let scratch = ScratchDir::new("eitri-pc", "raise");
        let dir = scratch.join("c");
        let first = ours(claim_for(&dir, &ControlKey::Project(&root()), None));
        let second = claim_for(&dir, &ControlKey::Project(&root()), None);
        assert!(matches!(second, Ok(Claim::Forwarded(Reply::Ok))), "{second:?}");
        assert_eq!(
            first
                .requests()
                .recv_timeout(Duration::from_secs(2))
                .map(|received| received.request),
            Ok(Request::Raise)
        );
    }

    #[test]
    fn a_claim_while_the_winner_is_still_starting_is_answered() {
        let scratch = ScratchDir::new("eitri-pc", "starting");
        let dir = scratch.join("c");
        // The winner's own side never polls `requests()`: the answer comes from the server thread.
        let _first = ours(claim_for(&dir, &ControlKey::Project(&root()), None));
        let started = Instant::now();
        let second = claim_for(&dir, &ControlKey::Project(&root()), None);
        assert!(matches!(second, Ok(Claim::Forwarded(Reply::Ok))), "{second:?}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_refused_attach_is_reported_back_and_never_delivered() {
        let scratch = ScratchDir::new("eitri-pc", "refused");
        let dir = scratch.join("c");
        let first = ours(claim_for(&dir, &ControlKey::Project(&root()), None));
        let second = claim_for(
            &dir,
            &ControlKey::Project(&root()),
            Some(&attach(Path::new("localhost:6666"))),
        );
        match second {
            Ok(Claim::Forwarded(Reply::Refused(why))) => assert!(why.contains("TCP"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(first.requests().recv_timeout(Duration::from_millis(300)).is_err());
    }

    /// `<tmp>/eitri-<uid>` is only as safe as what holds it: in a directory another user can write, this one can be
    /// renamed away and replaced after every check on it.
    #[test]
    fn nothing_is_made_or_used_in_a_directory_others_can_write() {
        let scratch = ScratchDir::new("eitri-pc", "shared");
        let parent = scratch.join("shared");
        std::fs::create_dir(&parent).unwrap();
        let mode = |bits| std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(bits)).unwrap();
        let dir = parent.join("c");

        mode(0o777);
        let refused = ensure_dir(&dir).unwrap_err();
        for named in ["XDG_RUNTIME_DIR", "TMPDIR", &parent.display().to_string()] {
            assert!(refused.contains(named), "{refused}");
        }
        assert!(!dir.exists(), "refused before anything is made");

        mode(0o1777);
        ensure_dir(&dir).unwrap();
        mode(0o777);
        assert!(ensure_dir(&dir).is_err(), "one that exists there is no better");
        assert!(claim_for(&dir, &ControlKey::Project(&root()), None).is_err());

        mode(0o700);
        ensure_dir(&dir).unwrap();
    }

    #[test]
    fn a_missing_parent_is_named() {
        let scratch = ScratchDir::new("eitri-pc", "missing");
        let parent = scratch.join("nothing");
        let refused = ensure_dir(&parent.join("c")).unwrap_err();
        assert!(refused.contains(&parent.display().to_string()), "{refused}");
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
        let server = ours(claim_for(&dir, &ControlKey::Project(&root()), None));
        assert_eq!(read_pid(&pid_path(&dir, &root())), Some(std::process::id()));
        server.cleanup();

        // The same with no pid file at all: stale only after the recheck.
        drop(UnixListener::bind(&sock).unwrap());
        let server = ours(claim_for(&dir, &ControlKey::Project(&root()), None));
        server.cleanup();
    }

    /// A claimant that binds the path while a reclaim is between its two removals, and writes its pid
    /// as the winner does, must keep that pid file: removing it would leave a live panel that every
    /// later claim finds silent and pidless.
    #[test]
    fn a_claimant_that_binds_during_a_reclaim_keeps_its_pid_file() {
        let scratch = ScratchDir::new("eitri-pc", "between");
        let dir = scratch.join("c");
        ensure_dir(&dir).unwrap();
        let sock = socket_path(&dir, &root()).unwrap();
        let pid_file = pid_path(&dir, &root());
        drop(UnixListener::bind(&sock).unwrap());
        std::fs::write(&pid_file, format!("{}\n", i32::MAX)).unwrap();
        let intruder: std::rc::Rc<std::cell::RefCell<Option<UnixListener>>> = Default::default();
        let held = std::rc::Rc::clone(&intruder);
        let (hook_sock, hook_pid_file) = (sock.clone(), pid_file.clone());
        between_removals::set(Box::new(move || {
            if let Ok(listener) = UnixListener::bind(&hook_sock) {
                std::fs::write(&hook_pid_file, "4242\n").unwrap();
                *held.borrow_mut() = Some(listener);
            }
        }));
        let claimed = claim_for(&dir, &ControlKey::Project(&root()), None);
        let recorded = read_pid(&pid_file);
        if intruder.borrow().is_some() {
            assert_eq!(
                recorded,
                Some(4242),
                "the intruder won the path but lost its pid file: {claimed:?}"
            );
        } else {
            let server = ours(claimed);
            assert_eq!(recorded, Some(std::process::id()));
            server.cleanup();
        }
    }

    /// The winner records its start time beside its pid, so a later claim can tell it from an
    /// unrelated process that was given the same pid after it died.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_pid_file_names_the_winner_by_pid_and_start_time() {
        let scratch = ScratchDir::new("eitri-pc", "pidstart");
        let dir = scratch.join("c");
        let server = ours(claim_for(&dir, &ControlKey::Project(&root()), None));
        let own = std::process::id();
        let start = crate::split::proc_start_time(own).unwrap();
        assert_eq!(
            std::fs::read_to_string(pid_path(&dir, &root())).unwrap(),
            format!("{own} {start}\n")
        );
        server.cleanup();
        assert!(
            !pid_path(&dir, &root()).exists(),
            "cleanup must still know its own pid file"
        );
    }

    #[test]
    fn a_pid_record_reads_both_forms_and_nothing_else() {
        assert_eq!(PidRecord::parse("42\n"), Some(PidRecord { pid: 42, start: None }));
        assert_eq!(
            PidRecord::parse("42 777\n"),
            Some(PidRecord {
                pid: 42,
                start: Some(777)
            })
        );
        for junk in ["", "0", "0 5", "x", "42 x", "42 7 8", "-1", "42 -7"] {
            assert_eq!(PidRecord::parse(junk), None, "{junk:?}");
        }
        let record = PidRecord {
            pid: 42,
            start: Some(777),
        };
        assert_eq!(PidRecord::parse(&record.line()), Some(record));
        let bare = PidRecord { pid: 42, start: None };
        assert_eq!(bare.line(), "42\n");
    }

    /// Where the current start time cannot be read, the pid alone decides, as it always did.
    #[test]
    fn a_pid_whose_start_time_cannot_be_read_counts_as_alive_when_it_runs() {
        let own = std::process::id();
        let unknown = |_: u32| None;
        assert!(PidRecord {
            pid: own,
            start: Some(1)
        }
        .is_alive(&unknown));
        assert!(PidRecord { pid: own, start: None }.is_alive(&|_| Some(5)));
        assert!(!PidRecord {
            pid: own,
            start: Some(1)
        }
        .is_alive(&|_| Some(2)));
        assert!(!PidRecord {
            pid: i32::MAX as u32,
            start: None
        }
        .is_alive(&unknown));
    }

    /// A recorded pid that is alive but started at another time than recorded is a reused pid: the
    /// socket it was recorded for is a leftover. The right start time keeps it alive.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_recorded_pid_with_another_start_time_is_not_alive() {
        let scratch = ScratchDir::new("eitri-pc", "reused");
        let dir = scratch.join("c");
        ensure_dir(&dir).unwrap();
        let sock = socket_path(&dir, &root()).unwrap();
        let pid_file = pid_path(&dir, &root());
        let own = std::process::id();
        let start = crate::split::proc_start_time(own).unwrap();

        drop(UnixListener::bind(&sock).unwrap());
        std::fs::write(&pid_file, format!("{own} {start}\n")).unwrap();
        let claimed = claim_for(&dir, &ControlKey::Project(&root()), None);
        assert!(
            matches!(claimed, Ok(Claim::Unresponsive { pid }) if pid == own),
            "{claimed:?}"
        );
        assert!(sock.exists() && pid_file.exists());

        std::fs::write(&pid_file, format!("{own} {}\n", start + 1)).unwrap();
        let server = ours(claim_for(&dir, &ControlKey::Project(&root()), None));
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
        let claimed = claim_for(&dir, &ControlKey::Project(&root()), None);
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
        let claimed = claim_for(&dir, &ControlKey::Project(&root()), None);
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
                        claim_for(&dir, &ControlKey::Project(&root()), None)
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
        let err = claim_for(&link, &ControlKey::Project(&root()), None).unwrap_err();
        assert!(
            err.contains("not a private directory") && err.contains("symbolic link"),
            "{err}"
        );
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());

        let open = scratch.join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o770)).unwrap();
        let err = claim_for(&open, &ControlKey::Project(&root()), None).unwrap_err();
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
        let err = claim_for(&dir, &ControlKey::Project(&root()), None).unwrap_err();
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
        let server = ours(claim_for(&dir, &ControlKey::Project(&root()), None));
        let sock = socket_path(&dir, &root()).unwrap();
        let cases: [(&[u8], &str); 6] = [
            (b"not json\n", "refused: malformed request\n"),
            (
                b"{\"v\":3,\"raise\":true}\n",
                "refused: unsupported protocol version 3\n",
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
            server
                .requests()
                .recv_timeout(Duration::from_secs(2))
                .map(|received| received.request),
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
    fn a_pidfds_fdinfo_names_its_pid_until_the_process_exits() {
        assert_eq!(
            pidfd_info_pid("pos:\t0\nflags:\t02000002\nPid:\t4242\nNSpid:\t4242\n"),
            Some(4242)
        );
        assert_eq!(pidfd_info_pid("Pid:\t-1\nNSpid:\t-1\n"), Some(-1));
        assert_eq!(pidfd_info_pid("flags:\t0\n"), None);
    }

    /// Where the kernel has `SO_PEERPIDFD`, the pidfd of a live peer names that peer's pid.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_peer_pidfd_names_a_live_peer() {
        let (a, _b) = UnixStream::pair().unwrap();
        match peer_pidfd(&a) {
            Some(pidfd) => assert!(pidfd_still_names(&pidfd, std::process::id())),
            None => eprintln!("this kernel has no SO_PEERPIDFD; the pid-reuse guard is off here"),
        }
    }

    #[test]
    fn peer_credentials_are_ours_for_a_local_connection() {
        let (a, _b) = UnixStream::pair().unwrap();
        let peer = peer(&a).unwrap();
        assert_eq!(peer.uid, euid());
        #[cfg(target_os = "linux")]
        assert_eq!(peer.pid, Some(std::process::id()));
    }

    #[test]
    fn cleanup_leaves_files_a_newer_panel_wrote() {
        let scratch = ScratchDir::new("eitri-pc", "newer");
        let dir = scratch.join("c");
        let server = ours(claim_for(&dir, &ControlKey::Project(&root()), None));
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
        let server = ours(claim_for(&dir, &ControlKey::Project(&root()), None));
        let sock = socket_path(&dir, &root()).unwrap();
        // The server still holds its socket, so the replacement cannot reuse its inode number.
        std::fs::remove_file(&sock).unwrap();
        let _newer = UnixListener::bind(&sock).unwrap();
        let theirs = sock.symlink_metadata().unwrap().ino();
        server.cleanup();
        assert_eq!(sock.symlink_metadata().unwrap().ino(), theirs);
        assert!(!pid_path(&dir, &root()).exists());
    }

    fn attach(addr: &Path) -> Request {
        Request::Attach {
            addr: addr.to_owned(),
            close_with: None,
        }
    }

    #[test]
    fn a_request_without_close_with_is_still_v1() {
        // The exact bytes a panel built before `close_with` existed sent, so an older running panel
        // still understands every request that does not need the new field.
        assert_eq!(
            encode(&attach(Path::new("/run/user/1000/nvim.1.0"))).unwrap(),
            "{\"attach\":\"/run/user/1000/nvim.1.0\",\"v\":1}\n"
        );
        assert_eq!(encode(&Request::Raise).unwrap(), "{\"raise\":true,\"v\":1}\n");
    }

    #[test]
    fn a_v2_attach_round_trips_and_extra_keys_are_refused() {
        let request = Request::Attach {
            addr: PathBuf::from("/run/user/1000/eitri/split-nvim"),
            close_with: Some(CloseWith {
                pid: 4242,
                start: 123_456_789_012,
            }),
        };
        let line = encode(&request).unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "v": 2,
                "attach": "/run/user/1000/eitri/split-nvim",
                "close_with": { "pid": 4242, "start": 123_456_789_012u64 },
            })
        );
        assert_eq!(parse_request(line.trim_end().as_bytes()), Ok(request));

        let malformed: [&[u8]; 12] = [
            br#"{"v":2,"attach":"/x"}"#,
            br#"{"v":2,"close_with":{"pid":1,"start":2}}"#,
            br#"{"v":2,"attach":"/x","close_with":{"pid":1,"start":2},"more":1}"#,
            br#"{"v":2,"attach":"/x","close_with":{"pid":1}}"#,
            br#"{"v":2,"attach":"/x","close_with":{"start":2}}"#,
            br#"{"v":2,"attach":"/x","close_with":{"pid":1,"start":2,"more":1}}"#,
            br#"{"v":2,"attach":"/x","close_with":null}"#,
            br#"{"v":2,"attach":"/x","close_with":{"pid":-1,"start":2}}"#,
            br#"{"v":2,"attach":"/x","close_with":{"pid":4294967296,"start":2}}"#,
            br#"{"v":2,"attach":"/x","close_with":{"pid":1,"start":2.5}}"#,
            br#"{"v":2,"attach":1,"close_with":{"pid":1,"start":2}}"#,
            br#"{"v":2,"raise":true}"#,
        ];
        for line in malformed {
            assert_eq!(
                parse_request(line),
                Err("malformed request".to_owned()),
                "{}",
                String::from_utf8_lossy(line)
            );
        }
        // v1 never carries the field: it is a v2 capability, not an optional v1 key.
        assert_eq!(
            parse_request(br#"{"v":1,"attach":"/x","close_with":{"pid":1,"start":2}}"#),
            Err("malformed request".to_owned())
        );
        assert_eq!(
            parse_request(br#"{"v":3,"attach":"/x","close_with":{"pid":1,"start":2}}"#),
            Err("unsupported protocol version 3".to_owned())
        );
    }

    #[test]
    fn a_v1_server_shape_still_parses() {
        assert_eq!(parse_request(br#"{"v":1,"attach":"/x"}"#), Ok(attach(Path::new("/x"))));
        assert_eq!(parse_request(br#"{"attach":"/x","v":1}"#), Ok(attach(Path::new("/x"))));
        assert_eq!(parse_request(br#"{"v":1,"raise":true}"#), Ok(Request::Raise));
    }

    #[test]
    fn the_split_socket_is_under_the_cap_under_macos_tmpdir() {
        // macOS's first account and its widest pid. A split needs a pidfd, so it runs on Linux only, where
        // the runtime dir (or `/tmp/eitri-<uid>`) is far shorter; this keeps the name honest everywhere.
        let dir = control_dir_for(None, Path::new(MACOS_TMP), 501);
        let sock = split_socket_path(&dir, &root(), 99999, 0xffff_ffff).unwrap();
        assert_eq!(sock.as_os_str().len(), 101);
        assert_eq!(sock.extension(), Some(OsStr::new("sock")));
        let stem = sock.file_stem().unwrap().to_str().unwrap();
        assert_eq!(stem, format!("split-{}-99999-ffffffff", key(&root())));
        // The nonce is always eight digits, so names never collide by width.
        let small = split_socket_path(&dir, &root(), 7, 1).unwrap();
        let small_stem = small.file_stem().unwrap().to_str().unwrap();
        assert!(small_stem.ends_with("-7-00000001"), "{}", small.display());
        assert_eq!(small.parent(), Some(dir.as_path()));
    }

    #[test]
    fn the_stat_fields_are_read_after_the_last_parenthesis() {
        // A command name may hold spaces and parentheses; field 4 is the parent, field 22 the start.
        let tail: Vec<String> = (5..=52).map(|n| (n * 10).to_string()).collect();
        let stat = format!("4242 (a) b (c) d) S 77 {}\n", tail.join(" "));
        assert_eq!(stat_parent_and_start(&stat), Some((77, 220)));
        assert_eq!(stat_parent_and_start("4242 (short) S 77 1 2"), None);
        assert_eq!(stat_parent_and_start("no parenthesis at all"), None);
    }

    #[test]
    fn close_with_is_judged_against_the_peer() {
        let tail: Vec<String> = (5..=52).map(|n| n.to_string()).collect();
        let stats = move |pid: u32| match pid {
            // Parent 100, start time 22 (field 22 carries its own number in this table).
            200 => Some(format!("200 (child) S 100 {}", tail.join(" "))),
            _ => None,
        };
        let child = CloseWith { pid: 200, start: 22 };
        assert_eq!(check_close_with(child, Some(100), &stats), Ok(()));
        assert!(check_close_with(child, Some(101), &stats).is_err());
        assert!(check_close_with(CloseWith { pid: 200, start: 23 }, Some(100), &stats).is_err());
        assert!(check_close_with(CloseWith { pid: 201, start: 22 }, Some(100), &stats).is_err());
        // No peer pid (macOS, or a peer in another pid namespace): nothing can be proved.
        assert!(check_close_with(child, None, &stats).is_err());
    }

    /// The request a connection delivered, or the reply it got when it was refused.
    #[cfg(target_os = "linux")]
    fn exchange(sock: &Path, server: &ControlServer, request: &Request) -> Result<Received, String> {
        let line = encode(request).unwrap();
        let reply = raw(sock, line.as_bytes(), false);
        if reply != "ok\n" {
            assert!(server.requests().try_recv().is_err(), "refused yet delivered");
            return Err(reply);
        }
        Ok(server.requests().recv_timeout(Duration::from_secs(2)).unwrap())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_attach_carries_its_senders_chain() {
        let scratch = ScratchDir::new("eitri-pc", "chain");
        let dir = scratch.join("c");
        let server = ours(claim_for(&dir, &ControlKey::Project(&root()), None));
        let sock = socket_path(&dir, &root()).unwrap();
        let (nvim, _listener) = fake_nvim(&scratch);
        let received = exchange(&sock, &server, &attach(&nvim)).unwrap();
        assert_eq!(received.request, attach(&nvim));
        assert_eq!(received.sender_chain.first(), Some(&std::process::id()));
        assert_eq!(
            received.sender_chain,
            crate::proc_chain::ppid_chain(std::process::id(), &crate::proc_chain::proc_stat, 32)
        );
        // A raise names no editor, so it carries no chain.
        let raised = exchange(&sock, &server, &Request::Raise).unwrap();
        assert_eq!(raised.request, Request::Raise);
        assert!(raised.sender_chain.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_close_with_that_is_not_the_senders_child_is_refused() {
        let scratch = ScratchDir::new("eitri-pc", "closewith");
        let dir = scratch.join("c");
        let server = ours(claim_for(&dir, &ControlKey::Project(&root()), None));
        let sock = socket_path(&dir, &root()).unwrap();
        let (nvim, _listener) = fake_nvim(&scratch);
        let start_of = |pid: u32| {
            stat_parent_and_start(&crate::proc_chain::proc_stat(pid).unwrap())
                .unwrap()
                .1
        };
        let with = |pid: u32, start: u64| Request::Attach {
            addr: nvim.clone(),
            close_with: Some(CloseWith { pid, start }),
        };
        let refused = "refused: close_with is not the sender's child\n".to_owned();

        let me = std::process::id();
        assert_eq!(exchange(&sock, &server, &with(me, start_of(me))), Err(refused.clone()));
        let parent = std::os::unix::process::parent_id();
        assert_eq!(
            exchange(&sock, &server, &with(parent, start_of(parent))),
            Err(refused.clone())
        );

        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let start = start_of(pid);
        let wrong = exchange(&sock, &server, &with(pid, start + 1));
        let right = exchange(&sock, &server, &with(pid, start));
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(wrong, Err(refused));
        let right = right.unwrap();
        assert_eq!(right.request, with(pid, start));
        assert_eq!(right.sender_chain, vec![pid]);
    }

    #[test]
    fn a_server_dropped_without_cleanup_still_removes_its_files() {
        let scratch = ScratchDir::new("eitri-pc", "dropped");
        let dir = scratch.join("c");
        drop(ours(claim_for(&dir, &ControlKey::Project(&root()), None)));
        assert!(!socket_path(&dir, &root()).unwrap().exists());
        assert!(!pid_path(&dir, &root()).exists());
    }
}
