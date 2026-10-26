//! Cargo must resolve `claude-runtime-protocol` by package name from Verdandi's git repository
//! even though it has no root Cargo.toml or workspace. No subtree or path override is used.
//! Importing the generated types confirms they are usable from this crate's tests and catches
//! missing build inputs as well as an incompatible wire API.

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
                    claude_runtime_protocol::v1::SettingSource::User as i32,
                    claude_runtime_protocol::v1::SettingSource::Project as i32,
                    claude_runtime_protocol::v1::SettingSource::Local as i32,
                ],
            }),
            // `false` preserves the value sent by clients that predate `permission_mode_switchable`.
            // This smoke test does not exercise permission-mode switching.
            permission_mode_switchable: false,
            // Set `provider_permission_prompts` to `true`, as `build_create_request` does when
            // the handshake advertises it. The assertion checks that the generated field carries
            // the value, beyond merely compiling.
            provider_permission_prompts: true,
            // `build_create_request` sends AUTO for `cli_permission_mode` when the handshake
            // offers both `cli_auto_mode` and `permission_defer`.
            cli_permission_mode: claude_runtime_protocol::v1::CliPermissionMode::Auto as i32,
            // `accept_unsupported_cli: false` preserves the value sent by clients
            // that predate this field.
            accept_unsupported_cli: false,
        }),
        // Leave model, effort and output format at their defaults. With both prompt fields absent,
        // the Agent SDK uses its minimal system prompt.
        model: None,
        effort: None,
        system_prompt: None,
        system_prompt_preset: None,
        output_format: None,
        // `await_account_identity` is unexercised here.
        await_account_identity: false,
    };
    assert_eq!(request.cwd, "/tmp/example");
    let policy = request.policy.unwrap();
    assert!(policy.provider_permission_prompts);
    assert_eq!(
        policy.cli_permission_mode(),
        claude_runtime_protocol::v1::CliPermissionMode::Auto
    );
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
            claude_runtime_protocol::v1::SettingSource::User as i32,
            claude_runtime_protocol::v1::SettingSource::Project as i32,
            claude_runtime_protocol::v1::SettingSource::Local as i32,
        ],
        "all three tiers, in the enum's order, as build_create_request sends them by default"
    );
    assert_eq!(policy.configuration(), ConfigurationProfile::Native);
    assert_eq!(policy.permissions(), PermissionMode::Interactive);
}
