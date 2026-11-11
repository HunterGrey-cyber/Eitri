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
use eitri_core::remote_audit::{AuditLog, Filter, JournalTarget, Record, WriterKind};
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
use eitri_remote::serve::session::{Audit, Hub, Limits};
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
        (
            "bad_token_is_recorded_without_the_token",
            bad_token_is_recorded_without_the_token,
        ),
        (
            "revoked_device_is_recorded_as_revoked",
            revoked_device_is_recorded_as_revoked,
        ),
        (
            "host_origin_and_path_refusals_are_recorded",
            host_origin_and_path_refusals_are_recorded,
        ),
        ("watch_device_act_is_recorded_once", watch_device_act_is_recorded_once),
        (
            "push_endpoint_set_records_the_host_only",
            push_endpoint_set_records_the_host_only,
        ),
        (
            "audit_mine_returns_only_this_devices_records",
            audit_mine_returns_only_this_devices_records,
        ),
        (
            "finished_keys_wake_only_belled_tabs",
            finished_keys_wake_only_belled_tabs,
        ),
        ("connected_device_is_not_woken", connected_device_is_not_woken),
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
    /// The audit directory beside the store, as `serve` has it.
    audit: PathBuf,
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
        let audit = eitri_core::remote_audit::audit_dir(&store_dir);
        let hub = World::start_hub(&control, &store, &audit, limits);
        let socket = serve_socket_path(&control).unwrap();
        let listener = UnixListener::bind(&socket).expect("serve's socket binds");
        let cfg = ServeConfig {
            listen: Listen::Socket,
            port: 41000,
            origins: Vec::new(),
            audit_keep_days: eitri_core::remote_audit::AUDIT_KEEP_DAYS_DEFAULT,
            notify: true,
        };
        serve_unix(listener, Arc::new(cfg), hub.clone()).unwrap();
        World {
            control,
            store,
            audit,
            hub,
            socket,
        }
    }

    /// A hub writing the audit log as `serve` does (no journal here: a test never writes to the real one).
    fn start_hub(control: &Path, store: &Path, audit: &Path, limits: Limits) -> Arc<Hub> {
        let log = AuditLog::open(audit, WriterKind::Serve, Some(90), JournalTarget::Off).expect("the audit log opens");
        Hub::start_audited(control, store, limits, Audit::new(log, audit)).expect("the hub starts")
    }

    /// Every record so far, newest first.
    fn records(&self) -> Vec<Record> {
        eitri_core::remote_audit::read(&self.audit, &Filter::default(), None).unwrap_or_default()
    }

    /// Waits until a record `wanted` accepts is in the log; every record then.
    fn record_where(&self, what: &str, wanted: impl Fn(&Record) -> bool) -> Result<Vec<Record>, String> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let records = self.records();
            if records.iter().any(&wanted) {
                return Ok(records);
            }
            if Instant::now() > deadline {
                return Err(format!("no record of {what}: {}", lines_of(&records)));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The log's bytes, every file.
    fn log_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        for entry in std::fs::read_dir(&self.audit).unwrap() {
            let path = entry.unwrap().path();
            if path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("audit.jsonl"))
            {
                bytes.extend(std::fs::read(path).unwrap());
            }
        }
        bytes
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
    active: Arc<std::sync::atomic::AtomicUsize>,
}

impl FakeWindow {
    fn open(world: &World) -> FakeWindow {
        let instance = new_instance();
        let (tx, lines) = mpsc::channel();
        let me = instance.clone();
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let busy = active.clone();
        let listener = RvListener::bind(&world.control, &instance, move |stream| {
            let (tx, me) = (tx.clone(), me.clone());
            busy.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let busy = busy.clone();
            std::thread::spawn(move || {
                serve_fake(stream, &tx, &me);
                busy.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            });
            true
        })
        .unwrap();
        let publisher = Publisher::start(&world.control, &instance, project(), window_ref(&instance)).unwrap();
        FakeWindow {
            instance,
            lines,
            listener: Some(listener),
            publisher: Some(publisher),
            active,
        }
    }

    /// Closes its `rv-` socket and binds the same instance's again: a new listener at the same path.
    fn rebind(&mut self, world: &World) {
        drop(self.listener.take());
        let (tx, lines) = mpsc::channel();
        let me = self.instance.clone();
        let busy = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        self.active = busy.clone();
        self.listener = Some(
            RvListener::bind(&world.control, &self.instance, move |stream| {
                let (tx, me) = (tx.clone(), me.clone());
                busy.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let busy = busy.clone();
                std::thread::spawn(move || {
                    serve_fake(stream, &tx, &me);
                    busy.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                });
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

    fn no_act_before_pages(&self, phone: &mut Phone, key: &str) -> Result<(), String> {
        for before_turn in 1..=20 {
            phone.send(json!({ "kind": "more", "key": key, "generation": 1, "before_turn": before_turn }));
            let deadline = Instant::now() + PATIENCE;
            loop {
                let line = self
                    .lines
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .map_err(|_| "the page marker never arrived")?;
                check(line["kind"] != "act", || format!("an act reached the window: {line}"))?;
                if line["kind"] == "more" && line["before_turn"] == before_turn {
                    break;
                }
            }
        }
        Ok(())
    }

    fn finish_report_connections(&self, world: &World) -> Result<(), String> {
        let mut client =
            RvClient::connect(&rv_socket_path(&world.control, &self.instance).unwrap()).map_err(|e| e.to_string())?;
        client.send(&RvInbound::Ping { v: 1 }).map_err(|e| e.to_string())?;
        let pong = client
            .recv(PATIENCE)
            .map_err(|e| e.to_string())?
            .ok_or("no report marker pong")?;
        check(pong.contains("pong"), || format!("the report marker answered {pong}"))?;
        drop(client);
        let deadline = Instant::now() + PATIENCE;
        while self.active.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            check(Instant::now() < deadline, || "a report connection stayed busy".into())?;
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }

    /// The next line of `kind` that reached it, skipping others, within `within`.
    /// The next `devices_connected` that names a phone: `serve`'s first report names nobody, and a window bound before
    /// the reporting thread's first look takes it first.
    fn next_naming(&self, within: Duration) -> Option<Value> {
        let deadline = Instant::now() + within;
        loop {
            let line = self.next_of("devices_connected", deadline.saturating_duration_since(Instant::now()))?;
            if line["devices"].as_array().is_some_and(|d| !d.is_empty()) {
                return Some(line);
            }
        }
    }

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

pub(crate) struct Phone {
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
    pub(crate) fn connect(socket: &Path) -> Phone {
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

    pub(crate) fn send(&mut self, mut body: Value) {
        self.seq += 1;
        body["v"] = json!(1);
        body["seq"] = json!(self.seq);
        body["ack"] = json!(self.seen);
        self.send_raw(body);
    }

    /// A close frame with code 1000, as a phone that leaves sends it.
    pub(crate) fn close(&mut self) {
        let key = [0x11u8, 0x22, 0x33, 0x44];
        let mut bytes = vec![0x88, 0x82];
        bytes.extend_from_slice(&key);
        bytes.extend([0x03 ^ key[0], 0xe8 ^ key[1]]);
        let _ = self.stream.write_all(&bytes);
    }

    pub(crate) fn hello(&mut self, device: &str, token: Option<&str>) {
        let mut body = json!({ "kind": "hello", "device": device, "layout": "slab", "protocol": 1 });
        if let Some(token) = token {
            body["token"] = json!(token);
        }
        self.send(body);
    }

    /// The next whole text message, `Ok(None)` when none came within `within`, `Err` when the server closed.
    pub(crate) fn recv(&mut self, within: Duration) -> Result<Option<Value>, String> {
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
    pub(crate) fn bytes_until_closed(&mut self, within: Duration) -> Result<Vec<u8>, String> {
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
    use eitri_remote::serve::session::{HelloReadForTests, HelloWaitForTests};
    let (wait, budgets, step) = HelloWaitForTests::new();
    let limits = Limits {
        hello_within: Duration::from_millis(300),
        hello_wait_for_tests: Some(wait.clone()),
        ..Limits::default()
    };
    let world = World::new(dir, limits);
    let mut silent = world.phone();
    check(
        budgets.recv_timeout(PATIENCE).unwrap() == Duration::from_millis(300),
        || "the initial hello budget changed".into(),
    )?;
    step.send(HelloReadForTests::Deadline).unwrap();
    let got = silent.bytes_until_closed(PATIENCE)?;
    check(got.is_empty(), || format!("{} bytes before the close", got.len()))?;

    // Each ping is read while inside the same hello wait. The remaining budget shrinks and is never reset.
    let mut pinging = world.phone();
    for remaining in [300, 200] {
        check(
            budgets.recv_timeout(PATIENCE).unwrap() == Duration::from_millis(remaining),
            || "a ping reset the hello budget".into(),
        )?;
        pinging
            .stream
            .write_all(&[0x89, 0x80, 1, 2, 3, 4])
            .map_err(|e| e.to_string())?;
        wait.advance(Duration::from_millis(100));
        step.send(HelloReadForTests::Read).unwrap();
    }
    check(
        budgets.recv_timeout(PATIENCE).unwrap() == Duration::from_millis(100),
        || "the pings did not spend the hello budget".into(),
    )?;
    step.send(HelloReadForTests::Deadline).unwrap();
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
    stand_in.no_act_before_pages(&mut phone, other_key)?;
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
            .next_naming(PATIENCE)
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
    let world = World::new(
        dir,
        Limits {
            report_interval_for_tests: Some(Duration::from_secs(86400)),
            ..Limits::default()
        },
    );
    let first = FakeWindow::open(&world);
    let _phone = world.phone_as(WATCH);
    first.next_naming(PATIENCE).ok_or("the first window was never told")?;
    let names_the_phone = |line: &Value| {
        let devices = line["devices"].as_array().cloned().unwrap_or_default();
        devices.len() == 1 && devices[0]["device"] == WATCH.0
    };
    let mut later = FakeWindow::open(&world);
    world.hub.report_look_for_tests();
    let line = later
        .next_of("devices_connected", PATIENCE)
        .ok_or("a window opened after the phone connected was never told")?;
    check(names_the_phone(&line), || format!("{}: {line}", later.instance))?;
    // Told once: the next look does not tell it again.
    for _ in 0..20 {
        world.hub.report_look_for_tests();
        later.finish_report_connections(&world)?;
        check(
            !later.lines.try_iter().any(|line| line["kind"] == "devices_connected"),
            || format!("{} was told twice", later.instance),
        )?;
    }
    // The same window's socket bound again is a window to tell again.
    later.rebind(&world);
    world.hub.report_look_for_tests();
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

/// Samples twenty advancing retry turns, each followed by the session's ordered pong.
fn drain(phone: &mut Phone, windows: &[&Window], world: &World) -> Result<Vec<Value>, String> {
    let mut got = Vec::new();
    let now = Instant::now();
    for turn in 1..=20 {
        world.hub.tick_sessions_for_tests(now + Duration::from_secs(turn * 60));
        turn_all(windows);
        phone.send(json!({ "kind": "ping" }));
        got.extend(
            phone
                .until(windows, "the retry turn's pong", |line| line["kind"] == "pong")?
                .into_iter()
                .filter(|line| line["kind"] != "pong"),
        );
    }
    Ok(got)
}

fn a_window_that_turns_every_connection_away_is_tried_with_pauses(dir: &Path) -> Result<(), String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let world = World::new(
        dir,
        Limits {
            report_interval_for_tests: Some(Duration::from_secs(86400)),
            ..Limits::default()
        },
    );
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
    // The count includes `serve`'s own report connections: its first, empty report and the one for the phone's open,
    // each one connection that this window turns away as well.
    check((2..=14).contains(&tries), || {
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
    let after = drain(&mut phone, &[&window], &world)?;
    check(after.is_empty(), || format!("still coming: {}", summary(&after)))?;
    // Room again: the phone follows the tab.
    drop(held);
    drop(probe);
    // An answered ping proves a reader slot has been released before the new subscription.
    let deadline = Instant::now() + PATIENCE;
    loop {
        check(Instant::now() < deadline, || {
            "the held readers never released their slots".into()
        })?;
        let mut marker = RvClient::connect(&socket).map_err(|e| e.to_string())?;
        if marker.send(&RvInbound::Ping { v: 1 }).is_err() {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        let mut free = false;
        loop {
            check(Instant::now() < deadline, || {
                "the slot marker was never answered".into()
            })?;
            window.turn();
            match marker.recv(Duration::from_millis(5)) {
                Ok(Some(line)) if line.contains("pong") => {
                    free = true;
                    break;
                }
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => {}
            }
        }
        if free {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    // Dropping the marker closes its peer immediately, but only the server's reader releases the slot.
    let deadline = Instant::now() + PATIENCE;
    let mut released = false;
    for _ in 0..10_000 {
        check(Instant::now() < deadline, || {
            "the marker reader never released its slot".into()
        })?;
        window.turn();
        if window.socket.as_ref().unwrap().live_readers_for_host_test() == 0 {
            released = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    check(released, || "the reader release exceeded its observation turns".into())?;
    phone.send(json!({ "kind": "subscribe", "key": key }));
    phone.until(&[&window], "the snapshot once there is room", |l| {
        l["kind"] == "snapshot" && l["key"] == key
    })?;
    phone.seqs_contiguous()
}

fn lines_of(records: &[Record]) -> String {
    records.iter().map(|r| r.line.as_str()).collect::<Vec<_>>().join("\n")
}

fn field<'a>(record: &'a Record, name: &str) -> &'a Value {
    record.fields.get(name).unwrap_or(&Value::Null)
}

fn bad_token_is_recorded_without_the_token(dir: &Path) -> Result<(), String> {
    let world = World::new(dir, Limits::default());
    let sentinel = "token-sentinel-4c1e";
    let mut phone = world.phone();
    phone.hello(STEER.0, Some(sentinel));
    phone.bytes_until_closed(PATIENCE)?;
    let records = world.record_where("the bad token", |r| r.event == "refused")?;
    let refused: Vec<&Record> = records.iter().filter(|r| r.event == "refused").collect();
    check(refused.len() == 1, || {
        format!("{} refusals: {}", refused.len(), lines_of(&records))
    })?;
    let r = refused[0];
    check(
        field(r, "reason") == "bad_token"
            && field(r, "device") == STEER.0
            && field(r, "device_name") == "phone"
            && field(r, "listener") == "socket"
            && field(r, "peer") == "unix",
        || format!("the refusal: {}", r.line),
    )?;
    check(!records.iter().any(|r| r.event == "conn_open"), || {
        "a refused hello opened a connection".into()
    })?;
    // The sentinel is the only token sent; it is long enough that no record word holds it by chance.
    check(!String::from_utf8_lossy(&world.log_bytes()).contains(sentinel), || {
        "the token is in the log".into()
    })
}

fn revoked_device_is_recorded_as_revoked(dir: &Path) -> Result<(), String> {
    let world = World::new(dir, Limits::default());
    // Seen by serve while it runs: the store forgets the device, and serve remembers it left.
    let mut first = world.phone_as(STEER);
    first.send(json!({ "kind": "ping" }));
    first.until(&[], "the pong", |l| l["kind"] == "pong")?;
    eitri_core::remote_devices::update(&world.store, |store| store.remove(STEER.0)).map_err(|e| e.to_string())?;
    first.bytes_until_closed(PATIENCE)?;
    let mut again = world.phone();
    again.hello(STEER.0, Some(STEER.1));
    again.bytes_until_closed(PATIENCE)?;
    let records = world.record_where("the revoked token", |r| field(r, "reason") == "revoked_token")?;
    let revoked = records.iter().find(|r| field(r, "reason") == "revoked_token").unwrap();
    check(
        field(revoked, "device") == STEER.0 && field(revoked, "device_name") == "phone",
        || format!("the refusal names the device as it was: {}", revoked.line),
    )?;
    check(
        records
            .iter()
            .any(|r| r.event == "conn_close" && field(r, "device") == STEER.0 && field(r, "why") == "revoked"),
        || format!("the revoked phone's close: {}", lines_of(&records)),
    )?;
    // A device that was never in the store is a bad token, not a revoked one.
    let mut stranger = world.phone();
    stranger.hello("d9", Some("nothing"));
    stranger.bytes_until_closed(PATIENCE)?;
    world.record_where("the stranger", |r| {
        field(r, "device") == "d9" && field(r, "reason") == "bad_token"
    })?;

    // After a restart: the store no longer names the watch device, and the only trace is the log's `revoke`, as
    // `eitri remote revoke` writes it.
    eitri_core::remote_devices::update(&world.store, |store| store.remove(WATCH.0)).map_err(|e| e.to_string())?;
    let mut cli = AuditLog::open(&world.audit, WriterKind::Cli, None, JournalTarget::Off).map_err(|e| e.to_string())?;
    cli.record(eitri_core::remote_audit::Event::Revoke {
        device: eitri_core::remote_audit::DeviceRef {
            id: WATCH.0.into(),
            name: "tablet".into(),
        },
        by: "cli".into(),
    })
    .map_err(|e| e.to_string())?;
    let restarted_dir = dir.join("restarted");
    let socket = restarted_dir.join("serve.sock");
    std::fs::create_dir_all(&restarted_dir).unwrap();
    let hub = World::start_hub(&world.control, &world.store, &world.audit, Limits::default());
    let cfg = ServeConfig {
        listen: Listen::Socket,
        port: 41000,
        origins: Vec::new(),
        audit_keep_days: eitri_core::remote_audit::AUDIT_KEEP_DAYS_DEFAULT,
        notify: true,
    };
    serve_unix(UnixListener::bind(&socket).unwrap(), Arc::new(cfg), hub).unwrap();
    let mut late = Phone::connect(&socket);
    late.hello(WATCH.0, Some(WATCH.1));
    late.bytes_until_closed(PATIENCE)?;
    let records = world.record_where("the revoked token after a restart", |r| {
        field(r, "reason") == "revoked_token" && field(r, "device") == WATCH.0
    })?;
    let late = records
        .iter()
        .find(|r| field(r, "reason") == "revoked_token" && field(r, "device") == WATCH.0)
        .unwrap();
    check(field(late, "device_name") == "tablet", || format!("{}", late.line))
}

fn host_origin_and_path_refusals_are_recorded(dir: &Path) -> Result<(), String> {
    let world = World::new(dir, Limits::default());
    let upgrade = |target: &str, extra: &str| {
        format!(
            "GET {target} HTTP/1.1\r\n{extra}Upgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
    };
    for request in [
        upgrade("/v1/ws", "Host: evil.example\r\n"),
        upgrade("/v1/ws", "Host: localhost\r\nOrigin: https://evil.example\r\n"),
        upgrade("/eitri/v1/ws", "Host: localhost\r\n"),
    ] {
        let mut stream = UnixStream::connect(&world.socket).unwrap();
        stream.set_read_timeout(Some(PATIENCE)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut answer = Vec::new();
        let _ = stream.read_to_end(&mut answer);
    }
    let deadline = Instant::now() + PATIENCE;
    let records = loop {
        let records = world.records();
        if records.iter().filter(|r| r.event == "refused").count() >= 3 || Instant::now() > deadline {
            break records;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let mut reasons: Vec<&str> = records
        .iter()
        .filter(|r| r.event == "refused")
        .map(|r| field(r, "reason").as_str().unwrap_or("?"))
        .collect();
    reasons.sort_unstable();
    check(reasons == ["host", "origin", "path_prefix"], || lines_of(&records))?;
    for r in records.iter().filter(|r| r.event == "refused") {
        check(
            field(r, "listener") == "socket" && field(r, "peer") == "unix" && r.fields.get("device").is_none(),
            || {
                format!(
                    "a refusal before hello names its listener and peer, and no device: {}",
                    r.line
                )
            },
        )?;
    }
    Ok(())
}

fn watch_device_act_is_recorded_once(dir: &Path) -> Result<(), String> {
    let world = World::new(dir, Limits::default());
    let stand_in = FakeWindow::open(&world);
    let key = "k-00000000000000c1";
    stand_in.publish(&[key]);
    let mut phone = world.phone_as(WATCH);
    phone.send(json!({ "kind": "subscribe", "key": key }));
    phone.until(&[], "the stand-in's snapshot", |l| l["kind"] == "snapshot")?;
    let sentinel = "act-text-sentinel-91";
    phone.send(json!({ "kind": "act", "key": key, "cmd": { "type": "send", "text": sentinel, "command_id": "w1" } }));
    let lines = phone.until(&[], "the result", |l| l["kind"] == "result")?;
    check(lines.last().unwrap()["why"] == "watch only", || summary(&lines))?;
    let notice = stand_in
        .next_of("refused_notice", PATIENCE)
        .ok_or("the window was not told of the refusal")?;
    check(
        notice["device_id"] == WATCH.0
            && notice["device"] == "tablet"
            && notice["key"] == key
            && notice["reason"] == "grant",
        || format!("the notice: {notice}"),
    )?;
    check(
        !notice.to_string().contains(sentinel) && !notice.to_string().contains("w1"),
        || format!("the notice carries the command: {notice}"),
    )?;
    stand_in.no_act_before_pages(&mut phone, key)?;
    let records = world.record_where("the refusal", |r| field(r, "reason") == "grant")?;
    let grants: Vec<&Record> = records.iter().filter(|r| field(r, "reason") == "grant").collect();
    check(grants.len() == 1, || {
        format!("{} grant refusals: {}", grants.len(), lines_of(&records))
    })?;
    check(
        field(grants[0], "device") == WATCH.0 && field(grants[0], "key") == key && field(grants[0], "kind") == "send",
        || grants[0].line.clone(),
    )?;
    check(!String::from_utf8_lossy(&world.log_bytes()).contains(sentinel), || {
        "the command's text is in the log".into()
    })
}

fn push_endpoint_set_records_the_host_only(dir: &Path) -> Result<(), String> {
    let world = World::new(dir, Limits::default());
    let stand_in = FakeWindow::open(&world);
    stand_in.publish(&["k-00000000000000d1"]);
    let mut phone = world.phone_as(STEER);
    // User information in the URL, as some push services carry a topic's credentials there: the window refuses to
    // keep it, so nothing is recorded as set.
    let with_user = concat!("https://topic-user:topic-secret", "@", "ntfy.example.org/up/AbCdEf123");
    phone.send(json!({ "kind": "set_push_endpoint", "url": with_user }));
    stand_in
        .next_of("set_push_endpoint", PATIENCE)
        .ok_or("the endpoint reached no window")?;
    phone.send(json!({ "kind": "set_push_endpoint", "url": "https://Push.example.net:8443/up/AbCdEf123?x=1" }));
    stand_in
        .next_of("set_push_endpoint", PATIENCE)
        .ok_or("the second endpoint reached no window")?;
    let records = world.record_where("the endpoint", |r| {
        r.event == "push_endpoint_set" && field(r, "host") == "push.example.net"
    })?;
    let sets: Vec<&Record> = records.iter().filter(|r| r.event == "push_endpoint_set").collect();
    check(sets.len() == 1, || {
        format!(
            "an endpoint the window refuses is recorded as set: {}",
            lines_of(&records)
        )
    })?;
    let set = sets[0];
    check(
        field(set, "host") == "push.example.net" && field(set, "device") == STEER.0,
        || set.line.clone(),
    )?;
    let log = String::from_utf8_lossy(&world.log_bytes()).into_owned();
    for part in ["topic-secret", "topic-user", "AbCdEf123", "8443", "/up"] {
        check(!log.contains(part), || format!("{part} is in the log"))?;
    }
    Ok(())
}

fn audit_mine_returns_only_this_devices_records(dir: &Path) -> Result<(), String> {
    let world = World::new(dir, Limits::default());
    let mut watcher = world.phone_as(WATCH);
    watcher.send(json!({ "kind": "ping" }));
    watcher.until(&[], "the pong", |l| l["kind"] == "pong")?;
    // A record about another device whose claimed name is this device's id.
    let mut window = AuditLog::open(
        &world.audit,
        WriterKind::Instance("a3f09c1d2e4b5f60".into()),
        None,
        JournalTarget::Off,
    )
    .map_err(|e| e.to_string())?;
    window
        .record(eitri_core::remote_audit::Event::DeviceFlagged {
            device: eitri_core::remote_audit::DeviceRef {
                id: "d5".into(),
                name: STEER.0.into(),
            },
        })
        .map_err(|e| e.to_string())?;
    let now = Instant::now();
    world.hub.audit().set_mine_time_for_tests(now);
    let mut phone = world.phone_as(STEER);
    world.record_where("this phone's open", |r| {
        r.event == "conn_open" && field(r, "device") == STEER.0
    })?;
    phone.send(json!({ "kind": "audit_mine", "limit": 500 }));
    let lines = phone.until(&[], "the records", |l| l["kind"] == "audit_mine")?;
    let answer = lines.last().unwrap();
    let records = answer["records"].as_array().ok_or("no records array")?;
    check(!records.is_empty(), || format!("{answer}"))?;
    check(records.len() <= 200, || format!("{} records", records.len()))?;
    check(records.iter().all(|r| r["device"] == STEER.0), || {
        format!("another device's records: {answer}")
    })?;
    check(records[0]["event"] == "conn_open", || format!("newest first: {answer}"))?;
    // Each answer reads the log: a second ask within a second is turned down, and answered after it.
    phone.send(json!({ "kind": "audit_mine", "limit": 0 }));
    let lines = phone.until(&[], "the refusal", |l| {
        l["kind"] == "refused" || l["kind"] == "audit_mine"
    })?;
    check(lines.last().unwrap()["kind"] == "refused", || summary(&lines))?;
    world.hub.audit().set_mine_time_for_tests(now + Duration::from_secs(1));
    phone.send(json!({ "kind": "audit_mine", "limit": 0 }));
    let lines = phone.until(&[], "no records", |l| l["kind"] == "audit_mine")?;
    check(lines.last().unwrap()["records"] == json!([]), || {
        format!("{}", lines.last().unwrap())
    })?;
    phone.seqs_contiguous()
}

// ---------------------------------------------------------------------------------------------------------------
// The notifier over the hub: whom a wake goes to.

/// Where the notifier's wakes went, in order; every one answered 200.
#[derive(Clone, Default)]
struct Posted(Arc<std::sync::Mutex<Vec<String>>>);

impl eitri_remote::serve::notifier::Poster for Posted {
    fn post(
        &self,
        endpoint: &str,
        _still_due: eitri_remote::serve::notifier::StillDue,
        done: Box<dyn FnOnce(String) + Send>,
    ) -> std::io::Result<()> {
        self.0.lock().unwrap().push(endpoint.to_owned());
        done("200".to_owned());
        Ok(())
    }
}

impl Posted {
    fn now(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

/// Gives both of the world's devices a push endpoint and starts a notifier over its hub, posting to `posted`.
fn notifier_over(world: &World, posted: &Posted) -> Result<DrivenNotifier, String> {
    eitri_core::remote_devices::update(&world.store, |store| {
        for id in [STEER.0, WATCH.0] {
            store.get_mut(id).unwrap().push_endpoint = Some(format!("https://push.example/up/{id}"));
        }
    })
    .map_err(|e| format!("the store: {e}"))?;
    let deadline = Instant::now() + PATIENCE;
    while world.hub.push_targets().len() != 2 {
        if Instant::now() >= deadline {
            return Err("the hub never loaded both push targets".into());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let sources = eitri_remote::serve::notifier::Sources::of_hub(&world.hub, &world.control);
    let (send, events) = mpsc::channel();
    let reader = eitri_core::agents::Reader::start(&world.control, eitri_core::agents::SCHEMA_V, move |event| {
        let _ = send.send(event);
    })
    .map_err(|e| e.to_string())?;
    Ok(DrivenNotifier {
        notifier: eitri_remote::serve::notifier::Notifier::new(sources, posted.clone(), world.hub.audit().clone()),
        _reader: reader,
        events,
        now: Instant::now(),
    })
}

struct DrivenNotifier {
    notifier: eitri_remote::serve::notifier::Notifier<Posted>,
    _reader: eitri_core::agents::Reader,
    events: mpsc::Receiver<eitri_core::agents::ReaderEvent>,
    now: Instant,
}

impl DrivenNotifier {
    fn row(&mut self, key: &str, class: eitri_core::agents::Class) -> Result<(), String> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let event = self
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|e| format!("the notifier never read {key}: {e}"))?;
            let eitri_core::agents::ReaderEvent::Change(change) = event else {
                return Err(format!("{event:?}"));
            };
            let wanted = match &change {
                eitri_core::agents::Change::Row { row } => row.key == key && row.class == class,
                eitri_core::agents::Change::Reset { view } => {
                    view.rows.iter().any(|row| row.key == key && row.class == class)
                }
                _ => false,
            };
            self.notifier.on_change(&change, self.now);
            if wanted {
                return Ok(());
            }
        }
    }

    fn turns(&mut self) {
        for _ in 0..20 {
            self.now += Duration::from_secs(1);
            self.notifier.tick(self.now);
        }
    }
}

fn classed(instance: &str, key: &str, number: u16, class: eitri_core::agents::Class) -> Row {
    Row {
        class,
        ..row(instance, key, number)
    }
}

/// Waits `within` for `posted` to hold `count` wakes.
fn wakes(posted: &Posted, count: usize, within: Duration) -> Vec<String> {
    let deadline = Instant::now() + within;
    while posted.now().len() < count && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    posted.now()
}

fn finished_keys_wake_only_belled_tabs(dir: &Path) -> Result<(), String> {
    use eitri_core::agents::Class;
    let world = World::new(dir, Limits::default());
    let instance = new_instance();
    let rows = Publisher::start(&world.control, &instance, project(), window_ref(&instance)).unwrap();
    let (belled, other) = ("k-00000000000000b1", "k-00000000000000b2");
    rows.offer_now(vec![
        classed(&instance, belled, 1, Class::Working),
        classed(&instance, other, 2, Class::Working),
    ]);
    let posted = Posted::default();
    let mut notifier = notifier_over(&world, &posted)?;
    notifier.row(belled, Class::Working)?;
    // The phone bells one tab, then leaves: its wakes are for when it is not connected.
    let mut phone = world.phone_as(STEER);
    phone.send(json!({ "kind": "set_finished_keys", "keys": [belled] }));
    phone.send(json!({ "kind": "ping" }));
    phone.until(&[], "the pong", |l| l["kind"] == "pong")?;
    phone.close();
    drop(phone);
    let deadline = Instant::now() + PATIENCE;
    while !world.hub.connected_devices().is_empty() {
        if Instant::now() > deadline {
            return Err("the phone was still connected".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    rows.offer_now(vec![
        classed(&instance, belled, 1, Class::Working),
        classed(&instance, other, 2, Class::Quiet),
    ]);
    notifier.row(other, Class::Quiet)?;
    notifier.turns();
    let early = posted.now();
    check(early.is_empty(), || format!("an unbelled tab's end woke {early:?}"))?;
    rows.offer_now(vec![
        classed(&instance, belled, 1, Class::Quiet),
        classed(&instance, other, 2, Class::Quiet),
    ]);
    notifier.row(belled, Class::Quiet)?;
    notifier.turns();
    let got = wakes(&posted, 1, PATIENCE);
    check(got == ["https://push.example/up/d1"], || {
        format!("the belled tab's end woke {got:?}")
    })?;
    let records = world.record_where("the push", |r| r.event == "push")?;
    let pushes: Vec<&Record> = records.iter().filter(|r| r.event == "push").collect();
    check(
        pushes.len() == 1
            && pushes[0].fields["wake"] == "finished"
            && pushes[0].fields["host"] == "push.example"
            && pushes[0].fields["status"] == "200",
        || lines_of(&records),
    )
}

fn connected_device_is_not_woken(dir: &Path) -> Result<(), String> {
    use eitri_core::agents::Class;
    let world = World::new(dir, Limits::default());
    let instance = new_instance();
    let rows = Publisher::start(&world.control, &instance, project(), window_ref(&instance)).unwrap();
    let key = "k-00000000000000c1";
    rows.offer_now(vec![classed(&instance, key, 1, Class::Quiet)]);
    let posted = Posted::default();
    let mut notifier = notifier_over(&world, &posted)?;
    notifier.row(key, Class::Quiet)?;
    let mut phone = world.phone_as(STEER);
    phone.send(json!({ "kind": "ping" }));
    phone.until(&[], "the pong", |l| l["kind"] == "pong")?;
    check(world.hub.connected_devices().contains(STEER.0), || {
        "the steering phone is not counted as connected".to_owned()
    })?;
    rows.offer_now(vec![classed(&instance, key, 1, Class::Needs)]);
    notifier.row(key, Class::Needs)?;
    let got = wakes(&posted, 1, PATIENCE);
    // Anything more would have come with the first.
    notifier.turns();
    let got_later = posted.now();
    check(got == ["https://push.example/up/d2"] && got_later == got, || {
        format!("a needs-you woke {got_later:?}")
    })?;
    let records = world.record_where("the push", |r| r.event == "push")?;
    let pushes: Vec<&Record> = records.iter().filter(|r| r.event == "push").collect();
    check(
        pushes.len() == 1 && pushes[0].fields["device"] == WATCH.0 && pushes[0].fields["wake"] == "needs_you",
        || lines_of(&records),
    )?;
    drop(phone);
    Ok(())
}
