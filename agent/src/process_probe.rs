// agent/src/process_probe.rs
//! Is a pid alive, how much memory does it hold, how many threads does it run -- asked the same
//! way on Linux and macOS.
//!
//! **Why this exists (macOS track M1, 2026-09-17).** This crate answered all three by reading
//! `/proc`, which macOS does not have. The failure is not loud: `!Path::new("/proc/<pid>").exists()`
//! is simply always true there, so a "the process is gone" assertion passes whether or not it is,
//! and an `Option` read of `/proc/<pid>/status` quietly becomes "unreadable".
//!
//! | question | Linux | macOS |
//! |---|---|---|
//! | [`pid_is_alive`] | `kill(pid, 0)` | `kill(pid, 0)` |
//! | [`resident_kib`] | `VmRSS` in `/proc/<pid>/status` (unchanged) | `proc_pidinfo(PROC_PIDTASKINFO).pti_resident_size` |
//! | [`thread_count`] | entries of `/proc/<pid>/task` (unchanged) | `proc_pidinfo(PROC_PIDTASKINFO).pti_threadnum` |
//!
//! `pid_is_alive` answers the same as the `/proc/<pid>` check it replaces on Linux: both say
//! "alive" for a zombie and for another user's process. Elsewhere the two measurements return
//! `None`, which callers report as missing rather than as zero.
//!
//! `resident_kib` and `thread_count` are `#[doc(hidden)] pub`: `agent/tests/` and `agent/src/
//! process.rs`'s own tests use them, and `state_dirs::prune_dead_roots` uses them too, but that one
//! is itself test support -- nothing else calls either, so hidden-from-docs is an honest signal.
//!
//! `pid_is_alive` is deliberately NOT `#[doc(hidden)]` (removed 2026-09-17, L2 T3 follow-up): since
//! T3, `neovibe-core`'s `instance_dir::sweep_stale_instance_dirs` -- real product code, not test
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
/// `neovibe-core`'s `instance_dir::sweep_stale_instance_dirs` reads a `false` from this function as
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

#[cfg(target_os = "macos")]
fn task_info(pid: u32) -> Option<libc::proc_taskinfo> {
    let pid = libc::c_int::try_from(pid).ok().filter(|&p| p > 0)?;
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    // SAFETY: all-zero is a valid `proc_taskinfo` (plain integers), and the buffer is exactly the
    // size passed; `proc_pidinfo` writes at most that many bytes and returns how many it wrote.
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let written = unsafe {
        libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, (&mut info as *mut libc::proc_taskinfo).cast(), size)
    };
    (written == size).then_some(info)
}

#[cfg(test)]
mod tests {
    use super::*;

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
