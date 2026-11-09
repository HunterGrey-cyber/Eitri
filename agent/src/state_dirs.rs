// agent/src/state_dirs.rs
//! The three directories this crate writes into, and the one redirect its tests use.
//!
//! Split out of `persistence.rs` and `lease.rs` for a single reason: a test must be able to move
//! them somewhere disposable. Without that, `cargo test -p agent` writes real conversation records
//! into the developer's own `$XDG_STATE_HOME/eitri/conversations/`, real stored histories into
//! `$XDG_STATE_HOME/eitri/history/`, and real lock files into their
//! `$XDG_RUNTIME_DIR/eitri/session-leases/`, and none of them is removed afterwards. That is not
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

/// `$XDG_STATE_HOME/eitri/conversations/`, falling back to
/// `~/.local/state/eitri/conversations/` per the XDG Base Directory spec's own stated default for
/// `XDG_STATE_HOME` when unset. Persistent on purpose: a conversation record has to survive a
/// reboot, so `$XDG_RUNTIME_DIR` (tmpfs, cleared on logout) would be the wrong home for it.
pub(crate) fn conversations_dir() -> std::io::Result<PathBuf> {
    if let Some(root) = test_root() {
        return Ok(root.join("conversations"));
    }
    state_kind_dir(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
        "conversations",
    )
}

/// `<state home>/eitri/<kind>` from the two variables that decide it, by the rule
/// `eitri_core::state_paths` follows for the rest of Eitri's state: an absolute `XDG_STATE_HOME`,
/// else an absolute `HOME`'s `.local/state`. An empty or relative value is ignored (the XDG spec
/// says a relative one is invalid): taken as given, it resolved against the working directory,
/// which can be the project itself, so conversation records and histories landed in the checkout
/// and were lost to a launch from anywhere else.
fn state_kind_dir(
    xdg_state_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
    kind: &str,
) -> std::io::Result<PathBuf> {
    match (xdg_state_home, home) {
        (Some(state), _) if Path::new(state).is_absolute() => Ok(PathBuf::from(state).join("eitri").join(kind)),
        (_, Some(home)) if Path::new(home).is_absolute() => {
            Ok(PathBuf::from(home).join(".local/state/eitri").join(kind))
        }
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "neither XDG_STATE_HOME nor HOME is an absolute path",
        )),
    }
}

/// `$XDG_STATE_HOME/eitri/history/`, beside `conversations_dir()` and resolved the same way.
///
/// A directory of its own rather than a file inside `conversations/<conversation_id>/`, which is
/// where it would most naturally go. Two functions walk that directory treating every
/// `.json` in it as a `ConversationRecord`: `persistence::read_conversation_records`, which would
/// read each history file and throw it away as unparseable on every start screen, and
/// `persistence::prune`, which deliberately does NOT delete what it cannot parse -- so a history
/// file left there would occupy one of that directory's slots and never be cleaned up. Beside it
/// instead, and neither function changes by a line.
pub(crate) fn history_dir() -> std::io::Result<PathBuf> {
    if let Some(root) = test_root() {
        return Ok(root.join("history"));
    }
    state_kind_dir(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
        "history",
    )
}

/// Where session-lease lock files live. Two platforms, two answers, and every process of one user
/// on one machine must reach the SAME directory -- two processes that disagree each take "the"
/// lock in their own directory, both succeed, and nothing reports it.
///
/// - **Linux:** `$XDG_RUNTIME_DIR/eitri/session-leases/`, matching this project's own established
///   pattern for ephemeral, per-session state (`agent::process`'s hook
///   sockets). A lease has no meaning across a reboot, so tmpfs is right here and wrong for the
///   records above. Unset is still an error.
/// - **macOS:** `<home>/Library/Application Support/eitri/session-leases/`, where `<home>` is the
///   account's home directory from the user database (`getpwuid_r`), not `$HOME`. macOS has no
///   `XDG_RUNTIME_DIR`, and it is deliberately NOT consulted even when someone sets it: a process
///   launched with it and one launched without would split the lock. Two other homes were
///   rejected (2026-09-17):
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
/// Before 2026-09-17 macOS took the Linux branch and failed on the unset variable: a new session started
/// without a lease, resume and handoff failed.
pub(crate) fn leases_dir() -> std::io::Result<PathBuf> {
    if let Some(root) = test_root() {
        return Ok(root.join("session-leases"));
    }
    platform_leases_dir()
}

#[cfg(not(target_os = "macos"))]
fn platform_leases_dir() -> std::io::Result<PathBuf> {
    runtime_leases_dir(std::env::var_os("XDG_RUNTIME_DIR").as_deref())
}

/// `$XDG_RUNTIME_DIR/eitri/session-leases`, only for an absolute `XDG_RUNTIME_DIR`: an empty or
/// relative one would put the locks under the working directory, where two windows started in
/// different directories each take "the" lock for one session.
#[cfg(not(target_os = "macos"))]
fn runtime_leases_dir(xdg_runtime_dir: Option<&std::ffi::OsStr>) -> std::io::Result<PathBuf> {
    match xdg_runtime_dir {
        Some(dir) if Path::new(dir).is_absolute() => Ok(PathBuf::from(dir).join("eitri/session-leases")),
        Some(_) => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "XDG_RUNTIME_DIR is not an absolute path",
        )),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "XDG_RUNTIME_DIR is not set",
        )),
    }
}

#[cfg(target_os = "macos")]
fn platform_leases_dir() -> std::io::Result<PathBuf> {
    Ok(macos_leases_dir_under(&account_home_dir()?))
}

#[cfg(target_os = "macos")]
fn macos_leases_dir_under(home: &Path) -> PathBuf {
    home.join("Library/Application Support/eitri/session-leases")
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
/// The root is `$TMPDIR/eitri-agent-test-state/<pid>`. libtest has no after-all-tests hook, so the
/// root is removed by a C `atexit` handler ([`remove_test_root_at_exit`]), which runs when the test
/// binary returns from `main` or calls `exit`, passing or failing; the parent goes with it when no
/// other running binary has a root there. A run that is killed runs no handler, so its root is left;
/// keyed on the pid, it is found by the next run's `prune_dead_roots` once that pid is gone (a root
/// made inside another pid namespace may name a pid that is alive out here, and is only ever
/// removed by its own handler).
#[doc(hidden)]
pub fn redirect_state_to_a_test_root() -> PathBuf {
    TEST_ROOT
        .get_or_init(|| {
            let parent = std::env::temp_dir().join("eitri-agent-test-state");
            prune_dead_roots(&parent);
            let root = parent.join(std::process::id().to_string());
            // A pid can be reused, so a leftover root from a dead process with this same pid may
            // exist. Start it empty rather than inheriting someone else's records.
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("creating the test state root should succeed");
            ROOT_OWNER.store(std::process::id(), std::sync::atomic::Ordering::SeqCst);
            // SAFETY: registers a plain `extern "C"` function with no arguments; it never unwinds.
            unsafe { libc::atexit(remove_test_root_at_exit) };
            root
        })
        .clone()
}

/// The pid that made [`TEST_ROOT`]: a child forked from the test process that calls `exit` instead of
/// `_exit` runs the parent's `atexit` handlers too, and must not remove a root the parent still uses.
static ROOT_OWNER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Removes this process's test root, then its parent if that is empty. Never panics (it runs inside
/// `exit`): a root that cannot be removed is left for `prune_dead_roots`. A thread nothing joins (a
/// prompt history's writer) can still be writing into the root while the process exits and make part
/// of it again, so the root is removed until it has stayed gone for 50 ms, for at most 5 s; the
/// process ends right after, and such a thread with it.
extern "C" fn remove_test_root_at_exit() {
    use std::time::{Duration, Instant};
    if ROOT_OWNER.load(std::sync::atomic::Ordering::SeqCst) != std::process::id() {
        return;
    }
    let Some(root) = TEST_ROOT.get() else { return };
    let give_up = Instant::now() + Duration::from_secs(5);
    let mut gone_since = None;
    while Instant::now() < give_up {
        if std::fs::symlink_metadata(root).is_ok() {
            let _ = std::fs::remove_dir_all(root);
            gone_since = None;
        } else if gone_since.get_or_insert_with(Instant::now).elapsed() >= Duration::from_millis(50) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // Only an empty parent goes: another test binary running now keeps its own root there.
    if let Some(parent) = root.parent() {
        let _ = std::fs::remove_dir(parent);
    }
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
    // Canonical, as a session's own project root always is: the permission policy cards every
    // path-judged call under a root that does not resolve to itself, and macOS's `$TMPDIR` is under
    // `/var -> private/var`.
    dir.canonicalize()
        .expect("a test workspace directory just created should resolve")
}

/// A test workspace (as [`test_workspace_dir`]) that is the top level of its own git repository:
/// a `.git` holding the least git accepts as a repository (`HEAD` naming a ref, `objects/`,
/// `refs/`), written by hand so neither git nor the host's templates are involved.
///
/// Test-only. For a test whose code under test looks for the repository a git call would use, by
/// walking up from the project for a `.git`. Under a bare workspace that walk reaches the system temp
/// dir, which other programs share: a `.git` anyone leaves there (a Codex sandbox makes an empty one
/// as a mount point while it runs) turns every git call in the workspace into "a repository above the
/// project root". A `.git` at the workspace itself ends the walk there, so nothing above it counts.
#[doc(hidden)]
pub fn test_repository_dir(label: &str) -> PathBuf {
    let dir = test_workspace_dir(label);
    let git_dir = dir.join(".git");
    std::fs::create_dir_all(git_dir.join("objects")).expect("creating a test repository's objects/");
    std::fs::create_dir_all(git_dir.join("refs/heads")).expect("creating a test repository's refs/");
    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("writing a test repository's HEAD");
    dir
}

/// Removes every sibling root whose owning process is gone.
///
/// Liveness is `crate::process_probe::pid_is_alive`, i.e. `kill(pid, 0)`, on Linux and macOS alike.
/// It used to be `/proc/<pid>` existing, which on macOS is never true, so every root -- including
/// a concurrently running test binary's -- was deleted (until 2026-09-17). A name that is
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

    /// A test binary that redirected its state leaves nothing in the temp dir once it exits: a
    /// fresh copy of this binary under a private `TMPDIR` makes a workspace in its root and ends,
    /// and neither the root nor `eitri-agent-test-state` is there afterwards.
    #[test]
    fn a_test_binary_leaves_no_state_root_behind_when_it_exits() {
        const CHILD: &str = "EITRI_TEST_STATE_ROOT_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let dir = test_workspace_dir("exit");
            std::fs::write(dir.join("record"), "kept until exit").unwrap();
            assert!(dir.starts_with(std::env::temp_dir()), "{dir:?}");
            return;
        }
        let tmp = crate::test_tmp::TmpDir::new("nv-state-exit", "child");
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "state_dirs::tests::a_test_binary_leaves_no_state_root_behind_when_it_exits",
                "--exact",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .env("TMPDIR", &*tmp)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "the child run failed or ran nothing:\n{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let left: Vec<_> = std::fs::read_dir(&*tmp)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        assert!(left.is_empty(), "left in the child's TMPDIR: {left:?}");
    }

    /// Both directories must land under the redirect, or a test run writes into the developer's
    /// real XDG directories -- the defect this module exists to close.
    #[test]
    fn a_redirected_process_writes_neither_records_nor_leases_into_the_real_xdg_dirs() {
        let root = redirect_state_to_a_test_root();
        assert!(conversations_dir().unwrap().starts_with(&root));
        assert!(history_dir().unwrap().starts_with(&root));
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
            dir.ends_with("Library/Application Support/eitri/session-leases"),
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

    /// The whole point of `history_dir` being its own directory: `read_conversation_records` and
    /// `prune` walk `conversations/` and treat every `.json` there as a record, and `prune` never
    /// deletes what it cannot parse. A history file underneath it would be read and discarded on
    /// every start screen, and would hold one of that conversation's 16 slots forever.
    #[test]
    fn history_lives_beside_the_conversation_records_and_never_inside_them() {
        redirect_state_to_a_test_root();
        let history = history_dir().unwrap();
        let conversations = conversations_dir().unwrap();
        assert!(
            !history.starts_with(&conversations),
            "{history:?} is inside {conversations:?}"
        );
        assert_eq!(history.parent(), conversations.parent());
    }

    /// An empty or relative `XDG_STATE_HOME` is ignored, as `eitri_core::state_paths` ignores it:
    /// taken as given it put records and histories under the working directory.
    #[test]
    fn an_empty_or_relative_state_home_is_ignored() {
        use std::ffi::OsStr;
        let home = Some(OsStr::new("/home/u"));
        for state in ["", "state", "./state"] {
            assert_eq!(
                state_kind_dir(Some(OsStr::new(state)), home, "conversations").unwrap(),
                Path::new("/home/u/.local/state/eitri/conversations"),
                "XDG_STATE_HOME={state:?}"
            );
        }
        assert_eq!(
            state_kind_dir(Some(OsStr::new("/s")), home, "history").unwrap(),
            Path::new("/s/eitri/history")
        );
        assert!(state_kind_dir(Some(OsStr::new("")), Some(OsStr::new("")), "history").is_err());
        assert!(state_kind_dir(None, Some(OsStr::new("rel")), "history").is_err());
        assert!(state_kind_dir(None, None, "history").is_err());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn an_empty_or_relative_runtime_dir_gives_no_lease_dir() {
        use std::ffi::OsStr;
        assert_eq!(
            runtime_leases_dir(Some(OsStr::new("/run/user/1000"))).unwrap(),
            Path::new("/run/user/1000/eitri/session-leases")
        );
        assert!(runtime_leases_dir(Some(OsStr::new(""))).is_err());
        assert!(runtime_leases_dir(Some(OsStr::new("run"))).is_err());
        assert!(runtime_leases_dir(None).is_err());
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
        // `a` is canonicalized (see `test_workspace_dir`'s own comment on why); on macOS
        // `$TMPDIR` has a symlinked ancestor (`/var` -> `/private/var`), so the uncanonicalized
        // root must be resolved the same way before comparing.
        let root = redirect_state_to_a_test_root();
        let root = if cfg!(target_os = "macos") {
            root.canonicalize().expect("the test root should resolve")
        } else {
            root
        };
        assert!(a.starts_with(root));
    }

    /// A root belonging to a pid that no longer exists is removed; a live one is not. Pid 1 always
    /// exists (init on Linux, launchd on macOS), and `u32::MAX` is above any real pid. `u32::MAX`
    /// and `0` are also the two names a bare `kill(pid, 0)` would call alive -- `-1` and "my own
    /// process group" -- so they are kept here as the regression cases for that.
    #[test]
    fn pruning_removes_dead_roots_and_leaves_live_ones() {
        let parent = std::env::temp_dir().join(format!("eitri-prune-test-{}", uuid::Uuid::new_v4()));
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
