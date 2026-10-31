//! `CodexProvider` against a scripted stand-in for `codex app-server` (`eitri-agent-standin`). The
//! scripts are built from the frames recorded from Codex 0.160.0 (`tests/fixtures/codex/`) where a
//! recording has the shape, and by hand for the shapes no run produced (reasoning, a file change, a
//! failed turn). No real Codex runs here and no model turn is spent.

use agent::providers::codex::provider::NOT_FOUND;
use agent::providers::codex::{parse_user_agent_version, BinaryKey, CodexLaunch, CodexProvider, MIN_CODEX};
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
        policy: TurnPolicy {
            reviewer,
            sandbox: agent::Sandbox::Project,
        },
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
            sandbox: agent::Sandbox::Project,
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
fn a_newer_codex_runs_with_a_note_and_a_tested_one_states_the_tested_range() {
    let tested = connect(handshake(UA, signed_in())).expect("connects");
    assert!(tested.info().startup_diagnostics.is_empty());
    assert_eq!(
        tested.info().build_description.as_deref(),
        Some("Codex 0.160.0 (tested 0.160.0 to 0.161.0)")
    );
    let highest = connect(handshake("codex/0.161.0 (Arch)", signed_in())).expect("connects");
    assert!(highest.info().startup_diagnostics.is_empty(), "0.161.0 is tested");
    assert_eq!(
        highest.info().build_description.as_deref(),
        Some("Codex 0.161.0 (tested 0.160.0 to 0.161.0)")
    );
    let newer = connect(handshake("codex/0.162.0 (Arch)", signed_in())).expect("connects");
    assert_eq!(newer.info().build_description.as_deref(), Some("Codex 0.162.0"));
    assert_eq!(
        newer.info().startup_diagnostics,
        vec!["untested Codex 0.162.0 (tested up to 0.161.0)".to_string()]
    );
}

#[test]
fn the_info_names_the_configuration_directory_codex_reported() {
    let provider = connect(handshake(UA, signed_in())).expect("connects");
    assert_eq!(provider.info().configuration_home.as_deref(), Some("/x"));
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
///
/// A child writes the script, never this process: a write fd held here would be copied into any child
/// another test thread forks in the meantime, and until that child `exec`s, running the script fails
/// with `ETXTBSY`. The start then fails before the script runs, so a test expecting the marker sees none.
fn marker_program(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("eitri-{name}-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("ran");
    let program = dir.join("codex");
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(r#"printf '%s' "$1" > "$2" && chmod 755 "$2""#)
        .arg("sh")
        .arg(format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()))
        .arg(&program)
        .status()
        .unwrap();
    assert!(status.success(), "writing the test program failed: {status}");
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

/// An absolute `CODEX_HOME` that names the project root or a directory in it would make a directory the
/// project ships load as the user's own configuration layer, which no trust answer covers. The start is
/// refused, naming the variable and the root, before any child runs; a home outside the project, or one
/// under a root that holds the user's own home, is not refused.
#[test]
fn a_codex_home_inside_the_project_refuses_the_start_before_anything_runs() {
    use std::os::unix::fs::symlink;
    let outside = std::env::temp_dir().join(format!("eitri-outside-home-{}", std::process::id()));
    std::fs::create_dir_all(&outside).unwrap();
    type Case = (
        &'static str,
        fn(&std::path::Path, &std::path::Path) -> Vec<(OsString, OsString)>,
        Vec<&'static str>,
    );
    let refused: Vec<Case> = vec![
        (
            "the root's .codex",
            |root, _| vec![("CODEX_HOME".into(), root.join(".codex").into())],
            vec![],
        ),
        (
            "the root itself",
            |root, _| vec![("CODEX_HOME".into(), root.into())],
            vec![],
        ),
        (
            "a directory not made yet",
            |root, _| vec![("CODEX_HOME".into(), root.join("a/b/c").into())],
            vec![],
        ),
        (
            "a link in the project to a directory outside",
            |root, outside| {
                symlink(outside, root.join("away")).unwrap();
                vec![("CODEX_HOME".into(), root.join("away").into())]
            },
            vec![],
        ),
        (
            "a link outside to the project",
            |root, outside| {
                let link = outside.join(format!("into-{}", root.file_name().unwrap().to_string_lossy()));
                symlink(root, &link).unwrap();
                vec![("CODEX_HOME".into(), link.join(".codex").into())]
            },
            vec![],
        ),
        (
            "an unset one whose ~/.codex is the root",
            |root, _| vec![("HOME".into(), root.parent().unwrap().into())],
            vec!["CODEX_HOME"],
        ),
    ];
    for (what, env, remove) in refused {
        let (dir, program, marker) = marker_program("home-in-project");
        // The project is a directory of its own whose name is `.codex` only in the last case.
        let root = if remove.is_empty() {
            dir.join("project")
        } else {
            dir.join(".codex")
        };
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let launch = CodexLaunch {
            program,
            cwd: root.clone(),
            state_home: dir.clone(),
        };
        let env = env(&root, &outside);
        let remove = remove.into_iter().map(OsString::from).collect();
        match CodexProvider::connect_with_env(launch, Vec::new(), env, remove) {
            Err(ProviderError::Provider { code, message }) => {
                assert_eq!(code, ProviderErrorCode::InvalidConfiguration, "{what}");
                assert!(
                    message.contains("CODEX_HOME") && message.contains(&root.display().to_string()),
                    "{what}: {message}"
                );
            }
            Err(other) => panic!("expected a refusal for {what}, got {other}"),
            Ok(_) => panic!("a CODEX_HOME inside the project ({what}) connected"),
        }
        assert!(!marker.exists(), "a child ran with {what}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    let allowed: Vec<Case> = vec![
        (
            "a home outside the project",
            |_, outside| vec![("CODEX_HOME".into(), outside.join(".codex").into())],
            vec![],
        ),
        (
            "a root that holds the user's home",
            |root, _| {
                vec![
                    ("HOME".into(), root.join("user").into()),
                    ("CODEX_HOME".into(), root.join("user/.codex").into()),
                ]
            },
            vec![],
        ),
        (
            "an unset one under a root that holds the user's home",
            |root, _| vec![("HOME".into(), root.join("user").into())],
            vec!["CODEX_HOME"],
        ),
        (
            "the root is the user's home",
            |root, _| vec![("HOME".into(), root.into())],
            vec!["CODEX_HOME"],
        ),
    ];
    for (what, env, remove) in allowed {
        let (dir, program, marker) = marker_program("home-outside-project");
        let root = std::fs::canonicalize(&dir).unwrap();
        let launch = CodexLaunch {
            program,
            cwd: root.clone(),
            state_home: dir.clone(),
        };
        let env = env(&root, &outside);
        let remove = remove.into_iter().map(OsString::from).collect();
        assert!(CodexProvider::connect_with_env(launch, Vec::new(), env, remove).is_err());
        assert!(marker.exists(), "{what} was refused");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::remove_dir_all(&outside).unwrap();
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

/// A session at a root that is no boundary: the start and each turn name the read-only sandbox, and an answer that
/// names any other stops the tab.
#[test]
fn a_read_only_session_says_so_at_its_start_and_every_turn() {
    let request = ExternalStart {
        policy: TurnPolicy {
            reviewer: Reviewer::User,
            sandbox: agent::Sandbox::ReadOnly,
        },
        ..start_request(Reviewer::User)
    };
    let read_only_answer = |id: u64| {
        write(json!({"id": id, "result": {
            "thread": {"id": "th-ro", "cwd": "/work/proj"},
            "model": "gpt-test",
            "cwd": "/work/proj",
            "approvalPolicy": "on-request",
            "approvalsReviewer": "user",
            "sandbox": {"type": "readOnly", "networkAccess": false},
        }}))
    };
    let turn_line = concat!(
        r#"{"id":4,"method":"turn/start","params":{"approvalPolicy":"on-request","approvalsReviewer":"user","#,
        r#""input":[{"text":"hello","type":"text"}],"#,
        r#""sandboxPolicy":{"networkAccess":false,"type":"readOnly"},"threadId":"th-ro"}}"#,
    );
    let mut script = handshake(UA, signed_in());
    script.push(json!({ "expect": r#""sandbox":"read-only""# }));
    script.push(read_only_answer(3));
    script.push(expect_exact(turn_line));
    script.push(turn_response(4, "turn-ro"));
    let provider = connect(script).expect("connects");
    assert_eq!(provider.start(request.clone()).expect("starts"), "th-ro");
    let events = provider.pump();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PolicyMismatch { .. })),
        "{events:?}"
    );
    let turn = provider
        .send_turn_with(
            SendTurnRequest {
                session_id: "th-ro".to_string(),
                text: "hello".to_string(),
            },
            request.policy,
        )
        .expect("the turn starts");
    assert_eq!(turn, "turn-ro");

    let mut script = handshake(UA, signed_in());
    script.push(expect_request(3, "thread/start"));
    script.push(thread_response(3, "th-wide"));
    let provider = connect(script).expect("connects");
    provider.start(request).expect("starts");
    assert!(
        provider
            .pump()
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PolicyMismatch { .. })),
        "a session that came up on the workspace sandbox runs nothing"
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

fn part_note(method: &str, thread: &str, turn: &str, item: &str, index: (&str, i64), delta: &str) -> Value {
    write(json!({"method": method, "params": {
        "threadId": thread, "turnId": turn, "itemId": item, "delta": delta, index.0: index.1,
    }}))
}

/// What a resumed turn holding the same reasoning items reads as: `resume.rs`'s
/// `reasoning_reads_as_it_streamed` folds them to this same text.
const REASONING_AS_ONE_BLOCK: &str = "weigh options\n\nthen act\n\nraw one\n\nraw two\n\nwhole a\n\nwhole b";

/// Reasoning parts and back-to-back reasoning items stream into one thinking block, separated by the blank
/// line a resumed turn joins them with, so a conversation reads the same live and resumed.
#[test]
fn reasoning_streams_as_it_resumes() {
    let (t, u) = ("th-9", "turn-9");
    let reasoning = |id: &str, summary: Value, content: Value| json!({"type": "reasoning", "id": id, "summary": summary, "content": content});
    let summary = "item/reasoning/summaryTextDelta";
    let raw = "item/reasoning/textDelta";
    let (provider, thread) = open(
        t,
        vec![
            expect_request(4, "turn/start"),
            turn_response(4, u),
            item_note("item/started", t, u, reasoning("r1", json!([]), json!([]))),
            part_note(summary, t, u, "r1", ("summaryIndex", 0), "weigh "),
            part_note(summary, t, u, "r1", ("summaryIndex", 0), "options"),
            part_note(summary, t, u, "r1", ("summaryIndex", 1), "then act"),
            item_note(
                "item/completed",
                t,
                u,
                reasoning("r1", json!(["weigh options", "then act"]), json!([])),
            ),
            item_note("item/started", t, u, reasoning("r2", json!([]), json!([]))),
            part_note(raw, t, u, "r2", ("contentIndex", 0), "raw one"),
            part_note(raw, t, u, "r2", ("contentIndex", 1), "raw two"),
            item_note(
                "item/completed",
                t,
                u,
                reasoning("r2", json!([]), json!(["raw one", "raw two"])),
            ),
            item_note(
                "item/completed",
                t,
                u,
                reasoning("r3", json!(["whole a", "whole b"]), json!([])),
            ),
            item_note(
                "item/completed",
                t,
                u,
                json!({"type": "agentMessage", "id": "m1", "text": "done"}),
            ),
            // After a reply, a thought is a new block and starts with no blank line.
            item_note("item/started", t, u, reasoning("r4", json!([]), json!([]))),
            part_note(summary, t, u, "r4", ("summaryIndex", 0), "after"),
            item_note("item/completed", t, u, reasoning("r4", json!(["after"]), json!([]))),
            turn_completed(t, u, "completed", Value::Null),
        ],
    );
    send(&provider, &thread, "go").expect("starts");
    let mut projection = agent::AgentSessionProjection::default();
    for event in until_turn_completed(&provider) {
        projection.apply(&event);
    }
    let blocks: Vec<&str> = projection.thinking.iter().map(|b| b.text.as_str()).collect();
    assert_eq!(blocks, [REASONING_AS_ONE_BLOCK, "after"]);
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
    // The second item follows the first in the same block, after the blank line a resumed turn puts there.
    assert_eq!(text_of(ContentKind::Thinking), ["weigh ", "options", "\n\na\n\nb"]);
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

/// A row of the error table: the `codexErrorInfo` as Codex writes it, and what the turn becomes.
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
fn turn_endings_follow_the_codex_error_info_table() {
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

// ---------------------------------------------------------------- what an interrupt leaves running

fn command(id: &str, status: &str) -> Value {
    json!({"type": "commandExecution", "id": id, "command": "for i in 1 2 3; do sleep 1; done", "cwd": "/work/proj",
           "status": status, "aggregatedOutput": if status == "inProgress" { Value::Null } else { json!("all three") },
           "exitCode": if status == "inProgress" { Value::Null } else { json!(0) }})
}

/// A session whose turn starts `item` and is interrupted: Codex ends the turn at once, then (after `then`)
/// whatever the script says.
fn interrupted_with(thread: &str, turn: &str, item: Value, then: Vec<Value>) -> (Arc<CodexProvider>, String) {
    let mut rest = vec![
        expect_request(4, "turn/start"),
        turn_response(4, turn),
        item_note("item/started", thread, turn, item),
        expect_exact(&format!(
            r#"{{"id":5,"method":"turn/interrupt","params":{{"threadId":"{thread}","turnId":"{turn}"}}}}"#
        )),
        turn_completed(thread, turn, "interrupted", Value::Null),
    ];
    rest.extend(then);
    let (provider, id) = open(thread, rest);
    send(&provider, &id, "go").expect("starts");
    // The item is read before the key is pressed.
    std::thread::sleep(Duration::from_millis(300));
    provider.pump();
    provider
        .interrupt_turn(InterruptTurnRequest { session_id: id.clone() })
        .expect("the interrupt is accepted");
    (provider, id)
}

fn position(events: &[AgentDomainEvent], what: impl Fn(&AgentDomainEvent) -> bool) -> Option<usize> {
    events.iter().position(what)
}

#[test]
fn an_interrupted_turn_waits_for_the_command_codex_kept_running() {
    let (t, u) = ("th-30", "turn-30");
    let (provider, thread) = interrupted_with(
        t,
        u,
        command("c1", "inProgress"),
        vec![
            json!({"sleep_ms": 800}),
            item_note("item/completed", t, u, command("c1", "completed")),
            expect_request(6, "turn/start"),
            turn_response(6, "turn-31"),
        ],
    );
    let pressed = Instant::now();
    let events = until_turn_completed(&provider);
    assert!(
        pressed.elapsed() >= Duration::from_millis(600),
        "the turn ended {:?} after the interrupt, while Codex still ran the command",
        pressed.elapsed()
    );
    let result = position(
        &events,
        |e| matches!(e, AgentDomainEvent::ToolCallCompleted { content, .. } if content == &json!("all three")),
    )
    .expect("the command's row got its result");
    let end = position(&events, |e| matches!(e, AgentDomainEvent::TurnCompleted { .. })).expect("the end");
    assert!(result < end, "the row's result came after the turn's end: {events:#?}");
    assert!(matches!(
        &events[end],
        AgentDomainEvent::TurnCompleted {
            outcome: TurnOutcome::Interrupted,
            ..
        }
    ));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. })),
        "a command that ended in time stopped the session"
    );
    assert_eq!(
        send(&provider, &thread, "next").expect("the session goes on"),
        "turn-31"
    );
}

#[test]
fn a_command_still_running_after_the_settle_time_stops_the_session() {
    let (t, u) = ("th-32", "turn-32");
    // Codex never reports the command ended; the stand-in then waits for EOF on its stdin.
    let (provider, thread) = interrupted_with(t, u, command("c1", "inProgress"), vec![]);
    let pid = provider.pid();
    let pressed = Instant::now();
    let events = pump_until(&provider, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. }))
    });
    let waited = pressed.elapsed();
    assert!(
        waited + Duration::from_millis(200) >= agent::providers::codex::provider::INTERRUPT_SETTLE,
        "the session was stopped after only {waited:?}"
    );
    assert!(!alive(pid), "the turn ended before the child was gone");
    let row = position(&events, |e| {
        matches!(e, AgentDomainEvent::ToolCallCompleted { is_error: true, content, .. }
            if content.as_str().is_some_and(|c| c.contains("stopped")))
    })
    .expect("the command's row says it was stopped");
    let end = position(&events, |e| {
        matches!(
            e,
            AgentDomainEvent::TurnCompleted {
                outcome: TurnOutcome::Interrupted,
                ..
            }
        )
    })
    .expect("the turn ends interrupted");
    assert!(row < end);
    let gone = position(&events, |e| matches!(e, AgentDomainEvent::SessionUnavailable { .. }));
    assert!(gone > Some(end), "the session ended before its turn did: {events:#?}");
    assert!(
        provider.stopped_resumable(),
        "`r` continues a session Eitri stopped whole"
    );
    let reason = events
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::SessionUnavailable { reason } => Some(reason.clone()),
            _ => None,
        })
        .expect("the session is over");
    assert_eq!(
        reason,
        "Codex kept running a command after the interrupt; its session was stopped so nothing more runs. \
         The conversation can be resumed."
    );
    assert!(
        send(&provider, &thread, "next").is_err(),
        "a stopped session took a turn"
    );
}

#[test]
fn a_call_eitri_cannot_read_counts_as_still_running() {
    let (t, u) = ("th-33", "turn-33");
    let (provider, _) = interrupted_with(t, u, json!({"type": "someFutureCall", "id": "z1"}), vec![]);
    let pid = provider.pid();
    let events = pump_until(&provider, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. }))
    });
    assert!(!alive(pid));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentDomainEvent::SessionUnavailable { reason } if reason.starts_with("Codex kept running something Eitri could not read")
    )));
}

#[test]
fn a_tool_server_call_left_running_is_not_a_reason_to_stop() {
    let (t, u) = ("th-34", "turn-34");
    let (provider, thread) = interrupted_with(
        t,
        u,
        json!({"type": "mcpToolCall", "id": "k1", "server": "s", "tool": "t", "status": "inProgress", "arguments": {}}),
        vec![expect_request(6, "turn/start"), turn_response(6, "turn-35")],
    );
    let pid = provider.pid();
    let events = until_turn_completed(&provider);
    assert!(alive(pid));
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. })));
    assert_eq!(
        send(&provider, &thread, "next").expect("the session goes on"),
        "turn-35"
    );
}

#[test]
fn an_interrupt_with_nothing_running_ends_the_turn_at_once() {
    // A command that ended before the key: nothing is waited on.
    let (t, u) = ("th-36", "turn-36");
    let (provider, thread) = open(
        t,
        vec![
            expect_request(4, "turn/start"),
            turn_response(4, u),
            item_note("item/started", t, u, command("c1", "inProgress")),
            item_note("item/completed", t, u, command("c1", "completed")),
            expect_request(5, "turn/interrupt"),
            turn_completed(t, u, "interrupted", Value::Null),
        ],
    );
    send(&provider, &thread, "go").expect("starts");
    pump_until(&provider, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::ToolCallCompleted { .. }))
    });
    provider
        .interrupt_turn(InterruptTurnRequest {
            session_id: thread.clone(),
        })
        .expect("accepted");
    let pressed = Instant::now();
    until_turn_completed(&provider);
    assert!(
        pressed.elapsed() < Duration::from_secs(1),
        "took {:?}",
        pressed.elapsed()
    );
}

#[test]
fn a_search_left_running_is_not_waited_on() {
    let (t, u) = ("th-37", "turn-37");
    let (provider, _) = interrupted_with(t, u, json!({"type": "webSearch", "id": "w1", "query": "x"}), vec![]);
    let pressed = Instant::now();
    let events = until_turn_completed(&provider);
    assert!(
        pressed.elapsed() < Duration::from_secs(1),
        "took {:?}",
        pressed.elapsed()
    );
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. })));
}

#[test]
fn a_family_turn_named_while_the_turn_settles_is_waited_for() {
    let (t, u) = ("th-38", "turn-38");
    let (provider, _) = interrupted_with(
        t,
        u,
        command("c1", "inProgress"),
        vec![
            // A turn of the tab's thread whose start cannot be read, then a message that names it, which
            // Eitri sends its interrupt.
            write(json!({"method": "turn/started", "params": {"threadId": t, "turn": {"bogus": true}}})),
            item_note(
                "item/started",
                t,
                "turn-38b",
                json!({"type": "agentMessage", "id": "m9", "text": ""}),
            ),
            expect_exact(&format!(
                r#"{{"id":6,"method":"turn/interrupt","params":{{"threadId":"{t}","turnId":"turn-38b"}}}}"#
            )),
            item_note("item/completed", t, u, command("c1", "completed")),
            json!({"sleep_ms": 700}),
            turn_completed(t, "turn-38b", "interrupted", Value::Null),
        ],
    );
    let pressed = Instant::now();
    let events = until_turn_completed(&provider);
    assert!(
        pressed.elapsed() >= Duration::from_millis(600),
        "the tab's turn ended {:?} after the key, while a family turn still ran: {events:#?}",
        pressed.elapsed()
    );
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. })));
}

#[test]
fn closing_during_a_stop_waits_until_the_child_is_gone() {
    let (t, u) = ("th-39", "turn-39");
    // The stand-in sleeps through its stdin's end, so only the process-group kill after the grace ends it.
    let (provider, thread) = interrupted_with(t, u, command("c1", "inProgress"), vec![json!({"sleep_ms": 30000})]);
    let pid = provider.pid();
    let pressed = Instant::now();
    while pressed.elapsed() < agent::providers::codex::provider::INTERRUPT_SETTLE + Duration::from_millis(300) {
        provider.pump();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(alive(pid), "the stand-in should still be inside its grace period");
    provider
        .close_session(CloseSessionRequest { session_id: thread })
        .expect("closes");
    assert!(
        !alive(pid),
        "the close returned while the stop had not killed the child"
    );
}

/// The binary the self-check passed, as a key naming `version`.
fn checked_as(version: &str) -> BinaryKey {
    BinaryKey::new(launch().program, 0, 0, version.to_string())
}

fn connect_checked(script: Vec<Value>, key: &BinaryKey) -> Result<CodexProvider, ProviderError> {
    CodexProvider::connect_checked_with_args(launch(), vec![OsString::from(Value::Array(script).to_string())], key)
}

#[test]
fn a_codex_that_reports_another_version_than_the_checked_one_is_refused() {
    // The path is resolved when the check runs and again when the tab's child starts; an upgrade in
    // between leaves a binary running that was never checked.
    let script = vec![
        expect_request(1, "initialize"),
        write(json!({"id": 1, "result": {"userAgent": UA, "codexHome": "/x"}})),
    ];
    match connect_checked(script, &checked_as("0.161.0")) {
        Err(ProviderError::Provider { code, message }) => {
            assert_eq!(code, ProviderErrorCode::ProviderUnavailable);
            assert!(
                message.contains("Codex changed while Eitri was checking it"),
                "{message}"
            );
        }
        Err(other) => panic!("expected a refusal, got {other}"),
        Ok(_) => panic!("a Codex other than the checked one connected"),
    }
}

#[test]
fn a_codex_that_reports_the_checked_version_connects() {
    let provider = connect_checked(handshake(UA, signed_in()), &checked_as("0.160.0")).expect("connects");
    assert_eq!(provider.version(), (0, 160, 0));
}

// ---------------------------------------------------------------- what a completed turn leaves running

fn background_counts(events: &[AgentDomainEvent]) -> Vec<u32> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::BackgroundCalls { running } => Some(*running),
            _ => None,
        })
        .collect()
}

/// A turn that completes while the command it started still runs (the model put it in the background).
fn left_running(thread: &str, turn: &str, then: Vec<Value>) -> (Arc<CodexProvider>, String, Vec<AgentDomainEvent>) {
    let mut rest = vec![
        expect_request(4, "turn/start"),
        turn_response(4, turn),
        item_note("item/started", thread, turn, command("bg1", "inProgress")),
        turn_completed(thread, turn, "completed", Value::Null),
    ];
    rest.extend(then);
    let (provider, id) = open(thread, rest);
    send(&provider, &id, "go").expect("starts");
    let events = until_turn_completed(&provider);
    (provider, id, events)
}

#[test]
fn a_background_command_gets_its_result_after_its_turn_ends() {
    background_result(false);
}

#[test]
fn a_background_command_gets_its_result_under_its_original_turn_while_another_runs() {
    background_result(true);
}

fn background_result(start_another_turn: bool) {
    let (thread, turn) = ("th-background-result", "turn-background-result");
    let mut completed = command("bg1", "completed");
    completed["aggregatedOutput"] = json!("done");
    let mut then = Vec::new();
    if start_another_turn {
        then.extend([expect_request(5, "turn/start"), turn_response(5, "turn-next")]);
    }
    then.push(item_note("item/completed", thread, turn, completed));
    let (provider, id, events) = left_running(thread, turn, then);
    if start_another_turn {
        send(&provider, &id, "again").expect("the next turn starts");
    }
    let events = pump_on(&provider, events, |events| background_counts(events).contains(&0));
    let end = position(
        &events,
        |e| matches!(e, AgentDomainEvent::TurnCompleted { turn_id, .. } if turn_id == turn),
    )
    .expect("the first turn ended");
    let results: Vec<_> = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            AgentDomainEvent::ToolCallCompleted {
                turn_id,
                tool_use_id,
                content,
                is_error,
            } if tool_use_id == "bg1" => Some((index, turn_id.as_str(), content, *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1, "the background row gets one result: {events:#?}");
    let (index, turn_id, content, is_error) = results[0];
    assert!(index > end, "the result follows its turn's end");
    assert_eq!(turn_id, turn);
    assert_eq!(content, &json!("done"));
    assert!(!is_error);
    if start_another_turn {
        assert!(events.iter().any(|e| matches!(
            e,
            AgentDomainEvent::TurnStarted { turn_id } if turn_id == "turn-next"
        )));
        assert!(!events.iter().any(|e| matches!(
            e,
            AgentDomainEvent::TurnCompleted { turn_id, .. } if turn_id == "turn-next"
        )));
    }
}

#[test]
fn a_background_command_whose_result_is_too_large_ends_its_original_row() {
    let (thread, turn) = ("th-background-large", "turn-background-large");
    let prefix = format!(
        r#"{{"method":"item/completed","params":{{"threadId":"{thread}","turnId":"{turn}","item":{{"type":"commandExecution","id":"bg1","command":"yes","cwd":"/work/proj","status":"completed","aggregatedOutput":""#
    );
    let (provider, _, events) = left_running(
        thread,
        turn,
        vec![json!({"write_big": {"prefix": prefix, "bytes": 17 * 1024 * 1024, "suffix": "\"}}}"}})],
    );
    let events = pump_on(&provider, events, |events| background_counts(events).contains(&0));
    let end = position(
        &events,
        |e| matches!(e, AgentDomainEvent::TurnCompleted { turn_id, .. } if turn_id == turn),
    )
    .expect("the turn ended");
    let results: Vec<_> = events[end + 1..]
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::ToolCallCompleted {
                turn_id,
                tool_use_id,
                content,
                is_error: false,
            } if tool_use_id == "bg1" => Some((turn_id.as_str(), content.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        results,
        vec![(turn, Some("ended; its result was larger than 16 MiB and is not shown"))]
    );
}

#[test]
fn a_command_left_running_by_a_completed_turn_is_counted_before_the_turn_ends() {
    let (t, u) = ("th-60", "turn-60");
    let (provider, _, events) = left_running(
        t,
        u,
        vec![
            json!({"sleep_ms": 300}),
            item_note("item/completed", t, u, command("bg1", "completed")),
        ],
    );
    let count = position(&events, |e| {
        matches!(e, AgentDomainEvent::BackgroundCalls { running: 1 })
    })
    .expect("the command left running is counted");
    let end = position(&events, |e| matches!(e, AgentDomainEvent::TurnCompleted { .. })).expect("the end");
    assert!(count < end, "the count came after the turn's end: {events:#?}");
    assert!(matches!(
        &events[end],
        AgentDomainEvent::TurnCompleted {
            outcome: TurnOutcome::Completed,
            ..
        }
    ));
    // Its end, after the turn's, takes the count back to nothing.
    let all = pump_on(&provider, events, |events| background_counts(events).contains(&0));
    assert_eq!(background_counts(&all[end + 1..]), vec![0]);
}

#[test]
fn a_command_that_ends_inside_its_turn_is_never_counted() {
    let (t, u) = ("th-61", "turn-61");
    let (provider, id) = open(
        t,
        vec![
            expect_request(4, "turn/start"),
            turn_response(4, u),
            item_note("item/started", t, u, command("c1", "inProgress")),
            item_note("item/completed", t, u, command("c1", "completed")),
            turn_completed(t, u, "completed", Value::Null),
        ],
    );
    send(&provider, &id, "go").expect("starts");
    let mut events = until_turn_completed(&provider);
    std::thread::sleep(Duration::from_millis(100));
    events.extend(provider.pump());
    assert!(background_counts(&events).is_empty(), "{events:#?}");
}

#[test]
fn stopping_the_background_stops_the_session() {
    let (t, u) = ("th-62", "turn-62");
    // The stand-in never reports the command ended and waits for its stdin to close.
    let (provider, id, _) = left_running(t, u, vec![]);
    let pid = provider.pid();
    assert!(!provider.stopped_resumable(), "a live session is no stopped one");
    provider.stop_background(&id, 1, &[]).expect("the stop is taken");
    let events = pump_until(&provider, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. }))
    });
    assert!(!alive(pid), "the session ended before its child was gone");
    assert_eq!(background_counts(&events), vec![0]);
    // The command's row, drawn by the turn that left it running, ends with the session.
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentDomainEvent::ToolCallCompleted { turn_id, tool_use_id, content, is_error: true }
                if turn_id == u && tool_use_id == "bg1" && content == "stopped with the session"
        )),
        "{events:#?}"
    );
    assert!(events.iter().any(|e| matches!(
        e,
        AgentDomainEvent::SessionUnavailable { reason }
            if reason == agent::providers::codex::provider::STOPPED_BACKGROUND
    )));
    assert!(send(&provider, &id, "next").is_err(), "a stopped session took a turn");
    assert!(
        provider.stopped_resumable(),
        "`r` continues a session Eitri stopped whole"
    );
}

#[test]
fn the_background_is_not_stopped_while_a_turn_runs_or_when_nothing_is_left() {
    let (t, u) = ("th-63", "turn-63");
    let (provider, id) = open(
        t,
        vec![
            expect_request(4, "turn/start"),
            turn_response(4, u),
            item_note("item/started", t, u, command("c1", "inProgress")),
        ],
    );
    let refused = |provider: &CodexProvider| match provider.stop_background(&id, 1, &[]) {
        Err(ProviderError::Provider { code, .. }) => code,
        other => panic!("expected a refusal, got {other:?}"),
    };
    assert_eq!(refused(&provider), ProviderErrorCode::NoActiveTurn, "nothing left yet");
    send(&provider, &id, "go").expect("starts");
    pump_until(&provider, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::ToolCallStarted { .. }))
    });
    assert_eq!(refused(&provider), ProviderErrorCode::TurnAlreadyActive);
    assert!(alive(provider.pid()));
}

/// A turn start Codex did not answer in time is reported as not started; should Codex start that turn
/// after all, it is interrupted rather than left running unseen by the user.
#[test]
fn a_turn_codex_starts_after_its_start_timed_out_is_interrupted() {
    let (t, u) = ("th-64", "turn-64");
    let (provider, id) = open(
        t,
        vec![
            expect_request(4, "turn/start"),
            json!({"sleep_ms": agent::UNARY_RPC_TIMEOUT.as_millis() as u64 + 500}),
            write(json!({"method": "turn/started", "params": {"threadId": t, "turn": {
                "id": u, "items": [], "itemsView": "notLoaded", "status": "inProgress", "error": null}}})),
            expect_exact(&format!(
                r#"{{"id":5,"method":"turn/interrupt","params":{{"threadId":"{t}","turnId":"{u}"}}}}"#
            )),
            turn_completed(t, u, "interrupted", Value::Null),
        ],
    );
    assert!(matches!(send(&provider, &id, "go"), Err(ProviderError::Timeout)));
    let events = until_turn_completed(&provider);
    assert!(matches!(
        the_completion(&events),
        AgentDomainEvent::TurnCompleted {
            outcome: TurnOutcome::Interrupted,
            ..
        }
    ));
}

/// A call whose end is larger than the link reads is over all the same: its row ends with a note instead of
/// reading as running until the turn ends.
#[test]
fn a_row_whose_end_is_too_large_to_read_ends_with_a_note() {
    let (t, u) = ("th-65", "turn-65");
    let prefix = format!(
        r#"{{"method":"item/completed","params":{{"threadId":"{t}","turnId":"{u}","item":{{"type":"commandExecution","id":"big1","command":"yes","cwd":"/work/proj","status":"completed","aggregatedOutput":""#
    );
    let (provider, id) = open(
        t,
        vec![
            expect_request(4, "turn/start"),
            turn_response(4, u),
            item_note("item/started", t, u, command("big1", "inProgress")),
            json!({"write_big": {"prefix": prefix, "bytes": 17 * 1024 * 1024, "suffix": "\"}}}"}}),
            json!({"sleep_ms": 300}),
            turn_completed(t, u, "completed", Value::Null),
        ],
    );
    send(&provider, &id, "go").expect("starts");
    let events = pump_until(&provider, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::ToolCallCompleted { .. }))
    });
    let ended = events.iter().find_map(|e| match e {
        AgentDomainEvent::ToolCallCompleted { content, .. } => content.as_str().map(str::to_string),
        _ => None,
    });
    assert!(
        ended.as_deref().is_some_and(|c| c.contains("larger than 16 MiB")),
        "{ended:?}"
    );
    let rest = pump_on(&provider, events, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::TurnCompleted { .. }))
    });
    assert!(
        !background_counts(&rest).iter().any(|n| *n > 0),
        "the call that ended counts as left running"
    );
}

/// A final message that is not the streamed text with more after it is shown whole as a message of its own,
/// never as its tail spliced onto what streamed.
#[test]
fn a_final_message_that_does_not_extend_what_streamed_is_not_spliced() {
    let (t, u) = ("th-66", "turn-66");
    let (provider, id) = open(
        t,
        vec![
            expect_request(4, "turn/start"),
            turn_response(4, u),
            item_note(
                "item/started",
                t,
                u,
                json!({"type": "agentMessage", "id": "m1", "text": ""}),
            ),
            delta_note("item/agentMessage/delta", t, u, "m1", "Hello wor"),
            item_note(
                "item/completed",
                t,
                u,
                json!({"type": "agentMessage", "id": "m1", "text": "Hi there, it is a new text"}),
            ),
            turn_completed(t, u, "completed", Value::Null),
        ],
    );
    send(&provider, &id, "go").expect("starts");
    let events = until_turn_completed(&provider);
    let texts: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::ContentDelta {
                kind: ContentKind::Text,
                text,
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["Hello wor", "Hi there, it is a new text"], "{events:#?}");
}

/// The user said yes to a number; when Codex's own count is another by now (its report not yet folded),
/// nothing is stopped and the panel asks again.
#[test]
fn a_stop_for_a_count_that_moved_is_refused() {
    let (t, u) = ("th-67", "turn-67");
    let (provider, id, _) = left_running(t, u, vec![]);
    match provider.stop_background(&id, 2, &[]) {
        Err(ProviderError::Provider { code, message }) => {
            assert_eq!(code, ProviderErrorCode::NoActiveTurn);
            assert!(message.contains("ask again"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(alive(provider.pid()), "nothing was stopped");
}

// ---------------------------------------------------------------- an interrupt Codex refuses

/// Codex refuses `turn/interrupt` with "no active turn to interrupt" when it comes the moment `turn/start` was
/// answered (measured on 0.161.0: three times in seven), and the turn then runs on. A refused interrupt of a
/// turn that still runs is sent again.
#[test]
fn a_refused_interrupt_of_a_turn_that_runs_is_sent_again() {
    let (t, u) = ("th-f4a", "turn-f4a");
    let interrupt =
        |id: u64| format!(r#"{{"id":{id},"method":"turn/interrupt","params":{{"threadId":"{t}","turnId":"{u}"}}}}"#);
    let (provider, thread) = open(
        t,
        vec![
            expect_request(4, "turn/start"),
            turn_response(4, u),
            expect_exact(&interrupt(5)),
            write(json!({"id": 5, "error": {"code": -32600, "message": "no active turn to interrupt"}})),
            expect_exact(&interrupt(6)),
            turn_completed(t, u, "interrupted", Value::Null),
        ],
    );
    send(&provider, &thread, "go").expect("starts");
    provider
        .interrupt_turn(InterruptTurnRequest {
            session_id: thread.clone(),
        })
        .expect("the interrupt is accepted");
    let events = until_turn_completed(&provider);
    assert!(matches!(
        the_completion(&events),
        AgentDomainEvent::TurnCompleted {
            outcome: TurnOutcome::Interrupted,
            ..
        }
    ));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. })),
        "a turn the second interrupt ended stopped the session"
    );
}

/// A turn that has not ended `INTERRUPT_SETTLE` after its interrupt was sent, whatever Codex answered, stops
/// the session the way a command left running does: the tab's turn ends interrupted and the session can be
/// resumed, so the tab is never left busy for good.
#[test]
fn a_turn_that_does_not_end_after_its_interrupt_stops_the_session() {
    let (t, u) = ("th-f4b", "turn-f4b");
    let (provider, thread) = open(
        t,
        vec![
            expect_request(4, "turn/start"),
            turn_response(4, u),
            expect_exact(&format!(
                r#"{{"id":5,"method":"turn/interrupt","params":{{"threadId":"{t}","turnId":"{u}"}}}}"#
            )),
            write(json!({"id": 5, "error": {"code": -32600, "message": "no active turn to interrupt"}})),
            // After its last step the stand-in reads its input to the end and answers nothing more.
        ],
    );
    send(&provider, &thread, "go").expect("starts");
    let pressed = Instant::now();
    provider
        .interrupt_turn(InterruptTurnRequest {
            session_id: thread.clone(),
        })
        .expect("the interrupt is accepted");
    let events = pump_until(&provider, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. }))
    });
    assert!(
        pressed.elapsed() >= agent::providers::codex::provider::INTERRUPT_SETTLE,
        "the session was stopped {:?} after the interrupt",
        pressed.elapsed()
    );
    let end = events
        .iter()
        .position(|e| {
            matches!(
                e,
                AgentDomainEvent::TurnCompleted {
                    outcome: TurnOutcome::Interrupted,
                    ..
                }
            )
        })
        .expect("the turn ends interrupted");
    let gone = events
        .iter()
        .position(
            |e| matches!(e, AgentDomainEvent::SessionUnavailable { reason } if reason.contains("did not end the turn")),
        )
        .expect("the session says why it stopped");
    assert!(end < gone, "{events:#?}");
    assert!(provider.stopped_resumable(), "the conversation cannot be resumed");
}

// ---------------------------------------------------------------- messages into the running turn

fn steer(provider: &CodexProvider, thread: &str, turn: &str, id: &str, text: &str) -> Result<(), ProviderError> {
    provider.steer_turn(agent::SteerTurnRequest {
        session_id: thread.to_string(),
        expected_turn_id: turn.to_string(),
        client_message_id: id.to_string(),
        text: format!("ctx\n\n{text}"),
        as_typed: text.to_string(),
    })
}

fn user_message(thread: &str, turn: &str, client_id: &str, text: &str) -> Vec<Value> {
    let item = json!({"type": "userMessage", "id": format!("um-{client_id}"), "clientId": client_id,
                      "content": [{"type": "text", "text": text, "text_elements": []}]});
    vec![
        item_note("item/started", thread, turn, item.clone()),
        item_note("item/completed", thread, turn, item),
    ]
}

fn index_of(events: &[AgentDomainEvent], what: impl Fn(&AgentDomainEvent) -> bool) -> usize {
    events
        .iter()
        .position(what)
        .unwrap_or_else(|| panic!("not found in {events:#?}"))
}

/// `turn/steer` carries the message, the client's id and the turn the host saw; the `userMessage` item that
/// carries the id back is the delivery, folded once, before the turn's end, with what the user typed.
#[test]
fn a_message_put_into_the_turn_is_delivered_when_codex_reads_it() {
    let steer_line = concat!(
        r#"{"id":5,"method":"turn/steer","params":{"clientUserMessageId":"m1","expectedTurnId":"turn-a","#,
        r#""input":[{"text":"ctx\n\nand the tests","type":"text"}],"threadId":"th-s1"}}"#,
    );
    let mut rest = vec![
        expect_request(4, "turn/start"),
        turn_response(4, "turn-a"),
        expect_exact(steer_line),
        write(json!({"id": 5, "result": {"turnId": "turn-a"}})),
    ];
    rest.extend(user_message("th-s1", "turn-a", "m1", "ctx\n\nand the tests"));
    rest.push(turn_completed("th-s1", "turn-a", "completed", Value::Null));
    let (provider, thread) = open("th-s1", rest);
    assert!(provider.capabilities().steer);
    let turn = send(&provider, &thread, "go").expect("the turn starts");
    steer(&provider, &thread, &turn, "m1", "and the tests").expect("sent");
    let events = until_turn_completed(&provider);
    let delivered: Vec<&AgentDomainEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentDomainEvent::UserMessageDelivered { .. }))
        .collect();
    assert_eq!(
        delivered,
        [&AgentDomainEvent::UserMessageDelivered {
            client_message_id: "m1".into(),
            turn_id: "turn-a".into(),
            text: "and the tests".into(),
        }],
        "once, with the typed text, not the wire text"
    );
    assert!(
        index_of(&events, |e| matches!(e, AgentDomainEvent::UserMessageDelivered { .. }))
            < index_of(&events, |e| matches!(e, AgentDomainEvent::TurnCompleted { .. }))
    );
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentDomainEvent::UserMessageDropped { .. })));
}

/// Codex's refusal drops the message with words of Eitri's; a Codex that does not know the method is not
/// asked again.
#[test]
fn a_refused_message_is_dropped_and_an_unknown_method_is_not_tried_again() {
    let rest = vec![
        expect_request(4, "turn/start"),
        turn_response(4, "turn-a"),
        expect_request(5, "turn/steer"),
        write(json!({"id": 5, "error": {"code": -32600,
            "message": "expected active turn id `turn-a` but found `turn-b`"}})),
        expect_request(6, "turn/steer"),
        write(json!({"id": 6, "error": {"code": -32601, "message": "method not found"}})),
        turn_completed("th-s2", "turn-a", "completed", Value::Null),
    ];
    let (provider, thread) = open("th-s2", rest);
    let turn = send(&provider, &thread, "go").expect("the turn starts");
    steer(&provider, &thread, &turn, "m1", "one").expect("sent");
    let events = pump_until(&provider, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::UserMessageDropped { .. }))
    });
    assert!(events.contains(&AgentDomainEvent::UserMessageDropped {
        client_message_id: "m1".into(),
        reason: "the turn had ended when it reached Codex".into(),
    }));
    steer(&provider, &thread, &turn, "m2", "two").expect("sent");
    let events = until_turn_completed(&provider);
    assert!(
        events.contains(&AgentDomainEvent::UserMessageDropped {
            client_message_id: "m2".into(),
            reason: "this Codex cannot take messages while a turn runs".into(),
        }),
        "the refusal read before the turn's end names its own reason: {events:#?}"
    );
    match steer(&provider, &thread, &turn, "m3", "three") {
        Err(ProviderError::UnsupportedCapability(_)) => {}
        other => panic!("expected the capability to be gone, got {other:?}"),
    }
    assert!(!provider.capabilities().steer, "and the tab is told");
}

/// Codex forgets a message it took but had not read when the turn ends (an interrupt): it is dropped before
/// the turn's end, so the host holds it again by the time it sees the turn over.
#[test]
fn a_message_codex_had_not_read_is_dropped_before_the_turn_ends() {
    let rest = vec![
        expect_request(4, "turn/start"),
        turn_response(4, "turn-a"),
        expect_request(5, "turn/steer"),
        write(json!({"id": 5, "result": {"turnId": "turn-a"}})),
        turn_completed("th-s3", "turn-a", "interrupted", Value::Null),
    ];
    let (provider, thread) = open("th-s3", rest);
    let turn = send(&provider, &thread, "go").expect("the turn starts");
    steer(&provider, &thread, &turn, "m1", "unread").expect("sent");
    let events = until_turn_completed(&provider);
    let dropped = index_of(&events, |e| {
        *e == AgentDomainEvent::UserMessageDropped {
            client_message_id: "m1".into(),
            reason: "the turn ended before Codex read it".into(),
        }
    });
    assert!(dropped < index_of(&events, |e| matches!(e, AgentDomainEvent::TurnCompleted { .. })));
}

/// Nothing is sent without a running turn, for a turn other than the running one, or after an interrupt
/// was asked for (Codex would forget it).
#[test]
fn a_message_for_no_turn_another_turn_or_an_ending_one_is_refused_unsent() {
    let rest = vec![
        expect_request(4, "turn/start"),
        turn_response(4, "turn-a"),
        expect_request(5, "turn/interrupt"),
        write(json!({"id": 5, "result": {}})),
        turn_completed("th-s4", "turn-a", "interrupted", Value::Null),
    ];
    let (provider, thread) = open("th-s4", rest);
    let no_turn = |r: Result<(), ProviderError>| match r {
        Err(ProviderError::Provider { code, .. }) => assert_eq!(code, ProviderErrorCode::NoActiveTurn),
        other => panic!("expected NoActiveTurn, got {other:?}"),
    };
    no_turn(steer(&provider, &thread, "turn-a", "m0", "idle"));
    let turn = send(&provider, &thread, "go").expect("the turn starts");
    no_turn(steer(&provider, &thread, "turn-x", "m1", "another turn"));
    provider
        .interrupt_turn(InterruptTurnRequest {
            session_id: thread.clone(),
        })
        .expect("interrupted");
    no_turn(steer(&provider, &thread, &turn, "m2", "after the key"));
    let events = until_turn_completed(&provider);
    assert!(!events.iter().any(|e| matches!(
        e,
        AgentDomainEvent::UserMessageDropped { .. } | AgentDomainEvent::UserMessageDelivered { .. }
    )));
}

/// The `turn/steer` request with this id for this client message id.
fn expect_steer(id: u64, client_id: &str) -> Value {
    json!({ "expect": format!(r#"^\{{"id":{id},"method":"turn/steer","params":\{{"clientUserMessageId":"{client_id}""#) })
}

/// A later message waits unsent until the earlier one's answer comes: refused, both are dropped in order
/// and the later one was never sent (it is dropped as waiting behind the refusal, not by the turn's end).
#[test]
fn a_message_waits_behind_an_unanswered_one_and_a_refusal_drops_both() {
    let rest = vec![
        expect_request(4, "turn/start"),
        turn_response(4, "turn-a"),
        expect_steer(5, "m1"),
        json!({"sleep_ms": 300}),
        write(json!({"id": 5, "error": {"code": -32600, "message": "input exceeds the maximum length"}})),
        turn_completed("th-s5", "turn-a", "completed", Value::Null),
    ];
    let (provider, thread) = open("th-s5", rest);
    let turn = send(&provider, &thread, "go").expect("the turn starts");
    steer(&provider, &thread, &turn, "m1", "too long").expect("sent");
    steer(&provider, &thread, &turn, "m2", "after it").expect("taken, to wait");
    let events = until_turn_completed(&provider);
    let dropped: Vec<&AgentDomainEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentDomainEvent::UserMessageDropped { .. }))
        .collect();
    assert_eq!(
        dropped,
        [
            &AgentDomainEvent::UserMessageDropped {
                client_message_id: "m1".into(),
                reason: "Codex did not take it: input exceeds the maximum length".into(),
            },
            &AgentDomainEvent::UserMessageDropped {
                client_message_id: "m2".into(),
                reason: "an earlier message was not taken, and this one waits behind it".into(),
            },
        ]
    );
}

/// Accepted, the waiting message goes out next, and both are read in the order they were typed.
#[test]
fn a_waiting_message_goes_out_once_the_earlier_one_is_taken() {
    let mut rest = vec![
        expect_request(4, "turn/start"),
        turn_response(4, "turn-a"),
        expect_steer(5, "m1"),
        json!({"sleep_ms": 200}),
        write(json!({"id": 5, "result": {"turnId": "turn-a"}})),
        expect_steer(6, "m2"),
        write(json!({"id": 6, "result": {"turnId": "turn-a"}})),
    ];
    rest.extend(user_message("th-s6", "turn-a", "m1", "ctx\n\nfirst"));
    rest.extend(user_message("th-s6", "turn-a", "m2", "ctx\n\nsecond"));
    rest.push(turn_completed("th-s6", "turn-a", "completed", Value::Null));
    let (provider, thread) = open("th-s6", rest);
    let turn = send(&provider, &thread, "go").expect("the turn starts");
    steer(&provider, &thread, &turn, "m1", "first").expect("sent");
    steer(&provider, &thread, &turn, "m2", "second").expect("taken, to wait");
    let events = until_turn_completed(&provider);
    let delivered: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::UserMessageDelivered { client_message_id, .. } => Some(client_message_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(delivered, ["m1", "m2"]);
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentDomainEvent::UserMessageDropped { .. })));
}
