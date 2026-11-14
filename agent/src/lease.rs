// agent/src/lease.rs
//! An exclusive, `flock`-backed advisory lock proving one Eitri-participating client currently
//! owns a given provider session. Lives under `$XDG_RUNTIME_DIR` (tmpfs, cleared
//! on logout) on Linux, and under `~/Library/Application Support` on macOS, which has no such
//! directory (see `state_dirs::leases_dir` for why not the temp dir) -- correct here, unlike the persistent identity in `persistence.rs`, since a lease
//! has no meaning across a reboot. `flock`'s own kernel semantics are the actual safety mechanism:
//! a crashed or killed process automatically releases every lock it held (every fd referencing the
//! lock's underlying open file description closes), so this module needs no heartbeat thread or
//! stale-lease sweep to stay correct.
//!
//! **One lease domain, and it is per SESSION, not per directory**: `try_acquire(provider,
//! canonical_cwd, provider_session_id)` answers "may this window drive THIS Claude session?", which
//! is only answerable once a session id exists -- on resume, or after adoption. It is deliberately
//! NOT a claim on the directory: two windows may each run their own agent in one project, and
//! only the SECOND window resuming the SAME session is refused.
//!
//! It is ADVISORY and this module says so rather than implying otherwise: it binds processes that
//! participate in this protocol. Nothing here can stop a `claude` someone runs by hand in the same
//! directory, and Claude's own documentation records that two clients resuming one session
//! interleave a single transcript. No stronger claim is made, because none would be true.

use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
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
    /// Another process already holds this exact lease: the session is in use. Not a degraded/retry
    /// condition; the caller must offer focus-existing-window
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

        /// The same redirect on another thread, for a test that runs an acquire there; it leaves the
        /// directory to the guard that made it.
        pub(super) fn at(dir: PathBuf) -> SharedLeasesDir {
            LEASES_DIR.with(|slot| *slot.borrow_mut() = Some(dir));
            SharedLeasesDir
        }
    }

    impl Drop for IsolatedLeasesDir {
        fn drop(&mut self) {
            LEASES_DIR.with(|slot| *slot.borrow_mut() = None);
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(super) struct SharedLeasesDir;

    impl Drop for SharedLeasesDir {
        fn drop(&mut self) {
            LEASES_DIR.with(|slot| *slot.borrow_mut() = None);
        }
    }
}

/// Refuses anything but a regular file owned by this process's effective user, read from the open
/// handle, so what is checked is what is locked and written.
fn require_own_regular_file(file: &File, path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata()?;
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    if meta.is_file() && meta.uid() == unsafe { libc::geteuid() } {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} is not a regular file of this user", path.display()),
        ))
    }
}

/// The file is named by the key's SHA-256, so no path appears in a name others can list -- the key
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
    /// holds this exact lease, never blocks waiting for it (an unbounded wait would hang the thread
    /// that asked). Writes diagnostic metadata (owner pid, instance id, a fixed `started_at`
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

        // The file is truncated and written once locked, so it must be this user's own regular file:
        // a link in its place is not followed (it would truncate whatever it names), and a FIFO fails
        // at once rather than blocking the open.
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .mode(crate::private_fs::PRIVATE_FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)
            .map_err(LeaseError::Io)?;
        require_own_regular_file(&file, &path).map_err(LeaseError::Io)?;

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
    /// tabs hold leases. **One cost, accepted:** a `try_acquire`
    /// racing this probe on another descriptor can see `AlreadyHeld` for the microseconds the
    /// shared lock exists. It runs only while pruning past the cap and while building the chooser.
    pub fn is_held(provider: &str, canonical_cwd: &str, provider_session_id: &str) -> std::io::Result<bool> {
        let dir = leases_dir()?;
        let path = dir.join(format!(
            "{}.lock",
            lease_key_hash(provider, canonical_cwd, provider_session_id)
        ));
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)
        {
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

    /// Whether the held lease names this live process. Read on a worker because lock and file probes can wait
    /// on the filesystem; the start time keeps a reused process id from identifying the linked peer.
    #[cfg(target_os = "linux")]
    pub fn is_held_by(provider: &str, canonical_cwd: &str, session: &str, pid: u32, start: u64) -> bool {
        if !Self::is_held(provider, canonical_cwd, session).unwrap_or(false) {
            return false;
        }
        let Ok(dir) = leases_dir() else {
            return false;
        };
        let path = dir.join(format!("{}.lock", lease_key_hash(provider, canonical_cwd, session)));
        let Ok(text) = crate::private_fs::read_private_to_string_capped(&path, 4096) else {
            return false;
        };
        let Ok(metadata) = serde_json::from_str::<serde_json::Value>(&text) else {
            return false;
        };
        metadata["owner_pid"].as_u64() == Some(pid as u64) && crate::child::start_time(pid).ok() == Some(start)
    }

    /// Prepares this lease's fd for inheritance across `fork`+`exec` and hands ownership to the
    /// caller as a raw fd number -- clears `FD_CLOEXEC` (Rust's `std::fs::File` sets it by default
    /// on open, which would otherwise close the fd at `exec` time and silently drop the lock) and
    /// `std::mem::forget`s `self` so `SessionLease`'s own `Drop` never runs and closes the fd out
    /// from under the process that's about to inherit it. Verified for real: this exact mechanism
    /// (fcntl F_SETFD to clear CLOEXEC, then spawn, then the child
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

    /// A second descriptor on this lease's open file, close-on-exec, for the window a conversation
    /// moves to. `flock` belongs to the open file, not the descriptor, so the lease stays held while
    /// either descriptor is open: handing it over never lets it go for a moment.
    pub fn descriptor_for_move(&self) -> std::io::Result<std::os::fd::OwnedFd> {
        // `try_clone` duplicates with `F_DUPFD_CLOEXEC`.
        Ok(self.file.try_clone()?.into())
    }

    /// The lease for exactly this provider, directory and session, from a descriptor another window
    /// handed over. Refused unless the descriptor is the lease file's own open file and holds its lock
    /// (taking it if nobody does), so a descriptor for another file or session never passes as one.
    pub fn from_moved(
        fd: std::os::fd::OwnedFd,
        provider: &str,
        canonical_cwd: &str,
        provider_session_id: &str,
    ) -> Result<Self, LeaseError> {
        use std::os::unix::fs::MetadataExt;
        let dir = leases_dir().map_err(LeaseError::Io)?;
        let path = dir.join(format!(
            "{}.lock",
            lease_key_hash(provider, canonical_cwd, provider_session_id)
        ));
        let file = File::from(fd);
        let raw = file.as_raw_fd();
        // Close-on-exec whatever the sender set: a lease must never leak into a child.
        let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
        if flags == -1 || unsafe { libc::fcntl(raw, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
            return Err(LeaseError::Io(std::io::Error::last_os_error()));
        }
        let held = file.metadata().map_err(LeaseError::Io)?;
        let named = std::fs::metadata(&path).map_err(LeaseError::Io)?;
        if (held.dev(), held.ino()) != (named.dev(), named.ino()) {
            return Err(LeaseError::Io(std::io::Error::other(
                "the descriptor handed over is not this session's lease file",
            )));
        }
        // Succeeds at once on the open file that holds the lock (it already does), and on a lock
        // nobody holds; fails only when another open file holds it.
        let rc = unsafe { libc::flock(raw, libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                return Err(LeaseError::AlreadyHeld);
            }
            return Err(LeaseError::Io(err));
        }
        Ok(Self { file, path })
    }

    /// Names `pid` as the lease's owner in its metadata, keeping whatever else the metadata holds (the
    /// instance id and start time `try_acquire` wrote). Written through the descriptor the lease holds
    /// and never by reopening it for writing, so the lock is not touched; read through a second,
    /// read-only open of the same file (a lock belongs to its open file, and closing another one
    /// releases nothing).
    ///
    /// Refused when the path no longer names the file this lease holds: what would be written is
    /// what nobody reads. Metadata that cannot be read back is replaced by the owner alone; no
    /// instance id or start time is made up for it.
    ///
    /// One write at offset 0, padded with spaces to the file's current length, so the file never
    /// gets shorter: there is no moment at which it holds the new text cut short or the old text's
    /// tail. A reader can still meet the empty file `try_acquire` leaves between its truncation and
    /// its write, so whoever reads this metadata treats text it cannot parse as an owner it does
    /// not know.
    pub fn rewrite_owner_pid(&self, pid: u32) -> std::io::Result<()> {
        use std::os::unix::fs::{FileExt, MetadataExt};
        let reader = std::fs::File::open(&self.path)?;
        let held = self.file.metadata()?;
        let read = reader.metadata()?;
        if (held.dev(), held.ino()) != (read.dev(), read.ino()) {
            return Err(std::io::Error::other("the lease file was replaced"));
        }
        let mut text = String::new();
        let mut metadata = std::io::Read::read_to_string(&mut &reader, &mut text)
            .ok()
            .and_then(|_| serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text).ok())
            .unwrap_or_default();
        metadata.insert("owner_pid".to_string(), serde_json::Value::from(pid));
        let mut bytes = serde_json::to_vec(&metadata).map_err(std::io::Error::other)?;
        let length = usize::try_from(held.len()).unwrap_or(usize::MAX);
        if bytes.len() < length {
            bytes.resize(length, b' ');
        }
        self.file.write_all_at(&bytes, 0)
    }
}

/// A minimal, dependency-free `YYYY-MM-DDTHH:MM:SSZ`-shaped UTC timestamp for the lease's
/// `started_at` diagnostic field -- deliberately not pulling in the `chrono` or `time` crate for
/// one informational field with no parser consuming it back in this phase. Uses
/// `std::time::SystemTime` and hand-rolled UTC civil-calendar math (the well-known
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

    /// Whether the lease can be taken, asked again for up to five seconds while it looks held. A
    /// lock just let go can look held for a moment while another test thread starts a child process:
    /// the child has a copy of every descriptor of this process, and so the lock, until it execs.
    /// One never let go is still held after the five seconds, so this checks "released within five
    /// seconds".
    fn lease_comes_free(provider: &str, cwd: &str, session: &str) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match SessionLease::try_acquire(provider, cwd, session) {
                Ok(_) => return true,
                Err(LeaseError::AlreadyHeld) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                }
                Err(_) => return false,
            }
        }
    }

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

    /// A link planted at a lease's path is not followed: the file it names is neither locked nor
    /// truncated.
    #[test]
    fn a_link_in_a_leases_place_is_refused_and_its_target_left_alone() {
        let isolated = IsolatedLeasesDir::new();
        let target = isolated.path().join("victim.txt");
        std::fs::write(&target, b"keep me").unwrap();
        let key = lease_key_hash("claude", "/tmp/project", "prov-link");
        std::os::unix::fs::symlink(&target, isolated.path().join(format!("{key}.lock"))).unwrap();
        let err = SessionLease::try_acquire("claude", "/tmp/project", "prov-link").unwrap_err();
        assert!(matches!(err, LeaseError::Io(_)), "{err:?}");
        assert_eq!(std::fs::read(&target).unwrap(), b"keep me");
        assert!(SessionLease::is_held("claude", "/tmp/project", "prov-link").is_err());
    }

    /// A FIFO at a lease's path fails at once instead of blocking the thread that asked.
    #[test]
    fn a_fifo_in_a_leases_place_fails_at_once() {
        let isolated = IsolatedLeasesDir::new();
        let key = lease_key_hash("claude", "/tmp/project", "prov-fifo");
        let fifo = std::ffi::CString::new(
            isolated
                .path()
                .join(format!("{key}.lock"))
                .into_os_string()
                .into_encoded_bytes(),
        )
        .unwrap();
        // SAFETY: a valid NUL-terminated path for the call.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        let dir = isolated.path().to_path_buf();
        std::thread::spawn(move || {
            let _isolated_here = test_override::IsolatedLeasesDir::at(dir);
            let _ = tx.send(SessionLease::try_acquire("claude", "/tmp/project", "prov-fifo").is_err());
        });
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(10)),
            Ok(true),
            "the acquire must fail, and at once"
        );
    }

    #[test]
    fn a_session_lease_is_reacquirable_once_released() {
        let _isolated = IsolatedLeasesDir::new();
        let lease = SessionLease::try_acquire("claude", "/tmp/project", "prov-1").unwrap();
        assert!(lease.path().exists());
        drop(lease);
        assert!(lease_comes_free("claude", "/tmp/project", "prov-1"));
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
    /// oversight: the lease claims a Claude session, never the directory. All it has to refuse is
    /// a second window resuming the SAME session, which the test above pins.
    #[test]
    fn two_different_sessions_in_one_directory_do_not_contend() {
        let _isolated = IsolatedLeasesDir::new();
        let _a = SessionLease::try_acquire("claude", "/tmp/project", "prov-1").unwrap();
        assert!(SessionLease::try_acquire("claude", "/tmp/project", "prov-2").is_ok());
    }

    /// The probe sees a lease held by this very process on another descriptor (flock is per open
    /// file description), reads a lock file nobody holds as free, and never takes it itself.
    /// The free case is a file no one ever locked, not a lease just dropped: a child another test
    /// thread is starting keeps a copy of a dropped lease's descriptor, and its lock, until it execs.
    #[test]
    fn is_held_sees_a_held_lease_and_takes_nothing() {
        let _dir = test_override::IsolatedLeasesDir::new();
        assert!(
            !SessionLease::is_held("claude", "/tmp/project", "prov-probe").unwrap(),
            "no lock file yet"
        );
        let _lease = SessionLease::try_acquire("claude", "/tmp/project", "prov-probe").unwrap();
        assert!(SessionLease::is_held("claude", "/tmp/project", "prov-probe").unwrap());
        assert!(
            !SessionLease::is_held("claude", "/tmp/project", "prov-other").unwrap(),
            "another session's lease is another key"
        );
        let never_locked = leases_dir().unwrap().join(format!(
            "{}.lock",
            lease_key_hash("claude", "/tmp/project", "prov-free")
        ));
        crate::test_tmp::write_private(&never_locked, b"").unwrap();
        assert!(!SessionLease::is_held("claude", "/tmp/project", "prov-free").unwrap());
        let _taken = SessionLease::try_acquire("claude", "/tmp/project", "prov-free")
            .expect("a probe must leave the lease free to take");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn is_held_by_names_the_live_owner_by_pid_and_start_time() {
        let _dir = IsolatedLeasesDir::new();
        let me = std::process::id();
        let start = crate::child::start_time(me).unwrap();
        assert!(
            !SessionLease::is_held_by("claude", "/tmp/project", "prov-holder", me, start),
            "a lease nobody holds names no one"
        );
        let lease = SessionLease::try_acquire("claude", "/tmp/project", "prov-holder").unwrap();
        assert!(SessionLease::is_held_by(
            "claude",
            "/tmp/project",
            "prov-holder",
            me,
            start
        ));
        assert!(
            !SessionLease::is_held_by("claude", "/tmp/project", "prov-holder", me, start + 1),
            "the same pid started at another time is another process"
        );
        assert!(!SessionLease::is_held_by(
            "claude",
            "/tmp/project",
            "prov-holder",
            me + 1,
            start
        ));
        std::fs::write(lease.path(), "{\"owner_pid\":").unwrap();
        assert!(
            !SessionLease::is_held_by("claude", "/tmp/project", "prov-holder", me, start),
            "metadata that does not read names no one"
        );
    }

    fn owner_in(path: &Path) -> serde_json::Value {
        let text = std::fs::read_to_string(path).unwrap();
        serde_json::from_str::<serde_json::Value>(&text).unwrap()
    }

    #[test]
    fn rewriting_the_owner_keeps_the_lock_and_the_rest_of_the_metadata() {
        let _dir = IsolatedLeasesDir::new();
        let lease = SessionLease::try_acquire("claude", "/tmp/project", "prov-owner").unwrap();
        let before = owner_in(lease.path());
        assert_eq!(before["owner_pid"], serde_json::json!(std::process::id()));

        lease.rewrite_owner_pid(4_194_000).unwrap();
        let after = owner_in(lease.path());
        assert_eq!(after["owner_pid"], serde_json::json!(4_194_000));
        assert_eq!(after["instance_id"], before["instance_id"]);
        assert_eq!(after["started_at"], before["started_at"]);
        assert!(matches!(
            SessionLease::try_acquire("claude", "/tmp/project", "prov-owner"),
            Err(LeaseError::AlreadyHeld)
        ));

        // A shorter pid leaves no tail of the longer one behind, and the file keeps its length.
        let length = std::fs::metadata(lease.path()).unwrap().len();
        lease.rewrite_owner_pid(7).unwrap();
        assert_eq!(owner_in(lease.path())["owner_pid"], serde_json::json!(7));
        assert_eq!(std::fs::metadata(lease.path()).unwrap().len(), length);
    }

    #[test]
    fn unreadable_metadata_is_replaced_by_the_owner_alone() {
        let _dir = IsolatedLeasesDir::new();
        let lease = SessionLease::try_acquire("claude", "/tmp/project", "prov-garbled").unwrap();
        std::fs::write(lease.path(), "{\"owner_pid\":12").unwrap();

        lease.rewrite_owner_pid(4_194_000).unwrap();
        let after = owner_in(lease.path());
        assert_eq!(
            after,
            serde_json::json!({ "owner_pid": 4_194_000 }),
            "nothing is made up"
        );
    }

    #[test]
    fn a_lease_file_replaced_under_the_lease_is_not_written() {
        let _dir = IsolatedLeasesDir::new();
        let lease = SessionLease::try_acquire("claude", "/tmp/project", "prov-replaced").unwrap();
        std::fs::remove_file(lease.path()).unwrap();
        std::fs::write(lease.path(), "{}").unwrap();

        assert!(lease.rewrite_owner_pid(4_194_000).is_err());
        assert_eq!(
            owner_in(lease.path()),
            serde_json::json!({}),
            "the new file is left as it is"
        );
    }

    #[test]
    fn a_moved_descriptor_holds_the_lease_after_the_original_is_dropped() {
        let _dir = IsolatedLeasesDir::new();
        let lease = SessionLease::try_acquire("claude", "/tmp/project", "prov-move").unwrap();
        let fd = lease.descriptor_for_move().unwrap();
        drop(lease);
        assert!(matches!(
            SessionLease::try_acquire("claude", "/tmp/project", "prov-move"),
            Err(LeaseError::AlreadyHeld)
        ));
        let moved = SessionLease::from_moved(fd, "claude", "/tmp/project", "prov-move").unwrap();
        assert!(moved.is_for("claude", "/tmp/project", "prov-move"));
        let flags = unsafe { libc::fcntl(moved.file.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0, "a moved lease never reaches a child");
        assert!(matches!(
            SessionLease::try_acquire("claude", "/tmp/project", "prov-move"),
            Err(LeaseError::AlreadyHeld)
        ));
        drop(moved);
        assert!(
            lease_comes_free("claude", "/tmp/project", "prov-move"),
            "dropping the moved lease lets it go"
        );
    }

    #[test]
    fn a_descriptor_for_another_sessions_lease_is_refused() {
        let _dir = IsolatedLeasesDir::new();
        let lease = SessionLease::try_acquire("claude", "/tmp/project", "prov-a").unwrap();
        let _other = SessionLease::try_acquire("claude", "/tmp/project", "prov-b").unwrap();
        let fd = lease.descriptor_for_move().unwrap();
        assert!(SessionLease::from_moved(fd, "claude", "/tmp/project", "prov-b").is_err());
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
