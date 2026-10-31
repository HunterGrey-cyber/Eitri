//! Tests whether an ACP or Codex agent continues its turn after its stdio is handed to
//! another process. Both tests are ignored; run with `-- --ignored --nocapture`.
//! The ACP stand-in requests permission, then a helper receives stdin, stdout and stderr
//! via `SCM_RIGHTS`, answers the request and reads the rest of the turn.
//! The Codex test uses `codex app-server` with a scripted local model, an isolated
//! `CODEX_HOME`, no login, and `TMUX` and `TMUX_PANE` unset. It waits for command approval.
//! In both tests the parent closes its copies of the descriptors after handing them over.
//! The helper later closes stdin and uses a pidfd to check whether the agent exits within 2 s.
//! All stdin messages are logged so each JSON-RPC response id can be checked for uniqueness.
//! The parent reads stdout one byte at a time to avoid retaining unread data in its own buffer.
//! The tests use a plain line reader because `JsonRpcLink` cannot yet stop its reader thread
//! and hand the descriptors over.

#![cfg(target_os = "linux")]

#[path = "support/codex_peer.rs"]
mod codex_peer;

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use agent::external::{DirTrust, Reviewer};
use agent::providers::codex::scripted_model::{ScriptedModel, Step};
use agent::providers::codex::wire::{method, InitializeParams, TurnStartParams};
use serde_json::{json, Value};

use codex_peer::{call_outputs, json_str, real_codex, start_params, text_of, write_home, write_project, Scratch};

const HELPER_ENV: &str = "PX0_HELPER";
/// The descriptor number the helper finds its end of the socketpair on.
const HELPER_SOCK_FD: RawFd = 3;
const HELPER_TEST: &str = "helper_finishes_a_turn_on_received_agent_stdio";

/// One request's answer.
const CALL_BUDGET: Duration = Duration::from_secs(30);
/// A turn, from its start to its end.
const TURN_BUDGET: Duration = Duration::from_secs(60);
/// How long an agent whose stdin was closed has to exit before it is noted as one that does not.
const EXIT_ON_EOF: Duration = Duration::from_secs(2);
/// The helper's whole run must finish well inside this, or something is stuck.
const HELPER_DEADLINE: Duration = Duration::from_secs(120);

// ------------------------------------------------------------------------------------ the wire

/// One end of an agent's stdio: newline-delimited JSON-RPC, read a byte at a time so that nothing past the
/// last message consumed is held back in this process.
struct Wire {
    stdin: Option<File>,
    stdout: OwnedFd,
    stderr: OwnedFd,
    /// ACP carries `"jsonrpc":"2.0"`, the Codex app-server leaves it out.
    jsonrpc_member: bool,
    /// Every line this process writes to the agent's stdin, tagged with who wrote it.
    log: File,
    who: &'static str,
    stderr_text: Vec<u8>,
    stdout_open: bool,
    /// Messages read, in order.
    read: Vec<Value>,
}

impl Wire {
    fn new(
        stdin: OwnedFd,
        stdout: OwnedFd,
        stderr: OwnedFd,
        jsonrpc_member: bool,
        log: &Path,
        who: &'static str,
    ) -> Wire {
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .expect("the stdin log opens");
        Wire {
            stdin: Some(File::from(stdin)),
            stdout,
            stderr,
            jsonrpc_member,
            log,
            who,
            stderr_text: Vec::new(),
            stdout_open: true,
            read: Vec::new(),
        }
    }

    fn send(&mut self, mut message: Value) {
        if self.jsonrpc_member {
            message["jsonrpc"] = json!("2.0");
        }
        let line = format!("{message}\n");
        let stdin = self.stdin.as_mut().expect("stdin is still open");
        stdin
            .write_all(line.as_bytes())
            .expect("the agent's stdin takes the line");
        // One write per line, opened for appending: lines from both processes never interleave.
        self.log
            .write_all(format!("{}\t{line}", self.who).as_bytes())
            .expect("the stdin log takes the line");
    }

    fn request(&mut self, id: u64, name: &str, params: Value) {
        self.send(json!({"id": id, "method": name, "params": params}));
    }

    fn respond(&mut self, id: Value, result: Value) {
        self.send(json!({"id": id, "result": result}));
    }

    /// Closes this process's copy of the agent's stdin.
    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Waits for stdout or stderr, draining stderr; true once stdout has a byte to read (or is closed).
    fn wait_stdout(&mut self, until: Instant) -> bool {
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            let mut fds = [
                libc::pollfd {
                    fd: self.stdout.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.stderr.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, left.as_millis().clamp(1, 1000) as i32) };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                assert_eq!(err.raw_os_error(), Some(libc::EINTR), "poll: {err}");
                continue;
            }
            if fds[1].revents != 0 {
                self.drain_stderr();
            }
            if fds[0].revents != 0 {
                return true;
            }
        }
    }

    fn drain_stderr(&mut self) {
        let mut buf = [0u8; 4096];
        let n = unsafe { libc::read(self.stderr.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            self.stderr_text.extend_from_slice(&buf[..n as usize]);
        }
    }

    fn stderr_tail(&self) -> String {
        let text = String::from_utf8_lossy(&self.stderr_text);
        let start = text.len().saturating_sub(2000);
        text.get(start..).unwrap_or(&text).to_string()
    }

    /// The next message, or `None` on end of file. Panics when `until` passes first.
    fn next(&mut self, until: Instant) -> Option<Value> {
        let mut line = Vec::new();
        loop {
            assert!(
                self.wait_stdout(until),
                "{}: no message in time; read so far {} messages; stderr: {}",
                self.who,
                self.read.len(),
                self.stderr_tail()
            );
            let mut byte = 0u8;
            let n = unsafe { libc::read(self.stdout.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) };
            if n == 0 {
                self.stdout_open = false;
                return None;
            }
            if n < 0 {
                let err = std::io::Error::last_os_error();
                assert_eq!(err.raw_os_error(), Some(libc::EINTR), "read: {err}");
                continue;
            }
            if byte != b'\n' {
                line.push(byte);
                continue;
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                line.clear();
                continue;
            }
            let message: Value = serde_json::from_slice(&line)
                .unwrap_or_else(|e| panic!("{}: not JSON ({e}): {}", self.who, String::from_utf8_lossy(&line)));
            self.read.push(message.clone());
            return Some(message);
        }
    }

    /// Reads until `wanted` holds for a message and returns it. A request from the agent that is not the one
    /// wanted is refused as unknown, as Eitri's link does.
    fn read_until(&mut self, within: Duration, what: &str, mut wanted: impl FnMut(&Value) -> bool) -> Value {
        let until = Instant::now() + within;
        loop {
            let Some(message) = self.next(until) else {
                panic!(
                    "{}: the agent's stdout ended waiting for {what}; stderr: {}",
                    self.who,
                    self.stderr_tail()
                );
            };
            if wanted(&message) {
                return message;
            }
            if message.get("method").is_some() && message.get("id").is_some() {
                let id = message["id"].clone();
                self.send(json!({"id": id, "error": {"code": -32601, "message": "method not found"}}));
            }
        }
    }

    /// The answer to the request with this id.
    fn answer_to(&mut self, id: u64, what: &str) -> Value {
        let message = self.read_until(CALL_BUDGET, what, |m| m.get("method").is_none() && m["id"] == json!(id));
        if let Some(error) = message.get("error") {
            panic!("{what} was refused: {error}");
        }
        message["result"].clone()
    }
}

fn is_request(message: &Value, name: &str) -> bool {
    message["method"] == json!(name) && message.get("id").is_some()
}

// ------------------------------------------------------------------------------------ descriptors

/// A connected seqpacket pair, both ends close-on-exec.
fn seqpacket_pair() -> (OwnedFd, OwnedFd) {
    let mut socks = [-1 as RawFd; 2];
    let rc = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            socks.as_mut_ptr(),
        )
    };
    assert_eq!(rc, 0, "socketpair: {}", std::io::Error::last_os_error());
    // SAFETY: both descriptors were just made and are owned by nothing else.
    unsafe { (OwnedFd::from_raw_fd(socks[0]), OwnedFd::from_raw_fd(socks[1])) }
}

/// Sends `payload` and, if any, `fds` in one packet.
fn send_packet(sock: RawFd, payload: &[u8], fds: &[RawFd]) {
    let mut iov = libc::iovec {
        iov_base: payload.as_ptr() as *mut libc::c_void,
        iov_len: payload.len(),
    };
    let data_len = std::mem::size_of_val(fds) as u32;
    let space = if fds.is_empty() {
        0
    } else {
        unsafe { libc::CMSG_SPACE(data_len) as usize }
    };
    let mut control = vec![0u8; space];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
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
    loop {
        let left = until.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "no packet arrived in time");
        let mut pfd = libc::pollfd {
            fd: sock,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut pfd, 1, left.as_millis().clamp(1, 1000) as i32) } > 0 {
            break;
        }
    }
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
                let count = bytes / std::mem::size_of::<RawFd>();
                let data = libc::CMSG_DATA(header).cast::<RawFd>();
                for i in 0..count {
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

// ------------------------------------------------------------------------------------ processes

/// An agent started by the test, in a process group of its own; the whole group is killed if the test ends
/// before the agent has been reaped.
struct Agent {
    child: Child,
    reaped: bool,
}

impl Agent {
    fn spawn(mut command: Command) -> (Agent, OwnedFd, OwnedFd, OwnedFd) {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = command.spawn().expect("the agent starts");
        let stdin = OwnedFd::from(child.stdin.take().expect("stdin"));
        let stdout = OwnedFd::from(child.stdout.take().expect("stdout"));
        let stderr = OwnedFd::from(child.stderr.take().expect("stderr"));
        (Agent { child, reaped: false }, stdin, stdout, stderr)
    }

    fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    /// Waits for the agent to exit; kills its group if it has not by then. Returns its status text and
    /// whether anything else in its process group was left running.
    fn finish(&mut self, within: Duration) -> (String, bool) {
        let until = Instant::now() + within;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                self.reaped = true;
                let left_behind = unsafe { libc::kill(-self.pid(), 0) } == 0;
                if left_behind {
                    // SAFETY: the group is the agent's own, led by the pid this test started.
                    unsafe { libc::kill(-self.pid(), libc::SIGKILL) };
                }
                return (status.to_string(), left_behind);
            }
            if Instant::now() >= until {
                // SAFETY: as above; the agent has not been reaped, so its pid is still its own.
                unsafe { libc::kill(-self.pid(), libc::SIGKILL) };
                let status = self.child.wait().expect("wait");
                self.reaped = true;
                return (format!("killed after {within:?} ({status})"), false);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        if !self.reaped {
            // SAFETY: not reaped, so the pid and its group are still the agent's.
            unsafe { libc::kill(-self.pid(), libc::SIGKILL) };
            let _ = self.child.wait();
        }
    }
}

/// The helper process; killed if the test ends before it has been reaped.
struct Helper {
    child: Child,
    reaped: bool,
}

impl Helper {
    /// The test binary re-run as the helper, with `theirs` on descriptor 3.
    fn spawn(theirs: OwnedFd) -> Helper {
        let raw = theirs.as_raw_fd();
        let mut command = Command::new(std::env::current_exe().expect("the test binary"));
        command
            .args([
                HELPER_TEST,
                "--exact",
                "--ignored",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .env(HELPER_ENV, "1");
        // SAFETY: runs in the forked child before exec and calls only async-signal-safe functions.
        unsafe {
            command.pre_exec(move || {
                if raw == HELPER_SOCK_FD {
                    // dup2 onto itself would keep close-on-exec; clear it instead.
                    let flags = libc::fcntl(raw, libc::F_GETFD);
                    if flags == -1 || libc::fcntl(raw, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                } else if libc::dup2(raw, HELPER_SOCK_FD) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("the helper starts");
        drop(theirs);
        Helper { child, reaped: false }
    }

    fn wait(&mut self, within: Duration) -> String {
        let until = Instant::now() + within;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                self.reaped = true;
                return status.to_string();
            }
            assert!(Instant::now() < until, "the helper did not finish");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Waits on a pidfd for the process to exit; the time it took, or `None` when `within` passed first.
fn exit_within(pid: i32, within: Duration, wire: &mut Wire) -> Option<Duration> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as RawFd;
    assert!(fd >= 0, "pidfd_open: {}", std::io::Error::last_os_error());
    // SAFETY: the descriptor was just opened and is owned by nothing else.
    let pidfd = unsafe { OwnedFd::from_raw_fd(fd) };
    let at = Instant::now();
    let until = at + within;
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        let mut fds = [
            libc::pollfd {
                fd: pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if wire.stdout_open { wire.stdout.as_raw_fd() } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wire.stderr.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 3, left.as_millis().clamp(1, 100) as i32) };
        if n <= 0 {
            continue;
        }
        if fds[0].revents != 0 {
            return Some(at.elapsed());
        }
        // Whatever the agent still writes is read, so that a full pipe never holds its exit up.
        if fds[1].revents != 0 {
            let mut buf = [0u8; 4096];
            let n = unsafe { libc::read(wire.stdout.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n == 0 {
                wire.stdout_open = false;
            }
        }
        if fds[2].revents != 0 {
            wire.drain_stderr();
        }
    }
}

// ------------------------------------------------------------------------------------ the handover

/// What the parent tells the helper about the turn it hands over.
fn handover(wire: Wire, pid: i32, turn: Value, log: &Path) -> Value {
    let (mine, theirs) = seqpacket_pair();
    let mut helper = Helper::spawn(theirs);
    let payload = json!({"pid": pid, "jsonrpc_member": wire.jsonrpc_member, "log": text_of(log), "turn": turn});
    let stdin = wire.stdin.as_ref().expect("stdin is still open").as_raw_fd();
    send_packet(
        mine.as_raw_fd(),
        payload.to_string().as_bytes(),
        &[stdin, wire.stdout.as_raw_fd(), wire.stderr.as_raw_fd()],
    );
    let at = Instant::now();
    // The parent lets go of all three now: the descriptors in flight keep them open.
    drop(wire);
    let (result, fds) = recv_packet(mine.as_raw_fd(), Instant::now() + HELPER_DEADLINE);
    assert!(fds.is_empty());
    let status = helper.wait(Duration::from_secs(30));
    let mut result: Value = serde_json::from_slice(&result).expect("the helper's result is JSON");
    result["helper_status"] = json!(status);
    result["handover_to_result_ms"] = json!(at.elapsed().as_millis() as u64);
    result
}

/// The helper's side: receives the agent's stdio, finishes the turn, closes stdin and watches the agent exit.
/// Without its environment variable (any ordinary `--ignored` run) it does nothing.
#[test]
#[ignore = "probe: run with --ignored"]
fn helper_finishes_a_turn_on_received_agent_stdio() {
    if std::env::var_os(HELPER_ENV).is_none() {
        return;
    }
    let (payload, fds) = recv_packet(HELPER_SOCK_FD, Instant::now() + HELPER_DEADLINE);
    let payload: Value = serde_json::from_slice(&payload).expect("the handover is JSON");
    assert_eq!(fds.len(), 3, "stdin, stdout and stderr arrive");
    let cloexec = fds.iter().all(|fd| is_cloexec(fd.as_raw_fd()));
    let mut fds = fds.into_iter();
    let (stdin, stdout, stderr) = (fds.next().unwrap(), fds.next().unwrap(), fds.next().unwrap());
    let log = PathBuf::from(payload["log"].as_str().expect("the log path"));
    let jsonrpc_member = payload["jsonrpc_member"].as_bool().unwrap_or(false);
    let mut wire = Wire::new(stdin, stdout, stderr, jsonrpc_member, &log, "helper");
    let pid = payload["pid"].as_i64().expect("the agent's pid") as i32;
    let turn = &payload["turn"];

    let mut result = match turn["kind"].as_str() {
        Some("acp") => finish_acp_turn(&mut wire, turn),
        Some("codex") => finish_codex_turn(&mut wire, turn),
        other => panic!("unknown turn kind {other:?}"),
    };
    result["received_cloexec"] = json!(cloexec);
    result["messages_read_by_helper"] = json!(wire.read.len());

    wire.close_stdin();
    let exited = exit_within(pid, EXIT_ON_EOF, &mut wire);
    result["exit_after_eof_ms"] = json!(exited.map(|d| d.as_millis() as u64));
    result["stderr_tail"] = json!(wire.stderr_tail());
    send_packet(HELPER_SOCK_FD, result.to_string().as_bytes(), &[]);
}

/// Every response written to the agent's stdin, by id, and who wrote it.
fn responses_in_log(log: &Path) -> BTreeMap<String, Vec<String>> {
    let mut by_id: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in fs::read_to_string(log).expect("the stdin log").lines() {
        let (who, text) = line.split_once('\t').expect("a tagged line");
        let message: Value = serde_json::from_str(text).expect("a JSON line");
        if message.get("method").is_none() {
            by_id
                .entry(message["id"].to_string())
                .or_default()
                .push(who.to_string());
        }
    }
    by_id
}

fn assert_each_response_once(log: &Path) -> BTreeMap<String, Vec<String>> {
    let by_id = responses_in_log(log);
    for (id, writers) in &by_id {
        assert_eq!(
            writers.len(),
            1,
            "the response to request {id} was written {} times",
            writers.len()
        );
    }
    by_id
}

// ------------------------------------------------------------------------------------ ACP

fn finish_acp_turn(wire: &mut Wire, turn: &Value) -> Value {
    let permission_id = turn["permission_id"].clone();
    let prompt_id = turn["prompt_id"].as_u64().expect("the prompt's id");
    wire.respond(
        permission_id,
        json!({"outcome": {"outcome": "selected", "optionId": "yes-once"}}),
    );
    let mut tool_status = None;
    let end = wire.read_until(TURN_BUDGET, "the prompt's answer", |m| {
        let update = &m["params"]["update"];
        if update["sessionUpdate"] == json!("tool_call_update") {
            tool_status = update["status"].as_str().map(str::to_string);
        }
        m.get("method").is_none() && m["id"] == json!(prompt_id)
    });
    json!({
        "stop_reason": end["result"]["stopReason"],
        "tool_status": tool_status,
    })
}

#[test]
#[ignore = "probe: runs tools/measure/acp_stand_in.py; run with --ignored"]
fn an_acp_agent_keeps_its_turn_when_its_stdio_moves_to_another_process() {
    let s = Scratch::new("px0-acp");
    let project = s.dir("proj");
    let log = s.path("stdin.log");
    let agent_log = s.path("agent.log");
    let stand_in = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tools/measure/acp_stand_in.py");
    let stand_in = fs::canonicalize(&stand_in).expect("the ACP stand-in");

    let mut command = Command::new("python3");
    command
        .arg(&stand_in)
        .current_dir(&project)
        .env("ACP_AGENT_LOG", &agent_log)
        .env("ACP_AGENT_STORE", s.path("sessions.json"))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE");
    let (mut agent, stdin, stdout, stderr) = Agent::spawn(command);
    let mut wire = Wire::new(stdin, stdout, stderr, true, &log, "parent");

    wire.request(
        1,
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false}}),
    );
    wire.answer_to(1, "initialize");
    wire.request(2, "session/new", json!({"cwd": text_of(&project), "mcpServers": []}));
    let session = wire.answer_to(2, "session/new")["sessionId"]
        .as_str()
        .expect("a session id")
        .to_string();
    wire.request(
        3,
        "session/prompt",
        json!({"sessionId": session, "prompt": [{"type": "text", "text": "run the card please"}]}),
    );
    let asked = wire.read_until(CALL_BUDGET, "the permission request", |m| {
        is_request(m, "session/request_permission")
    });
    let permission_id = asked["id"].clone();
    let read_by_parent = wire.read.len();

    let turn = json!({"kind": "acp", "permission_id": permission_id, "prompt_id": 3});
    let result = handover(wire, agent.pid(), turn, &log);
    let (status, left_behind) = agent.finish(Duration::from_secs(10));

    eprintln!("px0 acp: python3 {}", python_version());
    eprintln!("px0 acp: the parent read {read_by_parent} messages, the permission request's id {permission_id}");
    eprintln!("px0 acp: helper result {result}");
    eprintln!("px0 acp: agent status {status}; group left running: {left_behind}");

    assert_eq!(
        result["helper_status"],
        json!("exit status: 0"),
        "the helper failed: {result}"
    );
    assert_eq!(
        result["received_cloexec"],
        json!(true),
        "a received descriptor must be close-on-exec"
    );
    assert_eq!(
        result["tool_status"],
        json!("completed"),
        "the allowed tool call did not complete in the helper"
    );
    assert_eq!(
        result["stop_reason"],
        json!("end_turn"),
        "the turn did not end normally in the helper"
    );

    let responses = assert_each_response_once(&log);
    assert_eq!(
        responses.get(&permission_id.to_string()).map(Vec::as_slice),
        Some(&["helper".to_string()][..]),
        "the permission answer must come from the helper, once"
    );
    // The agent's own log of what it read: the answer arrived once.
    let read_by_agent = fs::read_to_string(&agent_log).expect("the stand-in's log");
    let answers_read = read_by_agent
        .lines()
        .filter_map(|l| l.split_once(" << ").map(|(_, text)| text))
        .filter_map(|text| serde_json::from_str::<Value>(text).ok())
        .filter(|m| m.get("method").is_none() && m["id"] == permission_id)
        .count();
    assert_eq!(
        answers_read, 1,
        "the stand-in read the permission answer {answers_read} times"
    );
    eprintln!(
        "px0 acp: exit after stdin EOF: {}",
        exit_note(&result["exit_after_eof_ms"])
    );
}

// ------------------------------------------------------------------------------------ Codex

fn finish_codex_turn(wire: &mut Wire, turn: &Value) -> Value {
    let thread = turn["thread"].as_str().expect("the thread").to_string();
    let turn_id = turn["turn"].as_str().expect("the turn").to_string();
    wire.respond(turn["approval_id"].clone(), json!({"decision": "accept"}));
    let mut resolved = false;
    let mut command_status = None;
    let end = wire.read_until(TURN_BUDGET, "turn/completed", |m| {
        let name = m["method"].as_str().unwrap_or_default();
        if name == method::SERVER_REQUEST_RESOLVED {
            resolved = true;
        }
        if name == method::ITEM_COMPLETED && json_str(m, "/params/item/type") == Some("commandExecution") {
            command_status = json_str(m, "/params/item/status").map(str::to_string);
        }
        name == method::TURN_COMPLETED
            && json_str(m, "/params/threadId") == Some(thread.as_str())
            && json_str(m, "/params/turn/id") == Some(turn_id.as_str())
    });
    json!({
        "turn_status": json_str(&end, "/params/turn/status"),
        "request_resolved_seen": resolved,
        "command_status": command_status,
    })
}

#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn a_codex_app_server_keeps_its_turn_when_its_stdio_moves_to_another_process() {
    let codex = real_codex();
    let s = Scratch::new("px0-codex");
    let outside = s.dir("outside");
    let marker = outside.join("approved");
    let model = ScriptedModel::start(vec![Step::exec("px1", &format!("touch {}", text_of(&marker)), true)
        .when_body_has("PX0-PROMPT")
        .unless_body_has("\"px1\"")])
    .expect("model");
    let home = s.dir("home");
    write_home(&home, &model, "");
    let project = s.dir("proj");
    write_project(&project, "");
    let log = s.path("stdin.log");

    let mut command = Command::new(&codex);
    command
        .args(["app-server", "--listen", "stdio://"])
        .current_dir(&project)
        .env("CODEX_HOME", &home)
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost");
    for name in [
        "TMUX",
        "TMUX_PANE",
        "NVIM",
        "NVIM_LISTEN_ADDRESS",
        "OPENAI_API_KEY",
        "CODEX_API_KEY",
    ] {
        command.env_remove(name);
    }
    let (mut agent, stdin, stdout, stderr) = Agent::spawn(command);
    let mut wire = Wire::new(stdin, stdout, stderr, false, &log, "parent");

    wire.request(
        1,
        method::INITIALIZE,
        serde_json::to_value(InitializeParams::eitri(env!("CARGO_PKG_VERSION"))).expect("params"),
    );
    let init = wire.answer_to(1, "initialize");
    assert_eq!(
        json_str(&init, "/codexHome").and_then(|p| fs::canonicalize(p).ok()),
        fs::canonicalize(&home).ok(),
        "codex does not use the private home it was given"
    );
    wire.send(json!({"method": method::INITIALIZED}));
    wire.request(2, method::THREAD_START, start_params(&project, DirTrust::Untrusted));
    let thread = json_str(&wire.answer_to(2, "thread/start"), "/thread/id")
        .expect("a thread id")
        .to_string();
    let params = TurnStartParams::new(
        thread.clone(),
        "PX0-PROMPT".to_string(),
        Reviewer::User,
        agent::Sandbox::Project,
    );
    wire.request(3, method::TURN_START, serde_json::to_value(params).expect("params"));
    let turn = json_str(&wire.answer_to(3, "turn/start"), "/turn/id")
        .expect("a turn id")
        .to_string();
    let asked = wire.read_until(TURN_BUDGET, "the command approval", |m| {
        is_request(m, method::COMMAND_APPROVAL)
    });
    let approval_id = asked["id"].clone();
    let read_by_parent = wire.read.len();
    assert!(!marker.exists(), "the command ran before it was approved");

    let handed = json!({"kind": "codex", "approval_id": approval_id, "thread": thread, "turn": turn});
    let result = handover(wire, agent.pid(), handed, &log);
    let (status, left_behind) = agent.finish(Duration::from_secs(10));

    eprintln!("px0 codex: the parent read {read_by_parent} messages, the approval request's id {approval_id}");
    eprintln!("px0 codex: helper result {result}");
    eprintln!("px0 codex: agent status {status}; group left running: {left_behind}");

    assert_eq!(
        result["helper_status"],
        json!("exit status: 0"),
        "the helper failed: {result}"
    );
    assert_eq!(
        result["received_cloexec"],
        json!(true),
        "a received descriptor must be close-on-exec"
    );
    assert_eq!(
        result["turn_status"],
        json!("completed"),
        "the turn did not complete in the helper"
    );
    assert!(marker.exists(), "the command approved by the helper did not run");
    assert!(
        call_outputs(&model).contains_key("px1"),
        "the command's output never reached the model"
    );

    let responses = assert_each_response_once(&log);
    assert_eq!(
        responses.get(&approval_id.to_string()).map(Vec::as_slice),
        Some(&["helper".to_string()][..]),
        "the approval must come from the helper, once"
    );
    eprintln!(
        "px0 codex: exit after stdin EOF: {}",
        exit_note(&result["exit_after_eof_ms"])
    );
}

fn exit_note(ms: &Value) -> String {
    match ms.as_u64() {
        Some(ms) => format!("exited {ms} ms after its stdin closed"),
        None => format!("did not exit within {EXIT_ON_EOF:?} of its stdin closing"),
    }
}

fn python_version() -> String {
    Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}
