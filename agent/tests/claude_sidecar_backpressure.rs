//! Adversarial coverage for a consumer that cannot keep up, and for an event stream that stops.
//!
//! `#[ignore]`d and billed. Run with
//! `cargo test -p agent --test claude_sidecar_backpressure -- --ignored --test-threads=1 --nocapture`.
//!
//! Partial streaming raised one turn from ~1 content event to ~400, so "does streaming work" is no
//! longer the risk. The risk is what happens when the far end stalls or the stream breaks, and the
//! forbidden outcome is specific:
//!
//! ```text
//! missing chunks -> plausible-looking incomplete message -> no visible error
//! ```
//!
//! A truncated assistant reply that LOOKS finished is worse than a crash, because nothing tells the
//! user to distrust it. Every test here is written to detect exactly that, and prefers a loud typed
//! failure over any form of recovery-by-guessing.
//!
//! **Process safety**: the stream-death tests kill a sidecar. They kill ONLY the pid this test's own
//! provider reports having spawned. Nothing here matches processes by name -- Claude Code sessions
//! on a developer machine are themselves processes named `claude`.

use agent::{
    AgentDomainEvent, AgentProvider, ClaudeSidecarProvider, CloseSessionRequest, ContentKind,
    CreateSessionRequest, InterruptTurnRequest, PermissionMode, ProjectionStatus, SendTurnRequest,
    StreamingPreference, TurnOutcome,
};
use std::time::{Duration, Instant};

const LONG_PROMPT: &str =
    "Write about 800 words on the history of version control, from SCCS to modern distributed \
     systems. Continuous prose, no headings, no bullet points.";

fn connect() -> ClaudeSidecarProvider {
    ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string())
        .expect("connecting to a real sidecar should succeed")
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

fn summarize(label: &str, events: &[AgentDomainEvent]) {
    let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
    for event in events {
        let kind = match event {
            AgentDomainEvent::SessionOpened { .. } => "SessionOpened",
            AgentDomainEvent::TurnStarted { .. } => "TurnStarted",
            AgentDomainEvent::ContentDelta { kind: ContentKind::Text, .. } => "ContentDelta(text)",
            AgentDomainEvent::ContentDelta { .. } => "ContentDelta(thinking)",
            AgentDomainEvent::ToolCallStarted { .. } => "ToolCallStarted",
            AgentDomainEvent::ToolCallCompleted { .. } => "ToolCallCompleted",
            AgentDomainEvent::PermissionRequested { .. } => "PermissionRequested",
            AgentDomainEvent::PermissionResolved { .. } => "PermissionResolved",
            AgentDomainEvent::TurnCompleted { .. } => "TurnCompleted",
            AgentDomainEvent::ResumeOutcome { .. } => "ResumeOutcome",
            AgentDomainEvent::SessionUnavailable { .. } => "SessionUnavailable",
            AgentDomainEvent::SessionClosed { .. } => "SessionClosed",
        };
        *counts.entry(kind).or_default() += 1;
    }
    eprintln!("---- {label}: {} events ----", events.len());
    for (kind, n) in counts {
        eprintln!("  {n:>4} {kind}");
    }
}

fn drain_for(provider: &ClaudeSidecarProvider, window: Duration) -> Vec<AgentDomainEvent> {
    let deadline = Instant::now() + window;
    let mut events = Vec::new();
    while Instant::now() < deadline {
        events.extend(provider.pump());
        std::thread::sleep(Duration::from_millis(25));
    }
    events
}

/// **The forbidden failure mode, tested directly.**
///
/// A watch stream that dies mid-turn must produce a loud, typed loss signal. Before this was fixed,
/// the watch loop logged to stderr and broke, so the session simply stopped producing events: the
/// projection kept `active_turn_id = Some`, the transcript held whatever had streamed so far, and
/// nothing anywhere said the rest was never coming. A UI showing that is showing a truncated reply
/// with no error -- exactly what must never happen.
///
/// Killing the sidecar is the bluntest real cause, but it stands in for every way the stream can end
/// early (transport reset, sidecar OOM, a crash in the broadcast path): the client cannot tell them
/// apart and must not have to.
#[test]
#[ignore]
fn a_watch_stream_that_dies_mid_turn_reports_a_typed_loss_not_silence() {
    let provider = connect();
    let sidecar_pid = provider.sidecar_pid();
    let session_id = open(&provider);

    provider
        .send_turn(SendTurnRequest { session_id: session_id.clone(), text: LONG_PROMPT.to_string() })
        .unwrap();

    // Wait for real streamed content, so the kill genuinely lands mid-reply.
    let mut streamed = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline && text_of(&streamed).len() < 300 {
        streamed.extend(provider.pump());
        std::thread::sleep(Duration::from_millis(25));
    }
    let partial_text = text_of(&streamed);
    assert!(partial_text.len() >= 300, "no partial content arrived, so there was no mid-turn to interrupt");
    assert!(
        streamed.iter().all(|e| !matches!(e, AgentDomainEvent::TurnCompleted { .. })),
        "the turn finished before the stream could be killed mid-reply"
    );
    eprintln!("killing sidecar pid {sidecar_pid} after {} streamed chars", partial_text.len());

    // ONLY the pid this provider reports having spawned. Never by name.
    // SAFETY: `sidecar_pid` came from this provider's own `Child::id()`.
    let killed = unsafe { libc::kill(sidecar_pid as i32, libc::SIGKILL) };
    assert_eq!(killed, 0, "could not signal the sidecar this test spawned");

    let after_kill = drain_for(&provider, Duration::from_secs(20));
    summarize("after the sidecar was killed", &after_kill);

    let loss = after_kill.iter().find_map(|e| match e {
        AgentDomainEvent::SessionUnavailable { reason } => Some(reason.clone()),
        _ => None,
    });
    let reason = loss.expect(
        "a stream that died mid-turn produced NO SessionUnavailable -- the projection is left with an \
         active turn and a truncated reply that looks finished, with nothing for a UI to show",
    );
    eprintln!("reported loss reason: {reason}");
    assert!(!reason.trim().is_empty(), "the loss reason must say something a user can act on");

    // And the turn must not be left looking like it is still running.
    let mut projection = agent::AgentSessionProjection::default();
    for event in streamed.iter().chain(after_kill.iter()) {
        projection.apply(event);
    }
    assert!(
        matches!(projection.status, ProjectionStatus::Unavailable { .. }),
        "the projection must end up Unavailable, got {:?}",
        projection.status
    );
}

/// A consumer that stops draining entirely must lose nothing when it comes back.
///
/// This is the ordinary backpressure case: the GTK main loop is blocked (a long synchronous resize,
/// a modal, a slow render) so `pump()` is not called for many seconds while ~400 events are produced.
/// The question is whether anything is dropped in the sidecar, in gRPC flow control, or in this
/// client -- and the answer must be "nothing", because a silently shortened reply is the forbidden
/// outcome.
#[test]
#[ignore]
fn a_consumer_that_stalls_for_twenty_seconds_loses_nothing() {
    let provider = connect();
    let session_id = open(&provider);

    provider
        .send_turn(SendTurnRequest { session_id: session_id.clone(), text: LONG_PROMPT.to_string() })
        .unwrap();

    // Deliberately do not pump at all. Every event produced in this window has to be held somewhere.
    eprintln!("stalling the consumer for 20s while the reply streams");
    std::thread::sleep(Duration::from_secs(20));

    let backlog = provider.pump();
    eprintln!("backlog drained in one pump(): {} events", backlog.len());
    assert!(
        backlog.len() > 50,
        "expected a real backlog to have accumulated during the stall, got {}",
        backlog.len()
    );

    let mut all = backlog;
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline && !all.iter().any(|e| matches!(e, AgentDomainEvent::TurnCompleted { .. })) {
        all.extend(provider.pump());
        std::thread::sleep(Duration::from_millis(25));
    }
    summarize("after a 20s stall", &all);

    assert!(
        all.iter().any(|e| matches!(e, AgentDomainEvent::TurnCompleted { .. })),
        "the turn never completed after the stall"
    );
    assert!(
        !all.iter().any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. })),
        "a stalled consumer must not be reported as a loss -- nothing was actually lost"
    );

    // The reconciliation assertion is what proves nothing was dropped: a gap anywhere in the chain
    // would leave the accumulated stream shorter than the turn's own final text.
    let final_text = all
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::TurnCompleted { result_text, .. } => Some(result_text.clone()),
            _ => None,
        })
        .expect("a completed turn reports its result text");
    assert_eq!(
        text_of(&all),
        final_text,
        "text was lost or duplicated across a stalled consumer"
    );

    provider.close_session(CloseSessionRequest { session_id }).unwrap();
}

/// Interrupting while a backlog is still undrained must still terminate the turn correctly.
///
/// The interesting part is ordering: the interrupt is issued while hundreds of already-produced
/// events are still queued ahead of it, so the terminal event arrives after a long tail of content
/// the client had not yet seen. A client that decided the turn was over as soon as it issued the
/// interrupt would mis-order everything after it.
#[test]
#[ignore]
fn interrupting_with_an_undrained_backlog_still_terminates_the_turn_correctly() {
    let provider = connect();
    let session_id = open(&provider);

    provider
        .send_turn(SendTurnRequest { session_id: session_id.clone(), text: LONG_PROMPT.to_string() })
        .unwrap();

    // Let a real backlog build without draining it, then interrupt on top of it. 25s, not 12:
    // at 12s the first run of this test interrupted during the thinking phase, before a single text
    // delta existed, and passed on 5 total events -- proving nothing about ordering under backlog.
    // The sibling stall test measured ~213 events accumulating in 20s on this same prompt.
    std::thread::sleep(Duration::from_secs(25));
    provider.interrupt_turn(InterruptTurnRequest { session_id: session_id.clone() }).unwrap();
    eprintln!("interrupted with an undrained backlog");

    let mut all = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline && !all.iter().any(|e| matches!(e, AgentDomainEvent::TurnCompleted { .. })) {
        all.extend(provider.pump());
        std::thread::sleep(Duration::from_millis(25));
    }
    summarize("interrupt with backlog", &all);

    // Guards the test's own premise. Without this it passes vacuously whenever the interrupt lands
    // before streaming starts, which is exactly what happened the first time it was run.
    let content_events = all.iter().filter(|e| matches!(e, AgentDomainEvent::ContentDelta { .. })).count();
    assert!(
        content_events > 50,
        "only {content_events} content events -- no real backlog existed, so this run is not \
         evidence about ordering under backlog. Re-run."
    );

    let outcome = all.iter().find_map(|e| match e {
        AgentDomainEvent::TurnCompleted { outcome, .. } => Some(*outcome),
        _ => None,
    });
    assert_eq!(outcome, Some(TurnOutcome::Interrupted), "got: {outcome:?}");

    // Exactly one terminal event, and it is last among the turn's events -- not delivered early
    // because the client asked for it.
    let terminal_positions: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, AgentDomainEvent::TurnCompleted { .. }))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(terminal_positions.len(), 1, "exactly one terminal event per turn");
    let content_after_terminal = all[terminal_positions[0] + 1..]
        .iter()
        .filter(|e| matches!(e, AgentDomainEvent::ContentDelta { .. }))
        .count();
    assert_eq!(content_after_terminal, 0, "content arrived AFTER the turn's terminal event");

    // The session survives.
    let mut projection = agent::AgentSessionProjection::default();
    for event in &all {
        projection.apply(event);
    }
    assert_eq!(projection.active_turn_id, None, "the interrupted turn must not still be active");
    assert!(!matches!(projection.status, ProjectionStatus::Unavailable { .. }));

    provider.close_session(CloseSessionRequest { session_id }).unwrap();
}

/// Measures the SIDECAR's own write queue during a real partial-streaming turn, using the depth
/// seam Verdandi added on 2026-09-15 (`VERDANDI_CLAUDE_SIDECAR_WRITE_QUEUE_STATS_MS`).
///
/// **What this can and cannot show, stated first because the distinction is the finding.**
/// `a_consumer_that_stalls_for_twenty_seconds_loses_nothing` above does not stall the socket: this
/// client's watch task keeps reading from the transport throughout and pushes into its own `Vec`,
/// and `pump()` only drains that `Vec`. A consumer that has stopped calling `pump()` is therefore
/// still, at the transport layer, a perfectly attentive reader — which is why the server-side queue
/// has never grown in that test and why its `max_delivery_lag_ms < 1000` assertion passes. Unbounded
/// growth in grpc-js's write buffer needs a subscriber that stops reading the SOCKET, which this
/// client has no seam for and which cannot be produced by stopping the whole process: that also
/// blocks the sidecar's stderr pipe, and a sidecar blocked on its own logging is not the thing
/// under test.
///
/// So this reports the queue under the highest event rate a real consumer actually produces, and
/// under a `pump()` stall, and asserts only what it can honestly see. If `buffered_writes` stays 0
/// throughout, that is evidence the congestion path is not reached by this consumer at this rate —
/// not evidence that it cannot be reached.
#[test]
#[ignore]
fn the_sidecar_write_queue_under_a_real_turn_and_a_pump_stall() {
    for (k, v) in [("VERDANDI_CLAUDE_SIDECAR_WRITE_QUEUE_STATS_MS", "1000")] {
        std::env::set_var(k, v);
    }
    let provider = ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string())
        .expect("connecting to a real sidecar should succeed");
    std::env::remove_var("VERDANDI_CLAUDE_SIDECAR_WRITE_QUEUE_STATS_MS");

    let pid = provider.sidecar_pid();
    let session_id = open(&provider);
    let rss = |label: &str| {
        if let Some(kib) = sidecar_rss_kib(pid) {
            eprintln!("[rss] {label}: {kib} KiB");
        }
    };
    rss("before the turn");

    provider
        .send_turn(SendTurnRequest { session_id: session_id.clone(), text: LONG_PROMPT.to_string() })
        .unwrap();

    // Phase 1: drain attentively, the shape a healthy UI has. Highest sustained event rate.
    let mut all = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut ticks = 0u32;
    while Instant::now() < deadline && !all.iter().any(|e| matches!(e, AgentDomainEvent::TurnCompleted { .. })) {
        all.extend(provider.pump());
        ticks += 1;
        if ticks % 40 == 0 {
            rss("mid-turn, draining");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    rss("turn complete");
    summarize("attentive drain", &all);

    // Phase 2: a second turn with NO pumping at all, for 30s. The client's own Vec absorbs it; the
    // point is to see whether anything moves on the sidecar's side while it does.
    provider
        .send_turn(SendTurnRequest { session_id: session_id.clone(), text: LONG_PROMPT.to_string() })
        .unwrap();
    for i in 0..6 {
        std::thread::sleep(Duration::from_secs(5));
        rss(&format!("pump-stalled {}s", (i + 1) * 5));
    }
    let backlog = provider.pump();
    eprintln!("[stall] backlog drained in one pump(): {} events", backlog.len());

    let stats = provider.backpressure_stats();
    eprintln!(
        "[client] pending={} received={} max_lag_ms={} last_lag_ms={} watch_reconnects={}",
        stats.pending_events, stats.events_received, stats.max_delivery_lag_ms,
        stats.last_delivery_lag_ms, stats.watch_reconnects
    );

    eprintln!("[sidecar] write-queue lines it printed (ring holds the last 40 stderr lines):");
    let mut seen_any = false;
    for line in provider.sidecar_stderr_tail() {
        if line.contains("write-queue:") {
            seen_any = true;
            eprintln!("    {line}");
        }
    }
    assert!(seen_any, "the write-queue seam printed nothing -- is this sidecar built from a revision that has it?");

    provider.close_session(agent::CloseSessionRequest { session_id }).unwrap();
}

/// The sidecar's resident memory, by the pid this provider reports spawning -- never by name, since
/// a developer machine's own Claude Code session is itself a process named `claude`.
fn sidecar_rss_kib(pid: u32) -> Option<u64> {
    agent::process_probe::resident_kib(pid)
}
