//! `serve` confines itself before it starts a thread: it holds the tokens of the phones connected to it, so a parser bug
//! in it must reach as little as it can.
//!
//! - **Not dumpable** (`PR_SET_DUMPABLE` 0): another process of this user can neither ptrace it nor read its memory or
//!   environment through `/proc`.
//! - **Landlock** (Linux 5.13 and later): files may be read beneath the control directory, the device store's directory
//!   and `/proc`, and read, written and removed beneath the agents directory, where the agents reader opens writers'
//!   locks for writing and removes what a dead writer left there. Nothing is removed beneath the control directory, so a
//!   dead writer's `ag-<instance>.sock` there, and its lock, are left for a window's reader to clean. Pathname Unix
//!   sockets may be connected to beneath the control directory only (where the kernel can say so). Nothing else of the
//!   file system: no write anywhere else, and no socket or file made.
//! - **seccomp**: `execve`, `execveat` and `truncate` fail with `EPERM`, in every thread (`SECCOMP_FILTER_FLAG_TSYNC`):
//!   `truncate` because Landlock before ABI 3 does not govern it. An x32 call fails with `EPERM` too; a system call
//!   from another architecture's table ends the process.
//!
//! Landlock binds the calling thread and the threads it starts afterwards, never one already running, so [`sandbox`]
//! refuses to run while a second thread exists. Without Landlock the other two still apply, and the state says so.

use std::io;
use std::path::Path;

use landlock::{
    path_beneath_rules, Access, AccessFs, BitFlags, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus, ABI,
};

/// What [`sandbox`] could put in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxState {
    /// Landlock, seccomp and not dumpable.
    Full,
    /// The kernel has no Landlock: seccomp and not dumpable only.
    NoLandlock,
}

impl SandboxState {
    /// The word `eitri remote status` shows after `sandbox:`.
    pub fn word(self) -> &'static str {
        match self {
            SandboxState::Full => "full",
            SandboxState::NoLandlock => "partial",
        }
    }
}

/// The newest Landlock ABI this build knows; rights an older kernel lacks are left out by the binding.
const LANDLOCK_ABI: ABI = ABI::V9;

/// Confines this process for good. Call it from the main thread before any other thread starts; `Err` when another
/// thread runs or a step fails, and the process must not go on to serve.
pub fn sandbox(agents_dir: &Path, control_dir: &Path, store_dir: &Path) -> Result<SandboxState, io::Error> {
    let threads = thread_count()?;
    if threads != 1 {
        return Err(io::Error::other(format!(
            "the sandbox must be set up before any thread starts, and {threads} threads run"
        )));
    }
    set_not_dumpable()?;
    // Asked of the kernel here too, so seccomp is allowed where Landlock is missing and set nothing.
    prctl(libc::PR_SET_NO_NEW_PRIVS, 1)?;
    let state = landlock(agents_dir, control_dir, store_dir)?;
    deny_exec()?;
    Ok(state)
}

fn landlock(agents_dir: &Path, control_dir: &Path, store_dir: &Path) -> io::Result<SandboxState> {
    let fail = |e: landlock::RulesetError| io::Error::other(format!("Landlock: {e}"));
    let read: BitFlags<AccessFs> = AccessFs::ReadFile | AccessFs::ReadDir;
    let agents: BitFlags<AccessFs> = read | AccessFs::WriteFile | AccessFs::RemoveFile;
    let control: BitFlags<AccessFs> = AccessFs::ResolveUnix.into();
    let status = Ruleset::default()
        .handle_access(AccessFs::from_all(LANDLOCK_ABI))
        .map_err(fail)?
        .create()
        .map_err(fail)?
        .add_rules(path_beneath_rules([control_dir, store_dir, Path::new("/proc")], read))
        .map_err(fail)?
        .add_rules(path_beneath_rules([control_dir], control))
        .map_err(fail)?
        .add_rules(path_beneath_rules([agents_dir], agents))
        .map_err(fail)?
        .restrict_self()
        .map_err(fail)?;
    Ok(match status.ruleset {
        RulesetStatus::NotEnforced => SandboxState::NoLandlock,
        RulesetStatus::FullyEnforced | RulesetStatus::PartiallyEnforced => SandboxState::Full,
    })
}

fn thread_count() -> io::Result<usize> {
    Ok(std::fs::read_dir("/proc/self/task")?.count())
}

fn prctl(option: libc::c_int, value: libc::c_ulong) -> io::Result<()> {
    // SAFETY: `prctl` with an integer argument and three zero ones; no memory is passed.
    if unsafe {
        libc::prctl(
            option,
            value,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn set_not_dumpable() -> io::Result<()> {
    prctl(libc::PR_SET_DUMPABLE, 0)
}

/// Whether this process is dumpable now.
pub fn is_dumpable() -> io::Result<bool> {
    // SAFETY: `PR_GET_DUMPABLE` reads a flag and passes no memory.
    let rc = unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc != 0)
    }
}

// The classic BPF the kernel runs over `struct seccomp_data` (nr at 0, arch at 4). Each code is its class, size or
// jump and source bits or'd together: `BPF_LD | BPF_W | BPF_ABS`, `BPF_JMP | BPF_JEQ | BPF_K`,
// `BPF_JMP | BPF_JGE | BPF_K`, `BPF_RET | BPF_K`.
const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JMP_JEQ_K: u16 = 0x15;
const BPF_JMP_JGE_K: u16 = 0x35;
const BPF_RET_K: u16 = 0x06;
const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_SET_MODE_FILTER: libc::c_ulong = 1;
const SECCOMP_FILTER_FLAG_TSYNC: libc::c_ulong = 1;

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xc000_003e;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xc000_00b7;
/// x86-64's x32 calls carry this bit in their number; none is wanted.
#[cfg(target_arch = "x86_64")]
const X32_SYSCALL_BIT: u32 = 0x4000_0000;
#[cfg(not(target_arch = "x86_64"))]
const X32_SYSCALL_BIT: u32 = u32::MAX;

fn statement(code: u16, jt: u8, jf: u8, k: u32) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

/// `execve`, `execveat` and `truncate` answer `EPERM` in every thread of the process.
fn deny_exec() -> io::Result<()> {
    let errno = SECCOMP_RET_ERRNO | libc::EPERM as u32;
    let filter = [
        statement(BPF_LD_W_ABS, 0, 0, 4),
        statement(BPF_JMP_JEQ_K, 1, 0, AUDIT_ARCH),
        statement(BPF_RET_K, 0, 0, SECCOMP_RET_KILL_PROCESS),
        statement(BPF_LD_W_ABS, 0, 0, 0),
        statement(BPF_JMP_JGE_K, 3, 0, X32_SYSCALL_BIT),
        statement(BPF_JMP_JEQ_K, 2, 0, libc::SYS_execve as u32),
        statement(BPF_JMP_JEQ_K, 1, 0, libc::SYS_execveat as u32),
        statement(BPF_JMP_JEQ_K, 0, 1, libc::SYS_truncate as u32),
        statement(BPF_RET_K, 0, 0, errno),
        statement(BPF_RET_K, 0, 0, SECCOMP_RET_ALLOW),
    ];
    let program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_ptr() as *mut libc::sock_filter,
    };
    // SAFETY: `program` points at `filter`, both alive for the call; the kernel copies the filter.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_SET_MODE_FILTER,
            SECCOMP_FILTER_FLAG_TSYNC,
            &program as *const libc::sock_fprog,
        )
    };
    match rc {
        0 => Ok(()),
        // With TSYNC, a positive answer is a thread that could not take the filter.
        tid if tid > 0 => Err(io::Error::other(format!(
            "seccomp: thread {tid} could not take the filter"
        ))),
        _ => Err(io::Error::last_os_error()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_words() {
        assert_eq!(SandboxState::Full.word(), "full");
        assert_eq!(SandboxState::NoLandlock.word(), "partial");
    }

    #[test]
    fn sandbox_refuses_to_run_beside_another_thread() {
        // libtest runs this beside its own threads, so a second thread exists whatever else happens; and the refusal
        // comes before any step, so this process is left as it was.
        let err = sandbox(
            Path::new("/nonexistent"),
            Path::new("/nonexistent"),
            Path::new("/nonexistent"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("before any thread starts"), "{err}");
        assert!(is_dumpable().unwrap(), "a refused sandbox changed the process");
    }
}
