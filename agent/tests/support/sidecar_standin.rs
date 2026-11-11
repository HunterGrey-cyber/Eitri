//! A stand-in for Verdandi's Claude sidecar, for the tests of handing a sidecar from one process to
//! another. It speaks the sidecar's gRPC service on the socket `VERDANDI_CLAUDE_SIDECAR_SOCKET` names,
//! with what those tests rest on and nothing more:
//! - every session keeps a ring of all its events, numbered from 1, and a watch replays the ring
//!   after the sequence it asks for, then follows live; any number of watchers per session;
//! - a permission is answered from any connection;
//! - the process ends on its stdin's end of file, and through nothing else (unless a turn told it to
//!   ignore that end, so a holder has to kill it).
//!
//! A turn's text is a script of steps separated by `;`:
//! - `text <message id> <words>`: a text delta of that message;
//! - `sleep <ms>`;
//! - `gate <directory>`: publish `ready`, wait for `release`, then remove the directory;
//! - `ask <permission id>`: a permission request; the rest of the script runs once it is answered;
//! - `ask-unknown <permission id>`: the same, with a `PermissionOrigin` no client knows;
//! - `unknown-event`: a session event whose only arm is a field no client knows;
//! - `drop-watches`: every open watch stream of the session ends (the clients reconnect);
//! - `ignore-eof`: stdin's end of file no longer ends the process;
//! - `end`: the turn completes.
//!
//! With `--cli-turns` the stand-in offers protocol 3.22's `cli_turns` and `human_origin`, and a script
//! can play what Claude Code does by itself (`<kind>` is `task`, `scheduled`, `peer` or `unknown`):
//! - `cli-start <kind>`: a turn the CLI started, for that cause; the steps after it are that turn's;
//! - `cause <kind>`: the current turn's cause, refined at its close;
//! - `cycle <kind>`: a cycle of the CLI's own inside the current turn, ahead of its prompt;
//! - `deliver`, `drop`: the CLI read the latest `SendTurn`'s message (named by its client id), or
//!   dropped it;
//! - `end-no-result`: the current turn ends without a result.
//!
//! `--record-sends <file>` appends a line per `SendTurn` and `SteerTurn` request, with the text after it when it
//! holds an `@`:
//! `<rpc> <client id, or -> <typed_by_person>`.
//!
//! A steered message's text is the number of milliseconds after which it is delivered (500 if it is
//! not a number), or `gate <directory>` to hold delivery behind a file gate.
//!
//! With `--client-tools` the stand-in offers protocol 3.23's `client_tools`, and answers its two RPCs as
//! the sidecar does: the first answer to a call wins and settles it, a later one is refused with
//! `CLIENT_TOOL_CALL_ALREADY_ANSWERED`, an unknown id with `CLIENT_TOOL_CALL_NOT_FOUND`, a closed session
//! with `SESSION_NOT_FOUND`, and a request of `--max-message-bytes <n>` or more with `RESOURCE_EXHAUSTED`,
//! leaving the call open. A script can drive it:
//! - `client-call <call id> <tool> [<tool use id>]`: the model called a tool, with fixed arguments;
//! - `hold-answers`: every later `AnswerClientToolCall` gets no reply until `release-answers`.
//!
//! `--record-answers <file>` appends a line per `AnswerClientToolCall`, as it arrives and before any
//! hold: `<call id> <taken, or the refusal> <is_error> <its text, blocks joined by |>`, and
//! `replied <call id>` when its reply goes out.
//!
//! A sidecar this client did not ship with is scripted through the environment, so a launcher can give
//! one stand-in its own script while the tests that start it share theirs:
//! - `EITRI_STANDIN_CAPABILITIES`: the handshake's whole list, separated by spaces;
//! - `EITRI_STANDIN_MINOR`, `EITRI_STANDIN_MAJOR`: the handshake's protocol numbers;
//! - `EITRI_STANDIN_UNKNOWN_WIRE`: the handshake and every session event carry a field no client knows;
//! - `EITRI_STANDIN_INCOMPATIBLE`: `Handshake` fails with `INCOMPATIBLE_PROTOCOL` and this text, as a
//!   sidecar of another major does;
//! - `EITRI_STANDIN_STALL_HANDSHAKE`: `Handshake` never answers, and nothing is printed to stderr;
//! - `EITRI_STANDIN_REFUSE_BIND`: this text goes to stderr and the process exits 1 before it binds, as
//!   a sidecar refusing a CLI does;
//! - `EITRI_STANDIN_NEVER_BIND`: the process stays up, silent, and never binds.

use claude_runtime_protocol::v1::client_tool_content::Kind as ContentKind;
use claude_runtime_protocol::v1::session_event::Event;
use claude_runtime_protocol::v1::{
    AnswerClientToolCallRequest, AnswerClientToolCallResponse, CliCycleStarted, ClientToolCall, ClientToolCallOutcome,
    ClientToolCallSettled, CloseSessionRequest, CloseSessionResponse, CreateSessionRequest, CreateSessionResponse,
    ErrorCode, ErrorDetail, GetClientToolCallStateRequest, GetClientToolCallStateResponse, HandshakeRequest,
    HandshakeResponse, InterruptTurnRequest, InterruptTurnResponse, OpenClientToolCall, PermissionOutcome,
    PermissionRequested, PermissionResolved, ResolvePermissionRequest, ResolvePermissionResponse, SendTurnRequest,
    SendTurnResponse, SessionCloseReason, SessionClosed, SessionEvent, SessionReady, SteerTurnRequest,
    SteerTurnResponse, TextDelta, TurnCause, TurnCauseKind, TurnCauseUpdated, TurnCompleted, TurnOrigin, TurnOutcome,
    TurnStarted, UserMessageDelivered, UserMessageDropReason, UserMessageDropped, WatchSessionEventsRequest,
};
use prost::bytes::{Buf, BufMut};
use prost::encoding::{DecodeContext, WireType};
use prost::Message as _;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tonic::body::Body;
use tonic::codegen::tokio_stream::wrappers::{UnboundedReceiverStream, UnixListenerStream};
use tonic::codegen::{http, BoxFuture, Context, Poll, Service};
use tonic::server::{Grpc, NamedService};
use tonic::Status;
use tonic_prost::ProstCodec;

const SERVICE: &str = "verdandi.claude.runtime.v1.RuntimeService";

/// The largest request a real sidecar's handshake states.
const MAX_MESSAGE_BYTES: u64 = 4 * 1024 * 1024;

/// A field number no `HandshakeResponse` or `SessionEvent` has.
const UNKNOWN_FIELD: u32 = 998;

/// A `SessionEvent` arm no client knows.
const UNKNOWN_EVENT_ARM: u32 = 999;

/// A `PermissionOrigin` no client knows.
const UNKNOWN_ORIGIN: i32 = 99;

type Watcher = tokio::sync::mpsc::UnboundedSender<Result<OnTheWire<SessionEvent>, Status>>;

/// `message` as it goes out, followed by fields its type has no name for.
#[derive(Debug, Default)]
struct OnTheWire<M> {
    message: M,
    unknown: Vec<u8>,
}

impl<M: prost::Message> prost::Message for OnTheWire<M> {
    fn encode_raw(&self, buf: &mut impl BufMut) {
        self.message.encode_raw(buf);
        buf.put_slice(&self.unknown);
    }

    fn merge_field(
        &mut self,
        _: u32,
        _: WireType,
        _: &mut impl Buf,
        _: DecodeContext,
    ) -> Result<(), prost::DecodeError> {
        unreachable!("only sent")
    }

    fn encoded_len(&self) -> usize {
        self.message.encoded_len() + self.unknown.len()
    }

    fn clear(&mut self) {
        self.message.clear();
        self.unknown.clear();
    }
}

/// Field `number` as a string, which a client that does not know the number skips.
fn unknown_field(number: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    prost::encoding::string::encode(number, &"from a later minor".to_string(), &mut bytes);
    bytes
}

/// What `EITRI_STANDIN_*` scripts: see the top of this file.
#[derive(Default)]
struct Scripted {
    capabilities: Option<Vec<String>>,
    major: Option<u32>,
    minor: Option<u32>,
    unknown_wire: bool,
    incompatible: Option<String>,
    stall_handshake: bool,
}

impl Scripted {
    fn from_environment() -> Self {
        let number = |name: &str| std::env::var(name).ok().map(|n| n.parse().expect("a number"));
        Scripted {
            capabilities: std::env::var("EITRI_STANDIN_CAPABILITIES")
                .ok()
                .map(|list| list.split_whitespace().map(str::to_string).collect()),
            major: number("EITRI_STANDIN_MAJOR"),
            minor: number("EITRI_STANDIN_MINOR"),
            unknown_wire: std::env::var_os("EITRI_STANDIN_UNKNOWN_WIRE").is_some(),
            incompatible: std::env::var("EITRI_STANDIN_INCOMPATIBLE").ok(),
            stall_handshake: std::env::var_os("EITRI_STANDIN_STALL_HANDSHAKE").is_some(),
        }
    }
}

#[derive(Default)]
struct Session {
    closed: bool,
    ring: Vec<SessionEvent>,
    watchers: Vec<Watcher>,
    /// The rest of a script, by the permission it waits on, with its turn.
    waiting: HashMap<String, (String, Vec<String>)>,
    /// The client id of the latest `SendTurn`'s message, if it named one.
    last_client_id: Option<String>,
    /// The client tool calls waiting for an answer, oldest first.
    open_calls: Vec<OpenClientToolCall>,
    /// The client tool calls answered, each of them spent.
    answered_calls: HashSet<String>,
}

#[derive(Default)]
struct Shared {
    sessions: Mutex<HashMap<String, Session>>,
    next_id: AtomicU64,
    without_system_prompt_preset: bool,
    record_create: Mutex<Option<std::fs::File>>,
    cli_turns: bool,
    record_sends: Mutex<Option<std::fs::File>>,
    client_tools: bool,
    /// The handshake's limit, and what `AnswerClientToolCall` holds a request to. Unset, [`MAX_MESSAGE_BYTES`].
    max_message_bytes: Option<u64>,
    record_answers: Mutex<Option<std::fs::File>>,
    /// While true, `AnswerClientToolCall` replies wait.
    holding_answers: tokio::sync::watch::Sender<bool>,
    scripted: Scripted,
}

static IGNORE_EOF: AtomicBool = AtomicBool::new(false);

impl Shared {
    fn id(&self, prefix: &str) -> String {
        format!("{prefix}-{}", self.next_id.fetch_add(1, Ordering::SeqCst) + 1)
    }

    fn emit(&self, session_id: &str, turn_id: Option<&str>, event: Event) {
        self.emit_any(session_id, turn_id, Some(event));
    }

    /// A ring holds what this binary can name, so an event with no arm stands for the unknown one
    /// [`Self::on_the_wire`] gives it.
    fn emit_any(&self, session_id: &str, turn_id: Option<&str>, event: Option<Event>) {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(session) = sessions.get_mut(session_id) else {
            return;
        };
        let occurred_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let event = SessionEvent {
            session_id: session_id.to_string(),
            sequence: session.ring.len() as u64 + 1,
            occurred_at,
            turn_id: turn_id.map(str::to_string),
            event,
        };
        session.ring.push(event.clone());
        session
            .watchers
            .retain(|watcher| watcher.send(Ok(self.on_the_wire(event.clone()))).is_ok());
    }

    fn on_the_wire(&self, event: SessionEvent) -> OnTheWire<SessionEvent> {
        let mut unknown = Vec::new();
        if self.scripted.unknown_wire {
            unknown.extend(unknown_field(UNKNOWN_FIELD));
        }
        if event.event.is_none() {
            unknown.extend(unknown_field(UNKNOWN_EVENT_ARM));
        }
        OnTheWire {
            message: event,
            unknown,
        }
    }

    fn client_id(&self, session_id: &str) -> String {
        self.sessions
            .lock()
            .unwrap()
            .get(session_id)
            .and_then(|session| session.last_client_id.clone())
            .unwrap_or_default()
    }

    fn record_answer(&self, line: &str) {
        if let Some(file) = self.record_answers.lock().unwrap().as_mut() {
            writeln!(file, "{line}").unwrap();
            file.flush().unwrap();
        }
    }

    fn max_message_bytes(&self) -> u64 {
        self.max_message_bytes.unwrap_or(MAX_MESSAGE_BYTES)
    }

    /// One `AnswerClientToolCall`, decided at once: `Ok` is the answer that won its call.
    fn take_answer(&self, request: &AnswerClientToolCallRequest) -> Result<(), Status> {
        if request.encoded_len() as u64 >= self.max_message_bytes() {
            return Err(Status::resource_exhausted("the request passes max_message_bytes"));
        }
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions
            .get_mut(&request.session_id)
            .filter(|session| !session.closed)
            .ok_or_else(|| refusal(tonic::Code::NotFound, ErrorCode::SessionNotFound, ""))?;
        match session
            .open_calls
            .iter()
            .position(|call| call.call_id == request.call_id)
        {
            Some(index) => {
                session.open_calls.remove(index);
                session.answered_calls.insert(request.call_id.clone());
                Ok(())
            }
            None if session.answered_calls.contains(&request.call_id) => Err(refusal(
                tonic::Code::FailedPrecondition,
                ErrorCode::ClientToolCallAlreadyAnswered,
                "answered",
            )),
            None => Err(refusal(tonic::Code::NotFound, ErrorCode::ClientToolCallNotFound, "")),
        }
    }

    fn record_send(&self, rpc: &str, client_id: Option<&str>, typed: bool, text: &str) {
        if let Some(file) = self.record_sends.lock().unwrap().as_mut() {
            let mention = if text.contains('@') {
                format!(" {text:?}")
            } else {
                String::new()
            };
            writeln!(file, "{rpc} {} {typed}{mention}", client_id.unwrap_or("-")).unwrap();
            file.flush().unwrap();
        }
    }
}

/// A refusal carrying its `ErrorDetail`, as the real service sends every one.
fn refusal(status: tonic::Code, code: ErrorCode, reason: &str) -> Status {
    refusal_saying(status, code, "refused by the stand-in", reason)
}

fn refusal_saying(status: tonic::Code, code: ErrorCode, message: &str, reason: &str) -> Status {
    Status::with_details(
        status,
        message,
        ErrorDetail {
            code: code as i32,
            message: message.to_string(),
            reason: reason.to_string(),
        }
        .encode_to_vec()
        .into(),
    )
}

/// A cause of `kind`, with the slots that kind fills.
fn cause(kind: Option<&str>) -> Option<TurnCause> {
    let mut cause = TurnCause::default();
    match kind {
        Some("task") => {
            cause.kind = TurnCauseKind::Task as i32;
            cause.task_id = Some("task-1".into());
            cause.description = Some("cargo test".into());
        }
        Some("scheduled") => {
            cause.kind = TurnCauseKind::Scheduled as i32;
            cause.schedule_reason = Some("check the deploy".into());
        }
        Some("peer") => {
            cause.kind = TurnCauseKind::Peer as i32;
            cause.peer_name = Some("reviewer".into());
            cause.text = Some("look at line 4".into());
        }
        _ => cause.kind = TurnCauseKind::Unknown as i32,
    }
    Some(cause)
}

/// Publishes readiness and waits for the fixture to release this gate.
async fn wait_for_gate(directory: &str) {
    let dir = std::path::Path::new(directory);
    std::fs::write(dir.join("ready"), b"").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(180);
    while !dir.join("release").exists() {
        assert!(std::time::Instant::now() < deadline, "gate was never released");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// Runs a turn's script from `steps`, until its end or a permission request.
fn run(shared: Arc<Shared>, session_id: String, mut turn_id: String, mut steps: Vec<String>) {
    tokio::spawn(async move {
        while !steps.is_empty() {
            let step = steps.remove(0);
            let mut words = step.split_whitespace();
            match words.next() {
                Some("text") => {
                    let message_id = words.next().unwrap_or("m").to_string();
                    let text = words.collect::<Vec<_>>().join(" ");
                    shared.emit(
                        &session_id,
                        Some(&turn_id),
                        Event::TextDelta(TextDelta {
                            turn_id: turn_id.clone(),
                            text,
                            message_id: Some(message_id),
                            ..Default::default()
                        }),
                    );
                }
                Some("gate") => {
                    wait_for_gate(words.next().expect("gate directory")).await;
                }
                Some("sleep") => {
                    let ms = words.next().and_then(|ms| ms.parse().ok()).unwrap_or(0);
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
                Some(step @ ("ask" | "ask-unknown")) => {
                    let permission_id = words.next().unwrap_or("p").to_string();
                    if let Some(session) = shared.sessions.lock().unwrap().get_mut(&session_id) {
                        let rest = std::mem::take(&mut steps);
                        session.waiting.insert(permission_id.clone(), (turn_id.clone(), rest));
                    }
                    shared.emit(
                        &session_id,
                        Some(&turn_id),
                        Event::PermissionRequested(PermissionRequested {
                            permission_id: permission_id.clone(),
                            tool_use_id: format!("tool-{permission_id}"),
                            tool_name: "Bash".to_string(),
                            input_json: r#"{"command":"true"}"#.to_string(),
                            origin: if step == "ask-unknown" { UNKNOWN_ORIGIN } else { 0 },
                            ..Default::default()
                        }),
                    );
                    return;
                }
                Some("client-call") => {
                    let call_id = words.next().unwrap_or("c").to_string();
                    let tool = words.next().unwrap_or("t").to_string();
                    let tool_use_id = words.next().unwrap_or_default().to_string();
                    if let Some(session) = shared.sessions.lock().unwrap().get_mut(&session_id) {
                        session.open_calls.push(OpenClientToolCall {
                            call_id: call_id.clone(),
                            tool_use_id: tool_use_id.clone(),
                            tool: tool.clone(),
                        });
                    }
                    shared.emit(
                        &session_id,
                        None,
                        Event::ClientToolCall(ClientToolCall {
                            call_id,
                            tool_use_id,
                            tool,
                            arguments_json: r#"{"path":"/the/arguments"}"#.to_string(),
                        }),
                    );
                }
                Some("unknown-event") => shared.emit_any(&session_id, Some(&turn_id), None),
                Some("hold-answers") => {
                    shared.holding_answers.send_replace(true);
                }
                Some("release-answers") => {
                    shared.holding_answers.send_replace(false);
                }
                Some("drop-watches") => {
                    let mut sessions = shared.sessions.lock().unwrap();
                    if let Some(session) = sessions.get_mut(&session_id) {
                        session.watchers.clear();
                    }
                }
                Some("ignore-eof") => IGNORE_EOF.store(true, Ordering::SeqCst),
                Some("cli-start") => {
                    turn_id = shared.id("cli");
                    shared.emit(
                        &session_id,
                        Some(&turn_id),
                        Event::TurnStarted(TurnStarted {
                            turn_id: turn_id.clone(),
                            origin: TurnOrigin::Cli as i32,
                            cause: cause(words.next()),
                        }),
                    );
                }
                Some("cause") => shared.emit(
                    &session_id,
                    Some(&turn_id),
                    Event::TurnCauseUpdated(TurnCauseUpdated {
                        turn_id: turn_id.clone(),
                        cause: cause(words.next()),
                    }),
                ),
                Some("cycle") => shared.emit(
                    &session_id,
                    Some(&turn_id),
                    Event::CliCycleStarted(CliCycleStarted {
                        turn_id: turn_id.clone(),
                        cause: cause(words.next()),
                    }),
                ),
                Some("deliver") => shared.emit(
                    &session_id,
                    Some(&turn_id),
                    Event::UserMessageDelivered(UserMessageDelivered {
                        client_message_id: shared.client_id(&session_id),
                        turn_id: turn_id.clone(),
                        into_running_turn: true,
                    }),
                ),
                Some("drop") => shared.emit(
                    &session_id,
                    Some(&turn_id),
                    Event::UserMessageDropped(UserMessageDropped {
                        client_message_id: shared.client_id(&session_id),
                        reason: UserMessageDropReason::Cancelled as i32,
                    }),
                ),
                Some("end-no-result") => shared.emit(
                    &session_id,
                    Some(&turn_id),
                    Event::TurnCompleted(TurnCompleted {
                        turn_id: turn_id.clone(),
                        outcome: TurnOutcome::Failed as i32,
                        terminal_reason: Some("no_result".to_string()),
                        ..Default::default()
                    }),
                ),
                Some("end") => shared.emit(
                    &session_id,
                    Some(&turn_id),
                    Event::TurnCompleted(TurnCompleted {
                        turn_id: turn_id.clone(),
                        outcome: TurnOutcome::Completed as i32,
                        result_text: "done".to_string(),
                        ..Default::default()
                    }),
                ),
                _ => {}
            }
        }
    });
}

fn handshake(shared: &Shared) -> HandshakeResponse {
    let mut response = HandshakeResponse {
        protocol_major: 3,
        protocol_minor: 21,
        sidecar_version: "stand-in".to_string(),
        capabilities: [
            "handshake",
            "create_session",
            "send_turn",
            "watch_session_events",
            "interrupt_turn",
            "resolve_permission",
            "close_session",
            "resume_session",
            "text_delta_message_id",
            "steer_turn",
            "system_prompt_preset",
        ]
        .iter()
        .map(ToString::to_string)
        .collect(),
        permission_modes: vec!["interactive".to_string()],
        max_message_bytes: shared.max_message_bytes(),
        ..Default::default()
    };
    if shared.without_system_prompt_preset {
        response.capabilities.retain(|c| c != "system_prompt_preset");
    }
    if shared.cli_turns {
        response.protocol_minor = 22;
        response
            .capabilities
            .extend(["cli_turns".to_string(), "human_origin".to_string()]);
    }
    if shared.client_tools {
        response.protocol_minor = 23;
        response.capabilities.push("client_tools".to_string());
    }
    let scripted = &shared.scripted;
    if let Some(capabilities) = &scripted.capabilities {
        response.capabilities = capabilities.clone();
    }
    response.protocol_major = scripted.major.unwrap_or(response.protocol_major);
    response.protocol_minor = scripted.minor.unwrap_or(response.protocol_minor);
    response
}

#[derive(Clone, Default)]
struct StandIn {
    shared: Arc<Shared>,
}

impl NamedService for StandIn {
    const NAME: &'static str = SERVICE;
}

/// One unary call: `answer` takes the request's message.
async fn unary<Req, Resp, F, Fut>(req: http::Request<Body>, answer: F) -> http::Response<Body>
where
    Req: prost::Message + Default + Send + 'static,
    Resp: prost::Message + Send + 'static,
    F: FnOnce(Req) -> Fut + Send + 'static,
    Fut: Future<Output = Result<Resp, Status>> + Send + 'static,
{
    let answer = Mutex::new(Some(answer));
    let service = tower::service_fn(move |request: tonic::Request<Req>| {
        let answer = answer.lock().unwrap().take().expect("one call per request");
        let reply = answer(request.into_inner());
        async move { reply.await.map(tonic::Response::new) }
    });
    Grpc::new(ProstCodec::<Resp, Req>::default()).unary(service, req).await
}

impl Service<http::Request<Body>> for StandIn {
    type Response = http::Response<Body>;
    type Error = std::convert::Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<Body>) -> Self::Future {
        let shared = Arc::clone(&self.shared);
        Box::pin(async move {
            let method = req
                .uri()
                .path()
                .strip_prefix(&format!("/{SERVICE}/"))
                .unwrap_or_default()
                .to_string();
            let response = match method.as_str() {
                "Handshake" => {
                    unary(req, move |_: HandshakeRequest| async move {
                        if shared.scripted.stall_handshake {
                            std::future::pending::<()>().await;
                        }
                        if let Some(words) = &shared.scripted.incompatible {
                            return Err(refusal_saying(
                                tonic::Code::FailedPrecondition,
                                ErrorCode::IncompatibleProtocol,
                                words,
                                "",
                            ));
                        }
                        Ok(OnTheWire {
                            message: handshake(&shared),
                            unknown: if shared.scripted.unknown_wire {
                                unknown_field(UNKNOWN_FIELD)
                            } else {
                                Vec::new()
                            },
                        })
                    })
                    .await
                }
                "CreateSession" => {
                    unary(req, move |request: CreateSessionRequest| async move {
                        // Record before consuming any fields, so assertions see exactly the decoded wire request.
                        if let Some(file) = shared.record_create.lock().unwrap().as_mut() {
                            let bytes = request.encode_to_vec();
                            file.write_all(&(bytes.len() as u32).to_le_bytes()).unwrap();
                            file.write_all(&bytes).unwrap();
                            file.flush().unwrap();
                        }
                        let session_id = shared.id("session");
                        let provider_session_id = request
                            .resume_provider_session_id
                            .unwrap_or_else(|| format!("claude-{session_id}"));
                        shared
                            .sessions
                            .lock()
                            .unwrap()
                            .insert(session_id.clone(), Session::default());
                        shared.emit(
                            &session_id,
                            None,
                            Event::SessionReady(SessionReady {
                                session_id: session_id.clone(),
                                provider_session_id,
                                model: "stand-in".to_string(),
                                cwd: request.cwd,
                                permission_mode: "default".to_string(),
                                ..Default::default()
                            }),
                        );
                        Ok(CreateSessionResponse {
                            session_id,
                            ..Default::default()
                        })
                    })
                    .await
                }
                "SendTurn" => {
                    unary(req, move |request: SendTurnRequest| async move {
                        shared.record_send(
                            "send",
                            request.client_message_id.as_deref(),
                            request.typed_by_person,
                            &request.text,
                        );
                        if let Some(session) = shared.sessions.lock().unwrap().get_mut(&request.session_id) {
                            session.last_client_id = request.client_message_id.clone();
                        }
                        let turn_id = shared.id("turn");
                        shared.emit(
                            &request.session_id,
                            Some(&turn_id),
                            Event::TurnStarted(TurnStarted {
                                turn_id: turn_id.clone(),
                                ..Default::default()
                            }),
                        );
                        let steps = request.text.split(';').map(|step| step.trim().to_string()).collect();
                        run(Arc::clone(&shared), request.session_id, turn_id.clone(), steps);
                        Ok(SendTurnResponse { turn_id })
                    })
                    .await
                }
                "SteerTurn" => {
                    unary(req, move |request: SteerTurnRequest| async move {
                        shared.record_send(
                            "steer",
                            Some(&request.client_message_id),
                            request.typed_by_person,
                            &request.text,
                        );
                        let after = request.text.trim().parse().unwrap_or(500);
                        let turn_id = request.expected_turn_id.clone();
                        let shared = Arc::clone(&shared);
                        tokio::spawn(async move {
                            if let Some(dir) = request.text.strip_prefix("gate ") {
                                wait_for_gate(dir).await;
                            } else {
                                tokio::time::sleep(Duration::from_millis(after)).await;
                            }
                            shared.emit(
                                &request.session_id,
                                Some(&request.expected_turn_id),
                                Event::UserMessageDelivered(UserMessageDelivered {
                                    client_message_id: request.client_message_id,
                                    turn_id: request.expected_turn_id.clone(),
                                    into_running_turn: true,
                                }),
                            );
                        });
                        Ok(SteerTurnResponse { turn_id })
                    })
                    .await
                }
                "ResolvePermission" => {
                    unary(req, move |request: ResolvePermissionRequest| async move {
                        let rest = shared
                            .sessions
                            .lock()
                            .unwrap()
                            .get_mut(&request.session_id)
                            .and_then(|session| session.waiting.remove(&request.permission_id));
                        let Some((turn_id, steps)) = rest else {
                            return Err(Status::not_found("no such permission"));
                        };
                        let outcome = if request.allow {
                            PermissionOutcome::Allowed
                        } else {
                            PermissionOutcome::Denied
                        };
                        shared.emit(
                            &request.session_id,
                            Some(&turn_id),
                            Event::PermissionResolved(PermissionResolved {
                                permission_id: request.permission_id,
                                outcome: outcome as i32,
                            }),
                        );
                        run(Arc::clone(&shared), request.session_id, turn_id, steps);
                        Ok(ResolvePermissionResponse {})
                    })
                    .await
                }
                "AnswerClientToolCall" => {
                    unary(req, move |request: AnswerClientToolCallRequest| async move {
                        let taken = shared.take_answer(&request);
                        let verdict = match &taken {
                            Ok(()) => "taken".to_string(),
                            Err(status) => format!("{:?}", status.code()),
                        };
                        let text: Vec<&str> = request
                            .content
                            .iter()
                            .filter_map(|block| block.kind.as_ref())
                            .map(|ContentKind::Text(text)| text.as_str())
                            .collect();
                        shared.record_answer(&format!(
                            "{} {verdict} {} {}",
                            request.call_id,
                            request.is_error,
                            text.join("|")
                        ));
                        if taken.is_ok() {
                            shared.emit(
                                &request.session_id,
                                None,
                                Event::ClientToolCallSettled(ClientToolCallSettled {
                                    call_id: request.call_id.clone(),
                                    outcome: ClientToolCallOutcome::Answered as i32,
                                }),
                            );
                        }
                        let mut holding = shared.holding_answers.subscribe();
                        let _ = holding.wait_for(|held| !*held).await;
                        shared.record_answer(&format!("replied {}", request.call_id));
                        taken.map(|()| AnswerClientToolCallResponse {})
                    })
                    .await
                }
                "GetClientToolCallState" => {
                    unary(req, move |request: GetClientToolCallStateRequest| async move {
                        let sessions = shared.sessions.lock().unwrap();
                        let session = sessions
                            .get(&request.session_id)
                            .ok_or_else(|| refusal(tonic::Code::NotFound, ErrorCode::SessionNotFound, ""))?;
                        Ok(GetClientToolCallStateResponse {
                            through_sequence: session.ring.len() as u64,
                            open: session.open_calls.clone(),
                        })
                    })
                    .await
                }
                "InterruptTurn" => unary(req, |_: InterruptTurnRequest| async { Ok(InterruptTurnResponse {}) }).await,
                "CloseSession" => {
                    unary(req, move |request: CloseSessionRequest| async move {
                        shared.emit(
                            &request.session_id,
                            None,
                            Event::SessionClosed(SessionClosed {
                                reason: SessionCloseReason::ClosedByHost as i32,
                            }),
                        );
                        // The terminal event precedes stream completion, as it does on the real service.
                        if let Some(session) = shared.sessions.lock().unwrap().get_mut(&request.session_id) {
                            session.closed = true;
                            session.watchers.clear();
                        }
                        Ok(CloseSessionResponse {})
                    })
                    .await
                }
                "WatchSessionEvents" => {
                    let service = tower::service_fn(move |request: tonic::Request<WatchSessionEventsRequest>| {
                        let shared = Arc::clone(&shared);
                        async move {
                            let request = request.into_inner();
                            let after = request.after_sequence.unwrap_or(0);
                            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                            let mut sessions = shared.sessions.lock().unwrap();
                            let Some(session) = sessions.get_mut(&request.session_id) else {
                                return Err(Status::not_found("no such session"));
                            };
                            for event in session.ring.iter().filter(|event| event.sequence > after) {
                                let _ = tx.send(Ok(shared.on_the_wire(event.clone())));
                            }
                            if !session.closed {
                                session.watchers.push(tx);
                            }
                            Ok(tonic::Response::new(UnboundedReceiverStream::new(rx)))
                        }
                    });
                    Grpc::new(ProstCodec::<OnTheWire<SessionEvent>, WatchSessionEventsRequest>::default())
                        .server_streaming(service, req)
                        .await
                }
                _ => Status::unimplemented(method).into_http(),
            };
            Ok(response)
        })
    }
}

fn main() {
    let mut shared = Shared {
        scripted: Scripted::from_environment(),
        ..Shared::default()
    };
    if let Some(words) = std::env::var_os("EITRI_STANDIN_REFUSE_BIND") {
        std::io::stderr().write_all(words.as_encoded_bytes()).unwrap();
        std::process::exit(1);
    }
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--without-system-prompt-preset") => shared.without_system_prompt_preset = true,
            Some("--cli-turns") => shared.cli_turns = true,
            Some("--client-tools") => shared.client_tools = true,
            Some("--max-message-bytes") => {
                let limit = args.next().expect("a limit");
                shared.max_message_bytes = Some(limit.to_str().and_then(|n| n.parse().ok()).expect("a number"));
            }
            Some("--record-answers") => {
                let path = std::path::PathBuf::from(args.next().expect("record file"));
                assert!(path.is_absolute(), "record file must be absolute");
                *shared.record_answers.get_mut().unwrap() = Some(agent::private_fs::open_private(&path).unwrap());
            }
            Some("--record-sends") => {
                let path = std::path::PathBuf::from(args.next().expect("record file"));
                assert!(path.is_absolute(), "record file must be absolute");
                *shared.record_sends.get_mut().unwrap() = Some(agent::private_fs::open_private(&path).unwrap());
            }
            Some("--record-create") => {
                let path = std::path::PathBuf::from(args.next().expect("record file"));
                assert!(path.is_absolute(), "record file must be absolute");
                *shared.record_create.get_mut().unwrap() = Some(agent::private_fs::open_private(&path).unwrap());
            }
            _ => panic!("unknown stand-in argument: {arg:?}"),
        }
    }
    let socket = std::env::var_os("VERDANDI_CLAUDE_SIDECAR_SOCKET").expect("the socket to serve on");
    // Ends on stdin's end of file, as the real sidecar does.
    std::thread::spawn(|| {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 256];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        loop {
            if !IGNORE_EOF.load(Ordering::SeqCst) {
                std::process::exit(0);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    if std::env::var_os("EITRI_STANDIN_NEVER_BIND").is_some() {
        loop {
            std::thread::park();
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    runtime.block_on(async move {
        let listener = tokio::net::UnixListener::bind(&socket).expect("the socket binds");
        println!("stand-in sidecar: serving");
        if !shared.scripted.stall_handshake {
            eprintln!("stand-in sidecar: started");
        }
        // A line in progress on each stream, finished only at the end of the process's life.
        print!("stand-in stdout line in progress");
        let _ = std::io::stdout().flush();
        tonic::transport::Server::builder()
            .add_service(StandIn {
                shared: Arc::new(shared),
            })
            .serve_with_incoming(UnixListenerStream::new(listener))
            .await
            .expect("the stand-in serves");
    });
}
