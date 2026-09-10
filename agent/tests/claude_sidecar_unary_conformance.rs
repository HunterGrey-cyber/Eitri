//! Ignored tests of unary RPC round trips against a freshly spawned `claude-sidecar`.
//! Session and turn ids or provider errors confirm response shapes without model queries
//! or Claude API usage. These tests do not check end-to-end conversation content.

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
fn resume_session_is_honestly_unsupported() {
    let provider = ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string()).unwrap();
    let result = provider.resume_session(agent::ResumeSessionRequest { provider_session_id: "does-not-matter".into(), cwd: "/tmp".into() });
    assert!(matches!(result, Err(agent::ProviderError::UnsupportedCapability("resume"))));
    assert!(!provider.capabilities().resume);
}

#[test]
#[ignore]
fn close_session_on_an_unknown_id_surfaces_a_typed_provider_error() {
    let provider = ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string()).unwrap();
    let result = provider.close_session(agent::CloseSessionRequest { session_id: "does-not-exist".into() });
    assert!(matches!(result, Err(agent::ProviderError::Provider(_))), "got: {result:?}");
}
