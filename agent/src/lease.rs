// agent/src/lease.rs
//! An exclusive, `flock`-backed advisory lock proving one Neovibe-participating client currently
//! owns a given provider session (design doc §8.2). Lives under `$XDG_RUNTIME_DIR` (tmpfs, cleared
//! on logout) -- correct here, unlike the persistent identity in `persistence.rs`, since a lease
//! has no meaning across a reboot. `flock`'s own kernel semantics are the actual safety mechanism:
//! a crashed or killed process automatically releases every lock it held (every fd referencing the
//! lock's underlying open file description closes), so this module needs no heartbeat thread or
//! stale-lease sweep to stay correct -- see this plan's "Explicitly out of scope" section.

use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::Write;
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

/// `$XDG_RUNTIME_DIR/neovibe/session-leases/`, matching this project's own established pattern
/// for ephemeral, per-session state (`supervisor::socket_path`, `agent::process`'s hook sockets).
fn leases_dir() -> std::io::Result<PathBuf> {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(runtime_dir).join("neovibe/session-leases"))
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
        std::fs::create_dir_all(&dir).map_err(LeaseError::Io)?;
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
        // `try_acquire`, i.e. the *host* process. After a Neovibe->CLI handoff (`handoff.rs`),
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
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
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
    use super::*;

    fn isolated_runtime_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-lease-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // SAFETY: this test mutates process-wide env state (XDG_RUNTIME_DIR). All three scenarios
    // are exercised sequentially within this one test function specifically so no other test in
    // this crate can interleave with these mutations on a separate thread -- cargo's default
    // multi-threaded test runner turned this exact shape (3 separate tests, each independently
    // calling `set_var`) into a real, reproduced race for `persistence.rs`'s own
    // `XDG_STATE_HOME`-mutating tests earlier in this same plan (4/9 parallel runs failed there
    // before being merged into one test the same way this one already is). Do not split this
    // back into 3 separate `#[test]` functions.
    #[test]
    fn session_lease_acquire_and_contention_behave_correctly() {
        // Scenario 1: acquire then release allows a fresh acquire.
        let dir = isolated_runtime_dir();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", &dir) };
        let lease = SessionLease::try_acquire("claude", "/tmp/project", "prov-1").unwrap();
        assert!(lease.path().exists());
        drop(lease);
        let lease2 = SessionLease::try_acquire("claude", "/tmp/project", "prov-1").unwrap();
        drop(lease2);
        let _ = std::fs::remove_dir_all(&dir);

        // Scenario 2: a second acquire of the same key while the first is held fails with AlreadyHeld.
        let dir = isolated_runtime_dir();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", &dir) };
        let _first = SessionLease::try_acquire("claude", "/tmp/project", "prov-1").unwrap();
        let second = SessionLease::try_acquire("claude", "/tmp/project", "prov-1");
        assert!(matches!(second, Err(LeaseError::AlreadyHeld)));
        drop(_first);
        let _ = std::fs::remove_dir_all(&dir);

        // Scenario 3: different keys do not contend.
        let dir = isolated_runtime_dir();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", &dir) };
        let _a = SessionLease::try_acquire("claude", "/tmp/project-a", "prov-1").unwrap();
        let b = SessionLease::try_acquire("claude", "/tmp/project-b", "prov-1");
        assert!(b.is_ok());
        let _ = std::fs::remove_dir_all(&dir);

        unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
    }
}
