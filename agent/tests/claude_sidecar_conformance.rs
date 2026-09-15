//! Real, `#[ignore]`d, real API cost -- the sidecar-provider analog of
//! `agent/tests/backend_conformance.rs`, exercising the same real behaviors that file pins for the
//! legacy backend, so the two are directly comparable (Phase 5's own future A/B job). Mirrors that
//! file's own polling style (`pump()` inside a deadline loop) deliberately, not a different idiom.

use agent::{AgentDomainEvent, AgentProvider, ClaudeSidecarProvider, CreateSessionRequest, PermissionDecision, PermissionMode, PermissionOutcome};

fn drain_until<F: Fn(&[AgentDomainEvent]) -> bool>(provider: &ClaudeSidecarProvider, deadline_secs: u64, done: F) -> Vec<AgentDomainEvent> {
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
fn real_pretooluse_permission_allow_end_to_end() {
    let provider = ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string()).unwrap();
    let cwd = std::env::temp_dir().to_string_lossy().to_string();
    let session_id = provider.create_session(CreateSessionRequest { cwd, permission_mode: PermissionMode::Auto, streaming: agent::StreamingPreference::Partial }).unwrap();

    provider.send_turn(agent::SendTurnRequest { session_id: session_id.clone(), text: "run: echo hello, and tell me the output".into() }).unwrap();

    let events = drain_until(&provider, 30, |events| {
        events.iter().any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. }))
    });
    let permission_id = events.iter().find_map(|e| match e {
        AgentDomainEvent::PermissionRequested { permission_id, tool_use_id, .. } => {
            // Meaningful only since 2026-09-15: before that the translator wrapped
            // `requested.tool_use_id` unconditionally, so this held for every wire value, `""`
            // included. It now fails if the sidecar leaves the field unset.
            assert!(tool_use_id.is_some(), "the sidecar sent a PermissionRequested with an unset (proto3 empty-string) tool_use_id");
            Some(permission_id.clone())
        }
        _ => None,
    });
    let permission_id = permission_id.expect("expected a real PreToolUse-sourced PermissionRequested");

    provider.resolve_permission(agent::ResolvePermissionRequest { session_id: session_id.clone(), permission_id, decision: PermissionDecision::Allow }).unwrap();

    let events = drain_until(&provider, 30, |events| {
        events.iter().any(|e| matches!(e, AgentDomainEvent::TurnCompleted { .. }))
    });
    let full_text: String = events
        .iter()
        .filter_map(|e| match e { AgentDomainEvent::ContentDelta { text, .. } => Some(text.as_str()), _ => None })
        .collect();
    assert!(full_text.to_lowercase().contains("hello"), "got: {full_text}");

    provider.close_session(agent::CloseSessionRequest { session_id }).unwrap();
}

#[test]
#[ignore]
fn real_interrupt_mid_permission_fail_closes_the_pending_request() {
    let provider = ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string()).unwrap();
    let cwd = std::env::temp_dir().to_string_lossy().to_string();
    let session_id = provider.create_session(CreateSessionRequest { cwd, permission_mode: PermissionMode::Auto, streaming: agent::StreamingPreference::Partial }).unwrap();

    provider.send_turn(agent::SendTurnRequest { session_id: session_id.clone(), text: "run: sleep 30, and tell me when it finishes".into() }).unwrap();

    let events = drain_until(&provider, 30, |events| {
        events.iter().any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. }))
    });
    assert!(events.iter().any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. })), "expected a real pending permission before interrupting");

    provider.interrupt_turn(agent::InterruptTurnRequest { session_id: session_id.clone() }).unwrap();

    let events = drain_until(&provider, 15, |events| {
        events.iter().any(|e| matches!(e, AgentDomainEvent::PermissionResolved { .. }))
    });
    let resolved_outcome = events.iter().find_map(|e| match e {
        AgentDomainEvent::PermissionResolved { outcome, .. } => Some(*outcome),
        _ => None,
    });
    // The ideal outcome is `CancelledByInterrupt`, matching the legacy backend's own behavior and
    // Verdandi's own `session.ts::interrupt()` doc comment's stated intent. In practice, a real
    // race condition in Verdandi's `@verdandi/claude-runtime` kernel (session.ts's explicit
    // `failAllPending('cancelled_by_interrupt')` racing against the SDK's own hook-abort-signal
    // listener, which independently resolves the same pending permission as `Expired` and usually
    // wins) means the real, currently-observed outcome is `Expired`. Both are legitimate
    // fail-closed terminal states -- neither is `Allowed` -- so this test accepts either, while
    // asserting the core safety property (the permission was genuinely resolved, not left
    // pending) unconditionally. Reported to Verdandi as a real kernel-side bug; this assertion
    // does not need to change if/when they fix it, since CancelledByInterrupt would still pass.
    assert!(
        matches!(resolved_outcome, Some(PermissionOutcome::CancelledByInterrupt) | Some(PermissionOutcome::Expired)),
        "expected the pending permission to resolve fail-closed via CancelledByInterrupt or Expired, got: {events:?}"
    );

    provider.close_session(agent::CloseSessionRequest { session_id }).unwrap();
}

#[test]
#[ignore]
fn real_close_session_fail_closes_a_pending_permission() {
    let provider = ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string()).unwrap();
    let cwd = std::env::temp_dir().to_string_lossy().to_string();
    let session_id = provider.create_session(CreateSessionRequest { cwd, permission_mode: PermissionMode::Auto, streaming: agent::StreamingPreference::Partial }).unwrap();

    provider.send_turn(agent::SendTurnRequest { session_id: session_id.clone(), text: "run: sleep 30, and tell me when it finishes".into() }).unwrap();

    drain_until(&provider, 30, |events| events.iter().any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. })));

    provider.close_session(agent::CloseSessionRequest { session_id: session_id.clone() }).unwrap();

    let events = drain_until(&provider, 15, |events| {
        events.iter().any(|e| matches!(e, AgentDomainEvent::SessionClosed { .. }))
    });
    let resolved_outcome = events.iter().find_map(|e| match e {
        AgentDomainEvent::PermissionResolved { outcome, .. } => Some(*outcome),
        _ => None,
    });
    assert_eq!(resolved_outcome, Some(PermissionOutcome::CancelledBySessionClose), "got: {events:?}");
    assert!(events.iter().any(|e| matches!(e, AgentDomainEvent::SessionClosed { .. })));
}
