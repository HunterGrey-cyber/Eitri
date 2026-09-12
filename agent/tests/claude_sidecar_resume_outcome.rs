//! Real-sidecar coverage for the provider's resume verdict: that a refused resume is reported as a
//! refusal rather than inferred from silence, and never degrades into a fresh session.
//!
//! `#[ignore]`d and billed. Run with
//! `cargo test -p agent --test claude_sidecar_resume_outcome -- --ignored --test-threads=1 --nocapture`
//!
//! **What changed, and why the old coverage could not catch it.** `CreateSession` returns before the
//! provider has tried to attach, so this client used to wait three seconds and read silence as
//! success. Measured, a real rejection surfaces at 1.7-2.3s -- under load it lands outside that
//! window and becomes an accepted, empty, dead conversation. Worse, the positive half of the check
//! could never run at all: it compared `SessionOpened.provider_session_id` to the requested id, but
//! the provider only reports its session id at the start of a TURN, and `resume()` sends none. The
//! provider now states a typed verdict instead, and these drive it.

use agent::{AgentConversation, ClaudeSidecarProvider, ConversationError, PermissionMode};

fn provider() -> std::sync::Arc<ClaudeSidecarProvider> {
    std::sync::Arc::new(
        ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string())
            .expect("connecting to a real sidecar should succeed"),
    )
}

/// **A resume that cannot be honored fails as a resume.**
///
/// The forbidden outcome is not "an error message" -- it is an apparently healthy, empty
/// conversation that the user believes is their previous one. Measured before this protocol existed:
/// resuming `00000000-dead-beef-...` returned Ok and produced exactly that.
#[test]
#[ignore]
fn resuming_a_session_that_does_not_exist_fails_as_a_resume() {
    let cwd = std::env::temp_dir();
    let missing = "00000000-dead-beef-0000-000000000000";

    let started = std::time::Instant::now();
    let result = AgentConversation::resume(provider(), &cwd, missing, PermissionMode::Bypass);
    let elapsed = started.elapsed();

    let error = match result {
        Ok(conversation) => panic!(
            "a resume of a nonexistent session produced a conversation (session_id={:?}, status={:?}). \
             That is the silent substitution this protocol exists to prevent.",
            conversation.session_id(),
            conversation.projection().status,
        ),
        Err(error) => error,
    };
    eprintln!("refused after {}ms: {error}", elapsed.as_millis());

    // Typed as a refused RESUME, not as a generic provider failure -- the two call for different
    // things from a user, and only one of them means "that conversation is gone".
    match &error {
        ConversationError::ResumeRejected { provider_session_id, reason } => {
            assert_eq!(provider_session_id, missing);
            assert!(
                reason.contains(missing),
                "the reason must name the session that could not be continued: {reason}"
            );
            // Ends with what to do instead. An error that only reports a fault leaves the reader on
            // a start screen with no next step.
            assert!(reason.contains("Start a new session"), "{reason}");
        }
        other => panic!("expected a typed ResumeRejected, got {other:?}"),
    }

    // Not benign: the caller asked for a conversation and does not have one, so the panel must show
    // the start screen rather than reporting a recoverable hiccup and carrying on.
    assert!(!error.is_benign(), "a refused resume must not be treated as a survivable command error");
}

/// **The refusal is a verdict the provider stated, not a conclusion drawn from silence.**
///
/// Discriminated by CONTENT rather than by timing. The measured verdict latency (~2.7s) sits close
/// enough to the 3s wait that a clock-based assertion would be a coin flip -- and worse, it would
/// still pass if the client went back to inferring, as long as the inference happened to be fast.
/// The provider's own sentence cannot be produced by a timer at all, which is what makes it proof.
#[test]
#[ignore]
fn the_refusal_carries_the_providers_own_account_rather_than_an_inference() {
    let cwd = std::env::temp_dir();
    let started = std::time::Instant::now();
    let result = AgentConversation::resume(
        provider(),
        &cwd,
        "11111111-2222-3333-4444-555555555555",
        PermissionMode::Bypass,
    );
    eprintln!("verdict in {}ms", started.elapsed().as_millis());

    let error = result.err().expect("a nonexistent session must not resume");
    let rendered = error.to_string();
    eprintln!("rendered: {rendered}");

    // The CLI's own words, carried through the kernel (which used to bind no error at all), the
    // sidecar, and this client. A timeout has nothing to say about WHY.
    assert!(
        rendered.contains("No conversation found with session ID"),
        "the refusal does not carry the provider's own explanation, so it cannot be distinguished \
         from a guess: {rendered}"
    );
    // And it is rendered once, not wrapped in a second account of the same event.
    assert_eq!(
        rendered.matches("Start a new session").count(),
        1,
        "the reason is being double-wrapped: {rendered}"
    );
}
