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
//! Every test used `PermissionMode::Bypass`, which was deliberate and not laziness: BYPASS was the
//! only permission policy that milestone shipped, so the lifecycle had to be proven on the path
//! that would really run. (The Auto/`interactive` permission path has its own coverage in
//! `claude_sidecar_conformance.rs`.)
//!
//! **Since R07 (2026-09-27) every session here is gated** -- `INTERACTIVE`, the CLI in `default`,
//! which is again the path that really runs: the client can no longer ask for anything else, and
//! the product's bypass is neovibe answering `allow`. So a turn that uses a tool raises a
//! `PermissionRequested`, and `run_turn` answers each one `Allow`, standing in for that answer.

use agent::{
    AgentDomainEvent, AgentProvider, ClaudeSidecarProvider, CloseSessionRequest, CreateSessionRequest,
    InterruptTurnRequest, PermissionDecision, ResolvePermissionRequest, SendTurnRequest, TurnOutcome,
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

/// Prints a compact event trace under `--nocapture`. Kept permanently rather than added and
/// removed: when one of these tests fails, the sequence the provider actually produced is the first
/// thing anyone needs, and reconstructing it costs another billed run.
fn trace(label: &str, events: &[AgentDomainEvent]) {
    eprintln!("---- {label}: {} events ----", events.len());
    for event in events {
        match event {
            AgentDomainEvent::SessionOpened {
                session_id,
                provider_session_id,
                ..
            } => {
                eprintln!("  SessionOpened   session_id={session_id} provider_session_id={provider_session_id}")
            }
            AgentDomainEvent::TurnStarted { turn_id } => eprintln!("  TurnStarted     turn_id={turn_id}"),
            AgentDomainEvent::ContentDelta { kind, text, .. } => {
                eprintln!(
                    "  ContentDelta    {kind:?} {:?}",
                    text.chars().take(40).collect::<String>()
                )
            }
            AgentDomainEvent::ToolCallStarted { name, tool_use_id, .. } => {
                eprintln!("  ToolCallStarted {name} tool_use_id={tool_use_id}")
            }
            AgentDomainEvent::ToolCallCompleted {
                tool_use_id, is_error, ..
            } => {
                eprintln!("  ToolCallDone    tool_use_id={tool_use_id} is_error={is_error}")
            }
            AgentDomainEvent::PermissionRequested {
                permission_id,
                tool_name,
                ..
            } => {
                eprintln!("  PermissionReq   {tool_name} permission_id={permission_id}")
            }
            AgentDomainEvent::PermissionResolved { permission_id, outcome } => {
                eprintln!("  PermissionDone  permission_id={permission_id} {outcome:?}")
            }
            AgentDomainEvent::TurnCompleted { turn_id, outcome, .. } => {
                eprintln!("  TurnCompleted   turn_id={turn_id} {outcome:?}")
            }
            other => eprintln!("  {other:?}"),
        }
    }
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

fn create_gated_session(provider: &ClaudeSidecarProvider) -> String {
    create_gated_session_with(provider, agent::StreamingPreference::Partial)
}

/// The streaming preference is a parameter so the same helper can drive both sides of a real
/// before/after measurement rather than two divergent copies of the setup.
fn create_gated_session_with(provider: &ClaudeSidecarProvider, streaming: agent::StreamingPreference) -> String {
    provider
        .create_session(CreateSessionRequest {
            cwd: std::env::temp_dir().to_string_lossy().to_string(),
            streaming,
        })
        .expect("create_session should succeed")
}

/// Sends one turn and drains until it completes. Returns every event produced during it.
fn run_turn(
    provider: &ClaudeSidecarProvider,
    session_id: &str,
    text: &str,
    deadline_secs: u64,
) -> Vec<AgentDomainEvent> {
    provider
        .send_turn(SendTurnRequest {
            session_id: session_id.to_string(),
            text: text.to_string(),
        })
        .expect("send_turn should be accepted");
    let deadline = Instant::now() + Duration::from_secs(deadline_secs);
    let mut events = Vec::new();
    while Instant::now() < deadline {
        let batch = provider.pump();
        allow_every_request(provider, session_id, &batch);
        events.extend(batch);
        if events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::TurnCompleted { .. }))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::TurnCompleted { .. })),
        "turn {text:?} did not complete within {deadline_secs}s; got: {events:?}"
    );
    events
}

/// Answers every `PermissionRequested` in `events` with `Allow` -- what neovibe's bypass does since
/// R07, done here at the provider level because these tests drive the provider directly.
fn allow_every_request(provider: &ClaudeSidecarProvider, session_id: &str, events: &[AgentDomainEvent]) {
    for event in events {
        if let AgentDomainEvent::PermissionRequested { permission_id, .. } = event {
            provider
                .resolve_permission(ResolvePermissionRequest {
                    session_id: session_id.to_string(),
                    permission_id: permission_id.clone(),
                    decision: PermissionDecision::Allow,
                })
                .expect("answering a pending request should succeed");
        }
    }
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
    let session_id = create_gated_session(&provider);

    let first = run_turn(
        &provider,
        &session_id,
        "Remember this number for later: 4271. Reply with just: OK",
        60,
    );
    assert_eq!(
        turn_outcomes(&first),
        vec![TurnOutcome::Completed],
        "first turn: {first:?}"
    );

    let second = run_turn(
        &provider,
        &session_id,
        "What number did I ask you to remember? Reply with just the number.",
        60,
    );
    assert_eq!(
        turn_outcomes(&second),
        vec![TurnOutcome::Completed],
        "second turn: {second:?}"
    );

    let recalled = text_of(&second);
    assert!(
        recalled.contains("4271"),
        "the second turn did not recall the first turn's content -- the session did not carry over. got: {recalled:?}"
    );

    trace("turn 1", &first);
    trace("turn 2", &second);

    // Every SessionOpened must carry the sidecar's own session id -- the one CreateSession returned
    // and the one every later RPC is addressed with. This is not automatic: the message's own inner
    // `session_id` field carries the CLAUDE session UUID instead (a real Verdandi wire-naming
    // defect), so this assertion is what pins that `translate` reads the envelope.
    //
    // Note there is one SessionOpened PER TURN, not one per session: the Agent SDK emits a
    // `system`/`init` message at the start of each turn even within a single streaming session, and
    // Verdandi translates each into `session_ready`. Harmless -- the projection's fold is idempotent
    // for it -- but it means a consumer must not treat SessionOpened as "a new session began".
    let opened: Vec<(&String, &String)> = first
        .iter()
        .chain(second.iter())
        .filter_map(|e| match e {
            AgentDomainEvent::SessionOpened {
                session_id,
                provider_session_id,
                ..
            } => Some((session_id, provider_session_id)),
            _ => None,
        })
        .collect();
    assert!(
        !opened.is_empty(),
        "expected at least one SessionOpened; got: {first:?} {second:?}"
    );
    for (event_session_id, _) in &opened {
        assert_eq!(
            **event_session_id, session_id,
            "SessionOpened.session_id must be the sidecar's own id, not Claude's"
        );
    }
    // The Claude identity is stable across turns and genuinely distinct from the sidecar's.
    let provider_ids: std::collections::BTreeSet<&&String> = opened.iter().map(|(_, p)| p).collect();
    assert_eq!(
        provider_ids.len(),
        1,
        "the provider session id must not change between turns: {provider_ids:?}"
    );
    assert_ne!(
        ***provider_ids.iter().next().unwrap(),
        session_id,
        "the Claude session id and the sidecar session id are different identities and must not be collapsed"
    );

    // Two turns, two distinct turn ids, each announced by a real TurnStarted event.
    //
    // This is the assertion that caught Verdandi's `turn_started` never reaching the wire: the
    // kernel pushed it into its history log instead of the buffer `pump()` drains, so a
    // WatchSessionEvents subscriber -- which is not the caller of sendTurn and has no other way to
    // learn a turn began -- saw zero of them across two real turns. Without TurnStarted,
    // `active_turn_id` never sets, so no consumer can light a turn-in-progress indicator, gate a
    // Stop button, or reject a concurrent turn from authoritative state.
    let turn_ids: Vec<&String> = first
        .iter()
        .chain(second.iter())
        .filter_map(|e| match e {
            AgentDomainEvent::TurnStarted { turn_id } => Some(turn_id),
            _ => None,
        })
        .collect();
    assert_eq!(
        turn_ids.len(),
        2,
        "expected exactly two TurnStarted events, got: {turn_ids:?}"
    );
    assert_ne!(turn_ids[0], turn_ids[1], "two turns must not share a turn id");

    // And each TurnStarted must pair with the TurnCompleted for the same turn.
    let completed_ids: Vec<&String> = first
        .iter()
        .chain(second.iter())
        .filter_map(|e| match e {
            AgentDomainEvent::TurnCompleted { turn_id, .. } => Some(turn_id),
            _ => None,
        })
        .collect();
    assert_eq!(
        turn_ids, completed_ids,
        "every started turn must complete under the same id"
    );

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
    let session_id = create_gated_session(&provider);

    provider
        .send_turn(SendTurnRequest {
            session_id: session_id.clone(),
            text: "Count slowly from 1 to 200, writing each number on its own line. Do not stop early.".into(),
        })
        .unwrap();

    // Interrupt only once the turn is genuinely underway. Interrupting before the turn has started
    // would test a different (and much less interesting) thing: whether a no-op interrupt is safe.
    let started = drain_until(&provider, 60, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::ContentDelta { .. }))
    });
    assert!(
        started
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::ContentDelta { .. })),
        "the turn never produced any content to interrupt; got: {started:?}"
    );

    provider
        .interrupt_turn(InterruptTurnRequest {
            session_id: session_id.clone(),
        })
        .unwrap();

    let after_interrupt = drain_until(&provider, 60, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::TurnCompleted { .. }))
    });
    assert_eq!(
        turn_outcomes(&after_interrupt),
        vec![TurnOutcome::Interrupted],
        "an interrupted turn must terminate as Interrupted, not Completed or Failed; got: {after_interrupt:?}"
    );
    assert!(
        !after_interrupt
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::SessionClosed { .. })),
        "interrupt must not close the session; got: {after_interrupt:?}"
    );

    // The point of the test: the session is still usable.
    let second = run_turn(&provider, &session_id, "Reply with exactly the word: pong", 60);
    assert_eq!(
        turn_outcomes(&second),
        vec![TurnOutcome::Completed],
        "the session did not survive the interrupt; got: {second:?}"
    );
    assert!(
        text_of(&second).to_lowercase().contains("pong"),
        "got: {:?}",
        text_of(&second)
    );

    provider.close_session(CloseSessionRequest { session_id }).unwrap();
}

/// T2.3 -- BYPASS must genuinely bypass, and must be honest about it.
///
/// That was: no permission request raised, AND the tool actually ran. **Since R07 (2026-09-27)
/// the first half is inverted by design**: the session is gated, so the request IS raised, and the
/// bypass is the host's `Allow` (`run_turn` answers it). What stays is the half that gave the old
/// test its meaning -- the tool really ran and its real output reached the model -- plus the new
/// fact that makes it safe: the call went through the gate first.
#[test]
#[ignore]
fn real_gated_session_runs_a_tool_once_the_host_allows_it() {
    let provider = connect();
    assert!(
        provider.capabilities().interactive_permission_mode,
        "the sidecar must advertise the interactive (gated) policy, the only one sessions are created under"
    );
    let session_id = create_gated_session(&provider);

    let events = run_turn(
        &provider,
        &session_id,
        "Run the bash command `echo neovibe_bypass_marker_9f3a` and reply with its exact output.",
        90,
    );

    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. })),
        "a gated session must raise a permission request for the Bash call; got: {events:?}"
    );

    let tool_calls: Vec<&AgentDomainEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentDomainEvent::ToolCallStarted { .. }))
        .collect();
    assert!(
        !tool_calls.is_empty(),
        "no tool ran at all -- the Allow answered nothing. got: {events:?}"
    );

    let completed_without_error = events
        .iter()
        .any(|e| matches!(e, AgentDomainEvent::ToolCallCompleted { is_error: false, .. }));
    assert!(
        completed_without_error,
        "the tool call never completed successfully; got: {events:?}"
    );

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
            agent::process_probe::pid_is_alive(pid),
            "the sidecar pid {pid} should exist while the provider is alive"
        );
        let session_id = create_gated_session(&provider);
        provider.close_session(CloseSessionRequest { session_id }).unwrap();
        pid
    }; // provider dropped here: stdin closes, the sidecar sees EOF and exits

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && agent::process_probe::pid_is_alive(sidecar_pid) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !agent::process_probe::pid_is_alive(sidecar_pid),
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

    // Read from the client's own constant rather than a literal. This assertion sat at a literal 2
    // from 2026-09-12 until the protocol-3 pin landed on 2026-09-15, by which point `connect()`
    // would have REFUSED any sidecar that could satisfy it -- the test was unrunnable and green,
    // because it is `#[ignore]`d. A literal here can only ever be stale or redundant: the handshake
    // already refuses a mismatch, so what is worth pinning is that `info` reports the negotiated
    // value rather than a default.
    assert_eq!(info.protocol_major, agent::CLIENT_PROTOCOL_MAJOR);
    assert!(!info.sidecar_version.is_empty(), "got: {info:?}");
    // The provider states how much history it retains, and this client now keeps it. A test that
    // configures a small ring has no other way to confirm the configuration actually took effect.
    assert!(
        info.event_buffer_policy.starts_with("bounded-"),
        "the provider must state its replay capacity, got: {:?}",
        info.event_buffer_policy
    );
    assert!(!info.actual_claude_code_version.is_empty(), "got: {info:?}");
    assert!(!info.advertised_capabilities.is_empty(), "got: {info:?}");

    assert!(
        capabilities.interrupt,
        "interrupt_turn is advertised and exercised by this very suite"
    );
    assert!(capabilities.bypass_permission_mode);
    // Resume is advertised by the sidecar AND implemented by this client, so it reports true.
    // Fork is advertised on the wire but this client does not drive it, so the intersection is
    // false -- that asymmetry is the point of the intersection rule.
    assert!(
        capabilities.resume,
        "resume_session should be advertised and implemented by now"
    );
    assert!(
        !capabilities.fork,
        "fork must stay false until this client actually drives one"
    );
}

/// T4.6 -- the resume acceptance criterion, and the single most important test in this file.
///
/// An `Ok` RPC result proves nothing here. proto3 ignores unknown fields, so a client built against
/// a protocol revision without `resume_provider_session_id` sends a request the sidecar reads as a
/// plain fresh session -- succeeding, returning a session id, and losing the conversation, with no
/// error anywhere. That is the failure this test exists to catch, and it needs BOTH assertions:
///
///   1. the resumed conversation remembers a fact planted in the original. If only this were
///      asserted and it failed, the failure would read as "the model forgot" rather than "resume
///      silently did nothing".
///   2. the resumed session reports the SAME `provider_session_id` that was requested. If only this
///      were asserted, a resume that reconnected to the right session but carried no history would
///      pass.
///
/// Deliberately uses a SECOND, freshly-connected provider for the resume: resuming inside the same
/// provider instance would leave open whether the continuity came from the wire or from state the
/// first provider still had in memory.
#[test]
#[ignore]
fn real_resume_continues_the_same_provider_session_with_its_history() {
    let cwd = std::env::temp_dir().to_string_lossy().to_string();

    let (provider_session_id, original_session_id) = {
        let provider = connect();
        assert!(
            provider.capabilities().resume,
            "the sidecar must advertise resume before this can pass"
        );
        let session_id = create_gated_session(&provider);

        let events = run_turn(
            &provider,
            &session_id,
            "Remember this number for later: 5903. Reply with just: OK",
            60,
        );
        trace("original session", &events);
        let provider_session_id = events
            .iter()
            .find_map(|e| match e {
                AgentDomainEvent::SessionOpened {
                    provider_session_id, ..
                } => Some(provider_session_id.clone()),
                _ => None,
            })
            .expect("the first turn must report a provider session id");
        assert_ne!(
            provider_session_id, session_id,
            "the two identities must not be the same value"
        );

        provider
            .close_session(CloseSessionRequest {
                session_id: session_id.clone(),
            })
            .unwrap();
        (provider_session_id, session_id)
    }; // the whole first provider -- process, runtime thread and all -- is gone here

    let provider = connect();
    let resumed_session_id = provider
        .resume_session(agent::ResumeSessionRequest {
            provider_session_id: provider_session_id.clone(),
            cwd,
            streaming: agent::StreamingPreference::Partial,
        })
        .expect("resume_session should succeed against a real, closed session");
    assert_ne!(
        resumed_session_id, original_session_id,
        "resuming mints a NEW Verdandi session id; only the Claude identity continues"
    );

    let events = run_turn(
        &provider,
        &resumed_session_id,
        "What number did I ask you to remember? Reply with just the number.",
        60,
    );
    trace("resumed session", &events);

    // Assertion 2 first: it is the one that distinguishes "resume did nothing" from "the model
    // forgot", so a failure here explains a failure of assertion 1.
    let resumed_provider_ids: Vec<&String> = events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::SessionOpened {
                provider_session_id, ..
            } => Some(provider_session_id),
            _ => None,
        })
        .collect();
    assert!(
        !resumed_provider_ids.is_empty(),
        "the resumed turn reported no SessionOpened at all"
    );
    for id in &resumed_provider_ids {
        assert_eq!(
            **id, provider_session_id,
            "the resumed session reports a DIFFERENT Claude session id -- resume silently started a fresh one"
        );
    }

    // Assertion 1: the history really came back.
    let recalled = text_of(&events);
    assert!(
        recalled.contains("5903"),
        "the resumed conversation did not remember the original's content. got: {recalled:?}"
    );

    provider
        .close_session(CloseSessionRequest {
            session_id: resumed_session_id,
        })
        .unwrap();
}
