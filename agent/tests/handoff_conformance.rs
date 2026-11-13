//! Real, `#[ignore]`d: proves the Eitri->CLI handoff mechanism actually works
//! end to end against a real sidecar, a real `claude` CLI, and a real spawned
//! `eitri-claude-handoff` process. Real API cost: 1 real turn (to give the session real content
//! worth resuming). Does NOT drive the resumed `claude --resume` process's own interactive
//! session -- the handoff's own scope stops at "the handoff genuinely started the right real
//! process holding the right real lease"; asserting what that process does next needs the
//! CLI->Eitri return direction, which does not exist yet (it is blocked on Verdandi's
//! `ResumeSession` RPC).

use agent::handoff::prepare_eitri_to_cli_handoff;
use agent::lease::{LeaseError, SessionLease};
use agent::{AgentDomainEvent, AgentProvider, ClaudeSidecarProvider, CreateSessionRequest};

fn drain_until<F: Fn(&[AgentDomainEvent]) -> bool>(
    provider: &ClaudeSidecarProvider,
    deadline_secs: u64,
    done: F,
) -> Vec<AgentDomainEvent> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(deadline_secs);
    let mut all_events = Vec::new();
    while std::time::Instant::now() < deadline {
        all_events.extend(provider.pump());
        if done(&all_events) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    all_events
}

#[test]
#[ignore]
fn a_real_session_hands_off_to_a_real_claude_resume_process_holding_the_lease() {
    // Linux-only and refused before spawning or billing: `/proc/<pid>/comm` below proves the
    // handoff child exec'd into `claude`. macOS has no `/proc`; its `proc_name`/`p_comm` behavior
    // with a symlinked CLI has not been observed. Guessing that check could let `AlreadyHeld`
    // pass while the wrapper still holds the lock rather than proving inheritance by the CLI.
    assert!(
        cfg!(target_os = "linux"),
        "this conformance test's exec check is Linux-only (/proc/<pid>/comm) and has no verified macOS port"
    );
    let project_dir = std::env::temp_dir().join(format!("agent-handoff-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&project_dir).unwrap();
    let cwd = project_dir.to_string_lossy().to_string();

    let provider = ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string()).unwrap();
    // `session_id` here is the SIDECAR's own internal session id (what create_session returns,
    // and what send_turn/close_session's own `session_id` field expects -- the sidecar's
    // SessionRegistry is keyed by this value, NOT by the real Claude CLI session UUID). This is
    // genuinely a different value from `provider_session_id` below -- conflating them is exactly
    // the kind of real bug that once dropped `provider_session_id` on the sidecar path while the
    // sidecar's own id looked like a usable stand-in; do not "simplify" this test by using one
    // value for both.
    let session_id = provider
        // Trusted: this measures the real CLI with the project tiers loaded, as it always has.
        .create_session(CreateSessionRequest {
            cwd: cwd.clone(),
            streaming: agent::StreamingPreference::Partial,
            project: agent::setting_sources::ProjectTrust::Trusted,
            client_tools: None,
        })
        .unwrap();

    // `SessionOpened` carries the real Claude session UUID, and it is NOT emitted at
    // create_session time. Verdandi translates it from the Agent SDK's own `system`/`init`
    // message, which the SDK emits when a *query* starts -- so it cannot exist until a turn has
    // been sent. (Verified in verdandi's packages/claude-runtime/src/eventTranslation.ts, after a
    // first version of this test waited for it before send_turn and timed out for exactly this
    // reason.) So: send the turn first, then read the id out of the events collected along the way.
    provider
        .send_turn(agent::SendTurnRequest::text(
            session_id.clone(),
            "reply with exactly the word: pong",
        ))
        .unwrap();
    let events = drain_until(&provider, 30, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::TurnCompleted { .. }))
    });
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::TurnCompleted { .. })),
        "turn did not complete"
    );

    let provider_session_id = events
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::SessionOpened {
                provider_session_id, ..
            } => Some(provider_session_id.clone()),
            _ => None,
        })
        .expect("expected a real SessionOpened event carrying a provider_session_id");

    provider
        .close_session(agent::CloseSessionRequest {
            session_id: session_id.clone(),
        })
        .unwrap();

    // The handoff itself: acquire the lease, spawn eitri-claude-handoff, confirm it's really
    // running the real claude binary and really holding the lease.
    let outcome = prepare_eitri_to_cli_handoff("claude", &cwd, &provider_session_id).unwrap();
    assert!(outcome.child_pid > 0);

    // Confirm the spawned process is actually `claude` or its exec target through
    // `/proc/<pid>/comm`, before asserting `AlreadyHeld`. `eitri-claude-handoff` execs rather
    // than supervising a child, so lease survival depends on the CLI retaining the inherited fd.
    // Checking the lock first would only prove some process at `outcome.child_pid` holds it,
    // possibly the wrapper before exec. Checking comm first makes the lock assertion prove it
    // survived into the real CLI.
    let comm_path = format!("/proc/{}/comm", outcome.child_pid);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut comm = String::new();
    while std::time::Instant::now() < deadline {
        if let Ok(c) = std::fs::read_to_string(&comm_path) {
            comm = c;
            if comm.trim() == "claude" {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        comm.trim(),
        "claude",
        "expected the handoff's real spawned process to have exec'd into claude"
    );

    // Confirm a second acquire attempt of the SAME lease key genuinely fails now that the process
    // is confirmed to genuinely be `claude` -- this is the real, load-bearing assertion: the lease
    // is really held by the exec'd `claude`, not just "a process was spawned" (see the ordering
    // comment above for why this must run second).
    let second_attempt = SessionLease::try_acquire("claude", &cwd, &provider_session_id);
    assert!(
        matches!(second_attempt, Err(LeaseError::AlreadyHeld)),
        "expected AlreadyHeld while the handoff process is running, got: {second_attempt:?}"
    );

    // Clean up: kill the real spawned claude --resume process (this test never actually interacts
    // with its own terminal session -- it exists only to prove the handoff mechanism, not to be a
    // real interactive session left running after the test).
    let _ = std::process::Command::new("kill")
        .arg("-9")
        .arg(outcome.child_pid.to_string())
        .status();
    let _ = std::fs::remove_dir_all(&project_dir);

    // After killing the handoff process the lease must become acquirable again. This pins
    // flock's automatic release on death through the actual spawn, exec and kill sequence,
    // rather than only the isolated lease primitive.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut released = false;
    while std::time::Instant::now() < deadline {
        if SessionLease::try_acquire("claude", &cwd, &provider_session_id).is_ok() {
            released = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        released,
        "expected the lease to become acquirable again after killing the handoff process"
    );
}
