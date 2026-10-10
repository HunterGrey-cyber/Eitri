//! The binary with its stdout and stderr on a pipe whose reader has gone (`eitri-mac proj | head` after `head`
//! exited): it ends with its own exit code, not a panic's (101). An option it refuses ends the start before any
//! window, so this runs without a display on every OS.

use std::os::fd::OwnedFd;
use std::process::{Command, Stdio};

#[test]
fn a_dead_output_pipe_is_not_a_panic() {
    let (read, write) = std::io::pipe().expect("a pipe");
    drop(read);
    let write = OwnedFd::from(write);
    let status = Command::new(env!("CARGO_BIN_EXE_eitri-mac"))
        .arg("--not-an-option")
        .stdin(Stdio::null())
        .stdout(Stdio::from(write.try_clone().unwrap()))
        .stderr(Stdio::from(write))
        .env_remove("EITRI_PROJECT_DIR")
        .status()
        .expect("the binary runs");
    // macOS refuses the option (1); any other OS says the app is macOS's (2). A panic writing the message is 101.
    let expected = if cfg!(target_os = "macos") { 1 } else { 2 };
    assert_eq!(status.code(), Some(expected), "{status:?}");
}
