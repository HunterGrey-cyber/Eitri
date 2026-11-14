#![allow(dead_code)]

#[path = "../src/audit.rs"]
mod audit;
#[path = "../src/daemon.rs"]
mod daemon;
#[path = "../src/ensure.rs"]
mod ensure;
#[path = "../src/gc.rs"]
mod gc;
#[path = "../src/peer.rs"]
mod peer;
#[path = "../src/run_dir.rs"]
mod run_dir;

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime};

use agent::private_fs;
use eitri_server_proto::probe::DaemonInfo;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REVISION: &str = env!("EITRI_BUILD_COMMIT");
const BINARY: &str = env!("CARGO_BIN_EXE_eitri-server");

fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}

/// The installation the fixture's daemon is taken to run from: a test binary runs from no installation of its own.
fn installation(paths: &run_dir::Paths) -> PathBuf {
    paths.data.join(format!("{VERSION}-{}", run_dir::rev7(REVISION)))
}

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        static SERIAL: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "d{:x}-{:x}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        private_fs::create_private_dir_all(&root.join("r"), &root).unwrap();
        Self { root }
    }

    fn paths(&self) -> run_dir::Paths {
        run_dir::Paths::new(
            VERSION,
            &self.root.join("d"),
            &self.root.join("s"),
            &std::env::temp_dir(),
        )
        .unwrap()
    }

    fn command(&self) -> Command {
        let mut command = Command::new(BINARY);
        self.configure(&mut command);
        command
    }

    fn configure(&self, command: &mut Command) {
        command
            .env("HOME", &self.root)
            .env("XDG_DATA_HOME", self.root.join("d"))
            .env("XDG_STATE_HOME", self.root.join("s"))
            .env("XDG_RUNTIME_DIR", self.root.join("r"))
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("DBUS_SYSTEM_BUS_ADDRESS");
    }

    fn start(&self, idle: u64) -> Process {
        let mut command = self.command();
        command.args(["daemon", "--idle-seconds", &idle.to_string()]);
        let mut process = Process(command.spawn().unwrap());
        let paths = self.paths();
        wait_for(|| {
            assert!(
                process.0.try_wait().unwrap().is_none(),
                "daemon exited before readiness"
            );
            runtime().block_on(ensure::hello(&paths)).is_ok()
        });
        process
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Ok(paths) = run_dir::Paths::new(
            VERSION,
            &self.root.join("d"),
            &self.root.join("s"),
            &std::env::temp_dir(),
        ) {
            if let Ok(info) = paths.published().map(|published| published.info) {
                let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, info.pid as libc::pid_t, 0) };
                if fd >= 0 {
                    use std::os::fd::{AsRawFd, FromRawFd};
                    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) };
                    if std::fs::read_link(format!("/proc/{}/exe", info.pid)).ok().as_deref()
                        == Some(std::path::Path::new(BINARY))
                    {
                        unsafe {
                            libc::syscall(
                                libc::SYS_pidfd_send_signal,
                                fd.as_raw_fd(),
                                libc::SIGTERM,
                                std::ptr::null::<libc::siginfo_t>(),
                                0,
                            );
                        }
                        let deadline = Instant::now() + Duration::from_secs(5);
                        loop {
                            let mut poll = libc::pollfd {
                                fd: fd.as_raw_fd(),
                                events: libc::POLLIN,
                                revents: 0,
                            };
                            let ready = unsafe { libc::poll(&mut poll, 1, 0) };
                            if ready > 0 || Instant::now() >= deadline {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn wait_for(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !predicate() {
        assert!(Instant::now() < deadline, "event did not arrive before hang guard");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn write_info(paths: &run_dir::Paths, pid: u32) {
    paths.prepare().unwrap();
    private_fs::write_private(
        &paths.metadata_run().join("info"),
        &serde_json::to_vec(&DaemonInfo {
            version: paths.version.clone(),
            rev: REVISION.to_owned(),
            pid,
            protocol_major: 1,
            protocol_minor: 0,
            started_unix_ms: 0,
        })
        .unwrap(),
    )
    .unwrap();
}

#[test]
fn concurrent_ensures_start_one_daemon() {
    let sandbox = Sandbox::new();
    let mut children: Vec<_> = (0..8)
        .map(|_| Process(sandbox.command().arg("ensure").stderr(Stdio::piped()).spawn().unwrap()))
        .collect();
    for child in &mut children {
        wait_for(|| child.0.try_wait().unwrap().is_some());
        assert!(child.0.wait().unwrap().success());
    }
    let paths = sandbox.paths();
    let first = paths.published().map(|published| published.info).unwrap().pid;
    assert!(sandbox.command().arg("ensure").status().unwrap().success());
    assert_eq!(paths.published().map(|published| published.info).unwrap().pid, first);
    let audit = private_fs::read_private_nofollow_capped(&paths.state.join("audit.log"), 16384).unwrap();
    assert_eq!(audit.lines().filter(|line| line.contains(" daemon-start ")).count(), 1);
    assert_eq!(
        std::fs::metadata(paths.socket().unwrap()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    for path in [
        &paths.state,
        &paths.run,
        &paths.state.join("journal"),
        &paths.state.join("pending"),
    ] {
        assert!(private_fs::is_private_dir(path));
    }
}

#[test]
fn daemon_survives_its_starters_session_ending() {
    use std::os::unix::process::CommandExt;
    let sandbox = Sandbox::new();
    let ready = sandbox.root.join("ready");
    let mut command = Command::new("/bin/sh");
    sandbox.configure(&mut command);
    command
        .args(["-c", "\"$1\" ensure || exit; : >\"$2\"; exec sleep 60", "starter"])
        .arg(BINARY)
        .arg(&ready);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut starter = Process(command.spawn().unwrap());
    wait_for(|| {
        assert!(starter.0.try_wait().unwrap().is_none(), "starter failed");
        ready.exists()
    });
    unsafe { libc::kill(-(starter.0.id() as i32), libc::SIGKILL) };
    assert!(runtime().block_on(ensure::hello(&sandbox.paths())).is_ok());
}

#[test]
fn daemon_exits_when_idle_and_owning_nothing() {
    let sandbox = Sandbox::new();
    let mut daemon = sandbox.start(2);
    wait_for(|| daemon.0.try_wait().unwrap().is_some());
    assert!(daemon.0.wait().unwrap().success());
    assert!(!sandbox.paths().socket().unwrap().exists());
    assert!(!sandbox.paths().metadata_run().join("info").exists());
}

#[test]
fn daemon_does_not_exit_while_a_connection_is_open() {
    let sandbox = Sandbox::new();
    let mut daemon = sandbox.start(2);
    let stream = std::os::unix::net::UnixStream::connect(sandbox.paths().socket().unwrap()).unwrap();
    // The wait spans the injected idle period; it is a liveness observation, not a speed bound.
    std::thread::sleep(Duration::from_secs(3));
    assert!(daemon.0.try_wait().unwrap().is_none());
    drop(stream);
    wait_for(|| daemon.0.try_wait().unwrap().is_some());
    assert!(daemon.0.wait().unwrap().success());
}

#[test]
fn connect_closes_the_far_side_when_stdin_ends() {
    let sandbox = Sandbox::new();
    let paths = sandbox.paths();
    paths.prepare().unwrap();
    let listener = std::os::unix::net::UnixListener::bind(paths.socket().unwrap()).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut child = Process(
        sandbox
            .command()
            .arg("connect")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut accepted = None;
    wait_for(|| match listener.accept() {
        Ok((stream, _)) => {
            accepted = Some(stream);
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
        Err(error) => panic!("accept: {error}"),
    });
    let mut stream = accepted.unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    child.0.stdin.as_mut().unwrap().write_all(b"opaque bytes").unwrap();
    let mut bytes = [0; 12];
    stream.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"opaque bytes");
    drop(child.0.stdin.take());
    assert_eq!(stream.read(&mut [0]).unwrap(), 0);
    wait_for(|| child.0.try_wait().unwrap().is_some());
    assert!(child.0.wait().unwrap().success());
}

#[test]
fn connect_exits_when_the_socket_closes_even_with_stdin_open() {
    let sandbox = Sandbox::new();
    let paths = sandbox.paths();
    paths.prepare().unwrap();
    let listener = std::os::unix::net::UnixListener::bind(paths.socket().unwrap()).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut child = Process(
        sandbox
            .command()
            .arg("connect")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for(|| match listener.accept() {
        Ok((stream, _)) => {
            drop(stream);
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
        Err(error) => panic!("accept: {error}"),
    });
    wait_for(|| child.0.try_wait().unwrap().is_some());
    assert!(child.0.wait().unwrap().success());
}

#[test]
fn long_home_falls_back_to_a_private_tmp_run_dir() {
    let sandbox = Sandbox::new();
    let home = sandbox.root.join("long".repeat(50));
    let paths = run_dir::Paths::new(
        VERSION,
        &home.join(".local/share"),
        &home.join(".local/state"),
        &std::env::temp_dir(),
    )
    .unwrap();
    assert_eq!(
        paths.run,
        std::env::temp_dir()
            .join(format!("eitri-server-{}", private_fs::euid()))
            .join(VERSION)
    );
    struct RemoveRun(PathBuf);
    impl Drop for RemoveRun {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = RemoveRun(paths.run.clone());
    paths.prepare().unwrap();
    assert!(private_fs::is_private_dir(&paths.run));
    assert!(agent::socket_path::in_dir(&paths.run, "sc-0123456789abcdef.sock").is_ok());
    let mut command = sandbox.command();
    command
        .env("HOME", &home)
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_STATE_HOME");
    let mut process = Process(command.arg("daemon").spawn().unwrap());
    wait_for(|| {
        assert!(process.0.try_wait().unwrap().is_none());
        runtime().block_on(ensure::hello(&paths)).is_ok()
    });
    assert_eq!(
        paths.published().map(|published| published.info).unwrap().pid,
        process.0.id()
    );
    let listed = sandbox
        .command()
        .arg("list-daemons")
        .env("HOME", &home)
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_STATE_HOME")
        .output()
        .unwrap();
    assert!(listed.status.success());
    let listed: DaemonInfo = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed.pid, process.0.id());
    unsafe { libc::kill(process.0.id() as i32, libc::SIGTERM) };
    wait_for(|| process.0.try_wait().unwrap().is_some());
    assert!(process.0.wait().unwrap().success());
}

#[test]
fn list_daemons_reports_only_live_answering_daemons() {
    let sandbox = Sandbox::new();
    let _daemon = sandbox.start(60);
    let stale = run_dir::Paths::new(
        "old",
        &sandbox.root.join("d"),
        &sandbox.root.join("s"),
        &std::env::temp_dir(),
    )
    .unwrap();
    write_info(&stale, std::process::id());
    let output = sandbox.command().arg("list-daemons").output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let records: Vec<DaemonInfo> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records, vec![sandbox.paths().published().unwrap().info]);
}

#[test]
fn connect_reaches_another_versions_daemon() {
    if std::env::var_os("EITRI_TEST_OLDER_DAEMON").is_some() {
        runtime()
            .block_on(daemon::run(
                run_dir::Paths::from_env("older").unwrap(),
                Duration::from_secs(60),
            ))
            .unwrap();
        return;
    }
    let sandbox = Sandbox::new();
    let paths = run_dir::Paths::new(
        "older",
        &sandbox.root.join("d"),
        &sandbox.root.join("s"),
        &std::env::temp_dir(),
    )
    .unwrap();
    paths.prepare().unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    sandbox.configure(&mut command);
    let mut older = Process(
        command
            .args(["--exact", "connect_reaches_another_versions_daemon", "--nocapture"])
            .env("EITRI_TEST_OLDER_DAEMON", "1")
            .spawn()
            .unwrap(),
    );
    wait_for(|| {
        assert!(older.0.try_wait().unwrap().is_none());
        runtime().block_on(ensure::hello(&paths)).is_ok()
    });
    runtime().block_on(async {
        let mut bridge_command = tokio::process::Command::from(sandbox.command());
        let mut bridge = bridge_command
            .args(["connect", "--daemon", "older"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let io = tokio::io::join(bridge.stdout.take().unwrap(), bridge.stdin.take().unwrap());
        let io = std::sync::Arc::new(std::sync::Mutex::new(Some(io)));
        let endpoint =
            tonic::transport::Endpoint::from_static("http://[::]:50051").connect_timeout(Duration::from_secs(10));
        let channel = endpoint
            .connect_with_connector(tower::service_fn(move |_| {
                let io = io.lock().unwrap().take();
                async move {
                    io.map(hyper_util::rt::TokioIo::new)
                        .ok_or_else(|| std::io::Error::other("test bridge already connected"))
                }
            }))
            .await
            .unwrap();
        let response = tokio::time::timeout(
            Duration::from_secs(10),
            eitri_server_proto::helper_client::HelperClient::new(channel).hello(eitri_server_proto::HelloRequest {
                protocol_major: 1,
                protocol_minor: 0,
                client_version: VERSION.into(),
            }),
        )
        .await
        .unwrap()
        .unwrap()
        .into_inner();
        assert_eq!(response.protocol_major, 1);
        assert!(response.capabilities.contains(&"sidecars".to_owned()));
        bridge.kill().await.unwrap();
    });
    unsafe { libc::kill(older.0.id() as i32, libc::SIGTERM) };
    wait_for(|| older.0.try_wait().unwrap().is_some());
    assert!(older.0.wait().unwrap().success());
}

#[test]
fn stop_daemon_checks_the_exe_before_signalling() {
    let sandbox = Sandbox::new();
    let mut child = Process(Command::new("/bin/sleep").arg("60").spawn().unwrap());
    let paths = sandbox.paths();
    write_info(&paths, child.0.id());
    private_fs::create_private_dir_all(&installation(&paths).join("bin"), &paths.data).unwrap();
    let encoded = eitri_server_proto::args_hex::encode_args(&[std::ffi::OsStr::new(VERSION)]);
    let output = sandbox
        .command()
        .args(["stop-daemon", "--args-hex", &encoded])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("daemon executable does not match its recorded installation identity; refusing to signal"),
        "{stderr}"
    );
    assert!(child.0.try_wait().unwrap().is_none());
    std::fs::remove_file(paths.metadata_run().join("info")).unwrap();
}

#[test]
fn stop_daemon_stops_an_installed_daemon_with_an_open_connection() {
    stop_daemon_stops_a_daemon_installed_as(&format!("{VERSION}-{}", run_dir::rev7(REVISION)), false);
}

/// The name the client installs under also carries the helper's and the manifest's hashes.
#[test]
fn stop_daemon_stops_a_daemon_of_a_content_keyed_installation() {
    stop_daemon_stops_a_daemon_installed_as(
        &format!(
            "{VERSION}-{}-0123456789abcdef-fedcba9876543210",
            run_dir::rev7(REVISION)
        ),
        false,
    );
}

#[test]
fn stop_daemon_accepts_verified_legacy_metadata_without_an_installation() {
    stop_daemon_stops_a_daemon_installed_as(&format!("{VERSION}-{}", run_dir::rev7(REVISION)), true);
}

/// The daemon runs from its installation and is keyed by it: `stop-daemon` takes that key.
fn stop_daemon_stops_a_daemon_installed_as(name: &str, legacy: bool) {
    let sandbox = Sandbox::new();
    let paths = run_dir::Paths::new(
        &run_dir::run_key_of(name),
        &sandbox.root.join("d"),
        &sandbox.root.join("s"),
        &std::env::temp_dir(),
    )
    .unwrap();
    let bin = paths.data.join(name).join("bin");
    private_fs::create_private_dir_all(&bin, &paths.data).unwrap();
    let installed = bin.join("eitri-server");
    daemon::test_exec::install_executable(std::path::Path::new(BINARY), &installed);
    let mut command = Command::new(&installed);
    sandbox.configure(&mut command);
    let mut process = Process(daemon::test_exec::spawn(command.arg("daemon")).unwrap());
    wait_for(|| {
        assert!(process.0.try_wait().unwrap().is_none());
        runtime().block_on(ensure::hello(&paths)).is_ok()
    });
    let _open = std::os::unix::net::UnixStream::connect(paths.socket().unwrap()).unwrap();
    let recorded: serde_json::Value =
        serde_json::from_slice(&std::fs::read(paths.metadata_run().join("info")).unwrap()).unwrap();
    assert_eq!(recorded["installation"], name, "the daemon records its installation");
    assert_eq!(recorded["version"], VERSION);
    let key = if legacy {
        let metadata = paths.state.join("run").join(VERSION);
        private_fs::create_private_dir_all(&metadata, &paths.state).unwrap();
        let mut old = recorded.clone();
        old.as_object_mut().unwrap().remove("installation");
        private_fs::write_private(&metadata.join("info"), &serde_json::to_vec(&old).unwrap()).unwrap();
        VERSION.to_owned()
    } else {
        paths.key.clone()
    };
    let encoded = eitri_server_proto::args_hex::encode_args(&[std::ffi::OsStr::new(&key)]);
    let output = sandbox
        .command()
        .args(["stop-daemon", "--args-hex", &encoded])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    wait_for(|| process.0.try_wait().unwrap().is_some());
    assert!(process.0.wait().unwrap().success());
}

#[test]
fn sigterm_stops_owned_sidecars_then_the_daemon() {
    if std::env::var_os("EITRI_TEST_OWNED_CHILD").is_some() {
        let paths = run_dir::Paths::from_env(VERSION).unwrap();
        let rt = runtime();
        rt.block_on(async {
            let child = tokio::process::Command::new("/bin/sleep")
                .arg("60")
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            private_fs::write_private(
                &paths.state.join("child-pid"),
                child.id().unwrap().to_string().as_bytes(),
            )
            .unwrap();
            daemon::run_with_sidecars(
                paths,
                Duration::from_secs(2),
                daemon::OwnedSidecars { children: vec![child] },
            )
            .await
            .unwrap();
        });
        return;
    }
    let sandbox = Sandbox::new();
    let paths = sandbox.paths();
    paths.prepare().unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    sandbox.configure(&mut command);
    command
        .args(["--exact", "sigterm_stops_owned_sidecars_then_the_daemon", "--nocapture"])
        .env("EITRI_TEST_OWNED_CHILD", "1");
    let mut process = Process(command.spawn().unwrap());
    wait_for(|| {
        assert!(process.0.try_wait().unwrap().is_none());
        runtime().block_on(ensure::hello(&paths)).is_ok()
    });
    let pid = private_fs::read_private_nofollow_capped(&paths.state.join("child-pid"), 64)
        .unwrap()
        .parse::<u32>()
        .unwrap();
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        process.0.try_wait().unwrap().is_none(),
        "owned child must prevent idle exit"
    );
    let connection = std::os::unix::net::UnixStream::connect(paths.socket().unwrap()).unwrap();
    unsafe { libc::kill(process.0.id() as i32, libc::SIGTERM) };
    wait_for(|| process.0.try_wait().unwrap().is_some());
    assert!(process.0.wait().unwrap().success());
    assert!(!run_dir::pid_alive(pid));
    let log = private_fs::read_private_nofollow_capped(&paths.state.join("audit.log"), 16384).unwrap();
    let events: Vec<_> = log
        .lines()
        .map(|line| line.split_whitespace().nth(1).unwrap())
        .collect();
    assert_eq!(
        events
            .into_iter()
            .filter(|e| *e != "login-env-fallback")
            .collect::<Vec<_>>(),
        ["daemon-start", "sidecar-start", "sidecar-stop", "daemon-stop"]
    );
    drop(connection);
}

#[test]
fn gc_keeps_the_running_version_and_recent_directories() {
    let sandbox = Sandbox::new();
    // No sockets are needed to exercise retention; keep filesystem paths independent of socket limits.
    let paths = run_dir::Paths {
        version: VERSION.into(),
        data: sandbox.root.join("d"),
        state: sandbox.root.join("s"),
        run: sandbox.root.join("r"),
        key: VERSION.into(),
        install: Some(
            sandbox
                .root
                .join("d")
                .join(format!("{VERSION}-{}", run_dir::rev7(REVISION))),
        ),
        installed: true,
    };
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10 * 24 * 3600);
    let current = installation(&paths);
    let recent = paths.data.join("recent-0123456");
    let old = paths.data.join("old-0123456");
    let live = paths.data.join("live-0123456");
    let unstamped = paths.data.join("unstamped-0123456");
    // The client's content-keyed names: the version is read past both hashes.
    let old_keyed = paths.data.join("old-0123456-0123456789abcdef-fedcba9876543210");
    let live_keyed = paths.data.join("live-0123456-0123456789abcdef-fedcba9876543210");
    // Kept by its own daemon, keyed by the installation's name and recording it.
    let owned_keyed = paths.data.join("owned-0123456-0123456789abcdef-fedcba9876543210");
    private_fs::create_private_dir_all(&unstamped, &paths.data).unwrap();
    for (path, stamp) in [
        (&current, "0"),
        (&recent, "864000000"),
        (&old, "0"),
        (&live, "0"),
        (&old_keyed, "0"),
        (&live_keyed, "0"),
        (&owned_keyed, "0"),
    ] {
        private_fs::create_private_dir_all(path, &paths.data).unwrap();
        private_fs::write_private(&path.join("last-used"), stamp.as_bytes()).unwrap();
    }
    let mut live_paths = paths.clone();
    live_paths.version = "live".into();
    live_paths.key = "live".into();
    live_paths.run = paths.state.join("run/live");
    write_info(&live_paths, std::process::id());
    let owned_name = "owned-0123456-0123456789abcdef-fedcba9876543210";
    let mut owned_paths = paths.clone();
    owned_paths.key = run_dir::run_key_of(owned_name);
    owned_paths.version = "owned".into();
    owned_paths.run = paths.state.join("run").join(&owned_paths.key);
    owned_paths.prepare().unwrap();
    private_fs::write_private(
        &owned_paths.metadata_run().join("info"),
        &serde_json::to_vec(&run_dir::Published {
            info: DaemonInfo {
                version: "owned".into(),
                rev: REVISION.to_owned(),
                pid: std::process::id(),
                protocol_major: 1,
                protocol_minor: 0,
                started_unix_ms: 0,
            },
            installation: Some(owned_name.into()),
        })
        .unwrap(),
    )
    .unwrap();
    gc::collect(&paths, now).unwrap();
    assert!(current.exists());
    assert!(recent.exists());
    assert!(!old_keyed.exists());
    assert!(live_keyed.exists());
    assert!(owned_keyed.exists());
    assert!(!old.exists());
    assert!(live.exists());
    assert!(unstamped.exists());
}

#[test]
fn ensure_waits_for_a_starting_or_stopping_lifetime_lock() {
    let sandbox = Sandbox::new();
    let paths = sandbox.paths();
    paths.prepare().unwrap();
    let live = private_fs::PrivateLock::open(&paths.metadata_run().join("live.lock")).unwrap();
    assert!(live.try_lock().unwrap());
    let mut child = Process(sandbox.command().arg("ensure").spawn().unwrap());
    let starter = private_fs::PrivateLock::open(&paths.metadata_run().join("lock")).unwrap();
    let deadline = Instant::now() + ensure::START_TIMEOUT;
    loop {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "ensure gave up while live.lock was held"
        );
        if !starter.try_lock().unwrap() {
            break;
        }
        // Release the probe's flock before the starter takes its own lock.
        unsafe {
            use std::os::fd::AsRawFd;
            libc::flock(starter.as_file().as_raw_fd(), libc::LOCK_UN);
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    // A stopping daemon can hold this lock through six seconds of TERM grace and the reap budget.
    std::thread::sleep(Duration::from_secs(8));
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "ensure abandoned a stopping daemon"
    );
    drop(live);
    while child.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "ensure exceeded its real startup budget");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(child.0.wait().unwrap().success());
    assert!(runtime().block_on(ensure::hello(&paths)).is_ok());
}
