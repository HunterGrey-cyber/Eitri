//! `eitri host exec` against this build's daemon through `LocalExec`: no ssh, no display, and a sync engine that only
//! records its flushes. The daemon is the fixture in `server/tests/exec_command.rs`; build it first:
//! `cargo test -p eitri-server --test exec_command --no-run` (which also builds `eitri-server`).
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent::remote_client::HelperClient;
use agent::ssh::{ChannelLauncher, ConnectError, LocalExec};
use eitri_core::remote_host::cli::{Agent, Contact, Context, Reach, Terminal};
use eitri_core::remote_host::cli_exec::{run_exec_in, ProcessIo};
use eitri_core::remote_host::cli_sync::SyncSettings;
use eitri_core::remote_host::install::InstallPlan;
use eitri_core::remote_project::{RemoteConnection, RemoteProject, RemoteProjects, SyncRecord};
use eitri_core::remote_sync::engine::{
    Conflict, EntryKind, FlushOutcome, Labels, Role, SessionSpec, SessionStatus, SyncEngine,
};
use eitri_core::remote_sync::SyncError;

const HOST: &str = "example-host";
const KEY: &str = "SHA256:example-host-key";
const TREE: &str = "eitri-tree-example";
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

/// The newest `exec_command` test binary: Cargo's ordinary layout and the runner's per-package layout both.
fn fixture_binary() -> PathBuf {
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
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("exec_command-") && !name.contains('.'))
            {
                candidates.push(path);
            }
        }
    }
    let debug = target_debug();
    let mut candidates = Vec::new();
    visit(&debug.join("build/eitri-server"), 4, &mut candidates);
    visit(&debug.join("deps"), 1, &mut candidates);
    candidates.sort_by_key(|path| std::fs::metadata(path).and_then(|meta| meta.modified()).ok());
    candidates
        .pop()
        .expect("build the daemon fixture: cargo test -p eitri-server --test exec_command --no-run")
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

/// This build's daemon in a private directory, the helper script `LocalExec` runs, the project's mirror (`work`)
/// recorded as a remote project at the same path, and a sync engine that records its flushes.
struct Fixture {
    root: PathBuf,
    helper: PathBuf,
    daemon: Child,
    store: RemoteProjects,
    engine: Arc<RecordingEngine>,
    /// Makes the next connection fail, as an ssh master that cannot be opened does.
    unreachable: AtomicBool,
}

impl Fixture {
    fn new() -> Self {
        // Short, so the daemon's socket path stays under the Unix limit.
        let root = PathBuf::from(format!(
            "/tmp/rx-{}-{}",
            std::process::id(),
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        agent::private_fs::create_private_dir_all(&root, &root).unwrap();
        std::fs::create_dir_all(root.join("work/src")).unwrap();
        std::fs::create_dir(root.join("outside")).unwrap();
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
        let mut daemon = Command::new(fixture_binary())
            .args(["--exact", "fixture_daemon_entry", "--nocapture"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &root)
            .env("TMPDIR", &root)
            .env("XDG_DATA_HOME", root.join("d"))
            .env("XDG_STATE_HOME", root.join("s"))
            .env("EITRI_TEST_DAEMON", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(root.join("daemon.err")).unwrap())
            .spawn()
            .unwrap();
        let info = root
            .join("s/eitri-server/run")
            .join(env!("CARGO_PKG_VERSION"))
            .join("info");
        let deadline = Instant::now() + HANG;
        while !info.exists() {
            assert!(
                daemon.try_wait().unwrap().is_none(),
                "daemon exited: {}",
                std::fs::read_to_string(root.join("daemon.err")).unwrap_or_default()
            );
            assert!(Instant::now() < deadline, "daemon startup hung");
            std::thread::sleep(Duration::from_millis(10));
        }
        let state = root.join("state");
        let store = RemoteProjects::new(Some(state.as_os_str()), None);
        let mut project = RemoteProject {
            version: 1,
            host: HOST.into(),
            host_key_sha256: KEY.into(),
            root: root.join("work"),
            helper_version: "0.4.0-example".into(),
            sync: None,
            created_at: 0,
            queued_stops: vec![],
        };
        project.set_sync_record(&SyncRecord {
            set_up: true,
            engine: "mutagen".into(),
            host_key_sha256: KEY.into(),
            tree_session: Some(TREE.into()),
            ..Default::default()
        });
        store.save(&project).unwrap();
        Self {
            engine: Arc::new(RecordingEngine::new(root.join("marker"))),
            root,
            helper,
            daemon,
            store,
            unreachable: AtomicBool::new(false),
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn connection(&self, project: &RemoteProject) -> RemoteConnection {
        let launcher: Arc<dyn ChannelLauncher> = Arc::new(LocalExec {
            helper: self.helper.clone(),
        });
        let client = HelperClient::connect(launcher, HOST, Some(KEY)).unwrap();
        RemoteConnection::with_client(client, project.clone(), String::new(), String::new())
    }

    /// Runs `eitri host exec <args>` from `cwd` and returns its exit code, stdout and stderr.
    fn exec(&self, cwd: &Path, args: &[&str], stdin: Box<dyn Read + Send>, terminal: bool) -> (i32, Vec<u8>, String) {
        let mut reach = FixtureReach {
            fixture: self,
            connected: false,
        };
        let mut term = Lines::default();
        let enabled = || true;
        let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let code = run_exec_in(
            &args,
            &mut Context {
                store: &self.store,
                reach: &mut reach,
                term: &mut term,
                remote_dev_enabled: &enabled,
                now: 0,
            },
            cwd,
            ProcessIo {
                stdin,
                stdin_is_terminal: terminal,
                stdout: &mut stdout,
                stderr: &mut stderr,
            },
        );
        assert!(
            term.said.is_empty(),
            "exec writes nothing to the terminal's stdout: {:?}",
            term.said
        );
        (code, stdout, String::from_utf8(stderr).unwrap())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct FixtureReach<'a> {
    fixture: &'a Fixture,
    connected: bool,
}

impl Reach for FixtureReach<'_> {
    fn connect(&mut self, _host: &str) -> Result<String, ConnectError> {
        unreachable!("exec connects through exec_connection")
    }
    fn first_contact(&mut self, _host: &str) -> Result<Contact, String> {
        unreachable!()
    }
    fn launcher(&self, _helper: &Path) -> Arc<dyn ChannelLauncher> {
        unreachable!()
    }
    fn plan(&mut self) -> Result<InstallPlan, String> {
        unreachable!()
    }
    fn agents(&mut self, _host: &str, _key: &str, _helper: &Path) -> Result<Vec<Agent>, String> {
        unreachable!()
    }
    fn sync_engine(&mut self) -> Result<Arc<dyn SyncEngine>, SyncError> {
        Ok(self.fixture.engine.clone())
    }
    fn sync_settings(&self) -> SyncSettings {
        SyncSettings {
            flush_cap: Duration::from_millis(1500),
            ..SyncSettings::default()
        }
    }
    fn exec_connection(&mut self, project: &RemoteProject) -> Result<RemoteConnection, String> {
        assert!(!self.connected, "one connection per run");
        self.connected = true;
        if self.fixture.unreachable.load(Ordering::SeqCst) {
            return Err("ssh: connect to host example-host port 22: Connection refused".into());
        }
        Ok(self.fixture.connection(project))
    }
}

#[derive(Default)]
struct Lines {
    said: Vec<String>,
}

impl Terminal for Lines {
    fn say(&mut self, line: &str) {
        self.said.push(line.to_owned());
    }
    fn ask(&mut self, _question: &str) -> bool {
        unreachable!("exec asks nothing")
    }
}

/// A sync engine that answers each flush with the next scripted outcome (then `Flushed`) and records, for each,
/// the cap it was given and whether the marker file existed at that moment. Its status carries the scripted
/// conflicts, and the caps it was asked for are recorded too.
struct RecordingEngine {
    marker: PathBuf,
    outcomes: Mutex<Vec<FlushOutcome>>,
    flushes: Mutex<Vec<(String, Duration, bool)>>,
    conflicts: Mutex<Vec<Conflict>>,
    status_caps: Mutex<Vec<Duration>>,
    flush_takes: Mutex<Duration>,
}

impl RecordingEngine {
    fn new(marker: PathBuf) -> Self {
        Self {
            marker,
            outcomes: Mutex::default(),
            flushes: Mutex::default(),
            conflicts: Mutex::default(),
            status_caps: Mutex::default(),
            flush_takes: Mutex::default(),
        }
    }

    fn script(&self, outcomes: Vec<FlushOutcome>) {
        *self.outcomes.lock().unwrap() = outcomes;
    }

    fn differ(&self, paths: &[&str]) {
        *self.conflicts.lock().unwrap() = paths
            .iter()
            .map(|path| Conflict {
                path: (*path).to_owned(),
                laptop: EntryKind::File,
                server: EntryKind::File,
            })
            .collect();
    }

    fn status_caps(&self) -> Vec<Duration> {
        self.status_caps.lock().unwrap().clone()
    }

    fn flushes(&self) -> Vec<(String, Duration, bool)> {
        self.flushes.lock().unwrap().clone()
    }
}

impl SyncEngine for RecordingEngine {
    fn create(&self, _spec: &SessionSpec) -> Result<(), SyncError> {
        unreachable!()
    }
    fn terminate(&self, _name: &str) -> Result<(), SyncError> {
        unreachable!()
    }
    fn pause(&self, _name: &str) -> Result<(), SyncError> {
        unreachable!()
    }
    fn resume(&self, _name: &str) -> Result<(), SyncError> {
        unreachable!()
    }
    fn status(&self, _labels: &Labels) -> Result<Vec<SessionStatus>, SyncError> {
        unreachable!()
    }
    fn session_status_within(&self, name: &str, cap: Duration) -> Result<SessionStatus, SyncError> {
        self.status_caps.lock().unwrap().push(cap);
        let mut status = SessionStatus::disconnected(name, Role::Tree, "a recording engine");
        status.conflicts = self.conflicts.lock().unwrap().clone();
        Ok(status)
    }
    fn flush(&self, name: &str, cap: Duration) -> FlushOutcome {
        std::thread::sleep(*self.flush_takes.lock().unwrap());
        self.flushes
            .lock()
            .unwrap()
            .push((name.to_owned(), cap, self.marker.exists()));
        let mut outcomes = self.outcomes.lock().unwrap();
        if outcomes.is_empty() {
            FlushOutcome::Flushed
        } else {
            outcomes.remove(0)
        }
    }
    fn monitor(&self, _name: &str) -> Result<Box<dyn Iterator<Item = SessionStatus> + Send>, SyncError> {
        unreachable!()
    }
}

/// A stdin that never ends and remembers being read: a terminal nobody types into, or a job's open pipe.
struct NeverEnds(Arc<AtomicBool>);

impl Read for NeverEnds {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        self.0.store(true, Ordering::SeqCst);
        loop {
            std::thread::park();
        }
    }
}

fn empty() -> Box<dyn Read + Send> {
    Box::new(std::io::empty())
}

#[test]
fn exec_flushes_before_and_after_and_returns_the_server_code() {
    let fixture = Fixture::new();
    let marker = fixture.path("marker");
    let marker_arg = marker.to_str().unwrap();
    let (code, stdout, stderr) = fixture.exec(
        &fixture.path("work/src"),
        &["--", "sh", "-c", "pwd; : > \"$1\"; exit 7", "sh", marker_arg],
        empty(),
        false,
    );
    assert_eq!(code, 7, "{stderr}");
    // The directory on the server is the one the person was in inside the mirror.
    assert_eq!(
        String::from_utf8(stdout).unwrap().trim_end(),
        fixture.path("work/src").to_str().unwrap()
    );
    assert_eq!(stderr, "");
    let cap = Duration::from_millis(1500);
    assert_eq!(
        fixture.engine.flushes(),
        vec![(TREE.to_owned(), cap, false), (TREE.to_owned(), cap, true)],
        "one flush before the command started and one after it wrote"
    );
    // --cwd names the directory; --no-flush flushes nothing; stdin passes through until its end.
    let (code, stdout, _) = fixture.exec(
        &fixture.path("outside"),
        &[
            "--cwd",
            fixture.path("work").to_str().unwrap(),
            "--no-flush",
            "--",
            "cat",
        ],
        Box::new(std::io::Cursor::new(b"formatted\n".to_vec())),
        false,
    );
    assert_eq!((code, stdout.as_slice()), (0, b"formatted\n".as_slice()));
    assert_eq!(fixture.engine.flushes().len(), 2);
}

#[test]
fn exec_after_a_flush_timeout_says_so_and_runs() {
    let fixture = Fixture::new();
    fixture.engine.script(vec![FlushOutcome::TimedOut]);
    let marker = fixture.path("marker");
    let (code, _, stderr) = fixture.exec(
        &fixture.path("work"),
        &["--", "sh", "-c", ": > \"$1\"", "sh", marker.to_str().unwrap()],
        empty(),
        false,
    );
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.starts_with("eitri host exec: your last save may not have reached example-host"),
        "{stderr}"
    );
    assert!(marker.exists(), "the command ran after the flush timed out");
}

#[test]
fn exec_outside_a_remote_project_exits_125() {
    let fixture = Fixture::new();
    let (code, stdout, stderr) = fixture.exec(&fixture.path("outside"), &["--", "true"], empty(), false);
    assert_eq!(code, 125);
    assert!(stdout.is_empty());
    assert!(stderr.starts_with("eitri host exec: "), "{stderr}");
    assert!(stderr.contains("is not inside a remote project"), "{stderr}");
    assert!(stderr.contains("nothing was run"), "{stderr}");
    assert!(fixture.engine.flushes().is_empty());
    // A command line it does not take runs nothing either.
    for args in [vec!["true"], vec!["--"], vec!["--cwd"], vec!["--colour", "--", "true"]] {
        let (code, _, stderr) = fixture.exec(&fixture.path("work"), &args, empty(), false);
        assert_eq!(code, 125, "{args:?}");
        assert!(stderr.contains("usage: eitri host exec"), "{stderr}");
    }
    // A program the server's login PATH does not have is the helper's refusal.
    let (code, _, stderr) = fixture.exec(&fixture.path("work"), &["--", "no-such-tool-here"], empty(), false);
    assert_eq!(code, 125);
    assert!(stderr.contains("is not on the login PATH of this account"), "{stderr}");
}

#[test]
fn a_signal_maps_to_128_plus_n() {
    let fixture = Fixture::new();
    let (code, _, stderr) = fixture.exec(
        &fixture.path("work"),
        &["--", "sh", "-c", "kill -TERM $$"],
        empty(),
        false,
    );
    assert_eq!(code, 128 + 15, "{stderr}");
}

#[test]
fn a_lost_link_after_close_exits_255_with_the_unknown_line() {
    let mut fixture = Fixture::new();
    let pid_file = fixture.path("command.pid");
    let pid_arg = pid_file.to_str().unwrap().to_owned();
    let daemon = fixture.daemon.id() as i32;
    let started = pid_file.clone();
    // The daemon goes away while the command runs, after the laptop's close.
    let killer = std::thread::spawn(move || {
        let deadline = Instant::now() + HANG;
        while !started.exists() {
            assert!(Instant::now() < deadline, "the command never started");
            std::thread::sleep(Duration::from_millis(10));
        }
        // SAFETY: the fixture's own child.
        unsafe { libc::kill(daemon, libc::SIGKILL) };
    });
    let (code, _, stderr) = fixture.exec(
        &fixture.path("work"),
        &["--", "sh", "-c", "echo $$ > \"$1\"; exec sleep 30", "sh", &pid_arg],
        empty(),
        false,
    );
    killer.join().unwrap();
    let _ = fixture.daemon.wait();
    assert_eq!(code, 255, "{stderr}");
    assert!(stderr.starts_with("eitri host exec: "), "{stderr}");
    assert!(stderr.contains("is unknown"), "{stderr}");
    // The orphaned command is this test's to end.
    if let Ok(pid) = std::fs::read_to_string(&pid_file)
        .unwrap_or_default()
        .trim()
        .parse::<i32>()
    {
        // SAFETY: a process this test started through the daemon.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

#[test]
fn a_terminal_stdin_closes_at_once_and_the_exit_returns() {
    let fixture = Fixture::new();
    for (args, terminal) in [(vec!["--", "cat"], true), (vec!["--no-stdin", "--", "cat"], false)] {
        let read = Arc::new(AtomicBool::new(false));
        let (code, stdout, stderr) = fixture.exec(
            &fixture.path("work"),
            &args,
            Box::new(NeverEnds(read.clone())),
            terminal,
        );
        assert_eq!((code, stdout.len()), (0, 0), "{args:?}: {stderr}");
        assert!(!read.load(Ordering::SeqCst), "{args:?}: stdin was read");
    }
    // Plain Write is enough for the outputs; nothing is flushed to the real stdout.
    let _ = std::io::stdout().flush();
}

#[test]
fn a_conflict_in_the_project_is_said_before_and_after_and_the_command_still_runs() {
    let fixture = Fixture::new();
    fixture.engine.differ(&["src/lib.rs", "notes.md"]);
    let (code, stdout, stderr) = fixture.exec(&fixture.path("work"), &["--", "echo", "ran"], empty(), false);
    assert_eq!((code, stdout.as_slice()), (0, b"ran\n".as_slice()), "{stderr}");
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(lines.len(), 2, "{stderr}");
    assert!(
        lines[0].starts_with("eitri host exec: your last save may not have reached example-host")
            && lines[0].contains("2 file(s) differ between this laptop and example-host"),
        "{stderr}"
    );
    assert!(
        lines[1].starts_with("eitri host exec: what echo changed on example-host may not have reached")
            && lines[1].contains("2 file(s) differ"),
        "{stderr}"
    );
    // --no-flush says nothing of the project's state.
    let (code, _, stderr) = fixture.exec(&fixture.path("work"), &["--no-flush", "--", "true"], empty(), false);
    assert_eq!((code, stderr.as_str()), (0, ""));
}

#[test]
fn a_flush_and_its_conflict_check_share_one_cap() {
    let fixture = Fixture::new();
    *fixture.engine.flush_takes.lock().unwrap() = Duration::from_millis(600);
    let (code, _, stderr) = fixture.exec(&fixture.path("work"), &["--", "true"], empty(), false);
    assert_eq!((code, stderr.as_str()), (0, ""));
    let caps = fixture.engine.status_caps();
    assert_eq!(caps.len(), 2, "{caps:?}");
    // The flush cap is 1.5 s and each flush took 0.6 s: the check gets what remains of it, not a second cap.
    for cap in caps {
        assert!(cap <= Duration::from_millis(900), "{cap:?}");
    }
}

#[test]
fn a_server_that_cannot_be_reached_exits_255_and_runs_nothing() {
    let fixture = Fixture::new();
    fixture.unreachable.store(true, Ordering::SeqCst);
    let (code, stdout, stderr) = fixture.exec(&fixture.path("work"), &["--", "true"], empty(), false);
    assert_eq!(code, 255, "{stderr}");
    assert!(stdout.is_empty());
    assert!(
        stderr.starts_with("eitri host exec: example-host cannot be reached"),
        "{stderr}"
    );
    assert!(stderr.contains("nothing was run"), "{stderr}");
    assert!(
        fixture.engine.flushes().is_empty(),
        "nothing was flushed either side of a run that never started"
    );
}
