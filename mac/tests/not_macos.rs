//! On any other OS the binary says what it is for and exits 2, so a stray `cargo run` is not a hang.
#![cfg(not(target_os = "macos"))]

use std::process::Command;

#[test]
fn the_binary_refuses_to_run_off_macos() {
    let out = Command::new(env!("CARGO_BIN_EXE_eitri-mac")).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(String::from_utf8_lossy(&out.stderr), "eitri-mac runs on macOS\n");
}
