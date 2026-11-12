//! Detached sidecar test seams, using only the scripted stand-in; no real Claude CLI runs.
#![cfg(unix)]

use agent::providers::claude_sidecar::classify_watch_open_for_test;
use agent::{ProviderError, ProviderErrorCode};
use claude_runtime_protocol::v1::runtime_service_client::RuntimeServiceClient;
use claude_runtime_protocol::v1::session_event::Event;
use claude_runtime_protocol::v1::{
    CloseSessionRequest, CreateSessionRequest, ErrorCode, ErrorDetail, HandshakeRequest, ReplayStart, SendTurnRequest,
    SessionEvent, WatchSessionEventsRequest,
};
use hyper_util::rt::TokioIo;
use prost::Message;
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tonic::transport::{Channel, Endpoint};

/// A hang guard; waits end as soon as the child or RPC answers.
const HANG: Duration = Duration::from_secs(30);
const PARENT_WATCH: &str = "VERDANDI_CLAUDE_SIDECAR_PARENT_WATCH";

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let parent = Path::new(env!("CARGO_TARGET_TMPDIR"));
        std::fs::create_dir_all(parent).unwrap();
        let path = parent.join(format!("ss-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]));
        std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct StandIn {
    child: Child,
    socket: PathBuf,
}

impl StandIn {
    fn spawn(dir: &Scratch, configure: impl FnOnce(&mut Command)) -> Self {
        let socket = agent::socket_path::in_dir(&dir.0, "s.sock").unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_eitri-sidecar-standin"));
        command
            .env_clear()
            .env("VERDANDI_CLAUDE_SIDECAR_SOCKET", &socket)
            .env(PARENT_WATCH, "none")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        configure(&mut command);
        Self {
            child: command.spawn().unwrap(),
            socket,
        }
    }

    async fn client(&mut self) -> RuntimeServiceClient<Channel> {
        tokio::time::timeout(HANG, async {
            loop {
                assert!(
                    self.child.try_wait().unwrap().is_none(),
                    "stand-in exited before connecting"
                );
                let socket = self.socket.clone();
                let channel = Endpoint::from_static("http://[::1]:50051")
                    .connect_with_connector(tower::service_fn(move |_| {
                        let socket = socket.clone();
                        async move { tokio::net::UnixStream::connect(socket).await.map(TokioIo::new) }
                    }))
                    .await;
                if let Ok(channel) = channel {
                    let mut client = RuntimeServiceClient::new(channel);
                    client.handshake(HandshakeRequest::default()).await.unwrap();
                    return client;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("stand-in never served")
    }

    fn wait(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + HANG;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "stand-in never exited");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

impl Drop for StandIn {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn create(client: &mut RuntimeServiceClient<Channel>) -> String {
    tokio::time::timeout(
        HANG,
        client.create_session(CreateSessionRequest {
            cwd: "/srv/project".to_string(),
            ..Default::default()
        }),
    )
    .await
    .unwrap()
    .unwrap()
    .into_inner()
    .session_id
}

async fn watch(
    client: &mut RuntimeServiceClient<Channel>,
    session: &str,
    after: u64,
) -> Result<tonic::Streaming<SessionEvent>, tonic::Status> {
    tokio::time::timeout(
        HANG,
        client.watch_session_events(WatchSessionEventsRequest {
            session_id: session.to_string(),
            start: ReplayStart::AfterSequence as i32,
            after_sequence: Some(after),
        }),
    )
    .await
    .unwrap()
    .map(tonic::Response::into_inner)
}

async fn next(stream: &mut tonic::Streaming<SessionEvent>) -> SessionEvent {
    tokio::time::timeout(HANG, stream.message())
        .await
        .unwrap()
        .unwrap()
        .expect("the stream ended before the event")
}

async fn send(client: &mut RuntimeServiceClient<Channel>, session: &str, script: &str) {
    tokio::time::timeout(
        HANG,
        client.send_turn(SendTurnRequest {
            session_id: session.to_string(),
            text: script.to_string(),
            ..Default::default()
        }),
    )
    .await
    .unwrap()
    .unwrap();
}

#[tokio::test]
async fn parent_watch_none_outlives_stdin_eof() {
    for value in ["none", " none "] {
        let dir = Scratch::new();
        let mut child = StandIn::spawn(&dir, |command| {
            command.env(PARENT_WATCH, value).stdin(Stdio::piped());
        });
        drop(child.child.stdin.take());
        let mut client = child.client().await;
        let session = create(&mut client).await;
        let mut events = watch(&mut client, &session, 0).await.unwrap();
        send(&mut client, &session, "text m alive; end").await;
        loop {
            if matches!(next(&mut events).await.event, Some(Event::TurnCompleted(_))) {
                break;
            }
        }
        assert!(child.child.try_wait().unwrap().is_none());
    }
}

#[test]
fn parent_watch_defaults_to_stdin_after_trimming() {
    let dir = Scratch::new();
    for value in [
        None,
        Some("stdin"),
        Some(" stdin \t"),
        Some(""),
        Some(" \t\n "),
        Some("\u{feff}stdin\u{feff}"),
        Some("\u{feff}"),
    ] {
        let mut child = StandIn::spawn(&dir, |command| {
            command.env("EITRI_STANDIN_NEVER_BIND", "1");
            command.env_remove(PARENT_WATCH);
            if let Some(value) = value {
                command.env(PARENT_WATCH, value);
            }
        });
        assert!(child.wait().success());
    }
}

#[test]
fn parent_watch_bad_value_exits() {
    let dir = Scratch::new();
    for value in [
        "invalid-watch",
        "bad\nwatch",
        " bad-watch ",
        "bad\"watch",
        "bad\u{000b}watch",
        "\u{0085}",
    ] {
        let mut child = StandIn::spawn(&dir, |command| {
            command.env(PARENT_WATCH, value).stderr(Stdio::piped());
        });
        assert!(!child.wait().success());
        let mut stderr = String::new();
        child.child.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
        assert_eq!(stderr.lines().count(), 1);
        assert_eq!(
            stderr,
            format!(
                "{PARENT_WATCH}: expected \"stdin\" or \"none\", got {}\n",
                serde_json::to_string(value).unwrap()
            )
        );
    }
}

#[test]
fn record_env_writes_the_environment() {
    let dir = Scratch::new();
    let path = dir.0.join("environment");
    let socket = agent::socket_path::in_dir(&dir.0, "s.sock").unwrap();
    let entries = [
        (
            OsString::from("EITRI_STANDIN_RECORD_ENV"),
            path.clone().into_os_string(),
        ),
        (OsString::from(PARENT_WATCH), OsString::from("stdin")),
        (OsString::from("EITRI_STANDIN_NEVER_BIND"), OsString::from("1")),
        (OsString::from("PLAIN"), OsString::from("value=with\nnewlines")),
        (OsString::from("EMPTY"), OsString::new()),
        (
            OsString::from_vec(b"RAW_\xff".to_vec()),
            OsString::from_vec(vec![b'v', 0xfe]),
        ),
    ];
    let mut child = StandIn::spawn(&dir, |command| {
        command.env_clear().envs(entries.clone());
        // The environment snapshot is written before the stdin watcher starts.
        command
            .env("VERDANDI_CLAUDE_SIDECAR_SOCKET", &socket)
            .stdin(Stdio::piped());
    });
    let deadline = Instant::now() + HANG;
    let bytes = loop {
        match std::fs::read(&path) {
            Ok(bytes) => break bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                assert!(
                    child.child.try_wait().unwrap().is_none(),
                    "stand-in exited before recording"
                );
                assert!(Instant::now() < deadline, "stand-in never recorded its environment");
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("cannot read environment snapshot: {error}"),
        }
    };
    drop(child.child.stdin.take());
    assert!(child.wait().success());
    assert_eq!(bytes.last(), Some(&0));
    let mut actual: Vec<_> = bytes[..bytes.len() - 1]
        .split(|byte| *byte == 0)
        .map(Vec::from)
        .collect();
    let mut expected: Vec<_> = entries
        .into_iter()
        .chain([(
            OsString::from("VERDANDI_CLAUDE_SIDECAR_SOCKET"),
            socket.into_os_string(),
        )])
        .map(|(name, value)| {
            let mut entry = name.into_vec();
            entry.push(b'=');
            entry.extend(value.into_vec());
            entry
        })
        .collect();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
    assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
}

#[test]
fn record_create_flag_opens_only_the_winning_path() {
    let dir = Scratch::new();
    let env_record = dir.0.join("env-creates");
    let flag_record = dir.0.join("flag-creates");
    std::fs::write(&env_record, b"keep this record").unwrap();
    for env_path in [
        env_record.clone(),
        dir.0.join("absent/creates"),
        PathBuf::from("relative"),
    ] {
        let mut child = StandIn::spawn(&dir, |command| {
            command
                .env(PARENT_WATCH, "stdin")
                .env("EITRI_STANDIN_NEVER_BIND", "1")
                .env("EITRI_STANDIN_RECORD_CREATE", &env_path)
                .arg("--record-create")
                .arg(&flag_record);
        });
        assert!(child.wait().success());
        assert_eq!(std::fs::read(&env_record).unwrap(), b"keep this record");
        assert_eq!(std::fs::read(&flag_record).unwrap(), b"");
        assert!(!dir.0.join("absent").exists());
    }
}

#[tokio::test]
async fn record_create_from_env_matches_the_flag() {
    let mut records = Vec::new();
    for flag in [false, true] {
        let dir = Scratch::new();
        let record = dir.0.join("creates");
        let mut child = StandIn::spawn(&dir, |command| {
            if flag {
                command.arg("--record-create").arg(&record);
            } else {
                command.env("EITRI_STANDIN_RECORD_CREATE", &record);
            }
        });
        let mut client = child.client().await;
        for _ in 0..2 {
            create(&mut client).await;
        }
        let bytes = std::fs::read(&record).unwrap();
        let request = CreateSessionRequest {
            cwd: "/srv/project".to_string(),
            ..Default::default()
        }
        .encode_to_vec();
        let one_record = [(request.len() as u32).to_le_bytes().as_slice(), &request].concat();
        assert_eq!(bytes, one_record.repeat(2));
        assert_eq!(std::fs::metadata(record).unwrap().permissions().mode() & 0o777, 0o600);
        records.push(bytes);
    }
    assert_eq!(records[0], records[1]);
}

#[tokio::test]
async fn stderr_flood_keeps_serving() {
    const BYTES: usize = 4 * 1024 * 1024 + 17;
    for close_pipe in [false, true] {
        let dir = Scratch::new();
        let mut child = StandIn::spawn(&dir, |command| {
            command
                .env("EITRI_STANDIN_STDERR_FLOOD", BYTES.to_string())
                .stderr(Stdio::piped());
        });
        let mut client = child.client().await;
        // Leave the pipe unread while the gRPC service answers, so a full stderr cannot stall it.
        create(&mut client).await;
        let stderr = child.child.stderr.take().unwrap();
        if close_pipe {
            drop(stderr);
        } else {
            let read = tokio::task::spawn_blocking(move || {
                let mut bytes = Vec::new();
                stderr
                    .take((BYTES + b"stand-in sidecar: started\n".len()) as u64)
                    .read_to_end(&mut bytes)
                    .unwrap();
                bytes
            });
            let bytes = tokio::time::timeout(HANG, read).await.unwrap().unwrap();
            assert_eq!(
                bytes,
                [b"stand-in sidecar: started\n".as_slice(), &vec![b'x'; BYTES]].concat()
            );
        }
        let session = create(&mut client).await;
        let mut events = watch(&mut client, &session, 0).await.unwrap();
        send(&mut client, &session, "text m serving; end").await;
        loop {
            if matches!(next(&mut events).await.event, Some(Event::TurnCompleted(_))) {
                break;
            }
        }
        assert!(child.child.try_wait().unwrap().is_none());
    }
}

#[tokio::test]
async fn bounded_ring_refuses_like_the_real_sidecar() {
    let dir = Scratch::new();
    let mut child = StandIn::spawn(&dir, |command| {
        command.env("EITRI_STANDIN_RING", "3");
    });
    let mut client = child.client().await;
    let session = create(&mut client).await;
    let mut events = watch(&mut client, &session, 0).await.unwrap();
    send(&mut client, &session, "text a one; text b two; text c three; end").await;
    let mut all = Vec::new();
    loop {
        let event = next(&mut events).await;
        let completed = matches!(event.event, Some(Event::TurnCompleted(_)));
        all.push(event);
        if completed {
            break;
        }
    }
    assert_eq!(
        all.iter().map(|event| event.sequence).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5, 6]
    );
    for after in [0, 2] {
        let status = watch(&mut client, &session, after).await.unwrap_err();
        assert_eq!(status.code(), tonic::Code::OutOfRange);
        assert_eq!(
            ErrorDetail::decode(status.details()).unwrap().code(),
            ErrorCode::EventGap
        );
        let (error, fatal) = classify_watch_open_for_test(status);
        assert!(matches!(
            error,
            ProviderError::Provider {
                code: ProviderErrorCode::EventGap,
                ..
            }
        ));
        assert!(fatal, "the laptop must end the lost session instead of retrying");
    }
    let mut replay = watch(&mut client, &session, 3).await.unwrap();
    for expected in &all[3..] {
        assert_eq!(next(&mut replay).await, *expected);
    }
    let mut live = watch(&mut client, &session, 6).await.unwrap();
    send(&mut client, &session, "end").await;
    assert_eq!(next(&mut live).await.sequence, 7);
    assert_eq!(next(&mut live).await.sequence, 8);
    let other = create(&mut client).await;
    assert_eq!(
        next(&mut watch(&mut client, &other, 0).await.unwrap()).await.sequence,
        1
    );
}

#[tokio::test]
async fn cursor_in_future_is_invalid_argument() {
    let dir = Scratch::new();
    let mut child = StandIn::spawn(&dir, |_| {});
    let mut client = child.client().await;
    let session = create(&mut client).await;
    let status = watch(&mut client, &session, 2).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    let detail = ErrorDetail::decode(status.details()).unwrap();
    assert_eq!(detail.code(), ErrorCode::InvalidConfiguration);
    assert_eq!(detail.reason, "replay_cursor_in_future");
    let (error, fatal) = classify_watch_open_for_test(status);
    assert!(matches!(
        error,
        ProviderError::Provider {
            code: ProviderErrorCode::InvalidConfiguration,
            ..
        }
    ));
    assert!(fatal);
}

#[tokio::test]
async fn ask_then_end_leaves_the_request_unresolved() {
    let dir = Scratch::new();
    let mut child = StandIn::spawn(&dir, |_| {});
    let mut client = child.client().await;
    let session = create(&mut client).await;
    let mut events = watch(&mut client, &session, 0).await.unwrap();
    send(&mut client, &session, "ask-then-end pending").await;
    let mut seen = Vec::new();
    loop {
        let event = next(&mut events).await;
        let completed = matches!(event.event, Some(Event::TurnCompleted(_)));
        seen.push(event);
        if completed {
            break;
        }
    }
    assert!(matches!(&seen[2].event, Some(Event::PermissionRequested(ask)) if ask.permission_id == "pending"));
    assert!(matches!(&seen[3].event, Some(Event::TurnCompleted(_))));
    tokio::time::timeout(
        HANG,
        client.close_session(CloseSessionRequest {
            session_id: session.clone(),
            ..Default::default()
        }),
    )
    .await
    .unwrap()
    .unwrap();
    seen.push(next(&mut events).await);
    assert!(tokio::time::timeout(HANG, events.message())
        .await
        .unwrap()
        .unwrap()
        .is_none());
    assert!(!seen
        .iter()
        .any(|event| matches!(event.event, Some(Event::PermissionResolved(_)))));
    let mut replay = watch(&mut client, &session, 0).await.unwrap();
    for expected in seen {
        assert_eq!(next(&mut replay).await, expected);
    }
}
