//! Where the user guide is, for the tests that read it.

use std::path::{Path, PathBuf};

/// Resolves the guide under `publish/files/docs/guide` when `publish/` exists, otherwise under
/// `docs/guide`. Checking `publish/` rather than the guide itself ensures a missing guide fails
/// instead of silently selecting a different directory.
pub(crate) fn guide_dir(root: &Path) -> PathBuf {
    if std::fs::symlink_metadata(root.join("publish")).is_ok() {
        root.join("publish/files/docs/guide")
    } else {
        root.join("docs/guide")
    }
}

/// The guide directory of the tree this crate is built in.
pub(crate) fn this_trees_guide() -> PathBuf {
    guide_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join(".."))
}
