#![allow(dead_code)]

#[path = "../src/audit.rs"]
mod audit;
#[path = "../src/daemon.rs"]
mod daemon;
#[path = "../src/peer.rs"]
mod peer;
#[path = "../src/run_dir.rs"]
mod run_dir;

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use agent::private_fs;
use claude_runtime_protocol::v1::{runtime_service_client::RuntimeServiceClient, session_event::Event, *};
use eitri_server_proto::{helper_client::HelperClient, *};
use prost::Message;
use sha2::{Digest, Sha256};
use tonic::{transport::Channel, Code};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REVISION: &str = env!("EITRI_BUILD_COMMIT");
fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}

/// The installation the fixture's daemon is taken to run from: a test binary runs from no installation of its own.
fn installation(paths: &run_dir::Paths) -> PathBuf {
    paths.data.join(format!("{VERSION}-{}", run_dir::rev7(REVISION)))
}

struct Fixture {
    root: PathBuf,
    paths: run_dir::Paths,
    env: BTreeMap<OsString, OsString>,
    daemon: Option<Child>,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        // Keep socket fixtures short enough for the Unix socket name limit.
        let root = PathBuf::from(format!(
            "/tmp/sv-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        private_fs::create_private_dir_all(&root, &root).unwrap();
        let paths = run_dir::Paths::new(VERSION, &root.join("d"), &root.join("s"), &std::env::temp_dir()).unwrap();
        paths.prepare().unwrap();
        let binary = std::env::var_os("EITRI_TEST_STANDIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("CARGO_TARGET_DIR").expect("CARGO_TARGET_DIR is required"))
                    .join("debug/eitri-sidecar-standin")
            });
        assert!(
            binary.is_file(),
            "build agent's eitri-sidecar-standin with test-support first: {}",
            binary.display()
        );
        let installed = installation(&paths).join("sidecar/test-rev");
        private_fs::create_private_dir_all(&installed, &paths.data).unwrap();
        daemon::test_exec::install_executable(&binary, &installed.join("verdandi-claude-sidecar"));
        let mut env = BTreeMap::from([
            (OsString::from("HOME"), root.clone().into_os_string()),
            ("PATH".into(), "/usr/bin:/bin".into()),
            (
                "EITRI_STANDIN_RECORD_ENV".into(),
                root.join("record-env").into_os_string(),
            ),
        ]);
        for key in [
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "DBUS_SESSION_BUS_ADDRESS",
            "DBUS_SYSTEM_BUS_ADDRESS",
        ] {
            env.insert(key.into(), "disabled:".into());
        }
        Self {
            root,
            paths,
            env,
            daemon: None,
        }
    }

    fn request(&self, command_id: &str) -> StartSidecarRequest {
        let binary = installation(&self.paths).join("sidecar/test-rev/verdandi-claude-sidecar");
        StartSidecarRequest {
            root: self.root.to_str().unwrap().to_owned(),
            account: "example-account".into(),
            env: Default::default(),
            command_id: command_id.into(),
            sidecar_revision: "test-rev".into(),
            sidecar_sha256: format!("{:x}", Sha256::digest(std::fs::read(binary).unwrap())),
        }
    }

    async fn start(&mut self) -> HelperClient<Channel> {
        let mut profile = Vec::new();
        for (key, value) in &self.env {
            profile.extend_from_slice(b"export '");
            profile.extend_from_slice(key.as_encoded_bytes());
            profile.push(b'=');
            for byte in value.as_encoded_bytes() {
                if *byte == b'\'' {
                    profile.extend_from_slice(b"'\\''");
                } else {
                    profile.push(*byte);
                }
            }
            profile.extend_from_slice(b"'\n");
        }
        private_fs::write_private(&self.root.join("login-profile"), &profile).unwrap();
        let shell = self.root.join("login-shell");
        let mut writer = Command::new("/bin/sh");
        writer
            .args(["-c", "cat >\"$1\" && chmod 700 \"$1\"", "writer"])
            .arg(&shell)
            .stdin(Stdio::piped());
        let mut writer = writer.spawn().unwrap();
        use std::io::Write;
        writer
            .stdin
            .take()
            .unwrap()
            .write_all(b"#!/bin/sh\nprintf . >>\"$HOME/login-calls\"\n. \"$HOME/login-profile\"\nif [ -n \"$EITRI_TEST_LOGIN_HOLD\" ]; then while [ -e \"$EITRI_TEST_LOGIN_HOLD\" ]; do /bin/sleep 0.01; done; fi\n/usr/bin/env -0 >\"$HOME/captured-env\"\nprintf 'Profile banner\\n'\neval \"$2\"\n")
            .unwrap();
        assert!(writer.wait().unwrap().success());
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "fixture_daemon_entry", "--nocapture"])
            .env_clear()
            .env("EITRI_TEST_DAEMON", "1")
            .env("HOME", &self.root)
            .env("SHELL", shell)
            .env("XDG_DATA_HOME", self.root.join("d"))
            .env("XDG_STATE_HOME", self.root.join("s"))
            .env("PATH", "/usr/bin:/bin")
            .env("EITRI_DAEMON_ONLY", "must-not-leak")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        self.daemon = Some(child);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                self.daemon.as_mut().unwrap().try_wait().unwrap().is_none(),
                "daemon exited during startup"
            );
            if let Ok(channel) = helper_channel(self.paths.socket().unwrap()).await {
                let mut client = HelperClient::new(channel);
                if client
                    .hello(HelloRequest {
                        protocol_major: PROTOCOL_MAJOR,
                        protocol_minor: PROTOCOL_MINOR,
                        client_version: VERSION.into(),
                    })
                    .await
                    .is_ok()
                {
                    return HelperClient::new(connect_channel(&self.root).await);
                }
            }
            assert!(tokio::time::Instant::now() < deadline, "daemon did not become ready");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn crash(&mut self) {
        let mut child = self.daemon.take().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        // A killed daemon cannot run its ordinary socket cleanup.
        std::fs::remove_file(self.paths.socket().unwrap()).unwrap();
    }

    fn recorded_env(&self) -> BTreeMap<OsString, OsString> {
        std::fs::read(self.root.join("record-env"))
            .unwrap()
            .split(|b| *b == 0)
            .filter(|v| !v.is_empty())
            .map(|v| {
                let p = v.iter().position(|b| *b == b'=').unwrap();
                (
                    OsString::from_vec(v[..p].to_vec()),
                    OsString::from_vec(v[p + 1..].to_vec()),
                )
            })
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Pin cleanup to each recorded start time rather than signalling a reused pid.
        if let Ok(files) = std::fs::read_dir(&self.paths.run) {
            for file in files
                .flatten()
                .filter(|f| f.path().extension() == Some(OsStr::new("pid")))
            {
                if let Ok(bytes) = std::fs::read(file.path()) {
                    if let Ok(record) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                        if let (Some(pid), Some(start)) = (record["pid"].as_u64(), record["start_time"].as_u64()) {
                            if daemon::sidecars::start_time(pid as u32).ok() == Some(start) {
                                unsafe {
                                    libc::kill(pid as i32, libc::SIGKILL);
                                }
                            }
                        }
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn parse_env(bytes: &[u8]) -> BTreeMap<OsString, OsString> {
    bytes
        .split(|b| *b == 0)
        .filter(|v| !v.is_empty())
        .map(|v| {
            let p = v.iter().position(|b| *b == b'=').unwrap();
            (
                OsString::from_vec(v[..p].to_vec()),
                OsString::from_vec(v[p + 1..].to_vec()),
            )
        })
        .collect()
}

#[test]
fn fixture_daemon_entry() {
    if std::env::var_os("EITRI_TEST_DAEMON").is_none() {
        return;
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut paths = run_dir::Paths::from_env(VERSION).unwrap();
        paths.install = Some(installation(&paths));
        let mut baseline = daemon::login_env::minimal_environment().unwrap();
        for key in ["HOME", "SHELL"] {
            baseline.insert(key.into(), std::env::var_os(key).unwrap());
        }
        daemon::run_with_environment(
            paths,
            Duration::from_secs(600),
            daemon::OwnedSidecars::default(),
            async move { Ok(daemon::login_env::capture_with(baseline).await) },
        )
        .await
        .unwrap();
    });
}

/// A byte relay standing where a sidecar's socket is expected: it forwards every connection to a real sidecar
/// and records the bytes it received from the helper, one file per connection.
#[test]
fn fixture_relay_entry() {
    let (Some(listen), Some(upstream), Some(record)) = (
        std::env::var_os("EITRI_TEST_RELAY_LISTEN"),
        std::env::var_os("EITRI_TEST_RELAY_UPSTREAM"),
        std::env::var_os("EITRI_TEST_RELAY_RECORD"),
    ) else {
        return;
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::UnixListener::bind(&listen).unwrap();
        std::fs::write(PathBuf::from(&record).join("listening"), b"").unwrap();
        for connection in 0.. {
            let (mut inbound, _) = listener.accept().await.unwrap();
            let upstream = upstream.clone();
            let file = PathBuf::from(&record).join(format!("conn-{connection}"));
            tokio::spawn(async move {
                let outbound = tokio::net::UnixStream::connect(&upstream).await.unwrap();
                let (mut from_sidecar, mut to_sidecar) = outbound.into_split();
                let (mut from_helper, mut to_helper) = inbound.split();
                let back = tokio::io::copy(&mut from_sidecar, &mut to_helper);
                let forth = async {
                    let mut buffer = vec![0; 64 * 1024];
                    loop {
                        let read = from_helper.read(&mut buffer).await?;
                        if read == 0 {
                            return to_sidecar.shutdown().await;
                        }
                        let mut log = std::fs::OpenOptions::new().create(true).append(true).open(&file)?;
                        std::io::Write::write_all(&mut log, &buffer[..read])?;
                        to_sidecar.write_all(&buffer[..read]).await?;
                    }
                };
                let _ = tokio::join!(back, forth);
            });
        }
    });
}

/// The DATA payload of every HTTP/2 stream in one recorded client-to-server byte stream.
fn http2_bodies(mut bytes: &[u8]) -> BTreeMap<u32, Vec<u8>> {
    const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
    bytes = bytes.strip_prefix(PREFACE).expect("an HTTP/2 client preface");
    let mut bodies = BTreeMap::<u32, Vec<u8>>::new();
    while bytes.len() >= 9 {
        let length = u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]) as usize;
        let (kind, flags) = (bytes[3], bytes[4]);
        let stream = u32::from_be_bytes(bytes[5..9].try_into().unwrap()) & 0x7fff_ffff;
        if bytes.len() < 9 + length {
            break;
        }
        let mut payload = &bytes[9..9 + length];
        if kind == 0 {
            if flags & 0x8 != 0 {
                let pad = payload[0] as usize;
                payload = &payload[1..payload.len() - pad];
            }
            bodies.entry(stream).or_default().extend_from_slice(payload);
        }
        bytes = &bytes[9 + length..];
    }
    bodies
}

/// A gRPC codec that sends and receives message bytes exactly as given.
#[derive(Default)]
struct Raw;
struct RawCoder;
impl tonic::codec::Codec for Raw {
    type Encode = bytes::Bytes;
    type Decode = bytes::Bytes;
    type Encoder = RawCoder;
    type Decoder = RawCoder;
    fn encoder(&mut self) -> RawCoder {
        RawCoder
    }
    fn decoder(&mut self) -> RawCoder {
        RawCoder
    }
}
impl tonic::codec::Encoder for RawCoder {
    type Item = bytes::Bytes;
    type Error = tonic::Status;
    fn encode(&mut self, item: bytes::Bytes, dst: &mut tonic::codec::EncodeBuf<'_>) -> Result<(), tonic::Status> {
        bytes::BufMut::put(dst, item);
        Ok(())
    }
}
impl tonic::codec::Decoder for RawCoder {
    type Item = bytes::Bytes;
    type Error = tonic::Status;
    fn decode(&mut self, src: &mut tonic::codec::DecodeBuf<'_>) -> Result<Option<bytes::Bytes>, tonic::Status> {
        let remaining = bytes::Buf::remaining(src);
        Ok(Some(bytes::Buf::copy_to_bytes(src, remaining)))
    }
}

fn journal_dir(f: &Fixture, sidecar: &str, session: &str) -> PathBuf {
    let name = format!("{:x}", Sha256::digest(serde_json::to_vec(&(sidecar, session)).unwrap()));
    f.paths.state.join("journal").join(name)
}

/// The refusal a laptop treats as its session being gone for good.
fn assert_gone(status: &tonic::Status) {
    assert_eq!(status.code(), Code::NotFound, "{status:?}");
    let detail = ErrorDetail::decode(status.details()).expect("an ErrorDetail");
    assert_eq!(detail.code, ErrorCode::SessionNotFound as i32);
    assert_eq!(detail.reason, "");
}

/// What a watch from `after` ends with, whether refused at once or after the events it still had.
async fn watch_end(
    runtime: &mut RuntimeServiceClient<Channel>,
    sidecar: &str,
    session: &str,
    after: u64,
) -> (Vec<SessionEvent>, tonic::Status) {
    let opened = runtime
        .watch_session_events(named(
            sidecar,
            WatchSessionEventsRequest {
                session_id: session.into(),
                start: ReplayStart::AfterSequence as i32,
                after_sequence: Some(after),
            },
        ))
        .await;
    let mut stream = match opened {
        Err(status) => return (Vec::new(), status),
        Ok(response) => response.into_inner(),
    };
    let mut events = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(30), stream.message())
            .await
            .expect("watch end hang guard")
        {
            Ok(Some(event)) => events.push(event),
            Ok(None) => panic!("the watch ended without a status"),
            Err(status) => return (events, status),
        }
    }
}

fn audited(f: &Fixture, event: &str) -> usize {
    std::fs::read_to_string(f.paths.state.join("audit.log"))
        .unwrap_or_default()
        .lines()
        .filter(|line| line.split(' ').nth(1) == Some(event))
        .count()
}

async fn helper_channel(socket: PathBuf) -> Result<Channel, tonic::transport::Error> {
    tonic::transport::Endpoint::from_static("http://localhost")
        .connect_with_connector(tower::service_fn(move |_| {
            let socket = socket.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(socket).await?;
                peer::check(&stream)?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
}

struct Pipe {
    child: tokio::process::Child,
    input: tokio::process::ChildStdin,
    output: tokio::process::ChildStdout,
}
impl tokio::io::AsyncRead for Pipe {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.output).poll_read(cx, buf)
    }
}
impl tokio::io::AsyncWrite for Pipe {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.input).poll_write(cx, bytes)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.input).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.input).poll_shutdown(cx)
    }
}

async fn connect_channel(root: &std::path::Path) -> Channel {
    let root = root.to_owned();
    tonic::transport::Endpoint::from_static("http://localhost")
        .connect_with_connector(tower::service_fn(move |_| {
            let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_eitri-server"))
                .arg("connect")
                .env_clear()
                .env("HOME", &root)
                .env("XDG_DATA_HOME", root.join("d"))
                .env("XDG_STATE_HOME", root.join("s"))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let input = child.stdin.take().unwrap();
            let output = child.stdout.take().unwrap();
            async move { Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(Pipe { child, input, output })) }
        }))
        .await
        .unwrap()
}

fn named<T>(sidecar: &str, message: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request
        .metadata_mut()
        .insert(SIDECAR_METADATA, sidecar.parse().unwrap());
    request
}

async fn start(f: &mut Fixture) -> (HelperClient<Channel>, RuntimeServiceClient<Channel>, String, String) {
    let mut helper = f.start().await;
    let sidecar = helper
        .start_sidecar(f.request("serve"))
        .await
        .unwrap()
        .into_inner()
        .sidecar_id;
    let mut runtime = RuntimeServiceClient::new(connect_channel(&f.root).await);
    let session = runtime
        .create_session(named(
            &sidecar,
            CreateSessionRequest {
                cwd: "/srv/project".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .session_id;
    (helper, runtime, sidecar, session)
}

async fn watch(
    runtime: &mut RuntimeServiceClient<Channel>,
    sidecar: &str,
    session: &str,
    after: u64,
) -> tonic::Response<tonic::Streaming<SessionEvent>> {
    runtime
        .watch_session_events(named(
            sidecar,
            WatchSessionEventsRequest {
                session_id: session.into(),
                start: ReplayStart::AfterSequence as i32,
                after_sequence: Some(after),
            },
        ))
        .await
        .unwrap()
}

async fn send(runtime: &mut RuntimeServiceClient<Channel>, sidecar: &str, session: &str, script: &str) {
    runtime
        .send_turn(named(
            sidecar,
            SendTurnRequest {
                session_id: session.into(),
                text: script.into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
}

async fn next(stream: &mut tonic::Streaming<SessionEvent>) -> SessionEvent {
    tokio::time::timeout(Duration::from_secs(30), stream.message())
        .await
        .expect("event hang guard")
        .unwrap()
        .expect("event")
}

async fn through_end(stream: &mut tonic::Streaming<SessionEvent>) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    loop {
        let event = next(stream).await;
        let end = matches!(event.event, Some(Event::TurnCompleted(_)));
        events.push(event);
        if end {
            return events;
        }
    }
}

async fn head(helper: &mut HelperClient<Channel>, sidecar: &str, session: &str, at_least: u64) -> u64 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let listed = helper.list_sidecars(ListSidecarsRequest {}).await.unwrap().into_inner();
        if let Some(session) = listed
            .sidecars
            .iter()
            .find(|s| s.sidecar_id == sidecar)
            .and_then(|s| s.sessions.iter().find(|s| s.session_id == session))
        {
            if session.journal_head >= at_least {
                return session.journal_head;
            }
        }
        assert!(tokio::time::Instant::now() < deadline, "journal head hang guard");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn pending(helper: &mut HelperClient<Channel>, sidecar: &str, session: &str) -> Vec<PendingPermission> {
    helper
        .pending(PendingRequest {
            sidecar_id: sidecar.into(),
            session_id: session.into(),
        })
        .await
        .unwrap()
        .into_inner()
        .requests
}

#[tokio::test]
async fn calls_reach_the_named_sidecar_byte_for_byte() {
    let mut f = Fixture::new();
    let record = f.root.join("creates");
    f.env
        .insert("EITRI_STANDIN_RECORD_CREATE".into(), record.clone().into_os_string());
    let mut helper = f.start().await;
    let first = helper
        .start_sidecar(f.request("first"))
        .await
        .unwrap()
        .into_inner()
        .sidecar_id;
    let first_record = f.root.join("first-creates");
    std::fs::rename(&record, &first_record).unwrap();
    let second = helper
        .start_sidecar(f.request("second"))
        .await
        .unwrap()
        .into_inner()
        .sidecar_id;
    let mut runtime = RuntimeServiceClient::new(connect_channel(&f.root).await);
    let requests = [
        CreateSessionRequest {
            cwd: "/srv/project".into(),
            resume_provider_session_id: Some("previous-session".into()),
            ..Default::default()
        },
        CreateSessionRequest {
            cwd: "/srv/project/other".into(),
            ..Default::default()
        },
    ];
    for ((sidecar, request), record) in [&first, &second].into_iter().zip(requests).zip([first_record, record]) {
        let bytes = request.encode_to_vec();
        let mut expected = (bytes.len() as u32).to_le_bytes().to_vec();
        expected.extend_from_slice(&bytes);
        runtime.create_session(named(sidecar, request)).await.unwrap();
        assert_eq!(std::fs::read(record).unwrap(), expected);
    }

    // The bytes a sidecar receives are the client's own: a relay in a sidecar's place records them, and the
    // request carries a field no build knows, which any decode and re-encode on the way would drop.
    let relay_id = "0123456789abcdef";
    let relay_socket = agent::socket_path::in_dir(&f.paths.run, &format!("sc-{relay_id}.sock")).unwrap();
    let relayed = f.root.join("relayed");
    private_fs::create_private_dir_all(&relayed, &f.root).unwrap();
    f.crash();
    drop(runtime);
    drop(helper);
    let mut relay = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "fixture_relay_entry", "--nocapture"])
        .env_clear()
        .env("EITRI_TEST_RELAY_LISTEN", &relay_socket)
        .env(
            "EITRI_TEST_RELAY_UPSTREAM",
            agent::socket_path::in_dir(&f.paths.run, &format!("sc-{first}.sock")).unwrap(),
        )
        .env("EITRI_TEST_RELAY_RECORD", &relayed)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !relayed.join("listening").exists() {
        assert!(std::time::Instant::now() < deadline, "relay did not listen");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let record = serde_json::json!({
        "pid": relay.id(),
        "start_time": daemon::sidecars::start_time(relay.id()).unwrap(),
        "socket": relay_socket,
        "sidecar_id": relay_id,
        "root": f.root,
        "account": "example-account",
        "command_id": "relay",
        "started_unix_ms": 1,
        "ready": true,
    });
    private_fs::write_private(
        &f.paths.metadata_run().join(format!("sc-{relay_id}.pid")),
        &serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
    let mut helper = f.start().await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !helper
        .list_sidecars(ListSidecarsRequest {})
        .await
        .unwrap()
        .into_inner()
        .sidecars
        .iter()
        .any(|sidecar| sidecar.sidecar_id == relay_id && sidecar.handshake)
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "relay sidecar never became ready"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut sent = CreateSessionRequest {
        cwd: "/srv/project/relayed".into(),
        ..Default::default()
    }
    .encode_to_vec();
    prost::encoding::string::encode(999, &"kept as sent".to_owned(), &mut sent);
    let mut grpc = tonic::client::Grpc::new(connect_channel(&f.root).await);
    grpc.ready().await.unwrap();
    let mut request = tonic::Request::new(bytes::Bytes::from(sent.clone()));
    request
        .metadata_mut()
        .insert(SIDECAR_METADATA, relay_id.parse().unwrap());
    let created = grpc
        .unary(
            request,
            http::uri::PathAndQuery::from_static("/verdandi.claude.runtime.v1.RuntimeService/CreateSession"),
            Raw,
        )
        .await
        .unwrap()
        .into_inner();
    assert!(!CreateSessionResponse::decode(created).unwrap().session_id.is_empty());
    let mut framed = vec![0];
    framed.extend((sent.len() as u32).to_be_bytes());
    framed.extend(&sent);
    let received = std::fs::read_dir(&relayed)
        .unwrap()
        .flatten()
        .filter(|file| file.file_name().to_string_lossy().starts_with("conn-"))
        .flat_map(|file| http2_bodies(&std::fs::read(file.path()).unwrap()).into_values())
        .collect::<Vec<_>>();
    assert!(
        received.contains(&framed),
        "the sidecar received other bytes than the client sent: {received:?}"
    );
    let _ = relay.kill();
    let _ = relay.wait();
}

#[tokio::test]
async fn unknown_sidecar_is_not_found() {
    let mut f = Fixture::new();
    let _helper = f.start().await;
    let mut runtime = RuntimeServiceClient::new(connect_channel(&f.root).await);
    let error = runtime
        .create_session(named("missing", CreateSessionRequest::default()))
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::NotFound);
    assert_eq!(error.message(), "no such sidecar on this server");
}

#[tokio::test]
async fn watch_from_any_cursor_after_the_turn_matches_the_live_stream() {
    let mut f = Fixture::new();
    let (_helper, mut runtime, sidecar, session) = start(&mut f).await;
    let mut live = watch(&mut runtime, &sidecar, &session, 0).await.into_inner();
    send(&mut runtime, &sidecar, &session, "text m one; text m two; end").await;
    let events = through_end(&mut live).await;
    for cursor in 0..events.last().unwrap().sequence {
        let mut replay = watch(&mut runtime, &sidecar, &session, cursor).await.into_inner();
        for expected in events.iter().filter(|e| e.sequence > cursor) {
            assert_eq!(&next(&mut replay).await, expected);
        }
    }
}

#[tokio::test]
async fn replay_hands_over_to_live_without_gap_or_duplicate() {
    let mut f = Fixture::new();
    let (_helper, mut runtime, sidecar, session) = start(&mut f).await;
    let mut live = watch(&mut runtime, &sidecar, &session, 0).await.into_inner();
    // Several replay batches of history before the live boundary.
    let payload = "x".repeat(1024);
    let script = std::iter::repeat_n(format!("text m {payload}"), 200)
        .chain(["end".into()])
        .collect::<Vec<_>>()
        .join(";");
    send(&mut runtime, &sidecar, &session, &script).await;
    let mut expected = through_end(&mut live).await;
    let mut replay = watch(&mut runtime, &sidecar, &session, 0).await.into_inner();
    for event in &expected[..10] {
        assert_eq!(&next(&mut replay).await, event);
    }
    // The next turn starts while the replay is still in the journal, then the stand-in holds its next event
    // until the replay has caught up with everything recorded.
    let gate = f.root.join("gate");
    private_fs::create_private_dir_all(&gate, &f.root).unwrap();
    send(
        &mut runtime,
        &sidecar,
        &session,
        &format!("text m during; gate {}; text m after; end", gate.display()),
    )
    .await;
    loop {
        let event = next(&mut live).await;
        let during = matches!(&event.event, Some(Event::TextDelta(delta)) if delta.text == "during");
        expected.push(event);
        if during {
            break;
        }
    }
    for event in &expected[10..] {
        assert_eq!(&next(&mut replay).await, event);
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(200), replay.message())
            .await
            .is_err(),
        "the replay must wait at the live boundary"
    );
    std::fs::write(gate.join("release"), b"").unwrap();
    for event in through_end(&mut live).await {
        assert_eq!(next(&mut replay).await, event);
    }
}

#[tokio::test]
async fn pending_request_is_served_after_reattach_and_answered_once() {
    let mut f = Fixture::new();
    f.env.insert("EITRI_STANDIN_UNKNOWN_WIRE".into(), "1".into());
    let (mut helper, mut runtime, sidecar, session) = start(&mut f).await;
    let mut live = watch(&mut runtime, &sidecar, &session, 0).await.into_inner();
    send(&mut runtime, &sidecar, &session, "ask permission; text m done; end").await;
    let requested = loop {
        let event = next(&mut live).await;
        if matches!(event.event, Some(Event::PermissionRequested(_))) {
            break event;
        }
    };
    drop(live);
    drop(runtime);
    let mut runtime = RuntimeServiceClient::new(connect_channel(&f.root).await);
    let mut replay = watch(&mut runtime, &sidecar, &session, requested.sequence - 1)
        .await
        .into_inner();
    assert_eq!(next(&mut replay).await, requested);
    let requests = pending(&mut helper, &sidecar, &session).await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].sequence, requested.sequence);
    assert_eq!(SessionEvent::decode(requests[0].event.as_slice()).unwrap(), requested);
    assert!(
        requests[0].event.len() > requested.encoded_len(),
        "unknown fields retained"
    );
    let request = ResolvePermissionRequest {
        session_id: session.clone(),
        permission_id: "permission".into(),
        ..Default::default()
    };
    runtime
        .resolve_permission(named(&sidecar, request.clone()))
        .await
        .unwrap();
    let events = through_end(&mut replay).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, Some(Event::PermissionResolved(_))))
            .count(),
        1
    );
    assert!(pending(&mut helper, &sidecar, &session).await.is_empty());
    // The stand-in's duplicate-answer refusal must pass through unchanged.
    let refused = runtime.resolve_permission(named(&sidecar, request)).await.unwrap_err();
    assert_eq!(refused.code(), Code::NotFound);
    assert_eq!(refused.message(), "no such permission");
}

#[tokio::test]
async fn pending_is_cleared_at_turn_end_and_session_close() {
    let mut f = Fixture::new();
    let (mut helper, mut runtime, sidecar, session) = start(&mut f).await;
    let mut live = watch(&mut runtime, &sidecar, &session, 0).await.into_inner();
    send(&mut runtime, &sidecar, &session, "ask-then-end unresolved").await;
    through_end(&mut live).await;
    assert!(pending(&mut helper, &sidecar, &session).await.is_empty());
    send(&mut runtime, &sidecar, &session, "ask closing").await;
    loop {
        if matches!(next(&mut live).await.event, Some(Event::PermissionRequested(_))) {
            break;
        }
    }
    assert_eq!(pending(&mut helper, &sidecar, &session).await.len(), 1);
    runtime
        .close_session(named(
            &sidecar,
            CloseSessionRequest {
                session_id: session.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    loop {
        if matches!(next(&mut live).await.event, Some(Event::SessionClosed(_))) {
            break;
        }
    }
    assert!(pending(&mut helper, &sidecar, &session).await.is_empty());
}

#[tokio::test]
async fn journal_head_metadata_is_sent_first() {
    let mut f = Fixture::new();
    let (mut helper, mut runtime, sidecar, session) = start(&mut f).await;
    let current = head(&mut helper, &sidecar, &session, 1).await;
    let response = watch(&mut runtime, &sidecar, &session, current).await;
    assert_eq!(
        response
            .metadata()
            .get(JOURNAL_HEAD_METADATA)
            .unwrap()
            .to_str()
            .unwrap(),
        current.to_string()
    );
    let mut stream = response.into_inner();
    send(&mut runtime, &sidecar, &session, "end").await;
    assert_eq!(next(&mut stream).await.sequence, current + 1);
}

#[tokio::test]
async fn daemon_restart_resumes_watching_from_the_journal_head() {
    let mut f = Fixture::new();
    let (mut helper, mut runtime, sidecar, session) = start(&mut f).await;
    let mut live = watch(&mut runtime, &sidecar, &session, 0).await.into_inner();
    send(&mut runtime, &sidecar, &session, "text m before; end").await;
    let events = through_end(&mut live).await;
    let cursor = events.last().unwrap().sequence;
    f.crash();
    drop(runtime);
    drop(helper);
    drop(live);
    helper = f.start().await;
    assert_eq!(head(&mut helper, &sidecar, &session, cursor).await, cursor);
    runtime = RuntimeServiceClient::new(connect_channel(&f.root).await);
    let mut stream = watch(&mut runtime, &sidecar, &session, cursor).await.into_inner();
    send(&mut runtime, &sidecar, &session, "text m after; end").await;
    let events = through_end(&mut stream).await;
    assert_eq!(events[0].sequence, cursor + 1);
    assert!(events.windows(2).all(|pair| pair[1].sequence == pair[0].sequence + 1));
}

#[tokio::test]
async fn a_gap_is_reported_never_skipped() {
    let mut f = Fixture::new();
    f.env.insert("EITRI_STANDIN_RING".into(), "2".into());
    let (mut helper, mut runtime, sidecar, session) = start(&mut f).await;
    let cursor = head(&mut helper, &sidecar, &session, 1).await;
    f.crash();
    drop(runtime);
    drop(helper);
    let socket = agent::socket_path::in_dir(&f.paths.run, &format!("sc-{sidecar}.sock")).unwrap();
    let mut direct = RuntimeServiceClient::new(helper_channel(socket).await.unwrap());
    let mut stream = watch(&mut direct, &sidecar, &session, cursor).await.into_inner();
    send(&mut direct, &sidecar, &session, "text m a; text m b; text m c; end").await;
    let events = through_end(&mut stream).await;
    let end = events.last().unwrap().sequence;
    helper = f.start().await;
    head(&mut helper, &sidecar, &session, end).await;
    runtime = RuntimeServiceClient::new(connect_channel(&f.root).await);
    let result = runtime
        .watch_session_events(named(
            &sidecar,
            WatchSessionEventsRequest {
                session_id: session.clone(),
                start: ReplayStart::AfterSequence as i32,
                after_sequence: Some(cursor),
            },
        ))
        .await;
    let status = match result {
        Err(status) => status,
        Ok(stream) => stream.into_inner().message().await.unwrap_err(),
    };
    assert_eq!(status.code(), Code::OutOfRange);
    let detail = ErrorDetail::decode(status.details()).unwrap();
    assert_eq!(detail.code, ErrorCode::EventGap as i32);
    assert_eq!(detail.reason, "");
}

fn rss(pid: u32) -> usize {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse::<usize>()
        .unwrap()
        * 1024
}

#[tokio::test]
async fn a_slow_reader_bounds_helper_memory() {
    let mut f = Fixture::new();
    let (mut helper, mut runtime, sidecar, session) = start(&mut f).await;
    let mut live = watch(&mut runtime, &sidecar, &session, 0).await.into_inner();
    let payload = "x".repeat(1024);
    let script = std::iter::repeat_n(format!("text m {payload}"), 512)
        .chain(["end".into()])
        .collect::<Vec<_>>()
        .join(";");
    let pid = f.daemon.as_ref().unwrap().id();
    send(&mut runtime, &sidecar, &session, &script).await;
    through_end(&mut live).await;
    let mut produced = 515;
    for _ in 0..98 {
        send(&mut runtime, &sidecar, &session, &script).await;
        produced += 514;
        head(&mut helper, &sidecar, &session, produced).await;
    }
    drop(live);
    let journal_bytes = produced as usize * payload.len();
    // A fast replay through several batches first, so first-use allocations are not counted below.
    let mut warm = watch(&mut runtime, &sidecar, &session, 0).await.into_inner();
    for _ in 0..256 {
        next(&mut warm).await;
    }
    drop(warm);
    // Calibration: the cost of one replay batch (64 KiB of journal) read as fast as the stream allows.
    let before = rss(pid);
    let mut one = watch(&mut runtime, &sidecar, &session, 0).await.into_inner();
    for _ in 0..64 {
        next(&mut one).await;
    }
    let batch = rss(pid).saturating_sub(before).max(64 * 1024);
    drop(one);
    // A replay that queued the journal would add all of it; the bound stays well below that.
    let bound = (batch * 64).min(journal_bytes / 4);
    let baseline = rss(pid);
    let mut slow = watch(&mut runtime, &sidecar, &session, 0).await.into_inner();
    // A 50,000-event journal must not be queued behind the slow reader. Sample two hundred reads at ten per second.
    for _ in 0..200 {
        next(&mut slow).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let grown = rss(pid).saturating_sub(baseline);
        assert!(
            grown < bound,
            "slow replay grew {grown} bytes, beyond {bound} (one batch measured {batch})"
        );
    }
}

#[tokio::test]
async fn a_sidecar_that_dies_ends_replays_with_the_session_gone() {
    let mut f = Fixture::new();
    let (mut helper, mut runtime, sidecar, session) = start(&mut f).await;
    let mut live = watch(&mut runtime, &sidecar, &session, 0).await.into_inner();
    send(&mut runtime, &sidecar, &session, "text m one; end").await;
    let events = through_end(&mut live).await;
    let pid = helper
        .list_sidecars(ListSidecarsRequest {})
        .await
        .unwrap()
        .into_inner()
        .sidecars
        .iter()
        .find(|listed| listed.sidecar_id == sidecar)
        .unwrap()
        .pid;
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
    // The open stream ends with the refusal, and a new one gets everything recorded first.
    let status = tokio::time::timeout(Duration::from_secs(30), live.message())
        .await
        .expect("the open watch must end")
        .unwrap_err();
    assert_gone(&status);
    let (replayed, status) = watch_end(&mut runtime, &sidecar, &session, 0).await;
    assert_eq!(replayed, events);
    assert_gone(&status);
    // With nothing left to send, the refusal comes at once.
    let last = events.last().unwrap().sequence;
    let (replayed, status) = watch_end(&mut runtime, &sidecar, &session, last).await;
    assert!(replayed.is_empty());
    assert_gone(&status);
}

#[tokio::test]
async fn a_session_that_cannot_be_observed_is_retryable_until_maintenance_adopts_it() {
    let mut f = Fixture::new();
    let mut helper = f.start().await;
    let sidecar = helper
        .start_sidecar(f.request("serve"))
        .await
        .unwrap()
        .into_inner()
        .sidecar_id;
    let mut runtime = RuntimeServiceClient::new(connect_channel(&f.root).await);
    // A file where the pending directory belongs: no pending record can be opened.
    let pending_dir = f.paths.state.join("pending");
    let aside = f.root.join("pending-aside");
    std::fs::rename(&pending_dir, &aside).unwrap();
    private_fs::write_private(&pending_dir, b"").unwrap();
    let session = runtime
        .create_session(named(
            &sidecar,
            CreateSessionRequest {
                cwd: "/srv/project".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .session_id;
    let (replayed, status) = watch_end(&mut runtime, &sidecar, &session, 0).await;
    assert!(replayed.is_empty());
    assert_eq!(status.code(), Code::Unavailable, "{status:?}");
    assert!(
        status.message().contains("cannot follow this session yet"),
        "{status:?}"
    );
    let status = helper
        .pending(PendingRequest {
            sidecar_id: sidecar.clone(),
            session_id: session.clone(),
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::Unavailable, "{status:?}");
    assert!(
        status.message().contains("cannot follow this session yet"),
        "{status:?}"
    );
    assert!(audited(&f, "watch-observe") >= 1);
    // The identity was kept, so maintenance observes the session once its pending record can be opened.
    assert!(journal_dir(&f, &sidecar, &session).join("identity").is_file());
    std::fs::remove_file(&pending_dir).unwrap();
    std::fs::rename(&aside, &pending_dir).unwrap();
    head(&mut helper, &sidecar, &session, 1).await;
    let mut stream = watch(&mut runtime, &sidecar, &session, 0).await.into_inner();
    assert_eq!(next(&mut stream).await.sequence, 1);
}

#[tokio::test]
async fn one_unreadable_journal_does_not_stop_the_others_from_being_adopted() {
    let mut f = Fixture::new();
    let (mut helper, mut runtime, sidecar, healthy) = start(&mut f).await;
    let mut sessions = Vec::new();
    for _ in 0..2 {
        let session = runtime
            .create_session(named(
                &sidecar,
                CreateSessionRequest {
                    cwd: "/srv/project".into(),
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner()
            .session_id;
        head(&mut helper, &sidecar, &session, 1).await;
        sessions.push(session);
    }
    let healthy_head = head(&mut helper, &sidecar, &healthy, 1).await;
    let (garbled, corrupt) = (sessions[0].clone(), sessions[1].clone());
    f.crash();
    drop(runtime);
    drop(helper);
    private_fs::write_private(
        &journal_dir(&f, &sidecar, &garbled).join("identity"),
        b"not an identity",
    )
    .unwrap();
    private_fs::write_private(
        &journal_dir(&f, &sidecar, &corrupt).join("seg-00000000000000099999.log"),
        b"not a segment",
    )
    .unwrap();
    let mut helper = f.start().await;
    assert_eq!(head(&mut helper, &sidecar, &healthy, healthy_head).await, healthy_head);
    let mut runtime = RuntimeServiceClient::new(connect_channel(&f.root).await);
    for session in [&garbled, &corrupt] {
        let (replayed, status) = watch_end(&mut runtime, &sidecar, session, 0).await;
        assert!(replayed.is_empty());
        assert_eq!(status.code(), Code::Unavailable, "{status:?}");
        assert!(
            status.message().contains("cannot follow this session yet"),
            "{status:?}"
        );
    }
    assert_eq!(audited(&f, "watch-adoption"), 1);
    assert_eq!(audited(&f, "watch-observe"), 1);
}

fn trust_scan_request(root: &std::path::Path, profile_key: &str) -> TrustScanRequest {
    TrustScanRequest {
        root: root.to_str().unwrap().to_owned(),
        profile_key: profile_key.into(),
    }
}

/// The account's home as passwd gives it, resolved as discovery needs it.
fn passwd_home() -> PathBuf {
    let home = PathBuf::from(
        daemon::login_env::minimal_environment()
            .unwrap()
            .remove(OsStr::new("HOME"))
            .unwrap(),
    );
    std::fs::canonicalize(&home).unwrap_or(home)
}

/// Whether `discovery` has an entry for `file`, an absolute path; entries are named relative to the top level.
fn lists(discovery: &eitri_core::project_trust::Discovery, file: &std::path::Path) -> bool {
    discovery.entries.iter().any(|entry| {
        let mut full = discovery.top.clone();
        for part in entry.path.components() {
            match part {
                std::path::Component::ParentDir => {
                    full.pop();
                }
                std::path::Component::CurDir => {}
                other => full.push(other),
            }
        }
        full == file
    })
}

fn scanned(response: TrustScanResponse) -> eitri_core::project_trust::Discovery {
    assert_eq!(response.refusal, "");
    serde_json::from_slice(&response.discovery_json).unwrap()
}

#[tokio::test]
async fn trust_scan_equals_local_discover_on_the_same_tree() {
    use eitri_core::project_trust::{discover, Finding, Limits, Profile, ProfileEnv};
    let mut f = Fixture::new();
    let mut helper = f.start().await;
    let project = f.root.join("proj");
    std::fs::create_dir_all(project.join(".claude")).unwrap();
    std::fs::write(
        project.join(".claude/settings.json"),
        br#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo hook"}]}]}}"#,
    )
    .unwrap();
    std::fs::write(project.join("CLAUDE.md"), "# the project\n").unwrap();
    std::fs::write(project.join("AGENTS.md"), "# for codex\n").unwrap();
    // The daemon's own HOME is the fixture root, where discovery would leave `.claude/` out as the user tier's.
    // The account's home comes from passwd, so this file is a project file above the top level.
    std::fs::create_dir_all(f.root.join(".claude")).unwrap();
    std::fs::write(f.root.join(".claude/CLAUDE.md"), "# above the project\n").unwrap();
    let home = passwd_home();
    let canonical = std::fs::canonicalize(&project).unwrap();
    for (key, profile) in [("claude", Profile::Claude), ("codex", Profile::Codex)] {
        let response = helper
            .trust_scan(trust_scan_request(&project, key))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(response.refusal, "");
        let local = discover(
            &canonical,
            Some(&home),
            &Limits::default(),
            &profile,
            &ProfileEnv::default(),
        );
        assert_eq!(
            String::from_utf8(response.discovery_json).unwrap(),
            serde_json::to_string(&local).unwrap(),
            "{key}"
        );
        if key == "claude" {
            assert!(local
                .findings
                .iter()
                .any(|finding| matches!(finding, Finding::Hook { .. })));
            assert!(local.entries.iter().any(|entry| entry.path.ends_with("CLAUDE.md")));
            let above = std::fs::canonicalize(&f.root).unwrap().join(".claude/CLAUDE.md");
            assert!(lists(&local, &above), "{:?}", local.entries);
        }
    }
}

#[tokio::test]
async fn trust_scan_runs_on_the_directory_a_sidecar_would_run_in() {
    use eitri_core::project_trust::{discover, Limits, Profile, ProfileEnv};
    let mut f = Fixture::new();
    let mut helper = f.start().await;
    let real = f.root.join("real");
    std::fs::create_dir_all(real.join("proj")).unwrap();
    std::fs::write(real.join("CLAUDE.md"), "# above the real project\n").unwrap();
    std::os::unix::fs::symlink(&real, f.root.join("link")).unwrap();
    let discovery = scanned(
        helper
            .trust_scan(trust_scan_request(&f.root.join("link/proj"), "claude"))
            .await
            .unwrap()
            .into_inner(),
    );
    let canonical = std::fs::canonicalize(real.join("proj")).unwrap();
    assert_eq!(discovery.root, canonical);
    assert!(
        lists(&discovery, &canonical.parent().unwrap().join("CLAUDE.md")),
        "{:?}",
        discovery.entries
    );
    let local = discover(
        &canonical,
        Some(&passwd_home()),
        &Limits::default(),
        &Profile::Claude,
        &ProfileEnv::default(),
    );
    assert_eq!(discovery, local);
}

#[tokio::test]
async fn trust_scan_refuses_a_relative_root() {
    let mut f = Fixture::new();
    let mut helper = f.start().await;
    for root in ["relative/dir", "", "/tmp/../tmp", "/nonexistent-eitri-root"] {
        let response = helper
            .trust_scan(TrustScanRequest {
                root: root.into(),
                profile_key: "claude".into(),
            })
            .await
            .unwrap()
            .into_inner();
        assert!(!response.refusal.is_empty(), "{root:?}");
        assert!(response.discovery_json.is_empty(), "{root:?}");
    }
    if private_fs::euid() != 0 {
        let response = helper
            .trust_scan(TrustScanRequest {
                root: "/".into(),
                profile_key: "claude".into(),
            })
            .await
            .unwrap()
            .into_inner();
        assert!(response.refusal.contains("owned"), "{}", response.refusal);
    }
}

#[tokio::test]
async fn trust_scan_refuses_a_profile_that_does_not_run_on_a_server() {
    let mut f = Fixture::new();
    let mut helper = f.start().await;
    for key in ["acp-some-agent", "", "Claude"] {
        let response = helper
            .trust_scan(trust_scan_request(&f.root, key))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            response.refusal, "only claude and codex tabs run on a server",
            "{key:?}"
        );
        assert!(response.discovery_json.is_empty());
    }
}
