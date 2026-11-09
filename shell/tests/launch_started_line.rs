//! The binary says `started` on a detached start's descriptor before anything else it does, so the launcher can tell
//! a window that ran from a start that never reached it (a scope the service manager refused).
//!
//! A plain `main` (`harness = false`): `own_display_scan` requires a test that starts the product binary to isolate its
//! display first, so the binary runs against this test's own `Xvfb` and no session bus, even though `--version` opens
//! neither.

#[path = "support/own_x_server.rs"]
mod own_x_server;

use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::Command;

fn said_on_three(args: &[&str]) -> (std::process::Output, String) {
    let (mut read, write) = std::io::pipe().unwrap();
    let raw = write.as_raw_fd();
    let mut command = Command::new(env!("CARGO_BIN_EXE_shell"));
    command
        .args(args)
        .env("EITRI_LAUNCH_READY_FD", "3")
        .env_remove("EITRI_LAUNCHED_FROM");
    // SAFETY: between fork and exec only `dup2`, which is async-signal-safe; descriptor 3 is the pipe's write end, for
    // the child alone (the copy `dup2` makes is not closed on exec).
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(raw, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let out = command.output().unwrap();
    drop(command);
    drop(write);
    let mut said = String::new();
    read.read_to_string(&mut said).unwrap();
    (out, said)
}

fn the_binary_says_started_on_the_launchers_descriptor_first() {
    let (out, said) = said_on_three(&["--version"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(said, "started\n");
}

fn main() {
    let _server = own_x_server::isolate("launch_started_line", "640x480x24");
    the_binary_says_started_on_the_launchers_descriptor_first();
    println!("test the_binary_says_started_on_the_launchers_descriptor_first ... ok");
}
