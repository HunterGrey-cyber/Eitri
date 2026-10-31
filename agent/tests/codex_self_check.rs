//! The per-binary self-check, end to end.
//!
//! - The tests that need no Codex run a stand-in script as the `codex` binary.
//! - The `#[ignore]`d ones run the real `codex` found on `PATH` against the scripted model on loopback, in a
//!   private `CODEX_HOME`: no login, no network beyond 127.0.0.1, and no model turn is spent. Run them with
//!   `cargo test -p agent --test codex_self_check -- --ignored` under a `TMUX`-free environment.
//!
//! The scratch directories live under the test target's own directory and not under `/tmp`: Codex's sandbox
//! always lets a command write `/tmp`, which would defeat the check's "outside the sandbox" files.

#[path = "support/codex_peer.rs"]
mod codex_peer;

use agent::providers::codex::self_check::{ensure_checked, ensure_checked_tweaked, CheckTweaks, SelfCheckError};
use agent::providers::codex::{locate_codex, CodexLaunch, CodexProvider};
use agent::{ProviderError, ProviderErrorCode};
use std::path::{Path, PathBuf};
use std::time::Instant;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("codex-self-check-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Scratch(dir)
    }

    fn state(&self) -> PathBuf {
        self.0.join("state")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Set to read the check's log after a run.
        if std::env::var_os("EITRI_KEEP_SCRATCH").is_some() {
            return;
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn real_codex() -> PathBuf {
    let program = locate_codex(None).expect("a codex on PATH (or EITRI_CODEX)");
    // An older binary is refused before the check runs (`TooOld`), so the tests below would fail on
    // that and say nothing about the check: the shared helper names the cause instead.
    codex_peer::require_supported_codex(&program);
    program
}

fn verdicts(state: &Path) -> usize {
    let Ok(dirs) = std::fs::read_dir(state.join("codex-check")) else {
        return 0;
    };
    dirs.flatten()
        .filter(|d| d.path().join("verdict.json").is_file())
        .count()
}

// ---------------------------------------------------------------- no Codex needed

/// Writes an executable script through a child process. Written from this process, the file's write
/// descriptor would be open here while a test on another thread forks; the forked child holds it until
/// it execs, and running the script meanwhile fails with `ETXTBSY` ("Text file busy").
fn write_script(path: &std::path::Path, body: &str) {
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(r#"printf '%s' "$1" > "$2" && chmod 755 "$2""#)
        .arg("sh")
        .arg(body)
        .arg(path)
        .status()
        .expect("sh runs");
    assert!(status.success(), "writing the script failed: {status}");
}

#[test]
fn a_codex_that_cannot_run_the_check_refuses_every_start() {
    let scratch = Scratch::new("stand-in");
    let program = scratch.0.join("codex");
    write_script(
        &program,
        "#!/bin/sh\ncase \"$1\" in\n  --version) echo 'codex-cli 0.161.0' ;;\n  *) exit 1 ;;\nesac\n",
    );
    let launch = || CodexLaunch {
        program: program.clone(),
        cwd: scratch.0.clone(),
        state_home: scratch.state(),
    };

    for _ in 0..2 {
        match CodexProvider::connect(launch()) {
            Err(ProviderError::Provider { code, message }) => {
                assert_eq!(code, ProviderErrorCode::ProviderUnavailable);
                assert!(
                    message.starts_with("Eitri could not confirm that this Codex keeps its levers; see "),
                    "{message}"
                );
                assert!(message.contains("check.log"), "{message}");
            }
            Err(other) => panic!("expected the check's notice, got {other}"),
            Ok(_) => panic!("a Codex that could not be checked connected"),
        }
    }
    assert_eq!(verdicts(&scratch.state()), 0, "a failure is not recorded");
    // The log says why, and the scratch directory of the run is gone.
    let dir = std::fs::read_dir(scratch.state().join("codex-check"))
        .expect("the check's directory")
        .flatten()
        .next()
        .expect("one binary")
        .path();
    let log = std::fs::read_to_string(dir.join("check.log")).expect("the log");
    assert!(log.contains("FAILED") && log.contains("reason:"), "{log}");
    assert!(!dir.join("run").exists());
}

// ---------------------------------------------------------------- the real Codex, scripted model

#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn self_check_passes_on_this_codex() {
    let scratch = Scratch::new("pass");
    let codex = real_codex();
    let started = Instant::now();
    ensure_checked(&codex, &scratch.state()).unwrap_or_else(|e| {
        let reasons = match &e {
            SelfCheckError::Failed { reasons, log } => {
                format!("{reasons:?}\n{}", std::fs::read_to_string(log).unwrap_or_default())
            }
            other => other.to_string(),
        };
        panic!("the check failed: {e}\n{reasons}")
    });
    eprintln!("the check passed in {:.1}s", started.elapsed().as_secs_f32());
    assert_eq!(verdicts(&scratch.state()), 1);

    // The verdict answers the next start without a second run.
    let again = Instant::now();
    ensure_checked(&codex, &scratch.state()).expect("the cached verdict passes");
    assert!(again.elapsed().as_secs() < 5, "a recorded pass is not run again");
}

/// A check that cannot tell the read-only sandbox from the workspace one proves nothing about it.
#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn self_check_fails_closed_when_a_read_only_thread_is_not() {
    let scratch = Scratch::new("no-read-only");
    fails_closed_with_the_sandbox_widened(&scratch.state());
}

/// The commands name their files in a path with a space: a split command would write nothing whatever the sandbox
/// did, and this would pass.
#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn self_check_tells_the_sandboxes_apart_in_a_state_directory_with_a_space() {
    let scratch = Scratch::new("spaced");
    let state = scratch.state().join("with a space");
    fails_closed_with_the_sandbox_widened(&state);
    ensure_checked(&real_codex(), &state).expect("the check passes in such a directory");
}

fn fails_closed_with_the_sandbox_widened(state: &std::path::Path) {
    let tweaks = CheckTweaks {
        widen_read_only: true,
        ..CheckTweaks::default()
    };
    let err = ensure_checked_tweaked(&real_codex(), state, &tweaks).expect_err("a wider sandbox fails the check");
    let SelfCheckError::Failed { reasons, .. } = &err else {
        panic!("{err:?}")
    };
    assert!(
        reasons
            .iter()
            .any(|r| r.contains("a plain write inside the project of a read-only thread")),
        "the check should name the write that got through: {reasons:?}"
    );
    assert!(
        reasons.iter().any(|r| r.contains("read-only start did not echo")),
        "the check should name the echo: {reasons:?}"
    );
    assert_eq!(verdicts(state), 0);
}

#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn self_check_fails_closed_when_spawn_is_refused() {
    let scratch = Scratch::new("no-spawn");
    let tweaks = CheckTweaks {
        spawn_without_namespace: true,
        ..CheckTweaks::default()
    };
    let err =
        ensure_checked_tweaked(&real_codex(), &scratch.state(), &tweaks).expect_err("a Codex that cannot spawn fails");
    let SelfCheckError::Failed { reasons, .. } = &err else {
        panic!("{err:?}")
    };
    assert!(
        reasons.iter().any(|r| r.contains("no child was announced")),
        "the check should name the missing child: {reasons:?}"
    );
    assert!(err
        .to_string()
        .starts_with("Eitri could not confirm that this Codex keeps its levers"));
    assert_eq!(verdicts(&scratch.state()), 0);
}

#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn self_check_fails_when_a_lever_is_removed() {
    let scratch = Scratch::new("no-skills-lever");
    let tweaks = CheckTweaks {
        drop_skills_lever: true,
        ..CheckTweaks::default()
    };
    let err = ensure_checked_tweaked(&real_codex(), &scratch.state(), &tweaks).expect_err("a missing lever fails");
    let SelfCheckError::Failed { reasons, .. } = &err else {
        panic!("{err:?}")
    };
    assert!(
        reasons.iter().any(|r| r.contains("skill")),
        "the check should find the skill's name in a request: {reasons:?}"
    );
    assert_eq!(verdicts(&scratch.state()), 0);
}

#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn self_check_fails_when_the_project_is_trusted() {
    let scratch = Scratch::new("trusted");
    let tweaks = CheckTweaks {
        grant_trust: true,
        ..CheckTweaks::default()
    };
    let err =
        ensure_checked_tweaked(&real_codex(), &scratch.state(), &tweaks).expect_err("a trusted fixture project fails");
    let SelfCheckError::Failed { reasons, .. } = &err else {
        panic!("{err:?}")
    };
    // The control: with trust, the project's own commands run and its files load, so every marker the
    // check looks for is one it can see.
    assert!(
        reasons.iter().any(|r| r.contains("project's own commands ran")),
        "{reasons:?}"
    );
    assert!(reasons.iter().any(|r| r.contains("instruction file")), "{reasons:?}");
}

/// A state directory under `/tmp`, where Codex's sandbox always lets commands write: the check puts the files
/// it must keep commands from writing elsewhere and passes on a Codex that keeps its levers.
#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn self_check_passes_with_its_state_under_tmp() {
    let state = PathBuf::from("/tmp").join(format!("eitri-self-check-tmp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&state);
    let result = ensure_checked(&real_codex(), &state);
    let log = std::fs::read_dir(state.join("codex-check"))
        .ok()
        .and_then(|mut dirs| dirs.next())
        .and_then(|d| std::fs::read_to_string(d.ok()?.path().join("check.log")).ok())
        .unwrap_or_default();
    let _ = std::fs::remove_dir_all(&state);
    result.unwrap_or_else(|e| panic!("the check failed under /tmp: {e}\n{log}"));
    assert!(log.contains("always lets commands write"), "{log}");
}

/// And an escape is still caught there: a trusted fixture project lets its exec-policy rule write outside
/// the sandbox, and the check fails.
#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn self_check_under_tmp_still_catches_a_write_outside() {
    let state = PathBuf::from("/tmp").join(format!("eitri-self-check-tmp-escape-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&state);
    let tweaks = CheckTweaks {
        grant_trust: true,
        ..CheckTweaks::default()
    };
    let err = ensure_checked_tweaked(&real_codex(), &state, &tweaks);
    let _ = std::fs::remove_dir_all(&state);
    let SelfCheckError::Failed { reasons, .. } = err.expect_err("a trusted fixture project fails") else {
        panic!("not a failed check")
    };
    assert!(
        reasons.iter().any(|r| r.contains("the sandbox let a write through")),
        "{reasons:?}"
    );
}
