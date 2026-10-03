//! Real, `#[ignore]`d, real API cost: confirms the full event pipeline (create_session ->
//! start_watching -> send_turn -> a real model reply's TextDelta/TurnCompleted events arriving via
//! pump()) works end to end against the real sidecar and real `claude` CLI. Task 9 covers
//! permission/interrupt/close-fail-close scenarios; this file is scoped to the plain-text happy
//! path only.

use agent::{AgentDomainEvent, AgentProvider, ClaudeSidecarProvider, CreateSessionRequest, TurnOutcome};

#[test]
#[ignore]
fn a_real_turn_s_reply_arrives_via_pump() {
    let provider = ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string()).unwrap();
    let session_id = provider
        // Trusted: this measures the real CLI with the project tiers loaded, as it always has.
        .create_session(CreateSessionRequest {
            cwd: std::env::temp_dir().to_string_lossy().to_string(),
            streaming: agent::StreamingPreference::Partial,
            project: agent::setting_sources::ProjectTrust::Trusted,
        })
        .unwrap();

    provider
        .send_turn(agent::SendTurnRequest {
            session_id: session_id.clone(),
            text: "reply with exactly the word: pong".into(),
        })
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut full_text = String::new();
    let mut completed = false;
    while std::time::Instant::now() < deadline && !completed {
        for event in provider.pump() {
            match event {
                AgentDomainEvent::ContentDelta { text, .. } => full_text.push_str(&text),
                AgentDomainEvent::TurnCompleted { outcome, .. } => {
                    assert_eq!(outcome, TurnOutcome::Completed);
                    completed = true;
                }
                _ => {}
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(completed, "turn did not complete within 30s");
    assert!(full_text.to_lowercase().contains("pong"), "got: {full_text}");

    provider
        .close_session(agent::CloseSessionRequest { session_id })
        .unwrap();
}
