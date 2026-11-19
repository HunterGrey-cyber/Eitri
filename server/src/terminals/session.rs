//! A terminal's shell session as processes, ended the conservative way sidecars are: the shell is pinned by a pidfd
//! taken before anything could reap it, and its session's members are looked for only while that pinned shell is
//! alive. A live shell leads its session, so the session id is surely still its own; once the shell is gone the id
//! may have been reused, and nothing is looked for again. A job that outlives its shell, or leaves its session, is not
//! hunted.

use std::io;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::super::sidecars;

/// After the post-kill wait, what is left is reported, not waited for.
const KILL_WAIT: Duration = Duration::from_secs(2);

/// The shell's identity as its host records it in `th-<id>.shell`, so a daemon can end the session when the host
/// cannot be asked to: its pid (the session id) and start time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellRecord {
    pub pid: u32,
    pub start_time: u64,
    /// The boot the pid and start time belong to; a record of another boot, or none, is never pinned.
    #[serde(default)]
    pub boot_id: String,
}

/// One process, pinned, with the start time it was pinned at.
pub struct Pinned {
    pub fd: OwnedFd,
    pub pid: u32,
    pub start: u64,
}

impl Pinned {
    pub fn alive(&self) -> bool {
        !sidecars::exited(&self.fd).unwrap_or(true)
    }
}

/// The recorded shell, pinned, while it still runs as recorded: the same boot, and its start time checked after
/// pinning. Errors leave the identity unconfirmed.
pub fn pin_shell(record: ShellRecord, stat: impl FnOnce(u32) -> io::Result<u64>) -> io::Result<Option<Pinned>> {
    if record.boot_id.is_empty() || record.boot_id != super::current_boot_id()? {
        return Ok(None);
    }
    let fd = match sidecars::pin(record.pid) {
        Ok(fd) => fd,
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return Ok(None),
        Err(error) => return Err(error),
    };
    let start = stat(record.pid);
    if sidecars::exited(&fd)? {
        return Ok(None);
    }
    let start = start?;
    Ok((start == record.start_time).then_some(Pinned {
        fd,
        pid: record.pid,
        start,
    }))
}

/// Session id and start time of `pid`.
fn identity(pid: u32) -> io::Result<(u32, u64)> {
    let stat = std::fs::read(format!("/proc/{pid}/stat"))?;
    let fields = sidecars::stat_fields(&stat)?;
    let session = fields
        .get(3)
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| crate::invalid("invalid process session"))?;
    Ok((session, sidecars::parse_start_time(&stat, false)?))
}

fn skipped(state: &Path, event: &str, pid: u32, what: &str, error: &io::Error) -> bool {
    if matches!(
        error.raw_os_error(),
        Some(libc::ENOENT | libc::ESRCH | libc::EACCES | libc::EPERM)
    ) {
        let _ = crate::audit::record(state, event, &format!("pid={pid} {what}: {error}"));
        true
    } else {
        false
    }
}

/// Adds newly pinned members while the shell still holds the session id. A failed scan keeps the members it
/// already verified, but the caller cannot report that the entire session is gone.
fn members(
    shell: &Pinned,
    found: &mut Vec<Pinned>,
    state: &Path,
    event: &str,
    pin: &impl Fn(u32) -> io::Result<OwnedFd>,
) -> bool {
    if !shell.alive() {
        return true;
    }
    let previous = found.len();
    let result = scan_members(shell, found, state, event, pin);
    if !shell.alive() {
        found.truncate(previous);
    }
    if let Err(error) = result {
        let _ = crate::audit::record(state, event, &format!("pid={} member-scan: {error}", shell.pid));
        return false;
    }
    true
}

fn scan_members(
    shell: &Pinned,
    found: &mut Vec<Pinned>,
    state: &Path,
    event: &str,
    pin: &impl Fn(u32) -> io::Result<OwnedFd>,
) -> io::Result<()> {
    let known = found.len();
    for item in std::fs::read_dir("/proc")? {
        let item = item?;
        let Some(pid) = item.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        if pid == shell.pid || found[..known].iter().any(|member| member.pid == pid) {
            continue;
        }
        let (session, start) = match identity(pid) {
            Ok(identity) => identity,
            Err(error) if skipped(state, event, pid, "member-scan", &error) => continue,
            Err(error) => return Err(error),
        };
        if session != shell.pid || start < shell.start {
            continue;
        }
        let fd = match pin(pid) {
            Ok(fd) => fd,
            Err(error) if skipped(state, event, pid, "member-pin", &error) => continue,
            Err(error) => return Err(error),
        };
        if !sidecars::exited(&fd)? {
            match identity(pid) {
                Ok(checked) if checked == (session, start) => found.push(Pinned { fd, pid, start }),
                Ok(_) => {}
                Err(error) if skipped(state, event, pid, "member-scan", &error) => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

fn send(process: &Pinned, signal: i32, state: &Path, event: &str) {
    if let Err(error) = sidecars::signal(&process.fd, signal) {
        if !skipped(state, event, process.pid, "signal", &error) {
            let _ = crate::audit::record(state, event, &format!("pid={} signal: {error}", process.pid));
        }
    }
}

/// Ends the session `shell` leads: SIGHUP to the shell and the members found while it is alive; after `grace` (or once
/// all have exited) the session is scanned again only if the shell is still alive, and SIGKILL goes to every pinned
/// member still alive and the shell. Every signal goes through a pidfd: no numeric pid or group id is signalled,
/// since the shell can be reaped and its ids reused between any check and the signal. The members of the shell's
/// process group are members of its session, found by the same scan. A shell already gone is signalled nothing new.
/// Answers whether the scans completed and everything pinned is gone.
pub fn end(shell: &Pinned, grace: Duration, state: &Path, event: &str) -> bool {
    end_with_pin(shell, grace, state, event, sidecars::pin)
}

pub(super) fn end_with_pin(
    shell: &Pinned,
    grace: Duration,
    state: &Path,
    event: &str,
    pin: impl Fn(u32) -> io::Result<OwnedFd>,
) -> bool {
    let mut pinned = Vec::new();
    let mut complete = members(shell, &mut pinned, state, event, &pin);
    send(shell, libc::SIGHUP, state, event);
    for member in &pinned {
        send(member, libc::SIGHUP, state, event);
    }
    let until = Instant::now() + grace;
    while Instant::now() < until && (shell.alive() || pinned.iter().any(Pinned::alive)) {
        std::thread::sleep(Duration::from_millis(20));
    }
    if shell.alive() {
        complete &= members(shell, &mut pinned, state, event, &pin);
    }
    if shell.alive() {
        send(shell, libc::SIGKILL, state, event);
    }
    for member in &pinned {
        if member.alive() {
            send(member, libc::SIGKILL, state, event);
        }
    }
    let killed = Instant::now();
    while killed.elapsed() < KILL_WAIT && (shell.alive() || pinned.iter().any(Pinned::alive)) {
        std::thread::sleep(Duration::from_millis(20));
    }
    complete && !(shell.alive() || pinned.iter().any(Pinned::alive))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};

    fn own_record(boot_id: String) -> ShellRecord {
        let pid = std::process::id();
        ShellRecord {
            pid,
            start_time: sidecars::identity_start_time(pid).unwrap(),
            boot_id,
        }
    }

    #[test]
    fn a_shell_record_is_pinned_only_in_the_boot_it_was_written_in() {
        let boot = super::super::current_boot_id().unwrap();
        let pin = |boot| pin_shell(own_record(boot), sidecars::identity_start_time).unwrap();
        assert!(pin(boot).is_some(), "this process, this boot");
        assert!(pin(String::new()).is_none(), "a record without a boot id");
        assert!(
            pin("00000000-0000-0000-0000-000000000000".into()).is_none(),
            "a record from another boot: the same pid and start time can be an unrelated process"
        );
    }

    #[tokio::test]
    async fn a_session_member_pin_failure_prevents_reporting_a_clean_stop() {
        let scratch = super::super::tests::Scratch::new();
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .args([
                "-c",
                "trap '' HUP; /bin/sleep 60 >/dev/null & printf '%s\\n' \"$!\"; wait",
            ])
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        // SAFETY: setsid creates a private session and does not access Rust memory.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let pid = child.id().unwrap();
        let shell = pin_shell(
            ShellRecord {
                pid,
                start_time: sidecars::identity_start_time(pid).unwrap(),
                boot_id: super::super::current_boot_id().unwrap(),
            },
            sidecars::identity_start_time,
        )
        .unwrap()
        .unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        let member_pid = stdout.next_line().await.unwrap().unwrap().parse().unwrap();
        let member = sidecars::pin(member_pid).unwrap();

        let stopped = end_with_pin(&shell, Duration::ZERO, &scratch.0, "terminal-stop-error", |pid| {
            if pid == member_pid {
                return Err(io::Error::from_raw_os_error(libc::EMFILE));
            }
            sidecars::pin(pid)
        });
        let shell_gone = !shell.alive();
        let member_alive = !sidecars::exited(&member).unwrap();
        sidecars::signal(&member, libc::SIGKILL).unwrap();
        child.wait().await.unwrap();

        assert!(shell_gone && member_alive);
        assert!(!stopped);
        let audit = std::fs::read_to_string(scratch.0.join("audit.log")).unwrap();
        assert!(audit.contains(&io::Error::from_raw_os_error(libc::EMFILE).to_string()));
    }
}
