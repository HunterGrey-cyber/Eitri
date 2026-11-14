//! Blocking SSH operations for worker threads; the desktop thread only consumes their results.

pub mod channel;
pub mod known_hosts;
pub mod launcher;
pub mod master;
pub mod version;

pub use channel::{channel_argv, step_argv, CHANNEL_OPTIONS};
pub use eitri_server_proto::args_hex::{decode_args, encode_args};
pub use eitri_server_proto::probe::{DaemonInfo, ProbeReport, RootState};
pub use eitri_server_proto::PROTOCOL_MAJOR;
pub use known_hosts::HostKey;
pub use launcher::{ChannelLauncher, LinkCheck, LocalExec, SshLauncher};
pub use master::ControlMaster;
pub use version::check_openssh;

use std::fmt;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectError {
    pub stderr_line: String,
    pub login_hint: Option<String>,
}

impl ConnectError {
    pub fn from_io(error: io::Error) -> Self {
        Self {
            stderr_line: error.to_string(),
            login_hint: None,
        }
    }

    pub fn from_stderr(host: &str, stderr: &str) -> Self {
        let mut line = stderr.lines().last().unwrap_or("ssh failed").to_owned();
        if stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED!") {
            let start = stderr
                .split_inclusive('\n')
                .scan(0, |offset, part| {
                    let start = *offset;
                    *offset += part.len();
                    Some((start, part))
                })
                .find_map(|(offset, part)| {
                    (part.starts_with("@@@@") || part.contains("WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!"))
                        .then_some(offset)
                });
            if let Some(start) = start {
                let warning = &stderr[start..];
                let end = warning
                    .find("Host key verification failed.")
                    .map(|offset| offset + "Host key verification failed.".len())
                    .unwrap_or_else(|| warning.trim_end_matches(['\r', '\n']).len());
                line = warning[..end].to_owned();
            }
        }
        let refused = stderr.lines().any(|line| {
            let refusal = line
                .strip_prefix("Permission denied (")
                .or_else(|| line.split_once(": Permission denied (").map(|(_, methods)| methods));
            refusal.is_some_and(|methods| {
                methods.strip_suffix(").").is_some_and(|methods| {
                    !methods.is_empty()
                        && methods
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b",@._-".contains(&byte))
                })
            })
        });
        Self {
            stderr_line: line,
            login_hint: refused.then(|| {
                format!("Eitri needs key-based login to {host} (password and 2FA prompts are not supported yet)")
            }),
        }
    }
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.stderr_line)?;
        if let Some(hint) = &self.login_hint {
            write!(f, "\n{hint}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConnectError {}

impl From<io::Error> for ConnectError {
    fn from(error: io::Error) -> Self {
        Self::from_io(error)
    }
}

impl From<String> for ConnectError {
    fn from(stderr_line: String) -> Self {
        Self {
            stderr_line,
            login_hint: None,
        }
    }
}

pub fn validate_host(host: &str) -> Result<(), String> {
    if host.is_empty() || host.starts_with('-') || host.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
        return Err(
            "SSH host must be nonempty, must not begin with '-', and must contain no whitespace or control characters"
                .to_owned(),
        );
    }
    Ok(())
}

fn safe_absolute_path(path: &str) -> bool {
    path.starts_with('/')
        && path.len() > 1
        && path
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._/+-".contains(&byte))
}

/// SSH option paths must survive its configuration tokenizer and runtime expansion unchanged.
pub fn validate_state_path(path: &Path) -> io::Result<()> {
    validate_option_path(path, false)
}

fn validate_option_path(path: &Path, allow_percent: bool) -> io::Result<()> {
    if !path.is_absolute()
        || path.to_str().is_none_or(|text| {
            text.chars().any(|ch| {
                ch.is_whitespace() || ch.is_control() || "\"'\\$".contains(ch) || (!allow_percent && ch == '%')
            })
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SSH path must be absolute UTF-8 without whitespace, quotes, backslashes, '$' or expanding '%' tokens",
        ));
    }
    Ok(())
}

pub(crate) fn private_dir(dir: &Path) -> io::Result<()> {
    validate_state_path(dir)?;
    if !dir.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SSH private directory must be absolute",
        ));
    }
    crate::private_fs::require_safe_parent(dir.parent().unwrap_or(dir), "SSH private state")?;
    crate::private_fs::create_private_dir_all(dir, dir)?;
    if !crate::private_fs::is_private_dir(dir) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SSH state needs a private real directory",
        ));
    }
    Ok(())
}

/// [`lock`] waiting at most `wait`; `TimedOut` when another holder keeps it longer.
pub(crate) fn lock_within(path: &Path, wait: Duration) -> io::Result<crate::private_fs::PrivateLock> {
    let lock = crate::private_fs::PrivateLock::open(path)?;
    let deadline = Instant::now() + wait;
    while !lock.try_lock()? {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "another restart of this connection is still running",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(lock)
}

pub(crate) fn lock(path: &Path) -> io::Result<crate::private_fs::PrivateLock> {
    let lock = crate::private_fs::PrivateLock::open(path)?;
    while !lock.try_lock()? {
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(lock)
}

#[doc(hidden)]
pub struct CommandOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}

impl CommandOutput {
    pub fn last_error(&self) -> String {
        String::from_utf8_lossy(&self.stderr)
            .lines()
            .last()
            .map(str::to_owned)
            .unwrap_or_else(|| {
                if self.timed_out {
                    "ssh timed out".to_owned()
                } else {
                    format!("ssh exited with {}", self.status)
                }
            })
    }

    pub fn connect_error(&self, host: &str) -> ConnectError {
        if self.stderr.is_empty() {
            self.last_error().into()
        } else {
            ConnectError::from_stderr(host, &String::from_utf8_lossy(&self.stderr))
        }
    }
}

/// Drain without waiting for EOF: a background SSH master may retain its parent's stderr.
#[doc(hidden)]
pub fn run(command: &mut Command, deadline: Duration) -> io::Result<CommandOutput> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let result = (|| {
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        nonblocking(stdout.as_raw_fd())?;
        nonblocking(stderr.as_raw_fd())?;
        let start = Instant::now();
        let mut out = Vec::new();
        let mut err = Vec::new();
        loop {
            drain(&mut stdout, &mut out)?;
            drain(&mut stderr, &mut err)?;
            if let Some(status) = child.try_wait()? {
                drain(&mut stdout, &mut out)?;
                drain(&mut stderr, &mut err)?;
                return Ok(CommandOutput {
                    status,
                    stdout: out,
                    stderr: err,
                    timed_out: false,
                });
            }
            if start.elapsed() >= deadline {
                let _ = child.kill();
                let status = child.wait()?;
                drain(&mut stdout, &mut out)?;
                drain(&mut stderr, &mut err)?;
                return Ok(CommandOutput {
                    status,
                    stdout: out,
                    stderr: err,
                    timed_out: true,
                });
            }
            std::thread::sleep(Duration::from_millis(10).min(deadline.saturating_sub(start.elapsed())));
        }
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: fd belongs to a live pipe; fcntl reads no pointers.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn drain(reader: &mut impl Read, tail: &mut Vec<u8>) -> io::Result<()> {
    // Bound both retained bytes and work per poll so continuous output cannot hide the deadline.
    let mut bytes = [0; 4096];
    for _ in 0..32 {
        match reader.read(&mut bytes) {
            Ok(0) => break,
            Ok(n) => {
                tail.extend_from_slice(&bytes[..n]);
                if tail.len() > 65536 {
                    tail.drain(..tail.len() - 65536);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;

    pub fn master_fixture() -> ControlMaster {
        ControlMaster::fixture(
            "example-host",
            Path::new("/unused/cm-test"),
            Path::new("/unused/kh-test"),
        )
    }

    pub fn assert_channel_options(argv: &[OsString]) {
        assert!(
            argv.windows(CHANNEL_OPTIONS.len())
                .any(|window| { window.iter().zip(CHANNEL_OPTIONS).all(|(a, b)| a == b) }),
            "{argv:?}"
        );
    }

    /// Each child owns its environment before its body starts any threads.
    pub fn isolated(name: &str, body: impl FnOnce(&Path)) {
        if std::env::var_os("EITRI_SSH_TEST").as_deref() == Some(std::ffi::OsStr::new(name)) {
            body(Path::new(&std::env::var_os("EITRI_SSH_FIXTURE").unwrap()));
            return;
        }
        // Leave room for OpenSSH's temporary socket suffix even under a long driver TMPDIR.
        let dir = loop {
            use std::os::unix::fs::DirBuilderExt;
            let path = std::env::temp_dir().join(&uuid::Uuid::new_v4().simple().to_string()[..6]);
            match std::fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => break path,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create SSH scratch: {error}"),
            }
        };
        struct Clean(std::path::PathBuf);
        impl Drop for Clean {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        crate::private_fs::create_private_dir_all(&dir, &dir).unwrap();
        let _clean = Clean(dir.clone());
        let bin = dir.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let ssh = bin.join("ssh");
        std::fs::write(&ssh, r##"#!/bin/sh
set -eu
state=$EITRI_SSH_FIXTURE
printf '%s\n' BEGIN "$@" END >> "$state/argv"
if [ "$1" = -V ]; then
    echo "${EITRI_SSH_VERSION:-OpenSSH_10.0p2, test}" >&2
    exit 0
fi
op=
previous=
scratch=
for arg do
    case "$arg" in
        UserKnownHostsFile=*) scratch=${arg#UserKnownHostsFile=} ;;
    esac
    if [ "$previous" = -O ]; then op=$arg; fi
    if [ "$previous" = -S ]; then control=$arg; fi
    previous=$arg
done
case "$op" in
    check) test -f "$state/live"; exit $? ;;
    exit) rm -f "$state/live"; echo exit >> "$state/exits"; exit 0 ;;
esac
if [ -f "$state/require-channel" ]; then
    for required in StrictHostKeyChecking=yes UserKnownHostsFile=/dev/null GlobalKnownHostsFile=/dev/null KnownHostsCommand=none VerifyHostKeyDNS=no; do
        found=no
        for arg do if [ "$arg" = "$required" ]; then found=yes; fi; done
        if [ "$found" = no ]; then echo "missing $required" >&2; exit 1; fi
    done
    exit 0
fi
if [ -f "$state/exec-channel" ]; then exec sh -c "$previous"; fi
for arg do
    if [ "$arg" = ControlMaster=yes ] && [ -n "${control:-}" ] && [ -e "$control" ]; then
        echo 'Control socket already exists' >&2; exit 255
    fi
    if [ "$arg" = -S ]; then control_next=yes
    elif [ "${control_next:-}" = yes ]; then control=$arg; control_next=no
    fi
done
if [ -f "$state/refuse" ]; then
    echo 'earlier diagnostic' >&2
    echo 'example-host: Permission denied (publickey).' >&2
    exit 255
fi
echo start >> "$state/starts"
touch "$state/live"
"##).unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env("EITRI_SSH_TEST", name)
            .env("EITRI_SSH_FIXTURE", &dir)
            .env("PATH", std::env::join_paths(paths).unwrap())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "child ran no test"
        );
        assert!(
            output
                .stdout
                .starts_with(format!("\nrunning 1 test\ntest {name} ... ok\n").as_bytes())
                && output.stderr.is_empty(),
            "child printed unsolicited output: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn login_hint_only_describes_server_authentication_refusals() {
        for stderr in [
            "sh: proxy-script: Permission denied",
            "Load key /key: Permission denied",
            "Authentication failed while opening a local proxy",
        ] {
            assert!(ConnectError::from_stderr("example-host", stderr).login_hint.is_none());
        }
        assert!(
            ConnectError::from_stderr("example-host", "example-host: Permission denied (publickey,password).")
                .login_hint
                .is_some()
        );
    }

    #[test]
    fn changed_key_warning_is_preserved_verbatim() {
        let warning = "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @\r\n@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\nOffending RSA key in /keys:3\r\nHost key verification failed.";
        let error = ConnectError::from_stderr("example-host", &format!("earlier diagnostic\n{warning}\r\n"));
        assert_eq!(error.stderr_line, warning);
        assert!(error.login_hint.is_none());
    }

    #[test]
    fn host_beginning_with_a_dash_is_refused() {
        for host in [
            "-oProxyCommand=x",
            "",
            "example host",
            "example\nhost",
            "example\0host",
            "example\u{85}host",
        ] {
            assert!(validate_host(host).is_err(), "{host:?}");
        }
        for host in ["example-host", "user@example-host", "ssh://user@example-host:2222"] {
            validate_host(host).unwrap();
        }
        assert!(ControlMaster::ensure("-bad", Path::new("/unused"), Path::new("/eitri")).is_err());
    }

    #[test]
    fn master_never_sets_strict_host_key_checking_no() {
        let forbidden = ["StrictHostKeyChecking", "=", "no"].concat();
        for entry in std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ssh")).unwrap() {
            let source = std::fs::read_to_string(entry.unwrap().path()).unwrap();
            let compact: String = source.chars().filter(|ch| !ch.is_whitespace()).collect();
            assert!(!compact.contains(&forbidden));
        }
    }

    #[test]
    fn a_timed_out_foreground_child_is_killed_and_reaped() {
        let output = run(Command::new("sh").args(["-c", "exec sleep 60"]), Duration::ZERO).unwrap();
        assert!(output.timed_out);
        assert!(!output.status.success());
    }
}
