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
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use agent::private_fs;
use eitri_server_proto::{helper_client::HelperClient, *};
use sha2::{Digest, Sha256};
use tonic::{transport::Channel, Code};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REVISION: &str = env!("EITRI_BUILD_COMMIT");
fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}

struct Fixture {
    root: PathBuf,
    paths: run_dir::Paths,
    env: BTreeMap<OsString, OsString>,
    daemon: Option<Child>,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        // Keep socket fixtures short enough for the Unix socket name limit.
        let root = PathBuf::from(format!(
            "/tmp/sc-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        private_fs::create_private_dir_all(&root, &root).unwrap();
        let paths = run_dir::Paths::new(VERSION, &root.join("d"), &root.join("s"), &std::env::temp_dir()).unwrap();
        paths.prepare().unwrap();
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
        let installed = paths.install(REVISION).join("sidecar/test-rev");
        private_fs::create_private_dir_all(&installed, &paths.data).unwrap();
        std::fs::copy(binary, installed.join("verdandi-claude-sidecar")).unwrap();
        let mut env = BTreeMap::from([
            (OsString::from("HOME"), root.clone().into_os_string()),
            ("PATH".into(), "/usr/bin:/bin".into()),
            (
                "EITRI_STANDIN_RECORD_ENV".into(),
                root.join("record-env").into_os_string(),
            ),
        ]);
        for key in [
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "DBUS_SESSION_BUS_ADDRESS",
            "DBUS_SYSTEM_BUS_ADDRESS",
        ] {
            env.insert(key.into(), "disabled:".into());
        }
        Self {
            root,
            paths,
            env,
            daemon: None,
        }
    }

    fn request(&self, command_id: &str) -> StartSidecarRequest {
        let binary = self
            .paths
            .install(REVISION)
            .join("sidecar/test-rev/verdandi-claude-sidecar");
        StartSidecarRequest {
            root: self.root.to_str().unwrap().to_owned(),
            account: "example-account".into(),
            env: Default::default(),
            command_id: command_id.into(),
            sidecar_revision: "test-rev".into(),
            sidecar_sha256: format!("{:x}", Sha256::digest(std::fs::read(binary).unwrap())),
        }
    }

    async fn start(&mut self) -> HelperClient<Channel> {
        let mut profile = Vec::new();
        for (key, value) in &self.env {
            profile.extend_from_slice(b"export '");
            profile.extend_from_slice(key.as_encoded_bytes());
            profile.push(b'=');
            for byte in value.as_encoded_bytes() {
                if *byte == b'\'' {
                    profile.extend_from_slice(b"'\\''");
                } else {
                    profile.push(*byte);
                }
            }
            profile.extend_from_slice(b"'\n");
        }
        private_fs::write_private(&self.root.join("login-profile"), &profile).unwrap();
        let shell = self.root.join("login-shell");
        let mut writer = Command::new("/bin/sh");
        writer
            .args(["-c", "cat >\"$1\" && chmod 700 \"$1\"", "writer"])
            .arg(&shell)
            .stdin(Stdio::piped());
        let mut writer = writer.spawn().unwrap();
        use std::io::Write;
        writer
            .stdin
            .take()
            .unwrap()
            .write_all(b"#!/bin/sh\nprintf . >>\"$HOME/login-calls\"\n. \"$HOME/login-profile\"\nif [ -n \"$EITRI_TEST_LOGIN_HOLD\" ]; then while [ -e \"$EITRI_TEST_LOGIN_HOLD\" ]; do /bin/sleep 0.01; done; fi\n/usr/bin/env -0 >\"$HOME/captured-env\"\nprintf 'Profile banner\\n'\neval \"$2\"\n")
            .unwrap();
        assert!(writer.wait().unwrap().success());
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "fixture_daemon_entry", "--nocapture"])
            .env_clear()
            .env("EITRI_TEST_DAEMON", "1")
            .env("HOME", &self.root)
            .env("SHELL", shell)
            .env("XDG_DATA_HOME", self.root.join("d"))
            .env("XDG_STATE_HOME", self.root.join("s"))
            .env("PATH", "/usr/bin:/bin")
            .env("EITRI_DAEMON_ONLY", "must-not-leak")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        self.daemon = Some(child);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                self.daemon.as_mut().unwrap().try_wait().unwrap().is_none(),
                "daemon exited during startup"
            );
            if let Ok(channel) = helper_channel(self.paths.socket().unwrap()).await {
                let mut client = HelperClient::new(channel);
                if client
                    .hello(HelloRequest {
                        protocol_major: PROTOCOL_MAJOR,
                        protocol_minor: PROTOCOL_MINOR,
                        client_version: VERSION.into(),
                    })
                    .await
                    .is_ok()
                {
                    return client;
                }
            }
            assert!(tokio::time::Instant::now() < deadline, "daemon did not become ready");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn crash(&mut self) {
        let mut child = self.daemon.take().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        // A killed daemon cannot run its ordinary socket cleanup.
        std::fs::remove_file(self.paths.socket().unwrap()).unwrap();
    }

    fn recorded_env(&self) -> BTreeMap<OsString, OsString> {
        std::fs::read(self.root.join("record-env"))
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
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Pin cleanup to each recorded start time rather than signalling a reused pid.
        if let Ok(files) = std::fs::read_dir(&self.paths.run) {
            for file in files
                .flatten()
                .filter(|f| f.path().extension() == Some(OsStr::new("pid")))
            {
                if let Ok(bytes) = std::fs::read(file.path()) {
                    if let Ok(record) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                        if let (Some(pid), Some(start)) = (record["pid"].as_u64(), record["start_time"].as_u64()) {
                            if daemon::sidecars::start_time(pid as u32).ok() == Some(start) {
                                unsafe {
                                    libc::kill(pid as i32, libc::SIGKILL);
                                }
                            }
                        }
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn parse_env(bytes: &[u8]) -> BTreeMap<OsString, OsString> {
    bytes
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

#[test]
fn fixture_daemon_entry() {
    if std::env::var_os("EITRI_TEST_DAEMON").is_none() {
        return;
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let paths = run_dir::Paths::from_env(VERSION).unwrap();
        let mut baseline = daemon::login_env::minimal_environment().unwrap();
        for key in ["HOME", "SHELL"] {
            baseline.insert(key.into(), std::env::var_os(key).unwrap());
        }
        daemon::run_with_environment(
            paths,
            Duration::from_secs(600),
            daemon::OwnedSidecars::default(),
            async move { Ok(daemon::login_env::capture_with(baseline).await) },
        )
        .await
        .unwrap();
    });
}

async fn helper_channel(socket: PathBuf) -> Result<Channel, tonic::transport::Error> {
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
}

async fn list(client: &mut HelperClient<Channel>) -> Vec<SidecarInfo> {
    client
        .list_sidecars(ListSidecarsRequest {})
        .await
        .unwrap()
        .into_inner()
        .sidecars
}

#[tokio::test]
async fn start_requires_an_owned_existing_root() {
    let mut f = Fixture::new();
    let mut client = f.start().await;
    for (path, why) in [
        ("relative".to_owned(), "absolute"),
        (f.root.join("missing").display().to_string(), "existing"),
        (f.root.join("login-profile").display().to_string(), "directory"),
    ] {
        let mut req = f.request("invalid-root");
        req.root = path;
        let error = client.start_sidecar(req).await.unwrap_err();
        assert_eq!(error.code(), Code::InvalidArgument);
        assert!(error.message().contains(why), "{error}");
    }
    if private_fs::euid() != 0 {
        let mut req = f.request("unowned");
        req.root = "/".into();
        let error = client.start_sidecar(req).await.unwrap_err();
        assert_eq!(error.code(), Code::InvalidArgument);
        assert!(error.message().contains("owned"));
    }
}

#[tokio::test]
async fn start_refuses_env_outside_the_allow_list() {
    let mut f = Fixture::new();
    let mut client = f.start().await;
    let mut req = f.request("env");
    req.env.insert("PATH".into(), "/srv/project".into());
    let error = client.start_sidecar(req).await.unwrap_err();
    assert_eq!(error.code(), Code::InvalidArgument);
    assert!(error.message().contains("PATH"));
    assert!(list(&mut client).await.is_empty());
}

#[tokio::test]
async fn start_is_idempotent_by_command_id() {
    let mut f = Fixture::new();
    let mut client = f.start().await;
    let req = f.request("same");
    let mut other = client.clone();
    let (a, b) = tokio::join!(other.start_sidecar(req.clone()), client.start_sidecar(req.clone()));
    let id = a.unwrap().into_inner().sidecar_id;
    assert_eq!(id, b.unwrap().into_inner().sidecar_id);
    assert_eq!(list(&mut client).await.len(), 1);
    assert_eq!(std::fs::read(f.root.join("login-calls")).unwrap(), b".");
    f.crash();
    let mut client = f.start().await;
    assert_eq!(id, client.start_sidecar(req).await.unwrap().into_inner().sidecar_id);
    assert_eq!(list(&mut client).await.len(), 1);
    assert_eq!(std::fs::read(f.root.join("login-calls")).unwrap(), b"..");
}

#[tokio::test]
async fn a_changed_sidecar_binary_is_not_started() {
    let mut f = Fixture::new();
    let mut client = f.start().await;
    let req = f.request("changed");
    std::fs::write(
        f.paths
            .install(REVISION)
            .join("sidecar/test-rev/verdandi-claude-sidecar"),
        b"changed",
    )
    .unwrap();
    let error = client.start_sidecar(req).await.unwrap_err();
    assert_eq!(error.code(), Code::FailedPrecondition);
    assert_eq!(error.message(), "sidecar changed since install");
    assert!(list(&mut client).await.is_empty());
}

#[tokio::test]
async fn a_sidecar_outlives_the_daemon() {
    let mut f = Fixture::new();
    let mut client = f.start().await;
    let id = client
        .start_sidecar(f.request("detach"))
        .await
        .unwrap()
        .into_inner()
        .sidecar_id;
    let pid = list(&mut client).await[0].pid;
    f.crash();
    assert!(run_dir::pid_alive(pid));
    daemon::sidecars::handshake(
        &agent::socket_path::in_dir(&f.paths.run, &format!("sc-{id}.sock")).unwrap(),
        pid,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn readopt_finds_live_sidecars_and_skips_reused_pids() {
    let mut f = Fixture::new();
    let mut client = f.start().await;
    let id = client
        .start_sidecar(f.request("adopt"))
        .await
        .unwrap()
        .into_inner()
        .sidecar_id;
    f.crash();
    let file = f.paths.run.join(format!("sc-{id}.pid"));
    let mut interrupted: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    interrupted["ready"] = false.into();
    private_fs::write_private(&file, &serde_json::to_vec(&interrupted).unwrap()).unwrap();
    let adopted = daemon::sidecars::Sidecars::new(f.paths.clone()).unwrap();
    adopted.refresh_handshakes().await;
    assert_eq!(adopted.list().sidecars.len(), 1);
    assert!(adopted.list().sidecars[0].alive);
    assert!(adopted.list().sidecars[0].handshake);
    let mut record: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(
        record["ready"], true,
        "maintenance claims a responding interrupted start"
    );
    record["start_time"] = (record["start_time"].as_u64().unwrap() + 1).into();
    private_fs::write_private(&file, &serde_json::to_vec(&record).unwrap()).unwrap();
    let skipped = daemon::sidecars::Sidecars::new(f.paths.clone()).unwrap();
    record["start_time"] = (record["start_time"].as_u64().unwrap() - 1).into();
    private_fs::write_private(&file, &serde_json::to_vec(&record).unwrap()).unwrap();
    assert!(!skipped.list().sidecars[0].alive);
}

#[tokio::test]
async fn stop_ends_an_adopted_sidecar() {
    let mut f = Fixture::new();
    let mut client = f.start().await;
    let id = client
        .start_sidecar(f.request("stop"))
        .await
        .unwrap()
        .into_inner()
        .sidecar_id;
    f.crash();
    let mut client = f.start().await;
    assert!(list(&mut client).await[0].alive);
    client
        .stop_sidecar(StopSidecarRequest {
            sidecar_id: id,
            command_id: "stop-command".into(),
        })
        .await
        .unwrap();
    assert!(!list(&mut client).await[0].alive);
}

#[tokio::test]
async fn list_shows_exited_sidecars_as_not_alive() {
    let mut f = Fixture::new();
    let mut client = f.start().await;
    let id = client
        .start_sidecar(f.request("exited"))
        .await
        .unwrap()
        .into_inner()
        .sidecar_id;
    client
        .stop_sidecar(StopSidecarRequest {
            sidecar_id: id.clone(),
            command_id: "stop".into(),
        })
        .await
        .unwrap();
    f.crash();
    let mut client = f.start().await;
    let entries = list(&mut client).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].sidecar_id, id);
    assert!(!entries[0].alive);
}

#[tokio::test]
async fn helper_sidecar_env_equals_local_sidecar_env() {
    let mut f = Fixture::new();
    for key in [
        "VERDANDI_CLAUDE_EXTRA_MCP_SERVERS",
        "VERDANDI_CLAUDE_SIDECAR_PARENT_WATCH",
        "VERDANDI_CLAUDE_SIDECAR_FAULT_DROP_WATCH_AFTER",
        "VERDANDI_CLAUDE_SIDECAR_RING_CAPACITY",
        "VERDANDI_CLAUDE_SIDECAR_WRITE_QUEUE_STATS_MS",
        "CLAUDE_CODE_AUTO_CONNECT_IDE",
    ] {
        f.env.insert(key.into(), "true".into());
    }
    f.env.insert("BASH_MAX_TIMEOUT_MS".into(), "98765".into());
    f.env.insert("EITRI_SIDECAR_TEST_SEAMS".into(), "1".into());
    f.env.insert("NON_UTF8".into(), OsString::from_vec(vec![255]));
    let mut client = f.start().await;
    client.start_sidecar(f.request("policy")).await.unwrap();
    let captured = parse_env(&std::fs::read(f.root.join("captured-env")).unwrap());
    let located = agent::locate_claude_in(&|k| captured.get(OsStr::new(k)).cloned());
    let cli = match &located {
        agent::ClaudeCli::Installed(path) => Some(path.as_path()),
        _ => None,
    };
    let policy = agent::sidecar_env_policy(
        &|k| captured.get(OsStr::new(k)).cloned(),
        Some("example-account"),
        false,
        cli,
    );
    let mut local = Command::new("/bin/true");
    local.env_clear().envs(&captured);
    agent::apply_sidecar_env(&mut local, &policy);
    let expected: BTreeMap<_, _> = local
        .get_envs()
        .filter_map(|(k, v)| v.map(|v| (k.to_owned(), v.to_owned())))
        .collect();
    let mut actual = f.recorded_env();
    actual.remove(OsStr::new("VERDANDI_CLAUDE_SIDECAR_SOCKET"));
    assert_eq!(
        actual.remove(OsStr::new("VERDANDI_CLAUDE_SIDECAR_PARENT_WATCH")),
        Some("none".into())
    );
    assert_eq!(actual, expected);
    assert!(!actual.contains_key(OsStr::new("EITRI_DAEMON_ONLY")));
}

#[tokio::test]
async fn claude_is_found_through_the_login_path() {
    let mut f = Fixture::new();
    let bin = f.root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::fs::copy("/bin/true", bin.join("claude")).unwrap();
    f.env
        .insert("PATH".into(), format!(":.:relative:{}", bin.display()).into());
    let mut client = f.start().await;
    client.start_sidecar(f.request("path")).await.unwrap();
    assert_eq!(f.recorded_env().get(OsStr::new("PATH")), Some(&bin.into_os_string()));
}

#[tokio::test]
async fn sidecar_survives_its_log_cap_with_the_daemon_dead() {
    let mut f = Fixture::new();
    f.env.insert(
        "EITRI_STANDIN_STDERR_FLOOD".into(),
        (20 * 1024 * 1024).to_string().into(),
    );
    let mut client = f.start().await;
    let id = client
        .start_sidecar(f.request("flood"))
        .await
        .unwrap()
        .into_inner()
        .sidecar_id;
    let pid = list(&mut client).await[0].pid;
    f.crash();
    let log = f.paths.run.join(format!("sc-{id}.err"));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while std::fs::metadata(&log).unwrap().len() < 20 * 1024 * 1024 {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    daemon::sidecars::handshake(
        &agent::socket_path::in_dir(&f.paths.run, &format!("sc-{id}.sock")).unwrap(),
        pid,
    )
    .await
    .unwrap();
    let mut client = f.start().await;
    assert!(list(&mut client).await[0].alive);
    while std::fs::metadata(&log).unwrap().len() > 8 * 1024 * 1024 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "startup maintenance did not trim the log"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        std::fs::metadata(log.with_extension("err.1")).unwrap().len(),
        1024 * 1024
    );
}

#[tokio::test]
async fn hello_answers_within_the_real_start_budget_while_login_capture_is_held() {
    let mut f = Fixture::new();
    let hold = f.root.join("hold-login");
    private_fs::write_private(&hold, b"held").unwrap();
    f.env
        .insert("EITRI_TEST_LOGIN_HOLD".into(), hold.clone().into_os_string());
    let mut client = f.start().await;
    let hello = client
        .hello(HelloRequest {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            client_version: VERSION.into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(hello.login_env(), LoginEnvState::Capturing);
    assert!(hello.warnings.is_empty());
    assert!(hold.exists());
    assert!(client
        .list_sidecars(ListSidecarsRequest {})
        .await
        .unwrap()
        .into_inner()
        .sidecars
        .is_empty());
    std::fs::remove_file(hold).unwrap();
    client.start_sidecar(f.request("after-capture")).await.unwrap();
    let response = client.list_sidecars(ListSidecarsRequest {}).await.unwrap().into_inner();
    assert_eq!(response.login_env(), LoginEnvState::Captured);
    assert!(response.warnings.is_empty());
}

#[tokio::test]
async fn incomplete_live_start_is_readopted_by_the_same_command_id() {
    let mut f = Fixture::new();
    let mut client = f.start().await;
    let request = f.request("interrupted-start");
    let id = client
        .start_sidecar(request.clone())
        .await
        .unwrap()
        .into_inner()
        .sidecar_id;
    f.crash();
    let file = f.paths.metadata_run().join(format!("sc-{id}.pid"));
    let mut record: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    record["ready"] = false.into();
    private_fs::write_private(&file, &serde_json::to_vec(&record).unwrap()).unwrap();
    let mut client = f.start().await;
    assert_eq!(client.start_sidecar(request).await.unwrap().into_inner().sidecar_id, id);
    assert_eq!(list(&mut client).await.len(), 1);
    let record: serde_json::Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
    assert_eq!(record["ready"], true);
}

#[tokio::test]
async fn symlinked_root_prefix_records_the_canonical_directory() {
    let mut f = Fixture::new();
    let project = f.root.join("var/home/project");
    std::fs::create_dir_all(&project).unwrap();
    std::os::unix::fs::symlink("var/home", f.root.join("home")).unwrap();
    let mut client = f.start().await;
    let mut request = f.request("linked-home");
    request.root = f.root.join("home/project").to_str().unwrap().into();
    client.start_sidecar(request).await.unwrap();
    assert_eq!(list(&mut client).await[0].root, project.to_str().unwrap());
}

#[tokio::test]
async fn stop_refuses_an_empty_command_id() {
    let mut f = Fixture::new();
    let mut client = f.start().await;
    let id = client
        .start_sidecar(f.request("stop-validation"))
        .await
        .unwrap()
        .into_inner()
        .sidecar_id;
    let error = client
        .stop_sidecar(StopSidecarRequest {
            sidecar_id: id,
            command_id: String::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::InvalidArgument);
    assert!(list(&mut client).await[0].alive);
}

#[tokio::test]
async fn a_failed_start_is_retried_with_the_same_command_after_daemon_restart() {
    let mut f = Fixture::new();
    f.env.insert("EITRI_STANDIN_REFUSE_BIND".into(), "refused".into());
    let mut client = f.start().await;
    let request = f.request("failed-start");
    assert_eq!(
        client.start_sidecar(request.clone()).await.unwrap_err().code(),
        Code::Unavailable
    );
    let failed_id = list(&mut client).await[0].sidecar_id.clone();
    assert_eq!(
        client.start_sidecar(request.clone()).await.unwrap_err().code(),
        Code::Unavailable
    );
    f.crash();
    f.env.remove(OsStr::new("EITRI_STANDIN_REFUSE_BIND"));
    let mut client = f.start().await;
    let started = client.start_sidecar(request).await.unwrap().into_inner().sidecar_id;
    assert_ne!(started, failed_id);
    assert!(list(&mut client)
        .await
        .iter()
        .any(|s| s.sidecar_id == started && s.alive));
}
