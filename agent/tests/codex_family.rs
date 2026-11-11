//! A Codex tab's family: the subagents its thread spawns, the tab turn that lasts as long as any of them
//! runs, the interrupt that reaches every running turn, and the close that stops them before the pipe
//! closes. The scripts are the traffic of the subagent measurements (a child that outlives its parent's
//! turn, a grandchild announced on the child's thread, an interrupt with two turns running), replayed from
//! the recordings where one has the shape and built by hand where none does. No real Codex runs here and no
//! model turn is spent.

use agent::providers::codex::translate::row_id;
use agent::providers::codex::{CodexLaunch, CodexProvider};
use agent::{
    AgentDomainEvent, AgentProvider, CloseSessionRequest, ContentKind, DirTrust, ExternalProvider, ExternalStart,
    InterruptTurnRequest, PermissionDecision, PermissionOutcome, ProviderError, ResolvePermissionRequest, Reviewer,
    SendTurnRequest, TrustPlan, TurnOutcome, TurnPolicy,
};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[path = "support/script_gate.rs"]
mod script_gate;
use script_gate::{until, ScriptGate};

const UA: &str = "eitri-probe/0.160.0 (Arch Linux Rolling Release; x86_64) tmux-256color (eitri-probe; 0)";
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/codex");

// ---------------------------------------------------------------- script building

/// A regular expression that matches exactly `text`.
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

/// A line that starts with `{"id":<id>,"method":"<method>"`.
fn expect_request(id: u64, method: &str) -> Value {
    json!({ "expect": format!(r#"^\{{"id":{id},"method":"{method}""#) })
}

fn expect_interrupt(id: u64, thread: &str, turn: &str) -> Value {
    expect_exact(&format!(
        r#"{{"id":{id},"method":"turn/interrupt","params":{{"threadId":"{thread}","turnId":"{turn}"}}}}"#
    ))
}

fn expect_answer(id: Value, result: &str) -> Value {
    expect_exact(&format!(r#"{{"id":{id},"result":{result}}}"#))
}

fn write(message: Value) -> Value {
    json!({ "write": message.to_string() })
}

fn handshake() -> Vec<Value> {
    vec![
        expect_exact(&format!(
            r#"{{"id":1,"method":"initialize","params":{{"clientInfo":{{"name":"eitri","title":"Eitri","version":"{}"}}}}}}"#,
            env!("CARGO_PKG_VERSION")
        )),
        write(json!({"id": 1, "result": {"userAgent": UA, "codexHome": "/x"}})),
        expect_exact(r#"{"method":"initialized"}"#),
        expect_exact(r#"{"id":2,"method":"account/read","params":{"refreshToken":false}}"#),
        write(
            json!({"id": 2, "result": {"account": {"type": "chatgpt", "email": "someone@example.com", "planType": "pro"},
                                          "requiresOpenaiAuth": true}}),
        ),
    ]
}

fn launch() -> CodexLaunch {
    CodexLaunch {
        program: PathBuf::from(env!("CARGO_BIN_EXE_eitri-agent-standin")),
        cwd: std::env::temp_dir(),
        state_home: std::env::temp_dir(),
    }
}

fn connect(script: Vec<Value>) -> CodexProvider {
    CodexProvider::connect_with_args(launch(), vec![OsString::from(Value::Array(script).to_string())])
        .expect("the handshake passes")
}

fn start_request() -> ExternalStart {
    ExternalStart {
        cwd: "/work/proj".into(),
        plan: TrustPlan {
            dirs: vec![(PathBuf::from("/work/proj"), DirTrust::Untrusted)],
            skills_off: true,
            fingerprint: "fp".into(),
            project_only: Vec::new(),
        },
        policy: TurnPolicy {
            reviewer: Reviewer::User,
            sandbox: agent::Sandbox::Project,
        },
        resume: None,
        editor_server: None,
    }
}

fn thread_response_for(id: u64, thread: &str, reviewer: &str) -> Value {
    write(json!({"id": id, "result": {
        "thread": {"id": thread, "cwd": "/work/proj"},
        "model": "gpt-test",
        "cwd": "/work/proj",
        "approvalPolicy": "on-request",
        "approvalsReviewer": reviewer,
        "sandbox": {"type": "workspaceWrite", "writableRoots": [], "networkAccess": false,
                    "excludeTmpdirEnvVar": false, "excludeSlashTmp": false},
    }}))
}

fn thread_response(id: u64, thread: &str) -> Value {
    thread_response_for(id, thread, "user")
}

fn turn_response(id: u64, turn: &str) -> Value {
    write(json!({"id": id, "result": {"turn": {
        "id": turn, "items": [], "itemsView": "notLoaded", "status": "inProgress", "error": null,
    }}}))
}

fn turn_notice(method: &str, thread: &str, turn: &str, status: &str) -> Value {
    write(json!({"method": method, "params": {
        "threadId": thread,
        "turn": {"id": turn, "items": [], "itemsView": "notLoaded", "status": status, "error": null},
    }}))
}

fn turn_started(thread: &str, turn: &str) -> Value {
    turn_notice("turn/started", thread, turn, "inProgress")
}

fn turn_completed(thread: &str, turn: &str, status: &str) -> Value {
    turn_notice("turn/completed", thread, turn, status)
}

fn item_event(method: &str, thread: &str, turn: &str, item: Value) -> Value {
    write(json!({"method": method, "params": {"item": item, "threadId": thread, "turnId": turn}}))
}

fn announce(on_thread: &str, on_turn: &str, child: &str, path: &str) -> Value {
    item_event(
        "item/started",
        on_thread,
        on_turn,
        json!({"type": "subAgentActivity", "id": format!("sp-{child}"), "kind": "started",
               "agentThreadId": child, "agentPath": path}),
    )
}

fn command_item(id: &str, command: &str, status: &str) -> Value {
    json!({"type": "commandExecution", "id": id, "command": command, "cwd": "/work/proj", "status": status})
}

fn command_request(request: u64, thread: &str, turn: &str, item: &str, command: &str) -> Value {
    write(
        json!({"id": request, "method": "item/commandExecution/requestApproval", "params": {
            "kind": "command", "threadId": thread, "turnId": turn, "itemId": item,
            "reason": "outside the sandbox", "command": command, "cwd": "/work/proj",
        }}),
    )
}

/// A session: the handshake, the `thread/start` answer, the first `turn/start` answer, then `rest`.
fn running(thread: &str, turn: &str, rest: Vec<Value>) -> (CodexProvider, String) {
    let mut script = handshake();
    script.push(expect_request(3, "thread/start"));
    script.push(thread_response(3, thread));
    script.push(expect_request(4, "turn/start"));
    script.push(turn_response(4, turn));
    script.extend(rest);
    let provider = connect(script);
    assert_eq!(provider.start(start_request()).expect("the session starts"), thread);
    assert_eq!(send(&provider, thread, "go").expect("the turn starts"), turn);
    (provider, thread.to_string())
}

fn send(provider: &CodexProvider, thread: &str, text: &str) -> Result<String, ProviderError> {
    provider.send_turn_with(
        SendTurnRequest {
            session_id: thread.to_string(),
            text: text.to_string(),
        },
        TurnPolicy {
            reviewer: Reviewer::User,
            sandbox: agent::Sandbox::Project,
        },
    )
}

// ---------------------------------------------------------------- reading events

/// Pumps until `done` is true of everything read, or fails after 20 s.
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

fn is_completed(e: &AgentDomainEvent) -> bool {
    matches!(e, AgentDomainEvent::TurnCompleted { .. })
}

fn is_started(e: &AgentDomainEvent) -> bool {
    matches!(e, AgentDomainEvent::TurnStarted { .. })
}

fn until_turn_completed(provider: &CodexProvider) -> Vec<AgentDomainEvent> {
    pump_until(provider, |events| events.iter().any(is_completed))
}

fn position(events: &[AgentDomainEvent], want: impl Fn(&AgentDomainEvent) -> bool) -> usize {
    events
        .iter()
        .position(want)
        .unwrap_or_else(|| panic!("no such event in {events:#?}"))
}

fn tool_started(id: impl Into<String>) -> impl Fn(&AgentDomainEvent) -> bool {
    let id = id.into();
    move |e| matches!(e, AgentDomainEvent::ToolCallStarted { tool_use_id, .. } if *tool_use_id == id)
}

fn tool_completed(id: impl Into<String>) -> impl Fn(&AgentDomainEvent) -> bool {
    let id = id.into();
    move |e| matches!(e, AgentDomainEvent::ToolCallCompleted { tool_use_id, .. } if *tool_use_id == id)
}

/// The row id of item `item` on whichever thread ran it, read off the row that started.
fn row_named(events: &[AgentDomainEvent], item: &str) -> String {
    let suffix = format!(":{item}");
    events
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::ToolCallStarted { tool_use_id, .. }
                if tool_use_id == item || tool_use_id.ends_with(&suffix) =>
            {
                Some(tool_use_id.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no row for {item} in {events:#?}"))
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only asks whether the pid exists.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn interrupt(provider: &CodexProvider, thread: &str) {
    provider
        .interrupt_turn(InterruptTurnRequest {
            session_id: thread.to_string(),
        })
        .expect("the interrupt is taken");
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

fn card_of(events: &[AgentDomainEvent]) -> (String, Value) {
    events
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::PermissionRequested {
                permission_id, input, ..
            } => Some((permission_id.clone(), input.clone())),
            _ => None,
        })
        .expect("a card")
}

fn input_of<'a>(events: &'a [AgentDomainEvent], tool: &str) -> &'a Value {
    events
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::ToolCallStarted { tool_use_id, input, .. } if tool_use_id == tool => Some(input),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no row for {tool} in {events:#?}"))
}

// ---------------------------------------------------------------- the recording of a late child

/// `m4-r2-a` as stand-in steps after its handshake: the parent's turn ends at once, its child writes a file
/// five seconds later and announces a grandchild, and the grandchild asks to run a command outside the
/// sandbox. `pause_after_parent_ms` stands for the time the recording shows between the parent's end and
/// the child's first command, so a test can look at the tab between the two.
fn late_child_script(pause_after_parent_ms: u64) -> (Vec<Value>, String, String) {
    replay_script("m4-r2-a.jsonl", pause_after_parent_ms)
}

/// A recording of the probe as stand-in steps after the handshake, with the ids of Eitri's own requests
/// moved past `account/read` and a pause after the parent's turn ends. Returns the steps, the tab's thread
/// and the tab's first turn.
fn replay_script(fixture: &str, pause_after_parent_ms: u64) -> (Vec<Value>, String, String) {
    let text = std::fs::read_to_string(format!("{FIXTURES}/{fixture}")).expect("the recording exists");
    let shift = |id: u64| if id >= 2 { id + 1 } else { id };
    let mut steps = handshake();
    let (mut thread, mut turn) = (String::new(), String::new());
    let mut started = false;
    let mut parent_done = false;
    for line in text.lines() {
        let frame: Value = serde_json::from_str(line).expect("a recorded frame");
        let message = frame["msg"].clone();
        let outgoing = frame["dir"] == "out";
        let method = message.get("method").and_then(Value::as_str);
        if !started {
            if outgoing && method == Some("thread/start") {
                started = true;
            } else {
                continue;
            }
        }
        if thread.is_empty() {
            if let Some(id) = message["result"]["thread"]["id"].as_str() {
                thread = id.to_string();
            }
        } else if turn.is_empty() {
            if let Some(id) = message["result"]["turn"]["id"].as_str() {
                turn = id.to_string();
            }
        }
        match (outgoing, method, message.get("id").and_then(Value::as_u64)) {
            (true, Some(method), Some(id)) => steps.push(expect_request(shift(id), method)),
            (true, Some(method), None) => steps.push(json!({"expect": format!(r#"^\{{"method":"{method}""#)})),
            // Eitri's answer to a request Codex made: the id is Codex's own and is not shifted.
            (true, None, Some(id)) => steps.push(expect_answer(json!(id), &message["result"].to_string())),
            (true, None, None) => panic!("an outgoing frame with neither method nor id"),
            (false, Some(_), Some(_)) => steps.push(write(message)),
            (false, _, Some(id)) => {
                let mut response = message;
                response["id"] = json!(shift(id));
                steps.push(write(response));
            }
            (false, _, None) => {
                let ends_parent = method == Some("turn/completed") && message["params"]["threadId"] == json!(thread);
                steps.push(write(message));
                if ends_parent && !parent_done {
                    parent_done = true;
                    steps.push(json!({"sleep_ms": pause_after_parent_ms}));
                }
            }
        }
    }
    (steps, thread, turn)
}

fn replay_late_child(pause_ms: u64) -> (CodexProvider, String, String) {
    let (script, thread, turn) = late_child_script(pause_ms);
    let provider = connect(script);
    assert_eq!(provider.start(start_request()).unwrap(), thread);
    assert_eq!(send(&provider, &thread, "PARENT-PROMPT").unwrap(), turn);
    (provider, thread, turn)
}

#[test]
fn tab_turn_waits_for_the_child() {
    let (provider, thread, turn) = replay_late_child(500);
    // The parent's own turn ends within milliseconds; its child is still to write. The grandchild's approval
    // is the last thing that holds the family: nothing read up to that card, however late it is read, may
    // end the tab's turn.
    let before_card = pump_until(&provider, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. }))
    });
    assert!(
        before_card.iter().any(
            |e| matches!(e, AgentDomainEvent::ContentDelta { kind: ContentKind::Text, text, .. } if text == "done")
        ),
        "the parent's reply was read: {before_card:#?}"
    );
    assert!(
        !before_card.iter().any(is_completed),
        "the tab's turn ended before the family: {before_card:#?}"
    );
    let (id, input) = card_of(&before_card);
    assert_eq!(input["subagent"], "/root/child/grand");
    answer(&provider, &thread, &id, PermissionDecision::Deny { reason: None });
    let events = pump_on(&provider, before_card, |events| events.iter().any(is_completed));
    assert_eq!(events.iter().filter(|e| is_started(e)).count(), 1, "one tab turn");
    assert_eq!(events.iter().filter(|e| is_completed(e)).count(), 1, "one tab turn");
    match &events[position(&events, is_completed)] {
        AgentDomainEvent::TurnCompleted {
            turn_id,
            outcome,
            result_text,
            ..
        } => {
            assert_eq!(turn_id, &turn);
            assert_eq!(*outcome, TurnOutcome::Completed);
            assert_eq!(result_text, "done");
        }
        other => panic!("{other:?}"),
    }
    // The child's reply is the child's own: the tab's text is the parent's alone.
    let replies = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentDomainEvent::ContentDelta {
                    kind: ContentKind::Text,
                    ..
                }
            )
        })
        .count();
    assert_eq!(replies, 1, "{events:#?}");
}

#[test]
fn child_late_write_lands_inside_the_tab_turn() {
    let (provider, thread, turn) = replay_late_child(50);
    let (id, events) = {
        let events = pump_until(&provider, |events| {
            events
                .iter()
                .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. }))
        });
        (card_of(&events).0, events)
    };
    answer(&provider, &thread, &id, PermissionDecision::Deny { reason: None });
    let mut events = events;
    events.extend(until_turn_completed(&provider));

    let started = position(&events, is_started);
    let cs1 = row_named(&events, "cs1");
    let write_started = position(&events, tool_started(cs1.clone()));
    let write_done = position(&events, tool_completed(cs1.clone()));
    let completed = position(&events, is_completed);
    assert!(
        started < write_started && write_started < write_done && write_done < completed,
        "the child's write falls between the tab's start and end: {events:#?}"
    );
    // Every row of the family is filed under the tab's turn, whichever thread ran it.
    for event in &events {
        match event {
            AgentDomainEvent::ToolCallStarted { turn_id, .. } | AgentDomainEvent::ToolCallCompleted { turn_id, .. } => {
                assert_eq!(turn_id, &turn)
            }
            _ => {}
        }
    }
    assert_eq!(input_of(&events, &cs1)["subagent"], "/root/child");
    assert_eq!(
        input_of(&events, &row_named(&events, "gc1"))["subagent"],
        "/root/child/grand"
    );
}

// ---------------------------------------------------------------- announcements

#[test]
fn announcement_only_from_member_threads() {
    let (provider, _) = running(
        "tab",
        "t1",
        vec![
            // A child of the tab, and a grandchild announced on the child's thread.
            announce("tab", "t1", "child", "/root/child"),
            turn_started("child", "c1"),
            announce("child", "c1", "grand", "/root/child/grand"),
            turn_started("grand", "g1"),
            // A thread nobody announced announces a thread of its own.
            announce("stranger", "s1", "ghost", "/root/ghost"),
            turn_started("stranger", "s1"),
            turn_started("ghost", "x1"),
            item_event(
                "item/started",
                "grand",
                "g1",
                command_item("grand-cmd", "echo grand", "inProgress"),
            ),
            item_event(
                "item/started",
                "ghost",
                "x1",
                command_item("ghost-cmd", "echo ghost", "inProgress"),
            ),
            item_event(
                "item/started",
                "stranger",
                "s1",
                command_item("stranger-cmd", "echo stranger", "inProgress"),
            ),
            turn_completed("grand", "g1", "completed"),
            turn_completed("child", "c1", "completed"),
            turn_completed("tab", "t1", "completed"),
        ],
    );
    let events = until_turn_completed(&provider);
    assert_eq!(
        input_of(&events, &row_id("tab", "grand", "grand-cmd"))["subagent"],
        "/root/child/grand",
        "a grandchild announced on its parent's thread is a member"
    );
    for (thread, foreign) in [("ghost", "ghost-cmd"), ("stranger", "stranger-cmd")] {
        assert!(
            !events.iter().any(tool_started(row_id("tab", thread, foreign))),
            "{foreign} belongs to no thread of the family: {events:#?}"
        );
    }
}

#[test]
fn a_completed_announcement_adds_no_member() {
    let (provider, _) = running(
        "tab",
        "t1",
        vec![
            item_event(
                "item/started",
                "tab",
                "t1",
                json!({"type": "subAgentActivity", "id": "done-1", "kind": "completed",
                       "agentThreadId": "late", "agentPath": "/root/late"}),
            ),
            turn_started("late", "l1"),
            item_event(
                "item/started",
                "late",
                "l1",
                command_item("late-cmd", "echo late", "inProgress"),
            ),
            turn_completed("tab", "t1", "completed"),
        ],
    );
    let events = until_turn_completed(&provider);
    assert!(
        !events.iter().any(tool_started(row_id("tab", "late", "late-cmd"))),
        "{events:#?}"
    );
}

/// A spawn as Codex 0.161.0 reports it under a model with the plain tool set: a `collabAgentToolCall` whose
/// end names the child (`receiverThreadIds`), and no `subAgentActivity` at all. Replayed from the recording:
/// the parent ends its turn while the child runs a command and asks to leave the sandbox. The child is a
/// member: its row and card are shown and say whose they are, and the tab's turn waits for it.
#[test]
fn a_child_announced_by_a_collab_call_joins_the_family() {
    let (script, thread, turn) = replay_script("t8-i11g-plain-spawn.jsonl", 300);
    let provider = connect(script);
    assert_eq!(provider.start(start_request()).unwrap(), thread);
    assert_eq!(send(&provider, &thread, "PARENT-PROMPT").unwrap(), turn);
    let events = pump_until(&provider, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. }))
    });
    assert!(
        !events.iter().any(is_completed),
        "the tab's turn ended with the parent's while the child ran: {events:#?}"
    );
    let (id, input) = card_of(&events);
    let label = input["subagent"].as_str().unwrap_or_default().to_string();
    assert!(label.starts_with("/root/"), "the card names the subagent: {input}");
    answer(&provider, &thread, &id, PermissionDecision::Deny { reason: None });
    let mut events = events;
    events.extend(until_turn_completed(&provider));
    let cc2 = row_named(&events, "cc2");
    assert_ne!(cc2, "cc2", "the child's call has a row of its own");
    assert_eq!(input_of(&events, &cc2)["subagent"], label.as_str());
    let (_, outcome, _) = ended(&events);
    assert_eq!(outcome, TurnOutcome::Completed);
    assert_eq!(events.iter().filter(|e| is_started(e)).count(), 1, "one tab turn");
}

/// A collab call's receivers join only when the call is on a member thread, and a child whose turn started
/// before the call that names it was read is still followed from that turn.
#[test]
fn collab_receivers_join_from_members_only_and_an_early_turn_is_kept() {
    let collab = |id: &str, receivers: Vec<&str>| {
        json!({"type": "collabAgentToolCall", "id": id, "tool": "spawnAgent", "status": "completed",
               "senderThreadId": "tab", "receiverThreadIds": receivers, "prompt": "go"})
    };
    let (provider, thread) = running(
        "tab",
        "t1",
        vec![
            // The child's turn is under way before the call that spawned it is reported.
            turn_started("child", "c1"),
            item_event("item/completed", "tab", "t1", collab("sp1", vec!["child"])),
            // A thread nobody follows names a receiver of its own.
            item_event("item/completed", "stranger", "s1", collab("sp9", vec!["ghost"])),
            turn_started("ghost", "g1"),
            item_event(
                "item/started",
                "ghost",
                "g1",
                command_item("ghost-cmd", "echo ghost", "inProgress"),
            ),
            item_event(
                "item/started",
                "child",
                "c1",
                command_item("child-cmd", "touch late", "inProgress"),
            ),
            turn_completed("tab", "t1", "completed"),
            // Only the child is left; an interrupt reaches it.
            expect_interrupt(5, "child", "c1"),
            turn_completed("child", "c1", "interrupted"),
        ],
    );
    let child_cmd = row_id("tab", "child", "child-cmd");
    let early = pump_until(&provider, |events| events.iter().any(tool_started(child_cmd.clone())));
    let early = {
        let mut all = early;
        provider.pause_time_for_tests();
        until(|| provider.lines_read_for_tests() >= 11);
        all.extend(provider.pump());
        all
    };
    assert!(
        !early.iter().any(is_completed),
        "the child's early turn keeps the tab's turn open: {early:#?}"
    );
    assert!(
        !early.iter().any(tool_started(row_id("tab", "ghost", "ghost-cmd"))),
        "a receiver named from outside the family is not a member: {early:#?}"
    );
    interrupt(&provider, &thread);
    until(|| provider.lines_read_for_tests() >= 12);
    let mut events = provider.pump();
    provider.advance_time_for_tests(agent::providers::codex::provider::INTERRUPT_SETTLE);
    until(|| {
        events.extend(provider.pump());
        events.iter().any(is_completed)
    });
    let (turn, outcome, _) = ended(&events);
    assert_eq!(turn, "t1");
    assert_eq!(outcome, TurnOutcome::Interrupted);
}

// ---------------------------------------------------------------- a family turn while the tab is idle

#[test]
fn family_turn_while_tab_idle_opens_a_tab_turn() {
    let (provider, thread) = running(
        "tab",
        "t1",
        vec![
            announce("tab", "t1", "child", "/root/child"),
            turn_completed("tab", "t1", "completed"),
            // The tab is idle. Its child starts a turn of its own and runs something.
            json!({"sleep_ms": 100}),
            turn_started("child", "c9"),
            item_event(
                "item/started",
                "child",
                "c9",
                command_item("late", "touch late", "inProgress"),
            ),
            item_event(
                "item/completed",
                "child",
                "c9",
                command_item("late", "touch late", "completed"),
            ),
            turn_completed("child", "c9", "completed"),
            // The tab is idle again and takes a turn of its own.
            expect_request(5, "turn/start"),
            turn_response(5, "t2"),
        ],
    );
    // Read as one list: a test thread held up past the stand-in's pause reads both turns in one pump.
    let all = pump_until(&provider, |events| {
        events.iter().filter(|e| is_completed(e)).count() >= 2
    });
    let first_end = position(&all, is_completed);
    assert_eq!(all[..first_end].iter().filter(|e| is_started(e)).count(), 1);
    let second = &all[first_end + 1..];
    assert!(
        matches!(second.iter().find(|e| is_started(e)), Some(AgentDomainEvent::TurnStarted { turn_id }) if turn_id == "c9"),
        "the child's turn opened a tab turn: {second:#?}"
    );
    let late = row_id("tab", "child", "late");
    assert!(second.iter().any(tool_started(late.clone())) && second.iter().any(tool_completed(late)));
    match &second[position(second, is_completed)] {
        AgentDomainEvent::TurnCompleted { turn_id, outcome, .. } => {
            assert_eq!(turn_id, "c9");
            assert_eq!(*outcome, TurnOutcome::Completed);
        }
        other => panic!("{other:?}"),
    }
    // The tab turn is over, so the tab takes a turn of its own.
    assert_eq!(send(&provider, &thread, "next").expect("the tab is idle"), "t2");
}

// ---------------------------------------------------------------- Codex's own reviewer in a child's turn

fn auto_review_completed(thread: &str, turn: &str, item: &str) -> Value {
    write(json!({"method": "item/autoApprovalReview/completed", "params": {
        "threadId": thread, "turnId": turn, "startedAtMs": 1, "reviewId": "rv1",
        "targetItemId": item, "decisionSource": "agent",
        "review": {"status": "approved", "riskLevel": "low", "userAuthorization": null, "rationale": "fine"},
        "action": {"type": "command", "source": "shell", "command": "touch x", "cwd": "/work/proj"},
    }}))
}

/// A subagent's turn that begins inside a tab turn started with reviewer `user` runs under that turn's
/// reviewer: Codex's own reviewer taking up a request in it fails the session, as it would in the tab's
/// own turn.
#[test]
fn codex_reviewing_in_a_childs_turn_inside_a_user_turn_trips() {
    let (provider, _thread) = running(
        "tab",
        "t1",
        vec![
            announce("tab", "t1", "child", "/root/child"),
            turn_started("child", "c1"),
            item_event(
                "item/started",
                "child",
                "c1",
                command_item("x", "touch x", "inProgress"),
            ),
            auto_review_completed("child", "c1", "x"),
        ],
    );
    let events = pump_until(&provider, |events| mismatch_of(events).is_some());
    let (requested, reported) = mismatch_of(&events).expect("a tripwire");
    assert!(requested.contains("\"user\""), "{requested}");
    assert!(reported.contains("item/autoApprovalReview/completed"), "{reported}");
}

/// A child's turn that opens a tab turn while the tab is idle was not started under any reviewer Eitri
/// named, so a review in it is shown as a verdict and trips nothing.
#[test]
fn a_review_in_a_childs_turn_opened_while_the_tab_is_idle_does_not_trip() {
    let (provider, _thread) = running(
        "tab",
        "t1",
        vec![
            announce("tab", "t1", "child", "/root/child"),
            turn_completed("tab", "t1", "completed"),
            json!({"sleep_ms": 100}),
            turn_started("child", "c9"),
            item_event(
                "item/started",
                "child",
                "c9",
                command_item("late", "touch late", "inProgress"),
            ),
            auto_review_completed("child", "c9", "late"),
            item_event(
                "item/completed",
                "child",
                "c9",
                command_item("late", "touch late", "completed"),
            ),
            turn_completed("child", "c9", "completed"),
        ],
    );
    let all = pump_until(&provider, |events| {
        events.iter().filter(|e| is_completed(e)).count() >= 2
    });
    assert!(mismatch_of(&all).is_none(), "{all:#?}");
    assert!(
        all.iter().any(|e| matches!(
            e,
            AgentDomainEvent::PermissionDenied { reason_type: Some(kind), .. } if kind == "auto_review_approved"
        )),
        "the verdict is a note as in Auto: {all:#?}"
    );
}

// ---------------------------------------------------------------- an end that cannot be read

/// A `turn/completed` over the line cap, for `thread`'s turn `turn`.
fn huge_turn_completed(thread: &str, turn: &str) -> Value {
    json!({"write_big": {
        "prefix": format!(
            r#"{{"method":"turn/completed","params":{{"threadId":"{thread}","turn":{{"id":"{turn}","items":[{{"type":"agentMessage","id":"x","text":""#
        ),
        "bytes": 17 * 1024 * 1024,
        "suffix": r#""}],"status":"completed"}}}"#,
    }})
}

/// A `turn/completed` whose turn cannot be read, for `thread`.
fn unreadable_turn_completed(thread: &str) -> Value {
    write(json!({"method": "turn/completed", "params": {"threadId": thread, "turn": {"id": 5}}}))
}

fn ended(events: &[AgentDomainEvent]) -> (String, TurnOutcome, Option<String>) {
    match &events[position(events, is_completed)] {
        AgentDomainEvent::TurnCompleted {
            turn_id,
            outcome,
            detail,
            ..
        } => (turn_id.clone(), *outcome, detail.reason.clone()),
        other => panic!("{other:?}"),
    }
}

/// The end of the tab's own turn arrives in a message Eitri cannot read while a child still runs. The tab's
/// turn is the family's, so it stays open: the child's later calls are still shown, an interrupt still
/// reaches it, and the tab's turn ends, failed, only when the child's does.
#[test]
fn an_unreadable_end_of_the_parents_turn_keeps_the_running_child() {
    for unreadable in [huge_turn_completed("tab", "t1"), unreadable_turn_completed("tab")] {
        let (provider, thread) = running(
            "tab",
            "t1",
            vec![
                announce("tab", "t1", "child", "/root/child"),
                turn_started("child", "c1"),
                unreadable,
                json!({"sleep_ms": 100}),
                item_event(
                    "item/started",
                    "child",
                    "c1",
                    command_item("late", "touch late", "inProgress"),
                ),
                // Only the child is still running, so only the child is sent an interrupt.
                expect_interrupt(5, "child", "c1"),
                turn_completed("child", "c1", "interrupted"),
            ],
        );
        let late = row_id("tab", "child", "late");
        let early = pump_until(&provider, |events| events.iter().any(tool_started(late.clone())));
        assert!(
            !early.iter().any(is_completed),
            "the tab's turn ended while its child ran: {early:#?}"
        );
        interrupt(&provider, &thread);
        let events = until_turn_completed(&provider);
        let (turn, outcome, reason) = ended(&events);
        assert_eq!(turn, "t1");
        assert_eq!(outcome, TurnOutcome::Failed, "the tab's own turn failed");
        assert!(
            matches!(reason.as_deref(), Some("message_too_large" | "unreadable_turn_end")),
            "{reason:?}"
        );
    }
}

/// A child's end that cannot be read ends the child's turn only: the tab's own turn goes on, and the tab's
/// turn ends with the tab's own outcome.
#[test]
fn an_unreadable_end_of_a_childs_turn_ends_only_the_child() {
    for unreadable in [huge_turn_completed("child", "c1"), unreadable_turn_completed("child")] {
        let (provider, _) = running(
            "tab",
            "t1",
            vec![
                announce("tab", "t1", "child", "/root/child"),
                turn_started("child", "c1"),
                unreadable,
                json!({"sleep_ms": 100}),
                item_event(
                    "item/started",
                    "tab",
                    "t1",
                    command_item("own", "echo own", "inProgress"),
                ),
                json!({"sleep_ms": 200}),
                turn_completed("tab", "t1", "completed"),
            ],
        );
        // The tab's turn ends after its own command started, however late both are read.
        let events = until_turn_completed(&provider);
        let own = events.iter().position(tool_started("own"));
        assert!(
            own.is_some_and(|own| own < position(&events, is_completed)),
            "the child's end ended the tab's turn: {events:#?}"
        );
        let (turn, outcome, _) = ended(&events);
        assert_eq!(turn, "t1");
        assert_eq!(
            outcome,
            TurnOutcome::Completed,
            "a child's end is not the tab's failure"
        );
    }
}

/// A child's start that cannot be read still makes its turn one of the family's: the first message naming
/// the turn says which it is, and the tab's turn waits for it.
#[test]
fn an_unreadable_start_of_a_childs_turn_is_still_followed() {
    let (provider, thread) = running(
        "tab",
        "t1",
        vec![
            announce("tab", "t1", "child", "/root/child"),
            json!({"write_big": {
                "prefix": r#"{"method":"turn/started","params":{"threadId":"child","turn":{"id":"c1","items":[{"type":"agentMessage","id":"x","text":""#,
                "bytes": 17 * 1024 * 1024,
                "suffix": r#""}],"status":"inProgress"}}}"#,
            }}),
            turn_completed("tab", "t1", "completed"),
            json!({"sleep_ms": 100}),
            item_event(
                "item/started",
                "child",
                "c1",
                command_item("late", "touch late", "inProgress"),
            ),
            expect_interrupt(5, "child", "c1"),
            turn_completed("child", "c1", "interrupted"),
        ],
    );
    let late = row_id("tab", "child", "late");
    let early = pump_until(&provider, |events| events.iter().any(tool_started(late.clone())));
    assert!(!early.iter().any(is_completed), "{early:#?}");
    interrupt(&provider, &thread);
    let events = until_turn_completed(&provider);
    let (turn, outcome, _) = ended(&events);
    assert_eq!(turn, "t1");
    assert_eq!(outcome, TurnOutcome::Interrupted);
}

/// A `turn/started` over the line cap, for `thread`'s turn `turn`.
fn huge_turn_started(thread: &str, turn: &str) -> Value {
    json!({"write_big": {
        "prefix": format!(
            r#"{{"method":"turn/started","params":{{"threadId":"{thread}","turn":{{"id":"{turn}","items":[{{"type":"agentMessage","id":"x","text":""#
        ),
        "bytes": 17 * 1024 * 1024,
        "suffix": r#""}],"status":"inProgress"}}}"#,
    }})
}

/// A `turn/started` whose turn cannot be read, for `thread`.
fn unreadable_turn_started(thread: &str) -> Value {
    write(json!({"method": "turn/started", "params": {"threadId": thread, "turn": {"id": 5}}}))
}

fn collab_spawn(id: &str, receiver: &str) -> Value {
    json!({"type": "collabAgentToolCall", "id": id, "tool": "spawnAgent", "status": "completed",
           "senderThreadId": "tab", "receiverThreadIds": [receiver], "prompt": "go"})
}

/// A child's start that cannot be read and arrives before the call that makes the child a member is still
/// followed once it is: the tab's turn waits for the child, its calls are shown, and an interrupt reaches
/// it.
#[test]
fn an_unreadable_start_before_the_announcement_is_followed_after_it() {
    for unreadable in [huge_turn_started("child", "c1"), unreadable_turn_started("child")] {
        let (provider, thread) = running(
            "tab",
            "t1",
            vec![
                unreadable,
                item_event("item/completed", "tab", "t1", collab_spawn("sp1", "child")),
                turn_completed("tab", "t1", "completed"),
                json!({"sleep_ms": 100}),
                item_event(
                    "item/started",
                    "child",
                    "c1",
                    command_item("late", "touch late", "inProgress"),
                ),
                expect_interrupt(5, "child", "c1"),
                turn_completed("child", "c1", "interrupted"),
            ],
        );
        let late = row_id("tab", "child", "late");
        let early = pump_until(&provider, |events| events.iter().any(tool_started(late.clone())));
        assert!(
            !early.iter().any(is_completed),
            "the tab's turn ended while its child ran: {early:#?}"
        );
        interrupt(&provider, &thread);
        let events = until_turn_completed(&provider);
        let (turn, outcome, _) = ended(&events);
        assert_eq!(turn, "t1");
        assert_eq!(outcome, TurnOutcome::Interrupted);
    }
}

/// A child whose unreadable start and readable end both come before its announcement is not running when
/// it joins: the tab's turn ends with the tab's own.
#[test]
fn an_early_turn_that_ended_before_the_announcement_is_not_followed() {
    let (provider, _) = running(
        "tab",
        "t1",
        vec![
            unreadable_turn_started("child"),
            turn_completed("child", "c1", "completed"),
            item_event("item/completed", "tab", "t1", collab_spawn("sp1", "child")),
            json!({"sleep_ms": 100}),
            turn_completed("tab", "t1", "completed"),
        ],
    );
    let events = until_turn_completed(&provider);
    let (turn, outcome, _) = ended(&events);
    assert_eq!(turn, "t1");
    assert_eq!(outcome, TurnOutcome::Completed);
}

/// A child whose start (read or not) and unreadable end both come before its announcement is not running
/// when it joins; one announced before its unreadable end is ended by it. Either way the tab's turn ends
/// with the tab's own, and the tab takes its next turn.
#[test]
fn an_unreadable_end_ends_a_child_announced_before_or_after_it() {
    let starts = || {
        [
            unreadable_turn_started("child"),
            huge_turn_started("child", "c1"),
            turn_started("child", "c1"),
        ]
    };
    let ends = || [huge_turn_completed("child", "c1"), unreadable_turn_completed("child")];
    for start in starts() {
        for end in ends() {
            for announced_first in [false, true] {
                let spawn = item_event("item/completed", "tab", "t1", collab_spawn("sp1", "child"));
                let mut rest = if announced_first {
                    vec![spawn, start.clone(), end.clone()]
                } else {
                    vec![start.clone(), end.clone(), spawn]
                };
                rest.extend([
                    json!({"sleep_ms": 100}),
                    turn_completed("tab", "t1", "completed"),
                    expect_request(5, "turn/start"),
                    turn_response(5, "t2"),
                ]);
                let (provider, thread) = running("tab", "t1", rest);
                let events = until_turn_completed(&provider);
                let (turn, outcome, _) = ended(&events);
                assert_eq!(turn, "t1", "{start} {end} announced first: {announced_first}");
                assert_eq!(
                    outcome,
                    TurnOutcome::Completed,
                    "a child's end is not the tab's failure"
                );
                assert_eq!(send(&provider, &thread, "next").expect("the tab is idle"), "t2");
            }
        }
    }
}

// ---------------------------------------------------------------- interrupt

/// A member's turn whose start could not be read has no id an interrupt could name. When nothing else of
/// the family runs, the key cannot reach it, so it is not waited on: the tab's turn ends interrupted and
/// the tab takes its next turn, with no `turn/interrupt` sent.
#[test]
fn interrupting_a_family_running_only_unnamed_turns_ends_the_tabs_turn() {
    let (provider, thread) = running(
        "tab",
        "t1",
        vec![
            item_event("item/completed", "tab", "t1", collab_spawn("sp1", "child")),
            unreadable_turn_started("child"),
            turn_completed("tab", "t1", "completed"),
            // An interrupt would have taken id 5.
            expect_request(5, "turn/start"),
            turn_response(5, "t2"),
        ],
    );
    provider.pause_time_for_tests();
    until(|| provider.lines_read_for_tests() >= 7);
    let early = provider.pump();
    assert!(
        !early.iter().any(is_completed),
        "the tab's turn ended while its child ran: {early:#?}"
    );
    interrupt(&provider, &thread);
    let events = until_turn_completed(&provider);
    let (turn, outcome, _) = ended(&events);
    assert_eq!(turn, "t1");
    assert_eq!(outcome, TurnOutcome::Interrupted);
    assert_eq!(send(&provider, &thread, "next").expect("the tab is idle"), "t2");
}

/// The interrupt reaches the turns it can name; once they have ended, a member turn it could not name is
/// not waited on.
#[test]
fn an_unnamed_turn_is_not_waited_on_once_the_named_ones_ended_after_an_interrupt() {
    let (provider, thread) = running(
        "tab",
        "t1",
        vec![
            item_event("item/completed", "tab", "t1", collab_spawn("sp1", "child")),
            unreadable_turn_started("child"),
            expect_interrupt(5, "tab", "t1"),
            turn_completed("tab", "t1", "interrupted"),
            expect_request(6, "turn/start"),
            turn_response(6, "t2"),
        ],
    );
    provider.pause_time_for_tests();
    until(|| provider.lines_read_for_tests() >= 6);
    provider.pump();
    interrupt(&provider, &thread);
    let events = until_turn_completed(&provider);
    let (turn, outcome, _) = ended(&events);
    assert_eq!(turn, "t1");
    assert_eq!(outcome, TurnOutcome::Interrupted);
    assert_eq!(send(&provider, &thread, "next").expect("the tab is idle"), "t2");
}

/// The tab's own turn whose start could not be read, opened while the tab was idle, ends interrupted on
/// the key too.
#[test]
fn interrupting_the_tabs_own_unnamed_turn_ends_it() {
    let (provider, thread) = running(
        "tab",
        "t1",
        vec![
            turn_completed("tab", "t1", "completed"),
            json!({"sleep_ms": 100}),
            unreadable_turn_started("tab"),
            expect_request(5, "turn/start"),
            turn_response(5, "t2"),
        ],
    );
    let first = until_turn_completed(&provider);
    assert_eq!(ended(&first).0, "t1");
    // The unnamed turn opens after the first one ended; both can be read in one pump.
    let opened = pump_on(&provider, first, |events| {
        events
            .iter()
            .position(is_completed)
            .is_some_and(|ended| events[ended..].iter().any(is_started))
    });
    let opened = &opened[position(&opened, is_completed)..];
    let unnamed = match &opened[position(opened, is_started)] {
        AgentDomainEvent::TurnStarted { turn_id } => turn_id.clone(),
        other => panic!("{other:?}"),
    };
    interrupt(&provider, &thread);
    let events = until_turn_completed(&provider);
    let (turn, outcome, _) = ended(&events);
    assert_eq!(turn, unnamed);
    assert_eq!(outcome, TurnOutcome::Interrupted);
    assert_eq!(send(&provider, &thread, "next").expect("the tab is idle"), "t2");
}

#[test]
fn interrupt_reaches_every_running_family_turn() {
    let gate = ScriptGate::new();
    let (provider, thread) = running(
        "tab",
        "t1",
        vec![
            announce("tab", "t1", "child", "/root/child"),
            turn_started("child", "c1"),
            // The key reaches the parent's turn and the child's, each once, in the order they started.
            expect_interrupt(5, "tab", "t1"),
            expect_interrupt(6, "child", "c1"),
            turn_completed("child", "c1", "interrupted"),
            gate.step(),
            turn_completed("tab", "t1", "interrupted"),
            // A third interrupt would have taken id 7.
            expect_request(7, "turn/start"),
            turn_response(7, "t2"),
        ],
    );
    pump_until(&provider, |events| {
        events.iter().any(|e| matches!(e, AgentDomainEvent::TurnStarted { .. }))
    });
    // Let the announcement and the child's start be read.
    provider.pause_time_for_tests();
    until(|| provider.lines_read_for_tests() >= 6);
    provider.pump();
    provider.pause_time_for_tests();
    for _ in 0..3 {
        interrupt(&provider, &thread);
    }
    gate.wait();
    provider.pause_time_for_tests();
    until(|| provider.lines_read_for_tests() >= 7);
    let before = provider.pump();
    assert!(!before.iter().any(is_completed), "the parent's turn is still running");
    gate.release();
    let events = pump_on(&provider, before, |events| events.iter().any(is_completed));
    match &events[position(&events, is_completed)] {
        AgentDomainEvent::TurnCompleted { turn_id, outcome, .. } => {
            assert_eq!(turn_id, "t1");
            assert_eq!(*outcome, TurnOutcome::Interrupted);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        send(&provider, &thread, "next").expect("no further interrupt was sent"),
        "t2"
    );
}

#[test]
fn interrupting_after_the_parent_ended_stops_the_child_and_ends_interrupted() {
    let (provider, thread) = running(
        "tab",
        "t1",
        vec![
            announce("tab", "t1", "child", "/root/child"),
            turn_started("child", "c1"),
            turn_completed("tab", "t1", "completed"),
            // Only the child still runs, so only the child is sent an interrupt.
            expect_interrupt(5, "child", "c1"),
            turn_completed("child", "c1", "interrupted"),
        ],
    );
    provider.pause_time_for_tests();
    until(|| provider.lines_read_for_tests() >= 7);
    provider.pump();
    interrupt(&provider, &thread);
    let events = until_turn_completed(&provider);
    match &events[position(&events, is_completed)] {
        AgentDomainEvent::TurnCompleted { turn_id, outcome, .. } => {
            assert_eq!(turn_id, "t1");
            assert_eq!(*outcome, TurnOutcome::Interrupted);
        }
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------- approvals

#[test]
fn child_approval_is_a_card_naming_the_subagent() {
    let command = "/usr/bin/zsh -lc 'touch /outside/child'";
    let (provider, thread) = running(
        "tab",
        "t1",
        vec![
            announce("tab", "t1", "child", "/root/child"),
            turn_started("child", "c1"),
            item_event(
                "item/started",
                "child",
                "c1",
                command_item("child-cmd", command, "inProgress"),
            ),
            command_request(0, "child", "c1", "child-cmd", command),
            expect_answer(json!(0), r#"{"decision":"accept"}"#),
            item_event(
                "item/completed",
                "child",
                "c1",
                command_item("child-cmd", command, "completed"),
            ),
            turn_completed("child", "c1", "completed"),
            turn_completed("tab", "t1", "completed"),
        ],
    );
    let events = pump_until(&provider, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. }))
    });
    let (id, input) = card_of(&events);
    assert_eq!(input["subagent"], "/root/child", "the card says whose it is");
    assert_eq!(input["command"], command);
    answer(&provider, &thread, &id, PermissionDecision::Allow);
    let rest = until_turn_completed(&provider);
    assert!(rest.iter().any(|e| matches!(
        e,
        AgentDomainEvent::PermissionResolved { permission_id, outcome: PermissionOutcome::Allowed } if *permission_id == id
    )));
}

/// Codex numbers a thread's items on that thread alone, so a parent and its child can both run a
/// `call-1`. Each is a row of its own, and what the child's call starts, ends, asks or is noted about lands
/// on the child's row, never on the parent's.
#[test]
fn a_childs_item_id_never_lands_on_the_parents_row() {
    let parent_cmd = "/usr/bin/zsh -lc 'echo parent'";
    let child_cmd = "/usr/bin/zsh -lc 'touch /outside/child'";
    let (provider, thread) = running(
        "tab",
        "t1",
        vec![
            announce("tab", "t1", "child", "/root/child"),
            turn_started("child", "c1"),
            item_event(
                "item/started",
                "tab",
                "t1",
                command_item("call-1", parent_cmd, "inProgress"),
            ),
            item_event(
                "item/started",
                "child",
                "c1",
                command_item("call-1", child_cmd, "inProgress"),
            ),
            command_request(0, "child", "c1", "call-1", child_cmd),
            expect_answer(json!(0), r#"{"decision":"decline"}"#),
            // Input typed into the child's command: answered by Eitri, with a note on the child's row.
            write(
                json!({"id": 1, "method": "item/commandExecution/requestApproval", "params": {
                    "kind": "writeStdin", "threadId": "child", "turnId": "c1", "itemId": "call-1",
                }}),
            ),
            expect_answer(json!(1), r#"{"decision":"decline"}"#),
            item_event(
                "item/completed",
                "tab",
                "t1",
                command_item("call-1", parent_cmd, "completed"),
            ),
            item_event(
                "item/completed",
                "child",
                "c1",
                command_item("call-1", child_cmd, "declined"),
            ),
            turn_completed("child", "c1", "completed"),
            turn_completed("tab", "t1", "completed"),
        ],
    );
    let mut events = pump_until(&provider, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. }))
    });
    let card = events
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::PermissionRequested {
                permission_id,
                tool_use_id,
                ..
            } => Some((permission_id.clone(), tool_use_id.clone())),
            _ => None,
        })
        .expect("a card");
    answer(&provider, &thread, &card.0, PermissionDecision::Deny { reason: None });
    events.extend(until_turn_completed(&provider));

    let child_row = row_id("tab", "child", "call-1");
    assert_ne!(child_row, "call-1");
    assert_eq!(row_id("tab", "tab", "call-1"), "call-1", "the tab's own ids are kept");
    let started: Vec<(&str, &Value)> = events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::ToolCallStarted { tool_use_id, input, .. } => Some((tool_use_id.as_str(), input)),
            _ => None,
        })
        .collect();
    assert_eq!(started.len(), 2, "two rows: {events:#?}");
    assert_eq!(started[0].0, "call-1");
    assert_eq!(started[0].1["command"], parent_cmd);
    assert_eq!(started[1].0, child_row);
    assert_eq!(started[1].1["command"], child_cmd);
    assert_eq!(
        card.1.as_deref(),
        Some(child_row.as_str()),
        "the card sits on the child's row"
    );
    let completed: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::ToolCallCompleted { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(completed, vec!["call-1", child_row.as_str()], "{events:#?}");
    let noted: Vec<Option<&str>> = events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::PermissionDenied { tool_use_id, .. } => Some(tool_use_id.as_deref()),
            _ => None,
        })
        .collect();
    assert_eq!(noted, vec![Some(child_row.as_str())], "{events:#?}");
}

#[test]
fn the_tabs_own_approval_names_no_subagent() {
    let command = "/usr/bin/zsh -lc 'touch /outside/own'";
    let (provider, thread) = running(
        "tab",
        "t1",
        vec![
            announce("tab", "t1", "child", "/root/child"),
            item_event(
                "item/started",
                "tab",
                "t1",
                command_item("own-cmd", command, "inProgress"),
            ),
            command_request(0, "tab", "t1", "own-cmd", command),
            expect_answer(json!(0), r#"{"decision":"decline"}"#),
            turn_completed("tab", "t1", "completed"),
        ],
    );
    let events = pump_until(&provider, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. }))
    });
    let (id, input) = card_of(&events);
    assert!(input.get("subagent").is_none(), "{input}");
    answer(&provider, &thread, &id, PermissionDecision::Deny { reason: None });
    until_turn_completed(&provider);
}

#[test]
fn stranger_thread_request_is_declined_and_logged() {
    let command = "/usr/bin/zsh -lc 'touch /outside/stranger'";
    let (provider, _) = running(
        "tab",
        "t1",
        vec![
            command_request(7, "stranger", "s1", "stranger-cmd", command),
            // Declined at once, with nobody asked: the next line the stand-in reads is the tab's own end.
            expect_answer(json!(7), r#"{"decision":"decline"}"#),
            turn_completed("tab", "t1", "completed"),
        ],
    );
    let events = until_turn_completed(&provider);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. })),
        "a stranger's request is not shown: {events:#?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionDenied { .. })),
        "and it leaves no note on a row of this tab: {events:#?}"
    );
}

// ---------------------------------------------------------------- close

#[test]
fn closing_mid_child_interrupts_the_family() {
    let gate = ScriptGate::new();
    let (provider, thread) = running(
        "tab",
        "t1",
        vec![
            announce("tab", "t1", "child", "/root/child"),
            turn_started("child", "c1"),
            item_event(
                "item/started",
                "child",
                "c1",
                command_item("sleeper", "sleep 60", "inProgress"),
            ),
            // The family is interrupted before anything else, then the thread is let go.
            expect_interrupt(5, "tab", "t1"),
            expect_interrupt(6, "child", "c1"),
            expect_exact(r#"{"id":7,"method":"thread/unsubscribe","params":{"threadId":"tab"}}"#),
            // Reaching the gate proves all three requests arrived in order. Hold the answer while
            // observing whether close has returned.
            gate.step(),
            write(json!({"id": 7, "result": {"status": "unsubscribed"}})),
        ],
    );
    pump_until(&provider, |events| {
        events.iter().any(tool_started(row_id("tab", "child", "sleeper")))
    });
    let pid = provider.pid();
    assert!(alive(pid));
    std::thread::scope(|scope| {
        let closing = scope.spawn(|| provider.close_on_eof_for_tests(CloseSessionRequest { session_id: thread }));
        gate.wait();
        assert!(alive(pid), "the child still holds the unsubscribe answer");
        assert!(
            !closing.is_finished(),
            "close has not returned at the child's unsubscribe checkpoint"
        );
        gate.release();
        assert!(closing.join().unwrap(), "the child exits on EOF before grace expires");
    });
    assert!(!alive(pid), "the child is gone and reaped");
    let events = provider.pump();
    assert_eq!(
        events.last(),
        Some(&AgentDomainEvent::SessionClosed {
            reason: "closed_by_host".into()
        })
    );
}

// ---------------------------------------------------------------- the caps

fn mismatch_of(events: &[AgentDomainEvent]) -> Option<(String, String)> {
    events.iter().find_map(|e| match e {
        AgentDomainEvent::PolicyMismatch { requested, reported } => Some((requested.clone(), reported.clone())),
        _ => None,
    })
}

/// A family larger than Eitri follows is not followed in part: a child left out could write files while
/// the tab reads as idle, so the session ends instead.
#[test]
fn more_subagents_than_are_followed_end_the_session() {
    let receivers: Vec<String> = (0..257).map(|n| format!("child-{n}")).collect();
    let collab = json!({"type": "collabAgentToolCall", "id": "sp1", "tool": "spawnAgent", "status": "completed",
                        "senderThreadId": "tab", "receiverThreadIds": receivers, "prompt": "go"});
    let (provider, thread) = running(
        "tab",
        "t1",
        vec![
            item_event("item/completed", "tab", "t1", collab),
            turn_started("child-256", "c256"),
        ],
    );
    let events = pump_until(&provider, |events| mismatch_of(events).is_some());
    let (requested, reported) = mismatch_of(&events).expect("a mismatch");
    assert!(requested.contains("256"), "{requested}");
    assert!(reported.contains("more"), "{reported}");
    assert!(
        send(&provider, &thread, "next").is_err(),
        "a tripped session takes no turn"
    );
}

/// The same for announcements one at a time.
#[test]
fn one_announcement_past_the_cap_ends_the_session() {
    let mut rest: Vec<Value> = (0..257)
        .map(|n| announce("tab", "t1", &format!("child-{n}"), &format!("/root/c{n}")))
        .collect();
    rest.push(turn_started("child-256", "c256"));
    let (provider, _) = running("tab", "t1", rest);
    let events = pump_until(&provider, |events| mismatch_of(events).is_some());
    assert!(mismatch_of(&events).is_some());
}

/// Turns of threads not yet announced are kept up to a cap; one more ends the session rather than leaving
/// a child that later joins unfollowed.
#[test]
fn more_early_turns_than_are_kept_end_the_session() {
    let rest: Vec<Value> = (0..65)
        .map(|n| turn_started(&format!("early-{n}"), &format!("e{n}")))
        .collect();
    let (provider, _) = running("tab", "t1", rest);
    let events = pump_until(&provider, |events| mismatch_of(events).is_some());
    let (requested, _) = mismatch_of(&events).expect("a mismatch");
    assert!(requested.contains("64"), "{requested}");
}

/// An interrupt pressed while the `turn/start` answer is awaited reaches the subagent turns that started
/// meanwhile too, not only the tab's own turn.
#[test]
fn an_interrupt_before_the_turn_id_reaches_the_family() {
    let gate = ScriptGate::new();
    let (provider, thread) = running(
        "tab",
        "t1",
        vec![
            announce("tab", "t1", "child", "/root/child"),
            turn_completed("tab", "t1", "completed"),
            expect_request(5, "turn/start"),
            // Read while the answer is awaited, so held until it comes.
            turn_started("child", "c2"),
            gate.step(),
            turn_response(5, "t2"),
            expect_interrupt(6, "tab", "t2"),
            expect_interrupt(7, "child", "c2"),
            turn_completed("tab", "t2", "interrupted"),
            turn_completed("child", "c2", "interrupted"),
        ],
    );
    pump_until(&provider, |events| events.iter().any(is_completed));
    std::thread::scope(|scope| {
        let sending = scope.spawn(|| send(&provider, &thread, "again"));
        gate.wait();
        provider
            .interrupt_turn(InterruptTurnRequest {
                session_id: thread.clone(),
            })
            .expect("taken while the start is awaited");
        gate.release();
        assert_eq!(sending.join().expect("the send").expect("the turn starts"), "t2");
    });
    let events = pump_until(&provider, |events| events.iter().any(is_completed));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentDomainEvent::TurnCompleted {
            outcome: TurnOutcome::Interrupted,
            ..
        }
    )));
}

/// An announcement that does not read may name a child Eitri would never follow; the session ends.
#[test]
fn an_unreadable_announcement_ends_the_session() {
    for item in [
        json!({"type": "subAgentActivity", "id": "sp-x", "agentThreadId": "child"}),
        json!({"type": "collabAgentToolCall", "id": "co-x", "receiverThreadIds": "child"}),
    ] {
        let (provider, thread) = running("tab", "t1", vec![item_event("item/started", "tab", "t1", item.clone())]);
        let events = pump_until(&provider, |events| mismatch_of(events).is_some());
        let (_, reported) = mismatch_of(&events).expect("a mismatch");
        assert!(reported.contains("could not read"), "{item}: {reported}");
        assert!(
            send(&provider, &thread, "next").is_err(),
            "a tripped session takes no turn"
        );
    }
}

/// A child's turn whose start does not read is not given the id of the child's previous turn by a late
/// message about that turn, so the tab's turn waits for the new one to end.
#[test]
fn a_late_end_of_an_earlier_turn_does_not_end_an_unread_one() {
    let (provider, _) = running(
        "tab",
        "t1",
        vec![
            announce("tab", "t1", "child", "/root/child"),
            turn_started("child", "c1"),
            turn_completed("child", "c1", "completed"),
            // The child's next turn starts in a form that does not read.
            write(json!({"method": "turn/started", "params": {"threadId": "child", "turn": {"bogus": true}}})),
            // A late, repeated end of the turn that is over.
            turn_completed("child", "c1", "completed"),
            turn_completed("tab", "t1", "completed"),
            json!({"sleep_ms": 800}),
            item_event("item/started", "child", "c2", command_item("x2", "true", "inProgress")),
            item_event("item/completed", "child", "c2", command_item("x2", "true", "completed")),
            turn_completed("child", "c2", "completed"),
        ],
    );
    // The tab's turn ends after the child's new turn ran, however late both are read.
    let events = pump_until(&provider, |events| events.iter().any(is_completed));
    let ran = events.iter().position(tool_completed(row_id("tab", "child", "x2")));
    assert!(
        ran.is_some_and(|ran| ran < position(&events, is_completed)),
        "the tab's turn ended while the child's new turn ran: {events:#?}"
    );
}

/// Only Eitri says whose a call is: a `subagent` key an item of the tab's own thread carries itself does not
/// make its row read as a subagent's.
#[test]
fn a_subagent_key_in_the_tab_s_own_item_is_not_shown() {
    let search = json!({"type": "webSearch", "id": "ws1", "query": "x", "subagent": "/root/impostor"});
    let (provider, _) = running(
        "tab",
        "t1",
        vec![
            item_event("item/started", "tab", "t1", search.clone()),
            item_event("item/completed", "tab", "t1", search),
            turn_completed("tab", "t1", "completed"),
        ],
    );
    let events = pump_until(&provider, |events| events.iter().any(is_completed));
    let input = events
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::ToolCallStarted { input, .. } => Some(input.clone()),
            _ => None,
        })
        .expect("the search's row");
    assert!(input.get("subagent").is_none(), "{input}");
}

/// The tab's turn whose `turn/start` timed out is interrupted when Codex starts it after all, also when a
/// subagent's turn opened the tab's turn first; the subagent's own turn is left alone.
#[test]
fn a_timed_out_turn_that_joins_a_subagent_turn_is_interrupted() {
    let gate = ScriptGate::new();
    let (provider, thread) = running(
        "tab",
        "t1",
        vec![
            announce("tab", "t1", "child", "/root/child"),
            turn_completed("tab", "t1", "completed"),
            expect_request(5, "turn/start"),
            gate.step(),
            turn_started("child", "c3"),
            turn_started("tab", "t2"),
            expect_interrupt(6, "tab", "t2"),
            turn_completed("tab", "t2", "interrupted"),
            json!({"sleep_ms": 300}),
            turn_completed("child", "c3", "completed"),
        ],
    );
    pump_until(&provider, |events| events.iter().any(is_completed));
    assert!(matches!(send(&provider, &thread, "again"), Err(ProviderError::Timeout)));
    gate.release();
    let events = pump_until(&provider, |events| events.iter().any(is_completed));
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentDomainEvent::TurnCompleted {
                outcome: TurnOutcome::Interrupted,
                ..
            }
        )),
        "{events:#?}"
    );
}

/// A child announced without a thread id could never be followed; the session ends as for one that does not
/// read.
#[test]
fn an_announcement_with_no_thread_id_ends_the_session() {
    let item = json!({"type": "subAgentActivity", "id": "sp-e", "kind": "started", "agentThreadId": "",
                      "agentPath": "/root/nameless"});
    let (provider, thread) = running("tab", "t1", vec![item_event("item/started", "tab", "t1", item)]);
    let events = pump_until(&provider, |events| mismatch_of(events).is_some());
    let (_, reported) = mismatch_of(&events).expect("a mismatch");
    assert!(reported.contains("no thread id"), "{reported}");
    assert!(send(&provider, &thread, "next").is_err());
}

// ---------------------------------------------------------------- a family turn that does not end after its interrupt

/// The tab's own turn reports `completed` after the key while a child ignores its interrupt: once the child is
/// overdue the session is stopped, and the tab's turn ends interrupted, since the key is what stopped the child.
#[test]
fn a_child_that_does_not_end_after_its_interrupt_stops_the_session_and_the_turn_ends_interrupted() {
    let (provider, thread) = running(
        "tab",
        "t1",
        vec![
            announce("tab", "t1", "child", "/root/child"),
            turn_started("child", "c1"),
            expect_interrupt(5, "tab", "t1"),
            expect_interrupt(6, "child", "c1"),
            turn_completed("tab", "t1", "completed"),
            // The child never ends; the stand-in reads its input to the end and answers nothing more.
        ],
    );
    provider.pause_time_for_tests();
    until(|| provider.lines_read_for_tests() >= 6);
    provider.pump();
    provider.pause_time_for_tests();
    interrupt(&provider, &thread);
    provider.advance_time_for_tests(agent::providers::codex::provider::INTERRUPT_SETTLE);
    let events = pump_until(&provider, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. }))
    });
    let (turn, outcome, _) = ended(&events);
    assert_eq!(turn, "t1");
    assert_eq!(outcome, TurnOutcome::Interrupted, "{events:#?}");
}

/// A turn a subagent opened has nothing on the tab's own thread to take a message: it is refused unsent,
/// saying so, rather than sent to Codex to be refused as "no active turn".
#[test]
fn a_message_while_only_a_subagent_runs_is_refused_unsent() {
    let gate = ScriptGate::new();
    let (provider, thread) = running(
        "th-sub",
        "t-own",
        vec![
            announce("th-sub", "t-own", "child", "/root/child"),
            turn_started("child", "c-1"),
            turn_completed("child", "c-1", "completed"),
            turn_completed("th-sub", "t-own", "completed"),
            json!({"sleep_ms": 100}),
            turn_started("child", "c-2"),
            gate.step(),
            turn_completed("child", "c-2", "completed"),
        ],
    );
    let events = pump_until(&provider, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::TurnStarted { turn_id } if turn_id == "c-2"))
    });
    assert!(events.iter().any(is_completed), "the tab's own turn ended first");
    let refused = provider.steer_turn(agent::SteerTurnRequest {
        session_id: thread.clone(),
        expected_turn_id: "c-2".into(),
        client_message_id: "m1".into(),
        text: "x".into(),
        as_typed: "x".into(),
    });
    match refused {
        Err(ProviderError::Provider { code, message }) => {
            assert_eq!(code, agent::ProviderErrorCode::NoActiveTurn);
            assert_eq!(message, "only a subagent is running");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    gate.release();
    until_turn_completed(&provider);
}
