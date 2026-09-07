//! Generates and tears down the per-conversation `.claude/settings.local.json` that points a
//! `PreToolUse` hook at the `agent-hook` companion binary (Task 3). Uses `settings.local.json`,
//! not `settings.json`, specifically so a project's own real, possibly-already-present
//! `.claude/settings.json` is never read or overwritten -- see
//! docs/superpowers/specs/2026-09-07-agent-v2-streaming-protocol-design.md.
//!
//! `settings.local.json` itself is Claude Code's own real per-project, machine-local settings file
//! (typically gitignored, and a place a user's own permission allowlists genuinely live), so a
//! pre-existing one is never destroyed either: `generate` captures its exact content before
//! overwriting it and `cleanup` writes that content back verbatim, deleting the file only when
//! there was nothing there to begin with. See `HookSettings::original_content`.

use serde_json::json;
use std::path::{Path, PathBuf};

/// Generous, explicit hook timeout (seconds) -- the commonly-assumed "60 second hard timeout" is
/// only the hook matcher's *default*, not a ceiling (verdandi's own corroborated research); a
/// real human approval must be able to take minutes without the CLI killing the hook early.
const HOOK_TIMEOUT_SECONDS: u64 = 600;

pub struct HookSettings {
    settings_path: PathBuf,
    /// The verbatim content of a `.claude/settings.local.json` that already existed when
    /// `generate` ran, captured before it was overwritten. `cleanup()` writes this back instead of
    /// deleting the file, so a project's own real (typically gitignored) machine-local settings --
    /// permission allowlists and the like -- survive a conversation byte-for-byte. `None` in the
    /// common case where no such file existed, where `cleanup()` deletes the generated file.
    ///
    /// Held in memory only, so restoration depends on `cleanup()` actually running: a hard crash
    /// of the host process (SIGKILL, a panic that skips `Drop`) still leaves the generated config
    /// in place with the original unrecoverable -- the same class of on-disk leftover as the
    /// per-conversation socket file, which `spawn`/`shutdown` likewise only clean up on an orderly
    /// path. Persisting a backup copy to disk would close that window and is not done here.
    ///
    /// Known, accepted limitation (a functionality trade-off, not data loss): if the pre-existing
    /// file declared its own hooks (e.g. its own `PreToolUse` entry), those are temporarily
    /// inactive for the lifetime of this conversation, since this crate's generated config
    /// replaces the whole file rather than merging into it. They come back intact on `cleanup()`.
    /// Merging two hook configurations is deliberately out of scope.
    original_content: Option<String>,
    /// True when `generate` itself created the `.claude/` directory (it did not exist before), so
    /// `cleanup()` knows it may remove it again -- and only ever when it's still empty, so a
    /// directory anything else has since put files into is never touched.
    created_claude_dir: bool,
}

impl HookSettings {
    pub fn generate(project_dir: &Path, socket_path: &Path) -> std::io::Result<Self> {
        let agent_hook_path = locate_agent_hook_binary()?;
        Self::generate_with_hook_path(project_dir, socket_path, &agent_hook_path)
    }

    fn generate_with_hook_path(
        project_dir: &Path,
        socket_path: &Path,
        agent_hook_path: &Path,
    ) -> std::io::Result<Self> {
        let claude_dir = project_dir.join(".claude");
        let created_claude_dir = !claude_dir.exists();
        std::fs::create_dir_all(&claude_dir)?;

        let command = format!(
            "NEOVIBE_AGENT_HOOK_SOCKET={} {}",
            socket_path.display(),
            agent_hook_path.display()
        );

        let config = json!({
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
        });

        let settings_path = claude_dir.join("settings.local.json");
        // Capture (never destroy) a pre-existing file first: this is Claude Code's own real
        // per-project machine-local settings file, and a user's own permission allowlists can
        // live in it. `cleanup()` restores whatever is captured here.
        //
        // Guard against re-persisting our OWN stale leftover instead of a real prior user file:
        // if a previous conversation in this same `project_dir` was never cleanly `shutdown()`
        // (a SIGKILL, or a second concurrent session racing this one), what's on disk right now
        // may already be a generated hook config pointing at a dead socket, not genuine user
        // content. Restoring that on `cleanup()` would leave the leftover in place forever
        // instead of ever being deleted -- every future `claude` invocation in that directory
        // would then invoke a hook whose socket nothing is listening on, fail to connect, and
        // exit with no decision, which reads as "allow" under `auto` mode (see `agent-hook.rs`'s
        // own connect-failure exit path). Treat any existing content containing our own marker
        // env var as "nothing genuinely pre-existing" rather than as a real original to restore.
        let original_content = match std::fs::read_to_string(&settings_path) {
            Ok(content) if content.contains("NEOVIBE_AGENT_HOOK_SOCKET") => None,
            Ok(content) => Some(content),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        std::fs::write(&settings_path, serde_json::to_string_pretty(&config)?)?;

        Ok(Self { settings_path, original_content, created_claude_dir })
    }

    /// Undoes `generate`: restores a captured pre-existing `settings.local.json` byte-for-byte if
    /// there was one, otherwise deletes the file this crate generated (and, only if `generate`
    /// itself created `.claude/` and nothing else has put anything in it since, removes that
    /// now-empty directory too).
    pub fn cleanup(&self) -> std::io::Result<()> {
        match &self.original_content {
            Some(content) => {
                std::fs::write(&self.settings_path, content)?;
            }
            None => {
                if self.settings_path.exists() {
                    std::fs::remove_file(&self.settings_path)?;
                }
                if self.created_claude_dir {
                    if let Some(claude_dir) = self.settings_path.parent() {
                        // `remove_dir` only succeeds on an empty directory -- exactly the
                        // condition wanted here, so a non-empty `.claude/` fails harmlessly.
                        let _ = std::fs::remove_dir(claude_dir);
                    }
                }
            }
        }
        Ok(())
    }
}

/// Locates the `agent-hook` binary as a sibling of the currently-running executable -- Cargo
/// places every real binary target from one workspace build into the same `target/<profile>/`
/// directory, so this holds for `shell` (the real runtime caller) and any other bin target.
///
/// It does NOT directly hold for a `cargo test`/`cargo bench` harness binary or a `cargo run
/// --example` binary, though: those land one directory deeper, in `target/<profile>/deps/` or
/// `target/<profile>/examples/` respectively. The `deps/` case was confirmed for real (Task 5)
/// when this crate's own real `#[ignore]`d `agent::process` tests called the production
/// `HookSettings::generate` path from inside `cargo test --lib` and hit exactly this mismatch.
/// The `examples/` case was confirmed for real (Task 7) when `cargo run --example demo` hit the
/// identical mismatch trying to run the crate's own shipped example. Rather than push every such
/// caller onto a test-only bypass (as Task 4's own unit tests already had to, via
/// `generate_with_hook_path`, since those don't need a real, working `agent-hook` process to
/// actually be invoked by the CLI), this also checks one directory up from a `deps/` or
/// `examples/` dir, so a real end-to-end test or the shipped example gets the crate's genuine
/// production code path under real exercise.
fn locate_agent_hook_binary() -> std::io::Result<PathBuf> {
    let current = std::env::current_exe()?;
    let dir = current.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "current_exe has no parent directory")
    })?;
    let candidate = dir.join("agent-hook");
    if candidate.exists() {
        return Ok(candidate);
    }
    let one_dir_deeper = matches!(dir.file_name().and_then(|n| n.to_str()), Some("deps") | Some("examples"));
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

    #[test]
    fn generate_writes_a_pretooluse_hook_pointing_at_agent_hook_with_the_socket_path() {
        let dir = tempfile_dir();
        let socket_path = dir.join("test.sock");
        // Use a fake hook binary path since test binaries are in target/debug/deps/ while
        // agent-hook is in target/debug/, so locate_agent_hook_binary() would fail.
        // The real runtime (shell) is always built in the same target/<profile>/ directory as agent-hook.
        let fake_hook_path = "/usr/bin/fake-agent-hook";
        let settings = HookSettings::generate_with_hook_path(&dir, &socket_path, fake_hook_path.as_ref()).unwrap();

        let written = std::fs::read_to_string(dir.join(".claude/settings.local.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&written).unwrap();
        let hook_entry = &parsed["hooks"]["PreToolUse"][0]["hooks"][0];
        let command = hook_entry["command"].as_str().unwrap();
        assert!(command.contains("NEOVIBE_AGENT_HOOK_SOCKET"));
        assert!(command.contains(socket_path.to_str().unwrap()));
        assert!(command.contains("agent-hook"));
        assert!(hook_entry["timeout"].as_u64().unwrap() >= 300);

        settings.cleanup().unwrap();
        assert!(!dir.join(".claude/settings.local.json").exists());
    }

    #[test]
    fn generate_never_touches_an_existing_settings_json() {
        let dir = tempfile_dir();
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(".claude/settings.json"), r#"{"existing": true}"#).unwrap();

        let socket_path = dir.join("test.sock");
        // Use a fake hook binary path (see comment in the first test)
        let fake_hook_path = "/usr/bin/fake-agent-hook";
        let settings = HookSettings::generate_with_hook_path(&dir, &socket_path, fake_hook_path.as_ref()).unwrap();
        settings.cleanup().unwrap();

        let still_there = std::fs::read_to_string(dir.join(".claude/settings.json")).unwrap();
        assert_eq!(still_there, r#"{"existing": true}"#);
    }

    #[test]
    fn generate_treats_a_stale_generated_config_as_nothing_to_restore() {
        // Simulates what's left behind after a prior conversation in this same `project_dir`
        // was never cleanly shut down (a SIGKILL, or a second concurrent session): a real
        // generated hook config, pointing at a now-dead socket, is already sitting on disk.
        // `generate` must NOT capture that as "the original" -- doing so would make `cleanup()`
        // restore it forever instead of ever deleting it, leaving every future `claude`
        // invocation in that directory pointed at a dead socket.
        let dir = tempfile_dir();
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        let stale_leftover = r#"{"hooks":{"PreToolUse":[{"matcher":"*","hooks":[{"type":"command","command":"NEOVIBE_AGENT_HOOK_SOCKET=/tmp/dead.sock /path/to/agent-hook","timeout":600}]}]}}"#;
        std::fs::write(dir.join(".claude/settings.local.json"), stale_leftover).unwrap();

        let socket_path = dir.join("test.sock");
        let fake_hook_path = "/usr/bin/fake-agent-hook";
        let settings = HookSettings::generate_with_hook_path(&dir, &socket_path, fake_hook_path.as_ref()).unwrap();
        settings.cleanup().unwrap();

        assert!(
            !dir.join(".claude/settings.local.json").exists(),
            "a stale generated config must be deleted on cleanup, not restored"
        );
    }

    /// The regression test for the real data-loss bug a final review found: `generate` used to
    /// unconditionally overwrite `.claude/settings.local.json` and `cleanup` to unconditionally
    /// delete it -- silently destroying a project's own real, typically-gitignored Claude Code
    /// settings (a user's own permission allowlists live there) with no way to get them back.
    /// Proves the file survives byte-for-byte, and -- crucially -- that this is a genuine
    /// overwrite-then-restore, not an accidental no-op: the file is opened mid-way through, after
    /// `generate()` and before `cleanup()`, and confirmed to hold THIS crate's hook config at that
    /// point, i.e. the original really was displaced and really was put back.
    #[test]
    fn a_pre_existing_settings_local_json_is_restored_byte_for_byte_after_generate_then_cleanup() {
        let dir = tempfile_dir();
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        let original = "{\n  \"permissions\": {\n    \"allow\": [\"Bash(git status:*)\"]\n  }\n}\n";
        std::fs::write(dir.join(".claude/settings.local.json"), original).unwrap();

        let socket_path = dir.join("test.sock");
        let settings =
            HookSettings::generate_with_hook_path(&dir, &socket_path, "/usr/bin/fake-agent-hook".as_ref()).unwrap();

        // Mid-way: the generated hook config must genuinely be in place right now (otherwise this
        // test would "pass" against a broken implementation that simply never wrote anything).
        let during = std::fs::read_to_string(dir.join(".claude/settings.local.json")).unwrap();
        assert!(during.contains("PreToolUse"), "generate() must actually install its hook config: {during}");
        assert!(during.contains("NEOVIBE_AGENT_HOOK_SOCKET"));
        assert!(!during.contains("git status"), "the original content must be displaced while the hook is active");

        settings.cleanup().unwrap();

        let restored = std::fs::read_to_string(dir.join(".claude/settings.local.json")).unwrap();
        assert_eq!(restored, original, "a pre-existing settings.local.json must survive byte-for-byte");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The complementary (common) case: with no pre-existing file, `cleanup` still deletes the
    /// generated one -- and also removes the `.claude/` directory it created, since it's empty
    /// again. Guards against the restore path above accidentally leaving generated files behind.
    #[test]
    fn with_no_pre_existing_file_cleanup_deletes_the_generated_one_and_the_dir_it_created() {
        let dir = tempfile_dir();
        assert!(!dir.join(".claude").exists());

        let socket_path = dir.join("test.sock");
        let settings =
            HookSettings::generate_with_hook_path(&dir, &socket_path, "/usr/bin/fake-agent-hook".as_ref()).unwrap();
        assert!(dir.join(".claude/settings.local.json").exists());

        settings.cleanup().unwrap();
        assert!(!dir.join(".claude/settings.local.json").exists());
        assert!(!dir.join(".claude").exists(), "an otherwise-empty .claude/ that generate() created must not be left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `.claude/` removal above must never touch a directory holding anything else -- the far
    /// more common real case, where a project's own `.claude/settings.json` lives alongside.
    #[test]
    fn cleanup_leaves_a_claude_dir_that_still_holds_other_files_alone() {
        let dir = tempfile_dir();
        let socket_path = dir.join("test.sock");
        let settings =
            HookSettings::generate_with_hook_path(&dir, &socket_path, "/usr/bin/fake-agent-hook".as_ref()).unwrap();
        // Something else appears in .claude/ during the conversation (a real project's own
        // settings.json, an agent-written file, anything).
        std::fs::write(dir.join(".claude/settings.json"), r#"{"existing": true}"#).unwrap();

        settings.cleanup().unwrap();
        assert!(dir.join(".claude").exists(), "a non-empty .claude/ must never be removed");
        assert_eq!(std::fs::read_to_string(dir.join(".claude/settings.json")).unwrap(), r#"{"existing": true}"#);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-settings-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
