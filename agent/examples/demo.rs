//! A minimal, real usage demo of the `agent` crate: starts a genuine `claude -p` session, asks
//! it a question that requires reading a real file (exercising a real tool call, not just a text
//! reply), and prints every `AgentEvent` as it arrives. Run with:
//!
//!   cargo run --manifest-path Cargo.toml -p agent --example demo
//!
//! Costs real API usage (same order of magnitude as this crate's own `#[ignore]`d tests, roughly
//! $0.10-0.15) -- this is a real, live call, not a mock.

use agent::{AgentEvent, AgentSession, CONSERVATIVE_DISALLOWED_TOOLS};

fn main() {
    let (mut session, session_id) = AgentSession::start_new(
        "Read the file agent/Cargo.toml and tell me, in one short sentence, what dependencies it declares.",
        CONSERVATIVE_DISALLOWED_TOOLS,
    )
    .expect("failed to spawn `claude` -- is it installed and on PATH?");

    println!("== agent demo == session {session_id}\n");

    loop {
        for event in session.pump() {
            match event {
                AgentEvent::SessionStarted { session_id, model, cwd } => {
                    println!("[session started] id={session_id} model={model} cwd={cwd}");
                }
                AgentEvent::AssistantText { text } => {
                    println!("[assistant] {text}");
                }
                AgentEvent::Thinking { .. } => {
                    println!("[thinking...]");
                }
                AgentEvent::ToolStarted { name, input, .. } => {
                    println!("[tool call] {name}({input})");
                }
                AgentEvent::ToolResult { content, is_error, .. } => {
                    println!("[tool result] error={is_error} content={content}");
                }
                AgentEvent::ProcessStderr { line } => {
                    println!("[stderr] {line}");
                }
                AgentEvent::ProcessExited { success } => {
                    println!("[process exited] success={success}");
                }
                AgentEvent::RateLimit { .. } => {}
                AgentEvent::TurnFinished { result_text, total_cost_usd, num_turns, .. } => {
                    println!(
                        "\n[turn finished] cost=${total_cost_usd:.4} turns={num_turns}\nresult: {result_text}"
                    );
                }
                AgentEvent::Unknown { .. } => {}
            }
        }

        if matches!(session.state.status, agent::SessionStatus::Finished { .. }) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    session.shutdown();
    println!("\n== session ended cleanly ==");
}
