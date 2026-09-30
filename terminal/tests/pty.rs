//! The owned PTY spawn, against real children. Headless: nothing here opens a window.

mod common;

use std::collections::BTreeMap;
use std::ffi::OsString;
// Used only by `no_orphan_survives_the_host_being_killed`, which is Linux-only: on macOS (this
// crate's second host, `Cargo.toml`) these would otherwise be unused-import warnings (fix round 1,
// review finding minor #3).
#[cfg(target_os = "linux")]
use std::io::{BufRead, BufReader};
use std::os::unix::ffi::OsStringExt;
#[cfg(target_os = "linux")]
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{size, spec};
use eitri_terminal::{child_environment, PtyChild, PtySize};

/// Reads the child's output until its side of the PTY is closed.
fn read_to_end(child: &mut PtyChild) -> Vec<u8> {
    let deadline = Instant::now() + common::WAIT;
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        assert!(
            Instant::now() < deadline,
            "no EOF; so far: {:?}",
            String::from_utf8_lossy(&out)
        );
        match child.read(&mut buf) {
            Ok(0) => return out,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => return out, // EIO: the user side is closed
        }
    }
}

/// Output as text, with the tty's `\r\n` put back to `\n`.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n")
}

/// `waitpid(pid, WNOHANG)` failing with ECHILD: the pid is not a child of ours any more, i.e. it
/// was reaped. A zombie would still answer.
fn is_reaped(pid: u32) -> bool {
    let mut status = 0;
    let rc = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
    rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD)
}

#[test]
fn the_child_gets_exactly_the_hosts_environment_minus_tmux_plus_three() {
    let term_before = std::env::var_os("TERM");
    let mut child = PtyChild::spawn(&spec("/usr/bin/env", &["-0"]), size(80, 24)).unwrap();
    let out = read_to_end(&mut child);
    child.wait().unwrap();

    let parse = |pairs: Vec<(OsString, OsString)>| pairs.into_iter().collect::<BTreeMap<_, _>>();
    let got = parse(
        text(&out)
            .split('\0')
            .filter(|kv| !kv.is_empty())
            .map(|kv| {
                let (k, v) = kv.split_once('=').expect("KEY=value");
                (OsString::from(k), OsString::from(v))
            })
            .collect(),
    );
    // Values the tty would have rewritten (`\n` -> `\r\n`) are compared after the same rewrite.
    let want = parse(
        child_environment(std::env::vars_os())
            .into_iter()
            .map(|(k, v)| (k, OsString::from(text(&v.into_vec()))))
            .collect(),
    );
    assert_eq!(got, want);
    assert_eq!(got.get(&OsString::from("TERM_PROGRAM")), Some(&OsString::from("eitri")));
    assert!(!got.contains_key(&OsString::from("TMUX")) && !got.contains_key(&OsString::from("TMUX_PANE")));
    // The owned spawn injects nothing of its own -- unlike `alacritty_terminal::tty`, which sets
    // `ALACRITTY_WINDOW_ID` unconditionally. A host that happens to run this test *inside*
    // Alacritty legitimately has that variable already, and `child_environment` passes it through
    // like any other host variable, so the assertion is against the host's own value, not against
    // absence (fix round 1, review finding "Important" #1: the unconditional-absence version
    // panicked with `ALACRITTY_WINDOW_ID=12345 cargo test ...`, blaming the spawn for the host).
    assert_eq!(
        got.get(&OsString::from("ALACRITTY_WINDOW_ID")),
        std::env::var_os("ALACRITTY_WINDOW_ID").as_ref(),
        "the owned spawn injects nothing of its own"
    );
    assert_eq!(
        std::env::var_os("TERM"),
        term_before,
        "the parent's own environment is untouched"
    );
}

#[test]
fn the_child_runs_in_the_requested_directory_on_a_tty_of_the_requested_size() {
    // `stty -a | grep -o '[-]*iutf8'` isolates the one flag `PtyChild::spawn` sets explicitly
    // (review 2026-09-23, minor finding 6: nothing checked IUTF8 was ever actually applied). Set,
    // `stty -a` prints the bare name; unset, a leading `-`.
    let mut child = PtyChild::spawn(
        &spec("/bin/sh", &["-c", "pwd; stty size; tty; stty -a | grep -o '[-]*iutf8'"]),
        size(100, 30),
    )
    .unwrap();
    let out = text(&read_to_end(&mut child));
    let cwd = std::env::temp_dir().canonicalize().unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(std::path::Path::new(lines[0]).canonicalize().unwrap(), cwd);
    assert_eq!(lines[1], "30 100");
    assert!(lines[2].starts_with("/dev/pts/"), "a real controlling tty: {out:?}");
    assert_eq!(lines[3], "iutf8", "IUTF8 should be set on the tty: {out:?}");
    child.wait().unwrap();
}

#[test]
fn a_resize_reaches_the_child() {
    let mut child = PtyChild::spawn(&spec("/bin/sh", &["-c", "read _; stty size"]), size(80, 24)).unwrap();
    child
        .resize(PtySize {
            cols: 50,
            rows: 10,
            cell_width_px: 9,
            cell_height_px: 18,
        })
        .unwrap();
    child.write(b"\n").unwrap();
    let out = text(&read_to_end(&mut child));
    assert!(out.lines().any(|l| l == "10 50"), "{out:?}");
    child.wait().unwrap();
}

#[test]
fn eof_then_wait_gives_the_exit_code_and_leaves_no_zombie() {
    let mut child = PtyChild::spawn(&spec("/bin/sh", &["-c", "exit 7"]), size(80, 24)).unwrap();
    let pid = child.pid();
    read_to_end(&mut child);
    assert_eq!(child.wait().unwrap().code(), Some(7));
    assert!(is_reaped(pid));
}

/// `$SHELL` naming a program that is not installed must be an error the host can report, not a
/// panic and not a pane that silently never shows anything (Review Focus 1).
#[test]
fn a_program_that_does_not_exist_is_an_error() {
    let err = PtyChild::spawn(&spec("/nonexistent/eitri-shell", &[]), size(80, 24))
        .err()
        .expect("spawning a missing program fails");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn hangup_ends_an_interactive_shell() {
    let mut child = PtyChild::spawn(&common::plain_sh(), size(80, 24)).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    child.hangup();
    let deadline = Instant::now() + common::WAIT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "sh survived SIGHUP");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(!status.success());
}

/// Reads the child's output until `needle` has appeared.
#[cfg(target_os = "linux")]
fn read_until(child: &mut PtyChild, needle: &str) {
    let deadline = Instant::now() + common::WAIT;
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    while !String::from_utf8_lossy(&out).contains(needle) {
        assert!(
            Instant::now() < deadline,
            "no {needle:?}; so far: {:?}",
            String::from_utf8_lossy(&out)
        );
        match child.read(&mut buf) {
            Ok(n) if n > 0 => out.extend_from_slice(&buf[..n]),
            _ => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

/// Whether `pid` has disappeared from `/proc`: reaped. A zombie is still listed, so unlike
/// `is_reaped` this never reaps anything itself -- it cannot mistake its own reaping for the code
/// under test's.
#[cfg(target_os = "linux")]
fn gone(pid: u32) -> bool {
    !std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// Dropping a `PtyChild` whose child still runs -- an early return after the spawn, a session
/// thread that panicked -- must not leave a live shell or a zombie. `Drop` hangs it up and returns
/// at once; a reaper kills it by its pid after the grace period (this child ignores SIGHUP) and
/// reaps it.
#[cfg(target_os = "linux")]
#[test]
fn dropping_a_running_child_hangs_it_up_and_reaps_it_off_thread() {
    let mut child = PtyChild::spawn(
        &spec("/bin/sh", &["-c", "trap '' HUP; printf ready; exec sleep 30"]),
        size(80, 24),
    )
    .unwrap();
    let pid = child.pid();
    read_until(&mut child, "ready");
    let started = Instant::now();
    drop(child);
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "Drop waited: {:?}",
        started.elapsed()
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while !gone(pid) {
        assert!(Instant::now() < deadline, "pid {pid} still exists 2 s after the drop");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The shell inherits its terminal and nothing else: a descriptor this process opened without
/// `O_CLOEXEC` -- a library's leak -- is closed when the shell execs.
#[cfg(target_os = "linux")]
#[test]
fn the_child_inherits_no_stray_descriptor() {
    use std::os::fd::AsRawFd;
    let (reader, writer) = std::io::pipe().unwrap();
    let fd = writer.as_raw_fd();
    // SAFETY: fcntl on a descriptor this test owns: clear its CLOEXEC, as a careless library would.
    assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, 0) }, 0);
    let script = format!("if [ -e /proc/self/fd/{fd} ]; then echo LEAKED; else echo CLEAN; fi");
    let mut child = PtyChild::spawn(&spec("/bin/sh", &["-c", &script]), size(80, 24)).unwrap();
    let out = text(&read_to_end(&mut child));
    child.wait().unwrap();
    drop((reader, writer));
    assert!(out.contains("CLEAN"), "{out:?}");
}

/// The helper half of `no_orphan_survives_the_host_being_killed`: when re-run by that test, open a
/// session, print the shell's pid, and wait to be killed. On its own it does nothing.
#[test]
#[ignore = "run only by no_orphan_survives_the_host_being_killed"]
fn orphan_helper() {
    if std::env::var_os("EITRI_TERMINAL_ORPHAN_HELPER").is_none() {
        return;
    }
    let child = PtyChild::spawn(&common::plain_sh(), size(80, 24)).unwrap();
    println!("CHILD_PID={}", child.pid());
    std::thread::sleep(Duration::from_secs(60));
}

/// Eitri dying abruptly (a crash, a SIGKILL) must not leave the shell running: the kernel closes
/// the master fd with the process, which hangs up the tty and SIGHUPs its session.
#[cfg(target_os = "linux")]
#[test]
fn no_orphan_survives_the_host_being_killed() {
    let mut helper = Command::new(std::env::current_exe().unwrap())
        .args([
            "orphan_helper",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("EITRI_TERMINAL_ORPHAN_HELPER", "1")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut shell_pid = None;
    for line in BufReader::new(helper.stdout.take().unwrap()).lines() {
        // libtest prints "test orphan_helper ... " without a newline first, so the marker is found
        // anywhere in the line rather than at its start.
        if let Some(pid) = line.unwrap().split("CHILD_PID=").nth(1) {
            shell_pid = Some(pid.trim().parse::<i32>().unwrap());
            break;
        }
    }
    let shell_pid = shell_pid.expect("the helper printed its shell's pid");
    let alive = |pid: i32| {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map(|stat| {
                stat.rsplit(')')
                    .next()
                    .is_some_and(|rest| !rest.trim_start().starts_with('Z'))
            })
            .unwrap_or(false)
    };
    assert!(alive(shell_pid), "the shell must be running before the host dies");

    // Killed by the pid this test spawned it with -- never by name.
    helper.kill().unwrap();
    helper.wait().unwrap();

    let deadline = Instant::now() + Duration::from_secs(1);
    while alive(shell_pid) {
        assert!(
            Instant::now() < deadline,
            "shell {shell_pid} outlived its host by more than 1s"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
