//! Invoked synchronously by the real `claude` CLI as a `PreToolUse` hook command (see
//! `agent::settings` for how the hook config naming this binary gets generated). Reads the
//! hook's real JSON from its own stdin, forwards it verbatim to the parent long-lived `agent`
//! process over a per-conversation Unix socket (path from `NEOVIBE_AGENT_HOOK_SOCKET`), blocks
//! for that process's real decision, then prints it to stdout and exits 0. The CLI itself
//! enforces the hook's configured timeout -- this binary has no timeout logic of its own.

use std::io::{Read, Write, BufRead, BufReader};
use std::os::unix::net::UnixStream;

fn main() {
    let socket_path = std::env::var("NEOVIBE_AGENT_HOOK_SOCKET").unwrap_or_else(|_| {
        eprintln!("agent-hook: NEOVIBE_AGENT_HOOK_SOCKET not set");
        std::process::exit(1);
    });

    let mut stdin_json = String::new();
    std::io::stdin().read_to_string(&mut stdin_json).unwrap_or_else(|e| {
        eprintln!("agent-hook: failed to read stdin: {e}");
        std::process::exit(1);
    });

    let mut stream = UnixStream::connect(&socket_path).unwrap_or_else(|e| {
        eprintln!("agent-hook: failed to connect to {socket_path}: {e}");
        std::process::exit(1);
    });

    // Forward the real payload verbatim (trimmed to one line -- the CLI's own stdin JSON has no
    // embedded newlines in the real captured fixture, but trim defensively regardless).
    let line = stdin_json.trim().to_string();
    if let Err(e) = writeln!(stream, "{line}") {
        eprintln!("agent-hook: failed to write to socket: {e}");
        std::process::exit(1);
    }

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    if let Err(e) = reader.read_line(&mut response) {
        eprintln!("agent-hook: failed to read decision from socket: {e}");
        std::process::exit(1);
    }

    print!("{}", response.trim());
}
