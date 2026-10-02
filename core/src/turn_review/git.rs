//! The one place in turn review that starts `git`.
//!
//! Turn review keeps its snapshots in a git directory of its own and runs git against the user's
//! project as a work tree. That git must behave identically on every machine: no setting from the
//! user's global or system config, and no `GIT_*` variable inherited from whatever started Eitri,
//! may reach it -- either could run a clean filter, an fsmonitor hook or a line-ending conversion
//! over the user's files, which would execute arbitrary commands and make a snapshot differ from
//! the bytes on disk. So [`command`] clears the environment and rebuilds it from a short allowlist,
//! names the git directory and the work tree explicitly, and never goes through a shell.
//!
//! Two read-only questions are asked of git in the user's own environment, because their answers
//! live there: where the user's global excludes file is ([`user_excludes_file`]), and where a
//! project's common git directory is ([`user_read_only`]). Neither writes anything.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The fixed identity every shadow commit carries. With the global config gone, `commit-tree`
/// would otherwise refuse to run for want of an email address.
pub const IDENTITY_NAME: &str = "eitri";
/// See [`IDENTITY_NAME`].
pub const IDENTITY_EMAIL: &str = "eitri@localhost";

/// Why a git run did not produce output.
#[derive(Debug)]
pub enum GitError {
    /// `git` could not be started at all (not installed, not on `PATH`, the work tree is gone).
    Spawn(std::io::Error),
    /// The run outlived its time and was killed.
    TimedOut,
    /// Writing the run's input or reading its output failed.
    Io(std::io::Error),
    /// git exited unsuccessfully; `stderr` is its own explanation, trimmed.
    Failed {
        what: String,
        status: ExitStatus,
        stderr: String,
    },
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitError::Spawn(e) => write!(f, "git could not be started: {e}"),
            GitError::TimedOut => write!(f, "git took too long and was stopped"),
            GitError::Io(e) => write!(f, "talking to git failed: {e}"),
            GitError::Failed { what, status, stderr } if stderr.is_empty() => write!(f, "git {what} failed ({status})"),
            GitError::Failed { what, stderr, .. } => write!(f, "git {what} failed: {stderr}"),
        }
    }
}

impl std::error::Error for GitError {}

impl GitError {
    /// `Err(Failed)` unless `output` reports success; `what` names the subcommand for the message.
    pub fn check(what: &str, output: Output) -> Result<Output, GitError> {
        if output.status.success() {
            Ok(output)
        } else {
            Err(GitError::Failed {
                what: what.to_owned(),
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            })
        }
    }
}

/// The isolated git command for a shadow git directory `git_dir` over the work tree `work_tree`,
/// with `index` as its index file when given. Add the subcommand and its arguments, then hand it
/// to [`run`].
///
/// - The environment is cleared and rebuilt from `PATH` and `HOME` (inherited), `LC_ALL=C`, the
///   switches that turn the global and system config and the credential prompt off, the fixed
///   identity, and `GIT_INDEX_FILE` when `index` is given. Nothing else of the caller's
///   environment reaches git.
/// - The git directory and the work tree are named on the command line, so nothing is discovered
///   from the current directory. The current directory is the work tree, so paths git prints and
///   pathspecs it reads are relative to its top.
/// - `-c` settings make paths come out unquoted, keep conversions, fsmonitor and the global
///   attributes file off even if the shadow's own config were damaged, silence advice, and pass
///   the user's global excludes file explicitly ([`user_excludes_file`]) -- the one piece of the
///   user's configuration a snapshot is meant to honour.
/// - The child leads a process group of its own, so [`run`] can stop it together with anything it
///   started.
pub fn command(git_dir: &Path, work_tree: &Path, index: Option<&Path>, excludes_file: Option<&Path>) -> Command {
    let mut cmd = isolated(work_tree);
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    cmd.arg(prefixed("--git-dir=", git_dir.as_os_str()))
        .arg(prefixed("--work-tree=", work_tree.as_os_str()))
        .args(["-c", "core.quotepath=false"])
        .args(["-c", "core.autocrlf=false"])
        .args(["-c", "core.fsmonitor=false"])
        .args(["-c", "core.attributesFile=/dev/null"])
        .args(["-c", "advice.addEmbeddedRepo=false"]);
    if let Some(excludes) = excludes_file {
        cmd.arg("-c").arg(prefixed("core.excludesFile=", excludes.as_os_str()));
    }
    cmd
}

/// `git init --bare` of a new shadow git directory at `git_dir`, isolated like [`command`]: an
/// empty template (so no hooks are copied in) and files 0600, directories 0700.
pub fn init_bare(git_dir: &Path) -> Command {
    let mut cmd = isolated(git_dir.parent().unwrap_or(Path::new("/")));
    cmd.args(["init", "--bare", "--template=", "--shared=0600", "-q"])
        .arg(git_dir);
    cmd
}

/// The cleared environment and allowlist shared by [`command`] and [`init_bare`].
fn isolated(current_dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.env_clear();
    for name in ["PATH", "HOME"] {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    cmd.env("LC_ALL", "C")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", IDENTITY_NAME)
        .env("GIT_COMMITTER_NAME", IDENTITY_NAME)
        .env("GIT_AUTHOR_EMAIL", IDENTITY_EMAIL)
        .env("GIT_COMMITTER_EMAIL", IDENTITY_EMAIL);
    cmd.current_dir(current_dir);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.process_group(0);
    cmd
}

/// Variables that make git use a repository other than the one found from its current directory.
/// [`user_read_only`] drops them: a window started from inside a git hook, or from a shell that
/// exported one, would otherwise answer about that repository instead of the project.
pub const REPOSITORY_SELECTION_VARS: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CEILING_DIRECTORIES",
    "GIT_NAMESPACE",
];

/// A git command run in the user's own environment, for the read-only questions only: the global
/// excludes file, and a project's common git directory. `dir` is its current directory, and the
/// repository is found from there alone ([`REPOSITORY_SELECTION_VARS`] are removed); the user's
/// config and every other variable still apply. Nothing built here may add, commit, write a ref
/// or touch an index.
pub fn user_read_only(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    for name in REPOSITORY_SELECTION_VARS {
        cmd.env_remove(name);
    }
    cmd.current_dir(dir);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.process_group(0);
    cmd
}

fn prefixed(prefix: &str, value: &OsStr) -> OsString {
    let mut s = OsString::from(prefix);
    s.push(value);
    s
}

/// Runs `cmd` with no input. See [`run_with_input`].
pub fn run(cmd: Command, timeout: Duration) -> Result<Output, GitError> {
    run_with_input(cmd, None, timeout)
}

/// Runs `cmd`, writing `input` to its standard input when given, and collects its output. A run
/// still going after `timeout` is killed -- the whole process group led by the child's own pid,
/// captured at spawn, while the child is known not to have been reaped -- and reported as
/// [`GitError::TimedOut`]. A zero `timeout` starts nothing. A finished run is `Ok` whatever its exit
/// status; [`GitError::check`] turns a failure into an error.
pub fn run_with_input(mut cmd: Command, input: Option<&[u8]>, timeout: Duration) -> Result<Output, GitError> {
    if timeout.is_zero() {
        return Err(GitError::TimedOut);
    }
    let deadline = Instant::now() + timeout;
    if input.is_some() {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd.spawn().map_err(GitError::Spawn)?;
    let pid = child.id();

    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);
    let writer = match (input, child.stdin.take()) {
        (Some(bytes), Some(mut stdin)) => {
            let bytes = bytes.to_vec();
            Some(std::thread::spawn(move || stdin.write_all(&bytes)))
        }
        _ => None,
    };

    let status = match wait_until(&mut child, deadline) {
        Ok(Some(status)) => status,
        Ok(None) => {
            kill_group(pid);
            let _ = child.wait();
            join_quietly(writer, stdout, stderr);
            return Err(GitError::TimedOut);
        }
        Err(e) => {
            kill_group(pid);
            let _ = child.wait();
            join_quietly(writer, stdout, stderr);
            return Err(GitError::Io(e));
        }
    };

    let write_result = writer.map(|w| w.join());
    let stdout = collect(stdout)?;
    let stderr = collect(stderr)?;
    match write_result {
        // git may close its input early once it has read all it needs and exited successfully;
        // a broken pipe then says nothing about the run.
        Some(Ok(Err(e))) if !(status.success() && e.kind() == std::io::ErrorKind::BrokenPipe) => {
            return Err(GitError::Io(e))
        }
        Some(Err(_)) => return Err(GitError::Io(std::io::Error::other("input writer panicked"))),
        _ => {}
    }
    Ok(Output { status, stdout, stderr })
}

type Drain = std::thread::JoinHandle<std::io::Result<Vec<u8>>>;

fn drain(mut source: impl Read + Send + 'static) -> Drain {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        source.read_to_end(&mut buf).map(|_| buf)
    })
}

fn collect(handle: Option<Drain>) -> Result<Vec<u8>, GitError> {
    match handle.map(|h| h.join()) {
        None => Ok(Vec::new()),
        Some(Ok(Ok(bytes))) => Ok(bytes),
        Some(Ok(Err(e))) => Err(GitError::Io(e)),
        Some(Err(_)) => Err(GitError::Io(std::io::Error::other("output reader panicked"))),
    }
}

fn join_quietly(writer: Option<std::thread::JoinHandle<std::io::Result<()>>>, out: Option<Drain>, err: Option<Drain>) {
    if let Some(w) = writer {
        let _ = w.join();
    }
    let _ = collect(out);
    let _ = collect(err);
}

/// Polls the child until it exits or `deadline` passes (`Ok(None)`). Polling rather than a waiting
/// thread keeps this thread the only one that can reap the child, so the pid a timeout kills is
/// still the child's.
fn wait_until(child: &mut Child, deadline: Instant) -> std::io::Result<Option<ExitStatus>> {
    let mut pause = Duration::from_micros(200);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        std::thread::sleep(pause.min(deadline - now));
        pause = (pause * 2).min(Duration::from_millis(10));
    }
}

fn kill_group(pid: u32) {
    let Ok(pid) = libc::pid_t::try_from(pid) else { return };
    // SAFETY: `kill` takes no pointers. The child led its own process group (`process_group(0)`),
    // so its group id is its pid, and the child has not been reaped yet, so neither id can belong
    // to anything else.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

/// The user's global excludes file, worked out once per process from the user's own environment
/// (see [`resolve_excludes_file`]).
pub fn user_excludes_file() -> Option<PathBuf> {
    static RESOLVED: OnceLock<Option<PathBuf>> = OnceLock::new();
    RESOLVED
        .get_or_init(|| {
            resolve_excludes_file(
                std::env::var_os("HOME").as_deref(),
                std::env::var_os("XDG_CONFIG_HOME").as_deref(),
            )
        })
        .clone()
}

/// Where the user's global excludes file is, the way their own git finds it: `core.excludesFile`
/// from their global config (read-only, `git config --global --type=path --get`, with `HOME` and
/// `XDG_CONFIG_HOME` as given and the rest of the environment inherited), else
/// `$XDG_CONFIG_HOME/git/ignore`, else `$HOME/.config/git/ignore`. `None` when neither variable
/// gives an absolute directory. The file need not exist.
pub fn resolve_excludes_file(home: Option<&OsStr>, xdg_config_home: Option<&OsStr>) -> Option<PathBuf> {
    let mut cmd = user_read_only(Path::new("/"));
    for (name, value) in [("HOME", home), ("XDG_CONFIG_HOME", xdg_config_home)] {
        match value {
            Some(v) => cmd.env(name, v),
            None => cmd.env_remove(name),
        };
    }
    cmd.args(["config", "--global", "--type=path", "--get", "core.excludesFile"]);
    if let Ok(output) = run(cmd, Duration::from_secs(5)) {
        if output.status.success() {
            let value = trim_newline(&output.stdout);
            if !value.is_empty() {
                let path = PathBuf::from(OsStr::from_bytes(value));
                if path.is_absolute() {
                    return Some(path);
                }
            }
        }
    }
    let absolute = |v: Option<&OsStr>| v.map(PathBuf::from).filter(|p| p.is_absolute());
    if let Some(xdg) = absolute(xdg_config_home) {
        return Some(xdg.join("git/ignore"));
    }
    absolute(home).map(|h| h.join(".config/git/ignore"))
}

/// `bytes` without one trailing `\n` (git ends single-value output with one).
pub fn trim_newline(bytes: &[u8]) -> &[u8] {
    bytes.strip_suffix(b"\n").unwrap_or(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_users_git_finds_the_repository_from_its_directory() {
        let cmd = user_read_only(Path::new("/"));
        let removed: Vec<_> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        for name in REPOSITORY_SELECTION_VARS {
            assert!(removed.iter().any(|r| r == name), "{name} reaches the user's git");
        }
    }

    #[test]
    fn a_zero_timeout_starts_nothing() {
        let cmd = user_read_only(Path::new("/"));
        assert!(matches!(run(cmd, Duration::ZERO), Err(GitError::TimedOut)));
    }

    #[test]
    fn the_isolated_environment_is_exactly_the_allowlist() {
        let cmd = command(
            Path::new("/g"),
            Path::new("/w"),
            Some(Path::new("/i")),
            Some(Path::new("/x")),
        );
        let mut names: Vec<_> = cmd.get_envs().map(|(k, _)| k.to_string_lossy().into_owned()).collect();
        names.sort();
        let mut expected: Vec<String> = [
            "GIT_AUTHOR_EMAIL",
            "GIT_AUTHOR_NAME",
            "GIT_COMMITTER_EMAIL",
            "GIT_COMMITTER_NAME",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_NOSYSTEM",
            "GIT_INDEX_FILE",
            "GIT_TERMINAL_PROMPT",
            "LC_ALL",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        for name in ["PATH", "HOME"] {
            if std::env::var_os(name).is_some() {
                expected.push(name.to_string());
            }
        }
        expected.sort();
        assert_eq!(names, expected);
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args[0], "--git-dir=/g");
        assert_eq!(args[1], "--work-tree=/w");
        assert!(args.windows(2).any(|w| w == ["-c", "core.quotepath=false"]));
        assert!(args.windows(2).any(|w| w == ["-c", "core.excludesFile=/x"]));
    }
}
