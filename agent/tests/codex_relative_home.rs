//! A Codex start under a relative `CODEX_HOME` inherited from Eitri's own environment. Codex resolves the
//! variable against its working directory, which is the project root, so the project's own directory of
//! that name would become the user's configuration. The start is refused before the per-binary check or
//! the session's child runs anything.
//!
//! The variable is set on this test process, so this file holds one test and nothing else reads it.

use agent::providers::codex::{CodexLaunch, CodexProvider};
use agent::{ProviderError, ProviderErrorCode};
use std::os::unix::fs::PermissionsExt;

#[test]
fn an_inherited_relative_codex_home_refuses_the_start_before_the_check_runs() {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("eitri-inherited-home-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("ran");
    let program = dir.join("codex");
    std::fs::write(&program, format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display())).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();

    std::env::set_var("CODEX_HOME", ".codex");
    let result = CodexProvider::connect(CodexLaunch {
        program,
        cwd: dir.clone(),
        state_home: dir.clone(),
    });
    std::env::remove_var("CODEX_HOME");

    match result {
        Err(ProviderError::Provider { code, message }) => {
            assert_eq!(code, ProviderErrorCode::InvalidConfiguration);
            assert!(
                message.contains("CODEX_HOME") && message.contains(".codex"),
                "{message}"
            );
        }
        Err(other) => panic!("expected a refusal, got {other}"),
        Ok(_) => panic!("a relative CODEX_HOME connected"),
    }
    assert!(!marker.exists(), "the self-check or the session ran a child");
    std::fs::remove_dir_all(&dir).unwrap();
}
