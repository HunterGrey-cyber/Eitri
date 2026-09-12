//! Retires the one real unresolved assumption Task 1 exists to close: that a plain Cargo git
//! dependency on Verdandi's repo (which has no root Cargo.toml or [workspace] anywhere) actually
//! resolves `crates/claude-runtime-protocol` by package name, and that its generated types are
//! genuinely usable from this crate's own test tree. See this plan's "Verified facts" section,
//! point 1 and 3, for the real verification this test's mere existence and passing re-confirms in
//! CI going forward.

use claude_runtime_protocol::v1::{ClaudeHostPolicy, ConfigurationProfile, CreateSessionRequest, PermissionMode};

#[test]
fn generated_types_construct_and_carry_the_expected_field_values() {
    let request = CreateSessionRequest {
        // resume/fork added 2026-09-11; a fresh session leaves the id absent (proto3 presence).
        resume_provider_session_id: None,
        fork: false,
        cwd: "/tmp/example".into(),
        policy: Some(ClaudeHostPolicy {
            configuration: ConfigurationProfile::Native as i32,
            permissions: PermissionMode::Interactive as i32,
            persistence: claude_runtime_protocol::v1::PersistenceMode::HostCli as i32,
            executable: claude_runtime_protocol::v1::ExecutableSource::HostCli as i32,
        }),
    };
    assert_eq!(request.cwd, "/tmp/example");
    let policy = request.policy.unwrap();
    assert_eq!(policy.configuration(), ConfigurationProfile::Native);
    assert_eq!(policy.permissions(), PermissionMode::Interactive);
}
