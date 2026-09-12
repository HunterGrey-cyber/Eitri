//! Confirms that Cargo resolves the `claude-runtime-protocol` package from a git dependency
//! without a root `Cargo.toml` or workspace. The test constructs its generated types and
//! checks that their fields carry the expected values.

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
