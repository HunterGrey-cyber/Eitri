//! `eitri run-guard`: one small process per running plugin task, which ties the task's life to whichever window
//! holds the task rather than to the window that started it.
//!
//! The window's task runner starts the guard in a process group of its own with four descriptors: 0 is the read
//! end of the **lifetime pipe** (the runner keeps the write end), 3 the write end of the **status pipe** (the runner
//! reads it), 4 and 5 the write ends of the task's stdout and stderr (the runner reads those too; the output never
//! passes through the guard). The task's command comes first on the lifetime pipe, length-prefixed, so it needs
//! neither an argument nor an environment variable the task would inherit.
//!
//! The guard starts the task as its child, leading a process group of its own, with `PR_SET_PDEATHSIG(SIGTERM)`
//! towards the guard: the guard is single-threaded, so that signal follows the guard's life and not a thread of a
//! window. It writes `pid <n> <start>` on the status pipe (the task's start time, read while the task is its unreaped
//! child, so a holder can tell the task from a later process that got the same number), closes its own copies of the
//! output pipes (so the runner's readers see end-of-file when the task ends), and waits for two things:
//! - the task's exit, written as `exit <code>` or `signal <n>`; whatever the task left running in its group is still
//!   held: the guard stays until that group is empty, or until the lifetime pipe ends, when it ends the rest of the
//!   group as below;
//! - end-of-file on the lifetime pipe (the holder closed it, or the holder died and the kernel closed it): SIGTERM
//!   to the task's group, SIGKILL two seconds later, the exit written, and the guard exits.
//!
//! A task that cannot start, or a command that does not arrive whole, is reported as `spawn-failed <why>`. Moving a task to another window moves the lifetime
//! write end and the status read end with its output pipes, so the task lives exactly as long as its holder and its
//! status is never lost.

use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::plugin::tasks::{decode_spec, GuardCommand, GRACE};

/// The hidden subcommand of the `eitri` executable that runs [`serve_inherited`].
pub const SUBCOMMAND: &str = "run-guard";

/// Set in a test binary's guard so its entry test serves instead of returning; removed from the task's environment.
pub const HELPER_VAR: &str = "EITRI_RUN_GUARD_HELPER";

pub const LIFETIME_FD: RawFd = 0;
pub const STATUS_FD: RawFd = 3;
pub const STDOUT_FD: RawFd = 4;
pub const STDERR_FD: RawFd = 5;

pub use crate::plugin::tasks::MAX_SPEC_BYTES;

/// The argument that makes `eitri-run-guard` print [`fingerprint`] and exit, so a window can tell whether the helper
/// beside it is the same build of the guard as the one inside it.
pub const FINGERPRINT_FLAG: &str = "--fingerprint";

const fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
        i += 1;
    }
    hash
}

/// A hash of the guard's source and of the runner that speaks to it: two builds that differ in either differ here.
const SOURCE_HASH: u64 = fnv1a(
    fnv1a(0xcbf2_9ce4_8422_2325, include_bytes!("run_guard.rs")),
    include_bytes!("plugin/tasks.rs"),
);

/// This build's guard, as `eitri-run-guard --fingerprint` prints it.
pub fn fingerprint() -> String {
    format!("{}-{SOURCE_HASH:016x}", env!("CARGO_PKG_VERSION"))
}

/// The task a guard starts, with the write ends of the output pipes its runner reads.
#[derive(Debug)]
pub struct TaskSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub stdout: OwnedFd,
    pub stderr: OwnedFd,
}

/// A guard for a test binary: the binary itself, running only the test named `entry`, which calls
/// [`serve_inherited`] when [`HELPER_VAR`] is set. A test binary has no `run-guard` subcommand.
pub fn test_binary_guard(entry: &str) -> GuardCommand {
    GuardCommand {
        program: std::env::current_exe().expect("the test binary's path"),
        args: ["--exact", entry, "--nocapture"].iter().map(OsString::from).collect(),
        env: vec![(HELPER_VAR.into(), "1".into())],
    }
}

/// Takes descriptor `fd` as this process's own, close-on-exec, if it is open.
fn take_inherited(fd: RawFd) -> Option<OwnedFd> {
    // SAFETY: `fcntl` on a number reads no memory of ours; a closed descriptor is EBADF.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return None;
    }
    // SAFETY: as above; marking it close-on-exec keeps it from the task.
    unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
    // SAFETY: the descriptor is open, and nothing else in this process owns it: the guard takes it before it does
    // anything else.
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The `eitri run-guard` entry: the descriptors at their fixed numbers, the task's command from the lifetime pipe.
/// Exits 2 when any of them is missing or the command does not parse, before anything is started; the status pipe,
/// when it is there, says why, so the runner reports a task that failed to start rather than a guard that died.
pub fn serve_inherited() -> ! {
    let [lifetime, status, stdout, stderr] = [LIFETIME_FD, STATUS_FD, STDOUT_FD, STDERR_FD].map(take_inherited);
    let mut status = status.map(File::from);
    let (Some(lifetime), Some(stdout), Some(stderr)) = (lifetime, stdout, stderr) else {
        refuse(status.as_mut(), "the guard started without its descriptors");
    };
    let Some(mut status) = status else {
        refuse(None, "the guard started without its status pipe");
    };
    let mut lifetime = File::from(lifetime);
    let Some((program, cwd, args)) = read_spec(&mut lifetime) else {
        refuse(
            Some(&mut status),
            "the task's command did not reach its guard whole (cut short, malformed or longer than 1 MiB)",
        );
    };
    run(
        lifetime.into(),
        status.into(),
        TaskSpec {
            program,
            args,
            cwd,
            stdout,
            stderr,
        },
    )
}

fn refuse(status: Option<&mut File>, why: &str) -> ! {
    eprintln!("[run-guard] {why}");
    if let Some(status) = status {
        let _ = writeln!(status, "spawn-failed {why}");
    }
    std::process::exit(2);
}

fn read_spec(from: &mut File) -> Option<(PathBuf, PathBuf, Vec<OsString>)> {
    let mut len = [0u8; 4];
    from.read_exact(&mut len).ok()?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_SPEC_BYTES {
        return None;
    }
    let mut body = vec![0u8; len];
    from.read_exact(&mut body).ok()?;
    decode_spec(&body)
}

/// Starts `task` and holds its life (see the module text). Never returns.
pub fn run(lifetime: OwnedFd, status: OwnedFd, task: TaskSpec) -> ! {
    // A holder that has gone must not take the guard with it through a write on the status pipe; the task gets
    // the default back (`Command` resets it in the child).
    // SAFETY: changing one signal's disposition reads no memory of ours.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let mut status = File::from(status);
    let TaskSpec {
        program,
        args,
        cwd,
        stdout,
        stderr,
    } = task;
    // SAFETY: `getpid` has no preconditions.
    let guard = unsafe { libc::getpid() };
    let mut command = Command::new(&program);
    command
        .args(&args)
        .current_dir(&cwd)
        .env_remove(HELPER_VAR)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    // SAFETY: only async-signal-safe calls run between fork and exec.
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Tied to the guard, whose only thread lives as long as the guard does.
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM as libc::c_ulong) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // The guard may have died before the line above took effect, and then nothing would signal this.
            if libc::getppid() != guard {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
    let spawned = command.spawn();
    // The command holds the guard's copies of the output pipes: dropping it closes them, so the runner's readers
    // see end-of-file once the task and its own children have closed theirs.
    drop(command);
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            let _ = writeln!(status, "spawn-failed {}: {e}", program.display());
            std::process::exit(0);
        }
    };
    let pid = child.id();
    // The child is ours and not yet reaped, so its pid names it here.
    let start = agent::child::start_time(pid).ok();
    let _ = match start {
        Some(start) => writeln!(status, "pid {pid} {start}"),
        None => writeln!(status, "pid {pid}"),
    };
    match watch(&lifetime, &mut child, start) {
        Watched::Exited(exit) => {
            report(&mut status, exit);
            hold_leftovers(&lifetime, pid);
        }
        Watched::HolderGone => {
            let exit = end_group(pid, &mut child);
            report(&mut status, exit);
        }
    }
    std::process::exit(0);
}

enum Watched {
    Exited(std::io::Result<ExitStatus>),
    HolderGone,
}

/// Waits for the task's exit or the lifetime pipe's end, whichever comes first.
fn watch(lifetime: &OwnedFd, child: &mut Child, start: Option<u64>) -> Watched {
    let pid = child.id();
    // The child is ours and not yet reaped, so its pid cannot name another process while this is opened.
    let pidfd = match start {
        Some(start) => agent::child::PidFd::open(pid, start),
        None => Err(std::io::Error::from_raw_os_error(libc::ESRCH)),
    };
    let mut scratch = [0u8; 512];
    loop {
        let mut fds = [
            libc::pollfd {
                fd: lifetime.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: pidfd.as_ref().map_or(-1, |p| std::os::fd::AsFd::as_fd(p).as_raw_fd()),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // Without a pidfd the exit is looked for every 50 ms instead.
        let timeout = if pidfd.is_ok() { -1 } else { 50 };
        // SAFETY: `fds` is a valid array of two `pollfd` (a negative descriptor is skipped).
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
        if r < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            // Nothing left to wait with: treat the holder as gone rather than leave a task nobody can end.
            return Watched::HolderGone;
        }
        if fds[1].revents != 0 || pidfd.is_err() {
            match child.try_wait() {
                Ok(Some(status)) => return Watched::Exited(Ok(status)),
                Ok(None) => {}
                Err(e) => return Watched::Exited(Err(e)),
            }
        }
        if fds[0].revents != 0 {
            // SAFETY: `scratch` is writable for its length.
            let n = unsafe { libc::read(lifetime.as_raw_fd(), scratch.as_mut_ptr().cast(), scratch.len()) };
            if n == 0 || (n < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted) {
                return Watched::HolderGone;
            }
        }
    }
}

fn group_alive(pgid: u32) -> bool {
    // SAFETY: signal 0 only asks whether the group exists.
    unsafe { libc::kill(-(pgid as libc::pid_t), 0) == 0 }
}

/// How often a guard whose task has ended looks whether the task's group has emptied.
const LEFTOVER_POLL: Duration = Duration::from_millis(100);

/// After the task's leader has ended, what it left running in its group is still the holder's: a holder may be
/// between two windows, with nobody reading the status pipe to end the rest. The guard stays until the group is
/// empty or the lifetime pipe ends; then it ends what is left, as it would have ended the task. The number cannot
/// name another group while this one has a member, so signalling it while it is alive reaches only the task's own.
fn hold_leftovers(lifetime: &OwnedFd, pgid: u32) {
    let mut scratch = [0u8; 512];
    while group_alive(pgid) {
        let mut fd = libc::pollfd {
            fd: lifetime.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one `pollfd` that lives for the call.
        let r = unsafe { libc::poll(&mut fd, 1, LEFTOVER_POLL.as_millis() as libc::c_int) };
        if r < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            // Nothing left to wait with: end the rest now rather than leave it to nobody.
            return end_leftovers(pgid);
        }
        if fd.revents != 0 {
            // SAFETY: `scratch` is writable for its length.
            let n = unsafe { libc::read(lifetime.as_raw_fd(), scratch.as_mut_ptr().cast(), scratch.len()) };
            if n == 0 || (n < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted) {
                return end_leftovers(pgid);
            }
        }
    }
}

/// SIGTERM to what is left of a group whose leader has ended, SIGKILL after the grace.
fn end_leftovers(pgid: u32) {
    // SAFETY: `kill` takes plain integers; a group that is gone is ESRCH.
    unsafe {
        libc::kill(-(pgid as libc::pid_t), libc::SIGTERM);
    }
    let deadline = Instant::now() + GRACE;
    while group_alive(pgid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if group_alive(pgid) {
        // SAFETY: as above.
        unsafe {
            libc::kill(-(pgid as libc::pid_t), libc::SIGKILL);
        }
    }
}

/// SIGTERM to the task's group, SIGKILL after the grace to whatever is left of it; the leader's status.
fn end_group(pgid: u32, child: &mut Child) -> std::io::Result<ExitStatus> {
    // SAFETY: `kill` takes plain integers; a group that is gone is ESRCH.
    unsafe {
        libc::kill(-(pgid as libc::pid_t), libc::SIGTERM);
    }
    let deadline = Instant::now() + GRACE;
    let mut reaped = None;
    loop {
        if reaped.is_none() {
            // Reaped as soon as it ends: an unreaped leader would count as a living member of its group.
            if let Ok(Some(status)) = child.try_wait() {
                reaped = Some(status);
            }
        }
        if (reaped.is_some() && !group_alive(pgid)) || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if group_alive(pgid) || reaped.is_none() {
        // SAFETY: as above.
        unsafe {
            libc::kill(-(pgid as libc::pid_t), libc::SIGKILL);
        }
    }
    match reaped {
        Some(status) => Ok(status),
        None => child.wait(),
    }
}

fn report(status: &mut File, exit: std::io::Result<ExitStatus>) {
    let line = match exit {
        Ok(s) => match (s.code(), s.signal()) {
            (Some(code), _) => format!("exit {code}"),
            (None, Some(signal)) => format!("signal {signal}"),
            (None, None) => "signal 0".to_owned(),
        },
        Err(e) => format!("spawn-failed waiting for the task: {e}"),
    };
    // The holder may be gone; nothing else can be told.
    let _ = writeln!(status, "{line}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::tasks::{spawn_guarded, GuardStart, Launch};
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader};

    const HANG_GUARD: Duration = Duration::from_secs(60);
    const SPAWNER_VAR: &str = "EITRI_RUN_GUARD_TEST_SPAWNER";
    const HOLDER_VAR: &str = "EITRI_RUN_GUARD_TEST_HOLDER";

    /// The guard of this test binary: it serves only when started as one.
    #[test]
    fn guard_entry() {
        if std::env::var_os(HELPER_VAR).is_some() {
            serve_inherited();
        }
    }

    /// A stand-in for the window that starts a guard and then ends: it inherits the guard's four descriptors at
    /// their numbers, starts the guard from a thread that ends, and exits.
    #[test]
    fn spawner_entry() {
        if std::env::var_os(SPAWNER_VAR).is_none() {
            return;
        }
        let guard = test_binary_guard("run_guard::tests::guard_entry");
        let starter = std::thread::spawn(move || {
            let mut command = Command::new(&guard.program);
            command
                .args(&guard.args)
                .envs(guard.env.iter().map(|(k, v)| (k, v)))
                .env_remove(SPAWNER_VAR)
                .stdout(Stdio::null())
                .process_group(0);
            command.spawn().expect("the guard starts").id()
        });
        let guard_pid = starter.join().expect("the starting thread");
        println!("guard {guard_pid}");
        std::process::exit(0);
    }

    /// A stand-in for a window that holds a task: it inherits the lifetime write end at descriptor 3 and waits to be
    /// killed.
    #[test]
    fn holder_entry() {
        if std::env::var_os(HOLDER_VAR).is_none() {
            return;
        }
        let _lifetime = take_inherited(3).expect("the lifetime write end");
        std::thread::sleep(Duration::from_secs(600));
        std::process::exit(0);
    }

    fn sh(script: &str) -> Launch {
        Launch {
            task: "t".into(),
            vars: BTreeMap::new(),
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: std::env::temp_dir(),
            timeout: None,
        }
    }

    /// Reads one status line, waiting at most the hang guard.
    fn status_line(status: &mut BufReader<File>) -> String {
        let fd = status.get_ref().as_raw_fd();
        if status.buffer().is_empty() {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one `pollfd` that lives for the call.
            let n = unsafe { libc::poll(&mut pfd, 1, HANG_GUARD.as_millis() as i32) };
            assert!(n > 0, "the guard said nothing");
        }
        let mut line = String::new();
        status.read_line(&mut line).expect("a status line");
        line.trim_end().to_owned()
    }

    /// The task's pid and start time from a `pid <n> <start>` line.
    fn pid_line(line: &str) -> (u32, u64) {
        let rest = line
            .strip_prefix("pid ")
            .unwrap_or_else(|| panic!("the first status line names the task's pid: {line:?}"));
        let (pid, start) = rest
            .split_once(' ')
            .expect("the pid line carries the task's start time");
        (pid.parse().expect("a pid"), start.parse().expect("a start time"))
    }

    fn pid_of(line: &str) -> u32 {
        pid_line(line).0
    }

    /// Whether `pid` is gone (or only a zombie waiting for its new parent to reap it).
    fn gone(pid: u32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(stat) => stat
                .rsplit(')')
                .next()
                .is_some_and(|rest| rest.trim_start().starts_with('Z')),
        }
    }

    /// Waits for group `pgid` to go; how long it took.
    fn wait_group_gone(pgid: u32) -> Duration {
        let began = Instant::now();
        while group_alive(pgid) {
            assert!(began.elapsed() < HANG_GUARD, "group {pgid} outlived its holder");
            std::thread::sleep(Duration::from_millis(20));
        }
        began.elapsed()
    }

    fn parent_of(pid: u32) -> u32 {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("the task's stat");
        let rest = &stat[stat.rfind(')').expect("a command name") + 1..];
        rest.split_whitespace()
            .nth(1)
            .expect("the parent field")
            .parse()
            .expect("a pid")
    }

    /// The four guard descriptors for a stand-in spawner: the lifetime read end as stdin, the others dup'ed to 3-5.
    fn start_spawner(lifetime_read: OwnedFd, status_w: OwnedFd, out_w: OwnedFd, err_w: OwnedFd) {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "run_guard::tests::spawner_entry", "--nocapture"])
            .env(SPAWNER_VAR, "1")
            .stdin(Stdio::from(lifetime_read))
            .stdout(Stdio::piped());
        let raws = [status_w.as_raw_fd(), out_w.as_raw_fd(), err_w.as_raw_fd()];
        // SAFETY: only async-signal-safe calls run between fork and exec.
        unsafe {
            command.pre_exec(move || place_at_three_to_five(raws));
        }
        let mut spawner = command.spawn().expect("the stand-in spawner starts");
        drop(command);
        drop((status_w, out_w, err_w));
        let status = spawner.wait().expect("the stand-in spawner exits");
        assert!(status.success(), "the stand-in spawner failed: {status:?}");
    }

    fn place_at_three_to_five(raws: [RawFd; 3]) -> std::io::Result<()> {
        let mut tmp = [0; 3];
        for (t, fd) in tmp.iter_mut().zip(raws) {
            // SAFETY: duplicating a descriptor reads no memory of ours.
            *t = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) };
            if *t < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        for (t, target) in tmp.into_iter().zip([STATUS_FD, STDOUT_FD, STDERR_FD]) {
            // SAFETY: as above.
            if unsafe { libc::dup2(t, target) } < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    #[test]
    fn a_task_started_from_a_thread_that_ends_outlives_its_spawner_and_ends_with_its_holder() {
        // Twice: once the holder closes the lifetime pipe, once the holder is SIGKILLed.
        for holder_killed in [false, true] {
            let (life_r, mut life_w) = std::io::pipe().unwrap();
            let (status_r, status_w) = std::io::pipe().unwrap();
            let (out_r, out_w) = std::io::pipe().unwrap();
            let (err_r, err_w) = std::io::pipe().unwrap();
            // The group ignores SIGTERM, so it ends only by the SIGKILL that follows.
            let launch = sh("trap '' TERM; sleep 300 & wait");
            let spec = crate::plugin::tasks::encode_spec(&launch);
            life_w.write_all(&spec).unwrap();
            start_spawner(life_r.into(), status_w.into(), out_w.into(), err_w.into());
            drop((out_r, err_r));
            let mut status = BufReader::new(File::from(OwnedFd::from(status_r)));
            let pid = pid_of(&status_line(&mut status));
            // The spawner and its starting thread are gone; give a parent-death signal time to land.
            std::thread::sleep(Duration::from_millis(300));
            assert!(!gone(pid), "the task died with the process that started its guard");
            assert!(group_alive(pid));
            let holder = if holder_killed {
                let raw = life_w.as_raw_fd();
                let mut command = Command::new(std::env::current_exe().unwrap());
                command
                    .args(["--exact", "run_guard::tests::holder_entry", "--nocapture"])
                    .env(HOLDER_VAR, "1")
                    .stdout(Stdio::null());
                // SAFETY: only async-signal-safe calls run between fork and exec.
                unsafe {
                    command.pre_exec(move || {
                        let tmp = libc::fcntl(raw, libc::F_DUPFD_CLOEXEC, 10);
                        if tmp < 0 || libc::dup2(tmp, 3) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
                let holder = command.spawn().expect("the stand-in holder starts");
                drop(command);
                Some(holder)
            } else {
                None
            };
            // This process lets go of its own copy: from here only the holder (or nobody) keeps the task alive.
            drop(life_w);
            if let Some(mut holder) = holder {
                std::thread::sleep(Duration::from_millis(300));
                assert!(group_alive(pid), "the task ended while its holder still lived");
                holder.kill().unwrap();
                holder.wait().unwrap();
            }
            // The group ignores SIGTERM, so it lasts the whole grace; the SIGKILL after it is what ends it. A guard
            // that waited on past its grace would take several times as long.
            let took = wait_group_gone(pid);
            assert!(
                took < GRACE * 3,
                "the group outlived its holder by {took:?}, past the guard's grace of {GRACE:?}"
            );
            let exit = status_line(&mut status);
            assert!(
                exit.starts_with("signal ") || exit.starts_with("exit "),
                "the status pipe reports how the task ended: {exit:?}"
            );
        }
    }

    #[test]
    fn the_guard_reports_the_tasks_pid_first() {
        let guard = test_binary_guard("run_guard::tests::guard_entry");
        let GuardStart {
            mut child,
            lifetime,
            status,
            stdout,
            stderr,
            ..
        } = spawn_guarded(&guard, &sh("sleep 0.3; exit 3")).expect("the guard starts");
        drop((stdout, stderr));
        let mut status = BufReader::new(File::from(OwnedFd::from(status)));
        let (pid, start) = pid_line(&status_line(&mut status));
        assert_eq!(
            agent::child::start_time(pid).unwrap(),
            start,
            "the pid line carries the task's own start time"
        );
        assert_eq!(
            agent::child::process_group(pid).unwrap(),
            pid,
            "the task leads its own group"
        );
        assert_eq!(
            parent_of(pid),
            child.id(),
            "the guard, not the window, is the task's parent"
        );
        assert_ne!(
            agent::child::process_group(child.id()).unwrap(),
            agent::child::process_group(std::process::id()).unwrap(),
            "the guard is in a group of its own"
        );
        assert_eq!(status_line(&mut status), "exit 3");
        assert!(child.wait().unwrap().success());
        drop(lifetime);
    }

    #[test]
    fn the_guard_closes_its_copies_of_the_output_pipes_so_readers_see_eof() {
        let guard = test_binary_guard("run_guard::tests::guard_entry");
        // The task closes its own stdout and stderr and keeps running: only the guard could still hold them open.
        let GuardStart {
            mut child,
            lifetime,
            status,
            stdout,
            stderr,
            ..
        } = spawn_guarded(&guard, &sh("echo hi; exec >&- 2>&-; sleep 300")).expect("the guard starts");
        let mut status = BufReader::new(File::from(OwnedFd::from(status)));
        let pid = pid_of(&status_line(&mut status));
        for (pipe, want) in [(stdout, "hi\n"), (stderr, "")] {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut all = String::new();
                let mut pipe = pipe;
                let _ = pipe.read_to_string(&mut all);
                let _ = tx.send(all);
            });
            let got = rx.recv_timeout(HANG_GUARD).expect("a reader never saw end-of-file");
            assert_eq!(got, want);
        }
        assert!(!gone(pid), "the task still runs");
        drop(lifetime);
        wait_group_gone(pid);
        child.wait().unwrap();
    }

    #[test]
    fn what_a_task_left_running_is_held_after_it_ends_and_ends_when_the_holder_lets_go() {
        let guard = test_binary_guard("run_guard::tests::guard_entry");
        // The leader ends at once and leaves a child in its group, with the output pipes closed.
        let GuardStart {
            mut child,
            lifetime,
            status,
            stdout,
            stderr,
            ..
        } = spawn_guarded(&guard, &sh("sleep 300 >/dev/null 2>&1 & echo $!; exit 4")).expect("the guard starts");
        drop(stderr);
        let mut left = String::new();
        BufReader::new(stdout).read_line(&mut left).unwrap();
        let left: u32 = left.trim().parse().expect("the child's pid");
        let mut status = BufReader::new(File::from(OwnedFd::from(status)));
        let pid = pid_of(&status_line(&mut status));
        assert_eq!(status_line(&mut status), "exit 4");
        std::thread::sleep(Duration::from_millis(300));
        assert!(!gone(left), "the task's child ended with its leader");
        assert!(
            child.try_wait().unwrap().is_none(),
            "the guard went while the task's group still had a member"
        );
        drop(lifetime);
        wait_group_gone(pid);
        assert!(gone(left));
        assert!(child.wait().unwrap().success());
        // A task that leaves nothing behind lets its guard go as soon as it ends, holder or not.
        let GuardStart {
            mut child,
            lifetime,
            status,
            ..
        } = spawn_guarded(&guard, &sh("exit 0")).expect("the guard starts");
        let mut status = BufReader::new(File::from(OwnedFd::from(status)));
        pid_of(&status_line(&mut status));
        assert_eq!(status_line(&mut status), "exit 0");
        assert!(child.wait().unwrap().success());
        drop(lifetime);
    }

    #[test]
    fn a_command_longer_than_the_guard_takes_is_reported_as_a_failed_start() {
        let guard = test_binary_guard("run_guard::tests::guard_entry");
        let mut launch = sh("true");
        launch.args.push("x".repeat(MAX_SPEC_BYTES + 1));
        let GuardStart {
            mut child,
            lifetime,
            status,
            ..
        } = spawn_guarded(&guard, &launch).expect("the guard starts");
        let mut status = BufReader::new(File::from(OwnedFd::from(status)));
        let line = status_line(&mut status);
        assert!(line.starts_with("spawn-failed "), "{line:?}");
        assert_eq!(child.wait().unwrap().code(), Some(2));
        drop(lifetime);
    }

    fn median(mut values: Vec<f64>) -> f64 {
        values.sort_by(|a, b| a.partial_cmp(b).unwrap());
        values[values.len() / 2]
    }

    fn pss_kib(pid: u32) -> f64 {
        let text = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).expect("smaps_rollup");
        text.lines()
            .find_map(|l| l.strip_prefix("Pss:"))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
            .expect("a Pss line")
    }

    /// The memory and start time of one guard holding `sleep 30`, median of 20. The guard measured is the one in
    /// `EITRI_RUN_GUARD_BIN` (the built `eitri`, started as `eitri run-guard`), so build it in release first.
    #[test]
    #[ignore]
    fn run_guard_cost() {
        let bin = std::env::var_os("EITRI_RUN_GUARD_BIN").expect("EITRI_RUN_GUARD_BIN names the built eitri");
        let guard = GuardCommand {
            program: bin.into(),
            args: vec![SUBCOMMAND.into()],
            env: Vec::new(),
        };
        let mut pss = Vec::new();
        let mut start_ms = Vec::new();
        for _ in 0..20 {
            let started = Instant::now();
            let GuardStart {
                mut child,
                lifetime,
                status,
                stdout,
                stderr,
                ..
            } = spawn_guarded(&guard, &sh("exec sleep 30")).expect("the guard starts");
            let mut status = BufReader::new(File::from(OwnedFd::from(status)));
            let pid = pid_of(&status_line(&mut status));
            start_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            // Settled: the guard is waiting in `poll`.
            std::thread::sleep(Duration::from_millis(200));
            pss.push(pss_kib(child.id()));
            drop(lifetime);
            wait_group_gone(pid);
            child.wait().unwrap();
            drop((stdout, stderr));
        }
        let (pss, start) = (median(pss), median(start_ms));
        println!("run_guard_cost: pss_median_kib={pss:.0} start_median_ms={start:.1} (20 guards)");
    }
}
