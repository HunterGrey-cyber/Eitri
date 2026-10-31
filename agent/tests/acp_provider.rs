//! `AcpProvider` against a scripted stand-in for an ACP agent (`eitri-agent-standin`). The scripts are
//! written from the traffic recorded from `codex-acp` 2.1.1 (the shapes of `initialize`, `session/new`,
//! `session/update` and `session/request_permission`), and by hand for the ones no run produced. No real
//! agent runs here and no model turn is spent.

use agent::providers::acp::{AcpAgentSpec, AcpProvider, ContextGauge};
use agent::{
    AgentDomainEvent, AgentProvider, CloseSessionRequest, ContentKind, DirTrust, ExternalProvider, ExternalStart,
    InterruptTurnRequest, PermissionDecision, PermissionOutcome, ProviderError, ProviderErrorCode,
    ResolvePermissionRequest, ResumeStatus, Reviewer, SendTurnRequest, TokenUsage, TrustPlan, TurnOutcome, TurnPolicy,
};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::{Duration, Instant};

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

/// What Eitri writes for a request: our members in a fixed order, the `jsonrpc` member first.
fn expect_request(id: u64, method: &str, params: Value) -> Value {
    expect_exact(&format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#
    ))
}

fn expect_notification(method: &str, params: Value) -> Value {
    expect_exact(&format!(r#"{{"jsonrpc":"2.0","method":"{method}","params":{params}}}"#))
}

/// What Eitri writes to answer a request of the agent's.
fn expect_answer(id: u64, result: Value) -> Value {
    expect_exact(&format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{result}}}"#))
}

fn write(message: Value) -> Value {
    json!({ "write": message.to_string() })
}

fn reply(id: u64, result: Value) -> Value {
    write(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn update(session: &str, body: Value) -> Value {
    write(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": body}}))
}

fn agent_request(id: u64, method: &str, params: Value) -> Value {
    write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
}

fn init_params() -> String {
    format!(
        r#"{{"clientCapabilities":{{"fs":{{"readTextFile":false,"writeTextFile":false}},"terminal":false}},"clientInfo":{{"name":"eitri","title":"Eitri","version":"{}"}},"protocolVersion":1}}"#,
        env!("CARGO_PKG_VERSION")
    )
}

fn init_answer(load_session: bool) -> Value {
    json!({
        "protocolVersion": 1,
        "agentInfo": {"name": "stand-in", "title": "Stand-in", "version": "1.2.3"},
        "agentCapabilities": {"loadSession": load_session, "sessionCapabilities": {"close": {}}},
        "authMethods": [{"id": "chat-gpt", "name": "ChatGPT"}]
    })
}

/// `initialize` answered, ready for `session/new` (id 2).
fn handshake(load_session: bool) -> Vec<Value> {
    vec![
        expect_exact(&format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}}"#,
            init_params()
        )),
        reply(1, init_answer(load_session)),
    ]
}

fn new_session(mode: &str) -> Vec<Value> {
    vec![
        expect_request(2, "session/new", json!({"cwd": "/work", "mcpServers": []})),
        reply(
            2,
            json!({"sessionId": "s1", "modes": {"currentModeId": mode, "availableModes": []}, "models": {"currentModelId": "m-1"}}),
        ),
    ]
}

/// The handshake and a new session in `mode`.
fn boot(mode: &str) -> Vec<Value> {
    let mut script = handshake(false);
    script.extend(new_session(mode));
    script
}

fn expect_prompt(id: u64, text: &str) -> Value {
    expect_request(
        id,
        "session/prompt",
        json!({"sessionId": "s1", "prompt": [{"type": "text", "text": text}]}),
    )
}

fn standin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_eitri-agent-standin"))
}

fn spec(script: Vec<Value>, auto_mode: Option<&str>) -> AcpAgentSpec {
    AcpAgentSpec {
        name: "stand-in".to_string(),
        argv: vec![
            OsString::from(standin()),
            OsString::from(Value::Array(script).to_string()),
        ],
        auto_mode: auto_mode.map(str::to_string),
        read_only_mode: None,
    }
}

fn connect(script: Vec<Value>, auto_mode: Option<&str>) -> Result<AcpProvider, ProviderError> {
    AcpProvider::connect(spec(script, auto_mode), std::env::temp_dir())
}

fn start_request(resume: Option<&str>) -> ExternalStart {
    ExternalStart {
        cwd: "/work".to_string(),
        plan: TrustPlan {
            dirs: vec![(PathBuf::from("/work"), DirTrust::Trusted)],
            skills_off: false,
            fingerprint: "fp".to_string(),
            project_only: Vec::new(),
        },
        policy: TurnPolicy {
            reviewer: Reviewer::Agent,
            sandbox: agent::Sandbox::Project,
        },
        resume: resume.map(str::to_string),
    }
}

/// A provider with its session started.
fn started(script: Vec<Value>, auto_mode: Option<&str>) -> AcpProvider {
    let provider = connect(script, auto_mode).expect("connects");
    provider.start(start_request(None)).expect("starts");
    provider
}

fn send(provider: &AcpProvider, text: &str) -> Result<String, ProviderError> {
    provider.send_turn_with(
        SendTurnRequest {
            session_id: "s1".to_string(),
            text: text.to_string(),
        },
        TurnPolicy {
            reviewer: Reviewer::Agent,
            sandbox: agent::Sandbox::Project,
        },
    )
}

fn allow(provider: &AcpProvider, permission_id: &str) -> Result<(), ProviderError> {
    resolve(provider, permission_id, PermissionDecision::Allow)
}

fn deny(provider: &AcpProvider, permission_id: &str) -> Result<(), ProviderError> {
    resolve(provider, permission_id, PermissionDecision::Deny { reason: None })
}

fn resolve(provider: &AcpProvider, permission_id: &str, decision: PermissionDecision) -> Result<(), ProviderError> {
    provider.resolve_permission(ResolvePermissionRequest {
        session_id: "s1".to_string(),
        permission_id: permission_id.to_string(),
        decision,
    })
}

/// Pumps until an event satisfies `done`; everything pumped on the way is returned.
fn pump_until(provider: &AcpProvider, what: &str, done: impl Fn(&AgentDomainEvent) -> bool) -> Vec<AgentDomainEvent> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut all = Vec::new();
    loop {
        let batch = provider.pump();
        let hit = batch.iter().any(&done);
        all.extend(batch);
        if hit {
            return all;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}; saw {all:#?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn until_completed(provider: &AcpProvider) -> Vec<AgentDomainEvent> {
    pump_until(provider, "the turn to end", |e| {
        matches!(e, AgentDomainEvent::TurnCompleted { .. })
    })
}

fn until_permission(provider: &AcpProvider) -> (String, Vec<AgentDomainEvent>) {
    let events = pump_until(provider, "a permission request", |e| {
        matches!(e, AgentDomainEvent::PermissionRequested { .. })
    });
    let id = events
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::PermissionRequested { permission_id, .. } => Some(permission_id.clone()),
            _ => None,
        })
        .expect("a permission request");
    (id, events)
}

/// An event as one short line, for comparing a stream. The session opening is left out.
fn shape(event: &AgentDomainEvent) -> Option<String> {
    Some(match event {
        AgentDomainEvent::SessionOpened { .. } => return None,
        AgentDomainEvent::TurnStarted { turn_id } => format!("started {turn_id}"),
        AgentDomainEvent::UserPromptSubmitted { text } => format!("user {text}"),
        AgentDomainEvent::ContentDelta { kind, text, .. } => match kind {
            ContentKind::Text => format!("text {text}"),
            ContentKind::Thinking => format!("thinking {text}"),
        },
        AgentDomainEvent::AssistantMessageBoundary { .. } => "boundary".to_string(),
        AgentDomainEvent::ToolCallStarted { tool_use_id, name, .. } => format!("tool {tool_use_id} {name}"),
        AgentDomainEvent::ToolCallCompleted {
            tool_use_id,
            content,
            is_error,
            ..
        } => format!(
            "tool-done {tool_use_id} {} {is_error}",
            content.as_str().unwrap_or("<not text>")
        ),
        AgentDomainEvent::TurnCompleted {
            turn_id,
            outcome,
            result_text,
            ..
        } => format!("completed {turn_id} {outcome:?} {result_text}"),
        other => format!("{other:?}"),
    })
}

fn shapes(events: &[AgentDomainEvent]) -> Vec<String> {
    events.iter().filter_map(shape).collect()
}

fn turn_end(events: &[AgentDomainEvent]) -> (TurnOutcome, Option<String>, Option<String>, Option<agent::UsageInfo>) {
    events
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::TurnCompleted {
                outcome,
                stop_reason,
                detail,
                usage,
                ..
            } => Some((*outcome, stop_reason.clone(), detail.reason.clone(), usage.clone())),
            _ => None,
        })
        .expect("a turn end")
}

fn tool_call(id: &str, kind: &str, title: &str) -> Value {
    json!({
        "sessionUpdate": "tool_call", "toolCallId": id, "kind": kind, "title": title,
        "status": "in_progress", "rawInput": {"command": "ls"}
    })
}

fn permission_params(call: &str, options: Value) -> Value {
    json!({
        "sessionId": "s1",
        "toolCall": {"toolCallId": call, "title": "Run command", "kind": "execute", "status": "pending",
                     "rawInput": {"command": "touch x", "cwd": "/work"}},
        "options": options
    })
}

// ---------------------------------------------------------------- the handshake

#[test]
fn initialize_advertises_no_fs_or_terminal() {
    // The expected line is written out, not built from the provider's types: it is the pin.
    let mut script = vec![expect_exact(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"clientCapabilities":{{"fs":{{"readTextFile":false,"writeTextFile":false}},"terminal":false}},"clientInfo":{{"name":"eitri","title":"Eitri","version":"{}"}},"protocolVersion":1}}}}"#,
        env!("CARGO_PKG_VERSION")
    ))];
    script.push(reply(1, init_answer(true)));
    script.extend(new_session("agent"));
    let provider = connect(script, None).expect("connects");
    assert_eq!(provider.name(), "acp:stand-in");
    assert!(provider.resumes());
    // The next line the child expects is `session/new` with no tool servers; it ends the child if not.
    assert_eq!(provider.start(start_request(None)).expect("starts"), "s1");
    let events = provider.pump();
    assert!(matches!(
        events.as_slice(),
        [AgentDomainEvent::SessionOpened { session_id, model, cwd, .. }]
            if session_id == "s1" && model == "m-1" && cwd == "/work"
    ));
    // No mode was registered, so none was set and the agent's own stays.
    assert_eq!(provider.auto_mode(), None);
}

#[test]
fn other_protocol_version_refused() {
    let mut script = handshake(false)[..1].to_vec();
    let mut answer = init_answer(false);
    answer["protocolVersion"] = json!(2);
    script.push(reply(1, answer));
    let err = connect(script, None).err().expect("refused");
    match err {
        ProviderError::Provider { code, message } => {
            assert_eq!(code, ProviderErrorCode::IncompatibleProtocol);
            assert!(message.contains("version 2"), "{message}");
        }
        other => panic!("unexpected error {other}"),
    }

    // An answer with no version is not version 1 either.
    let mut script = handshake(false)[..1].to_vec();
    script.push(reply(1, json!({"agentCapabilities": {}})));
    assert!(matches!(
        connect(script, None).err().expect("refused"),
        ProviderError::Provider {
            code: ProviderErrorCode::IncompatibleProtocol,
            ..
        }
    ));
}

#[test]
fn a_session_refused_is_an_error_for_the_tab() {
    let mut script = handshake(false);
    script.push(expect_request(
        2,
        "session/new",
        json!({"cwd": "/work", "mcpServers": []}),
    ));
    script.push(write(
        json!({"jsonrpc": "2.0", "id": 2, "error": {"code": -32000, "message": "sign in first"}}),
    ));
    let provider = connect(script, None).expect("connects");
    let err = provider.start(start_request(None)).expect_err("refused");
    assert!(err.to_string().contains("sign in first"), "{err}");
}

// ---------------------------------------------------------------- the stream

#[test]
fn updates_map_and_unknown_kind_is_dropped() {
    let mut script = boot("agent");
    script.push(expect_prompt(3, "hi"));
    let m = |id: &str, text: &str| {
        update(
            "s1",
            json!({"sessionUpdate": "agent_message_chunk", "messageId": id, "content": {"type": "text", "text": text}}),
        )
    };
    script.extend([
        // Kinds Eitri accepts and does not use, then one it does not know.
        update("s1", json!({"sessionUpdate": "available_commands_update", "availableCommands": []})),
        update("s1", json!({"sessionUpdate": "session_info_update", "title": "t"})),
        update("s1", json!({"sessionUpdate": "plan", "entries": []})),
        update("s1", json!({"sessionUpdate": "a_kind_from_the_future", "x": 1})),
        write(json!({"jsonrpc": "2.0", "method": "_auth/status_update", "params": {"authStatus": {}}})),
        // An update for another session is not this tab's.
        update(
            "other",
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "stray"}}),
        ),
        update(
            "s1",
            json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "pondering"}}),
        ),
        m("m1", "Hel"),
        m("m1", "lo"),
        // Not text: dropped, and it must not reset the message.
        update(
            "s1",
            json!({"sessionUpdate": "agent_message_chunk", "messageId": "m1", "content": {"type": "image", "data": "x"}}),
        ),
        update("s1", tool_call("t1", "execute", "Run ls")),
        update(
            "s1",
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed",
                   "content": [{"type": "content", "content": {"type": "text", "text": "out"}}]}),
        ),
        // A call that arrives already finished, with a diff.
        update(
            "s1",
            json!({"sessionUpdate": "tool_call", "toolCallId": "t2", "kind": "edit", "title": "Edit a.txt",
                   "status": "failed", "locations": [{"path": "/work/a.txt"}],
                   "content": [{"type": "diff", "path": "/work/a.txt", "oldText": "a", "newText": "b"}]}),
        ),
        // A late repeat about a finished call draws no second row.
        update(
            "s1",
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed"}),
        ),
        m("m2", "Done"),
        m("m3", "More"),
        reply(3, json!({"stopReason": "end_turn"})),
    ]);
    let provider = started(script, None);
    provider.pump();
    assert_eq!(send(&provider, "hi").expect("sends"), "acp-turn-1");
    let events = until_completed(&provider);
    assert_eq!(
        shapes(&events),
        vec![
            "started acp-turn-1",
            "thinking pondering",
            "text Hel",
            "text lo",
            "tool t1 acp:execute",
            "tool-done t1 out false",
            "tool t2 acp:edit",
            "tool-done t2  true",
            "text Done",
            "boundary",
            "text More",
            "completed acp-turn-1 Completed More",
        ]
    );
    // What a row shows of a call.
    let started_inputs: Vec<&Value> = events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::ToolCallStarted { input, .. } => Some(input),
            _ => None,
        })
        .collect();
    assert_eq!(
        started_inputs[0],
        &json!({"title": "Run ls", "rawInput": {"command": "ls"}, "locations": null, "diffs": []})
    );
    assert_eq!(
        started_inputs[1]["diffs"],
        json!([{"path": "/work/a.txt", "oldText": "a", "newText": "b"}])
    );
    assert_eq!(started_inputs[1]["locations"], json!([{"path": "/work/a.txt"}]));
    assert!(!format!("{events:?}").contains("stray"));
}

#[test]
fn command_lists_become_events_only_when_they_change() {
    let mut script = boot("agent");
    let list = |names: &[(&str, &str)]| {
        update(
            "s1",
            json!({"sessionUpdate": "available_commands_update",
                   "availableCommands": names.iter().map(|(n, d)| json!({"name": n, "description": d,
                       "input": {"hint": "args"}})).collect::<Vec<_>>()}),
        )
    };
    script.extend([
        expect_prompt(3, "x"),
        list(&[("web", "Search the web"), ("test", "Run\nthe tests")]),
        // The same list again is not news, and an unreadable one is dropped whole.
        list(&[("web", "Search the web"), ("test", "Run\nthe tests")]),
        update(
            "s1",
            json!({"sessionUpdate": "available_commands_update", "availableCommands": "no"}),
        ),
        list(&[("plan", "Plan")]),
        reply(3, json!({"stopReason": "end_turn"})),
    ]);
    let provider = started(script, None);
    send(&provider, "x").expect("sends");
    let events = until_completed(&provider);
    let lists: Vec<Vec<(String, String, Option<String>)>> = events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::CommandsAvailable { commands } => Some(
                commands
                    .iter()
                    .map(|c| (c.name.clone(), c.description.clone(), c.hint.clone()))
                    .collect(),
            ),
            _ => None,
        })
        .collect();
    let row = |n: &str, d: &str| (n.to_string(), d.to_string(), Some("args".to_string()));
    assert_eq!(
        lists,
        vec![
            vec![row("web", "Search the web"), row("test", "Run the tests")],
            vec![row("plan", "Plan")]
        ]
    );
}

#[test]
fn a_list_sent_while_the_session_starts_is_not_lost() {
    let mut script = handshake(false);
    script.extend([
        expect_request(2, "session/new", json!({"cwd": "/work", "mcpServers": []})),
        update(
            "s1",
            json!({"sessionUpdate": "available_commands_update",
                   "availableCommands": [{"name": "web", "description": "Search the web"}]}),
        ),
        reply(2, json!({"sessionId": "s1", "models": {"currentModelId": "m-1"}})),
    ]);
    let provider = started(script, None);
    let events = pump_until(&provider, "the commands", |e| {
        matches!(e, AgentDomainEvent::CommandsAvailable { .. })
    });
    let opened = events
        .iter()
        .position(|e| matches!(e, AgentDomainEvent::SessionOpened { .. }))
        .expect("opened");
    let listed = events
        .iter()
        .position(|e| matches!(e, AgentDomainEvent::CommandsAvailable { .. }))
        .expect("listed");
    assert!(opened < listed, "the list follows the session's opening");
}

#[test]
fn usage_update_is_context_not_tokens() {
    let mut script = boot("agent");
    script.extend([
        expect_prompt(3, "one"),
        update(
            "s1",
            json!({"sessionUpdate": "usage_update", "used": 1200, "size": 200000}),
        ),
        reply(3, json!({"stopReason": "end_turn"})),
        expect_prompt(4, "two"),
        update(
            "s1",
            json!({"sessionUpdate": "usage_update", "used": 2400, "size": 200000}),
        ),
        reply(
            4,
            json!({"stopReason": "end_turn",
                   "usage": {"totalTokens": 18, "inputTokens": 10, "outputTokens": 5, "cachedReadTokens": 3,
                             "thoughtTokens": 4}}),
        ),
    ]);
    let provider = started(script, None);
    assert_eq!(provider.context_gauge(), None);

    send(&provider, "one").expect("sends");
    let events = until_completed(&provider);
    // The gauge moved; no tokens were reported, so none are claimed.
    assert_eq!(
        provider.context_gauge(),
        Some(ContextGauge {
            used: 1200,
            size: 200000
        })
    );
    assert_eq!(turn_end(&events).3, None);

    send(&provider, "two").expect("sends");
    let events = until_completed(&provider);
    assert_eq!(
        provider.context_gauge(),
        Some(ContextGauge {
            used: 2400,
            size: 200000
        })
    );
    // The turn's tokens are the prompt answer's, with no cost and no gauge figure in them.
    let usage = turn_end(&events).3.expect("usage");
    assert_eq!(
        usage.tokens,
        Some(TokenUsage {
            input: 10,
            output: 5,
            cache_creation: 0,
            cache_read: 3
        })
    );
    assert!(!usage.cost_reported);
    assert_eq!(usage.model.as_deref(), Some("m-1"));
}

#[test]
fn stop_reasons_map() {
    let mut script = boot("agent");
    let reasons = [
        "end_turn",
        "max_tokens",
        "max_turn_requests",
        "refusal",
        "cancelled",
        "tool_budget",
    ];
    for (i, reason) in reasons.iter().enumerate() {
        let id = 3 + i as u64;
        script.push(expect_prompt(id, "go"));
        script.push(reply(id, json!({"stopReason": reason})));
    }
    // An error on the prompt, and an answer without a reason.
    script.push(expect_prompt(9, "go"));
    script.push(write(
        json!({"jsonrpc": "2.0", "id": 9, "error": {"code": -32603, "message": "the model is down"}}),
    ));
    script.push(expect_prompt(10, "go"));
    script.push(reply(10, json!({})));
    let provider = started(script, None);

    let mut ends = Vec::new();
    for _ in 0..8 {
        send(&provider, "go").expect("sends");
        let events = until_completed(&provider);
        let (outcome, stop, reason, _) = turn_end(&events);
        let message = events.iter().find_map(|e| match e {
            AgentDomainEvent::TurnCompleted { detail, .. } => detail.message.clone(),
            _ => None,
        });
        ends.push((outcome, stop, reason, message.is_some()));
    }
    let some = |s: &str| Some(s.to_string());
    assert_eq!(
        ends,
        vec![
            (TurnOutcome::Completed, some("end_turn"), None, false),
            (TurnOutcome::LimitReached, some("max_tokens"), some("max_tokens"), false),
            (
                TurnOutcome::LimitReached,
                some("max_turn_requests"),
                some("max_turns"),
                false
            ),
            (TurnOutcome::Failed, some("refusal"), some("refusal"), true),
            (TurnOutcome::Interrupted, some("cancelled"), None, false),
            (TurnOutcome::Failed, some("tool_budget"), some("tool_budget"), true),
            (TurnOutcome::Failed, None, some("agent_error"), true),
            (TurnOutcome::Failed, None, some("no_stop_reason"), true),
        ]
    );
}

#[test]
fn the_error_of_a_prompt_is_the_turns_message() {
    let mut script = boot("agent");
    script.push(expect_prompt(3, "go"));
    script.push(write(
        json!({"jsonrpc": "2.0", "id": 3, "error": {"code": -32603, "message": "the model is down"}}),
    ));
    let provider = started(script, None);
    send(&provider, "go").expect("sends");
    let events = until_completed(&provider);
    let message = events.iter().find_map(|e| match e {
        AgentDomainEvent::TurnCompleted { detail, .. } => detail.message.clone(),
        _ => None,
    });
    assert_eq!(message.as_deref(), Some("the model is down"));
}

// ---------------------------------------------------------------- permissions

fn options_with_always() -> Value {
    json!([
        {"optionId": "a-always", "name": "Yes, always", "kind": "allow_always"},
        {"optionId": "a-1", "name": "Yes, once", "kind": "allow_once"},
        {"optionId": "a-2", "name": "Yes, a second way", "kind": "allow_once"},
        {"optionId": "r-always", "name": "No, never", "kind": "reject_always"},
        {"optionId": "r-1", "name": "No, and tell the agent", "kind": "reject_once"},
        {"optionId": "r-2", "name": "No, another way", "kind": "reject_once"}
    ])
}

#[test]
fn permission_selects_first_allow_once_and_never_always() {
    let mut script = boot("agent");
    script.extend([
        expect_prompt(3, "touch"),
        agent_request(
            7,
            "session/request_permission",
            permission_params("c1", options_with_always()),
        ),
        expect_answer(7, json!({"outcome": {"outcome": "selected", "optionId": "a-1"}})),
        agent_request(
            8,
            "session/request_permission",
            permission_params("c2", options_with_always()),
        ),
        expect_answer(8, json!({"outcome": {"outcome": "selected", "optionId": "r-1"}})),
        reply(3, json!({"stopReason": "end_turn"})),
    ]);
    let provider = started(script, None);
    send(&provider, "touch").expect("sends");

    let (first, events) = until_permission(&provider);
    // The call gets a row, and the card names only what can be answered, by the agent's own words.
    assert!(shapes(&events).contains(&"tool c1 acp:execute".to_string()));
    let card = events
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::PermissionRequested {
                tool_use_id,
                tool_name,
                input,
                ..
            } => Some((tool_use_id.clone(), tool_name.clone(), input.clone())),
            _ => None,
        })
        .expect("a card");
    assert_eq!(card.0.as_deref(), Some("c1"));
    assert_eq!(card.1, "acp:execute");
    assert_eq!(
        card.2["options"],
        json!([
            {"kind": "allow_once", "name": "Yes, once"},
            {"kind": "allow_once", "name": "Yes, a second way"},
            {"kind": "reject_once", "name": "No, and tell the agent"},
            {"kind": "reject_once", "name": "No, another way"}
        ])
    );
    // No option id and no `*_always` option reaches the host.
    let shown = format!("{events:?}");
    for hidden in ["a-always", "r-always", "Yes, always", "No, never", "a-1", "r-1"] {
        assert!(!shown.contains(hidden), "{hidden} reached the host: {shown}");
    }
    allow(&provider, &first).expect("approves");

    let (second, _) = until_permission(&provider);
    deny(&provider, &second).expect("refuses");
    let events = until_completed(&provider);
    assert_eq!(turn_end(&events).0, TurnOutcome::Completed);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentDomainEvent::PermissionResolved {
            outcome: PermissionOutcome::Denied,
            ..
        }
    )));
    // A card that was answered is gone.
    assert!(matches!(
        allow(&provider, &first),
        Err(ProviderError::Provider {
            code: ProviderErrorCode::PermissionNotFound,
            ..
        })
    ));
}

#[test]
fn no_allow_once_cannot_be_approved() {
    let mut script = boot("agent");
    script.extend([
        expect_prompt(3, "touch"),
        // Only an `allow_always`: it must not be chosen in the place of an `allow_once`.
        agent_request(
            7,
            "session/request_permission",
            permission_params(
                "c1",
                json!([
                    {"optionId": "a-always", "name": "Always", "kind": "allow_always"},
                    {"optionId": "r-1", "name": "No", "kind": "reject_once"}
                ]),
            ),
        ),
        expect_answer(7, json!({"outcome": {"outcome": "selected", "optionId": "r-1"}})),
        // And a request that cannot be refused.
        agent_request(
            8,
            "session/request_permission",
            permission_params("c2", json!([{"optionId": "a-1", "name": "Yes", "kind": "allow_once"}])),
        ),
        expect_answer(8, json!({"outcome": {"outcome": "selected", "optionId": "a-1"}})),
        reply(3, json!({"stopReason": "end_turn"})),
    ]);
    let provider = started(script, None);
    send(&provider, "touch").expect("sends");

    let (first, _) = until_permission(&provider);
    let err = allow(&provider, &first).expect_err("cannot approve");
    assert!(err.is_benign(), "{err}");
    assert!(
        err.to_string()
            .contains("offers no way for this request to be approved"),
        "{err}"
    );
    // The card stays; the option that exists still works.
    deny(&provider, &first).expect("refuses");

    let (second, _) = until_permission(&provider);
    let err = deny(&provider, &second).expect_err("cannot refuse");
    assert!(err.is_benign(), "{err}");
    allow(&provider, &second).expect("approves");
    until_completed(&provider);
}

#[test]
fn defer_is_refused() {
    let mut script = boot("agent");
    script.extend([
        expect_prompt(3, "x"),
        agent_request(
            7,
            "session/request_permission",
            permission_params("c1", options_with_always()),
        ),
    ]);
    let provider = started(script, None);
    send(&provider, "x").expect("sends");
    let (id, _) = until_permission(&provider);
    assert!(matches!(
        resolve(&provider, &id, PermissionDecision::Defer),
        Err(ProviderError::UnsupportedCapability("defer"))
    ));
}

#[test]
fn cancel_answers_open_requests_cancelled() {
    let mut script = boot("agent");
    script.extend([
        expect_prompt(3, "touch"),
        agent_request(
            7,
            "session/request_permission",
            permission_params("c1", options_with_always()),
        ),
        expect_notification("session/cancel", json!({"sessionId": "s1"})),
        expect_answer(7, json!({"outcome": {"outcome": "cancelled"}})),
        // A request made after the cancel is answered at once, without a card.
        agent_request(
            8,
            "session/request_permission",
            permission_params("c2", options_with_always()),
        ),
        expect_answer(8, json!({"outcome": {"outcome": "cancelled"}})),
        reply(3, json!({"stopReason": "cancelled"})),
    ]);
    let provider = started(script, None);
    // Nothing runs yet.
    assert!(matches!(
        provider.interrupt_turn(InterruptTurnRequest {
            session_id: "s1".to_string()
        }),
        Err(ProviderError::Provider {
            code: ProviderErrorCode::NoActiveTurn,
            ..
        })
    ));
    send(&provider, "touch").expect("sends");
    let (id, _) = until_permission(&provider);
    provider
        .interrupt_turn(InterruptTurnRequest {
            session_id: "s1".to_string(),
        })
        .expect("interrupts");
    let events = until_completed(&provider);
    let resolved: Vec<&AgentDomainEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentDomainEvent::PermissionResolved { .. }))
        .collect();
    assert_eq!(resolved.len(), 1, "{events:#?}");
    assert!(matches!(
        resolved[0],
        AgentDomainEvent::PermissionResolved { permission_id, outcome: PermissionOutcome::CancelledByInterrupt }
            if *permission_id == id
    ));
    // No card for the request that came after, and the turn ended the way the agent said.
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. })));
    assert_eq!(turn_end(&events).0, TurnOutcome::Interrupted);
    // The script ran to its end: a mismatch anywhere would have ended the child and been reported.
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. })));
}

#[test]
fn a_request_the_provider_does_not_know_gets_method_not_found() {
    let mut script = boot("agent");
    script.extend([
        expect_prompt(3, "x"),
        agent_request(
            5,
            "fs/read_text_file",
            json!({"sessionId": "s1", "path": "/etc/passwd"}),
        ),
        expect_exact(r#"{"jsonrpc":"2.0","id":5,"error":{"code":-32601,"message":"method not found"}}"#),
        agent_request(6, "terminal/create", json!({"sessionId": "s1", "command": "ls"})),
        expect_exact(r#"{"jsonrpc":"2.0","id":6,"error":{"code":-32601,"message":"method not found"}}"#),
        reply(3, json!({"stopReason": "end_turn"})),
    ]);
    let provider = started(script, None);
    send(&provider, "x").expect("sends");
    // The turn ends only if the child saw both refusals: a missing one ends the child instead.
    let events = until_completed(&provider);
    assert_eq!(turn_end(&events).0, TurnOutcome::Completed);
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. })));
}

/// An agent that ends right after answering `session/new`: the link's end can be read in the same pass
/// that took the answer, before the session is open. It must still end the session; dropped there, the
/// tab looked open and its first turn never ended.
#[test]
fn an_agent_that_ends_right_after_opening_the_session_ends_the_session() {
    for round in 0..5 {
        let mut script = handshake(false);
        script.extend([
            expect_request(2, "session/new", json!({"cwd": "/work", "mcpServers": []})),
            sleep_ms(20),
            reply(
                2,
                json!({"sessionId": "s1", "modes": {"currentModeId": "agent", "availableModes": []}}),
            ),
            json!({"exit": 0}),
        ]);
        let provider = connect(script, None).expect("connects");
        if provider.start(start_request(None)).is_err() {
            // The end was seen before the answer: the start failed, which is honest too.
            continue;
        }
        let events = pump_until(&provider, "the session's end", |e| {
            matches!(e, AgentDomainEvent::SessionUnavailable { .. })
        });
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. })),
            "round {round}: {events:#?}"
        );
    }
}

// ---------------------------------------------------------------- modes

#[test]
fn an_unreadable_current_mode_report_trips_a_registered_mode() {
    unreadable_mode_report(json!({"sessionUpdate": "current_mode_update"}));
}

#[test]
fn an_unreadable_mode_option_trips_a_registered_mode() {
    unreadable_mode_report(json!({"sessionUpdate": "config_option_update", "configOptions": [
        {"id": "mode", "currentValue": 5}
    ]}));
}

fn unreadable_mode_report(report: Value) {
    let mut script = boot("workspace-write");
    script.extend([expect_prompt(3, "x"), update("s1", report.clone())]);
    let provider = started(script, Some("workspace-write"));
    send(&provider, "x").expect("sends");
    let events = pump_until(&provider, "the unreadable mode to be refused", |e| {
        matches!(e, AgentDomainEvent::PolicyMismatch { .. })
    });
    assert!(events.iter().any(|e| matches!(
        e,
        AgentDomainEvent::PolicyMismatch { requested, reported }
            if requested == "mode workspace-write" && reported.contains("could not read")
    )));
    assert!(send(&provider, "again").is_err(), "the session takes no more turns");

    let mut script = boot("agent");
    script.extend([
        expect_prompt(3, "x"),
        update("s1", report),
        reply(3, json!({"stopReason": "end_turn"})),
    ]);
    let provider = started(script, None);
    send(&provider, "x").expect("sends without a registered mode");
    let events = until_completed(&provider);
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentDomainEvent::PolicyMismatch { .. })));
}

#[test]
fn an_unreadable_mode_report_after_setting_the_mode_opens_and_trips() {
    for report in [
        json!({"sessionUpdate": "current_mode_update"}),
        json!({"sessionUpdate": "config_option_update", "configOptions": [
            {"id": "agent-mode", "category": "mode", "currentValue": 5}
        ]}),
    ] {
        let mut script = boot("agent");
        script.extend([
            expect_request(
                3,
                "session/set_mode",
                json!({"sessionId": "s1", "modeId": "workspace-write"}),
            ),
            update(
                "s1",
                json!({"sessionUpdate": "current_mode_update", "currentModeId": "workspace-write"}),
            ),
            update("s1", report),
            reply(3, json!({})),
        ]);
        let provider = started(script, Some("workspace-write"));
        let events = provider.pump();
        let opened = events
            .iter()
            .position(|e| matches!(e, AgentDomainEvent::SessionOpened { .. }));
        let tripped = events.iter().position(|e| {
            matches!(
                e,
                AgentDomainEvent::PolicyMismatch { reported, .. } if reported.contains("could not read")
            )
        });
        assert!(opened.is_some() && tripped.is_some() && opened < tripped, "{events:#?}");
        assert!(send(&provider, "x").is_err());
    }
}

/// ACP has no sandbox to ask for and Eitri cannot tell what a mode means, so only a registration can say which one is
/// read-only. With none, the agent's own rules stand, as the window's note says.
#[test]
fn the_read_only_mode_stands_for_the_registered_one_at_a_root_that_is_no_boundary() {
    let named = AcpAgentSpec {
        name: "stand-in".to_string(),
        argv: Vec::new(),
        auto_mode: Some("auto".to_string()),
        read_only_mode: Some("plan".to_string()),
    };
    let mode_at = |spec: &AcpAgentSpec, boundary: bool| spec.clone().at_root(boundary).auto_mode;
    assert_eq!(mode_at(&named, true).as_deref(), Some("auto"));
    assert_eq!(mode_at(&named, false).as_deref(), Some("plan"));
    let unnamed = AcpAgentSpec {
        read_only_mode: None,
        ..named.clone()
    };
    assert_eq!(mode_at(&unnamed, false).as_deref(), Some("auto"));
    let only_read_only = AcpAgentSpec {
        auto_mode: None,
        ..named
    };
    assert_eq!(mode_at(&only_read_only, true), None);
    assert_eq!(mode_at(&only_read_only, false).as_deref(), Some("plan"));
}

/// Set at the start and held to from then on, by the path `auto_mode` takes: an agent that leaves it stops the tab.
#[test]
fn a_read_only_mode_is_set_and_held_like_the_registered_one() {
    let mut script = boot("agent");
    script.extend([
        expect_request(3, "session/set_mode", json!({"sessionId": "s1", "modeId": "plan"})),
        reply(3, json!({})),
        update(
            "s1",
            json!({"sessionUpdate": "current_mode_update", "currentModeId": "auto"}),
        ),
    ]);
    let at_such_a_root = AcpAgentSpec {
        read_only_mode: Some("plan".to_string()),
        ..spec(script, Some("auto"))
    }
    .at_root(false);
    let provider = AcpProvider::connect(at_such_a_root, std::env::temp_dir()).expect("connects");
    provider.start(start_request(None)).expect("starts");
    assert_eq!(provider.auto_mode(), Some("plan"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let events = provider.pump();
        if events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PolicyMismatch { .. }))
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the agent left the mode unnoticed"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(send(&provider, "x").is_err());
}

#[test]
fn mode_mismatch_trips() {
    // The registered mode is set when the session is in another, and a later report of a different mode
    // stops the session.
    let mut script = boot("agent");
    script.extend([
        expect_request(
            3,
            "session/set_mode",
            json!({"sessionId": "s1", "modeId": "workspace-write"}),
        ),
        reply(3, json!({})),
        expect_prompt(4, "x"),
        update(
            "s1",
            json!({"sessionUpdate": "current_mode_update", "currentModeId": "workspace-write"}),
        ),
        update(
            "s1",
            json!({"sessionUpdate": "current_mode_update", "currentModeId": "agent-full-access"}),
        ),
    ]);
    let provider = started(script, Some("workspace-write"));
    assert_eq!(provider.auto_mode(), Some("workspace-write"));
    send(&provider, "x").expect("sends");
    let events = pump_until(&provider, "the mode to be refused", |e| {
        matches!(e, AgentDomainEvent::PolicyMismatch { .. })
    });
    assert!(events.iter().any(|e| matches!(
        e,
        AgentDomainEvent::PolicyMismatch { requested, reported }
            if requested == "mode workspace-write" && reported == "mode agent-full-access"
    )));
    // No turn starts after it.
    let err = send(&provider, "again").expect_err("blocked");
    assert!(matches!(
        err,
        ProviderError::Provider {
            code: ProviderErrorCode::ProviderUnavailable,
            ..
        }
    ));

    // The agent refuses the change: the session is stopped as soon as it opens.
    let mut script = boot("agent");
    script.extend([
        expect_request(
            3,
            "session/set_mode",
            json!({"sessionId": "s1", "modeId": "workspace-write"}),
        ),
        write(json!({"jsonrpc": "2.0", "id": 3, "error": {"code": -32602, "message": "unknown mode"}})),
    ]);
    let provider = started(script, Some("workspace-write"));
    let events = provider.pump();
    assert!(events.iter().any(|e| matches!(
        e,
        AgentDomainEvent::PolicyMismatch { requested, reported }
            if requested == "mode workspace-write" && reported.contains("mode agent") && reported.contains("unknown mode")
    )));
    assert!(send(&provider, "x").is_err());

    // A config option that holds the mode counts as a report of it.
    let mut script = boot("workspace-write");
    script.extend([
        expect_prompt(3, "x"),
        update(
            "s1",
            json!({"sessionUpdate": "config_option_update", "configOptions": [
                {"id": "model", "category": "model", "currentValue": "other"},
                {"id": "mode", "category": "mode", "currentValue": "agent"}
            ]}),
        ),
    ]);
    let provider = started(script, Some("workspace-write"));
    send(&provider, "x").expect("sends: the mode is already the one asked for");
    pump_until(&provider, "the option to be refused", |e| {
        matches!(e, AgentDomainEvent::PolicyMismatch { .. })
    });

    // With no registered mode nothing is set and no report is judged.
    let mut script = boot("agent");
    script.extend([
        expect_prompt(3, "x"),
        update(
            "s1",
            json!({"sessionUpdate": "current_mode_update", "currentModeId": "whatever"}),
        ),
        reply(3, json!({"stopReason": "end_turn"})),
    ]);
    let provider = started(script, None);
    send(&provider, "x").expect("sends");
    let events = until_completed(&provider);
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentDomainEvent::PolicyMismatch { .. })));
}

// ---------------------------------------------------------------- load

fn load_expect() -> Value {
    expect_request(
        2,
        "session/load",
        json!({"sessionId": "saved-1", "cwd": "/work", "mcpServers": []}),
    )
}

fn replay() -> Vec<Value> {
    let chunk = |kind: &str, id: &str, text: &str| {
        update(
            "saved-1",
            json!({"sessionUpdate": kind, "messageId": id, "content": {"type": "text", "text": text}}),
        )
    };
    vec![
        update(
            "saved-1",
            json!({"sessionUpdate": "available_commands_update", "availableCommands": []}),
        ),
        chunk("user_message_chunk", "u1", "Hel"),
        chunk("user_message_chunk", "u1", "lo"),
        chunk("agent_message_chunk", "a1", "Hi"),
        update(
            "saved-1",
            json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "kind": "read", "title": "Read a",
                   "status": "completed", "locations": [{"path": "/work/a"}]}),
        ),
        chunk("user_message_chunk", "u2", "Second"),
        chunk("agent_message_chunk", "a2", "Ok"),
        update("saved-1", json!({"sessionUpdate": "session_info_update", "title": "t"})),
    ]
}

#[test]
fn load_replays_then_checks_mode() {
    let mut script = handshake(true);
    script.push(load_expect());
    script.extend(replay());
    // The load answers after the replay, reporting a mode other than the one registered.
    script.push(reply(
        2,
        json!({"modes": {"currentModeId": "agent", "availableModes": []}}),
    ));
    script.push(expect_request(
        3,
        "session/set_mode",
        json!({"sessionId": "saved-1", "modeId": "workspace-write"}),
    ));
    script.push(reply(3, json!({})));
    let provider = connect(script, Some("workspace-write")).expect("connects");
    assert_eq!(
        provider.start(start_request(Some("saved-1"))).expect("loads"),
        "saved-1"
    );

    // The replay is the history, not the stream.
    let live = provider.pump();
    assert!(
        matches!(
            live.as_slice(),
            [
                AgentDomainEvent::SessionOpened { session_id, .. },
                AgentDomainEvent::ResumeOutcome { status: ResumeStatus::Attached, attached_provider_session_id, .. }
            ] if session_id == "saved-1" && attached_provider_session_id.as_deref() == Some("saved-1")
        ),
        "{live:#?}"
    );
    let history = provider.take_history();
    assert_eq!(
        shapes(&history),
        vec![
            "user Hello",
            "started acp-history-1",
            "text Hi",
            "tool t1 acp:read",
            "tool-done t1  false",
            "completed acp-history-1 Completed Hi",
            "user Second",
            "started acp-history-2",
            "text Ok",
            "completed acp-history-2 Completed Ok",
        ]
    );
    // Handed over once.
    assert!(provider.take_history().is_empty());
    // The mode was set, so no mismatch: a turn can start (the script has nothing more, so only the send
    // is checked).
    assert!(send_to(&provider, "saved-1", "go").is_ok());
}

fn send_to(provider: &AcpProvider, session: &str, text: &str) -> Result<String, ProviderError> {
    provider.send_turn_with(
        SendTurnRequest {
            session_id: session.to_string(),
            text: text.to_string(),
        },
        TurnPolicy {
            reviewer: Reviewer::Agent,
            sandbox: agent::Sandbox::Project,
        },
    )
}

#[test]
fn a_loaded_session_in_the_wrong_mode_keeps_its_history_and_trips() {
    let mut script = handshake(true);
    script.push(load_expect());
    script.extend(replay());
    script.push(reply(
        2,
        json!({"modes": {"currentModeId": "agent", "availableModes": []}}),
    ));
    script.push(expect_request(
        3,
        "session/set_mode",
        json!({"sessionId": "saved-1", "modeId": "workspace-write"}),
    ));
    script.push(write(
        json!({"jsonrpc": "2.0", "id": 3, "error": {"code": -32602, "message": "no"}}),
    ));
    let provider = connect(script, Some("workspace-write")).expect("connects");
    provider.start(start_request(Some("saved-1"))).expect("loads");
    let live = provider.pump();
    assert!(live
        .iter()
        .any(|e| matches!(e, AgentDomainEvent::PolicyMismatch { .. })));
    assert!(!provider.take_history().is_empty());
    assert!(send_to(&provider, "saved-1", "go").is_err());
}

#[test]
fn an_agent_that_cannot_load_rejects_the_resume() {
    let provider = connect(handshake(false), None).expect("connects");
    assert!(!provider.resumes());
    assert_eq!(
        provider.start(start_request(Some("saved-1"))).expect("opens"),
        "saved-1"
    );
    let events = provider.pump();
    assert!(events.iter().any(|e| matches!(
        e,
        AgentDomainEvent::ResumeOutcome { status: ResumeStatus::Rejected, detail: Some(d), .. }
            if d.contains("cannot load")
    )));
    assert!(send_to(&provider, "saved-1", "go").is_err());
}

#[test]
fn a_refused_load_is_the_agents_own_verdict() {
    let mut script = handshake(true);
    script.push(load_expect());
    script.push(write(
        json!({"jsonrpc": "2.0", "id": 2, "error": {"code": -32002, "message": "session not found"}}),
    ));
    let provider = connect(script, None).expect("connects");
    assert_eq!(
        provider.start(start_request(Some("saved-1"))).expect("opens"),
        "saved-1"
    );
    let events = provider.pump();
    assert!(events.iter().any(|e| matches!(
        e,
        AgentDomainEvent::ResumeOutcome { status: ResumeStatus::Rejected, detail: Some(d), .. }
            if d == "session not found"
    )));
    assert!(provider.take_history().is_empty());
}

// ---------------------------------------------------------------- the plain doors

#[test]
fn the_plain_claude_shaped_calls_refuse() {
    let provider = connect(handshake(false), None).expect("connects");
    assert!(matches!(
        provider.send_turn(SendTurnRequest {
            session_id: "s".into(),
            text: "x".into()
        }),
        Err(ProviderError::UnsupportedCapability("external"))
    ));
    // Closing a provider that never started a session is quiet.
    provider
        .close_session(CloseSessionRequest { session_id: "s".into() })
        .expect("closes");
    assert!(provider
        .pump()
        .iter()
        .any(|e| matches!(e, AgentDomainEvent::SessionClosed { .. })));
}

// ---------------------------------------------------------------- partial updates and long inputs

#[test]
fn updates_to_a_call_are_merged_and_a_change_waits_for_its_file() {
    let mut script = boot("agent");
    script.extend([
        expect_prompt(3, "edit"),
        // A change that names no file yet: no row, since the row's input is fixed when it starts.
        update(
            "s1",
            json!({"sessionUpdate": "tool_call", "toolCallId": "e1", "kind": "edit", "title": "Edit a.txt",
                   "status": "pending"}),
        ),
        update(
            "s1",
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "e1", "status": "in_progress",
                   "locations": [{"path": "/work/a.txt"}],
                   "content": [{"type": "diff", "path": "/work/a.txt", "oldText": "a", "newText": "b"}]}),
        ),
        update(
            "s1",
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "e1",
                   "content": [{"type": "content", "content": {"type": "text", "text": "wrote it"}}]}),
        ),
        // The end carries only a status: the output the earlier updates brought is the result.
        update(
            "s1",
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "e1", "status": "completed"}),
        ),
        // A command is shown at once, and a change that ends without ever naming a file still gets its row.
        update(
            "s1",
            json!({"sessionUpdate": "tool_call", "toolCallId": "x1", "kind": "execute", "title": "Run",
                   "status": "pending"}),
        ),
        update(
            "s1",
            json!({"sessionUpdate": "tool_call", "toolCallId": "e2", "kind": "edit", "title": "Edit b",
                   "status": "pending"}),
        ),
        update(
            "s1",
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "e2", "status": "failed"}),
        ),
        reply(3, json!({"stopReason": "end_turn"})),
    ]);
    let provider = started(script, None);
    send(&provider, "edit").expect("sends");
    let events = until_completed(&provider);
    assert_eq!(
        shapes(&events),
        vec![
            "started acp-turn-1",
            "tool e1 acp:edit",
            "tool-done e1 wrote it false",
            "tool x1 acp:execute",
            "tool e2 acp:edit",
            "tool-done e2  true",
            "completed acp-turn-1 Completed ",
        ]
    );
    let first = events
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::ToolCallStarted { input, .. } => Some(input.clone()),
            _ => None,
        })
        .expect("a row");
    assert_eq!(first["title"], "Edit a.txt");
    assert_eq!(first["locations"], json!([{"path": "/work/a.txt"}]));
    assert_eq!(
        first["diffs"],
        json!([{"path": "/work/a.txt", "oldText": "a", "newText": "b"}])
    );
}

#[test]
fn a_request_whose_input_would_be_cut_cannot_be_approved() {
    let mut script = boot("agent");
    let long = format!("{}; touch the-end", "x".repeat(300 * 1024));
    script.extend([
        expect_prompt(3, "touch"),
        agent_request(
            7,
            "session/request_permission",
            json!({
                "sessionId": "s1",
                "toolCall": {"toolCallId": "c1", "title": "Run command", "kind": "execute", "status": "pending",
                             "rawInput": {"command": long}},
                "options": [
                    {"optionId": "a-1", "name": "Yes", "kind": "allow_once"},
                    {"optionId": "r-1", "name": "No", "kind": "reject_once"}
                ]
            }),
        ),
        expect_answer(7, json!({"outcome": {"outcome": "selected", "optionId": "r-1"}})),
        reply(3, json!({"stopReason": "end_turn"})),
    ]);
    // Too long for an argument: the stand-in reads it from a file.
    let file = std::env::temp_dir().join(format!("eitri-acp-long-input-{}.json", std::process::id()));
    std::fs::write(&file, Value::Array(script).to_string()).expect("writes the script");
    let provider = AcpProvider::connect(
        AcpAgentSpec {
            name: "stand-in".to_string(),
            argv: vec![
                OsString::from(standin()),
                OsString::from(format!("@{}", file.display())),
            ],
            auto_mode: None,
            read_only_mode: None,
        },
        std::env::temp_dir(),
    )
    .expect("connects");
    provider.start(start_request(None)).expect("starts");
    send(&provider, "touch").expect("sends");
    let (id, events) = until_permission(&provider);
    let input = events
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::PermissionRequested { input, .. } => Some(input.clone()),
            _ => None,
        })
        .expect("a card");
    // The card says what it left out and offers no approval.
    assert_eq!(input["inputCut"], json!(true));
    assert_eq!(input["options"], json!([{"kind": "reject_once", "name": "No"}]));
    let err = allow(&provider, &id).expect_err("cannot approve what is not shown");
    assert!(err.is_benign(), "{err}");
    // The refusal names Eitri's own reason: the agent did offer an allow-once option.
    assert!(
        err.to_string().contains("Eitri held back stand-in's allow-once option"),
        "{err}"
    );
    deny(&provider, &id).expect("can refuse");
    until_completed(&provider);
    let _ = std::fs::remove_file(&file);
}

#[test]
fn a_request_whose_diff_would_not_be_shown_whole_cannot_be_approved() {
    // No raw input at all: only the edit's own diff, too large for a card to draw whole.
    let mut script = boot("agent");
    let old_text = "line\n".repeat(60 * 1024);
    script.extend([
        expect_prompt(3, "edit"),
        agent_request(
            7,
            "session/request_permission",
            json!({
                "sessionId": "s1",
                "toolCall": {"toolCallId": "c1", "title": "Edit a.txt", "kind": "edit", "status": "pending",
                             "content": [{"type": "diff", "path": "/work/a.txt", "oldText": old_text, "newText": ""}]},
                "options": [
                    {"optionId": "a-1", "name": "Yes", "kind": "allow_once"},
                    {"optionId": "r-1", "name": "No", "kind": "reject_once"}
                ]
            }),
        ),
        expect_answer(7, json!({"outcome": {"outcome": "selected", "optionId": "r-1"}})),
        reply(3, json!({"stopReason": "end_turn"})),
    ]);
    let file = std::env::temp_dir().join(format!("eitri-acp-long-diff-{}.json", std::process::id()));
    std::fs::write(&file, Value::Array(script).to_string()).expect("writes the script");
    let provider = AcpProvider::connect(
        AcpAgentSpec {
            name: "stand-in".to_string(),
            argv: vec![
                OsString::from(standin()),
                OsString::from(format!("@{}", file.display())),
            ],
            auto_mode: None,
            read_only_mode: None,
        },
        std::env::temp_dir(),
    )
    .expect("connects");
    provider.start(start_request(None)).expect("starts");
    send(&provider, "edit").expect("sends");
    let (id, events) = until_permission(&provider);
    let input = events
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::PermissionRequested { input, .. } => Some(input.clone()),
            _ => None,
        })
        .expect("a card");
    assert_eq!(input["inputCut"], json!(true));
    assert_eq!(input["options"], json!([{"kind": "reject_once", "name": "No"}]));
    let err = allow(&provider, &id).expect_err("cannot approve what is not shown");
    assert!(err.is_benign(), "{err}");
    // The refusal names Eitri's own reason: the agent did offer an allow-once option.
    assert!(
        err.to_string().contains("Eitri held back stand-in's allow-once option"),
        "{err}"
    );
    deny(&provider, &id).expect("can refuse");
    until_completed(&provider);
    let _ = std::fs::remove_file(&file);
}

#[test]
fn a_pump_during_the_start_cannot_take_the_modes_report() {
    // The agent reports another mode just ahead of its answer to `session/set_mode`. A host that pumps
    // while the start is running must not read that report away from the start's own check.
    let mut script = boot("agent");
    script.extend([
        expect_request(
            3,
            "session/set_mode",
            json!({"sessionId": "s1", "modeId": "workspace-write"}),
        ),
        sleep_ms(50),
        update(
            "s1",
            json!({"sessionUpdate": "current_mode_update", "currentModeId": "agent-full-access"}),
        ),
        sleep_ms(50),
        reply(3, json!({})),
    ]);
    let provider = std::sync::Arc::new(connect(script, Some("workspace-write")).expect("connects"));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pumper = {
        let (provider, stop) = (provider.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                seen.extend(provider.pump());
                std::thread::sleep(Duration::from_millis(1));
            }
            seen
        })
    };
    provider.start(start_request(None)).expect("starts");
    let mut events = provider.pump();
    std::thread::sleep(Duration::from_millis(20));
    events.extend(provider.pump());
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    events.extend(pumper.join().expect("the pumping thread ends"));
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentDomainEvent::PolicyMismatch { reported, .. } if reported == "mode agent-full-access"
        )),
        "the report was lost: {events:#?}"
    );
}

fn sleep_ms(ms: u64) -> Value {
    json!({ "sleep_ms": ms })
}

// ---------------------------------------------------------------- a request too large to read

/// The notes Eitri put on rows for requests it answered itself, by row.
fn answered_by_eitri(events: &[AgentDomainEvent]) -> Vec<(Option<String>, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::PermissionDenied {
                tool_use_id,
                reason_type: Some(kind),
                reason,
                ..
            } if kind == "eitri" => Some((tool_use_id.clone(), reason.clone().unwrap_or_default())),
            _ => None,
        })
        .collect()
}

/// A permission request larger than 16 MiB cannot be read or shown, so it is answered `cancelled` (ACP's
/// own refusal, never a Codex-shaped decline or an error) and the call's row says Eitri answered it. One
/// whose call id is past the bytes Eitri reads gets a row of its own. The turn goes on.
#[test]
fn an_over_cap_permission_request_is_cancelled_with_a_note_on_its_row() {
    let big = agent::jsonrpc_link::MAX_LINE + 1;
    let mut script = boot("agent");
    script.extend([
        expect_prompt(3, "touch"),
        update("s1", tool_call("c9", "execute", "Run command")),
        json!({"write_big": {
            "prefix": r#"{"jsonrpc":"2.0","id":7,"method":"session/request_permission","params":{"sessionId":"s1","toolCall":{"toolCallId":"c9","rawInput":{"command":""#,
            "bytes": big,
            "suffix": r#""}},"options":[]}}"#,
        }}),
        expect_answer(7, json!({"outcome": {"outcome": "cancelled"}})),
        json!({"write_big": {
            "prefix": r#"{"jsonrpc":"2.0","id":8,"method":"session/request_permission","params":{"sessionId":"s1","options":[{"name":""#,
            "bytes": big,
            "suffix": r#""}]}}"#,
        }}),
        expect_answer(8, json!({"outcome": {"outcome": "cancelled"}})),
        reply(3, json!({"stopReason": "end_turn"})),
    ]);
    let provider = started(script, None);
    send(&provider, "touch").expect("sends");
    let events = until_completed(&provider);
    assert_eq!(turn_end(&events).0, TurnOutcome::Completed);
    let notes = answered_by_eitri(&events);
    assert_eq!(notes.len(), 2, "{events:#?}");
    assert_eq!(notes[0].0.as_deref(), Some("c9"));
    let added = notes[1].0.clone().expect("a row");
    assert_ne!(added, "c9");
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentDomainEvent::ToolCallStarted { tool_use_id, .. } if *tool_use_id == added
        )),
        "the note sits on no row: {events:#?}"
    );
    for (_, note) in &notes {
        assert!(note.contains("larger than 16 MiB"), "{note}");
        assert!(note.contains("cancelled"), "{note}");
    }
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. })),
        "a request nobody could read became a card"
    );
}

/// A permission request whose session id comes after the bytes Eitri reads may be another session's: its call
/// id is not trusted to name a row of this one, and the note goes on a row Eitri adds.
#[test]
fn an_over_cap_permission_request_naming_no_session_does_not_mark_a_call_of_this_one() {
    let big = agent::jsonrpc_link::MAX_LINE + 1;
    let mut script = boot("agent");
    script.extend([
        expect_prompt(3, "touch"),
        update("s1", tool_call("c9", "execute", "Run command")),
        json!({"write_big": {
            "prefix": r#"{"jsonrpc":"2.0","id":7,"method":"session/request_permission","params":{"toolCall":{"toolCallId":"c9","rawInput":{"command":""#,
            "bytes": big,
            "suffix": r#""}},"sessionId":"s2","options":[]}}"#,
        }}),
        expect_answer(7, json!({"outcome": {"outcome": "cancelled"}})),
        reply(3, json!({"stopReason": "end_turn"})),
    ]);
    let provider = started(script, None);
    send(&provider, "touch").expect("sends");
    let events = until_completed(&provider);
    let notes = answered_by_eitri(&events);
    assert_eq!(notes.len(), 1, "{events:#?}");
    assert_ne!(
        notes[0].0.as_deref(),
        Some("c9"),
        "another session's request marked this session's call"
    );
}

// ---------------------------------------------------------------- an agent that ends on its own

/// An agent whose output ends while its stderr stays open and says nothing yet. The card it asked for
/// goes, and an answer to it from another thread (the UI's) returns while stderr is still open: the
/// notice of the end waits for stderr without holding the provider. The pump that reads the end carries
/// no notice yet; once stderr closes, the next pumps send it.
#[test]
fn a_card_answer_does_not_wait_for_a_closed_agents_stderr() {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let gate = std::env::temp_dir().join(format!("eitri-acp-stderr-gate-{}-{stamp}", std::process::id()));
    let mut script = boot("agent");
    script.extend([
        expect_prompt(3, "touch"),
        agent_request(
            7,
            "session/request_permission",
            permission_params("c1", options_with_always()),
        ),
        json!({"exit": 0}),
    ]);
    // A shell runs the stand-in, then closes its own stdout (the stand-in's was the same pipe) and holds
    // stderr open and empty until the gate file appears; the wait is capped so a failed test leaves no
    // child behind.
    let wrapper = r#""$0" "$1"
exec >&-
i=0
while [ ! -e "$2" ] && [ "$i" -lt 1000 ]; do sleep 0.01; i=$((i+1)); done
echo "stand-in: gone" >&2
"#;
    let agent = AcpAgentSpec {
        name: "stand-in".to_string(),
        argv: vec![
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from(wrapper),
            OsString::from(standin()),
            OsString::from(Value::Array(script).to_string()),
            OsString::from(&gate),
        ],
        auto_mode: None,
        read_only_mode: None,
    };
    let provider = std::sync::Arc::new(AcpProvider::connect(agent, std::env::temp_dir()).expect("connects"));
    provider.start(start_request(None)).expect("starts");
    send(&provider, "touch").expect("sends");

    // Pump until the card the agent asked for is dropped: that pump read the end of its output.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut card = None;
    let mut before_stderr = Vec::new();
    loop {
        let batch = provider.pump();
        for event in &batch {
            match event {
                AgentDomainEvent::PermissionRequested { permission_id, .. } => card = Some(permission_id.clone()),
                AgentDomainEvent::PermissionResolved {
                    permission_id,
                    outcome: PermissionOutcome::ProviderFailed,
                } => assert_eq!(Some(permission_id), card.as_ref(), "{batch:#?}"),
                _ => {}
            }
        }
        let ended = batch.iter().any(|e| {
            matches!(
                e,
                AgentDomainEvent::PermissionResolved {
                    outcome: PermissionOutcome::ProviderFailed,
                    ..
                }
            )
        });
        before_stderr.extend(batch);
        if ended {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the agent's end was never read: {before_stderr:#?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let card = card.expect("a card was drawn");
    assert!(
        !before_stderr
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. })),
        "the notice was sent while stderr was still open and empty, so the pump waited for it: {before_stderr:#?}"
    );

    // The UI thread's answer, while the agent's stderr is still open.
    let (tx, rx) = std::sync::mpsc::channel();
    let answering = {
        let provider = provider.clone();
        std::thread::spawn(move || {
            let _ = tx.send(allow(&provider, &card));
        })
    };
    let answer = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("an answer to a card of an ended agent hung");
    answering.join().expect("the answering thread ends");
    assert!(
        matches!(
            answer,
            Err(ProviderError::Provider {
                code: ProviderErrorCode::ProviderUnavailable,
                ..
            })
        ),
        "an ended agent takes no answer: {answer:?}"
    );

    std::fs::write(&gate, b"").expect("the gate opens");
    let events = pump_until(&provider, "the notice of the agent's end", |e| {
        matches!(e, AgentDomainEvent::SessionUnavailable { .. })
    });
    let _ = std::fs::remove_file(&gate);
    let notices: Vec<&String> = events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::SessionUnavailable { reason } => Some(reason),
            _ => None,
        })
        .collect();
    assert_eq!(notices.len(), 1, "{events:#?}");
    assert!(
        notices[0].starts_with("stand-in ended unexpectedly (the agent closed its output)"),
        "{}",
        notices[0]
    );
}
