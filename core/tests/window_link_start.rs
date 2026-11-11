//! Window 2's spawn between two real processes: this test binary is started through `spawn_second` as window 2's
//! stand-in, runs window 2's first acts on its argv and reports over the link. Its own binary, because each spawn
//! forks, and a fork holds copies of every descriptor of the process until it execs: in the library's test binary
//! those copies would keep other tests' descriptors (the pair lock's among them) open behind their backs.

#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use eitri_core::window_link::fdpass;
use eitri_core::window_link::start::{first_acts, parse_args, spawn_second, LinkedArgs, LINK_FD, SUBCOMMAND};

/// Set only in a test binary started by these tests as window 2's executable.
const HELPER_ENV: &str = "MW_START_HELPER";
const HELPER_TEST: &str = "stand_in_for_the_spawned_window";

/// A directory of its own under the build's scratch space, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Scratch {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("mw-start-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Writes a script that runs this test binary as the spawned window's stand-in: `exec`ed, it keeps the spawned
/// process (the pair's maker is its parent); run as a child of the shell, the pair's maker is its grandparent.
fn helper_script(dir: &Path, name: &str, exec: bool) -> PathBuf {
    let bin = std::env::current_exe().unwrap();
    let bin = bin.to_str().unwrap();
    assert!(!bin.contains('\''), "the test binary's path quotes as is");
    let run = format!("'{bin}' --exact {HELPER_TEST} --nocapture --test-threads=1 -- \"$@\"");
    let body = if exec {
        format!("#!/bin/sh\nexport {HELPER_ENV}=1\nexec {run}\n")
    } else {
        format!("#!/bin/sh\nexport {HELPER_ENV}=1\n{run}\nexit $?\n")
    };
    let path = dir.join(name);
    script_fixture::write_executable_script(&path, body, 0o700).unwrap();
    path
}

/// Spawns `exe` through `spawn_second` and returns what the stand-in reported over the link. A script written a
/// moment ago can be briefly busy while the other test's fork still holds its write handle; that spawn is retried.
fn spawn_and_read(exe: &Path, token: Option<&str>) -> String {
    for _ in 0..100 {
        let (mine, theirs) = fdpass::seqpacket_pair().unwrap();
        match spawn_second(
            exe,
            theirs,
            Path::new("/home/someone/project"),
            token,
            false,
            false,
            false,
            None,
        ) {
            Ok(mut child) => {
                let status = child.wait().unwrap();
                let got = fdpass::recv(mine.as_fd(), 4096)
                    .unwrap()
                    .map(|r| String::from_utf8(r.bytes).unwrap());
                return got.unwrap_or_else(|| format!("the stand-in reported nothing ({status})"));
            }
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            Err(e) => panic!("spawn failed: {e}"),
        }
    }
    panic!("the script stayed busy");
}

/// Runs as window 2 would, when started by the spawn tests; otherwise does nothing. It reports over descriptor 3
/// either `ok token=<token or ->` or what was wrong.
#[test]
fn stand_in_for_the_spawned_window() {
    if std::env::var_os(HELPER_ENV).is_none() {
        return;
    }
    let argv: Vec<OsString> = std::env::args_os().collect();
    let at = argv.iter().position(|a| a == SUBCOMMAND);
    let report = match at.map(|at| parse_args(&argv[at + 1..])) {
        None => Err(format!("no {SUBCOMMAND} in {argv:?}")),
        Some(Err(e)) => Err(e),
        Some(Ok(args)) => first_acts(&args).map(|fd| (args, fd)),
    };
    let text = match &report {
        Err(e) => e.clone(),
        Ok((args, fd)) => {
            stand_in_checks(args, fd).unwrap_or_else(|| format!("ok token={}", args.token.as_deref().unwrap_or("-")))
        }
    };
    // SAFETY: descriptor 3 is the link end this process was started with, open for the call whether or not the
    // checks owned it.
    let link = unsafe { BorrowedFd::borrow_raw(LINK_FD) };
    let _ = fdpass::send(link, text.as_bytes(), &[]);
}

/// What the spawn promised beyond the link checks; `None` when all of it holds.
fn stand_in_checks(args: &LinkedArgs, fd: &OwnedFd) -> Option<String> {
    if !fdpass::is_cloexec(fd.as_fd()).unwrap_or(false) {
        return Some("the link end is not close-on-exec".into());
    }
    // SAFETY: neither call has preconditions.
    if unsafe { libc::getpgrp() != libc::getpid() } {
        return Some("not the leader of its own process group".into());
    }
    for n in 0..3 {
        let target = std::fs::read_link(format!("/proc/self/fd/{n}")).ok();
        if target.as_deref() != Some(Path::new("/dev/null")) {
            return Some(format!("descriptor {n} is {target:?}, not /dev/null"));
        }
    }
    if let Some(token) = &args.token {
        if std::env::vars_os()
            .any(|(k, v)| k.to_string_lossy().contains(token.as_str()) || v.to_string_lossy().contains(token.as_str()))
        {
            return Some("the token is in the environment".into());
        }
    }
    None
}

#[test]
fn a_child_spawned_with_spawn_second_passes_the_checks_and_a_stranger_does_not() {
    let dir = Scratch::new("spawn");
    let direct = helper_script(&dir.0, "direct", true);
    assert_eq!(spawn_and_read(&direct, None), "ok token=-");

    // The stand-in runs under a shell that stays: its parent is the shell, while the pair was made here.
    let stranger = helper_script(&dir.0, "stranger", false);
    let got = spawn_and_read(&stranger, None);
    assert!(
        got.contains("parent"),
        "a peer that is not the parent is refused: {got}"
    );
}

#[test]
fn the_token_is_an_argument_and_never_in_the_environment() {
    let dir = Scratch::new("token");
    let direct = helper_script(&dir.0, "direct", true);
    let token = "activation-token-5f3a91";
    assert_eq!(spawn_and_read(&direct, Some(token)), format!("ok token={token}"));
}

#[cfg(test)]
mod script_fixture {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/executable.rs"));
}
