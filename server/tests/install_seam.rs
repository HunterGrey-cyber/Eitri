//! The laptop's installer and the helper agree on where an installation lives: a helper installed by the real
//! install plan (a content-keyed directory under the server's data directory) starts its daemon, starts a sidecar
//! from that installation through StartSidecar, and is stopped by its own `stop-daemon`.
//!
//! One test in this binary on purpose: it points the process's own HOME and XDG variables at a private home, which the
//! steps and the helper read the way a login on a server does.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent::ssh::{LocalExec, ProbeReport, RootState};
use eitri_core::remote_host::install;
use eitri_core::remote_host::provenance::HelperProvenance;
use eitri_core::remote_project::RemoteHost;
use sha2::{Digest, Sha256};

const BINARY: &str = env!("CARGO_BIN_EXE_eitri-server");

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

fn hash(path: &Path) -> String {
    format!("{:x}", Sha256::digest(std::fs::read(path).unwrap()))
}

fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal zero only probes the process.
    pid > 1 && unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Every daemon `list-daemons` of `helper` reports: its identity and the key `stop-daemon` takes for it.
fn listed(helper: &Path) -> Vec<(agent::ssh::DaemonInfo, String)> {
    let output = std::process::Command::new(helper)
        .args(["list-daemons"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            let run = value["run"].as_str().unwrap().to_owned();
            (serde_json::from_value(value).unwrap(), run)
        })
        .collect()
}

struct Home(PathBuf);

impl Drop for Home {
    fn drop(&mut self) {
        // A daemon this test could not stop is ended by the pid it published.
        if let Ok(runs) = std::fs::read_dir(self.0.join(".local/state/eitri-server/run")) {
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
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A helper installed by the real install plan starts its daemon, a sidecar from its installation, and is stopped by
/// `stop-daemon`. A second installation of the same version (another sidecar build) runs a daemon of its own, which
/// starts that installation's sidecar, and the first installation's helper stops both.
#[test]
fn an_installed_helper_starts_its_daemon_and_sidecar_and_stops() {
    let home = Home(std::env::temp_dir().join(format!("is-{}", std::process::id())));
    agent::private_fs::create_private_dir_all(&home.0.join("r"), &home.0).unwrap();
    // The helper and its steps find their directories as a login on the server does: from HOME alone, here.
    std::env::set_var("HOME", &home.0);
    std::env::set_var("XDG_RUNTIME_DIR", home.0.join("r"));
    for key in ["XDG_DATA_HOME", "XDG_STATE_HOME", "DISPLAY", "WAYLAND_DISPLAY"] {
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
        sha256: hash(Path::new(BINARY)),
    };
    let plan_of = |sidecar: PathBuf| {
        let sidecar = agent::UploadSidecar {
            path: sidecar,
            revision: "abcdef0".into(),
            origin: agent::UploadSidecarOrigin::PerUserBuild,
            warnings: Vec::new(),
        };
        install::plan_for(&probe, &provenance, sidecar, PathBuf::from(BINARY)).unwrap()
    };
    // The same helper with another sidecar build: the same version, another installation.
    let rebuilt = home.0.join("sidecar-rebuilt");
    let mut bytes = std::fs::read(standin()).unwrap();
    bytes.extend_from_slice(b"another build");
    std::fs::write(&rebuilt, bytes).unwrap();
    let mut first = plan_of(standin());
    let mut second = plan_of(rebuilt);
    assert_ne!(first.install_path(), second.install_path());
    let record = RemoteHost {
        version: 1,
        host_key_sha256: "SHA256:example".into(),
        data_dir: data.clone(),
        state_dir: state.clone(),
        helpers: [
            (first.version().to_owned(), provenance.sha256().to_owned()),
            (second.version().to_owned(), provenance.sha256().to_owned()),
        ]
        .into(),
        sidecars: Default::default(),
        consented: true,
    };
    let steps = LocalExec {
        helper: home.0.join("steps"),
    };
    let project = home.0.join("project");
    std::fs::create_dir(&project).unwrap();
    for plan in [&mut first, &mut second] {
        plan.use_host_record(&record).unwrap();
        install::install_with(&steps, plan, &|_, _| {}).unwrap();
        assert!(
            plan.install_path().join("bin/eitri-server").is_file(),
            "the installation is the plan's own directory"
        );
        install::ensure_started_with(&steps, plan).unwrap();
        let helper: Arc<dyn agent::ssh::ChannelLauncher> = Arc::new(LocalExec {
            helper: plan.install_path().join("bin/eitri-server"),
        });
        let client =
            agent::remote_client::HelperClient::connect(helper, "example-host", Some("SHA256:example")).unwrap();
        let directory = plan
            .sidecar_path()
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .unwrap()
            .to_owned();
        let id = client
            .start_sidecar(&agent::remote_client::SidecarStart {
                root: project.clone(),
                account: None,
                command_id: format!("install-seam-{directory}"),
                sidecar_revision: directory,
                sidecar_sha256: plan.sidecar_sha256().to_owned(),
            })
            .unwrap_or_else(|status| panic!("StartSidecar: {}", status.message()));
        let listed = client.list().unwrap();
        assert!(
            listed.iter().any(|sidecar| sidecar.sidecar_id == id && sidecar.alive),
            "{listed:?}"
        );
    }

    let running = listed(&first.install_path().join("bin/eitri-server"));
    assert_eq!(running.len(), 2, "one daemon for each installation: {running:?}");
    assert_ne!(running[0].1, running[1].1);
    let mut recorded: Vec<String> = running
        .iter()
        .map(|(_, run)| {
            let info: serde_json::Value =
                serde_json::from_slice(&std::fs::read(state.join("run").join(run).join("info")).unwrap()).unwrap();
            info["installation"].as_str().unwrap().to_owned()
        })
        .collect();
    recorded.sort();
    let mut installations = [first.version().to_owned(), second.version().to_owned()];
    installations.sort();
    assert_eq!(
        recorded, installations,
        "each daemon records the installation it runs from"
    );
    for (info, run) in &running {
        assert!(pid_alive(info.pid));
        eitri_core::remote_host::skew::stop_daemon_with(&steps, &record, Some(first.version()), info, run)
            .unwrap_or_else(|error| panic!("stop-daemon {run}: {error}"));
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while running.iter().any(|(info, _)| pid_alive(info.pid)) {
        assert!(Instant::now() < deadline, "a daemon did not stop");
        std::thread::sleep(Duration::from_millis(20));
    }
}
