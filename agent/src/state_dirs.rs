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
    let home = std::env::var("HOME").map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "neither XDG_STATE_HOME nor HOME is set")
    })?;
    Ok(PathBuf::from(home).join(".local/state/neovibe/conversations"))
}

/// `$XDG_RUNTIME_DIR/neovibe/session-leases/`, matching this project's own established pattern for
/// ephemeral, per-session state (`supervisor::socket_path`, `agent::process`'s hook sockets). A
/// lease has no meaning across a reboot, so tmpfs is right here and wrong for the records above.
pub(crate) fn leases_dir() -> std::io::Result<PathBuf> {
    if let Some(root) = test_root() {
        return Ok(root.join("session-leases"));
    }
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(runtime_dir).join("neovibe/session-leases"))
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
/// Linux-only by construction (`/proc/<pid>`), which this whole workspace already is -- GTK4 on
/// Wayland, `flock`, `/proc`-based process checks in `agent`'s own handoff tests. A name that is
/// not a pid, or a pid that is still alive, is left alone; so is anything that fails to delete,
/// since a test root that cannot be pruned is a cosmetic problem and a panic here would fail an
/// unrelated test.
fn prune_dead_roots(parent: &Path) {
    let Ok(entries) = std::fs::read_dir(parent) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<u32>() else { continue };
        if !Path::new(&format!("/proc/{pid}")).exists() {
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
        assert!(root.starts_with(std::env::temp_dir()), "the test root must live under TMPDIR: {root:?}");
    }

    /// The redirect is one-way and single-valued: a second call cannot move an already-redirected
    /// process somewhere else, which is what lets any helper call it without coordinating.
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
    /// exists on Linux, and `u32::MAX` is above `/proc/sys/kernel/pid_max` on every real system, so
    /// it can never be a live process.
    #[test]
    fn pruning_removes_dead_roots_and_leaves_live_ones() {
        let parent = std::env::temp_dir().join(format!("neovibe-prune-test-{}", uuid::Uuid::new_v4()));
        let dead = parent.join(u32::MAX.to_string());
        let live = parent.join("1");
        let not_a_pid = parent.join("notapid");
        for dir in [&dead, &live, &not_a_pid] {
            std::fs::create_dir_all(dir).unwrap();
        }

        prune_dead_roots(&parent);

        assert!(!dead.exists(), "a root whose process is gone must be pruned");
        assert!(live.exists(), "a root whose process is alive must be left alone");
        assert!(not_a_pid.exists(), "a directory that is not a pid must be left alone");
        let _ = std::fs::remove_dir_all(&parent);
    }
}
