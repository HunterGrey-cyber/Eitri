//! The daemon's `Terminal` service, reached as the laptop reaches it: an in-test tonic client over `eitri-server
//! connect`, to a fixture daemon (this test binary's `fixture_daemon_entry`) whose terminal hosts are the real
//! `eitri-server term-host`.
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
use std::ffi::OsString;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use agent::private_fs;
use daemon::terminals;
use eitri_server_proto::helper_client::HelperClient;
use eitri_server_proto::terminal_client::TerminalClient;
use eitri_server_proto::terminal_client_frame::Frame as ClientFrame;
use eitri_server_proto::terminal_server_frame::Frame as ServerFrame;
use eitri_server_proto::*;
use tonic::transport::Channel;
use tonic::Code;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REVISION: &str = env!("EITRI_BUILD_COMMIT");
const HOST: &str = env!("CARGO_BIN_EXE_eitri-server");
const WAIT: Duration = Duration::from_secs(60);

fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}

/// The fixture's account: this user's passwd names, the fixture's home, and a login shell of the test's choosing
/// (`/bin/sh` unless `EITRI_TEST_SHELL` names another), so no real profile is read.
fn config(home: &Path) -> terminals::Config {
    let mut identity = daemon::login_env::passwd_identity().unwrap();
    identity.insert("HOME".into(), home.into());
    let shell = std::env::var_os("EITRI_TEST_SHELL").unwrap_or_else(|| "/bin/sh".into());
    identity.insert("SHELL".into(), shell);
    terminals::Config {
        host_program: HOST.into(),
        identity: Some(identity),
        ..Default::default()
    }
}

#[test]
fn fixture_daemon_entry() {
    if std::env::var_os("EITRI_TEST_DAEMON").is_none() {
        return;
    }
    run_dir::save_inherited_umask();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let paths = run_dir::Paths::from_env(VERSION).unwrap();
        let idle = std::env::var("EITRI_TEST_IDLE").unwrap().parse().unwrap();
        daemon::run_with_terminals(
            paths,
            Duration::from_secs(idle),
            daemon::OwnedSidecars::default(),
            async {
                Ok(daemon::login_env::Captured {
                    env: BTreeMap::from([(OsString::from("LANG"), OsString::from("POSIX"))]),
                    fallback: None,
                })
            },
            config(&home),
            None,
        )
        .await
        .unwrap();
    });
}

struct Fixture {
    root: PathBuf,
    paths: run_dir::Paths,
    daemon: Option<Child>,
    strays: Vec<OwnedFd>,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/tmp/ts-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        private_fs::create_private_dir_all(&root, &root).unwrap();
        std::fs::create_dir(root.join("work")).unwrap();
        let paths = run_dir::Paths::new(VERSION, &root.join("d"), &root.join("s"), &std::env::temp_dir()).unwrap();
        paths.prepare().unwrap();
        Self {
            root: root.canonicalize().unwrap(),
            paths,
            daemon: None,
            strays: Vec::new(),
        }
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    /// Starts the fixture daemon and waits for its Hello.
    async fn start(&mut self, idle_seconds: u64, env: &[(&str, &str)]) {
        assert!(self.daemon.is_none());
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "fixture_daemon_entry", "--nocapture"])
            .env_clear()
            .env("EITRI_TEST_DAEMON", "1")
            .env("EITRI_TEST_IDLE", idle_seconds.to_string())
            .env("HOME", &self.root)
            .env("XDG_DATA_HOME", self.root.join("d"))
            .env("XDG_STATE_HOME", self.root.join("s"))
            .env("PATH", "/usr/bin:/bin")
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(self.root.join("daemon.err")).unwrap())
            .spawn()
            .unwrap();
        let pid = child.id();
        self.daemon = Some(child);
        let deadline = Instant::now() + WAIT;
        loop {
            assert!(
                self.daemon.as_mut().unwrap().try_wait().unwrap().is_none(),
                "daemon exited during startup"
            );
            let published = run_dir::Paths::published(&self.paths).is_ok_and(|p| p.info.pid == pid);
            if published && self.hello().await.is_ok() {
                return;
            }
            assert!(Instant::now() < deadline, "daemon did not start");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn hello(&self) -> Result<HelloResponse, tonic::Status> {
        let socket = self.paths.socket().unwrap();
        let channel = tonic::transport::Endpoint::from_static("http://localhost")
            .connect_with_connector(tower::service_fn(move |_| {
                let socket = socket.clone();
                async move {
                    let stream = tokio::net::UnixStream::connect(socket).await?;
                    Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
                }
            }))
            .await
            .map_err(|error| tonic::Status::unavailable(error.to_string()))?;
        HelperClient::new(channel)
            .hello(HelloRequest {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                client_version: VERSION.into(),
            })
            .await
            .map(tonic::Response::into_inner)
    }

    fn kill_daemon(&mut self) {
        let mut child = self.daemon.take().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        std::fs::remove_file(self.paths.socket().unwrap()).unwrap();
    }

    /// A channel through `eitri-server connect`, as the laptop's link has one through ssh.
    async fn channel(&self) -> Channel {
        let root = self.root.clone();
        tonic::transport::Endpoint::from_static("http://localhost")
            .connect_with_connector(tower::service_fn(move |_| {
                let mut child = tokio::process::Command::new(HOST)
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

    async fn client(&self) -> TerminalClient<Channel> {
        TerminalClient::new(self.channel().await)
    }

    async fn open(&self, command_id: &str) -> Result<String, tonic::Status> {
        self.client()
            .await
            .open_terminal(OpenTerminalRequest {
                command_id: command_id.into(),
                cwd: self.work().to_str().unwrap().into(),
                size: Some(SIZE),
                colors: None,
            })
            .await
            .map(|response| response.into_inner().terminal_id)
    }

    async fn list(&self) -> ListTerminalsResponse {
        self.client()
            .await
            .list_terminals(ListTerminalsRequest {})
            .await
            .unwrap()
            .into_inner()
    }

    async fn attach(&self, id: &str, client_id: &str) -> Attached {
        Attached::new(self.client().await, id, client_id).await
    }

    fn record(&self, id: &str) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(self.paths.metadata_run().join(format!("th-{id}.pid"))).unwrap()).unwrap()
    }

    fn host(&self, id: &str) -> OwnedFd {
        daemon::sidecars::pin(self.record(id)["pid"].as_u64().unwrap() as u32).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Ok(files) = std::fs::read_dir(self.paths.metadata_run()) {
            for file in files.flatten() {
                if !file.file_name().to_string_lossy().ends_with(".pid") {
                    continue;
                }
                let Ok(record) =
                    serde_json::from_slice::<serde_json::Value>(&std::fs::read(file.path()).unwrap_or_default())
                else {
                    continue;
                };
                if let (Some(pid), Some(start)) = (record["pid"].as_u64(), record["start_time"].as_u64()) {
                    if let Ok(fd) = daemon::sidecars::pin(pid as u32) {
                        if daemon::sidecars::start_time(pid as u32).ok() == Some(start) {
                            let _ = daemon::sidecars::signal(&fd, libc::SIGKILL);
                        }
                    }
                }
            }
        }
        for stray in &self.strays {
            let _ = daemon::sidecars::signal(stray, libc::SIGKILL);
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
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

const SIZE: TermSize = TermSize {
    columns: 80,
    rows: 24,
    cell_width: 8,
    cell_height: 16,
};

fn exited(fd: &OwnedFd) -> bool {
    daemon::sidecars::exited(fd).unwrap()
}

async fn wait_exited(fd: &OwnedFd) {
    let deadline = Instant::now() + WAIT;
    while !exited(fd) {
        assert!(Instant::now() < deadline, "process did not exit");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle.as_bytes())
}

/// One attachment through the daemon: frames in, text out (the snapshot's replay and live output, as bytes).
struct Attached {
    tx: Option<tokio::sync::mpsc::Sender<TerminalClientFrame>>,
    rx: tonic::Streaming<TerminalServerFrame>,
    text: Vec<u8>,
    sent: u64,
    written: u64,
    status: Option<tonic::Status>,
    ended: bool,
    /// The first snapshot's parts.
    parts: Vec<SnapshotPart>,
}

impl Attached {
    async fn new(client: TerminalClient<Channel>, id: &str, client_id: &str) -> Self {
        Self::sized(client, id, client_id, SIZE).await
    }

    async fn sized(mut client: TerminalClient<Channel>, id: &str, client_id: &str, size: TermSize) -> Self {
        let (tx, frames) = tokio::sync::mpsc::channel(16);
        tx.send(TerminalClientFrame {
            frame: Some(ClientFrame::Hello(AttachHello {
                terminal_id: id.into(),
                client_id: client_id.into(),
                input_sent: 0,
                size: Some(size),
                colors: None,
            })),
        })
        .await
        .unwrap();
        let rx = client
            .attach(tokio_stream::wrappers::ReceiverStream::new(frames))
            .await
            .unwrap()
            .into_inner();
        let mut attached = Self {
            tx: Some(tx),
            rx,
            text: Vec::new(),
            sent: 0,
            written: 0,
            status: None,
            ended: false,
            parts: Vec::new(),
        };
        loop {
            match attached.next().await {
                Some(ServerFrame::Snapshot(part)) => {
                    let last = part.last;
                    attached.parts.push(part);
                    if last {
                        break;
                    }
                }
                Some(_) => {}
                None => break,
            }
        }
        attached
    }

    async fn next(&mut self) -> Option<ServerFrame> {
        if self.ended {
            return None;
        }
        let frame = tokio::time::timeout(WAIT, self.rx.message())
            .await
            .expect("no frame from the daemon");
        match frame {
            Ok(Some(TerminalServerFrame { frame: Some(frame) })) => {
                match &frame {
                    ServerFrame::Snapshot(part) => {
                        self.text.extend_from_slice(&part.replay);
                        if part.last {
                            self.sent = part.input_written;
                            self.written = part.input_written;
                        }
                    }
                    ServerFrame::Output(output) => self.text.extend_from_slice(&output.data),
                    ServerFrame::InputWritten(written) => self.written = written.upto,
                    _ => {}
                }
                Some(frame)
            }
            Ok(Some(TerminalServerFrame { frame: None })) => panic!("an empty server frame"),
            Ok(None) => {
                self.ended = true;
                None
            }
            Err(status) => {
                self.ended = true;
                self.status = Some(status);
                None
            }
        }
    }

    async fn until(&mut self, mut wanted: impl FnMut(&ServerFrame) -> bool) -> ServerFrame {
        loop {
            match self.next().await {
                Some(frame) if wanted(&frame) => return frame,
                Some(_) => {}
                None => panic!("the stream ended first: {:?}", self.status),
            }
        }
    }

    async fn until_text(&mut self, needle: &str) {
        while !contains(&self.text, needle) {
            if self.next().await.is_none() {
                panic!(
                    "the stream ended before {needle:?} ({:?}); text: {}",
                    self.status,
                    String::from_utf8_lossy(&self.text)
                );
            }
        }
    }

    async fn send(&mut self, frame: ClientFrame) {
        self.tx
            .as_ref()
            .unwrap()
            .send(TerminalClientFrame { frame: Some(frame) })
            .await
            .unwrap();
    }

    async fn type_line(&mut self, line: &str) {
        let data = format!("{line}\r").into_bytes();
        let offset = self.sent;
        self.sent += data.len() as u64;
        self.send(ClientFrame::Input(InputBytes { offset, data })).await;
    }

    async fn written(&mut self) {
        while self.written < self.sent {
            if self.next().await.is_none() {
                panic!("the stream ended before the input was written");
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_attach_and_echo() {
    let mut f = Fixture::new();
    f.start(600, &[]).await;
    let id = f.open("open-echo").await.unwrap();
    assert_eq!(id.len(), 32);
    let mut client = f.attach(&id, "laptop").await;
    client.type_line("echo echoed-$((20+22))").await;
    client.until_text("echoed-42").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_gets_a_snapshot_then_live_output() {
    let mut f = Fixture::new();
    f.start(600, &[]).await;
    let id = f.open("open-live").await.unwrap();
    let mut first = f.attach(&id, "laptop").await;
    first.type_line("echo before-$((1+1))").await;
    first.until_text("before-2").await;
    drop(first);
    let mut client = f.attach(&id, "laptop-2").await;
    assert!(!client.parts.is_empty() && client.parts.last().unwrap().last);
    assert!(contains(&client.text, "before-2"), "the snapshot has the screen");
    let at = client.parts[0].offset;
    client.type_line("echo after-$((2+2))").await;
    let ServerFrame::Output(output) = client.until(|frame| matches!(frame, ServerFrame::Output(_))).await else {
        unreachable!()
    };
    assert_eq!(output.offset, at, "live output starts where the snapshot stands");
    client.until_text("after-4").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_snapshot_arrives_in_parts() {
    let mut f = Fixture::new();
    f.start(600, &[]).await;
    let id = f.open("open-large").await.unwrap();
    // Wide rows, so the history's rows hold far more than a part (the history keeps rows, not lines).
    let wide = TermSize { columns: 300, ..SIZE };
    let mut first = Attached::sized(f.client().await, &id, "laptop", wide).await;
    first
        .type_line("i=0; while [ $i -lt 5000 ]; do i=$((i+1)); printf '%0290d\\n' $i; done; echo filled-$((5*5))")
        .await;
    first.until_text("filled-25").await;
    drop(first);
    let client = Attached::sized(f.client().await, &id, "laptop-2", wide).await;
    assert!(client.parts.len() >= 2, "{} parts", client.parts.len());
    let total = client.parts[0].total_bytes;
    assert!(client
        .parts
        .iter()
        .all(|part| part.replay.len() <= TERM_SNAPSHOT_PART_MAX && part.total_bytes == total));
    assert_eq!(
        client.parts.iter().map(|part| part.replay.len() as u64).sum::<u64>(),
        total
    );
    assert!(client.parts[..client.parts.len() - 1].iter().all(|part| !part.last));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detach_and_reattach_shows_output_made_meanwhile() {
    let mut f = Fixture::new();
    f.start(600, &[]).await;
    let id = f.open("open-meanwhile").await.unwrap();
    let mut client = f.attach(&id, "laptop").await;
    client
        .type_line("sleep 1; i=0; while [ $i -lt 3000 ]; do i=$((i+1)); echo line-$i; done; echo done-$((6*7))")
        .await;
    client.written().await;
    drop(client);
    let deadline = Instant::now() + WAIT;
    loop {
        let client = f.attach(&id, "laptop").await;
        if contains(&client.text, "done-42") {
            assert!(contains(&client.text, "line-3000"));
            break;
        }
        assert!(Instant::now() < deadline, "the output never showed");
        drop(client);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_client_replaces_the_first() {
    let mut f = Fixture::new();
    f.start(600, &[]).await;
    let id = f.open("open-replace").await.unwrap();
    let mut first = f.attach(&id, "laptop-1").await;
    let _second = f.attach(&id, "laptop-2").await;
    let ServerFrame::Detached(detached) = first.until(|frame| matches!(frame, ServerFrame::Detached(_))).await else {
        unreachable!()
    };
    assert_eq!(detached.reason, DetachReason::Replaced as i32);
    assert!(first.next().await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_is_idempotent_by_command_id_also_after_a_daemon_restart() {
    let mut f = Fixture::new();
    f.start(600, &[]).await;
    let id = f.open("open-again").await.unwrap();
    assert_eq!(f.open("open-again").await.unwrap(), id);
    f.kill_daemon();
    f.start(600, &[]).await;
    assert_eq!(f.open("open-again").await.unwrap(), id);
    let ids: Vec<_> = f
        .list()
        .await
        .terminals
        .into_iter()
        .map(|info| info.terminal_id)
        .collect();
    assert_eq!(ids, vec![id]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_terminal_is_not_found() {
    let mut f = Fixture::new();
    f.start(600, &[]).await;
    let (tx, frames) = tokio::sync::mpsc::channel(1);
    tx.send(TerminalClientFrame {
        frame: Some(ClientFrame::Hello(AttachHello {
            terminal_id: "0".repeat(32),
            client_id: "laptop".into(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    let error = f
        .client()
        .await
        .attach(tokio_stream::wrappers::ReceiverStream::new(frames))
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::NotFound);
    assert_eq!(error.message(), "no such terminal on this server");
    let error = f
        .client()
        .await
        .close_terminal(CloseTerminalRequest {
            terminal_id: "0".repeat(32),
            command_id: "close-unknown".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::NotFound);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_hello_must_come_first_and_in_time() {
    let mut f = Fixture::new();
    f.start(600, &[]).await;
    let id = f.open("open-first").await.unwrap();
    let (tx, frames) = tokio::sync::mpsc::channel(1);
    tx.send(TerminalClientFrame {
        frame: Some(ClientFrame::Input(InputBytes {
            offset: 0,
            data: b"ls\r".to_vec(),
        })),
    })
    .await
    .unwrap();
    let error = f
        .client()
        .await
        .attach(tokio_stream::wrappers::ReceiverStream::new(frames))
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::InvalidArgument);
    let (_silent, frames) = tokio::sync::mpsc::channel::<TerminalClientFrame>(1);
    let started = Instant::now();
    let error = f
        .client()
        .await
        .attach(tokio_stream::wrappers::ReceiverStream::new(frames))
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::DeadlineExceeded);
    assert!(started.elapsed() >= terminals::host::HELLO_WITHIN);
    // The terminal is untouched by either.
    let mut client = f.attach(&id, "laptop").await;
    client.type_line("echo fine-$((3*3))").await;
    client.until_text("fine-9").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_input_frame_is_refused() {
    let mut f = Fixture::new();
    f.start(600, &[]).await;
    let id = f.open("open-oversized").await.unwrap();
    let mut client = f.attach(&id, "laptop").await;
    client
        .send(ClientFrame::Input(InputBytes {
            offset: client.sent,
            data: vec![b'x'; 65 * 1024],
        }))
        .await;
    while client.next().await.is_some() {}
    assert_eq!(
        client.status.as_ref().map(tonic::Status::code),
        Some(Code::InvalidArgument)
    );
}

/// With a two-second idle time the daemon stays while a terminal runs, and goes once it is closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_daemon_does_not_idle_exit_with_a_live_terminal() {
    let mut f = Fixture::new();
    f.start(2, &[]).await;
    let id = f.open("open-idle").await.unwrap();
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(
        f.daemon.as_mut().unwrap().try_wait().unwrap().is_none(),
        "the daemon idled out"
    );
    f.client()
        .await
        .close_terminal(CloseTerminalRequest {
            terminal_id: id,
            command_id: "close-idle".into(),
        })
        .await
        .unwrap();
    let mut daemon = f.daemon.take().unwrap();
    let deadline = Instant::now() + WAIT;
    while daemon.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the daemon did not idle out");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Seventy ended terminals (their records written as a daemon leaves them) make a list of the newest
/// `TERM_LIST_MAX`, newest first, that says it is truncated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_is_bounded_and_says_truncated() {
    let mut f = Fixture::new();
    let mut gone = Command::new("/bin/true").spawn().unwrap();
    let dead = gone.id();
    gone.wait().unwrap();
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
    for n in 0..70u64 {
        let id = format!("{n:032x}");
        let record = serde_json::json!({
            "pid": dead, "start_time": 1, "boot_id": boot.trim(),
            "socket": agent::socket_path::in_dir(&f.paths.run, &format!("th-{id}.sock")).unwrap(),
            "terminal_id": id, "start_dir": "/srv/project", "command_id": format!("open-{n}"),
            "started_unix_ms": 1_000 + n, "ready": true,
        });
        let dir = f.paths.metadata_run();
        private_fs::write_private(&dir.join(format!("th-{id}.pid")), &serde_json::to_vec(&record).unwrap()).unwrap();
        let exit = serde_json::json!({"code": n as i32, "signal": null, "left": false,
            "exited_unix_ms": run_dir::unix_ms()});
        private_fs::write_private(&dir.join(format!("th-{id}.exit")), &serde_json::to_vec(&exit).unwrap()).unwrap();
    }
    f.start(600, &[]).await;
    let listed = f.list().await;
    assert!(listed.truncated);
    assert_eq!(listed.terminals.len(), TERM_LIST_MAX);
    assert_eq!(listed.terminals[0].terminal_id, format!("{:032x}", 69));
    assert!(listed
        .terminals
        .windows(2)
        .all(|pair| pair[0].started_unix_ms > pair[1].started_unix_ms));
    assert!(listed.terminals.iter().all(|info| !info.alive && info.exit.is_some()));
}

/// Eighteen shells that exit at once leave eighteen hosts holding a final screen nobody has seen: the oldest are told
/// to go until sixteen stay up, and every exit record stays listed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exited_hosts_are_bounded_by_count() {
    let mut f = Fixture::new();
    f.start(600, &[("EITRI_TEST_SHELL", "/bin/true")]).await;
    let mut ids = Vec::new();
    for n in 0..18 {
        ids.push(f.open(&format!("open-{n}")).await.unwrap());
    }
    let hosts: Vec<OwnedFd> = ids.iter().map(|id| f.host(id)).collect();
    let deadline = Instant::now() + WAIT;
    loop {
        let up = hosts.iter().filter(|host| !exited(host)).count();
        if up == terminals::EXITED_UP_MAX {
            break;
        }
        assert!(up > terminals::EXITED_UP_MAX, "{up} exited hosts stayed up");
        assert!(Instant::now() < deadline, "{up} exited hosts are still up");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let listed = f.list().await;
    assert_eq!(listed.terminals.len(), 18);
    assert!(listed.terminals.iter().all(|info| info.exit.is_some()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_shows_job_cwd_and_detached_since() {
    let mut f = Fixture::new();
    std::fs::create_dir(f.work().join("sub")).unwrap();
    f.start(600, &[]).await;
    let id = f.open("open-facts").await.unwrap();
    let mut client = f.attach(&id, "laptop").await;
    client.type_line("cd sub && sleep 30").await;
    let sub = f.work().join("sub").display().to_string();
    let deadline = Instant::now() + WAIT;
    loop {
        let info = f.list().await.terminals.remove(0);
        if info.foreground_job == Some(true) && info.foreground_name == "sleep" && info.cwd == sub {
            assert!(info.attached && info.alive);
            assert_eq!(info.open_command_id, "open-facts");
            break;
        }
        assert!(Instant::now() < deadline, "no job facts: {info:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    drop(client);
    let deadline = Instant::now() + WAIT;
    loop {
        let info = f.list().await.terminals.remove(0);
        if !info.attached && info.detached_since_unix_ms > 0 {
            break;
        }
        assert!(Instant::now() < deadline, "still attached: {info:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_has_terminal_instance_and_boot() {
    let mut f = Fixture::new();
    f.start(600, &[]).await;
    let hello = f.hello().await.unwrap();
    assert!(hello.capabilities.iter().any(|cap| cap == CAP_TERMINAL));
    let instance = hello.daemon_instance.clone();
    assert_eq!(instance.len(), 32);
    assert!(instance
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
    assert_eq!(hello.boot_id, boot.trim());
    f.kill_daemon();
    f.start(600, &[]).await;
    let again = f.hello().await.unwrap();
    assert_ne!(again.daemon_instance, instance);
    assert_eq!(again.boot_id, hello.boot_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_is_idempotent_by_command_id() {
    let mut f = Fixture::new();
    f.start(600, &[]).await;
    let id = f.open("open-close").await.unwrap();
    let close = || async {
        f.client()
            .await
            .close_terminal(CloseTerminalRequest {
                terminal_id: id.clone(),
                command_id: "close-once".into(),
            })
            .await
            .unwrap()
            .into_inner()
    };
    let first = close().await;
    assert_eq!(first.exit.as_ref().and_then(|exit| exit.signal), Some(libc::SIGHUP));
    assert_eq!(close().await, first);
}

/// A client that keeps its side of the stream open after the host ended it (here by a close) does not hold the host
/// up: the daemon ends both directions, so the host goes well before its drain time would run out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_holding_its_side_open_does_not_delay_the_hosts_close() {
    let mut f = Fixture::new();
    f.start(600, &[]).await;
    let id = f.open("open-held").await.unwrap();
    let host = f.host(&id);
    let mut client = f.attach(&id, "laptop").await;
    let closed = Instant::now();
    f.client()
        .await
        .close_terminal(CloseTerminalRequest {
            terminal_id: id.clone(),
            command_id: "close-held".into(),
        })
        .await
        .unwrap();
    let ServerFrame::Detached(detached) = client.until(|frame| matches!(frame, ServerFrame::Detached(_))).await else {
        unreachable!()
    };
    assert_eq!(detached.reason, DetachReason::Closed as i32);
    // `client` still holds its sending side open.
    assert!(client.tx.is_some());
    wait_exited(&host).await;
    assert!(
        closed.elapsed() < terminals::host::DRAIN,
        "the host waited {:?} for a client's open side",
        closed.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_stops_reading_and_half_closes_releases_the_host() {
    let mut f = Fixture::new();
    f.start(600, &[]).await;
    let id = f.open("open-half").await.unwrap();
    let host = f.host(&id);
    let mut client = f.attach(&id, "laptop").await;
    client.type_line("yes | head -c 20000000").await;
    // Output now queues for a client that reads nothing more; it ends its request side.
    tokio::time::sleep(Duration::from_millis(300)).await;
    client.tx = None;
    let closed = Instant::now();
    f.client()
        .await
        .close_terminal(CloseTerminalRequest {
            terminal_id: id.clone(),
            command_id: "close-half".into(),
        })
        .await
        .unwrap();
    wait_exited(&host).await;
    assert!(
        closed.elapsed() < terminals::host::DRAIN,
        "the host waited {:?} for a stream its client had ended",
        closed.elapsed()
    );
    drop(client);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retried_open_answers_its_terminal_after_the_start_directory_moved() {
    let mut f = Fixture::new();
    f.start(600, &[]).await;
    let first = f.open("open-moved").await.unwrap();
    let moved = f.work().with_extension("moved");
    std::fs::rename(f.work(), &moved).unwrap();
    let again = f.open("open-moved").await;
    std::fs::rename(&moved, f.work()).unwrap();
    assert_eq!(again.unwrap(), first);
}
