//! Remote Claude tabs through the server helper: the real `eitri-server` built from this workspace,
//! reached through `LocalExec` (its `connect` subcommand on a child's stdio, as over an ssh channel),
//! starting the stand-in sidecar (`tests/support/sidecar_standin.rs`) installed where the helper looks
//! for a sidecar. No ssh and no real `claude` run here.
//!
//! The helper binary is `<target>/debug/eitri-server`: build it first
//! (`cargo build -p eitri-server --bins`). Each test runs its own helper daemon under its own private
//! state, data and home directories, and stops it, with the sidecars it holds, when it ends.
#![cfg(target_os = "linux")]

use agent::remote_client::{HelperClient, ReconnectClock, SidecarStart};
use agent::setting_sources::ProjectTrust;
use agent::ssh::{ChannelLauncher, LinkCheck, LocalExec};
use agent::{
    AgentConversation, AgentDomainEvent, AgentProvider, AgentSessionProjection, ClaudeSidecarProvider,
    ClientToolAnswer, CreateSessionRequest, PermissionDecision, RemoteEnding, RemoteLinkState,
    ResolvePermissionRequest, SendTurnRequest, StreamingPreference, WaitingRequest,
};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long any one wait here may take before the test is called hung. Not a speed bound: every wait
/// ends as soon as what it waits for has happened.
const HANG: Duration = Duration::from_secs(90);

const HOST: &str = "example-host";
const HOST_KEY: &str = "SHA256:example-host-key";
const SIDECAR_REVISION: &str = "test-rev";

fn helper_binary() -> PathBuf {
    // Cargo may keep test executables in its build directory rather than beside the product binaries.
    let exe = std::env::current_exe().unwrap();
    let debug = exe.parent().and_then(Path::parent).unwrap();
    let helper = std::env::var_os("CARGO_TARGET_DIR")
        .map(|target| PathBuf::from(target).join("debug/eitri-server"))
        .unwrap_or_else(|| debug.join("eitri-server"));
    assert!(
        helper.is_file(),
        "the server helper is not built: run `cargo build -p eitri-server --bins` first ({})",
        helper.display()
    );
    helper
}

/// Runs `body` as `sh -c`, with `args` as its positional parameters, through a child process: no write
/// descriptor of this process stays open for a forking test on another thread to inherit (a script
/// still open for writing fails to run with `ETXTBSY`).
fn sh(body: &str, args: &[&Path]) {
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(body)
        .arg("sh")
        .args(args)
        .status()
        .expect("sh runs");
    assert!(status.success(), "{body}: {status}");
}

fn quoted(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// One helper on this machine as if on a server: its own home, XDG directories and daemon.
struct Server {
    root: PathBuf,
    /// The project directory the sidecars run in.
    project: PathBuf,
    /// The helper command a launcher runs: the built helper with this server's environment.
    command: PathBuf,
    version: String,
    sidecar: PathBuf,
    sidecar_sha256: String,
}

impl Server {
    /// A server whose stand-in reads `conf` (lines of `standin.conf`); `{root}` stands for the
    /// server's root directory.
    fn new(conf: &[&str]) -> Self {
        let root = loop {
            // Leave room for the helper's socket suffix under the shared Unix socket path cap.
            let candidate =
                PathBuf::from("/tmp").join(format!("rt-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
            if !candidate.exists() {
                break candidate;
            }
        };
        agent::private_fs::create_private_dir_all(&root, &root).unwrap();
        let root = root.canonicalize().unwrap();
        let project = root.join("project");
        std::fs::create_dir(&project).unwrap();
        let helper = helper_binary();
        let command = root.join("eitri-server");
        let script = format!(
            "#!/bin/sh\nexport HOME={root} XDG_DATA_HOME={root}/d XDG_STATE_HOME={root}/s TMPDIR={root} PATH=/usr/bin:/bin\nunset XDG_RUNTIME_DIR\nexec {helper} \"$@\"\n",
            root = quoted(&root),
            helper = quoted(&helper),
        );
        sh(
            r#"umask 077 && printf '%s' "$1" > "$2" && chmod 700 "$2""#,
            &[Path::new(&script), &command],
        );
        let output = std::process::Command::new(&helper).arg("--version").output().unwrap();
        let line = String::from_utf8(output.stdout).unwrap();
        let mut words = line.split_whitespace().skip(1);
        let (version, rev7) = (words.next().unwrap().to_string(), words.next().unwrap().to_string());
        let installed = root
            .join("d/eitri-server")
            .join(format!("{version}-{rev7}"))
            .join("sidecar")
            .join(SIDECAR_REVISION);
        agent::private_fs::create_private_dir_all(&installed, &root).unwrap();
        let sidecar = installed.join("verdandi-claude-sidecar");
        sh(
            r#"umask 077 && cp "$1" "$2" && chmod 700 "$2""#,
            &[Path::new(env!("CARGO_BIN_EXE_eitri-sidecar-standin")), &sidecar],
        );
        let conf = conf.join("\n").replace("{root}", &root.display().to_string());
        sh(
            r#"umask 077 && printf '%s\n' "$1" > "$2""#,
            &[Path::new(&conf), &installed.join("standin.conf")],
        );
        let sidecar_sha256 = format!("{:x}", Sha256::digest(std::fs::read(&sidecar).unwrap()));
        let server = Server {
            root,
            project,
            command,
            version,
            sidecar,
            sidecar_sha256,
        };
        server.ensure();
        server
    }

    fn launcher(&self) -> LocalExec {
        LocalExec {
            helper: self.command.clone(),
        }
    }

    /// Starts the daemon, as an install's start step does.
    fn ensure(&self) {
        let tail: [OsString; 3] = ["ensure".into(), "--args-hex".into(), OsString::new()];
        let child = self.launcher().spawn(&tail).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "ensure failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn start(&self, account: Option<&str>) -> SidecarStart {
        SidecarStart {
            root: self.project.clone(),
            account: account.map(str::to_string),
            command_id: uuid::Uuid::new_v4().to_string(),
            sidecar_revision: SIDECAR_REVISION.into(),
            sidecar_sha256: self.sidecar_sha256.clone(),
        }
    }

    /// A remote tab's provider on this server, reconnecting on `clock`.
    fn provider(
        &self,
        launcher: Arc<dyn ChannelLauncher>,
        clock: Arc<dyn ReconnectClock>,
        account: Option<&str>,
    ) -> std::io::Result<ClaudeSidecarProvider> {
        let client = HelperClient::connect(launcher, HOST, Some(HOST_KEY))?;
        ClaudeSidecarProvider::connect_through_helper_on(client, HOST_KEY, &self.start(account), clock)
    }

    /// A connection for this server's project, opened for the host key it was recorded with.
    fn client(&self, launcher: Arc<dyn ChannelLauncher>) -> HelperClient {
        HelperClient::connect(launcher, HOST, Some(HOST_KEY)).unwrap()
    }

    /// The sidecars the helper lists as running.
    fn alive(&self, direct: &HelperClient) -> usize {
        direct.list().unwrap().iter().filter(|sidecar| sidecar.alive).count()
    }

    /// A connection of the test's own, beside the tab's, serving no project.
    fn direct(&self) -> HelperClient {
        HelperClient::connect(Arc::new(self.launcher()), HOST, None).unwrap()
    }

    fn run_dir(&self) -> PathBuf {
        self.root.join("s/eitri-server/run").join(&self.version)
    }

    /// The pids recorded in `run/<version>`: the daemon's `info` and the sidecars' `sc-*.pid`.
    fn recorded_pids(&self) -> Vec<u32> {
        let mut pids = Vec::new();
        let Ok(entries) = std::fs::read_dir(self.run_dir()) else {
            return pids;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "info" || name.ends_with(".pid") {
                if let Ok(bytes) = std::fs::read(entry.path()) {
                    if let Ok(record) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                        if let Some(pid) = record["pid"].as_u64() {
                            pids.push(pid as u32);
                        }
                    }
                }
            }
        }
        pids
    }
}

/// The executable a live pid runs, if it can be read.
fn exe_of(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

impl Drop for Server {
    /// Stops the daemon (which stops its sidecars) and anything left: only pids recorded under this
    /// server's run directory whose executable is this server's helper or sidecar, signalled through a
    /// pidfd so a reused pid is never hit.
    fn drop(&mut self) {
        let ours = |pid: u32| exe_of(pid).is_some_and(|exe| exe == helper_binary() || exe == self.sidecar);
        for signal in [libc::SIGTERM, libc::SIGKILL] {
            let deadline = Instant::now() + Duration::from_secs(10);
            for pid in self.recorded_pids() {
                // Pinned first and checked after: the pidfd names one process whatever the pid's later use.
                if let Some(fd) = pidfd(pid) {
                    if ours(pid) {
                        signal_through(&fd, signal);
                    }
                }
            }
            while Instant::now() < deadline && self.recorded_pids().into_iter().any(ours) {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        // The daemon holds `live.lock` until its process has gone; only then is nothing left to write here.
        let deadline = Instant::now() + Duration::from_secs(10);
        if let Ok(live) = agent::private_fs::PrivateLock::open_existing(&self.run_dir().join("live.lock")) {
            while Instant::now() < deadline && !live.try_lock().unwrap_or(true) {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn pidfd(pid: u32) -> Option<OwnedFd> {
    // SAFETY: pidfd_open takes a pid and flags and returns a new descriptor or -1.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::c_int, 0) };
    // SAFETY: a non-negative result is a descriptor this process now owns.
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) })
}

fn signal_through(fd: &OwnedFd, signal: libc::c_int) {
    use std::os::fd::AsRawFd;
    // SAFETY: the pidfd is open; a process that has gone gives ESRCH, which is fine.
    unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        );
    }
}

fn signal_pid(pid: u32, signal: libc::c_int) {
    if let Some(fd) = pidfd(pid) {
        signal_through(&fd, signal);
    }
}

/// A launcher whose link can be cut: while down, no channel starts, and cutting kills the channels
/// it started. Records each channel start and link check, in order.
struct Flaky {
    inner: LocalExec,
    down: AtomicBool,
    channels: Mutex<Vec<OwnedFd>>,
    /// Every channel started, pinned, for a test that watches them end.
    started: Mutex<Vec<OwnedFd>>,
    log: Mutex<Vec<&'static str>>,
    /// While set, a link check waits until it is cleared; `checks_held` counts the waiting checks.
    holding_checks: Mutex<bool>,
    checks_released: std::sync::Condvar,
    checks_held: AtomicU64,
    /// When set, every link check reports a restarted master showing this host key.
    restarted_with: Mutex<Option<String>>,
    /// While set (with `restarted_with`), the check reports the master alive: someone else restarted it.
    restarted_elsewhere: AtomicBool,
}

impl Flaky {
    fn new(server: &Server) -> Arc<Self> {
        Self::on(server.launcher())
    }

    fn on(inner: LocalExec) -> Arc<Self> {
        Arc::new(Self {
            inner,
            down: AtomicBool::new(false),
            channels: Mutex::default(),
            started: Mutex::default(),
            log: Mutex::default(),
            holding_checks: Mutex::new(false),
            checks_released: std::sync::Condvar::new(),
            checks_held: AtomicU64::new(0),
            restarted_with: Mutex::new(None),
            restarted_elsewhere: AtomicBool::new(false),
        })
    }

    fn hold_checks(&self, hold: bool) {
        *self.holding_checks.lock().unwrap() = hold;
        self.checks_released.notify_all();
    }

    fn restart_with(&self, key: Option<&str>) {
        *self.restarted_with.lock().unwrap() = key.map(str::to_string);
    }

    fn cut(&self) {
        self.down.store(true, Ordering::SeqCst);
        for channel in self.channels.lock().unwrap().drain(..) {
            signal_through(&channel, libc::SIGKILL);
        }
    }

    fn restore(&self) {
        self.down.store(false, Ordering::SeqCst);
    }

    fn log(&self) -> Vec<&'static str> {
        self.log.lock().unwrap().clone()
    }
}

impl ChannelLauncher for Flaky {
    fn spawn(&self, tail: &[OsString]) -> std::io::Result<Child> {
        self.log.lock().unwrap().push("channel");
        if self.down.load(Ordering::SeqCst) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "the link is down",
            ));
        }
        let child = self.inner.spawn(tail)?;
        // Pinned before the child can be reaped, so a later kill never reaches a reused pid.
        if let Some(fd) = pidfd(child.id()) {
            self.started.lock().unwrap().push(fd.try_clone().unwrap());
            self.channels.lock().unwrap().push(fd);
        }
        Ok(child)
    }

    fn check_link(&self) -> LinkCheck {
        self.log.lock().unwrap().push("check");
        let mut holding = self.holding_checks.lock().unwrap();
        if *holding {
            self.checks_held.fetch_add(1, Ordering::SeqCst);
            while *holding {
                holding = self.checks_released.wait(holding).unwrap();
            }
            self.checks_held.fetch_sub(1, Ordering::SeqCst);
        }
        drop(holding);
        match &*self.restarted_with.lock().unwrap() {
            Some(_) if self.restarted_elsewhere.load(Ordering::SeqCst) => LinkCheck::Alive,
            Some(_) => LinkCheck::Restarted,
            None => LinkCheck::NoMaster,
        }
    }

    fn host_key(&self) -> Option<std::io::Result<String>> {
        self.restarted_with.lock().unwrap().clone().map(Ok)
    }
}

/// A clock whose waits pass at once (a millisecond each, so the watch never spins) while counting the
/// time they stand for.
#[derive(Default)]
struct FastClock {
    waited_ms: AtomicU64,
    /// Every wait asked for, in order, in milliseconds.
    waits: Mutex<Vec<u64>>,
}

impl FastClock {
    fn waited(&self) -> Duration {
        Duration::from_millis(self.waited_ms.load(Ordering::SeqCst))
    }

    fn waits(&self) -> Vec<u64> {
        self.waits.lock().unwrap().clone()
    }

    /// Time passes with nothing waiting on it.
    fn advance(&self, by: Duration) {
        self.waited_ms.fetch_add(by.as_millis() as u64, Ordering::SeqCst);
    }
}

impl ReconnectClock for FastClock {
    fn sleep(&self, wait: Duration) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> {
        self.waits.lock().unwrap().push(wait.as_millis() as u64);
        self.waited_ms.fetch_add(wait.as_millis() as u64, Ordering::SeqCst);
        Box::pin(tokio::time::sleep(Duration::from_millis(1)))
    }

    fn now_unix_ms(&self) -> u64 {
        1_000_000 + self.waited_ms.load(Ordering::SeqCst)
    }
}

fn real_clock() -> Arc<dyn ReconnectClock> {
    Arc::new(agent::remote_client::SystemClock)
}

fn create(provider: &ClaudeSidecarProvider, cwd: &Path) -> String {
    provider
        .create_session(CreateSessionRequest {
            cwd: cwd.display().to_string(),
            streaming: StreamingPreference::Partial,
            project: ProjectTrust::Untrusted,
            client_tools: None,
        })
        .expect("the stand-in opens a session through the helper")
}

fn send(provider: &ClaudeSidecarProvider, session: &str, script: &str) {
    provider
        .send_turn(SendTurnRequest {
            session_id: session.to_string(),
            text: script.to_string(),
            images: Vec::new(),
        })
        .expect("the turn is sent through the helper");
}

/// Pumps the provider into `seen` until `done` holds for what was seen.
fn pump_until(
    provider: &ClaudeSidecarProvider,
    seen: &mut Vec<AgentDomainEvent>,
    what: &str,
    done: impl Fn(&[AgentDomainEvent]) -> bool,
) {
    let started = Instant::now();
    loop {
        seen.extend(provider.pump());
        if done(seen) {
            return;
        }
        assert!(started.elapsed() < HANG, "{what} never came; saw {} events", seen.len());
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn turn_completed(events: &[AgentDomainEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, AgentDomainEvent::TurnCompleted { .. }))
}

fn unavailable(events: &[AgentDomainEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentDomainEvent::SessionUnavailable { reason } => Some(reason.clone()),
            _ => None,
        })
        .collect()
}

fn text_of(events: &[AgentDomainEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            AgentDomainEvent::ContentDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < HANG, "{what} never happened");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The journal head the helper holds for the one session of the one sidecar on `server`.
fn journal_head(direct: &HelperClient) -> u64 {
    direct
        .list()
        .unwrap()
        .iter()
        .flat_map(|sidecar| sidecar.sessions.iter())
        .map(|session| session.journal_head)
        .max()
        .unwrap_or(0)
}

#[test]
fn a_remote_tab_runs_a_turn_through_the_helper() {
    let server = Server::new(&[]);
    let provider = server
        .provider(Arc::new(server.launcher()), real_clock(), None)
        .unwrap();
    assert_eq!(provider.info().host.as_deref(), Some(HOST));
    assert!(provider.info().build_description.unwrap().contains(HOST));
    let mut conversation =
        AgentConversation::create_remote_with_tools(Arc::new(provider), &server.project, ProjectTrust::Untrusted, None)
            .expect("a remote session starts");
    assert_eq!(conversation.canonical_cwd(), server.project);
    conversation.send_turn("text m1 hello from the server; end").unwrap();
    wait_for("the turn's reply", || {
        let projection = conversation.projection();
        projection.active_turn_id.is_none()
            && projection
                .transcript
                .iter()
                .any(|message| message.text.contains("hello from the server"))
    });
    assert_eq!(conversation.link_state(), Some(RemoteLinkState::Live));
}

/// The bytes of `CreateSession` a remote tab sends reach the sidecar exactly as a local tab's do: the
/// helper forwards them and adds nothing.
#[test]
fn create_session_bytes_through_the_helper_equal_the_local_ones() {
    let server = Server::new(&["--record-create {root}/remote-creates.bin"]);
    let remote = server
        .provider(Arc::new(server.launcher()), real_clock(), None)
        .unwrap();
    create(&remote, Path::new("/srv/project"));

    let local_record = server.root.join("local-creates.bin");
    let launcher = server.root.join("local-sidecar");
    let script = format!(
        "#!/bin/sh\nexec {} --record-create {}\n",
        quoted(Path::new(env!("CARGO_BIN_EXE_eitri-sidecar-standin"))),
        quoted(&local_record)
    );
    sh(
        r#"umask 077 && printf '%s' "$1" > "$2" && chmod 700 "$2""#,
        &[Path::new(&script), &launcher],
    );
    let local = ClaudeSidecarProvider::connect_to_program(&uuid::Uuid::new_v4().to_string(), &launcher).unwrap();
    create(&local, Path::new("/srv/project"));

    let remote_bytes = std::fs::read(server.root.join("remote-creates.bin")).unwrap();
    let local_bytes = std::fs::read(&local_record).unwrap();
    assert!(!remote_bytes.is_empty());
    assert_eq!(remote_bytes, local_bytes);
}

/// With the link down for three minutes while the agent asks for permission, the card arrives once
/// after the link returns, and answering it resolves it once.
#[test]
fn reattach_after_minutes_delivers_the_waiting_card_once() {
    let server = Server::new(&[]);
    let flaky = Flaky::new(&server);
    let clock = Arc::new(FastClock::default());
    let provider = server.provider(flaky.clone(), clock.clone(), None).unwrap();
    let session = create(&provider, &server.project);
    let gate = server.root.join("gate");
    std::fs::create_dir(&gate).unwrap();
    send(
        &provider,
        &session,
        &format!("gate {}; ask p1; text m1 after the answer; end", gate.display()),
    );
    wait_for("the stand-in at its gate", || gate.join("ready").exists());

    flaky.cut();
    std::fs::write(gate.join("release"), b"").unwrap();
    let direct = server.direct();
    wait_for("the helper holding the request", || {
        direct
            .list()
            .unwrap()
            .iter()
            .any(|sidecar| sidecar.sessions.iter().any(|s| s.pending == 1))
    });
    wait_for("three minutes of reconnects", || {
        clock.waited() >= Duration::from_secs(180)
    });
    assert!(matches!(
        provider.link_state(),
        Some(RemoteLinkState::Reconnecting { .. } | RemoteLinkState::Disconnected { .. })
    ));
    flaky.restore();

    let mut seen = Vec::new();
    pump_until(&provider, &mut seen, "the waiting card", |seen| {
        seen.iter()
            .any(|event| matches!(event, AgentDomainEvent::PermissionRequested { permission_id, .. } if permission_id == "p1"))
    });
    std::thread::sleep(Duration::from_millis(300));
    seen.extend(provider.pump());
    let mut projection = AgentSessionProjection::default();
    for event in &seen {
        projection.apply(event);
    }
    let asked = seen
        .iter()
        .filter(|event| matches!(event, AgentDomainEvent::PermissionRequested { .. }))
        .count();
    assert_eq!(asked, 1, "the card is delivered once");
    assert_eq!(projection.pending_permissions.len(), 1);
    assert!(projection.pending_permissions.contains_key("p1"));

    provider
        .resolve_permission(ResolvePermissionRequest {
            session_id: session.clone(),
            permission_id: "p1".into(),
            decision: PermissionDecision::Allow,
        })
        .unwrap();
    pump_until(&provider, &mut seen, "the turn's end", turn_completed);
    let resolved = seen
        .iter()
        .filter(|event| matches!(event, AgentDomainEvent::PermissionResolved { .. }))
        .count();
    assert_eq!(resolved, 1, "the answer resolves it once");
    assert!(text_of(&seen).contains("after the answer"));
    assert!(unavailable(&seen).is_empty(), "{:?}", unavailable(&seen));
    assert_eq!(provider.link_state(), Some(RemoteLinkState::Live));
}

/// Two hours without the link: the tab is still reconnecting, nothing says the session is gone, the
/// link is checked before every new channel, and the tab is live again once the link is back.
#[test]
fn reconnect_never_gives_up_while_the_helper_may_hold_the_agent() {
    let server = Server::new(&[]);
    let flaky = Flaky::new(&server);
    let clock = Arc::new(FastClock::default());
    let provider = server.provider(flaky.clone(), clock.clone(), None).unwrap();
    let session = create(&provider, &server.project);
    let mut seen = Vec::new();
    pump_until(&provider, &mut seen, "the session", |seen| {
        seen.iter()
            .any(|event| matches!(event, AgentDomainEvent::SessionOpened { .. }))
    });

    let before = flaky.log().len();
    flaky.cut();
    wait_for("two hours of reconnects", || {
        clock.waited() >= Duration::from_secs(2 * 3600 + 60)
    });
    assert!(
        matches!(
            provider.link_state(),
            Some(RemoteLinkState::Reconnecting { .. } | RemoteLinkState::Disconnected { .. })
        ),
        "{:?}",
        provider.link_state()
    );
    seen.extend(provider.pump());
    assert!(unavailable(&seen).is_empty(), "{:?}", unavailable(&seen));
    let since: Vec<_> = flaky.log()[before..].to_vec();
    assert_eq!(since.first(), Some(&"check"), "{since:?}");
    assert!(
        since.windows(2).all(|pair| pair != ["channel", "channel"]),
        "every new channel follows a link check: {since:?}"
    );
    // Each attempt reads disconnected while its link check runs, then reconnecting while it waits.
    let mut waiting = None;
    wait_for("a wait between attempts", || {
        waiting = match provider.link_state() {
            Some(RemoteLinkState::Reconnecting { attempt, next_in_ms }) => Some((attempt, next_in_ms)),
            _ => None,
        };
        waiting.is_some()
    });
    let (attempt, next_in_ms) = waiting.unwrap();
    assert!(attempt > 100, "{attempt}");
    assert_eq!(next_in_ms, 60_000);

    flaky.restore();
    wait_for("the link back", || provider.link_state() == Some(RemoteLinkState::Live));
    send(&provider, &session, "text m1 still here; end");
    pump_until(&provider, &mut seen, "the turn's end", turn_completed);
    assert!(text_of(&seen).contains("still here"));
    assert!(unavailable(&seen).is_empty());

    // Closing the tab while the link is down ends its reconnects with it.
    flaky.cut();
    wait_for("reconnecting again", || {
        matches!(provider.link_state(), Some(RemoteLinkState::Reconnecting { .. }))
    });
    drop(provider);
    let after_drop = flaky.log().len();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(flaky.log().len(), after_drop, "no reconnect after the provider went");
}

/// After an outage the watch replays what the journal holds and says how far it has come, then goes
/// live. The replay stops at the queue's high-water mark while nothing is taken, which holds it in
/// the middle where it can be read.
#[test]
fn catching_up_reports_progress_and_ends_live() {
    let server = Server::new(&[]);
    let flaky = Flaky::new(&server);
    let provider = server
        .provider(flaky.clone(), Arc::new(FastClock::default()), None)
        .unwrap();
    let session = create(&provider, &server.project);
    let gate = server.root.join("gate");
    std::fs::create_dir(&gate).unwrap();
    send(
        &provider,
        &session,
        &format!("gate {}; flood 2000 16384 m1; end", gate.display()),
    );
    wait_for("the stand-in at its gate", || gate.join("ready").exists());
    let mut seen = Vec::new();
    pump_until(&provider, &mut seen, "the turn's start", |seen| {
        seen.iter()
            .any(|event| matches!(event, AgentDomainEvent::TurnStarted { .. }))
    });
    flaky.cut();
    std::fs::write(gate.join("release"), b"").unwrap();
    let direct = server.direct();
    let head_before = journal_head(&direct);
    wait_for("the whole turn journaled", || {
        journal_head(&direct) >= head_before + 2001
    });
    let head = journal_head(&direct);
    flaky.restore();

    wait_for("the replay held at the high-water mark", || {
        matches!(provider.link_state(), Some(RemoteLinkState::CatchingUp { done, .. }) if done > 0) && {
            let first = provider.link_state();
            std::thread::sleep(Duration::from_millis(200));
            provider.link_state() == first
        }
    });
    let Some(RemoteLinkState::CatchingUp { done, total }) = provider.link_state() else {
        panic!("{:?}", provider.link_state());
    };
    assert!(done < total, "{done} of {total}");
    assert!(total >= 2001 && total <= head, "{total} against head {head}");

    pump_until(&provider, &mut seen, "the turn's end", turn_completed);
    wait_for("live", || provider.link_state() == Some(RemoteLinkState::Live));
    assert_eq!(text_of(&seen).len(), 2000 * 16384);
    assert!(unavailable(&seen).is_empty());
}

/// A replay of 5,000 events that would estimate well past the queue's 64 MiB is held at the
/// high-water mark until the conversation takes what is queued, and arrives whole.
#[test]
fn replay_of_5000_events_stays_under_the_queue_budget() {
    const EVENTS: usize = 5000;
    const BYTES: usize = 8192;
    let server = Server::new(&[]);
    let flaky = Flaky::new(&server);
    let provider = server
        .provider(flaky.clone(), Arc::new(FastClock::default()), None)
        .unwrap();
    let session = create(&provider, &server.project);
    let gate = server.root.join("gate");
    std::fs::create_dir(&gate).unwrap();
    send(
        &provider,
        &session,
        &format!("gate {}; flood {EVENTS} {BYTES} m1; end", gate.display()),
    );
    wait_for("the stand-in at its gate", || gate.join("ready").exists());
    flaky.cut();
    std::fs::write(gate.join("release"), b"").unwrap();
    let direct = server.direct();
    let head_before = journal_head(&direct);
    wait_for("the whole turn journaled", || {
        journal_head(&direct) > head_before + EVENTS as u64
    });
    flaky.restore();

    // Nothing is taken until the watch has stopped reading.
    wait_for("the replay held", || {
        let first = provider.queue_peak_bytes();
        std::thread::sleep(Duration::from_millis(300));
        first >= 16 * 1024 * 1024 && provider.queue_peak_bytes() == first
    });
    let mut seen = Vec::new();
    let mut text = 0usize;
    let started = Instant::now();
    loop {
        let batch = provider.pump();
        text += text_of(&batch).len();
        let done = turn_completed(&batch);
        seen.extend(
            batch
                .into_iter()
                .filter(|event| !matches!(event, AgentDomainEvent::ContentDelta { .. })),
        );
        if done {
            break;
        }
        assert!(
            started.elapsed() < HANG,
            "the replay never finished: {text} bytes so far"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(text, EVENTS * BYTES);
    assert!(unavailable(&seen).is_empty(), "{:?}", unavailable(&seen));
    let peak = provider.queue_peak_bytes();
    assert!(
        peak <= 16 * 1024 * 1024 + 4 * BYTES + 64 * 1024,
        "the queue held {peak} estimated bytes at most"
    );
}

type HelloAnswer =
    dyn Fn(eitri_server_proto::HelloRequest) -> Result<eitri_server_proto::HelloResponse, tonic::Status> + Send + Sync;

/// A daemon that answers Hello as told and nothing else.
struct ScriptedHelper(Arc<HelloAnswer>);

#[tonic::async_trait]
impl eitri_server_proto::helper_server::Helper for ScriptedHelper {
    async fn hello(
        &self,
        request: tonic::Request<eitri_server_proto::HelloRequest>,
    ) -> Result<tonic::Response<eitri_server_proto::HelloResponse>, tonic::Status> {
        (self.0)(request.into_inner()).map(tonic::Response::new)
    }
    async fn start_sidecar(
        &self,
        _: tonic::Request<eitri_server_proto::StartSidecarRequest>,
    ) -> Result<tonic::Response<eitri_server_proto::StartSidecarResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("not here"))
    }
    async fn stop_sidecar(
        &self,
        _: tonic::Request<eitri_server_proto::StopSidecarRequest>,
    ) -> Result<tonic::Response<eitri_server_proto::StopSidecarResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("not here"))
    }
    async fn list_sidecars(
        &self,
        _: tonic::Request<eitri_server_proto::ListSidecarsRequest>,
    ) -> Result<tonic::Response<eitri_server_proto::ListSidecarsResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("not here"))
    }
    async fn pending(
        &self,
        _: tonic::Request<eitri_server_proto::PendingRequest>,
    ) -> Result<tonic::Response<eitri_server_proto::PendingResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("not here"))
    }
    async fn trust_scan(
        &self,
        _: tonic::Request<eitri_server_proto::TrustScanRequest>,
    ) -> Result<tonic::Response<eitri_server_proto::TrustScanResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("not here"))
    }
    async fn sync_ignores(
        &self,
        _: tonic::Request<eitri_server_proto::SyncIgnoresRequest>,
    ) -> Result<tonic::Response<eitri_server_proto::SyncIgnoresResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("SyncIgnores is not built yet"))
    }
    async fn git_config(
        &self,
        _: tonic::Request<eitri_server_proto::GitConfigRequest>,
    ) -> Result<tonic::Response<eitri_server_proto::GitConfigResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("GitConfig is not built yet"))
    }

    type ListTreeStream = std::pin::Pin<
        Box<
            dyn tonic::codegen::tokio_stream::Stream<Item = Result<eitri_server_proto::ListTreeEntry, tonic::Status>>
                + Send,
        >,
    >;
    async fn list_tree(
        &self,
        _: tonic::Request<eitri_server_proto::ListTreeRequest>,
    ) -> Result<tonic::Response<Self::ListTreeStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("ListTree is not built yet"))
    }

    type ReadFileStream = std::pin::Pin<
        Box<
            dyn tonic::codegen::tokio_stream::Stream<Item = Result<eitri_server_proto::ReadFileChunk, tonic::Status>>
                + Send,
        >,
    >;
    async fn read_file(
        &self,
        _: tonic::Request<eitri_server_proto::ReadFileRequest>,
    ) -> Result<tonic::Response<Self::ReadFileStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("ReadFile is not built yet"))
    }
    async fn replace_file_if(
        &self,
        _: tonic::Request<eitri_server_proto::ReplaceFileIfRequest>,
    ) -> Result<tonic::Response<eitri_server_proto::ReplaceFileIfResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("ReplaceFileIf is not built yet"))
    }
    async fn remove_file_if(
        &self,
        _: tonic::Request<eitri_server_proto::RemoveFileIfRequest>,
    ) -> Result<tonic::Response<eitri_server_proto::RemoveFileIfResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("RemoveFileIf is not built yet"))
    }
    async fn wait_file_hash(
        &self,
        _: tonic::Request<eitri_server_proto::WaitFileHashRequest>,
    ) -> Result<tonic::Response<eitri_server_proto::WaitFileHashResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("WaitFileHash is not built yet"))
    }
    async fn path_sizes(
        &self,
        _: tonic::Request<eitri_server_proto::PathSizesRequest>,
    ) -> Result<tonic::Response<eitri_server_proto::PathSizesResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("PathSizes is not built yet"))
    }
    async fn project_activity(
        &self,
        _: tonic::Request<eitri_server_proto::ProjectActivityRequest>,
    ) -> Result<tonic::Response<eitri_server_proto::ProjectActivityResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("ProjectActivity is not built yet"))
    }
    async fn watch_modes(
        &self,
        _: tonic::Request<eitri_server_proto::WatchModesRequest>,
    ) -> Result<tonic::Response<eitri_server_proto::WatchModesResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("WatchModes is not built yet"))
    }
    type ExecStream = std::pin::Pin<
        Box<
            dyn tonic::codegen::tokio_stream::Stream<Item = Result<eitri_server_proto::ExecOutput, tonic::Status>>
                + Send,
        >,
    >;
    async fn exec(
        &self,
        _: tonic::Request<tonic::Streaming<eitri_server_proto::ExecInput>>,
    ) -> Result<tonic::Response<Self::ExecStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("Exec is not built yet"))
    }
}

impl Server {
    /// Stops the real daemon and puts one answering Hello with `hello` on its socket; a connection to
    /// it through `eitri-server connect`. The runtime serves it while it is kept.
    fn replace_daemon(
        &self,
        hello: impl Fn(eitri_server_proto::HelloRequest) -> Result<eitri_server_proto::HelloResponse, tonic::Status>
            + Send
            + Sync
            + 'static,
    ) -> (tokio::runtime::Runtime, HelperClient) {
        for pid in self.recorded_pids() {
            if let Some(fd) = pidfd(pid) {
                if exe_of(pid).is_some_and(|exe| exe == helper_binary()) {
                    signal_through(&fd, libc::SIGTERM);
                }
            }
        }
        wait_for("the daemon gone", || {
            !self
                .recorded_pids()
                .into_iter()
                .any(|pid| exe_of(pid).is_some_and(|exe| exe == helper_binary()))
        });
        let socket = agent::socket_path::in_dir(&self.run_dir(), "daemon.sock").unwrap();
        let _ = std::fs::remove_file(&socket);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let listener = {
            let _entered = runtime.enter();
            tokio::net::UnixListener::bind(&socket).unwrap()
        };
        let helper = ScriptedHelper(Arc::new(hello));
        runtime.spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(eitri_server_proto::helper_server::HelperServer::new(helper))
                .serve_with_incoming(tonic::codegen::tokio_stream::wrappers::UnixListenerStream::new(
                    listener,
                ))
                .await;
        });
        let client = self.client(Arc::new(self.launcher()));
        (runtime, client)
    }
}

/// A helper that answers Hello in another protocol major is refused with both sides' versions named.
#[test]
fn helper_major_mismatch_names_both_versions() {
    use eitri_server_proto::{HelloResponse, PROTOCOL_MAJOR, PROTOCOL_MINOR};
    let server = Server::new(&[]);
    let (runtime, client) = server.replace_daemon(|_| {
        Ok(HelloResponse {
            protocol_major: PROTOCOL_MAJOR + 1,
            protocol_minor: 4,
            helper_version: "9.9.9".into(),
            daemon_instance: String::new(),
            boot_id: String::new(),
            ..Default::default()
        })
    });
    let refused = client.hello().unwrap_err();
    let message = refused.message();
    assert!(message.contains("9.9.9"), "{message}");
    assert!(message.contains(&format!("{}.4", PROTOCOL_MAJOR + 1)), "{message}");
    assert!(message.contains(env!("CARGO_PKG_VERSION")), "{message}");
    assert!(
        message.contains(&format!("{PROTOCOL_MAJOR}.{PROTOCOL_MINOR}")),
        "{message}"
    );
    assert!(message.contains(HOST), "{message}");
    let start = ClaudeSidecarProvider::connect_through_helper_on(client, HOST_KEY, &server.start(None), real_clock());
    assert!(start.is_err());
    runtime.shutdown_background();
}

/// The account the server resolved and the host the tab runs on reach the provider's facts.
#[test]
fn account_and_host_reach_provider_info() {
    let server = Server::new(&["EITRI_STANDIN_ACCOUNTS={root}"]);
    std::fs::create_dir(server.root.join(".claude-example-account")).unwrap();
    let provider = server
        .provider(Arc::new(server.launcher()), real_clock(), Some("example-account"))
        .unwrap();
    let info = provider.info();
    assert_eq!(info.host.as_deref(), Some(HOST));
    assert_eq!(info.account_name.as_deref(), Some("example-account"));
    assert_eq!(
        info.account_config_dir,
        Some(server.root.join(".claude-example-account"))
    );
    assert_eq!(provider.local_sidecar_pid(), None, "the sidecar is not on this machine");
}

/// An account the server lacks fails the start with the account and the host named.
#[test]
fn unknown_account_on_the_server_fails_the_start_naming_the_host() {
    let server = Server::new(&["EITRI_STANDIN_ACCOUNTS={root}"]);
    let refused = server
        .provider(Arc::new(server.launcher()), real_clock(), Some("nobody"))
        .err()
        .expect("the start is refused")
        .to_string();
    assert_eq!(refused, format!("account nobody was not found on {HOST}"));
}

/// The editor's tools are answered by this side over the same connection: the call and its answer
/// cross the helper unchanged.
#[test]
fn client_tool_calls_cross_the_helper_unchanged() {
    let server = Server::new(&["--client-tools", "--record-answers {root}/answers.txt"]);
    let provider = server
        .provider(Arc::new(server.launcher()), real_clock(), None)
        .unwrap();
    assert!(provider.capabilities().client_tools);
    let session = create(&provider, &server.project);
    send(&provider, &session, "client-call c1 open_file tool-1");
    let mut seen = Vec::new();
    pump_until(&provider, &mut seen, "the tool call", |seen| {
        seen.iter()
            .any(|event| matches!(event, AgentDomainEvent::ClientToolCall { call_id, .. } if call_id == "c1"))
    });
    let call = seen
        .iter()
        .find_map(|event| match event {
            AgentDomainEvent::ClientToolCall {
                tool,
                tool_use_id,
                arguments_json,
                ..
            } => Some((tool.clone(), tool_use_id.clone(), arguments_json.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        call,
        (
            "open_file".to_string(),
            Some("tool-1".to_string()),
            r#"{"path":"/the/arguments"}"#.to_string()
        )
    );
    provider
        .answer_client_tool_call(ClientToolAnswer {
            session_id: session,
            call_id: "c1".into(),
            text: vec!["opened".into()],
            is_error: false,
        })
        .unwrap();
    let answers = server.root.join("answers.txt");
    wait_for("the answer at the sidecar", || {
        std::fs::read_to_string(&answers).is_ok_and(|text| text.contains("replied c1"))
    });
    assert_eq!(
        std::fs::read_to_string(&answers).unwrap().lines().next(),
        Some("c1 taken false opened")
    );
}

/// `link_state` is `Some` from a remote tab's start to its end, and `Live` while nothing is wrong.
#[test]
fn link_state_is_some_from_start_to_end_of_a_remote_tab() {
    let server = Server::new(&[]);
    let provider = server
        .provider(Arc::new(server.launcher()), real_clock(), None)
        .unwrap();
    assert_eq!(provider.link_state(), Some(RemoteLinkState::Live));
    let session = create(&provider, &server.project);
    assert_eq!(provider.link_state(), Some(RemoteLinkState::Live));
    send(&provider, &session, "text m1 one; end");
    let mut seen = Vec::new();
    pump_until(&provider, &mut seen, "the turn's end", turn_completed);
    assert_eq!(provider.link_state(), Some(RemoteLinkState::Live));
    provider
        .close_session(agent::CloseSessionRequest {
            session_id: session.clone(),
        })
        .unwrap();
    pump_until(&provider, &mut seen, "the session's close", |seen| {
        seen.iter()
            .any(|event| matches!(event, AgentDomainEvent::SessionClosed { .. }))
    });
    assert!(provider.link_state().is_some());
}

/// A local tab keeps its own rule: no link state, and a sidecar that dies is reported lost after its
/// few quick retries, not retried for minutes.
#[test]
fn a_local_tab_keeps_its_watch_rule() {
    let scratch = std::env::temp_dir().join(format!("rt-local-{}", uuid::Uuid::new_v4().simple()));
    agent::private_fs::create_private_dir_all(&scratch, &scratch).unwrap();
    struct Clean(PathBuf);
    impl Drop for Clean {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _clean = Clean(scratch.clone());
    let provider = ClaudeSidecarProvider::connect_to_program(
        &uuid::Uuid::new_v4().to_string(),
        Path::new(env!("CARGO_BIN_EXE_eitri-sidecar-standin")),
    )
    .unwrap();
    assert_eq!(provider.link_state(), None);
    assert_eq!(provider.info().host, None);
    let session = create(&provider, &scratch);
    let mut seen = Vec::new();
    pump_until(&provider, &mut seen, "the session", |seen| {
        seen.iter()
            .any(|event| matches!(event, AgentDomainEvent::SessionOpened { .. }))
    });
    signal_pid(provider.sidecar_pid(), libc::SIGKILL);
    pump_until(&provider, &mut seen, "the loss", |seen| !unavailable(seen).is_empty());
    let retries = provider.backpressure_stats().watch_reconnects;
    assert!(retries <= 3, "{retries} retries before the loss");
    assert_eq!(provider.link_state(), None);
    let _ = session;
}

/// A root this machine resolves to another path is refused before the helper starts anything.
#[test]
fn a_refused_root_starts_no_sidecar() {
    let server = Server::new(&[]);
    let link = server.root.join("link");
    std::os::unix::fs::symlink(&server.project, &link).unwrap();
    let client = server.client(Arc::new(server.launcher()));
    let start = SidecarStart {
        root: link.clone(),
        ..server.start(None)
    };
    let refused = ClaudeSidecarProvider::connect_through_helper_on(client, HOST_KEY, &start, real_clock())
        .err()
        .expect("the root is refused")
        .to_string();
    assert!(refused.contains(&link.display().to_string()), "{refused}");
    assert!(server.direct().list().unwrap().is_empty(), "nothing was started");
}

/// A sidecar the helper started is stopped again when the session cannot be created on it, and when
/// it does not pass the handshake.
#[test]
fn a_start_that_fails_after_the_sidecar_started_stops_it() {
    for knob in [
        "EITRI_STANDIN_REFUSE_CREATE=1",
        "EITRI_STANDIN_INCOMPATIBLE=a sidecar of another major",
    ] {
        let server = Server::new(&[knob]);
        let direct = server.direct();
        let client = server.client(Arc::new(server.launcher()));
        let failed = agent::remote_client::open_remote_session(
            client,
            HOST_KEY,
            &server.start(None),
            ProjectTrust::Untrusted,
            None,
        )
        .err()
        .expect("the start fails");
        assert!(!direct.list().unwrap().is_empty(), "{knob}: a sidecar was started");
        wait_for("the sidecar stopped", || server.alive(&direct) == 0);
        let _ = failed;
    }
}

/// The link breaks after the helper started the sidecar and before the tab's handshake answered: the start fails, the
/// stop sent after the failed handshake cannot go out either, and the failure names the sidecar left running.
#[test]
fn a_link_cut_during_the_handshake_names_the_sidecar_left_running_for_its_stop() {
    let gate = std::env::temp_dir().join(format!(
        "handshake-gate-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    ));
    agent::private_fs::create_private_dir_all(&gate, &gate).unwrap();
    let server = Server::new(&[&format!("EITRI_STANDIN_HANDSHAKE_GATE={}", gate.display())]);
    let flaky = Flaky::new(&server);
    let client = server.client(flaky.clone());
    let start = server.start(None);
    let starting = std::thread::spawn(move || {
        agent::remote_client::open_remote_session(client, HOST_KEY, &start, ProjectTrust::Untrusted, None)
    });
    wait_for("the tab's handshake", || gate.join("handshaking").exists());
    flaky.cut();
    std::fs::write(gate.join("release"), b"").unwrap();
    let failed = starting
        .join()
        .unwrap()
        .err()
        .expect("the start fails with the link cut");
    assert!(
        failed.info.is_none() && failed.message.contains("did not answer"),
        "the handshake failed, not a later step: {}",
        failed.message
    );
    let sidecar = failed.unstopped.expect("the sidecar left running is named");
    let direct = server.direct();
    assert_eq!(server.alive(&direct), 1, "the sidecar still runs: {}", failed.message);
    assert!(direct
        .list()
        .unwrap()
        .iter()
        .any(|listed| listed.sidecar_id == sidecar.sidecar_id));
    flaky.restore();
    let (daemon, id) = sidecar
        .stop_key()
        .split_once('/')
        .map_or((String::new(), sidecar.stop_key()), |(d, i)| {
            (d.to_string(), i.to_string())
        });
    server
        .client(Arc::new(server.launcher()))
        .stop_saved(&daemon, &id)
        .unwrap();
    wait_for("the sidecar stopped", || server.alive(&direct) == 0);
    let _ = std::fs::remove_dir_all(&gate);
}

/// The link breaks while the session is being created: the start fails, the stop it then sends cannot go out, and
/// the failure names the sidecar left running, whose kept stop stops it once the server can be reached again.
#[test]
fn a_link_cut_during_the_create_names_the_sidecar_left_running_for_its_stop() {
    let gate = std::env::temp_dir().join(format!(
        "create-gate-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    ));
    agent::private_fs::create_private_dir_all(&gate, &gate).unwrap();
    let server = Server::new(&[&format!("EITRI_STANDIN_CREATE_GATE={}", gate.display())]);
    let flaky = Flaky::new(&server);
    let client = server.client(flaky.clone());
    let start = server.start(None);
    let starting = std::thread::spawn(move || {
        agent::remote_client::open_remote_session(client, HOST_KEY, &start, ProjectTrust::Untrusted, None)
    });
    wait_for("the session being created", || gate.join("creating").exists());
    flaky.cut();
    std::fs::write(gate.join("release"), b"").unwrap();
    let failed = starting
        .join()
        .unwrap()
        .err()
        .expect("the start fails with the link cut");
    let sidecar = failed.unstopped.expect("the sidecar left running is named");
    let direct = server.direct();
    assert_eq!(server.alive(&direct), 1, "the sidecar still runs: {}", failed.message);
    assert_eq!(sidecar.host, HOST);
    assert_eq!(sidecar.host_key_sha256, HOST_KEY);
    assert!(direct
        .list()
        .unwrap()
        .iter()
        .any(|listed| listed.sidecar_id == sidecar.sidecar_id));
    // What a window's stop sender does with the kept stop once the link is back.
    flaky.restore();
    let (daemon, id) = sidecar
        .stop_key()
        .split_once('/')
        .map_or((String::new(), sidecar.stop_key()), |(d, i)| {
            (d.to_string(), i.to_string())
        });
    server
        .client(Arc::new(server.launcher()))
        .stop_saved(&daemon, &id)
        .unwrap();
    wait_for("the sidecar stopped", || server.alive(&direct) == 0);
    let _ = std::fs::remove_dir_all(&gate);
}

/// A start whose channel breaks while the helper is starting the sidecar is asked again with the same
/// command id: the helper answers with the sidecar it started, and one sidecar runs.
#[test]
fn a_start_whose_answer_was_lost_is_asked_again() {
    let server = Server::new(&["EITRI_STANDIN_BIND_AFTER_MS=1500"]);
    // Its channels say something on stderr first, as ssh does with a warning: their last word when cut.
    let talking = server.root.join("talking");
    sh(
        r#"umask 077 && printf '%s' "$1" > "$2" && chmod 700 "$2""#,
        &[
            Path::new(&format!(
                "#!/bin/sh\necho 'Warning: a line the channel printed' >&2\nexec {} \"$@\"\n",
                quoted(&server.command)
            )),
            &talking,
        ],
    );
    let flaky = Flaky::on(LocalExec { helper: talking });
    let client = server.client(flaky.clone());
    let start = server.start(None);
    let starting = std::thread::spawn(move || {
        ClaudeSidecarProvider::connect_through_helper_on(client, HOST_KEY, &start, real_clock())
    });
    let direct = server.direct();
    wait_for("the sidecar starting", || server.alive(&direct) == 1);
    flaky.cut();
    flaky.restore();
    let provider = starting.join().unwrap().expect("the start is asked again");
    assert_eq!(server.alive(&direct), 1, "one sidecar runs");
    assert_eq!(direct.list().unwrap().len(), 1, "and no other was started");
    drop(provider);
}

/// Two tabs on one connection: closing one ends its reconnects only; the other goes on.
#[test]
fn closing_one_of_two_tabs_on_one_connection_ends_only_its_watch() {
    let server = Server::new(&[]);
    let flaky = Flaky::new(&server);
    let client = server.client(flaky.clone());
    let (first_clock, second_clock) = (Arc::new(FastClock::default()), Arc::new(FastClock::default()));
    let first = ClaudeSidecarProvider::connect_through_helper_on(
        client.clone(),
        HOST_KEY,
        &server.start(None),
        first_clock.clone(),
    )
    .unwrap();
    let second = ClaudeSidecarProvider::connect_through_helper_on(
        client.clone(),
        HOST_KEY,
        &server.start(None),
        second_clock.clone(),
    )
    .unwrap();
    create(&first, &server.project);
    create(&second, &server.project);
    flaky.cut();
    wait_for("both reconnecting", || {
        first_clock.waited() > Duration::from_secs(10) && second_clock.waited() > Duration::from_secs(10)
    });
    drop(first);
    let first_after = first_clock.waited();
    let second_after = second_clock.waited();
    wait_for("the other tab going on", || {
        second_clock.waited() > second_after + Duration::from_secs(600)
    });
    assert_eq!(first_clock.waited(), first_after, "the closed tab waits no more");
    drop(second);
}

/// A tab closed while its link check is stuck goes at once; the check finishes on its own.
#[test]
fn closing_a_tab_during_a_link_check_does_not_wait_for_it() {
    let server = Server::new(&[]);
    let flaky = Flaky::new(&server);
    let provider = server
        .provider(flaky.clone(), Arc::new(FastClock::default()), None)
        .unwrap();
    create(&provider, &server.project);
    flaky.hold_checks(true);
    flaky.cut();
    wait_for("a link check under way", || {
        flaky.checks_held.load(Ordering::SeqCst) > 0
    });
    let dropped = Arc::new(AtomicBool::new(false));
    let dropper = {
        let dropped = dropped.clone();
        std::thread::spawn(move || {
            drop(provider);
            dropped.store(true, Ordering::SeqCst);
        })
    };
    wait_for("the tab gone while its check still runs", || {
        dropped.load(Ordering::SeqCst)
    });
    assert!(flaky.checks_held.load(Ordering::SeqCst) > 0, "the check is still held");
    flaky.hold_checks(false);
    dropper.join().unwrap();
}

/// A stream that stayed open a while ends its outage: the next outage counts its attempts from the
/// start and waits half a second first, not the minute the last outage reached.
#[test]
fn a_later_outage_starts_its_count_again() {
    let server = Server::new(&[]);
    let flaky = Flaky::new(&server);
    let clock = Arc::new(FastClock::default());
    let provider = server.provider(flaky.clone(), clock.clone(), None).unwrap();
    create(&provider, &server.project);
    // Everything the session has said is read before the outage, so no replay after it delivers an
    // event (which would start the count again by itself).
    let journaled = journal_head(&server.direct());
    assert!(journaled > 0);
    wait_for("the watch at the journal's head", || {
        provider.pump();
        provider.pumped_through().is_some_and(|through| through >= journaled)
    });
    flaky.cut();
    wait_for("a long outage", || {
        clock.waits().iter().filter(|wait| **wait == 60_000).count() >= 2
    });
    flaky.restore();
    wait_for("the link back", || provider.link_state() == Some(RemoteLinkState::Live));
    clock.advance(Duration::from_secs(31));
    let before = clock.waits().len();
    flaky.cut();
    wait_for("the next outage's first wait", || clock.waits().len() > before);
    assert_eq!(clock.waits()[before], 500, "{:?}", &clock.waits()[before..]);
}

/// A master restarted onto a host key other than the project's carries no channel: the tab stays
/// disconnected until the pinned key is back.
#[test]
fn a_restarted_master_with_another_host_key_is_not_used() {
    let server = Server::new(&[]);
    let flaky = Flaky::new(&server);
    let provider = server
        .provider(flaky.clone(), Arc::new(FastClock::default()), None)
        .unwrap();
    create(&provider, &server.project);
    flaky.restart_with(Some("SHA256:another-key"));
    flaky.cut();
    flaky.restore();
    let marked = flaky.log().len();
    wait_for("several checks", || {
        flaky.log()[marked..].iter().filter(|entry| **entry == "check").count() >= 5
    });
    assert!(
        !flaky.log()[marked..].contains(&"channel"),
        "no channel through the master with another key: {:?}",
        &flaky.log()[marked..]
    );
    assert_ne!(provider.link_state(), Some(RemoteLinkState::Live));
    flaky.restart_with(Some(HOST_KEY));
    wait_for("the link back on the pinned key", || {
        provider.link_state() == Some(RemoteLinkState::Live)
    });
}

/// What the channel said on its way out reaches the person.
#[test]
fn the_channels_reason_reaches_the_person() {
    let server = Server::new(&[]);
    let helper = server.root.join("refusing");
    sh(
        r#"umask 077 && printf '%s' "$1" > "$2" && chmod 700 "$2""#,
        &[
            Path::new("#!/bin/sh\necho 'example-host: Permission denied (publickey).' >&2\nexit 255\n"),
            &helper,
        ],
    );
    let failed = match HelperClient::connect(Arc::new(LocalExec { helper }), HOST, None) {
        Err(error) => error.to_string(),
        Ok(client) => client.hello().err().expect("no helper answers").message().to_string(),
    };
    assert!(failed.contains("Permission denied (publickey)."), "{failed}");
}

/// The helper's own refusal of this window's protocol keeps its words and names this window's version.
#[test]
fn the_helpers_refusal_of_this_protocol_reaches_the_error() {
    let server = Server::new(&[]);
    let (_runtime, client) = server.replace_daemon(|request| {
        let _ = request;
        Err(tonic::Status::failed_precondition("unsupported helper protocol major"))
    });
    let refused = client.hello().err().expect("refused");
    let message = refused.message();
    assert!(message.contains("unsupported helper protocol major"), "{message}");
    assert!(message.contains(env!("CARGO_PKG_VERSION")), "{message}");
    assert!(message.contains(HOST), "{message}");
}

/// A start that fails for another reason while the account's directory exists keeps the helper's words.
#[test]
fn a_failed_start_keeps_the_helpers_words_when_the_account_exists() {
    let server = Server::new(&[
        "EITRI_STANDIN_ACCOUNTS={root}",
        "EITRI_STANDIN_REFUSE_BIND=refusing to start for another reason",
    ]);
    std::fs::create_dir(server.root.join(".claude-example-account")).unwrap();
    let refused = server
        .provider(Arc::new(server.launcher()), real_clock(), Some("example-account"))
        .err()
        .expect("the start fails")
        .to_string();
    assert!(refused.contains("exited before binding"), "{refused}");
    assert!(!refused.contains("was not found"), "{refused}");
}

/// Dropping the connection kills its channel's child and reaps it.
#[test]
fn the_connect_child_ends_with_its_connection() {
    let server = Server::new(&[]);
    let flaky = Flaky::new(&server);
    let client = HelperClient::connect(flaky.clone(), HOST, None).unwrap();
    client.hello().unwrap();
    let started: Vec<OwnedFd> = flaky
        .started
        .lock()
        .unwrap()
        .iter()
        .map(|fd| fd.try_clone().unwrap())
        .collect();
    assert!(!started.is_empty());
    drop(client);
    for fd in &started {
        use std::os::fd::AsRawFd;
        wait_for("the channel's child ended", || {
            let mut poll = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one pollfd for an open pidfd, no wait.
            unsafe { libc::poll(&mut poll, 1, 0) == 1 }
        });
        // Reaped: no zombie of it is left to this process.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        wait_for("the channel's child reaped", || {
            // SAFETY: as above.
            let again = unsafe {
                libc::waitid(
                    libc::P_PIDFD,
                    fd.as_raw_fd() as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            again == -1
        });
    }
}

/// A master someone else restarted onto another host key is not used either: the connection is
/// compared with the pinned key before every channel, not only after this tab's own restart.
#[test]
fn a_master_restarted_elsewhere_onto_another_host_key_is_not_used() {
    let server = Server::new(&[]);
    let flaky = Flaky::new(&server);
    let provider = server
        .provider(flaky.clone(), Arc::new(FastClock::default()), None)
        .unwrap();
    create(&provider, &server.project);
    flaky.restarted_elsewhere.store(true, Ordering::SeqCst);
    flaky.restart_with(Some("SHA256:another-key"));
    flaky.cut();
    flaky.restore();
    let marked = flaky.log().len();
    wait_for("several checks", || {
        flaky.log()[marked..].iter().filter(|entry| **entry == "check").count() >= 5
    });
    assert!(
        !flaky.log()[marked..].contains(&"channel"),
        "no channel through a master showing another key: {:?}",
        &flaky.log()[marked..]
    );
    assert_ne!(provider.link_state(), Some(RemoteLinkState::Live));
    flaky.restart_with(Some(HOST_KEY));
    wait_for("the link back on the pinned key", || {
        provider.link_state() == Some(RemoteLinkState::Live)
    });
}

/// A connection that shows another host key than the project's from its very first channel starts
/// nothing: no channel, no sidecar, and the error names the host and both keys.
#[test]
fn a_first_connection_showing_another_host_key_starts_nothing() {
    let server = Server::new(&[]);
    let flaky = Flaky::new(&server);
    flaky.restart_with(Some("SHA256:another-key"));
    let refused = server
        .provider(flaky.clone(), real_clock(), None)
        .err()
        .expect("the start is refused")
        .to_string();
    assert!(refused.contains(HOST), "{refused}");
    assert!(
        refused.contains("SHA256:another-key") && refused.contains(HOST_KEY),
        "{refused}"
    );
    assert!(
        !flaky.log().contains(&"channel"),
        "no channel started: {:?}",
        flaky.log()
    );
    assert!(server.direct().list().unwrap().is_empty(), "no sidecar was started");
}

/// One connection serves one recorded host key: a tab of a project recorded with another key is refused
/// on it before anything starts, a tab with the same key is not, and a connection opened without a key
/// carries no tab.
#[test]
fn a_connection_serves_only_the_host_key_it_was_opened_with() {
    let server = Server::new(&[]);
    let direct = server.direct();
    let client = server.client(Arc::new(server.launcher()));
    let first =
        ClaudeSidecarProvider::connect_through_helper_on(client.clone(), HOST_KEY, &server.start(None), real_clock())
            .unwrap();
    let refused = ClaudeSidecarProvider::connect_through_helper_on(
        client.clone(),
        "SHA256:another-key",
        &server.start(None),
        real_clock(),
    )
    .err()
    .expect("a tab of another key is refused")
    .to_string();
    assert!(
        refused.contains("SHA256:another-key") && refused.contains(HOST_KEY),
        "{refused}"
    );
    assert_eq!(direct.list().unwrap().len(), 1, "nothing started for the refused tab");
    let same = ClaudeSidecarProvider::connect_through_helper_on(client, HOST_KEY, &server.start(None), real_clock())
        .expect("the same key is fine");
    let unpinned =
        ClaudeSidecarProvider::connect_through_helper_on(server.direct(), HOST_KEY, &server.start(None), real_clock())
            .err()
            .expect("a connection opened without a key carries no tab")
            .to_string();
    assert!(unpinned.contains(HOST_KEY), "{unpinned}");
    assert_eq!(direct.list().unwrap().len(), 2, "nothing started for it either");
    drop((first, same));
}

/// A connection opened without a project's host key starts no sidecar and scans no tree: both carry a
/// project, and its channels were never compared with the key the project was recorded with.
#[test]
fn a_connection_without_a_pin_serves_no_project() {
    let server = Server::new(&[]);
    let direct = server.direct();
    let refused = direct.start_sidecar(&server.start(None)).unwrap_err();
    assert_eq!(refused.code(), tonic::Code::FailedPrecondition, "{refused:?}");
    assert!(refused.message().contains(HOST), "{}", refused.message());
    let refused = direct.trust_scan(&server.project, "claude").unwrap_err();
    assert_eq!(refused.code(), tonic::Code::FailedPrecondition, "{refused:?}");
    assert!(direct.list().unwrap().is_empty(), "nothing started");
}

impl Server {
    /// Kills the daemon outright, as a crash does: its sidecars keep running, unwatched.
    fn kill_daemon(&self) {
        for pid in self.recorded_pids() {
            if let Some(fd) = pidfd(pid) {
                if exe_of(pid).is_some_and(|exe| exe == helper_binary()) {
                    signal_through(&fd, libc::SIGKILL);
                }
            }
        }
        wait_for("the daemon gone", || {
            !self
                .recorded_pids()
                .into_iter()
                .any(|pid| exe_of(pid).is_some_and(|exe| exe == helper_binary()))
        });
        let deadline = Instant::now() + HANG;
        let live = agent::private_fs::PrivateLock::open_existing(&self.run_dir().join("live.lock")).unwrap();
        while !live.try_lock().unwrap_or(false) {
            assert!(
                Instant::now() < deadline,
                "the daemon's lifetime lock was never released"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// A remote tab with a request left waiting in a stretch of events the journal never got: the daemon
/// is killed at the gate, the agent runs on past its sidecar's eight-event ring and asks, and a new
/// daemon adopts it. The tab's link is down meanwhile.
fn a_gap_with_a_waiting_request(server: &Server, flaky: &Arc<Flaky>, provider: &ClaudeSidecarProvider, session: &str) {
    a_gap(
        server,
        flaky,
        provider,
        session,
        "ask p1; text m1 after the answer; end",
    );
    let direct = server.direct();
    wait_for("the new daemon holding the request", || {
        direct
            .list()
            .unwrap()
            .iter()
            .any(|sidecar| sidecar.sessions.iter().any(|s| s.pending == 1))
    });
    flaky.restore();
}

/// The daemon is killed at the gate, the agent floods past its sidecar's eight-event ring and goes on with
/// `then`, and a new daemon adopts it, while the tab's link is down. The link is left down.
fn a_gap(server: &Server, flaky: &Arc<Flaky>, provider: &ClaudeSidecarProvider, session: &str, then: &str) {
    let gate = server.root.join("gate");
    std::fs::create_dir(&gate).unwrap();
    let flooded = server.root.join("flooded");
    let _ = std::fs::remove_file(&flooded);
    send(
        provider,
        session,
        &format!(
            "gate {}; flood 40 16 m1; touch {}; {then}",
            gate.display(),
            flooded.display()
        ),
    );
    wait_for("the stand-in at its gate", || gate.join("ready").exists());
    flaky.cut();
    server.kill_daemon();
    std::fs::write(gate.join("release"), b"").unwrap();
    // The flood is past the ring's room once the stand-in says it got beyond it.
    wait_for("the stand-in past its flood", || flooded.exists());
    server.ensure();
}

/// After a gap with nothing waiting the agent may still be running: closing the tab stops it on the server, and a
/// tab let go of keeps it there.
#[test]
fn closing_a_tab_after_a_gap_stops_its_agent_and_letting_go_keeps_it() {
    for let_go in [false, true] {
        let server = Server::new(&["EITRI_STANDIN_RING=8"]);
        let flaky = Flaky::new(&server);
        let provider = Arc::new(
            server
                .provider(flaky.clone(), Arc::new(FastClock::default()), None)
                .unwrap(),
        );
        let mut conversation = AgentConversation::create_remote_with_tools(
            provider.clone(),
            &server.project,
            ProjectTrust::Untrusted,
            None,
        )
        .unwrap();
        let session = conversation.session_id().unwrap().to_string();
        a_gap(&server, &flaky, &provider, &session, "text m1 after the flood; end");
        flaky.restore();
        wait_for("the gap's ending", || {
            matches!(
                conversation.projection().status,
                agent::ProjectionStatus::Unavailable { .. }
            )
        });
        assert_eq!(
            conversation.remote_ending(),
            Some(RemoteEnding::Gap { waiting: Vec::new() })
        );
        let direct = server.direct();
        assert_eq!(server.alive(&direct), 1);
        if let_go {
            conversation.detach();
            std::thread::sleep(Duration::from_millis(200));
            assert_eq!(server.alive(&direct), 1, "a tab let go of keeps its agent");
        } else {
            conversation.shutdown();
            assert_eq!(conversation.remote_stop_failure(), None);
            wait_for("the sidecar stopped", || server.alive(&direct) == 0);
        }
    }
}

/// A gap ends the tab as a gap always has, and before that the helper is asked what waits there: the
/// tab knows the waiting request by tool and input, so the person can decide about it.
#[test]
fn a_gap_ends_the_tab_and_lists_pending_requests() {
    let server = Server::new(&["EITRI_STANDIN_RING=8"]);
    let flaky = Flaky::new(&server);
    let provider = server
        .provider(flaky.clone(), Arc::new(FastClock::default()), None)
        .unwrap();
    let session = create(&provider, &server.project);
    let mut seen = Vec::new();
    pump_until(&provider, &mut seen, "the session", |seen| {
        seen.iter()
            .any(|event| matches!(event, AgentDomainEvent::SessionOpened { .. }))
    });
    assert_eq!(provider.remote_ending(), None);
    a_gap_with_a_waiting_request(&server, &flaky, &provider, &session);
    pump_until(&provider, &mut seen, "the loss", |seen| !unavailable(seen).is_empty());
    let lost = unavailable(&seen);
    assert_eq!(lost.len(), 1, "{lost:?}");
    assert!(lost[0].contains("journal no longer holds"), "{lost:?}");
    assert_eq!(
        provider.remote_ending(),
        Some(RemoteEnding::Gap {
            waiting: vec![WaitingRequest {
                permission_id: "p1".into(),
                tool_name: "Bash".into(),
                input_line: "true".into(),
            }]
        })
    );
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, AgentDomainEvent::PermissionRequested { .. })),
        "the request in the gap is not drawn as a card"
    );
}

/// "Deny them and stop the agent" sends each listed request the ordinary deny, then stops the sidecar.
/// Only what the helper listed may be denied this way.
#[test]
fn deny_and_stop_after_a_gap_denies_each_and_stops() {
    let server = Server::new(&["EITRI_STANDIN_RING=8", "--record-permissions {root}/permissions.txt"]);
    let flaky = Flaky::new(&server);
    let provider = Arc::new(
        server
            .provider(flaky.clone(), Arc::new(FastClock::default()), None)
            .unwrap(),
    );
    let conversation =
        AgentConversation::create_remote_with_tools(provider.clone(), &server.project, ProjectTrust::Untrusted, None)
            .unwrap();
    let session = conversation.session_id().unwrap().to_string();
    a_gap_with_a_waiting_request(&server, &flaky, &provider, &session);
    wait_for("the gap's ending", || conversation.remote_ending().is_some());
    let direct = server.direct();
    assert_eq!(server.alive(&direct), 1);

    let stray = conversation
        .deny_waiting_and_stop(vec!["p1".into(), "p9".into()])
        .recv_timeout(HANG)
        .unwrap();
    assert!(stray.unwrap_err().contains("p9"));
    assert_eq!(server.alive(&direct), 1, "a refused list stops nothing");

    let outcome = conversation
        .deny_waiting_and_stop(vec!["p1".into()])
        .recv_timeout(HANG)
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome,
        agent::DenyAndStopOutcome {
            stopped: true,
            failures: Vec::new()
        }
    );
    assert_eq!(
        std::fs::read_to_string(server.root.join("permissions.txt")).unwrap(),
        "p1 deny\n"
    );
    wait_for("the sidecar stopped", || server.alive(&direct) == 0);
}

/// Reattaching after the CLI gave up on a request (ten minutes, while the link was down): the replay
/// shows the CLI's denial and how long the request waited, and a late answer is refused as already
/// settled, which is benign.
#[test]
fn reattach_after_ten_minutes_shows_the_clis_denial() {
    let server = Server::new(&[]);
    let flaky = Flaky::new(&server);
    let clock = Arc::new(FastClock::default());
    let provider = server.provider(flaky.clone(), clock.clone(), None).unwrap();
    let session = create(&provider, &server.project);
    let mut seen = Vec::new();
    pump_until(&provider, &mut seen, "the session", |seen| {
        seen.iter()
            .any(|event| matches!(event, AgentDomainEvent::SessionOpened { .. }))
    });
    let gate = server.root.join("gate");
    std::fs::create_dir(&gate).unwrap();
    send(
        &provider,
        &session,
        &format!(
            "gate {}; ask-expire p1 599; text m1 after the expiry; end",
            gate.display()
        ),
    );
    wait_for("the stand-in at its gate", || gate.join("ready").exists());
    flaky.cut();
    let direct = server.direct();
    let head_before = journal_head(&direct);
    std::fs::write(gate.join("release"), b"").unwrap();
    wait_for("the turn journaled", || journal_head(&direct) >= head_before + 4);
    clock.advance(Duration::from_secs(601));
    flaky.restore();

    pump_until(&provider, &mut seen, "the turn's end", turn_completed);
    let notes: Vec<String> = seen
        .iter()
        .filter_map(|event| match event {
            AgentDomainEvent::SessionNotice { content, .. } => content.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(
        notes,
        ["the CLI denied this request after 10 min while no window was attached"],
        "{seen:?}"
    );
    let late = provider
        .resolve_permission(ResolvePermissionRequest {
            session_id: session,
            permission_id: "p1".into(),
            decision: PermissionDecision::Allow,
        })
        .unwrap_err();
    assert!(late.is_benign(), "{late:?}");
    assert!(unavailable(&seen).is_empty());
}

/// A reopened window folds the session afresh from the journal's start: an expiry the window that saved it had
/// already folded happened while it was attached, and is not shown as the CLI's denial; one after what that
/// window had folded is.
#[test]
fn a_restore_shows_the_clis_denial_only_past_what_the_saving_window_had_folded() {
    let server = Server::new(&[]);
    let provider = server
        .provider(Arc::new(server.launcher()), real_clock(), None)
        .unwrap();
    let session = create(&provider, &server.project);
    let mut seen = Vec::new();
    pump_until(&provider, &mut seen, "the session", |seen| {
        seen.iter()
            .any(|event| matches!(event, AgentDomainEvent::SessionOpened { .. }))
    });
    send(
        &provider,
        &session,
        "ask-expire p1 599; text m1 expired while watched; end",
    );
    pump_until(&provider, &mut seen, "the first turn", turn_completed);
    let saved = provider.helper_handoff().expect("where the agent is");
    let mut later = Vec::new();
    send(
        &provider,
        &session,
        "ask-expire p2 599; text m2 expired after the save; end",
    );
    pump_until(&provider, &mut later, "the second turn", turn_completed);
    let notes = |events: &[AgentDomainEvent]| -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                AgentDomainEvent::SessionNotice { content, .. } => content.clone(),
                _ => None,
            })
            .collect()
    };
    assert!(notes(&seen).is_empty() && notes(&later).is_empty(), "both seen live");
    drop(provider);

    let reopened = ClaudeSidecarProvider::reattach_through_helper_on(
        server.client(Arc::new(server.launcher())),
        &saved,
        real_clock(),
    )
    .unwrap();
    let mut replay = Vec::new();
    pump_until(&reopened, &mut replay, "both turns replayed", |seen| {
        seen.iter()
            .filter(|event| matches!(event, AgentDomainEvent::TurnCompleted { .. }))
            .count()
            == 2
    });
    assert_eq!(
        notes(&replay),
        ["the CLI denied this request after 10 min while no window was attached"],
        "only the expiry past the saved window's fold: {replay:?}"
    );
}

/// Closing a remote tab ends its agent on the server; a window letting go of it ends nothing there.
#[test]
fn closing_a_remote_tab_stops_its_sidecar() {
    let server = Server::new(&[]);
    let direct = server.direct();
    let open = || {
        let provider = server
            .provider(Arc::new(server.launcher()), real_clock(), None)
            .unwrap();
        AgentConversation::create_remote_with_tools(Arc::new(provider), &server.project, ProjectTrust::Untrusted, None)
            .unwrap()
    };
    let mut closed = open();
    let mut kept = open();
    assert_eq!(server.alive(&direct), 2);
    closed.shutdown();
    wait_for("the closed tab's sidecar stopped", || server.alive(&direct) == 1);
    kept.detach();
    drop(kept);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(server.alive(&direct), 1, "a window letting go stops nothing");
    let sessions: Vec<_> = direct
        .list()
        .unwrap()
        .into_iter()
        .filter(|sidecar| sidecar.alive)
        .flat_map(|sidecar| sidecar.sessions)
        .collect();
    assert_eq!(sessions.len(), 1);
    assert!(!sessions[0].closed, "its session goes on");
}

/// The other window's helper connections: the one it holds for this server's key.
struct Holding(HelperClient);

impl agent::remote_client::HelperConnections for Holding {
    fn open_for(&self, host: &str, host_key_sha256: &str) -> Option<HelperClient> {
        (host == HOST && host_key_sha256 == HOST_KEY).then(|| self.0.clone())
    }
}

/// Moving a remote tab to another window hands over the helper's sidecar id and where the watch stood, and no
/// descriptors; the taking window reaches the same sidecar over its own connection and goes on from there.
#[test]
fn moving_a_remote_tab_hands_the_sidecar_id() {
    use agent::chat_mover::{ChatMover, MovedKind};
    let server = Server::new(&[]);
    let source = server
        .provider(Arc::new(server.launcher()), real_clock(), None)
        .unwrap();
    let session = create(&source, &server.project);
    send(&source, &session, "text m1 before the move; end");
    let mut seen = Vec::new();
    pump_until(&source, &mut seen, "the first turn's end", turn_completed);
    let direct = server.direct();
    let listed = direct.list().unwrap();
    assert_eq!(listed.len(), 1);
    // The agent goes on by itself between the freeze and the commit: what it says then reaches the taker, once.
    let gate = server.root.join("move-gate");
    std::fs::create_dir(&gate).unwrap();
    send(
        &source,
        &session,
        &format!("gate {}; text m2 said between freeze and commit; end", gate.display()),
    );
    wait_for("the stand-in at its gate", || gate.join("ready").exists());
    seen.clear();
    pump_until(&source, &mut seen, "the turn's start", |seen| {
        seen.iter()
            .any(|event| matches!(event, AgentDomainEvent::TurnStarted { .. }))
    });

    let frozen = ChatMover::freeze_for_chat_move(&source).unwrap();
    let head_before = journal_head(&direct);
    std::fs::write(gate.join("release"), b"").unwrap();
    wait_for("the turn journaled after the freeze", || {
        journal_head(&direct) >= head_before + 2
    });
    assert!(frozen.fds.is_empty(), "a remote tab brings no descriptors");
    assert_eq!(MovedKind::Claude.descriptors_for(&frozen.record), 0);
    let helper = &frozen.record["transport"]["Helper"];
    assert_eq!(helper["sidecar_id"], listed[0].sidecar_id.as_str());
    assert_eq!(helper["session_id"], session.as_str());
    assert_eq!(helper["host_key_sha256"], HOST_KEY);
    let folded = helper["last_folded_sequence"].as_u64().unwrap();
    assert!(folded > 0);
    assert_eq!(frozen.record["folded_through"].as_u64(), Some(folded));

    let refused = agent::chat_mover::adopt(MovedKind::Claude, &frozen.record, Vec::new())
        .err()
        .expect("a window with no connection to the server refuses it");
    assert!(refused.contains(HOST), "{refused}");
    // A connection opened without the project's key carries no tab of it.
    let unpinned = Holding(server.direct());
    let refused = agent::chat_mover::adopt_with(MovedKind::Claude, &frozen.record, Vec::new(), &unpinned)
        .err()
        .expect("a connection without the key refuses it");
    assert!(refused.contains(HOST_KEY), "{refused}");
    let taking = Holding(server.client(Arc::new(server.launcher())));
    let mut stripped = frozen.record.clone();
    stripped.as_object_mut().unwrap().remove("remote_facts");
    let refused = agent::chat_mover::adopt_with(MovedKind::Claude, &stripped, Vec::new(), &taking)
        .err()
        .expect("a record without the agent's handshake is refused");
    assert!(refused.contains("handshake"), "{refused}");

    let adopted = agent::chat_mover::adopt_with(MovedKind::Claude, &frozen.record, Vec::new(), &taking).unwrap();
    let taker = adopted.agent;
    taker.mover().unwrap().commit_chat_move().unwrap();
    ChatMover::forget_chat_move(&source);
    drop(source);
    assert!(taker.link_state().is_some(), "the taken tab is remote");
    let mut moved = Vec::new();
    let started = Instant::now();
    while !turn_completed(&moved) {
        moved.extend(taker.pump());
        assert!(started.elapsed() < HANG, "the turn across the move never ended");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        text_of(&moved).matches("between freeze and commit").count(),
        1,
        "what came between the freeze and the commit is folded once"
    );
    assert!(!text_of(&moved).contains("before the move"), "nothing folded twice");
    taker
        .send_turn(SendTurnRequest {
            session_id: session.clone(),
            text: "text m1 after the move; end".into(),
            images: Vec::new(),
        })
        .unwrap();
    let mut after = Vec::new();
    let started = Instant::now();
    while !turn_completed(&after) {
        after.extend(taker.pump());
        assert!(started.elapsed() < HANG, "the turn after the move never ended");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(text_of(&after).contains("after the move"));
    assert!(!text_of(&after).contains("before the move"), "nothing folded twice");
    assert_eq!(server.alive(&direct), 1, "the same sidecar, still running");
}

/// A reopened window takes its saved remote tab up from the journal: the history is folded afresh from the
/// start, nothing is resumed, and the same agent takes the next turn.
#[test]
fn a_reopened_window_reattaches_its_remote_tabs_from_the_journal() {
    let server = Server::new(&["--record-create {root}/creates.bin", "--client-tools"]);
    let first = Arc::new(
        server
            .provider(Arc::new(server.launcher()), real_clock(), None)
            .unwrap(),
    );
    let mut conversation = AgentConversation::create_remote_with_tools(
        first.clone(),
        &server.project,
        ProjectTrust::Untrusted,
        Some(agent::ClientToolSet {
            server_name: "eitri".into(),
            timeout_ms: 900_000,
            tools: vec![agent::ClientToolDecl {
                name: "open_file".into(),
                description: "opens a file".into(),
                input_schema_json: "{}".into(),
                always_load: true,
            }],
        }),
    )
    .unwrap();
    conversation
        .send_turn("text m1 said before the window closed; end")
        .unwrap();
    wait_for("the first turn", || {
        let projection = conversation.projection();
        projection.active_turn_id.is_none()
            && projection
                .transcript
                .iter()
                .any(|message| message.text.contains("before the window closed"))
    });
    let saved = conversation
        .helper_handoff()
        .expect("a remote tab saves where its agent is");
    assert_eq!(saved.host, HOST);
    assert_eq!(
        saved.declared_client_tools,
        conversation.declared_client_tools().clone()
    );
    assert!(!saved.declared_client_tools.is_empty());
    conversation.detach();
    drop(conversation);
    drop(first);
    let creates = std::fs::read(server.root.join("creates.bin")).unwrap().len();

    let provider = ClaudeSidecarProvider::reattach_through_helper_on(
        server.client(Arc::new(server.launcher())),
        &saved,
        real_clock(),
    )
    .unwrap();
    let mut reopened =
        AgentConversation::reattach_remote(Arc::new(provider), &server.project, &saved.session_id).unwrap();
    assert_eq!(reopened.declared_client_tools(), &saved.declared_client_tools);
    wait_for("the history folded again", || {
        reopened
            .projection()
            .transcript
            .iter()
            .any(|message| message.text.contains("before the window closed"))
    });
    reopened.send_turn("text m1 and after it reopened; end").unwrap();
    wait_for("the next turn", || {
        reopened
            .projection()
            .transcript
            .iter()
            .any(|message| message.text.contains("after it reopened"))
    });
    assert_eq!(
        std::fs::read(server.root.join("creates.bin")).unwrap().len(),
        creates,
        "no session was created or resumed"
    );
    assert_eq!(server.alive(&server.direct()), 1);
}

impl Server {
    /// The server restarts: the daemon and every sidecar are gone, the journal is kept, and a new daemon
    /// starts.
    fn reboot(&self) {
        for pid in self.recorded_pids() {
            if let Some(fd) = pidfd(pid) {
                if exe_of(pid).is_some_and(|exe| exe == self.sidecar) {
                    signal_through(&fd, libc::SIGKILL);
                }
            }
        }
        self.kill_daemon();
        wait_for("the sidecars gone", || {
            !self
                .recorded_pids()
                .into_iter()
                .any(|pid| exe_of(pid).is_some_and(|exe| exe == self.sidecar))
        });
        self.ensure();
    }
}

/// After a server restart the helper no longer runs the tab's sidecar: the open tab keeps the history it
/// folded and ends with the agent gone, a reopened window's tab ends the same way, and the CLI's session is
/// continued through a new sidecar with the ordinary resume request.
#[test]
fn server_reboot_keeps_history_and_offers_resume() {
    let server = Server::new(&["--record-create {root}/creates.bin"]);
    let flaky = Flaky::new(&server);
    let provider = Arc::new(
        server
            .provider(flaky.clone(), Arc::new(FastClock::default()), None)
            .unwrap(),
    );
    let mut conversation =
        AgentConversation::create_remote_with_tools(provider.clone(), &server.project, ProjectTrust::Untrusted, None)
            .unwrap();
    conversation.send_turn("text m1 before the reboot; end").unwrap();
    wait_for("the turn", || {
        let projection = conversation.projection();
        projection.active_turn_id.is_none()
            && projection
                .transcript
                .iter()
                .any(|message| message.text.contains("before the reboot"))
    });
    let saved = conversation.helper_handoff().unwrap();
    let cli_session = conversation.provider_session_id().expect("the CLI's session id");

    flaky.cut();
    server.reboot();
    flaky.restore();
    wait_for("the open tab's end", || {
        matches!(
            conversation.projection().status,
            agent::ProjectionStatus::Unavailable { .. }
        )
    });
    assert_eq!(conversation.remote_ending(), Some(RemoteEnding::AgentGone));
    assert!(conversation
        .projection()
        .transcript
        .iter()
        .any(|message| message.text.contains("before the reboot")));

    let reopened_provider = ClaudeSidecarProvider::reattach_through_helper_on(
        server.client(Arc::new(server.launcher())),
        &saved,
        Arc::new(FastClock::default()),
    )
    .expect("a tab whose sidecar is gone is still taken up, to say so");
    let reopened =
        AgentConversation::reattach_remote(Arc::new(reopened_provider), &server.project, &saved.session_id).unwrap();
    wait_for("the reopened tab's history and its end", || {
        matches!(
            reopened.projection().status,
            agent::ProjectionStatus::Unavailable { .. }
        )
    });
    // The restarted helper serves no journal of a sidecar that died with the server, so a window that
    // reopens after the restart learns only that the agent is gone; its saved state names the CLI's session.
    assert_eq!(reopened.remote_ending(), Some(RemoteEnding::AgentGone));

    let creates_before = std::fs::read(server.root.join("creates.bin")).unwrap().len();
    let fresh = server
        .provider(Arc::new(server.launcher()), real_clock(), None)
        .unwrap();
    let mut resumed = AgentConversation::resume_remote_with_tools(
        Arc::new(fresh),
        &server.project,
        &cli_session,
        ProjectTrust::Untrusted,
        None,
    )
    .expect("the CLI's session continues through a new sidecar");
    assert!(
        std::fs::read(server.root.join("creates.bin")).unwrap().len() > creates_before,
        "the resume went out as a create request"
    );
    resumed.send_turn("text m1 after the resume; end").unwrap();
    wait_for("the resumed session's turn", || {
        let projection = resumed.projection();
        projection
            .transcript
            .iter()
            .any(|message| message.text.contains("after the resume"))
    });
    assert_eq!(resumed.provider_session_id().as_deref(), Some(cli_session.as_str()));
}

struct EnsuringLauncher(LocalExec);
impl ChannelLauncher for EnsuringLauncher {
    fn spawn(&self, tail: &[OsString]) -> std::io::Result<Child> {
        self.0.spawn(tail)
    }
    fn ensure_helper(&self, _: &str) -> std::io::Result<()> {
        let output = self
            .0
            .spawn(&["ensure".into(), "--args-hex".into(), OsString::new()])?
            .wait_with_output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        }
    }
}

fn kill_daemon(server: &Server) -> u32 {
    let info: serde_json::Value =
        serde_json::from_slice(&std::fs::read(server.run_dir().join("info")).unwrap()).unwrap();
    let pid = info["pid"].as_u64().unwrap() as u32;
    let fd = pidfd(pid).unwrap();
    assert_eq!(exe_of(pid), Some(helper_binary()));
    signal_through(&fd, libc::SIGKILL);
    // A reconnect can already have started a replacement holding the same lifetime lock.
    wait_for("the killed daemon exited", || {
        use std::os::fd::AsRawFd;
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd for the held pidfd, with no blocking wait.
        let ready = unsafe { libc::poll(&mut poll, 1, 0) };
        assert!(ready >= 0, "pidfd poll failed: {}", std::io::Error::last_os_error());
        ready > 0 && poll.revents & libc::POLLIN != 0
    });
    pid
}

#[test]
fn a_new_tab_restarts_a_daemon_that_went_away_while_the_window_was_idle() {
    let server = Server::new(&[]);
    let client = server.client(Arc::new(EnsuringLauncher(server.launcher())));
    kill_daemon(&server);
    let provider = ClaudeSidecarProvider::connect_through_helper(client, HOST_KEY, &server.start(None)).unwrap();
    let session = create(&provider, &server.project);
    send(&provider, &session, "text m1 after restart; end");
    let mut seen = Vec::new();
    pump_until(&provider, &mut seen, "the turn after restart", turn_completed);
    assert!(text_of(&seen).contains("after restart"));
}

#[test]
fn a_daemon_killed_during_a_turn_is_restarted_and_adopts_the_same_sidecar() {
    let server = Server::new(&[]);
    let clock = Arc::new(FastClock::default());
    let provider = server
        .provider(Arc::new(EnsuringLauncher(server.launcher())), clock, None)
        .unwrap();
    let session = create(&provider, &server.project);
    send(&provider, &session, "ask p; text m1 after restart; end");
    let mut seen = Vec::new();
    pump_until(&provider, &mut seen, "the permission before restart", |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. }))
    });
    let old_pid = kill_daemon(&server);
    wait_for("a replacement daemon published", || {
        std::fs::read(server.run_dir().join("info"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .is_some_and(|info| info["pid"].as_u64().is_some_and(|pid| pid != old_pid as u64))
    });
    wait_for("the adopted sidecar ready", || {
        provider.link_state() == Some(RemoteLinkState::Live)
    });
    provider
        .resolve_permission(ResolvePermissionRequest {
            session_id: session.clone(),
            permission_id: "p".into(),
            decision: PermissionDecision::Deny {
                reason: Some("continue after restart".into()),
            },
        })
        .unwrap();
    pump_until(
        &provider,
        &mut seen,
        "the turn continues after daemon restart",
        turn_completed,
    );
    assert!(unavailable(&seen).is_empty());
    assert!(text_of(&seen).contains("after restart"));
    assert_eq!(server.alive(&server.direct()), 1);
}

/// [`EnsuringLauncher`] whose restart, once armed, waits until `until` exists: a test holds the replacement daemon
/// back until the sidecar has written everything it was going to write.
struct HeldEnsure {
    inner: EnsuringLauncher,
    until: PathBuf,
    armed: AtomicBool,
}
impl ChannelLauncher for HeldEnsure {
    fn spawn(&self, tail: &[OsString]) -> std::io::Result<Child> {
        self.inner.spawn(tail)
    }
    fn ensure_helper(&self, daemon: &str) -> std::io::Result<()> {
        if self.armed.load(Ordering::SeqCst) {
            wait_for("the sidecar done writing", || self.until.exists());
        }
        self.inner.ensure_helper(daemon)
    }
}

/// The 2026-11-17 pass's crash, as a test: the daemon is killed while a turn streams with no permission waiting,
/// and no daemon runs while the sidecar writes the rest of the turn, its end included; the replacement re-adopts the
/// sidecar and the tab gets every chunk once, the turn's end and a next turn.
#[test]
fn a_daemon_killed_mid_stream_loses_and_repeats_nothing_and_the_next_turn_runs() {
    let server = Server::new(&[]);
    let done = server.root.join("written");
    let launcher = Arc::new(HeldEnsure {
        inner: EnsuringLauncher(server.launcher()),
        until: done.clone(),
        armed: AtomicBool::new(false),
    });
    let provider = server.provider(launcher.clone(), real_clock(), None).unwrap();
    let session = create(&provider, &server.project);
    let gate = server.root.join("gate");
    std::fs::create_dir(&gate).unwrap();
    send(
        &provider,
        &session,
        &format!(
            "text m1 drip-one ; gate {}; text m2 drip-two ; sleep 300; text m3 drip-three ; end; touch {}",
            gate.display(),
            done.display()
        ),
    );
    let mut seen = Vec::new();
    pump_until(&provider, &mut seen, "the first chunk", |events| {
        text_of(events).contains("drip-one")
    });
    wait_for("the stand-in at its gate", || gate.join("ready").exists());
    launcher.armed.store(true, Ordering::SeqCst);
    let old_pid = kill_daemon(&server);
    std::fs::write(gate.join("release"), b"").unwrap();
    pump_until(&provider, &mut seen, "the turn's end after the crash", turn_completed);
    assert!(done.exists(), "the turn ended while no daemon ran");
    let text = text_of(&seen);
    for chunk in ["drip-one", "drip-two", "drip-three"] {
        assert_eq!(text.matches(chunk).count(), 1, "{chunk} in {text:?}");
    }
    assert!(text.find("drip-one") < text.find("drip-two") && text.find("drip-two") < text.find("drip-three"));
    assert!(unavailable(&seen).is_empty(), "{:?}", unavailable(&seen));
    let info: serde_json::Value =
        serde_json::from_slice(&std::fs::read(server.run_dir().join("info")).unwrap()).unwrap();
    assert_ne!(
        info["pid"].as_u64(),
        Some(old_pid as u64),
        "a replacement daemon serves the tab"
    );
    send(&provider, &session, "text m4 next-turn ; end");
    let before = seen.len();
    pump_until(&provider, &mut seen, "the next turn", |events| {
        turn_completed(&events[before..]) && text_of(&events[before..]).contains("next-turn")
    });
    assert!(unavailable(&seen).is_empty());
    assert_eq!(server.alive(&server.direct()), 1, "the same sidecar, no second one");
}

struct Installation {
    command: PathBuf,
    name: String,
    run_key: String,
}
impl Installation {
    fn new(server: &Server, hash_digit: char) -> Self {
        let output = std::process::Command::new(helper_binary())
            .arg("--version")
            .output()
            .unwrap();
        let identity = String::from_utf8(output.stdout).unwrap();
        let revision = identity.split_whitespace().nth(2).unwrap();
        let name = format!(
            "{}-{}-{}-{}",
            server.version,
            revision,
            hash_digit.to_string().repeat(16),
            hash_digit.to_string().repeat(16)
        );
        let dir = server.root.join("d/eitri-server").join(&name);
        agent::private_fs::create_private_dir_all(&dir.join("bin"), &server.root).unwrap();
        agent::private_fs::create_private_dir_all(&dir.join("sidecar").join(SIDECAR_REVISION), &server.root).unwrap();
        sh(
            r#"cp "$1" "$2" && chmod 700 "$2""#,
            &[&helper_binary(), &dir.join("bin/eitri-server")],
        );
        sh(
            r#"cp "$1" "$2" && chmod 700 "$2""#,
            &[
                &server.sidecar,
                &dir.join("sidecar")
                    .join(SIDECAR_REVISION)
                    .join("verdandi-claude-sidecar"),
            ],
        );
        let command = server.root.join(format!("helper-{hash_digit}"));
        let original = std::fs::read_to_string(&server.command).unwrap();
        let script = original.replace(&quoted(&helper_binary()), &quoted(&dir.join("bin/eitri-server")));
        sh(
            r#"umask 077; printf '%s' "$1" > "$2"; chmod 700 "$2""#,
            &[Path::new(&script), &command],
        );
        let digest = format!(
            "{:x}",
            Sha256::digest(format!("eitri-server-installation\0{name}").as_bytes())
        );
        let installed = Self {
            command,
            name,
            run_key: format!("i-{}", &digest[..16]),
        };
        let child = LocalExec {
            helper: installed.command.clone(),
        }
        .spawn(&["ensure".into(), "--args-hex".into(), OsString::new()])
        .unwrap();
        assert!(child.wait_with_output().unwrap().status.success());
        installed
    }
    fn client(&self) -> HelperClient {
        HelperClient::connect_daemon(
            Arc::new(LocalExec {
                helper: self.command.clone(),
            }),
            HOST,
            Some(HOST_KEY),
            &self.name,
        )
        .unwrap()
    }
}
impl Drop for Installation {
    fn drop(&mut self) {
        let _ = LocalExec {
            helper: self.command.clone(),
        }
        .spawn(&[
            "stop-daemon".into(),
            "--args-hex".into(),
            agent::ssh::encode_args(&[std::ffi::OsStr::new(&self.run_key)]).into(),
        ])
        .and_then(|child| child.wait_with_output());
    }
}

#[test]
fn an_upgrade_reattaches_and_stops_sidecars_through_their_original_installation() {
    let server = Server::new(&[]);
    let old = Installation::new(&server, 'a');
    let new = Installation::new(&server, 'b');
    let original = old.client();
    let provider =
        ClaudeSidecarProvider::connect_through_helper(original.clone(), HOST_KEY, &server.start(None)).unwrap();
    let session = create(&provider, &server.project);
    send(&provider, &session, "text m1 before upgrade; end");
    let mut seen = Vec::new();
    pump_until(&provider, &mut seen, "the old installation's history", turn_completed);
    let saved = provider.helper_handoff().unwrap();
    assert_eq!(saved.daemon, old.name);
    let current = new.client();
    assert!(current.list().unwrap().is_empty());
    let reopened = ClaudeSidecarProvider::reattach_through_helper(current.clone(), &saved).unwrap();
    let mut replayed = Vec::new();
    pump_until(
        &reopened,
        &mut replayed,
        "the old daemon's journal after upgrade",
        turn_completed,
    );
    assert!(text_of(&replayed).contains("before upgrade"));
    assert_eq!(original.list().unwrap().iter().filter(|sc| sc.alive).count(), 1);
    let (daemon, sidecar) = saved
        .stop_key()
        .split_once('/')
        .map(|(d, s)| (d.to_owned(), s.to_owned()))
        .unwrap();
    current.for_daemon(&daemon).unwrap().stop_if_known(&sidecar).unwrap();
    assert!(original.list().unwrap().iter().all(|sc| !sc.alive));
}

#[test]
fn an_ownerless_upgrade_handoff_and_stop_find_the_older_installation() {
    let server = Server::new(&[]);
    let old = Installation::new(&server, 'a');
    let new = Installation::new(&server, 'b');
    let original = old.client();
    let provider =
        ClaudeSidecarProvider::connect_through_helper(original.clone(), HOST_KEY, &server.start(None)).unwrap();
    let session = create(&provider, &server.project);
    send(&provider, &session, "text m1 before upgrade; end");
    let mut seen = Vec::new();
    pump_until(&provider, &mut seen, "the old installation's history", turn_completed);
    let mut saved = provider.helper_handoff().unwrap();
    assert_eq!(saved.daemon, old.name);
    saved.daemon.clear();
    let current = new.client().with_installations([new.name.clone(), old.name.clone()]);
    assert!(current.list().unwrap().is_empty());
    let reopened = ClaudeSidecarProvider::reattach_through_helper(current.clone(), &saved).unwrap();
    let mut replayed = Vec::new();
    pump_until(
        &reopened,
        &mut replayed,
        "the old daemon's journal after upgrade",
        turn_completed,
    );
    assert!(text_of(&replayed).contains("before upgrade"));
    assert_eq!(original.list().unwrap().iter().filter(|sc| sc.alive).count(), 1);
    assert_eq!(reopened.helper_handoff().unwrap().daemon, old.name);
    assert_eq!(reopened.remote_ending(), None);
    current.stop_saved("", &saved.sidecar_id).unwrap();
    assert!(original.list().unwrap().iter().all(|sc| !sc.alive));
}

#[test]
fn ownerless_records_are_gone_only_after_every_installation_answers() {
    let server = Server::new(&[]);
    let old = Installation::new(&server, 'a');
    let new = Installation::new(&server, 'b');
    let client = new.client().with_installations([new.name.clone(), old.name.clone()]);
    let saved = agent::HelperHandoff {
        host: HOST.into(),
        host_key_sha256: HOST_KEY.into(),
        daemon: String::new(),
        sidecar_id: "gone".into(),
        session_id: "gone-session".into(),
        last_folded_sequence: 0,
        declared_client_tools: Default::default(),
    };
    let provider = ClaudeSidecarProvider::reattach_through_helper(client.clone(), &saved).unwrap();
    let mut events = Vec::new();
    pump_until(&provider, &mut events, "definite loss across installations", |events| {
        !unavailable(events).is_empty()
    });
    assert_eq!(provider.remote_ending(), Some(RemoteEnding::AgentGone));
    client.stop_saved("", &saved.sidecar_id).unwrap();
    // A forgotten owner after a data-directory change uses that same exhaustive decision.
    client.stop_saved("forgotten-installation", &saved.sidecar_id).unwrap();
}
