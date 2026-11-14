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
}

impl ControlMaster {
    pub fn ensure(host: &str, private_dir: &Path, eitri_exe: &Path) -> Result<Self, ConnectError> {
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
            control_path: crate::socket_path::in_dir(private_dir, &format!("cm-{id}"))?,
            host_key_record: private_dir.join(format!("kh-{id}")),
            eitri_exe: eitri_exe.to_owned(),
        };
        master.validate_control_path()?;
        super::known_hosts::command(eitri_exe, &master.host_key_record)?;
        super::private_dir(private_dir)?;
        let _lock = super::lock(&master.control_path.with_extension("lock"))?;
        super::check_openssh()?;
        master.ensure_locked()?;
        Ok(master)
    }

    fn ensure_locked(&self) -> Result<(), ConnectError> {
        if self.check(Duration::from_secs(2)) {
            return Ok(());
        }
        self.remove_stale_socket()?;
        match fs::remove_file(&self.host_key_record) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut command = Command::new("ssh");
        command.args(self.master_argv()?);
        let output = super::run(&mut command, Duration::from_secs(30))?;
        if !output.status.success() || output.timed_out {
            let _ = fs::remove_file(&self.host_key_record);
            return Err(output.connect_error(&self.host));
        }
        if !self.check(Duration::from_secs(2)) {
            let _ = fs::remove_file(&self.host_key_record);
            return Err("ssh master did not become ready".to_owned().into());
        }
        Ok(())
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
        // A failed check may be a stale network path: close it before trying a new connection.
        let output = super::run(&mut self.operation("exit"), Duration::from_secs(2))?;
        if output.timed_out {
            return Err(output.connect_error(&self.host));
        }
        self.ensure_locked()
    }

    /// Restarts the master only if it is still dead once the lock is held: another caller (a second
    /// tab, another window) may have brought it back meanwhile, and closing that one would cut every
    /// channel through it. `Ok(false)` when it was alive. The lock is waited for at most `lock_wait`.
    pub fn restart_if_dead(&self, lock_wait: Duration) -> Result<bool, ConnectError> {
        super::validate_host(&self.host)?;
        let _lock = super::lock_within(&self.control_path.with_extension("lock"), lock_wait)?;
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
    fn version_probe_uses_stderr() {
        isolated("ssh::master::tests::version_probe_uses_stderr", |_| {
            assert_eq!(super::super::check_openssh().unwrap(), (10, 0));
        });
    }
}
