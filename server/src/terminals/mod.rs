//! Terminal hosts: one detached `eitri-server term-host` process per terminal, started, adopted, listed and stopped
//! by the daemon the way it handles sidecars. A daemon that dies loses no terminal; a host that dies loses only its
//! own.

#[path = "adopt.rs"]
mod adopt;
#[path = "host.rs"]
pub mod host;
#[path = "session.rs"]
pub mod session;

use std::collections::{BTreeMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex, Weak,
};
use std::time::Duration;

use agent::private_fs;
use eitri_server_proto::terminal_client::TerminalClient;
use eitri_server_proto::{
    CloseTerminalRequest, CloseTerminalResponse, ListTerminalsRequest, ListTerminalsResponse, OpenTerminalRequest,
    TermColors, TermSize, TerminalInfo, TERM_COLUMNS_MAX, TERM_LIST_MAX, TERM_NAME_MAX, TERM_PATH_MAX, TERM_ROWS_MAX,
};
use serde::{Deserialize, Serialize};
use tokio::process::Child;
use tonic::transport::Channel;
use tonic::{Response, Status};

use super::login_env::{self, Environment};
use super::sidecars::{self, Sidecars};
use crate::run_dir::Paths;

pub use adopt::ExitRecord;

/// At most this many terminal hosts run for one daemon, exited ones still holding their screen included.
pub const MAX_HOSTS: usize = 32;
/// A closed terminal's shell session gets this long after its hangup before what is left is killed.
pub const CLOSE_GRACE: Duration = Duration::from_secs(3);
/// How long, past [`CLOSE_GRACE`], a stopping daemon waits for its hosts before it kills them.
pub const STOP_BUDGET: Duration = Duration::from_secs(5);
/// A host must answer its handshake within this long of its start, as a sidecar must.
const BIND_BUDGET: Duration = Duration::from_secs(15);
/// After the post-kill wait, a host that still has not exited is reported, not waited for.
const KILL_BUDGET: Duration = Duration::from_secs(2);
/// Exit records, and the records of hosts that ended without one, are kept this long.
pub const RETENTION_MS: u64 = 24 * 3600 * 1000;
/// At most this many exited hosts stay up holding their final screen; older ones are told to go (their exit records
/// stay).
pub const EXITED_UP_MAX: usize = 16;
/// At most this many records of ended hosts (exit records and gone hosts) are kept; the oldest go first.
pub const ENDED_KEPT_MAX: usize = 256;
/// What a list says of a host that ended without writing an exit record (killed, or crashed).
pub use eitri_server_proto::TERM_GONE_NOTE as GONE_NOTE;
const PASSWD_BUDGET: Duration = Duration::from_secs(5);
const DEFAULT_LANG: &str = "C.UTF-8";
const LANG_MAX: usize = 256;

/// How the daemon starts its hosts.
pub struct Config {
    /// The executable run as `term-host`: the daemon's own (`/proc/self/exe`, which names the running file even after
    /// an install replaced it).
    pub host_program: PathBuf,
    /// The account identity the shell gets (`login_env::IDENTITY_KEYS`); `None` looks it up with `passwd`.
    pub identity: Option<Environment>,
    /// The passwd lookup used when `identity` is `None`.
    pub passwd: fn() -> io::Result<Environment>,
    /// Faults tests inject; all off in a daemon.
    pub faults: Faults,
}

/// Failures a test injects into the daemon side.
#[derive(Clone, Copy, Debug, Default)]
pub struct Faults {
    /// The argument pipe is shrunk to one page.
    pub small_args_pipe: bool,
    /// Killing a host fails, as for one that does not exit within its budget.
    pub fail_kills: bool,
    #[cfg(test)]
    fail_session_end: bool,
    #[cfg(test)]
    exit_after_stop_pin: bool,
    #[cfg(test)]
    read_record_dir: Option<fn(&Path) -> io::Result<std::fs::ReadDir>>,
    #[cfg(test)]
    host_record_read: Option<fn(&Path, u64) -> io::Result<Vec<u8>>>,
    #[cfg(test)]
    shell_start_time: Option<fn(u32) -> io::Result<u64>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host_program: PathBuf::from("/proc/self/exe"),
            identity: None,
            passwd: login_env::passwd_identity,
            faults: Faults::default(),
        }
    }
}

/// A terminal host's private `th-<id>.pid` record, written by the daemon that started it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HostRecord {
    pub pid: u32,
    pub start_time: u64,
    /// The boot the pid and start time belong to: start times count from boot, so after a reboot another process
    /// can have both. A record of another boot, or none, is never pinned.
    #[serde(default)]
    pub boot_id: String,
    pub socket: PathBuf,
    #[serde(default)]
    pub terminal_id: String,
    #[serde(default)]
    pub start_dir: String,
    #[serde(default)]
    pub command_id: String,
    #[serde(default)]
    pub started_unix_ms: u64,
    #[serde(default)]
    pub ready: bool,
    #[serde(default)]
    pub failure: Option<String>,
    /// When a daemon first saw the host process gone.
    #[serde(default)]
    pub ended_unix_ms: Option<u64>,
}

/// What a list says of one terminal host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostState {
    /// The host runs and its shell has not exited. `handshake` is false until it has answered.
    Live { handshake: bool },
    /// The shell exited; `host_alive` while the host still holds its final screen.
    Exited { exit: ExitRecord, host_alive: bool },
    /// The host ended without an exit record ([`GONE_NOTE`]).
    Gone,
    /// The start failed, or the record could not be adopted, and why; `alive` while its process still runs.
    Failed { why: String, alive: bool },
}

#[derive(Clone, Debug)]
pub struct HostInfo {
    pub terminal_id: String,
    pub start_dir: String,
    pub command_id: String,
    pub started_unix_ms: u64,
    pub state: HostState,
}

/// Who stopped a host, for the audit log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopBy {
    Client,
    DaemonStop,
}

impl StopBy {
    fn as_str(self) -> &'static str {
        match self {
            StopBy::Client => "client",
            StopBy::DaemonStop => "daemon-stop",
        }
    }
}

struct Entry {
    record: HostRecord,
    pidfd: Option<Arc<OwnedFd>>,
    /// An identity check failed. Keep the host's slot and records until a check succeeds.
    pin_pending: Option<PinPending>,
    child: Option<Child>,
    handshake: bool,
    hidden_socket_audited: bool,
    exit: Option<ExitRecord>,
    /// A record that could not be adopted: never rewritten or stopped by maintenance.
    refused: bool,
    /// An exited host past the count kept up has been told to go.
    told_to_go: bool,
}

enum PinPending {
    Read(String),
    Identity(String),
}

impl PinPending {
    fn reason(&self) -> &str {
        match self {
            Self::Read(why) | Self::Identity(why) => why,
        }
    }
}

impl Entry {
    fn alive(&self) -> bool {
        self.pin_pending.is_some()
            || self
                .pidfd
                .as_ref()
                .is_some_and(|fd| !sidecars::exited(fd).unwrap_or(false))
    }

    fn reap(&mut self) {
        if self.alive() {
            return;
        }
        self.pidfd = None;
        if let Some(child) = &mut self.child {
            if matches!(child.try_wait(), Ok(Some(_))) {
                self.child = None;
            }
        }
    }
}

type SharedEntry = Arc<Mutex<Entry>>;

/// One passwd lookup at a time: a lookup that outlives its caller's budget (a stalled NSS) is kept, and the next
/// caller waits for that same lookup within its own budget instead of starting another.
pub struct PasswdLookup {
    lookup: fn() -> io::Result<Environment>,
    state: tokio::sync::Mutex<(
        Option<Environment>,
        Option<tokio::task::JoinHandle<io::Result<Environment>>>,
    )>,
}

impl PasswdLookup {
    pub fn new(lookup: fn() -> io::Result<Environment>) -> Self {
        Self {
            lookup,
            state: tokio::sync::Mutex::new((None, None)),
        }
    }

    pub async fn get(&self, budget: Duration) -> Result<Environment, Status> {
        let lookup = self.lookup;
        tokio::time::timeout(budget, async {
            let mut state = self.state.lock().await;
            if let Some(identity) = &state.0 {
                return Ok(identity.clone());
            }
            let worker = state.1.get_or_insert_with(|| tokio::task::spawn_blocking(lookup));
            let result = (&mut *worker).await;
            state.1 = None;
            let identity = result
                .map_err(internal)?
                .map_err(|error| Status::unavailable(error.to_string()))?;
            state.0 = Some(identity.clone());
            Ok(identity)
        })
        .await
        .map_err(|_| Status::unavailable("passwd lookup timed out"))?
    }
}

pub struct TerminalHosts {
    paths: Paths,
    config: Config,
    sidecars: Arc<Sidecars>,
    passwd: PasswdLookup,
    entries: Mutex<BTreeMap<String, SharedEntry>>,
    rescan_needed: Mutex<bool>,
    starts: Mutex<BTreeMap<String, Weak<tokio::sync::Mutex<()>>>>,
    shutting_down: AtomicBool,
    /// Starts that hold a place under [`MAX_HOSTS`] but are not registered yet; a stop waits for them.
    starting: AtomicUsize,
    started: tokio::sync::Notify,
    /// Closes already answered, by command id, so a retried close answers the same.
    closes: Mutex<CloseCache>,
}

#[derive(Default)]
struct CloseCache {
    by_command: BTreeMap<String, (String, ClosedTerminal)>,
    order: VecDeque<String>,
}

struct ClosedTerminal {
    exit: Option<ExitRecord>,
    session_end_failed: bool,
}

const REFUSED_HOST: &str =
    "this terminal runs under another run directory; close it or restart the server with the same TMPDIR";
const CLOSE_WARNING: &str = "eitri-terminal-close-warning";
const UNCONFIRMED_SESSION: &str = "the terminal's shell or its session could not be confirmed gone";

impl ClosedTerminal {
    fn response(&self) -> Response<CloseTerminalResponse> {
        let mut response = Response::new(CloseTerminalResponse {
            exit: self
                .exit
                .map(|exit| host::exit_to_wire(exit.code, exit.signal, exit.left)),
        });
        if self.session_end_failed {
            response.metadata_mut().insert(
                CLOSE_WARNING,
                tonic::metadata::MetadataValue::from_static(UNCONFIRMED_SESSION),
            );
        }
        response
    }
}

/// Where an attach goes: a running host's socket, or an ended terminal's exit record.
pub enum AttachTarget {
    Live(Channel),
    Exited(ExitRecord),
}

/// Closes remembered for retries.
const CLOSES_KEPT: usize = 256;

fn internal(error: impl std::fmt::Display) -> Status {
    Status::internal(error.to_string())
}

/// A place under [`MAX_HOSTS`] held by a start until it registers its host or gives up.
struct Reservation<'a>(&'a TerminalHosts);

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.0.starting.fetch_sub(1, Ordering::SeqCst);
        self.0.started.notify_waiters();
    }
}

/// The files a start made, removed unless the start registers its host.
struct Artifacts<'a> {
    paths: &'a Paths,
    id: String,
    keep: bool,
}

impl Drop for Artifacts<'_> {
    fn drop(&mut self) {
        if !self.keep {
            let _ = adopt::remove_files(self.paths, &self.id);
        }
    }
}

impl TerminalHosts {
    /// Reads every host record of this run directory: a host whose pid, start time and socket still match is adopted;
    /// the others are listed as exited (an exit record) or gone. A record that cannot be adopted is audited and
    /// listed as failed, and a process still running as it records is kept stoppable through its pidfd.
    pub fn new(paths: Paths, config: Config, sidecars: Arc<Sidecars>) -> Self {
        Self::new_with_pin(paths, config, sidecars, adopt::pin_record)
    }

    fn new_with_pin(
        paths: Paths,
        config: Config,
        sidecars: Arc<Sidecars>,
        pin: impl Fn(&HostRecord) -> io::Result<Option<OwnedFd>>,
    ) -> Self {
        let passwd = PasswdLookup::new(config.passwd);
        let hosts = Self {
            paths,
            config,
            sidecars,
            passwd,
            entries: Mutex::new(BTreeMap::new()),
            rescan_needed: Mutex::new(true),
            starts: Mutex::new(BTreeMap::new()),
            shutting_down: AtomicBool::new(false),
            starting: AtomicUsize::new(0),
            started: tokio::sync::Notify::new(),
            closes: Mutex::default(),
        };
        if let Err(error) = hosts.rescan_with(pin) {
            let _ = crate::audit::record(&hosts.paths.state, "terminal-adopt-error", &error.to_string());
        }
        hosts
    }

    fn rescan(&self) -> io::Result<()> {
        self.rescan_with(adopt::pin_record)
    }

    fn rescan_with(&self, pin: impl Fn(&HostRecord) -> io::Result<Option<OwnedFd>>) -> io::Result<()> {
        let mut needed = self.rescan_needed.lock().expect("terminal record scan");
        if !*needed {
            return Ok(());
        }
        let paths = &self.paths;
        for id in adopt::record_ids(paths, &self.config.faults)? {
            // A start registers its host before publishing the pid record.
            if self.entries.lock().expect("terminal registry").contains_key(&id) {
                continue;
            }
            let (mut record, refusal, pidfd, pin_pending) = match adopt::read_record(paths, &id, &self.config.faults) {
                Ok((record, refusal)) => {
                    let (fd, pending) = match pin(&record) {
                        Ok(fd) => (fd.map(Arc::new), None),
                        Err(error) => (
                            None,
                            Some(PinPending::Identity(format!(
                                "terminal host identity could not be checked: {error}"
                            ))),
                        ),
                    };
                    (record, refusal, fd, pending)
                }
                Err(error) => {
                    let record = HostRecord {
                        terminal_id: id.clone(),
                        ..HostRecord::default()
                    };
                    let pending = PinPending::Read(format!("terminal host record could not be read: {error}"));
                    (record, None, None, Some(pending))
                }
            };
            let exit = adopt::read_exit(paths, &id);
            if let Some(pending) = &pin_pending {
                let _ = crate::audit::record(
                    &paths.state,
                    "terminal-adopt-error",
                    &format!("id={id} pid={} {}", record.pid, pending.reason()),
                );
            }
            if let Some(why) = &refusal {
                let _ = crate::audit::record(
                    &paths.state,
                    "terminal-record-refused",
                    &format!("id={id} pid={} {why}", record.pid),
                );
            } else if pidfd.is_some() {
                let _ = crate::audit::record(&paths.state, "terminal-adopt", &format!("id={id} pid={}", record.pid));
            } else if pin_pending.is_none() && record.ended_unix_ms.is_none() {
                record.ended_unix_ms = Some(crate::run_dir::unix_ms());
                let _ = adopt::persist(paths, &record);
                if exit.is_none() && record.failure.is_none() {
                    let _ = crate::audit::record(&paths.state, "terminal-gone", &format!("id={id} pid={}", record.pid));
                }
            }
            self.entries.lock().expect("terminal registry").insert(
                id,
                Arc::new(Mutex::new(Entry {
                    record,
                    pidfd,
                    pin_pending,
                    child: None,
                    handshake: false,
                    hidden_socket_audited: false,
                    exit,
                    refused: refusal.is_some(),
                    told_to_go: false,
                })),
            );
        }
        *needed = false;
        Ok(())
    }

    fn retry_pin(&self, e: &mut Entry) -> io::Result<()> {
        if e.pin_pending.is_some() {
            if matches!(e.pin_pending, Some(PinPending::Read(_))) {
                let (record, refusal) = adopt::read_record(&self.paths, &e.record.terminal_id, &self.config.faults)?;
                if let Some(why) = &refusal {
                    let _ = crate::audit::record(
                        &self.paths.state,
                        "terminal-record-refused",
                        &format!("id={} pid={} {why}", record.terminal_id, record.pid),
                    );
                }
                e.record = record;
                e.refused = refusal.is_some();
            }
            e.pidfd = match adopt::pin_record(&e.record) {
                Ok(fd) => fd.map(Arc::new),
                Err(error) => {
                    e.pin_pending = Some(PinPending::Identity(format!(
                        "terminal host identity could not be checked: {error}"
                    )));
                    return Err(error);
                }
            };
            e.pin_pending = None;
            if e.pidfd.is_some() && !e.refused {
                let _ = crate::audit::record(
                    &self.paths.state,
                    "terminal-adopt",
                    &format!("id={} pid={}", e.record.terminal_id, e.record.pid),
                );
            }
        }
        Ok(())
    }

    fn snapshot(&self) -> Vec<SharedEntry> {
        self.entries
            .lock()
            .expect("terminal registry")
            .values()
            .cloned()
            .collect()
    }

    fn entry(&self, id: &str) -> Result<SharedEntry, Status> {
        self.rescan().map_err(|error| Status::unavailable(error.to_string()))?;
        self.entries
            .lock()
            .expect("terminal registry")
            .get(id)
            .cloned()
            .ok_or_else(|| Status::not_found("no such terminal on this server"))
    }

    fn start_lock(&self, command_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut starts = self.starts.lock().expect("start registry");
        starts.retain(|_, weak| weak.strong_count() != 0);
        let serial = starts
            .get(command_id)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| Arc::new(tokio::sync::Mutex::new(())));
        starts.insert(command_id.to_owned(), Arc::downgrade(&serial));
        serial
    }

    /// Holds a place for a new host, counting running hosts and other starts still on their way.
    fn reserve(&self) -> Result<Reservation<'_>, Status> {
        let entries = self.entries.lock().expect("terminal registry");
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(Status::unavailable("daemon is stopping"));
        }
        let running = entries
            .values()
            .filter(|entry| entry.lock().expect("terminal entry").alive())
            .count();
        if running + self.starting.load(Ordering::SeqCst) >= MAX_HOSTS {
            return Err(Status::resource_exhausted("too many terminals on this server"));
        }
        self.starting.fetch_add(1, Ordering::SeqCst);
        Ok(Reservation(self))
    }

    /// Starts a terminal host for `request` and answers its terminal id once the host has handshaken. A request whose
    /// command id already started a host answers that host's id; a start of it that never finished (its daemon was
    /// killed, or its caller went, before readiness) is finished when its host answers, or stopped, first.
    pub async fn start(&self, request: OpenTerminalRequest) -> Result<String, Status> {
        sidecars::validate_command_id(&request.command_id)?;
        let serial = self.start_lock(&request.command_id);
        let _serial = serial.lock().await;
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(Status::unavailable("daemon is stopping"));
        }
        // A retried open answers the terminal its first attempt started before anything about a new one is checked:
        // its start directory may have moved since, and the terminal still runs.
        self.rescan().map_err(|error| Status::unavailable(error.to_string()))?;
        if let Some(id) = self.recover_start(&request.command_id).await? {
            return Ok(id);
        }
        let dir = open_start_dir(&request.cwd)?;
        let start_dir = sidecars::canonical_root(&dir)?;
        if start_dir.len() > TERM_PATH_MAX {
            return Err(Status::invalid_argument("start directory exceeds byte cap"));
        }
        let size = host::size_from_wire(request.size.as_ref());
        let colors = request.colors.unwrap_or_else(default_colors);
        let reservation = self.reserve()?;
        let id = new_id().map_err(internal)?;
        let socket = agent::socket_path::in_dir(&self.paths.run, &format!("th-{id}.sock"))
            .map_err(|error| Status::failed_precondition(error.to_string()))?;
        let identity = self.identity().await?;
        let lang = self
            .sidecars
            .login_lang()
            .await
            .filter(|lang| !lang.is_empty() && lang.len() <= LANG_MAX)
            .unwrap_or_else(|| DEFAULT_LANG.into());
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(Status::unavailable("daemon is stopping"));
        }
        let mut artifacts = Artifacts {
            paths: &self.paths,
            id: id.clone(),
            keep: false,
        };
        let args = host::HostArgs {
            terminal_id: id.clone(),
            command_id: request.command_id.clone(),
            start_dir: start_dir.clone(),
            size,
            colors,
            run: self.paths.run.clone(),
            metadata: self.paths.metadata_run(),
            state: self.paths.state.clone(),
            env: shell_environment(&identity, lang),
        };
        let payload = args.encode();
        if payload.len() >= 60 * 1024 {
            return Err(Status::invalid_argument(
                "terminal host arguments exceed their byte cap",
            ));
        }
        // The arguments travel through a pipe the host reads after its start, never the command line. The pipe is
        // written only once the host runs: a pipe may hold as little as a page, and nothing reads it before then.
        let (args_read, args_write) = std::io::pipe().map_err(internal)?;
        if self.config.faults.small_args_pipe {
            // SAFETY: fcntl on a descriptor this function owns.
            unsafe { libc::fcntl(args_write.as_raw_fd(), libc::F_SETPIPE_SZ, 4096) };
        }
        let metadata = self.paths.metadata_run();
        let mut command = std::process::Command::new(&self.config.host_program);
        command
            .arg0("eitri-server")
            .args(["term-host", "--args-fd", &host::ARGS_FD.to_string()])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(private_fs::append_private(&metadata.join(format!("th-{id}.out"))).map_err(internal)?)
            .stderr(private_fs::append_private(&metadata.join(format!("th-{id}.err"))).map_err(internal)?);
        // The host starts in the directory this descriptor holds, so the shell gets the directory checked here.
        let dir_fd = dir.as_raw_fd();
        let args_fd = args_read.as_raw_fd();
        let umask = crate::run_dir::inherited_umask();
        unsafe {
            command.pre_exec(move || {
                sidecars::child_setup(dir_fd, umask)?;
                // After the directory change: the directory's descriptor may itself be the argument slot.
                if args_fd == host::ARGS_FD {
                    if libc::fcntl(args_fd, libc::F_SETFD, 0) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                } else if libc::dup2(args_fd, host::ARGS_FD) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut starting = sidecars::Starting(Some(tokio::process::Command::from(command).spawn().map_err(internal)?));
        drop(args_read);
        drop(dir);
        let pid = starting.0.as_ref().and_then(Child::id).expect("new child pid");
        let pidfd = Arc::new(sidecars::pin(pid).map_err(internal)?);
        let record = HostRecord {
            pid,
            // The child is unreaped, so its identity stands even if it has already exited.
            start_time: sidecars::identity_start_time(pid).map_err(internal)?,
            boot_id: current_boot_id().map_err(internal)?,
            socket: socket.clone(),
            terminal_id: id.clone(),
            start_dir: start_dir.clone(),
            command_id: request.command_id,
            started_unix_ms: crate::run_dir::unix_ms(),
            ready: false,
            failure: None,
            ended_unix_ms: None,
        };
        // Registered before any blocking work, so a stop stops it; a stop already under way makes it stop itself.
        let entry = {
            let mut entries = self.entries.lock().expect("terminal registry");
            if self.shutting_down.load(Ordering::SeqCst) {
                return Err(Status::unavailable("daemon is stopping"));
            }
            let entry = Arc::new(Mutex::new(Entry {
                record,
                pidfd: Some(pidfd.clone()),
                pin_pending: None,
                child: starting.0.take(),
                handshake: false,
                hidden_socket_audited: false,
                exit: None,
                refused: false,
                told_to_go: false,
            }));
            entries.insert(id.clone(), entry.clone());
            entry
        };
        artifacts.keep = true;
        drop(reservation);
        let persisted = {
            let record = entry.lock().expect("terminal entry").record.clone();
            adopt::persist(&self.paths, &record).and_then(|()| {
                crate::audit::record(
                    &self.paths.state,
                    "terminal-start",
                    &format!("id={id} dir={start_dir:?} pid={pid}"),
                )
            })
        };
        if let Err(error) = persisted {
            return Err(self.fail_start(&entry, error.to_string()).await);
        }
        let written = async {
            let file = std::fs::File::from(OwnedFd::from(args_write));
            let mut sender = tokio::net::unix::pipe::Sender::from_file(file)?;
            tokio::io::AsyncWriteExt::write_all(&mut sender, payload.as_bytes()).await
        };
        match tokio::time::timeout(BIND_BUDGET, written).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                return Err(self
                    .fail_start(&entry, format!("terminal host arguments: {error}"))
                    .await);
            }
            Err(_) => {
                return Err(self
                    .fail_start(&entry, "terminal host did not read its arguments".into())
                    .await)
            }
        }
        let readiness = tokio::time::timeout(BIND_BUDGET, async {
            loop {
                if self.shutting_down.load(Ordering::SeqCst) {
                    return Err(io::Error::other("daemon is stopping"));
                }
                if sidecars::exited(&pidfd)? {
                    return Err(io::Error::other("terminal host exited before binding"));
                }
                if handshake(&socket, pid, &id).await.is_ok() {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "terminal host bind timed out")));
        match readiness {
            Ok(()) => self.finish_start(&entry).await,
            Err(error) => Err(self.fail_start(&entry, error.to_string()).await),
        }
    }

    async fn finish_start(&self, entry: &SharedEntry) -> Result<String, Status> {
        let result = {
            let mut e = entry.lock().expect("terminal entry");
            if self.shutting_down.load(Ordering::SeqCst) || !e.alive() {
                Err(Status::unavailable("terminal host stopped during startup"))
            } else {
                e.record.ready = true;
                e.handshake = true;
                adopt::persist(&self.paths, &e.record)
                    .map_err(internal)
                    .map(|_| e.record.terminal_id.clone())
            }
        };
        match result {
            Ok(id) => Ok(id),
            Err(error) => Err(self.fail_start(entry, error.message().to_owned()).await),
        }
    }

    async fn fail_start(&self, entry: &SharedEntry, error: String) -> Status {
        let stopped = self.kill_host(entry).await;
        let mut e = entry.lock().expect("terminal entry");
        e.record.failure = Some(error.clone());
        e.record.ready = false;
        let _ = adopt::persist(&self.paths, &e.record);
        let _ = crate::audit::record(
            &self.paths.state,
            "terminal-start-failed",
            &format!("id={} {error}", e.record.terminal_id),
        );
        match stopped {
            Ok(()) => Status::unavailable(error),
            Err(stop_error) => internal(format!("{error}; terminal cleanup failed: {stop_error}")),
        }
    }

    /// The terminal `command_id` already started: a ready one answers at once; one whose start never finished is
    /// handshaken and finished, or stopped and marked failed, before any other host is started for it.
    async fn recover_start(&self, command_id: &str) -> Result<Option<String>, Status> {
        self.recover_start_with(command_id, handshake).await
    }

    async fn recover_start_with(
        &self,
        command_id: &str,
        handshake: impl AsyncFn(&Path, u32, &str) -> io::Result<()>,
    ) -> Result<Option<String>, Status> {
        for entry in self.snapshot() {
            let candidate = {
                let mut e = entry.lock().expect("terminal entry");
                let pinned = self.retry_pin(&mut e);
                // An unread record could belong to this open; only a known mismatch is safe to skip.
                if !matches!(e.pin_pending, Some(PinPending::Read(_))) && e.record.command_id != command_id {
                    continue;
                }
                pinned.map_err(|error| Status::unavailable(error.to_string()))?;
                if let Some(why) = &e.record.failure {
                    if e.alive() {
                        if e.refused {
                            return Err(Status::failed_precondition(REFUSED_HOST));
                        }
                        return Err(Status::unavailable(format!(
                            "an earlier start of this open is still running: {why}"
                        )));
                    }
                    continue;
                }
                if e.record.ready {
                    return Ok(Some(e.record.terminal_id.clone()));
                }
                e.alive().then(|| {
                    (
                        e.record.socket.clone(),
                        e.record.pid,
                        e.record.terminal_id.clone(),
                        e.record.started_unix_ms,
                    )
                })
            };
            if let Some((socket, pid, id, started)) = candidate {
                if handshake(&socket, pid, &id).await.is_ok() {
                    return self.finish_start(&entry).await.map(Some);
                }
                // A restart can find the original host still binding. Give it the rest of its startup window.
                let waited = tokio::time::timeout(bind_remaining(started), async {
                    while !self.shutting_down.load(Ordering::SeqCst) && entry.lock().expect("terminal entry").alive() {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        if handshake(&socket, pid, &id).await.is_ok() {
                            return true;
                        }
                    }
                    false
                })
                .await;
                if matches!(waited, Ok(true)) {
                    return self.finish_start(&entry).await.map(Some);
                }
            }
            let stopped = self
                .fail_start(&entry, "startup interrupted before readiness".into())
                .await;
            // Never a second host while the first may still run: maintenance retries the stop.
            if entry.lock().expect("terminal entry").alive() {
                return Err(Status::unavailable(format!(
                    "an earlier start of this open is still running: {}",
                    stopped.message()
                )));
            }
        }
        Ok(None)
    }

    async fn identity(&self) -> Result<Environment, Status> {
        if let Some(identity) = &self.config.identity {
            return Ok(identity.clone());
        }
        self.passwd.get(PASSWD_BUDGET).await
    }

    /// Every host this daemon knows, newest first: live, exited (with its exit record), gone, or failed.
    pub fn list(&self) -> Vec<HostInfo> {
        self.observe();
        let mut listed: Vec<_> = self
            .snapshot()
            .iter()
            .map(|entry| {
                let e = entry.lock().expect("terminal entry");
                let alive = e.alive();
                let failure = e
                    .record
                    .failure
                    .as_deref()
                    .or_else(|| e.pin_pending.as_ref().map(PinPending::reason));
                let state = match (failure, e.exit) {
                    (Some(why), _) => HostState::Failed {
                        why: why.to_owned(),
                        alive,
                    },
                    (None, Some(exit)) => HostState::Exited {
                        exit,
                        host_alive: alive,
                    },
                    (None, None) if alive => HostState::Live { handshake: e.handshake },
                    (None, None) => HostState::Gone,
                };
                HostInfo {
                    terminal_id: e.record.terminal_id.clone(),
                    start_dir: e.record.start_dir.clone(),
                    command_id: e.record.command_id.clone(),
                    started_unix_ms: e.record.started_unix_ms,
                    state,
                }
            })
            .collect();
        listed.sort_by(|a, b| b.started_unix_ms.cmp(&a.started_unix_ms));
        listed
    }

    /// Whether a host runs whose shell has not exited: such a host keeps the daemon from its idle exit. A host that
    /// only holds an exited shell's screen does not, since its exit record outlives the daemon.
    pub fn any_live(&self) -> bool {
        self.starting.load(Ordering::SeqCst) > 0
            || self.snapshot().iter().any(|entry| {
                let mut e = entry.lock().expect("terminal entry");
                if e.pin_pending.is_some() {
                    return true;
                }
                if !e.alive() {
                    return false;
                }
                if e.exit.is_none() {
                    e.exit = adopt::read_exit(&self.paths, &e.record.terminal_id);
                }
                e.exit.is_none()
            })
    }

    /// A client of the host's socket for `id`, with its peer uid and pid checked before the first byte.
    pub async fn connect(&self, id: &str) -> Result<TerminalClient<Channel>, Status> {
        let entry = self.entry(id)?;
        let (socket, pid, alive, refused) = {
            let e = entry.lock().expect("terminal entry");
            if let Some(pending) = &e.pin_pending {
                return Err(Status::unavailable(pending.reason().to_owned()));
            }
            (e.record.socket.clone(), e.record.pid, e.alive(), e.refused)
        };
        if !alive || refused {
            return Err(Status::not_found("the terminal host is not running"));
        }
        channel(&socket, pid)
            .await
            .map(TerminalClient::new)
            .map_err(|error| Status::unavailable(error.to_string()))
    }

    /// Where an attach to `id` goes: the running host (a fresh peer-checked connection for this one attachment), or the
    /// exit record of a terminal whose host has gone. A live host under another run directory refuses attachment.
    pub async fn attach_target(&self, id: &str) -> Result<AttachTarget, Status> {
        self.attach_target_with(id, channel).await
    }

    async fn attach_target_with(
        &self,
        id: &str,
        connect: impl AsyncFnOnce(&Path, u32) -> io::Result<Channel>,
    ) -> Result<AttachTarget, Status> {
        self.observe();
        let entry = self.entry(id)?;
        let (socket, pid, alive) = {
            let e = entry.lock().expect("terminal entry");
            if let Some(pending) = &e.pin_pending {
                return Err(Status::unavailable(pending.reason().to_owned()));
            }
            let alive = e.alive();
            if e.refused && alive {
                return Err(Status::failed_precondition(REFUSED_HOST));
            }
            (e.record.socket.clone(), e.record.pid, alive)
        };
        let connection_error = if alive {
            match connect(&socket, pid).await {
                Ok(channel) => return Ok(AttachTarget::Live(channel)),
                Err(error) => Some(Status::unavailable(error.to_string())),
            }
        } else {
            None
        };
        // The host can publish its exit and close the listener between observation and connection.
        let mut e = entry.lock().expect("terminal entry");
        if e.exit.is_none() {
            e.exit = adopt::read_exit(&self.paths, id);
        }
        match e.exit {
            Some(exit) if !e.alive() => Ok(AttachTarget::Exited(exit)),
            _ => Err(connection_error.unwrap_or_else(|| Status::not_found(GONE_NOTE))),
        }
    }

    /// Lists running hosts (as each reports itself), then ended terminals with an exit record, newest first in
    /// each group. An incomplete adoption scan or more than `TERM_LIST_MAX` entries marks the response truncated.
    pub async fn list_terminals(&self) -> ListTerminalsResponse {
        if self.rescan().is_err() {
            return ListTerminalsResponse {
                terminals: Vec::new(),
                truncated: true,
            };
        }
        let mut shown: Vec<_> = self
            .list()
            .into_iter()
            .filter(|info| matches!(info.state, HostState::Live { .. } | HostState::Exited { .. }))
            .collect();
        shown.sort_by_key(|info| matches!(info.state, HostState::Exited { host_alive: false, .. }));
        let truncated = shown.len() > TERM_LIST_MAX;
        shown.truncate(TERM_LIST_MAX);
        let terminals = futures_util::future::join_all(shown.into_iter().map(|info| async move {
            let reported = match &info.state {
                HostState::Live { .. } | HostState::Exited { host_alive: true, .. } => {
                    self.reported(&info.terminal_id).await
                }
                _ => None,
            };
            let mut wire = reported.unwrap_or_else(|| {
                let exit = match info.state {
                    HostState::Exited { exit, .. } => Some(host::exit_to_wire(exit.code, exit.signal, exit.left)),
                    _ => None,
                };
                TerminalInfo {
                    terminal_id: info.terminal_id.clone(),
                    start_dir: info.start_dir.clone(),
                    pid: adopt::read_shell(&self.paths, &info.terminal_id)
                        .ok()
                        .flatten()
                        .map_or(0, |shell| shell.pid),
                    started_unix_ms: info.started_unix_ms,
                    alive: exit.is_none(),
                    exit,
                    open_command_id: info.command_id.clone(),
                    ..Default::default()
                }
            });
            wire.start_dir = host::capped(&wire.start_dir, TERM_PATH_MAX);
            wire.cwd = host::capped(&wire.cwd, TERM_PATH_MAX);
            wire.foreground_name = host::capped(&wire.foreground_name, TERM_NAME_MAX);
            wire
        }))
        .await;
        ListTerminalsResponse { terminals, truncated }
    }

    /// What a running host says of its terminal, within two seconds; it must answer for the id asked.
    async fn reported(&self, id: &str) -> Option<TerminalInfo> {
        let mut client = self.connect(id).await.ok()?;
        let listed = tokio::time::timeout(Duration::from_secs(2), client.list_terminals(ListTerminalsRequest {}))
            .await
            .ok()?
            .ok()?
            .into_inner();
        listed
            .terminals
            .into_iter()
            .next()
            .filter(|info| info.terminal_id == id)
    }

    /// Answers a retried close with the first close's exit and any session cleanup warning.
    pub async fn close_by_command(
        &self,
        id: &str,
        command_id: &str,
    ) -> Result<Response<CloseTerminalResponse>, Status> {
        sidecars::validate_command_id(command_id)?;
        let serial = self.start_lock(&format!("close:{command_id}"));
        let _serial = serial.lock().await;
        if let Some((terminal_id, closed)) = self.closes.lock().expect("close registry").by_command.get(command_id) {
            if terminal_id == id {
                return Ok(closed.response());
            }
        }
        let closed = self.close_outcome(id, command_id).await?;
        let response = closed.response();
        let mut closes = self.closes.lock().expect("close registry");
        if closes.by_command.contains_key(command_id) {
            closes.order.retain(|key| key != command_id);
        } else if closes.order.len() == CLOSES_KEPT {
            let oldest = closes.order.pop_front().expect("full close cache");
            closes.by_command.remove(&oldest);
        }
        closes.order.push_back(command_id.to_owned());
        closes.by_command.insert(command_id.to_owned(), (id.to_owned(), closed));
        Ok(response)
    }

    #[cfg(test)]
    pub async fn close(&self, id: &str, command_id: &str) -> Result<Option<ExitRecord>, Status> {
        self.close_outcome(id, command_id).await.map(|closed| closed.exit)
    }

    /// Closes the terminal `id`: the host tells its client `Detached(Closed)`, hangs its shell's session up, kills
    /// what is left after [`CLOSE_GRACE`] and exits. A host that cannot be asked, or does not finish in time, is killed
    /// with its shell's session; a host already gone still has what is left of its shell's session ended. Answers the
    /// exit status, when the host reported or recorded one, and whether session cleanup failed.
    async fn close_outcome(&self, id: &str, command_id: &str) -> Result<ClosedTerminal, Status> {
        self.close_with(id, command_id, async |id, command_id| {
            let mut client = self.connect(id).await.ok()?;
            let reply = tokio::time::timeout(
                CLOSE_GRACE + STOP_BUDGET,
                client.close_terminal(CloseTerminalRequest {
                    terminal_id: id.to_owned(),
                    command_id: command_id.to_owned(),
                }),
            )
            .await
            .ok()?
            .ok()?;
            Some(reply)
        })
        .await
    }

    async fn close_with(
        &self,
        id: &str,
        command_id: &str,
        request_close: impl AsyncFnOnce(&str, &str) -> Option<Response<CloseTerminalResponse>>,
    ) -> Result<ClosedTerminal, Status> {
        sidecars::validate_command_id(command_id)?;
        let entry = self.entry(id)?;
        let alive = {
            let mut e = entry.lock().expect("terminal entry");
            self.retry_pin(&mut e).map_err(internal)?;
            e.alive()
        };
        let session_result = if alive {
            let closed = request_close(id, command_id).await;
            if let Some(exit) = closed.as_ref().and_then(|reply| reply.get_ref().exit.as_ref()) {
                let mut e = entry.lock().expect("terminal entry");
                // A host can know how its shell exited even when publishing the record failed.
                if e.exit.is_none() {
                    e.exit = Some(adopt::read_exit(&self.paths, id).unwrap_or_else(|| ExitRecord {
                        code: exit.code,
                        signal: exit.signal,
                        left: exit.left,
                        exited_unix_ms: crate::run_dir::unix_ms(),
                    }));
                }
            }
            let waited = closed.is_some() && {
                let pidfd = entry.lock().expect("terminal entry").pidfd.clone();
                match pidfd {
                    Some(fd) => matches!(
                        tokio::time::timeout(STOP_BUDGET, sidecars::wait_pidfd(&fd)).await,
                        Ok(Ok(()))
                    ),
                    None => true,
                }
            };
            let session_result = if waited {
                Ok(())
            } else {
                self.kill_host_with_session_result(&entry).await.map_err(internal)?
            };
            let session_result = session_result.and_then(|()| {
                if closed
                    .as_ref()
                    .is_some_and(|reply| reply.metadata().contains_key(CLOSE_WARNING))
                {
                    Err(io::Error::other(UNCONFIRMED_SESSION))
                } else {
                    Ok(())
                }
            });
            self.reap(&entry).await;
            let _ = crate::audit::record(
                &self.paths.state,
                "terminal-stop",
                &format!("id={id} by={}", StopBy::Client.as_str()),
            );
            session_result
        } else {
            self.end_session(id).await
        };
        // Once the host is gone, a session cleanup failure is a warning on the completed close.
        if let Err(error) = &session_result {
            let _ = crate::audit::record(&self.paths.state, "terminal-stop-error", &format!("id={id} {error}"));
        }
        self.observe();
        let mut e = entry.lock().expect("terminal entry");
        if e.exit.is_none() {
            e.exit = adopt::read_exit(&self.paths, id);
        }
        Ok(ClosedTerminal {
            exit: e.exit,
            session_end_failed: session_result.is_err(),
        })
    }

    /// The daemon is stopping: every host closes its terminal at once (its client gets `Detached(Stopped)`) and is
    /// waited for until [`CLOSE_GRACE`] and [`STOP_BUDGET`] from now, then killed with its shell's session. A start on
    /// its way is waited for first, within the same time; no new host starts after this.
    pub async fn stop_all(&self) -> io::Result<()> {
        {
            let _entries = self.entries.lock().expect("terminal registry");
            self.shutting_down.store(true, Ordering::SeqCst);
        }
        let deadline = tokio::time::Instant::now() + CLOSE_GRACE + STOP_BUDGET;
        loop {
            let started = self.started.notified();
            if self.starting.load(Ordering::SeqCst) == 0 {
                break;
            }
            if tokio::time::timeout_at(deadline, started).await.is_err() {
                break;
            }
        }
        // A start still before its spawn stops itself at registration; one still running here is reported.
        let unfinished = self.starting.load(Ordering::SeqCst);
        self.rescan()?;
        let entries = self.snapshot();
        let results = futures_util::future::join_all(entries.iter().map(|entry| self.stop_host(entry, deadline))).await;
        self.observe();
        results.into_iter().collect::<io::Result<Vec<_>>>()?;
        if unfinished > 0 {
            return Err(io::Error::other(format!(
                "{unfinished} terminal starts were still in progress; their terminals may still run"
            )));
        }
        Ok(())
    }

    async fn stop_host(&self, entry: &SharedEntry, deadline: tokio::time::Instant) -> io::Result<()> {
        let (fd, id) = {
            let mut e = entry.lock().expect("terminal entry");
            self.retry_pin(&mut e)?;
            e.reap();
            (e.pidfd.clone(), e.record.terminal_id.clone())
        };
        let Some(fd) = fd else {
            return self.end_session(&id).await;
        };
        #[cfg(test)]
        if self.config.faults.exit_after_stop_pin {
            sidecars::signal(&fd, libc::SIGKILL)?;
            sidecars::wait_pidfd(&fd).await?;
        }
        // SIGTERM asks the host to close its terminal as stopped; its own close bounds the shell's grace.
        let _ = sidecars::signal(&fd, libc::SIGTERM);
        let result = match tokio::time::timeout_at(deadline, sidecars::wait_pidfd(&fd)).await {
            // The pidfd also becomes readable if the host died before it could handle SIGTERM.
            Ok(Ok(())) => self.end_session(&id).await,
            _ => self.kill_host(entry).await,
        };
        self.reap(entry).await;
        let _ = crate::audit::record(
            &self.paths.state,
            "terminal-stop",
            &format!("id={id} by={}", StopBy::DaemonStop.as_str()),
        );
        result
    }

    /// Ends the session of the shell the host recorded, without asking the host, while that shell still runs as
    /// recorded (pid and start time, pinned by a pidfd): the same steps as the host's close, with no grace. A shell
    /// already gone is left with its session: its session id may have been reused, so nothing is hunted.
    async fn end_session(&self, id: &str) -> io::Result<()> {
        #[cfg(test)]
        if self.config.faults.fail_session_end {
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
        let Some(record) = adopt::read_shell(&self.paths, id)? else {
            return Ok(());
        };
        let state = self.paths.state.clone();
        let stat = sidecars::identity_start_time;
        #[cfg(test)]
        let stat = self.config.faults.shell_start_time.unwrap_or(stat);
        let gone = tokio::task::spawn_blocking(move || {
            Ok::<_, io::Error>(match session::pin_shell(record, stat)? {
                Some(shell) => session::end(&shell, Duration::ZERO, &state, "terminal-stop-error"),
                None => true,
            })
        })
        .await
        .map_err(io::Error::other)??;
        if gone {
            Ok(())
        } else {
            Err(io::Error::other(UNCONFIRMED_SESSION))
        }
    }

    /// Tries to end the shell's session, then kills the host so its PTY closes even if session cleanup failed.
    async fn kill_host(&self, entry: &SharedEntry) -> io::Result<()> {
        self.kill_host_with_session_result(entry).await?
    }

    /// Returns the session cleanup result once the host is confirmed gone.
    async fn kill_host_with_session_result(&self, entry: &SharedEntry) -> io::Result<io::Result<()>> {
        if self.config.faults.fail_kills {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "terminal host did not exit after SIGKILL",
            ));
        }
        let (fd, id) = {
            let mut e = entry.lock().expect("terminal entry");
            self.retry_pin(&mut e)?;
            (e.pidfd.clone(), e.record.terminal_id.clone())
        };
        let session_result = self.end_session(&id).await;
        if let Some(fd) = fd {
            if !sidecars::exited(&fd)? {
                sidecars::signal(&fd, libc::SIGKILL)?;
            }
            tokio::time::timeout(KILL_BUDGET, sidecars::wait_pidfd(&fd))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "terminal host did not exit after SIGKILL"))??;
        }
        self.reap(entry).await;
        Ok(session_result)
    }

    /// Waits for an owned host that has exited, so no zombie is left.
    async fn reap(&self, entry: &SharedEntry) {
        let child = {
            let mut e = entry.lock().expect("terminal entry");
            if e.alive() {
                return;
            }
            e.pidfd = None;
            e.child.take()
        };
        if let Some(mut child) = child {
            let _ = tokio::time::timeout(KILL_BUDGET, child.wait()).await;
        }
    }

    /// Reaps exited hosts, reads their exit records, audits hosts gone without one, and expires records kept past
    /// [`RETENTION_MS`].
    pub fn observe(&self) {
        let _ = self.rescan();
        let now = crate::run_dir::unix_ms();
        let mut expired = Vec::new();
        let mut exited_up = Vec::new();
        let mut ended = Vec::new();
        for entry in self.snapshot() {
            let mut e = entry.lock().expect("terminal entry");
            let _ = self.retry_pin(&mut e);
            e.reap();
            if e.exit.is_none() {
                e.exit = adopt::read_exit(&self.paths, &e.record.terminal_id);
            }
            if e.alive() {
                if let (Some(exit), false) = (e.exit, e.refused) {
                    exited_up.push((exit.exited_unix_ms, entry.clone()));
                }
                continue;
            }
            if e.record.ended_unix_ms.is_none() {
                e.record.ended_unix_ms = Some(now);
                if !e.refused {
                    let _ = adopt::persist(&self.paths, &e.record);
                }
                if e.exit.is_none() && e.record.failure.is_none() {
                    let _ = crate::audit::record(
                        &self.paths.state,
                        "terminal-gone",
                        &format!("id={} pid={}", e.record.terminal_id, e.record.pid),
                    );
                }
            }
            let since = e
                .exit
                .map(|exit| exit.exited_unix_ms)
                .or(e.record.ended_unix_ms)
                .unwrap_or(now);
            // A record whose process is unknown cannot be shown gone, so it stays.
            if e.child.is_none() && e.record.pid != 0 {
                if now.saturating_sub(since) >= RETENTION_MS {
                    expired.push(e.record.terminal_id.clone());
                } else {
                    ended.push((since, e.record.terminal_id.clone()));
                }
            }
        }
        // Bounded by count as well as by age: the newest are kept.
        ended.sort_by(|a, b| b.0.cmp(&a.0));
        expired.extend(ended.into_iter().skip(ENDED_KEPT_MAX).map(|(_, id)| id));
        exited_up.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, entry) in exited_up.into_iter().skip(EXITED_UP_MAX) {
            let mut e = entry.lock().expect("terminal entry");
            if e.told_to_go {
                continue;
            }
            let Some(fd) = &e.pidfd else {
                continue;
            };
            // The shell has exited; SIGTERM makes the host end its terminal and go, keeping its exit record.
            if sidecars::signal(fd, libc::SIGTERM).is_err() {
                continue;
            }
            e.told_to_go = true;
            let _ = crate::audit::record(
                &self.paths.state,
                "terminal-exited-host-ended",
                &format!("id={}", e.record.terminal_id),
            );
        }
        for id in expired {
            match adopt::remove_files(&self.paths, &id) {
                Ok(()) => {
                    self.entries.lock().expect("terminal registry").remove(&id);
                    let _ = crate::audit::record(&self.paths.state, "terminal-expire", &format!("id={id}"));
                }
                Err(error) => {
                    let _ =
                        crate::audit::record(&self.paths.state, "terminal-expire-error", &format!("id={id} {error}"));
                }
            }
        }
    }

    /// The daemon's maintenance tick: observation, log trimming, and handshakes for adopted hosts (retried; a host
    /// whose start was interrupted and never answered within the bind budget or whose socket is gone is stopped).
    pub async fn maintain(&self) {
        self.observe();
        self.retry_failed_stops().await;
        let ids: Vec<String> = self
            .entries
            .lock()
            .expect("terminal registry")
            .keys()
            .cloned()
            .collect();
        let metadata = self.paths.metadata_run();
        let state = self.paths.state.clone();
        let _ = tokio::task::spawn_blocking(move || {
            for id in ids {
                for extension in ["out", "err"] {
                    if let Err(error) = sidecars::trim_log(&metadata.join(format!("th-{id}.{extension}"))) {
                        let _ = crate::audit::record(
                            &state,
                            "terminal-log-trim-error",
                            &format!("id={id} file={extension} {error}"),
                        );
                    }
                }
            }
        })
        .await;
        self.refresh_handshakes().await;
    }

    /// Retries the stop of a failed start whose host could not be stopped, so a retried open can start again.
    pub async fn retry_failed_stops(&self) {
        for entry in self.snapshot() {
            let (id, retry) = {
                let e = entry.lock().expect("terminal entry");
                (
                    e.record.terminal_id.clone(),
                    !e.refused && e.record.failure.is_some() && e.alive(),
                )
            };
            if retry {
                if let Err(error) = self.kill_host(&entry).await {
                    let _ = crate::audit::record(&self.paths.state, "terminal-stop-error", &format!("id={id} {error}"));
                }
            }
        }
    }

    pub async fn refresh_handshakes(&self) {
        self.refresh_handshakes_with(&handshake, &same_mount_namespace).await;
    }

    async fn refresh_handshakes_with(
        &self,
        handshake: &impl AsyncFn(&Path, u32, &str) -> io::Result<()>,
        same_mount: &impl Fn(u32, &OwnedFd) -> io::Result<bool>,
    ) {
        futures_util::future::join_all(self.snapshot().into_iter().map(|entry| async move {
            let command_id = {
                let e = entry.lock().expect("terminal entry");
                if e.handshake || !e.alive() || e.pin_pending.is_some() || e.record.failure.is_some() || e.refused {
                    return;
                }
                e.record.command_id.clone()
            };
            // Maintenance and a retried open must not finish or stop the same unfinished start at once.
            let serial = self.start_lock(&command_id);
            let _serial = serial.lock().await;
            let (socket, pid, id, ready, started) = {
                let e = entry.lock().expect("terminal entry");
                if e.handshake || !e.alive() || e.record.failure.is_some() {
                    return;
                }
                (
                    e.record.socket.clone(),
                    e.record.pid,
                    e.record.terminal_id.clone(),
                    e.record.ready,
                    e.record.started_unix_ms,
                )
            };
            for _ in 0..3 {
                if handshake(&socket, pid, &id).await.is_ok() {
                    if ready {
                        entry.lock().expect("terminal entry").handshake = true;
                    } else {
                        let _ = self.finish_start(&entry).await;
                    }
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if !ready && bind_remaining(started).is_zero() {
                if handshake(&socket, pid, &id).await.is_ok() {
                    let _ = self.finish_start(&entry).await;
                    return;
                }
                let _ = self
                    .fail_start(&entry, "startup interrupted before readiness".into())
                    .await;
            } else if ready
                && matches!(socket.symlink_metadata(), Err(error) if error.kind() == io::ErrorKind::NotFound)
            {
                let Some(fd) = entry.lock().expect("terminal entry").pidfd.clone() else {
                    return;
                };
                let reason = match same_mount(pid, &fd) {
                    Ok(true) => None,
                    Ok(false) => Some("different mount namespace".to_owned()),
                    Err(error) => Some(format!("unknown mount namespace: {error}")),
                };
                if let Some(reason) = reason {
                    if !entry.lock().expect("terminal entry").hidden_socket_audited
                        && crate::audit::record(
                            &self.paths.state,
                            "terminal-socket-hidden",
                            &format!("id={id} pid={pid} {reason}"),
                        )
                        .is_ok()
                    {
                        entry.lock().expect("terminal entry").hidden_socket_audited = true;
                    }
                    return;
                }
                // A ready host binds once. In its own namespace a missing listener cannot take new connections.
                if let Err(error) = self.kill_host(&entry).await {
                    let _ = crate::audit::record(&self.paths.state, "terminal-stop-error", &format!("id={id} {error}"));
                }
            }
        }))
        .await;
    }
}

fn same_mount_namespace(pid: u32, pidfd: &OwnedFd) -> io::Result<bool> {
    let host = std::fs::read_link(format!("/proc/{pid}/ns/mnt"))?;
    if sidecars::exited(pidfd)? {
        return Err(io::Error::other("terminal host exited during namespace lookup"));
    }
    Ok(host == std::fs::read_link("/proc/self/ns/mnt")?)
}

fn bind_remaining(started: u64) -> Duration {
    BIND_BUDGET.saturating_sub(Duration::from_millis(crate::run_dir::unix_ms().saturating_sub(started)))
}

/// The start directory's rules: absolute, normal form, at most 4096 bytes, an existing directory, opened component by
/// component. Unlike a sidecar's root it need not be the daemon uid's own.
pub fn open_start_dir(path: &str) -> Result<File, Status> {
    sidecars::open_directory(path, "start directory")
}

/// The shell's whole environment: the passwd identity, a fixed `PATH`, the login's `LANG`, and the variables a local
/// window's shell gets from its terminal (`TERM`, `COLORTERM`, `TERM_PROGRAM`). Nothing of the daemon's or the
/// channel's environment.
pub fn shell_environment(identity: &Environment, lang: OsString) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = login_env::IDENTITY_KEYS
        .iter()
        .filter_map(|key| {
            identity
                .get(OsStr::new(key))
                .map(|value| (OsString::from(key), value.clone()))
        })
        .collect();
    env.push(("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()));
    env.push(("LANG".into(), lang));
    env.extend(
        eitri_terminal::pty::SET_ENV
            .iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value))),
    );
    env
}

fn default_colors() -> TermColors {
    host::colors_to_wire(&eitri_terminal::TerminalColors::default())
}

/// The size a request asks for, clamped to the grid bounds.
pub fn clamp_wire_size(size: &TermSize) -> TermSize {
    TermSize {
        columns: size.columns.clamp(1, TERM_COLUMNS_MAX),
        rows: size.rows.clamp(1, TERM_ROWS_MAX),
        cell_width: size.cell_width.min(u32::from(u16::MAX)),
        cell_height: size.cell_height.min(u32::from(u16::MAX)),
    }
}

/// A daemon instance's id: 128 random bits as 32 lowercase hex digits, made at the daemon's start.
pub fn new_instance_id() -> io::Result<String> {
    new_id()
}

/// 128 random bits as 32 lowercase hex digits.
fn new_id() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// A channel to a host's socket. The connector checks the peer's uid and that it is the recorded host process before
/// HTTP/2 reads or writes its first byte.
async fn channel(socket: &Path, pid: u32) -> io::Result<Channel> {
    let socket = socket.to_owned();
    tonic::transport::Endpoint::from_static("http://localhost")
        .connect_with_connector(tower::service_fn(move |_| {
            let socket = socket.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(socket).await?;
                crate::peer::check(&stream)?;
                if stream.peer_cred()?.pid() != Some(pid as i32) {
                    return Err(io::Error::other("terminal host socket peer pid differs"));
                }
                Ok::<_, io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
        .map_err(io::Error::other)
}

/// The host answers for its one terminal, by id.
async fn handshake(socket: &Path, pid: u32, id: &str) -> io::Result<()> {
    let exchange = async {
        let mut client = TerminalClient::new(channel(socket, pid).await?);
        let listed = client
            .list_terminals(ListTerminalsRequest {})
            .await
            .map_err(io::Error::other)?
            .into_inner();
        if listed.terminals.len() != 1 || listed.terminals[0].terminal_id != id {
            return Err(io::Error::other("terminal host answers for another terminal"));
        }
        Ok(())
    };
    tokio::time::timeout(Duration::from_secs(2), exchange)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "terminal host handshake timed out"))?
}

/// This boot's id (`/proc/sys/kernel/random/boot_id`), which persisted process identities carry.
pub(crate) fn current_boot_id() -> io::Result<String> {
    Ok(std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::AtomicBool;

    static CALLS: AtomicUsize = AtomicUsize::new(0);
    static RELEASE: AtomicBool = AtomicBool::new(false);

    pub(super) struct Scratch(pub(super) PathBuf);

    impl Scratch {
        pub(super) fn new() -> Self {
            let id = new_id().unwrap();
            let root = PathBuf::from(format!("/tmp/th-unit-{}", &id[..12]));
            private_fs::create_private_dir_all(&root, &root).unwrap();
            Self(root)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(super) fn fake_host(paths: &Paths) -> (Child, HostRecord) {
        let child = tokio::process::Command::new("/bin/sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let record = host_record(paths, child.id().unwrap());
        adopt::persist(paths, &record).unwrap();
        (child, record)
    }

    fn host_record(paths: &Paths, pid: u32) -> HostRecord {
        let terminal_id = "0123456789abcdef0123456789abcdef".to_owned();
        HostRecord {
            pid,
            start_time: sidecars::identity_start_time(pid).unwrap(),
            boot_id: current_boot_id().unwrap(),
            socket: agent::socket_path::in_dir(&paths.run, &format!("th-{terminal_id}.sock")).unwrap(),
            terminal_id,
            start_dir: "/srv/project".into(),
            command_id: "open-fake".into(),
            started_unix_ms: crate::run_dir::unix_ms(),
            ready: true,
            failure: None,
            ended_unix_ms: None,
        }
    }

    fn adopt_with_fake_pidfd(paths: &Paths) -> (TerminalHosts, std::io::PipeWriter) {
        let (reader, writer) = std::io::pipe().unwrap();
        let sidecars = Arc::new(Sidecars::new(paths.clone()).unwrap());
        let hosts = TerminalHosts::new_with_pin(paths.clone(), Config::default(), sidecars, |_| {
            Ok(Some(reader.try_clone()?.into()))
        });
        (hosts, writer)
    }

    pub(super) fn test_paths(scratch: &Scratch) -> Paths {
        let paths = Paths::new("1", &scratch.0.join("d"), &scratch.0, &scratch.0).unwrap();
        paths.prepare().unwrap();
        paths
    }

    #[tokio::test]
    async fn list_keeps_live_hosts_ahead_of_newer_exit_records() {
        let scratch = Scratch::new();
        let paths = test_paths(&scratch);
        let mut record = host_record(&paths, std::process::id());
        let exit = serde_json::to_vec(&ExitRecord {
            code: Some(0),
            signal: None,
            left: false,
            exited_unix_ms: crate::run_dir::unix_ms(),
        })
        .unwrap();
        for n in 0..TERM_LIST_MAX + 2 {
            let id = format!("{n:032x}");
            record.terminal_id = id.clone();
            record.socket = agent::socket_path::in_dir(&paths.run, &format!("th-{id}.sock")).unwrap();
            record.started_unix_ms = n as u64;
            if n >= 2 {
                record.pid = 0;
            }
            adopt::persist(&paths, &record).unwrap();
            if n > 0 {
                private_fs::write_private(&paths.metadata_run().join(format!("th-{id}.exit")), &exit).unwrap();
            }
        }
        let sidecars = Arc::new(Sidecars::new(paths.clone()).unwrap());
        let hosts = TerminalHosts::new(paths, Config::default(), sidecars);
        let listed = hosts.list_terminals().await;
        assert!(listed.truncated);
        assert_eq!(listed.terminals.len(), TERM_LIST_MAX);
        let starts: Vec<_> = listed.terminals.iter().map(|t| t.started_unix_ms).collect();
        assert_eq!(&starts[..2], &[1, 0]);
        assert_eq!(starts[2], TERM_LIST_MAX as u64 + 1);
        assert!(starts[2..].windows(2).all(|pair| pair[0] > pair[1]));
    }

    fn recorded_session_leader(paths: &Paths, id: &str) -> Child {
        let mut command = tokio::process::Command::new("/bin/sleep");
        command.arg("60").kill_on_drop(true);
        // SAFETY: setsid and signal are async-signal-safe and change only the child before exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::signal(libc::SIGHUP, libc::SIG_IGN) == libc::SIG_ERR {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let shell = command.spawn().unwrap();
        let pid = shell.id().unwrap();
        let shell_record = session::ShellRecord {
            pid,
            start_time: sidecars::identity_start_time(pid).unwrap(),
            boot_id: current_boot_id().unwrap(),
        };
        private_fs::write_private(
            &paths.metadata_run().join(format!("th-{id}.shell")),
            &serde_json::to_vec(&shell_record).unwrap(),
        )
        .unwrap();
        shell
    }

    #[tokio::test]
    async fn stop_ends_the_shell_when_the_pinned_host_dies_before_sigterm() {
        let scratch = Scratch::new();
        let paths = test_paths(&scratch);
        let (mut host, record) = fake_host(&paths);
        let mut shell = recorded_session_leader(&paths, &record.terminal_id);
        let mut config = Config::default();
        config.faults.exit_after_stop_pin = true;
        let sidecars = Arc::new(Sidecars::new(paths.clone()).unwrap());
        let hosts = TerminalHosts::new(paths, config, sidecars);
        hosts.stop_all().await.unwrap();
        assert!(host.try_wait().unwrap().is_some());
        assert!(
            shell.try_wait().unwrap().is_some(),
            "the recorded shell survived daemon stop"
        );
    }

    #[tokio::test]
    async fn stop_all_ends_the_recorded_shell_of_a_dead_host() {
        let scratch = Scratch::new();
        let paths = test_paths(&scratch);
        let (mut host, record) = fake_host(&paths);
        let mut shell = recorded_session_leader(&paths, &record.terminal_id);
        host.kill().await.unwrap();
        let sidecars = Arc::new(Sidecars::new(paths.clone()).unwrap());
        let mut config = Config::default();
        config.faults.shell_start_time = Some(|_| Err(io::Error::from_raw_os_error(libc::EMFILE)));
        let hosts = TerminalHosts::new(paths.clone(), config, sidecars.clone());
        assert_eq!(hosts.stop_all().await.unwrap_err().raw_os_error(), Some(libc::EMFILE));
        assert!(shell.try_wait().unwrap().is_none());

        let hosts = TerminalHosts::new(paths, Config::default(), sidecars);
        hosts.stop_all().await.unwrap();
        assert!(shell.try_wait().unwrap().is_some());
    }

    fn config_with_fallible_record_dir() -> Config {
        let mut config = Config::default();
        config.faults.read_record_dir = Some(|path| {
            if path.join("emfile").exists() {
                return Err(io::Error::from_raw_os_error(libc::EMFILE));
            }
            std::fs::read_dir(path)
        });
        config
    }

    #[tokio::test]
    async fn list_marks_an_incomplete_adoption_scan_as_truncated() {
        let scratch = Scratch::new();
        let paths = test_paths(&scratch);
        let (_host, record) = fake_host(&paths);
        let marker = paths.metadata_run().join("emfile");
        private_fs::write_private(&marker, b"").unwrap();
        let sidecars = Arc::new(Sidecars::new(paths.clone()).unwrap());
        let hosts = TerminalHosts::new(paths, config_with_fallible_record_dir(), sidecars);
        let listed = hosts.list_terminals().await;
        assert!(listed.truncated);
        std::fs::remove_file(marker).unwrap();
        let listed = hosts.list_terminals().await;
        assert!(!listed.truncated);
        assert_eq!(listed.terminals[0].terminal_id, record.terminal_id);
    }

    #[tokio::test]
    async fn observation_retries_an_unreadable_terminal_record_directory() {
        let scratch = Scratch::new();
        let paths = test_paths(&scratch);
        let (_child, record) = fake_host(&paths);
        let marker = paths.metadata_run().join("emfile");
        private_fs::write_private(&marker, b"").unwrap();
        let config = config_with_fallible_record_dir();
        let sidecars = Arc::new(Sidecars::new(paths.clone()).unwrap());
        let hosts = TerminalHosts::new(paths, config, sidecars);
        assert!(hosts.snapshot().is_empty());
        let retry = OpenTerminalRequest {
            command_id: record.command_id.clone(),
            ..Default::default()
        };
        assert_eq!(
            hosts.start(retry.clone()).await.unwrap_err().code(),
            tonic::Code::Unavailable
        );
        let attach = hosts.attach_target(&record.terminal_id).await.err().unwrap();
        assert_eq!(attach.code(), tonic::Code::Unavailable);
        std::fs::remove_file(marker).unwrap();
        hosts.observe();
        assert!(hosts.any_live());
        assert_eq!(hosts.start(retry).await.unwrap(), record.terminal_id);
    }

    #[tokio::test]
    async fn close_replay_evicts_the_first_inserted_command_id() {
        let scratch = Scratch::new();
        let paths = test_paths(&scratch);
        let (mut child, record) = fake_host(&paths);
        child.kill().await.unwrap();
        let sidecars = Arc::new(Sidecars::new(paths.clone()).unwrap());
        let hosts = TerminalHosts::new(paths, Config::default(), sidecars);
        for n in (0..=CLOSES_KEPT).rev() {
            hosts
                .close_by_command(&record.terminal_id, &format!("{n:064x}"))
                .await
                .unwrap();
        }
        hosts.entries.lock().unwrap().remove(&record.terminal_id);
        let first = hosts
            .close_by_command(&record.terminal_id, &format!("{CLOSES_KEPT:064x}"))
            .await;
        assert_eq!(first.unwrap_err().code(), tonic::Code::NotFound);
        for n in 0..CLOSES_KEPT {
            hosts
                .close_by_command(&record.terminal_id, &format!("{n:064x}"))
                .await
                .unwrap();
        }
    }

    fn record_left_exit(paths: &Paths, id: &str) -> ExitRecord {
        let exit = ExitRecord {
            code: None,
            signal: None,
            left: true,
            exited_unix_ms: crate::run_dir::unix_ms(),
        };
        private_fs::write_private(
            &paths.metadata_run().join(format!("th-{id}.exit")),
            &serde_json::to_vec(&exit).unwrap(),
        )
        .unwrap();
        exit
    }

    #[tokio::test]
    async fn attach_keeps_a_saved_left_exit_retryable_while_the_host_may_live() {
        let scratch = Scratch::new();
        let paths = test_paths(&scratch);
        let (mut host, record) = fake_host(&paths);
        let id = &record.terminal_id;
        let exit = record_left_exit(&paths, id);
        let sidecars = Arc::new(Sidecars::new(paths.clone()).unwrap());
        let hosts = TerminalHosts::new(paths, Config::default(), sidecars);
        let target = hosts
            .attach_target_with(id, async |_, _| Err(io::ErrorKind::ConnectionRefused.into()))
            .await;
        assert!(matches!(target, Err(error) if error.code() == tonic::Code::Unavailable));
        host.kill().await.unwrap();
        assert!(matches!(hosts.attach_target(id).await, Ok(AttachTarget::Exited(saved)) if saved == exit));
    }

    #[tokio::test]
    async fn attach_waits_for_host_identity_even_with_a_saved_exit() {
        let scratch = Scratch::new();
        let paths = test_paths(&scratch);
        let (_host, record) = fake_host(&paths);
        let id = &record.terminal_id;
        record_left_exit(&paths, id);
        private_fs::write_private(&paths.metadata_run().join(format!("th-{id}.emfile")), b"").unwrap();
        let mut config = Config::default();
        config.faults.host_record_read = Some(read_record_with_descriptor_limit);
        let sidecars = Arc::new(Sidecars::new(paths.clone()).unwrap());
        let hosts = TerminalHosts::new(paths, config, sidecars);
        assert!(matches!(hosts.attach_target(id).await, Err(error) if error.code() == tonic::Code::Unavailable));
    }

    #[tokio::test]
    async fn attach_uses_the_exit_written_during_connect() {
        let scratch = Scratch::new();
        let paths = test_paths(&scratch);
        let (mut child, record) = fake_host(&paths);
        let sidecars = Arc::new(Sidecars::new(paths.clone()).unwrap());
        let hosts = TerminalHosts::new(paths.clone(), Config::default(), sidecars);
        let exit = ExitRecord {
            code: Some(7),
            signal: None,
            left: false,
            exited_unix_ms: crate::run_dir::unix_ms(),
        };

        let target = hosts
            .attach_target_with(&record.terminal_id, async |_, _| {
                private_fs::write_private(
                    &paths.metadata_run().join(format!("th-{}.exit", record.terminal_id)),
                    &serde_json::to_vec(&exit).unwrap(),
                )
                .unwrap();
                child.kill().await.unwrap();
                Err(io::ErrorKind::ConnectionRefused.into())
            })
            .await
            .unwrap();
        assert!(matches!(target, AttachTarget::Exited(saved) if saved == exit));
    }

    #[tokio::test]
    async fn maintenance_stops_a_ready_adopted_host_without_its_socket() {
        let scratch = Scratch::new();
        let paths = test_paths(&scratch);
        let (mut child, record) = fake_host(&paths);
        let sidecars = Arc::new(Sidecars::new(paths.clone()).unwrap());
        let hosts = TerminalHosts::new(paths, Config::default(), sidecars);
        assert!(hosts.any_live());

        hosts.refresh_handshakes().await;

        assert!(child.try_wait().unwrap().is_some());
        assert!(!hosts.any_live());
        assert!(matches!(hosts.list()[0].state, HostState::Gone));
        assert_eq!(
            hosts.recover_start(&record.command_id).await.unwrap(),
            Some(record.terminal_id)
        );
    }

    #[tokio::test]
    async fn maintenance_keeps_a_ready_host_whose_socket_is_hidden_by_another_mount_namespace() {
        let scratch = Scratch::new();
        let paths = test_paths(&scratch);
        let record = host_record(&paths, std::process::id());
        adopt::persist(&paths, &record).unwrap();
        let (hosts, _live) = adopt_with_fake_pidfd(&paths);

        for _ in 0..2 {
            hosts
                .refresh_handshakes_with(&async |_, _, _| Err(io::ErrorKind::NotFound.into()), &|pid, _| {
                    assert_eq!(pid, record.pid);
                    Ok(false)
                })
                .await;
        }

        assert!(hosts.any_live());
        assert!(matches!(hosts.list()[0].state, HostState::Live { handshake: false }));
        let audit = private_fs::read_private_nofollow_capped(&paths.state.join("audit.log"), 8192).unwrap();
        assert_eq!(audit.matches("terminal-socket-hidden").count(), 1);
        assert!(audit.contains(&format!(
            "id={} pid={} different mount namespace",
            record.terminal_id, record.pid
        )));
        assert!(!audit.contains("terminal-stop"));

        hosts
            .refresh_handshakes_with(&async |_, _, _| Ok(()), &|_, _| unreachable!())
            .await;
        assert!(matches!(hosts.list()[0].state, HostState::Live { handshake: true }));
    }

    #[tokio::test]
    async fn a_host_binding_after_the_last_attempt_is_finished_not_killed() {
        let scratch = Scratch::new();
        let paths = test_paths(&scratch);
        let mut record = host_record(&paths, std::process::id());
        record.ready = false;
        record.started_unix_ms = 0;
        adopt::persist(&paths, &record).unwrap();
        let (hosts, _live) = adopt_with_fake_pidfd(&paths);
        let attempts = std::cell::Cell::new(0);

        hosts
            .refresh_handshakes_with(
                &async |_, _, _| {
                    attempts.set(attempts.get() + 1);
                    if attempts.get() <= 3 {
                        Err(io::ErrorKind::NotFound.into())
                    } else {
                        Ok(())
                    }
                },
                &same_mount_namespace,
            )
            .await;

        assert_eq!(attempts.get(), 4);
        assert!(matches!(hosts.list()[0].state, HostState::Live { handshake: true }));
        assert!(
            adopt::read_record(&paths, &record.terminal_id, &Faults::default())
                .unwrap()
                .0
                .ready
        );
    }

    #[tokio::test]
    async fn open_and_attach_refuse_a_live_host_in_another_run_directory() {
        let scratch = Scratch::new();
        let first = test_paths(&scratch);
        let record = host_record(&first, std::process::id());
        adopt::persist(&first, &record).unwrap();
        let mut next = first.clone();
        next.run = scratch.0.join("other-run");
        next.prepare().unwrap();
        let (hosts, mut exit_signal) = adopt_with_fake_pidfd(&next);
        let retry = OpenTerminalRequest {
            command_id: record.command_id,
            ..Default::default()
        };

        let open = hosts.start(retry.clone()).await.unwrap_err();
        let attach = hosts.attach_target(&record.terminal_id).await.err().unwrap();
        assert_eq!(
            (open.code(), attach.code()),
            (tonic::Code::FailedPrecondition, tonic::Code::FailedPrecondition)
        );
        for error in [open, attach] {
            assert_eq!(
                error.message(),
                "this terminal runs under another run directory; close it or restart the server with the same TMPDIR"
            );
        }
        assert!(hosts.any_live());
        assert_eq!(
            adopt::record_ids(&next, &Faults::default()).unwrap(),
            [record.terminal_id]
        );
        // Make the fake pidfd readable even if a concurrently forked child still holds the pipe writer.
        exit_signal.write_all(&[1]).unwrap();
        assert_eq!(
            hosts.start(retry).await.unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
    }

    #[tokio::test]
    async fn close_keeps_the_reported_exit_without_a_saved_record() {
        let scratch = Scratch::new();
        let paths = test_paths(&scratch);
        let (mut child, record) = fake_host(&paths);
        let sidecars = Arc::new(Sidecars::new(paths.clone()).unwrap());
        let hosts = TerminalHosts::new(paths.clone(), Config::default(), sidecars);

        let closed = hosts
            .close_with(&record.terminal_id, "close-fake", async |_, _| {
                child.kill().await.unwrap();
                Some(Response::new(CloseTerminalResponse {
                    exit: Some(host::exit_to_wire(Some(7), None, false)),
                }))
            })
            .await
            .unwrap();
        let exit = closed.exit.unwrap();
        assert_eq!((exit.code, exit.signal, exit.left), (Some(7), None, false));
        assert!(adopt::read_exit(&paths, &record.terminal_id).is_none());
        assert_eq!(
            hosts.close(&record.terminal_id, "close-again").await.unwrap(),
            Some(exit)
        );
    }

    #[tokio::test]
    async fn fallback_close_warns_when_pinned_shell_stat_cannot_be_read() {
        let scratch = Scratch::new();
        let paths = test_paths(&scratch);
        let (mut child, record) = fake_host(&paths);
        let id = &record.terminal_id;
        let mut shell = tokio::process::Command::new("/bin/sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = shell.id().unwrap();
        let shell_record = session::ShellRecord {
            pid,
            start_time: sidecars::identity_start_time(pid).unwrap(),
            boot_id: current_boot_id().unwrap(),
        };
        private_fs::write_private(
            &paths.metadata_run().join(format!("th-{id}.shell")),
            &serde_json::to_vec(&shell_record).unwrap(),
        )
        .unwrap();
        let mut config = Config::default();
        config.faults.shell_start_time = Some(|_| Err(io::Error::from_raw_os_error(libc::EMFILE)));
        let sidecars = Arc::new(Sidecars::new(paths.clone()).unwrap());
        let hosts = TerminalHosts::new(paths, config, sidecars);

        for _ in 0..2 {
            let response = hosts.close_by_command(id, "close-fake").await.unwrap();
            assert_eq!(response.metadata().get(CLOSE_WARNING).unwrap(), UNCONFIRMED_SESSION);
            assert!(child.try_wait().unwrap().is_some());
            assert!(shell.try_wait().unwrap().is_none());
        }
        shell.kill().await.unwrap();
    }

    #[tokio::test]
    async fn adoption_retries_a_pidfd_resource_error_before_shutdown() {
        let scratch = Scratch::new();
        let paths = test_paths(&scratch);
        let (mut child, record) = fake_host(&paths);
        let sidecars = Arc::new(Sidecars::new(paths.clone()).unwrap());
        let hosts = TerminalHosts::new_with_pin(paths.clone(), Config::default(), sidecars, |_| {
            Err(io::Error::from_raw_os_error(libc::EMFILE))
        });
        assert!(hosts.any_live());
        assert!(adopt::read_record(&paths, &record.terminal_id, &Faults::default())
            .unwrap()
            .0
            .ended_unix_ms
            .is_none());
        assert!(matches!(
            hosts.connect(&record.terminal_id).await,
            Err(error) if error.code() == tonic::Code::Unavailable
        ));

        hosts.stop_all().await.unwrap();

        assert!(child.try_wait().unwrap().is_some());
        assert!(!hosts.any_live());
    }

    fn read_record_with_descriptor_limit(path: &Path, cap: u64) -> io::Result<Vec<u8>> {
        if path.with_extension("emfile").exists() {
            return Err(io::Error::from_raw_os_error(libc::EMFILE));
        }
        private_fs::read_private_nofollow_bytes_capped(path, cap)
    }

    #[tokio::test]
    async fn open_retry_rereads_a_foreign_host_before_matching_its_command() {
        let scratch = Scratch::new();
        let first = test_paths(&scratch);
        let (_child, record) = fake_host(&first);
        let mut next = first.clone();
        next.run = scratch.0.join("other-run");
        next.prepare().unwrap();
        let marker = next.metadata_run().join(format!("th-{}.emfile", record.terminal_id));
        private_fs::write_private(&marker, b"").unwrap();
        let mut config = Config::default();
        config.faults.host_record_read = Some(read_record_with_descriptor_limit);
        let sidecars = Arc::new(Sidecars::new(next.clone()).unwrap());
        let hosts = TerminalHosts::new(next, config, sidecars);
        let retry = OpenTerminalRequest {
            command_id: record.command_id,
            ..Default::default()
        };

        assert_eq!(
            hosts.start(retry.clone()).await.unwrap_err().code(),
            tonic::Code::Unavailable
        );
        std::fs::remove_file(marker).unwrap();
        let error = hosts.start(retry).await.unwrap_err();
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        assert!(hosts.recover_start("other-open").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn adoption_rereads_a_host_record_after_descriptor_exhaustion() {
        let scratch = Scratch::new();
        let paths = test_paths(&scratch);
        let (mut child, record) = fake_host(&paths);
        let id = &record.terminal_id;
        let marker = paths.metadata_run().join(format!("th-{id}.emfile"));
        private_fs::write_private(&marker, b"").unwrap();
        let mut config = Config::default();
        config.faults.host_record_read = Some(read_record_with_descriptor_limit);
        let sidecars = Arc::new(Sidecars::new(paths.clone()).unwrap());
        let hosts = TerminalHosts::new(paths.clone(), config, sidecars);
        assert!(hosts.any_live());
        assert!(matches!(hosts.list()[0].state, HostState::Failed { alive: true, .. }));
        assert!(adopt::read_record(&paths, id, &Faults::default())
            .unwrap()
            .0
            .ended_unix_ms
            .is_none());
        assert_eq!(hosts.stop_all().await.unwrap_err().raw_os_error(), Some(libc::EMFILE));

        std::fs::remove_file(marker).unwrap();
        hosts.stop_all().await.unwrap();

        assert!(child.try_wait().unwrap().is_some());
        assert!(!hosts.any_live());
    }

    #[tokio::test]
    async fn exited_host_past_retention_is_signalled_when_pinning_recovers() {
        let scratch = Scratch::new();
        let paths = test_paths(&scratch);
        let (mut child, record) = fake_host(&paths);
        let id = &record.terminal_id;
        let marker = paths.metadata_run().join(format!("th-{id}.emfile"));
        private_fs::write_private(&marker, b"").unwrap();
        let now = crate::run_dir::unix_ms();
        for index in 0..=EXITED_UP_MAX {
            let mut record = record.clone();
            if index > 0 {
                record.terminal_id = format!("{index:032x}");
                record.socket =
                    agent::socket_path::in_dir(&paths.run, &format!("th-{}.sock", record.terminal_id)).unwrap();
                adopt::persist(&paths, &record).unwrap();
            }
            let exit = ExitRecord {
                code: Some(0),
                signal: None,
                left: false,
                exited_unix_ms: now + index as u64,
            };
            private_fs::write_private(
                &paths.metadata_run().join(format!("th-{}.exit", record.terminal_id)),
                &serde_json::to_vec(&exit).unwrap(),
            )
            .unwrap();
        }
        let mut config = Config::default();
        config.faults.host_record_read = Some(read_record_with_descriptor_limit);
        let sidecars = Arc::new(Sidecars::new(paths.clone()).unwrap());
        let hosts = TerminalHosts::new(paths, config, sidecars);
        let oldest = hosts.entry(id).unwrap();
        hosts.observe();
        assert!(oldest.lock().unwrap().pidfd.is_none());
        assert!(!oldest.lock().unwrap().told_to_go);

        std::fs::remove_file(marker).unwrap();
        hosts.observe();

        assert!(oldest.lock().unwrap().told_to_go);
        let exit = tokio::time::timeout(Duration::from_secs(60), child.wait())
            .await
            .unwrap()
            .unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(exit.signal(), Some(libc::SIGTERM));
    }

    #[tokio::test]
    async fn retried_open_waits_for_the_original_host_to_bind() {
        let scratch = Scratch::new();
        let paths = test_paths(&scratch);
        let (_child, mut record) = fake_host(&paths);
        record.ready = false;
        adopt::persist(&paths, &record).unwrap();
        let sidecars = Arc::new(Sidecars::new(paths.clone()).unwrap());
        let hosts = TerminalHosts::new(paths.clone(), Config::default(), sidecars);
        let attempts = std::cell::Cell::new(0);

        let recovered = hosts
            .recover_start_with(&record.command_id, async |_, pid, id| {
                assert_eq!(pid, record.pid);
                assert_eq!(id, record.terminal_id);
                attempts.set(attempts.get() + 1);
                if attempts.get() == 1 {
                    Err(io::ErrorKind::NotFound.into())
                } else {
                    Ok(())
                }
            })
            .await
            .unwrap();

        assert_eq!(recovered.as_deref(), Some(record.terminal_id.as_str()));
        assert!(
            adopt::read_record(&paths, &record.terminal_id, &Faults::default())
                .unwrap()
                .0
                .ready
        );
        assert!(matches!(hosts.list()[0].state, HostState::Live { handshake: true }));
    }

    #[tokio::test]
    async fn observing_an_exited_host_releases_its_pidfd_but_keeps_its_record() {
        let scratch = Scratch::new();
        let paths = test_paths(&scratch);
        let (mut child, record) = fake_host(&paths);
        let sidecars = Arc::new(Sidecars::new(paths.clone()).unwrap());
        let hosts = TerminalHosts::new(paths, Config::default(), sidecars);
        let entry = hosts.entry(&record.terminal_id).unwrap();
        let pidfd = Arc::downgrade(entry.lock().unwrap().pidfd.as_ref().unwrap());

        child.kill().await.unwrap();
        hosts.observe();

        assert!(pidfd.upgrade().is_none());
        assert!(hosts.entry(&record.terminal_id).is_ok());
        assert!(matches!(hosts.list()[0].state, HostState::Gone));
    }

    #[tokio::test]
    async fn changing_tmpdir_does_not_stop_a_refused_host() {
        let scratch = Scratch::new();
        let data = scratch.0.join("d");
        let state = scratch.0.join("s".repeat(24));
        let paths_in = |name| {
            let tmp = scratch.0.join(name);
            private_fs::create_private_dir_all(&tmp, &scratch.0).unwrap();
            Paths::new("1", &data, &state, &tmp).unwrap()
        };
        let first = paths_in("a");
        first.prepare().unwrap();
        let (mut child, record) = fake_host(&first);
        let next = paths_in("b");
        next.prepare().unwrap();
        assert_ne!(first.run, next.run);
        let sidecars = Arc::new(Sidecars::new(next.clone()).unwrap());
        let hosts = TerminalHosts::new(next, Config::default(), sidecars);
        let listed = hosts.list();
        assert!(matches!(&listed[0].state, HostState::Failed { alive: true, why } if why.contains("another socket")));
        hosts.maintain().await;
        assert!(child.try_wait().unwrap().is_none());
        assert!(hosts.any_live());
        hosts.close(&record.terminal_id, "close-fake").await.unwrap();
        assert!(child.try_wait().unwrap().is_some());
    }

    #[tokio::test]
    async fn close_kills_the_host_even_when_session_cleanup_fails() {
        let scratch = Scratch::new();
        let paths = test_paths(&scratch);
        let (mut child, record) = fake_host(&paths);
        let mut config = Config::default();
        config.faults.fail_session_end = true;
        let sidecars = Arc::new(Sidecars::new(paths.clone()).unwrap());
        let hosts = TerminalHosts::new(paths.clone(), config, sidecars);
        let id = &record.terminal_id;
        let exit = ExitRecord {
            code: Some(7),
            signal: None,
            left: false,
            exited_unix_ms: crate::run_dir::unix_ms(),
        };
        private_fs::write_private(
            &paths.metadata_run().join(format!("th-{id}.exit")),
            &serde_json::to_vec(&exit).unwrap(),
        )
        .unwrap();

        for command_id in ["close-fake", "close-fake", "close-again"] {
            let response = hosts.close_by_command(id, command_id).await.unwrap();
            assert_eq!(
                response.get_ref().exit,
                Some(host::exit_to_wire(exit.code, exit.signal, exit.left))
            );
            assert_eq!(
                response.metadata().get("eitri-terminal-close-warning").unwrap(),
                "the terminal's shell or its session could not be confirmed gone"
            );
            assert!(child.try_wait().unwrap().is_some());
            assert!(!hosts.any_live());
            assert!(adopt::read_record(&paths, id, &Faults::default())
                .unwrap()
                .0
                .ended_unix_ms
                .is_some());
        }
        let audit = private_fs::read_private_nofollow_capped(&paths.state.join("audit.log"), 8192).unwrap();
        assert_eq!(audit.matches(&format!("terminal-stop id={id} by=client")).count(), 1);
        let error = io::Error::from_raw_os_error(libc::EPERM);
        assert_eq!(
            audit.matches(&format!("terminal-stop-error id={id} {error}")).count(),
            2
        );
    }

    #[tokio::test]
    async fn opening_with_room_for_only_sidecar_sockets_fails_precondition() {
        let scratch = Scratch::new();
        let state = scratch.0.join("s".repeat(25));
        let tmp = scratch.0.join("t".repeat(70));
        let paths = Paths::new("1", &scratch.0.join("d"), &state, &tmp).unwrap();
        assert_eq!(paths.run, paths.metadata_run());
        assert!(agent::socket_path::in_dir(&paths.run, "sc-0123456789abcdef.sock").is_ok());
        paths.prepare().unwrap();
        let config = Config {
            passwd: || panic!("opening must check socket length before looking up the shell identity"),
            ..Config::default()
        };
        let sidecars = Arc::new(Sidecars::new(paths.clone()).unwrap());
        let hosts = TerminalHosts::new(paths, config, sidecars);

        let error = hosts
            .start(OpenTerminalRequest {
                command_id: "open-too-long".into(),
                cwd: scratch.0.to_str().unwrap().into(),
                size: None,
                colors: None,
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        assert!(error.message().contains("a shorter TMPDIR avoids it"));
        assert!(!hosts.any_live());
    }

    fn stalled() -> io::Result<Environment> {
        CALLS.fetch_add(1, Ordering::SeqCst);
        while !RELEASE.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(Environment::from([("HOME".into(), "/srv/project".into())]))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stalled_passwd_lookup_is_kept_and_shared_not_repeated() {
        let lookup = PasswdLookup::new(stalled);
        for _ in 0..3 {
            let error = lookup.get(Duration::from_millis(50)).await.unwrap_err();
            assert_eq!(error.code(), tonic::Code::Unavailable);
        }
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
        RELEASE.store(true, Ordering::SeqCst);
        let identity = lookup.get(Duration::from_secs(60)).await.unwrap();
        assert_eq!(identity.get(OsStr::new("HOME")).unwrap(), "/srv/project");
        assert_eq!(lookup.get(Duration::from_secs(1)).await.unwrap(), identity);
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    }
}
