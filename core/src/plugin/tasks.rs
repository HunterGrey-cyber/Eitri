//! Running a plugin's declared tasks. Each run leads a process group of its own; its output is read on
//! threads, cut into lines, capped, and handed to the GTK side in batches when the host's wake says
//! there is something to take. Nothing here blocks the caller, and a runner with nothing running has no
//! thread, no timer and raises no wake.
//!
//! A stop (cancel, timeout, plugin stop, window close) sends SIGTERM to the group and SIGKILL two seconds
//! later. Two seconds is longer than the terminal's half second because the plugin's own CLI stops its
//! child and exits within one.
//!
//! On Linux a runner is given a guard command (`eitri run-guard`, [`install_guard`]) and starts every task under a
//! guard of its own: the task lives exactly as long as the window that holds it, and a run can be paused and handed
//! to another window's runner with every byte it had not yet delivered ([`TaskRunner::pause_for_move`],
//! [`TaskRunner::adopt`]). A runner without one starts the task itself.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::fs::File;
use std::io::{PipeReader, PipeWriter, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

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

/// A guard's status lines are short; anything longer is not one and is dropped.
const MAX_STATUS_LINE: usize = 4096;

/// The guard's command line goes ahead of the guard into the lifetime pipe when it fits a pipe's smallest buffer
/// (one page, what a user over the pipe-pages limit gets), so the write cannot wait for a reader.
const SPEC_INLINE_BYTES: usize = 4096;

/// A command line longer than this is not a task's: the runner refuses to start it, and the guard refuses to read it
/// rather than allocate for it.
pub const MAX_SPEC_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub task: String,
    pub vars: BTreeMap<String, String>,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

/// How a runner starts a task's guard: `program` with `args` and `env` added to the inherited environment. The
/// shell passes its own executable with `run-guard`; a test binary passes itself with the test that serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
}

static INSTALLED_GUARD: OnceLock<GuardCommand> = OnceLock::new();

/// The guard every runner made afterwards by [`TaskRunner::new`] starts its tasks under. The first call wins.
pub fn install_guard(guard: GuardCommand) {
    let _ = INSTALLED_GUARD.set(guard);
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

/// The line a reader had begun when it was paused: its bytes, or, for a line already over the cap, how long it
/// had grown.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialLine {
    pub bytes: Vec<u8>,
    pub overflow: Option<u64>,
}

/// Cuts a byte stream into lines without holding more than one line's cap of it. A trailing `\r` is
/// dropped; bytes that are not UTF-8 are replaced.
#[derive(Debug, Default)]
pub struct LineCutter {
    current: Vec<u8>,
    overflow: Option<usize>,
}

impl LineCutter {
    /// A cutter that goes on with a line another reader had begun.
    pub fn resume(partial: PartialLine) -> LineCutter {
        let PartialLine { bytes, overflow } = partial;
        match overflow {
            Some(n) => LineCutter {
                current: Vec::new(),
                overflow: Some(n as usize),
            },
            None if bytes.len() > MAX_LINE_BYTES => LineCutter {
                overflow: Some(bytes.len()),
                current: Vec::new(),
            },
            None => LineCutter {
                current: bytes,
                overflow: None,
            },
        }
    }

    /// The line begun and not yet ended, for the reader that goes on with it.
    pub fn into_partial(self) -> PartialLine {
        PartialLine {
            bytes: self.current,
            overflow: self.overflow.map(|n| n as u64),
        }
    }

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

/// The guard's last word on its task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "how", rename_all = "snake_case")]
pub enum GuardExit {
    Exited {
        code: i32,
    },
    Signaled {
        signal: i32,
    },
    SpawnFailed {
        why: String,
    },
    /// The status pipe ended without one: the guard itself was killed, and its task got SIGTERM with it.
    Gone,
}

/// A run handed to another window: where it stands and what it had read but not delivered. Its descriptors
/// travel beside it, in [`MovedRunFds`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovedRun {
    pub run: u64,
    pub task: String,
    pub vars: BTreeMap<String, String>,
    /// The task's pid, which is also its group; absent while the guard has not yet said it.
    pub pid: Option<u32>,
    /// The task's start time (`/proc` field 22) as its guard read it, so the adopter never signals a group whose
    /// number was reused.
    pub start: Option<u64>,
    /// The timeout's deadline on the monotonic clock, in milliseconds ([`monotonic_ms`]): every process on the machine
    /// reads the same clock, so the time a run spends handed over counts toward its timeout.
    pub timeout_at_ms: Option<u64>,
    pub tail: Vec<String>,
    /// Lines read and not yet handed to the page, oldest first.
    pub held: Vec<(Stream, String)>,
    pub dropped_lines: u64,
    pub dropped_bytes: u64,
    pub reported_lines: u64,
    pub reported_bytes: u64,
    /// Each stream's line begun and not ended; absent for a stream that had already reached its end.
    pub stdout: Option<PartialLine>,
    pub stderr: Option<PartialLine>,
    /// A status line the guard had begun.
    pub status_partial: Vec<u8>,
    /// The guard's last word, when it had been read before the pause.
    pub exit: Option<GuardExit>,
}

/// The descriptors of a [`MovedRun`]: its output pipes' read ends (for the streams still open), the status
/// pipe's read end and the lifetime pipe's write end.
#[derive(Debug)]
pub struct MovedRunFds {
    pub stdout: Option<OwnedFd>,
    pub stderr: Option<OwnedFd>,
    pub status: OwnedFd,
    pub lifetime: OwnedFd,
}

#[derive(Debug)]
pub struct TaskHandoff {
    pub record: MovedRun,
    pub fds: MovedRunFds,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveRefused {
    NoSuchRun,
    /// Started without a guard: nothing but this window holds its life.
    NotGuarded,
    /// Its exit or a stop is already under way.
    Ending,
    /// Its command is still being written to its guard by this process, and would be cut short if this process
    /// ended before the guard had read it; asking again once the guard has started it succeeds.
    Starting,
    /// The run's watcher went away without answering; the run is lost.
    Lost,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdoptError {
    Busy(StartError),
    /// A run with this number is already reported on here.
    RunTaken,
}

/// A handoff the runner would not take, given back whole so the caller can return it.
#[derive(Debug)]
pub struct AdoptRefused {
    pub why: AdoptError,
    pub handoff: TaskHandoff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason {
    Cancelled,
    Timeout,
}

/// Where a guarded run stands, for a pause that must not cross its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Running,
    /// A pause was asked for; the run's threads are handing their state over.
    Pausing,
    /// Its exit was read: it ends here.
    Ending,
    /// Handed over: another runner, or a later adoption here, holds it now.
    Moved,
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

struct Group {
    /// The task's group, once known: at once for a run started here, from the guard's `pid` line otherwise.
    pgid: Option<u32>,
    /// The group leader's start time, as the guard read it while the leader was its child: a signal goes only while
    /// the pid names no other process.
    start: Option<u64>,
    /// Whether the group may still be signalled. Cleared once the group is known to be gone, so a late signal
    /// cannot reach a process group that reused the number.
    live: bool,
}

struct Shared {
    out: Mutex<Out>,
    done: Mutex<bool>,
    done_changed: Condvar,
    /// Why and when a stop was asked for.
    stop: Mutex<Option<(StopReason, Instant)>>,
    group: Mutex<Group>,
    phase: Mutex<Phase>,
    /// Readable once the run has ended, to release readers blocked on a pipe that an outside process holds open.
    /// A plain pipe rather than an eventfd, which Apple targets do not have.
    closer: Option<PipeReader>,
    /// The closer's write end, taken by the first `finish`: the signal is one byte, written once.
    closer_write: Mutex<Option<PipeWriter>>,
    /// Readable once a pause is asked for: a guarded run's readers and watcher poll it beside their pipe.
    pause: Option<PipeReader>,
    pause_write: Mutex<Option<PipeWriter>>,
}

fn pipe_pair() -> (Option<PipeReader>, Option<PipeWriter>) {
    match std::io::pipe() {
        Ok((r, w)) => (Some(r), Some(w)),
        Err(_) => (None, None),
    }
}

impl Shared {
    fn new(guarded: bool) -> Arc<Shared> {
        let (closer, closer_write) = pipe_pair();
        let (pause, pause_write) = if guarded { pipe_pair() } else { (None, None) };
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
            group: Mutex::new(Group {
                pgid: None,
                start: None,
                live: true,
            }),
            phase: Mutex::new(Phase::Running),
            closer,
            closer_write: Mutex::new(closer_write),
            pause,
            pause_write: Mutex::new(pause_write),
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
        self.set_done();
    }

    fn set_done(&self) {
        *lock(&self.done) = true;
        self.done_changed.notify_all();
    }

    /// The run is held elsewhere now: its timers end and nothing here reports on it again.
    fn moved(&self) {
        *lock(&self.phase) = Phase::Moved;
        lock(&self.out).closed = true;
        self.set_done();
    }

    fn raise_pause(&self) {
        if let Some(mut writer) = lock(&self.pause_write).take() {
            // Dropping the end after the byte also hangs the pipe up: either wakes every poller.
            let _ = writer.write_all(&[1]);
        }
    }

    /// Signals the run's group while it may still exist, and not once its leader's pid names another process.
    fn signal(&self, signal: libc::c_int) {
        let mut group = lock(&self.group);
        if let (true, Some(pgid)) = (group.live, group.pgid) {
            if !group_still_named(pgid, group.start) {
                group.live = false;
                return;
            }
            signal_group(pgid, signal);
        }
    }

    fn pgid(&self) -> Option<u32> {
        lock(&self.group).pgid
    }

    fn group_gone(&self) {
        lock(&self.group).live = false;
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

/// Signals the group led by `pgid`. The group exists from the moment the task's spawn returns (the child made it
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

/// Starts the task itself, for a runner without a guard.
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
            // Without a guard the child is told when the thread that started it ends. Runs are started on the
            // window's main thread, which lives as long as the window, so a window that dies takes them with it.
            #[cfg(target_os = "linux")]
            {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM as libc::c_ulong) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // The window may have died before the line above took effect, and then nothing would signal this.
                if libc::getppid() != parent {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
            }
            Ok(())
        });
    }
    command.spawn()
}

fn push_field(out: &mut Vec<u8>, field: &[u8]) {
    out.extend_from_slice(&(field.len() as u32).to_le_bytes());
    out.extend_from_slice(field);
}

/// The task's command line as the guard reads it from the lifetime pipe: a length, then the program, the working
/// directory and each argument, each with its own length. Bytes, not text, so no path or argument is refused.
pub(crate) fn encode_spec(launch: &Launch) -> Vec<u8> {
    let mut body = Vec::new();
    push_field(&mut body, launch.program.as_os_str().as_bytes());
    push_field(&mut body, launch.cwd.as_os_str().as_bytes());
    for arg in &launch.args {
        push_field(&mut body, arg.as_bytes());
    }
    let mut spec = Vec::with_capacity(body.len() + 4);
    push_field(&mut spec, &body);
    spec
}

/// The length of [`encode_spec`]'s body, the part the guard measures against [`MAX_SPEC_BYTES`].
fn spec_body_len(launch: &Launch) -> usize {
    let fields = [launch.program.as_os_str().len(), launch.cwd.as_os_str().len()];
    fields
        .into_iter()
        .chain(launch.args.iter().map(String::len))
        .map(|len| len + 4)
        .sum()
}

/// The program, the working directory and the arguments of a spec's body; `None` unless every field is whole.
/// Read only by the guard, which runs on Linux alone.
#[cfg(target_os = "linux")]
pub(crate) fn decode_spec(body: &[u8]) -> Option<(PathBuf, PathBuf, Vec<OsString>)> {
    use std::os::unix::ffi::OsStringExt;
    let mut fields = Vec::new();
    let mut rest = body;
    while !rest.is_empty() {
        let len = u32::from_le_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let field = rest.get(4..4 + len)?;
        fields.push(OsString::from_vec(field.to_vec()));
        rest = &rest[4 + len..];
    }
    let mut fields = fields.into_iter();
    let program = PathBuf::from(fields.next()?);
    let cwd = PathBuf::from(fields.next()?);
    if program.as_os_str().is_empty() {
        return None;
    }
    Some((program, cwd, fields.collect()))
}

/// A guard just started, and the holder's ends of its four pipes.
pub(crate) struct GuardStart {
    pub(crate) child: Child,
    pub(crate) lifetime: OwnedFd,
    pub(crate) status: PipeReader,
    pub(crate) stdout: PipeReader,
    pub(crate) stderr: PipeReader,
    /// The thread still writing a long command to the guard, if there is one.
    pub(crate) spec_writer: Option<JoinHandle<()>>,
}

/// Starts `guard` for `launch`: its stdin the lifetime pipe's read end (the task's command written there first),
/// descriptor 3 the status pipe's write end, 4 and 5 the task's stdout and stderr, in a process group of its own so
/// neither a `Ctrl+C` in nor the closing of the terminal that launched the window reaches it.
pub(crate) fn spawn_guarded(guard: &GuardCommand, launch: &Launch) -> std::io::Result<GuardStart> {
    let (life_r, mut life_w) = std::io::pipe()?;
    let (status_r, status_w) = std::io::pipe()?;
    let (out_r, out_w) = std::io::pipe()?;
    let (err_r, err_w) = std::io::pipe()?;
    let spec = encode_spec(launch);
    let inline = spec.len() <= SPEC_INLINE_BYTES;
    if inline {
        life_w.write_all(&spec)?;
    }
    let mut command = Command::new(&guard.program);
    command
        .args(&guard.args)
        .envs(guard.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::from(life_r))
        .stdout(Stdio::null())
        .process_group(0);
    let raws = [status_w.as_raw_fd(), out_w.as_raw_fd(), err_w.as_raw_fd()];
    // SAFETY: only async-signal-safe calls run between fork and exec.
    unsafe {
        command.pre_exec(move || place_guard_fds(raws));
    }
    let spawned = command.spawn();
    // The command and these ends hold the window's copies of the guard's ends: dropped here, so the status pipe and
    // the output pipes end when the guard and the task have closed theirs.
    drop(command);
    drop((status_w, out_w, err_w));
    let child = spawned?;
    let spec_writer = if inline {
        None
    } else {
        // Longer than a pipe surely holds: written beside the guard, which reads it first.
        let mut writer = life_w.try_clone()?;
        Some(std::thread::spawn(move || {
            let _ = writer.write_all(&spec);
        }))
    };
    Ok(GuardStart {
        child,
        lifetime: life_w.into(),
        status: status_r,
        stdout: out_r,
        stderr: err_r,
        spec_writer,
    })
}

/// Places the guard's three inherited write ends at 3, 4 and 5. Each goes through a close-on-exec copy above 9
/// first, so an end that already sits at one of those numbers is not overwritten before it is placed.
fn place_guard_fds(raws: [RawFd; 3]) -> std::io::Result<()> {
    let mut copies = [0; 3];
    for (copy, fd) in copies.iter_mut().zip(raws) {
        // SAFETY: duplicating a descriptor reads no memory of ours.
        *copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) };
        if *copy < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    for (copy, target) in copies.into_iter().zip([3, 4, 5]) {
        // SAFETY: as above; the copy at `target` is not close-on-exec, so the guard has it.
        if unsafe { libc::dup2(copy, target) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
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
                    // The tail is bounded on its own and is what a returning page reads, so it keeps the newest
                    // stdout line even when the queue below has no room for it.
                    if stream == Stream::Stdout {
                        out.tail.push(line.clone());
                    }
                    // A page can remain unready across many rate windows, so held output needs its own bound.
                    if out.bytes + line.len() + LINE_OVERHEAD > RATE_BYTES {
                        out.dropped_lines += 1;
                        out.dropped_bytes += line.len() as u64;
                        if out.since.is_none() {
                            out.since = Some(now);
                            raise = true;
                        }
                        continue;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ready {
    Data,
    /// The run has ended: stop, whatever is still holding the pipe's other end.
    Ended,
    /// A pause was asked for: stop without closing the pipe.
    Paused,
}

/// Waits until `pipe` has something to read (or hung up), the run has ended, or a pause was asked for.
fn wait_readable(pipe: RawFd, closer: Option<RawFd>, pause: Option<RawFd>) -> Ready {
    if closer.is_none() && pause.is_none() {
        return Ready::Data;
    }
    let poll_fd = |fd: Option<RawFd>| libc::pollfd {
        fd: fd.unwrap_or(-1),
        events: libc::POLLIN,
        revents: 0,
    };
    let mut fds = [poll_fd(Some(pipe)), poll_fd(closer), poll_fd(pause)];
    loop {
        // SAFETY: `fds` is a valid array of three `pollfd` (a negative descriptor is skipped).
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 3, -1) };
        if r < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Ready::Data;
        }
        return if fds[2].revents != 0 {
            Ready::Paused
        } else if fds[1].revents != 0 {
            Ready::Ended
        } else {
            Ready::Data
        };
    }
}

/// How a reader stopped.
enum ReaderEnd {
    Ended,
    /// Stopped for a move: the pipe, unread past this point, and the line it had begun.
    Paused(OwnedFd, LineCutter),
}

fn read_stream<P>(
    mut pipe: P,
    stream: Stream,
    shared: Arc<Shared>,
    wake: agent::UiWake,
    mut cutter: LineCutter,
) -> ReaderEnd
where
    P: Read + AsRawFd + Into<OwnedFd>,
{
    let mut buf = vec![0u8; 64 * 1024];
    let mut cuts = Vec::new();
    let closer = shared.closer.as_ref().map(AsRawFd::as_raw_fd);
    let pause = shared.pause.as_ref().map(AsRawFd::as_raw_fd);
    loop {
        match wait_readable(pipe.as_raw_fd(), closer, pause) {
            Ready::Data => {}
            Ready::Ended => return ReaderEnd::Ended,
            Ready::Paused => return ReaderEnd::Paused(pipe.into(), cutter),
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
    ReaderEnd::Ended
}

fn start_reader<P>(pipe: P, stream: Stream, shared: &Arc<Shared>, wake: &agent::UiWake, cutter: LineCutter) -> Reader
where
    P: Read + AsRawFd + Into<OwnedFd> + Send + 'static,
{
    let (s, w) = (shared.clone(), wake.clone());
    (
        stream,
        std::thread::spawn(move || read_stream(pipe, stream, s, w, cutter)),
    )
}

type Reader = (Stream, JoinHandle<ReaderEnd>);

/// How the run's leader ended, as its waiter learnt it.
enum Ended {
    Status(std::io::Result<ExitStatus>),
    Guard(GuardExit),
}

/// After the leader's end: the group's grace, its orphans killed, the readers given the grace to drain, the exit.
fn finish_run(shared: &Shared, readers: Vec<Reader>, wake: &agent::UiWake, ended: Ended) {
    if let Some(pgid) = shared.pgid() {
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
            shared.signal(libc::SIGKILL);
        }
    }
    shared.group_gone();
    // The readers end when every writer of the pipes is gone; one outside the group that kept them is not
    // waited for beyond the grace.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for (_, reader) in readers {
            let _ = reader.join();
        }
        let _ = tx.send(());
    });
    if rx.recv_timeout(GRACE).is_err() {
        eprintln!("[plugin] a task's output is still held open by a process outside its group; not waiting for it");
    }
    let stop = lock(&shared.stop).map(|(reason, _)| reason);
    let kind = match (stop, ended) {
        (Some(StopReason::Cancelled), _) => ExitKind::Cancelled,
        (Some(StopReason::Timeout), _) => ExitKind::Timeout,
        (None, Ended::Status(Ok(status))) => match (status.code(), status.signal()) {
            (Some(code), _) => ExitKind::Exited(code),
            (None, Some(signal)) => ExitKind::Killed(signal),
            (None, None) => ExitKind::Killed(0),
        },
        (None, Ended::Status(Err(e))) => ExitKind::SpawnFailed(e.to_string()),
        (None, Ended::Guard(GuardExit::Exited { code })) => ExitKind::Exited(code),
        (None, Ended::Guard(GuardExit::Signaled { signal })) => ExitKind::Killed(signal),
        (None, Ended::Guard(GuardExit::SpawnFailed { why })) => ExitKind::SpawnFailed(why),
        (None, Ended::Guard(GuardExit::Gone)) => ExitKind::Killed(0),
    };
    shared.finish(kind);
    wake();
}

fn wait_run(mut child: Child, readers: Vec<Reader>, shared: Arc<Shared>, wake: agent::UiWake) {
    let status = child.wait();
    finish_run(&shared, readers, &wake, Ended::Status(status));
}

/// A guarded run's status pipe as its watcher reads it.
struct Watch {
    status: File,
    partial: Vec<u8>,
    exit: Option<GuardExit>,
}

/// What a paused run's threads hand back.
struct Paused {
    watch: Watch,
    stdout: Option<(OwnedFd, LineCutter)>,
    stderr: Option<(OwnedFd, LineCutter)>,
}

/// Reads the guard's status lines until its last word or a pause. `guard` is the guard itself where this process
/// started it (it is reaped here); an adopted run's guard is its starter's to reap.
fn watch_guard(
    mut watch: Watch,
    guard: Option<Child>,
    readers: Vec<Reader>,
    shared: Arc<Shared>,
    wake: agent::UiWake,
    paused: mpsc::Sender<Paused>,
) {
    let pause = shared.pause.as_ref().map(AsRawFd::as_raw_fd);
    let mut buf = [0u8; 1024];
    while watch.exit.is_none() {
        match wait_readable(watch.status.as_raw_fd(), None, pause) {
            Ready::Paused => return hand_over(watch, guard, readers, &paused),
            Ready::Ended | Ready::Data => {}
        }
        match watch.status.read(&mut buf) {
            Ok(0) => watch.exit = Some(GuardExit::Gone),
            Ok(n) => {
                watch.partial.extend_from_slice(&buf[..n]);
                while let Some(nl) = watch.partial.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = watch.partial.drain(..=nl).collect();
                    let line = String::from_utf8_lossy(&line[..nl]).into_owned();
                    status_line(&shared, &mut watch, &line);
                    if watch.exit.is_some() {
                        break;
                    }
                }
                if watch.partial.len() > MAX_STATUS_LINE {
                    watch.partial.clear();
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => watch.exit = Some(GuardExit::Gone),
        }
    }
    {
        let mut phase = lock(&shared.phase);
        if *phase == Phase::Pausing {
            // The pause was asked for while the last word arrived: it goes with the run.
            drop(phase);
            return hand_over(watch, guard, readers, &paused);
        }
        *phase = Phase::Ending;
    }
    let exit = watch.exit.take().unwrap_or(GuardExit::Gone);
    finish_run(&shared, readers, &wake, Ended::Guard(exit));
    if let Some(mut guard) = guard {
        let _ = guard.wait();
    }
}

fn status_line(shared: &Arc<Shared>, watch: &mut Watch, line: &str) {
    let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
    match word {
        "pid" => {
            let (pid, start) = rest.split_once(' ').unwrap_or((rest, ""));
            if let Ok(pid) = pid.parse::<u32>() {
                let start = start.parse::<u64>().ok();
                {
                    // A line read long after the guard wrote it (a run handed over before its watcher read it) may
                    // name a pid that has since been reused: the start time tells.
                    let mut group = lock(&shared.group);
                    group.pgid = Some(pid);
                    group.start = start;
                    group.live = group.live && group_still_named(pid, start);
                }
                // A stop asked for before the group was known reaches it now.
                if lock(&shared.stop).is_some() {
                    term_then_kill(shared);
                }
            }
        }
        "exit" => {
            if let Ok(code) = rest.parse() {
                watch.exit = Some(GuardExit::Exited { code });
            }
        }
        "signal" => {
            if let Ok(signal) = rest.parse() {
                watch.exit = Some(GuardExit::Signaled { signal });
            }
        }
        "spawn-failed" => watch.exit = Some(GuardExit::SpawnFailed { why: rest.to_owned() }),
        _ => {}
    }
}

fn hand_over(watch: Watch, guard: Option<Child>, readers: Vec<Reader>, paused: &mpsc::Sender<Paused>) {
    let mut stdout = None;
    let mut stderr = None;
    for (stream, reader) in readers {
        if let Ok(ReaderEnd::Paused(fd, cutter)) = reader.join() {
            match stream {
                Stream::Stdout => stdout = Some((fd, cutter)),
                Stream::Stderr => stderr = Some((fd, cutter)),
            }
        }
    }
    let _ = paused.send(Paused { watch, stdout, stderr });
    // The guard is this process's child wherever its task is held now; reaped here once it ends.
    if let Some(mut guard) = guard {
        let _ = guard.wait();
    }
}

fn term_then_kill(shared: &Arc<Shared>) {
    shared.signal(libc::SIGTERM);
    let shared = shared.clone();
    std::thread::spawn(move || {
        if !shared.wait_done(GRACE) {
            shared.signal(libc::SIGKILL);
        }
    });
}

fn request_stop(shared: &Arc<Shared>, reason: StopReason) {
    {
        // A run being handed over is its new holder's to stop: its timeout goes with it.
        let phase = lock(&shared.phase);
        if matches!(*phase, Phase::Pausing | Phase::Moved) {
            return;
        }
        let mut stop = lock(&shared.stop);
        if stop.is_some() || *lock(&shared.done) {
            return;
        }
        *stop = Some((reason, Instant::now()));
    }
    term_then_kill(shared);
}

fn start_timeout(shared: &Arc<Shared>, limit: Duration) {
    let s = shared.clone();
    std::thread::spawn(move || {
        if !s.wait_done(limit) {
            request_stop(&s, StopReason::Timeout);
        }
    });
}

/// A guarded run's holder's ends: the lifetime pipe's write end (the task lives while it is open) and the answer
/// its watcher gives a pause.
struct Guarded {
    lifetime: OwnedFd,
    paused: mpsc::Receiver<Paused>,
    /// The thread writing a long command to the guard: the run cannot move until it is done.
    spec_writer: Option<JoinHandle<()>>,
}

struct Run {
    id: u64,
    task: String,
    vars: BTreeMap<String, String>,
    shared: Arc<Shared>,
    deadline: Option<Instant>,
    guarded: Option<Guarded>,
}

pub struct TaskRunner {
    wake: agent::UiWake,
    guard: Option<GuardCommand>,
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

/// The start time of `pid`, where this platform has one to read.
fn start_time_of(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        agent::child::start_time(pid).ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// The monotonic clock in milliseconds: the clock `Instant` reads, shared by every process on the machine, so a
/// deadline written in one window means the same moment in another.
pub fn monotonic_ms() -> u64 {
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `now` is a valid `timespec` for the call to fill.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now);
    }
    (now.tv_sec as u64).saturating_mul(1000) + (now.tv_nsec as u64) / 1_000_000
}

/// Whether the group a record names may be signalled here: not when its leader's pid now names a process that
/// started at another time (the number was reused).
fn group_still_named(pid: u32, start: Option<u64>) -> bool {
    match (start, start_time_of(pid)) {
        (Some(recorded), Some(now)) => recorded == now,
        _ => true,
    }
}

impl TaskRunner {
    /// A runner under the guard [`install_guard`] set, if any.
    pub fn new(wake: agent::UiWake) -> TaskRunner {
        TaskRunner::with_guard(wake, INSTALLED_GUARD.get().cloned())
    }

    pub fn with_guard(wake: agent::UiWake, guard: Option<GuardCommand>) -> TaskRunner {
        TaskRunner {
            wake,
            guard,
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
        let deadline = launch.timeout.map(|limit| Instant::now() + limit);
        let (shared, guarded) = match &self.guard {
            Some(guard) => self.start_guarded(guard, &launch),
            None => (self.start_direct(&launch), None),
        };
        if let Some(limit) = launch.timeout {
            if !*lock(&shared.done) {
                start_timeout(&shared, limit);
            }
        }
        self.runs.push(Run {
            id,
            task: launch.task,
            vars: launch.vars,
            shared,
            deadline,
            guarded,
        });
        Ok(id)
    }

    fn start_direct(&self, launch: &Launch) -> Arc<Shared> {
        let shared = Shared::new(false);
        match spawn(launch) {
            Err(e) => {
                shared.finish(ExitKind::SpawnFailed(format!("{}: {e}", launch.program.display())));
                (self.wake)();
            }
            Ok(mut child) => {
                lock(&shared.group).pgid = Some(child.id());
                let mut readers = Vec::new();
                if let Some(pipe) = child.stdout.take() {
                    readers.push(start_reader(
                        pipe,
                        Stream::Stdout,
                        &shared,
                        &self.wake,
                        LineCutter::default(),
                    ));
                }
                if let Some(pipe) = child.stderr.take() {
                    readers.push(start_reader(
                        pipe,
                        Stream::Stderr,
                        &shared,
                        &self.wake,
                        LineCutter::default(),
                    ));
                }
                let (s, w) = (shared.clone(), self.wake.clone());
                std::thread::spawn(move || wait_run(child, readers, s, w));
            }
        }
        shared
    }

    fn start_guarded(&self, guard: &GuardCommand, launch: &Launch) -> (Arc<Shared>, Option<Guarded>) {
        let shared = Shared::new(true);
        if spec_body_len(launch) > MAX_SPEC_BYTES {
            shared.finish(ExitKind::SpawnFailed(format!(
                "{}: the command line is longer than the {MAX_SPEC_BYTES} bytes a task's guard takes",
                launch.program.display()
            )));
            (self.wake)();
            return (shared, None);
        }
        let GuardStart {
            child,
            lifetime,
            status,
            stdout,
            stderr,
            spec_writer,
        } = match spawn_guarded(guard, launch) {
            Ok(start) => start,
            Err(e) => {
                shared.finish(ExitKind::SpawnFailed(format!(
                    "the task's guard {}: {e}",
                    guard.program.display()
                )));
                (self.wake)();
                return (shared, None);
            }
        };
        let readers = vec![
            start_reader(stdout, Stream::Stdout, &shared, &self.wake, LineCutter::default()),
            start_reader(stderr, Stream::Stderr, &shared, &self.wake, LineCutter::default()),
        ];
        let watch = Watch {
            status: File::from(OwnedFd::from(status)),
            partial: Vec::new(),
            exit: None,
        };
        let (tx, rx) = mpsc::channel();
        let (s, w) = (shared.clone(), self.wake.clone());
        std::thread::spawn(move || watch_guard(watch, Some(child), readers, s, w, tx));
        (
            shared,
            Some(Guarded {
                lifetime,
                paused: rx,
                spec_writer,
            }),
        )
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
        let shared = Shared::new(false);
        shared.finish(ExitKind::SpawnFailed(message));
        self.runs.push(Run {
            id,
            task: task.to_owned(),
            vars: vars.clone(),
            shared,
            deadline: None,
            guarded: None,
        });
        (self.wake)();
        Ok(id)
    }

    /// Whether `run` is one this runner still reports on.
    pub fn cancel(&mut self, run: u64) -> bool {
        let Some(r) = self.runs.iter().find(|r| r.id == run) else {
            return false;
        };
        request_stop(&r.shared, StopReason::Cancelled);
        true
    }

    pub fn stop_all(&mut self) {
        for r in &self.runs {
            request_stop(&r.shared, StopReason::Cancelled);
        }
    }

    /// Stops `run`'s readers and watcher without closing anything and takes it out of this runner, with every line
    /// it had read and not delivered, for another runner's [`Self::adopt`]. The task runs on; its lifetime goes
    /// with the handoff (dropping the handoff ends the task, as a holder's end does).
    ///
    /// Returns promptly: each of the run's threads waits in `poll` with the pause pipe among its descriptors, and
    /// between two polls it does one bounded read and takes one lock.
    pub fn pause_for_move(&mut self, run: u64) -> Result<TaskHandoff, MoveRefused> {
        let at = self
            .runs
            .iter()
            .position(|r| r.id == run)
            .ok_or(MoveRefused::NoSuchRun)?;
        {
            let r = &self.runs[at];
            let Some(guarded) = r.guarded.as_ref().filter(|_| r.shared.pause.is_some()) else {
                return Err(MoveRefused::NotGuarded);
            };
            // The rest of a long command is still in this process: a guard that never got it whole would hold its
            // holder's lifetime pipe and wait for it forever.
            if guarded.spec_writer.as_ref().is_some_and(|w| !w.is_finished()) {
                return Err(MoveRefused::Starting);
            }
            let mut phase = lock(&r.shared.phase);
            if *phase != Phase::Running || lock(&r.shared.stop).is_some() || *lock(&r.shared.done) {
                return Err(MoveRefused::Ending);
            }
            *phase = Phase::Pausing;
            r.shared.raise_pause();
        }
        let Run {
            id,
            task,
            vars,
            shared,
            deadline,
            guarded,
        } = self.runs.remove(at);
        let Some(Guarded { lifetime, paused, .. }) = guarded else {
            return Err(MoveRefused::NotGuarded);
        };
        let Ok(Paused { watch, stdout, stderr }) = paused.recv() else {
            shared.moved();
            return Err(MoveRefused::Lost);
        };
        let (tail, held, dropped_lines, dropped_bytes, reported_lines, reported_bytes) = {
            let mut out = lock(&shared.out);
            let held: Vec<(Stream, String)> = std::mem::take(&mut out.lines).into_iter().collect();
            let tail = out.tail.lines();
            out.bytes = 0;
            out.since = None;
            (
                tail,
                held,
                out.dropped_lines,
                out.dropped_bytes,
                out.reported_lines,
                out.reported_bytes,
            )
        };
        shared.moved();
        let (pid, start) = {
            let group = lock(&shared.group);
            (group.pgid, group.start)
        };
        let Watch {
            status,
            partial: status_partial,
            exit,
        } = watch;
        let (stdout_fd, stdout_partial) = split_paused(stdout);
        let (stderr_fd, stderr_partial) = split_paused(stderr);
        Ok(TaskHandoff {
            record: MovedRun {
                run: id,
                task,
                vars,
                pid,
                start,
                timeout_at_ms: deadline.map(|at| {
                    monotonic_ms().saturating_add(at.saturating_duration_since(Instant::now()).as_millis() as u64)
                }),
                tail,
                held,
                dropped_lines,
                dropped_bytes,
                reported_lines,
                reported_bytes,
                stdout: stdout_partial,
                stderr: stderr_partial,
                status_partial,
                exit,
            },
            fds: MovedRunFds {
                stdout: stdout_fd,
                stderr: stderr_fd,
                status: status.into(),
                lifetime,
            },
        })
    }

    /// Takes a run another runner paused (or this one, when a move is called off): its readers go on with the
    /// lines they had begun, its held lines are delivered as if read here, and its timeout keeps its deadline.
    pub fn adopt(&mut self, handoff: TaskHandoff) -> Result<u64, Box<AdoptRefused>> {
        let refuse = |why, handoff| Err(Box::new(AdoptRefused { why, handoff }));
        if self.runs.iter().any(|r| r.id == handoff.record.run) {
            return refuse(AdoptError::RunTaken, handoff);
        }
        if let Err(busy) = self.admit(&handoff.record.task) {
            return refuse(AdoptError::Busy(busy), handoff);
        }
        let TaskHandoff { record, fds } = handoff;
        let MovedRun {
            run,
            task,
            vars,
            pid,
            start,
            timeout_at_ms,
            tail,
            held,
            dropped_lines,
            dropped_bytes,
            reported_lines,
            reported_bytes,
            stdout,
            stderr,
            status_partial,
            exit,
        } = record;
        let MovedRunFds {
            stdout: stdout_fd,
            stderr: stderr_fd,
            status,
            lifetime,
        } = fds;
        let shared = Shared::new(true);
        let now = Instant::now();
        let waiting = {
            let mut out = lock(&shared.out);
            for line in tail {
                out.tail.push(line);
            }
            out.bytes = held.iter().map(|(_, line)| line.len() + LINE_OVERHEAD).sum();
            out.lines = held.into_iter().collect();
            out.dropped_lines = dropped_lines;
            out.dropped_bytes = dropped_bytes;
            out.reported_lines = reported_lines;
            out.reported_bytes = reported_bytes;
            let waiting = !out.lines.is_empty() || out.dropped_lines != out.reported_lines;
            if waiting {
                out.since = Some(now);
            }
            waiting
        };
        {
            let mut group = lock(&shared.group);
            group.pgid = pid;
            group.start = start;
            group.live = pid.is_none_or(|pid| group_still_named(pid, start));
        }
        let mut readers = Vec::new();
        for (stream, fd, partial) in [(Stream::Stdout, stdout_fd, stdout), (Stream::Stderr, stderr_fd, stderr)] {
            if let Some(fd) = fd {
                let cutter = LineCutter::resume(partial.unwrap_or_default());
                readers.push(start_reader(File::from(fd), stream, &shared, &self.wake, cutter));
            }
        }
        let watch = Watch {
            status: File::from(status),
            partial: status_partial,
            exit,
        };
        let (tx, rx) = mpsc::channel();
        {
            let (s, w) = (shared.clone(), self.wake.clone());
            std::thread::spawn(move || watch_guard(watch, None, readers, s, w, tx));
        }
        // What is left of the timeout: the time the run spent handed over counts, so moving a run never extends it.
        let limit = timeout_at_ms.map(|at| Duration::from_millis(at.saturating_sub(monotonic_ms())));
        if let Some(limit) = limit {
            start_timeout(&shared, limit);
        }
        self.next = self.next.max(run + 1);
        self.runs.push(Run {
            id: run,
            task,
            vars,
            shared,
            deadline: limit.map(|limit| now + limit),
            guarded: Some(Guarded {
                lifetime,
                paused: rx,
                spec_writer: None,
            }),
        });
        if waiting {
            (self.wake)();
        }
        Ok(run)
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

fn split_paused(paused: Option<(OwnedFd, LineCutter)>) -> (Option<OwnedFd>, Option<PartialLine>) {
    match paused {
        Some((fd, cutter)) => (Some(fd), Some(cutter.into_partial())),
        None => (None, None),
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
        let shared = Shared::new(false);
        // The write end stays open and silent for the whole test: only the closer can release the reader.
        let (silent_read, _silent_write) = std::io::pipe().expect("pipe");
        let closer = shared.closer.as_ref().map(AsRawFd::as_raw_fd);
        assert!(closer.is_some(), "the run-ended pipe could not be made");
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn({
            let shared = shared.clone();
            move || {
                let closer = shared.closer.as_ref().map(AsRawFd::as_raw_fd);
                let _ = tx.send(wait_readable(silent_read.as_raw_fd(), closer, None));
            }
        });
        shared.finish(ExitKind::Exited(0));
        // A second finish must not block on the one-byte pipe.
        shared.finish(ExitKind::Exited(0));
        let got = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the reader was not released");
        assert_eq!(got, Ready::Ended, "a reader must stop once the run has ended");
        reader.join().expect("reader thread");
    }

    #[test]
    fn held_output_stays_bounded_when_the_rate_window_renews() {
        for stream in [Stream::Stdout, Stream::Stderr] {
            let shared = Shared::new(false);
            let wake: agent::UiWake = Arc::new(|| {});
            let line = "x".repeat(RATE_BYTES / 4 - LINE_OVERHEAD);
            for _ in 0..8 {
                // A fresh budget models another output window while the page is still loading.
                lock(&shared.out).rate = RateWindow::new(RATE_WINDOW, RATE_BYTES);
                deliver(&shared, stream, &mut vec![Cut::Line(line.clone())], &wake);
            }
            let mut out = lock(&shared.out);
            assert_eq!(out.bytes, RATE_BYTES, "held output cannot grow across rate windows");
            assert_eq!(out.lines.len(), 4);
            assert_eq!(out.dropped_lines, 4);
            assert_eq!(out.dropped_bytes, (line.len() * 4) as u64);
            let mut events = Vec::new();
            drain(&mut out, 1, &mut events);
            assert_eq!(out.bytes, 0);
            drop(out);

            deliver(&shared, stream, &mut vec![Cut::Line("resumed".into())], &wake);
            assert_eq!(lock(&shared.out).lines.back(), Some(&(stream, "resumed".into())));
        }
    }

    #[test]
    fn stdout_refused_by_the_held_cap_still_becomes_the_tails_last_line() {
        let shared = Shared::new(false);
        let wake: agent::UiWake = Arc::new(|| {});
        let line = "x".repeat(RATE_BYTES / 4 - LINE_OVERHEAD);
        for _ in 0..4 {
            deliver(&shared, Stream::Stdout, &mut vec![Cut::Line(line.clone())], &wake);
        }
        assert_eq!(lock(&shared.out).bytes, RATE_BYTES, "the queue is full");
        // A new rate window: the line is admitted by the rate but finds no room in the queue.
        lock(&shared.out).rate = RateWindow::new(RATE_WINDOW, RATE_BYTES);
        deliver(
            &shared,
            Stream::Stdout,
            &mut vec![Cut::Line("status: newest".into())],
            &wake,
        );
        let out = lock(&shared.out);
        assert_eq!(out.lines.len(), 4, "the full queue takes no more");
        assert_eq!(out.dropped_lines, 1, "the refused line is counted as output not sent");
        assert_eq!(
            out.tail.lines().last().map(String::as_str),
            Some("status: newest"),
            "a returning page must read the run's newest stdout line"
        );
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

    /// Runs under this test binary's own guard, as a window's runs are under `eitri run-guard`.
    #[cfg(target_os = "linux")]
    mod guarded {
        use super::*;
        use crate::run_guard::test_binary_guard;

        const HANG_GUARD: Duration = Duration::from_secs(60);

        fn guard() -> GuardCommand {
            test_binary_guard("run_guard::tests::guard_entry")
        }

        fn runner() -> TaskRunner {
            TaskRunner::with_guard(Arc::new(|| {}), Some(guard()))
        }

        fn sh(task: &str, script: &str, timeout: Option<Duration>) -> Launch {
            Launch {
                task: task.into(),
                vars: BTreeMap::new(),
                program: "/bin/sh".into(),
                args: vec!["-c".into(), script.into()],
                cwd: std::env::temp_dir(),
                timeout,
            }
        }

        /// One poll with a far-future clock, so a partial batch is taken at once: `run`'s stdout lines and its exit.
        fn poll_once(r: &mut TaskRunner, run: u64) -> (Vec<String>, Option<(ExitKind, u64)>) {
            let (events, _) = r.poll(Instant::now() + Duration::from_secs(3600));
            let mut lines = Vec::new();
            let mut exit = None;
            for event in events {
                match event {
                    TaskEvent::Output {
                        run: of,
                        stream: Stream::Stdout,
                        lines: got,
                        ..
                    } if of == run => lines.extend(got),
                    TaskEvent::Exit {
                        run: of,
                        kind,
                        dropped_lines,
                        ..
                    } if of == run => exit = Some((kind, dropped_lines)),
                    _ => {}
                }
            }
            (lines, exit)
        }

        /// Every stdout line `r` reports for `run` until it ends, its exit and its dropped lines.
        fn until_exit(r: &mut TaskRunner, run: u64) -> (Vec<String>, ExitKind, u64) {
            let deadline = Instant::now() + HANG_GUARD;
            let mut seen = Vec::new();
            loop {
                assert!(Instant::now() < deadline, "run {run} never ended");
                let (lines, exit) = poll_once(r, run);
                seen.extend(lines);
                if let Some((kind, dropped)) = exit {
                    return (seen, kind, dropped);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn first_line(r: &mut TaskRunner, run: u64) -> String {
            let deadline = Instant::now() + HANG_GUARD;
            loop {
                assert!(Instant::now() < deadline, "no output from run {run}");
                if let Some(line) = poll_once(r, run).0.into_iter().next() {
                    return line;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn wait_gone(pid: u32) {
            let deadline = Instant::now() + HANG_GUARD;
            loop {
                let alive = std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                    stat.rsplit(')')
                        .next()
                        .is_some_and(|r| !r.trim_start().starts_with('Z'))
                });
                if !alive {
                    return;
                }
                assert!(Instant::now() < deadline, "pid {pid} outlived its run");
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        /// Pauses `run` in `r` until `ready` holds of the handoff; a handoff that is not yet ready is taken back.
        fn pause_when(r: &mut TaskRunner, run: u64, ready: impl Fn(&MovedRun) -> bool) -> TaskHandoff {
            let deadline = Instant::now() + HANG_GUARD;
            loop {
                let handoff = r.pause_for_move(run).expect("a running guarded task can be paused");
                if ready(&handoff.record) {
                    return handoff;
                }
                assert!(
                    Instant::now() < deadline,
                    "the run never reached the state to move it in"
                );
                r.adopt(handoff).expect("a paused run is taken back by its own runner");
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        #[test]
        fn a_task_that_wrote_hello_before_the_move_and_world_after_reports_helloworld() {
            let dir = crate::plugin::test_support::Scratch::new("helloworld");
            let go = dir.0.join("go");
            let script = format!(
                "echo ready; printf hello; while [ ! -e '{}' ]; do sleep 0.02; done; printf 'world\\n'",
                go.display()
            );
            let mut a = runner();
            let run = a.start(sh("t", &script, None)).unwrap();
            assert_eq!(first_line(&mut a, run), "ready");
            // Until the reader has taken `hello` into its line it is still in the pipe; the move must carry the
            // half line itself, so the pause waits for that.
            let handoff = pause_when(&mut a, run, |rec| {
                rec.stdout.as_ref().is_some_and(|p| p.bytes == b"hello")
            });
            assert!(handoff.record.held.is_empty(), "`ready` was delivered before the move");
            // The window that started the task is gone; the task lives on in the handoff.
            drop(a);
            let mut b = runner();
            assert_eq!(b.adopt(handoff).expect("adopted"), run, "the run keeps its number");
            std::fs::write(&go, b"").unwrap();
            let (lines, kind, dropped) = until_exit(&mut b, run);
            assert_eq!(lines, ["helloworld"]);
            assert_eq!((kind, dropped), (ExitKind::Exited(0), 0));
        }

        #[test]
        fn no_line_is_delivered_twice_after_adoption() {
            let mut a = runner();
            let script = "seq 1 100; sleep 0.3; seq 101 25000; sleep 1; seq 25001 50000";
            let run = a.start(sh("t", script, None)).unwrap();
            let mut seen = Vec::new();
            let deadline = Instant::now() + HANG_GUARD;
            while seen.last().map(String::as_str) != Some("100") {
                assert!(Instant::now() < deadline, "the first lines never came");
                seen.extend(poll_once(&mut a, run).0);
                std::thread::sleep(Duration::from_millis(5));
            }
            // Lines delivered here, lines read and held for the page, and lines still in the pipe all exist at once.
            let handoff = pause_when(&mut a, run, |rec| !rec.held.is_empty());
            let mut b = runner();
            // A run here already has the moved run's number, then the moved run's task is busy here: each refusal
            // gives the handoff back whole.
            let taken = b.start(sh("u", "sleep 300", None)).unwrap();
            assert_eq!(taken, run);
            let refused = b.adopt(handoff).expect_err("a runner with the number taken refuses it");
            assert_eq!(refused.why, AdoptError::RunTaken);
            assert!(b.cancel(taken));
            until_exit(&mut b, taken);
            let busy = b.start(sh("t", "sleep 300", None)).unwrap();
            let refused = b
                .adopt(refused.handoff)
                .expect_err("a runner with the task busy refuses it");
            assert_eq!(refused.why, AdoptError::Busy(StartError::TaskBusy("t".into())));
            assert!(b.cancel(busy));
            until_exit(&mut b, busy);
            b.adopt(refused.handoff)
                .expect("adopted once the number and the task are free");
            let (rest, kind, dropped) = until_exit(&mut b, run);
            seen.extend(rest);
            assert_eq!((kind, dropped), (ExitKind::Exited(0), 0));
            let want: Vec<String> = (1..=50000).map(|i| i.to_string()).collect();
            assert_eq!(seen.len(), want.len(), "every line once");
            assert!(seen == want, "every line once, in order");
        }

        #[test]
        fn the_timeout_and_stop_still_end_the_tasks_group() {
            // A timeout set in one window ends the task in the window it moved to.
            let mut a = runner();
            let mut b = runner();
            let run = a
                .start(sh("t", "sleep 300 & echo $!; wait", Some(Duration::from_millis(1500))))
                .unwrap();
            let pid: u32 = first_line(&mut a, run).parse().unwrap();
            let handoff = a.pause_for_move(run).unwrap();
            assert!(handoff
                .record
                .timeout_at_ms
                .is_some_and(|at| at <= monotonic_ms() + 1500));
            b.adopt(handoff).unwrap();
            let (_, kind, _) = until_exit(&mut b, run);
            assert_eq!(kind, ExitKind::Timeout);
            wait_gone(pid);
            // A stop of a run that never moved, whose group ignores SIGTERM.
            let run = a
                .start(sh(
                    "c",
                    "trap '' TERM; sleep 300 & echo $!; while :; do sleep 1; done",
                    None,
                ))
                .unwrap();
            let pid: u32 = first_line(&mut a, run).parse().unwrap();
            assert!(a.cancel(run));
            assert_eq!(until_exit(&mut a, run).1, ExitKind::Cancelled);
            wait_gone(pid);
            // A stop of a run that moved.
            let run = a.start(sh("m", "sleep 300 & echo $!; wait", None)).unwrap();
            let pid: u32 = first_line(&mut a, run).parse().unwrap();
            b.adopt(a.pause_for_move(run).unwrap()).unwrap();
            assert!(!a.cancel(run), "the run is no longer the first window's");
            assert!(b.cancel(run));
            assert_eq!(until_exit(&mut b, run).1, ExitKind::Cancelled);
            wait_gone(pid);
            assert!(a.is_idle() && b.is_idle());
        }

        /// Whether `pid` still runs (a zombie does not).
        fn alive(pid: u32) -> bool {
            std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                stat.rsplit(')')
                    .next()
                    .is_some_and(|r| !r.trim_start().starts_with('Z'))
            })
        }

        #[test]
        fn what_a_task_left_running_ends_with_its_handoff_after_its_leader_ended_during_the_move() {
            for adopted in [false, true] {
                let dir = crate::plugin::test_support::Scratch::new("leftover");
                let go = dir.0.join("go");
                let script = format!(
                    "sleep 300 & echo $!; while [ ! -e '{}' ]; do sleep 0.02; done; exit 0",
                    go.display()
                );
                let mut a = runner();
                let run = a.start(sh("t", &script, None)).unwrap();
                let left: u32 = first_line(&mut a, run).parse().unwrap();
                let handoff = pause_when(&mut a, run, |rec| rec.pid.is_some());
                let leader = handoff.record.pid.unwrap();
                // The leader ends while nobody reads its guard's status pipe; its child is still in the group.
                std::fs::write(&go, b"").unwrap();
                wait_gone(leader);
                std::thread::sleep(Duration::from_millis(300));
                assert!(alive(left), "the leader's child ended with the leader");
                if adopted {
                    let mut b = runner();
                    b.adopt(handoff).unwrap();
                    assert_eq!(until_exit(&mut b, run).1, ExitKind::Exited(0));
                } else {
                    drop(handoff);
                }
                wait_gone(left);
            }
        }

        #[test]
        fn a_pid_that_now_names_another_process_is_never_signalled() {
            let mut other = Command::new("sleep")
                .arg("300")
                .process_group(0)
                .spawn()
                .expect("sleep starts");
            let pid = other.id();
            let start = start_time_of(pid).expect("its start time");
            let shared = Shared::new(true);
            let mut watch = Watch {
                status: File::open("/dev/null").unwrap(),
                partial: Vec::new(),
                exit: None,
            };
            // A pid line read after the task it named had gone and its number had been given to `other`.
            status_line(&shared, &mut watch, &format!("pid {pid} {}", start + 1));
            assert_eq!(shared.pgid(), Some(pid));
            shared.signal(libc::SIGKILL);
            std::thread::sleep(Duration::from_millis(200));
            assert!(
                other.try_wait().unwrap().is_none(),
                "a group whose leader started at another time was signalled"
            );
            // The same line with the start time that matches does reach the group.
            let shared = Shared::new(true);
            status_line(&shared, &mut watch, &format!("pid {pid} {start}"));
            shared.signal(libc::SIGKILL);
            assert_eq!(other.wait().unwrap().signal(), Some(libc::SIGKILL));
        }

        #[test]
        fn the_time_a_run_spends_handed_over_counts_toward_its_timeout() {
            let timeout = Duration::from_millis(1500);
            let mut a = runner();
            let run = a.start(sh("t", "sleep 300 & echo $!; wait", Some(timeout))).unwrap();
            let pid: u32 = first_line(&mut a, run).parse().unwrap();
            let handoff = a.pause_for_move(run).unwrap();
            // Held for longer than the whole timeout: the run is past its deadline when it is taken.
            std::thread::sleep(timeout + Duration::from_millis(300));
            let mut b = runner();
            b.adopt(handoff).unwrap();
            let deadline = b.runs.iter().find(|r| r.id == run).and_then(|r| r.deadline);
            assert!(
                deadline.is_some_and(|at| at <= Instant::now()),
                "the adopted run was given its remaining time afresh"
            );
            assert_eq!(until_exit(&mut b, run).1, ExitKind::Timeout);
            wait_gone(pid);
        }

        #[test]
        fn a_run_whose_long_command_is_still_being_written_refuses_to_move() {
            // A guard that reads nothing, so the command's writer is still at work when the move is asked for.
            let deaf = GuardCommand {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "sleep 1".into()],
                env: Vec::new(),
            };
            let mut r = TaskRunner::with_guard(Arc::new(|| {}), Some(deaf));
            let mut launch = sh("t", "true", None);
            launch.args.push("x".repeat(256 * 1024));
            let run = r.start(launch).unwrap();
            assert_eq!(r.pause_for_move(run).unwrap_err(), MoveRefused::Starting);
            // The guard ends without a word: its status pipe closes.
            assert_eq!(until_exit(&mut r, run).1, ExitKind::Killed(0));
        }

        #[test]
        fn a_command_line_over_the_guards_limit_fails_to_start() {
            let mut r = runner();
            let mut launch = sh("t", "true", None);
            launch.args.push("x".repeat(MAX_SPEC_BYTES));
            let run = r.start(launch).unwrap();
            match until_exit(&mut r, run).1 {
                ExitKind::SpawnFailed(why) => assert!(why.contains("longer than"), "{why}"),
                other => panic!("expected a failed start, got {other:?}"),
            }
        }

        #[test]
        fn the_guard_is_started_from_the_injected_command() {
            let mut command = guard();
            command.env.push(("EITRI_TASKS_TEST_MARK".into(), "carried".into()));
            let mut r = TaskRunner::with_guard(Arc::new(|| {}), Some(command));
            let script = "tr '\\0' ' ' < /proc/$PPID/cmdline; echo; \
                          echo \"mark=$EITRI_TASKS_TEST_MARK helper=${EITRI_RUN_GUARD_HELPER:-unset}\"";
            let run = r.start(sh("t", script, None)).unwrap();
            let (lines, kind, _) = until_exit(&mut r, run);
            assert_eq!(kind, ExitKind::Exited(0));
            assert!(
                lines[0].contains("run_guard::tests::guard_entry"),
                "the task's parent is the injected guard: {lines:?}"
            );
            assert_eq!(lines[1], "mark=carried helper=unset");
            // A guard that cannot start is a run that failed to spawn.
            let missing = GuardCommand {
                program: "/nonexistent/eitri".into(),
                args: Vec::new(),
                env: Vec::new(),
            };
            let mut r = TaskRunner::with_guard(Arc::new(|| {}), Some(missing));
            let run = r.start(sh("t", "echo never", None)).unwrap();
            match until_exit(&mut r, run).1 {
                ExitKind::SpawnFailed(why) => assert!(why.contains("/nonexistent/eitri"), "{why}"),
                other => panic!("expected a failed spawn, got {other:?}"),
            }
            // Without a guard the runner starts the task itself, and such a run cannot be handed over.
            let mut r = TaskRunner::with_guard(Arc::new(|| {}), None);
            let run = r.start(sh("t", "sleep 300", None)).unwrap();
            assert_eq!(r.pause_for_move(run).unwrap_err(), MoveRefused::NotGuarded);
            assert!(r.cancel(run));
            assert_eq!(until_exit(&mut r, run).1, ExitKind::Cancelled);
        }

        #[test]
        fn a_spec_round_trips_bytes_that_are_not_text() {
            let mut launch = sh("t", "x", None);
            launch.program = PathBuf::from(std::ffi::OsStr::from_bytes(b"/bin/\xffsh"));
            launch.args.push(String::new());
            let spec = encode_spec(&launch);
            assert_eq!(spec_body_len(&launch), spec.len() - 4);
            let (program, cwd, args) = decode_spec(&spec[4..]).expect("decodes");
            assert_eq!(program, launch.program);
            assert_eq!(cwd, launch.cwd);
            assert_eq!(args, ["-c", "x", ""].map(OsString::from));
            assert_eq!(decode_spec(&spec[4..spec.len() - 1]), None, "a cut field is refused");
        }
    }
}
