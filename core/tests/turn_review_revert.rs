//! Turn review's revert engine against real git and real files: turns scripted through a
//! `TurnReview` take real snapshots of a project under the target dir, and every revert, undo and
//! recovery writes only inside it.
//!
//! The recovery tests re-run this binary as a child whose in-place write dies halfway. The child
//! test is `#[ignore]`d and does nothing unless the parent set its directory variable, so
//! `-- --ignored` over this target is harmless.

use std::cell::RefCell;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent::{AgentDomainEvent, TurnOutcome};
use eitri_core::editor_rpc::EditorRpc;
use eitri_core::nvim_rpc::{Answer, Pending};
use eitri_core::turn_review::{
    replace, review_dir_for, AppliedRevert, Blocked, Busy, Content, EditorCheck, EditorClear, ExactHunks, FsHooks,
    Journal, JournalEntry, JournalNote, Limits, Presence, PresenceGuard, ProjectDir, RecoverAnswer, Refusal,
    RevertKind, RevertTarget, ReviewOptions, Saved, Scope, Shadow, Snap, Stage, TurnReview, BUFFER_STATE_LUA,
};
use rmpv::Value;

const CHILD_DIR: &str = "EITRI_REVERT_CHILD_DIR";
const CHILD_REL: &str = "EITRI_REVERT_CHILD_REL";
const SESSION: &str = "s1";
const TAB: u64 = 1;
/// A pid no real window here has.
const OTHER_PID: u32 = 999_999;

/// A directory of the test's own under the target dir, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("turn_review_revert")
            .join(format!(
                "{name}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir.canonicalize().unwrap())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn project_of(dir: &Path) -> PathBuf {
    dir.join("project")
}

fn review_dir_of(dir: &Path) -> PathBuf {
    review_dir_for(Some(dir.join("state").as_os_str()), None, &project_of(dir)).unwrap()
}

/// A project, its turn review and the turns run so far.
struct World {
    scratch: Scratch,
    project: PathBuf,
    review_dir: PathBuf,
    review: TurnReview,
    turns: u32,
}

impl World {
    fn new(name: &str) -> World {
        World::with_limits(name, Limits::default())
    }

    fn with_limits(name: &str, limits: Limits) -> World {
        let scratch = Scratch::new(name);
        let project = project_of(&scratch.0);
        std::fs::create_dir_all(&project).unwrap();
        let review_dir = review_dir_of(&scratch.0);
        let options = ReviewOptions {
            limits,
            excludes: Some(None),
            trim_every: Duration::MAX,
            ..ReviewOptions::default()
        };
        let mut review = TurnReview::with_options(Some(review_dir.clone()), &project, options);
        review.observe(
            TAB,
            SESSION,
            &AgentDomainEvent::SessionOpened {
                session_id: "local".into(),
                provider_session_id: SESSION.into(),
                model: "m".into(),
                cwd: "/".into(),
            },
            now(),
        );
        World {
            scratch,
            project,
            review_dir,
            review,
            turns: 0,
        }
    }

    fn dir(&self) -> &Path {
        &self.scratch.0
    }

    fn file(&self, rel: &str) -> PathBuf {
        self.project.join(rel)
    }

    fn write(&self, rel: &str, bytes: &[u8], mode: u32) {
        write_file(&self.file(rel), bytes, mode);
    }

    fn read(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.file(rel)).unwrap()
    }

    /// Starts a turn and waits for its base snapshot; returns its number.
    fn start_turn(&mut self) -> u32 {
        self.turns += 1;
        let id = format!("t{}", self.turns);
        self.review.observe(
            TAB,
            SESSION,
            &AgentDomainEvent::TurnStarted { turn_id: id.clone() },
            now(),
        );
        self.wait_for(&id, "its base", |r| matches!(r.base, Snap::Taken { .. }));
        self.number_of(&id)
    }

    /// Ends the running turn and waits for its end snapshot.
    fn end_turn(&mut self) {
        let id = format!("t{}", self.turns);
        self.review.observe(
            TAB,
            SESSION,
            &AgentDomainEvent::TurnCompleted {
                turn_id: id.clone(),
                outcome: TurnOutcome::Completed,
                result_text: String::new(),
                stop_reason: None,
                usage: None,
                detail: Default::default(),
            },
            now(),
        );
        self.wait_for(&id, "its end", |r| matches!(r.end, Snap::Taken { .. }));
    }

    /// One whole turn whose change on disk is `change`.
    fn turn(&mut self, change: impl FnOnce(&World)) -> u32 {
        let n = self.start_turn();
        change(self);
        self.end_turn();
        n
    }

    fn number_of(&self, id: &str) -> u32 {
        self.review
            .turns(SESSION)
            .iter()
            .find(|r| r.turn_id == id)
            .expect("the turn is recorded")
            .n
    }

    fn wait_for(&mut self, id: &str, what: &str, done: impl Fn(&eitri_core::turn_review::TurnRecord) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.review.poll();
            let turns = self.review.turns(SESSION);
            if let Some(r) = turns.iter().find(|r| r.turn_id == id) {
                assert!(
                    !matches!(r.base, Snap::Unavailable(_)) && !matches!(r.end, Snap::Unavailable(_)),
                    "{r:#?}"
                );
                if done(r) {
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what} of {id}: {turns:#?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn guard(&self) -> PresenceGuard {
        PresenceGuard::for_tests(&self.review_dir, std::process::id(), 0)
    }

    fn clear(&self, rel: &str) -> EditorClear {
        EditorClear::for_tests(&self.file(rel))
    }

    fn hunks(&self, n: u32, rel: &str) -> ExactHunks {
        self.review.exact_hunks_job(SESSION, n, Scope::Turn, rel).run().unwrap()
    }

    /// Hunk `id` of `rel` in turn `n`, as the review shows it.
    fn hunk(&self, n: u32, rel: &str, id: u32) -> RevertTarget {
        let hunks = self.hunks(n, rel);
        let hunk = hunks.hunks.unwrap().into_iter().find(|h| h.id == id).expect("the hunk");
        RevertTarget::Hunk {
            id,
            header: hunk.header,
        }
    }

    fn revert_in(&self, n: u32, scope: Scope, rel: &str, target: RevertTarget) -> Result<AppliedRevert, Refusal> {
        self.review
            .revert_job(self.guard(), wanted(), SESSION, n, scope, rel, target, self.clear(rel))
            .run()
    }

    fn revert(&self, n: u32, rel: &str, target: RevertTarget) -> Result<AppliedRevert, Refusal> {
        self.revert_in(n, Scope::Turn, rel, target)
    }

    fn revert_file(&self, n: u32, rel: &str) -> Result<AppliedRevert, Refusal> {
        self.revert(n, rel, RevertTarget::File)
    }

    fn undo(&self, applied: &AppliedRevert) -> Result<(), Refusal> {
        self.review
            .undo_job(
                self.guard(),
                wanted(),
                SESSION,
                &applied.path,
                applied.pre.clone(),
                applied.post.clone(),
                self.clear(&applied.path),
            )
            .run()
    }

    fn recover(&self, entry: &JournalEntry) -> Result<(), Refusal> {
        let rel = entry.path.to_str().unwrap();
        self.review
            .recover_job(self.guard(), &entry.id, RecoverAnswer::Restore(self.clear(rel)))
            .run()
    }

    fn shadow(&self) -> Shadow {
        Shadow::open_with_excludes(&self.review_dir, &self.project, None).unwrap()
    }

    /// The pending entries. A lock just let go can look held for a moment while another test
    /// thread starts a child process (it shares every descriptor until it execs), so an empty
    /// answer is asked again for up to a second when `expected` entries should be there.
    fn pending_of(&self, expected: usize) -> Vec<JournalEntry> {
        let journal = Journal::open(&self.review_dir).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let pending = journal.pending();
            if pending.len() >= expected || Instant::now() > deadline {
                return pending;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn pending(&self) -> Vec<JournalEntry> {
        self.pending_of(0)
    }

    /// The shadow's refs under `prefix`.
    fn refs(&self, prefix: &str) -> Vec<String> {
        let out = tgit(
            &self.review_dir,
            &["--git-dir=git", "for-each-ref", "--format=%(refname)", prefix],
        );
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The bytes of the blob a shadow ref names.
    fn ref_bytes(&self, name: &str) -> Vec<u8> {
        tgit(&self.review_dir, &["--git-dir=git", "cat-file", "blob", name]).stdout
    }
}

fn wanted() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(true))
}

/// Test-side git in `dir`, isolated from the machine's config and from the process environment.
fn tgit_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir)
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .current_dir(dir);
    cmd
}

fn tgit(dir: &Path, args: &[&str]) -> Output {
    let out = tgit_cmd(dir).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn write_file(path: &Path, bytes: &[u8], mode: u32) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let _ = std::fs::remove_file(path);
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn meta(path: &Path) -> std::fs::Metadata {
    std::fs::symlink_metadata(path).unwrap()
}

fn mode_of(path: &Path) -> u32 {
    meta(path).mode() & 0o7777
}

/// The bytes, inode and modification time of a file, none of which a refused write may change.
fn fingerprint(path: &Path) -> (Vec<u8>, u64, i64, i64) {
    let m = meta(path);
    (std::fs::read(path).unwrap(), m.ino(), m.mtime(), m.mtime_nsec())
}

/// `n` numbered lines, `line <i>\n`.
fn numbered(n: usize) -> Vec<Vec<u8>> {
    (1..=n).map(|i| format!("line {i}\n").into_bytes()).collect()
}

fn assert_refused<T: std::fmt::Debug>(result: Result<T, Refusal>, want: &Refusal) {
    match result {
        Err(got) => assert_eq!(&got, want),
        Ok(v) => panic!("expected {want:?}, got Ok({v:?})"),
    }
}

/// Bytes no compression shrinks much.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

// ---- hunks -----------------------------------------------------------------------------------------

#[test]
fn a_hunk_reverts_to_the_base_and_nothing_else() {
    let mut w = World::new("hunk");
    let mut base = numbered(20);
    base[2] = b"crlf three\r\n".to_vec();
    base[3] = b"not utf-8 \xff\xfe four\n".to_vec();
    w.write("a.txt", &base.concat(), 0o644);
    let mut end = base.clone();
    end[3] = b"changed \xfe four\r\n".to_vec();
    end[16] = b"changed seventeen\n".to_vec();
    let n = w.turn(|w| w.write("a.txt", &end.concat(), 0o644));

    let hunks = w.hunks(n, "a.txt");
    let list = hunks.hunks.as_ref().unwrap();
    assert!(!hunks.binary);
    assert_eq!(list.len(), 2, "{list:#?}");
    assert_eq!(list[0].old_lines, base[0..7].to_vec(), "byte-exact, terminators kept");
    assert_eq!(list[0].new_lines, end[0..7].to_vec());

    // An edit of the user's between the two hunks survives the revert.
    let mut cur = end.clone();
    cur[9] = b"the user's line ten\n".to_vec();
    w.write("a.txt", &cur.concat(), 0o644);
    let applied = w.revert(n, "a.txt", w.hunk(n, "a.txt", 0)).unwrap();

    let mut expected = cur.clone();
    expected[3] = base[3].clone();
    assert_eq!(w.read("a.txt"), expected.concat());
    assert_eq!(
        applied.kind,
        RevertKind::Hunk {
            base: (1, 7),
            end: (1, 7)
        }
    );
    assert_eq!(applied.at_line, 1);
    assert_eq!(applied.reverted_to, base[0..7].concat());
    assert_eq!(applied.replaced, end[0..7].concat());
    assert_eq!(applied.turn, n);
    assert_eq!(applied.path, "a.txt");
    assert_eq!(applied.hunk.as_ref().map(|h| h.0), Some(0));
    assert!(matches!(applied.pre, Saved::Regular { mode: 0o644, .. }));
    assert_eq!(w.refs("refs/eitri-revert/").len(), 2, "pre and post are anchored");
}

#[test]
fn zero_length_ranges_splice_at_the_right_line() {
    let mut w = World::new("zerolen");
    let eight = numbered(8).concat();
    w.write("empty.txt", b"", 0o644);
    w.write("emptied.txt", b"x\ny\n", 0o644);
    w.write("tail.txt", &eight, 0o644);
    w.write("head.txt", &eight, 0o644);
    w.write("cut.txt", &eight, 0o644);
    let n = w.turn(|w| {
        w.write("empty.txt", b"x\ny\n", 0o644);
        w.write("emptied.txt", b"", 0o644);
        w.write(
            "tail.txt",
            &[eight.clone(), b"new 9\nnew 10\n".to_vec()].concat(),
            0o644,
        );
        w.write(
            "head.txt",
            &[b"new 0\nnew 0b\n".to_vec(), eight.clone()].concat(),
            0o644,
        );
        w.write("cut.txt", &numbered(6).concat(), 0o644);
    });
    let header = |rel: &str| match w.hunk(n, rel, 0) {
        RevertTarget::Hunk { header, .. } => header,
        RevertTarget::File => unreachable!(),
    };
    assert!(
        header("empty.txt").starts_with("@@ -0,0 +1,2 @@"),
        "{}",
        header("empty.txt")
    );
    assert!(
        header("emptied.txt").starts_with("@@ -1,2 +0,0 @@"),
        "{}",
        header("emptied.txt")
    );

    for (rel, base) in [
        ("empty.txt", b"".to_vec()),
        ("emptied.txt", b"x\ny\n".to_vec()),
        ("tail.txt", eight.clone()),
        ("head.txt", eight.clone()),
        ("cut.txt", eight.clone()),
    ] {
        let applied = w
            .revert(n, rel, w.hunk(n, rel, 0))
            .unwrap_or_else(|e| panic!("{rel}: {e}"));
        assert_eq!(w.read(rel), base, "{rel}");
        let RevertKind::Hunk { end, .. } = applied.kind else {
            panic!("{rel}: {:?}", applied.kind)
        };
        let expected_line = if end.1 == 0 { end.0 + 1 } else { end.0 };
        assert_eq!(applied.at_line, expected_line, "{rel}");
    }
}

#[test]
fn an_emptied_file_hunk_needs_it_still_empty() {
    let mut w = World::new("emptied");
    w.write("e.txt", b"x\ny\n", 0o644);
    let n = w.turn(|w| w.write("e.txt", b"", 0o644));
    let target = w.hunk(n, "e.txt", 0);
    w.write("e.txt", b"z\n", 0o644);
    assert_refused(w.revert(n, "e.txt", target), &Refusal::ChangedSinceEnd);
    assert_eq!(w.read("e.txt"), b"z\n");
}

#[test]
fn a_hunk_whose_region_changed_is_refused() {
    let mut w = World::new("region");
    w.write("a.txt", &numbered(20).concat(), 0o644);
    let mut end = numbered(20);
    end[14] = b"changed\n".to_vec();
    let n = w.turn(|w| w.write("a.txt", &end.concat(), 0o644));
    let target = w.hunk(n, "a.txt", 0);
    // Context counts: line 13 is inside the hunk's region.
    let mut cur = end.clone();
    cur[12] = b"edited context\n".to_vec();
    w.write("a.txt", &cur.concat(), 0o644);
    let before = fingerprint(&w.file("a.txt"));
    let refused = w.revert(n, "a.txt", target).unwrap_err();
    assert_eq!(refused, Refusal::ChangedSinceEnd);
    assert_eq!(
        refused.to_string(),
        "changed since the turn ended; open it in the editor (o)"
    );
    assert_eq!(fingerprint(&w.file("a.txt")), before);
    assert!(w.refs("refs/eitri-revert/").is_empty());
}

#[test]
fn a_hunk_that_moved_is_refused() {
    let mut w = World::new("moved");
    w.write("a.txt", &numbered(20).concat(), 0o644);
    let mut end = numbered(20);
    end[14] = b"changed\n".to_vec();
    let n = w.turn(|w| w.write("a.txt", &end.concat(), 0o644));
    let target = w.hunk(n, "a.txt", 0);
    // The region's bytes are all still there, one line further down.
    let cur = [b"a new first line\n".to_vec(), end.concat()].concat();
    w.write("a.txt", &cur, 0o644);
    assert_refused(w.revert(n, "a.txt", target), &Refusal::ChangedSinceEnd);
    assert_eq!(w.read("a.txt"), cur);
}

#[test]
fn binary_reverts_only_whole() {
    let mut w = World::new("binary");
    w.write("b.dat", b"\x00\x01binary\x00base", 0o644);
    let n = w.turn(|w| w.write("b.dat", b"\x00\x02binary\x00end", 0o644));
    let hunks = w.hunks(n, "b.dat");
    assert!(hunks.binary, "{hunks:?}");
    let target = RevertTarget::Hunk {
        id: 0,
        header: String::new(),
    };
    assert_refused(w.revert(n, "b.dat", target), &Refusal::BinaryHunk);
    assert_eq!(w.read("b.dat"), b"\x00\x02binary\x00end");
    w.revert_file(n, "b.dat").unwrap();
    assert_eq!(w.read("b.dat"), b"\x00\x01binary\x00base");
}

#[test]
fn an_out_of_date_header_is_refused() {
    let mut w = World::new("outofdate");
    w.write("a.txt", &numbered(10).concat(), 0o644);
    let mut end = numbered(10);
    end[4] = b"changed\n".to_vec();
    let n = w.turn(|w| w.write("a.txt", &end.concat(), 0o644));
    let stale = RevertTarget::Hunk {
        id: 0,
        header: "@@ -1,9 +1,9 @@".into(),
    };
    let refused = w.revert(n, "a.txt", stale).unwrap_err();
    assert_eq!(refused, Refusal::OutOfDate);
    assert_eq!(refused.to_string(), "the review is out of date; reopen it");
    let RevertTarget::Hunk { header, .. } = w.hunk(n, "a.txt", 0) else {
        unreachable!()
    };
    assert_refused(
        w.revert(n, "a.txt", RevertTarget::Hunk { id: 7, header }),
        &Refusal::OutOfDate,
    );
    assert_eq!(w.read("a.txt"), end.concat());
}

// ---- whole files -----------------------------------------------------------------------------------

#[test]
fn a_whole_file_revert_refuses_any_changed_byte() {
    let mut w = World::new("wholebyte");
    let end = numbered(30).concat();
    let n = w.turn(|w| w.write("new.rs", &end, 0o644));
    // One byte, far from anything a hunk would show.
    let mut cur = end.clone();
    cur[0] = b'L';
    w.write("new.rs", &cur, 0o644);
    let before = fingerprint(&w.file("new.rs"));
    assert_refused(w.revert_file(n, "new.rs"), &Refusal::ChangedSinceEnd);
    assert_eq!(fingerprint(&w.file("new.rs")), before);
}

#[test]
fn whole_file_delete_restore_replace() {
    let mut w = World::new("whole");
    w.write("bin/run.sh", b"#!/bin/sh\necho base\n", 0o755);
    w.write("modified.txt", b"base\r\n", 0o644);
    w.write("target.txt", b"the link's target\n", 0o644);
    std::os::unix::fs::symlink("target.txt", w.file("link")).unwrap();
    let n = w.turn(|w| {
        w.write("created.txt", b"made by the turn\n", 0o644);
        std::fs::remove_dir_all(w.file("bin")).unwrap();
        w.write("modified.txt", b"end\n", 0o644);
        std::fs::remove_file(w.file("link")).unwrap();
        w.write("link", b"a regular file now\n", 0o644);
    });
    let target_before = fingerprint(&w.file("target.txt"));

    let deleted = w.revert_file(n, "created.txt").unwrap();
    assert_eq!(deleted.kind, RevertKind::Delete);
    assert!(!w.file("created.txt").exists());
    assert_eq!(deleted.post, Saved::Absent);

    let restored = w.revert_file(n, "bin/run.sh").unwrap();
    assert_eq!(restored.kind, RevertKind::Restore);
    assert_eq!(w.read("bin/run.sh"), b"#!/bin/sh\necho base\n");
    assert_eq!(mode_of(&w.file("bin/run.sh")), 0o755, "the base's mode");
    assert_eq!(restored.pre, Saved::Absent);

    let replaced = w.revert_file(n, "modified.txt").unwrap();
    assert_eq!(replaced.kind, RevertKind::Replace);
    assert_eq!(w.read("modified.txt"), b"base\r\n");
    assert_eq!(replaced.reverted_to, b"base\r\n");
    assert_eq!(replaced.replaced, b"end\n");

    let relinked = w.revert_file(n, "link").unwrap();
    assert_eq!(relinked.kind, RevertKind::Replace);
    assert_eq!(std::fs::read_link(w.file("link")).unwrap(), Path::new("target.txt"));
    assert_eq!(fingerprint(&w.file("target.txt")), target_before);
}

#[test]
fn restore_requires_the_path_absent() {
    let mut w = World::new("restore");
    w.write("gone.txt", b"base\n", 0o644);
    let n = w.turn(|w| std::fs::remove_file(w.file("gone.txt")).unwrap());
    w.write("gone.txt", b"made again by the user\n", 0o644);
    assert_refused(w.revert_file(n, "gone.txt"), &Refusal::ChangedSinceEnd);
    assert_eq!(w.read("gone.txt"), b"made again by the user\n");
}

#[test]
fn replace_keeps_the_files_own_permission_bits() {
    let mut w = World::new("ownbits");
    w.write("private.txt", b"base\n", 0o600);
    let n = w.turn(|w| std::fs::write(w.file("private.txt"), b"end\n").unwrap());
    assert_eq!(mode_of(&w.file("private.txt")), 0o600);
    w.revert_file(n, "private.txt").unwrap();
    assert_eq!(w.read("private.txt"), b"base\n");
    assert_eq!(mode_of(&w.file("private.txt")), 0o600, "git's 0644 does not widen it");
}

#[test]
fn a_file_left_out_for_its_size_is_not_reverted() {
    let limits = Limits {
        max_file_bytes: 64,
        ..Limits::default()
    };
    let mut w = World::with_limits("toolarge", limits);
    w.write("big.txt", &[b'x'; 100], 0o644);
    let n = w.turn(|w| w.write("big.txt", b"small now\n", 0o644));
    let refused = w.revert_file(n, "big.txt").unwrap_err();
    assert!(
        matches!(&refused, Refusal::Unavailable(why) if why.contains("too large")),
        "{refused:?}"
    );
    assert_eq!(
        w.read("big.txt"),
        b"small now\n",
        "not deleted as if the turn had made it"
    );
}

#[test]
fn session_scope_compares_with_the_latest_end() {
    let mut w = World::new("session");
    w.write("a.txt", b"v0\n", 0o644);
    let first = w.turn(|w| w.write("a.txt", b"v1\n", 0o644));
    let second = w.turn(|w| w.write("a.txt", b"v2\n", 0o644));
    assert_refused(w.revert_file(first, "a.txt"), &Refusal::ChangedSinceEnd);
    let applied = w.revert_in(first, Scope::Session, "a.txt", RevertTarget::File).unwrap();
    assert_eq!(w.read("a.txt"), b"v0\n");
    assert_eq!(applied.turn, second);
    assert_eq!(applied.scope, Scope::Session);
    assert_eq!(applied.replaced, b"v2\n");
}

#[test]
fn an_unfinished_turn_is_refused() {
    let mut w = World::new("unfinished");
    w.write("a.txt", b"base\n", 0o644);
    let n = w.start_turn();
    w.write("a.txt", b"while running\n", 0o644);
    assert_refused(w.revert_file(n, "a.txt"), &Refusal::Unfinished);
    assert_eq!(w.read("a.txt"), b"while running\n");
}

#[test]
fn a_trimmed_turn_is_refused_with_a_reason() {
    let mut w = World::new("trimmed");
    w.write("a.txt", b"base\n", 0o644);
    let n = w.turn(|w| w.write("a.txt", b"end\n", 0o644));
    let month = 31 * 24 * 3600 * 1000;
    w.shadow()
        .trim(0, Duration::from_millis(1), now() + month, "now")
        .unwrap();
    let refused = w.revert_file(n, "a.txt").unwrap_err();
    assert_eq!(refused, Refusal::Retained);
    assert_eq!(refused.to_string(), "this turn's snapshots were removed by retention");
    assert_eq!(w.read("a.txt"), b"end\n");
}

// ---- preconditions ---------------------------------------------------------------------------------

#[test]
fn a_running_turn_refuses_here_and_in_another_window() {
    let mut w = World::new("running");
    w.write("a.txt", b"base\n", 0o644);
    let n = w.turn(|w| w.write("a.txt", b"end\n", 0o644));
    let before = fingerprint(&w.file("a.txt"));
    let revert = |guard: PresenceGuard| {
        w.review
            .revert_job(
                guard,
                wanted(),
                SESSION,
                n,
                Scope::Turn,
                "a.txt",
                RevertTarget::File,
                w.clear("a.txt"),
            )
            .run()
    };

    let here = PresenceGuard::for_tests(&w.review_dir, std::process::id(), 1);
    let refused = revert(here).unwrap_err();
    assert_eq!(refused, Refusal::Blocked(Blocked::TurnRunningHere));
    assert_eq!(refused.to_string(), "an agent turn is running");

    let mut other = Presence::open(&w.review_dir, OTHER_PID).unwrap();
    other.turn_started(3).unwrap();
    assert_refused(
        revert(w.guard()),
        &Refusal::Blocked(Blocked::Busy(Busy::TurnRunning { pid: OTHER_PID })),
    );
    other.turn_ended(3);
    let refused = revert(w.guard()).unwrap_err();
    assert_eq!(
        refused,
        Refusal::Blocked(Blocked::Busy(Busy::OtherWindow { pid: OTHER_PID }))
    );
    assert!(refused.to_string().contains("revert from one window at a time"));
    assert_eq!(fingerprint(&w.file("a.txt")), before);
    assert!(w.refs("refs/eitri-revert/").is_empty());

    drop(other);
    revert(w.guard()).unwrap();
    assert_eq!(w.read("a.txt"), b"base\n");
}

#[test]
fn a_cancelled_job_writes_nothing() {
    let mut w = World::new("cancelled");
    w.write("a.txt", b"base\n", 0o644);
    let n = w.turn(|w| w.write("a.txt", b"end\n", 0o644));
    let before = fingerprint(&w.file("a.txt"));
    let gone = Arc::new(AtomicBool::new(false));
    let refused = w
        .review
        .revert_job(
            w.guard(),
            gone.clone(),
            SESSION,
            n,
            Scope::Turn,
            "a.txt",
            RevertTarget::File,
            w.clear("a.txt"),
        )
        .run()
        .unwrap_err();
    assert_eq!(refused, Refusal::Cancelled);
    assert_eq!(refused.to_string(), "the tab closed; nothing was written");
    assert_eq!(fingerprint(&w.file("a.txt")), before);
    assert!(w.refs("refs/eitri-revert/").is_empty(), "nothing was stored either");

    let applied = w.revert_file(n, "a.txt").unwrap();
    let before = fingerprint(&w.file("a.txt"));
    let undo = w.review.undo_job(
        w.guard(),
        gone,
        SESSION,
        "a.txt",
        applied.pre.clone(),
        applied.post.clone(),
        w.clear("a.txt"),
    );
    assert_refused(undo.run(), &Refusal::Cancelled);
    assert_eq!(fingerprint(&w.file("a.txt")), before);
}

#[test]
fn a_clear_for_another_path_is_refused() {
    let mut w = World::new("otherclear");
    w.write("a.txt", b"base\n", 0o644);
    w.write("b.txt", b"b\n", 0o644);
    let n = w.turn(|w| w.write("a.txt", b"end\n", 0o644));
    let wrong = || Refusal::Unavailable("the editor check was for another file".into());
    let refused = w
        .review
        .revert_job(
            w.guard(),
            wanted(),
            SESSION,
            n,
            Scope::Turn,
            "a.txt",
            RevertTarget::File,
            w.clear("b.txt"),
        )
        .run();
    assert_refused(refused, &wrong());
    assert_eq!(w.read("a.txt"), b"end\n");

    let applied = w.revert_file(n, "a.txt").unwrap();
    let undo = w.review.undo_job(
        w.guard(),
        wanted(),
        SESSION,
        "a.txt",
        applied.pre.clone(),
        applied.post.clone(),
        w.clear("b.txt"),
    );
    assert_refused(undo.run(), &wrong());
    assert_eq!(w.read("a.txt"), b"base\n");

    let entry = crash_mid_write(&w, "a.txt");
    let half = w.read("a.txt");
    let refused = w
        .review
        .recover_job(w.guard(), &entry.id, RecoverAnswer::Restore(w.clear("b.txt")))
        .run();
    assert_refused(refused, &wrong());
    assert_eq!(w.read("a.txt"), half);
    assert_eq!(w.pending_of(1).len(), 1, "the entry is kept");
}

#[test]
fn a_change_between_check_and_write_is_refused() {
    let mut w = World::new("latechange");
    w.write("a.txt", b"base\n", 0o644);
    let n = w.turn(|w| w.write("a.txt", b"end\n", 0o644));
    let file = w.file("a.txt");
    let job = w
        .review
        .revert_job(
            w.guard(),
            wanted(),
            SESSION,
            n,
            Scope::Turn,
            "a.txt",
            RevertTarget::File,
            w.clear("a.txt"),
        )
        .on_final_check(Arc::new(move || {
            std::fs::write(&file, b"the user, just now\n").unwrap()
        }));
    assert_refused(job.run(), &Refusal::ChangedSinceEnd);
    assert_eq!(w.read("a.txt"), b"the user, just now\n");
}

#[test]
fn a_late_cancel_writes_nothing() {
    let mut w = World::new("latecancel");
    w.write("a.txt", b"base\n", 0o644);
    let n = w.turn(|w| w.write("a.txt", b"end\n", 0o644));
    let before = fingerprint(&w.file("a.txt"));
    let flag = wanted();
    let closer = flag.clone();
    let job = w
        .review
        .revert_job(
            w.guard(),
            flag,
            SESSION,
            n,
            Scope::Turn,
            "a.txt",
            RevertTarget::File,
            w.clear("a.txt"),
        )
        .on_final_check(Arc::new(move || closer.store(false, Ordering::SeqCst)));
    assert_refused(job.run(), &Refusal::Cancelled);
    assert_eq!(fingerprint(&w.file("a.txt")), before);
}

#[test]
fn a_turn_started_before_the_write_refuses() {
    let mut w = World::new("lateturn");
    w.write("a.txt", b"base\n", 0o644);
    let n = w.turn(|w| w.write("a.txt", b"end\n", 0o644));
    let before = fingerprint(&w.file("a.txt"));
    let review_dir = w.review_dir.clone();
    let other: Arc<Mutex<Option<Presence>>> = Arc::new(Mutex::new(None));
    let held = other.clone();
    let job = w
        .review
        .revert_job(
            w.guard(),
            wanted(),
            SESSION,
            n,
            Scope::Turn,
            "a.txt",
            RevertTarget::File,
            w.clear("a.txt"),
        )
        .on_final_check(Arc::new(move || {
            let mut p = Presence::open(&review_dir, OTHER_PID).unwrap();
            p.turn_started(1).unwrap();
            *held.lock().unwrap() = Some(p);
        }));
    assert_refused(
        job.run(),
        &Refusal::Blocked(Blocked::Busy(Busy::TurnRunning { pid: OTHER_PID })),
    );
    assert_eq!(fingerprint(&w.file("a.txt")), before);
    drop(other);
}

#[test]
fn a_parent_symlinked_out_of_the_project_refuses_revert_undo_and_recover() {
    let mut w = World::new("symparent");
    let mut base = numbered(12);
    w.write("src/x.rs", &base.concat(), 0o644);
    base[5] = b"changed six\n".to_vec();
    let end = base;
    let n = w.turn(|w| w.write("src/x.rs", &end.concat(), 0o644));
    let hunk = w.hunk(n, "src/x.rs", 0);
    let entry = crash_mid_write(&w, "src/x.rs");

    // `src` becomes a link to a directory outside holding exactly the end bytes.
    let outside = w.dir().join("outside");
    write_file(&outside.join("x.rs"), &end.concat(), 0o644);
    std::fs::rename(w.file("src"), w.dir().join("src-moved")).unwrap();
    std::os::unix::fs::symlink(&outside, w.file("src")).unwrap();
    let before = fingerprint(&outside.join("x.rs"));
    let not_inside = |r: Refusal| match r {
        Refusal::NotInProject(why) => assert!(why.contains("is not inside the project"), "{why}"),
        other => panic!("expected NotInProject, got {other:?}"),
    };

    not_inside(w.revert(n, "src/x.rs", hunk).unwrap_err());
    not_inside(w.revert_file(n, "src/x.rs").unwrap_err());
    let shadow = w.shadow();
    let saved = |bytes: &[u8]| Saved::Regular {
        blob: shadow.store_blob(bytes).unwrap(),
        mode: 0o644,
    };
    let undo = w.review.undo_job(
        w.guard(),
        wanted(),
        SESSION,
        "src/x.rs",
        saved(&numbered(12).concat()),
        saved(&end.concat()),
        w.clear("src/x.rs"),
    );
    not_inside(undo.run().unwrap_err());
    not_inside(w.recover(&entry).unwrap_err());

    assert_eq!(fingerprint(&outside.join("x.rs")), before);
    assert!(w.refs("refs/eitri-revert/").is_empty(), "no anchor was made");
    assert_eq!(w.pending_of(1).len(), 1, "the entry is kept");
}

/// A directory reached at the start of a job and then moved out of the project, with the file in
/// it untouched, is not written: the last look reaches the file from the root again.
#[test]
fn a_parent_moved_out_of_the_project_meanwhile_is_not_written() {
    let mut w = World::new("movedparent");
    let mut base = numbered(12);
    w.write("src/x.rs", &base.concat(), 0o644);
    base[5] = b"changed six\n".to_vec();
    let end = base;
    let n = w.turn(|w| w.write("src/x.rs", &end.concat(), 0o644));
    let hunk = w.hunk(n, "src/x.rs", 0);

    let inside = w.file("src");
    let moved = w.dir().join("moved-out");
    let move_out = {
        let (inside, moved) = (inside.clone(), moved.clone());
        Arc::new(move || std::fs::rename(&inside, &moved).unwrap())
    };
    let move_back = || std::fs::rename(&moved, &inside).unwrap();

    for target in [hunk, RevertTarget::File] {
        let job = w
            .review
            .revert_job(
                w.guard(),
                wanted(),
                SESSION,
                n,
                Scope::Turn,
                "src/x.rs",
                target,
                w.clear("src/x.rs"),
            )
            .on_final_check(move_out.clone());
        assert_refused(job.run(), &Refusal::ChangedSinceEnd);
        assert_eq!(std::fs::read(moved.join("x.rs")).unwrap(), end.concat());
        move_back();
    }

    let applied = w.revert_file(n, "src/x.rs").unwrap();
    assert_eq!(w.read("src/x.rs"), numbered(12).concat());
    let undo = w
        .review
        .undo_job(
            w.guard(),
            wanted(),
            SESSION,
            &applied.path,
            applied.pre.clone(),
            applied.post.clone(),
            w.clear("src/x.rs"),
        )
        .on_final_check(move_out.clone());
    assert_refused(undo.run(), &Refusal::ChangedSinceRevert);
    assert_eq!(std::fs::read(moved.join("x.rs")).unwrap(), numbered(12).concat());
    move_back();

    let entry = crash_mid_write(&w, "src/x.rs");
    let half = w.read("src/x.rs");
    let recover = w
        .review
        .recover_job(w.guard(), &entry.id, RecoverAnswer::Restore(w.clear("src/x.rs")))
        .on_final_check(move_out.clone());
    match recover.run() {
        Err(Refusal::Unavailable(why)) => assert!(why.contains("changed while it was being restored"), "{why}"),
        other => panic!("expected the recovery to be refused, got {other:?}"),
    }
    assert_eq!(std::fs::read(moved.join("x.rs")).unwrap(), half);
    move_back();
    assert_eq!(w.pending_of(1).len(), 1, "the entry is kept");
}

// ---- undo ------------------------------------------------------------------------------------------

#[test]
fn undo_puts_back_exactly_what_was_there() {
    let mut w = World::new("undo");
    let mut base = numbered(20);
    base[2] = b"crlf\r\n".to_vec();
    w.write("a.txt", &base.concat(), 0o640);
    let mut end = base.clone();
    end[3] = b"changed \xff\r\n".to_vec();
    let n = w.turn(|w| w.write("a.txt", &end.concat(), 0o640));
    let mut cur = end.clone();
    cur[15] = b"the user's\n".to_vec();
    w.write("a.txt", &cur.concat(), 0o640);
    let applied = w.revert(n, "a.txt", w.hunk(n, "a.txt", 0)).unwrap();
    assert_ne!(w.read("a.txt"), cur.concat());
    w.undo(&applied).unwrap();
    assert_eq!(w.read("a.txt"), cur.concat());
    assert_eq!(mode_of(&w.file("a.txt")), 0o640);
}

#[test]
fn undo_refuses_when_the_file_changed_since() {
    let mut w = World::new("undochanged");
    w.write("a.txt", b"base\n", 0o644);
    let n = w.turn(|w| w.write("a.txt", b"end\n", 0o644));
    let applied = w.revert_file(n, "a.txt").unwrap();
    w.write("a.txt", b"base, then the user\n", 0o644);
    let refused = w.undo(&applied).unwrap_err();
    assert_eq!(refused, Refusal::ChangedSinceRevert);
    assert_eq!(
        refused.to_string(),
        "changed since the revert; open it in the editor (o)"
    );
    assert_eq!(w.read("a.txt"), b"base, then the user\n");

    // A mode changed since is not a change of what the revert wrote.
    w.write("a.txt", b"base\n", 0o600);
    w.undo(&applied).unwrap();
    assert_eq!(w.read("a.txt"), b"end\n");
}

#[test]
fn undo_of_a_deleted_executable_gets_0755_back() {
    let mut w = World::new("undoexec");
    let n = w.turn(|w| w.write("run.sh", b"#!/bin/sh\necho hi\n", 0o755));
    let applied = w.revert_file(n, "run.sh").unwrap();
    assert!(!w.file("run.sh").exists());
    assert!(
        matches!(applied.pre, Saved::Regular { mode: 0o755, .. }),
        "{:?}",
        applied.pre
    );
    w.undo(&applied).unwrap();
    assert_eq!(w.read("run.sh"), b"#!/bin/sh\necho hi\n");
    assert_eq!(mode_of(&w.file("run.sh")), 0o755);
}

#[test]
fn undo_of_a_symlink_turned_regular() {
    let mut w = World::new("undolinkreg");
    w.write("target.txt", b"the target\n", 0o644);
    std::os::unix::fs::symlink("target.txt", w.file("link")).unwrap();
    let n = w.turn(|w| {
        std::fs::remove_file(w.file("link")).unwrap();
        w.write("link", b"regular now\n", 0o644);
    });
    let target = fingerprint(&w.file("target.txt"));
    let applied = w.revert_file(n, "link").unwrap();
    assert_eq!(std::fs::read_link(w.file("link")).unwrap(), Path::new("target.txt"));
    assert!(matches!(applied.post, Saved::Symlink { .. }));
    w.undo(&applied).unwrap();
    assert!(meta(&w.file("link")).file_type().is_file());
    assert_eq!(w.read("link"), b"regular now\n");
    assert_eq!(mode_of(&w.file("link")), 0o644);
    assert_eq!(fingerprint(&w.file("target.txt")), target);
}

#[test]
fn undo_of_a_regular_turned_symlink() {
    let mut w = World::new("undoreglink");
    w.write("target.txt", b"the target\n", 0o644);
    w.write("thing", b"regular\n", 0o644);
    let n = w.turn(|w| {
        std::fs::remove_file(w.file("thing")).unwrap();
        std::os::unix::fs::symlink("target.txt", w.file("thing")).unwrap();
    });
    let target = fingerprint(&w.file("target.txt"));
    let applied = w.revert_file(n, "thing").unwrap();
    assert!(meta(&w.file("thing")).file_type().is_file());
    assert_eq!(w.read("thing"), b"regular\n");
    assert_eq!(mode_of(&w.file("thing")), 0o644);
    w.undo(&applied).unwrap();
    assert_eq!(std::fs::read_link(w.file("thing")).unwrap(), Path::new("target.txt"));
    assert_eq!(fingerprint(&w.file("target.txt")), target);
}

#[test]
fn undo_of_a_mode_only_whole_file_revert() {
    let mut w = World::new("undomode");
    w.write("tool", b"#!/bin/sh\n", 0o644);
    let n = w.turn(|w| std::fs::set_permissions(w.file("tool"), std::fs::Permissions::from_mode(0o755)).unwrap());
    let applied = w.revert_file(n, "tool").unwrap();
    assert_eq!(mode_of(&w.file("tool")), 0o644);
    w.undo(&applied).unwrap();
    assert_eq!(mode_of(&w.file("tool")), 0o755);
    assert_eq!(w.read("tool"), b"#!/bin/sh\n");
}

// ---- comments' anchors -----------------------------------------------------------------------------

#[test]
fn an_anchor_quotes_the_end_lines() {
    let mut w = World::new("anchor");
    w.write("a.txt", b"one\n", 0o644);
    let n = w.turn(|w| w.write("a.txt", b"one\r\ntwo \xff\nthree", 0o644));
    let lines = w
        .review
        .anchor_job(SESSION, n, Scope::Turn, "a.txt", 1, 3)
        .run()
        .unwrap();
    assert_eq!(lines, vec!["one", "two \u{FFFD}", "three"]);
    assert!(w
        .review
        .anchor_job(SESSION, n, Scope::Turn, "a.txt", 2, 4)
        .run()
        .is_err());
    assert!(w
        .review
        .anchor_job(SESSION, n, Scope::Turn, "a.txt", 0, 1)
        .run()
        .is_err());
    assert!(w
        .review
        .anchor_job(SESSION, n, Scope::Turn, "a.txt", 1, 200)
        .run()
        .is_err());
}

// ---- the editor ------------------------------------------------------------------------------------

/// An editor that answers every question with `answer`, or never (`None`), keeping each
/// unanswered question alive: a dropped one would read as a closed connection, not as silence.
struct FakeEditor {
    answer: Option<Value>,
    connected: bool,
    asked: RefCell<Vec<(&'static str, Vec<Value>)>>,
    unanswered: RefCell<Vec<Answer>>,
}

impl FakeEditor {
    fn new(answer: Option<Value>) -> FakeEditor {
        FakeEditor {
            answer,
            connected: true,
            asked: RefCell::new(Vec::new()),
            unanswered: RefCell::new(Vec::new()),
        }
    }
}

impl EditorRpc for FakeEditor {
    fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending {
        self.asked.borrow_mut().push((code, args));
        let (answer, pending) = Pending::pair();
        match &self.answer {
            Some(value) => answer.send(Ok(value.clone())),
            None => self.unanswered.borrow_mut().push(answer),
        }
        pending
    }

    fn target(&self) -> Option<u64> {
        self.connected.then_some(7)
    }
}

fn buffer(found: bool, modified: bool) -> Value {
    Value::Map(vec![
        (Value::from("found"), Value::from(found)),
        (Value::from("modified"), Value::from(modified)),
    ])
}

fn editor_refusal(result: Option<Result<EditorClear, Refusal>>) -> String {
    match result {
        Some(Err(Refusal::Editor(why))) => why,
        other => panic!("expected an editor refusal, got {other:?}"),
    }
}

#[test]
fn editor_check_refuses_with_no_link_modified_or_silence() {
    let path = Path::new("/project/src/it's 100%.rs");
    let start = Instant::now();

    let why = editor_refusal(EditorCheck::start(None, path, start).poll(start));
    assert!(
        why.starts_with("no editor is connected, so Eitri cannot tell whether"),
        "{why}"
    );
    assert!(why.contains("it's 100%.rs"));
    let mut gone = FakeEditor::new(Some(buffer(false, false)));
    gone.connected = false;
    let why = editor_refusal(EditorCheck::start(Some(&gone), path, start).poll(start));
    assert!(why.starts_with("no editor is connected"), "{why}");
    assert!(
        gone.asked.borrow().is_empty(),
        "nothing is sent with no editor behind the handle"
    );

    let modified = FakeEditor::new(Some(buffer(true, true)));
    let mut check = EditorCheck::start(Some(&modified), path, start);
    let why = editor_refusal(check.poll(start));
    assert!(
        why.ends_with("has unsaved changes in the editor; write or discard them first"),
        "{why}"
    );
    assert!(check.poll(start).is_none(), "the answer is given once");
    let asked = modified.asked.borrow();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].0, BUFFER_STATE_LUA);
    assert_eq!(
        asked[0].1[0],
        Value::from(path.to_str().unwrap()),
        "the path is an argument"
    );
    assert_eq!(asked[0].1[1], Value::Nil);

    // An answer that says modified without saying found is still a modified buffer.
    let modified_only = FakeEditor::new(Some(Value::Map(vec![(Value::from("modified"), Value::from(true))])));
    let why = editor_refusal(EditorCheck::start(Some(&modified_only), path, start).poll(start));
    assert!(
        why.ends_with("has unsaved changes in the editor; write or discard them first"),
        "{why}"
    );

    let silent = FakeEditor::new(None);
    let mut check = EditorCheck::start(Some(&silent), path, start);
    assert!(check.poll(start + Duration::from_millis(4900)).is_none());
    let why = editor_refusal(check.poll(start + Duration::from_secs(5)));
    assert_eq!(why, "the editor did not answer; is it waiting for a key?");

    for answer in [buffer(true, false), buffer(false, false)] {
        let clean = FakeEditor::new(Some(answer));
        let clear = EditorCheck::start(Some(&clean), path, start)
            .poll(start)
            .unwrap()
            .unwrap();
        assert_eq!(clear.path(), path);
    }
}

// ---- recovery --------------------------------------------------------------------------------------

/// The size of the crash child's new content: over one write chunk, so the abort leaves a file
/// that is genuinely half written.
const CRASH_NEW_LEN: usize = 200 * 1024;

/// The child of the recovery tests: an in-place rewrite of a hard-linked file that dies after its
/// first chunk, as a crash of a window in the middle of a revert would.
#[test]
#[ignore = "run by the recovery tests"]
fn revert_crash_child() {
    let (Some(dir), Some(rel)) = (std::env::var_os(CHILD_DIR), std::env::var_os(CHILD_REL)) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let rel = PathBuf::from(rel);
    let project = project_of(&dir);
    let review_dir = review_dir_of(&dir);
    let shadow = Shadow::open_with_excludes(&review_dir, &project, None).unwrap();
    let journal = Journal::open(&review_dir).unwrap();
    let pre = std::fs::read(project.join(&rel)).unwrap();
    let new = noise(CRASH_NEW_LEN, 3);
    let note = JournalNote {
        session: SESSION.into(),
        path: rel.clone(),
        pre: Some(shadow.store_blob(&pre).unwrap()),
        pre_mode: mode_of(&project.join(&rel)),
        intended: Some(shadow.store_blob(&new).unwrap()),
    };
    let no_core = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `no_core` is a valid `rlimit`; no dump is wanted from the abort below.
    unsafe { libc::setrlimit(libc::RLIMIT_CORE, &no_core) };
    let hooks = FsHooks {
        fault: Some(Arc::new(|stage| {
            if matches!(stage, Stage::Wrote(n) if n >= 64 * 1024) {
                std::process::abort();
            }
            Ok(())
        })),
        ..FsHooks::default()
    };
    let target = ProjectDir::open(&project).unwrap().target(&rel, false).unwrap();
    let content = Content::Bytes {
        bytes: new,
        mode: 0o644,
    };
    let _ = replace(&target, &content, &note, &journal, &shadow, &hooks);
    unreachable!("the write was to abort");
}

/// Kills an in-place write of `rel` halfway (the file gets a second link outside the project, so
/// it is rewritten in place) and returns the journal entry it left.
fn crash_mid_write(w: &World, rel: &str) -> JournalEntry {
    static LINKS: AtomicUsize = AtomicUsize::new(0);
    let link = w
        .dir()
        .join(format!("second-link-{}", LINKS.fetch_add(1, Ordering::Relaxed)));
    std::fs::hard_link(w.file(rel), link).unwrap();
    let original = w.read(rel);
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "revert_crash_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_DIR, w.dir())
        .env(CHILD_REL, rel)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the crash child did not finish in 30 s");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(!status.success(), "the child was to die");
    assert_ne!(w.read(rel), original, "the file is half written");
    let entry = w
        .pending_of(1)
        .into_iter()
        .find(|e| e.path == Path::new(rel))
        .expect("the crash left an entry");
    assert_eq!(entry.session, SESSION);
    entry
}

#[test]
fn recover_restores_and_keeps_the_current_bytes() {
    let w = World::new("recover");
    let original = b"the original\r\nbytes \xff\n".to_vec();
    w.write("a.txt", &original, 0o640);
    let entry = crash_mid_write(&w, "a.txt");
    let half = w.read("a.txt");
    // Asked again for a moment, for the same reason as `pending_of`.
    let deadline = Instant::now() + Duration::from_secs(1);
    let offered = loop {
        let offered = w.review.recoveries_job().run();
        if !offered.is_empty() || Instant::now() > deadline {
            break offered;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(offered, vec![entry.clone()]);

    w.recover(&entry).unwrap();
    assert_eq!(w.read("a.txt"), original);
    assert_eq!(mode_of(&w.file("a.txt")), 0o640);
    assert!(w.pending().is_empty());
    assert!(w.refs("refs/eitri-journal/").is_empty(), "no pin is left");
    let anchors = w.refs("refs/eitri-revert/");
    let kept = anchors
        .iter()
        .find(|r| r.ends_with("/pre"))
        .expect("the crashed bytes are anchored");
    assert!(kept.starts_with(&format!("refs/eitri-revert/{SESSION}/")), "{kept}");
    assert_eq!(w.ref_bytes(kept), half);

    // Taken once: a second restore of the same entry is refused.
    let refused = w.recover(&entry).unwrap_err();
    assert_eq!(
        refused,
        Refusal::Unavailable("this interrupted revert was already handled".into())
    );
}

#[test]
fn dismiss_only_forgets() {
    let w = World::new("dismiss");
    w.write("a.txt", b"original\n", 0o644);
    let entry = crash_mid_write(&w, "a.txt");
    let before = fingerprint(&w.file("a.txt"));
    // Even with a turn running here: forgetting writes nothing.
    let busy = PresenceGuard::for_tests(&w.review_dir, std::process::id(), 1);
    w.review
        .recover_job(busy, &entry.id, RecoverAnswer::Dismiss)
        .run()
        .unwrap();
    assert!(w.pending().is_empty());
    assert!(w.refs("refs/eitri-journal/").is_empty());
    assert!(w.refs("refs/eitri-revert/").is_empty());
    assert_eq!(fingerprint(&w.file("a.txt")), before);
}

#[test]
fn recover_is_refused_while_a_turn_runs_anywhere() {
    let w = World::new("recoverbusy");
    w.write("a.txt", b"original\n", 0o644);
    let entry = crash_mid_write(&w, "a.txt");
    let before = fingerprint(&w.file("a.txt"));
    let restore = |guard: PresenceGuard| {
        w.review
            .recover_job(guard, &entry.id, RecoverAnswer::Restore(w.clear("a.txt")))
            .run()
    };

    let here = PresenceGuard::for_tests(&w.review_dir, std::process::id(), 1);
    assert_refused(restore(here), &Refusal::Blocked(Blocked::TurnRunningHere));
    let mut other = Presence::open(&w.review_dir, OTHER_PID).unwrap();
    other.turn_started(1).unwrap();
    assert_refused(
        restore(w.guard()),
        &Refusal::Blocked(Blocked::Busy(Busy::TurnRunning { pid: OTHER_PID })),
    );
    drop(other);
    assert_eq!(fingerprint(&w.file("a.txt")), before);
    assert_eq!(w.pending_of(1), vec![entry.clone()], "the entry is still pending");
    assert!(w.refs("refs/eitri-revert/").is_empty());
}

#[test]
fn recovery_survives_trim_and_gc_past_max_age() {
    let w = World::new("recovertrim");
    let original = b"kept for a month\n".to_vec();
    w.write("a.txt", &original, 0o644);
    let entry = crash_mid_write(&w, "a.txt");
    let month = 31 * 24 * 3600 * 1000;
    w.shadow()
        .trim(0, Duration::from_millis(1), entry.at_ms + month, "now")
        .unwrap();
    w.recover(&entry).unwrap();
    assert_eq!(w.read("a.txt"), original);
}

// ---- the project's own git -------------------------------------------------------------------------

/// What the project's own git says about itself, none of which a revert may change.
fn git_state(project: &Path) -> Vec<Vec<u8>> {
    let index = project.join(".git/index");
    let index_meta = meta(&index);
    vec![
        tgit(project, &["for-each-ref"]).stdout,
        std::fs::read(&index).unwrap(),
        format!("{} {}", index_meta.mtime(), index_meta.mtime_nsec()).into_bytes(),
        std::fs::read(project.join(".git/HEAD")).unwrap(),
        tgit(project, &["rev-parse", "HEAD"]).stdout,
        tgit(project, &["stash", "list"]).stdout,
        tgit(project, &["count-objects", "-v"]).stdout,
    ]
}

#[test]
fn the_projects_git_is_never_written_by_revert_undo_or_recover() {
    let mut w = World::new("projgit");
    let p = w.project.clone();
    tgit(&p, &["init", "-q"]);
    w.write("tracked.txt", b"v1\n", 0o644);
    w.write("linked.txt", b"linked\n", 0o644);
    tgit(&p, &["add", "-A"]);
    tgit(&p, &["commit", "-q", "-m", "one"]);
    w.write("tracked.txt", b"stashed\n", 0o644);
    tgit(&p, &["stash", "-q"]);
    let n = w.turn(|w| w.write("tracked.txt", b"v2\n", 0o644));
    let before = git_state(&p);

    let applied = w.revert_file(n, "tracked.txt").unwrap();
    assert_eq!(w.read("tracked.txt"), b"v1\n");
    w.undo(&applied).unwrap();
    assert_eq!(w.read("tracked.txt"), b"v2\n");
    let entry = crash_mid_write(&w, "linked.txt");
    w.recover(&entry).unwrap();
    assert_eq!(w.read("linked.txt"), b"linked\n");

    assert_eq!(git_state(&p), before);
}
