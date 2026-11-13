//! A stand-in for Verdandi's Claude sidecar, for the tests of handing a sidecar from one process to
//! another. It speaks the sidecar's gRPC service on the socket `VERDANDI_CLAUDE_SIDECAR_SOCKET` names,
//! with what those tests rest on and nothing more:
//! - every session keeps a ring of its events, numbered from 1, and a watch replays the ring
//!   after the sequence it asks for, then follows live; any number of watchers per session;
//!   a future cursor is always refused with `INVALID_CONFIGURATION`, even with an unbounded ring;
//! - a permission is answered from any connection;
//! - the process ends on stdin's end of file unless parent watching is disabled or a turn told it
//!   to ignore that end, so a holder has to kill it.
//!
//! A turn's text is a script of steps separated by `;`:
//! - `text <message id> <words>`: a text delta of that message;
//! - `sleep <ms>`;
//! - `gate <directory>`: publish `ready`, wait for `release`, then remove the directory;
//! - `ask <permission id>`: a permission request; the rest of the script runs once it is answered;
//! - `ask-write <permission id> <JSON path>`: a gated file write, performed only on Allow;
//!   emits the matching tool call and result, including a denial reason on Deny;
//! - `ask-then-end <permission id>`: request permission and end the turn with the request unresolved;
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
//! With `--turn-content` the stand-in offers protocol 3.26's `turn_content_v1` with the real sidecar's
//! `turn_content_limits` (four media types, 8 images, 3,670,016 bytes each and together), or with
//! `--content-limits <images>/<bytes each>/<bytes together>/<media types, comma separated>` those instead. It
//! takes whatever content arrives (the host's own checks are what the tests read), unless
//! `--refuse-content <reason>` makes it refuse every `SendTurn` carrying content with
//! `INVALID_CONFIGURATION` and that reason, before anything else happens, as the sidecar refuses content
//! it does not accept. A `SendTurn`'s line in `--record-sends` then names each image after the
//! typed mark, in order, as `image:<media type>:<length>:<sha-256 hex>`; never its bytes.
//! `--record-turns <file>` appends each `SendTurn` request as it arrived, length-prefixed as
//! `--record-create` writes, with every image's bytes emptied.
//!
//! `--record-answers <file>` appends a line per `AnswerClientToolCall`, as it arrives and before any
//! hold: `<call id> <taken, or the refusal> <is_error> <its text, blocks joined by |>`, and
//! `replied <call id>` when its reply goes out. With `--plan-mode` it also appends a line per plan decision,
//! `plan <interaction id> approve <mode> <digest>` or `plan <interaction id> keep <feedback>`, followed by
//! ` refused <reason>` when it was refused, and one per `SetPlanning`, `set-planning <enter> <the mode
//! acknowledged, or refused <reason>>`.
//!
//! With `--plan-mode` the stand-in offers protocol 3.26's `plan_mode_v1` (with the CLI's own prompts, which
//! Verdandi requires beside it). A session created with `client_interactions.plan_mode` must state its CLI
//! mode, as Verdandi requires, and reports that mode in `SessionReady`. `SetPlanning` moves it at once and
//! reports the move with `CliModeReported`; it refuses as Verdandi does (`planning_not_offered`,
//! `planning_interaction_pending`, `planning_already`). An approval reports the session's creation mode after
//! its settlement; "keep planning" settles with the feedback as sent. A script can drive it:
//! - `plan <text>`: the model proposes a plan with that text; the rest of the script runs once it is settled;
//! - `refuse-planning <reason>`: the next `SetPlanning` is refused with that reason (`TURN_ALREADY_ACTIVE`
//!   for `planning_turn_running`, `INVALID_CONFIGURATION` otherwise), nothing moved;
//! - `slow-planning`: the next `SetPlanning` moves the session and reports it, then answers
//!   `DEADLINE_EXCEEDED` with no reason, as the sidecar does when the CLI acknowledged late;
//! - `late-planning <ms>`: the next `SetPlanning` waits that long before it moves and answers.
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
//! - `EITRI_STANDIN_NEVER_BIND`: the process stays up, silent, and never binds;
//! - `VERDANDI_CLAUDE_SIDECAR_PARENT_WATCH`: trimmed; unset, blank or `stdin` watches stdin's EOF;
//!   `none` never reads stdin; any other value refuses startup;
//! - `EITRI_STANDIN_RECORD_CREATE=<absolute file>`: the same length-prefixed prost records as
//!   `--record-create <file>` (the flag takes precedence when both are supplied);
//! - `EITRI_STANDIN_RECORD_ENV=<absolute file>`: atomically publish a private snapshot of the whole
//!   environment at startup, including these knobs, as NUL-separated `name=value` entries with their original bytes;
//! - `EITRI_STANDIN_STDERR_FLOOD=<bytes>`: after binding, write this many bytes in 64 KiB chunks a few
//!   milliseconds apart on a separate thread; write errors do not stop the process;
//! - `EITRI_STANDIN_RING=<n>`: retain only the newest `n` events per session (unset is unbounded),
//!   refusing evicted cursors with `EVENT_GAP`.

use claude_runtime_protocol::v1::client_tool_content::Kind as ContentKind;
use claude_runtime_protocol::v1::resolve_interaction_request::Result as Decision;
use claude_runtime_protocol::v1::session_event::Event;
use claude_runtime_protocol::v1::{
    interaction_requested::Payload, CliModeReported, CliPermissionMode, InteractionKind, InteractionOutcome,
    InteractionRequested, InteractionResolved, PlanProposal, ResolveInteractionRequest, ResolveInteractionResponse,
    SetPlanningRequest, SetPlanningResponse,
};
use claude_runtime_protocol::v1::{turn_content_block, TurnContentBlock, TurnContentLimits};
use claude_runtime_protocol::v1::{
    AnswerClientToolCallRequest, AnswerClientToolCallResponse, CliCycleStarted, ClientToolCall, ClientToolCallOutcome,
    ClientToolCallSettled, CloseSessionRequest, CloseSessionResponse, CreateSessionRequest, CreateSessionResponse,
    ErrorCode, ErrorDetail, GetClientToolCallStateRequest, GetClientToolCallStateResponse, HandshakeRequest,
    HandshakeResponse, InterruptTurnRequest, InterruptTurnResponse, OpenClientToolCall, PermissionOutcome,
    PermissionRequested, PermissionResolved, ResolvePermissionRequest, ResolvePermissionResponse, SendTurnRequest,
    SendTurnResponse, SessionCloseReason, SessionClosed, SessionEvent, SessionReady, SteerTurnRequest,
    SteerTurnResponse, TextDelta, ToolCallCompleted, ToolCallStarted, TurnCause, TurnCauseKind, TurnCauseUpdated,
    TurnCompleted, TurnOrigin, TurnOutcome, TurnStarted, UserMessageDelivered, UserMessageDropReason,
    UserMessageDropped, WatchSessionEventsRequest,
};
use prost::bytes::{Buf, BufMut};
use prost::encoding::{DecodeContext, WireType};
use prost::Message as _;
use std::collections::{HashMap, HashSet, VecDeque};
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
    ring: VecDeque<SessionEvent>,
    latest_sequence: u64,
    watchers: Vec<Watcher>,
    /// The rest of a script, by the permission it waits on, with its turn.
    waiting: HashMap<String, PendingPermission>,
    /// The client id of the latest `SendTurn`'s message, if it named one.
    last_client_id: Option<String>,
    /// The client tool calls waiting for an answer, oldest first.
    open_calls: Vec<OpenClientToolCall>,
    /// The client tool calls answered, each of them spent.
    answered_calls: HashSet<String>,
    /// Created with `client_interactions.plan_mode`.
    plan_offered: bool,
    /// The CLI's word for the mode the session was created in.
    creation: String,
    /// Whether the session's CLI runs plan mode.
    in_plan: bool,
    /// The plan waiting for a decision.
    waiting_plan: Option<WaitingPlan>,
    /// The reason the next `SetPlanning` is refused with.
    refuse_planning: Option<String>,
    /// The next `SetPlanning` moves, then answers `DEADLINE_EXCEEDED`.
    slow_planning: bool,
    /// The next `SetPlanning` waits this long before it moves and answers.
    late_planning_ms: Option<u64>,
}

/// A plan the model proposed and nobody decided yet: the rest of its script waits for the decision.
struct WaitingPlan {
    interaction_id: String,
    plan_sha256: String,
    turn_id: String,
    rest: Vec<String>,
}

struct PendingPermission {
    turn_id: String,
    steps: Vec<String>,
    write_path: Option<std::path::PathBuf>,
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
    ring_capacity: Option<usize>,
    plan_mode: bool,
    /// The handshake's `turn_content_limits` and `turn_content_v1`, when offered.
    turn_content: Option<TurnContentLimits>,
    /// The reason every `SendTurn` carrying content is refused with.
    refuse_content: Option<String>,
    record_turns: Mutex<Option<std::fs::File>>,
}

/// The limits the real sidecar states.
fn sidecar_content_limits() -> TurnContentLimits {
    TurnContentLimits {
        image_media_types: ["image/png", "image/jpeg", "image/gif", "image/webp"]
            .map(String::from)
            .to_vec(),
        max_images_per_turn: 8,
        max_image_bytes: 3_670_016,
        max_content_bytes: 3_670_016,
    }
}

/// `<images>/<bytes each>/<bytes together>/<media types, comma separated>`.
fn parse_content_limits(text: &str) -> TurnContentLimits {
    let parts: Vec<&str> = text.splitn(4, '/').collect();
    assert_eq!(parts.len(), 4, "content limits: {text}");
    TurnContentLimits {
        max_images_per_turn: parts[0].parse().expect("a count"),
        max_image_bytes: parts[1].parse().expect("a size"),
        max_content_bytes: parts[2].parse().expect("a size"),
        image_media_types: parts[3].split(',').map(String::from).collect(),
    }
}

/// Each image of a turn's content as `--record-sends` names it.
fn content_marks(content: &[TurnContentBlock]) -> String {
    use sha2::Digest as _;
    content
        .iter()
        .map(|block| match &block.block {
            Some(turn_content_block::Block::Image(image)) => {
                let digest = sha2::Sha256::digest(&image.data);
                let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
                format!(" image:{}:{}:{hex}", image.media_type, image.data.len())
            }
            None => " block:unknown".to_string(),
        })
        .collect()
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
            sequence: session.latest_sequence + 1,
            occurred_at,
            turn_id: turn_id.map(str::to_string),
            event,
        };
        session.latest_sequence = event.sequence;
        session.ring.push_back(event.clone());
        if let Some(capacity) = self.ring_capacity {
            while session.ring.len() > capacity {
                session.ring.pop_front();
            }
        }
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

    fn record_send(&self, rpc: &str, client_id: Option<&str>, typed: bool, text: &str, content: &[TurnContentBlock]) {
        if let Some(file) = self.record_sends.lock().unwrap().as_mut() {
            let mention = if text.contains('@') {
                format!(" {text:?}")
            } else {
                String::new()
            };
            let images = content_marks(content);
            writeln!(file, "{rpc} {} {typed}{images}{mention}", client_id.unwrap_or("-")).unwrap();
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

/// The reason a refusal carries, empty when it carries none.
fn reason_of(status: &Status) -> String {
    ErrorDetail::decode(status.details())
        .map(|detail| detail.reason)
        .unwrap_or_default()
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
                // `block <message id> <text>`: the first delta of a text block (`starts_block`), as a
                // sidecar with `text_block_start` sends it; `text` is a delta that continues one.
                Some("block") => {
                    let message_id = words.next().unwrap_or("m").to_string();
                    let text = words.collect::<Vec<_>>().join(" ");
                    shared.emit(
                        &session_id,
                        Some(&turn_id),
                        Event::TextDelta(TextDelta {
                            turn_id: turn_id.clone(),
                            text,
                            message_id: Some(message_id),
                            starts_block: true,
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
                Some(kind @ ("ask" | "ask-unknown" | "ask-then-end" | "ask-write")) => {
                    let permission_id = words.next().unwrap_or("p").to_string();
                    let write_path = (kind == "ask-write").then(|| {
                        let path: String = serde_json::from_str(step.splitn(3, ' ').nth(2).expect("JSON path"))
                            .expect("a quoted path");
                        std::path::PathBuf::from(path)
                    });
                    let input_json = match &write_path {
                        Some(path) => serde_json::json!({"file_path": path, "content": "allowed\n"}).to_string(),
                        None => r#"{"command":"true"}"#.to_string(),
                    };
                    let tool_name = if write_path.is_some() { "Write" } else { "Bash" };
                    if write_path.is_some() {
                        shared.emit(
                            &session_id,
                            Some(&turn_id),
                            Event::ToolCallStarted(ToolCallStarted {
                                turn_id: turn_id.clone(),
                                tool_use_id: format!("tool-{permission_id}"),
                                name: tool_name.to_string(),
                                input_json: input_json.clone(),
                                ..Default::default()
                            }),
                        );
                    }
                    if let Some(session) = shared.sessions.lock().unwrap().get_mut(&session_id) {
                        let mut rest = std::mem::take(&mut steps);
                        if kind == "ask-then-end" {
                            rest.clear();
                        }
                        session.waiting.insert(
                            permission_id.clone(),
                            PendingPermission {
                                turn_id: turn_id.clone(),
                                steps: rest,
                                write_path,
                            },
                        );
                    }
                    shared.emit(
                        &session_id,
                        Some(&turn_id),
                        Event::PermissionRequested(PermissionRequested {
                            permission_id: permission_id.clone(),
                            tool_use_id: format!("tool-{permission_id}"),
                            tool_name: tool_name.to_string(),
                            input_json,
                            origin: if kind == "ask-unknown" { UNKNOWN_ORIGIN } else { 0 },
                            ..Default::default()
                        }),
                    );
                    if kind == "ask-then-end" {
                        steps.push("end".to_string());
                    } else {
                        return;
                    }
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
                Some("plan") => {
                    let text = words.collect::<Vec<_>>().join(" ");
                    let interaction_id = shared.id("plan");
                    let plan_sha256 = sha256_hex(&text);
                    if let Some(session) = shared.sessions.lock().unwrap().get_mut(&session_id) {
                        session.waiting_plan = Some(WaitingPlan {
                            interaction_id: interaction_id.clone(),
                            plan_sha256: plan_sha256.clone(),
                            turn_id: turn_id.clone(),
                            rest: std::mem::take(&mut steps),
                        });
                    }
                    shared.emit(
                        &session_id,
                        Some(&turn_id),
                        Event::InteractionRequested(InteractionRequested {
                            tool_use_id: format!("tool-{interaction_id}"),
                            interaction_id,
                            kind: InteractionKind::PlanApproval as i32,
                            payload: Some(Payload::Plan(PlanProposal {
                                plan: text,
                                plan_sha256,
                                allowed_prompts: Vec::new(),
                            })),
                            unsupported_reason: None,
                        }),
                    );
                    return;
                }
                Some("refuse-planning") => {
                    let reason = words.next().unwrap_or("planning_cli_refused").to_string();
                    if let Some(session) = shared.sessions.lock().unwrap().get_mut(&session_id) {
                        session.refuse_planning = Some(reason);
                    }
                }
                Some("late-planning") => {
                    let ms = words.next().and_then(|ms| ms.parse().ok()).unwrap_or(500);
                    if let Some(session) = shared.sessions.lock().unwrap().get_mut(&session_id) {
                        session.late_planning_ms = Some(ms);
                    }
                }
                Some("slow-planning") => {
                    if let Some(session) = shared.sessions.lock().unwrap().get_mut(&session_id) {
                        session.slow_planning = true;
                    }
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

fn sha256_hex(text: &str) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// One `SetPlanning`, decided at once: the mode acknowledged, and whether the answer is the sidecar's
/// deadline instead.
fn set_planning(shared: &Shared, request: &SetPlanningRequest) -> Result<(String, bool), Status> {
    let mut sessions = shared.sessions.lock().unwrap();
    let session = sessions
        .get_mut(&request.session_id)
        .filter(|session| !session.closed)
        .ok_or_else(|| refusal(tonic::Code::NotFound, ErrorCode::SessionNotFound, ""))?;
    let invalid = |reason: &str| refusal(tonic::Code::FailedPrecondition, ErrorCode::InvalidConfiguration, reason);
    if !session.plan_offered {
        return Err(invalid("planning_not_offered"));
    }
    if let Some(reason) = session.refuse_planning.take() {
        return Err(if reason == "planning_turn_running" {
            refusal(tonic::Code::FailedPrecondition, ErrorCode::TurnAlreadyActive, &reason)
        } else {
            invalid(&reason)
        });
    }
    if session.waiting_plan.is_some() {
        return Err(invalid("planning_interaction_pending"));
    }
    if session.in_plan == request.enter {
        return Err(invalid("planning_already"));
    }
    session.in_plan = request.enter;
    let mode = if request.enter {
        "plan".to_string()
    } else {
        session.creation.clone()
    };
    Ok((mode, std::mem::take(&mut session.slow_planning)))
}

/// One plan decision, decided at once: the settlement and what the CLI reports after it, with the rest of
/// the plan's script and its turn.
fn decide_plan(
    shared: &Shared,
    request: &ResolveInteractionRequest,
) -> Result<(InteractionResolved, Option<String>, String, Vec<String>), Status> {
    let refused = |reason: &str| {
        refusal(
            tonic::Code::FailedPrecondition,
            ErrorCode::InteractionAnswerRefused,
            reason,
        )
    };
    let mut sessions = shared.sessions.lock().unwrap();
    let session = sessions
        .get_mut(&request.session_id)
        .filter(|session| !session.closed)
        .ok_or_else(|| refusal(tonic::Code::NotFound, ErrorCode::SessionNotFound, ""))?;
    let Some(waiting) = session
        .waiting_plan
        .as_ref()
        .filter(|plan| plan.interaction_id == request.interaction_id)
    else {
        return Err(refusal(tonic::Code::NotFound, ErrorCode::InteractionNotFound, ""));
    };
    let mut settled = InteractionResolved {
        interaction_id: request.interaction_id.clone(),
        kind: InteractionKind::PlanApproval as i32,
        ..Default::default()
    };
    let report = match &request.result {
        Some(Decision::Approve(approval)) => {
            if approval.mode == 0 {
                return Err(refused("plan_mode_unspecified"));
            }
            if approval.plan_sha256 != waiting.plan_sha256 {
                return Err(refused("plan_digest_mismatch"));
            }
            settled.outcome = InteractionOutcome::Approved as i32;
            settled.mode_after = Some(approval.mode);
            session.in_plan = false;
            Some(session.creation.clone())
        }
        Some(Decision::KeepPlanning(keep)) => {
            if keep.feedback.len() > 16 * 1024 {
                return Err(refused("feedback_too_long"));
            }
            settled.outcome = InteractionOutcome::KeptPlanning as i32;
            settled.feedback = Some(keep.feedback.clone());
            None
        }
        _ => return Err(refused("kind_mismatch")),
    };
    let waiting = session.waiting_plan.take().expect("matched above");
    Ok((settled, report, waiting.turn_id, waiting.rest))
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
    if shared.plan_mode {
        response.protocol_minor = 26;
        response
            .capabilities
            .extend(["plan_mode_v1".to_string(), "provider_permission_prompts".to_string()]);
    }
    if let Some(limits) = &shared.turn_content {
        response.protocol_minor = 26;
        response.capabilities.push("turn_content_v1".to_string());
        response.turn_content_limits = Some(limits.clone());
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
                        let plan_offered = request
                            .client_interactions
                            .as_ref()
                            .is_some_and(|offer| offer.plan_mode);
                        let stated = request
                            .policy
                            .as_ref()
                            .map_or(CliPermissionMode::Unspecified as i32, |policy| {
                                policy.cli_permission_mode
                            });
                        if plan_offered && stated == CliPermissionMode::Unspecified as i32 {
                            return Err(refusal(
                                tonic::Code::InvalidArgument,
                                ErrorCode::InvalidConfiguration,
                                "client_interactions_plan_needs_cli_mode",
                            ));
                        }
                        let creation = if stated == CliPermissionMode::Auto as i32 {
                            "auto"
                        } else {
                            "default"
                        };
                        let session_id = shared.id("session");
                        let provider_session_id = request
                            .resume_provider_session_id
                            .unwrap_or_else(|| format!("claude-{session_id}"));
                        shared.sessions.lock().unwrap().insert(
                            session_id.clone(),
                            Session {
                                plan_offered,
                                creation: creation.to_string(),
                                ..Session::default()
                            },
                        );
                        shared.emit(
                            &session_id,
                            None,
                            Event::SessionReady(SessionReady {
                                session_id: session_id.clone(),
                                provider_session_id,
                                model: "stand-in".to_string(),
                                cwd: request.cwd,
                                permission_mode: creation.to_string(),
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
                        if let Some(file) = shared.record_turns.lock().unwrap().as_mut() {
                            let mut kept = request.clone();
                            for block in &mut kept.content {
                                if let Some(turn_content_block::Block::Image(image)) = &mut block.block {
                                    image.data.clear();
                                }
                            }
                            let bytes = kept.encode_to_vec();
                            file.write_all(&(bytes.len() as u32).to_le_bytes()).unwrap();
                            file.write_all(&bytes).unwrap();
                            file.flush().unwrap();
                        }
                        if let Some(reason) = shared.refuse_content.as_deref()
                            && !request.content.is_empty()
                        {
                            return Err(refusal(
                                tonic::Code::InvalidArgument,
                                ErrorCode::InvalidConfiguration,
                                reason,
                            ));
                        }
                        shared.record_send(
                            "send",
                            request.client_message_id.as_deref(),
                            request.typed_by_person,
                            &request.text,
                            &request.content,
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
                            &[],
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
                        let Some(PendingPermission {
                            turn_id,
                            steps,
                            write_path,
                        }) = rest
                        else {
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
                                permission_id: request.permission_id.clone(),
                                outcome: outcome as i32,
                            }),
                        );
                        if let Some(path) = write_path {
                            if request.allow {
                                agent::private_fs::write_private(&path, b"allowed\n").unwrap();
                            }
                            shared.emit(
                                &request.session_id,
                                Some(&turn_id),
                                Event::ToolCallCompleted(ToolCallCompleted {
                                    turn_id: turn_id.clone(),
                                    tool_use_id: format!("tool-{}", request.permission_id),
                                    content_json: serde_json::json!(if request.allow {
                                        "written".to_string()
                                    } else {
                                        request.reason
                                    })
                                    .to_string(),
                                    is_error: !request.allow,
                                    ..Default::default()
                                }),
                            );
                        }
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
                            through_sequence: session.latest_sequence,
                            open: session.open_calls.clone(),
                        })
                    })
                    .await
                }
                "SetPlanning" => {
                    unary(req, move |request: SetPlanningRequest| async move {
                        let late = shared
                            .sessions
                            .lock()
                            .unwrap()
                            .get_mut(&request.session_id)
                            .and_then(|session| session.late_planning_ms.take());
                        if let Some(ms) = late {
                            tokio::time::sleep(Duration::from_millis(ms)).await;
                        }
                        let decided = set_planning(&shared, &request);
                        let verdict = match &decided {
                            Ok((mode, _)) => mode.clone(),
                            Err(status) => format!("refused {}", reason_of(status)),
                        };
                        shared.record_answer(&format!("set-planning {} {verdict}", request.enter));
                        let (mode, slow) = decided?;
                        shared.emit(
                            &request.session_id,
                            None,
                            Event::CliModeReported(CliModeReported {
                                permission_mode: mode.clone(),
                            }),
                        );
                        if slow {
                            return Err(refusal(tonic::Code::DeadlineExceeded, ErrorCode::DeadlineExceeded, ""));
                        }
                        Ok(SetPlanningResponse { permission_mode: mode })
                    })
                    .await
                }
                "ResolveInteraction" => {
                    unary(req, move |request: ResolveInteractionRequest| async move {
                        let decided = decide_plan(&shared, &request);
                        let what = match &request.result {
                            Some(Decision::Approve(approval)) => {
                                format!("approve {} {}", approval.mode, approval.plan_sha256)
                            }
                            Some(Decision::KeepPlanning(keep)) => format!("keep {}", keep.feedback),
                            _ => "other".to_string(),
                        };
                        let refused = match &decided {
                            Ok(_) => String::new(),
                            Err(status) => format!(" refused {}", reason_of(status)),
                        };
                        shared.record_answer(&format!("plan {} {what}{refused}", request.interaction_id));
                        let (settled, report, turn_id, rest) = decided?;
                        let outcome = settled.outcome;
                        shared.emit(&request.session_id, Some(&turn_id), Event::InteractionResolved(settled));
                        if let Some(mode) = report {
                            shared.emit(
                                &request.session_id,
                                Some(&turn_id),
                                Event::CliModeReported(CliModeReported { permission_mode: mode }),
                            );
                        }
                        run(Arc::clone(&shared), request.session_id, turn_id, rest);
                        Ok(ResolveInteractionResponse { outcome })
                    })
                    .await
                }
                "InterruptTurn" => {
                    unary(req, move |request: InterruptTurnRequest| async move {
                        let waiting = shared
                            .sessions
                            .lock()
                            .unwrap()
                            .get_mut(&request.session_id)
                            .map(|session| std::mem::take(&mut session.waiting))
                            .ok_or_else(|| refusal(tonic::Code::NotFound, ErrorCode::SessionNotFound, ""))?;
                        let mut turns = HashSet::new();
                        for (permission_id, pending) in waiting {
                            shared.emit(
                                &request.session_id,
                                Some(&pending.turn_id),
                                Event::PermissionResolved(PermissionResolved {
                                    permission_id,
                                    outcome: PermissionOutcome::CancelledByInterrupt as i32,
                                }),
                            );
                            turns.insert(pending.turn_id);
                        }
                        for turn_id in turns {
                            shared.emit(
                                &request.session_id,
                                Some(&turn_id),
                                Event::TurnCompleted(TurnCompleted {
                                    turn_id: turn_id.clone(),
                                    outcome: TurnOutcome::Interrupted as i32,
                                    ..Default::default()
                                }),
                            );
                        }
                        Ok(InterruptTurnResponse {})
                    })
                    .await
                }
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
                            if request.after_sequence.is_some() {
                                if after > session.latest_sequence {
                                    return Err(refusal(
                                        tonic::Code::InvalidArgument,
                                        ErrorCode::InvalidConfiguration,
                                        "replay_cursor_in_future",
                                    ));
                                }
                                let oldest = session
                                    .ring
                                    .front()
                                    .map_or(session.latest_sequence.saturating_add(1), |event| event.sequence);
                                if after < oldest.saturating_sub(1) {
                                    return Err(refusal(tonic::Code::OutOfRange, ErrorCode::EventGap, ""));
                                }
                            }
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
    let parent_watch = std::env::var_os("VERDANDI_CLAUDE_SIDECAR_PARENT_WATCH").unwrap_or_default();
    let parent_watch = parent_watch.to_string_lossy();
    // JavaScript's trim includes the byte-order mark and excludes the Unicode next-line character.
    let value = parent_watch.trim_matches(|c: char| (c.is_whitespace() && c != '\u{0085}') || c == '\u{feff}');
    let watch_stdin = match value {
        "" | "stdin" => true,
        "none" => false,
        _ => {
            eprintln!(
                "VERDANDI_CLAUDE_SIDECAR_PARENT_WATCH: expected \"stdin\" or \"none\", got {}",
                serde_json::to_string(parent_watch.as_ref()).unwrap()
            );
            std::process::exit(1);
        }
    };
    if let Some(path) = std::env::var_os("EITRI_STANDIN_RECORD_ENV") {
        let path = std::path::PathBuf::from(path);
        assert!(path.is_absolute(), "record file must be absolute");
        let mut bytes = Vec::new();
        for (name, value) in std::env::vars_os() {
            bytes.extend_from_slice(name.as_encoded_bytes());
            bytes.push(b'=');
            bytes.extend_from_slice(value.as_encoded_bytes());
            bytes.push(0);
        }
        let directory = agent::private_fs::open_dir(path.parent().unwrap()).unwrap();
        agent::private_fs::publish_private_file(&directory, path.file_name().unwrap(), &bytes).unwrap();
    }
    let mut shared = Shared {
        scripted: Scripted::from_environment(),
        ring_capacity: std::env::var("EITRI_STANDIN_RING")
            .ok()
            .map(|n| n.parse().expect("a ring capacity")),
        ..Shared::default()
    };
    if let Some(words) = std::env::var_os("EITRI_STANDIN_REFUSE_BIND") {
        std::io::stderr().write_all(words.as_encoded_bytes()).unwrap();
        std::process::exit(1);
    }
    let mut record_create = std::env::var_os("EITRI_STANDIN_RECORD_CREATE");
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--without-system-prompt-preset") => shared.without_system_prompt_preset = true,
            Some("--cli-turns") => shared.cli_turns = true,
            Some("--client-tools") => shared.client_tools = true,
            Some("--plan-mode") => shared.plan_mode = true,
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
                record_create = Some(args.next().expect("record file"));
            }
            Some("--turn-content") => {
                shared.turn_content.get_or_insert_with(sidecar_content_limits);
            }
            Some("--content-limits") => {
                let limits = args.next().expect("the limits");
                shared.turn_content = Some(parse_content_limits(limits.to_str().expect("text")));
            }
            Some("--refuse-content") => {
                shared.refuse_content = Some(args.next().expect("a reason").into_string().expect("text"));
            }
            Some("--record-turns") => {
                let path = std::path::PathBuf::from(args.next().expect("record file"));
                assert!(path.is_absolute(), "record file must be absolute");
                *shared.record_turns.get_mut().unwrap() = Some(agent::private_fs::open_private(&path).unwrap());
            }
            _ => panic!("unknown stand-in argument: {arg:?}"),
        }
    }
    if let Some(path) = record_create {
        let path = std::path::PathBuf::from(path);
        assert!(path.is_absolute(), "record file must be absolute");
        *shared.record_create.get_mut().unwrap() = Some(agent::private_fs::open_private(&path).unwrap());
    }
    let socket = std::env::var_os("VERDANDI_CLAUDE_SIDECAR_SOCKET").expect("the socket to serve on");
    if watch_stdin {
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
    }
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
        if let Ok(bytes) = std::env::var("EITRI_STANDIN_STDERR_FLOOD") {
            let mut remaining: usize = bytes.parse().expect("a byte count");
            std::thread::spawn(move || {
                let chunk = [b'x'; 64 * 1024];
                let mut stderr = std::io::stderr();
                while remaining > 0 {
                    let count = remaining.min(chunk.len());
                    let _ = stderr.write_all(&chunk[..count]);
                    remaining -= count;
                    std::thread::sleep(Duration::from_millis(3));
                }
            });
        }
        tonic::transport::Server::builder()
            .add_service(StandIn {
                shared: Arc::new(shared),
            })
            .serve_with_incoming(UnixListenerStream::new(listener))
            .await
            .expect("the stand-in serves");
    });
}
