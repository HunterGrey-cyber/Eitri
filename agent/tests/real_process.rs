use agent::{AgentEvent, AgentProcess, SpawnMode, CONSERVATIVE_DISALLOWED_TOOLS};
use uuid::Uuid;

/// Real end-to-end: spawns a genuine `claude -p` process, confirms a real session starts,
/// produces real text, and finishes cleanly. Costs real API usage (this plan's own research
/// measured roughly $0.10-0.15 per invocation) -- run explicitly via
/// `cargo test -p agent -- --ignored`, never as part of a routine `cargo test`.
#[test]
#[ignore]
fn real_spawn_produces_a_session_started_and_a_turn_finished() {
    let session_id = Uuid::new_v4();
    let mut process = AgentProcess::spawn(
        "reply with exactly the word: pong",
        SpawnMode::New { session_id },
        CONSERVATIVE_DISALLOWED_TOOLS,
    )
    .expect("claude must be on PATH for this test");

    let mut saw_session_started = false;
    let mut saw_turn_finished = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while std::time::Instant::now() < deadline && !saw_turn_finished {
        for event in process.poll_events() {
            match event {
                AgentEvent::SessionStarted { .. } => saw_session_started = true,
                AgentEvent::TurnFinished { ref result_text, .. } => {
                    saw_turn_finished = true;
                    assert!(result_text.to_lowercase().contains("pong"), "got: {result_text}");
                }
                _ => {}
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    assert!(saw_session_started, "never saw SessionStarted within 60s");
    assert!(saw_turn_finished, "never saw TurnFinished within 60s");

    process.shutdown();
    assert!(process.has_exited());
}

/// Real end-to-end: confirms `SpawnMode::Resume` genuinely continues the same session, not just
/// that the CLI accepts a `--resume` flag without erroring. Spawns a first turn that produces a
/// distinctive word, shuts it down, then spawns a second turn on the same `session_id` asking
/// the CLI to recall what it just said -- only passes if the second response actually references
/// the first turn's content, which requires real context continuity, not merely a non-error exit
/// code. Costs real API usage (two invocations) -- see this crate's other `#[ignore]`d test for
/// the same cost/opt-in discipline.
#[test]
#[ignore]
fn real_resume_continues_the_same_session_with_context() {
    let session_id = Uuid::new_v4();
    let mut first = AgentProcess::spawn(
        "reply with exactly the word: pong",
        SpawnMode::New { session_id },
        CONSERVATIVE_DISALLOWED_TOOLS,
    )
    .unwrap();
    drain_until_finished(&mut first, std::time::Duration::from_secs(60));
    first.shutdown();

    let mut second = AgentProcess::spawn(
        "what was the exact word you just replied with?",
        SpawnMode::Resume { session_id },
        CONSERVATIVE_DISALLOWED_TOOLS,
    )
    .unwrap();
    let result_text = drain_until_finished(&mut second, std::time::Duration::from_secs(60));
    assert!(
        result_text.to_lowercase().contains("pong"),
        "resumed session should recall prior turn, got: {result_text}"
    );
    second.shutdown();
}

/// Shared helper: polls until a `TurnFinished` event arrives (or the deadline passes), returning
/// its `result_text`. Panics on timeout -- these are already-`#[ignore]`d real tests, a timeout
/// here means a real regression worth seeing fail loudly.
fn drain_until_finished(process: &mut AgentProcess, timeout: std::time::Duration) -> String {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        for event in process.poll_events() {
            if let AgentEvent::TurnFinished { result_text, .. } = event {
                return result_text;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("no TurnFinished within {timeout:?}");
}

/// Real end-to-end: confirms `shutdown()` genuinely leaves no orphaned OS process behind, the
/// same discipline this project's `nvim --embed` handling was already held to (see
/// `poc/neovide_embed_live`'s own orphan-safety verification). Checking `/proc/<pid>` directly
/// (rather than trusting `shutdown()`'s own return or `has_exited()`) confirms the process is
/// actually gone from the OS's point of view, not just that this crate's bookkeeping thinks so.
#[test]
#[ignore]
fn shutdown_leaves_no_orphaned_process() {
    let session_id = Uuid::new_v4();
    let mut process = AgentProcess::spawn(
        "reply with exactly the word: pong",
        SpawnMode::New { session_id },
        CONSERVATIVE_DISALLOWED_TOOLS,
    )
    .unwrap();
    let pid = process_pid(&process);

    drain_until_finished(&mut process, std::time::Duration::from_secs(60));
    process.shutdown();

    assert!(!pid_is_alive(pid), "claude process {pid} should be gone after shutdown()");
}

#[cfg(unix)]
fn process_pid(process: &AgentProcess) -> u32 {
    process.pid()
}

#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    // Signal 0 checks liveness without actually sending a signal.
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}
