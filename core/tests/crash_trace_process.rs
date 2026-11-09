//! A crash record and a window's log, from a real process: this test binary is run again as the window, since a panic
//! hook, the standard streams and `exit` belong to a whole process.

#![cfg(target_os = "linux")]

use std::ffi::OsStr;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use eitri_core::crash_trace::{self, relay, Route};

const MODE: &str = "CRASH_TRACE_MODE";
const DIR: &str = "CRASH_TRACE_DIR";
const ENTRY: &str = "child_entry_runs_only_when_a_test_runs_this_binary_again";
const SECRET: &str = "the-words-of-a-private-prompt";
const VERSION: &str = "eitri 9.9.9 (commit 000000000000, neovide fork 0000000, verdandi 0000000)";

/// A home of its own, and in it the state directory the child writes to.
struct Scratch {
    home: PathBuf,
    log: PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

impl std::ops::Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.log
    }
}

fn scratch(label: &str) -> Scratch {
    let home = std::env::temp_dir().join(format!("eitri-crash-trace-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let log = home.join("state/eitri/log");
    std::fs::create_dir_all(&log).unwrap();
    Scratch { home, log }
}

fn child(mode: &str, dir: &Scratch) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", ENTRY, "--nocapture", "--test-threads=1"])
        .env(MODE, mode)
        .env(DIR, &dir.log)
        .env("HOME", &dir.home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn run(mode: &str, dir: &Scratch) -> Output {
    child(mode, dir).output().unwrap()
}

fn records(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name.starts_with("crash-") && name.ends_with(".log"))
        })
        .collect();
    found.sort();
    found
}

fn only_record(dir: &Path) -> String {
    let found = records(dir);
    assert_eq!(found.len(), 1, "{found:?}");
    std::fs::read_to_string(&found[0]).unwrap()
}

fn devices_full_on(fds: &[i32]) {
    let full = std::fs::OpenOptions::new().write(true).open("/dev/full").unwrap();
    for fd in fds {
        // SAFETY: both descriptors are open; this process's own stream is replaced on purpose.
        unsafe { libc::dup2(full.as_raw_fd(), *fd) };
    }
}

fn logged(dir: &Path) {
    crash_trace::install(VERSION, Some(dir), std::env::var_os("HOME").as_deref());
    let log = relay::open_log(dir, "main-test.log").unwrap();
    assert!(crash_trace::keep_output(
        Some(log),
        Route::Relay { tee: true },
        Route::Relay { tee: true }
    ));
}

/// A function GLib would call: a panic cannot leave it.
extern "C" fn ffi_callback() {
    panic!("a literal panic in a callback");
}

fn panic_on_thread(name: &str, body: impl FnOnce() + Send + 'static) {
    let spawned = std::thread::Builder::new().name(name.to_string()).spawn(body).unwrap();
    let _ = spawned.join();
}

#[test]
fn child_entry_runs_only_when_a_test_runs_this_binary_again() {
    let Some(mode) = std::env::var_os(MODE) else { return };
    let dir = PathBuf::from(std::env::var_os(DIR).unwrap());
    match mode.to_str().unwrap() {
        "print-on-full-disk-with-no-relay" => {
            devices_full_on(&[1]);
            println!("a line");
        }
        "full-disk-first" => {
            devices_full_on(&[1, 2]);
            logged(&dir);
            for line in 0..1000 {
                println!("a line {line}");
                eprintln!("an error line {line}");
            }
            std::process::exit(0);
        }
        "log-on-full-disk" => {
            crash_trace::install(VERSION, Some(&dir), None);
            let full = std::fs::OpenOptions::new().write(true).open("/dev/full").unwrap();
            let stayed = crash_trace::keep_output(
                Some((full, dir.join("main-test.log"))),
                Route::Relay { tee: false },
                Route::Relay { tee: false },
            );
            assert!(stayed);
            for line in 0..1000 {
                println!("a line {line}");
            }
            std::process::exit(0);
        }
        "late-log" => {
            crash_trace::install(VERSION, Some(&dir), None);
            assert!(crash_trace::keep_output(
                None,
                Route::Relay { tee: true },
                Route::Relay { tee: true }
            ));
            println!("printed before the log had a name");
            let (file, path) = relay::open_log(&dir, "main-test.log").unwrap();
            crash_trace::attach_log(file, path);
            println!("printed after");
            std::process::exit(0);
        }
        "main-panic" => {
            logged(&dir);
            println!("the last line before the panic");
            panic!("a literal panic");
        }
        "thread-panic" => {
            logged(&dir);
            panic_on_thread("worker-seven", || panic!("a literal panic"));
            println!("the window goes on");
            std::process::exit(0);
        }
        "ffi-panic" => {
            logged(&dir);
            ffi_callback();
        }
        "secret-panics" => {
            logged(&dir);
            let home = PathBuf::from(std::env::var_os("HOME").unwrap());
            println!(
                "the home is {} and a key is sk-{}-AbCdEfGhIjKlMnOpQrStUvWxYz012345",
                home.display(),
                "stand-in"
            );
            panic_on_thread("slice", || {
                let _ = &format!("é{SECRET}")[1..];
            });
            panic_on_thread("unwrap", || {
                let failed: Result<(), String> = Err(SECRET.to_string());
                failed.unwrap();
            });
            panic_on_thread("built", || panic!("could not read {SECRET}"));
            panic_on_thread("assert", || assert_eq!(SECRET, "something else"));
            std::process::exit(0);
        }
        "no-dir" => {
            crash_trace::install(VERSION, None, None);
            panic_on_thread("worker", || panic!("a literal panic"));
            println!("the window goes on");
            std::process::exit(0);
        }
        "many-panics" => {
            logged(&dir);
            for _ in 0..30 {
                panic_on_thread("worker", || panic!("a literal panic"));
            }
            std::process::exit(0);
        }
        "print-and-exit" => {
            logged(&dir);
            for line in 0..200 {
                println!("stdout line {line}");
                eprintln!("stderr line {line}");
            }
            print!("no newline at the end");
            std::process::exit(0);
        }
        "print-a-lot" => {
            crash_trace::install(VERSION, Some(&dir), None);
            let log = relay::open_log(&dir, "main-test.log").unwrap();
            crash_trace::keep_output(Some(log), Route::Relay { tee: false }, Route::Relay { tee: false });
            let line = "x y ".repeat(50);
            let total = 3 * relay::LOG_LIMIT as usize / line.len();
            for at in 0..total {
                println!("{line}");
                // At the writer's pace: past its queue the relay drops, which is not what this checks.
                if at % 256 == 0 {
                    relay::flush(Duration::from_secs(5));
                }
            }
            relay::flush(Duration::from_secs(5));
            std::process::exit(0);
        }
        "slow-reader" => {
            logged(&dir);
            // More than a pipe holds, so the writer waits on the reader while every line below arrives on its own.
            println!("{}", "x".repeat(128 * 1024));
            for line in 0..2000 {
                println!("short line {line}");
            }
            relay::flush(Duration::from_secs(30));
            std::process::exit(0);
        }
        "nobody-reads-stdout" => {
            logged(&dir);
            let line = "x".repeat(4096);
            for _ in 0..4096 {
                println!("{line}");
            }
            std::process::exit(0);
        }
        // A window whose log is off, started by a launcher that has already gone (its pipe has no reader): a child
        // it starts then writes, and so does the window itself.
        "launcher-gone" | "launcher-gone-no-relay" => {
            // The launcher's pipe, with its reader gone, under both streams.
            let (read, write) = std::io::pipe().unwrap();
            drop(read);
            for fd in [1, 2] {
                // SAFETY: both descriptors are open; this process's own streams are replaced on purpose.
                unsafe { libc::dup2(write.as_raw_fd(), fd) };
            }
            drop(write);
            if mode == "launcher-gone" {
                assert!(crash_trace::keep_output_unlogged(
                    crash_trace::route_for(1),
                    crash_trace::route_for(2)
                ));
                relay::old_destinations_never_wait();
            }
            let status = Command::new("sh")
                .args([
                    "-c",
                    "echo a line from a child; echo and an error >&2; echo second line",
                ])
                .status()
                .unwrap();
            std::fs::write(dir.join("child-status"), status.to_string()).unwrap();
            println!("the window's own line");
            eprintln!("and its own error");
            relay::flush(Duration::from_secs(2));
            std::process::exit(0);
        }
        // A launcher that stopped reading (Ctrl+Z while it waits): the window prints more than its pipe holds.
        "launcher-stopped" | "launcher-stopped-blocking" => {
            logged_tee(&dir);
            if mode == "launcher-stopped" {
                relay::old_destinations_never_wait();
            }
            let line = "z".repeat(2048);
            for _ in 0..200 {
                println!("{line}");
            }
            println!("the last line");
            relay::flush(Duration::from_secs(3));
            std::process::exit(0);
        }
        other => panic!("unknown mode {other}"),
    }
}

/// The relay with a log and copies on to where the streams went (a launcher's pipe).
fn logged_tee(dir: &Path) {
    crash_trace::install(VERSION, Some(dir), std::env::var_os("HOME").as_deref());
    let log = relay::open_log(dir, "main-test.log").unwrap();
    assert!(crash_trace::keep_output(
        Some(log),
        crash_trace::route_for(1),
        crash_trace::route_for(2)
    ));
}

/// Runs `mode`, which puts its streams on a pipe with no reader (a launcher that has gone), and returns how it and
/// its child ended.
fn run_with_launcher_gone(mode: &str, dir: &Scratch) -> (std::process::ExitStatus, String) {
    let status = run(mode, dir).status;
    let child_status = std::fs::read_to_string(dir.join("child-status")).unwrap_or_default();
    (status, child_status)
}

#[test]
fn a_window_with_its_log_off_keeps_its_children_off_a_launcher_pipe_nobody_reads() {
    // The control: without the relay, the child writes into the dead pipe and SIGPIPE ends it.
    let dir = scratch("gone-control");
    let (status, child) = run_with_launcher_gone("launcher-gone-no-relay", &dir);
    assert!(
        child.contains("SIGPIPE"),
        "the control's child: {child:?} (window {status:?})"
    );
    let dir = scratch("gone");
    let (status, child) = run_with_launcher_gone("launcher-gone", &dir);
    assert!(status.success(), "the window: {status:?}");
    assert_eq!(child, "exit status: 0", "the child");
}

/// Runs `mode` with stdout a pipe nobody reads until it ends, and returns what reached the log.
fn run_with_launcher_stopped(mode: &str, dir: &Scratch) -> String {
    let mut started = child(mode, dir).spawn().unwrap();
    let stdout = started.stdout.take().unwrap();
    let mut stderr = started.stderr.take().unwrap();
    let drain = std::thread::spawn(move || {
        let mut sink = Vec::new();
        let _ = stderr.read_to_end(&mut sink);
    });
    let end = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = started.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= end {
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(stdout);
    if status.is_none() {
        started.kill().unwrap();
        let _ = started.wait();
    }
    drain.join().unwrap();
    assert!(status.is_some_and(|status| status.success()), "{status:?}");
    std::fs::read_to_string(dir.join("main-test.log")).unwrap()
}

#[test]
fn a_stopped_launcher_holds_up_neither_the_log_nor_the_window() {
    // The control: copies that wait for the launcher stop the log behind them.
    let dir = scratch("stopped-control");
    let log = run_with_launcher_stopped("launcher-stopped-blocking", &dir);
    assert!(!log.contains("the last line"), "the control reached the log's end");
    let dir = scratch("stopped");
    let log = run_with_launcher_stopped("launcher-stopped", &dir);
    assert!(
        log.contains("the last line"),
        "{} bytes of log without the last line",
        log.len()
    );
}

#[test]
fn a_print_on_a_full_disk_panics_without_the_relay() {
    let dir = scratch("control");
    let out = run("print-on-full-disk-with-no-relay", &dir);
    assert!(!out.status.success(), "the control: a failed write panics");
}

#[test]
fn a_full_disk_under_the_streams_ends_nothing() {
    let dir = scratch("full");
    let out = run("full-disk-first", &dir);
    assert!(out.status.success(), "{out:?}");
    let log = std::fs::read_to_string(dir.join("main-test.log")).unwrap();
    assert!(log.contains("a line 999") || log.contains("dropped"), "{log}");
}

#[test]
fn a_log_on_a_full_disk_ends_nothing() {
    let dir = scratch("log-full");
    let out = run("log-on-full-disk", &dir);
    assert!(out.status.success(), "{out:?}");
}

#[test]
fn a_panic_on_the_main_thread_leaves_a_private_record() {
    let dir = scratch("main");
    let out = run("main-panic", &dir);
    assert!(!out.status.success());
    let found = records(&dir);
    assert_eq!(found.len(), 1, "{found:?}");
    let mode = std::fs::metadata(&found[0]).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let record = only_record(&dir);
    for said in [
        VERSION,
        "build: ",
        "system: Linux",
        "time: 20",
        "core/tests/crash_trace_process.rs",
        "message: a literal panic",
        "backtrace:",
        "core::panicking",
        "the last line before the panic",
    ] {
        assert!(record.contains(said), "no {said:?} in:\n{record}");
    }
}

#[test]
fn a_panic_on_another_thread_is_recorded_and_the_window_goes_on() {
    let dir = scratch("thread");
    let out = run("thread-panic", &dir);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("the window goes on"));
    assert!(only_record(&dir).contains("thread: worker-seven"));
}

#[test]
fn a_panic_a_callback_cannot_unwind_still_leaves_a_record() {
    let dir = scratch("ffi");
    let out = run("ffi-panic", &dir);
    assert_eq!(out.status.signal(), Some(libc::SIGABRT), "{out:?}");
    assert!(only_record(&dir).contains("message: a literal panic in a callback"));
}

#[test]
fn what_a_panic_quotes_stays_out_of_every_file_and_still_reaches_stderr() {
    let dir = scratch("secret");
    let out = run("secret-panics", &dir);
    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(SECRET),
        "the words a panic always printed still reach where stderr went"
    );
    let record = only_record(&dir);
    assert!(record.contains("thread: slice"), "{record}");
    let log = std::fs::read_to_string(dir.join("main-test.log")).unwrap();
    for (name, text) in [("record", &record), ("log", &log)] {
        assert!(!text.contains(SECRET), "the {name} quotes it:\n{text}");
        assert!(!text.contains("AbCdEfGh"), "the {name} holds a key:\n{text}");
        assert!(
            !text.contains(dir.home.to_str().unwrap()),
            "the {name} holds the home:\n{text}"
        );
    }
    assert!(log.contains("the home is ~"), "{log}");
}

#[test]
fn with_nowhere_to_write_a_panic_is_only_what_it_was() {
    let dir = scratch("no-dir");
    let out = run("no-dir", &dir);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("a literal panic"));
    assert!(records(&dir).is_empty());
}

#[test]
fn a_worker_that_keeps_panicking_writes_a_bounded_record() {
    let dir = scratch("many");
    let out = run("many-panics", &dir);
    assert!(out.status.success(), "{out:?}");
    let record = only_record(&dir);
    assert_eq!(record.matches("message: a literal panic").count(), 8);
}

#[test]
fn an_exit_loses_nothing_on_the_way_to_stdout_stderr_or_the_log() {
    let dir = scratch("exit");
    let out = run("print-and-exit", &dir);
    assert!(out.status.success(), "{out:?}");
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(stdout.contains("stdout line 199"), "{stdout}");
    assert!(stdout.ends_with("no newline at the end"), "{stdout}");
    assert!(stderr.contains("stderr line 199"), "{stderr}");
    assert!(!stdout.contains("stderr line"), "each stream stays itself");
    let log = std::fs::read_to_string(dir.join("main-test.log")).unwrap();
    assert!(log.starts_with(VERSION), "{log}");
    assert!(
        log.contains("stdout line 199") && log.contains("stderr line 199"),
        "{log}"
    );
    assert!(log.contains("no newline at the end"), "{log}");
}

#[test]
fn a_log_named_late_starts_with_what_was_printed_before_it() {
    let dir = scratch("late");
    let out = run("late-log", &dir);
    assert!(out.status.success(), "{out:?}");
    let log = std::fs::read_to_string(dir.join("main-test.log")).unwrap();
    assert!(log.starts_with(VERSION), "{log}");
    assert!(
        log.contains("printed before the log had a name\nprinted after\n"),
        "{log}"
    );
}

#[test]
fn a_slow_reader_costs_no_lines() {
    let dir = scratch("slow");
    let mut started = child("slow-reader", &dir).stderr(Stdio::null()).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let mut said = String::new();
    started.stdout.take().unwrap().read_to_string(&mut said).unwrap();
    assert!(started.wait().unwrap().success());
    assert_eq!(said.matches("short line ").count(), 2000);
}

#[test]
fn a_log_stays_within_its_bound() {
    let dir = scratch("rotate");
    let out = run("print-a-lot", &dir);
    assert!(out.status.success(), "{out:?}");
    let size = |name: &str| std::fs::metadata(dir.join(name)).unwrap().len();
    assert!(size("main-test.log") <= relay::LOG_LIMIT);
    assert!(size("main-test.log.1") <= relay::LOG_LIMIT);
    assert!(size("main-test.log.1") > relay::LOG_LIMIT / 2);
}

#[test]
fn a_stdout_nobody_reads_holds_nothing_up() {
    let dir = scratch("stall");
    let mut started = child("nobody-reads-stdout", &dir).spawn().unwrap();
    let stdout = started.stdout.take().unwrap();
    let mut stderr = started.stderr.take().unwrap();
    let drain = std::thread::spawn(move || {
        let mut sink = Vec::new();
        let _ = stderr.read_to_end(&mut sink);
    });
    let end = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = started.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= end {
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(stdout);
    if status.is_none() {
        started.kill().unwrap();
        let _ = started.wait();
    }
    drain.join().unwrap();
    assert!(
        status.is_some_and(|status| status.success()),
        "the window's prints stopped behind a full pipe: {status:?}"
    );
}

#[test]
fn nothing_keeps_output_before_the_hook_is_installed() {
    assert!(!crash_trace::keep_output(
        None,
        Route::Relay { tee: true },
        Route::Relay { tee: true }
    ));
}
