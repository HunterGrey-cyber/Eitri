//! A `#[cfg(test)]`-only RAII guard shared by this crate's tests that each want their own throwaway
//! directory under the system temp dir. Its `Drop` removes the directory, so a test panicking on an
//! assertion still cleans up (a bare `remove_dir_all` written at the end of the test body would not
//! run in that case) and so a crate-wide test run never leaves anything behind in `/tmp`.
//!
//! Shared by tests in `agent_prefs`, `permission_store` and `prompt_history` so each gets
//! cleanup on unwinding without duplicating the guard.

#![cfg(test)]

use std::ops::Deref;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Owns a directory under the system temp dir (macOS: `agent::state_dirs::test_temp_dir()`) and removes it
/// (recursively) on drop.
///
/// The path it hands out is `state` inside that directory, not the directory itself: the state
/// writers judge the owner of the directory above theirs (`<state home>/eitri` in a real run), and
/// the temp dir itself belongs to root, so the scratch directory stands in for `eitri`.
pub(crate) struct ScratchDir {
    outer: PathBuf,
    dir: PathBuf,
}

impl ScratchDir {
    /// Creates `<std::env::temp_dir()>/<prefix>-<label>-<12 hex>/state` (macOS:
    /// `<agent::state_dirs::test_temp_dir()>/s<12 hex>/state`) and returns a guard that removes the whole
    /// directory below the temp dir on drop.
    pub(crate) fn new(prefix: &str, label: &str) -> Self {
        // Sockets are bound inside, and a socket path may not pass 103 bytes, so the name is kept short: 12
        // random hex digits are unique enough for one run's scratch, and a long `TMPDIR` (a build host's
        // per-run one) still leaves room. macOS's `TMPDIR` is already ~49 bytes, so there the directory is
        // named by the random id behind one letter, in the short test directory `agent` keeps for this. The
        // letter matters: that directory also holds the per-process state roots, named by pid and pruned when
        // their process is gone, and an id of digits alone would read as one of them.
        let id = &uuid::Uuid::new_v4().simple().to_string()[..12];
        #[cfg(target_os = "macos")]
        let outer = agent::state_dirs::test_temp_dir().join(format!("s{id}"));
        #[cfg(not(target_os = "macos"))]
        let outer = std::env::temp_dir().join(format!("{prefix}-{label}-{id}"));
        #[cfg(target_os = "macos")]
        let _ = (prefix, label);
        let dir = outer.join("state");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .unwrap();
        ScratchDir { outer, dir }
    }
}

impl Deref for ScratchDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.dir
    }
}

impl AsRef<Path> for ScratchDir {
    fn as_ref(&self) -> &Path {
        &self.dir
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        // A worker thread a test started (the editor record's writer, for one) is not joined and can still
        // write here for a moment after the test's last line, which makes the directory again under a
        // removal. So the directory must stay gone for a while, up to 5 s.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut absent_since = None;
        while Instant::now() < deadline {
            if self.outer.exists() {
                let _ = std::fs::remove_dir_all(&self.outer);
                absent_since = None;
            } else if absent_since.get_or_insert_with(Instant::now).elapsed() > Duration::from_millis(50) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Writes a private-state fixture with the production mode regardless of the test runner's umask.
pub(crate) fn write_private(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> std::io::Result<()> {
    agent::private_fs::write_private(path.as_ref(), bytes.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scratch_directory_name_never_reads_as_a_pid() {
        let scratch = ScratchDir::new("t", "name");
        let name = scratch.outer.file_name().unwrap().to_str().unwrap().to_owned();
        assert!(name.parse::<u32>().is_err(), "{name}");
    }
}
