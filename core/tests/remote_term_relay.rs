//! `eitri host term`'s relay ([`eitri_core::remote_lsp::term`]) against a fake `Terminal` service served in memory,
//! with a PTY pair standing for the person's terminal (a pipe where the test is about end of input). No ssh, no
//! display, no real helper. Reattaching waits out the link's own first retry delay, and a screen that never comes waits
//! out the end-of-input drain, so those tests take a second or two; none asserts a wall-clock bound. A stop while
//! connecting runs `eitri host term` itself, in a child process (the signal handlers are the process's).
#![cfg(target_os = "linux")]

use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use agent::remote_client::in_process;
use agent::remote_client::terminal_transport::{tokio, tokio_stream, tonic, wire};
use agent::ssh::{ChannelLauncher, ConnectError};
use eitri_core::remote_host::cli::{self, Agent, Contact, Context, Reach};
use eitri_core::remote_host::install::InstallPlan;
use eitri_core::remote_lsp::term::{self, Session, Terminal};
use eitri_core::remote_project::terminal_facts::TerminalFactsHandle;
use eitri_core::remote_project::terminal_link::{TerminalLink, TerminalLinkOptions, TerminalTarget};
use eitri_core::remote_project::terminals::close_on;
use eitri_core::remote_project::{RemoteConnection, RemoteProject, RemoteProjects};
use eitri_terminal::{PtySize, TerminalColors};
use tonic::{Request, Response, Status};
use wire::terminal_client_frame::Frame as Up;
use wire::terminal_server_frame::Frame as Down;

const HOST: &str = "example-host";
const KEY: &str = "SHA256:example-host-key";
const TERMINAL_ID: &str = "0123456789abcdef0123456789abcdef";
/// A hang guard, not an assertion about speed: how long a wait for something that should happen goes on.
const HANG: Duration = Duration::from_secs(60);
const SIZE: PtySize = PtySize {
    cols: 100,
    rows: 30,
    cell_width_px: 0,
    cell_height_px: 0,
};

// The fake server -----------------------------------------------------------------------------------------------

/// A Helper that answers Hello and nothing else.
struct StubHelper;

type Streamed<T> = Pin<Box<dyn tokio_stream::Stream<Item = Result<T, Status>> + Send>>;

fn nothing<T>() -> Result<Response<T>, Status> {
    Err(Status::unimplemented("not in these tests"))
}

#[tonic::async_trait]
impl wire::helper_server::Helper for StubHelper {
    async fn hello(&self, _: Request<wire::HelloRequest>) -> Result<Response<wire::HelloResponse>, Status> {
        Ok(Response::new(wire::HelloResponse {
            protocol_major: wire::PROTOCOL_MAJOR,
            protocol_minor: wire::PROTOCOL_MINOR,
            helper_version: "test".into(),
            capabilities: vec![wire::CAP_TERMINAL.into()],
            daemon_instance: "daemon-1".into(),
            boot_id: "boot-1".into(),
            ..Default::default()
        }))
    }
    async fn start_sidecar(
        &self,
        _: Request<wire::StartSidecarRequest>,
    ) -> Result<Response<wire::StartSidecarResponse>, Status> {
        nothing()
    }
    async fn stop_sidecar(
        &self,
        _: Request<wire::StopSidecarRequest>,
    ) -> Result<Response<wire::StopSidecarResponse>, Status> {
        nothing()
    }
    async fn list_sidecars(
        &self,
        _: Request<wire::ListSidecarsRequest>,
    ) -> Result<Response<wire::ListSidecarsResponse>, Status> {
        nothing()
    }
    async fn pending(&self, _: Request<wire::PendingRequest>) -> Result<Response<wire::PendingResponse>, Status> {
        nothing()
    }
    async fn trust_scan(
        &self,
        _: Request<wire::TrustScanRequest>,
    ) -> Result<Response<wire::TrustScanResponse>, Status> {
        nothing()
    }
    async fn sync_ignores(
        &self,
        _: Request<wire::SyncIgnoresRequest>,
    ) -> Result<Response<wire::SyncIgnoresResponse>, Status> {
        nothing()
    }
    async fn git_config(
        &self,
        _: Request<wire::GitConfigRequest>,
    ) -> Result<Response<wire::GitConfigResponse>, Status> {
        nothing()
    }
    type ListTreeStream = Streamed<wire::ListTreeEntry>;
    async fn list_tree(&self, _: Request<wire::ListTreeRequest>) -> Result<Response<Self::ListTreeStream>, Status> {
        nothing()
    }
    type ReadFileStream = Streamed<wire::ReadFileChunk>;
    async fn read_file(&self, _: Request<wire::ReadFileRequest>) -> Result<Response<Self::ReadFileStream>, Status> {
        nothing()
    }
    async fn replace_file_if(
        &self,
        _: Request<wire::ReplaceFileIfRequest>,
    ) -> Result<Response<wire::ReplaceFileIfResponse>, Status> {
        nothing()
    }
    async fn remove_file_if(
        &self,
        _: Request<wire::RemoveFileIfRequest>,
    ) -> Result<Response<wire::RemoveFileIfResponse>, Status> {
        nothing()
    }
    async fn wait_file_hash(
        &self,
        _: Request<wire::WaitFileHashRequest>,
    ) -> Result<Response<wire::WaitFileHashResponse>, Status> {
        nothing()
    }
    async fn path_sizes(
        &self,
        _: Request<wire::PathSizesRequest>,
    ) -> Result<Response<wire::PathSizesResponse>, Status> {
        nothing()
    }
    async fn project_activity(
        &self,
        _: Request<wire::ProjectActivityRequest>,
    ) -> Result<Response<wire::ProjectActivityResponse>, Status> {
        nothing()
    }
    async fn watch_modes(
        &self,
        _: Request<wire::WatchModesRequest>,
    ) -> Result<Response<wire::WatchModesResponse>, Status> {
        nothing()
    }
    type ExecStream = Streamed<wire::ExecOutput>;
    async fn exec(&self, _: Request<tonic::Streaming<wire::ExecInput>>) -> Result<Response<Self::ExecStream>, Status> {
        nothing()
    }
}

/// What the fake saw on an attachment.
enum Seen {
    Frame(usize, wire::TerminalClientFrame),
}

type ToClient = tokio::sync::mpsc::Sender<Result<wire::TerminalServerFrame, Status>>;

struct Shared {
    opens: Mutex<Vec<wire::OpenTerminalRequest>>,
    closes: Mutex<Vec<wire::CloseTerminalRequest>>,
    attaches: Mutex<Vec<Option<ToClient>>>,
    seen: Mutex<mpsc::Sender<Seen>>,
    refuse_attaches: AtomicBool,
    /// Opens wait for `release_open` before they answer.
    hold_opens: AtomicBool,
    release_open: tokio::sync::Notify,
    /// Closes are recorded and then never answered.
    hang_closes: AtomicBool,
    /// Closes made through the session's own fallback (a connection of their own) rather than the link's.
    fallback_closes: AtomicUsize,
}

struct Fake(Arc<Shared>);

#[tonic::async_trait]
impl wire::terminal_server::Terminal for Fake {
    async fn open_terminal(
        &self,
        request: Request<wire::OpenTerminalRequest>,
    ) -> Result<Response<wire::OpenTerminalResponse>, Status> {
        self.0.opens.lock().unwrap().push(request.into_inner());
        if self.0.hold_opens.load(Ordering::SeqCst) {
            self.0.release_open.notified().await;
        }
        Ok(Response::new(wire::OpenTerminalResponse {
            terminal_id: TERMINAL_ID.into(),
            daemon_instance: "daemon-1".into(),
        }))
    }

    type AttachStream = tokio_stream::wrappers::ReceiverStream<Result<wire::TerminalServerFrame, Status>>;

    async fn attach(
        &self,
        request: Request<tonic::Streaming<wire::TerminalClientFrame>>,
    ) -> Result<Response<Self::AttachStream>, Status> {
        if self.0.refuse_attaches.load(Ordering::SeqCst) {
            return Err(Status::unavailable("the link is down"));
        }
        let (to_client, stream) = tokio::sync::mpsc::channel(16);
        let number = {
            let mut attaches = self.0.attaches.lock().unwrap();
            attaches.push(Some(to_client));
            attaches.len() - 1
        };
        let seen = self.0.seen.lock().unwrap().clone();
        let mut incoming = request.into_inner();
        tokio::spawn(async move {
            while let Ok(Some(frame)) = incoming.message().await {
                let _ = seen.send(Seen::Frame(number, frame));
            }
        });
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(stream)))
    }

    async fn list_terminals(
        &self,
        _: Request<wire::ListTerminalsRequest>,
    ) -> Result<Response<wire::ListTerminalsResponse>, Status> {
        Ok(Response::new(wire::ListTerminalsResponse::default()))
    }

    async fn close_terminal(
        &self,
        request: Request<wire::CloseTerminalRequest>,
    ) -> Result<Response<wire::CloseTerminalResponse>, Status> {
        self.0.closes.lock().unwrap().push(request.into_inner());
        if self.0.hang_closes.load(Ordering::SeqCst) {
            tokio::time::sleep(HANG).await;
        }
        Ok(Response::new(wire::CloseTerminalResponse::default()))
    }
}

// The fixture ---------------------------------------------------------------------------------------------------

/// What the fake has seen so far from the relay's side.
#[derive(Default)]
struct Log {
    hellos: Vec<(usize, wire::AttachHello)>,
    input: Vec<u8>,
    resizes: Vec<wire::TermSize>,
}

struct Fixture {
    served: in_process::InProcess,
    shared: Arc<Shared>,
    seen: mpsc::Receiver<Seen>,
    log: Log,
}

impl Fixture {
    fn new() -> Self {
        let (tx, seen) = mpsc::channel();
        let shared = Arc::new(Shared {
            opens: Mutex::default(),
            closes: Mutex::default(),
            attaches: Mutex::default(),
            seen: Mutex::new(tx),
            refuse_attaches: AtomicBool::new(false),
            hold_opens: AtomicBool::new(false),
            release_open: tokio::sync::Notify::new(),
            hang_closes: AtomicBool::new(false),
            fallback_closes: AtomicUsize::new(0),
        });
        let routes = tonic::service::Routes::new(wire::helper_server::HelperServer::new(StubHelper))
            .add_service(wire::terminal_server::TerminalServer::new(Fake(Arc::clone(&shared))));
        Self {
            served: in_process::serve_routes(routes),
            shared,
            seen,
            log: Log::default(),
        }
    }

    /// A relay session on a new link to the fake, which opens a shell at `/srv/project` at `SIZE`.
    fn session(&self, host: &str, limit: Duration, size: Arc<Mutex<PtySize>>) -> Session {
        let client = self.served.helper_client(HOST, KEY);
        let link = TerminalLink::new(
            client.clone(),
            TerminalLinkOptions {
                installation: String::new(),
                target: TerminalTarget::Open {
                    command_id: "open-command-1".into(),
                    cwd: "/srv/project".into(),
                },
                size: SIZE,
                colors: TerminalColors::default(),
            },
            TerminalFactsHandle::default(),
        )
        .unwrap();
        let shared = Arc::clone(&self.shared);
        Session {
            host: host.into(),
            link,
            close: Box::new(move |id| {
                shared.fallback_closes.fetch_add(1, Ordering::SeqCst);
                close_on(&client, id)
            }),
            reattach_limit: limit,
            size: Box::new(move || Some(*size.lock().unwrap())),
        }
    }

    fn pump(&mut self) {
        while let Ok(Seen::Frame(number, frame)) = self.seen.try_recv() {
            match frame.frame {
                Some(Up::Hello(hello)) => self.log.hellos.push((number, hello)),
                Some(Up::Input(input)) => self.log.input.extend_from_slice(&input.data),
                Some(Up::Resize(size)) => self.log.resizes.push(size),
                _ => {}
            }
        }
    }

    fn until(&mut self, what: &str, done: impl Fn(&Log) -> bool) {
        let started = Instant::now();
        loop {
            self.pump();
            if done(&self.log) {
                return;
            }
            assert!(started.elapsed() < HANG, "waited for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn attached(&mut self, number: usize) {
        self.until("an attachment", |log| log.hellos.iter().any(|(n, _)| *n == number));
    }

    fn serve(&self, number: usize, frame: wire::TerminalServerFrame) {
        let attaches = self.shared.attaches.lock().unwrap();
        attaches[number]
            .as_ref()
            .expect("the attachment is still there")
            .try_send(Ok(frame))
            .expect("the frame is queued");
    }

    /// The link to the server is lost on attachment `number`.
    fn lose(&self, number: usize) {
        let to_client = self.shared.attaches.lock().unwrap()[number].take().unwrap();
        let _ = to_client.try_send(Err(Status::unavailable("the link was lost")));
    }

    fn closes(&self) -> Vec<wire::CloseTerminalRequest> {
        self.shared.closes.lock().unwrap().clone()
    }

    fn fallback_closes(&self) -> usize {
        self.shared.fallback_closes.load(Ordering::SeqCst)
    }

    fn until_opened(&self) {
        let started = Instant::now();
        while self.shared.opens.lock().unwrap().is_empty() {
            assert!(started.elapsed() < HANG, "waited for the open");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn snapshot(offset: u64, replay: &[u8]) -> wire::TerminalServerFrame {
    wire::TerminalServerFrame {
        frame: Some(Down::Snapshot(wire::SnapshotPart {
            offset,
            columns: SIZE.cols.into(),
            rows: SIZE.rows.into(),
            total_bytes: replay.len() as u64,
            replay: replay.to_vec(),
            last: true,
            ..Default::default()
        })),
    }
}

fn output(offset: u64, bytes: &[u8]) -> wire::TerminalServerFrame {
    wire::TerminalServerFrame {
        frame: Some(Down::Output(wire::OutputBytes {
            offset,
            data: bytes.to_vec(),
        })),
    }
}

fn input_written(upto: u64) -> wire::TerminalServerFrame {
    wire::TerminalServerFrame {
        frame: Some(Down::InputWritten(wire::InputWritten { upto })),
    }
}

fn exited(code: Option<i32>, signal: Option<i32>) -> wire::TerminalServerFrame {
    wire::TerminalServerFrame {
        frame: Some(Down::Exited(wire::TerminalExited {
            code,
            signal,
            left: false,
        })),
    }
}

// The person's side ---------------------------------------------------------------------------------------------

/// Everything the relay wrote to the person's terminal.
#[derive(Clone, Default)]
struct Screen(Arc<Mutex<Vec<u8>>>);

impl Screen {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes()).into_owned()
    }

    fn wait(&self, what: &str, done: impl Fn(&str) -> bool) {
        let started = Instant::now();
        while !done(&self.text()) {
            assert!(
                started.elapsed() < HANG,
                "waited for {what}; the screen so far: {:?}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Write for Screen {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: pipe fills two new descriptors.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: both descriptors were just made.
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

fn write_fd(fd: RawFd, bytes: &[u8]) {
    // SAFETY: write from a buffer of that length to a descriptor the test holds.
    let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    assert_eq!(written, bytes.len() as isize);
}

/// A PTY pair: the relay's input and output are the slave; the test reads the master as a terminal emulator would
/// and types into it.
struct Person {
    master: OwnedFd,
    slave: OwnedFd,
    screen: Screen,
}

impl Person {
    fn new() -> Self {
        let (mut master, mut slave) = (0, 0);
        // SAFETY: openpty fills two new descriptors; the optional arguments are null.
        let made = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(made, 0, "a PTY pair");
        // SAFETY: both descriptors were just made.
        let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        let screen = Screen::default();
        let mut reader = std::fs::File::from(master.try_clone().unwrap());
        let sink = screen.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 4096];
            // The read ends with an error once every descriptor of the slave is closed.
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let _ = sink.0.lock().unwrap().write_all(&buf[..n]);
            }
        });
        Self { master, slave, screen }
    }

    fn type_(&self, bytes: &[u8]) {
        write_fd(self.master.as_raw_fd(), bytes);
    }

    /// The slave's local mode flags as the master sees them.
    fn lflag(&self) -> libc::tcflag_t {
        // SAFETY: a zeroed termios is a valid out-parameter.
        let mut attrs: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: tcgetattr on a PTY master reports its slave's attributes.
        assert_eq!(unsafe { libc::tcgetattr(self.master.as_raw_fd(), &mut attrs) }, 0);
        attrs.c_lflag
    }

    /// Waits until the relay has put the terminal in raw mode: keys typed before it would be line-edited.
    fn until_raw(&self) {
        let started = Instant::now();
        while self.lflag() & libc::ICANON != 0 {
            assert!(started.elapsed() < HANG, "waited for raw mode");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn relay(&self, session: Session, signals: OwnedFd) -> JoinHandle<i32> {
        let input = self.slave.as_raw_fd();
        let output = std::fs::File::from(self.slave.try_clone().unwrap());
        std::thread::spawn(move || {
            term::run(
                session,
                Terminal {
                    input,
                    output: Box::new(output),
                    output_is_terminal: true,
                    signals,
                },
            )
        })
    }
}

/// The relay with a pipe for input (end of input is the write end's close) and a [`Screen`] for output.
fn piped_relay(session: Session, input: RawFd, signals: OwnedFd, screen: &Screen) -> JoinHandle<i32> {
    let output = screen.clone();
    std::thread::spawn(move || {
        term::run(
            session,
            Terminal {
                input,
                output: Box::new(output),
                output_is_terminal: false,
                signals,
            },
        )
    })
}

/// What the first snapshot is drawn over, and every later one.
const FIRST_RESET: &str = "\x18\x1bc";
const LATER_RESET: &str = "\x18\x1bc\x1b[3J";

fn sizes(cols: u16, rows: u16) -> Arc<Mutex<PtySize>> {
    Arc::new(Mutex::new(PtySize {
        cols,
        rows,
        cell_width_px: 0,
        cell_height_px: 0,
    }))
}

// The tests -----------------------------------------------------------------------------------------------------

#[test]
fn the_snapshot_comes_before_live_output() {
    let mut fx = Fixture::new();
    let person = Person::new();
    let (signals, _keep) = pipe();
    let relay = person.relay(fx.session(HOST, term::REATTACH_LIMIT, sizes(100, 30)), signals);
    fx.attached(0);
    let (number, hello) = fx.log.hellos[0].clone();
    assert_eq!((number, hello.terminal_id.as_str()), (0, TERMINAL_ID));
    {
        let opens = fx.shared.opens.lock().unwrap();
        assert_eq!(opens.len(), 1);
        assert_eq!(opens[0].cwd, "/srv/project");
        assert_eq!(
            opens[0].size.as_ref().map(|size| (size.columns, size.rows)),
            Some((100, 30))
        );
    }
    // The screen, its unfinished tail, then what the shell prints from there on, byte for byte.
    fx.serve(0, snapshot(0, b"\x1b[Hprompt$ "));
    person.screen.wait("the snapshot", |text| text.contains("prompt$ "));
    fx.serve(0, output(0, b"live"));
    person.screen.wait("live output", |text| text.contains("live"));
    // The terminal is reset first: the replay is made for a terminal in its first state.
    assert_eq!(person.screen.bytes(), b"\x18\x1bc\x1b[Hprompt$ live");
    // What is typed goes to the shell as typed (the terminal is raw: no echo, no line editing).
    person.type_(b"ls\r");
    fx.until("the typed input", |log| log.input == b"ls\r");
    // The shell's exit is the relay's.
    fx.serve(0, exited(Some(3), None));
    assert_eq!(relay.join().unwrap(), 3);
}

#[test]
fn resize_follows_sigwinch() {
    let mut fx = Fixture::new();
    let person = Person::new();
    let (signals, wake) = pipe();
    let size = sizes(100, 30);
    let relay = person.relay(fx.session(HOST, term::REATTACH_LIMIT, size.clone()), signals);
    fx.attached(0);
    fx.serve(0, snapshot(0, b"$ "));
    person.screen.wait("the snapshot", |text| text.contains("$ "));
    *size.lock().unwrap() = PtySize {
        cols: 132,
        rows: 43,
        cell_width_px: 0,
        cell_height_px: 0,
    };
    write_fd(wake.as_raw_fd(), &[libc::SIGWINCH as u8]);
    fx.until("the resize", |log| {
        log.resizes.iter().any(|size| (size.columns, size.rows) == (132, 43))
    });
    assert!(fx.closes().is_empty(), "a resize closes nothing");
    write_fd(wake.as_raw_fd(), &[libc::SIGTERM as u8]);
    assert_eq!(relay.join().unwrap(), 128 + libc::SIGTERM);
}

#[test]
fn eof_closes_the_server_terminal() {
    let mut fx = Fixture::new();
    let (input, end_of_input) = pipe();
    let (signals, _keep) = pipe();
    let screen = Screen::default();
    let session = fx.session(HOST, term::REATTACH_LIMIT, sizes(100, 30));
    let relay = {
        let (output, input) = (screen.clone(), input.as_raw_fd());
        std::thread::spawn(move || {
            term::run(
                session,
                Terminal {
                    input,
                    output: Box::new(output),
                    output_is_terminal: false,
                    signals,
                },
            )
        })
    };
    fx.attached(0);
    fx.serve(0, snapshot(0, b"$ "));
    screen.wait("the snapshot", |text| text.contains("$ "));
    write_fd(end_of_input.as_raw_fd(), b"exit\r");
    fx.until("the input", |log| log.input == b"exit\r");
    drop(end_of_input);
    fx.serve(0, input_written(5));
    assert_eq!(relay.join().unwrap(), 0);
    let closes = fx.closes();
    assert_eq!(closes.len(), 1, "one close");
    assert_eq!(closes[0].terminal_id, TERMINAL_ID);
    assert_eq!(fx.fallback_closes(), 0, "the close goes over the link's own connection");
    drop(input);
}

#[test]
fn a_hangup_closes_the_server_terminal_too() {
    let mut fx = Fixture::new();
    let (input, _keep_input) = pipe();
    let (signals, wake) = pipe();
    let screen = Screen::default();
    let session = fx.session(HOST, term::REATTACH_LIMIT, sizes(100, 30));
    let relay = {
        let (output, input) = (screen.clone(), input.as_raw_fd());
        std::thread::spawn(move || {
            term::run(
                session,
                Terminal {
                    input,
                    output: Box::new(output),
                    output_is_terminal: false,
                    signals,
                },
            )
        })
    };
    fx.attached(0);
    fx.serve(0, snapshot(0, b"$ "));
    screen.wait("the snapshot", |text| text.contains("$ "));
    write_fd(wake.as_raw_fd(), &[libc::SIGHUP as u8]);
    assert_eq!(relay.join().unwrap(), 128 + libc::SIGHUP);
    assert_eq!(fx.closes().len(), 1);
    let text = screen.text();
    assert!(
        !text.contains("\x1b[!p"),
        "no terminal on the output, no modes to give back: {text:?}"
    );
    // Not a new connection: on a buffer wipe the whole process group (ssh included) is killed two seconds on.
    assert_eq!(fx.fallback_closes(), 0, "the close goes over the link's own connection");
}

#[test]
fn a_lost_link_reattaches_with_a_fresh_snapshot() {
    let mut fx = Fixture::new();
    let person = Person::new();
    let (signals, _keep) = pipe();
    let relay = person.relay(fx.session(HOST, term::REATTACH_LIMIT, sizes(100, 30)), signals);
    fx.attached(0);
    fx.serve(0, snapshot(0, b"one"));
    person.screen.wait("the first screen", |text| text.contains("one"));
    fx.lose(0);
    // The link comes back on its own schedule, to the same terminal, with the size asked for, and a screen.
    fx.attached(1);
    assert_eq!(fx.log.hellos[1].1.terminal_id, TERMINAL_ID);
    assert_eq!(fx.shared.opens.lock().unwrap().len(), 1, "the shell is opened once");
    fx.serve(1, snapshot(40, b"two"));
    person
        .screen
        .wait("the second screen", |text| text.contains(&format!("{LATER_RESET}two")));
    let text = person.screen.text();
    assert!(text.starts_with(&format!("{FIRST_RESET}one")), "{text:?}");
    assert_eq!(text.matches("\x1bc").count(), 2, "a reset before each screen: {text:?}");
    assert_eq!(
        text.matches("\x1b[3J").count(),
        1,
        "the scrollback is erased before the second screen only: {text:?}"
    );
    assert!(text.find("one").unwrap() < text.find(LATER_RESET).unwrap());
    // The link's own line about the loss is a line of the relay's, not bare text on stderr.
    assert!(text.contains("\x18\r\n[remote terminal example-host]"), "{text:?}");
    // Live output continues from the new snapshot, and the old stream's offsets no longer apply.
    fx.serve(1, output(40, b"!"));
    person.screen.wait("live output", |text| text.ends_with("two!"));
    fx.serve(1, exited(Some(0), None));
    assert_eq!(relay.join().unwrap(), 0);
}

#[test]
fn after_120_s_it_exits_255_and_leaves_the_shell() {
    assert_eq!(term::REATTACH_LIMIT, Duration::from_secs(120));
    let mut fx = Fixture::new();
    let person = Person::new();
    let (signals, _keep) = pipe();
    // The limit is shortened for the test; the server refuses every attachment from the loss on.
    let relay = person.relay(fx.session(HOST, Duration::from_millis(300), sizes(100, 30)), signals);
    fx.attached(0);
    fx.serve(0, snapshot(0, b"$ "));
    person.screen.wait("the screen", |text| text.contains("$ "));
    fx.shared.refuse_attaches.store(true, Ordering::SeqCst);
    fx.lose(0);
    assert_eq!(relay.join().unwrap(), 255);
    // The PTY's reader may still be behind the relay's last write.
    person.screen.wait("the relay's last line", |text| {
        text.contains("the shell on example-host keeps running; `eitri host list` shows it")
    });
    assert!(fx.closes().is_empty(), "the shell is left running, not closed");
}

#[test]
fn a_hostile_host_name_is_escaped_in_the_relays_own_lines() {
    let mut fx = Fixture::new();
    let person = Person::new();
    let (signals, _keep) = pipe();
    let relay = person.relay(
        fx.session("ex\u{1b}[31mhost\u{7}", Duration::from_millis(300), sizes(100, 30)),
        signals,
    );
    fx.attached(0);
    fx.serve(0, snapshot(0, b"$ "));
    person.screen.wait("the screen", |text| text.contains("$ "));
    fx.shared.refuse_attaches.store(true, Ordering::SeqCst);
    fx.lose(0);
    assert_eq!(relay.join().unwrap(), 255);
    person.screen.wait("the relay's last line", |text| {
        text.contains("`eitri host list` shows it")
    });
    let text = person.screen.text();
    assert!(text.contains("ex\\u{1b}[31mhost\\u{7}"), "{text:?}");
    // The relay's own lines, after the screen it drew (which starts with its reset) and the modes it gave back.
    let reset = String::from_utf8(term::MODES_RESET.to_vec()).unwrap();
    let lines = text[text.find("$ ").unwrap()..].replace(&reset, "");
    assert!(!lines.contains('\u{1b}'), "{lines:?}");
    assert!(!lines.contains('\u{7}'), "{lines:?}");
}

#[test]
fn output_that_skips_bytes_ends_the_relay_and_leaves_the_shell() {
    let mut fx = Fixture::new();
    let person = Person::new();
    let (signals, _keep) = pipe();
    let relay = person.relay(fx.session(HOST, term::REATTACH_LIMIT, sizes(100, 30)), signals);
    fx.attached(0);
    fx.serve(0, snapshot(0, b"$ "));
    person.screen.wait("the screen", |text| text.contains("$ "));
    fx.serve(0, output(7, b"not the next byte"));
    assert_eq!(relay.join().unwrap(), 255);
    person.screen.wait("the relay's last line", |text| {
        text.contains("keeps running; `eitri host list` shows it")
    });
    let text = person.screen.text();
    assert!(!text.contains("not the next byte"), "{text:?}");
    assert!(fx.closes().is_empty());
}

#[test]
fn a_shell_ended_by_a_signal_exits_128_plus_it_and_nothing_is_closed() {
    let mut fx = Fixture::new();
    let person = Person::new();
    let (signals, _keep) = pipe();
    let relay = person.relay(fx.session(HOST, term::REATTACH_LIMIT, sizes(100, 30)), signals);
    fx.attached(0);
    fx.serve(0, snapshot(0, b"$ "));
    person.screen.wait("the screen", |text| text.contains("$ "));
    fx.serve(0, exited(None, Some(9)));
    assert_eq!(relay.join().unwrap(), 137);
    assert!(fx.closes().is_empty(), "an ended shell has nothing to close");
    person.screen.wait("the relay's last line", |text| {
        text.contains("the shell on example-host ended with signal 9")
    });
    let text = person.screen.text();
    assert!(
        !text.contains("\x1b[!p"),
        "the shell's own end leaves its modes as it left them: {text:?}"
    );
}

#[test]
fn the_terminal_is_raw_while_it_runs_and_given_back_after() {
    let mut fx = Fixture::new();
    let person = Person::new();
    let before = person.lflag();
    assert!(
        before & (libc::ICANON | libc::ECHO) == libc::ICANON | libc::ECHO,
        "a fresh PTY is cooked"
    );
    let (signals, _keep) = pipe();
    let relay = person.relay(fx.session(HOST, term::REATTACH_LIMIT, sizes(100, 30)), signals);
    fx.attached(0);
    fx.serve(0, snapshot(0, b"$ "));
    person.screen.wait("the screen", |text| text.contains("$ "));
    assert_eq!(
        person.lflag() & (libc::ICANON | libc::ECHO | libc::ISIG),
        0,
        "raw while it runs"
    );
    fx.serve(0, exited(Some(0), None));
    assert_eq!(relay.join().unwrap(), 0);
    assert_eq!(person.lflag(), before, "the terminal's mode is given back");
}

#[test]
fn a_resync_on_the_same_attachment_is_drawn_over_a_reset_and_an_erased_scrollback() {
    let mut fx = Fixture::new();
    let person = Person::new();
    let (signals, _keep) = pipe();
    let relay = person.relay(fx.session(HOST, term::REATTACH_LIMIT, sizes(100, 30)), signals);
    fx.attached(0);
    fx.serve(0, snapshot(0, b"history\r\none"));
    person.screen.wait("the first screen", |text| text.contains("one"));
    // The server's queue for this client overflowed: it sends a fresh screen, its history included.
    fx.serve(0, snapshot(900, b"history\r\ntwo"));
    person.screen.wait("the second screen", |text| {
        text.contains(&format!("{LATER_RESET}history\r\ntwo"))
    });
    fx.serve(0, output(900, b"!"));
    person.screen.wait("live output", |text| text.ends_with("two!"));
    fx.serve(0, exited(Some(0), None));
    assert_eq!(relay.join().unwrap(), 0);
}

#[test]
fn output_on_a_new_attachment_before_its_snapshot_ends_the_relay() {
    let mut fx = Fixture::new();
    let person = Person::new();
    let (signals, _keep) = pipe();
    let relay = person.relay(fx.session(HOST, term::REATTACH_LIMIT, sizes(100, 30)), signals);
    fx.attached(0);
    fx.serve(0, snapshot(0, b"one"));
    fx.serve(0, output(0, b"ab"));
    person.screen.wait("the first screen", |text| text.contains("oneab"));
    fx.lose(0);
    fx.attached(1);
    // It would continue the old stream exactly, but a new attachment's offsets start at its snapshot.
    fx.serve(1, output(2, b"stale"));
    assert_eq!(relay.join().unwrap(), 255);
    person.screen.wait("the relay's last line", |text| {
        text.contains("keeps running; `eitri host list` shows it")
    });
    let text = person.screen.text();
    assert!(!text.contains("stale"), "{text:?}");
    assert!(fx.closes().is_empty());
}

#[test]
fn typed_bytes_lost_in_a_reattach_are_said_when_the_relay_ends() {
    let mut fx = Fixture::new();
    let person = Person::new();
    let (signals, _keep) = pipe();
    let relay = person.relay(fx.session(HOST, term::REATTACH_LIMIT, sizes(100, 30)), signals);
    fx.attached(0);
    fx.serve(0, snapshot(0, b"one"));
    person.screen.wait("the first screen", |text| text.contains("one"));
    person.type_(b"abc");
    fx.until("the typed input", |log| log.input == b"abc");
    // The host never wrote them: the next snapshot says it wrote nothing of this client's input.
    fx.lose(0);
    fx.attached(1);
    assert_eq!(fx.log.hellos[1].1.input_sent, 3);
    fx.serve(1, snapshot(40, b"two"));
    fx.serve(1, output(40, b"!"));
    person.screen.wait("the second screen", |text| text.contains("two!"));
    // Not over the screen: the person's terminal would no longer show what the server's does.
    let line = "3 typed byte(s) did not reach the shell";
    assert!(!person.screen.text().contains(line), "{:?}", person.screen.text());
    fx.serve(1, exited(Some(0), None));
    assert_eq!(relay.join().unwrap(), 0);
    person.screen.wait("the line", |text| text.contains(line));
    let text = person.screen.text();
    assert!(text.rfind("two!").unwrap() < text.rfind(line).unwrap(), "{text:?}");
}

#[test]
fn typeahead_before_the_first_screen_reaches_the_shell() {
    let mut fx = Fixture::new();
    let person = Person::new();
    let (signals, _keep) = pipe();
    let relay = person.relay(fx.session(HOST, term::REATTACH_LIMIT, sizes(100, 30)), signals);
    person.until_raw();
    person.type_(b"ls\r");
    fx.attached(0);
    fx.serve(0, snapshot(0, b"$ "));
    fx.until("the typeahead", |log| log.input == b"ls\r");
    fx.serve(0, exited(Some(0), None));
    assert_eq!(relay.join().unwrap(), 0);
}

/// Bytes waiting to be read on `fd`.
fn unread(fd: RawFd) -> libc::c_int {
    let mut waiting: libc::c_int = 0;
    // SAFETY: FIONREAD writes an int through the pointer.
    assert_eq!(unsafe { libc::ioctl(fd, libc::FIONREAD as _, &mut waiting) }, 0);
    waiting
}

#[test]
fn end_of_input_before_the_first_screen_hands_over_what_was_typed_then_closes() {
    let mut fx = Fixture::new();
    let (input, end_of_input) = pipe();
    let (signals, _keep) = pipe();
    let screen = Screen::default();
    let relay = piped_relay(
        fx.session(HOST, term::REATTACH_LIMIT, sizes(100, 30)),
        input.as_raw_fd(),
        signals,
        &screen,
    );
    fx.attached(0);
    // Typed and ended before the screen, and read by the relay (the end comes with the same read or the next).
    write_fd(end_of_input.as_raw_fd(), b"echo hi\r");
    drop(end_of_input);
    let started = Instant::now();
    while unread(input.as_raw_fd()) > 0 {
        assert!(started.elapsed() < HANG, "waited for the relay to read");
        std::thread::sleep(Duration::from_millis(5));
    }
    fx.serve(0, snapshot(0, b"$ "));
    fx.until("the input", |log| log.input == b"echo hi\r");
    fx.serve(0, input_written(8));
    assert_eq!(relay.join().unwrap(), 0);
    assert_eq!(fx.closes().len(), 1, "closed once what was typed was written");
    drop(input);
}

#[test]
fn end_of_input_closes_only_once_the_server_wrote_what_was_typed() {
    let mut fx = Fixture::new();
    let (input, end_of_input) = pipe();
    let (signals, _keep) = pipe();
    let screen = Screen::default();
    let relay = piped_relay(
        fx.session(HOST, term::REATTACH_LIMIT, sizes(100, 30)),
        input.as_raw_fd(),
        signals,
        &screen,
    );
    fx.attached(0);
    fx.serve(0, snapshot(0, b"$ "));
    screen.wait("the snapshot", |text| text.contains("$ "));
    write_fd(end_of_input.as_raw_fd(), b"make\n");
    drop(end_of_input);
    fx.until("the input", |log| log.input == b"make\n");
    // On the stream but not yet written: a close sent now could reach the server first. Well within the drain.
    std::thread::sleep(Duration::from_millis(300));
    assert!(fx.closes().is_empty(), "closed before the server wrote the input");
    fx.serve(0, input_written(5));
    assert_eq!(relay.join().unwrap(), 0);
    assert_eq!(fx.closes().len(), 1, "closed once it was written");
    drop(input);
}

#[test]
fn end_of_input_with_typeahead_and_no_screen_closes_after_the_drain() {
    assert!(term::INPUT_DRAIN < Duration::from_secs(30));
    let fx = Fixture::new();
    let (input, end_of_input) = pipe();
    write_fd(end_of_input.as_raw_fd(), b"ls\r");
    drop(end_of_input);
    let (signals, _keep) = pipe();
    let screen = Screen::default();
    // The shell is opened and attached to, but no screen ever comes; the limit for one is far past the drain.
    let relay = piped_relay(
        fx.session(HOST, Duration::from_secs(30), sizes(100, 30)),
        input.as_raw_fd(),
        signals,
        &screen,
    );
    assert_eq!(relay.join().unwrap(), 0, "{:?}", screen.text());
    let text = screen.text();
    assert!(text.contains("3 typed byte(s) did not reach the shell"), "{text:?}");
    assert_eq!(fx.closes().len(), 1, "the shell is closed, not left running: {text:?}");
    drop(input);
}

#[test]
fn the_modes_are_given_back_when_the_relay_ends_with_the_program_running() {
    let mut fx = Fixture::new();
    let person = Person::new();
    let (signals, _keep) = pipe();
    let relay = person.relay(fx.session(HOST, Duration::from_millis(300), sizes(100, 30)), signals);
    fx.attached(0);
    // A full-screen program: the alternate screen, mouse reporting, bracketed paste, keyboard flags.
    fx.serve(0, snapshot(0, b"\x1b[?1049h\x1b[?1000h\x1b[?2004h\x1b[>1uapp"));
    person.screen.wait("the screen", |text| text.contains("app"));
    fx.shared.refuse_attaches.store(true, Ordering::SeqCst);
    fx.lose(0);
    assert_eq!(relay.join().unwrap(), 255);
    let last = "the shell on example-host keeps running";
    person.screen.wait("the relay's last line", |text| text.contains(last));
    let text = person.screen.text();
    let reset = String::from_utf8(term::MODES_RESET.to_vec()).unwrap();
    let at = text
        .find(&reset)
        .unwrap_or_else(|| panic!("the modes are given back: {text:?}"));
    assert!(
        text.find("app").unwrap() < at && at < text.find(last).unwrap(),
        "{text:?}"
    );
    assert_eq!(text.matches(&reset).count(), 1, "once: {text:?}");
}

/// What a stop while connecting leaves: [`stop_while_connecting_child`] in a process of its own (the handlers are the
/// process's), the connection made (`connected`) or failed by it (`refused`).
fn stop_while_connecting(how: &str) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "stop_while_connecting_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("EITRI_TEST_TERM_STOP", how)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let (stdout, stderr) = (
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        output.status.success() && stdout.contains("stopped code=130 opens=0 closes=0"),
        "{:?}\n{stdout}{stderr}",
        output.status
    );
    assert!(
        stderr
            .contains("eitri host term: stopped (signal 2) while example-host was being reached; nothing was started"),
        "{stderr}"
    );
    // The relay never ran: it would have had a shell opened, and said one may have been started.
    assert!(!stdout.contains("a shell may have been started"), "{stdout}");
}

#[test]
fn a_ctrl_c_while_connecting_starts_no_shell() {
    stop_while_connecting("connected");
}

#[test]
fn a_ctrl_c_that_fails_the_connection_is_a_stop_not_an_unreachable_host() {
    stop_while_connecting("refused");
}

/// A `Reach` whose connection is made (or refused) while the person presses Ctrl-C.
struct StopReach<'a> {
    fx: &'a Fixture,
    refuse: bool,
}

impl Reach for StopReach<'_> {
    fn connect(&mut self, _host: &str) -> Result<String, ConnectError> {
        unreachable!("term connects through exec_connection")
    }
    fn first_contact(&mut self, _host: &str) -> Result<Contact, String> {
        unreachable!()
    }
    fn launcher(&self, _helper: &std::path::Path) -> Arc<dyn ChannelLauncher> {
        unreachable!()
    }
    fn plan(&mut self) -> Result<InstallPlan, String> {
        unreachable!()
    }
    fn agents(&mut self, _host: &str, _key: &str, _helper: &std::path::Path) -> Result<Vec<Agent>, String> {
        unreachable!()
    }
    fn exec_connection(&mut self, project: &RemoteProject) -> Result<RemoteConnection, String> {
        // SAFETY: a signal to this thread, whose handler only writes to a pipe; it has run when raise returns.
        assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0);
        if self.refuse {
            return Err("ssh: interrupted".into());
        }
        let client = self.fx.served.helper_client(HOST, KEY);
        Ok(RemoteConnection::with_client(
            client,
            project.clone(),
            String::new(),
            String::new(),
        ))
    }
}

#[derive(Default)]
struct Lines;

impl cli::Terminal for Lines {
    fn say(&mut self, _line: &str) {}
    fn ask(&mut self, _question: &str) -> bool {
        unreachable!("term asks nothing")
    }
}

#[test]
fn stop_while_connecting_child() {
    let Some(how) = std::env::var_os("EITRI_TEST_TERM_STOP") else {
        return;
    };
    let fx = Fixture::new();
    let state = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("term-stop-{}", std::process::id()));
    agent::private_fs::create_private_dir_all(&state, &state).unwrap();
    let store = RemoteProjects::new(Some(state.as_os_str()), None);
    store
        .save(&RemoteProject {
            version: 1,
            host: HOST.into(),
            host_key_sha256: KEY.into(),
            root: "/srv/project".into(),
            helper_version: "0.4.0-example".into(),
            sync: None,
            created_at: 0,
            queued_stops: vec![],
        })
        .unwrap();
    let mut reach = StopReach {
        fx: &fx,
        refuse: how == "refused",
    };
    let enabled = || true;
    let code = eitri_core::remote_host::cli_term::run_term(
        &["--cwd".to_owned(), "/srv/project".to_owned()],
        &mut Context {
            store: &store,
            reach: &mut reach,
            term: &mut Lines,
            remote_dev_enabled: &enabled,
            now: 0,
        },
    );
    let _ = std::fs::remove_dir_all(&state);
    println!(
        "stopped code={code} opens={} closes={}",
        fx.shared.opens.lock().unwrap().len(),
        fx.closes().len()
    );
}

#[test]
fn end_of_input_before_the_open_is_answered_waits_for_it_and_closes_the_shell() {
    let fx = Fixture::new();
    fx.shared.hold_opens.store(true, Ordering::SeqCst);
    let (input, end_of_input) = pipe();
    drop(end_of_input);
    let (signals, _keep) = pipe();
    let screen = Screen::default();
    let relay = piped_relay(
        fx.session(HOST, term::REATTACH_LIMIT, sizes(100, 30)),
        input.as_raw_fd(),
        signals,
        &screen,
    );
    fx.until_opened();
    fx.shared.release_open.notify_one();
    assert_eq!(relay.join().unwrap(), 0);
    let closes = fx.closes();
    assert_eq!(
        closes.len(),
        1,
        "the shell the open started is closed: {:?}",
        screen.text()
    );
    assert_eq!(closes[0].terminal_id, TERMINAL_ID);
    drop(input);
}

#[test]
fn an_open_never_answered_is_said_to_maybe_have_started_a_shell() {
    let fx = Fixture::new();
    fx.shared.hold_opens.store(true, Ordering::SeqCst);
    let (input, _keep_input) = pipe();
    let (signals, wake) = pipe();
    let screen = Screen::default();
    let relay = piped_relay(
        fx.session(HOST, term::REATTACH_LIMIT, sizes(100, 30)),
        input.as_raw_fd(),
        signals,
        &screen,
    );
    fx.until_opened();
    write_fd(wake.as_raw_fd(), &[libc::SIGHUP as u8]);
    assert_eq!(relay.join().unwrap(), 128 + libc::SIGHUP);
    let text = screen.text();
    assert!(
        text.contains("a shell may have been started on example-host; `eitri host list` shows it"),
        "{text:?}"
    );
    assert!(fx.closes().is_empty());
}

#[test]
fn a_close_the_server_does_not_answer_is_given_up_and_said() {
    let mut fx = Fixture::new();
    fx.shared.hang_closes.store(true, Ordering::SeqCst);
    let (input, _keep_input) = pipe();
    let (signals, wake) = pipe();
    let screen = Screen::default();
    let relay = piped_relay(
        fx.session(HOST, term::REATTACH_LIMIT, sizes(100, 30)),
        input.as_raw_fd(),
        signals,
        &screen,
    );
    fx.attached(0);
    fx.serve(0, snapshot(0, b"$ "));
    screen.wait("the snapshot", |text| text.contains("$ "));
    write_fd(wake.as_raw_fd(), &[libc::SIGHUP as u8]);
    assert_eq!(relay.join().unwrap(), 128 + libc::SIGHUP);
    let text = screen.text();
    assert!(
        text.contains("the shell on example-host could not be closed") && text.contains("it may keep running"),
        "{text:?}"
    );
    assert_eq!(fx.closes().len(), 1, "the close was sent");
    assert_eq!(fx.fallback_closes(), 0);
}

/// The child half of [`resize_follows_a_real_sigwinch`]: the relay with the real handlers, in a process of its own
/// (they are the process's).
#[test]
fn sigwinch_child() {
    if std::env::var_os("EITRI_TEST_TERM_SIGNAL").is_none() {
        return;
    }
    let mut fx = Fixture::new();
    let person = Person::new();
    let signals = eitri_core::remote_lsp::signals::watch(&[libc::SIGWINCH, libc::SIGTERM]).unwrap();
    assert_eq!(
        eitri_core::remote_lsp::signals::watch(&[libc::SIGWINCH])
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AlreadyExists,
        "the handlers' pipe is taken once"
    );
    let size = sizes(100, 30);
    let relay = person.relay(fx.session(HOST, term::REATTACH_LIMIT, size.clone()), signals);
    fx.attached(0);
    fx.serve(0, snapshot(0, b"$ "));
    person.screen.wait("the snapshot", |text| text.contains("$ "));
    *size.lock().unwrap() = PtySize {
        cols: 132,
        rows: 43,
        cell_width_px: 0,
        cell_height_px: 0,
    };
    // SAFETY: a signal to this process, whose handler only writes to a pipe.
    assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGWINCH) }, 0);
    fx.until("the resize", |log| {
        log.resizes.iter().any(|size| (size.columns, size.rows) == (132, 43))
    });
    // SAFETY: as above.
    assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGTERM) }, 0);
    assert_eq!(relay.join().unwrap(), 128 + libc::SIGTERM);
    assert_eq!(fx.closes().len(), 1);
    println!("resized-by-signal");
}

#[test]
fn resize_follows_a_real_sigwinch() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "sigwinch_child", "--nocapture", "--test-threads=1"])
        .env("EITRI_TEST_TERM_SIGNAL", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("resized-by-signal"),
        "{:?}\n{stdout}{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}
