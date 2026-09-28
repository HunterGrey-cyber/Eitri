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
            // permission_mode_switchable added at 133dc03. `false` is what every session built
            // before this field existed sends -- this smoke test does not exercise switching.
            permission_mode_switchable: false,
            // provider_permission_prompts added at b3aa188; stated `true` (what neovibe's own
            // `build_create_request` sends when the handshake advertises it) so the assertion below
            // proves the generated field carries a value, not merely that it compiles.
            provider_permission_prompts: true,
        }),
        // model/effort/system_prompt/output_format added at 133dc03, each `optional`/absent =
        // the CLI's own default -- unexercised by this smoke test.
        model: None,
        effort: None,
        system_prompt: None,
        output_format: None,
    };
    assert_eq!(request.cwd, "/tmp/example");
    let policy = request.policy.unwrap();
    assert!(policy.provider_permission_prompts);
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
