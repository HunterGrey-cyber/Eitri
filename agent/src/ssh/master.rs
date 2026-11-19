use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use std::{fs, io};

use sha2::{Digest, Sha256};

use super::{ConnectError, HostKey, CHANNEL_OPTIONS};

#[derive(Clone, Debug)]
pub struct ControlMaster {
    pub host: String,
    pub control_path: PathBuf,
    pub host_key_record: PathBuf,
    eitri_exe: PathBuf,
    shared_terminal: bool,
}

/// What follows `cm-<id>` and `kh-<id>` in the names of a host's second master, the one a window's terminals use.
pub const TERMINAL_SUFFIX: &str = "-t";

impl ControlMaster {
    /// The host's master: `cm-<id>` and `kh-<id>` in `private_dir`, started unless one already answers its check.
    /// Blocking.
    pub fn ensure(host: &str, private_dir: &Path, eitri_exe: &Path) -> Result<Self, ConnectError> {
        let master = Self::named(host, private_dir, eitri_exe, "")?;
        let _ready = master.ensure_ready()?;
        Ok(master)
    }

    /// The host's second master, for a window's terminals: a TCP connection of its own, so a terminal's output never
    /// queues in front of the agents' channels inside one ssh connection. It lives beside this one (`cm-<id>-t` and
    /// `kh-<id>-t` in the same private directory, its own lock), runs the same executable as its host-key callback
    /// and starts from the same [`Self::master_argv`], so host-key checking, batch mode, forwarding and identity are
    /// exactly this master's. Started unless it already answers its check; restarting it later goes through the same
    /// argv. The key it shows is compared with nothing here: callers use [`Self::require_host_key`] before any
    /// channel. Blocking.
    pub fn ensure_terminal(&self) -> Result<Self, ConnectError> {
        let master = self.terminal_master()?;
        let _ready = master.ensure_ready()?;
        Ok(master)
    }

    /// Ensures the host's shared terminal master and checks the project's pin under the same lock. A reused
    /// master may carry other projects' channels. If the key check fails, close only a master started here.
    pub fn ensure_terminal_with_key(&self, expected: &str) -> io::Result<Self> {
        let connecting =
            |error: ConnectError| io::Error::other(format!("connecting the terminals to {}: {error}", self.host));
        let refusing =
            |error: io::Error| io::Error::new(error.kind(), format!("refusing the terminals' connection: {error}"));
        let master = self.terminal_master().map_err(connecting)?;
        let (_lock, started) = master.ensure_ready().map_err(connecting)?;
        let verified = super::known_hosts::read_key(&master.host_key_record)
            .and_then(|shown| master.require_key(&shown, expected));
        if let Err(error) = verified {
            if started {
                if let Err(exit) = master.exit_locked() {
                    eprintln!(
                        "[remote] {}: the refused terminals' master did not close: {exit}",
                        master.host
                    );
                }
            }
            return Err(refusing(error));
        }
        Ok(master)
    }

    fn terminal_master(&self) -> Result<Self, ConnectError> {
        let private_dir = self
            .control_path
            .parent()
            .ok_or_else(|| "the SSH control path has no private directory".to_owned())?;
        Self::named(&self.host, private_dir, &self.eitri_exe, TERMINAL_SUFFIX)
    }

    fn named(host: &str, private_dir: &Path, eitri_exe: &Path, suffix: &str) -> Result<Self, ConnectError> {
        super::validate_host(host)?;
        super::validate_state_path(private_dir)?;
        // Keep an absolute executable so a later worker's cwd cannot change the callback.
        if !eitri_exe.is_absolute() {
            return Err("the Eitri executable path must be absolute".to_owned().into());
        }
        let id = format!("{:x}", Sha256::digest(host.as_bytes()));
        let id = &id[..16];
        let master = Self {
            host: host.to_owned(),
            control_path: crate::socket_path::in_dir(private_dir, &format!("cm-{id}{suffix}"))?,
            host_key_record: private_dir.join(format!("kh-{id}{suffix}")),
            eitri_exe: eitri_exe.to_owned(),
            shared_terminal: suffix == TERMINAL_SUFFIX,
        };
        master.validate_control_path()?;
        if suffix.is_empty() {
            // The terminals' master lives beside this one under a longer name: a directory where only that name is
            // too long is refused here, when the agents' master is made, not later at a window's first terminal.
            let terminals = Self {
                control_path: crate::socket_path::in_dir(private_dir, &format!("cm-{id}{TERMINAL_SUFFIX}"))?,
                ..master.clone()
            };
            terminals.validate_control_path().map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("{error} (the terminals' master, cm-{id}{TERMINAL_SUFFIX}, needs this too)"),
                )
            })?;
        }
        super::known_hosts::command(eitri_exe, &master.host_key_record)?;
        super::private_dir(private_dir)?;
        Ok(master)
    }

    fn ensure_ready(&self) -> Result<(crate::private_fs::PrivateLock, bool), ConnectError> {
        let lock = super::lock(&self.control_path.with_extension("lock"))?;
        super::check_openssh()?;
        let started = self.ensure_locked()?;
        Ok((lock, started))
    }

    fn ensure_locked(&self) -> Result<bool, ConnectError> {
        let check = super::run(&mut self.operation("check"), Duration::from_secs(2))?;
        // A slow check may still have a live master behind it, carrying other windows' terminals.
        if check.timed_out && self.shared_terminal {
            return Err(check.connect_error(&self.host));
        }
        if !check.timed_out && check.status.success() {
            return Ok(false);
        }
        self.remove_stale_socket()?;
        match fs::remove_file(&self.host_key_record) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut command = Command::new("ssh");
        command.args(self.master_argv()?);
        // The known-hosts probe may have written the offered key before ssh checked it: a start that failed in any
        // way leaves no record.
        let output = match super::run(&mut command, Duration::from_secs(30)) {
            Ok(output) => output,
            Err(error) => {
                let _ = fs::remove_file(&self.host_key_record);
                return Err(error.into());
            }
        };
        if !output.status.success() || output.timed_out {
            let _ = fs::remove_file(&self.host_key_record);
            return Err(output.connect_error(&self.host));
        }
        let ready = super::run(&mut self.operation("check"), Duration::from_secs(2))?;
        if ready.timed_out {
            // The background master may already be serving channels. Its key is needed on the next check.
            return Err(ready.connect_error(&self.host));
        }
        if !ready.status.success() {
            let _ = fs::remove_file(&self.host_key_record);
            return Err("ssh master did not become ready".to_owned().into());
        }
        Ok(true)
    }

    fn validate_control_path(&self) -> io::Result<()> {
        super::validate_state_path(&self.control_path)?;
        // OpenSSH binds a temporary name before renaming it to ControlPath.
        const TEMPORARY_SUFFIX_BYTES: usize = 17;
        let limit = crate::socket_path::MAX_SOCKET_PATH_BYTES - TEMPORARY_SUFFIX_BYTES;
        if self.control_path.as_os_str().len() > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("SSH control path must be at most {limit} bytes to leave room for its temporary bind suffix"),
            ));
        }
        Ok(())
    }

    fn remove_stale_socket(&self) -> io::Result<()> {
        let parent = self
            .control_path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "SSH socket needs a private directory"))?;
        if !crate::private_fs::is_private_dir(parent) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "SSH socket needs a private real directory",
            ));
        }
        let meta = match fs::symlink_metadata(&self.control_path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if !meta.file_type().is_socket() || meta.uid() != crate::private_fs::euid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "stale SSH control path is not a socket owned by this user",
            ));
        }
        let directory = crate::private_fs::open_dir(parent)?;
        let removed = crate::private_fs::remove_if_same(
            &directory,
            self.control_path.file_name().expect("socket name"),
            crate::private_fs::FileIdentity {
                dev: meta.dev(),
                ino: meta.ino(),
            },
        )?;
        if !removed {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "stale SSH socket changed during cleanup",
            ));
        }
        Ok(())
    }

    /// Prove the network path works by completing one fixed command through the master.
    pub fn round_trip(&self, deadline: Duration) -> bool {
        let mut command = Command::new("ssh");
        let Ok(argv) = super::step_argv(self, "true", &[]) else {
            return false;
        };
        command.args(argv);
        super::run(&mut command, deadline).is_ok_and(|output| output.status.success() && !output.timed_out)
    }

    pub fn check(&self, deadline: Duration) -> bool {
        if super::validate_host(&self.host).is_err() {
            return false;
        }
        super::run(&mut self.operation("check"), deadline)
            .is_ok_and(|output| output.status.success() && !output.timed_out)
    }

    pub fn restart(&self) -> Result<(), ConnectError> {
        super::validate_host(&self.host)?;
        let _lock = super::lock(&self.control_path.with_extension("lock"))?;
        if self.shared_terminal {
            // A failed network probe cannot justify ending other windows' terminal channels.
            return if self.ensure_locked()? {
                Ok(())
            } else {
                Err("the shared terminal SSH master is running but its network probe failed"
                    .to_owned()
                    .into())
            };
        }
        // A failed check may be a stale network path: close it before trying a new connection.
        let output = super::run(&mut self.operation("exit"), Duration::from_secs(2))?;
        if output.timed_out {
            return Err(output.connect_error(&self.host));
        }
        self.ensure_locked().map(|_| ())
    }

    /// Restarts the master only if it is still dead once the lock is held: another caller (a second
    /// tab, another window) may have brought it back meanwhile, and closing that one would cut every
    /// channel through it. `Ok(false)` when it was alive. The lock is waited for at most `lock_wait`.
    pub fn restart_if_dead(&self, lock_wait: Duration) -> Result<bool, ConnectError> {
        super::validate_host(&self.host)?;
        let _lock = super::lock_within(&self.control_path.with_extension("lock"), lock_wait)?;
        if self.shared_terminal {
            return self.ensure_locked();
        }
        if self.check(Duration::from_secs(2)) {
            return Ok(false);
        }
        let output = super::run(&mut self.operation("exit"), Duration::from_secs(2))?;
        if output.timed_out {
            return Err(output.connect_error(&self.host));
        }
        self.ensure_locked()?;
        Ok(true)
    }

    pub fn verified_host_key(&self) -> io::Result<HostKey> {
        if !self.check(Duration::from_secs(2)) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "SSH master is not connected",
            ));
        }
        super::known_hosts::read_key(&self.host_key_record)
    }

    /// Closes this master (`-O exit`) under its lock, ending every channel through it; the next ensure starts a new
    /// one. Blocking.
    pub fn exit(&self) -> Result<(), ConnectError> {
        super::validate_host(&self.host)?;
        let _lock = super::lock(&self.control_path.with_extension("lock"))?;
        self.exit_locked()
    }

    fn exit_locked(&self) -> Result<(), ConnectError> {
        let output = super::run(&mut self.operation("exit"), Duration::from_secs(2))?;
        if !output.status.success() || output.timed_out {
            return Err(output.connect_error(&self.host));
        }
        Ok(())
    }

    /// Refuses this master unless the key it verified is `expected`, the SHA-256 fingerprint a project was recorded
    /// with: `PermissionDenied` naming both keys, or why none could be read. A different key is never taken as the
    /// record's and nothing is rewritten; the caller opens no channel over this master. Blocking.
    pub fn require_host_key(&self, expected: &str) -> io::Result<()> {
        let shown = self.verified_host_key()?;
        self.require_key(&shown, expected)
    }

    fn require_key(&self, shown: &HostKey, expected: &str) -> io::Result<()> {
        if shown.sha256 != expected {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} shows host key {}, not the {expected} this project was recorded with",
                    self.host, shown.sha256
                ),
            ));
        }
        Ok(())
    }

    fn operation(&self, operation: &str) -> Command {
        let mut command = Command::new("ssh");
        command.arg("-S").arg(&self.control_path);
        command.args(CHANNEL_OPTIONS).args(["-O", operation, "--", &self.host]);
        command
    }

    pub(crate) fn master_argv(&self) -> Result<Vec<std::ffi::OsString>, ConnectError> {
        self.validate_control_path()?;
        let callback = super::known_hosts::command(&self.eitri_exe, &self.host_key_record)?;
        let mut argv: Vec<std::ffi::OsString> = [
            "-f",
            "-N",
            "-o",
            "ControlMaster=yes",
            "-o",
            "ControlPersist=10m",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=4",
            "-o",
            "FingerprintHash=sha256",
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        argv.extend([
            "-S".into(),
            self.control_path.clone().into_os_string(),
            "-o".into(),
            callback,
        ]);
        argv.extend(CHANNEL_OPTIONS.iter().map(Into::into));
        // Strict checking prevents a permissive user Host block from accepting a changed key.
        argv.extend([
            "-o".into(),
            "StrictHostKeyChecking=yes".into(),
            "-o".into(),
            "VerifyHostKeyDNS=no".into(),
            "--".into(),
            self.host.clone().into(),
        ]);
        Ok(argv)
    }

    #[cfg(test)]
    pub(crate) fn fixture(host: &str, control_path: &Path, record: &Path) -> Self {
        Self {
            host: host.to_owned(),
            control_path: control_path.to_owned(),
            host_key_record: record.to_owned(),
            eitri_exe: "/eitri".into(),
            shared_terminal: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{assert_channel_options, isolated};
    use super::*;

    #[test]
    fn stale_socket_is_removed_before_starting_a_new_master() {
        isolated(
            "ssh::master::tests::stale_socket_is_removed_before_starting_a_new_master",
            |dir| {
                let master = ControlMaster::ensure("example-host", dir, Path::new("/eitri")).unwrap();
                fs::remove_file(dir.join("live")).unwrap();
                let listener = std::os::unix::net::UnixListener::bind(&master.control_path).unwrap();
                drop(listener);
                ControlMaster::ensure("example-host", dir, Path::new("/eitri")).unwrap();
                assert!(!master.control_path.exists());
                assert_eq!(fs::read_to_string(dir.join("starts")).unwrap(), "start\nstart\n");
            },
        );
    }

    #[test]
    fn stale_cleanup_refuses_regular_files_and_symlinks() {
        isolated(
            "ssh::master::tests::stale_cleanup_refuses_regular_files_and_symlinks",
            |dir| {
                let master = ControlMaster::ensure("example-host", dir, Path::new("/eitri")).unwrap();
                fs::remove_file(dir.join("live")).unwrap();
                let target = dir.join("target");
                fs::write(&target, "keep").unwrap();
                for symlink in [false, true] {
                    if symlink {
                        std::os::unix::fs::symlink(&target, &master.control_path).unwrap();
                    } else {
                        fs::write(&master.control_path, "keep").unwrap();
                    }
                    let error = ControlMaster::ensure("example-host", dir, Path::new("/eitri")).unwrap_err();
                    assert!(error.to_string().contains("socket"), "{error}");
                    assert_eq!(fs::read(&master.control_path).unwrap(), b"keep");
                    fs::remove_file(&master.control_path).unwrap();
                }
                assert_eq!(fs::read(&target).unwrap(), b"keep");
                assert_eq!(fs::read_to_string(dir.join("starts")).unwrap(), "start\n");
            },
        );
    }

    #[test]
    fn master_uses_s_and_reserves_the_temporary_bind_suffix() {
        let master = super::super::tests::master_fixture();
        let argv = master.master_argv().unwrap();
        assert!(argv
            .windows(2)
            .any(|w| w == ["-S", master.control_path.to_str().unwrap()]));
        assert!(!argv.iter().any(|a| a.to_string_lossy().starts_with("ControlPath=")));
        let at_limit = format!("/{}", "a".repeat(85));
        let master = ControlMaster::fixture("example-host", Path::new(&at_limit), Path::new("/unused/kh-test"));
        master.validate_control_path().unwrap();
        let too_long = format!("{at_limit}a");
        let master = ControlMaster::fixture("example-host", Path::new(&too_long), Path::new("/unused/kh-test"));
        assert!(master
            .validate_control_path()
            .unwrap_err()
            .to_string()
            .contains("temporary"));
        let dir = format!("/{}", "a".repeat(70));
        let error = ControlMaster::ensure("example-host", Path::new(&dir), Path::new("/eitri")).unwrap_err();
        assert!(error.to_string().contains("temporary"), "{error}");
    }

    #[test]
    fn expanded_or_tokenized_state_paths_are_refused_before_creating_them() {
        isolated(
            "ssh::master::tests::expanded_or_tokenized_state_paths_are_refused_before_creating_them",
            |dir| {
                for name in ["a b", "a%b", "a${HOME}", "a'b", "a\"b", "a\\b", "a\tb"] {
                    let path = dir.join(name);
                    let error = ControlMaster::ensure("example-host", &path, Path::new("/eitri")).unwrap_err();
                    assert!(error.to_string().contains("SSH path"), "{error}");
                    assert!(!path.exists());
                }
                assert!(!dir.join("argv").exists());
            },
        );
    }

    #[test]
    fn two_starters_make_one_master() {
        isolated("ssh::master::tests::two_starters_make_one_master", |dir| {
            let barrier = std::sync::Barrier::new(2);
            std::thread::scope(|scope| {
                let starters: Vec<_> = (0..2)
                    .map(|_| {
                        scope.spawn(|| {
                            barrier.wait();
                            ControlMaster::ensure("example-host", dir, Path::new("/eitri")).unwrap()
                        })
                    })
                    .collect();
                let masters: Vec<_> = starters.into_iter().map(|thread| thread.join().unwrap()).collect();
                assert_eq!(masters[0].control_path, masters[1].control_path);
            });
            assert_eq!(fs::read_to_string(dir.join("starts")).unwrap(), "start\n");
        });
    }

    #[test]
    fn channel_options_are_on_every_channel_and_the_master() {
        let master = super::super::tests::master_fixture();
        assert_channel_options(&master.master_argv().unwrap());
        assert_channel_options(&super::super::channel_argv(&master, Path::new("/srv/helper"), "probe", &[]).unwrap());
        assert_channel_options(&super::super::step_argv(&master, "cat", &[]).unwrap());
        let argv = master.master_argv().unwrap();
        assert!(argv.iter().any(|arg| arg == "StrictHostKeyChecking=yes"));
        assert!(argv.iter().any(|arg| arg == "VerifyHostKeyDNS=no"));
        assert!(argv.iter().any(|arg| arg == "FingerprintHash=sha256"));
    }

    #[test]
    fn refused_login_shows_the_last_stderr_line() {
        isolated("ssh::master::tests::refused_login_shows_the_last_stderr_line", |dir| {
            fs::write(dir.join("refuse"), "").unwrap();
            let error = ControlMaster::ensure("example-host", dir, Path::new("/eitri")).unwrap_err();
            assert_eq!(error.stderr_line, "example-host: Permission denied (publickey).");
            assert_eq!(error.to_string(), "example-host: Permission denied (publickey).\nEitri needs key-based login to example-host (password and 2FA prompts are not supported yet)");
        });
    }

    #[test]
    fn verified_key_requires_a_live_master_and_a_fresh_record() {
        isolated(
            "ssh::master::tests::verified_key_requires_a_live_master_and_a_fresh_record",
            |dir| {
                let master = ControlMaster::ensure("example-host", dir, Path::new("/eitri")).unwrap();
                assert_eq!(
                    super::super::known_hosts::probe_main(&[
                        master.host_key_record.clone().into(),
                        "key-alias".into(),
                        "ssh-ed25519".into(),
                        "SHA256:example".into()
                    ]),
                    0
                );
                assert_eq!(master.verified_host_key().unwrap().sha256, "SHA256:example");
                fs::remove_file(dir.join("live")).unwrap();
                assert_eq!(
                    master.verified_host_key().unwrap_err().kind(),
                    io::ErrorKind::NotConnected
                );
                master.restart().unwrap();
                assert!(!master.host_key_record.exists());
                assert_eq!(fs::read_to_string(dir.join("starts")).unwrap().lines().count(), 2);
            },
        );
    }

    #[test]
    fn reconnect_and_resume_leave_a_slow_shared_master_running() {
        use crate::ssh::{ChannelLauncher, LinkCheck, SshLauncher};

        isolated(
            "ssh::master::tests::reconnect_and_resume_leave_a_slow_shared_master_running",
            |dir| {
                let agents = ControlMaster::ensure("example-host", dir, Path::new("/eitri")).unwrap();
                let master = agents.ensure_terminal().unwrap();
                let launcher = SshLauncher {
                    master: master.clone(),
                    helper: "/srv/helper".into(),
                };
                let ssh = dir.join("bin/ssh");
                fs::rename(&ssh, dir.join("bin/ssh-fixture")).unwrap();
                fs::write(
                    &ssh,
                    r#"#!/bin/sh
set -eu
state=$EITRI_SSH_FIXTURE
case " $* " in
    *" -O check "*) if [ -f "$state/slow-check" ]; then exec sleep 60; fi ;;
esac
exec "$state/bin/ssh-fixture" "$@"
"#,
                )
                .unwrap();
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();

                fs::write(dir.join("slow-check"), "").unwrap();
                let reconnect = launcher.check_link();
                fs::remove_file(dir.join("slow-check")).unwrap();
                let resume = master.restart();
                assert!(!dir.join("exits").exists(), "a shared master received an exit");
                assert!(matches!(reconnect, LinkCheck::Failed(_)), "{reconnect:?}");
                assert!(resume.is_err(), "{resume:?}");
                assert!(master.check(Duration::from_secs(2)));
                assert_eq!(fs::read_to_string(dir.join("starts")).unwrap(), "start\n");

                fs::remove_file(dir.join("live")).unwrap();
                assert!(matches!(launcher.check_link(), LinkCheck::Restarted));
                assert!(!dir.join("exits").exists());
            },
        );
    }

    #[test]
    fn a_slow_first_readiness_check_keeps_the_shared_masters_key() {
        isolated(
            "ssh::master::tests::a_slow_first_readiness_check_keeps_the_shared_masters_key",
            |dir| {
                let agents = ControlMaster::ensure("example-host", dir, Path::new("/eitri")).unwrap();
                fs::remove_file(dir.join("live")).unwrap();
                let ssh = dir.join("bin/ssh");
                fs::rename(&ssh, dir.join("bin/ssh-fixture")).unwrap();
                fs::write(
                    &ssh,
                    r#"#!/bin/sh
set -eu
state=$EITRI_SSH_FIXTURE
case " $* " in
    *" -O check "*)
        if [ -f "$state/slow-check" ]; then
            rm "$state/slow-check"
            exec sleep 60
        fi ;;
esac
if [ "$1" = -f ]; then
    previous=
    for arg do
        if [ "$previous" = -S ]; then
            umask 077
            printf '%s\n' '["example-host","ssh-ed25519","SHA256:example"]' > "$state/kh-${arg##*/cm-}"
            break
        fi
        previous=$arg
    done
    touch "$state/slow-check"
fi
exec "$state/bin/ssh-fixture" "$@"
"#,
                )
                .unwrap();
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();

                assert!(agents.ensure_terminal_with_key("SHA256:example").is_err());
                let master = agents.ensure_terminal_with_key("SHA256:example").unwrap();
                assert_eq!(master.verified_host_key().unwrap().sha256, "SHA256:example");
                assert_eq!(fs::read_to_string(dir.join("starts")).unwrap(), "start\nstart\n");
                assert!(!dir.join("exits").exists());
            },
        );
    }

    /// The terminals' master sits beside the agents' one under names and a lock of its own, and starts from the same
    /// argv but for its socket and the record of its key.
    #[test]
    fn the_terminal_master_has_its_own_names_and_the_same_options() {
        isolated(
            "ssh::master::tests::the_terminal_master_has_its_own_names_and_the_same_options",
            |dir| {
                let agents = ControlMaster::ensure("example-host", dir, Path::new("/eitri")).unwrap();
                fs::remove_file(dir.join("live")).unwrap();
                let terminals = agents.ensure_terminal().unwrap();
                let name = agents.control_path.file_name().unwrap().to_str().unwrap();
                let id = name.strip_prefix("cm-").unwrap();
                assert_eq!(id.len(), 16);
                assert_eq!(agents.host_key_record, dir.join(format!("kh-{id}")));
                assert_eq!(terminals.control_path, dir.join(format!("cm-{id}-t")));
                assert_eq!(terminals.host_key_record, dir.join(format!("kh-{id}-t")));
                assert_eq!(terminals.host, agents.host);
                assert_eq!(terminals.eitri_exe, agents.eitri_exe);
                assert_eq!(
                    terminals.control_path.with_extension("lock"),
                    dir.join(format!("cm-{id}-t.lock"))
                );
                assert_eq!(fs::read_to_string(dir.join("starts")).unwrap(), "start\nstart\n");
                let started = fs::read_to_string(dir.join("argv")).unwrap();
                assert!(started.contains(&format!("\n-S\n{}\n", terminals.control_path.display())));

                let text = |argv: Vec<std::ffi::OsString>| -> Vec<String> {
                    argv.into_iter().map(|arg| arg.into_string().unwrap()).collect()
                };
                let theirs = text(terminals.master_argv().unwrap());
                assert!(theirs
                    .windows(2)
                    .any(|w| w == ["-S", terminals.control_path.to_str().unwrap()]));
                assert!(theirs.iter().any(|arg| arg.starts_with("KnownHostsCommand=")
                    && arg.contains(terminals.host_key_record.to_str().unwrap())));
                let as_agents: Vec<String> = theirs
                    .iter()
                    .map(|arg| {
                        arg.replace(
                            terminals.control_path.to_str().unwrap(),
                            agents.control_path.to_str().unwrap(),
                        )
                        .replace(
                            terminals.host_key_record.to_str().unwrap(),
                            agents.host_key_record.to_str().unwrap(),
                        )
                    })
                    .collect();
                assert_eq!(as_agents, text(agents.master_argv().unwrap()));
                let channel = super::super::channel_argv(&terminals, Path::new("/srv/helper"), "connect", &[]).unwrap();
                assert_eq!(
                    channel[..2],
                    [
                        std::ffi::OsString::from("-S"),
                        terminals.control_path.clone().into_os_string()
                    ]
                );
                assert_channel_options(&channel);
            },
        );
    }

    /// The terminals' master is refused unless it shows the project's key, or when its key cannot be read; neither
    /// its record nor the agents' is rewritten.
    #[test]
    fn the_terminal_master_is_refused_on_another_key() {
        isolated(
            "ssh::master::tests::the_terminal_master_is_refused_on_another_key",
            |dir| {
                let observe = |record: &Path, key: &str| {
                    assert_eq!(
                        super::super::known_hosts::probe_main(&[
                            record.into(),
                            "example-host".into(),
                            "ssh-ed25519".into(),
                            key.into()
                        ]),
                        0
                    );
                };
                let agents = ControlMaster::ensure("example-host", dir, Path::new("/eitri")).unwrap();
                observe(&agents.host_key_record, "SHA256:example");
                let terminals = agents.ensure_terminal().unwrap();
                assert!(
                    terminals.require_host_key("SHA256:example").is_err(),
                    "no key recorded yet"
                );
                observe(&terminals.host_key_record, "SHA256:other");
                let refused = terminals.require_host_key("SHA256:example").unwrap_err();
                assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
                let why = refused.to_string();
                assert!(why.contains("SHA256:other") && why.contains("SHA256:example"), "{why}");
                let read = |record: &Path| super::super::known_hosts::read_key(record).unwrap().sha256;
                assert_eq!(read(&terminals.host_key_record), "SHA256:other");
                assert_eq!(read(&agents.host_key_record), "SHA256:example");
                observe(&terminals.host_key_record, "SHA256:example");
                terminals.require_host_key("SHA256:example").unwrap();
                fs::remove_file(dir.join("live")).unwrap();
                assert_eq!(
                    terminals.require_host_key("SHA256:example").unwrap_err().kind(),
                    io::ErrorKind::NotConnected
                );
            },
        );
    }

    /// A master closed with `exit` is asked to exit through its socket, and the next ensure starts a new one.
    #[test]
    fn an_exited_master_is_started_afresh() {
        isolated("ssh::master::tests::an_exited_master_is_started_afresh", |dir| {
            let agents = ControlMaster::ensure("example-host", dir, Path::new("/eitri")).unwrap();
            fs::remove_file(dir.join("live")).unwrap();
            let terminals = agents.ensure_terminal().unwrap();
            let before = fs::read_to_string(dir.join("argv")).unwrap().len();
            terminals.exit().unwrap();
            assert_eq!(fs::read_to_string(dir.join("exits")).unwrap(), "exit\n");
            let argv = fs::read_to_string(dir.join("argv")).unwrap();
            assert!(
                argv[before..].contains(&format!("-S\n{}\n", terminals.control_path.display())),
                "{argv}"
            );
            assert!(!dir.join("live").exists());
            agents.ensure_terminal().unwrap();
            assert_eq!(fs::read_to_string(dir.join("starts")).unwrap(), "start\nstart\nstart\n");
        });
    }

    #[test]
    fn a_refused_master_exit_returns_an_error() {
        isolated("ssh::master::tests::a_refused_master_exit_returns_an_error", |dir| {
            let master = ControlMaster::ensure("example-host", dir, Path::new("/eitri")).unwrap();
            let ssh = dir.join("bin/ssh");
            fs::write(&ssh, "#!/bin/sh\necho 'master refused exit' >&2\nexit 255\n").unwrap();
            let error = master.exit().unwrap_err();
            assert_eq!(error.stderr_line, "master refused exit");
        });
    }

    /// A private directory where the agents' socket fits but the terminals' two bytes longer one does not is refused
    /// when the agents' master is made; one byte shorter, both masters start.
    #[test]
    fn the_terminal_socket_length_is_checked_with_the_agents_master() {
        isolated(
            "ssh::master::tests::the_terminal_socket_length_is_checked_with_the_agents_master",
            |dir| {
                // OpenSSH's 17-byte temporary suffix leaves 86 bytes; `/cm-<16 hex>-t` takes 22 of them.
                let padded = |len: usize| {
                    let base = dir.as_os_str().len() + 1;
                    assert!(len > base, "scratch directory too long for this test");
                    dir.join("a".repeat(len - base))
                };
                for len in [65, 66] {
                    let refused = ControlMaster::ensure("example-host", &padded(len), Path::new("/eitri")).unwrap_err();
                    assert!(refused.to_string().contains("terminals' master"), "{len}: {refused}");
                    assert!(!padded(len).exists());
                }
                let fits = padded(64);
                let agents = ControlMaster::ensure("example-host", &fits, Path::new("/eitri")).unwrap();
                fs::remove_file(dir.join("live")).unwrap();
                let terminals = agents.ensure_terminal().unwrap();
                assert_eq!(terminals.control_path.as_os_str().len(), 86);
                assert_eq!(fs::read_to_string(dir.join("starts")).unwrap(), "start\nstart\n");
            },
        );
    }

    #[test]
    fn version_probe_uses_stderr() {
        isolated("ssh::master::tests::version_probe_uses_stderr", |_| {
            assert_eq!(super::super::check_openssh().unwrap(), (10, 0));
        });
    }
}
