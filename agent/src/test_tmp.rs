// agent/src/test_tmp.rs
//! Test helper: a throwaway directory under the system temp dir that is removed when it goes out
//! of scope, so a test that fails an assertion (or returns early) leaves nothing behind. A
//! `remove_dir_all` written at the end of a test body does not run in those cases, and a shared
//! temp dir on a build host fills up with what they leave.

use std::ops::Deref;
use std::path::{Path, PathBuf};

/// Owns a directory and removes it, contents included, on drop.
#[derive(Debug)]
pub(crate) struct TmpDir(PathBuf);

impl TmpDir {
    /// A new directory `<temp dir>/<prefix>-<label>-<random>`. The random part is a UUID, so two
    /// test binaries running at once never share a name.
    pub(crate) fn new(prefix: &str, label: &str) -> Self {
        Self::in_dir(&std::env::temp_dir(), prefix, label)
    }

    /// The same under `parent`, which must exist.
    pub(crate) fn in_dir(parent: &Path, prefix: &str, label: &str) -> Self {
        let dir = parent.join(format!("{prefix}-{label}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&dir).unwrap_or_else(|e| panic!("create_dir {dir:?}: {e}"));
        TmpDir(dir)
    }
}

impl Deref for TmpDir {
    type Target = PathBuf;
    fn deref(&self) -> &PathBuf {
        &self.0
    }
}

impl AsRef<Path> for TmpDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        if std::fs::remove_dir_all(&self.0).is_ok() || !self.0.exists() {
            return;
        }
        // A test that took its own permissions away (a directory it made unreadable or read-only
        // to see the refusal) would otherwise keep the tree: give them back, then try again.
        open_up(&self.0);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Makes every directory under `path` (and `path`) traversable and writable by its owner, without
/// following symlinks.
fn open_up(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            open_up(&entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_directory_is_removed_on_drop_even_when_the_test_panics() {
        let seen = std::sync::Mutex::new(None);
        let result = std::panic::catch_unwind(|| {
            let dir = TmpDir::new("nv-tmpdir-guard", "panic");
            std::fs::write(dir.join("f"), b"x").unwrap();
            *seen.lock().unwrap() = Some(dir.to_path_buf());
            panic!("an assertion failed");
        });
        assert!(result.is_err());
        let path = seen.lock().unwrap().take().unwrap();
        assert!(!path.exists(), "{path:?} survived the panic");
    }

    #[test]
    fn a_tree_the_test_made_unreadable_is_still_removed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TmpDir::new("nv-tmpdir-guard", "locked");
        let path = dir.to_path_buf();
        let inner = dir.join("inner");
        std::fs::create_dir(&inner).unwrap();
        std::fs::write(inner.join("f"), b"x").unwrap();
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o000)).unwrap();
        drop(dir);
        assert!(!path.exists(), "{path:?} survived");
    }
}
