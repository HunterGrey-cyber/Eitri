//! What the language-server tests share: a fixture with the scripted language server
//! (`core/tests/fixtures/lsp/scripted-server`) and `lsp::Servers`, the bridge's side of an attach driven by the test,
//! and waits on what the server has read.
#![allow(dead_code)]

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::daemon::login_env::{Captured, Environment};
use crate::daemon::lsp::{Servers, Sink};
use crate::{daemon, run_dir};
use eitri_server_proto::lsp_server_frame::Frame;
use eitri_server_proto::*;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tonic::Status;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const SCRIPTED: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../core/tests/fixtures/lsp/scripted-server"
);
/// Only guards against a hang.
pub const WAIT: Duration = Duration::from_secs(60);
/// A scratch directory with a project, a home, a `bin` holding the scripted server as `scripted-lsp`, and the
/// helper's data and state homes. Paths stay short: the end-to-end tests bind sockets in it.
pub struct Fixture {
    pub base: PathBuf,
    pub paths: run_dir::Paths,
    pub servers: Arc<Servers>,
    pub next: AtomicU32,
}

impl Fixture {
    pub fn new() -> Self {
        Fixture::with(|servers| servers)
    }

    /// A fixture whose `Servers` are first passed through `adjust` (a test-only setting, say).
    pub fn with(adjust: impl FnOnce(Servers) -> Servers) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let base = PathBuf::from(format!(
            "/tmp/lsp-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&base);
        agent::private_fs::create_private_dir_all(&base, &base).unwrap();
        let base = base.canonicalize().unwrap();
        for dir in ["home", "bin", "project", "d", "s", "work"] {
            std::fs::create_dir(base.join(dir)).unwrap();
        }
        daemon::test_exec::install_executable(Path::new(SCRIPTED), &base.join("bin/scripted-lsp"));
        let paths = run_dir::Paths::new(VERSION, &base.join("d"), &base.join("s"), &base).unwrap();
        paths.prepare().unwrap();
        let sidecars = Arc::new(daemon::sidecars::Sidecars::new(paths.clone()).unwrap());
        sidecars.set_environment(Ok(Captured {
            env: login(&base),
            fallback: None,
        }));
        let servers = Arc::new(adjust(
            Servers::new(paths.clone(), sidecars).with_min_grace(Duration::from_millis(300)),
        ));
        Fixture {
            base,
            paths,
            servers,
            next: AtomicU32::new(0),
        }
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.base.join(rel)
    }

    pub fn unique(&self, prefix: &str) -> String {
        format!("{prefix}{}", self.next.fetch_add(1, Ordering::Relaxed))
    }

    /// A script of `steps` and the file the server writes what it reads to.
    pub fn script(&self, steps: &[Value]) -> (PathBuf, PathBuf) {
        let name = self.unique("script");
        let script = self.path(&format!("work/{name}.jsonl"));
        let text: String = steps.iter().map(|step| format!("{step}\n")).collect();
        std::fs::write(&script, text).unwrap();
        (script, self.path(&format!("work/{name}.received")))
    }

    pub fn request(&self, argv0: &str, steps: &[Value], client_id: &str) -> (OpenServerRequest, PathBuf) {
        let (script, received) = self.script(steps);
        let request = OpenServerRequest {
            command_id: self.unique("open-"),
            root: self.path("project").to_str().unwrap().into(),
            argv: vec![
                argv0.into(),
                script.to_str().unwrap().into(),
                received.to_str().unwrap().into(),
            ],
            grace_ms: 0,
            client_id: client_id.into(),
        };
        (request, received)
    }

    /// Opens the scripted server with `steps` and waits until it has started.
    pub async fn open(&self, steps: &[Value]) -> Opened {
        let (request, received) = self.request("scripted-lsp", steps, "client-a");
        self.open_request(request, received).await
    }

    pub async fn open_request(&self, request: OpenServerRequest, received: PathBuf) -> Opened {
        let response = self.servers.open(request).await.unwrap();
        assert!(!response.server_id.is_empty(), "{response:?}");
        let started = until_received(&received, |line| line.get("started").is_some()).await;
        Opened {
            id: response.server_id.clone(),
            pid: started["started"]["pid"].as_i64().unwrap() as i32,
            received,
            response,
        }
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.path(&format!("work/{}", self.unique(name)))
    }

    pub fn audit(&self) -> String {
        std::fs::read_to_string(self.paths.state.join("audit.log")).unwrap_or_default()
    }

    pub fn info(&self, id: &str) -> Option<LspServerInfo> {
        self.servers
            .list()
            .servers
            .into_iter()
            .find(|info| info.server_id == id)
    }

    pub async fn until_detached(&self, id: &str) {
        until(|| self.info(id).is_some_and(|info| !info.attached)).await;
    }

    pub async fn until_gone(&self, id: &str) {
        until(|| self.info(id).is_none()).await;
    }

    pub async fn attach(&self, id: &str) -> Bridge {
        let attachment = self.servers.attach(id, "client-a").await;
        Bridge {
            frames: attachment.frames,
            sink: attachment.sink,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

pub fn login(base: &Path) -> Environment {
    Environment::from([
        ("PATH".into(), login_path(base)),
        ("HOME".into(), base.join("home").into_os_string()),
        ("LANG".into(), "C.UTF-8".into()),
    ])
}

pub fn login_path(base: &Path) -> OsString {
    format!("{}:/usr/bin:/bin", base.join("bin").display()).into()
}

pub struct Opened {
    pub id: String,
    pub pid: i32,
    pub received: PathBuf,
    pub response: OpenServerResponse,
}

/// The bridge's side of an attach, driven by the test.
pub struct Bridge {
    pub frames: mpsc::Receiver<Result<LspServerFrame, Status>>,
    pub sink: Option<Sink>,
}

impl Bridge {
    pub async fn send(&mut self, message: Value) {
        self.send_bytes(&serde_json::to_vec(&message).unwrap()).await;
    }

    /// Sends `message` (a `textDocument/didSave`) with the laptop file's hash on its last chunk.
    pub async fn send_save(&mut self, message: Value, sha256: &str) {
        let bytes = serde_json::to_vec(&message).unwrap();
        let mut chunks = chunks(&bytes);
        chunks.last_mut().unwrap().saved_sha256 = sha256.to_owned();
        let sink = self.sink.as_mut().expect("an attached bridge");
        for chunk in chunks {
            sink.chunk(chunk).await;
        }
    }

    pub async fn send_bytes(&mut self, bytes: &[u8]) {
        let sink = self.sink.as_mut().expect("an attached bridge");
        for chunk in chunks(bytes) {
            sink.chunk(chunk).await;
        }
    }

    pub async fn frame(&mut self) -> Frame {
        tokio::time::timeout(WAIT, self.frames.recv())
            .await
            .expect("a frame in time")
            .expect("a frame")
            .unwrap()
            .frame
            .unwrap()
    }

    pub async fn message(&mut self) -> Value {
        let mut data = Vec::new();
        loop {
            match self.frame().await {
                Frame::Chunk(chunk) => {
                    assert!(chunk.data.len() <= LSP_CHUNK_MAX);
                    data.extend(chunk.data);
                    if chunk.last {
                        return serde_json::from_slice(&data).unwrap();
                    }
                }
                Frame::Attached(attached) => panic!("expected a message, got {attached:?}"),
                Frame::Ended(ended) => panic!("expected a message, got {ended:?}"),
            }
        }
    }

    pub async fn attached(&mut self) -> LspAttached {
        match self.frame().await {
            Frame::Attached(attached) => attached,
            Frame::Ended(ended) => panic!("expected LspAttached, got {ended:?}"),
            Frame::Chunk(_) => panic!("expected LspAttached, got a chunk"),
        }
    }

    pub async fn ended(&mut self) -> LspEnded {
        loop {
            match self.frame().await {
                Frame::Ended(ended) => return ended,
                Frame::Attached(attached) => panic!("expected LspEnded, got {attached:?}"),
                Frame::Chunk(_) => {}
            }
        }
    }
}

pub fn chunks(bytes: &[u8]) -> Vec<LspChunk> {
    let count = bytes.len().div_ceil(LSP_CHUNK_MAX).max(1);
    (0..count)
        .map(|at| LspChunk {
            data: bytes[at * LSP_CHUNK_MAX..((at + 1) * LSP_CHUNK_MAX).min(bytes.len())].to_vec(),
            last: at + 1 == count,
            saved_sha256: String::new(),
        })
        .collect()
}

/// Waits for `done`; the deadline only guards against a hang.
pub async fn until(mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "hang guard");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// What the scripted server has read, one JSON value per line.
pub fn lines(received: &Path) -> Vec<Value> {
    std::fs::read_to_string(received)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

pub async fn until_received(received: &Path, matches: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(line) = lines(received).into_iter().find(&matches) {
            return line;
        }
        assert!(Instant::now() < deadline, "hang guard: {:?}", lines(received));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

pub fn response_to(id: i64) -> impl Fn(&Value) -> bool {
    move |line| line.get("method").is_none() && line.get("id") == Some(&json!(id))
}

pub fn method_is(method: &'static str) -> impl Fn(&Value) -> bool {
    move |line| line.get("method") == Some(&json!(method))
}

pub fn request(id: i64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

pub fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

/// Whether `pid` has ended (gone, or a zombie nobody has reaped yet).
pub fn ended(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat.rsplit(')').next().unwrap().split_whitespace().next() == Some("Z"),
        Err(_) => true,
    }
}

pub fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}
