//! A Codex start under an absolute `CODEX_HOME` inherited from Eitri's own environment that lies inside
//! the project root. Codex would read that directory, which the project ships, as the user's own
//! configuration, and trust discovery never asks about the effective `CODEX_HOME`. The start is refused
//! before the per-binary check or the session's child runs anything.
//!
//! The inherited variable is supplied to a fresh child test process.

use agent::providers::codex::{refuse_codex_home, CodexLaunch, CodexProvider};
use agent::{ProviderError, ProviderErrorCode};

#[test]
fn an_inherited_codex_home_inside_the_project_refuses_the_start_before_the_check_runs() {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::var_os("EITRI_HOME_FIXTURE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("eitri-home-in-project-{}-{stamp}", std::process::id())));
    let root = dir.join("project");
    std::fs::create_dir_all(root.join(".codex")).unwrap();
    let root = std::fs::canonicalize(&root).unwrap();
    let marker = dir.join("ran");
    let program = dir.join("codex");
    script_fixture::write_executable_script(
        &program,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
        0o755,
    )
    .unwrap();

    if !agent::testkit::environment::in_child(&[
        ("CODEX_HOME", Some(root.join(".codex").as_os_str())),
        ("EITRI_HOME_FIXTURE_DIR", Some(dir.as_os_str())),
    ]) {
        return;
    }
    // The connect worker's own question, asked before it locates the binary.
    let asked = refuse_codex_home(&root);
    let other_root = refuse_codex_home(&dir.join("elsewhere"));
    let result = CodexProvider::connect(CodexLaunch {
        program,
        cwd: root.clone(),
        state_home: dir.clone(),
    });

    assert!(asked.is_err(), "the connect worker's check let it through");
    assert!(other_root.is_ok(), "{other_root:?}");
    match result {
        Err(ProviderError::Provider { code, message }) => {
            assert_eq!(code, ProviderErrorCode::InvalidConfiguration);
            assert!(
                message.contains("CODEX_HOME") && message.contains(&root.display().to_string()),
                "{message}"
            );
        }
        Err(other) => panic!("expected a refusal, got {other}"),
        Ok(_) => panic!("a CODEX_HOME inside the project connected"),
    }
    assert!(!marker.exists(), "the self-check or the session ran a child");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(test)]
mod script_fixture {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/executable.rs"));
}
