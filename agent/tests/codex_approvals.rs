//! Codex's approval requests in a tab: which become cards, which Eitri answers itself, and what each answer
//! writes. `CodexProvider` runs against the scripted stand-in (`eitri-agent-standin`); the MCP shapes come
//! from recordings of a real `codex app-server` (`record_mcp_approval_shapes`, ignored, run by hand), and
//! the command shapes from the recordings of the earlier measurements.

use agent::providers::codex::{CodexLaunch, CodexProvider};
use agent::{
    AgentDomainEvent, AgentProvider, DirTrust, ExternalProvider, ExternalStart, InterruptTurnRequest,
    PermissionDecision, PermissionOutcome, ProviderError, ResolvePermissionRequest, Reviewer, SendTurnRequest,
    TrustPlan, TurnOutcome, TurnPolicy,
};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const UA: &str = "eitri-probe/0.160.0 (Arch Linux Rolling Release; x86_64) tmux-256color (eitri-probe; 0)";
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/codex");

// ---------------------------------------------------------------- script building

fn exact(text: &str) -> String {
    let mut out = String::from("^");
    for c in text.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('$');
    out
}

fn expect_exact(line: &str) -> Value {
    json!({ "expect": exact(line) })
}

fn expect_request(id: u64, method: &str) -> Value {
    json!({ "expect": format!(r#"^\{{"id":{id},"method":"{method}""#) })
}

fn write(message: Value) -> Value {
    json!({ "write": message.to_string() })
}

fn handshake() -> Vec<Value> {
    vec![
        expect_request(1, "initialize"),
        write(json!({"id": 1, "result": {"userAgent": UA, "codexHome": "/x"}})),
        expect_exact(r#"{"method":"initialized"}"#),
        expect_request(2, "account/read"),
        write(json!({"id": 2, "result": {"account": {"type": "apiKey"}, "requiresOpenaiAuth": true}})),
    ]
}

fn launch() -> CodexLaunch {
    CodexLaunch {
        program: PathBuf::from(env!("CARGO_BIN_EXE_eitri-agent-standin")),
        cwd: std::env::temp_dir(),
        state_home: std::env::temp_dir(),
    }
}

fn policy(reviewer: Reviewer) -> TurnPolicy {
    TurnPolicy { reviewer }
}

/// A started session on `thread` with one turn `turn` running, then `rest`.
fn running(thread: &str, turn: &str, reviewer: Reviewer, rest: Vec<Value>) -> Arc<CodexProvider> {
    let mut script = handshake();
    script.push(expect_request(3, "thread/start"));
    script.push(write(json!({"id": 3, "result": {
        "thread": {"id": thread, "cwd": "/work/proj"}, "model": "gpt-test", "cwd": "/work/proj",
        "approvalPolicy": "on-request",
        "approvalsReviewer": if reviewer == Reviewer::Agent { "auto_review" } else { "user" },
        "sandbox": {"type": "workspaceWrite", "writableRoots": [], "networkAccess": false,
                    "excludeTmpdirEnvVar": false, "excludeSlashTmp": false},
    }})));
    script.push(expect_request(4, "turn/start"));
    script.push(write(json!({"id": 4, "result": {"turn": {
        "id": turn, "items": [], "itemsView": "notLoaded", "status": "inProgress", "error": null,
    }}})));
    script.extend(rest);
    let provider = Arc::new(
        CodexProvider::connect_with_args(launch(), vec![OsString::from(Value::Array(script).to_string())])
            .expect("the handshake passes"),
    );
    let start = ExternalStart {
        cwd: "/work/proj".into(),
        plan: TrustPlan {
            dirs: vec![(PathBuf::from("/work/proj"), DirTrust::Untrusted)],
            skills_off: true,
            fingerprint: "fp".into(),
            project_only: Vec::new(),
        },
        policy: policy(reviewer),
        resume: None,
    };
    let id = provider.start(start).expect("the session starts");
    assert_eq!(id, thread);
    provider
        .send_turn_with(
            SendTurnRequest {
                session_id: thread.to_string(),
                text: "go".into(),
            },
            policy(reviewer),
        )
        .expect("the turn starts");
    provider
}

fn item_started(thread: &str, turn: &str, item: Value) -> Value {
    write(json!({"method": "item/started", "params": {"threadId": thread, "turnId": turn, "item": item}}))
}

fn turn_completed(thread: &str, turn: &str, status: &str) -> Value {
    write(json!({"method": "turn/completed", "params": {
        "threadId": thread,
        "turn": {"id": turn, "items": [], "itemsView": "summary", "status": status, "error": null},
    }}))
}

fn request(id: Value, method: &str, params: Value) -> Value {
    write(json!({"id": id, "method": method, "params": params}))
}

/// The answer line Eitri writes for request `id`: `{"id":<id>,"result":<result>}` exactly.
fn expect_answer(id: Value, result: &str) -> Value {
    expect_exact(&format!(r#"{{"id":{id},"result":{result}}}"#))
}

fn command_item(id: &str, command: &str) -> Value {
    json!({"type": "commandExecution", "id": id, "command": command, "cwd": "/work/proj", "status": "inProgress"})
}

fn file_change_item(id: &str) -> Value {
    json!({"type": "fileChange", "id": id, "status": "inProgress", "changes": [
        {"path": "/work/proj/a.txt", "kind": {"type": "update", "move_path": null}, "diff": "@@ -1 +1 @@\n-old\n+new\n"},
        {"path": "/work/proj/b.txt", "kind": {"type": "add"}, "diff": "fresh\n"},
    ]})
}

// ---------------------------------------------------------------- reading events

fn pump_until(provider: &CodexProvider, done: impl FnMut(&[AgentDomainEvent]) -> bool) -> Vec<AgentDomainEvent> {
    pump_on(provider, Vec::new(), done)
}

/// Pumps until `done` holds over everything read, `seen` included. A wait that follows another goes on from
/// what the first read: one pump can return the events of both (a test thread held up past a stand-in's
/// pause reads them together), and a wait that started empty would never see the second.
fn pump_on(
    provider: &CodexProvider,
    mut seen: Vec<AgentDomainEvent>,
    mut done: impl FnMut(&[AgentDomainEvent]) -> bool,
) -> Vec<AgentDomainEvent> {
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        seen.extend(provider.pump());
        if done(&seen) {
            return seen;
        }
        assert!(Instant::now() < end, "timed out; the events so far: {seen:#?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn until_turn_completed(provider: &CodexProvider) -> Vec<AgentDomainEvent> {
    pump_until(provider, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::TurnCompleted { .. }))
    })
}

/// Pumps until a card is drawn; returns its id, tool name and input, and everything read.
fn until_card(provider: &CodexProvider) -> (String, String, Value, Vec<AgentDomainEvent>) {
    let events = pump_until(provider, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. }))
    });
    let (id, tool, input) = events
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::PermissionRequested {
                permission_id,
                tool_name,
                input,
                ..
            } => Some((permission_id.clone(), tool_name.clone(), input.clone())),
            _ => None,
        })
        .expect("a card");
    (id, tool, input, events)
}

fn answer(provider: &CodexProvider, thread: &str, permission_id: &str, decision: PermissionDecision) {
    provider
        .resolve_permission(ResolvePermissionRequest {
            session_id: thread.to_string(),
            permission_id: permission_id.to_string(),
            decision,
        })
        .expect("the answer is taken");
}

fn cards(events: &[AgentDomainEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. }))
        .count()
}

/// The note Eitri put on a call it answered itself: on the call's own row `item`, or, with `None`, on a row
/// Eitri added because the request had none. Either way the note sits on a row the panel draws.
fn note_on(events: &[AgentDomainEvent], item: Option<&str>) -> String {
    let (row, note) = events
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::PermissionDenied {
                tool_use_id: Some(row),
                reason_type,
                reason,
                ..
            } if item.is_none_or(|item| item == row) => {
                assert_eq!(reason_type.as_deref(), Some("eitri"), "Eitri says it answered");
                Some((row.clone(), reason.clone().expect("a note")))
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no note on {item:?}: {events:#?}"));
    let started = events
        .iter()
        .position(|e| matches!(e, AgentDomainEvent::ToolCallStarted { tool_use_id, .. } if *tool_use_id == row));
    let denied = events
        .iter()
        .position(|e| matches!(e, AgentDomainEvent::PermissionDenied { tool_use_id: Some(r), .. } if *r == row));
    assert!(
        started.is_some() && started < denied,
        "the note sits on a row that exists before it: {events:#?}"
    );
    if item.is_none() {
        assert!(row.starts_with("eitri-answered-"), "a row of Eitri's own: {row}");
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentDomainEvent::ToolCallCompleted { tool_use_id, content, .. }
                    if *tool_use_id == row && content.as_str() == Some("")
            )),
            "Eitri's row ends, with nothing of its own to add: {events:#?}"
        );
        // The note is said once on the row: not again in the row's input or its result.
        let said = events
            .iter()
            .filter(|e| match e {
                AgentDomainEvent::ToolCallStarted { tool_use_id, input, .. } => {
                    *tool_use_id == row && input.to_string().contains(note.as_str())
                }
                AgentDomainEvent::ToolCallCompleted {
                    tool_use_id, content, ..
                } => *tool_use_id == row && content.to_string().contains(note.as_str()),
                AgentDomainEvent::PermissionDenied { tool_use_id, .. } => tool_use_id.as_deref() == Some(row.as_str()),
                _ => false,
            })
            .count();
        assert_eq!(said, 1, "the note is said once: {events:#?}");
    }
    note
}

// ---------------------------------------------------------------- commands

#[test]
fn command_card_carries_exact_command_cwd_reason() {
    let command = "/usr/bin/zsh -lc \"printf 'x' > /outside/as.txt\"";
    let provider = running(
        "th-c1",
        "tu-c1",
        Reviewer::User,
        vec![
            item_started("th-c1", "tu-c1", command_item("exec-1", command)),
            request(
                json!(0),
                "item/commandExecution/requestApproval",
                json!({"kind": "command", "threadId": "th-c1", "turnId": "tu-c1", "itemId": "exec-1",
                       "reason": "May I write outside?", "command": command, "cwd": "/work/proj/sub"}),
            ),
            expect_answer(json!(0), r#"{"decision":"accept"}"#),
            turn_completed("th-c1", "tu-c1", "completed"),
        ],
    );
    let (id, tool, input, events) = until_card(&provider);
    assert_eq!(tool, "commandExecution");
    assert_eq!(
        input["command"], command,
        "the command exactly as Codex runs it, wrapper included"
    );
    assert_eq!(input["cwd"], "/work/proj/sub");
    assert_eq!(input["reason"], "May I write outside?");
    let tool_use = events.iter().find_map(|e| match e {
        AgentDomainEvent::PermissionRequested { tool_use_id, .. } => tool_use_id.clone(),
        _ => None,
    });
    assert_eq!(tool_use.as_deref(), Some("exec-1"), "the card sits on the call's row");
    answer(&provider, "th-c1", &id, PermissionDecision::Allow);
    let events = until_turn_completed(&provider);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentDomainEvent::PermissionResolved { permission_id, outcome: PermissionOutcome::Allowed } if *permission_id == id
    )));
}

#[test]
fn command_without_network_context_is_worded_outside_sandbox() {
    let provider = running(
        "th-c2",
        "tu-c2",
        Reviewer::User,
        vec![
            item_started("th-c2", "tu-c2", command_item("exec-2", "touch /x")),
            request(
                json!(0),
                "item/commandExecution/requestApproval",
                json!({"threadId": "th-c2", "turnId": "tu-c2", "itemId": "exec-2", "command": "touch /x",
                       "cwd": "/work/proj"}),
            ),
        ],
    );
    let (_, _, input, _) = until_card(&provider);
    assert_eq!(input["escalation"], "runs outside the sandbox with your full rights");
    assert!(input["network"].is_null());
    assert!(input["reason"].is_null(), "no reason given, none shown");
}

#[test]
fn network_context_is_worded_with_host() {
    // As Codex sends it: a network request carries an id of its own (the approval's key and a fresh
    // uuid), no command, no cwd, and no item was started for it.
    let net_id = "network#https#example.org#443#7f1c9a52-0d6e-4c7a-9a43-2f8b1e0c5d11";
    let provider = running(
        "th-c3",
        "tu-c3",
        Reviewer::User,
        vec![
            item_started("th-c3", "tu-c3", command_item("exec-3", "curl https://example.org")),
            request(
                json!(0),
                "item/commandExecution/requestApproval",
                json!({"threadId": "th-c3", "turnId": "tu-c3", "itemId": net_id, "startedAtMs": 1,
                       "reason": "network access to example.org was blocked",
                       "networkApprovalContext": {"host": "example.org", "protocol": "https"}}),
            ),
            expect_answer(json!(0), r#"{"decision":"accept"}"#),
            turn_completed("th-c3", "tu-c3", "completed"),
        ],
    );
    let (id, tool, input, events) = until_card(&provider);
    assert_eq!(tool, "commandExecution");
    assert_eq!(
        input["escalation"],
        "asks for network access to example.org (https); may run outside the sandbox with your full rights"
    );
    assert_eq!(input["network"], json!({"host": "example.org", "protocol": "https"}));
    assert!(input["command"].is_null(), "Codex named no command: {input}");
    assert!(input["cwd"].is_null(), "Codex named no cwd: {input}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionDenied { .. })),
        "a network request is a card, not an automatic decline: {events:#?}"
    );
    let on = events.iter().find_map(|e| match e {
        AgentDomainEvent::PermissionRequested { tool_use_id, .. } => tool_use_id.clone(),
        _ => None,
    });
    assert_eq!(on.as_deref(), Some(net_id), "the card is answered under Codex's own id");
    answer(&provider, "th-c3", &id, PermissionDecision::Allow);
    until_turn_completed(&provider);
}

/// A network request names a call no row has (its id is Codex's own key, and no item is started for it).
/// It gets a row of its own before its card, so a note the host puts on it ("allowed in bypass") and the
/// grant itself stay on screen; the row ends with the answer, since Codex never reports that call.
#[test]
fn a_network_request_gets_a_row_that_ends_with_its_answer() {
    for (decision, written, granted) in [
        (PermissionDecision::Allow, r#"{"decision":"accept"}"#, true),
        (
            PermissionDecision::Deny { reason: None },
            r#"{"decision":"decline"}"#,
            false,
        ),
    ] {
        let net_id = "network#https#example.org#443#7f1c9a52-0d6e-4c7a-9a43-2f8b1e0c5d11";
        let provider = running(
            "th-n1",
            "tu-n1",
            Reviewer::User,
            vec![
                item_started("th-n1", "tu-n1", command_item("exec-n", "curl https://example.org")),
                request(
                    json!(0),
                    "item/commandExecution/requestApproval",
                    json!({"threadId": "th-n1", "turnId": "tu-n1", "itemId": net_id, "startedAtMs": 1,
                           "networkApprovalContext": {"host": "example.org", "protocol": "https"}}),
                ),
                expect_answer(json!(0), written),
                turn_completed("th-n1", "tu-n1", "completed"),
            ],
        );
        let (id, _, _, mut events) = until_card(&provider);
        let row = events.iter().position(|e| {
            matches!(e, AgentDomainEvent::ToolCallStarted { tool_use_id, name, input, .. }
                if tool_use_id == net_id && name == "commandExecution" && input["network"]["host"] == "example.org")
        });
        let card = events.iter().position(
            |e| matches!(e, AgentDomainEvent::PermissionRequested { tool_use_id: Some(t), .. } if t == net_id),
        );
        assert!(
            row.is_some() && row < card,
            "a row for the request, before its card: {events:#?}"
        );
        answer(&provider, "th-n1", &id, decision);
        events.extend(until_turn_completed(&provider));
        let ends: Vec<(String, bool)> = events
            .iter()
            .filter_map(|e| match e {
                AgentDomainEvent::ToolCallCompleted {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } if tool_use_id == net_id => Some((content.as_str().unwrap_or_default().to_string(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(ends.len(), 1, "the row ends once: {events:#?}");
        assert_eq!(ends[0].1, !granted, "{ends:?}");
        assert!(ends[0].0.contains("example.org"), "{ends:?}");
    }
}

/// `item/autoApprovalReview/*` as Codex 0.160.0 writes it: the reviewed call, the verdict and the action.
fn review(
    method: &str,
    turn: &str,
    review_id: &str,
    target: Option<&str>,
    status: &str,
    rationale: Option<&str>,
    action: Value,
) -> Value {
    write(json!({"method": method, "params": {
        "threadId": "th-r", "turnId": turn, "startedAtMs": 1, "reviewId": review_id,
        "targetItemId": target, "decisionSource": "agent",
        "review": {"status": status, "riskLevel": "high", "userAuthorization": null, "rationale": rationale},
        "action": action,
    }}))
}

/// In Auto, Codex's own reviewer decides an escalation. Its verdict is a note on the call it reviewed: a
/// denial (or a review that ran out of time, which Codex treats as one) says "blocked", an approval says so
/// too, and a review still under way says nothing yet. A network review names no call, so it gets a row.
#[test]
fn the_reviewers_verdict_is_a_note_on_the_reviewed_call() {
    let started = "item/autoApprovalReview/started";
    let completed = "item/autoApprovalReview/completed";
    let command = |c: &str| json!({"type": "command", "source": "shell", "command": c, "cwd": "/work/proj"});
    let provider = running(
        "th-r",
        "tu-r",
        Reviewer::Agent,
        vec![
            item_started("th-r", "tu-r", command_item("exec-bad", "rm -rf build")),
            review(
                started,
                "tu-r",
                "rv1",
                Some("exec-bad"),
                "inProgress",
                None,
                command("rm -rf build"),
            ),
            review(
                completed,
                "tu-r",
                "rv1",
                Some("exec-bad"),
                "denied",
                Some("deletes the build"),
                command("rm -rf build"),
            ),
            item_started("th-r", "tu-r", command_item("exec-ok", "cargo build")),
            review(
                completed,
                "tu-r",
                "rv2",
                Some("exec-ok"),
                "approved",
                Some("builds the project"),
                command("cargo build"),
            ),
            item_started("th-r", "tu-r", command_item("exec-slow", "make")),
            review(
                completed,
                "tu-r",
                "rv3",
                Some("exec-slow"),
                "timedOut",
                None,
                command("make"),
            ),
            review(
                completed,
                "tu-r",
                "rv4",
                None,
                "approved",
                Some("a package mirror"),
                json!({"type": "networkAccess", "target": "https://example.org", "host": "example.org",
                       "protocol": "https", "port": 443}),
            ),
            turn_completed("th-r", "tu-r", "completed"),
        ],
    );
    let events = until_turn_completed(&provider);
    let notes: Vec<(String, Option<String>, Option<String>)> = events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::PermissionDenied {
                tool_use_id: Some(row),
                reason_type,
                reason,
                ..
            } => Some((row.clone(), reason_type.clone(), reason.clone())),
            _ => None,
        })
        .collect();
    let on = |row: &str| notes.iter().filter(|(r, _, _)| r == row).cloned().collect::<Vec<_>>();
    assert_eq!(
        on("exec-bad"),
        vec![(
            "exec-bad".into(),
            Some("auto_review".into()),
            Some("deletes the build".into())
        )],
        "one note, the verdict; the review under way said nothing: {events:#?}"
    );
    assert_eq!(
        on("exec-ok"),
        vec![(
            "exec-ok".into(),
            Some("auto_review_approved".into()),
            Some("builds the project".into())
        )]
    );
    let slow = on("exec-slow");
    assert_eq!(slow.len(), 1);
    assert_eq!(
        slow[0].1.as_deref(),
        Some("auto_review"),
        "a review out of time blocks the call"
    );
    // The network review: a row of its own, with the host, the verdict and an end.
    let net = events
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::ToolCallStarted { tool_use_id, input, .. }
                if input["network"]["host"] == "example.org" =>
            {
                Some(tool_use_id.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("a row for the network review: {events:#?}"));
    assert_eq!(
        on(&net),
        vec![(
            net.clone(),
            Some("auto_review_approved".into()),
            Some("a package mirror".into())
        )]
    );
    assert!(events.iter().any(
        |e| matches!(e, AgentDomainEvent::ToolCallCompleted { tool_use_id, is_error: false, .. } if *tool_use_id == net)
    ));
}

#[test]
fn a_command_request_naming_no_command_and_no_host_is_declined() {
    let provider = running(
        "th-c5",
        "tu-c5",
        Reviewer::User,
        vec![
            request(
                json!(0),
                "item/commandExecution/requestApproval",
                json!({"threadId": "th-c5", "turnId": "tu-c5", "itemId": "exec-ghost"}),
            ),
            expect_answer(json!(0), r#"{"decision":"decline"}"#),
            turn_completed("th-c5", "tu-c5", "completed"),
        ],
    );
    let events = until_turn_completed(&provider);
    assert_eq!(cards(&events), 0);
    assert_eq!(
        note_on(&events, None),
        "Codex asked to run a command Eitri could not show"
    );
}

#[test]
fn a_request_from_an_earlier_turn_of_the_thread_is_declined() {
    let provider = running(
        "th-c6",
        "tu-c6",
        Reviewer::User,
        vec![
            item_started("th-c6", "tu-c6", command_item("exec-6", "touch /x")),
            request(
                json!(0),
                "item/commandExecution/requestApproval",
                json!({"threadId": "th-c6", "turnId": "tu-earlier", "itemId": "exec-6", "command": "touch /x",
                       "cwd": "/work/proj"}),
            ),
            expect_answer(json!(0), r#"{"decision":"decline"}"#),
            turn_completed("th-c6", "tu-c6", "completed"),
        ],
    );
    let events = until_turn_completed(&provider);
    assert_eq!(cards(&events), 0, "no card for a turn that is not running: {events:#?}");
}

/// The recorded run in which a command outside the root was declined: the answer is `decline` and the turn
/// went on to a reply and its end.
#[test]
fn deny_sends_decline_and_turn_continues() {
    let text = std::fs::read_to_string(format!("{FIXTURES}/as-decline.jsonl")).expect("the recording exists");
    let frames: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).expect("a frame")["msg"].clone())
        .collect();
    let thread = frames
        .iter()
        .find_map(|m| m["result"]["thread"]["id"].as_str())
        .expect("a thread")
        .to_string();
    let turn = frames
        .iter()
        .find_map(|m| m["result"]["turn"]["id"].as_str())
        .expect("a turn")
        .to_string();
    // Everything Codex sent after the turn began, with the recorded answer to the approval replaced by an
    // expectation of Eitri's own.
    let mut rest = Vec::new();
    let mut began = false;
    let mut request_id = None;
    for m in &frames {
        if !began {
            began = m["method"] == "turn/started";
            if !began {
                continue;
            }
        }
        let method = m.get("method").and_then(Value::as_str);
        match (method, m.get("id")) {
            (Some("item/commandExecution/requestApproval"), Some(id)) => {
                request_id = Some(id.clone());
                rest.push(write(m.clone()));
            }
            (None, Some(id)) if Some(id) == request_id.as_ref() && m.get("result").is_some() => {
                rest.push(expect_answer(id.clone(), r#"{"decision":"decline"}"#));
            }
            (Some(_), _) => rest.push(write(m.clone())),
            _ => {}
        }
    }
    assert!(request_id.is_some(), "the recording holds the approval");
    let provider = running(&thread, &turn, Reviewer::User, rest);
    let (id, tool, input, _) = until_card(&provider);
    assert_eq!(tool, "commandExecution");
    assert!(input["command"].as_str().is_some_and(|c| c.contains("outside/as.txt")));
    answer(&provider, &thread, &id, PermissionDecision::Deny { reason: None });
    let events = until_turn_completed(&provider);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentDomainEvent::PermissionResolved {
            outcome: PermissionOutcome::Denied,
            ..
        }
    )));
    let declined_row = events.iter().any(
        |e| matches!(e, AgentDomainEvent::ToolCallCompleted { content, is_error: true, .. } if content == "declined"),
    );
    assert!(declined_row, "the call reads declined: {events:#?}");
    let reply = events
        .iter()
        .any(|e| matches!(e, AgentDomainEvent::ContentDelta { .. }));
    assert!(reply, "the turn went on to a reply after the decline");
}

#[test]
fn interrupt_sends_cancel() {
    let provider = running(
        "th-c4",
        "tu-c4",
        Reviewer::User,
        vec![
            item_started("th-c4", "tu-c4", command_item("exec-4", "touch /x")),
            request(
                json!("r-4"),
                "item/commandExecution/requestApproval",
                json!({"threadId": "th-c4", "turnId": "tu-c4", "itemId": "exec-4", "command": "touch /x"}),
            ),
            // `cancel` ends the turn by itself: no `turn/interrupt` follows it.
            expect_answer(json!("r-4"), r#"{"decision":"cancel"}"#),
            turn_completed("th-c4", "tu-c4", "interrupted"),
            expect_request(5, "thread/unsubscribe"),
            write(json!({"id": 5, "result": {}})),
        ],
    );
    let (id, _, _, _) = until_card(&provider);
    provider
        .interrupt_turn(InterruptTurnRequest {
            session_id: "th-c4".into(),
        })
        .expect("the interrupt is taken");
    let events = until_turn_completed(&provider);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentDomainEvent::PermissionResolved { permission_id, outcome: PermissionOutcome::CancelledByInterrupt }
            if *permission_id == id
    )));
    // The id after `turn/start` is the unsubscribe: had an interrupt been sent, it would have taken id 5.
    provider
        .close_session(agent::CloseSessionRequest {
            session_id: "th-c4".into(),
        })
        .expect("closes");
}

/// Should `cancel` not end the turn after all, pressing interrupt again sends the turn its `turn/interrupt`:
/// the first press took the cancel for the turn's end and sent none.
#[test]
fn a_second_interrupt_reaches_a_turn_a_cancel_did_not_end() {
    let provider = running(
        "th-c5",
        "tu-c5",
        Reviewer::User,
        vec![
            item_started("th-c5", "tu-c5", command_item("exec-5", "touch /x")),
            request(
                json!("r-5"),
                "item/commandExecution/requestApproval",
                json!({"threadId": "th-c5", "turnId": "tu-c5", "itemId": "exec-5", "command": "touch /x"}),
            ),
            expect_answer(json!("r-5"), r#"{"decision":"cancel"}"#),
            expect_exact(r#"{"id":5,"method":"turn/interrupt","params":{"threadId":"th-c5","turnId":"tu-c5"}}"#),
            write(
                json!({"method": "item/completed", "params": {"threadId": "th-c5", "turnId": "tu-c5",
                "item": {"type": "commandExecution", "id": "exec-5", "command": "touch /x", "cwd": "/work/proj",
                         "status": "declined"}}}),
            ),
            turn_completed("th-c5", "tu-c5", "interrupted"),
        ],
    );
    until_card(&provider);
    let interrupt = || {
        provider
            .interrupt_turn(InterruptTurnRequest {
                session_id: "th-c5".into(),
            })
            .expect("the interrupt is taken")
    };
    interrupt();
    std::thread::sleep(std::time::Duration::from_millis(200));
    provider.pump();
    interrupt();
    let events = until_turn_completed(&provider);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentDomainEvent::TurnCompleted {
            outcome: TurnOutcome::Interrupted,
            ..
        }
    )));
}

/// A session that trips with a card waiting cancels the card to Codex, so the turn that asked ends instead
/// of waiting for an answer that never comes.
#[test]
fn a_tripped_session_cancels_the_card_that_waits() {
    let provider = running(
        "th-c6",
        "tu-c6",
        Reviewer::User,
        vec![
            item_started("th-c6", "tu-c6", command_item("exec-6", "touch /x")),
            request(
                json!("r-6"),
                "item/commandExecution/requestApproval",
                json!({"threadId": "th-c6", "turnId": "tu-c6", "itemId": "exec-6", "command": "touch /x"}),
            ),
            json!({"sleep_ms": 300}),
            // A project hook in an untrusted session.
            write(
                json!({"method": "hook/started", "params": {"threadId": "th-c6", "turnId": "tu-c6",
                "run": {"id": "h", "source": "project", "sourcePath": "/work/proj/.codex/hooks.json"}}}),
            ),
            expect_answer(json!("r-6"), r#"{"decision":"cancel"}"#),
            turn_completed("th-c6", "tu-c6", "completed"),
        ],
    );
    let (id, _, _, read) = until_card(&provider);
    let events = pump_on(&provider, read, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PolicyMismatch { .. }))
    });
    assert!(events.iter().any(|e| matches!(
        e,
        AgentDomainEvent::PermissionResolved { permission_id, outcome: PermissionOutcome::ProviderFailed }
            if *permission_id == id
    )));
    // The stand-in reads the answer before it ends the turn; without it the turn never ends.
    pump_on(&provider, events, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::TurnCompleted { .. }))
    });
}

fn write_stdin_declined(reviewer: Reviewer, thread: &str) {
    let provider = running(
        thread,
        "tu-s",
        reviewer,
        vec![
            item_started(thread, "tu-s", command_item("exec-s", "python3")),
            request(
                json!(7),
                "item/commandExecution/requestApproval",
                json!({"kind": "writeStdin", "threadId": thread, "turnId": "tu-s", "itemId": "exec-s"}),
            ),
            expect_answer(json!(7), r#"{"decision":"decline"}"#),
            turn_completed(thread, "tu-s", "completed"),
        ],
    );
    let events = until_turn_completed(&provider);
    assert_eq!(cards(&events), 0, "no card for input Eitri cannot show");
    assert_eq!(
        note_on(&events, Some("exec-s")),
        "Codex asked to type into a running command; Eitri cannot show what it would type"
    );
}

#[test]
fn write_stdin_is_declined_automatically_with_row_note() {
    write_stdin_declined(Reviewer::Agent, "th-s-auto");
    // Bypass answers every card itself; this one never becomes a card, so bypass cannot approve it.
    write_stdin_declined(Reviewer::User, "th-s-bypass");
}

// ---------------------------------------------------------------- file changes

#[test]
fn file_change_card_takes_diff_from_the_item() {
    let provider = running(
        "th-f1",
        "tu-f1",
        Reviewer::User,
        vec![
            item_started("th-f1", "tu-f1", file_change_item("patch-1")),
            request(
                json!(0),
                "item/fileChange/requestApproval",
                json!({"threadId": "th-f1", "turnId": "tu-f1", "itemId": "patch-1", "reason": "needs edits"}),
            ),
            expect_answer(json!(0), r#"{"decision":"accept"}"#),
            turn_completed("th-f1", "tu-f1", "completed"),
        ],
    );
    let (id, tool, input, _) = until_card(&provider);
    assert_eq!(tool, "fileChange");
    assert_eq!(
        input["changes"],
        json!([
            {"path": "/work/proj/a.txt", "kind": "update", "diff": "@@ -1 +1 @@\n-old\n+new\n"},
            {"path": "/work/proj/b.txt", "kind": "add", "diff": "fresh\n"},
        ])
    );
    assert_eq!(input["reason"], "needs edits");
    answer(&provider, "th-f1", &id, PermissionDecision::Allow);
    until_turn_completed(&provider);
}

#[test]
fn unseen_file_change_is_declined() {
    // No item was ever announced for the request.
    let provider = running(
        "th-f2",
        "tu-f2",
        Reviewer::User,
        vec![
            request(
                json!(0),
                "item/fileChange/requestApproval",
                json!({"threadId": "th-f2", "turnId": "tu-f2", "itemId": "patch-ghost"}),
            ),
            expect_answer(json!(0), r#"{"decision":"decline"}"#),
            turn_completed("th-f2", "tu-f2", "completed"),
        ],
    );
    let events = until_turn_completed(&provider);
    assert_eq!(cards(&events), 0);
    assert_eq!(
        note_on(&events, None),
        "Codex asked to change files Eitri could not show"
    );

    // The item was too large to read: the link skipped it, so nothing was held.
    let big = r#"{"method":"item/started","params":{"threadId":"th-f3","turnId":"tu-f3","item":{"type":"fileChange","id":"patch-big","status":"inProgress","changes":[{"path":"/work/proj/a","kind":{"type":"add"},"diff":""#;
    let provider = running(
        "th-f3",
        "tu-f3",
        Reviewer::User,
        vec![
            json!({"write_big": {"prefix": big, "bytes": 17 * 1024 * 1024, "suffix": "\"}]}}}"}}),
            request(
                json!(0),
                "item/fileChange/requestApproval",
                json!({"threadId": "th-f3", "turnId": "tu-f3", "itemId": "patch-big"}),
            ),
            expect_answer(json!(0), r#"{"decision":"decline"}"#),
            turn_completed("th-f3", "tu-f3", "completed"),
        ],
    );
    let events = until_turn_completed(&provider);
    assert_eq!(cards(&events), 0);
    assert_eq!(
        note_on(&events, None),
        "Codex asked to change files Eitri could not show"
    );
}

#[test]
fn a_held_file_change_followed_by_an_unreadable_update_is_declined() {
    // The diff Eitri holds may no longer be the one Codex will apply once a message about the item could
    // not be read.
    let big = r#"{"method":"item/fileChange/patchUpdated","params":{"threadId":"th-f4","turnId":"tu-f4","itemId":"patch-4","diff":""#;
    let provider = running(
        "th-f4",
        "tu-f4",
        Reviewer::User,
        vec![
            item_started("th-f4", "tu-f4", file_change_item("patch-4")),
            json!({"write_big": {"prefix": big, "bytes": 17 * 1024 * 1024, "suffix": "\"}}"}}),
            request(
                json!(0),
                "item/fileChange/requestApproval",
                json!({"threadId": "th-f4", "turnId": "tu-f4", "itemId": "patch-4"}),
            ),
            expect_answer(json!(0), r#"{"decision":"decline"}"#),
            turn_completed("th-f4", "tu-f4", "completed"),
        ],
    );
    let events = until_turn_completed(&provider);
    assert_eq!(cards(&events), 0);
    assert_eq!(
        note_on(&events, Some("patch-4")),
        "Codex asked to change files Eitri could not show"
    );
}

fn grant_root_declined(reviewer: Reviewer, thread: &str) {
    let provider = running(
        thread,
        "tu-g",
        reviewer,
        vec![
            item_started(thread, "tu-g", file_change_item("patch-g")),
            request(
                json!(0),
                "item/fileChange/requestApproval",
                json!({"threadId": thread, "turnId": "tu-g", "itemId": "patch-g", "grantRoot": "/home/u"}),
            ),
            expect_answer(json!(0), r#"{"decision":"decline"}"#),
            turn_completed(thread, "tu-g", "completed"),
        ],
    );
    let events = until_turn_completed(&provider);
    assert_eq!(cards(&events), 0, "a session-wide write grant is never a card");
    assert_eq!(
        note_on(&events, Some("patch-g")),
        "Codex asked to write anywhere under /home/u for the session; Eitri does not grant that"
    );
}

#[test]
fn grant_root_is_declined_in_bypass_too() {
    grant_root_declined(Reviewer::Agent, "th-g-auto");
    grant_root_declined(Reviewer::User, "th-g-bypass");
}

// ---------------------------------------------------------------- other requests

#[test]
fn permissions_request_gets_empty_turn_grant() {
    let provider = running(
        "th-p",
        "tu-p",
        Reviewer::User,
        vec![
            request(
                json!(0),
                "item/permissions/requestApproval",
                json!({"threadId": "th-p", "turnId": "tu-p", "itemId": "exec-p", "cwd": "/work/proj",
                       "permissions": {"network": {"enabled": true}}}),
            ),
            expect_answer(json!(0), r#"{"permissions":{},"scope":"turn"}"#),
            turn_completed("th-p", "tu-p", "completed"),
        ],
    );
    let events = until_turn_completed(&provider);
    assert_eq!(cards(&events), 0);
    assert_eq!(
        note_on(&events, None),
        "Codex asked for extra permissions; Eitri grants none"
    );
}

/// The frames of a recording, as `(dir, message)`.
fn recorded(name: &str) -> Vec<(String, Value)> {
    let text = std::fs::read_to_string(format!("{FIXTURES}/{name}.jsonl")).expect("the recording exists");
    text.lines()
        .map(|l| {
            let frame: Value = serde_json::from_str(l).expect("a frame");
            (frame["dir"].as_str().unwrap_or("").to_string(), frame["msg"].clone())
        })
        .collect()
}

/// Codex's side of a recorded turn, from its `turn/started` to its `turn/completed`, with the recorded
/// answer to the approval request replaced by an expectation of `answer`. Returns the thread, the turn
/// and the steps.
fn recorded_turn(name: &str, approval: &str, answer_json: &str) -> (String, String, Vec<Value>) {
    let frames = recorded(name);
    let mut thread = String::new();
    let mut turn = String::new();
    let mut steps = Vec::new();
    let mut began = false;
    let mut request_id = None;
    for (dir, m) in frames {
        if !began {
            if m["method"] == "turn/started" {
                began = true;
                thread = m["params"]["threadId"].as_str().unwrap_or_default().to_string();
                turn = m["params"]["turn"]["id"].as_str().unwrap_or_default().to_string();
            } else {
                continue;
            }
        }
        let method = m.get("method").and_then(Value::as_str);
        match (dir.as_str(), method, m.get("id")) {
            ("in", Some(method), Some(id)) if method == approval => {
                request_id = Some(id.clone());
                steps.push(write(m.clone()));
            }
            ("out", None, Some(id)) if Some(id) == request_id.as_ref() => {
                steps.push(expect_answer(id.clone(), answer_json));
            }
            ("in", Some(_), _) => steps.push(write(m.clone())),
            _ => {}
        }
        if method == Some("turn/completed") {
            break;
        }
    }
    assert!(request_id.is_some(), "{name} holds a {approval}");
    (thread, turn, steps)
}

#[test]
fn mcp_approval_maps_to_one_time_accept() {
    // The default: the approval arrives as an elicitation form, naming the server and the arguments; the
    // tool is read from the call it belongs to.
    let (thread, turn, steps) = recorded_turn(
        "mcp_approval_elicitation",
        "mcpServer/elicitation/request",
        r#"{"_meta":null,"action":"accept","content":{}}"#,
    );
    let provider = running(&thread, &turn, Reviewer::User, steps);
    let (id, tool, input, events) = until_card(&provider);
    assert_eq!(tool, "mcpToolCall");
    assert_eq!(input["server"], "probe");
    assert_eq!(input["tool"], "probe_write");
    assert_eq!(input["arguments"]["line"], "c1");
    let on_row = events.iter().find_map(|e| match e {
        AgentDomainEvent::PermissionRequested { tool_use_id, .. } => tool_use_id.clone(),
        _ => None,
    });
    assert_eq!(on_row.as_deref(), Some("c1"));
    answer(&provider, &thread, &id, PermissionDecision::Allow);
    let events = until_turn_completed(&provider);
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentDomainEvent::ToolCallCompleted { is_error: false, .. })));

    // With elicitations turned off in the user's configuration, the same approval is a question whose
    // first option is the one-time accept.
    let (thread, turn, steps) = recorded_turn(
        "mcp_approval_user_input",
        "item/tool/requestUserInput",
        r#"{"answers":{"mcp_tool_call_approval_c1":{"answers":["Allow"]}}}"#,
    );
    let provider = running(&thread, &turn, Reviewer::User, steps);
    let (id, tool, input, _) = until_card(&provider);
    assert_eq!(tool, "mcpToolCall");
    assert_eq!(
        (input["server"].as_str(), input["tool"].as_str()),
        (Some("probe"), Some("probe_write"))
    );
    answer(&provider, &thread, &id, PermissionDecision::Allow);
    until_turn_completed(&provider);

    // A deny is the form's decline.
    let (thread, turn, steps) = recorded_turn(
        "mcp_approval_elicitation_decline",
        "mcpServer/elicitation/request",
        r#"{"_meta":null,"action":"decline","content":null}"#,
    );
    let provider = running(&thread, &turn, Reviewer::User, steps);
    let (id, _, _, _) = until_card(&provider);
    answer(&provider, &thread, &id, PermissionDecision::Deny { reason: None });
    until_turn_completed(&provider);
}

#[test]
fn interrupt_on_a_tool_card_declines_then_interrupts() {
    let arguments = json!({"path": "/work/proj/x", "line": "y"});
    let provider = running(
        "th-mi",
        "tu-mi",
        Reviewer::User,
        vec![
            item_started(
                "th-mi",
                "tu-mi",
                json!({"type": "mcpToolCall", "id": "call-mi", "server": "probe", "tool": "probe_write",
                       "status": "inProgress", "arguments": arguments}),
            ),
            request(
                json!(0),
                "mcpServer/elicitation/request",
                json!({"threadId": "th-mi", "turnId": "tu-mi", "serverName": "probe", "mode": "form",
                       "_meta": {"codex_approval_kind": "mcp_tool_call", "tool_params": arguments},
                       "message": "Allow?", "requestedSchema": {"type": "object", "properties": {}}}),
            ),
            // A tool approval has no answer that ends the turn, so the interrupt follows the decline.
            expect_answer(json!(0), r#"{"_meta":null,"action":"decline","content":null}"#),
            expect_request(5, "turn/interrupt"),
            write(json!({"id": 5, "result": {}})),
            turn_completed("th-mi", "tu-mi", "interrupted"),
        ],
    );
    let (id, tool, input, _) = until_card(&provider);
    assert_eq!(
        (tool.as_str(), input["tool"].as_str()),
        ("mcpToolCall", Some("probe_write"))
    );
    provider
        .interrupt_turn(InterruptTurnRequest {
            session_id: "th-mi".into(),
        })
        .expect("the interrupt is taken");
    let events = until_turn_completed(&provider);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentDomainEvent::PermissionResolved { permission_id, outcome: PermissionOutcome::CancelledByInterrupt }
            if *permission_id == id
    )));
}

fn mcp_call(id: &str, tool: &str, arguments: &Value) -> Value {
    json!({"type": "mcpToolCall", "id": id, "server": "probe", "tool": tool, "status": "inProgress",
           "arguments": arguments})
}

fn tool_approval(id: u64, thread: &str, turn: &str, arguments: &Value, message: &str) -> Value {
    request(
        json!(id),
        "mcpServer/elicitation/request",
        json!({"threadId": thread, "turnId": turn, "serverName": "probe", "mode": "form",
               "_meta": {"codex_approval_kind": "mcp_tool_call", "tool_params": arguments},
               "message": message, "requestedSchema": {"type": "object", "properties": {}}}),
    )
}

const TOOL_ACCEPT: &str = r#"{"_meta":null,"action":"accept","content":{}}"#;
const TOOL_DECLINE: &str = r#"{"_meta":null,"action":"decline","content":null}"#;

/// A tool call that could not be read is not held. An approval that names no tool may be for it, so it is
/// not matched to the held call that happens to share its server and arguments.
#[test]
fn an_unnamed_tool_approval_is_declined_on_a_thread_with_a_call_not_held() {
    let arguments = json!({"name": "prod"});
    let provider = running(
        "th-m5",
        "tu-m5",
        Reviewer::User,
        vec![
            item_started("th-m5", "tu-m5", mcp_call("call-read", "read", &arguments)),
            // A tool call whose start does not read (no server): its tool is unknown.
            item_started(
                "th-m5",
                "tu-m5",
                json!({"type": "mcpToolCall", "id": "call-x", "tool": "delete", "status": "inProgress"}),
            ),
            tool_approval(9, "th-m5", "tu-m5", &arguments, "Approve app tool call?"),
            expect_answer(json!(9), TOOL_DECLINE),
            turn_completed("th-m5", "tu-m5", "completed"),
        ],
    );
    let events = until_turn_completed(&provider);
    assert_eq!(
        cards(&events),
        0,
        "the approval was matched to a call it may not be for: {events:#?}"
    );
}

#[test]
fn a_tool_approval_that_could_be_either_of_two_tools_is_declined() {
    // Two calls running in parallel on one server with the same arguments; the approval names neither.
    let arguments = json!({"name": "prod"});
    let provider = running(
        "th-m2",
        "tu-m2",
        Reviewer::User,
        vec![
            item_started("th-m2", "tu-m2", mcp_call("call-status", "status", &arguments)),
            item_started("th-m2", "tu-m2", mcp_call("call-drop", "drop", &arguments)),
            tool_approval(0, "th-m2", "tu-m2", &arguments, "Approve this call?"),
            expect_answer(json!(0), TOOL_DECLINE),
            turn_completed("th-m2", "tu-m2", "completed"),
        ],
    );
    let events = until_turn_completed(&provider);
    assert_eq!(cards(&events), 0, "no card that may name the wrong tool: {events:#?}");
    assert_eq!(note_on(&events, None), "Codex asked to run a tool Eitri could not show");
}

#[test]
fn a_tool_approval_in_codex_wording_is_the_call_of_the_tool_it_names() {
    let arguments = json!({"name": "prod"});
    let provider = running(
        "th-m3",
        "tu-m3",
        Reviewer::User,
        vec![
            item_started("th-m3", "tu-m3", mcp_call("call-status", "status", &arguments)),
            item_started("th-m3", "tu-m3", mcp_call("call-drop", "drop", &arguments)),
            tool_approval(
                0,
                "th-m3",
                "tu-m3",
                &arguments,
                "Allow the probe MCP server to run tool \"drop\"?",
            ),
            expect_answer(json!(0), TOOL_DECLINE),
            turn_completed("th-m3", "tu-m3", "completed"),
        ],
    );
    let (id, _, input, events) = until_card(&provider);
    assert_eq!(input["tool"], "drop");
    let on = events.iter().find_map(|e| match e {
        AgentDomainEvent::PermissionRequested { tool_use_id, .. } => tool_use_id.clone(),
        _ => None,
    });
    assert_eq!(on.as_deref(), Some("call-drop"));
    answer(&provider, "th-m3", &id, PermissionDecision::Deny { reason: None });
    until_turn_completed(&provider);
}

#[test]
fn a_tool_approval_naming_a_tool_no_held_call_runs_is_declined() {
    let arguments = json!({"name": "prod"});
    let provider = running(
        "th-m4",
        "tu-m4",
        Reviewer::User,
        vec![
            item_started("th-m4", "tu-m4", mcp_call("call-status", "status", &arguments)),
            tool_approval(
                0,
                "th-m4",
                "tu-m4",
                &arguments,
                "Allow the probe MCP server to run tool \"drop\"?",
            ),
            expect_answer(json!(0), TOOL_DECLINE),
            turn_completed("th-m4", "tu-m4", "completed"),
        ],
    );
    let events = until_turn_completed(&provider);
    assert_eq!(cards(&events), 0, "{events:#?}");
}

#[test]
fn a_call_already_asked_about_is_not_matched_to_a_later_approval() {
    // The first call is approved and keeps running; the second call, of another tool with the same
    // arguments, then asks. Its card names its own tool, not the first call's.
    let arguments = json!({"name": "prod"});
    let provider = running(
        "th-m5",
        "tu-m5",
        Reviewer::User,
        vec![
            item_started("th-m5", "tu-m5", mcp_call("call-status", "status", &arguments)),
            tool_approval(0, "th-m5", "tu-m5", &arguments, "Approve this call?"),
            expect_answer(json!(0), TOOL_ACCEPT),
            item_started("th-m5", "tu-m5", mcp_call("call-drop", "drop", &arguments)),
            tool_approval(1, "th-m5", "tu-m5", &arguments, "Approve this call?"),
            expect_answer(json!(1), TOOL_DECLINE),
            turn_completed("th-m5", "tu-m5", "completed"),
        ],
    );
    let (first, _, input, _) = until_card(&provider);
    assert_eq!(input["tool"], "status");
    answer(&provider, "th-m5", &first, PermissionDecision::Allow);
    let events = pump_until(&provider, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. }))
    });
    let (second, input, on) = events
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::PermissionRequested {
                permission_id,
                input,
                tool_use_id,
                ..
            } => Some((permission_id.clone(), input.clone(), tool_use_id.clone())),
            _ => None,
        })
        .expect("a second card");
    assert_eq!(input["tool"], "drop", "the card names the call that asked: {input}");
    assert_eq!(on.as_deref(), Some("call-drop"));
    answer(&provider, "th-m5", &second, PermissionDecision::Deny { reason: None });
    until_turn_completed(&provider);
}

#[test]
fn other_elicitation_declined() {
    let provider = running(
        "th-e",
        "tu-e",
        Reviewer::User,
        vec![
            request(
                json!(0),
                "mcpServer/elicitation/request",
                json!({"threadId": "th-e", "turnId": "tu-e", "serverName": "github", "mode": "form",
                       "_meta": null, "message": "Your token?", "requestedSchema": {"type": "object",
                       "properties": {"token": {"type": "string"}}}}),
            ),
            expect_answer(json!(0), r#"{"_meta":null,"action":"decline","content":null}"#),
            turn_completed("th-e", "tu-e", "completed"),
        ],
    );
    let events = until_turn_completed(&provider);
    assert_eq!(cards(&events), 0);
    assert_eq!(
        note_on(&events, None),
        "Codex's tool server github asked for input Eitri cannot show; Eitri declined"
    );
}

#[test]
fn other_user_input_question_unanswered_with_notice() {
    let provider = running(
        "th-q",
        "tu-q",
        Reviewer::User,
        vec![
            request(
                json!(0),
                "item/tool/requestUserInput",
                json!({"threadId": "th-q", "turnId": "tu-q", "itemId": "call-q", "isBlocking": true,
                       "questions": [{"id": "q1", "header": "Pick", "question": "Which one?", "isOther": true,
                                      "isSecret": false, "options": [{"label": "A", "description": "a"}]}]}),
            ),
            expect_answer(json!(0), r#"{"answers":{}}"#),
            turn_completed("th-q", "tu-q", "completed"),
        ],
    );
    let events = until_turn_completed(&provider);
    assert_eq!(cards(&events), 0);
    assert_eq!(note_on(&events, None), "Codex asked a question Eitri cannot show yet");
}

#[test]
fn unknown_server_request_gets_minus_32601() {
    let provider = running(
        "th-u",
        "tu-u",
        Reviewer::User,
        vec![
            request(json!("u-1"), "item/tool/call", json!({"threadId": "th-u", "tool": "x"})),
            expect_exact(r#"{"id":"u-1","error":{"code":-32601,"message":"method not found"}}"#),
            turn_completed("th-u", "tu-u", "completed"),
        ],
    );
    let events = until_turn_completed(&provider);
    assert_eq!(cards(&events), 0);
}

#[test]
fn defer_is_refused_and_an_unknown_card_is_not_found() {
    let provider = running(
        "th-d",
        "tu-d",
        Reviewer::User,
        vec![
            item_started("th-d", "tu-d", command_item("exec-d", "touch /x")),
            request(
                json!(0),
                "item/commandExecution/requestApproval",
                json!({"threadId": "th-d", "turnId": "tu-d", "itemId": "exec-d", "command": "touch /x"}),
            ),
        ],
    );
    let (id, _, _, _) = until_card(&provider);
    let deferred = provider.resolve_permission(ResolvePermissionRequest {
        session_id: "th-d".into(),
        permission_id: id,
        decision: PermissionDecision::Defer,
    });
    assert!(
        matches!(deferred, Err(ProviderError::UnsupportedCapability(_))),
        "{deferred:?}"
    );
    let unknown = provider.resolve_permission(ResolvePermissionRequest {
        session_id: "th-d".into(),
        permission_id: "no-such-card".into(),
        decision: PermissionDecision::Allow,
    });
    assert!(unknown.is_err());
}

#[test]
fn a_card_left_waiting_when_codex_dies_is_resolved_as_failed() {
    let provider = running(
        "th-x",
        "tu-x",
        Reviewer::User,
        vec![
            item_started("th-x", "tu-x", command_item("exec-x", "touch /x")),
            request(
                json!(0),
                "item/commandExecution/requestApproval",
                json!({"threadId": "th-x", "turnId": "tu-x", "itemId": "exec-x", "command": "touch /x"}),
            ),
            json!({"sleep_ms": 300}),
            json!({"exit": 1}),
        ],
    );
    let (id, _, _, read) = until_card(&provider);
    let events = pump_on(&provider, read, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. }))
    });
    assert!(events.iter().any(|e| matches!(
        e,
        AgentDomainEvent::PermissionResolved { permission_id, outcome: PermissionOutcome::ProviderFailed }
            if *permission_id == id
    )));
}

#[test]
fn a_request_for_another_thread_is_declined() {
    let provider = running(
        "th-o",
        "tu-o",
        Reviewer::User,
        vec![
            request(
                json!(0),
                "item/commandExecution/requestApproval",
                json!({"threadId": "th-elsewhere", "turnId": "tu-x", "itemId": "exec-x", "command": "touch /x"}),
            ),
            expect_answer(json!(0), r#"{"decision":"decline"}"#),
            turn_completed("th-o", "tu-o", "completed"),
        ],
    );
    let events = until_turn_completed(&provider);
    assert_eq!(cards(&events), 0);
}

// ---------------------------------------------------------------- recording the MCP shapes

/// Records how a real `codex app-server` asks before a destructive MCP tool runs, three ways: the default
/// (an elicitation form) answered with the one-time accept, the same declined, and with elicitations turned
/// off (a question) answered with the one-time accept. Writes `fixtures/codex/mcp_approval_*.jsonl`.
///
/// No login and no network: a private `CODEX_HOME` points Codex's model provider at a scripted model on
/// 127.0.0.1 that asks for one call of the stand-in's `probe_write` tool, served by the stand-in in its
/// MCP mode.
#[test]
#[ignore = "runs the real codex; run by hand to refresh the recordings"]
fn record_mcp_approval_shapes() {
    recorder::run("mcp_approval_elicitation", recorder::Mode::ElicitationAccept);
    recorder::run("mcp_approval_elicitation_decline", recorder::Mode::ElicitationDecline);
    recorder::run("mcp_approval_user_input", recorder::Mode::QuestionAllow);
}

mod recorder {
    use super::FIXTURES;
    use serde_json::{json, Value};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::sync::mpsc::{channel, Receiver};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    #[derive(Clone, Copy, PartialEq)]
    pub enum Mode {
        ElicitationAccept,
        ElicitationDecline,
        QuestionAllow,
    }

    /// The SSE events of one model answer.
    fn sse(events: &[Value]) -> String {
        events
            .iter()
            .map(|e| format!("event: {}\ndata: {}\n\n", e["type"].as_str().unwrap_or(""), e))
            .collect()
    }

    fn completed(rid: &str) -> Value {
        json!({"type": "response.completed", "response": {"id": rid, "usage": {
            "input_tokens": 1, "input_tokens_details": null, "output_tokens": 1,
            "output_tokens_details": null, "total_tokens": 2}}})
    }

    /// The model's answers: the first asks for the tool, every later one says "done".
    fn answer_for(n: usize, written: &Path) -> Vec<Value> {
        let rid = format!("resp_{n}");
        if n == 1 {
            let args = json!({"path": written.to_string_lossy(), "line": "c1"}).to_string();
            vec![
                json!({"type": "response.created", "response": {"id": rid}}),
                json!({"type": "response.output_item.done", "item": {"type": "function_call", "call_id": "c1",
                       "name": "probe_write", "namespace": "mcp__probe", "arguments": args}}),
                completed(&rid),
            ]
        } else {
            vec![
                json!({"type": "response.created", "response": {"id": rid}}),
                json!({"type": "response.output_item.done", "item": {"type": "message", "role": "assistant",
                       "id": format!("msg_{rid}"), "content": [{"type": "output_text", "text": "done"}]}}),
                completed(&rid),
            ]
        }
    }

    fn serve(stream: TcpStream, counter: Arc<Mutex<usize>>, written: PathBuf) {
        let mut reader = BufReader::new(match stream.try_clone() {
            Ok(s) => s,
            Err(_) => return,
        });
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            return;
        }
        let mut length = 0usize;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).is_err() || header == "\r\n" || header.is_empty() {
                break;
            }
            let lower = header.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("content-length:") {
                length = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; length];
        let _ = reader.read_exact(&mut body);
        let mut stream = stream;
        let path = request_line.split_whitespace().nth(1).unwrap_or("");
        if !(request_line.starts_with("POST") && path.ends_with("/responses")) {
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            return;
        }
        let n = {
            let mut c = counter.lock().unwrap_or_else(|p| p.into_inner());
            *c += 1;
            *c
        };
        let events = sse(&answer_for(n, &written));
        let _ = stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
        );
        let _ = stream.write_all(events.as_bytes());
        let _ = stream.flush();
    }

    /// A scripted Responses API on 127.0.0.1; returns its port. The thread ends with the test process.
    fn scripted_model(written: PathBuf) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a local port");
        let port = listener.local_addr().expect("an address").port();
        let counter = Arc::new(Mutex::new(0usize));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let counter = Arc::clone(&counter);
                let written = written.clone();
                std::thread::spawn(move || serve(stream, counter, written));
            }
        });
        port
    }

    fn codex_on_path() -> PathBuf {
        let path = std::env::var_os("PATH").expect("PATH is set");
        std::env::split_paths(&path)
            .filter(|d| d.is_absolute())
            .map(|d| d.join("codex"))
            .find(|p| p.is_file())
            .expect("codex is on PATH")
    }

    struct Driver {
        child: Child,
        stdin: ChildStdin,
        lines: Receiver<String>,
        log: Vec<Value>,
        next_id: u64,
    }

    fn now() -> f64 {
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        t as f64 / 1000.0
    }

    impl Driver {
        fn send(&mut self, msg: Value) {
            self.log.push(json!({"t": now(), "dir": "out", "msg": msg}));
            writeln!(self.stdin, "{msg}").expect("codex reads its input");
            self.stdin.flush().expect("flushed");
        }

        fn request(&mut self, method: &str, params: Value) -> u64 {
            self.next_id += 1;
            let id = self.next_id;
            self.send(json!({"id": id, "method": method, "params": params}));
            id
        }

        /// Reads until `stop` says so, answering Codex's requests with `answer`.
        fn read_until(
            &mut self,
            within: Duration,
            mut answer: impl FnMut(&Value) -> Value,
            mut stop: impl FnMut(&Value) -> bool,
        ) -> Value {
            let end = Instant::now() + within;
            loop {
                let left = end.saturating_duration_since(Instant::now());
                assert!(!left.is_zero(), "codex did not get there in time; log: {:#?}", self.log);
                let Ok(line) = self.lines.recv_timeout(left) else {
                    panic!("codex stopped early; log: {:#?}", self.log)
                };
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                self.log.push(json!({"t": now(), "dir": "in", "msg": msg.clone()}));
                if msg.get("method").is_some() && msg.get("id").is_some() {
                    let reply = answer(&msg);
                    self.send(reply);
                }
                if stop(&msg) {
                    return msg;
                }
            }
        }

        fn finish(mut self) -> Vec<Value> {
            drop(self.stdin);
            let end = Instant::now() + Duration::from_secs(10);
            while Instant::now() < end {
                if let Ok(Some(_)) = self.child.try_wait() {
                    return self.log;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            // The pid this driver started, never a name.
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.log
        }
    }

    pub fn run(name: &str, mode: Mode) {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("codex-record-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        let proj = root.join("proj");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&proj).expect("project");
        let written = root.join("written.txt");
        let port = scripted_model(written.clone());
        let standin = env!("CARGO_BIN_EXE_eitri-agent-standin");
        let features = if mode == Mode::QuestionAllow {
            "\n[features]\ntool_call_mcp_elicitation = false\n"
        } else {
            ""
        };
        // A model slug Codex does not know gets the plain tool list, where an MCP tool is a namespace the
        // model can call directly.
        let config = format!(
            "model_provider = \"scripted\"\nmodel = \"scripted-model\"\n\n[model_providers.scripted]\n\
             name = \"scripted\"\nbase_url = \"http://127.0.0.1:{port}/v1\"\nwire_api = \"responses\"\n\
             requires_openai_auth = false\n\n[mcp_servers.probe]\ncommand = \"{standin}\"\nargs = [\"--mcp\"]\n\
             {features}"
        );
        std::fs::write(home.join("config.toml"), config).expect("config");

        let mut child = Command::new(codex_on_path())
            .args(["app-server", "--listen", "stdio://"])
            .current_dir(&proj)
            .env("CODEX_HOME", &home)
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEX_API_KEY")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("NVIM")
            .env_remove("NVIM_LISTEN_ADDRESS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("codex starts");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut d = Driver {
            child,
            stdin,
            lines,
            log: Vec::new(),
            next_id: 0,
        };
        let not_found = |m: &Value| json!({"id": m["id"], "error": {"code": -32601, "message": "method not found"}});

        let id = d.request(
            "initialize",
            json!({"clientInfo": {"name": "eitri-probe", "title": "probe", "version": "0"}}),
        );
        d.read_until(Duration::from_secs(30), not_found, |m| m["id"] == json!(id));
        d.send(json!({"method": "initialized"}));
        let proj_text = proj.to_string_lossy().to_string();
        let id = d.request(
            "thread/start",
            json!({"cwd": proj_text, "approvalPolicy": "on-request", "approvalsReviewer": "user",
                   "sandbox": "workspace-write",
                   "config": {"projects": {proj_text.clone(): {"trust_level": "untrusted"}},
                              "sandbox_workspace_write": {"writable_roots": [], "network_access": false,
                                                          "exclude_tmpdir_env_var": false,
                                                          "exclude_slash_tmp": false}}}),
        );
        let started = d.read_until(Duration::from_secs(60), not_found, |m| m["id"] == json!(id));
        let thread = started["result"]["thread"]["id"]
            .as_str()
            .expect("a thread id")
            .to_string();
        let id = d.request(
            "turn/start",
            json!({"threadId": thread, "input": [{"type": "text", "text": "probe"}]}),
        );
        d.read_until(Duration::from_secs(30), not_found, |m| m["id"] == json!(id));
        let mut asked = Vec::new();
        d.read_until(
            Duration::from_secs(60),
            |m| {
                let method = m["method"].as_str().unwrap_or("").to_string();
                asked.push(method.clone());
                match (method.as_str(), mode) {
                    ("mcpServer/elicitation/request", Mode::ElicitationAccept) => {
                        json!({"id": m["id"], "result": {"action": "accept", "content": {}, "_meta": null}})
                    }
                    ("mcpServer/elicitation/request", Mode::ElicitationDecline) => {
                        json!({"id": m["id"], "result": {"action": "decline", "content": null, "_meta": null}})
                    }
                    ("item/tool/requestUserInput", Mode::QuestionAllow) => {
                        let question = m["params"]["questions"][0]["id"].as_str().unwrap_or("").to_string();
                        json!({"id": m["id"], "result": {"answers": {question: {"answers": ["Allow"]}}}})
                    }
                    _ => json!({"id": m["id"], "error": {"code": -32601, "message": "method not found"}}),
                }
            },
            |m| m["method"] == "turn/completed",
        );
        let log = d.finish();

        let expected = match mode {
            Mode::QuestionAllow => "item/tool/requestUserInput",
            _ => "mcpServer/elicitation/request",
        };
        assert_eq!(asked, vec![expected.to_string()], "the one request Codex made");
        let ran = std::fs::read_to_string(&written).unwrap_or_default();
        match mode {
            Mode::ElicitationDecline => assert_eq!(ran, "", "a declined tool does not run"),
            _ => assert_eq!(ran, "c1\n", "an accepted tool runs once"),
        }

        // The scratch paths and the host's identity are this machine's; the recording names a stand-in
        // project and host.
        let roots = [
            std::fs::canonicalize(&root)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
            root.to_string_lossy().to_string(),
        ];
        let mut out = String::new();
        for mut frame in log {
            if frame["msg"]["method"] == "remoteControl/status/changed" {
                frame["msg"]["params"]["serverName"] = json!("host");
                frame["msg"]["params"]["installationId"] = json!("00000000-0000-0000-0000-000000000000");
            }
            let mut text = frame.to_string();
            for r in roots.iter().filter(|r| !r.is_empty()) {
                text = text.replace(r.as_str(), "/work/probe");
            }
            out.push_str(&text);
            out.push('\n');
        }
        std::fs::write(format!("{FIXTURES}/{name}.jsonl"), out).expect("the recording is written");
        let _ = std::fs::remove_dir_all(&root);
    }
}

// ---------------------------------------------------------------- a request too large to read

/// Requests larger than 16 MiB are answered at once by what their method allows, never a grant: a command
/// approval declined, an extra-permissions request granted nothing. The call's row says Eitri answered it,
/// a request that names no call gets a row of its own, and one about a thread outside the tab's family is
/// not shown.
#[test]
fn an_over_cap_request_is_refused_with_a_note_on_its_row() {
    let big = agent::jsonrpc_link::MAX_LINE + 1;
    let provider = running(
        "th-o1",
        "tu-o1",
        Reviewer::User,
        vec![
            item_started("th-o1", "tu-o1", command_item("exec-o1", "touch /x")),
            json!({"write_big": {
                "prefix": r#"{"id":"r-o1","method":"item/commandExecution/requestApproval","params":{"threadId":"th-o1","turnId":"tu-o1","itemId":"exec-o1","command":""#,
                "bytes": big,
                "suffix": r#""}}"#,
            }}),
            expect_answer(json!("r-o1"), r#"{"decision":"decline"}"#),
            json!({"write_big": {
                "prefix": r#"{"id":"r-o2","method":"item/permissions/requestApproval","params":{"x":""#,
                "bytes": big,
                "suffix": r#""}}"#,
            }}),
            expect_answer(json!("r-o2"), r#"{"permissions":{},"scope":"turn"}"#),
            json!({"write_big": {
                "prefix": r#"{"id":"r-o3","method":"item/fileChange/requestApproval","params":{"threadId":"th-elsewhere","itemId":"f-1","x":""#,
                "bytes": big,
                "suffix": r#""}}"#,
            }}),
            expect_answer(json!("r-o3"), r#"{"decision":"decline"}"#),
            json!({"sleep_ms": 200}),
            turn_completed("th-o1", "tu-o1", "completed"),
        ],
    );
    let events = until_turn_completed(&provider);
    let notes: Vec<(Option<String>, String, String)> = events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::PermissionDenied {
                tool_use_id,
                tool_name,
                reason_type: Some(kind),
                reason,
            } if kind == "eitri" => Some((
                tool_use_id.clone(),
                tool_name.clone(),
                reason.clone().unwrap_or_default(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(notes.len(), 2, "{events:#?}");
    assert_eq!(notes[0].0.as_deref(), Some("exec-o1"));
    assert_eq!(notes[0].1, "commandExecution");
    assert!(
        notes[0].2.contains("declined") && notes[0].2.contains("16 MiB"),
        "{}",
        notes[0].2
    );
    assert_eq!(notes[1].1, "permissions");
    assert!(notes[1].2.contains("grants none"), "{}", notes[1].2);
    let added = notes[1].0.clone().expect("a row");
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentDomainEvent::ToolCallStarted { tool_use_id, .. } if *tool_use_id == added
        )),
        "the note sits on no row: {events:#?}"
    );
    assert!(
        !format!("{events:?}").contains("f-1"),
        "a request from outside the family was shown"
    );
}
