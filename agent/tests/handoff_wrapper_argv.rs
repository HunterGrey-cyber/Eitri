//! The guard two comments in this crate used to claim existed and did not: what
//! `neovibe-claude-handoff` really `exec`s.
//!
//! Before this file, `handoff.rs`'s unit test asserted `claude_resume_argv(x) == claude_resume_argv(x)`
//! via `ClaudeResumeCommand::for_session`, plus a hardcoded string — a tautology and a literal,
//! neither of which mentions the wrapper binary. Reverting the wrapper to its own hardcoded
//! `Command::new("claude").arg("--resume")` left every test in the workspace green.
//!
//! This runs the real binary instead. `PATH` points at a temp directory holding a stub `claude`
//! that prints the argv it was given; the wrapper `exec`s it, so what is captured is genuinely the
//! argv the wrapper built, not a reconstruction of it.
//!
//! **No real Claude turn is spent and no network is touched** — the stub is a two-line shell
//! script, and `--resume` never reaches a real CLI.
//!
//! What it pins, exactly: the argv itself, against both a literal and `claude_resume_argv`'s own
//! output. A flag added to `claude_resume_argv` alone fails the literal assert; a flag added to the
//! wrapper alone fails both. What it cannot see is the wrapper being re-hardcoded to an argv that
//! happens to equal today's — this is a guard on the command, not on which function built it, and
//! the wrapper's own comment says so rather than claiming otherwise.

use std::io::Write;
use std::process::{Command, Stdio};

/// Writes an executable stub `claude` into `dir` that prints `$0` and each argument on its own
/// line, and returns the directory to put on `PATH`.
///
/// `$0` comes back as the resolved path the kernel `exec`'d, not the bare word the wrapper asked
/// for, so the caller compares its file name rather than the whole string — that is a property of
/// `PATH` resolution and of `#!` scripts, not of the wrapper.
fn stub_claude_in(dir: &std::path::Path) {
    let script = dir.join("claude");
    {
        let mut f = std::fs::File::create(&script).expect("create stub claude");
        // `$0` is how the wrapper named the program; the rest is its argv tail.
        writeln!(f, "#!/bin/sh").unwrap();
        writeln!(f, "printf '%s\\n' \"$0\" \"$@\"").unwrap();
    }
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&script, perms).unwrap();
}

/// A private temp directory for one run. Unique per call, not per process: cargo runs the tests in
/// this file on parallel threads, and a shared pid-keyed path had one test deleting the other's
/// stub out from under it.
fn temp_dir(tag: &str) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("neovibe-handoff-argv-{}-{}-{}", tag, std::process::id(), n));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Runs the wrapper with a stub `claude` on `PATH` and returns its stdout lines.
///
/// `NEOVIBE_LEASE_FD=1` is deliberate and is not a shortcut around the lease: the wrapper's own
/// check on that fd is `fcntl(fd, F_GETFD)`, i.e. "is this descriptor open", and fd 1 always is.
/// That weakness is a real, separately-recorded property of the wrapper (a REUSED fd number passes
/// it too) — it is not what this test is about, which is the argv the wrapper goes on to build.
fn run_wrapper(session_id: &str) -> (bool, Vec<String>, String) {
    let dir = temp_dir("stub");
    stub_claude_in(&dir);

    let output = Command::new(env!("CARGO_BIN_EXE_neovibe-claude-handoff"))
        .env("NEOVIBE_LEASE_FD", "1")
        .env("NEOVIBE_RESUME_SESSION_ID", session_id)
        .env("PATH", &dir)
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run neovibe-claude-handoff");

    let _ = std::fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (output.status.success(), stdout.lines().map(str::to_string).collect(), stderr)
}

#[test]
fn the_wrapper_execs_exactly_the_plain_interactive_resume_invocation() {
    let id = "1857dcd5-973b-46a2-8d1e-0f0c9b2a4e11";
    let (ok, lines, stderr) = run_wrapper(id);
    assert!(ok, "wrapper failed; stderr: {stderr}");

    // The program: `PATH` resolution means `$0` is the stub's full path, so the file name is what
    // carries the claim — the wrapper ran `claude`, not some other binary.
    assert_eq!(
        std::path::Path::new(&lines[0]).file_name().and_then(|n| n.to_str()),
        Some("claude"),
        "stderr: {stderr}"
    );

    // The literal, independent of anything in this crate: this is the whole invocation, and any
    // extra flag on either side shows up here as an extra line.
    assert_eq!(lines[1..], ["--resume".to_string(), id.to_string()], "stderr: {stderr}");

    // And the same argv the "continue in a terminal" card shows a human, so the two genuinely are
    // one command rather than two that agree today.
    assert_eq!(lines[1..], agent::handoff::claude_resume_argv(id).unwrap()[1..]);
    assert_eq!(
        lines[1..],
        agent::handoff::ClaudeResumeCommand::for_session("/tmp", id).unwrap().argv()[1..]
    );
}

/// The wrapper refuses an id that `claude --resume` would read as an option, rather than `exec`ing
/// it — the same refusal `claude_resume_argv` makes for the displayed line. If this ever stopped
/// holding, the stub would be invoked with a flag in the session-id position.
#[test]
fn an_id_that_would_read_as_a_flag_is_never_execd() {
    let (ok, lines, stderr) = run_wrapper("--dangerously-skip-permissions");
    assert!(!ok, "the wrapper exec'd something it should have refused: {lines:?}");
    assert!(lines.is_empty(), "nothing should have been exec'd, got {lines:?}");
    assert!(
        stderr.contains("begins with '-'"),
        "the refusal should name the real reason, got: {stderr}"
    );
}
