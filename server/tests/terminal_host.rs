//! Terminal hosts: one detached `eitri-server term-host` per terminal, started, adopted, listed and stopped by the
//! daemon. The daemon's `Terminal` service is not served yet, so these tests drive the daemon side through its Rust
//! API (`TerminalHosts`, in this process or in a fixture daemon process) and talk to a host's own socket with a tonic
//! client.
#![allow(dead_code)]

#[path = "../src/audit.rs"]
mod audit;
#[path = "../src/daemon.rs"]
mod daemon;
#[path = "../src/peer.rs"]
mod peer;
#[path = "../src/run_dir.rs"]
mod run_dir;

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent::private_fs;
use daemon::terminals::{self, HostState, TerminalHosts};
use eitri_server_proto::terminal_client::TerminalClient;
use eitri_server_proto::terminal_client_frame::Frame as ClientFrame;
use eitri_server_proto::terminal_server_frame::Frame as ServerFrame;
use eitri_server_proto::*;
use tonic::transport::Channel;
use tonic::Code;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REVISION: &str = env!("EITRI_BUILD_COMMIT");
const HOST: &str = env!("CARGO_BIN_EXE_eitri-server");
const WAIT: Duration = Duration::from_secs(60);

fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}

/// The fixture's account: this user's passwd names, a home of the fixture's own, and `/bin/sh` as the login shell, so
/// no real profile of the person running the tests is read.
fn identity(home: &Path) -> BTreeMap<OsString, OsString> {
    let mut identity = daemon::login_env::passwd_identity().unwrap();
    identity.insert("HOME".into(), home.into());
    identity.insert("SHELL".into(), "/bin/sh".into());
    identity
}

fn config(home: &Path) -> terminals::Config {
    terminals::Config {
        host_program: HOST.into(),
        identity: Some(identity(home)),
        ..Default::default()
    }
}

/// The login environment the fixture's daemons captured: only `LANG` matters to a terminal.
fn captured() -> daemon::login_env::Captured {
    daemon::login_env::Captured {
        env: BTreeMap::from([(OsString::from("LANG"), OsString::from("POSIX"))]),
        fallback: None,
    }
}

#[derive(Default)]
struct DaemonSpec {
    opens: Vec<(String, PathBuf)>,
    env: Vec<(&'static str, &'static str)>,
    umask: Option<libc::mode_t>,
    idle_seconds: Option<u64>,
}

struct Fixture {
    root: PathBuf,
    paths: run_dir::Paths,
    daemon: Option<Child>,
    daemons: u32,
    /// Processes a test left running on purpose, ended by their pidfds.
    strays: Vec<OwnedFd>,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        // Short, so the longest socket of the run directory fits the socket name limit.
        let root = PathBuf::from(format!(
            "/tmp/th-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        private_fs::create_private_dir_all(&root, &root).unwrap();
        std::fs::create_dir(root.join("work")).unwrap();
        let paths = run_dir::Paths::new(VERSION, &root.join("d"), &root.join("s"), &std::env::temp_dir()).unwrap();
        paths.prepare().unwrap();
        Self {
            root: root.canonicalize().unwrap(),
            paths,
            daemon: None,
            daemons: 0,
            strays: Vec::new(),
        }
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    /// The daemon side in this process, as a daemon starting on this run directory builds it.
    fn manager(&self) -> Arc<TerminalHosts> {
        self.manager_with(config(&self.root))
    }

    fn manager_with(&self, config: terminals::Config) -> Arc<TerminalHosts> {
        let sidecars = Arc::new(daemon::sidecars::Sidecars::new(self.paths.clone()).unwrap());
        sidecars.set_environment(Ok(captured()));
        Arc::new(TerminalHosts::new(self.paths.clone(), config, sidecars))
    }

    fn request(&self, command_id: &str, cwd: &Path) -> OpenTerminalRequest {
        OpenTerminalRequest {
            command_id: command_id.into(),
            cwd: cwd.to_str().unwrap().into(),
            size: Some(TermSize {
                columns: 80,
                rows: 24,
                cell_width: 8,
                cell_height: 16,
            }),
            colors: None,
        }
    }

    /// A daemon process of this test binary (`fixture_daemon_entry`), which opens `spec.opens` through its
    /// `TerminalHosts` as soon as it owns its run directory and writes what each open answered to `opened`.
    async fn spawn_daemon(&mut self, spec: DaemonSpec) {
        assert!(self.daemon.is_none());
        let _ = std::fs::remove_file(self.root.join("opened"));
        self.daemons += 1;
        let log = std::fs::File::create(self.root.join(format!("daemon-{}.err", self.daemons))).unwrap();
        let opens: Vec<String> = spec
            .opens
            .iter()
            .map(|(id, dir)| format!("{id}={}", dir.display()))
            .collect();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "fixture_daemon_entry", "--nocapture"])
            .env_clear()
            .env("EITRI_TEST_DAEMON", "1")
            .env("EITRI_TEST_OPENS", opens.join("\n"))
            .env("EITRI_TEST_IDLE", spec.idle_seconds.unwrap_or(600).to_string())
            .env("HOME", &self.root)
            .env("XDG_DATA_HOME", self.root.join("d"))
            .env("XDG_STATE_HOME", self.root.join("s"))
            .env("PATH", "/usr/bin:/bin")
            .envs(spec.env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log);
        if let Some(mask) = spec.umask {
            // SAFETY: umask is async-signal-safe and touches no Rust state.
            unsafe {
                command.pre_exec(move || {
                    libc::umask(mask);
                    Ok(())
                });
            }
        }
        self.daemon = Some(command.spawn().unwrap());
        let pid = self.daemon.as_ref().unwrap().id();
        let deadline = Instant::now() + WAIT;
        // A killed daemon leaves its `info` behind; this daemon's own, and its socket, say it has adopted and binds.
        while run_dir::Paths::published(&self.paths).map_or(true, |published| published.info.pid != pid)
            || !self.paths.socket().unwrap().exists()
        {
            assert!(
                self.daemon.as_mut().unwrap().try_wait().unwrap().is_none(),
                "daemon exited during startup"
            );
            assert!(Instant::now() < deadline, "daemon did not start");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// What each open of the last daemon answered: a terminal id, or the refusal.
    async fn opened(&self) -> Vec<Result<String, String>> {
        let path = self.root.join("opened");
        let deadline = Instant::now() + WAIT;
        while !path.exists() {
            assert!(Instant::now() < deadline, "the daemon's opens did not finish");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| match line.split_once(' ') {
                Some(("ok", id)) => Ok(id.to_owned()),
                Some((_, error)) => Err(error.to_owned()),
                None => panic!("bad opened line {line}"),
            })
            .collect()
    }

    fn daemon_pid(&self) -> i32 {
        self.daemon.as_ref().unwrap().id() as i32
    }

    /// SIGKILL to the daemon: it stops nothing and cleans nothing up.
    fn kill_daemon(&mut self) {
        let mut child = self.daemon.take().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        std::fs::remove_file(self.paths.socket().unwrap()).unwrap();
    }

    /// SIGTERM to the daemon, as `eitri host stop` sends it; answers how long it took to exit.
    async fn stop_daemon(&mut self) -> Duration {
        let mut child = self.daemon.take().unwrap();
        let started = Instant::now();
        // SAFETY: the child is ours and unreaped, so its pid is still its own.
        assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "daemon exit {status:?}");
                return started.elapsed();
            }
            assert!(Instant::now() < deadline, "daemon did not stop");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn record(&self, id: &str) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(self.paths.metadata_run().join(format!("th-{id}.pid"))).unwrap()).unwrap()
    }

    fn host_pid(&self, id: &str) -> u32 {
        self.record(id)["pid"].as_u64().unwrap() as u32
    }

    fn socket(&self, id: &str) -> PathBuf {
        agent::socket_path::in_dir(&self.paths.run, &format!("th-{id}.sock")).unwrap()
    }

    async fn attach(&self, id: &str, client_id: &str) -> Attached {
        Attached::new(&self.socket(id), id, client_id).await
    }

    fn audit(&self) -> String {
        std::fs::read_to_string(self.paths.state.join("audit.log")).unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Hosts by their recorded identity, never a reused pid; their shells hang up with them.
        if let Ok(files) = std::fs::read_dir(self.paths.metadata_run()) {
            for file in files.flatten() {
                let name = file.file_name();
                if !name.to_string_lossy().ends_with(".pid") {
                    continue;
                }
                let Ok(record) =
                    serde_json::from_slice::<serde_json::Value>(&std::fs::read(file.path()).unwrap_or_default())
                else {
                    continue;
                };
                if let (Some(pid), Some(start)) = (record["pid"].as_u64(), record["start_time"].as_u64()) {
                    if let Some(fd) = pin(pid as u32) {
                        if daemon::sidecars::start_time(pid as u32).ok() == Some(start) {
                            let _ = daemon::sidecars::signal(&fd, libc::SIGKILL);
                        }
                    }
                }
            }
        }
        for stray in &self.strays {
            let _ = daemon::sidecars::signal(stray, libc::SIGKILL);
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn pin(pid: u32) -> Option<OwnedFd> {
    daemon::sidecars::pin(pid).ok()
}

fn exited(fd: &OwnedFd) -> bool {
    daemon::sidecars::exited(fd).unwrap()
}

async fn wait_exited(fd: &OwnedFd) {
    let deadline = Instant::now() + WAIT;
    while !exited(fd) {
        assert!(Instant::now() < deadline, "process did not exit");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn channel(socket: &Path) -> Channel {
    let path = socket.to_owned();
    let socket = socket.to_owned();
    tonic::transport::Endpoint::from_static("http://localhost")
        .connect_with_connector(tower::service_fn(move |_| {
            let socket = socket.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(socket).await?;
                peer::check(&stream)?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
        .unwrap_or_else(|error| {
            let logs: String = ["out", "err"]
                .iter()
                .map(|log| {
                    let name = path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .replace(".sock", &format!(".{log}"));
                    let log = path.parent().unwrap().join(name);
                    String::from_utf8_lossy(&std::fs::read(&log).unwrap_or_default()).into_owned()
                })
                .collect();
            panic!("connect {}: {error:?}; host logs: {logs}", path.display())
        })
}

async fn info(socket: &Path) -> TerminalInfo {
    let mut listed = TerminalClient::new(channel(socket).await)
        .list_terminals(ListTerminalsRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.terminals.len(), 1);
    listed.terminals.remove(0)
}

/// One attachment to a host's socket: frames in, text out (the snapshot's replay and live output, as bytes).
struct Attached {
    tx: Option<tokio::sync::mpsc::Sender<TerminalClientFrame>>,
    rx: tonic::Streaming<TerminalServerFrame>,
    text: Vec<u8>,
    sent: u64,
    written: u64,
    ended: bool,
    /// The status the stream ended with, when it ended with an error.
    status: Option<tonic::Status>,
    /// The first snapshot's grid.
    size: (u32, u32),
}

const START_SIZE: TermSize = TermSize {
    columns: 80,
    rows: 24,
    cell_width: 8,
    cell_height: 16,
};

impl Attached {
    async fn new(socket: &Path, id: &str, client_id: &str) -> Self {
        Self::sized(socket, id, client_id, START_SIZE).await
    }

    async fn sized(socket: &Path, id: &str, client_id: &str, size: TermSize) -> Self {
        let (tx, frames) = tokio::sync::mpsc::channel(16);
        tx.send(TerminalClientFrame {
            frame: Some(ClientFrame::Hello(AttachHello {
                terminal_id: id.into(),
                client_id: client_id.into(),
                input_sent: 0,
                size: Some(size),
                colors: None,
            })),
        })
        .await
        .unwrap();
        let rx = TerminalClient::new(channel(socket).await)
            .attach(tokio_stream::wrappers::ReceiverStream::new(frames))
            .await
            .unwrap()
            .into_inner();
        let mut attached = Self {
            tx: Some(tx),
            rx,
            text: Vec::new(),
            sent: 0,
            written: 0,
            ended: false,
            status: None,
            size: (0, 0),
        };
        let ServerFrame::Snapshot(part) = attached
            .until(|frame| matches!(frame, ServerFrame::Snapshot(part) if part.last))
            .await
        else {
            unreachable!()
        };
        attached.size = (part.columns, part.rows);
        attached
    }

    /// Ends the client's side of the stream; the response side stays open, read or not.
    fn end_sending(&mut self) {
        self.tx = None;
    }

    /// The next frame, or `None` once the stream has ended (an error status ends it too).
    async fn next(&mut self) -> Option<ServerFrame> {
        if self.ended {
            return None;
        }
        let frame = tokio::time::timeout(WAIT, self.rx.message())
            .await
            .expect("no frame from the host");
        match frame {
            Ok(Some(TerminalServerFrame { frame: Some(frame) })) => {
                match &frame {
                    ServerFrame::Snapshot(part) => {
                        self.text.extend_from_slice(&part.replay);
                        if part.last {
                            self.sent = part.input_written;
                            self.written = part.input_written;
                        }
                    }
                    ServerFrame::Output(output) => self.text.extend_from_slice(&output.data),
                    ServerFrame::InputWritten(written) => self.written = written.upto,
                    _ => {}
                }
                Some(frame)
            }
            Ok(Some(TerminalServerFrame { frame: None })) => panic!("an empty server frame"),
            Ok(None) => {
                self.ended = true;
                None
            }
            Err(status) => {
                self.ended = true;
                self.status = Some(status);
                None
            }
        }
    }

    async fn until(&mut self, mut wanted: impl FnMut(&ServerFrame) -> bool) -> ServerFrame {
        loop {
            match self.next().await {
                Some(frame) if wanted(&frame) => return frame,
                Some(_) => {}
                None => panic!(
                    "the stream ended first; text so far: {}",
                    String::from_utf8_lossy(&self.text)
                ),
            }
        }
    }

    async fn until_text(&mut self, needle: &str) {
        while !contains(&self.text, needle) {
            if self.next().await.is_none() {
                panic!(
                    "the stream ended before {needle:?}; text: {}",
                    String::from_utf8_lossy(&self.text)
                );
            }
        }
    }

    async fn send(&mut self, frame: ClientFrame) {
        self.try_send(frame).await.expect("the stream's sending side is open");
    }

    /// Sends unless the host has ended the stream.
    async fn try_send(&mut self, frame: ClientFrame) -> Option<()> {
        self.tx
            .as_ref()?
            .send(TerminalClientFrame { frame: Some(frame) })
            .await
            .ok()
    }

    /// One input frame at this client's next offset, unless it cannot be handed over within `within`.
    async fn input_within(&mut self, data: Vec<u8>, within: Duration) -> bool {
        let offset = self.sent;
        self.sent += data.len() as u64;
        let Some(tx) = self.tx.as_ref() else {
            return false;
        };
        let frame = TerminalClientFrame {
            frame: Some(ClientFrame::Input(InputBytes { offset, data })),
        };
        matches!(tokio::time::timeout(within, tx.send(frame)).await, Ok(Ok(())))
    }

    /// One input frame at this client's next offset.
    async fn input(&mut self, data: Vec<u8>) -> Option<()> {
        let offset = self.sent;
        self.sent += data.len() as u64;
        self.try_send(ClientFrame::Input(InputBytes { offset, data })).await
    }

    async fn type_line(&mut self, line: &str) {
        let data = format!("{line}\r").into_bytes();
        let offset = self.sent;
        self.sent += data.len() as u64;
        self.send(ClientFrame::Input(InputBytes { offset, data })).await;
    }

    /// Waits until everything typed so far has reached the PTY.
    async fn written(&mut self) {
        while self.written < self.sent {
            if self.next().await.is_none() {
                panic!("the stream ended before the input was written");
            }
        }
    }
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle.as_bytes())
}

/// The digits after the first `marker` followed by a digit.
fn number_after(text: &[u8], marker: &str) -> Option<u32> {
    let text = String::from_utf8_lossy(text);
    text.match_indices(marker).find_map(|(at, _)| {
        let digits: String = text[at + marker.len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok()
    })
}

fn environ(pid: u32) -> BTreeMap<OsString, OsString> {
    std::fs::read(format!("/proc/{pid}/environ"))
        .unwrap()
        .split(|b| *b == 0)
        .filter(|v| !v.is_empty())
        .map(|v| {
            let p = v.iter().position(|b| *b == b'=').unwrap();
            (
                OsString::from_vec(v[..p].to_vec()),
                OsString::from_vec(v[p + 1..].to_vec()),
            )
        })
        .collect()
}

fn umask_of(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("Umask:").map(|mask| mask.trim().to_owned()))
        .unwrap()
}

#[test]
fn fixture_daemon_entry() {
    if std::env::var_os("EITRI_TEST_DAEMON").is_none() {
        return;
    }
    run_dir::save_inherited_umask();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let paths = run_dir::Paths::from_env(VERSION).unwrap();
        let idle = std::env::var("EITRI_TEST_IDLE").unwrap().parse().unwrap();
        let opens: Vec<(String, String)> = std::env::var("EITRI_TEST_OPENS")
            .unwrap()
            .lines()
            .map(|line| {
                let (id, dir) = line.split_once('=').unwrap();
                (id.to_owned(), dir.to_owned())
            })
            .collect();
        let opened = home.join("opened");
        let ready: daemon::TerminalsReady = Box::new(move |hosts: Arc<TerminalHosts>| {
            tokio::spawn(async move {
                let starts = opens.into_iter().map(|(command_id, cwd)| {
                    let hosts = hosts.clone();
                    async move {
                        hosts
                            .start(OpenTerminalRequest {
                                command_id,
                                cwd,
                                size: Some(TermSize {
                                    columns: 80,
                                    rows: 24,
                                    cell_width: 8,
                                    cell_height: 16,
                                }),
                                colors: None,
                            })
                            .await
                    }
                });
                let mut lines = String::new();
                for result in futures_util::future::join_all(starts).await {
                    match result {
                        Ok(id) => lines.push_str(&format!("ok {id}\n")),
                        Err(status) => lines.push_str(&format!("err {:?} {}\n", status.code(), status.message())),
                    }
                }
                private_fs::write_private(&opened.with_extension("tmp"), lines.as_bytes()).unwrap();
                std::fs::rename(opened.with_extension("tmp"), &opened).unwrap();
            });
        });
        daemon::run_with_terminals(
            paths,
            Duration::from_secs(idle),
            daemon::OwnedSidecars::default(),
            async { Ok(captured()) },
            config(&home),
            Some(ready),
        )
        .await
        .unwrap();
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_runs_a_login_shell_in_its_dir() {
    let f = Fixture::new();
    let hosts = f.manager();
    let id = hosts.start(f.request("open-1", &f.work())).await.unwrap();
    assert_eq!(id.len(), 32);
    let mut client = f.attach(&id, "laptop").await;
    client.type_line(r#"echo "$0:$PWD""#).await;
    client.until_text(&format!("-sh:{}", f.work().display())).await;
    let listed = info(&f.socket(&id)).await;
    assert_eq!(listed.start_dir, f.work().to_str().unwrap());
    assert_eq!(listed.open_command_id, "open-1");
    assert!(listed.alive && listed.attached);
    hosts.stop_all().await.unwrap();
}

/// The fixture daemon runs with variables of its own and of an SSH channel and a tmux; the shell's environment at
/// exec is exactly the passwd identity, `PATH`, the captured `LANG` and the local terminal's three variables, and the
/// host itself carries no environment at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_shell_environment_is_exact() {
    let mut f = Fixture::new();
    f.spawn_daemon(DaemonSpec {
        opens: vec![("open-env".into(), f.work())],
        env: vec![
            ("EITRI_TEST_LEAK", "1"),
            ("SSH_CONNECTION", "192.0.2.1 50000 192.0.2.2 22"),
            ("TMUX", "/tmp/tmux-1000/default,1,0"),
        ],
        ..Default::default()
    })
    .await;
    let id = f.opened().await.remove(0).unwrap();
    let shell = info(&f.socket(&id)).await.pid;
    let mut expected: BTreeMap<OsString, OsString> = identity(&f.root);
    for (key, value) in [
        ("PATH", "/usr/local/bin:/usr/bin:/bin"),
        ("LANG", "POSIX"),
        ("TERM", "xterm-256color"),
        ("COLORTERM", "truecolor"),
        ("TERM_PROGRAM", "eitri"),
    ] {
        expected.insert(key.into(), value.into());
    }
    assert_eq!(environ(shell), expected);
    assert!(environ(f.host_pid(&id)).is_empty());
    // The start directory and the environment reach the host through a pipe, not its world-readable command line.
    let cmdline = std::fs::read(format!("/proc/{}/cmdline", f.host_pid(&id))).unwrap();
    for secret in [f.work().to_str().unwrap(), "/bin/sh", "POSIX"] {
        assert!(!contains(&cmdline, secret), "{secret} on the host's command line");
    }
    let mut client = f.attach(&id, "laptop").await;
    // Each variable on a line of its own, marked, so neither the echoed command nor the screen's replay is read as one.
    client.type_line("env | sed 's/^/ENV:/'; echo env-$((40+2))-done").await;
    client.until_text("env-42-done").await;
    let text = String::from_utf8_lossy(&client.text).into_owned();
    let names: Vec<&str> = text
        .split(['\r', '\n'])
        .filter_map(|line| line.strip_prefix("ENV:"))
        .filter_map(|line| line.split_once('=').map(|(name, _)| name))
        .collect();
    for leaked in ["EITRI_TEST_LEAK", "SSH_CONNECTION", "TMUX"] {
        assert!(!names.contains(&leaked), "{leaked} reached the shell: {names:?}");
    }
    assert!(
        text.contains("ENV:TERM=xterm-256color") && text.contains("ENV:COLORTERM=truecolor"),
        "{names:?}"
    );
    f.stop_daemon().await;
}

/// The daemon is started with umask 027 as a login would leave it; the host and its shell keep that mask.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn umask_is_the_logins() {
    let mut f = Fixture::new();
    f.spawn_daemon(DaemonSpec {
        opens: vec![("open-umask".into(), f.work())],
        umask: Some(0o027),
        ..Default::default()
    })
    .await;
    let id = f.opened().await.remove(0).unwrap();
    let shell = info(&f.socket(&id)).await.pid;
    assert_eq!(umask_of(f.host_pid(&id)), "0027");
    assert_eq!(umask_of(shell), "0027");
    let mut client = f.attach(&id, "laptop").await;
    client.type_line("echo mask-$(umask)").await;
    client.until_text("mask-0027").await;
    f.stop_daemon().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_dir_rules_refuse_relative_dotdot_and_missing() {
    let f = Fixture::new();
    let hosts = f.manager();
    std::fs::write(f.root.join("file"), b"").unwrap();
    let work = f.work().display().to_string();
    for (cwd, why) in [
        ("relative/dir".to_owned(), "absolute"),
        (format!("{work}/.."), "normal form"),
        (format!("{work}/."), "normal form"),
        (format!("{work}/"), "normal form"),
        (format!("{work}//sub"), "normal form"),
        (f.root.join("missing").display().to_string(), "existing directory"),
        (f.root.join("file").display().to_string(), "existing directory"),
        (format!("/{}", "x".repeat(4096)), "byte cap"),
    ] {
        let mut request = f.request("bad-dir", Path::new("/"));
        request.cwd = cwd.clone();
        let error = hosts.start(request).await.unwrap_err();
        assert_eq!(error.code(), Code::InvalidArgument, "{cwd}: {error}");
        assert!(error.message().contains(why), "{cwd}: {error}");
    }
    assert!(hosts.list().is_empty());
    // A directory the daemon's user does not own is a place a shell may start, unlike a sidecar's root.
    if private_fs::euid() != 0 {
        let id = hosts.start(f.request("root-dir", Path::new("/"))).await.unwrap();
        assert_eq!(info(&f.socket(&id)).await.start_dir, "/");
    }
    hosts.stop_all().await.unwrap();
}

/// The daemon is SIGKILLed while its terminal prints; the host keeps the shell and its output, and the next daemon
/// adopts it by pid, start time and socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_killed_daemon_leaves_hosts_running_and_the_next_daemon_adopts_them() {
    let mut f = Fixture::new();
    f.spawn_daemon(DaemonSpec {
        opens: vec![("open-adopt".into(), f.work())],
        ..Default::default()
    })
    .await;
    let id = f.opened().await.remove(0).unwrap();
    let host = pin(f.host_pid(&id)).unwrap();
    let mut client = f.attach(&id, "laptop").await;
    client
        .type_line("i=0; while [ $i -lt 20 ]; do i=$((i+1)); echo tick-$i; sleep 0.05; done; echo ticks-$((7*6))")
        .await;
    client.until_text("tick-2").await;
    drop(client);
    f.kill_daemon();
    assert!(!exited(&host));
    f.spawn_daemon(DaemonSpec::default()).await;
    assert!(f.audit().contains(&format!("terminal-adopt id={id}")), "{}", f.audit());
    let mut client = f.attach(&id, "laptop-again").await;
    client.until_text("ticks-42").await;
    assert!(contains(&client.text, "tick-20"));
    drop(client);
    f.stop_daemon().await;
    wait_exited(&host).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_killed_host_is_gone_and_listed_so() {
    let f = Fixture::new();
    let hosts = f.manager();
    let id = hosts.start(f.request("open-gone", &f.work())).await.unwrap();
    assert!(matches!(hosts.list()[0].state, HostState::Live { handshake: true }));
    let host = pin(f.host_pid(&id)).unwrap();
    daemon::sidecars::signal(&host, libc::SIGKILL).unwrap();
    wait_exited(&host).await;
    let listed = hosts.list();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].state, HostState::Gone);
    assert!(f.audit().contains(&format!("terminal-gone id={id}")));
    // A later daemon reads the same from the records alone.
    let next = f.manager();
    assert_eq!(next.list()[0].state, HostState::Gone);
    assert_eq!(f.audit().matches(&format!("terminal-gone id={id}")).count(), 1);
    assert!(!next.any_live());
}

/// A job that ignores SIGHUP outlives its killed host: the terminal is listed as gone, with words that say what may
/// still run, and the job is still there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_ignoring_sighup_is_reported_honestly() {
    let mut f = Fixture::new();
    let hosts = f.manager();
    let id = hosts.start(f.request("open-hup", &f.work())).await.unwrap();
    let shell = pin(info(&f.socket(&id)).await.pid).unwrap();
    let mut client = f.attach(&id, "laptop").await;
    client.type_line("trap '' HUP; sleep 60 & echo job=$!").await;
    let mut job = None;
    while job.is_none() {
        client.next().await.expect("stream ended");
        job = number_after(&client.text, "job=");
    }
    let job = pin(job.unwrap()).unwrap();
    let job_fd = job.as_raw_fd();
    f.strays.push(job);
    let host = pin(f.host_pid(&id)).unwrap();
    daemon::sidecars::signal(&host, libc::SIGKILL).unwrap();
    wait_exited(&host).await;
    wait_exited(&shell).await;
    let listed = hosts.list();
    assert_eq!(listed[0].state, HostState::Gone);
    assert!(terminals::GONE_NOTE.contains("may still run"));
    let job = f.strays.iter().find(|fd| fd.as_raw_fd() == job_fd).unwrap();
    assert!(!exited(job), "the job ignoring SIGHUP should still run");
    daemon::sidecars::signal(job, libc::SIGKILL).unwrap();
    wait_exited(job).await;
}

/// The shell exits while the daemon runs with a two-second idle time; with no live terminal the daemon idles out,
/// and the daemon side reading the records afterwards (in this process) still reports the exit status.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exit_record_outlives_the_daemons_idle_exit() {
    let mut f = Fixture::new();
    f.spawn_daemon(DaemonSpec {
        opens: vec![("open-exit".into(), f.work())],
        idle_seconds: Some(2),
        ..Default::default()
    })
    .await;
    let id = f.opened().await.remove(0).unwrap();
    let host = pin(f.host_pid(&id)).unwrap();
    let mut client = f.attach(&id, "laptop").await;
    client.type_line("exit 7").await;
    client.written().await;
    drop(client);
    let exit_record = f.paths.metadata_run().join(format!("th-{id}.exit"));
    let deadline = Instant::now() + WAIT;
    while !exit_record.exists() {
        assert!(Instant::now() < deadline, "no exit record");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut daemon = f.daemon.take().unwrap();
    let deadline = Instant::now() + WAIT;
    let status = loop {
        if let Some(status) = daemon.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "the daemon did not idle out");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(status.success());
    let next = f.manager();
    let HostState::Exited { exit, host_alive } = next.list()[0].state.clone() else {
        panic!("{:?}", next.list());
    };
    assert_eq!((exit.code, exit.signal), (Some(7), None));
    if host_alive {
        // Nobody saw the exit yet: the host still holds the final screen and leaves once a client has seen it.
        let mut client = f.attach(&id, "laptop-again").await;
        let ServerFrame::Exited(exited_frame) = client.until(|frame| matches!(frame, ServerFrame::Exited(_))).await
        else {
            unreachable!()
        };
        assert_eq!(exited_frame.code, Some(7));
        assert!(client.next().await.is_none());
    }
    wait_exited(&host).await;
    assert!(f.audit().contains(&format!("terminal-exit id={id} code=7")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_hangs_up_and_reports_exit() {
    let f = Fixture::new();
    let hosts = f.manager();
    let id = hosts.start(f.request("open-close", &f.work())).await.unwrap();
    let host = pin(f.host_pid(&id)).unwrap();
    let mut client = f.attach(&id, "laptop").await;
    client.type_line("echo ready-$((1+1))").await;
    client.until_text("ready-2").await;
    let exit = hosts.close(&id, "close-1").await.unwrap().expect("an exit record");
    assert_eq!(exit.signal, Some(libc::SIGHUP), "{exit:?}");
    let ServerFrame::Detached(detached) = client.until(|frame| matches!(frame, ServerFrame::Detached(_))).await else {
        unreachable!()
    };
    assert_eq!(detached.reason, DetachReason::Closed as i32);
    assert!(client.next().await.is_none());
    wait_exited(&host).await;
    assert!(matches!(
        hosts.list()[0].state,
        HostState::Exited { host_alive: false, .. }
    ));
    let audit = f.audit();
    assert!(audit.contains(&format!("terminal-stop id={id} by=client")), "{audit}");
    assert!(
        audit.contains(&format!("terminal-exit id={id} code=none signal=1")),
        "{audit}"
    );
    // Closing again answers the same exit.
    assert_eq!(hosts.close(&id, "close-2").await.unwrap(), Some(exit));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_sigterm_tells_the_attached_client_stopped() {
    let mut f = Fixture::new();
    f.spawn_daemon(DaemonSpec {
        opens: vec![("open-a".into(), f.work()), ("open-b".into(), f.work())],
        ..Default::default()
    })
    .await;
    let ids: Vec<String> = f.opened().await.into_iter().map(Result::unwrap).collect();
    let pinned: Vec<OwnedFd> = ids.iter().map(|id| pin(f.host_pid(id)).unwrap()).collect();
    let mut client = f.attach(&ids[0], "laptop").await;
    // SAFETY: the daemon is our unreaped child.
    assert_eq!(unsafe { libc::kill(f.daemon_pid(), libc::SIGTERM) }, 0);
    let ServerFrame::Detached(detached) = client.until(|frame| matches!(frame, ServerFrame::Detached(_))).await else {
        unreachable!()
    };
    assert_eq!(detached.reason, DetachReason::Stopped as i32);
    assert!(client.next().await.is_none());
    let mut daemon = f.daemon.take().unwrap();
    assert!(daemon.wait().unwrap().success());
    for (id, host) in ids.iter().zip(&pinned) {
        assert!(exited(host));
        assert!(f.audit().contains(&format!("terminal-stop id={id} by=daemon-stop")));
    }
}

/// Every shell ignores SIGHUP, so each host's stop takes its whole close grace; 32 of them stop in about the time
/// one does, since the daemon stops them in parallel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_sigterm_stops_32_hosts_in_parallel() {
    async fn stop(f: &mut Fixture, count: usize) -> Duration {
        f.spawn_daemon(DaemonSpec {
            // Command ids of their own: a retried open answers the terminal its command id started.
            opens: (0..count).map(|n| (format!("open-{count}-{n}"), f.work())).collect(),
            ..Default::default()
        })
        .await;
        let ids: Vec<String> = f.opened().await.into_iter().map(Result::unwrap).collect();
        assert_eq!(ids.len(), count);
        for id in &ids {
            let mut client = f.attach(id, "laptop").await;
            client.type_line("trap '' HUP; echo trapped-$((2+3))").await;
            client.until_text("trapped-5").await;
        }
        f.stop_daemon().await
    }
    let mut f = Fixture::new();
    let one = stop(&mut f, 1).await;
    let all = stop(&mut f, terminals::MAX_HOSTS).await;
    assert!(all < one * 3, "32 hosts took {all:?} to stop where one took {one:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uninstall_inventory_counts_a_live_host() {
    let f = Fixture::new();
    let hosts = f.manager();
    let id = hosts.start(f.request("open-inventory", &f.work())).await.unwrap();
    let pid = f.host_pid(&id);
    let check = |extra: &[&str]| {
        Command::new(HOST)
            .arg("check-sidecars")
            .arg(&f.paths.state)
            .args(extra)
            .env_clear()
            .output()
            .unwrap()
    };
    let refused = check(&[]);
    assert!(!refused.status.success());
    let diagnostic = String::from_utf8_lossy(&refused.stderr);
    assert!(
        diagnostic.contains(&format!("run {VERSION} terminal {id} pid {pid} alive")),
        "{diagnostic}"
    );
    let live = check(&["--live-runs"]);
    assert!(live.status.success(), "{live:?}");
    let value: serde_json::Value = serde_json::from_slice(&live.stdout).unwrap();
    assert_eq!(value["run"], VERSION);
    hosts.close(&id, "close-inventory").await.unwrap();
    let cleared = check(&[]);
    assert!(cleared.status.success(), "{cleared:?}");
    assert!(check(&["--live-runs"]).stdout.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_33rd_terminal_is_refused() {
    let f = Fixture::new();
    let hosts = f.manager();
    let starts = (0..terminals::MAX_HOSTS).map(|n| {
        let hosts = hosts.clone();
        let request = f.request(&format!("open-{n}"), &f.work());
        async move { hosts.start(request).await }
    });
    for started in futures_util::future::join_all(starts).await {
        started.unwrap();
    }
    let files = || std::fs::read_dir(f.paths.metadata_run()).unwrap().count();
    let before = files();
    let error = hosts.start(f.request("open-33", &f.work())).await.unwrap_err();
    // A refused open leaves nothing behind: no log, record or socket.
    assert_eq!(files(), before);
    assert_eq!(error.code(), Code::ResourceExhausted);
    assert_eq!(error.message(), "too many terminals on this server");
    // A retried open of a running terminal still answers it.
    assert!(hosts.start(f.request("open-0", &f.work())).await.is_ok());
    hosts.stop_all().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_records_no_output() {
    let f = Fixture::new();
    let hosts = f.manager();
    let id = hosts.start(f.request("open-audit", &f.work())).await.unwrap();
    let mut client = f.attach(&id, "laptop").await;
    client.type_line("echo secret-$((6*7))-output").await;
    client.until_text("secret-42-output").await;
    hosts.close(&id, "close-audit").await.unwrap();
    let audit = f.audit();
    for event in ["terminal-start", "terminal-exit", "terminal-stop"] {
        assert!(audit.contains(&format!("{event} id={id}")), "{event}: {audit}");
    }
    assert!(
        audit.contains(&format!("dir={:?}", f.work().display().to_string())),
        "{audit}"
    );
    assert!(!audit.contains("secret"), "{audit}");
    for log in ["out", "err"] {
        let text = std::fs::read(f.paths.metadata_run().join(format!("th-{id}.{log}"))).unwrap_or_default();
        assert!(!contains(&text, "secret"));
    }
}

/// A host's socket is a 0600 socket in the private run directory, and a connection is admitted only from the host's
/// own uid, checked before HTTP/2 reads a byte (as the daemon's and a sidecar's are).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_host_socket_refuses_another_uid() {
    let f = Fixture::new();
    let hosts = f.manager();
    let id = hosts.start(f.request("open-uid", &f.work())).await.unwrap();
    let socket = f.socket(&id);
    let meta = std::fs::symlink_metadata(&socket).unwrap();
    assert!(meta.file_type().is_socket());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert!(private_fs::is_private_dir(&f.paths.run));
    let own = private_fs::euid();
    assert_eq!(info(&socket).await.terminal_id, id);
    hosts.stop_all().await.unwrap();
    // The host's own accept path, told to admit another uid only: this user's connection is closed unread.
    use futures_util::StreamExt;
    use tokio::io::AsyncReadExt;
    let probe = f.root.join("probe.sock");
    let mut refusing = Box::pin(terminals::host::admitted(
        tokio::net::UnixListener::bind(&probe).unwrap(),
        own.wrapping_add(1),
    ));
    let mut client = tokio::net::UnixStream::connect(&probe).await.unwrap();
    let mut accepted = std::pin::pin!(refusing.next());
    let mut byte = [0u8; 1];
    tokio::select! {
        _ = &mut accepted => panic!("another uid's connection was admitted"),
        read = client.read(&mut byte) => assert_eq!(read.unwrap(), 0, "the refused connection must be closed unread"),
    }
    drop(accepted);
    drop(refusing);
    std::fs::remove_file(&probe).unwrap();
    let mut admitting = Box::pin(terminals::host::admitted(
        tokio::net::UnixListener::bind(&probe).unwrap(),
        own,
    ));
    let _client = tokio::net::UnixStream::connect(&probe).await.unwrap();
    assert!(admitting.next().await.unwrap().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_hello_must_name_this_terminal_and_input_frames_are_bounded() {
    let f = Fixture::new();
    let hosts = f.manager();
    let id = hosts.start(f.request("open-frames", &f.work())).await.unwrap();
    let socket = f.socket(&id);
    let (tx, frames) = tokio::sync::mpsc::channel(4);
    tx.send(TerminalClientFrame {
        frame: Some(ClientFrame::Hello(AttachHello {
            terminal_id: "0".repeat(32),
            client_id: "laptop".into(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    let error = TerminalClient::new(channel(&socket).await)
        .attach(tokio_stream::wrappers::ReceiverStream::new(frames))
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::NotFound);
    let mut client = f.attach(&id, "laptop").await;
    client
        .send(ClientFrame::Input(InputBytes {
            offset: 0,
            data: vec![b'x'; TERM_INPUT_MAX + 1],
        }))
        .await;
    while client.next().await.is_some() {}
    assert_eq!(
        client.status.as_ref().map(tonic::Status::code),
        Some(Code::InvalidArgument)
    );
    // A second AttachHello on one stream ends it the same way.
    let mut client = f.attach(&id, "laptop").await;
    client
        .send(ClientFrame::Hello(AttachHello {
            terminal_id: id.clone(),
            client_id: "laptop".into(),
            ..Default::default()
        }))
        .await;
    while client.next().await.is_some() {}
    assert_eq!(
        client.status.as_ref().map(tonic::Status::code),
        Some(Code::InvalidArgument)
    );
    assert!(info(&socket).await.alive);
    hosts.stop_all().await.unwrap();
}

/// A stream that sends no AttachHello is refused when its five seconds are up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_hello_must_come_within_its_deadline() {
    let f = Fixture::new();
    let hosts = f.manager();
    let id = hosts.start(f.request("open-hello", &f.work())).await.unwrap();
    let (_tx, frames) = tokio::sync::mpsc::channel::<TerminalClientFrame>(1);
    let started = Instant::now();
    let error = TerminalClient::new(channel(&f.socket(&id)).await)
        .attach(tokio_stream::wrappers::ReceiverStream::new(frames))
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::DeadlineExceeded);
    assert!(started.elapsed() >= terminals::host::HELLO_WITHIN);
    hosts.stop_all().await.unwrap();
}

/// The snapshot stands at the size the AttachHello asked for, not the size the terminal was opened at: a laptop's
/// mirror takes no size it did not ask for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_snapshot_has_the_attach_size() {
    let f = Fixture::new();
    let hosts = f.manager();
    let id = hosts.start(f.request("open-size", &f.work())).await.unwrap();
    let size = TermSize {
        columns: 120,
        rows: 40,
        cell_width: 9,
        cell_height: 18,
    };
    let client = Attached::sized(&f.socket(&id), &id, "laptop", size).await;
    assert_eq!(client.size, (120, 40));
    assert_eq!(info(&f.socket(&id)).await.size, Some(size));
    hosts.stop_all().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_attach_replaces_the_first() {
    let f = Fixture::new();
    let hosts = f.manager();
    let id = hosts.start(f.request("open-replace", &f.work())).await.unwrap();
    let mut first = f.attach(&id, "laptop-1").await;
    let _second = f.attach(&id, "laptop-2").await;
    let ServerFrame::Detached(detached) = first.until(|frame| matches!(frame, ServerFrame::Detached(_))).await else {
        unreachable!()
    };
    assert_eq!(detached.reason, DetachReason::Replaced as i32);
    assert!(first.next().await.is_none());
    hosts.stop_all().await.unwrap();
}

/// A stream replaced by another of the same client id, its response blocked on a client that stopped reading, can
/// no longer type into the terminal or resize it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replaced_stream_no_longer_acts_on_the_terminal() {
    let f = Fixture::new();
    let hosts = f.manager();
    let id = hosts.start(f.request("open-stale", &f.work())).await.unwrap();
    let mut stale = f.attach(&id, "laptop").await;
    stale
        .type_line("yes flood | head -c 8000000; echo flood-$((3+4))-done")
        .await;
    stale.written().await;
    // `stale` reads nothing from here: the flood fills its stream and blocks its sender.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut current = f.attach(&id, "laptop").await;
    stale.input(b"echo injected-$((1+1))\r".to_vec()).await.unwrap();
    stale
        .send(ClientFrame::Resize(TermSize {
            columns: 100,
            rows: 30,
            cell_width: 8,
            cell_height: 16,
        }))
        .await;
    current.until_text("flood-7-done").await;
    current.type_line("echo check-$((2+2))").await;
    current.until_text("check-4").await;
    assert!(
        !contains(&current.text, "injected-2"),
        "the replaced stream typed into the terminal"
    );
    assert_eq!(info(&f.socket(&id)).await.size.unwrap().columns, 80);
    hosts.stop_all().await.unwrap();
}

/// A client stops reading while the program floods, then ends its side of the stream with input the program has not
/// read: that input is discarded at once, so the program later reads at most what the PTY itself had buffered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_ending_with_input_pending_discards_it() {
    let f = Fixture::new();
    let hosts = f.manager();
    let id = hosts.start(f.request("open-pending", &f.work())).await.unwrap();
    let mut client = f.attach(&id, "laptop").await;
    client
        .type_line(
            // Raw input mode first: a canonical line discipline drops what overflows its line, so input would never
            // wait in the host.
            "stty -icanon; yes flood | head -c 8000000; timeout --foreground 3 cat | wc -c > counted.tmp; mv counted.tmp counted",
        )
        .await;
    client.written().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let sent = 8 * TERM_INPUT_MAX;
    for _ in 0..8 {
        client.input(vec![b'a'; TERM_INPUT_MAX]).await.unwrap();
    }
    client.end_sending();
    let counted = f.work().join("counted");
    let deadline = Instant::now() + WAIT;
    while !counted.exists() {
        assert!(Instant::now() < deadline, "the program did not finish reading");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let text = std::fs::read_to_string(&counted).unwrap();
    let read: usize = text
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("counted {text:?}; screen: {}", String::from_utf8_lossy(&client.text)));
    assert!(
        read < sent / 4,
        "the program read {read} of {sent} bytes typed before the stream ended"
    );
    drop(client);
    hosts.stop_all().await.unwrap();
}

/// Input for a program that does not read stdin is not buffered without bound: past 1 MiB admitted and unwritten,
/// the attachment ends with RESOURCE_EXHAUSTED.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_past_its_budget_ends_the_attachment() {
    let f = Fixture::new();
    let hosts = f.manager();
    let id = hosts.start(f.request("open-budget", &f.work())).await.unwrap();
    let mut client = f.attach(&id, "laptop").await;
    // Raw input mode: the line discipline then holds input back instead of dropping what overflows a line.
    client.type_line("stty -icanon; sleep 30").await;
    client.written().await;
    let mut frames = 0;
    while frames < 64 && client.input(vec![b'a'; TERM_INPUT_MAX]).await.is_some() {
        frames += 1;
    }
    while client.next().await.is_some() {}
    assert_eq!(
        client.status.as_ref().map(tonic::Status::code),
        Some(Code::ResourceExhausted),
        "{:?}",
        client.status
    );
    assert!(info(&f.socket(&id)).await.alive);
    hosts.stop_all().await.unwrap();
}

/// Every process of a session `sid`, zombies aside.
fn session_processes(sid: u32) -> Vec<u32> {
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            let fields: Vec<&str> = stat
                .rsplit_once(')')
                .map(|(_, rest)| rest.split_whitespace().collect())
                .unwrap_or_default();
            fields.first() != Some(&"Z") && fields.get(3) == Some(&sid.to_string().as_str())
        })
        .collect()
}

/// A shell that survives the hangup (it traps HUP) and answers it by starting a job, in a process group of its own,
/// during the close grace: the shell still lives at the rescan, so the job is found and ended with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_kills_a_job_started_during_the_grace() {
    let f = Fixture::new();
    let hosts = f.manager();
    let id = hosts.start(f.request("open-grace", &f.work())).await.unwrap();
    let shell = info(&f.socket(&id)).await.pid;
    let mut client = f.attach(&id, "laptop").await;
    client
        .type_line(r#"trap 'sleep 60 & echo $! > job' HUP; echo trapping; while :; do sleep 0.1; done"#)
        .await;
    client.until_text("trapping\r").await;
    hosts.close(&id, "close-grace").await.unwrap();
    assert!(
        f.work().join("job").exists(),
        "the shell did not start its job during the grace"
    );
    assert_eq!(session_processes(shell), Vec::<u32>::new());
}

/// With the host's socket gone the daemon cannot ask it to close; it kills the shell's session and the host itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_without_the_host_socket_still_ends_the_shell() {
    let f = Fixture::new();
    let hosts = f.manager();
    let id = hosts.start(f.request("open-nosocket", &f.work())).await.unwrap();
    let shell = info(&f.socket(&id)).await.pid;
    let shell_fd = pin(shell).unwrap();
    let host = pin(f.host_pid(&id)).unwrap();
    std::fs::remove_file(f.socket(&id)).unwrap();
    hosts.close(&id, "close-nosocket").await.unwrap();
    wait_exited(&shell_fd).await;
    wait_exited(&host).await;
    assert_eq!(session_processes(shell), Vec::<u32>::new());
}

fn rewrite_record(f: &Fixture, id: &str, edit: impl FnOnce(&mut serde_json::Value)) {
    let mut record = f.record(id);
    edit(&mut record);
    private_fs::write_private(
        &f.paths.metadata_run().join(format!("th-{id}.pid")),
        &serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
}

fn host_records(f: &Fixture) -> usize {
    std::fs::read_dir(f.paths.metadata_run())
        .unwrap()
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("th-") && name.ends_with(".pid")
        })
        .count()
}

/// A daemon killed between a host's record and its readiness leaves an unfinished open: a retry of that open finishes
/// it when the host answers, and only replaces it (stopped, marked failed) when the host is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unfinished_open_is_finished_before_another_host_starts() {
    let f = Fixture::new();
    let first = f.manager();
    let id = first.start(f.request("open-unfinished", &f.work())).await.unwrap();
    rewrite_record(&f, &id, |record| record["ready"] = false.into());
    let next = f.manager();
    assert_eq!(next.start(f.request("open-unfinished", &f.work())).await.unwrap(), id);
    assert_eq!(host_records(&f), 1);
    assert!(matches!(next.list()[0].state, HostState::Live { handshake: true }));
    // Its host gone, the unfinished open is marked failed and a new one starts.
    rewrite_record(&f, &id, |record| record["ready"] = false.into());
    let host = pin(f.host_pid(&id)).unwrap();
    daemon::sidecars::signal(&host, libc::SIGKILL).unwrap();
    wait_exited(&host).await;
    let last = f.manager();
    let replaced = last.start(f.request("open-unfinished", &f.work())).await.unwrap();
    assert_ne!(replaced, id);
    let listed = last.list();
    let old = listed.iter().find(|info| info.terminal_id == id).unwrap();
    assert!(
        matches!(old.state, HostState::Failed { alive: false, .. }),
        "{:?}",
        old.state
    );
    last.stop_all().await.unwrap();
}

/// A record the daemon cannot adopt (here, a bad command id) is not dropped: it is audited, listed as failed while its
/// host runs, and the host is stopped through its pidfd.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unadoptable_record_is_listed_and_stoppable() {
    let f = Fixture::new();
    let first = f.manager();
    let id = first.start(f.request("open-refused", &f.work())).await.unwrap();
    let host = pin(f.host_pid(&id)).unwrap();
    rewrite_record(&f, &id, |record| record["command_id"] = "not a command id".into());
    let next = f.manager();
    assert!(
        f.audit().contains(&format!("terminal-record-refused id={id}")),
        "{}",
        f.audit()
    );
    let listed = next.list();
    assert!(
        matches!(&listed[0].state, HostState::Failed { alive: true, why } if why.contains("command id")),
        "{:?}",
        listed[0].state
    );
    assert!(next.any_live());
    next.stop_all().await.unwrap();
    wait_exited(&host).await;
}

/// A daemon told to stop answers no Hello while its terminals stop, so `ensure` cannot take it for a ready daemon.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopping_daemon_answers_no_hello() {
    let mut f = Fixture::new();
    f.spawn_daemon(DaemonSpec {
        opens: vec![("open-stopping".into(), f.work())],
        ..Default::default()
    })
    .await;
    let id = f.opened().await.remove(0).unwrap();
    let mut client = f.attach(&id, "laptop").await;
    client.type_line("trap '' HUP; echo trapped-$((2+3))").await;
    client.until_text("trapped-5").await;
    drop(client);
    let hello = |socket: PathBuf| async move {
        let channel = tonic::transport::Endpoint::from_static("http://localhost")
            .connect_with_connector(tower::service_fn(move |_| {
                let socket = socket.clone();
                async move {
                    let stream = tokio::net::UnixStream::connect(socket).await?;
                    Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
                }
            }))
            .await
            .map_err(|error| error.to_string())?;
        tokio::time::timeout(
            Duration::from_secs(2),
            eitri_server_proto::helper_client::HelperClient::new(channel).hello(HelloRequest {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                client_version: VERSION.into(),
            }),
        )
        .await
        .map_err(|_| "timed out".to_owned())?
        .map(|_| ())
        .map_err(|status| status.to_string())
    };
    let socket = f.paths.socket().unwrap();
    hello(socket.clone()).await.unwrap();
    // SAFETY: the daemon is our unreaped child.
    assert_eq!(unsafe { libc::kill(f.daemon_pid(), libc::SIGTERM) }, 0);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut daemon = f.daemon.take().unwrap();
    let mut asked = 0;
    while daemon.try_wait().unwrap().is_none() {
        assert!(hello(socket.clone()).await.is_err(), "a stopping daemon answered Hello");
        asked += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(asked > 0, "the daemon stopped before it could be asked");
}

/// The arguments reach the host through a pipe written only after the host runs: a payload far larger than a one-page
/// pipe gets through.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_argument_payload_passes_a_one_page_pipe() {
    let f = Fixture::new();
    let mut config = config(&f.root);
    let identity = config.identity.as_mut().unwrap();
    identity.insert("HOME".into(), format!("/nonexistent/{}", "x".repeat(9000)).into());
    config.faults.small_args_pipe = true;
    let hosts = f.manager_with(config);
    let id = tokio::time::timeout(WAIT, hosts.start(f.request("open-large", &f.work())))
        .await
        .expect("the start blocked on its argument pipe")
        .unwrap();
    let shell = info(&f.socket(&id)).await.pid;
    assert_eq!(
        environ(shell).get(OsStr::new("HOME")).unwrap().len(),
        "/nonexistent/".len() + 9000
    );
    hosts.stop_all().await.unwrap();
}

/// A client that stopped reading goes past its input budget and half-closes: the refusal does not keep its
/// attachment, so the input still waiting is discarded and the program never reads it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_stream_drops_its_pending_input_at_once() {
    let f = Fixture::new();
    let hosts = f.manager();
    let id = hosts.start(f.request("open-refused-input", &f.work())).await.unwrap();
    let mut client = f.attach(&id, "laptop").await;
    client
        .type_line(
            "stty -icanon; yes flood | head -c 8000000; timeout --foreground 3 cat | wc -c > counted.tmp; mv counted.tmp counted",
        )
        .await;
    client.written().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut sent = 0;
    while sent < 40
        && client
            .input_within(vec![b'a'; TERM_INPUT_MAX], Duration::from_secs(1))
            .await
    {
        sent += 1;
    }
    assert!(sent * TERM_INPUT_MAX > 1024 * 1024, "only {sent} frames went out");
    client.end_sending();
    let counted = f.work().join("counted");
    let deadline = Instant::now() + WAIT;
    while !counted.exists() {
        assert!(Instant::now() < deadline, "the program did not finish reading");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let read: usize = std::fs::read_to_string(&counted).unwrap().trim().parse().unwrap();
    assert!(
        read < 256 * 1024,
        "the program read {read} bytes typed into a refused stream"
    );
    drop(client);
    hosts.stop_all().await.unwrap();
}

/// A job that outlives its shell is not hunted: once the shell is gone its session id may belong to someone else.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_leaves_a_job_that_outlived_its_shell() {
    let mut f = Fixture::new();
    let hosts = f.manager();
    let id = hosts.start(f.request("open-left-job", &f.work())).await.unwrap();
    let shell = pin(info(&f.socket(&id)).await.pid).unwrap();
    let mut client = f.attach(&id, "laptop").await;
    client
        .type_line("trap '' HUP; sleep 600 & echo job=$!; sleep 1; exit")
        .await;
    let mut job = None;
    while job.is_none() {
        client.next().await.expect("stream ended");
        job = number_after(&client.text, "job=");
    }
    drop(client);
    let job = pin(job.unwrap()).unwrap();
    wait_exited(&shell).await;
    hosts.close(&id, "close-left-job").await.unwrap();
    assert!(!exited(&job), "a job that outlived its shell was hunted");
    f.strays.push(job);
}

/// A retried open whose earlier start cannot be stopped is refused, never answered with a second host; once the
/// earlier host is stopped (here by the next daemon's maintenance) the open starts one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unstoppable_earlier_start_refuses_the_retry() {
    let f = Fixture::new();
    let first = f.manager();
    let id = first.start(f.request("open-stuck", &f.work())).await.unwrap();
    rewrite_record(&f, &id, |record| record["ready"] = false.into());
    // It cannot answer its handshake, and killing it fails.
    std::fs::remove_file(f.socket(&id)).unwrap();
    let mut stuck = config(&f.root);
    stuck.faults.fail_kills = true;
    let next = f.manager_with(stuck);
    for _ in 0..2 {
        let error = next.start(f.request("open-stuck", &f.work())).await.unwrap_err();
        assert_eq!(error.code(), Code::Unavailable, "{error}");
        assert_eq!(host_records(&f), 1);
    }
    let host = pin(f.host_pid(&id)).unwrap();
    assert!(!exited(&host));
    let last = f.manager();
    last.maintain().await;
    wait_exited(&host).await;
    let started = last.start(f.request("open-stuck", &f.work())).await.unwrap();
    assert_ne!(started, id);
    last.stop_all().await.unwrap();
}

/// A daemon that cannot ask the host ends only a shell still running as recorded: a job left after its shell exited
/// is not touched by the daemon's own cleanup.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_daemon_fallback_never_signals_what_it_cannot_prove() {
    let mut f = Fixture::new();
    let hosts = f.manager();
    let id = hosts.start(f.request("open-fallback", &f.work())).await.unwrap();
    let shell = pin(info(&f.socket(&id)).await.pid).unwrap();
    let mut client = f.attach(&id, "laptop").await;
    client
        .type_line("trap '' HUP; sleep 600 & echo job=$!; sleep 1; exit")
        .await;
    let mut job = None;
    while job.is_none() {
        client.next().await.expect("stream ended");
        job = number_after(&client.text, "job=");
    }
    drop(client);
    let job = pin(job.unwrap()).unwrap();
    wait_exited(&shell).await;
    let host = pin(f.host_pid(&id)).unwrap();
    std::fs::remove_file(f.socket(&id)).unwrap();
    hosts.close(&id, "close-fallback").await.unwrap();
    wait_exited(&host).await;
    assert!(
        !exited(&job),
        "the daemon's fallback reached a process only the PTY named"
    );
    f.strays.push(job);
}

/// A shell that exits at once (here the login shell is `/bin/true`) is still recorded as itself: it was pinned by the
/// terminal right after its spawn, before anything could reap it and its pid could name another process.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_shell_is_pinned_before_it_can_be_reaped() {
    let f = Fixture::new();
    let mut config = config(&f.root);
    config
        .identity
        .as_mut()
        .unwrap()
        .insert("SHELL".into(), "/bin/true".into());
    let hosts = f.manager_with(config);
    let id = hosts.start(f.request("open-true", &f.work())).await.unwrap();
    let listed = info(&f.socket(&id)).await;
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(f.paths.metadata_run().join(format!("th-{id}.shell"))).unwrap()).unwrap();
    assert_eq!(record["pid"].as_u64(), Some(u64::from(listed.pid)));
    let start = record["start_time"].as_u64().unwrap();
    assert!(
        start >= f.record(&id)["start_time"].as_u64().unwrap(),
        "the shell started before its host"
    );
    // Whatever runs under that pid now is not the recorded shell.
    if let Ok(now) = daemon::sidecars::start_time(listed.pid) {
        assert_ne!(now, start);
    }
    hosts.stop_all().await.unwrap();
}
