//! Replacing a private file named by its path whole: write a temporary beside it, then rename it over the name.

use std::io::Write;
use std::path::Path;

/// How much of a [`replace_private`] reaches the disk before it returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flush {
    /// Nothing is flushed: a process killed mid-write leaves the previous file, but after a power loss the name
    /// may hold an empty file.
    Nothing,
    /// The temporary's bytes are flushed before the rename, so after a power loss the name holds the previous file
    /// or the new one whole; the rename itself may be lost.
    File,
    /// [`Flush::File`], and the directory after the rename, so the new name survives a power loss too.
    FileAndDirectory,
}

/// Writes `bytes` to `tmp` as a 0600 file (see [`super::open_private`]: a symlink at `tmp` is refused, never
/// written through) and renames it over `path`, so a reader sees the previous file or the new one, never half of
/// one. A symlink at `path` is replaced by the file, never written through.
///
/// Any failure up to and including the rename removes `tmp` and returns the error; `path` is then untouched. With
/// [`Flush::FileAndDirectory`] an error flushing the directory is returned after the rename has happened: `path`
/// holds the new file, but the caller is not told it is safe on disk.
pub fn replace_private(tmp: &Path, path: &Path, bytes: &[u8], flush: Flush) -> std::io::Result<()> {
    let renamed = super::open_private(tmp)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            if flush != Flush::Nothing {
                sync(&file)?;
            }
            Ok(())
        })
        .and_then(|()| {
            #[cfg(any(test, feature = "test-support"))]
            test_hooks::run_before_rename();
            std::fs::rename(tmp, path)
        });
    if let Err(error) = renamed {
        let _ = std::fs::remove_file(tmp);
        return Err(error);
    }
    if flush == Flush::FileAndDirectory {
        let dir = path.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "a state file path names no directory")
        })?;
        sync(&std::fs::File::open(dir)?)?;
    }
    Ok(())
}

fn sync(file: &std::fs::File) -> std::io::Result<()> {
    #[cfg(any(test, feature = "test-support"))]
    test_hooks::count_sync();
    file.sync_all()
}

/// Seams for tests in this crate and the crates built on it, on the calling thread only.
#[cfg(any(test, feature = "test-support"))]
pub mod test_hooks {
    use std::cell::{Cell, RefCell};

    thread_local! {
        static SYNCS: Cell<usize> = const { Cell::new(0) };
        static BEFORE_RENAME: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    }

    /// How many files and directories [`super::replace_private`] has flushed to disk on this thread.
    pub fn syncs() -> usize {
        SYNCS.with(Cell::get)
    }

    /// Runs `hook` once, inside the next [`super::replace_private`] on this thread that gets as far as the rename,
    /// after the temporary is written and before the rename. A replace that fails earlier leaves it armed.
    pub fn before_next_rename(hook: impl FnOnce() + 'static) {
        BEFORE_RENAME.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    }

    pub(super) fn count_sync() {
        SYNCS.with(|syncs| syncs.set(syncs.get() + 1));
    }

    pub(super) fn run_before_rename() {
        if let Some(hook) = BEFORE_RENAME.with(|slot| slot.borrow_mut().take()) {
            hook();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_tmp::TmpDir;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn scratch(label: &str) -> TmpDir {
        TmpDir::new("nv-replace-private", label)
    }

    const ALL: [Flush; 3] = [Flush::Nothing, Flush::File, Flush::FileAndDirectory];

    #[test]
    fn the_file_is_replaced_whole_as_0600_and_the_temporary_goes() {
        for flush in ALL {
            let dir = scratch("replaced");
            let path = dir.join("state.json");
            let tmp = dir.join("state.json.tmp");
            std::fs::write(&path, "old").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            replace_private(&tmp, &path, b"new", flush).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), b"new", "{flush:?}");
            assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600, "{flush:?}");
            assert!(tmp.symlink_metadata().is_err(), "{flush:?}");
        }
    }

    #[test]
    fn a_temporary_a_killed_writer_left_with_a_wide_mode_is_narrowed_and_overwritten() {
        let dir = scratch("stale-tmp");
        let path = dir.join("state.json");
        let tmp = dir.join("state.json.tmp");
        std::fs::write(&tmp, "a much longer half-written body").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o666)).unwrap();
        replace_private(&tmp, &path, b"new", Flush::Nothing).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn a_failed_rename_leaves_no_temporary_and_the_old_file_in_place() {
        for flush in ALL {
            let dir = scratch("rename-fails");
            // A directory with an entry in it: renaming a file over it fails on every Unix.
            let path = dir.join("state.json");
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("kept"), "kept").unwrap();
            let tmp = dir.join("state.json.tmp");
            assert!(replace_private(&tmp, &path, b"new", flush).is_err(), "{flush:?}");
            assert!(
                tmp.symlink_metadata().is_err(),
                "the temporary stays behind ({flush:?})"
            );
            assert_eq!(std::fs::read(path.join("kept")).unwrap(), b"kept", "{flush:?}");
        }
    }

    #[test]
    fn a_rename_into_a_missing_directory_is_an_error_and_leaves_no_temporary() {
        let dir = scratch("missing-dir");
        let tmp = dir.join("state.json.tmp");
        let error = replace_private(&tmp, &dir.join("gone/state.json"), b"new", Flush::FileAndDirectory).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(tmp.symlink_metadata().is_err());
    }

    #[test]
    fn a_symlink_at_the_temporary_is_refused_and_its_target_untouched() {
        let dir = scratch("tmp-link");
        let outside = dir.join("outside");
        std::fs::write(&outside, "outside").unwrap();
        let path = dir.join("state.json");
        std::fs::write(&path, "old").unwrap();
        let tmp = dir.join("state.json.tmp");
        std::os::unix::fs::symlink(&outside, &tmp).unwrap();
        assert!(replace_private(&tmp, &path, b"new", Flush::File).is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        assert!(
            tmp.symlink_metadata().is_err(),
            "the planted link is removed with the temporary"
        );
    }

    #[test]
    fn a_symlink_at_the_target_is_replaced_not_written_through() {
        let dir = scratch("target-link");
        let outside = dir.join("outside");
        std::fs::write(&outside, "outside").unwrap();
        let path = dir.join("state.json");
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        replace_private(&dir.join("state.json.tmp"), &path, b"new", Flush::Nothing).unwrap();
        assert!(path.symlink_metadata().unwrap().file_type().is_file());
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
    }

    #[test]
    fn each_flush_level_syncs_what_it_names() {
        for (flush, expected) in [(Flush::Nothing, 0), (Flush::File, 1), (Flush::FileAndDirectory, 2)] {
            let dir = scratch("syncs");
            let before = test_hooks::syncs();
            replace_private(&dir.join("s.tmp"), &dir.join("s"), b"x", flush).unwrap();
            assert_eq!(test_hooks::syncs() - before, expected, "{flush:?}");
        }
    }

    #[test]
    fn the_hook_runs_once_between_the_write_and_the_rename() {
        let dir = scratch("hook");
        let path = dir.join("state.json");
        let tmp = dir.join("state.json.tmp");
        let seen = std::rc::Rc::new(std::cell::Cell::new(None));
        let (seen_in, tmp_in, path_in) = (seen.clone(), tmp.clone(), path.clone());
        test_hooks::before_next_rename(move || {
            seen_in.set(Some((std::fs::read(&tmp_in).unwrap(), path_in.exists())));
        });
        replace_private(&tmp, &path, b"new", Flush::Nothing).unwrap();
        assert_eq!(seen.take(), Some((b"new".to_vec(), false)));
        replace_private(&tmp, &path, b"again", Flush::Nothing).unwrap();
        assert_eq!(seen.take(), None, "the hook is spent after one replace");
    }
}
