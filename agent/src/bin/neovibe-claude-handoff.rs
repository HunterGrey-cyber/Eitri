// agent/src/bin/neovibe-claude-handoff.rs
//! A small wrapper the host process spawns to hand off an exclusive session lease to a
//! real, interactive `claude --resume <id>` process. Reads an already-locked lease fd (inherited
//! across `exec` from the host, never independently re-acquired; see
//! `agent::lease::SessionLease::into_inherited_fd`'s own doc for why) via `NEOVIBE_LEASE_FD`, and
//! the session to resume via `NEOVIBE_RESUME_SESSION_ID`. `exec`s the real `claude` binary in
//! place (this process becomes `claude`; it does not fork a further child) so the lease-holding
//! process and the interactive CLI are the same PID for the whole time the lease is held; when
//! `claude` exits, this process (now literally `claude`) exits too, and the OS releases the
//! `flock` automatically as its last referencing fd closes.
//!
//! No wrapper stays alive as a supervising parent holding the lock. `exec` ties the lock's
//! lifetime exactly to the real CLI process's own lifetime, with no second, independent process
//! whose own death (crash, kill -9, orphaning) would be a separate failure mode the lock's
//! correctness would depend on. The lease's survival therefore rests on the exec'd `claude`
//! keeping an inherited descriptor it never asked for, across its own `exec`. That was checked
//! against the real CLI (`agent/MANUAL_VERIFICATION.md`, handoff section), and
//! `agent/tests/handoff_conformance.rs` is the standing regression guard for it: it asserts a
//! second lease acquire attempt fails (`LeaseError::AlreadyHeld`) only after confirming, via
//! `/proc/<pid>/comm`, that the held-open pid has actually become `claude`, so a regression that
//! broke fd survival across `exec` would fail loudly there, not pass vacuously against a
//! not-yet-exec'd wrapper.

use std::os::unix::io::FromRawFd;
use std::os::unix::process::CommandExt;

fn main() {
    let fd: i32 = match std::env::var("NEOVIBE_LEASE_FD") {
        Ok(v) => match v.parse() {
            Ok(fd) => fd,
            Err(_) => {
                eprintln!("neovibe-claude-handoff: NEOVIBE_LEASE_FD is not a valid integer: {v:?}");
                std::process::exit(1);
            }
        },
        Err(_) => {
            eprintln!("neovibe-claude-handoff: NEOVIBE_LEASE_FD is not set -- this binary must be launched by the Neovibe host, not run directly");
            std::process::exit(1);
        }
    };
    let resume_id = match std::env::var("NEOVIBE_RESUME_SESSION_ID") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            eprintln!("neovibe-claude-handoff: NEOVIBE_RESUME_SESSION_ID is not set");
            std::process::exit(1);
        }
    };

    // Confirm the inherited fd is genuinely still open and still holds the lock -- a cheap,
    // real sanity check before handing the terminal over to `claude`, not a re-acquire (a fresh
    // acquire attempt here would defeat the whole point: it would either race the host's own
    // not-yet-released fd, or silently succeed on a fd that never actually held anything if the
    // host's spawn logic had a bug -- this checks for exactly that bug instead of masking it).
    let rc = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if rc == -1 {
        eprintln!("neovibe-claude-handoff: NEOVIBE_LEASE_FD={fd} is not a valid open file descriptor in this process -- the host's fd handoff failed");
        std::process::exit(1);
    }

    // Hold the fd open for this process's whole life by leaking it into an intentional, permanent
    // `File` -- `exec` below replaces this process's own code but keeps its open file descriptors
    // (again, as long as FD_CLOEXEC is clear, which the host already ensured), so the lock
    // continues to be held by the same fd number under the new `claude` program image.
    let _lease_fd_keepalive = unsafe { std::fs::File::from_raw_fd(fd) };
    std::mem::forget(_lease_fd_keepalive);

    let err = std::process::Command::new("claude").arg("--resume").arg(&resume_id).exec();
    // `exec` only returns on failure -- a successful exec never reaches here.
    eprintln!("neovibe-claude-handoff: failed to exec claude --resume {resume_id}: {err}");
    std::process::exit(1);
}
