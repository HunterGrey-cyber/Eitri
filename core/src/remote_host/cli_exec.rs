//! `eitri host exec`: one command on the server of the remote project holding a directory (the current one, or
//! `--cwd`), at the same path there, with stdin, stdout and stderr piped through. The project is the longest recorded
//! root holding the directory. The tree session is flushed before the command starts and after it ends, unless
//! `--no-flush`. Every line of its own goes to stderr: stdout carries only the command's output (a formatter's
//! result). Exit codes: [`ExecOutcome::exit_code`], and 125 for a command line it does not take or a directory outside
//! every remote project.

use std::ffi::OsString;
use std::io::{IsTerminal, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use super::cli::Context;
use crate::remote_lsp::exec::{self, ExecOutcome, ExecRequest, StdIo};
use crate::remote_project::RemoteProject;

pub const USAGE: &str = "usage: eitri host exec [--cwd <dir>] [--no-flush] [--no-stdin] -- <cmd> [args...]";

/// The process's stdio as the command gets it: stdin read only when it is not a terminal and `--no-stdin` was not
/// given.
pub struct ProcessIo<'a> {
    pub stdin: Box<dyn Read + Send>,
    pub stdin_is_terminal: bool,
    pub stdout: &'a mut dyn Write,
    pub stderr: &'a mut dyn Write,
}

struct Options {
    cwd: Option<String>,
    flush: bool,
    no_stdin: bool,
    argv: Vec<OsString>,
}

/// `[--cwd <dir>] [--no-flush] [--no-stdin] -- <cmd> [args...]`; `--` is required, so a command's own options are
/// never read as these.
fn parse(args: &[String]) -> Option<Options> {
    let mut options = Options {
        cwd: None,
        flush: true,
        no_stdin: false,
        argv: Vec::new(),
    };
    let mut args = args.iter();
    loop {
        match args.next()?.as_str() {
            "--cwd" => options.cwd = Some(args.next()?.clone()),
            "--no-flush" => options.flush = false,
            "--no-stdin" => options.no_stdin = true,
            "--" => break,
            _ => return None,
        }
    }
    options.argv = args.map(OsString::from).collect();
    (!options.argv.is_empty()).then_some(options)
}

/// `eitri host exec <args>` with this process's stdio, from the current directory.
pub fn run_exec(args: &[String], cx: &mut Context<'_>) -> i32 {
    let stdin = std::io::stdin();
    let stdin_is_terminal = stdin.is_terminal();
    let (mut stdout, mut stderr) = (std::io::stdout(), std::io::stderr());
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            exec::say(
                &mut stderr,
                &format!("the current directory cannot be read: {error}; nothing was run"),
            );
            return 125;
        }
    };
    run_exec_in(
        args,
        cx,
        &cwd,
        ProcessIo {
            stdin: Box::new(stdin),
            stdin_is_terminal,
            stdout: &mut stdout,
            stderr: &mut stderr,
        },
    )
}

/// `eitri host exec <args>` from `cwd` with `io`; returns the exit code.
pub fn run_exec_in(args: &[String], cx: &mut Context<'_>, cwd: &Path, io: ProcessIo<'_>) -> i32 {
    let Some(options) = parse(args) else {
        let _ = writeln!(io.stderr, "{USAGE}");
        return 125;
    };
    let (project, rel) = match super::cli_sync::project_for(cx, cwd, options.cwd.as_deref()) {
        Ok(found) => found,
        Err(why) => {
            exec::say(io.stderr, &format!("{why}; nothing was run"));
            return 125;
        }
    };
    // The mirror is at the project's path on the server: the same directory there.
    let server_cwd = match rel.as_str() {
        "" => project.root.clone(),
        rel => project.root.join(rel),
    };
    let connection = match cx.reach.exec_connection(&project) {
        Ok(connection) => connection,
        Err(why) => {
            let outcome = ExecOutcome::Unknown(format!("{} cannot be reached ({why}); nothing was run", project.host));
            return finish(outcome, io.stderr);
        }
    };
    let flush_cap = cx.reach.sync_settings().flush_cap;
    let request = ExecRequest {
        cwd: server_cwd,
        argv: options.argv,
        flush_before: options.flush,
        flush_after: options.flush,
        flush_cap,
    };
    let stdio = StdIo {
        stdin: io.stdin,
        close_stdin: options.no_stdin || io.stdin_is_terminal,
        stdout: io.stdout,
        stderr: &mut *io.stderr,
    };
    let outcome = exec::run(&connection, request, stdio, &mut |cap| flush(cx, &project, cap));
    finish(outcome, io.stderr)
}

/// Runs `load` with this process's stdout pointing at its stderr, then puts stdout back: what `init.lua` prints while
/// `exec`'s settings are read must not land in a command's output (a formatter's stdout is the file's new text). Both
/// Rust's and C's buffered stdout are flushed before stdout goes back, also when `load` panics. The saved descriptor
/// is close-on-exec, so a process `init.lua` starts and leaves running never holds the real stdout open. When the
/// descriptors cannot be moved, `load` runs as it is; when stdout cannot be put back, nothing of the command has run
/// yet and the process ends with 125 rather than send a command's output to stderr.
pub fn with_stdout_on_stderr<T>(load: impl FnOnce() -> T) -> T {
    let _ = std::io::stdout().flush();
    let _redirect = StdoutOnStderr::new();
    load()
}

/// Descriptor 1 as a copy of descriptor 2, the real stdout held aside until this is dropped.
pub(crate) struct StdoutOnStderr {
    saved: libc::c_int,
}

impl StdoutOnStderr {
    pub(crate) fn new() -> Option<Self> {
        // SAFETY: fcntl, dup2 and close take and return descriptors only; 1 and 2 are this process's own stdout and
        // stderr. F_DUPFD_CLOEXEC from 3 keeps the copy off 0 to 2 and out of every child.
        let saved = unsafe { libc::fcntl(1, libc::F_DUPFD_CLOEXEC, 3) };
        if saved < 0 {
            return None;
        }
        // SAFETY: as above; `saved` is the descriptor just made.
        if unsafe { libc::dup2(2, 1) } < 0 {
            unsafe { libc::close(saved) };
            return None;
        }
        Some(Self { saved })
    }

    /// Leaves descriptor 1 as a copy of stderr for the life of the process and gives the real stdout, close-on-exec:
    /// for a verb whose stdout is a protocol's (`eitri host lsp`), where nothing else may print.
    pub(crate) fn keep_stdout(self) -> std::os::fd::OwnedFd {
        use std::os::fd::FromRawFd;
        let saved = self.saved;
        std::mem::forget(self);
        // SAFETY: `saved` is the descriptor `new` made and nothing else owns it once the guard is forgotten.
        unsafe { std::os::fd::OwnedFd::from_raw_fd(saved) }
    }
}

impl Drop for StdoutOnStderr {
    fn drop(&mut self) {
        let _ = std::io::stdout().flush();
        // SAFETY: fflush(NULL) flushes C's open output streams; dup2 and close act on the descriptor saved in `new`.
        let restored = unsafe {
            libc::fflush(std::ptr::null_mut());
            let restored = libc::dup2(self.saved, 1) >= 0;
            libc::close(self.saved);
            restored
        };
        if !restored {
            exec::say(
                &mut std::io::stderr(),
                "stdout could not be put back after reading init.lua; nothing was run",
            );
            std::process::exit(125);
        }
    }
}

/// Says why a run that was refused or whose outcome is unknown ended so, and gives its exit code.
fn finish(outcome: ExecOutcome, stderr: &mut dyn Write) -> i32 {
    if let ExecOutcome::Refused(text) | ExecOutcome::Unknown(text) = &outcome {
        exec::say(stderr, text);
    }
    outcome.exit_code()
}

/// One flush of `project`'s tree session within `cap` altogether (the confirmation, then the check for conflicts with
/// what is left of it); a file that differs between the two sides (a conflict) counts as not carried, since the server
/// then keeps its own copy.
fn flush(cx: &mut Context<'_>, project: &RemoteProject, cap: Duration) -> Result<(), String> {
    let started = Instant::now();
    let tree = super::cli_sync::flush_project(cx, project, cap)?;
    let engine = cx.reach.sync_engine().map_err(|error| error.to_string())?;
    let left = cap.saturating_sub(started.elapsed());
    if left.is_zero() {
        return Err(format!(
            "no time was left within {} s to look for differing files",
            cap.as_secs_f64()
        ));
    }
    let status = engine
        .session_status_within(&tree, left)
        .map_err(|error| error.to_string())?;
    let differing = status.conflicts.len() as u64 + status.excluded_conflicts;
    if differing > 0 {
        return Err(format!(
            "{differing} file(s) differ between this laptop and {}",
            project.host
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The child half of [`what_init_lua_prints_while_settings_load_goes_to_stderr`].
    #[test]
    fn stdout_guard_child() {
        if std::env::var_os("EITRI_TEST_STDOUT_GUARD").is_none() {
            return;
        }
        let loaded = with_stdout_on_stderr(|| {
            print!("printed while loading");
            // SAFETY: a NUL-terminated literal through C's buffered stdout, as Lua's print writes.
            unsafe { libc::printf(c" and through C\n".as_ptr()) };
            7
        });
        println!("loaded {loaded}");
    }

    #[test]
    fn what_init_lua_prints_while_settings_load_goes_to_stderr() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "remote_host::cli_exec::tests::stdout_guard_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("EITRI_TEST_STDOUT_GUARD", "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .unwrap();
        let (stdout, stderr) = (
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(output.status.success(), "{stdout}{stderr}");
        assert!(stdout.contains("loaded 7"), "{stdout}");
        assert!(
            !stdout.contains("printed while loading") && !stdout.contains("through C"),
            "{stdout}"
        );
        assert!(
            stderr.contains("printed while loading") && stderr.contains("and through C"),
            "{stderr}"
        );
    }

    /// The descriptors a child of this process inherits, as `sh` lists them.
    fn inherited_descriptors() -> Vec<String> {
        let output = std::process::Command::new("sh")
            .args(["-c", "for fd in /proc/$$/fd/*; do echo ${fd##*/}; done"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .unwrap();
        let mut fds: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect();
        fds.sort();
        fds
    }

    /// The child half of [`the_descriptor_holding_the_real_stdout_does_not_reach_a_process_init_lua_starts`].
    #[test]
    fn stdout_guard_descriptor_child() {
        if std::env::var_os("EITRI_TEST_STDOUT_GUARD_FDS").is_none() {
            return;
        }
        let before = inherited_descriptors();
        let during = with_stdout_on_stderr(inherited_descriptors);
        assert_eq!(
            during, before,
            "a process started while stdout was moved holds the saved descriptor"
        );
        assert_eq!(inherited_descriptors(), before);
    }

    #[test]
    fn the_descriptor_holding_the_real_stdout_does_not_reach_a_process_init_lua_starts() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "remote_host::cli_exec::tests::stdout_guard_descriptor_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("EITRI_TEST_STDOUT_GUARD_FDS", "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "{text}");
        assert!(text.contains("1 passed"), "the child test ran: {text}");
    }

    fn args(line: &[&str]) -> Vec<String> {
        line.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn the_command_line_needs_its_separator_and_a_command() {
        let options = parse(&args(&[
            "--cwd",
            "src",
            "--no-flush",
            "--no-stdin",
            "--",
            "cargo",
            "--",
            "-x",
        ]))
        .unwrap();
        assert_eq!(options.cwd.as_deref(), Some("src"));
        assert!(!options.flush && options.no_stdin);
        assert_eq!(options.argv, ["cargo", "--", "-x"]);
        let options = parse(&args(&["--", "make"])).unwrap();
        assert!(options.flush && !options.no_stdin && options.cwd.is_none());
        for line in [
            &[][..],
            &["make"][..],
            &["--"][..],
            &["--cwd"][..],
            &["--cwd", "src"][..],
            &["--stdin", "--", "make"][..],
        ] {
            assert!(parse(&args(line)).is_none(), "{line:?}");
        }
    }
}
