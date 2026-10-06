//! Shared protocol types and small helpers for the cross-window agent-status dashboard
//! (`eitri-supervisor`, `src/bin/eitri_supervisor.rs`) and `shell`'s own client of it
//! (`panel/src/supervisor_client.rs`). See
//! `docs/superpowers/specs/2026-09-08-supervisor-cross-window-agent-status-design.md`.

use std::path::PathBuf;

/// One of `shell`'s five derivable agent states (spec §4) -- computed in `shell` from
/// `agent::AgentSessionProjection` fields that already exist; this crate never depends on `agent` at
/// all, it only carries the resulting enum value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    NoSession,
    Idle,
    Working,
    Blocked,
    Done,
}

/// A message `shell` sends to `eitri-supervisor` over their shared connection (spec §3).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ShellMessage {
    Register {
        instance_id: String,
        project_name: String,
        project_dir: String,
        pid: u32,
    },
    Status {
        instance_id: String,
        status: AgentStatus,
    },
}

/// A message `eitri-supervisor` sends back to `shell` over that same connection (spec §3).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SupervisorMessage {
    Activate,
}

/// The one well-known socket every `shell` instance and `eitri-supervisor` agree on (spec §3)
/// -- unlike `agent-hook`'s per-conversation UUID sockets, there is exactly one of these on the
/// whole machine at a time, since every `shell` window must find the *same* supervisor.
pub fn socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join("eitri-supervisor.sock");
    }
    // Fallback: `std::env::temp_dir()` is world-writable (unlike `$XDG_RUNTIME_DIR`'s 0700
    // permissions), so on a multi-user machine another local user could squat this path before
    // this process does, or connect to it once bound. Not worth hardening on Linux — this
    // fallback is essentially unreachable there in practice (`$XDG_RUNTIME_DIR` is always set
    // under systemd, which this project already assumes elsewhere) — but worth a comment so a
    // future reader doesn't assume the fallback carries the same isolation guarantee as the
    // primary path. **On macOS this is not the unreachable branch, it is the only one taken**:
    // `XDG_RUNTIME_DIR` is never set there (it is a systemd/Linux convention with no macOS
    // equivalent), so `eitri-supervisor` always resolves through here on the Mac -- worth
    // knowing before assuming the lib's behaviour is Linux-tested and macOS-theoretical.
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    std::env::temp_dir().join(format!("eitri-supervisor-{user}.sock"))
}

/// Finds the `eitri-supervisor` binary next to whichever binary is currently running (mirrors
/// `agent::settings`'s own `locate_agent_hook_binary` exactly -- same "same cargo build, sibling
/// binary" assumption, same `deps`/`examples`-directory-one-level-up fallback for `cargo test`/
/// `cargo run --example` builds).
pub fn locate_supervisor_binary() -> std::io::Result<PathBuf> {
    let current = std::env::current_exe()?;
    let dir = current
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "current_exe has no parent directory"))?;
    let candidate = dir.join("eitri-supervisor");
    if candidate.exists() {
        return Ok(candidate);
    }
    let one_dir_deeper = matches!(
        dir.file_name().and_then(|n| n.to_str()),
        Some("deps") | Some("examples")
    );
    if one_dir_deeper {
        if let Some(parent) = dir.parent() {
            let fallback = parent.join("eitri-supervisor");
            if fallback.exists() {
                return Ok(fallback);
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("eitri-supervisor binary not found at {candidate:?} -- was it built in the same cargo build?"),
    ))
}

pub mod registry;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_message_round_trips_through_json() {
        let msg = ShellMessage::Register {
            instance_id: "abc-123".into(),
            project_name: "eitri".into(),
            project_dir: "/home/user/src/eitri".into(),
            pid: 4242,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"type":"register","instance_id":"abc-123","project_name":"eitri","project_dir":"/home/user/src/eitri","pid":4242}"#
        );
        let parsed: ShellMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn status_message_round_trips_with_snake_case_status_value() {
        let msg = ShellMessage::Status {
            instance_id: "abc-123".into(),
            status: AgentStatus::Blocked,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"status","instance_id":"abc-123","status":"blocked"}"#);
        let parsed: ShellMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn every_agent_status_variant_serializes_to_the_expected_snake_case_string() {
        let cases = [
            (AgentStatus::NoSession, "\"no_session\""),
            (AgentStatus::Idle, "\"idle\""),
            (AgentStatus::Working, "\"working\""),
            (AgentStatus::Blocked, "\"blocked\""),
            (AgentStatus::Done, "\"done\""),
        ];
        for (status, expected) in cases {
            assert_eq!(serde_json::to_string(&status).unwrap(), expected);
        }
    }

    #[test]
    fn activate_message_round_trips_through_json() {
        let json = serde_json::to_string(&SupervisorMessage::Activate).unwrap();
        assert_eq!(json, r#"{"type":"activate"}"#);
        let parsed: SupervisorMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, SupervisorMessage::Activate);
    }

    #[test]
    fn socket_path_respects_xdg_runtime_dir_and_falls_back_to_temp_dir_with_user_suffix() {
        // SAFETY: this test mutates process-wide env state (XDG_RUNTIME_DIR, USER). Both scenarios
        // are exercised sequentially within this one test function specifically so no other test
        // in this crate can interleave with these mutations on a separate thread -- cargo's default
        // multi-threaded test runner made that a real risk when this was two separate tests.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000") };
        assert_eq!(
            socket_path(),
            std::path::PathBuf::from("/run/user/1000/eitri-supervisor.sock")
        );

        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
            std::env::set_var("USER", "testuser");
        }
        let path = socket_path();
        assert_eq!(path, std::env::temp_dir().join("eitri-supervisor-testuser.sock"));

        unsafe { std::env::remove_var("USER") };
    }
}
