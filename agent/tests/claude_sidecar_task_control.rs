//! Task control against a private gRPC stand-in. No Claude or Codex binary runs.
use agent::{AgentProvider, ClaudeSidecarProvider, CreateSessionRequest, ProviderError, ProviderErrorCode};
use claude_runtime_protocol::v1 as wire;
use prost::Message;
use serde_json::{json, Value};
use std::future::Future;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;
use tonic::body::Body;
use tonic::codegen::tokio_stream::wrappers::UnixListenerStream;
use tonic::codegen::{http, BoxFuture, Context, Poll, Service};
use tonic::server::{Grpc, NamedService};
use tonic::Status;
use tonic_prost::ProstCodec;

const WAIT: Duration = Duration::from_secs(30);

/// Writes an executable script through a child process. Written from this process, the file's write
/// descriptor would be open here while a test on another thread forks; the forked child holds it until
/// it execs, and running the script meanwhile fails with `ETXTBSY` ("Text file busy").
fn write_script(path: &std::path::Path, body: &str) {
    let status = Command::new("sh")
        .arg("-c")
        .arg(r#"umask 077 && printf '%s' "$1" > "$2" && chmod 700 "$2""#)
        .arg("sh")
        .arg(body)
        .arg(path)
        .status()
        .expect("sh runs");
    assert!(status.success(), "writing the script failed: {status}");
}

// Each case runs in a child so its sidecar selection and capability choice are process-local.
fn case(name: &str, run: impl FnOnce(&mut ClaudeSidecarProvider)) {
    if std::env::var("EITRI_TASK_CONTROL_CASE").as_deref() == Ok(name) {
        let mut provider = ClaudeSidecarProvider::connect("t").expect("stand-in connects");
        run(&mut provider);
        return;
    }
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("tc-{}", uuid::Uuid::new_v4().simple()));
    agent::private_fs::create_private_dir_all(&root, &root).unwrap();
    let program = root.join("sidecar");
    let exe = std::env::current_exe().unwrap();
    let quoted = exe.to_str().unwrap().replace('\'', "'\\''");
    write_script(
        &program,
        &format!("#!/bin/sh\nEITRI_TASK_CONTROL_SERVER=1 exec '{quoted}' --exact standin_server --nocapture\n"),
    );
    let output = Command::new(exe)
        .args(["--exact", name, "--nocapture"])
        .env("EITRI_TASK_CONTROL_CASE", name)
        .env("EITRI_TASK_CONTROL_LOG", root.join("calls.jsonl"))
        .env("EITRI_SIDECAR_BINARY", &program)
        .env("XDG_RUNTIME_DIR", &root)
        .env_remove("EITRI_VERDANDI_CHECKOUT")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&root);
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn calls() -> Vec<Value> {
    std::fs::read_to_string(std::env::var_os("EITRI_TASK_CONTROL_LOG").unwrap())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn open(provider: &ClaudeSidecarProvider) {
    provider
        .create_session(CreateSessionRequest {
            cwd: "/".into(),
            streaming: agent::StreamingPreference::Partial,
            project: agent::setting_sources::ProjectTrust::Untrusted,
            client_tools: None,
        })
        .unwrap();
}

#[test]
fn stop_task_sends_one_request_with_a_fresh_command_id() {
    case("stop_task_sends_one_request_with_a_fresh_command_id", |provider| {
        for _ in 0..2 {
            provider
                .stop_task("session", "shell")
                .unwrap()
                .recv_timeout(WAIT)
                .unwrap()
                .unwrap();
        }
        let log = calls();
        let stops: Vec<_> = log.iter().filter(|row| row["method"] == "stop").collect();
        assert_eq!(stops.len(), 2);
        assert_eq!(stops[0]["session"], "session");
        assert_eq!(stops[0]["task"], "shell");
        let ids: Vec<_> = stops
            .iter()
            .map(|row| uuid::Uuid::parse_str(row["command"].as_str().unwrap()).unwrap())
            .collect();
        assert_ne!(ids[0], ids[1]);
    });
}

#[test]
fn stop_background_sends_one_stop_per_task_without_closing_the_session() {
    case(
        "stop_background_sends_one_stop_per_task_without_closing_the_session",
        |provider| {
            provider
                .stop_background("session", 3, &["shell".into(), "workflow".into(), "agent".into()])
                .unwrap();
            let deadline = std::time::Instant::now() + WAIT;
            let log = loop {
                let log = calls();
                if log.iter().filter(|row| row["method"] == "stop").count() == 3 {
                    break log;
                }
                assert!(std::time::Instant::now() < deadline, "stops never reached the stand-in");
                std::thread::sleep(Duration::from_millis(5));
            };
            let mut ids: Vec<_> = log
                .iter()
                .filter(|row| row["method"] == "stop")
                .map(|row| row["task"].as_str().unwrap())
                .collect();
            ids.sort();
            assert_eq!(ids, ["agent", "shell", "workflow"]);
            assert!(!log.iter().any(|row| row["method"] == "close"));
            provider
                .task_output("session", "shell")
                .unwrap()
                .recv_timeout(WAIT)
                .unwrap()
                .unwrap();
        },
    );
}

#[test]
fn task_requests_return_while_the_sidecar_holds_both_answers() {
    case(
        "task_requests_return_while_the_sidecar_holds_both_answers",
        |provider| {
            let stop = provider.stop_task("session", "held-stop").unwrap();
            let output = provider.task_output("session", "held-output").unwrap();
            provider
                .task_output("session", "shell")
                .unwrap()
                .recv_timeout(WAIT)
                .unwrap()
                .unwrap();
            assert!(matches!(stop.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));
            assert!(matches!(output.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));
            agent::private_fs::write_private(
                &PathBuf::from(std::env::var_os("EITRI_TASK_CONTROL_LOG").unwrap()).with_extension("release"),
                b"go",
            )
            .unwrap();
            stop.recv_timeout(WAIT).unwrap().unwrap();
            output.recv_timeout(WAIT).unwrap().unwrap();
        },
    );
}

#[test]
fn task_output_maps_success_unavailable_and_not_found_and_decodes_a_split_character() {
    case(
        "task_output_maps_success_unavailable_and_not_found_and_decodes_a_split_character",
        |provider| {
            let output = provider
                .task_output("session", "shell")
                .unwrap()
                .recv_timeout(WAIT)
                .unwrap()
                .unwrap();
            assert_eq!(output.output, "�tail");
            assert_eq!(output.total_bytes, 9000);
            assert!(output.truncated);
            for (task, code) in [
                ("workflow", ProviderErrorCode::TaskOutputUnavailable),
                ("local-agent", ProviderErrorCode::TaskOutputUnavailable),
                ("gone", ProviderErrorCode::TaskNotFound),
            ] {
                let error = provider
                    .task_output("session", task)
                    .unwrap()
                    .recv_timeout(WAIT)
                    .unwrap()
                    .unwrap_err();
                assert!(matches!(error, ProviderError::Provider {code: actual, ..} if actual == code));
                assert!(error.is_benign());
            }
            let output = provider
                .task_output("session", "oversized")
                .unwrap()
                .recv_timeout(WAIT)
                .unwrap()
                .unwrap();
            assert_eq!(output.output.len(), 8192);
            assert!(output.output.ends_with("tail"));
            assert!(output.truncated);
            assert_eq!(output.total_bytes, 10000);
        },
    );
}

#[test]
fn missing_task_capabilities_refuse_without_sending_an_rpc() {
    case("missing_task_capabilities_refuse_without_sending_an_rpc", |provider| {
        assert!(matches!(
            provider.stop_task("session", "shell"),
            Err(ProviderError::UnsupportedCapability("stop_task"))
        ));
        assert!(matches!(
            provider.task_output("session", "shell"),
            Err(ProviderError::UnsupportedCapability("task_output"))
        ));
        assert!(matches!(
            provider.stop_background("session", 1, &["shell".into()]),
            Err(ProviderError::UnsupportedCapability("stop_background"))
        ));
        assert!(calls().iter().all(|row| row["method"] == "handshake"));
    });
}

#[test]
fn per_task_stop_is_present_when_task_control_is_offered() {
    case("per_task_stop_is_present_when_task_control_is_offered", |provider| {
        open(provider);
        assert_eq!(
            calls().iter().find(|row| row["method"] == "create").unwrap()["perTaskStop"],
            true
        );
    });
}

#[test]
fn per_task_stop_is_absent_with_missing_task_control() {
    case("per_task_stop_is_absent_with_missing_task_control", |provider| {
        open(provider);
        assert_eq!(
            calls().iter().find(|row| row["method"] == "create").unwrap()["perTaskStop"],
            false
        );
    });
}

#[cfg(feature = "test-support")]
#[test]
fn per_task_stop_can_be_disabled_for_a_scripted_comparison() {
    case("per_task_stop_can_be_disabled_for_a_scripted_comparison", |provider| {
        provider.set_per_task_stop(false);
        open(provider);
        assert_eq!(
            calls().iter().find(|row| row["method"] == "create").unwrap()["perTaskStop"],
            false
        );
    });
}

#[cfg(feature = "test-support")]
#[test]
fn per_task_stop_is_present_with_the_test_override() {
    case("per_task_stop_is_present_with_the_test_override", |provider| {
        provider.set_per_task_stop(true);
        open(provider);
        assert_eq!(
            calls().iter().find(|row| row["method"] == "create").unwrap()["perTaskStop"],
            true
        );
        provider
            .resume_session(agent::ResumeSessionRequest {
                provider_session_id: "prior".into(),
                cwd: "/".into(),
                streaming: agent::StreamingPreference::Partial,
                project: agent::setting_sources::ProjectTrust::Untrusted,
                client_tools: None,
            })
            .unwrap();
        assert!(calls()
            .iter()
            .filter(|row| row["method"] == "create")
            .all(|row| row["perTaskStop"] == true));
    });
}

#[cfg(feature = "test-support")]
#[test]
fn the_test_override_does_not_override_a_missing_task_control_capability() {
    case(
        "the_test_override_does_not_override_a_missing_task_control_capability",
        |provider| {
            provider.set_per_task_stop(true);
            open(provider);
            assert_eq!(
                calls().iter().find(|row| row["method"] == "create").unwrap()["perTaskStop"],
                false
            );
        },
    );
}

#[test]
fn a_task_refusal_is_bounded_plain_text() {
    for code in [
        ProviderErrorCode::TaskNotFound,
        ProviderErrorCode::TaskOutputUnavailable,
        ProviderErrorCode::NoActiveTurn,
    ] {
        let text = agent::providers::claude_sidecar::task_refusal(ProviderError::Provider {
            code,
            message: "界".repeat(3000),
        });
        assert!(text.chars().count() <= 2000);
    }
}

fn record(value: Value) {
    let path = std::env::var_os("EITRI_TASK_CONTROL_LOG").unwrap();
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    file.write_all(&bytes).unwrap();
}

#[derive(Clone)]
struct Standin;
impl NamedService for Standin {
    const NAME: &'static str = "verdandi.claude.runtime.v1.RuntimeService";
}

async fn unary<Req, Resp, F, Fut>(req: http::Request<Body>, answer: F) -> http::Response<Body>
where
    Req: Message + Default + Send + 'static,
    Resp: Message + Send + 'static,
    F: FnOnce(Req) -> Fut + Send + 'static,
    Fut: Future<Output = Result<Resp, Status>> + Send + 'static,
{
    let answer = Mutex::new(Some(answer));
    let service = tower::service_fn(move |request: tonic::Request<Req>| {
        let reply = answer.lock().unwrap().take().unwrap()(request.into_inner());
        async move { reply.await.map(tonic::Response::new) }
    });
    Grpc::new(ProstCodec::<Resp, Req>::default()).unary(service, req).await
}

async fn hold_answer(task: &str) {
    if !task.starts_with("held-") {
        return;
    }
    let release = PathBuf::from(std::env::var_os("EITRI_TASK_CONTROL_LOG").unwrap()).with_extension("release");
    while !release.exists() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn failure(code: wire::ErrorCode) -> Status {
    Status::with_details(
        tonic::Code::FailedPrecondition,
        "task refusal",
        wire::ErrorDetail {
            code: code as i32,
            message: "refused by the stand-in".into(),
            ..Default::default()
        }
        .encode_to_vec()
        .into(),
    )
}

impl Service<http::Request<Body>> for Standin {
    type Response = http::Response<Body>;
    type Error = std::convert::Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        Box::pin(async move {
            let method = request.uri().path().rsplit('/').next().unwrap().to_owned();
            Ok(match method.as_str() {
                "Handshake" => unary(request, |_: wire::HandshakeRequest| async {
                    record(json!({"method": "handshake"}));
                    let missing = std::env::var("EITRI_TASK_CONTROL_CASE").unwrap().contains("missing_task");
                    let mut capabilities = vec!["create_session", "resume_session", "system_prompt_preset"];
                    if !missing { capabilities.extend(["task_control", "task_output"]); }
                    Ok(wire::HandshakeResponse { protocol_major: 3, protocol_minor: 22,
                        capabilities: capabilities.into_iter().map(str::to_owned).collect(), permission_modes: vec!["interactive".into()],
                        ..Default::default() })
                }).await,
                "CreateSession" => unary(request, |request: wire::CreateSessionRequest| async {
                    record(json!({"method": "create", "perTaskStop": request.policy.unwrap().per_task_stop}));
                    Ok(wire::CreateSessionResponse { session_id: "session".into(), ..Default::default() })
                }).await,
                "StopTask" => unary(request, |request: wire::StopTaskRequest| async move {
                    record(json!({"method": "stop", "session": request.session_id, "task": request.task_id, "command": request.command_id}));
                    hold_answer(&request.task_id).await;
                    if request.task_id == "gone" { return Err(failure(wire::ErrorCode::TaskNotFound)); }
                    Ok(wire::StopTaskResponse {})
                }).await,
                "GetTaskOutput" => unary(request, |request: wire::GetTaskOutputRequest| async move {
                    record(json!({"method": "output", "session": request.session_id, "task": request.task_id}));
                    hold_answer(&request.task_id).await;
                    match request.task_id.as_str() {
                        "gone" => Err(failure(wire::ErrorCode::TaskNotFound)),
                        "workflow" | "local-agent" => Err(failure(wire::ErrorCode::TaskOutputUnavailable)),
                        "oversized" => Ok(wire::GetTaskOutputResponse {output: [vec![b'x'; 9000], b"tail".to_vec()].concat(), total_bytes: 10000, truncated: false, offset: 0 }),
                        _ => Ok(wire::GetTaskOutputResponse { output: vec![0xA9, b't', b'a', b'i', b'l'], total_bytes: 9000, truncated: true, offset: 0 }),
                    }
                }).await,
                "CloseSession" => unary(request, |_: wire::CloseSessionRequest| async {
                    record(json!({"method": "close"}));
                    Ok(wire::CloseSessionResponse {})
                }).await,
                "WatchSessionEvents" => {
                    let service = tower::service_fn(|_: tonic::Request<wire::WatchSessionEventsRequest>| async {
                        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<wire::SessionEvent, Status>>();
                        tokio::spawn(async move { tx.closed().await; });
                        Ok::<_, Status>(tonic::Response::new(tonic::codegen::tokio_stream::wrappers::UnboundedReceiverStream::new(rx)))
                    });
                    Grpc::new(ProstCodec::<wire::SessionEvent, wire::WatchSessionEventsRequest>::default()).server_streaming(service, request).await
                }
                _ => Status::unimplemented(method).into_http(),
            })
        })
    }
}

#[test]
fn standin_server() {
    if std::env::var_os("EITRI_TASK_CONTROL_SERVER").is_none() {
        return;
    }
    agent::private_fs::write_private(&PathBuf::from(std::env::var_os("EITRI_TASK_CONTROL_LOG").unwrap()), b"").unwrap();
    std::thread::spawn(|| {
        let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
        std::process::exit(0);
    });
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let socket = std::env::var_os("VERDANDI_CLAUDE_SIDECAR_SOCKET").unwrap();
            let listener = tokio::net::UnixListener::bind(socket).expect("stand-in socket binds");
            tonic::transport::Server::builder()
                .add_service(Standin)
                .serve_with_incoming(UnixListenerStream::new(listener))
                .await
                .unwrap();
        });
}
