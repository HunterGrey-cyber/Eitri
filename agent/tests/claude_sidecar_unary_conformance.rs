//! Real, `#[ignore]`d tests against a real, freshly-spawned `claude-sidecar` process -- but no real
//! Claude API cost: every test here only exercises unary RPC round trips whose *shape* the sidecar
//! itself can confirm without ever spawning the real `claude` CLI (a session_id/turn_id coming
//! back, or a well-formed provider error). Real end-to-end conversation content is Task 9's job.

use agent::{AgentProvider, ClaudeSidecarProvider, CreateSessionRequest, PermissionMode};

#[test]
#[ignore]
fn create_session_returns_a_real_session_id() {
    let provider = ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string()).unwrap();
    let session_id = provider
        .create_session(CreateSessionRequest { cwd: std::env::temp_dir().to_string_lossy().to_string(), permission_mode: PermissionMode::Auto })
        .unwrap();
    assert!(!session_id.is_empty());
    provider.close_session(agent::CloseSessionRequest { session_id }).unwrap();
}

#[test]
#[ignore]
fn resume_is_now_advertised_and_an_empty_id_is_rejected_before_the_wire() {
    // This test used to assert resume was honestly UNSUPPORTED. It is supported as of the Verdandi
    // protocol change that added resume_provider_session_id to CreateSession; what it pins now is
    // the replacement honesty property -- the capability is real, and a request that cannot be
    // honored fails as a typed error rather than silently starting a fresh session.
    let provider = ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string()).unwrap();
    assert!(provider.capabilities().resume, "the sidecar must advertise resume_session");
    assert!(!provider.capabilities().fork, "fork is on the wire but this client does not drive it yet");

    let result = provider.resume_session(agent::ResumeSessionRequest {
        provider_session_id: "   ".into(),
        cwd: "/tmp".into(),
        permission_mode: PermissionMode::Bypass,
    });
    match result {
        Err(agent::ProviderError::Provider { code, .. }) => {
            assert_eq!(code, agent::ProviderErrorCode::InvalidConfiguration);
        }
        other => panic!("an empty resume id must be a typed error, never a fresh session: {other:?}"),
    }
}

#[test]
#[ignore]
fn close_session_on_an_unknown_id_surfaces_a_typed_provider_error() {
    let provider = ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string()).unwrap();
    let result = provider.close_session(agent::CloseSessionRequest { session_id: "does-not-exist".into() });
    // The typed code, not just "some provider error": the sidecar encodes a real SESSION_NOT_FOUND
    // in the grpc-status-details-bin trailer, and this crate now carries it across the boundary so a
    // caller can distinguish a dead session from a recoverable ordering complaint without parsing
    // English. Asserting only `Provider(_)` would have passed even while the code was discarded.
    match result {
        Err(agent::ProviderError::Provider { code, message }) => {
            assert_eq!(code, agent::ProviderErrorCode::SessionNotFound, "message was: {message}");
            assert!(!code.is_benign(), "an unknown session is not something to continue past");
        }
        other => panic!("expected a typed provider error, got: {other:?}"),
    }
}
