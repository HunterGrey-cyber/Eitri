//! The listener's poll loop over real loopback sockets: a thread running `run_on`, the masked client of
//! `support/ide_client.rs`, and the test itself in the place of the judge, the host, the keeper and the mention socket.
#![cfg(target_os = "linux")]

#[path = "support/ide_client.rs"]
mod ide_client;

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use eitri_core::editor_context::feed::PushFacts;
use eitri_core::editor_tools::tools::diagnostics::DiagnosticsAnswer;
use eitri_core::ide_server::channels::Wake;
use eitri_core::ide_server::handshake::Token;
use eitri_core::ide_server::lines::LineShape;
use eitri_core::ide_server::worker::{run_on, Syscalls, System, WorkerParts};
use eitri_core::ide_server::{
    Command, ConnId, ContextUpdate, DiagnosticsQuery, DiffRequest, Done, Event, Housekeeping, Job, Limits, MentionLine,
    MentionPort, RpcId, Started, ToolAnswer,
};
use serde_json::{json, Value};

use ide_client::{
    announcing, dial, empty_pings, ping_frame, read_to_end, unmasked_frame, IdeClient, Received, Upgrade, WAIT,
};

const MIB: usize = 1024 * 1024;
const TOOL_ERROR: i64 = -32000;
/// A short wait for something that must not come. A slow machine can only make these tests pass more easily, and each
/// has a positive check beside it.
const QUIET: Duration = Duration::from_millis(100);

/// Built at run time, so no such literal sits in the tree.
fn token_text() -> String {
    "ab".repeat(32)
}

#[derive(Default)]
struct KeeperState {
    refuse: Option<String>,
    panic_in_start: bool,
    panic_in_finish: bool,
    dial_on_finish: bool,
    watch: Option<Wake>,
    port: Mutex<Option<u16>>,
    dialled: Mutex<Option<bool>>,
    retry_at: Mutex<Option<Instant>>,
    finishes: AtomicUsize,
    wakes: AtomicUsize,
}

struct StubKeeper(Arc<KeeperState>);

impl Housekeeping for StubKeeper {
    fn start(&mut self, port: u16, _token: &str) -> Result<(), String> {
        *self.0.port.lock().unwrap() = Some(port);
        assert!(!self.0.panic_in_start, "the keeper was told to fail in start");
        self.0.refuse.clone().map_or(Ok(()), Err)
    }

    fn watch_fd(&self) -> Option<RawFd> {
        self.0.watch.as_ref().map(Wake::fd)
    }

    fn on_wake(&mut self, _now: Instant) {
        if let Some(watch) = &self.0.watch {
            watch.drain();
        }
        *self.0.retry_at.lock().unwrap() = None;
        self.0.wakes.fetch_add(1, Ordering::SeqCst);
    }

    fn retry_in(&self, now: Instant) -> Option<Duration> {
        self.0
            .retry_at
            .lock()
            .unwrap()
            .map(|at| at.saturating_duration_since(now))
    }

    fn finish(&mut self) {
        self.0.finishes.fetch_add(1, Ordering::SeqCst);
        assert!(!self.0.panic_in_finish, "the keeper was told to fail in finish");
        if self.0.dial_on_finish {
            let port = self.0.port.lock().unwrap().expect("started");
            *self.0.dialled.lock().unwrap() = Some(TcpStream::connect(("127.0.0.1", port)).is_ok());
        }
    }
}

struct MentionState {
    socket: Option<PathBuf>,
    readable: Wake,
    lines: Mutex<Vec<MentionLine>>,
    panic_in_poll: AtomicBool,
    opens: AtomicUsize,
    closes: AtomicUsize,
}

impl Default for MentionState {
    fn default() -> Self {
        MentionState {
            socket: None,
            readable: Wake::new().unwrap(),
            lines: Mutex::new(Vec::new()),
            panic_in_poll: AtomicBool::new(false),
            opens: AtomicUsize::new(0),
            closes: AtomicUsize::new(0),
        }
    }
}

struct StubMentions(Arc<MentionState>);

impl MentionPort for StubMentions {
    fn open(&mut self) -> Option<PathBuf> {
        self.0.opens.fetch_add(1, Ordering::SeqCst);
        self.0.socket.clone()
    }

    fn fds(&self) -> Vec<RawFd> {
        vec![self.0.readable.fd()]
    }

    fn poll(&mut self, _now: Instant) -> Vec<MentionLine> {
        self.0.readable.drain();
        assert!(
            !self.0.panic_in_poll.load(Ordering::SeqCst),
            "the mention port was told to fail"
        );
        std::mem::take(&mut *self.0.lines.lock().unwrap())
    }

    fn next_deadline(&self) -> Option<Instant> {
        None
    }

    fn close(&mut self) {
        self.0.closes.fetch_add(1, Ordering::SeqCst);
    }
}

/// Failures to bring about, a count of the loop's turns, and a way to pile things up for one pass. An errno of 0 is the
/// real call.
#[derive(Clone, Default)]
struct Faults {
    accept_errno: Arc<AtomicI32>,
    poll_errno: Arc<AtomicI32>,
    polls: Arc<AtomicUsize>,
    hold: Arc<AtomicBool>,
    held: Arc<AtomicBool>,
}

impl Faults {
    /// Stops the worker before its next `poll`, so that all the test does until [`Faults::release`] lands in one pass.
    fn hold_at_poll(&self, rig: &Rig) {
        self.hold.store(true, Ordering::SeqCst);
        rig.nudge();
        let waited = Instant::now();
        while !self.held.load(Ordering::SeqCst) {
            assert!(waited.elapsed() < WAIT, "the worker never came back to poll");
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn release(&self) {
        self.hold.store(false, Ordering::SeqCst);
    }
}

impl Syscalls for Faults {
    fn poll(&mut self, fds: &mut [libc::pollfd], timeout_ms: libc::c_int) -> io::Result<()> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        // Bounded, so a test that fails before its release still ends.
        let waited = Instant::now();
        while self.hold.load(Ordering::SeqCst) && waited.elapsed() < WAIT {
            self.held.store(true, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(1));
        }
        self.held.store(false, Ordering::SeqCst);
        match self.poll_errno.swap(0, Ordering::SeqCst) {
            0 => System.poll(fds, timeout_ms),
            errno => Err(io::Error::from_raw_os_error(errno)),
        }
    }

    fn accept(&mut self, listener: &TcpListener) -> io::Result<TcpStream> {
        match self.accept_errno.load(Ordering::SeqCst) {
            0 => System.accept(listener),
            errno => Err(io::Error::from_raw_os_error(errno)),
        }
    }
}

/// Keeps the worker's log lines for a test to read.
#[derive(Clone, Default)]
struct KeptLog(Arc<Mutex<Vec<String>>>);

impl KeptLog {
    fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

impl Syscalls for KeptLog {
    fn log(&mut self, line: &str) {
        self.0.lock().unwrap().push(line.to_owned());
    }
}

/// A socket bound on 127.0.0.1 and not listening. `SO_REUSEPORT` lets a second one share the port the first was given.
fn bound_socket(port: u16) -> TcpListener {
    let on: libc::c_int = 1;
    let address = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: port.to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from(Ipv4Addr::LOCALHOST).to_be(),
        },
        sin_zero: [0; 8],
    };
    // SAFETY: the descriptor is new and owned from here on; the option and the address are the sizes given.
    unsafe {
        let raw = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        assert!(raw >= 0, "socket: {}", io::Error::last_os_error());
        let socket = OwnedFd::from_raw_fd(raw);
        let option = (&on as *const libc::c_int).cast();
        let size = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let set = libc::setsockopt(socket.as_raw_fd(), libc::SOL_SOCKET, libc::SO_REUSEPORT, option, size);
        assert_eq!(set, 0, "SO_REUSEPORT: {}", io::Error::last_os_error());
        let name = (&address as *const libc::sockaddr_in).cast();
        let size = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        let bound = libc::bind(socket.as_raw_fd(), name, size);
        assert_eq!(bound, 0, "bind: {}", io::Error::last_os_error());
        TcpListener::from(socket)
    }
}

/// A worker on its thread, with the test holding the other end of each channel.
struct Rig {
    port: u16,
    _port_held: TcpListener,
    token: String,
    keeper: Arc<KeeperState>,
    mentions: Arc<MentionState>,
    events: Receiver<Event>,
    events_waiting: Arc<AtomicUsize>,
    commands: Sender<Command>,
    commands_wake: Arc<Wake>,
    jobs: Option<Receiver<Job>>,
    done: Option<Sender<Done>>,
    done_wake: Arc<Wake>,
    thread: Option<JoinHandle<()>>,
}

impl Rig {
    fn spawn<C: Syscalls + Send + 'static>(
        limits: Limits,
        keeper: KeeperState,
        mentions: MentionState,
        calls: C,
    ) -> Rig {
        // Bound and not listening, this takes no connection, and `bind` to port 0 hands out no port with a socket on it:
        // a dial after the listener closed cannot reach a listener another test was given this port for.
        let port_held = bound_socket(0);
        let port = port_held.local_addr().unwrap().port();
        let listener = bound_socket(port);
        // SAFETY: a valid descriptor, bound above.
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 128) }, 0);
        listener.set_nonblocking(true).unwrap();
        let token = Token::generate(|bytes| {
            bytes.fill(0xab);
            Ok(())
        })
        .unwrap();
        let keeper = Arc::new(keeper);
        let mentions = Arc::new(mentions);
        let (events_in, events) = channel();
        let (commands, commands_out) = channel();
        let (jobs_in, jobs) = channel();
        let (done, done_out) = channel();
        let commands_wake = Arc::new(Wake::new().unwrap());
        let done_wake = Arc::new(Wake::new().unwrap());
        let events_waiting = Arc::new(AtomicUsize::new(0));
        let parts = WorkerParts {
            listener,
            token,
            limits,
            keeper: Box::new(StubKeeper(Arc::clone(&keeper))),
            mentions: Box::new(StubMentions(Arc::clone(&mentions))),
            events: events_in,
            events_wake: Arc::new(Wake::new().unwrap()),
            events_waiting: Arc::clone(&events_waiting),
            commands: commands_out,
            commands_wake: Arc::clone(&commands_wake),
            jobs: jobs_in,
            done: done_out,
            done_wake: Arc::clone(&done_wake),
        };
        Rig {
            port,
            _port_held: port_held,
            token: token_text(),
            keeper,
            mentions,
            events,
            events_waiting,
            commands,
            commands_wake,
            jobs: Some(jobs),
            done: Some(done),
            done_wake,
            thread: Some(thread::spawn(move || run_on(parts, calls))),
        }
    }

    fn started<C: Syscalls + Send + 'static>(
        limits: Limits,
        keeper: KeeperState,
        mentions: MentionState,
        calls: C,
    ) -> Rig {
        let rig = Rig::spawn(limits, keeper, mentions, calls);
        match rig.next_event() {
            Event::Started(Started::Attached { port }) => assert_eq!(port, rig.port),
            other => panic!("the first event was {other:?}"),
        }
        rig
    }

    fn with_limits(limits: Limits) -> Rig {
        Rig::started(limits, KeeperState::default(), MentionState::default(), System)
    }

    fn new() -> Rig {
        Rig::with_limits(Limits::default())
    }

    fn client(&self) -> IdeClient {
        IdeClient::attach(self.port, &self.token)
    }

    /// A client that has told the server it is ready, known to have been heard.
    fn ready_client(&self) -> IdeClient {
        let mut client = self.client();
        client.notify("notifications/initialized", json!({}));
        client.round_trip();
        client
    }

    fn next_event(&self) -> Event {
        let event = self.events.recv_timeout(WAIT).expect("an event in time");
        self.events_waiting.fetch_sub(1, Ordering::Relaxed);
        event
    }

    /// Everything waiting, as a drain takes it.
    fn take_events(&self) -> Vec<Event> {
        let events: Vec<Event> = self.events.try_iter().collect();
        self.events_waiting.fetch_sub(events.len(), Ordering::Relaxed);
        events
    }

    fn no_event(&self) {
        match self.events.recv_timeout(QUIET) {
            Err(RecvTimeoutError::Timeout) => {}
            other => panic!("nothing should have come: {other:?}"),
        }
    }

    fn next_job(&self) -> Job {
        self.jobs.as_ref().unwrap().recv_timeout(WAIT).expect("a job in time")
    }

    fn no_job(&self) {
        assert!(matches!(
            self.jobs.as_ref().unwrap().try_recv(),
            Err(TryRecvError::Empty)
        ));
    }

    fn done(&self, done: Done) {
        self.done.as_ref().unwrap().send(done).unwrap();
        self.done_wake.signal();
    }

    fn command(&self, command: Command) {
        self.commands.send(command).unwrap();
        self.commands_wake.signal();
    }

    /// Which connection a client is: the worker names it on a call that reaches the host.
    fn conn_of(&self, client: &mut IdeClient) -> ConnId {
        client.call_tool(9_000, "getDiagnostics", json!({}));
        match self.next_event() {
            Event::Diagnostics { conn, .. } => conn,
            other => panic!("not the call: {other:?}"),
        }
    }

    /// Gives the worker a turn it has no other reason for.
    fn nudge(&self) {
        self.commands_wake.signal();
    }

    fn join(&mut self) {
        if let Some(thread) = self.thread.take() {
            // A worker told to fail ends in its panic, which is what such a test is about.
            let _ = thread.join();
        }
    }

    fn stop(&mut self) {
        self.command(Command::Stop);
        self.join();
    }

    fn expect_lost(&self) {
        loop {
            match self.next_event() {
                Event::Lost => return,
                Event::ClientGone { .. } => {}
                other => panic!("on the way to Lost: {other:?}"),
            }
        }
    }

    fn assert_port_closed(&self) {
        assert!(
            TcpStream::connect(("127.0.0.1", self.port)).is_err(),
            "the listener is still there"
        );
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if self.thread.is_some() {
            let _ = self.commands.send(Command::Stop);
            self.commands_wake.signal();
            self.join();
        }
    }
}

fn rpc(id: i64) -> RpcId {
    RpcId::Num(id)
}

fn diff_request(tab_name: &str) -> Box<DiffRequest> {
    Box::new(DiffRequest {
        tab_name: tab_name.to_owned(),
        real_path: PathBuf::from("/p/a"),
        rel: "a".to_owned(),
        file_text: Vec::new(),
        file_lines: Vec::new(),
        proposal_lines: Vec::new(),
        shape: LineShape { crlf: false, eol: true },
    })
}

fn no_entries() -> DiagnosticsAnswer {
    DiagnosticsAnswer {
        entries: Vec::new(),
        modified: Vec::new(),
        left_out: 0,
    }
}

fn expect_error_on(answer: &Value, id: i64) {
    assert_eq!(answer["id"], id, "{answer}");
    assert_eq!(answer["error"]["code"], TOOL_ERROR, "{answer}");
}

fn uri_call(client: &mut IdeClient, id: i64) {
    client.call_tool(id, "getDiagnostics", json!({"uri": "file:///p/a"}));
}

fn all_call(client: &mut IdeClient, id: i64) {
    client.call_tool(id, "getDiagnostics", json!({}));
}

// --- The upgrade ---

#[test]
fn a_wrong_host_a_token_and_an_origin_are_refused_end_to_end() {
    let rig = Rig::new();
    let right_host = format!("127.0.0.1:{}", rig.port);
    let wrong_token = "cd".repeat(32);
    let cases = [
        (
            "a wrong host",
            Some("example.com:80".to_owned()),
            Some(rig.token.as_str()),
            None,
            "403",
        ),
        (
            "an origin",
            Some(right_host.clone()),
            Some(rig.token.as_str()),
            Some("http://a.example"),
            "403",
        ),
        ("no token", Some(right_host.clone()), None, None, "401"),
        (
            "a wrong token",
            Some(right_host),
            Some(wrong_token.as_str()),
            None,
            "401",
        ),
    ];
    for (what, host, token, origin, status) in cases {
        let mut stream = dial(rig.port);
        let request = Upgrade { host, token, origin }.text();
        stream.write_all(request.as_bytes()).unwrap();
        let answer = String::from_utf8(read_to_end(&mut stream)).unwrap();
        assert!(answer.starts_with(&format!("HTTP/1.1 {status} ")), "{what}: {answer}");
    }
}

#[test]
fn a_refusal_is_logged_once_for_each_check_and_never_with_the_token() {
    let log = KeptLog::default();
    let rig = Rig::started(
        Limits {
            authed: 1,
            ..Limits::default()
        },
        KeeperState::default(),
        MentionState::default(),
        log.clone(),
    );
    let right_host = format!("127.0.0.1:{}", rig.port);
    let wrong_token = "cd".repeat(32);
    let origin = Upgrade {
        host: Some(right_host.clone()),
        token: Some(rig.token.as_str()),
        origin: Some("http://a.example"),
    };
    let token = Upgrade {
        host: Some(right_host),
        token: Some(wrong_token.as_str()),
        origin: None,
    };
    for upgrade in [&origin, &origin, &token, &token] {
        let mut stream = dial(rig.port);
        stream.write_all(upgrade.text().as_bytes()).unwrap();
        assert!(!read_to_end(&mut stream).is_empty());
    }
    let first = rig.client();
    let mut second = IdeClient::try_attach(rig.port, &Upgrade::proper(rig.port, &rig.token));
    assert!(second.read_head().starts_with("HTTP/1.1 503 "));
    drop(first);

    let lines = log.lines();
    assert_eq!(lines.len(), 3, "{lines:?}");
    for (line, check) in lines.iter().zip(["Origin", "token", "slot"]) {
        assert!(line.starts_with("[ide-server] refused a client: "), "{line}");
        assert!(line.contains(check), "{line}");
        assert!(!line.contains(&rig.token) && !line.contains(&wrong_token), "{line}");
    }
}

#[test]
fn a_head_over_8_kib_is_closed() {
    let rig = Rig::new();
    let mut stream = dial(rig.port);
    let head = format!("GET / HTTP/1.1\r\nX-Pad: {}", "a".repeat(9 * 1024));
    stream.write_all(head.as_bytes()).unwrap();
    let answer = String::from_utf8(read_to_end(&mut stream)).unwrap();
    assert!(answer.starts_with("HTTP/1.1 400 "), "{answer}");
}

#[test]
fn a_head_not_finished_in_time_is_closed() {
    let rig = Rig::with_limits(Limits {
        handshake_within: Duration::from_millis(200),
        ..Limits::default()
    });
    let mut stream = dial(rig.port);
    stream.write_all(b"GET / HTTP/1.1\r\n").unwrap();
    assert_eq!(read_to_end(&mut stream), b"");
}

#[test]
fn a_ninth_authenticated_connection_gets_503() {
    let rig = Rig::new();
    let mut clients: Vec<IdeClient> = (0..8).map(|_| rig.client()).collect();
    let mut ninth = IdeClient::try_attach(rig.port, &Upgrade::proper(rig.port, &rig.token));
    let head = ninth.read_head();
    assert!(head.starts_with("HTTP/1.1 503 "), "{head}");
    assert_eq!(ninth.read_frame(), Received::Gone);

    drop(clients.pop());
    assert!(matches!(rig.next_event(), Event::ClientGone { .. }));
    rig.client().round_trip();
}

#[test]
fn eight_idle_unauthenticated_connections_leave_a_real_client_able_to_attach() {
    let rig = Rig::new();
    let _idle: Vec<TcpStream> = (0..8).map(|_| dial(rig.port)).collect();
    let mut clients: Vec<IdeClient> = (0..8).map(|_| rig.client()).collect();
    for client in &mut clients {
        client.round_trip();
    }
}

#[test]
fn a_ninth_unauthenticated_connection_evicts_the_oldest() {
    let rig = Rig::new();
    let mut oldest = dial(rig.port);
    let _others: Vec<TcpStream> = (0..8).map(|_| dial(rig.port)).collect();
    assert_eq!(read_to_end(&mut oldest), b"");
}

/// The first bytes of the answer to an upgrade finished on `stream`; none when it was closed under it.
fn finish_the_upgrade(stream: &mut TcpStream, rest: &str) -> String {
    let _ = stream.write_all(rest.as_bytes());
    let mut answer = [0; 32];
    let read = stream.read(&mut answer).unwrap_or(0);
    String::from_utf8_lossy(&answer[..read]).into_owned()
}

#[test]
fn a_client_part_way_through_its_head_outlasts_connections_that_sent_nothing() {
    let rig = Rig::new();
    let head = Upgrade::proper(rig.port, &rig.token).text();
    let (first, rest) = head.split_at(20);
    let mut client = dial(rig.port);
    client.write_all(first.as_bytes()).unwrap();
    // Time for the worker to read what came, and then for it to take the ones that follow. Two lots, each smaller than a
    // pass's accepts: a connection accepted in a pass is not evicted in it, so a full pass of newcomers evicts what was
    // there before them, whatever it holds.
    thread::sleep(QUIET);
    let _silent: Vec<TcpStream> = (0..7).map(|_| dial(rig.port)).collect();
    thread::sleep(QUIET);
    let _more: Vec<TcpStream> = (0..7).map(|_| dial(rig.port)).collect();
    thread::sleep(QUIET);
    let answer = finish_the_upgrade(&mut client, rest);
    assert!(answer.starts_with("HTTP/1.1 101 "), "not upgraded: {answer:?}");
}

#[test]
fn the_connection_with_the_fewest_bytes_goes_first() {
    let rig = Rig::new();
    let head = Upgrade::proper(rig.port, &rig.token).text();
    let (first, rest) = head.split_at(20);
    let mut oldest = dial(rig.port);
    oldest.write_all(first.as_bytes()).unwrap();
    let mut others: Vec<TcpStream> = (0..7).map(|_| dial(rig.port)).collect();
    for other in &mut others {
        other.write_all(b"GET").unwrap();
    }
    thread::sleep(QUIET);
    let _ninth = dial(rig.port);
    assert_eq!(
        read_to_end(&mut others[0]),
        b"",
        "the fewest bytes, and the oldest of those"
    );
    let answer = finish_the_upgrade(&mut oldest, rest);
    assert!(answer.starts_with("HTTP/1.1 101 "), "not upgraded: {answer:?}");
}

/// Slots full of connections that have each sent something, so one accepted with nothing read yet holds the fewest
/// bytes of all: the connection behind it in the same pass must not take that for a reason to evict it.
#[test]
fn a_client_accepted_into_full_slots_gets_its_read_before_the_next_one_can_evict_it() {
    let faults = Faults::default();
    let rig = Rig::started(
        Limits::default(),
        KeeperState::default(),
        MentionState::default(),
        faults.clone(),
    );
    let mut fillers: Vec<TcpStream> = (0..8).map(|_| dial(rig.port)).collect();
    for filler in &mut fillers {
        filler.write_all(b"G").unwrap();
    }
    thread::sleep(QUIET);
    faults.hold_at_poll(&rig);
    let mut client = IdeClient::try_attach(rig.port, &Upgrade::proper(rig.port, &rig.token));
    let _follower = dial(rig.port);
    faults.release();
    let head = client.read_head();
    assert!(head.starts_with("HTTP/1.1 101 "), "not upgraded: {head}");
}

#[test]
fn a_burst_of_connections_cannot_evict_a_client_before_its_first_read() {
    let faults = Faults::default();
    let rig = Rig::started(
        Limits::default(),
        KeeperState::default(),
        MentionState::default(),
        faults.clone(),
    );
    faults.hold_at_poll(&rig);
    let mut client = IdeClient::try_attach(rig.port, &Upgrade::proper(rig.port, &rig.token));
    let _burst: Vec<TcpStream> = (0..32).map(|_| dial(rig.port)).collect();
    faults.release();
    let head = client.read_head();
    assert!(head.starts_with("HTTP/1.1 101 "), "not upgraded: {head}");
}

// --- Frames ---

#[test]
fn an_unmasked_client_frame_closes_the_connection() {
    let rig = Rig::new();
    let mut client = rig.client();
    client.send_raw(&unmasked_frame(0x1, b"{}"));
    client.expect_closed_with(1002);
}

#[test]
fn a_10_mib_open_diff_gets_an_error_on_its_id_and_the_connection_lives() {
    let rig = Rig::new();
    let mut client = rig.client();
    let contents = "x".repeat(10 * MIB);
    // The id last, as the CLI's encoder writes it.
    let message = format!(
        r#"{{"jsonrpc":"2.0","method":"tools/call","params":{{"name":"openDiff","arguments":{{"new_file_contents":"{contents}"}}}},"id":7}}"#
    );
    client.send_text(message.as_bytes());
    expect_error_on(&client.read_message(), 7);
    client.round_trip();
}

#[test]
fn a_message_past_64_mib_closes_it() {
    let rig = Rig::new();
    let mut client = rig.client();
    client.send_raw(&announcing(65 * MIB as u64));
    client.expect_closed_with(1009);
}

#[test]
fn an_oversized_message_with_no_id_in_its_tail_closes_it() {
    let rig = Rig::new();
    let mut client = rig.client();
    client.send_text("x".repeat(9 * MIB).as_bytes());
    client.expect_closed_with(1009);
}

#[test]
fn ping_is_answered_with_pong() {
    let rig = Rig::new();
    let mut client = rig.client();
    client.ping(b"hello");
    assert_eq!(client.read_frame(), Received::Pong(b"hello".to_vec()));
}

#[test]
fn a_close_is_completed() {
    let rig = Rig::new();
    let mut client = rig.client();
    client.close(1000);
    client.expect_closed_with(1000);
    assert!(matches!(rig.next_event(), Event::ClientGone { .. }));
}

#[test]
fn no_frame_follows_a_close() {
    let faults = Faults::default();
    let rig = Rig::started(
        Limits::default(),
        KeeperState::default(),
        MentionState::default(),
        faults.clone(),
    );
    let mut client = rig.ready_client();
    faults.hold_at_poll(&rig);
    client.close(1000);
    rig.done(Done::Selection(json!({"n": 1})));
    faults.release();
    // The selection may go out ahead of the close, if the close reached the socket a pass late; never after it.
    loop {
        match client.read_frame() {
            Received::Text(_) => {}
            Received::Close(code) => {
                assert_eq!(code, 1000);
                break;
            }
            other => panic!("before the close: {other:?}"),
        }
    }
    assert_eq!(client.read_frame(), Received::Gone);
}

#[test]
fn a_message_that_is_no_json_leaves_the_connection_open() {
    let rig = Rig::new();
    let mut client = rig.client();
    client.send_text(b"not json");
    client.send_text(b"not json either");
    client.pong();
    client.round_trip();
}

// --- Outbound ---

#[test]
fn a_client_that_stops_reading_holds_one_pending_selection_and_a_second_is_served() {
    let rig = Rig::with_limits(Limits {
        send_buffer: Some(4096),
        ..Limits::default()
    });
    let mut stalled = rig.ready_client();
    stalled.set_small_receive_buffer();
    let mut reader = rig.ready_client();

    let pad = "x".repeat(1024);
    for n in 0..1000 {
        rig.done(Done::Selection(json!({"n": n, "text": pad})));
        // Read before the next goes out, since a selection is newest-wins on this connection too.
        let pushed = reader.read_message();
        assert_eq!(pushed["method"], "selection_changed");
        assert_eq!(pushed["params"]["n"], n);
    }

    let mut seen = Vec::new();
    while seen.last() != Some(&999) {
        seen.push(stalled.read_message()["params"]["n"].as_i64().unwrap());
    }
    // A socket can take more as the kernel works through its buffers, so numbers may be skipped anywhere.
    assert!(
        seen.windows(2).all(|pair| pair[0] < pair[1]),
        "older selections, in order, then the newest: {seen:?}"
    );
    assert!(
        seen.len() < 500,
        "the run between went out after all: {} frames",
        seen.len()
    );
}

#[test]
fn answers_past_the_outbound_bound_close_only_that_connection() {
    let rig = Rig::with_limits(Limits {
        outbound_bytes: MIB,
        send_buffer: Some(4096),
        ..Limits::default()
    });
    let mut stalled = rig.client();
    stalled.set_small_receive_buffer();
    let mut other = rig.client();
    let conn = rig.conn_of(&mut stalled);
    let text = "x".repeat(300 * 1024);
    for id in 0..8 {
        rig.command(Command::Answer {
            conn,
            id: rpc(id),
            answer: ToolAnswer::DiagnosticsText(text.clone()),
        });
    }
    match rig.next_event() {
        Event::ClientGone { conn: gone } => assert_eq!(gone, conn),
        other => panic!("{other:?}"),
    }
    read_to_end(&mut stalled.stream);
    other.round_trip();
}

#[test]
fn a_nine_mib_answer_becomes_an_error_and_the_connection_lives() {
    let rig = Rig::new();
    let mut client = rig.client();
    let conn = rig.conn_of(&mut client);
    rig.command(Command::Answer {
        conn,
        id: rpc(3),
        answer: ToolAnswer::FileSaved("x".repeat(9 * MIB)),
    });
    expect_error_on(&client.read_message(), 3);
    client.round_trip();
}

#[test]
fn an_answer_over_the_clients_cap_becomes_an_error() {
    let rig = Rig::with_limits(Limits {
        answer_frame_bytes: MIB,
        ..Limits::default()
    });
    let mut client = rig.client();
    uri_call(&mut client, 4);
    let Job::Uri { conn, id, .. } = rig.next_job() else {
        panic!("not the call");
    };
    rig.done(Done::Answer {
        conn,
        id,
        answer: ToolAnswer::DiagnosticsText("x".repeat(2 * MIB)),
    });
    expect_error_on(&client.read_message(), 4);
    client.round_trip();
}

// --- The judge ---

#[test]
fn a_close_tab_right_after_open_diff_reaches_the_host_second() {
    let rig = Rig::new();
    let mut client = rig.client();
    client.open_diff(1, "text");
    client.call_tool(2, "close_tab", json!({"tab_name": "t"}));

    let Job::Diff { conn, id, args } = rig.next_job() else {
        panic!("not the diff");
    };
    assert_eq!(id, rpc(1));
    let Job::Pass(close) = rig.next_job() else {
        panic!("not the close");
    };
    rig.no_event();

    rig.done(Done::Host(Event::OpenDiff {
        conn,
        id,
        diff: diff_request(&args.tab_name),
    }));
    rig.done(Done::Host(close));
    assert!(matches!(rig.next_event(), Event::OpenDiff { .. }));
    match rig.next_event() {
        Event::CloseTab { id, tab_name, .. } => {
            assert_eq!(id, rpc(2));
            assert_eq!(tab_name, "t");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn close_all_goes_behind_a_diff_the_same_way() {
    let rig = Rig::new();
    let mut client = rig.client();
    client.open_diff(1, "text");
    client.call_tool(2, "closeAllDiffTabs", json!({}));
    assert!(matches!(rig.next_job(), Job::Diff { .. }));
    assert!(matches!(rig.next_job(), Job::Pass(Event::CloseAll { .. })));
}

#[test]
fn an_oversized_contents_never_reaches_the_judge() {
    let rig = Rig::new();
    let mut client = rig.client();
    client.open_diff(1, &"x".repeat(4 * MIB + 1));
    expect_error_on(&client.read_message(), 1);
    rig.no_job();
    client.open_diff(2, &"x".repeat(4 * MIB));
    assert!(matches!(rig.next_job(), Job::Diff { .. }));
}

#[test]
fn a_connection_past_its_job_bound_gets_an_error() {
    let rig = Rig::new();
    let mut client = rig.client();
    for id in 1..=16 {
        uri_call(&mut client, id);
    }
    uri_call(&mut client, 17);
    expect_error_on(&client.read_message(), 17);

    let jobs: Vec<Job> = (0..16).map(|_| rig.next_job()).collect();
    rig.no_job();
    let Some(Job::Uri { conn, id, .. }) = jobs.into_iter().next() else {
        panic!("not a call");
    };
    assert_eq!(id, rpc(1));

    rig.done(Done::Answer {
        conn,
        id,
        answer: ToolAnswer::Empty,
    });
    assert_eq!(client.read_message()["id"], 1);
    uri_call(&mut client, 18);
    assert!(matches!(rig.next_job(), Job::Uri { .. }));
}

#[test]
fn what_the_host_sends_back_to_the_judge_is_held_to_the_bound() {
    let rig = Rig::with_limits(Limits {
        jobs_per_conn: 1,
        ..Limits::default()
    });
    let mut client = rig.client();
    uri_call(&mut client, 1);
    let Job::Uri { conn, id, .. } = rig.next_job() else {
        panic!("not the call");
    };

    let entries = |id| Command::Entries {
        conn,
        id: rpc(id),
        named_uri: None,
        answer: no_entries(),
    };
    rig.command(entries(2));
    expect_error_on(&client.read_message(), 2);
    rig.command(Command::Saved {
        conn,
        id: rpc(3),
        lines: Vec::new(),
        shape: LineShape { crlf: false, eol: true },
    });
    expect_error_on(&client.read_message(), 3);
    rig.command(entries(1));
    expect_error_on(&client.read_message(), 1);
    rig.no_job();

    rig.done(Done::Answer {
        conn,
        id,
        answer: ToolAnswer::Empty,
    });
    assert_eq!(client.read_message()["id"], 1);
    rig.command(entries(4));
    assert!(matches!(rig.next_job(), Job::Entries { .. }), "the place is free again");
}

#[test]
fn a_withdrawn_call_holds_its_place_until_the_judge_gives_it_back() {
    let rig = Rig::with_limits(Limits {
        jobs_per_conn: 1,
        ..Limits::default()
    });
    let mut client = rig.client();
    uri_call(&mut client, 1);
    client.notify("notifications/cancelled", json!({"requestId": 1}));
    uri_call(&mut client, 2);
    expect_error_on(&client.read_message(), 2);

    let Job::Uri { conn, id, .. } = rig.next_job() else {
        panic!("not the call");
    };
    rig.no_job();
    rig.done(Done::Answer {
        conn,
        id,
        answer: ToolAnswer::Empty,
    });
    client.round_trip();
    uri_call(&mut client, 3);
    assert!(matches!(rig.next_job(), Job::Uri { .. }));
}

#[test]
fn a_call_with_an_id_already_out_gets_an_error() {
    let rig = Rig::new();
    let mut client = rig.client();
    uri_call(&mut client, 1);
    uri_call(&mut client, 1);
    expect_error_on(&client.read_message(), 1);
    assert!(matches!(rig.next_job(), Job::Uri { .. }));
    rig.no_job();
}

#[test]
fn a_cancel_of_an_id_never_seen_remembers_nothing() {
    let rig = Rig::new();
    let mut client = rig.client();
    client.notify("notifications/cancelled", json!({"requestId": 55}));
    client.round_trip();
    rig.no_event();
    uri_call(&mut client, 55);
    let Job::Uri { conn, id, .. } = rig.next_job() else {
        panic!("not the call");
    };
    rig.done(Done::Answer {
        conn,
        id,
        answer: ToolAnswer::Empty,
    });
    assert_eq!(client.read_message()["id"], 55);
}

#[test]
fn calls_the_host_holds_are_held_to_the_bound() {
    let rig = Rig::new();
    let mut client = rig.client();
    for id in 1..=16 {
        all_call(&mut client, id);
    }
    all_call(&mut client, 17);
    expect_error_on(&client.read_message(), 17);

    let conns: Vec<ConnId> = (1..=16)
        .map(|_| match rig.next_event() {
            Event::Diagnostics { conn, .. } => conn,
            other => panic!("not a call: {other:?}"),
        })
        .collect();
    rig.no_event();

    rig.command(Command::Answer {
        conn: conns[0],
        id: rpc(1),
        answer: ToolAnswer::Empty,
    });
    assert_eq!(client.read_message()["id"], 1);
    all_call(&mut client, 18);
    assert!(
        matches!(rig.next_event(), Event::Diagnostics { .. }),
        "the place is free"
    );
}

/// A `uri` and a proposal pass the judge, which gives them back, and neither is any less work for the host.
#[test]
fn a_call_past_the_judge_keeps_a_place_at_the_host() {
    let rig = Rig::with_limits(Limits {
        jobs_per_conn: 1,
        ..Limits::default()
    });
    let mut client = rig.client();
    let to_host = |client: &mut IdeClient, id: i64| {
        uri_call(client, id);
        let Job::Uri { conn, id, uri } = rig.next_job() else {
            panic!("not the call");
        };
        rig.done(Done::Host(Event::Diagnostics {
            conn,
            id,
            query: DiagnosticsQuery::One {
                uri,
                path: PathBuf::from("/p/a"),
            },
        }));
        conn
    };
    let conn = to_host(&mut client, 1);
    assert!(matches!(rig.next_event(), Event::Diagnostics { .. }));
    to_host(&mut client, 2);
    expect_error_on(&client.read_message(), 2);

    client.open_diff(3, "text");
    let Job::Diff { id, .. } = rig.next_job() else {
        panic!("not the diff");
    };
    rig.done(Done::Host(Event::OpenDiff {
        conn,
        id,
        diff: diff_request("t"),
    }));
    expect_error_on(&client.read_message(), 3);
    rig.no_event();
}

#[test]
fn a_close_keeps_a_place_at_the_host_until_it_is_answered() {
    let rig = Rig::with_limits(Limits {
        jobs_per_conn: 1,
        ..Limits::default()
    });
    let mut client = rig.client();
    let to_host = |client: &mut IdeClient, id: i64| {
        client.call_tool(id, "closeAllDiffTabs", json!({}));
        let Job::Pass(event) = rig.next_job() else {
            panic!("not the close");
        };
        rig.done(Done::Host(event));
    };
    to_host(&mut client, 1);
    let Event::CloseAll { conn, id } = rig.next_event() else {
        panic!("not the close");
    };
    to_host(&mut client, 2);
    expect_error_on(&client.read_message(), 2);
    rig.no_event();
    rig.command(Command::Answer {
        conn,
        id,
        answer: ToolAnswer::Empty,
    });
    assert_eq!(client.read_message()["id"], 1);
    to_host(&mut client, 3);
    assert!(matches!(rig.next_event(), Event::CloseAll { .. }));
}

#[test]
fn what_ends_a_call_at_the_host_frees_its_place() {
    let rig = Rig::with_limits(Limits {
        jobs_per_conn: 1,
        ..Limits::default()
    });
    let mut client = rig.client();
    let held = |client: &mut IdeClient, id: i64| {
        all_call(client, id);
        match rig.next_event() {
            Event::Diagnostics { conn, .. } => conn,
            other => panic!("not the call: {other:?}"),
        }
    };

    let conn = held(&mut client, 1);
    rig.command(Command::Entries {
        conn,
        id: rpc(1),
        named_uri: None,
        answer: no_entries(),
    });
    let Job::Entries { id, .. } = rig.next_job() else {
        panic!("not the entries");
    };
    rig.done(Done::Answer {
        conn,
        id,
        answer: ToolAnswer::Empty,
    });
    assert_eq!(client.read_message()["id"], 1);

    held(&mut client, 2);
    rig.command(Command::Saved {
        conn,
        id: rpc(2),
        lines: Vec::new(),
        shape: LineShape { crlf: false, eol: true },
    });
    let Job::Join { id, .. } = rig.next_job() else {
        panic!("not the join");
    };
    rig.done(Done::Answer {
        conn,
        id,
        answer: ToolAnswer::DiffRejected,
    });
    assert_eq!(client.read_message()["id"], 2);

    held(&mut client, 3);
    client.notify("notifications/cancelled", json!({"requestId": 3}));
    match rig.next_event() {
        Event::Cancelled { id, .. } => assert_eq!(id, rpc(3)),
        other => panic!("{other:?}"),
    }
    rig.command(Command::Released { conn, id: rpc(3) });
    client.round_trip();
    held(&mut client, 4);
}

#[test]
fn a_call_with_an_id_the_host_holds_gets_an_error() {
    let rig = Rig::new();
    let mut client = rig.client();
    all_call(&mut client, 1);
    assert!(matches!(rig.next_event(), Event::Diagnostics { .. }));
    all_call(&mut client, 1);
    expect_error_on(&client.read_message(), 1);
    uri_call(&mut client, 2);
    assert!(matches!(rig.next_job(), Job::Uri { .. }));
    all_call(&mut client, 2);
    expect_error_on(&client.read_message(), 2);
    rig.no_event();
}

/// Sends a request and reads up to its answer: every message sent before it has then been read. How many errors came
/// ahead of it.
fn errors_before_a_marker(client: &mut IdeClient) -> usize {
    const MARKER: i64 = 999_999;
    client.request(MARKER, "tools/list", json!({}));
    let mut errors = 0;
    loop {
        let message = client.read_message();
        if message["id"] == MARKER {
            return errors;
        }
        assert_eq!(message["error"]["code"], TOOL_ERROR, "{message}");
        errors += 1;
    }
}

/// `rounds` connections, one after the other, each asking for all it may and closing. The test is the judge, and answers
/// nothing.
#[test]
fn reconnecting_renews_no_allowance_at_a_stuck_judge() {
    let rig = Rig::new();
    let in_all = 8 * 16;
    let mut refused = 0;
    let rounds = 2 * 8;
    for _ in 0..rounds {
        let mut client = rig.client();
        for id in 1..=16 {
            uri_call(&mut client, id);
        }
        refused += errors_before_a_marker(&mut client);
        drop(client);
        assert!(matches!(rig.next_event(), Event::ClientGone { .. }));
    }
    let mut left_behind = Vec::new();
    while let Ok(job) = rig.jobs.as_ref().unwrap().try_recv() {
        left_behind.push(job);
    }
    assert_eq!(
        left_behind.len(),
        in_all,
        "the calls of connections long gone still wait"
    );
    assert_eq!(refused, rounds * 16 - in_all);

    // A job given back makes room, whoever asked.
    rig.done(Done::Answer {
        conn: 0,
        id: rpc(1),
        answer: ToolAnswer::Empty,
    });
    let mut client = rig.client();
    client.round_trip();
    uri_call(&mut client, 1);
    assert!(matches!(rig.next_job(), Job::Uri { .. }));
}

/// A connection's calls end with it at the host, but only once the host has got that far.
#[test]
fn reconnecting_renews_no_allowance_at_a_stalled_host() {
    let rig = Rig::new();
    let in_all = 8 * 16;
    let mut attached = 0;
    let turned_away = loop {
        assert!(
            attached < 2 * 8,
            "the host has been sent all of this and taken none of it"
        );
        let upgrade = Upgrade::proper(rig.port, &rig.token);
        let mut client = IdeClient::try_attach(rig.port, &upgrade);
        let head = client.read_head();
        if !head.starts_with("HTTP/1.1 101 ") {
            break head;
        }
        attached += 1;
        for id in 1..=16 {
            all_call(&mut client, id);
        }
        errors_before_a_marker(&mut client);
    };
    assert!(turned_away.starts_with("HTTP/1.1 503 "), "{turned_away}");
    let calls = rig
        .take_events()
        .iter()
        .filter(|event| matches!(event, Event::Diagnostics { .. }))
        .count();
    assert!(calls <= in_all, "{calls} calls wait for the host");
    rig.client().round_trip();
}

/// A window that keeps taking what it is sent, and an nvim that answers none of it: the host holds nothing that says so
/// but the calls still out, so a call a client leaves has to keep its place.
#[test]
fn cancelling_does_not_free_a_place_nvim_still_owes() {
    let rig = Rig::new();
    let mut client = rig.client();
    let mut handed = 0;
    for id in 1..=64 {
        all_call(&mut client, id);
        client.notify("notifications/cancelled", json!({"requestId": id}));
        errors_before_a_marker(&mut client);
        handed += rig
            .take_events()
            .iter()
            .filter(|event| matches!(event, Event::Diagnostics { .. }))
            .count();
    }
    assert_eq!(handed, 16);
}

/// As above, with connections going in place of the cancels.
#[test]
fn reconnecting_does_not_free_a_place_nvim_still_owes() {
    let rig = Rig::new();
    let in_all = 8 * 16;
    let mut handed = Vec::new();
    for _ in 0..16 {
        let mut client = rig.client();
        for id in 1..=16 {
            all_call(&mut client, id);
        }
        errors_before_a_marker(&mut client);
        drop(client);
        loop {
            let event = rig.next_event();
            let gone = matches!(event, Event::ClientGone { .. });
            if let Event::Diagnostics { conn, id, .. } = event {
                handed.push((conn, id));
            }
            if gone {
                break;
            }
        }
    }
    assert_eq!(handed.len(), in_all);

    let (conn, id) = handed[0].clone();
    rig.command(Command::Released { conn, id });
    let mut client = rig.client();
    all_call(&mut client, 1);
    assert!(matches!(rig.next_event(), Event::Diagnostics { .. }), "one came free");
    all_call(&mut client, 2);
    expect_error_on(&client.read_message(), 2);
}

/// What an answer does for a connection gone: whichever end comes first gives the place back, once.
#[test]
fn an_answer_for_a_gone_connection_frees_its_place() {
    let rig = Rig::with_limits(Limits {
        authed: 1,
        jobs_per_conn: 1,
        ..Limits::default()
    });
    let mut client = rig.client();
    let conn = rig.conn_of(&mut client);
    drop(client);
    assert!(matches!(rig.next_event(), Event::ClientGone { .. }));

    let mut client = rig.client();
    all_call(&mut client, 1);
    expect_error_on(&client.read_message(), 1);
    rig.command(Command::Answer {
        conn,
        id: rpc(9_000),
        answer: ToolAnswer::Empty,
    });
    client.round_trip();
    all_call(&mut client, 2);
    assert!(matches!(rig.next_event(), Event::Diagnostics { .. }));
}

#[test]
fn a_release_frees_only_a_call_the_client_left() {
    let rig = Rig::with_limits(Limits {
        jobs_per_conn: 1,
        ..Limits::default()
    });
    let mut client = rig.client();
    let conn = rig.conn_of(&mut client);
    rig.command(Command::Released { conn, id: rpc(9_000) });
    all_call(&mut client, 1);
    expect_error_on(&client.read_message(), 1);

    client.notify("notifications/cancelled", json!({"requestId": 9_000}));
    client.notify("notifications/cancelled", json!({"requestId": 9_000}));
    assert!(matches!(rig.next_event(), Event::Cancelled { .. }));
    all_call(&mut client, 2);
    expect_error_on(&client.read_message(), 2);
    rig.no_event();

    rig.command(Command::Released { conn, id: rpc(9_000) });
    client.round_trip();
    all_call(&mut client, 3);
    assert!(matches!(rig.next_event(), Event::Diagnostics { .. }));
}

#[test]
fn a_cancelled_diff_still_with_the_judge_is_dropped() {
    let rig = Rig::new();
    let mut client = rig.client();
    client.open_diff(1, "text");
    let Job::Diff { conn, id, .. } = rig.next_job() else {
        panic!("not the diff");
    };
    client.notify("notifications/cancelled", json!({"requestId": 1}));
    client.round_trip();
    rig.done(Done::Host(Event::OpenDiff {
        conn,
        id,
        diff: diff_request("t"),
    }));
    assert_eq!(
        rig.conn_of(&mut client),
        conn,
        "the next event is the marker, not the diff"
    );
}

#[test]
fn a_host_call_goes_back_through_the_judge_and_answers_the_client() {
    let rig = Rig::new();
    let mut client = rig.client();
    uri_call(&mut client, 5);
    let Job::Uri { conn, id, uri } = rig.next_job() else {
        panic!("not the call");
    };
    rig.done(Done::Host(Event::Diagnostics {
        conn,
        id,
        query: DiagnosticsQuery::One {
            uri,
            path: PathBuf::from("/p/a"),
        },
    }));
    assert!(matches!(rig.next_event(), Event::Diagnostics { .. }));

    rig.command(Command::Entries {
        conn,
        id: rpc(5),
        named_uri: None,
        answer: no_entries(),
    });
    let Job::Entries { id, .. } = rig.next_job() else {
        panic!("not the entries");
    };
    rig.done(Done::Answer {
        conn,
        id,
        answer: ToolAnswer::DiagnosticsText("[]".to_owned()),
    });
    assert_eq!(client.read_message()["id"], 5);

    rig.command(Command::Saved {
        conn,
        id: rpc(6),
        lines: Vec::new(),
        shape: LineShape { crlf: false, eol: true },
    });
    let Job::Join { id, .. } = rig.next_job() else {
        panic!("not the join");
    };
    rig.done(Done::Answer {
        conn,
        id,
        answer: ToolAnswer::DiffRejected,
    });
    assert_eq!(client.read_message()["id"], 6);
}

#[test]
fn what_comes_for_a_gone_connection_is_dropped() {
    let rig = Rig::new();
    let mut client = rig.client();
    uri_call(&mut client, 1);
    let Job::Uri { conn, id, uri } = rig.next_job() else {
        panic!("not the call");
    };
    drop(client);
    assert!(matches!(rig.next_event(), Event::ClientGone { .. }));

    rig.done(Done::Host(Event::Diagnostics {
        conn,
        id,
        query: DiagnosticsQuery::One {
            uri,
            path: PathBuf::from("/p/a"),
        },
    }));
    rig.command(Command::Entries {
        conn,
        id: rpc(1),
        named_uri: None,
        answer: no_entries(),
    });
    rig.command(Command::Saved {
        conn,
        id: rpc(1),
        lines: Vec::new(),
        shape: LineShape { crlf: false, eol: true },
    });
    rig.command(Command::Answer {
        conn,
        id: rpc(1),
        answer: ToolAnswer::Empty,
    });
    rig.command(Command::Context(ContextUpdate {
        context: None,
        facts: PushFacts::default(),
    }));
    assert!(
        matches!(rig.next_job(), Job::Context(_)),
        "the marker comes first: nothing else was queued"
    );
    rig.done(Done::Host(Event::MentionOutside));
    assert!(
        matches!(rig.next_event(), Event::MentionOutside),
        "no event for the gone connection"
    );
}

// --- Pushes ---

/// The newest selection reaches a connection the first time it says it is ready, by either notification, and the
/// other notification adds nothing.
fn newest_selection_once(first: (&str, Value), second: (&str, Value)) {
    let rig = Rig::new();
    rig.done(Done::Selection(json!({"n": 1})));
    rig.done(Done::Selection(json!({"n": 2})));
    let mut client = rig.client();
    client.round_trip();

    client.notify(first.0, first.1);
    let pushed = client.read_message();
    assert_eq!(pushed["method"], "selection_changed");
    assert_eq!(pushed["params"]["n"], 2);

    client.notify(second.0, second.1);
    client.round_trip();
}

#[test]
fn the_newest_selection_reaches_a_client_once_it_is_ready() {
    newest_selection_once(
        ("notifications/initialized", json!({})),
        ("ide_connected", json!({"pid": 7})),
    );
    newest_selection_once(
        ("ide_connected", json!({"pid": 7})),
        ("notifications/initialized", json!({})),
    );
}

#[test]
fn an_equal_selection_is_not_sent_again() {
    let rig = Rig::new();
    let mut client = rig.ready_client();
    rig.done(Done::Selection(json!({"n": 1})));
    assert_eq!(client.read_message()["params"]["n"], 1);
    rig.done(Done::Selection(json!({"n": 1})));
    rig.done(Done::Selection(json!({"n": 2})));
    assert_eq!(client.read_message()["params"]["n"], 2);
}

#[test]
fn the_first_client_opens_the_mention_socket_and_the_last_closes_it() {
    let socket = PathBuf::from("/nowhere/m.sock");
    let mentions = MentionState {
        socket: Some(socket.clone()),
        ..MentionState::default()
    };
    let rig = Rig::started(Limits::default(), KeeperState::default(), mentions, System);
    let first = rig.client();
    match rig.next_event() {
        Event::MentionUp { socket: told } => assert_eq!(told, socket),
        other => panic!("{other:?}"),
    }
    let second = rig.client();
    assert_eq!(rig.mentions.opens.load(Ordering::SeqCst), 1);

    drop(first);
    assert!(matches!(rig.next_event(), Event::ClientGone { .. }));
    assert_eq!(rig.mentions.closes.load(Ordering::SeqCst), 0);

    drop(second);
    assert!(matches!(rig.next_event(), Event::ClientGone { .. }));
    assert!(matches!(rig.next_event(), Event::MentionDown));
    assert_eq!(rig.mentions.closes.load(Ordering::SeqCst), 1);
}

#[test]
fn no_mention_socket_means_no_mention_events() {
    let rig = Rig::new();
    drop(rig.client());
    assert!(matches!(rig.next_event(), Event::ClientGone { .. }));
    rig.no_event();
}

#[test]
fn mention_lines_go_to_the_judge_and_reach_every_ready_client_in_order() {
    let rig = Rig::new();
    let mut ready = rig.ready_client();
    let mut other_ready = rig.ready_client();
    let mut unready = rig.client();

    let line = |line_start| MentionLine {
        file: "/p/a".to_owned(),
        line_start,
        line_end: line_start,
    };
    rig.mentions.lines.lock().unwrap().extend([line(1), line(2)]);
    rig.mentions.readable.signal();
    for wanted in 1..=2 {
        match rig.next_job() {
            Job::Mention(line) => assert_eq!(line.line_start, wanted),
            other => panic!("{other:?}"),
        }
    }

    rig.done(Done::Mention(json!({"lineStart": 1})));
    rig.done(Done::Mention(json!({"lineStart": 2})));
    for client in [&mut ready, &mut other_ready] {
        for wanted in 1..=2 {
            let pushed = client.read_message();
            assert_eq!(pushed["method"], "at_mentioned");
            assert_eq!(pushed["params"]["lineStart"], wanted);
        }
    }
    unready.round_trip();
}

fn mention_lines(rig: &Rig, count: u32) {
    let line = |line_start| MentionLine {
        file: "/p/a".to_owned(),
        line_start,
        line_end: line_start,
    };
    rig.mentions.lines.lock().unwrap().extend((0..count).map(line));
    rig.mentions.readable.signal();
    thread::sleep(QUIET);
}

/// A mention outside the project is an event for each line on the socket, and nvim writes those lines whether or not the
/// window is taking anything.
#[test]
fn what_a_mention_tells_the_window_stops_while_the_window_is_behind() {
    let rig = Rig::with_limits(Limits {
        authed: 1,
        jobs_per_conn: 4,
        ..Limits::default()
    });
    for _ in 0..10 {
        rig.done(Done::Host(Event::MentionOutside));
    }
    thread::sleep(QUIET);
    let told = rig.take_events();
    assert_eq!(told.len(), 4, "{told:?}");

    rig.done(Done::Host(Event::MentionOutside));
    assert!(
        matches!(rig.next_event(), Event::MentionOutside),
        "taken, there is room"
    );
}

#[test]
fn mention_lines_are_dropped_while_the_window_is_behind() {
    let rig = Rig::with_limits(Limits {
        authed: 1,
        jobs_per_conn: 4,
        ..Limits::default()
    });
    for _ in 0..4 {
        rig.done(Done::Host(Event::MentionOutside));
    }
    thread::sleep(QUIET);
    mention_lines(&rig, 3);
    rig.no_job();

    rig.take_events();
    mention_lines(&rig, 1);
    assert!(matches!(rig.next_job(), Job::Mention(_)), "taken, there is room");
}

#[test]
fn mention_lines_are_held_to_what_the_judge_may_have_out() {
    let rig = Rig::with_limits(Limits {
        authed: 1,
        jobs_per_conn: 4,
        ..Limits::default()
    });
    mention_lines(&rig, 10);
    for _ in 0..4 {
        assert!(matches!(rig.next_job(), Job::Mention(_)));
    }
    rig.no_job();

    rig.done(Done::Mention(json!({"lineStart": 1})));
    thread::sleep(QUIET);
    mention_lines(&rig, 1);
    assert!(matches!(rig.next_job(), Job::Mention(_)), "given back, there is room");
}

#[test]
fn the_keeper_is_woken_by_its_descriptor_and_by_its_retry() {
    let keeper = KeeperState {
        watch: Some(Wake::new().unwrap()),
        ..KeeperState::default()
    };
    let rig = Rig::started(Limits::default(), keeper, MentionState::default(), System);
    let wakes = || rig.keeper.wakes.load(Ordering::SeqCst);

    rig.keeper.watch.as_ref().unwrap().signal();
    let waited = Instant::now();
    while wakes() == 0 {
        assert!(waited.elapsed() < WAIT, "its descriptor woke nothing");
        thread::sleep(Duration::from_millis(5));
    }

    *rig.keeper.retry_at.lock().unwrap() = Some(Instant::now() + Duration::from_millis(50));
    rig.nudge();
    while wakes() == 1 {
        assert!(waited.elapsed() < WAIT, "the retry came due and nothing woke");
        thread::sleep(Duration::from_millis(5));
    }
}

// --- Ways out ---

#[test]
fn started_is_the_first_event_and_a_failed_keeper_leaves_no_listener() {
    let keeper = KeeperState {
        refuse: Some("no directory".to_owned()),
        ..KeeperState::default()
    };
    let mut rig = Rig::spawn(Limits::default(), keeper, MentionState::default(), System);
    match rig.next_event() {
        Event::Started(Started::NotAttached { why }) => assert_eq!(why, "no directory"),
        other => panic!("{other:?}"),
    }
    rig.join();
    rig.assert_port_closed();
    assert!(
        matches!(rig.events.try_recv(), Err(TryRecvError::Disconnected)),
        "nothing follows it"
    );
    assert_eq!(rig.keeper.finishes.load(Ordering::SeqCst), 0, "no lock was written");
}

#[test]
fn a_keeper_that_panics_in_start_still_answers_started() {
    let keeper = KeeperState {
        panic_in_start: true,
        ..KeeperState::default()
    };
    let mut rig = Rig::spawn(Limits::default(), keeper, MentionState::default(), System);
    match rig.next_event() {
        Event::Started(Started::NotAttached { .. }) => {}
        other => panic!("{other:?}"),
    }
    rig.join();
    assert_eq!(
        rig.keeper.finishes.load(Ordering::SeqCst),
        1,
        "a start that died may have written its lock"
    );
    rig.assert_port_closed();
}

#[test]
fn stop_runs_the_keeper_before_the_listener_closes() {
    let keeper = KeeperState {
        dial_on_finish: true,
        ..KeeperState::default()
    };
    let mut rig = Rig::started(Limits::default(), keeper, MentionState::default(), System);
    rig.stop();
    assert_eq!(
        *rig.keeper.dialled.lock().unwrap(),
        Some(true),
        "the port was open while the keeper finished"
    );
    assert_eq!(rig.keeper.finishes.load(Ordering::SeqCst), 1);
    rig.assert_port_closed();
}

#[test]
fn a_clean_stop_sends_no_lost() {
    let mut rig = Rig::new();
    let mut client = rig.client();
    rig.stop();
    assert_eq!(client.read_frame(), Received::Gone);
    let events: Vec<Event> = rig.events.try_iter().collect();
    assert!(matches!(events[..], [Event::ClientGone { .. }]), "{events:?}");
    assert_eq!(rig.mentions.closes.load(Ordering::SeqCst), 1);
}

#[test]
fn a_worker_that_dies_removes_its_lock() {
    let mut rig = Rig::new();
    let mut client = rig.client();
    rig.mentions.panic_in_poll.store(true, Ordering::SeqCst);
    rig.mentions.readable.signal();
    match rig.next_event() {
        Event::ClientGone { .. } => {}
        other => panic!("{other:?}"),
    }
    assert!(matches!(rig.next_event(), Event::Lost));
    assert_eq!(rig.keeper.finishes.load(Ordering::SeqCst), 1);
    assert_eq!(rig.mentions.closes.load(Ordering::SeqCst), 1);
    assert_eq!(client.read_frame(), Received::Gone);
    rig.join();
    rig.assert_port_closed();
}

#[test]
fn a_finish_that_panics_on_the_way_out_of_a_panic_still_ends_with_lost() {
    let keeper = KeeperState {
        panic_in_finish: true,
        ..KeeperState::default()
    };
    let mut rig = Rig::started(Limits::default(), keeper, MentionState::default(), System);
    rig.mentions.panic_in_poll.store(true, Ordering::SeqCst);
    rig.mentions.readable.signal();
    rig.expect_lost();
    rig.join();
    assert_eq!(rig.keeper.finishes.load(Ordering::SeqCst), 1);
    assert_eq!(
        rig.mentions.closes.load(Ordering::SeqCst),
        1,
        "the mention socket went too"
    );
    rig.assert_port_closed();
}

#[test]
fn a_failing_poll_or_a_gone_judge_ends_the_same_way() {
    let faults = Faults::default();
    let mut failing_poll = Rig::started(
        Limits::default(),
        KeeperState::default(),
        MentionState::default(),
        faults.clone(),
    );
    faults.poll_errno.store(libc::EINVAL, Ordering::SeqCst);
    failing_poll.nudge();

    // A judge can die only on a job, and its closed channel signals nothing, so no wake is sent here.
    let mut gone_done = Rig::new();
    let mut waiting = gone_done.client();
    uri_call(&mut waiting, 1);
    gone_done.next_job();
    gone_done.done = None;

    let mut gone_jobs = Rig::new();
    let mut client = gone_jobs.client();
    gone_jobs.jobs = None;
    uri_call(&mut client, 1);

    for rig in [&mut failing_poll, &mut gone_done, &mut gone_jobs] {
        rig.expect_lost();
        rig.join();
        assert_eq!(rig.keeper.finishes.load(Ordering::SeqCst), 1);
        assert_eq!(rig.mentions.closes.load(Ordering::SeqCst), 1);
        rig.assert_port_closed();
    }
}

#[test]
fn a_gone_main_context_ends_the_loop() {
    let (commands, _) = channel();
    let mut rig = Rig::new();
    rig.commands = commands;
    rig.nudge();
    rig.expect_lost();
    rig.join();
    assert_eq!(rig.keeper.finishes.load(Ordering::SeqCst), 1);
}

#[test]
fn an_interrupted_poll_is_no_failure() {
    let faults = Faults::default();
    let rig = Rig::started(
        Limits::default(),
        KeeperState::default(),
        MentionState::default(),
        faults.clone(),
    );
    let mut client = rig.client();
    faults.poll_errno.store(libc::EINTR, Ordering::SeqCst);
    rig.nudge();
    // The nudge ends the pass under way; the call that sees the error is the next one.
    let waited = Instant::now();
    while faults.poll_errno.load(Ordering::SeqCst) != 0 {
        assert!(waited.elapsed() < WAIT, "the worker never called poll again");
        thread::sleep(Duration::from_millis(1));
    }
    client.round_trip();
}

#[test]
fn a_failing_accept_does_not_spin() {
    let faults = Faults::default();
    let rig = Rig::started(
        Limits::default(),
        KeeperState::default(),
        MentionState::default(),
        faults.clone(),
    );
    let mut open = rig.client();

    faults.accept_errno.store(libc::EMFILE, Ordering::SeqCst);
    let _waiting = dial(rig.port);
    let before = faults.polls.load(Ordering::SeqCst);
    let watched = Duration::from_millis(1500);
    thread::sleep(watched);
    let wakes = faults.polls.load(Ordering::SeqCst) - before;
    assert!(
        wakes <= 8,
        "{wakes} wakes in {watched:?}: the loop is spinning on the waiting connection"
    );

    open.round_trip();
    faults.accept_errno.store(0, Ordering::SeqCst);
    rig.client().round_trip();
}

#[test]
fn one_clients_flood_does_not_delay_anothers_answer() {
    let rig = Rig::with_limits(Limits {
        // The flood is about the reads, so its pongs have room to wait.
        outbound_bytes: 256 * MIB,
        ..Limits::default()
    });
    let mut flooder = rig.client();
    let mut other = rig.client();

    const PING_BYTES: usize = 6;
    let start = empty_pings(MIB / PING_BYTES);
    let mut rest = empty_pings((32 * MIB - MIB) / PING_BYTES);
    rest.extend_from_slice(&ping_frame(b"mark"));

    let flood_sent = Arc::new(AtomicBool::new(false));
    let (begun, flood_begun) = channel();
    let mut writer = flooder.stream.try_clone().unwrap();
    let sent = Arc::clone(&flood_sent);
    let flood = thread::spawn(move || {
        writer.write_all(&start).unwrap();
        begun.send(()).unwrap();
        writer.write_all(&rest).unwrap();
        sent.store(true, Ordering::SeqCst);
    });
    flood_begun.recv_timeout(WAIT).expect("the flood began");

    other.round_trip();
    assert!(
        !flood_sent.load(Ordering::SeqCst),
        "the flood was read to its end before another client's call"
    );

    flooder.read_until_ends_with(&[0x8a, 4, b'm', b'a', b'r', b'k']);
    flood.join().unwrap();
}
