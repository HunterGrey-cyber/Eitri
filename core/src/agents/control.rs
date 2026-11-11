//! The control socket of one process's agent rows: `ag-<instance>.sock` in the control directory.
//!
//! A reader of the rows that wants the window owning a row to act sends one JSON line here and reads
//! one line back, `ok` or `refused: <why>`. Two verbs exist: `focus` brings a tab forward (or the
//! module that last had the keys, for `tab: null`) and `new` opens an empty tab. Neither answers a
//! permission card, a question or a trust prompt, sends text, or changes a mode; whether a person's
//! gesture started the request is judged where it starts, not here.
//!
//! The posture is the panel control socket's: the control directory 0700 and checked
//! (`panel_control::ensure_dir`), the socket 0600 and built through `agent::socket_path::in_dir`, a
//! peer served only when its uid is this process's effective uid, checked before a byte is read, and
//! one line of at most [`MAX_LINE`] bytes that has to arrive within [`DEADLINE`], unknown fields
//! refused. Any process of this user may call it; the uid and the private directory are the boundary.
//!
//! The server accepts, reads and calls the handler on its own thread, one connection at a time. The
//! handler may block that thread until the window's main loop has acted, so `ok` means it ran.
//! [`send`] blocks for up to two deadlines and is for a worker thread, never the GTK thread.

use crate::agents::Intent;
use crate::panel_control::{ensure_dir, euid, read_line, reply_line, LineFail};
use agent::private_fs::{remove_if_same, FileIdentity};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The longest request line, newline not counted.
pub const MAX_LINE: usize = 4096;

/// How long a peer has to send its whole line once accepted. A client waits twice this for the reply.
pub const DEADLINE: Duration = Duration::from_secs(1);

/// The only version of the request lines this build speaks.
pub const PROTOCOL_V: u32 = 1;

/// How many lowercase hex digits an instance id has.
const INSTANCE_HEX: usize = 16;

/// The pause after a failed `poll` or `accept` (descriptors exhausted, say), so the thread never spins.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// One request line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "verb", rename_all = "snake_case")]
pub enum Request {
    /// Bring a tab of this process forward and present its window.
    Focus {
        v: u32,
        /// The number after the dot in the row id; `None` is the module that last had the keys in
        /// this window. The key must be present: a line that leaves it out is refused, not read as
        /// `null`.
        #[serde(deserialize_with = "present")]
        tab: Option<u64>,
        /// Whether a waiting card comes forward with the tab.
        card_first: bool,
        intent: Intent,
        /// The activation token the window is presented with, when the caller had one.
        #[serde(deserialize_with = "present")]
        token: Option<String>,
    },
    /// Open an empty tab in this process's base chat view (or, sent to a hub, a new chat window).
    New {
        v: u32,
        #[serde(deserialize_with = "present")]
        token: Option<String>,
    },
}

/// A nullable field whose key is still required: serde reads a missing `Option` as `None` unless
/// the field has its own deserializer.
fn present<'de, D, T>(de: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(de)
}

impl Request {
    pub fn version(&self) -> u32 {
        match self {
            Request::Focus { v, .. } | Request::New { v, .. } => *v,
        }
    }
}

/// The one line that answers a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Ok,
    Refused(String),
}

impl Reply {
    /// The reply as it is sent, without the newline. A reason is kept on one line whatever it held.
    pub fn line(&self) -> String {
        match self {
            Reply::Ok => "ok".to_owned(),
            Reply::Refused(why) => format!("refused: {}", why.replace(['\n', '\r'], " ")),
        }
    }

    /// The reply one line says, or `None` for a line that is neither `ok` nor a refusal.
    pub fn parse(line: &str) -> Option<Reply> {
        if line == "ok" {
            return Some(Reply::Ok);
        }
        line.strip_prefix("refused: ").map(|why| Reply::Refused(why.to_owned()))
    }
}

/// A bound control socket and the thread serving it.
///
/// Stopping (or dropping) it removes the socket only while the name still holds the socket it bound,
/// and never waits for the thread: a handler may be waiting for the very thread that stops it. The
/// thread ends once a request in progress has been answered.
pub struct Server {
    dir: std::fs::File,
    name: OsString,
    bound: FileIdentity,
    stop: Arc<AtomicBool>,
    /// The other end of the thread's wake pair: closing it wakes the thread's `poll`.
    wake: Option<UnixStream>,
}

impl Server {
    /// Binds `ag-<instance>.sock` in `control_dir` and starts serving it. `handle` runs on the server's
    /// thread for each accepted request, and may block it until the GTK side answers.
    ///
    /// Fails, having created nothing, when the path would pass the socket path limit or `instance` is
    /// not an instance id; fails when the directory is not private or the name is taken.
    pub fn bind(
        control_dir: &Path,
        instance: &str,
        handle: impl Fn(Request) -> Reply + Send + 'static,
    ) -> io::Result<Server> {
        bind_with(control_dir, instance, crate::panel_control::peer_uid, handle)
    }

    /// Stops serving and removes the socket if it is still the one bound.
    pub fn stop(self) {
        drop(self);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Unlinked first, so no new peer finds it while the thread winds down.
        let _ = remove_if_same(&self.dir, &self.name, self.bound);
        self.wake.take();
    }
}

/// `control_dir/ag-<instance>.sock`, through `agent::socket_path::in_dir`, which refuses a path over
/// the socket path limit. `instance` must be 16 lowercase hex digits, so no id read from elsewhere
/// can name a path outside the directory.
pub fn socket_path(control_dir: &Path, instance: &str) -> io::Result<PathBuf> {
    if !is_instance(instance) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{instance:?} is not an instance id ({INSTANCE_HEX} lowercase hex digits)"),
        ));
    }
    agent::socket_path::in_dir(control_dir, &format!("ag-{instance}.sock"))
}

fn is_instance(id: &str) -> bool {
    id.len() == INSTANCE_HEX && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Sends `req` to the process `instance` and returns its reply. Checks that the socket is served by
/// this user before writing, and waits at most two [`DEADLINE`]s for the connection and again for the
/// reply. The connect is non-blocking with a deadline, so a server whose accept queue stays full
/// leaves nothing behind once the time is up. Blocks: call it off the GTK thread.
pub fn send(control_dir: &Path, instance: &str, req: &Request) -> io::Result<Reply> {
    send_with(control_dir, instance, req, crate::panel_control::peer_uid)
}

type PeerUid = fn(&UnixStream) -> io::Result<u32>;

fn bind_with<H>(control_dir: &Path, instance: &str, peer_uid: PeerUid, handle: H) -> io::Result<Server>
where
    H: Fn(Request) -> Reply + Send + 'static,
{
    let path = socket_path(control_dir, instance)?;
    let name = path
        .file_name()
        .map(OsString::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a socket path with no file name"))?;
    ensure_dir(control_dir).map_err(|why| io::Error::new(io::ErrorKind::PermissionDenied, why))?;
    let dir = agent::private_fs::open_dir(control_dir)?;
    let listener = crate::panel_control::bind_unfollowed(&path)?;
    let meta = path.symlink_metadata()?;
    let bound = FileIdentity {
        dev: meta.dev(),
        ino: meta.ino(),
    };
    // `bind` leaves the umask's mode on the file.
    let permissions = crate::panel_control::make_control_socket_private(&dir, &path, bound);
    let started = (move || -> io::Result<(UnixStream, Arc<AtomicBool>)> {
        permissions?;
        listener.set_nonblocking(true)?;
        let (wake, wake_rx) = UnixStream::pair()?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("eitri-agents-control".into())
            .spawn(move || run(&listener, &wake_rx, &thread_stop, peer_uid, &handle))?;
        Ok((wake, stop))
    })();
    match started {
        Ok((wake, stop)) => Ok(Server {
            dir,
            name,
            bound,
            stop,
            wake: Some(wake),
        }),
        Err(e) => {
            let _ = remove_if_same(&dir, &name, bound);
            Err(e)
        }
    }
}

/// The server thread: wait for a peer or the wake, serve one connection at a time, end when stopped.
fn run<H: Fn(Request) -> Reply>(
    listener: &UnixListener,
    wake: &UnixStream,
    stop: &AtomicBool,
    peer_uid: PeerUid,
    handle: &H,
) {
    let me = euid();
    while !stop.load(Ordering::Acquire) {
        let mut fds = [
            libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `fds` is a valid array of two initialized `pollfd`s whose descriptors stay open
        // for the call; the length passed is its length.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if rc < 0 {
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                std::thread::sleep(ACCEPT_BACKOFF);
            }
            continue;
        }
        // The wake end is never written: anything on it means the server's end was closed.
        if fds[1].revents != 0 {
            return;
        }
        match listener.accept() {
            Ok((stream, _)) => serve_one(&stream, stop, peer_uid, me, handle),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => std::thread::sleep(ACCEPT_BACKOFF),
        }
    }
}

fn serve_one<H: Fn(Request) -> Reply>(stream: &UnixStream, stop: &AtomicBool, peer_uid: PeerUid, me: u32, handle: &H) {
    // A socket accepted from a non-blocking listener inherits that flag on macOS and the BSDs.
    if stream.set_nonblocking(false).is_err() || stream.set_write_timeout(Some(DEADLINE)).is_err() {
        return;
    }
    let refuse = |why: &str| {
        let _ = reply_line(stream, &Reply::Refused(why.to_owned()).line());
    };
    match peer_uid(stream) {
        Ok(uid) if uid == me => {}
        _ => return refuse("not your process"),
    }
    let line = match read_line(stream, Instant::now() + DEADLINE, MAX_LINE) {
        Ok(line) => line,
        Err(LineFail::TooLong) => return refuse("request too long"),
        Err(LineFail::Partial) => return refuse("malformed request"),
        Err(LineFail::TimedOut) => return refuse("request timed out"),
        Err(LineFail::Empty | LineFail::Io) => return,
    };
    let reply = match judge(&line) {
        // A request read while the server was being stopped is not acted on.
        Ok(_) if stop.load(Ordering::Acquire) => Reply::Refused("closing".to_owned()),
        Ok(request) => handle(request),
        Err(why) => Reply::Refused(why),
    };
    let _ = reply_line(stream, &reply.line());
}

/// The request one line says, or why it is refused. The version is read first, so a newer client's
/// line is told its version is unsupported rather than that a field is unknown.
fn judge(line: &[u8]) -> Result<Request, String> {
    let malformed = || "malformed request".to_owned();
    let value: serde_json::Value = serde_json::from_slice(line).map_err(|_| malformed())?;
    let version = value
        .get("v")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(malformed)?;
    if version != u64::from(PROTOCOL_V) {
        return Err(format!("unsupported protocol version {version}"));
    }
    serde_json::from_value(value).map_err(|_| malformed())
}

fn send_with(control_dir: &Path, instance: &str, req: &Request, peer_uid: PeerUid) -> io::Result<Reply> {
    let path = socket_path(control_dir, instance)?;
    let line = serde_json::to_string(req).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    if line.len() > MAX_LINE {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "request too long"));
    }
    let stream = crate::instance_dir::connect_by(&path, Instant::now() + 2 * DEADLINE).map_err(|e| match e.kind() {
        io::ErrorKind::TimedOut => io::Error::new(
            io::ErrorKind::TimedOut,
            format!("{} did not accept a connection in time", path.display()),
        ),
        _ => e,
    })?;
    // Before a byte goes out: the socket file can be swapped after anyone looked at it.
    if peer_uid(&stream)? != euid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is served by another user", path.display()),
        ));
    }
    stream.set_write_timeout(Some(DEADLINE))?;
    let wrote = (&stream).write_all(format!("{line}\n").as_bytes());
    // A server that refused before reading may have closed before the write; its line still counts.
    let answer = read_line(&stream, Instant::now() + 2 * DEADLINE, MAX_LINE);
    let answer = match (wrote, answer) {
        (_, Ok(answer)) => answer,
        (Err(e), Err(_)) => return Err(e),
        (Ok(()), Err(why)) => return Err(line_error(why)),
    };
    std::str::from_utf8(&answer)
        .ok()
        .and_then(Reply::parse)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "an answer that is neither ok nor refused"))
}

fn line_error(why: LineFail) -> io::Error {
    match why {
        LineFail::TimedOut => io::Error::new(io::ErrorKind::TimedOut, "no reply in time"),
        LineFail::Empty | LineFail::Partial => io::Error::new(io::ErrorKind::UnexpectedEof, "closed without a reply"),
        LineFail::TooLong => io::Error::new(io::ErrorKind::InvalidData, "reply too long"),
        LineFail::Io => io::Error::other("could not read the reply"),
    }
}

/// Doors for tests that need a peer of another uid, which no test can be.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_hooks {
    use super::*;

    /// [`Server::bind`] with `peer_uid` in place of reading the peer's credentials.
    pub fn bind_reading_peer_uid_with(
        control_dir: &Path,
        instance: &str,
        peer_uid: fn(&UnixStream) -> io::Result<u32>,
        handle: impl Fn(Request) -> Reply + Send + 'static,
    ) -> io::Result<Server> {
        bind_with(control_dir, instance, peer_uid, handle)
    }

    /// [`send`] with `peer_uid` in place of reading the server's credentials.
    pub fn send_reading_peer_uid_with(
        control_dir: &Path,
        instance: &str,
        req: &Request,
        peer_uid: fn(&UnixStream) -> io::Result<u32>,
    ) -> io::Result<Reply> {
        send_with(control_dir, instance, req, peer_uid)
    }
}
