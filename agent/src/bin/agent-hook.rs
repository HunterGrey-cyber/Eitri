//! Invoked synchronously by the real `claude` CLI as a `PreToolUse` hook command (see
//! `agent::settings` for how the hook config naming this binary gets generated). Reads the
//! hook's real JSON from its own stdin, forwards it verbatim to the parent long-lived `agent`
//! process over a per-conversation Unix socket (path from `NEOVIBE_AGENT_HOOK_SOCKET`), blocks
//! for that process's real decision, then prints it to stdout and exits 0. The CLI itself
//! enforces the hook's configured timeout -- this binary has no timeout logic of its own.
//!
//! # Every failure exits 2, and that is the whole point of this binary's exit codes
//!
//! This is the ONLY real permission gate on the default (legacy) backend. `--disallowedTools` is
//! documented as best-effort by everything that describes it; the hook is the mechanism.
//!
//! Claude Code's hook contract (code.claude.com/docs/en/hooks, read 2026-09-17) assigns three
//! distinct meanings, and only one of them stops a tool:
//!
//! | exit | what the CLI does with a `PreToolUse` hook |
//! |---|---|
//! | 2 | **blocks the call**, and feeds this process's stderr back to the model |
//! | 1 | a non-blocking error: the call **proceeds** |
//! | 0, empty stdout | "continue through the normal permission flow" -- the call **proceeds** |
//!
//! Until 2026-09-18 every failure path here exited 1, and a relay that closed the connection
//! without answering fell out of `read_line` as `Ok(0)` and reached the final `print!` with an
//! empty string -- exit 0, no output. So the socket being gone, the listener dropping a
//! connection, or the host process dying mid-decision each let the tool call run with **no
//! permission card and no record that a gate was ever asked**. `agent::process` already described
//! a dropped connection as a silent allow; the binary on the other end never honoured that.
//!
//! The direction a gate fails in is not a detail -- a gate that fails open is worse than no gate,
//! because the absence of a card reads as "nothing needed approval". Every path below therefore
//! exits 2 and says why on stderr, where the model will see it.
//!
//! What is deliberately NOT here: a timeout, a retry, and any attempt to distinguish "the host is
//! shutting down cleanly" from "the host crashed". A shutting-down host has already released its
//! pending hook connections (`agent::process::release_pending_hook_connections`), and a tool call
//! arriving after that has no one to approve it either way. Guessing which case this is, in a
//! process whose only job is to relay one decision, would be guessing in the fail-open direction.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;

/// Blocks the tool call and tells the model why.
///
/// Exit 2 is the only code the CLI treats as a block; stderr is what it feeds back. The message is
/// written for that reader: it says the call was refused because the gate could not be reached, not
/// merely that something failed, so the model does not read it as a transient glitch worth retrying
/// around.
fn block(reason: &str) -> ! {
    eprintln!(
        "agent-hook: refusing this tool call because neovibe's permission gate could not be \
         reached: {reason}"
    );
    std::process::exit(2);
}

fn main() {
    let socket_path = std::env::var("NEOVIBE_AGENT_HOOK_SOCKET")
        .unwrap_or_else(|_| block("NEOVIBE_AGENT_HOOK_SOCKET is not set in this hook's environment"));

    let mut stdin_json = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut stdin_json) {
        block(&format!("could not read the hook payload from stdin: {e}"));
    }

    let mut stream = UnixStream::connect(&socket_path)
        .unwrap_or_else(|e| block(&format!("could not connect to {socket_path}: {e}")));

    // Forward the real payload verbatim (trimmed to one line -- the CLI's own stdin JSON has no
    // embedded newlines in the real captured fixture, but trim defensively regardless).
    let line = stdin_json.trim().to_string();
    if let Err(e) = writeln!(stream, "{line}") {
        block(&format!("could not send the request to {socket_path}: {e}"));
    }

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    match reader.read_line(&mut response) {
        Err(e) => block(&format!("could not read a decision from {socket_path}: {e}")),
        // `Ok(0)` is EOF: the relay closed without answering. This is the path that used to reach
        // `print!` with an empty string and exit 0, which the CLI reads as "no opinion, carry on".
        Ok(0) => block("the host closed the connection without answering"),
        Ok(_) => {}
    }

    let decision = response.trim();
    if decision.is_empty() {
        block("the host sent an empty decision");
    }
    print!("{decision}");
}
