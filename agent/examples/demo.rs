//! A live demo of the `agent` v2 crate: start a long-lived conversation, send two turns,
//! recall the first turn in the second, trigger a tool call, and print each `AgentDomainEvent`.
//! Run with:
//!
//!   cargo run --manifest-path Cargo.toml -p agent --example demo
//!
//! This uses the real API and incurs usage costs.
//!
//! The `PreToolUse` hook matches every tool call, including `Read` in the first prompt and
//! tools outside `CONSERVATIVE_DISALLOWED_TOOLS`. An unanswered permission request blocks
//! the turn until the CLI hook timeout. With no interactive permission UI, `send_and_wait`
//! auto-approves pending requests so the demo can complete. `agent-ui` provides that UI;
//! `real_pretooluse_hook_allow_end_to_end` and `real_pretooluse_hook_deny_end_to_end` exercise
//! the same `respond_permission` call.

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
            for event in session.respond_permission(&permission_id, agent::PermissionDecision::Allow).expect("respond_permission") {
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
        AgentDomainEvent::SessionOpened { session_id, provider_session_id, model, cwd } => {
            println!("[session opened] id={session_id} provider_session_id={provider_session_id} model={model} cwd={cwd}");
        }
        AgentDomainEvent::TurnStarted { turn_id } => println!("[turn started] {turn_id}"),
        AgentDomainEvent::UserPromptSubmitted { text } => println!("[user] {text}"),
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
        AgentDomainEvent::TurnCompleted { result_text, outcome, usage, .. } => {
            // "unknown", not "$0.0000": a backend that reports no usage (the sidecar provider, whose
            // wire has no usage fields) must not print a figure it was never given.
            let cost = match usage {
                Some(usage) => format!("${:.4} turns={}", usage.total_cost_usd, usage.num_turns),
                None => "unknown (this backend reports no usage)".to_string(),
            };
            println!("\n[turn completed] outcome={outcome:?} cost={cost}\nresult: {result_text}\n");
        }
        AgentDomainEvent::ResumeOutcome { requested_provider_session_id, status, attached_provider_session_id, forked, detail } => {
            println!(
                "[resume {status:?}] requested={requested_provider_session_id} attached={} forked={forked} detail={}",
                attached_provider_session_id.as_deref().unwrap_or("-"),
                detail.as_deref().unwrap_or("-"),
            );
        }
        AgentDomainEvent::SessionUnavailable { reason } => println!("[session unavailable] {reason}"),
        AgentDomainEvent::SessionClosed { reason } => println!("[session closed] {reason}"),
    }
}
