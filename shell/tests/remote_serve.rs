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
        (
            "sandbox_lets_serve_write_the_audit_dir_but_not_the_store",
            sandbox_lets_serve_write_the_audit_dir_but_not_the_store,
        ),
        (
            "sandbox_lets_serve_reach_the_journal_socket",
            sandbox_lets_serve_reach_the_journal_socket,
        ),
        (
            "missing_journal_dir_still_sandboxes",
            missing_journal_dir_still_sandboxes,
        ),
        ("serve_records_enable_and_disable", serve_records_enable_and_disable),
        (
            "conn_open_and_close_name_the_device",
            conn_open_and_close_name_the_device,
        ),
        (
            "a_stop_closes_the_open_connections_in_the_log",
            a_stop_closes_the_open_connections_in_the_log,
        ),
        ("helper_holds_only_its_socket", helper_holds_only_its_socket),
        ("helper_exits_when_serve_goes", helper_exits_when_serve_goes),
        (
            "helper_cannot_read_the_store_or_exec",
            helper_cannot_read_the_store_or_exec,
        ),
        (
            "helper_resolves_and_loads_roots_under_its_sandbox",
            helper_resolves_and_loads_roots_under_its_sandbox,
        ),
        (
            "missing_etc_files_do_not_fail_the_helper_sandbox",
            missing_etc_files_do_not_fail_the_helper_sandbox,
        ),
        (
            "needs_you_reaches_the_endpoint_end_to_end",
            needs_you_reaches_the_endpoint_end_to_end,
        ),
        ("no_notify_forks_no_helper", no_notify_forks_no_helper),
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

    fn audit(&self) -> PathBuf {
        eitri_core::remote_audit::audit_dir(&self.store())
    }

    /// Where the journal copy goes in this home: a test never writes to the real journal. Nothing listens there
    /// unless a check binds it, and while its directory is missing `serve` sends no copy.
    fn journal(&self) -> PathBuf {
        self.root.join("journal").join("socket")
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
            .env(eitri_core::remote_audit::JOURNAL_SOCKET_VAR, self.journal())
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
            .env("RS_AUDIT", self.audit())
            .env("RS_JOURNAL_DIR", self.journal().parent().unwrap())
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
    start_serve_with(shell, home, &[])
}

/// [`start_serve`] with more variables in `serve`'s environment.
fn start_serve_with(shell: &str, home: &Home, env: &[(&str, &str)]) -> Result<(Running, Run), String> {
    start_serve_args(shell, home, &[], env)
}

/// [`start_serve_with`] with more arguments after `--listen socket`.
fn start_serve_args(shell: &str, home: &Home, args: &[&str], env: &[(&str, &str)]) -> Result<(Running, Run), String> {
    let mut words = vec!["serve", "--listen", "socket"];
    words.extend_from_slice(args);
    let serve = shell_remote(shell, home, &words)
        .envs(env.iter().copied())
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

fn sandbox_lets_serve_write_the_audit_dir_but_not_the_store(_shell: &str, dir: &Path) -> Result<(), String> {
    let home = Home::new(dir);
    home.make_dirs();
    std::fs::write(home.store().join("devices.json"), b"{\"v\":1,\"devices\":[]}").unwrap();
    let run = run(home.child("sandbox-audit"))?;
    let mut wanted = vec![
        "audit-append ok",
        "audit-rotate ok",
        "audit-prune ok",
        "audit-after-rotation ok",
        "audit-new ok",
    ];
    if landlock_in_kernel() {
        wanted.extend(["store-write refused", "store-new refused"]);
    }
    for line in wanted {
        if !run.stdout.lines().any(|l| l == line) {
            return Err(format!("no line {line:?}; {}", run.says()));
        }
    }
    if run.code != Some(0) {
        return Err(run.says());
    }
    let rotated = std::fs::read_to_string(home.audit().join("audit.jsonl.1")).map_err(|e| e.to_string())?;
    let log = std::fs::read_to_string(home.audit().join("audit.jsonl")).map_err(|e| e.to_string())?;
    if !rotated.ends_with("\"why\":\"probe\"}\n") || !log.contains("\"why\":\"after-rotation\"") {
        return Err(format!("the records are not in the log: {log:?}"));
    }
    if landlock_in_kernel()
        && std::fs::read(home.store().join("devices.json")).ok().as_deref()
            != Some(b"{\"v\":1,\"devices\":[]}".as_slice())
    {
        return Err("the store was written under the sandbox".into());
    }
    Ok(())
}

fn sandbox_lets_serve_reach_the_journal_socket(_shell: &str, dir: &Path) -> Result<(), String> {
    let home = Home::new(dir);
    home.make_dirs();
    std::fs::create_dir_all(home.audit()).unwrap();
    std::fs::create_dir_all(home.journal().parent().unwrap()).unwrap();
    let journal = std::os::unix::net::UnixDatagram::bind(home.journal()).map_err(|e| e.to_string())?;
    journal.set_read_timeout(Some(HANG)).unwrap();
    let run = run(home.child("sandbox-journal"))?;
    if run.code != Some(0) || !run.stdout.lines().any(|l| l == "journal-send ok") {
        return Err(run.says());
    }
    let mut buf = [0u8; 256];
    let got = journal
        .recv(&mut buf)
        .map_err(|e| format!("nothing reached the journal socket: {e}"))?;
    if &buf[..got] != b"MESSAGE=probe\n" {
        return Err(format!("the journal socket got {:?}", &buf[..got]));
    }
    Ok(())
}

fn missing_journal_dir_still_sandboxes(_shell: &str, dir: &Path) -> Result<(), String> {
    let home = Home::new(dir);
    home.make_dirs();
    std::fs::create_dir_all(home.audit()).unwrap();
    std::fs::create_dir_all(home.journal().parent().unwrap()).unwrap();
    let run = run(home.child("sandbox-no-journal"))?;
    if run.code != Some(0) || !run.stdout.lines().any(|l| l.starts_with("state ok ")) {
        return Err(run.says());
    }
    Ok(())
}

/// The log's records, oldest first, each as its JSON object.
fn audit_records(home: &Home) -> Vec<serde_json::Value> {
    std::fs::read_to_string(home.audit().join("audit.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Waits until the log holds a record `wanted` accepts.
fn wait_for_record(
    home: &Home,
    what: &str,
    wanted: impl Fn(&serde_json::Value) -> bool,
) -> Result<Vec<serde_json::Value>, String> {
    let until = Instant::now() + HANG;
    loop {
        let records = audit_records(home);
        if records.iter().any(&wanted) {
            return Ok(records);
        }
        if Instant::now() > until {
            return Err(format!("no record of {what}: {records:?}"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn serve_records_enable_and_disable(shell: &str, dir: &Path) -> Result<(), String> {
    let home = Home::new(dir);
    // No window: `serve` ends at its idle deadline, shortened for the test.
    let mut idle = shell_remote(shell, &home, &["serve", "--listen", "socket"]);
    idle.env(eitri_remote::serve::IDLE_TEST_VAR, "300");
    let ran = run(idle)?;
    if ran.code != Some(0) {
        return Err(format!("the idle serve: {}", ran.says()));
    }
    let records = audit_records(&home);
    let events: Vec<&str> = records.iter().filter_map(|r| r["event"].as_str()).collect();
    if events != ["enable", "disable"] {
        return Err(format!("after an idle exit: {records:?}"));
    }
    if records[0]["listener"] != "socket" || records[0]["writer"] != "serve" || records[1]["why"] != "idle" {
        return Err(format!("after an idle exit: {records:?}"));
    }
    // And through `eitri remote stop`.
    let (serve, _) = start_serve(shell, &home)?;
    wait_for_record(&home, "the second enable", |r| {
        r["event"] == "enable" && r["run"] != records[0]["run"]
    })?;
    let stopped = run(shell_remote(shell, &home, &["stop"]))?;
    if stopped.code != Some(0) {
        return Err(format!("stop: {}", stopped.says()));
    }
    let records = wait_for_record(&home, "the stop", |r| r["event"] == "disable" && r["why"] == "stopped")?;
    drop(serve);
    let disables = records.iter().filter(|r| r["event"] == "disable").count();
    if disables != 2 {
        return Err(format!("{disables} disables: {records:?}"));
    }
    Ok(())
}

fn conn_open_and_close_name_the_device(shell: &str, dir: &Path) -> Result<(), String> {
    let home = Home::new(dir);
    home.make_dirs();
    let token = "conn-token-sentinel-5d";
    let store = serde_json::json!({
        "v": 1,
        "devices": [{ "id": "d1", "name": "Pixel 8", "token_sha256": eitri_core::remote_devices::token_hash_hex(token), "grant": "watch" }],
    });
    agent::private_fs::write_private(&home.store().join("devices.json"), store.to_string().as_bytes()).unwrap();
    std::fs::create_dir_all(home.journal().parent().unwrap()).unwrap();
    let journal = std::os::unix::net::UnixDatagram::bind(home.journal()).map_err(|e| e.to_string())?;
    journal.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    let (serve, _) = start_serve(shell, &home)?;
    let mut phone = phones::Phone::connect(&home.control().join("remote.sock"));
    phone.hello("d1", Some(token));
    phone.send(serde_json::json!({ "kind": "ping" }));
    let pong = phone.recv(HANG)?.ok_or("no pong")?;
    if pong["kind"] != "pong" {
        return Err(format!("the first answer: {pong}"));
    }
    phone.close();
    let _ = phone.bytes_until_closed(HANG);
    let records = wait_for_record(&home, "the close", |r| r["event"] == "conn_close")?;
    let open = records
        .iter()
        .find(|r| r["event"] == "conn_open")
        .ok_or("no conn_open")?;
    let close = records.iter().find(|r| r["event"] == "conn_close").unwrap();
    let opened_right = open["device"] == "d1"
        && open["device_name"] == "Pixel 8"
        && open["listener"] == "socket"
        && open["peer"] == "unix"
        && open["host"] == "localhost"
        && open["origin"].is_null();
    let closed_right = close["device"] == "d1"
        && close["device_name"] == "Pixel 8"
        && close["why"] == "closed"
        && close["secs"].is_u64()
        && close["bytes_in"].as_u64().is_some_and(|n| n > 0)
        && close["bytes_out"].as_u64().is_some_and(|n| n > 0);
    if !opened_right || !closed_right {
        return Err(format!("the records: {open} {close}"));
    }
    let stopped = run(shell_remote(shell, &home, &["stop"]))?;
    drop(serve);
    if stopped.code != Some(0) {
        return Err(stopped.says());
    }
    // The journal copy: each record as its own datagram, and never the token.
    let mut copies = Vec::new();
    let mut buf = vec![0u8; 16 * 1024];
    while let Ok(got) = journal.recv(&mut buf) {
        copies.push(String::from_utf8_lossy(&buf[..got]).into_owned());
    }
    for event in ["enable", "conn_open", "conn_close", "disable"] {
        let wanted = format!("EITRI_EVENT={event}\n");
        if !copies
            .iter()
            .any(|c| c.contains(&wanted) && c.contains("SYSLOG_IDENTIFIER=eitri-remote\n"))
        {
            return Err(format!("no journal copy of {event}: {copies:?}"));
        }
    }
    let log = std::fs::read_to_string(home.audit().join("audit.jsonl")).unwrap_or_default();
    if log.contains(token) || copies.iter().any(|c| c.contains(token)) {
        return Err("the token reached the log or the journal".into());
    }
    Ok(())
}

fn a_stop_closes_the_open_connections_in_the_log(shell: &str, dir: &Path) -> Result<(), String> {
    let home = Home::new(dir);
    home.make_dirs();
    let token = "stop-token-sentinel-3c";
    let store = serde_json::json!({
        "v": 1,
        "devices": [{ "id": "d1", "name": "Pixel 8", "token_sha256": eitri_core::remote_devices::token_hash_hex(token), "grant": "watch" }],
    });
    agent::private_fs::write_private(&home.store().join("devices.json"), store.to_string().as_bytes()).unwrap();
    let (serve, _) = start_serve(shell, &home)?;
    let mut phone = phones::Phone::connect(&home.control().join("remote.sock"));
    phone.hello("d1", Some(token));
    phone.send(serde_json::json!({ "kind": "ping" }));
    let pong = phone.recv(HANG)?.ok_or("no pong")?;
    if pong["kind"] != "pong" {
        return Err(format!("the first answer: {pong}"));
    }
    wait_for_record(&home, "the open", |r| r["event"] == "conn_open")?;
    // Stopped while the phone is still connected.
    let stopped = run(shell_remote(shell, &home, &["stop"]))?;
    drop(serve);
    if stopped.code != Some(0) {
        return Err(stopped.says());
    }
    let records = audit_records(&home);
    let events: Vec<&str> = records.iter().filter_map(|r| r["event"].as_str()).collect();
    if events != ["enable", "conn_open", "conn_close", "disable"] {
        return Err(format!("the records: {records:?}"));
    }
    let close = &records[2];
    let closed_right = close["device"] == "d1"
        && close["device_name"] == "Pixel 8"
        && close["why"] == "serve_ended"
        && close["secs"].is_u64()
        && close["bytes_in"].as_u64().is_some_and(|n| n > 0)
        && close["bytes_out"].as_u64().is_some_and(|n| n > 0);
    if !closed_right || records[3]["why"] != "stopped" {
        return Err(format!("the records: {records:?}"));
    }
    Ok(())
}

/// What a child of this test does, by its role. Each prints one line per thing it saw.
// ---------------------------------------------------------------------------------------------------------------
// The notifier's helper, in children of this test, and the wakes of a `serve` end to end.

/// The lines of `run`'s stdout, or why the child failed.
fn lines_of(run: &Run) -> Vec<&str> {
    run.stdout.lines().collect()
}

fn wants(run: &Run, lines: &[&str]) -> Result<(), String> {
    for line in lines {
        if !run.stdout.lines().any(|l| l == *line) {
            return Err(format!("no line {line:?}; {}", run.says()));
        }
    }
    if run.code != Some(0) {
        return Err(run.says());
    }
    Ok(())
}

fn helper_holds_only_its_socket(_shell: &str, dir: &Path) -> Result<(), String> {
    let home = Home::new(dir);
    let run = run(home.child("helper-fds"))?;
    let fds = lines_of(&run)
        .into_iter()
        .find_map(|l| l.strip_prefix("helper fds "))
        .ok_or_else(|| run.says())?;
    let fds: Vec<i32> = fds.split(',').filter_map(|fd| fd.parse().ok()).collect();
    if fds.len() != 4 || fds[..3] != [0, 1, 2] || fds[3] < 3 {
        return Err(format!("the helper held {fds:?}; {}", run.says()));
    }
    let sandbox = if kernel_landlock_abi() >= 9 { "full" } else { "partial" };
    wants(&run, &[&format!("helper sandbox {sandbox}")])
}

fn helper_exits_when_serve_goes(_shell: &str, dir: &Path) -> Result<(), String> {
    let home = Home::new(dir);
    let run = run(home.child("helper-exit"))?;
    wants(&run, &["helper exited 0"])
}

fn helper_cannot_read_the_store_or_exec(_shell: &str, dir: &Path) -> Result<(), String> {
    let home = Home::new(dir);
    home.make_dirs();
    std::fs::write(home.store().join("devices.json"), b"{}").unwrap();
    let run = run(home.child("helper-sandbox"))?;
    let mut lines = vec!["state ok", "exec refused", "dumpable false"];
    if landlock_in_kernel() {
        lines.extend(["store-read refused", "home-write refused"]);
    }
    wants(&run, &lines)?;
    if landlock_in_kernel() && home.home().join("by-the-helper").exists() {
        return Err("the helper wrote under $HOME".to_owned());
    }
    Ok(())
}

fn helper_resolves_and_loads_roots_under_its_sandbox(_shell: &str, dir: &Path) -> Result<(), String> {
    let home = Home::new(dir);
    let run = run(home.child("helper-resolve"))?;
    let sandbox = if kernel_landlock_abi() >= 9 { "full" } else { "partial" };
    wants(
        &run,
        &[
            &format!("state ok {sandbox}"),
            "roots loaded",
            "own ok",
            "resolve ok",
            "post refused",
        ],
    )
}

fn missing_etc_files_do_not_fail_the_helper_sandbox(_shell: &str, dir: &Path) -> Result<(), String> {
    let home = Home::new(dir);
    let run = run(home.child("helper-missing"))?;
    let sandbox = if kernel_landlock_abi() >= 9 { "full" } else { "partial" };
    wants(&run, &[&format!("state ok {sandbox}"), "etc-read ok"])
}

/// The processes whose parent is `pid`, from `/proc`.
fn children_of(pid: u32) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let child: u32 = entry.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
            // `pid (comm) state ppid ...`, where the name may hold spaces and parentheses.
            let after = &stat[stat.rfind(')')? + 1..];
            let ppid: u32 = after.split_whitespace().nth(1)?.parse().ok()?;
            (ppid == pid).then_some(child)
        })
        .collect()
}

/// A loopback HTTP server that answers every request 200 and hands each request's bytes over.
fn push_server() -> (std::net::SocketAddr, mpsc::Receiver<Vec<u8>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let tx = tx.clone();
            std::thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(HANG));
                let mut seen = Vec::new();
                let mut buf = [0u8; 1024];
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    seen.extend_from_slice(&buf[..n]);
                    if seen.ends_with(eitri_remote::notify::post::WAKE_BODY) {
                        break;
                    }
                }
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
                let _ = tx.send(seen);
            });
        }
    });
    (addr, rx)
}

fn needs_you_reaches_the_endpoint_end_to_end(shell: &str, dir: &Path) -> Result<(), String> {
    let home = Home::new(dir);
    home.make_dirs();
    let (addr, posts) = push_server();
    let endpoint = format!("http://127.0.0.1:{}/up/e2e", addr.port());
    let store = serde_json::json!({
        "v": 1,
        "devices": [{
            "id": "d1", "name": "phone", "token_sha256": eitri_core::remote_devices::token_hash_hex("tok"),
            "grant": "watch",
        }],
    });
    let store_path = home.store().join("devices.json");
    agent::private_fs::write_private(&store_path, store.to_string().as_bytes())
        .map_err(|e| format!("the store: {e}"))?;
    eitri_core::remote_devices::update(&store_path, |store| {
        store.get_mut("d1").unwrap().push_endpoint = Some(endpoint.clone());
    })
    .map_err(|e| format!("the store's endpoint: {e}"))?;
    // A window's row, quiet, before `serve` reads the rows: the notifier's baseline.
    let mut publisher = home.child("publisher-needs");
    publisher.stdin(Stdio::piped()).stderr(Stdio::inherit());
    let mut publisher = Running(publisher.spawn().map_err(|e| format!("the publisher: {e}"))?);
    let mut said = BufReader::new(publisher.0.stdout.take().unwrap());
    let mut line = String::new();
    said.read_line(&mut line).map_err(|e| e.to_string())?;
    if !line.starts_with("ready ") {
        return Err(format!("the publisher said {line:?}"));
    }
    let allow = addr.to_string();
    let (serve, _) = start_serve_with(shell, &home, &[("EITRI_REMOTE_TEST_POST_ALLOW", allow.as_str())])?;
    let helpers = children_of(serve.0.id());
    if helpers.len() != 1 {
        return Err(format!("serve has {} children, not its one helper", helpers.len()));
    }
    // The row needs the person now; nobody is at the desk and no phone is connected.
    writeln!(publisher.0.stdin.as_mut().unwrap()).map_err(|e| e.to_string())?;
    let request = posts
        .recv_timeout(HANG)
        .map_err(|_| "no wake reached the endpoint".to_owned())?;
    let text = String::from_utf8_lossy(&request);
    if !text.starts_with("POST /up/e2e HTTP/1.1\r\n") || !request.ends_with(eitri_remote::notify::post::WAKE_BODY) {
        return Err(format!("the endpoint got {text:?}"));
    }
    wait_for_record(&home, "the push", |r| r["event"] == "push")?;
    if let Ok(more) = posts.recv_timeout(Duration::from_millis(1500)) {
        return Err(format!("a second wake: {}", String::from_utf8_lossy(&more)));
    }
    let records = audit_records(&home);
    let pushes: Vec<&serde_json::Value> = records.iter().filter(|r| r["event"] == "push").collect();
    if pushes.len() != 1
        || pushes[0]["device"] != "d1"
        || pushes[0]["wake"] != "needs_you"
        || pushes[0]["host"] != "127.0.0.1"
        || pushes[0]["status"] != "200"
        || pushes[0].to_string().contains("/up/e2e")
    {
        return Err(format!("the log: {records:?}"));
    }
    let _ = run(shell_remote(shell, &home, &["stop"]));
    drop(serve);
    Ok(())
}

fn no_notify_forks_no_helper(shell: &str, dir: &Path) -> Result<(), String> {
    let home = Home::new(dir);
    let (serve, _) = start_serve_args(shell, &home, &["--no-notify"], &[])?;
    let children = children_of(serve.0.id());
    let status = run(shell_remote(shell, &home, &["status"]))?;
    let _ = run(shell_remote(shell, &home, &["stop"]));
    drop(serve);
    if !children.is_empty() {
        return Err(format!("serve --no-notify has children {children:?}"));
    }
    if !status.stdout.lines().any(|l| l == "notify: off") {
        return Err(format!("status does not say wakes are off: {}", status.says()));
    }
    Ok(())
}

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
            "sandbox-audit" => sandbox_audit(),
            "sandbox-journal" => sandbox_journal(),
            "sandbox-no-journal" => sandbox_no_journal(),
            "helper-fds" => helper_fds(),
            "helper-exit" => helper_exit(),
            "helper-sandbox" => helper_sandbox(),
            "helper-resolve" => helper_resolve(),
            "helper-missing" => helper_missing(),
            "publisher-needs" => publisher_needs(),
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
        let state = match sandbox(&agents, &control, &store, &store.join("audit"), None) {
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
        match sandbox(
            &dir("RS_AGENTS"),
            &dir("RS_CONTROL"),
            &dir("RS_STORE"),
            &dir("RS_AUDIT"),
            None,
        ) {
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
        if let Err(e) = sandbox(&agents, &control, &store, &store.join("audit"), None) {
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

    /// Opens the log as `serve` does, before the sandbox, then appends from a thread under it and tries the store.
    fn sandbox_audit() -> i32 {
        use eitri_core::remote_audit::{AuditLog, Event, JournalTarget, WriterKind};
        let (agents, control, store, audit) = (dir("RS_AGENTS"), dir("RS_CONTROL"), dir("RS_STORE"), dir("RS_AUDIT"));
        // A current file past its size, so the next record rotates it, and a rotated file older than the kept days
        // that the rotation moves to the last place, where the prune that follows removes it.
        agent::private_fs::create_private_dir_all(&audit, audit.parent().unwrap()).unwrap();
        let filler = format!("{}\n", "x".repeat(1023)).repeat(10 * 1024 + 1);
        agent::private_fs::write_private(&audit.join("audit.jsonl"), filler.as_bytes()).unwrap();
        let old = concat!(
            r#"{"v":1,"time":"2001-01-01T00:00:00.000Z","writer":"serve","run":"0000000000000001","seq":1,"#,
            r#""event":"disable","prev":"0000000000000000000000000000000000000000000000000000000000000000","why":"old"}"#,
            "\n"
        );
        agent::private_fs::write_private(&audit.join("audit.jsonl.4"), old.as_bytes()).unwrap();
        agent::private_fs::write_private(&audit.join("audit.jsonl.5"), b"not a record\n").unwrap();
        let mut log = match AuditLog::open(&audit, WriterKind::Serve, Some(90), JournalTarget::Off) {
            Ok(log) => log,
            Err(e) => {
                println!("open failed: {e}");
                return 1;
            }
        };
        if let Err(e) = sandbox(&agents, &control, &store, &audit, None) {
            println!("sandbox failed: {e}");
            return 1;
        }
        let thread = std::thread::spawn(move || {
            let appended = log.record(Event::Disable { why: "probe".into() });
            println!("audit-append {}", outcome(appended));
            let rotated = std::fs::metadata(audit.join("audit.jsonl.1")).is_ok_and(|m| m.len() > 10 * 1024 * 1024)
                && !audit.join("audit.jsonl").exists();
            let pruned = !audit.join("audit.jsonl.5").exists() && !audit.join("audit.jsonl.4").exists();
            println!("audit-rotate {}", if rotated { "ok" } else { "refused" });
            println!("audit-prune {}", if pruned { "ok" } else { "refused" });
            let after = log.record(Event::Disable {
                why: "after-rotation".into(),
            });
            println!("audit-after-rotation {}", outcome(after));
            println!(
                "audit-new {}",
                outcome(std::fs::write(audit.join("made-under-the-sandbox"), b"x"))
            );
            println!(
                "store-write {}",
                outcome(std::fs::write(store.join("devices.json"), b"{}"))
            );
            println!(
                "store-new {}",
                outcome(std::fs::write(store.join("made-beside-the-store"), b"x"))
            );
        });
        if thread.join().is_err() {
            return 1;
        }
        let _ = std::io::stdout().flush();
        0
    }

    /// Sends a datagram to the journal socket the parent bound, under the sandbox.
    fn sandbox_journal() -> i32 {
        let journal = dir("RS_JOURNAL_DIR");
        if let Err(e) = sandbox(
            &dir("RS_AGENTS"),
            &dir("RS_CONTROL"),
            &dir("RS_STORE"),
            &dir("RS_AUDIT"),
            Some(&journal),
        ) {
            println!("sandbox failed: {e}");
            return 1;
        }
        let sent = std::os::unix::net::UnixDatagram::unbound()
            .and_then(|socket| socket.send_to(b"MESSAGE=probe\n", journal.join("socket")));
        println!("journal-send {}", outcome(sent));
        let _ = std::io::stdout().flush();
        0
    }

    /// The sandbox with a journal directory that is not there.
    fn sandbox_no_journal() -> i32 {
        let missing = dir("RS_JOURNAL_DIR").join("not-there");
        match sandbox(
            &dir("RS_AGENTS"),
            &dir("RS_CONTROL"),
            &dir("RS_STORE"),
            &dir("RS_AUDIT"),
            Some(&missing),
        ) {
            Ok(state) => println!("state ok {}", state.word()),
            Err(e) => println!("sandbox failed: {e}"),
        }
        let _ = std::io::stdout().flush();
        0
    }

    fn helper_fds() -> i32 {
        // Descriptors a `serve` would hold by then, none of which the helper may keep.
        let _file = std::fs::File::open("/proc/self/status").unwrap();
        let _listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let _socket = std::os::unix::net::UnixDatagram::unbound().unwrap();
        let helper = match eitri_remote::notify::helper::Helper::spawn() {
            Ok(helper) => helper,
            Err(e) => {
                println!("spawn failed: {e}");
                return 1;
            }
        };
        let fds: Vec<String> = helper.ready().fds.iter().map(|fd| fd.to_string()).collect();
        println!("helper fds {}", fds.join(","));
        println!("helper sandbox {}", helper.ready().sandbox);
        0
    }

    fn helper_exit() -> i32 {
        let helper = match eitri_remote::notify::helper::Helper::spawn() {
            Ok(helper) => helper,
            Err(e) => {
                println!("spawn failed: {e}");
                return 1;
            }
        };
        let pid = helper.pid();
        // As when `serve` goes: its end of the pair closes.
        drop(helper);
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let mut status = 0;
            // SAFETY: `status` is a local the call writes; `pid` is this process's own child.
            let reaped = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if reaped == pid {
                println!("helper exited {}", libc::WEXITSTATUS(status));
                return 0;
            }
            if std::time::Instant::now() > deadline {
                println!("helper still running");
                // SAFETY: `kill` takes no pointers; the pid is this process's own child.
                unsafe { libc::kill(pid, libc::SIGKILL) };
                return 1;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn helper_sandbox() -> i32 {
        let (store, home) = (dir("RS_STORE"), dir("RS_HOME"));
        eitri_remote::notify::post::load_roots();
        let state = match eitri_remote::serve::sandbox::sandbox_helper() {
            Ok(state) => state,
            Err(e) => {
                println!("state failed: {e}");
                return 1;
            }
        };
        println!("state ok");
        println!("sandbox {}", state.word());
        println!("store-read {}", outcome(std::fs::read(store.join("devices.json"))));
        println!(
            "home-write {}",
            outcome(std::fs::write(home.join("by-the-helper"), b"x"))
        );
        println!("exec {}", outcome(std::process::Command::new("/bin/true").status()));
        println!("dumpable {}", is_dumpable().unwrap_or(true));
        0
    }

    fn helper_resolve() -> i32 {
        use eitri_remote::notify::post::{load_roots, own_addresses, post_wake, system_resolve, PostError};
        let roots = load_roots();
        let state = match eitri_remote::serve::sandbox::sandbox_helper() {
            Ok(state) => state,
            Err(e) => {
                println!("state failed: {e}");
                return 1;
            }
        };
        println!("state ok {}", state.word());
        println!("roots {}", if roots > 0 { "loaded" } else { "none" });
        match own_addresses() {
            Ok(_) => println!("own ok"),
            Err(e) => println!("own failed: {e}"),
        }
        match system_resolve("localhost", 9, Duration::from_secs(10)) {
            Ok(addrs) if !addrs.is_empty() => println!("resolve ok"),
            Ok(_) => println!("resolve empty"),
            Err(e) => println!("resolve failed: {e}"),
        }
        match post_wake("http://localhost:9/up", &system_resolve, &own_addresses) {
            Err(PostError::Refused(_)) => println!("post refused"),
            other => println!("post {other:?}"),
        }
        0
    }

    fn helper_missing() -> i32 {
        let home = dir("RS_HOME");
        let missing = home.join("no-such-dir");
        let read = [
            std::path::Path::new("/etc"),
            missing.as_path(),
            std::path::Path::new("/nonexistent-eitri/lib"),
        ];
        let connect = [std::path::Path::new("/nonexistent-eitri/run")];
        match eitri_remote::serve::sandbox::sandbox_helper_beneath(&read, &connect) {
            Ok(state) => println!("state ok {}", state.word()),
            Err(e) => {
                println!("state failed: {e}");
                return 1;
            }
        }
        println!("etc-read {}", outcome(std::fs::read_dir("/etc").map(|d| d.count())));
        0
    }

    fn publisher_needs() -> i32 {
        use eitri_core::agents::{new_instance, Class, ProjectRef, Publisher, Row, WindowRef};
        use std::io::BufRead;
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
        let row = |state: &str, class: Class| Row {
            id: format!("{instance}.1"),
            key: "k-00000000000000e1".into(),
            agent: "claude".into(),
            project: project.clone(),
            window: window.clone(),
            number: 1,
            state: state.into(),
            class,
            ..Row::default()
        };
        publisher.offer_now(vec![row("idle", Class::Quiet)]);
        println!("ready {instance}");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        publisher.offer_now(vec![row("card", Class::Needs)]);
        println!("needs");
        let _ = std::io::stdout().flush();
        loop {
            std::thread::park();
        }
    }
}
