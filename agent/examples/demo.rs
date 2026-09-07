//! A minimal, real usage demo of the `agent` v2 crate: starts a long-lived conversation, sends
//! two turns (the second recalling the first, proving multi-turn-in-one-process), demonstrates
//! a real tool call, and prints every `AgentEvent` as it arrives. Run with:
//!
//!   cargo run --manifest-path Cargo.toml -p agent --example demo
//!
//! Costs real API usage (roughly $0.10-0.20 for two turns) -- this is a real, live call, not a mock.
//!
//! Note on permission requests: the `PreToolUse` hook this crate installs matches *every* tool
//! call, including ones not on `CONSERVATIVE_DISALLOWED_TOOLS` (e.g. `Read`, which the first
//! prompt below deliberately triggers to demonstrate a real tool call). A real, unanswered
//! `PermissionRequest` blocks the turn until the CLI's own hook timeout elapses (600s,
//! deliberately not investigated by this crate -- see MANUAL_VERIFICATION.md's "explicitly
//! deferred" section) -- confirmed for real running this exact demo (Task 7). Since this demo has
//! no interactive UI to show a human the request, `send_and_wait` below auto-approves any pending
//! permission request so the demo completes in a reasonable time; a real approve/deny UI is left
//! to `agent-ui`, and `real_pretooluse_hook_allow_end_to_end`/`real_pretooluse_hook_deny_end_to_end`
//! are this crate's real round-trip tests of the same `respond_permission` call used here.

use agent::{AgentEvent, AgentSession, PermissionMode, CONSERVATIVE_DISALLOWED_TOOLS};

fn main() {
    let project_dir = std::env::current_dir().expect("cwd");
    let mut session = AgentSession::start(&project_dir, PermissionMode::Auto, CONSERVATIVE_DISALLOWED_TOOLS)
        .expect("failed to spawn `claude` -- is it installed and on PATH?");

    println!("== agent v2 demo ==\n");

    send_and_wait(&mut session, "Read the file agent/Cargo.toml and tell me, in one short sentence, what dependencies it declares.");
    send_and_wait(&mut session, "What was the first dependency you mentioned?");

    session.shutdown();
    println!("\n== session ended cleanly ==");
}

fn send_and_wait(session: &mut AgentSession, prompt: &str) {
    session.send_turn(prompt).expect("send_turn");
    loop {
        for event in session.pump() {
            print_event(&event);
        }
        // Answer every pending request, not just the newest: one assistant message can carry
        // several tool calls, each of which blocks its own `agent-hook` until answered.
        for pending in session.state.pending_permissions.clone() {
            println!(
                "[auto-approving] {} -- this demo has no interactive UI, see real_pretooluse_hook_allow_end_to_end/real_pretooluse_hook_deny_end_to_end for full round-trip examples"
                , pending.tool_name
            );
            session.respond_permission(&pending.request_id, true, None).expect("respond_permission");
        }
        if !session.state.turn_in_progress {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn print_event(event: &AgentEvent) {
    match event {
        AgentEvent::SessionStarted { session_id, model, cwd } => {
            println!("[session started] id={session_id} model={model} cwd={cwd}");
        }
        AgentEvent::AssistantText { text } => println!("[assistant] {text}"),
        AgentEvent::Thinking { .. } => println!("[thinking...]"),
        AgentEvent::ToolStarted { name, input, .. } => println!("[tool call] {name}({input})"),
        AgentEvent::ToolResult { content, is_error, .. } => {
            println!("[tool result] error={is_error} content={content}");
        }
        AgentEvent::PermissionRequest { tool_name, source, .. } => {
            println!("[permission request] tool={tool_name} source={source:?}");
        }
        AgentEvent::ControlResponse { subtype, .. } => println!("[control response] {subtype}"),
        AgentEvent::ProcessStderr { line } => println!("[stderr] {line}"),
        AgentEvent::ProcessExited { success } => println!("[process exited] success={success}"),
        AgentEvent::RateLimit { .. } => {}
        AgentEvent::TurnFinished { result_text, total_cost_usd, num_turns, .. } => {
            println!("\n[turn finished] cost=${total_cost_usd:.4} turns={num_turns}\nresult: {result_text}\n");
        }
        AgentEvent::Unknown { .. } => {}
    }
}
