//! Reverting a turn's change to a file, undoing that revert, and restoring what an interrupted
//! revert left: the decisions behind every write [`super::write`] makes.
//!
//! **Every write has the same preconditions, in the same order.** This window's presence is held,
//! no turn of this window runs and no other window has the project open or a turn running
//! ([`PresenceGuard::check`]); the editor said no buffer of this very file has unsaved changes
//! (an [`EditorClear`] naming it, which only [`EditorCheck`] makes); the file is reached from the
//! project root without following a link ([`ProjectDir`]); and the disk is what the job expects.
//! Right before the write the guard is asked again, the request is checked to be still wanted and
//! the file is read and compared once more, so the gap between the last look and the write is as
//! short as it can be. A job cannot be built without a guard and a clear.
//!
//! **Bytes, never text.** A file's lines come from the snapshots' blobs, split after each `\n`, so
//! line terminators and bytes that are not UTF-8 are written back exactly; the lossy text a review
//! shows is never used to decide or to write anything.
//!
//! **What a path was, whole.** Before a write, what the path is (a regular file with its bytes and
//! mode, a symlink with its target, or nothing) is stored as [`Saved`], and so is what is written;
//! both are anchored in the shadow so an undo can put the path back as it was, kind and mode
//! included.
//!
//! Every job runs git and reads the disk: build it anywhere, run it on a worker.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rmpv::Value;

use crate::editor_lines::{self, FileFormat};
use crate::editor_rpc::EditorRpc;
use crate::nvim_rpc::{Pending, RpcError};

use super::diff::{file_hunks, Side};
use super::journal::{Journal, JournalClaim, JournalEntry, JournalNote};
use super::lifecycle::{
    check_unambiguous, resolve_compare, Resolved, ReviewError, Scope, ShadowSpec, TurnRecord, TurnRef,
    MAX_OVERVIEW_DIFF_LINES,
};
use super::presence::{Blocked, PresenceGuard};
use super::shadow::{validate_session, Entry, EntryKind, Shadow, ShadowError, READ_TIMEOUT};
use super::write::{read_current, replace, Content, Current, FsHooks, ProjectDir, Target, WriteError};

/// The Lua that asks the editor about one file's buffers. Its arguments are
/// [`buffer_state_args`]; its answer is read by [`parse_buffer_state`].
pub const BUFFER_STATE_LUA: &str = include_str!("buffer_state.lua");

/// How long the editor has to say whether a file has unsaved changes. nvim queues a request while
/// it waits for a key, so silence is likely a pending key, not a crash.
const EDITOR_ANSWER_WAIT: Duration = Duration::from_secs(5);

/// The most lines a comment's anchor quotes.
const MAX_ANCHOR_LINES: u32 = 200;

/// What a revert puts back: one hunk of the file's patch, or the whole file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevertTarget {
    /// The hunk with this id and this exact header, as the review showed it.
    Hunk {
        id: u32,
        header: String,
    },
    File,
}

/// What a revert did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevertKind {
    /// One hunk: `(start, len)` of each side, as its header numbers them.
    Hunk { base: (u32, u32), end: (u32, u32) },
    /// The turn created the file; it was removed.
    Delete,
    /// The turn removed the file; it was put back.
    Restore,
    /// The turn changed the file; it got the base back.
    Replace,
}

/// One hunk with both sides' lines exactly as the files hold them, each with its terminator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactHunk {
    pub id: u32,
    pub header: String,
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    pub old_lines: Vec<Vec<u8>>,
    pub new_lines: Vec<Vec<u8>>,
}

/// A file's hunks with exact lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactHunks {
    pub path: String,
    /// Git calls the change binary, or a side is not a regular file: there are no lines to show.
    pub binary: bool,
    /// `None` when the patch is longer than the review shows.
    pub hunks: Option<Vec<ExactHunk>>,
}

/// A path's whole state before or after a write: its kind, its mode, and its bytes as a stored blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Saved {
    Regular {
        blob: String,
        mode: u32,
    },
    /// A symlink; the blob holds its target.
    Symlink {
        blob: String,
    },
    Absent,
}

impl Saved {
    fn blob(&self) -> Option<&str> {
        match self {
            Saved::Regular { blob, .. } | Saved::Symlink { blob } => Some(blob),
            Saved::Absent => None,
        }
    }
}

/// A revert that was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedRevert {
    pub session: String,
    /// The turn shown (for the session scope, the latest).
    pub turn: u32,
    pub scope: Scope,
    pub path: String,
    pub kind: RevertKind,
    /// The hunk's id and header, for a hunk.
    pub hunk: Option<(u32, String)>,
    /// The bytes now in the file where the change was: the hunk's base side (context included),
    /// or the whole base file (empty when there is none).
    pub reverted_to: Vec<u8>,
    /// The bytes they replaced: the hunk's end side, or the whole end file.
    pub replaced: Vec<u8>,
    /// The line (from 1) where `reverted_to` now starts in the file.
    pub at_line: u32,
    /// The path before the revert.
    pub pre: Saved,
    /// What the revert wrote.
    pub post: Saved,
    pub at_ms: u64,
}

/// Why a revert, an undo or a recovery wrote nothing (or, for [`Refusal::Write`], did not finish).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    Blocked(Blocked),
    /// The disk is not what the turn left.
    ChangedSinceEnd,
    /// The disk is not what the revert wrote.
    ChangedSinceRevert,
    /// The turn has no end snapshot to compare the disk with.
    Unfinished,
    /// A snapshot or a stored blob it needs was removed by retention.
    Retained,
    BinaryHunk,
    /// A directory, a FIFO, a device or a socket is where the file was.
    NotRegular,
    /// The path leaves the project, or reaches it through a link.
    NotInProject(String),
    /// The hunk asked for is not the one the snapshots give now.
    OutOfDate,
    /// The editor did not clear the file.
    Editor(String),
    /// The request's tab closed (or its session changed) before the write.
    Cancelled,
    Write(WriteError),
    Unavailable(String),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Blocked(blocked) => write!(f, "{blocked}"),
            Refusal::ChangedSinceEnd => f.write_str("changed since the turn ended; open it in the editor (o)"),
            Refusal::ChangedSinceRevert => f.write_str("changed since the revert; open it in the editor (o)"),
            Refusal::Unfinished => f.write_str("this turn has no end snapshot, so there is nothing fixed to revert"),
            Refusal::Retained => f.write_str("this turn's snapshots were removed by retention"),
            Refusal::BinaryHunk => f.write_str("a binary file reverts only whole"),
            Refusal::NotRegular => f.write_str("it is not a regular file or a link now; nothing was written"),
            Refusal::NotInProject(why) => f.write_str(why),
            Refusal::OutOfDate => f.write_str("the review is out of date; reopen it"),
            Refusal::Editor(why) => f.write_str(why),
            Refusal::Cancelled => f.write_str("the tab closed; nothing was written"),
            Refusal::Write(e) => write!(f, "{e}"),
            Refusal::Unavailable(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for Refusal {}

/// The editor's word that no buffer of one file has unsaved changes. Only [`EditorCheck::poll`]
/// makes one, and it names the absolute path that was asked about: a job given a clear for another
/// path refuses. Not `Clone`: one answer clears one job.
#[derive(Debug)]
pub struct EditorClear {
    path: PathBuf,
}

impl EditorClear {
    /// The absolute path the editor was asked about.
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests(path: &Path) -> EditorClear {
        EditorClear {
            path: path.to_path_buf(),
        }
    }
}

/// What the editor holds of one file, as [`BUFFER_STATE_LUA`] answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferState {
    /// A loaded buffer shows the file.
    pub found: bool,
    /// Some buffer of the file has unsaved changes.
    pub modified: bool,
    /// The asked range of the first such buffer, its lines without terminators; `None` when no
    /// range was asked or it is not in the buffer.
    pub lines: Option<Vec<Vec<u8>>>,
    pub fileformat: FileFormat,
    pub eol: bool,
    pub line_count: u32,
}

impl BufferState {
    /// The file bytes of the asked range, starting at line `from` (from 1), as this buffer would
    /// write them: its `'fileformat'` decides each terminator, and its `'eol'` whether the
    /// buffer's last line gets one. That is right for a buffer as it was read; one whose `'eol'`
    /// was turned off by hand still gets a final newline from nvim's `'fixeol'`, which this does
    /// not model.
    pub fn range_bytes(&self, from: u32) -> Option<Vec<u8>> {
        let lines = self.lines.as_ref()?;
        let len = u32::try_from(lines.len()).ok()?;
        let ends_buffer = len > 0 && from.checked_add(len - 1) == Some(self.line_count);
        editor_lines::buffer_range_bytes(lines, self.fileformat, self.eol, ends_buffer)
    }
}

/// A path as nvim receives it: text when it is UTF-8, raw bytes otherwise, which nvim's unpacker
/// turns into the same Lua string, so it still names the same file.
fn path_value(path: &Path) -> Value {
    use std::os::unix::ffi::OsStrExt;
    match path.to_str() {
        Some(text) => Value::from(text),
        None => Value::Binary(path.as_os_str().as_bytes().to_vec()),
    }
}

/// The arguments of [`BUFFER_STATE_LUA`]: the absolute path, and the range `(first line from 1,
/// number of lines)` to read, if any.
pub fn buffer_state_args(abs_path: &Path, lines: Option<(u32, u32)>) -> Vec<Value> {
    let range = match lines {
        Some((start, count)) => Value::Array(vec![Value::from(start), Value::from(count)]),
        None => Value::Nil,
    };
    vec![path_value(abs_path), range]
}

/// [`BUFFER_STATE_LUA`]'s answer.
pub fn parse_buffer_state(v: &Value) -> Result<BufferState, String> {
    let map = v.as_map().ok_or_else(|| format!("not a table: {v}"))?;
    let get = |key: &str| map.iter().find(|(k, _)| k.as_str() == Some(key)).map(|(_, v)| v);
    let flag = |key: &str| -> Result<bool, String> {
        match get(key) {
            None | Some(Value::Nil) => Ok(false),
            Some(Value::Boolean(b)) => Ok(*b),
            Some(other) => Err(format!("{key} is not a boolean: {other}")),
        }
    };
    let found = flag("found")?;
    let modified = flag("modified")?;
    let eol = match get("eol") {
        None | Some(Value::Nil) => true,
        Some(Value::Boolean(b)) => *b,
        Some(other) => return Err(format!("eol is not a boolean: {other}")),
    };
    // A buffer that says nothing of its format is compared with nothing.
    let fileformat = match get("fileformat") {
        None | Some(Value::Nil) => FileFormat::Other,
        Some(Value::String(s)) => editor_lines::parse_file_format(s.as_str().unwrap_or("")),
        Some(other) => return Err(format!("fileformat is not a string: {other}")),
    };
    let line_count = match get("line_count") {
        None | Some(Value::Nil) => 0,
        Some(n) => n
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| format!("line_count is not a count: {n}"))?,
    };
    let lines = match get("lines") {
        None | Some(Value::Nil) => None,
        // An empty Lua table may arrive as either.
        Some(Value::Map(m)) if m.is_empty() => Some(Vec::new()),
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .map(|item| match item {
                    // nvim sends a Lua string as msgpack str whatever its bytes are; they are kept.
                    Value::String(s) => Ok(s.as_bytes().to_vec()),
                    Value::Binary(b) => Ok(b.clone()),
                    other => Err(format!("a line is not a string: {other}")),
                })
                .collect::<Result<Vec<_>, String>>()?,
        ),
        Some(other) => return Err(format!("lines is not a list: {other}")),
    };
    Ok(BufferState {
        found,
        modified,
        lines,
        fileformat,
        eol,
        line_count,
    })
}

/// Asking the editor whether a file has unsaved changes, polled without waiting.
pub struct EditorCheck {
    path: PathBuf,
    started: Instant,
    state: CheckState,
}

enum CheckState {
    Asked(Pending),
    /// The result, until it is taken.
    Done(Option<Result<EditorClear, Refusal>>),
}

impl EditorCheck {
    /// Sends the question about `abs_path` to `rpc`. With no editor, the check is already refused.
    pub fn start(rpc: Option<&dyn EditorRpc>, abs_path: &Path, now: Instant) -> EditorCheck {
        let state = match rpc {
            Some(rpc) if rpc.target().is_some() => {
                CheckState::Asked(rpc.exec_lua(BUFFER_STATE_LUA, buffer_state_args(abs_path, None)))
            }
            _ => CheckState::Done(Some(Err(no_editor(abs_path)))),
        };
        EditorCheck {
            path: abs_path.to_path_buf(),
            started: now,
            state,
        }
    }

    /// The answer, once: `None` while it is on its way, and after it was taken. An editor silent
    /// for five seconds is refused.
    pub fn poll(&mut self, now: Instant) -> Option<Result<EditorClear, Refusal>> {
        let reply = match &mut self.state {
            CheckState::Done(result) => return result.take(),
            CheckState::Asked(pending) => pending.try_take(),
        };
        let result = match reply {
            Some(Ok(value)) => self.judge(&value),
            Some(Err(e)) => Err(self.failed(e)),
            None if now.saturating_duration_since(self.started) >= EDITOR_ANSWER_WAIT => Err(Refusal::Editor(
                "the editor did not answer; is it waiting for a key?".into(),
            )),
            None => return None,
        };
        // Dropping the question is safe for both transports: a late answer goes nowhere.
        self.state = CheckState::Done(None);
        Some(result)
    }

    fn judge(&self, value: &Value) -> Result<EditorClear, Refusal> {
        let state = parse_buffer_state(value)
            .map_err(|e| Refusal::Editor(format!("the editor's answer could not be read: {e}")))?;
        // `modified` alone refuses: an answer that says modified but not found is still one whose
        // buffer may hold unsaved changes, and every doubt here resolves toward refusing.
        if state.modified {
            return Err(Refusal::Editor(format!(
                "{} has unsaved changes in the editor; write or discard them first",
                self.path.display()
            )));
        }
        Ok(EditorClear {
            path: self.path.clone(),
        })
    }

    fn failed(&self, e: RpcError) -> Refusal {
        match e {
            RpcError::Unavailable(_) => no_editor(&self.path),
            RpcError::Closed => Refusal::Editor(format!(
                "the editor went away before it said whether {} has unsaved changes",
                self.path.display()
            )),
            RpcError::Nvim(_) | RpcError::Encode(_) => Refusal::Editor(format!(
                "the editor could not say whether {} has unsaved changes: {e}",
                self.path.display()
            )),
        }
    }
}

fn no_editor(path: &Path) -> Refusal {
    Refusal::Editor(format!(
        "no editor is connected, so Eitri cannot tell whether {} has unsaved changes",
        path.display()
    ))
}

/// What to do with an interrupted revert. A restore cannot be built without the editor's clear.
#[derive(Debug)]
pub enum RecoverAnswer {
    Restore(EditorClear),
    Dismiss,
}

/// Called right before the last checks of a write; tests use it to change the world there.
type FinalHook = Arc<dyn Fn() + Send + Sync>;

/// One file's hunks with exact lines; [`run`](Self::run) on a worker.
pub struct ExactHunksJob {
    pub(crate) spec: ShadowSpec,
    pub(crate) session: String,
    pub(crate) records: Vec<TurnRecord>,
    pub(crate) turn: u32,
    pub(crate) scope: Scope,
    pub(crate) path: PathBuf,
}

/// A hunk or whole-file revert; [`run`](Self::run) on a worker.
pub struct RevertJob {
    pub(crate) spec: ShadowSpec,
    pub(crate) guard: PresenceGuard,
    pub(crate) wanted: Arc<AtomicBool>,
    pub(crate) session: String,
    pub(crate) records: Vec<TurnRecord>,
    pub(crate) turn: u32,
    pub(crate) scope: Scope,
    pub(crate) path: PathBuf,
    pub(crate) target: RevertTarget,
    pub(crate) clear: EditorClear,
    pub(crate) final_hook: Option<FinalHook>,
}

/// The undo of a revert; [`run`](Self::run) on a worker.
pub struct UndoJob {
    pub(crate) spec: ShadowSpec,
    pub(crate) guard: PresenceGuard,
    pub(crate) wanted: Arc<AtomicBool>,
    pub(crate) session: String,
    pub(crate) path: PathBuf,
    pub(crate) pre: Saved,
    pub(crate) post: Saved,
    pub(crate) clear: EditorClear,
    pub(crate) final_hook: Option<FinalHook>,
}

/// A comment's quoted lines; [`run`](Self::run) on a worker.
pub struct AnchorJob {
    pub(crate) spec: ShadowSpec,
    pub(crate) session: String,
    pub(crate) records: Vec<TurnRecord>,
    pub(crate) turn: u32,
    pub(crate) scope: Scope,
    pub(crate) path: PathBuf,
    pub(crate) from: u32,
    pub(crate) to: u32,
}

/// The interrupted reverts; [`run`](Self::run) on a worker.
pub struct RecoveriesJob {
    pub(crate) spec: ShadowSpec,
}

/// Restoring or forgetting one interrupted revert; [`run`](Self::run) on a worker.
pub struct RecoverJob {
    pub(crate) spec: ShadowSpec,
    pub(crate) guard: PresenceGuard,
    pub(crate) entry: String,
    pub(crate) answer: RecoverAnswer,
    pub(crate) final_hook: Option<FinalHook>,
}

#[cfg(any(test, feature = "test-support"))]
impl RevertJob {
    /// Runs `hook` right before the last checks.
    pub fn on_final_check(mut self, hook: Arc<dyn Fn() + Send + Sync>) -> Self {
        self.final_hook = Some(hook);
        self
    }
}

#[cfg(any(test, feature = "test-support"))]
impl UndoJob {
    /// Runs `hook` right before the last checks.
    pub fn on_final_check(mut self, hook: Arc<dyn Fn() + Send + Sync>) -> Self {
        self.final_hook = Some(hook);
        self
    }
}

#[cfg(any(test, feature = "test-support"))]
impl RecoverJob {
    /// Runs `hook` right before the last checks.
    pub fn on_final_check(mut self, hook: Arc<dyn Fn() + Send + Sync>) -> Self {
        self.final_hook = Some(hook);
        self
    }
}

impl ExactHunksJob {
    pub fn run(self) -> Result<ExactHunks, ReviewError> {
        let unavailable = |e: ShadowError| ReviewError::Unavailable(e.to_string());
        let shadow = self.spec.open().map_err(ReviewError::Unavailable)?;
        let resolved = resolve_compare(&shadow, &self.session, &self.records, TurnRef::N(self.turn), self.scope)?;
        let Side::Snapshot(end_commit) = &resolved.to else {
            // The lines must come from a snapshot, never from a disk that may be changing.
            return Err(ReviewError::NotReady("this turn has no end snapshot yet".into()));
        };
        if resolved.too_large.contains(&self.path) {
            return Err(ReviewError::TooLarge(self.path));
        }
        check_unambiguous(&shadow, &resolved.from, &resolved.to, &self.path)?;
        // Held across the patch and the reads, so retention cannot remove a side in between.
        let _lock = shadow
            .lock_shared_until(Instant::now() + READ_TIMEOUT)
            .map_err(unavailable)?;
        let diff = file_hunks(
            &shadow,
            &resolved.from,
            &resolved.to,
            &self.path,
            MAX_OVERVIEW_DIFF_LINES,
        )
        .map_err(unavailable)?;
        let base = shadow.read_entry(&resolved.from, &self.path).map_err(unavailable)?;
        let end = shadow.read_entry(end_commit, &self.path).map_err(unavailable)?;
        let not_a_file = |e: &Option<Entry>| e.as_ref().is_some_and(|e| e.kind != EntryKind::Blob);
        let path = self.path.to_string_lossy().into_owned();
        let Some(diff) = diff else {
            return Ok(ExactHunks {
                path,
                binary: not_a_file(&base) || not_a_file(&end),
                hunks: None,
            });
        };
        let binary = diff.binary || not_a_file(&base) || not_a_file(&end);
        if binary {
            return Ok(ExactHunks {
                path,
                binary,
                hunks: Some(Vec::new()),
            });
        }
        let base_lines = base.map_or_else(Vec::new, |e| lines_of(&e.bytes));
        let end_lines = end.map_or_else(Vec::new, |e| lines_of(&e.bytes));
        let mismatch = || ReviewError::Unavailable("the patch does not match the snapshots".into());
        let mut hunks = Vec::with_capacity(diff.hunks.len());
        for h in diff.hunks {
            let old_lines = region(&base_lines, (h.old_start, h.old_len))
                .ok_or_else(mismatch)?
                .to_vec();
            let new_lines = region(&end_lines, (h.new_start, h.new_len))
                .ok_or_else(mismatch)?
                .to_vec();
            hunks.push(ExactHunk {
                id: h.id,
                header: h.header,
                old_start: h.old_start,
                old_len: h.old_len,
                new_start: h.new_start,
                new_len: h.new_len,
                old_lines,
                new_lines,
            });
        }
        Ok(ExactHunks {
            path,
            binary: false,
            hunks: Some(hunks),
        })
    }
}

impl RevertJob {
    pub fn run(self) -> Result<AppliedRevert, Refusal> {
        let rel = self.path.clone();
        let wanted = Some(self.wanted.as_ref());
        still_wanted(wanted)?;
        self.guard.check().map_err(Refusal::Blocked)?;
        let (root, target) = locate(&self.clear, &self.spec.root, &rel, &self.session)?;
        let (shadow, journal) = open_store(&self.spec)?;

        // Held from the resolve until the blobs are anchored: retention, which takes it
        // exclusively, can neither remove a snapshot between its resolve and its read nor prune a
        // fresh blob before its anchor exists.
        let lock = shadow
            .lock_shared_until(Instant::now() + READ_TIMEOUT)
            .map_err(store_failed)?;
        let resolved = resolve(&shadow, &self.session, &self.records, self.turn, self.scope)?;
        let Side::Snapshot(end_commit) = resolved.to.clone() else {
            return Err(Refusal::Unfinished);
        };
        refuse_unrevertable(&shadow, &resolved, &rel)?;
        let base = read_entry(&shadow, &resolved.from, &rel)?;
        let end = read_entry(&shadow, &end_commit, &rel)?;
        if [&base, &end]
            .iter()
            .any(|e| e.as_ref().is_some_and(|e| e.kind == EntryKind::Gitlink))
        {
            return Err(Refusal::Unavailable(format!(
                "{} is a nested repository; it is not reverted",
                rel.display()
            )));
        }
        let current = read_current(&target).map_err(|e| read_failed(&rel, e))?;
        if current == Current::Other {
            return Err(Refusal::NotRegular);
        }
        let planned = match &self.target {
            RevertTarget::Hunk { id, header } => {
                plan_hunk(&shadow, &resolved, &end_commit, &rel, *id, header, base, end, &current)?
            }
            RevertTarget::File => plan_file(&rel, base, end, &current)?,
        };
        let pre = save_current(&shadow, &current)?;
        let post = save_content(&shadow, &planned.content)?;
        let at_ms = wall_clock_ms();
        shadow
            .anchor_revert(&self.session, &stamp(at_ms), pre.blob(), post.blob())
            .map_err(store_failed)?;
        drop(lock);

        let target = final_checks(
            &self.guard,
            wanted,
            &root,
            &rel,
            &current,
            !matches!(planned.content, Content::Absent),
            self.final_hook.as_ref(),
            || Refusal::ChangedSinceEnd,
        )?;
        let note = journal_note(&self.session, &rel, &current, &pre, &post);
        replace(&target, &planned.content, &note, &journal, &shadow, &FsHooks::default()).map_err(Refusal::Write)?;
        Ok(AppliedRevert {
            session: self.session,
            turn: resolved.current,
            scope: self.scope,
            path: rel.to_string_lossy().into_owned(),
            kind: planned.kind,
            hunk: planned.hunk,
            reverted_to: planned.reverted_to,
            replaced: planned.replaced,
            at_line: planned.at_line,
            pre,
            post,
            at_ms,
        })
    }
}

impl UndoJob {
    pub fn run(self) -> Result<(), Refusal> {
        let rel = self.path.clone();
        let wanted = Some(self.wanted.as_ref());
        still_wanted(wanted)?;
        self.guard.check().map_err(Refusal::Blocked)?;
        let (root, target) = locate(&self.clear, &self.spec.root, &rel, &self.session)?;
        let (shadow, journal) = open_store(&self.spec)?;
        let stored = |blob: &str| -> Result<Vec<u8>, Refusal> {
            shadow.read_stored(blob).map_err(store_failed)?.ok_or(Refusal::Retained)
        };

        let current = read_current(&target).map_err(|e| read_failed(&rel, e))?;
        // The kind and the bytes must be what the revert wrote; the mode may have changed since,
        // which is not a change of content.
        let as_written = match (&self.post, &current) {
            (Saved::Absent, Current::Absent) => true,
            (Saved::Symlink { blob }, Current::Symlink { target }) => stored(blob)? == *target,
            (Saved::Regular { blob, .. }, Current::Regular { bytes, .. }) => stored(blob)? == *bytes,
            (_, Current::Other) => return Err(Refusal::NotRegular),
            _ => false,
        };
        if !as_written {
            return Err(Refusal::ChangedSinceRevert);
        }
        let content = match &self.pre {
            Saved::Regular { blob, mode } => Content::Bytes {
                bytes: stored(blob)?,
                mode: *mode,
            },
            Saved::Symlink { blob } => Content::Symlink { target: stored(blob)? },
            Saved::Absent => Content::Absent,
        };
        if content == Content::Absent && current == Current::Absent {
            return Err(Refusal::Unavailable("there is nothing to undo".into()));
        }

        let target = final_checks(
            &self.guard,
            wanted,
            &root,
            &rel,
            &current,
            !matches!(content, Content::Absent),
            self.final_hook.as_ref(),
            || Refusal::ChangedSinceRevert,
        )?;
        // The path now holds `post`, and `pre` is what goes in.
        let note = journal_note(&self.session, &rel, &current, &self.post, &self.pre);
        replace(&target, &content, &note, &journal, &shadow, &FsHooks::default()).map_err(Refusal::Write)
    }
}

impl AnchorJob {
    pub fn run(self) -> Result<Vec<String>, ReviewError> {
        let (from, to) = (self.from, self.to);
        if from == 0 || from > to {
            return Err(ReviewError::Unavailable(format!("lines {from}-{to} are not a range")));
        }
        if to - from >= MAX_ANCHOR_LINES {
            return Err(ReviewError::Unavailable(format!(
                "a comment quotes at most {MAX_ANCHOR_LINES} lines"
            )));
        }
        let unavailable = |e: ShadowError| ReviewError::Unavailable(e.to_string());
        let shadow = self.spec.open().map_err(ReviewError::Unavailable)?;
        let resolved = resolve_compare(&shadow, &self.session, &self.records, TurnRef::N(self.turn), self.scope)?;
        if resolved.too_large.contains(&self.path) {
            return Err(ReviewError::TooLarge(self.path));
        }
        check_unambiguous(&shadow, &resolved.from, &resolved.to, &self.path)?;
        let shown = self.path.display().to_string();
        let not_a_file = || ReviewError::Unavailable(format!("{shown} is not a regular file at the end of the turn"));
        let bytes = match &resolved.to {
            Side::Snapshot(end) => match shadow.read_entry(end, &self.path).map_err(unavailable)? {
                Some(Entry {
                    kind: EntryKind::Blob,
                    bytes,
                    ..
                }) => bytes,
                _ => return Err(not_a_file()),
            },
            Side::WorkTree => {
                let root = ProjectDir::open(&self.spec.root).map_err(|e| ReviewError::Unavailable(e.to_string()))?;
                let target = root
                    .target(&self.path, false)
                    .map_err(|e| ReviewError::Unavailable(e.to_string()))?;
                match read_current(&target).map_err(|e| ReviewError::Unavailable(format!("{shown}: {e}")))? {
                    Current::Regular { bytes, .. } => bytes,
                    _ => return Err(not_a_file()),
                }
            }
        };
        let lines = lines_of(&bytes);
        if to as usize > lines.len() {
            return Err(ReviewError::Unavailable(format!(
                "lines {from}-{to} are not in {shown}, which has {}",
                lines.len()
            )));
        }
        // Quoted text, so a lossy reading of bytes that are not UTF-8 does no harm here.
        Ok(editor_lines::to_buffer_lines(&lines[from as usize - 1..to as usize])
            .into_iter()
            .map(|line| String::from_utf8_lossy(&line.text).into_owned())
            .collect())
    }
}

impl RecoveriesJob {
    pub fn run(self) -> Vec<JournalEntry> {
        let Some(dir) = self.spec.review_dir.as_deref() else {
            return Vec::new();
        };
        match Journal::open(dir) {
            Ok(journal) => journal.pending(),
            Err(e) => {
                eprintln!("[review] the revert journal could not be opened: {e}");
                Vec::new()
            }
        }
    }
}

impl RecoverJob {
    pub fn run(self) -> Result<(), Refusal> {
        match self.answer {
            RecoverAnswer::Dismiss => {
                let (shadow, journal) = open_store(&self.spec)?;
                let (entry, _claim) = claim(&journal, &self.entry)?;
                journal.remove(&entry.id, &shadow).map_err(store_failed)
            }
            RecoverAnswer::Restore(ref clear) => {
                self.guard.check().map_err(Refusal::Blocked)?;
                let (shadow, journal) = open_store(&self.spec)?;
                // Held to the end, so another window cannot restore the same entry meanwhile.
                let (entry, _claim) = claim(&journal, &self.entry)?;
                restore_entry(
                    &self.spec,
                    &self.guard,
                    clear,
                    &shadow,
                    &journal,
                    &entry,
                    self.final_hook.as_ref(),
                )
            }
        }
    }
}

/// Puts back what `entry` names. There is no comparison with an expected state: the bytes a crash
/// left are unknown, so they are kept (stored and anchored) instead.
fn restore_entry(
    spec: &ShadowSpec,
    guard: &PresenceGuard,
    clear: &EditorClear,
    shadow: &Shadow,
    journal: &Journal,
    entry: &JournalEntry,
    hook: Option<&FinalHook>,
) -> Result<(), Refusal> {
    let rel = entry.path.clone();
    let (root, target) = locate(clear, &spec.root, &rel, &entry.session)?;
    let current = read_current(&target).map_err(|e| read_failed(&rel, e))?;
    if current == Current::Other {
        return Err(Refusal::NotRegular);
    }
    let pre_blob = entry.pre.as_deref().ok_or_else(|| {
        Refusal::Unavailable("this interrupted revert kept no copy of the file's earlier bytes".into())
    })?;
    let pre_bytes = shadow
        .read_stored(pre_blob)
        .map_err(store_failed)?
        .ok_or(Refusal::Retained)?;
    let now = save_current(shadow, &current)?;
    shadow
        .anchor_revert(&entry.session, &stamp(wall_clock_ms()), now.blob(), Some(pre_blob))
        .map_err(store_failed)?;
    let content = Content::Bytes {
        bytes: pre_bytes,
        mode: entry.mode,
    };
    let shown = rel.display().to_string();
    let target = final_checks(guard, None, &root, &rel, &current, true, hook, || {
        Refusal::Unavailable(format!(
            "{shown} changed while it was being restored; nothing was written"
        ))
    })?;
    let restored = Saved::Regular {
        blob: pre_blob.to_owned(),
        mode: entry.mode,
    };
    let note = journal_note(&entry.session, &rel, &current, &now, &restored);
    replace(&target, &content, &note, journal, shadow, &FsHooks::default()).map_err(Refusal::Write)?;
    journal.remove(&entry.id, shadow).map_err(|e| {
        Refusal::Unavailable(format!(
            "{shown} was restored, but its interrupted revert could not be forgotten: {e}"
        ))
    })
}

// ---- the steps every write shares ----------------------------------------------------------------

/// `Cancelled` once the request is no longer wanted. A recovery has no tab and no flag.
fn still_wanted(wanted: Option<&AtomicBool>) -> Result<(), Refusal> {
    match wanted {
        Some(flag) if !flag.load(Ordering::SeqCst) => Err(Refusal::Cancelled),
        _ => Ok(()),
    }
}

/// The editor's clear must be for this very file, and the file is reached from the project root
/// without following a link. Nothing is read or created yet.
fn locate(clear: &EditorClear, root: &Path, rel: &Path, session: &str) -> Result<(ProjectDir, Target), Refusal> {
    if clear.path() != root.join(rel) {
        return Err(Refusal::Unavailable("the editor check was for another file".into()));
    }
    validate_session(session).map_err(|e| Refusal::Unavailable(e.to_string()))?;
    let dir = ProjectDir::open(root)
        .map_err(|e| Refusal::Unavailable(format!("the project directory could not be opened: {e}")))?;
    let target = dir.target(rel, false).map_err(write_refusal)?;
    Ok((dir, target))
}

/// The last look before a write: the guard again, the request still wanted, and the file exactly
/// as it was read (`expected`); otherwise `changed`.
///
/// The file is reached from the project root once more, component by component, and the write goes
/// through that fresh descriptor, never the one opened when the job started: a directory moved out
/// of the project meanwhile keeps working through a descriptor opened before the move, so writing
/// through the old one would land outside. When something is to be put where nothing is, the
/// parent directories are made only now, after every other check passed.
#[allow(clippy::too_many_arguments)]
fn final_checks(
    guard: &PresenceGuard,
    wanted: Option<&AtomicBool>,
    root: &ProjectDir,
    rel: &Path,
    expected: &Current,
    writes_something: bool,
    hook: Option<&FinalHook>,
    changed: impl Fn() -> Refusal,
) -> Result<Target, Refusal> {
    if let Some(hook) = hook {
        hook();
    }
    guard.check().map_err(Refusal::Blocked)?;
    still_wanted(wanted)?;
    let create_missing = *expected == Current::Absent && writes_something;
    let target = root.target(rel, create_missing).map_err(write_refusal)?;
    if read_current(&target).map_err(|e| read_failed(rel, e))? != *expected {
        return Err(changed());
    }
    still_wanted(wanted)?;
    Ok(target)
}

fn write_refusal(e: WriteError) -> Refusal {
    match e {
        WriteError::Outside(why) => Refusal::NotInProject(why),
        other => Refusal::Write(other),
    }
}

fn open_store(spec: &ShadowSpec) -> Result<(Shadow, Journal), Refusal> {
    let shadow = spec.open().map_err(Refusal::Unavailable)?;
    let journal = Journal::open(shadow.review_dir()).map_err(store_failed)?;
    Ok((shadow, journal))
}

fn claim(journal: &Journal, id: &str) -> Result<(JournalEntry, JournalClaim), Refusal> {
    journal
        .claim(id)
        .map_err(store_failed)?
        .ok_or_else(|| Refusal::Unavailable("this interrupted revert was already handled".into()))
}

fn store_failed(e: ShadowError) -> Refusal {
    Refusal::Unavailable(e.to_string())
}

fn read_failed(rel: &Path, e: std::io::Error) -> Refusal {
    Refusal::Unavailable(format!("{} could not be read: {e}", rel.display()))
}

/// The comparison, resolved now, with this module's refusals.
fn resolve(
    shadow: &Shadow,
    session: &str,
    records: &[TurnRecord],
    turn: u32,
    scope: Scope,
) -> Result<Resolved, Refusal> {
    resolve_compare(shadow, session, records, TurnRef::N(turn), scope).map_err(|e| match e {
        // A turn this run recorded whose snapshots retention has since removed is not listed;
        // when it was the session's only one, the session has no turn left at all.
        ReviewError::NoSuchTurn(_) | ReviewError::NoTurns => Refusal::Retained,
        other => Refusal::Unavailable(other.to_string()),
    })
}

fn read_entry(shadow: &Shadow, commit: &str, rel: &Path) -> Result<Option<Entry>, Refusal> {
    shadow.read_entry(commit, rel).map_err(|e| match e {
        ShadowError::NoSuchCommit(_) => Refusal::Retained,
        other => store_failed(other),
    })
}

/// Paths whose snapshots cannot say what the base was.
fn refuse_unrevertable(shadow: &Shadow, resolved: &Resolved, rel: &Path) -> Result<(), Refusal> {
    // A file left out of a snapshot reads as absent there: a base too large to keep would look
    // like a file the turn created, and reverting it would delete it.
    if resolved.too_large.contains(rel) {
        return Err(Refusal::Unavailable(format!(
            "{} was too large to snapshot, so it cannot be reverted",
            rel.display()
        )));
    }
    check_unambiguous(shadow, &resolved.from, &resolved.to, rel).map_err(|e| Refusal::Unavailable(e.to_string()))
}

/// What a revert will write, and what it records about it.
struct Planned {
    kind: RevertKind,
    content: Content,
    hunk: Option<(u32, String)>,
    reverted_to: Vec<u8>,
    replaced: Vec<u8>,
    at_line: u32,
}

#[allow(clippy::too_many_arguments)]
fn plan_hunk(
    shadow: &Shadow,
    resolved: &Resolved,
    end_commit: &str,
    rel: &Path,
    id: u32,
    header: &str,
    base: Option<Entry>,
    end: Option<Entry>,
    current: &Current,
) -> Result<Planned, Refusal> {
    let diff = file_hunks(
        shadow,
        &resolved.from,
        &Side::Snapshot(end_commit.to_owned()),
        rel,
        MAX_OVERVIEW_DIFF_LINES,
    )
    .map_err(store_failed)?
    .ok_or_else(|| {
        Refusal::Unavailable("this file's patch is too long to revert by hunk; revert the whole file".into())
    })?;
    if diff.binary {
        return Err(Refusal::BinaryHunk);
    }
    let hunk = diff
        .hunks
        .iter()
        .find(|h| h.id == id && h.header == header)
        .ok_or(Refusal::OutOfDate)?;
    let (base, end) = match (base, end) {
        (Some(base), Some(end)) if base.kind == EntryKind::Blob && end.kind == EntryKind::Blob => (base, end),
        (Some(base), Some(end)) if base.kind == EntryKind::Symlink || end.kind == EntryKind::Symlink => {
            return Err(Refusal::Unavailable("a symlink reverts only whole, as a link".into()))
        }
        _ => {
            return Err(Refusal::Unavailable(
                "a file this turn created or deleted reverts only whole".into(),
            ))
        }
    };
    let (bytes, mode) = match current {
        Current::Regular { bytes, mode } => (bytes, *mode),
        Current::Other => return Err(Refusal::NotRegular),
        Current::Absent | Current::Symlink { .. } => return Err(Refusal::ChangedSinceEnd),
    };
    let spliced = splice_hunk(
        &lines_of(&base.bytes),
        &lines_of(&end.bytes),
        &lines_of(bytes),
        (hunk.old_start, hunk.old_len),
        (hunk.new_start, hunk.new_len),
    )?;
    Ok(Planned {
        kind: RevertKind::Hunk {
            base: (hunk.old_start, hunk.old_len),
            end: (hunk.new_start, hunk.new_len),
        },
        // Into the current file, so the user's edits outside the region stay; with its own mode.
        content: Content::Bytes {
            bytes: spliced.bytes,
            mode,
        },
        hunk: Some((hunk.id, hunk.header.clone())),
        reverted_to: spliced.reverted_to,
        replaced: spliced.replaced,
        at_line: spliced.at_line,
    })
}

fn plan_file(rel: &Path, base: Option<Entry>, end: Option<Entry>, current: &Current) -> Result<Planned, Refusal> {
    // Every byte of the path must be what the turn left, and a restore needs the path absent.
    let as_left = match (&end, current) {
        (_, Current::Other) => return Err(Refusal::NotRegular),
        (None, Current::Absent) => true,
        (Some(e), Current::Regular { bytes, .. }) => e.kind == EntryKind::Blob && *bytes == e.bytes,
        (Some(e), Current::Symlink { target }) => e.kind == EntryKind::Symlink && *target == e.bytes,
        _ => false,
    };
    if !as_left {
        return Err(Refusal::ChangedSinceEnd);
    }
    let unchanged = || Refusal::Unavailable(format!("this turn did not change {}", rel.display()));
    let bytes_of = |e: &Option<Entry>| e.as_ref().map_or_else(Vec::new, |e| e.bytes.clone());
    let (kind, content) = match (&base, &end) {
        (None, None) => return Err(unchanged()),
        (None, Some(_)) => (RevertKind::Delete, Content::Absent),
        (Some(b), None) => (RevertKind::Restore, entry_content(b, b.mode)),
        (Some(b), Some(e)) if b == e => return Err(unchanged()),
        (Some(b), Some(e)) => (RevertKind::Replace, entry_content(b, replace_mode(b, e, current))),
    };
    Ok(Planned {
        kind,
        content,
        hunk: None,
        reverted_to: bytes_of(&base),
        replaced: bytes_of(&end),
        at_line: 1,
    })
}

fn entry_content(entry: &Entry, mode: u32) -> Content {
    match entry.kind {
        EntryKind::Symlink => Content::Symlink {
            target: entry.bytes.clone(),
        },
        _ => Content::Bytes {
            bytes: entry.bytes.clone(),
            mode,
        },
    }
}

/// The mode a whole-file replace gives a regular file. Git records only 0644 or 0755, so the
/// base's mode would widen a file the user had made 0600: the file keeps its own bits, and only
/// an execute bit the turn changed is changed back. Where no regular file is (the turn left a
/// link), the base's mode is all there is.
fn replace_mode(base: &Entry, end: &Entry, current: &Current) -> u32 {
    match current {
        Current::Regular { mode, .. } if end.kind == EntryKind::Blob => {
            if base.mode == end.mode {
                *mode
            } else if base.mode & 0o111 != 0 {
                // Execute for whoever may read, as `chmod +x` gives it.
                mode | ((mode & 0o444) >> 2)
            } else {
                mode & !0o111
            }
        }
        _ => base.mode,
    }
}

/// A hunk's base region put in place of its end region in the current file.
#[derive(Debug, PartialEq, Eq)]
struct Spliced {
    bytes: Vec<u8>,
    reverted_to: Vec<u8>,
    replaced: Vec<u8>,
    at_line: u32,
}

/// Puts `base`'s lines `old` (a header's `(start, len)`) where `end`'s lines `new` are in `cur`,
/// after checking that `cur` holds exactly those end lines at exactly that place: a region that
/// changed or moved is refused, never searched for.
fn splice_hunk(
    base: &[Vec<u8>],
    end: &[Vec<u8>],
    cur: &[Vec<u8>],
    old: (u32, u32),
    new: (u32, u32),
) -> Result<Spliced, Refusal> {
    let base_region = region(base, old).ok_or(Refusal::OutOfDate)?;
    let end_region = region(end, new).ok_or(Refusal::OutOfDate)?;
    let at = range_index(new.0, new.1);
    if new.1 == 0 {
        // An empty end region says nothing about where it is; with three lines of context it
        // happens only when the turn left the file empty, which the file must still be.
        if cur != end {
            return Err(Refusal::ChangedSinceEnd);
        }
    } else if region(cur, new) != Some(end_region) {
        return Err(Refusal::ChangedSinceEnd);
    }
    let (before, rest) = cur.split_at(at);
    let after = &rest[end_region.len()..];
    // A line without a newline can only be a file's last: base lines ending that way must not be
    // followed by more lines, nor follow one.
    let open_end = |lines: &[Vec<u8>]| lines.last().is_some_and(|l| !l.ends_with(b"\n"));
    if !base_region.is_empty() && ((open_end(base_region) && !after.is_empty()) || open_end(before)) {
        return Err(Refusal::ChangedSinceEnd);
    }
    let reverted_to = base_region.concat();
    let mut bytes = before.concat();
    bytes.extend_from_slice(&reverted_to);
    bytes.extend_from_slice(&after.concat());
    Ok(Spliced {
        bytes,
        reverted_to,
        replaced: end_region.concat(),
        at_line: u32::try_from(at).unwrap_or(u32::MAX).saturating_add(1),
    })
}

/// Where a header's range `(start, len)` begins as an index into the file's lines. A zero-length
/// range sits after line `start` (git's convention), so it begins at index `start`, not
/// `start - 1`.
fn range_index(start: u32, len: u32) -> usize {
    if len == 0 {
        start as usize
    } else {
        (start as usize).saturating_sub(1)
    }
}

/// A header's range of `lines`; `None` when it does not fit.
fn region(lines: &[Vec<u8>], (start, len): (u32, u32)) -> Option<&[Vec<u8>]> {
    if len > 0 && start == 0 {
        return None;
    }
    let at = range_index(start, len);
    lines.get(at..at.checked_add(len as usize)?)
}

/// A file's lines, each with its terminator (the last may have none).
fn lines_of(bytes: &[u8]) -> Vec<Vec<u8>> {
    bytes.split_inclusive(|b| *b == b'\n').map(<[u8]>::to_vec).collect()
}

/// `current` stored as a [`Saved`].
fn save_current(shadow: &Shadow, current: &Current) -> Result<Saved, Refusal> {
    Ok(match current {
        Current::Regular { bytes, mode } => Saved::Regular {
            blob: shadow.store_blob(bytes).map_err(store_failed)?,
            mode: *mode,
        },
        Current::Symlink { target } => Saved::Symlink {
            blob: shadow.store_blob(target).map_err(store_failed)?,
        },
        Current::Absent => Saved::Absent,
        Current::Other => return Err(Refusal::NotRegular),
    })
}

/// `content` stored as a [`Saved`].
fn save_content(shadow: &Shadow, content: &Content) -> Result<Saved, Refusal> {
    Ok(match content {
        Content::Bytes { bytes, mode } => Saved::Regular {
            blob: shadow.store_blob(bytes).map_err(store_failed)?,
            mode: *mode,
        },
        Content::Symlink { target } => Saved::Symlink {
            blob: shadow.store_blob(target).map_err(store_failed)?,
        },
        Content::Absent => Saved::Absent,
    })
}

fn regular_mode(current: &Current) -> u32 {
    match current {
        Current::Regular { mode, .. } => *mode,
        _ => 0,
    }
}

/// The journal note of a write from `from` (what the path holds now, read as `current`) to `to`.
/// Only a regular file is ever rewritten in place, so only its blobs matter to the journal.
fn journal_note(session: &str, rel: &Path, current: &Current, from: &Saved, to: &Saved) -> JournalNote {
    let regular = |s: &Saved| match s {
        Saved::Regular { blob, .. } => Some(blob.clone()),
        _ => None,
    };
    JournalNote {
        session: session.to_owned(),
        path: rel.to_path_buf(),
        pre: regular(from),
        pre_mode: regular_mode(current),
        intended: regular(to),
    }
}

/// A revert anchor's stamp: the time, and a counter so two reverts in one millisecond differ.
fn stamp(at_ms: u64) -> String {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    format!("{at_ms}-{}", SEQ.fetch_add(1, Ordering::Relaxed))
}

fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<Vec<u8>> {
        text.iter().map(|l| l.as_bytes().to_vec()).collect()
    }

    #[test]
    fn range_index_follows_git_zero_length_rule() {
        // `-5,0 +6,2`: nothing removed after base line 5; two lines from end line 6.
        assert_eq!(range_index(5, 0), 5);
        assert_eq!(range_index(6, 2), 5);
        // `-0,0 +1,1`: an empty base, a one-line end.
        assert_eq!(range_index(0, 0), 0);
        assert_eq!(range_index(1, 1), 0);
        // `-3,2 +2,0`: base lines 3 and 4 removed; nothing left after end line 2.
        assert_eq!(range_index(3, 2), 2);
        assert_eq!(range_index(2, 0), 2);
        // `-4,3 +4,3`.
        assert_eq!(range_index(4, 3), 3);
        assert_eq!(
            region(&lines(&["a\n"]), (0, 1)),
            None,
            "a non-empty range starts at line 1"
        );
        assert_eq!(region(&lines(&["a\n", "b\n"]), (2, 2)), None, "past the end");
    }

    #[test]
    fn splice_keeps_bytes_outside_the_region() {
        let base = lines(&["1\n", "2\n", "3\n", "4\n", "5\n"]);
        // An insertion after base line 5, at the end of the file, with no context.
        let end = lines(&["1\n", "2\n", "3\n", "4\n", "5\n", "x\n", "y\n"]);
        let spliced = splice_hunk(&base, &end, &end, (5, 0), (6, 2)).unwrap();
        assert_eq!(spliced.bytes, base.concat());
        assert_eq!(spliced.at_line, 6);
        assert!(spliced.reverted_to.is_empty());
        assert_eq!(spliced.replaced, b"x\ny\n");

        // An insertion at line 0.
        let end = lines(&["x\n", "1\n", "2\n", "3\n", "4\n", "5\n"]);
        let spliced = splice_hunk(&base, &end, &end, (0, 0), (1, 1)).unwrap();
        assert_eq!(spliced.bytes, base.concat());
        assert_eq!(spliced.at_line, 1);

        // A deletion of base lines 3-4 checks the whole file, then splices after line 2.
        let end = lines(&["1\n", "2\n", "5\n"]);
        let spliced = splice_hunk(&base, &end, &end, (3, 2), (2, 0)).unwrap();
        assert_eq!(spliced.bytes, base.concat());
        assert_eq!(spliced.at_line, 3);
        let mut edited = end.clone();
        edited[2] = b"5 edited\n".to_vec();
        assert_eq!(
            splice_hunk(&base, &end, &edited, (3, 2), (2, 0)),
            Err(Refusal::ChangedSinceEnd)
        );

        // A deletion at the end of the file.
        let end = lines(&["1\n", "2\n", "3\n"]);
        let spliced = splice_hunk(&base, &end, &end, (4, 2), (3, 0)).unwrap();
        assert_eq!(spliced.bytes, base.concat());

        // A change with the user's own edits before and after the region: both stay.
        let end = lines(&["1\n", "2\n", "THREE\r\n", "4\n", "5\n"]);
        let mut cur = lines(&["1 mine\n", "2\n", "THREE\r\n", "4\n"]);
        cur.push(b"5 mine\xff\n".to_vec());
        let spliced = splice_hunk(&base, &end, &cur, (2, 3), (2, 3)).unwrap();
        assert_eq!(spliced.bytes, b"1 mine\n2\n3\n4\n5 mine\xff\n");
        assert_eq!(spliced.reverted_to, b"2\n3\n4\n");
        assert_eq!(spliced.replaced, b"2\nTHREE\r\n4\n");
        assert_eq!(spliced.at_line, 2);

        // The same region one line further down is not found by searching.
        let moved = lines(&["0\n", "1\n", "2\n", "THREE\r\n", "4\n", "5\n"]);
        assert_eq!(
            splice_hunk(&base, &end, &moved, (2, 3), (2, 3)),
            Err(Refusal::ChangedSinceEnd)
        );
        // A header past either side's end is out of date.
        assert_eq!(splice_hunk(&base, &end, &cur, (4, 3), (2, 3)), Err(Refusal::OutOfDate));
    }

    #[test]
    fn a_base_line_without_a_newline_is_never_followed_by_more() {
        let base = lines(&["a\n", "b"]);
        let end = lines(&["a\n", "b\n"]);
        let cur = lines(&["a\n", "b\n", "added later\n"]);
        assert_eq!(
            splice_hunk(&base, &end, &cur, (1, 2), (1, 2)),
            Err(Refusal::ChangedSinceEnd)
        );
        assert_eq!(splice_hunk(&base, &end, &end, (1, 2), (1, 2)).unwrap().bytes, b"a\nb");
    }

    #[test]
    fn replace_mode_keeps_the_files_own_bits() {
        let entry = |mode| Entry {
            mode,
            kind: EntryKind::Blob,
            bytes: Vec::new(),
        };
        let regular = |mode| Current::Regular {
            bytes: Vec::new(),
            mode,
        };
        assert_eq!(replace_mode(&entry(0o644), &entry(0o644), &regular(0o600)), 0o600);
        assert_eq!(replace_mode(&entry(0o644), &entry(0o755), &regular(0o755)), 0o644);
        assert_eq!(replace_mode(&entry(0o644), &entry(0o755), &regular(0o700)), 0o600);
        assert_eq!(replace_mode(&entry(0o755), &entry(0o644), &regular(0o644)), 0o755);
        assert_eq!(replace_mode(&entry(0o755), &entry(0o644), &regular(0o600)), 0o700);
        let link = Entry {
            mode: 0o777,
            kind: EntryKind::Symlink,
            bytes: b"t".to_vec(),
        };
        assert_eq!(
            replace_mode(&entry(0o755), &link, &Current::Symlink { target: b"t".to_vec() }),
            0o755
        );
    }

    #[test]
    fn buffer_state_reads_str_or_bin_lines_and_range_bytes() {
        let answer = Value::Map(vec![
            (Value::from("found"), Value::from(true)),
            (Value::from("modified"), Value::from(false)),
            (Value::from("fileformat"), Value::from("dos")),
            (Value::from("eol"), Value::from(false)),
            (Value::from("line_count"), Value::from(3)),
            (
                Value::from("lines"),
                Value::Array(vec![Value::from("b"), Value::Binary(b"c\xff".to_vec())]),
            ),
        ]);
        let state = parse_buffer_state(&answer).unwrap();
        assert_eq!(state.lines, Some(vec![b"b".to_vec(), b"c\xff".to_vec()]));
        assert_eq!(
            state.range_bytes(2),
            Some(b"b\r\nc\xff".to_vec()),
            "noeol at the buffer's end"
        );
        assert_eq!(state.range_bytes(1), Some(b"b\r\nc\xff\r\n".to_vec()), "not the end");
        let none = parse_buffer_state(&Value::Map(vec![
            (Value::from("found"), Value::from(false)),
            (Value::from("modified"), Value::from(false)),
        ]))
        .unwrap();
        assert!(!none.found && !none.modified && none.lines.is_none());
        assert_eq!(none.range_bytes(1), None);
        assert!(parse_buffer_state(&Value::from(1)).is_err());
    }

    #[test]
    fn the_path_is_an_argument() {
        use std::os::unix::ffi::OsStrExt;
        let args = buffer_state_args(Path::new("/p/it's 100%.txt"), Some((3, 2)));
        assert_eq!(args[0], Value::from("/p/it's 100%.txt"));
        assert_eq!(args[1], Value::Array(vec![Value::from(3), Value::from(2)]));
        let raw = Path::new(std::ffi::OsStr::from_bytes(b"/p/\xff"));
        assert_eq!(
            buffer_state_args(raw, None),
            vec![Value::Binary(b"/p/\xff".to_vec()), Value::Nil]
        );
    }
}
