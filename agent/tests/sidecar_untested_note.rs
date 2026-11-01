//! The two sentences that point at `eitri sidecar`: one after the compatibility diagnostic of a `claude` that is
//! inside the supported range but untested, one on a sidecar's refusal of a `claude`. Both appear only where the
//! command exists, that is, with `eitri-setup` beside the running binary. The sidecars are the stand-in
//! (`tests/support/sidecar_standin.rs`); no real `claude` runs.
#![cfg(target_os = "linux")]

use agent::providers::claude_sidecar::untested_note_applies;
use agent::{AgentProvider, ClaudeSidecarProvider, REFUSED_CLAUDE_NOTE, UNTESTED_CLAUDE_NOTE};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// How long the launcher may take to become runnable before the test is called hung. Not a speed bound.
const HANG: Duration = Duration::from_secs(30);

const DIAGNOSTIC: &str =
    "claude-sidecar: host CLI version diagnostic: claude 2.1.320 is inside the supported range but was not tested with this sidecar";

const REFUSAL: &str =
    "refusing to start: host claude CLI version 3.0.0 is outside this sidecar's supported range (>=2.1.0 <2.2.0)";

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("sidecar-note-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    /// A directory standing as the one an installed `eitri` runs from: with `eitri-setup` in it, or without.
    fn exe_dir(&self, name: &str, with_setup: bool) -> PathBuf {
        let dir = self.0.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        if with_setup {
            std::fs::write(dir.join("eitri-setup"), b"#!/bin/sh\n").unwrap();
        }
        dir
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

/// A program in `dir` that runs `before`, then the stand-in with `vars` in its environment.
fn launcher(dir: &Path, name: &str, vars: &[(&str, &str)], before: &str) -> PathBuf {
    let path = dir.join(name);
    let mut script = String::from("#!/bin/sh\n[ \"$1\" = ready ] && exit 0\n");
    script.push_str(before);
    script.push('\n');
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

/// A sidecar that starts, binds and answers, having said `DIAGNOSTIC` on stderr first.
fn untested_claude(dir: &Path) -> PathBuf {
    launcher(dir, "untested", &[], &format!("echo {} >&2", quoted(DIAGNOSTIC)))
}

/// A sidecar that says `stderr` and ends before it binds.
fn failing_with(dir: &Path, name: &str, stderr: &str) -> PathBuf {
    let words = format!("{stderr}\n");
    launcher(dir, name, &[("EITRI_STANDIN_REFUSE_BIND", words.as_str())], "")
}

fn instance() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn lines(texts: &[&str]) -> Vec<String> {
    texts.iter().map(|text| text.to_string()).collect()
}

#[test]
fn the_notes_say_what_they_point_at() {
    assert_eq!(
        UNTESTED_CLAUDE_NOTE,
        "If something looks wrong, a newer sidecar may fix it: eitri sidecar check"
    );
    assert_eq!(
        REFUSED_CLAUDE_NOTE,
        "A newer sidecar may support this claude: eitri sidecar update"
    );
}

#[test]
fn the_untested_note_applies_to_the_diagnostic_where_the_command_exists() {
    let dir = Scratch::new();
    let with = dir.exe_dir("with", true);
    let without = dir.exe_dir("without", false);
    let stderr = lines(&["claude-sidecar: account work", DIAGNOSTIC]);
    assert!(untested_note_applies(&stderr, Some(with.as_path())));
    assert!(
        !untested_note_applies(&stderr, Some(without.as_path())),
        "no eitri-setup beside"
    );
    assert!(!untested_note_applies(&stderr, None), "no directory to look in");
    assert!(
        !untested_note_applies(&stderr, Some(dir.0.join("not-a-directory").as_path())),
        "a directory that is not there has nothing beside it"
    );
}

#[test]
fn the_untested_note_does_not_apply_without_the_diagnostic() {
    let dir = Scratch::new();
    let with = dir.exe_dir("with", true);
    assert!(!untested_note_applies(
        &lines(&["claude-sidecar: account work", "stand-in sidecar: started"]),
        Some(with.as_path())
    ));
    assert!(!untested_note_applies(&[], Some(with.as_path())));
}

#[test]
fn the_note_follows_the_diagnostic_once() {
    let dir = Scratch::new();
    let exe_dir = dir.exe_dir("with", true);
    let provider =
        ClaudeSidecarProvider::connect_to_program_in(&instance(), &untested_claude(&dir.0), &exe_dir).unwrap();
    let diagnostics = provider.info().startup_diagnostics;
    let at = diagnostics
        .iter()
        .position(|line| line.contains("CLI version diagnostic"))
        .unwrap_or_else(|| panic!("the diagnostic is shown: {diagnostics:?}"));
    assert_eq!(
        diagnostics.get(at + 1).map(String::as_str),
        Some(UNTESTED_CLAUDE_NOTE),
        "the note comes right after the diagnostic: {diagnostics:?}"
    );
    assert_eq!(
        diagnostics.iter().filter(|line| *line == UNTESTED_CLAUDE_NOTE).count(),
        1,
        "{diagnostics:?}"
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|line| line.contains("CLI version diagnostic"))
            .count(),
        1,
        "the diagnostic itself is not repeated: {diagnostics:?}"
    );
}

#[test]
fn no_note_follows_the_diagnostic_where_there_is_no_command() {
    let dir = Scratch::new();
    let exe_dir = dir.exe_dir("without", false);
    let provider =
        ClaudeSidecarProvider::connect_to_program_in(&instance(), &untested_claude(&dir.0), &exe_dir).unwrap();
    let diagnostics = provider.info().startup_diagnostics;
    assert!(
        diagnostics.iter().any(|line| line.contains("CLI version diagnostic")),
        "the diagnostic is still shown: {diagnostics:?}"
    );
    assert!(
        !diagnostics.iter().any(|line| line == UNTESTED_CLAUDE_NOTE),
        "{diagnostics:?}"
    );
}

#[test]
fn a_session_with_nothing_to_report_gets_no_note() {
    let dir = Scratch::new();
    let exe_dir = dir.exe_dir("with", true);
    let quiet = launcher(&dir.0, "quiet", &[], "");
    let provider = ClaudeSidecarProvider::connect_to_program_in(&instance(), &quiet, &exe_dir).unwrap();
    let diagnostics = provider.info().startup_diagnostics;
    assert!(
        !diagnostics.iter().any(|line| line == UNTESTED_CLAUDE_NOTE),
        "{diagnostics:?}"
    );
}

#[test]
fn a_start_that_fails_with_the_diagnostic_in_its_stderr_carries_the_note() {
    let dir = Scratch::new();
    let exe_dir = dir.exe_dir("with", true);
    let sidecar = failing_with(&dir.0, "dies", &format!("{DIAGNOSTIC}\nthe sidecar went down"));
    let error = ClaudeSidecarProvider::connect_to_program_in(&instance(), &sidecar, &exe_dir)
        .err()
        .expect("the sidecar ends before it binds");
    let text = error.to_string();
    assert!(text.contains("the sidecar went down"), "its own words stay: {text}");
    assert!(text.contains(UNTESTED_CLAUDE_NOTE), "{text}");
    assert!(
        !text.contains(REFUSED_CLAUDE_NOTE),
        "that is the note of a refusal: {text}"
    );
}

#[test]
fn a_start_that_fails_with_the_diagnostic_carries_no_note_where_there_is_no_command() {
    let dir = Scratch::new();
    let exe_dir = dir.exe_dir("without", false);
    let sidecar = failing_with(&dir.0, "dies", &format!("{DIAGNOSTIC}\nthe sidecar went down"));
    let error = ClaudeSidecarProvider::connect_to_program_in(&instance(), &sidecar, &exe_dir)
        .err()
        .expect("the sidecar ends before it binds");
    let text = error.to_string();
    assert!(text.contains("the sidecar went down"), "{text}");
    assert!(!text.contains(UNTESTED_CLAUDE_NOTE), "{text}");
}

#[test]
fn a_start_that_fails_without_the_diagnostic_carries_no_note() {
    let dir = Scratch::new();
    let exe_dir = dir.exe_dir("with", true);
    let sidecar = failing_with(&dir.0, "dies", "the sidecar went down");
    let error = ClaudeSidecarProvider::connect_to_program_in(&instance(), &sidecar, &exe_dir)
        .err()
        .expect("the sidecar ends before it binds");
    let text = error.to_string();
    assert!(!text.contains(UNTESTED_CLAUDE_NOTE), "{text}");
    assert!(!text.contains(REFUSED_CLAUDE_NOTE), "{text}");
}

#[test]
fn a_refused_start_carries_the_refusal_note_where_the_command_exists() {
    let dir = Scratch::new();
    let exe_dir = dir.exe_dir("with", true);
    let sidecar = failing_with(&dir.0, "refuses", REFUSAL);
    let error = ClaudeSidecarProvider::connect_to_program_in(&instance(), &sidecar, &exe_dir)
        .err()
        .expect("the sidecar refuses");
    let text = error.to_string();
    assert!(text.contains(REFUSAL), "the refusal stays: {text}");
    assert!(text.contains(REFUSED_CLAUDE_NOTE), "{text}");
    assert!(
        !text.contains(UNTESTED_CLAUDE_NOTE),
        "that is the note of an untested claude: {text}"
    );
}

#[test]
fn a_refused_start_carries_no_note_where_there_is_no_command() {
    let dir = Scratch::new();
    let exe_dir = dir.exe_dir("without", false);
    let sidecar = failing_with(&dir.0, "refuses", REFUSAL);
    let error = ClaudeSidecarProvider::connect_to_program_in(&instance(), &sidecar, &exe_dir)
        .err()
        .expect("the sidecar refuses");
    let text = error.to_string();
    assert!(text.contains(REFUSAL), "{text}");
    assert!(!text.contains(REFUSED_CLAUDE_NOTE), "{text}");
}
