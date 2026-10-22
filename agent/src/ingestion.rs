// agent/src/ingestion.rs
//! Background ingestion: provider events are folded into canonical state on their own thread, so a
//! stalled UI cannot make them pile up as raw events.
//!
//! ## The problem this exists to solve
//!
//! Provider event throughput and UI repaint throughput are different quantities, and the old design
//! collapsed them. `AgentConversation::pump()` both drained the provider AND folded the projection,
//! and the only thing that called it was a 33ms GTK timer. So a UI that stopped pumping stopped the
//! reducer too, and every event produced meanwhile queued up raw. Measured over a deliberate 20s
//! stall: the client-side queue grew to 253 raw events while transport lag stayed flat at 1-3ms and
//! the sidecar's own memory did not move. Nothing was lost -- but "a slow UI loses no events" was
//! being bought with "store arbitrarily many raw provider events", which is not a property a
//! product can keep.
//!
//! ## Three separate notions of progress, deliberately not collapsed
//!
//! | progress | owned by | advances when |
//! | --- | --- | --- |
//! | provider sequence consumed | the provider's watch task (`SequenceTracker`) | an event arrives off the wire |
//! | semantic projection updated | this module's ingestion thread | an event is folded |
//! | UI last rendered | the GTK tick | the panel reads a delivery |
//!
//! The replay cursor tracks the FIRST of those and nothing else. That matters concretely: a frozen
//! UI must never drag the reconnect cursor backwards, because a cursor that lags behind events
//! already safely folded would force a replay that is not needed -- or, once the events had aged out
//! of the provider's ring, a wholly unnecessary EVENT_GAP. A stalled window would manufacture the
//! exact data loss this protocol spent a round making impossible.
//!
//! ## What is bounded, and what is not
//!
//! Bounded: the queue of events waiting to be handed to the UI (`UI_EVENT_QUEUE_CAPACITY`). When it
//! overflows, the queue is dropped and the UI is told to resynchronise from a snapshot instead --
//! complete by construction, since the projection holds the whole conversation.
//!
//! Not bounded, and correctly so: the projection itself. It grows with the CONVERSATION -- messages,
//! tool calls, pending permissions -- which is the thing the user is actually looking at. That is
//! the distinction this module is for: memory follows semantic state, not raw event count.

use crate::lease::SessionLease;
use crate::move_seed::{ChatMoveCore, HandoffRefused, MoveSeed};
use crate::projection::{AgentDomainEvent, AgentSessionProjection};
use crate::provider::EventSignal;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// How many folded events may wait for the UI before it is told to resynchronise instead.
///
/// Sized to comfortably cover a normal repaint interval at real streaming rates (a partial-streamed
/// turn produces roughly 400 events over several seconds, and the GTK tick is 33ms), so the cheap
/// incremental path carries every healthy case and the snapshot path only engages on a genuine
/// stall. It is a threshold for switching strategy, not a buffer to tune: a bigger number would not
/// make anything more correct, it would only delay the switch and hold more raw events.
pub const UI_EVENT_QUEUE_CAPACITY: usize = 256;

/// How often the ingestion thread drains a provider that raises no [`EventSignal`]. Short enough that
/// the provider's own internal queue stays near-empty between drains, which is what keeps the
/// raw-event backlog bounded at BOTH layers rather than moving it one step upstream. A provider that
/// raises one is drained as soon as it does, and the thread sleeps while it is quiet.
const INGEST_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(5);

/// What the UI should do when it next wakes up.
#[derive(Debug)]
pub enum UiDelivery {
    /// Nothing has changed since the UI last looked.
    Nothing,
    /// Apply these events, in order. The cheap path, and the one every healthy tick takes.
    Events(Vec<AgentDomainEvent>),
    /// Too much happened while the UI was away. Take a fresh snapshot of the projection instead.
    ///
    /// Not a degraded mode: the projection is the authority, so a snapshot is by definition complete
    /// and current. What is lost is only the ability to animate the intervening steps, which a UI
    /// that was not on screen could not have animated anyway.
    Resync,
}

/// [`UiDelivery`] with each event's revision, read under the lock that folds the event.
/// The tag is the projection's `last_revision` immediately after that fold.
///
/// Events may be folded and queued between a caller's last drain and its next snapshot. Tags let
/// the caller skip queued events already present in that snapshot instead of applying them twice.
/// `AgentSessionProjection::apply` increments `last_revision` once per fold and nothing else changes
/// it, so a tag at or below the snapshot's revision is already included. The caller stores that
/// revision with `AgentConversation::note_ui_snapshot`, and the next drain takes it back. Keeping
/// it on the conversation prevents a revision from being reused for another session.
#[derive(Debug)]
pub enum RevisedDelivery {
    Nothing,
    /// In fold order, so the tags strictly increase.
    Events(Vec<(u64, AgentDomainEvent)>),
    Resync,
}

impl RevisedDelivery {
    /// The same delivery, for a caller that never reads a snapshot against it.
    pub fn without_revisions(self) -> UiDelivery {
        match self {
            RevisedDelivery::Nothing => UiDelivery::Nothing,
            RevisedDelivery::Events(events) => UiDelivery::Events(events.into_iter().map(|(_, event)| event).collect()),
            RevisedDelivery::Resync => UiDelivery::Resync,
        }
    }
}

/// Counters for what ingestion is costing. Read by the slow-consumer tests, which need to prove
/// boundedness rather than merely observe that nothing crashed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestStats {
    /// Every provider event folded since this conversation started.
    pub events_ingested: u64,
    /// Events currently waiting to be handed to the UI.
    pub ui_backlog: usize,
    /// The deepest that queue has ever been.
    pub max_ui_backlog: usize,
    /// How many times the queue overflowed and the UI was asked to resynchronise.
    pub resyncs: u64,
    /// Whether a resync is owed right now.
    pub resync_pending: bool,
}

/// The provider's verdict on a resume, as folded.
///
/// Recorded here rather than left for the caller to catch on the event stream: the UI delivery queue
/// is not a place to read one-off facts from, and draining it to look for a verdict would steal
/// events the UI has not seen yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeOutcomeRecord {
    pub requested: String,
    pub status: crate::ResumeStatus,
    pub attached: Option<String>,
    pub forked: bool,
    pub detail: Option<String>,
}

/// Immutable facts the ingestion thread needs in order to adopt a provider session when one appears.
#[derive(Clone)]
struct AdoptionContext {
    /// The name the lease and the record are keyed by (`ExternalProvider::name()`, or `claude`).
    provider_name: &'static str,
    conversation_id: String,
    canonical_cwd: String,
    provider_advertises_resume: bool,
}

struct IngestState {
    projection: AgentSessionProjection,
    provider_session_id: Option<String>,
    /// Held for as long as this conversation drives the provider session. Taken on the ingestion
    /// thread rather than the UI thread, which is also where it belongs: acquiring it touches the
    /// filesystem, and doing that on a GTK tick is exactly the kind of thing that causes the stalls
    /// this module exists to tolerate.
    lease: Option<SessionLease>,
    /// Each with the projection revision its fold produced (see [`RevisedDelivery`]).
    pending_ui: VecDeque<(u64, AgentDomainEvent)>,
    resync_pending: bool,
    resume_outcome: Option<ResumeOutcomeRecord>,
    stats: IngestStats,
    /// The session's title, noted by the UI thread and written by this one: with the record when
    /// adoption finds it here (the normal order, since the Agent SDK reports Claude's id at the
    /// start of the first turn), else into the record once `record_written` says it exists. See
    /// `ConversationIngest::note_title`.
    pending_title: Option<String>,
    /// A rename not yet written; the outer `Option` says whether there is one.
    pending_name: Option<Option<String>>,
    /// Whether this session's record is on disk. A known session id does not guarantee this:
    /// adoption sets the id under the lock, then writes the record after releasing it.
    /// A title written during that gap can be lost because no record exists yet.
    /// This starts as `true` for a resume because `resume()` writes the record before returning.
    record_written: bool,
    /// Whether this conversation has produced something worth storing since its history was last
    /// written. Set by `fold` at a turn boundary and nowhere else -- see `flush_history`.
    history_dirty: bool,
    /// The last `SessionOpened` folded and every resume verdict, unavailability or close folded after
    /// it (all of them while none has opened the session), for the conversation's whole life: what
    /// sets the session's status when another window rebuilds this projection. A handful at most,
    /// since each `SessionOpened` starts it again.
    lifecycle_since_open: Vec<AgentDomainEvent>,
    /// The running turn, from the fold that started it until the projection is settled again: what a
    /// move of this conversation in the middle of a turn carries. Freed at the turn's end, so it is
    /// bounded by one turn.
    running_turn: Option<RunningTurn>,
    /// The provider's own sequence number of the last event it had taken in by the last pump whose
    /// events were folded ([`AgentProvider::pumped_through`]); 0 while it has reported none.
    ///
    /// [`AgentProvider::pumped_through`]: crate::provider::AgentProvider::pumped_through
    folded_through: u64,
}

/// A running turn as another window needs it to rebuild this projection: the settled export and the
/// lifecycle list as they were just before the turn's first fold, and every event folded since.
struct RunningTurn {
    seed: MoveSeed,
    lifecycle: Vec<AgentDomainEvent>,
    events: Vec<AgentDomainEvent>,
}

/// Whether folding `event` into a settled projection may start a turn's worth of live state (a turn,
/// an open reply or thought, a card) or is the prompt that begins one. No wildcard arm: a new event
/// has to say which it is, and saying `true` for one that is not only costs a copy.
fn may_start_a_turn(event: &AgentDomainEvent) -> bool {
    match event {
        AgentDomainEvent::UserPromptSubmitted { .. }
        | AgentDomainEvent::UserMessageDelivered { .. }
        | AgentDomainEvent::TurnStarted { .. }
        | AgentDomainEvent::ContentDelta { .. }
        | AgentDomainEvent::ToolCallStarted { .. }
        | AgentDomainEvent::PermissionRequested { .. }
        | AgentDomainEvent::MoveSeeded(_) => true,
        AgentDomainEvent::SessionOpened { .. }
        | AgentDomainEvent::UserMessageDropped { .. }
        | AgentDomainEvent::AssistantMessageBoundary { .. }
        | AgentDomainEvent::ToolCallCompleted { .. }
        | AgentDomainEvent::PermissionResolved { .. }
        | AgentDomainEvent::TurnCompleted { .. }
        | AgentDomainEvent::SessionUnavailable { .. }
        | AgentDomainEvent::ResumeOutcome { .. }
        | AgentDomainEvent::SessionClosed { .. }
        | AgentDomainEvent::PermissionModeChanged { .. }
        | AgentDomainEvent::UngatedCliMode { .. }
        | AgentDomainEvent::CliPermissionMode { .. }
        | AgentDomainEvent::PermissionDenied { .. }
        | AgentDomainEvent::PolicyMismatch { .. }
        | AgentDomainEvent::BackgroundCalls { .. }
        | AgentDomainEvent::CommandsAvailable { .. }
        | AgentDomainEvent::AccountLimits { .. }
        | AgentDomainEvent::ToolInputChanged { .. } => false,
    }
}

/// Keeps `lifecycle` as "the last `SessionOpened` and every status change folded after it", or every
/// status change folded so far while no `SessionOpened` has been.
fn note_lifecycle(lifecycle: &mut Vec<AgentDomainEvent>, event: &AgentDomainEvent) {
    match event {
        AgentDomainEvent::SessionOpened { .. } => {
            lifecycle.clear();
            lifecycle.push(event.clone());
        }
        // Kept before any `SessionOpened` too: a session lost before it opened is unavailable, and
        // without its loss another window would rebuild it as still starting.
        AgentDomainEvent::ResumeOutcome { .. }
        | AgentDomainEvent::SessionUnavailable { .. }
        | AgentDomainEvent::SessionClosed { .. } => lifecycle.push(event.clone()),
        _ => {}
    }
}

/// The ingest's state another window continues from, beside the projection's records.
pub(crate) struct IngestHandoff {
    pub(crate) provider_session_id: Option<String>,
    pub(crate) resume_outcome: Option<ResumeOutcomeRecord>,
    pub(crate) stats: IngestStats,
    pub(crate) pending_title: Option<String>,
    pub(crate) pending_name: Option<Option<String>>,
    pub(crate) record_written: bool,
    pub(crate) history_dirty: bool,
}

/// What an adopted conversation's ingest starts from: the rebuilt projection, the move's records it
/// was rebuilt from, the ingest's own state from the source, and the lease.
pub(crate) struct MovedIngest {
    pub(crate) projection: AgentSessionProjection,
    pub(crate) core: ChatMoveCore,
    pub(crate) state: IngestHandoff,
    pub(crate) lease: Option<SessionLease>,
}

impl IngestState {
    /// Folds `event`, then the event the projection derives from it (a changed tool input: the
    /// comparison is made once, here, and the derived event is folded and delivered like any other,
    /// so the panel learns of it even when the host answered the request it came with).
    fn fold(&mut self, event: AgentDomainEvent) {
        let derived = self.projection.derived_after(&event);
        self.fold_one(event);
        if let Some(derived) = derived {
            self.fold_one(derived);
        }
    }

    fn fold_one(&mut self, event: AgentDomainEvent) {
        // The reducer sees every event, in order, always. Coalescing happens strictly AFTER this --
        // collapsing "assistant partial / tool start / tool result / assistant partial" into one blob
        // before the reducer would destroy boundaries the projection is defined in terms of.
        if let AgentDomainEvent::ResumeOutcome {
            requested_provider_session_id,
            status,
            attached_provider_session_id,
            forked,
            detail,
        } = &event
        {
            self.resume_outcome = Some(ResumeOutcomeRecord {
                requested: requested_provider_session_id.clone(),
                status: *status,
                attached: attached_provider_session_id.clone(),
                forked: *forked,
                detail: detail.clone(),
            });
        }
        // A turn's first fold: the settled export and the lifecycle list as they stand now are what
        // another window rebuilds this turn on. Taken only from a settled projection, since the export
        // carries no live declaration; an unsettled one with no record open stays unmovable until it
        // settles.
        if self.running_turn.is_none() && may_start_a_turn(&event) && self.projection.is_settled() {
            self.running_turn = Some(RunningTurn {
                seed: self.projection.export_settled(),
                lifecycle: self.lifecycle_since_open.clone(),
                events: Vec::new(),
            });
        }
        self.projection.apply(&event);
        self.stats.events_ingested += 1;
        note_lifecycle(&mut self.lifecycle_since_open, &event);
        if let Some(turn) = &mut self.running_turn {
            turn.events.push(event.clone());
            // A prompt leaves the projection settled and is still the turn's start.
            let prompt = matches!(event, AgentDomainEvent::UserPromptSubmitted { .. });
            if self.projection.is_settled() && !prompt {
                self.running_turn = None;
            }
        }
        // A turn boundary, and only a turn boundary: that is the quiet moment. `apply` has just
        // closed `assistant_message_open`, so nothing half-written can be stored, and one write per
        // turn is three orders of magnitude fewer than one per event -- a partial-streamed reply is
        // 400+ events and each write is two fsyncs.
        if matches!(
            event,
            AgentDomainEvent::TurnCompleted { .. }
                | AgentDomainEvent::SessionClosed { .. }
                | AgentDomainEvent::SessionUnavailable { .. }
        ) {
            self.history_dirty = true;
        }

        if self.resync_pending {
            // Already owed a snapshot; queueing more events would be holding raw data the UI is
            // never going to read.
            return;
        }
        self.pending_ui.push_back((self.projection.last_revision, event));
        self.stats.ui_backlog = self.pending_ui.len();
        self.stats.max_ui_backlog = self.stats.max_ui_backlog.max(self.pending_ui.len());
        if self.pending_ui.len() > UI_EVENT_QUEUE_CAPACITY {
            self.pending_ui.clear();
            self.pending_ui.shrink_to_fit();
            self.resync_pending = true;
            self.stats.resyncs += 1;
            self.stats.ui_backlog = 0;
        }
    }

    fn take_delivery(&mut self) -> UiDelivery {
        self.take_revised_delivery().without_revisions()
    }

    /// The records a move carries, taken while folding is stopped. Refused while folded events wait
    /// for the page, and for an idle conversation holding a live declaration no fold would rebuild.
    ///
    /// Destructures `Self` without a rest pattern: a field added to the ingest does not compile here
    /// until it is placed in the move or named as not moved.
    fn take_move_record(&self) -> Result<(ChatMoveCore, IngestHandoff), HandoffRefused> {
        let IngestState {
            projection,
            provider_session_id,
            // Moves as a descriptor beside the record, never inside it.
            lease: _,
            pending_ui,
            resync_pending,
            resume_outcome,
            stats,
            pending_title,
            pending_name,
            record_written,
            history_dirty,
            lifecycle_since_open,
            running_turn,
            folded_through,
        } = self;
        // Emptied by the source's last pump; the destination's first delivery is a snapshot.
        if !pending_ui.is_empty() || *resync_pending {
            return Err(HandoffRefused::Undelivered);
        }
        let core = match running_turn {
            Some(turn) => ChatMoveCore {
                lifecycle: turn.lifecycle.clone(),
                seed: turn.seed.clone(),
                running_turn: turn.events.clone(),
                folded_through: *folded_through,
            },
            None if projection.is_settled() => ChatMoveCore {
                lifecycle: lifecycle_since_open.clone(),
                seed: projection.export_settled(),
                running_turn: Vec::new(),
                folded_through: *folded_through,
            },
            None => return Err(HandoffRefused::Unsettled),
        };
        let handoff = IngestHandoff {
            provider_session_id: provider_session_id.clone(),
            resume_outcome: resume_outcome.clone(),
            stats: *stats,
            pending_title: pending_title.clone(),
            pending_name: pending_name.clone(),
            record_written: *record_written,
            history_dirty: *history_dirty,
        };
        Ok((core, handoff))
    }

    /// An adopted conversation's state: the source's, with the running turn and the lifecycle list
    /// rebuilt from the move's records so a later move of it carries them again.
    fn moved(moved: MovedIngest) -> Self {
        let MovedIngest {
            projection,
            core,
            state,
            lease,
        } = moved;
        let ChatMoveCore {
            lifecycle,
            seed,
            running_turn,
            folded_through,
        } = core;
        let mut lifecycle_since_open = Vec::new();
        for event in lifecycle.iter().chain(running_turn.iter()) {
            note_lifecycle(&mut lifecycle_since_open, event);
        }
        let running_turn = (!running_turn.is_empty()).then_some(RunningTurn {
            seed,
            lifecycle,
            events: running_turn,
        });
        let IngestHandoff {
            provider_session_id,
            resume_outcome,
            stats,
            pending_title,
            pending_name,
            record_written,
            history_dirty,
        } = state;
        IngestState {
            projection,
            provider_session_id,
            lease,
            pending_ui: VecDeque::new(),
            resync_pending: false,
            resume_outcome,
            stats: IngestStats {
                ui_backlog: 0,
                resync_pending: false,
                ..stats
            },
            pending_title,
            pending_name,
            record_written,
            history_dirty,
            lifecycle_since_open,
            running_turn,
            folded_through,
        }
    }

    fn take_revised_delivery(&mut self) -> RevisedDelivery {
        if self.resync_pending {
            self.resync_pending = false;
            return RevisedDelivery::Resync;
        }
        if self.pending_ui.is_empty() {
            return RevisedDelivery::Nothing;
        }
        let events: Vec<(u64, AgentDomainEvent)> = self.pending_ui.drain(..).collect();
        self.stats.ui_backlog = 0;
        RevisedDelivery::Events(events)
    }
}

/// What the UI hands a conversation so it is told when there is something to take: called after events
/// were folded (on the ingestion thread, or on the caller's for a local fold). It must be cheap and must
/// never block or call back into the conversation.
pub type UiWake = Arc<dyn Fn() + Send + Sync>;

/// The UI's wake, shared between the conversation and its thread; empty until the UI installs one.
type UiWakeSlot = Arc<Mutex<Option<UiWake>>>;

/// Calls the installed wake, if any, outside the slot's lock.
fn wake_ui(slot: &Mutex<Option<UiWake>>) {
    let wake = slot.lock().unwrap().clone();
    if let Some(wake) = wake {
        wake();
    }
}

/// A conversation's ingestion thread and the state it owns.
pub struct ConversationIngest {
    state: Arc<Mutex<IngestState>>,
    stop: Arc<AtomicBool>,
    /// Raised by the provider when it queued events, and here when something the thread acts on
    /// was noted (a title, a name, a local fold, the stop).
    wake: Arc<EventSignal>,
    /// Told after every fold, so the UI need not poll for deliveries.
    ui_wake: UiWakeSlot,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Set while a move takes this conversation's records: the thread takes nothing from the
    /// provider and writes nothing until it is cleared.
    frozen: Arc<AtomicBool>,
    /// Held by the thread from before it takes a batch from the provider until that batch is folded
    /// and adopted and the writes after it are done. Taking it is how a freeze knows no batch is in
    /// hand.
    pump_gate: Arc<Mutex<()>>,
    /// What the lease and the record are keyed by, and whether the provider resumes: the ingest's
    /// fixed identity, which a move hands on.
    provider_name: &'static str,
    provider_advertises_resume: bool,
}

/// Borrowed access to the canonical projection.
///
/// A guard rather than a clone: the UI reads this on every tick (up to 30 a second), and cloning a conversation's
/// whole transcript that often would reintroduce, as CPU, the cost this module just removed as
/// memory.
pub struct ProjectionGuard<'a>(MutexGuard<'a, IngestState>);

impl std::ops::Deref for ProjectionGuard<'_> {
    type Target = AgentSessionProjection;
    fn deref(&self) -> &AgentSessionProjection {
        &self.0.projection
    }
}

impl ConversationIngest {
    /// Starts folding provider events immediately, on their own thread.
    ///
    /// `provider` is shared rather than owned because the UI thread keeps issuing commands
    /// (`send_turn`, `interrupt`, `resolve_permission`) against the same provider while this thread
    /// drains it. Every `AgentProvider` method takes `&self`, so the two need no lock between them --
    /// which matters: a mutex here would let a blocking `send_turn` stall ingestion for the duration
    /// of a gRPC round trip, quietly recreating the coupling this module exists to break.
    pub(crate) fn start(
        provider: Arc<dyn crate::provider::AgentProvider + Send + Sync>,
        provider_name: &'static str,
        conversation_id: String,
        canonical_cwd: String,
        provider_advertises_resume: bool,
        // A resume takes its lease and learns its Claude id BEFORE the provider is called at all, so
        // that path hands both in rather than waiting for ingestion to discover them. Seeding
        // `provider_session_id` is also what stops the first `SessionOpened` of a resumed session
        // from being mistaken for a fresh adoption and re-taking a lease already held.
        initial_provider_session_id: Option<String>,
        initial_lease: Option<SessionLease>,
        // What this conversation already said, restored from disk -- `AgentSessionProjection::default()`
        // for a fresh session, a seeded one for a resume (`history::load`). It is a PARAMETER, and
        // that is the whole ordering argument: this function spawns the thread that starts pumping
        // the provider, so the only moment at which "no live event has been folded yet" is certain
        // is before it is called. Seeding here therefore makes every live `seq` greater than every
        // historical one by construction -- no renumbering, no offset, nothing for the frontend's
        // `throughRevision` seeding to change.
        seed: AgentSessionProjection,
    ) -> Self {
        let record_written = initial_provider_session_id.is_some();
        let state = IngestState {
            projection: seed,
            provider_session_id: initial_provider_session_id,
            lease: initial_lease,
            pending_ui: VecDeque::new(),
            resync_pending: false,
            resume_outcome: None,
            stats: IngestStats::default(),
            pending_title: None,
            pending_name: None,
            record_written,
            history_dirty: false,
            lifecycle_since_open: Vec::new(),
            running_turn: None,
            folded_through: 0,
        };
        let context = AdoptionContext {
            provider_name,
            conversation_id,
            canonical_cwd,
            provider_advertises_resume,
        };
        Self::spawn(provider, state, context)
    }

    /// Continues a conversation another window held, from the state its move carried: the rebuilt
    /// projection, the source ingest's own state and the lease. Folding starts at once, from the
    /// adopted provider.
    pub(crate) fn start_moved(
        provider: Arc<dyn crate::provider::AgentProvider + Send + Sync>,
        provider_name: &'static str,
        conversation_id: String,
        canonical_cwd: String,
        provider_advertises_resume: bool,
        moved: MovedIngest,
    ) -> Self {
        let context = AdoptionContext {
            provider_name,
            conversation_id,
            canonical_cwd,
            provider_advertises_resume,
        };
        Self::spawn(provider, IngestState::moved(moved), context)
    }

    fn spawn(
        provider: Arc<dyn crate::provider::AgentProvider + Send + Sync>,
        state: IngestState,
        context: AdoptionContext,
    ) -> Self {
        let state = Arc::new(Mutex::new(state));
        let stop = Arc::new(AtomicBool::new(false));
        let frozen = Arc::new(AtomicBool::new(false));
        let pump_gate = Arc::new(Mutex::new(()));
        let announced = provider.event_signal();
        let wake = announced.clone().unwrap_or_default();
        let provider_name = context.provider_name;
        let provider_advertises_resume = context.provider_advertises_resume;

        let ui_wake: UiWakeSlot = Arc::new(Mutex::new(None));
        let thread = {
            let state = Arc::clone(&state);
            let halt = Halt {
                stop: Arc::clone(&stop),
                frozen: Arc::clone(&frozen),
                pump_gate: Arc::clone(&pump_gate),
            };
            let ui_wake = Arc::clone(&ui_wake);
            std::thread::Builder::new()
                .name("eitri-agent-ingest".to_string())
                .spawn(move || ingest_loop(provider, state, halt, announced, context, ui_wake))
                .expect("spawning the agent ingestion thread")
        };

        Self {
            state,
            stop,
            wake,
            ui_wake,
            thread: Some(thread),
            frozen,
            pump_gate,
            provider_name,
            provider_advertises_resume,
        }
    }

    /// Stops taking events from the provider, and returns once no batch is in hand: every event the
    /// provider gave up before this returns is folded, and none after it until [`Self::unfreeze`].
    /// The thread writes nothing while frozen either (no title, name, history or record), so what a
    /// move carries is all there is. Local folds still happen: they are the caller's own.
    ///
    /// Waits for a batch being folded, and the writes after it, to finish: call it off the GTK
    /// thread.
    pub(crate) fn freeze(&self) {
        let _held = self.pump_gate.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        self.frozen.store(true, Ordering::SeqCst);
    }

    /// Lets the thread take events again, from where the provider kept them.
    pub(crate) fn unfreeze(&self) {
        self.frozen.store(false, Ordering::SeqCst);
        self.wake.raise();
    }

    /// The records a move carries and the ingest's own state, read under one lock (see
    /// `IngestState::take_move_record`).
    pub(crate) fn take_move_record(&self) -> Result<(ChatMoveCore, IngestHandoff), HandoffRefused> {
        self.state.lock().unwrap().take_move_record()
    }

    /// The ingest's fixed identity: the name the lease and the record are keyed by, and whether the
    /// provider resumes.
    pub(crate) fn identity(&self) -> (&'static str, bool) {
        (self.provider_name, self.provider_advertises_resume)
    }

    /// A second descriptor on the session lease's open file, for the other window: the lock is the
    /// open file's, so it stays held while either descriptor is open. `None` without a lease.
    pub(crate) fn lease_descriptor(&self) -> Option<std::io::Result<std::os::fd::OwnedFd>> {
        self.state
            .lock()
            .unwrap()
            .lease
            .as_ref()
            .map(SessionLease::descriptor_for_move)
    }

    /// Writes `pid` into the lease file as its owner, through the descriptor the lease holds. Nothing
    /// to do without a lease.
    pub(crate) fn rewrite_lease_owner(&self, pid: u32) -> std::io::Result<()> {
        match &self.state.lock().unwrap().lease {
            Some(lease) => lease.rewrite_owner_pid(pid),
            None => Ok(()),
        }
    }

    /// Holds the state lock for as long as the returned guard lives.
    ///
    /// **Nothing else on this type may be called while that guard is alive.** Every accessor below
    /// locks the same `std::sync::Mutex`, which is not reentrant, so a second call from the same
    /// thread blocks on a lock that thread already holds and never wakes. On 2026-09-15 that took
    /// the whole GTK main loop down from one call site in `shell` that read
    /// `provider_session_id()` inside this guard's scope -- no input, no window close, and the
    /// last frame still on screen, so it looked alive. Read what you need from the guard, or take
    /// the other values first.
    pub fn projection(&self) -> ProjectionGuard<'_> {
        ProjectionGuard(self.state.lock().unwrap())
    }

    /// Locks. See `projection`'s warning: never call this while holding a `ProjectionGuard`.
    pub fn provider_session_id(&self) -> Option<String> {
        self.state.lock().unwrap().provider_session_id.clone()
    }

    pub fn stats(&self) -> IngestStats {
        self.state.lock().unwrap().stats
    }

    /// Keeps `title` for the ingestion thread to write (`AgentConversation::note_title`). Nothing
    /// here touches the disk: the caller is the GTK main loop, and a record write is two fsyncs.
    /// The ingestion thread writes it with the record if adoption has not happened yet, or into the
    /// record once it exists (`flush_title`), so there is no moment in which a title can be handed
    /// over and then find nowhere to go. Only the first title is kept.
    pub(crate) fn note_title(&self, title: String) {
        self.state.lock().unwrap().pending_title.get_or_insert(title);
        self.wake.raise();
    }

    /// Keeps a rename (`prefix ,`) for the ingestion thread to write. The last one noted wins,
    /// unlike `note_title`'s first-wins: `None` clears it. Nothing here touches the disk.
    pub(crate) fn note_name(&self, name: Option<String>) {
        self.state.lock().unwrap().pending_name = Some(name);
        self.wake.raise();
    }

    /// Installs the UI's wake: from here on every fold calls it. Called once more at once, so what was
    /// folded before the UI installed it is taken too.
    pub fn set_ui_wake(&self, wake: UiWake) {
        *self.ui_wake.lock().unwrap() = Some(wake);
        wake_ui(&self.ui_wake);
    }

    /// Whether a UI wake is installed: without one the UI has to poll for deliveries.
    pub fn has_ui_wake(&self) -> bool {
        self.ui_wake.lock().unwrap().is_some()
    }

    /// The provider's resume verdict, once it has stated one.
    pub fn resume_outcome(&self) -> Option<ResumeOutcomeRecord> {
        self.state.lock().unwrap().resume_outcome.clone()
    }

    /// Releases the session lease. Called last on shutdown, so the lease outlives the provider's own
    /// session teardown and no other client can take it while this one is still closing.
    pub(crate) fn release_lease(&self) {
        self.state.lock().unwrap().lease = None;
    }

    /// What the UI should apply. Called from the GTK tick.
    pub fn take_delivery(&self) -> UiDelivery {
        self.state.lock().unwrap().take_delivery()
    }

    /// [`Self::take_delivery`], with each event's fold revision ([`RevisedDelivery`]).
    pub fn take_revised_delivery(&self) -> RevisedDelivery {
        self.state.lock().unwrap().take_revised_delivery()
    }

    /// Folds an event this side produced rather than the provider.
    ///
    /// Two callers, and the rule is the same for both: only for facts THIS side owns. `shutdown`'s
    /// terminal events, which the provider can no longer be asked for; and the user's own prompt,
    /// which no provider reports back. Never for provider lifecycle state, which is the provider's
    /// to state.
    pub(crate) fn fold_locally(&self, event: AgentDomainEvent) {
        self.state.lock().unwrap().fold(event);
        // The fold may have marked the history for writing, which the thread does.
        self.wake.raise();
        wake_ui(&self.ui_wake);
    }

    /// Stops the ingestion thread and waits for it. Idempotent.
    pub(crate) fn stop(&mut self) {
        // Stored before the raise, which takes the signal's lock: the thread's wait reads the flag under
        // that lock, so it cannot miss it.
        self.stop.store(true, Ordering::SeqCst);
        self.wake.raise();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ConversationIngest {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What tells the ingestion thread to stop for good, or for a while.
struct Halt {
    stop: Arc<AtomicBool>,
    frozen: Arc<AtomicBool>,
    pump_gate: Arc<Mutex<()>>,
}

fn ingest_loop(
    provider: Arc<dyn crate::provider::AgentProvider + Send + Sync>,
    state: Arc<Mutex<IngestState>>,
    halt: Halt,
    // The provider's own signal, which `ConversationIngest` also raises; `None` when the provider
    // announces nothing and so has to be asked on a timer.
    announced: Option<Arc<EventSignal>>,
    context: AdoptionContext,
    // Told once a batch is folded (and once more after an adoption, which the UI reads too).
    ui_wake: UiWakeSlot,
) {
    let Halt {
        stop,
        frozen,
        pump_gate,
    } = halt;
    // The last position the provider reported, so an unchanged one costs no lock.
    let mut noted_through: Option<u64> = None;
    // Until the provider or this side raises the signal: an unfreeze raises it too, so a frozen
    // thread wakes when it is let go.
    let idle = |seen: Option<u64>| match (&announced, seen) {
        // The stop is looked at again under the signal's lock: one that landed after the check above
        // already raised the signal before `seen` was read, so the count alone would not end this
        // wait.
        (Some(signal), Some(seen)) => signal.wait_past_unless(seen, || stop.load(Ordering::SeqCst)),
        _ => std::thread::sleep(INGEST_POLL_INTERVAL),
    };
    while !stop.load(Ordering::SeqCst) {
        #[cfg(test)]
        idle_wakeups::between_the_check_and_the_look(&context.conversation_id);
        // Read before the pump: an event queued after the pump looked raises the count past this,
        // so the wait below ends at once instead of missing it.
        let seen = announced.as_ref().map(|signal| signal.seen());
        // Held until this batch is folded, adopted and written: a freeze waits for it, so it never
        // stops the thread with events taken from the provider and not yet folded.
        let batch = pump_gate.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if frozen.load(Ordering::SeqCst) {
            drop(batch);
            idle(seen);
            continue;
        }
        let events = provider.pump();
        // Asked under the same gate as the pump: a freeze, which waits for the gate, finds the
        // position of the last batch folded and of no later one.
        let through = provider
            .pumped_through()
            .filter(|through| Some(*through) != noted_through);
        noted_through = through.or(noted_through);
        // Before the idle check, so a title noted while the provider is quiet still gets written.
        flush_title(&state, &context);
        flush_name(&state, &context);
        // Likewise: the turn whose completion marked the history dirty is usually the last event of
        // a quiet stretch, so waiting for the next non-empty pump would delay every write.
        flush_history(&state, &context);
        if events.is_empty() {
            // A provider event can become no domain event, and the position still moves past it.
            if let Some(through) = through {
                state.lock().unwrap().folded_through = through;
            }
            drop(batch);
            idle(seen);
            continue;
        }
        // Adoption touches the filesystem, so it happens OUTSIDE the state lock -- holding it across
        // a `flock` and a JSON write would block every UI tick reading the projection for the
        // duration.
        let mut adopt: Option<String> = None;
        let mut title: Option<String> = None;
        let mut name: Option<Option<String>> = None;
        {
            let mut guard = state.lock().unwrap();
            if let Some(through) = through {
                guard.folded_through = through;
            }
            for event in events {
                if let AgentDomainEvent::SessionOpened {
                    provider_session_id, ..
                } = &event
                {
                    // Only the FIRST one. `SessionOpened` repeats -- the Agent SDK emits a
                    // system/init at the start of every turn, not once per session -- so adopting
                    // unconditionally would re-take a lease this conversation already holds, on
                    // every single turn.
                    if guard.provider_session_id.is_none() {
                        adopt = Some(provider_session_id.clone());
                        // A title noted before this point goes into the record adoption writes. One
                        // noted after it waits in `pending_title` until `record_written`, and
                        // `flush_title` writes it then.
                        title = guard.pending_title.take();
                        // Same shape for a rename noted before adoption.
                        name = guard.pending_name.take();
                    }
                    guard.provider_session_id = Some(provider_session_id.clone());
                }
                guard.fold(event);
            }
        }
        // After the lock is released: the UI takes the same lock when it is woken.
        wake_ui(&ui_wake);
        if let Some(provider_session_id) = adopt {
            let lease = adopt_provider_session(&context, &provider_session_id, title.as_deref(), name);
            {
                let mut guard = state.lock().unwrap();
                guard.lease = lease;
                guard.record_written = true;
            }
            wake_ui(&ui_wake);
        }
        drop(batch);
    }
}

/// Writes a title noted after this session's record was written -- into that record, on this
/// thread. A no-op until `record_written`, and whenever nothing is pending: one uncontended lock
/// per poll. Best-effort, like the record itself: a failure is a row without a title next time.
fn flush_title(state: &Mutex<IngestState>, context: &AdoptionContext) {
    let (title, provider_session_id) = {
        let mut guard = state.lock().unwrap();
        if !guard.record_written {
            return;
        }
        let Some(title) = guard.pending_title.take() else {
            return;
        };
        let Some(id) = guard.provider_session_id.clone() else {
            return;
        };
        (title, id)
    };
    if let Err(e) = crate::persistence::set_title_if_missing(&context.conversation_id, &provider_session_id, &title) {
        eprintln!("agent: could not record the title of session {provider_session_id}: {e}");
    }
}

/// Writes a rename noted after this session's record was written. The same two gates as
/// `flush_title`: the record is on disk, and the Claude id is known.
fn flush_name(state: &Mutex<IngestState>, context: &AdoptionContext) {
    let (name, provider_session_id) = {
        let mut guard = state.lock().unwrap();
        if !guard.record_written {
            return;
        }
        let Some(name) = guard.pending_name.take() else {
            return;
        };
        let Some(id) = guard.provider_session_id.clone() else {
            return;
        };
        (name, id)
    };
    if let Err(e) = crate::persistence::set_name(&context.conversation_id, &provider_session_id, name.as_deref()) {
        eprintln!("agent: could not record the name of session {provider_session_id}: {e}");
    }
}

/// Writes this session's history (A, `history::store`) -- on THIS thread, at a turn boundary, and
/// only when there is something new to write.
///
/// **Which thread this runs on is the whole point, and it is not a style preference.** The title
/// feature's first version wrote from the UI thread and argued from a lock that it was safe; a
/// review found the session id becomes visible before the record is on disk and reproduced the
/// title being dropped in that gap, 20 runs out of 20 (`record_written`'s own doc). A history write
/// is strictly worse on that thread than a title write was: it is two fsyncs and a serialization of
/// the whole conversation, landing on a 33ms GTK tick.
///
/// The lock is held only long enough to read the dirty flag and copy what the history keeps of the
/// three collections (`history_snapshot`: the newest items within the caps, not the whole session).
/// Serializing and writing happen after it is released.
///
/// Two gates, the same two `flush_title` has: the record must be on disk (a history file for a
/// session with no record would be an orphan from birth), and there must be a
/// `provider_session_id`, which is the file's name.
///
/// Best-effort. A failure is one line on stderr and a resume that falls back further; it must never
/// fail a live session. Precedent: `persist_record` and `flush_title`.
///
/// **What this deliberately does not do is flush at shutdown.** `AgentConversation::shutdown` stops
/// this thread FIRST and then folds its terminal events on the caller's thread, so the
/// `SessionClosed` it synthesizes never reaches this function -- writing it from there would put the
/// write back on the UI thread, which is the one thing this whole arrangement exists to prevent.
/// The cost is the design's own accepted one: a `kill -9`, a power loss or a close can lose the last
/// turn. A is the fallback; B normally covers it.
fn flush_history(state: &Mutex<IngestState>, context: &AdoptionContext) {
    let (provider_session_id, user_prompts, transcript, tool_calls) = {
        let mut guard = state.lock().unwrap();
        if !guard.history_dirty || !guard.record_written {
            return;
        }
        let Some(id) = guard.provider_session_id.clone() else {
            return;
        };
        // Cleared before the write, not after: a turn that completes while this one is writing must
        // mark the history dirty again rather than be swallowed by a later clear.
        guard.history_dirty = false;
        let (user_prompts, transcript, tool_calls) = history_snapshot(&guard.projection);
        (id, user_prompts, transcript, tool_calls)
    };
    let history = crate::history::StoredHistory::from_collections(
        context.conversation_id.clone(),
        provider_session_id.clone(),
        user_prompts,
        transcript,
        tool_calls,
    );
    #[cfg(test)]
    tests::note_history_writer_thread(&context.conversation_id);
    if let Err(e) = crate::history::store::save(&history) {
        eprintln!("agent: could not store the history of session {provider_session_id}: {e}");
        return;
    }
    // The turn changed the conversation, so the record's "last used" moves with it: the session picker
    // and the launch offer lead with the session used last, not the one opened last.
    if let Err(e) = crate::persistence::touch_updated_at(
        &context.conversation_id,
        &provider_session_id,
        &crate::conversation::epoch_millis(),
    ) {
        eprintln!("agent: could not update the record of session {provider_session_id}: {e}");
    }
}

/// What the stored history keeps of the conversation, copied: only what the history's caps keep,
/// since this runs under the lock every UI read of the conversation takes.
fn history_snapshot(
    projection: &AgentSessionProjection,
) -> (
    Vec<crate::UserPromptRecord>,
    Vec<crate::TranscriptMessage>,
    Vec<crate::ToolCallRecord>,
) {
    crate::history::store::newest_that_fit(&projection.user_prompts, &projection.transcript, &projection.tool_calls)
}

fn adopt_provider_session(
    context: &AdoptionContext,
    provider_session_id: &str,
    title: Option<&str>,
    name: Option<Option<String>>,
) -> Option<SessionLease> {
    let lease = match SessionLease::try_acquire(context.provider_name, &context.canonical_cwd, provider_session_id) {
        Ok(lease) => Some(lease),
        Err(e @ crate::lease::LeaseError::AlreadyHeld) => {
            eprintln!(
                "agent: could not take the session lease for {provider_session_id}: {e} -- \
                 continuing without it; another Eitri window may be driving the same session"
            );
            None
        }
        // The lease could not be taken for a reason other than contention, such as an unusable lease
        // directory. Do not imply another window holds it: a missing `XDG_RUNTIME_DIR` can cause this path.
        Err(e) => {
            eprintln!(
                "agent: could not take the session lease for {provider_session_id}: {e} -- \
                 continuing without it; this is not contention, the lease could not be taken at all"
            );
            None
        }
    };
    crate::conversation::persist_record(
        context.provider_name,
        &context.conversation_id,
        &context.canonical_cwd,
        provider_session_id,
        context.provider_advertises_resume,
        title,
        &name
            .map(crate::persistence::NameUpdate::Set)
            .unwrap_or(crate::persistence::NameUpdate::Keep),
    );
    lease
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{ContentKind, ProjectionStatus};
    use std::collections::BTreeMap;

    /// The name of the thread that last wrote each conversation's stored history, by conversation
    /// id.
    ///
    /// Test-only, written by `flush_history`, which is the only place a history write happens.
    /// It exists because "the write is on the ingestion thread" (invariant 12) is otherwise only
    /// argued from where the code sits, and the title feature's own history is what that argument
    /// is worth: its first version made the same argument from a lock and was wrong. It is kept per
    /// conversation because tests run in parallel and some call `flush_history` on their own
    /// thread: one slot for the whole process let such a test's write land between another test's
    /// write and its read.
    static HISTORY_WRITER_THREAD: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());

    pub(super) fn note_history_writer_thread(conversation_id: &str) {
        let name = std::thread::current().name().unwrap_or("<unnamed>").to_string();
        HISTORY_WRITER_THREAD
            .lock()
            .unwrap()
            .insert(conversation_id.to_string(), name);
    }

    /// The thread that last wrote `conversation_id`'s stored history, if any did.
    pub(crate) fn history_writer_thread(conversation_id: &str) -> Option<String> {
        HISTORY_WRITER_THREAD.lock().unwrap().get(conversation_id).cloned()
    }

    fn adoption_context(conversation_id: &str) -> AdoptionContext {
        AdoptionContext {
            provider_name: crate::conversation::PROVIDER_NAME,
            conversation_id: conversation_id.to_string(),
            canonical_cwd: "/tmp/ws".to_string(),
            provider_advertises_resume: true,
        }
    }

    /// The changed input is derived once, at the fold, and delivered as its own event right after
    /// the prompt it came from -- whether or not the host later answers (and so hides) the gate
    /// request. Both revisions strictly increase, the second being the derived event's.
    #[test]
    fn a_cli_prompt_that_differs_from_the_gate_is_followed_by_the_event_that_says_so() {
        let ask = |id: &str, input: serde_json::Value, from_cli: bool| AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: Some("toolu_1".into()),
            tool_name: "Bash".into(),
            input,
            provider_prompt: from_cli.then(crate::ProviderPrompt::default),
        };
        let mut st = state();
        st.fold(AgentDomainEvent::ToolCallStarted {
            turn_id: "t".into(),
            tool_use_id: "toolu_1".into(),
            name: "Bash".into(),
            input: serde_json::json!({ "command": "a" }),
        });
        st.fold(ask("g", serde_json::json!({ "command": "a" }), false));
        st.fold(ask("c", serde_json::json!({ "command": "b" }), true));
        st.fold(ask("c2", serde_json::json!({ "command": "a" }), true));
        let RevisedDelivery::Events(events) = st.take_revised_delivery() else {
            panic!("events were queued");
        };
        let kinds: Vec<&str> = events
            .iter()
            .map(|(_, e)| match e {
                AgentDomainEvent::ToolCallStarted { .. } => "started",
                AgentDomainEvent::PermissionRequested { .. } => "asked",
                AgentDomainEvent::ToolInputChanged { .. } => "changed",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["started", "asked", "asked", "changed", "asked"]);
        assert!(
            events.windows(2).all(|w| w[0].0 < w[1].0),
            "revisions strictly increase"
        );
        assert_eq!(
            st.projection.tool_calls[0]
                .changed_input
                .as_ref()
                .map(|c| c.fields["command"].clone()),
            Some(serde_json::json!("b"))
        );
    }

    /// Checks the `record_written` gate while the session id is known but no record is on disk.
    /// Adoption sets the id under the lock and writes the record after releasing it. A history
    /// file written during that gap would have no session record, making it unreachable from
    /// the picker and subject to cleanup by the next new session. The test sets this state
    /// directly so it can check the gate without relying on a timing race.
    #[test]
    fn nothing_is_stored_while_the_session_id_is_known_but_the_record_is_not_written() {
        crate::state_dirs::redirect_state_to_a_test_root();
        let conversation_id = uuid::Uuid::new_v4().simple().to_string();
        let mut initial = state();
        initial.provider_session_id = Some("sess-in-the-gap".to_string());
        initial.record_written = false;
        initial.history_dirty = true;
        initial
            .projection
            .apply(&AgentDomainEvent::UserPromptSubmitted { text: "hi".into() });
        let state = Mutex::new(initial);

        flush_history(&state, &adoption_context(&conversation_id));

        assert!(
            !crate::history::store::history_path(&conversation_id, "sess-in-the-gap")
                .unwrap()
                .exists(),
            "a history file must not exist before the session's record does"
        );
        // And the work is not lost: the flag stays set, so the write happens once the record lands.
        assert!(state.lock().unwrap().history_dirty);

        state.lock().unwrap().record_written = true;
        flush_history(&state, &adoption_context(&conversation_id));
        assert!(crate::history::store::load(&conversation_id, "sess-in-the-gap").is_ok());
    }

    /// A turn that changed the conversation moves the record's `updated_at` and leaves the rest of it
    /// (the start time, the title, the name) alone; a flush with nothing new does not touch it.
    #[test]
    fn a_stored_turn_moves_the_records_updated_at_and_nothing_else() {
        crate::state_dirs::redirect_state_to_a_test_root();
        let conversation_id = uuid::Uuid::new_v4().simple().to_string();
        crate::persistence::save_conversation_record(&crate::persistence::ConversationRecord {
            conversation_id: conversation_id.clone(),
            provider: crate::conversation::PROVIDER_NAME.to_string(),
            provider_session_id: "sess-used".to_string(),
            canonical_cwd: "/tmp/ws".to_string(),
            created_at: "1000".to_string(),
            updated_at: "1000".to_string(),
            provider_advertised_resume: true,
            title: Some("a title".to_string()),
            name: Some("a name".to_string()),
        })
        .unwrap();
        let mut initial = state();
        initial.provider_session_id = Some("sess-used".to_string());
        initial.record_written = true;
        initial.history_dirty = true;
        initial
            .projection
            .apply(&AgentDomainEvent::UserPromptSubmitted { text: "hi".into() });
        let state = Mutex::new(initial);

        flush_history(&state, &adoption_context(&conversation_id));

        let record = crate::persistence::load_conversation_record(&conversation_id, "sess-used").unwrap();
        assert_ne!(record.updated_at, "1000", "the turn moved it");
        assert!(
            record.updated_at.parse::<u128>().unwrap() > 1_000_000_000_000,
            "{}",
            record.updated_at
        );
        assert_eq!(record.created_at, "1000");
        assert_eq!(record.title.as_deref(), Some("a title"));
        assert_eq!(record.name.as_deref(), Some("a name"));

        let moved = record.updated_at.clone();
        flush_history(&state, &adoption_context(&conversation_id));
        let again = crate::persistence::load_conversation_record(&conversation_id, "sess-used").unwrap();
        assert_eq!(again.updated_at, moved, "nothing new was stored, so nothing moved");
    }

    /// A turn boundary marks the history dirty; nothing inside a turn does. The negative half is
    /// the load-bearing one: a partial-streamed reply is 400+ events, and marking on any of them
    /// would mean 400 writes, each two fsyncs.
    #[test]
    fn only_a_turn_boundary_marks_the_history_dirty() {
        let mut state = state();
        state.fold(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        state.fold(text("partial"));
        state.fold(AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_1".into(),
            name: "Read".into(),
            input: serde_json::json!({}),
        });
        state.fold(AgentDomainEvent::UserPromptSubmitted { text: "hello".into() });
        assert!(!state.history_dirty, "nothing inside a turn may mark the history dirty");

        state.fold(AgentDomainEvent::TurnCompleted {
            turn_id: "t1".into(),
            outcome: crate::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        });
        assert!(state.history_dirty);
    }

    /// The two terminal events mark it too: a session that ends without completing a turn still has
    /// whatever it said, and that is exactly the conversation most worth not losing.
    #[test]
    fn a_session_ending_marks_the_history_dirty() {
        for event in [
            AgentDomainEvent::SessionClosed {
                reason: "closed_by_host".into(),
            },
            AgentDomainEvent::SessionUnavailable {
                reason: "crashed".into(),
            },
        ] {
            let mut state = state();
            state.fold(event);
            assert!(state.history_dirty);
        }
    }

    fn state() -> IngestState {
        IngestState {
            projection: AgentSessionProjection::default(),
            provider_session_id: None,
            lease: None,
            pending_ui: VecDeque::new(),
            resync_pending: false,
            resume_outcome: None,
            stats: IngestStats::default(),
            pending_title: None,
            pending_name: None,
            record_written: false,
            history_dirty: false,
            lifecycle_since_open: Vec::new(),
            running_turn: None,
            folded_through: 0,
        }
    }

    fn text(t: &str) -> AgentDomainEvent {
        AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: ContentKind::Text,
            text: t.into(),
        }
    }

    #[test]
    fn a_ui_that_keeps_up_gets_its_events_in_order() {
        let mut state = state();
        state.fold(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        state.fold(text("hello "));
        state.fold(text("world"));

        match state.take_delivery() {
            UiDelivery::Events(events) => assert_eq!(events.len(), 3),
            other => panic!("expected events, got {other:?}"),
        }
        assert!(matches!(state.take_delivery(), UiDelivery::Nothing));
    }

    /// The property this module exists for: raw events stop accumulating once there are more than
    /// the UI could usefully be handed, and the projection carries the meaning instead.
    #[test]
    fn a_ui_that_never_wakes_stops_accumulating_raw_events() {
        let mut state = state();
        state.fold(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        for i in 0..10_000 {
            state.fold(text(&format!("{i} ")));
        }

        assert!(
            state.pending_ui.len() <= UI_EVENT_QUEUE_CAPACITY,
            "the raw-event queue grew to {} -- it is supposed to be bounded",
            state.pending_ui.len()
        );
        assert!(state.resync_pending, "the UI must be told it needs to resynchronise");
        assert_eq!(
            state.stats.events_ingested, 10_001,
            "every event must still have been folded"
        );

        // And the meaning survived in full: 10,000 partial updates coalesced into one assistant
        // message, which is the whole point -- memory follows the conversation, not the event count.
        assert_eq!(state.projection.transcript.len(), 1);
        assert!(state.projection.transcript[0].text.starts_with("0 1 2 "));
        assert!(state.projection.transcript[0].text.ends_with("9999 "));
    }

    #[test]
    fn an_overflowed_queue_asks_for_a_snapshot_once_and_then_resumes_deltas() {
        let mut state = state();
        for i in 0..(UI_EVENT_QUEUE_CAPACITY + 10) {
            state.fold(text(&format!("{i}")));
        }
        assert!(matches!(state.take_delivery(), UiDelivery::Resync));
        // The resync is owed once, not forever.
        assert!(matches!(state.take_delivery(), UiDelivery::Nothing));

        state.fold(text("after"));
        match state.take_delivery() {
            UiDelivery::Events(events) => assert_eq!(events.len(), 1),
            other => panic!("expected deltas to resume, got {other:?}"),
        }
    }

    /// A semantic transition that happens entirely inside an overflow must still be visible
    /// afterwards. It is, because the snapshot is the projection, and the projection saw every event.
    #[test]
    fn a_state_transition_inside_an_overflow_is_not_lost() {
        let mut state = state();
        for i in 0..(UI_EVENT_QUEUE_CAPACITY * 2) {
            state.fold(text(&format!("{i}")));
        }
        state.fold(AgentDomainEvent::PermissionRequested {
            permission_id: "p1".into(),
            tool_use_id: Some("tool-1".into()),
            tool_name: "Bash".into(),
            input: serde_json::json!({}),
            provider_prompt: None,
        });
        state.fold(AgentDomainEvent::SessionUnavailable {
            reason: "the stream died".into(),
        });

        assert!(matches!(state.take_delivery(), UiDelivery::Resync));
        assert!(state.projection.pending_permissions.contains_key("p1"));
        assert!(matches!(state.projection.status, ProjectionStatus::Unavailable { .. }));
        assert_eq!(state.projection.active_turn_id, None);
    }

    #[test]
    fn stats_report_the_deepest_backlog_even_after_it_drains() {
        let mut state = state();
        for i in 0..10 {
            state.fold(text(&format!("{i}")));
        }
        let _ = state.take_delivery();
        let stats = state.stats;
        assert_eq!(stats.ui_backlog, 0);
        assert_eq!(stats.max_ui_backlog, 10);
    }
}

#[cfg(test)]
mod history_snapshot_cost {
    use super::*;

    /// A session of `calls` tool calls, each with a 10,000-character result, and a message after each.
    fn session(calls: usize) -> AgentSessionProjection {
        let mut p = AgentSessionProjection::default();
        p.apply(&AgentDomainEvent::UserPromptSubmitted { text: "go".into() });
        for i in 0..calls {
            let id = format!("toolu_{i}");
            p.apply(&AgentDomainEvent::ToolCallStarted {
                turn_id: "t".into(),
                tool_use_id: id.clone(),
                name: "Read".into(),
                input: serde_json::json!({ "file_path": format!("/p/{i}.rs") }),
            });
            p.apply(&AgentDomainEvent::ToolCallCompleted {
                turn_id: "t".into(),
                tool_use_id: id,
                content: serde_json::Value::String("r".repeat(10_000)),
                is_error: false,
            });
            p.apply(&AgentDomainEvent::ContentDelta {
                turn_id: "t".into(),
                kind: crate::ContentKind::Text,
                text: format!("message {i}"),
            });
        }
        p
    }

    fn best(f: impl Fn() -> std::time::Duration) -> std::time::Duration {
        (0..3).map(|_| f()).min().unwrap()
    }

    fn snapshot_time(p: &AgentSessionProjection) -> std::time::Duration {
        best(|| {
            let started = std::time::Instant::now();
            let kept = history_snapshot(p);
            let elapsed = started.elapsed();
            assert!(kept.2.len() <= crate::history::HISTORY_MAX_ITEMS);
            elapsed
        })
    }

    /// What the history write holds the conversation's lock for no longer grows with the session:
    /// eight times the session takes about as long, where copying it all took eight times as long.
    #[test]
    fn the_lock_held_part_of_a_history_write_does_not_grow_with_the_session() {
        let small = snapshot_time(&session(500));
        let large = snapshot_time(&session(4_000));
        eprintln!("snapshot of 500 calls: {small:?}, of 4000: {large:?}");
        assert!(large < small * 3, "4000 calls took {large:?}, 500 took {small:?}");
    }

    /// The copy the history write used to make under the lock, beside the one it makes now, for a
    /// measurement run by hand (`--ignored --nocapture`).
    #[test]
    #[ignore]
    fn measure_the_history_snapshot() {
        for calls in [500, 4_000, 16_000] {
            let p = session(calls);
            let whole = best(|| {
                let started = std::time::Instant::now();
                let copied = (p.user_prompts.clone(), p.transcript.clone(), p.tool_calls.clone());
                let elapsed = started.elapsed();
                drop(copied);
                elapsed
            });
            eprintln!(
                "{calls} calls: copying it all {whole:?}, the snapshot {:?}",
                snapshot_time(&p)
            );
        }
    }
}

#[cfg(test)]
mod idle_wakeups {
    use super::*;
    use crate::provider::*;
    use std::sync::atomic::AtomicUsize;

    /// A provider that never has anything to say, and counts how often it is asked.
    #[derive(Default)]
    pub(super) struct Quiet {
        pub(super) pumps: AtomicUsize,
        pub(super) queued: Mutex<Vec<AgentDomainEvent>>,
    }

    impl AgentProvider for Quiet {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        fn info(&self) -> ProviderInfo {
            ProviderInfo::default()
        }
        fn create_session(&self, _: CreateSessionRequest) -> Result<String, ProviderError> {
            Ok("s".into())
        }
        fn resume_session(&self, _: ResumeSessionRequest) -> Result<String, ProviderError> {
            Ok("s".into())
        }
        fn send_turn(&self, _: SendTurnRequest) -> Result<String, ProviderError> {
            Ok("t".into())
        }
        fn interrupt_turn(&self, _: InterruptTurnRequest) -> Result<(), ProviderError> {
            Ok(())
        }
        fn resolve_permission(&self, _: ResolvePermissionRequest) -> Result<(), ProviderError> {
            Ok(())
        }
        fn close_session(&self, _: CloseSessionRequest) -> Result<(), ProviderError> {
            Ok(())
        }
        fn pump(&self) -> Vec<AgentDomainEvent> {
            self.pumps.fetch_add(1, Ordering::Relaxed);
            std::mem::take(&mut *self.queued.lock().unwrap())
        }
    }

    /// The same, announcing what it queues.
    #[derive(Default)]
    struct Announcing {
        quiet: Quiet,
        signal: Arc<EventSignal>,
    }

    impl AgentProvider for Announcing {
        fn capabilities(&self) -> ProviderCapabilities {
            self.quiet.capabilities()
        }
        fn info(&self) -> ProviderInfo {
            self.quiet.info()
        }
        fn create_session(&self, r: CreateSessionRequest) -> Result<String, ProviderError> {
            self.quiet.create_session(r)
        }
        fn resume_session(&self, r: ResumeSessionRequest) -> Result<String, ProviderError> {
            self.quiet.resume_session(r)
        }
        fn send_turn(&self, r: SendTurnRequest) -> Result<String, ProviderError> {
            self.quiet.send_turn(r)
        }
        fn interrupt_turn(&self, r: InterruptTurnRequest) -> Result<(), ProviderError> {
            self.quiet.interrupt_turn(r)
        }
        fn resolve_permission(&self, r: ResolvePermissionRequest) -> Result<(), ProviderError> {
            self.quiet.resolve_permission(r)
        }
        fn close_session(&self, r: CloseSessionRequest) -> Result<(), ProviderError> {
            self.quiet.close_session(r)
        }
        fn pump(&self) -> Vec<AgentDomainEvent> {
            self.quiet.pump()
        }
        fn event_signal(&self) -> Option<Arc<EventSignal>> {
            Some(Arc::clone(&self.signal))
        }
    }

    fn start(provider: Arc<dyn AgentProvider + Send + Sync>) -> ConversationIngest {
        ConversationIngest::start(
            provider,
            "claude",
            "c".into(),
            "/tmp/ws".into(),
            false,
            None,
            None,
            AgentSessionProjection::default(),
        )
    }

    fn wait_for(what: &str, done: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "{what} did not happen within 5 s");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// A provider that announces its events is not asked while it has none: the idle thread sleeps
    /// (it was woken about 200 times a second, measured), and an announced event is still folded at
    /// once.
    #[test]
    fn an_announcing_provider_is_not_polled_while_it_is_quiet() {
        let provider = Arc::new(Announcing::default());
        let mut ingest = start(provider.clone());
        wait_for("the first pump", || provider.quiet.pumps.load(Ordering::Relaxed) >= 1);
        std::thread::sleep(std::time::Duration::from_millis(500));
        let idle = provider.quiet.pumps.load(Ordering::Relaxed);
        assert!(idle <= 2, "asked {idle} times in half a second of silence");

        provider
            .quiet
            .queued
            .lock()
            .unwrap()
            .push(AgentDomainEvent::UserPromptSubmitted { text: "hi".into() });
        provider.signal.raise();
        wait_for("the announced event's fold", || {
            ingest.projection().user_prompts.len() == 1
        });

        let started = std::time::Instant::now();
        ingest.stop();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "a sleeping thread is woken to stop"
        );
    }

    /// The UI is told after a fold, so it need not poll for deliveries: once when it installs its wake (for
    /// what was folded before), once per folded batch, and once for a local fold -- and only then.
    #[test]
    fn the_ui_is_woken_by_a_fold_and_only_by_one() {
        let provider = Arc::new(Announcing::default());
        let ingest = start(provider.clone());
        wait_for("the first pump", || provider.quiet.pumps.load(Ordering::Relaxed) >= 1);
        let wakes = Arc::new(AtomicUsize::new(0));
        assert!(!ingest.has_ui_wake());
        {
            let wakes = wakes.clone();
            ingest.set_ui_wake(Arc::new(move || {
                wakes.fetch_add(1, Ordering::SeqCst);
            }));
        }
        assert!(ingest.has_ui_wake());
        assert_eq!(wakes.load(Ordering::SeqCst), 1, "installing it wakes once");
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(wakes.load(Ordering::SeqCst), 1, "a quiet provider wakes nothing");

        provider
            .quiet
            .queued
            .lock()
            .unwrap()
            .push(AgentDomainEvent::UserPromptSubmitted { text: "hi".into() });
        provider.signal.raise();
        wait_for("the wake after the fold", || wakes.load(Ordering::SeqCst) == 2);
        assert!(
            matches!(ingest.take_delivery(), UiDelivery::Events(ref events) if events.len() == 1),
            "the fold is there to take when the wake comes"
        );

        ingest.fold_locally(AgentDomainEvent::UserPromptSubmitted { text: "again".into() });
        assert_eq!(wakes.load(Ordering::SeqCst), 3, "a local fold wakes at once");
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(wakes.load(Ordering::SeqCst), 3);
    }

    type Hook = Arc<dyn Fn() + Send + Sync>;

    /// Test-only: run by the ingestion thread of the conversation with this id, each time round, after
    /// it found no stop and before it reads the signal. Keyed by conversation id so tests running in
    /// parallel do not reach each other's threads.
    static HOOKS: Mutex<Vec<(String, Hook)>> = Mutex::new(Vec::new());

    pub(super) fn between_the_check_and_the_look(conversation_id: &str) {
        let hook = HOOKS
            .lock()
            .unwrap()
            .iter()
            .find(|(id, _)| id == conversation_id)
            .map(|(_, hook)| hook.clone());
        if let Some(hook) = hook {
            hook();
        }
    }

    /// A stop that lands after the thread found no stop and before it read the signal still ends the
    /// thread. The stop's raise is then part of what the thread saw, so only a look at the stop after
    /// that read (here: under the signal's lock, as the wait's own condition) can notice it; waiting on
    /// the count alone slept until the provider's next event, which a closed session never sends, and
    /// `stop()` never returned.
    #[test]
    fn a_stop_between_the_check_and_the_look_still_ends_the_thread() {
        let provider = Arc::new(Announcing::default());
        let conversation_id = uuid::Uuid::new_v4().simple().to_string();
        // The second time round the thread parks here until the test has stopped it.
        let (parked_tx, parked_rx) = std::sync::mpsc::channel::<()>();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let rounds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let parked = Mutex::new(Some((parked_tx, go_rx)));
        HOOKS.lock().unwrap().push((
            conversation_id.clone(),
            Arc::new(move || {
                if rounds.fetch_add(1, Ordering::SeqCst) == 1 {
                    if let Some((parked_tx, go_rx)) = parked.lock().unwrap().take() {
                        parked_tx.send(()).unwrap();
                        let _ = go_rx.recv();
                    }
                }
            }),
        ));
        let ingest = ConversationIngest::start(
            provider.clone(),
            "claude",
            conversation_id.clone(),
            "/tmp/ws".into(),
            false,
            None,
            None,
            AgentSessionProjection::default(),
        );
        // A first round that finds nothing and sleeps; raised once to send it round again.
        wait_for("the first pump", || provider.quiet.pumps.load(Ordering::Relaxed) >= 1);
        provider.signal.raise();
        parked_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the second round");

        // The stop lands while the thread is parked between its check and its look.
        let (stopped_tx, stopped_rx) = std::sync::mpsc::channel::<()>();
        let stopper = std::thread::spawn(move || {
            let mut ingest = ingest;
            ingest.stop();
            let _ = stopped_tx.send(());
        });
        wait_for("the stop's raise", || provider.signal.seen() >= 2);
        go_tx.send(()).unwrap();
        HOOKS.lock().unwrap().retain(|(id, _)| id != &conversation_id);
        assert!(
            stopped_rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok(),
            "stop() did not return: the thread slept through the stop"
        );
        stopper.join().unwrap();
    }

    /// A provider that announces nothing (its `pump` may act on the clock) is still asked on a timer.
    #[test]
    fn a_provider_that_announces_nothing_is_still_polled() {
        let provider = Arc::new(Quiet::default());
        let mut ingest = start(provider.clone());
        std::thread::sleep(std::time::Duration::from_millis(200));
        let pumps = provider.pumps.load(Ordering::Relaxed);
        ingest.stop();
        assert!(pumps >= 10, "asked only {pumps} times in 200 ms");
    }
}
