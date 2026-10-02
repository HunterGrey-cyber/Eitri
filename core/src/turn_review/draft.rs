//! The review draft: the comments and the reverts one tab's user has collected while reading a
//! turn's changes, kept in Rust (not in the WebView) so a panel reload or crash cannot lose them.
//!
//! The draft is plain data. It never reaches a send path by itself: [`super::message`] turns it
//! into the one message the user confirms, and only that confirmed message is sent.

use std::collections::BTreeMap;
use std::path::Path;

use super::write::{read_current, Current, ProjectDir};

/// The longest comment, in characters.
pub const MAX_COMMENT_CHARS: usize = 4000;

/// One comment on lines of a file, with the text of those lines so the agent finds them even when
/// the file has moved on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    /// Per tab, monotonic: never reused, not even after the draft is cleared.
    pub id: u32,
    pub turn: u32,
    pub path: String,
    pub from: u32,
    pub to: u32,
    /// The commented lines, each without its terminator.
    pub anchor: Vec<String>,
    pub text: String,
}

/// Where a revert was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevertSource {
    /// In the review overlay: it wrote the file itself and can undo it.
    Panel,
    /// In the user's editor: a buffer edit, on disk only once the user saves it.
    Editor,
}

/// What a revert put back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevertShape {
    /// Lines `from..=to` of the file as it is now.
    Lines { from: u32, to: u32 },
    /// The turn created the file; it was removed.
    Deleted,
    /// The turn removed the file; it was put back.
    Restored,
    /// The turn changed the file; it got the base back.
    WholeFile,
}

/// What a path held before or after a revert, kind and permission bits included, so an undo puts
/// back exactly that. The draft's own copy of the revert engine's type: the draft depends on no
/// engine, and the review flow converts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoState {
    Regular { blob: String, mode: u32 },
    Symlink { blob: String },
    Absent,
}

/// What an undo needs: the path as it was before the revert and as the revert wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoData {
    pub pre: UndoState,
    pub post: UndoState,
}

/// A revert to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRevert {
    pub turn: u32,
    pub path: String,
    /// The hunk's id and header, for a hunk revert.
    pub hunk: Option<(u32, String)>,
    pub shape: RevertShape,
    pub source: RevertSource,
    /// The line (from 1) where `reverted_to` starts in the file.
    pub at_line: u32,
    /// The bytes now in the file where the change was.
    pub reverted_to: Vec<u8>,
    /// The bytes they replaced.
    pub replaced: Vec<u8>,
    /// Present for a panel revert, which can be undone.
    pub undo: Option<UndoData>,
}

/// A recorded revert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevertRecord {
    /// Per tab, monotonic, separate from the comments' ids.
    pub id: u32,
    pub new: NewRevert,
    pub undone: bool,
}

/// One tab's comments and reverts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewDraft {
    last_comment: u32,
    last_revert: u32,
    comments: Vec<Comment>,
    reverts: Vec<RevertRecord>,
}

/// A line break of any kind a text could carry.
fn is_line_break(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// A quoted line made safe to put in a message: no terminator, and no break inside it that could
/// start a line of its own.
fn one_line(line: &str) -> String {
    line.trim_end_matches('\r')
        .chars()
        .map(|c| if is_line_break(c) { ' ' } else { c })
        .collect()
}

impl ReviewDraft {
    /// Adds a comment on lines `from..=to` of `path`, quoting `anchor`. The text is trimmed and must
    /// then be non-empty, at most [`MAX_COMMENT_CHARS`] characters and one line.
    pub fn add_comment(
        &mut self,
        turn: u32,
        path: &str,
        from: u32,
        to: u32,
        anchor: Vec<String>,
        text: &str,
    ) -> Result<u32, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("a comment cannot be empty".into());
        }
        if text.chars().count() > MAX_COMMENT_CHARS {
            return Err(format!("a comment is at most {MAX_COMMENT_CHARS} characters"));
        }
        if text.chars().any(is_line_break) {
            return Err("a comment is one line".into());
        }
        if path.is_empty() {
            return Err("a comment names a file".into());
        }
        if from == 0 || to < from {
            return Err(format!("{from}-{to} is not a range of lines"));
        }
        let id = self.last_comment.checked_add(1).ok_or("too many comments")?;
        self.last_comment = id;
        self.comments.push(Comment {
            id,
            turn,
            path: path.to_owned(),
            from,
            to,
            anchor: anchor.iter().map(|line| one_line(line)).collect(),
            text: text.to_owned(),
        });
        Ok(id)
    }

    pub fn remove_comment(&mut self, id: u32) -> Result<(), String> {
        let at = self
            .comments
            .iter()
            .position(|c| c.id == id)
            .ok_or_else(|| format!("no comment {id}"))?;
        self.comments.remove(at);
        Ok(())
    }

    /// Records a revert that was made and returns its id.
    pub fn record_revert(&mut self, new: NewRevert) -> u32 {
        self.last_revert = self.last_revert.saturating_add(1);
        let id = self.last_revert;
        self.reverts.push(RevertRecord { id, new, undone: false });
        id
    }

    /// The newest revert made in the panel that is not undone: what `u` undoes. A revert made in
    /// the editor is undone there, with `u`, never from here.
    pub fn last_undoable(&self) -> Option<&RevertRecord> {
        self.reverts
            .iter()
            .rev()
            .find(|r| r.new.source == RevertSource::Panel && !r.undone)
    }

    pub fn mark_undone(&mut self, id: u32) {
        if let Some(record) = self.reverts.iter_mut().find(|r| r.id == id) {
            record.undone = true;
        }
    }

    pub fn comments(&self) -> &[Comment] {
        &self.comments
    }

    pub fn reverts(&self) -> &[RevertRecord] {
        &self.reverts
    }

    pub fn is_empty(&self) -> bool {
        self.comments.is_empty() && self.reverts.is_empty()
    }

    /// Empties the draft after it was sent. The ids keep counting, so one the panel still holds is
    /// never taken for a later item.
    pub fn clear(&mut self) {
        self.comments.clear();
        self.reverts.clear();
    }
}

/// Whether a recorded revert is still what it was when it was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevertStatus {
    /// The reverted bytes are in the file.
    OnDisk,
    /// Made in the editor and not written yet.
    OnlyInEditor,
    /// The change is back.
    Undone,
    /// The place holds neither.
    ChangedSince,
}

impl RevertStatus {
    /// Why a revert is left out of the message, as the preview words it; `None` for one on disk.
    pub fn why(self) -> Option<&'static str> {
        match self {
            RevertStatus::OnDisk => None,
            RevertStatus::OnlyInEditor => Some("only in the editor, not saved"),
            RevertStatus::Undone => Some("undone"),
            RevertStatus::ChangedSince => Some("changed since"),
        }
    }

    /// A stable name, for the digest.
    pub(crate) fn name(self) -> &'static str {
        match self {
            RevertStatus::OnDisk => "on_disk",
            RevertStatus::OnlyInEditor => "only_in_editor",
            RevertStatus::Undone => "undone",
            RevertStatus::ChangedSince => "changed_since",
        }
    }
}

/// A file's lines, each with its terminator (the last may have none).
fn lines_of(bytes: &[u8]) -> Vec<&[u8]> {
    bytes.split_inclusive(|b| *b == b'\n').collect()
}

/// Whether `file`'s lines starting at line `at_line` (from 1) are exactly `bytes`.
fn holds_at(file: &[&[u8]], at_line: u32, bytes: &[u8]) -> bool {
    let want = lines_of(bytes).len();
    let Some(start) = (at_line as usize).checked_sub(1) else {
        return false;
    };
    let Some(end) = start.checked_add(want) else {
        return false;
    };
    file.get(start..end).is_some_and(|region| region.concat() == bytes)
}

fn status_of(
    project: Option<&ProjectDir>,
    record: &RevertRecord,
    editor_bytes: &BTreeMap<u32, Option<Vec<u8>>>,
) -> RevertStatus {
    if record.undone {
        return RevertStatus::Undone;
    }
    let new = &record.new;
    let current = project
        .and_then(|project| project.target(Path::new(&new.path), false).ok())
        .and_then(|target| read_current(&target).ok());
    let Some(current) = current else {
        return RevertStatus::ChangedSince;
    };
    // What a whole-file revert wrote: a regular file's bytes, or a link's target.
    let whole = |bytes: &[u8]| match &current {
        Current::Regular { bytes: now, .. } => now == bytes,
        Current::Symlink { target } => target == bytes,
        _ => false,
    };
    match new.shape {
        RevertShape::Lines { .. } => {
            let Current::Regular { bytes, .. } = &current else {
                return RevertStatus::ChangedSince;
            };
            let file = lines_of(bytes);
            let reverted = holds_at(&file, new.at_line, &new.reverted_to);
            let replaced = holds_at(&file, new.at_line, &new.replaced);
            // A revert that removed lines leaves an empty region, which every file "holds"; only
            // the removed lines being absent says it is still reverted.
            let on_disk = if new.reverted_to.is_empty() {
                !replaced || new.replaced.is_empty()
            } else {
                reverted
            };
            if on_disk {
                RevertStatus::OnDisk
            } else if replaced {
                let in_editor =
                    editor_bytes.get(&record.id).and_then(Option::as_deref) == Some(new.reverted_to.as_slice());
                if new.source == RevertSource::Editor && in_editor {
                    RevertStatus::OnlyInEditor
                } else {
                    RevertStatus::Undone
                }
            } else {
                RevertStatus::ChangedSince
            }
        }
        RevertShape::Deleted => match current {
            Current::Absent => RevertStatus::OnDisk,
            _ => RevertStatus::ChangedSince,
        },
        RevertShape::Restored | RevertShape::WholeFile => {
            if whole(&new.reverted_to) {
                RevertStatus::OnDisk
            } else {
                RevertStatus::ChangedSince
            }
        }
    }
}

/// Each recorded revert against the disk, in the draft's order: what the message may say is decided
/// here, at send time, never from what was true when the revert was made. `editor_bytes[id]` is an
/// editor revert's range as its buffer would write it, `None` when the buffer is not loaded.
///
/// Files are read through [`ProjectDir`], so a path that is not inside `root` (or runs through a
/// link out of it) is read as changed, never followed. This reads files and so runs on a worker.
pub fn check_reverts(
    root: &Path,
    draft: &ReviewDraft,
    editor_bytes: &BTreeMap<u32, Option<Vec<u8>>>,
) -> Vec<(u32, RevertStatus)> {
    let project = ProjectDir::open(root).ok();
    draft
        .reverts
        .iter()
        .map(|record| (record.id, status_of(project.as_ref(), record, editor_bytes)))
        .collect()
}
