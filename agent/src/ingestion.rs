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
    pending_ui: VecDeque<AgentDomainEvent>,
    resync_pending: bool,
    resume_outcome: Option<ResumeOutcomeRecord>,
    stats: IngestStats,
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

        if self.resync_pending {
            // Already owed a snapshot; queueing more events would be holding raw data the UI is
            // never going to read.
            return;
        }
        self.pending_ui.push_back(event);
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
        if self.resync_pending {
            self.resync_pending = false;
            return UiDelivery::Resync;
        }
        if self.pending_ui.is_empty() {
            return UiDelivery::Nothing;
        }
        let events: Vec<AgentDomainEvent> = self.pending_ui.drain(..).collect();
        self.stats.ui_backlog = 0;
        UiDelivery::Events(events)
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
    ) -> Self {
        let state = Arc::new(Mutex::new(IngestState {
            projection: AgentSessionProjection::default(),
            provider_session_id: initial_provider_session_id,
            lease: initial_lease,
            pending_ui: VecDeque::new(),
            resync_pending: false,
            resume_outcome: None,
            stats: IngestStats::default(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let context = AdoptionContext { conversation_id, canonical_cwd, provider_advertises_resume };

        let thread = {
            let state = Arc::clone(&state);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("neovibe-agent-ingest".to_string())
                .spawn(move || ingest_loop(provider, state, stop, context))
                .expect("spawning the agent ingestion thread")
        };

        Self { state, stop, thread: Some(thread) }
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

    /// Folds an event this side produced rather than the provider.
    ///
    /// Only for the terminal events `shutdown` must record when the provider can no longer be asked
    /// -- never for provider lifecycle events, which are the provider's to state.
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
        if events.is_empty() {
            std::thread::sleep(INGEST_POLL_INTERVAL);
            continue;
        }
        // Adoption touches the filesystem, so it happens OUTSIDE the state lock -- holding it across
        // a `flock` and a JSON write would block every UI tick reading the projection for the
        // duration.
        let mut adopt: Option<String> = None;
        {
            let mut guard = state.lock().unwrap();
            for event in events {
                if let AgentDomainEvent::SessionOpened { provider_session_id, .. } = &event {
                    // Only the FIRST one. `SessionOpened` repeats -- the Agent SDK emits a
                    // system/init at the start of every turn, not once per session -- so adopting
                    // unconditionally would re-take a lease this conversation already holds, on
                    // every single turn.
                    if guard.provider_session_id.is_none() {
                        adopt = Some(provider_session_id.clone());
                    }
                    guard.provider_session_id = Some(provider_session_id.clone());
                }
                guard.fold(event);
            }
        }
        if let Some(provider_session_id) = adopt {
            let lease = adopt_provider_session(&context, &provider_session_id);
            state.lock().unwrap().lease = lease;
        }
    }
}

fn adopt_provider_session(context: &AdoptionContext, provider_session_id: &str) -> Option<SessionLease> {
    let lease = match SessionLease::try_acquire(
        crate::conversation::PROVIDER_NAME,
        &context.canonical_cwd,
        provider_session_id,
    ) {
        Ok(lease) => Some(lease),
        Err(e) => {
            eprintln!(
                "agent: could not take the session lease for {provider_session_id}: {e} -- \
                 continuing without it; another Neovibe window may be driving the same session"
            );
            None
        }
    };
    crate::conversation::persist_record(
        &context.conversation_id,
        &context.canonical_cwd,
        provider_session_id,
        context.provider_advertises_resume,
    );
    lease
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentKind, ProjectionStatus};

    fn state() -> IngestState {
        IngestState {
            projection: AgentSessionProjection::default(),
            provider_session_id: None,
            lease: None,
            pending_ui: VecDeque::new(),
            resync_pending: false,
            resume_outcome: None,
            stats: IngestStats::default(),
        }
    }

    fn text(t: &str) -> AgentDomainEvent {
        AgentDomainEvent::ContentDelta { turn_id: "t1".into(), kind: ContentKind::Text, text: t.into() }
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
        assert_eq!(state.stats.events_ingested, 10_001, "every event must still have been folded");

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
        });
        state.fold(AgentDomainEvent::SessionUnavailable { reason: "the stream died".into() });

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
