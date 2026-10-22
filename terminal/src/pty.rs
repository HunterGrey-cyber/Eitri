//! Own the pseudo-terminal child's creation, resizing, hangup, and reaping here.
//!
//! The `alacritty_terminal::tty` implementation has process-wide effects that do not fit a GTK
//! application with other child processes. It injects `WINDOWID` and `ALACRITTY_WINDOW_ID`,
//! exits the process on a failed `TIOCSWINSZ`, blocks in `child.wait()` during `Drop`, installs
//! a process-wide SIGCHLD handler, and changes the parent process's `TERM`. This implementation
//! avoids those effects.
//!
//! Child setup is the standard sequence, the one alacritty's Apache-2.0 `from_fd` also uses:
//! `setsid`, `TIOCSCTTY` on the new stdin, the six signal dispositions reset, IUTF8 on the tty.
//! No code is copied from it; the sequence matches because there is one correct sequence.
//!
//! Every child is reaped. The session reaps normal exits. On early return or panic,
//! [`PtyChild`]'s `Drop` sends hangup and hands the pid to a reaper thread, which kills it
//! after [`HANGUP_GRACE`] and reaps it without blocking the dropping thread.
//!
//! On macOS, `rustix-openpty` sets `CLOEXEC` after opening the PTY, so concurrent spawns can
//! inherit its descriptors and delay EOF. The `close_range` call is Linux-only; macOS children
//! can also inherit other descriptors without `CLOEXEC`. The descriptor-isolation guarantee and
//! its test therefore apply only on Linux.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::ManuallyDrop;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use rustix::termios::{self, InputModes, OptionalActions, Winsize};

use crate::metrics::TerminalMetrics;

/// Removed from the child's environment. An Eitri started from inside tmux would otherwise make
/// every program in its terminal believe it is inside THAT tmux: nested-session refusals,
/// vim-tmux-navigator driving the wrong server, `claude-wrapper` checking the wrong socket.
/// `NEOVIM_BIN` joins the other two: `shell`'s own choice of which
/// `nvim` the *editor* pane's Neovide runtime spawns must never reach a `neovide`/script started
/// inside this terminal -- that terminal is a separate program the user is running, not the fork.
pub const REMOVED_ENV: [&str; 3] = ["TMUX", "TMUX_PANE", "NEOVIM_BIN"];

/// Set on the child, replacing whatever the host had. `TERM` is `xterm-256color`;
/// `TERM_PROGRAM=eitri` lets shell configuration select eitri-only remaps.
pub const SET_ENV: [(&str, &str); 3] = [
    ("TERM", "xterm-256color"),
    ("COLORTERM", "truecolor"),
    ("TERM_PROGRAM", "eitri"),
];

/// How long a hung-up child gets to exit before it is killed by its pid.
pub const HANGUP_GRACE: Duration = Duration::from_millis(500);

/// The child's whole environment: the host's, minus [`REMOVED_ENV`], with [`SET_ENV`] on top.
/// Nothing else is added or removed -- the old sidecar scrubbed its child to 14 variables and broke
/// `wl-copy`, `ssh-agent` and D-Bus, which is what the in-process PTY exists to end. Computed as a
/// value and handed to `Command::env_clear().envs(..)`, never by `std::env::set_var`: this process
/// is multi-threaded, and its own environment must not change.
pub fn child_environment(host: impl IntoIterator<Item = (OsString, OsString)>) -> Vec<(OsString, OsString)> {
    let replaced = |key: &OsStr| {
        REMOVED_ENV.iter().any(|r| key == OsStr::new(r)) || SET_ENV.iter().any(|(s, _)| key == OsStr::new(s))
    };
    let mut env: Vec<(OsString, OsString)> = host.into_iter().filter(|(k, _)| !replaced(k)).collect();
    env.extend(SET_ENV.iter().map(|(k, v)| (OsString::from(k), OsString::from(v))));
    env
}

/// `$SHELL` when set and non-empty, else the passwd entry's shell, else `/bin/sh`. `passwd_shell`
/// is only called when `$SHELL` is missing: a passwd lookup can go through NSS, and the host calls
/// this on its UI thread.
pub fn choose_shell(env_shell: Option<OsString>, passwd_shell: impl FnOnce() -> Option<PathBuf>) -> PathBuf {
    match env_shell.filter(|s| !s.is_empty()) {
        Some(shell) => PathBuf::from(shell),
        None => passwd_shell().unwrap_or_else(|| PathBuf::from("/bin/sh")),
    }
}

/// The shell to try after every one in `tried` failed to start (a stale `$SHELL` naming an
/// uninstalled fish, say): the passwd entry's shell, then `/bin/sh`, each at most once. `None` when
/// both have been tried.
pub fn fallback_shell(tried: &[PathBuf], passwd_shell: Option<PathBuf>) -> Option<PathBuf> {
    passwd_shell
        .into_iter()
        .chain([PathBuf::from("/bin/sh")])
        .find(|shell| !tried.contains(shell))
}

/// The largest buffer [`passwd_shell`] will grow to before giving up. Four doublings past the
/// starting 4096 bytes -- 64KiB is already far past any real passwd entry, NSS/LDAP included.
const PASSWD_BUF_MAX_BYTES: usize = 64 * 1024;

/// The login shell for this uid, if present in the passwd database. Retry `ERANGE` with a
/// doubled buffer so a large NSS or LDAP entry does not look like a missing shell and silently
/// fall back to `/bin/sh`.
pub fn passwd_shell() -> Option<PathBuf> {
    let mut buf_size = 4096usize;
    loop {
        let mut buf = vec![0 as libc::c_char; buf_size];
        // SAFETY: `pwd` is plain data getpwuid_r fills in; every pointer in it points into `buf`,
        // which outlives every read below.
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = unsafe { libc::getpwuid_r(libc::getuid(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
        if rc == libc::ERANGE && buf_size < PASSWD_BUF_MAX_BYTES {
            buf_size *= 2;
            continue;
        }
        if rc != 0 || result.is_null() || pwd.pw_shell.is_null() {
            return None;
        }
        let shell = unsafe { std::ffi::CStr::from_ptr(pwd.pw_shell) }.to_bytes();
        return (!shell.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(shell)));
    }
}

/// What to run, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
}

impl SpawnSpec {
    /// The user's shell with no arguments, in `cwd`. No `-l`: an interactive zsh reads `.zshrc`
    /// because its stdin is a tty, which is what foot does too.
    pub fn user_shell(cwd: PathBuf) -> Self {
        SpawnSpec::shell(choose_shell(std::env::var_os("SHELL"), passwd_shell), cwd)
    }

    /// `program` with no arguments, in `cwd`: what [`fallback_shell`] names is run this way.
    pub fn shell(program: PathBuf, cwd: PathBuf) -> Self {
        SpawnSpec {
            program,
            args: Vec::new(),
            cwd,
        }
    }
}

/// The PTY's size, in cells and in pixels per cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub cols: u16,
    pub rows: u16,
    pub cell_width_px: u16,
    pub cell_height_px: u16,
}

impl PtySize {
    /// A grid is at least 1x1, and the pixel size is computed in `u32` and saturated: alacritty's
    /// own `to_winsize` multiplies in `u16`, which panics in a debug build on a wide terminal with
    /// a large font and wraps silently in a release one.
    pub fn to_winsize(self) -> Winsize {
        let (cols, rows) = (self.cols.max(1), self.rows.max(1));
        let px = |cells: u16, cell: u16| (u32::from(cells) * u32::from(cell)).min(u32::from(u16::MAX)) as u16;
        Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: px(cols, self.cell_width_px),
            ws_ypixel: px(rows, self.cell_height_px),
        }
    }

    /// The same size with the grid clamped to at least one cell, as [`PtySize::to_winsize`] and the
    /// `Term` both see it.
    pub fn clamped(self) -> Self {
        PtySize {
            cols: self.cols.max(1),
            rows: self.rows.max(1),
            ..self
        }
    }

    /// The size `metrics` currently lays out: its grid, and its cell rounded to whole device pixels.
    pub fn from_metrics(metrics: &TerminalMetrics) -> Self {
        let px = |v: f32| v.round().clamp(0.0, f32::from(u16::MAX)) as u16;
        PtySize {
            cols: metrics.cols(),
            rows: metrics.rows(),
            cell_width_px: px(metrics.cell_width()),
            cell_height_px: px(metrics.cell_height()),
        }
    }
}

/// `TIOCSWINSZ` on `fd`. An `Err`, never an exit -- alacritty's `OnResize` would take the whole
/// process down on a resize that failed.
pub fn set_winsize(fd: BorrowedFd<'_>, size: PtySize) -> io::Result<()> {
    termios::tcsetwinsize(fd, size.to_winsize()).map_err(io::Error::from)
}

/// A child process whose stdin, stdout and stderr are the user side of a fresh PTY, and the
/// controller side, which only this value holds.
///
/// Either this process spawned the child ([`PtyChild::spawn`]) and reaps it, or another process
/// did and handed the controller side over ([`PtyChild::adopt`]): then the child is reached only
/// through a pidfd, which names that one process even after its pid is reused, and its exit
/// status is whatever the spawner reports ([`PtyChild::set_exit_status`]).
pub struct PtyChild {
    master: File,
    child: Held,
}

enum Held {
    Spawned(Child),
    #[cfg(target_os = "linux")]
    Adopted(Adopted),
}

#[cfg(target_os = "linux")]
struct Adopted {
    pid: u32,
    pidfd: OwnedFd,
    /// Only the spawner can wait for the child; this is its report, once it has sent one.
    status: Option<ExitStatus>,
    /// Whether the child is this value's to end. Until a move is committed it is still the
    /// source's, and dropping this value must leave it running for the source to resume.
    committed: bool,
}

impl PtyChild {
    /// Opens a PTY of `size` and runs `spec` on it as a session leader with the PTY as its
    /// controlling terminal. The master is non-blocking: the session thread polls it.
    pub fn spawn(spec: &SpawnSpec, size: PtySize) -> io::Result<Self> {
        let pty = rustix_openpty::openpty(None, Some(&size.to_winsize())).map_err(io::Error::from)?;
        // Before the spawn, so no error after it can drop a child nobody reaps. `O_NONBLOCK` is on
        // the controller's open file description, which the child never has (it is `CLOEXEC`).
        set_nonblocking(pty.controller.as_fd())?;
        // IUTF8, so the line discipline erases a whole multi-byte character on Backspace in cooked
        // mode. Best-effort, as in every terminal: a tty that refuses it still works.
        if let Ok(mut attrs) = termios::tcgetattr(&pty.user) {
            attrs.input_modes.set(InputModes::IUTF8, true);
            let _ = termios::tcsetattr(&pty.user, OptionalActions::Now, &attrs);
        }
        let child = {
            // Scoped: `Command` owns the three `Stdio`s, i.e. the parent's copies of the user side.
            // They must be closed before anything reads the master, or EOF never arrives.
            let mut cmd = Command::new(&spec.program);
            cmd.args(&spec.args)
                .current_dir(&spec.cwd)
                .env_clear()
                .envs(child_environment(std::env::vars_os()))
                .stdin(Stdio::from(pty.user.try_clone()?))
                .stdout(Stdio::from(pty.user.try_clone()?))
                .stderr(Stdio::from(pty.user));
            // SAFETY: runs in the forked child before exec, and calls only async-signal-safe
            // functions. std has already put the PTY on fds 0/1/2 and reset the signal mask.
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    for signal in [
                        libc::SIGCHLD,
                        libc::SIGHUP,
                        libc::SIGINT,
                        libc::SIGQUIT,
                        libc::SIGTERM,
                        libc::SIGALRM,
                    ] {
                        libc::signal(signal, libc::SIG_DFL);
                    }
                    // The shell inherits its terminal and nothing else: every other descriptor is
                    // marked close-on-exec, so one a library in this process opened without
                    // `O_CLOEXEC` does not live on in the shell. CLOEXEC rather than closing them
                    // here: std reports a failed exec through its own pipe among them (already
                    // CLOEXEC), and closing it would turn `NotFound` into a false success.
                    // Best-effort: a kernel before 5.11 refuses the flag, and that is ignored.
                    #[cfg(target_os = "linux")]
                    libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, libc::CLOSE_RANGE_CLOEXEC);
                    Ok(())
                });
            }
            cmd.spawn()?
        };
        Ok(PtyChild {
            master: File::from(pty.controller),
            child: Held::Spawned(child),
        })
    }

    /// The controller side of a PTY whose child another process spawned, and that child, reached
    /// only through `pidfd`. The caller opened the pidfd and checked that it names the process the
    /// sender meant (its start time); `pid` is kept for reports only, and no signal is ever sent by
    /// it. No reaper: the child is not this process's to wait for.
    #[cfg(target_os = "linux")]
    pub fn adopt(master: OwnedFd, pidfd: OwnedFd, pid: u32) -> io::Result<PtyChild> {
        let mut pty = Self::adopt_uncommitted(master, pidfd, pid)?;
        pty.commit_adoption();
        Ok(pty)
    }

    /// As [`PtyChild::adopt`], for a move that is not committed yet: the child is still the
    /// source's, so dropping this value only closes the two descriptors -- no hangup, no kill --
    /// until [`PtyChild::commit_adoption`]. A destination that fails before its commit then
    /// leaves the source a child it can resume.
    #[cfg(target_os = "linux")]
    pub fn adopt_uncommitted(master: OwnedFd, pidfd: OwnedFd, pid: u32) -> io::Result<PtyChild> {
        set_nonblocking(master.as_fd())?;
        Ok(PtyChild {
            master: File::from(master),
            child: Held::Adopted(Adopted {
                pid,
                pidfd,
                status: None,
                committed: false,
            }),
        })
    }

    /// The move is committed: from now on the child is this value's, and dropping it hangs the
    /// child up. Nothing for a child this process spawned.
    pub fn commit_adoption(&mut self) {
        #[cfg(target_os = "linux")]
        if let Held::Adopted(adopted) = &mut self.child {
            adopted.committed = true;
        }
    }

    /// Gives up the child without touching it: no hangup, no reaper. The controller side is
    /// returned for the caller to close (another process holds its own copy, so closing this one
    /// hangs nothing up), with the pid, which the spawner still has to reap.
    pub fn forget(self) -> (OwnedFd, u32) {
        let pid = self.pid();
        let this = ManuallyDrop::new(self);
        // SAFETY: each field is read out exactly once, and `this` is never used or dropped again,
        // so nothing is dropped twice. A `std::process::Child` dropped this way neither kills nor
        // waits for its process.
        let (master, child) = unsafe { (std::ptr::read(&this.master), std::ptr::read(&this.child)) };
        drop(child);
        (OwnedFd::from(master), pid)
    }

    /// The spawner's report of how an adopted child ended. Ignored for a child this process
    /// spawned, which it reaps itself.
    pub fn set_exit_status(&mut self, status: ExitStatus) {
        #[cfg(target_os = "linux")]
        if let Held::Adopted(adopted) = &mut self.child {
            adopted.status = Some(status);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = status;
    }

    /// Whether the child was handed over by another process.
    pub fn is_adopted(&self) -> bool {
        match &self.child {
            Held::Spawned(_) => false,
            #[cfg(target_os = "linux")]
            Held::Adopted(_) => true,
        }
    }

    pub fn pid(&self) -> u32 {
        match &self.child {
            Held::Spawned(child) => child.id(),
            #[cfg(target_os = "linux")]
            Held::Adopted(adopted) => adopted.pid,
        }
    }

    /// The controller side, for `poll(2)`.
    pub fn master_fd(&self) -> BorrowedFd<'_> {
        self.master.as_fd()
    }

    /// One non-blocking read of the child's output. `Ok(0)` or an `EIO` error means every holder
    /// of the user side has closed it -- NOT that the child has exited: a child can close its
    /// terminal and keep running (`exec nohup cmd`).
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.master.read(buf)
    }

    /// One non-blocking write of input; returns how much the kernel took.
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.master.write(bytes)
    }

    pub fn resize(&self, size: PtySize) -> io::Result<()> {
        set_winsize(self.master.as_fd(), size)
    }

    /// SIGHUP to the child's process group -- it called `setsid`, so its pid is the group id. What a
    /// terminal window closing does to the shell inside it. A job the shell runs in a process group
    /// of its own is not signalled here; the shell passes the hangup on to its jobs, as it would in
    /// any terminal.
    ///
    /// An adopted child is signalled through its pidfd, which reaches that one process: the
    /// session leader, which passes the hangup on to its jobs. The process group is left to the
    /// kernel's own hangup when the last controller side closes; a group signal by pidfd would
    /// reach whatever group the process is in, which is not always its own.
    pub fn hangup(&self) {
        // SAFETY: `kill` is a syscall on a pid captured when this process spawned the child.
        // A stale or reused pid can signal the wrong process, but does not cause undefined behaviour.
        // `try_wait` and `wait` borrow `&mut self` only during their calls, so the type system still
        // allows a later `hangup(&self)` after reaping. Callers must avoid signalling after the child
        // has been reaped, since its pid may have been reused.
        match &self.child {
            Held::Spawned(child) => unsafe {
                libc::kill(-(child.id() as libc::pid_t), libc::SIGHUP);
            },
            #[cfg(target_os = "linux")]
            Held::Adopted(adopted) => {
                let _ = pidfd::signal(adopted.pidfd.as_fd(), libc::SIGHUP);
            }
        }
    }

    /// SIGKILL to the child itself, by the pid this value captured at spawn (or its pidfd).
    pub fn kill(&mut self) {
        match &mut self.child {
            Held::Spawned(child) => {
                let _ = child.kill();
            }
            #[cfg(target_os = "linux")]
            Held::Adopted(adopted) => {
                let _ = pidfd::signal(adopted.pidfd.as_fd(), libc::SIGKILL);
            }
        }
    }

    /// `Ok(None)` while the child runs. An adopted child that has exited before the spawner
    /// reported its status is an error: it is gone, and how it ended is not known here.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match &mut self.child {
            Held::Spawned(child) => child.try_wait(),
            #[cfg(target_os = "linux")]
            Held::Adopted(adopted) => adopted.try_wait(0),
        }
    }

    /// Reaps the child, blocking until it exits. Only for a child known to be exiting -- after
    /// `kill`, say. A master at EOF does not mean that (see [`PtyChild::read`]).
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        match &mut self.child {
            Held::Spawned(child) => child.wait(),
            #[cfg(target_os = "linux")]
            Held::Adopted(adopted) => loop {
                if let Some(status) = adopted.try_wait(-1)? {
                    return Ok(status);
                }
            },
        }
    }
}

#[cfg(target_os = "linux")]
impl Adopted {
    /// Waits up to `timeout_ms` (`-1`: without end) for the pidfd to report the exit.
    fn try_wait(&mut self, timeout_ms: libc::c_int) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        if !pidfd::exited(self.pidfd.as_fd(), timeout_ms)? {
            return Ok(None);
        }
        match self.status {
            Some(status) => Ok(Some(status)),
            None => Err(io::Error::other(format!(
                "pid {} exited; its status is its spawner's to report",
                self.pid
            ))),
        }
    }
}

impl Drop for PtyChild {
    /// A child nobody reaped: hang it up, and reap it off this thread -- killing it by its pid if it
    /// outlives [`HANGUP_GRACE`]. Already reaped (std keeps the status), or adopted for a move not
    /// committed yet, nothing happens.
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if matches!(&self.child, Held::Adopted(adopted) if !adopted.committed) {
            return;
        }
        if !matches!(self.try_wait(), Ok(None)) {
            return;
        }
        self.hangup();
        #[cfg(target_os = "linux")]
        if let Held::Adopted(adopted) = &self.child {
            // Not this process's child: no wait, only the kill after the grace, by the pidfd.
            let pid = adopted.pid;
            match adopted.pidfd.try_clone() {
                Ok(pidfd) => {
                    let killer = std::thread::Builder::new()
                        .name(format!("terminal-hangup-{pid}"))
                        .spawn(move || kill_after_grace(pidfd));
                    if let Err(err) = killer {
                        eprintln!("[terminal] could not watch pid {pid} after its hangup: {err}");
                    }
                }
                Err(err) => eprintln!("[terminal] could not watch pid {pid} after its hangup: {err}"),
            }
            return;
        }
        let pid = self.pid() as libc::pid_t;
        let reaper = std::thread::Builder::new()
            .name(format!("terminal-reap-{pid}"))
            .spawn(move || reap_after_grace(pid));
        if let Err(err) = reaper {
            eprintln!("[terminal] could not start a reaper for pid {pid}: {err}; it stays a zombie");
        }
    }
}

/// Reaps `pid`, which has been hung up, killing it once [`HANGUP_GRACE`] has passed. By this pid
/// only: nothing else in the process may be reaped here.
fn reap_after_grace(pid: libc::pid_t) {
    let deadline = Instant::now() + HANGUP_GRACE;
    let mut status = 0;
    loop {
        // SAFETY: plain syscalls on a pid this process spawned and nobody else reaps.
        match unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } {
            0 if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            0 => {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    // Retry the blocking wait on EINTR. A signal handler installed without
                    // SA_RESTART could otherwise interrupt it and leave this pid as a zombie.
                    while libc::waitpid(pid, &mut status, 0) == -1
                        && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
                    {}
                }
                return;
            }
            // Reaped, or no longer ours to reap.
            _ => return,
        }
    }
}

/// An adopted child that outlives [`HANGUP_GRACE`] after its hangup is killed through its pidfd.
#[cfg(target_os = "linux")]
fn kill_after_grace(pidfd: OwnedFd) {
    if let Ok(false) = pidfd::exited(pidfd.as_fd(), HANGUP_GRACE.as_millis() as libc::c_int) {
        let _ = pidfd::signal(pidfd.as_fd(), libc::SIGKILL);
    }
}

/// The two calls this crate makes on a pidfd its caller opened: a signal, and a poll for the exit.
#[cfg(target_os = "linux")]
mod pidfd {
    use std::io;
    use std::os::fd::{AsRawFd, BorrowedFd};

    pub fn signal(pidfd: BorrowedFd<'_>, signal: libc::c_int) -> io::Result<()> {
        // SAFETY: a plain syscall on a descriptor borrowed for the call; no info struct is passed.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                pidfd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0 as libc::c_uint,
            )
        };
        if rc == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Whether the process has exited, waiting up to `timeout_ms` (`-1`: without end). A pidfd is
    /// readable from the moment its process exits, reaped or not.
    pub fn exited(pidfd: BorrowedFd<'_>, timeout_ms: libc::c_int) -> io::Result<bool> {
        let mut pfd = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            // SAFETY: one live pollfd for the duration of the call.
            match unsafe { libc::poll(&mut pfd, 1, timeout_ms) } {
                -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => continue,
                -1 => return Err(io::Error::last_os_error()),
                0 => return Ok(false),
                _ => return Ok(true),
            }
        }
    }
}

/// When the process `pid` started, in clock ticks after boot (`/proc/<pid>/stat`, field 22): with
/// the pid, it names one process even after the pid is reused.
#[cfg(target_os = "linux")]
pub fn start_time(pid: u32) -> io::Result<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    // The command name (field 2) may hold spaces and parentheses; every field after it is plain.
    let rest = stat
        .rfind(')')
        .map(|at| &stat[at + 1..])
        .ok_or_else(|| io::Error::other("a stat line with no command name"))?;
    rest.split_whitespace()
        .nth(19)
        .and_then(|field| field.parse().ok())
        .ok_or_else(|| io::Error::other("a stat line with no start time"))
}

pub(crate) fn set_nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
    let fd = fd.as_raw_fd();
    // SAFETY: fcntl on an fd borrowed for the whole call.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    #[test]
    fn the_child_environment_is_the_hosts_minus_tmux_and_neovim_bin_plus_three() {
        let host = os(&[
            ("PATH", "/usr/bin"),
            ("TMUX", "/tmp/tmux-1000/default,1,0"),
            ("TMUX_PANE", "%3"),
            ("TERM", "tmux-256color"),
            ("WAYLAND_DISPLAY", "wayland-0"),
            // The editor pane's `NEOVIM_BIN` choice must not reach a `neovide` or script
            // run inside this terminal.
            ("NEOVIM_BIN", "/home/x/.local/share/eitri/nvim/0.11.4/bin/nvim"),
        ]);
        let mut env = child_environment(host);
        env.sort();
        assert_eq!(
            env,
            os(&[
                ("COLORTERM", "truecolor"),
                ("PATH", "/usr/bin"),
                ("TERM", "xterm-256color"),
                ("TERM_PROGRAM", "eitri"),
                ("WAYLAND_DISPLAY", "wayland-0"),
            ])
        );
    }

    #[test]
    fn shell_choice_prefers_shell_then_passwd_then_bin_sh() {
        let passwd = || Some(PathBuf::from("/usr/bin/zsh"));
        let never = || -> Option<PathBuf> { panic!("$SHELL is set: the passwd database is not consulted") };
        assert_eq!(
            choose_shell(Some("/bin/fish".into()), never),
            PathBuf::from("/bin/fish")
        );
        assert_eq!(choose_shell(Some("".into()), passwd), PathBuf::from("/usr/bin/zsh"));
        assert_eq!(choose_shell(None, passwd), PathBuf::from("/usr/bin/zsh"));
        assert_eq!(choose_shell(None, || None), PathBuf::from("/bin/sh"));
    }

    /// A stale `$SHELL` (an uninstalled fish) falls back to the passwd shell, then to `/bin/sh`,
    /// each once, and then gives up -- never a loop between the two.
    #[test]
    fn a_shell_that_fails_to_start_falls_back_to_passwd_then_bin_sh_once_each() {
        let fish = PathBuf::from("/usr/bin/fish");
        let zsh = PathBuf::from("/usr/bin/zsh");
        let sh = PathBuf::from("/bin/sh");
        assert_eq!(
            fallback_shell(std::slice::from_ref(&fish), Some(zsh.clone())),
            Some(zsh.clone())
        );
        assert_eq!(
            fallback_shell(&[fish.clone(), zsh.clone()], Some(zsh.clone())),
            Some(sh.clone())
        );
        assert_eq!(
            fallback_shell(&[fish.clone(), zsh.clone(), sh.clone()], Some(zsh.clone())),
            None
        );
        assert_eq!(
            fallback_shell(std::slice::from_ref(&zsh), Some(zsh.clone())),
            Some(sh.clone()),
            "$SHELL was the passwd shell"
        );
        assert_eq!(fallback_shell(&[fish], None), Some(sh.clone()));
        assert_eq!(fallback_shell(&[sh], None), None);
    }

    #[test]
    fn pixel_size_saturates_and_the_grid_is_at_least_one_cell() {
        let ws = PtySize {
            cols: 500,
            rows: 300,
            cell_width_px: 200,
            cell_height_px: 300,
        }
        .to_winsize();
        assert_eq!(
            (ws.ws_col, ws.ws_row, ws.ws_xpixel, ws.ws_ypixel),
            (500, 300, u16::MAX, u16::MAX)
        );
        let zero = PtySize {
            cols: 0,
            rows: 0,
            cell_width_px: 9,
            cell_height_px: 18,
        };
        let ws = zero.to_winsize();
        assert_eq!((ws.ws_col, ws.ws_row, ws.ws_xpixel, ws.ws_ypixel), (1, 1, 9, 18));
        assert_eq!((zero.clamped().cols, zero.clamped().rows), (1, 1));
    }

    #[test]
    fn a_failed_winsize_is_an_error_not_an_exit() {
        let (reader, _writer) = std::io::pipe().unwrap();
        let size = PtySize {
            cols: 80,
            rows: 24,
            cell_width_px: 9,
            cell_height_px: 18,
        };
        let err = set_winsize(reader.as_fd(), size).expect_err("a pipe is not a tty");
        assert_eq!(err.raw_os_error(), Some(libc::ENOTTY));
    }

    /// `from_metrics` rounds cell dimensions to whole device pixels and copies the grid
    /// dimensions unchanged.
    #[test]
    fn from_metrics_rounds_the_cell_size_and_copies_the_grid() {
        let metrics = TerminalMetrics::new(1000.0, 500.0, 1.0);
        let size = PtySize::from_metrics(&metrics);
        assert_eq!((size.cols, size.rows), (metrics.cols(), metrics.rows()));
        assert_eq!(size.cell_width_px, metrics.cell_width().round() as u16);
        assert_eq!(size.cell_height_px, metrics.cell_height().round() as u16);
    }
}
