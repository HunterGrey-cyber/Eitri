//! The real daemon and terminal host through LocalExec, with a private test shell and no SSH or display.
//! Build the fixture first: `cargo test --locked -p eitri-server --test terminal --no-run`.
#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent::remote_client::terminal_transport::wire;
use agent::remote_client::HelperClient;
use agent::ssh::{ChannelLauncher, LinkCheck, LocalExec};
use eitri_core::remote_project::terminal_facts::TerminalFactsHandle;
use eitri_core::remote_project::terminal_link::{TerminalLink, TerminalLinkOptions, TerminalTarget};
use eitri_core::remote_project::{RemoteConnection, RemoteProject, RemoteProjects, TerminalRoute};
use eitri_terminal::mirror::{LinkItem, LinkState, MirrorEnd, TermLink};
use eitri_terminal::{ExitInfo, PtySize, TerminalColors};

const HOST: &str = "example-host";
const KEY: &str = "SHA256:example-host-key";
const HANG: Duration = Duration::from_secs(60);

fn target_debug() -> PathBuf {
    if let Some(target) = std::env::var_os("CARGO_TARGET_DIR") {
        return PathBuf::from(target).join("debug");
    }
    std::env::current_exe()
        .unwrap()
        .ancestors()
        .find(|path| path.file_name().is_some_and(|name| name == "debug"))
        .unwrap()
        .to_path_buf()
}

fn fixture_binary() -> PathBuf {
    // Both Cargo's ordinary layout and the runner's per-package artifact layout are supported.
    let debug = target_debug();
    let mut candidates = Vec::new();
    fn visit(dir: &Path, depth: usize, candidates: &mut Vec<PathBuf>) {
        if depth == 0 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, depth - 1, candidates);
            } else if path
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.starts_with("terminal-") && !s.contains('.'))
            {
                candidates.push(path);
            }
        }
    }
    visit(&debug.join("build/eitri-server"), 4, &mut candidates);
    visit(&debug.join("deps"), 1, &mut candidates);
    candidates.sort_by_key(|path| std::fs::metadata(path).and_then(|m| m.modified()).ok());
    candidates
        .pop()
        .expect("build the daemon fixture: cargo test --locked -p eitri-server --test terminal --no-run")
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn script(path: &Path, body: &str) {
    assert!(Command::new("sh")
        .args([
            "-c",
            "umask 077; printf '%s' \"$1\" > \"$2\" && chmod 700 \"$2\"",
            "sh",
            body
        ])
        .arg(path)
        .status()
        .unwrap()
        .success());
}

struct Fixture {
    root: PathBuf,
    helper: PathBuf,
    daemon: Child,
    pins: Vec<OwnedFd>,
}

impl Fixture {
    fn new() -> Self {
        Self::with_shell("#!/bin/sh\nstty -echo\nexec /bin/cat\n")
    }

    fn with_shell(program: &str) -> Self {
        let root = PathBuf::from(format!(
            "/tmp/tl-{}-{}",
            std::process::id(),
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        agent::private_fs::create_private_dir_all(&root, &root).unwrap();
        std::fs::create_dir(root.join("work")).unwrap();
        let shell = root.join("shell");
        script(&shell, program);
        let helper = root.join("helper");
        let server = target_debug().join("eitri-server");
        assert!(server.is_file(), "build eitri-server first");
        script(
            &helper,
            &format!(
                "#!/bin/sh\nexport HOME={} XDG_DATA_HOME={} XDG_STATE_HOME={} TMPDIR={}\nexec {} \"$@\"\n",
                quote(&root),
                quote(&root.join("d")),
                quote(&root.join("s")),
                quote(&root),
                quote(&server)
            ),
        );
        let daemon = Command::new(fixture_binary())
            .args(["--exact", "fixture_daemon_entry", "--nocapture"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &root)
            .env("TMPDIR", &root)
            .env("XDG_DATA_HOME", root.join("d"))
            .env("XDG_STATE_HOME", root.join("s"))
            .env("EITRI_TEST_DAEMON", "1")
            .env("EITRI_TEST_IDLE", "3600")
            .env("EITRI_TEST_SHELL", shell)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(root.join("daemon.err")).unwrap())
            .spawn()
            .unwrap();
        let mut fixture = Self {
            root,
            helper,
            daemon,
            pins: Vec::new(),
        };
        let deadline = Instant::now() + HANG;
        let info = fixture
            .root
            .join("s/eitri-server/run")
            .join(env!("CARGO_PKG_VERSION"))
            .join("info");
        while !info.exists() {
            assert!(
                fixture.daemon.try_wait().unwrap().is_none(),
                "daemon exited: {}",
                std::fs::read_to_string(fixture.root.join("daemon.err")).unwrap()
            );
            assert!(Instant::now() < deadline, "daemon startup hung");
            std::thread::sleep(Duration::from_millis(10));
        }
        fixture
    }

    fn launcher(&self) -> Arc<Launcher> {
        Arc::new(Launcher {
            local: LocalExec {
                helper: self.helper.clone(),
            },
            children: Mutex::new(Vec::new()),
        })
    }

    fn client(&self) -> HelperClient {
        HelperClient::connect(self.launcher(), HOST, Some(KEY)).unwrap()
    }

    fn options(&self) -> TerminalLinkOptions {
        TerminalLinkOptions {
            installation: String::new(),
            target: TerminalTarget::Open {
                command_id: uuid::Uuid::new_v4().simple().to_string(),
                cwd: self.root.join("work"),
            },
            size: size(),
            colors: TerminalColors::default(),
        }
    }

    fn link(&self) -> TerminalLink {
        TerminalLink::new(self.client(), self.options(), TerminalFactsHandle::default()).unwrap()
    }

    fn attach(&self, id: String, boot: Option<String>) -> TerminalLink {
        let client = self.client();
        let hello = client.hello().unwrap();
        let mut options = self.options();
        options.target = TerminalTarget::Attach {
            terminal_id: id,
            daemon_instance: hello.daemon_instance,
            boot_id: boot.unwrap_or(hello.boot_id),
        };
        TerminalLink::new(client, options, TerminalFactsHandle::default()).unwrap()
    }

    fn pin_hosts(&mut self) {
        let run = self.root.join("s/eitri-server/run").join(env!("CARGO_PKG_VERSION"));
        let Ok(entries) = std::fs::read_dir(run) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|s| s == "pid")
                && path.file_name().unwrap().to_string_lossy().starts_with("th-")
            {
                let Ok(bytes) = std::fs::read(path) else {
                    continue;
                };
                let Ok(record) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                    continue;
                };
                let Some(pid) = record["pid"].as_i64() else {
                    continue;
                };
                let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
                if fd >= 0 {
                    let pin = unsafe { OwnedFd::from_raw_fd(fd) };
                    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default();
                    if record["start_time"].as_u64() == agent::child::start_time(pid as u32).ok()
                        && record["boot_id"].as_str() == Some(boot.trim())
                    {
                        self.pins.push(pin);
                    }
                }
            }
        }
    }

    fn close(&mut self, client: &HelperClient, id: &str) {
        self.pin_hosts();
        let (channel, runtime) = client.terminal_channel().unwrap();
        let id = id.to_owned();
        let result = runtime.block_on(async move {
            wire::terminal_client::TerminalClient::new(channel)
                .close_terminal(wire::CloseTerminalRequest {
                    terminal_id: id,
                    command_id: uuid::Uuid::new_v4().simple().to_string(),
                })
                .await
        });
        assert!(result.is_ok(), "{result:?}");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Pin only the processes the fixture's records own, before terminating this fixture's daemon.
        self.pin_hosts();
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        for pin in &self.pins {
            unsafe {
                libc::syscall(libc::SYS_pidfd_send_signal, pin.as_raw_fd(), libc::SIGKILL, 0, 0);
            }
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Launcher {
    local: LocalExec,
    children: Mutex<Vec<OwnedFd>>,
}
impl Launcher {
    fn pause(&self) {
        for pin in self.children.lock().unwrap().iter() {
            assert_eq!(
                unsafe { libc::syscall(libc::SYS_pidfd_send_signal, pin.as_raw_fd(), libc::SIGSTOP, 0, 0) },
                0
            );
            let deadline = Instant::now() + HANG;
            loop {
                let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
                assert_eq!(
                    unsafe {
                        libc::waitid(
                            libc::P_PIDFD,
                            pin.as_raw_fd() as u32,
                            &mut info,
                            libc::WSTOPPED | libc::WNOWAIT | libc::WNOHANG,
                        )
                    },
                    0
                );
                if unsafe { info.si_pid() } != 0 {
                    break;
                }
                assert!(Instant::now() < deadline, "channel did not stop");
                std::thread::yield_now();
            }
        }
    }

    fn cut(&self) {
        for pin in self.children.lock().unwrap().iter() {
            unsafe {
                libc::syscall(libc::SYS_pidfd_send_signal, pin.as_raw_fd(), libc::SIGKILL, 0, 0);
            }
        }
    }
}
impl ChannelLauncher for Launcher {
    fn spawn(&self, tail: &[OsString]) -> io::Result<Child> {
        let child = self.local.spawn(tail)?;
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) } as i32;
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        self.children.lock().unwrap().push(unsafe { OwnedFd::from_raw_fd(fd) });
        Ok(child)
    }
    fn host_key(&self) -> Option<io::Result<String>> {
        Some(Ok(KEY.into()))
    }
    fn check_link(&self) -> LinkCheck {
        LinkCheck::NoMaster
    }
}

fn size() -> PtySize {
    PtySize {
        cols: 80,
        rows: 24,
        cell_width_px: 8,
        cell_height_px: 16,
    }
}

fn wait(link: &mut dyn TermLink, mut predicate: impl FnMut(&LinkItem) -> bool) -> Vec<LinkItem> {
    let deadline = Instant::now() + HANG;
    let mut items = Vec::new();
    loop {
        let next = link.take();
        let done = next.iter().any(&mut predicate);
        items.extend(next);
        if done {
            return items;
        }
        assert!(Instant::now() < deadline, "link hung: {items:?}");
        let mut fd = libc::pollfd {
            fd: link.readiness().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert!(unsafe { libc::poll(&mut fd, 1, 100) } >= 0);
    }
}

fn attached(link: &mut dyn TermLink) -> Vec<LinkItem> {
    wait(link, |item| matches!(item, LinkItem::Snapshot(_)))
}

#[test]
fn open_attach_and_echo() {
    let fixture = Fixture::new();
    let mut link = fixture.link();
    assert_eq!(
        link.write_input(b"early").unwrap_err().kind(),
        io::ErrorKind::NotConnected
    );
    let items = attached(&mut link);
    let live = items
        .iter()
        .position(|i| {
            matches!(
                i,
                LinkItem::Link {
                    state: LinkState::Live,
                    ..
                }
            )
        })
        .unwrap();
    let snapshot = items.iter().position(|i| matches!(i, LinkItem::Snapshot(_))).unwrap();
    assert!(live < snapshot);
    assert_eq!(link.write_input(b"echo-marker\n").unwrap(), 12);
    let mut output = Vec::new();
    wait(&mut link, |item| {
        if let LinkItem::Output { bytes, .. } = item {
            output.extend_from_slice(bytes);
        }
        output.windows(b"echo-marker".len()).any(|w| w == b"echo-marker")
    });
}

#[test]
fn a_broken_channel_reconnects_and_resumes_with_a_snapshot() {
    let fixture = Fixture::new();
    let launcher = fixture.launcher();
    let client = HelperClient::connect(launcher.clone(), HOST, Some(KEY)).unwrap();
    let mut link = TerminalLink::new(client, fixture.options(), TerminalFactsHandle::default()).unwrap();
    attached(&mut link);
    launcher.cut();
    let items = attached(&mut link);
    assert!(items.iter().any(|i| matches!(
        i,
        LinkItem::Link {
            state: LinkState::Disconnected { .. },
            ..
        }
    )));
    let live = items
        .iter()
        .rposition(|i| {
            matches!(
                i,
                LinkItem::Link {
                    state: LinkState::Live,
                    ..
                }
            )
        })
        .unwrap();
    let snapshot = items.iter().position(|i| matches!(i, LinkItem::Snapshot(_))).unwrap();
    assert!(live < snapshot);
    assert_eq!(link.write_input(b"after\n").unwrap(), 6);
}

#[test]
fn input_while_disconnected_is_refused() {
    let fixture = Fixture::new();
    let launcher = fixture.launcher();
    let mut link = TerminalLink::new(
        HelperClient::connect(launcher.clone(), HOST, Some(KEY)).unwrap(),
        fixture.options(),
        TerminalFactsHandle::default(),
    )
    .unwrap();
    attached(&mut link);
    launcher.cut();
    wait(&mut link, |i| {
        matches!(
            i,
            LinkItem::Link {
                state: LinkState::Disconnected { .. },
                ..
            }
        )
    });
    assert_eq!(
        link.write_input(b"stale").unwrap_err().kind(),
        io::ErrorKind::NotConnected
    );
}

#[test]
fn a_closed_terminal_reattached_reports_its_saved_exit() {
    let mut fixture = Fixture::new();
    let mut original = fixture.link();
    attached(&mut original);
    let id = original.terminal_id().unwrap();
    fixture.close(&fixture.client(), &id);
    wait(&mut original, |i| matches!(i, LinkItem::Ended(_)));
    let mut link = fixture.attach(id, None);
    // The host has gone; its saved exit record comes through as the exit it was (a close hangs the shell up), never
    // reworded as a close the link cannot tell from an exit of the program's own.
    let items = wait(&mut link, |i| matches!(i, LinkItem::Exited(_) | LinkItem::Ended(_)));
    assert!(
        items
            .iter()
            .any(|i| matches!(i, LinkItem::Exited(ExitInfo { detached: false, .. }))),
        "{items:?}"
    );
    assert!(!items.iter().any(|i| matches!(i, LinkItem::Ended(_))));
}

#[test]
fn a_server_reboot_is_named() {
    let fixture = Fixture::new();
    let mut link = fixture.attach("a".repeat(32), Some("previous-boot".into()));
    let items = wait(&mut link, |i| matches!(i, LinkItem::Ended(_)));
    assert!(items
        .iter()
        .any(|i| matches!(i, LinkItem::Ended(MirrorEnd::Gone(why)) if why == "the server example-host restarted")));
}

#[test]
fn replaced_ends_the_mirror() {
    let fixture = Fixture::new();
    let mut first = fixture.link();
    attached(&mut first);
    let mut second = fixture.attach(first.terminal_id().unwrap(), None);
    attached(&mut second);
    let items = wait(&mut first, |i| matches!(i, LinkItem::Ended(_)));
    assert!(items.iter().any(|i| matches!(i, LinkItem::Ended(MirrorEnd::Replaced))));
}

#[test]
fn closing_ends_the_mirror() {
    let mut fixture = Fixture::new();
    let mut link = fixture.link();
    attached(&mut link);
    fixture.close(&fixture.client(), &link.terminal_id().unwrap());
    let items = wait(&mut link, |i| matches!(i, LinkItem::Ended(_)));
    assert!(items.iter().any(|i| matches!(i, LinkItem::Ended(MirrorEnd::Closed))));
}

#[test]
fn a_connection_without_a_pinned_key_refuses() {
    let fixture = Fixture::new();
    let client = HelperClient::connect(fixture.launcher(), HOST, None).unwrap();
    assert!(client.terminal_channel().is_err());
    assert!(client.terminal_connection("").is_err());
    assert!(TerminalLink::new(client, fixture.options(), TerminalFactsHandle::default()).is_err());
}

#[test]
fn terminals_use_their_own_connection() {
    let fixture = Fixture::new();
    let launcher = fixture.launcher();
    let client = HelperClient::connect(launcher.clone(), HOST, Some(KEY)).unwrap();
    let project = RemoteProject {
        version: 1,
        host: HOST.into(),
        host_key_sha256: KEY.into(),
        root: fixture.root.join("work"),
        helper_version: "fixture".into(),
        sync: None,
        created_at: 0,
        queued_stops: Vec::new(),
    };
    let connection = RemoteConnection::with_client(client.clone(), project, String::new(), String::new());
    let agents_channels = launcher.children.lock().unwrap().len();
    let mut link = connection
        .terminal_link(fixture.options(), TerminalFactsHandle::default())
        .unwrap();
    attached(&mut link);
    assert_eq!(launcher.children.lock().unwrap().len(), agents_channels + 1);
    let mut second = connection
        .terminal_link(fixture.options(), TerminalFactsHandle::default())
        .unwrap();
    attached(&mut second);
    assert_eq!(launcher.children.lock().unwrap().len(), agents_channels + 1);
    drop(link);
    drop(second);
    let mut third = connection
        .terminal_link(fixture.options(), TerminalFactsHandle::default())
        .unwrap();
    attached(&mut third);
    assert_eq!(launcher.children.lock().unwrap().len(), agents_channels + 2);
    assert!(
        client.hello().is_ok(),
        "the agent connection survives dropping the terminal link"
    );
}

/// A connection with a terminal route sends every terminal channel through it, a reconnect's included, and none
/// through the agents' launcher; the route is asked only when a terminal connection opens, and a route that refuses
/// (the terminals' master showed another key) refuses the terminal.
#[test]
fn terminals_go_through_their_route_and_reconnect_there() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Route {
        launcher: Arc<Launcher>,
        asked: AtomicUsize,
    }
    impl TerminalRoute for Route {
        fn launcher(&self) -> io::Result<Arc<dyn ChannelLauncher>> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            Ok(self.launcher.clone())
        }
    }
    struct Refusing;
    impl TerminalRoute for Refusing {
        fn launcher(&self) -> io::Result<Arc<dyn ChannelLauncher>> {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "another host key"))
        }
    }
    let fixture = Fixture::new();
    let agents = fixture.launcher();
    let client = HelperClient::connect(agents.clone(), HOST, Some(KEY)).unwrap();
    let project = RemoteProject {
        version: 1,
        host: HOST.into(),
        host_key_sha256: KEY.into(),
        root: fixture.root.join("work"),
        helper_version: "fixture".into(),
        sync: None,
        created_at: 0,
        queued_stops: Vec::new(),
    };
    let route = Arc::new(Route {
        launcher: fixture.launcher(),
        asked: AtomicUsize::new(0),
    });
    let connection = RemoteConnection::with_client(client.clone(), project.clone(), String::new(), String::new())
        .with_terminal_route(route.clone());
    let agents_channels = agents.children.lock().unwrap().len();
    assert_eq!(
        route.asked.load(Ordering::SeqCst),
        0,
        "nothing is started before a terminal"
    );
    let mut link = connection
        .terminal_link(fixture.options(), TerminalFactsHandle::default())
        .unwrap();
    attached(&mut link);
    assert_eq!(route.asked.load(Ordering::SeqCst), 1);
    assert_eq!(route.launcher.children.lock().unwrap().len(), 1);
    route.launcher.cut();
    let items = attached(&mut link);
    assert!(items.iter().any(|i| matches!(
        i,
        LinkItem::Link {
            state: LinkState::Disconnected { .. },
            ..
        }
    )));
    assert!(
        route.launcher.children.lock().unwrap().len() >= 2,
        "the reconnect went through the route"
    );
    assert_eq!(
        route.asked.load(Ordering::SeqCst),
        1,
        "a reconnect reuses the connection's launcher"
    );
    assert_eq!(agents.children.lock().unwrap().len(), agents_channels);

    let refused = RemoteConnection::with_client(client.clone(), project, String::new(), String::new())
        .with_terminal_route(Arc::new(Refusing));
    let error = refused
        .terminal_link(fixture.options(), TerminalFactsHandle::default())
        .err()
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(agents.children.lock().unwrap().len(), agents_channels);
    assert!(client.hello().is_ok(), "the agents' connection is untouched");
}

#[test]
fn facts_refresh_and_go_stale() {
    let fixture = Fixture::new();
    let facts = TerminalFactsHandle::default();
    let mut link = TerminalLink::new(fixture.client(), fixture.options(), facts.clone()).unwrap();
    attached(&mut link);
    let id = link.terminal_id().unwrap();
    facts.request_refresh(id.clone());
    let deadline = Instant::now() + HANG;
    while facts.get(&id).is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut saved = facts.get(&id).unwrap();
    assert_eq!(saved.cwd, Some(fixture.root.join("work")));
    saved.at = Instant::now() - Duration::from_secs(6);
    facts.set(id.clone(), saved);
    assert_eq!(facts.get(&id).unwrap().cwd, None);
}

#[test]
fn a_killed_host_is_named() {
    let mut fixture = Fixture::new();
    let mut link = fixture.link();
    attached(&mut link);
    fixture.pin_hosts();
    assert!(!fixture.pins.is_empty());
    for pin in &fixture.pins {
        unsafe {
            libc::syscall(libc::SYS_pidfd_send_signal, pin.as_raw_fd(), libc::SIGKILL, 0, 0);
        }
    }
    let items = wait(&mut link, |i| matches!(i, LinkItem::Ended(_)));
    assert!(items.iter().any(|i| matches!(i, LinkItem::Ended(MirrorEnd::Gone(why)) if why == "the terminal's process on example-host ended (killed, or out of memory); programs that ignore hangups may still run there")));
}

#[test]
fn stopped_ends_the_mirror() {
    let fixture = Fixture::new();
    let mut link = fixture.link();
    attached(&mut link);
    assert_eq!(unsafe { libc::kill(fixture.daemon.id() as i32, libc::SIGTERM) }, 0);
    let items = wait(&mut link, |i| matches!(i, LinkItem::Ended(_)));
    assert!(items.iter().any(|i| matches!(i, LinkItem::Ended(MirrorEnd::Stopped))));
}

struct Routed {
    current: LocalExec,
    older: LocalExec,
    routes: Mutex<Vec<String>>,
}
impl ChannelLauncher for Routed {
    fn spawn(&self, tail: &[OsString]) -> io::Result<Child> {
        let args = agent::ssh::decode_args(tail[2].to_str().unwrap()).map_err(io::Error::other)?;
        let installation = args.get(1).and_then(|s| s.to_str()).unwrap_or("");
        self.routes.lock().unwrap().push(installation.to_owned());
        let launcher = if installation == "older-installation" {
            &self.older
        } else {
            &self.current
        };
        launcher.spawn(&[tail[0].clone(), "--args-hex".into(), OsString::new()])
    }
    fn host_key(&self) -> Option<io::Result<String>> {
        Some(Ok(KEY.into()))
    }
    fn check_link(&self) -> LinkCheck {
        LinkCheck::Alive
    }
}

#[test]
fn the_older_installation_routes_its_terminal() {
    let current = Fixture::new();
    let older = Fixture::new();
    let mut original = older.link();
    attached(&mut original);
    let id = original.terminal_id().unwrap();
    let hello = older.client().hello().unwrap();
    let router = Arc::new(Routed {
        current: LocalExec {
            helper: current.helper.clone(),
        },
        older: LocalExec {
            helper: older.helper.clone(),
        },
        routes: Mutex::new(Vec::new()),
    });
    let client = HelperClient::connect(router.clone(), HOST, Some(KEY)).unwrap();
    let project = RemoteProject {
        version: 1,
        host: HOST.into(),
        host_key_sha256: KEY.into(),
        root: current.root.join("work"),
        helper_version: "fixture".into(),
        sync: None,
        created_at: 0,
        queued_stops: Vec::new(),
    };
    let connection = RemoteConnection::with_client(client, project, String::new(), String::new());
    let mut options = older.options();
    options.installation = "older-installation".into();
    options.target = TerminalTarget::Attach {
        terminal_id: id.clone(),
        daemon_instance: hello.daemon_instance,
        boot_id: hello.boot_id,
    };
    let mut link = connection
        .terminal_link(options, TerminalFactsHandle::default())
        .unwrap();
    attached(&mut link);
    assert_eq!(link.terminal_id(), Some(id));
    assert_eq!(*router.routes.lock().unwrap(), ["", "older-installation"]);
    let items = wait(&mut original, |i| matches!(i, LinkItem::Ended(_)));
    assert!(items.iter().any(|i| matches!(i, LinkItem::Ended(MirrorEnd::Replaced))));
}

fn wait_file(path: &Path) -> Vec<u8> {
    let deadline = Instant::now() + HANG;
    loop {
        if let Ok(bytes) = std::fs::read(path) {
            if !bytes.is_empty() {
                return bytes;
            }
        }
        assert!(Instant::now() < deadline, "program did not write {}", path.display());
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn lost_input_is_counted_in_bytes() {
    let mut fixture = Fixture::with_shell(
        r#"#!/usr/bin/python3
import os, signal, tty
tty.setraw(0)
open('../raw', 'w').write('ready')
remaining = 65536
while remaining:
    data = os.read(0, remaining)
    assert data and data == b'a' * len(data)
    remaining -= len(data)
open('../stopped', 'w').write(str(os.getpid()))
os.kill(os.getpid(), signal.SIGSTOP)
received = b''
while not received.endswith(b'after\n'):
    received += os.read(0, 65536)
open('../received', 'wb').write(received)
signal.pause()
"#,
    );
    let launcher = fixture.launcher();
    let mut link = TerminalLink::new(
        HelperClient::connect(launcher.clone(), HOST, Some(KEY)).unwrap(),
        fixture.options(),
        TerminalFactsHandle::default(),
    )
    .unwrap();
    attached(&mut link);
    wait_file(&fixture.root.join("raw"));
    assert_eq!(link.write_input(&vec![b'a'; 64 * 1024]).unwrap(), 64 * 1024);
    wait(&mut link, |i| matches!(i, LinkItem::InputWritten(65_536)));
    let pid: i32 = String::from_utf8(wait_file(&fixture.root.join("stopped")))
        .unwrap()
        .parse()
        .unwrap();
    let pin = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    assert!(pin >= 0);
    fixture.pins.push(unsafe { OwnedFd::from_raw_fd(pin) });
    // Hold the byte pipe before queuing the unwritten half, so exactly the acknowledged half reached the PTY.
    launcher.pause();
    assert_eq!(link.write_input(&vec![b'x'; 64 * 1024]).unwrap(), 64 * 1024);
    launcher.cut();
    let items = attached(&mut link);
    assert_eq!(
        items
            .iter()
            .filter_map(|i| match i {
                LinkItem::Link { lost_input_bytes, .. } if *lost_input_bytes > 0 => Some(*lost_input_bytes),
                _ => None,
            })
            .collect::<Vec<_>>(),
        [65_536]
    );
    assert!(items
        .iter()
        .any(|i| matches!(i, LinkItem::Snapshot(s) if s.input_written == 65_536)));
    assert_eq!(
        unsafe { libc::syscall(libc::SYS_pidfd_send_signal, pin, libc::SIGCONT, 0, 0) },
        0
    );
    assert_eq!(link.write_input(b"after\n").unwrap(), 6);
    // A wrong frame offset is refused by the real host and would never receive this acknowledgement.
    wait(&mut link, |i| matches!(i, LinkItem::InputWritten(65_542)));
    assert_eq!(wait_file(&fixture.root.join("received")), b"after\n");
}

#[test]
fn unread_state_changes_refuse_input() {
    let fixture = Fixture::new();
    let launcher = fixture.launcher();
    let mut link = TerminalLink::new(
        HelperClient::connect(launcher.clone(), HOST, Some(KEY)).unwrap(),
        fixture.options(),
        TerminalFactsHandle::default(),
    )
    .unwrap();
    attached(&mut link);
    launcher.cut();
    let mut fd = libc::pollfd {
        fd: link.readiness().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut fd, 1, HANG.as_millis() as i32) }, 1);
    assert_eq!(
        link.write_input(b"stale").unwrap_err().kind(),
        io::ErrorKind::NotConnected
    );
    let items = attached(&mut link);
    assert!(items.iter().any(|i| matches!(
        i,
        LinkItem::Link {
            state: LinkState::Disconnected { .. },
            ..
        }
    )));
    assert_eq!(link.write_input(b"current\n").unwrap(), 8);
}

#[test]
fn one_links_end_preserves_another_connections_facts_and_requests() {
    let fixture = Fixture::new();
    let facts = TerminalFactsHandle::default();
    let mut first = TerminalLink::new(fixture.client(), fixture.options(), facts.clone()).unwrap();
    let mut second = TerminalLink::new(fixture.client(), fixture.options(), facts.clone()).unwrap();
    attached(&mut first);
    attached(&mut second);
    let a = first.terminal_id().unwrap();
    let b = second.terminal_id().unwrap();
    let deadline = Instant::now() + HANG;
    while facts.get(&a).is_none() || facts.get(&b).is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(first);
    assert!(facts.get(&a).is_none());
    assert!(facts.get(&b).is_some());
    facts.request_refresh(b.clone());
    let at = facts.get(&b).unwrap().at;
    while facts.get(&b).unwrap().at == at {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_non_utf8_directory_ends_with_the_links_own_reason() {
    use std::os::unix::ffi::OsStringExt;
    let fixture = Fixture::new();
    let mut options = fixture.options();
    options.target = TerminalTarget::Open {
        command_id: "non-utf8".into(),
        cwd: PathBuf::from(OsString::from_vec(b"/bad-\xff".to_vec())),
    };
    let mut link = TerminalLink::new(fixture.client(), options, TerminalFactsHandle::default()).unwrap();
    assert!(wait(&mut link, |i| matches!(i, LinkItem::Ended(_)))
        .iter()
        .any(|i| matches!(i, LinkItem::Ended(MirrorEnd::Gone(why)) if why == "the terminal directory is not UTF-8")));
}

#[test]
fn a_connect_wait_does_not_hold_the_terminal_client_cache_lock() {
    struct Gated {
        launcher: Arc<Launcher>,
        gate: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
    }
    impl ChannelLauncher for Gated {
        fn spawn(&self, tail: &[OsString]) -> io::Result<Child> {
            let args = agent::ssh::decode_args(tail[2].to_str().unwrap()).map_err(io::Error::other)?;
            if args.get(1).is_some_and(|arg| arg == "waiting-installation") {
                let (started, release) = self.gate.lock().unwrap().take().unwrap();
                started.send(()).unwrap();
                release.recv_timeout(HANG).unwrap();
            }
            self.launcher
                .spawn(&[tail[0].clone(), "--args-hex".into(), OsString::new()])
        }
        fn host_key(&self) -> Option<io::Result<String>> {
            Some(Ok(KEY.into()))
        }
    }
    let fixture = Fixture::new();
    let (started, started_rx) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let launcher = Arc::new(Gated {
        launcher: fixture.launcher(),
        gate: Mutex::new(Some((started, release_rx))),
    });
    let client = HelperClient::connect(launcher, HOST, Some(KEY)).unwrap();
    let connection = RemoteConnection::with_client(
        client,
        RemoteProject {
            version: 1,
            host: HOST.into(),
            host_key_sha256: KEY.into(),
            root: fixture.root.join("work"),
            helper_version: "fixture".into(),
            sync: None,
            created_at: 0,
            queued_stops: Vec::new(),
        },
        String::new(),
        String::new(),
    );
    let mut first = connection
        .terminal_link(fixture.options(), TerminalFactsHandle::default())
        .unwrap();
    attached(&mut first);
    let mut options = fixture.options();
    options.installation = "waiting-installation".into();
    let waiting = connection.clone();
    let blocked = std::thread::spawn(move || waiting.terminal_link(options, TerminalFactsHandle::default()));
    started_rx.recv_timeout(HANG).unwrap();
    let cached = connection.clone();
    let options = fixture.options();
    let (finished, finished_rx) = std::sync::mpsc::channel();
    let other = std::thread::spawn(move || {
        let result = cached.terminal_link(options, TerminalFactsHandle::default());
        let _ = finished.send(result);
    });
    let result = finished_rx.recv_timeout(HANG);
    // Release and join the blocked connect even if the other call hit the hang guard.
    release.send(()).unwrap();
    let _waiting_link = blocked.join().unwrap().unwrap();
    other.join().unwrap();
    let mut second = result
        .expect("the cached installation waited behind another installation's connect")
        .unwrap();
    attached(&mut second);
}

#[test]
fn opened_returns_the_open_answer_and_the_attached_terminal_identity() {
    let fixture = Fixture::new();
    let client = fixture.client();
    let hello = client.hello().unwrap();
    let options = fixture.options();
    let command = match &options.target {
        TerminalTarget::Open { command_id, .. } => command_id.clone(),
        _ => unreachable!(),
    };
    let mut link = TerminalLink::new(client, options, TerminalFactsHandle::default()).unwrap();
    attached(&mut link);
    let opened = link.opened().expect("the open answered before its snapshot");
    assert_eq!(Some(opened.0.clone()), link.terminal_id());
    assert_eq!(opened.1, hello.daemon_instance);
    assert_eq!(opened.2, hello.boot_id);
    let query = fixture.client();
    let (channel, runtime) = query.terminal_channel().unwrap();
    let answer = runtime.block_on(async move {
        wire::terminal_client::TerminalClient::new(channel)
            .open_terminal(wire::OpenTerminalRequest {
                command_id: command,
                cwd: String::new(),
                size: None,
                colors: None,
            })
            .await
            .unwrap()
            .into_inner()
    });
    assert_eq!(opened, (answer.terminal_id, answer.daemon_instance, hello.boot_id));
    drop(link);
    let mut attached_link = fixture.attach(opened.0.clone(), None);
    attached(&mut attached_link);
    assert_eq!(attached_link.opened(), Some(opened));
}

#[test]
fn an_answer_handle_observes_the_open_after_the_link_moves_into_a_box() {
    let fixture = Fixture::new();
    let client = fixture.client();
    let hello = client.hello().unwrap();
    let link = TerminalLink::new(client, fixture.options(), TerminalFactsHandle::default()).unwrap();
    let answer = link.answer_handle();
    let copy = answer.clone();
    let mut mirror_link: Box<dyn TermLink> = Box::new(link);
    let opened = answer
        .wait(Instant::now() + HANG)
        .expect("the boxed link's open answer");
    assert_eq!(opened.1, hello.daemon_instance);
    assert_eq!(opened.2, hello.boot_id);
    assert_eq!(copy.get(), Some(opened.clone()));
    attached(mirror_link.as_mut());
    drop(mirror_link);
    assert_eq!(answer.get(), Some(opened));
}

#[test]
fn an_open_intent_records_the_connections_typed_host_and_no_generation() {
    let fixture = Fixture::new();
    let root = fixture.root.join("work");
    let state = fixture.root.join("laptop");
    let projects = RemoteProjects::new(Some(state.as_os_str()), None);
    let client = fixture.client();
    let connection = RemoteConnection::with_client(
        client,
        RemoteProject {
            version: 1,
            host: "user@example-host:2222".into(),
            host_key_sha256: KEY.into(),
            root: root.clone(),
            helper_version: env!("CARGO_PKG_VERSION").into(),
            sync: None,
            created_at: 0,
            queued_stops: Vec::new(),
        },
        String::new(),
        String::new(),
    );
    let intent = projects
        .terminals()
        .begin_open(&connection, &root, 1, "terminal", env!("CARGO_PKG_VERSION"), 42)
        .unwrap();
    let record = projects.terminals().record(KEY, &root).unwrap();
    assert_eq!(record.host.as_deref(), Some("user@example-host:2222"));
    // No helper round trip at intent time: the open's answer supplies the generation later.
    assert_eq!(
        (intent.daemon_instance.as_deref(), intent.boot_id.as_deref()),
        (None, None)
    );
    assert_eq!(record.entries, [intent]);
}

#[test]
fn a_close_before_the_open_answers_retries_until_it_can_stop_the_shell() {
    use eitri_core::remote_project::terminals::TerminalState;
    let fixture = Fixture::new();
    let root = fixture.root.join("work");
    let state = fixture.root.join("laptop");
    let projects = RemoteProjects::new(Some(state.as_os_str()), None);
    let mut connection = RemoteConnection::with_client(
        fixture.client(),
        RemoteProject {
            version: 1,
            host: HOST.into(),
            host_key_sha256: KEY.into(),
            root: root.clone(),
            helper_version: env!("CARGO_PKG_VERSION").into(),
            sync: None,
            created_at: 0,
            queued_stops: Vec::new(),
        },
        String::new(),
        String::new(),
    );
    let records = projects.terminals();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let intent = records
        .begin_open(&connection, &root, 1, "terminal", env!("CARGO_PKG_VERSION"), now)
        .unwrap();
    // The request is still on its way: inventory has no match, but the open can yet arrive.
    assert!(records
        .close(KEY, &root, &intent.command_id, true, Some(&mut connection))
        .is_err());
    assert_eq!(
        records.record(KEY, &root).unwrap().entries[0].state,
        TerminalState::StopQueued
    );
    let mut options = fixture.options();
    options.target = intent.target(&root).unwrap();
    let mut link = TerminalLink::new(fixture.client(), options, TerminalFactsHandle::default()).unwrap();
    attached(&mut link);
    assert!(records.send_queued(KEY, &root, &mut connection).unwrap().is_empty());
    assert!(records.record(KEY, &root).unwrap().entries.is_empty());
    assert!(wait(&mut link, |i| matches!(i, LinkItem::Ended(_)))
        .iter()
        .any(|i| matches!(i, LinkItem::Ended(MirrorEnd::Closed))));
}
