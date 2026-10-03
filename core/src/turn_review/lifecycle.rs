//! The turn lifecycle: which turns ran, when their snapshots were taken, and what each one's review
//! may honestly claim.
//!
//! [`TurnReview`] watches the events the tab set's pump delivers and turns them into snapshot jobs.
//! It never waits for one: every git run happens on a worker thread per provider session, fed
//! through a channel whose `send` cannot block, and the results come back on a second channel that
//! [`TurnReview::poll`] drains without waiting. Nothing a permission answer or a send depends on is
//! read from here, so a slow disk can make a review less precise but never makes a tool wait.
//!
//! Since nothing waits, a tool can run before its turn's base snapshot exists. That is not
//! prevented, only recorded: a turn whose tool call reached the pump while its base was still
//! pending is marked late, and its review says changes made before the base may be missing. The
//! same goes for the end of one turn still running when the next turn's tools start.
//!
//! The review of a turn ([`OverviewJob`], [`DiffJob`]) is a self-contained job too: it captures
//! what it needs here and runs git wherever the caller runs it, never on the thread that built it.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use agent::AgentDomainEvent;

use super::attribution::{origin, NamedPaths, Origin};
use super::diff::{changed_files, file_hunks, Hunk, Side};
use super::presence::PresenceGuard;
use super::revert::{
    AnchorJob, EditorClear, ExactHunksJob, RecoverAnswer, RecoverJob, RecoveriesJob, RevertJob, RevertTarget, Saved,
    UndoJob,
};
use super::shadow::{
    review_dir_for, FilePrint, LeftOut, Limits, Shadow, SnapshotKind, SnapshotLabel, SnapshotOutcome, SnapshotRef,
    TurnMarks,
};

/// A file patch longer than this many lines (context included) is refused, with its counts, rather
/// than shown cut short.
pub const MAX_OVERVIEW_DIFF_LINES: usize = 2000;

/// What every turn's review is headed with: the files listed changed on disk, whoever changed them.
pub const NOTE_TURN: &str = "changed on disk during this turn";
/// The session scope's heading.
pub const NOTE_SESSION: &str = "changed on disk during this session";
/// A turn whose end snapshot was still running when the next turn's tools started.
pub const NOTE_OVERLAPPED_NEXT: &str = "may include the next turn's first changes";
/// A turn that ran while another tab's turn on the same project was running.
pub const NOTE_OVERLAPPED_TAB: &str = "this turn overlapped another tab's turn";
/// A turn that has a base and no end, from an earlier run of Eitri.
pub const NOTE_UNFINISHED: &str = "this turn did not finish while Eitri was running";
/// A turn still running: its base is compared with the files as they are now.
pub const NOTE_RUNNING: &str = "this turn is still running: compared with the files on disk now";
/// A turn whose base snapshot has not finished yet.
pub const NOTE_BASE_PENDING: &str = "the baseline snapshot is still being taken";
/// A turn that ended and whose end snapshot has not finished yet.
pub const NOTE_END_PENDING: &str = "the end snapshot is still being taken: compared with the files on disk now";

/// One snapshot of a turn, as far as this side knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Snap {
    /// Asked for (or about to be) and not finished.
    Pending,
    Taken {
        commit: String,
    },
    /// There is no such snapshot; the reason is for the user.
    Unavailable(String),
}

impl Snap {
    pub fn commit(&self) -> Option<&str> {
        match self {
            Snap::Taken { commit } => Some(commit),
            _ => None,
        }
    }
}

/// What a turn's review may claim, derived from its two snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnState {
    /// Both snapshots exist: the review is the change between them.
    Ok,
    /// The base does not exist, so no change can be shown as the turn's.
    NoBaseline(String),
    /// The base exists and the end never will: the base is compared with the files on disk now.
    Unfinished(String),
    /// A snapshot is still to come: the turn is running, or one of its snapshots is queued.
    Pending,
}

impl TurnState {
    /// The wire's name for the state.
    pub fn as_str(&self) -> &'static str {
        match self {
            TurnState::Ok => "ok",
            TurnState::NoBaseline(_) => "no_baseline",
            TurnState::Unfinished(_) => "unfinished",
            TurnState::Pending => "pending",
        }
    }

    /// Why, for the two states that have a reason.
    pub fn reason(&self) -> Option<&str> {
        match self {
            TurnState::NoBaseline(reason) | TurnState::Unfinished(reason) => Some(reason),
            TurnState::Ok | TurnState::Pending => None,
        }
    }
}

/// One turn of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRecord {
    /// The turn's number within its session, the same as its snapshot refs' `<n>`: it continues
    /// after the turns of earlier runs rather than starting again at 1.
    pub n: u32,
    pub turn_id: String,
    /// The tab the turn ran in (`TabId`'s number).
    pub tab: u64,
    /// The provider session; empty until the session reported its id.
    pub session: String,
    pub started_ms: u64,
    pub ended_ms: Option<u64>,
    pub base: Snap,
    /// When the base snapshot finished.
    pub base_taken_ms: Option<u64>,
    pub end: Snap,
    /// A tool call of this turn reached the pump while its base was still pending: the base may
    /// already hold some of the turn's own changes.
    pub late: bool,
    /// The next turn's tools started while this turn's end was still pending: the end may hold
    /// some of the next turn's changes.
    pub overlapped_next: bool,
    /// Another tab's turn on the same project ran at the same time.
    pub overlapped_tab: bool,
    /// Files left out of a snapshot for their size, relative to the project root.
    pub skipped_large: Vec<PathBuf>,
    /// The same files by the snapshot that left them out, each with its print: what tells a file
    /// too large for both snapshots that did not change from one that did.
    pub left_out: LeftOutSides,
    /// Known only from the shadow's refs, not from this run's events.
    pub earlier_run: bool,
}

/// The files a turn's base snapshot and its end snapshot each left out for their size.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LeftOutSides {
    pub base: LeftOut,
    pub end: LeftOut,
}

impl TurnRecord {
    /// Derived from the two snapshots every time, never stored, so nothing can claim `ok` for a
    /// turn that lacks one.
    pub fn state(&self) -> TurnState {
        match (&self.base, &self.end) {
            (Snap::Unavailable(reason), _) => TurnState::NoBaseline(reason.clone()),
            (Snap::Pending, _) => TurnState::Pending,
            (Snap::Taken { .. }, Snap::Taken { .. }) => TurnState::Ok,
            (Snap::Taken { .. }, Snap::Pending) => TurnState::Pending,
            (Snap::Taken { .. }, Snap::Unavailable(reason)) => TurnState::Unfinished(reason.clone()),
        }
    }

    /// The marks kept beside the turn's snapshots for a later run.
    fn kept_marks(&self) -> TurnMarks {
        TurnMarks {
            late: self.late,
            overlapped_next: self.overlapped_next,
            overlapped_tab: self.overlapped_tab,
        }
    }
}

/// What the status band shows after a turn: how many files differ between its two snapshots.
/// `files: 0` clears an earlier hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewHint {
    pub tab: u64,
    pub turn: u32,
    pub files: usize,
}

/// Which turn a review is of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRef {
    Latest,
    N(u32),
}

/// One turn, or the whole session from its first base on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Turn,
    Session,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Turn => "turn",
            Scope::Session => "session",
        }
    }
}

/// One row of a review's file list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverviewFile {
    /// Relative to the project root.
    pub path: PathBuf,
    pub added: u32,
    pub removed: u32,
    pub origin: Origin,
    pub binary: bool,
    /// Left out of a snapshot for its size: nothing about its change is known.
    pub too_large: bool,
    pub nested: bool,
}

/// A review: the session's turns, the one shown, and the files that changed on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overview {
    pub session: String,
    pub scope: Scope,
    /// The turn shown (for the session scope, the latest).
    pub current: u32,
    /// Every turn of the session this run knows and the shadow keeps, in order.
    pub turns: Vec<TurnRecord>,
    pub files: Vec<OverviewFile>,
    /// Whether two snapshots (or a snapshot and the disk) were compared at all. `false` when the
    /// turn has no baseline or its baseline is still being taken: `files` is then empty because
    /// nothing was compared, not because nothing changed.
    pub compared: bool,
    /// Edit calls with no result yet.
    pub pending_no_result: usize,
    /// What the review must say about itself, in order.
    pub notes: Vec<String>,
}

/// One file's patch within a review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewDiff {
    pub turn: u32,
    pub path: PathBuf,
    pub added: u32,
    pub removed: u32,
    pub binary: bool,
    pub new_file: bool,
    pub deleted_file: bool,
    pub mode_change: Option<(String, String)>,
    /// `None` when the patch is longer than [`MAX_OVERVIEW_DIFF_LINES`]: refused, not cut.
    pub hunks: Option<Vec<Hunk>>,
}

/// Why a review could not be given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewError {
    /// The session has no turn this run or the shadow knows of.
    NoTurns,
    NoSuchTurn(String),
    /// The turn has no base: no change can be shown as its.
    NoBaseline(String),
    /// A snapshot the review needs is still being taken.
    NotReady(String),
    /// The file was left out of a snapshot for its size.
    TooLarge(PathBuf),
    /// The path asked for is not one file's name: it carries a replacement character, and either the
    /// turn changed a file whose real name is not valid UTF-8 (the list shows both as the same
    /// text) or no changed file has that name. Showing a patch would risk showing another file's.
    AmbiguousPath(PathBuf),
    /// The review store or git could not be used.
    Unavailable(String),
}

impl fmt::Display for ReviewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReviewError::NoTurns => write!(f, "no turn of this session has been recorded"),
            ReviewError::NoSuchTurn(reason) => write!(f, "{reason}"),
            ReviewError::NoBaseline(reason) => write!(f, "no baseline: {reason}"),
            ReviewError::NotReady(reason) => write!(f, "{reason}"),
            ReviewError::TooLarge(path) => write!(f, "{} is too large to snapshot", path.display()),
            ReviewError::AmbiguousPath(path) => write!(
                f,
                "{} cannot be told apart from another file's name, so its patch is not shown",
                path.display()
            ),
            ReviewError::Unavailable(reason) => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for ReviewError {}

/// A worker's progress through one job, for a test to watch or hold back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobEvent {
    Starting {
        session: String,
        kind: SnapshotKind,
        /// The turn's position among this run's turns of the session, from 1; 0 for the warm-up.
        position: u32,
    },
    Finished {
        session: String,
        kind: SnapshotKind,
        position: u32,
    },
}

/// Called on the worker thread around every job.
pub type JobHook = Arc<dyn Fn(&JobEvent) + Send + Sync>;

/// How a [`TurnReview`] works. [`Default`] is what a window uses.
#[derive(Clone)]
pub struct ReviewOptions {
    pub limits: Limits,
    /// The global excludes file: `None` reads the user's own (as `git` would), `Some(x)` uses `x`.
    pub excludes: Option<Option<PathBuf>>,
    pub on_job: Option<JobHook>,
    /// How often retention may run, at most.
    pub trim_every: Duration,
    /// How many turns of the project retention keeps.
    pub keep: usize,
    /// How old a turn retention keeps, at most.
    pub max_age: Duration,
}

impl Default for ReviewOptions {
    fn default() -> Self {
        ReviewOptions {
            limits: Limits::default(),
            excludes: None,
            on_job: None,
            trim_every: Duration::from_secs(60 * 60),
            keep: 100,
            max_age: Duration::from_secs(30 * 24 * 60 * 60),
        }
    }
}

impl fmt::Debug for ReviewOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReviewOptions")
            .field("limits", &self.limits)
            .field("excludes", &self.excludes)
            .field("on_job", &self.on_job.is_some())
            .field("trim_every", &self.trim_every)
            .field("keep", &self.keep)
            .field("max_age", &self.max_age)
            .finish()
    }
}

/// Where the shadow is and how to open it: everything a worker or a job needs, and cheap to clone.
#[derive(Debug, Clone)]
pub(crate) struct ShadowSpec {
    pub(crate) review_dir: Option<PathBuf>,
    /// The canonical project root.
    pub(crate) root: PathBuf,
    pub(crate) excludes: Option<Option<PathBuf>>,
}

impl ShadowSpec {
    /// Opens (creating on first use) the store. Runs git: only ever called off the GTK thread.
    pub(crate) fn open(&self) -> Result<Shadow, String> {
        let dir = self
            .review_dir
            .as_deref()
            .ok_or_else(|| "no state directory to keep snapshots in".to_string())?;
        let opened = match &self.excludes {
            None => Shadow::open(dir, &self.root),
            Some(excludes) => Shadow::open_with_excludes(dir, &self.root, excludes.clone()),
        };
        opened.map_err(|e| e.to_string())
    }
}

/// One snapshot for a worker to take.
struct Job {
    kind: SnapshotKind,
    position: u32,
    turn_id: String,
    tab: u64,
    time_ms: u64,
}

/// What a worker is handed: a snapshot to take, or a turn's marks to record.
enum Task {
    Snapshot(Job),
    Marks {
        position: u32,
        turn_id: String,
        marks: TurnMarks,
    },
}

/// What a worker reports back.
enum Done {
    /// The number the session's turns of earlier runs reached, read before the first snapshot
    /// this worker writes; `Err` while the shadow cannot be read.
    Numbering(Result<u32, String>),
    Snapshot {
        kind: SnapshotKind,
        position: u32,
        snap: Snap,
        left_out: LeftOut,
        at_ms: u64,
        /// For an end whose turn has both snapshots: how many files differ between them.
        files: Option<usize>,
    },
    /// A marks task is done, written or not.
    Marked,
}

/// One turn as this side tracks it.
struct Rec {
    /// Position among this run's turns of the session, from 1; 0 while the session is not known.
    position: u32,
    record: TurnRecord,
    /// An event ended the turn.
    ended: bool,
    /// The pump the end was observed in, valid once `ended`.
    ended_pump: u64,
    /// The marks last handed to the worker for this turn.
    marks_sent: TurnMarks,
}

/// One provider session: its worker and its turns.
struct SessionTrack {
    jobs: mpsc::Sender<Task>,
    /// Added to a turn's position to give its number; `None` until the worker has read it.
    offset: Option<u32>,
    numbering_failed: bool,
    records: Vec<Rec>,
    next_position: u32,
    /// Jobs sent and not yet reported back.
    outstanding: usize,
    /// The open tabs bound to this session.
    tabs: BTreeSet<u64>,
    warmed: bool,
}

impl SessionTrack {
    fn record_mut(&mut self, position: u32) -> Option<&mut Rec> {
        self.records.iter_mut().find(|r| r.position == position)
    }
}

/// The running turn of a tab.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Active {
    Bound {
        session: String,
        position: u32,
    },
    /// Held in the tab until its session reports its id (an index into `TabTrack::unbound`).
    Unbound(usize),
}

#[derive(Default)]
struct TabTrack {
    /// The provider session whose turns this tab keeps, and reviews under, while its backend lives.
    session: Option<String>,
    /// Whether the tab's next turn belongs to `session`. False once that session ended: whatever
    /// the tab starts next is a new session, whose turns wait for its own id rather than being
    /// filed under the one that ended.
    live: bool,
    active: Option<Active>,
    /// The tab's most recent turn, running or ended.
    latest: Option<Active>,
    /// Turns started before the tab's session had an id.
    unbound: Vec<Rec>,
}

impl TabTrack {
    fn live_session(&self) -> Option<&str> {
        self.session.as_deref().filter(|_| self.live)
    }

    fn drop_unbound(&mut self) {
        self.unbound.clear();
        if matches!(self.latest, Some(Active::Unbound(_))) {
            self.latest = None;
        }
    }
}

/// One project's turn review: the per-session workers and every turn this run has seen.
///
/// Building one touches no disk and runs nothing; only the workers and the jobs open the shadow.
pub struct TurnReview {
    spec: ShadowSpec,
    options: ReviewOptions,
    /// When retention last ran, shared by every worker so it runs at most once per period for the
    /// whole project.
    trim_stamp: Arc<AtomicU64>,
    done_tx: mpsc::Sender<(String, Done)>,
    done_rx: mpsc::Receiver<(String, Done)>,
    sessions: BTreeMap<String, SessionTrack>,
    tabs: BTreeMap<u64, TabTrack>,
    /// Counts [`poll`](Self::poll)s, which open every pump: events of different tabs are drained one
    /// tab after another, so their order within one pump, or across two neighbouring ones, is not
    /// known.
    pump: u64,
}

impl TurnReview {
    /// The review of the project at `project_root` (canonical), its store under
    /// `<state home>/eitri/review/<key>` as the two variables decide it.
    pub fn new(state_home: Option<&OsStr>, home: Option<&OsStr>, project_root: &Path) -> TurnReview {
        TurnReview::with_options(
            review_dir_for(state_home, home, project_root),
            project_root,
            ReviewOptions::default(),
        )
    }

    /// The review with its store in `review_dir` (`None`: there is nowhere to keep snapshots, and
    /// every turn says so) and the given options.
    pub fn with_options(review_dir: Option<PathBuf>, project_root: &Path, options: ReviewOptions) -> TurnReview {
        let (done_tx, done_rx) = mpsc::channel();
        TurnReview {
            spec: ShadowSpec {
                review_dir,
                root: project_root.to_path_buf(),
                excludes: options.excludes.clone(),
            },
            options,
            trim_stamp: Arc::new(AtomicU64::new(0)),
            done_tx,
            done_rx,
            sessions: BTreeMap::new(),
            tabs: BTreeMap::new(),
            pump: 0,
        }
    }

    /// The canonical project root this review was built for: the base every path handed to the
    /// editor must be joined to, because a write refuses an editor check made for any other name.
    pub fn project_root(&self) -> &Path {
        &self.spec.root
    }

    /// Takes in one delivered event of `tab`. `session` is the tab's provider session as its
    /// backend knows it now, empty when it has none yet. Only enqueues work; never waits.
    pub fn observe(&mut self, tab: u64, session: &str, event: &AgentDomainEvent, now_ms: u64) {
        match event {
            AgentDomainEvent::SessionOpened {
                provider_session_id, ..
            } => self.bind(tab, provider_session_id, now_ms, true),
            AgentDomainEvent::TurnStarted { turn_id } => {
                self.follow_backend(tab, session, now_ms);
                // Already recorded: a resync read this turn off the projection while its start was
                // still queued for the next pump. The same turn is not started twice.
                if self.is_latest_turn(tab, turn_id) {
                    return;
                }
                self.start_turn(tab, turn_id, now_ms, false);
            }
            AgentDomainEvent::ToolCallStarted { turn_id, .. } => self.tool_started(tab, turn_id),
            AgentDomainEvent::TurnCompleted { .. } => self.end_active(tab, now_ms, false),
            AgentDomainEvent::SessionClosed { .. } | AgentDomainEvent::SessionUnavailable { .. } => {
                self.session_ended(tab, now_ms)
            }
            AgentDomainEvent::ResumeOutcome {
                requested_provider_session_id,
                status,
                attached_provider_session_id,
                forked,
                ..
            } if !status.attached_to_the_requested_session(
                requested_provider_session_id,
                attached_provider_session_id.as_deref(),
                *forked,
            ) =>
            {
                self.session_ended(tab, now_ms)
            }
            // Nothing that starts, ends or marks a turn.
            _ => return,
        }
        self.flush_marks();
    }

    /// The tab has no live session (never started, failed, starting again): a turn it was running
    /// is over, and the tab lets go of its session, so the next session it starts is filed under
    /// its own id.
    pub fn tab_has_no_session(&mut self, tab: u64, now_ms: u64) {
        self.release(tab, now_ms, false);
        self.flush_marks();
    }

    /// The tab's events were dropped and its state rebuilt from the projection, whose running turn
    /// is `active_turn_id`. The dropped events may have ended the turn this side thinks is running,
    /// started another, or held a tool call; each is assumed rather than missed.
    pub fn observe_resync(&mut self, tab: u64, session: &str, active_turn_id: Option<&str>, now_ms: u64) {
        self.follow_backend(tab, session, now_ms);
        let current = self.active_turn_id(tab);
        match (current.as_deref(), active_turn_id) {
            (Some(ours), Some(theirs)) if ours == theirs => {
                // A tool call may have been among what was dropped.
                if let Some(rec) = self.active_rec_mut(tab) {
                    if rec.record.base == Snap::Pending {
                        rec.record.late = true;
                    }
                }
            }
            (Some(_), theirs) => {
                self.end_active(tab, now_ms, theirs.is_some());
                if let Some(theirs) = theirs {
                    self.start_turn(tab, theirs, now_ms, true);
                }
            }
            (None, Some(theirs)) => {
                if !self.is_latest_turn(tab, theirs) {
                    self.start_turn(tab, theirs, now_ms, true);
                }
            }
            (None, None) => {}
        }
        self.flush_marks();
    }

    /// The tab is gone: a turn it was running is over, and its session's worker ends once its
    /// queue is done, unless another tab holds the session.
    pub fn close_tab(&mut self, tab: u64, now_ms: u64) {
        self.release(tab, now_ms, false);
        self.tabs.remove(&tab);
        self.flush_marks();
    }

    /// Takes in what the workers finished, without waiting, and returns a hint for every turn whose
    /// end landed with both snapshots.
    pub fn poll(&mut self) -> Vec<ReviewHint> {
        self.pump += 1;
        let mut hints = Vec::new();
        while let Ok((session, done)) = self.done_rx.try_recv() {
            let Some(track) = self.sessions.get_mut(&session) else {
                continue;
            };
            match done {
                Done::Numbering(Ok(offset)) => {
                    track.offset = Some(offset);
                    track.numbering_failed = false;
                }
                Done::Numbering(Err(_)) => track.numbering_failed = true,
                Done::Marked => track.outstanding = track.outstanding.saturating_sub(1),
                Done::Snapshot {
                    kind,
                    position,
                    snap,
                    left_out,
                    at_ms,
                    files,
                } => {
                    track.outstanding = track.outstanding.saturating_sub(1);
                    let offset = track.offset;
                    let Some(rec) = track.record_mut(position) else {
                        continue;
                    };
                    let record = &mut rec.record;
                    for path in left_out.keys() {
                        if !record.skipped_large.contains(path) {
                            record.skipped_large.push(path.clone());
                        }
                    }
                    match kind {
                        SnapshotKind::Warm => {}
                        SnapshotKind::Base => {
                            record.left_out.base = left_out;
                            if matches!(snap, Snap::Taken { .. }) {
                                record.base_taken_ms = Some(at_ms);
                            }
                            record.base = snap;
                        }
                        SnapshotKind::End => {
                            record.left_out.end = left_out;
                            record.end = snap;
                            let both = record.state() == TurnState::Ok;
                            if let (true, Some(files), Some(offset)) = (both, files, offset) {
                                if self.tabs.contains_key(&record.tab) {
                                    hints.push(ReviewHint {
                                        tab: record.tab,
                                        turn: offset + position,
                                        files,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
        // A session no open tab holds is dropped once its worker reported everything; dropping
        // its sender is what lets the worker end.
        self.sessions.retain(|_, s| !s.tabs.is_empty() || s.outstanding > 0);
        hints
    }

    /// The turns of `session` this run has seen, in order. Reads memory only: it never runs git
    /// and never touches the shadow, so it is safe on the GTK thread. Turns of earlier runs come
    /// from [`overview_job`](Self::overview_job), which reads the shadow where it runs.
    pub fn turns(&self, session: &str) -> Vec<TurnRecord> {
        let Some(track) = self.sessions.get(session) else {
            return Vec::new();
        };
        // Until the worker has read how far earlier runs numbered (one tick, normally), a number
        // would be a guess. If it cannot read that at all, no snapshot was written either, and the
        // turns are listed by position.
        let offset = match track.offset {
            Some(offset) => offset,
            None if track.numbering_failed => 0,
            None => return Vec::new(),
        };
        track
            .records
            .iter()
            .map(|rec| {
                let mut record = rec.record.clone();
                record.n = offset + rec.position;
                record
            })
            .collect()
    }

    /// A review of `session` at `turn` in `scope`, attributing with `named` (the tab's own edit
    /// calls of the turns concerned, from [`named_paths`](super::attribution::named_paths)). Built
    /// here without git; [`OverviewJob::run`] does the work wherever it is run.
    pub fn overview_job(&self, session: &str, turn: TurnRef, scope: Scope, named: NamedPaths) -> OverviewJob {
        OverviewJob {
            spec: self.spec.clone(),
            session: session.to_owned(),
            records: self.turns(session),
            turn,
            scope,
            named,
        }
    }

    /// One file's patch in the review of `session` at `turn` in `scope`. Built here without git.
    pub fn diff_job(&self, session: &str, turn: u32, scope: Scope, path: &str) -> DiffJob {
        DiffJob {
            spec: self.spec.clone(),
            session: session.to_owned(),
            records: self.turns(session),
            turn,
            scope,
            path: PathBuf::from(path),
        }
    }

    /// One file's hunks in the review of `session` at `turn` in `scope`, with the lines of both
    /// sides as the snapshots hold them, terminators and all. Built here without git.
    pub fn exact_hunks_job(&self, session: &str, turn: u32, scope: Scope, path: &str) -> ExactHunksJob {
        ExactHunksJob {
            spec: self.spec.clone(),
            session: session.to_owned(),
            records: self.turns(session),
            turn,
            scope,
            path: PathBuf::from(path),
        }
    }

    /// Reverting one hunk or the whole of `path` to the base of `session`'s turn `turn` in
    /// `scope`. It can only be built with this window's presence guard and the editor's word that
    /// no buffer of the file has unsaved changes; `still_wanted` going false cancels it. Built
    /// here without git or disk.
    #[allow(clippy::too_many_arguments)]
    pub fn revert_job(
        &self,
        guard: PresenceGuard,
        still_wanted: Arc<AtomicBool>,
        session: &str,
        turn: u32,
        scope: Scope,
        path: &str,
        target: RevertTarget,
        clear: EditorClear,
    ) -> RevertJob {
        RevertJob {
            spec: self.spec.clone(),
            guard,
            wanted: still_wanted,
            session: session.to_owned(),
            records: self.turns(session),
            turn,
            scope,
            path: PathBuf::from(path),
            target,
            clear,
            final_hook: None,
        }
    }

    /// Putting `path` back to `pre`, what it was before a revert that wrote `post`. Built here
    /// without git or disk.
    #[allow(clippy::too_many_arguments)]
    pub fn undo_job(
        &self,
        guard: PresenceGuard,
        still_wanted: Arc<AtomicBool>,
        session: &str,
        path: &str,
        pre: Saved,
        post: Saved,
        clear: EditorClear,
    ) -> UndoJob {
        UndoJob {
            spec: self.spec.clone(),
            guard,
            wanted: still_wanted,
            session: session.to_owned(),
            path: PathBuf::from(path),
            pre,
            post,
            clear,
            final_hook: None,
        }
    }

    /// The end side's lines `from..=to` (from 1) of `path`, as the text a comment quotes. Built
    /// here without git.
    pub fn anchor_job(&self, session: &str, turn: u32, scope: Scope, path: &str, from: u32, to: u32) -> AnchorJob {
        AnchorJob {
            spec: self.spec.clone(),
            session: session.to_owned(),
            records: self.turns(session),
            turn,
            scope,
            path: PathBuf::from(path),
            from,
            to,
        }
    }

    /// The interrupted reverts the journal holds. Built here without disk.
    pub fn recoveries_job(&self) -> RecoveriesJob {
        RecoveriesJob {
            spec: self.spec.clone(),
        }
    }

    /// Restoring or forgetting the interrupted revert `entry` (its journal id). Built here without
    /// git or disk.
    pub fn recover_job(&self, guard: PresenceGuard, entry: &str, answer: RecoverAnswer) -> RecoverJob {
        RecoverJob {
            spec: self.spec.clone(),
            guard,
            entry: entry.to_owned(),
            answer,
            final_hook: None,
        }
    }

    // ---- event handling ------------------------------------------------------------------------

    /// Binds `tab` to `session`. A tab that changes session ends its running turn under the old
    /// one. Turns the tab started before it knew its session get their snapshots now; the
    /// session's warm-up is queued only when no turn is waiting for its base, which it would delay.
    fn bind(&mut self, tab: u64, session: &str, now_ms: u64, warm: bool) {
        if session.is_empty() {
            return;
        }
        if self.tabs.get(&tab).and_then(TabTrack::live_session) == Some(session) {
            return;
        }
        // The same session again (resumed into this tab) keeps its turns and numbering.
        let previous = self
            .tabs
            .get(&tab)
            .and_then(|t| t.session.clone())
            .filter(|previous| previous != session);
        if let Some(previous) = previous {
            if matches!(
                self.tabs.get(&tab).and_then(|t| t.active.as_ref()),
                Some(Active::Bound { .. })
            ) {
                self.end_active(tab, now_ms, false);
            }
            if let Some(s) = self.sessions.get_mut(&previous) {
                s.tabs.remove(&tab);
            }
        }
        self.ensure_session(session);
        let track = self.tabs.entry(tab).or_default();
        track.session = Some(session.to_owned());
        track.live = true;
        let held = std::mem::take(&mut track.unbound);
        let held_index = |slot: &Option<Active>| match slot {
            Some(Active::Unbound(index)) => Some(*index),
            _ => None,
        };
        let held_active = held_index(&track.active);
        let held_latest = held_index(&track.latest);
        let s = self.sessions.get_mut(session).expect("ensured above");
        s.tabs.insert(tab);
        let mut jobs = Vec::new();
        for (index, mut rec) in held.into_iter().enumerate() {
            rec.position = s.next_position;
            s.next_position += 1;
            rec.record.session = session.to_owned();
            jobs.push(job_of(SnapshotKind::Base, &rec, rec.record.started_ms));
            if rec.ended {
                jobs.push(job_of(SnapshotKind::End, &rec, rec.record.ended_ms.unwrap_or(now_ms)));
            }
            let bound = Active::Bound {
                session: session.to_owned(),
                position: rec.position,
            };
            let track = self.tabs.get_mut(&tab).expect("entered above");
            if held_active == Some(index) {
                track.active = Some(bound.clone());
            }
            if held_latest == Some(index) {
                track.latest = Some(bound);
            }
            s.records.push(rec);
        }
        if jobs.is_empty() && warm && !s.warmed {
            jobs.push(Job {
                kind: SnapshotKind::Warm,
                position: 0,
                turn_id: String::new(),
                tab,
                time_ms: now_ms,
            });
        }
        s.warmed = true;
        for job in jobs {
            self.send(session, job);
        }
    }

    fn start_turn(&mut self, tab: u64, turn_id: &str, now_ms: u64, late: bool) {
        // A turn still running here missed its end: it is over now, and its end snapshot is taken
        // after the new turn began.
        if self.tabs.get(&tab).is_some_and(|t| t.active.is_some()) {
            self.end_active(tab, now_ms, true);
        }
        // Another tab's turn overlaps this one if it is running, or if its end was observed in this
        // pump or the one before: the pump drains tab after tab, so an end observed there may have
        // happened after this start.
        let mut overlapped_tab = false;
        let recent = self.pump.saturating_sub(1);
        for other in self.tabs.keys().copied().collect::<Vec<_>>() {
            if other != tab {
                if let Some(rec) = self.latest_rec_mut(other) {
                    if !rec.ended || rec.ended_pump >= recent {
                        rec.record.overlapped_tab = true;
                        overlapped_tab = true;
                    }
                }
            }
        }
        let track = self.tabs.entry(tab).or_default();
        let mut rec = Rec {
            position: 0,
            record: TurnRecord {
                n: 0,
                turn_id: turn_id.to_owned(),
                tab,
                session: String::new(),
                started_ms: now_ms,
                ended_ms: None,
                base: Snap::Pending,
                base_taken_ms: None,
                end: Snap::Pending,
                late,
                overlapped_next: false,
                overlapped_tab,
                skipped_large: Vec::new(),
                left_out: LeftOutSides::default(),
                earlier_run: false,
            },
            ended: false,
            ended_pump: 0,
            marks_sent: TurnMarks::default(),
        };
        match track.live_session().map(str::to_owned) {
            None => {
                track.unbound.push(rec);
                track.active = Some(Active::Unbound(track.unbound.len() - 1));
                track.latest = track.active.clone();
            }
            Some(session) => {
                let s = self
                    .sessions
                    .get_mut(&session)
                    .expect("a bound tab's session is tracked");
                rec.position = s.next_position;
                s.next_position += 1;
                s.warmed = true;
                rec.record.session = session.clone();
                let job = job_of(SnapshotKind::Base, &rec, now_ms);
                let position = rec.position;
                s.records.push(rec);
                let track = self.tabs.get_mut(&tab).expect("entered above");
                track.active = Some(Active::Bound {
                    session: session.clone(),
                    position,
                });
                track.latest = track.active.clone();
                self.send(&session, job);
            }
        }
    }

    fn tool_started(&mut self, tab: u64, turn_id: &str) {
        let Some(active) = self.tabs.get(&tab).and_then(|t| t.active.clone()) else {
            return;
        };
        if self.active_turn_id(tab).as_deref() != Some(turn_id) {
            return;
        }
        match active {
            Active::Unbound(index) => {
                if let Some(rec) = self.tabs.get_mut(&tab).and_then(|t| t.unbound.get_mut(index)) {
                    rec.record.late = true;
                }
            }
            Active::Bound { session, position } => {
                let Some(s) = self.sessions.get_mut(&session) else {
                    return;
                };
                let previous_still_ending = position > 1
                    && s.record_mut(position - 1).is_some_and(|prev| {
                        if prev.record.end == Snap::Pending {
                            prev.record.overlapped_next = true;
                            true
                        } else {
                            false
                        }
                    });
                if let Some(rec) = s.record_mut(position) {
                    if rec.record.base == Snap::Pending || previous_still_ending {
                        rec.record.late = true;
                    }
                }
            }
        }
    }

    /// Ends the tab's running turn, if it has one, and queues its end snapshot.
    fn end_active(&mut self, tab: u64, now_ms: u64, overlapped_next: bool) {
        let pump = self.pump;
        let Some(track) = self.tabs.get_mut(&tab) else {
            return;
        };
        let Some(active) = track.active.take() else {
            return;
        };
        match active {
            Active::Unbound(index) => {
                if let Some(rec) = track.unbound.get_mut(index) {
                    rec.ended = true;
                    rec.ended_pump = pump;
                    rec.record.ended_ms = Some(now_ms);
                    rec.record.overlapped_next |= overlapped_next;
                }
            }
            Active::Bound { session, position } => {
                let Some(s) = self.sessions.get_mut(&session) else {
                    return;
                };
                let Some(rec) = s.record_mut(position) else {
                    return;
                };
                rec.ended = true;
                rec.ended_pump = pump;
                rec.record.ended_ms = Some(now_ms);
                rec.record.overlapped_next |= overlapped_next;
                let job = job_of(SnapshotKind::End, rec, now_ms);
                self.send(&session, job);
            }
        }
    }

    fn active_turn_id(&self, tab: u64) -> Option<String> {
        let track = self.tabs.get(&tab)?;
        match track.active.as_ref()? {
            Active::Unbound(index) => track.unbound.get(*index).map(|r| r.record.turn_id.clone()),
            Active::Bound { session, position } => self
                .sessions
                .get(session)?
                .records
                .iter()
                .find(|r| r.position == *position)
                .map(|r| r.record.turn_id.clone()),
        }
    }

    fn active_rec_mut(&mut self, tab: u64) -> Option<&mut Rec> {
        let track = self.tabs.get_mut(&tab)?;
        match track.active.clone()? {
            Active::Unbound(index) => track.unbound.get_mut(index),
            Active::Bound { session, position } => self.sessions.get_mut(&session)?.record_mut(position),
        }
    }

    fn latest_rec_mut(&mut self, tab: u64) -> Option<&mut Rec> {
        let track = self.tabs.get_mut(&tab)?;
        match track.latest.clone()? {
            Active::Unbound(index) => track.unbound.get_mut(index),
            Active::Bound { session, position } => self.sessions.get_mut(&session)?.record_mut(position),
        }
    }

    /// Whether `turn_id` is the latest turn of the session the tab is running now (or of its turns
    /// still waiting for an id). A turn of a session that ended does not count: ids are the
    /// provider's, and a new session may reuse one.
    fn is_latest_turn(&self, tab: u64, turn_id: &str) -> bool {
        let Some(track) = self.tabs.get(&tab) else {
            return false;
        };
        match &track.latest {
            Some(Active::Unbound(index)) => track.unbound.get(*index).is_some_and(|r| r.record.turn_id == turn_id),
            Some(Active::Bound { session, position }) => {
                track.live_session() == Some(session.as_str())
                    && self
                        .sessions
                        .get(session)
                        .and_then(|s| s.records.iter().find(|r| r.position == *position))
                        .is_some_and(|r| r.record.turn_id == turn_id)
            }
            None => false,
        }
    }

    /// Follows the session the tab's backend reports. An id it has not bound yet binds it (the
    /// projection may have folded the session's `SessionOpened` before a turn's start reached
    /// here). No id while the tab is running a session means the backend is a new one, which has
    /// not reported its session yet: the tab lets go of the old one, and the new turns wait.
    fn follow_backend(&mut self, tab: u64, session: &str, now_ms: u64) {
        let live = self.tabs.get(&tab).and_then(TabTrack::live_session).map(str::to_owned);
        if session.is_empty() {
            if live.is_some() {
                // That session's running turn missed its end, which is taken only now.
                self.release(tab, now_ms, true);
            }
        } else if live.as_deref() != Some(session) {
            self.bind(tab, session, now_ms, false);
        }
    }

    /// The tab's session ended: its running turn is over, and nothing the tab starts later belongs
    /// to it. Its turns stay, for the tab's review, until the backend goes; turns it ran before
    /// reporting its id never will learn one, and are dropped.
    fn session_ended(&mut self, tab: u64, now_ms: u64) {
        self.end_active(tab, now_ms, false);
        if let Some(track) = self.tabs.get_mut(&tab) {
            track.live = false;
            track.drop_unbound();
        }
    }

    /// The tab lets go of its session: its running turn ends, its turns that never learned a
    /// session are dropped (no review can name them), and the session's worker ends once no other
    /// tab holds it.
    fn release(&mut self, tab: u64, now_ms: u64, overlapped_next: bool) {
        self.end_active(tab, now_ms, overlapped_next);
        let Some(track) = self.tabs.get_mut(&tab) else {
            return;
        };
        track.drop_unbound();
        track.live = false;
        if let Some(session) = track.session.take() {
            if let Some(s) = self.sessions.get_mut(&session) {
                s.tabs.remove(&tab);
            }
        }
    }

    /// The session's track, starting its worker the first time.
    fn ensure_session(&mut self, session: &str) {
        if self.sessions.contains_key(session) {
            return;
        }
        let (jobs, receiver) = mpsc::channel();
        let worker = Worker {
            session: session.to_owned(),
            spec: self.spec.clone(),
            options: self.options.clone(),
            trim_stamp: self.trim_stamp.clone(),
            done: self.done_tx.clone(),
        };
        let spawned = std::thread::Builder::new()
            .name("eitri-review".into())
            .spawn(move || worker.run(receiver));
        if let Err(e) = spawned {
            eprintln!("[turn-review] the snapshot worker could not start: {e}");
        }
        self.sessions.insert(
            session.to_owned(),
            SessionTrack {
                jobs,
                offset: None,
                numbering_failed: false,
                records: Vec::new(),
                next_position: 1,
                outstanding: 0,
                tabs: BTreeSet::new(),
                warmed: false,
            },
        );
    }

    /// Hands a snapshot job to the session's worker.
    fn send(&mut self, session: &str, job: Job) {
        self.send_task(session, Task::Snapshot(job));
    }

    /// Hands a task to the session's worker. The channel is unbounded, so this never waits; a
    /// worker that is gone answers a snapshot as unavailable at once.
    fn send_task(&mut self, session: &str, task: Task) {
        let Some(s) = self.sessions.get_mut(session) else {
            return;
        };
        s.outstanding += 1;
        if let Err(mpsc::SendError(task)) = s.jobs.send(task) {
            let done = match task {
                Task::Snapshot(job) => Done::Snapshot {
                    kind: job.kind,
                    position: job.position,
                    snap: Snap::Unavailable("the snapshot worker is not running".into()),
                    left_out: LeftOut::new(),
                    at_ms: wall_clock_ms(),
                    files: None,
                },
                Task::Marks { .. } => Done::Marked,
            };
            let _ = self.done_tx.send((session.to_owned(), done));
        }
    }

    /// Hands the worker the marks of every turn whose marks changed since they were last handed
    /// over, so that a later run, which knows the turn only from the shadow, says what this one
    /// says. Some are learnt after the turn's end snapshot was asked for, so they cannot simply
    /// travel with a snapshot.
    fn flush_marks(&mut self) {
        let mut tasks = Vec::new();
        for (session, track) in &mut self.sessions {
            for rec in &mut track.records {
                let marks = rec.record.kept_marks();
                if marks != rec.marks_sent {
                    rec.marks_sent = marks;
                    tasks.push((
                        session.clone(),
                        Task::Marks {
                            position: rec.position,
                            turn_id: rec.record.turn_id.clone(),
                            marks,
                        },
                    ));
                }
            }
        }
        for (session, task) in tasks {
            self.send_task(&session, task);
        }
    }
}

fn job_of(kind: SnapshotKind, rec: &Rec, time_ms: u64) -> Job {
    Job {
        kind,
        position: rec.position,
        turn_id: rec.record.turn_id.clone(),
        tab: rec.record.tab,
        time_ms,
    }
}

// ---- the worker ----------------------------------------------------------------------------------

/// One session's snapshot worker. Its jobs run strictly in the order they were sent, so the end of
/// one turn always finishes before the base of the next starts.
struct Worker {
    session: String,
    spec: ShadowSpec,
    options: ReviewOptions,
    trim_stamp: Arc<AtomicU64>,
    done: mpsc::Sender<(String, Done)>,
}

impl Worker {
    fn run(self, tasks: mpsc::Receiver<Task>) {
        let mut shadow: Option<Shadow> = None;
        let mut offset: Option<u32> = None;
        let mut bases: BTreeMap<u32, String> = BTreeMap::new();
        for task in tasks {
            let done = match task {
                Task::Snapshot(job) => {
                    let (kind, position) = (job.kind, job.position);
                    self.hook(JobEvent::Starting {
                        session: self.session.clone(),
                        kind,
                        position,
                    });
                    let done = self.take(&job, &mut shadow, &mut offset, &mut bases);
                    self.hook(JobEvent::Finished {
                        session: self.session.clone(),
                        kind,
                        position,
                    });
                    done
                }
                Task::Marks {
                    position,
                    turn_id,
                    marks,
                } => {
                    self.mark(position, &turn_id, marks, &mut shadow, &mut offset);
                    Done::Marked
                }
            };
            let _ = self.done.send((self.session.clone(), done));
        }
    }

    fn hook(&self, event: JobEvent) {
        if let Some(hook) = &self.options.on_job {
            hook(&event);
        }
    }

    fn take(
        &self,
        job: &Job,
        shadow: &mut Option<Shadow>,
        offset: &mut Option<u32>,
        bases: &mut BTreeMap<u32, String>,
    ) -> Done {
        let unavailable = |reason: String| Done::Snapshot {
            kind: job.kind,
            position: job.position,
            snap: Snap::Unavailable(reason),
            left_out: LeftOut::new(),
            at_ms: wall_clock_ms(),
            files: None,
        };
        let (shadow, offset) = match self.ready(shadow, offset) {
            Ok(ready) => ready,
            Err(reason) => return unavailable(reason),
        };
        let turn = match job.kind {
            SnapshotKind::Warm => 0,
            _ => offset + job.position,
        };
        let label = SnapshotLabel {
            turn,
            kind: job.kind,
            turn_id: job.turn_id.clone(),
            tab: job.tab,
            time_ms: job.time_ms,
        };
        let (snap, left_out) = match shadow.snapshot(&self.session, &label, &self.options.limits) {
            SnapshotOutcome::Taken(snapshot) => (
                Snap::Taken {
                    commit: snapshot.commit,
                },
                snapshot.left_out,
            ),
            SnapshotOutcome::Unavailable(reason) => (Snap::Unavailable(reason), LeftOut::new()),
        };
        let at_ms = wall_clock_ms();
        let mut files = None;
        match (job.kind, snap.commit()) {
            (SnapshotKind::Base, Some(commit)) => {
                bases.insert(job.position, commit.to_owned());
            }
            (SnapshotKind::End, Some(end)) => {
                if let Some(base) = bases.remove(&job.position) {
                    files = changed_files(shadow, &base, &Side::Snapshot(end.to_owned()))
                        .ok()
                        .map(|changes| changes.len());
                }
            }
            _ => {}
        }
        if job.kind == SnapshotKind::End {
            self.retain(shadow);
        }
        Done::Snapshot {
            kind: job.kind,
            position: job.position,
            snap,
            left_out,
            at_ms,
            files,
        }
    }

    /// The opened store and the number this run's turns of the session count on from, opening and
    /// reading them the first time. A failure is reported as the session's numbering failing.
    fn ready<'a>(&self, shadow: &'a mut Option<Shadow>, offset: &mut Option<u32>) -> Result<(&'a Shadow, u32), String> {
        if shadow.is_none() {
            match self.spec.open() {
                Ok(opened) => *shadow = Some(opened),
                Err(reason) => {
                    let _ = self
                        .done
                        .send((self.session.clone(), Done::Numbering(Err(reason.clone()))));
                    return Err(reason);
                }
            }
        }
        let shadow = shadow.as_ref().expect("opened above");
        // The numbers earlier runs of this session reached, read before this worker writes any
        // ref: numbering from 1 again would overwrite their snapshots. A turn known only by its
        // marks counts too.
        if offset.is_none() {
            match shadow.snapshot_ref_ids() {
                Ok(refs) => {
                    let reached = refs
                        .iter()
                        .filter_map(|r| r.session_turn())
                        .filter(|(session, _)| *session == self.session)
                        .map(|(_, n)| n)
                        .max()
                        .unwrap_or(0);
                    *offset = Some(reached);
                    let _ = self.done.send((self.session.clone(), Done::Numbering(Ok(reached))));
                }
                Err(e) => {
                    let reason = format!("earlier snapshots could not be read: {e}");
                    let _ = self
                        .done
                        .send((self.session.clone(), Done::Numbering(Err(reason.clone()))));
                    return Err(reason);
                }
            }
        }
        Ok((shadow, offset.expect("read above")))
    }

    /// Records a turn's marks in the store. Losing them costs only what a later run can say about
    /// the turn, so a failure is logged, not reported.
    fn mark(
        &self,
        position: u32,
        turn_id: &str,
        marks: TurnMarks,
        shadow: &mut Option<Shadow>,
        offset: &mut Option<u32>,
    ) {
        let Ok((shadow, offset)) = self.ready(shadow, offset) else {
            return;
        };
        let turn = offset + position;
        if let Err(e) = shadow.write_marks(&self.session, turn, turn_id, marks) {
            eprintln!("[turn-review] what turn {turn} must say about itself could not be kept: {e}");
        }
    }

    /// Runs retention when no worker of this project has in the last period. Here, on the
    /// worker, so the session's next base waits for it rather than racing it for the lock.
    fn retain(&self, shadow: &Shadow) {
        let now = wall_clock_ms();
        let last = self.trim_stamp.load(Ordering::Acquire);
        let every = u64::try_from(self.options.trim_every.as_millis()).unwrap_or(u64::MAX);
        if now.saturating_sub(last) < every {
            return;
        }
        if self
            .trim_stamp
            .compare_exchange(last, now, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        if let Err(e) = shadow.trim(self.options.keep, self.options.max_age, now, "1.hour.ago") {
            eprintln!("[turn-review] retention could not run: {e}");
        }
    }
}

// ---- reviews -------------------------------------------------------------------------------------

/// A review of one turn or session, computed with git by [`run`](Self::run) on whatever thread
/// runs it.
#[derive(Debug, Clone)]
pub struct OverviewJob {
    spec: ShadowSpec,
    session: String,
    records: Vec<TurnRecord>,
    turn: TurnRef,
    scope: Scope,
    named: NamedPaths,
}

impl OverviewJob {
    pub fn run(self) -> Result<Overview, ReviewError> {
        let shadow = self.spec.open().map_err(ReviewError::Unavailable)?;
        let turns = merged_turns(&shadow, &self.session, self.records)?;
        // Made relative here, on the worker: it reads the file system.
        let named = self.named.relative_to(shadow.work_tree());
        let (mut overview, plan) = head(self.session, turns, self.turn, self.scope, named.pending_no_result)?;
        let (from, to) = match plan.compare {
            Compare::Between { from, to } => (from, to),
            // Nothing is shown as the turn's change; the notes say why.
            Compare::NoBaseline(_) | Compare::NotReady => return Ok(overview),
        };
        overview.compared = true;
        let changes = changed_files(&shadow, &from, &to).map_err(|e| ReviewError::Unavailable(e.to_string()))?;
        let mut rows: BTreeMap<PathBuf, OverviewFile> = BTreeMap::new();
        for change in changes {
            if plan.too_large.contains(&change.path) {
                continue;
            }
            let Some(origin) = origin(&change.path, &named, true) else {
                continue;
            };
            rows.insert(
                change.path.clone(),
                OverviewFile {
                    path: change.path,
                    added: change.added,
                    removed: change.removed,
                    origin,
                    binary: change.binary,
                    too_large: false,
                    nested: change.nested,
                },
            );
        }
        // Left out of both snapshots, or of the base with the disk as the end: reported only when
        // what can be compared without reading the file says it differs.
        let mut unchanged = plan.large_unchanged.clone();
        for (path, print) in &plan.large_vs_disk {
            if FilePrint::of_path(shadow.work_tree(), path) == Some(*print) {
                unchanged.insert(path.clone());
            }
        }
        for path in plan.too_large.iter().filter(|p| !unchanged.contains(*p)) {
            let origin = if named.paths.contains(path) {
                Origin::Agent
            } else {
                Origin::Workspace
            };
            rows.insert(
                path.clone(),
                OverviewFile {
                    path: path.clone(),
                    added: 0,
                    removed: 0,
                    origin,
                    binary: false,
                    too_large: true,
                    nested: false,
                },
            );
        }
        for path in &named.paths {
            // A named file that is too large stays marked so, whether or not its prints say it
            // changed: its patch is refused, and the row must not offer one.
            rows.entry(path.clone()).or_insert_with(|| OverviewFile {
                path: path.clone(),
                added: 0,
                removed: 0,
                origin: Origin::AgentOnly,
                binary: false,
                too_large: plan.too_large.contains(path),
                nested: false,
            });
        }
        overview.files = rows.into_values().collect();
        Ok(overview)
    }
}

/// One file's patch in a review, computed with git by [`run`](Self::run) on whatever thread runs it.
#[derive(Debug, Clone)]
pub struct DiffJob {
    spec: ShadowSpec,
    session: String,
    records: Vec<TurnRecord>,
    turn: u32,
    scope: Scope,
    path: PathBuf,
}

impl DiffJob {
    pub fn run(self) -> Result<ReviewDiff, ReviewError> {
        let shadow = self.spec.open().map_err(ReviewError::Unavailable)?;
        let Resolved {
            from,
            to,
            current,
            too_large,
        } = resolve_compare(&shadow, &self.session, &self.records, TurnRef::N(self.turn), self.scope)?;
        if too_large.contains(&self.path) {
            return Err(ReviewError::TooLarge(self.path));
        }
        let failed = |e: super::shadow::ShadowError| ReviewError::Unavailable(e.to_string());
        check_unambiguous(&shadow, &from, &to, &self.path)?;
        match file_hunks(&shadow, &from, &to, &self.path, MAX_OVERVIEW_DIFF_LINES).map_err(failed)? {
            Some(diff) => Ok(ReviewDiff {
                turn: current,
                path: self.path,
                added: diff.added,
                removed: diff.removed,
                binary: diff.binary,
                new_file: diff.new_file,
                deleted_file: diff.deleted_file,
                mode_change: diff.mode_change,
                hunks: Some(diff.hunks),
            }),
            None => {
                // Too long to show: the counts still come from the file list.
                let change = changed_files(&shadow, &from, &to)
                    .map_err(failed)?
                    .into_iter()
                    .find(|c| c.path == self.path);
                let (added, removed, binary) = change
                    .as_ref()
                    .map_or((0, 0, false), |c| (c.added, c.removed, c.binary));
                Ok(ReviewDiff {
                    turn: current,
                    path: self.path,
                    added,
                    removed,
                    binary,
                    new_file: change
                        .as_ref()
                        .is_some_and(|c| c.kind == super::diff::ChangeKind::Added),
                    deleted_file: change
                        .as_ref()
                        .is_some_and(|c| c.kind == super::diff::ChangeKind::Deleted),
                    mode_change: None,
                    hunks: None,
                })
            }
        }
    }
}

/// What one file's review, revert or anchor compares, resolved from the snapshots the shadow keeps
/// now rather than from a copy made when the review was opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resolved {
    /// The base side's commit.
    pub(crate) from: String,
    /// The end side: a snapshot, or the files on disk when the turn has no end snapshot.
    pub(crate) to: Side,
    /// The turn shown (for the session scope, the latest).
    pub(crate) current: u32,
    /// Files left out of a snapshot of the turns concerned for their size.
    pub(crate) too_large: BTreeSet<PathBuf>,
}

/// The two sides of `session`'s review at `turn` in `scope`, with the shadow's own turns merged
/// in. A turn with no baseline, or one still being taken, is an error: there is nothing to compare.
pub(crate) fn resolve_compare(
    shadow: &Shadow,
    session: &str,
    records: &[TurnRecord],
    turn: TurnRef,
    scope: Scope,
) -> Result<Resolved, ReviewError> {
    let turns = merged_turns(shadow, session, records.to_vec())?;
    let plan = plan(&turns, turn, scope)?;
    match plan.compare {
        Compare::Between { from, to } => Ok(Resolved {
            from,
            to,
            current: plan.current,
            too_large: plan.too_large,
        }),
        Compare::NoBaseline(reason) => Err(ReviewError::NoBaseline(reason)),
        Compare::NotReady => Err(ReviewError::NotReady(NOTE_BASE_PENDING.into())),
    }
}

/// Refuses a path that cannot be told apart from another changed file's name. The wire carries a
/// path as text, so a file whose name is not valid UTF-8 is listed with replacement characters,
/// and the same text names a different file that really has them. A name with one is served only
/// when exactly one changed file reads that way and its bytes are the ones asked for.
pub(crate) fn check_unambiguous(shadow: &Shadow, from: &str, to: &Side, path: &Path) -> Result<(), ReviewError> {
    if !path.to_str().is_some_and(|p| p.contains('\u{FFFD}')) {
        return Ok(());
    }
    let wanted = path.to_string_lossy();
    let same_text: Vec<PathBuf> = changed_files(shadow, from, to)
        .map_err(|e| ReviewError::Unavailable(e.to_string()))?
        .into_iter()
        .map(|c| c.path)
        .filter(|p| p.to_string_lossy() == wanted)
        .collect();
    if same_text != [path.to_path_buf()] {
        return Err(ReviewError::AmbiguousPath(path.to_path_buf()));
    }
    Ok(())
}

/// What a review compares.
enum Compare {
    Between { from: String, to: Side },
    NoBaseline(String),
    NotReady,
}

struct Plan {
    current: u32,
    compare: Compare,
    notes: Vec<String>,
    /// Files left out of a snapshot being compared for their size: nothing can be shown of them,
    /// and none can be reverted.
    too_large: BTreeSet<PathBuf>,
    /// Of those, the ones both snapshots left out and whose prints are equal: no change to report.
    large_unchanged: BTreeSet<PathBuf>,
    /// When the end side is the disk (no end snapshot), the print each base-side file had; the job
    /// reads the disk's own and drops those that match.
    large_vs_disk: BTreeMap<PathBuf, FilePrint>,
}

/// What the files left out for size amount to between two compared sides. `end` is `None` when the
/// end side is the disk, which is never left out of anything.
///
/// A file left out of one snapshot only is a change (it is in the other as a file, or gone). A file
/// left out of both is a change unless both prints are known and equal; a record written before
/// prints were kept has none, and then the file reads as possibly changed, as it did.
fn large_files(base: &LeftOut, end: Option<&LeftOut>) -> LargeFiles {
    let mut found = LargeFiles::default();
    found.too_large.extend(base.keys().cloned());
    match end {
        Some(end) => {
            found.too_large.extend(end.keys().cloned());
            for (path, print) in base {
                if let (Some(a), Some(Some(b))) = (print, end.get(path)) {
                    if a == b {
                        found.unchanged.insert(path.clone());
                    }
                }
            }
        }
        None => {
            for (path, print) in base {
                if let Some(print) = print {
                    found.vs_disk.insert(path.clone(), *print);
                }
            }
        }
    }
    found
}

#[derive(Default)]
struct LargeFiles {
    too_large: BTreeSet<PathBuf>,
    unchanged: BTreeSet<PathBuf>,
    vs_disk: BTreeMap<PathBuf, FilePrint>,
}

/// The review's head: the turns, the one shown and what the review must say about itself, with
/// nothing compared yet (`files` empty, `compared` false).
fn head(
    session: String,
    turns: Vec<TurnRecord>,
    turn: TurnRef,
    scope: Scope,
    pending_no_result: usize,
) -> Result<(Overview, Plan), ReviewError> {
    let mut plan = plan(&turns, turn, scope)?;
    let overview = Overview {
        session,
        scope,
        current: plan.current,
        turns,
        files: Vec::new(),
        compared: false,
        pending_no_result,
        notes: std::mem::take(&mut plan.notes),
    };
    Ok((overview, plan))
}

/// What the shadow keeps of one turn.
#[derive(Default)]
struct Kept {
    base: Option<SnapshotRef>,
    end: Option<SnapshotRef>,
    marks: Option<TurnMarks>,
}

/// The turns of `session`: this run's records, and the turns the shadow keeps that this run did
/// not see (earlier runs, or another window), with the marks and left-out files they were kept
/// with. A record whose snapshots retention has since removed is left out.
fn merged_turns(shadow: &Shadow, session: &str, records: Vec<TurnRecord>) -> Result<Vec<TurnRecord>, ReviewError> {
    let refs = shadow
        .snapshot_ref_ids()
        .map_err(|e| ReviewError::Unavailable(format!("earlier snapshots could not be read: {e}")))?;
    let mut kept: BTreeMap<u32, Kept> = BTreeMap::new();
    for r in refs {
        if let Some(marks) = r.marks {
            if let Some((s, n)) = r.session_turn() {
                if s == session && n != 0 {
                    kept.entry(n).or_default().marks = Some(marks);
                }
            }
            continue;
        }
        let Some((s, n, kind)) = r.parts() else {
            continue;
        };
        if s != session || n == 0 {
            continue;
        }
        let entry = kept.entry(n).or_default();
        match kind {
            SnapshotKind::Base => entry.base = Some(r),
            SnapshotKind::End => entry.end = Some(r),
            SnapshotKind::Warm => {}
        }
    }
    let has_snapshot = |n: &u32| kept.get(n).is_some_and(|k| k.base.is_some() || k.end.is_some());
    let mut turns: BTreeMap<u32, TurnRecord> = BTreeMap::new();
    for record in records {
        let trimmed = record.base.commit().is_some() && !has_snapshot(&record.n);
        if !trimmed {
            turns.insert(record.n, record);
        }
    }
    for (n, Kept { base, end, marks }) in kept {
        // A turn kept only by its marks has nothing to compare and no times to show.
        if turns.contains_key(&n) || (base.is_none() && end.is_none()) {
            continue;
        }
        let marks = marks.unwrap_or_default();
        let mut skipped_large: Vec<PathBuf> = Vec::new();
        for path in base.iter().chain(end.iter()).flat_map(|r| r.skipped_large.iter()) {
            if !skipped_large.contains(path) {
                skipped_large.push(path.clone());
            }
        }
        let left_out = LeftOutSides {
            base: base.as_ref().map(|r| r.left_out.clone()).unwrap_or_default(),
            end: end.as_ref().map(|r| r.left_out.clone()).unwrap_or_default(),
        };
        let label = base
            .as_ref()
            .and_then(|r| r.label.clone())
            .or_else(|| end.as_ref().and_then(|r| r.label.clone()));
        let started_ms = base.as_ref().and_then(|r| r.label.as_ref()).map_or(0, |l| l.time_ms);
        turns.insert(
            n,
            TurnRecord {
                n,
                turn_id: label.as_ref().map(|l| l.turn_id.clone()).unwrap_or_default(),
                tab: label.as_ref().map_or(0, |l| l.tab),
                session: session.to_owned(),
                started_ms,
                ended_ms: end.as_ref().and_then(|r| r.label.as_ref()).map(|l| l.time_ms),
                base: match &base {
                    Some(r) => Snap::Taken {
                        commit: r.commit.clone(),
                    },
                    None => Snap::Unavailable("no baseline snapshot was kept".into()),
                },
                base_taken_ms: base.as_ref().and_then(|r| r.taken_ms),
                end: match &end {
                    Some(r) => Snap::Taken {
                        commit: r.commit.clone(),
                    },
                    None => Snap::Unavailable(NOTE_UNFINISHED.into()),
                },
                late: marks.late,
                overlapped_next: marks.overlapped_next,
                overlapped_tab: marks.overlapped_tab,
                skipped_large,
                left_out,
                earlier_run: true,
            },
        );
    }
    Ok(turns.into_values().collect())
}

fn plan(turns: &[TurnRecord], turn: TurnRef, scope: Scope) -> Result<Plan, ReviewError> {
    let latest = turns.last().ok_or(ReviewError::NoTurns)?;
    match scope {
        Scope::Turn => {
            let current = match turn {
                TurnRef::Latest => latest,
                TurnRef::N(n) => turns
                    .iter()
                    .find(|t| t.n == n)
                    .ok_or_else(|| ReviewError::NoSuchTurn(format!("turn {n} of this session is not kept")))?,
            };
            let mut notes = vec![NOTE_TURN.to_string()];
            notes.extend(marks(current));
            let compare = match (current.state(), current.base.commit()) {
                (TurnState::NoBaseline(reason), _) => {
                    notes.push(format!("no baseline: {reason}"));
                    Compare::NoBaseline(reason)
                }
                (_, None) => {
                    notes.push(NOTE_BASE_PENDING.into());
                    Compare::NotReady
                }
                (_, Some(base)) => Compare::Between {
                    from: base.to_owned(),
                    to: current
                        .end
                        .commit()
                        .map_or(Side::WorkTree, |end| Side::Snapshot(end.to_owned())),
                },
            };
            let large = large_files(
                &current.left_out.base,
                current.end.commit().map(|_| &current.left_out.end),
            );
            Ok(Plan {
                current: current.n,
                compare,
                notes,
                too_large: large.too_large,
                large_unchanged: large.unchanged,
                large_vs_disk: large.vs_disk,
            })
        }
        Scope::Session => {
            let mut notes = vec![NOTE_SESSION.to_string()];
            let Some(first) = turns.iter().find(|t| t.base.commit().is_some()) else {
                let reason = "no turn of this session has a baseline".to_string();
                notes.push(format!("no baseline: {reason}"));
                return Ok(Plan {
                    current: latest.n,
                    compare: Compare::NoBaseline(reason),
                    notes,
                    too_large: BTreeSet::new(),
                    large_unchanged: BTreeSet::new(),
                    large_vs_disk: BTreeMap::new(),
                });
            };
            let covered: Vec<&TurnRecord> = turns.iter().filter(|t| t.n >= first.n).collect();
            for t in &covered {
                notes.extend(marks(t).into_iter().map(|m| format!("turn {}: {m}", t.n)));
            }
            let to = latest
                .end
                .commit()
                .map_or(Side::WorkTree, |end| Side::Snapshot(end.to_owned()));
            // Only the two snapshots compared can leave a file out of the comparison: a file too large
            // in a turn between them is in neither.
            let large = large_files(&first.left_out.base, latest.end.commit().map(|_| &latest.left_out.end));
            Ok(Plan {
                current: latest.n,
                compare: Compare::Between {
                    from: first.base.commit().expect("found by it").to_owned(),
                    to,
                },
                notes,
                too_large: large.too_large,
                large_unchanged: large.unchanged,
                large_vs_disk: large.vs_disk,
            })
        }
    }
}

/// What a turn's review must say about its own precision.
fn marks(turn: &TurnRecord) -> Vec<String> {
    let mut notes = Vec::new();
    if turn.late {
        let at = turn.base_taken_ms.unwrap_or(turn.started_ms);
        notes.push(format!(
            "baseline late: changes made before {} may be missing",
            clock_time(at)
        ));
    }
    if turn.overlapped_next {
        notes.push(NOTE_OVERLAPPED_NEXT.into());
    }
    if turn.overlapped_tab {
        notes.push(NOTE_OVERLAPPED_TAB.into());
    }
    match turn.state() {
        TurnState::Unfinished(_) if turn.earlier_run => notes.push(NOTE_UNFINISHED.into()),
        TurnState::Unfinished(reason) => notes.push(format!(
            "no end snapshot ({reason}): compared with the files on disk now"
        )),
        TurnState::Pending if turn.base.commit().is_some() => notes.push(
            if turn.ended_ms.is_some() {
                NOTE_END_PENDING
            } else {
                NOTE_RUNNING
            }
            .into(),
        ),
        _ => {}
    }
    notes
}

/// `HH:MM:SS` in local time.
fn clock_time(ms: u64) -> String {
    let secs = libc::time_t::try_from(ms / 1000).unwrap_or(0);
    // SAFETY: `localtime_r` writes only into `tm`, which lives for the call; both pointers are
    // valid and unaliased.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let converted = unsafe { !libc::localtime_r(&secs, &mut tm).is_null() };
    if converted {
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    } else {
        let day = ms / 1000 % 86_400;
        format!("{:02}:{:02}:{:02} UTC", day / 3600, day / 60 % 60, day % 60)
    }
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

    fn record(base: Snap, end: Snap) -> TurnRecord {
        TurnRecord {
            n: 1,
            turn_id: "t1".into(),
            tab: 1,
            session: "s1".into(),
            started_ms: 0,
            ended_ms: None,
            base,
            base_taken_ms: None,
            end,
            late: false,
            overlapped_next: false,
            overlapped_tab: false,
            skipped_large: Vec::new(),
            left_out: LeftOutSides::default(),
            earlier_run: false,
        }
    }

    fn taken(c: &str) -> Snap {
        Snap::Taken { commit: c.into() }
    }

    #[test]
    fn a_turn_is_ok_only_with_both_snapshots() {
        let gone = || Snap::Unavailable("why".into());
        assert_eq!(record(taken("a"), taken("b")).state(), TurnState::Ok);
        assert_eq!(record(taken("a"), Snap::Pending).state(), TurnState::Pending);
        assert_eq!(record(Snap::Pending, taken("b")).state(), TurnState::Pending);
        assert_eq!(record(Snap::Pending, Snap::Pending).state(), TurnState::Pending);
        assert_eq!(record(taken("a"), gone()).state(), TurnState::Unfinished("why".into()));
        assert_eq!(record(gone(), taken("b")).state(), TurnState::NoBaseline("why".into()));
        assert_eq!(record(gone(), Snap::Pending).state().as_str(), "no_baseline");
    }

    #[test]
    fn the_session_scope_runs_from_the_first_base_to_the_latest_end_or_the_disk() {
        let mut first = record(Snap::Unavailable("no".into()), taken("e0"));
        first.n = 1;
        let mut second = record(taken("b2"), taken("e2"));
        second.n = 2;
        second.late = true;
        let mut third = record(taken("b3"), taken("e3"));
        third.n = 3;
        let plan_of = |turns: &[TurnRecord]| plan(turns, TurnRef::Latest, Scope::Session).unwrap();

        let done = plan_of(&[first.clone(), second.clone(), third.clone()]);
        assert_eq!(done.current, 3);
        match done.compare {
            Compare::Between { from, to } => {
                assert_eq!(from, "b2");
                assert_eq!(to, Side::Snapshot("e3".into()));
            }
            _ => panic!("a session with a base is compared"),
        }
        assert!(done.notes[1].starts_with("turn 2: baseline late"), "{:?}", done.notes);

        third.end = Snap::Pending;
        let running = plan_of(&[first.clone(), second.clone(), third]);
        assert!(matches!(running.compare, Compare::Between { to: Side::WorkTree, .. }));

        // The latest turn has no base of its own, but its end still closes the session's range.
        let mut fourth = record(Snap::Unavailable("no".into()), taken("e4"));
        fourth.n = 4;
        let ended = plan_of(&[first.clone(), second.clone(), fourth]);
        match ended.compare {
            Compare::Between { from, to } => {
                assert_eq!(from, "b2");
                assert_eq!(to, Side::Snapshot("e4".into()));
            }
            _ => panic!("a session with a base is compared"),
        }

        let none = plan_of(&[first]);
        assert!(matches!(none.compare, Compare::NoBaseline(_)));
    }

    #[test]
    fn an_ended_turn_whose_end_is_still_being_taken_does_not_say_it_is_running() {
        let mut running = record(taken("a"), Snap::Pending);
        assert_eq!(marks(&running), vec![NOTE_RUNNING.to_string()]);
        running.ended_ms = Some(5);
        assert_eq!(marks(&running), vec![NOTE_END_PENDING.to_string()]);
    }

    /// The panel draws a review's `notes` as they are and nothing from the turn flags, so what the
    /// header says is exactly what these notes say. The envelopes are written to a fixture the web
    /// tests render (`agent-ui/web/src/fixtures/review-notes.json`); this compares it on every run,
    /// and `EITRI_WRITE_FIXTURES=1` rewrites it. The local clock time in a late-baseline note is
    /// replaced by `HH:MM:SS`, so the file does not depend on the time zone it was written in.
    #[test]
    fn the_review_notes_fixture_is_what_the_serializer_sends() {
        const AT: u64 = 1_790_000_000_000;
        let turn = |n: u32, base: Snap, end: Snap| TurnRecord {
            n,
            turn_id: format!("t{n}"),
            started_ms: AT + u64::from(n) * 60_000,
            ended_ms: end.commit().map(|_| AT + u64::from(n) * 60_000 + 20_000),
            base_taken_ms: base.commit().map(|_| AT + u64::from(n) * 60_000 + 300),
            ..record(base, end)
        };
        let gone = |why: &str| Snap::Unavailable(why.into());
        let mut flagged = turn(3, taken("b3"), taken("e3"));
        flagged.late = true;
        flagged.overlapped_next = true;
        flagged.overlapped_tab = true;
        let no_baseline = turn(4, gone("the snapshot took too long"), taken("e4"));
        let mut end_failed = turn(
            5,
            taken("b5"),
            gone("too many files to snapshot (20001, the limit is 20000)"),
        );
        end_failed.ended_ms = Some(AT + 5 * 60_000 + 20_000);
        let mut earlier = turn(6, taken("b6"), gone(NOTE_UNFINISHED));
        earlier.earlier_run = true;
        let base_pending = turn(7, Snap::Pending, Snap::Pending);
        let running = turn(8, taken("b8"), Snap::Pending);
        let ok = turn(9, taken("b9"), taken("e9"));

        let cases: Vec<(&str, Vec<TurnRecord>, TurnRef, Scope)> = vec![
            ("flagged", vec![ok.clone(), flagged.clone()], TurnRef::N(3), Scope::Turn),
            ("noBaseline", vec![no_baseline.clone()], TurnRef::Latest, Scope::Turn),
            ("endFailed", vec![end_failed.clone()], TurnRef::Latest, Scope::Turn),
            ("earlierRun", vec![earlier], TurnRef::Latest, Scope::Turn),
            (
                "basePending",
                vec![ok.clone(), base_pending.clone()],
                TurnRef::Latest,
                Scope::Turn,
            ),
            ("running", vec![running.clone()], TurnRef::Latest, Scope::Turn),
            ("ok", vec![ok.clone()], TurnRef::Latest, Scope::Turn),
            (
                "sessionNoBaseline",
                vec![no_baseline, base_pending],
                TurnRef::Latest,
                Scope::Session,
            ),
            (
                "session",
                vec![flagged.clone(), end_failed, running],
                TurnRef::Latest,
                Scope::Session,
            ),
        ];
        let late_at = clock_time(flagged.base_taken_ms.unwrap());
        let mut envelopes = serde_json::Map::new();
        for (name, turns, at, scope) in cases {
            let (mut overview, plan) = head("sess".into(), turns, at, scope, 0).unwrap();
            // What `OverviewJob::run` does once it has compared something.
            overview.compared = matches!(plan.compare, Compare::Between { .. });
            let json = crate::agent_bridge::serialize_review_for_js(
                name,
                crate::tabs::TabId(3),
                &overview,
                &crate::turn_review::ReviewDraft::default(),
            )
            .replace(&late_at, "HH:MM:SS");
            envelopes.insert(name.into(), serde_json::from_str(&json).unwrap());
        }
        let fixture = serde_json::json!({
            "writtenBy": "core/src/turn_review/lifecycle.rs, tests::the_review_notes_fixture_is_what_the_serializer_sends \
                          -- compared on every run; EITRI_WRITE_FIXTURES=1 rewrites it",
            "envelopes": envelopes,
        });
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/review-notes.json");
        let written = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
        let committed = std::fs::read_to_string(&path).ok();
        if committed.as_deref() == Some(written.as_str()) {
            return;
        }
        if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
            std::fs::write(&path, &written).expect("write the review notes fixture");
        } else {
            assert_eq!(
                committed.as_deref(),
                Some(written.as_str()),
                "the committed review notes fixture is not what this test produces; if the change is \
                 intended, regenerate it with EITRI_WRITE_FIXTURES=1"
            );
        }
    }

    #[test]
    fn a_turn_named_that_is_not_kept_is_refused() {
        let turns = [record(taken("a"), taken("b"))];
        assert!(matches!(
            plan(&turns, TurnRef::N(9), Scope::Turn),
            Err(ReviewError::NoSuchTurn(_))
        ));
        assert!(matches!(
            plan(&[], TurnRef::Latest, Scope::Turn),
            Err(ReviewError::NoTurns)
        ));
    }
}
