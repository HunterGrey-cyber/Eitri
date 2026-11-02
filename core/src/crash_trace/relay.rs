//! A window's stdout and stderr, through pipes this process reads.
//!
//! `println!` panics when its write fails, and on the GTK thread, inside a GLib callback, that panic ends the process.
//! A log file on a full disk fails its writes, and so does a stdout the window was started with, whatever it is. A
//! pipe does not: so each stream that is not a terminal becomes a pipe, and two threads carry what arrives to where it
//! was going and into the log. A write those threads cannot make is dropped.
//!
//! A terminal is left alone: its output is not lost, and the hangup watch in the shell looks after it.
//!
//! One thread reads and never touches a disk, so a stalled disk or a stdout nobody reads cannot fill the pipes and
//! hold up a window's `println!`: past [`MAX_QUEUED`], what arrives is counted and dropped. The other writes. Both set their
//! stack size, which keeps them off the environment: the process sets variables after they start.

use std::fs::File;
use std::io::{self, PipeReader, PipeWriter, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::scrub;

/// A log's size before it moves to `<name>.1`, so one window keeps at most twice this.
pub const LOG_LIMIT: u64 = 2 * 1024 * 1024;
/// The suffix of the log a new one replaces.
pub const LOG_KEEP: &str = "1";
/// The output a crash record keeps.
const TAIL: usize = 16 * 1024;
const READ_SIZE: usize = 16 * 1024;
/// What may wait for the writer. Counted in bytes: a window that prints line by line makes a chunk of each line, and a
/// busy machine must not cost lines a slow writer would have got to.
const MAX_QUEUED: usize = 4 * 1024 * 1024;
const THREAD_STACK: usize = 256 * 1024;
/// How long after a failed reopen the log is tried again.
const REOPEN_AFTER: Duration = Duration::from_secs(5);
/// How long an exit waits for what is on its way out. Reached only when a reader of stdout has stopped reading.
const AT_EXIT: Duration = Duration::from_secs(1);

/// What happens to a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// It stays as it is.
    Leave,
    /// It goes through the relay, and on to where it went before when `tee`.
    Relay { tee: bool },
}

/// A terminal stays; anything else goes through the relay and on to where it went.
pub fn route_for(fd: RawFd) -> Route {
    // SAFETY: `isatty` reads a descriptor number.
    if unsafe { libc::isatty(fd) } == 1 {
        Route::Leave
    } else {
        Route::Relay { tee: true }
    }
}

/// Opens `<dir>/<name>` for a new run: the directory is made private, the previous run's log moves to `<name>.1`
/// (replacing an older one), and a new one is opened 0600, refusing a link in its place.
pub fn open_log(dir: &Path, name: &str) -> io::Result<(File, PathBuf)> {
    crate::state_paths::create_state_dir(dir)?;
    let path = dir.join(name);
    match std::fs::rename(&path, moved_aside(&path)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    Ok((agent::private_fs::open_private(&path)?, path))
}

fn moved_aside(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".");
    name.push(LOG_KEEP);
    PathBuf::from(name)
}

/// The log file: bounded, and a write it cannot make is counted, never an error.
struct LogFile {
    path: PathBuf,
    file: Option<File>,
    written: u64,
    limit: u64,
    lost: u64,
    next_try: Option<Instant>,
}

impl LogFile {
    fn new(file: File, path: PathBuf, limit: u64) -> LogFile {
        LogFile {
            path,
            file: Some(file),
            written: 0,
            limit,
            lost: 0,
            next_try: None,
        }
    }

    fn reopen(&mut self) {
        self.file = None;
        self.written = 0;
        match std::fs::rename(&self.path, moved_aside(&self.path)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {}
            _ => self.file = agent::private_fs::open_private(&self.path).ok(),
        }
        self.next_try = self.file.is_none().then(|| Instant::now() + REOPEN_AFTER);
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.write_all(bytes),
            None => Err(io::ErrorKind::NotFound.into()),
        }
    }

    fn write(&mut self, line: &[u8]) {
        let len = line.len() as u64;
        if self.written + len > self.limit || self.next_try.is_some_and(|at| Instant::now() >= at) {
            self.reopen();
        }
        if self.lost > 0 {
            let note = format!("[log] {} bytes could not be written here\n", self.lost);
            if self.append(note.as_bytes()).is_ok() {
                self.written += note.len() as u64;
                self.lost = 0;
            }
        }
        match self.append(line) {
            Ok(()) => self.written += len,
            Err(_) => self.lost += len,
        }
    }
}

/// The last of what the log was given, for a crash record: kept scrubbed, in a buffer made at the start.
struct Tail {
    bytes: Box<[u8]>,
    next: usize,
    wrapped: bool,
}

impl Tail {
    fn new() -> Tail {
        Tail {
            bytes: vec![0; TAIL].into_boxed_slice(),
            next: 0,
            wrapped: false,
        }
    }

    fn push(&mut self, mut line: &[u8]) {
        while !line.is_empty() {
            let take = line.len().min(self.bytes.len() - self.next);
            self.bytes[self.next..self.next + take].copy_from_slice(&line[..take]);
            line = &line[take..];
            self.next += take;
            if self.next == self.bytes.len() {
                self.next = 0;
                self.wrapped = true;
            }
        }
    }
}

/// One stream's text on its way to lines.
#[derive(Default)]
struct Lines {
    pending: Vec<u8>,
}

impl Lines {
    /// One byte past the longest line is kept, so [`scrub::line`] sees that it was cut.
    fn push(&mut self, mut bytes: &[u8], out: &mut impl FnMut(&[u8])) {
        while !bytes.is_empty() {
            let newline = bytes.iter().position(|byte| *byte == b'\n');
            let (part, rest) = match newline {
                Some(at) => (&bytes[..at], &bytes[at + 1..]),
                None => (bytes, &[][..]),
            };
            let room = (scrub::MAX_LINE + 1).saturating_sub(self.pending.len());
            self.pending.extend_from_slice(&part[..part.len().min(room)]);
            if newline.is_some() {
                self.finish(out);
            }
            bytes = rest;
        }
    }

    fn finish(&mut self, out: &mut impl FnMut(&[u8])) {
        out(&self.pending);
        self.pending.clear();
    }
}

enum Message {
    Output(usize, Vec<u8>),
    Flush(u64),
}

/// Where output ends up: the streams' old destinations and the log.
struct Sink {
    tee: [Option<File>; 2],
    lines: [Lines; 2],
    log: Option<LogFile>,
    home: Option<Vec<u8>>,
}

impl Sink {
    fn line(log: &mut Option<LogFile>, home: Option<&[u8]>, shared: &Shared, raw: &[u8]) {
        let Some(mut line) = scrub::line(raw, home) else {
            return;
        };
        line.push(b'\n');
        if let Some(log) = log {
            log.write(&line);
        }
        if let Ok(mut tail) = shared.tail.lock() {
            tail.push(&line);
        }
    }

    fn take(&mut self, stream: usize, bytes: &[u8], shared: &Shared) {
        if let Some(tee) = &mut self.tee[stream] {
            let _ = tee.write_all(bytes);
        }
        let (log, home) = (&mut self.log, self.home.as_deref());
        self.lines[stream].push(bytes, &mut |raw| Sink::line(log, home, shared, raw));
    }

    /// A log named after the relay started begins with what the relay has kept so far.
    fn take_new_log(&mut self, shared: &Shared) {
        let Some(mut log) = shared.new_log.lock().ok().and_then(|mut new| new.take()) else {
            return;
        };
        if let Ok(tail) = shared.tail.lock() {
            if tail.wrapped {
                log.write(&tail.bytes[tail.next..]);
            }
            log.write(&tail.bytes[..tail.next]);
        }
        self.log = Some(log);
    }

    /// What arrived without its newline, and what could not wait to be written.
    fn settle(&mut self, shared: &Shared) {
        let dropped = shared.dropped.swap(0, Ordering::Relaxed);
        for lines in &mut self.lines {
            if !lines.pending.is_empty() {
                let (log, home) = (&mut self.log, self.home.as_deref());
                lines.finish(&mut |raw| Sink::line(log, home, shared, raw));
            }
        }
        if dropped > 0 {
            let note =
                format!("[log] {dropped} bytes of output were dropped: they came faster than they could be written");
            Sink::line(&mut self.log, None, shared, note.as_bytes());
        }
    }
}

struct Shared {
    /// A log that arrived after the relay started, for the writer to take.
    new_log: Mutex<Option<LogFile>>,
    wake: File,
    asked: AtomicU64,
    done: AtomicU64,
    dropped: AtomicU64,
    queued: AtomicUsize,
    tail: Mutex<Tail>,
    /// Where a panic's own words go when fd 2 is the relay's. They can quote what the program was handling, so they
    /// never reach the log; the crash record says what may be said.
    panic_stderr: Option<File>,
    /// Set once the threads run and the streams have moved.
    started: AtomicBool,
}

static RELAY: OnceLock<Shared> = OnceLock::new();

/// How the relay starts.
pub struct Start {
    /// A log, and the first line to write into it.
    pub log: Option<(File, PathBuf, String)>,
    pub stdout: Route,
    pub stderr: Route,
    pub home: Option<Vec<u8>>,
}

fn duplicate(fd: RawFd) -> Option<OwnedFd> {
    // SAFETY: the descriptor is only borrowed for this call; a closed one makes the clone fail.
    unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned().ok()
}

/// Moves each stream `start` routes through the relay. `Ok(false)` when none is, or when the relay already runs. A
/// failure leaves the streams as they were.
pub fn start(start: Start) -> io::Result<bool> {
    let routes = [(1, 0, start.stdout), (2, 1, start.stderr)];
    let mut readers: Vec<(usize, PipeReader)> = Vec::new();
    let mut writers: Vec<(RawFd, PipeWriter)> = Vec::new();
    let mut tee = [None, None];
    let mut panic_stderr = None;
    for (fd, index, route) in routes {
        let Route::Relay { tee: tees } = route else { continue };
        let (read, write) = io::pipe()?;
        if tees {
            tee[index] = duplicate(fd).map(File::from);
        }
        if fd == 2 {
            panic_stderr = Some(match &tee[index] {
                Some(file) => file.try_clone()?,
                None => File::options().write(true).open("/dev/null")?,
            });
        }
        readers.push((index, read));
        writers.push((fd, write));
    }
    if readers.is_empty() {
        return Ok(false);
    }
    let (wake_read, wake_write) = io::pipe()?;
    let (sender, receiver) = mpsc::channel();
    let shared = Shared {
        new_log: Mutex::new(None),
        wake: File::from(OwnedFd::from(wake_write)),
        asked: AtomicU64::new(0),
        done: AtomicU64::new(0),
        dropped: AtomicU64::new(0),
        queued: AtomicUsize::new(0),
        tail: Mutex::new(Tail::new()),
        panic_stderr,
        started: AtomicBool::new(false),
    };
    if RELAY.set(shared).is_err() {
        return Ok(false);
    }
    let Some(shared) = RELAY.get() else { return Ok(false) };
    let log = start.log.map(|(file, path, header)| {
        let mut log = LogFile::new(file, path, LOG_LIMIT);
        log.write(format!("{header}\n").as_bytes());
        log
    });
    let sink = Sink {
        tee,
        lines: Default::default(),
        log,
        home: start.home,
    };
    std::thread::Builder::new()
        .name("eitri-log-write".to_string())
        .stack_size(THREAD_STACK)
        .spawn(move || write_loop(receiver, sink, shared))?;
    std::thread::Builder::new()
        .name("eitri-log-read".to_string())
        .stack_size(THREAD_STACK)
        .spawn(move || read_loop(readers, wake_read, sender, shared))?;
    for (fd, write) in &writers {
        // SAFETY: `dup2` replaces the standard descriptor atomically, so a writer on another thread sees the old
        // destination or the new one; both descriptors are open.
        unsafe { libc::dup2(write.as_raw_fd(), *fd) };
    }
    shared.started.store(true, Ordering::Release);
    // SAFETY: `at_exit` takes no arguments and unwinds nothing.
    unsafe { libc::atexit(at_exit) };
    Ok(true)
}

extern "C" fn at_exit() {
    flush(AT_EXIT);
}

fn write_loop(receiver: Receiver<Message>, mut sink: Sink, shared: &'static Shared) {
    for message in receiver {
        match message {
            Message::Output(stream, bytes) => {
                sink.take(stream, &bytes, shared);
                shared.queued.fetch_sub(bytes.len(), Ordering::AcqRel);
            }
            Message::Flush(asked) => {
                sink.take_new_log(shared);
                sink.settle(shared);
                shared.done.fetch_max(asked, Ordering::Release);
            }
        }
    }
}

fn pollfd(fd: RawFd) -> libc::pollfd {
    libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }
}

/// Reads what is waiting on `readers` once each, and hands it to the writer or counts it.
fn forward(
    readers: &mut [(usize, PipeReader)],
    ready: &mut [libc::pollfd],
    buffer: &mut [u8],
    sender: &Sender<Message>,
    shared: &Shared,
) -> bool {
    let mut any = false;
    for ((stream, reader), poll) in readers.iter_mut().zip(ready) {
        if poll.revents == 0 {
            continue;
        }
        let Ok(read) = reader.read(buffer) else { continue };
        if read == 0 {
            // Every write end is gone: left in the set, it would make the poll return at once for good.
            poll.fd = -1;
            continue;
        }
        any = true;
        // Counted before it is sent: the writer takes it off the moment it arrives.
        let room = shared.queued.fetch_add(read, Ordering::AcqRel) + read <= MAX_QUEUED;
        if !room || sender.send(Message::Output(*stream, buffer[..read].to_vec())).is_err() {
            shared.queued.fetch_sub(read, Ordering::AcqRel);
            shared.dropped.fetch_add(read as u64, Ordering::Relaxed);
        }
    }
    any
}

fn poll_all(fds: &mut [libc::pollfd], timeout: i32) -> i32 {
    // SAFETY: `fds` is a live slice of pollfds and its length is what is passed.
    unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) }
}

fn read_loop(
    mut readers: Vec<(usize, PipeReader)>,
    mut wake: PipeReader,
    sender: Sender<Message>,
    shared: &'static Shared,
) {
    let mut buffer = vec![0; READ_SIZE];
    let mut fds: Vec<libc::pollfd> = readers.iter().map(|(_, reader)| pollfd(reader.as_raw_fd())).collect();
    fds.push(pollfd(wake.as_raw_fd()));
    let wake_at = fds.len() - 1;
    loop {
        for fd in &mut fds {
            fd.revents = 0;
        }
        if poll_all(&mut fds, -1) < 0 {
            // This thread has no way out: a window's writes would fill the pipes and stop. `ENOMEM` passes.
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                std::thread::sleep(Duration::from_millis(10));
            }
            continue;
        }
        let woken = fds[wake_at].revents != 0;
        forward(&mut readers, &mut fds[..wake_at], &mut buffer, &sender, shared);
        if !woken {
            continue;
        }
        let mut asked = [0; 64];
        let _ = wake.read(&mut asked);
        // Whatever was written before the flush was asked for is in the pipes now, and one read takes what a pipe
        // holds up to the buffer's size, so this ends; the bound is for a writer that never stops.
        for _ in 0..64 {
            for fd in &mut fds {
                fd.revents = 0;
            }
            if poll_all(&mut fds[..wake_at], 0) <= 0
                || !forward(&mut readers, &mut fds[..wake_at], &mut buffer, &sender, shared)
            {
                break;
            }
        }
        let _ = sender.send(Message::Flush(shared.asked.load(Ordering::Acquire)));
    }
}

/// Whether the relay carries a stream.
pub fn running() -> bool {
    started().is_some()
}

fn started() -> Option<&'static Shared> {
    RELAY.get().filter(|shared| shared.started.load(Ordering::Acquire))
}

/// Gives the running relay its log, which starts with `header` and what was printed so far. Before the project is
/// known a window has output and no log's name.
pub fn attach_log(file: File, path: PathBuf, header: &str) {
    let Some(shared) = started() else { return };
    let mut log = LogFile::new(file, path, LOG_LIMIT);
    log.write(format!("{header}\n").as_bytes());
    if let Ok(mut new) = shared.new_log.lock() {
        *new = Some(log);
    }
    flush(AT_EXIT);
}

/// Waits, at most `wait`, for what was written so far to reach the log. Nothing happens when no relay runs. Allocates
/// nothing, so a panic hook may call it.
pub fn flush(wait: Duration) {
    let Some(shared) = started() else { return };
    let asked = shared.asked.fetch_add(1, Ordering::AcqRel) + 1;
    let _ = (&shared.wake).write(&[1]);
    let end = Instant::now() + wait;
    while shared.done.load(Ordering::Acquire) < asked && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Where stderr went before the relay, when the relay carries it. fd 2 itself is left alone: a print on another thread
/// must never meet a stream that can fail.
pub(crate) fn stderr_outside_the_log() -> Option<&'static File> {
    started().and_then(|shared| shared.panic_stderr.as_ref())
}

/// Calls `out` with the kept output, oldest first; twice when it has wrapped. Not at all when the lock stays held: this
/// runs in a panic hook, and the output is not worth waiting for.
pub(crate) fn tail(mut out: impl FnMut(&[u8])) {
    let Some(shared) = started() else { return };
    let Ok(tail) = shared.tail.try_lock() else { return };
    if tail.wrapped {
        out(&tail.bytes[tail.next..]);
    }
    out(&tail.bytes[..tail.next]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(label: &str) -> crate::test_scratch_dir::ScratchDir {
        crate::test_scratch_dir::ScratchDir::new("crash-trace-relay", label)
    }

    fn lines_of(chunks: &[&[u8]]) -> Vec<Vec<u8>> {
        let mut seen = Vec::new();
        let mut lines = Lines::default();
        for chunk in chunks {
            lines.push(chunk, &mut |line| seen.push(line.to_vec()));
        }
        seen
    }

    #[test]
    fn lines_come_whole_however_the_bytes_were_cut() {
        assert_eq!(
            lines_of(&[b"on", b"e\ntw", b"o\n", b"three"]),
            [b"one".to_vec(), b"two".to_vec()]
        );
    }

    #[test]
    fn a_line_past_the_longest_stays_bounded_and_the_next_one_is_whole() {
        let long = vec![b'x'; scrub::MAX_LINE * 5];
        let seen = lines_of(&[&long, b"\nnext\n"]);
        assert_eq!(seen[0].len(), scrub::MAX_LINE + 1);
        assert_eq!(seen[1], b"next");
        assert!(scrub::line(&seen[0], None).unwrap().ends_with(b" [cut]"));
    }

    #[test]
    fn the_tail_keeps_the_newest_bytes_in_order() {
        let mut tail = Tail::new();
        tail.push(&vec![b'o'; TAIL - 2]);
        tail.push(b"newest");
        assert!(tail.wrapped);
        let mut kept = tail.bytes[tail.next..].to_vec();
        kept.extend_from_slice(&tail.bytes[..tail.next]);
        assert_eq!(kept.len(), TAIL);
        assert!(kept.ends_with(b"newest"));
    }

    #[test]
    fn a_log_moves_aside_at_its_limit_and_stays_private() {
        let dir = scratch("rotate");
        let (file, path) = open_log(&dir, "main.log").unwrap();
        let mut log = LogFile::new(file, path.clone(), 100);
        for _ in 0..50 {
            log.write(b"0123456789\n");
        }
        let old = moved_aside(&path);
        assert!(std::fs::metadata(&path).unwrap().len() <= 100);
        assert!(std::fs::metadata(&old).unwrap().len() <= 100);
        for file in [&path, &old] {
            assert_eq!(std::fs::metadata(file).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn a_new_run_moves_the_last_runs_log_aside() {
        let dir = scratch("runs");
        let (mut first, path) = open_log(&dir, "main.log").unwrap();
        first.write_all(b"the last run\n").unwrap();
        let _second = open_log(&dir, "main.log").unwrap();
        assert_eq!(std::fs::read_to_string(moved_aside(&path)).unwrap(), "the last run\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    }

    #[test]
    fn a_write_the_disk_refuses_is_dropped_and_counted() {
        let full = File::options().write(true).open("/dev/full").unwrap();
        let dir = scratch("full");
        let mut log = LogFile::new(full, dir.join("main.log"), 100);
        log.write(b"lost\n");
        assert_eq!(log.lost, 5);
        assert_eq!(log.written, 0);
    }

    #[test]
    fn a_log_that_cannot_be_reopened_drops_its_lines_and_tries_again_later() {
        let dir = scratch("gone");
        let (file, path) = open_log(&dir, "main.log").unwrap();
        let mut log = LogFile::new(file, path.clone(), 10);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::create_dir(moved_aside(&path)).unwrap();
        std::fs::write(moved_aside(&path).join("in the way"), b"").unwrap();
        log.write(b"past the limit of ten bytes\n");
        assert!(log.file.is_none());
        assert_eq!(log.lost, 28);
        assert!(log.next_try.is_some());
    }

    #[test]
    fn only_a_terminal_stays() {
        let file = File::open("/dev/null").unwrap();
        assert_eq!(route_for(file.as_raw_fd()), Route::Relay { tee: true });
    }
}
