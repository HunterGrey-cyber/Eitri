//! Real, `#[ignore]`d, real API cost -- the sidecar-provider analog of
//! `agent/tests/backend_conformance.rs`, exercising the same real behaviors that file pins for the
//! legacy backend, so the two are directly comparable (the ground for an A/B of the two backends). Mirrors that
//! file's own polling style (`pump()` inside a deadline loop) deliberately, not a different idiom.

use agent::{
    AgentDomainEvent, AgentProvider, ClaudeSidecarProvider, CreateSessionRequest, PermissionDecision, PermissionOutcome,
};

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
fn real_pretooluse_permission_allow_end_to_end() {
    let provider = ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string()).unwrap();
    let cwd = std::env::temp_dir().to_string_lossy().to_string();
    let session_id = provider
        // Trusted: this measures the real CLI with the project tiers loaded, as it always has.
        .create_session(CreateSessionRequest {
            cwd,
            streaming: agent::StreamingPreference::Partial,
            project: agent::setting_sources::ProjectTrust::Trusted,
            client_tools: None,
        })
        .unwrap();

    provider
        .send_turn(agent::SendTurnRequest {
            session_id: session_id.clone(),
            text: "run: echo hello, and tell me the output".into(),
        })
        .unwrap();

    // Answer EVERY permission this turn raises, each by its own id, until the turn completes.
    //
    // This used to answer exactly one and then wait for `TurnCompleted`, which was written when
    // `Bash` was in a session's initial tool set. On CLI 2.1.272 tools are deferred: the model
    // cannot call `Bash` until it has found it, the search is itself a gated tool call, and a real
    // turn therefore raises SEVERAL permission requests -- the first of them for `ToolSearch`, not
    // for the tool the prompt is about. Answering one and waiting stalls behind the second, and the
    // symptom is not a permission error: the turn simply never completes and the assistant text is
    // empty, which is why this read as a streaming bug for a day and cost a sibling project a fix
    // it did not owe. Verdandi's permission broker now documents the same rule from its side:
    // resolve every request by its own permissionId, assume no count, and assume nothing about
    // which tool the first one is for.
    //
    // Eitri's own product path was always right about this -- `AgentSessionProjection`'s
    // `pending_permissions` is a map and `agent-ui`'s is an array -- so what this fixes is the test,
    // not the client.
    let mut answered: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut saw_a_permission = false;
    let mut gated_tool_ids = std::collections::HashSet::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let mut all_events: Vec<AgentDomainEvent> = Vec::new();
    let mut completed = false;
    while std::time::Instant::now() < deadline && !completed {
        for event in provider.pump() {
            match &event {
                AgentDomainEvent::PermissionRequested {
                    permission_id,
                    tool_use_id,
                    ..
                } => {
                    // Meaningful only since 2026-09-15: before that the translator wrapped
                    // `requested.tool_use_id` unconditionally, so this held for every wire value,
                    // `""` included. It now fails if the sidecar leaves the field unset.
                    assert!(
                        tool_use_id.is_some(),
                        "the sidecar sent a PermissionRequested with an unset (proto3 empty-string) tool_use_id"
                    );
                    gated_tool_ids.insert(tool_use_id.clone().unwrap());
                    saw_a_permission = true;
                    if answered.insert(permission_id.clone()) {
                        provider
                            .resolve_permission(agent::ResolvePermissionRequest {
                                session_id: session_id.clone(),
                                permission_id: permission_id.clone(),
                                decision: PermissionDecision::Allow,
                            })
                            .unwrap();
                    }
                }
                AgentDomainEvent::TurnCompleted { .. } => completed = true,
                _ => {}
            }
            all_events.push(event);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    assert!(
        saw_a_permission,
        "expected at least one real PreToolUse-sourced PermissionRequested"
    );
    assert!(
        completed,
        "the turn never completed after answering {} permission request(s)",
        answered.len()
    );

    let full_text: String = all_events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::ContentDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(full_text.to_lowercase().contains("hello"), "got: {full_text}");

    let mut projection = agent::AgentSessionProjection::default();
    for event in &all_events {
        projection.apply(event);
    }
    assert!(
        projection.pending_permissions.is_empty(),
        "an answered request stayed pending"
    );
    for tool_id in gated_tool_ids {
        assert!(
            projection.tool_calls.iter().any(|call| call.tool_use_id == tool_id),
            "the gated tool_use_id {tool_id} names no tool call in this conversation"
        );
    }

    provider
        .close_session(agent::CloseSessionRequest { session_id })
        .unwrap();
}

#[test]
#[ignore]
fn real_pretooluse_permission_deny_end_to_end() {
    const REASON: &str = "test policy: marker writes are forbidden";
    let dir = agent::state_dirs::test_workspace_dir("sidecar-real-deny");
    let target = dir.join("denied-marker.txt");
    assert!(!target.exists());
    let provider = ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string()).unwrap();
    let session_id = provider
        .create_session(CreateSessionRequest {
            cwd: dir.to_string_lossy().into_owned(),
            streaming: agent::StreamingPreference::Partial,
            project: agent::setting_sources::ProjectTrust::Untrusted,
            client_tools: None,
        })
        .unwrap();
    provider
        .send_turn(agent::SendTurnRequest {
            session_id: session_id.clone(),
            text: format!(
                "Use a tool to create {} containing 'marker'. If permission is denied, stop and report the denial.",
                target.display()
            ),
        })
        .unwrap();

    let mut answered = std::collections::HashSet::new();
    let mut events = Vec::new();
    let mut completed = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while std::time::Instant::now() < deadline && !completed {
        for event in provider.pump() {
            match &event {
                AgentDomainEvent::PermissionRequested { permission_id, .. } => {
                    if answered.insert(permission_id.clone()) {
                        provider
                            .resolve_permission(agent::ResolvePermissionRequest {
                                session_id: session_id.clone(),
                                permission_id: permission_id.clone(),
                                decision: PermissionDecision::Deny {
                                    reason: Some(REASON.into()),
                                },
                            })
                            .unwrap();
                    }
                }
                AgentDomainEvent::TurnCompleted { .. } => completed = true,
                _ => {}
            }
            events.push(event);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        !answered.is_empty(),
        "no real permission request was denied: {events:?}"
    );
    assert!(completed, "the denied turn never completed: {events:?}");
    let results: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentDomainEvent::ToolCallCompleted { content, is_error, .. } => Some((content, *is_error)),
            _ => None,
        })
        .collect();
    assert!(
        !results.is_empty(),
        "no denied tool result reached the host: {events:?}"
    );
    assert!(
        results.iter().all(|(_, is_error)| *is_error),
        "a tool succeeded despite denying every request: {results:?}"
    );
    assert!(
        results
            .iter()
            .any(|(content, is_error)| *is_error && content.to_string().contains(REASON)),
        "no error result carried the deny reason verbatim: {results:?}"
    );
    // Completion follows all tool results, so the marker must still be absent after the turn.
    assert!(!target.exists(), "a denied tool created the marker");
    provider
        .close_session(agent::CloseSessionRequest { session_id })
        .unwrap();
    drop(provider);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore]
fn real_interrupt_mid_permission_fail_closes_the_pending_request() {
    let provider = ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string()).unwrap();
    let cwd = std::env::temp_dir().to_string_lossy().to_string();
    let session_id = provider
        .create_session(CreateSessionRequest {
            cwd,
            streaming: agent::StreamingPreference::Partial,
            project: agent::setting_sources::ProjectTrust::Trusted,
            client_tools: None,
        })
        .unwrap();

    provider
        .send_turn(agent::SendTurnRequest {
            session_id: session_id.clone(),
            text: "run: sleep 30, and tell me when it finishes".into(),
        })
        .unwrap();

    let events = drain_until(&provider, 30, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. }))
    });
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. })),
        "expected a real pending permission before interrupting"
    );
    let pending_ids: std::collections::HashSet<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentDomainEvent::PermissionRequested { permission_id, .. } => Some(permission_id.clone()),
            _ => None,
        })
        .collect();

    provider
        .interrupt_turn(agent::InterruptTurnRequest {
            session_id: session_id.clone(),
        })
        .unwrap();

    let events = drain_until(&provider, 15, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionResolved { .. }))
            && events
                .iter()
                .any(|e| matches!(e, AgentDomainEvent::TurnCompleted { .. }))
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
        matches!(
            resolved_outcome,
            Some(PermissionOutcome::CancelledByInterrupt) | Some(PermissionOutcome::Expired)
        ),
        "expected the pending permission to resolve fail-closed via CancelledByInterrupt or Expired, got: {events:?}"
    );
    for pending_id in pending_ids {
        assert!(
            events.iter().any(|event| matches!(
                event,
                AgentDomainEvent::PermissionResolved { permission_id, outcome }
                    if *permission_id == pending_id
                        && matches!(outcome, PermissionOutcome::CancelledByInterrupt | PermissionOutcome::Expired)
            )),
            "the interrupted request {pending_id} did not fail closed: {events:?}"
        );
    }
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentDomainEvent::TurnCompleted {
                outcome: agent::TurnOutcome::Interrupted,
                ..
            }
        )),
        "the interrupted turn did not finish as Interrupted: {events:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            AgentDomainEvent::SessionClosed { .. } | AgentDomainEvent::SessionUnavailable { .. }
        )),
        "interrupt ended the session: {events:?}"
    );

    provider
        .send_turn(agent::SendTurnRequest {
            session_id: session_id.clone(),
            text: "reply with exactly the word: pong".into(),
        })
        .unwrap();
    let follow_up = drain_until(&provider, 60, |events| {
        events
            .iter()
            .any(|event| matches!(event, AgentDomainEvent::TurnCompleted { .. }))
    });
    assert!(
        follow_up.iter().any(|event| matches!(
            event,
            AgentDomainEvent::TurnCompleted {
                outcome: agent::TurnOutcome::Completed,
                ..
            }
        )),
        "the same session did not complete a follow-up turn: {follow_up:?}"
    );
    let text: String = follow_up
        .iter()
        .filter_map(|event| match event {
            AgentDomainEvent::ContentDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(text.to_lowercase().contains("pong"), "got: {text}");

    provider
        .close_session(agent::CloseSessionRequest { session_id })
        .unwrap();
}

#[test]
#[ignore]
fn real_close_session_fail_closes_a_pending_permission() {
    let provider = ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string()).unwrap();
    let cwd = std::env::temp_dir().to_string_lossy().to_string();
    let session_id = provider
        .create_session(CreateSessionRequest {
            cwd,
            streaming: agent::StreamingPreference::Partial,
            project: agent::setting_sources::ProjectTrust::Trusted,
            client_tools: None,
        })
        .unwrap();

    provider
        .send_turn(agent::SendTurnRequest {
            session_id: session_id.clone(),
            text: "run: sleep 30, and tell me when it finishes".into(),
        })
        .unwrap();

    drain_until(&provider, 30, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. }))
    });

    provider
        .close_session(agent::CloseSessionRequest {
            session_id: session_id.clone(),
        })
        .unwrap();

    let events = drain_until(&provider, 15, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::SessionClosed { .. }))
    });
    let resolved_outcome = events.iter().find_map(|e| match e {
        AgentDomainEvent::PermissionResolved { outcome, .. } => Some(*outcome),
        _ => None,
    });
    assert_eq!(
        resolved_outcome,
        Some(PermissionOutcome::CancelledBySessionClose),
        "got: {events:?}"
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentDomainEvent::SessionClosed { .. })));
}

/// The MVP sentence's CLI-settleable clauses on the SIDECAR path.
///
/// The legacy twin is `backend_conformance.rs::real_edit_under_the_auto_gate_…`. This one matters
/// separately because the two backends express the same policy through different mechanisms: legacy
/// passes `--disallowedTools` on the command line, the sidecar sends `ClaudeHostPolicy.tool_policy`
/// over gRPC, and since 2026-09-18 both are built from `disallowed_tools_for(mode)` -- one
/// `disallowed_tools()` under the shared gate. A change that kept one working and broke the other
/// would be invisible in the other file.
#[test]
#[ignore]
fn real_edit_under_the_auto_gate_on_the_sidecar_path() {
    let dir = std::env::temp_dir().join(format!("sc-edit-mvp-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("greeting.txt");
    std::fs::write(&target, "alpha\nbravo\ncharlie\n").unwrap();

    let provider = ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string()).unwrap();
    let session_id = provider
        .create_session(CreateSessionRequest {
            cwd: dir.to_string_lossy().to_string(),
            streaming: agent::StreamingPreference::Partial,
            project: agent::setting_sources::ProjectTrust::Trusted,
            client_tools: None,
        })
        .unwrap();
    provider
        .send_turn(agent::SendTurnRequest {
            session_id: session_id.clone(),
            text: format!(
                "Use the Edit tool to change the word 'bravo' to 'BRAVO' in {}. Do not use a shell.",
                target.display()
            ),
        })
        .unwrap();

    // Every permission, by its own id -- tools are deferred on 2.1.272, so one turn raises several.
    let mut edit_input: Option<serde_json::Value> = None;
    let mut answered = std::collections::HashSet::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    let mut completed = false;
    while std::time::Instant::now() < deadline && !completed {
        for event in provider.pump() {
            match &event {
                AgentDomainEvent::PermissionRequested {
                    permission_id,
                    tool_name,
                    input,
                    ..
                } => {
                    if tool_name == "Edit" {
                        edit_input = Some(input.clone());
                    }
                    if answered.insert(permission_id.clone()) {
                        provider
                            .resolve_permission(agent::ResolvePermissionRequest {
                                session_id: session_id.clone(),
                                permission_id: permission_id.clone(),
                                decision: PermissionDecision::Allow,
                            })
                            .unwrap();
                    }
                }
                AgentDomainEvent::TurnCompleted { .. } => completed = true,
                _ => {}
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        completed,
        "the turn never completed after answering {} request(s)",
        answered.len()
    );

    let input = edit_input.expect(
        "the model never reached for `Edit` on the sidecar path -- check that tool_policy.deny is \
         built from disallowed_tools() and not from the conservative list",
    );
    for field in ["file_path", "old_string", "new_string"] {
        assert!(
            input.get(field).is_some(),
            "a reviewable Edit request must carry {field}: {input}"
        );
    }
    assert_eq!(input["file_path"].as_str().unwrap(), target.display().to_string());

    let after = std::fs::read_to_string(&target).unwrap();
    assert!(
        after.contains("BRAVO"),
        "the approved edit did not land; the file holds: {after:?}"
    );
    assert!(!after.contains("bravo"), "the old text survived: {after:?}");

    provider
        .close_session(agent::CloseSessionRequest { session_id })
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
