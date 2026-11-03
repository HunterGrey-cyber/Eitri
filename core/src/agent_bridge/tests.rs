//! The bridge's tests, by topic; each topic is a child file. What more than one of them uses is here.
#![cfg(test)]

use super::*;
use crate::tabs::{Marker, TabId};
use agent::PermissionOutcome;

mod bypass_and_hello;
mod fixtures;
mod message_parsing;
mod restore_and_trust;
mod review;
mod snapshot_basics;
mod snapshot_fields;
mod tab_messages;
mod thinking_and_twins;
mod window_messages;

/// A greeting with the given sessions, for the hello-envelope tests below.
fn greeting_with(resumable: Vec<agent::ResumableSession>) -> crate::agent_backend::BackendGreeting {
    crate::agent_backend::BackendGreeting {
        kind: crate::agent_backend::BackendKind::Sidecar,
        project_dir: std::path::PathBuf::from("/tmp/project"),
        permission_modes: &["bypass"],
        resumable,
        account: None,
    }
}

fn prompt() -> TrustPromptView {
    TrustPromptView {
        tab: TabId(3),
        nonce: 17,
        agent: "Claude".into(),
        root: "/home/u/p/src".into(),
        top: "/home/u/p".into(),
        fingerprint: "f".repeat(64),
        state: TrustPromptState::Untrusted,
        needs_yes: false,
        remember: TrustRememberView::Yes,
        remember_note: None,
        changed: None,
        items: vec![
            TrustItemView {
                what: "hook",
                file: ".claude/settings.json".into(),
                label: "SessionStart".into(),
                value: "touch /tmp/m".into(),
                outside: false,
            },
            TrustItemView {
                what: "mcp_server",
                file: ".mcp.json".into(),
                label: "marker".into(),
                value: "sh -c 'touch /tmp/n; exec sleep 60'".into(),
                outside: false,
            },
            TrustItemView {
                what: "symlink",
                file: ".claude/hooks/run".into(),
                label: ".claude/hooks/run".into(),
                value: "/opt/x/run".into(),
                outside: true,
            },
        ],
    }
}
