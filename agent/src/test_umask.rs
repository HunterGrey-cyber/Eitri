// agent/src/test_umask.rs
//! Test helper: run a check under a chosen umask without touching this process's.
//!
//! `umask` is process-wide, and the lib tests run on many threads that create files and
//! directories: changing it in one of them, even for a moment, changes the modes the others get.
//! So a test that needs a particular umask runs its body in a fresh copy of the test binary, which
//! has no other tests running, and the parent only reads the verdict.

use std::process::Command;

const CHILD_FLAG: &str = "EITRI_TEST_UMASK_CHILD";

/// Runs `body` with the umask set to `umask`, in a child copy of the test binary that runs only the
/// test named `test` (`module_path!()` plus the function name, as `cargo test` prints it, crate name
/// first). In the child it runs `body` directly, and a panic there fails the child and so the test
/// that spawned it.
pub(crate) fn with_umask(test: &str, umask: u32, body: impl FnOnce()) {
    if std::env::var_os(CHILD_FLAG).is_some() {
        // SAFETY: `umask` has no preconditions; this child runs this one test and nothing else.
        unsafe { libc::umask(umask as libc::mode_t) };
        body();
        return;
    }
    let filter = test.split_once("::").map_or(test, |(_, rest)| rest);
    let out = Command::new(std::env::current_exe().unwrap())
        .args([filter, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_FLAG, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "the child run of {filter} under umask {umask:04o} failed or ran nothing:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
