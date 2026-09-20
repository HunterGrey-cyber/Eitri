//! Builds the `PreToolUse` hook configuration that points the CLI at the `agent-hook` companion
//! binary, as a value passed on the command line -- `claude --settings '<json>'` -- and NOT as a
//! file written into the project.
//!
//! **Why this is a command-line argument and not a file.** Until 2026-09-15 this module generated
//! `<project_dir>/.claude/settings.local.json`, captured any pre-existing content, and restored it
//! on shutdown. That path is keyed on the project directory alone, so every `claude` invocation in
//! that directory -- including a real, human-facing terminal session that has nothing to do with
//! this crate -- read the same hook, and two conversations in one directory shared one file.
//!
//! A real-CLI spike on 2026-09-15 (work account, CLI 2.1.272, one long-lived
//! `--print --input-format stream-json` process, three turns each forced to call a tool) settled
//! what the old code only assumed, and settled it the dangerous way:
//!
//! | turn | file on disk | tool call | `PreToolUse` fired |
//! |---|---|---|---|
//! | 1 | hook tagged `VARIANT-A` | yes | `VARIANT-A` |
//! | 2 | **deleted mid-conversation** | yes, **executed** | **none at all** |
//! | 3 | restored, tagged `VARIANT-C` | yes | `VARIANT-C` |
//!
//! The CLI re-reads that file per tool invocation, live. So one conversation's orderly shutdown
//! deleting the file did not merely leave a mess for the next run -- it silently removed the
//! permission gate from a *still-running* second conversation in the same directory, whose tools
//! then executed with no hook and whose UI never showed a permission card.
//!
//! The same spike confirmed the replacement: with the hook passed as `--settings '<json>'`, a
//! foreign `.claude/settings.local.json` appearing and being deleted mid-conversation left this
//! process's own hook firing untouched, and the CLI created no `.claude/` directory of its own.
//! The configuration now lives in one process's argv, where nothing outside that process can reach
//! it.

use serde_json::json;
use std::path::{Path, PathBuf};

/// Generous, explicit hook timeout (seconds) -- the commonly-assumed "60 second hard timeout" is
/// only the hook matcher's *default*, not a ceiling (verdandi's own corroborated research); a
/// real human approval must be able to take minutes without the CLI killing the hook early.
const HOOK_TIMEOUT_SECONDS: u64 = 600;

/// The value for `claude --settings`: a `PreToolUse` hook on every tool (`matcher: "*"`), running
/// the `agent-hook` binary with this conversation's own socket path in its environment.
///
/// Touches no filesystem. The returned string is meant to be handed straight to `Command::arg`,
/// so the configuration exists only in the spawned process's argv and dies with it.
pub fn hook_settings_arg(socket_path: &Path) -> std::io::Result<String> {
    let agent_hook_path = locate_agent_hook_binary()?;
    Ok(hook_settings_arg_with_hook_path(socket_path, &agent_hook_path))
}

/// The real body of [`hook_settings_arg`], parameterized only by where `agent-hook` lives, so this
/// module's tests can build the genuine configuration without a built binary to point at.
fn hook_settings_arg_with_hook_path(socket_path: &Path, agent_hook_path: &Path) -> String {
    let command = format!(
        "NEOVIBE_AGENT_HOOK_SOCKET={} {}",
        socket_path.display(),
        agent_hook_path.display()
    );

    json!({
        "hooks": {
            "PreToolUse": [
                {
                    "matcher": "*",
                    "hooks": [
                        { "type": "command", "command": command, "timeout": HOOK_TIMEOUT_SECONDS }
                    ]
                }
            ]
        }
    })
    .to_string()
}

/// Locates the `agent-hook` binary as a sibling of the currently-running executable -- Cargo
/// places every real binary target from one workspace build into the same `target/<profile>/`
/// directory, so this holds for `shell` (the real runtime caller) and any other bin target.
///
/// It does NOT directly hold for a `cargo test`/`cargo bench` harness binary or a `cargo run
/// --example` binary, though: those land one directory deeper, in `target/<profile>/deps/` or
/// `target/<profile>/examples/` respectively. The `deps/` case was confirmed for real (Task 5)
/// when this crate's own real `#[ignore]`d `agent::process` tests called the production
/// hook-settings path from inside `cargo test --lib` and hit exactly this mismatch. The
/// `examples/` case was confirmed for real (Task 7) when `cargo run --example demo` hit the
/// identical mismatch trying to run the crate's own shipped example. Rather than push every such
/// caller onto a test-only bypass (as this module's own unit tests already do, via
/// `hook_settings_arg_with_hook_path`, since those don't need a real, working `agent-hook` process
/// to actually be invoked by the CLI), this also checks one directory up from a `deps/` or
/// `examples/` dir, so a real end-to-end test or the shipped example gets the crate's genuine
/// production code path under real exercise.
fn locate_agent_hook_binary() -> std::io::Result<PathBuf> {
    let current = std::env::current_exe()?;
    let dir = current
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "current_exe has no parent directory"))?;
    let candidate = dir.join("agent-hook");
    if candidate.exists() {
        return Ok(candidate);
    }
    let one_dir_deeper = matches!(
        dir.file_name().and_then(|n| n.to_str()),
        Some("deps") | Some("examples")
    );
    if one_dir_deeper {
        if let Some(parent) = dir.parent() {
            let fallback = parent.join("agent-hook");
            if fallback.exists() {
                return Ok(fallback);
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("agent-hook binary not found at {candidate:?} -- was it built in the same cargo build?"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAKE_HOOK: &str = "/usr/bin/fake-agent-hook";

    #[test]
    fn the_argument_is_a_pretooluse_hook_on_every_tool_naming_this_conversations_socket() {
        let socket_path = Path::new("/tmp/neovibe-agent-hook-abc123.sock");
        let arg = hook_settings_arg_with_hook_path(socket_path, FAKE_HOOK.as_ref());

        let parsed: serde_json::Value = serde_json::from_str(&arg).unwrap();
        let matcher = parsed["hooks"]["PreToolUse"][0]["matcher"].as_str().unwrap();
        assert_eq!(matcher, "*", "the gate must cover every tool, not a subset");

        let hook_entry = &parsed["hooks"]["PreToolUse"][0]["hooks"][0];
        let command = hook_entry["command"].as_str().unwrap();
        assert!(command.contains("NEOVIBE_AGENT_HOOK_SOCKET"));
        assert!(command.contains("/tmp/neovibe-agent-hook-abc123.sock"));
        assert!(command.contains("fake-agent-hook"));
        assert!(hook_entry["timeout"].as_u64().unwrap() >= 300);
    }

    /// The whole point of the change this module records. A configuration that exists only in one
    /// process's argv cannot be read, overwritten, or deleted by a second conversation in the same
    /// directory -- so this asserts the absence of any filesystem effect, not just the presence of
    /// the right JSON.
    #[test]
    fn building_the_argument_writes_nothing_to_disk() {
        let dir = tempfile_dir();
        let before = dir_snapshot(&dir);

        let _ = hook_settings_arg_with_hook_path(&dir.join("s.sock"), FAKE_HOOK.as_ref());

        assert!(!dir.join(".claude").exists(), "no .claude/ directory may be created");
        assert_eq!(before, dir_snapshot(&dir), "the project directory must be untouched");
    }

    /// Two conversations in ONE directory get two independent gates. Under the old file-based
    /// mechanism this was impossible by construction: both wrote `<project_dir>/.claude/
    /// settings.local.json`, so the second overwrote the first and the first's shutdown deleted
    /// the second's.
    #[test]
    fn two_conversations_in_one_directory_produce_two_independent_configurations() {
        let a = hook_settings_arg_with_hook_path(Path::new("/tmp/a.sock"), FAKE_HOOK.as_ref());
        let b = hook_settings_arg_with_hook_path(Path::new("/tmp/b.sock"), FAKE_HOOK.as_ref());

        assert_ne!(a, b);
        assert!(a.contains("/tmp/a.sock") && !a.contains("/tmp/b.sock"));
        assert!(b.contains("/tmp/b.sock") && !b.contains("/tmp/a.sock"));
    }

    fn dir_snapshot(dir: &Path) -> Vec<String> {
        let mut entries: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        entries
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("neovibe-settings-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
