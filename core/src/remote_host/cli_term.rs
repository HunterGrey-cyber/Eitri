//! `eitri host term [--cwd <dir>]`: a shell on the server of the remote project holding a directory (the current one,
//! or `--cwd`), started in the same directory there and shown on this process's terminal, as the window's server
//! terminals are. A raw relay over the helper's Terminal service ([`crate::remote_lsp::term`]); Neovim runs it in a
//! `:terminal` buffer (`:EitriRemote term`).
//!
//! Exit codes: the shell's own (`128 + n` for a signal), `128 + n` for a stop signal this process got, 0 after end of
//! input, 255 when the link stayed down past its limit and the shell was left running or the host cannot be reached,
//! 1 for a shell that is gone or a directory outside every remote project, 64 for a command line it does not take.

use std::io::IsTerminal;
use std::os::fd::AsFd;

use eitri_terminal::{PtySize, TerminalColors};

use super::cli::Context;
use crate::remote_lsp::term::{self, Session, Terminal, REATTACH_LIMIT};
use crate::remote_lsp::{link::random_id, signals};
use crate::remote_project::terminal_facts::TerminalFactsHandle;
use crate::remote_project::terminal_link::{TerminalLinkOptions, TerminalTarget};
use crate::remote_sync::terminal_text;

pub const USAGE: &str = "usage: eitri host term [--cwd <dir>]";

/// The size a shell opens at when the terminal reports none (output redirected, a terminal without a size).
const FALLBACK_SIZE: PtySize = PtySize {
    cols: 80,
    rows: 24,
    cell_width_px: 0,
    cell_height_px: 0,
};

/// The size of the person's terminal: stdin's, else stdout's (input may be redirected while output is not).
fn person_size() -> Option<PtySize> {
    term::local_size(0).or_else(|| term::local_size(1))
}

/// `[--cwd <dir>]`: the directory, when given.
fn parse(args: &[String]) -> Option<Option<String>> {
    match args {
        [] => Some(None),
        [flag, dir] if flag == "--cwd" => Some(Some(dir.clone())),
        _ => None,
    }
}

/// `eitri host term <args>` with this process's terminal, from the current directory.
pub fn run_term(args: &[String], cx: &mut Context<'_>) -> i32 {
    let Some(wanted) = parse(args) else {
        eprintln!("{USAGE}");
        return 64;
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            eprintln!("eitri host term: the current directory cannot be read: {error}; nothing was started");
            return 1;
        }
    };
    let (project, rel) = match super::cli_sync::project_for(cx, &cwd, wanted.as_deref()) {
        Ok(found) => found,
        Err(why) => {
            eprintln!("eitri host term: {}; nothing was started", terminal_text(&why));
            return 1;
        }
    };
    // The mirror is at the project's path on the server: the same directory there.
    let server_cwd = match rel.as_str() {
        "" => project.root.clone(),
        rel => project.root.join(rel),
    };
    // Before any process is started (the connection starts ssh): the handlers' pipe must be close-on-exec before an
    // exec could see it, and a hangup or termination from here on is seen by the relay, which closes what the link
    // opened, rather than ending this process with a shell left behind.
    let signals = match signals::watch(&[libc::SIGWINCH, libc::SIGHUP, libc::SIGTERM, libc::SIGINT]) {
        Ok(pipe) => pipe,
        Err(error) => {
            eprintln!("eitri host term: signals could not be watched: {error}; nothing was started");
            return 1;
        }
    };
    // Unbuffered, and a descriptor of its own: Rust's stdout holds back what has no newline.
    let output = match std::io::stdout().as_fd().try_clone_to_owned() {
        Ok(fd) => std::fs::File::from(fd),
        Err(error) => {
            eprintln!("eitri host term: stdout cannot be kept: {error}; nothing was started");
            return 1;
        }
    };
    let connection = cx.reach.exec_connection(&project);
    // A stop asked for while connecting could not be acted on then (the person waited out the connection, in cooked
    // mode, where a Ctrl-C reaches ssh too and may be why the connection failed): honoured now, before a shell starts.
    if let Some(signal) = term::stop_waiting(&signals) {
        eprintln!(
            "eitri host term: stopped (signal {signal}) while {} was being reached; nothing was started",
            terminal_text(&project.host)
        );
        return 128 + signal;
    }
    let connection = match connection {
        Ok(connection) => connection,
        Err(why) => {
            eprintln!(
                "eitri host term: {} cannot be reached ({}); nothing was started",
                terminal_text(&project.host),
                terminal_text(&why)
            );
            return 255;
        }
    };
    let size = person_size().unwrap_or(FALLBACK_SIZE);
    let link = match connection.terminal_link(
        TerminalLinkOptions {
            installation: String::new(),
            target: TerminalTarget::Open {
                command_id: random_id(),
                cwd: server_cwd,
            },
            size,
            colors: TerminalColors::default(),
        },
        TerminalFactsHandle::default(),
    ) {
        Ok(link) => link,
        Err(error) => {
            eprintln!(
                "eitri host term: {} cannot be reached ({}); nothing was started",
                terminal_text(&project.host),
                terminal_text(&error.to_string())
            );
            return 255;
        }
    };
    term::run(
        Session {
            host: project.host.clone(),
            link,
            close: Box::new(move |id| connection.close_terminal(id)),
            reattach_limit: REATTACH_LIMIT,
            size: Box::new(person_size),
        },
        Terminal {
            input: 0,
            output_is_terminal: output.is_terminal(),
            output: Box::new(output),
            signals,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &[&str]) -> Vec<String> {
        line.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn the_command_line_takes_a_directory_or_nothing() {
        assert_eq!(parse(&args(&[])), Some(None));
        assert_eq!(parse(&args(&["--cwd", "src"])), Some(Some("src".to_owned())));
        for line in [
            &["--cwd"][..],
            &["src"][..],
            &["--cwd", "a", "b"][..],
            &["--", "ls"][..],
        ] {
            assert!(parse(&args(line)).is_none(), "{line:?}");
        }
    }
}
