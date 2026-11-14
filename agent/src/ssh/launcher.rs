use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use super::ControlMaster;

/// `tail` is the helper argv after its executable: subcommand, --args-hex, encoded bytes.
pub trait ChannelLauncher: Send + Sync {
    fn spawn(&self, tail: &[OsString]) -> io::Result<Child>;

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
}

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
        decode_tail(tail)?;
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
                for args in [
                    vec![],
                    vec![std::ffi::OsStr::new("/srv/a b/$(x)/*"), std::ffi::OsStr::new("")],
                ] {
                    let tail = [
                        "probe".into(),
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
                }
            },
        );
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
}
