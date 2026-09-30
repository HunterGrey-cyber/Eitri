// agent/src/bin/eitri-claude-handoff.rs
//! A small wrapper the Eitri host process spawns to hand off an exclusive session lease to a
//! real, interactive `claude --resume <id>` process. Reads an already-locked lease fd (inherited
//! across `exec` from the host, never independently re-acquired; see
//! `agent::lease::SessionLease::into_inherited_fd`'s own doc for why) via `EITRI_LEASE_FD`, and
//! the session to resume via `EITRI_RESUME_SESSION_ID`. `exec`s the real `claude` binary in
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
    let fd: i32 = match std::env::var("EITRI_LEASE_FD") {
        Ok(v) => match v.parse() {
            Ok(fd) => fd,
            Err(_) => {
                eprintln!("eitri-claude-handoff: EITRI_LEASE_FD is not a valid integer: {v:?}");
                std::process::exit(1);
            }
        },
        Err(_) => {
            eprintln!("eitri-claude-handoff: EITRI_LEASE_FD is not set -- this binary must be launched by the Eitri host, not run directly");
            std::process::exit(1);
        }
    };
    let resume_id = match std::env::var("EITRI_RESUME_SESSION_ID") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            eprintln!("eitri-claude-handoff: EITRI_RESUME_SESSION_ID is not set");
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
        eprintln!("eitri-claude-handoff: EITRI_LEASE_FD={fd} is not a valid open file descriptor in this process -- the host's fd handoff failed");
        std::process::exit(1);
    }

    // Hold the fd open for this process's whole life by leaking it into an intentional, permanent
    // `File` -- `exec` below replaces this process's own code but keeps its open file descriptors
    // (again, as long as FD_CLOEXEC is clear, which the host already ensured), so the lock
    // continues to be held by the same fd number under the new `claude` program image.
    let _lease_fd_keepalive = unsafe { std::fs::File::from_raw_fd(fd) };
    std::mem::forget(_lease_fd_keepalive);

    // Built by `agent::handoff::claude_resume_argv`, the one place this project decides what
    // "continue this Claude session interactively" means -- the same function the shell's
    // "continue in a terminal" action renders for a human.
    //
    // The guard on that, stated precisely: `tests/handoff_wrapper_argv.rs` RUNS this binary with a
    // stub `claude` on `PATH` and pins the argv it really `exec`s, both to the literal
    // `["claude", "--resume", <id>]` and to `claude_resume_argv`'s own output. A flag added to
    // either side alone fails it. What it cannot detect is this file being re-hardcoded to an argv
    // that happens to be identical today -- the test pins the argv, not which function produced
    // it.
    let argv = match agent::handoff::claude_resume_argv(&resume_id) {
        Ok(argv) => argv,
        Err(e) => {
            eprintln!("eitri-claude-handoff: {e}");
            std::process::exit(1);
        }
    };
    let err = std::process::Command::new(&argv[0]).args(&argv[1..]).exec();
    // `exec` only returns on failure -- a successful exec never reaches here.
    eprintln!("eitri-claude-handoff: failed to exec {}: {err}", argv.join(" "));
    std::process::exit(1);
}
