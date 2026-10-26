//! A terminal handed from one session to another, with a real PTY and a real child: `Detach` on the source,
//! `TerminalSession::adopt` and `commit` on the destination, `Resume` when the destination never commits.
//!
//! Where the destination must be able to die without warning, it is a second process: this test binary run again
//! with `MW_TERM_HELPER=<case>`, which runs only `helper_process`. The PTY's controller side goes to it over a
//! seqpacket socket with `SCM_RIGHTS` (a few lines here, so this crate needs nothing of `eitri-core`), the rest of
//! the record through a JSON file, and each step of the move is one byte on the same socket:
//! `a` adopted, `g` (from the source) commit now, `c` committed, `d` done.
//!
//! What is lost or doubled is judged on the raw output: every chunk either side read, through each session's tap,
//! in order. What is drawn is judged against a `Screen` fed that same output from the start, which never moved.

#![cfg(target_os = "linux")]

mod common;

use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{plain_sh, screen_text, size, spec};
use eitri_terminal::replay::screen_replay::ScreenReplay;
use eitri_terminal::{
    DetachRefused, Detached, ExitInfo, PtyChild, PtySize, Screen, ScrollRequest, SelectCommand, SelectKind,
    SessionCommand, SessionConfig, SpawnSpec, Tail, TerminalColors, TerminalSession, ViewState,
};
use serde::{Deserialize, Serialize};
use terminal_input::keys::{Key, KeyEvent, ModifiersState, NamedKey};
use terminal_input::NormalizedInput;
use terminal_render::PaintList;

const COLS: u16 = 80;
const ROWS: u16 = 24;
const WAIT: Duration = Duration::from_secs(20);
/// A flood or a mebibyte paste through a debug build: generous, and only reached when something is stuck.
const LONG: Duration = Duration::from_secs(120);

const HELPER_ENV: &str = "MW_TERM_HELPER";
const HELPER_DIR_ENV: &str = "MW_TERM_DIR";
const HELPER_TEST: &str = "helper_process";
/// The helper's end of the socket, where its `pre_exec` puts it.
const HELPER_SOCK_FD: RawFd = 3;

fn grid() -> PtySize {
    size(COLS, ROWS)
}

fn sh(script: &str) -> SpawnSpec {
    spec("/bin/sh", &["-c", script])
}

/// A directory of this test's own, under the build's scratch directory.
fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("replay_handover")
        .join(format!(
            "{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

type Tapped = Arc<Mutex<Vec<u8>>>;
type TapFn = Box<dyn FnMut(&[u8]) + Send>;

fn tapped() -> (Tapped, TapFn) {
    let bytes: Tapped = Arc::default();
    let into = bytes.clone();
    (
        bytes,
        Box::new(move |chunk: &[u8]| into.lock().unwrap().extend_from_slice(chunk)),
    )
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// One session and what it has published.
struct Host {
    session: TerminalSession,
    woken: mpsc::Receiver<()>,
    frame: Option<PaintList>,
    frames: usize,
    exited: Option<ExitInfo>,
}

impl Host {
    fn config(spawn: SpawnSpec, tap: TapFn) -> SessionConfig {
        SessionConfig {
            spawn,
            size: grid(),
            colors: TerminalColors::default(),
            focused: true,
            tap: Some(tap),
        }
    }

    fn spawn(spawn: SpawnSpec, tap: TapFn) -> Host {
        Host::with(|wake| TerminalSession::spawn(Host::config(spawn, tap), wake))
    }

    fn adopt(detached: Detached, pidfd: OwnedFd, tap: TapFn) -> Host {
        Host::with(|wake| TerminalSession::adopt(detached, pidfd, Host::config(sh("exit"), tap), wake))
    }

    fn with(start: impl FnOnce(Box<dyn Fn() + Send>) -> io::Result<TerminalSession>) -> Host {
        let (tx, woken) = mpsc::channel();
        let session = start(Box::new(move || {
            let _ = tx.send(());
        }))
        .expect("the session starts");
        Host {
            session,
            woken,
            frame: None,
            frames: 0,
            exited: None,
        }
    }

    fn absorb(&mut self) {
        let update = self.session.take_update();
        if let Some(frame) = update.frame {
            self.frame = Some(frame);
            self.frames += 1;
        }
        if update.exited.is_some() {
            self.exited = update.exited;
        }
    }

    fn wait_for(&mut self, timeout: Duration, what: &str, pred: impl Fn(&Host) -> bool) {
        let deadline = Instant::now() + timeout;
        loop {
            self.absorb();
            if pred(self) {
                return;
            }
            let now = Instant::now();
            assert!(
                now < deadline,
                "timed out waiting for {what}; screen:\n{}",
                self.text().join("\n")
            );
            let _ = self
                .woken
                .recv_timeout((deadline - now).min(Duration::from_millis(100)));
        }
    }

    fn text(&self) -> Vec<String> {
        self.frame.as_ref().map(screen_text).unwrap_or_default()
    }

    fn type_str(&self, s: &str) {
        for ch in s.chars() {
            let key = if ch == '\n' {
                Key::Named(NamedKey::Enter)
            } else {
                Key::Character(ch.to_string())
            };
            self.session.send(SessionCommand::Input(NormalizedInput::Key {
                event: KeyEvent::press(key),
                mods: ModifiersState::empty(),
            }));
        }
    }

    fn try_detach(&self) -> Result<Detached, DetachRefused> {
        let (tx, rx) = mpsc::channel();
        self.session.send(SessionCommand::Detach(tx));
        rx.recv_timeout(WAIT).expect("the session answers a detach")
    }

    fn detach(&self) -> Detached {
        self.try_detach().expect("the terminal can move")
    }
}

/// The frame a terminal that never moved draws after `bytes`.
fn unmoved(bytes: &[u8]) -> PaintList {
    let mut screen = Screen::new(grid(), TerminalColors::default());
    screen.feed(bytes);
    screen.render(true)
}

fn pidfd_open(pid: u32) -> OwnedFd {
    // SAFETY: a plain syscall; the descriptor it returns is owned from here on.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    assert!(fd >= 0, "pidfd_open({pid}): {}", io::Error::last_os_error());
    unsafe { OwnedFd::from_raw_fd(fd as RawFd) }
}

/// Reaps a child of this process that has been or is being ended, or panics after `timeout`.
fn reap(pid: u32, timeout: Duration) -> libc::c_int {
    let deadline = Instant::now() + timeout;
    let mut status = 0;
    loop {
        // SAFETY: a plain syscall on a pid this process spawned and nothing else reaps.
        match unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) } {
            0 => {
                assert!(Instant::now() < deadline, "pid {pid} never ended");
                std::thread::sleep(Duration::from_millis(10));
            }
            -1 => panic!("waitpid({pid}): {}", io::Error::last_os_error()),
            _ => return status,
        }
    }
}

/// Ends a child of this process that is still running, by its pid, and reaps it.
fn end(pid: u32) {
    // SAFETY: a plain syscall on a pid this process spawned and has not reaped.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    reap(pid, WAIT);
}

// --- the descriptor and the steps between the two processes ----------------------------------------------------

fn send_fd(sock: RawFd, fd: RawFd) {
    let mut byte = [b'm'];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: `control` is CMSG_SPACE bytes for one descriptor, so the first header and its data fit.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&msg);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::copy_nonoverlapping(
            (&fd as *const RawFd).cast::<u8>(),
            libc::CMSG_DATA(header),
            std::mem::size_of::<RawFd>(),
        );
    }
    let sent = unsafe { libc::sendmsg(sock, &msg, libc::MSG_NOSIGNAL) };
    assert_eq!(sent, 1, "sendmsg: {}", io::Error::last_os_error());
}

/// One descriptor, close-on-exec from the moment it arrives.
fn recv_fd(sock: RawFd) -> OwnedFd {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    let got = unsafe { libc::recvmsg(sock, &mut msg, libc::MSG_CMSG_CLOEXEC) };
    assert_eq!(got, 1, "recvmsg: {}", io::Error::last_os_error());
    assert_eq!(msg.msg_flags & libc::MSG_CTRUNC, 0, "the descriptor was cut off");
    // SAFETY: the kernel filled `control`; the header is read only if present, and the descriptor in it is new.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&msg);
        assert!(!header.is_null(), "no descriptor arrived");
        let mut fd: RawFd = -1;
        std::ptr::copy_nonoverlapping(
            libc::CMSG_DATA(header),
            (&mut fd as *mut RawFd).cast::<u8>(),
            std::mem::size_of::<RawFd>(),
        );
        OwnedFd::from_raw_fd(fd)
    }
}

fn send_step(sock: RawFd, step: u8) {
    let sent = unsafe { libc::send(sock, (&step as *const u8).cast(), 1, libc::MSG_NOSIGNAL) };
    assert_eq!(sent, 1, "send: {}", io::Error::last_os_error());
}

/// The next step from the other side; `None` once it has closed its end. `timeout` of `None` waits without end.
fn recv_step(sock: RawFd, timeout: Option<Duration>) -> Option<u8> {
    let mut pfd = libc::pollfd {
        fd: sock,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = timeout.map_or(-1, |t| t.as_millis() as libc::c_int);
    let polled = unsafe { libc::poll(&mut pfd, 1, ms) };
    assert!(polled > 0, "no step from the other process in {timeout:?}");
    let mut step = 0u8;
    let got = unsafe { libc::recv(sock, (&mut step as *mut u8).cast(), 1, 0) };
    (got == 1).then_some(step)
}

/// Everything of a `Detached` but its descriptor, which travels by `SCM_RIGHTS`.
#[derive(Serialize, Deserialize)]
struct Record {
    pid: u32,
    start: u64,
    replay: ScreenReplay,
    tail: Tail,
    outbox: Vec<u8>,
    view: ViewState,
}

/// The destination in a process of its own.
struct Destination {
    child: std::process::Child,
    sock: OwnedFd,
    dir: PathBuf,
}

impl Destination {
    fn launch(case: &str, detached: Detached) -> Destination {
        let dir = scratch(case);
        let Detached {
            master,
            pid,
            start,
            replay,
            tail,
            outbox,
            view,
        } = detached;
        let record = Record {
            pid,
            start,
            replay,
            tail,
            outbox,
            view,
        };
        fs::write(dir.join("record.json"), serde_json::to_vec(&record).unwrap()).unwrap();
        let mut socks = [-1 as RawFd; 2];
        let rc = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                socks.as_mut_ptr(),
            )
        };
        assert_eq!(rc, 0, "socketpair: {}", io::Error::last_os_error());
        let (mine, theirs) = unsafe { (OwnedFd::from_raw_fd(socks[0]), OwnedFd::from_raw_fd(socks[1])) };
        let theirs_raw = theirs.as_raw_fd();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([HELPER_TEST, "--exact", "--nocapture", "--test-threads", "1"])
            .env(HELPER_ENV, case)
            .env(HELPER_DIR_ENV, &dir);
        // SAFETY: runs in the forked child before exec and calls only async-signal-safe functions.
        unsafe {
            command.pre_exec(move || {
                if theirs_raw == HELPER_SOCK_FD {
                    // dup2 onto itself would keep close-on-exec; clear it instead.
                    let flags = libc::fcntl(theirs_raw, libc::F_GETFD);
                    if flags == -1 || libc::fcntl(theirs_raw, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                } else if libc::dup2(theirs_raw, HELPER_SOCK_FD) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("the destination process starts");
        drop(theirs);
        send_fd(mine.as_raw_fd(), master.as_raw_fd());
        // The destination has its own copy now; the record's goes, as the shell's would after `move`.
        drop(master);
        Destination { child, sock: mine, dir }
    }

    fn expect(&self, step: u8, timeout: Duration) {
        assert_eq!(
            recv_step(self.sock.as_raw_fd(), Some(timeout)),
            Some(step),
            "the destination's next step"
        );
    }

    fn send(&self, step: u8) {
        send_step(self.sock.as_raw_fd(), step);
    }

    fn kill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }

    fn finish(mut self) -> PathBuf {
        let status = self.child.wait().unwrap();
        assert!(status.success(), "the destination process failed: {status:?}");
        self.dir
    }
}

/// The destination process (module doc). Does nothing in an ordinary run.
#[test]
fn helper_process() {
    let Ok(case) = std::env::var(HELPER_ENV) else {
        return;
    };
    let dir = PathBuf::from(std::env::var_os(HELPER_DIR_ENV).expect("the helper's directory"));
    // SAFETY: the parent put the socket on this descriptor for this process alone.
    let sock = unsafe { OwnedFd::from_raw_fd(HELPER_SOCK_FD) };
    let master = recv_fd(sock.as_raw_fd());
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFD) };
    assert!(flags & libc::FD_CLOEXEC != 0, "a received descriptor is close-on-exec");
    let record: Record = serde_json::from_slice(&fs::read(dir.join("record.json")).unwrap()).unwrap();
    let pidfd = pidfd_open(record.pid);
    assert_eq!(
        eitri_terminal::pty::start_time(record.pid).unwrap(),
        record.start,
        "the pid names the process the source read"
    );
    let detached = Detached {
        master,
        pid: record.pid,
        start: record.start,
        replay: record.replay,
        tail: record.tail,
        outbox: record.outbox,
        view: record.view,
    };
    if case == "no-descriptors" {
        // Every descriptor this process may still open is taken, so the session cannot make its
        // doorbell: the adoption fails before any commit, and the shell must be left alone.
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: plain syscalls on this process's own limit, with a live struct.
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) }, 0);
        limit.rlim_cur = limit.rlim_cur.min(256);
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        let mut taken = Vec::new();
        while let Ok(file) = fs::File::open("/dev/null") {
            taken.push(file);
        }
        let (_, tap_fn) = tapped();
        let started = TerminalSession::adopt(detached, pidfd, Host::config(sh("exit"), tap_fn), || {});
        drop(taken);
        assert!(started.is_err(), "the session cannot start with no descriptors left");
        drop(started);
        send_step(sock.as_raw_fd(), b'f');
        return;
    }
    let (tap, tap_fn) = tapped();
    let mut host = Host::adopt(detached, pidfd, tap_fn);
    host.wait_for(WAIT, "the rebuilt screen", |h| h.frame.is_some());
    fs::write(
        dir.join("adopted.json"),
        serde_json::to_vec(&screen_text(host.frame.as_ref().unwrap())).unwrap(),
    )
    .unwrap();
    send_step(sock.as_raw_fd(), b'a');
    if case == "abandoned" {
        // Held until the source kills this process; a source that went away instead closes the socket.
        let _ = recv_step(sock.as_raw_fd(), None);
        return;
    }
    assert_eq!(recv_step(sock.as_raw_fd(), None), Some(b'g'));
    assert!(tap.lock().unwrap().is_empty(), "nothing is read before the commit");
    send_step(sock.as_raw_fd(), b'c');
    host.session.commit();
    match case.as_str() {
        "commit" => {
            host.wait_for(LONG, "the child's end", |h| h.exited.is_some());
            fs::write(dir.join("after.bin"), &*tap.lock().unwrap()).unwrap();
        }
        "keys" => {
            host.type_str("echo ke''yd\n");
            host.wait_for(WAIT, "the typed command's output", |h| {
                h.text().iter().any(|l| l == "keyd")
            });
            fs::write(dir.join("keys.txt"), host.text().join("\n")).unwrap();
        }
        other => panic!("no helper case {other}"),
    }
    send_step(sock.as_raw_fd(), b'd');
}

// --- the counter script -------------------------------------------------------------------------------------------

fn counter(lines: u32) -> SpawnSpec {
    sh(&format!("i=0; while [ $i -lt {lines} ]; do echo $i; i=$((i+1)); done"))
}

/// Every number below `lines` exactly once and in order, in the tty's `\r\n` lines.
fn check_numbers(bytes: &[u8], lines: u32) {
    let text = String::from_utf8(bytes.to_vec()).expect("the counter writes digits and line ends only");
    let mut expected = 0u32;
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let n: u32 = line
            .parse()
            .unwrap_or_else(|_| panic!("not a number: {line:?} (expected {expected})"));
        assert_eq!(n, expected, "a number was lost, doubled or reordered");
        expected += 1;
    }
    assert_eq!(expected, lines, "the output stops early");
}

/// Waits until the source has read the counter's line `n`, mid-flood.
fn read_up_to(source: &mut Host, tap: &Tapped, n: u32) {
    let marker = format!("\n{n}\r\n").into_bytes();
    source.wait_for(LONG, "the counter to get going", |_| {
        contains(&tap.lock().unwrap(), &marker)
    });
}

/// A counter flooding the terminal moves to another process, which waits `stall` before its commit: the source
/// reads nothing once detached, and the two together read every number once.
fn hand_over_a_counter(lines: u32, stall: Duration) {
    let (tap, tap_fn) = tapped();
    let mut source = Host::spawn(counter(lines), tap_fn);
    let pid = source.session.pid();
    read_up_to(&mut source, &tap, 2_000);
    let detached = source.detach();
    let read_before = tap.lock().unwrap().len();
    let renders = source.session.renders();
    let destination = Destination::launch("commit", detached);
    destination.expect(b'a', WAIT);
    let stalled_until = Instant::now() + stall;
    while Instant::now() < stalled_until {
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(tap.lock().unwrap().len(), read_before, "the detached source read on");
        assert_eq!(source.session.renders(), renders, "the detached source rendered");
    }
    destination.send(b'g');
    destination.expect(b'c', WAIT);
    source.session.send(SessionCommand::Committed);
    destination.expect(b'd', LONG);
    let dir = destination.finish();
    assert_eq!(
        tap.lock().unwrap().len(),
        read_before,
        "the source read nothing after the move"
    );
    let mut all = tap.lock().unwrap().clone();
    all.extend_from_slice(&fs::read(dir.join("after.bin")).unwrap());
    check_numbers(&all, lines);
    let status = reap(pid, WAIT);
    assert!(libc::WIFEXITED(status), "the counter ended by itself");
}

#[test]
fn a_counter_script_loses_and_doubles_nothing_across_the_handover() {
    hand_over_a_counter(100_000, Duration::ZERO);
}

#[test]
fn a_destination_stalled_before_the_commit_leaves_the_source_frozen_and_reading_nothing() {
    hand_over_a_counter(50_000, Duration::from_secs(30));
}

#[test]
fn a_destination_killed_before_the_commit_while_output_floods_loses_and_doubles_nothing() {
    const LINES: u32 = 100_000;
    let (tap, tap_fn) = tapped();
    let mut source = Host::spawn(counter(LINES), tap_fn);
    read_up_to(&mut source, &tap, 2_000);
    let detached = source.detach();
    let destination = Destination::launch("abandoned", detached);
    destination.expect(b'a', WAIT);
    destination.kill();
    source.session.send(SessionCommand::Resume);
    source.wait_for(LONG, "the counter's end", |h| h.exited.is_some());
    assert_eq!(
        source.exited.and_then(|e| e.code),
        Some(0),
        "the source reaped its own child"
    );
    check_numbers(&tap.lock().unwrap(), LINES);
}

// --- pastes -----------------------------------------------------------------------------------------------------

const PASTE_BYTES: usize = 1024 * 1024;

/// About a mebibyte of text in lines, and the bytes a program with bracketed paste on receives for it.
fn mebibyte_paste() -> (NormalizedInput, Vec<u8>) {
    let mut text = String::with_capacity(PASTE_BYTES + 64);
    let mut n = 0;
    while text.len() < PASTE_BYTES {
        text.push_str(&format!("pasted line {n:06} of a mebibyte\n"));
        n += 1;
    }
    let mut sent = b"\x1b[200~".to_vec();
    sent.extend_from_slice(text.as_bytes());
    sent.extend_from_slice(b"\x1b[201~");
    let paste = NormalizedInput::Paste { text, bracketed: true };
    (paste, sent)
}

/// A program that turns bracketed paste on, then reads nothing for a second (so the paste waits in the session's
/// outbox), then copies exactly `bytes` of its input to `file`, byte for byte.
fn paste_reader(file: &Path, bytes: usize) -> SpawnSpec {
    sh(&format!(
        "stty raw -echo; printf '\\033[?2004h'; sleep 1; head -c {bytes} > '{}'; sleep 60",
        file.display()
    ))
}

/// Starts the reader and pastes into it just before the move.
fn paste_then_detach(file: &Path, sent: usize, paste: NormalizedInput) -> (Host, Detached) {
    let (tap, tap_fn) = tapped();
    let mut source = Host::spawn(paste_reader(file, sent), tap_fn);
    source.wait_for(WAIT, "bracketed paste on", |_| {
        contains(&tap.lock().unwrap(), b"\x1b[?2004h")
    });
    source.session.send(SessionCommand::Input(paste));
    let detached = source.detach();
    assert!(
        detached.outbox.len() > sent / 2,
        "most of the paste is still unwritten at the move ({} of {sent} bytes)",
        detached.outbox.len()
    );
    (source, detached)
}

fn wait_for_file(file: &Path, len: usize) -> Vec<u8> {
    let deadline = Instant::now() + LONG;
    loop {
        if let Ok(bytes) = fs::read(file) {
            if bytes.len() >= len {
                return bytes;
            }
        }
        assert!(Instant::now() < deadline, "the reader never received {len} bytes");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_one_mebibyte_paste_started_just_before_detach_arrives_once() {
    let dir = scratch("paste-commit");
    let file = dir.join("pasted.bin");
    let (paste, sent) = mebibyte_paste();
    let (source, detached) = paste_then_detach(&file, sent.len(), paste);
    let pid = detached.pid;
    let pidfd = pidfd_open(pid);
    let (_, tap_fn) = tapped();
    let destination = Host::adopt(detached, pidfd, tap_fn);
    source.session.send(SessionCommand::Committed);
    destination.session.commit();
    let got = wait_for_file(&file, sent.len());
    assert!(got == sent, "the reader got every pasted byte once, in order");
    drop(destination);
    reap(pid, WAIT);
}

#[test]
fn a_paste_cut_by_detach_with_the_destination_killed_arrives_once_from_the_source() {
    let dir = scratch("paste-abort");
    let file = dir.join("pasted.bin");
    let (paste, sent) = mebibyte_paste();
    let (source, detached) = paste_then_detach(&file, sent.len(), paste);
    let destination = Destination::launch("abandoned", detached);
    destination.expect(b'a', WAIT);
    destination.kill();
    source.session.send(SessionCommand::Resume);
    let got = wait_for_file(&file, sent.len());
    assert!(
        got == sent,
        "the reader got every pasted byte once, in order, from the source"
    );
    // The source still holds its child, and ends and reaps it when it is dropped.
}

// --- a sequence cut by the move ---------------------------------------------------------------------------------

/// A program prints `before`, waits for a line of input, then prints `after`. The move comes between the two with
/// no boundary wait, so the destination is handed the unfinished sequence raw; it must end up drawing what a
/// terminal that never moved draws.
fn cut_at_detach(before: &str, after: &str, calls: usize) -> (PaintList, Vec<u8>) {
    let script = format!("stty -echo -opost; printf '{before}'; read x; printf '{after}'; sleep 60");
    let (tap, tap_fn) = tapped();
    let mut source = Host::spawn(sh(&script), tap_fn);
    let printed = before.replace("\\033", "\x1b").replace("\\n", "\n").into_bytes();
    source.wait_for(WAIT, "the first half", |_| contains(&tap.lock().unwrap(), &printed));
    source.session.set_boundary_wait(Duration::ZERO);
    let detached = source.detach();
    match &detached.tail {
        Tail::Raw { bytes, calls: made } => {
            assert_eq!(bytes, &printed, "the whole unfinished sequence travels");
            assert_eq!(*made, calls, "the calls it already made travel with it");
        }
        Tail::ReadAhead(_) => panic!("the sequence is cut, so the bytes travel raw"),
    }
    let pid = detached.pid;
    let pidfd = pidfd_open(pid);
    let (after_tap, after_fn) = tapped();
    let mut destination = Host::adopt(detached, pidfd, after_fn);
    source.session.send(SessionCommand::Committed);
    destination.session.commit();
    destination.type_str("\n");
    let printed_after = after.replace("\\033", "\x1b").replace("\\\\", "\\").into_bytes();
    destination.wait_for(WAIT, "the second half", |_| {
        contains(&after_tap.lock().unwrap(), &printed_after)
    });
    let mut all = tap.lock().unwrap().clone();
    all.extend_from_slice(&after_tap.lock().unwrap());
    let expected = unmoved(&all);
    destination.wait_for(WAIT, "the frame of a terminal that never moved", |h| {
        h.frame.as_ref() == Some(&expected)
    });
    drop(destination);
    reap(pid, WAIT);
    (expected, all)
}

#[test]
fn a_csi_cut_at_detach_with_no_wait_gives_one_line_feed_and_a_red_x() {
    let (frame, _) = cut_at_detach("\\033[31\\n", "mX", 1);
    let reference = unmoved(b"\n\x1b[31mX");
    assert_eq!(frame, reference, "one line feed, then a red X");
}

#[test]
fn an_ignored_csi_cut_at_detach_with_no_wait_gives_one_line_feed_and_a_plain_x() {
    let (frame, _) = cut_at_detach("\\033[1?\\n", "mX", 1);
    assert_eq!(frame, unmoved(b"\nX"), "the ignored sequence draws nothing");
}

#[test]
fn an_apc_string_cut_at_detach_with_no_wait_draws_nothing_of_itself() {
    let (frame, _) = cut_at_detach("\\033_private", "data\\033\\\\X", 0);
    assert_eq!(frame, unmoved(b"X"), "only the X after the string");
}

#[test]
fn a_dcs_cut_at_detach_with_no_wait_draws_nothing_of_itself() {
    let (frame, _) = cut_at_detach("\\033P1$q", "m\\033\\\\X", 0);
    assert_eq!(frame, unmoved(b"X"), "only the X after the string");
}

// --- the alternate screen, an abort, and frames ------------------------------------------------------------------

/// A full-screen program as vim draws one: history on the primary screen first, then the alternate screen with a
/// coloured line, a cursor placed and hidden; a line of input ends it.
const FULL_SCREEN: &str = "stty -echo; i=0; while [ $i -lt 300 ]; do echo \"history line $i\"; i=$((i+1)); done; \
    printf '\\033[?1049h\\033[H\\033[2J\\033[1;31m~ VIM ~\\033[0m\\033[5;5Hsplit\\033[12;1H\\033[7m status \\033[0m\\033[5;9H\\033[?25l'; \
    read x; printf '\\033[?25h\\033[?1049l'; sleep 60";

#[test]
fn vim_on_the_alternate_screen_then_the_destination_killed_the_source_frame_equals_the_one_before_detach() {
    let (tap, tap_fn) = tapped();
    let mut source = Host::spawn(sh(FULL_SCREEN), tap_fn);
    source.wait_for(WAIT, "the full-screen program's frame", |h| {
        let bytes = tap.lock().unwrap();
        contains(&bytes, b"\x1b[?25l") && h.frame.as_ref() == Some(&unmoved(&bytes))
    });
    let before = source.frame.clone().unwrap();
    let detached = source.detach();
    let destination = Destination::launch("abandoned", detached);
    destination.expect(b'a', WAIT);
    let adopted: Vec<String> =
        serde_json::from_slice(&fs::read(destination.dir.join("adopted.json")).unwrap()).unwrap();
    assert_eq!(adopted, screen_text(&before), "the destination rebuilt the same screen");
    destination.kill();
    let frames = source.frames;
    source.session.send(SessionCommand::Resume);
    source.wait_for(WAIT, "the rebuilt screen's first frame", |h| h.frames > frames);
    assert!(
        source.frame.as_ref() == Some(&before),
        "the source draws exactly what it drew before the move"
    );
    source.type_str("\n");
    source.wait_for(WAIT, "the primary screen back", |h| {
        let bytes = tap.lock().unwrap();
        contains(&bytes, b"\x1b[?1049l") && h.frame.as_ref() == Some(&unmoved(&bytes))
    });
    assert!(source.text().iter().any(|l| l == "history line 299"));
    // And the history above it, as a terminal that never moved keeps it.
    let mut reference = Screen::new(grid(), TerminalColors::default());
    reference.feed(&tap.lock().unwrap());
    reference.scroll(ScrollRequest::Lines(100));
    let scrolled = reference.render(true);
    source.session.send(SessionCommand::Scroll(ScrollRequest::Lines(100)));
    source.wait_for(WAIT, "the history", |h| h.frame.as_ref() == Some(&scrolled));
}

#[test]
fn no_frame_is_published_from_the_read_term() {
    let (tap, tap_fn) = tapped();
    let mut source = Host::spawn(
        sh("stty -echo; printf 'before the move'; read x; printf ' after'; sleep 60"),
        tap_fn,
    );
    source.wait_for(WAIT, "the first output", |h| {
        h.frame.as_ref() == Some(&unmoved(&tap.lock().unwrap())) && !tap.lock().unwrap().is_empty()
    });
    let detached = source.detach();
    source.absorb();
    let renders = source.session.renders();
    let frames = source.frames;
    // Everything that would draw on a live screen.
    source.session.send(SessionCommand::Visible(false));
    source.session.send(SessionCommand::Visible(true));
    source.session.send(SessionCommand::Focus(false));
    source.session.send(SessionCommand::Resize(size(60, 20)));
    source.session.send(SessionCommand::Scroll(ScrollRequest::Lines(3)));
    source.session.send(SessionCommand::CopyMode(true));
    source
        .session
        .send(SessionCommand::SetColors(TerminalColors::default()));
    let quiet_until = Instant::now() + Duration::from_millis(600);
    while Instant::now() < quiet_until {
        let _ = source.woken.recv_timeout(Duration::from_millis(50));
        source.absorb();
        assert_eq!(source.frames, frames, "a frame was published from the read screen");
        assert_eq!(source.session.renders(), renders, "the read screen was rendered");
    }
    drop(detached);
    source.session.send(SessionCommand::Resume);
    source.wait_for(WAIT, "the rebuilt screen's frame", |h| h.frames > frames);
    let text = source.text();
    assert_eq!(text.first().map(String::as_str), Some("before the move"));
    assert_eq!(
        (source.frame.as_ref().unwrap().cols, source.frame.as_ref().unwrap().rows),
        (60, 20)
    );
    source.type_str("\n");
    source.wait_for(WAIT, "output after the abort", |h| {
        h.text().first().is_some_and(|l| l == "before the move after")
    });
}

#[test]
fn replies_made_while_feeding_read_ahead_follow_the_carried_outbox_after_the_commit() {
    let dir = scratch("replies");
    let file = dir.join("input.bin");
    // Text the program does not read yet, enough to fill the PTY's buffers so most of it waits in the outbox.
    let text: String = (0..256 * 1024).map(|i| (b'a' + (i % 26) as u8) as char).collect();
    let reply = b"\x1b[1;2R";
    let total = text.len() + reply.len();
    let script = format!(
        "stty raw -echo; printf '\\033[31'; sleep 2; printf 'mY\\033[6n'; head -c {total} > '{}'; sleep 60",
        file.display()
    );
    let (tap, tap_fn) = tapped();
    let mut source = Host::spawn(sh(&script), tap_fn);
    source.wait_for(WAIT, "the cut sequence", |_| contains(&tap.lock().unwrap(), b"\x1b[31"));
    source.session.send(SessionCommand::Input(NormalizedInput::Paste {
        text: text.clone(),
        bracketed: false,
    }));
    source.session.set_boundary_wait(Duration::from_secs(10));
    let detached = source.detach();
    assert_eq!(
        detached.tail,
        Tail::ReadAhead(b"Y\x1b[6n".to_vec()),
        "the sequence was finished, and what came after it waits unfed"
    );
    assert!(!detached.outbox.is_empty(), "input still waits to be written");
    let pid = detached.pid;
    let pidfd = pidfd_open(pid);
    let (_, tap_fn) = tapped();
    let destination = Host::adopt(detached, pidfd, tap_fn);
    source.session.send(SessionCommand::Committed);
    destination.session.commit();
    let got = wait_for_file(&file, total);
    let mut expected = text.into_bytes();
    expected.extend_from_slice(reply);
    assert!(
        got == expected,
        "the carried input first, then the answer to the question read after the move (tail {:?})",
        String::from_utf8_lossy(&got[got.len().saturating_sub(16)..])
    );
    drop(destination);
    reap(pid, WAIT);
}

// --- refusals, an update opened at the boundary, the view -------------------------------------------------------

/// `count` kitty keyboard pushes, then `before`; after a pause long enough for any move asked for in between,
/// `after`.
fn pushes_then(count: usize, before: &str, after: &str) -> SpawnSpec {
    sh(&format!(
        "stty -echo; i=0; while [ $i -lt {count} ]; do printf '\\033[>1u'; i=$((i+1)); done; \
         printf '{before}'; sleep 3; printf '{after}'; sleep 60"
    ))
}

#[test]
fn a_move_refused_after_reading_past_the_boundary_still_draws_what_it_read() {
    // One push short of the limit, then a push cut by the read: the wait for the boundary finishes it, which
    // makes the stack too deep, and reads what follows it.
    let (tap, tap_fn) = tapped();
    let mut source = Host::spawn(pushes_then(4095, "START\\033[>1", "uHELLO"), tap_fn);
    source.wait_for(WAIT, "the cut push", |_| {
        contains(&tap.lock().unwrap(), b"START\x1b[>1")
    });
    source.session.set_boundary_wait(Duration::from_secs(10));
    assert_eq!(
        source.try_detach().err(),
        Some(DetachRefused::TooDeep),
        "a stack at the limit does not move"
    );
    assert!(
        contains(&tap.lock().unwrap(), b"uHELLO"),
        "the wait read past the end of the cut push"
    );
    source.wait_for(WAIT, "what was read past the boundary, drawn", |h| {
        h.text().first().is_some_and(|l| l == "STARTHELLO") && h.frame.as_ref() == Some(&unmoved(&tap.lock().unwrap()))
    });
}

#[test]
fn a_move_refused_for_a_deep_keyboard_stack_reads_nothing_first() {
    let (tap, tap_fn) = tapped();
    let mut source = Host::spawn(pushes_then(4096, "START\\033[31", "mHELLO"), tap_fn);
    source.wait_for(WAIT, "the cut sequence", |_| {
        contains(&tap.lock().unwrap(), b"START\x1b[31")
    });
    source.session.set_boundary_wait(Duration::from_secs(10));
    assert_eq!(source.try_detach().err(), Some(DetachRefused::TooDeep));
    assert!(
        !contains(&tap.lock().unwrap(), b"mHELLO"),
        "a refusal that needs no reading is answered before any wait for a boundary"
    );
    source.wait_for(WAIT, "the rest of the output, drawn as usual", |h| {
        h.text().first().is_some_and(|l| l == "STARTHELLO") && h.frame.as_ref() == Some(&unmoved(&tap.lock().unwrap()))
    });
}

#[test]
fn a_synchronized_update_opened_by_the_wait_for_a_boundary_closes_before_the_move() {
    // The read ends inside `ESC[?2026h`; the byte that finishes it opens an update whose content and end follow.
    let (tap, tap_fn) = tapped();
    let mut source = Host::spawn(
        sh("stty -echo; printf 'A\\033[?2026'; sleep 1; printf 'hPARTIAL REST\\033[?2026l'; sleep 60"),
        tap_fn,
    );
    source.wait_for(WAIT, "the cut sequence", |_| {
        contains(&tap.lock().unwrap(), b"A\x1b[?2026")
    });
    source.session.set_boundary_wait(Duration::from_secs(10));
    let detached = source.detach();
    assert_eq!(
        detached.tail,
        Tail::ReadAhead(Vec::new()),
        "the update's content and its end were fed before the read, so nothing of it travels unfed"
    );
    let pid = detached.pid;
    let pidfd = pidfd_open(pid);
    let (_, tap_fn) = tapped();
    let mut destination = Host::adopt(detached, pidfd, tap_fn);
    destination.wait_for(WAIT, "the rebuilt screen", |h| h.frame.is_some());
    assert_eq!(
        destination.frame.as_ref(),
        Some(&unmoved(&tap.lock().unwrap())),
        "the destination draws the whole frame, not the update's first half"
    );
    source.session.send(SessionCommand::Committed);
    destination.session.commit();
    drop(destination);
    reap(pid, WAIT);
}

#[test]
fn a_scrolled_view_with_a_selection_in_copy_mode_is_the_same_after_the_move() {
    let (tap, tap_fn) = tapped();
    let mut source = Host::spawn(
        sh("i=0; while [ $i -lt 200 ]; do echo \"history line $i\"; i=$((i+1)); done; sleep 60"),
        tap_fn,
    );
    source.wait_for(WAIT, "200 lines", |h| {
        let bytes = tap.lock().unwrap();
        contains(&bytes, b"history line 199") && h.frame.as_ref() == Some(&unmoved(&bytes))
    });
    let gestures = [
        SelectCommand::Start {
            line: -25,
            col: 2,
            right_half: false,
            kind: SelectKind::Simple,
        },
        SelectCommand::Extend {
            line: -22,
            col: 10,
            right_half: true,
        },
    ];
    let mut reference = Screen::new(grid(), TerminalColors::default());
    reference.feed(&tap.lock().unwrap());
    reference.set_copy_mode(true);
    reference.scroll(ScrollRequest::Lines(30));
    source.session.send(SessionCommand::CopyMode(true));
    source.session.send(SessionCommand::Scroll(ScrollRequest::Lines(30)));
    for gesture in gestures {
        reference.select(gesture);
        source.session.send(SessionCommand::Select(gesture));
    }
    let expected = reference.render(true);
    source.wait_for(WAIT, "the scrolled view with its selection", |h| {
        h.frame.as_ref() == Some(&expected)
    });
    let detached = source.detach();
    assert_eq!(detached.view.display_offset, 30, "the view is 30 lines back");
    assert!(detached.view.pinned, "the view is held in history");
    assert!(detached.view.copy_mode, "copy mode is on");
    assert!(detached.view.selection.is_some(), "the selection travels");
    let pid = detached.pid;
    let pidfd = pidfd_open(pid);
    let (_, tap_fn) = tapped();
    let mut destination = Host::adopt(detached, pidfd, tap_fn);
    destination.wait_for(WAIT, "the rebuilt screen", |h| h.frame.is_some());
    assert!(
        destination.frame.as_ref() == Some(&expected),
        "the destination shows the same history window, selection and copy-mode indicator"
    );
    source.session.send(SessionCommand::Committed);
    destination.session.commit();
    drop(destination);
    reap(pid, WAIT);
}

#[test]
fn a_destination_that_cannot_start_leaves_the_sources_shell_running() {
    let (_tap, tap_fn) = tapped();
    let mut source = Host::spawn(plain_sh(), tap_fn);
    source.wait_for(WAIT, "the prompt", |h| {
        h.text().first().is_some_and(|l| l.starts_with('$'))
    });
    let detached = source.detach();
    let destination = Destination::launch("no-descriptors", detached);
    destination.expect(b'f', WAIT);
    destination.finish();
    source.session.send(SessionCommand::Resume);
    source.type_str("echo al''ive\n");
    source.wait_for(WAIT, "the shell's answer after the failed move", |h| {
        h.text().iter().any(|l| l == "alive")
    });
    assert_eq!(source.exited, None, "the shell was never hung up");
}

/// A shell moved to a destination in this process that ends after one line of input: the destination is told the
/// status only if `report` is given, the way the spawner would tell it.
fn adopted_shell_ending(report: bool) -> ExitInfo {
    let (_tap, tap_fn) = tapped();
    let source = Host::spawn(sh("stty -echo; read x; exit 7"), tap_fn);
    let detached = source.detach();
    let pid = detached.pid;
    let pidfd = pidfd_open(pid);
    let (_, tap_fn) = tapped();
    let mut destination = Host::adopt(detached, pidfd, tap_fn);
    source.session.send(SessionCommand::Committed);
    destination.session.commit();
    destination.type_str("\n");
    // The source forgot its child at the commit; this process spawned it, so it is the one that can wait.
    let status = reap(pid, WAIT);
    if report {
        destination
            .session
            .send(SessionCommand::ChildExited(ExitStatus::from_raw(status)));
    }
    destination.wait_for(WAIT, "the shell's end", |h| h.exited.is_some());
    destination.exited.unwrap()
}

#[test]
fn an_adopted_shells_exit_status_comes_from_its_spawners_report() {
    let exit = adopted_shell_ending(true);
    assert_eq!(exit.code, Some(7), "the status the spawner reaped: {exit:?}");
}

#[test]
fn an_adopted_shell_whose_spawner_never_reports_ends_unknown() {
    assert_eq!(adopted_shell_ending(false), ExitInfo::UNKNOWN);
}

// --- the adopted child, and keys after the move -------------------------------------------------------------------

fn pidfd_sleeper() -> std::process::Child {
    let mut command = Command::new("/bin/sleep");
    // A sleeper surviving a failed check must not keep the subprocess report pipe open.
    command
        .arg("60")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // A detached test launcher can ignore HUP; exec preserves ignored dispositions.
    // These children must accept it so their status can distinguish the signal's target.
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGHUP, libc::SIG_DFL);
            Ok(())
        });
    }
    command.spawn().expect("sleep starts")
}

#[test]
fn pidfd_sleepers_do_not_hold_the_report_pipe_after_an_early_exit() {
    const CHECK: &str = "pidfd_sleepers_do_not_hold_the_report_pipe_after_an_early_exit";
    if std::env::var(HELPER_ENV).as_deref() == Ok("pidfd-report") {
        let sleeper = pidfd_sleeper();
        println!("pidfd sleeper {}", sleeper.id());
        // Dropping Child leaves it alive, as when an assertion prevents explicit cleanup.
        return;
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", CHECK, "--nocapture", "--test-threads", "1"])
        .env(HELPER_ENV, "pidfd-report")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (started, pid) = mpsc::channel();
    let (finished, eof) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        use io::BufRead;
        for line in io::BufReader::new(stdout).lines() {
            let line = line.unwrap();
            if let Some((_, pid)) = line.split_once("pidfd sleeper ") {
                started.send(pid.parse::<u32>().unwrap()).unwrap();
            }
        }
        finished.send(()).unwrap();
    });
    let sleeper = pidfd_open(pid.recv_timeout(WAIT).expect("the helper started its sleeper"));
    let status = child.wait().unwrap();
    let closed = eof.recv_timeout(WAIT);
    // End the surviving fixture through its descriptor even when waiting for EOF failed.
    let killed = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            sleeper.as_raw_fd(),
            libc::SIGKILL,
            0usize,
            0,
        )
    };
    reader.join().unwrap();
    assert_eq!(killed, 0, "the surviving sleeper was ended");
    assert!(status.success(), "the helper exited early: {status}");
    assert!(
        closed.is_ok(),
        "the sleeper kept the report pipe open after the helper exited"
    );
}

#[test]
fn an_adopted_ptychild_signals_only_through_its_pidfd() {
    let (mut named, mut other) = (pidfd_sleeper(), pidfd_sleeper());
    let pty = PtyChild::spawn(&spec("/bin/sleep", &["60"]), grid()).unwrap();
    let (master, shell_pid) = pty.forget();
    // The pidfd names one process and the pid another: only the pidfd may be signalled.
    let mut adopted = PtyChild::adopt(master, pidfd_open(named.id()), other.id()).unwrap();
    assert_eq!(adopted.pid(), other.id(), "the pid is kept for reports");
    assert!(adopted.try_wait().unwrap().is_none());
    adopted.hangup();
    let status = named.wait().unwrap();
    assert_eq!(status.signal(), Some(libc::SIGHUP), "the pidfd's process was hung up");
    assert!(other.try_wait().unwrap().is_none(), "the process the pid names was not");
    assert!(adopted.try_wait().is_err(), "it exited, and only its spawner knows how");
    adopted.set_exit_status(status);
    assert_eq!(adopted.try_wait().unwrap(), Some(status), "the spawner's report");
    drop(adopted);
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        other.try_wait().unwrap().is_none(),
        "dropping it signalled nothing by pid"
    );
    other.kill().unwrap();
    other.wait().unwrap();
    end(shell_pid);
}

#[test]
fn the_pidfd_signal_check_also_runs_under_a_launcher_ignoring_hup() {
    const CHECK: &str = "an_adopted_ptychild_signals_only_through_its_pidfd";
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", CHECK, "--nocapture", "--test-threads", "1"])
        .stdout(std::process::Stdio::piped());
    // SAFETY: only an async-signal-safe disposition change in the forked child.
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    // Read the child's report on its own thread so a full pipe never stalls it.
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut report = String::new();
        io::Read::read_to_string(&mut stdout, &mut report).unwrap();
        report
    });
    let deadline = Instant::now() + WAIT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("pidfd check hung with inherited ignored HUP");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let report = reader.join().unwrap();
    assert!(
        status.success(),
        "pidfd check failed with inherited ignored HUP: {status}\n{report}"
    );
    // libtest exits 0 when an --exact filter matches nothing, so the report must show the check ran and passed.
    assert!(
        report.lines().any(|line| line == format!("test {CHECK} ... ok")),
        "the re-executed binary did not run {CHECK}:\n{report}"
    );
    assert!(
        report.contains("test result: ok. 1 passed;"),
        "the re-executed binary ran other than exactly one passing test:\n{report}"
    );
}

#[test]
fn keys_typed_after_the_handover_reach_the_shell() {
    let (tap, tap_fn) = tapped();
    let mut source = Host::spawn(plain_sh(), tap_fn);
    source.wait_for(WAIT, "the prompt", |h| {
        h.text().first().is_some_and(|l| l.starts_with('$'))
    });
    let _ = tap;
    let pid = source.session.pid();
    let detached = source.detach();
    let destination = Destination::launch("keys", detached);
    destination.expect(b'a', WAIT);
    destination.send(b'g');
    destination.expect(b'c', WAIT);
    source.session.send(SessionCommand::Committed);
    destination.expect(b'd', WAIT);
    let dir = destination.finish();
    let screen = fs::read_to_string(dir.join("keys.txt")).unwrap();
    assert!(
        screen.lines().any(|l| l == "$ echo ke''yd"),
        "the typed line reached the shell:\n{screen}"
    );
    // The destination's end hung the shell up and killed it; its spawner reaps it.
    reap(pid, WAIT);
}

/// The freeze a moving terminal costs: from `Detach` to the destination's first frame, with 10,000 lines of
/// history. Reported, not asserted: the target is 50 ms on the build host in a release build.
#[test]
#[ignore = "a measurement: run with --ignored --nocapture"]
fn freeze_time_for_ten_thousand_lines() {
    let (tap, tap_fn) = tapped();
    let mut source = Host::spawn(
        sh("i=0; while [ $i -lt 10000 ]; do printf '\\033[3%dmline %d of the history\\033[0m\\n' $((i % 8)) $i; i=$((i+1)); done; sleep 60"),
        tap_fn,
    );
    source.wait_for(LONG, "10,000 lines", |h| {
        let bytes = tap.lock().unwrap();
        contains(&bytes, b"line 9999 of") && h.frame.as_ref() == Some(&unmoved(&bytes))
    });
    let pid = source.session.pid();
    let started = Instant::now();
    let detached = source.detach();
    let read = started.elapsed();
    let replay_bytes = detached.replay.bytes.len();
    let pidfd = pidfd_open(pid);
    let (_, tap_fn) = tapped();
    let mut destination = Host::adopt(detached, pidfd, tap_fn);
    destination.wait_for(WAIT, "the first frame", |h| h.frame.is_some());
    let freeze = started.elapsed();
    eprintln!(
        "freeze: {:.1} ms (detach {:.1} ms), replay {replay_bytes} bytes, 10,000 lines",
        freeze.as_secs_f64() * 1e3,
        read.as_secs_f64() * 1e3
    );
    source.session.send(SessionCommand::Committed);
    destination.session.commit();
    drop(destination);
    reap(pid, WAIT);
}
