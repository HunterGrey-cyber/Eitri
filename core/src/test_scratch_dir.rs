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
pub(crate) struct ScratchDir(PathBuf);

impl ScratchDir {
    /// Creates `<std::env::temp_dir()>/<prefix>-<label>-<uuid>` and returns a guard that removes it
    /// on drop.
    pub(crate) fn new(prefix: &str, label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("{prefix}-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        ScratchDir(dir)
    }
}

impl Deref for ScratchDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for ScratchDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
