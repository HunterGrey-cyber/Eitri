//! End-to-end tests through the public `AgentSession` API. These spawn a real `claude`
//! child process and incur API usage; run explicitly with `--ignored`.
//!
//! Concurrent permissions are tested without CLI usage by
//! `agent/tests/projection.rs::two_concurrent_permission_requests_are_both_retained_and_independently_resolvable_in_either_order`.
//! Window close and orphan cleanup are GUI behaviors outside this crate's test surface.
//! For WebView rehydration, calling `load_html` again with the embedded document lets a
//! `snapshot` restore prior state. Literal `WebView::reload()` leaves this content blank.

use agent::{AgentDomainEvent, AgentSession, PermissionMode, ProjectionStatus, TurnOutcome, CONSERVATIVE_DISALLOWED_TOOLS};

#[test]
#[ignore]
fn real_multi_turn_conversation_in_one_process() {
    let dir = std::env::temp_dir().join(format!("agent-session-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut session = AgentSession::start(&dir, PermissionMode::Auto, CONSERVATIVE_DISALLOWED_TOOLS).unwrap();

    session.send_turn("reply with exactly the word: pong").unwrap();
    let result1 = drain_until_finished(&mut session);
    assert!(result1.to_lowercase().contains("pong"));
    assert!(session.projection.active_turn_id.is_none());

    session.send_turn("what word did you just say?").unwrap();
    let result2 = drain_until_finished(&mut session);
    assert!(result2.to_lowercase().contains("pong"), "got: {result2}");
    assert_eq!(session.projection.status, agent::ProjectionStatus::Running);

    session.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore]
fn real_pretooluse_hook_allow_end_to_end() {
    let dir = std::env::temp_dir().join(format!("agent-session-hook-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut session = AgentSession::start(&dir, PermissionMode::Auto, &[]).unwrap();

    session.send_turn("run: echo hello, and tell me the output").unwrap();
    let mut saw_permission_request = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline && session.projection.pending_permissions.is_empty() && !saw_permission_request {
        for event in session.pump() {
            if matches!(event, AgentDomainEvent::PermissionRequested { .. }) {
                saw_permission_request = true;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(saw_permission_request, "expected a real PreToolUse-hook-sourced PermissionRequested");
    let permission_id = session.projection.pending_permissions.keys().next().unwrap().clone();
    session.respond_permission(&permission_id, true, None).unwrap();
    assert!(
        !session.projection.pending_permissions.contains_key(&permission_id),
        "respond_permission must remove the answered request immediately"
    );

    let result = drain_until_finished(&mut session);
    assert!(result.to_lowercase().contains("hello"), "got: {result}");

    session.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Checks that the real CLI honors a denial relayed through the full hook path.
/// A tool call fires `PreToolUse`; `agent-hook` relays it over the socket, and an
/// `allow=false` response blocks the tool and returns the exact reason in `tool_result`.
/// Every permission request is denied because the model may retry with a different command.
/// Each retry fires another hook that needs an answer or the turn stalls until the 600s timeout.
#[test]
#[ignore]
fn real_pretooluse_hook_deny_end_to_end() {
    const DENY_REASON: &str = "neovibe test policy: shell commands are not permitted in this conversation";

    let dir = std::env::temp_dir().join(format!("agent-session-deny-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut session = AgentSession::start(&dir, PermissionMode::Auto, &[]).unwrap();

    session.send_turn("run: echo hello, and tell me the output").unwrap();

    let mut denied_any = false;
    let mut result_text = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    while std::time::Instant::now() < deadline && result_text.is_none() {
        for event in session.pump() {
            if let AgentDomainEvent::TurnCompleted { result_text: text, .. } = event {
                result_text = Some(text);
            }
        }
        let pending_ids: Vec<String> = session.projection.pending_permissions.keys().cloned().collect();
        for permission_id in pending_ids {
            session.respond_permission(&permission_id, false, Some(DENY_REASON)).unwrap();
            denied_any = true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    assert!(denied_any, "expected at least one real PreToolUse-hook-sourced PermissionRequested to deny");
    let result_text = result_text.expect("no TurnCompleted within 90s");

    let denied_results: Vec<_> = session
        .projection
        .tool_calls
        .iter()
        .filter_map(|call| call.result.as_ref().map(|r| (call.name.clone(), r.content.clone(), r.is_error)))
        .collect();
    assert!(
        denied_results.iter().any(|(_, content, is_error)| *is_error && content.to_string().contains(DENY_REASON)),
        "expected a real is_error tool_call_completed carrying the deny reason verbatim, got: {denied_results:?}\nfinal text: {result_text}"
    );
    assert!(
        denied_results.iter().all(|(_, _, is_error)| *is_error),
        "a tool call succeeded despite every permission request being denied: {denied_results:?}"
    );

    session.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Reproduces cross-session permission routing through directory-scoped hook settings.
/// `agent::settings::HookSettings::generate` writes `<project_dir>/.claude/settings.local.json`.
/// Two sessions in the same directory share this file. The later session overwrites it, so
/// `PreToolUse` hooks route to the socket named by the latest write.
///
/// Session B starts after A and must receive A's permission request while A does not.
/// Checking both sides distinguishes misrouting from an unrelated missing-request failure.
/// If B stops receiving A's request because routing is fixed, update or remove this baseline
/// test instead of weakening the assertion.
/// The non-empty `session_a.projection.tool_calls` assertion also confirms a tool call occurred,
/// so a run with no tool activity cannot masquerade as a reproduced collision.
#[test]
#[ignore]
fn real_two_sessions_in_the_same_project_dir_cross_wire_permission_hooks() {
    let dir = std::env::temp_dir().join(format!("agent-collision-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();

    let mut session_a = AgentSession::start(&dir, PermissionMode::Auto, &[]).unwrap();
    let mut session_b = AgentSession::start(&dir, PermissionMode::Auto, &[]).unwrap();

    session_a.send_turn("run: echo hello, and tell me the output").unwrap();

    let mut a_saw_request = false;
    let mut b_saw_request = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline && !a_saw_request && !b_saw_request {
        for event in session_a.pump() {
            if matches!(event, AgentDomainEvent::PermissionRequested { .. }) {
                a_saw_request = true;
            }
        }
        for event in session_b.pump() {
            if matches!(event, AgentDomainEvent::PermissionRequested { .. }) {
                b_saw_request = true;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    assert!(
        b_saw_request,
        "expected the CURRENT (buggy) behavior: session B receives session A's real hook request \
         because both share one settings.local.json in {dir:?}. If this now fails, see this \
         test's own doc comment -- the collision it pins may have already been fixed."
    );
    assert!(
        !a_saw_request,
        "session A should NOT see its own request once session B's settings.local.json write \
         took effect -- if both are seeing requests, the collision reproduced differently than \
         this test assumes and the test needs updating, not just the assertion relaxed"
    );
    // Confirm A really did trigger a real tool call this run (distinguishes "no hook fired at
    // all this run" from a genuine collision -- see the file's own module-level Minor-finding
    // note from Phase 0's final review).
    assert!(!session_a.projection.tool_calls.is_empty(), "session A never issued a real tool call this run -- re-run, this is not evidence of anything");

    let pending_ids: Vec<String> = session_b.projection.pending_permissions.keys().cloned().collect();
    for permission_id in pending_ids {
        let _ = session_b.respond_permission(&permission_id, false, Some("test cleanup"));
    }
    session_a.shutdown();
    session_b.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Phase 0 baseline: pins that `AgentSession::interrupt()` (a) denies and clears every pending
/// permission request immediately, (b) still lets the interrupted turn's own real `TurnCompleted`
/// arrive afterward without flipping the session's `ProjectionStatus` to `Closed`, and (c) leaves
/// the conversation genuinely usable for a further turn on the same process -- distinguishing
/// `interrupt()` from `shutdown()`, which ends the process outright.
#[test]
#[ignore]
fn real_interrupt_mid_permission_denies_pending_requests_without_ending_the_session() {
    let dir = std::env::temp_dir().join(format!("agent-interrupt-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut session = AgentSession::start(&dir, PermissionMode::Auto, &[]).unwrap();

    session.send_turn("run: echo hello, and tell me the output").unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline && session.projection.pending_permissions.is_empty() {
        session.pump();
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        !session.projection.pending_permissions.is_empty(),
        "expected a real PreToolUse-hook-sourced PermissionRequested before interrupting"
    );

    let interrupt_events = session.interrupt().unwrap();
    assert!(
        session.projection.pending_permissions.is_empty(),
        "interrupt() must deny and clear every pending permission immediately"
    );
    assert!(
        interrupt_events.iter().all(|e| matches!(e, AgentDomainEvent::PermissionResolved { outcome: agent::PermissionOutcome::CancelledByInterrupt, .. })),
        "interrupt() must return exactly the PermissionResolved events it caused, got: {interrupt_events:?}"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut turn_outcome = None;
    while std::time::Instant::now() < deadline && turn_outcome.is_none() {
        for event in session.pump() {
            if let AgentDomainEvent::TurnCompleted { outcome, .. } = event {
                turn_outcome = Some(outcome);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(turn_outcome, Some(TurnOutcome::Interrupted), "expected the interrupted turn's own TurnCompleted to report outcome Interrupted");
    assert!(session.projection.active_turn_id.is_none());
    assert_eq!(session.projection.status, ProjectionStatus::Running, "session must survive interrupt(), not end up Unavailable/Closed");

    session.send_turn("reply with exactly the word: pong").unwrap();
    let result = drain_until_finished(&mut session);
    assert!(result.to_lowercase().contains("pong"), "got: {result}");

    session.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

fn drain_until_finished(session: &mut agent::AgentSession) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while std::time::Instant::now() < deadline {
        for event in session.pump() {
            if let AgentDomainEvent::TurnCompleted { result_text, .. } = event {
                return result_text;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("no TurnCompleted within 60s");
}
