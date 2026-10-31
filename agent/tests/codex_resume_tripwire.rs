//! Resuming a Codex thread by pages, the policy echo and the trust tripwires, against the scripted
//! stand-in for `codex app-server` (`eitri-agent-standin`). No real Codex runs and no model turn is spent.

use agent::providers::codex::config::expected_echo;
use agent::providers::codex::tripwire::{check_echo, check_instruction_sources, TrustWatch};
use agent::providers::codex::wire::ThreadEcho;
use agent::providers::codex::{CodexLaunch, CodexProvider};
use agent::{
    AgentDomainEvent, AgentProvider, ContentKind, DirTrust, ExternalProvider, ExternalStart, ProviderError,
    ProviderErrorCode, Reviewer, SendTurnRequest, TrustPlan, TurnOutcome, TurnPolicy,
};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const UA: &str = "eitri-probe/0.160.0 (Arch Linux Rolling Release; x86_64) tmux-256color (eitri-probe; 0)";
const CODEX_HOME: &str = "/home/user/.codex";

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

/// A line that starts with `{"id":<id>,"method":"<method>"`.
fn expect_request(id: u64, method: &str) -> Value {
    json!({ "expect": format!(r#"^\{{"id":{id},"method":"{method}""#) })
}

/// A request whose line also matches `pattern` (a regular expression over the whole line).
fn expect_request_like(id: u64, method: &str, pattern: &str) -> Value {
    json!({ "expect": format!(r#"^\{{"id":{id},"method":"{method}",{pattern}"#) })
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
        write(json!({"id": 1, "result": {"userAgent": UA, "codexHome": CODEX_HOME}})),
        expect_exact(r#"{"method":"initialized"}"#),
        expect_exact(r#"{"id":2,"method":"account/read","params":{"refreshToken":false}}"#),
        write(json!({"id": 2, "result": {
            "account": {"type": "chatgpt", "email": "someone@example.com", "planType": "pro"},
            "requiresOpenaiAuth": true,
        }})),
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

fn plan(project_only: &[&str]) -> TrustPlan {
    TrustPlan {
        dirs: vec![
            (PathBuf::from("/work/proj"), DirTrust::Untrusted),
            (PathBuf::from("/work"), DirTrust::Untrusted),
            (PathBuf::from("/"), DirTrust::Untrusted),
        ],
        skills_off: true,
        fingerprint: "fp".into(),
        project_only: project_only.iter().map(|s| s.to_string()).collect(),
    }
}

fn request(resume: Option<&str>, project_only: &[&str]) -> ExternalStart {
    ExternalStart {
        cwd: "/work/proj".into(),
        plan: plan(project_only),
        policy: TurnPolicy {
            reviewer: Reviewer::User,
            sandbox: agent::Sandbox::Project,
        },
        resume: resume.map(str::to_string),
    }
}

fn sandbox() -> Value {
    json!({"type": "workspaceWrite", "writableRoots": [], "networkAccess": false,
           "excludeTmpdirEnvVar": false, "excludeSlashTmp": false})
}

/// The answer to `thread/start` or `thread/resume`, echoing the policy that was asked for.
fn thread_answer(id: u64, thread: &str, extra: Value) -> Value {
    let mut result = json!({
        "thread": {"id": thread, "cwd": "/work/proj"},
        "model": "gpt-test",
        "cwd": "/work/proj",
        "instructionSources": [],
        "approvalPolicy": "on-request",
        "approvalsReviewer": "user",
        "sandbox": sandbox(),
    });
    if let (Some(base), Some(more)) = (result.as_object_mut(), extra.as_object()) {
        for (key, value) in more {
            if value.is_null() {
                base.remove(key);
            } else {
                base.insert(key.clone(), value.clone());
            }
        }
    }
    write(json!({"id": id, "result": result}))
}

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

fn mismatch_in(events: &[AgentDomainEvent]) -> Option<(&str, &str)> {
    events.iter().find_map(|e| match e {
        AgentDomainEvent::PolicyMismatch { requested, reported } => Some((requested.as_str(), reported.as_str())),
        _ => None,
    })
}

// ---------------------------------------------------------------- the policy echo

fn good_echo() -> ThreadEcho {
    ThreadEcho {
        approval_policy: Some(json!("on-request")),
        approvals_reviewer: Some(json!("user")),
        sandbox: Some(sandbox()),
    }
}

#[test]
fn the_expected_echo_passes() {
    assert!(check_echo(&expected_echo(Reviewer::User, agent::Sandbox::Project), &good_echo()).is_ok());
    let mut auto = good_echo();
    auto.approvals_reviewer = Some(json!("auto_review"));
    assert!(check_echo(&expected_echo(Reviewer::Agent, agent::Sandbox::Project), &auto).is_ok());
}

/// A session asked to be read-only that says it is not, and the other way round: either way what Eitri believes the
/// tab runs under is not what it runs under.
#[test]
fn the_sandbox_echoed_must_be_the_one_asked_for() {
    let read_only = ThreadEcho {
        sandbox: Some(json!({"type": "readOnly", "networkAccess": false})),
        ..good_echo()
    };
    let as_read_only = expected_echo(Reviewer::User, agent::Sandbox::ReadOnly);
    let as_workspace = expected_echo(Reviewer::User, agent::Sandbox::Project);
    assert!(check_echo(&as_read_only, &read_only).is_ok());
    assert!(check_echo(&as_read_only, &good_echo()).is_err(), "a wider sandbox");
    assert!(check_echo(&as_workspace, &read_only).is_err(), "a narrower one");
    let with_network = ThreadEcho {
        sandbox: Some(json!({"type": "readOnly", "networkAccess": true})),
        ..good_echo()
    };
    assert!(
        check_echo(&as_read_only, &with_network).is_err(),
        "read-only with network"
    );
    let silent = ThreadEcho {
        sandbox: None,
        ..good_echo()
    };
    assert!(check_echo(&as_read_only, &silent).is_err(), "no sandbox reported");
}

#[test]
fn echo_mismatch_trips() {
    let sent = expected_echo(Reviewer::User, agent::Sandbox::Project);
    let cases: Vec<(&str, Box<dyn Fn(&mut ThreadEcho)>)> = vec![
        (
            "a different reviewer",
            Box::new(|e| e.approvals_reviewer = Some(json!("auto_review"))),
        ),
        (
            "a different policy",
            Box::new(|e| e.approval_policy = Some(json!("never"))),
        ),
        (
            "network on",
            Box::new(|e| e.sandbox.as_mut().unwrap()["networkAccess"] = json!(true)),
        ),
        (
            "a writable root",
            Box::new(|e| e.sandbox.as_mut().unwrap()["writableRoots"] = json!(["/home/user"])),
        ),
        (
            "exclude tmpdir",
            Box::new(|e| e.sandbox.as_mut().unwrap()["excludeTmpdirEnvVar"] = json!(true)),
        ),
        (
            "exclude slash tmp",
            Box::new(|e| e.sandbox.as_mut().unwrap()["excludeSlashTmp"] = json!(true)),
        ),
        (
            "another sandbox",
            Box::new(|e| e.sandbox = Some(json!({"type": "dangerFullAccess"}))),
        ),
        (
            "a sandbox with a field Eitri did not ask for",
            Box::new(|e| e.sandbox.as_mut().unwrap()["readOnlyAccess"] = json!({"type": "fullAccess"})),
        ),
    ];
    for (name, change) in cases {
        let mut got = good_echo();
        change(&mut got);
        let mismatch = check_echo(&sent, &got).expect_err(name);
        assert!(
            !mismatch.requested.is_empty() && !mismatch.reported.is_empty(),
            "{name}"
        );
    }
}

#[test]
fn echo_without_sandbox_field_trips() {
    let mut got = good_echo();
    got.sandbox = None;
    let mismatch =
        check_echo(&expected_echo(Reviewer::User, agent::Sandbox::Project), &got).expect_err("an absent sandbox");
    assert!(mismatch.reported.contains("sandbox absent"), "{mismatch}");

    // The other two fields fail closed the same way.
    for field in ["approvalPolicy", "approvalsReviewer"] {
        let mut got = good_echo();
        match field {
            "approvalPolicy" => got.approval_policy = None,
            _ => got.approvals_reviewer = None,
        }
        assert!(
            check_echo(&expected_echo(Reviewer::User, agent::Sandbox::Project), &got).is_err(),
            "{field}"
        );
    }
}

// ---------------------------------------------------------------- trust tripwires

#[test]
fn instruction_source_inside_walked_dir_trips_but_codex_home_does_not() {
    let walked = vec![PathBuf::from("/work/proj"), PathBuf::from("/work")];
    let home = Path::new(CODEX_HOME);
    let sources = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();

    let at_root = check_instruction_sources(&sources(&["/work/proj/AGENTS.md"]), &walked, home);
    assert!(at_root
        .expect_err("a root file")
        .reported
        .contains("/work/proj/AGENTS.md"));
    assert!(check_instruction_sources(&sources(&["/work/AGENTS.override.md"]), &walked, home).is_err());
    // A spelling that goes round the directory is still that file.
    assert!(check_instruction_sources(&sources(&["/work/proj/../proj/./AGENTS.md"]), &walked, home).is_err());

    assert!(check_instruction_sources(&sources(&[&format!("{CODEX_HOME}/AGENTS.md")]), &walked, home).is_ok());
    assert!(check_instruction_sources(&sources(&[]), &walked, home).is_ok());
    // Somewhere else on the disk is not a walked directory.
    assert!(check_instruction_sources(&sources(&["/etc/codex/AGENTS.md"]), &walked, home).is_ok());

    // A root at the home directory: the user's own file under `.codex` passes, the home's own does not.
    let at_home = vec![PathBuf::from("/home/user")];
    assert!(check_instruction_sources(&sources(&[&format!("{CODEX_HOME}/AGENTS.md")]), &at_home, home).is_ok());
    assert!(check_instruction_sources(&sources(&["/home/user/AGENTS.md"]), &at_home, home).is_err());

    // A file Eitri cannot place is not taken for one outside the project.
    let relative = check_instruction_sources(&sources(&["AGENTS.md"]), &walked, home);
    assert!(relative.expect_err("a relative file").reported.contains("AGENTS.md"));
    assert!(check_instruction_sources(&sources(&["proj/AGENTS.md"]), &walked, home).is_err());

    // A root inside the user's Codex directory: only a file directly in it is the user's own.
    let in_home: Vec<PathBuf> = [
        "/home/user/.codex/proj",
        "/home/user/.codex",
        "/home/user",
        "/home",
        "/",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    assert!(check_instruction_sources(&sources(&["/home/user/.codex/proj/AGENTS.md"]), &in_home, home).is_err());
    assert!(check_instruction_sources(&sources(&[&format!("{CODEX_HOME}/AGENTS.md")]), &in_home, home).is_ok());
}

#[test]
fn trust_watch_names_project_only_servers_and_project_hooks() {
    let home = Path::new(CODEX_HOME);
    let watch = TrustWatch::for_plan(&plan(&["proj-server"]), home);
    assert!(watch.mcp_started("proj-server").is_err());
    assert!(watch.mcp_started("user-server").is_ok());
    let user_file = format!("{CODEX_HOME}/hooks.json");
    let user_config = format!("{CODEX_HOME}/config.toml");
    assert!(watch
        .hook_started(Some("project"), Some("/work/proj/.codex/hooks.json"))
        .is_err());
    assert!(watch.hook_started(Some("user"), Some(&user_file)).is_ok());
    assert!(watch.hook_started(Some("user"), Some(&user_config)).is_ok());
    // Only the layer word measured for the user's own hooks passes; Codex's own default, a word it may
    // add later, a missing word and a near spelling all stop the session.
    for source in [
        None,
        Some("unknown"),
        Some("repo"),
        Some("projects"),
        Some(" user"),
        Some("User"),
    ] {
        assert!(watch.hook_started(source, Some(&user_file)).is_err(), "{source:?}");
    }
    for source in ["system", "mdm", "plugin", "sessionFlags", "cloudRequirements"] {
        assert!(watch.hook_started(Some(source), Some(&user_file)).is_err(), "{source}");
    }
    // The user's word on a file Eitri cannot place in the user's own directory is not believed.
    for path in [
        None,
        Some("/work/proj/.codex/hooks.json"),
        Some("/work/proj/hooks.json"),
        Some("/work/hooks.json"),
        Some("hooks.json"),
        Some("/home/user/.codex/../hooks.json"),
        Some("/home/user/.codex/sub/hooks.json"),
    ] {
        assert!(watch.hook_started(Some("user"), path).is_err(), "{path:?}");
    }
    // An unreadable hook announcement or startup status may be the project's.
    assert!(watch.unreadable_hook().is_err());
    assert!(watch.unreadable_mcp_status().is_err());
    // An untrusted plan with no project-only server has no server to miss.
    assert!(TrustWatch::for_plan(&plan(&[]), home).unreadable_mcp_status().is_ok());

    // A root inside the user's Codex directory: only a file directly in it is the user's own.
    let mut nested = plan(&[]);
    nested.dirs = [
        "/home/user/.codex/proj",
        "/home/user/.codex",
        "/home/user",
        "/home",
        "/",
    ]
    .iter()
    .map(|d| (PathBuf::from(d), DirTrust::Untrusted))
    .collect();
    let watch = TrustWatch::for_plan(&nested, home);
    assert!(watch.hook_started(Some("user"), Some(&user_file)).is_ok());
    assert!(watch
        .hook_started(Some("user"), Some("/home/user/.codex/proj/.codex/hooks.json"))
        .is_err());

    // A trusted plan has nothing to watch.
    let mut trusted = plan(&["proj-server"]);
    trusted.skills_off = false;
    let watch = TrustWatch::for_plan(&trusted, home);
    assert!(watch.mcp_started("proj-server").is_ok());
    assert!(watch
        .hook_started(Some("project"), Some("/work/proj/.codex/hooks.json"))
        .is_ok());
    assert!(watch.hook_started(None, None).is_ok());
    assert!(watch.unreadable_hook().is_ok());
    assert!(watch.unreadable_mcp_status().is_ok());
}

/// A session on `th-t`, then `rest`.
fn started_session(project_only: &[&str], rest: Vec<Value>) -> CodexProvider {
    let mut script = handshake();
    script.push(expect_request(3, "thread/start"));
    script.push(thread_answer(3, "th-t", json!({})));
    script.extend(rest);
    let provider = connect(script);
    provider.start(request(None, project_only)).expect("the session starts");
    provider
}

/// A tripwire Codex sent before the user's turn is read before the turn is sent: the turn is refused, not
/// sent under a policy that already failed.
#[test]
fn a_turn_after_an_unread_tripwire_is_refused() {
    let provider = started_session(
        &[],
        vec![write(json!({"method": "hook/started", "params": {
            "threadId": "th-t", "turnId": "tu-0",
            "run": {"id": "h", "source": "project", "sourcePath": "/work/proj/.codex/hooks.json"},
        }}))],
    );
    // The notification is in, not yet pumped.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let started = std::time::Instant::now();
    let sent = provider.send_turn_with(
        SendTurnRequest {
            session_id: "th-t".into(),
            text: "go".into(),
        },
        TurnPolicy {
            reviewer: Reviewer::User,
            sandbox: agent::Sandbox::Project,
        },
    );
    match sent {
        Err(ProviderError::Provider { code, .. }) => assert_eq!(code, ProviderErrorCode::ProviderUnavailable),
        other => panic!("the turn was not refused: {other:?} after {:?}", started.elapsed()),
    }
    let events = pump_until(&provider, |events| mismatch_in(events).is_some());
    assert!(mismatch_in(&events).is_some());
}

#[test]
fn project_only_mcp_startup_trips_on_any_family_thread() {
    // The tab's own thread, and one the family does not hold: both are Codex starting a server only the
    // project names.
    for thread in ["th-t", "th-child"] {
        let provider = started_session(
            &["proj-server"],
            vec![
                write(json!({"method": "mcpServer/startupStatus/updated", "params": {
                    "threadId": "th-t", "name": "user-server", "status": "starting",
                }})),
                write(json!({"method": "mcpServer/startupStatus/updated", "params": {
                    "threadId": thread, "name": "proj-server", "status": "starting",
                }})),
            ],
        );
        let events = pump_until(&provider, |events| mismatch_in(events).is_some());
        let (_, reported) = mismatch_in(&events).expect("a mismatch");
        assert!(
            reported.contains("project configuration Eitri did not allow"),
            "{reported}"
        );
        assert!(reported.contains("proj-server"), "{reported}");
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentDomainEvent::PolicyMismatch { .. }))
                .count(),
            1
        );
    }
}

#[test]
fn project_hook_start_trips() {
    let hook = |source: &str, path: &str| {
        write(json!({"method": "hook/started", "params": {
            "threadId": "th-t", "turnId": "tu-1",
            "run": {"id": "pre-tool-use:0:x:c1", "eventName": "preToolUse", "source": source, "sourcePath": path},
        }}))
    };
    let provider = started_session(
        &[],
        vec![
            hook("user", &format!("{CODEX_HOME}/hooks.json")),
            hook("project", "/work/proj/.codex/hooks.json"),
        ],
    );
    let events = pump_until(&provider, |events| mismatch_in(events).is_some());
    let (_, reported) = mismatch_in(&events).expect("a mismatch");
    assert!(reported.contains("hook"), "{reported}");
}

/// A hook start that does not say its layer, or cannot be read at all, may be the project's: an untrusted
/// session ends on it.
#[test]
fn a_hook_start_without_a_layer_or_unreadable_trips() {
    let user_hook = write(json!({"method": "hook/started", "params": {
        "threadId": "th-t", "turnId": "tu-1",
        "run": {"id": "u", "source": "user", "sourcePath": format!("{CODEX_HOME}/hooks.json")},
    }}));
    let no_source = write(json!({"method": "hook/started", "params": {
        "threadId": "th-t", "turnId": "tu-1", "run": {"id": "h"},
    }}));
    let no_id = write(json!({"method": "hook/started", "params": {
        "threadId": "th-t", "turnId": "tu-1", "run": {"source": "user"},
    }}));
    for (what, hook) in [("no source", no_source), ("no run id", no_id)] {
        let provider = started_session(&[], vec![user_hook.clone(), hook]);
        let events = pump_until(&provider, |events| mismatch_in(events).is_some());
        let (_, reported) = mismatch_in(&events).expect("a mismatch");
        assert!(reported.contains("hook"), "{what}: {reported}");
    }
}

/// An unreadable startup status may name a server only the project gives.
#[test]
fn an_unreadable_mcp_status_trips_when_a_project_server_is_watched() {
    let provider = started_session(
        &["proj-server"],
        vec![write(json!({"method": "mcpServer/startupStatus/updated", "params": {
            "threadId": "th-t", "status": "starting",
        }}))],
    );
    let events = pump_until(&provider, |events| mismatch_in(events).is_some());
    let (_, reported) = mismatch_in(&events).expect("a mismatch");
    assert!(reported.contains("tool server"), "{reported}");
}

#[test]
fn a_tripped_session_takes_no_turn() {
    let provider = started_session(
        &[],
        vec![write(json!({"method": "hook/started", "params": {
            "threadId": "th-t", "turnId": "tu-1", "run": {"id": "h", "source": "project"},
        }}))],
    );
    pump_until(&provider, |events| mismatch_in(events).is_some());
    let refused = provider.send_turn_with(
        SendTurnRequest {
            session_id: "th-t".into(),
            text: "go".into(),
        },
        TurnPolicy {
            reviewer: Reviewer::User,
            sandbox: agent::Sandbox::Project,
        },
    );
    assert!(matches!(
        refused,
        Err(ProviderError::Provider {
            code: ProviderErrorCode::ProviderUnavailable,
            ..
        })
    ));
}

#[test]
fn a_start_whose_answer_lacks_the_sandbox_fails_the_tab() {
    let mut script = handshake();
    script.push(expect_request(3, "thread/start"));
    script.push(thread_answer(3, "th-t", json!({"sandbox": null})));
    let provider = connect(script);
    provider.start(request(None, &[])).expect("the session starts");
    let events = provider.pump();
    let (requested, reported) = mismatch_in(&events).expect("a mismatch");
    assert!(requested.contains("sandbox"), "{requested}");
    assert!(reported.contains("sandbox absent"), "{reported}");
}

#[test]
fn a_start_that_loaded_a_project_instruction_file_fails_the_tab() {
    let mut script = handshake();
    script.push(expect_request(3, "thread/start"));
    script.push(thread_answer(
        3,
        "th-t",
        json!({"instructionSources": [format!("{CODEX_HOME}/AGENTS.md"), "/work/proj/AGENTS.md"]}),
    ));
    let provider = connect(script);
    provider.start(request(None, &[])).expect("the session starts");
    let events = provider.pump();
    let (_, reported) = mismatch_in(&events).expect("a mismatch");
    assert!(reported.contains("/work/proj/AGENTS.md"), "{reported}");
}

// ---------------------------------------------------------------- resume

fn turn_meta(id: &str, status: &str) -> Value {
    json!({"id": id, "items": [], "itemsView": "notLoaded", "status": status, "error": null})
}

fn turns_page(id: u64, turns: Vec<Value>, next: Option<&str>) -> Value {
    write(json!({"id": id, "result": {"data": turns, "nextCursor": next}}))
}

fn items_page(id: u64, turn: &str, items: Vec<Value>) -> Value {
    let data: Vec<Value> = items
        .into_iter()
        .map(|item| json!({"turnId": turn, "item": item}))
        .collect();
    write(json!({"id": id, "result": {"data": data, "nextCursor": null}}))
}

fn user_message(id: &str, text: &str) -> Value {
    json!({"type": "userMessage", "id": id, "content": [{"type": "text", "text": text, "text_elements": []}]})
}

fn agent_message(id: &str, text: &str) -> Value {
    json!({"type": "agentMessage", "id": id, "text": text})
}

fn rate_limits() -> Value {
    write(
        json!({"method": "account/rateLimits/updated", "params": {"rateLimits": {
            "limitId": "codex",
            "primary": {"usedPercent": 12.0, "windowDurationMins": 300, "resetsAt": 1_790_000_000},
            "planType": "pro",
        }}}),
    )
}

fn kind(event: &AgentDomainEvent) -> &'static str {
    match event {
        AgentDomainEvent::SessionOpened { .. } => "opened",
        AgentDomainEvent::ResumeOutcome { .. } => "resume",
        AgentDomainEvent::UserPromptSubmitted { .. } => "prompt",
        AgentDomainEvent::TurnStarted { .. } => "turn+",
        AgentDomainEvent::TurnCompleted { .. } => "turn-",
        AgentDomainEvent::ContentDelta { .. } => "text",
        AgentDomainEvent::AssistantMessageBoundary { .. } => "boundary",
        AgentDomainEvent::ToolCallStarted { .. } => "call+",
        AgentDomainEvent::ToolCallCompleted { .. } => "call-",
        AgentDomainEvent::AccountLimits { .. } => "limits",
        _ => "other",
    }
}

#[test]
fn resume_pages_history_then_releases_live_events_in_order() {
    let mut script = handshake();
    script.push(expect_request_like(
        3,
        "thread/resume",
        r#""params":\{.*"excludeTurns":true.*"threadId":"th-old".*\}\}$"#,
    ));
    script.push(thread_answer(3, "th-old", json!({})));
    // A live notification right behind the answer, before any history was asked for.
    script.push(rate_limits());
    script.push(expect_request_like(
        4,
        "thread/turns/list",
        r#""params":\{"itemsView":"notLoaded","limit":10,"sortDirection":"desc","threadId":"th-old"\}\}$"#,
    ));
    script.push(turns_page(
        4,
        vec![turn_meta("tu-3", "completed"), turn_meta("tu-2", "interrupted")],
        Some("cursor-2"),
    ));
    script.push(expect_request_like(
        5,
        "thread/items/list",
        r#""params":\{"limit":50,"threadId":"th-old","turnId":"tu-3"\}\}$"#,
    ));
    script.push(items_page(5, "tu-3", vec![agent_message("a3", "third answer")]));
    script.push(expect_request(6, "thread/items/list"));
    script.push(items_page(6, "tu-2", vec![user_message("u2", "second question")]));
    script.push(expect_request_like(
        7,
        "thread/turns/list",
        r#""params":\{"cursor":"cursor-2","itemsView":"notLoaded","limit":10,"sortDirection":"desc","threadId":"th-old"\}\}$"#,
    ));
    script.push(turns_page(7, vec![turn_meta("tu-1", "completed")], None));
    script.push(expect_request(8, "thread/items/list"));
    script.push(items_page(
        8,
        "tu-1",
        vec![
            user_message("u1", "first question"),
            agent_message("a1", "first answer"),
            json!({"type": "commandExecution", "id": "c1", "command": "ls", "cwd": "/work/proj",
                   "status": "completed", "aggregatedOutput": "a\nb\n", "exitCode": 0}),
        ],
    ));

    let provider = connect(script);
    let id = provider
        .start(request(Some("th-old"), &[]))
        .expect("the resume completes");
    assert_eq!(id, "th-old");
    // The history is handed over whole by the time the start returns, apart from the live stream: the
    // host folds it into the projection before ingestion starts, so no past turn passes for a live one.
    let history = provider.take_history();
    assert!(provider.take_history().is_empty(), "taken once");
    let kinds: Vec<&str> = history.iter().map(kind).collect();
    assert_eq!(
        kinds,
        vec![
            "prompt", "turn+", "boundary", "text", "call+", "call-", "turn-", // tu-1
            "prompt", "turn+", "turn-", // tu-2
            "turn+", "boundary", "text", "turn-", // tu-3
        ]
    );
    let events = pump_until(&provider, |events| events.iter().any(|e| kind(e) == "limits"));
    let live: Vec<&str> = events.iter().map(kind).collect();
    assert_eq!(
        live,
        vec!["opened", "resume", "limits"],
        "only the session and the live message are live"
    );
    assert!(matches!(
        &events[1],
        AgentDomainEvent::ResumeOutcome { status: agent::ResumeStatus::Attached, attached_provider_session_id: Some(a), .. }
            if a == "th-old"
    ));
    assert!(matches!(&history[0], AgentDomainEvent::UserPromptSubmitted { text } if text == "first question"));
    assert!(matches!(
        &history[3],
        AgentDomainEvent::ContentDelta { kind: ContentKind::Text, text, .. } if text == "first answer"
    ));
    // The turn that was cut off reads interrupted; the others completed.
    let outcomes: Vec<TurnOutcome> = history
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::TurnCompleted { outcome, .. } => Some(*outcome),
            _ => None,
        })
        .collect();
    assert_eq!(
        outcomes,
        vec![TurnOutcome::Completed, TurnOutcome::Interrupted, TurnOutcome::Completed]
    );
}

#[test]
fn over_cap_history_page_is_skipped_not_fatal() {
    let mut script = handshake();
    script.push(expect_request(3, "thread/resume"));
    script.push(thread_answer(3, "th-old", json!({})));
    script.push(expect_request(4, "thread/turns/list"));
    script.push(turns_page(
        4,
        vec![turn_meta("tu-2", "completed"), turn_meta("tu-1", "completed")],
        None,
    ));
    script.push(expect_request(5, "thread/items/list"));
    // 17 MiB on one line: past the cap, so it is skipped and the request it answers fails.
    script.push(json!({"write_big": {
        "prefix": r#"{"id":5,"result":{"data":[{"turnId":"tu-2","item":{"type":"agentMessage","id":"a1","text":""#,
        "bytes": 17 * 1024 * 1024,
        "suffix": r#""}}],"nextCursor":null}}"#,
    }}));
    script.push(expect_request(6, "thread/items/list"));
    script.push(items_page(6, "tu-1", vec![agent_message("a1", "still readable")]));

    let provider = connect(script);
    provider
        .start(request(Some("th-old"), &[]))
        .expect("the resume completes");
    let events = provider.take_history();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentDomainEvent::TurnCompleted { .. }))
            .count(),
        2,
        "{events:#?}"
    );
    let note = events
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::ToolCallStarted {
                name, input, turn_id, ..
            } if name == "history" => Some((turn_id.as_str(), input.to_string())),
            _ => None,
        })
        .expect("a note on the turn that was too large");
    assert_eq!(note.0, "tu-2");
    assert!(note.1.contains("too large to show"), "{}", note.1);
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentDomainEvent::ContentDelta { text, .. } if text == "still readable")));
    assert!(mismatch_in(&events).is_none());
}

#[test]
fn resume_of_a_thread_held_elsewhere_is_rejected_with_codex_message() {
    let mut script = handshake();
    script.push(expect_request(3, "thread/resume"));
    script.push(write(json!({"id": 3, "error": {
        "code": -32600,
        "message": "thread th-held is open in another Codex client",
    }})));
    let provider = connect(script);
    let id = provider
        .start(request(Some("th-held"), &[]))
        .expect("a refusal is a verdict, not a failure to start");
    assert_eq!(id, "th-held");
    let events = provider.pump();
    assert!(matches!(
        events.as_slice(),
        [
            AgentDomainEvent::SessionOpened { .. },
            AgentDomainEvent::ResumeOutcome {
                status: agent::ResumeStatus::Rejected,
                requested_provider_session_id,
                detail: Some(detail),
                ..
            },
        ] if requested_provider_session_id == "th-held" && detail == "thread th-held is open in another Codex client"
    ));
    let refused = provider.send_turn_with(
        SendTurnRequest {
            session_id: "th-held".into(),
            text: "go".into(),
        },
        TurnPolicy {
            reviewer: Reviewer::User,
            sandbox: agent::Sandbox::Project,
        },
    );
    assert!(refused.is_err());
}

/// A resume whose answer names another thread did not continue the conversation asked for: nothing of the
/// other thread is read or shown, the verdict says so, and no turn goes to it.
#[test]
fn a_resume_that_continues_another_thread_is_not_taken() {
    let mut script = handshake();
    script.push(expect_request(3, "thread/resume"));
    script.push(thread_answer(3, "th-other", json!({})));
    let provider = connect(script);
    let started = std::time::Instant::now();
    let id = provider
        .start(request(Some("th-old"), &[]))
        .expect("a verdict, not a failure to start");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "the other thread's history was asked for"
    );
    assert_eq!(id, "th-old");
    let events = provider.pump();
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentDomainEvent::ResumeOutcome {
                status: agent::ResumeStatus::Attached,
                requested_provider_session_id,
                attached_provider_session_id: Some(attached),
                ..
            } if requested_provider_session_id == "th-old" && attached == "th-other"
        )),
        "{events:#?}"
    );
    assert!(provider.take_history().is_empty());
    let refused = provider.send_turn_with(
        SendTurnRequest {
            session_id: "th-old".into(),
            text: "go".into(),
        },
        TurnPolicy {
            reviewer: Reviewer::User,
            sandbox: agent::Sandbox::Project,
        },
    );
    assert!(
        matches!(
            refused,
            Err(ProviderError::Provider {
                code: ProviderErrorCode::ProviderUnavailable,
                ..
            })
        ),
        "{refused:?}"
    );
}

/// A tripwire behind more messages than one batch reads still stops the next turn: the send reads on until
/// nothing Codex already said is left.
#[test]
fn a_tripwire_behind_a_flood_still_stops_the_next_turn() {
    const FLOOD: usize = 5000;
    let mut script = handshake();
    script.push(expect_request(3, "thread/resume"));
    script.push(thread_answer(3, "th-old", json!({})));
    script.push(expect_request(4, "thread/turns/list"));
    for n in 0..FLOOD {
        script.push(write(
            json!({"method": "account/rateLimits/updated", "params": {"rateLimits": {
                "limitId": "codex",
                "primary": {"usedPercent": n as f64 / 100.0, "windowDurationMins": 300, "resetsAt": 1_790_000_000},
                "planType": "pro",
            }}}),
        ));
    }
    script.push(write(json!({"method": "hook/started", "params": {
        "threadId": "th-old", "turnId": "tu-0",
        "run": {"id": "h", "source": "project", "sourcePath": "/work/proj/.codex/hooks.json"},
    }})));
    script.push(turns_page(4, vec![turn_meta("tu-1", "completed")], None));
    let file = std::env::temp_dir().join(format!("codex-flood-trip-script-{}.json", std::process::id()));
    std::fs::write(&file, Value::Array(script).to_string()).expect("the script file");
    let provider = CodexProvider::connect_with_args(launch(), vec![OsString::from(format!("@{}", file.display()))])
        .expect("the handshake passes");
    provider
        .start(request(Some("th-old"), &[]))
        .expect("the resume completes");
    let _ = std::fs::remove_file(&file);
    std::thread::sleep(Duration::from_millis(300));
    let sent = provider.send_turn_with(
        SendTurnRequest {
            session_id: "th-old".into(),
            text: "go".into(),
        },
        TurnPolicy {
            reviewer: Reviewer::User,
            sandbox: agent::Sandbox::Project,
        },
    );
    assert!(
        matches!(
            sent,
            Err(ProviderError::Provider {
                code: ProviderErrorCode::ProviderUnavailable,
                ..
            })
        ),
        "{sent:?}"
    );
}

/// Codex says more while the history is read than can be held for after it: the history stops with a note
/// instead of leaving the link's reader blocked behind the messages, and every held message is still
/// delivered, in order, once the start returns.
#[test]
fn a_flood_while_the_history_is_read_stops_it_and_keeps_the_live_messages() {
    const FLOOD: usize = 5000;
    let mut script = handshake();
    script.push(expect_request(3, "thread/resume"));
    script.push(thread_answer(3, "th-old", json!({})));
    script.push(expect_request(4, "thread/turns/list"));
    for n in 0..FLOOD {
        script.push(write(
            json!({"method": "account/rateLimits/updated", "params": {"rateLimits": {
                "limitId": "codex",
                "primary": {"usedPercent": n as f64 / 100.0, "windowDurationMins": 300, "resetsAt": 1_790_000_000},
                "planType": "pro",
            }}}),
        ));
    }
    script.push(turns_page(4, vec![turn_meta("tu-1", "completed")], None));
    let file = std::env::temp_dir().join(format!("codex-flood-script-{}.json", std::process::id()));
    std::fs::write(&file, Value::Array(script).to_string()).expect("the script file");
    let provider = CodexProvider::connect_with_args(launch(), vec![OsString::from(format!("@{}", file.display()))])
        .expect("the handshake passes");
    let started = Instant::now();
    provider
        .start(request(Some("th-old"), &[]))
        .expect("the resume completes");
    let _ = std::fs::remove_file(&file);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the start waited out a page: {:?}",
        started.elapsed()
    );
    let history = provider.take_history();
    let note = history
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::ToolCallStarted { name, input, .. } if name == "history" => Some(input.to_string()),
            _ => None,
        })
        .expect("a note on the history");
    assert!(note.contains("sent too much"), "{note}");
    let events = pump_until(&provider, |events| {
        events.iter().filter(|e| kind(e) == "limits").count() == FLOOD
    });
    let figures: Vec<f64> = events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::AccountLimits { used_percent, .. } => Some(*used_percent),
            _ => None,
        })
        .collect();
    assert!(
        figures.windows(2).all(|w| w[0] < w[1]),
        "the live messages kept their order"
    );
}
