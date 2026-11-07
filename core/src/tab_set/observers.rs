//! The feed of a tab for an observer outside the desk (a phone following the session): its own frames, its own
//! position in the tab's revisions and its own queue, so the desk's delivery is never touched.
//!
//! The desk's pages are fed by the tick, which drains each backend once (`take_revised_ui_delivery`) and decides
//! what each page sees, with the bookkeeping that goes with it: `stale`, `turn_times_unsent`, the queue settled
//! against a snapshot, the revision a snapshot covered (`note_ui_snapshot`). An observer reads the same drain and
//! none of that bookkeeping. It holds the backend generation and the revision its frames reached; from the drained
//! batch it takes only the events above that revision, serialized once for every observer at the same position. A
//! snapshot is serialized only when the observer takes its frames, never inside the tick, from the same view the
//! desk's snapshot reads, without settling the queue, stamping the tab's clock or noting a revision on the backend.
//!
//! Bounded per observer: queued frames past [`OBSERVER_QUEUE_CAP_BYTES`] (1 MiB) are dropped together and the observer is
//! owed a snapshot instead, so one that stops taking costs at most that much memory and never sees a gap. A backend
//! put in place of the one it followed (a restart, a resume, a reset), a `Resync` that lost events, a change to the
//! broker's cards (which no events frame carries), and an observer registered fresh all owe a snapshot the same way. A tab that closes, or is frozen to move to another window,
//! ends each of its observers with a `Gone` after what was already queued; nothing follows it.
//!
//! A snapshot holds the last turns only, under a byte ceiling (`tail`); older turns come as one `Page` at a time, asked
//! for with [`TabSet::observer_more`] and built at the observer's next take within that take's budget. A `/clear` in
//! the session starts a conversation whose turns are numbered anew, so it owes each observer a snapshot and drops a
//! page owed of the one before. A tab whose session fails or goes away sends `Ended` once for that session (once per
//! backend generation), after what was queued; a change to whether a phone may send to the
//! tab (its agent, its mode, whether it holds a session) sends `SendBlock`. Both are read from the tab when frames are
//! taken, so nothing in the tick has to report them.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use agent::AgentDomainEvent;

use super::{AgentId, BypassScope, RowKey, SessionModeChoice, Tab, TabBackend, TabSet, UnseenStretch};
use crate::agent_backend::ProjectionRef;
use crate::agent_bridge::{
    serialize_events_with_notes_and_revisions_for_js, serialize_snapshot_with_notes_for_js, serialize_tabs_for_js,
    CallNotes, SnapshotView, TurnTime,
};
use crate::remote_greeting::{remote_theme, Greeting, PendingPrompt};
use crate::remote_rv::{CutEntry, EndedHow, MoreRefusal, SendBlock};
use crate::tabs::TabId;

mod tail;
use tail::{Turns, SNAPSHOT_CEILING_BYTES};
#[cfg(test)]
use tail::{CUT_KEEP_BYTES, MORE_PAGE_TURNS, SNAPSHOT_TURNS};

/// An observer registered with [`TabSet::observe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObserverId(pub u64);

/// Why an observed tab is no longer here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoneWhy {
    Closed,
    /// Frozen to move to another window (or handed over to one): it is followed there, not here.
    Moved,
}

/// The work a frame shows, as a stop must name it: which session of the tab, and which stretch of the work the panel
/// cannot show. Neither is in the desk's envelope, which names a turn only by the agent's id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkMark {
    /// The tab's backend generation the frame is of ([`Tab::backend_generation`]).
    pub generation: u64,
    /// The latest stretch's number ([`Tab::unseen_stretch`]) when the tick has counted every fold through the
    /// frame's revision, else `None`: work folded since may have ended one stretch and begun another.
    pub unseen: Option<u64>,
}

/// One frame for an observer, in the order it must be applied. `json` is the desk's own envelope for the same
/// content (`kind` `snapshot` or `events`); `through` is the revision the observer stands at once it is applied, and
/// `work` names the session and the unseen stretch as they stand there.
#[derive(Debug, Clone)]
pub enum ObserverFrame {
    /// The tab's state through `through`, from turn `first_turn` on; whatever the observer held before is replaced.
    /// `cut` names the entries it holds only the start of.
    Snapshot {
        key: RowKey,
        through: u64,
        work: WorkMark,
        first_turn: u64,
        cut: Vec<CutEntry>,
        json: Arc<str>,
    },
    /// The events after revision `from` through `through`, with their own revisions; `from` is where the previous
    /// frame ended. Carries notes only (no events) when the batch above `from` settled notes and held no new event.
    Events {
        key: RowKey,
        from: u64,
        through: u64,
        work: WorkMark,
        json: Arc<str>,
    },
    /// Turns `from_turn` through `through_turn` of the session `mark` names, every entry whole: the answer to
    /// [`TabSet::observer_more`]. The envelope is the snapshot's, holding only those turns.
    Page {
        key: RowKey,
        mark: WorkMark,
        from_turn: u64,
        through_turn: u64,
        json: Arc<str>,
    },
    /// The tab's session failed or went away; the tab is still here, and a new session's snapshot may follow.
    Ended { key: RowKey, how: EndedHow },
    /// Whether, and why not, a phone may send to the tab now.
    SendBlock { key: RowKey, block: Option<SendBlock> },
    /// The last frame of this observer.
    Gone { key: RowKey, why: GoneWhy },
}

impl ObserverFrame {
    /// What the frame counts against a queue's cap and a take's budget.
    fn bytes(&self) -> usize {
        match self {
            ObserverFrame::Snapshot { json, .. }
            | ObserverFrame::Events { json, .. }
            | ObserverFrame::Page { json, .. } => json.len(),
            ObserverFrame::Ended { .. } | ObserverFrame::SendBlock { .. } | ObserverFrame::Gone { .. } => 0,
        }
    }
}

/// Why [`TabSet::observe`] registered nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObserveError {
    NoSuchTab,
    /// The tab holds no live session, or is frozen to move.
    NotLive,
}

/// The bytes of frames one observer may have waiting; past it they are dropped and a snapshot is owed.
pub const OBSERVER_QUEUE_CAP_BYTES: usize = 1024 * 1024;

/// Where an observer's owner that goes away without forgetting its observers (a socket dropped with its window)
/// leaves their ids; the set forgets them at its next pump. Cloned from [`TabSet::observer_reaper`], and safe to hand
/// ids to from any thread.
#[derive(Clone, Default)]
pub struct ObserverReaper(Arc<Mutex<Vec<ObserverId>>>);

impl ObserverReaper {
    /// Leaves `ids` to be forgotten; an id the set no longer knows changes nothing.
    pub fn hand_over(&self, ids: impl IntoIterator<Item = ObserverId>) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).extend(ids);
    }

    fn take(&self) -> Vec<ObserverId> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// Every observer of the set's tabs.
#[derive(Default)]
pub(super) struct Observers {
    next_id: u64,
    pub(super) by_id: BTreeMap<u64, Observer>,
    reaper: ObserverReaper,
    /// How many observer snapshots were serialized, for tests that prove none is built by the tick.
    #[cfg(test)]
    pub(super) snapshots_built: usize,
}

pub(super) struct Observer {
    tab: TabId,
    key: RowKey,
    /// The tab's backend generation the frames are of.
    generation: u64,
    /// The revision the observer stands at once its queue is applied.
    watermark: u64,
    /// The newest of the tab's turn times the observer was given, so a later frame carries only those after it.
    turn_time_sent: Option<TurnTime>,
    pub(super) queue: VecDeque<ObserverFrame>,
    pub(super) queued_bytes: usize,
    /// The next take starts with a snapshot; nothing is queued meanwhile but a `Gone`.
    needs_snapshot: bool,
    /// `Gone` is queued: nothing more is.
    gone: bool,
    /// The turn a page asked for with `more` ends before, until a take builds it.
    owed_page: Option<u64>,
    /// The tab's backend generation whose end the observer was told: `Ended` goes once per generation, whether or not
    /// the observer ever had that generation's snapshot.
    ended_of: Option<u64>,
    /// What the observer was last told about sending to the tab (by its greeting, then by `SendBlock`).
    send_block: Option<SendBlock>,
}

impl Observer {
    /// Drops what waits and owes a snapshot.
    fn want_snapshot(&mut self) {
        self.queue.clear();
        self.queued_bytes = 0;
        self.needs_snapshot = true;
    }

    fn push(&mut self, frame: ObserverFrame) {
        let bytes = frame.bytes();
        if self.queued_bytes.saturating_add(bytes) > OBSERVER_QUEUE_CAP_BYTES {
            self.want_snapshot();
            return;
        }
        self.queued_bytes += bytes;
        self.queue.push_back(frame);
    }
}

/// One drained batch of a tab, as the tick folded it, for the tab's observers: every event with its revision, and
/// the notes the batch settled (turn times aside, which each observer takes from the tab's list).
pub(super) struct ObservedBatch {
    pub(super) revisions: Vec<u64>,
    pub(super) events: Vec<AgentDomainEvent>,
    pub(super) notes: CallNotes,
}

/// Where an observer stands: its revision and the newest turn time it was given.
type Position = (u64, Option<TurnTime>);

/// An events frame's content, shared by every observer at the same position.
#[derive(Clone)]
struct Built {
    from: u64,
    through: u64,
    work: WorkMark,
    json: Arc<str>,
}

impl Observers {
    /// Forgets the observers handed to the reaper since the last pump.
    pub(super) fn reap(&mut self) {
        for id in self.reaper.take() {
            self.by_id.remove(&id.0);
        }
    }

    fn add(&mut self, tab: TabId, key: RowKey, generation: u64, send_block: Option<SendBlock>) -> ObserverId {
        self.next_id += 1;
        self.by_id.insert(
            self.next_id,
            Observer {
                tab,
                key,
                generation,
                watermark: 0,
                turn_time_sent: None,
                queue: VecDeque::new(),
                queued_bytes: 0,
                needs_snapshot: true,
                gone: false,
                owed_page: None,
                ended_of: None,
                send_block,
            },
        );
        ObserverId(self.next_id)
    }

    fn of(&mut self, tab: TabId) -> impl Iterator<Item = &mut Observer> + '_ {
        self.by_id.values_mut().filter(move |o| o.tab == tab && !o.gone)
    }

    /// Whether anything follows `tab`: the tick hands a batch over only then.
    pub(super) fn watches(&self, tab: TabId) -> bool {
        self.by_id.values().any(|o| o.tab == tab && !o.gone)
    }

    /// The tab changed in a way no events frame carries (its delivery lost events, the broker's cards changed): each
    /// observer is owed a snapshot, and a page it was owed is dropped. The lost events may have held a `/clear`, after
    /// which the page's turn number names a turn of another conversation; the phone drops the answer to any `more` it
    /// sent before the snapshot it is given, and asks again from that snapshot.
    pub(super) fn owe_snapshot(&mut self, tab: TabId) {
        for observer in self.of(tab) {
            observer.want_snapshot();
            observer.owed_page = None;
        }
    }

    /// Queues for each observer of `tab` what `batch` holds past its position. `generation` is the tab's backend
    /// generation the batch was drained from; `stretch` and `turn_times` are the tab's as the tick left them.
    pub(super) fn feed(
        &mut self,
        tab: TabId,
        generation: u64,
        stretch: UnseenStretch,
        batch: &ObservedBatch,
        turn_times: &[TurnTime],
    ) {
        let mut built: Vec<(Position, Option<Built>)> = Vec::new();
        // A `/clear` empties the conversation the observers' turn numbers and pages are of: each gets the new one as a
        // snapshot, and a page owed of the old one is never built from the new one.
        let reset = batch
            .events
            .iter()
            .any(|event| matches!(event, AgentDomainEvent::ConversationReset { .. }));
        for observer in self.of(tab) {
            if reset {
                observer.want_snapshot();
                observer.owed_page = None;
                continue;
            }
            if observer.generation != generation {
                observer.want_snapshot();
                continue;
            }
            if observer.needs_snapshot {
                continue;
            }
            let at = (observer.watermark, observer.turn_time_sent.clone());
            let frame = match built.iter().find(|(position, _)| *position == at) {
                Some((_, frame)) => frame.clone(),
                None => {
                    let frame = events_frame(
                        tab,
                        observer.watermark,
                        observer.turn_time_sent.as_ref(),
                        batch,
                        turn_times,
                    )
                    .map(|(from, through, json)| Built {
                        from,
                        through,
                        work: WorkMark {
                            generation,
                            unseen: stretch.at(through),
                        },
                        json,
                    });
                    built.push((at, frame.clone()));
                    frame
                }
            };
            let Some(Built {
                from,
                through,
                work,
                json,
            }) = frame
            else {
                continue;
            };
            observer.watermark = through;
            observer.turn_time_sent = turn_times.last().cloned();
            observer.push(ObserverFrame::Events {
                key: observer.key.clone(),
                from,
                through,
                work,
                json,
            });
        }
    }

    /// Ends every observer of `tab` with `Gone`, after what it has queued.
    pub(super) fn gone(&mut self, tab: TabId, why: GoneWhy) {
        for observer in self.of(tab) {
            observer.gone = true;
            observer.queue.push_back(ObserverFrame::Gone {
                key: observer.key.clone(),
                why,
            });
        }
    }
}

/// The turn times after `sent` in `times`; all of them when `sent` is no longer there (the list started over or
/// dropped it), which the page keeps by turn id, so one it already has changes nothing.
fn turn_times_after<'a>(times: &'a [TurnTime], sent: Option<&TurnTime>) -> &'a [TurnTime] {
    match sent.and_then(|sent| times.iter().rposition(|time| time == sent)) {
        Some(at) => &times[at + 1..],
        None => times,
    }
}

/// The events frame for an observer at `watermark` (its `from`, `through` and envelope), or `None` when the batch
/// holds nothing past it.
fn events_frame(
    tab: TabId,
    watermark: u64,
    sent: Option<&TurnTime>,
    batch: &ObservedBatch,
    turn_times: &[TurnTime],
) -> Option<(u64, u64, Arc<str>)> {
    // The tags strictly increase: what the observer's snapshot or an earlier frame carried is a prefix.
    let shown = batch.revisions.partition_point(|revision| *revision <= watermark);
    let mut notes = batch.notes.clone();
    notes.turn_times = turn_times_after(turn_times, sent).to_vec();
    if shown == batch.events.len() && notes.is_empty() {
        return None;
    }
    let through = batch.revisions.last().copied().unwrap_or(watermark).max(watermark);
    let json = serialize_events_with_notes_and_revisions_for_js(
        tab,
        watermark,
        through,
        &batch.events[shown..],
        &notes,
        Some(&batch.revisions[shown..]),
    );
    Some((watermark, through, Arc::from(json)))
}

/// An observer's snapshot as built: its revision, the work it shows, the first turn it holds, the entries it holds cut,
/// its envelope, and the newest of the tab's turn times when it was read.
struct BuiltSnapshot {
    through: u64,
    work: WorkMark,
    first_turn: u64,
    cut: Vec<CutEntry>,
    json: String,
    turn_time: Option<TurnTime>,
}

/// The tab's snapshot for an observer, tail-limited (`tail`). Read from the same view as the desk's snapshot, and
/// nothing of the tab's delivery state is changed. A conversation that fits whole is serialized from the live view;
/// otherwise the turns it keeps are copied while the projection is held, and serialized from the copy after.
fn observer_snapshot(tab: &Tab) -> Option<BuiltSnapshot> {
    let backend = tab.live()?;
    let view = SnapshotView::of(backend, &tab.session.host_answered);
    // The clock as the last tick left it, and only for the turn this very snapshot names: a clock from the turn
    // before would be taken as this one's start.
    let started_at = tab
        .session
        .turn_clock
        .as_ref()
        .filter(|(turn, _)| view.projection.active_turn_id.as_deref() == Some(turn.as_str()))
        .map(|(_, at)| *at);
    let through = view.projection.last_revision;
    // The stretch read beside the revision, under the same guard, as a stop's admission reads them.
    let work = WorkMark {
        generation: tab.backend_generation,
        unseen: tab.unseen_stretch_at(through),
    };
    // Every turn time the tab had when the snapshot was read: the next events frame carries only those after it.
    let turn_time = tab.notes.turn_times.last().cloned();
    let cards = &tab.session.editor_cards;
    let turns = Turns::of(&view.projection);
    let mut tail = tail::choose_tail(&view.projection, &turns, SNAPSHOT_CEILING_BYTES);
    if tail.is_whole() {
        let json = serialize_snapshot_with_notes_for_js(tab.id, &view, started_at, &tab.notes, cards);
        let over = json.len().saturating_sub(SNAPSHOT_CEILING_BYTES);
        // One turn over the ceiling has its largest entries cut, as a last turn alone over it does.
        let fewer = match (over, turns.count()) {
            (0, _) => None,
            (_, 1) => tail::cut_largest(tail, &view.projection, over),
            _ => Some(tail::shorter(tail, &turns, 1)),
        };
        let Some(fewer) = fewer else {
            // Under the ceiling, or nothing left to cut.
            return Some(BuiltSnapshot {
                through,
                work,
                first_turn: 0,
                cut: Vec::new(),
                json,
                turn_time,
            });
        };
        tail = fewer;
    }
    let SnapshotView {
        backend: kind,
        conversation_id,
        session_id,
        provider_session_id,
        capabilities,
        provider,
        projection,
        hidden_pending,
    } = view;
    let mut copy = tail::tail_copy(&projection, &tail);
    drop(projection);
    // The estimate keeps it under the ceiling; what the state beside the turns weighs is known only once serialized,
    // so a snapshot still over drops the share of its turns it is over by, a few times at most.
    for attempt in 0..4 {
        let view = SnapshotView {
            backend: kind,
            conversation_id,
            session_id,
            provider_session_id: provider_session_id.clone(),
            capabilities,
            provider,
            projection: ProjectionRef::Borrowed(&copy),
            hidden_pending,
        };
        let notes = tail::notes_between(&tab.notes, tail.from_seq(), u64::MAX);
        let json = serialize_snapshot_with_notes_for_js(tab.id, &view, started_at, &notes, cards);
        drop(view);
        let held = turns.count() as usize - tail.first_turn;
        let over = json.len().saturating_sub(SNAPSHOT_CEILING_BYTES);
        // Over the ceiling: drop the share of the turns it is over by, or, with one turn left, cut its largest entries.
        let fewer = match (over, held, attempt) {
            (0, _, _) | (_, _, 3) => None,
            (_, 1, _) => tail::cut_largest(tail.clone(), &copy, over),
            _ => Some(tail::shorter(
                tail.clone(),
                &turns,
                (held * over).div_ceil(json.len()).max(1),
            )),
        };
        let Some(fewer) = fewer else {
            return Some(BuiltSnapshot {
                through,
                work,
                first_turn: tail.first_turn as u64,
                cut: tail.cut_entries(),
                json,
                turn_time,
            });
        };
        tail = fewer;
        copy = tail::tail_copy(&copy, &tail);
    }
    unreachable!("the last attempt returns")
}

/// What a take reads for an owed page.
enum PageRead {
    /// The work it is of, its first and last turn and its envelope.
    Page(WorkMark, u64, u64, String),
    /// Its turns may still change by events the observer has not been sent: a later take builds it.
    Later,
    /// The tab holds no session or no such turn.
    None,
}

/// Whether a page of the turns before `before_turn` waits for a later take. The projection can be folded past what the
/// tick has sent the observer (`watermark`); while it is, a page whose last turn is the last one the observer was sent
/// the start of would hold what the events still to come add to it, and the phone would apply those twice. A page of
/// turns that ended before the watermark goes at once.
fn page_waits(projection: &agent::AgentSessionProjection, turns: &Turns, before_turn: u64, watermark: u64) -> bool {
    if projection.last_revision == watermark || before_turn == 0 || before_turn > turns.count() {
        return false;
    }
    let last = (before_turn - 1) as usize;
    // The turn after the page starts at `next`; that fold is revision `next + 1`.
    let (_, next) = tail::page_seqs(turns, last, last);
    next >= watermark
}

/// A page of turns before `before_turn` for an observer standing at `watermark`, within `limit` bytes (at least one
/// turn, however large), unless it waits ([`page_waits`]).
fn observer_page(tab: &Tab, before_turn: u64, limit: usize, watermark: u64) -> PageRead {
    let Some(backend) = tab.live() else {
        return PageRead::None;
    };
    let view = SnapshotView::of(backend, &tab.session.host_answered);
    let mark = WorkMark {
        generation: tab.backend_generation,
        unseen: tab.unseen_stretch_at(view.projection.last_revision),
    };
    let turns = Turns::of(&view.projection);
    if page_waits(&view.projection, &turns, before_turn, watermark) {
        return PageRead::Later;
    }
    let Some((from, through)) = tail::choose_page(&view.projection, &turns, before_turn, limit) else {
        return PageRead::None;
    };
    let SnapshotView {
        backend: kind,
        conversation_id,
        session_id,
        provider_session_id,
        capabilities,
        provider,
        projection,
        hidden_pending,
    } = view;
    let copy = tail::page_copy(&projection, &turns, from, through);
    drop(projection);
    let view = SnapshotView {
        backend: kind,
        conversation_id,
        session_id,
        provider_session_id,
        capabilities,
        provider,
        projection: ProjectionRef::Borrowed(&copy),
        hidden_pending,
    };
    let (lo, hi) = tail::page_seqs(&turns, from, through);
    let notes = tail::notes_between(&tab.notes, lo, hi);
    // No clock and no broker cards: a page is past turns, not the session as it stands.
    let json = serialize_snapshot_with_notes_for_js(tab.id, &view, None, &notes, &[]);
    PageRead::Page(mark, from as u64, through as u64, json)
}

/// Why a phone may not send to `tab` now, if it may not: only a Claude tab holding a session takes a message, and
/// never in Bypass.
fn send_block_of(tab: &Tab) -> Option<SendBlock> {
    if *tab.agent() != AgentId::Claude {
        return Some(SendBlock::NotClaude);
    }
    if tab.live().is_none() {
        return Some(SendBlock::NotStarted);
    }
    (tab.mode() == SessionModeChoice::Bypass).then_some(SendBlock::Bypass)
}

/// How `tab`'s session ended, when it has: failed (a failed start or tripwire, or a session the agent reported
/// unavailable), or gone (closed, or taken out of the tab).
fn ended_how(tab: &Tab) -> Option<EndedHow> {
    match &tab.backend {
        TabBackend::Live(backend) => match backend.projection().status {
            agent::ProjectionStatus::Unavailable { .. } => Some(EndedHow::Failed),
            agent::ProjectionStatus::Closed { .. } => Some(EndedHow::Lost),
            agent::ProjectionStatus::Starting | agent::ProjectionStatus::Running => None,
        },
        TabBackend::Failed { .. } => Some(EndedHow::Failed),
        TabBackend::NotStarted | TabBackend::Starting(_) | TabBackend::AwaitingTrust(_) => Some(EndedHow::Lost),
    }
}

impl TabSet {
    /// The tab named `key` in this window, if it is here.
    pub fn tab_by_key(&self, key: &RowKey) -> Option<TabId> {
        self.tabs.iter().find(|t| t.row_key() == key).map(|t| t.id)
    }

    /// Registers the observer; its first frame is a snapshot, built by the first take.
    pub fn observe(&mut self, key: &RowKey) -> Result<ObserverId, ObserveError> {
        let tab = self
            .tabs
            .iter()
            .find(|t| t.row_key() == key)
            .ok_or(ObserveError::NoSuchTab)?;
        if tab.live().is_none() || self.moving.contains(&tab.id.0) {
            return Err(ObserveError::NotLive);
        }
        Ok(self
            .observers
            .add(tab.id, key.clone(), tab.backend_generation, send_block_of(tab)))
    }

    /// Asks for the turns of the session `generation` before `before_turn`, as one `Page` at the observer's next take.
    /// Refused when that session is not the one the observer follows now, or while a snapshot is owed to it (a first
    /// one, a new session's, the conversation a `/clear` started): its turn numbers are those of the snapshot it has
    /// not had yet (`WorkChanged`); while a page is still owed
    /// (`Outstanding`: one at a time, so a peer asking again and again holds at most one page), and for a turn the
    /// session does not have (`NoSuchTurn`).
    pub fn observer_more(&mut self, id: ObserverId, generation: u64, before_turn: u64) -> Result<(), MoreRefusal> {
        let observer = self
            .observers
            .by_id
            .get_mut(&id.0)
            .filter(|o| !o.gone)
            .ok_or(MoreRefusal::WorkChanged)?;
        let tab = self
            .tabs
            .iter()
            .find(|t| t.id == observer.tab)
            .filter(|t| !self.moving.contains(&t.id.0))
            .ok_or(MoreRefusal::WorkChanged)?;
        if observer.needs_snapshot || observer.generation != generation || tab.backend_generation != generation {
            return Err(MoreRefusal::WorkChanged);
        }
        let backend = tab.live().ok_or(MoreRefusal::WorkChanged)?;
        if observer.owed_page.is_some() {
            return Err(MoreRefusal::Outstanding);
        }
        let count = Turns::of(&backend.projection()).count();
        if before_turn == 0 || before_turn > count {
            return Err(MoreRefusal::NoSuchTurn);
        }
        observer.owed_page = Some(before_turn);
        Ok(())
    }

    /// What a phone is shown of the tab `key` before its snapshot: the tab as the strip shows it (without the desk's
    /// question drafts), a trust or bypass question it waits on (what it says, never what answers it), the live notices,
    /// whether a `!` line runs, the window's colours and why a phone may not send to it. `root_note` and `theme` are the
    /// window's. `None` for a tab this window does not hold.
    pub fn remote_greeting(
        &self,
        key: &RowKey,
        root_note: Option<&str>,
        theme: &crate::theme::ThemeTokens,
    ) -> Option<Greeting> {
        let tab = self.tabs.iter().find(|t| t.row_key() == key)?;
        let strip = serialize_tabs_for_js(tab.id, &[tab.view(tab.resumable(self.kind))], self.default_mode);
        let mut entry = serde_json::from_str::<serde_json::Value>(&strip).ok()?["tabs"][0].take();
        if let Some(fields) = entry.as_object_mut() {
            fields.remove("questionDraft");
        }
        let prompt = match (&tab.backend, &self.pending_bypass) {
            (TabBackend::AwaitingTrust(_), _) => Some(PendingPrompt::Trust),
            (_, Some(plan)) if matches!(plan.scope, BypassScope::Tab(id) if id == tab.id) => {
                Some(PendingPrompt::Bypass {
                    lines: plan.lines.clone(),
                })
            }
            _ => None,
        };
        let notices = tab
            .live()
            .map(|backend| {
                let projection = backend.projection();
                [
                    &projection.api_retry,
                    &projection.rate_limit_warning,
                    &projection.notification,
                ]
                .into_iter()
                .flatten()
                .filter_map(|notice| notice.content.clone())
                .collect()
            })
            .unwrap_or_default();
        Some(Greeting {
            tab: entry,
            prompt,
            root_note: root_note.map(str::to_owned),
            notices,
            shell_running: self.shell_line_runs(tab.id),
            theme: remote_theme(theme),
            send_block: send_block_of(tab),
        })
    }

    /// Forgets the observer and whatever waits for it; an id it does not know changes nothing.
    pub fn unobserve(&mut self, id: ObserverId) {
        self.observers.by_id.remove(&id.0);
    }

    /// How many observers are registered.
    pub fn observer_count(&self) -> usize {
        self.observers.by_id.len()
    }

    /// Where an owner that cannot reach the set when it goes leaves its observers' ids; the next pump forgets them.
    pub fn observer_reaper(&self) -> ObserverReaper {
        self.observers.reaper.clone()
    }

    /// Observers with something to take: frames queued, a snapshot or a page owed that can be built now, an `Ended`
    /// or a `SendBlock` to tell.
    pub fn observers_with_frames(&self) -> Vec<ObserverId> {
        self.observers
            .by_id
            .iter()
            .filter(|(_, observer)| {
                if !observer.queue.is_empty() {
                    return true;
                }
                let Some(tab) = (!observer.gone)
                    .then(|| self.tabs.iter().find(|t| t.id == observer.tab))
                    .flatten()
                    .filter(|t| !self.moving.contains(&t.id.0))
                else {
                    return false;
                };
                let renewed = observer.generation != tab.backend_generation;
                let live = tab.live();
                // A page that waits for events is taken with them: the tick that sends them lists the observer.
                let page_now = observer.owed_page.zip(live).is_some_and(|(before_turn, backend)| {
                    let projection = backend.projection();
                    !page_waits(&projection, &Turns::of(&projection), before_turn, observer.watermark)
                });
                (live.is_some() && (observer.needs_snapshot || renewed || page_now))
                    || (observer.ended_of != Some(tab.backend_generation) && ended_how(tab).is_some())
                    || send_block_of(tab) != observer.send_block
            })
            .map(|(id, _)| ObserverId(*id))
            .collect()
    }

    /// Up to `max_bytes` of this observer's frames in order (a single frame larger than `max_bytes` is returned alone).
    /// Builds the pending snapshot here, never during the drain. An observer whose `Gone` is taken is forgotten.
    pub fn take_observer_frames(&mut self, id: ObserverId, max_bytes: usize) -> Vec<ObserverFrame> {
        let Some(observer) = self.observers.by_id.get_mut(&id.0) else {
            return Vec::new();
        };
        let mut frames = Vec::new();
        let mut used = 0usize;
        let moving = &self.moving;
        let tab = self
            .tabs
            .iter()
            .find(|t| t.id == observer.tab)
            .filter(|t| !moving.contains(&t.id.0));
        let tab = tab.filter(|_| !observer.gone);
        if let Some(tab) = tab {
            // Frames of a backend that is no longer the tab's are not this session's history, and its page is not
            // this session's either. Whether its end was told is kept by generation (`ended_of`), never reset here:
            // a new session that fails before its snapshot can be built is told once, not at every take.
            if observer.generation != tab.backend_generation {
                observer.want_snapshot();
                observer.owed_page = None;
            }
            if observer.needs_snapshot {
                if let Some(built) = observer_snapshot(tab) {
                    #[cfg(test)]
                    {
                        self.observers.snapshots_built += 1;
                    }
                    observer.needs_snapshot = false;
                    observer.generation = tab.backend_generation;
                    observer.watermark = built.through;
                    observer.turn_time_sent = built.turn_time;
                    used = built.json.len();
                    frames.push(ObserverFrame::Snapshot {
                        key: observer.key.clone(),
                        through: built.through,
                        work: built.work,
                        first_turn: built.first_turn,
                        cut: built.cut,
                        json: Arc::from(built.json),
                    });
                }
            }
        }
        while let Some(front) = observer.queue.front() {
            let bytes = front.bytes();
            if !frames.is_empty() && used.saturating_add(bytes) > max_bytes {
                break;
            }
            used = used.saturating_add(bytes);
            observer.queued_bytes -= bytes;
            frames.extend(observer.queue.pop_front());
        }
        // What is read from the tab itself comes after every frame that was queued before it.
        if let (Some(tab), true) = (tab, observer.queue.is_empty()) {
            if let Some(before_turn) = observer.owed_page.filter(|_| !observer.needs_snapshot) {
                // Within what the take has left; a page alone may be larger, as one frame always may.
                let limit = if frames.is_empty() {
                    max_bytes
                } else {
                    max_bytes.saturating_sub(used)
                };
                match observer_page(tab, before_turn, limit.min(SNAPSHOT_CEILING_BYTES), observer.watermark) {
                    PageRead::Page(_, _, _, json)
                        if !frames.is_empty() && used.saturating_add(json.len()) > max_bytes => {}
                    PageRead::Later => {}
                    PageRead::Page(mark, from_turn, through_turn, json) => {
                        observer.owed_page = None;
                        frames.push(ObserverFrame::Page {
                            key: observer.key.clone(),
                            mark,
                            from_turn,
                            through_turn,
                            json: Arc::from(json),
                        });
                    }
                    // No session to page through: its `Ended` says so.
                    PageRead::None => observer.owed_page = None,
                }
            }
            if observer.ended_of != Some(tab.backend_generation) {
                if let Some(how) = ended_how(tab) {
                    observer.ended_of = Some(tab.backend_generation);
                    frames.push(ObserverFrame::Ended {
                        key: observer.key.clone(),
                        how,
                    });
                }
            }
            let block = send_block_of(tab);
            if block != observer.send_block {
                observer.send_block = block;
                frames.push(ObserverFrame::SendBlock {
                    key: observer.key.clone(),
                    block,
                });
            }
        }
        if observer.gone && observer.queue.is_empty() {
            self.observers.by_id.remove(&id.0);
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;

    mod feed_tests;
    use crate::test_providers::RecordingProvider;
    use agent::{AgentConversation, AgentDomainEvent};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn workspace(label: &str) -> PathBuf {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir(label);
        std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
        dir
    }

    fn conversation(dir: &Path) -> (Arc<RecordingProvider>, TabBackend) {
        let provider = Arc::new(RecordingProvider::default());
        let conversation =
            AgentConversation::create(provider.clone(), dir, agent::setting_sources::ProjectTrust::Untrusted).unwrap();
        (
            provider,
            TabBackend::Live(AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation: Box::new(conversation),
            }),
        )
    }

    fn until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn opened(dir: &Path) -> AgentDomainEvent {
        AgentDomainEvent::SessionOpened {
            session_id: "fake-session".into(),
            provider_session_id: "sess-observed".into(),
            model: "m".into(),
            cwd: dir.to_string_lossy().into_owned(),
        }
    }

    fn started(turn: &str) -> AgentDomainEvent {
        AgentDomainEvent::TurnStarted { turn_id: turn.into() }
    }

    fn delta(text: &str) -> AgentDomainEvent {
        AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: agent::ContentKind::Text,
            text: text.into(),
        }
    }

    fn revision(set: &TabSet, tab: TabId) -> u64 {
        set.get(tab).unwrap().live().unwrap().projection().last_revision
    }

    /// Waits until `tab`'s conversation has folded through `through`, without pumping.
    fn folded(set: &TabSet, tab: TabId, through: u64) {
        until("the events to fold", || revision(set, tab) >= through);
    }

    /// A window whose active tab holds a live conversation over a recording provider, with its session open
    /// (revision 1) and that event delivered.
    fn opened_tab(label: &str) -> (PathBuf, TabSet, TabId, Arc<RecordingProvider>) {
        let dir = workspace(label);
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = set.active();
        let (provider, backend) = conversation(&dir);
        set.get_mut(tab).unwrap().set_backend(backend);
        provider.queue(opened(&dir));
        folded(&set, tab, 1);
        set.pump(&dir, true);
        (dir, set, tab, provider)
    }

    fn shut_down(set: &mut TabSet) {
        for tab in set.take_all() {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    fn key(set: &TabSet, tab: TabId) -> RowKey {
        set.get(tab).unwrap().row_key().clone()
    }

    fn json(text: &str) -> serde_json::Value {
        serde_json::from_str(text).expect("a frame's JSON parses")
    }

    fn revisions_of(payload: &serde_json::Value) -> Vec<u64> {
        payload["revisions"]
            .as_array()
            .map(|list| list.iter().map(|r| r.as_u64().unwrap()).collect())
            .unwrap_or_default()
    }

    /// What a phone holds after applying frames in order; every frame is checked against what came before.
    #[derive(Default)]
    struct Phone {
        through: Option<u64>,
        snapshots: Vec<u64>,
        revisions: Vec<u64>,
        gone: Option<GoneWhy>,
        frames: usize,
        /// The frames not about revisions (pages, ends, send blocks), in order.
        others: Vec<ObserverFrame>,
    }

    impl Phone {
        fn apply(&mut self, frames: Vec<ObserverFrame>) {
            for frame in frames {
                assert!(self.gone.is_none(), "a frame after gone: {frame:?}");
                self.frames += 1;
                match frame {
                    ObserverFrame::Snapshot {
                        through, json: text, ..
                    } => {
                        let payload = json(&text);
                        assert_eq!(payload["kind"], "snapshot");
                        assert_eq!(payload["throughRevision"], through, "the frame and its JSON agree");
                        self.through = Some(through);
                        self.snapshots.push(through);
                    }
                    ObserverFrame::Events {
                        from,
                        through,
                        json: text,
                        ..
                    } => {
                        assert_eq!(
                            self.through,
                            Some(from),
                            "an events frame starts where the last frame ended"
                        );
                        assert!(through >= from);
                        let payload = json(&text);
                        assert_eq!(payload["kind"], "events");
                        assert_eq!(payload["fromRevision"], from);
                        assert_eq!(payload["throughRevision"], through);
                        let revisions = revisions_of(&payload);
                        assert_eq!(
                            revisions,
                            (from + 1..=through).collect::<Vec<_>>(),
                            "every revision after the last frame, once, in order"
                        );
                        assert_eq!(payload["events"].as_array().unwrap().len(), revisions.len());
                        self.revisions.extend(revisions);
                        self.through = Some(through);
                    }
                    ObserverFrame::Gone { why, .. } => self.gone = Some(why),
                    other @ (ObserverFrame::Page { .. }
                    | ObserverFrame::Ended { .. }
                    | ObserverFrame::SendBlock { .. }) => self.others.push(other),
                }
            }
        }
    }

    fn frame_bytes(frame: &ObserverFrame) -> usize {
        frame.bytes()
    }

    #[test]
    fn first_take_is_a_snapshot_through_the_projection_revision() {
        let (dir, mut set, tab, provider) = opened_tab("observers-first-take");
        provider.queue_all(vec![started("t1"), delta("hello from the observed tab")]);
        folded(&set, tab, 3);
        set.pump(&dir, true);
        let key = key(&set, tab);
        assert_eq!(set.tab_by_key(&key), Some(tab));
        let id = set.observe(&key).expect("a live tab is observed");
        assert_eq!(
            set.observers_with_frames(),
            [id],
            "its first snapshot waits to be taken"
        );
        let frames = set.take_observer_frames(id, usize::MAX);
        assert_eq!(frames.len(), 1, "{frames:?}");
        let ObserverFrame::Snapshot {
            key: framed,
            through,
            json: text,
            ..
        } = &frames[0]
        else {
            panic!("the first frame is a snapshot: {frames:?}");
        };
        assert_eq!(framed, &key);
        assert_eq!(*through, 3);
        let payload = json(text);
        assert_eq!(payload["throughRevision"], 3);
        assert!(
            payload["state"]["transcript"]
                .to_string()
                .contains("hello from the observed tab"),
            "the snapshot holds the tab's transcript: {payload}"
        );
        assert!(set.take_observer_frames(id, usize::MAX).is_empty());
        assert!(set.observers_with_frames().is_empty());
        shut_down(&mut set);
    }

    #[test]
    fn observe_refuses_an_unknown_key_and_a_tab_without_a_session() {
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = set.active();
        assert_eq!(set.observe(&RowKey::new()).err(), Some(ObserveError::NoSuchTab));
        assert_eq!(set.tab_by_key(&RowKey::new()), None);
        let key = key(&set, tab);
        assert_eq!(set.observe(&key).err(), Some(ObserveError::NotLive));
    }

    #[test]
    fn observe_does_not_touch_desk_delivery_state() {
        let (_dir, mut set, tab, provider) = opened_tab("observers-desk-untouched");
        let steered = Queued {
            state: QueuedState::Steered {
                client_id: "m1".into(),
                sent_at_ms: 2,
            },
            ..Queued::held("x", "x".into(), 1)
        };
        {
            let t = set.get_mut(tab).unwrap();
            t.session.turn_times_unsent = 2;
            t.session.stale = true;
            t.session.queue = vec![steered.clone()];
        }
        // Read by the agent and folded, not yet pumped: the desk's own snapshot would settle the queue now.
        provider.queue_all(vec![
            started("t1"),
            AgentDomainEvent::UserMessageDelivered {
                client_message_id: "m1".into(),
                turn_id: "t1".into(),
                text: "x".into(),
            },
        ]);
        folded(&set, tab, 3);
        assert!(set
            .get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .steer_outcome("m1")
            .is_some());

        let id = set.observe(&key(&set, tab)).unwrap();
        let frames = set.take_observer_frames(id, usize::MAX);
        assert!(
            matches!(frames[..], [ObserverFrame::Snapshot { through: 3, .. }]),
            "{frames:?}"
        );

        let t = set.get_mut(tab).unwrap();
        assert_eq!(t.session.turn_times_unsent, 2);
        assert!(t.session.stale);
        assert_eq!(t.session.queue, [steered], "the queue is settled only by the desk");
        assert!(t.session.turn_clock.is_none(), "the tab's clock is the pump's to stamp");
        assert_eq!(t.live_mut().unwrap().take_ui_snapshot_revision(), None);
        shut_down(&mut set);
    }

    #[test]
    fn subscribe_mid_stream_leaves_desk_whole_and_phone_contiguous() {
        let dir = workspace("observers-mid-stream");
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = set.active();
        let (provider, backend) = conversation(&dir);
        set.get_mut(tab).unwrap().set_backend(backend);
        let mut desk = Vec::new();
        let mut phone = Phone::default();
        let mut id = None;
        for n in 1..=10u64 {
            provider.queue(match n {
                1 => opened(&dir),
                2 => started("t1"),
                n => delta(&format!("chunk {n}")),
            });
            folded(&set, tab, n);
            if n == 4 {
                // Folded and not yet delivered to the desk: the snapshot carries it, and the desk's next
                // envelope still does.
                let observer = set.observe(&key(&set, tab)).unwrap();
                phone.apply(set.take_observer_frames(observer, usize::MAX));
                id = Some(observer);
            }
            let out = set.pump_views(&dir, &|_| true);
            if let Some(payload) = out.active_payload {
                let payload = json(&payload);
                assert_eq!(payload["kind"], "events", "the desk is never sent a snapshot here");
                desk.extend(revisions_of(&payload));
            }
            if let Some(observer) = id {
                phone.apply(set.take_observer_frames(observer, usize::MAX));
            }
        }
        assert_eq!(
            desk,
            (1..=10).collect::<Vec<_>>(),
            "the desk sees every revision once, in order"
        );
        assert_eq!(phone.snapshots, [4]);
        assert_eq!(phone.revisions, (5..=10).collect::<Vec<_>>());
        shut_down(&mut set);
    }

    #[test]
    fn observer_gets_background_tab_events() {
        let (dir, mut set, tab, provider) = opened_tab("observers-background");
        let other = set.open(AgentId::Claude);
        assert_ne!(set.active(), tab);
        let id = set.observe(&key(&set, tab)).unwrap();
        let mut phone = Phone::default();
        phone.apply(set.take_observer_frames(id, usize::MAX));
        provider.queue_all(vec![started("t1"), delta("unseen")]);
        folded(&set, tab, 3);
        let out = set.pump(&dir, true);
        assert!(
            out.active_payload.is_none(),
            "the background tab sends the desk nothing"
        );
        assert!(out.view_payloads.is_empty());
        assert!(set.get(tab).unwrap().session.stale);
        phone.apply(set.take_observer_frames(id, usize::MAX));
        assert_eq!(phone.snapshots, [1]);
        assert_eq!(phone.revisions, [2, 3]);
        let _ = other;
        shut_down(&mut set);
    }

    #[test]
    fn backend_resync_gives_observers_a_fresh_snapshot() {
        let (dir, mut set, tab, provider) = opened_tab("observers-resync");
        let id = set.observe(&key(&set, tab)).unwrap();
        let mut phone = Phone::default();
        phone.apply(set.take_observer_frames(id, usize::MAX));
        let mut batch = vec![started("t1")];
        batch.extend((0..(agent::UI_EVENT_QUEUE_CAPACITY + 20)).map(|i| delta(&format!("chunk {i}"))));
        let last = 1 + batch.len() as u64;
        provider.queue_all(batch);
        folded(&set, tab, last);
        let out = set.pump(&dir, true);
        let payload = json(&out.active_payload.expect("a payload"));
        assert_eq!(payload["kind"], "snapshot", "the overflow became a resync");
        let frames = set.take_observer_frames(id, usize::MAX);
        assert!(
            matches!(&frames[..], [ObserverFrame::Snapshot { through, .. }] if *through == last),
            "{frames:?}"
        );
        phone.apply(frames);
        provider.queue(delta("after"));
        folded(&set, tab, last + 1);
        set.pump(&dir, true);
        phone.apply(set.take_observer_frames(id, usize::MAX));
        assert_eq!(phone.revisions, [last + 1]);
        shut_down(&mut set);
    }

    #[test]
    fn backend_replaced_gives_observers_a_fresh_snapshot() {
        let (dir, mut set, tab, provider) = opened_tab("observers-replaced");
        provider.queue_all(vec![started("t1"), delta("a"), delta("b"), delta("c")]);
        folded(&set, tab, 5);
        set.pump(&dir, true);
        let id = set.observe(&key(&set, tab)).unwrap();
        let mut phone = Phone::default();
        phone.apply(set.take_observer_frames(id, usize::MAX));
        assert_eq!(phone.snapshots, [5]);

        let (second, backend) = conversation(&dir);
        if let TabBackend::Live(mut old) = set.get_mut(tab).unwrap().set_backend(backend) {
            old.shutdown();
        }
        second.queue_all(vec![opened(&dir), started("t1"), delta("the second session")]);
        folded(&set, tab, 3);
        set.pump(&dir, true);
        let frames = set.take_observer_frames(id, usize::MAX);
        let [ObserverFrame::Snapshot {
            through: 3, json: text, ..
        }] = &frames[..]
        else {
            panic!("the new session's snapshot, never its events against the old watermark: {frames:?}");
        };
        assert!(json(text)["state"]["transcript"]
            .to_string()
            .contains("the second session"));
        phone.apply(frames);
        second.queue(delta("more"));
        folded(&set, tab, 4);
        set.pump(&dir, true);
        phone.apply(set.take_observer_frames(id, usize::MAX));
        assert_eq!(phone.snapshots, [5, 3]);
        assert_eq!(phone.revisions, [4]);
        shut_down(&mut set);
    }

    #[test]
    fn frames_name_the_session_and_the_unseen_stretch() {
        let (dir, mut set, tab, provider) = opened_tab("observers-work-mark");
        let generation = set.get(tab).unwrap().backend_generation();
        let id = set.observe(&key(&set, tab)).unwrap();
        let frames = set.take_observer_frames(id, usize::MAX);
        let [ObserverFrame::Snapshot { work, .. }] = &frames[..] else {
            panic!("a snapshot first: {frames:?}");
        };
        assert_eq!(
            *work,
            WorkMark {
                generation,
                unseen: Some(0)
            },
            "no stretch yet, counted through the snapshot"
        );

        let unseen_ended = AgentDomainEvent::TurnCompleted {
            turn_id: agent::NO_OPEN_TURN_ID.into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        };
        let mut marks = Vec::new();
        for (events, through) in [
            (vec![AgentDomainEvent::CliWorkUnseen], 2),
            (vec![AgentDomainEvent::CliWorkUnseen], 3),
            (vec![unseen_ended, AgentDomainEvent::CliWorkUnseen], 5),
        ] {
            provider.queue_all(events);
            folded(&set, tab, through);
            set.pump(&dir, true);
            for frame in set.take_observer_frames(id, usize::MAX) {
                if let ObserverFrame::Events { work, .. } = frame {
                    marks.push(work);
                }
            }
        }
        assert_eq!(
            marks,
            [1, 1, 2].map(|n| WorkMark {
                generation,
                unseen: Some(n)
            }),
            "each events frame names the stretch running once it is applied"
        );

        // Folded but not yet counted by a tick: a snapshot cannot name the stretch.
        provider.queue(AgentDomainEvent::CliWorkUnseen);
        folded(&set, tab, 6);
        let late = set.observe(&key(&set, tab)).unwrap();
        let frames = set.take_observer_frames(late, usize::MAX);
        let [ObserverFrame::Snapshot { through: 6, work, .. }] = &frames[..] else {
            panic!("a snapshot through the fold: {frames:?}");
        };
        assert_eq!(work.unseen, None);
        set.unobserve(late);

        let (second, backend) = conversation(&dir);
        if let TabBackend::Live(mut old) = set.get_mut(tab).unwrap().set_backend(backend) {
            old.shutdown();
        }
        second.queue(opened(&dir));
        folded(&set, tab, 1);
        set.pump(&dir, true);
        let frames = set.take_observer_frames(id, usize::MAX);
        let [ObserverFrame::Snapshot { work, .. }] = &frames[..] else {
            panic!("the new session's snapshot: {frames:?}");
        };
        assert_eq!(
            work.generation,
            generation + 1,
            "the new session is named apart from the old"
        );
        assert_eq!(work.unseen, Some(2), "the stretch number never goes down in the tab");
        shut_down(&mut set);
    }

    #[test]
    fn overflow_drops_the_queue_and_snapshots_lazily() {
        let (dir, mut set, tab, provider) = opened_tab("observers-overflow");
        let id = set.observe(&key(&set, tab)).unwrap();
        let mut phone = Phone::default();
        phone.apply(set.take_observer_frames(id, usize::MAX));
        assert_eq!(set.observers.snapshots_built, 1);
        provider.queue(started("t1"));
        folded(&set, tab, 2);
        set.pump(&dir, true);
        let big = "x".repeat(64 * 1024);
        let mut through = 2;
        let mut peak = 0;
        // Ten events of 64 KiB a pump, a frame under the cap, never taken: two of them pass it.
        for _ in 0..5 {
            provider.queue_all((0..10).map(|_| delta(&big)).collect());
            through += 10;
            folded(&set, tab, through);
            set.pump(&dir, true);
            peak = peak.max(set.observers.by_id[&id.0].queued_bytes);
        }
        assert!(peak > 0, "frames queue while the observer does not take");
        let observer = &set.observers.by_id[&id.0];
        assert!(observer.queue.is_empty(), "the overflow dropped the queue");
        assert_eq!(observer.queued_bytes, 0);
        assert!(observer.queued_bytes <= OBSERVER_QUEUE_CAP_BYTES);
        assert_eq!(set.observers.snapshots_built, 1, "no snapshot is built by a pump");
        assert_eq!(set.observers_with_frames(), [id]);
        let frames = set.take_observer_frames(id, usize::MAX);
        assert!(
            matches!(&frames[..], [ObserverFrame::Snapshot { through: t, .. }] if *t == through),
            "{frames:?}"
        );
        assert_eq!(set.observers.snapshots_built, 2);
        phone.apply(frames);
        shut_down(&mut set);
    }

    #[test]
    fn observer_queue_cap_is_one_mebibyte() {
        assert_eq!(OBSERVER_QUEUE_CAP_BYTES, 1024 * 1024);
        let (dir, mut set, tab, provider) = opened_tab("observers-cap-1mib");
        let id = set.observe(&key(&set, tab)).unwrap();
        set.take_observer_frames(id, usize::MAX);
        provider.queue(started("t1"));
        let mut through = 2;
        folded(&set, tab, through);
        set.pump(&dir, true);
        // One frame of a 60 KiB event a pump, never taken: the queue grows until the next would pass 1 MiB.
        let text = "z".repeat(60 * 1024);
        let mut peak = 0;
        let mut dropped = false;
        for _ in 0..40 {
            provider.queue(delta(&text));
            through += 1;
            folded(&set, tab, through);
            set.pump(&dir, true);
            let queued = set.observers.by_id[&id.0].queued_bytes;
            if queued < peak {
                dropped = true;
                break;
            }
            peak = queued;
        }
        assert!(
            dropped,
            "the queue was dropped once it would pass the cap (peak {peak})"
        );
        assert!(peak <= OBSERVER_QUEUE_CAP_BYTES, "never above the cap: {peak}");
        assert!(
            peak > OBSERVER_QUEUE_CAP_BYTES - 64 * 1024,
            "dropped only when one more frame would not fit: {peak}"
        );
        let frames = set.take_observer_frames(id, usize::MAX);
        assert!(
            matches!(&frames[..], [ObserverFrame::Snapshot { .. }, ..]),
            "a snapshot follows the drop: {frames:?}"
        );
        shut_down(&mut set);
    }

    #[test]
    fn observers_handed_to_the_reaper_are_forgotten_at_the_next_pump() {
        let (dir, mut set, tab, _provider) = opened_tab("observers-reaper");
        let first = set.observe(&key(&set, tab)).unwrap();
        let second = set.observe(&key(&set, tab)).unwrap();
        assert_eq!(set.observer_count(), 2);
        let reaper = set.observer_reaper();
        // Handed over from another thread, as a socket's owner going away may.
        std::thread::spawn(move || reaper.hand_over([first])).join().unwrap();
        assert_eq!(set.observer_count(), 2, "nothing is forgotten before a pump");
        set.pump(&dir, true);
        assert_eq!(set.observer_count(), 1, "the handed-over observer is forgotten");
        assert_eq!(set.observers_with_frames(), [second]);
        set.observer_reaper().hand_over([first, second, ObserverId(999)]);
        set.pump(&dir, true);
        assert_eq!(set.observer_count(), 0, "an id already gone changes nothing");
        shut_down(&mut set);
    }

    /// A small fixed-seed generator, so a failure replays.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[test]
    fn random_take_budgets_never_deliver_a_gap() {
        let (dir, mut set, tab, provider) = opened_tab("observers-random-budgets");
        let id = set.observe(&key(&set, tab)).unwrap();
        let mut phone = Phone::default();
        let mut rng = Lcg(0x5eed_0b5e_77e5);
        const BUDGETS: [usize; 8] = [0, 1, 64, 700, 4_000, 60_000, 1_500_000, usize::MAX];
        provider.queue(started("t1"));
        let mut through = 2;
        folded(&set, tab, through);
        set.pump(&dir, true);
        let mut sent = 0u64;
        while sent < 500 {
            let n = (1 + rng.below(8)).min(500 - sent);
            let events: Vec<_> = (0..n)
                .map(|i| {
                    let at = sent + i;
                    // A stretch with megabyte events and no takes passes the cap at least once.
                    let size = if (200..300).contains(&at) && at % 10 == 5 {
                        1024 * 1024
                    } else {
                        1 + rng.below(2_000) as usize
                    };
                    delta(&"y".repeat(size))
                })
                .collect();
            provider.queue_all(events);
            sent += n;
            through += n;
            folded(&set, tab, through);
            set.pump(&dir, true);
            if (200..300).contains(&sent) {
                continue;
            }
            for _ in 0..rng.below(3) {
                let budget = BUDGETS[rng.below(BUDGETS.len() as u64) as usize];
                let frames = set.take_observer_frames(id, budget);
                if frames.len() > 1 {
                    let bytes: usize = frames.iter().map(frame_bytes).sum();
                    assert!(
                        bytes <= budget,
                        "{} frames of {bytes} bytes over a budget of {budget}",
                        frames.len()
                    );
                }
                phone.apply(frames);
            }
        }
        set.pump(&dir, true);
        loop {
            let frames = set.take_observer_frames(id, 4_000);
            if frames.is_empty() {
                break;
            }
            phone.apply(frames);
        }
        assert_eq!(phone.through, Some(through), "the phone ends where the tab does");
        assert!(
            phone.snapshots.len() >= 2,
            "the overflow was exercised: {:?}",
            phone.snapshots
        );
        shut_down(&mut set);
    }

    #[test]
    fn closed_tab_sends_gone() {
        let (dir, mut set, tab, provider) = opened_tab("observers-closed");
        let id = set.observe(&key(&set, tab)).unwrap();
        let mut phone = Phone::default();
        phone.apply(set.take_observer_frames(id, usize::MAX));
        provider.queue_all(vec![started("t1"), delta("last words")]);
        folded(&set, tab, 3);
        set.pump(&dir, true);
        let removed = set.remove(tab).expect("the tab closes");
        if let Some(retired) = removed.retired {
            retired.into_backend().shutdown();
        }
        assert_eq!(set.observers_with_frames(), [id]);
        phone.apply(set.take_observer_frames(id, usize::MAX));
        assert_eq!(
            phone.revisions,
            [2, 3],
            "what was queued before the close still arrives"
        );
        assert_eq!(phone.gone, Some(GoneWhy::Closed));
        assert!(set.take_observer_frames(id, usize::MAX).is_empty());
        assert!(set.observers_with_frames().is_empty());
        assert!(
            set.observers.by_id.is_empty(),
            "a gone observer is forgotten once taken"
        );
        set.pump(&dir, true);
        assert!(set.take_observer_frames(id, usize::MAX).is_empty());
        shut_down(&mut set);
    }

    #[test]
    fn moved_tab_sends_gone() {
        let (dir, mut set, tab, provider) = opened_tab("observers-moved");
        let key = key(&set, tab);
        let id = set.observe(&key).unwrap();
        let mut phone = Phone::default();
        phone.apply(set.take_observer_frames(id, usize::MAX));
        // Folded and never pumped: the freeze's last pump delivers them.
        provider.queue_all(vec![started("t1"), delta("before the move")]);
        folded(&set, tab, 3);
        let deadline = Instant::now() + Duration::from_secs(10);
        let frozen = loop {
            match set.freeze_for_move(tab) {
                Err(MoveRefusedWhy::Folding) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1))
                }
                other => break other.expect("the tab freezes"),
            }
        };
        assert_eq!(
            set.observe(&key).err(),
            Some(ObserveError::NotLive),
            "a tab being moved is not observed"
        );
        phone.apply(set.take_observer_frames(id, usize::MAX));
        assert_eq!(phone.revisions, [2, 3], "the last pump's events come before gone");
        assert_eq!(phone.gone, Some(GoneWhy::Moved));
        // Given back: nothing more reaches the observer that was told the tab left.
        drop(frozen);
        set.unfreeze(tab);
        set.pump(&dir, true);
        assert!(set.take_observer_frames(id, usize::MAX).is_empty());
        assert!(set.observers_with_frames().is_empty());
        shut_down(&mut set);
    }

    #[test]
    fn two_observers_share_one_serialization() {
        let (dir, mut set, tab, provider) = opened_tab("observers-shared");
        let key = key(&set, tab);
        let first = set.observe(&key).unwrap();
        let second = set.observe(&key).unwrap();
        assert_ne!(first, second);
        set.take_observer_frames(first, usize::MAX);
        set.take_observer_frames(second, usize::MAX);
        provider.queue_all(vec![started("t1"), delta("shared")]);
        folded(&set, tab, 3);
        set.pump(&dir, true);
        let a = set.take_observer_frames(first, usize::MAX);
        let b = set.take_observer_frames(second, usize::MAX);
        let ([ObserverFrame::Events { json: a, .. }], [ObserverFrame::Events { json: b, .. }]) = (&a[..], &b[..])
        else {
            panic!("one events frame each: {a:?} {b:?}");
        };
        assert!(Arc::ptr_eq(a, b), "one serialization for both");
        set.unobserve(first);
        assert!(set.take_observer_frames(first, usize::MAX).is_empty());
        shut_down(&mut set);
    }

    /// A conversation of `turns` turns, each a prompt, some text, one tool call with a result and a completion.
    fn transcript_of(turns: usize) -> Vec<AgentDomainEvent> {
        let text = "The parser keeps one cursor per line and hands each token to the folding pass. ".repeat(5);
        let output = "src/lib.rs:120: fn fold(&mut self, event: Event) -> Revision {\n".repeat(24);
        let mut events = Vec::with_capacity(turns * 6);
        for n in 0..turns {
            let turn = format!("turn-{n}");
            events.push(AgentDomainEvent::TurnStarted { turn_id: turn.clone() });
            events.push(AgentDomainEvent::UserPromptSubmitted {
                text: format!("Please look at the folding pass, step {n}, and tell me what it does."),
            });
            events.push(AgentDomainEvent::ContentDelta {
                turn_id: turn.clone(),
                kind: agent::ContentKind::Text,
                text: text.clone(),
            });
            events.push(AgentDomainEvent::ToolCallStarted {
                turn_id: turn.clone(),
                tool_use_id: format!("call-{n}"),
                name: "Bash".into(),
                input: serde_json::json!({"command": format!("rg -n fold src/ | head -n 24 # {n}")}),
            });
            events.push(AgentDomainEvent::ToolCallCompleted {
                turn_id: turn.clone(),
                tool_use_id: format!("call-{n}"),
                content: serde_json::json!(output),
                is_error: false,
            });
            events.push(AgentDomainEvent::TurnCompleted {
                turn_id: turn,
                outcome: agent::TurnOutcome::Completed,
                result_text: String::new(),
                stop_reason: None,
                usage: None,
                detail: Default::default(),
            });
        }
        events
    }

    /// Prints the size and serialization time of an observer snapshot for 10, 100 and 1000 turns; run with
    /// `--ignored --nocapture`. It asserts only that ten times the turns cost less than fifteen times the time.
    #[test]
    #[ignore = "a measurement: prints figures for the remote-control decisions"]
    fn measure_snapshot_cost() {
        let mut measured = Vec::new();
        for turns in [10usize, 100, 1000] {
            let (dir, mut set, tab, provider) = opened_tab(&format!("observers-measure-{turns}"));
            let events = transcript_of(turns);
            let through = 1 + events.len() as u64;
            provider.queue_all(events);
            folded(&set, tab, through);
            set.pump(&dir, true);
            let mut times = Vec::new();
            let mut bytes = 0;
            for _ in 0..9 {
                let started = Instant::now();
                let text = observer_snapshot(set.get(tab).unwrap())
                    .expect("a live tab has a snapshot")
                    .json;
                times.push(started.elapsed());
                bytes = text.len();
            }
            times.sort();
            let median = times[times.len() / 2];
            println!(
                "{{\"turns\":{turns},\"snapshot_bytes\":{bytes},\"serialize_us_median\":{},\"serialize_us_min\":{}}}",
                median.as_micros(),
                times[0].as_micros()
            );
            measured.push((turns, median));
            shut_down(&mut set);
        }
        let (hundred, thousand) = (measured[1].1, measured[2].1);
        assert!(
            thousand < hundred * 15,
            "ten times the turns cost more than fifteen times the time: {hundred:?} -> {thousand:?}"
        );
    }

    #[test]
    fn changed_editor_cards_give_observers_a_fresh_snapshot() {
        let (_dir, mut set, tab, _provider) = opened_tab("observers-editor-cards");
        let id = set.observe(&key(&set, tab)).unwrap();
        let mut phone = Phone::default();
        phone.apply(set.take_observer_frames(id, usize::MAX));
        let card = crate::editor_tools::cards::EditorCardWire {
            id: "card-1".to_owned(),
            seq: 7,
            tool: "open_file".to_owned(),
            tool_use_id: Some("toolu_open".to_owned()),
            title: "Show a file in your editor?".to_owned(),
            body: Vec::new(),
            proposal: None,
        };

        set.set_editor_cards(tab, vec![card.clone()]);
        assert_eq!(
            set.observers_with_frames(),
            vec![id],
            "the new card is owed to the observer"
        );
        let frames = set.take_observer_frames(id, usize::MAX);
        let [ObserverFrame::Snapshot { json: text, .. }] = &frames[..] else {
            panic!("one snapshot: {frames:?}");
        };
        assert_eq!(
            json(text)["state"]["editorCards"][0]["id"],
            "card-1",
            "the card the broker holds"
        );
        phone.apply(frames);

        set.set_editor_cards(tab, vec![card]);
        assert!(
            set.observers_with_frames().is_empty(),
            "the same cards again owe nothing"
        );

        set.set_editor_cards(tab, Vec::new());
        let frames = set.take_observer_frames(id, usize::MAX);
        let [ObserverFrame::Snapshot { json: text, .. }] = &frames[..] else {
            panic!("one snapshot: {frames:?}");
        };
        let payload = json(text);
        assert!(payload["state"].is_object(), "{payload}");
        assert!(
            payload["state"]["editorCards"].is_null(),
            "the answered card is gone from it"
        );
        phone.apply(frames);
        shut_down(&mut set);
    }

    #[test]
    fn a_phone_message_read_in_events_a_resync_dropped_keeps_its_sender() {
        let (dir, mut set, tab, provider) = opened_tab("observers-resync-sender");
        let pixel = crate::agent_bridge::SentFrom {
            device_id: "d1".into(),
            name: "Pixel".into(),
        };
        set.get_mut(tab)
            .unwrap()
            .notes
            .note_remote_sender("m1".into(), pixel.clone());
        let mut batch = vec![
            started("t1"),
            AgentDomainEvent::UserMessageDelivered {
                client_message_id: "m1".into(),
                turn_id: "t1".into(),
                text: "and the tests".into(),
            },
        ];
        batch.extend((0..(agent::UI_EVENT_QUEUE_CAPACITY + 20)).map(|i| delta(&format!("chunk {i}"))));
        let last = 1 + batch.len() as u64;
        provider.queue_all(batch);
        folded(&set, tab, last);
        let out = set.pump(&dir, true);
        let payload = json(&out.active_payload.expect("a payload"));
        assert_eq!(payload["kind"], "snapshot", "the overflow became a resync");
        let prompt = &payload["state"]["userPrompts"][0];
        assert_eq!(prompt["text"], "and the tests");
        let seq = prompt["seq"].as_u64().unwrap();
        assert_eq!(
            payload["state"]["sentFrom"][seq.to_string()],
            serde_json::json!({"deviceId": "d1", "name": "Pixel"}),
            "the desk's snapshot names the phone"
        );
        assert_eq!(set.get(tab).unwrap().notes.sent_from.get(&seq), Some(&pixel));
        assert!(set.get(tab).unwrap().notes.remote_senders.is_empty(), "taken once");

        let id = set.observe(&key(&set, tab)).unwrap();
        let frames = set.take_observer_frames(id, usize::MAX);
        let Some(ObserverFrame::Snapshot { json: text, .. }) = frames.first() else {
            panic!("a snapshot first: {frames:?}");
        };
        assert_eq!(json(text)["state"]["sentFrom"][seq.to_string()]["name"], "Pixel");
        shut_down(&mut set);
    }

    /// A tab with one finished turn and one running turn whose reply has begun, delivered to the desk and to an
    /// observer that has taken its snapshot.
    fn observed_running_turn(label: &str) -> (PathBuf, TabSet, TabId, Arc<RecordingProvider>, ObserverId, Phone) {
        let (dir, mut set, tab, provider) = opened_tab(label);
        provider.queue_all(vec![
            started("t0"),
            AgentDomainEvent::UserPromptSubmitted { text: "first".into() },
            AgentDomainEvent::TurnCompleted {
                turn_id: "t0".into(),
                outcome: agent::TurnOutcome::Completed,
                result_text: String::new(),
                stop_reason: None,
                usage: None,
                detail: Default::default(),
            },
            started("t1"),
            AgentDomainEvent::UserPromptSubmitted { text: "second".into() },
            delta("Hello"),
        ]);
        folded(&set, tab, 7);
        set.pump(&dir, true);
        let id = set.observe(&key(&set, tab)).unwrap();
        let mut phone = Phone::default();
        phone.apply(set.take_observer_frames(id, usize::MAX));
        assert_eq!(phone.snapshots, [7]);
        (dir, set, tab, provider, id, phone)
    }

    fn page_through(frames: &[ObserverFrame]) -> Option<(u64, u64, u64)> {
        frames.iter().find_map(|frame| match frame {
            ObserverFrame::Page {
                from_turn,
                through_turn,
                json: text,
                ..
            } => Some((
                *from_turn,
                *through_turn,
                json(text)["throughRevision"].as_u64().unwrap(),
            )),
            _ => None,
        })
    }

    #[test]
    fn a_page_of_the_running_turn_waits_until_the_observer_stands_where_it_was_read() {
        let (dir, mut set, tab, provider, id, mut phone) = observed_running_turn("observers-page-running");
        let generation = set.get(tab).unwrap().backend_generation();
        set.observer_more(id, generation, 2).unwrap();
        // Folded and not yet pumped: the page would hold a delta the observer is still to be sent.
        provider.queue(delta(" phone"));
        folded(&set, tab, 8);
        assert!(
            set.observers_with_frames().is_empty(),
            "nothing to take until the delta is sent"
        );
        let frames = set.take_observer_frames(id, usize::MAX);
        assert_eq!(page_through(&frames), None, "no page ahead of the observer: {frames:?}");
        phone.apply(frames);
        set.pump(&dir, true);
        let frames = set.take_observer_frames(id, usize::MAX);
        assert_eq!(
            page_through(&frames),
            Some((0, 1, 8)),
            "the page once the delta was sent: {frames:?}"
        );
        phone.apply(frames);
        assert_eq!(phone.revisions, [8]);
        shut_down(&mut set);
    }

    #[test]
    fn a_page_of_finished_turns_does_not_wait_for_the_running_turn() {
        let (dir, mut set, tab, provider, id, _phone) = observed_running_turn("observers-page-finished");
        let generation = set.get(tab).unwrap().backend_generation();
        set.observer_more(id, generation, 1).unwrap();
        provider.queue(delta(" phone"));
        folded(&set, tab, 8);
        let frames = set.take_observer_frames(id, usize::MAX);
        assert_eq!(
            page_through(&frames).map(|(from, through, _)| (from, through)),
            Some((0, 0))
        );
        set.pump(&dir, true);
        shut_down(&mut set);
    }
}
