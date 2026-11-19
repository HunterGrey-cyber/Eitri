use super::*;
use crate::remote_sync::SyncKey;
use serde_json::{json, Value};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

struct Fake {
    root: PathBuf,
}

impl Fake {
    fn new(mut replies: Vec<Value>) -> Self {
        let target = std::env::var_os("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("CARGO_TARGET_DIR").map(|p| PathBuf::from(p).join("tmp")))
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/tmp"));
        let root = target.join(format!("mt-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        fs::create_dir_all(&root).unwrap();
        replies.extend([
            json!({"match":["version"], "stdout_file":"version.txt"}),
            json!({"match":["daemon","start"], "stdout":""}),
            json!({"match":["daemon","stop"], "stdout":""}),
            json!({"match":["sync","create"], "stdout":""}),
            json!({"match":["sync","pause"], "stdout":""}),
            json!({"match":["sync","resume"], "stdout":""}),
            json!({"match":["sync","terminate"], "stdout":""}),
            json!({"match":["sync","flush"], "stdout":""}),
        ]);
        fs::write(root.join("script.json"), serde_json::to_vec(&replies).unwrap()).unwrap();
        Self { root }
    }

    fn engine(&self) -> Result<MutagenEngine, SyncError> {
        self.with_transport(Some(self.root.join("transport")))
    }

    fn with_transport(&self, transport: Option<PathBuf>) -> Result<MutagenEngine, SyncError> {
        self.with_transport_exe(transport, None)
    }

    fn with_transport_exe(
        &self,
        transport: Option<PathBuf>,
        executable: Option<&Path>,
    ) -> Result<MutagenEngine, SyncError> {
        let binary = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mutagen/fake-mutagen");
        let mut daemon = Daemon::prepare(&self.root, Some(&binary), transport)?;
        if let Some(executable) = executable {
            daemon.set_transport_exe(executable);
        }
        daemon.test_env = vec![
            ("FAKE_MUTAGEN_LOG".into(), self.root.join("log.jsonl").into_os_string()),
            (
                "FAKE_MUTAGEN_SCRIPT".into(),
                self.root.join("script.json").into_os_string(),
            ),
            ("SSH_ASKPASS".into(), "must-be-removed".into()),
            ("SSH_ASKPASS_REQUIRE".into(), "must-be-removed".into()),
            ("DISPLAY".into(), "must-be-removed".into()),
            ("WAYLAND_DISPLAY".into(), "must-be-removed".into()),
            ("MUTAGEN_SSH_PATH".into(), "must-be-replaced".into()),
        ];
        MutagenEngine::from_daemon(daemon)
    }

    fn log(&self) -> Vec<Value> {
        fs::read_to_string(self.root.join("log.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn fixture(name: &str) -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/mutagen")
            .join(name),
    )
    .unwrap()
}

fn tree() -> SessionSpec {
    SessionSpec::tree(
        SyncKey::new("SHA256:example", Path::new("/srv/project")),
        "/srv/project-laptop".into(),
        "/srv/project".into(),
    )
}

fn arg_strings(args: Vec<OsString>) -> Vec<String> {
    args.into_iter().map(|a| a.into_string().unwrap()).collect()
}

#[test]
fn status_parses_every_recorded_state() {
    for (file, state, conflicts) in [
        ("list-ready.json", SyncState::Watching, 0),
        ("list-paused.json", SyncState::Paused, 0),
        ("list-conflicts.json", SyncState::Watching, 2),
        ("list-recreated.json", SyncState::Watching, 1),
        (
            "list-halted.json",
            SyncState::Halted("halted-on-root-emptied".into()),
            0,
        ),
    ] {
        let fake = Fake::new(vec![json!({"match":["sync","list"],"stdout_file":file})]);
        let status = fake.engine().unwrap().status(&Labels::new()).unwrap().remove(0);
        assert_eq!(status.state, state, "{file}");
        assert_eq!(status.conflicts.len(), conflicts, "{file}");
    }
    for file in ["monitor-staging-one-file.jsonl", "monitor-staging-batch.jsonl"] {
        let fake = Fake::new(vec![json!({"match":["sync","monitor"],"stdout_file":file})]);
        let engine = fake.engine().unwrap();
        let statuses: Vec<_> = engine.monitor("eitri-0123456789abcdef-tree").unwrap().collect();
        assert!(statuses.iter().any(|s| s.state == SyncState::Staging), "{file}");
        assert!(matches!(statuses.last().unwrap().state, SyncState::Disconnected(_)));
    }
    let conflicts = parse_sessions(&fixture("list-conflicts.json"))
        .unwrap()
        .remove(0)
        .conflicts;
    assert_eq!(conflicts[1].laptop, EntryKind::File);
    assert_eq!(conflicts[1].server, EntryKind::Directory);
}

#[test]
fn scan_problems_are_parsed_with_their_paths() {
    let status = parse_sessions(&fixture("list-conflicts.json")).unwrap().remove(0);
    assert_eq!(status.problems.len(), 3);
    assert_eq!(status.problems[0].path, "abs-link");
    assert_eq!(status.problems[0].side, Side::Server);
    assert!(status.problems[0].text.contains("target is absolute"));
    assert!(status.problems[1].path.contains("non-UTF-8"));
    assert_eq!(status.problems[2].path, "esc-link");
    let mut raw: Value = serde_json::from_slice(&fixture("list-conflicts.json")).unwrap();
    raw[0]["labels"]["eitri-role"] = json!("git");
    raw[0]["alpha"]["transitionProblems"] = json!([{"path":"locked","error":"permission denied"}]);
    raw[0]["lastError"] = json!("sync failed");
    let git = parse_sessions(&serde_json::to_vec(&raw).unwrap()).unwrap().remove(0);
    assert_eq!(git.problems[0].side, Side::Server);
    assert_eq!(git.problems[1].side, Side::Laptop);
    assert_eq!(git.conflicts[1].laptop, EntryKind::Directory);
    assert_eq!(git.conflicts[1].server, EntryKind::File);
    assert_eq!(git.last_error.as_deref(), Some("sync failed"));
}

#[test]
fn session_status_reports_list_failure_after_successful_flush() {
    let fake = Fake::new(vec![
        json!({"match":["sync","list"],"exit":1,"stderr":"daemon socket absent\n"}),
    ]);
    let engine = fake.engine().unwrap();
    let name = tree().name;
    assert_eq!(engine.flush(&name, Duration::from_secs(5)), FlushOutcome::Flushed);
    assert_eq!(
        engine.session_status(&name).unwrap_err().to_string(),
        "daemon socket absent"
    );
}

#[test]
fn session_status_reports_empty_listing_as_missing() {
    let fake = Fake::new(vec![json!({"match":["sync","list"],"stdout":"[]\n"})]);
    let name = tree().name;
    let error = fake.engine().unwrap().session_status(&name).unwrap_err().to_string();
    assert!(error.contains("no such session") && error.contains(&name), "{error}");
}

#[test]
fn session_status_maps_reported_disconnected_state_with_no_conflicts() {
    let name = tree().name;
    let listing = json!([{"name":name,"status":"disconnected","lastError":"connection lost"}]);
    let fake = Fake::new(vec![json!({"match":["sync","list"],"stdout":listing.to_string()})]);
    let status = fake.engine().unwrap().session_status(&name).unwrap();
    assert_eq!(status.state, SyncState::Disconnected("connection lost".into()));
    assert!(status.conflicts.is_empty());
}

#[test]
fn session_status_keeps_conflicts_for_a_reported_disconnection() {
    let name = tree().name;
    let mut raw: Value = serde_json::from_slice(&fixture("list-conflicts.json")).unwrap();
    raw[0]["name"] = json!(name);
    raw[0]["beta"]["connected"] = json!(false);
    let fake = Fake::new(vec![json!({"match":["sync","list"],"stdout":raw.to_string()})]);
    let status = fake.engine().unwrap().session_status(&name).unwrap();
    assert_eq!(status.state, SyncState::Disconnected("endpoint disconnected".into()));
    assert!(status.conflicts.iter().any(|conflict| conflict.path == "both.txt"));
}

#[test]
fn a_halted_or_missing_session_is_never_ready() {
    let halted = parse_sessions(&fixture("list-halted.json")).unwrap().remove(0);
    assert!(matches!(halted.state, SyncState::Halted(ref text) if text == "halted-on-root-emptied"));
    let fake = Fake::new(vec![]);
    let engine = fake.engine().unwrap();
    let error = engine.session_status("eitri-missing-tree").unwrap_err();
    assert!(no_such_session(&error.to_string()));
    let status = engine.status(&tree().labels).unwrap().remove(0);
    assert_eq!(status.state, SyncState::Disconnected("no such session".into()));
    let fake = Fake::new(vec![
        json!({"match":["sync","list"],"exit":1,"stderr":"daemon socket absent\n"}),
    ]);
    let error = fake.engine().unwrap().session_status("eitri-missing-tree").unwrap_err();
    assert_eq!(error.to_string(), "daemon socket absent");
    let fake = Fake::new(vec![json!({"match":["sync","monitor"],"stdout":"[]\n"})]);
    let mut monitor = fake.engine().unwrap().monitor("eitri-missing-tree").unwrap();
    assert_eq!(
        monitor.next().unwrap().state,
        SyncState::Disconnected("no such session".into())
    );
    let mut raw: Value = serde_json::from_slice(&fixture("list-ready.json")).unwrap();
    for (value, expected) in [
        ("scanning", SyncState::Scanning),
        ("staging-beta", SyncState::Staging),
        ("connecting-alpha", SyncState::Connecting),
        ("transitioning", SyncState::Applying),
        ("new-engine-state", SyncState::Other("new-engine-state".into())),
    ] {
        raw[0]["status"] = json!(value);
        raw[0]["futureField"] = json!({"ignored":true});
        assert_eq!(
            parse_sessions(&serde_json::to_vec(&raw).unwrap()).unwrap()[0].state,
            expected
        );
    }
    raw[0]["status"] = json!("watching");
    raw[0]["beta"]["connected"] = json!(false);
    assert!(matches!(
        parse_sessions(&serde_json::to_vec(&raw).unwrap()).unwrap()[0].state,
        SyncState::Disconnected(_)
    ));
    assert!(parse_sessions(b"{}").is_err());
}

/// The replica's description and the engine's spec are one session: same name, endpoints, ignores and symlink mode.
#[test]
fn git_spec_matches_the_replica_plan() {
    let key = tree().key;
    let spec = SessionSpec::git(key.clone(), "/srv/project-laptop".into(), "/srv/project".into());
    let plan = crate::remote_sync::git_replica::git_session_plan(
        &key,
        Path::new("/srv/project-laptop"),
        Path::new("/srv/project"),
    );
    assert_eq!(spec.name, plan.name);
    assert_eq!(
        spec.alpha,
        Endpoint::Server {
            key: key.clone(),
            root: plan.server_root.clone()
        }
    );
    assert_eq!(spec.beta, Endpoint::Laptop(plan.laptop_git_dir.clone()));
    assert_eq!(spec.ignores, plan.ignores);
    assert_eq!(spec.symlink_mode, Some(SymlinkMode::Ignore));
    assert_eq!(plan.symlink_mode, "ignore");
    let args = arg_strings(create_args(&spec).unwrap());
    for ignore in &plan.ignores {
        assert!(
            args.windows(2).any(|w| w[0] == "-i" && w[1] == csv_value(ignore)),
            "{ignore}"
        );
    }
    assert!(!args.contains(&"--ignore-vcs".to_owned()));
}

#[test]
fn git_session_has_the_server_as_alpha() {
    let spec = SessionSpec::git(tree().key, "/srv/project-laptop".into(), "/srv/project".into());
    let args = arg_strings(create_args(&spec).unwrap());
    assert_eq!(args[args.len() - 2], format!("eitri-{}:/srv/project/.git", spec.key));
    assert_eq!(args.last().unwrap(), "/srv/project-laptop/.git");
    assert!(args.windows(2).any(|w| w == ["--mode", "one-way-replica"]));
    let mut reversed = spec.clone();
    std::mem::swap(&mut reversed.alpha, &mut reversed.beta);
    assert!(create_args(&reversed).is_err());
}

#[test]
fn create_passes_mode_ignores_vcs_staging_and_labels() {
    let fake = Fake::new(vec![]);
    let engine = fake.engine().unwrap();
    let mut spec = tree();
    spec.ignores
        .extend(["target/*".into(), "!target/doc".into(), "space here".into()]);
    engine.create(&spec).unwrap();
    let log = fake.log();
    let args: Vec<String> = log.last().unwrap()["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().into())
        .collect();
    assert_eq!(args, arg_strings(create_args(&spec).unwrap()));
    for pair in [
        ["--mode", "two-way-safe"],
        ["-i", "\".git\""],
        ["-i", "\"target/*\""],
        ["-i", "\"!target/doc\""],
        ["-i", "\"space here\""],
    ] {
        assert!(args.windows(2).any(|w| w == pair));
    }
    assert!(args.contains(&"--ignore-vcs".into()));
    assert!(args.contains(&"--stage-mode-beta=neighboring".into()));
    for (key, value) in &spec.labels {
        assert!(args.windows(2).any(|w| w == ["--label", &format!("{key}={value}")]));
    }
    assert_eq!(spec.labels["eitri-root"], "f90394bf70603293");
    engine.pause(&spec.name).unwrap();
    engine.resume(&spec.name).unwrap();
    engine.terminate(&spec.name).unwrap();
    for verb in ["pause", "resume", "terminate"] {
        assert!(fake
            .log()
            .iter()
            .any(|entry| entry["argv"] == json!(["sync", verb, spec.name])));
    }
}

#[test]
fn tree_session_ignores_dot_git_files_and_uses_neighboring_staging() {
    let mut spec = tree();
    spec.ignores.clear();
    spec.ignore_vcs = false;
    spec.staging = StagingMode::Default;
    let args = arg_strings(create_args(&spec).unwrap());
    assert!(args.contains(&"--ignore-vcs".into()));
    assert!(args.windows(2).any(|w| w == ["-i", "\".git\""]));
    assert!(args.contains(&"--stage-mode-beta=neighboring".into()));
}

#[test]
fn ignores_with_a_newline_or_nul_are_refused() {
    let fake = Fake::new(vec![]);
    let engine = fake.engine().unwrap();
    let before = fake.log().len();
    for ignore in ["target\nother", "target\0other"] {
        let mut spec = tree();
        spec.ignores.push(ignore.into());
        assert!(engine.create(&spec).unwrap_err().to_string().contains("ignore pattern"));
    }
    assert_eq!(fake.log().len(), before);
}

#[test]
fn invalid_endpoints_and_labels_are_refused() {
    for root in [
        "relative",
        "/srv/project:other",
        "/srv/project\nother",
        "/srv/project\0other",
    ] {
        let endpoint = Endpoint::Server {
            key: tree().key,
            root: root.into(),
        };
        assert!(endpoint.render().is_err());
    }
    for value in ["", "-bad", "bad-", "bad value", "bad=label", "bad\nlabel"] {
        assert!(!valid_label_value(value));
    }
    for value in ["tree", "0", "a-b_c.d0"] {
        assert!(valid_label_value(value));
    }
    let mut spec = tree();
    spec.labels.insert("extra".into(), "bad value".into());
    assert!(create_args(&spec).is_err());
    spec = tree();
    spec.labels.remove("eitri-root");
    assert!(create_args(&spec).is_err());
}

#[test]
fn staging_reports_the_running_total() {
    let statuses: Vec<_> = String::from_utf8(fixture("monitor-staging-batch.jsonl"))
        .unwrap()
        .lines()
        .flat_map(|line| parse_sessions(line.as_bytes()).unwrap())
        .collect();
    let staging = statuses[1].staging.as_ref().unwrap();
    assert_eq!(staging.total_received_bytes, 67_960_832);
    assert_eq!(staging.received_bytes, 851_968);
    assert_eq!(staging.expected_bytes, 8_388_608);
    assert_eq!(staging.expected_files, 60);
    assert_eq!(staging.path.as_deref(), Some("data/f31.bin"));
    assert_eq!(staging.side, Side::Laptop);
    let mut raw: Value = serde_json::from_str(
        String::from_utf8(fixture("monitor-staging-batch.jsonl"))
            .unwrap()
            .lines()
            .nth(1)
            .unwrap(),
    )
    .unwrap();
    raw[0]["labels"]["eitri-role"] = json!("git");
    assert_eq!(
        parse_sessions(&serde_json::to_vec(&raw).unwrap()).unwrap()[0]
            .staging
            .as_ref()
            .unwrap()
            .side,
        Side::Server
    );
}

#[test]
fn flush_cap_kills_the_flush_and_reports_timed_out() {
    let fake = Fake::new(vec![json!({"match":["sync","flush"],"sleep":60})]);
    let engine = fake.engine().unwrap();
    assert_eq!(
        engine.flush(&tree().name, Duration::from_millis(500)),
        FlushOutcome::TimedOut
    );
    let pid = fake.log().last().unwrap()["pid"].as_i64().unwrap() as libc::pid_t;
    // SAFETY: kill with signal zero only probes existence; waitpid only inspects this pid.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    assert_eq!(unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) }, -1);
    assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
}

#[test]
fn flush_preserves_the_last_stderr_line() {
    let stderr = String::from_utf8(fixture("flush-paused.stderr")).unwrap();
    let fake = Fake::new(vec![json!({"match":["sync","flush"],"exit":1,"stderr":stderr})]);
    assert_eq!(
        fake.engine().unwrap().flush(&tree().name, Duration::from_secs(10)),
        FlushOutcome::Failed("unable to flush session: session is paused".into())
    );
    let fake = Fake::new(vec![]);
    assert_eq!(
        fake.engine().unwrap().flush(&tree().name, Duration::from_secs(10)),
        FlushOutcome::Flushed
    );
}

#[test]
fn old_mutagen_is_refused_with_its_version() {
    let fake = Fake::new(vec![json!({"match":["version"],"stdout":"0.17.9\n"})]);
    assert!(matches!(fake.engine(), Err(SyncError::Unsupported(text)) if text == "Mutagen 0.17.9 is older than 0.18"));
    assert_eq!(fake.log().len(), 1);
    let fake = Fake::new(vec![json!({"match":["version"],"stdout":"0.18.0\n"})]);
    assert_eq!(fake.engine().unwrap().version(), "0.18.0");
}

#[test]
fn every_run_uses_the_private_data_dir_no_autostart_and_transport_path() {
    let fake = Fake::new(vec![]);
    let transport = fake.root.join("transport");
    fs::create_dir(&transport).unwrap();
    let engine = fake.with_transport(Some(transport.clone())).unwrap();
    engine.create(&tree()).unwrap();
    engine.status(&Labels::new()).unwrap();
    engine.flush(&tree().name, Duration::from_secs(10));
    engine.monitor(&tree().name).unwrap().next();
    engine.stop_if_idle().unwrap();
    assert_eq!(
        fs::metadata(engine.data_dir()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for entry in fake.log() {
        let env = &entry["env"];
        assert_eq!(env["MUTAGEN_DATA_DIRECTORY"], engine.data_dir().to_str().unwrap());
        assert_eq!(env["MUTAGEN_SSH_PATH"], transport.to_str().unwrap());
        assert_eq!(
            env["EITRI_MUTAGEN_TRANSPORT_EXE"],
            std::env::current_exe().unwrap().to_str().unwrap()
        );
        assert_eq!(
            std::env::split_paths(&OsString::from(env["PATH"].as_str().unwrap())).next(),
            Some(transport.clone())
        );
        let starting = entry["argv"] == json!(["daemon", "start"]);
        assert_eq!(
            env["MUTAGEN_DISABLE_AUTOSTART"],
            if starting { Value::Null } else { json!("1") }
        );
        for key in ["SSH_ASKPASS", "SSH_ASKPASS_REQUIRE", "DISPLAY", "WAYLAND_DISPLAY"] {
            assert_eq!(env[key], Value::Null);
        }
    }
}

#[test]
fn daemon_restarts_on_version_mismatch() {
    let fake = Fake::new(vec![
        json!({"match":["sync","pause"],"once":true,"exit":1,"stderr":"version mismatch\n"}),
    ]);
    let engine = fake.engine().unwrap();
    engine.pause(&tree().name).unwrap();
    let log = fake.log();
    assert_eq!(log[2]["argv"], json!(["daemon", "stop"]));
    assert_eq!(log[3]["argv"], json!(["daemon", "start"]));
    assert_eq!(log[4]["argv"], json!(["sync", "pause", tree().name]));
    assert!(!log.iter().any(|entry| entry["argv"] == json!(["daemon"])));
}

#[test]
fn daemon_restarts_when_transport_changes() {
    let fake = Fake::new(vec![]);
    let transport = fake.root.join("transport");
    fs::create_dir(&transport).unwrap();
    fs::write(transport.join("ssh"), "first transport").unwrap();
    fake.with_transport(Some(transport.clone()))
        .unwrap()
        .create(&tree())
        .unwrap();
    fs::write(transport.join("ssh"), "changed transport").unwrap();
    fake.with_transport(Some(transport)).unwrap().create(&tree()).unwrap();
    assert!(fake
        .log()
        .iter()
        .any(|entry| entry["argv"] == json!(["daemon", "stop"])));
}

#[test]
fn different_live_executables_share_the_daemon_without_restarting_each_other() {
    let fake = Fake::new(vec![]);
    let transport = Some(fake.root.join("transport"));
    let installed = fake.root.join("installed-eitri");
    let source = fake.root.join("source-eitri");
    fs::write(&installed, "installed build").unwrap();
    fs::write(&source, "source build").unwrap();
    let first = fake.with_transport_exe(transport.clone(), Some(&installed)).unwrap();
    let second = fake.with_transport_exe(transport, Some(&source)).unwrap();
    first.create(&tree()).unwrap();
    let identity_file = first.data_dir().join("eitri-daemon-environment");
    let identity = fs::read(&identity_file).unwrap();
    for engine in [&first, &second, &first, &second] {
        engine.pause(&tree().name).unwrap();
    }
    // An explicit start is a no-op for the live daemon and retains its recorded target.
    second.create(&tree()).unwrap();
    assert_eq!(fs::read(&identity_file).unwrap(), identity);
    assert!(!fake
        .log()
        .iter()
        .any(|entry| entry["argv"] == json!(["daemon", "stop"])));
    let saved: Value = serde_json::from_slice(&identity).unwrap();
    assert_eq!(saved["target"]["path"], json!(installed.as_os_str().as_bytes()));
    assert!(saved["target"]["metadata"].is_array());

    // Only the target the daemon actually uses matters, regardless of which build notices the change.
    fs::write(&source, "another source build").unwrap();
    second.pause(&tree().name).unwrap();
    fs::write(fake.root.join("replacement"), "replacement installed build").unwrap();
    fs::rename(fake.root.join("replacement"), &installed).unwrap();
    for engine in [&second, &first, &second, &first] {
        engine.pause(&tree().name).unwrap();
    }
    assert_eq!(
        fake.log()
            .iter()
            .filter(|entry| entry["argv"] == json!(["daemon", "stop"]))
            .count(),
        1
    );
}

#[test]
fn deleted_executable_suffix_and_vanished_targets_do_not_fail_operations() {
    let fake = Fake::new(vec![]);
    let executable = fake.root.join("eitri-bin");
    fs::write(&executable, "build").unwrap();
    let deleted = fake.root.join("eitri-bin (deleted)");
    let engine = fake
        .with_transport_exe(Some(fake.root.join("transport")), Some(&deleted))
        .unwrap();
    engine.create(&tree()).unwrap();
    engine.pause(&tree().name).unwrap();
    for entry in fake.log() {
        assert_eq!(
            entry["env"]["EITRI_MUTAGEN_TRANSPORT_EXE"],
            executable.to_str().unwrap()
        );
        assert_ne!(entry["argv"], json!(["daemon", "stop"]));
    }
    fs::remove_file(&executable).unwrap();
    engine.pause(&tree().name).unwrap();
    assert_eq!(
        fake.log()
            .iter()
            .filter(|entry| entry["argv"] == json!(["daemon", "stop"]))
            .count(),
        1
    );
}

#[test]
fn daemon_stops_when_no_session_is_left() {
    let fake = Fake::new(vec![]);
    let engine = fake.engine().unwrap();
    assert!(!fake
        .log()
        .iter()
        .any(|entry| entry["argv"] == json!(["daemon", "stop"])));
    assert!(engine.stop_if_idle().unwrap());
    let log = fake.log();
    assert_eq!(log.last().unwrap()["argv"], json!(["daemon", "stop"]));
    assert!(log[1]["argv"].as_array().unwrap().contains(&json!("eitri-key")));
    let fake = Fake::new(vec![json!({"match":["sync","list"],"stdout_file":"list-ready.json"})]);
    assert!(!fake.engine().unwrap().stop_if_idle().unwrap());
    assert!(!fake
        .log()
        .iter()
        .any(|entry| entry["argv"] == json!(["daemon", "stop"])));
    let fake = Fake::new(vec![
        json!({"match":["sync","list"],"exit":1,"stderr":"daemon socket absent"}),
    ]);
    assert!(fake.engine().unwrap().stop_if_idle().is_err());
    assert!(!fake
        .log()
        .iter()
        .any(|entry| entry["argv"] == json!(["daemon", "stop"])));
}

#[test]
fn daemon_socket_over_103_bytes_is_refused_with_its_path() {
    let state = PathBuf::from(format!("/{}", "x".repeat(100)));
    let error = Daemon::prepare(&state, Some(Path::new("/example-mutagen")), None).unwrap_err();
    let socket_error = agent::socket_path::in_dir(&state.join("eitri/mutagen/daemon"), "daemon.sock").unwrap_err();
    assert_eq!(error.to_string(), socket_error.to_string());
    assert!(error.to_string().contains("103"));
}

#[test]
fn dropping_a_monitor_kills_and_reaps_it() {
    let fake = Fake::new(vec![
        json!({"match":["sync","monitor"],"stdout_file":"monitor-staging-batch.jsonl"}),
    ]);
    let engine = fake.engine().unwrap();
    let mut monitor = engine.monitor("eitri-0123456789abcdef-tree").unwrap();
    assert_eq!(monitor.next().unwrap().state, SyncState::Staging);
    let pid = fake.log().last().unwrap()["pid"].as_i64().unwrap() as libc::pid_t;
    drop(monitor);
    // SAFETY: waitpid with WNOHANG does not block or signal another process.
    assert_eq!(unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) }, -1);
    assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
}

#[test]
fn no_transport_removes_inherited_overrides_and_unknown_daemon_version_does_not_restart() {
    let fake = Fake::new(vec![]);
    fake.with_transport(None).unwrap();
    for entry in fake.log() {
        assert_eq!(entry["env"]["MUTAGEN_SSH_PATH"], Value::Null);
        assert_eq!(entry["env"]["EITRI_MUTAGEN_TRANSPORT_EXE"], Value::Null);
        assert_ne!(entry["argv"], json!(["daemon", "stop"]));
    }
    assert!(Daemon::prepare(&fake.root, None, Some(fake.root.join("bad:transport"))).is_err());
}

#[test]
fn watching_needs_a_completed_scan_and_successful_cycle() {
    let mut raw: Value = serde_json::from_slice(&fixture("list-ready.json")).unwrap();
    for endpoint in ["alpha", "beta"] {
        raw[0][endpoint]["scanned"] = json!(false);
        assert_eq!(
            parse_sessions(&serde_json::to_vec(&raw).unwrap()).unwrap()[0].state,
            SyncState::Scanning
        );
        raw[0][endpoint]["scanned"] = json!(true);
    }
    raw[0]["successfulCycles"] = json!(0);
    assert_eq!(
        parse_sessions(&serde_json::to_vec(&raw).unwrap()).unwrap()[0].state,
        SyncState::Scanning
    );
}

#[test]
fn ignore_patterns_are_csv_quoted_in_the_fake_argv() {
    let fake = Fake::new(vec![]);
    let mut spec = tree();
    spec.ignores = vec!["a,b".into(), "x\"y".into()];
    fake.engine().unwrap().create(&spec).unwrap();
    let log = fake.log();
    let args = log.last().unwrap()["argv"].as_array().unwrap();
    assert!(args.windows(2).any(|pair| pair == [json!("-i"), json!("\"a,b\"")]));
    assert!(args.windows(2).any(|pair| pair == [json!("-i"), json!("\"x\"\"y\"")]));
}

#[test]
fn tree_and_git_share_the_project_root_label_and_validate_it() {
    let tree = tree();
    let mut git = SessionSpec::git(tree.key.clone(), tree.laptop_root.clone(), "/srv/project".into());
    assert_eq!(tree.labels["eitri-root"], git.labels["eitri-root"]);
    assert_eq!(git.beta, Endpoint::Laptop(tree.laptop_root.join(".git")));
    create_args(&git).unwrap();
    git.laptop_root = git.laptop_root.join(".git");
    assert!(create_args(&git).unwrap_err().to_string().contains("project root"));
}

#[test]
fn git_ignores_symlinks_and_neighboring_staging_targets_beta() {
    let mut spec = SessionSpec::git(tree().key, "/srv/project-laptop".into(), "/srv/project".into());
    let args = arg_strings(create_args(&spec).unwrap());
    assert!(args.contains(&"--symlink-mode=ignore".into()));
    assert!(!args.iter().any(|arg| arg.starts_with("--stage-mode")));
    spec.staging = StagingMode::Neighboring;
    let args = arg_strings(create_args(&spec).unwrap());
    assert!(args.contains(&"--stage-mode-beta=neighboring".into()));
    assert!(!args.contains(&"--stage-mode-alpha=neighboring".into()));
    for (mode, expected) in [
        (SymlinkMode::PosixRaw, "posix-raw"),
        (SymlinkMode::Portable, "portable"),
    ] {
        spec.symlink_mode = Some(mode);
        assert!(arg_strings(create_args(&spec).unwrap()).contains(&format!("--symlink-mode={expected}")));
    }
}

#[test]
fn excluded_problem_and_conflict_counts_are_retained() {
    for role in [Role::Tree, Role::Git] {
        let raw = json!([{"labels":{"eitri-role":role.as_str()},"excludedConflicts":7,
            "alpha":{"excludedScanProblems":2,"excludedTransitionProblems":3},
            "beta":{"excludedScanProblems":5,"excludedTransitionProblems":11}}]);
        let status = parse_sessions(&serde_json::to_vec(&raw).unwrap()).unwrap().remove(0);
        assert_eq!(status.excluded_conflicts, 7);
        let (scan, transition) = if role == Role::Tree {
            ((2, 5), (3, 11))
        } else {
            ((5, 2), (11, 3))
        };
        assert_eq!(
            status.excluded_scan_problems,
            SideCounts {
                laptop: scan.0,
                server: scan.1
            }
        );
        assert_eq!(
            status.excluded_transition_problems,
            SideCounts {
                laptop: transition.0,
                server: transition.1
            }
        );
    }
}

#[test]
fn null_flags_counts_and_endpoints_default_without_making_a_session_ready() {
    let raw = json!([{"status":"watching","paused":null,"successfulCycles":null,"excludedConflicts":null,
        "alpha":null,"beta":null}]);
    let status = parse_sessions(&serde_json::to_vec(&raw).unwrap()).unwrap().remove(0);
    assert!(matches!(status.state, SyncState::Disconnected(_)));
    assert_eq!(status.excluded_conflicts, 0);
    for side in ["alpha", "beta"] {
        let mut raw = json!([{"status":"staging-alpha","paused":null,"successfulCycles":null}]);
        raw[0][side] = json!({"connected":null,"scanned":null,"excludedScanProblems":null,
            "excludedTransitionProblems":null,"stagingProgress":{"receivedSize":null,"expectedSize":null,
            "receivedFiles":null,"expectedFiles":null,"totalReceivedSize":null}});
        let status = parse_sessions(&serde_json::to_vec(&raw).unwrap()).unwrap().remove(0);
        assert_eq!(status.excluded_scan_problems, SideCounts::default());
        assert_eq!(status.excluded_transition_problems, SideCounts::default());
        assert_eq!(
            status.staging,
            Some(Staging {
                side: if side == "alpha" { Side::Laptop } else { Side::Server },
                expected_bytes: 0,
                received_bytes: 0,
                total_received_bytes: 0,
                received_files: 0,
                expected_files: 0,
                path: None,
            })
        );
    }
}

#[test]
fn null_collections_strings_and_missing_problem_paths_are_accepted() {
    let raw = json!([{"name":null,"labels":null,"status":null,"conflicts":null,
        "alpha":{"scanProblems":null,"transitionProblems":null}}]);
    let status = parse_sessions(&serde_json::to_vec(&raw).unwrap()).unwrap().remove(0);
    assert_eq!(status.state, SyncState::Other(String::new()));
    let raw = json!([{"alpha":{"scanProblems":[{}, {"path":null,"error":null}]},
        "conflicts":[{"alphaChanges":null,"betaChanges":null},
        {"root":null,"alphaChanges":[{"path":null,"new":{"kind":null}}]},
        {"alphaChanges":[{}]}]}]);
    let status = parse_sessions(&serde_json::to_vec(&raw).unwrap()).unwrap().remove(0);
    assert_eq!(status.problems.len(), 2);
    assert_eq!(status.problems[0].path, "");
    assert_eq!(status.problems[0].text, "");
    assert_eq!(status.conflicts.len(), 3);
}

#[test]
fn reconciling_is_applying_and_unknown_states_keep_the_raw_string() {
    for (raw, expected) in [
        ("reconciling", SyncState::Applying),
        ("future-state", SyncState::Other("future-state".into())),
    ] {
        let value = json!([{"status":raw}]);
        assert_eq!(
            parse_sessions(&serde_json::to_vec(&value).unwrap()).unwrap()[0].state,
            expected
        );
    }
}

#[test]
fn duplicate_names_are_refused_for_create_and_status() {
    let mut raw: Value = serde_json::from_slice(&fixture("list-ready.json")).unwrap();
    raw[0]["name"] = json!(tree().name);
    let fake = Fake::new(vec![json!({"match":["sync","list"],"stdout":raw.to_string()})]);
    let engine = fake.engine().unwrap();
    assert!(engine
        .create(&tree())
        .unwrap_err()
        .to_string()
        .contains("already exists"));
    assert!(!fake.log().iter().any(|entry| entry["argv"][1] == "create"));
    let duplicate = json!([raw[0], raw[0]]);
    let fake = Fake::new(vec![json!({"match":["sync","list"],"stdout":duplicate.to_string()})]);
    assert!(fake
        .engine()
        .unwrap()
        .session_status(&tree().name)
        .unwrap_err()
        .to_string()
        .contains("multiple"));
}

#[test]
fn absent_name_errors_refuse_list_and_disconnect_monitor() {
    let fake = Fake::new(vec![]);
    let engine = fake.engine().unwrap();
    let error = engine.session_status(&tree().name).unwrap_err();
    assert!(no_such_session(&error.to_string()));
    assert_eq!(
        engine.monitor(&tree().name).unwrap().next().unwrap().state,
        SyncState::Disconnected("no such session".into())
    );
    let output = engine.daemon.output(&words(&["daemon"])).unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Usage"));
}

#[test]
fn numeric_versions_accept_builds_and_newer_prereleases_but_not_floor_prereleases() {
    for (version, allowed) in [
        ("0.18.0-dev", false),
        ("0.19.0-dev", true),
        ("0.18.1+build.4", true),
        ("0.18.0+build", true),
        ("0.18.0", true),
        ("0.17.9+build", false),
        ("0.18.x", false),
        ("0.18.1.2", false),
    ] {
        let fake = Fake::new(vec![json!({"match":["version"],"stdout":version})]);
        assert_eq!(fake.engine().is_ok(), allowed, "{version}");
    }
}

#[test]
fn path_lookup_drops_relative_and_empty_entries_and_commands_use_the_data_directory() {
    assert_eq!(
        super::super::daemon::absolute_path_entries(std::ffi::OsStr::new(":relative:/usr/bin:.:/bin")),
        vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")]
    );
    let fake = Fake::new(vec![]);
    let engine = fake.engine().unwrap();
    engine.create(&tree()).unwrap();
    engine.status(&Labels::new()).unwrap();
    for entry in fake.log() {
        assert_eq!(entry["cwd"], engine.data_dir().to_str().unwrap());
        assert_eq!(entry["env"]["SSH_ASKPASS_REQUIRE"], Value::Null);
    }
}

#[test]
fn a_local_engine_refuses_server_endpoints_before_starting_a_daemon() {
    let fake = Fake::new(vec![]);
    let engine = fake.with_transport(None).unwrap();
    assert!(engine
        .create(&tree())
        .unwrap_err()
        .to_string()
        .contains("transport directory"));
    assert_eq!(fake.log().len(), 1);
    // Both constructors remain available with the transport requirement expressed in their signatures.
    let _local = MutagenEngine::open_local;
    let _remote = MutagenEngine::open;
}

#[test]
fn a_status_only_open_does_not_start_or_recover_an_absent_daemon() {
    let fake = Fake::new(vec![json!({"match":["sync","list"],"exit":1,
        "stderr":"unable to connect to daemon: connection timed out (is the daemon running?)"})]);
    let engine = fake.engine().unwrap();
    assert!(engine.status(&Labels::new()).is_err());
    let error = engine.session_status(&tree().name).unwrap_err();
    assert!(error.to_string().contains("unable to connect to daemon"));
    assert!(!fake.log().iter().any(|entry| entry["argv"][0] == "daemon"));
}

#[test]
fn operations_start_a_missing_daemon_and_retry_only_once() {
    for once in [true, false] {
        let fake = Fake::new(vec![json!({"match":["sync","resume"],"once":once,"exit":1,
            "stderr":"unable to connect to daemon: connection timed out (is the daemon running?)"})]);
        let engine = fake.engine().unwrap();
        assert_eq!(engine.resume(&tree().name).is_ok(), once);
        assert_eq!(
            fake.log()
                .iter()
                .filter(|entry| entry["argv"] == json!(["daemon", "start"]))
                .count(),
            1
        );
        assert_eq!(
            fake.log().iter().filter(|entry| entry["argv"][1] == "resume").count(),
            2
        );
    }
}

#[test]
fn daemon_recovery_releases_the_flock_before_the_retried_command() {
    for reason in ["version mismatch", "unable to connect to daemon:", "changed transport"] {
        let fake = Fake::new(vec![]);
        let transport = fake.root.join("transport");
        fs::create_dir(&transport).unwrap();
        fs::write(transport.join("ssh"), "first").unwrap();
        let engine = fake.with_transport(Some(transport.clone())).unwrap();
        engine.create(&tree()).unwrap();
        if reason == "changed transport" {
            fs::write(transport.join("ssh"), "replacement").unwrap();
        }
        let release = fake.root.join("release-resume");
        let mut replies: Vec<Value> =
            serde_json::from_slice(&fs::read(fake.root.join("script.json")).unwrap()).unwrap();
        replies.insert(0, json!({"match":["sync","resume"],"wait_file":release}));
        if reason != "changed transport" {
            replies.insert(
                0,
                json!({"match":["sync","resume"],"once":true,"exit":1,"stderr":reason}),
            );
        }
        fs::write(fake.root.join("script.json"), serde_json::to_vec(&replies).unwrap()).unwrap();
        thread::scope(|scope| {
            let running = scope.spawn(|| engine.resume(&tree().name));
            let expected = if reason == "changed transport" { 1 } else { 2 };
            let hang_guard = Instant::now() + Duration::from_secs(10);
            while fake.log().iter().filter(|entry| entry["argv"][1] == "resume").count() < expected {
                assert!(Instant::now() < hang_guard, "resume did not reach its retry");
                thread::sleep(Duration::from_millis(5));
            }
            let contender = agent::private_fs::PrivateLock::open(&engine.data_dir().join("eitri-daemon.lock")).unwrap();
            let available = contender.try_lock().unwrap();
            drop(contender);
            fs::write(&release, "").unwrap();
            running.join().unwrap().unwrap();
            assert!(available, "{reason}: recovery held the flock during resume");
        });
    }
}

#[test]
fn command_caps_use_the_kill_and_reap_path() {
    assert_eq!(CREATE_CAP, Duration::from_secs(120));
    assert_eq!(COMMAND_CAP, Duration::from_secs(30));
    for verb in ["create", "resume", "pause", "terminate", "list"] {
        let mut replies = vec![json!({"match":["sync",verb],"sleep":60})];
        if verb == "create" {
            replies.insert(0, json!({"match":["sync","list"],"stdout":"[]"}));
        }
        let fake = Fake::new(replies);
        let mut engine = fake.engine().unwrap();
        engine.test_cap = Some(Duration::from_millis(150));
        let error = match verb {
            "create" => engine.create(&tree()).unwrap_err(),
            "resume" => engine.resume(&tree().name).unwrap_err(),
            "pause" => engine.pause(&tree().name).unwrap_err(),
            "terminate" => engine.terminate(&tree().name).unwrap_err(),
            "list" => engine.status(&tree().labels).unwrap_err(),
            _ => unreachable!(),
        };
        assert!(error.to_string().contains("timed out"), "{verb}: {error}");
        let log = fake.log();
        let last = log.last().unwrap();
        assert_eq!(last["argv"][1], verb, "timeout must reach the requested command");
        let pid = last["pid"].as_i64().unwrap() as libc::pid_t;
        // SAFETY: signal zero probes existence without sending a signal.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }
}

fn wait_for_argv(fake: &Fake, verb: &str) {
    let hang_guard = Instant::now() + Duration::from_secs(10);
    while !fake.log().iter().any(|entry| entry["argv"][1] == verb) {
        assert!(Instant::now() < hang_guard, "fake did not reach {verb}");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_stalled_create_or_resume_does_not_block_pause_or_status() {
    for verb in ["create", "resume"] {
        let fake = Fake::new(vec![]);
        let release = fake.root.join("release");
        let mut replies: Vec<Value> =
            serde_json::from_slice(&fs::read(fake.root.join("script.json")).unwrap()).unwrap();
        replies.insert(0, json!({"match":["sync",verb],"wait_file":release}));
        fs::write(fake.root.join("script.json"), serde_json::to_vec(&replies).unwrap()).unwrap();
        let engine = fake.engine().unwrap();
        thread::scope(|scope| {
            let stalled = scope.spawn(|| {
                if verb == "create" {
                    engine.create(&tree())
                } else {
                    engine.resume(&tree().name)
                }
            });
            wait_for_argv(&fake, verb);
            let (tx, rx) = std::sync::mpsc::channel();
            let engine = &engine;
            scope.spawn(move || {
                let result = engine
                    .pause(&tree().name)
                    .and_then(|_| engine.status(&Labels::new()).map(|_| ()));
                tx.send(result).unwrap();
            });
            let result = rx.recv_timeout(Duration::from_secs(5));
            fs::write(&release, "").unwrap();
            stalled.join().unwrap().unwrap();
            result
                .expect("pause/status remained blocked by another ordinary command")
                .unwrap();
        });
    }
}

#[test]
fn start_create_and_idle_stop_hold_the_same_data_directory_flock() {
    for verb in ["create", "list", "start"] {
        let fake = Fake::new(vec![]);
        let release = fake.root.join("release");
        let mut replies: Vec<Value> =
            serde_json::from_slice(&fs::read(fake.root.join("script.json")).unwrap()).unwrap();
        replies.insert(
            0,
            json!({"match":[if verb == "start" {"daemon"} else {"sync"},verb],"wait_file":release,"stdout":if verb == "list" {"[]"} else {""}}),
        );
        fs::write(fake.root.join("script.json"), serde_json::to_vec(&replies).unwrap()).unwrap();
        let engine = fake.engine().unwrap();
        thread::scope(|scope| {
            let running = scope.spawn(|| {
                if verb != "list" {
                    engine.create(&tree())
                } else {
                    engine.stop_if_idle().map(|_| ())
                }
            });
            wait_for_argv(&fake, verb);
            let contender = agent::private_fs::PrivateLock::open(&engine.data_dir().join("eitri-daemon.lock")).unwrap();
            let excluded = !contender.try_lock().unwrap();
            drop(contender);
            fs::write(&release, "").unwrap();
            running.join().unwrap().unwrap();
            assert!(excluded, "{verb} did not hold the shared data-directory flock");
        });
    }
}

#[test]
fn flush_timeout_detaches_drains_held_by_grandchildren_and_huge_caps_saturate() {
    let fake = Fake::new(vec![]);
    let release = fake.root.join("release-grandchild");
    fs::write(
        fake.root.join("script.json"),
        serde_json::to_vec(&json!([
            {"match":["sync","flush"],"grandchild":release,"sleep":60},
            {"match":["version"],"stdout_file":"version.txt"}
        ]))
        .unwrap(),
    )
    .unwrap();
    let engine = fake.engine().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(|| {
            tx.send(engine.flush(&tree().name, Duration::from_millis(500))).unwrap();
        });
        let outcome = rx.recv_timeout(Duration::from_secs(10));
        fs::write(&release, "").unwrap();
        let hang_guard = Instant::now() + Duration::from_secs(10);
        while !release.with_file_name("release-grandchild.done").exists() {
            assert!(Instant::now() < hang_guard, "grandchild did not acknowledge release");
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(outcome.expect("pipe drain joined a grandchild"), FlushOutcome::TimedOut);
    });
    let fake = Fake::new(vec![]);
    assert_eq!(
        fake.engine().unwrap().flush(&tree().name, Duration::MAX),
        FlushOutcome::Flushed
    );
}

#[test]
fn a_status_version_mismatch_restarts_but_does_not_probe_a_daemon_version() {
    let fake = Fake::new(vec![
        json!({"match":["sync","list"],"once":true,"exit":1,"stderr":"version mismatch"}),
    ]);
    let engine = fake.engine().unwrap();
    assert!(engine.status(&Labels::new()).unwrap().is_empty());
    let log = fake.log();
    assert_eq!(
        log.iter()
            .filter(|entry| entry["argv"] == json!(["daemon", "stop"]))
            .count(),
        1
    );
    assert_eq!(
        log.iter()
            .filter(|entry| entry["argv"] == json!(["daemon", "start"]))
            .count(),
        1
    );
    assert!(!log.iter().any(|entry| entry["argv"] == json!(["daemon"])));
}

#[test]
fn flush_cap_also_bounds_waiting_for_exclusive_daemon_control() {
    let fake = Fake::new(vec![]);
    let engine = fake.engine().unwrap();
    let _exclusive = engine.control.write().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(|| {
            tx.send(engine.flush(&tree().name, Duration::from_millis(20))).unwrap();
        });
        let result = rx.recv_timeout(Duration::from_secs(10));
        drop(_exclusive);
        assert_eq!(
            result.expect("flush waited beyond its control deadline"),
            FlushOutcome::TimedOut
        );
    });
    assert_eq!(
        fake.log().len(),
        1,
        "a flush child must not run while daemon control is exclusive"
    );
}

#[test]
fn resuming_existing_sessions_refreshes_a_changed_transport_without_starting_on_open() {
    let fake = Fake::new(vec![]);
    let transport = fake.root.join("transport");
    fs::create_dir(&transport).unwrap();
    fs::write(transport.join("ssh"), "first").unwrap();
    fake.with_transport(Some(transport.clone()))
        .unwrap()
        .create(&tree())
        .unwrap();
    let before = fake.log().len();
    fs::write(transport.join("ssh"), "replacement").unwrap();
    let reopened = fake.with_transport(Some(transport)).unwrap();
    assert_eq!(fake.log().len(), before + 1, "open must only query the client version");
    reopened.resume(&tree().name).unwrap();
    assert_eq!(fake.log()[before + 1]["argv"], json!(["daemon", "stop"]));
    assert_eq!(fake.log()[before + 2]["argv"], json!(["daemon", "start"]));
}

#[test]
fn flush_deadline_also_kills_and_reaps_a_recovery_command() {
    let fake = Fake::new(vec![
        json!({"match":["sync","flush"],"once":true,"exit":1,"stderr":"version mismatch"}),
        json!({"match":["daemon","start"],"sleep":60}),
    ]);
    let engine = fake.engine().unwrap();
    assert_eq!(
        engine.flush(&tree().name, Duration::from_millis(500)),
        FlushOutcome::TimedOut
    );
    let pid = fake.log().last().unwrap()["pid"].as_i64().unwrap() as libc::pid_t;
    // SAFETY: signal zero only checks whether the pid still exists.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
}

#[test]
fn changed_transport_can_start_when_the_old_daemon_is_already_gone() {
    let fake = Fake::new(vec![json!({"match":["daemon","stop"],"exit":1,"stderr":
        "unable to connect to daemon: connection timed out (is the daemon running?)"})]);
    let transport = fake.root.join("transport");
    fs::create_dir(&transport).unwrap();
    fs::write(transport.join("ssh"), "first").unwrap();
    fake.with_transport(Some(transport.clone()))
        .unwrap()
        .create(&tree())
        .unwrap();
    fs::write(transport.join("ssh"), "changed").unwrap();
    fake.with_transport(Some(transport))
        .unwrap()
        .resume(&tree().name)
        .unwrap();
}
