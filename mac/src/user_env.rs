//! The user's command environment for everything the app starts besides Neovide's nvim: the sidecar, the
//! `claude` it runs, the agent's tools and the terminal handoff.
//!
//! LaunchServices (the Dock, Finder, `open`) starts an app with `PATH=/usr/bin:/bin:/usr/sbin:/sbin`, where no
//! `claude` installed by its own installer, Homebrew or npm is found: the sidecar's probe of bare `claude` failed
//! and no session started. Neovide meets the same problem for nvim and solves it by starting nvim through the
//! user's login shell when its parent is launchd (pid 1): `/usr/bin/login -fpq <user> <shell> -c <command>`.
//! Its function is private to the fork, so the same rule is applied here, once, at start: on such a start the
//! login shell is asked for its `PATH`, which becomes this process's (so every child inherits it, as nvim's
//! environment comes from that shell), and `claude` is resolved on it to an absolute path handed to the sidecar
//! as `VERDANDI_CLAUDE_CLI_PATH`, the one variable it reads to pick the CLI. A start from a terminal keeps the
//! environment it was given, as Neovide does.

use std::ffi::{CStr, OsStr, OsString};
use std::io::Read;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The variable the sidecar reads to pick which `claude` its SDK runs.
pub const CLI_PATH_VAR: &str = "VERDANDI_CLAUDE_CLI_PATH";

/// How long a login shell may take to say its `PATH`. A slow shell configuration delays the window by as much;
/// past it the inherited `PATH` is kept and the log says why.
pub const LOGIN_SHELL_BUDGET: Duration = Duration::from_secs(5);

/// The program Neovide runs nvim through on a start from the desktop.
pub const LOGIN: &str = "/usr/bin/login";

/// What the shell prints around its `PATH`, so whatever a shell configuration prints itself is not taken for it.
const BEGIN: &str = "__EITRI_PATH_BEGIN__";
const END: &str = "__EITRI_PATH_END__";

/// The shell command that prints `PATH` between the markers.
fn print_path_script() -> String {
    format!("printf '\\n%s%s%s\\n' {BEGIN} \"$PATH\" {END}")
}

/// Whether the app was started the way Neovide's rule calls a start from the desktop: its parent is launchd.
pub fn started_from_the_desktop(parent_pid: i32) -> bool {
    parent_pid == 1
}

/// The command that runs `shell` as `user`'s login shell, as Neovide does for nvim (`-f` no authentication for
/// the user already logged in, `-p` the environment kept, `-q` no login messages), printing its `PATH`.
pub fn login_shell_command(user: &OsStr, shell: &Path) -> (PathBuf, Vec<OsString>) {
    let args = vec![
        OsString::from("-fpq"),
        user.to_owned(),
        shell.as_os_str().to_owned(),
        OsString::from("-c"),
        OsString::from(print_path_script()),
    ];
    (PathBuf::from(LOGIN), args)
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

/// The most of a shell's output kept; what a configuration prints past it is read and dropped.
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
/// `cap`, the oldest bytes are dropped when `keep_tail`, else the newest.
fn read_round(reader: &mut dyn Read, into: &mut Vec<u8>, cap: usize, keep_tail: bool, deadline: Instant) -> Pipe {
    let mut chunk = [0u8; 4096];
    let mut read = 0;
    while read < READ_PER_ROUND && Instant::now() < deadline {
        match reader.read(&mut chunk) {
            Ok(0) => return Pipe::Closed,
            Ok(n) => {
                read += n;
                if keep_tail {
                    into.extend_from_slice(&chunk[..n]);
                    let over = into.len().saturating_sub(cap);
                    into.drain(..over);
                } else if into.len() < cap {
                    into.extend_from_slice(&chunk[..n]);
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
fn with_error_tail(why: String, stderr: &[u8]) -> String {
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
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("{} could not start: {e}", program.display()))?;
    let pid = child.id() as libc::pid_t;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    // Read without blocking, so a shell that never ends is still timed out.
    set_nonblocking(&stdout);
    set_nonblocking(&stderr);
    let deadline = Instant::now() + budget;
    let mut output = Vec::new();
    let mut errors = Vec::new();
    let (mut out, mut err) = (Pipe::Empty, Pipe::Empty);
    let status = loop {
        if out != Pipe::Closed {
            out = read_round(&mut stdout, &mut output, OUTPUT_CAP, false, deadline);
        }
        if err != Pipe::Closed {
            err = read_round(&mut stderr, &mut errors, ERROR_TAIL_BYTES, true, deadline);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) | Err(_) => {
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
        out = read_round(&mut stdout, &mut output, OUTPUT_CAP, false, deadline);
        if out == Pipe::Empty {
            break;
        }
    }
    while err != Pipe::Closed && Instant::now() < deadline {
        err = read_round(&mut stderr, &mut errors, ERROR_TAIL_BYTES, true, deadline);
        if err == Pipe::Empty {
            break;
        }
    }
    path_from_output(&output)
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

/// The first executable regular file named `name` in an absolute directory of `path`. Relative and empty entries
/// are skipped: the working directory is the project, and a program there must not stand in for the user's.
pub fn find_program(name: &str, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

/// Where the `PATH` the app runs with came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathFrom {
    /// Not started from the desktop: what the terminal or the starter gave.
    Inherited,
    /// The user's login shell, with the entries of its `PATH` that were not absolute directories left out.
    LoginShell { left_out: Vec<String> },
    /// Started from the desktop, but the login shell gave none; the inherited one is kept.
    InheritedShellFailed(String),
}

/// What [`decide`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandEnv {
    /// The `PATH` to run with.
    pub path: OsString,
    pub path_from: PathFrom,
    /// The `claude` the sidecar is to run: the inherited `VERDANDI_CLAUDE_CLI_PATH` when one was given, else
    /// `claude` on `path`.
    pub claude: Option<PathBuf>,
    /// Whether `claude` came from an inherited `VERDANDI_CLAUDE_CLI_PATH` (left as it is).
    pub cli_path_inherited: bool,
}

impl CommandEnv {
    /// The lines the app logs about it: which `PATH`, from where, and which `claude`.
    pub fn report(&self) -> Vec<String> {
        let from = match &self.path_from {
            PathFrom::Inherited => "inherited (not started from the desktop)".to_owned(),
            PathFrom::LoginShell { left_out } if left_out.is_empty() => {
                "from the login shell (started from the desktop)".to_owned()
            }
            PathFrom::LoginShell { left_out } => format!(
                "from the login shell (started from the desktop), leaving out what is not an absolute directory: {}",
                left_out.join(", ")
            ),
            PathFrom::InheritedShellFailed(why) => format!("inherited: the login shell gave none ({why})"),
        };
        let claude = match (&self.claude, self.cli_path_inherited) {
            (Some(claude), true) => format!("claude: {} ({CLI_PATH_VAR} inherited)", claude.display()),
            (Some(claude), false) => format!("claude: {} (found on PATH, set as {CLI_PATH_VAR})", claude.display()),
            (None, _) => "claude: not found on PATH; no agent session can start".to_owned(),
        };
        vec![
            format!("[mac] PATH {from}: {}", self.path.to_string_lossy()),
            format!("[mac] {claude}"),
        ]
    }
}

/// The command environment for a start whose parent is `parent_pid`, from the inherited `PATH` and
/// `VERDANDI_CLAUDE_CLI_PATH`. `login_path` asks the login shell; it is called only on a start from the desktop.
pub fn decide(
    parent_pid: i32,
    inherited_path: Option<OsString>,
    inherited_cli_path: Option<OsString>,
    login_path: &dyn Fn() -> Result<OsString, String>,
) -> CommandEnv {
    let inherited = inherited_path.unwrap_or_default();
    let (path, path_from) = if started_from_the_desktop(parent_pid) {
        match login_path().map(|path| (absolute_entries(&path), path)) {
            Ok((Some((path, left_out)), _)) => (path, PathFrom::LoginShell { left_out }),
            Ok((None, path)) => {
                let why = format!("its PATH {path:?} names no absolute directory");
                (inherited, PathFrom::InheritedShellFailed(why))
            }
            Err(why) => (inherited, PathFrom::InheritedShellFailed(why)),
        }
    } else {
        (inherited, PathFrom::Inherited)
    };
    let inherited_cli = inherited_cli_path.filter(|value| !value.is_empty()).map(PathBuf::from);
    let cli_path_inherited = inherited_cli.is_some();
    let claude = inherited_cli.or_else(|| find_program("claude", &path));
    CommandEnv {
        path,
        path_from,
        claude,
        cli_path_inherited,
    }
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

/// The login shell's `PATH`, by Neovide's invocation.
pub fn login_shell_path() -> Result<OsString, String> {
    let (user, shell) = user_and_shell().ok_or("no user or login shell in the password database")?;
    let (program, args) = login_shell_command(&user, &shell);
    path_from_command(&program, &args, LOGIN_SHELL_BUDGET).map_err(|why| format!("{}: {why}", shell.display()))
}

/// Decides the command environment for this process and puts it in place: `PATH` and, unless one was inherited,
/// `VERDANDI_CLAUDE_CLI_PATH`. Returns the lines to log. Call it before any thread exists.
pub fn apply() -> Vec<String> {
    // SAFETY: `getppid` takes nothing and cannot fail.
    let parent = unsafe { libc::getppid() };
    let env = decide(
        parent,
        std::env::var_os("PATH"),
        std::env::var_os(CLI_PATH_VAR),
        &login_shell_path,
    );
    // SAFETY: the caller runs this before any other thread exists, so nothing else reads the environment.
    unsafe {
        if matches!(env.path_from, PathFrom::LoginShell { .. }) {
            std::env::set_var("PATH", &env.path);
        }
        if let (Some(claude), false) = (&env.claude, env.cli_path_inherited) {
            std::env::set_var(CLI_PATH_VAR, claude);
        }
    }
    env.report()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAUNCHSERVICES_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

    fn executable(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A stand-in login shell: chatter from a shell configuration first, then the `PATH` it sets, printed by the
    /// same script the real invocation runs.
    fn stand_in_shell(dir: &Path, path: &str) -> PathBuf {
        let shell = dir.join("stand-in-shell");
        executable(
            &shell,
            &format!(
                "#!/bin/sh\necho 'Welcome, from a .zprofile'\nPATH='{path}'\nexport PATH\n{}\necho 'and after'\n",
                print_path_script()
            ),
        );
        shell
    }

    #[test]
    fn a_desktop_start_finds_claude_on_the_login_shells_path() {
        let tmp = tempfile::tempdir().unwrap();
        // Stands for ~/.local/bin or /opt/homebrew/bin: on the login shell's PATH, not on LaunchServices'.
        let bin = tmp.path().join("local-bin");
        std::fs::create_dir(&bin).unwrap();
        executable(&bin.join("claude"), "#!/bin/sh\necho 2.1.288\n");
        let login_path = format!("{}:{LAUNCHSERVICES_PATH}", bin.display());
        let shell = stand_in_shell(tmp.path(), &login_path);

        assert_eq!(find_program("claude", OsStr::new(LAUNCHSERVICES_PATH)), None, "{bin:?}");
        let env = decide(1, Some(LAUNCHSERVICES_PATH.into()), None, &|| {
            path_from_command(&shell, &[], Duration::from_secs(5))
        });
        assert_eq!(env.path_from, PathFrom::LoginShell { left_out: vec![] });
        assert_eq!(env.path, OsString::from(&login_path));
        assert_eq!(env.claude, Some(bin.join("claude")));
        assert!(!env.cli_path_inherited);
        let report = env.report();
        assert!(
            report[0].contains("from the login shell") && report[0].contains("local-bin"),
            "{report:?}"
        );
        assert!(
            report[1].contains("local-bin/claude") && report[1].contains(CLI_PATH_VAR),
            "{report:?}"
        );
    }

    #[test]
    fn a_terminal_start_keeps_its_path_and_never_asks_the_shell() {
        let env = decide(4242, Some(LAUNCHSERVICES_PATH.into()), None, &|| panic!("not asked"));
        assert_eq!(env.path_from, PathFrom::Inherited);
        assert_eq!(env.path, OsString::from(LAUNCHSERVICES_PATH));
    }

    #[test]
    fn an_inherited_cli_path_wins_and_is_kept() {
        let env = decide(
            1,
            Some(LAUNCHSERVICES_PATH.into()),
            Some("/opt/claude-wrapper".into()),
            &|| Ok(LAUNCHSERVICES_PATH.into()),
        );
        assert_eq!(env.claude, Some(PathBuf::from("/opt/claude-wrapper")));
        assert!(env.cli_path_inherited);
        assert!(env.report()[1].contains("inherited"), "{:?}", env.report());
        // An empty one counts as unset.
        let env = decide(4242, Some("/nowhere".into()), Some("".into()), &|| unreachable!());
        assert!(!env.cli_path_inherited);
        assert_eq!(env.claude, None);
    }

    #[test]
    fn a_login_shell_that_fails_or_hangs_leaves_the_inherited_path() {
        let tmp = tempfile::tempdir().unwrap();
        let silent = tmp.path().join("silent");
        executable(
            &silent,
            "#!/bin/sh\necho 'no markers here'\necho 'zsh: command not found: brew' >&2\nexit 127\n",
        );
        let why = path_from_command(&silent, &[], Duration::from_secs(5)).unwrap_err();
        assert!(why.contains("printed no PATH"), "{why}");
        // The log says why the shell failed in its own words.
        assert!(why.contains("command not found: brew"), "{why}");

        let hangs = tmp.path().join("hangs");
        executable(&hangs, "#!/bin/sh\nsleep 30 &\nsleep 30\n");
        let started = Instant::now();
        let why = path_from_command(&hangs, &[], Duration::from_millis(300)).unwrap_err();
        assert!(why.contains("no answer"), "{why}");
        assert!(started.elapsed() < Duration::from_secs(5), "killed at the budget");

        let env = decide(1, Some(LAUNCHSERVICES_PATH.into()), None, &|| Err("gave up".into()));
        assert_eq!(env.path, OsString::from(LAUNCHSERVICES_PATH));
        assert_eq!(env.path_from, PathFrom::InheritedShellFailed("gave up".into()));
        assert!(env.report()[0].contains("gave up"), "{:?}", env.report());
    }

    /// Whether any process of the group `pgid` is left, waiting up to `within` for the ones just killed to go.
    fn group_left(pgid: libc::pid_t, within: Duration) -> bool {
        let until = Instant::now() + within;
        loop {
            // SAFETY: signal 0 only asks whether the group still has a member this user may signal.
            if unsafe { libc::kill(-pgid, 0) } != 0 {
                return false;
            }
            if Instant::now() >= until {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_login_shell_that_prints_without_end_is_cut_at_the_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_file = tmp.path().join("pid");
        let endless = tmp.path().join("endless");
        // A configuration with a stray `yes` on each stream, one of them in a child of its own.
        executable(
            &endless,
            "#!/bin/sh\necho $$ > \"$1\"\nyes 'on stderr' >&2 &\nyes 'on stdout'\n",
        );
        let budget = Duration::from_millis(500);
        let started = Instant::now();
        let why = path_from_command(&endless, &[pid_file.clone().into_os_string()], budget).unwrap_err();
        assert!(
            started.elapsed() < budget + Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert!(why.contains("no answer"), "{why}");
        assert!(why.contains("on stderr"), "the end of its errors: {why}");
        let pgid: libc::pid_t = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        assert!(
            !group_left(pgid, Duration::from_secs(3)),
            "process group {pgid} was not killed"
        );
    }

    #[test]
    fn only_the_last_lines_of_the_errors_are_quoted() {
        let errors: String = (1..=40).map(|n| format!("line {n}\n\n")).collect();
        assert_eq!(
            error_tail(errors.as_bytes()),
            "line 36 | line 37 | line 38 | line 39 | line 40"
        );
        assert_eq!(error_tail(b"  \n"), "");
        assert_eq!(with_error_tail("failed".into(), b""), "failed");
    }

    #[test]
    fn a_login_path_without_an_absolute_directory_keeps_the_inherited_one() {
        // nushell does not expand "$PATH": the markers hold the literal text.
        let env = decide(1, Some(LAUNCHSERVICES_PATH.into()), None, &|| Ok("$PATH".into()));
        assert_eq!(env.path, OsString::from(LAUNCHSERVICES_PATH));
        assert!(
            matches!(&env.path_from, PathFrom::InheritedShellFailed(why) if why.contains("no absolute directory")),
            "{:?}",
            env.path_from
        );
        assert!(env.report()[0].contains("$PATH"), "{:?}", env.report());
        let env = decide(1, Some(LAUNCHSERVICES_PATH.into()), None, &|| Ok("relative:.:".into()));
        assert_eq!(env.path, OsString::from(LAUNCHSERVICES_PATH));
    }

    #[test]
    fn relative_and_empty_entries_of_a_login_path_are_left_out() {
        let env = decide(1, Some(LAUNCHSERVICES_PATH.into()), None, &|| {
            Ok("/opt/homebrew/bin::bin:/usr/bin".into())
        });
        assert_eq!(env.path, OsString::from("/opt/homebrew/bin:/usr/bin"));
        assert_eq!(
            env.path_from,
            PathFrom::LoginShell {
                left_out: vec!["\"\"".into(), "\"bin\"".into()]
            }
        );
        let report = env.report();
        assert!(
            report[0].contains("leaving out") && report[0].contains("\"bin\""),
            "{report:?}"
        );
    }

    #[test]
    fn only_executable_files_in_absolute_directories_count() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(a.join("claude")).unwrap(); // a directory of that name
        std::fs::create_dir(&b).unwrap();
        std::fs::write(b.join("claude"), "").unwrap(); // not executable
        let c = tmp.path().join("c");
        std::fs::create_dir(&c).unwrap();
        executable(&c.join("claude"), "#!/bin/sh\n");
        let path = std::env::join_paths([a.as_path(), b.as_path(), Path::new("relative"), Path::new(""), &c]).unwrap();
        assert_eq!(find_program("claude", &path), Some(c.join("claude")));
    }

    #[test]
    fn the_path_is_read_between_the_last_markers() {
        let out = format!("motd\n{BEGIN}/fake{END}\nnoise\n{BEGIN}/usr/bin:/bin{END}\nbye\n");
        assert_eq!(path_from_output(out.as_bytes()), Some(OsString::from("/usr/bin:/bin")));
        assert_eq!(path_from_output(b"no markers"), None);
        assert_eq!(path_from_output(format!("{BEGIN}{END}").as_bytes()), None);
    }

    #[test]
    fn the_login_invocation_is_neovides() {
        let (program, args) = login_shell_command(OsStr::new("grey"), Path::new("/bin/zsh"));
        assert_eq!(program, Path::new("/usr/bin/login"));
        assert_eq!(&args[..4], ["-fpq", "grey", "/bin/zsh", "-c"].map(OsString::from));
        assert!(args[4].to_string_lossy().contains("\"$PATH\""));
        assert!(started_from_the_desktop(1));
        assert!(!started_from_the_desktop(0) && !started_from_the_desktop(4242));
    }
}
