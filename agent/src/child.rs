//! A child process handed from one window to the other. It is named by pid and start time, so a reused pid is
//! never taken for it; a pidfd signals and watches it whether or not it is this process's child; and its exit
//! status is read where this process can read one (only the kernel parent can).

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildKind {
    Shell,
    Sidecar,
    Agent,
    TaskGuard,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildRecord {
    pub pid: u32,
    pub start: u64,
    pub kind: ChildKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "how", rename_all = "snake_case")]
pub enum ExitCode {
    Exited { code: i32 },
    Signaled { signal: i32 },
}

/// Linux's `PIDFD_SIGNAL_PROCESS_GROUP` (since 6.9): the signal goes to the pidfd's process group.
const PIDFD_SIGNAL_PROCESS_GROUP: libc::c_uint = 1 << 2;

/// Field 22 of `/proc/<pid>/stat`: the start time in clock ticks since boot.
pub fn start_time(pid: u32) -> io::Result<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    parse_start_time(&text).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("/proc/{pid}/stat has no start time"),
        )
    })
}

/// Read after the last `)`, so a command name holding spaces or parentheses cannot shift the fields; the first
/// field after it is field 3.
fn parse_start_time(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

pub fn process_group(pid: u32) -> io::Result<u32> {
    // SAFETY: `getpgid` reads no memory of ours.
    let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
    if pgid < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(pgid as u32)
    }
}

#[derive(Debug)]
pub struct PidFd {
    fd: OwnedFd,
    pid: u32,
    start: u64,
}

impl PidFd {
    /// A pidfd for `pid`, refused when the process now at `pid` started at another time than `start`. The start time
    /// is read after the pidfd exists: a pidfd keeps naming the process it was opened for, so a match proves it.
    pub fn open(pid: u32, start: u64) -> io::Result<PidFd> {
        // SAFETY: `pidfd_open` takes a pid and flags and returns a new descriptor (close-on-exec) or -1.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0 as libc::c_uint) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: new and owned by nobody else.
        let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
        if start_time(pid)? != start {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("pid {pid} now names another process"),
            ));
        }
        Ok(PidFd { fd, pid, start })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn signal(&self, signal: i32) -> io::Result<()> {
        self.send(signal, 0)
    }

    /// The signal to the process group this process leads (a sidecar, an agent or a task started in a group of its
    /// own), also once the leader has exited and been reaped: the rest of the group still runs and goes too. Through
    /// the pidfd where the kernel can (Linux 6.9 and later), else by the group's number ([`Self::signal_group_by_number`]).
    pub fn signal_group(&self, signal: i32) -> io::Result<()> {
        match self.send(signal, PIDFD_SIGNAL_PROCESS_GROUP) {
            Err(err) if err.raw_os_error() == Some(libc::EINVAL) => self.signal_group_by_number(signal),
            other => other,
        }
    }

    /// `kill(-pid)`, refused (`ESRCH`) once the leader's pid names another process. The kernel hands a pid out again
    /// only when no process uses it, as its pid or as its group's number, so while any member of the group lives the
    /// group number names this group alone, whether the leader runs, is a zombie or has been reaped. The number can
    /// name another group only after the whole group ended and the pid was handed to a new process that made a group
    /// of its own: since the leader exited when that process has ended too or cannot be seen in this `/proc`
    /// (`hidepid`, another pid namespace), or between the look at `/proc` and the `kill`. Each needs the cyclic pid
    /// allocation to come round to this very pid in that time.
    fn signal_group_by_number(&self, signal: i32) -> io::Result<()> {
        if !group_number_is_ours(self.exited(), start_time(self.pid), self.start) {
            return Err(io::Error::from_raw_os_error(libc::ESRCH));
        }
        // SAFETY: `kill` reads no memory of ours.
        if unsafe { libc::kill(-(self.pid as libc::pid_t), signal) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn send(&self, signal: i32, flags: libc::c_uint) -> io::Result<()> {
        // SAFETY: `pidfd_send_signal` with a null `siginfo` reads no memory of ours.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.fd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                flags,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn exited(&self) -> bool {
        self.wait_exited(Duration::ZERO)
    }

    /// Whether the process ended within `timeout` (a pidfd is readable once it has). Never call it on the GTK thread
    /// with a timeout above zero.
    pub fn wait_exited(&self, timeout: Duration) -> bool {
        let mut pfd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        loop {
            // SAFETY: one `pollfd` that lives for the call.
            let n = unsafe { libc::poll(&mut pfd, 1, ms) };
            if n >= 0 {
                return n > 0;
            }
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return false;
            }
        }
    }

    /// The status of this process's own child, reaped; `None` while it runs. A process that is not our child is an
    /// error (`ECHILD`): its status is its parent's to read.
    pub fn try_reap(&self) -> io::Result<Option<ExitCode>> {
        // SAFETY: a zeroed `siginfo_t` is a valid buffer for `waitid`.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is writable for the call.
        let rc = unsafe {
            libc::waitid(
                libc::P_PIDFD,
                self.fd.as_raw_fd() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `waitid` filled `info` (or left it zeroed, which reads as pid 0: still running).
        let (pid, status) = unsafe { (info.si_pid(), info.si_status()) };
        if pid == 0 {
            return Ok(None);
        }
        Ok(Some(if info.si_code == libc::CLD_EXITED {
            ExitCode::Exited { code: status }
        } else {
            ExitCode::Signaled { signal: status }
        }))
    }
}

/// Whether the leader's pid still names its group, from whether the leader has exited and what `/proc/<pid>/stat`
/// says now: a leader that runs or is a zombie holds the number; a reaped one leaves it to the members that remain,
/// unless the pid now names another process.
fn group_number_is_ours(exited: bool, at_pid: io::Result<u64>, start: u64) -> bool {
    if !exited {
        return true;
    }
    match at_pid {
        Ok(now) => now == start,
        // `ESRCH`: the stat file was opened before the leader was released and read after.
        Err(err) => err.kind() == io::ErrorKind::NotFound || err.raw_os_error() == Some(libc::ESRCH),
    }
}

impl PidFd {
    /// The descriptor itself, for a crate that cannot depend on this one (the terminal's adopted child).
    pub fn into_fd(self) -> OwnedFd {
        self.fd
    }
}

impl AsFd for PidFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    fn is_cloexec(fd: BorrowedFd<'_>) -> io::Result<bool> {
        // SAFETY: `F_GETFD` reads the descriptor's flags and nothing of ours.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        if flags < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(flags & libc::FD_CLOEXEC != 0)
        }
    }

    #[test]
    fn the_start_time_is_read_after_the_command_name() {
        let mut stat = String::from("4242 (we ird) ) name) S");
        for field in 4..=21 {
            stat.push_str(&format!(" {field}"));
        }
        stat.push_str(" 987654 rest 1 2");
        assert_eq!(parse_start_time(&stat), Some(987654));
        assert_eq!(parse_start_time("1 (x) S 2"), None);
    }

    #[test]
    fn a_pidfd_names_the_process_it_was_opened_for_and_no_other() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let start = start_time(pid).unwrap();
        let fd = PidFd::open(pid, start).unwrap();
        assert!(is_cloexec(fd.as_fd()).unwrap());
        assert_eq!(PidFd::open(pid, start + 1).unwrap_err().kind(), io::ErrorKind::NotFound);
        assert!(!fd.exited());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn a_signal_through_the_pidfd_ends_the_child_and_its_parent_reaps_the_status() {
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let fd = PidFd::open(child.id(), start_time(child.id()).unwrap()).unwrap();
        assert_eq!(fd.try_reap().unwrap(), None);
        fd.signal(libc::SIGTERM).unwrap();
        assert!(fd.wait_exited(Duration::from_secs(2)));
        assert_eq!(
            fd.try_reap().unwrap(),
            Some(ExitCode::Signaled { signal: libc::SIGTERM })
        );
        std::mem::forget(child);
    }

    #[test]
    fn an_exit_code_is_reaped() {
        let child = Command::new("sh").args(["-c", "sleep 0.2; exit 3"]).spawn().unwrap();
        let fd = PidFd::open(child.id(), start_time(child.id()).unwrap()).unwrap();
        assert!(fd.wait_exited(Duration::from_secs(5)));
        assert_eq!(fd.try_reap().unwrap(), Some(ExitCode::Exited { code: 3 }));
        std::mem::forget(child);
    }

    #[test]
    fn a_process_that_is_not_our_child_is_watched_and_signalled_but_not_reaped() {
        let mut sh = Command::new("sh")
            .args(["-c", "sleep 30 >/dev/null 2>&1 & echo $!"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(sh.stdout.take().unwrap()).read_line(&mut line).unwrap();
        sh.wait().unwrap();
        let pid: u32 = line.trim().parse().unwrap();
        let fd = PidFd::open(pid, start_time(pid).unwrap()).unwrap();
        fd.signal(libc::SIGTERM).unwrap();
        assert!(fd.wait_exited(Duration::from_secs(2)));
        assert!(fd.try_reap().is_err(), "only the parent reads the status");
    }

    #[test]
    fn a_group_signal_reaches_every_member_of_the_group() {
        let mut sh = Command::new("sh")
            .args(["-c", "sleep 30 >/dev/null 2>&1 & echo $!; wait"])
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(sh.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let member: u32 = line.trim().parse().unwrap();
        let member_fd = PidFd::open(member, start_time(member).unwrap()).unwrap();
        let leader = PidFd::open(sh.id(), start_time(sh.id()).unwrap()).unwrap();
        assert_eq!(process_group(member).unwrap(), sh.id());
        leader.signal_group(libc::SIGTERM).unwrap();
        assert!(member_fd.wait_exited(Duration::from_secs(2)));
        assert!(leader.wait_exited(Duration::from_secs(2)));
        sh.wait().unwrap();
    }

    /// A group whose leader has exited and been reaped by its parent (this test), with one member left: the
    /// member's pidfd, and the leader's opened while it ran.
    struct Orphaned {
        member: PidFd,
        leader: PidFd,
    }

    impl Drop for Orphaned {
        fn drop(&mut self) {
            let _ = self.member.signal(libc::SIGKILL);
        }
    }

    fn orphaned_group() -> Orphaned {
        let mut sh = Command::new("sh")
            .args(["-c", "sleep 30 >/dev/null 2>&1 & echo $!; read _; exit 0"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(sh.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let member: u32 = line.trim().parse().unwrap();
        let group = Orphaned {
            member: PidFd::open(member, start_time(member).unwrap()).unwrap(),
            leader: PidFd::open(sh.id(), start_time(sh.id()).unwrap()).unwrap(),
        };
        drop(sh.stdin.take());
        assert!(sh.wait().unwrap().success());
        assert_eq!(
            start_time(sh.id()).unwrap_err().kind(),
            io::ErrorKind::NotFound,
            "the leader is reaped"
        );
        assert_eq!(process_group(member).unwrap(), sh.id());
        assert!(!group.member.exited());
        group
    }

    #[test]
    fn a_group_signal_reaches_the_members_left_after_the_leader_is_reaped() {
        let group = orphaned_group();
        group.leader.signal_group(libc::SIGTERM).unwrap();
        assert!(group.member.wait_exited(Duration::from_secs(5)));
    }

    #[test]
    fn without_group_signals_through_a_pidfd_the_group_number_still_reaches_the_members_left() {
        let group = orphaned_group();
        group.leader.signal_group_by_number(libc::SIGTERM).unwrap();
        assert!(group.member.wait_exited(Duration::from_secs(5)));
    }

    #[test]
    fn the_group_number_is_used_only_while_the_leaders_pid_names_no_other_process() {
        let gone = || Err(io::Error::from(io::ErrorKind::NotFound));
        assert!(group_number_is_ours(false, gone(), 7), "the pidfd says the leader runs");
        assert!(group_number_is_ours(true, Ok(7), 7), "a zombie leader");
        assert!(
            group_number_is_ours(true, gone(), 7),
            "reaped: the members left hold the number"
        );
        assert!(
            group_number_is_ours(true, Err(io::Error::from_raw_os_error(libc::ESRCH)), 7),
            "released while its stat was read"
        );
        assert!(!group_number_is_ours(true, Ok(8), 7), "the pid names another process");
        assert!(!group_number_is_ours(
            true,
            Err(io::Error::from(io::ErrorKind::PermissionDenied)),
            7
        ));
    }
}
