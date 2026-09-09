//! A minimal, real usage demo of the `agent` v2 crate: starts a long-lived conversation, sends
//! two turns (the second recalling the first, proving multi-turn-in-one-process), demonstrates
//! a real tool call, and prints every `AgentDomainEvent` as it arrives. Run with:
//!
//!   cargo run --manifest-path Cargo.toml -p agent --example demo
//!
//! Costs real API usage (roughly $0.10-0.20 for two turns) -- this is a real, live call, not a mock.
//!
//! Note on permission requests: the `PreToolUse` hook this crate installs matches *every* tool
//! call, including ones not on `CONSERVATIVE_DISALLOWED_TOOLS` (e.g. `Read`, which the first
//! prompt below deliberately triggers to demonstrate a real tool call). A real, unanswered
//! permission request blocks the turn until the CLI's own hook timeout elapses (600s,
//! deliberately not investigated by this crate -- see MANUAL_VERIFICATION.md's "explicitly
//! deferred" section) -- confirmed for real running this exact demo (Task 7). Since this demo has
//! no interactive UI to show a human the request, `send_and_wait` below auto-approves any pending
//! permission request so the demo completes in a reasonable time; a real approve/deny UI is left
//! to `agent-ui`, and `real_pretooluse_hook_allow_end_to_end`/`real_pretooluse_hook_deny_end_to_end`
//! are this crate's real round-trip tests of the same `respond_permission` call used here.

use agent::{AgentDomainEvent, AgentSession, PermissionMode, CONSERVATIVE_DISALLOWED_TOOLS};

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
    for event in session.send_turn(prompt).expect("send_turn") {
        print_event(&event);
    }
    loop {
        for event in session.pump() {
            print_event(&event);
        }
        // Answer every pending request, not just the newest: one assistant message can carry
        // several tool calls, each of which blocks its own `agent-hook` until answered.
        let pending: Vec<(String, String)> = session
            .projection
            .pending_permissions
            .values()
            .map(|p| (p.permission_id.clone(), p.tool_name.clone()))
            .collect();
        for (permission_id, tool_name) in pending {
            println!(
                "[auto-approving] {tool_name} -- this demo has no interactive UI, see real_pretooluse_hook_allow_end_to_end/real_pretooluse_hook_deny_end_to_end for full round-trip examples"
            );
            for event in session.respond_permission(&permission_id, true, None).expect("respond_permission") {
                print_event(&event);
            }
        }
        if session.projection.active_turn_id.is_none() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn print_event(event: &AgentDomainEvent) {
    match event {
        AgentDomainEvent::SessionOpened { session_id, model, cwd } => {
            println!("[session opened] id={session_id} model={model} cwd={cwd}");
        }
        AgentDomainEvent::TurnStarted { turn_id } => println!("[turn started] {turn_id}"),
        AgentDomainEvent::ContentDelta { kind: agent::ContentKind::Text, text, .. } => println!("[assistant] {text}"),
        AgentDomainEvent::ContentDelta { kind: agent::ContentKind::Thinking, .. } => println!("[thinking...]"),
        AgentDomainEvent::ToolCallStarted { name, input, .. } => println!("[tool call] {name}({input})"),
        AgentDomainEvent::ToolCallCompleted { content, is_error, .. } => {
            println!("[tool result] error={is_error} content={content}");
        }
        AgentDomainEvent::PermissionRequested { tool_name, .. } => {
            println!("[permission request] tool={tool_name}");
        }
        AgentDomainEvent::PermissionResolved { permission_id, outcome } => {
            println!("[permission resolved] id={permission_id} outcome={outcome:?}");
        }
        AgentDomainEvent::TurnCompleted { result_text, outcome, total_cost_usd, num_turns, .. } => {
            println!("\n[turn completed] outcome={outcome:?} cost=${total_cost_usd:.4} turns={num_turns}\nresult: {result_text}\n");
        }
        AgentDomainEvent::SessionUnavailable { reason } => println!("[session unavailable] {reason}"),
        AgentDomainEvent::SessionClosed { reason } => println!("[session closed] {reason}"),
    }
}
