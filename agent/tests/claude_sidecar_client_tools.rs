//! Tools the host declares and answers itself, through the sidecar provider and a conversation, against
//! a stand-in sidecar (`tests/support/sidecar_standin.rs --client-tools`). No real `claude` runs here.
//!
//! What is checked: what reaches the wire on a create and a resume, read from the stand-in's record of the
//! decoded request; that an answer leaves without its caller waiting, and that a refused one is no error
//! to the host; and what a read of the open calls gives.
#![cfg(target_os = "linux")]

use agent::setting_sources::ProjectTrust;
use agent::{
    AgentConversation, AgentDomainEvent, AgentProvider, ClaudeSidecarProvider, ClientCallRef, ClientToolAnswer,
    ClientToolDecl, ClientToolSet, CloseSessionRequest, CreateSessionRequest, ProviderError, ResumeSessionRequest,
    SendTurnRequest, StreamingPreference, UNARY_RPC_TIMEOUT,
};
use claude_runtime_protocol::v1::{ClientToolDeclaration, ClientTools, CreateSessionRequest as ProtoRequest};
use prost::Message as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long any one wait here may take before the test is called hung. Not a speed bound: every wait
/// ends as soon as what it waits for has happened.
const HANG: Duration = Duration::from_secs(30);

/// A private directory under the test target, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("client-tools-{}", uuid::Uuid::new_v4()));
        agent::private_fs::create_private_dir_all(&dir, &dir).unwrap();
        Self(dir)
    }

    fn creates(&self) -> PathBuf {
        self.0.join("creates.bin")
    }

    fn answers(&self) -> PathBuf {
        self.0.join("answers.txt")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_script(path: &Path, body: &str) {
    // Through a child process, so no write descriptor of this process is open for a forking test on
    // another thread to inherit: a script still open for writing fails to run with `ETXTBSY`.
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(r#"umask 077 && printf '%s' "$1" > "$2" && chmod 700 "$2""#)
        .arg("sh")
        .arg(body)
        .arg(path)
        .status()
        .expect("sh runs");
    assert!(status.success(), "writing the script failed: {status}");
}

/// A provider on the stand-in, recording into `scratch`, with `flags` on the stand-in's command line.
fn stand_in(scratch: &Scratch, flags: &str) -> ClaudeSidecarProvider {
    let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    let launcher = scratch.0.join("sidecar");
    let script = format!(
        "#!/bin/sh\nexec {} --record-create {} --record-answers {} {flags}\n",
        quote(Path::new(env!("CARGO_BIN_EXE_eitri-sidecar-standin"))),
        quote(&scratch.creates()),
        quote(&scratch.answers()),
    );
    write_script(&launcher, &script);
    ClaudeSidecarProvider::connect_to_program(&uuid::Uuid::new_v4().to_string(), &launcher)
        .expect("the stand-in sidecar starts and answers the handshake")
}

fn tools() -> ClientToolSet {
    ClientToolSet {
        server_name: "eitri".into(),
        timeout_ms: 900_000,
        tools: vec![
            ClientToolDecl {
                name: "open_file".into(),
                description: "Opens a file".into(),
                input_schema_json: r#"{"type":"object"}"#.into(),
                always_load: true,
            },
            ClientToolDecl {
                name: "read_buffer".into(),
                description: "Reads a buffer".into(),
                input_schema_json: r#"{"type":"object","required":["path"]}"#.into(),
                always_load: false,
            },
        ],
    }
}

/// The wire's form of [`tools`], field by field.
fn tools_on_the_wire() -> ClientTools {
    ClientTools {
        server_name: "eitri".into(),
        timeout_ms: 900_000,
        tools: vec![
            ClientToolDeclaration {
                name: "open_file".into(),
                description: "Opens a file".into(),
                input_schema_json: r#"{"type":"object"}"#.into(),
                always_load: true,
            },
            ClientToolDeclaration {
                name: "read_buffer".into(),
                description: "Reads a buffer".into(),
                input_schema_json: r#"{"type":"object","required":["path"]}"#.into(),
                always_load: false,
            },
        ],
    }
}

fn create(provider: &ClaudeSidecarProvider, set: Option<ClientToolSet>) -> String {
    provider
        .create_session(CreateSessionRequest {
            cwd: "/".into(),
            streaming: StreamingPreference::Partial,
            project: ProjectTrust::Untrusted,
            client_tools: set,
        })
        .expect("the stand-in opens a session")
}

fn resume(provider: &ClaudeSidecarProvider, provider_session_id: &str, set: Option<ClientToolSet>) -> String {
    provider
        .resume_session(ResumeSessionRequest {
            provider_session_id: provider_session_id.into(),
            cwd: "/".into(),
            streaming: StreamingPreference::Partial,
            project: ProjectTrust::Untrusted,
            client_tools: set,
        })
        .expect("the stand-in resumes a session")
}

/// The decoded `CreateSession` requests the stand-in has recorded, in order.
fn creates(scratch: &Scratch) -> Vec<ProtoRequest> {
    let bytes = std::fs::read(scratch.creates()).unwrap();
    let mut rest = bytes.as_slice();
    let mut requests = Vec::new();
    while !rest.is_empty() {
        let length = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
        rest = &rest[4..];
        requests.push(ProtoRequest::decode(&rest[..length]).unwrap());
        rest = &rest[length..];
    }
    requests
}

/// The lines of the stand-in's answer record, once one of them satisfies `done`.
fn answers_until(scratch: &Scratch, done: impl Fn(&str) -> bool) -> Vec<String> {
    let started = Instant::now();
    loop {
        let lines: Vec<String> = std::fs::read_to_string(scratch.answers())
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        if lines.iter().any(|line| done(line)) {
            return lines;
        }
        assert!(started.elapsed() < HANG, "only {lines:?} recorded");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Sends `script` as a turn and waits for the provider to hand over a `ClientToolCall` of `call_id`.
fn call(provider: &ClaudeSidecarProvider, session: &str, script: &str, call_id: &str) {
    provider
        .send_turn(SendTurnRequest {
            session_id: session.to_string(),
            text: script.to_string(),
        })
        .expect("the stand-in takes the turn");
    let started = Instant::now();
    loop {
        let seen = provider
            .pump()
            .iter()
            .any(|event| matches!(event, AgentDomainEvent::ClientToolCall { call_id: seen, .. } if seen == call_id));
        if seen {
            return;
        }
        assert!(started.elapsed() < HANG, "no call {call_id} arrived");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn answer(session: &str, call_id: &str, text: &str) -> ClientToolAnswer {
    ClientToolAnswer {
        session_id: session.to_string(),
        call_id: call_id.to_string(),
        text: vec![text.to_string()],
        is_error: false,
    }
}

/// The opened session's provider id, from the events the provider hands over.
fn opened(provider: &ClaudeSidecarProvider) -> String {
    let started = Instant::now();
    loop {
        for event in provider.pump() {
            if let AgentDomainEvent::SessionOpened {
                provider_session_id, ..
            } = event
            {
                return provider_session_id;
            }
        }
        assert!(started.elapsed() < HANG, "the session never opened");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn client_tools_go_out_on_create_and_on_resume() {
    let scratch = Scratch::new();
    let provider = stand_in(&scratch, "--client-tools");
    assert!(provider.capabilities().client_tools);
    create(&provider, Some(tools()));
    let provider_session_id = opened(&provider);
    resume(&provider, &provider_session_id, Some(tools()));
    let sent = creates(&scratch);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].client_tools, Some(tools_on_the_wire()), "a fresh session");
    assert_eq!(sent[1].client_tools, Some(tools_on_the_wire()), "a resumed session");
    assert_eq!(
        sent[1].resume_provider_session_id.as_deref(),
        Some(provider_session_id.as_str())
    );
}

#[test]
fn no_client_tools_field_when_the_handshake_lacks_it() {
    let scratch = Scratch::new();
    let provider = stand_in(&scratch, "");
    assert!(!provider.capabilities().client_tools);
    create(&provider, Some(tools()));
    let provider_session_id = opened(&provider);
    resume(&provider, &provider_session_id, Some(tools()));
    let sent = creates(&scratch);
    assert_eq!(sent.len(), 2);
    assert!(sent.iter().all(|request| request.client_tools.is_none()));
}

#[test]
fn an_empty_set_is_never_sent() {
    let scratch = Scratch::new();
    let provider = stand_in(&scratch, "--client-tools");
    let empty = ClientToolSet {
        tools: vec![],
        ..tools()
    };
    create(&provider, Some(empty.clone()));
    let provider_session_id = opened(&provider);
    resume(&provider, &provider_session_id, Some(empty));
    let sent = creates(&scratch);
    assert_eq!(sent.len(), 2);
    assert!(sent.iter().all(|request| request.client_tools.is_none()));
}

#[test]
fn an_answer_is_sent_once_and_a_second_is_a_quiet_refusal() {
    let scratch = Scratch::new();
    let provider = stand_in(&scratch, "--client-tools");
    let session = create(&provider, Some(tools()));
    call(&provider, &session, "client-call c1 open_file tool-1", "c1");
    provider
        .answer_client_tool_call(answer(&session, "c1", "first"))
        .unwrap();
    let lines = answers_until(&scratch, |line| line == "replied c1");
    assert_eq!(lines[0], "c1 taken false first");
    provider
        .answer_client_tool_call(answer(&session, "c1", "second"))
        .unwrap();
    let lines = answers_until(&scratch, |line| line.ends_with("second"));
    assert_eq!(
        lines.iter().filter(|line| line.contains(" taken ")).count(),
        1,
        "the first answer wins: {lines:?}"
    );
    assert_eq!(lines[2], "c1 FailedPrecondition false second");
}

#[test]
fn an_unknown_id_is_quiet_too() {
    let scratch = Scratch::new();
    let provider = stand_in(&scratch, "--client-tools");
    let session = create(&provider, Some(tools()));
    provider
        .answer_client_tool_call(answer(&session, "nobody", "x"))
        .unwrap();
    let lines = answers_until(&scratch, |line| line == "replied nobody");
    assert_eq!(lines[0], "nobody NotFound false x");
}

#[test]
fn a_closed_session_is_quiet_too() {
    let scratch = Scratch::new();
    let provider = stand_in(&scratch, "--client-tools");
    let session = create(&provider, Some(tools()));
    call(&provider, &session, "client-call c1 open_file tool-1", "c1");
    provider
        .close_session(CloseSessionRequest {
            session_id: session.clone(),
        })
        .unwrap();
    provider
        .answer_client_tool_call(answer(&session, "c1", "late"))
        .unwrap();
    let lines = answers_until(&scratch, |line| line == "replied c1");
    assert_eq!(lines[0], "c1 NotFound false late");
}

/// The stand-in holds the reply until told otherwise, and the telling comes from this thread, after the
/// call returns. A caller that waited for the reply would get it only at the end of the RPC's own bound.
#[test]
fn answering_never_blocks_the_caller() {
    let scratch = Scratch::new();
    let provider = stand_in(&scratch, "--client-tools");
    let session = create(&provider, Some(tools()));
    call(
        &provider,
        &session,
        "hold-answers; client-call c1 open_file tool-1",
        "c1",
    );
    let started = Instant::now();
    provider
        .answer_client_tool_call(answer(&session, "c1", "held"))
        .unwrap();
    assert!(started.elapsed() < UNARY_RPC_TIMEOUT, "the caller waited out the RPC");
    let lines = answers_until(&scratch, |line| line.starts_with("c1 taken"));
    assert!(!lines.contains(&"replied c1".to_string()), "the reply is still held");
    provider
        .send_turn(SendTurnRequest {
            session_id: session,
            text: "release-answers".to_string(),
        })
        .unwrap();
    answers_until(&scratch, |line| line == "replied c1");
}

#[test]
fn an_answer_over_max_message_bytes_goes_out_as_a_short_error() {
    let scratch = Scratch::new();
    let limit = 64 * 1024;
    let provider = stand_in(&scratch, &format!("--client-tools --max-message-bytes {limit}"));
    let session = create(&provider, Some(tools()));
    call(&provider, &session, "client-call c1 open_file tool-1", "c1");
    let long = "x".repeat(limit);
    provider.answer_client_tool_call(answer(&session, "c1", &long)).unwrap();
    let lines = answers_until(&scratch, |line| line == "replied c1");
    assert_eq!(lines[0], "c1 taken true The tool's answer was too long to send.");

    // Under the limit by its margin an answer goes out as it is: the line is the limit, not any long text.
    call(&provider, &session, "client-call c2 open_file tool-2", "c2");
    let fits = "y".repeat(limit - 8 * 1024);
    provider.answer_client_tool_call(answer(&session, "c2", &fits)).unwrap();
    let lines = answers_until(&scratch, |line| line == "replied c2");
    assert!(lines.iter().any(|line| *line == format!("c2 taken false {fits}")));
}

#[test]
fn a_call_state_read_answers_through_its_receiver() {
    let scratch = Scratch::new();
    let provider = stand_in(&scratch, "--client-tools");
    let session = create(&provider, Some(tools()));
    call(&provider, &session, "client-call c1 open_file tool-1", "c1");
    let reply = provider.client_tool_call_state(&session).unwrap();
    let state = reply.recv_timeout(HANG).unwrap().unwrap();
    assert_eq!(state.open.len(), 1);
    assert!(state.through_sequence >= 1);
    provider
        .answer_client_tool_call(answer(&session, "c1", "done"))
        .unwrap();
    answers_until(&scratch, |line| line == "replied c1");
    let reply = provider.client_tool_call_state(&session).unwrap();
    let later = reply.recv_timeout(HANG).unwrap().unwrap();
    assert!(later.open.is_empty(), "an answered call is settled");
    assert!(
        later.through_sequence > state.through_sequence,
        "the settle is on the stream"
    );
}

#[test]
fn the_state_gives_ids_with_no_arguments() {
    let scratch = Scratch::new();
    let provider = stand_in(&scratch, "--client-tools");
    let session = create(&provider, Some(tools()));
    call(
        &provider,
        &session,
        "client-call c1 open_file tool-1; client-call c2 read_buffer",
        "c2",
    );
    let reply = provider.client_tool_call_state(&session).unwrap();
    let state = reply.recv_timeout(HANG).unwrap().unwrap();
    assert_eq!(
        state.open,
        [
            ClientCallRef {
                call_id: "c1".into(),
                tool_use_id: Some("tool-1".into()),
                tool: "open_file".into(),
            },
            ClientCallRef {
                call_id: "c2".into(),
                tool_use_id: None,
                tool: "read_buffer".into(),
            },
        ]
    );
}

#[test]
fn a_sidecar_without_the_capability_is_refused_before_anything_is_sent() {
    let scratch = Scratch::new();
    let provider = stand_in(&scratch, "");
    let session = create(&provider, None);
    let refused = provider
        .answer_client_tool_call(answer(&session, "c1", "x"))
        .unwrap_err();
    assert!(matches!(refused, ProviderError::UnsupportedCapability("client_tools")));
    let refused = provider.client_tool_call_state(&session).map(|_| ()).unwrap_err();
    assert!(matches!(refused, ProviderError::UnsupportedCapability("client_tools")));
    assert!(!scratch.answers().exists() || std::fs::read(scratch.answers()).unwrap().is_empty());
}

#[test]
fn a_conversation_states_its_tools_and_answers_by_its_session() {
    let scratch = Scratch::new();
    let provider = Arc::new(stand_in(&scratch, "--client-tools"));
    let cwd = agent::state_dirs::test_workspace_dir("client-tools");
    let mut conversation =
        AgentConversation::create_with_tools(provider.clone(), &cwd, ProjectTrust::Untrusted, Some(tools()))
            .expect("a conversation opens");
    let names: Vec<&str> = conversation
        .declared_client_tools()
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(names, ["open_file", "read_buffer"]);
    conversation
        .send_turn("client-call c1 open_file tool-1")
        .expect("the stand-in takes the turn");
    let started = Instant::now();
    while !conversation.projection().open_client_calls.contains_key("c1") {
        assert!(started.elapsed() < HANG, "the call never folded");
        std::thread::sleep(Duration::from_millis(5));
    }
    conversation
        .answer_client_tool_call("c1", vec!["done".to_string()], false)
        .unwrap();
    assert_eq!(
        answers_until(&scratch, |line| line == "replied c1")[0],
        "c1 taken false done"
    );
    let state = conversation.client_tool_call_state().unwrap();
    assert!(state.recv_timeout(HANG).unwrap().unwrap().open.is_empty());
    conversation.shutdown();
}

#[test]
fn a_conversation_on_a_sidecar_without_the_capability_has_no_tools() {
    let scratch = Scratch::new();
    let provider = Arc::new(stand_in(&scratch, ""));
    let cwd = agent::state_dirs::test_workspace_dir("client-tools-unoffered");
    let mut conversation = AgentConversation::create_with_tools(provider, &cwd, ProjectTrust::Untrusted, Some(tools()))
        .expect("a conversation opens");
    assert!(conversation.declared_client_tools().is_empty());
    conversation.shutdown();
}
