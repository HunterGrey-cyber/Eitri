//! Remote control switched on, end to end: windows (real panels with a recording provider, in this process) decide
//! and start `eitri remote serve` the way a window does (`eitri_remote::serve::spawn`), the product binary runs it in a
//! home of its own, sandboxed; `eitri remote add-device` reaches the window through it (whatever it listens on) and a
//! headless WebSocket client (the phone) enrolls with the code shown, watches a tab and stops its turn through it;
//! `eitri remote grant` changes the phone's grant after the desk's yes; `eitri remote stop` then closes the phone and
//! nothing starts it again.
//!
//! A plain `main` (`harness = false`): `own_display_scan` requires a test that starts the product binary to isolate its
//! display in `main`, which libtest's harness never runs. `serve` gets no display.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use eitri_core::agents::{new_instance, ProjectRef, Publisher, Row, WindowRef};
use eitri_core::lua::config::{RemoteListen, RemoteSettings};
use eitri_core::remote_devices::{DeviceStore, Grant};
use eitri_core::remote_pairing::{Endpoint, PairMode, PairScheme, PairingRoutes};
use eitri_core::remote_runtime::{start_serve, RemotePlan, ServeConfig, Spawner};
use eitri_core::remote_rv::{rv_socket_path, serve_socket_path};
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

type Check = fn(&str, &Path) -> Result<(), String>;

fn main() {
    // Before anything that could connect to a display or a bus.
    let server = own_x_server::isolate("remote_e2e", "640x480x24");
    agent::state_dirs::redirect_state_to_a_test_root();
    let shell = env!("CARGO_BIN_EXE_shell");
    let scratch: PathBuf = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("re2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).expect("creating the scratch directory must succeed");
    let checks: Vec<(&str, Check)> = vec![
        (
            "disabled_window_binds_nothing_and_starts_nothing",
            disabled_window_binds_nothing_and_starts_nothing,
        ),
        ("two_windows_start_one_serve", two_windows_start_one_serve),
        (
            "enroll_watch_and_interrupt_through_serve",
            enroll_watch_and_interrupt_through_serve,
        ),
        ("stop_closes_phones_and_serve", stop_closes_phones_and_serve),
        (
            "add_device_reaches_the_window_whatever_serve_listens_on",
            add_device_reaches_the_window_whatever_serve_listens_on,
        ),
        (
            "grant_reaches_the_window_through_serve",
            grant_reaches_the_window_through_serve,
        ),
    ];
    // `cargo test --test remote_e2e -- <part of a name>` runs the checks whose names hold it.
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
        match outcome {
            Ok(()) => println!("test {name} ... ok"),
            Err(why) => {
                failed += 1;
                println!("test {name} ... FAILED\n    {why}");
            }
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
    println!("remote_e2e: {} passed, {failed} failed", ran - failed);
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
        Home { root: dir.to_owned() }
    }

    fn dirs(&self) -> Dirs {
        Dirs {
            control: self.root.join("run/eitri"),
            state: self.root.join("state/eitri/remote"),
        }
    }

    fn env(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("HOME", self.root.join("home")),
            ("XDG_CONFIG_HOME", self.root.join("config")),
            ("XDG_DATA_HOME", self.root.join("data")),
            ("XDG_STATE_HOME", self.root.join("state")),
            ("XDG_CACHE_HOME", self.root.join("cache")),
            ("XDG_RUNTIME_DIR", self.root.join("run")),
        ]
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

/// `eitri remote add-device Pixel` while `window` turns: the code its page was shown.
fn add_device(shell: &str, home: &Home, window: &Window) -> Result<String, String> {
    add_device_by(
        shell,
        home,
        window,
        &[],
        "Pixel will connect directly to 127.0.0.1:41000 (http).",
    )
}

/// `eitri remote add-device <flags> Pixel` while `window` turns, which says `endpoint`: the code its page was shown.
fn add_device_by(shell: &str, home: &Home, window: &Window, flags: &[&str], endpoint: &str) -> Result<String, String> {
    let before = window.envelopes("remote_pairing").len();
    let args: Vec<&str> = std::iter::once("add-device")
        .chain(flags.iter().copied())
        .chain(["Pixel"])
        .collect();
    let added = run_while(home.remote(shell, &args), || {
        window.turn();
        Ok(())
    })?;
    check(
        added.code == Some(0) && added.stdout.contains("Scan the code") && added.stdout.contains(endpoint),
        || format!("add-device: {}", added.says()),
    )?;
    let shown = window.asked("remote_pairing", before)?;
    shown["code"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("no code in {shown}"))
}

/// A spawner that counts the `serve`s it starts and starts them through `inner`.
struct Counting<'a> {
    inner: &'a ServeSpawner,
    started: AtomicUsize,
}

impl Spawner for Counting<'_> {
    fn spawn_serve(&self, cfg: &ServeConfig) -> std::io::Result<()> {
        self.started.fetch_add(1, Ordering::SeqCst);
        self.inner.spawn_serve(cfg)
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

fn listening_on(listen: RemoteListen) -> RemoteSettings {
    RemoteSettings {
        listen,
        ..settings(true)
    }
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
    focused: bool,
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
        spawner: &dyn Spawner,
        focused: bool,
        label: &str,
    ) -> Result<(Window, bool), String> {
        let instance = new_instance();
        let page = Rc::new(RecordingPage::default());
        let handle = AgentPanelHandle::for_host_test(page.clone());
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r-ready"}"#);
        let window_ref = WindowRef {
            instance: instance.clone(),
            name: "1".into(),
            focused,
        };
        let publisher = Publisher::start(&dirs.control, &instance, project(), window_ref).map_err(|e| e.to_string())?;
        let window = Window {
            instance,
            handle,
            page,
            publisher: Some(publisher),
            focused,
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
            // A phone reaches this window's `serve` directly by default here, as it does a confirmed address, so its
            // grant may steer; the listener itself is the home's Unix socket. `--ssh` takes the SSH route.
            socket.set_pairing(Pairing {
                routes: PairingRoutes {
                    ssh: Some(Endpoint {
                        mode: PairMode::Ssh,
                        scheme: PairScheme::Http,
                        host: "127.0.0.1".into(),
                        port: 41000,
                    }),
                    direct: Some(Endpoint {
                        mode: PairMode::Direct,
                        scheme: PairScheme::Http,
                        host: "127.0.0.1".into(),
                        port: 41000,
                    }),
                },
                ssh_dir: dirs.state.clone(),
            });
            window.handle.set_remote(socket);
        }
        let started = start_serve(&decided, spawner).map_err(|e| format!("serve did not start: {e}"))?;
        window.publish(&[]);
        Ok((window, started))
    }

    fn publish(&self, keys: &[&RowKey]) {
        let rows = keys
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
                    focused: self.focused,
                },
                number: at as u16 + 1,
                state: "idle".into(),
                ..Row::default()
            })
            .collect();
        self.publisher.as_ref().unwrap().offer_now(rows);
    }

    fn tab(&self) -> Tab {
        let provider = Arc::new(RecordingProvider::interruptible());
        let (id, key) = self.handle.open_live_tab_for_host_test(provider.clone(), &self.cwd);
        self.publish(&[&key]);
        Tab { id, key, provider }
    }

    /// One tick, which pumps the `rv-` socket the panel holds.
    fn turn(&self) {
        self.handle.tick();
    }

    fn start_turn(&self, tab: &Tab, turn: &str) -> Result<(), String> {
        tab.provider
            .queue(agent::AgentDomainEvent::TurnStarted { turn_id: turn.into() });
        let deadline = Instant::now() + PATIENCE;
        while self.handle.tab_progress_for_host_test(tab.id).1.as_deref() != Some(turn) {
            if Instant::now() > deadline {
                return Err(format!("the turn {turn} never started"));
            }
            self.turn();
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }

    /// Turns until the page was sent a `kind` envelope after `after` of them; that envelope.
    fn asked(&self, kind: &str, after: usize) -> Result<Value, String> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(found) = self.envelopes(kind).into_iter().nth(after) {
                return Ok(found);
            }
            if Instant::now() > deadline {
                return Err(format!("the page was never sent {kind}"));
            }
            self.turn();
            std::thread::sleep(Duration::from_millis(2));
        }
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

impl Phone {
    fn connect(dirs: &Dirs) -> Result<Phone, String> {
        let path = serve_socket_path(&dirs.control).map_err(|e| e.to_string())?;
        let mut stream = UnixStream::connect(&path).map_err(|e| format!("serve's socket: {e}"))?;
        stream
            .write_all(
                b"GET /v1/ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
                  Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
            )
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
            n => {
                bytes.push(0x80 | 126);
                bytes.extend((n as u16).to_be_bytes());
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
}

// ---------------------------------------------------------------------------------------------------------------
// The checks.

fn disabled_window_binds_nothing_and_starts_nothing(shell: &str, dir: &Path) -> Result<(), String> {
    let home = Home::new(dir);
    let dirs = home.dirs();
    let spawner = home.spawner(shell);
    let counting = Counting {
        inner: &spawner,
        started: AtomicUsize::new(0),
    };
    let (window, started) = Window::open(&dirs, &settings(false), &counting, true, "re-off")?;
    check(!started && counting.started.load(Ordering::SeqCst) == 0, || {
        "a serve was started".into()
    })?;
    check(!window.handle.has_remote(), || "the panel holds an rv- socket".into())?;
    let socket = rv_socket_path(&dirs.control, &window.instance).map_err(|e| e.to_string())?;
    check(!socket.exists(), || format!("{} exists", socket.display()))?;
    let left: Vec<String> = std::fs::read_dir(&dirs.control)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("rv-") || name.starts_with("remote"))
        .collect();
    check(left.is_empty(), || format!("remote control left {left:?}"))?;
    let status = home.status(shell)?;
    check(status.code == Some(3) && status.stdout.trim() == "not running", || {
        status.says()
    })
}

fn two_windows_start_one_serve(shell: &str, dir: &Path) -> Result<(), String> {
    let home = Home::new(dir);
    let dirs = home.dirs();
    let spawner = home.spawner(shell);
    let counting = Counting {
        inner: &spawner,
        started: AtomicUsize::new(0),
    };
    let (first, first_started) = Window::open(&dirs, &settings(true), &counting, true, "re-one-a")?;
    check(first_started && first.handle.has_remote(), || {
        "the first window started no serve".into()
    })?;
    let pid = home.until_running(shell)?;
    let (second, second_started) = Window::open(&dirs, &settings(true), &counting, false, "re-one-b")?;
    check(!second_started && second.handle.has_remote(), || {
        "the second window started a serve, or bound nothing".into()
    })?;
    check(counting.started.load(Ordering::SeqCst) == 1, || {
        format!("{} serves started", counting.started.load(Ordering::SeqCst))
    })?;
    // Still the first one: nothing took its place.
    let again = home.until_running(shell)?;
    check(again == pid, || format!("serve {pid} became {again}"))?;
    for window in [&first, &second] {
        let socket = rv_socket_path(&dirs.control, &window.instance).map_err(|e| e.to_string())?;
        check(socket.exists(), || format!("{} is missing", socket.display()))?;
    }
    Ok(())
}

/// A window that started `serve`, a live tab in it, and a phone enrolled through `serve` with the desk's yes: the
/// window, the tab, the device id and its token.
fn enrolled(shell: &str, home: &Home, label: &str) -> Result<(Window, Tab, String, String), String> {
    let dirs = home.dirs();
    let spawner = home.spawner(shell);
    let (window, started) = Window::open(&dirs, &settings(true), &spawner, true, label)?;
    check(started, || "the window started no serve".into())?;
    home.until_running(shell)?;
    let tab = window.tab();

    // The code the window shows when `eitri remote add-device` asks it through `serve`.
    let code = add_device(shell, home, &window)?;

    let mut phone = Phone::connect(&dirs)?;
    phone.hello("", None);
    phone.send(json!({
        "kind": "enroll", "instance": window.instance, "code": code, "device_name": "Pixel", "recipe": "direct"
    }));
    let asked = window.asked("remote_enroll", 0)?;
    check(asked["name"] == "Pixel", || format!("the desk was asked {asked}"))?;
    let nonce = asked["nonce"].as_u64().ok_or_else(|| format!("no nonce in {asked}"))?;
    window.handle.page_message(
        ViewId::BASE,
        &format!(
            r#"{{"type":"remote_enroll_answer","request_id":"yes","nonce":{nonce},"yes":true,"grant":"watch+steer"}}"#
        ),
    );
    let lines = phone.until(&[&window], "enrolled", |l| {
        l["kind"] == "enrolled" || l["kind"] == "enroll_refused"
    })?;
    let enrolled = lines.last().unwrap();
    check(enrolled["kind"] == "enrolled", || format!("{enrolled}"))?;
    let device = enrolled["device"].as_str().ok_or("no device")?.to_owned();
    let token = enrolled["token"].as_str().ok_or("no token")?.to_owned();
    Ok((window, tab, device, token))
}

fn enroll_watch_and_interrupt_through_serve(shell: &str, dir: &Path) -> Result<(), String> {
    let home = Home::new(dir);
    let (window, tab, device, token) = enrolled(shell, &home, "re-steer")?;
    window.start_turn(&tab, "t1")?;
    let key = tab.key.as_str();

    let mut phone = Phone::connect(&home.dirs())?;
    phone.hello(&device, Some(&token));
    phone.send(json!({ "kind": "subscribe", "key": key }));
    let lines = phone.until(&[&window], "the snapshot", |l| l["kind"] == "snapshot")?;
    let generation = lines.last().unwrap()["generation"].as_u64().ok_or("no generation")?;
    phone.send(json!({
        "kind": "act",
        "key": key,
        "cmd": {
            "type": "interrupt",
            "command_id": "s1",
            "expect_work": { "turn": { "id": "t1", "generation": generation } },
        },
    }));
    let lines = phone.until(&[&window], "the result", |l| l["kind"] == "result")?;
    let result = lines.last().unwrap();
    check(result["ok"] == true, || format!("{result}"))?;
    check(tab.provider.interrupts() == 1, || {
        format!("the provider was interrupted {} times", tab.provider.interrupts())
    })
}

fn stop_closes_phones_and_serve(shell: &str, dir: &Path) -> Result<(), String> {
    let home = Home::new(dir);
    let (window, tab, device, token) = enrolled(shell, &home, "re-stop")?;
    let mut phone = Phone::connect(&home.dirs())?;
    phone.hello(&device, Some(&token));
    phone.send(json!({ "kind": "subscribe", "key": tab.key.as_str() }));
    phone.until(&[&window], "the snapshot", |l| l["kind"] == "snapshot")?;

    let stopped = run(home.remote(shell, &["stop"]))?;
    check(stopped.code == Some(0) && stopped.stdout.trim() == "stopped", || {
        format!("stop: {}", stopped.says())
    })?;
    let deadline = Instant::now() + PATIENCE;
    loop {
        window.turn();
        match phone.recv(Duration::from_millis(20)) {
            Err(_) => break,
            Ok(_) if Instant::now() > deadline => return Err("the phone is still connected after the stop".into()),
            Ok(_) => {}
        }
    }
    // The window stays, with remote control on, and does not start it again.
    for _ in 0..50 {
        window.turn();
        std::thread::sleep(Duration::from_millis(20));
    }
    let status = home.status(shell)?;
    check(status.code == Some(3) && status.stdout.trim() == "not running", || {
        format!("after the stop: {}", status.says())
    })?;
    check(window.handle.has_remote(), || "the window let its rv- socket go".into())
}

fn add_device_reaches_the_window_whatever_serve_listens_on(shell: &str, dir: &Path) -> Result<(), String> {
    for (at, listen) in [RemoteListen::Socket, RemoteListen::Loopback].into_iter().enumerate() {
        let home = Home::new(&dir.join(at.to_string()));
        let dirs = home.dirs();
        let said = |what: String| format!("listening on {listen:?}: {what}");
        // With no serve, nothing is asked and the command says so.
        let alone = run(home.remote(shell, &["add-device", "Pixel"]))?;
        check(
            alone.code == Some(1) && alone.stderr.contains("no Eitri window"),
            || said(alone.says()),
        )?;
        let spawner = home.spawner(shell);
        let (window, started) = Window::open(&dirs, &listening_on(listen.clone()), &spawner, true, "re-add")?;
        check(started, || said("the window started no serve".into()))?;
        home.until_running(shell).map_err(said)?;
        let code = add_device(shell, &home, &window).map_err(said)?;
        check(code.len() >= 16, || said(format!("the code shown is {code:?}")))?;
        let shown = window.asked("remote_pairing", window.envelopes("remote_pairing").len() - 1)?;
        check(
            shown["uri"]
                .as_str()
                .is_some_and(|uri| uri.contains("&m=direct&s=http&")),
            || said(format!("the pairing shown is {shown}")),
        )?;
        add_device_by(
            shell,
            &home,
            &window,
            &["--ssh"],
            "Pixel will connect through SSH to 127.0.0.1:41000 on this desk.",
        )
        .map_err(said)?;
        let shown = window.asked("remote_pairing", window.envelopes("remote_pairing").len() - 1)?;
        check(
            shown["uri"].as_str().is_some_and(|uri| uri.contains("&m=ssh&s=http&")),
            || said(format!("the pairing shown is {shown}")),
        )?;
        drop(window);
        let _ = run(home.remote(shell, &["stop"]));
    }
    Ok(())
}

fn grant_reaches_the_window_through_serve(shell: &str, dir: &Path) -> Result<(), String> {
    let home = Home::new(dir);
    let (window, _tab, device, _token) = enrolled(shell, &home, "re-grant")?;
    let grant_of = || -> Result<Grant, String> {
        let store = DeviceStore::load(&home.dirs().state.join("devices.json")).map_err(|e| e.to_string())?;
        store
            .get(&device)
            .map(|d| d.grant)
            .ok_or_else(|| format!("{device} is not in the store"))
    };
    check(grant_of()? == Grant::WatchSteer, || {
        "the phone was not let steer".into()
    })?;
    let mut answered = false;
    let granted = run_while(home.remote(shell, &["grant", &device, "watch"]), || {
        window.turn();
        if !answered {
            if let Some(asked) = window.envelopes("remote_grant").first() {
                let nonce = asked["nonce"].as_u64().ok_or_else(|| format!("no nonce in {asked}"))?;
                window.handle.page_message(
                    ViewId::BASE,
                    &format!(
                        r#"{{"type":"remote_enroll_answer","request_id":"g","nonce":{nonce},"yes":true,"grant":"watch"}}"#
                    ),
                );
                answered = true;
            }
        }
        Ok(())
    })?;
    check(granted.code == Some(0) && granted.stdout.contains("may now"), || {
        format!("grant: {}", granted.says())
    })?;
    check(answered, || "the desk was never asked".into())?;
    check(grant_of()? == Grant::Watch, || "the grant did not change".into())
}
