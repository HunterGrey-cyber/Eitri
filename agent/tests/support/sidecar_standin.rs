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
//! - `ask <permission id>`: a permission request; the rest of the script runs once it is answered;
//! - `drop-watches`: every open watch stream of the session ends (the clients reconnect);
//! - `ignore-eof`: stdin's end of file no longer ends the process;
//! - `end`: the turn completes.
//!
//! A steered message's text is the number of milliseconds after which it is delivered (500 if it is
//! not a number).

use claude_runtime_protocol::v1::session_event::Event;
use claude_runtime_protocol::v1::{
    CloseSessionRequest, CloseSessionResponse, CreateSessionRequest, CreateSessionResponse, HandshakeRequest,
    HandshakeResponse, InterruptTurnRequest, InterruptTurnResponse, PermissionOutcome, PermissionRequested,
    PermissionResolved, ResolvePermissionRequest, ResolvePermissionResponse, SendTurnRequest, SendTurnResponse,
    SessionCloseReason, SessionClosed, SessionEvent, SessionReady, SteerTurnRequest, SteerTurnResponse, TextDelta,
    TurnCompleted, TurnOutcome, TurnStarted, UserMessageDelivered, WatchSessionEventsRequest,
};
use std::collections::HashMap;
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

type Watcher = tokio::sync::mpsc::UnboundedSender<Result<SessionEvent, Status>>;

#[derive(Default)]
struct Session {
    ring: Vec<SessionEvent>,
    watchers: Vec<Watcher>,
    /// The rest of a script, by the permission it waits on, with its turn.
    waiting: HashMap<String, (String, Vec<String>)>,
}

#[derive(Default)]
struct Shared {
    sessions: Mutex<HashMap<String, Session>>,
    next_id: AtomicU64,
}

static IGNORE_EOF: AtomicBool = AtomicBool::new(false);

impl Shared {
    fn id(&self, prefix: &str) -> String {
        format!("{prefix}-{}", self.next_id.fetch_add(1, Ordering::SeqCst) + 1)
    }

    fn emit(&self, session_id: &str, turn_id: Option<&str>, event: Event) {
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
            event: Some(event),
        };
        session.ring.push(event.clone());
        session
            .watchers
            .retain(|watcher| watcher.send(Ok(event.clone())).is_ok());
    }
}

/// Runs a turn's script from `steps`, until its end or a permission request.
fn run(shared: Arc<Shared>, session_id: String, turn_id: String, mut steps: Vec<String>) {
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
                        }),
                    );
                }
                Some("sleep") => {
                    let ms = words.next().and_then(|ms| ms.parse().ok()).unwrap_or(0);
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
                Some("ask") => {
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
                            ..Default::default()
                        }),
                    );
                    return;
                }
                Some("drop-watches") => {
                    if let Some(session) = shared.sessions.lock().unwrap().get_mut(&session_id) {
                        session.watchers.clear();
                    }
                }
                Some("ignore-eof") => IGNORE_EOF.store(true, Ordering::SeqCst),
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

fn handshake() -> HandshakeResponse {
    HandshakeResponse {
        protocol_major: 3,
        protocol_minor: 20,
        sidecar_version: "stand-in".to_string(),
        capabilities: [
            "handshake",
            "create_session",
            "send_turn",
            "watch_session_events",
            "interrupt_turn",
            "resolve_permission",
            "close_session",
            "text_delta_message_id",
            "steer_turn",
        ]
        .iter()
        .map(ToString::to_string)
        .collect(),
        permission_modes: vec!["interactive".to_string()],
        ..Default::default()
    }
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
                "Handshake" => unary(req, |_: HandshakeRequest| async { Ok(handshake()) }).await,
                "CreateSession" => {
                    unary(req, move |request: CreateSessionRequest| async move {
                        let session_id = shared.id("session");
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
                                provider_session_id: format!("claude-{session_id}"),
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
                        let turn_id = shared.id("turn");
                        shared.emit(
                            &request.session_id,
                            Some(&turn_id),
                            Event::TurnStarted(TurnStarted {
                                turn_id: turn_id.clone(),
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
                        let after = request.text.trim().parse().unwrap_or(500);
                        let turn_id = request.expected_turn_id.clone();
                        let shared = Arc::clone(&shared);
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(after)).await;
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
                                let _ = tx.send(Ok(event.clone()));
                            }
                            session.watchers.push(tx);
                            Ok(tonic::Response::new(UnboundedReceiverStream::new(rx)))
                        }
                    });
                    Grpc::new(ProstCodec::<SessionEvent, WatchSessionEventsRequest>::default())
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
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    runtime.block_on(async move {
        let listener = tokio::net::UnixListener::bind(&socket).expect("the socket binds");
        println!("stand-in sidecar: serving");
        eprintln!("stand-in sidecar: started");
        // A line in progress on each stream, finished only at the end of the process's life.
        print!("stand-in stdout line in progress");
        let _ = std::io::stdout().flush();
        tonic::transport::Server::builder()
            .add_service(StandIn::default())
            .serve_with_incoming(UnixListenerStream::new(listener))
            .await
            .expect("the stand-in serves");
    });
}
