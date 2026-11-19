// agent/src/process_probe.rs
//! Is a pid alive, how much memory does it hold, how many threads does it run -- asked the same
//! way on Linux and macOS.
//!
//! **Why this exists (2026-09-17).** This crate answered all three by reading
//! `/proc`, which macOS does not have. The failure is not loud: `!Path::new("/proc/<pid>").exists()`
//! is simply always true there, so a "the process is gone" assertion passes whether or not it is,
//! and an `Option` read of `/proc/<pid>/status` quietly becomes "unreadable".
//!
//! | question | Linux | macOS |
//! |---|---|---|
//! | [`pid_is_alive`] | `kill(pid, 0)` | `kill(pid, 0)` |
//! | [`resident_kib`] | `VmRSS` in `/proc/<pid>/status` (unchanged) | `proc_pidinfo(PROC_PIDTASKINFO).pti_resident_size` |
//! | [`thread_count`] | entries of `/proc/<pid>/task` (unchanged) | `proc_pidinfo(PROC_PIDTASKINFO).pti_threadnum` |
//! | [`start_time`] | none: callers read `/proc/<pid>/stat` | `proc_pidinfo(PROC_PIDTBSDINFO)` |
//!
//! `pid_is_alive` answers the same as the `/proc/<pid>` check it replaces on Linux: both say
//! "alive" for a zombie and for another user's process. Elsewhere the two measurements return
//! `None`, which callers report as missing rather than as zero.
//!
//! `resident_kib` and `thread_count` are `#[doc(hidden)] pub`: `agent/tests/` and `agent/src/
//! process.rs`'s own tests use them, and `state_dirs::prune_dead_roots` uses them too, but that one
//! is itself test support -- nothing else calls either, so hidden-from-docs is an honest signal.
//!
//! `pid_is_alive` is deliberately NOT `#[doc(hidden)]` (removed 2026-09-17): since
//! then, `eitri-core`'s `instance_dir::sweep_stale_instance_dirs` -- real product code, not test
//! support -- reads its `false` as permission to delete a directory. See its own doc for the
//! contract that puts on it; a safety-critical contract a product path depends on belongs where its
//! callers will actually look for it, not hidden from the docs alongside two functions that really
//! are test-only.

/// Whether `pid` names a process that exists (running or zombie), by `kill(pid, 0)`.
///
/// `0` and anything above `i32::MAX` are "not a live pid" before any syscall, and must be: as a
/// `pid_t`, `0` means this process's own group and `u32::MAX` wraps to `-1`, "every process this
/// user may signal", and `kill` succeeds for both. `EPERM` means the process exists but belongs to
/// someone else, so it counts as alive; `ESRCH` means it does not exist.
///
/// **Load-bearing contract: `false` must only ever mean ESRCH, never "could not determine".**
/// `eitri-core`'s `instance_dir::sweep_stale_instance_dirs` reads a `false` from this function as
/// permission to `remove_dir_all` a directory -- so a `false` that actually means "unknown" would
/// delete a live instance's state. The consequence follows directly: a platform this crate cannot
/// answer the question on must fail to COMPILE, not silently return `false`. `libc::kill` below is
/// called unconditionally, with no `#[cfg(target_os = ...)]` guard, so a non-unix target simply
/// fails to build here -- that is the correct outcome, not an oversight to fix. In particular, do
/// not "fix" a future non-unix build failure by giving this the same shape `resident_kib` and
/// `thread_count` use just below (`#[cfg(not(any(target_os = "linux", target_os = "macos")))]` ->
/// a fallback value): those two have an honest "unknown" in `Option::None`, but `bool` has none --
/// a `false` stub would make every instance directory on that platform silently sweepable
/// regardless of whether the process it names is actually alive.
pub fn pid_is_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: signal 0 performs only the existence and permission checks; nothing is delivered.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// `pid`'s resident set size in KiB, or `None` when it cannot be read.
#[doc(hidden)]
pub fn resident_kib(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        status
            .lines()
            .find(|line| line.starts_with("VmRSS:"))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|kib| kib.parse().ok())
    }
    #[cfg(target_os = "macos")]
    {
        task_info(pid).map(|info| info.pti_resident_size / 1024)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// How many threads `pid` is running, or `None` when that cannot be read.
#[doc(hidden)]
pub fn thread_count(pid: u32) -> Option<usize> {
    #[cfg(target_os = "linux")]
    {
        Some(std::fs::read_dir(format!("/proc/{pid}/task")).ok()?.count())
    }
    #[cfg(target_os = "macos")]
    {
        task_info(pid).and_then(|info| usize::try_from(info.pti_threadnum).ok())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// When `pid` started, in microseconds since the epoch, from `proc_pidinfo(PROC_PIDTBSDINFO)`; `None` for a
/// process that is gone or that this user may not inspect. Together with the pid it names one process for good, as a
/// pid alone does not once it is reused: it is what Linux's start time in `/proc/<pid>/stat` is used for, and like that
/// value it is only ever compared with another reading on the same machine. Never 0 for a process that exists.
#[cfg(target_os = "macos")]
pub fn start_time(pid: u32) -> Option<u64> {
    start_of(&bsd_info(pid)?)
}

/// The most processes [`descendants`] records: a bound for a tree that forks without end, far above what an agent's
/// commands and their children come to. Reaching it is logged.
#[cfg(target_os = "macos")]
const DESCENDANTS_CAP: usize = 4096;

/// `pbi_flags`' bit for a process being traced (`<sys/proc_info.h>`).
#[cfg(target_os = "macos")]
const PROC_FLAG_TRACED: u32 = 2;

/// The processes below `pid`, at any depth, each with its start time ([`start_time`]), read with
/// `proc_listchildpids`. Read while `pid` is alive: a child whose parent has exited belongs to launchd and can no longer
/// be traced to it.
///
/// What it is for: on macOS nothing ends a child with its parent, and an agent can start a command as the leader of a
/// session of its own (Codex does), which its parent's process-group signal misses. Linux's agents end theirs with
/// the parent (Codex's sandbox), so this is macOS's alone.
///
/// A process is recorded only as its parent's real child: its own parent pid is the parent, it started no earlier
/// than the parent, and it is not being traced (a traced process is listed under its tracer, whatever its own tree).
/// A parent's children are kept only if the parent is still the process it was when found (its start time), so a pid
/// reused meanwhile never brings in another tree.
#[cfg(target_os = "macos")]
pub(crate) fn descendants(pid: u32) -> Vec<(u32, u64)> {
    let mut found: Vec<(u32, u64)> = Vec::new();
    let Some(root_start) = start_time(pid) else {
        return found;
    };
    let mut next = vec![(pid, root_start)];
    while let Some((parent, parent_start)) = next.pop() {
        let listed = children(parent);
        if start_time(parent) != Some(parent_start) {
            continue;
        }
        for child in listed {
            if found.iter().any(|(p, _)| *p == child) {
                continue;
            }
            let Some(info) = bsd_info(child) else { continue };
            let Some(start) = start_of(&info) else { continue };
            if info.pbi_ppid != parent || start < parent_start || info.pbi_flags & PROC_FLAG_TRACED != 0 {
                continue;
            }
            if found.len() >= DESCENDANTS_CAP {
                log_cap_once(pid);
                return found;
            }
            found.push((child, start));
            next.push((child, start));
        }
    }
    found
}

/// Says once per process that the cap was reached. Never `eprintln!`: this runs while a link is being dropped, and a
/// write to a stderr whose reader has gone must not panic there.
#[cfg(target_os = "macos")]
fn log_cap_once(pid: u32) {
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        let _ = writeln!(
            std::io::stderr(),
            "agent: more than {DESCENDANTS_CAP} processes under pid {pid}; the rest are not recorded"
        );
    }
}

/// Adds to `recorded` what `more` has that it lacks (the same pid and start time).
#[cfg(target_os = "macos")]
pub(crate) fn merge(recorded: &mut Vec<(u32, u64)>, more: Vec<(u32, u64)>) {
    for entry in more {
        if !recorded.contains(&entry) {
            recorded.push(entry);
        }
    }
}

#[cfg(target_os = "macos")]
fn children(pid: u32) -> Vec<u32> {
    let Some(pid) = libc::pid_t::try_from(pid).ok().filter(|&p| p > 0) else {
        return Vec::new();
    };
    let mut buf: Vec<libc::pid_t> = vec![0; 256];
    loop {
        let bytes = libc::c_int::try_from(buf.len() * std::mem::size_of::<libc::pid_t>()).unwrap_or(libc::c_int::MAX);
        // SAFETY: the buffer holds `bytes` bytes of `pid_t`s; the call writes at most that many and returns how many
        // pids it wrote.
        let n = unsafe { libc::proc_listchildpids(pid, buf.as_mut_ptr().cast(), bytes) };
        let Ok(n) = usize::try_from(n) else {
            return Vec::new();
        };
        // A full buffer may have been cut short: read again with room to spare.
        if n >= buf.len() && buf.len() < DESCENDANTS_CAP {
            buf.resize(buf.len() * 4, 0);
            continue;
        }
        return buf[..n.min(buf.len())]
            .iter()
            .filter_map(|&p| u32::try_from(p).ok())
            .filter(|&p| p > 0)
            .collect();
    }
}

/// SIGKILL to each recorded process that is still the one recorded (the same start time), and, when it leads a
/// session of its own, to its process group: a group in that session holds only its own descendants, so a command
/// started as a session leader takes its children down with it and nothing else. Before that, what each recorded
/// process that is still alive has started since is recorded too (a live parent can still be traced).
///
/// The start time is checked and the signal sent in two calls; macOS has no handle that pins a process, so a pid
/// reused in between would be signalled. That takes the process to end and the pid counter (sequential, to 99999)
/// to come round to it within those microseconds.
#[cfg(target_os = "macos")]
pub(crate) fn kill_recorded(recorded: &[(u32, u64)]) {
    let mut all = recorded.to_vec();
    for &(pid, start) in recorded {
        if start_time(pid) == Some(start) {
            merge(&mut all, descendants(pid));
        }
    }
    for &(pid, start) in &all {
        if start_time(pid) != Some(start) {
            continue;
        }
        let Ok(p) = libc::pid_t::try_from(pid) else { continue };
        // SAFETY: `getsid`, `getpgid` and `kill` take no pointers; the pid was just confirmed to name the recorded
        // process.
        unsafe {
            if libc::getsid(p) == p && libc::getpgid(p) == p {
                libc::kill(-p, libc::SIGKILL);
            }
            libc::kill(p, libc::SIGKILL);
        }
    }
}

#[cfg(target_os = "macos")]
fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
    let pid = libc::c_int::try_from(pid).ok().filter(|&p| p > 0)?;
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: all-zero is a valid `proc_bsdinfo` (integers and byte arrays), and the buffer is exactly the size
    // passed; `proc_pidinfo` writes at most that many bytes and returns how many it wrote.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (written == size).then_some(info)
}

#[cfg(target_os = "macos")]
fn start_of(info: &libc::proc_bsdinfo) -> Option<u64> {
    let micros = info
        .pbi_start_tvsec
        .checked_mul(1_000_000)?
        .checked_add(info.pbi_start_tvusec)?;
    (micros != 0).then_some(micros)
}

#[cfg(target_os = "macos")]
fn task_info(pid: u32) -> Option<libc::proc_taskinfo> {
    let pid = libc::c_int::try_from(pid).ok().filter(|&p| p > 0)?;
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    // SAFETY: all-zero is a valid `proc_taskinfo` (plain integers), and the buffer is exactly the
    // size passed; `proc_pidinfo` writes at most that many bytes and returns how many it wrote.
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            (&mut info as *mut libc::proc_taskinfo).cast(),
            size,
        )
    };
    (written == size).then_some(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A start time names one process: the same for every reading of a running one, another for a later process,
    /// none for one that is gone.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_start_time_is_stable_for_a_process_and_gone_with_it() {
        let own = start_time(std::process::id()).expect("this process has a start time");
        assert_eq!(start_time(std::process::id()), Some(own));
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let started = start_time(child.id()).expect("a running child has a start time");
        assert!(started >= own, "a child cannot start before its parent");
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(start_time(child.id()), None);
        assert_eq!(start_time(0), None);
        assert_eq!(start_time(u32::MAX), None);
    }

    /// A zombie's `pbi_status` (`<sys/proc.h>`). `proc_pidinfo` fails for a zombie on macOS 26 (measured), so a
    /// zombie mostly reads as gone; the status is checked for a system that answers.
    #[cfg(target_os = "macos")]
    const SZOMB: u32 = 5;

    /// Whether `pid` is gone or a zombie, waiting up to two seconds.
    #[cfg(target_os = "macos")]
    fn ends_soon(pid: u32) -> bool {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < end {
            if bsd_info(pid).is_none_or(|i| i.pbi_status == SZOMB) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    /// The child a `perl` command prints on its first line.
    #[cfg(target_os = "macos")]
    fn spawn_printing(script: &str) -> (std::process::Child, u32) {
        use std::io::BufRead;
        let mut leader = std::process::Command::new("/usr/bin/perl")
            .args(["-MPOSIX", "-e", script])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(leader.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        (leader, line.trim().parse().unwrap())
    }

    /// A recorded session leader is killed with its group: a member that is no longer its descendant (its parent
    /// exited and launchd took it), so that only the group signal can reach it, goes too.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_recorded_session_leader_is_killed_with_its_group() {
        let (mut leader, member) = spawn_printing(
            r#"$|=1; POSIX::setsid(); my $i = fork(); if ($i == 0) { my $g = fork(); if ($g == 0) { exec "sleep", "300" } print "$g\n"; POSIX::_exit(0) } waitpid($i, 0); exec "sleep", "300""#,
        );
        let end = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while bsd_info(member).is_some_and(|i| i.pbi_ppid != 1) {
            assert!(
                std::time::Instant::now() < end,
                "the member was never handed to launchd"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!ends_soon_now(member), "the member runs");
        assert!(
            !descendants(leader.id()).iter().any(|&(p, _)| p == member),
            "the member is not the leader's descendant"
        );
        let start = start_time(leader.id()).unwrap();
        kill_recorded(&[(leader.id(), start)]);
        leader.wait().unwrap();
        assert!(ends_soon(member), "the group's member outlived the group kill");
    }

    /// A record whose start time is not the process's own (as a reused pid's would not be) signals nothing.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_record_with_another_start_time_is_never_signalled() {
        let mut sleeper = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let start = start_time(sleeper.id()).unwrap();
        kill_recorded(&[(sleeper.id(), start + 1), (sleeper.id(), start - 1)]);
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            sleeper.try_wait().unwrap().is_none(),
            "a record of another process killed this one"
        );
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
    }

    /// The walk finds a child and the child's own child, each with its start time, and nothing for a pid that is gone.
    #[cfg(target_os = "macos")]
    #[test]
    fn descendants_are_found_at_any_depth_with_their_start_times() {
        let (mut top, grandchild) = spawn_printing(
            r#"$|=1; my $c = fork(); if ($c == 0) { my $g = fork(); if ($g == 0) { exec "sleep", "300" } print "$g\n"; exec "sleep", "300" } exec "sleep", "300""#,
        );
        let found = descendants(top.id());
        assert!(
            found.iter().any(|&(p, s)| p == grandchild && start_time(p) == Some(s)),
            "{found:?}"
        );
        assert_eq!(found.len(), 2, "{found:?}");
        kill_recorded(&found);
        top.kill().unwrap();
        top.wait().unwrap();
        assert!(ends_soon(grandchild));
        assert!(descendants(top.id()).is_empty());
    }

    #[cfg(target_os = "macos")]
    fn ends_soon_now(pid: u32) -> bool {
        bsd_info(pid).is_none_or(|i| i.pbi_status == SZOMB)
    }

    #[test]
    fn this_process_is_alive() {
        assert!(pid_is_alive(std::process::id()));
    }

    /// The two values `kill` would misread: `0` is this process's group and `u32::MAX` wraps to
    /// `-1`, and `kill(0|-1, 0)` both succeed.
    #[test]
    fn pid_zero_and_pids_past_i32_max_are_never_alive() {
        assert!(!pid_is_alive(0));
        assert!(!pid_is_alive(u32::MAX));
        assert!(!pid_is_alive(i32::MAX as u32 + 1));
    }

    /// Pid 1 is `init`/`systemd` on Linux and `launchd` on macOS, owned by root: `kill` fails with
    /// `EPERM` for an unprivileged caller, and that still means alive.
    #[test]
    fn a_process_this_user_cannot_signal_is_still_alive() {
        assert!(pid_is_alive(1));
    }

    /// A real child, reaped: alive while it runs, not alive once `wait` has collected it.
    #[test]
    fn a_reaped_child_is_not_alive() {
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        assert!(pid_is_alive(pid));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!pid_is_alive(pid));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn this_process_reports_a_plausible_rss_and_thread_count() {
        let rss = resident_kib(std::process::id()).expect("RSS must be readable for this process");
        assert!((1024..16 * 1024 * 1024).contains(&rss), "implausible RSS: {rss} KiB");

        let before = thread_count(std::process::id()).expect("thread count must be readable");
        assert!(before >= 1);
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let (started, ready) = std::sync::mpsc::channel::<()>();
        let extra = std::thread::spawn(move || {
            started.send(()).unwrap();
            let _ = wait.recv();
        });
        ready.recv().unwrap();
        // Other tests' threads come and go concurrently, so only "at least one" is reliable.
        assert!(thread_count(std::process::id()).unwrap() >= 2);
        release.send(()).unwrap();
        extra.join().unwrap();
    }

    #[test]
    fn a_pid_that_does_not_exist_has_no_measurements() {
        assert_eq!(resident_kib(u32::MAX), None);
        assert_eq!(thread_count(u32::MAX), None);
    }
}
