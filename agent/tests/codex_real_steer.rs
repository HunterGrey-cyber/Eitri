//! A message put into Codex's running turn (`turn/steer`), against the real `codex app-server` and the
//! scripted model: what Codex answers, when the message reaches the model, and what an interrupt does to one
//! it has not read. Every test is `#[ignore]`d: it needs a `codex` on `PATH` (or `EITRI_CODEX`) and runs it in
//! a private `CODEX_HOME` whose model provider is the scripted stand-in on `127.0.0.1`; no account is used and
//! no request leaves the machine. Run them with
//!
//! ```text
//! env -u TMUX -u TMUX_PANE TMPDIR=<a directory of its own> \
//!     cargo test -p agent --test codex_real_steer -- --ignored --test-threads=1
//! ```
//!
//! (a real Codex leaves empty `.git`/`.codex` mount targets in `TMPDIR`). Each test prints the
//! `codex --version` it ran against.

#[path = "support/codex_peer.rs"]
mod codex_peer;

use agent::external::DirTrust;
use agent::providers::codex::scripted_model::{ScriptedModel, Step};
use agent::providers::codex::wire::method;
use agent::providers::codex::{CodexLaunch, CodexProvider};
use agent::{
    AgentDomainEvent, AgentProvider, ExternalProvider, ExternalStart, InterruptTurnRequest, PermissionDecision,
    ProviderError, ProviderErrorCode, ResolvePermissionRequest, Reviewer, SendTurnRequest, SteerTurnRequest,
    TurnOutcome, TurnPolicy,
};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, Instant};

use codex_peer::*;

/// The model's script: an escalated command (a card) for each prompt that asks for one, then one line.
fn gated_model(outside: &Path) -> ScriptedModel {
    ScriptedModel::start(vec![
        Step::exec("g1", &format!("touch {}/one", text_of(outside)), true)
            .when_body_has("PROMPT-GATED")
            .unless_body_has("\"g1\""),
        Step::exec("g2", &format!("touch {}/two", text_of(outside)), true)
            .when_body_has("PROMPT-INTERRUPTED")
            .unless_body_has("\"g2\""),
    ])
    .expect("model")
}

fn steer_params(thread: &str, client_id: &str, text: &str, expected_turn: &str) -> Value {
    json!({
        "threadId": thread,
        "clientUserMessageId": client_id,
        "input": [{"type": "text", "text": text}],
        "expectedTurnId": expected_turn,
    })
}

/// The `userMessage` items carrying `client_id`, as `(method, turn id)`, in the order Codex sent them.
fn user_messages(peer: &Peer, client_id: &str) -> Vec<(String, String)> {
    peer.notes
        .iter()
        .filter(|n| n.method == method::ITEM_STARTED || n.method == method::ITEM_COMPLETED)
        .filter(|n| json_str(&n.params, "/item/type") == Some("userMessage"))
        .filter(|n| json_str(&n.params, "/item/clientId") == Some(client_id))
        .map(|n| {
            (
                n.method.clone(),
                json_str(&n.params, "/turnId").unwrap_or("").to_string(),
            )
        })
        .collect()
}

/// Where in `peer.notes` the first note satisfying `what` is.
fn note_index(peer: &Peer, what: impl Fn(&Note) -> bool) -> Option<usize> {
    peer.notes.iter().position(what)
}

/// What Codex itself does: it refuses a message with no turn running or for another turn, takes one while
/// a card waits under the same turn id, and reads it only once the gated call is over -- the `userMessage`
/// item with the client's id comes after the call's end and before the turn's, and the model's next request
/// carries the text.
#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn codex_takes_a_message_during_a_gated_call_and_reads_it_at_the_next_step() {
    let codex = real_codex();
    let s = Scratch::new("steer-peer");
    let outside = s.dir("outside");
    let model = gated_model(&outside);
    let home = s.dir("home");
    write_home(&home, &model, "");
    let project = s.dir("proj");
    write_project(&project, "");
    let mut peer = Peer::start(&codex, &home, &project, Box::new(|_| None));
    let thread = start_thread(&mut peer, start_params(&project, DirTrust::Untrusted));

    let idle = peer
        .call("turn/steer", steer_params(&thread.id, "c0", "IDLE", "no-turn"))
        .expect_err("no turn runs");
    assert!(idle.contains("no active turn to steer"), "{idle}");

    let turn = send_turn(&mut peer, &thread.id, "PROMPT-GATED");
    assert!(peer.wait_for(Duration::from_secs(30), |p| p
        .requests
        .iter()
        .any(|r| r.method == method::COMMAND_APPROVAL)));
    let other = peer
        .call("turn/steer", steer_params(&thread.id, "c1", "WRONG", "another-turn"))
        .expect_err("another turn");
    assert!(other.contains("expected active turn id"), "{other}");

    let before = model.requests().len();
    let answer = peer
        .call("turn/steer", steer_params(&thread.id, "c2", "STEER-TEXT", &turn))
        .expect("taken while the card waits");
    assert_eq!(
        json_str(&answer, "/turnId"),
        Some(turn.as_str()),
        "the same turn goes on"
    );
    peer.pump();
    assert!(user_messages(&peer, "c2").is_empty(), "not read while the call waits");
    assert_eq!(model.requests().len(), before, "and the model was not asked meanwhile");

    let request = peer
        .requests
        .iter()
        .find(|r| r.method == method::COMMAND_APPROVAL)
        .cloned()
        .unwrap();
    peer.link.respond(request.id, json!({"decision": "decline"}));
    assert_eq!(
        peer.wait_turn(&thread.id, &turn, TURN_BUDGET).as_deref(),
        Some("completed")
    );
    let read = user_messages(&peer, "c2");
    assert!(!read.is_empty(), "the userMessage item with the client's id");
    assert!(read.iter().all(|(_, t)| *t == turn), "{read:?}");
    let call_end = note_index(&peer, |n| {
        n.method == method::ITEM_COMPLETED && json_str(&n.params, "/item/type") == Some("commandExecution")
    })
    .expect("the gated call ended");
    let item = note_index(&peer, |n| json_str(&n.params, "/item/clientId") == Some("c2")).unwrap();
    let end = note_index(&peer, |n| n.method == method::TURN_COMPLETED).unwrap();
    assert!(
        call_end < item && item < end,
        "call end {call_end}, message {item}, turn end {end}"
    );
    let next = &model.requests()[before];
    assert!(next.to_string().contains("STEER-TEXT"), "the next request carries it");
    assert!(
        !model
            .requests()
            .iter()
            .any(|r| r.to_string().contains("WRONG") || r.to_string().contains("IDLE")),
        "a refused message reaches no model"
    );
    peer.close();
}

fn real_provider(codex: &Path, s: &Scratch, home: &Path, project: &Path) -> CodexProvider {
    let no_proxy = OsString::from("127.0.0.1,localhost");
    CodexProvider::connect_with_env(
        CodexLaunch {
            program: codex.to_path_buf(),
            cwd: project.to_path_buf(),
            state_home: s.dir("state"),
        },
        ["app-server", "--listen", "stdio://"]
            .iter()
            .map(OsString::from)
            .collect(),
        vec![
            (OsString::from("CODEX_HOME"), home.as_os_str().to_os_string()),
            (OsString::from("NO_PROXY"), no_proxy.clone()),
            (OsString::from("no_proxy"), no_proxy),
        ],
        [
            "TMUX",
            "TMUX_PANE",
            "NVIM",
            "NVIM_LISTEN_ADDRESS",
            "OPENAI_API_KEY",
            "CODEX_API_KEY",
        ]
        .iter()
        .map(OsString::from)
        .collect(),
    )
    .expect("the provider connects to the real codex")
}

/// Pumps until `done` holds over everything read, or fails after `within`.
fn pump_until(
    provider: &CodexProvider,
    seen: &mut Vec<AgentDomainEvent>,
    within: Duration,
    done: impl Fn(&[AgentDomainEvent]) -> bool,
) {
    let end = Instant::now() + within;
    loop {
        seen.extend(provider.pump());
        if done(seen) {
            return;
        }
        assert!(Instant::now() < end, "timed out; the events so far: {seen:#?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn steer(provider: &CodexProvider, thread: &str, turn: &str, id: &str, text: &str) -> Result<(), ProviderError> {
    provider.steer_turn(SteerTurnRequest {
        session_id: thread.to_string(),
        expected_turn_id: turn.to_string(),
        client_message_id: id.to_string(),
        text: format!("ctx\n\n{text}"),
        as_typed: text.to_string(),
    })
}

fn position(events: &[AgentDomainEvent], what: impl Fn(&AgentDomainEvent) -> bool) -> usize {
    events
        .iter()
        .position(what)
        .unwrap_or_else(|| panic!("not found in {events:#?}"))
}

fn the_card(events: &[AgentDomainEvent]) -> Option<String> {
    events.iter().find_map(|e| match e {
        AgentDomainEvent::PermissionRequested { permission_id, .. } => Some(permission_id.clone()),
        _ => None,
    })
}

fn turn_end(events: &[AgentDomainEvent]) -> Option<TurnOutcome> {
    events.iter().find_map(|e| match e {
        AgentDomainEvent::TurnCompleted { outcome, .. } => Some(*outcome),
        _ => None,
    })
}

/// What Eitri's provider makes of it: nothing is sent with no turn running (the host holds the message); a
/// message sent while a card waits is delivered under the same turn, with the typed text, before the turn's
/// end; and one Codex has not read when the user interrupts is dropped before the turn's end -- Codex forgets
/// it (the next turn's request does not carry it), so the host must send it again.
#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn the_provider_delivers_a_message_read_and_drops_one_an_interrupt_lost() {
    let codex = real_codex();
    let s = Scratch::new("steer-provider");
    let outside = s.dir("outside");
    let model = gated_model(&outside);
    let home = s.dir("home");
    write_home(&home, &model, "");
    let project = s.dir("proj");
    write_project(&project, "");
    let provider = real_provider(&codex, &s, &home, &project);
    assert!(provider.capabilities().steer);
    let policy = TurnPolicy {
        reviewer: Reviewer::User,
        sandbox: agent::Sandbox::Project,
    };
    let thread = provider
        .start(ExternalStart {
            cwd: text_of(&project).to_string(),
            plan: plan(&project, DirTrust::Untrusted),
            policy,
            resume: None,
            editor_server: None,
        })
        .expect("the session starts");

    match steer(&provider, &thread, "no-turn", "m0", "idle") {
        Err(ProviderError::Provider { code, .. }) => assert_eq!(code, ProviderErrorCode::NoActiveTurn),
        other => panic!("nothing is sent with no turn running: {other:?}"),
    }

    // Delivered: sent while the card waits, read once the card is answered.
    let send = |text: &str| {
        provider.send_turn_with(
            SendTurnRequest {
                session_id: thread.clone(),
                text: text.to_string(),
            },
            policy,
        )
    };
    let turn = send("PROMPT-GATED").expect("the turn starts");
    let mut events = Vec::new();
    pump_until(&provider, &mut events, Duration::from_secs(30), |e| {
        the_card(e).is_some()
    });
    steer(&provider, &thread, &turn, "m1", "STEER-READ").expect("sent");
    let card = the_card(&events).unwrap();
    provider
        .resolve_permission(ResolvePermissionRequest {
            session_id: thread.clone(),
            permission_id: card,
            decision: PermissionDecision::Deny { reason: None },
        })
        .expect("declined");
    pump_until(&provider, &mut events, TURN_BUDGET, |e| turn_end(e).is_some());
    let delivered = AgentDomainEvent::UserMessageDelivered {
        client_message_id: "m1".into(),
        turn_id: turn.clone(),
        text: "STEER-READ".into(),
    };
    assert!(
        position(&events, |e| *e == delivered)
            < position(&events, |e| matches!(e, AgentDomainEvent::TurnCompleted { .. }))
    );
    assert_eq!(turn_end(&events), Some(TurnOutcome::Completed));
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentDomainEvent::UserMessageDropped { .. })));
    assert!(model
        .requests()
        .iter()
        .any(|r| r.to_string().contains("ctx\\n\\nSTEER-READ")));

    // Dropped: sent while the card waits, then the user interrupts.
    let turn = send("PROMPT-INTERRUPTED").expect("the turn starts");
    let mut events = Vec::new();
    pump_until(&provider, &mut events, Duration::from_secs(30), |e| {
        the_card(e).is_some()
    });
    steer(&provider, &thread, &turn, "m2", "STEER-LOST").expect("sent");
    let until = Instant::now() + TURN_BUDGET;
    while !provider.steer_accepted_for_tests("m2") {
        assert!(Instant::now() < until, "Codex never accepted the message");
        events.extend(provider.pump());
        std::thread::yield_now();
    }
    provider
        .interrupt_turn(InterruptTurnRequest {
            session_id: thread.clone(),
        })
        .expect("interrupted");
    pump_until(&provider, &mut events, TURN_BUDGET, |e| turn_end(e).is_some());
    let dropped = AgentDomainEvent::UserMessageDropped {
        client_message_id: "m2".into(),
        reason: "the turn ended before Codex read it".into(),
    };
    assert!(
        position(&events, |e| *e == dropped)
            < position(&events, |e| matches!(e, AgentDomainEvent::TurnCompleted { .. })),
        "{events:#?}"
    );
    assert_eq!(turn_end(&events), Some(TurnOutcome::Interrupted));
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentDomainEvent::UserMessageDelivered { .. })));

    // Codex kept nothing of it: the next turn's request does not carry it.
    let from = model.requests().len();
    send("PROMPT-AFTER").expect("the next turn starts");
    let mut events = Vec::new();
    pump_until(&provider, &mut events, TURN_BUDGET, |e| turn_end(e).is_some());
    assert!(model.requests().len() > from);
    assert!(
        !requests_text(&model, from).contains("STEER-LOST"),
        "Codex forgot the message the interrupt caught, so Eitri sends it again"
    );
    provider
        .close_session(agent::CloseSessionRequest { session_id: thread })
        .expect("closed");
}

/// A message that arrives after the model's last reply, while the turn is finishing (a user `Stop` hook that
/// holds a file gate there): Codex takes it, reads it (`userMessage` with our id) and asks the model again with
/// it before the turn ends, so it is answered in that turn, not read and left unanswered (measured on 0.162.1).
#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn a_message_at_the_turns_end_window() {
    let codex = real_codex();
    let s = Scratch::new("steer-end-window");
    let model = ScriptedModel::start(vec![]).expect("model");
    let home = s.dir("home");
    let project = s.dir("proj");
    write_project(&project, "");
    let stopping = s.path("stop-hook-running");
    let release = s.path("stop-hook-release");
    write_file(
        &home.join("hooks.json"),
        &json!({"hooks": {"Stop": [{"hooks": [{
            "type": "command",
            "command": format!("/bin/sh -c 'touch {}; while [ ! -f {} ]; do sleep 0.01; done'", text_of(&stopping), text_of(&release)),
            "timeout": 180,
        }]}]}})
        .to_string(),
    );
    write_home(&home, &model, "");
    let mut peer = Peer::start(&codex, &home, &project, decline_all());
    let listed = peer
        .call("hooks/list", json!({ "cwds": [text_of(&project)] }))
        .expect("hooks/list");
    peer.close();
    let (key, hash) = find_any_hook(&listed).expect("Codex listed the user's Stop hook");
    write_home(
        &home,
        &model,
        &format!("\n[hooks.state.\"{key}\"]\ntrusted_hash = \"{hash}\"\nenabled = true\n"),
    );

    let mut peer = Peer::start(&codex, &home, &project, decline_all());
    let thread = start_thread(&mut peer, start_params(&project, DirTrust::Untrusted));
    let turn = send_turn(&mut peer, &thread.id, "PROMPT-END");
    assert!(appears(&stopping, Duration::from_secs(30)), "the Stop hook never ran");
    let before = model.requests().len();
    let answer = peer.call("turn/steer", steer_params(&thread.id, "late", "LATE-STEER", &turn));
    std::fs::write(&release, b"").unwrap();
    eprintln!("MEASURED turn/steer while the Stop hook runs: {answer:?}");
    let status = peer.wait_turn(&thread.id, &turn, TURN_BUDGET);
    peer.pump();
    let read = user_messages(&peer, "late");
    let item = note_index(&peer, |n| json_str(&n.params, "/item/clientId") == Some("late"));
    let reply_after = item.is_some_and(|at| {
        peer.notes[at..]
            .iter()
            .any(|n| n.method == method::ITEM_COMPLETED && json_str(&n.params, "/item/type") == Some("agentMessage"))
    });
    let asked_again = requests_text(&model, before).contains("LATE-STEER");
    eprintln!(
        "MEASURED turn ended {status:?}; userMessage with our id: {read:?}; a reply after it: {reply_after}; \
         model asked again with it: {asked_again}; requests {before} -> {}",
        model.requests().len()
    );
    assert!(answer.is_ok(), "taken while the turn finishes");
    assert_eq!(status.as_deref(), Some("completed"));
    assert!(
        read.iter().all(|(_, t)| *t == turn) && !read.is_empty(),
        "read in the same turn"
    );
    assert!(asked_again && reply_after, "and answered in it");
    peer.close();
}

fn find_any_hook(value: &Value) -> Option<(String, String)> {
    match value {
        Value::Object(map) => {
            if let (Some(Value::String(key)), Some(Value::String(hash))) = (map.get("key"), map.get("currentHash")) {
                return Some((key.clone(), hash.clone()));
            }
            map.values().find_map(find_any_hook)
        }
        Value::Array(items) => items.iter().find_map(find_any_hook),
        _ => None,
    }
}
