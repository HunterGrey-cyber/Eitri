//! The bridge against this build's daemon and a real language server process: a session over a `RemoteConnection`
//! through `LocalExec` (no ssh, no display), the daemon's own `Lsp` service, and the scripted language server
//! (`core/tests/fixtures/lsp/scripted-server`). The daemon is the fixture in `server/tests/exec_command.rs`; build it
//! first: `cargo build -p eitri-server --bins` and `cargo test -p eitri-server --test exec_command --no-run`.
//!
//! The dial is `eitri host lsp`'s own (`cli_lsp::dial`): it asks Hello for `lsp` first, as the CLI does.
#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent::remote_client::terminal_transport::wire;
use agent::remote_client::HelperClient;
use agent::ssh::{ChannelLauncher, LinkCheck, LocalExec};
use eitri_core::remote_host::{cli_lsp, cli_read};
use eitri_core::remote_lsp::bridge::Disk;
use eitri_core::remote_lsp::framing::{encode_frame, Frame, FrameReader};
use eitri_core::remote_lsp::link::Redial;
use eitri_core::remote_lsp::session::{self, Io, Options};
use eitri_core::remote_lsp::uris::Rewriter;
use eitri_core::remote_project::{RemoteConnection, RemoteProject};
use eitri_core::remote_sync::ignores::Matcher;
use serde_json::{json, Value};

const HOST: &str = "example-host";
const KEY: &str = "SHA256:example-host-key";
const HANG: Duration = Duration::from_secs(60);

fn target_debug() -> PathBuf {
    if let Some(target) = std::env::var_os("CARGO_TARGET_DIR") {
        return PathBuf::from(target).join("debug");
    }
    std::env::current_exe()
        .unwrap()
        .ancestors()
        .find(|path| path.file_name().is_some_and(|name| name == "debug"))
        .unwrap()
        .to_path_buf()
}

/// The newest `exec_command` test binary: Cargo's ordinary layout and the runner's per-package layout both.
fn fixture_binary() -> PathBuf {
    fn visit(dir: &Path, depth: usize, candidates: &mut Vec<PathBuf>) {
        if depth == 0 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, depth - 1, candidates);
            } else if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("exec_command-") && !name.contains('.'))
            {
                candidates.push(path);
            }
        }
    }
    let debug = target_debug();
    let mut candidates = Vec::new();
    visit(&debug.join("build/eitri-server"), 4, &mut candidates);
    visit(&debug.join("deps"), 1, &mut candidates);
    candidates.sort_by_key(|path| std::fs::metadata(path).and_then(|meta| meta.modified()).ok());
    candidates
        .pop()
        .expect("build the daemon fixture: cargo test -p eitri-server --test exec_command --no-run")
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn script(path: &Path, body: &str) {
    assert!(Command::new("sh")
        .args([
            "-c",
            "umask 077; printf '%s' \"$1\" > \"$2\" && chmod 700 \"$2\"",
            "sh",
            body
        ])
        .arg(path)
        .status()
        .unwrap()
        .success());
}

/// `LocalExec`, with every channel process it starts held by a pidfd so the test can cut them.
struct Launcher {
    local: LocalExec,
    children: Mutex<Vec<OwnedFd>>,
}

impl Launcher {
    /// Kills every channel process: the link under the bridge's stream breaks, as a dropped ssh connection does.
    fn cut(&self) {
        for pin in self.children.lock().unwrap().drain(..) {
            // SAFETY: a signal through a pidfd this launcher opened for a process it started.
            unsafe {
                libc::syscall(libc::SYS_pidfd_send_signal, pin.as_raw_fd(), libc::SIGKILL, 0, 0);
            }
        }
    }
}

impl ChannelLauncher for Launcher {
    fn spawn(&self, tail: &[OsString]) -> io::Result<Child> {
        let child = self.local.spawn(tail)?;
        // SAFETY: pidfd_open of the child just started; the descriptor is owned below.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) } as i32;
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is the new pidfd and nothing else owns it.
        self.children.lock().unwrap().push(unsafe { OwnedFd::from_raw_fd(fd) });
        Ok(child)
    }

    fn host_key(&self) -> Option<io::Result<String>> {
        Some(Ok(KEY.into()))
    }

    fn check_link(&self) -> LinkCheck {
        LinkCheck::NoMaster
    }
}

struct Fixture {
    root: PathBuf,
    daemon: Child,
    launcher: Arc<Launcher>,
}

impl Fixture {
    fn new() -> Self {
        // Short, so the daemon's socket path stays under the Unix limit.
        let root = PathBuf::from(format!(
            "/tmp/lb-{}-{}",
            std::process::id(),
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        agent::private_fs::create_private_dir_all(&root, &root).unwrap();
        std::fs::create_dir(root.join("work")).unwrap();
        let helper = root.join("helper");
        let server = target_debug().join("eitri-server");
        assert!(server.is_file(), "build eitri-server first");
        script(
            &helper,
            &format!(
                "#!/bin/sh\nexport HOME={} XDG_DATA_HOME={} XDG_STATE_HOME={} TMPDIR={}\nexec {} \"$@\"\n",
                quote(&root),
                quote(&root.join("d")),
                quote(&root.join("s")),
                quote(&root),
                quote(&server)
            ),
        );
        let mut daemon = Command::new(fixture_binary())
            .args(["--exact", "fixture_daemon_entry", "--nocapture"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &root)
            .env("TMPDIR", &root)
            .env("XDG_DATA_HOME", root.join("d"))
            .env("XDG_STATE_HOME", root.join("s"))
            .env("EITRI_TEST_DAEMON", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(root.join("daemon.err")).unwrap())
            .spawn()
            .unwrap();
        let info = root
            .join("s/eitri-server/run")
            .join(env!("CARGO_PKG_VERSION"))
            .join("info");
        let deadline = Instant::now() + HANG;
        while !info.exists() {
            assert!(
                daemon.try_wait().unwrap().is_none(),
                "daemon exited: {}",
                std::fs::read_to_string(root.join("daemon.err")).unwrap_or_default()
            );
            assert!(Instant::now() < deadline, "daemon startup hung");
            std::thread::sleep(Duration::from_millis(10));
        }
        let launcher = Arc::new(Launcher {
            local: LocalExec { helper },
            children: Mutex::new(Vec::new()),
        });
        Self { root, daemon, launcher }
    }

    fn connection(&self) -> RemoteConnection {
        let launcher: Arc<dyn ChannelLauncher> = self.launcher.clone();
        let client = HelperClient::connect(launcher, HOST, Some(KEY)).unwrap();
        let project = RemoteProject {
            version: 1,
            host: HOST.into(),
            host_key_sha256: KEY.into(),
            root: self.root.join("work"),
            helper_version: "0.4.0-example".into(),
            sync: None,
            created_at: 0,
            queued_stops: vec![],
        };
        RemoteConnection::with_client(client, project, String::new(), String::new())
    }

    /// The lines the language server wrote for each message it read, parsed.
    fn received(&self) -> Vec<Value> {
        std::fs::read_to_string(self.root.join("received"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// Waits until the language server has read messages for which `done` holds, and gives them all.
    fn wait_received(&self, what: &str, mut done: impl FnMut(&[Value]) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + HANG;
        loop {
            let received = self.received();
            if done(&received) {
                return received;
            }
            assert!(
                Instant::now() < deadline,
                "the language server never read {what}; it read {received:?}; daemon: {}",
                std::fs::read_to_string(self.root.join("daemon.err")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The mirror's root is the project's own directory; no file is hashed.
struct NoDisk(PathBuf);

impl Disk for NoDisk {
    fn sha256(&self, _: &Path) -> io::Result<String> {
        Err(io::ErrorKind::NotFound.into())
    }

    fn carried(&self, _: &Path) -> bool {
        false
    }

    fn rewriter(&self, host: &str) -> Rewriter {
        Rewriter::new(host, &self.0, Matcher::new(&[]))
    }
}

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Nvim {
    stdin: std::io::PipeWriter,
    out: Shared,
    err: Shared,
    taken: usize,
    reader: FrameReader,
}

impl Nvim {
    fn send(&mut self, message: &Value) {
        self.stdin
            .write_all(&encode_frame(&serde_json::to_vec(message).unwrap()))
            .unwrap();
    }

    /// The next message the bridge wrote to its stdout.
    fn message(&mut self) -> Value {
        let until = Instant::now() + HANG;
        loop {
            if let Some(Frame::Message(body)) = self.reader.next_frame().unwrap() {
                return serde_json::from_slice(&body).unwrap();
            }
            assert!(
                Instant::now() < until,
                "nothing came; stderr: {}",
                String::from_utf8_lossy(&self.err.0.lock().unwrap())
            );
            let bytes = self.out.0.lock().unwrap().clone();
            self.reader.push(&bytes[self.taken..]);
            self.taken = bytes.len();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The answer to Neovim's request `id`, past anything else the bridge wrote meanwhile.
    fn answer(&mut self, id: i64) -> Value {
        loop {
            let message = self.message();
            if message["id"] == id && message.get("method").is_none() {
                return message;
            }
        }
    }
}

fn method_count(received: &[Value], method: &str) -> usize {
    received.iter().filter(|message| message["method"] == method).count()
}

#[test]
fn the_bridge_drives_a_real_language_server_through_the_daemon() {
    let fixture = Fixture::new();
    let connection = Arc::new(fixture.connection());
    let server = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/lsp/scripted-server");
    let script_file = fixture.root.join("script");
    std::fs::write(&script_file, "").unwrap();
    let work = fixture.root.join("work");
    let options = Options {
        host: HOST.into(),
        root: work.clone(),
        argv: vec![
            server.into(),
            script_file.display().to_string(),
            fixture.root.join("received").display().to_string(),
        ],
        grace_ms: 0,
        backoff: vec![Duration::from_millis(50)],
        open_deadline: Duration::from_secs(20),
        nvim_queue_max: session::NVIM_QUEUE_MAX,
        stop_signals: None,
        flush: None,
        quiet_exit: session::QUIET_EXIT,
    };
    // The dial the CLI makes: Hello lists `lsp`, so the bridge uses the daemon's service and runs nothing here.
    let Ok(first) = cli_lsp::dial(&connection) else {
        panic!("the daemon's Lsp service is not reachable")
    };
    let redial_connection = Arc::clone(&connection);
    let redial: Redial = Box::new(move || cli_lsp::dial(&redial_connection));
    let (stdin_read, stdin) = std::io::pipe().unwrap();
    let (out, err) = (Shared::default(), Shared::default());
    let io = Io {
        stdin: Box::new(stdin_read),
        stdout: Box::new(out.clone()),
        stderr: Box::new(err.clone()),
    };
    let mirror = work.clone();
    let session = std::thread::spawn(move || session::run(options, io, Some(first), redial, Box::new(NoDisk(mirror))));
    let mut nvim = Nvim {
        stdin,
        out,
        err,
        taken: 0,
        reader: FrameReader::new(1 << 27),
    };

    // The round trip: the server echoes requests, so its answer shows what it was sent.
    let root_uri = format!("file://{}", work.display());
    nvim.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"processId": 4242, "rootUri": root_uri, "capabilities": {}}}));
    let answer = nvim.answer(1);
    assert_eq!(answer["result"]["method"], "initialize", "{answer}");
    assert_eq!(answer["result"]["params"]["processId"], Value::Null);
    assert_eq!(answer["result"]["params"]["rootUri"], root_uri.as_str());
    let started = fixture.wait_received("its start", |received| !received.is_empty());
    assert_eq!(started[0]["started"]["cwd"], work.display().to_string());
    let pid = started[0]["started"]["pid"].as_i64().unwrap();
    nvim.send(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
    let doc = format!("{root_uri}/a.rs");
    nvim.send(&json!({"jsonrpc": "2.0", "method": "textDocument/didOpen",
        "params": {"textDocument": {"uri": doc, "languageId": "rust", "version": 1, "text": "fn a() {}"}}}));
    fixture.wait_received("the didOpen", |received| {
        method_count(received, "textDocument/didOpen") == 1
    });
    nvim.send(&json!({"jsonrpc": "2.0", "method": "textDocument/didChange",
        "params": {"textDocument": {"uri": doc, "version": 2}, "contentChanges": [{"text": "fn a2() {}"}]}}));
    fixture.wait_received("the didChange", |received| {
        method_count(received, "textDocument/didChange") == 1
    });

    // The link drops (the channel's processes are killed): the bridge reattaches to the same server and resyncs.
    fixture.launcher.cut();
    let received = fixture.wait_received("the resync", |received| {
        method_count(received, "textDocument/didOpen") == 2
    });
    let tail: Vec<&Value> = received.iter().rev().take(2).collect();
    assert_eq!(tail[1]["method"], "textDocument/didClose", "{received:?}");
    assert_eq!(tail[0]["params"]["textDocument"]["text"], "fn a2() {}");
    assert_eq!(tail[0]["params"]["textDocument"]["version"], 2);
    let starts = received
        .iter()
        .filter(|message| message.get("started").is_some())
        .count();
    assert_eq!(starts, 1, "the same server, not a fresh one");
    nvim.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "textDocument/hover", "params": {}}));
    assert_eq!(nvim.answer(2)["result"]["method"], "textDocument/hover");

    // Shutdown: passed on, then `exit`, and the server is stopped before the bridge ends.
    nvim.send(&json!({"jsonrpc": "2.0", "id": 3, "method": "shutdown", "params": null}));
    assert_eq!(nvim.answer(3)["result"]["method"], "shutdown");
    nvim.send(&json!({"jsonrpc": "2.0", "method": "exit", "params": null}));
    assert_eq!(session.join().unwrap(), 0);
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "the language server is still running"
    );
    let client = connection.lsp_client().unwrap();
    let (channel, handle) = client.lsp_channel().unwrap();
    let listed = handle
        .block_on(async move {
            wire::lsp_client::LspClient::new(channel)
                .list_servers(wire::ListServersRequest {})
                .await
        })
        .unwrap()
        .into_inner();
    assert!(listed.servers.is_empty(), "{:?}", listed.servers);
}

/// `eitri host read`'s fetch against this build's daemon: a file in chunks with its hash checked, and the daemon's
/// refusals as the one line a person is shown.
#[test]
fn eitri_host_read_fetches_a_file_through_the_daemon_and_says_why_it_cannot() {
    let fixture = Fixture::new();
    let client = fixture.connection().lsp_client().unwrap();
    let work = fixture.root.join("work");
    let small = work.join("dep.rs");
    std::fs::write(&small, "pub fn dep() {}\n").unwrap();
    let big: Vec<u8> = (0..300_000u32).map(|at| (at % 251) as u8).collect();
    std::fs::write(work.join("big.bin"), &big).unwrap();
    std::fs::write(work.join("empty"), b"").unwrap();
    std::os::unix::fs::symlink(&small, work.join("link.rs")).unwrap();
    let read = |path: &Path| cli_read::fetch(&client, path.to_str().unwrap());
    assert_eq!(read(&small).unwrap(), b"pub fn dep() {}\n");
    assert_eq!(read(&work.join("big.bin")).unwrap(), big);
    assert_eq!(read(&work.join("empty")).unwrap(), b"");
    for (path, why) in [
        (work.clone(), "is a directory"),
        (work.join("absent.rs"), "does not exist"),
        (work.join("link.rs"), "is a symbolic link"),
    ] {
        let error = read(&path).unwrap_err();
        assert!(error.contains(why), "{path:?}: {error}");
    }
}
