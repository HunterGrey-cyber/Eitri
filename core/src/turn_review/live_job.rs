//! Worker jobs for live drawings and their snapshot-backed settle.

use std::path::{Path, PathBuf};
use std::time::Instant;

use super::diff::{blob_hunks, Side};
use super::git::{self, GitError};
use super::lifecycle::{
    check_unambiguous, resolve_compare, ReviewError, Scope, ShadowSpec, TurnRecord, TurnRef, TurnState,
    MAX_OVERVIEW_DIFF_LINES,
};
use super::revert::{exact_hunks, lines_of, ExactHunk, ExactHunks, ExactHunksJob};
use super::shadow::{remaining, EntryKind, Shadow, ShadowError, READ_TIMEOUT};
use super::write::{self, Current, ProjectDir, WriteError};

const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// The in-memory facts a live drawing needs about one turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveRecord {
    pub tab: u64,
    /// Zero until the worker has learned the session's numbering offset.
    pub n: u32,
    pub started_ns: u64,
    pub ended_ns: Option<u64>,
    pub ended: bool,
    pub state: TurnState,
    pub late: bool,
}

/// One file compared with the turn's base and read from disk on a worker.
pub struct LiveHunksJob {
    pub(crate) spec: ShadowSpec,
    pub(crate) record: Option<TurnRecord>,
    pub(crate) path: PathBuf,
}

/// A file's live hunks and the number of lines that the disk read actually held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveHunks {
    pub path: String,
    pub hunks: Vec<ExactHunk>,
    /// Zero for an empty file, which nvim holds as one empty line: only that line may stand for it.
    pub read_lines: u64,
}

/// A file that cannot honestly be drawn as the running turn's change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveSkip {
    NoBase,
    Late,
    Ignored,
    LeftOut,
    Ambiguous,
    TooLarge,
    Binary,
    OverCap,
    NotAFile,
    Unchanged,
}

/// Exact turn hunks together with the end snapshot's buffer line count.
pub struct LiveSettleJob {
    pub(crate) exact: ExactHunksJob,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSettle {
    pub exact: ExactHunks,
    /// Zero for a file empty or gone at the end, as [`LiveHunks::read_lines`].
    pub end_lines: u64,
}

impl LiveHunksJob {
    pub fn run(self) -> Result<Result<LiveHunks, LiveSkip>, ReviewError> {
        let Some(record) = self.record else {
            return Ok(Err(LiveSkip::NoBase));
        };
        let Some(commit) = record.base.commit() else {
            return Ok(Err(LiveSkip::NoBase));
        };
        if record.late {
            return Ok(Err(LiveSkip::Late));
        }
        if record.left_out.base.contains_key(&self.path) {
            return Ok(Err(LiveSkip::LeftOut));
        }
        let shadow = self.spec.open().map_err(ReviewError::Unavailable)?;
        let deadline = Instant::now() + READ_TIMEOUT;
        // Keep unreferenced blobs alive between storing them and reading their diff, even when
        // another window repacks the store and then prunes loose objects.
        let _lock = shadow.lock_shared_until(deadline).map_err(unavailable)?;
        let project = ProjectDir::open(&self.spec.root).map_err(|e| ReviewError::Unavailable(e.to_string()))?;
        let target = match project.target(&self.path, false) {
            Ok(target) => target,
            Err(WriteError::Outside(_)) => return Ok(Err(LiveSkip::NotAFile)),
            Err(e) => return Err(ReviewError::Unavailable(e.to_string())),
        };
        match check_unambiguous(&shadow, commit, &Side::WorkTree, &self.path) {
            Ok(()) => {}
            Err(ReviewError::AmbiguousPath(_)) => return Ok(Err(LiveSkip::Ambiguous)),
            Err(e) => return Err(e),
        }
        let base = shadow.read_entry_with_id(commit, &self.path).map_err(unavailable)?;
        if base.as_ref().is_some_and(|(e, _)| e.kind != EntryKind::Blob) {
            return Ok(Err(LiveSkip::NotAFile));
        }
        if base.is_none() {
            // The shadow keeps a nested repository as one entry, so a file inside one has no entry of
            // its own and would read as new; the panel never lists changes inside one.
            if project
                .inside_nested_repository(&self.path)
                .map_err(|e| ReviewError::Unavailable(e.to_string()))?
            {
                return Ok(Err(LiveSkip::Ignored));
            }
            // Use the shadow's excludes, including the project's info/exclude copied when the
            // baseline was taken. An ignored file absent from the baseline is not a new file. The
            // `top` magic keeps a name that starts with `:` from being read as magic itself.
            let mut spec = std::ffi::OsString::from(":(top)");
            spec.push(self.path.as_os_str());
            let mut ignored = shadow.git(None);
            ignored.args(["check-ignore", "--no-index", "-q", "--"]).arg(&spec);
            let output = git::run(ignored, remaining(deadline).map_err(unavailable)?)
                .map_err(|e| ReviewError::Unavailable(e.to_string()))?;
            match output.status.code() {
                Some(0) => return Ok(Err(LiveSkip::Ignored)),
                Some(1) => {}
                _ => {
                    GitError::check("check-ignore", output).map_err(|e| ReviewError::Unavailable(e.to_string()))?;
                }
            }
            // That answer uses the `.gitignore` files on disk now. One the turn changed may have let
            // out a file the base's rules kept out of the snapshot, which is not new either.
            if ignore_files_changed(&shadow, commit, &project, &self.path)? {
                return Ok(Err(LiveSkip::Ignored));
            }
        }
        let size = write::file_len(&target).map_err(|e| ReviewError::Unavailable(e.to_string()))?;
        if size.is_some_and(|size| size > MAX_FILE_BYTES) {
            return Ok(Err(LiveSkip::TooLarge));
        }
        let current =
            write::read_current_within(&target, MAX_FILE_BYTES).map_err(|e| ReviewError::Unavailable(e.to_string()))?;
        let Current::Regular { bytes, .. } = current else {
            return Ok(Err(LiveSkip::NotAFile));
        };
        let (before, base_blob) = match base {
            Some((entry, id)) => (entry.bytes, id),
            // Only the empty base of a new file is not in the store yet.
            None => (Vec::new(), shadow.store_blob(&[]).map_err(unavailable)?),
        };
        if bytes.len() as u64 > MAX_FILE_BYTES || before.len() as u64 > MAX_FILE_BYTES {
            return Ok(Err(LiveSkip::TooLarge));
        }
        if before == bytes {
            return Ok(Err(LiveSkip::Unchanged));
        }
        let disk_blob = shadow.store_blob(&bytes).map_err(unavailable)?;
        #[cfg(test)]
        super::test_hooks::run(&super::test_hooks::AFTER_LIVE_STORE);
        let patch = blob_hunks(&shadow, &base_blob, &disk_blob).map_err(unavailable)?;
        // Identical bytes were handled above; a differing pair with no text hunks is binary.
        if patch.is_empty() {
            return Ok(Err(LiveSkip::Binary));
        }
        if patch.iter().map(|h| h.lines.len()).sum::<usize>() > MAX_OVERVIEW_DIFF_LINES {
            return Ok(Err(LiveSkip::OverCap));
        }
        let before_lines = lines_of(&before);
        let after_lines = lines_of(&bytes);
        let read_lines = after_lines.len() as u64;
        let hunks = exact_hunks(patch, &before_lines, &after_lines)?;
        Ok(Ok(LiveHunks {
            path: self.path.to_string_lossy().into_owned(),
            hunks,
            read_lines,
        }))
    }
}

impl LiveSettleJob {
    pub fn run(self) -> Result<LiveSettle, ReviewError> {
        let shadow = self.exact.spec.open().map_err(ReviewError::Unavailable)?;
        // The extra end read and ExactHunksJob must observe the same retained snapshots.
        let _lock = shadow
            .lock_shared_until(Instant::now() + READ_TIMEOUT)
            .map_err(unavailable)?;
        let resolved = resolve_compare(
            &shadow,
            &self.exact.session,
            &self.exact.records,
            TurnRef::N(self.exact.turn),
            Scope::Turn,
        )?;
        let Side::Snapshot(end_commit) = &resolved.to else {
            return Err(ReviewError::NotReady("this turn has no end snapshot yet".into()));
        };
        let end = shadow.read_entry(end_commit, &self.exact.path).map_err(unavailable)?;
        let end_lines = end.map_or(0, |e| e.bytes.split_inclusive(|b| *b == b'\n').count() as u64);
        let exact = self.exact.run_resolved(&shadow, &resolved)?;
        Ok(LiveSettle { exact, end_lines })
    }
}

fn unavailable(error: ShadowError) -> ReviewError {
    ReviewError::Unavailable(error.to_string())
}

/// Whether a `.gitignore` in a directory above `path` differs from the base snapshot's copy (made,
/// changed, removed, or no longer a plain file).
fn ignore_files_changed(shadow: &Shadow, commit: &str, project: &ProjectDir, path: &Path) -> Result<bool, ReviewError> {
    for dir in path.ancestors().skip(1) {
        let file = dir.join(".gitignore");
        let base = shadow.read_entry(commit, &file).map_err(unavailable)?;
        let target = match project.target(&file, false) {
            Ok(target) => target,
            Err(WriteError::Outside(_)) => return Ok(true),
            Err(e) => return Err(ReviewError::Unavailable(e.to_string())),
        };
        let now =
            write::read_current_within(&target, MAX_FILE_BYTES).map_err(|e| ReviewError::Unavailable(e.to_string()))?;
        let same = match (&base, &now) {
            (None, Current::Absent) => true,
            (Some(entry), Current::Regular { bytes, .. }) => entry.kind == EntryKind::Blob && entry.bytes == *bytes,
            _ => false,
        };
        if !same {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use agent::AgentDomainEvent;

    use crate::turn_review::test_hooks::{self, AFTER_LIVE_STORE, BEFORE_GC, BEFORE_PRUNE};
    use crate::turn_review::{ReviewOptions, Shadow, TurnReview};

    #[test]
    fn the_job_holds_one_shared_lock_across_store_and_diff() {
        const HANG: Duration = Duration::from_secs(30);
        let scratch = crate::test_scratch_dir::ScratchDir::new("eitri-live-job", "collection");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&*scratch, std::fs::Permissions::from_mode(0o700)).unwrap();
        let project = scratch.join("project");
        let review_dir = scratch.join("review");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("a"), b"base\n").unwrap();
        let mut review = TurnReview::with_options(
            Some(review_dir.clone()),
            &project,
            ReviewOptions {
                excludes: Some(None),
                trim_every: Duration::MAX,
                ..ReviewOptions::default()
            },
        );
        review.observe(1, "s1", &AgentDomainEvent::TurnStarted { turn_id: "t1".into() }, 1);
        let deadline = Instant::now() + HANG;
        loop {
            review.poll();
            if review.turns("s1").first().is_some_and(|r| r.base.commit().is_some()) {
                break;
            }
            assert!(Instant::now() < deadline, "the base snapshot finishes");
            std::thread::sleep(Duration::from_millis(5));
        }
        std::fs::write(project.join("a"), b"changed\n").unwrap();
        let shadow = Shadow::open_with_excludes(&review_dir, &project, None).unwrap();
        let (in_gc_tx, in_gc_rx) = mpsc::channel();
        let (stored_tx, stored_rx) = mpsc::channel();
        let (at_prune_tx, at_prune_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel();
        let collection = std::thread::spawn(move || {
            test_hooks::set(&BEFORE_GC, move || {
                in_gc_tx.send(()).unwrap();
                stored_rx.recv_timeout(HANG).expect("the live job stored its bytes");
            });
            test_hooks::set(&BEFORE_PRUNE, move || {
                let waits = crate::turn_review::shadow::flock_file(
                    &review_dir.join("lock"),
                    libc::LOCK_EX,
                    Some(Instant::now() + Duration::from_millis(500)),
                )
                .is_err();
                at_prune_tx.send(waits).unwrap();
            });
            shadow.trim(100, Duration::MAX, 2, "now")
        });
        in_gc_rx.recv_timeout(HANG).expect("the collection reached its repack");
        let job = review.live_hunks_job("s1", "t1", "a");
        let live = std::thread::spawn(move || {
            test_hooks::set(&AFTER_LIVE_STORE, move || {
                stored_tx.send(()).unwrap();
                go_rx.recv_timeout(HANG).expect("the collection reached its prune");
            });
            job.run()
        });
        let waits = at_prune_rx
            .recv_timeout(HANG)
            .expect("the collection reached its prune");
        // Release the live job even if the assertion fails, so neither worker remains parked.
        go_tx.send(()).unwrap();
        let result = live.join().unwrap().unwrap().unwrap();
        collection.join().unwrap().unwrap();
        assert!(
            waits,
            "collection cannot prune between storing the file and reading its patch"
        );
        assert_eq!(result.hunks[0].new_lines.concat(), b"changed\n");
    }
}
