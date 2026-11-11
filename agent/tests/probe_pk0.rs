//! A headless spike on what ties a child process to the window that spawned it, run with
//! `-- --ignored --nocapture`. Three processes play the parts:
//!
//! - **A**, a window started from a terminal: the test binary re-run under `script -qc`, so it is the leader of a
//!   session whose controlling terminal is a pty of its own. A starts two stand-in sidecars, `sh -c 'cat
//!   >/dev/null'` (a sidecar ends on end-of-file on its stdin): the first as the sidecar is spawned today (no
//!   process group of its own, so it stays in A's), the second with `process_group(0)`. A hands both stdin write
//!   ends to B over `SCM_RIGHTS` and closes its own copies.
//! - **B**, the window the children are handed to: the test binary re-run in a process group of its own. It
//!   receives the descriptors with `MSG_CMSG_CLOEXEC` and lists what a child it spawns inherits.
//! - **The test process**, which plays the user: it hangs up A's terminal (kills `script`, which closes the pty's
//!   master) and sends `SIGINT` to A's process group, as a terminal's Ctrl+C would. One test hangs up first, the
//!   other interrupts first (A still running), then hangs up.
//!
//! Each test checks two claims:
//!
//! - (a) process groups: the stand-in in A's group dies with A's terminal; the one in its own group survives, and
//!   acknowledges input after both signals and ends on EOF once B closes its stdin;
//! - (c) close-on-exec: a child B spawns after receiving the descriptors inherits none of them. A deliberate
//!   inheritable copy, made and closed before the real listing, shows the listing would catch a leak.
//!
//! Spike code, never product code.

#![cfg(target_os = "linux")]

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Which part the re-run test binary plays: `a` or `b`. Unset in an ordinary run.
const ROLE_ENV: &str = "PK0_ROLE";
/// The path of the socket B listens on and A connects to.
const SOCK_ENV: &str = "PK0_SOCK";
const ROLE_TEST: &str = "pk0_role";
/// The prefix of every line B writes for the test process; anything else on its stdout is the test harness's.
const LINE_TAG: &str = "PK0 ";

/// Each re-run process gives up and exits after this, so a stuck run leaves nothing behind for long.
const ROLE_DEADLINE: Duration = Duration::from_secs(120);
/// From starting A to B's report of what it received.
const SETUP_BUDGET: Duration = Duration::from_secs(30);
/// From the hangup to A's exit.
const HANGUP_BUDGET: Duration = Duration::from_secs(60);
/// How long the stand-in in A's group has to die after the signal.
const SIGINT_BUDGET: Duration = Duration::from_secs(60);
/// How long the surviving stand-in has to end once the last copy of its stdin is closed.
const EOF_BUDGET: Duration = Duration::from_secs(60);
/// A descriptor number far above anything B opens itself, for the inheritable copy that shows a leak is seen.
const CONTROL_FD_FLOOR: RawFd = 200;

// ------------------------------------------------------------------------------------ descriptors

fn sockaddr_for(path: &Path) -> (libc::sockaddr_un, libc::socklen_t) {
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    assert!(bytes.len() < addr.sun_path.len(), "socket path too long: {path:?}");
    for (slot, byte) in addr.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let len = std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1;
    (addr, len as libc::socklen_t)
}

fn seqpacket_socket() -> OwnedFd {
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());
    // SAFETY: the descriptor was just made and is owned by nothing else.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn listen_at(path: &Path) -> OwnedFd {
    let sock = seqpacket_socket();
    let (addr, len) = sockaddr_for(path);
    let rc = unsafe { libc::bind(sock.as_raw_fd(), (&addr as *const libc::sockaddr_un).cast(), len) };
    assert_eq!(rc, 0, "bind {path:?}: {}", std::io::Error::last_os_error());
    let rc = unsafe { libc::listen(sock.as_raw_fd(), 1) };
    assert_eq!(rc, 0, "listen: {}", std::io::Error::last_os_error());
    sock
}

fn connect_to(path: &Path) -> OwnedFd {
    let sock = seqpacket_socket();
    let (addr, len) = sockaddr_for(path);
    let rc = unsafe { libc::connect(sock.as_raw_fd(), (&addr as *const libc::sockaddr_un).cast(), len) };
    assert_eq!(rc, 0, "connect {path:?}: {}", std::io::Error::last_os_error());
    sock
}

/// Waits until `fd` is readable; false when `until` passed first.
fn readable_by(fd: RawFd, until: Instant) -> bool {
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut pfd, 1, left.as_millis().clamp(1, 1000) as i32) } > 0 {
            return true;
        }
    }
}

fn accept_by(listener: RawFd, until: Instant) -> OwnedFd {
    assert!(readable_by(listener, until), "nobody connected in time");
    let fd = unsafe { libc::accept4(listener, std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) };
    assert!(fd >= 0, "accept4: {}", std::io::Error::last_os_error());
    // SAFETY: the descriptor was just accepted and is owned by nothing else.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

/// Sends `payload` and `fds` in one packet.
fn send_packet(sock: RawFd, payload: &[u8], fds: &[RawFd]) {
    let mut iov = libc::iovec {
        iov_base: payload.as_ptr() as *mut libc::c_void,
        iov_len: payload.len(),
    };
    let data_len = std::mem::size_of_val(fds) as u32;
    let space = unsafe { libc::CMSG_SPACE(data_len) as usize };
    let mut control = vec![0u8; space];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: `control` is CMSG_SPACE bytes for these descriptors, so the first header and its data fit.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&msg);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(data_len) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr().cast::<u8>(), libc::CMSG_DATA(header), data_len as usize);
    }
    let sent = unsafe { libc::sendmsg(sock, &msg, libc::MSG_NOSIGNAL) };
    assert_eq!(
        sent,
        payload.len() as isize,
        "sendmsg: {}",
        std::io::Error::last_os_error()
    );
}

/// Receives one packet and the descriptors it carries, each close-on-exec from the moment it arrives.
fn recv_packet(sock: RawFd, until: Instant) -> (Vec<u8>, Vec<OwnedFd>) {
    assert!(readable_by(sock, until), "no packet arrived in time");
    let mut payload = vec![0u8; 64 * 1024];
    let mut iov = libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: payload.len(),
    };
    let space = unsafe { libc::CMSG_SPACE((8 * std::mem::size_of::<RawFd>()) as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    let got = unsafe { libc::recvmsg(sock, &mut msg, libc::MSG_CMSG_CLOEXEC) };
    assert!(got >= 0, "recvmsg: {}", std::io::Error::last_os_error());
    assert_eq!(
        msg.msg_flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC),
        0,
        "the packet was cut off"
    );
    payload.truncate(got as usize);
    let mut fds = Vec::new();
    // SAFETY: the kernel filled `control` and `msg_controllen`; headers are walked only while present.
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&msg);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let bytes = (*header).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let data = libc::CMSG_DATA(header).cast::<RawFd>();
                for i in 0..bytes / std::mem::size_of::<RawFd>() {
                    fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(i))));
                }
            }
            header = libc::CMSG_NXTHDR(&msg, header);
        }
    }
    (payload, fds)
}

fn is_cloexec(fd: RawFd) -> bool {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    flags != -1 && flags & libc::FD_CLOEXEC != 0
}

/// What `/proc/<pid>/fd/<n>` reads for a pipe end: `pipe:[<inode>]`.
fn pipe_name(fd: RawFd) -> String {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstat(fd, &mut st) };
    assert_eq!(rc, 0, "fstat: {}", std::io::Error::last_os_error());
    format!("pipe:[{}]", st.st_ino)
}

// ------------------------------------------------------------------------------------ processes

/// A process watched through a pidfd, so a pid reused after its exit is never signalled.
struct Watched {
    pidfd: OwnedFd,
}

impl Watched {
    fn open(pid: i32) -> Watched {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as RawFd;
        assert!(fd >= 0, "pidfd_open {pid}: {}", std::io::Error::last_os_error());
        // SAFETY: the descriptor was just opened and is owned by nothing else.
        Watched {
            pidfd: unsafe { OwnedFd::from_raw_fd(fd) },
        }
    }

    /// How long it took the process to exit, or `None` if it was still running when `within` passed.
    fn exit_within(&self, within: Duration) -> Option<Duration> {
        let at = Instant::now();
        let until = at + within;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            let mut pfd = libc::pollfd {
                fd: self.pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut pfd, 1, left.as_millis().min(1000) as i32) };
            if ready > 0 {
                return Some(at.elapsed());
            }
            if left.is_zero() {
                return None;
            }
        }
    }

    fn running(&self) -> bool {
        self.exit_within(Duration::ZERO).is_none()
    }

    fn kill(&self) {
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
    }
}

impl Drop for Watched {
    fn drop(&mut self) {
        // A test that stops early must not leave a stand-in or A running; a process already gone ignores it.
        self.kill();
    }
}

/// A child the test started in a process group of its own; the group is killed if the test ends first.
struct Started {
    child: Child,
    reaped: bool,
}

impl Started {
    fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    fn wait_within(&mut self, within: Duration) -> Option<String> {
        let until = Instant::now() + within;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                self.reaped = true;
                return Some(status.to_string());
            }
            if Instant::now() >= until {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Started {
    fn drop(&mut self) {
        if !self.reaped {
            // SAFETY: not reaped, so the pid and the group it leads are still this child's.
            unsafe { libc::kill(-self.pid(), libc::SIGKILL) };
            let _ = self.child.wait();
        }
    }
}

/// Fields 3 to 7 of `/proc/<pid>/stat`: state, parent, process group, session and controlling terminal.
#[derive(Debug, Clone, Copy)]
struct ProcStat {
    state: char,
    ppid: i32,
    pgrp: i32,
    session: i32,
    tty_nr: i32,
}

fn proc_stat(pid: i32) -> Option<ProcStat> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may hold spaces and parentheses; the fields start after its last `)`.
    let rest = &text[text.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    Some(ProcStat {
        state: fields.first()?.chars().next()?,
        ppid: fields.get(1)?.parse().ok()?,
        pgrp: fields.get(2)?.parse().ok()?,
        session: fields.get(3)?.parse().ok()?,
        tty_nr: fields.get(4)?.parse().ok()?,
    })
}

fn stat_json(pid: i32) -> Value {
    match proc_stat(pid) {
        Some(s) => json!({
            "pid": pid, "state": s.state.to_string(), "ppid": s.ppid, "pgrp": s.pgrp,
            "session": s.session, "tty_nr": s.tty_nr,
        }),
        None => json!({ "pid": pid, "gone": true }),
    }
}

/// The test binary re-run as `role`, only the role test selected.
fn role_command(role: &str, sock: &Path) -> (PathBuf, Vec<&'static str>, [(&'static str, String); 2]) {
    let exe = std::env::current_exe().expect("the test binary");
    let args = vec![ROLE_TEST, "--exact", "--ignored", "--nocapture", "--test-threads", "1"];
    let env = [(ROLE_ENV, role.to_string()), (SOCK_ENV, sock.display().to_string())];
    (exe, args, env)
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

// ------------------------------------------------------------------------------------ the roles

/// A's part: start the two stand-ins, hand their stdin to B, then wait to be hung up.
fn role_a() {
    let started = Instant::now();
    let sock = PathBuf::from(std::env::var_os(SOCK_ENV).expect("the socket path"));
    let stand_in = |own_group: bool| {
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                if own_group {
                    "while read -r line; do printf 'ack\n' > \"$1\"; done; printf 'eof\n' > \"$1.eof\""
                } else {
                    "cat >/dev/null"
                },
                "stand-in",
            ])
            .arg(sock.with_extension("ack"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if own_group {
            command.process_group(0);
        }
        command.spawn().expect("a stand-in starts")
    };
    let mut in_my_group = stand_in(false);
    let mut in_its_own_group = stand_in(true);
    let first_stdin = OwnedFd::from(in_my_group.stdin.take().expect("stdin"));
    let second_stdin = OwnedFd::from(in_its_own_group.stdin.take().expect("stdin"));

    let me = std::process::id() as i32;
    let report = json!({
        "a": {
            "pid": me,
            "pgrp": unsafe { libc::getpgrp() },
            "sid": unsafe { libc::getsid(0) },
            "stdin_is_a_tty": unsafe { libc::isatty(0) } == 1,
            "foreground_pgrp": unsafe { libc::tcgetpgrp(0) },
        },
        "first_pid": in_my_group.id(),
        "second_pid": in_its_own_group.id(),
    });
    let conn = connect_to(&sock);
    send_packet(
        conn.as_raw_fd(),
        report.to_string().as_bytes(),
        &[first_stdin.as_raw_fd(), second_stdin.as_raw_fd()],
    );
    drop(first_stdin);
    drop(second_stdin);

    // A window keeps running until its terminal goes; the deadline only bounds a run nobody hangs up.
    while started.elapsed() < ROLE_DEADLINE {
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = in_my_group.kill();
    let _ = in_its_own_group.kill();
    std::process::exit(3);
}

/// One `sh -c <script>` child of B's, its stdout as text; nothing set up beyond std's own stdio handling, so
/// what it inherits is what any child of a window would.
fn listing(script: &str) -> String {
    let output = Command::new("sh")
        .args(["-c", script])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .expect("sh runs");
    assert!(output.status.success(), "{script}: {}", output.status);
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The descriptor numbers in `ls /proc/self/fd`.
fn numbers_listed() -> (BTreeSet<RawFd>, String) {
    let text = listing("ls /proc/self/fd");
    let numbers = text.split_whitespace().filter_map(|word| word.parse().ok()).collect();
    (numbers, text)
}

/// The link targets in `ls -l /proc/self/fd` (`pipe:[123]`, `/dev/null`, ...).
fn targets_listed() -> (BTreeSet<String>, String) {
    let text = listing("ls -l /proc/self/fd");
    let targets = text
        .lines()
        .filter_map(|line| line.split_once(" -> ").map(|(_, target)| target.trim().to_string()))
        .collect();
    (targets, text)
}

fn say(line: &Value) {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{LINE_TAG}{line}").expect("stdout");
    out.flush().expect("stdout");
}

/// B's part: receive the stand-ins' stdin, list what a child inherits, then close the second's stdin and the
/// first's when told.
fn role_b() {
    let until = Instant::now() + ROLE_DEADLINE;
    let sock = PathBuf::from(std::env::var_os(SOCK_ENV).expect("the socket path"));
    let listener = listen_at(&sock);
    say(&json!({ "listening": true }));

    let conn = accept_by(listener.as_raw_fd(), until);
    let (payload, fds) = recv_packet(conn.as_raw_fd(), until);
    let from_a: Value = serde_json::from_slice(&payload).expect("A's report is JSON");
    assert_eq!(fds.len(), 2, "both stand-ins' stdin arrive");
    let numbers: Vec<RawFd> = fds.iter().map(|fd| fd.as_raw_fd()).collect();
    let cloexec: Vec<bool> = numbers.iter().map(|&fd| is_cloexec(fd)).collect();
    let names: Vec<String> = numbers.iter().map(|&fd| pipe_name(fd)).collect();

    // An inheritable copy first, so a listing that misses a leak would show here, not pass silently.
    let control = unsafe { libc::fcntl(numbers[1], libc::F_DUPFD, CONTROL_FD_FLOOR) };
    assert!(
        control >= CONTROL_FD_FLOOR,
        "F_DUPFD: {}",
        std::io::Error::last_os_error()
    );
    let (control_numbers, _) = numbers_listed();
    let (control_targets, _) = targets_listed();
    unsafe { libc::close(control) };
    let control_seen = json!({
        "fd": control,
        "seen_by_number": control_numbers.contains(&control),
        "seen_by_target": control_targets.contains(&names[1]),
    });

    let (listed_numbers, numbers_text) = numbers_listed();
    let (listed_targets, targets_text) = targets_listed();
    let leaked_numbers: Vec<RawFd> = numbers.iter().copied().filter(|n| listed_numbers.contains(n)).collect();
    let leaked_targets: Vec<&String> = names.iter().filter(|name| listed_targets.contains(*name)).collect();

    say(&json!({
        "received": {
            "from_a": from_a,
            "fds": numbers,
            "cloexec": cloexec,
            "pipes": names,
            "control": control_seen,
            "listed_numbers": listed_numbers,
            "listing": numbers_text.split_whitespace().collect::<Vec<_>>(),
            "listing_long": targets_text.lines().skip(1).collect::<Vec<_>>(),
            "leaked_numbers": leaked_numbers,
            "leaked_targets": leaked_targets,
        }
    }));

    let mut fds = fds.into_iter();
    let mut first = fds.next();
    let mut second = fds.next().map(File::from);
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        match line.expect("stdin").trim() {
            "probe-second" => {
                writeln!(second.as_mut().expect("second stdin"), "probe").unwrap();
                say(&json!({ "probed": "second" }));
            }
            "close-second" => {
                second.take();
                say(&json!({ "closed": "second" }));
            }
            "exit" => break,
            other => panic!("unknown command {other:?}"),
        }
        if Instant::now() >= until {
            break;
        }
    }
    first.take();
    drop(second);
}

/// The re-run binary's entry: plays A or B. Without its environment variable (any ordinary `--ignored` run) it
/// does nothing.
#[test]
#[ignore = "probe: run with --ignored"]
fn pk0_role() {
    match std::env::var(ROLE_ENV).as_deref() {
        Ok("a") => role_a(),
        Ok("b") => role_b(),
        _ => {}
    }
}

// ------------------------------------------------------------------------------------ the test process

/// B's tagged lines, read on a thread so the test can wait on them with a deadline.
fn tagged_lines(stdout: std::process::ChildStdout) -> Receiver<Value> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            // The harness writes `test <name> ... ` before the test's own output, on the same line.
            if let Some(body) = line.find(LINE_TAG).map(|at| &line[at + LINE_TAG.len()..]) {
                if let Ok(value) = serde_json::from_str::<Value>(body) {
                    if tx.send(value).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

fn expect_line(rx: &Receiver<Value>, key: &str, within: Duration, why: &dyn Fn() -> String) -> Value {
    let until = Instant::now() + within;
    loop {
        let left = until.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(value) if value.get(key).is_some() => return value[key].clone(),
            Ok(_) => continue,
            Err(error) => panic!("B said no `{key}` ({error}); {}", why()),
        }
    }
}

fn tell(stdin: &mut ChildStdin, command: &str) {
    writeln!(stdin, "{command}").expect("B's stdin");
    stdin.flush().expect("B's stdin");
}

fn millis(duration: Option<Duration>) -> Value {
    duration.map_or(Value::Null, |d| json!(d.as_millis() as u64))
}

/// How the terminal A runs on goes away.
#[derive(Debug, Clone, Copy)]
enum Ending {
    /// The terminal's window is closed, then Ctrl+C reaches whatever is left in A's group.
    HangupThenSigint,
    /// Ctrl+C in the terminal while A runs, then the terminal's window is closed.
    SigintThenHangup,
}

#[test]
#[ignore = "probe: re-runs this binary under `script`; run with --ignored"]
fn a_child_in_its_spawners_group_dies_with_a_closed_terminal_and_one_in_its_own_group_does_not() {
    run(Ending::HangupThenSigint);
}

#[test]
#[ignore = "probe: re-runs this binary under `script`; run with --ignored"]
fn a_child_in_its_spawners_group_dies_with_ctrl_c_and_one_in_its_own_group_does_not() {
    run(Ending::SigintThenHangup);
}

fn run(ending: Ending) {
    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("pk0-{}-{ending:?}", std::process::id()));
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch).expect("scratch dir");
    let sock = agent::socket_path::in_dir(&scratch, "b.sock").expect("a short enough socket path");
    let a_log = scratch.join("a.log");
    let b_err = scratch.join("b.err");
    let tail = |path: &Path| {
        let text = fs::read_to_string(path).unwrap_or_default();
        let start = text.len().saturating_sub(2000);
        text.get(start..).unwrap_or(&text).to_string()
    };
    let why = || format!("A's terminal: {:?}; B's stderr: {:?}", tail(&a_log), tail(&b_err));

    // B, in a process group of its own, so neither the hangup nor the signal reaches it.
    let (exe, args, env) = role_command("b", &sock);
    let mut command = Command::new(&exe);
    command
        .args(&args)
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(File::create(&b_err).expect("b.err"))
        .process_group(0);
    let mut b = Started {
        child: command.spawn().expect("B starts"),
        reaped: false,
    };
    let mut b_stdin = b.child.stdin.take().expect("B's stdin");
    let b_lines = tagged_lines(b.child.stdout.take().expect("B's stdout"));
    expect_line(&b_lines, "listening", SETUP_BUDGET, &why);

    // A, as a window started from a terminal: `script` gives it a pty, and `exec` makes it the session's leader.
    let (exe, args, env) = role_command("a", &sock);
    let mut line = format!("exec {}", shell_quote(&exe.display().to_string()));
    for arg in &args {
        line.push(' ');
        line.push_str(&shell_quote(arg));
    }
    let a_out = File::create(&a_log).expect("a.log");
    let mut command = Command::new("script");
    command
        .args(["-q", "-c", &line, "/dev/null"])
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .env("SHELL", "/bin/sh")
        // A pipe held open, so `script` never sees end-of-file on its input and passes nothing on to A.
        .stdin(Stdio::piped())
        .stdout(a_out.try_clone().expect("a.log"))
        .stderr(a_out)
        .process_group(0);
    let mut script = Started {
        child: command.spawn().expect("script starts"),
        reaped: false,
    };

    let received = expect_line(&b_lines, "received", SETUP_BUDGET, &why);
    let from_a = &received["from_a"];
    let a_pid = from_a["a"]["pid"].as_i64().expect("A's pid") as i32;
    let first_pid = from_a["first_pid"].as_i64().expect("the first stand-in's pid") as i32;
    let second_pid = from_a["second_pid"].as_i64().expect("the second stand-in's pid") as i32;
    let a = Watched::open(a_pid);
    let first = Watched::open(first_pid);
    let second = Watched::open(second_pid);
    let before = json!({ "a": stat_json(a_pid), "first": stat_json(first_pid), "second": stat_json(second_pid) });
    let a_stat = proc_stat(a_pid).expect("A is running");
    let first_stat = proc_stat(first_pid).expect("the first stand-in is running");
    let second_stat = proc_stat(second_pid).expect("the second stand-in is running");

    // The hangup kills `script`, and the pty's master closes with it; Ctrl+C is `SIGINT` to A's process group.
    let hang_up = |script: &mut Started| {
        script.child.kill().expect("kill script");
        script.wait_within(HANGUP_BUDGET).expect("script is reaped");
    };
    let interrupt = || {
        if unsafe { libc::kill(-a_stat.pgrp, libc::SIGINT) } == 0 {
            json!("delivered")
        } else {
            json!(format!("not delivered: {}", std::io::Error::last_os_error()))
        }
    };
    let mut steps = serde_json::Map::new();
    let (a_exit, first_exit) = match ending {
        Ending::HangupThenSigint => {
            hang_up(&mut script);
            let a_exit = a.exit_within(HANGUP_BUDGET);
            steps.insert("first_alive_after_a_exit".into(), json!(first.running()));
            steps.insert("second_alive_after_a_exit".into(), json!(second.running()));
            steps.insert(
                "after_hangup".into(),
                json!({ "first": stat_json(first_pid), "second": stat_json(second_pid) }),
            );
            steps.insert("sigint".into(), interrupt());
            (a_exit, first.exit_within(SIGINT_BUDGET))
        }
        Ending::SigintThenHangup => {
            steps.insert("sigint".into(), interrupt());
            let a_exit = a.exit_within(SIGINT_BUDGET);
            let first_exit = first.exit_within(SIGINT_BUDGET);
            steps.insert("second_alive_after_sigint".into(), json!(second.running()));
            hang_up(&mut script);
            steps.insert("after_hangup".into(), json!({ "second": stat_json(second_pid) }));
            (a_exit, first_exit)
        }
    };
    tell(&mut b_stdin, "probe-second");
    expect_line(&b_lines, "probed", SETUP_BUDGET, &why);
    let ack = sock.with_extension("ack");
    let deadline = Instant::now() + SETUP_BUDGET;
    while !ack.exists() {
        assert!(Instant::now() < deadline, "the survivor never acknowledged input");
        std::thread::sleep(Duration::from_millis(5));
    }
    let second_alive_after_both = second.running();
    let after_both = json!({ "second": stat_json(second_pid) });

    // B closes the last copy of the survivor's stdin.
    tell(&mut b_stdin, "close-second");
    expect_line(&b_lines, "closed", SETUP_BUDGET, &why);
    let second_exit_on_eof = second.exit_within(EOF_BUDGET);
    assert!(
        ack.with_extension("ack.eof").exists(),
        "the survivor read EOF rather than dying from a signal"
    );
    tell(&mut b_stdin, "exit");
    drop(b_stdin);
    let b_status = b.wait_within(Duration::from_secs(10));

    let control = &received["control"];
    let all_cloexec = received["cloexec"]
        .as_array()
        .is_some_and(|v| v.iter().all(|c| c == &json!(true)));
    let leaked_numbers = received["leaked_numbers"].as_array().map_or(0, Vec::len);
    let leaked_targets = received["leaked_targets"].as_array().map_or(0, Vec::len);
    let claim_a = first_exit.is_some() && second_alive_after_both && second_exit_on_eof.is_some();
    let claim_c = all_cloexec
        && leaked_numbers == 0
        && leaked_targets == 0
        && control["seen_by_number"] == json!(true)
        && control["seen_by_target"] == json!(true);
    let record = json!({
        "verdict": if claim_a && claim_c { "PASS" } else { "FAIL" },
        "claim_a_process_groups": claim_a,
        "claim_c_close_on_exec": claim_c,
        "ending": format!("{ending:?}"),
        "before": before,
        "a_exit_ms": millis(a_exit),
        "first_exit_ms": millis(first_exit),
        "steps": steps,
        "second_acknowledged_after_both": second_alive_after_both,
        "after_both": after_both,
        "second_exit_after_eof_ms": millis(second_exit_on_eof),
        "b_status": b_status,
        "received": received,
    });
    println!(
        "PK0 record ({ending:?}): {}",
        serde_json::to_string_pretty(&record).expect("json")
    );

    // The layout the claims rest on, checked after the record is printed so a surprise is still recorded.
    assert_eq!(
        from_a["a"]["stdin_is_a_tty"],
        json!(true),
        "A runs on a terminal of its own"
    );
    assert_eq!(
        a_stat.session, a_pid,
        "A leads its session, as a program started from a shell does"
    );
    assert_eq!(a_stat.pgrp, a_pid, "A leads its process group");
    assert_ne!(a_stat.tty_nr, 0, "A has a controlling terminal");
    assert_eq!(
        first_stat.pgrp, a_pid,
        "a child spawned without its own group stays in its spawner's"
    );
    assert_eq!(
        second_stat.pgrp, second_pid,
        "a child spawned with process_group(0) leads its own group"
    );
    assert_eq!(
        second_stat.session, a_pid,
        "its own group, but still its spawner's session"
    );
    assert!(a_exit.is_some(), "A exits on the hangup or the SIGINT; {}", why());

    assert!(
        first_exit.is_some(),
        "the child in the spawner's group dies with the spawner's terminal or its Ctrl+C"
    );
    assert!(
        second_alive_after_both,
        "the child in its own group survives the hangup and the SIGINT"
    );
    assert!(
        second_exit_on_eof.is_some(),
        "the surviving child ends once the last copy of its stdin closes"
    );
    assert!(
        all_cloexec,
        "received descriptors are close-on-exec: {:?}",
        received["cloexec"]
    );
    assert_eq!(
        control["seen_by_number"],
        json!(true),
        "the listing shows an inheritable descriptor"
    );
    assert_eq!(
        control["seen_by_target"],
        json!(true),
        "the long listing shows an inheritable pipe"
    );
    assert_eq!(
        leaked_numbers, 0,
        "a child lists no received descriptor: {:?}",
        received["leaked_numbers"]
    );
    assert_eq!(
        leaked_targets, 0,
        "a child holds no received pipe: {:?}",
        received["leaked_targets"]
    );
    let _ = fs::remove_dir_all(&scratch);
}
