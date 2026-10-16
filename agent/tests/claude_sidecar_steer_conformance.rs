//! Messages put into a running turn (`AgentProvider::steer_turn`), proven against a real sidecar and a
//! real Claude CLI.
//!
//! `#[ignore]`d and billed: every test here spends real tokens, so it runs only under the TEST profile:
//! `cargo test -p agent --test claude_sidecar_steer_conformance -- --ignored --test-threads=1 --nocapture`,
//! with a sidecar that advertises `steer_turn` (Verdandi protocol 3.20 or later; see the environment
//! variables `EITRI_SIDECAR_BINARY` / `EITRI_VERDANDI_CHECKOUT`).
//!
//! Two shapes, the two the provider has to get right:
//! - a message steered while a tool runs is folded into that turn: one delivery in the running turn,
//!   one `TurnCompleted`, no second turn;
//! - a message steered and then interrupted runs after the turn as a follow-up the sidecar opens: the
//!   turn's end and the follow-up's start come out of ONE `pump()` (the provider holds the end until
//!   the start arrives), and the delivery is in the follow-up.

use agent::{
    AgentDomainEvent, AgentProvider, ClaudeSidecarProvider, CloseSessionRequest, CreateSessionRequest,
    InterruptTurnRequest, PermissionDecision, ResolvePermissionRequest, SendTurnRequest, SteerTurnRequest, TurnOutcome,
};
use std::time::{Duration, Instant};

const STEERED: &str = "Also: end your final answer with the single word PINEAPPLE.";

fn connect() -> ClaudeSidecarProvider {
    let provider = ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string())
        .expect("connecting to a real sidecar should succeed");
    let info = provider.info();
    eprintln!(
        "sidecar {} protocol {}.{}, CLI {}",
        info.sidecar_version, info.protocol_major, info.protocol_minor, info.actual_claude_code_version
    );
    assert!(
        provider.capabilities().steer,
        "this sidecar does not advertise steer_turn (protocol 3.20 and a CLI >= 2.1.252 are needed): {:?}",
        info.advertised_capabilities
    );
    provider
}

fn create_session(provider: &ClaudeSidecarProvider) -> String {
    provider
        .create_session(CreateSessionRequest {
            cwd: std::env::temp_dir().to_string_lossy().to_string(),
            streaming: agent::StreamingPreference::Partial,
            project: agent::setting_sources::ProjectTrust::Untrusted,
        })
        .expect("create_session should succeed")
}

/// Answers every `PermissionRequested` with `Allow`, as Eitri's bypass would.
fn allow_every_request(provider: &ClaudeSidecarProvider, session_id: &str, events: &[AgentDomainEvent]) {
    for event in events {
        if let AgentDomainEvent::PermissionRequested { permission_id, .. } = event {
            provider
                .resolve_permission(ResolvePermissionRequest {
                    session_id: session_id.to_string(),
                    permission_id: permission_id.clone(),
                    decision: PermissionDecision::Allow,
                })
                .expect("answering a pending request should succeed");
        }
    }
}

/// Pumps until `done` holds over everything seen, keeping each `pump()` batch apart (the follow-up
/// test reads which events came out together). Every request is allowed on the way.
fn pump_until(
    provider: &ClaudeSidecarProvider,
    session_id: &str,
    batches: &mut Vec<Vec<AgentDomainEvent>>,
    deadline_secs: u64,
    what: &str,
    done: impl Fn(&[AgentDomainEvent]) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(deadline_secs);
    loop {
        let batch = provider.pump();
        if !batch.is_empty() {
            allow_every_request(provider, session_id, &batch);
            batches.push(batch);
        }
        let all: Vec<AgentDomainEvent> = batches.iter().flatten().cloned().collect();
        if done(&all) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}; got: {all:#?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn flat(batches: &[Vec<AgentDomainEvent>]) -> Vec<AgentDomainEvent> {
    batches.iter().flatten().cloned().collect()
}

fn turn_started(events: &[AgentDomainEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::TurnStarted { turn_id } => Some(turn_id.clone()),
            _ => None,
        })
        .collect()
}

fn turn_completed(events: &[AgentDomainEvent]) -> Vec<(String, TurnOutcome)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::TurnCompleted { turn_id, outcome, .. } => Some((turn_id.clone(), *outcome)),
            _ => None,
        })
        .collect()
}

fn bash_started(events: &[AgentDomainEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, AgentDomainEvent::ToolCallStarted { name, .. } if name == "Bash"))
}

fn outcomes(events: &[AgentDomainEvent]) -> Vec<&AgentDomainEvent> {
    events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentDomainEvent::UserMessageDelivered { .. } | AgentDomainEvent::UserMessageDropped { .. }
            )
        })
        .collect()
}

fn reply_text(events: &[AgentDomainEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::ContentDelta {
                kind: agent::ContentKind::Text,
                text,
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Prints what the CLI listed as its slash commands, the first time a list arrives: the record of what
/// a Claude tab's `/` menu is built from.
fn print_commands(events: &[AgentDomainEvent]) {
    if let Some(AgentDomainEvent::CommandsAvailable { commands }) = events
        .iter()
        .find(|e| matches!(e, AgentDomainEvent::CommandsAvailable { .. }))
    {
        let names: Vec<String> = commands
            .iter()
            .map(|c| match &c.hint {
                Some(hint) => format!("{} {hint}", c.name),
                None => c.name.clone(),
            })
            .collect();
        eprintln!("CommandsAvailable: {} commands: {}", commands.len(), names.join(", "));
    } else {
        eprintln!("CommandsAvailable: none arrived");
    }
}

/// Starts a turn whose Bash call sleeps, and returns the turn's id once the call has started.
fn start_a_sleeping_turn(
    provider: &ClaudeSidecarProvider,
    session_id: &str,
    batches: &mut Vec<Vec<AgentDomainEvent>>,
) -> String {
    let turn = provider
        .send_turn(SendTurnRequest {
            session_id: session_id.to_string(),
            text: "Run exactly this Bash command: sleep 8 && echo slept. Then reply with one short sentence."
                .to_string(),
        })
        .expect("send_turn should be accepted");
    pump_until(
        provider,
        session_id,
        batches,
        90,
        "the Bash call to start",
        bash_started,
    );
    turn
}

fn steer(provider: &ClaudeSidecarProvider, session_id: &str, turn: &str, id: &str) {
    provider
        .steer_turn(SteerTurnRequest {
            session_id: session_id.to_string(),
            expected_turn_id: turn.to_string(),
            client_message_id: id.to_string(),
            text: STEERED.to_string(),
            as_typed: STEERED.to_string(),
        })
        .expect("the sidecar takes a message while a tool runs");
}

#[test]
#[ignore]
fn real_steer_during_a_bash_sleep_is_delivered_into_the_running_turn() {
    let provider = connect();
    let session_id = create_session(&provider);
    let mut batches = Vec::new();
    let turn = start_a_sleeping_turn(&provider, &session_id, &mut batches);
    steer(&provider, &session_id, &turn, "steer-tool-1");
    pump_until(&provider, &session_id, &mut batches, 120, "the turn's end", |all| {
        !turn_completed(all).is_empty()
    });
    // Anything left in flight lands too.
    std::thread::sleep(Duration::from_millis(500));
    batches.push(provider.pump());
    let all = flat(&batches);
    print_commands(&all);

    let fates = outcomes(&all);
    assert_eq!(
        fates,
        [&AgentDomainEvent::UserMessageDelivered {
            client_message_id: "steer-tool-1".into(),
            turn_id: turn.clone(),
            text: STEERED.into(),
        }],
        "exactly one outcome, a delivery into the running turn, carrying the typed text"
    );
    assert_eq!(turn_started(&all), [turn.clone()], "no follow-up turn");
    assert_eq!(turn_completed(&all), [(turn.clone(), TurnOutcome::Completed)]);
    // What the model made of it is printed, not asserted: of three runs on 2026-10-14 (CLI 2.1.293, the
    // TEST profile's default model, the user settings tier loaded), two followed it, and in the other
    // the model said it saw the message inside the command's output, took it for an instruction
    // injected through that output, and declined it. The protocol facts above are what this provider
    // owns.
    eprintln!("reply: {:?}", reply_text(&all));
    provider.close_session(CloseSessionRequest { session_id }).unwrap();
}

#[test]
#[ignore]
fn real_steer_then_interrupt_runs_as_a_follow_up_whose_start_comes_with_the_end() {
    let provider = connect();
    let session_id = create_session(&provider);
    let mut batches = Vec::new();
    let turn = start_a_sleeping_turn(&provider, &session_id, &mut batches);
    steer(&provider, &session_id, &turn, "steer-interrupt-1");
    // `steer_turn` returns before the message is sent (the provider's runtime sends it); give it time to
    // reach the CLI, or the interrupt could end the turn first and the message would be refused.
    std::thread::sleep(Duration::from_secs(1));
    provider
        .interrupt_turn(InterruptTurnRequest {
            session_id: session_id.clone(),
        })
        .expect("interrupt should be accepted");
    pump_until(
        &provider,
        &session_id,
        &mut batches,
        120,
        "the follow-up's end",
        |all| turn_completed(all).len() >= 2,
    );
    std::thread::sleep(Duration::from_millis(500));
    batches.push(provider.pump());
    let all = flat(&batches);

    let started = turn_started(&all);
    assert_eq!(started.len(), 2, "the turn and one follow-up: {started:?}");
    assert_eq!(started[0], turn);
    let follow_up = started[1].clone();
    assert_ne!(follow_up, turn);
    let ended = turn_completed(&all);
    assert_eq!(ended[0], (turn.clone(), TurnOutcome::Interrupted), "{ended:?}");
    assert_eq!(ended[1].0, follow_up);

    // The property the host depends on: the interrupted turn's end and the follow-up's start came out
    // of the same pump, in that order.
    let together = batches.iter().find(|batch| {
        batch
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::TurnCompleted { turn_id, .. } if *turn_id == turn))
    });
    let together = together.expect("the end was pumped");
    let at_end = together
        .iter()
        .position(|e| matches!(e, AgentDomainEvent::TurnCompleted { turn_id, .. } if *turn_id == turn));
    let at_start = together
        .iter()
        .position(|e| matches!(e, AgentDomainEvent::TurnStarted { turn_id } if *turn_id == follow_up));
    assert!(
        matches!((at_end, at_start), (Some(end), Some(start)) if end < start),
        "the turn's end and the follow-up's start must leave in one pump: {together:#?}"
    );

    for (n, batch) in batches.iter().enumerate() {
        let kinds: Vec<String> = batch
            .iter()
            .filter(|e| !matches!(e, AgentDomainEvent::ContentDelta { .. }))
            .map(|e| format!("{e:?}").chars().take(60).collect())
            .collect();
        if !kinds.is_empty() {
            eprintln!("pump {n}: {kinds:?}");
        }
    }
    eprintln!("follow-up reply: {:?}", reply_text(&all));

    let fates = outcomes(&all);
    assert_eq!(
        fates,
        [&AgentDomainEvent::UserMessageDelivered {
            client_message_id: "steer-interrupt-1".into(),
            turn_id: follow_up.clone(),
            text: STEERED.into(),
        }],
        "the interrupt did not withdraw it: it ran in the follow-up"
    );
    provider.close_session(CloseSessionRequest { session_id }).unwrap();
}
