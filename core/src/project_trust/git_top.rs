//! Finding the git top level above a project root, the way git itself would, without running git,
//! following a link or reading a FIFO.
//!
//! git walks up from the working directory and stops at the first `.git` it accepts: a directory,
//! or a gitfile (`gitdir: <path>`) naming one, whose `HEAD` names a ref or an object id and whose
//! common directory has searchable `objects/` and `refs/`. A `.git` it would not accept does not end
//! its walk, and neither does one here. A `.git` this walk cannot check without following a link or
//! reading something other than a small regular file is treated the same way and reported: walking
//! further up only adds directories to ask about, while stopping too early could miss one.
//!
//! For the same reason, a walk that accepts nothing still remembers the highest directory whose
//! `.git` it could not accept ([`GitWalk::unverified_top`]): git may well accept that one (through
//! a link this walk does not follow, say), and then the CLI reads the local tier there, so
//! discovery must reach it rather than stop at the root.
//!
//! Only the fixed names `.git`, `HEAD` and `commondir` are looked at (and the directories a gitfile
//! or `commondir` names, opened as directories), each file read capped at 4 KiB, so each ancestor
//! costs a handful of system calls.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::nofollow::{normalize_lexically, DirHandle, FileKind, Refusal};
use super::Finding;

/// The most of `.git`, `HEAD` or `commondir` that is read; real ones are a line long.
const MAX_GIT_FILE_BYTES: u64 = 4096;

/// What the walk found: the top level, if any, and every `.git` on the way that it could not accept.
#[derive(Debug, Default)]
pub struct GitWalk {
    pub top: Option<PathBuf>,
    /// With no `top`: the highest directory walked whose `.git` (or the directory itself) was there
    /// but could not be accepted. git might accept it where this walk could not, so it is as far up
    /// as the git top level can be. Always `None` when `top` is found, since everything this walk
    /// could not accept lies below that.
    pub unverified_top: Option<PathBuf>,
    /// Only `Finding::GitUnverified`, with absolute paths.
    pub findings: Vec<Finding>,
    pub over_budget: Option<String>,
}

/// Walks `root` and its ancestors, nearest first, looking for a `.git` git would accept. Stops after
/// `stop_at` when it is passed, and after `max_walk_up` directories, which without a top level is
/// reported as over budget.
pub fn git_top_level(root: &Path, stop_at: Option<&Path>, max_walk_up: usize) -> GitWalk {
    let mut walk = GitWalk::default();
    for (seen, dir) in root.ancestors().enumerate() {
        if seen == max_walk_up {
            walk.over_budget = Some(format!(
                "more than {max_walk_up} directories above the root without a repository"
            ));
            break;
        }
        let reported = walk.findings.len();
        if accepts(dir, &mut walk.findings) {
            walk.top = Some(dir.to_path_buf());
            walk.unverified_top = None;
            return walk;
        }
        if walk.findings.len() > reported {
            walk.unverified_top = Some(dir.to_path_buf());
        }
        if stop_at == Some(dir) {
            break;
        }
    }
    walk
}

fn unverified(findings: &mut Vec<Finding>, path: PathBuf, reason: impl Into<String>, target: Option<PathBuf>) {
    findings.push(Finding::GitUnverified {
        path,
        reason: reason.into(),
        target,
    });
}

/// The reason for a `.git`, `HEAD` or `commondir` that is not a regular file or link.
fn not_read(kind: FileKind) -> String {
    format!("{}, not read", kind.describe())
}

/// The reason a refused open or read gives in a finding, and the link target if it was a link.
fn refused(refusal: Refusal) -> (String, Option<PathBuf>) {
    match refusal {
        Refusal::Symlink { target } => ("passes through a symlink, not followed".to_string(), Some(target)),
        Refusal::TooLarge(_) => (format!("over {MAX_GIT_FILE_BYTES} bytes, not read"), None),
        Refusal::NotRegular(what) => (format!("{what}, not read"), None),
        other => (format!("cannot be read: {}", other.describe()), None),
    }
}

/// Drops trailing CR and LF, as git does reading these one-line files.
fn trim_line_ends(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .rposition(|b| *b != b'\n' && *b != b'\r')
        .map_or(0, |i| i + 1);
    &bytes[..end]
}

/// Whether `dir/.git` is a repository git would accept. Everything it could not accept, or could not
/// check without following a link, is a finding.
fn accepts(dir: &Path, findings: &mut Vec<Finding>) -> bool {
    let dot_git_path = dir.join(".git");
    let handle = match DirHandle::open_absolute(dir) {
        Ok(handle) => handle,
        Err(refusal) => {
            let (reason, target) = refused(refusal);
            unverified(findings, dir.to_path_buf(), format!("the directory {reason}"), target);
            return false;
        }
    };
    let dot_git = OsStr::new(".git");
    let (git_dir_path, git_dir) = match handle.lstat(dot_git) {
        Err(Refusal::Missing) => return false,
        Err(refusal) => {
            let (reason, target) = refused(refusal);
            unverified(findings, dot_git_path, reason, target);
            return false;
        }
        Ok(FileKind::Symlink) => {
            let target = handle.readlink(dot_git).ok();
            unverified(findings, dot_git_path, "a symlink, not followed", target);
            return false;
        }
        Ok(FileKind::Dir) => match handle.open_dir(dot_git) {
            Ok(git_dir) => (dot_git_path.clone(), git_dir),
            Err(refusal) => {
                let (reason, target) = refused(refusal);
                unverified(findings, dot_git_path, reason, target);
                return false;
            }
        },
        Ok(FileKind::Regular { .. }) => {
            let bytes = match handle.read_file(dot_git, MAX_GIT_FILE_BYTES) {
                Ok(bytes) => bytes,
                Err(refusal) => {
                    let (reason, target) = refused(refusal);
                    unverified(findings, dot_git_path, reason, target);
                    return false;
                }
            };
            let Some(named) = bytes
                .strip_prefix(b"gitdir: ")
                .map(trim_line_ends)
                .filter(|n| !n.is_empty())
            else {
                unverified(findings, dot_git_path, "a file that is not a gitfile", None);
                return false;
            };
            let named = Path::new(OsStr::from_bytes(named));
            let Some(git_dir_path) = normalize_lexically(dir, named) else {
                unverified(
                    findings,
                    dot_git_path,
                    "its gitdir has a `..` after a name, not resolved",
                    Some(named.to_path_buf()),
                );
                return false;
            };
            match DirHandle::open_absolute(&git_dir_path) {
                Ok(git_dir) => (git_dir_path, git_dir),
                Err(refusal) => {
                    let (reason, _) = refused(refusal);
                    unverified(
                        findings,
                        dot_git_path,
                        format!("its gitdir {reason}"),
                        Some(named.to_path_buf()),
                    );
                    return false;
                }
            }
        }
        Ok(kind) => {
            unverified(findings, dot_git_path, not_read(kind), None);
            return false;
        }
    };
    if !head_names_a_ref_or_an_object(&git_dir, &git_dir_path, findings) {
        return false;
    }
    let Some(common) = common_dir(git_dir, &git_dir_path, findings) else {
        return false;
    };
    if common.searchable(OsStr::new("objects")) && common.searchable(OsStr::new("refs")) {
        true
    } else {
        unverified(
            findings,
            git_dir_path,
            "no searchable objects/ and refs/ directories",
            None,
        );
        false
    }
}

/// `HEAD` is a link into `refs/` (read as a link, never opened), or a small regular file holding
/// `ref: refs/...` or a full object id.
fn head_names_a_ref_or_an_object(git_dir: &DirHandle, git_dir_path: &Path, findings: &mut Vec<Finding>) -> bool {
    let head = OsStr::new("HEAD");
    let head_path = git_dir_path.join("HEAD");
    match git_dir.lstat(head) {
        Ok(FileKind::Symlink) => match git_dir.readlink(head) {
            Ok(target) if target.as_os_str().as_bytes().starts_with(b"refs/") => true,
            Ok(target) => {
                unverified(
                    findings,
                    head_path,
                    "a symlink outside refs/, not followed",
                    Some(target),
                );
                false
            }
            Err(refusal) => {
                let (reason, target) = refused(refusal);
                unverified(findings, head_path, reason, target);
                false
            }
        },
        Ok(FileKind::Regular { .. }) => {
            let bytes = match git_dir.read_file(head, MAX_GIT_FILE_BYTES) {
                Ok(bytes) => bytes,
                Err(refusal) => {
                    let (reason, target) = refused(refusal);
                    unverified(findings, head_path, reason, target);
                    return false;
                }
            };
            let line = trim_line_ends(&bytes);
            let good = if let Some(rest) = line.strip_prefix(b"ref:") {
                let start = rest
                    .iter()
                    .position(|b| *b != b' ' && *b != b'\t')
                    .unwrap_or(rest.len());
                rest[start..].starts_with(b"refs/")
            } else {
                matches!(line.len(), 40 | 64) && line.iter().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
            };
            if !good {
                unverified(findings, head_path, "names neither a ref nor an object id", None);
            }
            good
        }
        Ok(kind) => {
            unverified(findings, head_path, not_read(kind), None);
            false
        }
        Err(Refusal::Missing) => {
            unverified(findings, head_path, "missing", None);
            false
        }
        Err(refusal) => {
            let (reason, target) = refused(refusal);
            unverified(findings, head_path, reason, target);
            false
        }
    }
}

/// The common directory: the git directory itself without a `commondir`, else the directory that
/// file names (relative to the git directory), opened component by component.
fn common_dir(git_dir: DirHandle, git_dir_path: &Path, findings: &mut Vec<Finding>) -> Option<DirHandle> {
    let commondir = OsStr::new("commondir");
    let commondir_path = git_dir_path.join("commondir");
    match git_dir.lstat(commondir) {
        Err(Refusal::Missing) => return Some(git_dir),
        Err(refusal) => {
            let (reason, target) = refused(refusal);
            unverified(findings, commondir_path, reason, target);
            return None;
        }
        Ok(FileKind::Symlink) => {
            let target = git_dir.readlink(commondir).ok();
            unverified(findings, commondir_path, "a symlink, not followed", target);
            return None;
        }
        Ok(FileKind::Regular { .. }) => {}
        Ok(kind) => {
            unverified(findings, commondir_path, not_read(kind), None);
            return None;
        }
    }
    let bytes = match git_dir.read_file(commondir, MAX_GIT_FILE_BYTES) {
        Ok(bytes) => bytes,
        Err(refusal) => {
            let (reason, target) = refused(refusal);
            unverified(findings, commondir_path, reason, target);
            return None;
        }
    };
    let named = trim_line_ends(&bytes);
    if named.is_empty() {
        unverified(findings, commondir_path, "empty", None);
        return None;
    }
    let named = Path::new(OsStr::from_bytes(named));
    let Some(common_path) = normalize_lexically(git_dir_path, named) else {
        unverified(
            findings,
            commondir_path,
            "names a `..` after a name, not resolved",
            Some(named.to_path_buf()),
        );
        return None;
    };
    match DirHandle::open_absolute(&common_path) {
        Ok(common) => Some(common),
        Err(refusal) => {
            let (reason, _) = refused(refusal);
            unverified(
                findings,
                commondir_path,
                format!("names a directory that {reason}"),
                Some(named.to_path_buf()),
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_trust::nofollow::tests::{canonical_scratch, mkfifo};
    use std::os::unix::fs::symlink;

    /// A minimal git directory by hand, the layout git's own check asks for.
    fn fake_repo(dir: &Path) {
        std::fs::create_dir_all(dir.join(".git/objects")).unwrap();
        std::fs::create_dir_all(dir.join(".git/refs/heads")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
    }

    fn reasons(walk: &GitWalk) -> Vec<String> {
        walk.findings
            .iter()
            .map(|f| match f {
                Finding::GitUnverified { reason, .. } => reason.clone(),
                other => panic!("not a git finding: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn a_hand_made_repository_is_the_top_level() {
        let (_s, dir) = canonical_scratch("gt-plain");
        fake_repo(&dir.join("repo"));
        std::fs::create_dir_all(dir.join("repo/src/deep")).unwrap();
        let walk = git_top_level(&dir.join("repo/src/deep"), Some(&dir), 64);
        assert_eq!(walk.top.as_deref(), Some(dir.join("repo").as_path()));
        assert!(walk.findings.is_empty(), "{:?}", walk.findings);
    }

    #[test]
    fn an_object_id_head_is_accepted_and_garbage_is_not() {
        let (_s, dir) = canonical_scratch("gt-head");
        fake_repo(&dir.join("repo"));
        std::fs::write(dir.join("repo/.git/HEAD"), "a".repeat(40)).unwrap();
        assert!(git_top_level(&dir.join("repo"), Some(&dir), 64).top.is_some());
        std::fs::write(dir.join("repo/.git/HEAD"), b"garbage").unwrap();
        let walk = git_top_level(&dir.join("repo"), Some(&dir), 64);
        assert_eq!(walk.top, None);
        assert_eq!(reasons(&walk), vec!["names neither a ref nor an object id"]);
    }

    #[test]
    fn a_missing_objects_directory_is_not_a_repository() {
        let (_s, dir) = canonical_scratch("gt-objects");
        fake_repo(&dir.join("repo"));
        std::fs::remove_dir(dir.join("repo/.git/objects")).unwrap();
        let walk = git_top_level(&dir.join("repo"), Some(&dir), 64);
        assert_eq!(walk.top, None);
        assert_eq!(walk.findings.len(), 1);
    }

    #[test]
    fn a_dot_git_link_is_reported_with_its_target() {
        let (_s, dir) = canonical_scratch("gt-link");
        fake_repo(&dir.join("real"));
        std::fs::create_dir(dir.join("repo")).unwrap();
        symlink(dir.join("real/.git"), dir.join("repo/.git")).unwrap();
        let walk = git_top_level(&dir.join("repo"), Some(&dir), 64);
        assert_eq!(walk.top, None);
        match &walk.findings[..] {
            [Finding::GitUnverified { path, reason, target }] => {
                assert_eq!(path, &dir.join("repo/.git"));
                assert_eq!(reason, "a symlink, not followed");
                assert_eq!(target.as_deref(), Some(dir.join("real/.git").as_path()));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_dot_git_it_cannot_check_is_as_far_up_as_the_top_can_be() {
        let (_s, dir) = canonical_scratch("gt-unverified");
        fake_repo(&dir.join("real"));
        std::fs::create_dir_all(dir.join("repo/a/b/.git")).unwrap();
        symlink(dir.join("real/.git"), dir.join("repo/.git")).unwrap();
        let walk = git_top_level(&dir.join("repo/a/b"), Some(&dir), 64);
        assert_eq!(walk.top, None);
        // The empty `.git` in `b` and the link in `repo`: the higher one bounds the walk.
        assert_eq!(walk.unverified_top.as_deref(), Some(dir.join("repo").as_path()));
        // A repository found clears it: everything unchecked lies below the top.
        fake_repo(&dir.join("repo/a"));
        let walk = git_top_level(&dir.join("repo/a/b"), Some(&dir), 64);
        assert_eq!(walk.top.as_deref(), Some(dir.join("repo/a").as_path()));
        assert_eq!(walk.unverified_top, None);
        // A missing `.git` is not a candidate.
        std::fs::create_dir_all(dir.join("plain/x")).unwrap();
        assert_eq!(git_top_level(&dir.join("plain/x"), Some(&dir), 64).unverified_top, None);
    }

    #[test]
    fn a_fifo_at_dot_git_is_not_read() {
        let (_s, dir) = canonical_scratch("gt-fifo");
        std::fs::create_dir(dir.join("repo")).unwrap();
        mkfifo(&dir.join("repo/.git"));
        let walk = git_top_level(&dir.join("repo"), Some(&dir), 64);
        assert_eq!(walk.top, None);
        assert_eq!(reasons(&walk), vec!["a FIFO, not read"]);
    }

    #[test]
    fn a_gitfile_with_a_dotdot_after_a_name_is_refused() {
        let (_s, dir) = canonical_scratch("gt-dotdot");
        fake_repo(&dir.join("real"));
        std::fs::create_dir(dir.join("repo")).unwrap();
        std::fs::write(dir.join("repo/.git"), b"gitdir: x/../../real/.git\n").unwrap();
        let walk = git_top_level(&dir.join("repo"), Some(&dir), 64);
        assert_eq!(walk.top, None);
        assert_eq!(reasons(&walk), vec!["its gitdir has a `..` after a name, not resolved"]);
        // A leading `..` is folded over the directory already opened.
        std::fs::write(dir.join("repo/.git"), b"gitdir: ../real/.git\n").unwrap();
        let walk = git_top_level(&dir.join("repo"), Some(&dir), 64);
        assert_eq!(walk.top.as_deref(), Some(dir.join("repo").as_path()));
    }

    #[test]
    fn the_walk_stops_at_stop_at_and_at_its_budget() {
        let (_s, dir) = canonical_scratch("gt-budget");
        fake_repo(&dir);
        let mut deep = dir.join("home");
        for i in 0..5 {
            deep.push(format!("d{i}"));
        }
        std::fs::create_dir_all(&deep).unwrap();
        // The repository is above `stop_at`, so it is never seen.
        let walk = git_top_level(&deep, Some(&dir.join("home")), 64);
        assert_eq!(walk.top, None);
        assert_eq!(walk.over_budget, None);
        // Six directories from `deep` up to `home`; a budget of three runs out.
        let walk = git_top_level(&deep, Some(&dir.join("home")), 3);
        assert_eq!(walk.top, None);
        assert_eq!(
            walk.over_budget.as_deref(),
            Some("more than 3 directories above the root without a repository")
        );
        // Without a stop, the repository above is the top level.
        assert_eq!(git_top_level(&deep, None, 64).top.as_deref(), Some(dir.as_path()));
    }
}
