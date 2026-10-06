//! A `#[cfg(test)]`-only RAII guard shared by this crate's tests that each want their own throwaway
//! directory under the system temp dir. Its `Drop` removes the directory, so a test panicking on an
//! assertion still cleans up (a bare `remove_dir_all` written at the end of the test body would not
//! run in that case) and so a crate-wide test run never leaves anything behind in `/tmp`.
//!
//! Grown out of three call sites (`agent_prefs`, `permission_store`, `prompt_history`) that each had
//! their own copy of a `scratch(label) -> PathBuf` helper which created a
//! `<prefix>-<label>-<uuid>` directory and never removed it -- 194/510/850 of them respectively were
//! found still sitting in `/tmp` before this existed. Once three files wanted the same guard, this
//! module replaced the three copies rather than adding a fourth.

#![cfg(test)]

use std::ops::Deref;
use std::path::{Path, PathBuf};

/// Owns a directory under the system temp dir and removes it (recursively) on drop.
///
/// The path it hands out is `state` inside that directory, not the directory itself: the state
/// writers judge the owner of the directory above theirs (`<state home>/eitri` in a real run), and
/// the temp dir itself belongs to root, so the scratch directory stands in for `eitri`.
pub(crate) struct ScratchDir {
    outer: PathBuf,
    dir: PathBuf,
}

impl ScratchDir {
    /// Creates `<std::env::temp_dir()>/<prefix>-<label>-<uuid>/state` and returns a guard that
    /// removes the whole `<prefix>-<label>-<uuid>` directory on drop.
    pub(crate) fn new(prefix: &str, label: &str) -> Self {
        // macOS's `TMPDIR` is already ~49 bytes and a socket path may not pass 103, so there the
        // directory is named by a short random id alone; the long form is kept where paths are roomy.
        #[cfg(target_os = "macos")]
        let outer = std::env::temp_dir().join(&uuid::Uuid::new_v4().simple().to_string()[..12]);
        #[cfg(not(target_os = "macos"))]
        let outer = std::env::temp_dir().join(format!("{prefix}-{label}-{}", uuid::Uuid::new_v4()));
        #[cfg(target_os = "macos")]
        let _ = (prefix, label);
        let dir = outer.join("state");
        std::fs::create_dir_all(&dir).unwrap();
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
        let _ = std::fs::remove_dir_all(&self.outer);
    }
}
