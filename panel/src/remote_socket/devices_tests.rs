//! Enrollment and the listen address through the `rv-` socket, end to end within a window: the reader's checks, the
//! base page's question, the desk's answer and what reaches the store and the peer.

use super::*;
use crate::agent_panel::tests::*;
use eitri_core::remote_devices::{update, DeviceStore};
use eitri_core::remote_pairing::confirmed_listen;
use eitri_core::remote_rv::{rv_socket_path, RvClient};
use std::os::unix::fs::DirBuilderExt;
use std::sync::atomic::AtomicU32;

const PATIENCE: Duration = Duration::from_secs(30);

/// A private directory standing in for the control and state directories, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Scratch {
        static SERIAL: AtomicU32 = AtomicU32::new(0);
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(std::env::temp_dir);
        let path = base.join(format!(
            "re-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Scratch(path)
    }

    /// The device store, in a directory of its own that does not exist yet.
    fn store(&self) -> PathBuf {
        self.0.join("remote").join("devices.json")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One window: its panel, its base page and its `rv-` socket.
struct Window {
    handle: AgentPanelHandle,
    base: Rc<RecordingPage>,
    second: Rc<RecordingPage>,
    socket: RemoteSocket,
    path: PathBuf,
    _wake: PanelWake,
}

impl Window {
    fn new(scratch: &Scratch, instance: &str) -> Window {
        agent::state_dirs::redirect_state_to_a_test_root();
        let (handle, base, second) = handle_with_two_pages();
        let wake = PanelWake::new().unwrap();
        let socket = RemoteSocket::bind(&scratch.0, instance, &scratch.store(), &wake).expect("the socket binds");
        socket.set_pairing(Pairing {
            endpoint: Endpoint {
                mode: PairMode::Direct,
                host: "127.0.0.1".into(),
                port: 7341,
            },
            ssh_dir: scratch.0.clone(),
        });
        let path = rv_socket_path(&scratch.0, instance).unwrap();
        Window {
            handle,
            base,
            second,
            socket,
            path,
            _wake: wake,
        }
    }

    fn connect(&self) -> RvClient {
        RvClient::connect(&self.path).expect("the client connects")
    }

    fn turn(&self) {
        self.handle.tick();
        self.socket.pump(&self.handle);
    }

    /// Turns until `client` receives a line `done` accepts, and returns it.
    fn until(&self, client: &mut RvClient, what: &str, done: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let deadline = Instant::now() + PATIENCE;
        let mut seen = Vec::new();
        loop {
            assert!(Instant::now() < deadline, "timed out waiting for {what}; got {seen:?}");
            self.turn();
            while let Some(line) = client
                .recv(Duration::from_millis(5))
                .expect("the connection stays open")
            {
                let value: serde_json::Value = serde_json::from_str(&line).expect("one JSON object per line");
                if done(&value) {
                    return value;
                }
                seen.push(value);
            }
        }
    }

    /// Turns until the base page was sent a `kind` envelope after `after` of them, and returns it.
    fn asked(&self, kind: &str, after: usize) -> serde_json::Value {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let found = self
                .base
                .sent
                .borrow()
                .iter()
                .map(|e| json_of(e))
                .filter(|v| v["kind"] == kind)
                .nth(after);
            if let Some(found) = found {
                return found;
            }
            assert!(Instant::now() < deadline, "the base page was never sent {kind}");
            self.turn();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn count(&self, kind: &str) -> usize {
        self.base
            .sent
            .borrow()
            .iter()
            .filter(|e| json_of(e)["kind"] == kind)
            .count()
    }

    /// `start_enroll` through `client`; the code the base page shows.
    fn start(&self, client: &mut RvClient) -> String {
        let before = self.count("remote_pairing");
        client.send(&RvInbound::StartEnroll { v: RV_PROTOCOL }).unwrap();
        let answer = self.until(client, "start_enroll's result", |v| {
            v["kind"] == "result" || v["kind"] == "refused"
        });
        assert_eq!(answer["ok"], true, "{answer}");
        let shown = self.asked("remote_pairing", before);
        shown["code"].as_str().unwrap().to_owned()
    }

    fn enroll(client: &mut RvClient, code: &str, name: &str) {
        client
            .send(&RvInbound::Enroll {
                v: RV_PROTOCOL,
                code: Secret::new(code),
                device_name: name.into(),
                host: "127.0.0.1:7341".into(),
                peer: "127.0.0.1:50122".into(),
            })
            .unwrap();
    }

    fn answer(&self, request: &str, nonce: u64, yes: bool, grant: &str) {
        self.handle.page_message(
            ViewId::BASE,
            &format!(
                r#"{{"type":"remote_enroll_answer","request_id":"{request}","nonce":{nonce},"yes":{yes},"grant":"{grant}"}}"#
            ),
        );
    }

    fn listen_answer(&self, request: &str, nonce: u64, yes: bool) {
        self.handle.page_message(
            ViewId::BASE,
            &format!(r#"{{"type":"remote_listen_answer","request_id":"{request}","nonce":{nonce},"yes":{yes}}}"#),
        );
    }

    /// Enrolls a phone named `name` and says yes with `grant`; the device id and token it is given.
    fn enroll_with_yes(&self, client: &mut RvClient, name: &str, grant: &str) -> (String, String) {
        let code = self.start(client);
        let before = self.count("remote_enroll");
        Window::enroll(client, &code, name);
        let asked = self.asked("remote_enroll", before);
        assert_eq!(asked["name"], name);
        self.answer("yes", asked["nonce"].as_u64().unwrap(), true, grant);
        let enrolled = self.until(client, "enrolled", |v| {
            v["kind"] == "enrolled" || v["kind"] == "enroll_refused"
        });
        assert_eq!(enrolled["kind"], "enrolled", "{enrolled}");
        (
            enrolled["device"].as_str().unwrap().to_owned(),
            enrolled["token"].as_str().unwrap().to_owned(),
        )
    }
}

impl Window {
    /// Turns until `done` holds.
    fn turn_until(&self, what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + PATIENCE;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            self.turn();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Turns a while, for anything on its way to arrive.
    fn settle(&self) {
        for _ in 0..25 {
            self.turn();
            std::thread::sleep(Duration::from_millis(4));
        }
    }

    /// The next `result` `client` receives.
    fn result(&self, client: &mut RvClient, what: &str) -> serde_json::Value {
        self.until(client, what, |v| v["kind"] == "result")
    }

    /// The base page's `remote_device_flagged` envelopes naming `device`.
    fn flagged(&self, device: &str) -> usize {
        self.base
            .sent
            .borrow()
            .iter()
            .map(|e| json_of(e))
            .filter(|v| v["kind"] == "remote_device_flagged" && v["device"] == device)
            .count()
    }
}

fn grant_request(client: &mut RvClient, device: &str, grant: Grant) {
    client
        .send(&RvInbound::GrantRequest {
            v: RV_PROTOCOL,
            device: device.into(),
            grant,
        })
        .unwrap();
}

fn revoke_self(client: &mut RvClient, device: &str, token: &str) {
    client
        .send(&RvInbound::RevokeSelf {
            v: RV_PROTOCOL,
            device: device.into(),
            token: Secret::new(token),
        })
        .unwrap();
}

fn set_push(client: &mut RvClient, device: &str, token: &str, url: &str) {
    client
        .send(&RvInbound::SetPushEndpoint {
            v: RV_PROTOCOL,
            device: device.into(),
            token: Secret::new(token),
            url: Secret::new(url),
        })
        .unwrap();
}

/// `serve`'s report that the devices in `connected` (id, `Host`) are connected now.
fn report(client: &mut RvClient, connected: &[(&str, &str)]) {
    client
        .send(&RvInbound::DevicesConnected {
            v: RV_PROTOCOL,
            devices: connected
                .iter()
                .map(|(device, host)| Connected {
                    device: (*device).into(),
                    host: (*host).into(),
                    peer: "127.0.0.1:50122".into(),
                    since: 1,
                })
                .collect(),
        })
        .unwrap();
}

fn stored(store: &Path, device: &str) -> Option<Device> {
    devices_in(store).into_iter().find(|d| d.id == device)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn devices_in(store: &Path) -> Vec<Device> {
    match DeviceStore::load(store) {
        Ok(store) => store.devices().to_vec(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => panic!("the store reads: {e}"),
    }
}

const INSTANCE_A: &str = "a1b2c3d4e5f60718";
const INSTANCE_B: &str = "0f1e2d3c4b5a6978";

#[test]
fn no_token_without_a_desk_yes() {
    let scratch = Scratch::new();
    let window = Window::new(&scratch, INSTANCE_A);
    let mut client = window.connect();
    let covered = Rc::new(Cell::new(false));
    let _gate = recording_gate(&window.handle, &[1], covered.clone());

    // `n`, and Escape, which the page sends as the same `no`: refused, nothing stored.
    for request in ["n", "escape"] {
        let code = window.start(&mut client);
        let before = window.count("remote_enroll");
        Window::enroll(&mut client, &code, "Pixel");
        let asked = window.asked("remote_enroll", before);
        window.answer(request, asked["nonce"].as_u64().unwrap(), false, "watch");
        let told = window.until(&mut client, "the refusal", |v| v["kind"] == "enroll_refused");
        assert_eq!(told["why"], "the desk said no");
        assert!(devices_in(&scratch.store()).is_empty());
    }

    // A `y` from a covered chat is refused by the window: nothing stored, the question still waits.
    let code = window.start(&mut client);
    let before = window.count("remote_enroll");
    Window::enroll(&mut client, &code, "Pixel");
    let asked = window.asked("remote_enroll", before);
    let nonce = asked["nonce"].as_u64().unwrap();
    covered.set(true);
    window.answer("covered", nonce, true, "watch");
    assert!(matches!(
        result_for(&window.base.sent.borrow(), "covered"),
        Some(Err(_))
    ));
    for _ in 0..20 {
        window.turn();
    }
    assert!(
        client.recv(Duration::from_millis(20)).unwrap().is_none(),
        "nothing reached the phone"
    );
    assert!(devices_in(&scratch.store()).is_empty());
    assert_eq!(window.handle.state.borrow().remote_desk.enroll_nonce(), Some(nonce));

    // A code presented twice asks once; a wrong one never asks.
    let before = window.count("remote_enroll");
    Window::enroll(&mut client, &code, "Pixel");
    let told = window.until(&mut client, "the second presentation", |v| {
        v["kind"] == "enroll_refused"
    });
    assert_eq!(
        told["why"],
        "that enrollment code is not this desk's, or was already used"
    );
    Window::enroll(&mut client, "AAAAAAAAAAAAAAAAAAAAAA", "Pixel");
    window.until(&mut client, "the wrong code", |v| v["kind"] == "enroll_refused");
    assert_eq!(window.count("remote_enroll"), before);

    // Uncovered, the same question takes the desk's yes: only now is a token minted.
    covered.set(false);
    window.answer("uncovered", nonce, true, "watch");
    let enrolled = window.until(&mut client, "enrolled", |v| v["kind"] == "enrolled");
    assert!(!enrolled["token"].as_str().unwrap().is_empty());
    assert_eq!(devices_in(&scratch.store()).len(), 1);
}

#[test]
fn token_is_stored_as_its_hash_only() {
    let scratch = Scratch::new();
    let window = Window::new(&scratch, INSTANCE_A);
    let mut client = window.connect();
    let (device, token) = window.enroll_with_yes(&mut client, "Pixel \u{202e}9", "watch+steer");
    assert_eq!(token.len(), 43, "256 bits of unpadded base64url");

    let text = std::fs::read_to_string(scratch.store()).unwrap();
    assert!(!text.contains(&token), "the token itself is never written");
    let store = DeviceStore::load(&scratch.store()).unwrap();
    assert_eq!(
        store.verify(&device, &token),
        Some(("Pixel \u{202e}9".to_owned(), Grant::WatchSteer))
    );
    let stored = store.get(&device).unwrap();
    assert_eq!(
        stored.token_sha256,
        <[u8; 32]>::from(sha2::Sha256::digest(token.as_bytes()))
    );
    assert_eq!(stored.recipe, Recipe::Direct);
    assert_eq!(stored.enrolled_host, "127.0.0.1:7341");
    assert!(stored.confirmed.is_some(), "the desk's yes is recorded with the device");
    assert!(stored.push_endpoint.is_none() && stored.last_seen.is_none());
    use std::os::unix::fs::MetadataExt;
    assert_eq!(std::fs::metadata(scratch.store()).unwrap().mode() & 0o777, 0o600);
}

#[test]
fn a_phone_using_ssh_is_enrolled_to_watch() {
    let scratch = Scratch::new();
    let window = Window::new(&scratch, INSTANCE_A);
    window.socket.set_pairing(Pairing {
        endpoint: Endpoint {
            mode: PairMode::Ssh,
            host: "desk.example.net".into(),
            port: 7341,
        },
        ssh_dir: scratch.0.clone(),
    });
    let mut client = window.connect();
    let code = window.start(&mut client);
    let shown = window.asked("remote_pairing", 0);
    assert!(shown["uri"].as_str().unwrap().contains("&m=ssh&"), "{shown}");
    let before = window.count("remote_enroll");
    Window::enroll(&mut client, &code, "Pixel");
    let asked = window.asked("remote_enroll", before);
    assert_eq!(asked["grants"], serde_json::json!(["watch"]));
    window.answer("steer", asked["nonce"].as_u64().unwrap(), true, "watch+steer");
    assert!(matches!(result_for(&window.base.sent.borrow(), "steer"), Some(Err(_))));
    window.answer("watch", asked["nonce"].as_u64().unwrap(), true, "watch");
    let enrolled = window.until(&mut client, "enrolled", |v| v["kind"] == "enrolled");
    let store = DeviceStore::load(&scratch.store()).unwrap();
    let stored = store.get(enrolled["device"].as_str().unwrap()).unwrap();
    assert_eq!((stored.grant, stored.recipe), (Grant::Watch, Recipe::Ssh));
}

#[test]
fn enroll_routes_to_the_issuing_instance() {
    let scratch = Scratch::new();
    let a = Window::new(&scratch, INSTANCE_A);
    let b = Window::new(&scratch, INSTANCE_B);
    let mut to_a = a.connect();
    let mut to_b = b.connect();
    let code = a.start(&mut to_a);
    let shown = a.asked("remote_pairing", 0);
    assert!(
        shown["uri"].as_str().unwrap().contains(&format!("&i={INSTANCE_A}&")),
        "the pairing URI names the window that made the code: {shown}"
    );

    // Another window has no such code, and asks nothing.
    Window::enroll(&mut to_b, &code, "Pixel");
    let told = b.until(&mut to_b, "b's refusal", |v| v["kind"] == "enroll_refused");
    assert_eq!(
        told["why"],
        "that enrollment code is not this desk's, or was already used"
    );
    assert_eq!(b.count("remote_enroll"), 0);

    // The window that made it asks its own desk.
    Window::enroll(&mut to_a, &code, "Pixel");
    let asked = a.asked("remote_enroll", 0);
    assert_eq!(asked["peer"], "127.0.0.1:50122");
    assert_eq!(b.count("remote_enroll"), 0);
}

#[test]
fn start_enroll_without_an_address_for_phones_is_refused() {
    let scratch = Scratch::new();
    agent::state_dirs::redirect_state_to_a_test_root();
    let (handle, base, _second) = handle_with_two_pages();
    let wake = PanelWake::new().unwrap();
    let socket = RemoteSocket::bind(&scratch.0, INSTANCE_A, &scratch.store(), &wake).unwrap();
    let mut client = RvClient::connect(&rv_socket_path(&scratch.0, INSTANCE_A).unwrap()).unwrap();
    client.send(&RvInbound::StartEnroll { v: RV_PROTOCOL }).unwrap();
    let deadline = Instant::now() + PATIENCE;
    let line = loop {
        assert!(Instant::now() < deadline);
        handle.tick();
        socket.pump(&handle);
        if let Some(line) = client.recv(Duration::from_millis(5)).unwrap() {
            break serde_json::from_str::<serde_json::Value>(&line).unwrap();
        }
    };
    assert_eq!(line["kind"], "refused");
    assert_eq!(line["why"], NO_ENDPOINT);
    assert!(!base
        .sent
        .borrow()
        .iter()
        .any(|e| json_of(e)["kind"] == "remote_pairing"));
}

#[test]
fn named_address_needs_a_desk_yes() {
    let scratch = Scratch::new();
    let window = Window::new(&scratch, INSTANCE_A);
    window
        .socket
        .set_link_of(|ip| (ip.to_string() == "198.51.100.20").then(|| ("eth0".to_owned(), IfaceKind::Plain)));
    let mut client = window.connect();
    let request = |client: &mut RvClient, addr: &str| {
        client
            .send(&RvInbound::ListenRequest {
                v: RV_PROTOCOL,
                addr: addr.into(),
            })
            .unwrap();
    };

    // A wildcard, loopback, or an address no link holds is no, without asking.
    for addr in ["0.0.0.0", "::", "127.0.0.1", "203.0.113.9"] {
        request(&mut client, addr);
        let told = window.until(&mut client, "the answer", |v| v["kind"] == "listen_answer");
        assert_eq!(
            (told["addr"].as_str(), told["yes"].as_bool()),
            (Some(addr), Some(false))
        );
    }
    assert_eq!(window.count("remote_listen"), 0);

    // A held address asks the desk; `n` is no and records nothing.
    request(&mut client, "198.51.100.20");
    let asked = window.asked("remote_listen", 0);
    assert_eq!(
        (asked["iface"].as_str(), asked["plain"].as_bool()),
        (Some("eth0"), Some(true))
    );
    window.listen_answer("n", asked["nonce"].as_u64().unwrap(), false);
    let told = window.until(&mut client, "the no", |v| v["kind"] == "listen_answer");
    assert_eq!(told["yes"], false);
    let state_dir = scratch.store().parent().unwrap().to_path_buf();
    assert_eq!(confirmed_listen(&state_dir).unwrap(), None);

    // Asked again, `y` is recorded before the yes goes out.
    request(&mut client, "198.51.100.20");
    let asked = window.asked("remote_listen", 1);
    window.listen_answer("y", asked["nonce"].as_u64().unwrap(), true);
    let told = window.until(&mut client, "the yes", |v| v["kind"] == "listen_answer");
    assert_eq!(told["yes"], true);
    assert_eq!(confirmed_listen(&state_dir).unwrap().as_deref(), Some("198.51.100.20"));
}

#[test]
fn two_windows_see_one_confirmation() {
    let scratch = Scratch::new();
    let a = Window::new(&scratch, INSTANCE_A);
    let b = Window::new(&scratch, INSTANCE_B);
    let plain = |ip: IpAddr| (ip.to_string() == "198.51.100.20").then(|| ("eth0".to_owned(), IfaceKind::Plain));
    a.socket.set_link_of(plain);
    b.socket.set_link_of(plain);
    let mut to_a = a.connect();
    let mut to_b = b.connect();

    // The desk's yes in one window is the yes the other reads: a device, confirmed in the store.
    let (device, token) = a.enroll_with_yes(&mut to_a, "Pixel", "watch");
    let seen_by_b = DeviceStore::load(&scratch.store()).unwrap();
    assert!(seen_by_b.get(&device).unwrap().confirmed.is_some());
    assert!(seen_by_b.verify(&device, &token).is_some());

    // And a listen address: yes in A, then B is answered yes without asking its desk.
    to_a.send(&RvInbound::ListenRequest {
        v: RV_PROTOCOL,
        addr: "198.51.100.20".into(),
    })
    .unwrap();
    let asked = a.asked("remote_listen", 0);
    a.listen_answer("y", asked["nonce"].as_u64().unwrap(), true);
    assert_eq!(
        a.until(&mut to_a, "a's yes", |v| v["kind"] == "listen_answer")["yes"],
        true
    );
    to_b.send(&RvInbound::ListenRequest {
        v: RV_PROTOCOL,
        addr: "198.51.100.20".into(),
    })
    .unwrap();
    assert_eq!(
        b.until(&mut to_b, "b's yes", |v| v["kind"] == "listen_answer")["yes"],
        true
    );
    assert_eq!(b.count("remote_listen"), 0, "one confirmation, not one per window");

    // The same address on a link of another kind is a new question.
    b.socket
        .set_link_of(|ip| (ip.to_string() == "198.51.100.20").then(|| ("wg0".to_owned(), IfaceKind::WireGuard)));
    to_b.send(&RvInbound::ListenRequest {
        v: RV_PROTOCOL,
        addr: "198.51.100.20".into(),
    })
    .unwrap();
    b.asked("remote_listen", 0);
}

#[test]
fn grant_change_needs_a_desk_yes() {
    let scratch = Scratch::new();
    let window = Window::new(&scratch, INSTANCE_A);
    let mut client = window.connect();
    let covered = Rc::new(Cell::new(false));
    let _gate = recording_gate(&window.handle, &[1], covered.clone());
    let (device, _token) = window.enroll_with_yes(&mut client, "Pixel", "watch");
    let grant_of = || stored(&scratch.store(), &device).unwrap().grant;

    // The grant it has: yes at once, the desk not asked.
    grant_request(&mut client, &device, Grant::Watch);
    assert_eq!(window.result(&mut client, "the same grant")["ok"], true);
    // A device the store does not hold: refused, the desk not asked.
    grant_request(&mut client, "d0000000000000000", Grant::WatchSteer);
    let told = window.result(&mut client, "an unknown device");
    assert_eq!(
        (told["ok"].as_bool(), told["why"].as_str()),
        (Some(false), Some(NO_SUCH_DEVICE))
    );
    assert_eq!(window.count("remote_grant"), 0);

    // The desk is asked, naming the device and both grants; `n` changes nothing.
    grant_request(&mut client, &device, Grant::WatchSteer);
    let asked = window.asked("remote_grant", 0);
    assert_eq!(
        (&asked["device"], &asked["name"], &asked["from"], &asked["to"]),
        (
            &serde_json::json!(device),
            &serde_json::json!("Pixel"),
            &serde_json::json!("watch"),
            &serde_json::json!("watch+steer")
        )
    );
    window.answer("n", asked["nonce"].as_u64().unwrap(), false, "watch+steer");
    let told = window.result(&mut client, "the no");
    assert_eq!(
        (told["ok"].as_bool(), told["why"].as_str()),
        (Some(false), Some("the desk said no"))
    );
    assert_eq!(grant_of(), Grant::Watch);

    // A `y` from a covered chat is refused by the window, and a `y` naming another grant by the question.
    grant_request(&mut client, &device, Grant::WatchSteer);
    let nonce = window.asked("remote_grant", 1)["nonce"].as_u64().unwrap();
    covered.set(true);
    window.answer("covered", nonce, true, "watch+steer");
    assert!(matches!(
        result_for(&window.base.sent.borrow(), "covered"),
        Some(Err(_))
    ));
    covered.set(false);
    window.answer("other", nonce, true, "watch");
    assert!(matches!(result_for(&window.base.sent.borrow(), "other"), Some(Err(_))));
    window.settle();
    assert!(
        client.recv(Duration::from_millis(20)).unwrap().is_none(),
        "nothing decided yet"
    );
    assert_eq!(grant_of(), Grant::Watch);
    assert_eq!(window.handle.state.borrow().remote_desk.grant_nonce(), Some(nonce));

    // The desk's `y`: only now does the grant change.
    window.answer("y", nonce, true, "watch+steer");
    assert_eq!(window.result(&mut client, "the yes")["ok"], true);
    assert_eq!(grant_of(), Grant::WatchSteer);

    // A phone of the SSH recipe is not offered more than watching, and the desk is not asked.
    let ssh = Device {
        id: "dssh".into(),
        name: "Termux".into(),
        token_sha256: [1; 32],
        grant: Grant::Watch,
        recipe: Recipe::Ssh,
        confirmed: Some(Confirmed { at: 1 }),
        push_endpoint: None,
        last_seen: None,
        last_host: None,
        enrolled_host: "127.0.0.1:43117".into(),
    };
    update(&scratch.store(), |s| s.insert(ssh)).unwrap();
    grant_request(&mut client, "dssh", Grant::WatchSteer);
    let told = window.result(&mut client, "the SSH refusal");
    assert_eq!(told["why"], SSH_ONLY_WATCHES);
    assert_eq!(window.count("remote_grant"), 2);

    // Nor is a device whose store entry does not say how it connects.
    let unknown = Device {
        id: "dold".into(),
        name: "Old".into(),
        recipe: Recipe::Unknown,
        confirmed: None,
        ..stored(&scratch.store(), "dssh").unwrap()
    };
    update(&scratch.store(), |s| s.insert(unknown)).unwrap();
    grant_request(&mut client, "dold", Grant::WatchSteer);
    let told = window.result(&mut client, "the unknown-recipe refusal");
    assert_eq!(told["why"], UNKNOWN_RECIPE_ONLY_WATCHES);
    assert_eq!(window.count("remote_grant"), 2);
}

#[test]
fn revoke_self_needs_its_own_token() {
    let scratch = Scratch::new();
    let window = Window::new(&scratch, INSTANCE_A);
    let mut client = window.connect();
    let (a, token_a) = window.enroll_with_yes(&mut client, "Pixel", "watch");
    let (b, token_b) = window.enroll_with_yes(&mut client, "Tablet", "watch");

    for (device, token) in [
        (a.as_str(), token_b.as_str()),
        (a.as_str(), "wrong"),
        ("d0000", token_a.as_str()),
    ] {
        revoke_self(&mut client, device, token);
        let told = window.result(&mut client, "the refusal");
        assert_eq!(
            (told["ok"].as_bool(), told["why"].as_str()),
            (Some(false), Some(NOT_THIS_DEVICE))
        );
    }
    assert_eq!(devices_in(&scratch.store()).len(), 2, "nothing was taken out");

    revoke_self(&mut client, &a, &token_a);
    assert_eq!(window.result(&mut client, "the revocation")["ok"], true);
    assert!(stored(&scratch.store(), &a).is_none());
    assert!(stored(&scratch.store(), &b).is_some(), "the other device stays");

    // Revoked, the token proves nothing any more.
    revoke_self(&mut client, &a, &token_a);
    assert_eq!(window.result(&mut client, "a second revocation")["ok"], false);
}

#[test]
fn push_endpoint_is_validated() {
    let long = format!("https://n.example/{}", "a".repeat(PUSH_ENDPOINT_MAX_BYTES - 18));
    assert_eq!(long.len(), PUSH_ENDPOINT_MAX_BYTES);
    for good in [
        "https://ntfy.example/up7f3a",
        "HTTP://203.0.113.1:8080/up?x=1",
        "https://[fd00::1]:443/up",
        "https://ntfy.example",
        long.as_str(),
    ] {
        assert_eq!(push_endpoint_ok(good), Ok(()), "{good}");
    }
    let too_long = format!("{long}a");
    for bad in [
        "",
        "ftp://ntfy.example/t",
        "https://",
        "https:///t",
        "https://user:pw@desk/t",
        "https://@ntfy.example/t",
        "http://ntfy example/t",
        "https://ntfy.example:0/t",
        "https://ntfy.example:99999/t",
        "https://ntfy.example:x/t",
        "https://[fd00::1/t",
        "https://[fd00::1]x/t",
        "javascript:alert(1)",
        "https://ntfy.example/\u{e9}",
        "https://ntfy.example\\@evil/t",
        "https://ntfy.example/t\n",
        too_long.as_str(),
    ] {
        assert!(push_endpoint_ok(bad).is_err(), "{bad:?}");
    }

    let scratch = Scratch::new();
    let window = Window::new(&scratch, INSTANCE_A);
    let mut client = window.connect();
    let (device, token) = window.enroll_with_yes(&mut client, "Pixel", "watch");

    set_push(&mut client, &device, &token, "https://user:pw@desk/t");
    let told = window.result(&mut client, "a bad endpoint");
    assert_eq!(
        (told["ok"].as_bool(), told["why"].as_str()),
        (Some(false), Some(BAD_PUSH_ENDPOINT))
    );
    set_push(&mut client, &device, "wrong", "https://ntfy.example/up7f3a");
    let told = window.result(&mut client, "a wrong token");
    assert_eq!(told["why"], NOT_THIS_DEVICE);
    assert_eq!(stored(&scratch.store(), &device).unwrap().push_endpoint, None);

    set_push(&mut client, &device, &token, "https://ntfy.example/up7f3a");
    assert_eq!(window.result(&mut client, "the endpoint")["ok"], true);
    assert_eq!(
        stored(&scratch.store(), &device).unwrap().push_endpoint.as_deref(),
        Some("https://ntfy.example/up7f3a")
    );

    // Revoked, the device's endpoint goes with it.
    revoke_self(&mut client, &device, &token);
    assert_eq!(window.result(&mut client, "the revocation")["ok"], true);
    assert!(!std::fs::read_to_string(scratch.store())
        .unwrap()
        .contains("ntfy.example"));
}

#[test]
fn last_seen_and_host_follow_devices_connected() {
    let scratch = Scratch::new();
    let window = Window::new(&scratch, INSTANCE_A);
    let mut client = window.connect();
    let (a, _) = window.enroll_with_yes(&mut client, "Pixel", "watch");
    let (b, _) = window.enroll_with_yes(&mut client, "Tablet", "watch");
    let start = now_secs();
    let store = scratch.store();
    let set_seen = |device: &str, at: Option<u64>| {
        update(&store, |s| s.get_mut(device).unwrap().last_seen = at).unwrap();
    };

    // Opened: the device's `Host` and the time; the other device is not touched.
    report(&mut client, &[(&a, "203.0.113.5:43117")]);
    window.turn_until("a's last host", || stored(&store, &a).unwrap().last_host.is_some());
    let seen = stored(&store, &a).unwrap();
    assert_eq!(seen.last_host.as_deref(), Some("203.0.113.5:43117"));
    assert!(seen.last_seen.unwrap() >= start);
    let other = stored(&store, &b).unwrap();
    assert_eq!((other.last_seen, other.last_host), (None, None));

    // Closed: the time again, the `Host` kept.
    set_seen(&a, Some(0));
    report(&mut client, &[]);
    window.turn_until("a's close", || stored(&store, &a).unwrap().last_seen != Some(0));
    let seen = stored(&store, &a).unwrap();
    assert!(seen.last_seen.unwrap() >= start);
    assert_eq!(seen.last_host.as_deref(), Some("203.0.113.5:43117"));

    // A device closed before is not touched by a later report; a device the store does not hold is not added.
    set_seen(&a, Some(0));
    report(&mut client, &[(&b, "127.0.0.1:43117"), ("d0000", "127.0.0.1:43117")]);
    window.turn_until("b's last host", || stored(&store, &b).unwrap().last_host.is_some());
    assert_eq!(stored(&store, &a).unwrap().last_seen, Some(0));
    assert_eq!(devices_in(&store).len(), 2);

    // A report is not answered.
    window.settle();
    assert!(client.recv(Duration::from_millis(20)).unwrap().is_none());
}

#[test]
fn every_report_tells_the_base_page_who_is_connected() {
    let scratch = Scratch::new();
    let window = Window::new(&scratch, INSTANCE_A);
    let mut client = window.connect();
    let (a, _) = window.enroll_with_yes(&mut client, "Pixel", "watch");
    let (b, _) = window.enroll_with_yes(&mut client, "Tablet", "watch+steer");

    // The newest `remote_connected` the base page was sent, once there are `after` of them.
    let latest = |after: usize| {
        window.turn_until("a remote_connected", || window.count("remote_connected") >= after);
        let all: Vec<serde_json::Value> = window
            .base
            .sent
            .borrow()
            .iter()
            .map(|e| json_of(e))
            .filter(|v| v["kind"] == "remote_connected")
            .collect();
        all.last().unwrap().clone()
    };

    // Named from the store (name and grant) and the report (`Host`); a device the store does not hold is left out.
    report(
        &mut client,
        &[
            (&a, "203.0.113.5:43117"),
            (&b, "127.0.0.1:43117"),
            ("d0000", "127.0.0.1:1"),
        ],
    );
    let told = latest(1);
    let devices = told["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 2, "{told}");
    assert_eq!(devices[0]["device"], a.as_str());
    assert_eq!(devices[0]["name"], "Pixel");
    assert_eq!(devices[0]["grant"], "watch");
    assert_eq!(devices[0]["host"], "203.0.113.5:43117");
    assert_eq!(devices[1]["name"], "Tablet");
    assert_eq!(devices[1]["grant"], "watch+steer");

    // The list replaces the last one, and an empty report clears it.
    report(&mut client, &[(&b, "127.0.0.1:43117")]);
    assert_eq!(latest(2)["devices"].as_array().unwrap().len(), 1);
    report(&mut client, &[]);
    assert_eq!(latest(3)["devices"], serde_json::json!([]));
    assert!(!told.to_string().contains("token"), "{told}");
}

#[test]
fn the_list_of_connected_phones_reaches_every_page_and_a_new_document() {
    let scratch = Scratch::new();
    let window = Window::new(&scratch, INSTANCE_A);
    let mut client = window.connect();
    let (a, _) = window.enroll_with_yes(&mut client, "Pixel", "watch");
    let told = |page: &RecordingPage| {
        page.sent
            .borrow()
            .iter()
            .map(|e| json_of(e))
            .filter(|v| v["kind"] == "remote_connected")
            .count()
    };
    let last = |page: &RecordingPage| {
        page.sent
            .borrow()
            .iter()
            .map(|e| json_of(e))
            .filter(|v| v["kind"] == "remote_connected")
            .next_back()
            .map(|v| v["devices"].as_array().unwrap().len())
    };

    // A page made for a tab broken out is told as well as the base page.
    report(&mut client, &[(&a, "127.0.0.1:43117")]);
    window.turn_until("the second page's list", || told(&window.second) >= 1);
    assert_eq!(last(&window.second), Some(1));
    assert_eq!(last(&window.base), Some(1));

    // A document that loads while a phone is connected starts with nobody: its `ready` brings the list back.
    let before = (told(&window.base), told(&window.second));
    window.handle.page_message(ViewId::BASE, READY);
    window.handle.page_message(view(2), READY);
    assert_eq!(told(&window.base), before.0 + 1);
    assert_eq!(told(&window.second), before.1 + 1);
    assert_eq!(last(&window.base), Some(1));
    assert_eq!(last(&window.second), Some(1));

    // Once the last phone is gone a new document is told nothing about phones.
    report(&mut client, &[]);
    window.turn_until("the empty list", || last(&window.base) == Some(0));
    let after = told(&window.base);
    window.handle.page_message(ViewId::BASE, READY);
    assert_eq!(told(&window.base), after);
}

#[test]
fn a_phone_connected_twice_is_listed_once() {
    let scratch = Scratch::new();
    let window = Window::new(&scratch, INSTANCE_A);
    let mut client = window.connect();
    let (a, _) = window.enroll_with_yes(&mut client, "Pixel", "watch");
    let (b, _) = window.enroll_with_yes(&mut client, "Tablet", "watch+steer");

    report(
        &mut client,
        &[(&a, "127.0.0.1:1"), (&b, "127.0.0.1:2"), (&a, "127.0.0.1:3")],
    );
    let told = window.asked("remote_connected", 0);
    let devices = told["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 2, "{told}");
    assert_eq!(devices[0]["device"], a.as_str());
    assert_eq!(devices[1]["device"], b.as_str());
}

#[test]
fn device_without_confirmation_is_flagged_once_per_process() {
    let scratch = Scratch::new();
    let a = Window::new(&scratch, INSTANCE_A);
    let b = Window::new(&scratch, INSTANCE_B);
    let mut to_a = a.connect();
    let mut to_b = b.connect();
    let (confirmed, _) = a.enroll_with_yes(&mut to_a, "Pixel", "watch");

    // A device written by something other than a window's enrollment: no desk's yes recorded with it.
    let mut random = [0u8; 8];
    eitri_core::remote_pairing::random_fill(&mut random).unwrap();
    let planted: String = std::iter::once("d".to_owned())
        .chain(random.iter().map(|b| format!("{b:02x}")))
        .collect();
    let store = scratch.store();
    update(&store, |s| {
        s.insert(Device {
            id: planted.clone(),
            name: "Planted".into(),
            token_sha256: [2; 32],
            grant: Grant::WatchSteer,
            recipe: Recipe::Unknown,
            confirmed: None,
            push_endpoint: None,
            last_seen: None,
            last_host: None,
            enrolled_host: String::new(),
        })
    })
    .unwrap();

    report(&mut to_a, &[(&planted, "127.0.0.1:43117")]);
    a.turn_until("the flag", || a.flagged(&planted) == 1);

    // Reported again, to this window and to another in the same process: not flagged again.
    for (window, client) in [(&a, &mut to_a), (&b, &mut to_b)] {
        update(&store, |s| s.get_mut(&planted).unwrap().last_seen = Some(0)).unwrap();
        report(client, &[(&planted, "127.0.0.1:43117")]);
        window.turn_until("the report", || stored(&store, &planted).unwrap().last_seen != Some(0));
        window.settle();
    }
    assert_eq!(a.flagged(&planted) + b.flagged(&planted), 1);
    assert_eq!(
        a.flagged(&confirmed) + b.flagged(&confirmed),
        0,
        "a device the desk said yes to is not flagged"
    );
}

#[test]
fn phone_errands_held_by_the_store_lock_are_bounded() {
    let scratch = Scratch::new();
    let window = Window::new(&scratch, INSTANCE_A);
    let mut client = window.connect();
    let (device, token) = window.enroll_with_yes(&mut client, "Pixel", "watch");
    // Another process holds the store's lock: every errand that reaches the store waits for it.
    let lock = agent::private_fs::PrivateLock::open(&scratch.store().with_extension("lock")).unwrap();
    assert!(lock.try_lock().unwrap());
    let sent = PHONE_ERRANDS_MAX + 6;
    for _ in 0..sent {
        set_push(&mut client, &device, &token, "https://ntfy.example/up7f3a");
    }
    // The ones past the bound are told at once; the others wait for the lock, and give up on it in time.
    let mut busy = 0;
    for _ in 0..sent {
        let told = window.result(&mut client, "an errand's answer");
        assert!(window.socket.shared.devices.phone_errands.load(Ordering::SeqCst) <= PHONE_ERRANDS_MAX);
        assert_ne!(
            told["ok"], true,
            "the store was changed while its lock was held: {told}"
        );
        if told["why"] == ERRANDS_BUSY {
            busy += 1;
        }
    }
    assert!(busy >= 1, "every errand waited on a thread of its own");
    drop(lock);
    window.turn_until("every errand's thread to end", || {
        window.socket.shared.devices.phone_errands.load(Ordering::SeqCst) == 0
    });
    // With room again, the endpoint is taken.
    set_push(&mut client, &device, &token, "https://ntfy.example/up7f3a");
    assert_eq!(window.result(&mut client, "the endpoint")["ok"], true);
}
