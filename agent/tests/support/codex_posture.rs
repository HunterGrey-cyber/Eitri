//! What every test that runs a real `codex app-server` on a real account starts under.
//!
//! [`config`] is the session config Eitri sends for a project, with each of the user's own tool servers
//! (named by the caller, from the account's `config.toml`) turned off: a test on a real account must not
//! start servers the account's user configured for other purposes. A server's name is one key of a nested
//! object, so a name that holds `.` still names exactly that server.
//!
//! Included with `#[path = "support/codex_posture.rs"] mod codex_posture;`.

#![allow(dead_code)]

use agent::external::{DirTrust, TrustPlan};
use agent::providers::codex::config::{session_config, SessionConfig};
use std::path::Path;

/// The trust plan of a test project: the directory alone, at the given level.
pub fn plan(project: &Path, trust: DirTrust) -> TrustPlan {
    TrustPlan {
        dirs: vec![(project.to_path_buf(), trust)],
        skills_off: trust == DirTrust::Untrusted,
        fingerprint: "real-login-suite".to_string(),
        project_only: Vec::new(),
    }
}

/// The session config for a project, with the account's own tool servers `user_mcp_names` disabled.
pub fn config(project: &Path, trust: DirTrust, user_mcp_names: &[&str]) -> SessionConfig {
    session_config(&plan(project, trust), None)
        .expect("a config for the test project")
        .with_tool_servers_off(user_mcp_names)
}
