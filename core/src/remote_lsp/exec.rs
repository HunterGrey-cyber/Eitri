//! One command run on a remote project's server for the laptop (a formatter, a linter, `make`), with the project's
//! tree session flushed before it starts, so it sees the last save, and after it ends, so what it wrote in place is on
//! the laptop when it returns. Its exit code follows `ssh`: the command's own, `128 + n` for a signal, 125 when it was
//! not run, 255 when its outcome is unknown or the server could not be reached.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use crate::remote_project::{ExecFailure, RemoteConnection};

/// What the lines this module writes begin with.
pub const PREFIX: &str = "eitri host exec";
/// The longest refusal from the server shown as it came; a longer one is described, not shown.
pub const REFUSAL_MAX: usize = 4096;

/// One command and where it runs: `cwd` is the directory on the server (the mirror's path), `argv` the program first.
#[derive(Clone, Debug)]
pub struct ExecRequest {
    pub cwd: PathBuf,
    pub argv: Vec<OsString>,
    pub flush_before: bool,
    pub flush_after: bool,
    /// How long each flush may take (`remote_dev.flush_cap`).
    pub flush_cap: Duration,
}

/// The command's stdio. With `close_stdin` (stdin is a terminal, or the caller said so) `stdin` is never read and the
/// run is closed at once, as `ssh -n` does: the server sends the exit status only after the close, and a terminal or a
/// job's open pipe never ends.
pub struct StdIo<'a> {
    pub stdin: Box<dyn Read + Send>,
    pub close_stdin: bool,
    pub stdout: &'a mut dyn Write,
    pub stderr: &'a mut dyn Write,
}

/// How a run ended. The texts are the server's or this module's, unescaped: [`crate::remote_sync::terminal_text`]
/// before a terminal shows them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecOutcome {
    Exited(i32),
    Signalled(i32),
    /// Nothing was run.
    Refused(String),
    /// The command may have run and its end was not seen, or the server could not be reached.
    Unknown(String),
}

impl ExecOutcome {
    /// The exit code a caller returns: the command's own (one outside 0 through 255, which no process exit gives,
    /// as 255), `128 + n` for signal `n`, 125 when it was refused, 255 when its outcome is unknown.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Exited(code) if (0..=255).contains(code) => *code,
            Self::Exited(_) | Self::Unknown(_) => 255,
            Self::Signalled(signal) => (128 + signal).clamp(129, 255),
            Self::Refused(_) => 125,
        }
    }
}

/// Runs `req` on `conn`'s server: refused at once when the helper does not run commands; then `flush` (given
/// `req.flush_cap`) before the start when `req.flush_before`, the command with `io`, and `flush` again after an end
/// that may have changed files when `req.flush_after`. A flush that fails is said on `io.stderr` and the run goes on.
/// Blocking.
pub fn run(
    conn: &RemoteConnection,
    req: ExecRequest,
    io: StdIo<'_>,
    flush: &mut dyn FnMut(Duration) -> Result<(), String>,
) -> ExecOutcome {
    run_with(
        conn.host(),
        req,
        io,
        flush,
        || conn.offers_exec_command(),
        |cwd, argv, stdin, stdout, stderr| conn.exec_command(cwd, argv, stdin, stdout, stderr),
    )
}

fn run_with(
    host: &str,
    req: ExecRequest,
    io: StdIo<'_>,
    flush: &mut dyn FnMut(Duration) -> Result<(), String>,
    offered: impl FnOnce() -> Result<(), ExecFailure>,
    exec: impl FnOnce(
        &std::path::Path,
        &[OsString],
        Box<dyn Read + Send>,
        &mut dyn Write,
        &mut dyn Write,
    ) -> Result<agent::remote_client::ExecExit, ExecFailure>,
) -> ExecOutcome {
    let program = req
        .argv
        .first()
        .map(|program| program.to_string_lossy().into_owned())
        .unwrap_or_default();
    if let Err(failure) = offered() {
        return failed(host, &program, failure);
    }
    if req.flush_before {
        if let Err(why) = flush(req.flush_cap) {
            say(
                io.stderr,
                &format!("your last save may not have reached {host} ({why})"),
            );
        }
    }
    let stdin: Box<dyn Read + Send> = if io.close_stdin {
        Box::new(std::io::empty())
    } else {
        io.stdin
    };
    // Whether the command may have started: not when it was refused or its stream could not be opened.
    let (outcome, may_have_run) = match exec(&req.cwd, &req.argv, stdin, &mut *io.stdout, &mut *io.stderr) {
        Ok(exit) if !exit.refusal.is_empty() => (ExecOutcome::Refused(bounded_refusal(host, &exit.refusal)), false),
        Ok(exit) if exit.signal != 0 => (ExecOutcome::Signalled(exit.signal), true),
        Ok(exit) => (ExecOutcome::Exited(exit.code), true),
        Err(failure) => {
            let started = matches!(failure, ExecFailure::Lost(_));
            (failed(host, &program, failure), started)
        }
    };
    if req.flush_after && may_have_run {
        if let Err(why) = flush(req.flush_cap) {
            say(
                io.stderr,
                &format!("what {program} changed on {host} may not have reached this laptop yet ({why})"),
            );
        }
    }
    outcome
}

fn failed(host: &str, program: &str, failure: ExecFailure) -> ExecOutcome {
    match failure {
        ExecFailure::NotRun(why) => ExecOutcome::Refused(why),
        ExecFailure::Unreached(why) => {
            ExecOutcome::Unknown(format!("{host} cannot be reached ({why}); nothing was run"))
        }
        ExecFailure::Lost(why) => ExecOutcome::Unknown(format!(
            "the outcome of {program} on {host} is unknown: the connection was lost, the server ended the run, or this \
             side could not write its output ({why})"
        )),
    }
}

/// The server's refusal as it may be shown: at most [`REFUSAL_MAX`] bytes, else a sentence about it.
fn bounded_refusal(host: &str, refusal: &str) -> String {
    if refusal.len() <= REFUSAL_MAX {
        refusal.to_owned()
    } else {
        format!(
            "the Eitri server on {host} refused the command with a reason of {} bytes, longer than Eitri shows",
            refusal.len()
        )
    }
}

/// A line on stderr, its hidden characters escaped.
pub fn say(stderr: &mut dyn Write, line: &str) {
    let _ = writeln!(stderr, "{PREFIX}: {}", crate::remote_sync::terminal_text(line));
    let _ = stderr.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::remote_client::ExecExit;
    use std::cell::RefCell;

    fn request() -> ExecRequest {
        ExecRequest {
            cwd: "/srv/project".into(),
            argv: vec!["rustfmt".into(), "--edition".into(), "2021".into()],
            flush_before: true,
            flush_after: true,
            flush_cap: Duration::from_secs(3),
        }
    }

    /// Runs with `offered` and an exec answering `exit`; returns the outcome, stderr and what happened in order.
    fn drive(
        offered: Result<(), ExecFailure>,
        answer: Result<ExecExit, ExecFailure>,
        flushes: Vec<Result<(), String>>,
    ) -> (ExecOutcome, String, Vec<String>) {
        let events = RefCell::new(Vec::new());
        let flushes = RefCell::new(flushes);
        let mut stderr = Vec::new();
        let mut stdout = Vec::new();
        let mut flush = |cap: Duration| {
            events.borrow_mut().push(format!("flush {}", cap.as_secs()));
            let mut flushes = flushes.borrow_mut();
            if flushes.is_empty() {
                Ok(())
            } else {
                flushes.remove(0)
            }
        };
        let outcome = run_with(
            "example-host",
            request(),
            StdIo {
                stdin: Box::new(std::io::empty()),
                close_stdin: true,
                stdout: &mut stdout,
                stderr: &mut stderr,
            },
            &mut flush,
            || offered,
            |cwd, argv, _, _, _| {
                events
                    .borrow_mut()
                    .push(format!("exec {} {}", cwd.display(), argv[0].to_string_lossy()));
                answer
            },
        );
        (outcome, String::from_utf8(stderr).unwrap(), events.into_inner())
    }

    fn exit(code: i32, signal: i32, refusal: &str) -> Result<ExecExit, ExecFailure> {
        Ok(ExecExit {
            code,
            signal,
            refusal: refusal.into(),
        })
    }

    #[test]
    fn exit_codes_follow_ssh() {
        for (outcome, code) in [
            (ExecOutcome::Exited(0), 0),
            (ExecOutcome::Exited(3), 3),
            (ExecOutcome::Exited(-1), 255),
            (ExecOutcome::Signalled(15), 143),
            (ExecOutcome::Refused("no".into()), 125),
            (ExecOutcome::Unknown("lost".into()), 255),
        ] {
            assert_eq!(outcome.exit_code(), code, "{outcome:?}");
        }
    }

    #[test]
    fn a_helper_without_exec_command_is_refused_before_anything_is_flushed_or_sent() {
        let (outcome, stderr, events) = drive(
            Err(ExecFailure::NotRun(
                "the Eitri server on example-host is older; nothing was run".into(),
            )),
            exit(0, 0, ""),
            vec![],
        );
        assert_eq!(
            outcome,
            ExecOutcome::Refused("the Eitri server on example-host is older; nothing was run".into())
        );
        assert_eq!(outcome.exit_code(), 125);
        assert!(events.is_empty() && stderr.is_empty(), "{events:?} {stderr}");
    }

    #[test]
    fn flushes_come_before_and_after_and_a_refused_run_skips_the_second() {
        let (outcome, _, events) = drive(Ok(()), exit(7, 0, ""), vec![]);
        assert_eq!(outcome, ExecOutcome::Exited(7));
        assert_eq!(events, ["flush 3", "exec /srv/project rustfmt", "flush 3"]);
        let (outcome, _, events) = drive(Ok(()), exit(-1, 0, "`rustfmt` is not on the login PATH"), vec![]);
        assert_eq!(outcome.exit_code(), 125);
        assert_eq!(events, ["flush 3", "exec /srv/project rustfmt"]);
        let (outcome, stderr, events) = drive(
            Ok(()),
            Err(ExecFailure::Lost("execution link closed".into())),
            vec![Ok(()), Err("example-host did not confirm within 3 s".into())],
        );
        assert_eq!(outcome.exit_code(), 255);
        assert_eq!(events.len(), 3, "an unknown outcome may have changed files: {events:?}");
        assert!(
            stderr.contains("what rustfmt changed on example-host may not have reached"),
            "{stderr}"
        );
    }

    #[test]
    fn a_stream_that_could_not_be_opened_ran_nothing_and_flushes_no_second_time() {
        let (outcome, stderr, events) = drive(Ok(()), Err(ExecFailure::Unreached("channel closed".into())), vec![]);
        assert_eq!(outcome.exit_code(), 255);
        assert_eq!(
            events,
            ["flush 3", "exec /srv/project rustfmt"],
            "nothing ran, so nothing to fetch"
        );
        assert!(stderr.is_empty(), "{stderr}");
        let ExecOutcome::Unknown(text) = outcome else { panic!() };
        assert!(text.contains("nothing was run"), "{text}");
    }

    #[test]
    fn a_lost_run_says_the_server_may_have_ended_it_as_well() {
        let (outcome, _, _) = drive(Ok(()), Err(ExecFailure::Lost("execution link closed".into())), vec![]);
        let ExecOutcome::Unknown(text) = outcome else { panic!() };
        assert!(text.contains("the server ended the run"), "{text}");
        assert!(text.contains("this side could not write its output"), "{text}");
    }

    #[test]
    fn a_long_or_hidden_refusal_is_bounded_and_escaped() {
        let (outcome, _, _) = drive(Ok(()), exit(-1, 0, &"x".repeat(REFUSAL_MAX + 1)), vec![]);
        let ExecOutcome::Refused(text) = outcome else { panic!() };
        assert!(text.contains(&format!("{} bytes", REFUSAL_MAX + 1)), "{text}");
        let mut stderr = Vec::new();
        say(&mut stderr, "refused \u{1b}]52;c;payload\u{7}");
        let stderr = String::from_utf8(stderr).unwrap();
        assert!(!stderr.contains('\u{1b}') && !stderr.contains('\u{7}'), "{stderr:?}");
        assert!(stderr.starts_with("eitri host exec: refused "), "{stderr:?}");
    }
}
