//! The `PATH` of the user's login shell, for a start that did not come from a terminal.
//!
//! A program started from a desktop launcher, a session manager or the macOS Dock gets that starter's `PATH`, where
//! what the user's shell configuration adds (`~/.local/bin`, `~/.cargo/bin`, a version manager's directory) is
//! missing; the same program from a terminal has it. As VS Code does, such a start asks the login shell once: it runs
//! with input from `/dev/null`, as the leader of its own process group, killed with that group at a budget, and
//! prints its `PATH` between markers so whatever the configuration prints itself is not taken for it
//! ([`path_from_command`]). Only `PATH` is taken, and only its absolute entries ([`absolute_entries`]). The Linux
//! window runs `$SHELL -i -l -c` ([`linux_shell_command`]); the macOS app runs Neovide's `/usr/bin/login` invocation
//! over the same reader (`eitri-mac`'s `user_env`).

use std::ffi::{CStr, OsStr, OsString};
use std::io::Read;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Set to `1` on the shell this asks, so a configuration can skip what only an interactive terminal needs (VS Code
/// sets `VSCODE_RESOLVING_ENVIRONMENT` the same way).
pub const RESOLVING_VAR: &str = "EITRI_RESOLVING_ENVIRONMENT";

/// What the shell prints around its `PATH`, so whatever a shell configuration prints itself is not taken for it.
pub const BEGIN: &str = "__EITRI_PATH_BEGIN__";
pub const END: &str = "__EITRI_PATH_END__";

/// The shell command that prints `PATH` between the markers.
pub fn print_path_script() -> String {
    format!("printf '\\n%s%s%s\\n' {BEGIN} \"$PATH\" {END}")
}

/// The `PATH` between the last pair of markers in a shell's output. `None` without one, or when it is empty.
pub fn path_from_output(stdout: &[u8]) -> Option<OsString> {
    let begin = find_last(stdout, BEGIN.as_bytes())? + BEGIN.len();
    let rest = &stdout[begin..];
    let end = find_first(rest, END.as_bytes())?;
    let path = &rest[..end];
    (!path.is_empty()).then(|| OsString::from_vec(path.to_vec()))
}

fn find_last(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|w| w == needle)
}

fn find_first(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The most of a shell's output kept, from its end (between this and twice this): the markers come last, so a
/// configuration that prints more than this first loses only its own words.
const OUTPUT_CAP: usize = 1 << 20;

/// The most read from one stream before the deadline is looked at again: a configuration that prints without
/// end (a stray `yes`) never leaves the pipe empty, and must not hold the read past the budget.
const READ_PER_ROUND: usize = 64 * 1024;

/// How much of the shell's error output is kept for the log, from its end.
const ERROR_TAIL_BYTES: usize = 2048;

/// How many of the error output's last lines a failure reason quotes.
const ERROR_TAIL_LINES: usize = 5;

/// What one [`read_round`] left the pipe as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pipe {
    /// Every writer closed it.
    Closed,
    /// Nothing to read now.
    Empty,
    /// The round ended at its byte bound or the deadline with more perhaps waiting.
    More,
}

/// Reads what `reader` has now into `into`: at most [`READ_PER_ROUND`] bytes, and never past `deadline`. Past
/// twice `cap` the oldest bytes are dropped down to `cap`, and `before_drop` sees the whole buffer first.
fn read_round(
    reader: &mut dyn Read,
    into: &mut Vec<u8>,
    cap: usize,
    before_drop: &mut dyn FnMut(&[u8]),
    deadline: Instant,
) -> Pipe {
    let mut chunk = [0u8; 4096];
    let mut read = 0;
    while read < READ_PER_ROUND && Instant::now() < deadline {
        match reader.read(&mut chunk) {
            Ok(0) => return Pipe::Closed,
            Ok(n) => {
                read += n;
                into.extend_from_slice(&chunk[..n]);
                if into.len() > 2 * cap {
                    before_drop(into);
                    let over = into.len() - cap;
                    into.drain(..over);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Pipe::Empty,
            Err(_) => return Pipe::Closed,
        }
    }
    Pipe::More
}

fn set_nonblocking(fd: &impl std::os::fd::AsRawFd) {
    let fd = fd.as_raw_fd();
    // SAFETY: `fcntl` on a pipe's read end, which the caller owns for the length of the call.
    unsafe { libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK) };
}

/// The last few non-blank lines of a shell's error output, joined for one log line; empty when it said nothing.
pub fn error_tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    lines[lines.len().saturating_sub(ERROR_TAIL_LINES)..].join(" | ")
}

/// A failure reason with the shell's last error lines after it, when it printed any.
pub fn with_error_tail(why: String, stderr: &[u8]) -> String {
    match error_tail(stderr) {
        tail if tail.is_empty() => why,
        tail => format!("{why}; it said: {tail}"),
    }
}

/// Runs `program` with `args` and reads the `PATH` it prints between the markers, within `budget`. Its input is
/// `/dev/null`; the end of its error output is kept for the failure reason. Past the budget its whole process
/// group (it is started as the leader of its own) is killed by the pid it was given, so a shell configuration's
/// own children go too; a shell that prints without end is no exception, since no read outlasts the budget.
pub fn path_from_command(program: &Path, args: &[OsString], budget: Duration) -> Result<OsString, String> {
    path_from_command_waiting(program, args, budget, &mut |child| child.try_wait())
}

/// [`path_from_command`] with the wait for the shell given, so a test can make it fail as a starter that ignores
/// SIGCHLD makes it fail (the kernel reaps the shell, and the wait says ECHILD).
fn path_from_command_waiting(
    program: &Path,
    args: &[OsString],
    budget: Duration,
    try_wait: &mut dyn FnMut(&mut std::process::Child) -> std::io::Result<Option<std::process::ExitStatus>>,
) -> Result<OsString, String> {
    let spawned = Command::new(program)
        .args(args)
        .env(RESOLVING_VAR, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn();
    let mut child = spawned.map_err(|e| format!("{} could not start: {e}", program.display()))?;
    let pid = child.id() as libc::pid_t;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    // Read without blocking, so a shell that never ends is still timed out.
    set_nonblocking(&stdout);
    set_nonblocking(&stderr);
    let deadline = Instant::now() + budget;
    let mut output = Vec::new();
    let mut errors = Vec::new();
    // The last PATH seen in output that has since been dropped; the kept output's own comes first.
    let mut dropped_path: Option<OsString> = None;
    let mut keep_path = |buffer: &[u8]| {
        if let Some(path) = path_from_output(buffer) {
            dropped_path = Some(path);
        }
    };
    let mut nothing = |_: &[u8]| {};
    let (mut out, mut err) = (Pipe::Empty, Pipe::Empty);
    let status = loop {
        if out != Pipe::Closed {
            out = read_round(&mut stdout, &mut output, OUTPUT_CAP, &mut keep_path, deadline);
        }
        if err != Pipe::Closed {
            err = read_round(&mut stderr, &mut errors, ERROR_TAIL_BYTES, &mut nothing, deadline);
        }
        match try_wait(&mut child) {
            Ok(Some(status)) => break status.to_string(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            // The shell cannot be waited for: it has ended and was reaped already (SIGCHLD ignored by whoever
            // started this window). What it printed decides, read until its pipe closes, its PATH has come (a job
            // its configuration left may hold the pipe open) or the budget ends.
            Err(e) => {
                while out != Pipe::Closed && Instant::now() < deadline {
                    out = read_round(&mut stdout, &mut output, OUTPUT_CAP, &mut keep_path, deadline);
                    if path_from_output(&output).is_some() {
                        break;
                    }
                    if out == Pipe::Empty {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                if let Some(path) = path_from_output(&output).or(dropped_path) {
                    return Ok(path);
                }
                // SAFETY: as below; a group with no member left makes the call fail harmlessly.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
                let why = format!(
                    "{} could not be waited for ({e}) and printed no PATH",
                    program.display()
                );
                return Err(with_error_tail(why, &errors));
            }
            Ok(None) => {
                // SAFETY: `pid` is the child just started, the leader of its own process group; it has not been
                // waited for, so the number still names it.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
                let _ = child.wait();
                let why = format!("{} gave no answer within {budget:?}", program.display());
                return Err(with_error_tail(why, &errors));
            }
        }
    };
    // What it wrote just before it ended, until the pipe is empty. A child it left behind may hold the pipe open
    // and keep writing; the reads still end at the budget.
    while out != Pipe::Closed && Instant::now() < deadline {
        out = read_round(&mut stdout, &mut output, OUTPUT_CAP, &mut keep_path, deadline);
        if out == Pipe::Empty {
            break;
        }
    }
    while err != Pipe::Closed && Instant::now() < deadline {
        err = read_round(&mut stderr, &mut errors, ERROR_TAIL_BYTES, &mut nothing, deadline);
        if err == Pipe::Empty {
            break;
        }
    }
    path_from_output(&output)
        .or(dropped_path)
        .ok_or_else(|| with_error_tail(format!("{} printed no PATH ({status})", program.display()), &errors))
}

/// `path` with every entry that is not an absolute directory left out (a relative or empty one would search the
/// working directory, the project; a shell that does not expand `"$PATH"`, such as nushell, yields the literal
/// text), and the entries left out. `None` when no entry is absolute.
pub fn absolute_entries(path: &OsStr) -> Option<(OsString, Vec<String>)> {
    let (kept, dropped): (Vec<PathBuf>, Vec<PathBuf>) = std::env::split_paths(path).partition(|dir| dir.is_absolute());
    if kept.is_empty() {
        return None;
    }
    let joined = std::env::join_paths(&kept).ok()?;
    let dropped = dropped.iter().map(|dir| format!("{:?}", dir.as_os_str())).collect();
    Some((joined, dropped))
}

/// This user's name and login shell, from the password database.
pub fn user_and_shell() -> Option<(OsString, PathBuf)> {
    // SAFETY: `getpwuid_r` fills `entry` with pointers into `buffer`, both alive for the reads below; a zero
    // return with a non-null result means the entry is complete.
    unsafe {
        let mut entry: libc::passwd = std::mem::zeroed();
        let mut buffer = vec![0 as libc::c_char; 4096];
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let uid = libc::geteuid();
        if libc::getpwuid_r(uid, &mut entry, buffer.as_mut_ptr(), buffer.len(), &mut result) != 0 || result.is_null() {
            return None;
        }
        if entry.pw_name.is_null() || entry.pw_shell.is_null() {
            return None;
        }
        let name = OsStr::from_bytes(CStr::from_ptr(entry.pw_name).to_bytes()).to_owned();
        let shell = PathBuf::from(OsStr::from_bytes(CStr::from_ptr(entry.pw_shell).to_bytes()));
        (!name.is_empty() && shell.is_absolute()).then_some((name, shell))
    }
}

/// How long the Linux window lets the login shell take: a slow configuration delays a start from the desktop by as
/// much, and past it the inherited `PATH` is kept and the log says why.
pub const LINUX_BUDGET: Duration = Duration::from_secs(2);

/// The login shell to ask: `$SHELL` when it is an absolute path, else the password database's entry.
pub fn login_shell(env_shell: Option<&OsStr>, passwd_shell: &dyn Fn() -> Option<PathBuf>) -> Option<PathBuf> {
    env_shell
        .map(PathBuf::from)
        .filter(|shell| shell.is_absolute())
        .or_else(passwd_shell)
}

/// The arguments that make `shell` an interactive login shell running the `PATH` script, as VS Code passes them:
/// `-ic` for csh and tcsh, which take no `-l` beside `-c`, else `-i -l -c`.
pub fn linux_shell_command(shell: &Path) -> Vec<OsString> {
    let name = shell.file_name().map(OsStr::to_string_lossy).unwrap_or_default();
    let mut args: Vec<OsString> = match name.as_ref() {
        "csh" | "tcsh" => vec!["-ic".into()],
        _ => vec!["-i".into(), "-l".into(), "-c".into()],
    };
    args.push(print_path_script().into());
    args
}

/// What a Linux start did with its `PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginPath {
    /// The login shell's, with what was not an absolute directory left out.
    FromShell {
        path: OsString,
        shell: PathBuf,
        left_out: Vec<String>,
        took: Duration,
    },
    /// The inherited one is kept, and why.
    Kept(String),
}

impl LoginPath {
    /// The one line a start logs about it.
    pub fn log_line(&self) -> String {
        match self {
            LoginPath::FromShell {
                shell, left_out, took, ..
            } if left_out.is_empty() => format!(
                "PATH from the login shell {} ({} ms; not started from a terminal)",
                shell.display().to_string().escape_debug(),
                took.as_millis()
            ),
            LoginPath::FromShell {
                shell, left_out, took, ..
            } => format!(
                "PATH from the login shell {} ({} ms; not started from a terminal), leaving out what is not an \
                 absolute directory: {}",
                shell.display().to_string().escape_debug(),
                took.as_millis(),
                left_out.join(", ").escape_debug()
            ),
            LoginPath::Kept(why) => format!("PATH kept as inherited: {}", why.escape_debug()),
        }
    }
}

/// Asks `shell` for its `PATH` within `budget`, through `run` ([`path_from_command`] in a start).
pub fn ask_login_shell(
    shell: Option<PathBuf>,
    budget: Duration,
    run: &dyn Fn(&Path, &[OsString], Duration) -> Result<OsString, String>,
) -> LoginPath {
    let Some(shell) = shell else {
        return LoginPath::Kept(
            "no login shell: $SHELL is not an absolute path and the password database has none".into(),
        );
    };
    let started = Instant::now();
    match run(&shell, &linux_shell_command(&shell), budget) {
        Ok(path) => match absolute_entries(&path) {
            Some((path, left_out)) => LoginPath::FromShell {
                path,
                shell,
                left_out,
                took: started.elapsed(),
            },
            None => LoginPath::Kept(format!(
                "{}: its PATH {path:?} names no absolute directory",
                shell.display()
            )),
        },
        Err(why) => LoginPath::Kept(format!("{}: {why}", shell.display())),
    }
}

/// The password database's login shell for this user.
pub fn passwd_shell() -> Option<PathBuf> {
    user_and_shell().map(|(_, shell)| shell)
}

/// `off` keeps a Linux window started from the desktop on the `PATH` it was given.
pub const LINUX_SWITCH_VAR: &str = "EITRI_LOGIN_PATH";

/// Whether a Linux window start asks the login shell: the launcher said it did not come from a terminal
/// (`launched_from` is `desktop`) and [`LINUX_SWITCH_VAR`] is not `off`. A window started any other way, by the
/// binary itself or from a terminal, keeps the `PATH` it was given.
pub fn wanted(launched_from: Option<&OsStr>, switch: Option<&OsStr>) -> bool {
    launched_from.is_some_and(|from| from == "desktop") && !switch.is_some_and(|value| value == "off")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("eitri-login-path-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
        /// Written by a short-lived `sh`: a write descriptor held by this process could be inherited by a
        /// concurrent test's child, and running the script then fails with ETXTBSY until that child execs.
        fn script(&self, name: &str, text: &str) -> PathBuf {
            let path = self.0.join(name);
            let status = std::process::Command::new("sh")
                .args(["-c", r#"printf '%s' "$1" > "$2" && chmod 755 "$2""#, "sh", text])
                .arg(&path)
                .status()
                .unwrap();
            assert!(status.success(), "writing {} failed: {status}", path.display());
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn only_a_start_the_launcher_says_is_not_from_a_terminal_asks() {
        assert!(wanted(Some(OsStr::new("desktop")), None));
        assert!(!wanted(Some(OsStr::new("desktop")), Some(OsStr::new("off"))));
        assert!(wanted(Some(OsStr::new("desktop")), Some(OsStr::new("on"))));
        assert!(!wanted(Some(OsStr::new("terminal")), None));
        assert!(!wanted(None, None));
        assert!(!wanted(Some(OsStr::new("")), None));
    }

    #[test]
    fn the_shell_is_shell_if_absolute_else_the_password_databases() {
        let passwd = || Some(PathBuf::from("/usr/bin/zsh"));
        assert_eq!(
            login_shell(Some(OsStr::new("/bin/bash")), &passwd),
            Some("/bin/bash".into())
        );
        assert_eq!(
            login_shell(Some(OsStr::new("bash")), &passwd),
            Some("/usr/bin/zsh".into())
        );
        assert_eq!(login_shell(Some(OsStr::new("")), &passwd), Some("/usr/bin/zsh".into()));
        assert_eq!(login_shell(None, &passwd), Some("/usr/bin/zsh".into()));
        assert_eq!(login_shell(None, &|| None), None);
    }

    #[test]
    fn the_invocation_is_vs_codes() {
        let script = OsString::from(print_path_script());
        assert_eq!(
            linux_shell_command(Path::new("/usr/bin/zsh")),
            vec!["-i".into(), "-l".into(), "-c".into(), script.clone()]
        );
        assert_eq!(
            linux_shell_command(Path::new("/bin/tcsh")),
            vec!["-ic".into(), script.clone()]
        );
        assert_eq!(linux_shell_command(Path::new("/bin/csh")), vec!["-ic".into(), script]);
    }

    /// A stand-in for `$SHELL -i -l -c SCRIPT`: a configuration's banner on both streams, a `PATH` of its own, then
    /// the script, and a record of what it was given.
    fn stand_in(scratch: &Scratch, path: &str) -> PathBuf {
        let record = scratch.0.join("record");
        scratch.script(
            "shell",
            &format!(
                "#!/bin/sh\necho 'Welcome to zsh, with a banner'\necho 'a warning' >&2\n\
                 {{ echo \"args=$1 $2 $3\"; echo \"resolving=${{{RESOLVING_VAR}:-}}\"; \
                 if read -r line; then echo stdin=data; else echo stdin=empty; fi; }} > '{}'\n\
                 shift 3\nPATH='{path}'\nexport PATH\neval \"$1\"\necho 'and a goodbye'\n",
                record.display()
            ),
        )
    }

    #[test]
    fn a_login_shell_with_a_banner_gives_its_path_and_gets_dev_null_and_the_resolving_variable() {
        let scratch = Scratch::new();
        let shell = stand_in(&scratch, "/home/u/.local/bin:relative:/usr/bin");
        let got = ask_login_shell(Some(shell.clone()), Duration::from_secs(10), &path_from_command);
        let LoginPath::FromShell { path, left_out, .. } = &got else {
            panic!("{got:?}")
        };
        assert_eq!(path, &OsString::from("/home/u/.local/bin:/usr/bin"));
        assert_eq!(left_out, &vec!["\"relative\"".to_string()]);
        let record = std::fs::read_to_string(scratch.0.join("record")).unwrap();
        assert!(record.contains("args=-i -l -c"), "{record}");
        assert!(record.contains("resolving=1"), "{record}");
        assert!(record.contains("stdin=empty"), "{record}");
        assert!(got.log_line().contains("leaving out"), "{}", got.log_line());
    }

    #[test]
    fn a_shell_that_hangs_is_killed_at_the_budget_and_the_path_is_kept() {
        let scratch = Scratch::new();
        let shell = scratch.script("shell", "#!/bin/sh\necho 'starting tmux'\nsleep 30 &\nsleep 30\n");
        let started = Instant::now();
        let got = ask_login_shell(Some(shell), Duration::from_millis(300), &path_from_command);
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        assert!(
            matches!(&got, LoginPath::Kept(why) if why.contains("no answer")),
            "{got:?}"
        );
    }

    #[test]
    fn a_shell_that_cannot_be_waited_for_is_read_to_its_end() {
        // A starter that ignores SIGCHLD has the kernel reap the shell, and every wait then fails with ECHILD.
        let scratch = Scratch::new();
        let shell = stand_in(&scratch, "/opt/reaped/bin:/usr/bin");
        let args = linux_shell_command(&shell);
        let got = path_from_command_waiting(&shell, &args, Duration::from_secs(10), &mut |_| {
            std::thread::sleep(Duration::from_millis(20));
            Err(std::io::Error::from_raw_os_error(libc::ECHILD))
        });
        assert_eq!(got, Ok(OsString::from("/opt/reaped/bin:/usr/bin")));
        // With nothing printed, the reason says the wait failed, not that the shell hung.
        let silent = scratch.script("silent", "#!/bin/sh\nexit 0\n");
        let got = path_from_command_waiting(&silent, &[], Duration::from_secs(10), &mut |_| {
            Err(std::io::Error::from_raw_os_error(libc::ECHILD))
        });
        assert!(
            matches!(&got, Err(why) if why.contains("could not be waited for")),
            "{got:?}"
        );
    }

    #[test]
    fn a_shell_that_cannot_be_waited_for_and_leaves_a_job_on_its_output_returns_at_its_path() {
        let scratch = Scratch::new();
        let pidfile = scratch.0.join("job.pid");
        let shell = scratch.script(
            "shell",
            &format!(
                "#!/bin/sh\nsleep 30 &\necho $! > '{}'\nshift 3\nPATH=/opt/job/bin:/usr/bin\nexport PATH\n\
                 eval \"$1\"\nexit 0\n",
                pidfile.display()
            ),
        );
        let args = linux_shell_command(&shell);
        let started = Instant::now();
        let got = path_from_command_waiting(&shell, &args, Duration::from_secs(10), &mut |_| {
            std::thread::sleep(Duration::from_millis(20));
            Err(std::io::Error::from_raw_os_error(libc::ECHILD))
        });
        let took = started.elapsed();
        if let Ok(pid) = std::fs::read_to_string(&pidfile)
            .unwrap_or_default()
            .trim()
            .parse::<libc::pid_t>()
        {
            // SAFETY: the pid the stand-in wrote of the `sleep` it started; this test ends only that one.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        assert_eq!(got, Ok(OsString::from("/opt/job/bin:/usr/bin")));
        assert!(took < Duration::from_secs(3), "{took:?}");
    }

    #[test]
    fn a_banner_with_the_begin_marker_in_it_does_not_take_the_path() {
        let scratch = Scratch::new();
        let shell = scratch.script(
            "shell",
            &format!(
                "#!/bin/sh\necho 'motd: {BEGIN}/not/this:/bin'\necho 'more banner'\nshift 3\n\
                 PATH=/opt/right/bin:/usr/bin\nexport PATH\neval \"$1\"\n"
            ),
        );
        let got = ask_login_shell(Some(shell), Duration::from_secs(10), &path_from_command);
        assert!(
            matches!(&got, LoginPath::FromShell { path, .. } if path == &OsString::from("/opt/right/bin:/usr/bin")),
            "{got:?}"
        );
    }

    #[test]
    fn more_output_than_the_cap_still_gives_the_path() {
        let scratch = Scratch::new();
        let shell = scratch.script(
            "shell",
            "#!/bin/sh\nhead -c 3000000 /dev/zero | tr '\\0' x\necho\nshift 3\n\
             PATH=/opt/after-banner/bin:/usr/bin\nexport PATH\neval \"$1\"\n",
        );
        let got = ask_login_shell(Some(shell), Duration::from_secs(10), &path_from_command);
        let want = OsString::from("/opt/after-banner/bin:/usr/bin");
        assert!(
            matches!(&got, LoginPath::FromShell { path, .. } if path == &want),
            "{got:?}"
        );
        // A PATH printed before more than the cap of output is still the answer.
        let then_noise = scratch.script(
            "noisy",
            "#!/bin/sh\nshift 3\nPATH=/opt/before-noise/bin:/usr/bin\nexport PATH\neval \"$1\"\n\
             head -c 3000000 /dev/zero | tr '\\0' y\n",
        );
        let got = ask_login_shell(Some(then_noise), Duration::from_secs(10), &path_from_command);
        let want = OsString::from("/opt/before-noise/bin:/usr/bin");
        assert!(
            matches!(&got, LoginPath::FromShell { path, .. } if path == &want),
            "{got:?}"
        );
    }

    #[test]
    fn a_shell_that_exits_with_a_child_holding_its_output_returns_at_once() {
        let scratch = Scratch::new();
        let pidfile = scratch.0.join("child.pid");
        let shell = scratch.script(
            "shell",
            &format!(
                "#!/bin/sh\nsleep 30 &\necho $! > '{}'\nshift 3\nPATH=/opt/held/bin:/usr/bin\nexport PATH\n\
                 eval \"$1\"\nexit 0\n",
                pidfile.display()
            ),
        );
        let started = Instant::now();
        let got = ask_login_shell(Some(shell), Duration::from_secs(10), &path_from_command);
        let took = started.elapsed();
        if let Ok(pid) = std::fs::read_to_string(&pidfile)
            .unwrap_or_default()
            .trim()
            .parse::<libc::pid_t>()
        {
            // SAFETY: the pid the stand-in wrote of the `sleep` it started; this test ends only that one.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        assert!(took < Duration::from_secs(2), "{took:?}");
        let want = OsString::from("/opt/held/bin:/usr/bin");
        assert!(
            matches!(&got, LoginPath::FromShell { path, .. } if path == &want),
            "{got:?}"
        );
    }

    #[test]
    fn odd_shells_and_odd_answers_keep_the_inherited_path() {
        let scratch = Scratch::new();
        // A shell that exits before running the script (an rc that `exec`s something that fails).
        let exits = scratch.script(
            "exits",
            "#!/bin/sh\necho 'open terminal failed: not a terminal' >&2\nexit 1\n",
        );
        let got = ask_login_shell(Some(exits), Duration::from_secs(10), &path_from_command);
        assert!(
            matches!(&got, LoginPath::Kept(why) if why.contains("printed no PATH") && why.contains("not a terminal")),
            "{got:?}"
        );
        // A shell whose `$PATH` is no list of directories (nushell prints the literal text).
        let literal = |_: &Path, _: &[OsString], _: Duration| Ok(OsString::from("[/usr/bin /bin]"));
        let got = ask_login_shell(Some("/usr/bin/nu".into()), Duration::from_secs(1), &literal);
        assert!(
            matches!(&got, LoginPath::Kept(why) if why.contains("no absolute directory")),
            "{got:?}"
        );
        // A shell that is not there.
        let got = ask_login_shell(Some(scratch.0.join("none")), Duration::from_secs(1), &path_from_command);
        assert!(
            matches!(&got, LoginPath::Kept(why) if why.contains("could not start")),
            "{got:?}"
        );
        // No shell at all.
        assert!(matches!(
            ask_login_shell(None, Duration::from_secs(1), &path_from_command),
            LoginPath::Kept(_)
        ));
        // A log line holds no raw control character.
        let got = LoginPath::Kept("a\nb\u{1b}".into());
        assert!(!got.log_line().contains('\n') && !got.log_line().contains('\u{1b}'));
    }
}
