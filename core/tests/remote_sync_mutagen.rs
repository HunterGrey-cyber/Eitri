//! Opt-in local-endpoint checks against a user-installed Mutagen.

use eitri_core::remote_sync::engine::*;
use eitri_core::remote_sync::mutagen::MutagenEngine;
use eitri_core::remote_sync::SyncKey;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

struct Scratch {
    root: PathBuf,
    binary: PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let result = Command::new(&self.binary)
            .args(["daemon", "stop"])
            .env("MUTAGEN_DATA_DIRECTORY", self.root.join("eitri/mutagen"))
            .env("MUTAGEN_DISABLE_AUTOSTART", "1")
            .current_dir(self.root.join("eitri/mutagen"))
            .env_remove("SSH_ASKPASS")
            .env_remove("SSH_ASKPASS_REQUIRE")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("MUTAGEN_SSH_PATH")
            .env_remove("EITRI_MUTAGEN_TRANSPORT_EXE")
            .stdin(Stdio::null())
            .output();
        match result {
            Ok(output) if !output.status.success() => eprintln!(
                "private Mutagen daemon cleanup failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
            Err(error) => eprintln!("private Mutagen daemon cleanup failed: {error}"),
            _ => {}
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Real {
    engine: MutagenEngine,
    scratch: Scratch,
    laptop: PathBuf,
    server: PathBuf,
}

impl Real {
    fn open() -> Self {
        let Some(binary) = std::env::var_os("EITRI_TEST_MUTAGEN") else {
            panic!("real Mutagen test requires EITRI_TEST_MUTAGEN to name an absolute binary path");
        };
        let binary = PathBuf::from(binary);
        assert!(
            binary.is_absolute(),
            "EITRI_TEST_MUTAGEN must name an absolute binary path"
        );
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("rm-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        fs::create_dir_all(&root).unwrap();
        let scratch = Scratch { root, binary };
        let engine = MutagenEngine::open_local(&scratch.root, Some(&scratch.binary)).unwrap();
        let laptop = scratch.root.join("laptop");
        let server = scratch.root.join("server");
        fs::create_dir(&laptop).unwrap();
        fs::create_dir(&server).unwrap();
        Self {
            engine,
            scratch,
            laptop,
            server,
        }
    }

    fn spec(&self) -> SessionSpec {
        let key = SyncKey::new("SHA256:example", &self.server);
        let mut spec = SessionSpec::tree(key, self.laptop.clone(), self.server.clone());
        spec.beta = Endpoint::Laptop(self.server.clone());
        spec
    }

    fn flush(&self, name: &str) {
        assert_eq!(self.engine.flush(name, Duration::from_secs(30)), FlushOutcome::Flushed);
    }
}

#[test]
#[ignore = "requires EITRI_TEST_MUTAGEN and permission to bind a private daemon socket"]
fn real_two_way_safe_conflict_is_reported_with_both_kinds() {
    let real = Real::open();
    fs::write(real.laptop.join("both.txt"), "base\n").unwrap();
    fs::write(real.laptop.join("fvd.txt"), "base\n").unwrap();
    let spec = real.spec();
    real.engine.create(&spec).unwrap();
    real.flush(&spec.name);
    real.engine.pause(&spec.name).unwrap();
    fs::write(real.laptop.join("both.txt"), "laptop edit\n").unwrap();
    fs::write(real.server.join("both.txt"), "server edit\n").unwrap();
    fs::write(real.laptop.join("fvd.txt"), "laptop file\n").unwrap();
    fs::remove_file(real.server.join("fvd.txt")).unwrap();
    fs::create_dir(real.server.join("fvd.txt")).unwrap();
    fs::write(real.server.join("fvd.txt/child"), "server directory\n").unwrap();
    real.engine.resume(&spec.name).unwrap();
    real.flush(&spec.name);
    let status = real.engine.session_status(&spec.name).unwrap();
    let both = status
        .conflicts
        .iter()
        .find(|c| c.path == "both.txt")
        .expect("two-sided file conflict");
    assert_eq!(both.laptop, EntryKind::File);
    assert_eq!(both.server, EntryKind::File);
    let fvd = status
        .conflicts
        .iter()
        .find(|c| c.path == "fvd.txt")
        .expect("file/directory conflict");
    assert_eq!(fvd.laptop, EntryKind::File);
    assert_eq!(fvd.server, EntryKind::Directory);
}

#[test]
#[ignore = "requires EITRI_TEST_MUTAGEN and permission to bind a private daemon socket"]
fn real_flush_returns_after_a_save() {
    let real = Real::open();
    let spec = real.spec();
    fs::write(real.laptop.join("save.txt"), "initial\n").unwrap();
    real.engine.create(&spec).unwrap();
    real.flush(&spec.name);
    fs::write(real.laptop.join("save.txt"), "saved\n").unwrap();
    real.flush(&spec.name);
    assert_eq!(fs::read(real.server.join("save.txt")).unwrap(), b"saved\n");
    assert_eq!(
        real.engine.session_status(&spec.name).unwrap().state,
        SyncState::Watching
    );
}

#[test]
#[ignore = "requires EITRI_TEST_MUTAGEN and permission to bind a private daemon socket"]
fn real_pause_holds_changes_and_resume_delivers_them() {
    let real = Real::open();
    let spec = real.spec();
    fs::write(real.laptop.join("pause.txt"), "before\n").unwrap();
    real.engine.create(&spec).unwrap();
    real.flush(&spec.name);
    real.engine.pause(&spec.name).unwrap();
    fs::write(real.laptop.join("pause.txt"), "after\n").unwrap();
    assert_eq!(real.engine.session_status(&spec.name).unwrap().state, SyncState::Paused);
    assert_eq!(
        real.engine.flush(&spec.name, Duration::from_secs(10)),
        FlushOutcome::Failed("unable to flush session: session is paused".into())
    );
    assert_eq!(fs::read(real.server.join("pause.txt")).unwrap(), b"before\n");
    real.engine.resume(&spec.name).unwrap();
    real.flush(&spec.name);
    assert_eq!(fs::read(real.server.join("pause.txt")).unwrap(), b"after\n");
}

#[test]
#[ignore = "requires EITRI_TEST_MUTAGEN and permission to bind a private daemon socket"]
fn real_status_by_label_lists_only_this_project() {
    let real = Real::open();
    let spec = real.spec();
    real.engine.create(&spec).unwrap();
    real.flush(&spec.name);
    let other_laptop = real.scratch.root.join("other-laptop");
    let other_server = real.scratch.root.join("other-server");
    fs::create_dir(&other_laptop).unwrap();
    fs::create_dir(&other_server).unwrap();
    let mut other = SessionSpec::tree(
        SyncKey::new("SHA256:other", &other_server),
        other_laptop,
        other_server.clone(),
    );
    other.beta = Endpoint::Laptop(other_server);
    real.engine.create(&other).unwrap();
    real.flush(&other.name);
    let statuses = real
        .engine
        .status(&Labels::from([(
            "eitri-root".into(),
            spec.labels["eitri-root"].clone(),
        )]))
        .unwrap();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].name, spec.name);
    assert_eq!(statuses[0].state, SyncState::Watching);
    assert!(!real.engine.stop_if_idle().unwrap());
    real.engine.terminate(&spec.name).unwrap();
    real.engine.terminate(&other.name).unwrap();
    assert!(real.engine.stop_if_idle().unwrap());
}

fn git(directory: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "tag.gpgSign=false",
        ])
        .args(args)
        .current_dir(directory)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Example")
        .env("GIT_AUTHOR_EMAIL", "example@example.invalid")
        .env("GIT_COMMITTER_NAME", "Example")
        .env("GIT_COMMITTER_EMAIL", "example@example.invalid")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn objects(directory: &Path) -> Vec<PathBuf> {
    fn visit(root: &Path, path: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, out);
            } else {
                out.push(path.strip_prefix(root).unwrap().to_owned());
            }
        }
    }
    let mut paths = Vec::new();
    visit(directory, &directory.join("objects"), &mut paths);
    paths.sort();
    paths
}

#[test]
#[ignore = "requires EITRI_TEST_MUTAGEN, git, and permission to bind a private daemon socket"]
fn real_replica_with_server_alpha_keeps_every_server_ref_and_object() {
    let real = Real::open();
    git(&real.server, &["init", "--initial-branch=main"]);
    fs::write(real.server.join("main.txt"), "first\n").unwrap();
    git(&real.server, &["add", "main.txt"]);
    git(&real.server, &["commit", "-m", "first"]);
    git(&real.server, &["branch", "topic"]);
    git(&real.server, &["tag", "-a", "v1", "-m", "first tag"]);
    git(&real.laptop, &["init", "--initial-branch=main"]);
    let laptop_git = real.laptop.join(".git");
    let server_git = real.server.join(".git");
    let mut spec = SessionSpec::git(
        SyncKey::new("SHA256:example", &real.server),
        real.laptop.clone(),
        real.server.clone(),
    );
    spec.alpha = Endpoint::Laptop(server_git.clone());
    spec.ignores = vec!["/config".into(), "/hooks".into(), "/modules".into(), "*.lock".into()];
    real.engine.create(&spec).unwrap();
    real.flush(&spec.name);
    fs::write(real.server.join("main.txt"), "second\n").unwrap();
    git(&real.server, &["add", "main.txt"]);
    git(&real.server, &["commit", "-m", "second"]);
    git(&real.server, &["update-ref", "refs/example/private", "HEAD"]);
    fs::write(laptop_git.join("laptop-only"), "must be removed\n").unwrap();
    real.flush(&spec.name);
    for directory in [&real.server, &real.laptop] {
        git(directory, &["fsck", "--full"]);
    }
    assert_eq!(
        git(&real.server, &["for-each-ref"]),
        git(&real.laptop, &["for-each-ref"])
    );
    let server_objects = objects(&server_git);
    assert!(!server_objects.is_empty());
    assert_eq!(server_objects, objects(&laptop_git));
    for path in server_objects {
        assert_eq!(
            fs::read(server_git.join(&path)).unwrap(),
            fs::read(laptop_git.join(&path)).unwrap()
        );
    }
    assert!(!laptop_git.join("laptop-only").exists());
}

#[test]
#[ignore = "requires EITRI_TEST_MUTAGEN and permission to bind a private daemon socket"]
fn real_ignore_paths_round_trip_commas_and_quotes() {
    let real = Real::open();
    let mut spec = real.spec();
    spec.ignores.extend(["a,b".into(), "x\"y".into()]);
    real.engine.create(&spec).unwrap();
    let output = Command::new(&real.scratch.binary)
        .args(["sync", "list", "--template", "{{json .}}", &spec.name])
        .env("MUTAGEN_DATA_DIRECTORY", real.engine.data_dir())
        .env("MUTAGEN_DISABLE_AUTOSTART", "1")
        .env_remove("SSH_ASKPASS")
        .env_remove("SSH_ASKPASS_REQUIRE")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("MUTAGEN_SSH_PATH")
        .env_remove("EITRI_MUTAGEN_TRANSPORT_EXE")
        .current_dir(real.engine.data_dir())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let raw: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(raw[0]["ignore"]["paths"], serde_json::json!([".git", "a,b", "x\"y"]));
}

#[test]
fn explicitly_requested_real_tests_require_a_binary() {
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "real_flush_returns_after_a_save", "--nocapture"])
        .env_remove("EITRI_TEST_MUTAGEN")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("real Mutagen test requires EITRI_TEST_MUTAGEN"));
}
