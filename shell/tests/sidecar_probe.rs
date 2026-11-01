//! `shell --sidecar-probe <path>`: start the sidecar at `path`, check its handshake against what this Eitri
//! cannot do without, print `ok <version> protocol <major>.<minor>` and exit 0, or exit 1 with the reason as the
//! last line of stderr. It is a command of its own, answered before anything that could start GTK, a window
//! or an editor.
//!
//! A plain `main` (`harness = false`): `own_display_scan` requires a test that starts the product binary to
//! isolate its display in `main`, and with libtest's harness `main` never runs. The child gets no display at
//! all, so a probe that reached GTK would fail here instead of opening a window anywhere.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

// The display this test runs on: its own Xvfb, never an inherited one.
#[path = "support/own_x_server.rs"]
mod own_x_server;

/// How long one run of the binary may take before the test calls it hung. Not a speed bound: a run ends as soon
/// as the binary does.
const HANG: Duration = Duration::from_secs(120);

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// `shell` with `args`, in a home of its own and with no display to find.
fn run(shell: &str, scratch: &Path, args: &[&std::ffi::OsStr]) -> Result<Run, String> {
    let mut command = Command::new(shell);
    command
        .args(args)
        .env("HOME", scratch)
        .env("XDG_CONFIG_HOME", scratch.join("config"))
        .env("XDG_DATA_HOME", scratch.join("data"))
        .env("XDG_STATE_HOME", scratch.join("state"))
        .env("XDG_CACHE_HOME", scratch.join("cache"))
        .env("XDG_RUNTIME_DIR", scratch)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("WAYLAND_SOCKET")
        .env_remove("XAUTHORITY")
        .env_remove("GDK_BACKEND")
        .env_remove("EITRI_SIDECAR_BINARY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().map_err(|e| format!("the binary did not start: {e}"))?;
    let pid = child.id();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(child.wait_with_output());
    });
    match receiver.recv_timeout(HANG) {
        Ok(Ok(output)) => Ok(Run {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }),
        Ok(Err(e)) => Err(format!("waiting for the binary failed: {e}")),
        Err(_) => {
            // SAFETY: `kill` has no memory-safety preconditions; the pid is that of this run's own child, which
            // the waiting thread has not reaped, since it has not returned.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            Err(format!("the binary was still running after {} s", HANG.as_secs()))
        }
    }
}

fn no_path_is_a_usage_error(shell: &str, scratch: &Path) -> Result<(), String> {
    let run = run(shell, scratch, &["--sidecar-probe".as_ref()])?;
    if run.code != Some(64) {
        return Err(format!(
            "exit {:?}, wanted 64; stderr: {}",
            run.code,
            run.stderr.trim_end()
        ));
    }
    if run.stdout.starts_with("ok ") {
        return Err(format!("a probe of nothing printed {:?}", run.stdout));
    }
    Ok(())
}

fn a_missing_program_exits_1_with_the_reason_last(shell: &str, scratch: &Path) -> Result<(), String> {
    let missing = scratch.join("no-such-sidecar");
    let run = run(shell, scratch, &["--sidecar-probe".as_ref(), missing.as_os_str()])?;
    if run.code != Some(1) {
        return Err(format!(
            "exit {:?}, wanted 1; stderr: {}",
            run.code,
            run.stderr.trim_end()
        ));
    }
    if run.stdout.starts_with("ok ") {
        return Err(format!("a missing program printed {:?}", run.stdout));
    }
    // The reason is the probe's: the program it could not start, not the shell's complaint about a flag it
    // does not know.
    let last = run.stderr.trim_end().lines().last().unwrap_or("").to_string();
    let lower = last.to_lowercase();
    if lower.contains("unrecognized option") || lower.contains("--sidecar-probe") {
        return Err(format!("the last line is not a reason from the probe: {last:?}"));
    }
    let says_why = last.contains("no-such-sidecar")
        || lower.contains("no such file")
        || lower.contains("not found")
        || lower.contains("does not exist");
    if says_why {
        Ok(())
    } else {
        Err(format!(
            "the last line of stderr gives no reason for the failure: {last:?}"
        ))
    }
}

fn the_arm_sits_before_anything_that_starts_gtk() -> Result<(), String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    let source = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let start = source
        .find("fn main() -> glib::ExitCode {")
        .ok_or("main.rs has no main")?;
    let end = start
        + source[start..]
            .find("fn build_application")
            .ok_or("main.rs has no build_application after main")?;
    let body = &source[start..end];
    let arm = ["--sidecar-probe", "sidecar_probe"]
        .iter()
        .filter_map(|needle| body.find(needle))
        .min()
        .ok_or("main has no arm for the probe")?;
    for later in [
        "drop_inherited_editor_env()",
        "BackendKind::from_env",
        "resolve_launch()",
        "nvim_bin::resolve()",
        "build_application(kind)",
    ] {
        let at = body
            .find(later)
            .ok_or_else(|| format!("main no longer holds {later}"))?;
        if arm > at {
            return Err(format!("the probe's arm comes after {later}"));
        }
    }
    Ok(())
}

fn main() {
    // Before anything that could connect to a display or a bus.
    let server = own_x_server::isolate("sidecar_probe", "640x480x24");
    let shell = env!("CARGO_BIN_EXE_shell");
    let scratch: PathBuf = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("sidecar-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).expect("creating the scratch directory must succeed");

    let checks: [(&str, Result<(), String>); 3] = [
        ("no_path_is_a_usage_error", no_path_is_a_usage_error(shell, &scratch)),
        (
            "a_missing_program_exits_1_with_the_reason_last",
            a_missing_program_exits_1_with_the_reason_last(shell, &scratch),
        ),
        (
            "the_arm_sits_before_anything_that_starts_gtk",
            the_arm_sits_before_anything_that_starts_gtk(),
        ),
    ];
    let mut failed = 0;
    for (name, result) in &checks {
        match result {
            Ok(()) => println!("test {name} ... ok"),
            Err(why) => {
                failed += 1;
                println!("test {name} ... FAILED\n    {why}");
            }
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
    println!("sidecar_probe: {} passed, {failed} failed", checks.len() - failed);
    server.exit(if failed == 0 { 0 } else { 1 });
}
