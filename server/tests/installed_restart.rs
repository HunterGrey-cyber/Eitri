//! A daemon killed mid-turn is started again by the product's own reconnect, not by a test launcher: a helper
//! installed by the real install plan, a window's `RemoteConnection` over it (whose launcher runs the recorded
//! installation's checked START when a connection needs recovery), and a remote tab's provider on that connection.
//! The daemon is SIGKILLed while a turn is held mid-stream; the restart is held back until the sidecar has written
//! the rest of the turn and its end; the tab must still get every chunk once, the turn's end and a next turn.
//!
//! One test in this binary on purpose: it points the process's own HOME and XDG variables at a private home, which the
//! steps and the helper read the way a login on a server does.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent::remote_client::HelperConnections;
use agent::setting_sources::ProjectTrust;
use agent::ssh::{ChannelLauncher, LocalExec, ProbeReport, RootState};
use eitri_core::remote_host::install;
use eitri_core::remote_host::provenance::HelperProvenance;
use eitri_core::remote_host::steps;
use eitri_core::remote_project::{RemoteConnection, RemoteHost, RemoteProject, RemoteProjects};
use sha2::{Digest, Sha256};

const BINARY: &str = env!("CARGO_BIN_EXE_eitri-server");
const HOST: &str = "example-host";
const HOST_KEY: &str = "SHA256:example";
/// How long any one wait may take before the test is called hung; not a speed bound.
const HANG: Duration = Duration::from_secs(90);

fn standin() -> PathBuf {
    let binary = std::env::var_os("EITRI_TEST_STANDIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("CARGO_TARGET_DIR").expect("CARGO_TARGET_DIR is required"))
                .join("debug/eitri-sidecar-standin")
        });
    assert!(
        binary.is_file(),
        "build agent's eitri-sidecar-standin with test-support first: {}",
        binary.display()
    );
    binary
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < HANG, "{what} never happened");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The server as the window's connection sees it locally: each helper channel runs the helper the window's launcher
/// names, and the one step a window's recovery runs, START, runs as `sh -c` in the home. Once armed, START waits until
/// `until` exists, so a daemon restart can be held back; every START is counted.
struct Server {
    steps: LocalExec,
    until: PathBuf,
    armed: AtomicBool,
    steps_run: AtomicUsize,
}

impl ChannelLauncher for Server {
    fn spawn(&self, _tail: &[OsString]) -> std::io::Result<std::process::Child> {
        Err(std::io::Error::other("every channel names its installation's helper"))
    }
    fn spawn_at(&self, helper: &Path, tail: &[OsString]) -> std::io::Result<std::process::Child> {
        LocalExec {
            helper: helper.to_owned(),
        }
        .spawn(tail)
    }
    fn spawn_step(&self, literal: &'static str, args: &[&str]) -> std::io::Result<std::process::Child> {
        if literal != steps::START {
            return Err(std::io::Error::other("a window's recovery runs START only"));
        }
        if self.armed.load(Ordering::SeqCst) {
            wait_for("the sidecar done writing", || self.until.exists());
        }
        self.steps_run.fetch_add(1, Ordering::SeqCst);
        self.steps.spawn_step(steps::START, args)
    }
}

struct Home(PathBuf);

impl Drop for Home {
    fn drop(&mut self) {
        // Daemons and sidecars this test started end by the pids they published.
        for dir in [".local/state/eitri-server/run"] {
            if let Ok(runs) = std::fs::read_dir(self.0.join(dir)) {
                for run in runs.flatten() {
                    let pid = std::fs::read(run.path().join("info"))
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                        .and_then(|value| value["pid"].as_u64());
                    if let Some(pid) = pid {
                        let exe = std::fs::read_link(format!("/proc/{pid}/exe")).unwrap_or_default();
                        if exe.starts_with(&self.0) {
                            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                        }
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn daemon_pid(state: &Path) -> Option<u32> {
    let runs = std::fs::read_dir(state.join("run")).ok()?;
    runs.flatten().find_map(|run| {
        let info: serde_json::Value = serde_json::from_slice(&std::fs::read(run.path().join("info")).ok()?).ok()?;
        let pid = info["pid"].as_u64()? as u32;
        // SAFETY: signal zero only probes the process.
        (unsafe { libc::kill(pid as i32, 0) } == 0).then_some(pid)
    })
}

#[test]
fn a_daemon_killed_mid_turn_is_restarted_by_the_windows_own_launcher_and_the_tab_goes_on() {
    let home = Home(std::env::temp_dir().join(format!("ir-{}", std::process::id())));
    agent::private_fs::create_private_dir_all(&home.0.join("r"), &home.0).unwrap();
    std::env::set_var("HOME", &home.0);
    std::env::set_var("XDG_RUNTIME_DIR", home.0.join("r"));
    for key in [
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CONFIG_HOME",
        "DISPLAY",
        "WAYLAND_DISPLAY",
    ] {
        std::env::remove_var(key);
    }
    for key in ["DBUS_SESSION_BUS_ADDRESS", "DBUS_SYSTEM_BUS_ADDRESS"] {
        std::env::set_var(key, "disabled:");
    }
    let data = home.0.join(".local/share/eitri-server");
    let state = home.0.join(".local/state/eitri-server");
    let probe = ProbeReport {
        uname: "Linux x86_64".into(),
        home: home.0.clone(),
        data_dir: data.clone(),
        state_dir: state.clone(),
        login_shell: "/bin/sh".into(),
        login_path: "/usr/bin:/bin".into(),
        claude: None,
        git: None,
        glibc: Some(eitri_core::remote_host::probe_step::GLIBC_FLOOR),
        linger: Some(true),
        kill_user_processes: Some(false),
        free_bytes: Some(u64::MAX),
        gzip: false,
        root: RootState::Missing,
        sync_marker: None,
    };
    let provenance = HelperProvenance::Unsigned {
        sha256: format!("{:x}", Sha256::digest(std::fs::read(BINARY).unwrap())),
    };
    let sidecar = agent::UploadSidecar {
        path: standin(),
        revision: "abcdef0".into(),
        origin: agent::UploadSidecarOrigin::PerUserBuild,
        warnings: Vec::new(),
    };
    let mut plan = install::plan_for(&probe, &provenance, sidecar, PathBuf::from(BINARY)).unwrap();
    // The laptop's records live in the same private home: they are what the window reads.
    let store = RemoteProjects::new(Some(home.0.join("laptop-state").as_os_str()), None);
    let consented = RemoteHost {
        version: 1,
        host_key_sha256: HOST_KEY.into(),
        data_dir: data.clone(),
        state_dir: state.clone(),
        helpers: [(plan.version().to_owned(), provenance.sha256().to_owned())].into(),
        sidecars: Default::default(),
        consented: true,
    };
    store.save_host(&consented).unwrap();
    plan.use_host_record(&consented).unwrap();
    let server = Arc::new(Server {
        steps: LocalExec {
            helper: home.0.join("steps"),
        },
        until: home.0.join("written"),
        armed: AtomicBool::new(false),
        steps_run: AtomicUsize::new(0),
    });
    install::install_with(&server.steps, &plan, &|_, _| {}).unwrap();
    install::ensure_started_with(&server.steps, &plan).unwrap();
    plan.record_installed(&store).unwrap();
    let host = store.host(HOST_KEY).unwrap().expect("the installation is recorded");
    let project_root = home.0.join("project");
    std::fs::create_dir(&project_root).unwrap();
    let project = RemoteProject {
        version: 1,
        host: HOST.into(),
        host_key_sha256: HOST_KEY.into(),
        root: project_root.clone(),
        helper_version: plan.version().to_owned(),
        sync: None,
        created_at: 1,
        queued_stops: Vec::new(),
    };
    store.save(&project).unwrap();

    // The window's connection and a tab started through it, as a window starts one: its launcher is a window's,
    // with the recorded installation's START for recovery, and the tab's events are folded by its own ingestion.
    let connection =
        RemoteConnection::open(server.clone(), HOST_KEY, &project, &host, plan.version(), "abcdef0").unwrap();
    let mut tab = connection
        .start_tab(None, ProjectTrust::Untrusted, None, &store)
        .unwrap_or_else(|error| panic!("the tab starts: {}", error.message));
    let sidecar_id = tab.helper_handoff().expect("a remote tab").sidecar_id;

    // 1. Killed while the turn is held mid-stream, the restart held back until the sidecar has written the rest of
    //    the turn and its end: all of it is read back from the sidecar's ring.
    let gate = home.0.join("gate");
    std::fs::create_dir(&gate).unwrap();
    send(
        &mut tab,
        &format!(
            "text m1 drip-one ; gate {}; text m2 drip-two ; sleep 300; text m3 drip-three ; end; touch {}",
            gate.display(),
            server.until.display()
        ),
    );
    wait_for("the first chunk on the tab", || transcript(&tab).contains("drip-one"));
    wait_for("the stand-in at its gate", || gate.join("ready").exists());
    let steps_before = server.steps_run.load(Ordering::SeqCst);
    server.armed.store(true, Ordering::SeqCst);
    let first = kill_daemon(&state);
    std::fs::write(gate.join("release"), b"").unwrap();
    wait_for("the turn's end on the tab", || turn_done(&tab, "drip-three"));
    let second = restarted(&server, &state, &data, first, steps_before);
    assert_once_in_order(&tab, &["drip-one", "drip-two", "drip-three"]);
    server.armed.store(false, Ordering::SeqCst);

    // 2. Killed while the sidecar streams, the restart not held: the replacement adopts the sidecar mid-stream.
    let chunks: Vec<String> = (1..=40).map(|k| format!("stream-{k:02}")).collect();
    let streamed_all = home.0.join("streamed-all");
    let script = chunks
        .iter()
        .map(|chunk| format!("text m-{chunk} {chunk} ; sleep 150"))
        .chain(["end".to_string(), format!("touch {}", streamed_all.display())])
        .collect::<Vec<_>>()
        .join("; ");
    send(&mut tab, &script);
    wait_for("streaming under way", || transcript(&tab).contains("stream-03"));
    let steps_before = server.steps_run.load(Ordering::SeqCst);
    kill_daemon_at(second);
    restarted(&server, &state, &data, second, steps_before);
    assert!(
        !streamed_all.exists(),
        "the replacement came up while the sidecar was still streaming"
    );
    wait_for("the streamed turn's end on the tab", || turn_done(&tab, "stream-40"));
    let streamed: Vec<&str> = chunks.iter().map(String::as_str).collect();
    assert_once_in_order(&tab, &streamed);

    // 3. A next turn on the same tab and the same sidecar.
    send(&mut tab, "text m9 next-turn ; end");
    wait_for("the next turn on the tab", || turn_done(&tab, "next-turn"));
    let projection = tab.projection();
    assert!(
        matches!(projection.status, agent::ProjectionStatus::Running),
        "{:?}",
        projection.status
    );
    assert!(projection.turn_endings.is_empty(), "{:?}", projection.turn_endings);
    drop(projection);
    let listed = connection.open_for(HOST, HOST_KEY).unwrap().list().unwrap();
    let alive: Vec<_> = listed.iter().filter(|sidecar| sidecar.alive).collect();
    assert_eq!(alive.len(), 1, "{listed:?}");
    assert_eq!(alive[0].sidecar_id, sidecar_id, "the same sidecar throughout");
    drop(tab);
}

fn send(tab: &mut eitri_core::agent_backend::AgentBackend, script: &str) {
    tab.send_turn(script, script, agent::PromptOrigin::Typed, Vec::new())
        .unwrap_or_else(|error| panic!("the turn is sent: {}", error.message));
}

/// Everything the tab shows of the agent's replies, in order.
fn transcript(tab: &eitri_core::agent_backend::AgentBackend) -> String {
    tab.projection()
        .transcript
        .iter()
        .map(|message| message.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The tab's turn has ended (no turn running) with `last` shown, and nothing said the session went away.
fn turn_done(tab: &eitri_core::agent_backend::AgentBackend, last: &str) -> bool {
    let projection = tab.projection();
    if let agent::ProjectionStatus::Unavailable { reason } | agent::ProjectionStatus::Closed { reason } =
        &projection.status
    {
        panic!("the tab's session ended: {reason}");
    }
    projection.active_turn_id.is_none() && transcript_of(&projection).contains(last)
}

fn transcript_of(projection: &agent::AgentSessionProjection) -> String {
    projection
        .transcript
        .iter()
        .map(|message| message.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_once_in_order(tab: &eitri_core::agent_backend::AgentBackend, chunks: &[&str]) {
    let text = transcript(tab);
    let mut from = 0;
    for chunk in chunks {
        assert_eq!(text.matches(chunk).count(), 1, "{chunk} once in {text:?}");
        let at = text.find(chunk).unwrap();
        assert!(at >= from, "{chunk} out of order in {text:?}");
        from = at;
    }
}

/// SIGKILLs the running daemon, by the pid it published, and waits until it is gone. Its pid.
fn kill_daemon(state: &Path) -> u32 {
    let pid = daemon_pid(state).expect("the installed daemon runs");
    kill_daemon_at(pid);
    pid
}

fn kill_daemon_at(pid: u32) {
    // SAFETY: SIGKILL to the daemon this test's install started, by the pid it published.
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
    wait_for("the killed daemon gone", || unsafe { libc::kill(pid as i32, 0) } != 0);
}

/// The daemon that replaced `old`: started by the window's own START (`steps_before` counted before the kill) and
/// running from the installation. Its pid.
fn restarted(server: &Server, state: &Path, data: &Path, old: u32, steps_before: usize) -> u32 {
    wait_for("a replacement daemon", || {
        daemon_pid(state).is_some_and(|pid| pid != old)
    });
    assert!(
        server.steps_run.load(Ordering::SeqCst) > steps_before,
        "the window's launcher ran the recorded installation's START"
    );
    let new = daemon_pid(state).unwrap();
    let exe = std::fs::read_link(format!("/proc/{new}/exe")).unwrap();
    assert!(
        exe.starts_with(data),
        "the replacement runs from the installation: {}",
        exe.display()
    );
    new
}
