//! The embedded nvim's own process, and how Eitri ends it (v1 hardening Task 6 and its fix
//! rounds; codex pre-release review part 1, finding 2).
//!
//! **Ending nvim, never `:qa!`** (round 4, the main session's ruling, 2026-09-27). `:qa!` discards
//! unsaved buffers AND deletes their swap files; the pinned fork's own shutdown used to send it.
//! Now ending an editor is only: nvim's own `:confirm qall` (it asks the user, `shell`'s
//! `editor_quit`); closing its stdin (the fork's `LiveHarness::hang_up`), on which nvim exits
//! keeping its swap files; and, where that did not end it, [`end`]'s escalation by the pid held
//! here -- SIGTERM, which nvim answers the same way and which ends a dialog EOF does not, then
//! SIGKILL once [`SCHEDULE`]'s patience is spent. Where nvim may be in a prompt, SIGTERM comes
//! before the hang-up, not after it ([`SCHEDULE_IN_A_PROMPT`], the quit follow-up: after EOF, the
//! fork's own exit request fails and nvim waits for a key it can never get). `tests/
//! no_force_quit.rs` holds the rule.
//!
//! **Which process is nvim.** Two sources, the better one first ([`NvimProcess`]):
//!
//! 1. **nvim's own answer.** Once the session is up the pane asks nvim `getpid()` over RPC, on a
//!    thread of its own ([`NvimProcess::learn`]). What answers on the fork's own pipe is nvim,
//!    whatever started it. Until the fix round after the Task 6 review nothing asked: the pane took
//!    the fork's direct child for nvim, and a `nvim` on `PATH` that is a shell script running nvim
//!    without `exec` -- or a `NEOVIM_BIN` launcher -- IS that child, `--embed` in its argv and all.
//!    A forced end then reached the launcher and left the hung nvim running (codex finding C2, the
//!    Opus review's T6-2; `a_launcher_that_does_not_exec_nvim_is_not_what_an_end_reaches`).
//! 2. **Until nvim answers** (it hung in its config before its loop ever ran): the process the fork
//!    started -- found by listing this process's direct children just before
//!    `LiveHarness::with_options` and again just after it, the one new child whose argv carries
//!    `--embed` ([`NvimChild::find_new`]) -- and only if it is nvim, its `/proc/<pid>/exe` a file
//!    named `nvim`. Otherwise the one `--embed` child of that process that is (a launcher's nvim),
//!    held from the launch on, since a launcher that exits leaves nvim re-parented and no longer
//!    under it. Neither: the pid is not known, nothing is signalled ([`Ended::Unknown`]), and only
//!    the fork's own report says whether nvim ended.
//!
//! A pidfd is taken on whatever is held, which pins that exact process: a later signal cannot reach
//! another process that reused the pid, and one sent to a process that already exited is a no-op
//! (`ESRCH`). Without a pidfd (a kernel before 5.3) a signal checks again, just before it is sent,
//! that the pid still carries `--embed` and, for the fork's own child, is still ours.
//!
//! Only Linux is implemented: elsewhere nothing is ever held, so nothing is signalled and only the
//! hang-up ends nvim.

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
    /// Source 2, resolved at launch -- when the fork's child is not nvim itself, the one nvim
    /// `--embed` child under it, held by a pidfd from then on. Found while the launcher still runs:
    /// once it has exited (round 3, codex finding 1), nvim is re-parented and no longer under it,
    /// and only this hold still says whether nvim lives.
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

/// When [`end`] closes nvim's stdin and escalates, counted from the moment the ending starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Schedule {
    /// nvim's stdin is closed (the fork's `LiveHarness::hang_up`) then, if nvim still lives.
    pub(crate) hang_up_after: Duration,
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

/// The schedule Eitri ends nvim on (the round-4 ruling, 2026-09-27; [`end`]): stdin closed at
/// once, SIGTERM from 1 s, SIGKILL at 5 s.
pub(crate) const SCHEDULE: Schedule = Schedule {
    hang_up_after: Duration::ZERO,
    term_after: Duration::from_secs(1),
    term_every: Duration::from_secs(1),
    kill_after: Duration::from_secs(5),
};

/// The schedule for an nvim that may be in a prompt (the quit follow-up, 2026-09-28): SIGTERM at
/// once, while nvim's stdin -- its one channel to the UI -- is still open; stdin closed at 1 s;
/// SIGTERM again each second; SIGKILL at 5 s, still the last resort.
///
/// Why the order matters there, measured on nvim 0.12.5 under the pinned fork's own `lua/init.lua`
/// (`an_nvim_in_its_quit_dialog_exits_on_sigterm_sent_first`, and the matrix in
/// `ending_orders_in_a_quit_dialog`): EOF during `:confirm qall`'s dialog does nothing (the stdio
/// channel's exit event skips `preserve_exit` while `exiting` is set, and `:qall` sets it before
/// it asks). A SIGTERM after it runs `preserve_exit` -- the swap files get what nvim had not synced,
/// deadly signals are refused from then on -- and `getout`, whose `VimLeavePre` runs the fork's
/// `rpcrequest(neovide_channel_id, "neovide.quit", ...)` on the channel EOF already closed. That
/// error sets `did_emsg`, so `getout` calls `wait_return`, which inside the dialog's own prompt
/// (`vgetc_busy == 0`) waits for a key no UI can send; the later SIGTERMs are refused, and only the
/// SIGKILL at 5 s ends it (the GUI re-check's 2/2). Sent first, SIGTERM finds the channel open, the
/// fork answers, and nvim exits within milliseconds, the edit in the swap either way.
pub(crate) const SCHEDULE_IN_A_PROMPT: Schedule = Schedule {
    hang_up_after: Duration::from_secs(1),
    term_after: Duration::ZERO,
    term_every: Duration::from_secs(1),
    kill_after: Duration::from_secs(5),
};

/// [`SCHEDULE_IN_A_PROMPT`] when nvim may be in a prompt -- the pane knows it only for its own quit,
/// whose `:confirm qall` request has not come back -- else [`SCHEDULE`], the round-4 ruling's order,
/// which idle nvim answers on the hang-up alone.
pub(crate) fn schedule(may_be_in_a_prompt: bool) -> Schedule {
    if may_be_in_a_prompt {
        SCHEDULE_IN_A_PROMPT
    } else {
        SCHEDULE
    }
}

/// Ends nvim on `schedule`, counted from `started_at`, and returns how. `hang_up` closes nvim's stdin
/// (the fork's `LiveHarness::hang_up`); it is called once, at `hang_up_after`, or sooner when nvim
/// is gone or no process is known to be nvim -- a caller that closes stdin itself passes a no-op.
///
/// Never `:qa!`, which Eitri never sends (the round-4 ruling; `tests/no_force_quit.rs`): on EOF
/// idle nvim exits keeping the swap files of modified buffers, and writes what it had not synced
/// into them (measured on 0.12.5, `an_idle_nvim_exits_on_the_hang_up_keeping_its_edit`). SIGTERM
/// from `term_after`, once per `term_every`: nvim's own deadly-signal handler answers it as it
/// answers EOF (`preserve_exit`: swap files kept, edits written into them), and ends a dialog,
/// which EOF does not -- sent before the hang-up where nvim may be in a prompt
/// ([`SCHEDULE_IN_A_PROMPT`]'s doc says why). Only when nothing ended it by `kill_after` (a stopped
/// process, a Lua busy loop, a prompt `getout` waits in) SIGKILL by the pid held, which keeps the
/// swap files without what nvim had not written to them. Blocks until one of those, at most
/// `kill_after` plus a reap; `Unknown` at once, after the hang-up, when no process is known to be
/// nvim.
pub(crate) fn end(process: &NvimProcess, started_at: Instant, schedule: Schedule, hang_up: &mut dyn FnMut()) -> Ended {
    match process.with_nvim(|nvim| end_one(nvim, started_at, schedule, &mut *hang_up)) {
        Some(ended) => ended,
        None => {
            hang_up();
            Ended::Unknown
        }
    }
}

fn end_one(nvim: &NvimChild, started_at: Instant, schedule: Schedule, hang_up: &mut dyn FnMut()) -> Ended {
    let mut hung_up = false;
    let mut terminated = false;
    let mut next_term = schedule.term_after;
    let mut hang_up_once = |hung_up: &mut bool| {
        if !*hung_up {
            *hung_up = true;
            hang_up();
        }
    };
    loop {
        let gone = |terminated| if terminated { Ended::Terminated } else { Ended::Exited };
        if !imp::alive_within(nvim, Duration::ZERO) {
            hang_up_once(&mut hung_up);
            return gone(terminated);
        }
        let elapsed = started_at.elapsed();
        if elapsed >= schedule.hang_up_after {
            hang_up_once(&mut hung_up);
        }
        if elapsed >= schedule.kill_after {
            let delivered = imp::signal(nvim, libc::SIGKILL);
            let reaped = nvim.reap(Duration::from_millis(500));
            println!(
                "[live] nvim (pid {}) did not exit within {:?}: SIGKILL {}, {}",
                nvim.pid(),
                schedule.kill_after,
                if delivered { "sent" } else { "not needed" },
                if reaped { "gone" } else { "not yet gone" },
            );
            return Ended::Killed;
        }
        if elapsed >= next_term && next_term < schedule.kill_after {
            if imp::signal(nvim, libc::SIGTERM) && !terminated {
                terminated = true;
                println!(
                    "[live] nvim (pid {}): SIGTERM {}",
                    nvim.pid(),
                    if hung_up {
                        "(it did not exit on its hang-up)"
                    } else {
                        "(its stdin still open: it may be in a prompt)"
                    }
                );
            }
            next_term += schedule.term_every;
        }
        let mut checkpoint = schedule.kill_after;
        if !hung_up {
            checkpoint = checkpoint.min(schedule.hang_up_after);
        }
        if next_term < schedule.kill_after {
            checkpoint = checkpoint.min(next_term);
        }
        // Waits for the next step, or for nvim to go; `alive_within` answers at once when it has.
        let _ = imp::alive_within(nvim, checkpoint.saturating_sub(started_at.elapsed()));
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

/// Every test in this crate that spawns an `nvim` takes this first: [`NvimChild::find_new`] looks
/// at every child the test process has, so a concurrent spawn elsewhere (`nvim_rpc`'s real-nvim
/// test) would be "new" too (codex finding C5).
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
        hang_up_after: Duration::ZERO,
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
        assert_eq!(end(&process, Instant::now(), QUICK, &mut || {}), Ended::Exited);
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

    /// Codex's finding C2 (= the Opus review's T6-2): a `nvim` that is a launcher running nvim
    /// without `exec` is the fork's direct child, with `--embed` in its argv. What `end` watches and
    /// signals must be nvim itself, not the launcher -- a killed launcher leaves the hung nvim
    /// running.
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
        let mut stdin = launched.stdin.take();
        let ended = end(&process, Instant::now(), QUICK, &mut || drop(stdin.take()));
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
        let ended = end(&process, Instant::now(), QUICK, &mut || {});
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

        let ended = end(&process, Instant::now(), QUICK, &mut || {});
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

    /// Round 3, codex finding 1: a launcher that does not `exec` nvim exits (here: is killed) and nvim
    /// lives on, re-parented, still holding the pipes -- which is when the fork reports nvim exited.
    /// What the pane holds says nvim is alive; after nvim is ended, that it is gone. Found both by
    /// nvim's own answer and, before it answers, by the nvim found under the launcher at launch.
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
        let ended = end(&found, Instant::now(), QUICK, &mut || {});
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

    /// The round-4 ruling's endings, measured on the nvim on `PATH`: nvim with an unsaved, unsynced
    /// edit, its stdin closed, then [`end`]'s schedule. Idle, it exits on the hang-up alone; with a
    /// `:confirm` dialog up the hang-up does nothing (nvim 0.12.5 stays in the dialog), and SIGTERM
    /// ends it -- nvim's own deadly-signal handler keeps the swap file AND writes the edit into it,
    /// which SIGKILL does not; a stopped nvim answers neither and is killed at the patience. The file
    /// itself is never written, and the swap file always stays.
    ///
    /// **This UI is not the fork's** (the quit follow-up): it answers nothing and installs no
    /// `VimLeavePre`, so a SIGTERM after the hang-up ends the dialog here and did not under the
    /// fork. [`fork_nvim`] is the fork's UI, and the ending order is tested there.
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
        let schedule = Schedule {
            hang_up_after: Duration::ZERO,
            term_after: Duration::from_millis(500),
            term_every: Duration::from_millis(250),
            kill_after: Duration::from_millis(2000),
        };
        let mut stdin = Some(stdin);
        let ended = end(&process, Instant::now(), schedule, &mut || drop(stdin.take()));
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

    /// The pinned fork's `lua/init.lua`, as cargo resolved the fork for this workspace
    /// (`tests/no_force_quit.rs` finds it the same way): what the fork runs in every nvim it starts.
    fn fork_init_lua() -> String {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the workspace root");
        let output = Command::new(env!("CARGO"))
            .args([
                "metadata",
                "--format-version",
                "1",
                "--offline",
                "--locked",
                "--manifest-path",
            ])
            .arg(workspace.join("Cargo.toml"))
            .output()
            .expect("cargo metadata runs");
        assert!(
            output.status.success(),
            "cargo metadata: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).expect("metadata is JSON");
        let manifest = metadata["packages"]
            .as_array()
            .expect("packages")
            .iter()
            .find(|p| p["name"] == "neovide")
            .expect("the neovide fork is a dependency")["manifest_path"]
            .as_str()
            .expect("a manifest path")
            .to_owned();
        let init = Path::new(&manifest)
            .parent()
            .expect("its directory")
            .join("lua/init.lua");
        let lua = std::fs::read_to_string(&init).unwrap_or_else(|e| panic!("{}: {e}", init.display()));
        assert!(
            lua.contains("\"VimLeavePre\"") && lua.contains("rpcrequest(\"neovide.quit\""),
            "the case: the fork's VimLeavePre asks its UI, by request, to quit ({})",
            init.display()
        );
        lua
    }

    /// The fork's own UI answers requests: `neovide.quit`, the one its `VimLeavePre` makes, with nil
    /// (`bridge/handler.rs`). This answers every request with nil, which is all an exit asks of it.
    /// Written out as `async_trait` expands it, which is how nvim-rs declares the trait.
    #[derive(Clone)]
    struct ForkUi;

    impl nvim_rs::Handler for ForkUi {
        type Writer = neovide::bridge::NeovimWriter;

        fn handle_request<'life0, 'async_trait>(
            &'life0 self,
            _name: String,
            _args: Vec<nvim_rs::Value>,
            _neovim: nvim_rs::Neovim<Self::Writer>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<nvim_rs::Value, nvim_rs::Value>> + Send + 'async_trait>,
        >
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async { Ok(nvim_rs::Value::Nil) })
        }
    }

    /// nvim as the pinned fork starts it for Eitri, with an edit made half a second before its
    /// ending: `--embed` over the fork's own session (whose `HangUp` closes stdin while the
    /// connection lives), the fork's `lua/init.lua` run as `setup_neovide_specific_state` runs it, a
    /// UI attached with the fork's options and answering as the fork does. `a.txt` holds "original"
    /// on disk; the edit, "late edit", is not in the swap file when the ending starts
    /// (`updatetime` as given, and asserted). With `dialog`, `:confirm qall` is open, sent as the
    /// pane sends it.
    struct ForkNvim {
        runtime: tokio::runtime::Runtime,
        session: neovide::bridge::session::NeovimSession,
        process: NvimProcess,
        pid: u32,
        dir: PathBuf,
        swap: PathBuf,
    }

    const LATE_EDIT: &[u8] = b"late edit";

    fn swap_holds(swap: &Path, text: &[u8]) -> bool {
        std::fs::read_dir(swap).is_ok_and(|dir| {
            dir.flatten().any(|entry| {
                std::fs::read(entry.path()).is_ok_and(|bytes| bytes.windows(text.len()).any(|w| w == text))
            })
        })
    }

    /// A request's answer, or a failed test naming it: nvim never hangs a test.
    fn answered<T>(what: &str, within: Result<T, tokio::time::error::Elapsed>) -> T {
        within.unwrap_or_else(|_| panic!("{what}: no answer within 5 s"))
    }

    fn fork_nvim(case: &str, updatetime: u32, dialog: bool) -> ForkNvim {
        use nvim_rs::Value;
        let dir = scratch(case);
        let swap = dir.join("swap");
        std::fs::create_dir_all(&swap).expect("swap dir");
        std::fs::write(dir.join("a.txt"), "original\n").expect("a.txt");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let mut command = tokio::process::Command::new("nvim");
        command
            .args(["--clean", "--embed", "--cmd"])
            .arg(format!("set directory={}//", swap.display()))
            .arg("--cmd")
            .arg(format!("set updatetime={updatetime}"))
            .arg("a.txt")
            .current_dir(&dir)
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("XDG_CONFIG_HOME", dir.join("config"));
        let session = runtime
            .block_on(neovide::bridge::session::NeovimSession::new(
                neovide::bridge::session::NeovimInstance::Embedded(command),
                ForkUi,
            ))
            .expect("nvim must be on PATH for this test");
        let nvim = session.neovim.clone();
        let init_lua = fork_init_lua();
        let pid = runtime.block_on(async {
            let deadline = Duration::from_secs(5);
            let info = answered(
                "get_api_info",
                tokio::time::timeout(deadline, nvim.get_api_info()).await,
            )
            .expect("api info");
            let args = Value::Map(vec![
                ("neovide_channel_id".into(), info[0].clone()),
                ("neovide_version".into(), "test".into()),
                ("config_path".into(), "".into()),
                ("register_clipboard".into(), false.into()),
                ("register_right_click".into(), false.into()),
                ("remote".into(), false.into()),
                ("global_variable_settings".into(), Value::Array(vec![])),
                ("option_settings".into(), Value::Array(vec![])),
            ]);
            answered(
                "init.lua",
                tokio::time::timeout(deadline, nvim.exec_lua(&init_lua, vec![args])).await,
            )
            .expect("the fork's init.lua runs");
            let mut options = nvim_rs::UiAttachOptions::new();
            options.set_linegrid_external(true);
            options.set_multigrid_external(true);
            options.set_rgb(true);
            answered(
                "ui_attach",
                tokio::time::timeout(deadline, nvim.ui_attach(80, 24, &options)).await,
            )
            .expect("ui_attach");
            answered(
                "getpid",
                tokio::time::timeout(deadline, nvim.call_function("getpid", vec![])).await,
            )
            .expect("getpid")
            .as_u64()
            .expect("a pid") as u32
        });
        let process = NvimProcess::new(None);
        assert!(process.learn(pid), "nvim is a live --embed process");
        let edited_at = Instant::now();
        runtime.block_on(async {
            let edit = nvim.exec_lua("vim.api.nvim_buf_set_lines(0, 0, -1, false, {'late edit'})", vec![]);
            answered("the edit", tokio::time::timeout(Duration::from_secs(5), edit).await).expect("the edit");
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::fs::read_dir(&swap).map(|d| d.count()).unwrap_or(0) == 0 {
            assert!(Instant::now() < deadline, "{case}: nvim never made its swap file");
            std::thread::sleep(Duration::from_millis(20));
        }
        if dialog {
            // Sent as the pane sends its quit, an RPC request that comes back only when the dialog is
            // answered -- here, never.
            let quit = nvim.clone();
            runtime.spawn(async move {
                let _ = quit.exec_lua("pcall(vim.cmd, 'confirm qall')", vec![]).await;
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let mode = runtime
                    .block_on(async { tokio::time::timeout(Duration::from_secs(1), nvim.get_mode()).await })
                    .ok()
                    .and_then(Result::ok)
                    .and_then(|pairs| {
                        pairs
                            .into_iter()
                            .find(|(k, _)| k.as_str() == Some("mode"))
                            .and_then(|(_, v)| v.as_str().map(str::to_owned))
                    });
                if mode.as_deref() == Some("r?") {
                    break;
                }
                assert!(Instant::now() < deadline, "{case}: the dialog never came up ({mode:?})");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        std::thread::sleep(Duration::from_millis(500).saturating_sub(edited_at.elapsed()));
        assert!(
            !swap_holds(&swap, LATE_EDIT),
            "{case}: the case -- the edit is not in the swap file yet"
        );
        ForkNvim {
            runtime,
            session,
            process,
            pid,
            dir,
            swap,
        }
    }

    /// What one ending of a [`fork_nvim`] did.
    #[derive(Debug)]
    struct Outcome {
        ended: Ended,
        took: Duration,
        /// The edit reached the swap file.
        kept: bool,
        /// nvim's own deadly-signal handler ran `preserve_exit` (it says so on stderr).
        preserved: bool,
    }

    /// Ends a [`fork_nvim`] on `schedule`; the hang-up is the fork's own `HangUp`.
    fn end_fork_nvim(case: &str, updatetime: u32, dialog: bool, schedule: Schedule) -> Outcome {
        let _serial = SPAWNING.lock().unwrap_or_else(|e| e.into_inner());
        let mut fork = fork_nvim(case, updatetime, dialog);
        let hang_up = fork.session.hang_up.clone();
        let started = Instant::now();
        let ended = end(&fork.process, started, schedule, &mut || hang_up.hang_up());
        let took = started.elapsed();
        let gone = wait_gone(fork.pid, Duration::from_secs(2));
        if !gone {
            // SAFETY: a signal to the nvim this test started, by the pid it reported.
            unsafe { libc::kill(fork.pid as libc::pid_t, libc::SIGKILL) };
        }
        let stderr = fork.session.stderr_task.take().map_or_else(Vec::new, |task| {
            fork.runtime
                .block_on(async { tokio::time::timeout(Duration::from_secs(2), task).await })
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default()
        });
        let kept = swap_holds(&fork.swap, LATE_EDIT);
        let untouched = std::fs::read_to_string(fork.dir.join("a.txt")).is_ok_and(|text| text == "original\n");
        let dir = fork.dir.clone();
        drop(fork.session);
        fork.runtime.shutdown_timeout(Duration::from_secs(1));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(gone, "{case}: nvim is gone after end() ({ended:?})");
        assert!(untouched, "{case}: nothing was written over the file");
        Outcome {
            ended,
            took,
            kept,
            preserved: stderr.iter().any(|line| line.contains("preserving files")),
        }
    }

    /// The quit follow-up, held: nvim in its own `:confirm qall` dialog -- the quit the pane sent is
    /// outstanding -- under the fork's UI ends on SIGTERM sent while its stdin is still open, within
    /// milliseconds, the edit made half a second before in its swap file even at an `updatetime` of
    /// 10 s. On the round-4 order (stdin closed first, SIGTERM at 1 s) the fork's exit request fails
    /// on the closed channel, nvim then waits for a key inside the dialog, and only the SIGKILL at
    /// 5 s ends it (the GUI re-check's 2/2; `ending_orders_in_a_quit_dialog` measures both).
    #[test]
    fn an_nvim_in_its_quit_dialog_exits_on_sigterm_sent_first() {
        let outcome = end_fork_nvim("fork-dialog", 10_000, true, schedule(true));
        assert_eq!(outcome.ended, Ended::Terminated, "{outcome:?}");
        assert!(
            outcome.took < SCHEDULE_IN_A_PROMPT.hang_up_after,
            "it exited before its stdin was closed: {outcome:?}"
        );
        assert!(outcome.kept && outcome.preserved, "{outcome:?}");
    }

    /// The other half: with no prompt known, the round-4 order stays -- idle nvim under the fork's
    /// UI exits on the hang-up alone, the edit in its swap file, no signal sent.
    #[test]
    fn an_idle_nvim_under_the_forks_ui_exits_on_the_hang_up() {
        let outcome = end_fork_nvim("fork-idle", 10_000, false, schedule(false));
        assert_eq!(outcome.ended, Ended::Exited, "{outcome:?}");
        assert!(outcome.took < SCHEDULE.term_after, "{outcome:?}");
        assert!(outcome.kept, "{outcome:?}");
    }

    /// The quit follow-up's measurement (task 1): each order an ending can take, on an nvim in its
    /// `:confirm qall` dialog under the fork's UI, at an `updatetime` of 4 s and of 10 s, the edit
    /// made 0.5 s before the ending. Ignored: it takes about 25 s, most of it waiting out SIGKILLs.
    /// Its assertions are the measurement (nvim 0.12.5, fork `c3a3fe6`), so a newer nvim or fork
    /// that behaves differently fails it rather than passing silently:
    ///
    /// | order | ends | edit in swap, 4 s | edit in swap, 10 s |
    /// |---|---|---|---|
    /// | EOF only | SIGKILL at 5 s | yes (nvim synced it idle, at 4 s) | **no** |
    /// | SIGTERM only | SIGTERM, ms | yes | yes |
    /// | EOF, SIGTERM from 1 s (round 4) | SIGKILL at 5 s | yes (SIGTERM's `preserve_exit`) | yes |
    /// | SIGTERM, EOF at 1 s (now) | SIGTERM, ms | yes | yes |
    ///
    /// And, idle, the round-4 order exits on the EOF and the new one on the SIGTERM.
    #[test]
    #[ignore = "a measurement: about 25 s of real nvim endings; run with --include-ignored"]
    fn ending_orders_in_a_quit_dialog() {
        let never = Duration::from_secs(5);
        let eof_only = Schedule {
            hang_up_after: Duration::ZERO,
            term_after: never,
            ..SCHEDULE
        };
        let sigterm_only = Schedule {
            hang_up_after: never,
            term_after: Duration::ZERO,
            ..SCHEDULE
        };
        let quick = Duration::from_millis(900);
        for updatetime in [4_000, 10_000] {
            let rows = [
                ("eof-only", eof_only, true),
                ("sigterm-only", sigterm_only, true),
                ("eof-then-sigterm", SCHEDULE, true),
                ("sigterm-then-eof", SCHEDULE_IN_A_PROMPT, true),
                ("idle-eof-then-sigterm", SCHEDULE, false),
                ("idle-sigterm-then-eof", SCHEDULE_IN_A_PROMPT, false),
            ];
            for (name, schedule, dialog) in rows {
                let outcome = end_fork_nvim(&format!("fork-{name}-{updatetime}"), updatetime, dialog, schedule);
                println!("updatetime {updatetime:>5}  {name:22} {outcome:?}");
                let (ended, fast, kept) = match name {
                    "eof-only" => (Ended::Killed, false, updatetime < 5_000),
                    "sigterm-only" | "sigterm-then-eof" | "idle-sigterm-then-eof" => (Ended::Terminated, true, true),
                    "eof-then-sigterm" => (Ended::Killed, false, true),
                    "idle-eof-then-sigterm" => (Ended::Exited, true, true),
                    _ => unreachable!(),
                };
                assert_eq!(outcome.ended, ended, "{name} at {updatetime}: {outcome:?}");
                assert_eq!(outcome.took < quick, fast, "{name} at {updatetime}: {outcome:?}");
                assert_eq!(outcome.kept, kept, "{name} at {updatetime}: {outcome:?}");
                if name == "eof-then-sigterm" {
                    assert!(outcome.preserved, "SIGTERM's handler ran before the wait: {outcome:?}");
                }
            }
        }
    }

    /// No process known to be nvim: nothing is signalled, and the hang-up -- all that can end it --
    /// happens at once, whatever the schedule says.
    #[test]
    fn nothing_known_is_nothing_ended() {
        let mut hung_up = 0;
        let started = Instant::now();
        let ended = end(&NvimProcess::new(None), started, SCHEDULE_IN_A_PROMPT, &mut || {
            hung_up += 1
        });
        assert_eq!(ended, Ended::Unknown);
        assert_eq!(hung_up, 1);
        assert!(started.elapsed() < Duration::from_millis(100), "nothing waited for");
    }
}
