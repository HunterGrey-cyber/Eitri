//! The remote audit trail, end to end. A window (a real panel with recording providers, in this process) starts
//! `eitri remote serve` the way a window does, the product binary runs it in a home of its own, sandboxed, and a
//! headless WebSocket client (the phone) enrolls, connects, sends every command kind, is refused for every reason a
//! phone can reach and reads its own records. The log in the home then holds each event once per action, written by
//! the writer that saw it, `serve`, the window or `eitri remote` in a terminal; every record also reached a journal
//! socket of the test's own; no token, enrollment code, prompt text or push path is in the log's bytes or the journal's;
//! and `eitri remote log --verify` is clean after a `serve` restart, then names an edited record, a deleted one and a
//! forged journal entry. Nothing here reaches the real journal: the journal socket is the test's, and the `journalctl`
//! the CLI runs is a stub that prints what that socket received.
//!
//! A plain `main` (`harness = false`): `own_display_scan` requires a test that starts the product binary to isolate its
//! display in `main`, which libtest's harness never runs. `serve` gets no display.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use eitri_core::agent_backend::AgentId;
use eitri_core::agent_bridge::SessionModeChoice;
use eitri_core::agents::{new_instance, ProjectRef, Publisher, Row, WindowRef};
use eitri_core::lua::config::{RemoteListen, RemoteSettings};
use eitri_core::remote_audit::{audit_dir, read, Filter, Record, JOURNAL_SOCKET_VAR};
use eitri_core::remote_pairing::{Endpoint, PairMode, PairScheme, PairingRoutes};
use eitri_core::remote_runtime::{start_serve, RemotePlan};
use eitri_core::remote_rv::serve_socket_path;
use eitri_core::tab_set::RowKey;
use eitri_core::tabs::TabId;
use eitri_core::test_providers::RecordingProvider;
use eitri_panel::agent_panel::{AgentPanelHandle, Pairing, RemoteSocket, ViewId};
use eitri_panel::panel_page::PageSurface;
use eitri_remote::serve::spawn::{plan, ServeSpawner};
use eitri_remote::serve::Dirs;
use serde_json::{json, Value};

// The display this test runs on: its own Xvfb, never an inherited one.
#[path = "support/own_x_server.rs"]
mod own_x_server;

/// How long any one step may take on a loaded machine; a check that passes never waits it out.
const PATIENCE: Duration = Duration::from_secs(30);

/// A text only the prompts carry: it must be in no record and no journal entry.
const SENTINEL: &str = "SENTINEL-PROMPT-7f3a91c2";

/// A path only the push endpoint carries: a record names the host, never this.
const PUSH_PATH: &str = "push-topic-5d8e0b14";

type Check = fn(&str, &Path) -> Result<(), String>;

fn main() {
    // Before anything that could connect to a display or a bus.
    let server = own_x_server::isolate("remote_audit_e2e", "640x480x24");
    agent::state_dirs::redirect_state_to_a_test_root();
    let shell = env!("CARGO_BIN_EXE_shell");
    let scratch: PathBuf = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("rae2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).expect("creating the scratch directory must succeed");
    let checks: Vec<(&str, Check)> = vec![
        ("remote_off_writes_no_log_and_no_journal", remote_off_writes_nothing),
        ("the_audit_trail_end_to_end", the_audit_trail_end_to_end),
    ];
    // `cargo test --test remote_audit_e2e -- <part of a name>` runs the checks whose names hold it.
    let filter = std::env::args().skip(1).find(|arg| !arg.starts_with('-'));
    let mut failed = 0;
    let mut ran = 0;
    for (at, (name, check)) in checks.iter().enumerate() {
        if filter.as_deref().is_some_and(|part| !name.contains(part)) {
            continue;
        }
        ran += 1;
        let dir = scratch.join(at.to_string());
        std::fs::create_dir_all(&dir).expect("creating a check's directory must succeed");
        let home = Home::new(&dir);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(shell, &dir)))
            .unwrap_or_else(|_| Err("panicked".to_owned()));
        // Whatever the check left running ends with it.
        let _ = run(home.remote(shell, &["stop"]));
        std::env::remove_var(JOURNAL_SOCKET_VAR);
        match outcome {
            Ok(()) => println!("test {name} ... ok"),
            Err(why) => {
                failed += 1;
                println!("test {name} ... FAILED\n    {why}");
            }
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
    println!("remote_audit_e2e: {} passed, {failed} failed", ran - failed);
    server.exit(if failed == 0 { 0 } else { 1 });
}

fn check(ok: bool, what: impl FnOnce() -> String) -> Result<(), String> {
    if ok {
        Ok(())
    } else {
        Err(what())
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The home: private directories standing in for the user's, given to every process the test starts.

struct Home {
    root: PathBuf,
    /// The test's journal socket and the stub `journalctl`'s directory, once [`Home::with_journal`] was called.
    journal: Option<(PathBuf, PathBuf)>,
}

impl Home {
    fn new(dir: &Path) -> Home {
        use std::os::unix::fs::PermissionsExt;
        for sub in [
            "home",
            "run",
            "state",
            "run/eitri",
            "run/eitri/agents",
            "state/eitri",
            "state/eitri/remote",
        ] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
            std::fs::set_permissions(dir.join(sub), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        Home {
            root: dir.to_owned(),
            journal: None,
        }
    }

    /// Sends every record of every process of this home to `socket`, and makes the `journalctl` the CLI runs the stub
    /// in `bin`.
    fn with_journal(mut self, socket: &Path, bin: &Path) -> Home {
        self.journal = Some((socket.to_owned(), bin.to_owned()));
        self
    }

    fn dirs(&self) -> Dirs {
        Dirs {
            control: self.root.join("run/eitri"),
            state: self.root.join("state/eitri/remote"),
        }
    }

    fn audit(&self) -> PathBuf {
        audit_dir(&self.dirs().state)
    }

    fn env(&self) -> Vec<(&'static str, OsString)> {
        let mut env: Vec<(&'static str, OsString)> = vec![
            ("HOME", self.root.join("home").into()),
            ("XDG_CONFIG_HOME", self.root.join("config").into()),
            ("XDG_DATA_HOME", self.root.join("data").into()),
            ("XDG_STATE_HOME", self.root.join("state").into()),
            ("XDG_CACHE_HOME", self.root.join("cache").into()),
            ("XDG_RUNTIME_DIR", self.root.join("run").into()),
        ];
        if let Some((socket, bin)) = &self.journal {
            env.push((JOURNAL_SOCKET_VAR, socket.clone().into()));
            let mut path = OsString::from(bin);
            path.push(":");
            path.push(std::env::var_os("PATH").unwrap_or_default());
            env.push(("PATH", path));
        }
        env
    }

    /// The product binary's `eitri remote <args>`, in this home.
    fn remote(&self, shell: &str, args: &[&str]) -> Command {
        let mut command = Command::new(shell);
        command
            .arg("remote")
            .args(args)
            .envs(self.env())
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// What a window starts `serve` with: the product binary, in this home.
    fn spawner(&self, shell: &str) -> ServeSpawner {
        self.env()
            .into_iter()
            .fold(ServeSpawner::new(PathBuf::from(shell)), |spawner, (key, value)| {
                spawner.env(key, value)
            })
    }

    fn status(&self, shell: &str) -> Result<Run, String> {
        run(self.remote(shell, &["status"]))
    }

    /// Waits until `status` says a `serve` runs, sandboxed; its pid.
    fn until_running(&self, shell: &str) -> Result<u32, String> {
        let until = Instant::now() + PATIENCE;
        loop {
            let status = self.status(shell)?;
            if status.code == Some(0) && status.stdout.contains("sandbox:") {
                return status
                    .stdout
                    .lines()
                    .find_map(|l| l.strip_prefix("running: pid "))
                    .and_then(|pid| pid.parse().ok())
                    .ok_or_else(|| status.says());
            }
            if Instant::now() > until {
                return Err(format!("serve was not reported running; {}", status.says()));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Waits until `status` says no `serve` runs.
    fn until_stopped(&self, shell: &str) -> Result<(), String> {
        let until = Instant::now() + PATIENCE;
        loop {
            let status = self.status(shell)?;
            if status.code == Some(3) {
                return Ok(());
            }
            if Instant::now() > until {
                return Err(format!("serve still runs; {}", status.says()));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    fn says(&self) -> String {
        format!(
            "exit {:?}; stdout: {}; stderr: {}",
            self.code,
            self.stdout.trim_end(),
            self.stderr.trim_end()
        )
    }
}

/// Runs `command` to its end, killed by its pid after [`PATIENCE`].
fn run(mut command: Command) -> Result<Run, String> {
    let child = command.spawn().map_err(|e| format!("did not start: {e}"))?;
    let pid = child.id();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(child.wait_with_output());
    });
    match receiver.recv_timeout(PATIENCE) {
        Ok(Ok(output)) => Ok(Run {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }),
        Ok(Err(e)) => Err(format!("waiting failed: {e}")),
        Err(_) => {
            // SAFETY: `kill` takes no pointers; the pid is this run's own child, not yet reaped by its waiter.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            Err(format!("still running after {} s", PATIENCE.as_secs()))
        }
    }
}

/// Runs `command` to its end, calling `each` meanwhile (a window must turn to answer it), killed by its pid after
/// [`PATIENCE`].
fn run_while(mut command: Command, mut each: impl FnMut() -> Result<(), String>) -> Result<Run, String> {
    let child = command.spawn().map_err(|e| format!("did not start: {e}"))?;
    let pid = child.id();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(child.wait_with_output());
    });
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Err(why) = each() {
            // SAFETY: `kill` takes no pointers; the pid is this run's own child, not yet reaped by its waiter.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            return Err(why);
        }
        match receiver.recv_timeout(Duration::from_millis(5)) {
            Ok(Ok(output)) => {
                return Ok(Run {
                    code: output.status.code(),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                })
            }
            Ok(Err(e)) => return Err(format!("waiting failed: {e}")),
            Err(_) if Instant::now() > deadline => {
                // SAFETY: as above.
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                return Err(format!("still running after {} s", PATIENCE.as_secs()));
            }
            Err(_) => {}
        }
    }
}

fn settings(enabled: bool) -> RemoteSettings {
    RemoteSettings {
        enabled,
        listen: RemoteListen::Socket,
        port: None,
        origins: Vec::new(),
        audit_keep_days: 90,
        notify: true,
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The journal: a datagram socket of the test's own, and the `journalctl` the CLI runs.

/// Receives what every writer sends the journal, in order.
struct Journal {
    socket: PathBuf,
    got: Arc<Mutex<Vec<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Journal {
    fn start(dir: &Path) -> Result<Journal, String> {
        let socket = dir.join("journal.sock");
        let bound = UnixDatagram::bind(&socket).map_err(|e| format!("the journal socket: {e}"))?;
        bound
            .set_read_timeout(Some(Duration::from_millis(50)))
            .map_err(|e| e.to_string())?;
        let got = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::spawn({
            let (got, stop) = (got.clone(), stop.clone());
            move || {
                let mut buf = vec![0u8; 64 * 1024];
                while !stop.load(Ordering::SeqCst) {
                    if let Ok(n) = bound.recv(&mut buf) {
                        got.lock().unwrap().push(buf[..n].to_vec());
                    }
                }
            }
        });
        Ok(Journal {
            socket,
            got,
            stop,
            thread: Some(thread),
        })
    }

    fn datagrams(&self) -> Vec<Vec<u8>> {
        self.got.lock().unwrap().clone()
    }

    /// The `EITRI_RECORD` of every datagram.
    fn records(&self) -> Vec<String> {
        self.datagrams()
            .iter()
            .filter_map(|d| decode(d))
            .filter_map(|fields| {
                fields
                    .into_iter()
                    .find(|(name, _)| name == "EITRI_RECORD")
                    .map(|(_, value)| String::from_utf8_lossy(&value).into_owned())
            })
            .collect()
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The fields of a native-protocol datagram: `NAME=value\n`, or `NAME\n`, eight bytes of length, the value, `\n`.
fn decode(mut bytes: &[u8]) -> Option<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    while !bytes.is_empty() {
        let end = bytes.iter().position(|b| *b == b'\n' || *b == b'=')?;
        let name = String::from_utf8(bytes[..end].to_vec()).ok()?;
        if bytes[end] == b'=' {
            let rest = &bytes[end + 1..];
            let stop = rest.iter().position(|b| *b == b'\n')?;
            out.push((name, rest[..stop].to_vec()));
            bytes = &rest[stop + 1..];
        } else {
            let rest = &bytes[end + 1..];
            let len = u64::from_le_bytes(rest.get(..8)?.try_into().ok()?) as usize;
            let value = rest.get(8..8 + len)?.to_vec();
            if rest.get(8 + len) != Some(&b'\n') {
                return None;
            }
            out.push((name, value));
            bytes = &rest[8 + len + 1..];
        }
    }
    Some(out)
}

/// A directory with a `journalctl` that prints the lines of `<dir>/entries.jsonl` and nothing else, so
/// `eitri remote log --verify` reads the test's journal and never the machine's. Its path.
fn journalctl_stub(dir: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).map_err(|e| e.to_string())?;
    let script = bin.join("journalctl");
    let body = format!("#!/bin/sh\ncat '{}'\n", dir.join("entries.jsonl").display());
    std::fs::write(&script, body).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    Ok(bin)
}

/// What the stub prints: one entry per record, sent by `exe`, then each of `forged` as sent by `/usr/bin/forger`.
fn write_entries(dir: &Path, exe: &str, records: &[String], forged: &[String]) -> Result<(), String> {
    let mut text = String::new();
    for (exe, record) in records
        .iter()
        .map(|r| (exe, r))
        .chain(forged.iter().map(|r| ("/usr/bin/forger", r)))
    {
        text.push_str(&json!({ "_EXE": exe, "EITRI_RECORD": record, "SYSLOG_IDENTIFIER": "eitri-remote" }).to_string());
        text.push('\n');
    }
    std::fs::write(dir.join("entries.jsonl"), text).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------------------------------------------
// A window: a real panel whose `rv-` socket it pumps on its own tick, its rows published.

/// A page that keeps every envelope it is sent.
#[derive(Default)]
struct RecordingPage {
    sent: RefCell<Vec<String>>,
}

impl PageSurface for RecordingPage {
    fn send(&self, envelope_json: &str) {
        self.sent.borrow_mut().push(envelope_json.to_owned());
    }
    fn is_visible(&self) -> bool {
        true
    }
    fn load_document(&self, _html: &str, _base_uri: &str) {}
    fn set_background(&self, _rgb: [u8; 3]) {}
}

struct Window {
    instance: String,
    handle: AgentPanelHandle,
    page: Rc<RecordingPage>,
    publisher: Option<Publisher>,
    keys: RefCell<Vec<RowKey>>,
    cwd: PathBuf,
}

struct Tab {
    id: TabId,
    key: RowKey,
    provider: Arc<RecordingProvider>,
}

fn project() -> ProjectRef {
    ProjectRef {
        key: "9b2c4e7a10f3d856".into(),
        name: "project".into(),
    }
}

impl Window {
    /// A window of `dirs` that starts remote control as a window with `settings` does: decides, binds, hands the
    /// socket to its panel, starts `serve` through `spawner` when the decision says to. Whether it started one.
    fn open(
        dirs: &Dirs,
        settings: &RemoteSettings,
        spawner: &ServeSpawner,
        label: &str,
    ) -> Result<(Window, bool), String> {
        let instance = new_instance();
        let page = Rc::new(RecordingPage::default());
        let handle = AgentPanelHandle::for_host_test(page.clone());
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r-ready"}"#);
        let window_ref = WindowRef {
            instance: instance.clone(),
            name: "1".into(),
            focused: true,
        };
        let publisher = Publisher::start(&dirs.control, &instance, project(), window_ref).map_err(|e| e.to_string())?;
        let window = Window {
            instance,
            handle,
            page,
            publisher: Some(publisher),
            keys: RefCell::new(Vec::new()),
            cwd: agent::state_dirs::test_workspace_dir(label),
        };
        let decided = plan(settings, dirs);
        if let RemotePlan::On { bind_rv: true, .. } = decided {
            let socket = RemoteSocket::bind_waking(
                &dirs.control,
                &window.instance,
                &dirs.state.join("devices.json"),
                window.handle.remote_waker(),
            )
            .map_err(|e| format!("the rv- socket did not bind: {e}"))?;
            let direct = Endpoint {
                mode: PairMode::Direct,
                scheme: PairScheme::Http,
                host: "127.0.0.1".into(),
                port: 41000,
            };
            socket.set_pairing(Pairing {
                routes: PairingRoutes {
                    ssh: None,
                    direct: Some(direct),
                },
                ssh_dir: dirs.state.clone(),
            });
            window.handle.set_remote(socket);
        }
        let started = start_serve(&decided, spawner).map_err(|e| format!("serve did not start: {e}"))?;
        window.publish();
        Ok((window, started))
    }

    fn publish(&self) {
        let rows = self
            .keys
            .borrow()
            .iter()
            .enumerate()
            .map(|(at, key)| Row {
                id: format!("{}.{}", self.instance, at + 1),
                key: key.as_str().to_owned(),
                agent: "claude".into(),
                project: project(),
                window: WindowRef {
                    instance: self.instance.clone(),
                    name: "1".into(),
                    focused: true,
                },
                number: at as u16 + 1,
                state: "idle".into(),
                ..Row::default()
            })
            .collect();
        self.publisher.as_ref().unwrap().offer_now(rows);
    }

    fn tab(&self, provider: RecordingProvider) -> Tab {
        let provider = Arc::new(provider);
        let (id, key) = self.handle.open_live_tab_for_host_test(provider.clone(), &self.cwd);
        self.keys.borrow_mut().push(key.clone());
        self.publish();
        Tab { id, key, provider }
    }

    /// A tab of `agent` that holds no session yet, its row published.
    fn unstarted_tab(&self, agent: AgentId) -> RowKey {
        let key = self.handle.open_unstarted_tab_for_host_test(agent);
        self.keys.borrow_mut().push(key.clone());
        self.publish();
        key
    }

    /// A key a published row names that no tab of this window holds, as a tab's is for the moment after it closes.
    fn ghost_key(&self) -> RowKey {
        let key = RowKey::new();
        self.keys.borrow_mut().push(key.clone());
        self.publish();
        key
    }

    /// The desk cycles `tab`'s mode (and confirms the Bypass question) until it is `wanted`.
    fn set_mode(&self, tab: &Tab, wanted: SessionModeChoice) -> Result<(), String> {
        for round in 0..4 {
            if self.handle.mode_for_host_test(tab.id) == wanted {
                return Ok(());
            }
            let asked = self.envelopes("confirm_bypass").len();
            self.handle.page_message(
                ViewId::BASE,
                &format!(r#"{{"type":"cycle_mode","request_id":"m{round}","tab":{}}}"#, tab.id.0),
            );
            self.turn();
            if let Some(prompt) = self.envelopes("confirm_bypass").get(asked) {
                let nonce = prompt["nonce"].as_u64().ok_or("no nonce in the Bypass question")?;
                self.handle.page_message(
                    ViewId::BASE,
                    &format!(
                        r#"{{"type":"confirm_bypass","request_id":"y{round}","tab":{},"scope":"tab","nonce":{nonce}}}"#,
                        tab.id.0
                    ),
                );
            }
        }
        check(self.handle.mode_for_host_test(tab.id) == wanted, || {
            format!("the tab did not reach {wanted:?}")
        })
    }

    /// One tick, which pumps the `rv-` socket the panel holds.
    fn turn(&self) {
        self.handle.tick();
    }

    /// Turns until `done`.
    fn until(&self, what: &str, done: impl Fn() -> bool) -> Result<(), String> {
        let deadline = Instant::now() + PATIENCE;
        while !done() {
            if Instant::now() > deadline {
                return Err(format!("timed out waiting for {what}"));
            }
            self.turn();
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }

    fn start_turn(&self, tab: &Tab, turn: &str) -> Result<(), String> {
        tab.provider
            .queue(agent::AgentDomainEvent::TurnStarted { turn_id: turn.into() });
        self.until(&format!("the turn {turn} to start"), || {
            self.handle.tab_progress_for_host_test(tab.id).1.as_deref() == Some(turn)
        })
    }

    /// Turns until the page was sent a `kind` envelope after `after` of them; that envelope.
    fn asked(&self, kind: &str, after: usize) -> Result<Value, String> {
        self.until(&format!("the page to be sent {kind}"), || {
            self.envelopes(kind).len() > after
        })?;
        Ok(self.envelopes(kind).remove(after))
    }

    fn envelopes(&self, kind: &str) -> Vec<Value> {
        self.page
            .sent
            .borrow()
            .iter()
            .filter_map(|e| serde_json::from_str::<Value>(e).ok())
            .filter(|v| v["kind"] == kind)
            .collect()
    }

    /// The desk answers the question the page was just asked with a yes or a no.
    fn answer(&self, nonce: u64, yes: bool, grant: &str) {
        self.handle.page_message(
            ViewId::BASE,
            &format!(
                r#"{{"type":"remote_enroll_answer","request_id":"a{nonce}","nonce":{nonce},"yes":{yes},"grant":"{grant}"}}"#
            ),
        );
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // As a window's close does: the socket goes with the panel.
        let _watch = self.handle.shutdown();
        if let Some(publisher) = self.publisher.take() {
            publisher.stop();
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The phone: a WebSocket client over `serve`'s Unix socket.

struct Phone {
    stream: UnixStream,
    buf: Vec<u8>,
    seq: u64,
    seen: u64,
    fragments: Vec<u8>,
}

/// A handshake request for `path` with the given `Host` and optional `Origin`.
fn handshake(path: &str, host: &str, origin: Option<&str>) -> Vec<u8> {
    let origin = origin.map(|o| format!("Origin: {o}\r\n")).unwrap_or_default();
    format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\n{origin}Upgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
    .into_bytes()
}

impl Phone {
    fn connect(dirs: &Dirs) -> Result<Phone, String> {
        let path = serve_socket_path(&dirs.control).map_err(|e| e.to_string())?;
        let mut stream = UnixStream::connect(&path).map_err(|e| format!("serve's socket: {e}"))?;
        stream
            .write_all(&handshake("/v1/ws", "localhost", None))
            .map_err(|e| e.to_string())?;
        stream.set_read_timeout(Some(PATIENCE)).unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).map_err(|e| format!("no 101: {e}"))?;
            head.push(byte[0]);
        }
        check(head.starts_with(b"HTTP/1.1 101 "), || {
            String::from_utf8_lossy(&head).into_owned()
        })?;
        Ok(Phone {
            stream,
            buf: Vec::new(),
            seq: 0,
            seen: 0,
            fragments: Vec::new(),
        })
    }

    fn send(&mut self, mut body: Value) {
        self.seq += 1;
        body["v"] = json!(1);
        body["seq"] = json!(self.seq);
        body["ack"] = json!(self.seen);
        let payload = body.to_string().into_bytes();
        let key = [0x11u8, 0x22, 0x33, 0x44];
        let mut bytes = vec![0x81];
        match payload.len() {
            n @ 0..=125 => bytes.push(0x80 | n as u8),
            n if n <= u16::MAX as usize => {
                bytes.push(0x80 | 126);
                bytes.extend((n as u16).to_be_bytes());
            }
            n => {
                bytes.push(0x80 | 127);
                bytes.extend((n as u64).to_be_bytes());
            }
        }
        bytes.extend_from_slice(&key);
        bytes.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i % 4]));
        let _ = self.stream.write_all(&bytes);
    }

    fn hello(&mut self, device: &str, token: Option<&str>) {
        let mut body = json!({ "kind": "hello", "device": device, "layout": "slab", "protocol": 1 });
        if let Some(token) = token {
            body["token"] = json!(token);
        }
        self.send(body);
    }

    /// The next whole text message; `Ok(None)` when none came within `within`, `Err` once the server closed.
    fn recv(&mut self, within: Duration) -> Result<Option<Value>, String> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(message) = self.take_message()? {
                let value: Value = serde_json::from_slice(&message).map_err(|e| format!("not JSON: {e}"))?;
                if let Some(seq) = value["seq"].as_u64() {
                    self.seen = seq;
                }
                return Ok(Some(value));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            self.stream
                .set_read_timeout(Some(left.max(Duration::from_millis(1))))
                .unwrap();
            let mut chunk = [0u8; 64 * 1024];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err("closed".into()),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(e) => return Err(format!("closed: {e}")),
            }
        }
    }

    fn take_message(&mut self) -> Result<Option<Vec<u8>>, String> {
        loop {
            if self.buf.len() < 2 {
                return Ok(None);
            }
            let (first, second) = (self.buf[0], self.buf[1]);
            let (len, head) = match second & 0x7f {
                126 if self.buf.len() >= 4 => (u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize, 4),
                127 if self.buf.len() >= 10 => (u64::from_be_bytes(self.buf[2..10].try_into().unwrap()) as usize, 10),
                126 | 127 => return Ok(None),
                n => (n as usize, 2),
            };
            if self.buf.len() < head + len {
                return Ok(None);
            }
            let payload: Vec<u8> = self.buf[head..head + len].to_vec();
            self.buf.drain(..head + len);
            match first & 0x0f {
                0x1 | 0x0 => {
                    self.fragments.extend_from_slice(&payload);
                    if first & 0x80 != 0 {
                        return Ok(Some(std::mem::take(&mut self.fragments)));
                    }
                }
                0x8 => return Err("closed".into()),
                0x9 | 0xa => {}
                other => return Err(format!("opcode {other}")),
            }
        }
    }

    /// Turns `windows` and reads until a message `done` accepts; every message read, that one last.
    fn until(&mut self, windows: &[&Window], what: &str, done: impl Fn(&Value) -> bool) -> Result<Vec<Value>, String> {
        let deadline = Instant::now() + PATIENCE;
        let mut got: Vec<Value> = Vec::new();
        loop {
            if Instant::now() > deadline {
                let kinds: Vec<String> = got.iter().map(|v| v["kind"].to_string()).collect();
                return Err(format!("timed out waiting for {what}; got {}", kinds.join(" ")));
            }
            for window in windows {
                window.turn();
            }
            while let Some(message) = self
                .recv(Duration::from_millis(5))
                .map_err(|e| format!("{what}: {e}"))?
            {
                let found = done(&message);
                got.push(message);
                if found {
                    return Ok(got);
                }
            }
        }
    }

    /// Turns `windows` until the server closes this connection.
    fn until_closed(&mut self, windows: &[&Window], what: &str) -> Result<(), String> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            for window in windows {
                window.turn();
            }
            match self.recv(Duration::from_millis(20)) {
                Err(_) => return Ok(()),
                Ok(_) if Instant::now() > deadline => return Err(format!("still connected after {what}")),
                Ok(_) => {}
            }
        }
    }

    /// Like [`Self::act`], for a key whose row `serve` may not have read yet: until it has, `serve` answers "no such
    /// tab" itself and records nothing, so the act is sent again, under a new command id, until `logged` says a window
    /// recorded the refusal (waited for a moment after each "no such tab", which a window says too).
    fn act_routed(
        &mut self,
        windows: &[&Window],
        key: &RowKey,
        cmd: Value,
        logged: impl Fn() -> bool,
    ) -> Result<Value, String> {
        let until = Instant::now() + PATIENCE;
        let id = cmd["command_id"].as_str().unwrap_or_default().to_owned();
        for attempt in 0.. {
            let mut cmd = cmd.clone();
            cmd["command_id"] = json!(format!("{id}-{attempt}"));
            let result = self.act(windows, key, cmd)?;
            if result["why"] != "no such tab" {
                return Ok(result);
            }
            let settle = Instant::now() + Duration::from_secs(1);
            while Instant::now() < settle {
                if logged() {
                    return Ok(result);
                }
                for window in windows {
                    window.turn();
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            if Instant::now() > until {
                break;
            }
        }
        Err(format!("serve never routed an act to {}", key.as_str()))
    }

    /// Sends `act` for `key` with `cmd` and turns until its `result`; that line.
    fn act(&mut self, windows: &[&Window], key: &RowKey, cmd: Value) -> Result<Value, String> {
        let id = cmd["command_id"].as_str().unwrap_or_default().to_owned();
        self.send(json!({ "kind": "act", "key": key.as_str(), "cmd": cmd }));
        let lines = self.until(windows, &format!("the result of {id}"), |l| {
            l["kind"] == "result" && (id.is_empty() || l["command_id"] == id.as_str())
        })?;
        Ok(lines.last().unwrap().clone())
    }
}

/// A handshake `serve` refuses, sent over its Unix socket; what it answered, if anything.
fn refused_handshake(dirs: &Dirs, request: &[u8]) -> Result<String, String> {
    let path = serve_socket_path(&dirs.control).map_err(|e| e.to_string())?;
    let mut stream = UnixStream::connect(&path).map_err(|e| format!("serve's socket: {e}"))?;
    stream.write_all(request).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    let mut answer = Vec::new();
    let _ = stream.read_to_end(&mut answer);
    Ok(String::from_utf8_lossy(&answer).into_owned())
}

// ---------------------------------------------------------------------------------------------------------------
// The log, as the test reads it.

fn records(home: &Home) -> Vec<Record> {
    let mut all = read(&home.audit(), &Filter::default(), None).unwrap_or_default();
    all.reverse();
    all
}

/// Who wrote a record.
#[derive(Clone, Copy)]
enum By<'a> {
    Serve,
    Cli,
    Window(&'a str),
}

impl By<'_> {
    fn is(&self, record: &Record) -> bool {
        match self {
            By::Serve => record.writer == "serve",
            By::Cli => record.writer == "cli",
            By::Window(instance) => record.writer == *instance,
        }
    }
}

fn field<'a>(record: &'a Record, name: &str) -> &'a str {
    record.fields.get(name).and_then(Value::as_str).unwrap_or_default()
}

/// How many records of `event` the writer `by` wrote that `which` accepts.
fn count(all: &[Record], by: By, event: &str, which: impl Fn(&Record) -> bool) -> usize {
    all.iter().filter(|r| r.event == event && by.is(r) && which(r)).count()
}

/// Waits until the log holds `n` records of `event` by `by` that `which` accepts; windows turn meanwhile.
fn wait_for(
    home: &Home,
    windows: &[&Window],
    what: &str,
    by: By,
    event: &str,
    n: usize,
    which: impl Fn(&Record) -> bool,
) -> Result<(), String> {
    let until = Instant::now() + PATIENCE;
    loop {
        for window in windows {
            window.turn();
        }
        let all = records(home);
        if count(&all, by, event, &which) >= n {
            return Ok(());
        }
        if Instant::now() > until {
            return Err(format!(
                "no {what} in the log: {:?}",
                all.iter().map(|r| r.event.as_str()).collect::<Vec<_>>()
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The records of `event` by `by` that `which` accepts number exactly `n`.
fn exactly(
    all: &[Record],
    what: &str,
    by: By,
    event: &str,
    n: usize,
    which: impl Fn(&Record) -> bool,
) -> Result<(), String> {
    let found = count(all, by, event, which);
    check(found == n, || {
        let lines: Vec<&str> = all
            .iter()
            .filter(|r| r.event == event)
            .map(|r| r.line.as_str())
            .collect();
        format!("{what}: {found} records, wanted {n}; every {event}: {lines:#?}")
    })
}

// ---------------------------------------------------------------------------------------------------------------
// The checks.

/// With remote control off, no window and no `eitri remote` command writes a log, makes the audit directory or sends
/// the journal anything.
fn remote_off_writes_nothing(shell: &str, dir: &Path) -> Result<(), String> {
    let journal = Journal::start(dir)?;
    std::env::set_var(JOURNAL_SOCKET_VAR, &journal.socket);
    let bin = journalctl_stub(dir)?;
    let home = Home::new(dir).with_journal(&journal.socket, &bin);
    let dirs = home.dirs();
    let spawner = home.spawner(shell);
    {
        let (window, started) = Window::open(&dirs, &settings(false), &spawner, "rae-off")?;
        check(!started, || "a serve was started".into())?;
        window.tab(RecordingProvider::interruptible());
        for _ in 0..100 {
            window.turn();
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    let revoked = run(home.remote(shell, &["revoke", "nobody"]))?;
    check(revoked.code == Some(1), || format!("revoke: {}", revoked.says()))?;
    std::thread::sleep(Duration::from_millis(300));
    check(!home.audit().exists(), || {
        format!("{} exists with remote control off", home.audit().display())
    })?;
    check(journal.datagrams().is_empty(), || {
        "the journal was sent something with remote control off".into()
    })
}

/// `eitri remote add-device <name>` while `window` turns: the code its page was shown.
fn add_device(shell: &str, home: &Home, window: &Window, name: &str) -> Result<String, String> {
    let before = window.envelopes("remote_pairing").len();
    let added = run_while(home.remote(shell, &["add-device", name]), || {
        window.turn();
        Ok(())
    })?;
    check(added.code == Some(0) && added.stdout.contains("Scan the code"), || {
        format!("add-device: {}", added.says())
    })?;
    let shown = window.asked("remote_pairing", before)?;
    shown["code"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("no code in {shown}"))
}

/// A phone presents `code` as `name`; what the desk is asked, if anything, with the phone.
fn present(home: &Home, window: &Window, name: &str, code: &str) -> Result<Phone, String> {
    let mut phone = Phone::connect(&home.dirs())?;
    phone.hello("", None);
    phone.send(json!({
        "kind": "enroll", "instance": window.instance, "code": code, "device_name": name, "recipe": "direct"
    }));
    Ok(phone)
}

/// A phone enrolls as `name` with a fresh code and the desk answers `yes` with `grant`: the device, its token and the
/// code it used when it was enrolled.
fn enroll(
    shell: &str,
    home: &Home,
    window: &Window,
    name: &str,
    yes: bool,
    grant: &str,
) -> Result<Option<(String, String, String)>, String> {
    let code = add_device(shell, home, window, name)?;
    let before = window.envelopes("remote_enroll").len();
    let mut phone = present(home, window, name, &code)?;
    let asked = window.asked("remote_enroll", before)?;
    check(asked["name"] == name, || format!("the desk was asked {asked}"))?;
    let nonce = asked["nonce"].as_u64().ok_or_else(|| format!("no nonce in {asked}"))?;
    window.answer(nonce, yes, grant);
    let lines = phone.until(&[window], "the enrollment's end", |l| {
        l["kind"] == "enrolled" || l["kind"] == "enroll_refused"
    })?;
    let last = lines.last().unwrap();
    if last["kind"] != "enrolled" {
        return Ok(None);
    }
    Ok(Some((
        last["device"].as_str().ok_or("no device")?.to_owned(),
        last["token"].as_str().ok_or("no token")?.to_owned(),
        code,
    )))
}

fn result_ok(result: &Value, what: &str) -> Result<(), String> {
    check(result["ok"] == true, || format!("{what}: {result}"))
}

fn result_refused(result: &Value, what: &str) -> Result<(), String> {
    check(result["ok"] == false, || format!("{what} was not refused: {result}"))
}

fn the_audit_trail_end_to_end(shell: &str, dir: &Path) -> Result<(), String> {
    let journal = Journal::start(dir)?;
    std::env::set_var(JOURNAL_SOCKET_VAR, &journal.socket);
    let bin = journalctl_stub(dir)?;
    let home = Home::new(dir).with_journal(&journal.socket, &bin);
    let dirs = home.dirs();
    let spawner = home.spawner(shell);

    // ----- A window starts `serve`; two tabs, one that takes turns and one that runs background work.
    let (window, started) = Window::open(&dirs, &settings(true), &spawner, "rae-trail")?;
    check(started, || "the window started no serve".into())?;
    home.until_running(shell)?;
    // Tabs for the refusals that need them: one with no session yet, a Codex tab, and a row no tab stands behind.
    let unstarted = window.unstarted_tab(AgentId::Claude);
    let codex = window.unstarted_tab(AgentId::Codex);
    let ghost = window.ghost_key();
    let tab_a = window.tab(RecordingProvider::interruptible());
    let tab_b = window.tab(RecordingProvider::task_controlling());
    // The tab whose mode the desk cycles is the last one opened, the one its page shows.
    let tab_c = window.tab(RecordingProvider::interruptible());
    let w: &[&Window] = &[&window];
    let me = By::Window(&window.instance);

    // ----- Enrollment: a code that lapsed because another was shown, a wrong one, a no, and two yeses.
    let stale = add_device(shell, &home, &window, "Pixel")?;
    let fresh = add_device(shell, &home, &window, "Pixel")?;
    wait_for(&home, w, "the lapse of the first code", me, "enroll_expired", 1, |_| {
        true
    })?;
    for (name, code) in [("Stale", stale.as_str()), ("Typo", "not-the-code")] {
        let mut phone = present(&home, &window, name, code)?;
        let lines = phone.until(w, "the refusal", |l| l["kind"] == "enroll_refused")?;
        check(lines.last().unwrap()["kind"] == "enroll_refused", || {
            "no refusal".into()
        })?;
    }
    // The code still on show goes to a phone the desk says no to.
    let before = window.envelopes("remote_enroll").len();
    let mut declined = present(&home, &window, "Declined", &fresh)?;
    let asked = window.asked("remote_enroll", before)?;
    window.answer(asked["nonce"].as_u64().ok_or("no nonce")?, false, "watch");
    declined.until(w, "the no", |l| l["kind"] == "enroll_refused")?;
    let (steer_id, steer_token, steer_code) =
        enroll(shell, &home, &window, "Pixel", true, "watch+steer")?.ok_or("Pixel not enrolled")?;
    let (watch_id, watch_token, watch_code) =
        enroll(shell, &home, &window, "Viewer", true, "watch")?.ok_or("Viewer not enrolled")?;

    // ----- The refusals only `serve` makes, before any device is proved and at the hello.
    let host = refused_handshake(&dirs, &handshake("/v1/ws", "evil.example", None))?;
    check(!host.starts_with("HTTP/1.1 101"), || {
        format!("a foreign Host was let in: {host}")
    })?;
    let origin = refused_handshake(&dirs, &handshake("/v1/ws", "localhost", Some("https://evil.example")))?;
    check(!origin.starts_with("HTTP/1.1 101"), || {
        format!("a foreign Origin was let in: {origin}")
    })?;
    let path = refused_handshake(&dirs, &handshake("/other", "localhost", None))?;
    check(!path.starts_with("HTTP/1.1 101"), || {
        format!("another path was let in: {path}")
    })?;
    let mut wrong = Phone::connect(&dirs)?;
    wrong.hello(&steer_id, Some("not-the-token"));
    wrong.until_closed(w, "a wrong token")?;

    // ----- The steering phone: every command kind, and the refusals a window decides.
    let mut phone = Phone::connect(&dirs)?;
    phone.hello(&steer_id, Some(&steer_token));
    for tab in [&tab_a, &tab_b] {
        phone.send(json!({ "kind": "subscribe", "key": tab.key.as_str() }));
        phone.until(w, "a snapshot", |l| {
            l["kind"] == "snapshot" && l["key"] == tab.key.as_str()
        })?;
    }
    phone.send(json!({ "kind": "set_push_endpoint", "url": format!("https://ntfy.example.org/{PUSH_PATH}") }));
    wait_for(
        &home,
        w,
        "the push endpoint's record",
        By::Serve,
        "push_endpoint_set",
        1,
        |_| true,
    )?;

    // send, then a turn runs; queue behind it; take it back; a second take_back finds nothing of the phone's.
    let sent = phone.act(
        w,
        &tab_a.key,
        json!({ "type": "send", "command_id": "c-send", "text": format!("{SENTINEL} first") }),
    )?;
    result_ok(&sent, "send")?;
    window.start_turn(&tab_a, "t1")?;
    let generation = window.handle.tab_progress_for_host_test(tab_a.id).0;
    let queued = phone.act(
        w,
        &tab_a.key,
        json!({ "type": "queue", "command_id": "c-queue", "text": format!("{SENTINEL} queued") }),
    )?;
    result_ok(&queued, "queue")?;
    let taken = phone.act(w, &tab_a.key, json!({ "type": "take_back", "command_id": "c-take" }))?;
    result_ok(&taken, "take_back")?;
    let nothing = phone.act(
        w,
        &tab_a.key,
        json!({ "type": "take_back", "command_id": "c-take-again" }),
    )?;
    result_refused(&nothing, "a second take_back")?;
    // The work named is not the work running; the same command id twice; a slash; a text too long; a kind a phone
    // may not send.
    let moved = phone.act(
        w,
        &tab_a.key,
        json!({
            "type": "interrupt", "command_id": "c-stale",
            "expect_work": { "turn": { "id": "t-other", "generation": generation } },
        }),
    )?;
    result_refused(&moved, "an interrupt of other work")?;
    let again = phone.act(
        w,
        &tab_a.key,
        json!({ "type": "send", "command_id": "c-send", "text": format!("{SENTINEL} again") }),
    )?;
    result_refused(&again, "a command id used twice")?;
    let slash = phone.act(
        w,
        &tab_a.key,
        json!({ "type": "send", "command_id": "c-slash", "text": "/clear" }),
    )?;
    result_refused(&slash, "a slash command")?;
    let long = phone.act(
        w,
        &tab_a.key,
        json!({ "type": "send", "command_id": "c-long", "text": "x".repeat(64 * 1024 + 1) }),
    )?;
    result_refused(&long, "a text too long")?;
    let unknown = phone.act(w, &tab_a.key, json!({ "type": "allow", "command_id": "c-allow" }))?;
    result_refused(&unknown, "a kind a phone may not send")?;
    // Interrupt the running turn.
    let stopped = phone.act(
        w,
        &tab_a.key,
        json!({
            "type": "interrupt", "command_id": "c-stop",
            "expect_work": { "turn": { "id": "t1", "generation": generation } },
        }),
    )?;
    result_ok(&stopped, "interrupt")?;
    check(tab_a.provider.interrupts() == 1, || {
        format!("the provider was interrupted {} times", tab_a.provider.interrupts())
    })?;
    // Background work on the other tab: one task, then all of it.
    let revision = window.handle.tab_progress_for_host_test(tab_b.id).2;
    tab_b.provider.queue_all(vec![
        agent::AgentDomainEvent::BackgroundTasksChanged {
            tasks: vec![agent::BackgroundTask {
                task_id: "b7".into(),
                run_id: None,
                kind: agent::TaskKind::Shell,
                task_type: "shell".into(),
                description: "watch the build".into(),
                subagent_type: None,
                parent_task_id: None,
                tool_use_id: None,
                ambient: false,
                workflow_name: None,
            }],
            occurred_at: None,
        },
        agent::AgentDomainEvent::BackgroundCalls { running: 1 },
    ]);
    window.until("the background task", || {
        window.handle.tab_progress_for_host_test(tab_b.id).2 > revision
    })?;
    let task = phone.act(
        w,
        &tab_b.key,
        json!({ "type": "stop_task", "command_id": "c-task", "task": "b7" }),
    )?;
    result_ok(&task, "stop_task")?;
    let background = phone.act(
        w,
        &tab_b.key,
        json!({ "type": "stop_background", "command_id": "c-bg" }),
    )?;
    result_ok(&background, "stop_background")?;
    // The agent turns a stop down: the other tab's provider cannot interrupt.
    window.start_turn(&tab_b, "tb1")?;
    let generation_b = window.handle.tab_progress_for_host_test(tab_b.id).0;
    let agent = phone.act(
        w,
        &tab_b.key,
        json!({
            "type": "interrupt", "command_id": "c-agent",
            "expect_work": { "turn": { "id": "tb1", "generation": generation_b } },
        }),
    )?;
    result_refused(&agent, "a stop the agent cannot carry out")?;
    // Tabs a send cannot reach: no tab behind the key, a Claude tab with no session yet, a Codex tab.
    for (key, id, reason, what) in [
        (&ghost, "c-ghost", "tab_not_live", "a send to a tab that is gone"),
        (
            &unstarted,
            "c-unstarted",
            "not_started",
            "a send to a tab with no session",
        ),
        (&codex, "c-codex", "not_claude", "a send to a tab that is not Claude"),
    ] {
        let logged = || {
            count(&records(&home), By::Window(&window.instance), "refused", |r| {
                field(r, "reason") == reason
            }) >= 1
        };
        let refused = phone.act_routed(
            w,
            key,
            json!({ "type": "send", "command_id": id, "text": format!("{SENTINEL} {id}") }),
            logged,
        )?;
        result_refused(&refused, what)?;
    }
    // A send waits on the provider; a second one to the same tab is refused as busy meanwhile.
    window.set_mode(&tab_c, SessionModeChoice::Manual)?;
    let release = tab_c.provider.hold_calls();
    phone.send(json!({
        "kind": "act", "key": tab_c.key.as_str(),
        "cmd": { "type": "send", "command_id": "c-held", "text": format!("{SENTINEL} held") },
    }));
    window.until("the held send to reach the provider", || {
        tab_c.provider.calls_waiting() >= 1
    })?;
    let busy = phone.act(
        w,
        &tab_c.key,
        json!({ "type": "send", "command_id": "c-busy", "text": format!("{SENTINEL} busy") }),
    )?;
    result_refused(&busy, "a send while another waits on the provider")?;
    drop(release);
    let held = phone.until(w, "the held send's result", |l| {
        l["kind"] == "result" && l["command_id"] == "c-held"
    })?;
    result_ok(held.last().unwrap(), "the held send")?;
    // A message queued behind a running turn is refused when the tab's mode widened before the turn ended.
    window.start_turn(&tab_c, "tc1")?;
    let widened = phone.act(
        w,
        &tab_c.key,
        json!({ "type": "queue", "command_id": "c-widen", "text": format!("{SENTINEL} widen") }),
    )?;
    result_ok(&widened, "queue on tab c")?;
    window.set_mode(&tab_c, SessionModeChoice::Auto)?;
    tab_c.provider.queue(agent::AgentDomainEvent::TurnCompleted {
        turn_id: "tc1".into(),
        outcome: agent::TurnOutcome::Completed,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
        detail: Default::default(),
    });
    phone.until(w, "the queued message's refusal", |l| {
        l["kind"] == "result" && l["command_id"] == "c-widen" && l["ok"] == false
    })?;
    // In Bypass a phone's send is refused outright.
    window.set_mode(&tab_c, SessionModeChoice::Bypass)?;
    let bypassed = phone.act(
        w,
        &tab_c.key,
        json!({ "type": "send", "command_id": "c-bypass", "text": format!("{SENTINEL} bypass") }),
    )?;
    result_refused(&bypassed, "a send to a tab in Bypass")?;
    // The phone reads its own records: only its own, and none holds text.
    phone.send(json!({ "kind": "audit_mine", "limit": 50 }));
    let lines = phone.until(w, "its records", |l| l["kind"] == "audit_mine")?;
    let mine = lines.last().unwrap()["records"].as_array().cloned().unwrap_or_default();
    check(!mine.is_empty(), || "audit_mine held no records".into())?;
    check(mine.iter().all(|r| r["device"] == steer_id.as_str()), || {
        format!("audit_mine holds another device's record: {mine:?}")
    })?;
    check(mine.iter().any(|r| r["event"] == "cmd"), || {
        format!("audit_mine holds no cmd record: {mine:?}")
    })?;

    // ----- The watching phone: its act is refused by `serve`, the grant is raised at the desk, a CLI revoke takes it out.
    let mut watcher = Phone::connect(&dirs)?;
    watcher.hello(&watch_id, Some(&watch_token));
    watcher.send(json!({ "kind": "subscribe", "key": tab_a.key.as_str() }));
    watcher.until(w, "a snapshot", |l| l["kind"] == "snapshot")?;
    let denied = watcher.act(
        w,
        &tab_a.key,
        json!({ "type": "send", "command_id": "w-send", "text": format!("{SENTINEL} watcher") }),
    )?;
    result_refused(&denied, "an act of a watch-only device")?;
    let mut answered = false;
    let granted = run_while(home.remote(shell, &["grant", &watch_id, "steer"]), || {
        window.turn();
        if !answered {
            if let Some(asked) = window.envelopes("remote_grant").first() {
                window.answer(asked["nonce"].as_u64().ok_or("no nonce")?, true, "watch+steer");
                answered = true;
            }
        }
        Ok(())
    })?;
    check(granted.code == Some(0), || format!("grant: {}", granted.says()))?;
    let revoked = run(home.remote(shell, &["revoke", &watch_id]))?;
    check(revoked.code == Some(0), || format!("revoke: {}", revoked.says()))?;
    watcher.until_closed(w, "a revoke at the terminal")?;

    // ----- The steering phone takes itself out; its token is then a revoked one.
    phone.send(json!({ "kind": "revoke_self" }));
    phone.until_closed(w, "revoking itself")?;
    let mut gone = Phone::connect(&dirs)?;
    gone.hello(&steer_id, Some(&steer_token));
    gone.until_closed(w, "a revoked token")?;
    wait_for(&home, w, "the revoked token's refusal", By::Serve, "refused", 1, |r| {
        field(r, "reason") == "revoked_token"
    })?;

    // ----- `serve` ends and another starts: the log goes on in one chain.
    let stopped = run(home.remote(shell, &["stop"]))?;
    check(stopped.code == Some(0), || format!("stop: {}", stopped.says()))?;
    home.until_stopped(shell)?;
    drop(window);
    let (second, restarted) = Window::open(&dirs, &settings(true), &spawner, "rae-trail-2")?;
    check(restarted, || "the second window started no serve".into())?;
    home.until_running(shell)?;
    let stopped = run(home.remote(shell, &["stop"]))?;
    check(stopped.code == Some(0), || format!("second stop: {}", stopped.says()))?;
    home.until_stopped(shell)?;
    drop(second);

    // ----- Every record reached the journal, and the log holds what the actions made, by their writers.
    let until = Instant::now() + PATIENCE;
    loop {
        if journal.records().len() >= records(&home).len() && !records(&home).is_empty() {
            break;
        }
        if Instant::now() > until {
            return Err(format!(
                "the journal got {} records for {} in the log",
                journal.records().len(),
                records(&home).len()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let all = records(&home);
    let lines: Vec<String> = all.iter().map(|r| r.line.clone()).collect();
    let mut sent = journal.records();
    let mut kept = lines.clone();
    sent.sort();
    kept.sort();
    check(sent == kept, || {
        format!(
            "the journal's records are not the log's: {} against {}",
            sent.len(),
            kept.len()
        )
    })?;
    let first = all.first().map(|r| r.writer.clone()).unwrap_or_default();
    check(first == "serve", || format!("the log opens with a {first} record"))?;

    let me = By::Window(
        &all.iter()
            .find(|r| r.event == "enroll_offered")
            .ok_or("no enroll_offered")?
            .writer,
    );
    let serve = By::Serve;
    let any = |_: &Record| true;
    let reason = |word: &'static str| move |r: &Record| field(r, "reason") == word;
    let kind = |word: &'static str| move |r: &Record| field(r, "kind") == word;
    // `serve`: its two runs, each connection, the refusals it makes.
    exactly(&all, "serve's enables", serve, "enable", 2, any)?;
    exactly(&all, "serve's disables", serve, "disable", 2, any)?;
    exactly(&all, "connections opened", serve, "conn_open", 2, any)?;
    exactly(&all, "connections closed", serve, "conn_close", 2, any)?;
    exactly(&all, "the push endpoint", serve, "push_endpoint_set", 1, |r| {
        field(r, "host") == "ntfy.example.org"
    })?;
    for word in ["bad_token", "revoked_token", "host", "origin", "path_prefix", "grant"] {
        exactly(
            &all,
            &format!("serve's {word} refusals"),
            serve,
            "refused",
            1,
            reason(word),
        )?;
    }
    // The window: enrollment and the desk's answers.
    exactly(&all, "codes shown", me, "enroll_offered", 4, any)?;
    exactly(&all, "codes that lapsed", me, "enroll_expired", 1, any)?;
    exactly(&all, "enrollments confirmed", me, "enroll_confirmed", 2, any)?;
    exactly(&all, "enrollments the desk refused", me, "enroll_refused", 1, |r| {
        field(r, "why") == "desk_said_no"
    })?;
    exactly(
        &all,
        "wrong or stale codes",
        me,
        "refused",
        2,
        reason("enroll_code_invalid"),
    )?;
    exactly(&all, "grants changed", me, "grant_changed", 1, any)?;
    exactly(&all, "revokes by the device", me, "revoke", 1, |r| {
        field(r, "by") == "device"
    })?;
    exactly(&all, "revokes at the terminal", By::Cli, "revoke", 1, |r| {
        field(r, "by") == "cli"
    })?;
    // The window: every command kind once, and the refusals it decides.
    for (word, n) in [
        ("send", 2),
        ("queue", 2),
        ("take_back", 1),
        ("stop_task", 1),
        ("stop_background", 1),
    ] {
        exactly(&all, &format!("{word} commands"), me, "cmd", n, kind(word))?;
    }
    // A stop is a command when the window takes it, and a refusal again when the agent cannot carry it out: the stop
    // that worked, and the one the agent turned down.
    exactly(&all, "interrupt commands", me, "cmd", 2, kind("interrupt"))?;
    for word in [
        "not_allowed",
        "slash_or_bang",
        "too_long",
        "duplicate_command",
        "work_changed",
        "not_yours",
        "agent",
        "tab_not_live",
        "not_started",
        "not_claude",
        "busy",
        "mode_widened",
        "bypass",
    ] {
        exactly(&all, &format!("{word} refusals"), me, "refused", 1, reason(word))?;
    }
    // Nothing else is in the log: the events and the refusals by writer number exactly what the actions made.
    let role = |r: &Record| match () {
        _ if serve.is(r) => "serve",
        _ if me.is(r) => "window",
        _ if By::Cli.is(r) => "cli",
        _ => "other",
    };
    let mut events: BTreeMap<String, usize> = BTreeMap::new();
    let mut refusals: BTreeMap<(&str, String), usize> = BTreeMap::new();
    for r in &all {
        let name = match r.event.as_str() {
            "cmd" => format!("cmd/{}", field(r, "kind")),
            other => other.to_owned(),
        };
        *events.entry(name).or_default() += 1;
        if r.event == "refused" {
            *refusals.entry((role(r), field(r, "reason").to_owned())).or_default() += 1;
        }
    }
    let wanted_events: BTreeMap<String, usize> = [
        ("enable", 2),
        ("disable", 2),
        ("conn_open", 2),
        ("conn_close", 2),
        ("push_endpoint_set", 1),
        ("enroll_offered", 4),
        ("enroll_expired", 1),
        ("enroll_confirmed", 2),
        ("enroll_refused", 1),
        ("grant_changed", 1),
        ("revoke", 2),
        ("cmd/send", 2),
        ("cmd/queue", 2),
        ("cmd/take_back", 1),
        ("cmd/stop_task", 1),
        ("cmd/stop_background", 1),
        ("cmd/interrupt", 2),
        ("refused", 21),
    ]
    .into_iter()
    .map(|(name, n)| (name.to_owned(), n))
    .collect();
    check(events == wanted_events, || {
        format!("the log's events are not what the actions made: {events:?}")
    })?;
    let wanted_refusals: BTreeMap<(&str, String), usize> = [
        ("serve", "bad_token", 1),
        ("serve", "revoked_token", 1),
        ("serve", "host", 1),
        ("serve", "origin", 1),
        ("serve", "path_prefix", 1),
        ("serve", "grant", 1),
        ("window", "enroll_code_invalid", 2),
        ("window", "not_allowed", 1),
        ("window", "slash_or_bang", 1),
        ("window", "too_long", 1),
        ("window", "duplicate_command", 1),
        ("window", "work_changed", 1),
        ("window", "not_yours", 1),
        ("window", "agent", 1),
        ("window", "tab_not_live", 1),
        ("window", "not_started", 1),
        ("window", "not_claude", 1),
        ("window", "busy", 1),
        ("window", "mode_widened", 1),
        ("window", "bypass", 1),
    ]
    .into_iter()
    .map(|(by, reason, n)| ((by, reason.to_owned()), n))
    .collect();
    check(refusals == wanted_refusals, || {
        format!("the log's refusals are not what the actions made: {refusals:?}")
    })?;
    // Nothing is written by a writer that did not see it.
    for event in ["conn_open", "conn_close", "enable", "disable", "push_endpoint_set"] {
        check(all.iter().filter(|r| r.event == event).all(|r| serve.is(r)), || {
            format!("{event} was written by another writer than serve")
        })?;
    }
    for event in [
        "cmd",
        "enroll_offered",
        "enroll_confirmed",
        "enroll_refused",
        "enroll_expired",
        "grant_changed",
    ] {
        check(all.iter().filter(|r| r.event == event).all(|r| me.is(r)), || {
            format!("{event} was written by another writer than the window")
        })?;
    }

    // ----- What must never be in it.
    let secrets = vec![
        ("the steering token", steer_token.clone()),
        ("the watching token", watch_token.clone()),
        ("the first code", stale.clone()),
        ("the second code", fresh.clone()),
        ("the steering device's code", steer_code.clone()),
        ("the watching device's code", watch_code.clone()),
        ("the sentinel prompt text", SENTINEL.to_owned()),
        ("the push endpoint's path", PUSH_PATH.to_owned()),
    ];
    for entry in std::fs::read_dir(home.audit()).map_err(|e| e.to_string())?.flatten() {
        let bytes = std::fs::read(entry.path()).map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&bytes);
        for (what, secret) in &secrets {
            check(!text.contains(secret.as_str()), || {
                format!("{what} is in {}", entry.path().display())
            })?;
        }
    }
    for datagram in journal.datagrams() {
        let text = String::from_utf8_lossy(&datagram);
        for (what, secret) in &secrets {
            check(!text.contains(secret.as_str()), || {
                format!("{what} is in a journal datagram")
            })?;
        }
    }

    // ----- `eitri remote log --verify`: clean after the restart, then each tampering named.
    let exe = std::fs::canonicalize(shell)
        .map_err(|e| e.to_string())?
        .to_string_lossy()
        .into_owned();
    let verify = |home: &Home| run(home.remote(shell, &["log", "--verify"]));
    write_entries(dir, &exe, &journal.records(), &[])?;
    let clean = verify(&home)?;
    check(clean.code == Some(0) && clean.stdout.trim() == "clean", || {
        format!("verify of an untouched log: {}", clean.says())
    })?;
    let file = home.audit().join("audit.jsonl");
    let original = std::fs::read_to_string(&file).map_err(|e| e.to_string())?;
    let all_lines: Vec<&str> = original.lines().collect();
    let middle = all_lines.len() / 2;
    // What a report says of one record: the file's line, whole, so its writer, run and seq are in it.
    let says = |found: &Run, head: &str| -> Vec<String> {
        found
            .stdout
            .lines()
            .filter_map(|l| l.strip_prefix(head))
            .map(str::to_owned)
            .collect()
    };

    // An edited record.
    let mut edited = all_lines.clone();
    let swapped = edited[middle].replacen("\"event\":\"", "\"event\":\"x-", 1);
    check(swapped != edited[middle], || "the edit changed nothing".into())?;
    edited[middle] = &swapped;
    std::fs::write(&file, edited.join("\n") + "\n").map_err(|e| e.to_string())?;
    let found = verify(&home)?;
    check(
        found.code == Some(1) && says(&found, "altered in the file: ") == [all_lines[middle].to_owned()],
        || {
            format!(
                "verify of an edited record did not name record {middle}: {}",
                found.says()
            )
        },
    )?;

    // A deleted record.
    let mut deleted = all_lines.clone();
    deleted.remove(middle);
    std::fs::write(&file, deleted.join("\n") + "\n").map_err(|e| e.to_string())?;
    let found = verify(&home)?;
    check(
        found.code == Some(1) && says(&found, "missing from the file: ") == [all_lines[middle].to_owned()],
        || {
            format!(
                "verify of a deleted record did not name record {middle}: {}",
                found.says()
            )
        },
    )?;

    // A forged journal entry, from another executable.
    std::fs::write(&file, &original).map_err(|e| e.to_string())?;
    write_entries(dir, &exe, &journal.records(), &[all_lines[middle].to_owned()])?;
    let found = verify(&home)?;
    // The journal's entry carries no seq of its own to name; it is counted, and the file's records stay unflagged.
    check(
        found.code == Some(1)
            && says(&found, "journal entries not sent by this binary or not records: ") == ["1"]
            && says(&found, "altered in the file: ").is_empty()
            && says(&found, "missing from the file: ").is_empty(),
        || format!("verify of a forged entry: {}", found.says()),
    )?;

    // Put back, it is clean again.
    write_entries(dir, &exe, &journal.records(), &[])?;
    let clean = verify(&home)?;
    check(clean.code == Some(0) && clean.stdout.trim() == "clean", || {
        format!("verify after the log was put back: {}", clean.says())
    })
}
