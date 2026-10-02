//! Diffs between snapshots against real git, in temporary directories under the target dir.
//!
//! The test-side git is isolated like the shadow's own (cleared environment, no global or system
//! config), so the machine's configuration cannot change a result.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use eitri_core::turn_review::{
    changed_files, file_hunks, review_dir_for, ChangeKind, Limits, LineKind, Shadow, ShadowError, Side, SnapshotKind,
    SnapshotLabel, SnapshotOutcome,
};

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("turn_review_diff")
            .join(format!(
                "{name}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir.canonicalize().unwrap())
    }

    fn project(&self) -> PathBuf {
        let p = self.0.join("project");
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn shadow(&self, project: &Path) -> Shadow {
        let review = review_dir_for(Some(self.0.join("state").as_os_str()), None, project).unwrap();
        Shadow::open_with_excludes(&review, project, None).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tgit(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir)
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, bytes).unwrap();
}

fn snap(shadow: &Shadow, turn: u32, kind: SnapshotKind) -> String {
    let label = SnapshotLabel {
        turn,
        kind,
        turn_id: format!("turn-{turn}"),
        tab: 1,
        time_ms: 1_000 + u64::from(turn),
    };
    match shadow.snapshot("s1", &label, &Limits::default()) {
        SnapshotOutcome::Taken(s) => s.commit,
        SnapshotOutcome::Unavailable(why) => panic!("snapshot unavailable: {why}"),
    }
}

fn lines(n: usize) -> Vec<u8> {
    (0..n).map(|i| format!("line {i}\n")).collect::<String>().into_bytes()
}

fn change<'a>(
    changes: &'a [eitri_core::turn_review::FileChange],
    path: &str,
) -> &'a eitri_core::turn_review::FileChange {
    changes
        .iter()
        .find(|c| c.path == Path::new(path))
        .unwrap_or_else(|| panic!("no change for {path}: {changes:?}"))
}

#[test]
fn modified_created_and_deleted_files_are_listed_with_their_line_counts() {
    let s = Scratch::new("kinds");
    let project = s.project();
    write(&project.join("mod.txt"), b"a\nb\nc\n");
    write(&project.join("gone.txt"), b"x\ny\n");
    write(&project.join("same.txt"), b"same\n");
    let shadow = s.shadow(&project);
    let base = snap(&shadow, 1, SnapshotKind::Base);

    write(&project.join("mod.txt"), b"a\nB\nc\nd\n");
    std::fs::remove_file(project.join("gone.txt")).unwrap();
    write(&project.join("dir/new.txt"), b"1\n2\n3\n");
    let end = snap(&shadow, 1, SnapshotKind::End);

    let changes = changed_files(&shadow, &base, &Side::Snapshot(end)).unwrap();
    assert_eq!(changes.len(), 3, "{changes:?}");
    let m = change(&changes, "mod.txt");
    assert_eq!(
        (m.kind, m.added, m.removed, m.binary, m.nested),
        (ChangeKind::Modified, 2, 1, false, false)
    );
    let g = change(&changes, "gone.txt");
    assert_eq!((g.kind, g.added, g.removed), (ChangeKind::Deleted, 0, 2));
    let n = change(&changes, "dir/new.txt");
    assert_eq!((n.kind, n.added, n.removed), (ChangeKind::Added, 3, 0));
}

#[test]
fn a_modified_file_has_hunks_with_old_and_new_line_numbers() {
    let s = Scratch::new("hunks");
    let project = s.project();
    write(&project.join("f.txt"), &lines(40));
    let shadow = s.shadow(&project);
    let base = snap(&shadow, 1, SnapshotKind::Base);

    let mut edited = String::from_utf8(lines(40)).unwrap();
    edited = edited.replace("line 10\n", "line ten\nextra\n");
    edited = edited.replace("line 30\n", "");
    write(&project.join("f.txt"), edited.as_bytes());
    let end = snap(&shadow, 1, SnapshotKind::End);

    let diff = file_hunks(&shadow, &base, &Side::Snapshot(end), Path::new("f.txt"), 2000)
        .unwrap()
        .unwrap();
    assert_eq!((diff.added, diff.removed), (2, 2));
    assert_eq!(diff.hunks.len(), 2);
    assert_eq!(diff.hunks[0].id, 0);
    assert_eq!(diff.hunks[1].id, 1);
    let first = &diff.hunks[0];
    let removed = first.lines.iter().find(|l| l.kind == LineKind::Removed).unwrap();
    assert_eq!(
        (removed.text.as_str(), removed.old_no, removed.new_no),
        ("line 10", Some(11), None)
    );
    let added: Vec<_> = first.lines.iter().filter(|l| l.kind == LineKind::Added).collect();
    assert_eq!((added[0].text.as_str(), added[0].new_no), ("line ten", Some(11)));
    assert_eq!((added[1].text.as_str(), added[1].new_no), ("extra", Some(12)));
}

#[test]
fn a_created_file_and_a_deleted_file_have_headers_and_one_sided_numbers() {
    let s = Scratch::new("created-deleted");
    let project = s.project();
    write(&project.join("gone.txt"), b"x\ny\n");
    let shadow = s.shadow(&project);
    let base = snap(&shadow, 1, SnapshotKind::Base);
    std::fs::remove_file(project.join("gone.txt")).unwrap();
    write(&project.join("new.txt"), b"one\ntwo");
    let end = Side::Snapshot(snap(&shadow, 1, SnapshotKind::End));

    let created = file_hunks(&shadow, &base, &end, Path::new("new.txt"), 2000)
        .unwrap()
        .unwrap();
    assert!(created.new_file && !created.deleted_file);
    let kinds: Vec<_> = created.hunks[0].lines.iter().map(|l| l.kind).collect();
    // No trailing newline in the file: the marker follows its last line.
    assert_eq!(kinds, [LineKind::Added, LineKind::Added, LineKind::NoNewline]);
    assert_eq!(created.hunks[0].lines[0].new_no, Some(1));

    let deleted = file_hunks(&shadow, &base, &end, Path::new("gone.txt"), 2000)
        .unwrap()
        .unwrap();
    assert!(deleted.deleted_file && !deleted.new_file);
    assert_eq!(deleted.removed, 2);
}

#[test]
fn a_binary_file_is_flagged_and_has_no_hunks() {
    let s = Scratch::new("binary");
    let project = s.project();
    write(&project.join("b.bin"), &[0, 1, 2, 3, 0, 255]);
    write(&project.join("t.txt"), b"x\n");
    let shadow = s.shadow(&project);
    let base = snap(&shadow, 1, SnapshotKind::Base);
    write(&project.join("b.bin"), &[0, 1, 2, 9, 0, 255, 0]);
    let end = Side::Snapshot(snap(&shadow, 1, SnapshotKind::End));

    let changes = changed_files(&shadow, &base, &end).unwrap();
    let b = change(&changes, "b.bin");
    assert!(b.binary);
    assert_eq!((b.added, b.removed), (0, 0));
    let diff = file_hunks(&shadow, &base, &end, Path::new("b.bin"), 2000)
        .unwrap()
        .unwrap();
    assert!(diff.binary);
    assert!(diff.hunks.is_empty());
}

#[test]
fn a_mode_change_is_a_modified_file_with_no_lines_and_both_modes() {
    let s = Scratch::new("mode");
    let project = s.project();
    write(&project.join("run.sh"), b"#!/bin/sh\n");
    std::fs::set_permissions(project.join("run.sh"), std::fs::Permissions::from_mode(0o644)).unwrap();
    let shadow = s.shadow(&project);
    let base = snap(&shadow, 1, SnapshotKind::Base);
    std::fs::set_permissions(project.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let end = Side::Snapshot(snap(&shadow, 1, SnapshotKind::End));

    let changes = changed_files(&shadow, &base, &end).unwrap();
    let c = change(&changes, "run.sh");
    assert_eq!(
        (c.kind, c.added, c.removed, c.binary),
        (ChangeKind::Modified, 0, 0, false)
    );
    let diff = file_hunks(&shadow, &base, &end, Path::new("run.sh"), 2000)
        .unwrap()
        .unwrap();
    assert_eq!(diff.mode_change, Some(("100644".to_owned(), "100755".to_owned())));
    assert!(diff.hunks.is_empty());
}

#[test]
fn a_file_that_became_a_symlink_is_a_type_change() {
    let s = Scratch::new("typechange");
    let project = s.project();
    write(&project.join("link"), b"plain\n");
    write(&project.join("target"), b"t\n");
    let shadow = s.shadow(&project);
    let base = snap(&shadow, 1, SnapshotKind::Base);
    std::fs::remove_file(project.join("link")).unwrap();
    std::os::unix::fs::symlink("target", project.join("link")).unwrap();
    let end = Side::Snapshot(snap(&shadow, 1, SnapshotKind::End));

    let changes = changed_files(&shadow, &base, &end).unwrap();
    assert_eq!(change(&changes, "link").kind, ChangeKind::TypeChanged);
}

#[test]
fn a_nested_repository_is_flagged_and_carries_no_line_counts() {
    let s = Scratch::new("nested");
    let project = s.project();
    write(&project.join("a.txt"), b"a\n");
    let shadow = s.shadow(&project);
    let base = snap(&shadow, 1, SnapshotKind::Base);

    let nested = project.join("vendor/lib");
    std::fs::create_dir_all(&nested).unwrap();
    tgit(&nested, &["init", "-q"]);
    write(&nested.join("x.txt"), b"x\n");
    tgit(&nested, &["add", "."]);
    tgit(&nested, &["commit", "-q", "-m", "one"]);
    let end = Side::Snapshot(snap(&shadow, 1, SnapshotKind::End));

    let changes = changed_files(&shadow, &base, &end).unwrap();
    let n = change(&changes, "vendor/lib");
    assert!(n.nested);
    assert_eq!((n.kind, n.added, n.removed, n.binary), (ChangeKind::Added, 0, 0, false));
}

#[test]
fn a_patch_over_the_cap_is_refused_not_cut() {
    let s = Scratch::new("cap");
    let project = s.project();
    write(&project.join("keep.txt"), b"k\n");
    let shadow = s.shadow(&project);
    let base = snap(&shadow, 1, SnapshotKind::Base);
    write(&project.join("big.txt"), &lines(2001));
    write(&project.join("fits.txt"), &lines(2000));
    let end = Side::Snapshot(snap(&shadow, 1, SnapshotKind::End));

    assert_eq!(
        file_hunks(&shadow, &base, &end, Path::new("big.txt"), 2000).unwrap(),
        None
    );
    let fits = file_hunks(&shadow, &base, &end, Path::new("fits.txt"), 2000)
        .unwrap()
        .unwrap();
    assert_eq!(fits.added, 2000);
    // The overview still reports the counts of the file that was refused.
    let changes = changed_files(&shadow, &base, &end).unwrap();
    assert_eq!(change(&changes, "big.txt").added, 2001);
}

#[test]
fn the_work_tree_side_is_what_is_on_disk_now_and_leaves_no_trace_in_the_store() {
    let s = Scratch::new("worktree");
    let project = s.project();
    write(&project.join("a.txt"), b"one\n");
    write(&project.join("b.txt"), b"keep\n");
    let shadow = s.shadow(&project);
    let base = snap(&shadow, 1, SnapshotKind::Base);
    let refs_before = shadow.snapshot_refs().unwrap().len();
    let entries_before: Vec<_> = std::fs::read_dir(shadow.review_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();

    // Not snapshotted: the live files are all there is.
    write(&project.join("a.txt"), b"one\ntwo\n");
    write(&project.join("c.txt"), b"new\n");
    std::fs::remove_file(project.join("b.txt")).unwrap();

    let changes = changed_files(&shadow, &base, &Side::WorkTree).unwrap();
    assert_eq!(changes.len(), 3, "{changes:?}");
    assert_eq!(change(&changes, "a.txt").added, 1);
    assert_eq!(change(&changes, "b.txt").kind, ChangeKind::Deleted);
    assert_eq!(change(&changes, "c.txt").kind, ChangeKind::Added);
    let diff = file_hunks(&shadow, &base, &Side::WorkTree, Path::new("a.txt"), 2000)
        .unwrap()
        .unwrap();
    assert_eq!(
        diff.hunks[0].lines.iter().filter(|l| l.kind == LineKind::Added).count(),
        1
    );

    // The same comparison against a snapshot taken at the same moment gives the same list.
    let end = snap(&shadow, 1, SnapshotKind::End);
    let against_snapshot = changed_files(&shadow, &base, &Side::Snapshot(end)).unwrap();
    assert_eq!(changes, against_snapshot);

    // No ref, and no leftover index: a comparison with the live files does not touch the
    // bookkeeping the snapshots keep.
    assert_eq!(shadow.snapshot_refs().unwrap().len(), refs_before + 1);
    let mut after: Vec<_> = std::fs::read_dir(shadow.review_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    after.sort();
    let mut before = entries_before;
    before.sort();
    assert_eq!(after, before);
}

#[test]
fn an_unknown_snapshot_and_a_path_outside_the_project_are_refused() {
    let s = Scratch::new("refused");
    let project = s.project();
    write(&project.join("a.txt"), b"a\n");
    let shadow = s.shadow(&project);
    let base = snap(&shadow, 1, SnapshotKind::Base);

    let missing = "0123456789abcdef0123456789abcdef01234567";
    assert!(matches!(
        changed_files(&shadow, missing, &Side::WorkTree),
        Err(ShadowError::NoSuchCommit(_))
    ));
    assert!(matches!(
        changed_files(&shadow, "not hex; rm -rf", &Side::WorkTree),
        Err(ShadowError::NoSuchCommit(_))
    ));
    assert!(matches!(
        file_hunks(&shadow, &base, &Side::WorkTree, Path::new("../x"), 10),
        Err(ShadowError::InvalidPath(_))
    ));
}

#[test]
fn a_path_with_glob_characters_names_exactly_that_file() {
    let s = Scratch::new("literal");
    let project = s.project();
    write(&project.join("a.txt"), b"a\n");
    let shadow = s.shadow(&project);
    let base = snap(&shadow, 1, SnapshotKind::Base);
    write(&project.join("[x]*.txt"), b"odd\n");
    write(&project.join("x.txt"), b"other\n");
    let end = Side::Snapshot(snap(&shadow, 1, SnapshotKind::End));

    let diff = file_hunks(&shadow, &base, &end, Path::new("[x]*.txt"), 2000)
        .unwrap()
        .unwrap();
    assert_eq!(diff.added, 1);
    assert_eq!(diff.hunks[0].lines[0].text, "odd");
}

#[test]
fn crlf_files_are_diffed_byte_for_byte() {
    let s = Scratch::new("crlf");
    let project = s.project();
    write(&project.join("w.txt"), b"a\r\nb\r\n");
    let shadow = s.shadow(&project);
    let base = snap(&shadow, 1, SnapshotKind::Base);
    write(&project.join("w.txt"), b"a\r\nB\r\n");
    let end = Side::Snapshot(snap(&shadow, 1, SnapshotKind::End));

    let diff = file_hunks(&shadow, &base, &end, Path::new("w.txt"), 2000)
        .unwrap()
        .unwrap();
    let texts: Vec<_> = diff.hunks[0].lines.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(texts, ["a\r", "b\r", "B\r"]);
}
