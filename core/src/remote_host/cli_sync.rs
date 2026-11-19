//! Command entry points for remote project sync and forwarded git.

use super::cli::Context;

pub fn run_sync(_args: &[String], _cx: &mut Context<'_>) -> i32 {
    eprintln!("eitri host sync: not built yet");
    64
}

pub fn run_git(_args: &[String], _cx: &mut Context<'_>) -> i32 {
    eprintln!("eitri host git: not built yet");
    64
}
