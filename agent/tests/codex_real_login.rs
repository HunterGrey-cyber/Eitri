//! The real `codex app-server` on the machine's own Codex login, for what a scripted model cannot show: how a real
//! model behaves under the reviewer, a decline and a tool approval. **These spend real turns.**
//!
//! - Every test is `#[ignore]`d and is run by hand, once, by whoever owns the login:
//!   `cargo test -p agent --test codex_real_login -- --ignored --test-threads=1`. The suite spends at most
//!   three turns (one per test) on the cheapest model the account lists, at the lowest reasoning effort the
//!   models share, and prints the `codex --version` it ran against. It lists models once (no turn).
//!   `EITRI_CODEX_LOGIN_MODEL` names the model instead, and `EITRI_CODEX_LOGIN_EFFORT` the reasoning effort
//!   (default `low`; an empty value is the default), for a model that does not accept `low`.
//! - The login is the machine's default one: this file never reads, copies or prints a credential, never
//!   writes under the account's Codex home (its threads are `ephemeral`), and learns only the *names* of the
//!   account's own tool servers, from the app-server's `config/read`, so that the session can turn each off.
//!   `EITRI_CODEX_LOGIN_MCP` (comma-separated) names more.
//! - Each session is the posture Eitri sends: [`codex_posture::config`], the narrow sandbox, a scratch
//!   project under the test target's directory (never `/tmp`), no network access for commands.
//! - A real model may not do what the prompt asks. A test that sees the model decline to try says so and
//!   fails rather than passing on nothing.

#[path = "support/codex_peer.rs"]
mod codex_peer;
#[path = "support/codex_posture.rs"]
mod codex_posture;

use agent::external::{DirTrust, Reviewer};
use agent::providers::codex::wire::{
    method, parse_notification, ElicitationResponse, ServerNotification, ThreadStartParams, TurnStartParams,
};
use codex_peer::*;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// A real model takes longer than a scripted one.
const REAL_TURN: Duration = Duration::from_secs(240);
/// The lowest reasoning effort every current model accepts.
const DEFAULT_EFFORT: &str = "low";

/// The reasoning effort the sessions ask for: `EITRI_CODEX_LOGIN_EFFORT`, else [`DEFAULT_EFFORT`].
fn effort() -> String {
    effort_from(std::env::var("EITRI_CODEX_LOGIN_EFFORT").ok())
}

fn effort_from(var: Option<String>) -> String {
    var.filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_EFFORT.to_string())
}

#[test]
fn the_effort_is_low_unless_named() {
    assert_eq!(effort_from(None), "low");
    assert_eq!(effort_from(Some(String::new())), "low");
    assert_eq!(effort_from(Some("  ".to_string())), "low");
    assert_eq!(effort_from(Some("minimal".to_string())), "minimal");
}

// ------------------------------------------------------------------------------------ the account

fn login_codex() -> PathBuf {
    let program = agent::providers::codex::locate_codex(None).expect("a codex on PATH (or EITRI_CODEX)");
    let version = Command::new(&program)
        .arg("--version")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|e| format!("unknown ({e})"));
    eprintln!("codex: {version} at {}; the machine's Codex login", program.display());
    program
}

/// The names of the tool servers in an effective configuration as `config/read` answers it: the keys of
/// its `mcp_servers` object, whatever layer (user file, profile, managed file) each came from.
fn tool_server_names(config_answer: &Value) -> Vec<String> {
    config_answer
        .pointer("/config/mcp_servers")
        .and_then(Value::as_object)
        .map(|servers| servers.keys().cloned().collect())
        .unwrap_or_default()
}

/// The account's own tool servers, to be turned off in a test session: what the app-server itself reports
/// for the effective configuration (no working directory, so no project layer), plus any the caller names.
fn account_tool_servers(peer: &mut Peer) -> Vec<String> {
    let answer = peer
        .call("config/read", json!({"includeLayers": false}))
        .expect("config/read answers; without it the account's tool servers cannot be turned off");
    let mut names = tool_server_names(&answer);
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
    eprintln!("account tool servers turned off: {names:?}");
    names
}

#[test]
fn tool_server_names_are_the_keys_of_the_effective_mcp_servers() {
    let answer = json!({"config": {"model": "x", "mcp_servers": {
        "alpha": {"command": "a", "env": {"TOKEN": "secret"}},
        "b.c": {"url": "u"},
    }}});
    assert_eq!(tool_server_names(&answer), vec!["alpha".to_string(), "b.c".to_string()]);
    assert!(tool_server_names(&json!({"config": {"model": "x"}})).is_empty());
    assert!(tool_server_names(&json!({})).is_empty());
}

/// The cheapest model the account lists: a "mini" one. When the account lists none this fails and asks for
/// `EITRI_CODEX_LOGIN_MODEL`, since the account's own default may be its most expensive model.
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

/// A started thread under the posture, with the cheapest model and the lowest effort (or the ones named).
fn login_thread(peer: &mut Peer, project: &Path, trust: DirTrust, reviewer: Reviewer, model: &str) -> Thread {
    let names = account_tool_servers(peer);
    let off: Vec<&str> = names.iter().map(String::as_str).collect();
    let config = codex_posture::config(project, trust, &off);
    let mut params =
        serde_json::to_value(ThreadStartParams::new(text_of(project).to_string(), reviewer, config)).expect("params");
    params["model"] = json!(model);
    params["config"]["model_reasoning_effort"] = json!(effort());
    // The tests leave nothing behind in the account's Codex home.
    params["ephemeral"] = json!(true);
    start_thread(peer, params)
}

fn login_turn(peer: &mut Peer, thread: &str, reviewer: Reviewer, text: &str) -> String {
    let params =
        serde_json::to_value(TurnStartParams::new(thread.to_string(), text.to_string(), reviewer)).expect("params");
    let answer = peer.call(method::TURN_START, params).expect("turn/start");
    let turn = json_str(&answer, "/turn/id").expect("a turn id").to_string();
    peer.wait_turn(thread, &turn, REAL_TURN)
        .unwrap_or_else(|| panic!("the turn did not end in time; stderr tail: {}", peer.stderr_tail()))
}

/// What the model is asked to do: one command that needs to leave the sandbox.
fn escalate_prompt(target: &Path) -> String {
    format!(
        "Run exactly this one shell command outside the sandbox: ask for escalated permissions \
         (sandbox_permissions set to require_escalated, with a one-line justification). Do nothing else. \
         When the command has run or been refused, reply with the single word done.\n\ntouch {}",
        text_of(target)
    )
}

/// Whether the model tried the escalation at all: an approval request arrived, or a reviewer row did.
fn model_tried(peer: &Peer) -> bool {
    peer.requests.iter().any(|r| r.method == method::COMMAND_APPROVAL)
        || peer.notes.iter().any(|n| n.method == method::AUTO_REVIEW_STARTED)
}

// ------------------------------------------------------------------------------------ the checks

/// Under the agent's own reviewer (the tab's Auto mode) a command that asks to leave the sandbox is
/// judged by Codex's reviewer, and the judgement arrives as `item/autoApprovalReview/*` rows that parse.
#[test]
#[ignore = "spends one real turn on the machine's Codex login; run by hand, once"]
fn reviewer_rows_appear_in_auto() {
    let codex = login_codex();
    let s = Scratch::new("login-auto");
    let outside = s.dir("outside");
    let project = s.dir("proj");
    write_file(&project.join("README.md"), "probe\n");
    let target = outside.join("auto-touch");

    // A request that reaches the client in Auto is not expected; if one does, it is declined.
    let mut peer = Peer::start_on_login(&codex, &project, decline_all());
    let model = cheapest_model(&mut peer);
    eprintln!("model: {model}, effort {}", effort());
    let thread = login_thread(&mut peer, &project, DirTrust::Untrusted, Reviewer::Agent, &model);
    let status = login_turn(&mut peer, &thread.id, Reviewer::Agent, &escalate_prompt(&target));
    peer.pump_for(Duration::from_secs(1));
    eprintln!("turn {status}; target created: {}", target.exists());

    let started = peer.note(method::AUTO_REVIEW_STARTED).cloned();
    let completed = peer.note(method::AUTO_REVIEW_COMPLETED).cloned();
    peer.close();
    let (Some(started), Some(completed)) = (started, completed) else {
        panic!(
            "no reviewer rows arrived (the model may not have tried the escalation): {:?}",
            s.0
        );
    };
    match parse_notification(method::AUTO_REVIEW_STARTED, started.params) {
        ServerNotification::AutoReviewStarted(row) => {
            assert_eq!(row.thread_id, thread.id);
            eprintln!("review started: {:?}", row.review.status);
        }
        other => panic!("the started row did not parse: {other:?}"),
    }
    match parse_notification(method::AUTO_REVIEW_COMPLETED, completed.params) {
        ServerNotification::AutoReviewCompleted(row) => {
            eprintln!(
                "review completed: {:?}, risk {:?}",
                row.review.status, row.review.risk_level
            );
        }
        other => panic!("the completed row did not parse: {other:?}"),
    }
}

/// A declined command is not run and the model, told so, carries on to an answer of its own.
#[test]
#[ignore = "spends one real turn on the machine's Codex login; run by hand, once"]
fn decline_continues_on_a_real_model() {
    let codex = login_codex();
    let s = Scratch::new("login-decline");
    let outside = s.dir("outside");
    let project = s.dir("proj");
    write_file(&project.join("README.md"), "probe\n");
    let target = outside.join("declined-touch");

    let mut peer = Peer::start_on_login(&codex, &project, decline_all());
    let model = cheapest_model(&mut peer);
    eprintln!("model: {model}, effort {}", effort());
    let thread = login_thread(&mut peer, &project, DirTrust::Untrusted, Reviewer::User, &model);
    let status = login_turn(&mut peer, &thread.id, Reviewer::User, &escalate_prompt(&target));
    peer.pump_for(Duration::from_secs(1));
    assert!(
        model_tried(&peer),
        "the model never asked to leave the sandbox, so nothing was declined: {:?}",
        s.0
    );
    let request_at = peer
        .requests
        .iter()
        .find(|r| r.method == method::COMMAND_APPROVAL)
        .map(|r| r.at)
        .expect("an approval request");
    let said_after = peer.notes.iter().any(|n| {
        n.method == method::ITEM_COMPLETED
            && n.at > request_at
            && json_str(&n.params, "/item/type") == Some("agentMessage")
    });
    let created = target.exists();
    peer.close();

    assert!(!created, "a declined command ran");
    assert_eq!(
        status, "completed",
        "a declined command should leave the turn to finish"
    );
    assert!(said_after, "the model said nothing after the decline");
}

/// A tool of a local tool server that is not read-only reaches the client as a form for that server; accepting
/// it once runs the tool and the turn finishes.
#[test]
#[ignore = "spends one real turn on the machine's Codex login; run by hand, once"]
fn mcp_approval_card_with_a_local_server() {
    let codex = login_codex();
    let s = Scratch::new("login-mcp");
    let project = s.dir("proj");
    let pid_file = s.path("tool-server.pid");
    let _guard = KillOnDrop(pid_file.clone());
    // The server is the project's own, so the project must be trusted for it to start.
    write_project(&project, &local_tool_server(&s, "eitri_local", &pid_file, false));

    let answer: Answer = Box::new(|req| {
        if req.method == method::MCP_ELICITATION {
            Some(serde_json::to_value(ElicitationResponse::accept_once()).expect("json"))
        } else {
            Some(json!({"decision": "decline"}))
        }
    });
    let mut peer = Peer::start_on_login(&codex, &project, answer);
    let model = cheapest_model(&mut peer);
    eprintln!("model: {model}, effort {}", effort());
    let thread = login_thread(&mut peer, &project, DirTrust::Trusted, Reviewer::User, &model);
    let status = login_turn(
        &mut peer,
        &thread.id,
        Reviewer::User,
        "Call the `ping` tool of the `eitri_local` tool server exactly once, with no arguments, then reply with \
         what it returned.",
    );
    peer.pump_for(Duration::from_secs(1));
    let forms: Vec<Req> = peer
        .requests
        .iter()
        .filter(|r| r.method == method::MCP_ELICITATION)
        .cloned()
        .collect();
    let called = peer.notes.iter().any(|n| {
        n.method == method::ITEM_COMPLETED
            && json_str(&n.params, "/item/type") == Some("mcpToolCall")
            && json_str(&n.params, "/item/status") == Some("completed")
    });
    peer.close();

    assert!(
        !forms.is_empty(),
        "no tool approval form arrived (the model may not have called the tool): {:?}",
        s.0
    );
    assert_eq!(json_str(&forms[0].params, "/serverName"), Some("eitri_local"));
    assert_eq!(json_str(&forms[0].params, "/mode"), Some("form"));
    assert!(
        json_str(&forms[0].params, "/message").is_some_and(|m| m.contains("ping")),
        "the form does not name the tool: {}",
        forms[0].params
    );
    assert!(called, "the accepted tool call did not complete");
    assert_eq!(status, "completed");
}
