// agent/src/state_dirs.rs
//! The two directories this crate writes into, and the one redirect its tests use.
//!
//! Split out of `persistence.rs` and `lease.rs` for a single reason: a test must be able to move
//! both somewhere disposable. Without that, `cargo test -p agent` writes real conversation records
//! into the developer's own `$XDG_STATE_HOME/neovibe/conversations/` and real lock files into their
//! `$XDG_RUNTIME_DIR/neovibe/session-leases/`, and neither is removed afterwards. That is not
//! avoidable by "just not calling persistence" either: the record write and the session-lease
//! acquire both happen on `AgentConversation`'s own ingestion thread when the provider's first
//! `SessionOpened` is folded, so any test that drives a conversation past that event reaches them.
//!
//! The redirect is a process-global `OnceLock`, and it is compiled into EVERY build rather than
//! hidden behind `#[cfg(test)]`, because a `#[cfg(test)]` hook cannot serve either of the two cases
//! that need it:
//!
//! - the writes happen on a thread the test never spawned, so a thread-local override (the shape
//!   `lease::test_override` uses for `lease`'s own single-threaded tests) cannot reach them;
//! - `agent/tests/*.rs` are separate crates that link this one compiled WITHOUT `cfg(test)`, so a
//!   `#[cfg(test)]` item would be invisible to exactly the integration tests that need it.
//!
//! It is `#[doc(hidden)]`, named so it cannot be mistaken for product API, one-way (there is no
//! un-redirect and no second root), and nothing outside a `#[cfg(test)]` module or an
//! `agent/tests/` file calls it. It is deliberately NOT an environment variable: setting one from
//! a test means `std::env::set_var` in a multi-threaded test binary, which is the exact
//! process-wide hazard this module exists to stop repeating.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static TEST_ROOT: OnceLock<PathBuf> = OnceLock::new();

fn test_root() -> Option<&'static Path> {
    TEST_ROOT.get().map(PathBuf::as_path)
}

/// `$XDG_STATE_HOME/neovibe/conversations/`, falling back to
/// `~/.local/state/neovibe/conversations/` per the XDG Base Directory spec's own stated default for
/// `XDG_STATE_HOME` when unset. Persistent on purpose: a conversation record has to survive a
/// reboot, so `$XDG_RUNTIME_DIR` (tmpfs, cleared on logout) would be the wrong home for it.
pub(crate) fn conversations_dir() -> std::io::Result<PathBuf> {
    if let Some(root) = test_root() {
        return Ok(root.join("conversations"));
    }
    if let Ok(state_home) = std::env::var("XDG_STATE_HOME") {
        return Ok(PathBuf::from(state_home).join("neovibe/conversations"));
    }
    let home = std::env::var("HOME")
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::NotFound, "neither XDG_STATE_HOME nor HOME is set"))?;
    Ok(PathBuf::from(home).join(".local/state/neovibe/conversations"))
}

/// Where session-lease lock files live. Two platforms, two answers, and every process of one user
/// on one machine must reach the SAME directory -- two processes that disagree each take "the"
/// lock in their own directory, both succeed, and nothing reports it.
///
/// - **Linux:** `$XDG_RUNTIME_DIR/neovibe/session-leases/`, matching this project's own established
///   pattern for ephemeral, per-session state (`supervisor::socket_path`, `agent::process`'s hook
///   sockets). A lease has no meaning across a reboot, so tmpfs is right here and wrong for the
///   records above. Unset is still an error, as before M1.
/// - **macOS:** `<home>/Library/Application Support/neovibe/session-leases/`, where `<home>` is the
///   account's home directory from the user database (`getpwuid_r`), not `$HOME`. macOS has no
///   `XDG_RUNTIME_DIR`, and it is deliberately NOT consulted even when someone sets it: a process
///   launched with it and one launched without would split the lock. Two other homes were
///   rejected (macOS track M1, 2026-09-17; design §4 M1 item 1):
///   - `std::env::temp_dir()` reads `$TMPDIR` first, so it has the same split-brain problem, and
///     this project runs tests with `TMPDIR=/nonexistent`;
///   - `/var/folders/…/T/` itself is swept by `dirhelper` of files not accessed for 3 days. `flock`
///     does not touch atime, and the lease file is opened by path with `create(true)`, so a lease
///     held longer than that (a handed-off `claude --resume` left open) could lose its path and a
///     second holder could lock a fresh inode alongside the first.
///
///   Nothing cleans `Application Support`. `flock` state does not survive a reboot, so a stale lock
///   file there is only a file. `SessionLease::try_acquire` creates the directory 0700.
///
/// Before M1 macOS took the Linux branch and failed on the unset variable: a new session started
/// without a lease, resume and handoff failed.
pub(crate) fn leases_dir() -> std::io::Result<PathBuf> {
    if let Some(root) = test_root() {
        return Ok(root.join("session-leases"));
    }
    platform_leases_dir()
}

#[cfg(not(target_os = "macos"))]
fn platform_leases_dir() -> std::io::Result<PathBuf> {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(runtime_dir).join("neovibe/session-leases"))
}

#[cfg(target_os = "macos")]
fn platform_leases_dir() -> std::io::Result<PathBuf> {
    Ok(macos_leases_dir_under(&account_home_dir()?))
}

#[cfg(target_os = "macos")]
fn macos_leases_dir_under(home: &Path) -> PathBuf {
    home.join("Library/Application Support/neovibe/session-leases")
}

/// This process's real user's home directory, from the user database rather than `$HOME`.
#[cfg(target_os = "macos")]
fn account_home_dir() -> std::io::Result<PathBuf> {
    use std::ffi::{CStr, OsStr};
    use std::os::unix::ffi::OsStrExt;

    let mut buf_len = 4096;
    loop {
        let mut buf = vec![0 as libc::c_char; buf_len];
        // SAFETY: an all-zero `passwd` is valid (null pointers and integers); `getpwuid_r` fills it
        // with pointers into `buf`, which outlives every read of them below.
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = unsafe { libc::getpwuid_r(libc::getuid(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
        if rc == libc::ERANGE && buf_len < 1 << 20 {
            buf_len *= 4;
            continue;
        }
        if rc != 0 {
            return Err(std::io::Error::from_raw_os_error(rc));
        }
        if result.is_null() || pwd.pw_dir.is_null() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "this user has no entry in the user database",
            ));
        }
        let dir = unsafe { CStr::from_ptr(pwd.pw_dir) };
        let dir = PathBuf::from(OsStr::from_bytes(dir.to_bytes()));
        if !dir.is_absolute() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("the user database gives a relative home directory {dir:?}"),
            ));
        }
        return Ok(dir);
    }
}

/// Points every directory above at a disposable root for the rest of this PROCESS, and returns it.
///
/// Test-only (see this module's header for why it is not `#[cfg(test)]`). Idempotent: the first
/// call wins and every later one returns the same root, so a test file can call it from each of its
/// own helpers without coordinating.
///
/// The root is `$TMPDIR/neovibe-agent-test-state/<pid>`. Keyed on the pid rather than a fresh uuid
/// deliberately: libtest has no after-all-tests hook, so nothing can delete the root when the run
/// ends, and a uuid would leave one directory behind per `cargo test` invocation forever. A pid can
/// instead be checked for liveness by the NEXT run, which is what `prune_dead_roots` does below --
/// so what accumulates is one directory per test binary currently running, not one per run ever
/// made. It is still true that a root outlives its own run until something prunes it; the claim
/// here is bounded growth, not immediate cleanup.
#[doc(hidden)]
pub fn redirect_state_to_a_test_root() -> PathBuf {
    TEST_ROOT
        .get_or_init(|| {
            let parent = std::env::temp_dir().join("neovibe-agent-test-state");
            prune_dead_roots(&parent);
            let root = parent.join(std::process::id().to_string());
            // A pid can be reused, so a leftover root from a dead process with this same pid may
            // exist. Start it empty rather than inheriting someone else's records.
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("creating the test state root should succeed");
            root
        })
        .clone()
}

/// A workspace directory under the test root, created and returned.
///
/// Test-only. Exists so a test that needs a real, distinct project directory gets one inside the
/// pruned root instead of leaving a fresh `$TMPDIR` entry behind on every run. `label` is only for
/// a human reading `ls`; the uuid is what makes it unique.
#[doc(hidden)]
pub fn test_workspace_dir(label: &str) -> PathBuf {
    let dir = redirect_state_to_a_test_root()
        .join("workspaces")
        .join(format!("{label}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("creating a test workspace directory should succeed");
    dir
}

/// Removes every sibling root whose owning process is gone.
///
/// Liveness is `crate::process_probe::pid_is_alive`, i.e. `kill(pid, 0)`, on Linux and macOS alike.
/// It used to be `/proc/<pid>` existing, which on macOS is never true, so every root -- including
/// a concurrently running test binary's -- was deleted (M1, 2026-09-17). A name that is
/// not a pid, or a pid that is still alive, is left alone; so is anything that fails to delete,
/// since a test root that cannot be pruned is a cosmetic problem and a panic here would fail an
/// unrelated test.
fn prune_dead_roots(parent: &Path) {
    let Ok(entries) = std::fs::read_dir(parent) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<u32>() else { continue };
        if !crate::process_probe::pid_is_alive(pid) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both directories must land under the redirect, or a test run writes into the developer's
    /// real XDG directories -- the defect this module exists to close.
    #[test]
    fn a_redirected_process_writes_neither_records_nor_leases_into_the_real_xdg_dirs() {
        let root = redirect_state_to_a_test_root();
        assert!(conversations_dir().unwrap().starts_with(&root));
        assert!(leases_dir().unwrap().starts_with(&root));
        assert!(
            root.starts_with(std::env::temp_dir()),
            "the test root must live under TMPDIR: {root:?}"
        );
    }

    /// The redirect is one-way and single-valued: a second call cannot move an already-redirected
    /// process somewhere else, which is what lets any helper call it without coordinating.
    /// The unredirected macOS answer, resolved but never created (this must not write into the
    /// developer's home): under the account's home, in `Application Support`, and not under
    /// `temp_dir()` -- whose `$TMPDIR` dependence and 3-day sweep are the two reasons it moved.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_macos_lease_dir_is_under_the_account_home_and_not_the_temp_dir() {
        let home = account_home_dir().expect("this user must have a home directory");
        let dir = platform_leases_dir().unwrap();
        assert_eq!(dir, macos_leases_dir_under(&home));
        assert!(
            dir.ends_with("Library/Application Support/neovibe/session-leases"),
            "{dir:?}"
        );
        assert!(
            !dir.starts_with(std::env::temp_dir()),
            "{dir:?} must not be under TMPDIR"
        );
        assert!(
            !dir.starts_with("/var/folders") && !dir.starts_with("/private/var/folders"),
            "{dir:?}"
        );
    }

    /// The home comes from the user database, so it cannot drift with the environment. Compared
    /// with `dscl`'s answer for the same account, out of process.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_account_home_matches_the_directory_service() {
        let user = std::process::Command::new("id").arg("-un").output().unwrap();
        let user = String::from_utf8(user.stdout).unwrap();
        let out = std::process::Command::new("dscl")
            .args([".", "-read", &format!("/Users/{}", user.trim()), "NFSHomeDirectory"])
            .output()
            .unwrap();
        let out = String::from_utf8(out.stdout).unwrap();
        let expected = out
            .trim()
            .strip_prefix("NFSHomeDirectory:")
            .expect("dscl output")
            .trim();
        assert_eq!(account_home_dir().unwrap(), Path::new(expected));
    }

    #[test]
    fn redirecting_twice_returns_the_same_root() {
        assert_eq!(redirect_state_to_a_test_root(), redirect_state_to_a_test_root());
    }

    #[test]
    fn a_test_workspace_dir_is_created_under_the_root_and_is_unique() {
        let a = test_workspace_dir("unit");
        let b = test_workspace_dir("unit");
        assert_ne!(a, b);
        assert!(a.is_dir() && b.is_dir());
        assert!(a.starts_with(redirect_state_to_a_test_root()));
    }

    /// A root belonging to a pid that no longer exists is removed; a live one is not. Pid 1 always
    /// exists (init on Linux, launchd on macOS), and `u32::MAX` is above any real pid. `u32::MAX`
    /// and `0` are also the two names a bare `kill(pid, 0)` would call alive -- `-1` and "my own
    /// process group" -- so they are kept here as the regression cases for that.
    #[test]
    fn pruning_removes_dead_roots_and_leaves_live_ones() {
        let parent = std::env::temp_dir().join(format!("neovibe-prune-test-{}", uuid::Uuid::new_v4()));
        let dead = parent.join(u32::MAX.to_string());
        let pid_zero = parent.join("0");
        let live = parent.join("1");
        let not_a_pid = parent.join("notapid");
        for dir in [&dead, &pid_zero, &live, &not_a_pid] {
            std::fs::create_dir_all(dir).unwrap();
        }

        prune_dead_roots(&parent);

        assert!(!dead.exists(), "a root whose process is gone must be pruned");
        assert!(
            !pid_zero.exists(),
            "pid 0 is never a live test process and must be pruned"
        );
        assert!(live.exists(), "a root whose process is alive must be left alone");
        assert!(not_a_pid.exists(), "a directory that is not a pid must be left alone");
        let _ = std::fs::remove_dir_all(&parent);
    }
}
