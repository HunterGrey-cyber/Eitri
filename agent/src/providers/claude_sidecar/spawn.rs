//! Locates, builds if necessary, and spawns Verdandi's `apps/claude-sidecar` Node.js process --
//! deliberately NOT reusing `panel/src/supervisor_client.rs`'s spawn pattern wholesale. Two real,
//! verified differences from that pattern:
//!
//! 1. `apps/claude-sidecar/dist/` is gitignored in Verdandi, not committed -- a git-dependency
//!    checkout of the Rust protocol crate does NOT give this process a runnable sidecar.
//!    This module locates a real Verdandi checkout on disk and builds the sidecar there if needed.
//! 2. The sidecar treats stdin EOF as its own parent-death shutdown signal (its own `lifecycle.ts`
//!    says so explicitly). `Stdio::null()` -- the pattern `supervisor_client.rs` uses -- makes it
//!    exit within the same tick it starts, since reading from `/dev/null` is an instant EOF. This
//!    module spawns with `Stdio::piped()` for stdin and holds the write handle open for the whole
//!    `SpawnedSidecar`'s lifetime; dropping (or explicitly closing) that handle is the intended
//!    shutdown signal, not a process_group/signal trick.
//!
//! Unlike `supervisor` (a shared, machine-wide, detached daemon `shell` never waits on), each
//! `ClaudeSidecarProvider` holds exactly one sidecar process, closer to how
//! `agent::process::AgentProcess` owns its one `claude` child: the sidecar ends when its holder ends
//! it or dies (stdin EOF), and through nothing else. So it is started in a process group of its own:
//! in the spawner's group, a `Ctrl+C` in the terminal that started the window, or that terminal
//! closing, would end it too, also after the chat was handed to another window that still holds it.
//!
//! A sidecar can be handed to another process (a chat moving to the other window): its stdin,
//! stdout and stderr go as descriptors, the drains stop at the byte they reached and hand over the
//! line in progress and the stderr tail, and the side that let go ends nothing. The side that took
//! it over is its holder from the commit on, and ends it as the spawner would, through a pidfd.

// Handing a sidecar to another process is Linux-only (`moving.rs`); elsewhere the pieces it uses
// here are compiled and unused.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// How long `SpawnedSidecar::drop` waits, after closing the sidecar's stdin, before escalating to
/// SIGKILL. A sidecar that advertises the `set_permission_mode` capability (protocol 3; an older one
/// does not reap) reaps the `claude` CLI it owns before exiting on its own: a 250ms
/// internal grace, then SIGTERM, then SIGKILL 3s later if that CLI hasn't exited, then
/// gives up 1s after that: ~4.3s worst case. Killing the SIDECAR inside that window orphans the
/// `claude` process it was in the middle of ending, which is worse than the thing this constant
/// exists to bound. 6s leaves >1.5s of margin over that worst case; measured ~1.4s mid-turn
/// (Verdandi's own test).
pub const SIDECAR_EXIT_GRACE: Duration = Duration::from_secs(6);

/// How long a started sidecar is given to bind its socket before the start is called failed.
///
/// Before it binds, the sidecar loads its own single-executable Node binary (about 105 MB), the
/// Claude Agent SDK's module graph, and runs the host CLI's `--version` to check it, synchronously
/// (through a launcher script on some machines). With a cold page cache after boot that took over
/// three seconds, while the same sidecar warm binds in well under half a second: a 3 s budget made
/// the first start of a session fail and the retry work. Fifteen seconds is five times the
/// budget that proved too short, and only a slow start ever reaches it. A sidecar that refuses to
/// start is not waited out: the loop notices a dead child at once, so this only bounds one that is
/// alive and slow. The wait runs on the connect's own worker thread, never the GTK thread.
const SIDECAR_BIND_BUDGET: Duration = Duration::from_secs(15);
/// How often the socket is tried while the sidecar starts.
const SIDECAR_BIND_POLL: Duration = Duration::from_millis(50);

/// The timeout's summary line; it names the budget actually waited so the message cannot drift from it.
fn bind_timeout_summary(socket_path: &std::path::Path, budget: Duration) -> String {
    format!(
        "claude-sidecar did not bind {socket_path:?} within {}ms",
        budget.as_millis()
    )
}

/// How many of the sidecar's most recent stderr lines are retained for diagnostics. Bounded on
/// purpose: a long-lived sidecar can log indefinitely, and this buffer exists to explain a startup
/// failure or carry a compatibility warning, not to be a second log file.
const STDERR_TAIL_CAPACITY: usize = 40;

/// The sidecar's recent stderr, shared between its drain thread and whoever needs to explain what
/// went wrong. Before this existed, the sidecar's own startup errors reached `eprintln!` and
/// nowhere else: a real CLI-version refusal (which happens *before* the socket is bound) surfaced
/// to the caller as nothing but `TimedOut: did not bind ... within 1000ms`, with the actual cause
/// invisible to any UI.
#[derive(Default)]
struct StderrTail {
    lines: VecDeque<String>,
}

impl StderrTail {
    fn push(&mut self, line: String) {
        if self.lines.len() == STDERR_TAIL_CAPACITY {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    fn snapshot(&self) -> Vec<String> {
        self.lines.iter().cloned().collect()
    }

    /// The lines as bytes, each ended by a newline: the form another process takes the tail up in.
    fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        for line in &self.lines {
            bytes.extend_from_slice(line.as_bytes());
            bytes.push(b'\n');
        }
        bytes
    }

    /// The tail [`Self::to_bytes`] wrote, as lines again (at most the capacity, newest kept).
    fn from_bytes(bytes: &[u8]) -> Self {
        let mut tail = Self::default();
        for line in bytes.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
            tail.push(String::from_utf8_lossy(line).into_owned());
        }
        tail
    }
}

/// The longest line in progress a drain keeps before it prints what it has as a line of its own: a
/// sidecar that writes without newlines must not grow this process's memory.
const MAX_PARTIAL_LINE: usize = 64 * 1024;

/// A sidecar this process started, or took over from the process that did. Which of them ends it
/// is `holds`: the spawner holds it from the start, a process taking it over from its commit on, and
/// a process that handed it on no longer does.
pub(crate) struct SpawnedSidecar {
    pub(crate) socket_path: PathBuf,
    /// Which Verdandi checkout and revision this sidecar was built and launched from. Descriptive,
    /// always present -- reaches the UI as `ProviderInfo::verdandi_checkout`, NOT as a diagnostic.
    pub(crate) build_description: String,
    /// Only things genuinely worth warning about (today: baseline drift). Usually empty.
    pub(crate) build_warnings: Vec<String>,
    /// This process's copy of the sidecar's stdin. The sidecar ends on its EOF, so the last copy to
    /// close, wherever it is, is the sidecar's end.
    stdin_keepalive: Option<File>,
    process: SidecarProcess,
    stdout: Output,
    stderr: Output,
    stderr_tail: Arc<Mutex<StderrTail>>,
    /// Whether dropping this ends the sidecar (close stdin, wait, kill, delete the socket file). When
    /// it does not, dropping it only closes this process's copies of the descriptors.
    holds: bool,
}

/// How this process names the sidecar's process.
enum SidecarProcess {
    /// Started here: this process is its kernel parent and reaps it.
    Spawned(Child),
    /// Taken over from the process that started it, named by a pidfd opened for its pid and checked
    /// against its start time, so a signal never reaches a process that took a reused pid.
    #[cfg(target_os = "linux")]
    Adopted(crate::child::PidFd),
}

/// One of the sidecar's output pipes, and whether this process is reading it.
enum Output {
    Draining(Drain),
    /// Read by nobody: stopped for a move, or not started yet in the process taking one over. The
    /// bytes after the last newline read so far wait in `partial`.
    Parked {
        pipe: File,
        partial: Vec<u8>,
    },
    /// The sidecar's end of it closed, or this process let go of it.
    Closed,
}

/// Wakes a drain blocked in `poll`, for good: the write end is shut down, so the read end stays
/// readable.
struct StopSignal {
    raised: AtomicBool,
    wake: UnixStream,
    watch: UnixStream,
}

impl StopSignal {
    fn new() -> std::io::Result<Arc<StopSignal>> {
        let (wake, watch) = UnixStream::pair()?;
        Ok(Arc::new(StopSignal {
            raised: AtomicBool::new(false),
            wake,
            watch,
        }))
    }

    fn raise(&self) {
        self.raised.store(true, Ordering::SeqCst);
        let _ = self.wake.shutdown(std::net::Shutdown::Write);
    }

    /// Waits until `fd` has something to read or hung up (true), or the stop is raised (false, which
    /// wins when both are ready).
    fn wait_readable(&self, fd: RawFd) -> bool {
        let pollin = |fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let mut fds = [pollin(fd), pollin(self.watch.as_raw_fd())];
        loop {
            // SAFETY: `fds` is a valid, writable array of two `pollfd`s for the call.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
            if ready >= 0 {
                break;
            }
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                // The read that follows reports whatever is wrong with the descriptor.
                return !self.raised.load(Ordering::SeqCst);
            }
        }
        fds[1].revents == 0 && !self.raised.load(Ordering::SeqCst)
    }
}

/// A thread printing one of the sidecar's output pipes line by line (stderr's lines also go to the
/// tail), until the pipe's end or a stop. It polls the pipe together with a stop descriptor rather
/// than blocking in `read`, so a sidecar that writes nothing can still be handed on at once.
struct Drain {
    stop: Arc<StopSignal>,
    thread: Option<JoinHandle<Drained>>,
}

/// Where a drain left its pipe.
struct Drained {
    pipe: File,
    /// The line in progress: bytes read after the last newline.
    partial: Vec<u8>,
    /// The sidecar's end closed (or the pipe failed): nothing more will come.
    ended: bool,
}

impl Drain {
    fn start(
        pipe: File,
        partial: Vec<u8>,
        stream_name: &'static str,
        tail: Option<Arc<Mutex<StderrTail>>>,
    ) -> std::io::Result<Drain> {
        let stop = StopSignal::new()?;
        let signal = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name(format!("sidecar-{stream_name}"))
            .spawn(move || drain(pipe, partial, stream_name, tail, &signal))?;
        Ok(Drain {
            stop,
            thread: Some(thread),
        })
    }

    /// Stops the drain at the byte it reached. `None` only if its thread panicked, with the pipe.
    fn stop(mut self) -> Option<Drained> {
        self.stop.raise();
        self.thread.take()?.join().ok()
    }
}

fn drain(
    mut pipe: File,
    mut partial: Vec<u8>,
    stream_name: &'static str,
    tail: Option<Arc<Mutex<StderrTail>>>,
    stop: &StopSignal,
) -> Drained {
    let mut buf = [0u8; 8192];
    loop {
        if !stop.wait_readable(pipe.as_raw_fd()) {
            return Drained {
                pipe,
                partial,
                ended: false,
            };
        }
        match pipe.read(&mut buf) {
            Ok(0) => {
                if !partial.is_empty() {
                    print_line(&std::mem::take(&mut partial), stream_name, tail.as_ref());
                }
                return Drained {
                    pipe,
                    partial,
                    ended: true,
                };
            }
            Ok(n) => {
                partial.extend_from_slice(&buf[..n]);
                while let Some(at) = partial.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = partial.drain(..=at).collect();
                    print_line(&line[..line.len() - 1], stream_name, tail.as_ref());
                }
                if partial.len() > MAX_PARTIAL_LINE {
                    print_line(&std::mem::take(&mut partial), stream_name, tail.as_ref());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => {
                return Drained {
                    pipe,
                    partial,
                    ended: true,
                }
            }
        }
    }
}

fn print_line(line: &[u8], stream_name: &str, tail: Option<&Arc<Mutex<StderrTail>>>) {
    let line = String::from_utf8_lossy(line).into_owned();
    log_line(format!("claude-sidecar[{stream_name}]: {line}"));
    if let Some(tail) = tail {
        if let Ok(mut tail) = tail.lock() {
            tail.push(line);
        }
    }
}

/// How many of the sidecar's lines may wait for this process's stderr before further ones are
/// counted rather than kept.
const LOG_BACKLOG: usize = 1024;

/// The sidecar's lines on their way to this process's stderr, written by one thread of their own. A
/// drain never writes there itself: a stderr whose reader stopped (a full pipe, a terminal paused
/// with Ctrl+S) blocks the write, and a drain blocked in it could not be stopped, so a move would
/// wait on it for good. `None` when that thread could not be started; the lines are then counted.
static LOG: std::sync::OnceLock<Option<std::sync::mpsc::SyncSender<String>>> = std::sync::OnceLock::new();

/// Lines not printed because the backlog was full or there is no printing thread.
static LOG_DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Hands one line to the printing thread without waiting.
fn log_line(line: String) {
    let sender = LOG.get_or_init(|| {
        let (sender, lines) = std::sync::mpsc::sync_channel::<String>(LOG_BACKLOG);
        std::thread::Builder::new()
            .name("sidecar-log".to_string())
            .spawn(move || print_lines(lines))
            .ok()
            .map(|_| sender)
    });
    let sent = sender.as_ref().is_some_and(|sender| sender.try_send(line).is_ok());
    if !sent {
        LOG_DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

fn print_lines(lines: std::sync::mpsc::Receiver<String>) {
    use std::io::Write;
    for line in lines {
        let mut stderr = std::io::stderr().lock();
        let dropped = LOG_DROPPED.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            let _ = writeln!(
                stderr,
                "claude-sidecar: {dropped} line(s) not printed: this process's stderr was not taking them"
            );
        }
        // A stderr that fails (a hung-up terminal) loses the line; it must not end the thread.
        let _ = writeln!(stderr, "{line}");
    }
}

/// The sidecar's three standard descriptors as another process takes them: stdin's write end, and
/// the read ends of stdout and stderr. Each is a duplicate (close-on-exec) of this process's copy.
#[derive(Debug)]
pub struct SidecarFds {
    pub stdin: OwnedFd,
    pub stdout: OwnedFd,
    pub stderr: OwnedFd,
}

impl SidecarFds {
    /// In the order they travel: stdin, stdout, stderr.
    pub fn into_array(self) -> [OwnedFd; 3] {
        [self.stdin, self.stdout, self.stderr]
    }

    pub fn from_array([stdin, stdout, stderr]: [OwnedFd; 3]) -> Self {
        SidecarFds { stdin, stdout, stderr }
    }
}

/// What a sidecar's output handed on carries besides its descriptors.
pub(crate) struct HandedOutput {
    pub(crate) stdout_partial: Vec<u8>,
    pub(crate) stderr_partial: Vec<u8>,
    pub(crate) stderr_tail: Vec<u8>,
}

impl SpawnedSidecar {
    /// The sidecar's most recent stderr lines, oldest first. Not only a failure path: the sidecar
    /// emits a real CLI-compatibility diagnostic here on a successful-but-untested start, which a
    /// UI should be able to show without scraping this process's own stderr.
    pub(crate) fn stderr_tail(&self) -> Vec<String> {
        self.stderr_tail.lock().map(|tail| tail.snapshot()).unwrap_or_default()
    }

    /// The sidecar's OS pid -- the only safe handle for an orphan check (see
    /// `ClaudeSidecarProvider::sidecar_pid`).
    pub(crate) fn pid(&self) -> u32 {
        match &self.process {
            SidecarProcess::Spawned(child) => child.id(),
            #[cfg(target_os = "linux")]
            SidecarProcess::Adopted(pidfd) => pidfd.pid(),
        }
    }

    /// Stops both drains where they are, so that nothing here reads the sidecar's output until
    /// [`Self::restart_output`]. Refused, with the drains running again, when an output has closed:
    /// the sidecar is ending, and there is nothing to hand on.
    pub(crate) fn park_output(&mut self) -> std::io::Result<()> {
        let mut closed = false;
        for output in [&mut self.stdout, &mut self.stderr] {
            *output = match std::mem::replace(output, Output::Closed) {
                Output::Draining(drain) => match drain.stop() {
                    Some(Drained {
                        pipe,
                        partial,
                        ended: false,
                    }) => Output::Parked { pipe, partial },
                    Some(Drained { ended: true, .. }) | None => {
                        closed = true;
                        Output::Closed
                    }
                },
                Output::Parked { pipe, partial } => Output::Parked { pipe, partial },
                Output::Closed => {
                    closed = true;
                    Output::Closed
                }
            };
        }
        if closed {
            self.restart_output()?;
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the sidecar's output has closed",
            ));
        }
        Ok(())
    }

    /// Starts a drain on each parked output, from the line it had in progress.
    pub(crate) fn restart_output(&mut self) -> std::io::Result<()> {
        let tail = Arc::clone(&self.stderr_tail);
        for (output, name, tail) in [
            (&mut self.stdout, "stdout", None),
            (&mut self.stderr, "stderr", Some(tail)),
        ] {
            if let Output::Parked { pipe, partial } = std::mem::replace(output, Output::Closed) {
                *output = Output::Draining(Drain::start(pipe, partial, name, tail)?);
            }
        }
        Ok(())
    }

    /// Duplicates of the three descriptors, for the process taking the sidecar over. Only while the
    /// output is parked: a running drain would go on reading what the other process now reads.
    pub(crate) fn duplicate_fds(&self) -> std::io::Result<SidecarFds> {
        let parked = |output: &Output| match output {
            Output::Parked { pipe, .. } => pipe.try_clone().map(OwnedFd::from),
            _ => Err(std::io::Error::other("the sidecar's output is still read here")),
        };
        let stdin = self
            .stdin_keepalive
            .as_ref()
            .ok_or_else(|| std::io::Error::other("this process no longer holds the sidecar's stdin"))?
            .try_clone()?;
        Ok(SidecarFds {
            stdin: OwnedFd::from(stdin),
            stdout: parked(&self.stdout)?,
            stderr: parked(&self.stderr)?,
        })
    }

    /// The parked lines in progress and the stderr tail.
    pub(crate) fn handed_output(&self) -> HandedOutput {
        let partial = |output: &Output| match output {
            Output::Parked { partial, .. } => partial.clone(),
            _ => Vec::new(),
        };
        HandedOutput {
            stdout_partial: partial(&self.stdout),
            stderr_partial: partial(&self.stderr),
            stderr_tail: self.stderr_tail.lock().map(|tail| tail.to_bytes()).unwrap_or_default(),
        }
    }

    /// Lets go of the sidecar without a side effect: this process stops reading it and closes its
    /// copies, and ends nothing -- no stdin EOF the other holder's copy does not outlive, no signal, no
    /// socket file deleted. The kernel parent is still this process, if it was; dropping a `Child`
    /// neither waits for nor signals it.
    pub(crate) fn let_go(&mut self) {
        self.holds = false;
        self.stdin_keepalive = None;
        for output in [&mut self.stdout, &mut self.stderr] {
            if let Output::Draining(drain) = std::mem::replace(output, Output::Closed) {
                drop(drain.stop());
            }
        }
    }

    /// From here on this process ends the sidecar when it is dropped, and reads its output.
    /// Holding starts even when a drain cannot be started: the move is committed by then, and the
    /// sidecar must still end with this process.
    pub(crate) fn start_holding(&mut self) -> std::io::Result<()> {
        self.holds = true;
        self.restart_output()
    }

    /// A sidecar another process started and hands over, held by nobody here until
    /// [`Self::start_holding`]: until then dropping it only closes the descriptors received.
    #[cfg(target_os = "linux")]
    pub(crate) fn adopted(
        socket_path: PathBuf,
        child: &crate::child::ChildRecord,
        fds: SidecarFds,
        output: HandedOutput,
        build_description: String,
        build_warnings: Vec<String>,
    ) -> std::io::Result<SpawnedSidecar> {
        if child.kind != crate::child::ChildKind::Sidecar {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the process handed over is not a sidecar",
            ));
        }
        let pidfd = crate::child::PidFd::open(child.pid, child.start)?;
        let SidecarFds { stdin, stdout, stderr } = fds;
        let HandedOutput {
            stdout_partial,
            stderr_partial,
            stderr_tail,
        } = output;
        Ok(SpawnedSidecar {
            socket_path,
            build_description,
            build_warnings,
            stdin_keepalive: Some(File::from(stdin)),
            process: SidecarProcess::Adopted(pidfd),
            stdout: Output::Parked {
                pipe: File::from(stdout),
                partial: stdout_partial,
            },
            stderr: Output::Parked {
                pipe: File::from(stderr),
                partial: stderr_partial,
            },
            stderr_tail: Arc::new(Mutex::new(StderrTail::from_bytes(&stderr_tail))),
            holds: false,
        })
    }
}

/// The Verdandi revision this client is developed and verified against (protocol 3.22).
///
/// Not a hard pin -- `EITRI_VERDANDI_CHECKOUT` exists precisely so a Verdandi feature branch or a
/// protocol migration can be tested against an unreleased sidecar, and hard-failing on a different
/// revision would defeat that. It is a BASELINE: when the checkout is at a different revision, that
/// fact is surfaced as a startup diagnostic instead of being silent, so "which sidecar build was
/// this session actually running?" is answerable after the fact rather than guessed at.
///
/// It is also the `rev` that `agent/Cargo.toml` pins `claude-runtime-protocol` to, and the two must
/// stay equal, because the generated wire types this crate compiles against come from exactly that
/// revision, so it is the revision "verified against" can honestly refer to. (The checkout actually
/// RUNNING may differ; that is what the drift warning below reports, and what
/// `EITRI_VERDANDI_CHECKOUT` is for.) `agent/Cargo.toml` names the full commit; this stays the
/// 7-character prefix, because the per-user sidecar directory and the release checks are keyed by
/// exactly that.
///
/// The value earns its way here by the real suite, never by a version bump: the whole `#[ignore]`d
/// real-sidecar set is re-run against the exact pushed revision first (multi-turn with a content
/// oracle, BYPASS tool execution, post-interrupt reuse, orphan-free teardown, a real resume proven
/// by both a content oracle and a provider-session-id match, partial assistant streaming measured
/// before/after, and the replay/recovery and backpressure suites). Move it again on the same
/// terms, not before. When a bump changes no generated type and no capability, it exists only to
/// track the sidecar artifact this client spawns.
///
/// Keep this doc comment and the literal in step. They once drifted apart (the prose named one
/// revision while the constant held another, two revisions apart) and nothing failed.
///
/// **Slated for retirement, and the replacement is already on the wire.** A git revision is a
/// development-machine concept: a packaged install will spawn a shipped sidecar ARTIFACT with no
/// checkout and no `.git` to read, so this check has nothing to compare and the description line
/// above it has nothing to describe. The compatibility token is to become protocol major (already
/// checked at handshake) plus `sidecar_version`, which Verdandi stamps from `package.json` at build
/// time -- both already carried on `ProviderInfo`. Until this client range-checks that version,
/// this constant is what warns about skew, so it is kept accurate rather than deleted early.
///
/// What this client reads from the protocol at this revision, by minor version:
/// - Earlier minors: `SetPermissionMode`, `PermissionModeChanged` and `TextDelta.message_id`;
///   `ClaudeHostPolicy.provider_permission_prompts` (tag 9), `PermissionRequested.origin` with the
///   four `provider_*` fields (tags 5-9), `PermissionOrigin`, `MatchedAskRule`, and capability
///   `provider_permission_prompts`.
/// - 3.14: `ClaudeHostPolicy.cli_permission_mode` (tag 11, `CliPermissionMode`),
///   `ResolvePermissionRequest.defer` (tag 6), `PermissionOutcome.DEFERRED`, the `PermissionDenied`
///   event (`SessionEvent` arm 23); capabilities `cli_auto_mode`, `permission_defer` and
///   `permission_denied_events`. Fields this client does not send or read:
///   `await_account_identity`, `ToolAllowList.init_check`, the handshake's `egress_probe` and
///   `structured_output_tools`, `SessionReady.effective_*`.
/// - 3.20: the `SteerTurn` RPC, the `UserMessageDelivered`, `UserMessageDropped` and
///   `CommandsUpdated` events (`SessionEvent` arms 24-26), error codes `TURN_MISMATCH` and
///   `STEER_UNAVAILABLE`, and `ErrorDetail.reason`; capabilities `steer_turn` (only when the
///   sidecar's CLI is at least 2.1.252) and `list_commands`. Added and not read or sent: the
///   handshake's `cli_compatibility`, `bundled_cli_compatibility` and `effective_claude_config_dir`,
///   `ClaudeHostPolicy.accept_unsupported_cli` (always false), and `SessionReady`'s
///   `effective_setting_sources`, `effective_configuration` and `cli_permission_mode_kind`; seven
///   more capabilities from minors 15-19 this client does not branch on.
/// - 3.21: `CreateSessionRequest.system_prompt_preset` (field 10) and `SystemPromptPreset` with an
///   optional append; the field-7 comments distinguish replacement from the SDK's minimal default.
///   Read: capability `system_prompt_preset`, selecting the preset on fresh and resumed sessions.
/// - 3.22: Claude Code's own turns, background tasks and workflows, the CLI's notices, `/clear`
///   and structured questions, behind ten capabilities. Read: the ten capability flags
///   (`ProviderCapabilities`); the eight new `SessionEvent` arms are received, logged and dropped
///   until each one is folded. Not sent: `ClaudeHostPolicy.per_task_stop`,
///   `CreateSessionRequest.client_interactions`, `typed_by_person` and
///   `SendTurnRequest.client_message_id`. Not called: `StopTask`, `GetTaskOutput`,
///   `ResolveInteraction`, `GetInteractionState`.
pub const EXPECTED_VERDANDI_REVISION: &str = "842d54e";

/// Where `EITRI_VERDANDI_CHECKOUT` came from, and what it points at. Carried onto `ProviderInfo`
/// so the UI can name the backend build it is talking to.
#[derive(Debug)]
pub(crate) struct VerdandiCheckout {
    pub(crate) path: PathBuf,
    /// `git rev-parse --short HEAD`, or `None` when the checkout is not a git repo or `git` is
    /// unavailable. Best-effort diagnostics only -- never a reason to refuse to start.
    pub(crate) revision: Option<String>,
    /// True when `EITRI_VERDANDI_CHECKOUT` chose this path rather than the default.
    pub(crate) from_override: bool,
    /// A release artifact this checkout has ALREADY built for this machine, if any.
    ///
    /// When present it is run directly and `node <dist entry>` is never reached -- so a source tree
    /// and an installed copy execute the same executable rather than two shapes of the same code.
    /// Before this, every sidecar measurement taken on a development machine was of `node` out of a
    /// `dist/` tree while the shipped product ran a Node SEA binary, and nothing said so.
    pub(crate) prebuilt: Option<PathBuf>,
}

/// Where a checkout's own release build leaves its artifacts, relative to the checkout root.
/// Verdandi's `apps/claude-sidecar/scripts/buildBinary.mjs` writes
/// `dist-bin/verdandi-claude-sidecar-<version>-<platform>-<arch>`.
const CHECKOUT_ARTIFACT_DIR: &str = "apps/claude-sidecar/dist-bin";

/// The `<platform>-<arch>` suffix an artifact built for THIS machine carries, in Node's own
/// spelling (`process.platform`-`process.arch`), which is what that build script names files with.
///
/// Load-bearing rather than cosmetic. `dist-bin/` is one directory holding whatever was built last,
/// and a Verdandi checkout can be synced between machines -- this environment already syncs
/// `~/src` to a Mac. Running a `linux-x64` artifact on an arm64 Mac fails with an exec
/// error that names neither the platform nor this decision. `None` on a target this mapping does
/// not know, which simply leaves that machine on the `node <dist>` path rather than guessing.
fn node_platform_suffix() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("linux-x64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        ("macos", "x86_64") => Some("darwin-x64"),
        ("macos", "aarch64") => Some("darwin-arm64"),
        _ => None,
    }
}

/// A release artifact already built inside `checkout`, for this machine, if there is one.
///
/// Newest by mtime when several versions are present, which is the same rule `publish.sh` stages
/// with. Purely a lookup: it never builds anything, which is the whole point -- an artifact that is
/// already there costs nothing, and "run npm first" is exactly what must not happen behind a
/// backend that was chosen for the user.
fn prebuilt_artifact_in(checkout: &Path) -> Option<PathBuf> {
    let suffix = node_platform_suffix()?;
    let prefix = format!("{PACKAGED_SIDECAR_BINARY}-");
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(checkout.join(CHECKOUT_ARTIFACT_DIR)).ok()?.flatten() {
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if !name.starts_with(&prefix) || !name.ends_with(suffix) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else { continue };
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if newest.as_ref().is_none_or(|(best, _)| modified > *best) {
            newest = Some((modified, entry.path()));
        }
    }
    newest.map(|(_, path)| path)
}

/// Where a dev machine's Verdandi checkout is looked for when nothing overrides it, relative to
/// `$HOME`. Named rather than inlined because the packaging scripts carry the same default and
/// nothing but `packaging_scripts_default_to_the_same_checkout_this_code_does` makes them agree --
/// they drifted for real: three of them said `verdandi-old-checkout`, a detached checkout from while
/// Verdandi's protocol-3 merge was outstanding, after their main absorbed it on 2026-09-18.
const DEFAULT_CHECKOUT_UNDER_HOME: &str = "src/verdandi";

/// Locates a real Verdandi checkout.
///
/// `$EITRI_VERDANDI_CHECKOUT` is a **supported development/integration override**, not a
/// temporary hack: it is how this client is pointed at a Verdandi feature branch, a protocol
/// migration, or an isolated checkout while the default one is mid-work. It takes precedence over
/// the default `$HOME/src/verdandi` and is validated the same way, so a typo'd path
/// fails with a clear message rather than silently falling back to a different sidecar than the
/// operator intended.
///
/// Either way this is a dev-machine story, not a production distribution one -- packaging
/// profiles remain separately deferred.
///
/// Takes the override -- and `$HOME` -- as arguments rather than reading
/// either here. `resolve_sidecar_program` already has to know whether an override was supplied, in
/// order to rank it against a packaged artifact, so reading it in both places made the value and the
/// decision two separate sources of truth -- a test could pass one path and this function would use
/// another. Found by exactly that test. `home` joined the same reasoning when step 5 (the default
/// checkout) needed to be reachable in a test without depending on this MACHINE's own
/// `$HOME` -- which, on the machine this was written on, really does have a Verdandi checkout with a
/// real prebuilt artifact underneath it, so a test that read `std::env::var("HOME")` directly would
/// have silently passed against that checkout instead of the fake one it built.
fn locate_verdandi_checkout(explicit: Option<&str>, home: Option<&OsStr>) -> std::io::Result<VerdandiCheckout> {
    let (path, from_override) = match explicit.map(str::trim).filter(|p| !p.is_empty()) {
        Some(path) => (PathBuf::from(path), true),
        None => {
            let home = home.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "HOME is unset and EITRI_VERDANDI_CHECKOUT was not provided",
                )
            })?;
            (PathBuf::from(home).join(DEFAULT_CHECKOUT_UNDER_HOME), false)
        }
    };

    // Validated for BOTH paths now. Previously only the default was checked, so a mistyped override
    // reached `ensure_sidecar_built` and failed somewhere much less informative.
    if !path.join("apps/claude-sidecar/package.json").exists() {
        let origin = if from_override {
            "EITRI_VERDANDI_CHECKOUT points at"
        } else {
            "no Verdandi checkout found at the default"
        };
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "{origin} {path:?}, which has no apps/claude-sidecar/package.json -- \
                 set EITRI_VERDANDI_CHECKOUT to a real Verdandi checkout"
            ),
        ));
    }

    let revision = git_short_revision(&path);
    let prebuilt = prebuilt_artifact_in(&path);
    Ok(VerdandiCheckout {
        path,
        revision,
        from_override,
        prebuilt,
    })
}

/// Best-effort `git rev-parse --short HEAD`. Any failure (not a repo, no `git`, detached weirdness)
/// yields `None` -- this is diagnostics, and a diagnostic that can refuse to start is worse than no
/// diagnostic.
fn git_short_revision(checkout: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(checkout)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let revision = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if revision.is_empty() {
        None
    } else {
        Some(revision)
    }
}

/// One line saying which backend build this session is running, plus a separate list of things
/// genuinely worth warning about.
///
/// The split is load-bearing. The description is unconditional -- there is always a checkout, and a
/// missing git revision degrades to "unknown revision" rather than suppressing the line -- so
/// folding it into the warning list made that list never empty, and any UI that treats "has
/// diagnostics" as "something is wrong" lit up permanently for every healthy session. A warning
/// that is always on is not a warning.
fn describe_checkout(checkout: &VerdandiCheckout) -> (String, Vec<String>) {
    let source = if checkout.from_override {
        "EITRI_VERDANDI_CHECKOUT"
    } else {
        "default path"
    };
    let revision = checkout.revision.as_deref().unwrap_or("unknown revision");
    let shape = match &checkout.prebuilt {
        Some(artifact) => format!(", running its prebuilt artifact {}", artifact.display()),
        None => String::new(),
    };
    let description = format!(
        "Verdandi checkout: {} @ {revision} (via {source}){shape}",
        checkout.path.display()
    );

    let mut warnings = Vec::new();
    // `starts_with` rather than equality: `git rev-parse --short` picks its own abbreviation length,
    // which grows as a repository does, so a strict comparison would start reporting false drift.
    if let Some(actual) = &checkout.revision {
        if !actual.starts_with(EXPECTED_VERDANDI_REVISION) && !EXPECTED_VERDANDI_REVISION.starts_with(actual.as_str()) {
            warnings.push(format!(
                "Verdandi baseline drift: running {actual}, this client was verified against \
                 {EXPECTED_VERDANDI_REVISION}. Not an error -- testing a Verdandi branch is exactly \
                 what EITRI_VERDANDI_CHECKOUT is for -- but if behavior looks wrong, this is the \
                 first thing to check."
            ));
        }
    }
    (description, warnings)
}

/// True if the checkout's root `package.json` declares a `build` script.
///
/// Load-bearing, not a nicety: `apps/claude-sidecar`'s own `build` script does NOT build the
/// `@verdandi/claude-runtime` kernel it imports, and the sidecar resolves that package through a
/// workspace symlink to `packages/claude-runtime/dist`. Building only the app therefore either
/// fails outright (clean checkout: 8x TS2307) or -- worse -- silently succeeds against whatever
/// stale kernel `dist` happens to be present. Verdandi gained a kernel-first root fan-out on
/// 2026-09-11; a checkout predating that has no root script at all, so this probe is what decides
/// between the correct path and the legacy fallback below.
///
/// It prefers `build:claude` over `build` for a reason with a date on it -- see the comment inside.
fn root_build_script_for_claude(checkout: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(checkout.join("package.json")).ok()?;
    let manifest = serde_json::from_str::<serde_json::Value>(&contents).ok()?;
    let scripts = manifest.get("scripts")?;
    // `build:claude` first, deliberately. Verdandi's root `build` is
    // `build:terminal && build:claude`, and the terminal half is a workspace Eitri does not
    // consume and does not build against -- its `node-pty` is a native module a checkout can
    // easily be missing. Running the whole fan-out makes a failure in code this crate never loads
    // into a failure to start the agent panel at all, which is what it did on 2026-09-15:
    // `packages/terminal-runtime` failed `TS2307: Cannot find module 'node-pty'` and `build:claude`
    // never ran. Build what we consume.
    for candidate in ["build:claude", "build"] {
        if scripts.get(candidate).and_then(|b| b.as_str()).is_some() {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Existence check only, not mtime-based staleness (unlike `shell/build.rs`'s own agent-ui
/// pattern) -- this runs at every provider construction (spawn time), not once at `cargo build`
/// time, so walking every source file's mtime on each spawn is a real, avoidable cost. A developer
/// who changes Verdandi's sidecar/kernel source without rebuilding must `rm -rf dist/` themselves
/// to force a rebuild -- a documented manual step, not a silent staleness bug this module hides.
///
/// Both dist entries are checked, not just the sidecar's: a present sidecar `dist` with a missing
/// kernel `dist` is a real, reachable state (they are built by separate `tsc` invocations) and it
/// fails at `node` startup with a module-resolution error rather than anywhere useful.
fn ensure_sidecar_built(checkout: &Path) -> std::io::Result<PathBuf> {
    let sidecar_dir = checkout.join("apps/claude-sidecar");
    let dist_entry = sidecar_dir.join("dist/src/index.js");
    let kernel_dist_entry = checkout.join("packages/claude-runtime/dist/src/index.js");
    if dist_entry.exists() && kernel_dist_entry.exists() {
        return Ok(dist_entry);
    }
    if !checkout.join("node_modules").exists() {
        run_command(checkout, "npm", &["ci"])?;
    }
    if let Some(script) = root_build_script_for_claude(checkout) {
        run_command(checkout, "npm", &["run", &script])?;
    } else {
        // A Verdandi checkout predating the root fan-out. Do the ordering by hand rather than
        // building only the app, which is what silently produced stale-kernel runs.
        eprintln!(
            "agent: {checkout:?} has no root `build` script (it predates Verdandi's kernel-first \
             fan-out); building the kernel and the sidecar separately, in that order"
        );
        run_command(&checkout.join("packages/claude-runtime"), "npm", &["run", "build"])?;
        run_command(&sidecar_dir, "npm", &["run", "build"])?;
    }
    for entry in [&kernel_dist_entry, &dist_entry] {
        if !entry.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("the Verdandi build completed but {entry:?} still does not exist"),
            ));
        }
    }
    Ok(dist_entry)
}

fn run_command(dir: &Path, program: &str, args: &[&str]) -> std::io::Result<()> {
    let status = Command::new(program).args(args).current_dir(dir).status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "{program} {args:?} failed in {dir:?} with status {status}"
        )));
    }
    Ok(())
}

/// The text shown to the user when the sidecar cannot be found at all -- today only when
/// `EITRI_SIDECAR_BINARY` names a path that is not a file, but
/// whoever later turns "no sidecar, no legacy fallback" into a hard error hands this same text to
/// the panel as the tab's failure, rather than writing a second version of it.
///
/// `agent-ui/web/src/problems.ts::classify` recognises this exact opening sentence (its
/// "sidecar missing" row) and splits the rest off as the remedy -- so changing the sentence without
/// updating that marker breaks the panel's headline silently. `problems.test.ts` pins it from the
/// Rust source directly, which is the check that catches it.
pub fn sidecar_missing_message(searched: &[PathBuf]) -> String {
    let mut message = String::from("The agent sidecar is not installed. Looked for it at:\n");
    for path in searched {
        message.push_str(&format!("  - {}\n", path.display()));
    }
    message.push_str(&format!(
        "Install the eitri package that ships it ({PACKAGED_SIDECAR_BINARY}), or point EITRI_SIDECAR_BINARY at a real one."
    ));
    message
}

/// The name a PACKAGED sidecar artifact is installed under, beside this binary.
///
/// Versionless on purpose: Verdandi's release output carries its version and architecture in the
/// file name (`verdandi-claude-sidecar-0.1.0-linux-x64`), which is right for a download and wrong
/// for an installed path -- an install that encoded the version here would make every sidecar
/// upgrade a change to this crate. The compatibility statement lives in the handshake
/// (`protocol_major` plus `sidecar_version`), not in a file name.
const PACKAGED_SIDECAR_BINARY: &str = "verdandi-claude-sidecar";

/// Where the sidecar comes from, resolved once per spawn.
///
/// Two genuinely different shapes, which is why this is an enum rather than a path plus a flag: a
/// packaged artifact is one self-contained executable with no checkout, no `.git` to describe and
/// nothing to build, while the development path is `node <dist entry>` out of a checkout that may
/// need `npm` run first and whose revision is worth reporting.
#[derive(Debug)]
enum SidecarProgram {
    /// A shipped, self-contained executable -- normally a sibling of the running binary, the same
    /// convention `agent-hook`, `eitri-supervisor` and `eitri-tmux-shim` already follow.
    Packaged(PathBuf),
    /// A Verdandi checkout, run through `node`.
    Checkout(VerdandiCheckout),
}

/// Decides which of the two shapes to use, from values the caller supplies rather than from the
/// process environment, so the precedence is testable without mutating it.
///
/// Precedence, and each step earns its place:
/// 1. `EITRI_SIDECAR_BINARY` -- an explicit artifact wins over everything, including a checkout,
///    because someone who names a binary is testing that binary.
/// 2. `EITRI_VERDANDI_CHECKOUT` -- an explicit checkout beats an installed artifact for the same
///    reason in the other direction: a developer pointing at a branch wants that branch, and
///    silently preferring the shipped artifact would make that override look broken. This is also
///    the ONLY branch that may build (`ensure_sidecar_built`, called from `invocation_for`'s
///    `Checkout{prebuilt: None}` arm, which `spawn()` feeds it into) -- naming a checkout is the
///    explicit act that authorizes it.
/// 3. A packaged artifact beside this binary -- the shape a real install has. A sibling may carry an
///    optional `<PACKAGED_SIDECAR_BINARY>.rev` file; `spawn()` compares it against
///    `EXPECTED_VERDANDI_REVISION` and warns on a mismatch (`sibling_rev_skew_warning`), the same
///    warning the checkout path gives for its own drift.
/// 4. **New:** a per-user build at `user_sidecar_path`'s exact rev-keyed location -- what
///    `eitri setup` leaves behind. Keyed by `EXPECTED_VERDANDI_REVISION`, so a build for a
///    DIFFERENT revision, sitting in its own directory, is never found -- never "almost right".
/// 5. The default checkout, **and only when it already holds a prebuilt artifact** for this machine.
///    It never builds: before the sidecar became the default, this branch was reachable only by
///    someone who had already chosen the sidecar backend, so an unprompted `npm ci` behind it cost
///    nothing that was not already asked for. Now an unset backend selects the sidecar, so without
///    this restriction, any
///    user with a Verdandi clone at the default path would get an unprompted build at session start
///    -- the exact "a build must never happen behind a backend nobody chose" invariant this crate
///    already states elsewhere. So a checkout found only here, with nothing prebuilt, is treated the
///    same as no checkout at all: resolution falls through to the shared `NO_SIDECAR_HINT` error
///    rather than returning a `Checkout` that would reach `ensure_sidecar_built`. This is why every
///    `Checkout` this function returns with `prebuilt: None` carries `from_override: true` -- nothing
///    below step 2 can produce the other combination, so `invocation_for`'s build arm can keep
///    trusting that shape without re-checking it.
///
/// Nothing found by any of the five: `Err`, naming `NO_SIDECAR_HINT` plus the exact path a
/// `eitri setup` run would use (`no_sidecar_message`).
fn resolve_sidecar_program(
    explicit_binary: Option<&str>,
    explicit_checkout: Option<&str>,
    exe_dir: Option<&Path>,
    xdg_data_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> std::io::Result<SidecarProgram> {
    if let Some(binary) = explicit_binary.map(str::trim).filter(|b| !b.is_empty()) {
        let path = PathBuf::from(binary);
        if !path.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                sidecar_missing_message(std::slice::from_ref(&path)),
            ));
        }
        return Ok(SidecarProgram::Packaged(path));
    }
    if explicit_checkout.map(str::trim).is_some_and(|c| !c.is_empty()) {
        return Ok(SidecarProgram::Checkout(locate_verdandi_checkout(
            explicit_checkout,
            home,
        )?));
    }
    if let Some(sibling) = exe_dir.map(|d| d.join(PACKAGED_SIDECAR_BINARY)).filter(|p| p.is_file()) {
        return Ok(SidecarProgram::Packaged(sibling));
    }
    if let Some(user_path) = user_sidecar_path(xdg_data_home, home, EXPECTED_VERDANDI_REVISION).filter(|p| p.is_file())
    {
        return Ok(SidecarProgram::Packaged(user_path));
    }
    // Step 5: the default checkout counts only when it already holds a prebuilt artifact. A
    // missing or invalid checkout (`Err`, e.g. no `HOME`, or no `apps/claude-sidecar/package.json`)
    // is swallowed here rather than propagated -- it means exactly the same thing as a present
    // checkout with nothing built: there is nothing to run without building, and building here is
    // exactly what must not happen.
    if let Ok(checkout) = locate_verdandi_checkout(None, home) {
        if checkout.prebuilt.is_some() {
            return Ok(SidecarProgram::Checkout(checkout));
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        no_sidecar_message(xdg_data_home, home),
    ))
}

/// What a session start will find, as far as backend selection's one startup line cares
/// (`eitri_core::agent_backend::BackendKind::choose`).
///
/// **Correction (2026-09-27): this
/// decides nothing any more.** It was `packaged_sidecar_available() -> bool`, and it decided the
/// default backend when `EITRI_AGENT_BACKEND` was unset: `true` selected the sidecar, `false` fell
/// back to legacy. Legacy is compiled out of every release now and is never a fallback, so every
/// build selects the sidecar whatever this answers, and the answer only picks the sentence `choose`
/// prints. It became three-way, ranked exactly like `resolve_sidecar_program`, because the `bool`
/// made that sentence read "not installed. Run eitri setup" to a developer whose named
/// `EITRI_VERDANDI_CHECKOUT` the spawn was about to build. The paragraphs below are why
/// [`SidecarAvailability::Runnable`] means "with nothing to build"; the backend they name as the
/// fallback is the one that no longer is.
///
/// **Corrected 2026-09-18, and the correction is the point.** This first read "a shipped artifact,
/// and a Verdandi CHECKOUT deliberately does not count", reasoned from: every developer machine has
/// a checkout, and counting it would switch every source build to a backend whose first start runs
/// `npm ci` and a TypeScript build with no UI saying why. The conclusion was right and the object
/// was wrong. What must not happen behind a backend nobody chose is **a build**, not a checkout --
/// and a checkout that already holds a built artifact for this machine involves no build at all. So
/// a checkout counts exactly when `prebuilt_artifact_in` finds one, and a checkout with nothing
/// built still does not count, which keeps the original guarantee intact where it was actually
/// about something.
///
/// It buys a second thing the first version gave away: with this, a source build and an installed
/// copy run the SAME executable. Before, `cargo run` ran `node` out of a `dist/` tree while the
/// package ran a Node SEA binary, so a developer-machine measurement was never of the shipped shape.
///
/// **Widened again:** a per-user build at `user_sidecar_path`'s location counts too,
/// for the same reason the checkout case does -- running it involves no build. Without it, a machine
/// that had already run `eitri setup` would have fallen back to legacy at backend-selection time
/// (before backend selection printed a line), and would now print the "not installed" line above a
/// session that then starts fine from step 4.
pub fn sidecar_availability() -> SidecarAvailability {
    // Read once and pass in, not once per call site: `checkout_path_for` and
    // `sidecar_availability_in` both need `HOME`, and reading it twice (a `var_os` here and a `var`
    // inside `checkout_path_for`) would make them two independent sources of truth that could
    // disagree on a non-UTF-8 `HOME` -- `var` fails closed on one, `var_os` never does.
    let home = std::env::var_os("HOME");
    let explicit_checkout = std::env::var("EITRI_VERDANDI_CHECKOUT").ok();
    let checkout_is_explicit = explicit_checkout
        .as_deref()
        .map(str::trim)
        .is_some_and(|c| !c.is_empty());
    sidecar_availability_in(
        std::env::var("EITRI_SIDECAR_BINARY").ok().as_deref(),
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
            .as_deref(),
        checkout_path_for(explicit_checkout.as_deref(), home.as_deref()).as_deref(),
        checkout_is_explicit,
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        home.as_deref(),
    )
}

/// [`sidecar_availability`]'s answer. Decides nothing (see that function's correction): every value
/// selects the sidecar backend, and each only picks the line `BackendKind::choose` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidecarAvailability {
    /// Something runs with nothing to build: a named `EITRI_SIDECAR_BINARY` that is a file, the
    /// packaged sibling, the per-user build, or a checkout holding a prebuilt artifact.
    Runnable,
    /// `EITRI_VERDANDI_CHECKOUT` names a checkout with nothing prebuilt for this machine, so the
    /// first session start builds it there (`npm`, discovery step 2) -- naming a checkout is the act
    /// that authorizes that build. Only an explicit checkout can be this; the default one never
    /// builds (step 5).
    BuildsNamedCheckout,
    /// Nothing to run: a session start fails with [`NO_SIDECAR_HINT`] (or, for a named binary that
    /// is not a file, `sidecar_missing_message`).
    Missing,
}

/// [`sidecar_availability`] with its inputs passed in, so the matrix is testable without mutating
/// the process environment -- the same reason `BackendKind::choose` takes the answer as an
/// argument rather than calling this.
///
/// **Ranked exactly like `resolve_sidecar_program`**: the first step that applies
/// decides, so the startup line and the spawn cannot describe different sidecars. This used to be an
/// OR over the same inputs, which answered "available" for an explicit checkout with nothing built
/// beside an installed artifact while the spawn built that checkout; that was harmless while the
/// answer only chose the default backend, and wrong once it chose the words. `checkout` is
/// `checkout_path_for`'s answer and `checkout_is_explicit` says whether it came from
/// `EITRI_VERDANDI_CHECKOUT` (step 2) or the default location (step 5).
fn sidecar_availability_in(
    named_binary: Option<&str>,
    exe_dir: Option<&Path>,
    checkout: Option<&Path>,
    checkout_is_explicit: bool,
    xdg_data_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> SidecarAvailability {
    let prebuilt = |path: &Path| prebuilt_artifact_in(path).is_some();
    // Step 1: a named binary wins, and `resolve_sidecar_program` refuses one that is not a file.
    if let Some(binary) = named_binary.map(str::trim).filter(|b| !b.is_empty()) {
        return if Path::new(binary).is_file() {
            SidecarAvailability::Runnable
        } else {
            SidecarAvailability::Missing
        };
    }
    // Step 2: a named checkout wins over everything below it, built or not.
    if checkout_is_explicit {
        return match checkout {
            Some(path) if prebuilt(path) => SidecarAvailability::Runnable,
            _ => SidecarAvailability::BuildsNamedCheckout,
        };
    }
    // Steps 3-5: the sibling, the per-user build, a default checkout holding a prebuilt artifact.
    let sibling = exe_dir.is_some_and(|dir| dir.join(PACKAGED_SIDECAR_BINARY).is_file());
    let user_build =
        user_sidecar_path(xdg_data_home, home, EXPECTED_VERDANDI_REVISION).is_some_and(|path| path.is_file());
    if sibling || user_build || checkout.is_some_and(prebuilt) {
        SidecarAvailability::Runnable
    } else {
        SidecarAvailability::Missing
    }
}

/// The checkout path that would be used, override first, `$HOME` default second. Unvalidated on
/// purpose: this feeds a "does an artifact exist" probe, and a path that is not a checkout simply
/// holds no artifact. Refusing belongs in `locate_verdandi_checkout`, where it can say so.
///
/// Takes `home` rather than reading `$HOME` itself, for the same reason `locate_verdandi_checkout`
/// does: its one caller (`sidecar_availability`) already reads it once for
/// `sidecar_availability_in`, and reading it a second time here would let the two disagree on a
/// non-UTF-8 `HOME` (`std::env::var` fails on one; `var_os` never does).
fn checkout_path_for(explicit: Option<&str>, home: Option<&OsStr>) -> Option<PathBuf> {
    if let Some(path) = explicit.map(str::trim).filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(path));
    }
    home.map(|home| PathBuf::from(home).join(DEFAULT_CHECKOUT_UNDER_HOME))
}

/// `$XDG_DATA_HOME/eitri/sidecar/<rev>/verdandi-claude-sidecar`: where a
/// sidecar this MACHINE built for itself, with `eitri setup`, lives -- keyed by the exact revision
/// it was built for so a build for a different revision is never picked up as "close enough" (the
/// binary looks only in its own rev's directory). Falls back to
/// `<home>/.local/share/eitri/sidecar/<rev>/verdandi-claude-sidecar` when `XDG_DATA_HOME` is
/// unset, empty or not an absolute path -- the same three-case rule
/// `core::layout::persist::state_subdir` uses for `XDG_STATE_HOME` (`state_dir`, that module's own
/// call site). Reproduced here rather than shared: `core` depends on `agent`, never the reverse, so
/// this crate cannot reach that helper. `None` when neither variable gives an absolute directory to
/// build on, rather than silently resolving against this process's cwd -- the same "no boundary, no
/// answer" shape `core::layout::persist::state_subdir`/`state_dir` give a relative or absent `$HOME`
/// (cited above). **Not** `state_dirs::conversations_dir`: that function accepts any set `$HOME`,
/// relative included, and refuses only when `$HOME` is unset entirely -- an earlier revision of
/// this comment claimed otherwise.
pub fn user_sidecar_path(xdg_data_home: Option<&OsStr>, home: Option<&OsStr>, rev: &str) -> Option<PathBuf> {
    let base = match xdg_data_home {
        Some(data_home) if Path::new(data_home).is_absolute() => PathBuf::from(data_home),
        _ => match home {
            Some(home) if Path::new(home).is_absolute() => PathBuf::from(home).join(".local/share"),
            _ => return None,
        },
    };
    Some(base.join("eitri/sidecar").join(rev).join(PACKAGED_SIDECAR_BINARY))
}

/// The fixed hint printed whenever the sidecar backend is selected -- unconditionally, now that it is
/// the default -- but nothing this client can run is found by any of the five discovery steps.
/// `pub` and shared verbatim with `eitri_core::agent_backend::BackendKind::choose` (imported
/// rather than copied), which prints exactly this line at backend-selection time,
/// before any resolution or spawn is even attempted -- so a user sees the identical words whichever
/// moment actually catches the failure.
///
/// **Starts with `sidecar_missing_message`'s own opening sentence, "The agent sidecar is not
/// installed.", on purpose.** Both functions describe the same
/// fact -- no sidecar this client can run -- and
/// `agent-ui/web/src/problems.ts::classifySidecarMissing` recognises that exact sentence anywhere in
/// a tab's failure text, splitting the rest off as the shown remedy; `problems.test.ts` pins the
/// sentence from this file's own source. With a different opening sentence, step 5's
/// fallthrough -- a fresh public-release user with nothing built, the most common failure --
/// would reach the panel as unrecognised raw text with no headline or remedy at all.
/// `sidecar_missing_message`'s own remedy ("Install the Eitri package that ships it") is specific
/// to that function's one call site (an explicitly named `EITRI_SIDECAR_BINARY` that is not a
/// file) and would be the wrong remedy to show here even with the sentence matching, which is why
/// this reuses the sentence rather than the whole function.
///
/// Deliberately does NOT carry the filesystem path an `eitri setup` run would use: that path needs
/// `$XDG_DATA_HOME`/`$HOME`, readable only at runtime, and this is a `const`. Nor does it carry
/// `EXPECTED_VERDANDI_REVISION` -- `concat!` takes literal tokens, not a `const` identifier, so
/// splicing the revision in here would mean a second string-literal copy of it, a second source of
/// truth. `no_sidecar_message` (below) is a runtime `format!` and appends both when they can be
/// computed; `BackendKind::choose`, which has no specific resolution attempt to report a path for, prints
/// this constant alone.
pub const NO_SIDECAR_HINT: &str = "The agent sidecar is not installed. Run \"eitri setup\" -- it builds one for you.";

/// The full message for `resolve_sidecar_program`'s last resort (step 5 finding no prebuilt
/// checkout, after steps 1-4 already found nothing): `NO_SIDECAR_HINT` plus the pinned revision and
/// the exact path `eitri setup` would write to -- the same path `user_sidecar_path` (step 4) just
/// looked at and did not find. Falls back to naming what is missing, rather than silently omitting
/// the path, when neither `$XDG_DATA_HOME` nor `$HOME` gives that function anywhere to answer.
fn no_sidecar_message(xdg_data_home: Option<&OsStr>, home: Option<&OsStr>) -> String {
    match user_sidecar_path(xdg_data_home, home, EXPECTED_VERDANDI_REVISION) {
        Some(path) => format!(
            "{NO_SIDECAR_HINT} (verdandi {EXPECTED_VERDANDI_REVISION}, into {})",
            path.display()
        ),
        None => format!(
            "{NO_SIDECAR_HINT} (verdandi {EXPECTED_VERDANDI_REVISION}, but cannot say where -- neither \
             $XDG_DATA_HOME nor $HOME is a usable absolute path)"
        ),
    }
}

/// Everything the sidecar child's process image is, in one place a test can read back.
///
/// `VERDANDI_CLAUDE_ACCOUNT` is the whole of the account handoff (2026-09-21). Verdandi's
/// `packages/claude-runtime/src/account.ts` resolves it **once for the sidecar process** and then
/// sets the full `CLAUDE_PROFILE`/`CLAUDE_CONFIG_DIR`/`CLAUDE_SECURESTORAGE_CONFIG_DIR`/
/// `ANTHROPIC_CONFIG_DIR` tuple on every `claude` it spawns -- overriding whatever this process
/// inherited. So Eitri hands over the *name* and lets the sidecar derive the rest: writing the
/// tuple here as well would be a second copy of a convention that already lives on one side, and
/// the copy that drifted would win silently on some hosts and lose on others. There is no proto
/// field for this and none is needed -- it is a property of the process Eitri itself starts.
///
/// With no account configured this sets exactly what it always set, and the sidecar's own account
/// support does not engage at all (`VERDANDI_CLAUDE_ACCOUNT` unset is its shipped default).
fn sidecar_command(
    program_path: &Path,
    program_args: &[PathBuf],
    socket_path: &Path,
    account: Option<&crate::account::ClaudeAccount>,
) -> Command {
    let keep_test_seams = std::env::var_os(TEST_SEAMS_VAR).is_some_and(|v| v == "1");
    let bash_max_timeout = std::env::var_os("BASH_MAX_TIMEOUT_MS");
    sidecar_command_for(
        program_path,
        program_args,
        socket_path,
        account,
        keep_test_seams,
        bash_max_timeout.as_deref(),
    )
}

fn sidecar_command_for(
    program_path: &Path,
    program_args: &[PathBuf],
    socket_path: &Path,
    account: Option<&crate::account::ClaudeAccount>,
    keep_test_seams: bool,
    bash_max_timeout: Option<&OsStr>,
) -> Command {
    let mut command = Command::new(program_path);
    command
        .args(program_args)
        .env("VERDANDI_CLAUDE_SIDECAR_SOCKET", socket_path)
        .stdin(Stdio::piped())
        // stdout/stderr are captured, not nulled -- nulling a spawned child's stderr silently
        // discards real crash diagnostics (found on `supervisor`'s spawn, 2026-09-09). A Node.js
        // sidecar crash during real CLI parity testing is exactly the kind of thing worth seeing.
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A group of its own: a `Ctrl+C` in, or the hangup of, the terminal that started this window
        // goes to this window's group and must not reach a sidecar another window may hold by then.
        // Its end is its stdin's EOF, which every holder's end brings about, a crash included.
        .process_group(0)
        // The sidecar merges this variable's MCP servers into every session it serves. Eitri states
        // which servers a session gets and states none today, so a value left in the user's shell
        // must not turn into servers (and tools) in an Eitri session.
        .env_remove(EXTRA_MCP_SERVERS_VAR)
        // `none` makes the sidecar ignore end-of-file on its stdin, which is how it learns that the
        // window holding the other end is gone, so a window killed with SIGKILL would leave it
        // running; any other value than `stdin` or `none` fails every start. The window states
        // nothing here, so the sidecar's own default (`stdin`) is what applies.
        .env_remove(PARENT_WATCH_VAR);
    // Verdandi's fault-injection and sizing knobs change how a session behaves (dropped watches, a
    // one-event ring); a value left in a shell must not reach a window's sidecar. The real-sidecar
    // tests that set them on purpose also set `TEST_SEAMS_VAR`.
    if !keep_test_seams {
        for var in TEST_SEAM_VARS {
            command.env_remove(var);
        }
    }
    if let Some(account) = account {
        command.env("VERDANDI_CLAUDE_ACCOUNT", account.name());
    }
    // Raise the ceiling for explicitly timed commands while keeping the CLI's default timeout.
    // A value supplied by the user takes precedence, including an empty or non-UTF-8 value.
    command.env(
        "BASH_MAX_TIMEOUT_MS",
        bash_max_timeout.unwrap_or(OsStr::new("2147483647")),
    );
    command
}

/// Verdandi's test seams: they are inherited by the sidecar only when [`TEST_SEAMS_VAR`] is `1`.
const TEST_SEAM_VARS: [&str; 3] = [
    "VERDANDI_CLAUDE_SIDECAR_RING_CAPACITY",
    "VERDANDI_CLAUDE_SIDECAR_FAULT_DROP_WATCH_AFTER",
    "VERDANDI_CLAUDE_SIDECAR_WRITE_QUEUE_STATS_MS",
];

/// Set by the real-sidecar tests that pass a seam on purpose; never set by a window.
const TEST_SEAMS_VAR: &str = "EITRI_SIDECAR_TEST_SEAMS";

/// How the sidecar notices that its parent is gone; never inherited from the shell.
const PARENT_WATCH_VAR: &str = "VERDANDI_CLAUDE_SIDECAR_PARENT_WATCH";

/// The sidecar's process-level list of extra MCP servers (JSON); never inherited from the shell.
const EXTRA_MCP_SERVERS_VAR: &str = "VERDANDI_CLAUDE_EXTRA_MCP_SERVERS";

/// True when `path` is exactly discovery step 3's candidate -- `current_exe()`'s sibling -- rather
/// than an explicitly named `EITRI_SIDECAR_BINARY` (step 1) or the per-user rev-keyed path (step
/// 4). Compared by path instead of adding a third `SidecarProgram::Packaged` shape to every match
/// arm and existing test in this file, for a distinction only `sibling_rev_skew_warning`'s call site
/// needs.
fn is_exe_sibling_artifact(path: &Path, exe_dir: Option<&Path>) -> bool {
    exe_dir.map(|dir| dir.join(PACKAGED_SIDECAR_BINARY)).as_deref() == Some(path)
}

/// The drift warning a step-3 sibling's own `<PACKAGED_SIDECAR_BINARY>.rev` file gives: a sibling
/// may carry an optional `verdandi-claude-sidecar.rev` file; when present
/// and it does not start with `EXPECTED_VERDANDI_REVISION`, the spawn adds the same skew warning the
/// checkout path gives. Absent, or a prefix match either direction (the same rule
/// checkout path gives"). Absent, or a prefix match either direction (the same rule
/// `describe_checkout` uses for its own git-revision drift check, since `git rev-parse --short`'s
/// abbreviation length grows with a repository): no warning, the common case for a real install --
/// the package writes a matching `.rev` file, or none at all on an install predating this file.
fn sibling_rev_skew_warning(artifact_path: &Path) -> Vec<String> {
    let Some(dir) = artifact_path.parent() else {
        return Vec::new();
    };
    let Ok(actual) = std::fs::read_to_string(dir.join(format!("{PACKAGED_SIDECAR_BINARY}.rev"))) else {
        return Vec::new();
    };
    let actual = actual.trim();
    if actual.is_empty()
        || actual.starts_with(EXPECTED_VERDANDI_REVISION)
        || EXPECTED_VERDANDI_REVISION.starts_with(actual)
    {
        return Vec::new();
    }
    vec![format!(
        "Verdandi baseline drift: the sidecar beside this binary was built for {actual}, this client \
         was verified against {EXPECTED_VERDANDI_REVISION}. Not an error -- but if behavior looks \
         wrong, this is the first thing to check."
    )]
}

/// Turns a resolved `SidecarProgram` into what `spawn()` actually executes -- description, drift
/// warnings, the program path and its args -- building a checkout that has nothing prebuilt through
/// the injected `build` closure rather than calling `ensure_sidecar_built` (or `npm`) directly.
///
/// This seam exists so the wiring `spawn()` used to have inline
/// -- "a `Checkout` with `prebuilt: None` reaches `ensure_sidecar_built`" -- is itself a unit-tested
/// fact rather than an untested implementation detail. Before this, `resolve_sidecar_program`
/// returning `Err` for a present-but-unbuilt *default* checkout (step 5) was tested, but nothing
/// pinned that a `Checkout{prebuilt: None}` that DOES reach this point (only possible from the
/// explicit `EITRI_VERDANDI_CHECKOUT` branch, step 2) still triggers a build -- a later edit to
/// `spawn()`'s wiring could have silently stopped building for that case, or started building for a
/// case that should not, with no test failing either way. The tests on this function now assert
/// both directions with a fake closure, no real `npm`/`node` and no process-env mutation.
fn invocation_for(
    program: SidecarProgram,
    exe_dir: Option<&Path>,
    build: impl FnOnce(&Path) -> std::io::Result<PathBuf>,
) -> std::io::Result<(String, Vec<String>, PathBuf, Vec<PathBuf>)> {
    Ok(match program {
        SidecarProgram::Packaged(path) => {
            // Only a discovery-step-3 sibling of THIS binary carries an optional `.rev` file to
            // check -- an explicitly named `EITRI_SIDECAR_BINARY` was chosen on
            // purpose, and the per-user path (step 4) is already keyed by rev in its own directory
            // name, so neither needs this warning.
            let warnings = if is_exe_sibling_artifact(&path, exe_dir) {
                sibling_rev_skew_warning(&path)
            } else {
                Vec::new()
            };
            (
                format!("packaged sidecar artifact: {}", path.display()),
                warnings,
                path,
                Vec::new(),
            )
        }
        SidecarProgram::Checkout(checkout) => {
            let (description, warnings) = describe_checkout(&checkout);
            match checkout.prebuilt {
                // The same executable the package ships, found already built in the checkout. Taken
                // over `node <dist>` deliberately: a developer machine then runs the shape that
                // ships, and nothing here has to start `npm`.
                Some(artifact) => (description, warnings, artifact, Vec::new()),
                // `build` runs only on this path. A packaged artifact has nothing to build, and
                // reaching for a build step there would be the bug this branch exists to avoid.
                None => {
                    let dist_entry = build(&checkout.path)?;
                    (description, warnings, PathBuf::from("node"), vec![dist_entry])
                }
            }
        }
    })
}

/// Spawns a fresh sidecar for one `ClaudeSidecarProvider` instance. `instance_id` becomes part of
/// the socket path (`crate::socket_path::sidecar_socket`, which mirrors the legacy backend's own
/// per-conversation UUID socket and keeps both under macOS's 103-byte socket-path limit) so
/// multiple concurrent providers never collide on one path.
pub(crate) fn spawn(instance_id: &str) -> std::io::Result<SpawnedSidecar> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let xdg_data_home = std::env::var_os("XDG_DATA_HOME");
    let home = std::env::var_os("HOME");
    let program = resolve_sidecar_program(
        std::env::var("EITRI_SIDECAR_BINARY").ok().as_deref(),
        std::env::var("EITRI_VERDANDI_CHECKOUT").ok().as_deref(),
        exe_dir.as_deref(),
        xdg_data_home.as_deref(),
        home.as_deref(),
    )?;
    let (build_description, build_warnings, program_path, program_args) =
        invocation_for(program, exe_dir.as_deref(), ensure_sidecar_built)?;
    spawn_resolved(
        instance_id,
        build_description,
        build_warnings,
        &program_path,
        &program_args,
    )
}

/// Spawns `program` as the sidecar, with no build step and no search: the crate's own tests start
/// a stand-in this way.
#[cfg(feature = "test-support")]
pub(crate) fn spawn_program(instance_id: &str, program: &Path) -> std::io::Result<SpawnedSidecar> {
    spawn_resolved(
        instance_id,
        format!("stand-in sidecar: {}", program.display()),
        Vec::new(),
        program,
        &[],
    )
}

fn spawn_resolved(
    instance_id: &str,
    build_description: String,
    build_warnings: Vec<String>,
    program_path: &Path,
    program_args: &[PathBuf],
) -> std::io::Result<SpawnedSidecar> {
    eprintln!("agent: {build_description}");
    for line in &build_warnings {
        eprintln!("agent: {line}");
    }
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    let socket_dir = crate::socket_path::private_socket_dir(
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).as_deref(),
        &std::env::temp_dir(),
        euid,
    )?;
    let socket_path = crate::socket_path::sidecar_socket(&socket_dir, instance_id)?;
    let _ = std::fs::remove_file(&socket_path); // stale leftover from a prior crash, if any

    let account = crate::account::configured();
    if let Some(account) = account {
        eprintln!(
            "agent: sidecar pinned to claude account '{}' ({})",
            account.name(),
            account.config_dir().display()
        );
    }
    let mut command = sidecar_command(program_path, program_args, &socket_path, account);

    let mut child = command.spawn()?;
    let (Some(stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(std::io::Error::other("the sidecar's pipes were not created"));
    };
    let stdin_keepalive = Some(File::from(OwnedFd::from(stdin)));

    let stderr_tail = Arc::new(Mutex::new(StderrTail::default()));
    let drains = Drain::start(File::from(OwnedFd::from(stdout)), Vec::new(), "stdout", None).and_then(|stdout| {
        Drain::start(
            File::from(OwnedFd::from(stderr)),
            Vec::new(),
            "stderr",
            Some(Arc::clone(&stderr_tail)),
        )
        .map(|stderr| (stdout, stderr))
    });
    let (stdout, stderr) = match drains {
        Ok(drains) => drains,
        Err(e) => {
            drop(stdin_keepalive);
            let _ = stop_child(&mut child, SIDECAR_EXIT_GRACE);
            return Err(e);
        }
    };

    let started_waiting = std::time::Instant::now();
    loop {
        if std::os::unix::net::UnixStream::connect(&socket_path).is_ok() {
            return Ok(SpawnedSidecar {
                socket_path,
                build_description,
                build_warnings,
                stdin_keepalive,
                process: SidecarProcess::Spawned(child),
                stdout: Output::Draining(stdout),
                stderr: Output::Draining(stderr),
                stderr_tail,
                holds: true,
            });
        }
        // Check for a dead child BEFORE sleeping again. The sidecar fails closed on a policy
        // violation (an incompatible Claude CLI, a socket already in use) by throwing before it
        // ever binds, so the process is simply gone -- waiting out the remaining retries would turn
        // an immediate, fully-explained failure into a multi-second silent timeout. This is exactly
        // how a real CLI-version refusal used to reach the UI as `did not bind ... within 1000ms`.
        if let Ok(Some(status)) = child.try_wait() {
            drop(stdin_keepalive);
            // The drain threads may still be flushing the final lines the child wrote on its way
            // out; those lines are the entire diagnosis, so give them a moment to land rather than
            // reporting an empty cause.
            std::thread::sleep(Duration::from_millis(100));
            return Err(std::io::Error::other(describe_startup_failure(
                &format!("claude-sidecar exited with {status} before binding {socket_path:?}"),
                &stderr_tail,
            )));
        }
        if started_waiting.elapsed() >= SIDECAR_BIND_BUDGET {
            break;
        }
        std::thread::sleep(SIDECAR_BIND_POLL);
    }
    // Still running but never bound -- clean up rather than leaking a half-started process, then
    // report the failure with whatever it managed to say for itself.
    drop(stdin_keepalive);
    let _ = child.kill();
    let _ = child.wait();
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        describe_startup_failure(&bind_timeout_summary(&socket_path, SIDECAR_BIND_BUDGET), &stderr_tail),
    ))
}

/// Builds a startup-failure message that actually names the cause, by appending whatever the
/// sidecar printed to stderr before dying. Without this the caller gets a transport-shaped symptom
/// ("did not bind") for what is usually a policy-shaped cause ("refusing to start: claude CLI
/// version 2.1.269 ...") -- and the real message exists only in this process's own stderr, where no
/// UI can reach it.
fn describe_startup_failure(summary: &str, stderr_tail: &Arc<Mutex<StderrTail>>) -> String {
    let lines = stderr_tail.lock().map(|tail| tail.snapshot()).unwrap_or_default();
    if lines.is_empty() {
        return format!("{summary} (the sidecar printed nothing to stderr)");
    }
    format!("{summary}; its stderr said:\n{}", lines.join("\n"))
}

/// Polls `child` for up to `grace`, then escalates to SIGKILL if it hasn't exited on its own.
/// Extracted out of `SpawnedSidecar::drop` so the grace/kill/give-up shape is independently
/// testable against a real child process rather than only through a whole `SpawnedSidecar`.
///
/// Genuinely bounded either way: within `grace` if the child exits on its own, or near-instantly
/// once `grace` elapses and this falls through to `kill()` + `wait()` -- never an unbounded wait
/// that could hang the thread dropping it.
pub(crate) fn stop_child(child: &mut Child, grace: Duration) -> std::io::Result<ExitStatus> {
    let deadline = std::time::Instant::now() + grace;
    loop {
        match child.try_wait()? {
            Some(status) => return Ok(status),
            None if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            None => break,
        }
    }
    kill_group_or_child(child)?;
    child.wait()
}

/// SIGKILL to the group `child` leads, which the sidecar does (it is started in a group of its own):
/// a sidecar killed alone would leave the `claude` it runs, and that CLI's own children, running
/// with nobody to end them. A child that leads no group of its own is killed alone. The child is not
/// reaped yet, so its pid, and the group named by it, are still its own.
fn kill_group_or_child(child: &mut Child) -> std::io::Result<()> {
    let pid = child.id() as libc::pid_t;
    // SAFETY: `kill` takes plain integers; `-pid` names the process group whose id is the child's pid.
    if unsafe { libc::kill(-pid, libc::SIGKILL) } == 0 {
        return Ok(());
    }
    child.kill()
}

impl Drop for SpawnedSidecar {
    /// Graceful-first shutdown: close the held-open stdin (the sidecar's own documented
    /// parent-death signal, verified for real), wait up to
    /// `SIDECAR_EXIT_GRACE` (long enough for the sidecar to reap the `claude` CLI it owns before
    /// exiting -- a graceful exit lets the sidecar fail-close any pending permissions server-side
    /// before this process moves on), then escalate to SIGKILL if it still hasn't exited --
    /// the same graceful-then-forced escalation every child here gets. Deliberately simpler than
    /// `panel/src/supervisor_client.rs::connect_or_spawn`'s own background-thread reaper: that
    /// pattern exists there because a *detached* supervisor process may run for an unbounded time
    /// after being spawned, so its eventual `wait()` has no natural bound to block on. Here, by the
    /// time `drop` runs, the sidecar is either already exiting gracefully (bounded by
    /// `SIDECAR_EXIT_GRACE`) or about to be SIGKILLed (which returns near-instantly) -- both paths
    /// are genuinely bounded, so blocking `Drop` itself for at most `SIDECAR_EXIT_GRACE` is a
    /// deliberate, bounded wait, not an unbounded one.
    ///
    /// Only by the sidecar's holder. A process that handed the sidecar on, or took it over and never
    /// committed, closes its copies of the descriptors as the fields drop, and ends nothing.
    fn drop(&mut self) {
        if !self.holds {
            return;
        }
        drop(self.stdin_keepalive.take()); // closes the write end -> sidecar sees stdin EOF
        match &mut self.process {
            SidecarProcess::Spawned(child) => {
                let _ = stop_child(child, SIDECAR_EXIT_GRACE);
            }
            #[cfg(target_os = "linux")]
            SidecarProcess::Adopted(pidfd) => end_adopted(pidfd, SIDECAR_EXIT_GRACE),
        }
        // The directory outlives windows, so a killed sidecar's file would otherwise pile up in it.
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// [`stop_child`] for a sidecar this process took over: its stdin is already closed by the caller;
/// waits up to `grace` on the pidfd, then SIGKILL through it. Nothing is reaped here: the status is
/// the kernel parent's to read (the window that started the sidecar, or init once it is gone).
#[cfg(target_os = "linux")]
pub(crate) fn end_adopted(pidfd: &crate::child::PidFd, grace: Duration) {
    if !pidfd.wait_exited(grace) {
        // The group, as for a sidecar this process started, so its CLI goes with it.
        let _ = pidfd.signal_group(libc::SIGKILL);
        let _ = pidfd.wait_exited(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {

    /// An explicit artifact wins over everything, including an explicit checkout.
    ///
    /// The precedence matters more than it looks: these two overrides select DIFFERENT SHAPES, not
    /// two spellings of one, so a wrong order is not a tie-break -- it silently runs `node` out of a
    /// tree when someone asked for a binary, or the reverse. Resolution takes its inputs as
    /// arguments precisely so this is checkable without mutating the process environment, which is
    /// shared by every other test in this binary.
    #[test]
    fn an_explicit_binary_beats_an_explicit_checkout() {
        let artifact = std::env::temp_dir().join(format!("nv-sidecar-{}", uuid::Uuid::new_v4()));
        std::fs::write(&artifact, b"#!/bin/sh\nexit 0\n").unwrap();
        let resolved = resolve_sidecar_program(artifact.to_str(), Some("/definitely/not/a/checkout"), None, None, None)
            .expect("an existing artifact resolves");
        assert!(
            matches!(&resolved, SidecarProgram::Packaged(p) if *p == artifact),
            "{resolved:?}"
        );
        let _ = std::fs::remove_file(&artifact);
    }

    /// A named artifact that does not exist is a hard error, not a silent fall-through to a
    /// checkout. Falling through would run a DIFFERENT build than the one named and report success,
    /// which is the failure mode every override in this crate is written to avoid.
    #[test]
    fn a_named_artifact_that_is_missing_fails_rather_than_falling_back() {
        let err = resolve_sidecar_program(Some("/no/such/sidecar/binary"), None, None, None, None)
            .expect_err("a named artifact that is absent must fail");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(err.to_string().contains("EITRI_SIDECAR_BINARY"), "{err}");
        // `sidecar_missing_message` builds this error text. Pin the searched path here so changes
        // to either the error path or its wording cannot silently omit it.
        assert!(err.to_string().contains("The agent sidecar is not installed."), "{err}");
        assert!(err.to_string().contains("/no/such/sidecar/binary"), "{err}");
    }

    /// The missing-sidecar message names every searched path and the package that ships it.
    /// `agent-ui/web/src/problems.ts::classify` splits that package hint off as the remedy.
    #[test]
    fn sidecar_missing_message_names_every_searched_path_and_the_package() {
        let searched = [PathBuf::from("/a/b/sidecar"), PathBuf::from("/c/d/sidecar")];
        let message = sidecar_missing_message(&searched);
        assert!(message.starts_with("The agent sidecar is not installed."), "{message}");
        assert!(message.contains("/a/b/sidecar"), "{message}");
        assert!(message.contains("/c/d/sidecar"), "{message}");
        assert!(message.contains(PACKAGED_SIDECAR_BINARY), "{message}");
        assert!(message.contains("EITRI_SIDECAR_BINARY"), "{message}");
    }

    /// A sibling artifact is used when nothing was named -- the shape a real install has, and the
    /// same `current_exe()`-sibling convention `agent-hook` and `eitri-supervisor` already follow.
    #[test]
    fn a_sibling_artifact_is_found_when_nothing_is_named() {
        let dir = std::env::temp_dir().join(format!("nv-sidecar-dir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let artifact = dir.join(PACKAGED_SIDECAR_BINARY);
        std::fs::write(&artifact, b"#!/bin/sh\nexit 0\n").unwrap();
        let resolved = resolve_sidecar_program(None, None, Some(&dir), None, None).expect("the sibling resolves");
        assert!(
            matches!(&resolved, SidecarProgram::Packaged(p) if *p == artifact),
            "{resolved:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An explicit checkout beats a sibling artifact. A developer pointing at a branch wants that
    /// branch; preferring the shipped artifact would make `EITRI_VERDANDI_CHECKOUT` look broken
    /// on exactly the machines where both exist -- which, once Eitri ships the artifact, is every
    /// developer machine.
    #[test]
    fn an_explicit_checkout_beats_a_sibling_artifact() {
        let dir = std::env::temp_dir().join(format!("nv-sidecar-dir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(PACKAGED_SIDECAR_BINARY), b"#!/bin/sh\nexit 0\n").unwrap();
        // Points at a directory that is not a checkout, so this resolves to the Checkout ARM and
        // then fails inside it -- which is the observation: the sibling was not chosen.
        let err = resolve_sidecar_program(None, Some("/definitely/not/a/checkout"), Some(&dir), None, None)
            .expect_err("an explicit checkout that is not one must fail rather than silently using the sibling");
        assert!(
            !err.to_string().contains(PACKAGED_SIDECAR_BINARY),
            "the sibling artifact was used despite an explicit checkout: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An empty override is treated as absent. `EITRI_SIDECAR_BINARY=` in a wrapper script is a
    /// way of saying "not set", and honouring it literally would fail every spawn with a path that
    /// is the empty string.
    #[test]
    fn an_empty_override_is_not_an_override() {
        let dir = std::env::temp_dir().join(format!("nv-sidecar-dir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let artifact = dir.join(PACKAGED_SIDECAR_BINARY);
        std::fs::write(&artifact, b"#!/bin/sh\nexit 0\n").unwrap();
        let resolved = resolve_sidecar_program(Some("   "), Some(""), Some(&dir), None, None)
            .expect("blank overrides fall through to the sibling");
        assert!(
            matches!(&resolved, SidecarProgram::Packaged(p) if *p == artifact),
            "{resolved:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_bind_budget_outlasts_a_cold_start_and_the_timeout_names_it() {
        // A cold start measured over 3 s; the budget must clear that with room, and stay a bound.
        assert!(SIDECAR_BIND_BUDGET >= Duration::from_secs(10));
        assert!(SIDECAR_BIND_BUDGET <= Duration::from_secs(30));
        assert!(SIDECAR_BIND_POLL <= Duration::from_millis(100));
        let message = bind_timeout_summary(std::path::Path::new("/run/x/endpoint"), SIDECAR_BIND_BUDGET);
        assert!(message.contains("within 15000ms"), "{message}");
        assert!(message.contains("did not bind"), "{message}");
    }

    /// A sidecar with the `set_permission_mode` capability (protocol 3; an older one does not reap) reaps its CLIs
    /// before exiting -- 250 ms, SIGTERM, SIGKILL 3 s later, give up 1 s after that: ~4.3 s worst case. Killing it
    /// inside that window orphans the `claude` it was ending.
    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn the_grace_covers_the_sidecars_own_reap() {
        assert!(super::SIDECAR_EXIT_GRACE >= Duration::from_millis(250 + 3000 + 1000 + 500));
    }

    #[test]
    fn a_child_that_exits_inside_the_grace_is_not_killed() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "read _; sleep 0.3; exit 7"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        drop(child.stdin.take());
        let status = super::stop_child(&mut child, Duration::from_secs(2)).unwrap();
        assert_eq!(status.code(), Some(7), "exited on its own: {status:?}");
    }

    #[test]
    fn a_child_still_running_after_the_grace_is_killed() {
        use std::os::unix::process::ExitStatusExt;
        let mut child = std::process::Command::new("sh")
            .args(["-c", "trap '' TERM; read _; sleep 30"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        drop(child.stdin.take());
        let started = std::time::Instant::now();
        let status = super::stop_child(&mut child, Duration::from_millis(200)).unwrap();
        assert_eq!(status.signal(), Some(9));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// A sidecar that will not end is killed with its group, so what it started goes too.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_child_killed_after_the_grace_takes_its_group_with_it() {
        use std::io::BufRead;
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("sh")
            .args(["-c", "trap '' TERM; sleep 30 & echo $!; read _; sleep 30"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let grandchild: libc::pid_t = line.trim().parse().unwrap();
        drop(child.stdin.take());
        super::stop_child(&mut child, Duration::from_millis(200)).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let state = std::fs::read_to_string(format!("/proc/{grandchild}/stat"))
                .ok()
                .and_then(|stat| stat.rsplit(')').next().map(|rest| rest.trim_start().chars().next()))
                .flatten();
            if matches!(state, None | Some('Z') | Some('X')) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the sidecar's own child {grandchild} still runs"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    use super::*;

    /// Spawn the actual compiled sidecar and confirm it binds its socket and exits when the
    /// held-open stdin closes. No CLI turn is sent, so this incurs no API cost.
    ///
    /// The default checkout must already be runnable: a bare default checkout fails with
    /// `NO_SIDECAR_HINT` instead of building on demand. A prebuilt artifact or user-path build
    /// is runnable; only an explicit `EITRI_VERDANDI_CHECKOUT` can select an unbuilt checkout
    /// and build it through the explicit branch.
    #[test]
    #[ignore]
    fn spawn_binds_the_socket_and_stdin_close_shuts_it_down_cleanly() {
        let instance_id = uuid::Uuid::new_v4().to_string();
        let mut sidecar = spawn(&instance_id).expect("spawn should succeed against a real Verdandi checkout");
        assert!(std::os::unix::net::UnixStream::connect(&sidecar.socket_path).is_ok());

        drop(sidecar.stdin_keepalive.take());
        let child = match &mut sidecar.process {
            SidecarProcess::Spawned(child) => child,
            #[cfg(target_os = "linux")]
            SidecarProcess::Adopted(_) => unreachable!("spawn starts the sidecar here"),
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut exited = false;
        while std::time::Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                exited = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(exited, "sidecar did not exit within 5s of its stdin being closed");
    }

    /// Packaging scripts that name a default checkout must agree with the resolver. A detached
    /// `~/src/verdandi-old-checkout` default once differed from this file's
    /// `~/src/verdandi`; with both directories present a launcher guard could pass
    /// against one while the binary read the other. Compare literal text so this works even
    /// where neither directory exists.
    ///
    /// `packaging/eitri.launcher.sh` names no default checkout: it finds an explicit override,
    /// a packaged sibling or a revision-keyed user install, or prints a setup hint.
    /// The root `install.sh` is a wrapper over `packaging/install.sh --from-source --checkout`
    /// and passes `--verdandi-checkout` only when `EITRI_VERDANDI_CHECKOUT` is set. Neither script
    /// has a guessed default to compare. Without an override the installer uses pinned public
    /// Verdandi source rather than a path specific to one machine.
    #[test]
    fn packaging_scripts_default_to_the_same_checkout_this_code_does() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("agent/ has a parent");
        let expected = format!("${{EITRI_VERDANDI_CHECKOUT:-$HOME/{DEFAULT_CHECKOUT_UNDER_HOME}}}");

        for script in [
            "try-eitri.sh",
            // publish.sh (the private deploy script) does not ship publicly.
        ] {
            let path = repo_root.join(script);
            let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            let uses = text
                .lines()
                .filter(|l| l.contains("EITRI_VERDANDI_CHECKOUT:-"))
                .collect::<Vec<_>>();
            // At least one, and EVERY one agreeing -- not "exactly one". An exact count went red
            // when `install.sh` legitimately gained a second use (one in the artifact-copying
            // helper, one in the checkout guard below it). "Exactly
            // one" was only ever a way of making `uses[0]` cover the whole file; checking all of
            // them covers it properly, and does not stand in the way of a script that honestly
            // needs the default twice. What must not happen -- a script defaulting somewhere this
            // code does not -- is still caught, now on every line rather than the first.
            assert!(!uses.is_empty(), "{script} never names the default Verdandi checkout");
            for line in &uses {
                assert!(
                    line.contains(&expected),
                    "{script} defaults the Verdandi checkout somewhere this code does not:\n  \
                     script: {}\n  expected to contain: {expected}",
                    line.trim(),
                );
            }
        }
    }

    /// The other half of the change above: `eitri.launcher.sh` does not merely stop AGREEING
    /// with this default -- it stops naming `EITRI_VERDANDI_CHECKOUT` as a default for anything
    /// at all. A stray `EITRI_VERDANDI_CHECKOUT:-...` line re-added there (e.g. by a bad merge
    /// with an older revision of this file) would silently reintroduce the exact drift the test
    /// above exists to catch, one script at a time -- while itself passing, since a script this
    /// test no longer scans can drift freely.
    #[test]
    fn the_launcher_no_longer_defaults_the_verdandi_checkout() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("agent/ has a parent");
        let path = repo_root.join("packaging/eitri.launcher.sh");
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        assert!(
            !text.contains("EITRI_VERDANDI_CHECKOUT:-"),
            "packaging/eitri.launcher.sh still defaults EITRI_VERDANDI_CHECKOUT to a path"
        );
    }

    /// The root `install.sh` wraps `packaging/install.sh --from-source --checkout`, passing
    /// `--verdandi-checkout` only if `EITRI_VERDANDI_CHECKOUT` is set. A guessed default such as
    /// `~/src/verdandi` would fail on other machines and could disagree with this
    /// resolver. `${EITRI_VERDANDI_CHECKOUT:-}` is an empty-string presence check and is allowed.
    #[test]
    fn the_root_install_sh_no_longer_defaults_the_verdandi_checkout() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("agent/ has a parent");
        let path = repo_root.join("install.sh");
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        assert!(
            !text.contains(&format!("EITRI_VERDANDI_CHECKOUT:-$HOME/{DEFAULT_CHECKOUT_UNDER_HOME}"))
                && !text.contains("EITRI_VERDANDI_CHECKOUT:-$HOME"),
            "the root install.sh still defaults EITRI_VERDANDI_CHECKOUT to a guessed path"
        );
        assert!(
            text.contains("--from-source") && text.contains("--checkout"),
            "the root install.sh no longer looks like packaging/install.sh --from-source --checkout's wrapper"
        );
    }

    /// Builds a directory that `locate_verdandi_checkout` accepts, optionally holding artifacts.
    /// Returns the checkout root; the caller removes it.
    fn fake_checkout(artifacts: &[&str]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("nv-checkout-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("apps/claude-sidecar")).unwrap();
        std::fs::write(root.join("apps/claude-sidecar/package.json"), b"{}").unwrap();
        if !artifacts.is_empty() {
            std::fs::create_dir_all(root.join(CHECKOUT_ARTIFACT_DIR)).unwrap();
        }
        for name in artifacts {
            std::fs::write(root.join(CHECKOUT_ARTIFACT_DIR).join(name), b"#!/bin/sh\nexit 0\n").unwrap();
        }
        root
    }

    /// A fake `$HOME` whose `src/verdandi` is a checkout `locate_verdandi_checkout`
    /// accepts (mirroring `fake_checkout`, but nested at the exact path step 5's default-checkout
    /// lookup uses), optionally holding artifacts. Returns the home root; the caller removes it.
    ///
    /// Exists because this development machine's REAL `$HOME/src/verdandi` really does
    /// hold a prebuilt sidecar artifact -- so a test of "what happens with no user override and the
    /// real default" that read `std::env::var("HOME")` directly would pass against the wrong
    /// checkout on this machine and differently on a fresh one. `locate_verdandi_checkout` and
    /// `resolve_sidecar_program` take `home` as a parameter for exactly this reason.
    fn fake_home_with_default_checkout(artifacts: &[&str]) -> PathBuf {
        let home = std::env::temp_dir().join(format!("nv-home-{}", uuid::Uuid::new_v4()));
        let checkout = home.join(DEFAULT_CHECKOUT_UNDER_HOME);
        std::fs::create_dir_all(checkout.join("apps/claude-sidecar")).unwrap();
        std::fs::write(checkout.join("apps/claude-sidecar/package.json"), b"{}").unwrap();
        if !artifacts.is_empty() {
            std::fs::create_dir_all(checkout.join(CHECKOUT_ARTIFACT_DIR)).unwrap();
        }
        for name in artifacts {
            std::fs::write(checkout.join(CHECKOUT_ARTIFACT_DIR).join(name), b"#!/bin/sh\nexit 0\n").unwrap();
        }
        home
    }

    /// A platform suffix this machine is definitely not, so a test can plant an artifact that must
    /// be ignored. Derived from the real mapping rather than hardcoded, so it stays wrong-on-purpose
    /// on every target instead of only on x86_64 Linux.
    fn foreign_platform_suffix() -> &'static str {
        match node_platform_suffix() {
            Some("darwin-arm64") => "linux-x64",
            _ => "darwin-arm64",
        }
    }

    /// The correction this predicate carries in its own doc, as a test: a checkout that has ALREADY
    /// built an artifact for this machine makes the sidecar startable, because running it involves
    /// no build. Before 2026-09-18 this answered `false` and every source build silently stayed on
    /// the legacy backend -- which is the backend with no partial streaming, so the product's most
    /// visible complaint was reachable by the default on the machine its author uses.
    #[test]
    fn a_checkout_holding_a_built_artifact_counts_as_available() {
        let suffix = node_platform_suffix().expect("this test target has a known Node platform name");
        let root = fake_checkout(&[&format!("{PACKAGED_SIDECAR_BINARY}-0.1.0-{suffix}")]);
        assert_eq!(
            sidecar_availability_in(None, None, Some(&root), false, None, None),
            SidecarAvailability::Runnable
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// And the half of the original guarantee that was actually about something: a checkout with
    /// nothing built still does not count. Counting it would put the first start of a source build
    /// into `npm ci` and a TypeScript build, with no UI saying why, behind a backend nobody chose.
    #[test]
    fn a_checkout_with_nothing_built_is_not_availability() {
        let root = fake_checkout(&[]);
        assert_eq!(
            sidecar_availability_in(None, None, Some(&root), false, None, None),
            SidecarAvailability::Missing
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// An artifact built for a different machine is not an artifact. `dist-bin/` holds whatever was
    /// built last and this environment syncs `~/src` to a Mac, so a `linux-x64` binary
    /// really can be sitting in an arm64 machine's checkout. Picking it would fail at exec with a
    /// message naming neither the platform nor this decision.
    #[test]
    fn an_artifact_built_for_another_platform_is_neither_picked_nor_counted() {
        let root = fake_checkout(&[&format!(
            "{PACKAGED_SIDECAR_BINARY}-0.1.0-{}",
            foreign_platform_suffix()
        )]);
        assert_eq!(prebuilt_artifact_in(&root), None);
        assert_eq!(
            sidecar_availability_in(None, None, Some(&root), false, None, None),
            SidecarAvailability::Missing
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Several versions in one `dist-bin/` resolve to the newest, the same rule `publish.sh` stages
    /// with -- so a rebuild takes effect without anyone cleaning the directory first.
    #[test]
    fn the_newest_artifact_wins_when_a_checkout_holds_several() {
        let suffix = node_platform_suffix().expect("this test target has a known Node platform name");
        let older = format!("{PACKAGED_SIDECAR_BINARY}-0.1.0-{suffix}");
        let newer = format!("{PACKAGED_SIDECAR_BINARY}-0.2.0-{suffix}");
        let root = fake_checkout(&[&older, &newer]);
        // Written in one loop above, so mtimes can tie at this filesystem's resolution. Make the
        // ordering real rather than assuming creation order survived.
        let newer_path = root.join(CHECKOUT_ARTIFACT_DIR).join(&newer);
        filetime_bump(&newer_path);
        assert_eq!(prebuilt_artifact_in(&root), Some(newer_path));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Rewrites `path` so its mtime is strictly later than its siblings', without depending on the
    /// clock: `std::fs::write` stamps the current time, and a second write after a real sleep is the
    /// only portable way to guarantee a difference on a coarse-resolution filesystem.
    fn filetime_bump(path: &Path) {
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
    }

    /// The whole reason for the widening: a checkout with a built artifact runs THAT, not `node` out
    /// of a `dist/` tree, so a developer machine executes the same binary the package ships.
    #[test]
    fn a_checkout_with_a_built_artifact_resolves_to_running_it() {
        let suffix = node_platform_suffix().expect("this test target has a known Node platform name");
        let artifact_name = format!("{PACKAGED_SIDECAR_BINARY}-0.1.0-{suffix}");
        let root = fake_checkout(&[&artifact_name]);
        let resolved = resolve_sidecar_program(None, root.to_str(), None, None, None).expect("the checkout resolves");
        let SidecarProgram::Checkout(checkout) = resolved else {
            panic!("an explicit checkout must resolve to the checkout shape: {resolved:?}");
        };
        assert_eq!(
            checkout.prebuilt,
            Some(root.join(CHECKOUT_ARTIFACT_DIR).join(&artifact_name))
        );
        // And it says so, because "which of the two shapes ran" is not otherwise visible.
        let (description, _) = describe_checkout(&checkout);
        assert!(description.contains("prebuilt artifact"), "{description}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A checkout with nothing built keeps the `node <dist>` shape, and says nothing about an
    /// artifact -- the negative control for the assertion above.
    #[test]
    fn a_checkout_with_nothing_built_still_describes_the_node_shape() {
        let root = fake_checkout(&[]);
        let resolved = resolve_sidecar_program(None, root.to_str(), None, None, None).expect("the checkout resolves");
        let SidecarProgram::Checkout(checkout) = resolved else {
            panic!("{resolved:?}")
        };
        assert_eq!(checkout.prebuilt, None);
        let (description, _) = describe_checkout(&checkout);
        assert!(!description.contains("prebuilt"), "{description}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Reads the account back off the real `Command` the real spawn path builds -- the env map, not
    /// a stand-in for it -- so deleting the `VERDANDI_CLAUDE_ACCOUNT` line fails here.
    #[test]
    fn a_configured_account_reaches_the_sidecar_child_as_one_named_variable() {
        let account =
            crate::account::ClaudeAccount::resolve_from("work", |k| (k == "HOME").then(|| "/home/user".to_string()))
                .unwrap();
        // Built through `socket_path`, not spelled out, so this test cannot become the one place
        // in the crate that invents a socket path (`socket_path`'s own scanner enforces that).
        let socket = crate::socket_path::sidecar_socket(Path::new("/tmp"), "account-test").unwrap();
        let command = sidecar_command(
            Path::new("/usr/lib/eitri/verdandi-claude-sidecar"),
            &[],
            &socket,
            Some(&account),
        );
        let env: Vec<(String, Option<String>)> = command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().to_string(),
                    v.map(|v| v.to_string_lossy().to_string()),
                )
            })
            .collect();
        assert!(
            env.contains(&("VERDANDI_CLAUDE_ACCOUNT".to_string(), Some("work".to_string()))),
            "{env:?}"
        );
        assert!(
            env.contains(&(
                "VERDANDI_CLAUDE_SIDECAR_SOCKET".to_string(),
                Some(socket.to_string_lossy().to_string())
            )),
            "{env:?}"
        );
        // The tuple stays Verdandi's to derive; a second copy here is what would drift.
        for derived in [
            "CLAUDE_PROFILE",
            "CLAUDE_CONFIG_DIR",
            "CLAUDE_SECURESTORAGE_CONFIG_DIR",
            "ANTHROPIC_CONFIG_DIR",
        ] {
            assert!(
                !env.iter().any(|(k, _)| k == derived),
                "{derived} should not be set here"
            );
        }
    }

    #[test]
    fn the_sidecar_gets_the_raised_background_ceiling() {
        for keep_test_seams in [false, true] {
            let command = sidecar_command_for(
                Path::new("/usr/lib/eitri/verdandi-claude-sidecar"),
                &[],
                Path::new("/never/bound"),
                None,
                keep_test_seams,
                None,
            );
            let env: Vec<_> = command.get_envs().collect();
            assert!(env.contains(&(OsStr::new("BASH_MAX_TIMEOUT_MS"), Some(OsStr::new("2147483647")))));
            assert!(!env.iter().any(|(key, _)| *key == "BASH_DEFAULT_TIMEOUT_MS"));
        }
    }

    #[test]
    fn a_user_value_of_the_ceiling_is_kept() {
        use std::os::unix::ffi::OsStrExt;

        for value in [OsStr::new("9000000"), OsStr::new(""), OsStr::from_bytes(b"\xff")] {
            let command = sidecar_command_for(
                Path::new("/usr/lib/eitri/verdandi-claude-sidecar"),
                &[],
                Path::new("/never/bound"),
                None,
                false,
                Some(value),
            );
            let env: Vec<_> = command.get_envs().collect();
            assert!(env.contains(&(OsStr::new("BASH_MAX_TIMEOUT_MS"), Some(value))));
            assert!(!env.iter().any(|(key, _)| *key == "BASH_DEFAULT_TIMEOUT_MS"));
        }
    }

    /// `get_envs` reports a removal as `(key, None)`; an inherited value is gone only if that entry
    /// exists, and it must survive an account being set too.
    #[test]
    fn the_extra_mcp_servers_variable_is_removed_from_the_sidecar_child() {
        let socket = crate::socket_path::sidecar_socket(Path::new("/tmp"), "account-test").unwrap();
        let account =
            crate::account::ClaudeAccount::resolve_from("work", |k| (k == "HOME").then(|| "/home/user".to_string()))
                .unwrap();
        for account in [None, Some(&account)] {
            let command = sidecar_command(
                Path::new("/usr/lib/eitri/verdandi-claude-sidecar"),
                &[],
                &socket,
                account,
            );
            let removed: Vec<_> = command
                .get_envs()
                .filter(|(k, _)| k.to_string_lossy() == "VERDANDI_CLAUDE_EXTRA_MCP_SERVERS")
                .collect();
            assert_eq!(removed.len(), 1, "{removed:?}");
            assert!(removed[0].1.is_none(), "it must be removed, not set: {removed:?}");
        }
    }

    /// An inherited `none` would make the sidecar ignore its stdin closing, so it would outlive a
    /// window that was killed; the removal holds with the test switch on too.
    #[test]
    fn the_parent_watch_variable_is_removed_from_the_sidecar_child_in_every_case() {
        let socket = crate::socket_path::sidecar_socket(Path::new("/tmp"), "watch-test").unwrap();
        let program = Path::new("/usr/lib/eitri/verdandi-claude-sidecar");
        for keep in [false, true] {
            let command = sidecar_command_for(program, &[], &socket, None, keep, None);
            let removed: Vec<_> = command
                .get_envs()
                .filter(|(k, _)| k.to_string_lossy() == PARENT_WATCH_VAR)
                .collect();
            assert_eq!(removed.len(), 1, "keep_test_seams={keep}: {removed:?}");
            assert!(removed[0].1.is_none(), "it must be removed, not set: {removed:?}");
        }
        assert_eq!(PARENT_WATCH_VAR, "VERDANDI_CLAUDE_SIDECAR_PARENT_WATCH");
    }

    /// The seams are removed from the child unless the test switch is on, in which case they are
    /// left to be inherited.
    #[test]
    fn verdandis_test_seams_are_removed_from_the_sidecar_child_unless_a_test_asks() {
        let socket = crate::socket_path::sidecar_socket(Path::new("/tmp"), "seam-test").unwrap();
        let program = Path::new("/usr/lib/eitri/verdandi-claude-sidecar");
        let removed = |keep: bool| -> Vec<String> {
            sidecar_command_for(program, &[], &socket, None, keep, None)
                .get_envs()
                .filter(|(_, v)| v.is_none())
                .map(|(k, _)| k.to_string_lossy().into_owned())
                .collect()
        };
        let by_default = removed(false);
        for var in TEST_SEAM_VARS {
            assert!(
                by_default.iter().any(|k| k == var),
                "{var} must be removed: {by_default:?}"
            );
        }
        let kept = removed(true);
        for var in TEST_SEAM_VARS {
            assert!(
                !kept.iter().any(|k| k == var),
                "{var} must be inherited on request: {kept:?}"
            );
        }
        assert!(
            kept.iter().any(|k| k == EXTRA_MCP_SERVERS_VAR),
            "the switch never frees the MCP list"
        );
    }

    /// Without an account the child sets only the socket and timeout ceiling, besides removing
    /// inherited knobs that would change the sidecar's behaviour.
    #[test]
    fn with_no_account_the_child_sets_the_socket_and_ceiling_and_removes_the_inherited_knobs() {
        let socket = crate::socket_path::sidecar_socket(Path::new("/tmp"), "account-test").unwrap();
        let command = sidecar_command_for(
            Path::new("/usr/lib/eitri/verdandi-claude-sidecar"),
            &[],
            &socket,
            None,
            false,
            None,
        );
        let keys: Vec<String> = command
            .get_envs()
            .map(|(k, _)| k.to_string_lossy().to_string())
            .collect();
        let mut keys = keys;
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "BASH_MAX_TIMEOUT_MS".to_string(),
                "VERDANDI_CLAUDE_EXTRA_MCP_SERVERS".to_string(),
                "VERDANDI_CLAUDE_SIDECAR_FAULT_DROP_WATCH_AFTER".to_string(),
                "VERDANDI_CLAUDE_SIDECAR_PARENT_WATCH".to_string(),
                "VERDANDI_CLAUDE_SIDECAR_RING_CAPACITY".to_string(),
                "VERDANDI_CLAUDE_SIDECAR_SOCKET".to_string(),
                "VERDANDI_CLAUDE_SIDECAR_WRITE_QUEUE_STATS_MS".to_string(),
            ]
        );
    }

    // -- The per-user sidecar is keyed by the pinned revision so builds do not share artifacts. --

    /// An absolute `$XDG_DATA_HOME` wins outright when locating the user install.
    #[test]
    fn user_sidecar_path_prefers_an_absolute_xdg_data_home() {
        let path = user_sidecar_path(Some(OsStr::new("/data")), Some(OsStr::new("/home/x")), "abc1234")
            .expect("an absolute XDG_DATA_HOME resolves");
        assert_eq!(
            path,
            Path::new("/data/eitri/sidecar/abc1234").join(PACKAGED_SIDECAR_BINARY)
        );
    }

    /// And falls back to `<home>/.local/share` when `$XDG_DATA_HOME` is unset, empty or relative --
    /// the same three cases `core::layout::persist::state_subdir` handles for `XDG_STATE_HOME`.
    #[test]
    fn user_sidecar_path_falls_back_to_home_local_share() {
        for unusable in [None, Some(OsStr::new("")), Some(OsStr::new("relative/dir"))] {
            let path = user_sidecar_path(unusable, Some(OsStr::new("/home/x")), "abc1234")
                .unwrap_or_else(|| panic!("home should still resolve for {unusable:?}"));
            assert_eq!(
                path,
                Path::new("/home/x/.local/share/eitri/sidecar/abc1234").join(PACKAGED_SIDECAR_BINARY),
                "{unusable:?}"
            );
        }
    }

    /// Neither variable gives an absolute directory to build on: `None`, never a path resolved
    /// against this process's own cwd.
    #[test]
    fn user_sidecar_path_is_none_when_neither_variable_is_usable() {
        assert_eq!(user_sidecar_path(None, None, "abc1234"), None);
        assert_eq!(
            user_sidecar_path(
                Some(OsStr::new("relative")),
                Some(OsStr::new("also/relative")),
                "abc1234"
            ),
            None
        );
        assert_eq!(
            user_sidecar_path(Some(OsStr::new("")), None, "abc1234"),
            None,
            "an empty XDG_DATA_HOME with no HOME must not resolve"
        );
    }

    /// Revision-keyed directories keep different builds apart so a newer client cannot mistake
    /// another revision's artifact for its own.
    #[test]
    fn user_sidecar_path_is_keyed_by_rev() {
        let a = user_sidecar_path(Some(OsStr::new("/data")), None, "aaaaaaa").unwrap();
        let b = user_sidecar_path(Some(OsStr::new("/data")), None, "bbbbbbb").unwrap();
        assert_ne!(a, b);
        assert!(a.to_string_lossy().contains("aaaaaaa"), "{a:?}");
        assert!(b.to_string_lossy().contains("bbbbbbb"), "{b:?}");
    }

    /// Step 4: a per-user build at the exact rev-keyed path this client expects is found when
    /// nothing earlier in the precedence (steps 1-3) matches.
    #[test]
    fn resolve_sidecar_program_finds_the_user_path_when_nothing_else_matches() {
        let xdg_data_home = std::env::temp_dir().join(format!("nv-xdg-{}", uuid::Uuid::new_v4()));
        let artifact = user_sidecar_path(Some(xdg_data_home.as_os_str()), None, EXPECTED_VERDANDI_REVISION)
            .expect("an absolute XDG_DATA_HOME resolves");
        std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        std::fs::write(&artifact, b"#!/bin/sh\nexit 0\n").unwrap();

        let resolved = resolve_sidecar_program(None, None, None, Some(xdg_data_home.as_os_str()), None)
            .expect("the user path resolves");
        assert!(
            matches!(&resolved, SidecarProgram::Packaged(p) if *p == artifact),
            "{resolved:?}"
        );
        let _ = std::fs::remove_dir_all(&xdg_data_home);
    }

    /// A user-built sidecar for a DIFFERENT revision is never found -- the whole reason step 4 is
    /// keyed by `EXPECTED_VERDANDI_REVISION`: a build from a previous release must be invisible,
    /// never "almost right", so a stale sidecar cannot silently keep running against a newer client.
    /// Isolated from step 5 by pointing `home` at an empty directory with no default checkout.
    #[test]
    fn a_user_sidecar_built_for_another_rev_is_never_found() {
        let xdg_data_home = std::env::temp_dir().join(format!("nv-xdg-other-rev-{}", uuid::Uuid::new_v4()));
        let other_rev_artifact = user_sidecar_path(Some(xdg_data_home.as_os_str()), None, "deadbee")
            .expect("a fake other-rev path resolves");
        std::fs::create_dir_all(other_rev_artifact.parent().unwrap()).unwrap();
        std::fs::write(&other_rev_artifact, b"#!/bin/sh\nexit 0\n").unwrap();

        let home = std::env::temp_dir().join(format!("nv-home-empty-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();

        let err = resolve_sidecar_program(
            None,
            None,
            None,
            Some(xdg_data_home.as_os_str()),
            Some(home.as_os_str()),
        )
        .expect_err("a build for a different revision must be invisible");
        assert!(err.to_string().contains(NO_SIDECAR_HINT), "{err}");
        assert!(
            !err.to_string().contains("deadbee"),
            "the wrong-rev artifact must never be named as the answer: {err}"
        );

        let _ = std::fs::remove_dir_all(&xdg_data_home);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The default checkout counts only when it already holds a prebuilt
    /// artifact. One present but unbuilt must not be picked up as a build opportunity -- resolution
    /// ends in the shared `NO_SIDECAR_HINT` rather than reaching for `npm`. `resolve_sidecar_program`
    /// itself calls neither `ensure_sidecar_built` nor `Command::new("npm")` in any of its branches
    /// (see its own doc comment), so this is a structural guarantee this test pins, not a timing one
    /// that could flake.
    #[test]
    fn the_default_checkout_never_builds_when_it_is_present_but_unbuilt() {
        let home = fake_home_with_default_checkout(&[]);
        let err = resolve_sidecar_program(None, None, None, None, Some(home.as_os_str()))
            .expect_err("an unbuilt default checkout must not resolve to something runnable");
        assert!(err.to_string().contains(NO_SIDECAR_HINT), "{err}");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A packaged sibling beats a per-user build for this exact revision when both exist.
    /// Testing both candidates together catches a swapped priority that isolated candidates miss.
    #[test]
    fn a_sibling_artifact_beats_a_user_build_for_the_same_revision() {
        let dir = std::env::temp_dir().join(format!("nv-sidecar-dir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let sibling = dir.join(PACKAGED_SIDECAR_BINARY);
        std::fs::write(&sibling, b"#!/bin/sh\nexit 0\n").unwrap();
        let xdg_data_home = std::env::temp_dir().join(format!("nv-xdg-sibling-{}", uuid::Uuid::new_v4()));
        let user_build = user_sidecar_path(Some(xdg_data_home.as_os_str()), None, EXPECTED_VERDANDI_REVISION)
            .expect("an absolute XDG_DATA_HOME resolves");
        std::fs::create_dir_all(user_build.parent().unwrap()).unwrap();
        std::fs::write(&user_build, b"#!/bin/sh\nexit 0\n").unwrap();

        let resolved = resolve_sidecar_program(None, None, Some(&dir), Some(xdg_data_home.as_os_str()), None)
            .expect("both candidates are runnable");
        assert!(
            matches!(&resolved, SidecarProgram::Packaged(p) if *p == sibling),
            "step 3 (the sibling) must outrank step 4 (the user build): {resolved:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&xdg_data_home);
    }

    /// Steps 4 and 5 ranked against each other: a per-user build for this exact
    /// revision beats a default checkout that holds a prebuilt artifact. The user build is keyed to
    /// `EXPECTED_VERDANDI_REVISION`; the default checkout holds whatever revision was last built
    /// there, and at best earns a drift warning. Before this test no resolver test ever gave the
    /// default checkout an artifact, so moving step 4 below step 5 failed nothing. The user build
    /// sits under the fake home's own `.local/share` (no `XDG_DATA_HOME`), so the `$HOME` fallback
    /// is exercised through the resolver too.
    #[test]
    fn a_user_build_beats_a_default_checkout_with_a_prebuilt_artifact() {
        let suffix = node_platform_suffix().expect("this test target has a known Node platform name");
        let home = fake_home_with_default_checkout(&[&format!("{PACKAGED_SIDECAR_BINARY}-0.1.0-{suffix}")]);
        let user_build = user_sidecar_path(None, Some(home.as_os_str()), EXPECTED_VERDANDI_REVISION)
            .expect("an absolute HOME resolves");
        std::fs::create_dir_all(user_build.parent().unwrap()).unwrap();
        std::fs::write(&user_build, b"#!/bin/sh\nexit 0\n").unwrap();

        let resolved = resolve_sidecar_program(None, None, None, None, Some(home.as_os_str()))
            .expect("both candidates are runnable");
        assert!(
            matches!(&resolved, SidecarProgram::Packaged(p) if *p == user_build),
            "step 4 (the user build) must outrank step 5 (the default checkout): {resolved:?}"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    /// A default checkout with a prebuilt artifact and no higher-priority candidate resolves
    /// and runs without building. Testing only an unbuilt checkout would miss deletion of this
    /// branch. Availability must also report it runnable, or startup would advertise a sidecar
    /// that spawning refuses; the same mismatch could select a backend that cannot start.
    #[test]
    fn a_default_checkout_with_a_prebuilt_artifact_runs_it_without_building() {
        let suffix = node_platform_suffix().expect("this test target has a known Node platform name");
        let home = fake_home_with_default_checkout(&[&format!("{PACKAGED_SIDECAR_BINARY}-0.1.0-{suffix}")]);
        let checkout_root = home.join(DEFAULT_CHECKOUT_UNDER_HOME);
        let expected_artifact =
            prebuilt_artifact_in(&checkout_root).expect("this fake default checkout has a prebuilt artifact");

        // Availability and resolution must agree, or the startup line and the spawn disagree.
        assert_eq!(
            sidecar_availability_in(
                None,
                None,
                checkout_path_for(None, Some(home.as_os_str())).as_deref(),
                false,
                None,
                Some(home.as_os_str()),
            ),
            SidecarAvailability::Runnable
        );

        let resolved = resolve_sidecar_program(None, None, None, None, Some(home.as_os_str()))
            .expect("a prebuilt default checkout resolves");
        let SidecarProgram::Checkout(checkout) = &resolved else {
            panic!("a prebuilt default checkout must resolve to the checkout shape: {resolved:?}");
        };
        assert!(!checkout.from_override, "found by step 5, not named by the operator");
        assert_eq!(checkout.path, checkout_root);
        assert_eq!(checkout.prebuilt.as_ref(), Some(&expected_artifact));

        let (description, _, program_path, args) =
            invocation_for(resolved, None, |_| panic!("the default checkout must never build"))
                .expect("a prebuilt checkout never fails to resolve its program");
        assert_eq!(program_path, expected_artifact);
        assert!(args.is_empty());
        assert!(description.contains("default path"), "{description}");
        assert!(description.contains("prebuilt artifact"), "{description}");

        let _ = std::fs::remove_dir_all(&home);
    }

    /// The default checkout must never return a buildable `Checkout`, but an explicit
    /// `EITRI_VERDANDI_CHECKOUT` can. Pin the other side of that wiring: a `Checkout{prebuilt: None}`
    /// that reaches `spawn` must actually build. `invocation_for` allows a fake build closure to
    /// stand in for `ensure_sidecar_built`, without real npm or Node or process-environment mutation.
    #[test]
    fn invocation_for_builds_an_explicit_checkout_that_has_nothing_prebuilt() {
        let root = fake_checkout(&[]);
        let resolved = resolve_sidecar_program(None, root.to_str(), None, None, None).expect("the checkout resolves");
        let build_calls = std::cell::Cell::new(0);
        let (_, _, program_path, args) = invocation_for(resolved, None, |checkout_path| {
            build_calls.set(build_calls.get() + 1);
            assert_eq!(checkout_path, root.as_path());
            Ok(checkout_path.join("apps/claude-sidecar/dist/src/index.js"))
        })
        .expect("the fake build succeeds");
        assert_eq!(
            build_calls.get(),
            1,
            "an unbuilt explicit checkout must build exactly once"
        );
        assert_eq!(program_path, PathBuf::from("node"));
        assert_eq!(args, vec![root.join("apps/claude-sidecar/dist/src/index.js")]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The two shapes that must NEVER build, proven by a `build` closure that panics if called at
    /// all -- stronger than asserting a call count afterward, since a panic inside `invocation_for`
    /// fails this test even if a later line is never reached.
    #[test]
    fn invocation_for_never_builds_a_checkout_that_already_has_a_prebuilt_artifact() {
        let suffix = node_platform_suffix().expect("this test target has a known Node platform name");
        let root = fake_checkout(&[&format!("{PACKAGED_SIDECAR_BINARY}-0.1.0-{suffix}")]);
        let resolved = resolve_sidecar_program(None, root.to_str(), None, None, None).expect("the checkout resolves");
        let expected_artifact = prebuilt_artifact_in(&root).expect("this fake checkout has a prebuilt artifact");
        let (_, _, program_path, args) = invocation_for(resolved, None, |_| {
            panic!("a checkout with a prebuilt artifact must never build")
        })
        .expect("resolving a prebuilt checkout never fails");
        assert_eq!(program_path, expected_artifact);
        assert!(args.is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn invocation_for_never_builds_a_packaged_artifact() {
        let path = PathBuf::from("/fake/verdandi-claude-sidecar");
        let (_, _, program_path, args) = invocation_for(SidecarProgram::Packaged(path.clone()), None, |_| {
            panic!("a packaged artifact has nothing to build")
        })
        .expect("resolving a packaged artifact never fails");
        assert_eq!(program_path, path);
        assert!(args.is_empty());
    }

    /// And when there is no default checkout at all (a fresh machine, or `$HOME` pointing nowhere
    /// useful), resolution ends the same way -- not with `locate_verdandi_checkout`'s own "no
    /// checkout found" wording, which would name a path nobody asked about -- and the message names
    /// exactly the path an `eitri setup` run would use.
    #[test]
    fn nothing_found_at_all_gives_the_shared_hint_and_the_exact_user_path() {
        let xdg_data_home = std::env::temp_dir().join(format!("nv-xdg-nothing-{}", uuid::Uuid::new_v4()));
        let home = std::env::temp_dir().join(format!("nv-home-nothing-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();

        let err = resolve_sidecar_program(
            None,
            None,
            None,
            Some(xdg_data_home.as_os_str()),
            Some(home.as_os_str()),
        )
        .expect_err("nothing at all must still fail, never silently succeed");
        assert!(err.to_string().contains(NO_SIDECAR_HINT), "{err}");
        let expected_path = user_sidecar_path(
            Some(xdg_data_home.as_os_str()),
            Some(home.as_os_str()),
            EXPECTED_VERDANDI_REVISION,
        )
        .expect("this test's own home is absolute");
        assert!(err.to_string().contains(&expected_path.display().to_string()), "{err}");

        let _ = std::fs::remove_dir_all(&home);
    }

    /// `sidecar_availability_in` must count the user path too, or a machine that has run `eitri
    /// setup` reports no sidecar above a session that starts fine from that path. Availability
    /// and spawning must agree, just as they must for a runnable default checkout.
    #[test]
    fn sidecar_availability_counts_the_user_path() {
        let xdg_data_home = std::env::temp_dir().join(format!("nv-xdg-avail-{}", uuid::Uuid::new_v4()));
        let artifact = user_sidecar_path(Some(xdg_data_home.as_os_str()), None, EXPECTED_VERDANDI_REVISION)
            .expect("an absolute XDG_DATA_HOME resolves");
        std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        std::fs::write(&artifact, b"#!/bin/sh\nexit 0\n").unwrap();

        assert_eq!(
            sidecar_availability_in(None, None, None, false, Some(xdg_data_home.as_os_str()), None),
            SidecarAvailability::Runnable
        );
        let _ = std::fs::remove_dir_all(&xdg_data_home);
    }

    /// And a build for another revision does not count as availability either -- the same
    /// invisibility `resolve_sidecar_program` enforces (above), checked at the other function that
    /// must agree with it or the startup line and the actual spawn could disagree about whether
    /// anything is really there.
    #[test]
    fn sidecar_availability_does_not_count_another_revs_user_path() {
        let xdg_data_home = std::env::temp_dir().join(format!("nv-xdg-avail-other-{}", uuid::Uuid::new_v4()));
        let other_rev_artifact = user_sidecar_path(Some(xdg_data_home.as_os_str()), None, "deadbee")
            .expect("a fake other-rev path resolves");
        std::fs::create_dir_all(other_rev_artifact.parent().unwrap()).unwrap();
        std::fs::write(&other_rev_artifact, b"#!/bin/sh\nexit 0\n").unwrap();

        assert_eq!(
            sidecar_availability_in(None, None, None, false, Some(xdg_data_home.as_os_str()), None),
            SidecarAvailability::Missing
        );
        let _ = std::fs::remove_dir_all(&xdg_data_home);
    }

    /// A named `EITRI_VERDANDI_CHECKOUT` with nothing prebuilt is the one case the spawn builds
    /// (step 2), so availability says so rather than "not installed" -- the line a developer running
    /// `try-eitri.sh` against an unbuilt checkout used to see above an `npm` build. And it ranks
    /// like `resolve_sidecar_program`: an installed sibling or a per-user build below it does not
    /// turn the answer into "runnable", because the spawn still builds the named checkout.
    #[test]
    fn a_named_checkout_with_nothing_prebuilt_builds_even_beside_an_installed_artifact() {
        let root = fake_checkout(&[]);
        assert_eq!(
            sidecar_availability_in(None, None, Some(&root), true, None, None),
            SidecarAvailability::BuildsNamedCheckout
        );

        let exe_dir = std::env::temp_dir().join(format!("nv-avail-sibling-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::write(exe_dir.join(PACKAGED_SIDECAR_BINARY), b"#!/bin/sh\nexit 0\n").unwrap();
        assert_eq!(
            sidecar_availability_in(None, Some(&exe_dir), Some(&root), true, None, None),
            SidecarAvailability::BuildsNamedCheckout,
            "step 2 outranks step 3, as in resolve_sidecar_program"
        );
        // The same sibling with no named checkout is runnable (step 3).
        assert_eq!(
            sidecar_availability_in(None, Some(&exe_dir), Some(&root), false, None, None),
            SidecarAvailability::Runnable
        );

        let suffix = node_platform_suffix().expect("this test target has a known Node platform name");
        let built = fake_checkout(&[&format!("{PACKAGED_SIDECAR_BINARY}-0.1.0-{suffix}")]);
        assert_eq!(
            sidecar_availability_in(None, None, Some(&built), true, None, None),
            SidecarAvailability::Runnable,
            "a named checkout holding a prebuilt artifact runs it without building"
        );

        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_dir_all(&built).unwrap();
        let _ = std::fs::remove_dir_all(&exe_dir);
    }

    /// A named `EITRI_SIDECAR_BINARY` decides alone (step 1), and one that is not a file is
    /// missing -- `resolve_sidecar_program` refuses it with `sidecar_missing_message` -- even when a
    /// sibling artifact sits right there.
    #[test]
    fn a_named_binary_decides_alone_and_a_missing_one_is_missing() {
        let exe_dir = std::env::temp_dir().join(format!("nv-avail-named-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&exe_dir).unwrap();
        let sibling = exe_dir.join(PACKAGED_SIDECAR_BINARY);
        std::fs::write(&sibling, b"#!/bin/sh\nexit 0\n").unwrap();
        let missing = exe_dir.join("not-there");

        assert_eq!(
            sidecar_availability_in(sibling.to_str(), None, None, false, None, None),
            SidecarAvailability::Runnable
        );
        assert_eq!(
            sidecar_availability_in(missing.to_str(), Some(&exe_dir), None, false, None, None),
            SidecarAvailability::Missing,
            "step 1 names a binary that is not a file; the sibling below it is never reached"
        );
        assert!(resolve_sidecar_program(missing.to_str(), None, Some(&exe_dir), None, None).is_err());
        let _ = std::fs::remove_dir_all(&exe_dir);
    }

    /// `is_exe_sibling_artifact` matches only the exact step-3 candidate -- not an arbitrary path,
    /// and not the sibling shape with no `exe_dir` known at all.
    #[test]
    fn is_exe_sibling_artifact_matches_only_the_sibling_candidate() {
        let dir = Path::new("/some/exe/dir");
        let sibling = dir.join(PACKAGED_SIDECAR_BINARY);
        assert!(is_exe_sibling_artifact(&sibling, Some(dir)));
        assert!(!is_exe_sibling_artifact(
            Path::new("/elsewhere/verdandi-claude-sidecar"),
            Some(dir)
        ));
        assert!(!is_exe_sibling_artifact(&sibling, None));
    }

    /// A sibling with no `.rev` file gives no warning -- the common case for an install
    /// predating this file's existence.
    #[test]
    fn sibling_rev_skew_warning_is_empty_when_the_rev_file_is_absent() {
        let dir = std::env::temp_dir().join(format!("nv-sibling-rev-absent-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let artifact = dir.join(PACKAGED_SIDECAR_BINARY);
        std::fs::write(&artifact, b"#!/bin/sh\nexit 0\n").unwrap();
        assert!(sibling_rev_skew_warning(&artifact).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `.rev` file matching this client's expectation gives no warning either -- the common case
    /// for a real, current install.
    #[test]
    fn sibling_rev_skew_warning_is_empty_when_the_rev_file_matches() {
        let dir = std::env::temp_dir().join(format!("nv-sibling-rev-match-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let artifact = dir.join(PACKAGED_SIDECAR_BINARY);
        std::fs::write(&artifact, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::write(
            dir.join(format!("{PACKAGED_SIDECAR_BINARY}.rev")),
            EXPECTED_VERDANDI_REVISION,
        )
        .unwrap();
        assert!(sibling_rev_skew_warning(&artifact).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `.rev` file that does not start with `EXPECTED_VERDANDI_REVISION`, nor vice versa,
    /// produces exactly one warning naming both revisions so the mismatch can be diagnosed.
    #[test]
    fn sibling_rev_skew_warning_names_both_revs_on_a_mismatch() {
        let dir = std::env::temp_dir().join(format!("nv-sibling-rev-mismatch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let artifact = dir.join(PACKAGED_SIDECAR_BINARY);
        std::fs::write(&artifact, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::write(dir.join(format!("{PACKAGED_SIDECAR_BINARY}.rev")), "deadbee").unwrap();
        let warnings = sibling_rev_skew_warning(&artifact);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("deadbee") && warnings[0].contains(EXPECTED_VERDANDI_REVISION),
            "{warnings:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Installed packages write the public twin's full 40-character revision while this client
    /// can compare a 7-character `EXPECTED_VERDANDI_REVISION`. Equal-length inputs cannot prove
    /// that `sibling_rev_skew_warning` accepts a full hash starting with the expected prefix;
    /// this pins that direction of the bidirectional `starts_with` comparison.
    #[test]
    fn sibling_rev_skew_warning_is_empty_when_the_rev_file_is_a_longer_sha_starting_with_the_expected_prefix() {
        let dir = std::env::temp_dir().join(format!("nv-sibling-rev-long-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let artifact = dir.join(PACKAGED_SIDECAR_BINARY);
        std::fs::write(&artifact, b"#!/bin/sh\nexit 0\n").unwrap();
        let full_sha = format!("{EXPECTED_VERDANDI_REVISION}1234567890abcdef1234567890abcdef1");
        assert_eq!(full_sha.len(), 40, "{full_sha}");
        std::fs::write(dir.join(format!("{PACKAGED_SIDECAR_BINARY}.rev")), &full_sha).unwrap();
        assert!(sibling_rev_skew_warning(&artifact).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other direction: a `.rev` file shorter than `EXPECTED_VERDANDI_REVISION` that is still a
    /// genuine prefix of it also gives no warning ("and vice versa" in the original wording of the rule).
    #[test]
    fn sibling_rev_skew_warning_is_empty_when_the_rev_file_is_a_shorter_prefix_of_the_expected_revision() {
        let dir = std::env::temp_dir().join(format!("nv-sibling-rev-short-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let artifact = dir.join(PACKAGED_SIDECAR_BINARY);
        std::fs::write(&artifact, b"#!/bin/sh\nexit 0\n").unwrap();
        let short_prefix = &EXPECTED_VERDANDI_REVISION[..EXPECTED_VERDANDI_REVISION.len() - 1];
        std::fs::write(dir.join(format!("{PACKAGED_SIDECAR_BINARY}.rev")), short_prefix).unwrap();
        assert!(sibling_rev_skew_warning(&artifact).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `NO_SIDECAR_HINT` must start with `sidecar_missing_message`'s opening sentence so
    /// `agent-ui/web/src/problems.ts::classifySidecarMissing` recognizes the resolver fallthrough.
    /// Otherwise a fresh install gets raw text without a headline or remedy. `problems.test.ts`
    /// independently pins the same literal; this is the Rust half of that agreement.
    #[test]
    fn no_sidecar_hint_starts_with_the_panels_sidecar_missing_headline() {
        const SIDECAR_MISSING_HEADLINE: &str = "The agent sidecar is not installed.";
        assert!(
            NO_SIDECAR_HINT.starts_with(SIDECAR_MISSING_HEADLINE),
            "{NO_SIDECAR_HINT:?} must start with {SIDECAR_MISSING_HEADLINE:?}"
        );
    }

    fn wait_for_line(tail: &Arc<Mutex<StderrTail>>, line: &str) {
        let started = std::time::Instant::now();
        while !tail.lock().unwrap().snapshot().iter().any(|l| l == line) {
            assert!(started.elapsed() < Duration::from_secs(30), "never printed {line:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_stopped_drain_hands_over_its_line_in_progress_and_the_next_one_finishes_it() {
        use std::io::Write;
        let (read, mut write) = UnixStream::pair().unwrap();
        let tail = Arc::new(Mutex::new(StderrTail::default()));
        let drain = Drain::start(
            File::from(OwnedFd::from(read)),
            Vec::new(),
            "stderr",
            Some(Arc::clone(&tail)),
        )
        .unwrap();
        write.write_all(b"first line\nsecond ha").unwrap();
        wait_for_line(&tail, "first line");
        let Drained { pipe, partial, ended } = drain.stop().unwrap();
        assert!(!ended);
        assert!(b"second ha".starts_with(&partial), "{partial:?}");

        write.write_all(b"lf\n").unwrap();
        let drain = Drain::start(pipe, partial, "stderr", Some(Arc::clone(&tail))).unwrap();
        wait_for_line(&tail, "second half");
        drop(write);
        drop(drain.stop());
        let lines = tail.lock().unwrap().snapshot();
        assert_eq!(lines, ["first line", "second half"]);
    }

    /// A stderr nobody reads (here: its lock held, so every write to it waits) must not keep a drain
    /// from reading on or from stopping, or a move would wait on it for good.
    #[test]
    fn a_drain_reads_on_and_stops_while_this_processs_stderr_takes_nothing() {
        use std::io::Write;
        let held = std::io::stderr().lock();
        let (read, mut write) = UnixStream::pair().unwrap();
        let tail = Arc::new(Mutex::new(StderrTail::default()));
        let drain = Drain::start(
            File::from(OwnedFd::from(read)),
            Vec::new(),
            "stderr",
            Some(Arc::clone(&tail)),
        )
        .unwrap();
        let lines = LOG_BACKLOG * 2;
        let writer = std::thread::spawn(move || {
            for n in 0..lines {
                writeln!(write, "line {n}").unwrap();
            }
            write
        });
        wait_for_line(&tail, &format!("line {}", lines - 1));
        let _write = writer.join().unwrap();
        let (stopped, done) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = stopped.send(drain.stop().map(|drained| drained.ended));
        });
        let ended = done.recv_timeout(Duration::from_secs(10));
        drop(held);
        assert_eq!(ended.expect("the drain stopped"), Some(false));
    }

    #[test]
    fn a_drain_stops_at_once_on_a_pipe_nobody_writes_to() {
        let (read, _write) = UnixStream::pair().unwrap();
        let drain = Drain::start(File::from(OwnedFd::from(read)), b"half".to_vec(), "stdout", None).unwrap();
        let Drained { partial, ended, .. } = drain.stop().unwrap();
        assert!(!ended);
        assert_eq!(partial, b"half");
    }

    #[test]
    fn the_stderr_tail_crosses_as_bytes_and_comes_back_the_same() {
        let mut tail = StderrTail::default();
        tail.push("one".into());
        tail.push("two words".into());
        let back = StderrTail::from_bytes(&tail.to_bytes());
        assert_eq!(back.snapshot(), ["one", "two words"]);
    }

    #[test]
    fn the_sidecar_leads_a_process_group_of_its_own() {
        let mut command = sidecar_command_for(
            Path::new("/bin/sleep"),
            &[PathBuf::from("30")],
            Path::new("/never/bound"),
            None,
            false,
            None,
        );
        let mut child = command.spawn().unwrap();
        let pid = child.id() as libc::pid_t;
        // SAFETY: `getpgid` reads no memory of ours.
        let group = unsafe { libc::getpgid(pid) };
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(group, pid);
    }
}
