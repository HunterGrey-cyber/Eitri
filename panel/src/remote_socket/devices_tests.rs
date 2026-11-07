//! Enrollment and the listen address through the `rv-` socket, end to end within a window: the reader's checks, the
//! base page's question, the desk's answer and what reaches the store and the peer.

use super::*;
use crate::agent_panel::tests::*;
use eitri_core::remote_devices::{update, DeviceStore};
use eitri_core::remote_pairing::{confirmed_listen, PairScheme};
use eitri_core::remote_rv::{rv_socket_path, RvClient};
use std::os::unix::fs::DirBuilderExt;
use std::sync::atomic::AtomicU32;

const PATIENCE: Duration = Duration::from_secs(30);

/// A private directory standing in for the control and state directories, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Scratch {
        static SERIAL: AtomicU32 = AtomicU32::new(0);
        let base =
            PathBuf::from(option_env!("CARGO_TARGET_DIR").unwrap_or(concat!(env!("CARGO_MANIFEST_DIR"), "/../target")))
                .join("tmp");
        std::fs::create_dir_all(&base).unwrap();
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
            routes: PairingRoutes {
                ssh: None,
                direct: Some(Endpoint {
                    mode: PairMode::Direct,
                    scheme: PairScheme::Http,
                    host: "127.0.0.1".into(),
                    port: 7341,
                }),
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
        self.start_by(client, None).0
    }

    /// `start_enroll` asking for `recipe` through `client`; the code the base page shows, and the answer `add-device`
    /// reads.
    fn start_by(&self, client: &mut RvClient, recipe: Option<PairMode>) -> (String, serde_json::Value) {
        let before = self.count("remote_pairing");
        client.send(&RvInbound::StartEnroll { v: RV_PROTOCOL, recipe }).unwrap();
        let answer = self.until(client, "start_enroll's answer", |v| {
            v["kind"] == "enroll_shown" || v["kind"] == "result" || v["kind"] == "refused"
        });
        assert_eq!(answer["kind"], "enroll_shown", "{answer}");
        let shown = self.asked("remote_pairing", before);
        (shown["code"].as_str().unwrap().to_owned(), answer)
    }

    /// A phone's enrollment by the direct recipe, from a loopback peer (a proxy on the desk, as `serve` sees it).
    fn enroll(client: &mut RvClient, code: &str, name: &str) {
        Window::enroll_by(client, code, name, PairMode::Direct, "127.0.0.1:50122");
    }

    fn enroll_by(client: &mut RvClient, code: &str, name: &str, recipe: PairMode, peer: &str) {
        client
            .send(&RvInbound::Enroll {
                v: RV_PROTOCOL,
                code: Secret::new(code),
                device_name: name.into(),
                host: "127.0.0.1:7341".into(),
                peer: peer.into(),
                recipe,
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

/// The number the next report of the tests' stand-in `serve` gets: one sequence for every test, so each window sees its
/// reports in the order they were sent.
static REPORT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// `serve`'s report that the devices in `connected` (id, `Host`) are connected now.
fn report(client: &mut RvClient, connected: &[(&str, &str)]) {
    let seq = REPORT_SEQ.fetch_add(1, Ordering::SeqCst);
    report_as(client, connected, 1, seq);
}

/// [`report`] as report `seq` of the `serve` that started at `epoch`.
fn report_as(client: &mut RvClient, connected: &[(&str, &str)], epoch: u64, seq: u64) {
    client
        .send(&RvInbound::DevicesConnected {
            v: RV_PROTOCOL,
            epoch,
            seq,
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
        routes: PairingRoutes {
            ssh: Some(ssh_endpoint()),
            direct: None,
        },
        ssh_dir: scratch.0.clone(),
    });
    let mut client = window.connect();
    let (code, answer) = window.start_by(&mut client, None);
    assert_eq!(answer["other"], false, "the window has no direct route");
    let shown = window.asked("remote_pairing", 0);
    assert!(shown["uri"].as_str().unwrap().contains("&m=ssh&s=http&"), "{shown}");
    let before = window.count("remote_enroll");
    Window::enroll_by(&mut client, &code, "Pixel", PairMode::Ssh, "127.0.0.1:50122");
    let asked = window.asked("remote_enroll", before);
    assert_eq!(asked["grants"], serde_json::json!(["watch"]));
    assert_eq!(asked["recipe"], "ssh");
    assert_eq!(asked["target"], "through SSH to 127.0.0.1:41000 on this desk");
    window.answer("steer", asked["nonce"].as_u64().unwrap(), true, "watch+steer");
    assert!(matches!(result_for(&window.base.sent.borrow(), "steer"), Some(Err(_))));
    window.answer("watch", asked["nonce"].as_u64().unwrap(), true, "watch");
    let enrolled = window.until(&mut client, "enrolled", |v| v["kind"] == "enrolled");
    let store = DeviceStore::load(&scratch.store()).unwrap();
    let stored = store.get(enrolled["device"].as_str().unwrap()).unwrap();
    assert_eq!((stored.grant, stored.recipe), (Grant::Watch, Recipe::Ssh));
}

/// The SSH route of a loopback `serve` on port 41000.
fn ssh_endpoint() -> Endpoint {
    Endpoint {
        mode: PairMode::Ssh,
        scheme: PairScheme::Http,
        host: "127.0.0.1".into(),
        port: 41000,
    }
}

/// A window whose `serve` listens on loopback behind Tailscale Serve: both routes.
fn window_with_both_routes(scratch: &Scratch) -> Window {
    let window = Window::new(scratch, INSTANCE_A);
    window.socket.set_pairing(Pairing {
        routes: PairingRoutes {
            ssh: Some(ssh_endpoint()),
            direct: Some(Endpoint {
                mode: PairMode::Direct,
                scheme: PairScheme::Https,
                host: "desk.tail1.ts.net".into(),
                port: 443,
            }),
        },
        ssh_dir: scratch.0.clone(),
    });
    window
}

#[test]
fn each_enrollment_takes_the_recipe_it_asks_for() {
    let scratch = Scratch::new();
    let window = window_with_both_routes(&scratch);
    let mut client = window.connect();

    // With none asked, the name set up for phones.
    let (_, answer) = window.start_by(&mut client, None);
    assert_eq!(answer["recipe"], "direct");
    assert_eq!(answer["target"], "directly to desk.tail1.ts.net:443 (https)");
    assert_eq!(answer["other"], true, "the SSH route is there too");
    let shown = window.asked("remote_pairing", 0);
    assert!(
        shown["uri"]
            .as_str()
            .unwrap()
            .contains("&m=direct&s=https&h=desk.tail1.ts.net&p=443"),
        "{shown}"
    );

    // `--ssh`: a phone's own SSH route still enrolls beside the Tailscale name.
    let (code, answer) = window.start_by(&mut client, Some(PairMode::Ssh));
    assert_eq!(answer["recipe"], "ssh");
    assert_eq!(answer["other"], true);
    assert_eq!(answer["target"], "through SSH to 127.0.0.1:41000 on this desk");
    let shown = window.asked("remote_pairing", 1);
    assert!(
        shown["uri"]
            .as_str()
            .unwrap()
            .contains("&m=ssh&s=http&h=127.0.0.1&p=41000"),
        "{shown}"
    );
    let before = window.count("remote_enroll");
    Window::enroll_by(&mut client, &code, "Pixel", PairMode::Ssh, "127.0.0.1:50122");
    let asked = window.asked("remote_enroll", before);
    assert_eq!(asked["target"], "through SSH to 127.0.0.1:41000 on this desk");
    assert_eq!(asked["grants"], serde_json::json!(["watch"]));
    window.answer("yes", asked["nonce"].as_u64().unwrap(), true, "watch");
    let enrolled = window.until(&mut client, "enrolled", |v| v["kind"] == "enrolled");
    let stored = stored(&scratch.store(), enrolled["device"].as_str().unwrap()).unwrap();
    assert_eq!(
        (stored.grant, stored.recipe),
        (Grant::Watch, Recipe::Ssh),
        "the code's recipe"
    );
}

#[test]
fn an_enrollment_by_another_recipe_than_its_code_is_refused() {
    let scratch = Scratch::new();
    let window = window_with_both_routes(&scratch);
    let mut client = window.connect();
    let refused = |window: &Window, client: &mut RvClient, why: &str| {
        let told = window.until(client, "the refusal", |v| v["kind"] == "enroll_refused");
        assert_eq!(told["why"], why);
        assert_eq!(window.count("remote_enroll"), 0, "the desk is not asked");
        assert!(devices_in(&scratch.store()).is_empty(), "nothing is stored");
    };

    // A direct code presented by a phone that came through its SSH forward is not stored as direct.
    let code = window.start(&mut client);
    Window::enroll_by(&mut client, &code, "Pixel", PairMode::Ssh, "127.0.0.1:50122");
    refused(&window, &mut client, RECIPE_DIFFERS);

    // An SSH code from a phone that says direct.
    let (code, _) = window.start_by(&mut client, Some(PairMode::Ssh));
    Window::enroll_by(&mut client, &code, "Pixel", PairMode::Direct, "127.0.0.1:50122");
    refused(&window, &mut client, RECIPE_DIFFERS);

    // An SSH code over a connection that came from another machine, not through a forward on the desk.
    for peer in ["198.51.100.9:50122", "[fd00::9]:50122", "unknown"] {
        let (code, _) = window.start_by(&mut client, Some(PairMode::Ssh));
        Window::enroll_by(&mut client, &code, "Pixel", PairMode::Ssh, peer);
        refused(&window, &mut client, SSH_CODE_NOT_FORWARDED);
    }

    // The code is spent by a refused try: the same code again is unknown.
    let (code, _) = window.start_by(&mut client, Some(PairMode::Ssh));
    Window::enroll_by(&mut client, &code, "Pixel", PairMode::Direct, "127.0.0.1:50122");
    refused(&window, &mut client, RECIPE_DIFFERS);
    Window::enroll_by(&mut client, &code, "Pixel", PairMode::Ssh, "127.0.0.1:50122");
    refused(
        &window,
        &mut client,
        "that enrollment code is not this desk's, or was already used",
    );

    // Through the Unix socket, or a loopback peer in IPv4-mapped form, an SSH code is forwarded.
    for peer in ["unix", "[::ffff:127.0.0.1]:50122", "[::1]:50122"] {
        let (code, _) = window.start_by(&mut client, Some(PairMode::Ssh));
        let before = window.count("remote_enroll");
        Window::enroll_by(&mut client, &code, "Pixel", PairMode::Ssh, peer);
        let asked = window.asked("remote_enroll", before);
        window.answer("no", asked["nonce"].as_u64().unwrap(), false, "watch");
        window.until(&mut client, "the desk's no", |v| v["kind"] == "enroll_refused");
    }
}

#[test]
fn a_recipe_the_window_has_no_route_for_is_refused_and_shows_nothing() {
    let scratch = Scratch::new();
    // A named address: direct only.
    let window = Window::new(&scratch, INSTANCE_A);
    let mut client = window.connect();
    client
        .send(&RvInbound::StartEnroll {
            v: RV_PROTOCOL,
            recipe: Some(PairMode::Ssh),
        })
        .unwrap();
    let told = window.until(&mut client, "the refusal", |v| {
        v["kind"] == "refused" || v["kind"] == "result"
    });
    assert_eq!(told["why"], eitri_core::remote_pairing::NO_SSH_ROUTE, "{told}");
    assert_eq!(window.count("remote_pairing"), 0);
    window.socket.set_pairing(Pairing {
        routes: PairingRoutes {
            ssh: Some(ssh_endpoint()),
            direct: None,
        },
        ssh_dir: scratch.0.clone(),
    });
    client
        .send(&RvInbound::StartEnroll {
            v: RV_PROTOCOL,
            recipe: Some(PairMode::Direct),
        })
        .unwrap();
    let told = window.until(&mut client, "the refusal", |v| {
        v["kind"] == "refused" || v["kind"] == "result"
    });
    assert_eq!(told["why"], eitri_core::remote_pairing::NO_DIRECT_ROUTE, "{told}");
    assert_eq!(window.count("remote_pairing"), 0);
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
    client
        .send(&RvInbound::StartEnroll {
            v: RV_PROTOCOL,
            recipe: None,
        })
        .unwrap();
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
fn a_report_older_than_the_last_one_taken_is_dropped() {
    let scratch = Scratch::new();
    let window = Window::new(&scratch, INSTANCE_A);
    let mut client = window.connect();
    let (a, _) = window.enroll_with_yes(&mut client, "Pixel", "watch");
    let sent = || window.count("remote_connected");
    let latest = || {
        let all: Vec<serde_json::Value> = window
            .base
            .sent
            .borrow()
            .iter()
            .map(|e| json_of(e))
            .filter(|v| v["kind"] == "remote_connected")
            .collect();
        all.last().cloned()
    };
    // Each report on a connection of its own, as `serve` sends them.
    let send = |connected: &[(&str, &str)], epoch, seq| {
        let mut one = window.connect();
        report_as(&mut one, connected, epoch, seq);
        one
    };

    // A serve that started at 2000 reports a phone as its second report.
    let _held = send(&[(&a, "127.0.0.1:43117")], 2000, 1);
    window.turn_until("the phone's report", || sent() >= 1);
    assert_eq!(latest().unwrap()["devices"].as_array().unwrap().len(), 1);

    // Its first report, the empty one, read late: dropped, the phone stays listed.
    let _late = send(&[], 2000, 0);
    // A report of an earlier serve, read after the newer serve's: dropped too.
    let _dead = send(&[], 1000, 9);
    // The same report again: nothing new.
    let _again = send(&[(&a, "127.0.0.1:43117")], 2000, 1);
    window.settle();
    assert_eq!(sent(), 1, "{:?}", latest());

    // The next report of that serve, and the first of a later one, are taken.
    let _next = send(&[], 2000, 2);
    window.turn_until("the close", || sent() >= 2);
    assert_eq!(latest().unwrap()["devices"], serde_json::json!([]));
    let _newer = send(&[(&a, "127.0.0.1:43117")], 3000, 0);
    window.turn_until("the later serve's report", || sent() >= 3);
    assert_eq!(latest().unwrap()["devices"].as_array().unwrap().len(), 1);
    window.settle();
    assert_eq!(sent(), 3, "no dropped report was taken late");
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

#[test]
fn remote_phone_list_clears_when_the_serve_lock_is_released() {
    let scratch = Scratch::new();
    let window = Window::new(&scratch, INSTANCE_A);
    let lock_path = scratch.0.join("remote.lock");
    let lock = agent::private_fs::PrivateLock::open(&lock_path).unwrap();
    assert!(lock.try_lock().unwrap());
    let probes = Arc::new(AtomicUsize::new(0));
    let counted = probes.clone();
    window.socket.watch_serve(move |phones| {
        let running = phones.then(|| {
            counted.fetch_add(1, Ordering::SeqCst);
            !agent::private_fs::PrivateLock::open(&lock_path)
                .unwrap()
                .try_lock()
                .unwrap()
        });
        (running, Some("partial".into()))
    });
    let mut client = window.connect();
    let (device, _) = window.enroll_with_yes(&mut client, "Pixel", "watch");
    check_serve(&window.socket.shared);
    assert_eq!(probes.load(Ordering::SeqCst), 0, "no lock check without phones");
    report(&mut client, &[(&device, "127.0.0.1:43117")]);
    window.turn_until("phone mark", || {
        crate::agent_panel::remote_enroll::connected_for_new_document(&window.handle.state.borrow()).is_some()
    });
    check_serve(&window.socket.shared);
    window.turn();
    assert!(crate::agent_panel::remote_enroll::connected_for_new_document(&window.handle.state.borrow()).is_some());
    let shown = window
        .base
        .sent
        .borrow()
        .iter()
        .map(|s| json_of(s))
        .filter(|v| v["kind"] == "remote_connected")
        .last()
        .unwrap();
    assert_eq!(shown["sandbox"], "partial");
    let store = scratch.store();
    update(&store, |s| s.get_mut(&device).unwrap().last_seen = Some(0)).unwrap();
    drop(lock);
    check_serve(&window.socket.shared);
    window.turn();
    assert!(crate::agent_panel::remote_enroll::connected_for_new_document(&window.handle.state.borrow()).is_none());
    assert!(stored(&store, &device).unwrap().last_seen.unwrap() > 0);
    let shown = window
        .base
        .sent
        .borrow()
        .iter()
        .map(|s| json_of(s))
        .filter(|v| v["kind"] == "remote_connected")
        .last()
        .unwrap();
    assert_eq!(shown["devices"], serde_json::json!([]));
    let before = probes.load(Ordering::SeqCst);
    check_serve(&window.socket.shared);
    assert_eq!(probes.load(Ordering::SeqCst), before, "empty stays unchecked");
    let restarted = agent::private_fs::PrivateLock::open(&scratch.0.join("remote.lock")).unwrap();
    assert!(restarted.try_lock().unwrap());
    report(&mut client, &[(&device, "127.0.0.1:43117")]);
    window.turn_until("a later report", || {
        crate::agent_panel::remote_enroll::connected_for_new_document(&window.handle.state.borrow()).is_some()
    });
}

// The window's audit records of enrollments and the desk's answers about devices.

/// Every record of `window`'s log so far, oldest first.
fn audit_records(window: &Window, scratch: &Scratch) -> Vec<eitri_core::remote_audit::Record> {
    window.socket.audit().flush();
    let dir = eitri_core::remote_audit::audit_dir(scratch.store().parent().unwrap());
    let mut records = eitri_core::remote_audit::read(&dir, &Default::default(), None).unwrap_or_default();
    records.reverse();
    records
}

fn audit_field<'a>(record: &'a eitri_core::remote_audit::Record, name: &str) -> Option<&'a str> {
    record.fields.get(name).and_then(serde_json::Value::as_str)
}

/// The records of `event`.
fn of_event<'a>(
    records: &'a [eitri_core::remote_audit::Record],
    event: &str,
) -> Vec<&'a eitri_core::remote_audit::Record> {
    records.iter().filter(|r| r.event == event).collect()
}

#[test]
fn enrollment_records_each_outcome() {
    let scratch = Scratch::new();
    let window = Window::new(&scratch, INSTANCE_A);
    let mut client = window.connect();
    let covered = Rc::new(Cell::new(false));
    let _gate = recording_gate(&window.handle, &[1], covered);

    // `y`: offered, then confirmed once the device is stored.
    let (device, token) = window.enroll_with_yes(&mut client, "Pixel", "watch");
    let records = audit_records(&window, &scratch);
    let offered = of_event(&records, "enroll_offered");
    assert_eq!(offered.len(), 1);
    assert_eq!(
        (audit_field(offered[0], "recipe"), audit_field(offered[0], "instance")),
        (Some("direct"), Some(INSTANCE_A))
    );
    let confirmed = of_event(&records, "enroll_confirmed");
    assert_eq!(confirmed.len(), 1);
    assert_eq!(
        (
            audit_field(confirmed[0], "device"),
            audit_field(confirmed[0], "device_name"),
            audit_field(confirmed[0], "recipe"),
            audit_field(confirmed[0], "grant"),
            audit_field(confirmed[0], "host"),
            audit_field(confirmed[0], "peer"),
        ),
        (
            Some(device.as_str()),
            Some("Pixel"),
            Some("direct"),
            Some("watch"),
            Some("127.0.0.1:7341"),
            Some("127.0.0.1:50122")
        )
    );
    assert!(records.iter().all(|r| r.writer == INSTANCE_A));

    // `n`: refused, why the desk's no.
    let code = window.start(&mut client);
    let before = window.count("remote_enroll");
    Window::enroll(&mut client, &code, "Tablet");
    let asked = window.asked("remote_enroll", before);
    window.answer("no", asked["nonce"].as_u64().unwrap(), false, "watch");
    window.until(&mut client, "the refusal", |v| v["kind"] == "enroll_refused");
    let records = audit_records(&window, &scratch);
    let refused = of_event(&records, "enroll_refused");
    assert_eq!(refused.len(), 1);
    assert_eq!(
        (audit_field(refused[0], "device_name"), audit_field(refused[0], "why")),
        (Some("Tablet"), Some("desk_said_no"))
    );
    assert!(of_event(&records, "enroll_confirmed").len() == 1, "no second device");

    // A wrong code: refused as an invalid code, naming the phone as it claims to be.
    let shown = window.start(&mut client);
    Window::enroll(&mut client, "AAAAAAAAAAAAAAAAAAAAAA", "Stranger");
    window.until(&mut client, "the refusal", |v| v["kind"] == "enroll_refused");
    let records = audit_records(&window, &scratch);
    let invalid: Vec<_> = of_event(&records, "refused")
        .into_iter()
        .filter(|r| audit_field(r, "reason") == Some("enroll_code_invalid"))
        .collect();
    assert_eq!(invalid.len(), 1);
    assert_eq!(audit_field(invalid[0], "device_name"), Some("Stranger"));

    // A code left unused: expired once, when a newer one replaces it or its time runs out.
    window.socket.note_lapsed_at(Instant::now());
    assert!(
        of_event(&audit_records(&window, &scratch), "enroll_expired").is_empty(),
        "still in time"
    );
    let _ = window.start(&mut client);
    let later = Instant::now() + eitri_core::remote_pairing::ENROLL_CODE_TTL + Duration::from_secs(1);
    window.socket.note_lapsed_at(later);
    window.socket.note_lapsed_at(later);
    let records = audit_records(&window, &scratch);
    let expired = of_event(&records, "enroll_expired");
    assert_eq!(
        expired.len(),
        2,
        "the code replaced unused, and the one that ran out, each once"
    );
    assert!(expired.iter().all(|r| audit_field(r, "instance") == Some(INSTANCE_A)));

    // Never a code or a token.
    let bytes = std::fs::read_to_string(
        eitri_core::remote_audit::audit_dir(scratch.store().parent().unwrap()).join("audit.jsonl"),
    )
    .unwrap();
    for secret in [token.as_str(), code.as_str(), shown.as_str()] {
        assert!(!bytes.contains(secret), "a secret reached the log");
    }
}

#[test]
fn listen_and_grant_answers_are_recorded() {
    let scratch = Scratch::new();
    let window = Window::new(&scratch, INSTANCE_A);
    window
        .socket
        .set_link_of(|ip| (ip.to_string() == "198.51.100.20").then(|| ("eth0".to_owned(), IfaceKind::Plain)));
    let mut client = window.connect();
    let covered = Rc::new(Cell::new(false));
    let _gate = recording_gate(&window.handle, &[1], covered);
    let listen = |client: &mut RvClient| {
        client
            .send(&RvInbound::ListenRequest {
                v: RV_PROTOCOL,
                addr: "198.51.100.20".into(),
            })
            .unwrap();
    };

    // `n` records nothing; `y` records the address, its link and its kind.
    listen(&mut client);
    let asked = window.asked("remote_listen", 0);
    window.listen_answer("n", asked["nonce"].as_u64().unwrap(), false);
    window.until(&mut client, "the no", |v| v["kind"] == "listen_answer");
    assert!(of_event(&audit_records(&window, &scratch), "listen_changed").is_empty());
    listen(&mut client);
    let asked = window.asked("remote_listen", 1);
    window.listen_answer("y", asked["nonce"].as_u64().unwrap(), true);
    window.until(&mut client, "the yes", |v| v["kind"] == "listen_answer");
    let records = audit_records(&window, &scratch);
    let changed = of_event(&records, "listen_changed");
    assert_eq!(changed.len(), 1);
    assert_eq!(
        (
            audit_field(changed[0], "listener"),
            audit_field(changed[0], "listen_addr"),
            audit_field(changed[0], "listen_iface"),
            audit_field(changed[0], "listen_kind"),
            changed[0].fields.get("confirmed").and_then(serde_json::Value::as_bool),
        ),
        (
            Some("address"),
            Some("198.51.100.20"),
            Some("eth0"),
            Some(IfaceKind::Plain.as_str()),
            Some(true)
        )
    );

    // A grant: `n` records nothing, `y` the change once written.
    let (device, _token) = window.enroll_with_yes(&mut client, "Pixel", "watch");
    grant_request(&mut client, &device, Grant::WatchSteer);
    let asked = window.asked("remote_grant", 0);
    window.answer("gn", asked["nonce"].as_u64().unwrap(), false, "watch+steer");
    window.result(&mut client, "the no");
    assert!(of_event(&audit_records(&window, &scratch), "grant_changed").is_empty());
    grant_request(&mut client, &device, Grant::WatchSteer);
    let asked = window.asked("remote_grant", 1);
    window.answer("gy", asked["nonce"].as_u64().unwrap(), true, "watch+steer");
    assert_eq!(window.result(&mut client, "the yes")["ok"], true);
    let records = audit_records(&window, &scratch);
    let granted = of_event(&records, "grant_changed");
    assert_eq!(granted.len(), 1);
    assert_eq!(
        (
            audit_field(granted[0], "device"),
            audit_field(granted[0], "device_name"),
            audit_field(granted[0], "from"),
            audit_field(granted[0], "to"),
        ),
        (Some(device.as_str()), Some("Pixel"), Some("watch"), Some("watch+steer"))
    );
}

#[test]
fn revoke_self_records_by_device() {
    let scratch = Scratch::new();
    let window = Window::new(&scratch, INSTANCE_A);
    let mut client = window.connect();
    let covered = Rc::new(Cell::new(false));
    let _gate = recording_gate(&window.handle, &[1], covered);
    let (device, token) = window.enroll_with_yes(&mut client, "Pixel", "watch");
    revoke_self(&mut client, &device, "wrong");
    window.result(&mut client, "the refusal");
    assert!(
        of_event(&audit_records(&window, &scratch), "revoke").is_empty(),
        "a wrong token revokes nothing"
    );
    revoke_self(&mut client, &device, &token);
    assert_eq!(window.result(&mut client, "the revocation")["ok"], true);
    let records = audit_records(&window, &scratch);
    let revoked = of_event(&records, "revoke");
    assert_eq!(revoked.len(), 1);
    assert_eq!(
        (
            audit_field(revoked[0], "device"),
            audit_field(revoked[0], "device_name"),
            audit_field(revoked[0], "by"),
        ),
        (Some(device.as_str()), Some("Pixel"), Some("device"))
    );
    assert!(!revoked[0].line.contains(&token), "never the token");
}

impl Window {
    /// A page kind from `from`, its request id `request`, with `fields` (a JSON object's inside, may be empty).
    fn page(&self, from: ViewId, kind: &str, request: &str, fields: &str) {
        let comma = if fields.is_empty() { "" } else { "," };
        self.handle.page_message(
            from,
            &format!(r#"{{"type":"{kind}","request_id":"{request}"{comma}{fields}}}"#),
        );
    }

    /// The device list the base page was sent after `after` of them.
    fn listed(&self, after: usize) -> Vec<serde_json::Value> {
        let list = self.asked("remote_devices", after);
        list["devices"].as_array().expect("a list of devices").clone()
    }

    /// Asks the base page to revoke `device` through the list's `x`, and says yes; the question it was shown.
    fn revoke_from_the_list(&self, device: &str) -> serde_json::Value {
        let before = self.count("remote_revoke_ask");
        self.page(
            ViewId::BASE,
            "remote_revoke_ask",
            "x",
            &format!(r#""device":"{device}""#),
        );
        let asked = self.asked("remote_revoke_ask", before);
        let nonce = asked["nonce"].as_u64().unwrap();
        self.page(
            ViewId::BASE,
            "remote_revoke",
            "y",
            &format!(r#""device":"{device}","nonce":{nonce}"#),
        );
        asked
    }
}

#[test]
fn start_enroll_only_from_the_base_page() {
    let scratch = Scratch::new();
    let window = Window::new(&scratch, INSTANCE_A);
    window.turn();
    window.page(view(2), "remote_start_enroll", "s2", r#""recipe":"auto""#);
    window.page(view(2), "remote_devices_open", "o2", "");
    for request in ["s2", "o2"] {
        assert_eq!(
            result_for(&window.second.sent.borrow(), request),
            Some(Err(crate::agent_panel::remote_enroll::REMOTE_IS_THE_BASE_PAGES.into())),
            "{request}"
        );
    }
    window.settle();
    assert_eq!(window.count("remote_pairing"), 0, "another page makes no code");

    window.page(ViewId::BASE, "remote_start_enroll", "s1", r#""recipe":"direct""#);
    assert_eq!(result_for(&window.base.sent.borrow(), "s1"), Some(Ok(())));
    let shown = window.asked("remote_pairing", 0);
    let code = shown["code"].as_str().unwrap().to_owned();
    assert!(shown["uri"].as_str().unwrap().contains("m=direct"), "{shown}");
    let offered = of_event(&audit_records(&window, &scratch), "enroll_offered").len();
    assert_eq!(offered, 1);

    // The code is the one `add-device` would show: a phone presenting it still waits for the desk's yes.
    let mut client = window.connect();
    Window::enroll(&mut client, &code, "Pixel");
    let asked = window.asked("remote_enroll", 0);
    assert_eq!(asked["name"], "Pixel");
    assert!(
        devices_in(&scratch.store()).is_empty(),
        "nothing is stored before the desk's yes"
    );

    // A recipe this window has no route for is said in the base page, and no code is made.
    let notices = window.count("notice");
    window.page(ViewId::BASE, "remote_start_enroll", "s3", r#""recipe":"ssh""#);
    window.asked("notice", notices);
    assert_eq!(window.count("remote_pairing"), 1);
}

#[test]
fn desk_revoke_is_recorded_by_desk() {
    let scratch = Scratch::new();
    let window = Window::new(&scratch, INSTANCE_A);
    let mut client = window.connect();
    let _gate = recording_gate(&window.handle, &[1], Rc::new(Cell::new(false)));
    let (device, token) = window.enroll_with_yes(&mut client, "Pixel", "watch");
    let asked = window.revoke_from_the_list(&device);
    assert_eq!(asked["name"], "Pixel");
    window.turn_until("the device leaves the store", || {
        stored(&scratch.store(), &device).is_none()
    });
    let records = audit_records(&window, &scratch);
    let revoked = of_event(&records, "revoke");
    assert_eq!(revoked.len(), 1);
    assert_eq!(
        (
            audit_field(revoked[0], "device"),
            audit_field(revoked[0], "device_name"),
            audit_field(revoked[0], "by"),
        ),
        (Some(device.as_str()), Some("Pixel"), Some("desk"))
    );
    assert!(!revoked[0].line.contains(&token), "never the token");
}

#[test]
fn revoke_from_the_list_closes_the_phone() {
    let scratch = Scratch::new();
    let window = Window::new(&scratch, INSTANCE_A);
    let mut client = window.connect();
    let covered = Rc::new(Cell::new(true));
    let _gate = recording_gate(&window.handle, &[1], covered.clone());
    covered.set(false);
    let (device, token) = window.enroll_with_yes(&mut client, "Pixel", "watch+steer");
    let (other, _) = window.enroll_with_yes(&mut client, "Tablet", "watch");
    let lists = window.count("remote_devices");

    // A covered chat's yes takes nothing out.
    covered.set(true);
    window.revoke_from_the_list(&device);
    window.settle();
    assert!(stored(&scratch.store(), &device).is_some());
    covered.set(false);

    window.revoke_from_the_list(&device);
    window.turn_until("the device leaves the store", || {
        stored(&scratch.store(), &device).is_none()
    });
    // Taken out through the store's update: the token no longer names a device, which is what `serve` watches for.
    let store = DeviceStore::load(&scratch.store()).unwrap();
    assert!(store.verify(&device, &token).is_none());
    assert!(store.get(&other).is_some(), "only that device goes");
    // The list is drawn again without it.
    let after = window.listed(lists);
    let ids: Vec<&str> = after.iter().map(|d| d["id"].as_str().unwrap()).collect();
    assert_eq!(ids, [other.as_str()]);
}

#[test]
fn device_list_reads_off_the_tick() {
    let scratch = Scratch::new();
    let window = Window::new(&scratch, INSTANCE_A);
    let mut client = window.connect();
    let _gate = recording_gate(&window.handle, &[1], Rc::new(Cell::new(false)));
    let (device, _token) = window.enroll_with_yes(&mut client, "Pixel\u{202e}9", "watch");
    report(&mut client, &[(&device, "desk.tail1.ts.net")]);
    window.turn_until("the report is shown", || window.count("remote_connected") >= 1);
    window.socket.audit().flush();

    let (release, held) = std::sync::mpsc::channel::<()>();
    let held = std::sync::Mutex::new(held);
    let reading = Arc::new(AtomicBool::new(false));
    let read = Arc::clone(&reading);
    window.socket.set_list_reader(move |store| {
        read.store(true, Ordering::SeqCst);
        let _ = held.lock().unwrap().recv();
        device_rows(store)
    });
    window.page(ViewId::BASE, "remote_devices_open", "o1", "");
    assert_eq!(result_for(&window.base.sent.borrow(), "o1"), Some(Ok(())));
    window.turn_until("the read starts", || reading.load(Ordering::SeqCst));
    // The read is held on its own thread: the tick goes on and is not answered yet.
    for _ in 0..5 {
        window.turn();
    }
    assert_eq!(window.count("remote_devices"), 0);
    release.send(()).unwrap();
    let devices = window.listed(0);
    assert_eq!(devices.len(), 1);
    let row = &devices[0];
    assert_eq!(row["id"], device.as_str());
    assert_eq!(row["name"], "Pixel\u{202e}9", "the name as stored; the page escapes it");
    assert_eq!(
        (row["grant"].as_str(), row["recipe"].as_str()),
        (Some("watch"), Some("direct"))
    );
    assert_eq!(row["connected"], true);
    assert_eq!(row["last_host"], "desk.tail1.ts.net");
    assert!(row["enrolled_at"].as_u64().is_some());
    assert!(row["push_host"].is_null());
    let events: Vec<&str> = row["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["event"].as_str().unwrap())
        .collect();
    assert_eq!(events.first(), Some(&"enroll_confirmed"), "newest first: {events:?}");
    assert!(events.len() <= DEVICE_RECORDS);
    let fields = row["records"][0]["fields"].as_array().unwrap();
    assert!(
        fields.iter().any(|f| f[0] == "device_name" && f[1] == "Pixel\u{202e}9"),
        "{fields:?}"
    );
}

#[test]
fn device_list_reads_run_one_at_a_time_and_coalesce() {
    let scratch = Scratch::new();
    let window = Window::new(&scratch, INSTANCE_A);
    let mut client = window.connect();
    let _gate = recording_gate(&window.handle, &[1], Rc::new(Cell::new(false)));
    let (device, _token) = window.enroll_with_yes(&mut client, "Pixel", "watch");

    let (release, held) = std::sync::mpsc::channel::<()>();
    let held = std::sync::Mutex::new(held);
    let at_once = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let most = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (now, top, count) = (Arc::clone(&at_once), Arc::clone(&most), Arc::clone(&reads));
    window.socket.set_list_reader(move |store| {
        top.fetch_max(now.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
        count.fetch_add(1, Ordering::SeqCst);
        let _ = held.lock().unwrap().recv();
        now.fetch_sub(1, Ordering::SeqCst);
        device_rows(store)
    });
    // `r` held down: one request after another while the first read is still held.
    for n in 0..24 {
        window.page(ViewId::BASE, "remote_devices_open", &format!("o{n}"), "");
        window.turn();
    }
    window.turn_until("the first read starts", || reads.load(Ordering::SeqCst) >= 1);
    for _ in 0..5 {
        window.turn();
    }
    assert_eq!(most.load(Ordering::SeqCst), 1, "one read at a time");
    assert_eq!(reads.load(Ordering::SeqCst), 1, "the others wait for it");
    drop(release);
    let devices = window.listed(0);
    assert_eq!(devices[0]["id"], device.as_str());
    window.settle();
    assert_eq!(
        reads.load(Ordering::SeqCst),
        2,
        "the requests made during the read are answered by one more"
    );
    assert_eq!(window.count("remote_devices"), 1, "and only the newer list is drawn");
    assert_eq!(most.load(Ordering::SeqCst), 1);
}

#[test]
fn a_device_shows_its_last_twenty_records_and_the_push_host_only() {
    let scratch = Scratch::new();
    let window = Window::new(&scratch, INSTANCE_A);
    let mut client = window.connect();
    let _gate = recording_gate(&window.handle, &[1], Rc::new(Cell::new(false)));
    let (device, token) = window.enroll_with_yes(&mut client, "Pixel", "watch");
    set_push(
        &mut client,
        &device,
        &token,
        "https://ntfy.example.net/upAbCdEfSecretTopic?up=1",
    );
    assert_eq!(window.result(&mut client, "the push endpoint")["ok"], true);
    for n in 0..(DEVICE_RECORDS + 5) {
        window
            .socket
            .audit()
            .record(eitri_core::remote_audit::Event::GrantChanged {
                device: eitri_core::remote_audit::DeviceRef {
                    id: device.clone(),
                    name: format!("Pixel {n}"),
                },
                from: "watch".into(),
                to: "watch".into(),
            });
    }
    window.socket.audit().flush();
    window.page(ViewId::BASE, "remote_devices_open", "o1", "");
    let devices = window.listed(0);
    let row = &devices[0];
    assert_eq!(row["push_host"], "ntfy.example.net");
    assert!(
        !row.to_string().contains("SecretTopic"),
        "the endpoint's path can be a credential"
    );
    let records = row["records"].as_array().unwrap();
    assert_eq!(records.len(), DEVICE_RECORDS);
    let newest = &records[0]["fields"];
    assert!(newest
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f[1] == format!("Pixel {}", DEVICE_RECORDS + 4)));
}
