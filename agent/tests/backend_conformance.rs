//! End-to-end tests through the public `AgentSession` API. These spawn a real `claude`
//! child process and incur API usage; run explicitly with `--ignored`.
//!
//! Concurrent permissions are tested without CLI usage by
//! `session_state.rs::two_concurrent_permission_requests_are_both_retained_and_independently_answerable`.
//! Window close and orphan cleanup are GUI behaviors outside this crate's test surface.
//! WebView reload rehydration is not covered here. `applySnapshot` only fires for the panel's
//! initial `"ready"` message.

use agent::{AgentEvent, AgentSession, PermissionMode, CONSERVATIVE_DISALLOWED_TOOLS};

#[test]
#[ignore]
fn real_multi_turn_conversation_in_one_process() {
    let dir = std::env::temp_dir().join(format!("agent-session-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut session = AgentSession::start(&dir, PermissionMode::Auto, CONSERVATIVE_DISALLOWED_TOOLS).unwrap();

    session.send_turn("reply with exactly the word: pong").unwrap();
    let result1 = drain_until_finished(&mut session);
    assert!(result1.to_lowercase().contains("pong"));
    assert!(!session.state.turn_in_progress);

    session.send_turn("what word did you just say?").unwrap();
    let result2 = drain_until_finished(&mut session);
    assert!(result2.to_lowercase().contains("pong"), "got: {result2}");
    assert!(matches!(session.state.status, agent::SessionStatus::Running));

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
    while std::time::Instant::now() < deadline && session.state.pending_permissions.is_empty() && !saw_permission_request
    {
        for event in session.pump() {
            if matches!(event, AgentEvent::PermissionRequest { .. }) {
                saw_permission_request = true;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(saw_permission_request, "expected a real PreToolUse-hook-sourced PermissionRequest");
    let request_id = session.state.pending_permissions[0].request_id.clone();
    session.respond_permission(&request_id, true, None).unwrap();
    assert!(
        session.state.find_pending_permission(&request_id).is_none(),
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
            if let AgentEvent::TurnFinished { result_text: text, .. } = event {
                result_text = Some(text);
            }
        }
        for pending in session.state.pending_permissions.clone() {
            session.respond_permission(&pending.request_id, false, Some(DENY_REASON)).unwrap();
            denied_any = true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    assert!(denied_any, "expected at least one real PreToolUse-hook-sourced PermissionRequest to deny");
    let result_text = result_text.expect("no TurnFinished within 90s");

    // The CLI must have blocked the tool: at least one tool result must be an error
    // carrying this test's deny reason verbatim.
    let denied_results: Vec<_> = session
        .state
        .tool_calls
        .iter()
        .filter_map(|call| call.result.as_ref().map(|(content, is_error)| (call.name.clone(), content, *is_error)))
        .collect();
    assert!(
        denied_results.iter().any(|(_, content, is_error)| *is_error && content.to_string().contains(DENY_REASON)),
        "expected a real is_error tool_result carrying the deny reason verbatim, got: {denied_results:?}\nfinal text: {result_text}"
    );
    // And nothing was allowed to run: every tool call that completed came back as an error, since
    // this test denied every single request the hook relayed.
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
            if matches!(event, AgentEvent::PermissionRequest { .. }) {
                a_saw_request = true;
            }
        }
        for event in session_b.pump() {
            if matches!(event, AgentEvent::PermissionRequest { .. }) {
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

    // Deny whatever B received so the CLI's hook doesn't hang for its full 600s timeout, then shut
    // both sessions down cleanly.
    for pending in session_b.state.pending_permissions.clone() {
        let _ = session_b.respond_permission(&pending.request_id, false, Some("test cleanup"));
    }
    session_a.shutdown();
    session_b.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Phase 0 baseline: pins that `AgentSession::interrupt()` (a) denies and clears every pending
/// permission request immediately, (b) still lets the interrupted turn's own real `TurnFinished`
/// arrive afterward without flipping the session to `Finished`, and (c) leaves the conversation
/// genuinely usable for a further turn on the same process -- distinguishing `interrupt()` from
/// `shutdown()`, which ends the process outright.
#[test]
#[ignore]
fn real_interrupt_mid_permission_denies_pending_requests_without_ending_the_session() {
    let dir = std::env::temp_dir().join(format!("agent-interrupt-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut session = AgentSession::start(&dir, PermissionMode::Auto, &[]).unwrap();

    session.send_turn("run: echo hello, and tell me the output").unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline && session.state.pending_permissions.is_empty() {
        session.pump();
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        !session.state.pending_permissions.is_empty(),
        "expected a real PreToolUse-hook-sourced PermissionRequest before interrupting"
    );

    session.interrupt().unwrap();
    assert!(
        session.state.pending_permissions.is_empty(),
        "interrupt() must deny and clear every pending permission immediately"
    );

    // The CLI still emits its own terminal `result` line for the interrupted turn -- drain until
    // it arrives, confirming the reducer clears turn_in_progress without ending the session.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut saw_turn_finished = false;
    while std::time::Instant::now() < deadline && !saw_turn_finished {
        for event in session.pump() {
            if matches!(event, AgentEvent::TurnFinished { .. }) {
                saw_turn_finished = true;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(saw_turn_finished, "expected the interrupted turn's own TurnFinished to still arrive");
    assert!(!session.state.turn_in_progress);
    assert!(
        matches!(session.state.status, agent::SessionStatus::Running),
        "session must survive interrupt(), not end up Finished: got {:?}",
        session.state.status
    );

    // Prove the session is genuinely still usable: send one more turn on the same process.
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
            if let AgentEvent::TurnFinished { result_text, .. } = event {
                return result_text;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("no TurnFinished within 60s");
}
