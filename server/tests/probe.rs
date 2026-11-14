//! `eitri-server probe`, run as the binary a laptop would run over ssh.
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use eitri_server_proto::probe::{ProbeReport, RootState};

const BINARY: &str = env!("CARGO_BIN_EXE_eitri-server");

struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        static SERIAL: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "pb{:x}-{:x}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        agent::private_fs::create_private_dir_all(&root, &root).unwrap();
        Self(root)
    }

    fn probe(&self, root: Option<&Path>) -> ProbeReport {
        let mut command = Command::new(BINARY);
        command
            .arg("probe")
            .env_clear()
            .env("HOME", &self.0)
            .env("PATH", "/usr/bin:/bin")
            .env("XDG_DATA_HOME", self.0.join("data"))
            .env("XDG_STATE_HOME", self.0.join("state"));
        if let Some(root) = root {
            command
                .arg("--args-hex")
                .arg(eitri_server_proto::args_hex::encode_args(&[root.as_os_str()]));
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        ProbeReport::parse(std::str::from_utf8(&output.stdout).unwrap()).unwrap()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn probe_output_parses() {
    let sandbox = Sandbox::new();
    let project = sandbox.0.join("proj/sub");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir(sandbox.0.join("proj/.stfolder")).unwrap();
    let report = sandbox.probe(Some(&project));
    assert_eq!(report.uname.split(' ').next(), Some("Linux"));
    assert!(report.home.is_absolute());
    assert_eq!(report.data_dir, sandbox.0.join("data/eitri-server"));
    assert_eq!(report.state_dir, sandbox.0.join("state/eitri-server"));
    // An empty passwd shell field is legal (it means /bin/sh) and is reported as it is written.
    assert!(report.login_path.split(':').any(|entry| entry.starts_with('/')));
    assert!(report.glibc.is_some_and(|(major, _)| major >= 2));
    assert!(report.free_bytes.is_some_and(|bytes| bytes > 0));
    assert_eq!(report.root, RootState::Ok);
    assert_eq!(
        report.sync_marker,
        Some(std::fs::canonicalize(sandbox.0.join("proj/.stfolder")).unwrap())
    );
    // The report is what the laptop parses: rendering it again gives the same text.
    assert_eq!(ProbeReport::parse(&report.render()).unwrap(), report);
}

#[test]
fn probe_looks_for_the_sync_marker_above_the_real_directory() {
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(sandbox.0.join("data/work/proj")).unwrap();
    std::fs::create_dir(sandbox.0.join("data/.stfolder")).unwrap();
    std::fs::create_dir(sandbox.0.join("home")).unwrap();
    std::os::unix::fs::symlink(sandbox.0.join("data/work"), sandbox.0.join("home/work")).unwrap();
    let report = sandbox.probe(Some(&sandbox.0.join("home/work/proj")));
    assert_eq!(report.root, RootState::Ok);
    assert_eq!(
        report.sync_marker,
        Some(std::fs::canonicalize(sandbox.0.join("data/.stfolder")).unwrap())
    );
}

#[test]
fn probe_reports_a_missing_root() {
    let sandbox = Sandbox::new();
    let report = sandbox.probe(Some(&sandbox.0.join("absent")));
    assert_eq!(report.root, RootState::Missing);
    assert_eq!(report.sync_marker, None);
    let file = sandbox.0.join("file");
    std::fs::write(&file, b"").unwrap();
    assert_eq!(sandbox.probe(Some(&file)).root, RootState::NotADirectory);
}

#[test]
fn probe_refuses_a_root_a_sidecar_start_refuses_and_extra_arguments() {
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(sandbox.0.join("a/proj")).unwrap();
    // A sidecar start takes only a root in normal form; these name an existing directory all the same.
    let trailing = format!("{}/", sandbox.0.join("a/proj").display());
    let dotdot = sandbox.0.join("a/proj/../proj");
    for args in [
        vec![OsStr::new("relative")],
        vec![OsStr::new("/srv/project"), OsStr::new("/srv/other")],
        vec![OsStr::new(&trailing)],
        vec![dotdot.as_os_str()],
    ] {
        let output = Command::new(BINARY)
            .arg("probe")
            .arg("--args-hex")
            .arg(eitri_server_proto::args_hex::encode_args(&args))
            .env_clear()
            .env("HOME", &sandbox.0)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
    }
}
