//! `eitri-server term-host`: the process that owns one terminal. It runs the shell on a PTY inside a
//! `HeadlessTerminal`, serves the `Terminal` service for that one terminal on its own socket in the daemon's run
//! directory, writes the shell's exit record, and outlives the daemon that started it.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use eitri_server_proto::terminal_client_frame::Frame as ClientFrame;
use eitri_server_proto::terminal_server::{Terminal, TerminalServer};
use eitri_server_proto::terminal_server_frame::Frame as ServerFrame;
use eitri_server_proto::{
    CloseTerminalRequest, CloseTerminalResponse, DetachReason, Detached, InputWritten, ListTerminalsRequest,
    ListTerminalsResponse, OpenTerminalRequest, OpenTerminalResponse, OutputBytes, Resized, SnapshotPart, TailKind,
    TermColors, TermSize, TerminalClientFrame, TerminalExited, TerminalInfo, TerminalServerFrame, WaitReason, Waiting,
    TERM_ID_MAX, TERM_INPUT_MAX, TERM_NAME_MAX, TERM_OUTPUT_MAX, TERM_PATH_MAX, TERM_SNAPSHOT_MAX,
    TERM_SNAPSHOT_PART_MAX,
};
use eitri_terminal::{
    Attachment, DetachWhy, ExitInfo, HeadlessConfig, HeadlessTerminal, PtySize, Snapshot, SpawnExtras, SpawnSpec,
    StreamItem, Tail, TerminalColors, WaitWhy,
};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, watch};
use tonic::{Request, Response, Status, Streaming};

use super::super::sidecars;
use super::adopt::{self, ExitRecord};
use super::session;
use super::{clamp_wire_size, ClosedTerminal, CLOSE_GRACE, RETENTION_MS};

/// The first argument of the encoded list: the layout of the rest.
const FORMAT: &str = "1";
const FIXED_ARGS: usize = 14;
/// An attachment's live-output budget before it owes a resync instead.
const QUEUE_BUDGET: usize = 4 * 1024 * 1024;
/// A client's first frame must be its `AttachHello`, within this long.
pub const HELLO_WITHIN: Duration = Duration::from_secs(5);
/// A refused stream's status waits at most this long for a client that stopped reading.
const REFUSAL_WITHIN: Duration = Duration::from_secs(5);
/// Once the host is ending, its server gets this long to deliver what its clients are owed.
pub const DRAIN: Duration = Duration::from_secs(5);
const WATCH_EVERY: Duration = Duration::from_millis(250);
const DEFAULT_SIZE: TermSize = TermSize {
    columns: 80,
    rows: 24,
    cell_width: 0,
    cell_height: 0,
};

/// What the daemon tells a host it starts, through [`ARGS_FD`].
#[derive(Clone, Debug, PartialEq)]
pub struct HostArgs {
    pub terminal_id: String,
    pub command_id: String,
    pub start_dir: String,
    pub size: TermSize,
    pub colors: TermColors,
    /// Where its socket goes (the daemon's run directory, or its socket fallback).
    pub run: PathBuf,
    /// Where its records go (`state/run/<run key>`).
    pub metadata: PathBuf,
    /// The helper's state directory, for `audit.log`.
    pub state: PathBuf,
    /// The shell's whole environment.
    pub env: Vec<(OsString, OsString)>,
}

impl HostArgs {
    pub fn encode(&self) -> String {
        let number = |n: u32| OsString::from(n.to_string());
        let mut args: Vec<OsString> = vec![
            FORMAT.into(),
            self.terminal_id.clone().into(),
            self.command_id.clone().into(),
            self.start_dir.clone().into(),
            number(self.size.columns),
            number(self.size.rows),
            number(self.size.cell_width),
            number(self.size.cell_height),
            number(self.colors.background),
            number(self.colors.foreground),
            self.colors.cursor.map(number).unwrap_or_default(),
            self.run.clone().into(),
            self.metadata.clone().into(),
            self.state.clone().into(),
        ];
        for (key, value) in &self.env {
            let mut pair = key.clone();
            pair.push("=");
            pair.push(value);
            args.push(pair);
        }
        let refs: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
        eitri_server_proto::args_hex::encode_args(&refs)
    }

    pub fn decode(args: Vec<OsString>) -> io::Result<Self> {
        let invalid = |what: &str| crate::invalid(format!("term-host: invalid {what}"));
        if args.len() < FIXED_ARGS || args[0] != FORMAT {
            return Err(invalid("arguments"));
        }
        let text = |index: usize, what: &str| args[index].to_str().map(str::to_owned).ok_or_else(|| invalid(what));
        let number = |index: usize, what: &str| -> io::Result<u32> {
            args[index]
                .to_str()
                .and_then(|value| value.parse().ok())
                .ok_or_else(|| invalid(what))
        };
        let terminal_id = text(1, "terminal id")?;
        if !adopt::valid_id(&terminal_id) {
            return Err(invalid("terminal id"));
        }
        let command_id = text(2, "command id")?;
        sidecars::validate_command_id(&command_id).map_err(|_| invalid("command id"))?;
        let start_dir = text(3, "start directory")?;
        let path = |index: usize, what: &str| {
            let path = PathBuf::from(&args[index]);
            if path.is_absolute() {
                Ok(path)
            } else {
                Err(invalid(what))
            }
        };
        let env = args[FIXED_ARGS..]
            .iter()
            .map(|pair| {
                let bytes = pair.as_bytes();
                let split = bytes.iter().position(|b| *b == b'=').filter(|at| *at > 0);
                split
                    .map(|at| {
                        (
                            OsString::from_vec(bytes[..at].to_vec()),
                            OsString::from_vec(bytes[at + 1..].to_vec()),
                        )
                    })
                    .ok_or_else(|| invalid("environment"))
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self {
            terminal_id,
            command_id,
            start_dir,
            size: clamp_wire_size(&TermSize {
                columns: number(4, "columns")?,
                rows: number(5, "rows")?,
                cell_width: number(6, "cell width")?,
                cell_height: number(7, "cell height")?,
            }),
            colors: TermColors {
                background: number(8, "background")?,
                foreground: number(9, "foreground")?,
                cursor: if args[10].is_empty() {
                    None
                } else {
                    Some(number(10, "cursor")?)
                },
            },
            run: path(11, "run directory")?,
            metadata: path(12, "record directory")?,
            state: path(13, "state directory")?,
            env,
        })
    }
}

/// A requested size, clamped to the grid bounds; none is 80x24.
pub fn size_from_wire(size: Option<&TermSize>) -> TermSize {
    clamp_wire_size(size.unwrap_or(&DEFAULT_SIZE))
}

fn pty_size(size: &TermSize) -> PtySize {
    let size = clamp_wire_size(size);
    PtySize {
        cols: size.columns as u16,
        rows: size.rows as u16,
        cell_width_px: size.cell_width as u16,
        cell_height_px: size.cell_height as u16,
    }
}

fn size_to_wire(size: PtySize) -> TermSize {
    TermSize {
        columns: size.cols.into(),
        rows: size.rows.into(),
        cell_width: size.cell_width_px.into(),
        cell_height: size.cell_height_px.into(),
    }
}

pub fn colors_to_wire(colors: &TerminalColors) -> TermColors {
    let rgb = |r: u8, g: u8, b: u8| (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b);
    TermColors {
        background: rgb(colors.background.r, colors.background.g, colors.background.b),
        foreground: rgb(colors.foreground.r, colors.foreground.g, colors.foreground.b),
        cursor: colors.cursor.map(|c| rgb(c.r, c.g, c.b)),
    }
}

pub fn colors_from_wire(wire: &TermColors) -> TerminalColors {
    let mut colors = TerminalColors::default();
    let set = |value: u32, r: &mut u8, g: &mut u8, b: &mut u8| {
        *r = (value >> 16) as u8;
        *g = (value >> 8) as u8;
        *b = value as u8;
    };
    set(
        wire.background,
        &mut colors.background.r,
        &mut colors.background.g,
        &mut colors.background.b,
    );
    set(
        wire.foreground,
        &mut colors.foreground.r,
        &mut colors.foreground.g,
        &mut colors.foreground.b,
    );
    colors.cursor = wire.cursor.map(|value| {
        let mut cursor = colors.foreground;
        set(value, &mut cursor.r, &mut cursor.g, &mut cursor.b);
        cursor
    });
    colors
}

pub(super) fn exit_to_wire(code: Option<i32>, signal: Option<i32>, left: bool) -> TerminalExited {
    TerminalExited { code, signal, left }
}

/// The descriptor a host reads its arguments from: a pipe the daemon filled before the spawn, so the start directory
/// and the shell's environment never appear on the host's command line (`/proc/<pid>/cmdline` is world-readable).
pub const ARGS_FD: i32 = 3;
/// The encoded arguments are hex: twice the byte cap, and a little.
const ARGS_TEXT_MAX: u64 = 2 * eitri_server_proto::args_hex::MAX_ARGS_BYTES as u64 + 2;

/// Reads the arguments the daemon wrote to [`ARGS_FD`].
pub fn read_args() -> io::Result<Vec<OsString>> {
    // SAFETY: fstat on a descriptor number only reads its metadata.
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(ARGS_FD, &mut stat) } != 0 || stat.st_mode & libc::S_IFMT != libc::S_IFIFO {
        return Err(crate::invalid("term-host: no argument pipe"));
    }
    // SAFETY: the descriptor is a pipe inherited for this one read, owned from here on.
    let file = unsafe { std::fs::File::from_raw_fd(ARGS_FD) };
    let mut text = String::new();
    file.take(ARGS_TEXT_MAX).read_to_string(&mut text)?;
    eitri_server_proto::args_hex::decode_args(text.trim_end()).map_err(|error| crate::invalid(error.to_string()))
}

/// Bytes of one attachment's input admitted but not yet written to the PTY; past it the attachment is ended rather
/// than its input buffered for a program that does not read.
pub const INPUT_BUDGET: u64 = 1024 * 1024;

/// One attachment's input accounting: what its stream admitted, and what the terminal reports written.
struct Flow {
    admitted: AtomicU64,
    /// The client's written count when this attachment's first snapshot was taken; `u64::MAX` until then.
    base: AtomicU64,
    upto: AtomicU64,
}

impl Flow {
    fn written(&self, upto: u64) {
        let _ = self
            .base
            .compare_exchange(u64::MAX, upto, Ordering::SeqCst, Ordering::SeqCst);
        self.upto.fetch_max(upto, Ordering::SeqCst);
    }

    /// Admits `len` more bytes unless that would leave more than [`INPUT_BUDGET`] unwritten.
    fn admit(&self, len: u64) -> bool {
        let base = self.base.load(Ordering::SeqCst);
        let written = if base == u64::MAX {
            0
        } else {
            self.upto.load(Ordering::SeqCst).saturating_sub(base)
        };
        let admitted = self.admitted.load(Ordering::SeqCst) + len;
        if admitted.saturating_sub(written) > INPUT_BUDGET {
            return false;
        }
        self.admitted.store(admitted, Ordering::SeqCst);
        true
    }
}

pub(super) struct Shared {
    id: String,
    command_id: String,
    start_dir: String,
    metadata: PathBuf,
    state: PathBuf,
    started_unix_ms: u64,
    shell_pid: u32,
    /// The shell, pinned by the terminal right after its spawn, before anything could reap it.
    shell: Option<Arc<session::Pinned>>,
    /// `None` once the terminal is closed or finished.
    term: Mutex<Option<HeadlessTerminal>>,
    /// A successful attach advances this under the terminal lock so only the current stream can act on it.
    generation: AtomicU64,
    exit: Mutex<Option<ExitRecord>>,
    /// Set (to a `DetachReason`) when the terminal is being closed: every attachment says so and ends first.
    closing: watch::Sender<Option<i32>>,
    /// Serializes closes and retains whether session cleanup failed for another close of this host.
    closing_serial: tokio::sync::Mutex<bool>,
    #[cfg(test)]
    end_session: fn(&session::Pinned, Duration, &Path, &str) -> bool,
    shutdown: watch::Sender<bool>,
}

impl Shared {
    fn spawn(args: HostArgs) -> io::Result<Self> {
        let shell = args
            .env
            .iter()
            .find(|(key, value)| key == "SHELL" && !value.is_empty())
            .map(|(_, value)| PathBuf::from(value))
            .unwrap_or_else(|| PathBuf::from("/bin/sh"));
        let mut arg0 = OsString::from("-");
        arg0.push(shell.file_name().unwrap_or(OsStr::new("sh")));
        // The daemon started this process in the start directory it checked; the shell enters the same directory.
        let here = std::ffi::CString::new(".").expect("no NUL");
        // SAFETY: open returns a new descriptor or -1 and touches no Rust memory.
        let cwd = unsafe { libc::open(here.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
        if cwd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the descriptor was just opened and is owned here alone.
        let cwd = unsafe { OwnedFd::from_raw_fd(cwd) };
        let term = HeadlessTerminal::spawn(HeadlessConfig {
            spawn: SpawnSpec {
                program: shell,
                args: Vec::new(),
                cwd: PathBuf::from(&args.start_dir),
                env: Vec::new(),
                extra_env: Vec::new(),
            },
            extras: SpawnExtras {
                exact_env: Some(args.env),
                arg0: Some(arg0),
                umask: Some(crate::run_dir::inherited_umask() as u32),
                cwd_fd: Some(cwd),
            },
            size: pty_size(&args.size),
            colors: colors_from_wire(&args.colors),
            queue_budget: QUEUE_BUDGET,
            snapshot_cap: TERM_SNAPSHOT_MAX,
        })?;
        let pid = term.status().pid;
        let shell = term
            .child_pin()
            .map(|(fd, start)| Arc::new(session::Pinned { fd, pid, start }));
        if let Some(shell) = &shell {
            // The daemon ends this session through it when the host cannot be asked to.
            let record = session::ShellRecord {
                pid,
                start_time: shell.start,
                boot_id: super::current_boot_id().unwrap_or_default(),
            };
            let written = serde_json::to_vec(&record).map_err(io::Error::other).and_then(|bytes| {
                agent::private_fs::publish_private_file(
                    &agent::private_fs::open_dir(&args.metadata)?,
                    OsStr::new(&format!("th-{}.shell", args.terminal_id)),
                    &bytes,
                )
            });
            if let Err(error) = written {
                eprintln!("eitri-server term-host: shell record: {error}");
            }
        }
        let (shutdown, _) = watch::channel(false);
        let (closing, _) = watch::channel(None);
        Ok(Self {
            id: args.terminal_id,
            command_id: args.command_id,
            start_dir: args.start_dir,
            metadata: args.metadata,
            state: args.state,
            started_unix_ms: crate::run_dir::unix_ms(),
            shell_pid: pid,
            shell,
            term: Mutex::new(Some(term)),
            generation: AtomicU64::new(0),
            exit: Mutex::new(None),
            closing,
            closing_serial: tokio::sync::Mutex::new(false),
            #[cfg(test)]
            end_session: session::end,
            shutdown,
        })
    }

    fn with_term<T>(&self, f: impl FnOnce(&HeadlessTerminal) -> T) -> Option<T> {
        self.term.lock().unwrap_or_else(PoisonError::into_inner).as_ref().map(f)
    }

    fn attach(&self, hello: &eitri_server_proto::AttachHello) -> Result<Option<(Attachment, u64)>, Status> {
        self.with_term(|term| {
            let attachment = term.attach_with(
                hello.client_id.clone(),
                hello.size.as_ref().map(pty_size),
                hello.colors.as_ref().map(colors_from_wire),
            )?;
            let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
            Ok((attachment, generation))
        })
        .transpose()
        .map_err(|e: io::Error| Status::unavailable(format!("cannot attach to terminal: {e}")))
    }

    /// Runs `f` only while `generation` is still the current attachment's.
    fn with_current<T>(&self, generation: u64, f: impl FnOnce(&HeadlessTerminal) -> T) -> Option<T> {
        let term = self.term.lock().unwrap_or_else(PoisonError::into_inner);
        if self.generation.load(Ordering::SeqCst) != generation {
            return None;
        }
        term.as_ref().map(f)
    }

    fn request_shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    fn recorded_exit(&self) -> Option<ExitRecord> {
        *self.exit.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Writes the exit record once, at the first exit seen; never the terminal's output.
    fn record_exit(&self, info: ExitInfo) {
        let mut exit = self.exit.lock().unwrap_or_else(PoisonError::into_inner);
        if exit.is_some() {
            return;
        }
        let record = ExitRecord {
            code: info.code,
            signal: info.signal,
            left: info.detached,
            exited_unix_ms: crate::run_dir::unix_ms(),
        };
        let written = serde_json::to_vec(&record).map_err(io::Error::other).and_then(|bytes| {
            agent::private_fs::publish_private_file(
                &agent::private_fs::open_dir(&self.metadata)?,
                OsStr::new(&format!("th-{}.exit", self.id)),
                &bytes,
            )
        });
        if let Err(error) = written {
            eprintln!("eitri-server term-host: exit record: {error}");
        }
        let _ = crate::audit::record(
            &self.state,
            "terminal-exit",
            &format!(
                "id={} code={} signal={} left={}",
                self.id,
                info.code.map_or("none".into(), |c| c.to_string()),
                info.signal.map_or("none".into(), |s| s.to_string()),
                info.detached
            ),
        );
        *exit = Some(record);
    }

    /// Closes the terminal once: every attachment is told `reason` and ends, then the shell's session is hung up,
    /// what outlives [`CLOSE_GRACE`] (and what was started meanwhile) is killed, and the shell reaped. Answers the
    /// exit record and whether session cleanup failed.
    async fn close(&self, reason: DetachReason) -> ClosedTerminal {
        let mut session_end_failed = self.closing_serial.lock().await;
        let term = self.term.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(term) = term {
            self.closing.send_replace(Some(reason as i32));
            let shell = self.shell.clone();
            let state = self.state.clone();
            #[cfg(not(test))]
            let end = session::end;
            #[cfg(test)]
            let end = self.end_session;
            let (exit, failed) =
                tokio::task::spawn_blocking(move || close_session(term, shell.as_deref(), &state, end))
                    .await
                    .unwrap_or((ExitInfo::UNKNOWN, true));
            *session_end_failed = failed;
            self.record_exit(exit);
        }
        ClosedTerminal {
            exit: self.recorded_exit(),
            session_end_failed: *session_end_failed,
        }
    }

    fn info(&self) -> TerminalInfo {
        let status = self.with_term(HeadlessTerminal::status);
        let now = crate::run_dir::unix_ms();
        let recorded = self.recorded_exit();
        let exit = status
            .as_ref()
            .and_then(|status| status.exit)
            .map(|exit| exit_to_wire(exit.code, exit.signal, exit.detached))
            .or_else(|| recorded.map(|exit| exit_to_wire(exit.code, exit.signal, exit.left)));
        let job = status.as_ref().and_then(|status| status.job.clone());
        TerminalInfo {
            terminal_id: self.id.clone(),
            start_dir: self.start_dir.clone(),
            cwd: status
                .as_ref()
                .and_then(|status| status.cwd.as_ref())
                .map(|cwd| capped(&cwd.to_string_lossy(), TERM_PATH_MAX))
                .unwrap_or_default(),
            pid: self.shell_pid,
            started_unix_ms: self.started_unix_ms,
            alive: status.as_ref().is_some_and(|status| status.alive),
            exit,
            attached: status.as_ref().is_some_and(|status| status.attached.is_some()),
            detached_since_unix_ms: status
                .as_ref()
                .and_then(|status| status.detached_since)
                .map(|since| now.saturating_sub(since.elapsed().as_millis() as u64))
                .unwrap_or(0),
            foreground_job: job.as_ref().map(|job| job.busy),
            foreground_name: job
                .and_then(|job| job.foreground_name)
                .map(|name| capped(&name.to_string_lossy(), TERM_NAME_MAX))
                .unwrap_or_default(),
            size: status.as_ref().map(|status| size_to_wire(status.size)),
            open_command_id: self.command_id.clone(),
        }
    }
}

pub(super) fn capped(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// The shell's session ends while the shell lives (`session::end`: SIGHUP to the shell and the members of its session
/// found while it is alive; after [`CLOSE_GRACE`], a rescan only if the shell still lives, then SIGKILL), then the
/// terminal reaps the shell. A shell already gone gets the terminal's own hangup and nothing is looked for: its session
/// id may have been reused.
fn close_session(
    term: HeadlessTerminal,
    shell: Option<&session::Pinned>,
    state: &Path,
    end: fn(&session::Pinned, Duration, &Path, &str) -> bool,
) -> (ExitInfo, bool) {
    let (exit, failed) = match shell {
        Some(shell) if shell.alive() => {
            let complete = end(shell, CLOSE_GRACE, state, "terminal-stop-error");
            (term.close(Duration::ZERO), !complete)
        }
        _ => (term.close(CLOSE_GRACE), false),
    };
    (exit.unwrap_or(ExitInfo::UNKNOWN), failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::terminals::tests::{fake_host, test_paths, Scratch};
    use crate::daemon::terminals::{Config, TerminalHosts, CLOSE_WARNING, UNCONFIRMED_SESSION};
    use agent::private_fs;

    #[test]
    fn a_failed_attach_preserves_the_running_host_and_its_attachment() {
        // File descriptor limits belong to the process, so only this test runs in the child.
        const CHILD: &str = "EITRI_TEST_ATTACH_NOFILE";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", std::thread::current().name().unwrap(), "--nocapture"])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let scratch = Scratch::new();
        let paths = test_paths(&scratch);
        let shared = Shared::spawn(HostArgs {
            terminal_id: "term".into(),
            command_id: "open-term".into(),
            start_dir: "/".into(),
            size: DEFAULT_SIZE,
            colors: TermColors::default(),
            run: paths.run.clone(),
            metadata: paths.metadata_run(),
            state: paths.state,
            env: vec![("SHELL".into(), "/bin/sh".into())],
        })
        .unwrap();
        let hello = eitri_server_proto::AttachHello {
            client_id: "first".into(),
            ..Default::default()
        };
        let (_first, generation) = shared.attach(&hello).unwrap().unwrap();
        let mut saved = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: both limits are initialized and the original hard limit is preserved.
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut saved) }, 0);
        let limit = libc::rlimit { rlim_cur: 0, ..saved };
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        let result = shared.attach(&eitri_server_proto::AttachHello {
            client_id: "second".into(),
            ..hello
        });
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &saved) }, 0);
        assert_eq!(result.err().map(|s| s.code()), Some(tonic::Code::Unavailable));
        assert_eq!(shared.with_current(generation, |term| term.status().alive), Some(true));
        assert!(!*shared.shutdown.borrow());
        assert!(shared.recorded_exit().is_none());
        assert!(!shared.metadata.join("th-term.exit").try_exists().unwrap());
    }

    #[tokio::test]
    async fn host_close_rpc_preserves_a_session_member_pin_failure() {
        let scratch = Scratch::new();
        let paths = test_paths(&scratch);
        let (mut host_process, record) = fake_host(&paths);
        let shell = scratch.0.join("shell");
        let member_file = scratch.0.join("member");
        private_fs::write_private(&member_file, b"").unwrap();
        private_fs::write_private(
            &shell,
            b"#!/bin/sh\ntrap '' HUP\n/bin/sleep 60 &\nprintf '%s\\n' \"$!\" > \"$HOME/member\"\nwait\n",
        )
        .unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut shared = Shared::spawn(HostArgs {
            terminal_id: record.terminal_id.clone(),
            command_id: record.command_id,
            start_dir: record.start_dir,
            size: DEFAULT_SIZE,
            colors: TermColors::default(),
            run: paths.run.clone(),
            metadata: paths.metadata_run(),
            state: paths.state.clone(),
            env: vec![
                ("SHELL".into(), shell.into()),
                ("HOME".into(), scratch.0.clone().into()),
            ],
        })
        .unwrap();
        shared.end_session = |shell, _, state, event| {
            session::end_with_pin(shell, Duration::ZERO, state, event, |_| {
                Err(io::Error::from_raw_os_error(libc::EMFILE))
            })
        };
        let service = Service(Arc::new(shared));
        let member = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let text = private_fs::read_private_nofollow_capped(&member_file, 64).unwrap();
                if let Some(pid) = text.strip_suffix('\n').and_then(|pid| pid.parse().ok()) {
                    break sidecars::pin(pid).unwrap();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the shell started its session member");
        let sidecars = Arc::new(sidecars::Sidecars::new(paths.clone()).unwrap());
        let hosts = TerminalHosts::new(paths, Config::default(), sidecars);
        let result = hosts
            .close_with(&record.terminal_id, "close-fake", async |id, command_id| {
                let reply = service
                    .close_terminal(Request::new(CloseTerminalRequest {
                        terminal_id: id.into(),
                        command_id: command_id.into(),
                    }))
                    .await;
                host_process.kill().await.unwrap();
                reply.ok()
            })
            .await;

        let member_alive = !sidecars::exited(&member).unwrap();
        sidecars::signal(&member, libc::SIGKILL).unwrap();
        assert!(member_alive);
        let response = result.unwrap().response();
        assert_eq!(response.get_ref().exit.as_ref().unwrap().signal, Some(libc::SIGKILL));
        assert_eq!(response.metadata().get(CLOSE_WARNING).unwrap(), UNCONFIRMED_SESSION);
    }
}

/// The connections `listener` accepts from `uid` alone; another uid's are closed unread. A failed accept (no
/// descriptors left, say) waits and goes on: it must not end the terminal.
pub fn admitted(
    listener: UnixListener,
    uid: u32,
) -> impl futures_util::Stream<Item = io::Result<tokio::net::UnixStream>> {
    futures_util::stream::unfold(listener, move |listener| async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let admitted = stream
                        .peer_cred()
                        .and_then(|cred| crate::peer::require_own(cred.uid(), uid));
                    if admitted.is_ok() {
                        return Some((Ok(stream), listener));
                    }
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    })
}

/// Runs the host until its terminal is closed or stopped, or its shell's exit has been handed to the daemon (or was
/// recorded [`RETENTION_MS`] ago).
pub async fn run(args: Vec<OsString>) -> io::Result<()> {
    let args = HostArgs::decode(args)?;
    let socket = agent::socket_path::in_dir(&args.run, &format!("th-{}.sock", args.terminal_id))?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let shared = Arc::new(Shared::spawn(args)?);
    let listener = match UnixListener::bind(&socket) {
        Ok(listener) => listener,
        Err(error) => {
            finish(&shared).await;
            return Err(error);
        }
    };
    let _cleanup = RemoveSocket(socket.clone());
    std::fs::set_permissions(
        &socket,
        std::fs::Permissions::from_mode(agent::private_fs::PRIVATE_FILE_MODE),
    )?;
    {
        let shared = shared.clone();
        tokio::spawn(async move {
            if terminate.recv().await.is_some() {
                shared.close(DetachReason::Stopped).await;
                shared.request_shutdown();
            }
        });
    }
    tokio::spawn(watch_exit(shared.clone()));
    let ending = {
        let mut rx = shared.shutdown.subscribe();
        async move {
            let _ = rx.wait_for(|end| *end).await;
        }
    };
    let drained = {
        let mut rx = shared.shutdown.subscribe();
        async move {
            let _ = rx.wait_for(|end| *end).await;
            tokio::time::sleep(DRAIN).await;
        }
    };
    let server = tonic::transport::Server::builder()
        .http2_keepalive_interval(Some(Duration::from_secs(10)))
        .http2_keepalive_timeout(Some(Duration::from_secs(20)))
        .add_service(TerminalServer::new(Service(shared.clone())))
        .serve_with_incoming_shutdown(admitted(listener, agent::private_fs::euid()), ending);
    let result = tokio::select! {
        result = server => result.map_err(io::Error::other),
        _ = drained => Ok(()),
    };
    finish(&shared).await;
    result
}

/// Ends a terminal still open when the host goes (its exit handed to the daemon, or kept a day). A shell that left
/// its terminal is hung up with it.
async fn finish(shared: &Arc<Shared>) {
    let term = shared.term.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some(term) = term {
        if let Ok(Some(exit)) = tokio::task::spawn_blocking(move || term.close(Duration::ZERO)).await {
            shared.record_exit(exit);
        }
    }
}

/// Records the shell's exit as soon as it is known, so it outlives this host and the daemon; ends the host a day
/// after an exit nobody came to see.
async fn watch_exit(shared: Arc<Shared>) {
    let mut ended = shared.shutdown.subscribe();
    loop {
        tokio::select! {
            _ = tokio::time::sleep(WATCH_EVERY) => {}
            _ = ended.wait_for(|end| *end) => return,
        }
        if let Some(Some(exit)) = shared.with_term(|term| term.status().exit) {
            shared.record_exit(exit);
        }
        if let Some(exit) = shared.recorded_exit() {
            if crate::run_dir::unix_ms().saturating_sub(exit.exited_unix_ms) >= RETENTION_MS {
                shared.request_shutdown();
                return;
            }
        }
    }
}

struct RemoveSocket(PathBuf);
impl Drop for RemoveSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

struct Service(Arc<Shared>);

/// The host treats the exit as handed to the daemon when the transport drains this stream through EOF after `Exited`.
/// The daemon can still have frames buffered for the client when the host releases its final screen.
struct Frames {
    rx: mpsc::Receiver<Result<TerminalServerFrame, Status>>,
    exited_sent: bool,
    shared: Arc<Shared>,
}

impl futures_util::Stream for Frames {
    type Item = Result<TerminalServerFrame, Status>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        let polled = self.rx.poll_recv(cx);
        match &polled {
            std::task::Poll::Ready(Some(Ok(TerminalServerFrame {
                frame: Some(ServerFrame::Exited(_)),
            }))) => self.exited_sent = true,
            std::task::Poll::Ready(None) if self.exited_sent => {
                self.exited_sent = false;
                self.shared.request_shutdown();
            }
            _ => {}
        }
        polled
    }
}

fn frames(rx: mpsc::Receiver<Result<TerminalServerFrame, Status>>, shared: Arc<Shared>) -> FrameStream {
    Box::pin(Frames {
        rx,
        exited_sent: false,
        shared,
    })
}

type FrameStream = Pin<Box<dyn futures_util::Stream<Item = Result<TerminalServerFrame, Status>> + Send + 'static>>;

#[tonic::async_trait]
impl Terminal for Service {
    async fn open_terminal(&self, _: Request<OpenTerminalRequest>) -> Result<Response<OpenTerminalResponse>, Status> {
        Err(Status::unimplemented(
            "a terminal host serves one terminal; the daemon opens terminals",
        ))
    }

    type AttachStream = FrameStream;

    async fn attach(
        &self,
        request: Request<Streaming<TerminalClientFrame>>,
    ) -> Result<Response<Self::AttachStream>, Status> {
        let mut inbound = request.into_inner();
        let first = tokio::time::timeout(HELLO_WITHIN, inbound.message())
            .await
            .map_err(|_| Status::deadline_exceeded("the first frame must be AttachHello, within 5 seconds"))??;
        let Some(TerminalClientFrame {
            frame: Some(ClientFrame::Hello(hello)),
        }) = first
        else {
            return Err(Status::invalid_argument("the first frame must be AttachHello"));
        };
        if hello.terminal_id != self.0.id {
            return Err(Status::not_found("no such terminal on this server"));
        }
        validate_client_id(&hello.client_id)?;
        let shared = self.0.clone();
        let attached = shared.attach(&hello)?;
        let (tx, rx) = mpsc::channel(32);
        let response = Response::new(frames(rx, shared.clone()));
        let Some((attachment, generation)) = attached else {
            // Closed already: the exit is all there is to say.
            let exit = shared
                .recorded_exit()
                .ok_or_else(|| Status::not_found("no such terminal on this server"))?;
            let _ = tx.try_send(Ok(frame(ServerFrame::Exited(exit_to_wire(
                exit.code,
                exit.signal,
                exit.left,
            )))));
            return Ok(response);
        };
        let flow = Arc::new(Flow {
            admitted: AtomicU64::new(0),
            base: AtomicU64::new(u64::MAX),
            upto: AtomicU64::new(0),
        });
        // Dropped when the client's side ends: the pump drops the attachment at once, whatever it was sending, which
        // discards input not yet written.
        let (sending, ended) = watch::channel(());
        let (pumping, mut pump_ended) = tokio::sync::oneshot::channel::<()>();
        {
            let shared = shared.clone();
            let flow = flow.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let _pumping = pumping;
                pump(attachment, tx, ended, shared, flow).await;
            });
        }
        let client = hello.client_id;
        // Only the pump holds the stream open: it ends when the attachment does, whatever the client still sends.
        let refusals = tx.downgrade();
        drop(tx);
        tokio::spawn(async move {
            let _sending = sending;
            loop {
                let message = tokio::select! {
                    message = inbound.message() => message,
                    _ = &mut pump_ended => break,
                };
                let refusal = match message {
                    Ok(Some(TerminalClientFrame { frame: Some(frame) })) => match frame {
                        ClientFrame::Input(input) if input.data.len() > TERM_INPUT_MAX => {
                            Some(Status::invalid_argument("an input frame carries at most 64 KiB"))
                        }
                        ClientFrame::Input(input) => {
                            if flow.admit(input.data.len() as u64) {
                                shared.with_current(generation, |term| term.input(&client, input.offset, input.data));
                                None
                            } else {
                                Some(Status::resource_exhausted(
                                    "more than 1 MiB of input is waiting for the program to read it",
                                ))
                            }
                        }
                        ClientFrame::Resize(size) => {
                            shared.with_current(generation, |term| term.resize(pty_size(&size)));
                            None
                        }
                        ClientFrame::Colors(colors) => {
                            shared.with_current(generation, |term| term.set_colors(colors_from_wire(&colors)));
                            None
                        }
                        ClientFrame::Hello(_) => Some(Status::invalid_argument("AttachHello comes once, first")),
                    },
                    Ok(Some(TerminalClientFrame { frame: None })) => Some(Status::invalid_argument("an empty frame")),
                    Ok(None) | Err(_) => break,
                };
                if let Some(refusal) = refusal {
                    // The attachment ends first, its unwritten input with it, whether or not the client is still
                    // reading to hear why.
                    let tx = refusals.upgrade();
                    drop(_sending);
                    if let Some(tx) = tx {
                        let _ = tokio::time::timeout(REFUSAL_WITHIN, tx.send(Err(refusal))).await;
                    }
                    return;
                }
            }
        });
        Ok(response)
    }

    async fn list_terminals(
        &self,
        _: Request<ListTerminalsRequest>,
    ) -> Result<Response<ListTerminalsResponse>, Status> {
        let shared = self.0.clone();
        let info = tokio::task::spawn_blocking(move || shared.info())
            .await
            .map_err(|error| Status::internal(error.to_string()))?;
        Ok(Response::new(ListTerminalsResponse {
            terminals: vec![info],
            truncated: false,
        }))
    }

    async fn close_terminal(
        &self,
        request: Request<CloseTerminalRequest>,
    ) -> Result<Response<CloseTerminalResponse>, Status> {
        let request = request.into_inner();
        if request.terminal_id != self.0.id {
            return Err(Status::not_found("no such terminal on this server"));
        }
        sidecars::validate_command_id(&request.command_id)?;
        let shared = self.0.clone();
        // A caller disconnecting must not cancel a close half way.
        let closed = tokio::spawn(async move {
            let closed = shared.close(DetachReason::Closed).await;
            shared.request_shutdown();
            closed
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))?;
        Ok(closed.response())
    }
}

/// A client id: 1 to `TERM_ID_MAX` bytes of letters, digits and `._:-`.
fn validate_client_id(id: &str) -> Result<(), Status> {
    if id.is_empty() || id.len() > TERM_ID_MAX || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
    {
        return Err(Status::invalid_argument(
            "invalid client_id: expected 1..64 letters, digits or ._:-",
        ));
    }
    Ok(())
}

fn frame(frame: ServerFrame) -> TerminalServerFrame {
    TerminalServerFrame { frame: Some(frame) }
}

/// The attachment's readiness pipe, borrowed for the reactor; the attachment outlives it.
struct Ready(std::os::fd::RawFd);
impl AsRawFd for Ready {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.0
    }
}

/// Moves one attachment's items to its stream until the attachment ends, the stream goes, the client's side ends, or
/// the terminal closes (then it says why and ends first). Every wait also watches the client's side, so a client that
/// stopped reading cannot keep its attachment, and its unwritten input, alive.
async fn pump(
    attachment: Attachment,
    tx: mpsc::Sender<Result<TerminalServerFrame, Status>>,
    mut ended: watch::Receiver<()>,
    shared: Arc<Shared>,
    flow: Arc<Flow>,
) {
    let mut closing = shared.closing.subscribe();
    let Ok(ready) =
        tokio::io::unix::AsyncFd::with_interest(Ready(attachment.ready().as_raw_fd()), tokio::io::Interest::READABLE)
    else {
        return;
    };
    'pump: loop {
        // Drain first: the readiness pipe is edge-reported, and items may already wait.
        loop {
            let closed = *closing.borrow();
            if let Some(reason) = closed {
                send(&tx, frame(ServerFrame::Detached(Detached { reason })), &mut ended).await;
                break 'pump;
            }
            let Some(item) = attachment.next() else {
                break;
            };
            let (frames, end) = frames_of(item, &shared, &flow);
            for frame in frames {
                if !send(&tx, frame, &mut ended).await {
                    break 'pump;
                }
            }
            if end {
                break 'pump;
            }
        }
        tokio::select! {
            guard = ready.readable() => match guard {
                Ok(mut guard) => guard.clear_ready(),
                Err(_) => break 'pump,
            },
            _ = ended.changed() => break 'pump,
            _ = closing.changed() => {}
        }
    }
    drop(ready);
    drop(attachment);
}

/// Sends one frame unless the client's side ends first.
async fn send(
    tx: &mpsc::Sender<Result<TerminalServerFrame, Status>>,
    frame: TerminalServerFrame,
    ended: &mut watch::Receiver<()>,
) -> bool {
    tokio::select! {
        sent = tx.send(Ok(frame)) => sent.is_ok(),
        _ = ended.changed() => false,
    }
}

/// The frames of one item, and whether the attachment ends with it.
fn frames_of(item: StreamItem, shared: &Shared, flow: &Flow) -> (Vec<TerminalServerFrame>, bool) {
    match item {
        StreamItem::Snapshot(snapshot) => {
            flow.written(snapshot.input_written);
            (snapshot_frames(snapshot), false)
        }
        StreamItem::Output { offset, bytes } => (
            bytes
                .chunks(TERM_OUTPUT_MAX)
                .scan(offset, |at, chunk| {
                    let start = *at;
                    *at += chunk.len() as u64;
                    Some(frame(ServerFrame::Output(OutputBytes {
                        offset: start,
                        data: chunk.to_vec(),
                    })))
                })
                .collect(),
            false,
        ),
        StreamItem::Resized { offset, size } => (
            vec![frame(ServerFrame::Resized(Resized {
                offset,
                size: Some(size_to_wire(size)),
            }))],
            false,
        ),
        StreamItem::InputWritten(upto) => {
            flow.written(upto);
            (vec![frame(ServerFrame::InputWritten(InputWritten { upto }))], false)
        }
        StreamItem::Exited(exit) => {
            shared.record_exit(exit);
            (
                vec![frame(ServerFrame::Exited(exit_to_wire(
                    exit.code,
                    exit.signal,
                    exit.detached,
                )))],
                true,
            )
        }
        StreamItem::Detached(why) => {
            let reason = match why {
                DetachWhy::Replaced => DetachReason::Replaced,
                DetachWhy::Stopped => DetachReason::Stopped,
                DetachWhy::Closed => (*shared.closing.borrow())
                    .and_then(|reason| DetachReason::try_from(reason).ok())
                    .unwrap_or(DetachReason::Closed),
            };
            (
                vec![frame(ServerFrame::Detached(Detached { reason: reason as i32 }))],
                true,
            )
        }
        StreamItem::Waiting(why) => {
            let reason = match why {
                WaitWhy::OpenSequence => WaitReason::OpenSequence,
                WaitWhy::TooDeep => WaitReason::TooDeep,
                WaitWhy::SnapshotTooLarge => WaitReason::SnapshotTooLarge,
            };
            (
                vec![frame(ServerFrame::Waiting(Waiting { reason: reason as i32 }))],
                false,
            )
        }
    }
}

/// A snapshot as consecutive parts of at most `TERM_SNAPSHOT_PART_MAX` replay bytes; the last part carries the tail
/// and the facts. A tail read ahead goes as `TAIL_KIND_READ_AHEAD` even when empty, so the record round-trips.
pub fn snapshot_frames(snapshot: Snapshot) -> Vec<TerminalServerFrame> {
    let Snapshot {
        offset,
        replay,
        tail,
        history_at_cap,
        history_trimmed,
        output_while_detached,
        input_written,
    } = snapshot;
    let total_bytes = replay.bytes.len() as u64;
    let parts: Vec<&[u8]> = if replay.bytes.is_empty() {
        vec![&[]]
    } else {
        replay.bytes.chunks(TERM_SNAPSHOT_PART_MAX).collect()
    };
    let count = parts.len();
    let (tail_kind, tail_bytes, tail_calls) = match tail {
        Tail::ReadAhead(bytes) => (TailKind::ReadAhead, bytes, 0),
        Tail::Raw { bytes, calls } => (TailKind::Raw, bytes, u32::try_from(calls).unwrap_or(u32::MAX)),
    };
    let mut tail_bytes = Some(tail_bytes);
    parts
        .into_iter()
        .enumerate()
        .map(|(index, part)| {
            let last = index + 1 == count;
            frame(ServerFrame::Snapshot(SnapshotPart {
                offset,
                columns: replay.columns.into(),
                rows: replay.rows.into(),
                total_bytes,
                replay: part.to_vec(),
                last,
                last_input: if last { replay.last_input.map(u32::from) } else { None },
                tail_kind: if last { tail_kind as i32 } else { TailKind::None as i32 },
                tail: if last {
                    tail_bytes.take().unwrap_or_default()
                } else {
                    Vec::new()
                },
                tail_calls: if last { tail_calls } else { 0 },
                history_at_cap: last && history_at_cap,
                history_trimmed: if last { history_trimmed } else { 0 },
                output_while_detached: last && output_while_detached,
                input_written: if last { input_written } else { 0 },
            }))
        })
        .collect()
}
