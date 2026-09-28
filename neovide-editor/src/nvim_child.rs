//! Tracks the embedded nvim process and ends it without discarding unsaved buffers with `:qa!`.
//!
//! `:qa!` discards unsaved buffers and deletes their swap files. Normal quitting uses
//! `:confirm qall` so the user can respond. Closing stdin with `LiveHarness::hang_up` lets nvim
//! exit while keeping swap files. If it remains running, [`end`] signals the held process with
//! SIGTERM, which can also end a dialog that EOF does not. SIGKILL follows when [`SCHEDULE`]
//! expires.
//!
//! [`NvimProcess`] identifies nvim from two sources, in order of preference:
//!
//! 1. Once the session is running, [`NvimProcess::learn`] asks nvim for `getpid()` over RPC on a
//!    separate thread. This identifies nvim even if a launcher script started it without `exec`.
//!    Signalling only the direct child could kill that launcher and leave nvim running.
//! 2. Before nvim can answer, [`NvimChild::find_new`] compares direct children before and after
//!    `LiveHarness::with_options`. It accepts the new child carrying `--embed` only if its
//!    `/proc/<pid>/exe` is named `nvim`. Otherwise it looks for that process's single `--embed`
//!    child whose executable is nvim. It holds this child from launch, since an exiting launcher
//!    leaves nvim reparented. If neither identifies nvim, [`Ended::Unknown`] signals nothing and
//!    only the harness report indicates whether nvim ended.
//!
//! A pidfd pins the exact process, preventing a signal from reaching a later process that reuses
//! its pid. Signalling an exited process yields `ESRCH`. Without pidfds, on kernels before 5.3,
//! each signal first rechecks `--embed` and, for a direct child, that it is still our child.
//!
//! Only Linux implements process tracking. Elsewhere no process is held or signalled, so only
//! hanging up ends nvim.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The pids of this process's direct children, now. Empty where it cannot be read.
pub(crate) fn direct_children() -> Vec<u32> {
    imp::children_of(std::process::id())
}

/// One process held as nvim (or as the process the fork started, [`NvimChild::find_new`]).
#[derive(Debug)]
pub(crate) struct NvimChild {
    pid: u32,
    /// The fork's own direct child. Checked again by a kill that has no pidfd; a pid nvim reported,
    /// or one found under a launcher, is not ours to reap or to check for parentage.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    direct: bool,
    #[cfg(target_os = "linux")]
    pidfd: Option<std::os::fd::OwnedFd>,
}

impl NvimChild {
    /// The one direct child of this process that was not in `before` and whose argv carries
    /// `--embed`: the process the fork started, nvim or a launcher of it. `None` when there is no
    /// such child, or more than one: a kill must never guess.
    pub(crate) fn find_new(before: &[u32]) -> Option<NvimChild> {
        let mut found = direct_children()
            .into_iter()
            .filter(|pid| !before.contains(pid) && imp::argv_has_embed(*pid));
        let pid = found.next()?;
        if found.next().is_some() {
            return None;
        }
        Some(imp::hold(pid, true))
    }

    /// `pid` as nvim itself reported it (`getpid()`), held only if that process is alive with
    /// `--embed` in its argv once the pidfd pins it -- so a pid reused between the answer and the
    /// hold is refused, not held.
    pub(crate) fn reported(pid: u32) -> Option<NvimChild> {
        imp::hold_checked(pid)
    }

    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    /// Waits up to `within` for the killed process to be gone, reaping it if it is this process's
    /// own child: after the fork's own runtime is shut down (`LiveHarness::shutdown` returned)
    /// nothing else will. A launcher's nvim is reaped by the launcher. `true` once it is gone.
    pub(crate) fn reap(&self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        loop {
            match imp::try_reap(self) {
                Some(true) => return true,
                Some(false) | None if Instant::now() >= deadline => return false,
                _ => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}

/// The embedded nvim's process, as far as it is known (the module doc's two sources).
#[derive(Debug)]
pub(crate) struct NvimProcess {
    /// What the fork started (source 2): nvim, or a launcher of it.
    launched: Option<NvimChild>,
    /// The nvim `--embed` child found beneath a launcher at launch, held by a pidfd.
    /// Find it while the launcher still runs: once the launcher exits, nvim is re-parented and cannot
    /// be found beneath it. The held pidfd continues to identify whether nvim lives.
    found_below: Option<NvimChild>,
    /// Whether `launched` ran nvim itself when it was found (a process gone since has no `exe`).
    launched_is_nvim: bool,
    /// nvim's own `getpid()` answer (source 1), held once it came back.
    reported: OnceLock<NvimChild>,
}

impl NvimProcess {
    pub(crate) fn new(launched: Option<NvimChild>) -> NvimProcess {
        let launched_is_nvim = launched.as_ref().is_some_and(|child| imp::exe_is_nvim(child.pid));
        let found_below = launched
            .as_ref()
            .filter(|_| !launched_is_nvim)
            .and_then(|child| imp::nvim_child_of(child.pid));
        NvimProcess {
            launched,
            found_below,
            launched_is_nvim,
            reported: OnceLock::new(),
        }
    }

    /// The best process known to be nvim: its own answer, else the fork's child if that ran nvim,
    /// else the nvim found under it at launch.
    fn known_nvim(&self) -> Option<&NvimChild> {
        self.reported
            .get()
            .or(self.launched.as_ref().filter(|_| self.launched_is_nvim))
            .or(self.found_below.as_ref())
    }

    /// Whether nvim's own process is still alive, waiting up to `grace` for one that is exiting
    /// (an nvim whose exit the fork has just reported may take a moment to finish). `None` when no
    /// process is known to be nvim -- then only the fork's own report says anything.
    pub(crate) fn alive_within(&self, grace: Duration) -> Option<bool> {
        self.known_nvim().map(|nvim| imp::alive_within(nvim, grace))
    }

    /// The pid the fork started, for the log line at launch.
    pub(crate) fn launched_pid(&self) -> Option<u32> {
        self.launched.as_ref().map(NvimChild::pid)
    }

    /// nvim's own answer to `getpid()`. `false`, holding nothing, when that pid is no longer a live
    /// `--embed` process (it exited, or the number was reused before the hold).
    pub(crate) fn learn(&self, pid: u32) -> bool {
        if self.reported.get().is_some_and(|held| held.pid == pid) {
            return true;
        }
        match NvimChild::reported(pid) {
            Some(held) => self.reported.set(held).is_ok(),
            None => false,
        }
    }

    /// The pid [`end`] would watch and signal: nvim itself -- its own `getpid()` answer, else what
    /// the fork started if that is nvim, else the nvim a launcher started. Never the launcher.
    pub(crate) fn nvim_pid(&self) -> Option<u32> {
        self.with_nvim(NvimChild::pid)
    }

    fn with_nvim<R>(&self, f: impl FnOnce(&NvimChild) -> R) -> Option<R> {
        if let Some(nvim) = self.known_nvim() {
            return Some(f(nvim));
        }
        let launched = self.launched.as_ref()?;
        // Gone already (a zombie has no `exe` to read): a kill of it is a no-op, as it always was.
        if !imp::is_alive(launched) || imp::exe_is_nvim(launched.pid) {
            return Some(f(launched));
        }
        // A launcher that started nvim only after the launch was looked at.
        let below = imp::nvim_child_of(launched.pid)?;
        Some(f(&below))
    }
}

/// When [`end`] escalates, counted from the moment nvim's stdin was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Schedule {
    /// SIGTERM from then on, once per `term_every`, while nvim lives.
    pub(crate) term_after: Duration,
    pub(crate) term_every: Duration,
    /// SIGKILL, once, if nvim still lives.
    pub(crate) kill_after: Duration,
}

/// How [`end`] ended nvim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ended {
    /// It exited on the hang-up alone.
    Exited,
    /// It exited after SIGTERM.
    Terminated,
    /// SIGKILL: its swap files stay, without what it had not written to them yet.
    Killed,
    /// No process is known to be nvim: nothing is watched or signalled.
    Unknown,
}

/// The signal schedule used by [`end`] after closing nvim's stdin.
pub(crate) const SCHEDULE: Schedule = Schedule {
    term_after: Duration::from_secs(1),
    term_every: Duration::from_secs(1),
    kill_after: Duration::from_secs(5),
};

/// Ends nvim after stdin was closed at `hung_up_at` and returns how it ended.
/// EOF lets nvim preserve modified buffers in swap files instead of discarding them with `:qa!`.
/// With nvim 0.12.5, EOF also writes edits that were not yet synced into those files.
/// If EOF does not end nvim, such as in a `:confirm` dialog or after EOF was dropped during an
/// external command, send SIGTERM from `term_after`, repeated every `term_every`.
/// Nvim's `preserve_exit` handler preserves edits and swap files and can end the dialog.
/// At `kill_after`, send SIGKILL to the held pid if needed; unsynced edits may then be lost.
/// Blocks until exit, at most `kill_after` plus a reap. Returns `Unknown` immediately if no nvim
/// process is known.
pub(crate) fn end(process: &NvimProcess, hung_up_at: Instant, schedule: Schedule) -> Ended {
    process
        .with_nvim(|nvim| end_one(nvim, hung_up_at, schedule))
        .unwrap_or(Ended::Unknown)
}

fn end_one(nvim: &NvimChild, hung_up_at: Instant, schedule: Schedule) -> Ended {
    let mut terminated = false;
    let mut next_term = schedule.term_after;
    loop {
        let elapsed = hung_up_at.elapsed();
        let gone = |terminated| if terminated { Ended::Terminated } else { Ended::Exited };
        if elapsed >= schedule.kill_after {
            if !imp::alive_within(nvim, Duration::ZERO) {
                return gone(terminated);
            }
            let delivered = imp::signal(nvim, libc::SIGKILL);
            let reaped = nvim.reap(Duration::from_millis(500));
            println!(
                "[live] nvim (pid {}) did not exit within {:?} of its hang-up: SIGKILL {}, {}",
                nvim.pid(),
                schedule.kill_after,
                if delivered { "sent" } else { "not needed" },
                if reaped { "gone" } else { "not yet gone" },
            );
            return Ended::Killed;
        }
        let checkpoint = next_term.min(schedule.kill_after);
        if !imp::alive_within(nvim, checkpoint.saturating_sub(elapsed)) {
            return gone(terminated);
        }
        if hung_up_at.elapsed() >= next_term && next_term < schedule.kill_after {
            if imp::signal(nvim, libc::SIGTERM) && !terminated {
                println!("[live] nvim (pid {}) did not exit on its hang-up; SIGTERM", nvim.pid());
            }
            terminated = true;
            next_term += schedule.term_every;
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::time::Duration;

    use super::NvimChild;

    /// `/proc/<pid>/stat`'s fourth field. The second, the command name, is in parentheses and may
    /// itself hold spaces or parentheses, so the fields are counted from the last `)`.
    fn parent_of(pid: u32) -> Option<u32> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after_name = &stat[stat.rfind(')')? + 1..];
        after_name.split_whitespace().nth(1)?.parse().ok()
    }

    pub(super) fn children_of(parent: u32) -> Vec<u32> {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        entries
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
            .filter(|pid| parent_of(*pid) == Some(parent))
            .collect()
    }

    pub(super) fn argv_has_embed(pid: u32) -> bool {
        std::fs::read(format!("/proc/{pid}/cmdline"))
            .is_ok_and(|argv| argv.split(|b| *b == 0).any(|arg| arg == b"--embed"))
    }

    /// Whether the program `pid` runs is a file named `nvim` -- what a launcher script is not: its
    /// `exe` is its interpreter (`/usr/bin/bash`), whatever the script itself is called. A binary
    /// replaced while it runs reads `nvim (deleted)`.
    pub(super) fn exe_is_nvim(pid: u32) -> bool {
        std::fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|exe| {
            exe.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == "nvim" || name == "nvim (deleted)")
        })
    }

    pub(super) fn hold(pid: u32, direct: bool) -> NvimChild {
        // SAFETY: a plain syscall with no pointers; a non-negative return is a new fd we own.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        let pidfd = (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd as i32) });
        NvimChild { pid, direct, pidfd }
    }

    /// Whether the held process is still running (not exited, not a zombie).
    pub(super) fn is_alive(child: &NvimChild) -> bool {
        match &child.pidfd {
            Some(pidfd) => !exited(pidfd, 0),
            None => std::fs::read_to_string(format!("/proc/{}/stat", child.pid))
                .ok()
                .and_then(|stat| {
                    let after = &stat[stat.rfind(')')? + 1..];
                    after.split_whitespace().next().map(|state| state != "Z")
                })
                .unwrap_or(false),
        }
    }

    /// Whether the held process is still running after waiting up to `grace` for it to exit.
    pub(super) fn alive_within(child: &NvimChild, grace: Duration) -> bool {
        match &child.pidfd {
            Some(pidfd) => !exited(pidfd, i32::try_from(grace.as_millis()).unwrap_or(i32::MAX)),
            None => {
                let deadline = std::time::Instant::now() + grace;
                while is_alive(child) {
                    if std::time::Instant::now() >= deadline {
                        return true;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                false
            }
        }
    }

    /// A pidfd polls readable once its process has exited (Linux 5.3), for a child or not.
    fn exited(pidfd: &OwnedFd, timeout_ms: i32) -> bool {
        let mut poll = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one `pollfd` that outlives the call.
        let rc = unsafe { libc::poll(&mut poll, 1, timeout_ms) };
        rc > 0 && poll.revents & libc::POLLIN != 0
    }

    /// Held, then checked through the hold: `/proc/<pid>` is read first and the pidfd asked second,
    /// so a process alive at the second step is the one the first step read.
    pub(super) fn hold_checked(pid: u32) -> Option<NvimChild> {
        let held = hold(pid, false);
        (argv_has_embed(pid) && is_alive(&held)).then_some(held)
    }

    /// The one child of `parent` that is nvim (`--embed`, and [`exe_is_nvim`]).
    pub(super) fn nvim_child_of(parent: u32) -> Option<NvimChild> {
        let mut found = children_of(parent)
            .into_iter()
            .filter(|pid| argv_has_embed(*pid) && exe_is_nvim(*pid));
        let pid = found.next()?;
        if found.next().is_some() {
            return None;
        }
        let held = hold(pid, false);
        (parent_of(pid) == Some(parent) && argv_has_embed(pid) && exe_is_nvim(pid) && is_alive(&held)).then_some(held)
    }

    pub(super) fn signal(child: &NvimChild, signal: libc::c_int) -> bool {
        match &child.pidfd {
            Some(pidfd) => {
                // SAFETY: `pidfd` is a live fd we own; a null siginfo is what `kill(2)` would send.
                let rc = unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        pidfd.as_raw_fd(),
                        signal,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    )
                };
                rc == 0
            }
            None => {
                // No pidfd: only a pid that still carries `--embed` (and, for the fork's child, is
                // still ours) is signalled.
                if !argv_has_embed(child.pid) || (child.direct && parent_of(child.pid) != Some(std::process::id())) {
                    return false;
                }
                // SAFETY: plain syscall.
                unsafe { libc::kill(child.pid as libc::pid_t, signal) == 0 }
            }
        }
    }

    /// `Some(true)` gone (reaped now, or exited and not ours to reap), `Some(false)` still there,
    /// `None` on another error.
    pub(super) fn try_reap(child: &NvimChild) -> Option<bool> {
        if let Some(pidfd) = &child.pidfd {
            if !exited(pidfd, 0) {
                return Some(false);
            }
            if child.direct {
                let mut status = 0;
                // SAFETY: `status` outlives the call. The pid cannot have been reused: an exited
                // child of ours that is not reaped yet keeps it.
                unsafe { libc::waitpid(child.pid as libc::pid_t, &mut status, libc::WNOHANG) };
            }
            return Some(true);
        }
        let mut status = 0;
        // SAFETY: `status` outlives the call.
        let rc = unsafe { libc::waitpid(child.pid as libc::pid_t, &mut status, libc::WNOHANG) };
        match rc {
            0 => Some(false),
            r if r == child.pid as libc::pid_t => Some(true),
            _ if std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) => Some(true),
            _ => None,
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::NvimChild;

    pub(super) fn children_of(_parent: u32) -> Vec<u32> {
        Vec::new()
    }
    pub(super) fn argv_has_embed(_pid: u32) -> bool {
        false
    }
    pub(super) fn exe_is_nvim(_pid: u32) -> bool {
        false
    }
    pub(super) fn hold(pid: u32, direct: bool) -> NvimChild {
        NvimChild { pid, direct }
    }
    pub(super) fn is_alive(_child: &NvimChild) -> bool {
        false
    }
    pub(super) fn alive_within(_child: &NvimChild, _grace: std::time::Duration) -> bool {
        false
    }
    pub(super) fn hold_checked(_pid: u32) -> Option<NvimChild> {
        None
    }
    pub(super) fn nvim_child_of(_parent: u32) -> Option<NvimChild> {
        None
    }
    pub(super) fn signal(_child: &NvimChild, _signal: i32) -> bool {
        false
    }
    pub(super) fn try_reap(_child: &NvimChild) -> Option<bool> {
        None
    }
}

/// Every test that spawns nvim in this crate takes this lock first.
/// [`NvimChild::find_new`] examines all children of the test process, so a concurrent spawn in
/// `nvim_rpc` could be mistaken for its own new child.
#[cfg(test)]
pub(crate) static SPAWNING: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(all(test, target_os = "linux"))]
mod tests {
    //! Real `nvim` processes (on `PATH`), no display: not `#[ignore]`d (the v1-hardening plan's rule
    //! -- ignore only what needs a display). Serialised by [`SPAWNING`], because
    //! [`NvimChild::find_new`] looks at every child this test process has.
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};

    fn scratch(case: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nvchild-{}-{case}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// `nvim --embed`, as the fork starts it, with nothing attached: it waits on its stdin for a UI,
    /// which is as good as hung for a quit that never gets answered.
    fn spawn_embed(dir: &Path, extra: &[&str]) -> Child {
        Command::new("nvim")
            .args(["--clean", "--embed", "--headless"])
            .args(extra)
            .current_dir(dir)
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("nvim must be on PATH for this test")
    }

    fn running(pid: u32) -> bool {
        // A zombie is not running: it is waiting to be reaped.
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                let after = &stat[stat.rfind(')')? + 1..];
                after.split_whitespace().next().map(|state| state != "Z")
            })
            .unwrap_or(false)
    }

    #[test]
    fn the_new_embed_child_is_found_and_nothing_else() {
        let _serial = SPAWNING.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch("find");
        let before = direct_children();
        let mut other = Command::new("sleep").arg("30").spawn().expect("sleep");
        assert_eq!(
            NvimChild::find_new(&before).map(|c| c.pid()),
            None,
            "a child without --embed is not nvim"
        );
        let mut nvim = spawn_embed(&dir, &[]);
        let found = NvimChild::find_new(&before).expect("the nvim --embed child");
        assert_eq!(found.pid(), nvim.id());
        assert!(
            NvimChild::find_new(&direct_children()).is_none(),
            "a child already there before is not new"
        );
        let _ = other.kill();
        let _ = other.wait();
        let _ = nvim.kill();
        let _ = nvim.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Short enough for a test; the product's own is [`SCHEDULE`].
    const QUICK: Schedule = Schedule {
        term_after: Duration::from_millis(300),
        term_every: Duration::from_millis(200),
        kill_after: Duration::from_millis(1500),
    };

    /// An nvim already gone -- exited and reaped by its owner -- is signalled nothing, and `end`
    /// returns at once: the pidfd pins the process, not the number a new one may have taken.
    #[test]
    fn an_nvim_already_gone_is_signalled_nothing() {
        let _serial = SPAWNING.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch("gone");
        let before = direct_children();
        let mut nvim = spawn_embed(&dir, &[]);
        let process = NvimProcess::new(NvimChild::find_new(&before));
        nvim.kill().expect("kill");
        nvim.wait().expect("reaped by its owner");
        let started = Instant::now();
        assert_eq!(end(&process, Instant::now(), QUICK), Ended::Exited);
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "no wait for what is gone"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `nvim` on `PATH` that is a shell script running nvim without `exec`.
    fn write_wrapper(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("wrap");
        std::fs::create_dir_all(&bin).expect("wrapper dir");
        let wrapper = bin.join("nvim");
        let real = std::process::Command::new("sh")
            .args(["-c", "command -v nvim"])
            .output()
            .expect("sh");
        let real = String::from_utf8(real.stdout).expect("utf-8").trim().to_string();
        std::fs::write(&wrapper, format!("#!/bin/sh\n{real} \"$@\"\n")).expect("write the wrapper");
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        wrapper
    }

    /// The direct children of `pid` whose argv carries `--embed`, waited for up to 5 s.
    fn embed_child_of(pid: u32) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let found: Vec<u32> = imp::children_of(pid)
                .into_iter()
                .filter(|p| imp::argv_has_embed(*p))
                .collect();
            if let [only] = found.as_slice() {
                return *only;
            }
            assert!(Instant::now() < deadline, "the wrapper never started nvim");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_gone(pid: u32, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while running(pid) {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    /// A launcher can be the harness's direct child and carry `--embed` in its arguments while
    /// running nvim without `exec`. `end` must watch and signal nvim itself, since killing the launcher
    /// can leave a hung nvim running.
    #[test]
    fn a_launcher_that_does_not_exec_nvim_is_not_what_an_end_reaches() {
        let _serial = SPAWNING.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch("wrapper");
        let wrapper = write_wrapper(&dir);
        let before = direct_children();
        let mut launched = Command::new(&wrapper)
            .args(["--clean", "--embed", "--headless"])
            .current_dir(&dir)
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the wrapper");
        let nvim = embed_child_of(launched.id());
        let process = NvimProcess::new(NvimChild::find_new(&before));
        assert_eq!(
            process.launched_pid(),
            Some(launched.id()),
            "the fork's child is the launcher"
        );
        let watched = process.nvim_pid();
        // SIGSTOP, so neither the hang-up nor SIGTERM ends it and `end` must reach the pid it holds.
        // SAFETY: a signal to the nvim this test's own wrapper started.
        unsafe { libc::kill(nvim as libc::pid_t, libc::SIGSTOP) };
        drop(launched.stdin.take());
        let ended = end(&process, Instant::now(), QUICK);
        let nvim_gone = wait_gone(nvim, Duration::from_secs(2));
        // Cleanup first, by the pids this test started, so a failing assert leaves nothing behind.
        if !nvim_gone {
            // SAFETY: as above.
            unsafe { libc::kill(nvim as libc::pid_t, libc::SIGKILL) };
        }
        let _ = launched.kill();
        let _ = launched.wait();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(watched, Some(nvim), "nvim is what is watched, not its launcher");
        assert_eq!(ended, Ended::Killed);
        assert!(nvim_gone, "nvim itself was ended");
    }

    /// The other half of C2: a new `--embed` child that is not nvim and has no nvim under it (a
    /// launcher that is still starting, or something else entirely) is never killed in nvim's place
    /// -- the pid is "not known", and the caller says so instead of guessing.
    #[test]
    fn a_direct_child_that_is_not_nvim_is_never_killed() {
        let _serial = SPAWNING.lock().unwrap_or_else(|e| e.into_inner());
        let before = direct_children();
        // Two commands, so the shell forks `sleep` instead of exec'ing it and stays itself.
        let mut other = Command::new("sh")
            .args(["-c", "sleep 30; :", "--embed"])
            .spawn()
            .expect("sh");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !imp::argv_has_embed(other.id()) {
            assert!(Instant::now() < deadline, "sh never started");
            std::thread::sleep(Duration::from_millis(10));
        }
        let process = NvimProcess::new(NvimChild::find_new(&before));
        let watched = process.nvim_pid();
        let ended = end(&process, Instant::now(), QUICK);
        std::thread::sleep(Duration::from_millis(100));
        let alive = running(other.id());
        let _ = other.kill();
        let _ = other.wait();
        assert_eq!(watched, None, "no process is known to be nvim");
        assert_eq!(ended, Ended::Unknown, "nothing is signalled");
        assert!(alive, "and the process that is not nvim is still running");
    }

    /// C2's preferred fix: nvim's own `getpid()`, asked over RPC as the pane asks it once the session
    /// is up, names nvim under any launcher, and is what `end` reaches. The case is two launchers
    /// deep, where looking at processes finds nothing (the fork's child is a shell, and so is its
    /// `--embed` child): only nvim's own answer reaches it. A pid that is no longer a live `--embed`
    /// process is refused. The connection keeps nvim's stdin open here, as a still-running fork
    /// would, so SIGTERM is what ends it.
    #[test]
    fn nvims_own_pid_is_what_an_end_reaches() {
        use nvim_rs::rpc::handler::Dummy;
        use std::os::unix::fs::PermissionsExt;
        let _serial = SPAWNING.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch("reported");
        let inner = write_wrapper(&dir);
        let wrapper = dir.join("outer-nvim");
        std::fs::write(&wrapper, format!("#!/bin/sh\n{} \"$@\"\n", inner.display())).expect("write the outer wrapper");
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let before = direct_children();
        let (nvim, _io, mut launched) = runtime
            .block_on(async {
                nvim_rs::create::tokio::new_child_cmd(
                    tokio::process::Command::new(&wrapper)
                        .args(["--clean", "--embed", "-n"])
                        .current_dir(&dir)
                        .env("XDG_STATE_HOME", dir.join("state")),
                    Dummy::new(),
                )
                .await
            })
            .expect("the wrapper");
        let launcher = launched.id().expect("pid");
        let process = NvimProcess::new(NvimChild::find_new(&before));
        assert_eq!(process.launched_pid(), Some(launcher));
        let reported = crate::nvim_rpc::block_on(nvim.call_function("getpid", vec![]))
            .ok()
            .and_then(|pid| pid.as_u64())
            .expect("nvim answers getpid") as u32;
        assert_ne!(reported, launcher, "the case: nvim is not the process the fork started");
        assert_eq!(
            process.nvim_pid(),
            None,
            "the case: without nvim's answer no process is known to be nvim"
        );
        assert!(process.learn(reported), "a live --embed process is held");
        assert!(!process.learn(launcher + 1_000_000), "a pid that is no nvim is refused");
        assert_eq!(process.nvim_pid(), Some(reported));

        let ended = end(&process, Instant::now(), QUICK);
        let nvim_gone = wait_gone(reported, Duration::from_secs(2));
        if !nvim_gone {
            // SAFETY: a signal to the nvim this test's own wrapper started.
            unsafe { libc::kill(reported as libc::pid_t, libc::SIGKILL) };
        }
        let _ = runtime.block_on(launched.wait());
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(ended, Ended::Terminated);
        assert!(nvim_gone, "nvim itself is gone");
    }

    /// Killing a launcher that did not `exec` leaves nvim alive, re-parented and holding the pipes,
    /// even though the harness reports an exit. Both the pid reported by nvim and the child found
    /// beneath the launcher must show nvim alive until it is ended, then show it gone.
    #[test]
    fn nvim_outliving_its_launcher_is_seen_alive() {
        use nvim_rs::rpc::handler::Dummy;
        let _serial = SPAWNING.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch("orphan");
        let wrapper = write_wrapper(&dir);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let before = direct_children();
        let (nvim, _io, mut launched) = runtime
            .block_on(async {
                nvim_rs::create::tokio::new_child_cmd(
                    tokio::process::Command::new(&wrapper)
                        .args(["--clean", "--embed", "-n"])
                        .current_dir(&dir)
                        .env("XDG_STATE_HOME", dir.join("state")),
                    Dummy::new(),
                )
                .await
            })
            .expect("the wrapper");
        let launcher = launched.id().expect("pid");
        let reported = crate::nvim_rpc::block_on(nvim.call_function("getpid", vec![]))
            .ok()
            .and_then(|pid| pid.as_u64())
            .expect("nvim answers getpid") as u32;
        // Found at launch, before nvim's answer: the nvim under the launcher.
        let found = NvimProcess::new(NvimChild::find_new(&before));
        // And by nvim's own answer.
        let told = NvimProcess::new(None);
        assert!(told.learn(reported));

        // SAFETY: a signal to the launcher this test spawned and still holds.
        unsafe { libc::kill(launcher as libc::pid_t, libc::SIGKILL) };
        let _ = runtime.block_on(launched.wait());
        let orphan_alive = running(reported);
        let seen_found = found.alive_within(Duration::from_millis(100));
        let seen_told = told.alive_within(Duration::from_millis(100));
        let ended = end(&found, Instant::now(), QUICK);
        let gone_found = found.alive_within(Duration::from_secs(2));
        let gone_told = told.alive_within(Duration::from_secs(2));
        if running(reported) {
            // SAFETY: the nvim this test's own wrapper started.
            unsafe { libc::kill(reported as libc::pid_t, libc::SIGKILL) };
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(orphan_alive, "the case: nvim outlives its launcher");
        assert_eq!(
            seen_found,
            Some(true),
            "the nvim found under the launcher at launch is alive"
        );
        assert_eq!(seen_told, Some(true), "nvim by its own answer is alive");
        assert_eq!(
            ended,
            Ended::Terminated,
            "SIGTERM ends it; its stdin is still open here"
        );
        assert_eq!(gone_found, Some(false));
        assert_eq!(gone_told, Some(false));
        assert_eq!(
            NvimProcess::new(None).alive_within(Duration::ZERO),
            None,
            "nothing known"
        );
    }

    /// Checks ending nvim with an unsaved, unsynced edit after closing stdin.
    /// Idle nvim exits on EOF. In a `:confirm` dialog, nvim 0.12.5 needs SIGTERM, whose handler writes
    /// the edit to the swap file. A stopped process needs SIGKILL, which cannot flush the edit.
    /// The original file is never written and the swap file remains in every case.
    fn ending(case: &str, dialog: bool, stop: bool) -> (Ended, bool, bool, bool) {
        use std::io::Write;
        let _serial = SPAWNING.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch(case);
        let swap = dir.join("swap");
        std::fs::create_dir_all(&swap).expect("swap dir");
        let directory = format!("set directory={}//", swap.display());
        let before = direct_children();
        // A UI attaches, as the fork's does: `:confirm`'s dialog then waits for that UI's keys.
        let mut nvim = Command::new("nvim")
            // The file on the command line, as a launch gives it: its swap file is made when it loads,
            // so an edit made after that is in the buffer and not yet in the swap file.
            .args([
                "--clean",
                "--embed",
                "--cmd",
                &directory,
                "a.txt",
                "-c",
                "call setline(1, 'unsynced edit')",
            ])
            .current_dir(&dir)
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("nvim must be on PATH for this test");
        let process = NvimProcess::new(NvimChild::find_new(&before));
        let mut stdout = nvim.stdout.take().expect("stdout");
        std::thread::spawn(move || std::io::copy(&mut stdout, &mut std::io::sink()));
        let request = |id: u64, method: &str, params: Vec<nvim_rs::Value>| {
            let mut bytes = Vec::new();
            let message = nvim_rs::Value::Array(vec![0.into(), id.into(), method.into(), params.into()]);
            rmpv::encode::write_value(&mut bytes, &message).expect("encode");
            bytes
        };
        let mut stdin = nvim.stdin.take().expect("stdin");
        let options = nvim_rs::Value::Map(vec![("ext_linegrid".into(), true.into())]);
        stdin
            .write_all(&request(1, "nvim_ui_attach", vec![80.into(), 24.into(), options]))
            .unwrap();
        stdin.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::fs::read_dir(&swap).map(|d| d.count()).unwrap_or(0) == 0 {
            if Instant::now() >= deadline {
                // Not left running behind a failed test.
                let _ = nvim.kill();
                let _ = nvim.wait();
                panic!("nvim never made its swap file");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // Once the file is loaded and edited, never before: a `:confirm qall` over nothing modified
        // simply quits.
        if dialog {
            let lua = "pcall(vim.cmd, 'confirm qall')";
            stdin
                .write_all(&request(
                    3,
                    "nvim_exec_lua",
                    vec![lua.into(), nvim_rs::Value::Array(vec![])],
                ))
                .unwrap();
            stdin.flush().unwrap();
        }
        std::thread::sleep(Duration::from_millis(300));
        if stop {
            // SAFETY: a signal to the child this test spawned and still holds.
            unsafe { libc::kill(nvim.id() as libc::pid_t, libc::SIGSTOP) };
        }
        drop(stdin);
        let schedule = Schedule {
            term_after: Duration::from_millis(500),
            term_every: Duration::from_millis(250),
            kill_after: Duration::from_millis(2000),
        };
        let ended = end(&process, Instant::now(), schedule);
        let gone = !running(nvim.id());
        let swap_files: Vec<PathBuf> = std::fs::read_dir(&swap)
            .map(|d| d.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        let edit_in_swap = swap_files
            .iter()
            .any(|f| std::fs::read(f).is_ok_and(|b| b.windows(13).any(|w| w == b"unsynced edit")));
        let written = dir.join("a.txt").exists();
        if !gone {
            let _ = nvim.kill();
        }
        let _ = nvim.wait();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(gone, "{case}: nvim is gone after end() ({ended:?})");
        assert!(!swap_files.is_empty(), "{case}: the swap file stays ({ended:?})");
        assert!(!written, "{case}: nothing was written over the file");
        (ended, gone, !swap_files.is_empty(), edit_in_swap)
    }

    #[test]
    fn an_idle_nvim_exits_on_the_hang_up_keeping_its_edit() {
        let (ended, _, _, edit_in_swap) = ending("end-idle", false, false);
        assert_eq!(ended, Ended::Exited);
        assert!(
            edit_in_swap,
            "nvim wrote the unsynced edit into its swap file on the way out"
        );
    }

    #[test]
    fn an_nvim_in_a_dialog_is_terminated_keeping_its_edit() {
        let (ended, _, _, edit_in_swap) = ending("end-dialog", true, false);
        assert_eq!(ended, Ended::Terminated, "the hang-up alone leaves nvim in its dialog");
        assert!(edit_in_swap, "SIGTERM's own handler preserved the edit");
    }

    #[test]
    fn a_stopped_nvim_is_killed_at_the_patience() {
        let (ended, _, _, _) = ending("end-stopped", false, true);
        assert_eq!(ended, Ended::Killed);
    }

    #[test]
    fn nothing_known_is_nothing_ended() {
        let schedule = Schedule {
            term_after: Duration::ZERO,
            term_every: Duration::from_millis(10),
            kill_after: Duration::from_millis(10),
        };
        assert_eq!(end(&NvimProcess::new(None), Instant::now(), schedule), Ended::Unknown);
    }
}
