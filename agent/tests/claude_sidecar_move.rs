//! A Claude chat's sidecar handed from one process to another without a restart, against a stand-in
//! sidecar (`tests/support/sidecar_standin.rs`): the same gRPC service, a replay ring, several
//! watchers per session, the end on stdin's EOF. No real `claude` runs here.
//!
//! Most tests hand the sidecar from one provider to another inside this process: the descriptors are
//! duplicates either way, and the pidfd names the sidecar whoever its parent is. The hangup test
//! runs the giving side as a process of its own, under a pseudo-terminal of its own.
#![cfg(target_os = "linux")]

#[path = "support/script_gate.rs"]
mod script_gate;
use script_gate::ScriptGate;

use agent::child::{process_group, start_time, ExitCode, PidFd};
use agent::lease::SessionLease;
use agent::providers::claude_sidecar::{ClaudeProviderHandoff, MoveRefusedWhy, SidecarFds};
use agent::setting_sources::ProjectTrust;
use agent::{
    AgentDomainEvent, AgentProvider, AgentSessionProjection, ClaudeSidecarProvider, ContentKind, CreateSessionRequest,
    SendTurnRequest, SteerTurnRequest, StreamingPreference,
};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long any one wait here may take before the test is called hung. Not a speed bound: every
/// wait ends as soon as what it waits for has happened.
const HANG: Duration = Duration::from_secs(30);

/// Set in the environment of the giving process the hangup test starts; nothing else sets it.
const HELPER_SOCKET: &str = "EITRI_SIDECAR_MOVE_HELPER_SOCKET";

fn stand_in() -> ClaudeSidecarProvider {
    let program = Path::new(env!("CARGO_BIN_EXE_eitri-sidecar-standin"));
    ClaudeSidecarProvider::connect_to_program(&uuid::Uuid::new_v4().to_string(), program)
        .expect("the stand-in sidecar starts and answers the handshake")
}

/// A scratch directory under the test target, removed by the caller when it is done.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("sidecar-move-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn open_session(provider: &ClaudeSidecarProvider) -> String {
    provider
        .create_session(CreateSessionRequest {
            cwd: "/".to_string(),
            streaming: StreamingPreference::Partial,
            project: ProjectTrust::Untrusted,
            client_tools: None,
        })
        .expect("the stand-in opens a session")
}

/// Pumps `provider` into `seen` until `done` holds for what was seen.
fn pump_until(
    provider: &ClaudeSidecarProvider,
    seen: &mut Vec<AgentDomainEvent>,
    what: &str,
    done: impl Fn(&[AgentDomainEvent]) -> bool,
) {
    let started = Instant::now();
    loop {
        seen.extend(provider.pump());
        if done(seen) {
            return;
        }
        assert!(started.elapsed() < HANG, "never saw {what}; saw {seen:#?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn opened(events: &[AgentDomainEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, AgentDomainEvent::SessionOpened { .. }))
}

fn turn_completed(events: &[AgentDomainEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, AgentDomainEvent::TurnCompleted { .. }))
}

fn text_of(event: &AgentDomainEvent) -> Option<&str> {
    match event {
        AgentDomainEvent::ContentDelta {
            kind: ContentKind::Text,
            text,
            ..
        } => Some(text),
        _ => None,
    }
}

fn send(provider: &ClaudeSidecarProvider, session_id: &str, script: &str) -> String {
    provider
        .send_turn(SendTurnRequest::text(session_id.to_string(), script.to_string()))
        .expect("the turn is taken")
}

/// The whole hand-over inside this process: freeze, adopt, commit, forget.
fn hand_over(source: &ClaudeSidecarProvider) -> (ClaudeSidecarProvider, ClaudeProviderHandoff) {
    let (handoff, fds) = source.freeze_for_move().expect("the source hands the sidecar over");
    let destination = ClaudeSidecarProvider::adopt(handoff.clone(), fds).expect("the destination takes it over");
    destination.commit().expect("the destination commits");
    source.forget_for_move();
    (destination, handoff)
}

#[test]
fn adopt_with_a_stand_in_that_sends_nothing_commits_at_once() {
    let source = stand_in();
    let session = open_session(&source);
    let mut seen = Vec::new();
    pump_until(&source, &mut seen, "the session opening", opened);

    let (handoff, fds) = source.freeze_for_move().unwrap();
    assert_eq!(handoff.provider_session_id, session);
    assert!(
        handoff.folded_through > 0,
        "the opening was folded, so its sequence is handed on"
    );
    let destination = ClaudeSidecarProvider::adopt(handoff, fds).unwrap();
    let refused = destination.send_turn(SendTurnRequest::text(session.clone(), "end".to_string()));
    assert!(refused.is_err(), "nothing is sent to the sidecar before the commit");

    // The sidecar has nothing newer than what the source folded: a commit that waited for an event
    // would never return. The hang guard is the only clock here.
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| done_tx.send(destination.commit().is_ok()).unwrap());
        assert_eq!(
            done_rx.recv_timeout(HANG),
            Ok(true),
            "the commit returns without any event"
        );
    });
    source.forget_for_move();
    assert!(
        destination.pump().is_empty(),
        "nothing the source folded is delivered again"
    );

    send(&destination, &session, "text m1 hello;end");
    let mut after = Vec::new();
    pump_until(
        &destination,
        &mut after,
        "the turn's end in the destination",
        turn_completed,
    );
    assert!(after.iter().any(|event| text_of(event) == Some("hello")));
    assert!(!opened(&after), "the session's opening is not replayed");
}

#[test]
fn a_move_between_two_text_messages_keeps_two_messages() {
    let between = ScriptGate::new();
    let source = stand_in();
    let session = open_session(&source);
    let mut folded = Vec::new();
    pump_until(&source, &mut folded, "the session opening", opened);
    send(
        &source,
        &session,
        &format!("text m1 one; {}; text m2 two; end", between.command()),
    );
    pump_until(&source, &mut folded, "the first message", |seen| {
        seen.iter().any(|event| text_of(event) == Some("one"))
    });
    between.wait();
    between.release();
    // The watch reads ahead of the fold: the second message's delta (and the boundary it opens here)
    // arrives and is translated, and nobody takes it.
    let started = Instant::now();
    while source.backpressure_stats().pending_events < 2 {
        assert!(
            started.elapsed() < HANG,
            "the second message never reached the source's watch"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    let (handoff, fds) = source.freeze_for_move().unwrap();
    assert_eq!(
        handoff.last_text.as_deref(),
        Some("m1"),
        "the split as of the last event folded"
    );
    let destination = ClaudeSidecarProvider::adopt(handoff, fds).unwrap();
    destination.commit().unwrap();
    source.forget_for_move();

    let mut moved = Vec::new();
    pump_until(
        &destination,
        &mut moved,
        "the turn's end in the destination",
        turn_completed,
    );
    let boundary = moved
        .iter()
        .position(|event| matches!(event, AgentDomainEvent::AssistantMessageBoundary { .. }));
    let second = moved.iter().position(|event| text_of(event) == Some("two"));
    assert!(
        matches!((boundary, second), (Some(b), Some(t)) if b < t),
        "the second message opens a message of its own: {moved:#?}"
    );
    assert!(!moved.iter().any(|event| text_of(event) == Some("one")));

    let mut projection = AgentSessionProjection::default();
    for event in folded.iter().chain(&moved) {
        projection.apply(event);
    }
    let texts: Vec<&str> = projection.transcript.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["one", "two"]);
}

#[test]
fn a_move_is_refused_while_a_steer_is_open_and_goes_through_once_it_is_answered() {
    let turn_gate = ScriptGate::new();
    let steer_gate = ScriptGate::new();
    let source = stand_in();
    let session = open_session(&source);
    let mut seen = Vec::new();
    pump_until(&source, &mut seen, "the session opening", opened);
    let turn = send(&source, &session, &format!("text m1 one; {}; end", turn_gate.command()));
    source
        .steer_turn(SteerTurnRequest {
            session_id: session.clone(),
            expected_turn_id: turn,
            client_message_id: "steer-1".to_string(),
            text: steer_gate.command(),
            as_typed: "and also this".to_string(),
        })
        .unwrap();

    let refused = source.freeze_for_move().map(|_| ()).unwrap_err();
    assert!(
        matches!(refused, MoveRefusedWhy::SteerOpen | MoveRefusedWhy::SteerInFlight),
        "{refused:?}"
    );
    steer_gate.release();
    // A refused freeze leaves the source handing events over: the delivery below is pumped here.
    pump_until(&source, &mut seen, "the steered message's delivery", |seen| {
        seen.iter()
            .any(|event| matches!(event, AgentDomainEvent::UserMessageDelivered { .. }))
    });
    let (destination, _) = hand_over(&source);
    turn_gate.release();
    let mut moved = Vec::new();
    pump_until(
        &destination,
        &mut moved,
        "the turn's end in the destination",
        turn_completed,
    );
}

/// A message steered while frozen would be open on this side only: the taking side's gate would have
/// no record of it and would drop its delivery.
#[test]
fn a_frozen_source_sends_nothing_to_the_sidecar_until_it_is_unfrozen() {
    let turn_gate = ScriptGate::new();
    let source = stand_in();
    let session = open_session(&source);
    let mut seen = Vec::new();
    pump_until(&source, &mut seen, "the session opening", opened);
    let turn = send(&source, &session, &format!("text m1 one; {}; end", turn_gate.command()));
    let (_handoff, fds) = source.freeze_for_move().unwrap();
    drop(fds);
    let steer = SteerTurnRequest {
        session_id: session.clone(),
        expected_turn_id: turn,
        client_message_id: "steer-1".to_string(),
        text: "0".to_string(),
        as_typed: "and also this".to_string(),
    };
    assert!(source.steer_turn(steer).is_err(), "refused while frozen");
    assert!(
        source
            .send_turn(SendTurnRequest::text(session.clone(), "end".to_string()))
            .is_err(),
        "refused while frozen"
    );
    source.unfreeze().unwrap();
    turn_gate.release();
    pump_until(&source, &mut seen, "the turn's end after the thaw", turn_completed);
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, AgentDomainEvent::UserMessageDelivered { .. })),
        "nothing refused reached the sidecar"
    );
}

#[test]
fn the_lease_moves_without_an_unlock_and_a_third_process_cannot_take_it_at_any_point() {
    let source = stand_in();
    let _session = open_session(&source);
    let mut seen = Vec::new();
    pump_until(&source, &mut seen, "the session opening", opened);

    let dir = scratch("lease");
    let cwd = dir.to_string_lossy().to_string();
    let claude_session = format!("lease-test-{}", uuid::Uuid::new_v4());
    let lease = SessionLease::try_acquire("claude", &cwd, &claude_session).unwrap();
    let path = lease.path().to_path_buf();
    let third_takes_it = || {
        Command::new("flock")
            .args(["--nonblock", "--exclusive"])
            .arg(&path)
            .arg("true")
            .status()
            .expect("flock runs")
            .success()
    };
    assert!(!third_takes_it(), "held by the source");

    let (handoff, fds) = source.freeze_for_move().unwrap();
    let lease_fd = lease.descriptor_for_move().unwrap();
    assert!(!third_takes_it(), "frozen");
    let destination = ClaudeSidecarProvider::adopt(handoff, fds).unwrap();
    let moved_lease = SessionLease::from_moved(lease_fd, "claude", &cwd, &claude_session).unwrap();
    assert!(!third_takes_it(), "taken over, not committed");
    destination.commit().unwrap();
    moved_lease.rewrite_owner_pid(std::process::id()).unwrap();
    assert!(!third_takes_it(), "committed");
    source.forget_for_move();
    drop(source);
    drop(lease);
    assert!(!third_takes_it(), "the source let go of its copy");

    drop(destination);
    drop(moved_lease);
    assert!(third_takes_it(), "the holder's end releases it");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn forget_deletes_no_socket_and_a_later_reconnect_finds_it() {
    let source = stand_in();
    let session = open_session(&source);
    let mut seen = Vec::new();
    pump_until(&source, &mut seen, "the session opening", opened);
    let (destination, handoff) = hand_over(&source);
    drop(source);
    assert!(
        handoff.socket_path.exists(),
        "the source deleted the socket file of a sidecar it handed on"
    );

    // The stand-in ends every watch stream: the destination's watch connects again through the socket.
    send(
        &destination,
        &session,
        "drop-watches;sleep 300;text m1 after the reconnect;end",
    );
    let mut moved = Vec::new();
    pump_until(
        &destination,
        &mut moved,
        "the turn's end in the destination",
        turn_completed,
    );
    assert!(moved.iter().any(|event| text_of(event) == Some("after the reconnect")));
    assert!(destination.backpressure_stats().watch_reconnects >= 1);
    assert!(UnixStream::connect(&handoff.socket_path).is_ok());
}

#[test]
fn the_holders_clean_end_signals_through_the_pidfd_and_deletes_the_socket() {
    let source = stand_in();
    let session = open_session(&source);
    let mut seen = Vec::new();
    pump_until(&source, &mut seen, "the session opening", opened);
    // From here the stand-in outlives its stdin's end, so only a signal ends it.
    send(&source, &session, "ignore-eof;end");
    pump_until(&source, &mut seen, "the turn's end", turn_completed);
    let pid = source.sidecar_pid();
    // This process started the sidecar, so it reads the status the holder's end leaves.
    let watcher = PidFd::open(pid, start_time(pid).unwrap()).unwrap();
    let (destination, handoff) = hand_over(&source);
    drop(source);
    assert!(!watcher.exited(), "handing the sidecar on ends nothing");

    drop(destination);
    assert!(watcher.wait_exited(HANG));
    assert_eq!(
        watcher.try_reap().unwrap(),
        Some(ExitCode::Signaled { signal: libc::SIGKILL })
    );
    assert!(!handoff.socket_path.exists());
}

#[test]
fn the_holders_end_closes_stdin_and_a_sidecar_that_honours_it_exits_by_itself() {
    let source = stand_in();
    let _session = open_session(&source);
    let mut seen = Vec::new();
    pump_until(&source, &mut seen, "the session opening", opened);
    let pid = source.sidecar_pid();
    let watcher = PidFd::open(pid, start_time(pid).unwrap()).unwrap();
    let (destination, handoff) = hand_over(&source);
    drop(source);
    drop(destination);
    assert!(watcher.wait_exited(HANG));
    assert_eq!(watcher.try_reap().unwrap(), Some(ExitCode::Exited { code: 0 }));
    assert!(!handoff.socket_path.exists());
}

#[test]
fn a_move_given_back_before_the_commit_ends_nothing_and_the_source_reads_on() {
    let source = stand_in();
    let session = open_session(&source);
    let mut seen = Vec::new();
    pump_until(&source, &mut seen, "the session opening", opened);
    let (handoff, fds) = source.freeze_for_move().unwrap();
    let destination = ClaudeSidecarProvider::adopt(handoff.clone(), fds).unwrap();
    drop(destination);
    assert!(handoff.socket_path.exists());

    source.unfreeze().unwrap();
    send(&source, &session, "text m1 still here;end");
    pump_until(&source, &mut seen, "the turn's end in the source", turn_completed);
    assert!(seen.iter().any(|event| text_of(event) == Some("still here")));
}

// ---------------------------------------------------------------------------------------------
// The spawning window as a process of its own, under its own pseudo-terminal.
// ---------------------------------------------------------------------------------------------

/// Room for one control message carrying three descriptors, aligned for `cmsghdr`.
type ControlBuffer = [u64; 8];

fn send_with_fds(socket: RawFd, body: &[u8], fds: &[RawFd; 3]) {
    let mut iov = libc::iovec {
        iov_base: body.as_ptr() as *mut libc::c_void,
        iov_len: body.len(),
    };
    let mut control: ControlBuffer = [0; 8];
    // SAFETY: `CMSG_SPACE` only computes a size.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of_val(fds) as u32) } as usize;
    assert!(space <= std::mem::size_of_val(&control));
    // SAFETY: an all-zero `msghdr` is a valid empty message; every pointer set below outlives the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: `msg_control` points at `space` writable, aligned bytes, room for one header and three
    // descriptors.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(fds) as u32) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(cmsg).cast::<RawFd>(), 3);
    }
    // SAFETY: `msg` is fully initialised above.
    let sent = unsafe { libc::sendmsg(socket, &msg, libc::MSG_NOSIGNAL) };
    assert_eq!(sent, body.len() as isize, "{}", std::io::Error::last_os_error());
}

fn recv_with_fds(socket: RawFd) -> (Vec<u8>, [OwnedFd; 3]) {
    let mut body = vec![0u8; 64 * 1024];
    let mut iov = libc::iovec {
        iov_base: body.as_mut_ptr().cast(),
        iov_len: body.len(),
    };
    let mut control: ControlBuffer = [0; 8];
    // SAFETY: as in `send_with_fds`.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&control) as _;
    // SAFETY: `msg` points at writable buffers that outlive the call.
    let read = unsafe { libc::recvmsg(socket, &mut msg, libc::MSG_CMSG_CLOEXEC) };
    assert!(read > 0, "{}", std::io::Error::last_os_error());
    body.truncate(read as usize);
    let mut raw = [-1 as RawFd; 3];
    // SAFETY: the kernel filled the control buffer; the header is checked before its data is read.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        assert!(!cmsg.is_null() && (*cmsg).cmsg_type == libc::SCM_RIGHTS);
        std::ptr::copy_nonoverlapping(libc::CMSG_DATA(cmsg).cast::<RawFd>(), raw.as_mut_ptr(), 3);
    }
    // SAFETY: three descriptors this process just received and nobody else owns.
    let fds = raw.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) });
    (body, fds)
}

fn read_byte(stream: &mut UnixStream) -> u8 {
    let mut byte = [0u8];
    stream.read_exact(&mut byte).expect("the other side answers");
    byte[0]
}

#[test]
#[ignore = "the spawning window's half of the hangup test, which starts it under `script`"]
fn spawner_helper() {
    let Some(path) = std::env::var_os(HELPER_SOCKET) else {
        return;
    };
    let mut link = UnixStream::connect(path).unwrap();
    let provider = stand_in();
    let _session = open_session(&provider);
    let mut seen = Vec::new();
    pump_until(&provider, &mut seen, "the session opening", opened);
    let (handoff, fds) = provider.freeze_for_move().unwrap();
    let body = serde_json::to_vec(&(std::process::id(), handoff)).unwrap();
    let SidecarFds { stdin, stdout, stderr } = fds;
    send_with_fds(
        link.as_raw_fd(),
        &body,
        &[stdin.as_raw_fd(), stdout.as_raw_fd(), stderr.as_raw_fd()],
    );
    drop((stdin, stdout, stderr));
    assert_eq!(read_byte(&mut link), b'C', "the taking side committed");
    provider.forget_for_move();
    link.write_all(b"F").unwrap();
    // Stays until its terminal goes away or a signal ends it.
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[test]
fn a_moved_sidecar_survives_the_spawners_terminal_hangup_and_sigint() {
    let dir = scratch("hangup");
    let socket = dir.join("link");
    let listener = UnixListener::bind(&socket).unwrap();
    let exe = std::env::current_exe().unwrap();
    let command = format!(
        "exec '{}' --exact spawner_helper --ignored --nocapture --test-threads=1",
        exe.display()
    );
    // The spawning window stands in for one a person started from a terminal, which a Ctrl+C or a hangup ends. A suite
    // started in the background inherits both ignored (`nohup`, a script's `&`), and an ignored disposition survives
    // exec: the window would outlive the test's signals, and the sidecar it spawns would survive them only by
    // inheritance. So the terminal starts with both at their defaults.
    let mut terminal = Command::new("script");
    // SAFETY: runs in the forked child before exec and calls only `signal`, which is async-signal-safe.
    unsafe {
        terminal.pre_exec(|| {
            for signal in [libc::SIGHUP, libc::SIGINT] {
                libc::signal(signal, libc::SIG_DFL);
            }
            Ok(())
        });
    }
    let mut terminal = terminal
        .args(["--quiet", "--command", &command, "/dev/null"])
        .env(HELPER_SOCKET, &socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("script runs");

    listener.set_nonblocking(true).unwrap();
    let started = Instant::now();
    let mut link = loop {
        match listener.accept() {
            Ok((link, _)) => break link,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(started.elapsed() < HANG, "the spawning process never connected");
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("{e}"),
        }
    };
    link.set_nonblocking(false).unwrap();
    link.set_read_timeout(Some(HANG)).unwrap();
    let (body, fds) = recv_with_fds(link.as_raw_fd());
    let (spawner_pid, handoff): (u32, ClaudeProviderHandoff) = serde_json::from_slice(&body).unwrap();
    let spawner = PidFd::open(spawner_pid, start_time(spawner_pid).unwrap()).unwrap();
    let sidecar_pid = handoff.child.pid;
    let sidecar = PidFd::open(sidecar_pid, handoff.child.start).unwrap();
    let session = handoff.provider_session_id.clone();

    let destination = ClaudeSidecarProvider::adopt(handoff.clone(), SidecarFds::from_array(fds)).unwrap();
    destination.commit().unwrap();
    link.write_all(b"C").unwrap();
    assert_eq!(read_byte(&mut link), b'F', "the spawning side let go");

    let spawner_group = process_group(spawner_pid).unwrap();
    assert_eq!(
        process_group(sidecar_pid).unwrap(),
        sidecar_pid,
        "the sidecar leads a group of its own"
    );
    assert_ne!(spawner_group, sidecar_pid);
    // A Ctrl+C in the spawner's terminal, then the terminal itself going away.
    // SAFETY: `kill` reads no memory of ours.
    assert_eq!(unsafe { libc::kill(-(spawner_group as libc::pid_t), libc::SIGINT) }, 0);
    terminal.kill().unwrap();
    terminal.wait().unwrap();
    assert!(spawner.wait_exited(HANG), "the spawning process ended");
    assert!(
        !sidecar.wait_exited(Duration::from_secs(1)),
        "the sidecar outlives the process that spawned it and that process's terminal"
    );

    send(&destination, &session, "text m1 still alive;end");
    let mut moved = Vec::new();
    pump_until(
        &destination,
        &mut moved,
        "the turn's end in the destination",
        turn_completed,
    );
    assert!(moved.iter().any(|event| text_of(event) == Some("still alive")));

    drop(destination);
    assert!(sidecar.wait_exited(HANG), "the holder's end ends it");
    assert!(!handoff.socket_path.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_idle_claude_chat_moves_and_commits_with_no_new_event() {
    let source = stand_in();
    let session = open_session(&source);
    let mut seen = Vec::new();
    pump_until(&source, &mut seen, "the session opening", opened);

    // As two windows move it: through the face every provider moves by, its record as JSON and copies of its
    // descriptors, taken up again by the kind the record names.
    let mover = AgentProvider::mover(&source).expect("a Claude chat moves");
    let frozen = mover.freeze_for_chat_move().expect("the sidecar is handed over");
    let record: serde_json::Value = serde_json::from_str(&frozen.record.to_string()).unwrap();
    let kind = agent::chat_mover::MovedKind::from_wire(frozen.kind.wire()).unwrap();
    let adopted = agent::chat_mover::adopt(kind, &record, frozen.copies().unwrap()).expect("taken up");
    let destination = adopted.agent;
    assert!(adopted.external.is_none());
    // Nothing newer than what the source folded exists: a commit that waited for an event would never return.
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let destination = &destination;
        scope.spawn(move || {
            done_tx
                .send(destination.mover().unwrap().commit_chat_move().is_ok())
                .unwrap()
        });
        assert_eq!(
            done_rx.recv_timeout(HANG),
            Ok(true),
            "the commit returns without any event"
        );
    });
    mover.forget_chat_move();
    drop(frozen);
    assert!(
        destination.pump().is_empty(),
        "nothing the source folded is delivered again"
    );

    destination
        .send_turn(SendTurnRequest::text(session.clone(), "text m1 hello;end".to_string()))
        .expect("the destination drives the session");
    let start = Instant::now();
    let mut after = Vec::new();
    while !turn_completed(&after) {
        assert!(start.elapsed() < HANG, "never saw the turn end; saw {after:#?}");
        after.extend(destination.pump());
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(after.iter().any(|event| text_of(event) == Some("hello")));
    assert!(!opened(&after), "the session's opening is not replayed");
}
