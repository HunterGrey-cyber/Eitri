//! A remote project's window reaching its server: one connection to the helper, opened for the host key the
//! project was recorded with, and the facts of the installation the helper runs. A window's remote tabs start,
//! come back and continue through it, and stops queued while the server was out of reach go out through it.
//! Every call here crosses the network or the disk: a worker's.

#[cfg(test)]
use std::path::Path;
use std::sync::Arc;

use agent::remote_client::{self as wire, HelperClient, HelperConnections, SidecarStart};
use agent::setting_sources::ProjectTrust;
use agent::ssh::{ChannelLauncher, ControlMaster, SshLauncher};

use super::{RemoteHost, RemoteProject, RemoteProjects};
use crate::agent_backend::{AgentBackend, BackendError};

/// The SHA-256 fingerprint of the host key `master` verified when it connected (ssh's own record of it).
/// Installation and consent records are kept under this key, the one this connection really shows, never
/// a key taken from elsewhere.
pub fn verified_host_key(master: &ControlMaster) -> Result<String, String> {
    master
        .verified_host_key()
        .map(|key| key.sha256)
        .map_err(|error| format!("the host key {} showed could not be read: {error}", master.host))
}

/// A remote project's connection to its server's helper. Cheap to clone; clones share the connection.
#[derive(Clone)]
pub struct RemoteConnection {
    client: HelperClient,
    project: RemoteProject,
    /// The sidecar directory the helper starts sidecars from, and the SHA-256 it must have.
    sidecar_directory: String,
    sidecar_sha256: String,
}

impl RemoteConnection {
    /// Opens the connection over `master`, which the window's start path has connected (the host's key
    /// checked against the record) and whose helper it has started at installation `version` with the
    /// sidecar of `sidecar_revision`. The host record is read under the key the master verified.
    pub fn open_over_ssh(
        master: &ControlMaster,
        store: &RemoteProjects,
        project: &RemoteProject,
        version: &str,
        sidecar_revision: &str,
    ) -> Result<Self, String> {
        let verified = verified_host_key(master)?;
        let host = store
            .host(&verified)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| {
                format!(
                    "Eitri server was never installed on {} under host key {verified}",
                    project.host
                )
            })?;
        let launcher = Arc::new(SshLauncher {
            master: master.clone(),
            helper: host.data_dir.join(version).join("bin/eitri-server"),
        });
        Self::open(launcher, &verified, project, &host, version, sidecar_revision)
    }

    /// [`Self::open_over_ssh`] over any launcher: `verified_host_key` is the key the connection under it
    /// showed, and `host` the record kept under that key.
    pub fn open(
        launcher: Arc<dyn ChannelLauncher>,
        verified_host_key: &str,
        project: &RemoteProject,
        host: &RemoteHost,
        version: &str,
        sidecar_revision: &str,
    ) -> Result<Self, String> {
        let (sidecar_directory, sidecar_sha256) =
            installed_sidecar(verified_host_key, project, host, version, sidecar_revision)?;
        let launcher = Arc::new(InstalledLauncher {
            base: launcher,
            host: host.clone(),
            installation: version.to_owned(),
            installation_args: Default::default(),
            checked_start: true.into(),
        });
        let client = HelperClient::connect_daemon(launcher, &project.host, Some(&project.host_key_sha256), version)
            .map_err(|error| error.to_string())?
            .with_installations(host.helpers.keys().cloned());
        Ok(Self::with_client(
            client,
            project.clone(),
            sidecar_directory,
            sidecar_sha256,
        ))
    }

    /// A connection already open, with the sidecar directory and hash the helper checks: the way a test
    /// gives a window its connection.
    pub fn with_client(
        client: HelperClient,
        project: RemoteProject,
        sidecar_directory: String,
        sidecar_sha256: String,
    ) -> Self {
        Self {
            client,
            project,
            sidecar_directory,
            sidecar_sha256,
        }
    }

    pub fn project(&self) -> &RemoteProject {
        &self.project
    }

    pub fn host(&self) -> &str {
        &self.project.host
    }

    /// What a new tab asks the helper to start: the root as recorded, the window's account by name, the
    /// installed sidecar, and a command id of its own.
    pub fn sidecar_start(&self, account: Option<&str>) -> SidecarStart {
        SidecarStart {
            root: self.project.root.clone(),
            account: account.map(str::to_string),
            command_id: uuid::Uuid::new_v4().to_string(),
            sidecar_revision: self.sidecar_directory.clone(),
            sidecar_sha256: self.sidecar_sha256.clone(),
        }
    }

    /// Starts a remote Claude tab (`AgentBackend::start_remote`) spending `account`. A sidecar left running by a
    /// failed start whose stop did not go through has its stop kept in the project's record in `store`, where the
    /// window's stop sender (and any later window's) sends it once the server can be reached. Blocking.
    pub fn start_tab(
        &self,
        account: Option<&str>,
        project: ProjectTrust,
        client_tools: Option<agent::ClientToolSet>,
        store: &RemoteProjects,
    ) -> Result<AgentBackend, BackendError> {
        let kept = std::cell::Cell::new(None);
        let started = AgentBackend::start_remote(
            self.client.clone(),
            &self.project.host_key_sha256,
            &self.sidecar_start(account),
            project,
            client_tools,
            |sidecar| kept.set(Some(keep_unstopped(store, &self.project, sidecar))),
        );
        started.map_err(|error| with_kept_stop(error, kept.take(), &self.project.host))
    }

    /// Takes a saved remote tab up again: the sidecar `handoff` names, its history folded afresh from the
    /// journal's start, never a resume. A sidecar the server no longer runs still shows its journal and
    /// then ends as gone, so it can be continued. Blocking.
    pub fn reattach_tab(&self, handoff: &agent::HelperHandoff) -> Result<AgentBackend, BackendError> {
        self.check_handoff(handoff)?;
        AgentBackend::reattach_remote(self.client.clone(), handoff, &self.project.root)
    }

    /// Continues the CLI's own session `provider_session_id` through a new sidecar, with the existing
    /// resume request: a tab whose agent the server no longer runs. A sidecar left running by a failed resume is
    /// kept to be stopped as [`Self::start_tab`] keeps one. Blocking.
    pub fn resume_tab(
        &self,
        provider_session_id: &str,
        account: Option<&str>,
        project: ProjectTrust,
        client_tools: Option<agent::ClientToolSet>,
        store: &RemoteProjects,
    ) -> Result<AgentBackend, BackendError> {
        let kept = std::cell::Cell::new(None);
        let resumed = AgentBackend::resume_remote(
            self.client.clone(),
            &self.project.host_key_sha256,
            &self.sidecar_start(account),
            provider_session_id,
            project,
            client_tools,
            |sidecar| kept.set(Some(keep_unstopped(store, &self.project, sidecar))),
        );
        resumed.map_err(|error| with_kept_stop(error, kept.take(), &self.project.host))
    }

    /// Stops the sidecar `sidecar_id` on the server (a tab closed while connected, a stop queued earlier).
    /// Blocking.
    pub fn stop_sidecar(&self, sidecar_id: &str) -> Result<(), String> {
        self.client.stop_if_known(sidecar_id)
    }

    /// Sends every stop queued in the project's record while the server was out of reach; each leaves the
    /// record once it went. A sidecar the helper no longer knows needs no stop. A stop that does not go out
    /// stays queued and the rest are still sent; the failures come back together. Returns the ids stopped.
    /// Blocking.
    pub fn send_queued_stops(&self, store: &RemoteProjects) -> Result<Vec<String>, String> {
        send_queued_for(store, &self.project, |key| {
            send_queued_stop(key, |daemon, id| self.client.stop_saved(daemon, id))
        })
    }

    fn require_capability(&self, capability: &str) -> Result<(), String> {
        let capabilities = self.client.hello_capabilities().map_err(|error| error.to_string())?;
        check_capability(self.host(), &capabilities, capability)
    }

    /// Runs one git command with streaming stdio. Commands that do not consume stdin close it immediately;
    /// after close, a missing exit status leaves the command's outcome unknown.
    pub fn exec_git(
        &self,
        cwd: &std::path::Path,
        args: &[std::ffi::OsString],
        stdin: Box<dyn std::io::Read + Send>,
        stdout: &mut dyn std::io::Write,
        stderr: &mut dyn std::io::Write,
    ) -> Result<i32, String> {
        self.require_capability(wire::CAP_EXEC)?;
        exec_git_with(
            self.host(),
            cwd,
            args,
            unsafe { libc::isatty(0) } == 1,
            stdin,
            stdout,
            stderr,
            |start| self.client.exec_stream(start).map_err(|e| e.to_string()),
        )
    }

    pub fn sync_ignores(&self, request: wire::SyncIgnoresRequest) -> Result<wire::SyncIgnoresResponse, String> {
        self.require_capability(wire::CAP_SYNC_IGNORES)?;
        self.client.sync_ignores(request).map_err(|error| error.to_string())
    }

    pub fn git_config(&self, request: wire::GitConfigRequest) -> Result<wire::GitConfigResponse, String> {
        self.require_capability(wire::CAP_GIT_CONFIG)?;
        self.client.git_config(request).map_err(|error| error.to_string())
    }

    pub fn replace_file_if(&self, request: wire::ReplaceFileIfRequest) -> Result<wire::ReplaceFileIfResponse, String> {
        self.require_capability(wire::CAP_SYNC_FILES)?;
        self.client.replace_file_if(request).map_err(|error| error.to_string())
    }

    pub fn remove_file_if(&self, request: wire::RemoveFileIfRequest) -> Result<wire::RemoveFileIfResponse, String> {
        self.require_capability(wire::CAP_SYNC_FILES)?;
        self.client.remove_file_if(request).map_err(|error| error.to_string())
    }

    pub fn wait_file_hash(&self, request: wire::WaitFileHashRequest) -> Result<wire::WaitFileHashResponse, String> {
        self.require_capability(wire::CAP_SYNC_FILES)?;
        self.client.wait_file_hash(request).map_err(|error| error.to_string())
    }

    pub fn path_sizes(&self, request: wire::PathSizesRequest) -> Result<wire::PathSizesResponse, String> {
        self.require_capability(wire::CAP_SYNC_FILES)?;
        self.client.path_sizes(request).map_err(|error| error.to_string())
    }

    pub fn project_activity(
        &self,
        request: wire::ProjectActivityRequest,
    ) -> Result<wire::ProjectActivityResponse, String> {
        self.require_capability(wire::CAP_SYNC_FILES)?;
        self.client.project_activity(request).map_err(|error| error.to_string())
    }

    pub fn watch_modes(&self, request: wire::WatchModesRequest) -> Result<wire::WatchModesResponse, String> {
        self.require_capability(wire::CAP_SYNC_MODES)?;
        self.client.watch_modes(request).map_err(|error| error.to_string())
    }

    /// The mode keepers' lines from a fresh Hello (every Hello repeats them; the caller keeps what it showed).
    pub fn mode_lines(&self) -> Result<Vec<crate::remote_sync::sessions::ModeLine>, String> {
        self.require_capability(wire::CAP_SYNC_MODES)?;
        let hello = self.client.hello().map_err(|error| error.to_string())?;
        Ok(hello
            .mode_warnings
            .into_iter()
            .map(|line| crate::remote_sync::sessions::ModeLine {
                root: line.root,
                text: line.text,
                sequence: line.sequence,
                epoch: line.epoch,
            })
            .collect())
    }

    pub fn list_tree(
        &self,
        request: wire::ListTreeRequest,
        entry_limit: usize,
    ) -> Result<Vec<wire::ListTreeEntry>, String> {
        self.require_capability(wire::CAP_SYNC_FILES)?;
        let stream = self.client.list_tree(request).map_err(|error| error.to_string())?;
        self.client
            .collect_sync_stream(stream, entry_limit, usize::MAX, |_| 0)
            .map_err(|error| error.to_string())
    }

    pub fn read_file(
        &self,
        request: wire::ReadFileRequest,
        byte_limit: usize,
    ) -> Result<Vec<wire::ReadFileChunk>, String> {
        self.require_capability(wire::CAP_SYNC_FILES)?;
        let stream = self.client.read_file(request).map_err(|error| error.to_string())?;
        // Even empty chunks cost an entry; bound them as well as the file's data bytes.
        self.client
            .collect_sync_stream(stream, byte_limit.saturating_add(1), byte_limit, |chunk| {
                chunk.data.len()
            })
            .map_err(|error| error.to_string())
    }

    fn check_handoff(&self, handoff: &agent::HelperHandoff) -> Result<(), BackendError> {
        match handoff_refusal(&self.project, handoff) {
            Some(message) => Err(BackendError { message, benign: false }),
            None => Ok(()),
        }
    }
}

fn exec_git_with(
    host: &str,
    cwd: &std::path::Path,
    args: &[std::ffi::OsString],
    is_tty: bool,
    stdin: Box<dyn std::io::Read + Send>,
    stdout: &mut dyn std::io::Write,
    stderr: &mut dyn std::io::Write,
    open: impl FnOnce(wire::ExecStart) -> Result<agent::remote_client::HelperExec, String>,
) -> Result<i32, String> {
    let cwd = cwd
        .to_str()
        .ok_or("remote git cwd is not UTF-8; nothing was run")?
        .to_owned();
    let argv = std::iter::once(Ok("git".to_owned()))
        .chain(args.iter().map(|arg| {
            arg.to_str()
                .map(str::to_owned)
                .ok_or("remote git argument is not UTF-8; nothing was run")
        }))
        .collect::<Result<Vec<_>, _>>()?;
    let unknown = || {
        format!(
            "the outcome on {} is unknown; check with `git status` in the terminal",
            host
        )
    };
    let mut stream = open(wire::ExecStart {
        cwd,
        argv,
        purpose: wire::ExecPurpose::Git as i32,
    })
    .map_err(|_| unknown())?;
    // A TTY must never hold the helper's start barrier open.
    let stdin = git_input(args, is_tty, stdin);
    let exit = git_stream_result(host, stream.copy_stdio(stdin, stdout, stderr))?;
    if !exit.refusal.is_empty() {
        return Err(exit.refusal);
    }
    Ok(if exit.signal != 0 { 128 + exit.signal } else { exit.code })
}

fn git_input(
    args: &[std::ffi::OsString],
    is_tty: bool,
    input: Box<dyn std::io::Read + Send>,
) -> Box<dyn std::io::Read + Send> {
    if crate::remote_sync::git_shim::reads_stdin(args, is_tty) {
        input
    } else {
        Box::new(std::io::empty())
    }
}

fn git_stream_result(host: &str, result: Result<wire::ExecExit, String>) -> Result<wire::ExecExit, String> {
    result.map_err(|_| format!("the outcome on {host} is unknown; check with `git status` in the terminal"))
}

fn check_capability(host: &str, capabilities: &[String], capability: &str) -> Result<(), String> {
    if capabilities.iter().any(|offered| offered == capability) {
        return Ok(());
    }
    Err(format!(
        "the server's Eitri helper on {host} does not offer {capability}"
    ))
}

/// Keeps the stop of `sidecar`, which a failed start left running, in `project`'s record under the identity the
/// sidecar was started for. Another window may hold the record's lock for a moment: a few tries before giving up.
fn keep_unstopped(
    store: &RemoteProjects,
    project: &RemoteProject,
    sidecar: &agent::HelperHandoff,
) -> Result<(), String> {
    let write = || {
        store.write_stop_for(
            &project.root,
            &sidecar.host,
            &sidecar.host_key_sha256,
            &sidecar.stop_key(),
            true,
        )
    };
    let mut written = write();
    for _ in 0..3 {
        match &written {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(200));
                written = write();
            }
            _ => break,
        }
    }
    written.map_err(|error| error.to_string())
}

/// A failed start's error, saying what became of the sidecar it left running: `kept` is the outcome of keeping its
/// stop, `None` when nothing was left running.
fn with_kept_stop(mut error: BackendError, kept: Option<Result<(), String>>, host: &str) -> BackendError {
    match kept {
        None => {}
        Some(Ok(())) => error
            .message
            .push_str(&format!("; its agent on {host} is stopped once {host} can be reached")),
        Some(Err(why)) => error.message.push_str(&format!(
            "; its agent on {host} keeps running there: its stop could not be kept ({why})"
        )),
    }
    error
}

/// Why a saved tab's agent is not taken up through `project`'s connection: it was saved under another host key,
/// or under the same key with another host name than the one this project connects to (the connection is to
/// that name, and a tab saved under another one is taken up only where it was saved).
fn handoff_refusal(project: &RemoteProject, handoff: &agent::HelperHandoff) -> Option<String> {
    if handoff.host_key_sha256 != project.host_key_sha256 {
        return Some(format!(
            "this saved tab's agent runs on {} under another host key than this project's",
            handoff.host
        ));
    }
    (handoff.host != project.host).then(|| {
        format!(
            "this saved tab's agent was saved for the host {}, and this project connects to {} under the same key",
            handoff.host, project.host
        )
    })
}

struct InstalledLauncher {
    base: Arc<dyn ChannelLauncher>,
    host: RemoteHost,
    installation: String,
    installation_args: std::sync::Mutex<std::collections::HashSet<String>>,
    /// The window completed this installation's checked START before opening its first channel.
    checked_start: std::sync::atomic::AtomicBool,
}

impl InstalledLauncher {
    fn helper(&self, installation: &str) -> Result<std::path::PathBuf, std::io::Error> {
        let installation = if installation.is_empty() {
            &self.installation
        } else {
            installation
        };
        if !crate::remote_host::install::safe_name(installation) || !self.host.helpers.contains_key(installation) {
            return Err(std::io::Error::other("no recorded helper installation for this agent"));
        }
        Ok(self.host.data_dir.join(installation).join("bin/eitri-server"))
    }
}

impl ChannelLauncher for InstalledLauncher {
    fn spawn_step(&self, literal: &'static str, args: &[&str]) -> std::io::Result<std::process::Child> {
        self.base.spawn_step(literal, args)
    }
    fn spawn(&self, tail: &[std::ffi::OsString]) -> std::io::Result<std::process::Child> {
        let args = tail
            .get(2)
            .and_then(|s| s.to_str())
            .map(agent::ssh::decode_args)
            .transpose()
            .map_err(std::io::Error::other)?
            .unwrap_or_default();
        let installation = args.get(1).and_then(|s| s.to_str()).unwrap_or(&self.installation);
        let helper = self.helper(installation)?;
        let supports_args = self
            .installation_args
            .lock()
            .expect("helper capabilities")
            .contains(installation);
        let legacy = [
            tail[0].clone(),
            "--args-hex".into(),
            agent::ssh::encode_args(&[]).into(),
        ];
        self.base.spawn_at(&helper, if supports_args { tail } else { &legacy })
    }
    fn helper_hello(&self, installation: &str, capabilities: &[String]) {
        let installation = if installation.is_empty() {
            &self.installation
        } else {
            installation
        };
        let mut supported = self.installation_args.lock().expect("helper capabilities");
        if capabilities
            .iter()
            .any(|cap| cap == agent::remote_client::CAP_INSTALLATION_ARGS)
        {
            supported.insert(installation.to_owned());
        } else {
            supported.remove(installation);
        }
    }
    fn ensure_helper(&self, installation: &str) -> std::io::Result<()> {
        let installation = if installation.is_empty() {
            &self.installation
        } else {
            installation
        };
        if installation == self.installation && self.checked_start.swap(false, std::sync::atomic::Ordering::AcqRel) {
            return Ok(());
        }
        self.helper(installation)?;
        let prefix = format!("{installation}/");
        let revision = self
            .host
            .sidecars
            .keys()
            .find_map(|key| key.strip_prefix(&prefix))
            .ok_or_else(|| std::io::Error::other("no recorded sidecar for this helper installation"))?;
        eprintln!("[remote] checking helper installation {installation} before reconnect");
        crate::remote_host::install::start_recorded_with(self.base.as_ref(), &self.host, installation, revision)
            .map_err(|e| {
                eprintln!("[remote] checked helper restart failed: {e}");
                match &e {
                    crate::remote_host::install::InstallError::Reinstall(why)
                        if why == agent::remote_client::MISSING_INSTALLATION && installation != self.installation =>
                    {
                        std::io::Error::new(std::io::ErrorKind::NotFound, agent::remote_client::MISSING_INSTALLATION)
                    }
                    _ => std::io::Error::other(e.to_string()),
                }
            })
    }
    fn hold_host_key(&self, expected: &str) -> std::io::Result<Box<dyn Send>> {
        self.base.hold_host_key(expected)
    }
    fn check_link(&self) -> agent::ssh::LinkCheck {
        self.base.check_link()
    }
    fn host_key(&self) -> Option<std::io::Result<String>> {
        self.base.host_key()
    }
}

/// The sidecar a remote tab asks the helper to start, as this machine recorded installing it: its directory on the
/// server and its SHA-256. Refused when the connection showed another key than the project's, when the record is
/// kept under another key, when the installation was never consented to, or when no such sidecar was recorded.
fn installed_sidecar(
    verified_host_key: &str,
    project: &RemoteProject,
    host: &RemoteHost,
    version: &str,
    sidecar_revision: &str,
) -> Result<(String, String), String> {
    if verified_host_key != project.host_key_sha256 {
        return Err(format!(
            "{}'s host key is {verified_host_key}, not the {} this project was recorded with",
            project.host, project.host_key_sha256
        ));
    }
    if host.host_key_sha256 != verified_host_key {
        return Err(format!(
            "the installation record for {} is kept under another host key",
            project.host
        ));
    }
    if !host.consented {
        return Err(format!(
            "Eitri server on {} was not installed with your consent",
            project.host
        ));
    }
    let sidecar_sha256 = host
        .sidecars
        .get(&crate::remote_host::install::sidecar_record_key(
            version,
            sidecar_revision,
        ))
        .cloned()
        .ok_or_else(|| {
            format!(
                "no sidecar of revision {sidecar_revision} is recorded as installed on {} for {version}",
                project.host
            )
        })?;
    if sidecar_sha256.len() < 16 || !sidecar_sha256.is_ascii() {
        return Err(format!("the recorded sidecar hash for {} is not a hash", project.host));
    }
    Ok((
        crate::remote_host::install::sidecar_directory(sidecar_revision, &sidecar_sha256),
        sidecar_sha256,
    ))
}

fn send_queued_for(
    store: &RemoteProjects,
    project: &RemoteProject,
    stop: impl FnMut(&str) -> Result<(), String>,
) -> Result<Vec<String>, String> {
    let queued = store.queued_stops_for(project).map_err(|error| error.to_string())?;
    send_each(queued, stop, |id| {
        store
            .write_stop_for(&project.root, &project.host, &project.host_key_sha256, id, false)
            .map_err(|error| error.to_string())
    })
}

fn send_queued_stop(key: &str, mut stop: impl FnMut(&str, &str) -> Result<(), String>) -> Result<(), String> {
    if let Some((daemon, id)) = key.split_once('/').filter(|(daemon, _)| !daemon.is_empty()) {
        return stop(daemon, id);
    }
    stop("", key.trim_start_matches('/'))
}

/// Sends each queued stop with `stop` and, once it went, takes it off the queue with `sent`; one that fails is left
/// queued and the next is tried. The ids stopped, or every failure, together.
fn send_each(
    queued: Vec<String>,
    mut stop: impl FnMut(&str) -> Result<(), String>,
    mut sent: impl FnMut(&str) -> Result<(), String>,
) -> Result<Vec<String>, String> {
    let mut stopped = Vec::new();
    let mut failures = Vec::new();
    for sidecar_id in queued {
        match stop(&sidecar_id).and_then(|()| sent(&sidecar_id)) {
            Ok(()) => stopped.push(sidecar_id),
            Err(why) => failures.push(format!("the stop of {sidecar_id} did not go out: {why}")),
        }
    }
    if failures.is_empty() {
        Ok(stopped)
    } else {
        Err(failures.join("; "))
    }
}

impl HelperConnections for RemoteConnection {
    fn open_for(&self, host: &str, host_key_sha256: &str) -> Option<HelperClient> {
        (host == self.project.host && host_key_sha256 == self.project.host_key_sha256).then(|| self.client.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn missing_capability_names_it_in_the_client_error() {
        let capabilities = vec![wire::CAP_SYNC_FILES.to_string()];
        for capability in [
            wire::CAP_SYNC_IGNORES,
            wire::CAP_GIT_CONFIG,
            wire::CAP_SYNC_MODES,
            wire::CAP_EXEC,
        ] {
            assert_eq!(
                check_capability("example-host", &capabilities, capability).unwrap_err(),
                format!("the server's Eitri helper on example-host does not offer {capability}")
            );
        }
        check_capability("example-host", &capabilities, wire::CAP_SYNC_FILES).unwrap();
    }

    #[test]
    fn queued_stops_belong_to_the_host_key_user_and_port_that_queued_them() {
        let root = crate::test_scratch_dir::ScratchDir::new("remote", "stop-identity");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let mut original = project();
        original.queued_stops = vec!["same-installation/sc-1".into()];
        for (host, key) in [
            ("other-example-host", "SHA256:example"),
            ("user@example-host", "SHA256:example"),
            ("ssh://example-host:2222", "SHA256:example"),
            ("example-host", "SHA256:other"),
        ] {
            let mut replacement = original.clone();
            replacement.host = host.into();
            replacement.host_key_sha256 = key.into();
            store.save(&replacement).unwrap();
            assert!(send_queued_for(&store, &original, |_| panic!("must not send another identity's stops")).is_err());
            assert_eq!(store.find(&original.root).unwrap(), Some(replacement.clone()));
            assert!(store
                .write_stop_for(
                    &original.root,
                    &original.host,
                    &original.host_key_sha256,
                    "sc-old",
                    true
                )
                .is_err());
        }
        store.save(&original).unwrap();
        assert_eq!(
            send_queued_for(&store, &original, |_| Ok(())).unwrap(),
            original.queued_stops
        );
        assert!(store.find(&original.root).unwrap().unwrap().queued_stops.is_empty());
    }
    #[test]
    fn a_stop_acknowledgement_cannot_clear_a_replacement_servers_queue() {
        let root = crate::test_scratch_dir::ScratchDir::new("remote", "stop-ack-identity");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let mut original = project();
        original.queued_stops = vec!["same-installation/sc-1".into()];
        for (host, key) in [
            ("other-example-host", "SHA256:example"),
            ("user@example-host", "SHA256:example"),
            ("ssh://example-host:2222", "SHA256:example"),
            ("example-host", "SHA256:other"),
        ] {
            let mut replacement = original.clone();
            replacement.host = host.into();
            replacement.host_key_sha256 = key.into();
            store.save(&original).unwrap();
            let result = send_queued_for(&store, &original, |_| {
                store.save(&replacement).unwrap();
                Ok(())
            });
            assert!(
                result.is_err(),
                "replacement between send and ack must retain its stops"
            );
            assert_eq!(store.find(&original.root).unwrap(), Some(replacement));
        }
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn reconnect_rechecks_the_recorded_manifest_and_restarts_the_same_installation() {
        let root = crate::test_scratch_dir::ScratchDir::new("remote", "restart-installation");
        let (mut plan, launcher) = crate::remote_host::install::test_plan(&root, "restart");
        let host = crate::remote_host::install::test_host_record(&plan);
        plan.use_host_record(&host).unwrap();
        crate::remote_host::install::install_with(&launcher, &plan, &|_, _| {}).unwrap();
        let route = InstalledLauncher {
            base: Arc::new(launcher),
            host,
            installation: plan.version().into(),
            installation_args: Default::default(),
            checked_start: false.into(),
        };
        route.ensure_helper(plan.version()).unwrap();
        let started = plan.install_path().join("started");
        assert!(started.exists());
        std::fs::remove_file(&started).unwrap();
        route.ensure_helper(plan.version()).unwrap();
        assert!(started.exists());
        std::fs::write(plan.install_path().join("bin/eitri-server"), b"changed").unwrap();
        assert!(route.ensure_helper(plan.version()).is_err());
        assert!(route.ensure_helper("unrecorded").is_err());
        let delegated = route
            .spawn_step("printf delegated", &[])
            .unwrap()
            .wait_with_output()
            .unwrap();
        assert!(delegated.status.success());
        assert_eq!(delegated.stdout, b"delegated");
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_collected_older_installation_is_definitely_missing_but_current_damage_needs_reinstall() {
        let root = crate::test_scratch_dir::ScratchDir::new("remote", "collected-installation");
        let (mut old, launcher) = crate::remote_host::install::test_plan(&root, "old");
        let (new, _) = crate::remote_host::install::test_plan(&root, "new");
        let mut host = crate::remote_host::install::test_host_record(&old);
        let newer = crate::remote_host::install::test_host_record(&new);
        host.helpers.extend(newer.helpers);
        host.sidecars.extend(newer.sidecars);
        old.use_host_record(&host).unwrap();
        crate::remote_host::install::install_with(&launcher, &old, &|_, _| {}).unwrap();
        let route = InstalledLauncher {
            base: Arc::new(launcher),
            host,
            installation: new.version().into(),
            installation_args: Default::default(),
            checked_start: false.into(),
        };
        // Corruption at the same older path is not evidence that its agent has stopped.
        std::fs::write(old.install_path().join("bin/eitri-server"), b"damaged").unwrap();
        assert_ne!(
            route.ensure_helper(old.version()).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        std::fs::remove_dir_all(old.install_path()).unwrap();
        let missing = route.ensure_helper(old.version()).unwrap_err();
        assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(missing.to_string(), agent::remote_client::MISSING_INSTALLATION);
        assert_ne!(
            route.ensure_helper(new.version()).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[test]
    fn installation_arguments_require_a_hello_capability_and_older_helpers_use_their_default() {
        struct Launcher(std::sync::Mutex<Vec<(std::path::PathBuf, Vec<std::ffi::OsString>)>>);
        impl ChannelLauncher for Launcher {
            fn spawn(&self, _: &[std::ffi::OsString]) -> std::io::Result<std::process::Child> {
                unreachable!()
            }
            fn spawn_at(&self, helper: &Path, tail: &[std::ffi::OsString]) -> std::io::Result<std::process::Child> {
                let args = agent::ssh::decode_args(tail[2].to_str().unwrap()).unwrap();
                self.0.lock().unwrap().push((helper.into(), args));
                std::process::Command::new("true").spawn()
            }
        }
        let base = Arc::new(Launcher(Default::default()));
        let route = InstalledLauncher {
            base: base.clone(),
            host: host(true),
            installation: "0.3.0-a-b".into(),
            installation_args: Default::default(),
            checked_start: false.into(),
        };
        let tail = [
            "connect".into(),
            "--args-hex".into(),
            agent::ssh::encode_args(&[std::ffi::OsStr::new("--daemon"), std::ffi::OsStr::new("0.3.0-a-b")]).into(),
        ];
        for capabilities in [
            None,
            Some(Vec::new()),
            Some(vec![agent::remote_client::CAP_INSTALLATION_ARGS.into()]),
            Some(Vec::new()),
        ] {
            if let Some(capabilities) = capabilities {
                route.helper_hello("0.3.0-a-b", &capabilities);
            }
            assert!(route.spawn(&tail).unwrap().wait().unwrap().success());
        }
        let calls = base.0.lock().unwrap();
        for (helper, _) in calls.iter() {
            assert_eq!(helper, &host(true).data_dir.join("0.3.0-a-b/bin/eitri-server"));
        }
        assert!(calls[0].1.is_empty());
        assert!(calls[1].1.is_empty());
        assert_eq!(calls[2].1, [std::ffi::OsString::from("--daemon"), "0.3.0-a-b".into()]);
        assert!(calls[3].1.is_empty());
    }

    #[test]
    fn the_first_window_channel_reuses_the_checked_start_but_recovery_checks_again() {
        struct Never;
        impl ChannelLauncher for Never {
            fn spawn(&self, _: &[std::ffi::OsString]) -> std::io::Result<std::process::Child> {
                panic!("missing manifest must not start a child")
            }
        }
        let route = InstalledLauncher {
            base: Arc::new(Never),
            host: host(true),
            installation: "0.3.0".into(),
            installation_args: Default::default(),
            checked_start: true.into(),
        };
        route.ensure_helper("0.3.0").expect("the window already checked START");
        assert!(
            route.ensure_helper("0.3.0").is_err(),
            "recovery must require the recorded manifest again"
        );
    }

    #[test]
    fn unreachable_legacy_stops_stay_queued_until_ownership_can_be_checked() {
        let root = crate::test_scratch_dir::ScratchDir::new("remote", "legacy-stop");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let mut project = project();
        project.queued_stops = vec!["sc-legacy".into(), "old/sc-gone".into(), "old/sc-offline".into()];
        store.save(&project).unwrap();
        let mut attempts = Vec::new();
        let result = send_each(
            project.queued_stops.clone(),
            |key| {
                send_queued_stop(key, |daemon, id| {
                    attempts.push((daemon.to_owned(), id.to_owned()));
                    if id == "sc-gone" {
                        Ok(())
                    } else {
                        Err("unreachable".into())
                    }
                })
            },
            |key| store.stop_sent(&project.root, key).map_err(|error| error.to_string()),
        );
        assert!(result.unwrap_err().contains("sc-offline"));
        assert_eq!(
            store.find(&project.root).unwrap().unwrap().queued_stops,
            ["sc-legacy", "old/sc-offline"]
        );
        assert_eq!(
            attempts,
            [
                ("".into(), "sc-legacy".into()),
                ("old".into(), "sc-gone".into()),
                ("old".into(), "sc-offline".into())
            ]
        );
    }

    #[test]
    fn a_sidecar_a_failed_start_left_running_has_its_stop_kept_and_said() {
        let root = crate::test_scratch_dir::ScratchDir::new("remote", "unstopped");
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        let project = project();
        store.save(&project).unwrap();
        let sidecar = agent::HelperHandoff {
            daemon: "0.3.0-a-b".into(),
            declared_client_tools: Default::default(),
            host: project.host.clone(),
            host_key_sha256: project.host_key_sha256.clone(),
            sidecar_id: "sc-left".into(),
            session_id: String::new(),
            last_folded_sequence: 0,
        };
        let failed = || BackendError {
            message: "failed to create a Claude session".into(),
            benign: false,
        };
        let kept = keep_unstopped(&store, &project, &sidecar);
        assert_eq!(kept, Ok(()));
        assert_eq!(
            store.queued_stops_for(&project).unwrap(),
            vec!["0.3.0-a-b/sc-left".to_string()]
        );
        let said = with_kept_stop(failed(), Some(kept), &project.host).message;
        assert!(
            said.ends_with("its agent on example-host is stopped once example-host can be reached"),
            "{said}"
        );
        // A sidecar started for another server identity is not kept under this project's record.
        let mut other = sidecar.clone();
        other.host_key_sha256 = "SHA256:other".into();
        let refused = keep_unstopped(&store, &project, &other);
        assert!(refused.is_err());
        assert!(with_kept_stop(failed(), Some(refused), &project.host)
            .message
            .contains("keeps running there"));
        assert_eq!(with_kept_stop(failed(), None, &project.host).message, failed().message);
    }

    fn project() -> RemoteProject {
        RemoteProject {
            version: 1,
            host: "example-host".into(),
            host_key_sha256: "SHA256:example".into(),
            root: "/srv/project".into(),
            helper_version: "0.3.0".into(),
            sync: None,
            created_at: 1,
            queued_stops: Vec::new(),
        }
    }

    fn host(consented: bool) -> RemoteHost {
        RemoteHost {
            version: 1,
            host_key_sha256: "SHA256:example".into(),
            data_dir: "/home/example/.local/share/eitri-server".into(),
            state_dir: "/home/example/.local/state/eitri-server".into(),
            helpers: BTreeMap::from([("0.3.0-a-b".into(), "a".repeat(64))]),
            sidecars: BTreeMap::from([("0.3.0-a-b/abcdef0".into(), "c".repeat(64))]),
            consented,
        }
    }

    /// A saved tab is taken up through the project's connection only when it was saved under the project's key and
    /// host name; each refusal says which differs.
    #[test]
    fn a_saved_tab_is_refused_with_what_differs() {
        let project = project();
        let saved = agent::HelperHandoff {
            host: "example-host".into(),
            host_key_sha256: "SHA256:example".into(),
            sidecar_id: "sc-1".into(),
            session_id: "session-1".into(),
            last_folded_sequence: 0,
            daemon: String::new(),
            declared_client_tools: Default::default(),
        };
        assert_eq!(handoff_refusal(&project, &saved), None);
        let other_key = agent::HelperHandoff {
            host_key_sha256: "SHA256:other".into(),
            ..saved.clone()
        };
        assert!(handoff_refusal(&project, &other_key)
            .unwrap()
            .contains("another host key"));
        let other_name = agent::HelperHandoff {
            host: "example-alias".into(),
            ..saved
        };
        let why = handoff_refusal(&project, &other_name).unwrap();
        assert!(why.contains("example-alias") && why.contains("same key"), "{why}");
        assert!(!why.contains("another host key"), "{why}");
    }

    /// One stop that keeps failing does not hold the others back: each is tried, the ones that went leave the
    /// queue, and the failures come back together.
    #[test]
    fn a_failing_queued_stop_does_not_hold_the_rest_back() {
        let mut queue = vec!["sc-1".to_string(), "sc-2".to_string(), "sc-3".to_string()];
        let result = send_each(
            queue.clone(),
            |id| match id {
                "sc-1" => Err("refused".to_string()),
                _ => Ok(()),
            },
            |id| {
                queue.retain(|queued| queued != id);
                Ok(())
            },
        );
        let why = result.unwrap_err();
        assert!(why.contains("sc-1") && why.contains("refused"), "{why}");
        assert_eq!(queue, ["sc-1"], "the others were sent and left the queue");
        assert_eq!(send_each(Vec::new(), |_| Ok(()), |_| Ok(())), Ok(Vec::new()));
    }

    /// The window's start path starts a remote tab with the sidecar it installed, looked up under the key the
    /// connection really showed; another key, a record kept under another, no consent or no such sidecar refuse.
    #[test]
    fn a_remote_tab_starts_the_sidecar_recorded_under_the_verified_key() {
        let (directory, sha) =
            installed_sidecar("SHA256:example", &project(), &host(true), "0.3.0-a-b", "abcdef0").unwrap();
        assert_eq!(directory, format!("abcdef0-{}", "c".repeat(16)));
        assert_eq!(sha, "c".repeat(64));

        let changed = installed_sidecar("SHA256:other", &project(), &host(true), "0.3.0-a-b", "abcdef0").unwrap_err();
        assert!(
            changed.contains("SHA256:other") && changed.contains("SHA256:example"),
            "{changed}"
        );
        let mut elsewhere = host(true);
        elsewhere.host_key_sha256 = "SHA256:other".into();
        assert!(installed_sidecar("SHA256:example", &project(), &elsewhere, "0.3.0-a-b", "abcdef0").is_err());
        let unasked =
            installed_sidecar("SHA256:example", &project(), &host(false), "0.3.0-a-b", "abcdef0").unwrap_err();
        assert!(unasked.contains("consent"), "{unasked}");
        let missing = installed_sidecar("SHA256:example", &project(), &host(true), "0.3.0-a-b", "1234567").unwrap_err();
        assert!(
            missing.contains("1234567") && missing.contains("example-host"),
            "{missing}"
        );
    }
}

#[cfg(test)]
mod exec_git_tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::Path;
    struct NeverEof;
    impl std::io::Read for NeverEof {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            panic!("stdin must not be read before close");
        }
    }
    #[test]
    fn exec_git_maps_a_stream_ending_after_close_to_unknown_outcome() {
        for (args, tty) in [
            (vec![OsString::from("commit"), "-mrelease".into()], false),
            (vec!["apply".into()], true),
        ] {
            let (stream, closed) = agent::remote_client::HelperExec::test_link_loss_after_close();
            let result = exec_git_with(
                "example-host",
                Path::new("/srv/project"),
                &args,
                tty,
                Box::new(NeverEof),
                &mut vec![],
                &mut vec![],
                |start| {
                    assert_eq!(start.cwd, "/srv/project");
                    Ok(stream)
                },
            );
            closed
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("close must be the first input");
            assert_eq!(
                result.unwrap_err(),
                "the outcome on example-host is unknown; check with `git status` in the terminal"
            );
        }
    }
    #[test]
    fn exec_git_non_utf8_argument_says_nothing_was_run() {
        use std::os::unix::ffi::OsStringExt;
        let result = exec_git_with(
            "example-host",
            Path::new("/srv/project"),
            &["add".into(), OsString::from_vec(vec![255])],
            false,
            Box::new(std::io::empty()),
            &mut vec![],
            &mut vec![],
            |_| panic!("must refuse before opening Exec"),
        );
        assert!(result.unwrap_err().contains("nothing was run"));
    }
}
