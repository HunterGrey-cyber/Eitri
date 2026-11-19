//! Re-creating the tree session (an ignore change, a tripwire exclusion, a host-key resume, a lost session) without
//! losing anything silently.
//!
//! A new session has no ancestor: a file that differs on the two sides becomes a conflict, and a file on one side only
//! is copied to the other, so a delete made on one side while no session ran comes back. So after terminating the old
//! session and before creating the new one, both trees are listed with the new ignores; after the new session's first
//! cycle, every file that was on one side only and is now on both is recorded in `SyncRecord.copied_back`, which
//! `:EitriSync` shows until the person dismisses it. Paths the new ignores leave out while the laptop still holds them
//! are recorded in `kept_here`.

use std::collections::BTreeSet;
use std::io;
use std::path::Path;

use super::ignores::Matcher;
use super::sessions::SyncHelper;

/// The most paths `copied_back` and `kept_here` keep; past it, `copied_back_more` counts the rest.
pub const RECORDED_PATHS: usize = 2000;

/// The most bytes of paths a record list keeps, so the project record stays far below its 4 MiB read cap.
pub const RECORDED_BYTES: usize = 256 * 1024;

/// How many of `paths` fit within [`RECORDED_PATHS`] and [`RECORDED_BYTES`].
pub fn bounded_count(paths: &[String]) -> usize {
    let mut bytes = 0;
    paths
        .iter()
        .take(RECORDED_PATHS)
        .take_while(|path| {
            bytes += path.len() + 4;
            bytes <= RECORDED_BYTES
        })
        .count()
}

/// How many entries one side's listing may hold before re-creation refuses to go on.
pub const LIST_BUDGET: u64 = 1_000_000;

/// The band and `:EitriSync` line when a listing is cut short.
pub const TOO_MANY_FILES: &str = "too many files to re-create sync safely; see :EitriSync";

#[derive(Debug, PartialEq, Eq)]
pub enum ListError {
    Truncated,
    Failed(String),
}

/// One side's files (anything but a directory), and the laptop paths newly left out by the ignores.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Listing {
    pub files: BTreeSet<String>,
    pub kept_here: Vec<String>,
    /// Newly ignored paths beyond the [`RECORDED_PATHS`] `kept_here` holds.
    pub kept_here_more: u64,
}

/// Walks the laptop's copy at `root` without following links. What both `old` and `new` ignore is not entered;
/// what only `new` ignores is recorded (top-most) as kept here; the rest is listed against `new`.
pub fn list_laptop(root: &Path, old: &Matcher, new: &Matcher, budget: u64) -> Result<Listing, ListError> {
    let mut listing = Listing::default();
    let mut seen = 0u64;
    walk(root, "", old, new, budget, &mut seen, &mut listing)?;
    Ok(listing)
}

fn walk(
    root: &Path,
    rel: &str,
    old: &Matcher,
    new: &Matcher,
    budget: u64,
    seen: &mut u64,
    listing: &mut Listing,
) -> Result<(), ListError> {
    let dir = if rel.is_empty() {
        root.to_owned()
    } else {
        root.join(rel)
    };
    let entries = std::fs::read_dir(&dir).map_err(|error| ListError::Failed(format!("{}: {error}", dir.display())))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| ListError::Failed(format!("{}: {error}", dir.display())))?;
        *seen += 1;
        if *seen > budget {
            return Err(ListError::Truncated);
        }
        names.push(entry.file_name());
    }
    names.sort();
    for name in names {
        // Names the sync cannot carry are Mutagen's scan problems, shown as such.
        let Some(name) = name.to_str() else { continue };
        if name == ".git" || name.starts_with(".mutagen-temporary-") {
            continue;
        }
        let child = if rel.is_empty() {
            name.to_owned()
        } else {
            format!("{rel}/{name}")
        };
        let meta = std::fs::symlink_metadata(root.join(&child))
            .map_err(|error| ListError::Failed(format!("{child}: {error}")))?;
        let is_dir = meta.is_dir();
        let ignored_now = new.ignored(&child, is_dir);
        if ignored_now {
            if !old.ignored(&child, is_dir) {
                if listing.kept_here.len() < RECORDED_PATHS {
                    listing.kept_here.push(child);
                } else {
                    listing.kept_here_more += 1;
                }
            }
            continue;
        }
        if is_dir {
            walk(root, &child, old, new, budget, seen, listing)?;
        } else {
            listing.files.insert(child);
        }
    }
    Ok(())
}

/// The server's files under the new ignores, through the helper's bounded ListTree.
pub fn list_server(
    helper: &dyn SyncHelper,
    root: &Path,
    patterns: &[String],
    budget: u64,
) -> Result<BTreeSet<String>, ListError> {
    let entries = helper.list_tree(root, patterns, budget).map_err(ListError::Failed)?;
    let mut files = BTreeSet::new();
    for entry in entries {
        if !entry.refusal.is_empty() {
            return Err(ListError::Failed(entry.refusal));
        }
        if entry.truncated {
            return Err(ListError::Truncated);
        }
        if entry.kind != "directory" && entry.kind != "not-utf8" {
            files.insert(entry.rel);
        }
    }
    Ok(files)
}

/// The files on one side only: `(laptop only, server only)`.
pub fn one_sided(laptop: &BTreeSet<String>, server: &BTreeSet<String>) -> (Vec<String>, Vec<String>) {
    (
        laptop.difference(server).cloned().collect(),
        server.difference(laptop).cloned().collect(),
    )
}

/// Which of the one-sided files are now on both sides, after the new session's first cycle: `after_laptop` and
/// `after_server` are the two listings taken then.
pub fn copied_back(
    before: &(Vec<String>, Vec<String>),
    after_laptop: &BTreeSet<String>,
    after_server: &BTreeSet<String>,
) -> Vec<String> {
    let mut copied: Vec<String> = before
        .0
        .iter()
        .chain(&before.1)
        .filter(|path| after_laptop.contains(*path) && after_server.contains(*path))
        .cloned()
        .collect();
    copied.sort();
    copied
}

/// Lists the laptop's files under the new ignores only (after the first cycle).
pub fn laptop_files(root: &Path, patterns: &Matcher, budget: u64) -> Result<BTreeSet<String>, ListError> {
    list_laptop(root, patterns, patterns, budget).map(|listing| listing.files)
}

/// The patterns a tree session was created with, kept in the sync directory (`ignores.json`, 0600) so a later
/// re-creation can tell which paths the new ignores leave out. Missing or unreadable: unknown (`None`).
pub fn read_created_patterns(sync_dir: &Path) -> Option<Vec<String>> {
    let bytes =
        agent::private_fs::read_private_nofollow_bytes_capped(&sync_dir.join("ignores.json"), 64 * 1024 * 1024).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn write_created_patterns(sync_dir: &Path, patterns: &[String]) -> io::Result<()> {
    use std::io::Write;
    #[cfg(test)]
    if test_failures::UNWRITABLE.lock().unwrap().contains(sync_dir) {
        return Err(io::Error::other("the disk is full"));
    }
    let bytes = serde_json::to_vec(patterns).map_err(io::Error::other)?;
    let temporary = sync_dir.join(format!(".ignores-{}.tmp", uuid::Uuid::new_v4()));
    let written = (|| {
        let mut file = agent::private_fs::open_private(&temporary)?;
        file.write_all(&bytes)?;
        drop(file);
        std::fs::rename(&temporary, sync_dir.join("ignores.json"))
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

/// Tests make the ignore list's write fail for one sync directory while the old list stays readable.
#[cfg(test)]
pub(crate) mod test_failures {
    pub(crate) static UNWRITABLE: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>> =
        std::sync::LazyLock::new(Default::default);
}

/// A re-creation in progress, kept in the sync directory (`recreate.json`, 0600) so a holder that lets go (window
/// closed, watch handing over, a crash) leaves it to the next one: `why` it started, and, before creating the replacement, the files
/// then on one side only. The first cycle's accounting needs that listing even if the creator exits.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Progress {
    pub why: String,
    #[serde(default)]
    pub one_sided: Option<(Vec<String>, Vec<String>)>,
}

pub fn read_progress(sync_dir: &Path) -> Option<Progress> {
    let bytes =
        agent::private_fs::read_private_nofollow_bytes_capped(&sync_dir.join("recreate.json"), 256 * 1024 * 1024)
            .ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn write_progress(sync_dir: &Path, progress: &Progress) -> io::Result<()> {
    use std::io::Write;
    let bytes = serde_json::to_vec(progress).map_err(io::Error::other)?;
    let temporary = sync_dir.join(format!(".recreate-{}.tmp", uuid::Uuid::new_v4()));
    let written = (|| {
        let mut file = agent::private_fs::open_private(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, sync_dir.join("recreate.json"))
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

pub fn clear_progress(sync_dir: &Path) {
    let _ = std::fs::remove_file(sync_dir.join("recreate.json"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|p| (*p).to_owned()).collect()
    }

    #[test]
    fn laptop_walk_lists_files_prunes_ignored_and_records_newly_ignored() {
        let root = agent::state_dirs::test_workspace_dir("recreate-walk");
        for (rel, text) in [
            ("src/main.rs", "x"),
            ("target/debug/a", "x"),
            ("dist/app.js", "x"),
            (".git/HEAD", "x"),
            ("notes.txt", "x"),
        ] {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        std::os::unix::fs::symlink("/etc/passwd", root.join("link")).unwrap();
        let old = Matcher::new(&["/target/".into()]);
        let new = Matcher::new(&["/target/".into(), "/dist/".into()]);
        let listing = list_laptop(&root, &old, &new, 100).unwrap();
        assert_eq!(listing.files, set(&["link", "notes.txt", "src/main.rs"]));
        assert_eq!(listing.kept_here, vec!["dist".to_owned()]);
        assert_eq!(list_laptop(&root, &old, &new, 3), Err(ListError::Truncated));
    }

    #[test]
    fn copied_back_is_what_was_one_sided_and_is_now_on_both() {
        let laptop = set(&["a", "only-laptop", "both"]);
        let server = set(&["a", "only-server", "both"]);
        let before = one_sided(&laptop, &server);
        assert_eq!(before, (vec!["only-laptop".into()], vec!["only-server".into()]));
        let after_laptop = set(&["a", "only-laptop", "only-server", "both"]);
        let after_server = set(&["a", "only-server", "both"]);
        assert_eq!(
            copied_back(&before, &after_laptop, &after_server),
            vec!["only-server".to_owned()]
        );
    }

    #[test]
    fn progress_round_trips_and_clears() {
        let dir = agent::state_dirs::test_workspace_dir("recreate-progress");
        assert_eq!(read_progress(&dir), None);
        let progress = Progress {
            why: "lost".into(),
            one_sided: Some((vec!["a".into()], vec![])),
        };
        write_progress(&dir, &progress).unwrap();
        assert_eq!(read_progress(&dir), Some(progress));
        clear_progress(&dir);
        assert_eq!(read_progress(&dir), None);
    }

    #[test]
    fn created_patterns_round_trip_privately() {
        let dir = agent::state_dirs::test_workspace_dir("recreate-patterns");
        assert_eq!(read_created_patterns(&dir), None);
        write_created_patterns(&dir, &["/target/".into(), "a,\"b\"".into()]).unwrap();
        assert_eq!(
            read_created_patterns(&dir),
            Some(vec!["/target/".into(), "a,\"b\"".into()])
        );
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(dir.join("ignores.json")).unwrap().mode() & 0o777,
            0o600
        );
    }
}
