//! Real-sidecar coverage for the two things the replay protocol exists for: recovering a broken
//! watch stream without loss or duplication, and refusing to paper over a gap it cannot repair.
//!
//! `#[ignore]`d and billed. Run with
//! `cargo test -p agent --test claude_sidecar_replay_recovery -- --ignored --test-threads=1 --nocapture`
//!
//! **Why these need fault injection rather than a real network.** The existing broken-stream test
//! kills the whole sidecar, which proves the client reports an unrecoverable loss -- and says
//! nothing about recovery, because there is nothing left to reconnect to. The case replay exists
//! for is the other one: the TRANSPORT fails while the provider keeps running. Waiting for a real
//! transient failure to occur is not a test, so the sidecar carries a narrowly-scoped seam
//! (`VERDANDI_CLAUDE_SIDECAR_FAULT_DROP_WATCH_AFTER`) that breaks one watch and deliberately leaves
//! the session alive.
//!
//! **Environment, and why `--test-threads=1` is not optional here.** These set process-global
//! environment variables that the sidecar inherits (`spawn.rs` never calls `env_clear`, so the Node
//! child gets the whole parent environment). Two tests in one binary setting different values would
//! interfere, and the symptom would be a flaky PASS -- a test whose ring was not actually small
//! never evicts, and never gapping looks exactly like correct behaviour.

use agent::{
    AgentDomainEvent, AgentProvider, ClaudeSidecarProvider, ContentKind, CreateSessionRequest,
    PermissionMode, ProjectionStatus, SendTurnRequest, StreamingPreference,
};
use std::time::{Duration, Instant};

/// Long enough to keep the event stream busy for several seconds, so a break lands mid-flight and
/// events keep being produced while nothing is watching.
const STREAMING_PROMPT: &str =
    "Write about 600 words on why distributed version control won. Continuous prose, no headings.";

/// Sets the sidecar's configuration for this test, then connects.
///
/// SAFETY-ish note: `std::env::set_var` is process-global. Every test in this file must run
/// single-threaded (see the module doc) or they will overwrite each other's configuration and pass
/// for the wrong reason.
fn connect_with(vars: &[(&str, &str)]) -> ClaudeSidecarProvider {
    for (key, value) in vars {
        std::env::set_var(key, value);
    }
    let provider = ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string())
        .expect("connecting to a real sidecar should succeed");
    for (key, _) in vars {
        std::env::remove_var(key);
    }
    provider
}

fn open(provider: &ClaudeSidecarProvider) -> String {
    provider
        .create_session(CreateSessionRequest {
            cwd: std::env::temp_dir().to_string_lossy().to_string(),
            permission_mode: PermissionMode::Bypass,
            streaming: StreamingPreference::Partial,
        })
        .expect("create_session should succeed")
}

fn text_of(events: &[AgentDomainEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::ContentDelta { kind: ContentKind::Text, text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn drain_until<F: Fn(&[AgentDomainEvent]) -> bool>(
    provider: &ClaudeSidecarProvider,
    window: Duration,
    done: F,
) -> Vec<AgentDomainEvent> {
    let deadline = Instant::now() + window;
    let mut all = Vec::new();
    while Instant::now() < deadline {
        all.extend(provider.pump());
        if done(&all) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    all
}

fn turn_ended(events: &[AgentDomainEvent]) -> bool {
    events.iter().any(|e| {
        matches!(
            e,
            AgentDomainEvent::TurnCompleted { .. }
                | AgentDomainEvent::SessionUnavailable { .. }
                | AgentDomainEvent::SessionClosed { .. }
        )
    })
}

/// **The case replay exists for.** A watch stream breaks while the provider keeps running; the
/// client reconnects from its last delivered sequence and the missing events are replayed.
///
/// Acceptance is deliberately not "the stream reconnected". A reconnect that silently skipped the
/// events produced while it was down would also look like a reconnect. What is asserted is that the
/// turn's own final text -- the provider's authoritative account of what it said -- is exactly what
/// this client accumulated across the break.
#[test]
#[ignore]
fn a_broken_watch_stream_recovers_without_loss_or_duplication() {
    // Break the first watch after 5 live events. The ring stays at its default capacity, so the
    // events produced during the reconnect are all still retained and the gap path is not involved.
    let provider = connect_with(&[("VERDANDI_CLAUDE_SIDECAR_FAULT_DROP_WATCH_AFTER", "5")]);
    let session_id = open(&provider);

    provider
        .send_turn(SendTurnRequest { session_id: session_id.clone(), text: STREAMING_PROMPT.into() })
        .unwrap();

    let all = drain_until(&provider, Duration::from_secs(180), turn_ended);

    // THE PREMISE. A successful recovery is completely silent from the outside, so without this the
    // test passes just as happily when the seam never fired -- a plain streaming test wearing a
    // recovery test's name. This is the assertion that makes the rest of it mean anything.
    let reconnects = provider.backpressure_stats().watch_reconnects;
    assert!(
        reconnects > 0,
        "the watch stream was never broken, so nothing here exercised replay. Re-run; if this \
         repeats, the fault seam is not reaching the sidecar."
    );
    eprintln!("watch reconnects during this turn: {reconnects}");

    let broke = all.iter().any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. }));
    assert!(
        !broke,
        "the client gave up instead of recovering: {:?}",
        all.iter().find_map(|e| match e {
            AgentDomainEvent::SessionUnavailable { reason } => Some(reason.clone()),
            _ => None,
        })
    );

    let completed = all.iter().find_map(|e| match e {
        AgentDomainEvent::TurnCompleted { result_text, .. } => Some(result_text.clone()),
        _ => None,
    });
    let final_text = completed.expect("the turn never completed after the stream was broken");
    let accumulated = text_of(&all);

    eprintln!(
        "recovered turn: {} events, {} accumulated chars, {} final chars",
        all.len(),
        accumulated.len(),
        final_text.len()
    );

    // No loss and no duplication, judged against the provider's own final text. `result_text` is the
    // LAST assistant message of the turn, so on a multi-message turn it is a suffix rather than the
    // whole thing -- hence `ends_with` rather than equality.
    assert!(!final_text.is_empty(), "the turn produced no final text to reconcile against");
    assert!(
        accumulated.ends_with(&final_text),
        "what this client accumulated across the break does not end in what the provider says it \
         said. accumulated tail: {:?}\nfinal: {:?}",
        &accumulated[accumulated.len().saturating_sub(200)..],
        &final_text[..final_text.len().min(200)],
    );

    // And the session is still usable afterwards -- a recovery that leaves a one-turn conversation
    // is not a recovery.
    provider
        .send_turn(SendTurnRequest { session_id: session_id.clone(), text: "reply with exactly: still-here".into() })
        .unwrap();
    let second = drain_until(&provider, Duration::from_secs(120), turn_ended);
    assert!(
        text_of(&second).contains("still-here"),
        "the session did not survive the recovery. got: {:?}",
        text_of(&second)
    );

    let mut projection = agent::AgentSessionProjection::default();
    for event in all.iter().chain(second.iter()) {
        projection.apply(event);
    }
    assert_eq!(projection.active_turn_id, None);
    assert!(!matches!(projection.status, ProjectionStatus::Unavailable { .. }));
}

/// **A gap the client cannot repair must be loud.**
///
/// Forced by starving the ring: capacity 1 means that by the time a broken watch reconnects, the
/// event immediately after its cursor has almost certainly been evicted. The server answers
/// EVENT_GAP, the client treats it as fatal rather than retrying (retrying replays the same
/// refusal), and the session ends visibly.
///
/// The alternative -- reconnecting at whatever is still retained -- would deliver a reply with a
/// hole in the middle and no indication that anything was missing. That is the outcome this whole
/// protocol round exists to make impossible.
#[test]
#[ignore]
fn a_gap_too_large_to_replay_ends_the_session_visibly() {
    let provider = connect_with(&[
        ("VERDANDI_CLAUDE_SIDECAR_RING_CAPACITY", "1"),
        ("VERDANDI_CLAUDE_SIDECAR_FAULT_DROP_WATCH_AFTER", "3"),
    ]);
    assert!(
        provider.info().event_buffer_policy.contains("bounded-1"),
        "the sidecar did not take the configured ring capacity, so this test would prove nothing. \
         it reports: {:?}",
        provider.info().event_buffer_policy
    );
    let session_id = open(&provider);

    provider
        .send_turn(SendTurnRequest { session_id, text: STREAMING_PROMPT.into() })
        .unwrap();

    let all = drain_until(&provider, Duration::from_secs(180), |events| {
        events.iter().any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. }))
    });

    let reason = all
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::SessionUnavailable { reason } => Some(reason.clone()),
            _ => None,
        })
        .expect(
            "the stream broke against a one-event replay buffer and the client recovered anyway -- \
             which means it accepted a hole. Re-run; if this repeats, the gap is not being reported.",
        );
    eprintln!("reported: {reason}");

    // Named as a replay problem, not as a generic transport failure: the two call for different
    // things from whoever reads it.
    assert!(
        reason.contains("replay buffer") || reason.contains("evicted") || reason.contains("older"),
        "a gap must be reported as a gap, not as an unexplained loss. got: {reason}"
    );

    let mut projection = agent::AgentSessionProjection::default();
    for event in &all {
        projection.apply(event);
    }
    assert!(matches!(projection.status, ProjectionStatus::Unavailable { .. }));
    assert_eq!(projection.active_turn_id, None, "a session that lost events is not still working");
}

/// **The same stall, but through a real `AgentConversation`.**
///
/// The test below measures the PROVIDER in isolation, where a stalled caller still lets its queue
/// grow -- that is the provider's own contract and it is unchanged. This one measures what the
/// product actually assembles: a conversation, whose ingestion thread drains that provider
/// continuously regardless of what the UI is doing. Both are true, and keeping them side by side is
/// the point: the raw queue did not disappear, it stopped being the UI's to hold.
#[test]
#[ignore]
fn a_conversation_keeps_ingesting_while_its_ui_is_stalled() {
    let provider = std::sync::Arc::new(
        ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string())
            .expect("connecting to a real sidecar should succeed"),
    );
    // Its own directory, inside this process's disposable state root: a real session writes a real
    // conversation record and takes a real session lease, and `test_workspace_dir` keeps both out
    // of the developer's own XDG directories (see `agent::state_dirs`).
    let cwd = agent::state_dirs::test_workspace_dir("replay-recovery");
    let mut conversation = agent::AgentConversation::create(provider.clone(), &cwd, PermissionMode::Bypass)
        .expect("creating a conversation should succeed");

    conversation.send_turn(STREAMING_PROMPT).expect("send_turn should succeed");

    // The UI never looks. Not once, for the whole turn.
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut worst_provider_queue = 0usize;
    while Instant::now() < deadline {
        worst_provider_queue = worst_provider_queue.max(provider.backpressure_stats().pending_events);
        let done = {
            let projection = conversation.projection();
            projection.active_turn_id.is_none() && projection.last_revision > 2
        };
        if done {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let ingest = conversation.ingest_stats();
    let wire = provider.backpressure_stats();
    eprintln!("---- a real turn with the UI stalled throughout ----");
    eprintln!("  provider events received : {}", wire.events_received);
    eprintln!("  events folded            : {}", ingest.events_ingested);
    eprintln!("  worst provider-side queue: {worst_provider_queue}");
    eprintln!("  deepest UI backlog       : {} (cap {})", ingest.max_ui_backlog, agent::UI_EVENT_QUEUE_CAPACITY);
    eprintln!("  resyncs owed to the UI   : {}", ingest.resyncs);
    eprintln!("  transcript messages      : {}", conversation.projection().transcript.len());

    // The premise: a real streamed turn actually happened.
    assert!(ingest.events_ingested > 50, "only {} events -- no real stream to stall against", ingest.events_ingested);

    // Ingestion kept up even though nothing rendered: the provider's own queue never became the
    // place the backlog lives.
    assert!(
        worst_provider_queue < 200,
        "the provider's queue reached {worst_provider_queue} -- ingestion is not draining it, which \
         would mean the unbounded pile simply moved one layer down"
    );
    assert!(
        ingest.max_ui_backlog <= agent::UI_EVENT_QUEUE_CAPACITY + 1,
        "the UI queue reached {}, past its cap",
        ingest.max_ui_backlog
    );

    // And the conversation is complete despite never having been rendered: partial updates coalesced
    // into real messages rather than being held as raw events waiting for a repaint.
    let projection = conversation.projection();
    assert!(!projection.transcript.is_empty(), "the turn produced no assistant message");
    assert_eq!(projection.active_turn_id, None, "the turn should have completed");
    assert!(!matches!(projection.status, ProjectionStatus::Unavailable { .. }));
}

/// **Is it lossless because recovery is bounded, or because something buffers without limit?**
///
/// The existing stall test answers only the first half of that question. This one measures the
/// cost: how deep the client's own queue gets, how far behind production delivery falls, and how
/// long catching up takes. It asserts the correctness property and REPORTS the rest, because the
/// numbers are the finding -- tuning a buffer before knowing them would be guessing.
#[test]
#[ignore]
fn a_stalled_consumer_is_lossless_and_its_cost_is_measured() {
    let provider = connect_with(&[]);
    let sidecar_pid = provider.sidecar_pid();
    let session_id = open(&provider);

    let before = provider.backpressure_stats();
    assert_eq!(before.pending_events, 0);

    provider
        .send_turn(SendTurnRequest { session_id: session_id.clone(), text: STREAMING_PROMPT.into() })
        .unwrap();

    // Deliberately do not pump. Everything produced in this window has to be held somewhere.
    eprintln!("stalling the consumer for 20s");
    let mut samples = Vec::new();
    for _ in 0..20 {
        std::thread::sleep(Duration::from_secs(1));
        let stats = provider.backpressure_stats();
        samples.push((stats.pending_events, stats.max_delivery_lag_ms, sidecar_rss_kib(sidecar_pid)));
    }

    let stalled = provider.backpressure_stats();
    let resumed_at = Instant::now();
    let all = drain_until(&provider, Duration::from_secs(180), turn_ended);
    let catch_up = resumed_at.elapsed();
    let after = provider.backpressure_stats();

    eprintln!("---- backpressure under a 20s stall ----");
    for (i, (pending, lag, rss)) in samples.iter().enumerate() {
        eprintln!(
            "  t+{:>2}s  pending={:>5}  max_lag={:>6}ms  sidecar_rss={}",
            i + 1,
            pending,
            lag,
            rss.map(|kib| format!("{kib}KiB")).unwrap_or_else(|| "unreadable".into()),
        );
    }
    eprintln!("  peak client-side queue depth : {}", stalled.pending_events);
    eprintln!("  events received total        : {}", after.events_received);
    eprintln!("  worst delivery lag           : {}ms", after.max_delivery_lag_ms);
    eprintln!("  lag once caught up           : {}ms", after.last_delivery_lag_ms);
    eprintln!("  time to drain after resuming : {}ms", catch_up.as_millis());

    // The correctness property, unchanged: a stall costs memory, not content.
    assert!(
        !all.iter().any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. })),
        "a stalled consumer must not be reported as a loss"
    );
    let final_text = all
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::TurnCompleted { result_text, .. } => Some(result_text.clone()),
            _ => None,
        })
        .expect("the turn never completed");
    assert!(text_of(&all).ends_with(&final_text), "text was lost or duplicated across the stall");

    // The premise: the stall actually built a backlog. Without this the numbers above could all be
    // zero and the test would still pass.
    assert!(stalled.pending_events > 50, "no real backlog accumulated: {}", stalled.pending_events);

    // THE SHAPE OF THE ANSWER, and it is not what a first guess suggests. The queue grows while
    // delivery lag stays flat at a millisecond or two -- so nothing in the transport is holding
    // events back. grpc-js and tonic both keep delivering eagerly, and every queued event is
    // sitting in THIS client's own unbounded `Vec<AgentDomainEvent>`, waiting for a `pump()` that
    // is not coming.
    //
    // That is the honest answer to "lossless because bounded, or because something buffers without
    // limit": it is the second, and the unbounded buffer is ours. Which is the good news -- it is
    // the one layer we can actually put a limit on, and the sidecar's own memory stays flat
    // throughout rather than absorbing the backlog invisibly on the far side of the wire.
    //
    // Asserted rather than merely printed, because if this ever inverts -- lag climbing while the
    // local queue stays small -- the backpressure has moved into the transport, where neither end
    // exposes a depth and the only symptom is memory growth in a process nobody is watching.
    assert!(
        after.max_delivery_lag_ms < 1000,
        "delivery lag climbed to {}ms while the consumer was stalled, which means the transport is \
         now doing the queueing rather than this client's own Vec. That moves the unbounded buffer \
         somewhere neither end can measure -- re-read the backpressure section before changing this.",
        after.max_delivery_lag_ms
    );

    provider
        .close_session(agent::CloseSessionRequest { session_id })
        .unwrap();
}

/// The sidecar's own resident memory, so a stall's cost is visible on BOTH sides of the wire and not
/// only in this client's queue -- the server-side gRPC queue is the one layer neither end exposes a
/// depth for, and RSS is the only observable it does move.
///
/// Read through `agent::process_probe` (Linux `/proc/<pid>/status`, macOS `proc_pidinfo`) for the pid
/// this provider reports having spawned, never by name.
/// Returns `None` rather than 0 when it cannot be read: a zero printed among real measurements reads
/// as "measured, and it was zero".
fn sidecar_rss_kib(pid: u32) -> Option<u64> {
    agent::process_probe::resident_kib(pid)
}
