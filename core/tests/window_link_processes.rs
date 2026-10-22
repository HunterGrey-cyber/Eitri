//! The link between two real processes: this test binary is run again as the other window, with its end of the
//! pair on fd 3 and a status pipe on fd 4, and writes one status line the parent reads. Its own binary, so the
//! children it starts never hold copies of the descriptors other tests are checking.

#![cfg(target_os = "linux")]

use std::fs::File;
use std::io::{self, PipeReader, PipeWriter, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use eitri_core::window_link::fdpass::{self, FdKind};
use eitri_core::window_link::link::park::start_parked;
use eitri_core::window_link::link::{Inbound, LinkHandle, PresenceFlag};
use eitri_core::window_link::moves::{MoveId, Moves, SourceVerdict, Window};
use eitri_core::window_link::{Msg, Shape};
use serde_json::json;

const LONG: Duration = Duration::from_secs(20);
const HELPER: &str = "MW_LINK_HELPER";
const HELPER_TEST: &str = "helper_process_entry_runs_only_when_re_run_by_a_two_process_test";

fn readable(fd: BorrowedFd<'_>, ms: i32) -> bool {
    let mut fds = [libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    }];
    loop {
        // SAFETY: one valid entry for the call.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 1, ms) };
        if rc >= 0 {
            return fds[0].revents & libc::POLLIN != 0;
        }
        assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::Interrupted);
    }
}

fn next(rx: &Receiver<Inbound>) -> Inbound {
    rx.recv_timeout(LONG).expect("the link passes something on")
}

fn next_msg(rx: &Receiver<Inbound>) -> (Msg, Vec<OwnedFd>) {
    match next(rx) {
        Inbound::Msg { msg, fds } => (msg, fds),
        other => panic!("a message was due, got {other:?}"),
    }
}

fn pipe() -> (PipeReader, PipeWriter) {
    std::io::pipe().unwrap()
}

/// Puts `fds` on 3 and 4 in the child. Copies to high numbers first, so a source that happens to be 3 or 4 is not
/// overwritten before it is placed.
fn place(fds: [libc::c_int; 2]) -> io::Result<()> {
    let mut high = [0; 2];
    for (slot, fd) in high.iter_mut().zip(fds) {
        // SAFETY: `fcntl` and `dup2` are async-signal-safe and act only on descriptor numbers.
        *slot = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 100) };
        if *slot < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    for (target, fd) in (3..).zip(high) {
        // SAFETY: as above.
        if unsafe { libc::dup2(fd, target) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn spawn_helper(scenario: &str, link_end: OwnedFd, status: PipeWriter) -> Child {
    let fds = [link_end.as_raw_fd(), status.as_raw_fd()];
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", HELPER_TEST, "--nocapture", "--test-threads=1", "-q"])
        .env(HELPER, scenario)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    // SAFETY: the closure calls only async-signal-safe functions.
    unsafe { cmd.pre_exec(move || place(fds)) };
    let child = cmd.spawn().unwrap();
    drop(link_end);
    drop(status);
    child
}

fn status_line(r: &mut PipeReader) -> String {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while !line.ends_with(b"\n") {
        assert!(readable(r.as_fd(), 60_000), "the helper reports: {line:?}");
        if r.read(&mut byte).unwrap() == 0 {
            break;
        }
        line.push(byte[0]);
    }
    String::from_utf8(line).unwrap().trim().to_owned()
}

#[test]
fn helper_process_entry_runs_only_when_re_run_by_a_two_process_test() {
    let Ok(scenario) = std::env::var(HELPER) else {
        return;
    };
    fdpass::set_cloexec_raw(3).unwrap();
    fdpass::set_cloexec_raw(4).unwrap();
    // SAFETY: fds 3 and 4 were placed by the parent for this process alone.
    let (sock, mut status) = unsafe { (OwnedFd::from_raw_fd(3), File::from_raw_fd(4)) };
    let result = match scenario.as_str() {
        "pass" => helper_pass(sock),
        "park" => helper_destination(sock, true, &mut status),
        _ => helper_destination(sock, false, &mut status),
    };
    let (line, code) = match result {
        Ok(line) => (line, 0),
        Err(why) => (format!("bad: {why}"), 1),
    };
    status.write_all(format!("{line}\n").as_bytes()).unwrap();
    std::process::exit(code);
}

fn helper_pass(sock: OwnedFd) -> Result<String, String> {
    let (link, rx) = LinkHandle::start(sock, PresenceFlag::default()).map_err(|e| e.to_string())?;
    let (msg, fds) = next_msg(&rx);
    let Msg::Move { mv, record } = msg else {
        return Err(format!("{msg:?}"));
    };
    let kinds = record["kinds"].as_array().ok_or("no kinds")?;
    if kinds.len() != fds.len() {
        return Err(format!("{} descriptors for {} kinds", fds.len(), kinds.len()));
    }
    for (fd, kind) in fds.into_iter().zip(kinds) {
        let want = match kind.as_str() {
            Some("pty") => FdKind::PtyMaster,
            Some("pipe") => FdKind::Pipe,
            _ => FdKind::OwnFile,
        };
        fdpass::check(fd.as_fd(), want).map_err(|e| format!("{kind}: {e}"))?;
        if !fdpass::is_cloexec(fd.as_fd()).unwrap_or(false) {
            return Err(format!("{kind} arrived without close-on-exec"));
        }
        let mut file = File::from(fd);
        match want {
            FdKind::PtyMaster => file.write_all(b"t\n"),
            FdKind::Pipe => file.write_all(b"p"),
            _ => file.write_all(b"l"),
        }
        .map_err(|e| format!("{kind}: {e}"))?;
    }
    let written = link
        .send_confirmed(Msg::MoveDone { mv }, Vec::new())
        .map_err(|e| e.to_string())?;
    written
        .recv_timeout(LONG)
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    Ok("ok".into())
}

/// The destination of one move; with `park`, its link thread is held just before `move_done` is written.
fn helper_destination(sock: OwnedFd, park: bool, status: &mut File) -> Result<String, String> {
    let (link, rx, hook) = start_parked(sock, PresenceFlag::default(), if park { "move_done" } else { "none" })
        .map_err(|e| e.to_string())?;
    let mut moves = Moves::new(Window::Two);
    let (msg, _) = next_msg(&rx);
    let Msg::MoveOffer { mv, .. } = msg else {
        return Err(format!("{msg:?}"));
    };
    let id = MoveId(mv);
    if !moves.accept_offer(id) || !moves.prepared(id) {
        return Err(format!("offer {mv} not taken"));
    }
    link.send(Msg::MoveReady { mv }, Vec::new())
        .map_err(|e| e.to_string())?;
    let (msg, fds) = next_msg(&rx);
    if !matches!(msg, Msg::Move { mv: m, .. } if m == mv) {
        return Err(format!("{msg:?}"));
    }
    moves.received(id, fds).map_err(|e| e.to_string())?;
    let written = link
        .send_confirmed(Msg::MoveDone { mv }, Vec::new())
        .map_err(|e| e.to_string())?;
    let line = if park {
        if !hook.wait_parked(LONG) {
            return Err("never reached the write".into());
        }
        if let Ok(result) = written.try_recv() {
            moves.done_sent(id, result);
        }
        if moves.commit(id).is_ok() {
            return Err("committed before move_done was written".into());
        }
        "parked, commit refused".to_owned()
    } else {
        moves.done_sent(id, written.recv_timeout(LONG).map_err(|e| e.to_string())?);
        let fds = moves
            .commit(id)
            .map_err(|_| "commit refused after the write".to_owned())?;
        let mut file = File::from(fds.into_iter().next().ok_or("no descriptor")?);
        file.write_all(b"x").map_err(|e| e.to_string())?;
        "committed".to_owned()
    };
    // Report, then wait to be killed; a parent that died first ends this within the deadline.
    let deadline = Instant::now() + Duration::from_secs(60);
    status
        .write_all(format!("{line}\n").as_bytes())
        .map_err(|e| e.to_string())?;
    while Instant::now() < deadline {
        if let Ok(Inbound::Closed) = rx.recv_timeout(Duration::from_secs(1)) {
            break;
        }
    }
    std::process::exit(0);
}

fn pty() -> (OwnedFd, File) {
    // SAFETY: opens a new pseudo-terminal master.
    let raw = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    assert!(raw >= 0);
    // SAFETY: new.
    let master = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut name = [0 as libc::c_char; 128];
    // SAFETY: `name` has room for the path `ptsname_r` writes; the master is open.
    unsafe {
        assert_eq!(libc::grantpt(master.as_raw_fd()), 0);
        assert_eq!(libc::unlockpt(master.as_raw_fd()), 0);
        assert_eq!(libc::ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len()), 0);
    }
    // SAFETY: `ptsname_r` wrote a NUL-terminated path.
    let raw = unsafe { libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    assert!(raw >= 0);
    // SAFETY: new.
    (master, unsafe { File::from_raw_fd(raw) })
}

#[test]
fn two_processes_pass_a_pty_master_pipes_and_a_lease_file_over_a_real_link() {
    let (a, b) = fdpass::seqpacket_pair().unwrap();
    let (mut status_r, status_w) = pipe();
    let mut child = spawn_helper("pass", b, status_w);
    let (link, rx) = LinkHandle::start(a, PresenceFlag::default()).unwrap();
    let (master, mut slave) = pty();
    // Kept open here so the input the helper writes is still there once the helper has exited.
    let _master_kept = master.try_clone().unwrap();
    let (mut r1, w1) = pipe();
    let (mut r2, w2) = pipe();
    let lease_path = std::env::temp_dir().join(format!("eitri-link-lease-{}", std::process::id()));
    let lease = File::create(&lease_path).unwrap();
    let mut lease_back = File::open(&lease_path).unwrap();
    std::fs::remove_file(&lease_path).unwrap();
    link.send(
        Msg::Move {
            mv: 1,
            record: json!({"kinds": ["pty", "pipe", "pipe", "lease"]}),
        },
        vec![master, w1.into(), w2.into(), lease.into()],
    )
    .unwrap();
    assert_eq!(status_line(&mut status_r), "ok");
    assert_eq!(next_msg(&rx).0, Msg::MoveDone { mv: 1 });
    assert!(child.wait().unwrap().success());
    for r in [&mut r1, &mut r2] {
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"p", "a byte through each pipe");
    }
    assert!(readable(slave.as_fd(), 20_000));
    let mut got = [0u8; 16];
    let n = slave.read(&mut got).unwrap();
    assert_eq!(&got[..n], b"t\n", "a byte through the pty");
    let mut got = Vec::new();
    lease_back.read_to_end(&mut got).unwrap();
    assert_eq!(got, b"l");
}

/// The source's half of one move to a helper destination: offer, ready, freeze, `move` with a pipe's write end.
struct SourceSide {
    child: Child,
    _link: LinkHandle,
    rx: Receiver<Inbound>,
    moves: Moves,
    id: MoveId,
    /// The read end of the pipe whose write end moved.
    moved: PipeReader,
    status: PipeReader,
}

fn source_sends_a_move(scenario: &str) -> SourceSide {
    let (a, b) = fdpass::seqpacket_pair().unwrap();
    let (status_r, status_w) = pipe();
    let child = spawn_helper(scenario, b, status_w);
    let (link, rx) = LinkHandle::start(a, PresenceFlag::default()).unwrap();
    let mut moves = Moves::new(Window::One);
    let id = moves.offer(Instant::now());
    link.send(
        Msg::MoveOffer {
            mv: id.0,
            module: "terminal#2".into(),
            kind: "terminal".into(),
            shape: Shape::Tiled,
            size: (80, 24),
            session: None,
        },
        Vec::new(),
    )
    .unwrap();
    assert_eq!(next_msg(&rx).0, Msg::MoveReady { mv: id.0 });
    assert!(moves.ready(id));
    assert!(moves.freeze(id, Instant::now()));
    let (r, w) = pipe();
    link.send(
        Msg::Move {
            mv: id.0,
            record: json!({}),
        },
        vec![w.into()],
    )
    .unwrap();
    assert!(moves.sent(id, Instant::now()));
    SourceSide {
        child,
        _link: link,
        rx,
        moves,
        id,
        moved: r,
        status: status_r,
    }
}

#[test]
fn a_destination_killed_with_move_done_queued_but_unwritten_lets_the_source_resume_and_nothing_acted() {
    let SourceSide {
        mut child,
        _link,
        rx,
        mut moves,
        id,
        mut moved,
        mut status,
    } = source_sends_a_move("park");
    assert_eq!(status_line(&mut status), "parked, commit refused");
    child.kill().unwrap();
    child.wait().unwrap();
    match next(&rx) {
        Inbound::Closed => {}
        other => panic!("the end-of-file with no move_done, got {other:?}"),
    }
    let verdicts = moves.link_closed();
    assert!(
        matches!(verdicts.as_slice(), [(v, SourceVerdict::Resume(_))] if *v == id),
        "{verdicts:?}"
    );
    let mut got = Vec::new();
    moved.read_to_end(&mut got).unwrap();
    assert!(got.is_empty(), "the destination never acted on the moved descriptor");
}

#[test]
fn a_destination_killed_just_after_move_done_was_written_is_a_commit_at_the_source() {
    let SourceSide {
        mut child,
        _link,
        rx,
        mut moves,
        id,
        mut moved,
        mut status,
    } = source_sends_a_move("written");
    assert_eq!(status_line(&mut status), "committed");
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(next_msg(&rx).0, Msg::MoveDone { mv: id.0 });
    assert_eq!(moves.done(id), Some(SourceVerdict::Committed));
    assert!(matches!(next(&rx), Inbound::Closed));
    assert!(
        moves.link_closed().is_empty(),
        "nothing resumes after the commit was read"
    );
    let mut got = Vec::new();
    moved.read_to_end(&mut got).unwrap();
    assert_eq!(got, b"x", "the destination acted only after its commit");
}
