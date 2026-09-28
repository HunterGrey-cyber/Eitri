//! Holds the embedded nvim process so it can be killed after shutdown times out or nvim
//! stops answering.
//!
//! `LiveHarness::shutdown` asks nvim to quit and waits 5 s for `NeovimExited`. If that event
//! never arrives, it returns `false` and leaves nvim running. The child pid is not exposed
//! to the caller of `bridge::NeovimRuntime::launch`, and dropping the fork's
//! `tokio::process::Child` does not kill it.
//!
//! [`NvimProcess`] uses nvim's own pid when available, with process discovery as a fallback.
//! Once the session is up, [`NvimProcess::learn`] asks nvim `getpid()` over RPC on a separate
//! thread. This identifies nvim even when a shell script on `PATH` or a `NEOVIM_BIN` launcher
//! starts it without `exec`. The fork's direct child can be that launcher, with `--embed`
//! in its argv; killing the launcher alone would leave nvim running.
//!
//! Before nvim answers, [`NvimChild::find_new`] compares direct children before and after
//! `LiveHarness::with_options` to find the new child with `--embed` in its argv. It accepts
//! that process only if `/proc/<pid>/exe` names `nvim`. Otherwise it looks for that process's
//! `--embed` child whose executable is `nvim`. If neither can be identified, nothing is
//! killed and the host reports [`Killed::Unknown`].
//!
//! A pidfd pins the held process, preventing kills from reaching another process that
//! reuses its pid. Killing a process that has already exited is a no-op (`ESRCH`).
//! Without a pidfd, as on kernels before 5.3, each kill first checks that the pid still
//! carries `--embed` and, for the fork's direct child, still belongs to this process.
//!
//! SIGKILL prevents nvim from cleaning up its swap files, leaving saved swap contents
//! available for recovery. The fork's `:qa!` shutdown deletes those files.
//! `a_killed_nvim_keeps_its_swap_file` checks that the swap file remains.
//!
//! Only Linux is implemented. Elsewhere no process is held, so a timed-out shutdown leaves
//! nvim running and a kill reports [`Killed::Unknown`].

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

    /// Sends SIGKILL. `true` if it was delivered, i.e. the process was still there.
    pub(crate) fn kill(&self) -> bool {
        imp::kill(self)
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

/// What a kill of nvim reached ([`NvimProcess::kill`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Killed {
    /// SIGKILL was delivered to nvim, this pid.
    Sent(u32),
    /// nvim, this pid, was already gone.
    AlreadyGone(u32),
    /// No process is known to be nvim: nothing was signalled.
    Unknown,
}

/// The embedded nvim's process, as far as it is known (the module doc's two sources).
#[derive(Debug)]
pub(crate) struct NvimProcess {
    /// What the fork started (source 2): nvim, or a launcher of it.
    launched: Option<NvimChild>,
    /// nvim's own `getpid()` answer (source 1), held once it came back.
    reported: OnceLock<NvimChild>,
}

impl NvimProcess {
    pub(crate) fn new(launched: Option<NvimChild>) -> NvimProcess {
        NvimProcess {
            launched,
            reported: OnceLock::new(),
        }
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

    /// Sends SIGKILL to nvim itself: its own `getpid()` answer, else what the fork started if that
    /// is nvim, else the nvim a launcher started. Never the launcher.
    pub(crate) fn kill(&self) -> Killed {
        self.with_nvim(|nvim| {
            if nvim.kill() {
                Killed::Sent(nvim.pid())
            } else {
                Killed::AlreadyGone(nvim.pid())
            }
        })
        .unwrap_or(Killed::Unknown)
    }

    fn with_nvim<R>(&self, f: impl FnOnce(&NvimChild) -> R) -> Option<R> {
        if let Some(nvim) = self.reported.get() {
            return Some(f(nvim));
        }
        let launched = self.launched.as_ref()?;
        // Gone already (a zombie has no `exe` to read): a kill of it is a no-op, as it always was.
        if !imp::is_alive(launched) || imp::exe_is_nvim(launched.pid) {
            return Some(f(launched));
        }
        let below = imp::nvim_child_of(launched.pid)?;
        Some(f(&below))
    }
}

/// What happens after `LiveHarness::shutdown` returned `exited_cleanly`: nothing when it did, and
/// otherwise nvim is killed and reaped. `true` when nvim is known to be gone.
pub(crate) fn after_shutdown(exited_cleanly: bool, process: &NvimProcess) -> bool {
    if exited_cleanly {
        return true;
    }
    let gone = process.with_nvim(|nvim| {
        let delivered = nvim.kill();
        let gone = nvim.reap(Duration::from_millis(500));
        println!(
            "[live] nvim (pid {}) did not exit within the shutdown's wait: SIGKILL {}, {}",
            nvim.pid(),
            if delivered { "sent" } else { "not needed (already gone)" },
            if gone { "gone" } else { "not yet gone" },
        );
        gone
    });
    gone.unwrap_or_else(|| {
        println!("[live] nvim did not exit and its pid is not known; it may be left running");
        false
    })
}

#[cfg(target_os = "linux")]
mod imp {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

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

    pub(super) fn kill(child: &NvimChild) -> bool {
        match &child.pidfd {
            Some(pidfd) => {
                // SAFETY: `pidfd` is a live fd we own; a null siginfo is what `kill(2)` would send.
                let rc = unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        pidfd.as_raw_fd(),
                        libc::SIGKILL,
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
                unsafe { libc::kill(child.pid as libc::pid_t, libc::SIGKILL) == 0 }
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
    pub(super) fn hold_checked(_pid: u32) -> Option<NvimChild> {
        None
    }
    pub(super) fn nvim_child_of(_parent: u32) -> Option<NvimChild> {
        None
    }
    pub(super) fn kill(_child: &NvimChild) -> bool {
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
        // A zombie is not running: it is waiting to be reaped, and `after_shutdown` reaps.
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

    /// Codex p1 #2: `LiveHarness::shutdown` returned `false` (it waited 5 s for an nvim that never
    /// quit). The nvim it leaves is killed and reaped: no nvim process is left.
    #[test]
    fn a_timed_out_shutdown_leaves_no_nvim_process() {
        let _serial = SPAWNING.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch("timeout");
        let before = direct_children();
        let nvim = spawn_embed(&dir, &[]);
        let pid = nvim.id();
        let child = NvimChild::find_new(&before).expect("the nvim --embed child");
        assert!(running(pid), "the control: nvim is running before the shutdown");

        assert!(
            after_shutdown(false, &NvimProcess::new(Some(child))),
            "a timed-out shutdown ends nvim"
        );
        assert!(!running(pid), "no nvim process is left");
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "and it was reaped, not left a zombie"
        );
        drop(nvim);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A shutdown that saw nvim exit kills nothing.
    #[test]
    fn a_clean_shutdown_kills_nothing() {
        let _serial = SPAWNING.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch("clean");
        let before = direct_children();
        let mut nvim = spawn_embed(&dir, &[]);
        let child = NvimChild::find_new(&before).expect("the nvim --embed child");
        assert!(after_shutdown(true, &NvimProcess::new(Some(child))));
        std::thread::sleep(Duration::from_millis(100));
        assert!(running(nvim.id()), "nothing was killed");
        let _ = nvim.kill();
        let _ = nvim.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A kill after the process is gone and reaped reaches nothing: the pidfd pins the process, not
    /// the number.
    #[test]
    fn a_kill_after_nvim_is_gone_reaches_nothing() {
        let _serial = SPAWNING.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch("gone");
        let before = direct_children();
        let mut nvim = spawn_embed(&dir, &[]);
        let child = NvimChild::find_new(&before).expect("the nvim --embed child");
        nvim.kill().expect("kill");
        nvim.wait().expect("reaped by its owner");
        assert!(!child.kill(), "nothing to signal");
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
    /// running nvim without `exec`. Signals must reach nvim itself, since killing the launcher
    /// can leave a hung nvim running.
    #[test]
    fn a_launcher_that_does_not_exec_nvim_is_not_what_a_kill_reaches() {
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
        let killed = process.kill();
        let nvim_gone = wait_gone(nvim, Duration::from_secs(2));
        // Cleanup first, by the pids this test started, so a failing assert leaves nothing behind.
        if !nvim_gone {
            // SAFETY: a signal to the nvim this test's own wrapper started.
            unsafe { libc::kill(nvim as libc::pid_t, libc::SIGKILL) };
        }
        let _ = launched.kill();
        let _ = launched.wait();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            killed,
            Killed::Sent(nvim),
            "the signal went to nvim, not to its launcher"
        );
        assert!(nvim_gone, "nvim itself was killed");
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
        let killed = NvimProcess::new(NvimChild::find_new(&before)).kill();
        std::thread::sleep(Duration::from_millis(100));
        let alive = running(other.id());
        let _ = other.kill();
        let _ = other.wait();
        assert_eq!(killed, Killed::Unknown, "nothing is signalled");
        assert!(alive, "and the process that is not nvim is still running");
    }

    /// C2's preferred fix: nvim's own `getpid()`, asked over RPC as the pane asks it once the session
    /// is up, names nvim even under a launcher, and is what a kill reaches -- before, and instead of,
    /// anything found by looking at processes. A pid that is no longer a live `--embed` process is
    /// refused.
    #[test]
    fn nvims_own_pid_is_what_a_kill_reaches() {
        use nvim_rs::rpc::handler::Dummy;
        let _serial = SPAWNING.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch("reported");
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
        let process = NvimProcess::new(NvimChild::find_new(&before));
        let reported = crate::nvim_rpc::block_on(nvim.call_function("getpid", vec![]))
            .ok()
            .and_then(|pid| pid.as_u64())
            .expect("nvim answers getpid") as u32;
        assert_ne!(reported, launcher, "the case: nvim is not the process the fork started");
        assert!(process.learn(reported), "a live --embed process is held");
        assert!(!process.learn(launcher + 1_000_000), "a pid that is no nvim is refused");

        let killed = process.kill();
        let nvim_gone = wait_gone(reported, Duration::from_secs(2));
        if !nvim_gone {
            // SAFETY: a signal to the nvim this test's own wrapper started.
            unsafe { libc::kill(reported as libc::pid_t, libc::SIGKILL) };
        }
        let _ = runtime.block_on(launched.wait());
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(killed, Killed::Sent(reported));
        assert!(nvim_gone, "nvim itself is gone");
    }

    /// SIGKILL leaves nvim's swap file available for recovering unsaved changes.
    /// `:qa!` deletes the swap file.
    #[test]
    fn a_killed_nvim_keeps_its_swap_file() {
        let _serial = SPAWNING.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch("swap");
        let swap = dir.join("swap");
        std::fs::create_dir_all(&swap).expect("swap dir");
        let directory = format!("set directory={}//", swap.display());
        let before = direct_children();
        let nvim = spawn_embed(
            &dir,
            &[
                "--cmd",
                &directory,
                "-c",
                "edit a.txt",
                "-c",
                "call setline(1, 'unsaved text')",
                "-c",
                "preserve",
            ],
        );
        let child = NvimChild::find_new(&before).expect("the nvim --embed child");
        let deadline = Instant::now() + Duration::from_secs(5);
        let swap_files = || std::fs::read_dir(&swap).map(|d| d.count()).unwrap_or(0);
        while swap_files() == 0 {
            assert!(Instant::now() < deadline, "nvim never wrote a swap file");
            std::thread::sleep(Duration::from_millis(20));
        }

        assert!(after_shutdown(false, &NvimProcess::new(Some(child))));
        assert_eq!(swap_files(), 1, "the swap file is still there");
        assert!(!dir.join("a.txt").exists(), "and nothing was written over the file");
        drop(nvim);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
