//! `probe`: start the sidecar at a path, ask it for its handshake, check it against what this Eitri cannot do
//! without, and end it. The sidecars here are the stand-in (`tests/support/sidecar_standin.rs`) scripted through
//! `EITRI_STANDIN_*`; no real `claude` runs.
//!
//! Every stand-in is started through a small launcher that sets its script, since the tests share this
//! process's environment.
#![cfg(target_os = "linux")]

use agent::providers::claude_sidecar::probe;
use agent::REQUIRED_CAPABILITIES;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// How long the launcher may take to become runnable before the test is called hung. Not a speed bound: the
/// wait ends as soon as the launcher runs.
const HANG: Duration = Duration::from_secs(30);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("sidecar-probe-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// A program in `dir` that runs the stand-in with `vars` in its environment.
fn launcher(dir: &Path, name: &str, vars: &[(&str, &str)]) -> PathBuf {
    let path = dir.join(name);
    let mut script = String::from("#!/bin/sh\n[ \"$1\" = ready ] && exit 0\n");
    for (name, value) in vars {
        script.push_str(&format!("export {name}={}\n", quoted(value)));
    }
    script.push_str(&format!(
        "exec {} \"$@\"\n",
        quoted(env!("CARGO_BIN_EXE_eitri-sidecar-standin"))
    ));
    agent::private_fs::write_private(&path, script.as_bytes()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    // A child forked by another test at the moment of the write holds the file open for writing until it
    // reaches its own exec, and until then this file does not run.
    let started = Instant::now();
    while Command::new(&path).arg("ready").status().is_err() {
        assert!(started.elapsed() < HANG, "the launcher never ran");
        std::thread::sleep(Duration::from_millis(1));
    }
    path
}

/// What a sidecar of this build's own pin advertises, with one name taken out (none, for `""`).
fn advertised_without(absent: &str) -> String {
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/handshake/3.24.json");
    let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
    let kept: Vec<String> = json["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry.get("when").is_none())
        .map(|entry| entry["name"].as_str().unwrap().to_string())
        .filter(|name| name != absent)
        .collect();
    kept.join(" ")
}

#[test]
fn a_sidecar_that_answers_with_everything_required_is_reported_as_it_said() {
    let dir = Scratch::new();
    let capabilities = advertised_without("");
    let sidecar = launcher(
        &dir.0,
        "good",
        &[
            ("EITRI_STANDIN_CAPABILITIES", capabilities.as_str()),
            ("EITRI_STANDIN_MINOR", "24"),
        ],
    );
    let report = probe(&sidecar).unwrap_or_else(|reason| panic!("the probe refused a good sidecar: {reason}"));
    assert_eq!(report.sidecar_version, "stand-in");
    assert_eq!(report.protocol_major, 3);
    assert_eq!(report.protocol_minor, 24);
}

#[test]
fn a_higher_minor_than_this_build_knows_is_reported_as_it_is() {
    let dir = Scratch::new();
    let capabilities = advertised_without("");
    let sidecar = launcher(
        &dir.0,
        "later",
        &[
            ("EITRI_STANDIN_CAPABILITIES", capabilities.as_str()),
            ("EITRI_STANDIN_MINOR", "99"),
        ],
    );
    let report = probe(&sidecar).unwrap_or_else(|reason| panic!("the probe refused a later minor: {reason}"));
    assert_eq!(report.protocol_minor, 99);
}

#[test]
fn each_missing_required_capability_is_an_error_naming_it() {
    let dir = Scratch::new();
    for required in REQUIRED_CAPABILITIES {
        let capabilities = advertised_without(required);
        let sidecar = launcher(
            &dir.0,
            &format!("without-{required}"),
            &[
                ("EITRI_STANDIN_CAPABILITIES", capabilities.as_str()),
                ("EITRI_STANDIN_MINOR", "24"),
            ],
        );
        let reason = probe(&sidecar)
            .err()
            .unwrap_or_else(|| panic!("a sidecar without {required} passed the probe"));
        assert!(reason.contains(required), "the reason names {required}: {reason}");
    }
}

#[test]
fn a_sidecar_that_never_binds_is_an_error_with_a_reason() {
    let dir = Scratch::new();
    let sidecar = launcher(&dir.0, "silent", &[("EITRI_STANDIN_NEVER_BIND", "1")]);
    let reason = probe(&sidecar)
        .err()
        .expect("a sidecar that never binds fails the probe");
    assert!(!reason.trim().is_empty(), "the failure says something");
}

#[test]
fn a_sidecar_that_refuses_before_binding_is_an_error_quoting_its_refusal() {
    let dir = Scratch::new();
    let refusal = "refusing to start: host claude CLI version 3.0.0 is outside this sidecar's supported range";
    let words = format!("{refusal}\n");
    let sidecar = launcher(&dir.0, "refuses", &[("EITRI_STANDIN_REFUSE_BIND", words.as_str())]);
    let reason = probe(&sidecar).err().expect("a refusal fails the probe");
    assert!(reason.contains(refusal), "{reason}");
}

#[test]
fn a_path_with_no_program_is_an_error_with_a_reason() {
    let dir = Scratch::new();
    let reason = probe(&dir.0.join("no-such-sidecar"))
        .err()
        .expect("a missing program fails the probe");
    assert!(!reason.trim().is_empty(), "the failure says something");
}
