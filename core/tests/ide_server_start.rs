//! `IdeServer` whole: the real keeper, judge, worker and mention socket over real loopback sockets, with a client that
//! talks the way the CLI does and the test in the place of the main context. Every home is a value under a scratch
//! directory, handed in, so no real `~/.claude` is ever looked at.
#![cfg(target_os = "linux")]

#[path = "support/ide_client.rs"]
mod ide_client;

use std::cell::RefCell;
use std::fs;
use std::io::{self, ErrorKind};
use std::net::{Ipv4Addr, TcpStream};
use std::os::fd::RawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eitri_core::editor_rpc::EditorRpc;
use eitri_core::ide_server::channels::Wake;
use eitri_core::ide_server::handshake::getrandom_fill;
use eitri_core::ide_server::host::IdeHost;
use eitri_core::ide_server::lock::{NOT_PRIVATE, NO_HOME};
use eitri_core::ide_server::server::IdeServer;
use eitri_core::ide_server::view::{IDE_DIFF_OPEN, IDE_TAKE_EVENTS};
use eitri_core::ide_server::{Event, Housekeeping, Limits, RpcId, ServerConfig, Started, IDE_NAME};
use eitri_core::nvim_rpc::{Answer, Pending};
use ide_client::{IdeClient, Received, Upgrade, WAIT};
use rmpv::Value;
use serde_json::{json, Value as Json};

/// A bound nothing here should come near: a `stop` that used it would fail the guard in [`stops_in_time`].
const FAR_OFF: Duration = Duration::from_secs(3600);
/// Shorter than any keeper the tests hold.
const SHORT: Duration = Duration::from_millis(20);
const LISTEN: &str = "0A";

/// A project, a `HOME` beside it and a directory outside both.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new() -> Scratch {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "ide-start-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        for name in ["project/src", "home", "elsewhere"] {
            fs::create_dir_all(dir.join(name)).unwrap();
        }
        let dir = dir.canonicalize().unwrap();
        // Whatever the umask: a root others can write to is no boundary.
        fs::set_permissions(dir.join("project"), fs::Permissions::from_mode(0o700)).unwrap();
        Scratch { dir }
    }

    fn root(&self) -> PathBuf {
        self.dir.join("project")
    }

    fn home(&self) -> PathBuf {
        self.dir.join("home")
    }

    fn claude(&self) -> PathBuf {
        self.home().join(".claude")
    }

    fn lock(&self, port: u16) -> PathBuf {
        self.claude().join("ide").join(format!("{port}.lock"))
    }

    fn lock_json(&self, port: u16) -> Json {
        serde_json::from_slice(&fs::read(self.lock(port)).unwrap()).expect("the lock is JSON")
    }

    fn token(&self, port: u16) -> String {
        self.lock_json(port)["authToken"]
            .as_str()
            .expect("the lock has a token")
            .to_owned()
    }

    fn config(&self) -> ServerConfig {
        ServerConfig {
            root: self.root(),
            home: Some(self.home()),
        }
    }

    fn started(&self) -> (IdeServer, u16) {
        let server = IdeServer::start(self.config()).expect("a loopback port can be had");
        let port = attached(&server);
        (server, port)
    }

    fn write(&self, relative: &str, text: &str) -> String {
        let path = self.root().join(relative);
        fs::write(&path, text).unwrap();
        path.to_str().unwrap().to_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // What a failed test left stays only until the next clean of the target directory.
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A keeper whose `start` waits for the test, as one on a slow home would. A closed channel lets a worker the test has
/// given up on run to its end.
struct HeldKeeper(Receiver<()>);

impl Housekeeping for HeldKeeper {
    fn start(&mut self, _port: u16, _token: &str) -> Result<(), String> {
        let _ = self.0.recv();
        Ok(())
    }

    fn watch_fd(&self) -> Option<RawFd> {
        None
    }

    fn on_wake(&mut self, _now: Instant) {}

    fn retry_in(&self, _now: Instant) -> Option<Duration> {
        None
    }

    fn finish(&mut self) {}
}

/// A keeper that takes the worker down when its descriptor wakes.
struct DyingKeeper(Arc<Wake>);

impl Housekeeping for DyingKeeper {
    fn start(&mut self, _port: u16, _token: &str) -> Result<(), String> {
        Ok(())
    }

    fn watch_fd(&self) -> Option<RawFd> {
        Some(self.0.fd())
    }

    fn on_wake(&mut self, _now: Instant) {
        panic!("the keeper was told to fail");
    }

    fn retry_in(&self, _now: Instant) -> Option<Duration> {
        None
    }

    fn finish(&mut self) {}
}

/// An editor the test answers by hand.
#[derive(Default)]
struct Editor {
    calls: RefCell<Vec<(&'static str, Answer)>>,
}

impl Editor {
    fn answer(&self, code: &str, reply: Value) {
        let mut calls = self.calls.borrow_mut();
        let at = calls
            .iter()
            .position(|(asked, _)| *asked == code)
            .expect("that chunk is out");
        calls.remove(at).1.send(Ok(reply));
    }
}

impl EditorRpc for Editor {
    fn exec_lua(&self, code: &'static str, _args: Vec<Value>) -> Pending {
        let (answer, pending) = Pending::pair();
        self.calls.borrow_mut().push((code, answer));
        pending
    }

    fn target(&self) -> Option<u64> {
        Some(1)
    }
}

fn attached(server: &IdeServer) -> u16 {
    match server.first_answer(WAIT) {
        Some(Started::Attached { port }) => port,
        other => panic!("not attached: {other:?}"),
    }
}

fn refused(server: &IdeServer) -> String {
    match server.first_answer(WAIT) {
        Some(Started::NotAttached { why }) => why,
        other => panic!("not refused: {other:?}"),
    }
}

fn await_wake(fd: RawFd) {
    let mut fds = [libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }];
    // SAFETY: `fds` is a valid array of one entry for the call.
    let ready = unsafe { libc::poll(fds.as_mut_ptr(), 1, WAIT.as_millis() as libc::c_int) };
    assert_eq!(ready, 1, "no wake in time");
}

/// Everything drained up to and including the wake that brought what `wanted` picks.
fn events_until(server: &IdeServer, wanted: impl Fn(&Event) -> bool) -> Vec<Event> {
    let mut seen = Vec::new();
    loop {
        await_wake(server.wake_fd());
        seen.extend(server.drain());
        if seen.iter().any(&wanted) {
            return seen;
        }
    }
}

/// Fails rather than hangs on a `stop` that does not return.
fn stops_in_time(server: IdeServer, within: Duration) {
    let (done, finished) = channel();
    std::thread::spawn(move || {
        server.stop(within);
        let _ = done.send(());
    });
    finished.recv_timeout(WAIT).expect("stop returns");
}

fn assert_no_listener(port: u16) {
    let error = TcpStream::connect(("127.0.0.1", port)).expect_err("nothing listens there");
    assert_eq!(error.kind(), ErrorKind::ConnectionRefused);
}

/// The local address of every listening socket on `port`, in the form `/proc/net` writes it.
fn listening_on(port: u16) -> Vec<String> {
    let mut found = Vec::new();
    for table in ["/proc/self/net/tcp", "/proc/self/net/tcp6"] {
        // A kernel with no IPv6 has no second table.
        let text = fs::read_to_string(table).unwrap_or_default();
        for row in text.lines().skip(1) {
            let fields: Vec<&str> = row.split_whitespace().collect();
            if fields[3] == LISTEN && fields[1].ends_with(&format!(":{port:04X}")) {
                found.push(fields[1].to_owned());
            }
        }
    }
    found
}

fn thread_names() -> Vec<String> {
    fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|task| fs::read_to_string(task.unwrap().path().join("comm")).ok())
        .map(|name| name.trim_end().to_owned())
        .collect()
}

fn map(fields: Vec<(&str, Value)>) -> Value {
    Value::Map(
        fields
            .into_iter()
            .map(|(name, value)| (Value::from(name), value))
            .collect(),
    )
}

#[test]
fn a_failing_random_source_means_no_listener_no_lock_and_no_port() {
    let scratch = Scratch::new();
    let server = IdeServer::start_with(scratch.config(), Limits::default(), |_| {
        Err(io::Error::other("no random bytes"))
    });
    assert!(server.is_none(), "no token, no listener");
    assert!(!scratch.claude().exists());
}

#[test]
fn a_relative_home_means_no_listener_and_says_why() {
    let scratch = Scratch::new();
    let relative = PathBuf::from("ide-server-start-relative-home");
    for home in [Some(relative.clone()), None] {
        let server = IdeServer::start(ServerConfig {
            root: scratch.root(),
            home,
        })
        .expect("the bind does not depend on the home");
        let port = server.port();

        assert_eq!(refused(&server), NO_HOME);

        stops_in_time(server, FAR_OFF);
        assert_no_listener(port);
    }
    assert!(!relative.exists());
    assert!(!scratch.claude().exists());
}

#[test]
fn an_unsafe_directory_means_no_listener() {
    let scratch = Scratch::new();
    fs::create_dir(scratch.claude()).unwrap();
    fs::set_permissions(scratch.claude(), fs::Permissions::from_mode(0o777)).unwrap();
    let server = IdeServer::start(scratch.config()).unwrap();
    let port = server.port();

    assert_eq!(refused(&server), NOT_PRIVATE);

    stops_in_time(server, FAR_OFF);
    assert_no_listener(port);
    assert!(!scratch.claude().join("ide").exists());
    assert_eq!(
        fs::metadata(scratch.claude()).unwrap().mode() & 0o7777,
        0o777,
        "a directory that is the CLI's keeps its mode"
    );
}

#[test]
fn the_lock_names_the_port_and_a_real_client_attaches() {
    let scratch = Scratch::new();
    let (server, port) = scratch.started();

    assert_eq!(port, server.port());
    let lock = scratch.lock_json(port);
    assert_eq!(lock["pid"], std::process::id());
    assert_eq!(lock["ideName"], IDE_NAME);

    let mut client = IdeClient::attach(port, lock["authToken"].as_str().unwrap());
    client.request(1, "initialize", json!({"protocolVersion": "2025-06-18"}));
    let answer = client.read_message();
    assert_eq!(answer["id"], 1);
    assert_eq!(answer["result"]["serverInfo"]["name"], IDE_NAME);

    stops_in_time(server, FAR_OFF);
}

/// The worker hands the window no more than it has room for, and only a drain makes room, so a drain has to say so.
#[test]
fn a_drain_gives_the_worker_its_room_back() {
    let scratch = Scratch::new();
    let server = IdeServer::start_with(
        scratch.config(),
        Limits {
            authed: 1,
            jobs_per_conn: 3,
            ..Limits::default()
        },
        getrandom_fill,
    )
    .expect("a listener");
    let port = attached(&server);
    let lock = scratch.lock_json(port);
    let mut client = IdeClient::attach(port, lock["authToken"].as_str().unwrap());

    // The first connection's mention socket is one event, and that leaves room for two calls.
    for id in 1..=3 {
        client.call_tool(id, "getDiagnostics", json!({}));
    }
    assert_eq!(
        client.read_message()["id"],
        3,
        "past the room this call is refused here"
    );
    let seen = events_until(
        &server,
        |event| matches!(event, Event::Diagnostics { id, .. } if *id == RpcId::Num(2)),
    );
    assert_eq!(
        seen.iter()
            .filter(|event| matches!(event, Event::Diagnostics { .. }))
            .count(),
        2
    );

    client.call_tool(4, "getDiagnostics", json!({}));
    events_until(
        &server,
        |event| matches!(event, Event::Diagnostics { id, .. } if *id == RpcId::Num(4)),
    );
    stops_in_time(server, FAR_OFF);
}

#[test]
fn the_listener_is_bound_to_loopback_only() {
    let scratch = Scratch::new();
    let (server, port) = scratch.started();

    let loopback = u32::from_ne_bytes(Ipv4Addr::LOCALHOST.octets());
    assert_eq!(listening_on(port), [format!("{loopback:08X}:{port:04X}")]);

    stops_in_time(server, FAR_OFF);
}

#[test]
fn the_threads_have_their_names() {
    let scratch = Scratch::new();
    let (server, _) = scratch.started();

    let names = thread_names();
    assert!(names.iter().any(|name| name == "ide-worker"), "{names:?}");
    assert!(names.iter().any(|name| name == "ide-judge"), "{names:?}");

    stops_in_time(server, FAR_OFF);
}

#[test]
fn the_token_in_the_lock_is_the_one_checked() {
    let scratch = Scratch::new();
    let (server, port) = scratch.started();
    let token = scratch.token(port);

    let wrong = "ab".repeat(32);
    assert_ne!(token, wrong);
    let mut stranger = IdeClient::try_attach(port, &Upgrade::proper(port, &wrong));
    let head = stranger.read_head();
    assert!(head.starts_with("HTTP/1.1 401 "), "{head}");

    let mut client = IdeClient::attach(port, &token);
    client.round_trip();

    stops_in_time(server, FAR_OFF);
}

#[test]
fn a_new_listener_has_a_new_token() {
    let scratch = Scratch::new();
    let (first, first_port) = scratch.started();
    let (second, second_port) = scratch.started();

    let (one, other) = (scratch.token(first_port), scratch.token(second_port));
    assert_eq!(one.len(), 64);
    assert_eq!(other.len(), 64);
    assert_ne!(one, other);

    stops_in_time(first, FAR_OFF);
    stops_in_time(second, FAR_OFF);
}

#[test]
fn first_answer_is_bounded_and_arrives_later_through_drain() {
    let scratch = Scratch::new();
    let (release, held) = channel();
    let keeper = Box::new(HeldKeeper(held));
    let server = IdeServer::start_with_keeper(scratch.config(), Limits::default(), getrandom_fill, keeper).unwrap();

    assert_eq!(server.first_answer(SHORT), None, "the keeper is still held");

    release.send(()).unwrap();
    await_wake(server.wake_fd());
    assert_eq!(server.first_answer(WAIT), None, "only the first call waits or answers");

    let events = server.drain();
    assert!(
        matches!(events.as_slice(), [Event::Started(Started::Attached { port })] if *port == server.port()),
        "{events:?}"
    );
    stops_in_time(server, FAR_OFF);
}

#[test]
fn an_answer_first_answer_gave_does_not_come_again() {
    let scratch = Scratch::new();
    let (server, _) = scratch.started();

    let events = server.drain();
    assert!(events.is_empty(), "{events:?}");

    stops_in_time(server, FAR_OFF);
}

#[test]
fn first_answer_after_a_drain_leaves_the_events_behind_it() {
    let scratch = Scratch::new();
    let server = IdeServer::start(scratch.config()).unwrap();
    let port = server.port();
    let _ = events_until(&server, |event| matches!(event, Event::Started(_)));

    let _client = IdeClient::attach(port, &scratch.token(port));
    await_wake(server.wake_fd());
    assert_eq!(
        server.first_answer(WAIT),
        None,
        "a drain has seen what it would wait for"
    );

    let events = server.drain();
    assert!(matches!(events.as_slice(), [Event::MentionUp { .. }]), "{events:?}");
    stops_in_time(server, FAR_OFF);
}

#[test]
fn stop_after_the_worker_died_returns_at_once() {
    let scratch = Scratch::new();
    fs::create_dir(scratch.claude()).unwrap();
    fs::set_permissions(scratch.claude(), fs::Permissions::from_mode(0o777)).unwrap();
    let server = IdeServer::start(scratch.config()).unwrap();
    assert_eq!(refused(&server), NOT_PRIVATE);

    stops_in_time(server, FAR_OFF);
}

#[test]
fn a_worker_that_dies_serving_says_lost_and_stop_returns_at_once() {
    let scratch = Scratch::new();
    let watch = Arc::new(Wake::new().unwrap());
    let keeper = Box::new(DyingKeeper(Arc::clone(&watch)));
    let server = IdeServer::start_with_keeper(scratch.config(), Limits::default(), getrandom_fill, keeper).unwrap();
    let port = attached(&server);

    watch.signal();
    let events = events_until(&server, |event| matches!(event, Event::Lost));
    assert!(matches!(events.last(), Some(Event::Lost)), "{events:?}");

    stops_in_time(server, FAR_OFF);
    assert_no_listener(port);
}

#[test]
fn stop_gives_up_on_a_worker_stuck_past_its_bound() {
    let scratch = Scratch::new();
    let (release, held) = channel();
    let keeper = Box::new(HeldKeeper(held));
    let server = IdeServer::start_with_keeper(scratch.config(), Limits::default(), getrandom_fill, keeper).unwrap();

    stops_in_time(server, SHORT);

    drop(release);
}

#[test]
fn a_dropped_server_stops_its_worker() {
    let scratch = Scratch::new();
    let (server, port) = scratch.started();
    let lock = scratch.lock(port);
    assert!(lock.exists());

    drop(server);

    let waited = Instant::now();
    while lock.exists() {
        assert!(waited.elapsed() < WAIT, "the lock outlived its server");
        std::thread::sleep(SHORT);
    }
}

#[test]
fn two_windows_of_one_project_have_two_locks_and_two_ports() {
    let scratch = Scratch::new();
    let (first, first_port) = scratch.started();
    let (second, second_port) = scratch.started();

    assert_ne!(first_port, second_port);
    let mut first_client = IdeClient::attach(first_port, &scratch.token(first_port));
    let mut second_client = IdeClient::attach(second_port, &scratch.token(second_port));
    first_client.round_trip();
    second_client.round_trip();

    stops_in_time(first, FAR_OFF);
    assert!(!scratch.lock(first_port).exists());
    assert!(scratch.lock(second_port).exists());
    second_client.round_trip();

    stops_in_time(second, FAR_OFF);
}

#[test]
fn stop_removes_the_lock_and_connected_clients_see_the_end() {
    let scratch = Scratch::new();
    let (server, port) = scratch.started();
    let mut client = IdeClient::attach(port, &scratch.token(port));
    client.round_trip();

    stops_in_time(server, FAR_OFF);

    assert!(!scratch.lock(port).exists());
    assert_eq!(client.read_frame(), Received::Gone);
    assert_no_listener(port);
}

#[test]
fn stop_leaves_a_lock_that_was_replaced() {
    let scratch = Scratch::new();
    let (server, port) = scratch.started();
    let lock = scratch.lock(port);
    let other = lock.with_extension("other");
    fs::write(&other, "someone else's").unwrap();
    fs::rename(&other, &lock).unwrap();

    stops_in_time(server, FAR_OFF);

    assert_eq!(fs::read_to_string(&lock).unwrap(), "someone else's");
}

#[test]
fn an_open_diff_reaches_the_host_and_the_answer_reaches_the_client() {
    let scratch = Scratch::new();
    let path = scratch.write("src/a.rs", "old\n");
    let (server, port) = scratch.started();
    let mut client = IdeClient::attach(port, &scratch.token(port));
    let mut host = IdeHost::new(scratch.root());
    let editor = Editor::default();
    let now = Instant::now();
    let mut out = Vec::new();

    client.call_tool(
        7,
        "openDiff",
        json!({"old_file_path": path, "new_file_path": path, "new_file_contents": "new\n", "tab_name": "t"}),
    );
    for event in events_until(&server, |event| matches!(event, Event::OpenDiff { .. })) {
        host.on_event(event, &editor, now, &mut out);
    }
    assert!(out.is_empty(), "{out:?}");

    editor.answer(IDE_DIFF_OPEN, map(vec![("opened", Value::from(true))]));
    host.tick(&editor, now, &mut out);
    let accepted = map(vec![
        ("view", Value::from(1)),
        ("kind", Value::from("accept")),
        (
            "lines",
            Value::Array(vec![Value::Binary(b"new".to_vec()), Value::Binary(b"edited".to_vec())]),
        ),
    ]);
    editor.answer(IDE_TAKE_EVENTS, Value::Array(vec![accepted]));
    host.tick(&editor, now, &mut out);
    assert_eq!(out.len(), 1, "{out:?}");
    server.send(out);

    let answer = client.read_message();
    assert_eq!(answer["id"], 7);
    assert_eq!(answer["result"]["content"][0]["text"], "FILE_SAVED");
    assert_eq!(answer["result"]["content"][1]["text"], "new\nedited\n");

    stops_in_time(server, FAR_OFF);
}

#[test]
fn get_diagnostics_outside_never_reaches_the_host() {
    let scratch = Scratch::new();
    let inside = scratch.write("src/a.rs", "text\n");
    let (server, port) = scratch.started();
    let mut client = IdeClient::attach(port, &scratch.token(port));

    let elsewhere = scratch.dir.join("elsewhere/a.rs");
    let outside = format!("file://{}", elsewhere.display());
    client.call_tool(8, "getDiagnostics", json!({"uri": outside}));
    let answer = client.read_message();
    assert_eq!(answer["id"], 8);
    assert_eq!(answer["result"]["content"][0]["text"], "[]");

    // Behind the one above on the one judge, so anything the first had sent the host is here too.
    client.call_tool(9, "getDiagnostics", json!({"uri": format!("file://{inside}")}));
    let events = events_until(&server, |event| matches!(event, Event::Diagnostics { .. }));
    let asked: Vec<&Event> = events
        .iter()
        .filter(|event| matches!(event, Event::Diagnostics { .. }))
        .collect();
    assert_eq!(asked.len(), 1, "{asked:?}");

    stops_in_time(server, FAR_OFF);
}

#[test]
fn the_first_authenticated_client_brings_mention_up_and_the_last_takes_it_down() {
    let scratch = Scratch::new();
    let (server, port) = scratch.started();
    let token = scratch.token(port);

    let mut stranger = IdeClient::try_attach(port, &Upgrade::proper(port, "not the token"));
    assert!(stranger.read_head().starts_with("HTTP/1.1 401 "));
    assert!(server.drain().is_empty(), "a refused client brings nothing up");

    let mut first = IdeClient::attach(port, &token);
    let events = events_until(&server, |event| matches!(event, Event::MentionUp { .. }));
    let socket = match events.as_slice() {
        [Event::MentionUp { socket }] => socket.clone(),
        other => panic!("{other:?}"),
    };
    assert!(socket.exists());

    let mut second = IdeClient::attach(port, &token);
    second.round_trip();
    assert!(server.drain().is_empty(), "the socket is already up");

    first.close(1000);
    first.expect_closed_with(1000);
    let events = events_until(&server, |event| matches!(event, Event::ClientGone { .. }));
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(socket.exists(), "one client is still connected");

    second.close(1000);
    second.expect_closed_with(1000);
    let events = events_until(&server, |event| matches!(event, Event::MentionDown));
    assert!(matches!(events.last(), Some(Event::MentionDown)), "{events:?}");
    assert!(!socket.exists());

    stops_in_time(server, FAR_OFF);
}
