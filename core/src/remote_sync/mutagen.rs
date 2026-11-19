//! Mutagen-backed synchronization, called only from worker threads.

use super::daemon::{
    contains_version_mismatch, daemon_gone, last_error, no_such_session, version_tuple, words, Daemon,
};
use super::engine::*;
use super::mutagen_json::{parse_sessions, role_for_name};
use super::SyncError;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, ChildStdout, Output, Stdio};
use std::sync::{Arc, RwLock, TryLockError, TryLockResult};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub struct MutagenEngine {
    daemon: Arc<Daemon>,
    version: String,
    local_only: bool,
    #[cfg(test)]
    test_cap: Option<Duration>,
    control: Arc<RwLock<()>>,
}

pub const CREATE_CAP: Duration = Duration::from_secs(120);
pub const COMMAND_CAP: Duration = Duration::from_secs(30);

impl MutagenEngine {
    /// Local endpoints only; remote endpoints require Eitri's host-verifying transport.
    pub fn open_local(state_home: &Path, configured: Option<&Path>) -> Result<Self, SyncError> {
        Self::from_daemon(Daemon::prepare(state_home, configured, None)?)
    }

    pub fn open(state_home: &Path, configured: Option<&Path>, transport: &Path) -> Result<Self, SyncError> {
        super::transport::install(state_home, transport).map_err(|error| SyncError::Io(error.to_string()))?;
        Self::from_daemon(Daemon::prepare(state_home, configured, Some(transport.to_owned()))?)
    }

    fn from_daemon(daemon: Daemon) -> Result<Self, SyncError> {
        let output = daemon.output(&words(&["version"]))?;
        if !output.status.success() {
            return Err(SyncError::Engine(last_error(&output)));
        }
        let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let parsed = version_tuple(&version)
            .ok_or_else(|| SyncError::Unsupported(format!("unrecognized Mutagen version: {version}")))?;
        if parsed < (0, 18, 0) || parsed == (0, 18, 0) && version.split('+').next().unwrap().contains('-') {
            return Err(SyncError::Unsupported(format!("Mutagen {version} is older than 0.18")));
        }
        let local_only = !daemon.has_transport();
        Ok(Self {
            daemon: Arc::new(daemon),
            version,
            local_only,
            #[cfg(test)]
            test_cap: None,
            control: Arc::new(RwLock::new(())),
        })
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn data_dir(&self) -> &Path {
        self.daemon.data_dir()
    }

    fn run_until(
        &self,
        args: &[OsString],
        deadline: Option<Instant>,
        recover: bool,
        flock_held: bool,
    ) -> Result<Output, FlushOutcome> {
        if recover && self.daemon.environment_changed().map_err(outcome_error)? {
            let _recovery_lock = if flock_held {
                None
            } else {
                Some(self.daemon.lock(deadline).map_err(outcome_error)?)
            };
            let _control = lock_until(|| self.control.try_write(), deadline)?;
            self.daemon.start_until(deadline)?;
        }
        for attempt in 0..2 {
            let output = {
                let _control = lock_until(|| self.control.try_read(), deadline)?;
                output_until(&self.daemon, args, deadline)?
            };
            if output.status.success() {
                return Ok(output);
            }
            if attempt == 0 && (contains_version_mismatch(&output) || recover && daemon_gone(&output)) {
                let _recovery_lock = if flock_held {
                    None
                } else {
                    Some(self.daemon.lock(deadline).map_err(outcome_error)?)
                };
                let _control = lock_until(|| self.control.try_write(), deadline)?;
                if contains_version_mismatch(&output) {
                    checked_output(&self.daemon, &words(&["daemon", "stop"]), deadline)?;
                }
                self.daemon.start_until(deadline)?;
            } else {
                return Err(FlushOutcome::Failed(last_error(&output)));
            }
        }
        unreachable!()
    }

    fn cap_for(&self, operation: &str) -> Duration {
        #[cfg(test)]
        if let Some(cap) = self.test_cap {
            return cap;
        }
        if matches!(operation, "create" | "resume") {
            CREATE_CAP
        } else {
            COMMAND_CAP
        }
    }

    fn run(&self, args: &[OsString], recover: bool) -> Result<Output, SyncError> {
        let cap = self.cap_for(args.get(1).and_then(|arg| arg.to_str()).unwrap_or(""));
        self.run_until(args, deadline_after(cap), recover, false)
            .map_err(command_error)
    }

    pub fn session_status(&self, name: &str) -> Result<SessionStatus, SyncError> {
        check_name(name)?;
        let args = words(&["sync", "list", "--template", "{{json .}}", name]);
        let output = self.run(&args, false)?;
        let mut sessions = parse_sessions(&output.stdout)?
            .into_iter()
            .filter(|session| session.name == name);
        let session = sessions
            .next()
            .ok_or_else(|| SyncError::Engine(format!("no such session: {name}")))?;
        if sessions.next().is_some() {
            return Err(SyncError::Engine(format!("multiple Mutagen sessions named {name}")));
        }
        Ok(session)
    }

    /// The flock excludes start/create in other processes throughout the list-and-stop transaction.
    pub fn stop_if_idle(&self) -> Result<bool, SyncError> {
        let deadline = deadline_after(COMMAND_CAP);
        let _flock = self.daemon.lock(deadline)?;
        let _control = lock_until(|| self.control.try_write(), deadline).map_err(command_error)?;
        let args = words(&[
            "sync",
            "list",
            "--template",
            "{{json .}}",
            "--label-selector",
            "eitri-key",
        ]);
        let mut output = output_until(&self.daemon, &args, deadline).map_err(command_error)?;
        if !output.status.success() && contains_version_mismatch(&output) {
            checked_output(&self.daemon, &words(&["daemon", "stop"]), deadline).map_err(command_error)?;
            self.daemon.start_until(deadline).map_err(command_error)?;
            output = output_until(&self.daemon, &args, deadline).map_err(command_error)?;
        }
        if !output.status.success() {
            return Err(SyncError::Engine(last_error(&output)));
        }
        if !parse_sessions(&output.stdout)?.is_empty() {
            return Ok(false);
        }
        checked_output(&self.daemon, &words(&["daemon", "stop"]), deadline).map_err(command_error)?;
        Ok(true)
    }

    fn operation(&self, operation: &str, name: &str) -> Result<(), SyncError> {
        check_name(name)?;
        self.run(&words(&["sync", operation, name]), true).map(|_| ())
    }
}

impl SyncEngine for MutagenEngine {
    fn create(&self, spec: &SessionSpec) -> Result<(), SyncError> {
        let args = create_args(spec)?;
        if self.local_only
            && [&spec.alpha, &spec.beta]
                .iter()
                .any(|endpoint| matches!(endpoint, Endpoint::Server { .. }))
        {
            return Err(SyncError::Engine(
                "remote Mutagen endpoints require a transport directory; open_local only accepts local endpoints"
                    .into(),
            ));
        }
        let _flock = self.daemon.lock(deadline_after(self.cap_for("create")))?;
        {
            let _control = lock_until(|| self.control.try_write(), deadline_after(self.cap_for("create")))
                .map_err(command_error)?;
            self.daemon.start()?;
        }
        let list = words(&["sync", "list", "--template", "{{json .}}", &spec.name]);
        match self.run_until(&list, deadline_after(self.cap_for("list")), true, true) {
            Ok(output) if !parse_sessions(&output.stdout)?.is_empty() => {
                return Err(SyncError::Engine(format!(
                    "Mutagen session already exists: {}",
                    spec.name
                )));
            }
            Ok(_) => {}
            Err(FlushOutcome::Failed(error)) if no_such_session(&error) => {}
            Err(error) => return Err(command_error(error)),
        }
        self.run_until(&args, deadline_after(self.cap_for("create")), true, true)
            .map(|_| ())
            .map_err(command_error)
    }

    fn terminate(&self, name: &str) -> Result<(), SyncError> {
        self.operation("terminate", name)
    }
    fn pause(&self, name: &str) -> Result<(), SyncError> {
        self.operation("pause", name)
    }
    fn resume(&self, name: &str) -> Result<(), SyncError> {
        self.operation("resume", name)
    }

    fn status(&self, labels: &Labels) -> Result<Vec<SessionStatus>, SyncError> {
        let selector = label_selector(labels)?;
        let args = words(&[
            "sync",
            "list",
            "--template",
            "{{json .}}",
            "--label-selector",
            &selector,
        ]);
        let expected = expected_session(labels);
        match self.run(&args, false) {
            Ok(output) => {
                let mut sessions = parse_sessions(&output.stdout)?;
                if sessions.is_empty() {
                    if let Some((name, role)) = expected {
                        sessions.push(SessionStatus::disconnected(&name, role, "no such session"));
                    }
                }
                Ok(sessions)
            }
            Err(error) if is_command_timeout(&error) => Err(error),
            Err(error) => match expected {
                Some((name, role)) => Ok(vec![SessionStatus::disconnected(&name, role, error.to_string())]),
                None => Err(error),
            },
        }
    }

    fn flush(&self, name: &str, cap: Duration) -> FlushOutcome {
        if let Err(error) = check_name(name) {
            return FlushOutcome::Failed(error.to_string());
        }
        match self.run_until(&words(&["sync", "flush", name]), deadline_after(cap), true, false) {
            Ok(_) => FlushOutcome::Flushed,
            Err(outcome) => outcome,
        }
    }

    fn monitor(&self, name: &str) -> Result<Box<dyn Iterator<Item = SessionStatus> + Send>, SyncError> {
        check_name(name)?;
        Ok(Box::new(Monitor::spawn(
            self.daemon.clone(),
            self.control.clone(),
            name,
        )?))
    }
}

fn expected_session(labels: &Labels) -> Option<(String, Role)> {
    let key = labels.get("eitri-key")?;
    let role = match labels.get("eitri-role")?.as_str() {
        "tree" => Role::Tree,
        "git" => Role::Git,
        _ => return None,
    };
    Some((format!("eitri-{key}-{}", role.as_str()), role))
}

fn check_name(name: &str) -> Result<(), SyncError> {
    if !valid_label_value(name) {
        return Err(SyncError::Engine(format!("invalid Mutagen session name: {name:?}")));
    }
    Ok(())
}

fn label_selector(labels: &Labels) -> Result<String, SyncError> {
    let mut selector = String::from("eitri-key");
    for (key, value) in labels {
        if !valid_label_value(key) || !valid_label_value(value) {
            return Err(SyncError::Engine(format!("invalid Mutagen label: {key:?}={value:?}")));
        }
        selector.push_str(&format!(",{key}={value}"));
    }
    Ok(selector)
}

pub fn create_args(spec: &SessionSpec) -> Result<Vec<OsString>, SyncError> {
    if spec.name != session_name(&spec.key, spec.role) {
        return Err(SyncError::Engine(format!("invalid Eitri session name: {}", spec.name)));
    }
    let laptop = match (&spec.alpha, &spec.beta, spec.role, spec.mode) {
        (Endpoint::Laptop(root), Endpoint::Server { key, .. }, Role::Tree, Mode::TwoWaySafe) if key == &spec.key => {
            root
        }
        (Endpoint::Server { key, .. }, Endpoint::Laptop(root), Role::Git, Mode::OneWayReplica) if key == &spec.key => {
            root
        }
        // Local endpoints are also useful for exercising the engine without SSH.
        (Endpoint::Laptop(alpha), Endpoint::Laptop(beta), role, mode)
            if (role == Role::Tree && mode == Mode::TwoWaySafe)
                || (role == Role::Git && mode == Mode::OneWayReplica) =>
        {
            if role == Role::Tree {
                alpha
            } else {
                beta
            }
        }
        _ => {
            return Err(SyncError::Engine(
                "sync endpoints or mode do not match the session role".into(),
            ))
        }
    };
    check_root_for_labels(&spec.laptop_root)?;
    let expected_endpoint = if spec.role == Role::Git {
        spec.laptop_root.join(".git")
    } else {
        spec.laptop_root.clone()
    };
    if laptop != &expected_endpoint {
        return Err(SyncError::Engine(
            "laptop endpoint does not match the project root and session role".into(),
        ));
    }
    let expected = project_labels(&spec.key, spec.role, &spec.laptop_root);
    for (key, value) in &expected {
        if spec.labels.get(key) != Some(value) {
            return Err(SyncError::Engine(format!("missing or incorrect Mutagen label: {key}")));
        }
    }
    label_selector(&spec.labels)?;
    let mut args = words(&[
        "sync",
        "create",
        "--name",
        &spec.name,
        "--mode",
        match spec.mode {
            Mode::TwoWaySafe => "two-way-safe",
            Mode::OneWayReplica => "one-way-replica",
        },
    ]);
    if spec.ignore_vcs || spec.role == Role::Tree {
        args.push("--ignore-vcs".into());
    }
    if spec.staging == StagingMode::Neighboring || spec.role == Role::Tree {
        args.push("--stage-mode-beta=neighboring".into());
    }
    for ignore in &spec.ignores {
        if ignore.contains(['\0', '\n']) {
            return Err(SyncError::Engine(format!("invalid Mutagen ignore pattern: {ignore:?}")));
        }
        args.push("-i".into());
        args.push(csv_value(ignore).into());
    }
    if spec.role == Role::Tree && !spec.ignores.iter().any(|ignore| ignore == ".git") {
        args.extend(words(&["-i", &csv_value(".git")]));
    }
    if let Some(mode) = spec.symlink_mode {
        args.push(
            format!(
                "--symlink-mode={}",
                match mode {
                    SymlinkMode::Ignore => "ignore",
                    SymlinkMode::PosixRaw => "posix-raw",
                    SymlinkMode::Portable => "portable",
                }
            )
            .into(),
        );
    }
    for (key, value) in &spec.labels {
        args.extend(words(&["--label", &format!("{key}={value}")]));
    }
    args.push(spec.alpha.render()?);
    args.push(spec.beta.render()?);
    Ok(args)
}

pub(super) fn output_until(
    daemon: &Daemon,
    args: &[OsString],
    deadline: Option<Instant>,
) -> Result<Output, FlushOutcome> {
    if deadline.is_some_and(|d| Instant::now() >= d) {
        return Err(FlushOutcome::TimedOut);
    }
    let mut child = daemon
        .command(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| FlushOutcome::Failed(error.to_string()))?;
    // Drain both pipes while waiting, so a verbose child cannot fill a pipe and stall the deadline.
    let stdout = drain(child.stdout.take().unwrap());
    let stderr = drain(child.stderr.take().unwrap());
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if deadline.is_none_or(|d| Instant::now() < d) => thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                // Child::kill signals this child's pid, and wait reaps that same child.
                let _ = child.kill();
                let _ = child.wait();
                break Err(FlushOutcome::TimedOut);
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(FlushOutcome::Failed(error.to_string()));
            }
        }
    };
    // On timeout, drop the drain handles: a grandchild may still hold the pipe writers.
    let status = status?;
    while !stdout.is_finished() || !stderr.is_finished() {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(FlushOutcome::TimedOut);
        }
        thread::sleep(Duration::from_millis(5));
    }
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    Ok(Output { status, stdout, stderr })
}

pub(super) fn deadline_after(cap: Duration) -> Option<Instant> {
    // None represents a deadline beyond Instant's range, rather than an already elapsed deadline.
    Instant::now().checked_add(cap)
}

pub(super) fn command_error(outcome: FlushOutcome) -> SyncError {
    SyncError::Engine(match outcome {
        FlushOutcome::TimedOut => "Mutagen command timed out".into(),
        FlushOutcome::Failed(error) => error,
        FlushOutcome::Flushed => "unexpected flush outcome".into(),
    })
}

fn lock_until<T>(mut acquire: impl FnMut() -> TryLockResult<T>, deadline: Option<Instant>) -> Result<T, FlushOutcome> {
    loop {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(FlushOutcome::TimedOut);
        }
        match acquire() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(_)) => return Err(FlushOutcome::Failed("Mutagen control lock poisoned".into())),
            Err(TryLockError::WouldBlock) => thread::sleep(Duration::from_millis(5)),
        }
    }
}

fn is_command_timeout(error: &SyncError) -> bool {
    matches!(error, SyncError::Engine(text) if matches!(text.as_str(), "Mutagen command timed out" | "Mutagen daemon lock timed out"))
}

pub(super) fn outcome_error(error: SyncError) -> FlushOutcome {
    if is_command_timeout(&error) {
        FlushOutcome::TimedOut
    } else {
        FlushOutcome::Failed(error.to_string())
    }
}

pub(super) fn checked_output(
    daemon: &Daemon,
    args: &[OsString],
    deadline: Option<Instant>,
) -> Result<Output, FlushOutcome> {
    let output = output_until(daemon, args, deadline)?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(FlushOutcome::Failed(last_error(&output)))
    }
}

fn check_root_for_labels(root: &Path) -> Result<(), SyncError> {
    Endpoint::Laptop(root.to_owned()).render().map(|_| ())
}

fn csv_value(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn drain(mut pipe: impl Read + Send + 'static) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        bytes
    })
}

struct Monitor {
    daemon: Arc<Daemon>,
    control: Arc<RwLock<()>>,
    child: Child,
    stdout: BufReader<ChildStdout>,
    stderr: Option<JoinHandle<Vec<u8>>>,
    pending: VecDeque<SessionStatus>,
    name: String,
    done: bool,
    restarted: bool,
}

impl Monitor {
    fn spawn(daemon: Arc<Daemon>, control: Arc<RwLock<()>>, name: &str) -> Result<Self, SyncError> {
        let mut child = daemon
            .command(&words(&["sync", "monitor", "--template", "{{json .}}", name]))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| SyncError::Engine(e.to_string()))?;
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let stderr = Some(drain(child.stderr.take().unwrap()));
        Ok(Self {
            daemon,
            control,
            child,
            stdout,
            stderr,
            pending: VecDeque::new(),
            name: name.into(),
            done: false,
            restarted: false,
        })
    }

    fn disconnected(&self, reason: impl Into<String>) -> SessionStatus {
        SessionStatus::disconnected(&self.name, role_for_name(&self.name), reason)
    }
}

impl Iterator for Monitor {
    type Item = SessionStatus;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(status) = self.pending.pop_front() {
                return Some(status);
            }
            if self.done {
                return None;
            }
            let mut line = String::new();
            match self.stdout.read_line(&mut line) {
                Ok(0) => {
                    self.done = true;
                    let status = self.child.wait().ok();
                    let stderr = self
                        .stderr
                        .take()
                        .and_then(|reader| reader.join().ok())
                        .unwrap_or_default();
                    if !self.restarted
                        && String::from_utf8_lossy(&stderr)
                            .to_ascii_lowercase()
                            .contains("version mismatch")
                    {
                        let restarted = {
                            self.daemon.lock(deadline_after(COMMAND_CAP)).ok().and_then(|_flock| {
                                self.control.write().ok().and_then(|_guard| self.daemon.restart().ok())
                            })
                        };
                        if restarted.is_some() {
                            if let Ok(mut monitor) = Self::spawn(self.daemon.clone(), self.control.clone(), &self.name)
                            {
                                monitor.restarted = true;
                                *self = monitor;
                                continue;
                            }
                        }
                    }
                    let reason = if let Some(status) = status.filter(|s| !s.success()) {
                        last_error(&Output {
                            status,
                            stdout: vec![],
                            stderr,
                        })
                    } else {
                        "monitor ended".into()
                    };
                    return Some(self.disconnected(if no_such_session(&reason) {
                        "no such session".into()
                    } else {
                        reason
                    }));
                }
                Ok(_) => match parse_sessions(line.as_bytes()) {
                    Ok(sessions) => {
                        self.pending
                            .extend(sessions.into_iter().filter(|session| session.name == self.name));
                        if self.pending.is_empty() {
                            return Some(self.disconnected("no such session"));
                        }
                    }
                    Err(error) => {
                        self.done = true;
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                        return Some(self.disconnected(error.to_string()));
                    }
                },
                Err(error) => {
                    self.done = true;
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return Some(self.disconnected(error.to_string()));
                }
            }
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // A descendant can retain stderr after the monitor itself has exited.
        self.stderr.take();
    }
}

#[cfg(test)]
mod tests;
