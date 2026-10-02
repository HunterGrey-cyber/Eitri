//! The review draft, the message built from it and the send-time check of what its reverts say,
//! against real files in temporary directories under the target dir.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use eitri_core::turn_review::{
    check_reverts, compose, NewRevert, RevertShape, RevertSource, RevertStatus, ReviewDraft, UndoData, UndoState,
    MAX_COMMENT_CHARS,
};

/// A directory of the test's own under the target dir, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("turn_review_draft")
            .join(format!(
                "{name}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir.canonicalize().unwrap())
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn lines_revert(turn: u32, path: &str, at_line: u32, to: &[u8], from: &[u8], source: RevertSource) -> NewRevert {
    let count = to.split_inclusive(|b| *b == b'\n').count() as u32;
    NewRevert {
        turn,
        path: path.into(),
        hunk: Some((0, "@@ -1 +1 @@".into())),
        shape: RevertShape::Lines {
            from: at_line,
            to: at_line + count.saturating_sub(1),
        },
        source,
        at_line,
        reverted_to: to.to_vec(),
        replaced: from.to_vec(),
        undo: None,
    }
}

fn whole(turn: u32, path: &str, shape: RevertShape, to: &[u8], from: &[u8]) -> NewRevert {
    NewRevert {
        turn,
        path: path.into(),
        hunk: None,
        shape,
        source: RevertSource::Panel,
        at_line: 1,
        reverted_to: to.to_vec(),
        replaced: from.to_vec(),
        undo: None,
    }
}

fn status_of(root: &Path, draft: &ReviewDraft, id: u32, editor: &BTreeMap<u32, Option<Vec<u8>>>) -> RevertStatus {
    check_reverts(root, draft, editor)
        .into_iter()
        .find(|(i, _)| *i == id)
        .unwrap()
        .1
}

fn none() -> BTreeMap<u32, Option<Vec<u8>>> {
    BTreeMap::new()
}

#[test]
fn the_message_has_the_fixed_shape() {
    let mut draft = ReviewDraft::default();
    let revert = draft.record_revert(lines_revert(
        7,
        "core/src/foo.rs",
        120,
        &b"l\n".repeat(12),
        b"changed\n",
        RevertSource::Panel,
    ));
    draft
        .add_comment(
            7,
            "core/src/agent_backend.rs",
            837,
            842,
            vec!["let base = review.base_for(turn_id);".into()],
            "Why is the base looked up by turn id and not by tab?",
        )
        .unwrap();
    draft
        .add_comment(
            7,
            "core/src/x.rs",
            10,
            11,
            vec!["a".into(), "b".into()],
            "  tidy these  ",
        )
        .unwrap();
    let preview = compose(&draft, &[(revert, RevertStatus::OnDisk)], 7);
    let want = "\
Review of your last turn: 2 comments, 1 revert.

I reverted these changes; the files no longer contain them:
- core/src/foo.rs lines 120-131 (back to how they were before your turn)

Comments:
1. core/src/agent_backend.rs:837-842
   > let base = review.base_for(turn_id);
   Why is the base looked up by turn id and not by tab?
2. core/src/x.rs:10-11
   > a
   > b
   tidy these";
    assert_eq!(preview.text, want);
    assert!(preview.not_on_disk.is_empty());
    assert_eq!(preview.digest.len(), 16);
    assert!(preview.digest.bytes().all(|b| b.is_ascii_hexdigit()));
}

#[test]
fn the_other_revert_shapes_and_the_counts_read_correctly() {
    let mut draft = ReviewDraft::default();
    let a = draft.record_revert(whole(7, "gone.rs", RevertShape::Deleted, b"", b"x\n"));
    let b = draft.record_revert(whole(7, "back.rs", RevertShape::Restored, b"y\n", b""));
    let c = draft.record_revert(whole(7, "all.rs", RevertShape::WholeFile, b"z\n", b"w\n"));
    let statuses = [a, b, c].map(|id| (id, RevertStatus::OnDisk));
    let text = compose(&draft, &statuses, 7).text;
    assert_eq!(
        text,
        "\
Review of your last turn: 0 comments, 3 reverts.

I reverted these changes; the files no longer contain them:
- gone.rs: deleted (it did not exist before your turn)
- back.rs: restored (you had deleted it)
- all.rs: the whole file is back to how it was before your turn"
    );

    let mut one = ReviewDraft::default();
    one.add_comment(7, "a.rs", 1, 1, vec![], "x").unwrap();
    let text = compose(&one, &[], 7).text;
    assert!(
        text.starts_with("Review of your last turn: 1 comment, 0 reverts."),
        "{text}"
    );
    assert!(
        !text.contains("I reverted"),
        "no revert section without a revert: {text}"
    );
}

#[test]
fn the_header_names_the_turns_when_they_are_not_all_the_last() {
    let mut draft = ReviewDraft::default();
    draft.add_comment(5, "a.rs", 1, 1, vec![], "old").unwrap();
    draft.add_comment(7, "a.rs", 2, 2, vec![], "new").unwrap();
    assert!(compose(&draft, &[], 9)
        .text
        .starts_with("Review of turns 5, 7: 2 comments, 0 reverts."));

    let mut only = ReviewDraft::default();
    only.add_comment(5, "a.rs", 1, 1, vec![], "old").unwrap();
    assert!(compose(&only, &[], 7)
        .text
        .starts_with("Review of turn 5: 1 comment, 0 reverts."));
}

#[test]
fn only_what_is_on_disk_is_reported() {
    let mut draft = ReviewDraft::default();
    let on = draft.record_revert(lines_revert(7, "a.rs", 1, b"1\n", b"x\n", RevertSource::Panel));
    let editor = draft.record_revert(lines_revert(7, "b.rs", 1, b"1\n", b"x\n", RevertSource::Editor));
    let undone = draft.record_revert(lines_revert(7, "c.rs", 1, b"1\n", b"x\n", RevertSource::Panel));
    let changed = draft.record_revert(lines_revert(7, "d.rs", 1, b"1\n", b"x\n", RevertSource::Panel));
    let preview = compose(
        &draft,
        &[
            (on, RevertStatus::OnDisk),
            (editor, RevertStatus::OnlyInEditor),
            (undone, RevertStatus::Undone),
            (changed, RevertStatus::ChangedSince),
        ],
        7,
    );
    assert!(preview.text.contains("a.rs lines 1-1"), "{}", preview.text);
    for left_out in ["b.rs", "c.rs", "d.rs"] {
        assert!(
            !preview.text.contains(left_out),
            "{left_out} is not on disk: {}",
            preview.text
        );
    }
    assert!(preview
        .text
        .starts_with("Review of your last turn: 0 comments, 1 revert."));
}

#[test]
fn the_preview_names_every_revert_not_on_disk() {
    let mut draft = ReviewDraft::default();
    let on = draft.record_revert(lines_revert(7, "a.rs", 1, b"1\n", b"x\n", RevertSource::Panel));
    let editor = draft.record_revert(lines_revert(7, "b.rs", 10, b"1\n2\n", b"x\n", RevertSource::Editor));
    let undone = draft.record_revert(lines_revert(7, "c.rs", 1, b"1\n", b"x\n", RevertSource::Panel));
    let changed = draft.record_revert(whole(7, "d.rs", RevertShape::WholeFile, b"1\n", b"x\n"));
    let missing = draft.record_revert(lines_revert(7, "e.rs", 1, b"1\n", b"x\n", RevertSource::Panel));
    // `missing` has no status at all: it is reported as changed, never as on disk.
    let preview = compose(
        &draft,
        &[
            (on, RevertStatus::OnDisk),
            (editor, RevertStatus::OnlyInEditor),
            (undone, RevertStatus::Undone),
            (changed, RevertStatus::ChangedSince),
        ],
        7,
    );
    assert_eq!(
        preview.not_on_disk,
        vec![
            (editor, RevertStatus::OnlyInEditor),
            (undone, RevertStatus::Undone),
            (changed, RevertStatus::ChangedSince),
            (missing, RevertStatus::ChangedSince),
        ]
    );
    assert_eq!(RevertStatus::OnlyInEditor.why(), Some("only in the editor, not saved"));
    assert_eq!(RevertStatus::Undone.why(), Some("undone"));
    assert_eq!(RevertStatus::ChangedSince.why(), Some("changed since"));
    assert_eq!(RevertStatus::OnDisk.why(), None);
}

#[test]
fn an_editor_revert_not_written_says_only_in_the_editor() {
    let dir = Scratch::new("editor");
    // The file still holds the agent's lines; the buffer holds the reverted ones.
    dir.write("a.rs", b"a\nB\nC\nd\n");
    let mut draft = ReviewDraft::default();
    let id = draft.record_revert(lines_revert(7, "a.rs", 2, b"b\nc\n", b"B\nC\n", RevertSource::Editor));

    let mut editor = BTreeMap::new();
    editor.insert(id, Some(b"b\nc\n".to_vec()));
    assert_eq!(status_of(&dir.0, &draft, id, &editor), RevertStatus::OnlyInEditor);

    // The buffer is not loaded, or no longer holds the reverted lines: it was undone there.
    editor.insert(id, None);
    assert_eq!(status_of(&dir.0, &draft, id, &editor), RevertStatus::Undone);
    editor.insert(id, Some(b"B\nC\n".to_vec()));
    assert_eq!(status_of(&dir.0, &draft, id, &editor), RevertStatus::Undone);

    // Saved: on disk.
    dir.write("a.rs", b"a\nb\nc\nd\n");
    editor.insert(id, Some(b"b\nc\n".to_vec()));
    assert_eq!(status_of(&dir.0, &draft, id, &editor), RevertStatus::OnDisk);

    // A panel revert whose file holds the replaced lines is undone, whatever an editor map says.
    dir.write("a.rs", b"a\nB\nC\nd\n");
    let panel = draft.record_revert(lines_revert(7, "a.rs", 2, b"b\nc\n", b"B\nC\n", RevertSource::Panel));
    editor.insert(panel, Some(b"b\nc\n".to_vec()));
    assert_eq!(status_of(&dir.0, &draft, panel, &editor), RevertStatus::Undone);
}

#[test]
fn an_undone_revert_is_left_out() {
    let dir = Scratch::new("undone");
    dir.write("a.rs", b"a\nb\nc\nd\n");
    let mut draft = ReviewDraft::default();
    let id = draft.record_revert(lines_revert(7, "a.rs", 2, b"b\nc\n", b"B\nC\n", RevertSource::Panel));
    assert_eq!(status_of(&dir.0, &draft, id, &none()), RevertStatus::OnDisk);

    draft.mark_undone(id);
    // Marked, so it is not read: the file may be anything.
    std::fs::remove_file(dir.0.join("a.rs")).unwrap();
    let statuses = check_reverts(&dir.0, &draft, &none());
    assert_eq!(statuses, vec![(id, RevertStatus::Undone)]);
    let text = compose(&draft, &statuses, 7).text;
    assert!(!text.contains("a.rs"), "{text}");
    assert!(text.contains("0 reverts"), "{text}");
}

#[test]
fn a_changed_region_is_changed_since() {
    let dir = Scratch::new("changed");
    let mut draft = ReviewDraft::default();
    let id = draft.record_revert(lines_revert(7, "a.rs", 2, b"b\nc\n", b"B\nC\n", RevertSource::Panel));

    // The user edited the reverted lines.
    dir.write("a.rs", b"a\nb\nmine\nd\n");
    assert_eq!(status_of(&dir.0, &draft, id, &none()), RevertStatus::ChangedSince);
    // The lines moved down by one: a region is checked where it was, not searched for.
    dir.write("a.rs", b"new\na\nb\nc\nd\n");
    assert_eq!(status_of(&dir.0, &draft, id, &none()), RevertStatus::ChangedSince);
    // The region is past the end.
    dir.write("a.rs", b"a\n");
    assert_eq!(status_of(&dir.0, &draft, id, &none()), RevertStatus::ChangedSince);
    // The file is gone.
    std::fs::remove_file(dir.0.join("a.rs")).unwrap();
    assert_eq!(status_of(&dir.0, &draft, id, &none()), RevertStatus::ChangedSince);
    // A last line with no newline is compared byte for byte.
    dir.write("a.rs", b"a\nb\nc");
    assert_eq!(status_of(&dir.0, &draft, id, &none()), RevertStatus::ChangedSince);
}

#[test]
fn a_revert_that_removed_lines_is_checked_by_their_absence() {
    let dir = Scratch::new("zero-length");
    let mut draft = ReviewDraft::default();
    // The turn added "x\ny\n" after line 2; the revert removed them, so nothing is left at line 3.
    let id = draft.record_revert(lines_revert(7, "a.rs", 3, b"", b"x\ny\n", RevertSource::Panel));
    dir.write("a.rs", b"1\n2\n3\n");
    assert_eq!(status_of(&dir.0, &draft, id, &none()), RevertStatus::OnDisk);
    // The added lines are back there.
    dir.write("a.rs", b"1\n2\nx\ny\n3\n");
    assert_eq!(status_of(&dir.0, &draft, id, &none()), RevertStatus::Undone);
}

#[test]
fn whole_file_reverts_are_checked_against_the_path() {
    let dir = Scratch::new("whole");
    let mut draft = ReviewDraft::default();
    let deleted = draft.record_revert(whole(7, "new.rs", RevertShape::Deleted, b"", b"x\n"));
    let restored = draft.record_revert(whole(7, "old.rs", RevertShape::Restored, b"old\n", b""));
    let replaced = draft.record_revert(whole(7, "mod.rs", RevertShape::WholeFile, b"base\n", b"agent\n"));

    dir.write("old.rs", b"old\n");
    dir.write("mod.rs", b"base\n");
    for id in [deleted, restored, replaced] {
        assert_eq!(status_of(&dir.0, &draft, id, &none()), RevertStatus::OnDisk);
    }

    // The created file is back, the restored one was edited, the replaced one changed again.
    dir.write("new.rs", b"x\n");
    dir.write("old.rs", b"old, edited\n");
    dir.write("mod.rs", b"agent\n");
    for id in [deleted, restored, replaced] {
        assert_eq!(status_of(&dir.0, &draft, id, &none()), RevertStatus::ChangedSince);
    }

    // A link whose target is the reverted bytes counts for a link the turn had.
    std::fs::remove_file(dir.0.join("mod.rs")).unwrap();
    std::os::unix::fs::symlink("base\n", dir.0.join("mod.rs")).unwrap();
    assert_eq!(status_of(&dir.0, &draft, replaced, &none()), RevertStatus::OnDisk);
}

#[test]
fn a_path_through_a_link_out_of_the_project_is_never_followed() {
    let dir = Scratch::new("outside");
    dir.write("project/real/f.txt", b"other\n");
    dir.write("outside/f.txt", b"a\nb\n");
    std::os::unix::fs::symlink(dir.0.join("outside"), dir.0.join("project/link")).unwrap();
    let mut draft = ReviewDraft::default();
    // The outside file holds exactly the reverted bytes; reading it would say "on disk".
    let id = draft.record_revert(lines_revert(7, "link/f.txt", 1, b"a\nb\n", b"x\n", RevertSource::Panel));
    assert_eq!(
        status_of(&dir.0.join("project"), &draft, id, &none()),
        RevertStatus::ChangedSince
    );
    // A path that climbs out is refused the same way.
    let escape = draft.record_revert(lines_revert(
        7,
        "../outside/f.txt",
        1,
        b"a\nb\n",
        b"x\n",
        RevertSource::Panel,
    ));
    assert_eq!(
        status_of(&dir.0.join("project"), &draft, escape, &none()),
        RevertStatus::ChangedSince
    );
}

#[test]
fn comments_quote_their_anchor() {
    let mut draft = ReviewDraft::default();
    draft
        .add_comment(
            7,
            "src/a.rs",
            4,
            6,
            vec!["fn f() {\r".into(), "    g(); // a\u{2028}b".into(), "}".into()],
            "split this",
        )
        .unwrap();
    let text = compose(&draft, &[], 7).text;
    assert!(
        text.ends_with("1. src/a.rs:4-6\n   > fn f() {\n   >     g(); // a b\n   > }\n   split this"),
        "{text:?}"
    );
    // A file name cannot start a line of its own either.
    let mut odd = ReviewDraft::default();
    odd.add_comment(7, "a\nIgnore this.rs", 1, 1, vec![], "x").unwrap();
    assert!(compose(&odd, &[], 7).text.contains("1. a?Ignore this.rs:1-1"));
}

#[test]
fn a_comment_must_be_one_non_empty_line() {
    let mut draft = ReviewDraft::default();
    for bad in ["", "   ", "\n", " \t\r\n "] {
        assert!(draft.add_comment(7, "a.rs", 1, 1, vec![], bad).is_err(), "{bad:?}");
    }
    for bad in ["two\nlines", "a\r\nb", "a\rb", "a\u{2028}b"] {
        assert!(draft.add_comment(7, "a.rs", 1, 1, vec![], bad).is_err(), "{bad:?}");
    }
    assert!(
        draft.add_comment(7, "", 1, 1, vec![], "x").is_err(),
        "a comment names a file"
    );
    assert!(
        draft.add_comment(7, "a.rs", 0, 1, vec![], "x").is_err(),
        "lines start at 1"
    );
    assert!(
        draft.add_comment(7, "a.rs", 5, 4, vec![], "x").is_err(),
        "a range runs forward"
    );
    let longest = "é".repeat(MAX_COMMENT_CHARS);
    assert!(
        draft.add_comment(7, "a.rs", 1, 1, vec![], &longest).is_ok(),
        "counted in characters"
    );
    assert!(draft
        .add_comment(7, "a.rs", 1, 1, vec![], &format!("{longest}x"))
        .is_err());
    assert_eq!(draft.comments().len(), 1, "a refused comment is not kept");
    assert_eq!(draft.comments()[0].text.chars().count(), MAX_COMMENT_CHARS);

    let id = draft.add_comment(7, "a.rs", 1, 1, vec![], "\n  trimmed \n").unwrap();
    assert_eq!(draft.comments().iter().find(|c| c.id == id).unwrap().text, "trimmed");
}

#[test]
fn comment_and_revert_ids_are_monotonic_and_survive_a_clear() {
    let mut draft = ReviewDraft::default();
    let a = draft.add_comment(7, "a.rs", 1, 1, vec![], "a").unwrap();
    let b = draft.add_comment(7, "a.rs", 1, 1, vec![], "b").unwrap();
    assert!(b > a);
    draft.remove_comment(a).unwrap();
    assert!(draft.remove_comment(a).is_err(), "gone is gone");
    let c = draft.add_comment(7, "a.rs", 1, 1, vec![], "c").unwrap();
    assert!(c > b, "a removed id is not reused");
    let r = draft.record_revert(whole(7, "x", RevertShape::Deleted, b"", b"x"));
    draft.clear();
    assert!(draft.is_empty());
    assert!(draft.add_comment(7, "a.rs", 1, 1, vec![], "d").unwrap() > c);
    assert!(draft.record_revert(whole(7, "x", RevertShape::Deleted, b"", b"x")) > r);
}

#[test]
fn last_undoable_skips_editor_and_undone_reverts() {
    let mut draft = ReviewDraft::default();
    assert!(draft.last_undoable().is_none());
    let first = draft.record_revert(lines_revert(7, "a.rs", 1, b"1\n", b"x\n", RevertSource::Panel));
    let second = draft.record_revert(lines_revert(7, "a.rs", 5, b"1\n", b"x\n", RevertSource::Panel));
    let editor = draft.record_revert(lines_revert(7, "b.rs", 1, b"1\n", b"x\n", RevertSource::Editor));
    assert_eq!(
        draft.last_undoable().unwrap().id,
        second,
        "the editor's is newer but is undone there"
    );
    draft.mark_undone(second);
    assert_eq!(draft.last_undoable().unwrap().id, first);
    draft.mark_undone(first);
    assert!(draft.last_undoable().is_none());
    assert!(!draft.reverts().iter().find(|r| r.id == editor).unwrap().undone);
    draft.mark_undone(99);
}

#[test]
fn undo_data_is_kept_as_recorded() {
    let mut draft = ReviewDraft::default();
    let mut revert = whole(7, "run.sh", RevertShape::Deleted, b"", b"#!/bin/sh\n");
    revert.undo = Some(UndoData {
        pre: UndoState::Regular {
            blob: "aa".into(),
            mode: 0o755,
        },
        post: UndoState::Absent,
    });
    let id = draft.record_revert(revert.clone());
    let record = draft.last_undoable().unwrap();
    assert_eq!(record.id, id);
    assert_eq!(record.new, revert);
}

#[test]
fn the_digest_changes_with_the_disk() {
    let dir = Scratch::new("digest");
    dir.write("a.rs", b"a\nB\nC\nd\n");
    let mut draft = ReviewDraft::default();
    let id = draft.record_revert(lines_revert(7, "a.rs", 2, b"b\nc\n", b"B\nC\n", RevertSource::Editor));
    draft.add_comment(7, "a.rs", 1, 1, vec![], "hm").unwrap();
    let preview = |editor: &BTreeMap<u32, Option<Vec<u8>>>| compose(&draft, &check_reverts(&dir.0, &draft, editor), 7);

    let mut editor = BTreeMap::new();
    editor.insert(id, Some(b"b\nc\n".to_vec()));
    let only_in_editor = preview(&editor);
    assert_eq!(only_in_editor.not_on_disk, vec![(id, RevertStatus::OnlyInEditor)]);
    assert_eq!(
        preview(&editor).digest,
        only_in_editor.digest,
        "the same state, the same digest"
    );

    // Undone in the editor: the message text is the same (neither is reported), the status is not.
    editor.insert(id, None);
    let undone = preview(&editor);
    assert_eq!(undone.text, only_in_editor.text);
    assert_ne!(undone.digest, only_in_editor.digest);

    // Saved: now the message says it.
    dir.write("a.rs", b"a\nb\nc\nd\n");
    let saved = preview(&editor);
    assert_ne!(saved.text, only_in_editor.text);
    assert_ne!(saved.digest, only_in_editor.digest);
    assert_ne!(saved.digest, undone.digest);

    // The comment changes it too.
    let mut edited = draft.clone();
    edited.add_comment(7, "a.rs", 2, 2, vec![], "one more").unwrap();
    let other = compose(&edited, &check_reverts(&dir.0, &edited, &editor), 7);
    assert_ne!(other.digest, saved.digest);
}
