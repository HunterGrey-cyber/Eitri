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
//!   `agent/tests/projection.rs::two_concurrent_permission_requests_are_both_retained_and_independently_resolvable_in_either_order`.
//! - "window close / orphan cleanup" is already pinned by real, repeated sandbox verification in
//!   `shell/MANUAL_VERIFICATION.md`'s "agent-ui verification" section (2026-09-08) -- a GUI
//!   concern outside this crate's own test surface.
//! - "WebView reload" is now pinned too, by real sandbox verification in
//!   `shell/MANUAL_VERIFICATION.md`'s "agent-ui verification, protocol v2" section (2026-09-09,
//!   Task 5 of this same plan), check 4 -- closing what used to be a real, open Phase 0 gap. One
//!   honest caveat that verification itself found: a literal `WebView::reload()` call does NOT
//!   work on this panel's `load_html()`-loaded content (it leaves the page permanently blank, no
//!   further activity ever observed) -- the check used the real working substitute instead
//!   (re-invoking `load_html` with the same embedded document) and confirmed the resulting
//!   `snapshot` envelope genuinely rehydrates prior state, not that literal `reload()` does.
//! See `agent/BACKEND_BASELINE.md` (added in this plan's Task 3) for the full baseline record.

use agent::{
    AgentDomainEvent, AgentSession, PermissionDecision, PermissionMode, ProjectionStatus, TurnOutcome,
    CONSERVATIVE_DISALLOWED_TOOLS,
};

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

/// The allow half of the real round trip -- and, since 2026-09-15, the one place the
/// permission-to-tool-call link is checked against a real CLI rather than against a fixture.
///
/// The `tool_use_id` assertions below close a step the fixture evidence genuinely cannot:
/// `agent/CAPTURE_NOTES.md` step 5 shows a real hook stdin's id matching a real `tool_result`'s
/// `tool_use_id` in the same run, and a `tool_result`'s id is by protocol the id of the `tool_use`
/// block it answers -- but that run's own `tool_use` block was never quoted, so "the id on the
/// card equals the id on the tool call it marks" was an inference. Here both sides are real
/// objects in one live conversation.
///
/// **This test has not been run since those assertions were added** (adding them cost nothing;
/// running them costs a real billed turn). If it fails, the interesting output is the two ids, not
/// the boolean.
#[test]
#[ignore]
fn real_pretooluse_hook_allow_end_to_end() {
    let dir = std::env::temp_dir().join(format!("agent-session-hook-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut session = AgentSession::start(&dir, PermissionMode::Auto, &[]).unwrap();

    session.send_turn("run: echo hello, and tell me the output").unwrap();
    let mut gated_tool_use_id: Option<Option<String>> = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline
        && session.projection.pending_permissions.is_empty()
        && gated_tool_use_id.is_none()
    {
        for event in session.pump() {
            if let AgentDomainEvent::PermissionRequested { tool_use_id, .. } = event {
                gated_tool_use_id = Some(tool_use_id);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let gated_tool_use_id = gated_tool_use_id.expect("expected a real PreToolUse-hook-sourced PermissionRequested");
    // Asserted on the real event rather than on the projection record, so a projection that
    // happened to drop the field could not stand in for the wire carrying it.
    let gated_tool_use_id = gated_tool_use_id.expect(
        "a real PreToolUse hook relay must carry the payload's own tool_use_id -- without it the \
         permission card cannot name the call it gates",
    );

    let permission_id = session.projection.pending_permissions.keys().next().unwrap().clone();
    session
        .respond_permission(&permission_id, PermissionDecision::Allow)
        .unwrap();
    assert!(
        !session.projection.pending_permissions.contains_key(&permission_id),
        "respond_permission must remove the answered request immediately"
    );

    let result = drain_until_finished(&mut session);
    assert!(result.to_lowercase().contains("hello"), "got: {result}");

    // The step the capture notes leave as an inference: the id the hook gated really is the id of
    // a tool call in this same conversation's transcript. Checked after the turn, because the
    // assistant `tool_use` block and the `PreToolUse` hook have no guaranteed arrival order.
    let recorded: Vec<&str> = session
        .projection
        .tool_calls
        .iter()
        .map(|c| c.tool_use_id.as_str())
        .collect();
    assert!(
        recorded.contains(&gated_tool_use_id.as_str()),
        "the gated tool_use_id {gated_tool_use_id} names no tool call in this conversation; \
         the transcript recorded {recorded:?}"
    );

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
            if let AgentDomainEvent::TurnCompleted { result_text: text, .. } = event {
                result_text = Some(text);
            }
        }
        let pending_ids: Vec<String> = session.projection.pending_permissions.keys().cloned().collect();
        for permission_id in pending_ids {
            session
                .respond_permission(
                    &permission_id,
                    PermissionDecision::Deny {
                        reason: Some(DENY_REASON.to_string()),
                    },
                )
                .unwrap();
            denied_any = true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    assert!(
        denied_any,
        "expected at least one real PreToolUse-hook-sourced PermissionRequested to deny"
    );
    let result_text = result_text.expect("no TurnCompleted within 90s");

    let denied_results: Vec<_> = session
        .projection
        .tool_calls
        .iter()
        .filter_map(|call| {
            call.result
                .as_ref()
                .map(|r| (call.name.clone(), r.content.clone(), r.is_error))
        })
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

/// Was a Phase 0 baseline pinning the cross-session collision this refactor exists to fix; it now
/// pins the FIX, in the same shape, so a regression is a failure rather than a silence.
///
/// The collision it used to pin: `HookSettings::generate` wrote a directory-scoped
/// `<project_dir>/.claude/settings.local.json`, so two `AgentSession`s in one directory shared one
/// hook config. Whichever started later overwrote it, and a `PreToolUse` hook fired by ANY tool
/// call in that directory -- including a real human's own terminal session -- was relayed to
/// whichever socket the file currently named. The old assertion was deliberately asymmetric: B
/// receives A's request and A does not, because proving B receives it is what made it an
/// unambiguous reproduction rather than a mere absence.
///
/// A real-CLI spike on 2026-09-15 found the mechanism was worse than "sessions cross-wire": the
/// CLI re-reads that file per tool invocation, so the FIRST session to shut down deleted the gate
/// out from under a still-running second one, whose tools then ran with no `PreToolUse` at all.
/// See `agent::settings`'s module documentation for the spike's own table.
///
/// The fix passes the hook as `claude --settings '<json>'`, so it exists only in one process's
/// argv. This test therefore asserts the mirror image of what it used to: A -- whose real tool
/// call fires the hook -- sees its own request, and B sees nothing. It also asserts the directory
/// stays clean, since "no file exists to collide over" is the actual mechanism of the fix and an
/// assertion on routing alone would still pass if a file came back for some other reason.
#[test]
#[ignore]
fn real_two_sessions_in_the_same_project_dir_each_see_only_their_own_permission_hooks() {
    let dir = std::env::temp_dir().join(format!("agent-collision-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();

    let mut session_a = AgentSession::start(&dir, PermissionMode::Auto, &[]).unwrap();
    let mut session_b = AgentSession::start(&dir, PermissionMode::Auto, &[]).unwrap();

    // The mechanism, asserted before the behavior: starting two sessions must not have written
    // anything into the project. Under the old code this directory already held one
    // settings.local.json naming session B's socket.
    assert!(
        !dir.join(".claude").exists(),
        "starting a session must not create {:?} -- the hook config belongs in argv",
        dir.join(".claude")
    );

    session_a.send_turn("run: echo hello, and tell me the output").unwrap();

    let mut a_saw_request = false;
    let mut b_saw_request = false;
    // Unlike the old version, this loop does NOT stop at the first request seen: it must keep
    // watching B for the whole window, or "B stayed silent" would only mean "A answered first".
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
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
        if a_saw_request && !session_a.projection.tool_calls.is_empty() {
            // Give B a further grace window to (incorrectly) receive it too, rather than declaring
            // isolation the moment A is served.
            std::thread::sleep(std::time::Duration::from_millis(500));
            for event in session_b.pump() {
                if matches!(event, AgentDomainEvent::PermissionRequested { .. }) {
                    b_saw_request = true;
                }
            }
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // Confirm A really did trigger a real tool call this run, so a run where no hook fired at all
    // cannot be mistaken for evidence of isolation (kept from the Phase 0 version -- its own final
    // review raised exactly this).
    assert!(
        !session_a.projection.tool_calls.is_empty(),
        "session A never issued a real tool call this run -- re-run, this is not evidence of anything"
    );
    assert!(
        a_saw_request,
        "session A must receive the permission request its OWN tool call fired, in {dir:?}"
    );
    assert!(
        !b_saw_request,
        "session B received a request it never asked for -- the directory-scoped hook collision \
         has come back. See this test's doc comment and `agent::settings`."
    );
    assert!(
        !dir.join(".claude").exists(),
        "a live conversation must still not have written anything into the project directory"
    );

    let pending_ids: Vec<String> = session_a.projection.pending_permissions.keys().cloned().collect();
    for permission_id in pending_ids {
        let _ = session_a.respond_permission(
            &permission_id,
            PermissionDecision::Deny {
                reason: Some("test cleanup".to_string()),
            },
        );
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
        interrupt_events.iter().all(|e| matches!(
            e,
            AgentDomainEvent::PermissionResolved {
                outcome: agent::PermissionOutcome::CancelledByInterrupt,
                ..
            }
        )),
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
    assert_eq!(
        turn_outcome,
        Some(TurnOutcome::Interrupted),
        "expected the interrupted turn's own TurnCompleted to report outcome Interrupted"
    );
    assert!(session.projection.active_turn_id.is_none());
    assert_eq!(
        session.projection.status,
        ProjectionStatus::Running,
        "session must survive interrupt(), not end up Unavailable/Closed"
    );

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

/// **The product's own MVP sentence, as far as this crate can carry it**: "I tell it to change the
/// file I am looking at, I see a diff, I approve, the change lands."
///
/// This covers the first, third and fourth clauses. The diff is rendered by `agent-ui/web`, whose
/// own tests cover it, and the buffer reload is `neovibe-core::buffer_reload`'s. What is asserted
/// here is the part only a real CLI can settle: that with the product's own Auto-mode deny list the
/// model may reach for `Edit` at all, that the gate stops it, that the request carries the fields a
/// diff is drawn from, and that approving it really writes the file.
///
/// Before 2026-09-18 this test could not have been written. `Edit` was on the deny list in every
/// mode, so the model never reached for it and no permission card for an edit could exist -- the
/// MVP sentence failed at its first clause for a reason nothing in the UI explained.
///
/// Deliberately runs with the REAL `disallowed_tools_for(Auto)` rather than an empty list: an empty
/// list would prove the CLI can edit, which was never in doubt, and not that the product's own
/// policy permits it.
#[test]
#[ignore]
fn real_edit_under_the_auto_gate_carries_a_reviewable_request_and_then_writes_the_file() {
    let dir = std::env::temp_dir().join(format!("agent-edit-mvp-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("greeting.txt");
    std::fs::write(&target, "hello world\n").unwrap();

    let mut session = AgentSession::start(
        &dir,
        PermissionMode::Auto,
        agent::disallowed_tools_for(PermissionMode::Auto),
    )
    .unwrap();
    session
        .send_turn(&format!(
            "Use the Edit tool to change the word 'world' to 'neovibe' in {}. Do not use a shell.",
            target.display()
        ))
        .unwrap();

    // Answer every request as it arrives, by its own id. One turn raises several on CLI 2.1.272:
    // tools are deferred, so the model must `ToolSearch` for `Edit` first and each search is itself
    // a gated call. A harness that answers one and waits stalls behind the second -- measured, and
    // it cost a sibling project a fix before an event dump named it.
    let mut edit_request: Option<serde_json::Value> = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    let mut answered = std::collections::HashSet::new();
    let mut finished = false;
    while std::time::Instant::now() < deadline && !finished {
        for event in session.pump() {
            match &event {
                AgentDomainEvent::PermissionRequested {
                    permission_id,
                    tool_name,
                    input,
                    ..
                } => {
                    if tool_name == "Edit" {
                        edit_request = Some(input.clone());
                    }
                    if answered.insert(permission_id.clone()) {
                        session
                            .respond_permission(permission_id, PermissionDecision::Allow)
                            .unwrap();
                    }
                }
                AgentDomainEvent::TurnCompleted { .. } => finished = true,
                _ => {}
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        finished,
        "the turn never completed after answering {} request(s)",
        answered.len()
    );

    // Clause 1 and 2's data: the model reached for `Edit`, and the gated request carries exactly the
    // fields `agent-ui/web/src/diff.ts` draws a diff from. Asserted on the real wire value rather
    // than on the projection, so a projection that dropped a field could not stand in for it.
    let input = edit_request.expect(
        "the model never reached for `Edit`; with it on the deny list this is what used to happen, \
         and it is the failure the MVP sentence's first clause describes",
    );
    for field in ["file_path", "old_string", "new_string"] {
        assert!(
            input.get(field).is_some(),
            "a reviewable Edit request must carry {field}: {input}"
        );
    }
    assert_eq!(input["file_path"].as_str().unwrap(), target.display().to_string());

    // Clause 4: approving really wrote it.
    let after = std::fs::read_to_string(&target).unwrap();
    assert!(
        after.contains("neovibe"),
        "the approved edit did not land; the file holds: {after:?}"
    );
    assert!(!after.contains("world"), "the old text survived: {after:?}");

    session.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
