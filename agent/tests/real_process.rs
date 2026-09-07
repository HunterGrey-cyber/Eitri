//! Real, end-to-end tests against the crate's actual public API (`AgentSession`), not
//! `AgentProcess` directly -- mirroring v1's own precedent of testing the crate's real public
//! surface end-to-end rather than an internal component in isolation. These spawn a real `claude`
//! CLI child process and cost real API usage; run explicitly with `--ignored`.

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

/// The deny half of the same real round trip -- the single most safety-critical behavior in this
/// crate, and (until this test) the one thing the hook mechanism existed for that was never
/// actually exercised end-to-end against the real CLI. Task 1's fixture-based tests only proved
/// the wire-level JSON *shape* of a deny; this proves the real CLI genuinely honors one relayed
/// through the whole stack: real tool call -> real `PreToolUse` hook fires -> relayed by the real
/// `agent-hook` binary over the real socket -> answered `allow=false` with a real reason -> the
/// CLI actually blocks the tool and surfaces that exact reason back in the `tool_result`.
///
/// Denies EVERY permission request it sees (not just the first): a denied model commonly retries
/// with a different command, and each retry is its own real hook firing that must also be answered
/// or the turn stalls until the CLI's 600s hook timeout.
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

    // The CLI must have genuinely blocked the tool: at least one tool result came back as an
    // error, carrying this test's own deny reason verbatim (the real, captured behavior -- see
    // agent/CAPTURE_NOTES.md, where a custom hook reason landed verbatim in the transcript).
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
