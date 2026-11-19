//! Private Mutagen process environment and daemon lifetime. Calls run on workers.

use super::engine::FlushOutcome;
use super::mutagen::{command_error, deadline_after, output_until, COMMAND_CAP};
use super::SyncError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Daemon {
    binary: PathBuf,
    data_dir: PathBuf,
    transport: Option<PathBuf>,
    transport_exe: Option<PathBuf>,
    path: Option<OsString>,
    #[cfg(test)]
    pub(super) test_env: Vec<(OsString, OsString)>,
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
struct EnvironmentIdentity {
    target: Option<ExecutableIdentity>,
    transport: Option<Vec<u8>>,
    scripts: String,
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
struct ExecutableIdentity {
    path: Vec<u8>,
    metadata: Option<[u64; 5]>,
}

impl Daemon {
    pub fn prepare(
        state_home: &Path,
        configured: Option<&Path>,
        transport: Option<PathBuf>,
    ) -> Result<Self, SyncError> {
        if !state_home.is_absolute() || transport.as_ref().is_some_and(|path| !path.is_absolute()) {
            return Err(SyncError::Engine(
                "Mutagen state and transport directories must be absolute".into(),
            ));
        }
        let path = transport_path(transport.as_deref())?;
        let transport_exe = transport
            .as_ref()
            .map(|_| {
                std::env::current_exe()
                    .map(|path| executable_path(&path))
                    .map_err(|error| SyncError::Io(error.to_string()))
            })
            .transpose()?;
        let data_dir = state_home.join("eitri/mutagen");
        agent::socket_path::in_dir(&data_dir.join("daemon"), "daemon.sock")
            .map_err(|e| SyncError::Io(e.to_string()))?;
        let binary = match configured {
            Some(path) if path.is_absolute() => match path.try_exists() {
                Ok(true) => path.to_owned(),
                Ok(false) => {
                    return Err(SyncError::Unsupported(format!(
                        "Mutagen was not found at {}",
                        path.display()
                    )))
                }
                Err(e) => return Err(SyncError::Io(format!("{}: {e}", path.display()))),
            },
            Some(_) => return Err(SyncError::Engine("configured Mutagen path must be absolute".into())),
            None => find_binary()?,
        };
        agent::private_fs::create_private_dir_all(&data_dir, &state_home.join("eitri"))
            .map_err(|e| SyncError::Io(e.to_string()))?;
        Ok(Self {
            binary,
            data_dir,
            transport,
            transport_exe,
            path,
            #[cfg(test)]
            test_env: Vec::new(),
        })
    }

    pub(super) fn has_transport(&self) -> bool {
        self.transport.is_some()
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn command(&self, args: &[OsString]) -> Command {
        let mut command = Command::new(&self.binary);
        command.args(args).stdin(Stdio::null()).current_dir(&self.data_dir);
        #[cfg(test)]
        command.envs(self.test_env.iter().cloned());
        command
            .env("MUTAGEN_DATA_DIRECTORY", &self.data_dir)
            .env("MUTAGEN_DISABLE_AUTOSTART", "1")
            .env_remove("SSH_ASKPASS")
            .env_remove("SSH_ASKPASS_REQUIRE")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("MUTAGEN_SSH_PATH")
            .env_remove("EITRI_MUTAGEN_TRANSPORT_EXE");
        if let Some(transport) = &self.transport {
            command.env("PATH", self.path.as_ref().unwrap());
            command.env("MUTAGEN_SSH_PATH", transport);
            command.env("EITRI_MUTAGEN_TRANSPORT_EXE", self.transport_exe.as_ref().unwrap());
        }
        if args == [OsString::from("daemon"), OsString::from("start")] {
            command.env_remove("MUTAGEN_DISABLE_AUTOSTART");
        }
        command
    }

    pub(super) fn output(&self, args: &[OsString]) -> Result<Output, SyncError> {
        output_until(self, args, deadline_after(COMMAND_CAP)).map_err(command_error)
    }

    pub(super) fn lock(&self, deadline: Option<Instant>) -> Result<agent::private_fs::PrivateLock, SyncError> {
        let lock = agent::private_fs::PrivateLock::open(&self.data_dir.join("eitri-daemon.lock"))
            .map_err(|e| SyncError::Io(e.to_string()))?;
        while !lock.try_lock().map_err(|e| SyncError::Io(e.to_string()))? {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(SyncError::Engine("Mutagen daemon lock timed out".into()));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(lock)
    }

    // The caller holds the data-directory flock and the engine's exclusive control lock.
    pub(super) fn start(&self) -> Result<(), SyncError> {
        self.start_until(deadline_after(COMMAND_CAP)).map_err(command_error)
    }

    pub(super) fn environment_changed(&self) -> Result<bool, SyncError> {
        match agent::private_fs::read_private_to_string(&self.data_dir.join("eitri-daemon-environment")) {
            Ok(old) => {
                let Ok(old) = serde_json::from_str::<EnvironmentIdentity>(&old) else {
                    return Ok(true);
                };
                let recorded = self.with_environment(&old)?;
                let current = recorded.identity()?;
                Ok(current.target.as_ref().is_some_and(|target| target.metadata.is_none()) || old != current)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(SyncError::Io(e.to_string())),
        }
    }

    pub(super) fn start_until(&self, deadline: Option<Instant>) -> Result<(), FlushOutcome> {
        let identity_file = self.data_dir.join("eitri-daemon-environment");
        let changed = self.environment_changed().map_err(super::mutagen::outcome_error)?;
        if changed {
            self.stop_until(deadline)?;
        }
        // A running daemon keeps its original transport target even when another build starts a client.
        let identity = if changed {
            None
        } else {
            agent::private_fs::read_private_to_string(&identity_file)
                .ok()
                .and_then(|text| serde_json::from_str::<EnvironmentIdentity>(&text).ok())
        };
        let identity = identity
            .map(Ok)
            .unwrap_or_else(|| self.identity())
            .map_err(super::mutagen::outcome_error)?;
        let starter = self
            .with_environment(&identity)
            .map_err(super::mutagen::outcome_error)?;
        let start = output_until(&starter, &words(&["daemon", "start"]), deadline)?;
        if !start.status.success() {
            if contains_version_mismatch(&start) {
                self.stop_until(deadline)?;
                super::mutagen::checked_output(&starter, &words(&["daemon", "start"]), deadline)?;
            } else {
                return Err(FlushOutcome::Failed(last_error(&start)));
            }
        }
        let identity = serde_json::to_vec(&identity).map_err(|e| FlushOutcome::Failed(e.to_string()))?;
        agent::private_fs::write_private(&identity_file, &identity).map_err(|e| FlushOutcome::Failed(e.to_string()))
    }

    fn stop_until(&self, deadline: Option<Instant>) -> Result<(), FlushOutcome> {
        let output = output_until(self, &words(&["daemon", "stop"]), deadline)?;
        if output.status.success() || daemon_gone(&output) {
            Ok(())
        } else {
            Err(FlushOutcome::Failed(last_error(&output)))
        }
    }

    pub(super) fn restart(&self) -> Result<(), SyncError> {
        self.stop()?;
        self.start()
    }

    pub fn stop(&self) -> Result<(), SyncError> {
        let output = self.output(&words(&["daemon", "stop"]))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(SyncError::Engine(last_error(&output)))
        }
    }

    fn with_environment(&self, identity: &EnvironmentIdentity) -> Result<Self, SyncError> {
        let mut daemon = self.clone();
        daemon.transport_exe = identity.target.as_ref().map(|target| bytes_path(&target.path));
        daemon.transport = identity.transport.as_deref().map(bytes_path);
        daemon.path = transport_path(daemon.transport.as_deref())?;
        Ok(daemon)
    }

    #[cfg(test)]
    pub(super) fn set_transport_exe(&mut self, executable: &Path) {
        self.transport_exe = Some(executable_path(executable));
    }

    fn identity(&self) -> Result<EnvironmentIdentity, SyncError> {
        let target = self.transport_exe.as_ref().map(|executable| {
            let executable = executable_path(executable);
            // Missing targets require a restart, including an executable unlinked during an upgrade.
            let metadata = std::fs::metadata(&executable).ok().map(|metadata| {
                [
                    metadata.dev(),
                    metadata.ino(),
                    metadata.len(),
                    metadata.mtime() as u64,
                    metadata.mtime_nsec() as u64,
                ]
            });
            ExecutableIdentity {
                path: executable.as_os_str().as_bytes().to_vec(),
                metadata,
            }
        });
        let mut hash = Sha256::new();
        if let Some(transport) = &self.transport {
            hash.update(transport.as_os_str().as_bytes());
            let mut entries = match std::fs::read_dir(transport) {
                Ok(entries) => entries
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| SyncError::Io(e.to_string()))?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(e) => return Err(SyncError::Io(e.to_string())),
            };
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                hash.update(entry.file_name().as_bytes());
                if entry.path().is_file() {
                    hash.update(std::fs::read(entry.path()).map_err(|e| SyncError::Io(e.to_string()))?);
                }
            }
        }
        Ok(EnvironmentIdentity {
            target,
            transport: self.transport.as_ref().map(|path| path.as_os_str().as_bytes().to_vec()),
            scripts: format!("{:x}", hash.finalize()),
        })
    }
}

fn bytes_path(bytes: &[u8]) -> PathBuf {
    OsString::from_vec(bytes.to_vec()).into()
}

fn executable_path(path: &Path) -> PathBuf {
    let bytes = path.as_os_str().as_bytes();
    bytes_path(bytes.strip_suffix(b" (deleted)").unwrap_or(bytes))
}

fn transport_path(transport: Option<&Path>) -> Result<Option<OsString>, SyncError> {
    transport
        .map(|transport| {
            let mut paths = vec![transport.to_owned()];
            paths.extend(absolute_path_entries(&std::env::var_os("PATH").unwrap_or_default()));
            std::env::join_paths(paths).map_err(|error| {
                SyncError::Engine(format!(
                    "invalid Mutagen transport path {}: {error}",
                    transport.display()
                ))
            })
        })
        .transpose()
}

fn find_binary() -> Result<PathBuf, SyncError> {
    let mut unusable = None;
    for directory in absolute_path_entries(&std::env::var_os("PATH").unwrap_or_default()) {
        let path = directory.join("mutagen");
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() && meta.permissions().mode() & 0o111 != 0 => return Ok(path),
            Ok(_) => unusable = Some(format!("{} is not an executable file", path.display())),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(e) => unusable = Some(format!("{}: {e}", path.display())),
        }
    }
    if let Some(why) = unusable {
        return Err(SyncError::Io(why));
    }
    Err(SyncError::Unsupported(
        "Mutagen is not installed; install Mutagen 0.18 or newer".into(),
    ))
}

pub(super) fn words(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

pub(super) fn last_error(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().strip_prefix("Error: ").unwrap_or(line.trim()).to_owned())
        .unwrap_or_else(|| format!("Mutagen exited with {}", output.status))
}

pub(super) fn contains_version_mismatch(output: &Output) -> bool {
    String::from_utf8_lossy(&output.stderr)
        .to_ascii_lowercase()
        .contains("version mismatch")
}

pub(super) fn daemon_gone(output: &Output) -> bool {
    String::from_utf8_lossy(&output.stderr).contains("unable to connect to daemon:")
}

pub(super) fn no_such_session(text: &str) -> bool {
    text.contains("unable to locate requested sessions:") && text.contains("did not match any sessions")
}

pub(super) fn absolute_path_entries(path: &std::ffi::OsStr) -> Vec<PathBuf> {
    std::env::split_paths(path)
        .filter(|entry| entry.is_absolute())
        .collect()
}

pub(super) fn version_tuple(version: &str) -> Option<(u64, u64, u64)> {
    let numeric = version.split(['-', '+']).next()?;
    let mut parts = numeric.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}
