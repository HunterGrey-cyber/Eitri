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
            // streaming added 2026-09-12; PARTIAL is what a UI client asks for.
            streaming: claude_runtime_protocol::v1::StreamingMode::Partial as i32,
            executable: claude_runtime_protocol::v1::ExecutableSource::HostCli as i32,
            // tool_policy and setting_sources added 2026-09-15 with protocol major 3. Both are
            // stated here rather than left `None`, because this test exists to prove the generated
            // types carry what the wire declares -- and a `None` would prove only that the field
            // compiles.
            tool_policy: Some(claude_runtime_protocol::v1::ToolPolicy {
                deny: vec!["Bash".to_string()],
                // False beside a stated restriction, which is the only pair the sidecar accepts:
                // it rejects `unrestricted` alongside a deny list rather than guessing.
                unrestricted: false,
                allow: None,
            }),
            setting_sources: Some(claude_runtime_protocol::v1::SettingSourceSelection {
                sources: vec![
                    claude_runtime_protocol::v1::SettingSource::Project as i32,
                    claude_runtime_protocol::v1::SettingSource::Local as i32,
                ],
            }),
            // `false` preserves the value sent by clients that predate `permission_mode_switchable`.
            // This smoke test does not exercise permission-mode switching.
            permission_mode_switchable: false,
        }),
        // Absent `model`, `effort`, `system_prompt` and `output_format` use the CLI defaults.
        // This smoke test does not exercise these options.
        model: None,
        effort: None,
        system_prompt: None,
        output_format: None,
    };
    assert_eq!(request.cwd, "/tmp/example");
    let policy = request.policy.unwrap();
    let tool_policy = policy.tool_policy.clone().expect("a stated tool policy must survive");
    assert_eq!(tool_policy.deny, vec!["Bash".to_string()]);
    assert!(
        tool_policy.allow.is_none(),
        "absent is not the same as an empty allow list"
    );
    assert_eq!(
        policy
            .setting_sources
            .clone()
            .expect("a stated tier set must survive")
            .sources,
        vec![
            claude_runtime_protocol::v1::SettingSource::Project as i32,
            claude_runtime_protocol::v1::SettingSource::Local as i32,
        ],
        "user is deliberately absent -- see build_create_request"
    );
    assert_eq!(policy.configuration(), ConfigurationProfile::Native);
    assert_eq!(policy.permissions(), PermissionMode::Interactive);
}
