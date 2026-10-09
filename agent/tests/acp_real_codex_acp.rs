//! `codex-acp` itself, registered as an ACP agent, in a scratch root: what the stand-in cannot show, which is the
//! real adapter's traffic going through `AcpProvider` end to end. **This spends real turns.**
//!
//! - The test is `#[ignore]`d and is run by hand, once, by whoever owns the Codex login:
//!   `cargo test -p agent --test acp_real_codex_acp -- --ignored --test-threads=1`. It needs `npx` and a `codex` on
//!   `PATH` (the adapter runs that `codex`, through `CODEX_PATH`, not the copy it bundles), and the adapter package
//!   `@agentclientprotocol/codex-acp`, which `npx` fetches on first use. It spends two turns of its own, on the
//!   cheapest model the account lists (`EITRI_CODEX_LOGIN_MODEL` names another) at the lowest reasoning effort
//!   (`EITRI_CODEX_LOGIN_EFFORT` names another).
//! - **The adapter spends a third turn the test cannot hold to its posture.** After the first turn it starts a thread
//!   of its own to write the session's title, on a model it picks, with nothing but the directory: `CODEX_CONFIG`
//!   does not reach that thread, so the account's own tool servers can start for it. No client setting turns the
//!   title off, and overrides on the `codex` command line that only disable a server are refused as incomplete
//!   server entries. Settle how to hold that thread before running this again.
//! - The login is the machine's default one: this file never reads, copies or prints a credential and never writes
//!   under the account's Codex home. The adapter is told, through `CODEX_CONFIG`, to start none of the account's
//!   own tool servers (their names come from the app-server's `config/read`; `EITRI_CODEX_LOGIN_MCP` names more).
//! - The posture is the registration's: `auto_mode = "workspace-write"`, a scratch root under the test target's
//!   directory (never `/tmp`) with no `.git` and no `.codex/`. A real model may not do what the prompt asks; a turn
//!   that does not try says so and fails rather than passing on nothing.
//! - `TMUX` and `TMUX_PANE` are removed from this process before the agent starts, so the account's own status hook
//!   exits at once. The test therefore changes its own environment and must run on one thread.

#[path = "support/codex_peer.rs"]
mod codex_peer;

use agent::providers::acp::{AcpAgentSpec, AcpProvider};
use agent::{
    AgentDomainEvent, AgentProvider, CloseSessionRequest, DirTrust, ExternalProvider, ExternalStart,
    PermissionDecision, ResolvePermissionRequest, Reviewer, SendTurnRequest, TrustPlan, TurnOutcome, TurnPolicy,
};
use codex_peer::{decline_all, Peer, Scratch};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// A real model and a first `npx` fetch take longer than a scripted agent.
const REAL_TURN: Duration = Duration::from_secs(240);

/// The program `name` resolves to on `PATH`, if any.
fn on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

fn version_of(program: &Path) -> String {
    Command::new(program)
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|e| format!("unknown ({e})"))
}

/// The cheapest model the account lists (a "mini" one), or the one `EITRI_CODEX_LOGIN_MODEL` names.
fn cheapest_model(peer: &mut Peer) -> String {
    if let Ok(name) = std::env::var("EITRI_CODEX_LOGIN_MODEL") {
        return name;
    }
    let listed = peer.call("model/list", json!({})).expect("model/list");
    let models: Vec<String> = listed
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|m| m.get("hidden").and_then(Value::as_bool) != Some(true))
        .filter_map(|m| m.get("model").or_else(|| m.get("id")).and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    eprintln!("models offered: {models:?}");
    models
        .into_iter()
        .find(|m| m.contains("mini"))
        .expect("no \"mini\" model is listed for this account; set EITRI_CODEX_LOGIN_MODEL to the cheapest one")
}

/// The adapter's `CODEX_CONFIG`: the model and effort of the test, and every tool server of the account off.
fn adapter_config(codex: &Path, cwd: &Path) -> String {
    let mut peer = Peer::start_on_login(codex, cwd, decline_all());
    let model = cheapest_model(&mut peer);
    let answer = peer
        .call("config/read", json!({"includeLayers": false}))
        .expect("config/read answers; without it the account's tool servers cannot be turned off");
    let mut names: Vec<String> = answer
        .pointer("/config/mcp_servers")
        .and_then(Value::as_object)
        .map(|servers| servers.keys().cloned().collect())
        .unwrap_or_default();
    if let Ok(more) = std::env::var("EITRI_CODEX_LOGIN_MCP") {
        names.extend(
            more.split(',')
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_string),
        );
    }
    names.sort();
    names.dedup();
    eprintln!("model: {model}; account tool servers turned off: {names:?}");
    peer.close();
    let servers: serde_json::Map<String, Value> = names
        .into_iter()
        .map(|name| (name, json!({"enabled": false})))
        .collect();
    // `EITRI_CODEX_LOGIN_EFFORT` names another reasoning effort, as the login checks of Codex tabs allow.
    let effort = std::env::var("EITRI_CODEX_LOGIN_EFFORT").unwrap_or_else(|_| "low".to_string());
    eprintln!("reasoning effort: {effort}");
    json!({"model": model, "model_reasoning_effort": effort, "mcp_servers": servers}).to_string()
}

fn pump_until(provider: &AcpProvider, what: &str, done: impl Fn(&AgentDomainEvent) -> bool) -> Vec<AgentDomainEvent> {
    let deadline = Instant::now() + REAL_TURN;
    let mut all = Vec::new();
    loop {
        let batch = provider.pump();
        let hit = batch.iter().any(&done);
        all.extend(batch);
        if hit {
            return all;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}; saw {all:#?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn is_turn_end(event: &AgentDomainEvent) -> bool {
    matches!(event, AgentDomainEvent::TurnCompleted { .. })
}

fn turn_outcome(events: &[AgentDomainEvent]) -> TurnOutcome {
    events
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::TurnCompleted { outcome, .. } => Some(*outcome),
            _ => None,
        })
        .expect("a turn ending")
}

fn send(provider: &AcpProvider, session: &str, text: &str) {
    provider
        .send_turn_with(
            SendTurnRequest {
                session_id: session.to_string(),
                text: text.to_string(),
            },
            TurnPolicy {
                reviewer: Reviewer::Agent,
            },
        )
        .expect("the turn is sent");
}

#[test]
#[ignore = "spends two real Codex turns on the machine's login; needs npx and codex on PATH"]
fn codex_acp_as_a_registered_agent_in_a_trusted_scratch_root() {
    let npx = on_path("npx").expect("npx on PATH (the adapter is run as `npx @agentclientprotocol/codex-acp`)");
    let codex = on_path("codex").expect("a codex on PATH (the adapter runs it through CODEX_PATH)");
    eprintln!(
        "codex: {} at {}; npx at {}",
        version_of(&codex),
        codex.display(),
        npx.display()
    );

    let scratch = Scratch::new("acp-codex");
    let root = scratch.dir("root");
    let outside = scratch.dir("outside");
    std::fs::write(root.join("README.txt"), "scratch root for the codex-acp check\n").expect("README");
    let target = outside.join("not-written.txt");

    let config = adapter_config(&codex, &root);
    std::env::remove_var("TMUX");
    std::env::remove_var("TMUX_PANE");
    std::env::set_var("CODEX_PATH", &codex);
    std::env::set_var("CODEX_CONFIG", &config);

    // The registration `init.lua` would make: the argv, and the one mode Eitri sets in both of its modes.
    let spec = AcpAgentSpec {
        name: "codex-acp".to_string(),
        argv: vec![
            OsString::from(&npx),
            OsString::from("--yes"),
            OsString::from("@agentclientprotocol/codex-acp"),
        ],
        auto_mode: Some("workspace-write".to_string()),
    };
    let provider = AcpProvider::connect(spec, root.clone()).expect("codex-acp starts and answers initialize");
    let session = provider
        .start(ExternalStart {
            cwd: root.to_string_lossy().into_owned(),
            plan: TrustPlan {
                dirs: vec![(root.clone(), DirTrust::Trusted)],
                skills_off: false,
                fingerprint: "real-codex-acp".to_string(),
                project_only: Vec::new(),
            },
            policy: TurnPolicy {
                reviewer: Reviewer::Agent,
            },
            resume: None,
        })
        .expect("a session opens in workspace-write mode");
    eprintln!(
        "session {session}; loadSession: {}; context gauge: {:?}",
        provider.can_load_session(),
        provider.context_gauge()
    );

    // Turn 1: no tool, only the stream and the ending.
    send(
        &provider,
        &session,
        "Reply with the single word ready. Do not run any command.",
    );
    let events = pump_until(&provider, "turn 1 to end", is_turn_end);
    assert_eq!(turn_outcome(&events), TurnOutcome::Completed, "turn 1: {events:#?}");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::ContentDelta { text, .. } if !text.is_empty())),
        "turn 1 streamed no text: {events:#?}"
    );
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentDomainEvent::SessionUnavailable { .. } | AgentDomainEvent::PolicyMismatch { .. }
        )),
        "the session tripped on turn 1: {events:#?}"
    );

    // Turn 2: a write outside the root is the one thing workspace-write sends to the client as a card.
    send(
        &provider,
        &session,
        &format!(
            "Run exactly this shell command and nothing else, outside the sandbox if it needs that: \
             echo hello > {}",
            target.display()
        ),
    );
    let events = pump_until(&provider, "a card or the end of turn 2", |e| {
        matches!(e, AgentDomainEvent::PermissionRequested { .. }) || is_turn_end(e)
    });
    let card = events.iter().find_map(|e| match e {
        AgentDomainEvent::PermissionRequested {
            permission_id, input, ..
        } => Some((permission_id.clone(), input.clone())),
        _ => None,
    });
    let Some((permission_id, input)) = card else {
        panic!(
            "the model did not ask to run the command outside the sandbox (turn 2 ended without a card): {events:#?}"
        );
    };
    // Only the once-only options are shown: the adapter's "don't ask again" ones never reach the card.
    let kinds: Vec<&str> = input["options"]
        .as_array()
        .map(|options| options.iter().filter_map(|o| o["kind"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        kinds.iter().all(|k| matches!(*k, "allow_once" | "reject_once")),
        "options: {input}"
    );
    assert!(kinds.contains(&"allow_once"), "no way to approve was offered: {input}");

    // Refusing ends the turn through ACP; nothing was written.
    provider
        .resolve_permission(ResolvePermissionRequest {
            session_id: session.clone(),
            permission_id,
            decision: PermissionDecision::Deny { reason: None },
        })
        .expect("the refusal is sent");
    let rest = pump_until(&provider, "turn 2 to end", is_turn_end);
    assert_eq!(turn_outcome(&rest), TurnOutcome::Interrupted, "turn 2: {rest:#?}");
    assert!(!target.exists(), "the refused command wrote {}", target.display());

    provider
        .close_session(CloseSessionRequest { session_id: session })
        .expect("the session closes");
}
