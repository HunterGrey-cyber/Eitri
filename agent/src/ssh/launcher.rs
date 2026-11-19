use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use super::{ConnectError, ControlMaster};

/// What checking the network path under a launcher's channels found.
#[derive(Debug)]
pub enum LinkCheck {
    /// Channels start without a shared connection (a local fixture), so there is nothing to check.
    NoMaster,
    /// The connection answered its check.
    Alive,
    /// The connection had gone and a new one started.
    Restarted,
    /// The connection had gone and starting a new one failed.
    Failed(ConnectError),
}

/// `tail` is the helper argv after its executable: subcommand, --args-hex, encoded bytes.
pub trait ChannelLauncher: Send + Sync {
    fn spawn(&self, tail: &[OsString]) -> io::Result<Child>;
    fn spawn_at(&self, _helper: &std::path::Path, tail: &[OsString]) -> io::Result<Child> {
        self.spawn(tail)
    }

    /// Records the capabilities learned through this installation's own default connection.
    fn helper_hello(&self, _installation: &str, _capabilities: &[String]) {}

    /// Runs the recorded installation's checked START when its connection needs recovery.
    fn ensure_helper(&self, _installation: &str) -> io::Result<()> {
        Ok(())
    }

    /// Checks the consented key while excluding master replacement. Callers keep this guard until the channel
    /// confirms attachment, then release it before transfers or command waits. A direct fallback uses strict SSH checks.
    fn hold_host_key(&self, expected: &str) -> io::Result<Box<dyn Send>> {
        check_host_key(self.host_key(), expected)?;
        Ok(Box::new(()))
    }

    /// Checks the connection new channels go through and restarts it when it is gone. Blocking: it
    /// runs on a worker, never on a runtime or the GTK thread.
    fn check_link(&self) -> LinkCheck {
        LinkCheck::NoMaster
    }

    /// The SHA-256 fingerprint of the host key the connection's master verified, when it has one.
    /// Read before every channel of a pinned connection, so it must be cheap.
    fn host_key(&self) -> Option<io::Result<String>> {
        None
    }

    /// [`Self::host_key`] as a finished master start left it. While a start may be under way, when the record
    /// can hold the key a server offered before ssh checked it, `Some(Err)` of kind `WouldBlock`: the key could
    /// not be looked at. Blocking for a moment. A launcher that wraps another forwards this as well as
    /// `host_key`.
    fn settled_host_key(&self) -> Option<io::Result<String>> {
        self.host_key()
    }

    fn spawn_step(&self, _literal: &'static str, _args: &[&str]) -> io::Result<Child> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "launcher does not support installation steps",
        ))
    }
}

pub struct SshLauncher {
    pub master: ControlMaster,
    pub helper: PathBuf,
}

impl ChannelLauncher for SshLauncher {
    fn spawn_at(&self, helper: &std::path::Path, tail: &[OsString]) -> io::Result<Child> {
        Self {
            master: self.master.clone(),
            helper: helper.to_owned(),
        }
        .spawn(tail)
    }

    fn hold_host_key(&self, expected: &str) -> io::Result<Box<dyn Send>> {
        let held = super::lock_within(&self.master.control_path.with_extension("lock"), MASTER_LOCK_WAIT)?;
        check_host_key(self.host_key(), expected)?;
        Ok(Box::new(held))
    }
    fn spawn_step(&self, literal: &'static str, args: &[&str]) -> io::Result<Child> {
        let argv = super::step_argv(&self.master, literal, args)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        pipes(Command::new("ssh").args(argv)).spawn()
    }

    fn spawn(&self, tail: &[OsString]) -> io::Result<Child> {
        let (subcommand, args) = decode_tail(tail)?;
        let args: Vec<_> = args.iter().map(|arg| arg.as_os_str()).collect();
        let argv = super::channel_argv(&self.master, &self.helper, subcommand, &args)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        pipes(Command::new("ssh").args(argv)).spawn()
    }

    fn check_link(&self) -> LinkCheck {
        if self.master.check(MASTER_CHECK) {
            return LinkCheck::Alive;
        }
        match self.master.restart_if_dead(MASTER_LOCK_WAIT) {
            Ok(true) => LinkCheck::Restarted,
            Ok(false) => LinkCheck::Alive,
            Err(error) => LinkCheck::Failed(error),
        }
    }

    /// ssh's own record of the key the master verified; rewritten by every master start.
    fn host_key(&self) -> Option<io::Result<String>> {
        Some(super::known_hosts::read_key(&self.master.host_key_record).map(|key| key.sha256))
    }

    /// Every master start runs under the master's lock, and its known-hosts probe writes the key the server offers
    /// before ssh checks it; a refused start removes the record before it lets the lock go. Read under the lock, the
    /// record is a finished start's: a key ssh accepted, or none.
    fn settled_host_key(&self) -> Option<io::Result<String>> {
        let Ok(_held) = super::lock_within(&self.master.control_path.with_extension("lock"), SETTLED_KEY_WAIT) else {
            return Some(Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "a start of the ssh master is under way",
            )));
        };
        self.host_key()
    }
}

fn check_host_key(observed: Option<io::Result<String>>, expected: &str) -> io::Result<()> {
    if let Some(observed) = observed {
        let observed = observed?;
        if observed != expected {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                super::known_hosts::HostKeyRefusal::Mismatch {
                    expected: expected.to_owned(),
                    observed,
                },
            ));
        }
    }
    Ok(())
}

/// How long a master gets to answer `-O check` before it counts as gone.
pub const MASTER_CHECK: Duration = Duration::from_secs(2);

/// How long a check waits for another caller's restart of the same master before it reports failure.
pub const MASTER_LOCK_WAIT: Duration = Duration::from_secs(5);
/// How long a look at the settled host key waits for a master start to finish; a longer one is not looked at.
const SETTLED_KEY_WAIT: Duration = Duration::from_millis(200);

/// A helper fixture launched directly, without SSH or a network connection.
pub struct LocalExec {
    pub helper: PathBuf,
}

impl ChannelLauncher for LocalExec {
    fn spawn_step(&self, literal: &'static str, args: &[&str]) -> io::Result<Child> {
        super::channel::validate_step(literal, args)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let home = self
            .helper
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "fixture needs a home"))?;
        pipes(
            Command::new("sh")
                .args(["-c", literal, "sh"])
                .args(args)
                .env("HOME", home)
                .current_dir(home),
        )
        .spawn()
    }

    fn spawn(&self, tail: &[OsString]) -> io::Result<Child> {
        let (subcommand, _) = decode_tail(tail)?;
        if subcommand == "list-daemons" {
            return pipes(
                Command::new("sh")
                    .args(["-c", super::channel::LIFECYCLE_COMMAND, "sh"])
                    .arg(&self.helper)
                    .args(tail),
            )
            .spawn();
        }
        pipes(Command::new(&self.helper).args(tail)).spawn()
    }
}

fn pipes(command: &mut Command) -> &mut Command {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
}

fn decode_tail(tail: &[OsString]) -> io::Result<(&str, Vec<OsString>)> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "expected subcommand --args-hex <hex>");
    let [subcommand, flag, hex] = tail else {
        return Err(invalid());
    };
    let subcommand = subcommand.to_str().ok_or_else(invalid)?;
    if flag != "--args-hex" {
        return Err(invalid());
    }
    let args = eitri_server_proto::args_hex::decode_args(hex.to_str().ok_or_else(invalid)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    Ok((subcommand, args))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    /// While another holds the master's lock a start may be under way, and its record may name a key ssh then
    /// refuses: the settled key is read only with the lock free.
    #[test]
    fn the_settled_host_key_is_read_only_while_no_master_start_runs() {
        use std::os::unix::fs::DirBuilderExt;
        let dir = std::env::temp_dir().join(format!("kh-settled-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let record = dir.join("kh-0123456789abcdef");
        std::fs::write(&record, r#"["example-host","ssh-ed25519","SHA256:offered"]"#).unwrap();
        std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o600)).unwrap();
        let control = dir.join("cm-0123456789abcdef");
        let launcher = SshLauncher {
            master: ControlMaster::fixture("example-host", &control, &record),
            helper: "/unused".into(),
        };
        assert_eq!(
            launcher.settled_host_key().map(|key| key.unwrap()),
            Some("SHA256:offered".to_owned())
        );
        let held = crate::private_fs::PrivateLock::open(&control.with_extension("lock")).unwrap();
        assert!(held.try_lock().unwrap());
        assert!(
            matches!(launcher.settled_host_key(), Some(Err(error)) if error.kind() == io::ErrorKind::WouldBlock),
            "read while a start may run"
        );
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn both_launchers_preserve_empty_and_nonempty_helper_arguments() {
        super::super::tests::isolated(
            "ssh::launcher::tests::both_launchers_preserve_empty_and_nonempty_helper_arguments",
            |dir| {
                std::fs::write(dir.join("exec-channel"), "").unwrap();
                let helper = dir.join("helper");
                std::fs::write(&helper, "#!/bin/sh\nprintf '%s\\0' \"$@\"\ncat\n").unwrap();
                std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
                let local = LocalExec { helper: helper.clone() };
                let ssh = SshLauncher {
                    master: super::super::tests::master_fixture(),
                    helper,
                };
                for subcommand in ["probe", "list-daemons"] {
                    for args in [
                        vec![],
                        vec![std::ffi::OsStr::new("/srv/a b/$(x)/*"), std::ffi::OsStr::new("")],
                    ] {
                        let tail = [
                            subcommand.into(),
                            "--args-hex".into(),
                            eitri_server_proto::args_hex::encode_args(&args).into(),
                        ];
                        let outputs: Vec<_> = [&local as &dyn ChannelLauncher, &ssh as &dyn ChannelLauncher]
                            .iter()
                            .map(|launcher| {
                                let mut child = launcher.spawn(&tail).unwrap();
                                child.stdin.take().unwrap().write_all(b"stdin data").unwrap();
                                let output = child.wait_with_output().unwrap();
                                assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
                                output.stdout
                            })
                            .collect();
                        assert_eq!(outputs[0], outputs[1]);
                        assert_eq!(
                            outputs[0].starts_with(super::super::channel::LIFECYCLE_ATTACHED),
                            subcommand == "list-daemons"
                        );
                    }
                }
            },
        );
    }

    /// A master that no longer answers its check is closed and started again before a channel is
    /// tried, and one that answers is left alone.
    #[test]
    fn dead_master_is_checked_and_restarted_before_retrying() {
        super::super::tests::isolated(
            "ssh::launcher::tests::dead_master_is_checked_and_restarted_before_retrying",
            |dir| {
                let master = ControlMaster::ensure("example-host", dir, std::path::Path::new("/eitri")).unwrap();
                let launcher = SshLauncher {
                    master,
                    helper: "/srv/eitri-server".into(),
                };
                assert!(matches!(launcher.check_link(), LinkCheck::Alive));
                assert_eq!(std::fs::read_to_string(dir.join("starts")).unwrap(), "start\n");
                std::fs::remove_file(dir.join("live")).unwrap();
                let before = std::fs::read_to_string(dir.join("argv")).unwrap().len();
                assert!(matches!(launcher.check_link(), LinkCheck::Restarted));
                assert_eq!(std::fs::read_to_string(dir.join("starts")).unwrap(), "start\nstart\n");
                assert_eq!(std::fs::read_to_string(dir.join("exits")).unwrap(), "exit\n");
                let argv = std::fs::read_to_string(dir.join("argv")).unwrap();
                let since = &argv[before..];
                let check = since.find("\ncheck\n").expect("a check");
                let exit = since.find("\nexit\n").expect("an exit");
                let start = since.find("ControlMaster=yes").expect("a start");
                assert!(check < exit && exit < start, "{since}");
                assert!(dir.join("live").exists());
            },
        );
    }

    /// Two checks that both find the master dead start one new master between them, and neither
    /// closes the one the other started.
    #[test]
    fn concurrent_checks_of_a_dead_master_start_it_once() {
        super::super::tests::isolated(
            "ssh::launcher::tests::concurrent_checks_of_a_dead_master_start_it_once",
            |dir| {
                let master = ControlMaster::ensure("example-host", dir, std::path::Path::new("/eitri")).unwrap();
                let launcher = std::sync::Arc::new(SshLauncher {
                    master,
                    helper: "/srv/eitri-server".into(),
                });
                std::fs::remove_file(dir.join("live")).unwrap();
                let checks: Vec<_> = (0..2)
                    .map(|_| {
                        let launcher = std::sync::Arc::clone(&launcher);
                        std::thread::spawn(move || launcher.check_link())
                    })
                    .collect();
                for check in checks {
                    assert!(matches!(check.join().unwrap(), LinkCheck::Alive | LinkCheck::Restarted));
                }
                assert_eq!(std::fs::read_to_string(dir.join("starts")).unwrap(), "start\nstart\n");
                assert_eq!(std::fs::read_to_string(dir.join("exits")).unwrap(), "exit\n");
                // A live master is checked and left alone.
                assert!(matches!(launcher.check_link(), LinkCheck::Alive));
                assert_eq!(std::fs::read_to_string(dir.join("exits")).unwrap(), "exit\n");
            },
        );
    }

    /// A check does not wait without end behind another restart of the same master.
    #[test]
    fn a_check_behind_a_stuck_restart_gives_up() {
        super::super::tests::isolated("ssh::launcher::tests::a_check_behind_a_stuck_restart_gives_up", |dir| {
            let master = ControlMaster::ensure("example-host", dir, std::path::Path::new("/eitri")).unwrap();
            let held = crate::private_fs::PrivateLock::open(&master.control_path.with_extension("lock")).unwrap();
            assert!(held.try_lock().unwrap());
            let launcher = SshLauncher {
                master,
                helper: "/srv/eitri-server".into(),
            };
            std::fs::remove_file(dir.join("live")).unwrap();
            assert!(matches!(launcher.check_link(), LinkCheck::Failed(_)));
            assert_eq!(std::fs::read_to_string(dir.join("starts")).unwrap(), "start\n");
        });
    }

    #[test]
    fn local_exec_uses_the_same_helper_tail_and_piped_stdio() {
        super::super::tests::isolated(
            "ssh::launcher::tests::local_exec_uses_the_same_helper_tail_and_piped_stdio",
            |dir| {
                let helper = dir.join("helper");
                std::fs::write(&helper, "#!/bin/sh\nprintf '%s\\n' \"$@\"\ncat\n").unwrap();
                std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
                let tail = [
                    "exec".into(),
                    "--args-hex".into(),
                    eitri_server_proto::args_hex::encode_args(&[std::ffi::OsStr::new("/srv/a b/$(x)/*")]).into(),
                ];
                let mut child = LocalExec { helper }.spawn(&tail).unwrap();
                child.stdin.take().unwrap().write_all(b"stdin data\n").unwrap();
                let output = child.wait_with_output().unwrap();
                assert!(output.status.success());
                assert_eq!(
                    String::from_utf8(output.stdout).unwrap(),
                    format!("exec\n--args-hex\n{}\nstdin data\n", tail[2].to_str().unwrap())
                );
            },
        );
    }
    #[test]
    fn lifecycle_key_guard_excludes_master_replacement_and_refuses_another_key() {
        super::super::tests::isolated(
            "ssh::launcher::tests::lifecycle_key_guard_excludes_master_replacement_and_refuses_another_key",
            |dir| {
                let master = ControlMaster::ensure("example-host", dir, std::path::Path::new("/eitri")).unwrap();
                assert_eq!(
                    super::super::known_hosts::probe_main(&[
                        master.host_key_record.clone().into_os_string(),
                        "example-host".into(),
                        "ssh-ed25519".into(),
                        "SHA256:example".into()
                    ]),
                    0
                );
                let launcher = SshLauncher {
                    master: master.clone(),
                    helper: "/srv/helper".into(),
                };
                let key = launcher.host_key().unwrap().unwrap();
                let guard = launcher.hold_host_key(&key).unwrap();
                let other = crate::private_fs::PrivateLock::open(&master.control_path.with_extension("lock")).unwrap();
                assert!(!other.try_lock().unwrap());
                drop(guard);
                assert!(other.try_lock().unwrap());
                drop(other);
                assert!(launcher.hold_host_key("SHA256:another").is_err());
            },
        );
    }
}
