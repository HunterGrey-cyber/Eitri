//! Environment-conformance regression test for the real bug found and fixed 2026-09-18 (work,
//! production binary `~/.local/share/claude/approved -> versions/2.1.272`): on that host, `claude`
//! resolved off the INHERITED `PATH` was not the real CLI at all but a multi-account launcher
//! (`claude-wrapper`) that refused any invocation carrying `--settings` --
//!
//!     claude-wrapper: production launcher owns --settings for autoMemoryDirectory
//!
//! -- exit 64, with no `system`/`init` line ever emitted. `--settings` is exactly the flag
//! `agent::process::spawn_with_binary` depends on to install the real `PreToolUse` permission
//! gate in Auto mode (see `agent::settings::hook_settings_arg`), so on that host every Auto-mode
//! session died before opening, and the panel had nothing more specific to show than "the
//! provider process exited unexpectedly".
//!
//! This test is the regression guard for exactly that shape of failure: it never opens a real
//! session, spends no tokens, and touches no network -- it only confirms that whatever `claude`
//! this environment's inherited `PATH` resolves to accepts the product's own gate-bearing
//! `--settings` value when given `--version` as a cheap, immediate terminator (the same probe
//! `agent::process`'s own preflight check performs before every real Auto-mode spawn). It fails in
//! under a second on a host carrying today's bug.
//!
//! **Must skip cleanly, not fail, when this environment has no `claude` on `PATH` at all** -- a CI
//! image with no Claude CLI installed has nothing to conform-check, and this test must not turn
//! that absence into red CI. The skip is printed to stderr so it is visible in test output rather
//! than a silent, unremarked-upon pass.
//!
//! **`#[ignore]`d, deliberately.** This asserts a property of the HOST, not of this code, and it
//! is red on any host carrying the bug -- including this one, until the launcher merge lands.
//! Leaving it in the default target would make `cargo test -p agent` permanently red for a reason
//! living in a different repository, and the first thing a red suite teaches the next reader is to
//! stop reading it. Run it by name:
//!
//!     cargo test -p agent --test environment_conformance -- --ignored --nocapture
//!
//! **The baseline arm is what keeps the accusation honest.** A bare `--version` is run first; only
//! if THAT succeeds does a failing `--settings` arm mean the collision this test is named for. An
//! environment that refuses even a bare `--version` (wrong role, no approved terminal, a launcher
//! guard unrelated to `--settings`) is skipped with its own stderr shown, because asserting one
//! cause with confidence while a different one is true sends the next reader at the wrong file --
//! measured: with the role environment stripped, the earlier version of this test failed with
//! `invalid role environment` while printing that it was the --settings collision.

use std::process::{Command, Stdio};

const NAME: &str = "claude_resolved_from_inherited_path_accepts_the_products_gate_bearing_settings_flag";

#[test]
#[ignore = "asserts a property of the host, not of this code; run by name -- see this file's header"]
fn claude_resolved_from_inherited_path_accepts_the_products_gate_bearing_settings_flag() {
    // The baseline: can this environment run `claude --version` AT ALL, with nothing of ours in
    // the argv? `.is_ok()` was the earlier guard and is true whenever the process merely SPAWNS,
    // whatever it then exits with -- so every unrelated launcher refusal fell through to the
    // assertion below and was reported as the --settings collision.
    let baseline = Command::new("claude")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    match &baseline {
        Err(_) => {
            eprintln!(
                "SKIPPED {NAME}: no `claude` on this environment's inherited PATH -- nothing to \
                 conform-check"
            );
            return;
        }
        Ok(out) if !out.status.success() => {
            eprintln!(
                "SKIPPED {NAME}: this environment refuses a bare `claude --version` (exit {:?}), so a \
                 failing --settings arm would not isolate the --settings collision. Fix the \
                 environment first.\nstderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
            return;
        }
        Ok(_) => {}
    }

    // The exact hook-settings JSON a real Auto-mode `spawn_with_binary` would build, aimed at a
    // socket path that this test never binds and nothing ever connects to -- what is under test
    // here is whether the resolved binary accepts a `--settings` value shaped like this at all,
    // not whether a real hook fires (that is `agent::process`'s own, much larger, test suite).
    let probe_socket = agent::socket_path::in_dir(&std::env::temp_dir(), &format!("nv-env-conf-{}.sock", uuid::Uuid::new_v4().simple()))
        .expect("the probe socket path must fit the macOS length limit, same as every real one");
    let settings_json = agent::settings::hook_settings_arg(&probe_socket)
        .expect("building the hook-settings argument does not itself require a live socket or a real claude");

    let output = Command::new("claude")
        .arg("--settings")
        .arg(&settings_json)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("claude was already confirmed runnable by the PATH check above");

    assert!(
        output.status.success(),
        "the `claude` this environment's inherited PATH resolves to REFUSED the product's own \
         gate-bearing --settings flag (exit {:?}).\nstderr: {}\nstdout: {}\n\n\
         This is exactly the 2026-09-18 launcher-collision bug: something on PATH ahead of the \
         real CLI owns --settings for its own purposes and refuses a caller's use of it. Every \
         Auto-mode session on this host will die before opening until that is resolved -- see \
         agent::process's preflight check (`preflight_gate_flag_is_accepted`), which exists to \
         catch this before a real spawn rather than let a session fail silently, and \
         docs/canonical/dated_record.md's 2026-09-18 'Auto mode could not start...' entry, which \
         carries the note on the production/PATH binary split that poc/tools/work-claude.sh \
         (deleted 2026-09-19) used to carry.",
        output.status,
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );
}
