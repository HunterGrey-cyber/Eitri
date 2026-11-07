//! `eitri remote serve` end to end: the product binary's `remote` subcommands in a home of their own, and the sandbox
//! in child processes of this test (its rules bind a process for good, so it is never set up in this one).
//!
//! A plain `main` (`harness = false`): `own_display_scan` requires a test that starts the product binary to isolate its
//! display in `main`, which libtest's harness never runs, and the sandbox must be set up while a process has one
//! thread, which only a process this test starts itself can promise. The children get no display.
//!
//! A child is this same binary started again with `EITRI_REMOTE_SERVE_TEST_ROLE` set; it does its one thing, prints
//! what it saw, and exits before `main` reaches the display.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

// The display this test runs on: its own Xvfb, never an inherited one.
#[path = "support/own_x_server.rs"]
mod own_x_server;

// `serve`'s sessions, in this process.
#[path = "remote_serve/phones.rs"]
mod phones;

/// Names the role a child of this test plays.
const ROLE: &str = "EITRI_REMOTE_SERVE_TEST_ROLE";
/// How long a child or a run of the binary may take before the test calls it hung. Not a speed bound: each ends as
/// soon as it is done.
const HANG: Duration = Duration::from_secs(60);

fn main() {
    if let Ok(role) = std::env::var(ROLE) {
        std::process::exit(child::run(&role));
    }
    // Before anything that could connect to a display or a bus.
    let server = own_x_server::isolate("remote_serve", "640x480x24");
    let shell = env!("CARGO_BIN_EXE_shell");
    let scratch: PathBuf = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("rs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).expect("creating the scratch directory must succeed");

    let checks: Vec<(&str, fn(&str, &Path) -> Result<(), String>)> = vec![
        (
            "sandbox_forbids_exec_and_writes_in_every_thread",
            sandbox_forbids_exec_and_writes_in_every_thread,
        ),
        (
            "sandbox_refuses_to_run_beside_a_second_thread",
            sandbox_refuses_to_run_beside_a_second_thread,
        ),
        (
            "agents_feed_still_cleans_a_dead_writer_under_the_sandbox",
            agents_feed_still_cleans_a_dead_writer_under_the_sandbox,
        ),
        ("wildcard_listen_is_refused", wildcard_listen_is_refused),
        ("unconfirmed_address_is_not_bound", unconfirmed_address_is_not_bound),
        (
            "serve_is_not_dumpable_and_status_says_how_it_runs",
            serve_is_not_dumpable_and_status_says_how_it_runs,
        ),
    ];
    // `cargo test --test remote_serve -- <part of a name>` runs the checks whose names hold it.
    let filter = std::env::args().skip(1).find(|arg| !arg.starts_with('-'));
    let wanted = |name: &str| filter.as_deref().is_none_or(|part| name.contains(part));
    let mut failed = 0;
    let mut ran = 0;
    for (at, (name, check)) in checks.iter().enumerate() {
        if !wanted(name) {
            continue;
        }
        ran += 1;
        let dir = scratch.join(at.to_string());
        std::fs::create_dir_all(&dir).expect("creating a check's directory must succeed");
        match check(shell, &dir) {
            Ok(()) => println!("test {name} ... ok"),
            Err(why) => {
                failed += 1;
                println!("test {name} ... FAILED\n    {why}");
            }
        }
    }
    let in_process = phones::checks();
    for (at, (name, check)) in in_process.iter().enumerate() {
        if !wanted(name) {
            continue;
        }
        ran += 1;
        let dir = scratch.join(format!("p{at}"));
        std::fs::create_dir_all(&dir).expect("creating a check's directory must succeed");
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&dir)))
            .unwrap_or_else(|_| Err("panicked".to_owned()));
        match outcome {
            Ok(()) => println!("test {name} ... ok"),
            Err(why) => {
                failed += 1;
                println!("test {name} ... FAILED\n    {why}");
            }
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
    println!("remote_serve: {} passed, {failed} failed", ran - failed);
    server.exit(if failed == 0 { 0 } else { 1 });
}

/// The directories a home of its own gives `serve`, made 0700.
struct Home {
    root: PathBuf,
}

impl Home {
    fn new(dir: &Path) -> Home {
        use std::os::unix::fs::PermissionsExt;
        for sub in ["home", "run", "state"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
            std::fs::set_permissions(dir.join(sub), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        Home { root: dir.to_owned() }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn control(&self) -> PathBuf {
        self.root.join("run").join("eitri")
    }

    fn agents(&self) -> PathBuf {
        self.control().join("agents")
    }

    fn store(&self) -> PathBuf {
        self.root.join("state").join("eitri").join("remote")
    }

    /// The control, agents and store directories, made as `serve` makes them.
    fn make_dirs(&self) {
        use std::os::unix::fs::PermissionsExt;
        for dir in [self.control(), self.agents(), self.store()] {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        std::fs::set_permissions(self.root.join("state/eitri"), std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .env("HOME", self.home())
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .env_remove("XAUTHORITY")
            .env_remove("GDK_BACKEND")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// This test binary as a child playing `role`, given the home's directories.
    fn child(&self, role: &str) -> Command {
        let exe = std::env::current_exe().expect("the test binary has a path");
        let mut command = self.command(exe.to_str().expect("a UTF-8 path"));
        command
            .env(ROLE, role)
            .env("RS_AGENTS", self.agents())
            .env("RS_CONTROL", self.control())
            .env("RS_STORE", self.store())
            .env("RS_HOME", self.home());
        command
    }
}

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    fn says(&self) -> String {
        format!(
            "exit {:?}; stdout: {}; stderr: {}",
            self.code,
            self.stdout.trim_end(),
            self.stderr.trim_end()
        )
    }
}

/// Runs `command` to its end, killed by its pid after [`HANG`].
fn run(mut command: Command) -> Result<Run, String> {
    let child = command.spawn().map_err(|e| format!("did not start: {e}"))?;
    let pid = child.id();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(child.wait_with_output());
    });
    match receiver.recv_timeout(HANG) {
        Ok(Ok(output)) => Ok(Run {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }),
        Ok(Err(e)) => Err(format!("waiting failed: {e}")),
        Err(_) => {
            // SAFETY: `kill` takes no pointers; the pid is this run's own child, not yet reaped by its waiter.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            Err(format!("still running after {} s", HANG.as_secs()))
        }
    }
}

/// A child left running; killed by its pid and reaped when dropped.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn shell_remote(shell: &str, home: &Home, args: &[&str]) -> Command {
    let mut command = home.command(shell);
    command.arg("remote").args(args);
    command
}

fn kernel_landlock_abi() -> u32 {
    // SAFETY: the version query takes a null pointer and size zero and writes no memory.
    let abi = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<u8>(), 0usize, 1u32) };
    if abi > 0 {
        abi as u32
    } else {
        0
    }
}

fn landlock_in_kernel() -> bool {
    kernel_landlock_abi() > 0
}

fn sandbox_forbids_exec_and_writes_in_every_thread(_shell: &str, dir: &Path) -> Result<(), String> {
    let home = Home::new(dir);
    home.make_dirs();
    std::fs::write(home.store().join("devices.json"), b"{}").unwrap();
    std::fs::write(home.agents().join("probe"), b"").unwrap();
    std::fs::write(home.home().join("truncate-me"), b"kept").unwrap();
    let run = run(home.child("sandbox-thread"))?;
    let out = &run.stdout;
    let abi = kernel_landlock_abi();
    let mut wanted = vec![
        "state ok",
        "thread exec refused",
        "thread agents-write ok",
        "thread store-read ok",
        // Refused by seccomp, whatever Landlock ABI the kernel has.
        "thread home-truncate EPERM",
        "dumpable false",
    ];
    wanted.push(if abi >= 9 { "sandbox full" } else { "sandbox partial" });
    if abi > 0 {
        wanted.extend(["thread home-write refused", "thread store-write refused"]);
    }
    for line in wanted {
        if !out.lines().any(|l| l == line) {
            return Err(format!("no line {line:?}; {}", run.says()));
        }
    }
    if run.code != Some(0) {
        return Err(run.says());
    }
    // Without Landlock nothing stops that write; only seccomp's refusals hold there.
    if abi > 0 && home.home().join("written-by-a-thread").exists() {
        return Err("a thread wrote under $HOME".to_owned());
    }
    if std::fs::read(home.home().join("truncate-me")).ok().as_deref() != Some(b"kept".as_slice()) {
        return Err("a thread truncated a file under $HOME".to_owned());
    }
    Ok(())
}

fn sandbox_refuses_to_run_beside_a_second_thread(_shell: &str, dir: &Path) -> Result<(), String> {
    let home = Home::new(dir);
    home.make_dirs();
    let run = run(home.child("sandbox-busy"))?;
    if run.code != Some(0) || !run.stdout.lines().any(|l| l == "refused, and dumpable true") {
        return Err(run.says());
    }
    Ok(())
}

fn agents_feed_still_cleans_a_dead_writer_under_the_sandbox(_shell: &str, dir: &Path) -> Result<(), String> {
    let home = Home::new(dir);
    home.make_dirs();
    let mut publisher = home.child("publisher");
    publisher.stderr(Stdio::inherit());
    let mut publisher = Running(
        publisher
            .spawn()
            .map_err(|e| format!("the publisher did not start: {e}"))?,
    );
    let stdout = publisher.0.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    let line = rx
        .recv_timeout(HANG)
        .map_err(|_| "the publisher never said it was ready".to_owned())?;
    let instance = line
        .trim()
        .strip_prefix("ready ")
        .ok_or_else(|| format!("the publisher said {line:?}"))?
        .to_owned();
    let files = [
        home.agents().join(format!("{instance}.json")),
        home.agents().join(format!("{instance}.lock")),
        home.control().join(format!("ag-{instance}.sock")),
    ];
    for file in &files {
        if !file.exists() {
            return Err(format!("the publisher left no {}", file.display()));
        }
    }
    // Killed as a crashed window is: nothing of its own runs to clean up.
    drop(publisher);

    let run = run(home.child("feed-sandboxed"))?;
    if run.code != Some(0) {
        return Err(run.says());
    }
    if !run.stdout.lines().any(|l| l == "rows 0") {
        return Err(format!("the dead writer's row is still shown; {}", run.says()));
    }
    if files[0].exists() {
        return Err(format!("{} is still there; {}", files[0].display(), run.says()));
    }
    // The sandbox removes nothing beneath the control directory, so the socket, and the lock that marks the rest for
    // cleaning, are left for a window's reader.
    if landlock_in_kernel() && !(files[1].exists() && files[2].exists()) {
        return Err(format!(
            "the sandboxed reader removed what the sandbox leaves to a window; {}",
            run.says()
        ));
    }
    // A window's reader takes a lock with no file beside it once it is older than a writer's poll.
    let until = Instant::now() + Duration::from_millis(3 * eitri_core::agents::LOCK_POLL_MS);
    loop {
        eitri_core::agents::scan_once(&home.control(), eitri_core::agents::SCHEMA_V)
            .map_err(|e| format!("a window's reader could not scan: {e}"))?;
        let Some(left) = files.iter().find(|file| file.exists()) else {
            return Ok(());
        };
        if Instant::now() > until {
            return Err(format!(
                "{} is still there after a window's reader; {}",
                left.display(),
                run.says()
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn wildcard_listen_is_refused(shell: &str, dir: &Path) -> Result<(), String> {
    let home = Home::new(dir);
    for wildcard in ["0.0.0.0", "::"] {
        let run = run(shell_remote(shell, &home, &["serve", "--listen", wildcard]))?;
        if run.code != Some(2) || !run.stderr.contains("wildcard") {
            return Err(format!("--listen {wildcard}: {}", run.says()));
        }
    }
    Ok(())
}

fn unconfirmed_address_is_not_bound(shell: &str, dir: &Path) -> Result<(), String> {
    let home = Home::new(dir);
    let own = eitri_remote::serve::listen::interface_addresses()
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|(_, addr, _)| addr.is_ipv4() && !addr.is_loopback());
    let Some((_, addr, _)) = own else {
        println!("    (no address but loopback here: nothing to refuse)");
        return Ok(());
    };
    let probe = std::net::TcpListener::bind((addr, 0)).map_err(|e| e.to_string())?;
    let port = probe.local_addr().unwrap().port().to_string();
    drop(probe);
    let run = run(shell_remote(
        shell,
        &home,
        &["serve", "--listen", &addr.to_string(), "--port", &port],
    ))?;
    if run.code != Some(2) || !run.stderr.contains("not confirmed at the desk") {
        return Err(run.says());
    }
    if std::net::TcpStream::connect((addr, port.parse::<u16>().unwrap())).is_ok() {
        return Err(format!("something listens on {addr}:{port}"));
    }
    Ok(())
}

/// Starts `serve` on the home's Unix socket and waits until `status` reports it.
fn start_serve(shell: &str, home: &Home) -> Result<(Running, Run), String> {
    let serve = shell_remote(shell, home, &["serve", "--listen", "socket"])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("serve did not start: {e}"))?;
    let mut serve = Running(serve);
    let until = Instant::now() + HANG;
    loop {
        if let Some(status) = serve.0.try_wait().map_err(|e| e.to_string())? {
            return Err(format!("serve ended at once: {status}"));
        }
        let status = run(shell_remote(shell, home, &["status"]))?;
        if status.code == Some(0) && status.stdout.contains("sandbox:") {
            return Ok((serve, status));
        }
        if Instant::now() > until {
            return Err(format!("serve was not reported running; {}", status.says()));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn serve_is_not_dumpable_and_status_says_how_it_runs(shell: &str, dir: &Path) -> Result<(), String> {
    let home = Home::new(dir);
    let (serve, status) = start_serve(shell, &home)?;
    let pid = serve.0.id();
    let record: eitri_remote::serve::config::Record =
        serde_json::from_slice(&std::fs::read(home.control().join(eitri_remote::serve::RECORD_FILE)).unwrap()).unwrap();
    let wanted_word = if kernel_landlock_abi() >= 9 { "full" } else { "partial" };
    if record.sandbox != wanted_word || (wanted_word == "partial" && record.sandbox_why.is_none()) {
        return Err(format!("sandbox record lacks the expected word or reason: {record:?}"));
    }
    let wanted_sandbox = format!("sandbox: {}", record.sandbox_text());
    for line in [
        &format!("running: pid {pid}") as &str,
        "listen: socket",
        "origins: none",
        &wanted_sandbox,
    ] {
        if !status.stdout.lines().any(|l| l == line) {
            return Err(format!("status lacks {line:?}; {}", status.says()));
        }
    }
    // Not dumpable: another process of this user cannot read its environment (nor its memory).
    match std::fs::read(format!("/proc/{pid}/environ")) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
        other => return Err(format!("/proc/{pid}/environ read as {other:?}")),
    }

    only_the_ws_path_upgrades_end_to_end(&home)?;
    second_serve_exits(shell, &home, pid)?;

    let stopped = run(shell_remote(shell, &home, &["stop"]))?;
    if stopped.code != Some(0) || stopped.stdout.trim() != "stopped" {
        return Err(format!("stop: {}", stopped.says()));
    }
    let after = run(shell_remote(shell, &home, &["status"]))?;
    if after.code != Some(3) || after.stdout.trim() != "not running" {
        return Err(format!("status after stop: {}", after.says()));
    }
    drop(serve);
    Ok(())
}

fn second_serve_exits(shell: &str, home: &Home, pid: u32) -> Result<(), String> {
    let same = run(shell_remote(shell, home, &["serve", "--listen", "socket"]))?;
    if same.code != Some(3) || !same.stderr.contains(&format!("already running (pid {pid})")) {
        return Err(format!("the same configuration again: {}", same.says()));
    }
    let other = run(shell_remote(
        shell,
        home,
        &["serve", "--listen", "loopback", "--origin", "desk.tail1.ts.net"],
    ))?;
    if other.code != Some(4)
        || !other.stderr.contains("listen: running socket, asked loopback")
        || !other.stderr.contains("origins: running none, asked desk.tail1.ts.net")
    {
        return Err(format!("another configuration: {}", other.says()));
    }
    Ok(())
}

/// Sends `request` over the home's Unix socket and `then` after the answer's head; returns what came back until the
/// server closed, or until `after_head` bytes followed the answer's head.
fn exchange(home: &Home, request: &[u8], then: &[u8], after_head: Option<usize>) -> Result<Vec<u8>, String> {
    let mut stream = UnixStream::connect(home.control().join("remote.sock")).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    stream.write_all(request).map_err(|e| e.to_string())?;
    let mut answer = Vec::new();
    let mut buf = [0u8; 4096];
    let mut sent_then = then.is_empty();
    loop {
        let head_end = answer.windows(4).position(|w| w == b"\r\n\r\n").map(|at| at + 4);
        if let (Some(end), Some(wanted)) = (head_end, after_head) {
            if answer.len() >= end + wanted {
                break;
            }
        }
        let got = stream.read(&mut buf).map_err(|e| format!("reading: {e}"))?;
        if got == 0 {
            break;
        }
        answer.extend_from_slice(&buf[..got]);
        if !sent_then && answer.windows(4).any(|w| w == b"\r\n\r\n") {
            stream.write_all(then).map_err(|e| e.to_string())?;
            sent_then = true;
        }
    }
    Ok(answer)
}

fn only_the_ws_path_upgrades_end_to_end(home: &Home) -> Result<(), String> {
    let upgrade = |target: &str, host: &str| {
        format!(
            "GET {target} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        .into_bytes()
    };
    let status_of = |answer: &[u8]| {
        String::from_utf8_lossy(answer)
            .chars()
            .skip(9)
            .take(3)
            .collect::<String>()
    };
    for (target, host, wanted) in [
        ("/", "127.0.0.1:41000", "404"),
        ("/index.html", "localhost", "404"),
        ("/eitri/v1/ws", "localhost", "404"),
        ("/v1/ws", "evil.example", "403"),
    ] {
        let answer = exchange(home, &upgrade(target, host), b"", None)?;
        if status_of(&answer) != wanted {
            return Err(format!("{target} as {host}: {:?}", String::from_utf8_lossy(&answer)));
        }
    }
    // A masked ping of "hi" after the 101; the pong carries it back unmasked.
    let ping = [0x89, 0x82, 1, 2, 3, 4, b'h' ^ 1, b'i' ^ 2];
    let answer = exchange(home, &upgrade("/v1/ws", "127.0.0.1:41000"), &ping, Some(4))?;
    let text = String::from_utf8_lossy(&answer);
    if !text.starts_with("HTTP/1.1 101 ") || !text.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=") {
        return Err(format!("/v1/ws: {text:?}"));
    }
    let end = answer.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    if answer[end..].len() < 4 || answer[end..end + 4] != [0x8a, 0x02, b'h', b'i'] {
        return Err(format!("no pong after the 101: {:?}", &answer[end..]));
    }
    Ok(())
}

/// What a child of this test does, by its role. Each prints one line per thing it saw.
mod child {
    use std::io::Write;
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::Duration;

    use eitri_remote::serve::sandbox::{is_dumpable, sandbox};

    fn dir(var: &str) -> PathBuf {
        PathBuf::from(std::env::var_os(var).unwrap_or_else(|| panic!("{var} is set by the parent")))
    }

    pub fn run(role: &str) -> i32 {
        match role {
            "sandbox-thread" => sandbox_thread(),
            "sandbox-busy" => sandbox_busy(),
            "publisher" => publisher(),
            "feed-sandboxed" => feed_sandboxed(),
            other => {
                eprintln!("unknown role {other:?}");
                2
            }
        }
    }

    fn outcome<T>(result: std::io::Result<T>) -> &'static str {
        match result {
            Ok(_) => "ok",
            Err(_) => "refused",
        }
    }

    fn sandbox_thread() -> i32 {
        let (agents, control, store, home) = (dir("RS_AGENTS"), dir("RS_CONTROL"), dir("RS_STORE"), dir("RS_HOME"));
        let state = match sandbox(&agents, &control, &store) {
            Ok(state) => state,
            Err(e) => {
                println!("state failed: {e}");
                return 1;
            }
        };
        println!("state ok");
        println!("sandbox {}", state.word());
        let thread = std::thread::spawn(move || {
            let exec = std::process::Command::new("/bin/true").status();
            println!("thread exec {}", outcome(exec));
            let home_write = std::fs::write(home.join("written-by-a-thread"), b"x");
            println!("thread home-write {}", outcome(home_write));
            let target = std::ffi::CString::new(home.join("truncate-me").into_os_string().into_vec()).unwrap();
            // SAFETY: a NUL-terminated path that outlives the call.
            let truncated = if unsafe { libc::truncate(target.as_ptr(), 0) } == 0 {
                "ok".to_owned()
            } else {
                match std::io::Error::last_os_error().raw_os_error() {
                    Some(libc::EPERM) => "EPERM".to_owned(),
                    Some(libc::EACCES) => "EACCES".to_owned(),
                    other => format!("errno {other:?}"),
                }
            };
            println!("thread home-truncate {truncated}");
            // As the agents reader opens a writer's lock: an existing file, for writing, nothing truncated or made.
            let agents_write = std::fs::OpenOptions::new()
                .write(true)
                .open(agents.join("probe"))
                .and_then(|mut file| file.write_all(b"x"));
            println!("thread agents-write {}", outcome(agents_write));
            println!(
                "thread store-read {}",
                outcome(std::fs::read(store.join("devices.json")))
            );
            println!(
                "thread store-write {}",
                outcome(std::fs::write(store.join("devices.json"), b"{}"))
            );
        });
        if thread.join().is_err() {
            return 1;
        }
        println!("dumpable {}", is_dumpable().unwrap_or(true));
        let _ = std::io::stdout().flush();
        0
    }

    fn sandbox_busy() -> i32 {
        let (started_tx, started) = mpsc::channel();
        let (_stop, stop_rx) = mpsc::channel::<()>();
        let _other = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let _ = stop_rx.recv();
        });
        started.recv().unwrap();
        match sandbox(&dir("RS_AGENTS"), &dir("RS_CONTROL"), &dir("RS_STORE")) {
            Err(e) if e.to_string().contains("before any thread starts") => {
                println!("refused, and dumpable {}", is_dumpable().unwrap_or(false));
                0
            }
            other => {
                println!("not refused: {other:?}");
                1
            }
        }
    }

    fn publisher() -> i32 {
        use eitri_core::agents::{new_instance, ProjectRef, Publisher, Row, WindowRef};
        let control = dir("RS_CONTROL");
        let instance = new_instance();
        let project = ProjectRef {
            key: "9b2c4e7a10f3d856".into(),
            name: "project".into(),
        };
        let window = WindowRef {
            instance: instance.clone(),
            name: "1".into(),
            focused: true,
        };
        let publisher = match Publisher::start(&control, &instance, project.clone(), window.clone()) {
            Ok(publisher) => publisher,
            Err(e) => {
                eprintln!("publisher: {e}");
                return 1;
            }
        };
        publisher.offer_now(vec![Row {
            id: format!("{instance}.1"),
            key: "k-0000000000000001".into(),
            agent: "claude".into(),
            project,
            window,
            number: 1,
            state: "idle".into(),
            ..Row::default()
        }]);
        let file = control.join("agents").join(format!("{instance}.json"));
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !std::fs::read_to_string(&file).is_ok_and(|text| text.contains("k-0000000000000001")) {
            if std::time::Instant::now() > deadline {
                eprintln!("publisher: nothing published");
                return 1;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // The window's own agents socket, which a dead writer leaves behind beside its files.
        let _socket = std::os::unix::net::UnixListener::bind(control.join(format!("ag-{instance}.sock")));
        println!("ready {instance}");
        let _ = std::io::stdout().flush();
        loop {
            std::thread::park();
        }
    }

    fn feed_sandboxed() -> i32 {
        let (agents, control, store) = (dir("RS_AGENTS"), dir("RS_CONTROL"), dir("RS_STORE"));
        if let Err(e) = sandbox(&agents, &control, &store) {
            println!("sandbox failed: {e}");
            return 1;
        }
        let (tx, rx) = mpsc::channel();
        let feed = match eitri_remote::agents_feed::AgentsFeed::start(&control, move |frame| {
            let _ = tx.send(frame);
        }) {
            Ok(feed) => feed,
            Err(e) => {
                println!("feed failed: {e}");
                return 1;
            }
        };
        let Ok(first) = rx.recv_timeout(Duration::from_secs(30)) else {
            println!("no first frame");
            return 1;
        };
        let view: serde_json::Value = serde_json::from_str(&first).unwrap_or_default();
        println!("kind {}", view["kind"]);
        println!("rows {}", view["view"]["rows"].as_array().map_or(usize::MAX, Vec::len));
        feed.stop();
        let _ = std::io::stdout().flush();
        0
    }
}
