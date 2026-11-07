//! `serve`'s sessions end to end, all in this process: a headless WebSocket client (the phone) against a `serve` hub on
//! a Unix socket of its own, and Eitri windows behind their `rv-` sockets: real panels (a panel handle with a recording
//! provider), or stand-ins that record what reaches them. No sandbox (it would bind this process for good) and no
//! display.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use eitri_core::agents::{new_instance, ProjectRef, Publisher, Row, WindowRef};
use eitri_core::remote_devices::token_hash_hex;
use eitri_core::remote_rv::{
    refused_line, rv_socket_path, serve_socket_path, RvClient, RvInbound, RvListener, RV_MAX_CONNECTIONS,
    RV_REFUSED_FULL,
};
use eitri_core::tab_set::RowKey;
use eitri_core::tabs::TabId;
use eitri_core::test_providers::RecordingProvider;
use eitri_panel::agent_panel::{AgentPanelHandle, RemoteSocket, ViewId};
use eitri_panel::panel_page::PageSurface;
use eitri_panel::panel_wake::PanelWake;
use eitri_remote::serve::config::{Listen, ServeConfig};
use eitri_remote::serve::serve_unix;
use eitri_remote::serve::session::{Hub, Limits};
use serde_json::{json, Value};

/// How long any one step may take on a loaded machine; a check that passes never waits it out.
const PATIENCE: Duration = Duration::from_secs(30);
/// What the store holds: `d1` may steer (token `tok`), `d2` may only watch (token `wtok`).
const STEER: (&str, &str) = ("d1", "tok");
const WATCH: (&str, &str) = ("d2", "wtok");

/// One check: what went wrong, if anything.
pub type Check = fn(&Path) -> Result<(), String>;

pub fn checks() -> Vec<(&'static str, Check)> {
    vec![
        ("hello_needs_a_known_token", hello_needs_a_known_token),
        ("hello_deadline_closes", hello_deadline_closes),
        ("rows_then_tab_snapshot_then_events", rows_then_tab_snapshot_then_events),
        (
            "act_from_watch_device_refused_twice",
            act_from_watch_device_refused_twice,
        ),
        ("steer_device_interrupt_end_to_end", steer_device_interrupt_end_to_end),
        (
            "moved_tab_resubscribes_at_the_new_owner",
            moved_tab_resubscribes_at_the_new_owner,
        ),
        ("reconnect_gets_fresh_greeting", reconnect_gets_fresh_greeting),
        ("two_processes_one_endpoint", two_processes_one_endpoint),
        ("stalled_phone_bounds_serve_memory", stalled_phone_bounds_serve_memory),
        (
            "phone_resumes_with_a_snapshot_after_overflow",
            phone_resumes_with_a_snapshot_after_overflow,
        ),
        ("revoked_device_gets_nothing_more", revoked_device_gets_nothing_more),
        (
            "devices_connected_reaches_every_panel",
            devices_connected_reaches_every_panel,
        ),
        (
            "a_window_opened_later_is_told_who_is_connected",
            a_window_opened_later_is_told_who_is_connected,
        ),
        (
            "a_refused_more_keeps_the_subscription",
            a_refused_more_keeps_the_subscription,
        ),
        (
            "a_window_that_turns_every_connection_away_is_tried_with_pauses",
            a_window_that_turns_every_connection_away_is_tried_with_pauses,
        ),
        ("a_full_panel_is_not_hammered", a_full_panel_is_not_hammered),
    ]
}

fn check(ok: bool, what: impl FnOnce() -> String) -> Result<(), String> {
    if ok {
        Ok(())
    } else {
        Err(what())
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The world: directories, the store, the hub.

struct World {
    control: PathBuf,
    store: PathBuf,
    hub: Arc<Hub>,
    socket: PathBuf,
}

impl World {
    fn new(dir: &Path, limits: Limits) -> World {
        use std::os::unix::fs::PermissionsExt;
        agent::state_dirs::redirect_state_to_a_test_root();
        let control = dir.join("run").join("eitri");
        let store_dir = dir.join("state");
        for d in [dir.join("run"), control.clone(), store_dir.clone()] {
            std::fs::create_dir_all(&d).unwrap();
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let store = store_dir.join("devices.json");
        let body = json!({
            "v": 1,
            "devices": [
                { "id": STEER.0, "name": "phone", "token_sha256": token_hash_hex(STEER.1), "grant": "watch+steer" },
                { "id": WATCH.0, "name": "tablet", "token_sha256": token_hash_hex(WATCH.1), "grant": "watch" },
            ],
        });
        agent::private_fs::write_private(&store, body.to_string().as_bytes()).unwrap();
        let hub = Hub::start(&control, &store, limits).expect("the hub starts");
        let socket = serve_socket_path(&control).unwrap();
        let listener = UnixListener::bind(&socket).expect("serve's socket binds");
        let cfg = ServeConfig {
            listen: Listen::Socket,
            port: 41000,
            origins: Vec::new(),
        };
        serve_unix(listener, Arc::new(cfg), hub.clone()).unwrap();
        World {
            control,
            store,
            hub,
            socket,
        }
    }

    fn phone(&self) -> Phone {
        Phone::connect(&self.socket)
    }

    /// A phone past its `hello` as `device`.
    fn phone_as(&self, device: (&str, &str)) -> Phone {
        let mut phone = self.phone();
        phone.hello(device.0, Some(device.1));
        phone
    }
}

// ---------------------------------------------------------------------------------------------------------------
// A window: a real panel behind its rv- socket, its rows published.

struct NoPage;

impl PageSurface for NoPage {
    fn send(&self, _envelope_json: &str) {}
    fn is_visible(&self) -> bool {
        false
    }
    fn load_document(&self, _html: &str, _base_uri: &str) {}
    fn set_background(&self, _rgb: [u8; 3]) {}
}

struct Window {
    instance: String,
    handle: AgentPanelHandle,
    socket: Option<RemoteSocket>,
    wake: PanelWake,
    publisher: Option<Publisher>,
    control: PathBuf,
    store: PathBuf,
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

fn window_ref(instance: &str) -> WindowRef {
    WindowRef {
        instance: instance.to_owned(),
        name: "1".into(),
        focused: true,
    }
}

fn row(instance: &str, key: &str, number: u16) -> Row {
    Row {
        id: format!("{instance}.{number}"),
        key: key.to_owned(),
        agent: "claude".into(),
        project: project(),
        window: window_ref(instance),
        number,
        state: "idle".into(),
        ..Row::default()
    }
}

impl Window {
    fn open(world: &World, label: &str) -> Window {
        let instance = new_instance();
        let handle = AgentPanelHandle::for_host_test(Rc::new(NoPage));
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r-ready"}"#);
        let wake = PanelWake::new().unwrap();
        let socket = RemoteSocket::bind(&world.control, &instance, &world.store, &wake).expect("the rv- socket binds");
        let publisher = Publisher::start(&world.control, &instance, project(), window_ref(&instance)).unwrap();
        Window {
            instance,
            handle,
            socket: Some(socket),
            wake,
            publisher: Some(publisher),
            control: world.control.clone(),
            store: world.store.clone(),
            cwd: agent::state_dirs::test_workspace_dir(label),
        }
    }

    /// A live tab, published as this window's only row.
    fn tab(&self) -> Tab {
        let provider = Arc::new(RecordingProvider::interruptible());
        let (id, key) = self.handle.open_live_tab_for_host_test(provider.clone(), &self.cwd);
        self.publish(&[&key]);
        Tab { id, key, provider }
    }

    fn publish(&self, keys: &[&RowKey]) {
        let rows = keys
            .iter()
            .enumerate()
            .map(|(at, key)| row(&self.instance, key.as_str(), at as u16 + 1))
            .collect();
        self.publisher.as_ref().unwrap().offer_now(rows);
    }

    fn turn(&self) {
        self.handle.tick();
        if let Some(socket) = &self.socket {
            socket.pump(&self.handle);
        }
    }

    fn revision(&self, tab: &Tab) -> u64 {
        self.handle.tab_progress_for_host_test(tab.id).2
    }

    fn generation(&self, tab: &Tab) -> u64 {
        self.handle.tab_progress_for_host_test(tab.id).0
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

    /// Queues `count` deltas of `text` and turns until they are folded; the revision reached.
    fn fold(&self, tab: &Tab, count: usize, text: &str) -> Result<u64, String> {
        let target = self.revision(tab) + count as u64;
        tab.provider.queue_all((0..count).map(|_| delta(text)).collect());
        let deadline = Instant::now() + PATIENCE;
        while self.revision(tab) < target {
            if Instant::now() > deadline {
                return Err("a batch was never folded".into());
            }
            self.turn();
        }
        Ok(self.revision(tab))
    }

    /// Closes the rv- socket, every connection with it, and binds it again: what a window that closed `serve`'s
    /// connection looks like to `serve`.
    fn rebind(&mut self) {
        if let Some(socket) = self.socket.take() {
            socket.close(&self.handle);
        }
        self.socket = Some(RemoteSocket::bind(&self.control, &self.instance, &self.store, &self.wake).unwrap());
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        if let Some(socket) = self.socket.take() {
            socket.close(&self.handle);
        }
        if let Some(publisher) = self.publisher.take() {
            publisher.stop();
        }
    }
}

fn delta(text: &str) -> agent::AgentDomainEvent {
    agent::AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: agent::ContentKind::Text,
        text: text.into(),
    }
}

fn turn_all(windows: &[&Window]) {
    for window in windows {
        window.turn();
    }
}

// ---------------------------------------------------------------------------------------------------------------
// A stand-in window: records every line that reaches its rv- socket and answers `subscribe` with a snapshot whose
// frame names it.

struct FakeWindow {
    instance: String,
    lines: mpsc::Receiver<Value>,
    listener: Option<RvListener>,
    publisher: Option<Publisher>,
}

impl FakeWindow {
    fn open(world: &World) -> FakeWindow {
        let instance = new_instance();
        let (tx, lines) = mpsc::channel();
        let me = instance.clone();
        let listener = RvListener::bind(&world.control, &instance, move |stream| {
            let (tx, me) = (tx.clone(), me.clone());
            std::thread::spawn(move || serve_fake(stream, &tx, &me));
            true
        })
        .unwrap();
        let publisher = Publisher::start(&world.control, &instance, project(), window_ref(&instance)).unwrap();
        FakeWindow {
            instance,
            lines,
            listener: Some(listener),
            publisher: Some(publisher),
        }
    }

    /// Closes its `rv-` socket and binds the same instance's again: a new listener at the same path.
    fn rebind(&mut self, world: &World) {
        drop(self.listener.take());
        let (tx, lines) = mpsc::channel();
        let me = self.instance.clone();
        self.listener = Some(
            RvListener::bind(&world.control, &self.instance, move |stream| {
                let (tx, me) = (tx.clone(), me.clone());
                std::thread::spawn(move || serve_fake(stream, &tx, &me));
                true
            })
            .unwrap(),
        );
        self.lines = lines;
    }

    fn publish(&self, keys: &[&str]) {
        let rows = keys
            .iter()
            .enumerate()
            .map(|(at, key)| row(&self.instance, key, at as u16 + 1))
            .collect();
        self.publisher.as_ref().unwrap().offer_now(rows);
    }

    /// The next line of `kind` that reached it, skipping others, within `within`.
    fn next_of(&self, kind: &str, within: Duration) -> Option<Value> {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self.lines.recv_timeout(left).ok()?;
            if line["kind"] == kind {
                return Some(line);
            }
        }
    }
}

impl Drop for FakeWindow {
    fn drop(&mut self) {
        if let Some(publisher) = self.publisher.take() {
            publisher.stop();
        }
    }
}

fn serve_fake(stream: UnixStream, tx: &mpsc::Sender<Value>, me: &str) {
    let mut writer = stream.try_clone().unwrap();
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { return };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            return;
        };
        let answer = match value["kind"].as_str() {
            Some("subscribe") => Some(json!({
                "v": 1, "kind": "snapshot", "key": value["key"], "through": 1, "generation": 1, "unseen": null,
                "first_turn": 0, "cut": [], "frame": { "from": me },
            })),
            Some("ping") => Some(json!({ "v": 1, "kind": "pong" })),
            _ => None,
        };
        if let Some(answer) = answer {
            if writer.write_all(format!("{answer}\n").as_bytes()).is_err() {
                return;
            }
        }
        if tx.send(value).is_err() {
            return;
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The phone: a WebSocket client over the Unix socket.

struct Phone {
    stream: UnixStream,
    buf: Vec<u8>,
    seq: u64,
    /// The last `seq` received.
    seen: u64,
    /// Every `seq` received, in order.
    seqs: Vec<u64>,
    fragments: Vec<u8>,
}

impl Phone {
    fn connect(socket: &Path) -> Phone {
        let mut stream = UnixStream::connect(socket).expect("serve's socket answers");
        stream
            .write_all(
                b"GET /v1/ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
                  Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
            )
            .unwrap();
        stream.set_read_timeout(Some(PATIENCE)).unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).expect("the 101 comes");
            head.push(byte[0]);
        }
        assert!(head.starts_with(b"HTTP/1.1 101 "), "{}", String::from_utf8_lossy(&head));
        Phone {
            stream,
            buf: Vec::new(),
            seq: 0,
            seen: 0,
            seqs: Vec::new(),
            fragments: Vec::new(),
        }
    }

    fn send_raw(&mut self, frame: Value) {
        let payload = frame.to_string().into_bytes();
        let key = [0x11u8, 0x22, 0x33, 0x44];
        let mut bytes = vec![0x81];
        match payload.len() {
            n @ 0..=125 => bytes.push(0x80 | n as u8),
            n @ 126..=65535 => {
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
        // A server that already closed is what some checks look for; a failed write shows as the close.
        let _ = self.stream.write_all(&bytes);
    }

    fn send(&mut self, mut body: Value) {
        self.seq += 1;
        body["v"] = json!(1);
        body["seq"] = json!(self.seq);
        body["ack"] = json!(self.seen);
        self.send_raw(body);
    }

    fn hello(&mut self, device: &str, token: Option<&str>) {
        let mut body = json!({ "kind": "hello", "device": device, "layout": "slab", "protocol": 1 });
        if let Some(token) = token {
            body["token"] = json!(token);
        }
        self.send(body);
    }

    /// The next whole text message, `Ok(None)` when none came within `within`, `Err` when the server closed.
    fn recv(&mut self, within: Duration) -> Result<Option<Value>, String> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(message) = self.take_message()? {
                let value: Value = serde_json::from_slice(&message).map_err(|e| format!("not JSON: {e}"))?;
                if let Some(seq) = value["seq"].as_u64() {
                    self.seen = seq;
                    self.seqs.push(seq);
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

    /// A whole text message out of the buffer, the fragments of one put together.
    fn take_message(&mut self) -> Result<Option<Vec<u8>>, String> {
        loop {
            if self.buf.len() < 2 {
                return Ok(None);
            }
            let (first, second) = (self.buf[0], self.buf[1]);
            if second & 0x80 != 0 {
                return Err("a server frame was masked".into());
            }
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
            let fin = first & 0x80 != 0;
            match first & 0x0f {
                0x1 | 0x0 => {
                    self.fragments.extend_from_slice(&payload);
                    if fin {
                        return Ok(Some(std::mem::take(&mut self.fragments)));
                    }
                }
                0x8 => {
                    let code = payload.get(..2).map(|c| u16::from_be_bytes([c[0], c[1]]));
                    return Err(format!("close frame {code:?}"));
                }
                0x9 | 0xa => {}
                other => return Err(format!("opcode {other}")),
            }
        }
    }

    /// Reads until the server closes; every byte that came (none, for a close with no reply).
    fn bytes_until_closed(&mut self, within: Duration) -> Result<Vec<u8>, String> {
        let deadline = Instant::now() + within;
        let mut got = std::mem::take(&mut self.buf);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!("still open after {within:?}; {} bytes came", got.len()));
            }
            self.stream.set_read_timeout(Some(left)).unwrap();
            let mut chunk = [0u8; 4096];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Ok(got),
                Ok(n) => got.extend_from_slice(&chunk[..n]),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(_) => return Ok(got),
            }
        }
    }

    /// Turns `windows` and reads until a message `done` accepts; every message on the way, that one last.
    fn until(&mut self, windows: &[&Window], what: &str, done: impl Fn(&Value) -> bool) -> Result<Vec<Value>, String> {
        let deadline = Instant::now() + PATIENCE;
        let mut got = Vec::new();
        loop {
            if Instant::now() > deadline {
                return Err(format!("timed out waiting for {what}; got {}", summary(&got)));
            }
            turn_all(windows);
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

    /// The seqs received so far run 1, 2, 3, ... with no gap.
    fn seqs_contiguous(&self) -> Result<(), String> {
        check(
            self.seqs.iter().enumerate().all(|(at, seq)| *seq == at as u64 + 1),
            || format!("seqs {:?}", &self.seqs[..self.seqs.len().min(40)]),
        )
    }
}

fn summary(lines: &[Value]) -> String {
    let parts: Vec<String> = lines
        .iter()
        .map(|l| format!("{} {} {} {} {}", l["kind"], l["key"], l["from"], l["through"], l["why"]))
        .collect();
    format!("{parts:?}")
}

/// `lines` of `key` start with a snapshot (greetings aside), and each events frame begins where the frame before it
/// ended; a later snapshot starts over. The revision reached.
fn contiguous(lines: &[Value], key: &str) -> Result<u64, String> {
    let frames: Vec<&Value> = lines
        .iter()
        .filter(|l| l["key"] == key && (l["kind"] == "snapshot" || l["kind"] == "events"))
        .collect();
    if frames.first().map(|f| &f["kind"]) != Some(&json!("snapshot")) {
        return Err(format!("no snapshot first: {}", summary(lines)));
    }
    let mut at = 0;
    for frame in frames {
        if frame["kind"] == "events" && frame["from"].as_u64() != Some(at) {
            return Err(format!("a gap or a repeat at {at}: {frame}"));
        }
        at = frame["through"].as_u64().unwrap_or(0);
    }
    Ok(at)
}

fn command_id_of(line: &Value) -> Option<&str> {
    line["command_id"].as_str()
}

fn interrupt(command_id: &str, turn: &str, generation: u64) -> Value {
    json!({
        "type": "interrupt",
        "command_id": command_id,
        "expect_work": { "turn": { "id": turn, "generation": generation } },
    })
}

// ---------------------------------------------------------------------------------------------------------------
// The checks.

fn hello_needs_a_known_token(dir: &Path) -> Result<(), String> {
    let world = World::new(dir, Limits::default());
    let window = Window::open(&world, "rs-hello");
    let tab = window.tab();
    for (device, token, what) in [
        (STEER.0, Some("not-the-token"), "a wrong token"),
        ("d9", Some(STEER.1), "an unknown device"),
        (STEER.0, Some(WATCH.1), "another device's token"),
    ] {
        let mut phone = world.phone();
        phone.hello(device, token);
        phone.send(json!({ "kind": "subscribe_rows" }));
        let got = phone.bytes_until_closed(PATIENCE)?;
        check(got.is_empty(), || {
            format!("{what}: {} bytes came before the close", got.len())
        })?;
    }
    let mut first_not_hello = world.phone();
    first_not_hello.send(json!({ "kind": "subscribe_rows" }));
    let got = first_not_hello.bytes_until_closed(PATIENCE)?;
    check(got.is_empty(), || {
        format!("a first frame that is not hello: {} bytes", got.len())
    })?;

    let mut old = world.phone();
    old.send(json!({ "kind": "hello", "device": STEER.0, "token": STEER.1, "layout": "slab", "protocol": 2 }));
    let bye = old.recv(PATIENCE)?.ok_or("no bye")?;
    check(bye["kind"] == "bye" && bye["reason"] == "protocol", || format!("{bye}"))?;

    let mut phone = world.phone_as(STEER);
    phone.send(json!({ "kind": "subscribe_rows" }));
    let lines = phone.until(&[&window], "the rows", |l| {
        l["kind"] == "agents_view" && l["view"]["rows"].as_array().is_some_and(|rows| !rows.is_empty())
            || l["kind"] == "agents_row"
    })?;
    check(lines.iter().any(|l| l.to_string().contains(tab.key.as_str())), || {
        format!("the row is not shown: {}", summary(&lines))
    })?;
    phone.seqs_contiguous()
}

fn hello_deadline_closes(dir: &Path) -> Result<(), String> {
    let limits = Limits {
        hello_within: Duration::from_millis(300),
        ..Limits::default()
    };
    let world = World::new(dir, limits);
    let mut silent = world.phone();
    let started = Instant::now();
    let got = silent.bytes_until_closed(PATIENCE)?;
    check(got.is_empty(), || format!("{} bytes before the close", got.len()))?;
    check(started.elapsed() >= Duration::from_millis(300), || {
        format!("closed before the deadline, after {:?}", started.elapsed())
    })?;
    // A ping is not a hello: it keeps nothing open.
    let mut pinging = world.phone();
    let started = Instant::now();
    loop {
        pinging.stream.write_all(&[0x89, 0x80, 1, 2, 3, 4]).ok();
        if started.elapsed() > Duration::from_millis(600) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let closed = pinging.bytes_until_closed(PATIENCE);
    check(closed.is_ok(), || format!("pings kept it open: {closed:?}"))
}

fn rows_then_tab_snapshot_then_events(dir: &Path) -> Result<(), String> {
    let world = World::new(dir, Limits::default());
    let window = Window::open(&world, "rs-rows");
    let tab = window.tab();
    window.start_turn(&tab, "t1")?;
    let key = tab.key.as_str();
    let mut phone = world.phone_as(STEER);
    phone.send(json!({ "kind": "subscribe_rows" }));
    let rows = phone.until(&[&window], "the rows", |l| l.to_string().contains(key))?;
    check(rows[0]["kind"] == "agents_view", || format!("first {}", rows[0]))?;
    phone.send(json!({ "kind": "subscribe", "key": key }));
    let mut lines = phone.until(&[&window], "the snapshot", |l| {
        l["kind"] == "snapshot" && l["key"] == key
    })?;
    let through = window.fold(&tab, 30, "hello ")?;
    lines.extend(phone.until(&[&window], "the events", |l| {
        l["kind"] == "events" && l["through"].as_u64() == Some(through)
    })?);
    let reached = contiguous(&lines, key)?;
    check(reached == through, || format!("reached {reached}, folded {through}"))?;
    phone.seqs_contiguous()?;
    let acked = phone.seen;
    phone.send(json!({ "kind": "ping" }));
    let pong = phone.until(&[&window], "the pong", |l| l["kind"] == "pong")?;
    check(pong.last().unwrap()["ack"].as_u64() == Some(phone.seq), || {
        format!(
            "the ack names the phone's last seq: {} after {acked}",
            pong.last().unwrap()
        )
    })
}

fn act_from_watch_device_refused_twice(dir: &Path) -> Result<(), String> {
    let world = World::new(dir, Limits::default());
    let window = Window::open(&world, "rs-watch");
    let tab = window.tab();
    window.start_turn(&tab, "t1")?;
    let key = tab.key.as_str();
    let generation = window.generation(&tab);
    // A watch device may follow a tab; its `act` is refused by serve and never reaches the window. A stand-in window
    // owns the second tab, so what reaches it is seen.
    let stand_in = FakeWindow::open(&world);
    let other_key = "k-00000000000000a1";
    stand_in.publish(&[other_key]);
    let mut phone = world.phone_as(WATCH);
    phone.send(json!({ "kind": "subscribe", "key": key }));
    phone.until(&[&window], "the snapshot", |l| l["kind"] == "snapshot")?;
    phone.send(json!({ "kind": "subscribe", "key": other_key }));
    phone.until(&[&window], "the stand-in's snapshot", |l| {
        l["kind"] == "snapshot" && l["key"] == other_key
    })?;
    for (id, target) in [("w1", key), ("w3", other_key)] {
        phone.send(json!({ "kind": "act", "key": target, "cmd": interrupt(id, "t1", generation) }));
        let lines = phone.until(&[&window], "the result", |l| l["kind"] == "result")?;
        let result = lines.last().unwrap();
        check(
            command_id_of(result) == Some(id) && result["ok"] == false && result["why"] == "watch only",
            || format!("serve's answer: {result}"),
        )?;
    }
    check(stand_in.next_of("act", Duration::from_millis(500)).is_none(), || {
        "a watch device's act reached the window".into()
    })?;
    // And the window refuses it too, to anyone who reaches its socket with that device's token.
    let mut direct = RvClient::connect(&rv_socket_path(&world.control, &window.instance).unwrap()).unwrap();
    direct
        .send(&RvInbound::Act {
            v: 1,
            device: WATCH.0.into(),
            token: WATCH.1.into(),
            key: key.to_owned(),
            cmd: interrupt("w2", "t1", generation),
        })
        .unwrap();
    let deadline = Instant::now() + PATIENCE;
    let answer = loop {
        window.turn();
        if let Some(line) = direct.recv(Duration::from_millis(5)).map_err(|e| e.to_string())? {
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["kind"] == "result" {
                break value;
            }
        }
        if Instant::now() > deadline {
            return Err("the window never answered".into());
        }
    };
    check(answer["ok"] == false && answer["why"] == "watch only", || {
        format!("the window's answer: {answer}")
    })?;
    check(tab.provider.interrupts() == 0, || "the provider was interrupted".into())
}

fn steer_device_interrupt_end_to_end(dir: &Path) -> Result<(), String> {
    let world = World::new(dir, Limits::default());
    let window = Window::open(&world, "rs-steer");
    let tab = window.tab();
    window.start_turn(&tab, "t1")?;
    let key = tab.key.as_str();
    let mut phone = world.phone_as(STEER);
    phone.send(json!({ "kind": "subscribe", "key": key }));
    let lines = phone.until(&[&window], "the snapshot", |l| l["kind"] == "snapshot")?;
    let generation = lines.last().unwrap()["generation"].as_u64().ok_or("no generation")?;
    phone.send(json!({ "kind": "act", "key": key, "cmd": interrupt("s1", "t1", generation) }));
    let lines = phone.until(&[&window], "the result", |l| l["kind"] == "result")?;
    let result = lines.last().unwrap();
    check(command_id_of(result) == Some("s1") && result["ok"] == true, || {
        format!("{result}")
    })?;
    check(tab.provider.interrupts() == 1, || {
        format!("the provider was interrupted {} times", tab.provider.interrupts())
    })?;
    // A tab no row names is refused by serve, with the command's id.
    phone.send(json!({ "kind": "act", "key": "k-0000000000000000", "cmd": interrupt("s2", "t1", generation) }));
    let lines = phone.until(&[&window], "the refusal", |l| l["kind"] == "result")?;
    let refused = lines.last().unwrap();
    check(command_id_of(refused) == Some("s2") && refused["ok"] == false, || {
        format!("{refused}")
    })
}

fn moved_tab_resubscribes_at_the_new_owner(dir: &Path) -> Result<(), String> {
    let world = World::new(dir, Limits::default());
    let window = Window::open(&world, "rs-moved");
    let tab = window.tab();
    window.start_turn(&tab, "t1")?;
    let key = tab.key.as_str();
    let other = FakeWindow::open(&world);
    let mut phone = world.phone_as(STEER);
    phone.send(json!({ "kind": "subscribe", "key": key }));
    phone.until(&[&window], "the snapshot from the first window", |l| {
        l["kind"] == "snapshot"
    })?;
    check(window.handle.observer_count_for_host_test() == 1, || {
        "the first window has no observer".into()
    })?;

    // The row leaves the first window's file and appears in the other's, as a move does.
    window.publish(&[]);
    other.publish(&[key]);
    let lines = phone.until(&[&window], "the snapshot from the new owner", |l| {
        l["kind"] == "snapshot" && l["frame"]["from"] == other.instance.as_str()
    })?;
    check(
        other.next_of("subscribe", PATIENCE).is_some_and(|l| l["key"] == key),
        || "the new owner was never asked".into(),
    )?;
    check(!lines.iter().any(|l| l["kind"] == "gone"), || {
        format!("the phone was told the tab went: {}", summary(&lines))
    })?;
    // The first window let go of it, and what it still makes of the tab reaches the phone no more.
    let deadline = Instant::now() + PATIENCE;
    while window.handle.observer_count_for_host_test() != 0 {
        if Instant::now() > deadline {
            return Err("the first window still has the observer".into());
        }
        window.turn();
        std::thread::sleep(Duration::from_millis(5));
    }
    window.fold(&tab, 5, "after the move ")?;
    let until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < until {
        window.turn();
        if let Some(late) = phone.recv(Duration::from_millis(10))? {
            return Err(format!("the old owner's frame reached the phone: {late}"));
        }
    }
    phone.seqs_contiguous()
}

fn reconnect_gets_fresh_greeting(dir: &Path) -> Result<(), String> {
    let world = World::new(dir, Limits::default());
    let mut window = Window::open(&world, "rs-reconnect");
    let tab = window.tab();
    window.start_turn(&tab, "t1")?;
    let key = tab.key.as_str();
    let mut phone = world.phone_as(STEER);
    phone.send(json!({ "kind": "subscribe", "key": key }));
    phone.until(&[&window], "the first snapshot", |l| l["kind"] == "snapshot")?;
    let through = window.fold(&tab, 3, "x")?;
    phone.until(&[&window], "the events", |l| l["through"].as_u64() == Some(through))?;

    // The window closes serve's connection: serve connects again and the phone gets the tab afresh, unasked.
    window.rebind();
    let lines = phone.until(&[&window], "a fresh snapshot after the window closed", |l| {
        l["kind"] == "snapshot" && l["key"] == key
    })?;
    let of_key: Vec<&Value> = lines.iter().filter(|l| l["key"] == key).collect();
    check(
        of_key.len() == 2 && of_key[0]["kind"] == "greeting" && of_key[1]["kind"] == "snapshot",
        || format!("a fresh greeting, then the snapshot: {}", summary(&lines)),
    )?;
    check(lines.last().unwrap()["through"].as_u64() == Some(through), || {
        format!("the fresh snapshot is of now: {}", lines.last().unwrap())
    })?;
    phone.seqs_contiguous()?;

    // The phone itself goes and comes back: its new connection starts over, from a fresh greeting and snapshot.
    drop(phone);
    let mut again = world.phone_as(STEER);
    again.send(json!({ "kind": "subscribe", "key": key }));
    let lines = again.until(&[&window], "the snapshot on the new connection", |l| {
        l["kind"] == "snapshot"
    })?;
    check(lines[0]["seq"] == 1, || {
        format!("a new connection counts from 1: {}", lines[0])
    })?;
    check(
        lines.len() == 2 && lines[0]["kind"] == "greeting" && lines[1]["kind"] == "snapshot",
        || summary(&lines),
    )
}

fn two_processes_one_endpoint(dir: &Path) -> Result<(), String> {
    let world = World::new(dir, Limits::default());
    let first = Window::open(&world, "rs-two-a");
    let second = Window::open(&world, "rs-two-b");
    let a = first.tab();
    let b = second.tab();
    first.start_turn(&a, "t1")?;
    second.start_turn(&b, "t1")?;
    let both = [&first, &second];
    let mut phone = world.phone_as(STEER);
    phone.send(json!({ "kind": "subscribe", "key": a.key.as_str() }));
    phone.send(json!({ "kind": "subscribe", "key": b.key.as_str() }));
    let mut lines = Vec::new();
    for key in [a.key.as_str(), b.key.as_str()] {
        if !lines.iter().any(|l: &Value| l["kind"] == "snapshot" && l["key"] == key) {
            lines.extend(phone.until(&both, "both snapshots", |l| l["kind"] == "snapshot" && l["key"] == key)?);
        }
    }
    let through_a = first.fold(&a, 4, "a")?;
    let through_b = second.fold(&b, 6, "b")?;
    let done = |lines: &[Value], key: &str, through: u64| {
        lines
            .iter()
            .any(|l| l["key"] == key && l["through"].as_u64() == Some(through))
    };
    while !(done(&lines, a.key.as_str(), through_a) && done(&lines, b.key.as_str(), through_b)) {
        let more = phone.until(&both, "both tabs' events", |l| l["kind"] == "events")?;
        lines.extend(more);
    }
    check(contiguous(&lines, a.key.as_str())? == through_a, || {
        "the first tab".into()
    })?;
    check(contiguous(&lines, b.key.as_str())? == through_b, || {
        "the second tab".into()
    })?;
    phone.seqs_contiguous()
}

/// A phone that stops reading while its tab streams: serve's queue for it stays within its cap and one frame, serve
/// lets go of the window's observer, and the phone, reading again, gets a fresh snapshot of now and no gap.
fn stall_then_resume(dir: &Path, resume: bool) -> Result<(), String> {
    const CAP: usize = 64 * 1024;
    let limits = Limits {
        out_cap: CAP,
        write_stall: Duration::from_millis(500),
        ..Limits::default()
    };
    let world = World::new(dir, limits);
    let window = Window::open(&world, "rs-stall");
    let tab = window.tab();
    window.start_turn(&tab, "t1")?;
    let key = tab.key.as_str();
    let mut phone = world.phone_as(STEER);
    phone.send(json!({ "kind": "subscribe", "key": key }));
    let mut lines = phone.until(&[&window], "the snapshot", |l| l["kind"] == "snapshot")?;

    // The phone reads nothing more while 4 MB stream.
    let text = "y".repeat(1024);
    for _ in 0..40 {
        window.fold(&tab, 100, &text)?;
    }
    let deadline = Instant::now() + PATIENCE;
    loop {
        window.turn();
        let dropped = world.hub.gauges().iter().any(|(_, overflows)| *overflows > 0);
        if dropped && window.handle.observer_count_for_host_test() == 0 {
            break;
        }
        if Instant::now() > deadline {
            return Err(format!(
                "serve never let go: gauges {:?}, observers {}",
                world.hub.gauges(),
                window.handle.observer_count_for_host_test()
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // A window's frame is at most its writer budget, 1 MiB; one frame always fits an empty queue.
    let bound = CAP + 1024 * 1024 + 64 * 1024;
    for (high, _) in world.hub.gauges() {
        check(high <= bound, || {
            format!("serve held {high} bytes for one phone, over {bound}")
        })?;
    }
    // More streams while the phone is away; none of it is kept for it.
    let through = window.fold(&tab, 50, &text)?;
    for (high, _) in world.hub.gauges() {
        check(high <= bound, || format!("serve held {high} bytes after more came"))?;
    }
    if !resume {
        return Ok(());
    }
    lines.extend(phone.until(&[&window], "a fresh snapshot of now", |l| {
        l["kind"] == "snapshot" && l["through"].as_u64() == Some(through)
    })?);
    let snapshots = lines.iter().filter(|l| l["kind"] == "snapshot").count();
    check(snapshots >= 2, || format!("no fresh snapshot: {}", summary(&lines)))?;
    check(contiguous(&lines, key)? == through, || "the frames end at now".into())?;
    phone.seqs_contiguous()?;
    // And it follows on from there.
    let later = window.fold(&tab, 3, "z")?;
    lines.extend(phone.until(&[&window], "events after the resync", |l| {
        l["kind"] == "events" && l["through"].as_u64() == Some(later)
    })?);
    check(contiguous(&lines, key)? == later, || "events after the resync".into())?;
    check(window.handle.observer_count_for_host_test() == 1, || {
        "one observer again".into()
    })
}

fn stalled_phone_bounds_serve_memory(dir: &Path) -> Result<(), String> {
    stall_then_resume(dir, false)
}

fn phone_resumes_with_a_snapshot_after_overflow(dir: &Path) -> Result<(), String> {
    stall_then_resume(dir, true)
}

fn revoked_device_gets_nothing_more(dir: &Path) -> Result<(), String> {
    let world = World::new(dir, Limits::default());
    let window = Window::open(&world, "rs-revoked");
    let tab = window.tab();
    window.start_turn(&tab, "t1")?;
    let key = tab.key.as_str();
    let mut phone = world.phone_as(STEER);
    phone.send(json!({ "kind": "subscribe_rows" }));
    phone.send(json!({ "kind": "subscribe", "key": key }));
    phone.until(&[&window], "the snapshot", |l| l["kind"] == "snapshot")?;
    let through = window.fold(&tab, 3, "before ")?;
    phone.until(&[&window], "the events", |l| l["through"].as_u64() == Some(through))?;
    // Quiet: everything sent so far has been read.
    let until = Instant::now() + Duration::from_millis(300);
    while Instant::now() < until {
        window.turn();
        while phone.recv(Duration::from_millis(5))?.is_some() {}
    }

    eitri_core::remote_devices::update(&world.store, |store| store.remove(STEER.0)).map_err(|e| e.to_string())?;
    // The tab streams and the rows change; none of it may reach the phone, by any path.
    window.fold(&tab, 20, "after ")?;
    window.publish(&[]);
    let deadline = Instant::now() + PATIENCE;
    let mut after = Vec::new();
    loop {
        window.turn();
        phone.stream.set_read_timeout(Some(Duration::from_millis(5))).unwrap();
        let mut chunk = [0u8; 4096];
        match phone.stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => after.extend_from_slice(&chunk[..n]),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(_) => break,
        }
        if Instant::now() > deadline {
            return Err("the revoked phone's connection is still open".into());
        }
    }
    check(after.is_empty(), || {
        format!(
            "{} bytes reached the revoked phone: {:?}",
            after.len(),
            String::from_utf8_lossy(&after)
        )
    })?;
    let deadline = Instant::now() + PATIENCE;
    while window.handle.observer_count_for_host_test() != 0 {
        if Instant::now() > deadline {
            return Err("the window still serves the revoked phone's subscription".into());
        }
        window.turn();
        std::thread::sleep(Duration::from_millis(5));
    }
    // And it cannot come back with the same token.
    let mut again = world.phone();
    again.hello(STEER.0, Some(STEER.1));
    let got = again.bytes_until_closed(PATIENCE)?;
    check(got.is_empty(), || {
        format!("{} bytes to the revoked device's new connection", got.len())
    })
}

fn devices_connected_reaches_every_panel(dir: &Path) -> Result<(), String> {
    let world = World::new(dir, Limits::default());
    let first = FakeWindow::open(&world);
    let second = FakeWindow::open(&world);
    let phone = world.phone_as(STEER);
    let mut seen = BTreeMap::new();
    for window in [&first, &second] {
        let line = window
            .next_of("devices_connected", PATIENCE)
            .ok_or_else(|| format!("{} was never told", window.instance))?;
        let devices = line["devices"].as_array().cloned().unwrap_or_default();
        check(
            devices.len() == 1
                && devices[0]["device"] == STEER.0
                && devices[0]["host"] == "localhost"
                && devices[0]["peer"] == "unix",
            || format!("{}: {line}", window.instance),
        )?;
        seen.insert(window.instance.clone(), line);
    }
    // A phone still saying hello is not reported.
    let _enrolling = world.phone();
    drop(phone);
    for window in [&first, &second] {
        let line = window
            .next_of("devices_connected", PATIENCE)
            .ok_or_else(|| format!("{} was not told of the close", window.instance))?;
        check(line["devices"].as_array().is_some_and(Vec::is_empty), || {
            format!("{}: {line}", window.instance)
        })?;
    }
    check(world.hub.connections() <= 1, || {
        format!("{} connections left", world.hub.connections())
    })
}

/// A window that opens while a phone is connected is told who is connected without waiting for another phone to come
/// or go, and so is a window whose socket is bound again.
fn a_window_opened_later_is_told_who_is_connected(dir: &Path) -> Result<(), String> {
    let world = World::new(dir, Limits::default());
    let first = FakeWindow::open(&world);
    let _phone = world.phone_as(WATCH);
    first
        .next_of("devices_connected", PATIENCE)
        .ok_or("the first window was never told")?;
    let names_the_phone = |line: &Value| {
        let devices = line["devices"].as_array().cloned().unwrap_or_default();
        devices.len() == 1 && devices[0]["device"] == WATCH.0
    };
    let mut later = FakeWindow::open(&world);
    let line = later
        .next_of("devices_connected", PATIENCE)
        .ok_or("a window opened after the phone connected was never told")?;
    check(names_the_phone(&line), || format!("{}: {line}", later.instance))?;
    // Told once: the next look does not tell it again.
    check(
        later.next_of("devices_connected", Duration::from_secs(3)).is_none(),
        || format!("{} was told twice", later.instance),
    )?;
    // The same window's socket bound again is a window to tell again.
    later.rebind(&world);
    let line = later
        .next_of("devices_connected", PATIENCE)
        .ok_or("a socket bound again was never told")?;
    check(names_the_phone(&line), || format!("{}: {line}", later.instance))
}

fn a_refused_more_keeps_the_subscription(dir: &Path) -> Result<(), String> {
    let world = World::new(dir, Limits::default());
    let window = Window::open(&world, "rs-more");
    let tab = window.tab();
    window.start_turn(&tab, "t1")?;
    let key = tab.key.as_str();
    let mut phone = world.phone_as(STEER);
    phone.send(json!({ "kind": "subscribe", "key": key }));
    let lines = phone.until(&[&window], "the snapshot", |l| l["kind"] == "snapshot")?;
    let generation = lines.last().unwrap()["generation"].as_u64().ok_or("no generation")?;
    // A `more` the window carries out comes back as a page through the subscription.
    phone.send(json!({ "kind": "more", "key": key, "generation": generation, "before_turn": 1 }));
    let lines = phone.until(&[&window], "the page", |l| {
        l["kind"] == "page" || l["kind"] == "refused"
    })?;
    let page = lines.last().unwrap();
    check(
        page["kind"] == "page" && page["key"] == key && page["from_turn"] == 0 && page["through_turn"] == 0,
        || summary(&lines),
    )?;
    // One that names a session the tab no longer holds is turned down by the window; the tab is still followed.
    phone.send(json!({ "kind": "more", "key": key, "generation": generation + 1, "before_turn": 1 }));
    let lines = phone.until(&[&window], "the answer to more", |l| l["kind"] == "refused")?;
    check(
        lines.last().unwrap()["key"] == key && lines.last().unwrap()["why"] == "the session changed",
        || summary(&lines),
    )?;
    let through = window.fold(&tab, 3, "after more ")?;
    phone.until(&[&window], "the events after a refused more", |l| {
        l["kind"] == "events" && l["through"].as_u64() == Some(through)
    })?;
    check(window.handle.observer_count_for_host_test() == 1, || {
        format!("{} observers", window.handle.observer_count_for_host_test())
    })?;
    phone.send(json!({ "kind": "subscribe", "key": key }));
    let lines = phone.until(&[&window], "the answer to a second subscribe", |l| {
        l["kind"] == "refused"
    })?;
    check(lines.last().unwrap()["why"] == "already subscribed", || summary(&lines))?;
    phone.seqs_contiguous()
}

/// Reads what reaches the phone for `within`, turning `windows`.
fn drain(phone: &mut Phone, windows: &[&Window], within: Duration) -> Result<Vec<Value>, String> {
    let until = Instant::now() + within;
    let mut got = Vec::new();
    while Instant::now() < until {
        turn_all(windows);
        while let Some(line) = phone.recv(Duration::from_millis(5))? {
            got.push(line);
        }
    }
    Ok(got)
}

fn a_window_that_turns_every_connection_away_is_tried_with_pauses(dir: &Path) -> Result<(), String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let world = World::new(dir, Limits::default());
    let instance = new_instance();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counted = accepted.clone();
    // What a window at its connection cap does: one refusal with no key, and the connection closed.
    let _listener = RvListener::bind(&world.control, &instance, move |mut stream| {
        counted.fetch_add(1, Ordering::SeqCst);
        let _ = stream.write_all(format!("{}\n", refused_line(None, RV_REFUSED_FULL)).as_bytes());
        true
    })
    .unwrap();
    let publisher = Publisher::start(&world.control, &instance, project(), window_ref(&instance)).unwrap();
    let key = "k-00000000000000b2";
    publisher.offer_now(vec![row(&instance, key, 1)]);
    let mut phone = world.phone_as(STEER);
    phone.send(json!({ "kind": "subscribe", "key": key }));
    let lines = phone.until(&[], "the tab given up", |l| l["kind"] == "gone" && l["key"] == key)?;
    let tries = accepted.load(Ordering::SeqCst);
    publisher.stop();
    check(!lines.iter().any(|l| l["why"] == RV_REFUSED_FULL), || {
        format!(
            "the window's refusal of a connection reached the phone: {}",
            summary(&lines)
        )
    })?;
    check((2..=12).contains(&tries), || {
        format!("the window was tried {tries} times")
    })?;
    check(lines.len() <= 3, || {
        format!("{} frames: {}", lines.len(), summary(&lines))
    })?;
    phone.send(json!({ "kind": "ping" }));
    phone.until(&[], "the pong after", |l| l["kind"] == "pong")?;
    phone.seqs_contiguous()
}

fn a_full_panel_is_not_hammered(dir: &Path) -> Result<(), String> {
    let world = World::new(dir, Limits::default());
    let window = Window::open(&world, "rs-full");
    let tab = window.tab();
    window.start_turn(&tab, "t1")?;
    let key = tab.key.as_str();
    let socket = rv_socket_path(&world.control, &window.instance).unwrap();
    let held: Vec<RvClient> = (0..RV_MAX_CONNECTIONS)
        .map(|_| RvClient::connect(&socket).unwrap())
        .collect();
    // The window is full: one more connection is turned away.
    let mut probe = RvClient::connect(&socket).unwrap();
    let turned = probe.recv(PATIENCE).map_err(|e| e.to_string())?.unwrap_or_default();
    check(turned.contains(RV_REFUSED_FULL), || format!("the window said {turned}"))?;
    let mut phone = world.phone_as(STEER);
    phone.send(json!({ "kind": "subscribe", "key": key }));
    let lines = phone.until(&[&window], "the tab given up", |l| {
        l["kind"] == "gone" && l["key"] == key
    })?;
    check(!lines.iter().any(|l| l["why"] == RV_REFUSED_FULL), || {
        format!(
            "the window's refusal of a connection reached the phone: {}",
            summary(&lines)
        )
    })?;
    check(lines.len() <= 3, || {
        format!("{} frames: {}", lines.len(), summary(&lines))
    })?;
    let after = drain(&mut phone, &[&window], Duration::from_millis(500))?;
    check(after.is_empty(), || format!("still coming: {}", summary(&after)))?;
    // Room again: the phone follows the tab.
    drop(held);
    drop(probe);
    phone.send(json!({ "kind": "subscribe", "key": key }));
    phone.until(&[&window], "the snapshot once there is room", |l| {
        l["kind"] == "snapshot" && l["key"] == key
    })?;
    phone.seqs_contiguous()
}
