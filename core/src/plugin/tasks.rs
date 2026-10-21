//! Running a plugin's declared tasks. Each run leads a process group of its own; its output is read on
//! threads, cut into lines, capped, and handed to the GTK side in batches when the host's wake says
//! there is something to take. Nothing here blocks the caller, and a runner with nothing running has no
//! thread, no timer and raises no wake.
//!
//! A stop (cancel, timeout, plugin stop, window close) sends SIGTERM to the group and SIGKILL two seconds
//! later. Two seconds is longer than the terminal's half second because the plugin's own CLI stops its
//! child and exits within one.

use std::collections::{BTreeMap, VecDeque};
use std::io::{PipeReader, PipeWriter, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const GRACE: Duration = Duration::from_secs(2);
pub const BATCH_EVERY: Duration = Duration::from_millis(50);
pub const BATCH_BYTES: usize = 64 * 1024;
pub const MAX_LINE_BYTES: usize = 1024 * 1024;
pub const RATE_WINDOW: Duration = Duration::from_secs(10);
pub const RATE_BYTES: usize = 4 * 1024 * 1024;
pub const TAIL_BYTES: usize = 1024 * 1024;
pub const MAX_RUNS: usize = 3;

/// What a held line costs besides its bytes (its `String` header and queue slot), charged to the rate window,
/// the batch size and the tail, so a flood of empty lines is bounded like any other output.
pub const LINE_OVERHEAD: usize = 32;

/// The granularity the rate window keeps its history in, so its memory is bounded whatever the line rate.
const RATE_BUCKET: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub task: String,
    pub vars: BTreeMap<String, String>,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitKind {
    Exited(i32),
    Killed(i32),
    Timeout,
    Cancelled,
    SpawnFailed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskEvent {
    /// The counters are what was dropped since this run's previous `Output`.
    Output {
        run: u64,
        stream: Stream,
        lines: Vec<String>,
        dropped_lines: u64,
        dropped_bytes: u64,
    },
    /// Always the run's last event; the counters are its totals.
    Exit {
        run: u64,
        kind: ExitKind,
        dropped_lines: u64,
        dropped_bytes: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunInfo {
    pub run: u64,
    pub task: String,
    pub vars: BTreeMap<String, String>,
    pub tail: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError {
    TaskBusy(String),
    TooManyRuns,
}

/// A budget of bytes over a sliding window. Pure: the caller passes the time.
#[derive(Debug)]
pub struct RateWindow {
    window: Duration,
    budget: usize,
    spent: VecDeque<(Instant, usize)>,
    total: usize,
}

impl RateWindow {
    pub fn new(window: Duration, budget: usize) -> RateWindow {
        RateWindow {
            window,
            budget,
            spent: VecDeque::new(),
            total: 0,
        }
    }

    pub fn admit(&mut self, now: Instant, bytes: usize) -> bool {
        while let Some(&(at, n)) = self.spent.front() {
            if now.saturating_duration_since(at) < self.window {
                break;
            }
            self.spent.pop_front();
            self.total -= n;
        }
        if self.total + bytes > self.budget {
            return false;
        }
        self.total += bytes;
        match self.spent.back_mut() {
            Some((at, n)) if now.saturating_duration_since(*at) < RATE_BUCKET => *n += bytes,
            _ => self.spent.push_back((now, bytes)),
        }
        true
    }
}

/// A run's last stdout lines: at most `TAIL_BYTES` counting each line's overhead, but always the last line.
#[derive(Debug, Default)]
pub struct Tail {
    lines: VecDeque<String>,
    bytes: usize,
}

impl Tail {
    pub fn push(&mut self, line: String) {
        self.bytes += line.len() + LINE_OVERHEAD;
        self.lines.push_back(line);
        while self.bytes > TAIL_BYTES && self.lines.len() > 1 {
            if let Some(old) = self.lines.pop_front() {
                self.bytes -= old.len() + LINE_OVERHEAD;
            }
        }
    }

    pub fn lines(&self) -> Vec<String> {
        self.lines.iter().cloned().collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cut {
    Line(String),
    /// A line over `MAX_LINE_BYTES`, of this many bytes, never held whole.
    Dropped(usize),
}

/// Cuts a byte stream into lines without holding more than one line's cap of it. A trailing `\r` is
/// dropped; bytes that are not UTF-8 are replaced.
#[derive(Debug, Default)]
pub struct LineCutter {
    current: Vec<u8>,
    overflow: Option<usize>,
}

impl LineCutter {
    pub fn feed(&mut self, chunk: &[u8], out: &mut Vec<Cut>) {
        let mut rest = chunk;
        while !rest.is_empty() {
            match rest.iter().position(|&b| b == b'\n') {
                Some(nl) => {
                    self.take(&rest[..nl]);
                    self.finish(out);
                    rest = &rest[nl + 1..];
                }
                None => {
                    self.take(rest);
                    rest = &[];
                }
            }
        }
    }

    /// The stream ended: a last line without a newline still counts.
    pub fn end(&mut self, out: &mut Vec<Cut>) {
        if self.overflow.is_some() || !self.current.is_empty() {
            self.finish(out);
        }
    }

    fn take(&mut self, bytes: &[u8]) {
        if let Some(n) = self.overflow.as_mut() {
            *n += bytes.len();
        } else if self.current.len() + bytes.len() > MAX_LINE_BYTES {
            self.overflow = Some(self.current.len() + bytes.len());
            self.current = Vec::new();
        } else {
            self.current.extend_from_slice(bytes);
        }
    }

    fn finish(&mut self, out: &mut Vec<Cut>) {
        if let Some(n) = self.overflow.take() {
            out.push(Cut::Dropped(n));
            return;
        }
        let mut line = std::mem::take(&mut self.current);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        out.push(Cut::Line(String::from_utf8_lossy(&line).into_owned()));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason {
    Cancelled,
    Timeout,
}

/// What a run's threads hand to the GTK side.
struct Out {
    lines: VecDeque<(Stream, String)>,
    bytes: usize,
    /// When the oldest line still held arrived (or a drop was first counted): the batch is due 50 ms later.
    since: Option<Instant>,
    dropped_lines: u64,
    dropped_bytes: u64,
    reported_lines: u64,
    reported_bytes: u64,
    tail: Tail,
    rate: RateWindow,
    exit: Option<ExitKind>,
    /// Set with `exit`: a writer that outlives the run (one that left the group with the pipes open) has
    /// its lines discarded, since nothing will take them.
    closed: bool,
}

struct Shared {
    out: Mutex<Out>,
    done: Mutex<bool>,
    done_changed: Condvar,
    /// Why and when a stop was asked for.
    stop: Mutex<Option<(StopReason, Instant)>>,
    /// Whether the run's process group may still be signalled. Cleared once the group is known to be gone,
    /// so a late signal cannot reach a process group that reused the number.
    group_live: Mutex<bool>,
    /// Readable once the run has ended, to release readers blocked on a pipe that an outside process holds open.
    /// A plain pipe rather than an eventfd, which Apple targets do not have.
    closer: Option<PipeReader>,
    /// The closer's write end, taken by the first `finish`: the signal is one byte, written once.
    closer_write: Mutex<Option<PipeWriter>>,
}

impl Shared {
    fn new() -> Arc<Shared> {
        let (closer, closer_write) = match std::io::pipe() {
            Ok((r, w)) => (Some(r), Some(w)),
            Err(_) => (None, None),
        };
        Arc::new(Shared {
            out: Mutex::new(Out {
                lines: VecDeque::new(),
                bytes: 0,
                since: None,
                dropped_lines: 0,
                dropped_bytes: 0,
                reported_lines: 0,
                reported_bytes: 0,
                tail: Tail::default(),
                rate: RateWindow::new(RATE_WINDOW, RATE_BYTES),
                exit: None,
                closed: false,
            }),
            done: Mutex::new(false),
            done_changed: Condvar::new(),
            stop: Mutex::new(None),
            group_live: Mutex::new(true),
            closer,
            closer_write: Mutex::new(closer_write),
        })
    }

    fn finish(&self, kind: ExitKind) {
        {
            let mut out = lock(&self.out);
            out.exit = Some(kind);
            out.closed = true;
        }
        if let Some(mut writer) = lock(&self.closer_write).take() {
            // A failed write only leaves the readers to their EOF; dropping the end afterwards also hangs the
            // pipe up, so a reader that polls later still sees it.
            let _ = writer.write_all(&[1]);
        }
        *lock(&self.done) = true;
        self.done_changed.notify_all();
    }

    /// Signals the run's group while it may still exist.
    fn signal(&self, pgid: u32, signal: libc::c_int) {
        let live = lock(&self.group_live);
        if *live {
            signal_group(pgid, signal);
        }
    }

    fn group_gone(&self) {
        *lock(&self.group_live) = false;
    }

    /// Waits up to `limit` for the run to end; whether it did.
    fn wait_done(&self, limit: Duration) -> bool {
        let done = lock(&self.done);
        let (done, _) = self
            .done_changed
            .wait_timeout_while(done, limit, |d| !*d)
            .unwrap_or_else(|e| e.into_inner());
        *done
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Signals the group led by `pgid`. The group exists from the moment `spawn` returns (the child made it
/// before exec, or the spawn failed), so there is no fallback to the process alone.
fn signal_group(pgid: u32, signal: libc::c_int) {
    // SAFETY: `kill` takes plain integers; a group that is gone is ESRCH and nothing happens.
    unsafe {
        libc::kill(-(pgid as libc::pid_t), signal);
    }
}

fn group_alive(pgid: u32) -> bool {
    // SAFETY: signal 0 only asks whether the group exists.
    unsafe { libc::kill(-(pgid as libc::pid_t), 0) == 0 }
}

fn spawn(launch: &Launch) -> std::io::Result<Child> {
    let mut command = Command::new(&launch.program);
    command
        .args(&launch.args)
        .current_dir(&launch.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: `getpid` has no preconditions.
    #[cfg(target_os = "linux")]
    let parent = unsafe { libc::getpid() };
    // SAFETY: only async-signal-safe calls run between fork and exec.
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // The child is told when the thread that started it ends. Runs are started on the window's main
            // thread, which lives as long as the window, so an Eitri that dies takes its tasks with it.
            #[cfg(target_os = "linux")]
            {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM as libc::c_ulong) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // Eitri may have died before the line above took effect, and then nothing would signal this.
                if libc::getppid() != parent {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
            }
            Ok(())
        });
    }
    command.spawn()
}

fn deliver(shared: &Shared, stream: Stream, cuts: &mut Vec<Cut>, wake: &agent::UiWake) {
    let now = Instant::now();
    let mut raise = false;
    {
        let mut out = lock(&shared.out);
        if out.closed {
            cuts.clear();
            return;
        }
        for cut in cuts.drain(..) {
            match cut {
                Cut::Dropped(n) => {
                    out.dropped_lines += 1;
                    out.dropped_bytes += n as u64;
                    if out.since.is_none() {
                        out.since = Some(now);
                        raise = true;
                    }
                }
                Cut::Line(line) => {
                    if !out.rate.admit(now, line.len() + LINE_OVERHEAD) {
                        out.dropped_lines += 1;
                        out.dropped_bytes += line.len() as u64;
                        if out.since.is_none() {
                            out.since = Some(now);
                            raise = true;
                        }
                        continue;
                    }
                    if stream == Stream::Stdout {
                        out.tail.push(line.clone());
                    }
                    if out.since.is_none() {
                        out.since = Some(now);
                        raise = true;
                    }
                    out.bytes += line.len() + LINE_OVERHEAD;
                    out.lines.push_back((stream, line));
                    if out.bytes >= BATCH_BYTES {
                        raise = true;
                    }
                }
            }
        }
    }
    if raise {
        wake();
    }
}

/// Waits until `pipe` has something to read (or hung up); false once the run has ended and the reader should
/// stop, whatever is still holding the pipe's other end.
fn wait_readable(pipe: RawFd, closer: Option<RawFd>) -> bool {
    let Some(closer) = closer else { return true };
    let mut fds = [
        libc::pollfd {
            fd: pipe,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: closer,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        // SAFETY: `fds` is a valid array of two `pollfd`.
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if r < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return true;
        }
        return fds[1].revents == 0;
    }
}

fn read_stream(mut pipe: impl Read + AsRawFd, stream: Stream, shared: Arc<Shared>, wake: agent::UiWake) {
    let mut cutter = LineCutter::default();
    let mut buf = vec![0u8; 64 * 1024];
    let mut cuts = Vec::new();
    let closer = shared.closer.as_ref().map(AsRawFd::as_raw_fd);
    loop {
        if !wait_readable(pipe.as_raw_fd(), closer) {
            return;
        }
        let n = match pipe.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        cutter.feed(&buf[..n], &mut cuts);
        if !cuts.is_empty() {
            deliver(&shared, stream, &mut cuts, &wake);
        }
    }
    cutter.end(&mut cuts);
    if !cuts.is_empty() {
        deliver(&shared, stream, &mut cuts, &wake);
    }
}

fn wait_run(mut child: Child, readers: Vec<JoinHandle<()>>, shared: Arc<Shared>, wake: agent::UiWake) {
    let pgid = child.id();
    let status = child.wait();
    // A stop gives the whole group its grace, not just the leader: a leader that exits on SIGTERM at once
    // must not cut short a child that is cleaning up.
    let deadline = lock(&shared.stop).map(|(_, at)| at + GRACE);
    if let Some(deadline) = deadline {
        while group_alive(pgid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    // Whatever is left of the group is an orphan of a run that has ended.
    if group_alive(pgid) {
        shared.signal(pgid, libc::SIGKILL);
    }
    shared.group_gone();
    // The readers end when every writer of the pipes is gone; one outside the group that kept them is not
    // waited for beyond the grace.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for reader in readers {
            let _ = reader.join();
        }
        let _ = tx.send(());
    });
    if rx.recv_timeout(GRACE).is_err() {
        eprintln!("[plugin] a task's output is still held open by a process outside its group; not waiting for it");
    }
    let stop = lock(&shared.stop).map(|(reason, _)| reason);
    let kind = match (stop, status) {
        (Some(StopReason::Cancelled), _) => ExitKind::Cancelled,
        (Some(StopReason::Timeout), _) => ExitKind::Timeout,
        (None, Ok(status)) => match (status.code(), status.signal()) {
            (Some(code), _) => ExitKind::Exited(code),
            (None, Some(signal)) => ExitKind::Killed(signal),
            (None, None) => ExitKind::Killed(0),
        },
        (None, Err(e)) => ExitKind::SpawnFailed(e.to_string()),
    };
    shared.finish(kind);
    wake();
}

fn request_stop(shared: &Arc<Shared>, pgid: u32, reason: StopReason) {
    {
        let mut stop = lock(&shared.stop);
        if stop.is_some() || *lock(&shared.done) {
            return;
        }
        *stop = Some((reason, Instant::now()));
    }
    shared.signal(pgid, libc::SIGTERM);
    let shared = shared.clone();
    std::thread::spawn(move || {
        if !shared.wait_done(GRACE) {
            shared.signal(pgid, libc::SIGKILL);
        }
    });
}

struct Run {
    id: u64,
    task: String,
    vars: BTreeMap<String, String>,
    pgid: Option<u32>,
    shared: Arc<Shared>,
}

pub struct TaskRunner {
    wake: agent::UiWake,
    next: u64,
    runs: Vec<Run>,
}

/// Whether every run that existed when it was taken has ended.
pub struct RunsWaiter(Vec<Arc<Shared>>);

impl RunsWaiter {
    pub fn finished(&self) -> bool {
        self.0.iter().all(|s| *lock(&s.done))
    }
}

impl TaskRunner {
    pub fn new(wake: agent::UiWake) -> TaskRunner {
        TaskRunner {
            wake,
            next: 1,
            runs: Vec::new(),
        }
    }

    fn admit(&self, task: &str) -> Result<u64, StartError> {
        if self.runs.iter().any(|r| r.task == task) {
            return Err(StartError::TaskBusy(task.to_owned()));
        }
        if self.runs.len() >= MAX_RUNS {
            return Err(StartError::TooManyRuns);
        }
        Ok(self.next)
    }

    pub fn start(&mut self, launch: Launch) -> Result<u64, StartError> {
        let id = self.admit(&launch.task)?;
        self.next += 1;
        let shared = Shared::new();
        let pgid = match spawn(&launch) {
            Err(e) => {
                shared.finish(ExitKind::SpawnFailed(format!("{}: {e}", launch.program.display())));
                (self.wake)();
                None
            }
            Ok(mut child) => {
                let pgid = child.id();
                let mut readers = Vec::new();
                if let Some(pipe) = child.stdout.take() {
                    let (s, w) = (shared.clone(), self.wake.clone());
                    readers.push(std::thread::spawn(move || read_stream(pipe, Stream::Stdout, s, w)));
                }
                if let Some(pipe) = child.stderr.take() {
                    let (s, w) = (shared.clone(), self.wake.clone());
                    readers.push(std::thread::spawn(move || read_stream(pipe, Stream::Stderr, s, w)));
                }
                {
                    let (s, w) = (shared.clone(), self.wake.clone());
                    std::thread::spawn(move || wait_run(child, readers, s, w));
                }
                if let Some(limit) = launch.timeout {
                    let s = shared.clone();
                    std::thread::spawn(move || {
                        if !s.wait_done(limit) {
                            request_stop(&s, pgid, StopReason::Timeout);
                        }
                    });
                }
                Some(pgid)
            }
        };
        self.runs.push(Run {
            id,
            task: launch.task,
            vars: launch.vars,
            pgid,
            shared,
        });
        Ok(id)
    }

    /// A run that ends at once as failed to spawn, for a program that is not built.
    pub fn start_failed(
        &mut self,
        task: &str,
        vars: &BTreeMap<String, String>,
        message: String,
    ) -> Result<u64, StartError> {
        let id = self.admit(task)?;
        self.next += 1;
        let shared = Shared::new();
        shared.finish(ExitKind::SpawnFailed(message));
        self.runs.push(Run {
            id,
            task: task.to_owned(),
            vars: vars.clone(),
            pgid: None,
            shared,
        });
        (self.wake)();
        Ok(id)
    }

    /// Whether `run` is one this runner still reports on.
    pub fn cancel(&mut self, run: u64) -> bool {
        let Some(r) = self.runs.iter().find(|r| r.id == run) else {
            return false;
        };
        if let Some(pgid) = r.pgid {
            request_stop(&r.shared, pgid, StopReason::Cancelled);
        }
        true
    }

    pub fn stop_all(&mut self) {
        for r in &self.runs {
            if let Some(pgid) = r.pgid {
                request_stop(&r.shared, pgid, StopReason::Cancelled);
            }
        }
    }

    /// Takes what is ready. A batch is ready 50 ms after its first line, at 64 KiB, or when its run has
    /// ended; the second value is when the next held batch will be, for a one-shot timer.
    pub fn poll(&mut self, now: Instant) -> (Vec<TaskEvent>, Option<Instant>) {
        let mut events = Vec::new();
        let mut next: Option<Instant> = None;
        let mut ended = Vec::new();
        for run in &self.runs {
            let mut out = lock(&run.shared.out);
            let waiting = !out.lines.is_empty() || out.dropped_lines != out.reported_lines;
            let due = out.exit.is_some()
                || out.bytes >= BATCH_BYTES
                || out.since.is_some_and(|since| now >= since + BATCH_EVERY);
            if waiting && !due {
                if let Some(since) = out.since {
                    let at = since + BATCH_EVERY;
                    next = Some(next.map_or(at, |n| n.min(at)));
                }
                continue;
            }
            if waiting {
                drain(&mut out, run.id, &mut events);
            }
            if let Some(kind) = out.exit.take() {
                events.push(TaskEvent::Exit {
                    run: run.id,
                    kind,
                    dropped_lines: out.dropped_lines,
                    dropped_bytes: out.dropped_bytes,
                });
                ended.push(run.id);
            }
        }
        self.runs.retain(|r| !ended.contains(&r.id));
        (events, next)
    }

    pub fn runs(&self) -> Vec<RunInfo> {
        self.runs
            .iter()
            .map(|r| RunInfo {
                run: r.id,
                task: r.task.clone(),
                vars: r.vars.clone(),
                tail: lock(&r.shared.out).tail.lines(),
            })
            .collect()
    }

    /// [`Self::runs`] for a page that is attaching: each run's stdout still held for the next batch is
    /// already in the tail handed back, so it is taken out of the queue, in the same critical section that
    /// copies the tail -- the page then sees every such line once, in the tail, and never again as output.
    /// Stderr is not part of a tail and stays queued.
    pub fn attach(&mut self) -> Vec<RunInfo> {
        self.runs
            .iter()
            .map(|r| {
                let mut out = lock(&r.shared.out);
                let held = std::mem::take(&mut out.lines);
                out.lines = held
                    .into_iter()
                    .filter(|(stream, _)| *stream == Stream::Stderr)
                    .collect();
                out.bytes = out.lines.iter().map(|(_, line)| line.len() + LINE_OVERHEAD).sum();
                if out.lines.is_empty() && out.dropped_lines == out.reported_lines {
                    out.since = None;
                }
                RunInfo {
                    run: r.id,
                    task: r.task.clone(),
                    vars: r.vars.clone(),
                    tail: out.tail.lines(),
                }
            })
            .collect()
    }

    pub fn is_idle(&self) -> bool {
        self.runs.is_empty()
    }

    pub fn waiter(&self) -> RunsWaiter {
        RunsWaiter(self.runs.iter().map(|r| r.shared.clone()).collect())
    }
}

/// Moves every held line into `Output` events, one stream at a time and at most `BATCH_BYTES` each (a
/// single longer line goes alone), with the drops since the last report on the last of them.
fn drain(out: &mut Out, run: u64, events: &mut Vec<TaskEvent>) {
    let mut batch: Vec<String> = Vec::new();
    let mut batch_bytes = 0;
    let mut stream: Option<Stream> = None;
    while let Some((s, line)) = out.lines.pop_front() {
        let other_stream = stream.is_some_and(|current| current != s);
        let cost = line.len() + LINE_OVERHEAD;
        if !batch.is_empty() && (other_stream || batch_bytes + cost > BATCH_BYTES) {
            events.push(TaskEvent::Output {
                run,
                stream: stream.unwrap_or(s),
                lines: std::mem::take(&mut batch),
                dropped_lines: 0,
                dropped_bytes: 0,
            });
            batch_bytes = 0;
        }
        stream = Some(s);
        batch_bytes += cost;
        batch.push(line);
    }
    let dropped_lines = out.dropped_lines - out.reported_lines;
    let dropped_bytes = out.dropped_bytes - out.reported_bytes;
    out.reported_lines = out.dropped_lines;
    out.reported_bytes = out.dropped_bytes;
    events.push(TaskEvent::Output {
        run,
        stream: stream.unwrap_or(Stream::Stdout),
        lines: batch,
        dropped_lines,
        dropped_bytes,
    });
    out.bytes = 0;
    out.since = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reader_blocked_on_a_silent_pipe_is_released_when_the_run_ends() {
        let shared = Shared::new();
        // The write end stays open and silent for the whole test: only the closer can release the reader.
        let (silent_read, _silent_write) = std::io::pipe().expect("pipe");
        let closer = shared.closer.as_ref().map(AsRawFd::as_raw_fd);
        assert!(closer.is_some(), "the run-ended pipe could not be made");
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn({
            let shared = shared.clone();
            move || {
                let closer = shared.closer.as_ref().map(AsRawFd::as_raw_fd);
                let _ = tx.send(wait_readable(silent_read.as_raw_fd(), closer));
            }
        });
        shared.finish(ExitKind::Exited(0));
        // A second finish must not block on the one-byte pipe.
        shared.finish(ExitKind::Exited(0));
        let got = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the reader was not released");
        assert!(!got, "a reader must stop once the run has ended");
        reader.join().expect("reader thread");
    }

    #[test]
    fn the_rate_window_admits_a_steady_day_and_refuses_a_burst_until_it_drains() {
        let start = Instant::now();
        let mut steady = RateWindow::new(RATE_WINDOW, RATE_BYTES);
        // 300 KiB/s for 24 hours in 100 ms steps: 3 MiB per window, always admitted.
        for step in 0..(24 * 3600 * 10u64) {
            let now = start + Duration::from_millis(step * 100);
            assert!(steady.admit(now, 30 * 1024), "refused at step {step}");
        }
        let mut burst = RateWindow::new(RATE_WINDOW, RATE_BYTES);
        let mut admitted = 0;
        for _ in 0..5000 {
            if burst.admit(start, 1024) {
                admitted += 1;
            }
        }
        assert_eq!(admitted, RATE_BYTES / 1024);
        assert!(!burst.admit(start + Duration::from_secs(9), 1024));
        assert!(burst.admit(start + RATE_WINDOW, 1024), "the window drained");
    }

    #[test]
    fn the_tail_keeps_the_last_mebibyte_and_always_the_last_line() {
        let mut tail = Tail::default();
        for i in 0..2000 {
            tail.push(format!("{i:01023}"));
        }
        let lines = tail.lines();
        assert!(lines.iter().map(String::len).sum::<usize>() <= TAIL_BYTES);
        assert_eq!(lines.last().unwrap(), &format!("{:01023}", 1999));
        let mut one = Tail::default();
        one.push("x".repeat(TAIL_BYTES + 10));
        assert_eq!(one.lines().len(), 1, "a single line over the cap is still kept");
    }

    #[test]
    fn the_cutter_splits_lines_drops_an_over_long_one_and_keeps_a_last_unterminated_line() {
        let mut cutter = LineCutter::default();
        let mut out = Vec::new();
        cutter.feed(b"a\r\nb", &mut out);
        cutter.feed(b"c\n", &mut out);
        cutter.feed(&vec![b'x'; MAX_LINE_BYTES + 1], &mut out);
        cutter.feed(b"yz\nlast", &mut out);
        cutter.end(&mut out);
        assert_eq!(
            out,
            [
                Cut::Line("a".into()),
                Cut::Line("bc".into()),
                Cut::Dropped(MAX_LINE_BYTES + 3),
                Cut::Line("last".into()),
            ]
        );
    }
}
