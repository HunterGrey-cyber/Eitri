//! Real, end-to-end tests against the crate's actual public API (`AgentSession`), not
//! `AgentProcess` directly -- mirroring v1's own precedent of testing the crate's real public
//! surface end-to-end rather than an internal component in isolation. These spawn a real `claude`
//! CLI child process and cost real API usage; run explicitly with `--ignored`.
//!
//! Renamed from `real_process.rs` (Phase 0 of the Claude runtime/provider refactor,
//! docs/superpowers/specs/2026-09-09-claude-runtime-provider-refactor-design.md §15): this file is
//! the committed regression baseline the refactor's later phases must not silently break. Two
//! baseline items the design doc's Phase 0 section names are deliberately NOT re-tested here:
//! - "并发 permission" (concurrent permission) is already pinned at the pure-reducer level, with
//!   no real CLI cost, by
//!   `session_state.rs::two_concurrent_permission_requests_are_both_retained_and_independently_answerable`.
//! - "window close / orphan cleanup" is already pinned by real, repeated sandbox verification in
//!   `shell/MANUAL_VERIFICATION.md`'s "agent-ui verification" section (2026-09-08) -- a GUI
//!   concern outside this crate's own test surface.
//! - "WebView reload" is NOT pinned anywhere, despite an earlier draft of this plan claiming
//!   otherwise -- `shell/MANUAL_VERIFICATION.md:489-490` documents the opposite: `applySnapshot`
//!   "currently only ever fires once, on the panel's initial `"ready"` message -- never again for
//!   the life of the panel." Reload rehydration is a real, open Phase 0 gap, not a covered case.
//! See `agent/BACKEND_BASELINE.md` (added in this plan's Task 3) for the full baseline record.

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

/// Phase 0 baseline (docs/superpowers/specs/2026-09-09-claude-runtime-provider-refactor-design.md
/// §2.1, structural problem 2): pins the CURRENT, real cross-session collision this refactor
/// exists to fix. `agent::settings::HookSettings::generate` writes a directory-scoped
/// `<project_dir>/.claude/settings.local.json`, so two `AgentSession`s started against the same
/// `project_dir` silently share one hook config file -- whichever session started more recently
/// overwrites the file, and a real `PreToolUse` hook fired by ANY tool call in that directory gets
/// relayed to whichever socket the file currently names.
///
/// This asserts the failure mode directly and asymmetrically: session B (started after A, so B's
/// write is the one left on disk) receives session A's own permission request, and session A --
/// whose real tool call actually triggered the hook -- does not. A weaker "A never sees its own
/// request" check alone could also be explained by an unrelated bug; proving B receives it is what
/// makes this a real, unambiguous reproduction of the collision rather than a mere absence.
///
/// If this test starts failing (B no longer receives A's request), the directory-scoped settings
/// collision described in the design doc's §2.1 has already been fixed by a later phase of that
/// migration -- update or remove this baseline test as part of that fix, don't just relax the
/// assertion.
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
