//! Streamed execution on the server for explicitly supported purposes.
//!
//! A run starts with the laptop's first message after the start: a `close` starts the child with no input at all
//! (stdin from `/dev/null`) and the run counts as closed at once; stdin starts it with a pipe that carries the
//! laptop's bytes until its explicit `close`. A laptop that goes away before that first message starts nothing. The
//! child's stdout and stderr go back in chunks as the laptop takes them (a laptop that stops reading stops the child at
//! a full pipe), and its exit status comes last, after the `close`.
//!
//! A laptop that goes away **before** `close` never sent the whole input, so the child and its process group are
//! killed while its stdin is still open: it never sees an end of input and acts on none of it. Going away is seen on
//! the input stream ending or failing, and also on the output stream being dropped, which the transport does on a
//! lost link even while a write to a child that is not reading waits. A dropped output only counts once the messages
//! already queued on the input are taken in: a `close` among them means the laptop handed over its whole input before
//! it went, and the run goes on (starting with that input, if it had not started yet). One that goes away **after**
//! `close` has handed
//! over a command that must not be cut short half-way (a commit), so the child runs to its end, its output is read and
//! dropped, and its status goes to the audit log while the daemon is running. At daemon exit the readers go away,
//! and a child still writing to a pipe can get SIGPIPE.
pub mod git;

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::future::Future;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eitri_server_proto::{exec_input, exec_output, ExecExit, ExecInput, ExecOutput};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::ChildStdin;
use tokio::sync::mpsc;
use tokio_stream::{Stream, StreamExt};
use tonic::Status;

use crate::daemon::sync::audit_word;

/// The largest stdout or stderr chunk sent in one message.
pub const OUTPUT_CHUNK: usize = 64 * 1024;
/// How long a pipe may stay silent once the child has exited before it is no longer read: a descendant that keeps the
/// pipes open (a detached maintenance job) does not hold the exit status back. A laptop that is slow to take output
/// that is already there is waited for.
const DRAIN_IDLE_AFTER_EXIT: Duration = Duration::from_secs(2);
/// The longest one run may take from its start message to its end, a laptop that stops reading included; past it the
/// child's group is killed. A forwarded git write takes seconds; this only ends a run that hangs.
pub const EXEC_DEADLINE: Duration = Duration::from_secs(3600);
/// How long a stopping daemon waits for runs past their `close` before it leaves them with an unknown outcome.
pub const STOP_GRACE: Duration = Duration::from_secs(10);

pub type Output = mpsc::Receiver<Result<ExecOutput, Status>>;

/// A command ready to run: what to execute, its arguments after the program name, where, with what environment, and
/// how long it may take.
#[derive(Clone, Debug)]
pub struct Prepared {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: Vec<(OsString, OsString)>,
    /// The audit line's name for the command (`git <subcommand>`).
    pub audit: String,
    pub deadline: Duration,
}

/// How a run ended, as the audit log records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ended {
    Exited(ExecExit),
    /// The laptop went away before `close`; the child was killed with its input unfinished.
    KilledBeforeClose,
    /// The run passed its deadline; the child was killed.
    KilledAtDeadline,
    /// The daemon stopped before the laptop closed the input; the child was killed.
    KilledByDaemonStop,
    /// The laptop went away (or sent nothing more) before the first message after the start: nothing ran.
    NotStarted,
    /// The child could not be started.
    Refused(String),
}

/// The runs this daemon has going, so a stopping daemon can kill the ones whose input is unfinished and wait a while
/// for the others.
#[derive(Default)]
pub struct Runs {
    next: AtomicU64,
    stopping: tokio::sync::watch::Sender<bool>,
    runs: Mutex<HashMap<u64, RunEntry>>,
    changed: tokio::sync::Notify,
    /// Holds a byte once stopping has begun. Each child looks at it in `pre_exec`, after `chdir` and just before exec, so
    /// admission is decided in the child: one whose spawn outlasts the start of a stop (a `chdir` waiting on a stalled
    /// mount) never execs, and the spawning thread holds no lock while it waits.
    /// Only a pipe that was made is kept: a failure (out of descriptors) refuses that one run, and the next tries again.
    admission: std::sync::OnceLock<(std::io::PipeReader, std::io::PipeWriter)>,
    /// Called on the spawning thread just before a child is created, so a test can start a stop at that moment.
    #[cfg(test)]
    spawn_gate: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// How many of the next attempts to make the admission pipe fail, as when the daemon is out of descriptors.
    #[cfg(test)]
    pipe_failures: AtomicU64,
}

struct RunEntry {
    /// The child's process group, once it exists and until it is reaped.
    group: Option<i32>,
    closed: bool,
    /// The daemon stopping killed it.
    stopped: bool,
    line: String,
}

impl Runs {
    pub fn running(&self) -> usize {
        self.runs.lock().expect("runs").len()
    }

    fn add(&self, line: String) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.runs.lock().expect("runs").insert(
            id,
            RunEntry {
                group: None,
                closed: false,
                stopped: false,
                line,
            },
        );
        id
    }

    fn update(&self, id: u64, change: impl FnOnce(&mut RunEntry)) {
        if let Some(entry) = self.runs.lock().expect("runs").get_mut(&id) {
            change(entry);
        }
    }

    fn remove(&self, id: u64) {
        self.runs.lock().expect("runs").remove(&id);
        self.changed.notify_waiters();
    }

    /// Kills the process group of every run whose laptop has not closed its input, as the daemon stops: the pipe to
    /// its stdin closes with the daemon, and an end of input the laptop never sent must not be read as one.
    pub fn kill_unclosed(&self) {
        let mut runs = self.runs.lock().expect("runs");
        if !self.stopping.send_replace(true) {
            if let Ok((_, writer)) = self.admission() {
                // The pipe is empty until now, so one byte never blocks.
                let _ = std::io::Write::write(&mut &*writer, &[0]);
            }
        }
        for entry in runs.values_mut() {
            if entry.closed {
                continue;
            }
            if let Some(group) = entry.group {
                entry.stopped = true;
                // SAFETY: kill takes no pointers; the group is the unreaped child's own.
                unsafe { libc::kill(-group, libc::SIGKILL) };
            }
        }
    }

    /// The admission pipe, made on first use. A run only spawns after this returned the pipe, and a stop writes into the
    /// same pipe, so every child that could be forked sees the stop.
    fn admission(&self) -> Result<&(std::io::PipeReader, std::io::PipeWriter), String> {
        if let Some(pipe) = self.admission.get() {
            return Ok(pipe);
        }
        #[cfg(test)]
        if self
            .pipe_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err("the admission pipe could not be made: Too many open files".into());
        }
        let made = std::io::pipe().map_err(|error| format!("the admission pipe could not be made: {error}"))?;
        // A run made one at the same moment: either pipe works, and the one kept is the one every caller sees.
        let _ = self.admission.set(made);
        Ok(self.admission.get().expect("the admission pipe was just set"))
    }

    fn stopped(&self, id: u64) -> bool {
        self.runs
            .lock()
            .expect("runs")
            .get(&id)
            .is_some_and(|entry| entry.stopped)
    }

    /// Waits up to `grace` for the remaining runs to end; each still going is audited `outcome-unknown`, since the
    /// daemon will not see its end.
    pub async fn settle(&self, state: &Path, grace: Duration) {
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            let notified = self.changed.notified();
            if self.running() == 0 || tokio::time::timeout_at(deadline, notified).await.is_err() {
                break;
            }
        }
        let left: Vec<String> = self
            .runs
            .lock()
            .expect("runs")
            .values()
            .map(|entry| entry.line.clone())
            .collect();
        for line in left {
            let _ = crate::audit::record(state, "exec", &format!("{line} outcome-unknown"));
        }
    }
}

/// The output of a refused start: one exit message carrying the refusal.
pub fn refused(refusal: String) -> Output {
    let (sender, receiver) = mpsc::channel(1);
    let _ = sender.try_send(Ok(exit_message(ExecExit {
        refusal,
        ..Default::default()
    })));
    receiver
}

fn exit_message(exit: ExecExit) -> ExecOutput {
    ExecOutput {
        output: Some(exec_output::Output::Exit(exit)),
    }
}

/// Runs `prepared` once the laptop's next message says how (see the module's header) and returns its output and the
/// run's end. `input` carries the laptop's messages after the start. The run owns itself from here: dropping the handle
/// stops nothing, and dropping the output before `close` counts as the laptop going away. `state` is the audit log's
/// directory and `started_ms` the time the laptop's call arrived; both go into the run's audit lines.
pub fn run<S>(
    prepared: Prepared,
    input: S,
    state: PathBuf,
    started_ms: u64,
    runs: Arc<Runs>,
) -> (Output, tokio::task::JoinHandle<Ended>)
where
    S: Stream<Item = Result<ExecInput, Status>> + Send + Unpin + 'static,
{
    let (sender, receiver) = mpsc::channel(4);
    let handle = tokio::spawn(supervise(prepared, input, sender, state, started_ms, runs));
    (receiver, handle)
}

type Writing = Pin<Box<dyn Future<Output = (ChildStdin, std::io::Result<()>)> + Send>>;

/// Holds what keeps a child's input open, and kills the child's group before letting go of it while the laptop has
/// not closed the input: a run dropped for any reason (the daemon stopping, the task aborted) never hands its child an
/// end of input. The kill comes first in `drop`, before the pipe is dropped.
struct Guard {
    runs: Arc<Runs>,
    id: u64,
    group: Option<i32>,
    closed: bool,
    stdin: Option<ChildStdin>,
    writing: Option<Writing>,
}

impl Guard {
    /// Registration and shutdown share a lock; process creation, which may wait on the working directory, runs
    /// outside it. A child that comes back from `spawn` passed admission before the stop began (see [`Runs`]), so it is
    /// treated as a run published before the stop: killed when its input is unfinished, left to finish when closed.
    fn spawn(
        &mut self,
        closed: bool,
        spawn: impl FnOnce() -> std::io::Result<tokio::process::Child>,
    ) -> std::io::Result<Option<tokio::process::Child>> {
        if *self.runs.stopping.borrow() {
            return Ok(None);
        }
        #[cfg(test)]
        if let Some(gate) = self.runs.spawn_gate.lock().expect("spawn gate").as_ref() {
            gate();
        }
        let child = spawn()?;
        let mut runs = self.runs.runs.lock().expect("runs");
        let entry = runs.get_mut(&self.id).expect("registered run");
        self.group = child.id().map(|pid| pid as i32);
        entry.group = self.group;
        if *self.runs.stopping.borrow() && !closed {
            entry.stopped = true;
            if let Some(group) = self.group {
                // SAFETY: this child has not been reaped and owns the group.
                unsafe {
                    libc::kill(-group, libc::SIGKILL);
                }
            }
        } else {
            self.closed = closed;
            entry.closed = closed;
        }
        Ok(Some(child))
    }

    fn close(&mut self) -> bool {
        self.runs.update(self.id, |entry| {
            if !entry.stopped {
                entry.closed = true;
                self.closed = true;
            }
        });
        self.closed
    }

    /// The child has been reaped: its group id is no longer its own to signal.
    fn reaped(&mut self) {
        self.group = None;
        self.runs.update(self.id, |entry| entry.group = None);
    }

    /// Kills the child's group while its stdin is still held open.
    fn kill(&mut self) {
        if let Some(group) = self.group {
            // SAFETY: kill takes no pointers; the child is unreaped, so its group id is still its own.
            unsafe { libc::kill(-group, libc::SIGKILL) };
        }
    }

    fn let_go_of_stdin(&mut self) {
        self.writing.take();
        self.stdin.take();
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.closed {
            self.kill();
        }
        self.let_go_of_stdin();
        self.runs.remove(self.id);
    }
}

/// What ended the wait for the child's input.
enum Input {
    Closed,
    Gone,
    Deadline,
}

async fn supervise<S>(
    prepared: Prepared,
    input: S,
    sender: mpsc::Sender<Result<ExecOutput, Status>>,
    state: PathBuf,
    started_ms: u64,
    runs: Arc<Runs>,
) -> Ended
where
    S: Stream<Item = Result<ExecInput, Status>> + Send + Unpin + 'static,
{
    let deadline = tokio::time::Instant::now() + prepared.deadline;
    let line = format!(
        "{} cwd={} started_ms={started_ms}",
        prepared.audit,
        audit_word(&prepared.cwd)
    );
    let mut guard = Guard {
        id: runs.add(line.clone()),
        runs,
        group: None,
        closed: false,
        stdin: None,
        writing: None,
    };
    let record = |event: &'static str, fields: String| {
        let state = state.clone();
        async move {
            let _ = tokio::task::spawn_blocking(move || crate::audit::record(&state, event, &fields)).await;
        }
    };
    let finish = |ended: Ended| {
        let audited = record("exec", format!("{line} {}", outcome(&ended)));
        async move {
            audited.await;
            ended
        }
    };
    let mut input = Laptop {
        input,
        queued: VecDeque::new(),
        lost: false,
    };
    // Nothing runs until the laptop says whether there is input. What it sent before going away still counts.
    let mut stopping = guard.runs.stopping.subscribe();
    let mut stopped_first = false;
    let first = loop {
        tokio::select! {
            biased;
            _ = stopping.wait_for(|stopping| *stopping) => {
                stopped_first = true;
                break None;
            }
            next = input.next() => break next,
            _ = sender.closed(), if !input.lost => {
                if !input.lost_after_close(None) {
                    break None;
                }
            }
            _ = tokio::time::sleep_until(deadline) => break None,
        }
    };
    // A refusal is the run's only message. The run stays registered until the transport has taken it, so a stopping
    // daemon (which waits for its runs) does not drop the server with the refusal still queued.
    let refuse = |refusal: String, ended: Ended| {
        let sender = sender.clone();
        async move {
            let sent = sender
                .send(Ok(exit_message(ExecExit {
                    refusal,
                    ..Default::default()
                })))
                .await;
            if sent.is_ok() {
                taken(&sender).await;
            }
            ended
        }
    };
    let stopping_refusal = || {
        format!(
            "the Eitri server is stopping; {} did not start; nothing was run",
            prepared.audit
        )
    };
    let (first_bytes, closed) = match first.map(|message| message.map(|message| message.input)) {
        Some(Ok(Some(exec_input::Input::Stdin(bytes)))) => (Some(bytes), false),
        Some(Ok(Some(exec_input::Input::Close(true)))) => (None, true),
        // The stop came first, even with the laptop's first message already queued: it is told nothing ran.
        _ if stopped_first => return finish(refuse(stopping_refusal(), Ended::NotStarted).await).await,
        _ => return finish(Ended::NotStarted).await,
    };
    let admission = match guard.runs.admission() {
        Ok((admission, _)) => admission.as_raw_fd(),
        Err(why) => {
            let refusal = format!("{} could not start in {}: {why}", prepared.audit, prepared.cwd);
            let ended = refuse(refusal.clone(), Ended::Refused(refusal)).await;
            return finish(ended).await;
        }
    };
    record("exec-start", line.clone()).await;
    let mut command = tokio::process::Command::new(&prepared.program);
    command
        .args(&prepared.args)
        .current_dir(&prepared.cwd)
        .env_clear()
        .envs(prepared.env.iter().map(|(key, value)| (key, value)))
        .stdin(if closed { Stdio::null() } else { Stdio::piped() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false);
    // Its own process group, so a kill reaches the hooks and helpers it started.
    let daemon_pid = std::process::id() as libc::pid_t;
    // SAFETY: getppid, poll and setsid are async-signal-safe; poll writes only to this closure's own entry.
    unsafe {
        command.pre_exec(move || {
            // chdir precedes this hook. The mount can recover after the daemon's grace period and exit.
            if libc::getppid() != daemon_pid {
                return Err(std::io::Error::from_raw_os_error(libc::ECANCELED));
            }
            // A byte (or any error) on the admission pipe: the daemon has begun to stop, so this child never execs.
            let mut stop = libc::pollfd {
                fd: admission,
                events: libc::POLLIN,
                revents: 0,
            };
            if libc::poll(&mut stop, 1, 0) != 0 {
                return Err(std::io::Error::from_raw_os_error(libc::ECANCELED));
            }
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // The blocking worker owns registration while chdir/exec waits. Shutdown can take the registry lock
    // and start its grace timer even when both runtime workers would otherwise be waiting on a mount.
    let (mut guard, started) = tokio::task::spawn_blocking(move || {
        let result = guard.spawn(closed, || command.spawn());
        (guard, result)
    })
    .await
    .expect("spawn worker returns its guard");
    let refused_by_stop =
        |error: &std::io::Error| error.raw_os_error() == Some(libc::ECANCELED) && *guard.runs.stopping.borrow();
    let mut child = match started {
        Ok(Some(child)) => child,
        // The daemon began to stop before the child was admitted: nothing ran, and the laptop is told so.
        Ok(None) => return finish(refuse(stopping_refusal(), Ended::NotStarted).await).await,
        Err(error) if refused_by_stop(&error) => {
            return finish(refuse(stopping_refusal(), Ended::NotStarted).await).await
        }
        Err(error) => {
            let refusal = format!("{} could not start in {}: {error}", prepared.audit, prepared.cwd);
            return finish(refuse(refusal.clone(), Ended::Refused(refusal)).await).await;
        }
    };
    if guard.runs.stopped(guard.id) {
        guard.stdin = child.stdin.take();
        let status = child.wait().await;
        guard.reaped();
        return finish(stopped_outcome(status.ok())).await;
    }
    // Taken out of the child: tokio's wait would otherwise close it, which must happen only on the laptop's close.
    guard.stdin = child.stdin.take();
    let listening = Arc::new(AtomicBool::new(true));
    let (exited_sender, exited) = tokio::sync::watch::channel(false);
    let mut out = tokio::spawn(pump(
        child.stdout.take().expect("piped stdout"),
        sender.clone(),
        listening.clone(),
        exec_output::Output::Stdout,
        exited.clone(),
    ));
    let mut err = tokio::spawn(pump(
        child.stderr.take().expect("piped stderr"),
        sender.clone(),
        listening.clone(),
        exec_output::Output::Stderr,
        exited,
    ));
    let mut status: Option<std::io::Result<ExitStatus>> = None;
    if !closed {
        if let (Some(bytes), Some(stdin)) = (first_bytes, guard.stdin.take()) {
            guard.writing = Some(write(stdin, bytes));
        }
        let reaped = |exit: ExecExit| record("exec-exited", format!("{line} {}", outcome(&Ended::Exited(exit))));
        let ended = feed(
            &mut guard,
            &mut input,
            &sender,
            &mut child,
            &mut status,
            deadline,
            &reaped,
        )
        .await;
        if !matches!(ended, Input::Closed) || !guard.close() {
            let killed = status.is_none();
            if killed {
                guard.kill();
                status = Some(child.wait().await);
                guard.reaped();
            }
            guard.let_go_of_stdin();
            listening.store(false, Ordering::SeqCst);
            out.abort();
            err.abort();
            let ended = match (ended, status) {
                (_, status) if guard.runs.stopped(guard.id) => stopped_outcome(status.and_then(Result::ok)),
                (Input::Deadline, _) if killed => Ended::KilledAtDeadline,
                (_, Some(Ok(status))) if !killed => Ended::Exited(exit_of(status)),
                _ => Ended::KilledBeforeClose,
            };
            return finish(ended).await;
        }
    }
    // The laptop's close is the end of the child's input.
    guard.let_go_of_stdin();
    let status = match status {
        Some(status) => status,
        None => {
            tokio::select! {
                status = child.wait() => status,
                _ = tokio::time::sleep_until(deadline) => {
                    guard.kill();
                    let _ = child.wait().await;
                    guard.reaped();
                    out.abort();
                    err.abort();
                    return finish(Ended::KilledAtDeadline).await;
                }
            }
        }
    };
    guard.reaped();
    let _ = exited_sender.send(true);
    let drained = tokio::time::timeout_at(deadline, async {
        let _ = (&mut out).await;
        let _ = (&mut err).await;
    })
    .await;
    if drained.is_err() {
        listening.store(false, Ordering::SeqCst);
        out.abort();
        err.abort();
    }
    let exit = match status {
        Ok(status) => exit_of(status),
        Err(error) => ExecExit {
            code: -1,
            signal: 0,
            refusal: format!("waiting for {} failed: {error}", prepared.audit),
        },
    };
    // Nobody may be reading any more; the status still goes to the audit log.
    let _ = tokio::time::timeout_at(deadline, sender.send(Ok(exit_message(exit.clone())))).await;
    finish(Ended::Exited(exit)).await
}

/// Waits, for at most two seconds, until the transport has taken every message queued on `sender` or the laptop is
/// gone.
async fn taken(sender: &mpsc::Sender<Result<ExecOutput, Status>>) {
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while sender.capacity() < sender.max_capacity() && !sender.is_closed() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
}

fn stopped_outcome(status: Option<ExitStatus>) -> Ended {
    use std::os::unix::process::ExitStatusExt;
    match status {
        Some(status) if status.signal() != Some(libc::SIGKILL) => Ended::Exited(exit_of(status)),
        _ => Ended::KilledByDaemonStop,
    }
}

fn exit_of(status: ExitStatus) -> ExecExit {
    use std::os::unix::process::ExitStatusExt;
    ExecExit {
        code: status.code().unwrap_or(-1),
        signal: status.signal().unwrap_or(0),
        refusal: String::new(),
    }
}

fn write(mut stdin: ChildStdin, bytes: Vec<u8>) -> Writing {
    Box::pin(async move {
        let written = stdin.write_all(&bytes).await;
        (stdin, written)
    })
}

/// What a message from the laptop asks of the input.
enum Asked {
    Data,
    Close,
    /// The stream ended or failed without a close, or sent a second start or an empty message.
    Gone,
}

fn asked(next: &Option<Result<ExecInput, Status>>) -> Asked {
    match next {
        Some(Ok(ExecInput {
            input: Some(exec_input::Input::Stdin(_)),
        })) => Asked::Data,
        Some(Ok(ExecInput {
            input: Some(exec_input::Input::Close(true)),
        })) => Asked::Close,
        _ => Asked::Gone,
    }
}

/// Passes the laptop's stdin to the child until `close`, and says what ended it. One message at a time waits behind a
/// pending write; an end of the input is acted on at once. The child is reaped as soon as it exits (its status is kept
/// in `status` until the protocol lets it be sent, and audited through `reaped` at once), and the laptop going away is
/// watched all along, on the output being dropped too, which a lost link does even while a write to a child that is not
/// reading holds the input back.
async fn feed<S, R, A>(
    guard: &mut Guard,
    input: &mut Laptop<S>,
    sender: &mpsc::Sender<Result<ExecOutput, Status>>,
    child: &mut tokio::process::Child,
    status: &mut Option<std::io::Result<ExitStatus>>,
    deadline: tokio::time::Instant,
    reaped: &R,
) -> Input
where
    S: Stream<Item = Result<ExecInput, Status>> + Unpin,
    R: Fn(ExecExit) -> A + Sync,
    A: Future<Output = ()> + Send,
{
    // A child that no longer reads its stdin (it exited, or closed it) gets no more; the input is still followed.
    let mut stopping = guard.runs.stopping.subscribe();
    let mut writable = true;
    let mut ahead: Option<Option<Result<ExecInput, Status>>> = None;
    loop {
        if let Some(next) = &ahead {
            let idle = guard.writing.is_none();
            match asked(next) {
                Asked::Gone => return Input::Gone,
                Asked::Close if idle => return Input::Closed,
                Asked::Data if idle => {
                    if let Some(Some(Ok(ExecInput {
                        input: Some(exec_input::Input::Stdin(bytes)),
                    }))) = ahead.take()
                    {
                        if let (true, Some(stdin)) = (writable, guard.stdin.take()) {
                            guard.writing = Some(write(stdin, bytes));
                        }
                    }
                    continue;
                }
                // Waits behind the pending write.
                Asked::Close | Asked::Data => {}
            }
        }
        let writing = guard.writing.is_some();
        let reading = ahead.is_none();
        let reaping = status.is_none();
        tokio::select! {
            _ = async { let _ = stopping.wait_for(|stopping| *stopping).await; } => return Input::Gone,
            written = async { guard.writing.as_mut().expect("a write is pending").await }, if writing => {
                let (stdin, result) = written;
                guard.writing = None;
                writable &= result.is_ok();
                guard.stdin = Some(stdin);
            }
            next = input.next(), if reading => ahead = Some(next),
            exited = child.wait(), if reaping => {
                guard.reaped();
                if let Ok(exited) = &exited {
                    reaped(exit_of(*exited)).await;
                }
                *status = Some(exited);
            }
            _ = sender.closed(), if !input.lost => {
                if !input.lost_after_close(ahead.as_ref()) {
                    return Input::Gone;
                }
            }
            _ = tokio::time::sleep_until(deadline) => return Input::Deadline,
        }
    }
}

/// The laptop's side of a run's input: its stream, and what was already queued on it when the laptop went away.
struct Laptop<S> {
    input: S,
    queued: VecDeque<Option<Result<ExecInput, Status>>>,
    /// The output was dropped: only messages queued before that are read.
    lost: bool,
}

impl<S: Stream<Item = Result<ExecInput, Status>> + Unpin> Laptop<S> {
    /// The next message: a queued one, or the stream's next; once the laptop is gone and the queue is empty, the end.
    async fn next(&mut self) -> Option<Result<ExecInput, Status>> {
        if let Some(next) = self.queued.pop_front() {
            return next;
        }
        if self.lost {
            return None;
        }
        self.input.next().await
    }

    /// The output was dropped (the link is gone): takes in every message already queued on the input, and says
    /// whether a `close` is among them (or is `ahead`, waiting behind a write). A laptop that sent its `close` and then
    /// went away handed over its whole input, so the run goes on to its end; one that did not is gone.
    fn lost_after_close(&mut self, ahead: Option<&Option<Result<ExecInput, Status>>>) -> bool {
        use futures_util::FutureExt;
        self.lost = true;
        while let Some(next) = self.input.next().now_or_never() {
            let ended = next.is_none();
            self.queued.push_back(next);
            if ended {
                break;
            }
        }
        ahead
            .into_iter()
            .chain(&self.queued)
            .any(|next| matches!(asked(next), Asked::Close))
    }
}

/// Reads one of the child's pipes in chunks and sends each as `wrap` makes it, while anyone listens; once nobody does,
/// it reads on and drops what it reads, so the child is never stopped by a pipe nobody empties. Once the child has
/// exited (`exited`), a pipe silent for [`DRAIN_IDLE_AFTER_EXIT`] is left; a laptop slow to take what was read is
/// waited for, so output is never cut short for that.
async fn pump<R: AsyncRead + Unpin>(
    mut reader: R,
    sender: mpsc::Sender<Result<ExecOutput, Status>>,
    listening: Arc<AtomicBool>,
    wrap: fn(Vec<u8>) -> exec_output::Output,
    mut exited: tokio::sync::watch::Receiver<bool>,
) {
    let mut chunk = vec![0u8; OUTPUT_CHUNK];
    let mut after_exit = false;
    loop {
        after_exit |= *exited.borrow();
        let read = if after_exit {
            match tokio::time::timeout(DRAIN_IDLE_AFTER_EXIT, reader.read(&mut chunk)).await {
                Ok(read) => read,
                Err(_) => return,
            }
        } else {
            tokio::select! {
                read = reader.read(&mut chunk) => read,
                changed = exited.changed() => {
                    after_exit |= changed.is_err();
                    continue;
                }
            }
        };
        let read = match read {
            Ok(0) | Err(_) => return,
            Ok(read) => read,
        };
        if listening.load(Ordering::SeqCst)
            && sender
                .send(Ok(ExecOutput {
                    output: Some(wrap(chunk[..read].to_vec())),
                }))
                .await
                .is_err()
        {
            listening.store(false, Ordering::SeqCst);
        }
    }
}

fn outcome(ended: &Ended) -> String {
    match ended {
        Ended::Exited(exit) if !exit.refusal.is_empty() => format!("failed {}", audit_word(&exit.refusal)),
        Ended::Exited(exit) if exit.signal != 0 => format!("exit=signal-{}", exit.signal),
        Ended::Exited(exit) => format!("exit={}", exit.code),
        Ended::KilledBeforeClose => "killed=input-unfinished".to_owned(),
        Ended::KilledAtDeadline => "killed=deadline".to_owned(),
        Ended::KilledByDaemonStop => "killed=daemon-stop".to_owned(),
        Ended::NotStarted => "not-started".to_owned(),
        Ended::Refused(refusal) => format!("refused {}", audit_word(refusal)),
    }
}

#[cfg(test)]
mod registration_tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_can_start_while_a_child_is_spawning() {
        use std::os::unix::process::ExitStatusExt;
        // The child here stands for one that passed admission just before the stop began.
        for closed in [false, true] {
            let runs = Arc::new(Runs::default());
            let mut guard = Guard {
                id: runs.add("git".into()),
                runs: runs.clone(),
                group: None,
                closed: false,
                stdin: None,
                writing: None,
            };
            let mut child = guard
                .spawn(closed, || {
                    if runs.runs.try_lock().is_err() {
                        return Err(std::io::Error::other("spawn holds the registry"));
                    }
                    let child = tokio::process::Command::new("sleep")
                        .arg("60")
                        .process_group(0)
                        .spawn()?;
                    runs.kill_unclosed();
                    Ok(child)
                })
                .unwrap()
                .unwrap();
            if closed {
                // Its laptop handed over the whole input: it is left to finish, as a run published before the stop.
                assert!(!runs.stopped(guard.id) && guard.closed);
                let group = guard.group.unwrap();
                // SAFETY: the test's own unreaped child.
                unsafe { libc::kill(-group, libc::SIGKILL) };
            } else {
                assert!(runs.stopped(guard.id));
            }
            assert_eq!(child.wait().await.unwrap().signal(), Some(libc::SIGKILL));
            guard.reaped();
        }
    }

    #[tokio::test]
    async fn a_close_first_child_admitted_after_a_stop_began_never_execs_and_says_so() {
        let dir = std::env::temp_dir().join(format!(
            "eitri-exec-admission-{}-{}",
            std::process::id(),
            uuid_free_suffix()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("ran");
        let runs = Arc::new(Runs::default());
        let weak = Arc::downgrade(&runs);
        // The stop begins while the spawn is under way (for example, while chdir waits on a stalled mount).
        *runs.spawn_gate.lock().unwrap() = Some(Box::new(move || {
            if let Some(runs) = weak.upgrade() {
                runs.kill_unclosed();
            }
        }));
        let prepared = Prepared {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), format!("touch '{}'", marker.display())],
            cwd: dir.to_str().unwrap().into(),
            env: vec![("PATH".into(), "/usr/bin:/bin".into())],
            audit: "git commit".into(),
            deadline: Duration::from_secs(60),
        };
        let input = tokio_stream::iter(vec![Ok(ExecInput {
            input: Some(exec_input::Input::Close(true)),
        })]);
        let (mut output, ended) = run(prepared, input, dir.join("state"), 1, runs.clone());
        let mut messages = vec![];
        while let Some(message) = output.recv().await {
            messages.push(message.unwrap());
        }
        assert_eq!(ended.await.unwrap(), Ended::NotStarted);
        assert!(!marker.exists(), "the child ran after the stop began");
        match messages.as_slice() {
            [ExecOutput {
                output: Some(exec_output::Output::Exit(exit)),
            }] => assert!(exit.refusal.contains("nothing was run"), "{exit:?}"),
            other => panic!("the laptop must get one exit message: {other:?}"),
        }
        let audit = std::fs::read_to_string(dir.join("state/audit.log")).unwrap();
        assert!(audit.contains("not-started"), "{audit}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn an_admission_pipe_that_could_not_be_made_is_tried_again_for_the_next_run() {
        let dir = std::env::temp_dir().join(format!(
            "eitri-exec-admission-retry-{}-{}",
            std::process::id(),
            uuid_free_suffix()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let runs = Arc::new(Runs::default());
        runs.pipe_failures.store(1, Ordering::SeqCst);
        let mut exits = vec![];
        for _ in 0..2 {
            let prepared = Prepared {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "exit 7".into()],
                cwd: dir.to_str().unwrap().into(),
                env: vec![],
                audit: "git commit".into(),
                deadline: Duration::from_secs(60),
            };
            let input = tokio_stream::iter(vec![Ok(ExecInput {
                input: Some(exec_input::Input::Close(true)),
            })]);
            let (mut output, ended) = run(prepared, input, dir.join("state"), 1, runs.clone());
            let mut exit = None;
            while let Some(message) = output.recv().await {
                if let Some(exec_output::Output::Exit(status)) = message.unwrap().output {
                    exit = Some(status);
                }
            }
            exits.push((exit.expect("an exit message"), ended.await.unwrap()));
        }
        assert!(exits[0].0.refusal.contains("admission pipe"), "{:?}", exits[0]);
        assert!(matches!(exits[0].1, Ended::Refused(_)), "{:?}", exits[0]);
        // Descriptors were freed in between: the next run makes the pipe and runs.
        assert_eq!(
            (exits[1].0.code, exits[1].0.refusal.as_str()),
            (7, ""),
            "{:?}",
            exits[1]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn uuid_free_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }
}
