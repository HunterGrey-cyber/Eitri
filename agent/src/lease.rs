// agent/src/lease.rs
//! An exclusive, `flock`-backed advisory lock proving one Eitri-participating client currently
//! owns a given provider session (design doc §8.2). Lives under `$XDG_RUNTIME_DIR` (tmpfs, cleared
//! on logout) on Linux, and under `~/Library/Application Support` on macOS, which has no such
//! directory (see `state_dirs::leases_dir` for why not the temp dir) -- correct here, unlike the persistent identity in `persistence.rs`, since a lease
//! has no meaning across a reboot. `flock`'s own kernel semantics are the actual safety mechanism:
//! a crashed or killed process automatically releases every lock it held (every fd referencing the
//! lock's underlying open file description closes), so this module needs no heartbeat thread or
//! stale-lease sweep to stay correct -- see this plan's "Explicitly out of scope" section.
//!
//! **One lease domain, and it is per SESSION, not per directory**: `try_acquire(provider,
//! canonical_cwd, provider_session_id)` answers "may this window drive THIS Claude session?", which
//! is only answerable once a session id exists -- on resume, or after adoption. It is deliberately
//! NOT a claim on the directory: two windows may each run their own agent in one project, and
//! design doc §16.5 asks only that the SECOND window resuming the SAME session be refused.
//!
//! It is ADVISORY and this module says so rather than implying otherwise: it binds processes that
//! participate in this protocol. Nothing here can stop a `claude` someone runs by hand in the same
//! directory, and Claude's own documentation records that two clients resuming one session
//! interleave a single transcript. Design doc §8.5 and §17.7 reject any stronger claim.

use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// `Debug` is derived deliberately, not incidentally: a caller that expected `AlreadyHeld` and got
/// something else needs to be able to print what it actually got. Both fields are cheap and safe to
/// format (`File` renders as its fd, `PathBuf` as the lease path), and neither carries a secret.
///
/// No manual `Drop` impl: the `flock` is released when the last file descriptor referring to the
/// open file description closes, which for a normally-dropped `SessionLease` is simply `file`'s
/// own automatic drop -- the compiler's field drop glue already does this with no impl needed.
/// (`into_inherited_fd` calls `std::mem::forget(self)` specifically to suppress that field drop
/// when the fd is being handed to a child process instead.)
#[derive(Debug)]
pub struct SessionLease {
    file: File,
    path: PathBuf,
}

#[derive(Debug)]
pub enum LeaseError {
    /// Another process already holds this exact lease -- design doc §8.2: "acquire 失败返回
    /// session_in_use". Not a degraded/retry condition; the caller must offer focus-existing-window
    /// or fork, never a silent retry loop.
    AlreadyHeld,
    Io(std::io::Error),
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeaseError::AlreadyHeld => write!(f, "session lease already held by another process"),
            LeaseError::Io(e) => write!(f, "session lease I/O error: {e}"),
        }
    }
}

impl std::error::Error for LeaseError {}

/// Where the lock files live -- `state_dirs::leases_dir()`, i.e.
/// `$XDG_RUNTIME_DIR/eitri/session-leases/` on Linux and
/// `~/Library/Application Support/eitri/session-leases/` on macOS, unless a test has redirected it.
///
/// The thread-local checked first is narrower still and belongs to this module's own tests: several
/// of them acquire the SAME key (`claude`/`/tmp/project`/`prov-1`) to assert contention, so they
/// have to be invisible to each other as well as to the real runtime directory.
fn leases_dir() -> std::io::Result<PathBuf> {
    #[cfg(test)]
    if let Some(dir) = test_override::current() {
        return Ok(dir);
    }
    crate::state_dirs::leases_dir()
}

/// A per-TEST-THREAD lease directory, so this module's own tests can isolate themselves from each
/// other -- two of them deliberately acquire the same key.
///
/// They used to do this by setting `XDG_RUNTIME_DIR`, which is not safe in a multi-threaded test
/// binary: `std::env::set_var`/`remove_var` are process-wide, so this teardown could unset the
/// variable out from under any other test reading it on another thread. Worse than flaky: the
/// isolated directory is deleted at the end of each scenario, and a concurrent acquirer would then
/// recreate and re-lock the same path, so a contention assertion could fail for a reason that has
/// nothing to do with contention.
///
/// A thread-local is the right shape because cargo runs each test on its own thread, so this is
/// genuinely private to the test that set it and invisible to every other. It is also why it cannot
/// serve the crate's other tests: `AgentConversation` takes its session lease on an ingestion thread
/// no test owns. Those use `state_dirs::redirect_state_to_a_test_root()` instead.
#[cfg(test)]
mod test_override {
    use std::cell::RefCell;
    use std::path::PathBuf;

    thread_local! {
        static LEASES_DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }

    pub(super) fn current() -> Option<PathBuf> {
        LEASES_DIR.with(|dir| dir.borrow().clone())
    }

    /// Redirects this thread's lease directory for as long as it is alive, and deletes it on drop
    /// so a test leaves nothing behind.
    pub(super) struct IsolatedLeasesDir(PathBuf);

    impl IsolatedLeasesDir {
        pub(super) fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("agent-lease-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            LEASES_DIR.with(|slot| *slot.borrow_mut() = Some(dir.clone()));
            Self(dir)
        }

        pub(super) fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for IsolatedLeasesDir {
        fn drop(&mut self) {
            LEASES_DIR.with(|slot| *slot.borrow_mut() = None);
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// Design doc §8.2: "文件名使用 key 的 SHA-256，不把路径原文暴露在公共 socket 名称里" -- the key
/// is `provider + canonical_cwd + provider_session_id`, hex-encoded SHA-256.
fn lease_key_hash(provider: &str, canonical_cwd: &str, provider_session_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(provider.as_bytes());
    hasher.update(b"\0");
    hasher.update(canonical_cwd.as_bytes());
    hasher.update(b"\0");
    hasher.update(provider_session_id.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

impl SessionLease {
    /// Non-blocking: returns `Err(LeaseError::AlreadyHeld)` immediately if another process already
    /// holds this exact lease, never blocks waiting for it (Global Constraints: no unbounded
    /// blocking wait). Writes diagnostic metadata (owner pid, instance id, a fixed `started_at`
    /// timestamp) into the file after acquiring the lock -- purely informational, since the lock
    /// itself (not this metadata) is what a future acquirer's own `try_acquire` actually checks.
    pub fn try_acquire(provider: &str, canonical_cwd: &str, provider_session_id: &str) -> Result<Self, LeaseError> {
        let dir = leases_dir().map_err(LeaseError::Io)?;
        // 0700 for every component this creates. On macOS the directory is under the user's home
        // rather than an already-private runtime dir (see `state_dirs::leases_dir`), and lock names
        // are hashes of a cwd and a session id, which are nobody else's business. Directories that
        // already exist keep their mode.
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(LeaseError::Io)?;
        let key = lease_key_hash(provider, canonical_cwd, provider_session_id);
        let path = dir.join(format!("{key}.lock"));

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(LeaseError::Io)?;

        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                return Err(LeaseError::AlreadyHeld);
            }
            return Err(LeaseError::Io(err));
        }

        // Acquired -- now safe to (re)write metadata; truncate first since a stale prior owner's
        // metadata (from a crash that left the file behind, though not the lock -- flock released
        // automatically on crash) should not linger.
        file.set_len(0).map_err(LeaseError::Io)?;
        let instance_id = uuid::Uuid::new_v4();
        // `owner_pid` records `std::process::id()` -- the pid of whichever process called
        // `try_acquire`, i.e. the *host* process. After an Eitri->CLI handoff (`handoff.rs`),
        // sole ownership of the lock passes to the exec'd `claude` child, which never calls
        // `try_acquire` itself and so never updates this field. Someone debugging an unexpected
        // `AlreadyHeld` after a handoff will read `owner_pid` here and find the host process that
        // originally acquired the lease and has since closed its own descriptor -- not the
        // `claude` process that is the lock's actual current sole holder. This is informational
        // metadata only (see the module doc: the lock itself, not this field, is what a future
        // `try_acquire` checks), so it is left as-is rather than "fixed" to track the handoff.
        let metadata = format!(
            "{{\"owner_pid\":{},\"instance_id\":\"{instance_id}\",\"started_at\":\"{}\"}}",
            std::process::id(),
            chrono_like_now(),
        );
        file.write_all(metadata.as_bytes()).map_err(LeaseError::Io)?;

        Ok(Self { file, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether this lease is the one for exactly this provider, directory and session. A caller that
    /// took a lease ahead of time and hands it to a resume uses this so the resume never drives a
    /// session it holds no lock on.
    pub fn is_for(&self, provider: &str, canonical_cwd: &str, provider_session_id: &str) -> bool {
        leases_dir().is_ok_and(|dir| {
            self.path
                == dir.join(format!(
                    "{}.lock",
                    lease_key_hash(provider, canonical_cwd, provider_session_id)
                ))
        })
    }

    /// Whether some descriptor anywhere holds this lease right now, without taking it.
    ///
    /// Opens the lock file read-only and tries a shared, non-blocking `flock`, released at once.
    /// flock is per open file description, so this sees a lease THIS process holds on another
    /// descriptor too -- which is what the prune and the session chooser need: one window's own
    /// tabs hold leases. **One cost, accepted (session tabs plan, ruling 21):** a `try_acquire`
    /// racing this probe on another descriptor can see `AlreadyHeld` for the microseconds the
    /// shared lock exists. It runs only while pruning past the cap and while building the chooser.
    pub fn is_held(provider: &str, canonical_cwd: &str, provider_session_id: &str) -> std::io::Result<bool> {
        let dir = leases_dir()?;
        let path = dir.join(format!(
            "{}.lock",
            lease_key_hash(provider, canonical_cwd, provider_session_id)
        ));
        let file = match OpenOptions::new().read(true).open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
        if rc == 0 {
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
            return Ok(false);
        }
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            Ok(true)
        } else {
            Err(err)
        }
    }

    /// Prepares this lease's fd for inheritance across `fork`+`exec` and hands ownership to the
    /// caller as a raw fd number -- clears `FD_CLOEXEC` (Rust's `std::fs::File` sets it by default
    /// on open, which would otherwise close the fd at `exec` time and silently drop the lock) and
    /// `std::mem::forget`s `self` so `SessionLease`'s own `Drop` never runs and closes the fd out
    /// from under the process that's about to inherit it. This plan's own "Verified facts" point 3
    /// confirms this exact mechanism (fcntl F_SETFD to clear CLOEXEC, then spawn, then the child
    /// reconstructs a `File` from the inherited fd number) genuinely carries the `flock` state
    /// across `exec` -- do not "simplify" this back to passing a fresh path for the child to
    /// re-open and re-lock itself, which would race against this process's own still-held lock.
    pub fn into_inherited_fd(self) -> std::io::Result<std::os::unix::io::RawFd> {
        let fd = self.file.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags == -1 {
            return Err(std::io::Error::last_os_error());
        }
        let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) };
        if rc == -1 {
            return Err(std::io::Error::last_os_error());
        }
        std::mem::forget(self);
        Ok(fd)
    }
}

/// A minimal, dependency-free `YYYY-MM-DDTHH:MM:SSZ`-shaped UTC timestamp for the lease's
/// `started_at` diagnostic field -- deliberately not pulling in the `chrono` or `time` crate for
/// one informational field with no parser consuming it back in this phase (Global Constraints:
/// YAGNI). Uses `std::time::SystemTime` and hand-rolled UTC civil-calendar math (the well-known
/// "days since epoch -> y/m/d" algorithm), not wall-clock-library-quality but sufficient for a
/// human-readable diagnostic string.
fn chrono_like_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let (h, m, s) = (time_of_day / 3600, (time_of_day % 3600) / 60, time_of_day % 60);
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Howard Hinnant's well-known `civil_from_days` algorithm (public domain, widely reused across
/// language standard libraries) for epoch-days -> proleptic Gregorian (y, m, d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::test_override::IsolatedLeasesDir;
    use super::*;

    // These were one long test function, because isolating them meant mutating XDG_RUNTIME_DIR and
    // that is process-wide. `IsolatedLeasesDir` is per-thread, so they are separate tests again and
    // each can say in its own name what it pins.

    /// `try_acquire` creates a missing lease directory private to the user.
    #[test]
    fn a_missing_lease_directory_is_created_0700() {
        use std::os::unix::fs::PermissionsExt;
        let isolated = IsolatedLeasesDir::new();
        std::fs::remove_dir_all(isolated.path()).unwrap();

        let lease = SessionLease::try_acquire("claude", "/tmp/project", "prov-mode").unwrap();
        let mode = std::fs::metadata(isolated.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "lease directory mode was {mode:o}");
        drop(lease);
    }

    #[test]
    fn a_session_lease_is_reacquirable_once_released() {
        let _isolated = IsolatedLeasesDir::new();
        let lease = SessionLease::try_acquire("claude", "/tmp/project", "prov-1").unwrap();
        assert!(lease.path().exists());
        drop(lease);
        assert!(SessionLease::try_acquire("claude", "/tmp/project", "prov-1").is_ok());
    }

    #[test]
    fn a_second_acquire_of_a_held_session_lease_reports_contention() {
        let _isolated = IsolatedLeasesDir::new();
        let _first = SessionLease::try_acquire("claude", "/tmp/project", "prov-1").unwrap();
        let second = SessionLease::try_acquire("claude", "/tmp/project", "prov-1");
        assert!(matches!(second, Err(LeaseError::AlreadyHeld)));
    }

    #[test]
    fn session_leases_for_different_workspaces_do_not_contend() {
        let _isolated = IsolatedLeasesDir::new();
        let _a = SessionLease::try_acquire("claude", "/tmp/project-a", "prov-1").unwrap();
        assert!(SessionLease::try_acquire("claude", "/tmp/project-b", "prov-1").is_ok());
    }

    /// Two DIFFERENT sessions in one directory do not contend, and that is the design, not an
    /// oversight: the lease claims a Claude session, never the directory. Design doc §16.5 asks
    /// only that a second window resuming the SAME session be refused, which the test above pins.
    #[test]
    fn two_different_sessions_in_one_directory_do_not_contend() {
        let _isolated = IsolatedLeasesDir::new();
        let _a = SessionLease::try_acquire("claude", "/tmp/project", "prov-1").unwrap();
        assert!(SessionLease::try_acquire("claude", "/tmp/project", "prov-2").is_ok());
    }

    /// Ruling 21: the probe sees a lease held by this very process on another descriptor (flock is
    /// per open file description), forgets it once dropped, and never takes it itself.
    #[test]
    fn is_held_sees_a_held_lease_and_takes_nothing() {
        let _dir = test_override::IsolatedLeasesDir::new();
        assert!(
            !SessionLease::is_held("claude", "/tmp/project", "prov-probe").unwrap(),
            "no lock file yet"
        );
        let lease = SessionLease::try_acquire("claude", "/tmp/project", "prov-probe").unwrap();
        assert!(SessionLease::is_held("claude", "/tmp/project", "prov-probe").unwrap());
        assert!(
            !SessionLease::is_held("claude", "/tmp/project", "prov-other").unwrap(),
            "another session's lease is another key"
        );
        drop(lease);
        assert!(!SessionLease::is_held("claude", "/tmp/project", "prov-probe").unwrap());
        let _again = SessionLease::try_acquire("claude", "/tmp/project", "prov-probe")
            .expect("a probe must leave the lease free to take");
    }

    /// A lease handed to a resume must be the one for that very session: the resume would otherwise
    /// drive a session it holds no lock on.
    #[test]
    fn a_lease_knows_which_session_it_is_for() {
        let _dir = test_override::IsolatedLeasesDir::new();
        let lease = SessionLease::try_acquire("claude", "/tmp/project", "prov-own").unwrap();
        assert!(lease.is_for("claude", "/tmp/project", "prov-own"));
        assert!(!lease.is_for("claude", "/tmp/project", "prov-other"));
        assert!(!lease.is_for("claude", "/tmp/elsewhere", "prov-own"));
    }
}
