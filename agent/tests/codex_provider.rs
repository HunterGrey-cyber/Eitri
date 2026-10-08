//! `CodexProvider` against a scripted stand-in for `codex app-server` (`eitri-agent-standin`). The
//! scripts are built from the frames recorded from Codex 0.160.0 (`tests/fixtures/codex/`) where a
//! recording has the shape, and by hand for the shapes no run produced (reasoning, a file change, a
//! failed turn). No real Codex runs here and no model turn is spent.

use agent::providers::codex::provider::NOT_FOUND;
use agent::providers::codex::{parse_user_agent_version, CodexLaunch, CodexProvider, MIN_CODEX};
use agent::{
    AgentDomainEvent, AgentProvider, CloseSessionRequest, ContentKind, DirTrust, ExternalProvider, ExternalStart,
    InterruptTurnRequest, ProviderError, ProviderErrorCode, Reviewer, SendTurnRequest, TrustPlan, TurnOutcome,
    TurnPolicy,
};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

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

fn write(message: Value) -> Value {
    json!({ "write": message.to_string() })
}

/// The handshake Eitri sends and a Codex that answers it: `initialize` (id 1), `initialized`, `account/read`
/// (id 2).
fn handshake(ua: &str, account: Value) -> Vec<Value> {
    vec![
        expect_exact(&format!(
            r#"{{"id":1,"method":"initialize","params":{{"clientInfo":{{"name":"eitri","title":"Eitri","version":"{}"}}}}}}"#,
            env!("CARGO_PKG_VERSION")
        )),
        write(json!({"id": 1, "result": {"userAgent": ua, "codexHome": "/x"}})),
        expect_exact(r#"{"method":"initialized"}"#),
        expect_exact(r#"{"id":2,"method":"account/read","params":{"refreshToken":false}}"#),
        write(json!({"id": 2, "result": account})),
    ]
}

fn signed_in() -> Value {
    json!({"account": {"type": "chatgpt", "email": "someone@example.com", "planType": "pro"}, "requiresOpenaiAuth": true})
}

fn launch() -> CodexLaunch {
    CodexLaunch {
        program: PathBuf::from(env!("CARGO_BIN_EXE_eitri-agent-standin")),
        cwd: std::env::temp_dir(),
        state_home: std::env::temp_dir(),
    }
}

fn connect(script: Vec<Value>) -> Result<CodexProvider, ProviderError> {
    CodexProvider::connect_with_args(launch(), vec![OsString::from(Value::Array(script).to_string())])
}

fn plan(cwd: &str) -> TrustPlan {
    TrustPlan {
        dirs: vec![(PathBuf::from(cwd), DirTrust::Untrusted)],
        skills_off: true,
        fingerprint: "fp".into(),
        project_only: Vec::new(),
    }
}

fn start_request(reviewer: Reviewer) -> ExternalStart {
    ExternalStart {
        cwd: "/work/proj".into(),
        plan: plan("/work/proj"),
        policy: TurnPolicy { reviewer },
        resume: None,
    }
}

/// A `thread/start` answer that echoes the policy the session was asked for, with `reviewer` as Codex's
/// word for who reviews.
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

fn turn_completed(thread: &str, turn: &str, status: &str, error: Value) -> Value {
    write(json!({"method": "turn/completed", "params": {
        "threadId": thread,
        "turn": {"id": turn, "items": [], "itemsView": "summary", "status": status, "error": error},
    }}))
}

/// A session: the handshake and the `thread/start` answer, then `rest`.
fn session_script(thread: &str, rest: Vec<Value>) -> Vec<Value> {
    let mut script = handshake(UA, signed_in());
    script.push(expect_request(3, "thread/start"));
    script.push(thread_response(3, thread));
    script.extend(rest);
    script
}

/// A started session.
fn open(thread: &str, rest: Vec<Value>) -> (Arc<CodexProvider>, String) {
    let provider = Arc::new(connect(session_script(thread, rest)).expect("the handshake passes"));
    let id = provider
        .start(start_request(Reviewer::User))
        .expect("the session starts");
    assert_eq!(id, thread);
    (provider, id)
}

fn send(provider: &CodexProvider, thread: &str, text: &str) -> Result<String, ProviderError> {
    provider.send_turn_with(
        SendTurnRequest {
            session_id: thread.to_string(),
            text: text.to_string(),
        },
        TurnPolicy {
            reviewer: Reviewer::User,
        },
    )
}

/// The frames of a recording after its `initialized`, as stand-in steps: what Eitri sends is expected by its
/// method, what Codex sent is written. Ids from the second on move up by one, since Eitri's handshake
/// has one more request (`account/read`) than the recording. `last` is the number of recorded lines to
/// use.
fn recording(name: &str, last: usize) -> Vec<Value> {
    let text = std::fs::read_to_string(format!("{FIXTURES}/{name}.jsonl")).expect("the recording exists");
    let shift = |id: u64| if id >= 2 { id + 1 } else { id };
    let mut steps = Vec::new();
    let mut started = false;
    for line in text.lines() {
        let frame: Value = serde_json::from_str(line).expect("a recorded frame");
        let message = frame["msg"].clone();
        let outgoing = frame["dir"] == "out";
        let method = message.get("method").and_then(Value::as_str);
        if !started {
            // Everything up to the first request after `initialized` is the handshake.
            if outgoing && method == Some("thread/start") {
                started = true;
            } else {
                continue;
            }
        }
        if steps.len() >= last {
            break;
        }
        match (outgoing, method, message.get("id").and_then(Value::as_u64)) {
            (true, Some(method), Some(id)) => steps.push(expect_request(shift(id), method)),
            (true, Some(method), None) => steps.push(json!({"expect": format!(r#"^\{{"method":"{method}""#)})),
            (true, None, _) => panic!("a recorded answer to a server request has no place in this test"),
            (false, _, Some(id)) => {
                let mut answer = message;
                answer["id"] = json!(shift(id));
                steps.push(write(answer));
            }
            (false, _, None) => steps.push(write(message)),
        }
    }
    steps
}

fn recorded_ids(name: &str) -> (String, String) {
    let text = std::fs::read_to_string(format!("{FIXTURES}/{name}.jsonl")).expect("the recording exists");
    let (mut thread, mut turn) = (String::new(), String::new());
    for line in text.lines() {
        let m: Value = serde_json::from_str::<Value>(line).expect("a frame")["msg"].clone();
        if thread.is_empty() {
            if let Some(id) = m["result"]["thread"]["id"].as_str() {
                thread = id.to_string();
            }
        }
        if turn.is_empty() {
            if let Some(id) = m["result"]["turn"]["id"].as_str() {
                turn = id.to_string();
            }
        }
    }
    (thread, turn)
}

// ---------------------------------------------------------------- reading events

/// Pumps until `done` is true of everything read, or fails after 20 s.
fn pump_until(provider: &CodexProvider, mut done: impl FnMut(&[AgentDomainEvent]) -> bool) -> Vec<AgentDomainEvent> {
    let mut seen = Vec::new();
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

fn the_completion(events: &[AgentDomainEvent]) -> &AgentDomainEvent {
    events
        .iter()
        .find(|e| matches!(e, AgentDomainEvent::TurnCompleted { .. }))
        .expect("a TurnCompleted")
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only asks whether the pid exists.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

// ---------------------------------------------------------------- the handshake

#[test]
fn handshake_refuses_below_min_version() {
    let script = vec![
        expect_request(1, "initialize"),
        write(json!({"id": 1, "result": {"userAgent": "codex/0.159.9 (Arch)", "codexHome": "/x"}})),
    ];
    match connect(script) {
        Err(ProviderError::Provider { code, message }) => {
            assert_eq!(code, ProviderErrorCode::UnsupportedCliVersion);
            assert!(message.contains("0.159.9"), "names the version found: {message}");
            assert!(message.contains("0.160.0"), "names the version needed: {message}");
        }
        Err(other) => panic!("expected a version refusal, got {other}"),
        Ok(_) => panic!("a Codex below the floor connected"),
    }
    assert_eq!(MIN_CODEX, (0, 160, 0));
}

#[test]
fn a_user_agent_with_no_readable_version_is_refused() {
    let script = vec![
        expect_request(1, "initialize"),
        write(json!({"id": 1, "result": {"userAgent": "codex (no version here)", "codexHome": "/x"}})),
    ];
    match connect(script) {
        Err(ProviderError::Provider { code, .. }) => assert_eq!(code, ProviderErrorCode::UnsupportedCliVersion),
        Err(other) => panic!("expected a refusal, got {other}"),
        Ok(_) => panic!("an unreadable version connected"),
    }
    assert_eq!(parse_user_agent_version(UA), Some((0, 160, 0)));
}

#[test]
fn signed_out_is_a_tab_notice() {
    let script = handshake(UA, json!({"account": null, "requiresOpenaiAuth": true}));
    match connect(script) {
        Err(ProviderError::Provider { code, message }) => {
            assert_eq!(code, ProviderErrorCode::InvalidConfiguration);
            assert_eq!(message, "Codex is not signed in: run `codex login` in a terminal");
        }
        Err(other) => panic!("expected a notice, got {other}"),
        Ok(_) => panic!("a signed-out Codex connected"),
    }
}

#[test]
fn account_present_or_auth_not_required_passes() {
    connect(handshake(UA, signed_in())).expect("an account passes");
    connect(handshake(UA, json!({"account": null, "requiresOpenaiAuth": false}))).expect("no auth needed passes");
}

#[test]
fn a_newer_codex_runs_with_a_note_and_the_tested_one_with_none() {
    let tested = connect(handshake(UA, signed_in())).expect("connects");
    assert!(tested.info().startup_diagnostics.is_empty());
    let newer = connect(handshake("codex/0.161.0 (Arch)", signed_in())).expect("connects");
    assert_eq!(
        newer.info().startup_diagnostics,
        vec!["untested Codex 0.161.0".to_string()]
    );
}

#[test]
fn missing_binary_is_a_tab_notice() {
    let launch = CodexLaunch {
        program: PathBuf::from("/nonexistent/dir/codex"),
        cwd: std::env::temp_dir(),
        state_home: std::env::temp_dir(),
    };
    match CodexProvider::connect(launch) {
        Err(ProviderError::Provider { code, message }) => {
            assert_eq!(code, ProviderErrorCode::ProviderUnavailable);
            assert_eq!(message, NOT_FOUND);
            assert!(message.contains("codex login") && message.contains("EITRI_CODEX"));
        }
        Err(other) => panic!("expected a notice, got {other}"),
        Ok(_) => panic!("a missing binary connected"),
    }
}

/// A scratch directory holding `codex`, a script that leaves `ran` beside itself when anything runs it.
/// Returns the directory, the program and the marker.
fn marker_program(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("eitri-{name}-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("ran");
    let program = dir.join("codex");
    std::fs::write(&program, format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display())).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    (dir, program, marker)
}

/// A relative `CODEX_HOME` is resolved by Codex against its working directory, the project root, so the
/// project's own directory of that name would be read as the user's configuration. The start is refused,
/// naming the variable, before any child runs; an absolute or empty one is not refused.
#[test]
fn a_relative_codex_home_refuses_the_start_before_anything_runs() {
    for value in [".codex", "./.codex", "codex-home", "../elsewhere"] {
        let (dir, program, marker) = marker_program("relative-home");
        let launch = CodexLaunch {
            program,
            cwd: dir.clone(),
            state_home: dir.clone(),
        };
        let env = vec![(OsString::from("CODEX_HOME"), OsString::from(value))];
        match CodexProvider::connect_with_env(launch, Vec::new(), env, Vec::new()) {
            Err(ProviderError::Provider { code, message }) => {
                assert_eq!(code, ProviderErrorCode::InvalidConfiguration, "{value}");
                assert!(
                    message.contains("CODEX_HOME") && message.contains(value) && message.contains("absolute"),
                    "{message}"
                );
            }
            Err(other) => panic!("expected a refusal for {value}, got {other}"),
            Ok(_) => panic!("a relative CODEX_HOME ({value}) connected"),
        }
        assert!(!marker.exists(), "a child ran under CODEX_HOME={value}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    // An absolute or an empty one is Codex's own business: the child runs (and here fails).
    for value in ["/nonexistent/eitri-codex-home", ""] {
        let (dir, program, marker) = marker_program("absolute-home");
        let launch = CodexLaunch {
            program,
            cwd: dir.clone(),
            state_home: dir.clone(),
        };
        let env = vec![(OsString::from("CODEX_HOME"), OsString::from(value))];
        assert!(CodexProvider::connect_with_env(launch, Vec::new(), env, Vec::new()).is_err());
        assert!(marker.exists(), "CODEX_HOME={value:?} was refused");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[test]
fn the_plain_claude_shaped_calls_refuse() {
    let (provider, thread) = open("th-plain", vec![]);
    let refused = |e: Result<String, ProviderError>| matches!(e, Err(ProviderError::UnsupportedCapability("external")));
    assert!(refused(provider.send_turn(SendTurnRequest {
        session_id: thread,
        text: "x".into()
    })));
    assert_eq!(provider.name(), "codex");
}

// ---------------------------------------------------------------- start and turn

#[test]
fn thread_start_sends_session_config_and_policy() {
    // The exact line, written out by hand: the reviewer is Codex's own in Auto, the policy and sandbox are
    // the constants, every trust level is the untrusted one and every ancestor has an entry, and the
    // config holds nested objects only.
    let expected = concat!(
        r#"{"id":3,"method":"thread/start","params":{"approvalPolicy":"on-request","approvalsReviewer":"auto_review","#,
        r#""config":{"features":{"exec_permission_approvals":false,"request_permissions_tool":false},"#,
        r#""projects":{"/":{"trust_level":"untrusted"},"/work":{"trust_level":"untrusted"},"#,
        r#""/work/proj":{"trust_level":"untrusted"}},"#,
        r#""sandbox_workspace_write":{"exclude_slash_tmp":false,"exclude_tmpdir_env_var":false,"#,
        r#""network_access":false,"writable_roots":[]},"skills":{"include_instructions":false}},"#,
        r#""cwd":"/work/proj","sandbox":"workspace-write"}}"#,
    );
    let mut script = handshake(UA, signed_in());
    script.push(expect_exact(expected));
    script.push(thread_response_for(3, "th-1", "auto_review"));
    let provider = connect(script).expect("connects");
    let id = provider.start(start_request(Reviewer::Agent)).expect("starts");
    assert_eq!(id, "th-1");
    let events = provider.pump();
    assert_eq!(
        events,
        vec![AgentDomainEvent::SessionOpened {
            session_id: "th-1".into(),
            provider_session_id: "th-1".into(),
            model: "gpt-test".into(),
            cwd: "/work/proj".into(),
        }]
    );
}

#[test]
fn the_plain_resume_capability_stays_off() {
    // Resuming goes through the external start (`resume` in `ExternalStart`); the Claude-shaped capability
    // the conversation layer reads stays off.
    let provider = connect(handshake(UA, signed_in())).expect("connects");
    assert!(!provider.capabilities().resume);
}

#[test]
fn turn_id_comes_from_the_turn_start_response() {
    let turn_line = concat!(
        r#"{"id":4,"method":"turn/start","params":{"approvalPolicy":"on-request","approvalsReviewer":"user","#,
        r#""input":[{"text":"hello","type":"text"}],"#,
        r#""sandboxPolicy":{"excludeSlashTmp":false,"excludeTmpdirEnvVar":false,"networkAccess":false,"#,
        r#""type":"workspaceWrite","writableRoots":[]},"threadId":"th-2"}}"#,
    );
    let (provider, thread) = open(
        "th-2",
        vec![expect_exact(turn_line), turn_response(4, "turn-from-answer")],
    );
    let turn = send(&provider, &thread, "hello").expect("the turn starts");
    assert_eq!(turn, "turn-from-answer");
    let events = provider.pump();
    assert!(
        events.contains(&AgentDomainEvent::TurnStarted {
            turn_id: "turn-from-answer".into()
        }),
        "{events:?}"
    );
}

#[test]
fn second_send_before_turn_completed_is_refused() {
    let (provider, thread) = open(
        "th-3",
        vec![
            expect_request(4, "turn/start"),
            turn_response(4, "turn-a"),
            turn_completed("th-3", "turn-a", "completed", Value::Null),
            // The refused send never went out, so the send after the turn ended takes id 5.
            expect_request(5, "turn/start"),
            turn_response(5, "turn-b"),
        ],
    );
    send(&provider, &thread, "one").expect("the first turn starts");
    let refused = send(&provider, &thread, "two").expect_err("a turn is running");
    match &refused {
        ProviderError::Provider { code, .. } => assert_eq!(*code, ProviderErrorCode::TurnAlreadyActive),
        other => panic!("expected TurnAlreadyActive, got {other:?}"),
    }
    assert!(refused.is_benign(), "the session is healthy after the refusal");

    let events = until_turn_completed(&provider);
    assert!(matches!(
        the_completion(&events),
        AgentDomainEvent::TurnCompleted {
            outcome: TurnOutcome::Completed,
            ..
        }
    ));
    assert_eq!(
        send(&provider, &thread, "two").expect("the second turn starts"),
        "turn-b"
    );
}

#[test]
fn turn_start_wait_is_bounded_by_10s() {
    let (provider, thread) = open(
        "th-4",
        vec![expect_request(4, "turn/start"), json!({"sleep_ms": 40_000})],
    );
    let began = Instant::now();
    let result = send(&provider, &thread, "never answered");
    let took = began.elapsed();
    assert!(matches!(result, Err(ProviderError::Timeout)), "{result:?}");
    assert!(
        took >= Duration::from_secs(9) && took <= Duration::from_secs(11),
        "the wait took {took:?}"
    );
    // The failed start left no turn behind.
    let interrupt = provider.interrupt_turn(InterruptTurnRequest {
        session_id: thread.clone(),
    });
    assert!(matches!(
        interrupt,
        Err(ProviderError::Provider {
            code: ProviderErrorCode::NoActiveTurn,
            ..
        })
    ));
}

// ---------------------------------------------------------------- the stream

#[test]
fn stream_maps_a_recorded_command_and_message() {
    let name = "m4-i2-sandbox";
    let (thread, turn) = recorded_ids(name);
    // The recording from the first `thread/start` on, to the end of the turn.
    let mut script = handshake(UA, signed_in());
    script.extend(recording(name, usize::MAX));
    let provider = connect(script).expect("connects");
    let mut request = start_request(Reviewer::User);
    request.cwd = "/scratch/acp/measure/m4/i2/proj-i2-44".into();
    request.plan = plan(&request.cwd);
    assert_eq!(provider.start(request).expect("starts"), thread);
    assert_eq!(send(&provider, &thread, "probe").expect("turn starts"), turn);
    let events = until_turn_completed(&provider);

    // The turn start comes before anything it caused, though Codex wrote its notifications at once.
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match e {
            AgentDomainEvent::SessionOpened { .. } => "opened",
            AgentDomainEvent::TurnStarted { .. } => "turn",
            AgentDomainEvent::ToolCallStarted { .. } => "call",
            AgentDomainEvent::ToolCallCompleted { .. } => "result",
            AgentDomainEvent::AssistantMessageBoundary { .. } => "boundary",
            AgentDomainEvent::ContentDelta { .. } => "text",
            AgentDomainEvent::TurnCompleted { .. } => "done",
            other => panic!("an event the recording does not explain: {other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        ["opened", "turn", "call", "result", "boundary", "text", "done"],
        "{events:#?}"
    );

    match &events[0] {
        AgentDomainEvent::SessionOpened { model, .. } => assert_eq!(model, "gpt-6-luna"),
        other => panic!("{other:?}"),
    }
    match &events[2] {
        AgentDomainEvent::ToolCallStarted {
            turn_id,
            tool_use_id,
            name,
            input,
        } => {
            assert_eq!(
                (turn_id.as_str(), tool_use_id.as_str(), name.as_str()),
                (turn.as_str(), "c1", "commandExecution")
            );
            assert!(input["command"]
                .as_str()
                .unwrap()
                .starts_with("/usr/bin/zsh -lc \"python3"));
            assert_eq!(input["cwd"], "/scratch/acp/measure/m4/i2/proj-i2-44");
        }
        other => panic!("{other:?}"),
    }
    match &events[3] {
        AgentDomainEvent::ToolCallCompleted {
            tool_use_id,
            content,
            is_error,
            ..
        } => {
            assert_eq!(tool_use_id, "c1");
            assert!(content.as_str().unwrap().contains("EXTRA_WRITE_FAIL"));
            assert!(!is_error);
        }
        other => panic!("{other:?}"),
    }
    match &events[5] {
        AgentDomainEvent::ContentDelta { kind, text, .. } => {
            assert_eq!((*kind, text.as_str()), (ContentKind::Text, "done"));
        }
        other => panic!("{other:?}"),
    }
    match &events[6] {
        AgentDomainEvent::TurnCompleted {
            outcome,
            result_text,
            usage,
            detail,
            ..
        } => {
            assert_eq!(*outcome, TurnOutcome::Completed);
            assert_eq!(result_text, "done");
            assert_eq!(detail, &Default::default());
            let usage = usage.as_ref().expect("the turn reported tokens");
            // Codex reports no cost: it is absent, not zero.
            assert!(!usage.cost_reported);
            let tokens = usage.tokens.as_ref().expect("tokens");
            assert_eq!((tokens.input, tokens.output), (2, 2));
            assert_eq!(usage.model.as_deref(), Some("gpt-6-luna"));
        }
        other => panic!("{other:?}"),
    }
}

fn item_note(method: &str, thread: &str, turn: &str, item: Value) -> Value {
    write(json!({"method": method, "params": {"threadId": thread, "turnId": turn, "item": item}}))
}

fn delta_note(method: &str, thread: &str, turn: &str, item: &str, delta: &str) -> Value {
    write(json!({"method": method, "params": {"threadId": thread, "turnId": turn, "itemId": item, "delta": delta}}))
}

#[test]
fn stream_maps_messages_reasoning_commands_file_changes() {
    let (t, u) = ("th-5", "turn-5");
    let file_change = |status: &str| {
        json!({"type": "fileChange", "id": "f1", "status": status, "changes": [
            {"path": "/work/proj/a.txt", "kind": {"type": "add"}, "diff": "+hello\n"},
            {"path": "/work/proj/b.txt", "kind": {"type": "update", "move_path": "/work/proj/c.txt"}, "diff": "-x\n+y\n"},
        ]})
    };
    let (provider, thread) = open(
        t,
        vec![
            expect_request(4, "turn/start"),
            turn_response(4, u),
            // Reasoning that streams.
            item_note(
                "item/started",
                t,
                u,
                json!({"type": "reasoning", "id": "r1", "summary": [], "content": []}),
            ),
            delta_note("item/reasoning/summaryTextDelta", t, u, "r1", "weigh "),
            delta_note("item/reasoning/summaryTextDelta", t, u, "r1", "options"),
            item_note(
                "item/completed",
                t,
                u,
                json!({"type": "reasoning", "id": "r1", "summary": ["weigh options"], "content": []}),
            ),
            // Reasoning that arrives whole.
            item_note(
                "item/completed",
                t,
                u,
                json!({"type": "reasoning", "id": "r2", "summary": ["a", "b"], "content": []}),
            ),
            // A message that streams, with the rest of its text only in the final item.
            item_note(
                "item/started",
                t,
                u,
                json!({"type": "agentMessage", "id": "m1", "text": ""}),
            ),
            delta_note("item/agentMessage/delta", t, u, "m1", "Hel"),
            delta_note("item/agentMessage/delta", t, u, "m1", "lo"),
            item_note(
                "item/completed",
                t,
                u,
                json!({"type": "agentMessage", "id": "m1", "text": "Hello world"}),
            ),
            // A file change, then a failed command, then another message.
            item_note("item/started", t, u, file_change("inProgress")),
            item_note("item/completed", t, u, file_change("completed")),
            item_note(
                "item/started",
                t,
                u,
                json!({"type": "commandExecution", "id": "c9", "command": "false", "cwd": "/work/proj", "status": "inProgress"}),
            ),
            delta_note("item/commandExecution/outputDelta", t, u, "c9", "ignored while running"),
            item_note(
                "item/completed",
                t,
                u,
                json!({"type": "commandExecution", "id": "c9", "command": "false", "cwd": "/work/proj", "status": "failed", "aggregatedOutput": "boom", "exitCode": 1}),
            ),
            item_note(
                "item/completed",
                t,
                u,
                json!({"type": "mcpToolCall", "id": "k1", "server": "s", "tool": "t", "status": "completed", "arguments": {"a": 1}, "result": {"ok": true}}),
            ),
            // Noise: another thread, another turn, an item type that is not shown.
            item_note(
                "item/completed",
                "other-thread",
                u,
                json!({"type": "agentMessage", "id": "x1", "text": "not ours"}),
            ),
            item_note(
                "item/completed",
                t,
                "other-turn",
                json!({"type": "agentMessage", "id": "x2", "text": "not this turn"}),
            ),
            item_note(
                "item/completed",
                t,
                u,
                json!({"type": "plan", "id": "p1", "text": "a plan"}),
            ),
            item_note(
                "item/started",
                t,
                u,
                json!({"type": "agentMessage", "id": "m2", "text": "Bye"}),
            ),
            item_note(
                "item/completed",
                t,
                u,
                json!({"type": "agentMessage", "id": "m2", "text": "Bye"}),
            ),
            turn_completed(t, u, "completed", Value::Null),
        ],
    );
    send(&provider, &thread, "go").expect("starts");
    let events = until_turn_completed(&provider);

    let text_of = |kind: ContentKind| -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                AgentDomainEvent::ContentDelta { kind: k, text, .. } if *k == kind => Some(text.clone()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(text_of(ContentKind::Thinking), ["weigh ", "options", "a\n\nb"]);
    assert_eq!(text_of(ContentKind::Text), ["Hel", "lo", " world", "Bye"]);
    let boundaries = events
        .iter()
        .filter(|e| matches!(e, AgentDomainEvent::AssistantMessageBoundary { .. }))
        .count();
    assert_eq!(boundaries, 2, "one per assistant message");

    let calls: Vec<(String, String, Value)> = events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::ToolCallStarted {
                tool_use_id,
                name,
                input,
                ..
            } => Some((tool_use_id.clone(), name.clone(), input.clone())),
            _ => None,
        })
        .collect();
    let names: Vec<(&str, &str)> = calls.iter().map(|(i, n, _)| (i.as_str(), n.as_str())).collect();
    assert_eq!(
        names,
        [("f1", "fileChange"), ("c9", "commandExecution"), ("k1", "mcpToolCall")],
        "a call that only completed is announced before its result"
    );
    assert_eq!(calls[0].2["changes"][0]["kind"], "add");
    assert_eq!(calls[0].2["changes"][0]["diff"], "+hello\n");
    assert_eq!(calls[0].2["changes"][1]["kind"], "update");
    assert_eq!(calls[0].2["changes"][1]["movePath"], "/work/proj/c.txt");
    assert_eq!(calls[1].2, json!({"command": "false", "cwd": "/work/proj"}));
    assert_eq!(calls[2].2, json!({"server": "s", "tool": "t", "arguments": {"a": 1}}));

    let results: Vec<(String, Value, bool)> = events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::ToolCallCompleted {
                tool_use_id,
                content,
                is_error,
                ..
            } => Some((tool_use_id.clone(), content.clone(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(
        results,
        [
            ("f1".to_string(), json!("applied"), false),
            ("c9".to_string(), json!("boom"), true),
            ("k1".to_string(), json!({"ok": true}), false),
        ]
    );
    match the_completion(&events) {
        AgentDomainEvent::TurnCompleted { result_text, usage, .. } => {
            assert_eq!(result_text, "Bye", "the last whole message is the turn's result");
            assert!(usage.is_none(), "no token report, no usage");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn rate_limits_become_account_limits_without_email() {
    let (provider, thread) = open(
        "th-6",
        vec![
            expect_request(4, "turn/start"),
            turn_response(4, "turn-6"),
            // The email is in a field Eitri has no type for; the closer window is the one that is kept.
            write(
                json!({"method": "account/rateLimits/updated", "params": {"rateLimits": {
                    "limitId": "codex", "limitName": "someone@example.com",
                    "primary": {"usedPercent": 42.5, "windowDurationMins": 300, "resetsAt": 1_790_000_000},
                    "secondary": {"usedPercent": 61.0, "windowDurationMins": 10080, "resetsAt": 1_790_500_000},
                    "planType": "pro", "email": "someone@example.com",
                }}}),
            ),
            // The same figure again says nothing new.
            write(
                json!({"method": "account/rateLimits/updated", "params": {"rateLimits": {
                    "limitId": "codex",
                    "primary": {"usedPercent": 42.5, "windowDurationMins": 300, "resetsAt": 1_790_000_000},
                    "secondary": {"usedPercent": 61.0, "windowDurationMins": 10080, "resetsAt": 1_790_500_000},
                    "planType": "pro",
                }}}),
            ),
            // No window reports a figure: nothing to say.
            write(
                json!({"method": "account/rateLimits/updated", "params": {"rateLimits": {
                    "limitId": "codex", "primary": null, "secondary": null, "planType": null,
                }}}),
            ),
            turn_completed("th-6", "turn-6", "completed", Value::Null),
        ],
    );
    send(&provider, &thread, "go").expect("starts");
    let events = until_turn_completed(&provider);
    let limits: Vec<&AgentDomainEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentDomainEvent::AccountLimits { .. }))
        .collect();
    assert_eq!(
        limits,
        [&AgentDomainEvent::AccountLimits {
            used_percent: 61.0,
            window_minutes: Some(10080),
            resets_at: Some(1_790_500_000),
            plan: Some("pro".into()),
        }]
    );
    assert!(
        !format!("{events:?}").contains('@'),
        "no event carries the address: {events:?}"
    );
}

#[test]
fn server_requests_are_refused_not_left_pending() {
    let (provider, thread) = open(
        "th-7",
        vec![
            expect_request(4, "turn/start"),
            turn_response(4, "turn-7"),
            write(json!({"id": "srv-1", "method": "item/tool/call", "params": {
                "threadId": "th-7", "turnId": "turn-7", "callId": "c1", "tool": "x", "arguments": {},
            }})),
            // The answer is a refusal, and the link goes on: the turn then ends.
            expect_exact(r#"{"id":"srv-1","error":{"code":-32601,"message":"method not found"}}"#),
            turn_completed("th-7", "turn-7", "completed", Value::Null),
        ],
    );
    send(&provider, &thread, "go").expect("starts");
    until_turn_completed(&provider);
}

#[test]
fn an_over_cap_turn_message_ends_the_turn_failed() {
    let (provider, thread) = open(
        "th-8",
        vec![
            expect_request(4, "turn/start"),
            turn_response(4, "turn-8"),
            json!({"write_big": {
                "prefix": r#"{"method":"turn/completed","params":{"threadId":"th-8","turn":{"items":[""#,
                "bytes": (16 << 20) + 1,
            }}),
        ],
    );
    send(&provider, &thread, "go").expect("starts");
    let events = until_turn_completed(&provider);
    match the_completion(&events) {
        AgentDomainEvent::TurnCompleted { outcome, detail, .. } => {
            assert_eq!(*outcome, TurnOutcome::Failed);
            assert_eq!(detail.message.as_deref(), Some("a message from Codex was too large"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_child_that_exits_is_a_session_unavailable_notice() {
    let (provider, thread) = open(
        "th-9",
        vec![
            expect_request(4, "turn/start"),
            turn_response(4, "turn-9"),
            json!({"stderr": "codex: something broke for someone@example.com"}),
            json!({"exit": 3}),
        ],
    );
    send(&provider, &thread, "go").expect("starts");
    let events = pump_until(&provider, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. }))
    });
    let reason = events
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::SessionUnavailable { reason } => Some(reason.clone()),
            _ => None,
        })
        .expect("a notice");
    assert!(reason.contains("something broke"), "{reason}");
    assert!(
        !reason.contains("someone@example.com"),
        "the address is replaced: {reason}"
    );
    assert!(
        send(&provider, &thread, "again").is_err(),
        "a dead connection takes no turn"
    );
}

// ---------------------------------------------------------------- how a turn ends

/// A `codexErrorInfo` value as Codex writes it, with the expected turn outcome.
struct Row {
    info: Value,
    outcome: TurnOutcome,
    reason: &'static str,
    status: Option<u32>,
    /// Text the detail's message must contain.
    says: &'static str,
}

fn rows() -> Vec<Row> {
    let unit = |name: &'static str, outcome: TurnOutcome, reason: &'static str, says: &'static str| Row {
        info: json!(name),
        outcome,
        reason,
        status: None,
        says,
    };
    let http = |name: &'static str, code: Option<u16>| Row {
        info: json!({ name: { "httpStatusCode": code } }),
        outcome: TurnOutcome::Failed,
        reason: name,
        status: code.map(u32::from),
        says: "Codex said this",
    };
    vec![
        unit(
            "usageLimitExceeded",
            TurnOutcome::LimitReached,
            "usage_limit",
            "Codex said this",
        ),
        unit(
            "rateLimitExceeded",
            TurnOutcome::LimitReached,
            "rate_limited",
            "Codex said this",
        ),
        unit(
            "sessionBudgetExceeded",
            TurnOutcome::LimitReached,
            "session_budget",
            "Codex said this",
        ),
        unit(
            "contextWindowExceeded",
            TurnOutcome::LimitReached,
            "context_window_exceeded",
            "Codex said this",
        ),
        unit(
            "tooManyDenials",
            TurnOutcome::Failed,
            "too_many_denials",
            "Codex said this",
        ),
        unit("sandboxError", TurnOutcome::Failed, "sandbox_error", "Codex said this"),
        unit(
            "unauthorized",
            TurnOutcome::Failed,
            "auth_required",
            "Codex is not signed in: run `codex login`",
        ),
        http("httpConnectionFailed", Some(502)),
        http("httpConnectionFailed", None),
        http("responseStreamConnectionFailed", Some(503)),
        http("responseStreamDisconnected", None),
        http("responseTooManyFailedAttempts", Some(429)),
        unit(
            "serverOverloaded",
            TurnOutcome::Failed,
            "serverOverloaded",
            "Codex said this",
        ),
        unit(
            "aVariantFromTheFuture",
            TurnOutcome::Failed,
            "aVariantFromTheFuture",
            "Codex said this",
        ),
    ]
}

/// The turn's ending for one `codexErrorInfo`, through a whole connection.
fn ending_of(status: &str, error: Value) -> AgentDomainEvent {
    let (provider, thread) = open(
        "th-end",
        vec![
            expect_request(4, "turn/start"),
            turn_response(4, "turn-end"),
            turn_completed("th-end", "turn-end", status, error),
        ],
    );
    send(&provider, &thread, "go").expect("starts");
    let events = until_turn_completed(&provider);
    the_completion(&events).clone()
}

#[test]
fn turn_endings_follow_the_spec_table() {
    for row in rows() {
        // Both forms Codex writes: the bare name, and an object with one key (a unit variant is never an
        // object on the wire, but the reader accepts it).
        let name = match &row.info {
            Value::String(s) => s.clone(),
            Value::Object(m) => m.keys().next().cloned().unwrap_or_default(),
            _ => unreachable!(),
        };
        let forms = [row.info.clone(), json!(name), json!({ name.clone(): {} })];
        for (n, form) in forms.into_iter().enumerate() {
            let error = json!({"message": "Codex said this", "codexErrorInfo": form});
            let event = ending_of("failed", error);
            let AgentDomainEvent::TurnCompleted { outcome, detail, .. } = event else {
                panic!("not a completion");
            };
            let with_status = n == 0 && row.status.is_some();
            assert_eq!(outcome, row.outcome, "{name} form {n}");
            assert_eq!(detail.reason.as_deref(), Some(row.reason), "{name} form {n}");
            assert_eq!(
                detail.api_error_status,
                if with_status { row.status } else { None },
                "{name} form {n}"
            );
            assert!(
                detail.message.as_deref().is_some_and(|m| m.contains(row.says)),
                "{name} form {n}: {:?}",
                detail.message
            );
        }
    }
}

#[test]
fn a_failed_turn_without_an_error_info_says_what_codex_said() {
    let event = ending_of("failed", json!({"message": "it went wrong", "codexErrorInfo": null}));
    let AgentDomainEvent::TurnCompleted { outcome, detail, .. } = event else {
        panic!("not a completion");
    };
    assert_eq!(outcome, TurnOutcome::Failed);
    assert_eq!(detail.reason, None);
    assert_eq!(detail.message.as_deref(), Some("it went wrong"));
}

#[test]
fn completed_and_interrupted_carry_no_message() {
    for (status, outcome) in [
        ("completed", TurnOutcome::Completed),
        ("interrupted", TurnOutcome::Interrupted),
    ] {
        // An error beside a status that is not "failed" is not what ended the turn.
        let event = ending_of(status, json!({"message": "noise", "codexErrorInfo": "sandboxError"}));
        let AgentDomainEvent::TurnCompleted {
            outcome: got, detail, ..
        } = event
        else {
            panic!("not a completion");
        };
        assert_eq!(got, outcome);
        assert_eq!(detail, Default::default());
    }
    // A status this build does not know is a failure that names it.
    let event = ending_of("paused", Value::Null);
    let AgentDomainEvent::TurnCompleted { outcome, detail, .. } = event else {
        panic!("not a completion");
    };
    assert_eq!(outcome, TurnOutcome::Failed);
    assert_eq!(detail.reason.as_deref(), Some("paused"));
}

// ---------------------------------------------------------------- interrupt and close

#[test]
fn a_recorded_interrupt_ends_the_turn_interrupted() {
    let name = "m4-i10c-interrupt";
    let (thread, turn) = recorded_ids(name);
    // The recording's first turn: start, the turn's first items, the interrupt, its end.
    let mut script = handshake(UA, signed_in());
    let steps = recording(name, usize::MAX);
    let end = steps
        .iter()
        .position(|s| {
            s["write"]
                .as_str()
                .is_some_and(|w| w.contains(r#""status": "interrupted""#) || w.contains(r#""status":"interrupted""#))
        })
        .expect("the recording has an interrupted turn");
    script.extend(steps.into_iter().take(end + 1));
    let provider = connect(script).expect("connects");
    assert_eq!(provider.start(start_request(Reviewer::User)).unwrap(), thread);
    assert_eq!(send(&provider, &thread, "one").unwrap(), turn);
    provider
        .interrupt_turn(InterruptTurnRequest {
            session_id: thread.clone(),
        })
        .expect("the interrupt is sent");
    let events = until_turn_completed(&provider);
    match the_completion(&events) {
        AgentDomainEvent::TurnCompleted { outcome, detail, .. } => {
            assert_eq!(*outcome, TurnOutcome::Interrupted);
            assert_eq!(detail, &Default::default());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn interrupt_sends_one_turn_interrupt_and_never_waits() {
    let interrupt = r#"{"id":5,"method":"turn/interrupt","params":{"threadId":"th-10","turnId":"turn-10"}}"#;
    let (provider, thread) = open(
        "th-10",
        vec![
            expect_request(4, "turn/start"),
            turn_response(4, "turn-10"),
            expect_exact(interrupt),
            // Codex does not answer the interrupt (id 5); it ends the turn.
            turn_completed("th-10", "turn-10", "interrupted", Value::Null),
            // The next line must be the next turn's start: a second interrupt would have taken id 6.
            expect_request(6, "turn/start"),
            turn_response(6, "turn-11"),
        ],
    );
    send(&provider, &thread, "go").expect("starts");
    let began = Instant::now();
    for _ in 0..3 {
        provider
            .interrupt_turn(InterruptTurnRequest {
                session_id: thread.clone(),
            })
            .expect("an interrupt is accepted");
    }
    assert!(
        began.elapsed() < Duration::from_secs(2),
        "interrupts must not wait for an answer"
    );
    let events = until_turn_completed(&provider);
    assert!(matches!(
        the_completion(&events),
        AgentDomainEvent::TurnCompleted {
            outcome: TurnOutcome::Interrupted,
            ..
        }
    ));
    assert_eq!(
        send(&provider, &thread, "next").expect("the next turn starts"),
        "turn-11"
    );
}

#[test]
fn interrupt_before_the_turn_id_is_held_until_the_answer() {
    let (provider, thread) = open(
        "th-12",
        vec![
            expect_request(4, "turn/start"),
            json!({"sleep_ms": 600}),
            turn_response(4, "turn-12"),
            expect_exact(r#"{"id":5,"method":"turn/interrupt","params":{"threadId":"th-12","turnId":"turn-12"}}"#),
            turn_completed("th-12", "turn-12", "interrupted", Value::Null),
        ],
    );
    let sender = {
        let provider = Arc::clone(&provider);
        let thread = thread.clone();
        std::thread::spawn(move || send(&provider, &thread, "go"))
    };
    // Wait until the turn is in flight, then press the key before its id is known.
    std::thread::sleep(Duration::from_millis(250));
    provider
        .interrupt_turn(InterruptTurnRequest {
            session_id: thread.clone(),
        })
        .expect("the interrupt is accepted");
    assert_eq!(
        sender.join().expect("the sender ran").expect("the turn started"),
        "turn-12"
    );
    let events = until_turn_completed(&provider);
    assert!(matches!(
        the_completion(&events),
        AgentDomainEvent::TurnCompleted {
            outcome: TurnOutcome::Interrupted,
            ..
        }
    ));
}

#[test]
fn close_unsubscribes_then_eof() {
    // After the unsubscribe the script ends, and the stand-in then reads stdin until EOF and exits 0: only an
    // EOF lets it go before the grace period, which is why the close must come back in well under 6 s.
    let (provider, thread) = open(
        "th-13",
        vec![
            expect_exact(r#"{"id":4,"method":"thread/unsubscribe","params":{"threadId":"th-13"}}"#),
            write(json!({"id": 4, "result": {"status": "unsubscribed"}})),
        ],
    );
    let pid = provider.pid();
    assert!(alive(pid));
    let began = Instant::now();
    provider
        .close_session(CloseSessionRequest {
            session_id: thread.clone(),
        })
        .expect("closes");
    assert!(began.elapsed() < Duration::from_secs(4), "took {:?}", began.elapsed());
    assert!(!alive(pid), "the child is gone and reaped");
    let events = provider.pump();
    assert_eq!(
        events.last(),
        Some(&AgentDomainEvent::SessionClosed {
            reason: "closed_by_host".into()
        })
    );
    // Closing twice is harmless.
    provider
        .close_session(CloseSessionRequest { session_id: thread })
        .expect("idempotent");
}

#[test]
fn closing_a_running_turn_interrupts_it_first() {
    let (provider, thread) = open(
        "th-14",
        vec![
            expect_request(4, "turn/start"),
            turn_response(4, "turn-14"),
            expect_exact(r#"{"id":5,"method":"turn/interrupt","params":{"threadId":"th-14","turnId":"turn-14"}}"#),
            expect_exact(r#"{"id":6,"method":"thread/unsubscribe","params":{"threadId":"th-14"}}"#),
            write(json!({"id": 6, "result": {"status": "unsubscribed"}})),
        ],
    );
    send(&provider, &thread, "go").expect("starts");
    provider
        .close_session(CloseSessionRequest { session_id: thread })
        .expect("closes");
    assert!(!alive(provider.pid()));
}
