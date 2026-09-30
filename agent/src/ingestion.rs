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
use crate::projection::{AgentDomainEvent, AgentSessionProjection};
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

/// How often the ingestion thread drains the provider. Short enough that the provider's own internal
/// queue stays near-empty between drains, which is what keeps the raw-event backlog bounded at BOTH
/// layers rather than moving it one step upstream.
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

/// [`UiDelivery`], with each event's revision: the projection's `last_revision` right after that
/// event was folded, read under the same lock as the fold itself.
///
/// **What it is for (P1-A2 round 2).** A caller that also hands the UI a snapshot needs to know
/// which queued events that snapshot already contains. Folding and queueing happen on the ingestion
/// thread, so between the caller's last drain and the moment it reads a snapshot, more events can be
/// folded (and so be in the snapshot) and queued (and so reach the next drain) -- the same event,
/// twice. The snapshot's own `last_revision` and these tags compare directly, because
/// `AgentSessionProjection::apply` bumps `last_revision` by exactly one per fold and nothing else
/// moves it: an event tagged at or below a snapshot's revision is already in that snapshot. The
/// caller notes that revision on the conversation (`AgentConversation::note_ui_snapshot`) and its
/// next drain takes it back, so a revision never outlives the session it was read from.
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeOutcomeRecord {
    pub requested: String,
    pub status: crate::ResumeStatus,
    pub attached: Option<String>,
    pub forked: bool,
    pub detail: Option<String>,
}

/// Immutable facts the ingestion thread needs in order to adopt a provider session when one appears.
struct AdoptionContext {
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
    /// Whether this session's record is on disk. NOT the same moment as `provider_session_id`
    /// becoming `Some`: adoption sets the id under the lock and writes the record after releasing
    /// it, and a title written into that gap found no record and was lost (review of `7fb787b`,
    /// reproduced 20/20). `true` from the start for a resume, whose record `resume()` writes before
    /// it returns.
    record_written: bool,
    /// Whether this conversation has produced something worth storing since its history was last
    /// written. Set by `fold` at a turn boundary and nowhere else -- see `flush_history`.
    history_dirty: bool,
}

impl IngestState {
    fn fold(&mut self, event: AgentDomainEvent) {
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
        self.projection.apply(&event);
        self.stats.events_ingested += 1;
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

/// A conversation's ingestion thread and the state it owns.
pub struct ConversationIngest {
    state: Arc<Mutex<IngestState>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Borrowed access to the canonical projection.
///
/// A guard rather than a clone: the UI reads this on every 33ms tick, and cloning a conversation's
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
        let state = Arc::new(Mutex::new(IngestState {
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
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let context = AdoptionContext {
            conversation_id,
            canonical_cwd,
            provider_advertises_resume,
        };

        let thread = {
            let state = Arc::clone(&state);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("eitri-agent-ingest".to_string())
                .spawn(move || ingest_loop(provider, state, stop, context))
                .expect("spawning the agent ingestion thread")
        };

        Self {
            state,
            stop,
            thread: Some(thread),
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
    }

    /// Keeps a rename (`prefix ,`) for the ingestion thread to write. The last one noted wins,
    /// unlike `note_title`'s first-wins: `None` clears it. Nothing here touches the disk.
    pub(crate) fn note_name(&self, name: Option<String>) {
        self.state.lock().unwrap().pending_name = Some(name);
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
    }

    /// Stops the ingestion thread and waits for it. Idempotent.
    pub(crate) fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
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

fn ingest_loop(
    provider: Arc<dyn crate::provider::AgentProvider + Send + Sync>,
    state: Arc<Mutex<IngestState>>,
    stop: Arc<AtomicBool>,
    context: AdoptionContext,
) {
    while !stop.load(Ordering::Relaxed) {
        let events = provider.pump();
        // Before the idle check, so a title noted while the provider is quiet still gets written.
        flush_title(&state, &context);
        flush_name(&state, &context);
        // Likewise: the turn whose completion marked the history dirty is usually the last event of
        // a quiet stretch, so waiting for the next non-empty pump would delay every write.
        flush_history(&state, &context);
        if events.is_empty() {
            std::thread::sleep(INGEST_POLL_INTERVAL);
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
        if let Some(provider_session_id) = adopt {
            let lease = adopt_provider_session(&context, &provider_session_id, title.as_deref(), name);
            let mut guard = state.lock().unwrap();
            guard.lease = lease;
            guard.record_written = true;
        }
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
/// The lock is held only long enough to read the dirty flag and CLONE the three collections.
/// Serializing and writing happen after it is released. That does not contradict `ProjectionGuard`'s
/// "do not clone the transcript on every tick" rule -- that rule is about the 33ms tick, and this
/// runs once per turn.
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
        (
            id,
            guard.projection.user_prompts.clone(),
            guard.projection.transcript.clone(),
            guard.projection.tool_calls.clone(),
        )
    };
    let history = crate::history::StoredHistory::from_collections(
        context.conversation_id.clone(),
        provider_session_id.clone(),
        user_prompts,
        transcript,
        tool_calls,
    );
    #[cfg(test)]
    tests::note_history_writer_thread();
    if let Err(e) = crate::history::store::save(&history) {
        eprintln!("agent: could not store the history of session {provider_session_id}: {e}");
    }
}

fn adopt_provider_session(
    context: &AdoptionContext,
    provider_session_id: &str,
    title: Option<&str>,
    name: Option<Option<String>>,
) -> Option<SessionLease> {
    let lease = match SessionLease::try_acquire(
        crate::conversation::PROVIDER_NAME,
        &context.canonical_cwd,
        provider_session_id,
    ) {
        Ok(lease) => Some(lease),
        Err(e @ crate::lease::LeaseError::AlreadyHeld) => {
            eprintln!(
                "agent: could not take the session lease for {provider_session_id}: {e} -- \
                 continuing without it; another Eitri window may be driving the same session"
            );
            None
        }
        // Not contention: the lease directory could not be used at all. Saying "another window"
        // here sent the reader looking for a process that did not exist -- on macOS before M1,
        // every session took this branch because `XDG_RUNTIME_DIR` was unset.
        Err(e) => {
            eprintln!(
                "agent: could not take the session lease for {provider_session_id}: {e} -- \
                 continuing without it; this is not contention, the lease could not be taken at all"
            );
            None
        }
    };
    crate::conversation::persist_record(
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

    /// The name of the thread that last wrote a stored history.
    ///
    /// Test-only, written by `flush_history`, which is the only place a history write happens.
    /// It exists because "the write is on the ingestion thread" (invariant 12) is otherwise only
    /// argued from where the code sits, and the title feature's own history is what that argument
    /// is worth: its first version made the same argument from a lock and was wrong.
    pub(crate) static HISTORY_WRITER_THREAD: Mutex<Option<String>> = Mutex::new(None);

    pub(super) fn note_history_writer_thread() {
        *HISTORY_WRITER_THREAD.lock().unwrap() = Some(std::thread::current().name().unwrap_or("<unnamed>").to_string());
    }

    fn adoption_context(conversation_id: &str) -> AdoptionContext {
        AdoptionContext {
            conversation_id: conversation_id.to_string(),
            canonical_cwd: "/tmp/ws".to_string(),
            provider_advertises_resume: true,
        }
    }

    /// The `record_written` gate, which `provider_session_id` alone does NOT cover.
    ///
    /// There is a real window in which the id is visible and the record is not yet on disk:
    /// adoption sets the id under the lock and writes the record after releasing it. That window is
    /// what lost 20 titles out of 20 in the review of `7fb787b`. A history file written in it would
    /// belong to a session with no record -- an orphan from birth, unreachable from the picker and
    /// swept by the next new session. Driven directly rather than raced, because a test that had to
    /// hit that window would be a flaky test of a deterministic rule.
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
