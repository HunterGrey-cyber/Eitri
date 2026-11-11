//! The window's clipboard, served to the embedded nvim's `tmux` shim.
//!
//! With `$TMUX` set and none of `wl-copy`, `xclip` or `xsel` on `PATH`, nvim's clipboard provider is
//! `tmux`: a yank to `+` or `*` runs `tmux load-buffer [-w] -` with the text on stdin, a paste runs
//! `tmux save-buffer -` and reads stdout. The shim answers those two calls by asking the window that
//! started nvim over `c.sock`, beside the pane-switch socket in the same 0700 directory, and the
//! window copies or reads with its toolkit's clipboard. Where the person set `g:clipboard` or a tool
//! is installed, nvim never reaches the shim, so nothing here runs.
//!
//! One request per connection, served one connection at a time on a thread of its own, so nothing
//! here ever runs on the toolkit's thread. Every step has a bound:
//!
//! - A request must arrive whole within [`READ_REQUEST_WITHIN`]; a peer that is not this euid is
//!   closed before a byte is read.
//! - Text in either direction is at most [`MAX_TEXT_BYTES`]; a longer length prefix is refused
//!   before anything is allocated.
//! - The host has [`ANSWER_WITHIN`] to answer; after that the shim is told the clipboard did not
//!   answer, and a late answer is dropped. nvim blocks on a paste for at most that long plus the
//!   shim's own start, and never on a copy (it hands the copy to a job).
//!
//! The text itself is never logged, never put in an error and never kept after its answer is
//! written: every message this module makes names a size or a reason, not content.
//!
//! The wire, all lengths `u32` little-endian:
//! request `c <len> <bytes>` (copy) or `p <0>` (paste); answer `k <0>` (done), `t <len> <bytes>`
//! (the clipboard's text) or `e <len> <utf-8 reason>` (failed, at most [`MAX_REASON_BYTES`]).

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The socket's file name, in the pane-switch channel's directory. As short as the switch socket's
/// own `s.sock`, so it costs nothing of that directory's path budget.
pub const SOCKET_NAME: &str = "c.sock";

/// The most text one copy or paste carries: 16 MiB, the same cap the nvim RPC link puts on a frame.
/// A copy over it is refused (the text stays in nvim's own register); a paste over it is refused
/// rather than cut.
pub const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;

/// How long the host has to answer one request. Two seconds is a clipboard owner that is not
/// answering, the bound the bottom terminal's own paste waits; a Wayland or X11 transfer of
/// ordinary text takes milliseconds.
pub const ANSWER_WITHIN: Duration = Duration::from_secs(2);

/// How long the server waits for a whole request to arrive once a connection is accepted.
pub const READ_REQUEST_WITHIN: Duration = Duration::from_secs(2);

/// How long the shim waits for an answer after its request is written: the host's own bound plus a
/// margin for the server thread and the socket.
pub const CLIENT_WAIT: Duration = Duration::from_secs(3);

/// The longest reason an `e` answer carries.
pub const MAX_REASON_BYTES: usize = 512;

/// How often the server thread looks at its stop flag while no connection arrives.
const STOP_CHECK: Duration = Duration::from_millis(250);

/// What nvim asked for.
#[derive(Clone, PartialEq, Eq)]
pub enum Request {
    /// Put these bytes on the clipboard.
    Copy(Vec<u8>),
    /// Give back the clipboard's text.
    Paste,
}

/// What the host answered.
#[derive(Clone, PartialEq, Eq)]
pub enum Answer {
    /// The copy is on the clipboard.
    Done,
    /// The clipboard's text.
    Text(Vec<u8>),
    /// Why the request could not be done; never holds clipboard text.
    Failed(String),
}

/// Prints a size, never text, so a `Request` in a test failure or a log cannot show what was yanked.
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Request::Copy(text) => write!(f, "Copy({} bytes)", text.len()),
            Request::Paste => f.write_str("Paste"),
        }
    }
}

/// Prints sizes, never text, so an `Answer` in a test failure or a log cannot show what was copied.
impl std::fmt::Debug for Answer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Answer::Done => f.write_str("Done"),
            Answer::Text(text) => write!(f, "Text({} bytes)", text.len()),
            Answer::Failed(reason) => f.debug_tuple("Failed").field(reason).finish(),
        }
    }
}

/// Why a request or an answer could not be read.
#[derive(Debug)]
pub enum WireError {
    Io(io::Error),
    /// A kind byte that is not one of the wire's.
    UnknownKind(u8),
    /// A length over the cap for its kind.
    TooLong(usize),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Io(e) => write!(f, "{e}"),
            WireError::UnknownKind(kind) => write!(f, "unknown message kind {kind:#04x}"),
            WireError::TooLong(len) => write!(f, "{len} bytes is over the limit"),
        }
    }
}

impl From<io::Error> for WireError {
    fn from(e: io::Error) -> Self {
        WireError::Io(e)
    }
}

fn write_frame(w: &mut impl Write, kind: u8, body: &[u8]) -> io::Result<()> {
    let len = u32::try_from(body.len()).map_err(|_| io::Error::other("frame over 4 GiB"))?;
    let mut head = [0u8; 5];
    head[0] = kind;
    head[1..].copy_from_slice(&len.to_le_bytes());
    w.write_all(&head)?;
    w.write_all(body)?;
    w.flush()
}

/// Reads a kind and a length, checks the length against `cap(kind)` before allocating, then the body.
fn read_frame(r: &mut impl Read, cap: impl Fn(u8) -> Option<usize>) -> Result<(u8, Vec<u8>), WireError> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)?;
    let kind = head[0];
    let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
    let max = cap(kind).ok_or(WireError::UnknownKind(kind))?;
    if len > max {
        return Err(WireError::TooLong(len));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok((kind, body))
}

pub fn write_request(w: &mut impl Write, request: &Request) -> io::Result<()> {
    match request {
        Request::Copy(text) => write_frame(w, b'c', text),
        Request::Paste => write_frame(w, b'p', &[]),
    }
}

pub fn read_request(r: &mut impl Read) -> Result<Request, WireError> {
    let (kind, body) = read_frame(r, |kind| match kind {
        b'c' => Some(MAX_TEXT_BYTES),
        b'p' => Some(0),
        _ => None,
    })?;
    Ok(match kind {
        b'c' => Request::Copy(body),
        _ => Request::Paste,
    })
}

pub fn write_answer(w: &mut impl Write, answer: &Answer) -> io::Result<()> {
    match answer {
        Answer::Done => write_frame(w, b'k', &[]),
        Answer::Text(text) => write_frame(w, b't', text),
        Answer::Failed(reason) => write_frame(w, b'e', bounded_reason(reason).as_bytes()),
    }
}

pub fn read_answer(r: &mut impl Read) -> Result<Answer, WireError> {
    let (kind, body) = read_frame(r, |kind| match kind {
        b'k' => Some(0),
        b't' => Some(MAX_TEXT_BYTES),
        b'e' => Some(MAX_REASON_BYTES),
        _ => None,
    })?;
    Ok(match kind {
        b'k' => Answer::Done,
        b't' => Answer::Text(body),
        _ => Answer::Failed(bounded_reason(&String::from_utf8_lossy(&body))),
    })
}

/// A reason on one line of printable characters, at most [`MAX_REASON_BYTES`], cut at a character
/// boundary: nvim shows it on its message line.
pub fn bounded_reason(reason: &str) -> String {
    let mut out = String::new();
    for c in reason.chars() {
        let c = if c.is_control() { ' ' } else { c };
        if out.len() + c.len_utf8() > MAX_REASON_BYTES {
            break;
        }
        out.push(c);
    }
    out
}

/// The host's half of one request: answer it once, from any thread. Dropping it unanswered answers
/// `Failed`, so a host that loses a request never leaves the shim waiting the whole bound.
pub struct Reply {
    sender: Option<mpsc::SyncSender<Answer>>,
}

impl Reply {
    pub fn send(mut self, answer: Answer) {
        if let Some(sender) = self.sender.take() {
            // The server has stopped waiting when this fails; the answer is simply dropped.
            let _ = sender.try_send(answer);
        }
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.try_send(Answer::Failed("the window dropped the request".to_string()));
        }
    }
}

/// The running server. Dropping it (or [`ClipboardServer::stop`]) ends the thread within
/// [`STOP_CHECK`] once the connection in hand, if any, is done.
pub struct ClipboardServer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ClipboardServer {
    /// Asks the thread to end and does not wait for it.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Asks the thread to end and waits until it has (tests).
    pub fn stop_and_join(mut self) {
        self.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ClipboardServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Who may use the socket: a peer of this euid that descends from the window's nvim.
///
/// "The window's nvim" is the direct child of the serving process (`server_pid`) whose environment,
/// as given to it at exec, holds `EITRI_PANE_SWITCH_SOCKET=<switch_socket>`: the window hands that
/// variable to its nvim child alone, so the walk needs no pid from the editor, and an nvim the
/// window starts again is found the same way. The peer's pid comes from `SO_PEERCRED`, and the walk
/// goes up `/proc/<pid>/stat`'s parent links. Anything the walk cannot read refuses: where there is
/// no `/proc`, or no peer pid (macOS's `getpeereid` gives none), every request is refused, which
/// costs nothing there because nvim's provider takes `pbcopy` first and never reaches the shim.
#[derive(Clone, Debug)]
pub struct Admission {
    /// The only uid served.
    pub euid: u32,
    /// The process the window's nvim is a direct child of.
    pub server_pid: u32,
    /// The value `EITRI_PANE_SWITCH_SOCKET` has in the nvim child's environment.
    pub switch_socket: std::ffi::OsString,
    /// Where `/proc` is (tests point it at a fake tree).
    pub proc_root: std::path::PathBuf,
    /// A person's own nvim the window holds, taken over from its terminal: no child of the window, so it and what it
    /// starts are admitted by its pid and start time, never by widening the rule for every process.
    pub attached: Option<AttachedNvim>,
    /// The server the window's nvim started for a `:restart` the editor followed: once that nvim has exited, the
    /// server is no descendant of the window, so it is admitted by the pid and start time the editor learned from it,
    /// asked at each request (the editor learns it after the server answers).
    pub followed: Option<FollowedNvim>,
}

/// Where [`Admission::followed`] reads the followed server's process from, at each request.
#[derive(Clone)]
pub struct FollowedNvim(std::sync::Arc<dyn Fn() -> Option<AttachedNvim> + Send + Sync>);

impl FollowedNvim {
    pub fn new(learned: impl Fn() -> Option<AttachedNvim> + Send + Sync + 'static) -> Self {
        FollowedNvim(std::sync::Arc::new(learned))
    }

    fn now(&self) -> Option<AttachedNvim> {
        (self.0)()
    }
}

impl std::fmt::Debug for FollowedNvim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FollowedNvim({:?})", self.now())
    }
}

/// The process of a person's own nvim the window shows ([`Admission::attached`]): its pid, and the start time that
/// tells it from a later process given the same pid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachedNvim {
    pub pid: u32,
    pub start_time: u64,
}

impl Admission {
    /// This process, this euid, the real `/proc`, and the channel's switch socket.
    pub fn for_window(switch_socket: &Path) -> Self {
        Self {
            // SAFETY: `geteuid` takes no arguments and cannot fail.
            euid: unsafe { libc::geteuid() },
            server_pid: std::process::id(),
            switch_socket: switch_socket.as_os_str().to_owned(),
            proc_root: std::path::PathBuf::from("/proc"),
            attached: None,
            followed: None,
        }
    }

    /// The same admission, also letting in `nvim` and what descends from it.
    pub fn with_attached(mut self, nvim: AttachedNvim) -> Self {
        self.attached = Some(nvim);
        self
    }

    /// The same admission, also letting in the server a followed `:restart` started, once the editor learned it.
    pub fn with_followed(mut self, followed: FollowedNvim) -> Self {
        self.followed = Some(followed);
        self
    }

    /// Whether `pid` is the attached nvim's process, or the followed server's, started when it was.
    fn is_the_attached_nvim(&self, pid: u32) -> bool {
        let is = |nvim: AttachedNvim| nvim.pid == pid && start_time_of(&self.proc_root, pid) == Some(nvim.start_time);
        self.attached.is_some_and(is) || self.followed.as_ref().and_then(FollowedNvim::now).is_some_and(is)
    }

    /// Whether `pid` is the window's nvim or descends from it. Fails closed on anything unreadable.
    pub fn descends_from_nvim(&self, pid: u32) -> bool {
        let mut pid = pid;
        // A chain longer than this is no nvim job; it also ends a loop a fake tree could make.
        for _ in 0..MAX_ANCESTRY {
            if self.is_the_attached_nvim(pid) {
                return true;
            }
            let Some(parent) = parent_of(&self.proc_root, pid) else {
                return false;
            };
            if parent == self.server_pid {
                return self.is_the_windows_nvim(pid);
            }
            if parent <= 1 {
                return false;
            }
            pid = parent;
        }
        false
    }

    /// Whether `pid`, a direct child of the server, was started with this channel's switch socket.
    fn is_the_windows_nvim(&self, pid: u32) -> bool {
        use std::os::unix::ffi::OsStrExt;
        let Ok(environ) = std::fs::read(self.proc_root.join(pid.to_string()).join("environ")) else {
            return false;
        };
        let mut want = b"EITRI_PANE_SWITCH_SOCKET=".to_vec();
        want.extend_from_slice(self.switch_socket.as_bytes());
        environ.split(|&b| b == 0).any(|entry| entry == want.as_slice())
    }
}

/// How many parent links [`Admission::descends_from_nvim`] follows at most.
const MAX_ANCESTRY: usize = 64;

/// The start time in `<proc>/<pid>/stat` (its 22nd field), in clock ticks since boot.
fn start_time_of(proc_root: &Path, pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(proc_root.join(pid.to_string()).join("stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// The parent pid in `<proc>/<pid>/stat`: the field after the state, which follows the last `)` (a
/// command name may hold spaces and parentheses).
fn parent_of(proc_root: &Path, pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(proc_root.join(pid.to_string()).join("stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// The server's time limits; [`Bounds::DEFAULT`] in production, shorter ones in tests.
#[derive(Clone, Copy, Debug)]
pub struct Bounds {
    /// How long a whole request may take to arrive.
    pub read_request: Duration,
    /// How long the host has to answer, and how long the whole answer may take to write.
    pub answer: Duration,
}

impl Bounds {
    pub const DEFAULT: Bounds = Bounds {
        read_request: READ_REQUEST_WITHIN,
        answer: ANSWER_WITHIN,
    };
}

/// The reason a peer that is not the window's nvim (or a descendant) is refused with.
pub const NOT_NVIM: &str = "this socket serves only the window's own nvim";

/// Serves `listener` on a thread of its own for the peers `admission` lets in: each request is
/// handed to `host` with a [`Reply`], and whatever the host answers within [`ANSWER_WITHIN`] is
/// written back. `host` is called on the server thread and must not block: a toolkit host posts the
/// request to its own loop and answers from there.
pub fn serve<H>(listener: UnixListener, admission: Admission, host: H) -> io::Result<ClipboardServer>
where
    H: Fn(Request, Reply) + Send + 'static,
{
    serve_with(listener, admission, Bounds::DEFAULT, host)
}

/// [`serve`] with other time limits.
pub fn serve_with<H>(
    listener: UnixListener,
    admission: Admission,
    bounds: Bounds,
    host: H,
) -> io::Result<ClipboardServer>
where
    H: Fn(Request, Reply) + Send + 'static,
{
    listener.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("eitri-clipboard".to_string())
        .spawn(move || run(listener, &admission, bounds, host, &thread_stop))?;
    Ok(ClipboardServer {
        stop,
        thread: Some(thread),
    })
}

fn run<H: Fn(Request, Reply)>(
    listener: UnixListener,
    admission: &Admission,
    bounds: Bounds,
    host: H,
    stop: &AtomicBool,
) {
    while !stop.load(Ordering::Relaxed) {
        if !wait_readable(&listener, STOP_CHECK) {
            continue;
        }
        match listener.accept() {
            Ok((stream, _)) => serve_one(stream, admission, bounds, &host),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                eprintln!("[clipboard] accept failed: {e}");
                std::thread::sleep(STOP_CHECK);
            }
        }
    }
}

/// Whether `listener` has a connection waiting, after at most `within`.
fn wait_readable(listener: &UnixListener, within: Duration) -> bool {
    let mut fd = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = within.as_millis().min(i32::MAX as u128) as libc::c_int;
    // SAFETY: one valid `pollfd` that lives across the call, and its count.
    let got = unsafe { libc::poll(&mut fd, 1, ms) };
    got > 0 && fd.revents & libc::POLLIN != 0
}

/// Whether the peer on `stream` may be served: its uid first (another uid is closed before a byte
/// is read or written), then its descent from the window's nvim. The pid `SO_PEERCRED` gives was
/// taken at connect; where the kernel gives a pidfd (Linux 6.5 and later) it must still name that
/// pid after the walk, so a peer that exited and whose number went to a process under nvim is not
/// taken for it. On an older kernel that narrow case is not caught.
enum Admitted {
    Yes,
    /// Same uid, not the window's nvim: told why, then closed.
    NotNvim,
    /// Another uid, or no credentials: closed unanswered.
    Closed,
}

fn admit(stream: &UnixStream, admission: &Admission) -> Admitted {
    match crate::panel_control::peer_uid(stream) {
        Ok(uid) if uid == admission.euid => {}
        Ok(_) => return Admitted::Closed,
        Err(e) => {
            eprintln!("[clipboard] could not read a peer's uid: {e}");
            return Admitted::Closed;
        }
    }
    let Some(pid) = crate::panel_control::peer_pid(stream) else {
        return Admitted::NotNvim;
    };
    let pidfd = crate::panel_control::peer_pidfd(stream);
    if !admission.descends_from_nvim(pid) {
        return Admitted::NotNvim;
    }
    if pidfd.is_some_and(|fd| !crate::panel_control::pidfd_still_names(&fd, pid)) {
        return Admitted::NotNvim;
    }
    Admitted::Yes
}

fn serve_one<H: Fn(Request, Reply)>(mut stream: UnixStream, admission: &Admission, bounds: Bounds, host: &H) {
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    match admit(&stream, admission) {
        Admitted::Yes => {}
        Admitted::Closed => return,
        Admitted::NotNvim => {
            let _ = write_answer(
                &mut Deadline::new(&mut stream, bounds.answer),
                &Answer::Failed(NOT_NVIM.to_string()),
            );
            return;
        }
    }
    let request = match read_request_within(&mut stream, bounds.read_request) {
        Ok(request) => request,
        Err(WireError::TooLong(len)) if len > MAX_TEXT_BYTES => {
            let _ = write_answer(
                &mut Deadline::new(&mut stream, bounds.answer),
                &Answer::Failed(over_limit(len)),
            );
            return;
        }
        Err(e) => {
            eprintln!("[clipboard] dropped a request: {e}");
            return;
        }
    };
    let (sender, receiver) = mpsc::sync_channel(1);
    host(request, Reply { sender: Some(sender) });
    let answer = receiver.recv_timeout(bounds.answer).unwrap_or_else(|_| {
        Answer::Failed(format!(
            "the clipboard did not answer within {} s",
            bounds.answer.as_secs_f32()
        ))
    });
    // One deadline over the whole answer: a peer that drains a long text slowly cannot hold the
    // thread past it, however the writes are split.
    if let Err(e) = write_answer(&mut Deadline::new(&mut stream, bounds.answer), &answer) {
        eprintln!("[clipboard] could not write an answer: {e}");
    }
}

/// The reason a text over [`MAX_TEXT_BYTES`] is refused with, naming its size and the limit.
pub fn over_limit(len: usize) -> String {
    format!(
        "{:.1} MiB is over the clipboard limit of {} MiB",
        len as f64 / (1024.0 * 1024.0),
        MAX_TEXT_BYTES / (1024 * 1024)
    )
}

/// [`read_request`] with one deadline over the whole request, however it is split across reads.
fn read_request_within(stream: &mut UnixStream, within: Duration) -> Result<Request, WireError> {
    read_request(&mut Deadline::new(stream, within))
}

/// A stream whose every read and write gets only the time left before `until`, so one deadline
/// bounds a whole message however the kernel splits it.
struct Deadline<'a> {
    stream: &'a mut UnixStream,
    until: Instant,
}

impl<'a> Deadline<'a> {
    fn new(stream: &'a mut UnixStream, within: Duration) -> Self {
        Self {
            stream,
            until: Instant::now() + within,
        }
    }

    fn until(stream: &'a mut UnixStream, until: Instant) -> Self {
        Self { stream, until }
    }

    fn left(&self) -> io::Result<Duration> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "the time ran out"));
        }
        Ok(left)
    }
}

impl Read for Deadline<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.left()?;
        match self.stream.set_read_timeout(Some(left)) {
            Ok(()) => self.stream.read(buf),
            // Nothing more can arrive, so what is left is read as one call that never waits.
            Err(e) if shut_both_ways(&e) => recv_without_waiting(self.stream, buf),
            Err(e) => Err(e),
        }
    }
}

impl Write for Deadline<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let left = self.left()?;
        match self.stream.set_write_timeout(Some(left)) {
            Ok(()) => self.stream.write(buf),
            Err(e) if shut_both_ways(&e) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the socket is shut in both directions",
            )),
            Err(e) => Err(e),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

/// Whether setting a timeout failed because both directions of the socket are shut. macOS refuses every socket
/// option then, with `EINVAL`: the client has shut its writing side once its request is out, so a server that answers
/// and closes before the client's next read leaves it in that state, and so does a peer that closed on a server.
/// Neither direction can block any more: a read returns what is left and then the end, a write fails. Linux sets the
/// timeout in that state too, so there this is never true and the plain read or write runs.
fn shut_both_ways(e: &io::Error) -> bool {
    cfg!(target_os = "macos") && e.kind() == io::ErrorKind::InvalidInput
}

/// One `recv` that never waits (`MSG_DONTWAIT`), for a socket [`shut_both_ways`] says nothing more can arrive on.
fn recv_without_waiting(stream: &UnixStream, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: `buf` is writable for its whole length, and the descriptor is `stream`'s own, open for the call.
    let n = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::MSG_DONTWAIT,
        )
    };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

/// Why the shim's request got no answer from the window.
#[derive(Debug)]
pub enum ClientError {
    /// No window listens at the socket (connect failed).
    Unreachable(io::Error),
    /// The window took longer than [`CLIENT_WAIT`].
    TimedOut,
    /// Anything else on the wire.
    Wire(WireError),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Unreachable(e) => write!(f, "the window's clipboard is not reachable: {e}"),
            ClientError::TimedOut => write!(f, "the window did not answer within {} s", CLIENT_WAIT.as_secs()),
            ClientError::Wire(e) => write!(f, "the window's answer could not be read: {e}"),
        }
    }
}

/// Sends one request to the window serving `socket` and waits at most `wait` ([`CLIENT_WAIT`] in
/// the shim) for its answer, the connect included: a server whose queue is full cannot hold nvim
/// past it. Nothing is written until the socket's holder is known to be this euid, so a copy never
/// goes to a listener another user put at the path.
pub fn request(socket: &Path, request: &Request, wait: Duration) -> Result<Answer, ClientError> {
    let until = Instant::now() + wait;
    let timed_out = |e: io::Error| match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => ClientError::TimedOut,
        _ => ClientError::Wire(WireError::Io(e)),
    };
    let mut stream = crate::instance_dir::connect_by(socket, until).map_err(|e| match e.kind() {
        io::ErrorKind::TimedOut => ClientError::TimedOut,
        _ => ClientError::Unreachable(e),
    })?;
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    let euid = unsafe { libc::geteuid() };
    match crate::panel_control::peer_uid(&stream) {
        Ok(uid) if uid == euid => {}
        Ok(_) => {
            return Err(ClientError::Unreachable(io::Error::other(
                "the socket is another user's",
            )))
        }
        Err(e) => return Err(ClientError::Unreachable(e)),
    }
    write_request(&mut Deadline::until(&mut stream, until), request).map_err(timed_out)?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    read_answer(&mut Deadline::until(&mut stream, until)).map_err(|e| match e {
        WireError::Io(e) => timed_out(e),
        other => ClientError::Wire(other),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Server tests use short limits so they assert outcomes, never wall-clock bounds: each limit is
    /// far shorter than the client's own wait, so an answer that comes from the server's limit (and not
    /// the client giving up) proves the limit held.
    const SHORT: Bounds = Bounds {
        read_request: Duration::from_millis(300),
        answer: Duration::from_millis(300),
    };
    /// Long enough that only a server that never bounds its wait could let the client reach it.
    const PATIENT: Duration = Duration::from_secs(60);

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nv-hc-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    const SWITCH: &str = "/chan/switch-socket";
    const FAKE_SERVER: u32 = 4_000_000;

    /// One process in a fake `/proc`: its `stat` (command name with a space and a parenthesis, as
    /// real names may have) and, when given, its environment.
    fn fake_proc(root: &Path, pid: u32, parent: u32, environ: Option<&[u8]>) {
        let dir = root.join(pid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stat"), format!("{pid} (odd (name) x) S {parent} 1 1 0 -1\n")).unwrap();
        if let Some(environ) = environ {
            std::fs::write(dir.join("environ"), environ).unwrap();
        }
    }

    fn nvim_environ() -> Vec<u8> {
        format!("HOME=/h\0EITRI_PANE_SWITCH_SOCKET={SWITCH}\0PATH=/bin\0").into_bytes()
    }

    fn fake_admission(root: &Path) -> Admission {
        Admission {
            // SAFETY: `geteuid` takes no arguments and cannot fail.
            euid: unsafe { libc::geteuid() },
            server_pid: FAKE_SERVER,
            switch_socket: SWITCH.into(),
            proc_root: root.to_path_buf(),
            attached: None,
            followed: None,
        }
    }

    /// An admission under which this test process is the window's nvim: a fake `/proc` naming it a
    /// direct child of a made-up server, started with the switch socket.
    fn admitting(dir: &Path) -> Admission {
        let root = dir.join("proc");
        fake_proc(&root, std::process::id(), FAKE_SERVER, Some(&nvim_environ()));
        fake_admission(&root)
    }

    fn bound(dir: &Path) -> (std::path::PathBuf, UnixListener) {
        let socket = dir.join(SOCKET_NAME);
        let listener = UnixListener::bind(&socket).unwrap();
        (socket, listener)
    }

    #[test]
    fn requests_and_answers_round_trip() {
        for request in [
            Request::Copy(b"two\nlines\n".to_vec()),
            Request::Copy(Vec::new()),
            Request::Paste,
        ] {
            let mut wire = Vec::new();
            write_request(&mut wire, &request).unwrap();
            assert_eq!(read_request(&mut wire.as_slice()).unwrap(), request);
        }
        for answer in [
            Answer::Done,
            Answer::Text(b"x\0y".to_vec()),
            Answer::Failed("no text".to_string()),
        ] {
            let mut wire = Vec::new();
            write_answer(&mut wire, &answer).unwrap();
            assert_eq!(read_answer(&mut wire.as_slice()).unwrap(), answer);
        }
    }

    #[test]
    fn a_length_over_the_cap_is_refused_before_anything_is_read() {
        let mut wire = vec![b'c'];
        wire.extend_from_slice(&((MAX_TEXT_BYTES + 1) as u32).to_le_bytes());
        assert!(matches!(read_request(&mut wire.as_slice()), Err(WireError::TooLong(_))));
        let mut wire = vec![b'p'];
        wire.extend_from_slice(&1u32.to_le_bytes());
        wire.push(b'x');
        assert!(matches!(read_request(&mut wire.as_slice()), Err(WireError::TooLong(1))));
        let mut wire = vec![b'e'];
        wire.extend_from_slice(&((MAX_REASON_BYTES + 1) as u32).to_le_bytes());
        assert!(matches!(read_answer(&mut wire.as_slice()), Err(WireError::TooLong(_))));
        let wire = [b'z', 0, 0, 0, 0];
        assert!(matches!(
            read_request(&mut wire.as_slice()),
            Err(WireError::UnknownKind(b'z'))
        ));
    }

    /// An answer the server wrote and closed on is read whole even once both directions are shut, which is when
    /// macOS refuses the read timeout.
    #[test]
    fn an_answer_left_on_a_socket_shut_both_ways_is_read_whole() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        // The client's order: its request is out and its writing side shut before the answer comes.
        client.shutdown(std::net::Shutdown::Write).unwrap();
        write_answer(&mut server, &Answer::Text(b"left behind".to_vec())).unwrap();
        drop(server);
        let answer = read_answer(&mut Deadline::new(&mut client, PATIENT)).unwrap();
        assert_eq!(answer, Answer::Text(b"left behind".to_vec()));
        let mut rest = [0u8; 1];
        assert_eq!(Deadline::new(&mut client, PATIENT).read(&mut rest).unwrap(), 0);
    }

    #[test]
    fn a_reason_is_one_bounded_line() {
        let reason = bounded_reason(&format!("a\nb\x1b[31m{}", "é".repeat(600)));
        assert!(reason.len() <= MAX_REASON_BYTES);
        assert!(!reason.chars().any(char::is_control));
        assert!(reason.starts_with("a b [31m"));
    }

    #[test]
    fn requests_and_answers_print_their_size_never_their_text() {
        assert_eq!(
            format!("{:?}", Answer::Text(b"secret-needle".to_vec())),
            "Text(13 bytes)"
        );
        assert_eq!(
            format!("{:?}", Request::Copy(b"secret-needle".to_vec())),
            "Copy(13 bytes)"
        );
        assert_eq!(format!("{:?}", Request::Paste), "Paste");
    }

    #[test]
    #[cfg_attr(
        target_os = "macos",
        ignore = "the server admits a peer by its pid, which macOS does not give, so every request is refused there"
    )]
    fn the_server_copies_and_pastes_through_the_host() {
        let dir = scratch("serve");
        let (socket, listener) = bound(&dir);
        let held = Arc::new(std::sync::Mutex::new(Vec::new()));
        let host_held = Arc::clone(&held);
        let server = serve(listener, admitting(&dir), move |request, reply| match request {
            Request::Copy(text) => {
                *host_held.lock().unwrap() = text;
                reply.send(Answer::Done);
            }
            Request::Paste => reply.send(Answer::Text(host_held.lock().unwrap().clone())),
        })
        .unwrap();
        let copied = request(&socket, &Request::Copy(b"one\ntwo\n".to_vec()), CLIENT_WAIT).unwrap();
        assert_eq!(copied, Answer::Done);
        let pasted = request(&socket, &Request::Paste, CLIENT_WAIT).unwrap();
        assert_eq!(pasted, Answer::Text(b"one\ntwo\n".to_vec()));
        server.stop_and_join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The answer comes from the server's own limit (its words), not from the client giving up after
    /// a minute: that is the bound holding. A dropped reply is answered with its own words.
    #[test]
    #[cfg_attr(
        target_os = "macos",
        ignore = "the server admits a peer by its pid, which macOS does not give, so every request is refused there"
    )]
    fn a_host_that_never_answers_is_bounded_and_one_that_drops_the_reply_fails_at_once() {
        let dir = scratch("silent");
        let (socket, listener) = bound(&dir);
        let kept = Arc::new(std::sync::Mutex::new(Vec::new()));
        let host_kept = Arc::clone(&kept);
        let server = serve_with(listener, admitting(&dir), SHORT, move |request, reply| {
            if request == Request::Paste {
                host_kept.lock().unwrap().push(reply);
            }
        })
        .unwrap();
        let answer = request(&socket, &Request::Paste, PATIENT).unwrap();
        assert!(
            matches!(answer, Answer::Failed(ref r) if r.contains("did not answer within 0.3 s")),
            "{answer:?}"
        );
        let answer = request(&socket, &Request::Copy(b"x".to_vec()), PATIENT).unwrap();
        assert!(
            matches!(answer, Answer::Failed(ref r) if r.contains("dropped")),
            "{answer:?}"
        );
        server.stop_and_join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg_attr(
        target_os = "macos",
        ignore = "the server admits a peer by its pid, which macOS does not give, so every request is refused there"
    )]
    fn a_copy_over_the_limit_is_refused_with_its_size() {
        let dir = scratch("big");
        let (socket, listener) = bound(&dir);
        let called = Arc::new(AtomicBool::new(false));
        let host_called = Arc::clone(&called);
        let server = serve_with(listener, admitting(&dir), SHORT, move |_, reply| {
            host_called.store(true, Ordering::Relaxed);
            reply.send(Answer::Done);
        })
        .unwrap();
        let mut stream = UnixStream::connect(&socket).unwrap();
        let mut head = vec![b'c'];
        head.extend_from_slice(&((MAX_TEXT_BYTES + 1) as u32).to_le_bytes());
        stream.write_all(&head).unwrap();
        stream.set_read_timeout(Some(PATIENT)).unwrap();
        let answer = read_answer(&mut stream).unwrap();
        assert!(
            matches!(answer, Answer::Failed(ref r) if r.contains("over the clipboard limit of 16 MiB")),
            "{answer:?}"
        );
        assert!(!called.load(Ordering::Relaxed), "the host never sees a refused request");
        server.stop_and_join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A connection that never sends its request is dropped at the read limit, and the next request
    /// is served; an unbounded read would hold the server until the client gave up a minute later.
    #[test]
    #[cfg_attr(
        target_os = "macos",
        ignore = "the server admits a peer by its pid, which macOS does not give, so every request is refused there"
    )]
    fn a_request_that_never_arrives_does_not_hold_the_server() {
        let dir = scratch("stall");
        let (socket, listener) = bound(&dir);
        let server = serve_with(listener, admitting(&dir), SHORT, |_, reply| reply.send(Answer::Done)).unwrap();
        let _stalled = UnixStream::connect(&socket).unwrap();
        let answer = request(&socket, &Request::Copy(b"x".to_vec()), PATIENT).unwrap();
        assert_eq!(answer, Answer::Done);
        server.stop_and_join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A peer that asks for a long paste and never reads the answer holds the server only until the
    /// answer's deadline: the next request is answered, where an unbounded write would hold it until
    /// the client gave up.
    #[test]
    #[cfg_attr(
        target_os = "macos",
        ignore = "the server admits a peer by its pid, which macOS does not give, so every request is refused there"
    )]
    fn a_peer_that_never_reads_its_answer_holds_the_server_only_until_the_deadline() {
        let dir = scratch("drain");
        let (socket, listener) = bound(&dir);
        let server = serve_with(listener, admitting(&dir), SHORT, |request, reply| match request {
            Request::Paste => reply.send(Answer::Text(vec![b'x'; MAX_TEXT_BYTES])),
            Request::Copy(_) => reply.send(Answer::Done),
        })
        .unwrap();
        let mut slow = UnixStream::connect(&socket).unwrap();
        write_request(&mut slow, &Request::Paste).unwrap();
        let answer = request(&socket, &Request::Copy(b"x".to_vec()), PATIENT).unwrap();
        assert_eq!(answer, Answer::Done);
        drop(slow);
        server.stop_and_join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A listener that never accepts, with its queue full: the client's own wait ends the connect,
    /// which a blocking `connect` would not. The outcome is the check; the watchdog only turns a hang
    /// into a failure.
    #[test]
    #[cfg_attr(
        target_os = "macos",
        ignore = "macOS takes a backlog of 0 as its maximum and refuses a connect to a full queue at once \
                  (ECONNREFUSED), so there is no full-queue wait to bound there"
    )]
    fn a_full_queue_is_bounded_by_the_clients_wait() {
        let dir = scratch("queue");
        let (socket, listener) = bound(&dir);
        // Linux takes a second `listen` as a new backlog: 0 leaves room for one pending connection.
        // SAFETY: a listening socket this test owns.
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
        let mut held = Vec::new();
        while held.len() < 16 {
            match crate::instance_dir::connect_by(&socket, Instant::now() + Duration::from_millis(50)) {
                Ok(stream) => held.push(stream),
                Err(_) => break,
            }
        }
        assert!(
            held.len() < 16,
            "the queue never filled: {} connections went in",
            held.len()
        );
        let (sent, got) = mpsc::channel();
        let to = socket.clone();
        std::thread::spawn(move || {
            let _ = sent.send(request(&to, &Request::Paste, Duration::from_millis(200)));
        });
        let got = got
            .recv_timeout(PATIENT)
            .expect("the client's connect must end by its own wait");
        assert!(matches!(got, Err(ClientError::TimedOut)), "{got:?}");
        drop(held);
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_listening_is_unreachable() {
        let dir = scratch("none");
        let got = request(&dir.join(SOCKET_NAME), &Request::Paste, CLIENT_WAIT);
        assert!(matches!(got, Err(ClientError::Unreachable(_))), "{got:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A peer of another uid is closed before anything is read or answered, and the host never hears
    /// of it. The test is that other uid by telling the server its own euid is someone else's.
    #[test]
    fn a_peer_of_another_uid_is_closed_unanswered() {
        let dir = scratch("uid");
        let (socket, listener) = bound(&dir);
        let mut admission = admitting(&dir);
        admission.euid = admission.euid.wrapping_add(1);
        let called = Arc::new(AtomicBool::new(false));
        let host_called = Arc::clone(&called);
        let server = serve_with(listener, admission, SHORT, move |_, reply| {
            host_called.store(true, Ordering::Relaxed);
            reply.send(Answer::Text(b"clip".to_vec()));
        })
        .unwrap();
        let got = request(&socket, &Request::Paste, PATIENT);
        assert!(matches!(got, Err(ClientError::Wire(_))), "{got:?}");
        assert!(!called.load(Ordering::Relaxed));
        server.stop_and_join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The real `/proc`: this test process is the server and connects to itself, so it is no child of
    /// its own and is refused with the reason; the host never hears of it.
    #[test]
    fn a_peer_that_is_not_the_windows_nvim_is_refused() {
        let dir = scratch("outsider");
        let (socket, listener) = bound(&dir);
        let called = Arc::new(AtomicBool::new(false));
        let host_called = Arc::clone(&called);
        let admission = Admission::for_window(Path::new(SWITCH));
        let server = serve_with(listener, admission, SHORT, move |_, reply| {
            host_called.store(true, Ordering::Relaxed);
            reply.send(Answer::Done);
        })
        .unwrap();
        for req in [Request::Paste, Request::Copy(b"x".to_vec())] {
            let got = request(&socket, &req, PATIENT).unwrap();
            assert_eq!(got, Answer::Failed(NOT_NVIM.to_string()));
        }
        assert!(!called.load(Ordering::Relaxed));
        server.stop_and_join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn descent_is_read_from_proc_and_fails_closed() {
        let dir = scratch("tree");
        let root = dir.join("proc");
        // server -> nvim (with the variable) -> sh -> shim
        fake_proc(&root, 10, FAKE_SERVER, Some(&nvim_environ()));
        fake_proc(&root, 11, 10, None);
        fake_proc(&root, 12, 11, None);
        // server -> terminal (no variable) -> bash
        fake_proc(&root, 20, FAKE_SERVER, Some(b"HOME=/h\0"));
        fake_proc(&root, 21, 20, None);
        // server -> a child naming another window's socket
        fake_proc(
            &root,
            30,
            FAKE_SERVER,
            Some(b"EITRI_PANE_SWITCH_SOCKET=/other/switch-socket\0"),
        );
        // a variable that only starts with the right value
        fake_proc(
            &root,
            31,
            FAKE_SERVER,
            Some(format!("EITRI_PANE_SWITCH_SOCKET={SWITCH}x\0").as_bytes()),
        );
        // outside the server altogether: under init
        fake_proc(&root, 40, 1, Some(&nvim_environ()));
        // nvim's environment unreadable
        fake_proc(&root, 50, FAKE_SERVER, None);
        fake_proc(&root, 51, 50, None);
        // a loop
        fake_proc(&root, 60, 61, None);
        fake_proc(&root, 61, 60, None);
        let a = fake_admission(&root);
        assert!(a.descends_from_nvim(10), "nvim itself");
        assert!(a.descends_from_nvim(12), "its grandchild");
        for (pid, why) in [
            (21, "the terminal's shell"),
            (30, "another window's nvim"),
            (31, "a value that only starts like ours"),
            (40, "a process under init"),
            (51, "nvim's environment unreadable"),
            (60, "a loop"),
            (99, "no such process"),
            (FAKE_SERVER, "the server itself"),
        ] {
            assert!(!a.descends_from_nvim(pid), "{why}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A person's nvim the window holds is no child of the window: it and what it starts are let in by its pid and
    /// start time, and a later process given that pid is not.
    #[test]
    fn the_attached_nvim_and_its_descendants_are_admitted_by_pid_and_start_time() {
        let dir = scratch("attached");
        let root = dir.join("proc");
        let full_stat = |pid: u32, parent: u32, start: u64| {
            let dir = root.join(pid.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            let fields: Vec<String> = (0..17).map(|i| i.to_string()).collect();
            std::fs::write(
                dir.join("stat"),
                format!("{pid} (n v) S {parent} {} {start} 0 0\n", fields.join(" ")),
            )
            .unwrap();
        };
        // terminal -> nvim (the person's, no window variable) -> sh -> shim
        full_stat(70, 7, 555);
        full_stat(71, 70, 600);
        full_stat(72, 71, 601);
        // the nvim's terminal, beside it
        full_stat(7, 1, 100);
        let plain = fake_admission(&root);
        assert!(
            !plain.descends_from_nvim(72),
            "without the attach, the person's nvim is no window's"
        );
        let a = fake_admission(&root).with_attached(AttachedNvim {
            pid: 70,
            start_time: 555,
        });
        assert!(a.descends_from_nvim(70), "the attached nvim");
        assert!(a.descends_from_nvim(72), "what it starts");
        assert!(!a.descends_from_nvim(7), "its terminal");
        let reused = fake_admission(&root).with_attached(AttachedNvim {
            pid: 70,
            start_time: 554,
        });
        assert!(!reused.descends_from_nvim(72), "another process given the same pid");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The server a followed `:restart` started is no child of the window once the nvim that started it exited: it is
    /// admitted by the pid and start time the editor learned, and only once it learned them.
    #[test]
    fn a_followed_servers_process_is_admitted_by_the_pid_the_editor_learned() {
        let dir = scratch("followed");
        let root = dir.join("proc");
        let full_stat = |pid: u32, parent: u32, start: u64| {
            let dir = root.join(pid.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            let fields: Vec<String> = (0..17).map(|i| i.to_string()).collect();
            std::fs::write(
                dir.join("stat"),
                format!("{pid} (nvim) S {parent} {} {start} 0 0\n", fields.join(" ")),
            )
            .unwrap();
        };
        // init -> the restarted server -> sh -> shim
        full_stat(80, 1, 900);
        full_stat(81, 80, 901);
        full_stat(82, 81, 902);
        let learned = std::sync::Arc::new(std::sync::Mutex::new(None::<AttachedNvim>));
        let a = {
            let learned = learned.clone();
            fake_admission(&root).with_followed(FollowedNvim::new(move || *learned.lock().unwrap()))
        };
        assert!(!a.descends_from_nvim(82), "a server the editor has not learned yet");
        *learned.lock().unwrap() = Some(AttachedNvim {
            pid: 80,
            start_time: 900,
        });
        assert!(a.descends_from_nvim(80), "the followed server");
        assert!(a.descends_from_nvim(82), "what it starts");
        *learned.lock().unwrap() = Some(AttachedNvim {
            pid: 80,
            start_time: 899,
        });
        assert!(!a.descends_from_nvim(82), "another process given the same pid");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No `/proc` at all (macOS, or one not mounted): nothing descends from anything.
    #[test]
    fn without_proc_nothing_is_admitted() {
        let mut a = Admission::for_window(Path::new(SWITCH));
        a.proc_root = std::path::PathBuf::from("/no-such-proc");
        assert!(!a.descends_from_nvim(std::process::id()));
    }

    /// The real `/proc`, real processes: a child started with the switch socket and that child's own
    /// child descend from it; the same child started without it does not.
    #[test]
    #[cfg_attr(
        target_os = "macos",
        ignore = "the descent walk reads parent links in /proc, which macOS does not have, so nothing descends there"
    )]
    fn real_children_descend_only_through_the_one_holding_the_switch_socket() {
        let start = |with_var: bool| {
            let mut sh = std::process::Command::new("sh");
            sh.args(["-c", "sleep 30 & echo $!; wait"])
                .env_remove("EITRI_PANE_SWITCH_SOCKET")
                .stdout(std::process::Stdio::piped());
            if with_var {
                sh.env("EITRI_PANE_SWITCH_SOCKET", SWITCH);
            }
            let mut child = sh.spawn().expect("sh");
            let mut line = String::new();
            std::io::BufRead::read_line(
                &mut std::io::BufReader::new(child.stdout.take().expect("stdout")),
                &mut line,
            )
            .expect("grandchild pid");
            let grandchild: u32 = line.trim().parse().expect("a pid");
            (child, grandchild)
        };
        let a = Admission::for_window(Path::new(SWITCH));
        let (mut nvim, under_nvim) = start(true);
        let (mut other, under_other) = start(false);
        let seen = [
            a.descends_from_nvim(nvim.id()),
            a.descends_from_nvim(under_nvim),
            a.descends_from_nvim(other.id()),
            a.descends_from_nvim(under_other),
        ];
        for pid in [under_nvim, under_other] {
            // SAFETY: a pid this test's own child started and printed; killed by that number only.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        }
        let _ = nvim.wait();
        let _ = other.wait();
        assert_eq!(seen, [true, true, false, false]);
    }
}
