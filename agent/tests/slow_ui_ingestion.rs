//! What happens to a conversation when the UI stops looking at it.
//!
//! **Free and deterministic.** Driven by a synthetic provider rather than a real one: the property
//! under test is how tens of thousands of events behave against a stalled consumer, and paying a
//! model to generate them would buy nondeterminism at a price. Everything below the provider is the
//! real thing -- the real ingestion thread, the real `AgentSessionProjection`, the real bounded
//! delivery queue.
//!
//! Run with `cargo test -p agent --test slow_ui_ingestion`.
//!
//! ## The property
//!
//! Provider event throughput and UI repaint throughput are different quantities. A UI that stalls
//! for twenty seconds should cost memory proportional to the CONVERSATION -- messages, tool calls,
//! pending permissions -- not to the number of raw events that happened to arrive while it was away.
//! Before the ingestion thread existed, `pump()` both drained the provider and folded the reducer,
//! and a GTK timer was its only caller: a stalled UI stalled the reducer, and raw events queued.
//! Measured then, over a 20s stall: 253 raw events held, with transport lag flat at 1-3ms.

use agent::{
    AgentConversation, AgentDomainEvent, AgentProvider, CloseSessionRequest, ContentKind, CreateSessionRequest,
    InterruptTurnRequest, ProjectionStatus, ProviderCapabilities, ProviderError, ProviderInfo,
    ResolvePermissionRequest, ResumeSessionRequest, SendTurnRequest, TurnOutcome, UiDelivery, UI_EVENT_QUEUE_CAPACITY,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A provider that hands out whatever has been queued for it, as fast as it is asked.
///
/// Stands in for the sidecar's watch task: the real one pushes translated events into a `Vec` that
/// `pump()` drains, which is exactly this shape. What it does NOT stand in for is the wire, so
/// nothing here proves anything about gRPC -- the real-sidecar suites do that. This proves what
/// happens above the provider boundary, at volumes a real provider would be slow and costly to reach.
struct ScriptedProvider {
    queued: Mutex<std::collections::VecDeque<AgentDomainEvent>>,
    handed_out: AtomicU64,
}

impl ScriptedProvider {
    fn new() -> Self {
        Self {
            queued: Mutex::new(std::collections::VecDeque::new()),
            handed_out: AtomicU64::new(0),
        }
    }

    fn emit(&self, event: AgentDomainEvent) {
        self.queued.lock().unwrap().push_back(event);
    }

    fn handed_out(&self) -> u64 {
        self.handed_out.load(Ordering::Relaxed)
    }

    /// How many events are still sitting in the provider itself.
    ///
    /// Asserted as well as the ingestion queue, because a fix that only bounded the queue one layer
    /// up would have moved the unbounded pile here rather than removed it.
    fn still_queued(&self) -> usize {
        self.queued.lock().unwrap().len()
    }
}

impl AgentProvider for ScriptedProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            resume: false,
            fork: false,
            interrupt: true,
            bypass_permission_mode: true,
            interactive_permission_mode: true,
            cli_auto_mode: false,
        }
    }
    fn info(&self) -> ProviderInfo {
        ProviderInfo::default()
    }
    fn create_session(&self, _r: CreateSessionRequest) -> Result<String, ProviderError> {
        Ok("verdandi-session-1".to_string())
    }
    fn resume_session(&self, _r: ResumeSessionRequest) -> Result<String, ProviderError> {
        Err(ProviderError::UnsupportedCapability("resume"))
    }
    fn send_turn(&self, _r: SendTurnRequest) -> Result<String, ProviderError> {
        Ok("turn-1".to_string())
    }
    fn interrupt_turn(&self, _r: InterruptTurnRequest) -> Result<(), ProviderError> {
        Ok(())
    }
    fn resolve_permission(&self, _r: ResolvePermissionRequest) -> Result<(), ProviderError> {
        Ok(())
    }
    fn close_session(&self, _r: CloseSessionRequest) -> Result<(), ProviderError> {
        Ok(())
    }
    fn pump(&self) -> Vec<AgentDomainEvent> {
        let drained: Vec<AgentDomainEvent> = self.queued.lock().unwrap().drain(..).collect();
        self.handed_out.fetch_add(drained.len() as u64, Ordering::Relaxed);
        drained
    }
}

/// Each conversation gets its own directory inside this process's disposable state root.
///
/// Load-bearing rather than tidiness: these tests fold a real `SessionOpened`, and that is what
/// makes `AgentConversation`'s ingestion thread take a session lease and write a conversation
/// record. `test_workspace_dir` redirects both away from the developer's own
/// `~/.local/state/eitri/` and `$XDG_RUNTIME_DIR/eitri/` (see `agent::state_dirs`), and a
/// distinct cwd per conversation keeps these tests from sharing a lease key with each other.
fn conversation(provider: Arc<ScriptedProvider>) -> AgentConversation {
    let dir = agent::state_dirs::test_workspace_dir("slow-ui");
    AgentConversation::create(provider, &dir, agent::setting_sources::ProjectTrust::Untrusted)
        .expect("create should succeed against the scripted provider")
}

fn text(text: &str) -> AgentDomainEvent {
    AgentDomainEvent::ContentDelta {
        turn_id: "turn-1".into(),
        kind: ContentKind::Text,
        text: text.into(),
    }
}

/// Waits until ingestion has folded everything handed out so far.
fn settle(provider: &ScriptedProvider, conversation: &AgentConversation) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if provider.still_queued() == 0 && conversation.ingest_stats().events_ingested >= provider.handed_out() {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!(
        "ingestion never caught up: folded {} of {} handed out, {} still queued in the provider",
        conversation.ingest_stats().events_ingested,
        provider.handed_out(),
        provider.still_queued()
    );
}

/// This process's resident memory, for the "does it follow the conversation or the event count"
/// question. `None` rather than 0 when unreadable: a zero among real numbers reads as a measurement.
/// Read through `agent::process_probe`, which covers Linux and macOS; it used to be
/// `/proc/self/status`, so on macOS this test printed "RSS unreadable" and passed.
fn rss_kib() -> Option<u64> {
    agent::process_probe::resident_kib(std::process::id())
}

/// **The headline property, at a scale a real provider would never reach cheaply.**
///
/// Twenty thousand partial updates arrive while nothing ever reads the UI queue. Raw events must not
/// accumulate; the conversation must be complete anyway.
#[test]
fn twenty_thousand_events_against_a_stalled_ui_do_not_accumulate_as_raw_events() {
    let provider = Arc::new(ScriptedProvider::new());
    let conversation = conversation(Arc::clone(&provider));

    provider.emit(AgentDomainEvent::SessionOpened {
        session_id: "verdandi-session-1".into(),
        provider_session_id: "claude-uuid-abc".into(),
        model: "claude-sonnet-5".into(),
        cwd: "/tmp".into(),
    });
    provider.emit(AgentDomainEvent::TurnStarted {
        turn_id: "turn-1".into(),
    });

    let rss_before = rss_kib();
    const EVENTS: usize = 20_000;
    for i in 0..EVENTS {
        provider.emit(text(&format!("{i} ")));
        // Let ingestion interleave rather than handing it one giant batch -- a single drain would
        // not exercise the queue the way a live stream does.
        if i % 500 == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    // The UI is never asked. Not once.
    settle(&provider, &conversation);
    let rss_after = rss_kib();

    let stats = conversation.ingest_stats();
    eprintln!("---- {EVENTS} events, UI never pumped ----");
    eprintln!("  events ingested         : {}", stats.events_ingested);
    eprintln!("  UI backlog now          : {}", stats.ui_backlog);
    eprintln!(
        "  deepest UI backlog      : {} (cap {UI_EVENT_QUEUE_CAPACITY})",
        stats.max_ui_backlog
    );
    eprintln!("  resyncs owed to the UI  : {}", stats.resyncs);
    eprintln!("  still queued in provider: {}", provider.still_queued());
    eprintln!(
        "  transcript messages     : {}",
        conversation.projection().transcript.len()
    );
    match (rss_before, rss_after) {
        (Some(before), Some(after)) => {
            eprintln!("  RSS {before}KiB -> {after}KiB (+{}KiB)", after.saturating_sub(before))
        }
        // Linux and macOS always read it (`process_probe`'s own tests insist); only another
        // platform reaches this, and it says so rather than printing a number.
        _ => eprintln!("  RSS unreadable on this platform -- not measured"),
    }

    // Every event was folded. Nothing was skipped to keep the queue small.
    assert_eq!(stats.events_ingested as usize, EVENTS + 2);

    // The raw-event backlog is bounded, at BOTH layers -- the delivery queue and the provider's own.
    assert!(
        stats.max_ui_backlog <= UI_EVENT_QUEUE_CAPACITY + 1,
        "the UI queue reached {}, past its {UI_EVENT_QUEUE_CAPACITY} cap",
        stats.max_ui_backlog
    );
    assert_eq!(
        provider.still_queued(),
        0,
        "events piled up in the provider instead of the UI queue"
    );
    assert!(
        stats.resyncs > 0,
        "with 20k events and no UI, the queue must have overflowed at least once"
    );

    // And memory follows the CONVERSATION: 20,000 partial updates are one assistant message.
    let projection = conversation.projection();
    assert_eq!(
        projection.transcript.len(),
        1,
        "partial updates must coalesce into one message"
    );
    assert!(projection.transcript[0].text.starts_with("0 1 2 "));
    assert!(projection.transcript[0].text.ends_with("19999 "));
    assert_eq!(projection.active_turn_id.as_deref(), Some("turn-1"));
}

/// The UI comes back and is handed current state in one go, not twenty thousand events.
#[test]
fn a_ui_that_returns_after_a_stall_resynchronises_instead_of_replaying_everything() {
    let provider = Arc::new(ScriptedProvider::new());
    let conversation = conversation(Arc::clone(&provider));
    provider.emit(AgentDomainEvent::TurnStarted {
        turn_id: "turn-1".into(),
    });
    for i in 0..5_000 {
        provider.emit(text(&format!("{i} ")));
    }
    settle(&provider, &conversation);

    let resumed_at = Instant::now();
    let delivery = conversation.take_ui_delivery();
    let catch_up = resumed_at.elapsed();
    eprintln!("catch-up after a 5,000-event stall: {}us", catch_up.as_micros());

    assert!(
        matches!(delivery, UiDelivery::Resync),
        "a UI that missed 5,000 events must be told to resynchronise, not handed them"
    );
    // Exactly one. The resync is owed once, not once per missed event.
    assert!(matches!(conversation.take_ui_delivery(), UiDelivery::Nothing));
    // Whatever it reads now is complete and current, which is what makes the resync lossless.
    assert_eq!(conversation.projection().transcript.len(), 1);
}

/// **A turn ending while the UI is away.**
///
/// The conversation must stop being shown as working, the text already received must be intact, the
/// terminal event must appear exactly once, and the session must still be usable -- none of which
/// may depend on the UI having processed the intervening updates.
#[test]
fn an_interrupt_during_a_stall_terminates_the_turn_exactly_once() {
    let provider = Arc::new(ScriptedProvider::new());
    let conversation = conversation(Arc::clone(&provider));
    provider.emit(AgentDomainEvent::TurnStarted {
        turn_id: "turn-1".into(),
    });
    for i in 0..2_000 {
        provider.emit(text(&format!("{i} ")));
    }
    provider.emit(AgentDomainEvent::TurnCompleted {
        turn_id: "turn-1".into(),
        outcome: TurnOutcome::Interrupted,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
        detail: Default::default(),
    });
    settle(&provider, &conversation);

    // The UI has still seen nothing at this point.
    let projection = conversation.projection();
    assert_eq!(
        projection.active_turn_id, None,
        "an interrupted turn must not still read as working"
    );
    assert!(!matches!(projection.status, ProjectionStatus::Unavailable { .. }));
    assert_eq!(projection.transcript.len(), 1);
    assert!(
        projection.transcript[0].text.starts_with("0 1 2 "),
        "text received before the interrupt must survive"
    );
    drop(projection);

    // The session is reusable: a second turn folds normally on top.
    provider.emit(AgentDomainEvent::TurnStarted {
        turn_id: "turn-2".into(),
    });
    provider.emit(AgentDomainEvent::ContentDelta {
        turn_id: "turn-2".into(),
        kind: ContentKind::Text,
        text: "second".into(),
    });
    settle(&provider, &conversation);
    assert_eq!(conversation.projection().transcript.len(), 2);
    assert_eq!(conversation.projection().active_turn_id.as_deref(), Some("turn-2"));
}

/// **A permission boundary crossed entirely inside a stall.**
///
/// Coalescing must not lose a semantic transition: the request has to still be pending when the UI
/// returns, and the provider-confirmed resolution has to clear it.
#[test]
fn a_permission_requested_during_a_stall_is_still_pending_when_the_ui_returns() {
    let provider = Arc::new(ScriptedProvider::new());
    let conversation = conversation(Arc::clone(&provider));
    provider.emit(AgentDomainEvent::TurnStarted {
        turn_id: "turn-1".into(),
    });
    for i in 0..1_000 {
        provider.emit(text(&format!("{i} ")));
    }
    provider.emit(AgentDomainEvent::PermissionRequested {
        permission_id: "p1".into(),
        tool_use_id: Some("tool-1".into()),
        tool_name: "Bash".into(),
        input: serde_json::json!({ "command": "echo hi" }),
        provider_prompt: None,
    });
    for i in 0..1_000 {
        provider.emit(text(&format!("after-{i} ")));
    }
    settle(&provider, &conversation);

    // Buried under 2,000 content events on either side, and still there.
    assert!(
        conversation.projection().pending_permissions.contains_key("p1"),
        "a permission request inside an overflow must survive the coalescing"
    );
    // Exactly one card, not one per delivery.
    assert_eq!(conversation.projection().pending_permissions.len(), 1);

    // And only a provider-confirmed resolution clears it -- nothing here synthesizes one.
    provider.emit(AgentDomainEvent::PermissionResolved {
        permission_id: "p1".into(),
        outcome: agent::PermissionOutcome::Allowed,
    });
    settle(&provider, &conversation);
    assert!(conversation.projection().pending_permissions.is_empty());
}

/// **The session dying while the UI is away.**
///
/// Availability outranks stale turn identity: the UI must come back to an unavailable session, not
/// to `active_turn_id = Some` left over from a turn that will never finish.
#[test]
fn a_session_that_dies_during_a_stall_is_unavailable_when_the_ui_returns() {
    let provider = Arc::new(ScriptedProvider::new());
    let conversation = conversation(Arc::clone(&provider));
    provider.emit(AgentDomainEvent::TurnStarted {
        turn_id: "turn-1".into(),
    });
    for i in 0..1_000 {
        provider.emit(text(&format!("{i} ")));
    }
    provider.emit(AgentDomainEvent::SessionUnavailable {
        reason: "the connection to the provider ended before this session did".into(),
    });
    settle(&provider, &conversation);

    let projection = conversation.projection();
    assert!(
        matches!(projection.status, ProjectionStatus::Unavailable { .. }),
        "got {:?}",
        projection.status
    );
    assert_eq!(
        projection.active_turn_id, None,
        "availability must outrank a stale turn id"
    );
    assert_eq!(
        projection.transcript.len(),
        1,
        "the truncated reply is kept, and the banner says it may be"
    );
}

/// Ingestion keeps folding while the UI is away -- which is the whole architecture in one assertion.
///
/// Stated separately because it is the thing that makes the reconnect cursor safe: the cursor tracks
/// provider ingestion, and ingestion is not gated on rendering. A UI stall that held ingestion back
/// would drag the cursor with it, and a cursor lagging behind events already folded would force a
/// replay that is not needed -- or, once those events aged out of the provider's ring, an EVENT_GAP
/// that never had to happen. A frozen window would manufacture the exact loss the protocol exists
/// to prevent.
#[test]
fn the_projection_advances_while_the_ui_is_asleep() {
    let provider = Arc::new(ScriptedProvider::new());
    let conversation = conversation(Arc::clone(&provider));

    provider.emit(AgentDomainEvent::TurnStarted {
        turn_id: "turn-1".into(),
    });
    settle(&provider, &conversation);
    let early = conversation.projection().last_revision;

    for i in 0..300 {
        provider.emit(text(&format!("{i} ")));
    }
    settle(&provider, &conversation);

    assert!(
        conversation.projection().last_revision > early,
        "the projection did not advance without the UI asking it to"
    );
    assert_eq!(conversation.ingest_stats().events_ingested, 301);
}

/// `AgentConversation`'s accessors all share ONE `std::sync::Mutex`, which is not reentrant --
/// so a caller holding the projection guard must not call any of the others.
///
/// This is a characterization test, and it is here because the property is invisible at the call
/// site: `projection()` returns something that derefs like a plain struct, and
/// `provider_session_id()` reads like a field access. On 2026-09-15 a `shell` call site put the
/// second inside the first's scope and deadlocked the GTK main loop outright -- no input, no window
/// close, the last frame still painted, so it looked alive rather than hung. Only the sidecar path
/// could show it: the legacy backend's `projection()` borrows a plain field and locks nothing.
///
/// Measured across threads rather than re-entered on one, because re-entering is the deadlock and a
/// test cannot come back from it.
#[test]
fn every_accessor_shares_one_lock_with_the_projection_guard() {
    let provider = Arc::new(ScriptedProvider::new());
    let conversation = Arc::new(conversation(provider.clone()));

    let holder = {
        let conversation = Arc::clone(&conversation);
        std::thread::spawn(move || {
            let guard = conversation.projection();
            std::thread::sleep(std::time::Duration::from_millis(300));
            drop(guard);
        })
    };
    // Long enough that the holder is certainly inside its sleep, short enough to leave most of it.
    std::thread::sleep(std::time::Duration::from_millis(60));

    let start = std::time::Instant::now();
    let _ = conversation.provider_session_id();
    let waited = start.elapsed();
    holder.join().unwrap();

    assert!(
        waited >= std::time::Duration::from_millis(200),
        "provider_session_id() returned in {waited:?} while another thread held the projection \
         guard -- if the locks have been genuinely split, that is an improvement and this test \
         should be rewritten deliberately. If they have not, the measurement is wrong."
    );
}
