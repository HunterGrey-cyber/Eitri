//! Whether one Claude sidecar serves a session to two client connections, and outlives the process
//! that spawned it while another process holds a copy of the sidecar's stdin.
//!
//! Real sidecar, real `claude`, real tokens, so `#[ignore]`d. Run it under the test profile, which
//! pins both the account and the CLI binary the sidecar starts:
//! `cargo test -p agent --test probe_pc0 -- --ignored --nocapture`.
//!
//! Three clauses, each printed with its own verdict before the test asserts on all of them:
//! - **second watcher**: while one connection watches a session, a second connection that asks for
//!   the events after a sequence of its choosing is sent exactly those events, in order and equal to
//!   what the first connection saw, a pending permission request among them;
//! - **answer from another connection**: that permission, raised while only the first connection
//!   watched, is answered from the second; the first connection is then closed, and the second sees
//!   the tool call run and the turn complete;
//! - **stdin held elsewhere**: the spawner closes its copy of the sidecar's stdin after handing a
//!   duplicate to a helper process; the sidecar keeps serving, and once the helper exits it exits
//!   within `SIDECAR_EXIT_GRACE`.
//!
//! The sidecar is started the way `ClaudeSidecarProvider::connect` starts one -- the same program
//! resolution order, the same scrubbed environment, the socket in a private directory, the peer's
//! uid checked on every connection, stdin held open as the sidecar's parent-death signal -- but by
//! this file, because the probe needs the stdin write end and two independent connections, neither
//! of which the provider hands out.
#![cfg(target_os = "linux")]

use claude_runtime_protocol::v1::runtime_service_client::RuntimeServiceClient;
use claude_runtime_protocol::v1::session_event::Event;
use claude_runtime_protocol::v1::{
    ClaudeHostPolicy, CliPermissionMode, ConfigurationProfile, CreateSessionRequest, ExecutableSource,
    HandshakeRequest, HandshakeResponse, PermissionMode, PermissionOutcome, PermissionRequested, PersistenceMode,
    ReplayStart, ResolvePermissionRequest, SendTurnRequest, SessionEvent, SettingSourceSelection, StreamingMode,
    ToolPolicy, TurnOutcome, WatchSessionEventsRequest,
};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tonic::transport::{Channel, Endpoint, Uri};

/// Set to `1` in the environment of the helper process this file starts; nothing else sets it.
const HELPER_VAR: &str = "PC0_HELPER";
/// The test the helper process runs: the half of the stdin hand-over that holds the duplicate.
const HELPER_TEST: &str = "stdin_holder_helper";

/// How the sidecar learns its parent is gone, its fault-injection knobs and its extra MCP servers:
/// none of them may leak in from the shell that runs the probe, exactly as for a window's sidecar.
const SCRUBBED_VARS: [&str; 5] = [
    "VERDANDI_CLAUDE_SIDECAR_PARENT_WATCH",
    "VERDANDI_CLAUDE_EXTRA_MCP_SERVERS",
    "VERDANDI_CLAUDE_SIDECAR_RING_CAPACITY",
    "VERDANDI_CLAUDE_SIDECAR_FAULT_DROP_WATCH_AFTER",
    "VERDANDI_CLAUDE_SIDECAR_WRITE_QUEUE_STATS_MS",
];

/// The installed name of a sidecar artifact, and where a Verdandi checkout keeps the ones it built.
const SIDECAR_BINARY: &str = "verdandi-claude-sidecar";
const CHECKOUT_ARTIFACT_DIR: &str = "apps/claude-sidecar/dist-bin";
const DEFAULT_CHECKOUT_UNDER_HOME: &str = "src/verdandi";

/// How long a slow sidecar start (cold page cache, a launcher script for the CLI) may take to bind.
const BIND_BUDGET: Duration = Duration::from_secs(15);
/// A single unary call on a local socket.
const RPC_BUDGET: Duration = Duration::from_secs(10);
/// From the turn's start to the model asking for its one tool call.
const PERMISSION_BUDGET: Duration = Duration::from_secs(180);
/// A replay of a handful of buffered events over a local socket.
const REPLAY_BUDGET: Duration = Duration::from_secs(30);
/// From the answer to the end of the turn: the tool runs, then the model writes a short reply.
const TURN_END_BUDGET: Duration = Duration::from_secs(240);
/// How long the sidecar must keep serving after the spawner's copy of its stdin is closed.
const HELD_ALIVE_FOR: Duration = Duration::from_secs(2);

const PROMPT: &str = "Use the Bash tool to run exactly this command in the current directory: touch probe-file\n\
                      Run nothing else. After it has run, reply with the single word done.";

// ---------------------------------------------------------------------------------------------
// Passing one descriptor over a unix socket.
// ---------------------------------------------------------------------------------------------

/// Room for one control message carrying one descriptor, aligned for `cmsghdr`.
type ControlBuffer = [u64; 8];

fn send_fd(socket: RawFd, fd: RawFd) -> std::io::Result<()> {
    let mut byte = [b'F'];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: byte.len(),
    };
    let mut control: ControlBuffer = [0; 8];
    // SAFETY: `CMSG_SPACE` only computes a size.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    assert!(
        space <= std::mem::size_of_val(&control),
        "one descriptor's control message fits the buffer"
    );
    // SAFETY: an all-zero `msghdr` is a valid empty message; every pointer set below outlives the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: `msg_control` points at `space` writable, aligned bytes, so the first header exists
    // and its data has room for one descriptor.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>(), fd);
    }
    // SAFETY: `msg` is fully initialised above.
    let sent = unsafe { libc::sendmsg(socket, &msg, libc::MSG_NOSIGNAL) };
    if sent < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn recv_fd(socket: RawFd) -> std::io::Result<OwnedFd> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: byte.len(),
    };
    let mut control: ControlBuffer = [0; 8];
    // SAFETY: as in `send_fd`.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&control) as _;
    // SAFETY: `msg` points at buffers that live for the call. The received descriptor is
    // close-on-exec so nothing this process might start inherits it.
    let received = unsafe { libc::recvmsg(socket, &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if received < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if received == 0 {
        return Err(std::io::Error::other("the socket closed before a descriptor arrived"));
    }
    // SAFETY: `recvmsg` filled `msg`, so its control headers are walkable; the data of an
    // `SCM_RIGHTS` header is a descriptor this process now owns.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() || (*cmsg).cmsg_level != libc::SOL_SOCKET || (*cmsg).cmsg_type != libc::SCM_RIGHTS {
            return Err(std::io::Error::other("the message carried no descriptor"));
        }
        let fd = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>());
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

/// The helper process: receives the sidecar's stdin write end on its own stdin (a unix socket),
/// says so, holds the descriptor until the probe writes to or closes that socket, then exits.
/// Run with nothing in `PC0_HELPER`, it does nothing, so a plain `--ignored` run passes it.
#[test]
#[ignore = "the helper half of the stdin hand-over; started by the probe below, never on its own"]
fn stdin_holder_helper() {
    if std::env::var_os(HELPER_VAR).is_none_or(|value| value != "1") {
        return;
    }
    let held = recv_fd(0).expect("the helper receives the sidecar's stdin write end on its stdin");
    // SAFETY: fd 0 is the socket the probe connected this process's stdin to, and nothing else in
    // this process uses it.
    let mut control = unsafe { StdUnixStream::from_raw_fd(0) };
    control
        .write_all(b"R")
        .expect("the helper says it holds the descriptor");
    let mut go = [0u8; 1];
    let _ = control.read(&mut go);
    drop(held);
    std::process::exit(0);
}

// ---------------------------------------------------------------------------------------------
// Starting the sidecar.
// ---------------------------------------------------------------------------------------------

/// Newest artifact built for this machine inside a checkout, if any; never builds one.
fn prebuilt_in(checkout: &Path) -> Option<PathBuf> {
    let suffix = match std::env::consts::ARCH {
        "x86_64" => "linux-x64",
        "aarch64" => "linux-arm64",
        _ => return None,
    };
    let prefix = format!("{SIDECAR_BINARY}-");
    std::fs::read_dir(checkout.join(CHECKOUT_ARTIFACT_DIR))
        .ok()?
        .flatten()
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.starts_with(&prefix) && name.ends_with(suffix)
        })
        .filter_map(|entry| {
            let metadata = entry.metadata().ok().filter(|m| m.is_file())?;
            Some((metadata.modified().ok()?, entry.path()))
        })
        .max()
        .map(|(_, path)| path)
}

/// The program a window would run and its arguments, in the provider's order: a named binary, a
/// named checkout (its prebuilt artifact, else `node` on its built `dist` entry), the per-user build
/// for the pinned revision, then the default checkout's prebuilt artifact. Nothing is built here: a
/// named checkout with no build is refused, where a window would run `npm` in it first.
fn sidecar_program() -> (PathBuf, Vec<PathBuf>) {
    let nonempty = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    if let Some(binary) = nonempty("EITRI_SIDECAR_BINARY") {
        return (PathBuf::from(binary.trim()), Vec::new());
    }
    if let Some(checkout) = nonempty("EITRI_VERDANDI_CHECKOUT") {
        let checkout = Path::new(checkout.trim());
        if let Some(artifact) = prebuilt_in(checkout) {
            return (artifact, Vec::new());
        }
        let entry = checkout.join("apps/claude-sidecar/dist/src/index.js");
        let kernel = checkout.join("packages/claude-runtime/dist/src/index.js");
        assert!(
            entry.is_file() && kernel.is_file(),
            "the named Verdandi checkout {checkout:?} has neither a prebuilt sidecar nor a built dist; build it first"
        );
        return (PathBuf::from("node"), vec![entry]);
    }
    let home = std::env::var_os("HOME");
    let data_home = std::env::var_os("XDG_DATA_HOME");
    if let Some(user) =
        agent::user_sidecar_path(data_home.as_deref(), home.as_deref(), agent::EXPECTED_VERDANDI_REVISION)
            .filter(|p| p.is_file())
    {
        return (user, Vec::new());
    }
    let default_checkout = PathBuf::from(home.expect("HOME is set")).join(DEFAULT_CHECKOUT_UNDER_HOME);
    let artifact = prebuilt_in(&default_checkout).unwrap_or_else(|| panic!("{}", agent::NO_SIDECAR_HINT));
    (artifact, Vec::new())
}

/// A private directory for the socket: `$XDG_RUNTIME_DIR/eitri`, created 0700, refused when it is
/// a symlink, someone else's, or open to the group or others.
fn private_socket_dir() -> PathBuf {
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR is set"));
    assert!(runtime.is_absolute(), "XDG_RUNTIME_DIR is an absolute path");
    let dir = runtime.join("eitri");
    if dir.symlink_metadata().is_err() {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .expect("the socket directory can be created");
    }
    let metadata = dir.symlink_metadata().expect("the socket directory exists");
    // SAFETY: `geteuid` has no preconditions.
    let euid = unsafe { libc::geteuid() };
    assert!(
        metadata.is_dir() && metadata.uid() == euid && metadata.permissions().mode() & 0o077 == 0,
        "the socket directory {dir:?} is a real directory of this user that nobody else can enter"
    );
    dir
}

/// The spawned sidecar. Dropping it ends it as a window does: stdin closed, up to
/// `SIDECAR_EXIT_GRACE` for the sidecar to reap its CLI, then SIGKILL.
struct Sidecar {
    child: Child,
    stdin: Option<ChildStdin>,
    socket_path: PathBuf,
    stderr: Arc<Mutex<Vec<String>>>,
}

impl Sidecar {
    fn start(scratch: &Path) -> Self {
        let (program, args) = sidecar_program();
        eprintln!("probe_pc0: sidecar program {} {args:?}", program.display());
        let socket_path = agent::socket_path::in_dir(
            &private_socket_dir(),
            &format!("sc-pc0-{}.sock", &uuid::Uuid::new_v4().simple().to_string()[..12]),
        )
        .expect("the socket path fits");
        let _ = std::fs::remove_file(&socket_path);
        let mut command = Command::new(&program);
        command
            .args(&args)
            .env("VERDANDI_CLAUDE_SIDECAR_SOCKET", &socket_path)
            .current_dir(scratch)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for var in SCRUBBED_VARS {
            command.env_remove(var);
        }
        if let Some(account) = agent::account::configured() {
            command.env("VERDANDI_CLAUDE_ACCOUNT", account.name());
        }
        let mut child = command.spawn().expect("the sidecar starts");
        let stdin = child.stdin.take();
        let stderr = Arc::new(Mutex::new(Vec::new()));
        drain(child.stdout.take().expect("stdout is piped"), "stdout", None);
        drain(
            child.stderr.take().expect("stderr is piped"),
            "stderr",
            Some(Arc::clone(&stderr)),
        );
        let mut sidecar = Sidecar {
            child,
            stdin,
            socket_path,
            stderr,
        };
        let started = Instant::now();
        while StdUnixStream::connect(&sidecar.socket_path).is_err() {
            if let Ok(Some(status)) = sidecar.child.try_wait() {
                std::thread::sleep(Duration::from_millis(100));
                panic!(
                    "the sidecar exited with {status} before binding its socket; its stderr:\n{}",
                    sidecar.stderr.lock().unwrap().join("\n")
                );
            }
            assert!(
                started.elapsed() < BIND_BUDGET,
                "the sidecar did not bind its socket within {BIND_BUDGET:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        eprintln!(
            "probe_pc0: sidecar pid {} bound after {:?}",
            sidecar.child.id(),
            started.elapsed()
        );
        sidecar
    }

    fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + agent::SIDECAR_EXIT_GRACE;
        while self.running() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        if self.running() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

fn drain<R: Read + Send + 'static>(reader: R, name: &'static str, tail: Option<Arc<Mutex<Vec<String>>>>) {
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(reader).lines().map_while(Result::ok) {
            eprintln!("claude-sidecar[{name}]: {line}");
            if let Some(tail) = &tail {
                tail.lock().unwrap().push(line);
            }
        }
    });
}

/// A helper process the probe started; killed if the probe ends before it does.
struct Helper(Child);

impl Drop for Helper {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

// ---------------------------------------------------------------------------------------------
// Connections.
// ---------------------------------------------------------------------------------------------

/// One HTTP/2 connection to the sidecar over its own unix stream. `socket` is a duplicate of that
/// stream, kept so the connection can be shut down for certain rather than left to a pool.
struct Connection {
    client: RuntimeServiceClient<Channel>,
    socket: Arc<Mutex<Option<OwnedFd>>>,
}

impl Connection {
    async fn open(socket_path: &Path) -> Result<(Self, HandshakeResponse), String> {
        let path = socket_path.to_path_buf();
        let socket: Arc<Mutex<Option<OwnedFd>>> = Arc::default();
        let held = Arc::clone(&socket);
        let channel = Endpoint::try_from("http://[::]:50051")
            .map_err(|e| e.to_string())?
            .connect_with_connector(tower::service_fn(move |_: Uri| {
                let path = path.clone();
                let held = Arc::clone(&held);
                async move {
                    let stream = tokio::net::UnixStream::connect(path).await?;
                    // The protocol carries no credential: only this user's own process is talked to.
                    // SAFETY: `geteuid` has no preconditions.
                    let euid = unsafe { libc::geteuid() };
                    let peer = stream.peer_cred()?.uid();
                    if peer != euid {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            format!("the sidecar socket is served by uid {peer}, not this user ({euid})"),
                        ));
                    }
                    // SAFETY: duplicating a descriptor this stream owns; the copy is owned below.
                    let copy = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
                    if copy < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    // SAFETY: `copy` is a fresh descriptor nothing else owns.
                    *held.lock().unwrap() = Some(unsafe { OwnedFd::from_raw_fd(copy) });
                    Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
                }
            }))
            .await
            .map_err(|e| format!("connect: {e}"))?;
        let mut client = RuntimeServiceClient::new(channel);
        let handshake = tokio::time::timeout(
            RPC_BUDGET,
            client.handshake(HandshakeRequest {
                client_protocol_major: agent::CLIENT_PROTOCOL_MAJOR,
            }),
        )
        .await
        .map_err(|_| "handshake timed out".to_string())?
        .map_err(|status| format!("handshake refused: {status}"))?
        .into_inner();
        if handshake.protocol_major != agent::CLIENT_PROTOCOL_MAJOR {
            return Err(format!(
                "the sidecar speaks protocol major {}, this client {}",
                handshake.protocol_major,
                agent::CLIENT_PROTOCOL_MAJOR
            ));
        }
        Ok((Connection { client, socket }, handshake))
    }

    /// Closes the connection's stream in both directions, so the sidecar sees it end now.
    fn shut_down(self) {
        if let Some(fd) = self.socket.lock().unwrap().take() {
            // SAFETY: `fd` is an open descriptor of the connection's stream.
            unsafe { libc::shutdown(fd.as_raw_fd(), libc::SHUT_RDWR) };
        }
        drop(self.client);
    }
}

type Watched = mpsc::UnboundedReceiver<Result<SessionEvent, String>>;

/// Opens a watch of `session_id` for the events after `after` and forwards them, then the reason
/// the stream ended, to the returned receiver.
fn watch(connection: &Connection, session_id: &str, after: u64) -> (Watched, tokio::task::JoinHandle<()>) {
    let mut client = connection.client.clone();
    let request = WatchSessionEventsRequest {
        session_id: session_id.to_string(),
        start: ReplayStart::AfterSequence as i32,
        after_sequence: Some(after),
    };
    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let mut stream = match client.watch_session_events(request).await {
            Ok(response) => response.into_inner(),
            Err(status) => {
                let _ = tx.send(Err(format!("the watch was refused: {status}")));
                return;
            }
        };
        loop {
            match stream.message().await {
                Ok(Some(event)) => {
                    if tx.send(Ok(event)).is_err() {
                        return;
                    }
                }
                Ok(None) => {
                    let _ = tx.send(Err("the event stream ended".to_string()));
                    return;
                }
                Err(status) => {
                    let _ = tx.send(Err(format!("the event stream failed: {status}")));
                    return;
                }
            }
        }
    });
    (rx, task)
}

/// The next event before `deadline`, or why there is none.
async fn next(rx: &mut Watched, deadline: Instant) -> Result<SessionEvent, String> {
    let left = deadline.saturating_duration_since(Instant::now());
    match tokio::time::timeout(left, rx.recv()).await {
        Ok(Some(result)) => result,
        Ok(None) => Err("the watch task ended".to_string()),
        Err(_) => Err("no event before the deadline".to_string()),
    }
}

fn describe(event: &SessionEvent) -> String {
    let kind = match &event.event {
        Some(Event::SessionReady(_)) => "SessionReady".to_string(),
        Some(Event::TurnStarted(_)) => "TurnStarted".to_string(),
        Some(Event::TextDelta(_)) => "TextDelta".to_string(),
        Some(Event::ThinkingDelta(_)) => "ThinkingDelta".to_string(),
        Some(Event::ToolCallStarted(started)) => format!("ToolCallStarted({})", started.name),
        Some(Event::ToolCallCompleted(completed)) => format!("ToolCallCompleted(is_error={})", completed.is_error),
        Some(Event::PermissionRequested(requested)) => format!("PermissionRequested({})", requested.tool_name),
        Some(Event::PermissionResolved(resolved)) => format!("PermissionResolved(outcome={})", resolved.outcome),
        Some(Event::TurnCompleted(completed)) => format!("TurnCompleted(outcome={})", completed.outcome),
        Some(Event::SessionClosed(closed)) => format!("SessionClosed(reason={})", closed.reason),
        Some(Event::ProviderNotice(notice)) => format!("ProviderNotice({})", notice.kind),
        Some(other) => format!("{other:?}").chars().take(60).collect(),
        None => "(empty)".to_string(),
    };
    format!("#{} {kind}", event.sequence)
}

/// Reads a watch opened after `after` up to the permission request at `permission_sequence`, and
/// says whether it carried exactly connection 1's events after `after`, field for field.
async fn replay_matches(
    rx: &mut Watched,
    after: u64,
    permission_sequence: u64,
    first_events: &[SessionEvent],
) -> (bool, String) {
    let expected: Vec<SessionEvent> = first_events.iter().filter(|e| e.sequence > after).cloned().collect();
    let mut replayed: Vec<SessionEvent> = Vec::new();
    let deadline = Instant::now() + REPLAY_BUDGET;
    let end = loop {
        match next(rx, deadline).await {
            Ok(event) => {
                eprintln!("probe_pc0: conn2 (after #{after}) {}", describe(&event));
                let reached = event.sequence >= permission_sequence;
                replayed.push(event);
                if reached {
                    break None;
                }
            }
            Err(e) => break Some(e),
        }
    };
    let equal = replayed == expected;
    let detail = format!(
        "asked for the events after #{after}; got [{}], expected [{}]{}{}",
        replayed.iter().map(describe).collect::<Vec<_>>().join(", "),
        expected.iter().map(describe).collect::<Vec<_>>().join(", "),
        if equal {
            ", equal to connection 1's events field for field"
        } else {
            ", NOT equal to connection 1's events"
        },
        end.as_ref()
            .map(|e| format!("; the watch stopped: {e}"))
            .unwrap_or_default(),
    );
    (end.is_none() && equal, detail)
}

/// Reads connection 1's watch past the permission request until the answer given on connection 2
/// shows up, and says whether it did: a stream that ended or went quiet means opening the second
/// watch cost the first one.
async fn first_still_watching(rx: &mut Watched, permission_sequence: u64, permission_id: &str) -> (bool, String) {
    let mut seen: Vec<String> = Vec::new();
    let deadline = Instant::now() + REPLAY_BUDGET;
    loop {
        match next(rx, deadline).await {
            Ok(event) => {
                eprintln!("probe_pc0: conn1 (still watching) {}", describe(&event));
                if event.sequence <= permission_sequence {
                    return (
                        false,
                        format!(
                            "connection 1's watch delivered #{} again after #{permission_sequence}",
                            event.sequence
                        ),
                    );
                }
                let answer = matches!(
                    &event.event,
                    Some(Event::PermissionResolved(resolved)) if resolved.permission_id == permission_id
                );
                seen.push(describe(&event));
                if answer {
                    return (
                        true,
                        format!(
                            "connection 1's watch stayed open alongside connection 2's and carried [{}] after the \
                             answer was given on connection 2",
                            seen.join(", ")
                        ),
                    );
                }
            }
            Err(e) => {
                return (
                    false,
                    format!(
                        "connection 1's watch did not carry the answer given on connection 2 ({e}); after \
                         #{permission_sequence} it saw [{}]",
                        seen.join(", ")
                    ),
                );
            }
        }
    }
}

async fn resolve_allow(connection: &Connection, session_id: &str, permission_id: &str) -> Result<(), String> {
    let mut client = connection.client.clone();
    let request = ResolvePermissionRequest {
        session_id: session_id.to_string(),
        command_id: uuid::Uuid::new_v4().to_string(),
        permission_id: permission_id.to_string(),
        allow: true,
        reason: String::new(),
        defer: false,
    };
    tokio::time::timeout(RPC_BUDGET, client.resolve_permission(request))
        .await
        .map_err(|_| "resolvePermission timed out".to_string())?
        .map(|_| ())
        .map_err(|status| format!("resolvePermission refused: {status}"))
}

/// The pipe inode behind the sidecar's stdin, and every descriptor of this process on that pipe.
fn copies_of_sidecar_stdin(sidecar_pid: u32) -> (String, Vec<String>) {
    let target = std::fs::read_link(format!("/proc/{sidecar_pid}/fd/0"))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mine = std::fs::read_dir("/proc/self/fd")
        .map(|dir| {
            dir.flatten()
                .filter(|entry| {
                    std::fs::read_link(entry.path()).is_ok_and(|link| link.to_string_lossy() == target.as_str())
                })
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    (target, mine)
}

// ---------------------------------------------------------------------------------------------
// The probe.
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Verdict {
    lines: Vec<String>,
    failed: Vec<&'static str>,
}

impl Verdict {
    fn check(&mut self, clause: &'static str, holds: bool, detail: String) {
        let mark = if holds { "holds" } else { "FAILS" };
        let line = format!("[{clause}] {mark}: {detail}");
        eprintln!("probe_pc0: {line}");
        self.lines.push(line);
        if !holds && !self.failed.contains(&clause) {
            self.failed.push(clause);
        }
    }
}

const SECOND_WATCHER: &str = "second watcher";
const CROSS_ANSWER: &str = "answer from another connection";
const STDIN_ELSEWHERE: &str = "stdin held elsewhere";

#[test]
#[ignore = "spends real Claude tokens; run under with --ignored"]
fn one_sidecar_serves_a_second_watcher_takes_its_answer_and_outlives_a_spawner_that_hands_off_its_stdin() {
    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("probe_pc0-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&scratch).expect("the scratch directory can be created");
    let scratch = scratch.canonicalize().expect("the scratch directory resolves");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime for the gRPC client");
    let mut sidecar = Sidecar::start(&scratch);
    let verdict = runtime.block_on(probe(&mut sidecar, &scratch));
    drop(sidecar);
    let _ = std::fs::remove_dir_all(&scratch);

    eprintln!("probe_pc0: ---- record ----");
    for line in &verdict.lines {
        eprintln!("probe_pc0: {line}");
    }
    let overall = if verdict.failed.is_empty() { "PASS" } else { "FAIL" };
    eprintln!("probe_pc0: PC-0 verdict: {overall}");
    assert!(
        verdict.failed.is_empty(),
        "the sidecar does not support handing a live session to another connection or process: {:?} failed",
        verdict.failed
    );
}

async fn probe(sidecar: &mut Sidecar, scratch: &Path) -> Verdict {
    let mut verdict = Verdict::default();
    let (first, handshake) = Connection::open(&sidecar.socket_path)
        .await
        .unwrap_or_else(|e| panic!("the first connection is set up: {e}"));
    verdict.lines.push(format!(
        "sidecar {} protocol {}.{}, CLI {} (SDK declares {}), account {:?}",
        handshake.sidecar_version,
        handshake.protocol_major,
        handshake.protocol_minor,
        handshake.actual_claude_code_version,
        handshake.sdk_declared_claude_code_version,
        handshake.account_name,
    ));
    let provider_prompts = handshake
        .capabilities
        .iter()
        .any(|c| c == "provider_permission_prompts");

    // A gated session in the CLI's default (manual) mode, no settings tiers, no tool restricted.
    let created = {
        let mut client = first.client.clone();
        let request = CreateSessionRequest {
            cwd: scratch.to_string_lossy().into_owned(),
            policy: Some(ClaudeHostPolicy {
                configuration: ConfigurationProfile::Native as i32,
                permissions: PermissionMode::Interactive as i32,
                persistence: PersistenceMode::HostCli as i32,
                executable: ExecutableSource::HostCli as i32,
                streaming: StreamingMode::Complete as i32,
                tool_policy: Some(ToolPolicy {
                    deny: Vec::new(),
                    allow: None,
                    unrestricted: true,
                }),
                setting_sources: Some(SettingSourceSelection { sources: Vec::new() }),
                permission_mode_switchable: false,
                provider_permission_prompts: provider_prompts,
                cli_permission_mode: CliPermissionMode::Unspecified as i32,
                accept_unsupported_cli: false,
            }),
            ..Default::default()
        };
        tokio::time::timeout(Duration::from_secs(60), client.create_session(request))
            .await
            .expect("createSession answers within a minute")
            .expect("createSession succeeds")
            .into_inner()
    };
    let session_id = created.session_id;
    eprintln!("probe_pc0: session {session_id} in {}", scratch.display());

    // Connection 1 watches from the start, then the turn is sent.
    let (mut first_rx, first_task) = watch(&first, &session_id, 0);
    {
        let mut client = first.client.clone();
        let request = SendTurnRequest {
            session_id: session_id.clone(),
            command_id: uuid::Uuid::new_v4().to_string(),
            text: PROMPT.to_string(),
        };
        tokio::time::timeout(RPC_BUDGET, client.send_turn(request))
            .await
            .expect("sendTurn answers")
            .expect("sendTurn succeeds");
    }
    let mut first_events: Vec<SessionEvent> = Vec::new();
    let deadline = Instant::now() + PERMISSION_BUDGET;
    let permission: PermissionRequested = loop {
        let event = next(&mut first_rx, deadline)
            .await
            .unwrap_or_else(|e| panic!("connection 1 sees a permission request (inconclusive otherwise): {e}"));
        eprintln!("probe_pc0: conn1 {}", describe(&event));
        let requested = match &event.event {
            Some(Event::PermissionRequested(requested)) => Some(requested.clone()),
            Some(Event::TurnCompleted(_)) => panic!("the turn ended without asking a permission (inconclusive)"),
            _ => None,
        };
        first_events.push(event);
        if let Some(requested) = requested {
            break requested;
        }
    };
    let permission_sequence = first_events.last().unwrap().sequence;
    let contiguous = first_events.iter().enumerate().all(|(i, e)| e.sequence == i as u64 + 1);
    let s = first_events
        .iter()
        .rev()
        .nth(1)
        .map(|e| e.sequence)
        .expect("an event precedes the permission request");
    let after = s.saturating_sub(3);
    verdict.lines.push(format!(
        "connection 1 saw {} events, sequences 1..={permission_sequence} (contiguous: {contiguous}); \
         the permission request ({}, {}) is #{permission_sequence}, S = {s}, second watch after #{after}",
        first_events.len(),
        permission.tool_name,
        permission.input_json,
    ));

    // Connection 2: a new connection with its own handshake, watching after S - 3.
    let second = match Connection::open(&sidecar.socket_path).await {
        Ok((connection, _)) => connection,
        Err(e) => {
            verdict.check(SECOND_WATCHER, false, format!("a second connection is refused: {e}"));
            verdict.check(CROSS_ANSWER, false, "not reached: no second connection".to_string());
            verdict.check(STDIN_ELSEWHERE, false, "not reached: no second connection".to_string());
            return verdict;
        }
    };
    let (mut second_rx, second_task) = watch(&second, &session_id, after);
    let (holds, detail) = replay_matches(&mut second_rx, after, permission_sequence, &first_events).await;
    verdict.check(SECOND_WATCHER, holds, detail);
    // When S - 3 is 0 the replay above is the whole history, which says nothing about a start in the
    // middle of it; a third watch, after S - 1, asks for that too.
    let mid = s.saturating_sub(1);
    if after == 0 && mid > 0 {
        let (mut mid_rx, mid_task) = watch(&second, &session_id, mid);
        let (holds, detail) = replay_matches(&mut mid_rx, mid, permission_sequence, &first_events).await;
        mid_task.abort();
        verdict.check(SECOND_WATCHER, holds, detail);
    }

    // Connection 2 answers. A sidecar that took connection 2's watch by ending connection 1's would still
    // replay correctly above, so connection 1's own stream must carry the answer too before it is closed:
    // that shows both watches were live at once.
    let answered = resolve_allow(&second, &session_id, &permission.permission_id).await;
    let (holds, detail) = first_still_watching(&mut first_rx, permission_sequence, &permission.permission_id).await;
    verdict.check(SECOND_WATCHER, holds, detail);
    first_task.abort();
    drop(first_rx);
    first.shut_down();
    let mut answer_detail = match &answered {
        Ok(()) => "connection 2's resolvePermission(allow) was accepted; connection 1 closed".to_string(),
        Err(e) => format!("connection 2's resolvePermission(allow) failed: {e}"),
    };
    let mut resolved_allowed = false;
    let mut tool_completed = None;
    let mut turn_outcome = None;
    let mut further_permissions = 0;
    let mut stream_end = None;
    let deadline = Instant::now() + TURN_END_BUDGET;
    while turn_outcome.is_none() {
        let event = match next(&mut second_rx, deadline).await {
            Ok(event) => event,
            Err(e) => {
                stream_end = Some(e);
                break;
            }
        };
        if event.sequence <= permission_sequence {
            continue;
        }
        eprintln!("probe_pc0: conn2 {}", describe(&event));
        match &event.event {
            Some(Event::PermissionResolved(resolved)) if resolved.permission_id == permission.permission_id => {
                resolved_allowed = resolved.outcome == PermissionOutcome::Allowed as i32;
            }
            Some(Event::PermissionRequested(more)) => {
                further_permissions += 1;
                if let Err(e) = resolve_allow(&second, &session_id, &more.permission_id).await {
                    answer_detail.push_str(&format!("; a later request could not be answered: {e}"));
                }
            }
            Some(Event::ToolCallCompleted(completed)) if completed.tool_use_id == permission.tool_use_id => {
                tool_completed = Some(!completed.is_error);
            }
            Some(Event::TurnCompleted(completed)) => {
                turn_outcome = Some(completed.outcome);
            }
            _ => {}
        }
    }
    let file_made = scratch.join("probe-file").exists();
    let still_running = sidecar.running();
    let turn_completed = turn_outcome == Some(TurnOutcome::Completed as i32);
    verdict.check(
        CROSS_ANSWER,
        answered.is_ok() && resolved_allowed && tool_completed == Some(true) && turn_completed && still_running,
        format!(
            "{answer_detail}; PermissionResolved allowed: {resolved_allowed}; the tool call completed without \
             error: {tool_completed:?}; TurnCompleted outcome: {turn_outcome:?} (COMPLETED = {}); probe-file \
             exists: {file_made}; further permission requests answered on connection 2: {further_permissions}; \
             sidecar still running: {still_running}{}",
            TurnOutcome::Completed as i32,
            stream_end
                .map(|e| format!("; connection 2's stream: {e}"))
                .unwrap_or_default(),
        ),
    );
    second_task.abort();
    drop(second_rx);
    second.shut_down();

    // The spawner hands a duplicate of the sidecar's stdin to a helper and closes its own copy.
    match hand_stdin_to_helper(sidecar).await {
        Ok(detail) => verdict.check(STDIN_ELSEWHERE, true, detail),
        Err(detail) => verdict.check(STDIN_ELSEWHERE, false, detail),
    }
    verdict
}

async fn hand_stdin_to_helper(sidecar: &mut Sidecar) -> Result<String, String> {
    let pid = sidecar.child.id();
    let stdin = sidecar
        .stdin
        .take()
        .ok_or("the spawner no longer holds the sidecar's stdin")?;
    let (mut ours, theirs) = StdUnixStream::pair().map_err(|e| format!("socketpair: {e}"))?;
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let child = Command::new(exe)
        .args([HELPER_TEST, "--exact", "--ignored", "--nocapture", "--test-threads=1"])
        .env(HELPER_VAR, "1")
        .stdin(Stdio::from(OwnedFd::from(theirs)))
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("the helper does not start: {e}"))?;
    let mut helper = Helper(child);
    send_fd(ours.as_raw_fd(), stdin.as_raw_fd()).map_err(|e| format!("SCM_RIGHTS send: {e}"))?;
    ours.set_read_timeout(Some(Duration::from_secs(20))).ok();
    let mut ack = [0u8; 1];
    ours.read_exact(&mut ack)
        .map_err(|e| format!("the helper did not confirm it holds the descriptor: {e}"))?;
    drop(stdin);
    let (pipe, ours_left) = copies_of_sidecar_stdin(pid);
    if !ours_left.is_empty() {
        return Err(format!(
            "this process still holds the sidecar's stdin ({pipe}) on fds {ours_left:?}; nothing can be judged"
        ));
    }

    tokio::time::sleep(HELD_ALIVE_FOR).await;
    if !sidecar.running() {
        return Err(format!(
            "the sidecar exited within {HELD_ALIVE_FOR:?} of the spawner closing its copy of stdin ({pipe}), \
             although the helper (pid {}) held another",
            helper.0.id()
        ));
    }
    let answered = Connection::open(&sidecar.socket_path).await;
    let answered = match answered {
        Ok((connection, handshake)) => {
            connection.shut_down();
            format!("answered a fresh handshake (sidecar {})", handshake.sidecar_version)
        }
        Err(e) => {
            return Err(format!(
                "the sidecar is alive but its socket does not answer a handshake: {e}"
            ))
        }
    };

    // The helper lets go: its copy was the last one.
    let _ = ours.write_all(b"X");
    drop(ours);
    let helper_deadline = Instant::now() + Duration::from_secs(20);
    while matches!(helper.0.try_wait(), Ok(None)) {
        if Instant::now() >= helper_deadline {
            return Err("the helper did not exit when told to".to_string());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let released = Instant::now();
    let grace = agent::SIDECAR_EXIT_GRACE;
    let give_up = released + grace + Duration::from_secs(5);
    while sidecar.running() && Instant::now() < give_up {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let exited_after = released.elapsed();
    if sidecar.running() {
        return Err(format!(
            "after the helper's copy of stdin closed the sidecar was still running {exited_after:?} later \
             (grace {grace:?})"
        ));
    }
    let status = sidecar.child.wait().map(|s| s.to_string()).unwrap_or_default();
    let detail = format!(
        "the helper received {pipe} by SCM_RIGHTS, the spawner closed its copy; {HELD_ALIVE_FOR:?} later the \
         sidecar was running and {answered}; the helper exited and the sidecar exited {exited_after:?} later \
         ({status}), grace {grace:?}"
    );
    if exited_after <= grace {
        Ok(detail)
    } else {
        Err(detail)
    }
}
