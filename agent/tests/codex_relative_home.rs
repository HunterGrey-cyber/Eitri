//! A Codex start under a relative `CODEX_HOME` inherited from Eitri's own environment. Codex resolves the
//! variable against its working directory, which is the project root, so the project's own directory of
//! that name would become the user's configuration. The start is refused before the per-binary check or
//! the session's child runs anything.
//!
//! The inherited variable is supplied to a fresh child test process.

use agent::providers::codex::{CodexLaunch, CodexProvider};
use agent::{ProviderError, ProviderErrorCode};

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
    script_fixture::write_executable_script(
        &program,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
        0o755,
    )
    .unwrap();

    if !agent::testkit::environment::in_child(&[("CODEX_HOME", Some(std::ffi::OsStr::new(".codex")))]) {
        std::fs::remove_dir_all(&dir).unwrap();
        return;
    }
    let result = CodexProvider::connect(CodexLaunch {
        program,
        cwd: dir.clone(),
        state_home: dir.clone(),
    });

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

#[cfg(test)]
mod script_fixture {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/executable.rs"));
}
