//! Turn review's revert engine against real git and real files: turns scripted through a
//! `TurnReview` take real snapshots of a project under the system's temporary directory, and every
//! revert, undo and recovery writes only inside it.
//!
//! The recovery tests re-run this binary as a child whose in-place write dies halfway. The child
//! test is `#[ignore]`d and does nothing unless the parent set its directory variable, so
//! `-- --ignored` over this target is harmless.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use agent::{AgentDomainEvent, TurnOutcome};
use eitri_core::editor_rpc::EditorRpc;
use eitri_core::nvim_rpc::{Answer, Pending};
use eitri_core::turn_review::{
    replace, review_dir_for, AppliedRevert, Blocked, Busy, Content, EditorCheck, EditorClear, EmbeddedAsk, ExactHunks,
    FsHooks, GroupGuard, Journal, JournalEntry, JournalNote, Limits, Presence, PresenceGuard, ProjectDir,
    RecoverAnswer, Recovered, Refusal, RevertKind, RevertTarget, ReviewOptions, Saved, Scope, Shadow, Snap, Stage,
    TurnReview, WriteError, WriteSlot, BUFFER_STATE_LUA, MAX_CLEAR_AGE, NOT_LINKED,
};
use rmpv::Value;

const CHILD_DIR: &str = "EITRI_REVERT_CHILD_DIR";
const CHILD_REL: &str = "EITRI_REVERT_CHILD_REL";
const SESSION: &str = "s1";
const TAB: u64 = 1;
/// A pid no real window here has.
const OTHER_PID: u32 = 999_999;

/// A directory of the test's own under the system's temporary directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        // The system's temporary directory, not the target dir: these tests need user extended
        // attributes (the in-place write), git's fsmonitor and a disk fast enough for the snapshot
        // budget, and a checkout on a network mount has none of the three. A fresh directory of
        // this user's own holds it: a random name made here at 0700, never one already in place in
        // a shared directory, set exactly so whatever the umask, and removed on drop.
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let dir = std::env::temp_dir().join(format!(
            "eitri-turn_review_revert-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        Scratch(dir.canonicalize().unwrap())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // A snapshot or journal worker a test started can still write here for a moment after the
        // test's last line (they are not joined), and a removal racing it leaves part of the tree,
        // or the worker makes it again. So the directory must stay gone for a while, up to 5 s.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut absent_since = None;
        while std::time::Instant::now() < deadline {
            if self.0.exists() {
                let _ = std::fs::remove_dir_all(&self.0);
                absent_since = None;
            } else if absent_since.get_or_insert_with(std::time::Instant::now).elapsed()
                > std::time::Duration::from_millis(100)
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

/// This process's umask, from `/proc/self/status` (setting it to read it would race the other tests).
fn umask() -> u32 {
    let status = std::fs::read_to_string("/proc/self/status").expect("/proc/self/status");
    let value = status
        .lines()
        .find_map(|line| line.strip_prefix("Umask:"))
        .expect("a Umask line");
    u32::from_str_radix(value.trim(), 8).expect("an octal umask")
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
        Self::with_excludes(name, limits, Some(None))
    }

    fn with_excludes(name: &str, limits: Limits, excludes: Option<Option<PathBuf>>) -> World {
        let scratch = Scratch::new(name);
        let project = project_of(&scratch.0);
        std::fs::create_dir_all(&project).unwrap();
        let review_dir = review_dir_of(&scratch.0);
        let options = ReviewOptions {
            limits,
            excludes,
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

    fn recover(&self, entry: &JournalEntry) -> Result<Recovered, Refusal> {
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
            let pending = journal.pending().unwrap().entries;
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

#[test]
fn a_file_exposed_by_changed_ignore_rules_is_not_deleted_but_keeps_its_view() {
    for ignore in [".gitignore", "sub/.gitignore"] {
        let mut w = World::new("ignore-change");
        w.write(ignore, b"config.local\n", 0o644);
        w.write("sub/config.local", b"pre-existing secret\n", 0o600);
        let n = w.turn(|w| w.write(ignore, b"", 0o644));
        // Restoring the live rules must not hide the change between the two snapshots.
        w.write(ignore, b"config.local\n", 0o644);
        for scope in [Scope::Turn, Scope::Session] {
            let result = w.revert_in(n, scope, "sub/config.local", RevertTarget::File);
            assert!(
                matches!(result, Err(Refusal::Unavailable(ref why)) if why.contains("ignore rules")),
                "{result:?}"
            );
            assert_eq!(w.read("sub/config.local"), b"pre-existing secret\n");
            let diff = w.review.diff_job(SESSION, n, scope, "sub/config.local").run();
            assert!(diff.is_ok(), "{diff:?}");
        }
    }
}

#[test]
fn an_info_exclude_change_never_authorizes_deletion() {
    let mut w = World::new("info-exclude");
    tgit(&w.project, &["init", "-q"]);
    w.write(".git/info/exclude", b"config.local\nuntouched.local\n", 0o644);
    w.write("config.local", b"pre-existing\n", 0o600);
    w.write("untouched.local", b"untouched\n", 0o600);
    let n = w.turn(|w| {
        w.write(".git/info/exclude", b"", 0o644);
        // Even a replacement inode cannot bypass the exclude-source guard.
        w.write("config.local", b"rewritten pre-existing\n", 0o600);
        w.write("new.txt", b"created\n", 0o644);
    });
    for rel in ["config.local", "untouched.local", "new.txt"] {
        for scope in [Scope::Turn, Scope::Session] {
            assert!(matches!(
                w.revert_in(n, scope, rel, RevertTarget::File),
                Err(Refusal::Unavailable(_))
            ));
            assert!(w.file(rel).exists());
            assert!(w.review.diff_job(SESSION, n, scope, rel).run().is_ok());
        }
    }
}

#[test]
fn changed_global_excludes_are_recorded_per_snapshot() {
    for mode in ["default", "configured", "switch", "after-switch"] {
        let scratch = Scratch::new("global-excludes");
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "global_excludes_child"])
            .env("EITRI_EXCLUDES_CHILD", mode)
            .env("HOME", &scratch.0)
            .env("XDG_CONFIG_HOME", scratch.0.join(".config"))
            .env("GIT_CONFIG_GLOBAL", scratch.0.join(".gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
#[ignore = "child with private HOME, launched by changed_global_excludes_are_recorded_per_snapshot"]
fn global_excludes_child() {
    let Ok(mode) = std::env::var("EITRI_EXCLUDES_CHILD") else {
        return;
    };
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let global = if mode == "default" {
        home.join(".config/git/ignore")
    } else {
        home.join("ignore")
    };
    write_file(&global, b"old.txt\n", 0o600);
    let config = home.join(".gitconfig");
    if mode != "default" {
        std::fs::write(&config, format!("[core]\nexcludesFile = {}\n", global.display())).unwrap();
    }
    let mut w = World::with_excludes("global-excludes-child", Limits::default(), None);
    if mode == "after-switch" {
        w.write(".gitignore", b"ignored\n", 0o644);
        let n = w.turn(|w| {
            w.write(".gitignore", b"other\n", 0o644);
            w.write("new.txt", b"created\n", 0o644);
        });
        let replacement = home.join("other-ignore");
        // Identical bytes still leave the check using a different external source.
        std::fs::write(&replacement, b"old.txt\n").unwrap();
        std::fs::write(&config, format!("[core]\nexcludesFile = {}\n", replacement.display())).unwrap();
        assert!(matches!(w.revert_file(n, "new.txt"), Err(Refusal::Unavailable(_))));
        assert_eq!(w.read("new.txt"), b"created\n");
        return;
    }
    w.write("old.txt", b"old\n", 0o600);
    let n = w.turn(|w| {
        if mode == "switch" {
            let replacement = home.join("other-ignore");
            std::fs::write(&replacement, b"").unwrap();
            std::fs::write(&config, format!("[core]\nexcludesFile = {}\n", replacement.display())).unwrap();
        } else {
            std::fs::write(&global, b"").unwrap();
        }
        w.write("old.txt", b"rewritten old\n", 0o600);
        assert!(
            w.review
                .diff_job(SESSION, 1, Scope::Turn, "old.txt")
                .run()
                .unwrap()
                .new_file
        );
    });
    assert!(
        w.review
            .diff_job(SESSION, n, Scope::Turn, "old.txt")
            .run()
            .unwrap()
            .new_file
    );
    assert!(matches!(w.revert_file(n, "old.txt"), Err(Refusal::Unavailable(_))));
    assert_eq!(w.read("old.txt"), b"rewritten old\n");
}

#[test]
fn oversized_recovery_is_unavailable_and_keeps_the_journal() {
    let w = World::new("large-recovery");
    w.write("a.txt", b"before\n", 0o644);
    let entry = crash_mid_write(&w, "a.txt");
    std::fs::OpenOptions::new()
        .write(true)
        .open(w.file("a.txt"))
        .unwrap()
        .set_len(1 << 30)
        .unwrap();
    assert_refused(
        w.recover(&entry),
        &Refusal::Unavailable("too large to restore here".into()),
    );
    assert_eq!(w.pending_of(1).len(), 1);
    assert_eq!(meta(&w.file("a.txt")).len(), 1 << 30);
}

fn has_birth_time(path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    let supported = {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let mut st = std::mem::MaybeUninit::<libc::statx>::zeroed();
        // SAFETY: path is a valid C string and st is writable statx storage.
        unsafe {
            libc::statx(
                libc::AT_FDCWD,
                path.as_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
                libc::STATX_BTIME,
                st.as_mut_ptr(),
            ) == 0
                && st.assume_init().stx_mask & libc::STATX_BTIME != 0
        }
    };
    #[cfg(not(target_os = "linux"))]
    let supported = std::fs::symlink_metadata(path).unwrap().created().is_ok();
    if !supported {
        eprintln!("skipping birth-time-dependent test: scratch filesystem has no birth time (STATX_BTIME on Linux)");
    }
    supported
}

#[test]
fn rename_rewritten_file_exposed_by_root_ignore_is_not_deleted() {
    let mut w = World::new("rename-root-ignore");
    w.write(".gitignore", b".env\n", 0o644);
    w.write(".env", b"old\n", 0o600);
    let n = w.turn(|w| {
        w.write(".gitignore", b"", 0o644);
        w.write("temp", b"rewritten\n", 0o600);
        std::fs::rename(w.file("temp"), w.file(".env")).unwrap();
    });
    for scope in [Scope::Turn, Scope::Session] {
        assert!(matches!(
            w.revert_in(n, scope, ".env", RevertTarget::File),
            Err(Refusal::Unavailable(_))
        ));
        assert_eq!(w.read(".env"), b"rewritten\n");
    }
}

#[test]
fn rename_rewritten_file_exposed_by_self_ignore_is_not_deleted() {
    let mut w = World::new("rename-self-ignore");
    w.write(".venv/.gitignore", b"*\n", 0o644);
    w.write(".venv/x", b"old\n", 0o600);
    let n = w.turn(|w| {
        std::fs::remove_file(w.file(".venv/.gitignore")).unwrap();
        w.write(".venv/temp", b"rewritten\n", 0o600);
        std::fs::rename(w.file(".venv/temp"), w.file(".venv/x")).unwrap();
    });
    assert!(matches!(w.revert_file(n, ".venv/x"), Err(Refusal::Unavailable(_))));
    assert_eq!(w.read(".venv/x"), b"rewritten\n");
}

fn assert_omitted_ignore_refuses_dataset_deletion(mode: &str) {
    let limits = if mode == "large" {
        Limits {
            max_file_bytes: 64,
            ..Limits::default()
        }
    } else {
        Limits::default()
    };
    let mut w = World::with_excludes("omitted-ignore", limits, Some(None));
    tgit(&w.project, &["init", "-q"]);
    let ignore = if mode == "root" {
        ".gitignore"
    } else {
        "data/.gitignore"
    };
    let dataset = if mode == "root" { "train.csv" } else { "data/train.csv" };
    let readme = if mode == "root" { "README.md" } else { "data/README.md" };
    let rules = if mode == "large" {
        format!("train.csv\n#{}\n", "padding".repeat(20))
    } else {
        "*\n!README.md\n".to_owned()
    };
    if mode == "parent" {
        w.write(".gitignore", b"data/.gitignore\n", 0o644);
    }
    w.write(ignore, rules.as_bytes(), 0o644);
    w.write(readme, b"tracked\n", 0o644);
    w.write(dataset, b"old dataset\n", 0o600);
    tgit(&w.project, &["add", "-f", ignore, readme]);
    let n = w.turn(|w| {
        if mode == "rewrite" {
            w.write(ignore, b"", 0o644);
        } else {
            std::fs::remove_file(w.file(ignore)).unwrap();
        }
        w.write("temp", b"edited dataset\n", 0o600);
        std::fs::rename(w.file("temp"), w.file(dataset)).unwrap();
    });
    for scope in [Scope::Turn, Scope::Session] {
        let result = w.revert_in(n, scope, dataset, RevertTarget::File);
        assert!(matches!(result, Err(Refusal::Unavailable(_))), "{mode}: {result:?}");
        assert_eq!(w.read(dataset), b"edited dataset\n");
        assert!(w.review.diff_job(SESSION, n, scope, dataset).run().unwrap().new_file);
    }
}

#[test]
fn omitted_ignore_remove_does_not_authorize_dataset_deletion() {
    assert_omitted_ignore_refuses_dataset_deletion("remove");
}

#[test]
fn omitted_ignore_rewrite_does_not_authorize_dataset_deletion() {
    assert_omitted_ignore_refuses_dataset_deletion("rewrite");
}

#[test]
fn omitted_ignore_root_does_not_authorize_dataset_deletion() {
    assert_omitted_ignore_refuses_dataset_deletion("root");
}

#[test]
fn omitted_ignore_parent_does_not_authorize_dataset_deletion() {
    assert_omitted_ignore_refuses_dataset_deletion("parent");
}

#[test]
fn omitted_ignore_large_does_not_authorize_dataset_deletion() {
    assert_omitted_ignore_refuses_dataset_deletion("large");
}

#[test]
fn an_old_directory_moved_over_a_base_symlink_does_not_authorize_deletion() {
    let mut w = World::new("symlink-ancestor");
    w.write(".gitignore", b".cache/\n", 0o644);
    w.write(".cache/real/x", b"old\n", 0o644);
    std::os::unix::fs::symlink(".cache/real", w.file("link")).unwrap();
    let n = w.turn(|w| {
        std::fs::remove_file(w.file("link")).unwrap();
        std::fs::rename(w.file(".cache/real"), w.file("link")).unwrap();
        w.write("temp", b"rewritten\n", 0o644);
        std::fs::rename(w.file("temp"), w.file("link/x")).unwrap();
    });
    let result = w.revert_file(n, "link/x");
    assert!(matches!(result, Err(Refusal::Unavailable(_))), "{result:?}");
    assert_eq!(w.read("link/x"), b"rewritten\n");
}

#[test]
fn excludes_changed_after_the_end_refuse_deletion_with_changed_ancestor_rules() {
    for source in ["info", "global"] {
        let external = Scratch::new("later-excludes");
        let global = external.0.join("ignore");
        std::fs::write(&global, b"").unwrap();
        let mut w = World::with_excludes("later-excludes", Limits::default(), Some(Some(global.clone())));
        tgit(&w.project, &["init", "-q"]);
        w.write(".gitignore", b"ignored\n", 0o644);
        let n = w.turn(|w| {
            w.write(".gitignore", b"other\n", 0o644);
            w.write("new.txt", b"created\n", 0o644);
        });
        if source == "info" {
            w.write(".git/info/exclude", b"different\n", 0o644);
            w.shadow().sync_excludes().unwrap();
        } else {
            std::fs::write(&global, b"different\n").unwrap();
        }
        let result = w.revert_file(n, "new.txt");
        assert!(matches!(result, Err(Refusal::Unavailable(_))), "{source}: {result:?}");
        assert_eq!(w.read("new.txt"), b"created\n");
    }
}

#[test]
fn new_files_beside_ignored_files_are_deleted() {
    for (old, new, rule) in [
        ("logs/a.log", "logs/new.txt", "*.log\n"),
        ("pkg/__pycache__/stale.pyc", "pkg/__init__.py", "__pycache__/\n"),
    ] {
        let mut w = World::new("new-beside-ignored");
        w.write(".gitignore", rule.as_bytes(), 0o644);
        w.write(old, b"old\n", 0o644);
        let n = w.turn(|w| w.write(new, b"created\n", 0o644));
        assert_eq!(w.revert_file(n, new).unwrap().kind, RevertKind::Delete);
        assert!(!w.file(new).exists());
        assert_eq!(w.read(old), b"old\n");
    }
}

#[test]
fn new_files_in_new_and_tracked_directories_are_deleted() {
    let mut w = World::new("new-directories");
    if !has_birth_time(&w.project) {
        return;
    }
    w.write("tracked/kept", b"base\n", 0o644);
    let n = w.turn(|w| {
        w.write("new/nested/x", b"created\n", 0o644);
        w.write("tracked/x", b"created\n", 0o644);
    });
    for rel in ["new/nested/x", "tracked/x"] {
        assert_eq!(w.revert_file(n, rel).unwrap().kind, RevertKind::Delete);
        assert!(!w.file(rel).exists());
    }
    assert_eq!(w.read("tracked/kept"), b"base\n");
}

#[test]
fn undo_with_a_pruned_post_blob_and_changed_kind_is_changed() {
    let mut w = World::new("pruned-undo-kind");
    w.write("a", b"base\n", 0o644);
    let n = w.turn(|w| w.write("a", b"end\n", 0o644));
    let mut applied = w.revert_file(n, "a").unwrap();
    // A missing object stands for a blob retention has pruned.
    applied.post = Saved::Regular {
        blob: "0".repeat(40),
        mode: 0o644,
    };
    std::fs::remove_file(w.file("a")).unwrap();
    assert_refused(w.undo(&applied), &Refusal::ChangedSinceRevert);
    std::os::unix::fs::symlink("elsewhere", w.file("a")).unwrap();
    assert_refused(w.undo(&applied), &Refusal::ChangedSinceRevert);
    applied.post = Saved::Symlink { blob: "0".repeat(40) };
    std::fs::remove_file(w.file("a")).unwrap();
    w.write("a", b"new regular\n", 0o644);
    assert_refused(w.undo(&applied), &Refusal::ChangedSinceRevert);
}

#[test]
fn oversized_live_anchor_is_too_large() {
    let mut w = World::new("large-live-anchor");
    w.write("a", b"base\n", 0o644);
    let n = w.start_turn();
    std::fs::OpenOptions::new()
        .write(true)
        .open(w.file("a"))
        .unwrap()
        .set_len(1 << 30)
        .unwrap();
    assert!(matches!(w.review.anchor_job(SESSION, n, Scope::Turn, "a", 1, 1).run(),
        Err(eitri_core::turn_review::ReviewError::TooLarge(path)) if path == Path::new("a")));
    w.end_turn();
}

#[test]
fn deleting_a_self_ignoring_gitignore_does_not_delete_the_old_file() {
    let mut w = World::new("self-ignore");
    if !has_birth_time(&w.project) {
        return;
    }
    w.write(".venv/.gitignore", b"*\n", 0o644);
    w.write(".venv/x", b"old bytes\n", 0o644);
    let n = w.turn(|w| {
        std::fs::remove_file(w.file(".venv/.gitignore")).unwrap();
        // Modification time is new, but birth time still identifies the old file.
        std::fs::write(w.file(".venv/x"), b"edited old bytes\n").unwrap();
    });
    assert!(matches!(w.revert_file(n, ".venv/x"), Err(Refusal::Unavailable(_))));
    assert_eq!(w.read(".venv/x"), b"edited old bytes\n");
    assert!(
        w.review
            .diff_job(SESSION, n, Scope::Turn, ".venv/x")
            .run()
            .unwrap()
            .new_file
    );
}

#[test]
fn a_new_file_keeps_diff_hunks_anchors_and_revert_after_ignore_edits() {
    let mut w = World::new("new-with-ignore");
    if !has_birth_time(&w.project) {
        return;
    }
    w.write(".gitignore", b"ignored\n", 0o644);
    let n = w.turn(|w| {
        w.write(".gitignore", b"other\n", 0o644);
        w.write("new.txt", b"created\n", 0o644);
    });
    for scope in [Scope::Turn, Scope::Session] {
        assert!(w.review.diff_job(SESSION, n, scope, "new.txt").run().unwrap().new_file);
        assert!(!w
            .review
            .exact_hunks_job(SESSION, n, scope, "new.txt")
            .run()
            .unwrap()
            .hunks
            .unwrap()
            .is_empty());
        assert_eq!(
            w.review.anchor_job(SESSION, n, scope, "new.txt", 1, 1).run().unwrap(),
            vec!["created"]
        );
    }
    assert_eq!(w.revert_file(n, "new.txt").unwrap().kind, RevertKind::Delete);
    assert!(!w.file("new.txt").exists());
}

#[test]
fn old_snapshot_without_exclude_provenance_refuses_only_deletion() {
    for field in ["delete-provenance:", "delete-ignored:"] {
        let mut w = World::new("old-provenance");
        let n = w.turn(|w| w.write("new.txt", b"new\n", 0o644));
        let shadow = w.shadow();
        let reference = format!("refs/eitri/{SESSION}/{n}/base");
        let gitdir = format!("--git-dir={}", shadow.git_dir().display());
        let message = tgit(&w.project, &[&gitdir, "show", "-s", "--format=%B", &reference]);
        let message: String = String::from_utf8(message.stdout)
            .unwrap()
            .lines()
            .filter(|l| !l.starts_with(field))
            .map(|l| format!("{l}\n"))
            .collect();
        let tree = tgit(&w.project, &[&gitdir, "rev-parse", &format!("{reference}^{{tree}}")]);
        let tree = String::from_utf8(tree.stdout).unwrap();
        let commit = tgit(&w.project, &[&gitdir, "commit-tree", tree.trim(), "-m", &message]);
        let commit = String::from_utf8(commit.stdout).unwrap();
        tgit(&w.project, &[&gitdir, "update-ref", &reference, commit.trim()]);
        // Reopen so no in-memory record keeps the newer base commit.
        let review = TurnReview::with_options(
            Some(w.review_dir.clone()),
            &w.project,
            ReviewOptions {
                excludes: Some(None),
                ..ReviewOptions::default()
            },
        );
        let result = review
            .revert_job(
                w.guard(),
                wanted(),
                SESSION,
                n,
                Scope::Turn,
                "new.txt",
                RevertTarget::File,
                w.clear("new.txt"),
            )
            .run();
        assert!(matches!(result, Err(Refusal::Unavailable(_))), "{result:?}");
        assert!(review.diff_job(SESSION, n, Scope::Turn, "new.txt").run().is_ok());
        assert_eq!(w.read("new.txt"), b"new\n");
    }
}

#[test]
fn a_deleted_duplicate_block_is_not_reverted_in_the_surviving_block() {
    let mut w = World::new("duplicate-block");
    let block = b"a\nb\nc\nNEW\nd\ne\nf\n";
    w.write("a.txt", &[b"a\nb\nc\nOLD\nd\ne\nf\n".as_slice(), block].concat(), 0o644);
    let n = w.turn(|w| w.write("a.txt", &block.repeat(2), 0o644));
    let target = w.hunk(n, "a.txt", 0);
    w.write("a.txt", block, 0o644);
    assert_refused(w.revert(n, "a.txt", target), &Refusal::ChangedSinceEnd);
    assert_eq!(w.read("a.txt"), block);
}

#[test]
fn a_sparse_current_file_refuses_revert_undo_and_the_final_recheck() {
    let mut w = World::new("sparse-current");
    w.write("a.txt", b"before\n", 0o644);
    let n = w.turn(|w| w.write("a.txt", b"after\n", 0o644));
    let hunk = w.hunk(n, "a.txt", 0);
    let grow = |path: &Path| {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_len(1 << 30)
            .unwrap();
    };
    grow(&w.file("a.txt"));
    for target in [RevertTarget::File, hunk] {
        assert_refused(w.revert(n, "a.txt", target), &Refusal::ChangedSinceEnd);
        assert_eq!(meta(&w.file("a.txt")).len(), 1 << 30);
    }
    w.write("a.txt", b"after\n", 0o644);
    let applied = w.revert_file(n, "a.txt").unwrap();
    grow(&w.file("a.txt"));
    assert_refused(w.undo(&applied), &Refusal::ChangedSinceRevert);
    assert_eq!(meta(&w.file("a.txt")).len(), 1 << 30);
    w.write("a.txt", b"after\n", 0o644);
    let path = w.file("a.txt");
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
        .on_final_check(Arc::new(move || grow(&path)));
    assert_refused(job.run(), &Refusal::ChangedSinceEnd);
    assert_eq!(meta(&w.file("a.txt")).len(), 1 << 30);
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
fn a_hunk_shifted_by_an_insertion_reverts_at_its_new_line() {
    let mut w = World::new("moved");
    w.write("a.txt", &numbered(20).concat(), 0o644);
    let mut end = numbered(20);
    end[14] = b"changed\n".to_vec();
    let n = w.turn(|w| w.write("a.txt", &end.concat(), 0o644));
    let target = w.hunk(n, "a.txt", 0);
    // The region's bytes are all still there, one line further down.
    let cur = [b"a new first line\n".to_vec(), end.concat()].concat();
    w.write("a.txt", &cur, 0o644);
    let applied = w.revert(n, "a.txt", target).unwrap();
    assert_eq!(applied.at_line, 13);
    assert_eq!(
        w.read("a.txt"),
        [b"a new first line\n".to_vec(), numbered(20).concat()].concat()
    );
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
    assert_eq!(
        mode_of(&w.file("bin/run.sh")),
        0o755 & !umask(),
        "the base's mode, under the umask"
    );
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

/// Window 1 as a module window's guard hears it: the answers it will give, in order, and how often it was asked.
#[derive(Default)]
struct FakeWindow1 {
    answers: Mutex<VecDeque<Option<Result<(), String>>>>,
    asked: AtomicUsize,
}

impl FakeWindow1 {
    /// Each `Some` is an answer; `None` is no link to ask on.
    fn answering(answers: impl IntoIterator<Item = Option<Result<(), String>>>) -> Arc<FakeWindow1> {
        Arc::new(FakeWindow1 {
            answers: Mutex::new(answers.into_iter().collect()),
            asked: AtomicUsize::new(0),
        })
    }
}

impl GroupGuard for FakeWindow1 {
    fn ask(&self) -> Option<(u64, mpsc::Receiver<Result<(), String>>)> {
        let job = self.asked.fetch_add(1, Ordering::SeqCst) as u64;
        let answer = self
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .expect("no more than expected")?;
        let (tell, told) = mpsc::channel();
        tell.send(answer).unwrap();
        Some((job, told))
    }

    fn gave_up(&self, _job: u64) {}
}

fn module_window_guard(w: &World, window_1: &Arc<FakeWindow1>) -> PresenceGuard {
    let guard = w.guard();
    guard.set_group(window_1.clone());
    guard
}

#[test]
fn a_module_windows_revert_needs_window_1s_word_at_both_checks() {
    let mut w = World::new("group-both");
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

    let at_the_end = FakeWindow1::answering([Some(Ok(())), Some(Err("an agent turn is running".to_string()))]);
    let refused = revert(module_window_guard(&w, &at_the_end)).unwrap_err();
    assert_eq!(
        refused,
        Refusal::Blocked(Blocked::Group("an agent turn is running".to_string()))
    );
    assert_eq!(refused.to_string(), "an agent turn is running");
    assert_eq!(at_the_end.asked.load(Ordering::SeqCst), 2);
    assert_eq!(fingerprint(&w.file("a.txt")), before);

    let yes = FakeWindow1::answering([Some(Ok(())), Some(Ok(()))]);
    revert(module_window_guard(&w, &yes)).unwrap();
    assert_eq!(w.read("a.txt"), b"base\n");
}

#[test]
fn a_module_window_with_no_link_to_window_1_does_not_write() {
    let mut w = World::new("group-unlinked");
    w.write("a.txt", b"base\n", 0o644);
    let n = w.turn(|w| w.write("a.txt", b"end\n", 0o644));
    let before = fingerprint(&w.file("a.txt"));
    let unlinked = FakeWindow1::answering([None]);
    let refused = w
        .review
        .revert_job(
            module_window_guard(&w, &unlinked),
            wanted(),
            SESSION,
            n,
            Scope::Turn,
            "a.txt",
            RevertTarget::File,
            w.clear("a.txt"),
        )
        .run()
        .unwrap_err();
    assert_eq!(refused, Refusal::Blocked(Blocked::NotHeld(NOT_LINKED.to_string())));
    assert_eq!(fingerprint(&w.file("a.txt")), before);
}

#[test]
fn a_module_window_with_a_turn_of_its_own_refuses_before_asking() {
    let mut w = World::new("group-own");
    w.write("a.txt", b"base\n", 0o644);
    let n = w.turn(|w| w.write("a.txt", b"end\n", 0o644));
    let window_1 = FakeWindow1::answering([]);
    let busy = PresenceGuard::for_tests(&w.review_dir, std::process::id(), 1);
    busy.set_group(window_1.clone());
    let refused = w
        .review
        .revert_job(
            busy,
            wanted(),
            SESSION,
            n,
            Scope::Turn,
            "a.txt",
            RevertTarget::File,
            w.clear("a.txt"),
        )
        .run()
        .unwrap_err();
    assert_eq!(refused, Refusal::Blocked(Blocked::TurnRunningHere));
    assert_eq!(window_1.asked.load(Ordering::SeqCst), 0);
}

#[test]
fn a_module_windows_undo_and_recovery_ask_window_1_too() {
    let mut w = World::new("group-undo");
    w.write("a.txt", b"base\n", 0o644);
    let n = w.turn(|w| w.write("a.txt", b"end\n", 0o644));
    let applied = w.revert_file(n, "a.txt").unwrap();
    let refusal = || Refusal::Blocked(Blocked::Group("no".to_string()));

    let window_1 = FakeWindow1::answering([Some(Err("no".to_string()))]);
    let undo = w.review.undo_job(
        module_window_guard(&w, &window_1),
        wanted(),
        SESSION,
        "a.txt",
        applied.pre.clone(),
        applied.post.clone(),
        w.clear("a.txt"),
    );
    assert_refused(undo.run(), &refusal());
    assert_eq!(w.read("a.txt"), b"base\n");

    let w = World::new("group-recover");
    w.write("a.txt", b"original\n", 0o644);
    let entry = crash_mid_write(&w, "a.txt");
    let window_1 = FakeWindow1::answering([Some(Err("no".to_string()))]);
    let restore = w.review.recover_job(
        module_window_guard(&w, &window_1),
        &entry.id,
        RecoverAnswer::Restore(w.clear("a.txt")),
    );
    assert_refused(restore.run(), &refusal());
    assert_eq!(w.pending_of(1), vec![entry], "the entry is still pending");
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

/// The editor's word about unsaved changes holds only for a while: the buffer may be edited after
/// it answered, so a write whose clear is older than `MAX_CLEAR_AGE` refuses, for a revert, an
/// undo and a recovery alike, and writes nothing.
#[test]
fn an_old_clear_is_refused() {
    let mut w = World::new("oldclear");
    w.write("a.txt", b"base\n", 0o644);
    let n = w.turn(|w| w.write("a.txt", b"end\n", 0o644));
    let old = || {
        let answered = Instant::now()
            .checked_sub(MAX_CLEAR_AGE + Duration::from_secs(1))
            .expect("the clock is past the bound");
        EditorClear::answered_at_for_tests(&w.file("a.txt"), answered)
    };
    let stale = |result: Result<String, Refusal>| match result {
        Err(Refusal::Editor(why)) => assert!(why.contains("press the key again"), "{why}"),
        other => panic!("expected an editor refusal, got {other:?}"),
    };
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
            old(),
        )
        .run();
    stale(refused.map(|a| format!("{a:?}")));
    assert_eq!(w.read("a.txt"), b"end\n");

    let applied = w.revert_file(n, "a.txt").unwrap();
    let undo = w.review.undo_job(
        w.guard(),
        wanted(),
        SESSION,
        "a.txt",
        applied.pre.clone(),
        applied.post.clone(),
        old(),
    );
    stale(undo.run().map(|()| String::new()));
    assert_eq!(w.read("a.txt"), b"base\n");

    let entry = crash_mid_write(&w, "a.txt");
    let half = w.read("a.txt");
    let refused = w
        .review
        .recover_job(w.guard(), &entry.id, RecoverAnswer::Restore(old()))
        .run();
    stale(refused.map(|r| format!("{r:?}")));
    assert_eq!(w.read("a.txt"), half);
    assert_eq!(w.pending_of(1).len(), 1, "the entry is kept");

    // A clear given just now, as every other test's, still writes.
    assert!(matches!(w.recover(&entry), Ok(Recovered::Restored)));
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
    assert_eq!(
        mode_of(&w.file("thing")),
        0o644 & !umask(),
        "git's mode, under the umask"
    );
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

    let why = editor_refusal(EditorCheck::start(&EmbeddedAsk(None), path, start).poll(start));
    assert!(
        why.starts_with("no editor is connected, so Eitri cannot tell whether"),
        "{why}"
    );
    assert!(why.contains("it's 100%.rs"));
    let mut gone = FakeEditor::new(Some(buffer(false, false)));
    gone.connected = false;
    let why = editor_refusal(EditorCheck::start(&EmbeddedAsk(Some(&gone)), path, start).poll(start));
    assert!(why.starts_with("no editor is connected"), "{why}");
    assert!(
        gone.asked.borrow().is_empty(),
        "nothing is sent with no editor behind the handle"
    );

    let modified = FakeEditor::new(Some(buffer(true, true)));
    let mut check = EditorCheck::start(&EmbeddedAsk(Some(&modified)), path, start);
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
    let why = editor_refusal(EditorCheck::start(&EmbeddedAsk(Some(&modified_only)), path, start).poll(start));
    assert!(
        why.ends_with("has unsaved changes in the editor; write or discard them first"),
        "{why}"
    );

    let silent = FakeEditor::new(None);
    let mut check = EditorCheck::start(&EmbeddedAsk(Some(&silent)), path, start);
    assert!(check.poll(start + Duration::from_millis(4900)).is_none());
    let why = editor_refusal(check.poll(start + Duration::from_secs(5)));
    assert_eq!(why, "the editor did not answer; is it waiting for a key?");

    for answer in [buffer(true, false), buffer(false, false)] {
        let clean = FakeEditor::new(Some(answer));
        let clear = EditorCheck::start(&EmbeddedAsk(Some(&clean)), path, start)
            .poll(start)
            .unwrap()
            .unwrap();
        assert_eq!(clear.path(), path);
    }
}

/// An answer the editor gave in time but the panel took late (a stalled main loop) is as old as
/// the question: the clear it makes must not read as fresh, or a buffer edited since passes.
#[test]
fn a_late_poll_does_not_make_a_stale_clear_fresh() {
    let mut w = World::new("latepoll");
    w.write("a.txt", b"base\n", 0o644);
    let n = w.turn(|w| w.write("a.txt", b"end\n", 0o644));
    let file = w.file("a.txt");
    let asked_at = Instant::now()
        .checked_sub(MAX_CLEAR_AGE + Duration::from_secs(1))
        .expect("the clock is past the bound");
    let editor = FakeEditor::new(Some(buffer(true, false)));
    let clear = EditorCheck::start(&EmbeddedAsk(Some(&editor)), &file, asked_at)
        .poll(Instant::now())
        .expect("the answer is there")
        .expect("a clean buffer");
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
            clear,
        )
        .run();
    match refused {
        Err(Refusal::Editor(why)) => assert!(why.contains("press the key again"), "{why}"),
        other => panic!("expected a stale refusal, got {other:?}"),
    }
    assert_eq!(w.read("a.txt"), b"end\n");
}

// ---- recovery --------------------------------------------------------------------------------------

/// The size of the crash child's new content: over one write chunk, so the abort leaves a file
/// that is genuinely half written.
const CRASH_NEW_LEN: usize = 200 * 1024;

/// The child of the recovery tests: an in-place rewrite of a file with an extended attribute that dies after its
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
    let _ = replace(&target, &content, &note, &journal, &shadow, &hooks, None);
    unreachable!("the write was to abort");
}

/// Gives `path` a user extended attribute, which makes a write rewrite it in place.
fn mark_for_in_place(path: &Path) {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    let name = std::ffi::CString::new("user.eitri_test").unwrap();
    #[cfg(target_vendor = "apple")]
    // SAFETY: both strings are NUL-terminated; the value is 3 valid bytes.
    let set = unsafe { libc::setxattr(c.as_ptr(), name.as_ptr(), b"yes".as_ptr().cast(), 3, 0, 0) };
    #[cfg(not(target_vendor = "apple"))]
    // SAFETY: both strings are NUL-terminated; the value is 3 valid bytes.
    let set = unsafe { libc::setxattr(c.as_ptr(), name.as_ptr(), b"yes".as_ptr().cast(), 3, 0) };
    assert_eq!(
        set,
        0,
        "these tests need user extended attributes: {}",
        std::io::Error::last_os_error()
    );
}

/// Kills an in-place write of `rel` halfway (the file gets an extended attribute, so it is
/// rewritten in place) and returns the journal entry it left.
fn crash_mid_write(w: &World, rel: &str) -> JournalEntry {
    mark_for_in_place(&w.file(rel));
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
        let offered = w.review.recoveries_job().run().unwrap().entries;
        if !offered.is_empty() || Instant::now() > deadline {
            break offered;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(offered, vec![entry.clone()]);
    assert_eq!(
        entry.intended_mode,
        Some(0o644),
        "the entry says which mode the write was to leave"
    );

    assert_eq!(w.recover(&entry), Ok(Recovered::Restored));
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

// ---- hard links, and a restore of a write that finished ----------------------------------------------

#[test]
fn a_hard_linked_file_is_not_reverted_undone_or_recovered() {
    let mut w = World::new("hardlinked");
    let end = b"end\n".to_vec();
    w.write("a.txt", b"base\n", 0o644);
    let n = w.turn(|w| w.write("a.txt", &end, 0o644));
    // A second name for the same bytes, as the other user of a file outside the project would hold.
    let outside = w.dir().join("outside-link");
    std::fs::hard_link(w.file("a.txt"), &outside).unwrap();

    let refused = w.revert_file(n, "a.txt").unwrap_err();
    assert_eq!(refused, Refusal::Write(WriteError::HardLinked { links: 2 }));
    assert!(refused.to_string().contains("2 hard links"), "{refused}");
    assert_eq!(w.read("a.txt"), end);
    assert_eq!(std::fs::read(&outside).unwrap(), end, "the other link is untouched");
    assert!(w.pending().is_empty(), "nothing was journalled");

    // The same file after the link is gone reverts, and a link made afterwards refuses the undo.
    std::fs::remove_file(&outside).unwrap();
    let applied = w.revert_file(n, "a.txt").unwrap();
    assert_eq!(w.read("a.txt"), b"base\n");
    std::fs::hard_link(w.file("a.txt"), &outside).unwrap();
    assert_eq!(
        w.undo(&applied).unwrap_err(),
        Refusal::Write(WriteError::HardLinked { links: 2 })
    );
    assert_eq!(w.read("a.txt"), b"base\n");
    assert_eq!(std::fs::read(&outside).unwrap(), b"base\n");
}

#[test]
fn a_recovery_of_a_hard_linked_file_writes_nothing() {
    let w = World::new("recover-hardlink");
    w.write("a.txt", b"the original\n", 0o644);
    let entry = crash_mid_write(&w, "a.txt");
    let outside = w.dir().join("outside-link");
    std::fs::hard_link(w.file("a.txt"), &outside).unwrap();
    let half = w.read("a.txt");
    assert_eq!(
        w.recover(&entry).unwrap_err(),
        Refusal::Write(WriteError::HardLinked { links: 2 })
    );
    assert_eq!(w.read("a.txt"), half);
    assert_eq!(std::fs::read(&outside).unwrap(), half);
    // Asked until it shows: a lock another test's child shares for a moment hides it from one read.
    assert_eq!(w.pending_of(1).len(), 1, "the entry stays for a decision");
}

#[test]
fn an_entry_whose_write_finished_is_dropped_not_restored() {
    let w = World::new("recover-finished");
    w.write("a.txt", b"the original\n", 0o644);
    let entry = crash_mid_write(&w, "a.txt");
    // The write had in fact been completed (only its journal entry was left behind).
    let intended = noise(CRASH_NEW_LEN, 3);
    w.write("a.txt", &intended, 0o644);
    assert_eq!(
        w.recover(&entry),
        Ok(Recovered::AlreadyWritten),
        "told apart from a restore"
    );
    assert_eq!(w.read("a.txt"), intended, "the good write is not undone");
    assert!(w.pending().is_empty());
    assert!(w.refs("refs/eitri-journal/").is_empty(), "no pin is left");
    assert!(
        w.refs("refs/eitri-revert/").is_empty(),
        "nothing was restored, so nothing was anchored"
    );
}

#[test]
fn intended_bytes_with_the_wrong_mode_are_restored_not_dropped() {
    let w = World::new("recover-wrong-mode");
    w.write("a.txt", b"the original\n", 0o644);
    let entry = crash_mid_write(&w, "a.txt");
    // The last byte landed but the mode was never set: a write killed just before its `fchmod`.
    let intended = noise(CRASH_NEW_LEN, 3);
    w.write("a.txt", &intended, 0o600);
    assert_eq!(entry.intended_mode, Some(0o644));
    assert_eq!(w.recover(&entry), Ok(Recovered::Restored));
    assert_eq!(w.read("a.txt"), b"the original\n");
    assert_eq!(mode_of(&w.file("a.txt")), 0o644);
    assert!(w.pending().is_empty());
}

#[test]
fn an_older_entry_without_its_intended_mode_is_never_taken_for_finished() {
    let w = World::new("recover-old-entry");
    w.write("a.txt", b"the original\n", 0o644);
    let entry = crash_mid_write(&w, "a.txt");
    // The entry as a build that did not record the intended mode wrote it.
    let file = w.review_dir.join("journal").join(format!("{}.json", entry.id));
    let mut json: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert!(json.as_object_mut().unwrap().remove("intended_mode").is_some());
    std::fs::write(&file, serde_json::to_vec(&json).unwrap()).unwrap();
    let entry = w
        .pending_of(1)
        .into_iter()
        .find(|e| e.id == entry.id)
        .expect("an older entry is still offered");
    assert_eq!(entry.intended_mode, None);
    // Even holding exactly the intended bytes, the file is restored: whether its mode is right is
    // not known.
    w.write("a.txt", &noise(CRASH_NEW_LEN, 3), 0o644);
    assert_eq!(w.recover(&entry), Ok(Recovered::Restored));
    assert_eq!(w.read("a.txt"), b"the original\n");
    assert!(w.pending().is_empty());
}

// ---- the project's write slot ----------------------------------------------------------------------

const PROBE_DIR: &str = "EITRI_REVERT_SLOT_PROBE_DIR";

/// The child of the slot probe: tries to take the review directory's write slot both ways, as
/// another window would, and says which it got. Does nothing unless the parent set the directory.
#[test]
#[ignore = "run by the write slot tests"]
fn write_slot_probe_child() {
    let Some(dir) = std::env::var_os(PROBE_DIR) else {
        return;
    };
    let slot = WriteSlot::open(Path::new(&dir)).unwrap();
    let exclusive = slot.try_exclusive().is_ok();
    let shared = slot.try_shared().is_ok();
    println!("SLOT exclusive={exclusive} shared={shared}");
}

/// Whether another process could take `review_dir`'s write slot exclusively, and shared.
fn probe_slot_from_another_process(review_dir: &Path) -> (bool, bool) {
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "write_slot_probe_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(PROBE_DIR, review_dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .find_map(|l| l.split_once("SLOT ").map(|(_, said)| said))
        .unwrap_or_else(|| panic!("the probe said nothing: {stdout}"));
    (line.contains("exclusive=true"), line.contains("shared=true"))
}

#[test]
fn a_revert_parked_after_its_last_check_still_holds_the_slot_until_after_its_replace() {
    let mut w = World::new("slot-parked");
    w.write("a.txt", b"one\ntwo\n", 0o644);
    let n = w.turn(|w| w.write("a.txt", b"one\nTWO\n", 0o644));

    // The write parks once its new content is synced, after every last check and before the
    // rename that puts it in place.
    let (parked_tx, parked_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let (parked_tx, release_rx) = (Mutex::new(parked_tx), Mutex::new(release_rx));
    let hooks = FsHooks {
        fault: Some(Arc::new(move |stage| {
            if stage == Stage::Synced {
                let _ = parked_tx.lock().unwrap().send(());
                let _ = release_rx.lock().unwrap().recv();
            }
            Ok(())
        })),
        ..FsHooks::default()
    };
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
        .with_fs_hooks(hooks);
    let worker = std::thread::spawn(move || job.run());
    parked_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("the revert reached its write");

    assert_eq!(w.read("a.txt"), b"one\nTWO\n", "nothing is in place yet");
    assert_eq!(
        probe_slot_from_another_process(&w.review_dir),
        (false, false),
        "another window can neither write nor read the disk for a message while the revert is parked"
    );
    release_tx.send(()).unwrap();
    let applied = worker.join().unwrap().expect("the revert lands once released");
    assert_eq!(applied.kind, RevertKind::Replace);
    assert_eq!(w.read("a.txt"), b"one\ntwo\n");
    assert_eq!(
        probe_slot_from_another_process(&w.review_dir),
        (true, true),
        "the slot is let go once the write has returned"
    );
}

#[test]
fn a_write_job_is_refused_while_another_holder_has_the_slot_and_writes_nothing() {
    let mut w = World::new("slot-refused");
    w.write("a.txt", b"one\ntwo\n", 0o644);
    let n = w.turn(|w| w.write("a.txt", b"one\nTWO\n", 0o644));
    let slot = WriteSlot::open(&w.review_dir).unwrap();
    let other_window = slot.try_shared().unwrap();
    let refused = w.revert_file(n, "a.txt").unwrap_err();
    assert!(
        matches!(&refused, Refusal::SlotBusy(_)),
        "a message being read from the disk elsewhere holds the write back: {refused:?}"
    );
    assert!(refused.to_string().contains("another Eitri window"), "{refused}");
    assert_eq!(w.read("a.txt"), b"one\nTWO\n");
    drop(other_window);
    w.revert_file(n, "a.txt").expect("free again");
    assert_eq!(w.read("a.txt"), b"one\ntwo\n");
}
