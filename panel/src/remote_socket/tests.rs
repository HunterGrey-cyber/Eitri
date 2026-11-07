//! The socket's tests, and the rig each handler file's tests share.

use super::*;
use crate::agent_panel::tests::*;
use eitri_core::remote_devices::token_hash_hex;
use eitri_core::remote_rv::{rv_socket_path, RvClient};
use std::os::unix::fs::DirBuilderExt;
use std::sync::atomic::AtomicU32;

/// How long any one step may take on a loaded machine; a test that passes never waits it out.
pub(super) const PATIENCE: Duration = Duration::from_secs(30);

const INSTANCE: &str = "b4e1c09a7d3f5e21";

/// A private directory standing in for the control directory, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Scratch {
        static SERIAL: AtomicU32 = AtomicU32::new(0);
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(std::env::temp_dir);
        let path = base.join(format!(
            "rs-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Scratch(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The store: `d1` ("phone", token `tok`) may steer, `d2` ("tablet", token `wtok`) may only watch, `d3` (also
/// "phone", token `tok3`) may steer.
fn write_store(dir: &Path) -> PathBuf {
    let path = dir.join("devices.json");
    let body = serde_json::json!({
        "v": 1,
        "devices": [
            { "id": "d1", "name": "phone", "token_sha256": token_hash_hex("tok"), "grant": "watch+steer" },
            { "id": "d2", "name": "tablet", "token_sha256": token_hash_hex("wtok"), "grant": "watch" },
            { "id": "d3", "name": "phone", "token_sha256": token_hash_hex("tok3"), "grant": "watch+steer" },
        ],
    });
    agent::private_fs::write_private(&path, body.to_string().as_bytes()).unwrap();
    path
}

pub(super) struct Rig {
    pub(super) handle: AgentPanelHandle,
    pub(super) socket: Option<RemoteSocket>,
    pub(super) path: PathBuf,
    pub(super) wake: PanelWake,
    pub(super) dir: PathBuf,
    _scratch: Scratch,
}

impl Rig {
    pub(super) fn new(label: &str) -> Rig {
        Rig::with_deadlines(label, Deadlines::default())
    }

    /// A rig whose connections keep `deadlines` in place of the socket's own.
    pub(super) fn with_deadlines(label: &str, deadlines: Deadlines) -> Rig {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir(label);
        let (handle, _base, _second) = handle_with_two_pages();
        let scratch = Scratch::new();
        let store = write_store(&scratch.0);
        let wake = PanelWake::new().unwrap();
        let socket =
            RemoteSocket::bind_with(&scratch.0, INSTANCE, &store, wake.waker(), deadlines).expect("the socket binds");
        let path = rv_socket_path(&scratch.0, INSTANCE).unwrap();
        Rig {
            handle,
            socket: Some(socket),
            path,
            wake,
            dir,
            _scratch: scratch,
        }
    }

    pub(super) fn socket(&self) -> &RemoteSocket {
        self.socket.as_ref().unwrap()
    }

    pub(super) fn connect(&self) -> RvClient {
        RvClient::connect(&self.path).expect("the client connects")
    }

    /// One tick and one pump.
    pub(super) fn turn(&self) {
        self.handle.tick();
        self.socket().pump(&self.handle);
    }

    /// A new Claude tab whose provider can interrupt, and its row key.
    pub(super) fn tab(&self) -> (TabId, Arc<RecordingProvider>, RowKey) {
        let provider = Arc::new(RecordingProvider::interruptible());
        let conversation =
            agent::AgentConversation::create(provider.clone(), &self.dir, ProjectTrust::Untrusted).unwrap();
        let backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };
        let mut state = self.handle.state.borrow_mut();
        let tab = state.tabs.open(AgentId::Claude);
        let held = state.tabs.get_mut(tab).unwrap();
        held.set_backend(TabBackend::Live(backend));
        let key = held.row_key().clone();
        (tab, provider, key)
    }

    pub(super) fn generation(&self, tab: TabId) -> u64 {
        self.handle.state.borrow().tabs.get(tab).unwrap().backend_generation()
    }

    pub(super) fn active_turn(&self, tab: TabId) -> Option<String> {
        let state = self.handle.state.borrow();
        let turn = state.tabs.get(tab).unwrap().live()?.projection().active_turn_id.clone();
        turn
    }

    pub(super) fn revision(&self, tab: TabId) -> u64 {
        let state = self.handle.state.borrow();
        let revision = state.tabs.get(tab).unwrap().live().unwrap().projection().last_revision;
        revision
    }

    pub(super) fn start_turn(&self, tab: TabId, provider: &RecordingProvider, turn: &str) {
        provider.queue(agent::AgentDomainEvent::TurnStarted { turn_id: turn.into() });
        tick_handle_until(&self.handle, "the turn", || {
            self.active_turn(tab).as_deref() == Some(turn)
        });
    }

    /// Queues `count` deltas of `text` and turns until they are folded; the revision reached.
    pub(super) fn fold(&self, tab: TabId, provider: &RecordingProvider, count: usize, text: &str) -> u64 {
        let target = self.revision(tab) + count as u64;
        provider.queue_all((0..count).map(|_| delta("t1", text)).collect());
        let deadline = Instant::now() + PATIENCE;
        while self.revision(tab) < target {
            assert!(Instant::now() < deadline, "the batch was never folded");
            self.turn();
        }
        self.revision(tab)
    }

    /// What every connection's writer holds.
    pub(super) fn writer_bytes(&self) -> usize {
        self.socket()
            .conns
            .borrow()
            .values()
            .map(|c| c.gauge.bytes.load(Ordering::SeqCst))
            .sum()
    }

    /// Turns until `client` has received a line `done` accepts; every line received on the way, that one last.
    pub(super) fn until(
        &self,
        client: &mut RvClient,
        what: &str,
        done: impl Fn(&serde_json::Value) -> bool,
    ) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + PATIENCE;
        let mut lines = Vec::new();
        loop {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; got {:?}",
                summary(&lines)
            );
            self.turn();
            while let Some(line) = client
                .recv(Duration::from_millis(5))
                .expect("the connection stays open")
            {
                let value: serde_json::Value = serde_json::from_str(&line).expect("one JSON object per line");
                assert_eq!(value["v"], 1, "every line carries the version");
                let found = done(&value);
                lines.push(value);
                if found {
                    return lines;
                }
            }
        }
    }

    /// Turns until `client`'s connection is closed by the server.
    pub(super) fn until_closed(&self, client: &mut RvClient, what: &str) {
        let deadline = Instant::now() + PATIENCE;
        loop {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            self.turn();
            match client.recv(Duration::from_millis(5)) {
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return,
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => return,
                Err(e) => panic!("{what}: {e}"),
                Ok(_) => {}
            }
        }
    }

    pub(super) fn subscribe(&self, client: &mut RvClient, key: &RowKey) {
        client
            .send(&RvInbound::Subscribe {
                v: 1,
                key: key.to_string(),
            })
            .unwrap();
    }

    pub(super) fn act(&self, client: &mut RvClient, device: &str, token: &str, key: &RowKey, cmd: serde_json::Value) {
        client
            .send(&RvInbound::Act {
                v: 1,
                device: device.into(),
                token: token.into(),
                key: key.to_string(),
                cmd,
            })
            .unwrap();
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if let Some(socket) = self.socket.take() {
            socket.close(&self.handle);
        }
        shut_down_every_tab(&self.handle);
    }
}

/// Each line's kind and revisions, for a failure message.
pub(super) fn summary(lines: &[serde_json::Value]) -> Vec<String> {
    lines
        .iter()
        .map(|l| format!("{} {} {} {}", l["kind"], l["from"], l["through"], l["why"]))
        .collect()
}

pub(super) fn interrupt(command_id: &str, turn: &str, generation: u64) -> serde_json::Value {
    serde_json::json!({
        "type": "interrupt",
        "command_id": command_id,
        "expect_work": { "turn": { "id": turn, "generation": generation } },
    })
}

pub(super) fn delta(turn: &str, text: &str) -> agent::AgentDomainEvent {
    agent::AgentDomainEvent::ContentDelta {
        turn_id: turn.into(),
        kind: agent::ContentKind::Text,
        text: text.into(),
    }
}

/// Checks that `lines` (of one key) start with a snapshot and that each events frame begins where the frame
/// before it ended; a later snapshot starts over. The revision reached.
pub(super) fn contiguous(lines: &[serde_json::Value]) -> u64 {
    let frames: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|l| l["kind"] == "snapshot" || l["kind"] == "events")
        .collect();
    assert_eq!(
        frames.first().map(|f| &f["kind"]),
        Some(&serde_json::json!("snapshot")),
        "a snapshot first"
    );
    let mut at = 0;
    for frame in frames {
        if frame["kind"] == "snapshot" {
            at = frame["through"].as_u64().unwrap();
        } else {
            assert_eq!(frame["from"].as_u64(), Some(at), "no gap and no repeat before {frame}");
            at = frame["through"].as_u64().unwrap();
        }
    }
    at
}

#[test]
fn subscribe_streams_snapshot_then_events() {
    let rig = Rig::new("remote-subscribe");
    let (tab, provider, key) = rig.tab();
    rig.start_turn(tab, &provider, "t1");
    let mut client = rig.connect();
    rig.subscribe(&mut client, &key);
    let mut lines = rig.until(&mut client, "the snapshot", |l| l["kind"] == "snapshot");
    assert_eq!(
        (&lines[0]["kind"], &lines[0]["key"]),
        (&serde_json::json!("greeting"), &serde_json::json!(key.as_str())),
        "the tab's greeting comes first"
    );
    let snapshot = lines.last().unwrap().clone();
    assert_eq!(snapshot["key"], key.as_str());
    assert_eq!(snapshot["frame"]["kind"], "snapshot", "the desk's own envelope");

    provider.queue(delta("t1", "hello"));
    provider.queue(delta("t1", " world"));
    let target = {
        tick_handle_until(&rig.handle, "the deltas", || {
            rig.revision(tab) >= snapshot["through"].as_u64().unwrap() + 2
        });
        rig.revision(tab)
    };
    lines.extend(rig.until(&mut client, "the events", |l| {
        l["kind"] == "events" && l["through"].as_u64() == Some(target)
    }));
    assert!(
        lines.iter().any(|l| l["kind"] == "events"),
        "events follow the snapshot"
    );
    assert_eq!(contiguous(&lines), target);
}

#[test]
fn act_without_token_or_with_watch_grant_is_refused() {
    let rig = Rig::new("remote-act-refused");
    let (tab, provider, key) = rig.tab();
    rig.start_turn(tab, &provider, "t1");
    let mut client = rig.connect();
    for (n, (device, token, why)) in [
        ("d1", "", "not authorized"),
        ("d1", "wrong", "not authorized"),
        ("d9", "tok", "not authorized"),
        ("d2", "wtok", "watch only"),
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("c{n}");
        rig.act(
            &mut client,
            device,
            token,
            &key,
            interrupt(&id, "t1", rig.generation(tab)),
        );
        let lines = rig.until(&mut client, "the result", |l| l["kind"] == "result");
        let result = lines.last().unwrap();
        assert_eq!(result["command_id"], id.as_str());
        assert_eq!(result["ok"], false, "{device} with {token:?}");
        assert_eq!(result["why"], why, "{device} with {token:?}");
    }
    assert_eq!(provider.interrupts(), 0, "nothing stopped");
    assert_eq!(rig.active_turn(tab).as_deref(), Some("t1"), "the turn keeps running");
}

#[test]
fn act_with_steer_grant_interrupts() {
    let rig = Rig::new("remote-act-steer");
    let (tab, provider, key) = rig.tab();
    rig.start_turn(tab, &provider, "t1");
    let mut client = rig.connect();
    // The phone names the session as the frames it was sent name it.
    rig.subscribe(&mut client, &key);
    let lines = rig.until(&mut client, "the snapshot", |l| l["kind"] == "snapshot");
    let generation = lines.last().unwrap()["generation"]
        .as_u64()
        .expect("the snapshot names its session");
    rig.act(&mut client, "d1", "tok", &key, interrupt("c1", "t1", generation));
    let lines = rig.until(&mut client, "the result", |l| l["kind"] == "result");
    let result = lines.last().unwrap();
    assert_eq!(
        result,
        &serde_json::json!({ "v": 1, "kind": "result", "command_id": "c1", "ok": true, "why": null })
    );
    assert_eq!(provider.interrupts(), 1, "the turn was stopped");

    // The list behind the token is still the remote one: a desk-only kind is refused whoever sends it.
    let answer = serde_json::json!({ "type": "permission_response", "command_id": "c2", "permission_id": "p1" });
    rig.act(&mut client, "d1", "tok", &key, answer);
    let lines = rig.until(&mut client, "the result", |l| l["kind"] == "result");
    assert_eq!(lines.last().unwrap()["ok"], false);
    assert_eq!(lines.last().unwrap()["why"], "malformed");
}

#[test]
fn unseen_work_is_stopped_by_the_number_its_frames_carry() {
    let rig = Rig::new("remote-unseen-by-frames");
    let (_tab, provider, key) = rig.tab();
    let mut client = rig.connect();
    rig.subscribe(&mut client, &key);
    rig.until(&mut client, "the snapshot", |l| l["kind"] == "snapshot");
    provider.queue(agent::AgentDomainEvent::CliWorkUnseen);
    let lines = rig.until(&mut client, "a frame naming the stretch", |l| {
        l["kind"] == "events" && l["unseen"].is_u64()
    });
    let stretch = lines.last().unwrap()["unseen"].clone();
    let stop = serde_json::json!({ "type": "interrupt", "command_id": "c1", "expect_work": { "unseen": stretch } });
    rig.act(&mut client, "d1", "tok", &key, stop);
    let lines = rig.until(&mut client, "the result", |l| l["kind"] == "result");
    assert_eq!(lines.last().unwrap()["ok"], true, "{:?}", lines.last());
    assert_eq!(provider.interrupts(), 1, "the unseen work was stopped");
}

#[test]
fn wrong_protocol_version_is_refused() {
    let rig = Rig::new("remote-version");
    let (_tab, _provider, key) = rig.tab();
    let mut client = rig.connect();
    client
        .send(&RvInbound::Subscribe {
            v: 2,
            key: key.to_string(),
        })
        .unwrap();
    rig.until_closed(&mut client, "the close");
    assert!(
        rig.handle.state.borrow().tabs.observers_with_frames().is_empty(),
        "nothing was subscribed"
    );
}

#[test]
fn line_over_cap_closes_the_connection() {
    let rig = Rig::new("remote-line-cap");
    let (tab, provider, key) = rig.tab();
    rig.start_turn(tab, &provider, "t1");
    let mut other = rig.connect();
    let mut raw = UnixStream::connect(&rig.path).unwrap();
    // The server may close before the whole line is written.
    let _ = raw.write_all(&vec![b'x'; RV_MAX_LINE + 2]);
    raw.set_read_timeout(Some(Duration::from_millis(5))).unwrap();
    let deadline = Instant::now() + PATIENCE;
    let mut byte = [0u8; 1];
    loop {
        assert!(Instant::now() < deadline, "the over-long line's connection stays open");
        rig.turn();
        match raw.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => panic!("nothing is answered to an over-long line"),
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => break,
            Err(_) => {}
        }
    }
    rig.subscribe(&mut other, &key);
    rig.until(&mut other, "the other client's snapshot", |l| l["kind"] == "snapshot");
}

#[test]
fn connection_and_observer_caps_hold() {
    let rig = Rig::new("remote-caps");
    let tabs: Vec<RowKey> = (0..=RV_MAX_OBSERVERS_PER_CONNECTION).map(|_| rig.tab().2).collect();
    let mut clients: Vec<RvClient> = (0..RV_MAX_CONNECTIONS).map(|_| rig.connect()).collect();
    // Each has been taken in: a subscription on each is answered.
    for client in clients.iter_mut() {
        rig.subscribe(client, &tabs[0]);
        rig.until(client, "the snapshot", |l| l["kind"] == "snapshot");
    }
    let mut ninth = rig.connect();
    let lines = rig.until(&mut ninth, "the refusal", |l| l["kind"] == "refused");
    assert_eq!(lines.last().unwrap()["why"], "too many connections");
    rig.until_closed(&mut ninth, "the ninth's close");

    let first = &mut clients[0];
    for key in &tabs[1..RV_MAX_OBSERVERS_PER_CONNECTION] {
        rig.subscribe(first, key);
        rig.until(first, "the snapshot", |l| {
            l["kind"] == "snapshot" && l["key"] == key.as_str()
        });
    }
    let over = &tabs[RV_MAX_OBSERVERS_PER_CONNECTION];
    rig.subscribe(first, over);
    let lines = rig.until(first, "the refusal", |l| l["kind"] == "refused");
    let refusal = lines.last().unwrap();
    assert_eq!(
        (refusal["key"].as_str(), refusal["why"].as_str()),
        (Some(over.as_str()), Some("too many subscriptions"))
    );

    // A connection that leaves makes room for another.
    clients.pop();
    let deadline = Instant::now() + PATIENCE;
    loop {
        assert!(
            Instant::now() < deadline,
            "a place freed by a closed connection is never taken"
        );
        rig.turn();
        let mut again = rig.connect();
        // A refused connection may be closed before this is written.
        let _ = again.send(&RvInbound::Subscribe {
            v: 1,
            key: tabs[1].to_string(),
        });
        let mut answered = None;
        let until = Instant::now() + Duration::from_secs(2);
        while answered.is_none() && Instant::now() < until {
            rig.turn();
            match again.recv(Duration::from_millis(5)) {
                Ok(Some(line)) => answered = Some(serde_json::from_str::<serde_json::Value>(&line).unwrap()),
                Ok(None) => {}
                Err(_) => break,
            }
        }
        if answered.is_some_and(|l| l["kind"] == "greeting" || l["kind"] == "snapshot") {
            break;
        }
    }
}

#[test]
fn inbound_line_raises_the_wake() {
    let rig = Rig::new("remote-wake");
    let (_tab, _provider, key) = rig.tab();
    let mut client = rig.connect();
    std::thread::sleep(Duration::from_millis(50));
    rig.wake.take();
    rig.subscribe(&mut client, &key);
    let deadline = Instant::now() + PATIENCE;
    while !rig.wake.take() {
        assert!(Instant::now() < deadline, "the wake was never raised");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn stalled_client_never_blocks_the_tick() {
    const EVENTS: usize = 10_000;
    const BATCH: usize = 100;
    let rig = Rig::new("remote-stalled");
    let (tab, provider, key) = rig.tab();
    rig.start_turn(tab, &provider, "t1");
    let mut client = rig.connect();
    rig.subscribe(&mut client, &key);
    // Taken in, and given its snapshot in the same pump; the client reads none of it.
    let deadline = Instant::now() + PATIENCE;
    while rig.socket().conns.borrow().values().all(|c| c.observers.is_empty()) {
        assert!(Instant::now() < deadline, "the subscription was never taken");
        rig.turn();
    }

    let text = "x".repeat(1024);
    let mut folded = rig.revision(tab);
    for _ in 0..EVENTS / BATCH {
        provider.queue_all((0..BATCH).map(|_| delta("t1", &text)).collect());
        let target = folded + BATCH as u64;
        let deadline = Instant::now() + PATIENCE;
        while rig.revision(tab) < target {
            assert!(Instant::now() < deadline, "the batch was never folded");
            rig.turn();
        }
        folded = rig.revision(tab);
    }
    rig.turn();
    let gauge = |rig: &Rig| {
        rig.socket()
            .conns
            .borrow()
            .values()
            .map(|c| c.gauge.bytes.load(Ordering::SeqCst))
            .sum::<usize>()
    };
    assert!(gauge(&rig) >= WRITER_BYTES, "the writer is full");
    provider.queue(delta("t1", "one more"));
    let deadline = Instant::now() + PATIENCE;
    while rig.revision(tab) <= folded {
        assert!(Instant::now() < deadline, "the last event was never folded");
        rig.turn();
    }
    folded = rig.revision(tab);
    for _ in 0..3 {
        rig.turn();
        assert!(
            !rig.handle.state.borrow().tabs.observers_with_frames().is_empty(),
            "the observer's frames wait in the tab set while the writer is full"
        );
    }

    eprintln!("folded through {folded}");
    let lines = rig.until(&mut client, "the last revision", |l| {
        (l["kind"] == "events" || l["kind"] == "snapshot") && l["through"].as_u64() == Some(folded)
    });
    assert_eq!(contiguous(&lines), folded);
}

#[test]
fn frames_left_by_the_budget_follow_the_wake() {
    let rig = Rig::new("remote-budget-wake");
    let (tab, provider, key) = rig.tab();
    rig.start_turn(tab, &provider, "t1");
    let mut client = rig.connect();
    rig.subscribe(&mut client, &key);
    let deadline = Instant::now() + PATIENCE;
    while rig.socket().conns.borrow().values().all(|c| c.observers.is_empty()) {
        assert!(Instant::now() < deadline, "the subscription was never taken");
        rig.turn();
    }
    // The writer fills, then about twice its room waits in the tab set, under the observer's own cap.
    let text = "x".repeat(1024);
    let mut folded = rig.revision(tab);
    while rig.writer_bytes() < WRITER_BYTES {
        folded = rig.fold(tab, &provider, 100, &text);
    }
    for _ in 0..20 {
        folded = rig.fold(tab, &provider, 100, &text);
    }
    assert!(
        !rig.handle.state.borrow().tabs.observers_with_frames().is_empty(),
        "frames wait in the tab set"
    );

    // From here only the wake drives a pump, as the window's loop does once the backend is quiet.
    rig.wake.take();
    let deadline = Instant::now() + PATIENCE;
    let mut lines: Vec<serde_json::Value> = Vec::new();
    while !lines.iter().any(|l| l["through"].as_u64() == Some(folded)) {
        assert!(
            Instant::now() < deadline,
            "frames stayed behind with nothing to wake the pump; got {:?}",
            summary(&lines)
        );
        if rig.wake.take() {
            rig.turn();
        }
        while let Some(line) = client
            .recv(Duration::from_millis(5))
            .expect("the connection stays open")
        {
            lines.push(serde_json::from_str(&line).expect("one JSON object per line"));
        }
    }
    assert_eq!(contiguous(&lines), folded);
}

#[test]
fn a_half_closed_peer_frees_its_writer() {
    let rig = Rig::new("remote-half-close");
    let (tab, provider, key) = rig.tab();
    rig.start_turn(tab, &provider, "t1");
    // A snapshot of about 0.9 MB (whole, under the snapshot ceiling), far more than the socket holds unread.
    let text = "x".repeat(1024);
    for _ in 0..9 {
        rig.fold(tab, &provider, 100, &text);
    }
    let mut raw = UnixStream::connect(&rig.path).unwrap();
    let subscribe = RvInbound::Subscribe {
        v: 1,
        key: key.to_string(),
    };
    raw.write_all(format!("{}\n", serde_json::to_string(&subscribe).unwrap()).as_bytes())
        .unwrap();
    let deadline = Instant::now() + PATIENCE;
    let gauge = loop {
        assert!(Instant::now() < deadline, "the snapshot was never queued");
        rig.turn();
        let queued = rig
            .socket()
            .conns
            .borrow()
            .values()
            .find(|c| !c.observers.is_empty() && c.gauge.bytes.load(Ordering::SeqCst) > 0)
            .map(|c| Arc::clone(&c.gauge));
        if let Some(gauge) = queued {
            break gauge;
        }
    };
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        gauge.bytes.load(Ordering::SeqCst) > 0,
        "the writer is held in its write"
    );

    // The peer stops sending but keeps its receiving side open and never reads.
    raw.shutdown(Shutdown::Write).unwrap();
    let deadline = Instant::now() + PATIENCE;
    while !rig.socket().conns.borrow().is_empty() || Arc::strong_count(&gauge) > 1 {
        assert!(
            Instant::now() < deadline,
            "the writer of a peer that stopped sending is still held"
        );
        rig.turn();
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(raw);
}

#[test]
fn devices_sharing_a_name_keep_their_own_command_ids() {
    let rig = Rig::new("remote-same-name");
    let (tab, provider, key) = rig.tab();
    rig.start_turn(tab, &provider, "t1");
    let mut client = rig.connect();
    rig.act(
        &mut client,
        "d1",
        "tok",
        &key,
        interrupt("c1", "t1", rig.generation(tab)),
    );
    let lines = rig.until(&mut client, "the first result", |l| l["kind"] == "result");
    assert_eq!(lines.last().unwrap()["ok"], true);

    rig.start_turn(tab, &provider, "t2");
    rig.act(
        &mut client,
        "d3",
        "tok3",
        &key,
        interrupt("c1", "t2", rig.generation(tab)),
    );
    let lines = rig.until(&mut client, "the second result", |l| l["kind"] == "result");
    let result = lines.last().unwrap();
    assert_eq!(
        (&result["ok"], &result["why"]),
        (&serde_json::json!(true), &serde_json::Value::Null),
        "another device's command ids are its own, whatever its name"
    );
    assert_eq!(provider.interrupts(), 2);
}

impl Rig {
    /// Why each connection the socket forgot was closed, in order.
    pub(super) fn closes(&self) -> Vec<CloseWhy> {
        self.socket().closes.borrow().clone()
    }
}

/// Deadlines short enough for a test to see them pass; the one not under test stays long.
fn deadlines(idle: Duration, write_stall: Duration) -> Deadlines {
    Deadlines { idle, write_stall }
}

#[test]
fn oversized_command_id_is_neither_kept_nor_echoed() {
    let rig = Rig::new("remote-long-id");
    let (tab, provider, key) = rig.tab();
    rig.start_turn(tab, &provider, "t1");
    let mut client = rig.connect();
    let long = "c".repeat(COMMAND_ID_MAX_BYTES + 1);
    rig.act(
        &mut client,
        "d1",
        "tok",
        &key,
        interrupt(&long, "t1", rig.generation(tab)),
    );
    let lines = rig.until(&mut client, "the result", |l| l["kind"] == "result");
    let result = lines.last().unwrap();
    assert!(result["command_id"].is_null(), "the id is not copied back: {result}");
    assert_eq!(
        (&result["ok"], &result["why"]),
        (&serde_json::json!(false), &serde_json::json!("malformed"))
    );
    assert_eq!(provider.interrupts(), 0, "nothing stopped");
    assert!(
        crate::agent_panel::remote_entry::first_command_use("d1", &long),
        "the id was never kept"
    );
}

#[test]
fn control_lines_count_against_the_byte_gauge() {
    let gauge = Gauge::default();
    assert!(
        gauge.control_fits(CONTROL_BYTES_MAX),
        "an empty gauge takes a full answer"
    );
    assert!(!gauge.control_fits(CONTROL_BYTES_MAX + 1));
    gauge.control_bytes.store(CONTROL_BYTES_MAX - 10, Ordering::SeqCst);
    assert!(gauge.control_fits(10));
    assert!(!gauge.control_fits(11));
    gauge.control_bytes.store(0, Ordering::SeqCst);
    gauge.control.store(CONTROL_LINES_MAX, Ordering::SeqCst);
    assert!(!gauge.control_fits(1), "the count still holds");

    // A connection whose writer drains nothing: answers stand in its gauge until the next would pass the cap.
    let rig = Rig::new("remote-control-bytes");
    let (ours, mut theirs) = UnixStream::pair().unwrap();
    let (out, lines) = mpsc::channel();
    let gauge = Arc::new(Gauge::default());
    rig.socket().conns.borrow_mut().insert(
        9_999,
        Conn {
            stream: ours,
            out,
            gauge: Arc::clone(&gauge),
            observers: BTreeMap::new(),
        },
    );
    let big = "x".repeat(40_000);
    rig.socket().answer(9_999, big.clone());
    assert_eq!(
        gauge.bytes.load(Ordering::SeqCst),
        big.len() + 1,
        "the answer counts in the writer's bytes"
    );
    assert_eq!(gauge.control_bytes.load(Ordering::SeqCst), big.len() + 1);
    rig.socket().answer(9_999, big);
    assert_eq!(lines.try_iter().count(), 1, "the second answer was not queued");
    theirs.set_read_timeout(Some(PATIENCE)).unwrap();
    let mut byte = [0u8; 1];
    assert_eq!(theirs.read(&mut byte).unwrap(), 0, "the connection was closed");
}

#[test]
fn idle_peer_is_closed_after_the_deadline() {
    let idle = Duration::from_millis(300);
    let rig = Rig::with_deadlines("remote-idle", deadlines(idle, RV_WRITE_STALL));
    let opened = Instant::now();
    let mut client = rig.connect();
    rig.until_closed(&mut client, "the idle close");
    assert!(
        opened.elapsed() >= idle,
        "not before the deadline: {:?}",
        opened.elapsed()
    );
    let deadline = Instant::now() + PATIENCE;
    while rig.closes().is_empty() {
        assert!(Instant::now() < deadline, "the close never reached the tick");
        rig.turn();
    }
    assert_eq!(rig.closes(), [CloseWhy::Idle]);
}

#[test]
fn ping_keeps_an_idle_connection() {
    let idle = Duration::from_millis(400);
    let rig = Rig::with_deadlines("remote-ping", deadlines(idle, RV_WRITE_STALL));
    let mut client = rig.connect();
    let until = Instant::now() + idle * 4;
    let mut pongs = 0;
    while Instant::now() < until {
        client
            .send(&RvInbound::Ping { v: 1 })
            .expect("the connection stays open");
        let next = Instant::now() + Duration::from_millis(100);
        while Instant::now() < next {
            rig.turn();
            if let Some(line) = client
                .recv(Duration::from_millis(5))
                .expect("the connection stays open")
            {
                let line: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(line, serde_json::json!({ "v": 1, "kind": "pong" }));
                pongs += 1;
            }
        }
    }
    assert!(pongs >= 4, "each ping is answered: {pongs}");
    assert!(rig.closes().is_empty(), "nothing was closed: {:?}", rig.closes());
    // Once the pings stop, the deadline closes it.
    rig.until_closed(&mut client, "the idle close");
}

#[test]
fn stalled_writer_is_closed_after_the_deadline() {
    let rig = Rig::with_deadlines(
        "remote-write-stall",
        deadlines(Duration::from_secs(3600), Duration::from_millis(300)),
    );
    let (tab, provider, key) = rig.tab();
    rig.start_turn(tab, &provider, "t1");
    // A snapshot of about 0.9 MB (whole, under the snapshot ceiling), far more than the socket holds unread.
    let text = "x".repeat(1024);
    for _ in 0..9 {
        rig.fold(tab, &provider, 100, &text);
    }
    let mut raw = UnixStream::connect(&rig.path).unwrap();
    let subscribe = RvInbound::Subscribe {
        v: 1,
        key: key.to_string(),
    };
    raw.write_all(format!("{}\n", serde_json::to_string(&subscribe).unwrap()).as_bytes())
        .unwrap();
    // The peer keeps both directions open and never reads.
    let deadline = Instant::now() + PATIENCE;
    while !rig.closes().contains(&CloseWhy::WriteStall) {
        assert!(
            Instant::now() < deadline,
            "a writer that moves nothing is never closed: {:?}",
            rig.closes()
        );
        rig.turn();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(rig.socket().conns.borrow().is_empty(), "the connection is forgotten");
    assert_eq!(
        rig.handle.state.borrow().tabs.observer_count(),
        0,
        "and its observer with it"
    );
    drop(raw);
}

#[test]
fn a_write_cut_short_by_the_deadline_is_the_stall() {
    let stall = Duration::from_millis(1500);
    let rig = Rig::with_deadlines("remote-write-short", deadlines(Duration::from_secs(3600), stall));
    let (tab, provider, key) = rig.tab();
    rig.start_turn(tab, &provider, "t1");
    // A snapshot of about 0.9 MB: the first write moves what the socket holds unread at once, then waits.
    let text = "x".repeat(1024);
    for _ in 0..9 {
        rig.fold(tab, &provider, 100, &text);
    }
    let mut raw = UnixStream::connect(&rig.path).unwrap();
    let subscribe = RvInbound::Subscribe {
        v: 1,
        key: key.to_string(),
    };
    let asked = Instant::now();
    raw.write_all(format!("{}\n", serde_json::to_string(&subscribe).unwrap()).as_bytes())
        .unwrap();
    // The peer never reads, so no byte moves after the first write's: one deadline from then closes it, not two.
    let deadline = Instant::now() + PATIENCE;
    while !rig.closes().contains(&CloseWhy::WriteStall) {
        assert!(Instant::now() < deadline, "never closed: {:?}", rig.closes());
        rig.turn();
        std::thread::sleep(Duration::from_millis(5));
    }
    let took = asked.elapsed();
    assert!(took >= stall - stall / 10, "not before the deadline: {took:?}");
    assert!(
        took < stall + stall / 2,
        "closed one deadline after the last byte moved, not after a second whole deadline: {took:?}"
    );
    drop(raw);
}

#[test]
fn dropping_without_close_forgets_observers() {
    let mut rig = Rig::new("remote-drop-reaps");
    let (_tab, _provider, key) = rig.tab();
    let mut client = rig.connect();
    rig.subscribe(&mut client, &key);
    rig.until(&mut client, "the snapshot", |l| l["kind"] == "snapshot");
    assert_eq!(rig.handle.state.borrow().tabs.observer_count(), 1);
    drop(rig.socket.take());
    rig.handle.tick();
    assert_eq!(
        rig.handle.state.borrow().tabs.observer_count(),
        0,
        "the next tick forgets the dropped socket's observers"
    );
}

#[test]
fn every_new_kind_parses_and_is_refused_not_yet() {
    let rig = Rig::new("remote-not-yet-socket");
    let (tab, provider, key) = rig.tab();
    rig.start_turn(tab, &provider, "t1");
    let mut client = rig.connect();
    let errands = [(
        RvInbound::More {
            v: 1,
            key: key.to_string(),
            generation: rig.generation(tab),
            before_turn: 1,
        },
        serde_json::json!(key.as_str()),
    )];
    for (line, key) in errands {
        client.send(&line).unwrap();
        let lines = rig.until(&mut client, "the refusal", |l| l["kind"] == "refused");
        let refusal = lines.last().unwrap();
        // `more` is carried out: about a tab this connection does not follow, it is refused as such.
        let why = if matches!(line, RvInbound::More { .. }) {
            "not subscribed"
        } else {
            "not yet"
        };
        assert_eq!(
            (&refusal["why"], &refusal["key"]),
            (&serde_json::json!(why), &key),
            "{line:?}"
        );
    }
    assert_eq!(provider.interrupts(), 0, "nothing reached the agent");

    // A field none of them has closes the connection, as any line that does not parse does.
    let mut line = serde_json::to_value(RvInbound::StartEnroll { v: 1, recipe: None }).unwrap();
    line["x"] = serde_json::json!(1);
    let mut stream = UnixStream::connect(&rig.path).unwrap();
    stream.write_all(format!("{line}\n").as_bytes()).unwrap();
    stream.set_read_timeout(Some(Duration::from_millis(5))).unwrap();
    let deadline = Instant::now() + PATIENCE;
    let mut byte = [0u8; 1];
    loop {
        assert!(
            Instant::now() < deadline,
            "a line with an unknown field left the connection open"
        );
        rig.turn();
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => panic!("nothing is answered to a line that does not parse"),
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => break,
            Err(_) => {}
        }
    }
}
