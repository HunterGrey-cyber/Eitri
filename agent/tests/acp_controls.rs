//! The model and effort controls of an ACP tab: what `AcpProvider` offers from the options the agent itself lists
//! (`configOptions`), and how a change is sent and answered. A scripted stand-in plays the agent; no real agent
//! runs and no model turn is spent. The option shapes follow `codex-acp`'s: a `mode` select, a `model` select
//! (category `model`) and a `reasoning_effort` select (category `thought_level`).

#[path = "support/script_gate.rs"]
mod script_gate;
use script_gate::until;

use agent::providers::acp::{AcpAgentSpec, AcpProvider};
use agent::{
    AgentDomainEvent, AgentProvider, DirTrust, ExternalProvider, ExternalStart, ProviderError, ProviderErrorCode,
    Reviewer, TrustPlan, TurnPolicy,
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

fn write(message: Value) -> Value {
    json!({ "write": message.to_string() })
}

fn reply(id: u64, result: Value) -> Value {
    write(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn update(session: &str, body: Value) -> Value {
    write(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": body}}))
}

fn init_params() -> String {
    format!(
        r#"{{"clientCapabilities":{{"fs":{{"readTextFile":false,"writeTextFile":false}},"terminal":false}},"clientInfo":{{"name":"eitri","title":"Eitri","version":"{}"}},"protocolVersion":1}}"#,
        env!("CARGO_PKG_VERSION")
    )
}

fn init_answer(load_session: bool, list_sessions: bool) -> Value {
    json!({
        "protocolVersion": 1,
        "agentInfo": {"name": "stand-in", "title": "Stand-in", "version": "1.2.3"},
        "agentCapabilities": {"loadSession": load_session, "sessionCapabilities": if list_sessions { json!({"close": {}, "list": {}}) } else { json!({"close": {}}) }},
        "authMethods": [{"id": "chat-gpt", "name": "ChatGPT"}]
    })
}

/// `initialize` answered, ready for `session/new` (id 2).
fn handshake(load_session: bool, list_sessions: bool) -> Vec<Value> {
    vec![
        expect_exact(&format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}}"#,
            init_params()
        )),
        reply(1, init_answer(load_session, list_sessions)),
    ]
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
        editor_server: None,
    }
}

/// A provider with its session started.
fn started(script: Vec<Value>, auto_mode: Option<&str>) -> AcpProvider {
    let provider = connect(script, auto_mode).expect("connects");
    provider.start(start_request(None)).expect("starts");
    provider
}

fn options(model: &str, effort: &str, mode: &str) -> Value {
    json!([
        {"id": "mode", "name": "Mode", "category": "mode", "type": "select", "currentValue": mode,
         "options": [{"value": "read-only", "name": "Read only"}, {"value": "workspace-write", "name": "Workspace write"}]},
        {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": model,
         "options": [
            {"value": "m-1", "name": "Model One", "description": "the first"},
            {"group": "more", "name": "More", "options": [{"value": "m-2", "name": "Model Two"}, {"value": "m-3", "name": "Model\nThree"}]}
         ]},
        {"id": "reasoning_effort", "name": "Reasoning effort", "category": "thought_level", "type": "select",
         "currentValue": effort,
         "options": [{"value": "low", "name": "Low"}, {"value": "high", "name": "High", "description": "slow"}]},
        {"id": "fast", "name": "Fast", "type": "boolean", "currentValue": false}
    ])
}

/// The handshake and a new session in `mode` whose answer lists `configOptions`.
fn boot_with(config: Option<Value>, mode: &str) -> Vec<Value> {
    let mut script = handshake(false, false);
    let mut answer = json!({"sessionId": "s1", "modes": {"currentModeId": mode, "availableModes": []}});
    if let Some(config) = config {
        answer["configOptions"] = config;
    }
    script.extend([
        expect_request(2, "session/new", json!({"cwd": "/work", "mcpServers": []})),
        reply(2, answer),
    ]);
    script
}

fn pump_all(provider: &AcpProvider) -> Vec<AgentDomainEvent> {
    provider.pump()
}

/// Pumps until the receiver has an answer; the events pumped on the way come back with it.
fn answer_of<T>(
    provider: &AcpProvider,
    rx: &std::sync::mpsc::Receiver<Result<T, ProviderError>>,
) -> (Result<T, ProviderError>, Vec<AgentDomainEvent>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut events = Vec::new();
    loop {
        events.extend(provider.pump());
        if let Ok(answer) = rx.try_recv() {
            return (answer, events);
        }
        assert!(Instant::now() < deadline, "no answer; saw {events:#?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn refusal(error: ProviderError) -> String {
    match error {
        ProviderError::Provider {
            code: ProviderErrorCode::SettingRefused,
            message,
        } => message,
        other => panic!("not a refused setting: {other:?}"),
    }
}

fn opened_model(events: &[AgentDomainEvent]) -> Option<String> {
    events.iter().find_map(|e| match e {
        AgentDomainEvent::SessionOpened { model, .. } => Some(model.clone()),
        _ => None,
    })
}

#[test]
fn the_lists_are_the_options_the_agent_reported() {
    let provider = started(
        boot_with(Some(options("m-2", "high", "workspace-write")), "workspace-write"),
        None,
    );
    let events = pump_all(&provider);
    assert_eq!(
        opened_model(&events).as_deref(),
        Some("m-2"),
        "no `models` block: the model option names it"
    );
    assert!(events.contains(&AgentDomainEvent::EffortReported {
        effort: Some("high".into())
    }));

    let list = provider.list_models("s1").unwrap().recv().unwrap().unwrap();
    let values: Vec<_> = list.models.iter().map(|m| m.value.as_str()).collect();
    assert_eq!(
        values,
        ["m-1", "m-2", "m-3"],
        "a group's choices follow in order; the mode is not a model"
    );
    assert_eq!(list.models[0].display_name, "Model One");
    assert_eq!(list.models[0].description, "the first");
    assert_eq!(
        list.models[2].display_name, "Model Three",
        "a name is folded to one line"
    );
    assert_eq!(list.applied.model, "m-2");
    assert_eq!(list.applied.effort, "high");
    let efforts: Vec<_> = list.efforts.iter().map(|e| e.value.as_str()).collect();
    assert_eq!(efforts, ["low", "high"]);
    assert_eq!(list.efforts[1].description, "slow");
}

#[test]
fn a_model_change_is_sent_as_the_agents_option_and_answered_with_what_it_now_runs() {
    let mut script = boot_with(Some(options("m-1", "high", "workspace-write")), "workspace-write");
    script.extend([
        expect_request(
            3,
            "session/set_config_option",
            json!({"sessionId": "s1", "configId": "model", "value": "m-3"}),
        ),
        // The agent changes the effort the new model takes along.
        reply(3, json!({"configOptions": options("m-3", "low", "workspace-write")})),
    ]);
    let provider = started(script, None);
    pump_all(&provider);

    let rx = provider.set_model("s1", "m-3").unwrap();
    let (applied, events) = answer_of(&provider, &rx);
    let applied = applied.unwrap();
    assert_eq!((applied.model.as_str(), applied.effort.as_str()), ("m-3", "low"));
    assert!(events.contains(&AgentDomainEvent::ModelReported { model: "m-3".into() }));
    assert!(events.contains(&AgentDomainEvent::EffortReported {
        effort: Some("low".into())
    }));
    let list = provider.list_models("s1").unwrap().recv().unwrap().unwrap();
    assert_eq!(list.applied.model, "m-3", "the list follows the agent's answer");
}

#[test]
fn a_value_is_found_by_any_letter_case_and_an_answer_without_a_list_is_taken_as_asked() {
    let mut script = boot_with(Some(options("m-1", "low", "workspace-write")), "workspace-write");
    script.extend([
        expect_request(
            3,
            "session/set_config_option",
            json!({"sessionId": "s1", "configId": "reasoning_effort", "value": "high"}),
        ),
        reply(3, json!({})),
    ]);
    let provider = started(script, None);
    pump_all(&provider);
    let rx = provider.apply_effort("s1", "HIGH").unwrap();
    let (applied, events) = answer_of(&provider, &rx);
    assert_eq!(applied.unwrap().effort, "high");
    assert!(events.contains(&AgentDomainEvent::EffortReported {
        effort: Some("high".into())
    }));
}

#[test]
fn nothing_is_sent_for_a_value_the_agent_did_not_offer_or_for_the_mode() {
    // The script has no request after the session: a line Eitri wrote would find the stand-in unmatched.
    let provider = started(
        boot_with(Some(options("m-1", "low", "workspace-write")), "workspace-write"),
        None,
    );
    pump_all(&provider);
    for (call, what) in [
        (provider.set_model("s1", "workspace-write"), "a mode value"),
        (provider.set_model("s1", "m-9"), "an unlisted model"),
        (provider.apply_effort("s1", "mode"), "the mode's id as an effort"),
        (
            provider.apply_effort("s1", ""),
            "going back to the settings, which an agent has none of",
        ),
    ] {
        let (answer, _) = answer_of(&provider, &call.unwrap());
        let words = refusal(answer.unwrap_err());
        assert!(!words.is_empty(), "{what}");
    }
    let (answer, _) = answer_of(&provider, &provider.set_model("s1", "m-9").unwrap());
    assert!(
        refusal(answer.unwrap_err()).contains("m-1, m-2, m-3"),
        "it says what is offered"
    );
    assert!(
        !pump_all(&provider).iter().any(|e| matches!(
            e,
            AgentDomainEvent::PolicyMismatch { .. } | AgentDomainEvent::SessionUnavailable { .. }
        )),
        "the agent was left alone"
    );
}

#[test]
fn an_agent_that_lists_nothing_says_so() {
    let provider = started(boot_with(None, "workspace-write"), None);
    pump_all(&provider);
    let list = provider.list_models("s1").unwrap().recv().unwrap().unwrap();
    assert!(list.models.is_empty() && list.efforts.is_empty());
    assert_eq!((list.applied.model.as_str(), list.applied.effort.as_str()), ("", ""));
    let (answer, _) = answer_of(&provider, &provider.set_model("s1", "m-1").unwrap());
    assert!(refusal(answer.unwrap_err()).contains("reports no model"));
    let (answer, _) = answer_of(&provider, &provider.apply_effort("s1", "low").unwrap());
    assert!(refusal(answer.unwrap_err()).contains("reports no effort"));
}

#[test]
fn an_agent_that_lists_only_a_mode_offers_neither() {
    let only_mode = json!([{"id": "mode", "category": "mode", "type": "select", "currentValue": "workspace-write",
        "options": [{"value": "workspace-write", "name": "Write"}]}]);
    let provider = started(boot_with(Some(only_mode), "workspace-write"), None);
    pump_all(&provider);
    let list = provider.list_models("s1").unwrap().recv().unwrap().unwrap();
    assert!(
        list.models.is_empty() && list.efforts.is_empty(),
        "the mode is never a model or an effort"
    );
}

#[test]
fn a_refused_change_comes_back_in_the_agents_words() {
    let mut script = boot_with(Some(options("m-1", "low", "workspace-write")), "workspace-write");
    script.extend([
        expect_request(
            3,
            "session/set_config_option",
            json!({"sessionId": "s1", "configId": "model", "value": "m-2"}),
        ),
        write(json!({"jsonrpc": "2.0", "id": 3, "error": {"code": -32602, "message": "Invalid\nparams"}})),
    ]);
    let provider = started(script, None);
    pump_all(&provider);
    let (answer, _) = answer_of(&provider, &provider.set_model("s1", "m-2").unwrap());
    let words = refusal(answer.unwrap_err());
    assert!(
        words.contains("would not make that change") && words.contains("Invalid params"),
        "{words}"
    );
    let list = provider.list_models("s1").unwrap().recv().unwrap().unwrap();
    assert_eq!(list.applied.model, "m-1", "nothing changed");
}

#[test]
fn an_answer_that_reports_another_mode_stops_the_session_like_any_other_report() {
    let mut script = boot_with(Some(options("m-1", "low", "workspace-write")), "workspace-write");
    script.extend([
        expect_request(
            3,
            "session/set_config_option",
            json!({"sessionId": "s1", "configId": "model", "value": "m-2"}),
        ),
        reply(3, json!({"configOptions": options("m-2", "low", "read-only")})),
    ]);
    let provider = started(script, Some("workspace-write"));
    pump_all(&provider);
    let (_, events) = answer_of(&provider, &provider.set_model("s1", "m-2").unwrap());
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PolicyMismatch { .. })),
        "{events:#?}"
    );
}

#[test]
fn a_move_is_refused_at_once_while_a_change_waits_for_its_answer() {
    let mut script = boot_with(Some(options("m-1", "low", "workspace-write")), "workspace-write");
    // The agent reads the change and never answers it.
    script.push(expect_request(
        3,
        "session/set_config_option",
        json!({"sessionId": "s1", "configId": "model", "value": "m-2"}),
    ));
    let provider = started(script, None);
    pump_all(&provider);
    let _waiting = provider.set_model("s1", "m-2").unwrap();
    match provider.freeze_with_clock_for_tests(Instant::now, |_| panic!("this refusal must not wait for a start")) {
        Err(agent::MoveRefusedWhy::Link(why)) => assert!(why.contains("model or the effort"), "{why}"),
        Err(other) => panic!("refused for another reason: {other}"),
        Ok(_) => panic!("a chat with a change waiting moved"),
    }
}

#[test]
fn a_list_the_agent_sends_by_itself_replaces_the_held_one() {
    let mut script = boot_with(Some(options("m-1", "low", "workspace-write")), "workspace-write");
    script.extend([
        expect_request(
            3,
            "session/prompt",
            json!({"sessionId": "s1", "prompt": [{"type": "text", "text": "go"}]}),
        ),
        update(
            "s1",
            json!({"sessionUpdate": "config_option_update", "configOptions": options("m-2", "high", "workspace-write")}),
        ),
        reply(3, json!({"stopReason": "end_turn"})),
    ]);
    let provider = started(script, None);
    pump_all(&provider);
    provider
        .send_turn_with(
            agent::SendTurnRequest {
                session_id: "s1".to_string(),
                text: "go".to_string(),
            },
            TurnPolicy {
                reviewer: Reviewer::Agent,
                sandbox: agent::Sandbox::Project,
            },
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut events = Vec::new();
    while !events
        .iter()
        .any(|e| matches!(e, AgentDomainEvent::TurnCompleted { .. }))
    {
        events.extend(provider.pump());
        assert!(Instant::now() < deadline, "{events:#?}");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(events.contains(&AgentDomainEvent::ModelReported { model: "m-2".into() }));
    assert!(events.contains(&AgentDomainEvent::EffortReported {
        effort: Some("high".into())
    }));
    let list = provider.list_models("s1").unwrap().recv().unwrap().unwrap();
    assert_eq!(
        (list.applied.model.as_str(), list.applied.effort.as_str()),
        ("m-2", "high")
    );
}

#[test]
fn hostile_option_text_is_bounded_before_it_is_held() {
    let long = "x".repeat(500);
    let hostile = json!([
        {"id": "model", "category": "model", "type": "select", "currentValue": "a", "options": [
            {"value": "a", "name": long, "description": long},
            {"value": "has space", "name": "dropped"},
            {"value": "bidi\u{202e}", "name": "dropped"},
            {"value": long, "name": "dropped for its length"},
            {"value": "a", "name": "a repeat"}
        ]}
    ]);
    let provider = started(boot_with(Some(hostile), "workspace-write"), None);
    pump_all(&provider);
    let list = provider.list_models("s1").unwrap().recv().unwrap().unwrap();
    assert_eq!(list.models.len(), 1);
    assert!(list.models[0].display_name.chars().count() <= 80);
    assert!(list.models[0].description.chars().count() <= 200);
}

// ---------------------------------------------------------------- the agent's own sessions

fn listing_boot() -> Vec<Value> {
    let mut script = handshake(true, true);
    script.extend([
        expect_request(2, "session/new", json!({"cwd": "/work", "mcpServers": []})),
        reply(2, json!({"sessionId": "s1"})),
        expect_request(3, "session/list", json!({"cwd": "/work"})),
    ]);
    script
}

fn until_listed(provider: &AcpProvider, count: usize) -> Vec<agent::ListedSession> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        provider.pump();
        let listed = provider.listed_sessions();
        if listed.len() == count {
            return listed;
        }
        assert!(Instant::now() < deadline, "listed so far: {listed:#?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn the_sessions_an_agent_lists_are_asked_for_once_it_is_open_and_read_bounded() {
    let mut script = listing_boot();
    script.push(reply(
        3,
        json!({"sessions": [
            {"sessionId": "old-one", "cwd": "/work", "title": "first\nth\u{202e}ing", "updatedAt": "2026-11-01T00:00:00Z"},
            {"sessionId": "new_one", "cwd": "/work/", "title": null, "updatedAt": "2026-11-02T05:00:00.250+00:00"},
            {"sessionId": "has space", "cwd": "/work", "title": "dropped"},
            {"sessionId": "elsewhere", "cwd": "/other", "title": "dropped"},
            {"sessionId": "../escape", "title": "dropped"},
            {"sessionId": "old-one", "title": "a repeat"},
            {"sessionId": "no-time"}
        ]}),
    ));
    let provider = started(script, None);
    let listed = until_listed(&provider, 3);
    let ids: Vec<_> = listed.iter().map(|s| s.session_id.as_str()).collect();
    assert_eq!(
        ids,
        ["new_one", "old-one", "no-time"],
        "newest first, a session with no time last"
    );
    assert_eq!(listed[0].updated_at_ms, Some(1_793_595_600_250));
    assert_eq!(listed[0].title, None);
    assert_eq!(listed[1].title.as_deref(), Some("first thing"));
    assert_eq!(listed[1].updated_at_ms, Some(1_793_491_200_000));
}

#[test]
fn an_agent_is_asked_again_when_the_host_wants_a_fresh_list() {
    let mut script = listing_boot();
    script.extend([
        reply(
            3,
            json!({"sessions": [{"sessionId": "a", "updatedAt": "2026-11-01T00:00:00Z"}]}),
        ),
        expect_request(4, "session/list", json!({"cwd": "/work"})),
        reply(
            4,
            json!({"sessions": [
                {"sessionId": "b", "updatedAt": "2026-11-02T00:00:00Z"},
                {"sessionId": "a", "updatedAt": "2026-11-01T00:00:00Z"}
            ]}),
        ),
    ]);
    let provider = started(script, None);
    until_listed(&provider, 1);
    provider.refresh_sessions();
    let listed = until_listed(&provider, 2);
    assert_eq!(listed[0].session_id, "b");
}

#[test]
fn an_agent_that_lists_no_sessions_is_not_asked() {
    // No `list` capability, and one that lists but cannot load: neither is sent `session/list`.
    for (load, list) in [(false, false), (false, true), (true, false)] {
        let mut script = handshake(load, list);
        script.extend([
            expect_request(2, "session/new", json!({"cwd": "/work", "mcpServers": []})),
            reply(2, json!({"sessionId": "s1"})),
            expect_request(
                3,
                "session/prompt",
                json!({"sessionId": "s1", "prompt": [{"type": "text", "text": "go"}]}),
            ),
            reply(3, json!({"stopReason": "end_turn"})),
        ]);
        let provider = started(script, None);
        provider.refresh_sessions();
        provider
            .send_turn_with(
                agent::SendTurnRequest {
                    session_id: "s1".into(),
                    text: "go".into(),
                },
                TurnPolicy {
                    reviewer: Reviewer::Agent,
                    sandbox: agent::Sandbox::Project,
                },
            )
            .unwrap();
        let mut events = Vec::new();
        until(|| {
            events.extend(provider.pump());
            events.iter().any(|e| {
                matches!(
                    e,
                    AgentDomainEvent::TurnCompleted { .. } | AgentDomainEvent::SessionUnavailable { .. }
                )
            })
        });
        assert!(provider.listed_sessions().is_empty());
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. })),
            "load {load}, list {list}: the agent was sent a line it did not expect: {events:#?}"
        );
    }
}

#[test]
fn a_refused_list_leaves_the_held_one_and_the_session_alone() {
    let mut script = listing_boot();
    script.extend([
        reply(3, json!({"sessions": [{"sessionId": "a"}]})),
        expect_request(4, "session/list", json!({"cwd": "/work"})),
        write(json!({"jsonrpc": "2.0", "id": 4, "error": {"code": -32000, "message": "Authentication required"}})),
    ]);
    let provider = started(script, None);
    until_listed(&provider, 1);
    provider.refresh_sessions();
    until(|| provider.lines_read_for_tests() >= 4);
    let events = provider.pump();
    assert_eq!(provider.listed_sessions().len(), 1);
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. })));
}
