use std::collections::BTreeMap;
use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, Weak,
};
use std::time::Duration;

use agent::private_fs;
use claude_runtime_protocol::v1::{runtime_service_client::RuntimeServiceClient, HandshakeRequest};
use eitri_server_proto::{ListSidecarsResponse, LoginEnvState, SidecarInfo, StartSidecarRequest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::process::Child;
use tonic::Status;

use super::login_env::{self, Captured};
use crate::run_dir::Paths;

pub const STARTSIDECAR_ENV_ALLOWED: &[&str] = &[];
const SIDECAR_BIND_BUDGET: Duration = Duration::from_secs(15);
const STOP_GRACE: Duration = Duration::from_secs(6);
const KILL_BUDGET: Duration = Duration::from_secs(2);
const LOG_CAP: u64 = 8 * 1024 * 1024;
const LOG_TAIL: u64 = 1024 * 1024;
const RETENTION_MS: u64 = 7 * 24 * 3600 * 1000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub pid: u32,
    pub start_time: u64,
    pub socket: PathBuf,
    #[serde(default)]
    pub sidecar_id: String,
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub account: String,
    #[serde(default)]
    pub command_id: String,
    #[serde(default)]
    pub started_unix_ms: u64,
    #[serde(default)]
    pub ready: bool,
    #[serde(default)]
    pub failure: Option<String>,
    #[serde(default)]
    pub exited_unix_ms: Option<u64>,
}

struct Entry {
    record: Record,
    pidfd: Option<Arc<OwnedFd>>,
    child: Option<Child>,
    handshake: bool,
}

type SharedEntry = Arc<Mutex<Entry>>;
type EnvResult = Option<Result<Arc<Captured>, String>>;

pub struct Sidecars {
    paths: Paths,
    env: tokio::sync::watch::Sender<EnvResult>,
    env_retry: tokio::sync::Mutex<()>,
    entries: Mutex<BTreeMap<String, SharedEntry>>,
    starts: Mutex<BTreeMap<String, Weak<tokio::sync::Mutex<()>>>>,
    shutting_down: AtomicBool,
}

fn internal(error: impl std::fmt::Display) -> Status {
    Status::internal(error.to_string())
}

impl Sidecars {
    pub fn new(paths: Paths) -> io::Result<Self> {
        let mut entries = BTreeMap::new();
        for record in read_records(&paths) {
            let pidfd = pin_record(&record).ok().map(Arc::new);
            if pidfd.is_some() {
                let _ = crate::audit::record(
                    &paths.state,
                    "sidecar-adopt",
                    &format!("id={} pid={}", record.sidecar_id, record.pid),
                );
            }
            entries.insert(
                record.sidecar_id.clone(),
                Arc::new(Mutex::new(Entry {
                    record,
                    pidfd,
                    child: None,
                    handshake: false,
                })),
            );
        }
        let (env, _) = tokio::sync::watch::channel(None);
        Ok(Self {
            paths,
            env,
            env_retry: tokio::sync::Mutex::new(()),
            entries: Mutex::new(entries),
            starts: Mutex::new(BTreeMap::new()),
            shutting_down: AtomicBool::new(false),
        })
    }

    #[cfg(test)]
    pub(super) fn test_adopted_self(&self, id: &str, handshake: bool) {
        let pid = std::process::id();
        let record = Record {
            pid,
            start_time: start_time(pid).unwrap(),
            socket: PathBuf::new(),
            sidecar_id: id.into(),
            root: "/srv/project".into(),
            account: String::new(),
            command_id: "adopted".into(),
            started_unix_ms: 0,
            ready: true,
            failure: None,
            exited_unix_ms: None,
        };
        self.entries.lock().unwrap().insert(
            id.into(),
            Arc::new(Mutex::new(Entry {
                record,
                pidfd: Some(Arc::new(pin(pid).unwrap())),
                child: None,
                handshake,
            })),
        );
    }

    pub fn set_environment(&self, captured: io::Result<Captured>) {
        let captured = captured
            .map(|mut captured| {
                if let Some(reason) = &captured.fallback {
                    let _ = crate::audit::record(&self.paths.state, "login-env-fallback", reason);
                }
                login_env::absolute_path(&mut captured.env);
                Arc::new(captured)
            })
            .map_err(|e| {
                let reason = e.to_string();
                let _ = crate::audit::record(&self.paths.state, "login-env-unavailable", &reason);
                reason
            });
        self.env.send_replace(Some(captured));
    }

    pub fn warnings(&self) -> Vec<String> {
        match self.env.borrow().as_ref() {
            Some(Ok(captured)) => captured.fallback.iter().cloned().collect(),
            _ => Vec::new(),
        }
    }

    pub fn login_env_state(&self) -> LoginEnvState {
        match self.env.borrow().as_ref() {
            Some(Ok(captured)) if captured.fallback.is_some() => LoginEnvState::FallbackMinimal,
            Some(Ok(_)) => LoginEnvState::Captured,
            Some(Err(_)) => LoginEnvState::Unavailable,
            None => LoginEnvState::Capturing,
        }
    }

    async fn environment(&self) -> Result<Arc<Captured>, Status> {
        self.environment_with(login_env::capture).await
    }

    /// The login environment sidecars start in (captured once, retried after a failure), for other children the
    /// daemon runs as the account.
    pub(crate) async fn login_environment(&self) -> Result<Arc<Captured>, Status> {
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(Status::unavailable("daemon is stopping"));
        }
        self.environment().await
    }

    async fn environment_with<F, Fut>(&self, capture: F) -> Result<Arc<Captured>, Status>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = io::Result<Captured>>,
    {
        let mut env = self.env.subscribe();
        let initial = env.borrow().clone();
        match initial {
            Some(Ok(captured)) => return Ok(captured),
            None => {
                // A caller already waiting for capture gets that attempt's result. Only a later
                // start retries a failed lookup, keeping each request within one capture budget.
                return env
                    .wait_for(|value| value.is_some())
                    .await
                    .map_err(internal)?
                    .as_ref()
                    .expect("environment ready")
                    .clone()
                    .map_err(Status::unavailable);
            }
            Some(Err(_)) => {}
        }
        let _retry = self.env_retry.lock().await;
        let current = self.env.borrow().clone();
        if let Some(Ok(captured)) = current {
            return Ok(captured);
        }
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(Status::unavailable("daemon is stopping"));
        }
        self.env.send_replace(None);
        self.set_environment(capture().await);
        self.env
            .borrow()
            .as_ref()
            .expect("capture completed")
            .clone()
            .map_err(Status::unavailable)
    }

    fn snapshot(&self) -> Vec<SharedEntry> {
        self.entries
            .lock()
            .expect("sidecar registry")
            .values()
            .cloned()
            .collect()
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

    pub async fn start(&self, request: StartSidecarRequest) -> Result<String, Status> {
        self.start_with(request, |_| {}).await
    }

    async fn start_with(&self, request: StartSidecarRequest, after_spawn: impl FnOnce(u32)) -> Result<String, Status> {
        validate_request(&request)?;
        let root = open_root(&request.root)?;
        let canonical_root = canonical_root(&root)?;
        let serial = self.start_lock(&request.command_id);
        let _serial = serial.lock().await;
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(Status::unavailable("daemon is stopping"));
        }
        let captured = self.environment().await?;
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(Status::unavailable("daemon is stopping"));
        }
        if let Some(id) = self.recover_start(&request.command_id).await? {
            return Ok(id);
        }
        let binary = self
            .paths
            .install
            .as_ref()
            .ok_or_else(|| Status::failed_precondition("the helper does not run from an installation"))?
            .join("sidecar")
            .join(&request.sidecar_revision)
            .join("verdandi-claude-sidecar");
        let expected = request.sidecar_sha256.clone();
        let executable = tokio::task::spawn_blocking(move || checked_binary(&binary, &expected))
            .await
            .map_err(internal)?
            .map_err(|_| Status::failed_precondition("sidecar changed since install"))?;
        let id = self.new_id().map_err(internal)?;
        let socket = agent::socket_path::in_dir(&self.paths.run, &format!("sc-{id}.sock")).map_err(internal)?;
        let located = {
            let captured = captured.clone();
            tokio::task::spawn_blocking(move || agent::locate_claude_in(&|k| captured.env.get(OsStr::new(k)).cloned()))
                .await
                .map_err(internal)?
        };
        let cli_found = match &located {
            agent::ClaudeCli::Installed(path) => Some(path.as_path()),
            _ => None,
        };
        let policy = agent::sidecar_env_policy(
            &|k| captured.env.get(OsStr::new(k)).cloned(),
            (!request.account.is_empty()).then_some(request.account.as_str()),
            false,
            cli_found,
        );
        // Keep the verified executable and project inode open until the child has exec'd.
        let mut command = std::process::Command::new(format!("/proc/self/fd/{}", executable.as_raw_fd()));
        command.arg0("verdandi-claude-sidecar").env_clear().envs(&captured.env);
        agent::apply_sidecar_env(&mut command, &policy);
        command
            .env("VERDANDI_CLAUDE_SIDECAR_SOCKET", &socket)
            .env("VERDANDI_CLAUDE_SIDECAR_PARENT_WATCH", "none")
            .stdin(Stdio::null())
            .stdout(
                private_fs::append_private(&self.paths.metadata_run().join(format!("sc-{id}.out")))
                    .map_err(internal)?,
            )
            .stderr(
                private_fs::append_private(&self.paths.metadata_run().join(format!("sc-{id}.err")))
                    .map_err(internal)?,
            );
        let root_fd = root.as_raw_fd();
        let umask = crate::run_dir::inherited_umask();
        unsafe {
            command.pre_exec(move || child_setup(root_fd, umask));
        }
        // Shutdown and publication share this short lock; neither can miss a newly spawned child.
        let entry = {
            let mut entries = self.entries.lock().expect("sidecar registry");
            if self.shutting_down.load(Ordering::SeqCst) {
                return Err(Status::unavailable("daemon is stopping"));
            }
            let mut starting = Starting(Some(tokio::process::Command::from(command).spawn().map_err(internal)?));
            let pid = starting.0.as_ref().and_then(Child::id).expect("new child pid");
            let pidfd = pin(pid).map_err(internal)?;
            after_spawn(pid);
            let record = Record {
                pid,
                // This owned child has not been reaped, so even an exited leader still reserves its identity.
                // Readiness reports an early exit with the same refusal regardless of when it happened.
                start_time: identity_start_time(pid).map_err(internal)?,
                socket: socket.clone(),
                sidecar_id: id.clone(),
                root: canonical_root,
                account: request.account,
                command_id: request.command_id,
                started_unix_ms: crate::run_dir::unix_ms(),
                ready: false,
                failure: None,
                exited_unix_ms: None,
            };
            self.persist(&record).map_err(internal)?;
            crate::audit::record(&self.paths.state, "sidecar-start", &format!("id={id} pid={pid}"))
                .map_err(internal)?;
            let entry = Arc::new(Mutex::new(Entry {
                record,
                pidfd: Some(Arc::new(pidfd)),
                child: starting.0.take(),
                handshake: false,
            }));
            entries.insert(id.clone(), entry.clone());
            entry
        };
        let pid = entry.lock().expect("sidecar entry").record.pid;
        let readiness = tokio::time::timeout(SIDECAR_BIND_BUDGET, async {
            loop {
                if self.shutting_down.load(Ordering::SeqCst) {
                    return Err(io::Error::other("daemon is stopping"));
                }
                {
                    let mut e = entry.lock().expect("sidecar entry");
                    e.reap_exited()?;
                    if !e.alive() {
                        return Err(io::Error::other("sidecar exited before binding"));
                    }
                }
                if handshake(&socket, pid).await.is_ok() {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "sidecar bind timed out")));
        self.finish_start(&entry, readiness).await
    }

    async fn fail_start(&self, entry: &SharedEntry, error: Status) -> Status {
        {
            let mut e = entry.lock().expect("sidecar entry");
            e.record.ready = false;
            e.record.failure = Some(error.message().to_owned());
        }
        // Cleanup must precede any fallible record publication.
        let stopped = stop_entry(entry, &self.paths.state).await;
        let e = entry.lock().expect("sidecar entry");
        let _ = self.persist(&e.record);
        let _ = crate::audit::record(
            &self.paths.state,
            "sidecar-start-failed",
            &format!("id={} {error}", e.record.sidecar_id),
        );
        match stopped {
            Ok(()) => error,
            Err(stop_error) => internal(format!("{error}; could not stop sidecar: {stop_error}")),
        }
    }

    async fn finish_start(&self, entry: &SharedEntry, readiness: io::Result<()>) -> Result<String, Status> {
        if let Err(error) = readiness {
            return Err(self.fail_start(entry, Status::unavailable(error.to_string())).await);
        }
        let result = {
            let mut e = entry.lock().expect("sidecar entry");
            // A successful handshake must not turn a concurrent explicit stop into a successful start.
            if self.shutting_down.load(Ordering::SeqCst) || !e.alive() {
                Err(Status::unavailable("sidecar stopped during startup"))
            } else {
                e.record.ready = true;
                e.handshake = true;
                self.persist(&e.record)
                    .map_err(internal)
                    .map(|_| e.record.sidecar_id.clone())
            }
        };
        match result {
            Ok(id) => Ok(id),
            Err(error) => Err(self.fail_start(entry, error).await),
        }
    }

    async fn recover_start(&self, command_id: &str) -> Result<Option<String>, Status> {
        for entry in self.snapshot() {
            let candidate = {
                let e = entry.lock().expect("sidecar entry");
                if e.record.command_id != command_id {
                    continue;
                }
                if e.record.ready && e.record.failure.is_none() {
                    return Ok(Some(e.record.sidecar_id.clone()));
                }
                if e.alive() && e.record.failure.is_none() {
                    Some((e.record.socket.clone(), e.record.pid))
                } else {
                    None
                }
            };
            if let Some((socket, pid)) = candidate {
                if handshake(&socket, pid).await.is_ok() {
                    return self.finish_start(&entry, Ok(())).await.map(Some);
                }
            }
            // A retry cannot leave the previous attempt running, even if it never became ready.
            stop_entry(&entry, &self.paths.state).await.map_err(internal)?;
            let mut e = entry.lock().expect("sidecar entry");
            e.record
                .failure
                .get_or_insert_with(|| "startup interrupted before readiness".into());
            self.persist(&e.record).map_err(internal)?;
        }
        Ok(None)
    }

    fn new_id(&self) -> io::Result<String> {
        loop {
            let mut bytes = [0u8; 8];
            File::open("/dev/urandom")?.read_exact(&mut bytes)?;
            let id = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
            if !self.entries.lock().expect("sidecar registry").contains_key(&id)
                && !self.paths.metadata_run().join(format!("sc-{id}.pid")).exists()
            {
                return Ok(id);
            }
        }
    }

    fn persist(&self, record: &Record) -> io::Result<()> {
        private_fs::publish_private_file(
            &private_fs::open_dir(&self.paths.metadata_run())?,
            OsStr::new(&format!("sc-{}.pid", record.sidecar_id)),
            &serde_json::to_vec(record).map_err(io::Error::other)?,
        )?;
        Ok(())
    }

    pub fn list(&self) -> ListSidecarsResponse {
        let sidecars = self
            .snapshot()
            .iter()
            .map(|entry| {
                let mut entry = entry.lock().expect("sidecar entry");
                let _ = entry.reap_exited();
                SidecarInfo {
                    sidecar_id: entry.record.sidecar_id.clone(),
                    root: entry.record.root.clone(),
                    account: entry.record.account.clone(),
                    pid: entry.record.pid,
                    started_unix_ms: entry.record.started_unix_ms,
                    alive: entry.alive(),
                    handshake: entry.handshake && entry.alive(),
                    sessions: Vec::new(),
                    helper_version: self.paths.version.clone(),
                }
            })
            .collect();
        ListSidecarsResponse {
            sidecars,
            warnings: self.warnings(),
            login_env: self.login_env_state() as i32,
        }
    }

    pub fn any_alive(&self) -> bool {
        self.list().sidecars.iter().any(|s| s.alive)
    }

    pub async fn stop(&self, id: &str) -> Result<(), Status> {
        let entry = self
            .entries
            .lock()
            .expect("sidecar registry")
            .get(id)
            .cloned()
            .ok_or_else(|| Status::not_found("unknown sidecar_id"))?;
        stop_entry(&entry, &self.paths.state).await.map_err(internal)?;
        self.observe_exits();
        Ok(())
    }

    pub async fn stop_all(&self) -> io::Result<()> {
        let entries = {
            let entries = self.entries.lock().expect("sidecar registry");
            self.shutting_down.store(true, Ordering::SeqCst);
            let pending_environment = self.env.borrow().is_none();
            if pending_environment {
                self.env.send_replace(Some(Err("daemon is stopping".into())));
            }
            entries.values().cloned().collect::<Vec<_>>()
        };
        let results =
            futures_util::future::join_all(entries.iter().map(|entry| stop_entry(entry, &self.paths.state))).await;
        self.observe_exits();
        results.into_iter().collect::<io::Result<Vec<_>>>().map(|_| ())
    }

    pub fn observe_exits(&self) {
        let now = crate::run_dir::unix_ms();
        let mut expired = Vec::new();
        for entry in self.snapshot() {
            let mut entry = entry.lock().expect("sidecar entry");
            let _ = entry.reap_exited();
            if entry.alive() {
                continue;
            }
            if entry.record.exited_unix_ms.is_none() {
                entry.record.exited_unix_ms = Some(now);
                let _ = self.persist(&entry.record);
                let _ = crate::audit::record(
                    &self.paths.state,
                    "sidecar-stop",
                    &format!("id={} pid={}", entry.record.sidecar_id, entry.record.pid),
                );
            }
            if now.saturating_sub(entry.record.exited_unix_ms.unwrap_or(now)) >= RETENTION_MS {
                expired.push(entry.record.sidecar_id.clone());
            }
        }
        for id in expired {
            let result = (|| -> io::Result<()> {
                for extension in ["out", "err", "out.1", "err.1", "pid"] {
                    match std::fs::remove_file(self.paths.metadata_run().join(format!("sc-{id}.{extension}"))) {
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e),
                    }
                }
                let socket = agent::socket_path::in_dir(&self.paths.run, &format!("sc-{id}.sock"))?;
                match std::fs::remove_file(socket) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
                Ok(())
            })();
            match result {
                Ok(()) => {
                    self.entries.lock().expect("sidecar registry").remove(&id);
                    let _ = crate::audit::record(&self.paths.state, "sidecar-expire", &format!("id={id}"));
                }
                Err(e) => {
                    let _ = crate::audit::record(&self.paths.state, "sidecar-expire-error", &format!("id={id} {e}"));
                }
            }
        }
    }

    pub async fn refresh_handshakes(&self) {
        futures_util::future::join_all(self.snapshot().into_iter().map(|entry| async move {
            let command_id = entry.lock().expect("sidecar entry").record.command_id.clone();
            // Maintenance and retries must not stop or claim the same interrupted start concurrently.
            let serial = self.start_lock(&command_id);
            let _serial = serial.lock().await;
            let (socket, pid, interrupted, alive) = {
                let e = entry.lock().expect("sidecar entry");
                let interrupted = e.child.is_none() && !e.record.ready && e.record.failure.is_none();
                if e.handshake && !interrupted {
                    return;
                }
                (e.record.socket.clone(), e.record.pid, interrupted, e.alive())
            };
            if alive {
                for _ in 0..3 {
                    if handshake(&socket, pid).await.is_ok() {
                        if interrupted {
                            let _ = self.finish_start(&entry, Ok(())).await;
                        } else {
                            entry.lock().expect("sidecar entry").handshake = true;
                        }
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            let expired = {
                let e = entry.lock().expect("sidecar entry");
                interrupted
                    && crate::run_dir::unix_ms().saturating_sub(e.record.started_unix_ms)
                        >= SIDECAR_BIND_BUDGET.as_millis() as u64
            };
            if expired {
                let _ = self
                    .fail_start(&entry, Status::unavailable("startup interrupted before readiness"))
                    .await;
            }
        }))
        .await;
    }

    pub fn trim_logs(&self) {
        for entry in self.snapshot() {
            let id = entry.lock().expect("sidecar entry").record.sidecar_id.clone();
            for extension in ["out", "err"] {
                let path = self.paths.metadata_run().join(format!("sc-{id}.{extension}"));
                if let Err(error) = trim_log(&path) {
                    let _ = crate::audit::record(
                        &self.paths.state,
                        "sidecar-log-trim-error",
                        &format!("id={id} file={extension} {error}"),
                    );
                }
            }
        }
    }
}

fn validate_token(value: &str, field: &str) -> Result<(), Status> {
    if value.is_empty()
        || value.len() > 128
        || !value.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
    {
        return Err(Status::invalid_argument(format!(
            "invalid {field}: expected 1..128 safe ASCII bytes"
        )));
    }
    Ok(())
}

pub fn validate_command_id(value: &str) -> Result<(), Status> {
    validate_token(value, "command_id")
}

fn validate_request(request: &StartSidecarRequest) -> Result<(), Status> {
    validate_command_id(&request.command_id)?;
    if request.root.len() > 4096 {
        return Err(Status::invalid_argument("root exceeds byte cap"));
    }
    if !request.account.is_empty() {
        if request.account.len() > 128 {
            return Err(Status::invalid_argument("account exceeds byte cap"));
        }
        let account = agent::ClaudeAccount::resolve_from(&request.account, |key| (key == "HOME").then(|| "/".into()))
            .map_err(|_| Status::invalid_argument("invalid account name"))?;
        if account.name() != request.account {
            return Err(Status::invalid_argument("account must be normal form"));
        }
    }
    crate::run_dir::validate_version(&request.sidecar_revision)
        .map_err(|_| Status::invalid_argument("invalid sidecar_revision"))?;
    if request.sidecar_sha256.len() != 64
        || !request
            .sidecar_sha256
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(Status::invalid_argument("invalid sidecar_sha256"));
    }
    if let Some(key) = request
        .env
        .keys()
        .find(|k| !STARTSIDECAR_ENV_ALLOWED.contains(&k.as_str()))
    {
        return Err(Status::invalid_argument(format!(
            "environment key {key} is not allowed"
        )));
    }
    Ok(())
}

/// Whether an absolute `root` is in normal form: no empty, `.` or `..` component, so no doubled or trailing slash.
pub(crate) fn in_normal_form(root: &[u8]) -> bool {
    root == b"/"
        || root
            .split(|byte| *byte == b'/')
            .skip(1)
            .all(|part| !part.is_empty() && part != b"." && part != b"..")
}

pub(crate) fn open_root(root: &str) -> Result<File, Status> {
    if !root.starts_with('/') {
        return Err(Status::invalid_argument("root must be absolute"));
    }
    if !in_normal_form(root.as_bytes()) {
        return Err(Status::invalid_argument("root must be in normal form"));
    }
    let mut dir = private_fs::open_dir(Path::new("/")).map_err(internal)?;
    for part in root.split('/').skip(1).filter(|s| !s.is_empty()) {
        let name = CString::new(part).map_err(|_| Status::invalid_argument("invalid root"))?;
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(Status::invalid_argument("root must be an existing directory"));
        }
        dir = unsafe { File::from_raw_fd(fd) };
    }
    if dir.metadata().map_err(internal)?.uid() != private_fs::euid() {
        return Err(Status::invalid_argument("root must be owned by the daemon's uid"));
    }
    Ok(dir)
}

/// The path the kernel gives the directory `root` holds, resolved through every link: what a sidecar started in
/// it records as its root.
pub(crate) fn canonical_root(root: &File) -> Result<String, Status> {
    std::fs::read_link(format!("/proc/self/fd/{}", root.as_raw_fd()))
        .map_err(internal)?
        .into_os_string()
        .into_string()
        .map_err(|_| Status::invalid_argument("canonical root must be UTF-8"))
}

unsafe fn child_setup(root_fd: i32, umask: libc::mode_t) -> io::Result<()> {
    if unsafe { libc::setsid() < 0 || libc::fchdir(root_fd) < 0 } {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        libc::umask(umask);
    }
    Ok(())
}

struct Starting(Option<Child>);
impl Drop for Starting {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            if let Some(pid) = child.id() {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            let _ = child.start_kill();
        }
    }
}

impl Entry {
    fn reap_exited(&mut self) -> io::Result<()> {
        if let Some(child) = &mut self.child {
            if self.pidfd.as_ref().is_some_and(|fd| exited(fd).unwrap_or(false)) {
                // An unreaped owned leader reserves the group's identity until descendants are ended.
                if child.id() == Some(self.record.pid) {
                    unsafe {
                        libc::kill(-(self.record.pid as i32), libc::SIGKILL);
                    }
                }
                let _ = child.try_wait()?;
            }
        }
        Ok(())
    }

    fn alive(&self) -> bool {
        self.pidfd.as_ref().is_some_and(|fd| !exited(fd).unwrap_or(false))
    }
}

fn pin_record(record: &Record) -> io::Result<OwnedFd> {
    let fd = pin(record.pid)?;
    if exited(&fd)? || start_time(record.pid)? != record.start_time {
        return Err(io::Error::other("sidecar identity changed"));
    }
    Ok(fd)
}

async fn stop_entry(entry: &SharedEntry, state: &Path) -> io::Result<()> {
    stop_entry_with(entry, state, pin_session_members, signal).await
}

async fn stop_entry_with(
    entry: &SharedEntry,
    state: &Path,
    scan: impl FnOnce(&OwnedFd, u32, u64, &Path) -> io::Result<Vec<OwnedFd>> + Send + 'static,
    send: impl Fn(&OwnedFd, i32) -> io::Result<()>,
) -> io::Result<()> {
    let target = {
        let e = entry.lock().expect("sidecar entry");
        // A dead adopted leader no longer reserves its session id. Its leftover children are
        // deliberately not hunted. Owned children retain their group identity until reaping.
        e.pidfd
            .as_ref()
            .filter(|fd| !exited(fd).unwrap_or(false))
            .map(|fd| (fd.clone(), e.record.pid, e.record.start_time, e.child.is_none()))
    };
    if let Some((fd, pid, start, adopted)) = target {
        let mut members = if adopted {
            let leader = fd.clone();
            let scan_state = state.to_owned();
            match tokio::task::spawn_blocking(move || scan(&leader, pid, start, &scan_state))
                .await
                .map_err(io::Error::other)
                .and_then(|result| result)
            {
                Ok(members) => members,
                Err(error) => {
                    audit_stop(state, pid, "scan", &error);
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        for sig in [libc::SIGTERM, libc::SIGKILL] {
            // A matching, unreaped leader still reserves the group's identity after exit.
            if identity_start_time(pid).ok() == Some(start) {
                let result = unsafe { libc::kill(-(pid as i32), sig) };
                if result < 0 {
                    audit_stop(state, pid, "group-signal", &io::Error::last_os_error());
                }
            }
            if let Err(error) = send(&fd, sig) {
                audit_stop(state, pid, "leader-signal", &error);
            }
            members.retain(|member| match send(member, sig) {
                Ok(()) => true,
                Err(error) => {
                    // An unsignallable member must never prevent escalation to the other targets.
                    audit_stop(state, pid, "member-signal", &error);
                    false
                }
            });
            if sig == libc::SIGTERM {
                if let Ok(Err(error)) = tokio::time::timeout(STOP_GRACE, wait_pidfd(&fd)).await {
                    audit_stop(state, pid, "grace-wait", &error);
                }
            }
        }
        tokio::time::timeout(KILL_BUDGET, async {
            wait_pidfd(&fd).await?;
            for member in &members {
                wait_pidfd(member).await?;
            }
            Ok::<_, io::Error>(())
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "sidecar did not exit after SIGKILL"))??;
    }
    let mut e = entry.lock().expect("sidecar entry");
    e.reap_exited()?;
    if let Some(child) = &mut e.child {
        if child.try_wait()?.is_none() {
            return Err(io::Error::other("sidecar has not been reaped yet"));
        }
    }
    e.child = None;
    e.pidfd = None;
    e.handshake = false;
    Ok(())
}

fn audit_stop(state: &Path, pid: u32, operation: &str, error: &impl std::fmt::Display) {
    let _ = crate::audit::record(state, "sidecar-stop-error", &format!("pid={pid} {operation}: {error}"));
}

fn pin_session_members(leader_fd: &OwnedFd, leader: u32, leader_start: u64, state: &Path) -> io::Result<Vec<OwnedFd>> {
    let mut members = Vec::new();
    if exited(leader_fd)? {
        return Ok(members);
    }
    for item in std::fs::read_dir("/proc")? {
        if exited(leader_fd)? {
            break;
        }
        let item = item?;
        let Some(pid) = item.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        if pid == leader {
            continue;
        }
        let read_identity = || -> io::Result<(u32, u64)> {
            let stat = std::fs::read(item.path().join("stat"))?;
            let fields = stat_fields(&stat)?;
            let session = fields
                .get(3)
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| crate::invalid("invalid process session"))?;
            Ok((session, parse_start_time(&stat, false)?))
        };
        let (session, start) = match read_identity() {
            Ok(identity) => identity,
            Err(e) if skipped_member_error(state, pid, &e) => continue,
            Err(e) => return Err(e),
        };
        if session != leader || start < leader_start {
            continue;
        }
        let fd = match pin(pid) {
            Ok(fd) => fd,
            Err(e) if skipped_member_error(state, pid, &e) => continue,
            Err(e) => return Err(e),
        };
        // Pin and recheck both identities while the leader still reserves its session id.
        if !exited(&fd)? {
            match read_identity() {
                Ok(identity) if identity == (session, start) => {
                    if exited(leader_fd)? {
                        break;
                    }
                    members.push(fd);
                }
                Ok(_) => {}
                Err(e) if skipped_member_error(state, pid, &e) => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(members)
}

fn skipped_member_error(state: &Path, pid: u32, error: &io::Error) -> bool {
    if matches!(
        error.raw_os_error(),
        Some(libc::ENOENT | libc::ESRCH | libc::EACCES | libc::EPERM)
    ) {
        audit_stop(state, pid, "member-scan", error);
        true
    } else {
        false
    }
}

fn checked_binary(path: &Path, expected: &str) -> io::Result<File> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("sidecar is not a file"));
    }
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    if format!("{:x}", hash.finalize()) != expected {
        return Err(io::Error::other("sidecar hash mismatch"));
    }
    Ok(file)
}

/// A process name can contain spaces and parentheses; field 22 is relative to its final `)`.
pub fn start_time(pid: u32) -> io::Result<u64> {
    if pid < 2 || pid > i32::MAX as u32 {
        return Err(crate::invalid("invalid sidecar pid"));
    }
    let stat = std::fs::read(format!("/proc/{pid}/stat"))?;
    stat_start_time(&stat)
}

fn identity_start_time(pid: u32) -> io::Result<u64> {
    parse_start_time(&std::fs::read(format!("/proc/{pid}/stat"))?, false)
}

fn stat_start_time(stat: &[u8]) -> io::Result<u64> {
    parse_start_time(stat, true)
}

fn parse_start_time(stat: &[u8], require_alive: bool) -> io::Result<u64> {
    let fields = stat_fields(stat)?;
    if require_alive && matches!(fields.first(), Some(&"Z") | Some(&"X")) {
        return Err(io::Error::other("sidecar has exited"));
    }
    fields
        .get(19)
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| crate::invalid("invalid process start time"))
}

fn stat_fields(stat: &[u8]) -> io::Result<Vec<&str>> {
    let end_name = stat
        .iter()
        .rposition(|b| *b == b')')
        .ok_or_else(|| crate::invalid("invalid process stat"))?;
    Ok(std::str::from_utf8(&stat[end_name + 1..])
        .map_err(|_| crate::invalid("invalid process stat"))?
        .split_whitespace()
        .collect())
}

fn pin(pid: u32) -> io::Result<OwnedFd> {
    if pid < 2 || pid > i32::MAX as u32 {
        return Err(crate::invalid("invalid sidecar pid"));
    }
    // SAFETY: pidfd_open returns a new descriptor or -1, and does not access Rust memory.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

fn signal(fd: &OwnedFd, signal: i32) -> io::Result<()> {
    // SAFETY: the owned pidfd pins the target; no raw process id can be reused here.
    if unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    } == 0
    {
        return Ok(());
    }
    Err(io::Error::last_os_error())
}

fn exited(fd: &OwnedFd) -> io::Result<bool> {
    loop {
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one initialized pollfd, zero timeout; this never blocks a runtime worker.
        if unsafe { libc::poll(&mut poll, 1, 0) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if poll.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(io::Error::other("invalid sidecar pidfd"));
        }
        return Ok(poll.revents & (libc::POLLIN | libc::POLLHUP) != 0);
    }
}

async fn wait_pidfd(fd: &OwnedFd) -> io::Result<()> {
    while !exited(fd)? {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

/// Peer credentials are checked by the connector before HTTP/2 can read or write its first byte.
pub async fn handshake(socket: &Path, pid: u32) -> io::Result<()> {
    let socket = socket.to_owned();
    let exchange = async {
        let channel = tonic::transport::Endpoint::from_static("http://localhost")
            .connect_with_connector(tower::service_fn(move |_| {
                let socket = socket.clone();
                async move {
                    let stream = tokio::net::UnixStream::connect(socket).await?;
                    crate::peer::check(&stream)?;
                    if stream.peer_cred()?.pid() != Some(pid as i32) {
                        return Err(io::Error::other("sidecar socket peer pid differs"));
                    }
                    Ok::<_, io::Error>(hyper_util::rt::TokioIo::new(stream))
                }
            }))
            .await
            .map_err(io::Error::other)?;
        let response = RuntimeServiceClient::new(channel)
            .handshake(HandshakeRequest {
                client_protocol_major: agent::CLIENT_PROTOCOL_MAJOR,
            })
            .await
            .map_err(io::Error::other)?
            .into_inner();
        if response.protocol_major != agent::CLIENT_PROTOCOL_MAJOR {
            return Err(io::Error::other("unsupported sidecar protocol major"));
        }
        Ok(())
    };
    tokio::time::timeout(Duration::from_secs(2), exchange)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "sidecar handshake timed out"))?
}

/// A refusing record remains available to explain why removal cannot safely proceed.
pub(crate) struct RefusingRecord {
    pub id: String,
    pub pid: Option<u32>,
    pub status: &'static str,
    pub error: Option<String>,
}

/// Collection keeps artifacts needed to adopt a surviving sidecar even while its daemon is gone.
pub(crate) fn recorded_alive_in(run: &Path) -> io::Result<bool> {
    recorded_alive_with(run, identity_start_time)
}

fn recorded_alive_with(run: &Path, stat: impl Fn(u32) -> io::Result<u64>) -> io::Result<bool> {
    let records = refusing_records_with(run, stat)?;
    if let Some(record) = records.iter().find(|record| record.status == "unreadable") {
        return Err(io::Error::other(
            record.error.clone().unwrap_or_else(|| "unreadable sidecar".into()),
        ));
    }
    Ok(!records.is_empty())
}

pub(crate) fn refusing_records_in(run: &Path) -> io::Result<Vec<RefusingRecord>> {
    refusing_records_with(run, identity_start_time)
}

/// The roots of the sidecars another daemon's run directory records as still running: `None` for a record that names
/// a live process but cannot be read or judged, since that sidecar may run anywhere.
pub(crate) fn live_roots_in(run: &Path) -> io::Result<Vec<Option<String>>> {
    let entries = match std::fs::read_dir(run) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut roots = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|name| name.strip_prefix("sc-"))
            .and_then(|name| name.strip_suffix(".pid"))
        else {
            continue;
        };
        if id.len() != 16 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let judged = (|| {
            let bytes = private_fs::read_private_nofollow_bytes_capped(&entry.path(), 64 * 1024)?;
            let record: Record = serde_json::from_slice(&bytes)?;
            Ok::<_, io::Error>(record_alive_with(&record, &identity_start_time)?.then_some(record.root))
        })();
        match judged {
            Ok(Some(root)) => roots.push(Some(root)),
            Ok(None) => {}
            Err(_) => roots.push(None),
        }
    }
    Ok(roots)
}

fn refusing_records_with(run: &Path, stat: impl Fn(u32) -> io::Result<u64>) -> io::Result<Vec<RefusingRecord>> {
    let entries = match std::fs::read_dir(run) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut refusing = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|name| name.strip_prefix("sc-"))
            .and_then(|name| name.strip_suffix(".pid"))
        else {
            continue;
        };
        if id.len() != 16 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let mut pid = None;
        let alive = (|| {
            let bytes = private_fs::read_private_nofollow_bytes_capped(&entry.path(), 64 * 1024)?;
            let record: Record = serde_json::from_slice(&bytes)?;
            pid = Some(record.pid);
            record_alive_with(&record, &stat)
        })();
        match alive {
            Ok(false) => {}
            result => refusing.push(RefusingRecord {
                id: id.into(),
                pid,
                status: if result.is_ok() { "alive" } else { "unreadable" },
                error: result.err().map(|error| error.to_string()),
            }),
        }
    }
    Ok(refusing)
}

fn record_alive_with(record: &Record, stat: &impl Fn(u32) -> io::Result<u64>) -> io::Result<bool> {
    record_alive_readers(record, stat, &|fd| signal(fd, 0))
}

fn record_alive_readers(
    record: &Record,
    stat: &impl Fn(u32) -> io::Result<u64>,
    ownership: &impl Fn(&OwnedFd) -> io::Result<()>,
) -> io::Result<bool> {
    let fd = match pin(record.pid) {
        Ok(fd) => fd,
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return Ok(false),
        Err(error) => return Err(error),
    };
    if exited(&fd)? {
        return Ok(false);
    }
    match stat(record.pid) {
        Ok(start) => Ok(start == record.start_time),
        // A same-uid process can hide its stat by becoming non-dumpable. Check signal permission
        // independently while the pidfd pins it; uncertainty must preserve adoption records.
        Err(error) if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::EACCES)) => match ownership(&fd) {
            Ok(()) => Ok(true),
            Err(error) if error.raw_os_error() == Some(libc::EPERM) => Ok(false),
            Err(error) => Err(error),
        },
        Err(_) if exited(&fd)? => Ok(false),
        Err(error) => Err(error),
    }
}

fn read_records(paths: &Paths) -> Vec<Record> {
    let metadata_run = paths.metadata_run();
    let run_dir = &paths.run;
    let Ok(files) = std::fs::read_dir(metadata_run) else {
        return Vec::new();
    };
    files
        .flatten()
        .filter_map(|file| {
            let name = file.file_name();
            let name = name.to_str()?;
            let id = name.strip_prefix("sc-")?.strip_suffix(".pid")?;
            if id.len() != 16 || !id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
                return None;
            }
            let bytes = private_fs::read_private_nofollow_bytes_capped(&file.path(), 64 * 1024).ok()?;
            let mut record: Record = serde_json::from_slice(&bytes).ok()?;
            if record.socket != agent::socket_path::in_dir(run_dir, &format!("sc-{id}.sock")).ok()? {
                return None;
            }
            validate_command_id(&record.command_id).ok()?;
            if record.root.len() > 4096
                || record.account.len() > 128
                || record.failure.as_ref().is_some_and(|s| s.len() > 4096)
            {
                return None;
            }
            record.sidecar_id = id.to_owned();
            Some(record)
        })
        .collect()
}

fn trim_log(path: &Path) -> io::Result<()> {
    trim_log_with(path, |dir, name, bytes| {
        private_fs::publish_private_file(dir, name, bytes).map(|_| ())
    })
}

fn trim_log_with(path: &Path, publish: impl FnOnce(&File, &OsStr, &[u8]) -> io::Result<()>) -> io::Result<()> {
    // The append descriptor stays attached to this inode while a detached child writes it.
    let output = match std::fs::OpenOptions::new()
        .append(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !output.metadata()?.is_file() {
        return Err(io::Error::other("sidecar log is not a file"));
    }
    let len = output.metadata()?.len();
    if len <= LOG_CAP {
        return Ok(());
    }
    let mut input = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = input.metadata()?;
    let target = output.metadata()?;
    if !metadata.is_file() || metadata.dev() != target.dev() || metadata.ino() != target.ino() {
        return Err(io::Error::other("sidecar log changed while trimming"));
    }
    input.seek(SeekFrom::Start(len.saturating_sub(LOG_TAIL)))?;
    let mut tail = Vec::new();
    input.take(LOG_TAIL).read_to_end(&mut tail)?;
    let dir = private_fs::open_dir(path.parent().expect("log directory"))?;
    let name = format!("{}.1", path.file_name().expect("log name").to_string_lossy());
    match publish(&dir, OsStr::new(&name), &tail) {
        Ok(_) => output.set_len(0),
        Err(error) if error.raw_os_error() == Some(libc::ENOSPC) => {
            output.set_len(0)?;
            Err(error)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let root = std::env::temp_dir().join(format!(
                "sidecar-files-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            private_fs::create_private_dir_all(&root, &root).unwrap();
            Self(root)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn hidden_same_uid_pids_preserve_their_adoption_records() {
        let scratch = Scratch::new();
        let pid = std::process::id();
        let record = serde_json::json!({"pid": pid, "start_time": identity_start_time(pid).unwrap(), "socket": ""});
        private_fs::write_private(
            &scratch.0.join("sc-0123456789abcdef.pid"),
            &serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        for errno in [libc::ENOENT, libc::EACCES] {
            assert!(recorded_alive_with(&scratch.0, |_| Err(io::Error::from_raw_os_error(errno))).unwrap());
        }
        assert!(recorded_alive_with(&scratch.0, |_| Err(io::Error::from_raw_os_error(libc::EIO))).is_err());
        assert!(recorded_alive_with(&scratch.0, identity_start_time).unwrap());
        assert!(!recorded_alive_with(&scratch.0, |_| Ok(0)).unwrap());
    }

    #[test]
    fn hidden_identity_checks_ownership_independently() {
        let pid = std::process::id();
        let record: Record = serde_json::from_value(serde_json::json!({
            "pid": pid, "start_time": identity_start_time(pid).unwrap(), "socket": ""
        }))
        .unwrap();
        for errno in [libc::ENOENT, libc::EACCES] {
            let stat = |_| Err(io::Error::from_raw_os_error(errno));
            assert!(record_alive_readers(&record, &stat, &|_| Ok(())).unwrap());
            assert!(
                !record_alive_readers(&record, &stat, &|_| Err(io::Error::from_raw_os_error(libc::EPERM))).unwrap()
            );
            for failure in [libc::EIO, libc::ESRCH, libc::EACCES] {
                assert_eq!(
                    record_alive_readers(&record, &stat, &|_| Err(io::Error::from_raw_os_error(failure)))
                        .unwrap_err()
                        .raw_os_error(),
                    Some(failure)
                );
            }
        }
    }

    #[test]
    fn process_start_time_ignores_arbitrary_bytes_in_the_name() {
        let mut stat = b"42 (name ) with \xff\n) S".to_vec();
        for n in 4..=21 {
            stat.extend_from_slice(format!(" {n}").as_bytes());
        }
        stat.extend_from_slice(b" 123456 23 24\n");
        assert_eq!(stat_start_time(&stat).unwrap(), 123456);
        assert!(stat_start_time(b"42 (gone) Z 1 2 3").is_err());
        assert!(stat_start_time(b"42 (short) S 1 2 3").is_err());
    }

    #[test]
    fn trimming_preserves_the_tail_and_the_childs_append_inode() {
        let scratch = Scratch::new();
        let path = scratch.0.join("sc-0123456789abcdef.err");
        let mut child_log = private_fs::append_private(&path).unwrap();
        child_log.set_len(LOG_CAP + 1).unwrap();
        let tail = vec![b'x'; LOG_TAIL as usize];
        child_log.write_all(&tail).unwrap();
        let inode = child_log.metadata().unwrap().ino();
        trim_log(&path).unwrap();
        assert_eq!(child_log.metadata().unwrap().len(), 0);
        assert_eq!(std::fs::read(path.with_extension("err.1")).unwrap(), tail);
        child_log.write_all(b"after trimming").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"after trimming");
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(
            std::fs::metadata(path.with_extension("err.1"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn checked_executable_stays_the_same_after_an_install_replacement() {
        let scratch = Scratch::new();
        let path = scratch.0.join("sidecar");
        crate::daemon::test_exec::install_executable(Path::new("/bin/true"), &path);
        let hash = format!("{:x}", Sha256::digest(std::fs::read(&path).unwrap()));
        let executable = checked_binary(&path, &hash).unwrap();
        std::fs::rename(&path, scratch.0.join("old")).unwrap();
        crate::daemon::test_exec::install_executable(Path::new("/bin/false"), &path);
        assert!(checked_binary(&path, &hash).is_err());
        assert!(
            crate::daemon::test_exec::status(&mut std::process::Command::new(format!(
                "/proc/self/fd/{}",
                executable.as_raw_fd()
            )))
            .unwrap()
            .success()
        );
    }

    #[tokio::test]
    async fn pidfd_stop_reaps_an_owned_process() {
        let mut child = tokio::process::Command::new("/bin/sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        assert!(child.try_wait().unwrap().is_none());
        let entry = Arc::new(Mutex::new(Entry {
            record: Record {
                pid,
                start_time: start_time(pid).unwrap(),
                socket: PathBuf::new(),
                sidecar_id: "0123456789abcdef".into(),
                root: "/srv/project".into(),
                account: String::new(),
                command_id: "stop".into(),
                started_unix_ms: 0,
                ready: true,
                failure: None,
                exited_unix_ms: None,
            },
            pidfd: Some(Arc::new(pin(pid).unwrap())),
            child: Some(child),
            handshake: true,
        }));
        assert!(entry.lock().unwrap().alive());
        stop_entry(&entry, &short_scratch().0).await.unwrap();
        assert!(!entry.lock().unwrap().alive());
        assert!(entry.lock().unwrap().child.is_none());
        assert!(!crate::run_dir::pid_alive(pid));
    }
    fn short_scratch() -> Scratch {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/tmp/sc-unit-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        private_fs::create_private_dir_all(&root, &root).unwrap();
        Scratch(root)
    }

    fn manager(scratch: &Scratch) -> Sidecars {
        let mut paths = Paths::new(
            "test",
            &scratch.0.join("d"),
            &scratch.0.join("s"),
            &std::env::temp_dir(),
        )
        .unwrap();
        paths.prepare().unwrap();
        paths.install = Some(scratch.0.join("d/eitri-server/test-0123456"));
        let manager = Sidecars::new(paths).unwrap();
        manager.set_environment(Ok(Captured {
            env: login_env::Environment::new(),
            fallback: None,
        }));
        manager
    }

    #[tokio::test]
    async fn an_owned_child_exiting_before_publication_keeps_the_startup_refusal() {
        let scratch = short_scratch();
        let manager = manager(&scratch);
        let installed = manager.paths.install.as_ref().unwrap().join("sidecar/test-rev");
        private_fs::create_private_dir_all(&installed, &manager.paths.data).unwrap();
        let executable = installed.join("verdandi-claude-sidecar");
        crate::daemon::test_exec::install_executable(Path::new("/bin/false"), &executable);
        let error = manager
            .start_with(
                StartSidecarRequest {
                    root: scratch.0.to_str().unwrap().into(),
                    command_id: "early-exit".into(),
                    sidecar_revision: "test-rev".into(),
                    sidecar_sha256: format!("{:x}", Sha256::digest(std::fs::read(executable).unwrap())),
                    ..Default::default()
                },
                |pid| {
                    let fd = pin(pid).unwrap();
                    let mut poll = libc::pollfd {
                        fd: fd.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // SAFETY: poll gets one initialized descriptor; the timeout only guards a hung child.
                    assert_eq!(unsafe { libc::poll(&mut poll, 1, 15_000) }, 1);
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::Unavailable, "{error:?}");
        assert_eq!(error.message(), "sidecar exited before binding");
        let records = read_records(&manager.paths);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].failure.as_deref(), Some(error.message()));
        assert!(!manager.any_alive());
        assert!(manager.snapshot()[0].lock().unwrap().child.is_none());
    }

    fn record(id: &str) -> Record {
        Record {
            pid: i32::MAX as u32,
            start_time: 0,
            socket: PathBuf::new(),
            sidecar_id: id.into(),
            root: "/srv/project".into(),
            account: "example-account".into(),
            command_id: "command".into(),
            started_unix_ms: 0,
            ready: true,
            failure: None,
            exited_unix_ms: None,
        }
    }

    fn sleeping_entry() -> SharedEntry {
        let mut command = tokio::process::Command::new("/bin/sleep");
        command.arg("60");
        command_entry(command)
    }

    fn command_entry(mut command: tokio::process::Command) -> SharedEntry {
        command.kill_on_drop(true);
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        let pid = child.id().unwrap();
        let mut record = record("0123456789abcdef");
        record.pid = pid;
        record.start_time = start_time(pid).unwrap();
        record.ready = false;
        Arc::new(Mutex::new(Entry {
            record,
            pidfd: Some(Arc::new(pin(pid).unwrap())),
            child: Some(child),
            handshake: false,
        }))
    }

    async fn term_ignoring_entry(scratch: &Scratch) -> SharedEntry {
        let ready = scratch.0.join("term-ready");
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .args(["-c", "trap '' TERM; : > \"$1\"; exec /bin/sleep 60", "leader"])
            .arg(&ready);
        let entry = command_entry(command);
        tokio::time::timeout(Duration::from_secs(15), async {
            while !ready.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        entry
    }

    #[tokio::test]
    async fn dead_adopted_entries_never_scan_or_signal_a_reused_session_id() {
        let scratch = short_scratch();
        let manager = manager(&scratch);
        let old = sleeping_entry();
        let dead_fd = old.lock().unwrap().pidfd.as_ref().unwrap().clone();
        stop_entry(&old, &manager.paths.state).await.unwrap();
        assert!(exited(&dead_fd).unwrap());
        let unrelated = sleeping_entry();
        let unrelated_pid = unrelated.lock().unwrap().record.pid;
        let unrelated_fd = unrelated.lock().unwrap().pidfd.as_ref().unwrap().clone();
        // Model a recorded numeric session id now belonging to an unrelated live session.
        let mut stale = record("0123456789abcdef");
        stale.pid = unrelated_pid;
        stale.start_time = 0;
        stale.ready = false;
        let stale = Arc::new(Mutex::new(Entry {
            record: stale,
            pidfd: Some(dead_fd.clone()),
            child: None,
            handshake: false,
        }));
        stop_entry_with(
            &stale,
            &manager.paths.state,
            |_, _, _, _| panic!("a dead leader must not trigger a session scan"),
            |_, _| panic!("a dead entry must not signal any pidfd"),
        )
        .await
        .unwrap();
        manager
            .entries
            .lock()
            .unwrap()
            .insert("0123456789abcdef".into(), stale.clone());
        for action in 0..3 {
            stale.lock().unwrap().pidfd = Some(dead_fd.clone());
            match action {
                0 => manager.stop("0123456789abcdef").await.unwrap(),
                1 => assert!(manager.recover_start("command").await.unwrap().is_none()),
                _ => manager.stop_all().await.unwrap(),
            }
            assert!(!exited(&unrelated_fd).unwrap(), "unrelated session must survive");
        }
        stop_entry(&unrelated, &manager.paths.state).await.unwrap();
    }

    #[tokio::test]
    async fn adopted_stop_audits_unsignallable_members_and_escalates_other_targets() {
        let scratch = short_scratch();
        let manager = manager(&scratch);
        let leader = term_ignoring_entry(&scratch).await;
        let mut child = leader.lock().unwrap().child.take().unwrap();
        let leader_fd = leader.lock().unwrap().pidfd.as_ref().unwrap().clone();
        let denied = sleeping_entry();
        let vanished = sleeping_entry();
        let member_scratch = short_scratch();
        let member = term_ignoring_entry(&member_scratch).await;
        let member_fd = member.lock().unwrap().pidfd.as_ref().unwrap().clone();
        let pins: Vec<_> = [&denied, &vanished, &member]
            .iter()
            .map(|entry| pin(entry.lock().unwrap().record.pid).unwrap())
            .collect();
        let denied_raw = pins[0].as_raw_fd();
        let vanished_raw = pins[1].as_raw_fd();
        let sent = Mutex::new(Vec::new());
        stop_entry_with(
            &leader,
            &manager.paths.state,
            move |_, _, _, _| Ok(pins),
            |fd, sig| {
                sent.lock().unwrap().push((fd.as_raw_fd(), sig));
                if fd.as_raw_fd() == denied_raw {
                    Err(io::Error::from_raw_os_error(libc::EPERM))
                } else if fd.as_raw_fd() == vanished_raw {
                    Err(io::Error::from_raw_os_error(libc::ESRCH))
                } else {
                    signal(fd, sig)
                }
            },
        )
        .await
        .unwrap();
        child.wait().await.unwrap();
        assert!(exited(&leader_fd).unwrap());
        assert!(exited(&member_fd).unwrap());
        let sent = sent.into_inner().unwrap();
        assert!(sent.contains(&(leader_fd.as_raw_fd(), libc::SIGKILL)));
        for raw in [denied_raw, vanished_raw] {
            assert!(sent.contains(&(raw, libc::SIGTERM)));
            assert!(
                !sent.contains(&(raw, libc::SIGKILL)),
                "skipped member must not be waited on or retried"
            );
        }
        let audit = std::fs::read_to_string(manager.paths.state.join("audit.log")).unwrap();
        assert!(audit.contains("member-signal: Operation not permitted"));
        assert!(audit.contains("member-signal: No such process"));
        for entry in [&denied, &vanished, &member] {
            stop_entry(entry, &manager.paths.state).await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_failed_session_scan_still_kills_the_leader_and_group() {
        let scratch = short_scratch();
        let manager = manager(&scratch);
        let pid_file = scratch.0.join("member");
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .args([
                "-c",
                "trap '' TERM; /bin/sleep 60 & printf '%s' $! >\"$1\"; wait",
                "leader",
            ])
            .arg(&pid_file);
        let leader = command_entry(command);
        let mut child = leader.lock().unwrap().child.take().unwrap();
        let leader_fd = leader.lock().unwrap().pidfd.as_ref().unwrap().clone();
        let member_pid = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(pid) = std::fs::read_to_string(&pid_file)
                    .ok()
                    .and_then(|s| s.parse::<u32>().ok())
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let member_fd = pin(member_pid).unwrap();
        let signals = Mutex::new(Vec::new());
        stop_entry_with(
            &leader,
            &manager.paths.state,
            |_, _, _, _| Err(io::Error::from_raw_os_error(libc::EMFILE)),
            |fd, sig| {
                signals.lock().unwrap().push(sig);
                signal(fd, sig)
            },
        )
        .await
        .unwrap();
        child.wait().await.unwrap();
        assert_eq!(signals.into_inner().unwrap(), [libc::SIGTERM, libc::SIGKILL]);
        assert!(exited(&leader_fd).unwrap());
        tokio::time::timeout(KILL_BUDGET, wait_pidfd(&member_fd))
            .await
            .unwrap()
            .unwrap();
        let audit = std::fs::read_to_string(manager.paths.state.join("audit.log")).unwrap();
        assert!(audit.contains("scan: Too many open files"));
    }

    #[tokio::test]
    async fn maintenance_stops_an_expired_adopted_start_without_a_retry() {
        let scratch = short_scratch();
        let manager = manager(&scratch);
        let original = sleeping_entry();
        let fd = original.lock().unwrap().pidfd.as_ref().unwrap().clone();
        {
            let mut original = original.lock().unwrap();
            original.record.socket =
                agent::socket_path::in_dir(&manager.paths.run, "sc-0123456789abcdef.sock").unwrap();
            // A recent interrupted record gets the remainder of its bind budget.
            original.record.started_unix_ms = crate::run_dir::unix_ms();
            manager.persist(&original.record).unwrap();
        }
        let adopted = Sidecars::new(manager.paths.clone()).unwrap();
        adopted.refresh_handshakes().await;
        assert!(adopted.any_alive());
        assert!(read_records(&manager.paths)[0].failure.is_none());
        adopted.snapshot()[0].lock().unwrap().record.started_unix_ms =
            crate::run_dir::unix_ms() - SIDECAR_BIND_BUDGET.as_millis() as u64;
        adopted.refresh_handshakes().await;
        assert!(exited(&fd).unwrap());
        assert!(!adopted.any_alive());
        let failed = &read_records(&manager.paths)[0];
        assert!(!failed.ready);
        assert_eq!(failed.failure.as_deref(), Some("startup interrupted before readiness"));
        stop_entry(&original, &manager.paths.state).await.unwrap();
    }

    #[tokio::test]
    async fn startup_errors_stop_the_child_even_when_record_publication_fails() {
        let scratch = short_scratch();
        let manager = manager(&scratch);
        std::fs::rename(manager.paths.metadata_run(), scratch.0.join("saved-run")).unwrap();
        for readiness in [Err(io::Error::other("readiness failed")), Ok(())] {
            let failed_readiness = readiness.is_err();
            let entry = sleeping_entry();
            let fd = entry.lock().unwrap().pidfd.as_ref().unwrap().try_clone().unwrap();
            let error = manager.finish_start(&entry, readiness).await.unwrap_err();
            assert_eq!(
                error.code(),
                if failed_readiness {
                    tonic::Code::Unavailable
                } else {
                    tonic::Code::Internal
                }
            );
            if failed_readiness {
                assert_eq!(error.message(), "readiness failed");
            }
            assert!(exited(&fd).unwrap());
            let entry = entry.lock().unwrap();
            assert!(!entry.record.ready);
            assert!(entry.record.failure.is_some());
            assert!(entry.child.is_none());
        }
        let audit = std::fs::read_to_string(manager.paths.state.join("audit.log")).unwrap();
        assert_eq!(audit.matches("sidecar-start-failed").count(), 2);
    }

    #[tokio::test]
    async fn held_pidfd_controls_liveness_and_stop_without_reading_proc_identity() {
        let entry = sleeping_entry();
        let fd = entry.lock().unwrap().pidfd.as_ref().unwrap().try_clone().unwrap();
        {
            let mut entry = entry.lock().unwrap();
            // Unreadable metadata must not override an already pinned live process.
            entry.record.pid = i32::MAX as u32;
            entry.record.start_time = 0;
            assert!(entry.alive());
        }
        stop_entry(&entry, &short_scratch().0).await.unwrap();
        assert!(exited(&fd).unwrap());
        assert!(!entry.lock().unwrap().alive());
    }

    #[tokio::test]
    async fn an_interrupted_unresponsive_start_is_stopped_before_a_retry() {
        let scratch = short_scratch();
        let manager = manager(&scratch);
        let original = sleeping_entry();
        let fd = original.lock().unwrap().pidfd.as_ref().unwrap().try_clone().unwrap();
        {
            let mut original = original.lock().unwrap();
            original.record.socket =
                agent::socket_path::in_dir(&manager.paths.run, "sc-0123456789abcdef.sock").unwrap();
            manager.persist(&original.record).unwrap();
        }
        let reloaded = Sidecars::new(manager.paths.clone()).unwrap();
        reloaded.set_environment(Ok(Captured {
            env: login_env::Environment::new(),
            fallback: None,
        }));
        let request = StartSidecarRequest {
            root: scratch.0.to_str().unwrap().into(),
            command_id: "command".into(),
            sidecar_revision: "missing".into(),
            sidecar_sha256: "a".repeat(64),
            ..Default::default()
        };
        assert_eq!(
            reloaded.start(request).await.unwrap_err().code(),
            tonic::Code::FailedPrecondition
        );
        assert!(exited(&fd).unwrap());
        stop_entry(&original, &manager.paths.state).await.unwrap();
        assert!(read_records(&manager.paths)[0].failure.is_some());
    }

    #[tokio::test]
    async fn failed_identity_capture_is_audited_and_retried_then_cached() {
        let scratch = short_scratch();
        let manager = manager(&scratch);
        manager.set_environment(Err(io::Error::other("passwd unavailable")));
        assert!(std::fs::read_to_string(manager.paths.state.join("audit.log"))
            .unwrap()
            .contains("login-env-unavailable"));
        assert!(manager
            .environment_with(|| async { Err(io::Error::other("still unavailable")) })
            .await
            .is_err());
        let captured = manager
            .environment_with(|| async {
                Ok(Captured {
                    env: login_env::Environment::from([("USER".into(), "example-user".into())]),
                    fallback: None,
                })
            })
            .await
            .unwrap();
        assert_eq!(captured.env[OsStr::new("USER")], "example-user");
        let cached = manager
            .environment_with(|| async { panic!("successful capture must be cached") })
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&captured, &cached));
    }

    #[test]
    fn login_environment_has_typed_states_and_only_fallback_has_a_warning() {
        for (state, number, name) in [
            (LoginEnvState::Capturing, 0, "LOGIN_ENV_STATE_CAPTURING"),
            (LoginEnvState::Captured, 1, "LOGIN_ENV_STATE_CAPTURED"),
            (LoginEnvState::FallbackMinimal, 2, "LOGIN_ENV_STATE_FALLBACK_MINIMAL"),
            (LoginEnvState::Unavailable, 3, "LOGIN_ENV_STATE_UNAVAILABLE"),
        ] {
            assert_eq!(state as i32, number);
            assert_eq!(state.as_str_name(), name);
            assert_eq!(LoginEnvState::from_str_name(name), Some(state));
        }
        let scratch = short_scratch();
        let ready = manager(&scratch);
        let manager = Sidecars::new(ready.paths.clone()).unwrap();
        assert_eq!(manager.login_env_state(), LoginEnvState::Capturing);
        assert!(manager.warnings().is_empty());
        assert_eq!(manager.list().login_env(), LoginEnvState::Capturing);
        manager.set_environment(Ok(Captured {
            env: login_env::Environment::new(),
            fallback: None,
        }));
        assert_eq!(manager.list().login_env(), LoginEnvState::Captured);
        assert!(manager.warnings().is_empty());
        manager.set_environment(Ok(Captured {
            env: login_env::Environment::new(),
            fallback: Some("minimal fallback".into()),
        }));
        assert_eq!(manager.list().login_env(), LoginEnvState::FallbackMinimal);
        assert_eq!(manager.warnings(), vec!["minimal fallback"]);
        manager.set_environment(Err(io::Error::other("passwd unavailable")));
        assert_eq!(manager.list().login_env(), LoginEnvState::Unavailable);
        assert!(manager.warnings().is_empty());
    }

    #[tokio::test]
    async fn a_start_waiting_for_capture_returns_that_attempts_error_without_retrying() {
        let scratch = short_scratch();
        let ready = manager(&scratch);
        let manager = Sidecars::new(ready.paths.clone()).unwrap();
        let waiting = manager.environment_with(|| async { panic!("a pending capture must not be retried") });
        let publish = async {
            while manager.env.receiver_count() == 0 {
                tokio::task::yield_now().await;
            }
            manager.set_environment(Err(io::Error::other("passwd unavailable")));
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(15), async { tokio::join!(waiting, publish) })
            .await
            .unwrap();
        assert_eq!(result.err().unwrap().message(), "passwd unavailable");
    }

    #[test]
    fn trimming_never_creates_an_absent_log() {
        let scratch = Scratch::new();
        let path = scratch.0.join("expired.err");
        trim_log(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn persisted_requests_are_bounded_before_starting() {
        let valid = StartSidecarRequest {
            root: "/srv/project".into(),
            account: "example-account".into(),
            env: Default::default(),
            command_id: "example-command:1".into(),
            sidecar_revision: "test-rev".into(),
            sidecar_sha256: "a".repeat(64),
        };
        assert!(validate_request(&valid).is_ok());
        for command in [
            "x".repeat(129),
            "bad\ncommand".into(),
            "../command".into(),
            String::new(),
        ] {
            let mut request = valid.clone();
            request.command_id = command;
            assert!(validate_request(&request).is_err());
        }
        for name in ["../account", "with space", " example-account "] {
            let mut request = valid.clone();
            request.account = name.into();
            assert!(validate_request(&request).is_err());
        }
        let mut request = valid;
        request.root = "/".repeat(4097);
        assert!(validate_request(&request).is_err());
    }

    #[test]
    fn root_handle_follows_links_refuses_non_normal_paths_and_pins_the_directory() {
        let scratch = Scratch::new();
        let root = scratch.0.join("project");
        std::fs::create_dir(&root).unwrap();
        let held = open_root(root.to_str().unwrap()).unwrap();
        let original = held.metadata().unwrap().ino();
        std::fs::rename(&root, scratch.0.join("original")).unwrap();
        std::fs::create_dir(&root).unwrap();
        assert_eq!(held.metadata().unwrap().ino(), original);
        assert_ne!(std::fs::metadata(&root).unwrap().ino(), original);
        std::os::unix::fs::symlink(&root, scratch.0.join("link")).unwrap();
        assert_eq!(
            open_root(scratch.0.join("link").to_str().unwrap())
                .unwrap()
                .metadata()
                .unwrap()
                .ino(),
            std::fs::metadata(&root).unwrap().ino()
        );
        for path in [
            root.join(".."),
            root.join("."),
            PathBuf::from(format!("{}/", root.display())),
        ] {
            assert!(open_root(path.to_str().unwrap()).is_err(), "{}", path.display());
        }
    }

    #[test]
    fn one_untrimmable_log_does_not_prevent_trimming_another() {
        let scratch = short_scratch();
        let manager = manager(&scratch);
        let id = "0123456789abcdef";
        let bad = manager.paths.metadata_run().join(format!("sc-{id}.out"));
        std::os::unix::fs::symlink("/dev/null", &bad).unwrap();
        let good = manager.paths.metadata_run().join(format!("sc-{id}.err"));
        private_fs::append_private(&good).unwrap().set_len(LOG_CAP + 1).unwrap();
        manager.entries.lock().unwrap().insert(
            id.into(),
            Arc::new(Mutex::new(Entry {
                record: record(id),
                pidfd: None,
                child: None,
                handshake: false,
            })),
        );
        manager.trim_logs();
        assert_eq!(std::fs::metadata(good).unwrap().len(), 0);
        let audit = std::fs::read_to_string(manager.paths.state.join("audit.log")).unwrap();
        assert!(audit.contains("sidecar-log-trim-error"));
    }

    #[test]
    fn exited_records_and_logs_expire_only_after_seven_days() {
        let scratch = short_scratch();
        let manager = manager(&scratch);
        for (id, age) in [
            ("0123456789abcdef", RETENTION_MS + 1),
            ("fedcba9876543210", RETENTION_MS - 1000),
        ] {
            let mut record = record(id);
            record.socket = agent::socket_path::in_dir(&manager.paths.run, &format!("sc-{id}.sock")).unwrap();
            record.exited_unix_ms = Some(crate::run_dir::unix_ms() - age);
            manager.persist(&record).unwrap();
            private_fs::append_private(&manager.paths.metadata_run().join(format!("sc-{id}.err"))).unwrap();
            manager.entries.lock().unwrap().insert(
                id.into(),
                Arc::new(Mutex::new(Entry {
                    record,
                    pidfd: None,
                    child: None,
                    handshake: false,
                })),
            );
        }
        manager.observe_exits();
        assert_eq!(manager.list().sidecars.len(), 1);
        assert_eq!(manager.list().sidecars[0].sidecar_id, "fedcba9876543210");
        assert!(!manager.paths.metadata_run().join("sc-0123456789abcdef.pid").exists());
        assert!(!manager.paths.metadata_run().join("sc-0123456789abcdef.err").exists());
        assert!(std::fs::read_to_string(manager.paths.state.join("audit.log"))
            .unwrap()
            .contains("sidecar-expire"));
    }

    #[tokio::test]
    async fn missed_adoption_handshake_keeps_the_process_listed_and_stoppable() {
        let scratch = short_scratch();
        let manager = manager(&scratch);
        let mut command = tokio::process::Command::new("/bin/sleep");
        command.arg("60").kill_on_drop(true);
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let pid = child.id().unwrap();
        let mut record = record("0123456789abcdef");
        record.pid = pid;
        record.start_time = start_time(pid).unwrap();
        record.socket = agent::socket_path::in_dir(&manager.paths.run, "sc-0123456789abcdef.sock").unwrap();
        manager.persist(&record).unwrap();
        let adopted = Sidecars::new(manager.paths.clone()).unwrap();
        adopted.refresh_handshakes().await;
        let listed = adopted.list().sidecars;
        assert!(listed[0].alive);
        assert!(!listed[0].handshake);
        adopted.stop("0123456789abcdef").await.unwrap();
        child.wait().await.unwrap();
        assert!(!adopted.any_alive());
    }

    #[tokio::test]
    async fn failed_start_records_are_not_replayed_as_success_after_reload() {
        let scratch = short_scratch();
        let manager = manager(&scratch);
        let mut failed = record("0123456789abcdef");
        failed.ready = false;
        failed.failure = Some("sidecar exited before binding".into());
        failed.socket = agent::socket_path::in_dir(&manager.paths.run, "sc-0123456789abcdef.sock").unwrap();
        manager.persist(&failed).unwrap();
        let request = StartSidecarRequest {
            root: scratch.0.to_str().unwrap().into(),
            account: String::new(),
            env: Default::default(),
            command_id: failed.command_id,
            sidecar_revision: "missing".into(),
            sidecar_sha256: "a".repeat(64),
        };
        for _ in 0..2 {
            let reloaded = Sidecars::new(manager.paths.clone()).unwrap();
            reloaded.set_environment(Ok(Captured {
                env: login_env::Environment::new(),
                fallback: None,
            }));
            // A fresh attempt reaches executable validation rather than returning the failed id.
            assert_eq!(
                reloaded.start(request.clone()).await.unwrap_err().code(),
                tonic::Code::FailedPrecondition
            );
        }
    }

    #[test]
    fn child_uses_the_held_root_and_restores_the_inherited_file_modes() {
        let scratch = Scratch::new();
        let root = scratch.0.join("project");
        std::fs::create_dir(&root).unwrap();
        let linked_home = scratch.0.join("home");
        std::os::unix::fs::symlink(&scratch.0, &linked_home).unwrap();
        let held = open_root(linked_home.join("project").to_str().unwrap()).unwrap();
        let original = scratch.0.join("original");
        std::fs::rename(&root, &original).unwrap();
        std::fs::create_dir(&root).unwrap();
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", ": > created; mkdir directory"]);
        let fd = held.as_raw_fd();
        unsafe {
            command.pre_exec(move || child_setup(fd, 0o022));
        }
        assert!(command.status().unwrap().success());
        assert!(!root.join("created").exists());
        assert_eq!(
            std::fs::metadata(original.join("created"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
        assert_eq!(
            std::fs::metadata(original.join("directory"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }

    #[tokio::test]
    async fn adopted_stop_kills_term_ignoring_members_after_the_leader_is_reaped() {
        let scratch = short_scratch();
        let pid_file = scratch.0.join("descendant");
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args([
            "-c",
            "/bin/sh -c 'trap \"\" TERM; printf \"%s\" \"$$\" >\"$1\"; exec /bin/sleep 60' descendant \"$1\" & wait",
            "leader",
        ]).arg(&pid_file).kill_on_drop(true);
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let pid = child.id().unwrap();
        let mut record = record("0123456789abcdef");
        record.pid = pid;
        record.start_time = start_time(pid).unwrap();
        let entry = Arc::new(Mutex::new(Entry {
            record,
            pidfd: Some(Arc::new(pin(pid).unwrap())),
            child: None,
            handshake: false,
        }));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let descendant = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
            {
                break pid;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let descendant_fd = pin(descendant).unwrap();
        let state = &scratch.0;
        assert!(entry.lock().unwrap().alive());
        assert!(!exited(&descendant_fd).unwrap(), "descendant must ignore TERM");
        let stop = stop_entry(&entry, state);
        let reap = async {
            child.wait().await.unwrap();
            assert!(identity_start_time(pid).is_err(), "leader is reaped during stop");
        };
        let (result, ()) = tokio::join!(stop, reap);
        result.unwrap();
        assert!(exited(&descendant_fd).unwrap());
    }

    #[tokio::test]
    async fn stop_kills_descendants_even_when_the_leader_exits_on_term() {
        let scratch = short_scratch();
        let manager = manager(&scratch);
        let pid_file = scratch.0.join("descendant");
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .args([
                "-c",
                "(trap '' TERM; exec sleep 60) & printf '%s' $! >\"$1\"; wait",
                "leader",
            ])
            .arg(&pid_file)
            .kill_on_drop(true);
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        let pid = child.id().unwrap();
        let mut record = record("0123456789abcdef");
        record.pid = pid;
        record.start_time = start_time(pid).unwrap();
        let entry = Arc::new(Mutex::new(Entry {
            record,
            pidfd: Some(Arc::new(pin(pid).unwrap())),
            child: Some(child),
            handshake: false,
        }));
        manager.entries.lock().unwrap().insert("0123456789abcdef".into(), entry);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let descendant = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
            {
                break pid;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert!(start_time(descendant).is_ok());
        manager.stop("0123456789abcdef").await.unwrap();
        while start_time(descendant).is_ok() {
            assert!(tokio::time::Instant::now() < deadline, "descendant survived stop");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    #[test]
    fn disk_full_while_copying_the_tail_still_truncates_the_append_inode() {
        let scratch = Scratch::new();
        let path = scratch.0.join("sc-0123456789abcdef.err");
        let output = private_fs::append_private(&path).unwrap();
        output.set_len(LOG_CAP + 1).unwrap();
        let error = trim_log_with(&path, |_, _, _| Err(io::Error::from_raw_os_error(libc::ENOSPC))).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
        assert_eq!(output.metadata().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn pending_environment_does_not_lock_listing_or_shutdown() {
        let scratch = short_scratch();
        let ready_manager = manager(&scratch);
        let manager = Arc::new(Sidecars::new(ready_manager.paths.clone()).unwrap());
        let task = {
            let manager = manager.clone();
            let root = scratch.0.to_str().unwrap().to_owned();
            tokio::spawn(async move {
                manager
                    .start(StartSidecarRequest {
                        root,
                        account: String::new(),
                        env: Default::default(),
                        command_id: "waiting".into(),
                        sidecar_revision: "test".into(),
                        sidecar_sha256: "a".repeat(64),
                    })
                    .await
            })
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while manager.env.receiver_count() == 0 {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::task::yield_now().await;
        }
        assert!(manager.list().sidecars.is_empty());
        manager.stop_all().await.unwrap();
        assert_eq!(task.await.unwrap().unwrap_err().code(), tonic::Code::Unavailable);
    }
}
