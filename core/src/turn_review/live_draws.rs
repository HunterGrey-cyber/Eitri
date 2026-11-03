//! One window's live drawings, decided without I/O: which edited files to ask the editor about,
//! which worker jobs to start, what to draw or clear, and what to tell the contested-paths table.
//!
//! The caller feeds it the drained edits, the editor's "is it shown" answers, the jobs' results and
//! the window's own events, then turns each [`LiveStep`] of [`LiveDraws::steps`] into a call. Turn
//! records are read through [`LiveRecords`] on every tick while a turn is held, so a turn's end is
//! seen from its record whether or not anything else said so.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::tab_set::LiveEdit;

use super::lifecycle::{monotonic_ns, ReviewError, TurnReview, TurnState};
use super::live_contest::TurnKey;
use super::live_job::{LiveHunks, LiveRecord, LiveSettle, LiveSkip};
use super::revert::ExactHunk;

/// Paths asked about in one shown question.
pub const MAX_SHOWN_PATHS: usize = 256;
/// Live and settle jobs out at once, so the panel's own redraws always find a worker slot.
pub const MAX_LIVE_JOBS: usize = 2;
/// How long an ended turn may wait for its end snapshot before its drawings are cleared: the
/// snapshot's own time limit and some slack.
pub const LIVE_SETTLE_WAIT: Duration = Duration::from_secs(15);
/// A path answered "not shown" is asked once more this long after its edit, unless a follow run
/// for it ended first: following opens the file a little after the edit lands.
pub const FOLLOW_REASK: Duration = Duration::from_secs(1);
/// How long a turn whose record has not appeared is kept waiting for it: an edit can be drained
/// before the review has made its turn's record.
pub const RECORD_WAIT: Duration = Duration::from_secs(5);
/// Settled turns remembered, so a late edit of one is only noted and never settles it twice.
const MAX_FINISHED_TURNS: usize = 256;

/// What the window does next for its live drawings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveStep {
    /// Ask the editor which of these paths have a loaded buffer in a non-floating window; answer
    /// with [`LiveDraws::shown`]. At most [`MAX_SHOWN_PATHS`]; one question is out at a time.
    AskShown { paths: Vec<PathBuf> },
    /// Tell window 1's contested-paths table that this turn named these paths; `ended_ns` once it
    /// has ended (the times are the records' `CLOCK_MONOTONIC` ones).
    Note {
        key: TurnKey,
        started_ns: u64,
        ended_ns: Option<u64>,
        paths: Vec<PathBuf>,
    },
    /// Start a live hunks job for the path; answer with [`LiveDraws::live_done`].
    RunLive { key: TurnKey, tab: u64, path: PathBuf },
    /// Start a settle job for the path at turn number `turn`; answer with [`LiveDraws::settle_done`].
    RunSettle {
        key: TurnKey,
        tab: u64,
        turn: u32,
        path: PathBuf,
    },
    /// Draw these hunks over the path's buffer. `running` while the turn runs (no revert);
    /// a settled draw carries the hunks' own headers and the end snapshot's line count. `turn` is
    /// the record's turn number, 0 in a running draw while the session's numbering is not known.
    Draw {
        path: PathBuf,
        key: TurnKey,
        tab: u64,
        turn: u32,
        running: bool,
        read_lines: u64,
        hunks: Vec<ExactHunk>,
        headers: BTreeMap<u32, String>,
    },
    /// Take the path's live drawing off, if it still has one.
    Clear { path: PathBuf },
}

/// Where the turn records come from; [`TurnReview`] in the window, a table in tests.
pub trait LiveRecords {
    fn live_record(&self, session: &str, turn_id: &str) -> Option<LiveRecord>;
}

impl LiveRecords for TurnReview {
    fn live_record(&self, session: &str, turn_id: &str) -> Option<LiveRecord> {
        TurnReview::live_record(self, session, turn_id)
    }
}

/// Where one turn's file stands on its way to a live draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiveState {
    /// Goes in the next shown question.
    Ask,
    /// In the shown question out now.
    Asked,
    /// Answered "not shown"; asked once more at `at`.
    NotShown { at: Instant },
    /// Shown: its job starts when a slot is free.
    Ready,
    /// Its job is out; `dirty` when an edit came after the job started, so one more runs after it.
    Out { dirty: bool },
}

struct LiveItem {
    state: LiveState,
    /// When the edit that put the path here came.
    since: Instant,
    /// The one extra question was asked.
    reasked: bool,
    /// A follow run for the path ended while its question was out.
    followed: bool,
}

/// Where one turn's file stands in its settle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettleState {
    Ask,
    Asked,
    Ready,
    Out,
    Done,
}

enum Phase {
    /// The record says the turn runs.
    Running,
    /// The record says it ended; the end snapshot is awaited since `since`.
    Ended { since: Instant },
    /// Settling from the snapshots as turn number `turn`: live results are dropped from here on.
    Settling { turn: u32 },
}

struct Turn {
    key: TurnKey,
    tab: u64,
    /// When its first edit came; a turn whose record has not appeared waits [`RECORD_WAIT`].
    created: Instant,
    /// The record's turn number; 0 while the numbering is not known.
    n: u32,
    started_ns: Option<u64>,
    ended_ns: Option<u64>,
    /// Every path the turn's edits named.
    paths: BTreeSet<PathBuf>,
    /// Paths named since the contested-paths table was last told.
    unnoted: BTreeSet<PathBuf>,
    end_noted: bool,
    phase: Phase,
    live: BTreeMap<PathBuf, LiveItem>,
    settle: BTreeMap<PathBuf, SettleState>,
}

impl Turn {
    fn new(key: TurnKey, tab: u64, now: Instant) -> Turn {
        Turn {
            key,
            tab,
            created: now,
            n: 0,
            started_ns: None,
            ended_ns: None,
            paths: BTreeSet::new(),
            unnoted: BTreeSet::new(),
            end_noted: false,
            phase: Phase::Running,
            live: BTreeMap::new(),
            settle: BTreeMap::new(),
        }
    }

    fn seen(&mut self, record: &LiveRecord) {
        self.n = record.n;
        self.started_ns = Some(record.started_ns);
        if record.ended_ns.is_some() {
            self.ended_ns = record.ended_ns;
        }
    }
}

/// A turn whose drawings were settled, kept so a late edit of it is only noted.
struct Finished {
    key: TurnKey,
    started_ns: u64,
    ended_ns: u64,
}

/// One window's live drawings.
pub struct LiveDraws {
    /// Turns that ran or still settle, in the order they were first seen.
    turns: Vec<Turn>,
    finished: VecDeque<Finished>,
    /// The turn that named each path last: only it draws or clears the path, so an older turn's
    /// late answer or settle never replaces a newer turn's drawing. An edit drained before its
    /// turn was seen ended takes the path; a later one only takes a path nobody holds. A finished
    /// turn keeps its paths while another held turn names them, so that turn cannot take them back.
    owner: BTreeMap<PathBuf, TurnKey>,
    /// The paths this window drew live, with the turn and tab that drew them.
    drawn: BTreeMap<PathBuf, (TurnKey, u64)>,
    /// A shown question is out.
    asking: bool,
    /// Steps decided between ticks, given out by the next [`steps`](Self::steps).
    out: Vec<LiveStep>,
    /// `CLOCK_MONOTONIC` in nanoseconds, for a turn whose tab went before its record ended.
    clock: fn() -> u64,
}

impl Default for LiveDraws {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveDraws {
    pub fn new() -> LiveDraws {
        LiveDraws {
            turns: Vec::new(),
            finished: VecDeque::new(),
            owner: BTreeMap::new(),
            drawn: BTreeMap::new(),
            asking: false,
            out: Vec::new(),
            clock: monotonic_ns,
        }
    }

    /// Takes the edits drained from the tabs. An edit of the tab's running turn is asked about and
    /// drawn; one of a turn that already ended is only noted, and its settle covers it.
    pub fn edits(&mut self, edits: Vec<LiveEdit>, records: &dyn LiveRecords, now: Instant) {
        for edit in edits {
            let key = TurnKey {
                session: edit.session,
                turn_id: edit.turn_id,
            };
            let path = edit.path;
            if let Some(done) = self.finished.iter().find(|done| done.key == key) {
                self.out.push(LiveStep::Note {
                    key,
                    started_ns: done.started_ns,
                    ended_ns: Some(done.ended_ns),
                    paths: vec![path],
                });
                continue;
            }
            let record = records.live_record(&key.session, &key.turn_id);
            let index = match self.turns.iter().position(|turn| turn.key == key) {
                Some(index) => index,
                None => {
                    self.turns.push(Turn::new(key.clone(), edit.tab.0, now));
                    self.turns.len() - 1
                }
            };
            let turn = &mut self.turns[index];
            turn.paths.insert(path.clone());
            turn.unnoted.insert(path.clone());
            // Not known yet, or gone: the record's first sight, or its absence, decides on the tick.
            let Some(record) = record else {
                continue;
            };
            if turn.started_ns.is_none() {
                // Seen for the first time here: what it named before is taken up on the tick.
                continue;
            }
            turn.seen(&record);
            if !matches!(turn.phase, Phase::Running) {
                // Its end was already seen: only the settle covers this edit.
                self.owner.entry(path.clone()).or_insert_with(|| key.clone());
                if matches!(turn.phase, Phase::Settling { .. }) && self.owner.get(&path) == Some(&key) {
                    turn.settle.entry(path).or_insert(SettleState::Ask);
                }
                continue;
            }
            if record.ended {
                // Drained before its end was seen: the last edit often comes in the batch that ends it.
                if self.takes(&path, &key, record.ended_ns) {
                    self.owner.insert(path, key);
                }
                continue;
            }
            self.owner.insert(path.clone(), key);
            let turn = &mut self.turns[index];
            match turn.live.get_mut(&path) {
                Some(LiveItem {
                    state: LiveState::Out { dirty },
                    ..
                }) => *dirty = true,
                Some(LiveItem {
                    state: LiveState::Ask | LiveState::Asked | LiveState::Ready,
                    ..
                }) => {}
                Some(LiveItem {
                    state: LiveState::NotShown { .. },
                    ..
                })
                | None => {
                    turn.live.insert(
                        path,
                        LiveItem {
                            state: LiveState::Ask,
                            since: now,
                            reasked: false,
                            followed: false,
                        },
                    );
                }
            }
        }
    }

    /// The editor's answer to the last [`LiveStep::AskShown`]: `shown[i]` for `asked[i]`; a
    /// missing answer counts as "not shown".
    pub fn shown(&mut self, asked: Vec<PathBuf>, shown: Vec<bool>, now: Instant) {
        self.asking = false;
        for (index, path) in asked.iter().enumerate() {
            let is_shown = shown.get(index).copied().unwrap_or(false);
            for turn in &mut self.turns {
                if let Some(item) = turn.live.get_mut(path) {
                    if item.state == LiveState::Asked {
                        if is_shown {
                            item.state = LiveState::Ready;
                        } else if item.reasked {
                            turn.live.remove(path);
                        } else {
                            let at = if item.followed { now } else { item.since + FOLLOW_REASK };
                            item.state = LiveState::NotShown { at };
                        }
                    }
                }
                if let Some(state) = turn.settle.get_mut(path) {
                    if *state == SettleState::Asked {
                        if is_shown {
                            *state = SettleState::Ready;
                        } else {
                            // A loaded buffer in no window: its drawing must not stay "running".
                            *state = SettleState::Done;
                            if self.owner.get(path) == Some(&turn.key) {
                                clear_drawn(&mut self.drawn, &mut self.out, path);
                            }
                        }
                    }
                }
            }
        }
        // A path left out of the answer is asked again.
        for turn in &mut self.turns {
            for item in turn.live.values_mut() {
                if item.state == LiveState::Asked {
                    item.state = LiveState::Ask;
                }
            }
            for state in turn.settle.values_mut() {
                if *state == SettleState::Asked {
                    *state = SettleState::Ask;
                }
            }
        }
    }

    /// A follow run of `tab` that opened `path` ended: a path it was answered "not shown" for is
    /// asked once more now.
    pub fn follow_ended(&mut self, tab: u64, path: &Path, now: Instant) {
        for turn in self.turns.iter_mut().filter(|turn| turn.tab == tab) {
            let Some(item) = turn.live.get_mut(path) else {
                continue;
            };
            match item.state {
                LiveState::NotShown { .. } => item.state = LiveState::NotShown { at: now },
                LiveState::Asked => item.followed = true,
                _ => {}
            }
        }
    }

    /// A live hunks job's answer. Dropped once the turn's settle was queued, when a newer turn
    /// named the path, or when nothing waits for it (the tab went, the editor was lost).
    pub fn live_done(&mut self, key: &TurnKey, path: &Path, result: Result<Result<LiveHunks, LiveSkip>, ReviewError>) {
        let Some(turn) = self.turns.iter_mut().find(|turn| turn.key == *key) else {
            return;
        };
        if matches!(turn.phase, Phase::Settling { .. }) {
            return;
        }
        let Some(item) = turn.live.get_mut(path) else {
            return;
        };
        let LiveState::Out { dirty } = item.state else {
            return;
        };
        let owns = self.owner.get(path) == Some(key);
        if dirty && owns && matches!(turn.phase, Phase::Running) {
            item.state = LiveState::Ready;
        } else {
            turn.live.remove(path);
        }
        if !owns {
            return;
        }
        match result {
            Ok(Ok(live)) if !live.hunks.is_empty() => {
                self.drawn.insert(path.to_path_buf(), (key.clone(), turn.tab));
                self.out.push(LiveStep::Draw {
                    path: path.to_path_buf(),
                    key: key.clone(),
                    tab: turn.tab,
                    turn: turn.n,
                    running: true,
                    read_lines: live.read_lines,
                    hunks: live.hunks,
                    headers: BTreeMap::new(),
                });
            }
            // Nothing to show as this turn's change now: an older drawing would be wrong.
            Ok(_) => clear_drawn(&mut self.drawn, &mut self.out, path),
            // The store could not be read this time; the next edit or the settle decides.
            Err(_) => {}
        }
    }

    /// A settle job's answer: drawn as the finished turn's exact hunks, or cleared.
    pub fn settle_done(&mut self, key: &TurnKey, path: &Path, result: Result<LiveSettle, ReviewError>) {
        let Some(turn) = self.turns.iter_mut().find(|turn| turn.key == *key) else {
            return;
        };
        let Phase::Settling { turn: n } = turn.phase else {
            return;
        };
        let Some(state) = turn.settle.get_mut(path) else {
            return;
        };
        if *state != SettleState::Out {
            return;
        }
        *state = SettleState::Done;
        if self.owner.get(path) != Some(key) {
            return;
        }
        let hunks = match result {
            Ok(settle) if !settle.exact.binary => settle
                .exact
                .hunks
                .filter(|hunks| !hunks.is_empty())
                .map(|hunks| (hunks, settle.end_lines)),
            _ => None,
        };
        match hunks {
            Some((hunks, end_lines)) => {
                let headers = hunks.iter().map(|hunk| (hunk.id, hunk.header.clone())).collect();
                self.drawn.insert(path.to_path_buf(), (key.clone(), turn.tab));
                self.out.push(LiveStep::Draw {
                    path: path.to_path_buf(),
                    key: key.clone(),
                    tab: turn.tab,
                    turn: n,
                    running: false,
                    read_lines: end_lines,
                    hunks,
                    headers,
                });
            }
            None => clear_drawn(&mut self.drawn, &mut self.out, path),
        }
    }

    /// The tab closed, restarted or changed session: its running turn ends for the contested-paths
    /// table, its jobs' answers are dropped, and every drawing it made is cleared.
    pub fn tab_gone(&mut self, tab: u64) {
        let (gone, kept): (Vec<Turn>, Vec<Turn>) = std::mem::take(&mut self.turns)
            .into_iter()
            .partition(|turn| turn.tab == tab);
        self.turns = kept;
        for turn in gone {
            self.finish(turn);
        }
        self.out
            .retain(|step| !matches!(step, LiveStep::Draw { tab: drawn_by, .. } if *drawn_by == tab));
        let paths: Vec<PathBuf> = self
            .drawn
            .iter()
            .filter(|(_, (_, drawn_by))| *drawn_by == tab)
            .map(|(path, _)| path.clone())
            .collect();
        for path in paths {
            clear_drawn(&mut self.drawn, &mut self.out, &path);
        }
    }

    /// The window's editor went (nvim restarted, the companion detached, teardown): its drawings
    /// went with it, so every job and question for it is dropped. Turns are still followed, and
    /// told to the contested-paths table, until they end.
    pub fn editor_lost(&mut self) {
        self.asking = false;
        self.drawn.clear();
        self.out.retain(|step| matches!(step, LiveStep::Note { .. }));
        for turn in &mut self.turns {
            turn.live.clear();
            for state in turn.settle.values_mut() {
                *state = SettleState::Done;
            }
        }
    }

    /// The module took the path's drawing off (keep, close, reload, unload).
    pub fn off(&mut self, path: &Path) {
        self.drawn.remove(path);
    }

    /// What to do now. `live_jobs_out` is the number of live hunks and settle jobs the caller has
    /// out; new ones start only while it is under [`MAX_LIVE_JOBS`].
    pub fn steps(&mut self, records: &dyn LiveRecords, live_jobs_out: usize, now: Instant) -> Vec<LiveStep> {
        let mut index = 0;
        while index < self.turns.len() {
            if self.advance(index, records, now) {
                let turn = self.turns.remove(index);
                self.finish(turn);
            } else {
                index += 1;
            }
        }
        let mut steps = std::mem::take(&mut self.out);
        self.ask(&mut steps, now);
        self.run(&mut steps, live_jobs_out);
        steps
    }

    /// True while a turn is followed or a step waits to be given out.
    pub fn wants_ticks(&self) -> bool {
        !self.turns.is_empty() || !self.out.is_empty() || self.asking
    }

    /// Reads the turn's record and moves it on; true when the turn is done.
    fn advance(&mut self, index: usize, records: &dyn LiveRecords, now: Instant) -> bool {
        let turn = &mut self.turns[index];
        let Some(record) = records.live_record(&turn.key.session, &turn.key.turn_id) else {
            if turn.started_ns.is_none() {
                // Its record has not appeared; nothing was drawn or held for it meanwhile.
                return now.saturating_duration_since(turn.created) >= RECORD_WAIT;
            }
            // The tab's session went: nothing can be drawn as this turn's any more.
            self.clear_turn(index);
            return true;
        };
        let first_sight = turn.started_ns.is_none();
        turn.seen(&record);
        if first_sight && matches!(turn.phase, Phase::Running) {
            // Every path it named was drained before its end was seen.
            let key = turn.key.clone();
            let paths: Vec<PathBuf> = turn.paths.iter().cloned().collect();
            for path in paths {
                if record.ended {
                    if self.takes(&path, &key, record.ended_ns) {
                        self.owner.insert(path, key.clone());
                    }
                    continue;
                }
                self.owner.insert(path.clone(), key.clone());
                self.turns[index].live.entry(path).or_insert(LiveItem {
                    state: LiveState::Ask,
                    since: now,
                    reasked: false,
                    followed: false,
                });
            }
        }
        let turn = &mut self.turns[index];
        if record.ended && matches!(turn.phase, Phase::Running) {
            turn.phase = Phase::Ended { since: now };
            // The settle covers every path; a live answer still out may draw until it is queued.
            turn.live.retain(|_, item| matches!(item.state, LiveState::Out { .. }));
            let ended_ns = turn.ended_ns.unwrap_or_else(self.clock);
            turn.ended_ns = Some(ended_ns);
            turn.end_noted = true;
            turn.unnoted.clear();
            self.out.push(LiveStep::Note {
                key: turn.key.clone(),
                started_ns: record.started_ns,
                ended_ns: Some(ended_ns),
                paths: turn.paths.iter().cloned().collect(),
            });
        }
        if !turn.unnoted.is_empty() {
            let paths = std::mem::take(&mut turn.unnoted).into_iter().collect();
            self.out.push(LiveStep::Note {
                key: turn.key.clone(),
                started_ns: record.started_ns,
                ended_ns: turn.end_noted.then_some(turn.ended_ns).flatten(),
                paths,
            });
        }
        match turn.phase {
            Phase::Running => false,
            Phase::Ended { since } => {
                let unusable =
                    record.late || matches!(record.state, TurnState::NoBaseline(_) | TurnState::Unfinished(_));
                if unusable {
                    self.clear_turn(index);
                    return true;
                }
                if record.state == TurnState::Ok && record.n > 0 {
                    turn.phase = Phase::Settling { turn: record.n };
                    turn.live.clear();
                    for path in &turn.paths {
                        let owner = self.owner.entry(path.clone()).or_insert_with(|| turn.key.clone());
                        if *owner == turn.key {
                            turn.settle.insert(path.clone(), SettleState::Ask);
                        } else if self.drawn.get(path).is_some_and(|(drawn_by, _)| *drawn_by == turn.key) {
                            // Its own running drawing of a path a newer turn holds must not stay.
                            clear_drawn(&mut self.drawn, &mut self.out, path);
                        }
                    }
                    return turn.settle.is_empty();
                }
                // Still pending, or its numbering is not known yet.
                if now.saturating_duration_since(since) >= LIVE_SETTLE_WAIT {
                    self.clear_turn(index);
                    return true;
                }
                false
            }
            Phase::Settling { .. } => turn.settle.values().all(|state| *state == SettleState::Done),
        }
    }

    /// Whether a turn that ended at `ended_ns`, whose edit of `path` was drained before its end was
    /// seen, takes the path: not from a turn that started after it ended, whose edits are newer.
    fn takes(&self, path: &Path, key: &TurnKey, ended_ns: Option<u64>) -> bool {
        let Some(owner) = self.owner.get(path) else {
            return true;
        };
        if owner == key {
            return true;
        }
        let owner_started = self
            .turns
            .iter()
            .find(|turn| turn.key == *owner)
            .and_then(|turn| turn.started_ns)
            .or_else(|| {
                self.finished
                    .iter()
                    .find(|done| done.key == *owner)
                    .map(|done| done.started_ns)
            });
        match (owner_started, ended_ns) {
            (Some(started), Some(ended)) => started <= ended,
            _ => true,
        }
    }

    /// Clears every drawing of a path this turn owns or drew: the turn has nothing that may be drawn.
    fn clear_turn(&mut self, index: usize) {
        let turn = &self.turns[index];
        for path in &turn.paths {
            let drew = self.drawn.get(path).is_some_and(|(drawn_by, _)| *drawn_by == turn.key);
            if drew || self.owner.get(path) == Some(&turn.key) {
                clear_drawn(&mut self.drawn, &mut self.out, path);
            }
        }
    }

    /// Forgets a turn: its end, and any path named after it was told, go to the contested-paths
    /// table. A path no held turn names stops being anyone's.
    fn finish(&mut self, turn: Turn) {
        let turns = &self.turns;
        self.owner
            .retain(|path, _| turns.iter().any(|held| held.paths.contains(path)));
        // A turn whose record never appeared has nothing to tell, and a later edit may still find it.
        let Some(started_ns) = turn.started_ns else {
            return;
        };
        let ended_ns = turn.ended_ns.unwrap_or_else(self.clock);
        let paths: Vec<PathBuf> = if turn.end_noted {
            turn.unnoted.into_iter().collect()
        } else {
            turn.paths.into_iter().collect()
        };
        if !turn.end_noted || !paths.is_empty() {
            self.out.push(LiveStep::Note {
                key: turn.key.clone(),
                started_ns,
                ended_ns: Some(ended_ns),
                paths,
            });
        }
        if self.finished.len() == MAX_FINISHED_TURNS {
            self.finished.pop_front();
        }
        self.finished.push_back(Finished {
            key: turn.key,
            started_ns,
            ended_ns,
        });
    }

    /// One shown question for the paths waiting for one, unless one is out.
    fn ask(&mut self, steps: &mut Vec<LiveStep>, now: Instant) {
        for turn in &mut self.turns {
            for item in turn.live.values_mut() {
                if let LiveState::NotShown { at } = item.state {
                    if now >= at {
                        item.state = LiveState::Ask;
                        item.reasked = true;
                    }
                }
            }
        }
        if self.asking {
            return;
        }
        let mut paths: Vec<PathBuf> = Vec::new();
        let mut chosen: BTreeSet<PathBuf> = BTreeSet::new();
        for turn in &self.turns {
            let settle = turn.settle.iter().filter(|(_, state)| **state == SettleState::Ask);
            let live = turn.live.iter().filter(|(_, item)| item.state == LiveState::Ask);
            for path in settle.map(|(path, _)| path).chain(live.map(|(path, _)| path)) {
                if chosen.len() < MAX_SHOWN_PATHS && chosen.insert(path.clone()) {
                    paths.push(path.clone());
                }
            }
        }
        if paths.is_empty() {
            return;
        }
        for turn in &mut self.turns {
            for (path, state) in turn.settle.iter_mut() {
                if *state == SettleState::Ask && chosen.contains(path) {
                    *state = SettleState::Asked;
                }
            }
            for (path, item) in turn.live.iter_mut() {
                if item.state == LiveState::Ask && chosen.contains(path) {
                    item.state = LiveState::Asked;
                }
            }
        }
        self.asking = true;
        steps.push(LiveStep::AskShown { paths });
    }

    /// Starts jobs while slots are free: settles first, since they end their turns.
    fn run(&mut self, steps: &mut Vec<LiveStep>, live_jobs_out: usize) {
        let mut free = MAX_LIVE_JOBS.saturating_sub(live_jobs_out);
        for turn in &mut self.turns {
            let Phase::Settling { turn: n } = turn.phase else {
                continue;
            };
            for (path, state) in turn.settle.iter_mut() {
                if free == 0 {
                    return;
                }
                if *state == SettleState::Ready && self.owner.get(path) != Some(&turn.key) {
                    // A newer turn named the path since: its settle decides.
                    *state = SettleState::Done;
                } else if *state == SettleState::Ready {
                    *state = SettleState::Out;
                    free -= 1;
                    steps.push(LiveStep::RunSettle {
                        key: turn.key.clone(),
                        tab: turn.tab,
                        turn: n,
                        path: path.clone(),
                    });
                }
            }
        }
        for turn in &mut self.turns {
            if !matches!(turn.phase, Phase::Running) {
                continue;
            }
            for (path, item) in turn.live.iter_mut() {
                if free == 0 {
                    return;
                }
                if item.state == LiveState::Ready {
                    item.state = LiveState::Out { dirty: false };
                    free -= 1;
                    steps.push(LiveStep::RunLive {
                        key: turn.key.clone(),
                        tab: turn.tab,
                        path: path.clone(),
                    });
                }
            }
        }
    }
}

/// Queues a `Clear` for the path when this window drew it.
fn clear_drawn(drawn: &mut BTreeMap<PathBuf, (TurnKey, u64)>, out: &mut Vec<LiveStep>, path: &Path) {
    if drawn.remove(path).is_some() {
        out.push(LiveStep::Clear {
            path: path.to_path_buf(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tabs::TabId;
    use crate::turn_review::revert::ExactHunks;

    const STARTED: u64 = 100;
    const ENDED: u64 = 200;
    const GONE_AT: u64 = 999;

    #[derive(Default)]
    struct Records(BTreeMap<(String, String), LiveRecord>);

    impl LiveRecords for Records {
        fn live_record(&self, session: &str, turn_id: &str) -> Option<LiveRecord> {
            self.0.get(&(session.to_owned(), turn_id.to_owned())).cloned()
        }
    }

    impl Records {
        fn running(&mut self, tab: u64, session: &str, turn_id: &str) {
            self.0.insert(
                (session.to_owned(), turn_id.to_owned()),
                LiveRecord {
                    tab,
                    n: 3,
                    started_ns: STARTED,
                    ended_ns: None,
                    ended: false,
                    state: TurnState::Pending,
                    late: false,
                },
            );
        }

        fn end(&mut self, session: &str, turn_id: &str, state: TurnState) {
            let record = self.0.get_mut(&(session.to_owned(), turn_id.to_owned())).unwrap();
            record.ended = true;
            record.ended_ns = Some(ENDED);
            record.state = state;
        }

        fn record(&mut self, session: &str, turn_id: &str) -> &mut LiveRecord {
            self.0.get_mut(&(session.to_owned(), turn_id.to_owned())).unwrap()
        }
    }

    fn fresh() -> LiveDraws {
        let mut draws = LiveDraws::new();
        draws.clock = || GONE_AT;
        draws
    }

    fn key(session: &str, turn_id: &str) -> TurnKey {
        TurnKey {
            session: session.into(),
            turn_id: turn_id.into(),
        }
    }

    fn edit(tab: u64, session: &str, turn_id: &str, path: &str) -> LiveEdit {
        LiveEdit {
            tab: TabId(tab),
            session: session.into(),
            turn_id: turn_id.into(),
            path: PathBuf::from(path),
        }
    }

    fn p(path: &str) -> PathBuf {
        PathBuf::from(path)
    }

    fn hunk(id: u32) -> ExactHunk {
        ExactHunk {
            id,
            header: format!("@@ -{id} +{id} @@ fn f{id}"),
            old_start: id,
            old_len: 1,
            new_start: id,
            new_len: 1,
            old_lines: vec![b"old".to_vec()],
            new_lines: vec![b"new".to_vec()],
        }
    }

    fn live_ok(path: &str, read_lines: u64) -> Result<Result<LiveHunks, LiveSkip>, ReviewError> {
        Ok(Ok(LiveHunks {
            path: path.into(),
            hunks: vec![hunk(1)],
            read_lines,
        }))
    }

    fn settle_ok(path: &str, ids: &[u32], end_lines: u64) -> Result<LiveSettle, ReviewError> {
        Ok(LiveSettle {
            exact: ExactHunks {
                path: path.into(),
                binary: false,
                hunks: Some(ids.iter().map(|&id| hunk(id)).collect()),
            },
            end_lines,
        })
    }

    fn asks(steps: &[LiveStep]) -> Vec<Vec<PathBuf>> {
        steps
            .iter()
            .filter_map(|s| match s {
                LiveStep::AskShown { paths } => Some(paths.clone()),
                _ => None,
            })
            .collect()
    }

    fn runs(steps: &[LiveStep]) -> Vec<PathBuf> {
        steps
            .iter()
            .filter_map(|s| match s {
                LiveStep::RunLive { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect()
    }

    fn settles(steps: &[LiveStep]) -> Vec<(PathBuf, u32)> {
        steps
            .iter()
            .filter_map(|s| match s {
                LiveStep::RunSettle { path, turn, .. } => Some((path.clone(), *turn)),
                _ => None,
            })
            .collect()
    }

    fn drawn(steps: &[LiveStep]) -> Vec<(PathBuf, bool)> {
        steps
            .iter()
            .filter_map(|s| match s {
                LiveStep::Draw { path, running, .. } => Some((path.clone(), *running)),
                _ => None,
            })
            .collect()
    }

    fn clears(steps: &[LiveStep]) -> Vec<PathBuf> {
        steps
            .iter()
            .filter_map(|s| match s {
                LiveStep::Clear { path } => Some(path.clone()),
                _ => None,
            })
            .collect()
    }

    fn notes(steps: &[LiveStep]) -> Vec<(TurnKey, Option<u64>, Vec<PathBuf>)> {
        steps
            .iter()
            .filter_map(|s| match s {
                LiveStep::Note {
                    key, ended_ns, paths, ..
                } => Some((key.clone(), *ended_ns, paths.clone())),
                _ => None,
            })
            .collect()
    }

    /// Edits `path` in a running turn and walks it to a drawn live drawing.
    fn draw_live(draws: &mut LiveDraws, records: &Records, tab: u64, turn_id: &str, path: &str, now: Instant) {
        draws.edits(vec![edit(tab, "s", turn_id, path)], records, now);
        let steps = draws.steps(records, 0, now);
        assert_eq!(
            asks(&steps),
            vec![vec![p(path)]],
            "an edit asks whether its file is shown"
        );
        draws.shown(vec![p(path)], vec![true], now);
        let steps = draws.steps(records, 0, now);
        assert_eq!(runs(&steps), vec![p(path)], "a shown file gets a live job");
        draws.live_done(&key("s", turn_id), &p(path), live_ok(path, 10));
        let steps = draws.steps(records, 0, now);
        assert_eq!(
            drawn(&steps),
            vec![(p(path), true)],
            "the job's hunks are drawn as running"
        );
    }

    #[test]
    fn one_job_per_path_and_one_dirty() {
        let now = Instant::now();
        let mut records = Records::default();
        records.running(1, "s", "t1");
        let mut draws = fresh();
        draws.edits(vec![edit(1, "s", "t1", "a.rs")], &records, now);
        let steps = draws.steps(&records, 0, now);
        assert_eq!(asks(&steps), vec![vec![p("a.rs")]]);
        draws.shown(vec![p("a.rs")], vec![true], now);
        let steps = draws.steps(&records, 0, now);
        assert_eq!(runs(&steps), vec![p("a.rs")]);

        // Two more edits while the job is out: no second job, no new question.
        draws.edits(
            vec![edit(1, "s", "t1", "a.rs"), edit(1, "s", "t1", "a.rs")],
            &records,
            now,
        );
        let steps = draws.steps(&records, 1, now);
        assert!(runs(&steps).is_empty(), "one job per path: {steps:?}");
        assert!(
            asks(&steps).is_empty(),
            "a path with a job out is not asked again: {steps:?}"
        );

        // Its answer is drawn, and exactly one more job runs for the edits that came meanwhile.
        draws.live_done(&key("s", "t1"), &p("a.rs"), live_ok("a.rs", 10));
        let steps = draws.steps(&records, 0, now);
        assert_eq!(drawn(&steps), vec![(p("a.rs"), true)]);
        assert_eq!(runs(&steps), vec![p("a.rs")], "the dirty path runs once more");

        draws.live_done(&key("s", "t1"), &p("a.rs"), live_ok("a.rs", 11));
        let steps = draws.steps(&records, 0, now);
        assert_eq!(drawn(&steps), vec![(p("a.rs"), true)]);
        assert!(runs(&steps).is_empty(), "no edit came during the second job: {steps:?}");
    }

    #[test]
    fn a_fifth_edit_waits_dirty_never_dropped() {
        let now = Instant::now();
        let mut records = Records::default();
        records.running(1, "s", "t1");
        let mut draws = fresh();
        let names = ["a", "b", "c", "d", "e"];
        draws.edits(names.iter().map(|n| edit(1, "s", "t1", n)).collect(), &records, now);
        let steps = draws.steps(&records, 0, now);
        let asked = asks(&steps);
        assert_eq!(asked.len(), 1, "one question for every path: {steps:?}");
        draws.shown(asked[0].clone(), vec![true; asked[0].len()], now);

        let steps = draws.steps(&records, 0, now);
        let mut ran = runs(&steps);
        assert_eq!(
            ran.len(),
            MAX_LIVE_JOBS,
            "live jobs never take more than their share: {steps:?}"
        );
        let out = ran.len();

        // The limit is full: nothing more starts, and an edit to a waiting path is not lost.
        let steps = draws.steps(&records, out, now);
        assert!(runs(&steps).is_empty(), "{steps:?}");
        draws.edits(vec![edit(1, "s", "t1", "e")], &records, now);
        // An edit to a path whose job is out marks it dirty.
        draws.edits(vec![edit(1, "s", "t1", "a")], &records, now);
        let steps = draws.steps(&records, out, now);
        assert!(runs(&steps).is_empty(), "{steps:?}");

        // Each finished job frees one slot; every waiting path runs, and the dirty one runs again.
        let mut out_now: VecDeque<PathBuf> = ran.iter().cloned().collect();
        while let Some(done) = out_now.pop_front() {
            assert!(ran.len() < 20, "the jobs never drained: {ran:?}");
            draws.live_done(&key("s", "t1"), &done, live_ok(done.to_str().unwrap(), 1));
            let steps = draws.steps(&records, out_now.len(), now);
            let started = runs(&steps);
            assert!(out_now.len() + started.len() <= MAX_LIVE_JOBS, "{steps:?}");
            ran.extend(started.iter().cloned());
            out_now.extend(started);
        }
        ran.sort();
        assert_eq!(ran, vec![p("a"), p("a"), p("b"), p("c"), p("d"), p("e")]);
    }

    #[test]
    fn an_edit_of_an_ended_turn_is_noted_and_not_run() {
        let now = Instant::now();
        let mut records = Records::default();
        records.running(1, "s", "t1");
        records.end("s", "t1", TurnState::Pending);
        let mut draws = fresh();
        draws.edits(vec![edit(1, "s", "t1", "a.rs")], &records, now);
        let steps = draws.steps(&records, 0, now);
        assert_eq!(notes(&steps), vec![(key("s", "t1"), Some(ENDED), vec![p("a.rs")])]);
        assert!(asks(&steps).is_empty(), "{steps:?}");
        assert!(runs(&steps).is_empty(), "{steps:?}");
    }

    #[test]
    fn a_live_result_after_the_settle_is_dropped() {
        let now = Instant::now();
        let mut records = Records::default();
        records.running(1, "s", "t1");
        let mut draws = fresh();
        draw_live(&mut draws, &records, 1, "t1", "a.rs", now);
        draws.edits(vec![edit(1, "s", "t1", "a.rs")], &records, now);
        draws.steps(&records, 0, now);
        draws.shown(vec![p("a.rs")], vec![true], now);
        assert_eq!(runs(&draws.steps(&records, 0, now)), vec![p("a.rs")]);

        records.end("s", "t1", TurnState::Ok);
        let steps = draws.steps(&records, 1, now);
        assert_eq!(asks(&steps), vec![vec![p("a.rs")]], "the settle asks about its paths");

        draws.live_done(&key("s", "t1"), &p("a.rs"), live_ok("a.rs", 10));
        let steps = draws.steps(&records, 0, now);
        assert!(
            drawn(&steps).is_empty(),
            "a live result after the settle was queued is dropped: {steps:?}"
        );
        assert!(runs(&steps).is_empty(), "{steps:?}");
    }

    #[test]
    fn settle_draws_exact_for_an_ok_turn_and_clears_otherwise() {
        // Ok: drawn again from the snapshots, as a finished turn, with its headers and the end's lines.
        let now = Instant::now();
        let mut records = Records::default();
        records.running(1, "s", "t1");
        let mut draws = fresh();
        draw_live(&mut draws, &records, 1, "t1", "a.rs", now);
        records.end("s", "t1", TurnState::Ok);
        records.record("s", "t1").n = 7;
        let steps = draws.steps(&records, 0, now);
        assert_eq!(asks(&steps), vec![vec![p("a.rs")]]);
        draws.shown(vec![p("a.rs")], vec![true], now);
        let steps = draws.steps(&records, 0, now);
        assert_eq!(settles(&steps), vec![(p("a.rs"), 7)]);
        draws.settle_done(&key("s", "t1"), &p("a.rs"), settle_ok("a.rs", &[1, 2], 42));
        let steps = draws.steps(&records, 0, now);
        let draw = steps.iter().find(|s| matches!(s, LiveStep::Draw { .. }));
        let Some(LiveStep::Draw {
            path,
            key: drawn_key,
            tab,
            turn,
            running,
            read_lines,
            hunks,
            headers,
        }) = draw
        else {
            panic!("an Ok turn's settle draws: {steps:?}");
        };
        assert_eq!(
            (path, drawn_key, *tab, *turn, *running, *read_lines),
            (&p("a.rs"), &key("s", "t1"), 1, 7, false, 42)
        );
        assert_eq!(hunks.len(), 2);
        assert_eq!(headers.get(&2), Some(&hunk(2).header));
        assert!(clears(&steps).is_empty());
        assert!(!draws.wants_ticks(), "a settled turn leaves nothing to tick for");

        // Ok, but the settle's job finds nothing to show: cleared.
        let mut records = Records::default();
        records.running(1, "s", "t1");
        let mut draws = fresh();
        draw_live(&mut draws, &records, 1, "t1", "a.rs", now);
        records.end("s", "t1", TurnState::Ok);
        draws.steps(&records, 0, now);
        draws.shown(vec![p("a.rs")], vec![true], now);
        draws.steps(&records, 0, now);
        draws.settle_done(&key("s", "t1"), &p("a.rs"), settle_ok("a.rs", &[], 42));
        assert_eq!(
            clears(&draws.steps(&records, 0, now)),
            vec![p("a.rs")],
            "an unchanged file is cleared"
        );

        // Every turn without a usable end clears its drawings at once.
        let cases: Vec<(&str, Box<dyn Fn(&mut Records)>)> = vec![
            (
                "no baseline",
                Box::new(|r: &mut Records| r.end("s", "t1", TurnState::NoBaseline("none".into()))),
            ),
            (
                "unfinished",
                Box::new(|r: &mut Records| r.end("s", "t1", TurnState::Unfinished("gone".into()))),
            ),
            (
                "late",
                Box::new(|r: &mut Records| {
                    r.end("s", "t1", TurnState::Ok);
                    r.record("s", "t1").late = true;
                }),
            ),
            (
                "no record",
                Box::new(|r: &mut Records| {
                    r.0.clear();
                }),
            ),
        ];
        for (name, end) in cases {
            let mut records = Records::default();
            records.running(1, "s", "t1");
            let mut draws = fresh();
            draw_live(&mut draws, &records, 1, "t1", "a.rs", now);
            end(&mut records);
            let steps = draws.steps(&records, 0, now);
            assert_eq!(clears(&steps), vec![p("a.rs")], "{name}: {steps:?}");
            assert!(
                settles(&steps).is_empty() && asks(&steps).is_empty(),
                "{name}: {steps:?}"
            );
            assert!(!draws.wants_ticks(), "{name}: the turn is done");
        }

        // Still pending: it waits for its end snapshot, then clears.
        let mut records = Records::default();
        records.running(1, "s", "t1");
        let mut draws = fresh();
        draw_live(&mut draws, &records, 1, "t1", "a.rs", now);
        records.end("s", "t1", TurnState::Pending);
        assert!(clears(&draws.steps(&records, 0, now)).is_empty());
        let almost = now + LIVE_SETTLE_WAIT - Duration::from_millis(1);
        assert!(
            clears(&draws.steps(&records, 0, almost)).is_empty(),
            "waits for the end snapshot"
        );
        assert!(draws.wants_ticks());
        let steps = draws.steps(&records, 0, now + LIVE_SETTLE_WAIT);
        assert_eq!(clears(&steps), vec![p("a.rs")], "{steps:?}");
        assert!(settles(&steps).is_empty());

        // Ok whose numbering is not known yet: waits as a pending one does.
        let mut records = Records::default();
        records.running(1, "s", "t1");
        let mut draws = fresh();
        draw_live(&mut draws, &records, 1, "t1", "a.rs", now);
        records.end("s", "t1", TurnState::Ok);
        records.record("s", "t1").n = 0;
        let steps = draws.steps(&records, 0, now);
        assert!(asks(&steps).is_empty() && clears(&steps).is_empty(), "{steps:?}");
        records.record("s", "t1").n = 4;
        let steps = draws.steps(&records, 0, now + Duration::from_secs(1));
        assert_eq!(asks(&steps), vec![vec![p("a.rs")]]);
    }

    #[test]
    fn a_turn_end_is_noted_once_with_its_paths() {
        let now = Instant::now();
        let mut records = Records::default();
        records.running(1, "s", "t1");
        let mut draws = fresh();
        draws.edits(
            vec![edit(1, "s", "t1", "a.rs"), edit(1, "s", "t1", "b.rs")],
            &records,
            now,
        );
        let steps = draws.steps(&records, 0, now);
        assert_eq!(notes(&steps), vec![(key("s", "t1"), None, vec![p("a.rs"), p("b.rs")])]);
        assert!(
            notes(&draws.steps(&records, 0, now)).is_empty(),
            "each edit is noted once"
        );

        records.end("s", "t1", TurnState::NoBaseline("none".into()));
        let steps = draws.steps(&records, 0, now);
        assert_eq!(
            notes(&steps),
            vec![(key("s", "t1"), Some(ENDED), vec![p("a.rs"), p("b.rs")])]
        );
        assert!(
            notes(&draws.steps(&records, 0, now)).is_empty(),
            "the end is noted once"
        );
        draws.tab_gone(1);
        assert!(notes(&draws.steps(&records, 0, now)).is_empty(), "and never again");

        // A tab going notes its running turn's end, at the time it went.
        records.running(2, "s", "t2");
        draws.edits(vec![edit(2, "s", "t2", "c.rs")], &records, now);
        draws.steps(&records, 0, now);
        draws.tab_gone(2);
        let steps = draws.steps(&records, 0, now);
        assert_eq!(notes(&steps), vec![(key("s", "t2"), Some(GONE_AT), vec![p("c.rs")])]);
        assert!(notes(&draws.steps(&records, 0, now)).is_empty());
    }

    #[test]
    fn settle_covers_paths_not_shown_now() {
        let now = Instant::now();
        let mut records = Records::default();
        records.running(1, "s", "t1");
        let mut draws = fresh();
        draw_live(&mut draws, &records, 1, "t1", "a.rs", now);
        // b.rs was never shown, so it was never drawn.
        draws.edits(vec![edit(1, "s", "t1", "b.rs")], &records, now);
        let steps = draws.steps(&records, 0, now);
        draws.shown(asks(&steps).remove(0), vec![false], now);
        draws.steps(&records, 0, now);

        records.end("s", "t1", TurnState::Ok);
        let steps = draws.steps(&records, 0, now);
        let asked = asks(&steps);
        assert_eq!(
            asked,
            vec![vec![p("a.rs"), p("b.rs")]],
            "every path the turn named is settled"
        );
        // a.rs's buffer left its window: its drawing is cleared rather than left running.
        draws.shown(asked[0].clone(), vec![false, false], now);
        let steps = draws.steps(&records, 0, now);
        assert_eq!(clears(&steps), vec![p("a.rs")], "{steps:?}");
        assert!(settles(&steps).is_empty(), "no job for a file in no window: {steps:?}");
        assert!(!draws.wants_ticks());
    }

    #[test]
    fn a_skip_clears_a_drawn_path() {
        let now = Instant::now();
        let mut records = Records::default();
        records.running(1, "s", "t1");
        let mut draws = fresh();
        draw_live(&mut draws, &records, 1, "t1", "a.rs", now);
        draws.edits(
            vec![edit(1, "s", "t1", "a.rs"), edit(1, "s", "t1", "b.rs")],
            &records,
            now,
        );
        draws.steps(&records, 0, now);
        draws.shown(vec![p("a.rs"), p("b.rs")], vec![true, true], now);
        assert_eq!(runs(&draws.steps(&records, 0, now)), vec![p("a.rs"), p("b.rs")]);
        draws.live_done(&key("s", "t1"), &p("a.rs"), Ok(Err(LiveSkip::Unchanged)));
        draws.live_done(&key("s", "t1"), &p("b.rs"), Ok(Err(LiveSkip::Ignored)));
        let steps = draws.steps(&records, 0, now);
        assert_eq!(
            clears(&steps),
            vec![p("a.rs")],
            "only the drawn path is cleared: {steps:?}"
        );
        assert!(drawn(&steps).is_empty());

        // An error keeps whatever is drawn: the settle decides.
        draw_live(&mut draws, &records, 1, "t1", "c.rs", now);
        draws.edits(vec![edit(1, "s", "t1", "c.rs")], &records, now);
        draws.steps(&records, 0, now);
        draws.shown(vec![p("c.rs")], vec![true], now);
        draws.steps(&records, 0, now);
        draws.live_done(
            &key("s", "t1"),
            &p("c.rs"),
            Err(ReviewError::Unavailable("busy".into())),
        );
        let steps = draws.steps(&records, 0, now);
        assert!(clears(&steps).is_empty() && drawn(&steps).is_empty(), "{steps:?}");
    }

    #[test]
    fn following_tab_asks_again_once() {
        let now = Instant::now();
        let mut records = Records::default();
        records.running(1, "s", "t1");

        // The follow run opens the file after the first answer: asked once more when it ends.
        let mut draws = fresh();
        draws.edits(vec![edit(1, "s", "t1", "a.rs")], &records, now);
        draws.steps(&records, 0, now);
        draws.shown(vec![p("a.rs")], vec![false], now);
        assert!(asks(&draws.steps(&records, 0, now)).is_empty());
        let later = now + Duration::from_millis(300);
        draws.follow_ended(1, &p("a.rs"), later);
        let steps = draws.steps(&records, 0, later);
        assert_eq!(asks(&steps), vec![vec![p("a.rs")]], "asked again after the follow run");
        draws.shown(vec![p("a.rs")], vec![false], later);
        let steps = draws.steps(&records, 0, now + Duration::from_secs(5));
        assert!(asks(&steps).is_empty(), "asked once more, never twice: {steps:?}");
        draws.follow_ended(1, &p("a.rs"), now + Duration::from_secs(5));
        assert!(asks(&draws.steps(&records, 0, now + Duration::from_secs(5))).is_empty());

        // No follow run ended: asked once more a second after the edit.
        let mut draws = fresh();
        draws.edits(vec![edit(1, "s", "t1", "a.rs")], &records, now);
        draws.steps(&records, 0, now);
        draws.shown(vec![p("a.rs")], vec![false], now);
        let steps = draws.steps(&records, 0, now + FOLLOW_REASK - Duration::from_millis(1));
        assert!(asks(&steps).is_empty(), "{steps:?}");
        let steps = draws.steps(&records, 0, now + FOLLOW_REASK);
        assert_eq!(asks(&steps), vec![vec![p("a.rs")]]);
        draws.shown(vec![p("a.rs")], vec![true], now + FOLLOW_REASK);
        assert_eq!(runs(&draws.steps(&records, 0, now + FOLLOW_REASK)), vec![p("a.rs")]);

        // The follow run ends while the first question is still out: asked again right after it.
        let mut draws = fresh();
        draws.edits(vec![edit(1, "s", "t1", "a.rs")], &records, now);
        draws.steps(&records, 0, now);
        draws.follow_ended(1, &p("a.rs"), now);
        draws.shown(vec![p("a.rs")], vec![false], now);
        assert_eq!(asks(&draws.steps(&records, 0, now)), vec![vec![p("a.rs")]]);

        // Another tab's follow run is not this path's.
        let mut draws = fresh();
        draws.edits(vec![edit(1, "s", "t1", "a.rs")], &records, now);
        draws.steps(&records, 0, now);
        draws.shown(vec![p("a.rs")], vec![false], now);
        draws.follow_ended(2, &p("a.rs"), now);
        assert!(asks(&draws.steps(&records, 0, now)).is_empty());
    }

    #[test]
    fn tab_gone_clears_its_paths_and_drops_its_jobs() {
        let now = Instant::now();
        let mut records = Records::default();
        records.running(1, "s", "t1");
        records.running(2, "s2", "u1");
        let mut draws = fresh();
        draw_live(&mut draws, &records, 1, "t1", "a.rs", now);
        // Tab 2 draws its own file.
        draws.edits(vec![edit(2, "s2", "u1", "r.rs")], &records, now);
        draws.steps(&records, 0, now);
        draws.shown(vec![p("r.rs")], vec![true], now);
        draws.steps(&records, 0, now);
        draws.live_done(&key("s2", "u1"), &p("r.rs"), live_ok("r.rs", 3));
        draws.steps(&records, 0, now);
        // Tab 1 has a job out for b.rs.
        draws.edits(vec![edit(1, "s", "t1", "b.rs")], &records, now);
        draws.steps(&records, 0, now);
        draws.shown(vec![p("b.rs")], vec![true], now);
        assert_eq!(runs(&draws.steps(&records, 0, now)), vec![p("b.rs")]);

        draws.tab_gone(1);
        let steps = draws.steps(&records, 1, now);
        assert_eq!(
            clears(&steps),
            vec![p("a.rs")],
            "only the gone tab's drawings: {steps:?}"
        );
        draws.live_done(&key("s", "t1"), &p("b.rs"), live_ok("b.rs", 3));
        let steps = draws.steps(&records, 0, now);
        assert!(drawn(&steps).is_empty(), "its job's answer is dropped: {steps:?}");
        assert!(draws.wants_ticks(), "tab 2's turn still runs");

        records.end("s2", "u1", TurnState::Unfinished("gone".into()));
        assert_eq!(clears(&draws.steps(&records, 0, now)), vec![p("r.rs")]);
        assert!(!draws.wants_ticks());
    }

    #[test]
    fn editor_lost_drops_every_live_job() {
        let now = Instant::now();
        let mut records = Records::default();
        records.running(1, "s", "t1");
        let mut draws = fresh();
        draw_live(&mut draws, &records, 1, "t1", "a.rs", now);
        draws.edits(vec![edit(1, "s", "t1", "a.rs")], &records, now);
        draws.steps(&records, 0, now);
        draws.shown(vec![p("a.rs")], vec![true], now);
        assert_eq!(runs(&draws.steps(&records, 0, now)), vec![p("a.rs")]);
        draws.edits(vec![edit(1, "s", "t1", "b.rs")], &records, now);

        draws.editor_lost();
        draws.live_done(&key("s", "t1"), &p("a.rs"), live_ok("a.rs", 10));
        let steps = draws.steps(&records, 0, now);
        assert!(
            drawn(&steps).is_empty(),
            "a job out when the editor went is dropped: {steps:?}"
        );
        assert!(runs(&steps).is_empty() && asks(&steps).is_empty(), "{steps:?}");
        assert!(clears(&steps).is_empty(), "there is no editor to clear: {steps:?}");

        // The next edit starts again, and the turn's end clears nothing the lost editor drew.
        draws.edits(vec![edit(1, "s", "t1", "a.rs")], &records, now);
        assert_eq!(asks(&draws.steps(&records, 0, now)), vec![vec![p("a.rs")]]);
        draws.shown(vec![p("a.rs")], vec![false], now);
        records.end("s", "t1", TurnState::Unfinished("gone".into()));
        let steps = draws.steps(&records, 0, now);
        assert!(clears(&steps).is_empty(), "{steps:?}");
    }

    #[test]
    fn wants_ticks_is_false_when_idle() {
        let now = Instant::now();
        let mut records = Records::default();
        let mut draws = fresh();
        assert!(!draws.wants_ticks(), "a new window has nothing to do");
        records.running(1, "s", "t1");
        draws.edits(vec![edit(1, "s", "t1", "a.rs")], &records, now);
        assert!(draws.wants_ticks(), "an edit waits for its question");
        draws.steps(&records, 0, now);
        draws.shown(vec![p("a.rs")], vec![false], now);
        draws.steps(&records, 0, now + FOLLOW_REASK);
        draws.shown(vec![p("a.rs")], vec![false], now + FOLLOW_REASK);
        draws.steps(&records, 0, now + FOLLOW_REASK);
        assert!(draws.wants_ticks(), "a running turn is read each tick for its end");
        records.end("s", "t1", TurnState::Ok);
        draws.steps(&records, 0, now + FOLLOW_REASK);
        draws.shown(vec![p("a.rs")], vec![false], now + FOLLOW_REASK);
        draws.steps(&records, 0, now + FOLLOW_REASK);
        assert!(
            !draws.wants_ticks(),
            "a settled turn and no drawing leave nothing to do"
        );
        draws.off(&p("a.rs"));
        assert!(!draws.wants_ticks());
    }

    #[test]
    fn a_turn_seen_before_its_record_waits_for_it() {
        let now = Instant::now();
        let mut records = Records::default();
        let mut draws = fresh();
        draws.edits(vec![edit(1, "s", "t1", "a.rs")], &records, now);
        let steps = draws.steps(&records, 0, now);
        assert!(
            asks(&steps).is_empty() && notes(&steps).is_empty(),
            "nothing is known of the turn yet: {steps:?}"
        );

        // The record appears running: the path named before it is asked about and noted.
        records.running(1, "s", "t1");
        let steps = draws.steps(&records, 0, now);
        assert_eq!(asks(&steps), vec![vec![p("a.rs")]], "{steps:?}");
        assert_eq!(notes(&steps), vec![(key("s", "t1"), None, vec![p("a.rs")])]);
        draws.shown(vec![p("a.rs")], vec![true], now);
        assert_eq!(runs(&draws.steps(&records, 0, now)), vec![p("a.rs")]);

        // A record that never comes: the turn is let go after the wait, and not retired for good.
        let mut records = Records::default();
        let mut draws = fresh();
        draws.edits(vec![edit(1, "s", "t1", "a.rs")], &records, now);
        draws.steps(&records, 0, now);
        assert!(draws.wants_ticks(), "waits for the record");
        let steps = draws.steps(&records, 0, now + RECORD_WAIT);
        assert!(steps.is_empty(), "{steps:?}");
        assert!(!draws.wants_ticks(), "let go after the wait");
        records.running(1, "s", "t1");
        draws.edits(vec![edit(1, "s", "t1", "a.rs")], &records, now + RECORD_WAIT);
        let steps = draws.steps(&records, 0, now + RECORD_WAIT);
        assert_eq!(asks(&steps), vec![vec![p("a.rs")]], "a later edit is drawn: {steps:?}");
    }

    #[test]
    fn paths_named_after_the_end_note_are_noted_when_the_turn_goes() {
        // The tab goes before the next tick.
        let now = Instant::now();
        let mut records = Records::default();
        records.running(1, "s", "t1");
        records.end("s", "t1", TurnState::Pending);
        let mut draws = fresh();
        draws.edits(vec![edit(1, "s", "t1", "a.rs")], &records, now);
        draws.steps(&records, 0, now);
        draws.edits(vec![edit(1, "s", "t1", "z.rs")], &records, now);
        draws.tab_gone(1);
        let steps = draws.steps(&records, 0, now);
        assert_eq!(
            notes(&steps),
            vec![(key("s", "t1"), Some(ENDED), vec![p("z.rs")])],
            "{steps:?}"
        );

        // The record goes before the next tick.
        let mut records = Records::default();
        records.running(1, "s", "t1");
        records.end("s", "t1", TurnState::Pending);
        let mut draws = fresh();
        draws.edits(vec![edit(1, "s", "t1", "a.rs")], &records, now);
        draws.steps(&records, 0, now);
        draws.edits(vec![edit(1, "s", "t1", "z.rs")], &records, now);
        records.0.clear();
        let steps = draws.steps(&records, 0, now);
        assert_eq!(
            notes(&steps),
            vec![(key("s", "t1"), Some(ENDED), vec![p("z.rs")])],
            "{steps:?}"
        );
    }

    /// Ends `turn_id`'s settle of `path` as `Ok` and returns the steps that follow.
    fn settle_one(draws: &mut LiveDraws, records: &Records, session: &str, turn_id: &str, path: &str) -> Vec<LiveStep> {
        let now = Instant::now();
        draws.shown(vec![p(path)], vec![true], now);
        let steps = draws.steps(records, 0, now);
        assert_eq!(settles(&steps).len(), 1, "{steps:?}");
        draws.settle_done(&key(session, turn_id), &p(path), settle_ok(path, &[1], 9));
        draws.steps(records, 0, now)
    }

    fn drawn_by(steps: &[LiveStep]) -> Vec<(PathBuf, TurnKey)> {
        steps
            .iter()
            .filter_map(|s| match s {
                LiveStep::Draw { path, key, .. } => Some((path.clone(), key.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_ending_turns_last_edit_takes_the_path_from_an_older_settle() {
        let now = Instant::now();
        let mut records = Records::default();
        records.running(1, "s", "t1");
        let mut draws = fresh();
        draw_live(&mut draws, &records, 1, "t1", "a.rs", now);
        records.end("s", "t1", TurnState::Ok);
        assert_eq!(asks(&draws.steps(&records, 0, now)), vec![vec![p("a.rs")]]);
        draws.shown(vec![p("a.rs")], vec![true], now);
        assert_eq!(settles(&draws.steps(&records, 0, now)), vec![(p("a.rs"), 3)]);

        // Another session's turn edits the file, and its last edit comes in the batch that ends it.
        records.running(2, "s2", "u1");
        records.end("s2", "u1", TurnState::Ok);
        draws.edits(vec![edit(2, "s2", "u1", "a.rs")], &records, now);
        let steps = draws.steps(&records, 1, now);
        assert_eq!(
            asks(&steps),
            vec![vec![p("a.rs")]],
            "the newer turn settles the path: {steps:?}"
        );
        draws.settle_done(&key("s", "t1"), &p("a.rs"), settle_ok("a.rs", &[1], 9));
        let steps = settle_one(&mut draws, &records, "s2", "u1", "a.rs");
        assert_eq!(drawn_by(&steps), vec![(p("a.rs"), key("s2", "u1"))], "{steps:?}");
        assert!(!draws.wants_ticks());
    }

    #[test]
    fn an_old_turns_late_edit_does_not_take_a_newer_turns_path() {
        let now = Instant::now();
        let mut records = Records::default();
        records.running(1, "s", "t1");
        let mut draws = fresh();
        draw_live(&mut draws, &records, 1, "t1", "a.rs", now);
        // A turn that ended before t1 started has an edit drained only now.
        records.running(2, "s0", "o1");
        records.end("s0", "o1", TurnState::Ok);
        records.record("s0", "o1").started_ns = STARTED - 50;
        records.record("s0", "o1").ended_ns = Some(STARTED - 10);
        draws.edits(vec![edit(2, "s0", "o1", "a.rs")], &records, now);
        let steps = draws.steps(&records, 0, now);
        assert!(
            asks(&steps).is_empty() && clears(&steps).is_empty() && drawn(&steps).is_empty(),
            "{steps:?}"
        );
        // t1 still draws the path.
        draws.edits(vec![edit(1, "s", "t1", "a.rs")], &records, now);
        draws.steps(&records, 0, now);
        draws.shown(vec![p("a.rs")], vec![true], now);
        assert_eq!(runs(&draws.steps(&records, 0, now)), vec![p("a.rs")]);
        draws.live_done(&key("s", "t1"), &p("a.rs"), live_ok("a.rs", 10));
        assert_eq!(drawn(&draws.steps(&records, 0, now)), vec![(p("a.rs"), true)]);
    }

    #[test]
    fn a_finished_newer_turn_keeps_its_path_from_an_older_settle() {
        let now = Instant::now();
        let mut records = Records::default();
        records.running(1, "s", "t1");
        let mut draws = fresh();
        draw_live(&mut draws, &records, 1, "t1", "a.rs", now);
        // t1 ends, and its end snapshot is slow.
        records.end("s", "t1", TurnState::Pending);
        draws.steps(&records, 0, now);

        // t2 edits the same file, ends and settles before it.
        records.running(1, "s", "t2");
        draw_live(&mut draws, &records, 1, "t2", "a.rs", now);
        records.end("s", "t2", TurnState::Ok);
        assert_eq!(asks(&draws.steps(&records, 0, now)), vec![vec![p("a.rs")]]);
        let steps = settle_one(&mut draws, &records, "s", "t2", "a.rs");
        assert_eq!(drawn_by(&steps), vec![(p("a.rs"), key("s", "t2"))]);

        // t1's snapshot comes: its older change is not drawn over t2's, and t2's drawing stays.
        records.record("s", "t1").state = TurnState::Ok;
        let steps = draws.steps(&records, 0, now);
        assert!(
            asks(&steps).is_empty() && clears(&steps).is_empty() && drawn(&steps).is_empty(),
            "{steps:?}"
        );
        assert!(!draws.wants_ticks());

        // Once no turn names the path, a new turn's late edit may take it.
        records.running(1, "s", "t3");
        records.end("s", "t3", TurnState::Ok);
        draws.edits(vec![edit(1, "s", "t3", "a.rs")], &records, now);
        assert_eq!(asks(&draws.steps(&records, 0, now)), vec![vec![p("a.rs")]]);
    }
}
