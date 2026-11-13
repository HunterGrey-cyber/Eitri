//! Permission decisions through the default provider and the sidecar gRPC stand-in.
//! No real Claude CLI runs: the child writes a marker only when its Write request is allowed.

use agent::{
    AgentDomainEvent, AgentProvider, ClaudeSidecarProvider, CloseSessionRequest, CreateSessionRequest,
    InterruptTurnRequest, PermissionDecision, PermissionOutcome, ResolvePermissionRequest, SendTurnRequest,
    StreamingPreference, TurnOutcome,
};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        Self(agent::state_dirs::test_workspace_dir("sidecar-gate"))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn stand_in() -> ClaudeSidecarProvider {
    ClaudeSidecarProvider::connect_to_program(
        &uuid::Uuid::new_v4().to_string(),
        Path::new(env!("CARGO_BIN_EXE_eitri-sidecar-standin")),
    )
    .expect("the stand-in starts and answers the handshake")
}

fn until(provider: &ClaudeSidecarProvider, done: impl Fn(&[AgentDomainEvent]) -> bool) -> Vec<AgentDomainEvent> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut seen = Vec::new();
    loop {
        seen.extend(provider.pump());
        assert!(
            !seen
                .iter()
                .any(|event| matches!(event, AgentDomainEvent::SessionUnavailable { .. })),
            "the stand-in became unavailable: {seen:?}"
        );
        if done(&seen) {
            return seen;
        }
        assert!(
            Instant::now() < deadline,
            "never received the expected events: {seen:?}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn create(provider: &ClaudeSidecarProvider, cwd: &Path) -> String {
    let session_id = provider
        .create_session(CreateSessionRequest {
            cwd: cwd.to_string_lossy().into_owned(),
            streaming: StreamingPreference::Partial,
            project: agent::setting_sources::ProjectTrust::Untrusted,
            client_tools: None,
        })
        .unwrap();
    until(provider, |events| {
        events.iter().any(|event| {
            matches!(
                event, AgentDomainEvent::SessionOpened { session_id: id, .. } if *id == session_id
            )
        })
    });
    session_id
}

fn write_request(provider: &ClaudeSidecarProvider, session: &str, permission: &str, target: &Path) -> String {
    assert!(!target.exists());
    let turn = provider
        .send_turn(SendTurnRequest::text(
            session,
            format!("ask-write {permission} {};end", serde_json::to_string(target).unwrap()),
        ))
        .unwrap();
    let events = until(provider, |events| {
        events.iter().any(|event| {
            matches!(
                event, AgentDomainEvent::PermissionRequested { permission_id, .. } if permission_id == permission
            )
        })
    });
    let tool_id = events
        .iter()
        .find_map(|event| match event {
            AgentDomainEvent::PermissionRequested {
                permission_id,
                tool_use_id,
                tool_name,
                input,
                ..
            } if permission_id == permission => {
                assert_eq!(tool_name, "Write");
                assert_eq!(input["file_path"], target.to_string_lossy().as_ref());
                Some(tool_use_id.as_ref().expect("the gated call has an id"))
            }
            _ => None,
        })
        .unwrap();
    assert!(
        events.iter().any(|event| matches!(
            event, AgentDomainEvent::ToolCallStarted { turn_id, tool_use_id, .. }
                if *turn_id == turn && tool_use_id == tool_id
        )),
        "the permission must gate this turn's actual tool call: {events:?}"
    );
    assert!(!target.exists(), "the write ran before any permission answer");
    turn
}

fn answer(
    provider: &ClaudeSidecarProvider,
    session: &str,
    permission: &str,
    decision: PermissionDecision,
) -> Result<(), agent::ProviderError> {
    provider.resolve_permission(ResolvePermissionRequest {
        session_id: session.into(),
        permission_id: permission.into(),
        decision,
    })
}

fn completed(provider: &ClaudeSidecarProvider, turn: &str, outcome: TurnOutcome) -> Vec<AgentDomainEvent> {
    let events = until(provider, |events| {
        events.iter().any(|event| {
            matches!(
                event, AgentDomainEvent::TurnCompleted { turn_id, .. } if turn_id == turn
            )
        })
    });
    let outcomes: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentDomainEvent::TurnCompleted { turn_id, outcome, .. } if turn_id == turn => Some(*outcome),
            _ => None,
        })
        .collect();
    assert_eq!(outcomes, vec![outcome], "{events:?}");
    events
}

fn resolved(events: &[AgentDomainEvent], permission: &str, expected: PermissionOutcome) {
    let outcomes: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentDomainEvent::PermissionResolved { permission_id, outcome } if permission_id == permission => {
                Some(*outcome)
            }
            _ => None,
        })
        .collect();
    assert_eq!(outcomes, vec![expected], "{events:?}");
}

#[test]
fn a_denied_write_has_an_error_result_and_no_disk_effect() {
    const REASON: &str = "test policy: this write is not permitted";
    let scratch = Scratch::new();
    let provider = stand_in();
    let session = create(&provider, &scratch.0);
    let target = scratch.0.join("write marker.txt");
    let turn = write_request(&provider, &session, "deny-write", &target);
    answer(
        &provider,
        &session,
        "deny-write",
        PermissionDecision::Deny {
            reason: Some(REASON.into()),
        },
    )
    .unwrap();
    let events = completed(&provider, &turn, TurnOutcome::Completed);
    resolved(&events, "deny-write", PermissionOutcome::Denied);
    let results: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentDomainEvent::ToolCallCompleted {
                tool_use_id,
                content,
                is_error,
                ..
            } => Some((tool_use_id, content, *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1, "{events:?}");
    assert_eq!(results[0].0, "tool-deny-write");
    assert!(
        results.iter().all(|(_, _, is_error)| *is_error),
        "a denied call succeeded: {results:?}"
    );
    assert!(
        results[0].1.to_string().contains(REASON),
        "the deny reason was lost: {results:?}"
    );
    // Turn completion follows the child deciding whether to write, so absence here is final.
    assert!(!target.exists(), "a denied Write changed the disk");

    // The same child and target must actually write when allowed: an inert fixture cannot pass.
    let turn = write_request(&provider, &session, "allow-write", &target);
    answer(&provider, &session, "allow-write", PermissionDecision::Allow).unwrap();
    let events = completed(&provider, &turn, TurnOutcome::Completed);
    resolved(&events, "allow-write", PermissionOutcome::Allowed);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "allowed\n");
    provider
        .close_session(CloseSessionRequest { session_id: session })
        .unwrap();
}

#[test]
fn two_sessions_in_one_directory_answer_only_their_own_permissions() {
    for (a_permission, b_permission) in [("a-permission", "b-permission"), ("same-id", "same-id")] {
        let scratch = Scratch::new();
        let provider = stand_in();
        let a = create(&provider, &scratch.0);
        let b = create(&provider, &scratch.0);
        assert_ne!(a, b);
        let a_target = scratch.0.join("a.txt");
        let b_target = scratch.0.join("b.txt");
        let a_turn = write_request(&provider, &a, a_permission, &a_target);
        let b_turn = write_request(&provider, &b, b_permission, &b_target);
        assert_ne!(a_turn, b_turn);
        if a_permission != b_permission {
            assert!(
                answer(&provider, &b, a_permission, PermissionDecision::Allow).is_err(),
                "B must refuse A's request id"
            );
            assert!(
                answer(&provider, &a, b_permission, PermissionDecision::Allow).is_err(),
                "A must refuse B's request id"
            );
        }
        answer(&provider, &a, a_permission, PermissionDecision::Allow).unwrap();
        let events = completed(&provider, &a_turn, TurnOutcome::Completed);
        resolved(&events, a_permission, PermissionOutcome::Allowed);
        assert_eq!(std::fs::read_to_string(&a_target).unwrap(), "allowed\n");
        assert!(!b_target.exists(), "answering A also allowed B's pending write");
        assert!(
            !events.iter().any(|event| matches!(
                event, AgentDomainEvent::TurnCompleted { turn_id, .. } if *turn_id == b_turn
            )),
            "B completed while only A was answered: {events:?}"
        );
        // A second successful answer proves B stayed pending, even when both ids have the same spelling.
        answer(
            &provider,
            &b,
            b_permission,
            PermissionDecision::Deny {
                reason: Some("B is denied".into()),
            },
        )
        .unwrap();
        let events = completed(&provider, &b_turn, TurnOutcome::Completed);
        resolved(&events, b_permission, PermissionOutcome::Denied);
        provider.close_session(CloseSessionRequest { session_id: a }).unwrap();
        provider.close_session(CloseSessionRequest { session_id: b }).unwrap();
        drop(provider);
        assert!(!b_target.exists(), "B's denied write occurred before child shutdown");
    }
}

#[test]
fn two_providers_in_one_directory_answer_only_their_own_permissions() {
    let scratch = Scratch::new();
    let provider_a = stand_in();
    let provider_b = stand_in();
    assert_ne!(provider_a.sidecar_pid(), provider_b.sidecar_pid());
    let a = create(&provider_a, &scratch.0);
    let b = create(&provider_b, &scratch.0);
    let a_target = scratch.0.join("a.txt");
    let b_target = scratch.0.join("b.txt");

    // A completed control turn orders B's pump after its own sidecar has processed a request.
    let b_has_no_permission = || {
        let turn = provider_b
            .send_turn(SendTurnRequest::text(b.clone(), "text marker pong;end"))
            .unwrap();
        let events = completed(&provider_b, &turn, TurnOutcome::Completed);
        assert!(
            !events.iter().any(|event| matches!(
                event,
                AgentDomainEvent::PermissionRequested { .. } | AgentDomainEvent::PermissionResolved { .. }
            )),
            "B received A's permission events: {events:?}"
        );
    };
    let a_turn = write_request(&provider_a, &a, "a-only", &a_target);
    b_has_no_permission();
    answer(&provider_a, &a, "a-only", PermissionDecision::Allow).unwrap();
    let events = completed(&provider_a, &a_turn, TurnOutcome::Completed);
    resolved(&events, "a-only", PermissionOutcome::Allowed);
    assert_eq!(std::fs::read_to_string(&a_target).unwrap(), "allowed\n");
    b_has_no_permission();
    std::fs::remove_file(&a_target).unwrap();

    // Both children issue the same request id; A's answer must leave B's request pending.
    let a_turn = write_request(&provider_a, &a, "same-id", &a_target);
    let b_turn = write_request(&provider_b, &b, "same-id", &b_target);
    answer(&provider_a, &a, "same-id", PermissionDecision::Allow).unwrap();
    let events = completed(&provider_a, &a_turn, TurnOutcome::Completed);
    resolved(&events, "same-id", PermissionOutcome::Allowed);
    assert_eq!(std::fs::read_to_string(&a_target).unwrap(), "allowed\n");
    let events = provider_b.pump();
    assert!(
        !events.iter().any(|event| matches!(
            event,
            AgentDomainEvent::PermissionResolved { .. } | AgentDomainEvent::TurnCompleted { .. }
        )),
        "A's answer resolved B's request: {events:?}"
    );
    assert!(!b_target.exists(), "answering A also allowed B's write");
    answer(
        &provider_b,
        &b,
        "same-id",
        PermissionDecision::Deny {
            reason: Some("B is denied".into()),
        },
    )
    .unwrap();
    let events = completed(&provider_b, &b_turn, TurnOutcome::Completed);
    resolved(&events, "same-id", PermissionOutcome::Denied);
    provider_a.close_session(CloseSessionRequest { session_id: a }).unwrap();
    provider_b.close_session(CloseSessionRequest { session_id: b }).unwrap();
    drop(provider_a);
    drop(provider_b);
    assert!(!b_target.exists(), "B's denied write occurred before child shutdown");
}

#[test]
fn interrupting_a_pending_write_blocks_its_effect_and_keeps_the_session_usable() {
    let scratch = Scratch::new();
    let provider = stand_in();
    assert!(matches!(
        provider.interrupt_turn(InterruptTurnRequest {
            session_id: "does-not-exist".into(),
        }),
        Err(agent::ProviderError::Provider {
            code: agent::ProviderErrorCode::SessionNotFound,
            ..
        })
    ));
    let pid = provider.sidecar_pid();
    let session = create(&provider, &scratch.0);
    let target = scratch.0.join("interrupted.txt");
    let turn = write_request(&provider, &session, "interrupted", &target);
    provider
        .interrupt_turn(InterruptTurnRequest {
            session_id: session.clone(),
        })
        .unwrap();
    let events = completed(&provider, &turn, TurnOutcome::Interrupted);
    resolved(&events, "interrupted", PermissionOutcome::CancelledByInterrupt);
    assert!(!target.exists(), "the interrupted permission allowed a write");
    assert!(
        answer(&provider, &session, "interrupted", PermissionDecision::Allow).is_err(),
        "an interrupted request must not accept a late Allow"
    );
    let follow_up = provider
        .send_turn(SendTurnRequest::text(session.clone(), "text follow-up pong;end"))
        .unwrap();
    assert_ne!(turn, follow_up);
    let events = completed(&provider, &follow_up, TurnOutcome::Completed);
    assert!(
        events.iter().any(|event| matches!(event,
            AgentDomainEvent::ContentDelta { text, .. } if text == "pong"
        )),
        "the follow-up turn produced no reply: {events:?}"
    );
    assert_eq!(provider.sidecar_pid(), pid);
    assert!(agent::process_probe::pid_is_alive(pid));
    provider
        .close_session(CloseSessionRequest { session_id: session })
        .unwrap();
    drop(provider);
    assert!(!target.exists(), "the interrupted write happened later");
}

#[test]
fn shutdown_after_interrupt_reaps_the_spawned_sidecar_child() {
    let scratch = Scratch::new();
    let provider = stand_in();
    let session = create(&provider, &scratch.0);
    let pid = provider.sidecar_pid();
    assert!(agent::process_probe::pid_is_alive(pid));
    #[cfg(target_os = "linux")]
    let start = agent::child::start_time(pid).expect("the live child's OS start time");
    #[cfg(target_os = "macos")]
    let start = agent::process_probe::start_time(pid).expect("the live child's OS start time");
    let target = scratch.0.join("shutdown.txt");
    provider
        .send_turn(SendTurnRequest::text(
            session.clone(),
            format!(
                "ignore-eof;ask-write shutdown {};end",
                serde_json::to_string(&target).unwrap()
            ),
        ))
        .unwrap();
    until(&provider, |events| {
        events.iter().any(|event| {
            matches!(
                event, AgentDomainEvent::PermissionRequested { permission_id, .. } if permission_id == "shutdown"
            )
        })
    });
    provider
        .interrupt_turn(InterruptTurnRequest {
            session_id: session.clone(),
        })
        .unwrap();
    // EOF cannot end this child; closing must exhaust the grace period and kill/reap it.
    // Close immediately after interrupt, before draining completion, as a window close does.
    provider
        .close_session(CloseSessionRequest { session_id: session })
        .unwrap();
    drop(provider);
    let deadline = Instant::now() + Duration::from_secs(60);
    while agent::process_probe::pid_is_alive(pid) {
        #[cfg(target_os = "linux")]
        if agent::child::start_time(pid).is_ok_and(|current| current != start) {
            break;
        }
        #[cfg(target_os = "macos")]
        if agent::process_probe::start_time(pid).is_some_and(|current| current != start) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "child pid {pid} is still present (zombies count as unreaped)"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(!target.exists(), "shutdown let an interrupted write run");
}
