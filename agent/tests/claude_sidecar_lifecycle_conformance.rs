//! The long-lived session lifecycle, proven against a real sidecar and a real Claude CLI.
//!
//! `#[ignore]`d and billed: every test here spends real tokens. Run with
//! `cargo test -p agent --test claude_sidecar_lifecycle_conformance -- --ignored --test-threads=1`.
//!
//! Why this file exists at all. The legacy CLI backend has pinned multi-turn continuity since Phase
//! 0 (`backend_conformance.rs::real_multi_turn_conversation_in_one_process`); the sidecar provider
//! never had an equivalent. Every sidecar test written in Phase 3 sends exactly one turn, so the
//! whole "long-lived session" premise of this migration rested on reading Verdandi's kernel source
//! and observing that `turnInProgress` is cleared when a `result` message arrives. Reading is not
//! evidence -- this project has repeatedly shipped defects that only running could find. These
//! tests are the evidence.
//!
//! Every test uses `PermissionMode::Bypass`, which is deliberate and not laziness: BYPASS is the
//! only permission policy this milestone actually ships, so the lifecycle must be proven on the
//! path that will really run. (The Auto/`interactive` permission path has its own coverage in
//! `claude_sidecar_conformance.rs`.)

use agent::{
    AgentDomainEvent, AgentProvider, ClaudeSidecarProvider, CloseSessionRequest, CreateSessionRequest,
    InterruptTurnRequest, PermissionMode, SendTurnRequest, TurnOutcome,
};
use std::time::{Duration, Instant};

/// Polls `pump()` until `done` is satisfied or the deadline expires, accumulating every event seen.
/// Mirrors the polling style the existing sidecar conformance files already use rather than
/// inventing a second idiom.
fn drain_until<F: Fn(&[AgentDomainEvent]) -> bool>(
    provider: &ClaudeSidecarProvider,
    deadline_secs: u64,
    done: F,
) -> Vec<AgentDomainEvent> {
    let deadline = Instant::now() + Duration::from_secs(deadline_secs);
    let mut all_events = Vec::new();
    while Instant::now() < deadline {
        all_events.extend(provider.pump());
        if done(&all_events) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    all_events
}

fn text_of(events: &[AgentDomainEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::ContentDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn turn_outcomes(events: &[AgentDomainEvent]) -> Vec<TurnOutcome> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::TurnCompleted { outcome, .. } => Some(*outcome),
            _ => None,
        })
        .collect()
}

fn connect() -> ClaudeSidecarProvider {
    ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string())
        .expect("connecting to a real sidecar should succeed")
}

fn create_bypass_session(provider: &ClaudeSidecarProvider) -> String {
    provider
        .create_session(CreateSessionRequest {
            cwd: std::env::temp_dir().to_string_lossy().to_string(),
            permission_mode: PermissionMode::Bypass,
        })
        .expect("create_session should succeed")
}

/// Sends one turn and drains until it completes. Returns every event produced during it.
fn run_turn(provider: &ClaudeSidecarProvider, session_id: &str, text: &str, deadline_secs: u64) -> Vec<AgentDomainEvent> {
    provider
        .send_turn(SendTurnRequest { session_id: session_id.to_string(), text: text.to_string() })
        .expect("send_turn should be accepted");
    let events = drain_until(provider, deadline_secs, |events| {
        events.iter().any(|e| matches!(e, AgentDomainEvent::TurnCompleted { .. }))
    });
    assert!(
        events.iter().any(|e| matches!(e, AgentDomainEvent::TurnCompleted { .. })),
        "turn {text:?} did not complete within {deadline_secs}s; got: {events:?}"
    );
    events
}

/// T2.1 -- the sidecar analog of the legacy backend's own multi-turn conformance test.
///
/// The assertion is a CONTENT ORACLE, not an event count. Two `TurnCompleted` events prove only
/// that two turns ran; they would pass just as happily against two unrelated single-turn sessions.
/// Asking the model to recall a fact planted in turn 1 is what actually proves the second turn ran
/// on the same conversation with the first turn's context intact.
#[test]
#[ignore]
fn real_second_turn_on_the_same_session_recalls_the_first_turns_content() {
    let provider = connect();
    let session_id = create_bypass_session(&provider);

    let first = run_turn(
        &provider,
        &session_id,
        "Remember this number for later: 4271. Reply with just: OK",
        60,
    );
    assert_eq!(turn_outcomes(&first), vec![TurnOutcome::Completed], "first turn: {first:?}");

    let second = run_turn(
        &provider,
        &session_id,
        "What number did I ask you to remember? Reply with just the number.",
        60,
    );
    assert_eq!(turn_outcomes(&second), vec![TurnOutcome::Completed], "second turn: {second:?}");

    let recalled = text_of(&second);
    assert!(
        recalled.contains("4271"),
        "the second turn did not recall the first turn's content -- the session did not carry over. got: {recalled:?}"
    );

    // Both turns belong to the same session, and each got its own distinct turn id.
    let session_ids: Vec<&String> = first
        .iter()
        .chain(second.iter())
        .filter_map(|e| match e {
            AgentDomainEvent::SessionOpened { session_id, .. } => Some(session_id),
            _ => None,
        })
        .collect();
    assert!(session_ids.iter().all(|id| **id == session_id), "got: {session_ids:?}");

    let turn_ids: Vec<&String> = first
        .iter()
        .chain(second.iter())
        .filter_map(|e| match e {
            AgentDomainEvent::TurnStarted { turn_id } => Some(turn_id),
            _ => None,
        })
        .collect();
    assert_eq!(turn_ids.len(), 2, "expected exactly two TurnStarted events, got: {turn_ids:?}");
    assert_ne!(turn_ids[0], turn_ids[1], "two turns must not share a turn id");

    provider.close_session(CloseSessionRequest { session_id }).unwrap();
}

/// T2.2 -- interrupt must cancel the in-flight turn WITHOUT ending the session.
///
/// Verdandi's kernel documents this ("The session stays usable afterward (interrupt does not close
/// it)"), but nothing had ever confirmed it through the gRPC boundary. A provider whose session is
/// silently dead after an interrupt would look fine in every existing test -- they all close the
/// session immediately afterward.
#[test]
#[ignore]
fn real_interrupt_cancels_the_turn_and_the_session_still_accepts_another() {
    let provider = connect();
    let session_id = create_bypass_session(&provider);

    provider
        .send_turn(SendTurnRequest {
            session_id: session_id.clone(),
            text: "Count slowly from 1 to 200, writing each number on its own line. Do not stop early.".into(),
        })
        .unwrap();

    // Interrupt only once the turn is genuinely underway. Interrupting before the turn has started
    // would test a different (and much less interesting) thing: whether a no-op interrupt is safe.
    let started = drain_until(&provider, 60, |events| {
        events.iter().any(|e| matches!(e, AgentDomainEvent::ContentDelta { .. }))
    });
    assert!(
        started.iter().any(|e| matches!(e, AgentDomainEvent::ContentDelta { .. })),
        "the turn never produced any content to interrupt; got: {started:?}"
    );

    provider.interrupt_turn(InterruptTurnRequest { session_id: session_id.clone() }).unwrap();

    let after_interrupt = drain_until(&provider, 60, |events| {
        events.iter().any(|e| matches!(e, AgentDomainEvent::TurnCompleted { .. }))
    });
    assert_eq!(
        turn_outcomes(&after_interrupt),
        vec![TurnOutcome::Interrupted],
        "an interrupted turn must terminate as Interrupted, not Completed or Failed; got: {after_interrupt:?}"
    );
    assert!(
        !after_interrupt.iter().any(|e| matches!(e, AgentDomainEvent::SessionClosed { .. })),
        "interrupt must not close the session; got: {after_interrupt:?}"
    );

    // The point of the test: the session is still usable.
    let second = run_turn(&provider, &session_id, "Reply with exactly the word: pong", 60);
    assert_eq!(
        turn_outcomes(&second),
        vec![TurnOutcome::Completed],
        "the session did not survive the interrupt; got: {second:?}"
    );
    assert!(text_of(&second).to_lowercase().contains("pong"), "got: {:?}", text_of(&second));

    provider.close_session(CloseSessionRequest { session_id }).unwrap();
}

/// T2.3 -- BYPASS must genuinely bypass, and must be honest about it.
///
/// BYPASS is this milestone's only shipped permission policy, and until now every real sidecar test
/// used `PermissionMode::Auto`. Two things have to be true at once, and a test asserting only one
/// of them proves nothing useful:
///   1. NO permission request is raised (otherwise the UI, which has no permission surface in this
///      milestone, would deadlock on a request nobody can answer);
///   2. the tool actually RAN (otherwise "no permission request" would be trivially satisfied by a
///      policy that silently refuses every tool).
#[test]
#[ignore]
fn real_bypass_runs_a_tool_with_no_permission_request() {
    let provider = connect();
    assert!(
        provider.capabilities().bypass_permission_mode,
        "the sidecar must advertise the bypass permission mode before this milestone relies on it"
    );
    let session_id = create_bypass_session(&provider);

    let events = run_turn(
        &provider,
        &session_id,
        "Run the bash command `echo neovibe_bypass_marker_9f3a` and reply with its exact output.",
        90,
    );

    assert!(
        !events.iter().any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. })),
        "BYPASS raised a permission request, which nothing in this milestone can answer; got: {events:?}"
    );

    let tool_calls: Vec<&AgentDomainEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentDomainEvent::ToolCallStarted { .. }))
        .collect();
    assert!(
        !tool_calls.is_empty(),
        "no tool ran at all -- 'no permission request' is meaningless without this. got: {events:?}"
    );

    let completed_without_error = events.iter().any(|e| {
        matches!(e, AgentDomainEvent::ToolCallCompleted { is_error: false, .. })
    });
    assert!(completed_without_error, "the tool call never completed successfully; got: {events:?}");

    let reply = text_of(&events);
    assert!(
        reply.contains("neovibe_bypass_marker_9f3a"),
        "the model never reported the tool's real output, so the tool result did not reach it. got: {reply:?}"
    );

    provider.close_session(CloseSessionRequest { session_id }).unwrap();
}

/// T2.4 -- dropping the provider must leave no sidecar process behind.
///
/// Checked against the ONE pid this provider spawned, captured from the provider itself. Never by
/// name: Claude Code sessions on a developer machine are literally processes named `claude`, and a
/// name-matched search here would sweep up the session running this test.
#[test]
#[ignore]
fn dropping_the_provider_leaves_no_orphaned_sidecar() {
    let sidecar_pid = {
        let provider = connect();
        let pid = provider.sidecar_pid();
        assert!(
            std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "the sidecar pid {pid} should exist while the provider is alive"
        );
        let session_id = create_bypass_session(&provider);
        provider.close_session(CloseSessionRequest { session_id }).unwrap();
        pid
    }; // provider dropped here: stdin closes, the sidecar sees EOF and exits

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && std::path::Path::new(&format!("/proc/{sidecar_pid}")).exists() {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !std::path::Path::new(&format!("/proc/{sidecar_pid}")).exists(),
        "sidecar pid {sidecar_pid} outlived its provider -- this is a leak to report, not to clean up by hand"
    );
}

/// Free (no model turn, no billing): a connected provider reports real, wire-sourced capabilities
/// and version info rather than a hardcoded literal. Still `#[ignore]`d because it spawns a real
/// sidecar process.
#[test]
#[ignore]
fn a_connected_provider_reports_wire_sourced_capabilities_and_versions() {
    let provider = connect();
    let info = provider.info();
    let capabilities = provider.capabilities();

    assert_eq!(info.protocol_major, 1);
    assert!(!info.sidecar_version.is_empty(), "got: {info:?}");
    assert!(!info.actual_claude_code_version.is_empty(), "got: {info:?}");
    assert!(!info.advertised_capabilities.is_empty(), "got: {info:?}");

    assert!(capabilities.interrupt, "interrupt_turn is advertised and exercised by this very suite");
    assert!(capabilities.bypass_permission_mode);
    // Honest today, and a real regression guard: this must stay false until `resume_session` does
    // something other than return UnsupportedCapability.
    assert!(!capabilities.resume, "resume must not be advertised before it works end to end");
    assert!(matches!(
        provider.resume_session(agent::ResumeSessionRequest {
            provider_session_id: "irrelevant".into(),
            cwd: "/tmp".into()
        }),
        Err(agent::ProviderError::UnsupportedCapability("resume"))
    ));
}
