//! Turn review: what changed on disk during each agent turn, measured against Eitri's own
//! snapshots of the project rather than the project's git history.
//!
//! The snapshots live in a shadow git repository per project ([`shadow`]), outside the project and
//! never touching its own `.git`. Every git process is started by [`git`], which isolates it from
//! the user's git configuration and environment so that no filter, hook or conversion runs over
//! the user's files.
//!
//! [`TurnReview`] ([`lifecycle`]) follows the turns of every tab and queues their snapshots on a
//! worker thread per session; [`attribution`] says which changed files a turn's own edit calls
//! named.
//!
//! Writing a file back (a revert, its undo, a recovery) goes only through [`write`], which reaches
//! every file from the project root without following a link and journals ([`journal`]) any
//! rewrite that cannot be atomic.
//!
//! [`draft`] holds the comments and reverts a user collects while reading a turn, and [`message`]
//! turns them into the one message the user confirms.
//!
//! [`flow`] is the window's coordinator of the panel's review requests: it checks, asks the editor
//! and runs the writes on workers without ever waiting.
//!
//! [`revert`] decides what a revert, its undo or a recovery writes, and refuses unless no turn
//! runs anywhere, the editor holds no unsaved buffer of the file and the disk is still what the
//! turn left.

pub mod attribution;
pub mod diff;
pub mod draft;
pub mod flow;
pub mod git;
pub mod journal;
pub mod lifecycle;
pub mod message;
pub mod presence;
pub mod revert;
pub mod shadow;
pub mod write;

pub use attribution::{named_paths, origin, NamedPaths, Origin};

pub use diff::{
    changed_files, file_hunks, parse_unified, ChangeKind, DiffLine, FileChange, FileDiff, Hunk, LineKind, Side,
};
pub use draft::{
    check_reverts, Comment, NewRevert, RevertRecord, RevertShape, RevertSource, RevertStatus, ReviewDraft, UndoData,
    UndoState, MAX_COMMENT_CHARS,
};
pub use flow::{saved_to_undo, show_hunk, undo_to_saved, Out, ReviewFlow};
pub use journal::{Journal, JournalEntry, JournalNote};
pub use lifecycle::{
    DiffJob, JobEvent, JobHook, Overview, OverviewFile, OverviewJob, ReviewDiff, ReviewError, ReviewHint,
    ReviewOptions, Scope, Snap, TurnRecord, TurnRef, TurnReview, TurnState, MAX_OVERVIEW_DIFF_LINES,
};
pub use message::{compose, Preview};
pub use presence::{busy_elsewhere, Blocked, Busy, Presence, PresenceGuard, PresenceHolder};
pub use revert::{
    buffer_state_args, parse_buffer_state, AnchorJob, AppliedRevert, BufferState, EditorCheck, EditorClear, ExactHunk,
    ExactHunks, ExactHunksJob, RecoverAnswer, RecoverJob, RecoveriesJob, Refusal, RevertJob, RevertKind, RevertTarget,
    Saved, UndoJob, BUFFER_STATE_LUA,
};
pub use shadow::{
    review_dir_for, Entry, EntryKind, Limits, Shadow, ShadowError, ShadowLock, Snapshot, SnapshotKind, SnapshotLabel,
    SnapshotOutcome, SnapshotRef, TurnMarks,
};
pub use write::{
    decide_mode, read_current, replace, write_mode, Content, Current, FsHooks, InPlaceWhy, ProjectDir, Stage, Target,
    WriteError, WriteMode,
};
