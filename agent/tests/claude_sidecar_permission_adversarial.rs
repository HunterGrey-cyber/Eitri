//! Adversarial coverage for the permission round trip: what happens when the human is slow, decides
//! twice, or is answering a session that is already gone.
//!
//! `#[ignore]`d and billed. Run with
//! `cargo test -p agent --test claude_sidecar_permission_adversarial -- --ignored --test-threads=1 --nocapture`.
//!
//! These run through `AgentConversation` rather than the raw provider, because that is the layer the
//! shell actually calls and the layer where a pending request lives long enough to go stale. The
//! happy path is already pinned at the provider level in `claude_sidecar_conformance.rs`.
//!
//! **Process safety**: one test kills a sidecar. It kills ONLY the pid the provider under test
//! reports having spawned -- never by name, because Claude Code sessions on a developer machine are
//! themselves processes named `claude`.

use agent::{
    AgentConversation, AgentDomainEvent, ClaudeSidecarProvider, PermissionDecision, PermissionMode,
    PermissionOutcome, ProjectionStatus,
};
use std::time::{Duration, Instant};

/// Asks for a tool the model cannot answer from memory, so `PreToolUse` actually fires.
const TOOL_PROMPT: &str = "run: echo neovibe_permission_probe, and tell me the exact output";

fn conversation() -> (AgentConversation, u32) {
    let provider = ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string())
        .expect("connecting to a real sidecar should succeed");
    let pid = provider.sidecar_pid();
    let cwd = std::env::temp_dir();
    // PermissionMode::Auto -> the sidecar's `interactive` policy. This is the mode the start screen
    // now offers on this backend, so it is the mode these tests must exercise.
    let conversation = AgentConversation::create(Box::new(provider), &cwd, PermissionMode::Auto)
        .expect("creating a conversation in interactive mode should succeed");
    (conversation, pid)
}

/// Pumps until `done`, folding as it goes. Returns every event seen.
fn drain_until<F: Fn(&AgentConversation) -> bool>(
    conversation: &mut AgentConversation,
    window: Duration,
    done: F,
) -> Vec<AgentDomainEvent> {
    let deadline = Instant::now() + window;
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        seen.extend(conversation.pump());
        if done(conversation) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    seen
}

fn wait_for_a_pending_permission(conversation: &mut AgentConversation) -> String {
    drain_until(conversation, Duration::from_secs(60), |c| !c.projection.pending_permissions.is_empty());
    conversation
        .projection
        .pending_permissions
        .keys()
        .next()
        .cloned()
        .expect("no PermissionRequested arrived -- interactive mode did not gate this tool call")
}

/// **A human who takes a minute to decide must not lose the turn.**
///
/// The pending request is held open by a real provider process the whole time. If anything in the
/// chain (the SDK's own permission wait, the sidecar, the transport) gave up on its own, the answer
/// would land on a request that no longer exists and the tool would never run -- while the panel,
/// which only ever sees `PermissionResolved`, showed a card that simply stopped responding.
#[test]
#[ignore]
fn a_decision_that_takes_a_minute_is_still_honored() {
    let (mut conversation, _) = conversation();
    conversation.send_turn(TOOL_PROMPT).unwrap();
    let permission_id = wait_for_a_pending_permission(&mut conversation);

    eprintln!("holding the decision for 60s, as a slow human would");
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        conversation.pump();
        std::thread::sleep(Duration::from_millis(200));
    }
    // Still pending after a full minute: nothing timed it out behind the user's back.
    assert!(
        conversation.projection.pending_permissions.contains_key(&permission_id),
        "the request was resolved by something other than the user during the wait"
    );
    assert!(
        !matches!(conversation.projection.status, ProjectionStatus::Unavailable { .. }),
        "the session did not survive an idle minute"
    );

    conversation.respond_permission(&permission_id, PermissionDecision::Allow).unwrap();

    let events = drain_until(&mut conversation, Duration::from_secs(60), |c| {
        c.projection.active_turn_id.is_none() && c.projection.pending_permissions.is_empty()
    });
    let outcome = events.iter().find_map(|e| match e {
        AgentDomainEvent::PermissionResolved { permission_id: id, outcome } if *id == permission_id => Some(*outcome),
        _ => None,
    });
    assert_eq!(outcome, Some(PermissionOutcome::Allowed), "got: {outcome:?}");

    let text: String = events
        .iter()
        .filter_map(|e| match e { AgentDomainEvent::ContentDelta { text, .. } => Some(text.as_str()), _ => None })
        .collect();
    assert!(
        text.contains("neovibe_permission_probe"),
        "the tool never actually ran after the late approval. got: {text}"
    );
    conversation.shutdown();
}

/// A second decision on the same request must be refused, and refused *benignly*.
///
/// Two ways this happens for real: a double-click while the resolution is in flight, and a card the
/// frontend has not cleared yet because `PermissionResolved` has not arrived. Neither may run the
/// tool twice, and neither may tear down a healthy conversation.
#[test]
#[ignore]
fn deciding_twice_is_refused_without_killing_the_conversation() {
    let (mut conversation, _) = conversation();
    conversation.send_turn(TOOL_PROMPT).unwrap();
    let permission_id = wait_for_a_pending_permission(&mut conversation);

    conversation.respond_permission(&permission_id, PermissionDecision::Allow).unwrap();

    // Immediately, before PermissionResolved can possibly have arrived: the projection still lists
    // it as pending, so this is the double-click case and not the already-cleared one.
    let second = conversation.respond_permission(&permission_id, PermissionDecision::Deny { reason: None });
    match second {
        Ok(()) => {
            // Also acceptable: the provider itself is the authority on idempotency. What is NOT
            // acceptable is the tool running twice, which the outcome assertion below covers.
            eprintln!("the second decision was accepted by the provider; checking it changed nothing");
        }
        Err(error) => {
            assert!(error.is_benign(), "a repeated decision must not be fatal: {error}");
            eprintln!("second decision refused benignly: {error}");
        }
    }

    let events = drain_until(&mut conversation, Duration::from_secs(60), |c| c.projection.active_turn_id.is_none());
    let outcomes: Vec<PermissionOutcome> = events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::PermissionResolved { permission_id: id, outcome } if *id == permission_id => Some(*outcome),
            _ => None,
        })
        .collect();
    assert_eq!(outcomes, vec![PermissionOutcome::Allowed], "exactly one resolution, and it is the first one");
    assert!(
        !matches!(conversation.projection.status, ProjectionStatus::Unavailable { .. }),
        "a double decision must not end the session"
    );

    // And the conversation still works afterwards.
    conversation.send_turn("reply with exactly: still-here").unwrap();
    let events = drain_until(&mut conversation, Duration::from_secs(60), |c| c.projection.active_turn_id.is_none());
    let text: String = events
        .iter()
        .filter_map(|e| match e { AgentDomainEvent::ContentDelta { text, .. } => Some(text.as_str()), _ => None })
        .collect();
    assert!(text.contains("still-here"), "got: {text}");
    conversation.shutdown();
}

/// An id that was never pending is refused before it reaches the wire.
#[test]
#[ignore]
fn an_unknown_permission_id_never_reaches_the_provider() {
    let (mut conversation, _) = conversation();
    conversation.send_turn(TOOL_PROMPT).unwrap();
    let real_id = wait_for_a_pending_permission(&mut conversation);

    let error = conversation
        .respond_permission("not-a-real-permission-id", PermissionDecision::Allow)
        .expect_err("an unknown id must be refused");
    assert!(error.is_benign(), "an unknown id is a stale click, not a broken session: {error}");

    // The real one still works afterwards -- the refusal did not disturb the pending request.
    conversation.respond_permission(&real_id, PermissionDecision::Allow).unwrap();
    drain_until(&mut conversation, Duration::from_secs(60), |c| c.projection.active_turn_id.is_none());
    conversation.shutdown();
}

/// **The session dies while a decision is outstanding.**
///
/// The card is on screen, the user has not clicked, and the provider goes away. What must happen:
/// a typed `SessionUnavailable`, a projection that ends `Unavailable`, and the pending request still
/// *listed* -- it genuinely was never answered, and removing it would read as a resolution nobody
/// made. `PermissionCard`'s `sessionEnded` prop is what makes that surviving record inert in the UI
/// instead of a button that posts into a session that no longer exists.
#[test]
#[ignore]
fn a_session_that_dies_with_a_decision_outstanding_reports_it_and_keeps_the_record() {
    let (mut conversation, sidecar_pid) = conversation();
    conversation.send_turn(TOOL_PROMPT).unwrap();
    let permission_id = wait_for_a_pending_permission(&mut conversation);
    eprintln!("killing sidecar pid {sidecar_pid} with {permission_id} still unanswered");

    // ONLY the pid this test's own provider reported. Never by name.
    // SAFETY: `sidecar_pid` came from that provider's own `Child::id()`.
    let killed = unsafe { libc::kill(sidecar_pid as i32, libc::SIGKILL) };
    assert_eq!(killed, 0, "could not signal the sidecar this test spawned");

    drain_until(&mut conversation, Duration::from_secs(30), |c| {
        matches!(c.projection.status, ProjectionStatus::Unavailable { .. })
    });

    let ProjectionStatus::Unavailable { reason } = &conversation.projection.status else {
        panic!("a session that died with a permission pending reported nothing: {:?}", conversation.projection.status);
    };
    eprintln!("reported: {reason}");
    assert!(!reason.trim().is_empty());

    assert!(
        conversation.projection.pending_permissions.contains_key(&permission_id),
        "the unanswered request was silently dropped -- that reads as a resolution nobody made"
    );
    assert_eq!(conversation.projection.active_turn_id, None, "the turn cannot still be in progress");

    // And answering it now fails instead of appearing to work.
    let error = conversation
        .respond_permission(&permission_id, PermissionDecision::Allow)
        .expect_err("a decision posted into a dead session must fail, not appear to succeed");
    eprintln!("late decision refused: {error}");
    conversation.shutdown();
}
