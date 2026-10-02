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

pub mod attribution;
pub mod diff;
pub mod git;
pub mod lifecycle;
pub mod shadow;

pub use attribution::{named_paths, origin, NamedPaths, Origin};

pub use diff::{
    changed_files, file_hunks, parse_unified, ChangeKind, DiffLine, FileChange, FileDiff, Hunk, LineKind, Side,
};
pub use lifecycle::{
    DiffJob, JobEvent, JobHook, Overview, OverviewFile, OverviewJob, ReviewDiff, ReviewError, ReviewHint,
    ReviewOptions, Scope, Snap, TurnRecord, TurnRef, TurnReview, TurnState, MAX_OVERVIEW_DIFF_LINES,
};
pub use shadow::{
    review_dir_for, Limits, Shadow, ShadowError, ShadowLock, Snapshot, SnapshotKind, SnapshotLabel, SnapshotOutcome,
    SnapshotRef, TurnMarks,
};
