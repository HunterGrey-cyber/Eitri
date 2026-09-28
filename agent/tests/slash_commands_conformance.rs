//! Probe what happens when a Claude Code slash command reaches the sidecar as prompt text.
//! Commands such as `/compact` need observation to determine their effect through this backend.
//! This ignored test makes real, billed CLI calls. Run it once, alone, in an isolated profile.
//! The only outcome assertion is completion or timeout. Classify the printed results by hand
//! as works, sent as text, no-op, error or hangs, using events, reply text and token usage.
//!
//! Run:
//! ```sh
//! claude --version   # record the CLI build
//! ANTHROPIC_MODEL=haiku cargo test -p agent --test slash_commands_conformance -- \
//!     --ignored --nocapture --test-threads=1
//! ```

use agent::{
    AgentDomainEvent, AgentProvider, ClaudeSidecarProvider, CloseSessionRequest, ContentKind, CreateSessionRequest,
    InterruptTurnRequest, PermissionDecision, ResolvePermissionRequest, SendTurnRequest, StreamingPreference,
    UsageInfo,
};
use std::time::{Duration, Instant};

/// The commands probed, in a fixed order so results can be compared row for row.
const COMMANDS: &[&str] = &[
    "/help",
    "/clear",
    "/compact",
    "/cost",
    "/context",
    "/usage",
    "/status",
    "/model",
    "/config",
    "/memory",
    "/init",
    "/review",
    "/security-review",
    "/pr-comments",
    "/todos",
    "/export",
    "/mcp",
    "/agents",
    "/hooks",
    "/permissions",
    "/add-dir",
    "/resume",
    "/rewind",
    "/login",
    "/logout",
    "/doctor",
    "/release-notes",
    "/output-style",
];

/// `/compact` and `/clear` need prior history to make their effects observable:
/// shrinking the next turn's input and starting a fresh context, respectively.
const NEEDS_WARMUP: &[&str] = &["/compact", "/clear"];

const TURN_DEADLINE: Duration = Duration::from_secs(60);
const INTERRUPT_DEADLINE: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// A scratch cwd under `$HOME/.cache`, never `/tmp` (a small shared tmpfs, per the plan's global
/// constraints) and never this repository -- a command that gets treated as a real instruction (a
/// bare `/init` reads as "initialize this project" to a model that has seen real Claude Code
/// transcripts, and could reach for `Write`/`Bash`) lands here, not in a checkout or a shared scratch
/// space.
fn scratch_cwd() -> std::path::PathBuf {
    let home = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let dir = home.join(".cache/nv-v1ui-10-slash-probe");
    std::fs::create_dir_all(&dir).expect("scratch cwd must be creatable");
    dir
}

/// Data for classifying one command by hand: the events seen, any assistant text,
/// the turn's token usage, and the final status.
struct TurnResult {
    /// `Debug`-formatted, in arrival order -- deliberately not reduced to a type name, since
    /// `ToolCallStarted`'s `name`/`input` and `TurnCompleted`'s `outcome` are exactly what tells a
    /// "no-op" apart from a "works".
    events: Vec<String>,
    assistant_text: String,
    usage: Option<UsageInfo>,
    /// "completed" | "timed_out" | "send_error"
    final_status: &'static str,
}

/// Sends `text` on `session_id` and waits up to `TURN_DEADLINE`, recording everything the
/// classification step needs. Never panics on a slow or odd reply -- observing that IS the point.
fn run_probe_turn(provider: &ClaudeSidecarProvider, session_id: &str, text: &str) -> TurnResult {
    let mut result = TurnResult {
        events: Vec::new(),
        assistant_text: String::new(),
        usage: None,
        final_status: "timed_out",
    };

    if let Err(err) = provider.send_turn(SendTurnRequest {
        session_id: session_id.to_string(),
        text: text.to_string(),
    }) {
        result.events.push(format!("send_turn error: {err:?}"));
        result.final_status = "send_error";
        return result;
    }

    let deadline = Instant::now() + TURN_DEADLINE;
    let mut completed = false;
    while Instant::now() < deadline && !completed {
        for event in provider.pump() {
            if let AgentDomainEvent::ContentDelta {
                kind: ContentKind::Text,
                text,
                ..
            } = &event
            {
                result.assistant_text.push_str(text);
            }
            // Answer permission requests so a command that calls a tool can finish
            // and be measured without stalling on a card.
            if let AgentDomainEvent::PermissionRequested { permission_id, .. } = &event {
                let _ = provider.resolve_permission(ResolvePermissionRequest {
                    session_id: session_id.to_string(),
                    permission_id: permission_id.clone(),
                    decision: PermissionDecision::Allow,
                });
            }
            if let AgentDomainEvent::TurnCompleted { usage, .. } = &event {
                result.final_status = "completed";
                result.usage = *usage;
                completed = true;
            }
            result.events.push(format!("{event:?}"));
        }
        if !completed {
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    if !completed {
        // Courtesy interrupt: a command that hangs should not keep running (and billing) in the
        // background once the probe has moved on to the next command. Best-effort only -- the
        // recorded `final_status` stays "timed_out" regardless of whether the interrupt lands.
        let _ = provider.interrupt_turn(InterruptTurnRequest {
            session_id: session_id.to_string(),
        });
        let interrupt_deadline = Instant::now() + INTERRUPT_DEADLINE;
        while Instant::now() < interrupt_deadline {
            for event in provider.pump() {
                result.events.push(format!("(post-timeout) {event:?}"));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    result
}

#[test]
#[ignore]
fn probe_which_slash_commands_do_something_through_the_sidecar() {
    let provider = ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string())
        .expect("connecting to a real sidecar should succeed");
    let cwd = scratch_cwd().to_string_lossy().to_string();

    for command in COMMANDS {
        let session_id = match provider.create_session(CreateSessionRequest {
            cwd: cwd.clone(),
            // Every session is gated. `run_probe_turn` answers each request `Allow`
            // so commands can complete their tool calls.
            streaming: StreamingPreference::Complete,
        }) {
            Ok(id) => id,
            Err(err) => {
                eprintln!(
                    "{}",
                    serde_json::json!({
                        "command": command,
                        "final_status": "create_session_error",
                        "error": format!("{err:?}"),
                    })
                );
                continue;
            }
        };

        if NEEDS_WARMUP.contains(command) {
            let warmup = run_probe_turn(
                &provider,
                &session_id,
                "This is a warm-up message so this session has some history before the real probe \
                 turn. Reply with a short acknowledgement only.",
            );
            if warmup.final_status != "completed" {
                eprintln!(
                    "{}",
                    serde_json::json!({
                        "command": command,
                        "final_status": "warmup_failed",
                        "warmup_events": warmup.events,
                    })
                );
                let _ = provider.close_session(CloseSessionRequest { session_id });
                continue;
            }
        }

        let result = run_probe_turn(&provider, &session_id, command);
        let stderr_tail = provider.sidecar_stderr_tail();

        eprintln!(
            "{}",
            serde_json::json!({
                "command": command,
                "final_status": result.final_status,
                "assistant_text": result.assistant_text,
                "usage": result.usage,
                "events": result.events,
                "sidecar_stderr_tail": stderr_tail,
            })
        );

        let _ = provider.close_session(CloseSessionRequest { session_id });
    }
}
