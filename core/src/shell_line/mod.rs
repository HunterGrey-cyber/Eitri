//! A shell line the person typed into a Claude tab's composer: `!command`.
//!
//! The CLI's own `!` cannot be reached from a host (a line sent to it is plain text to the model), so Eitri
//! runs it, the way the CLI does: the person's `$SHELL` with the aliases and functions of its rc file, no
//! terminal, nothing on stdin, stdout and stderr on one stream, two minutes at most. It answers to no
//! permission mode, rule or card, in the terminal or here: it is the person's own command, and the only way
//! in is the page's `run_shell`, which only the composer's Enter posts.
//!
//! Nothing here runs on the GTK thread: [`start`] spawns and returns, and a worker waits.

pub mod wire;

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a line may run. The CLI's own limit for one.
pub const TIMEOUT: Duration = Duration::from_secs(120);
/// How long a process group has between the polite signal and the other.
const TERM_GRACE: Duration = Duration::from_secs(1);
/// How often the worker looks at the child.
const POLL: Duration = Duration::from_millis(20);
/// How long to go on reading once the shell has gone. A background job left running keeps the pipe open, and
/// the line is over without it.
const DRAIN_GRACE: Duration = Duration::from_millis(200);
/// Bytes held in memory: more than [`wire::INLINE_CHARS`] characters of any text can take.
const MEMORY_BYTES: usize = wire::INLINE_CHARS * 4;
/// Bytes kept on disk. Past it the output is read and dropped, so `yes` cannot fill a disk in its two minutes.
const SPOOL_BYTES: u64 = 64 * 1024 * 1024;

/// From a shell's own environment, the tmux variables and the editor's binary do not reach a line, as they do
/// not reach the bottom terminal's shell.
const REMOVED_ENV: [&str; 3] = ["TMUX", "TMUX_PANE", "NEOVIM_BIN"];

/// What ran and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub command: String,
    /// Where it starts: the tab's last `cd`, or the project.
    pub cwd: PathBuf,
    /// The project: a line may `cd` around in it, and the next one starts back here if it left.
    pub root: PathBuf,
    /// Where a long output goes; `None` is [`spool_dir`], which the worker finds.
    pub spool_dir: Option<PathBuf>,
    pub timeout: Duration,
}

/// How a line ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    Exited(i32),
    /// A signal it did not catch. A shell's own exit status has no word for it.
    Signalled,
    /// The limit passed, and it was killed.
    TimedOut(Duration),
    /// The person stopped it.
    Interrupted,
    /// It could not start: the words why.
    NotStarted,
}

/// What a line left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    pub command: String,
    pub ending: Ending,
    /// Its output as text, whole, unless it went to `spooled`, when this is its first part.
    pub output: String,
    pub spooled: Option<Spooled>,
    /// The directory the next line starts in, when this one moved and stayed in the project.
    pub cwd: Option<PathBuf>,
    /// It ended outside the project.
    pub left_project: bool,
}

/// A long output, on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spooled {
    pub path: PathBuf,
    /// How much it printed, which may be more than the file holds.
    pub bytes: u64,
}

/// A line that is running. Dropping it kills what it started.
pub struct Running {
    control: Arc<Control>,
    done: mpsc::Receiver<Finished>,
}

struct Control {
    /// The process group, until the worker has reaped its leader: past that the number may be another's, so it
    /// is taken under this lock and nothing signals it after.
    group: Mutex<Option<i32>>,
    interrupted: AtomicBool,
}

impl Control {
    fn signal(&self, signal: i32) {
        if let Some(group) = *self.group.lock().unwrap() {
            // SAFETY: a plain syscall on a process group this lock says still has its leader unreaped.
            unsafe {
                libc::kill(-group, signal);
            }
        }
    }
}

impl Running {
    /// Stops it; [`Running::finished`] says so when it has gone. Never waits.
    pub fn interrupt(&self) {
        self.control.interrupted.store(true, Ordering::SeqCst);
        self.control.signal(libc::SIGTERM);
    }

    /// Its end, once it has one.
    pub fn finished(&self) -> Option<Finished> {
        self.done.try_recv().ok()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.control.interrupted.store(true, Ordering::SeqCst);
        self.control.signal(libc::SIGKILL);
    }
}

/// Which shell runs a line: `$SHELL` when the CLI's kinds include it, else the first of them that is there.
/// `None` for a machine with none.
pub fn choose_shell(env_shell: Option<&OsStr>, exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let usable = |path: &Path| matches!(shell_name(path), Some("bash" | "zsh" | "sh")) && exists(path);
    env_shell
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && usable(path))
        .or_else(|| {
            ["/bin/bash", "/usr/bin/bash", "/bin/zsh", "/usr/bin/zsh", "/bin/sh"]
                .iter()
                .map(PathBuf::from)
                .find(|path| usable(path))
        })
}

fn shell_name(path: &Path) -> Option<&str> {
    path.file_name().and_then(|name| name.to_str())
}

/// The script a shell is given. The line and the file it reports its directory to come in as `$1` and `$2`, so
/// no word of them is ever part of this text. The rc file is read for its aliases and functions, quietly and
/// with nothing on stdin, and may not move the line out of its directory.
fn script(shell: &Path) -> String {
    let rc = match shell_name(shell) {
        Some("zsh") => r#"__eitri_rc="${ZDOTDIR:-$HOME}/.zshrc""#,
        Some("bash") => r#"shopt -s expand_aliases; __eitri_rc="$HOME/.bashrc""#,
        _ => "__eitri_rc=",
    };
    format!(
        r#"__eitri_line=$1 __eitri_cwd_file=$2 __eitri_dir=$PWD
{rc}
if [ -n "$__eitri_rc" ] && [ -r "$__eitri_rc" ]; then . "$__eitri_rc" >/dev/null 2>&1 </dev/null; fi
cd -- "$__eitri_dir" 2>/dev/null
eval "$__eitri_line"
__eitri_status=$?
pwd -P >| "$__eitri_cwd_file" 2>/dev/null
exit $__eitri_status
"#
    )
}

/// The environment of a line: the host's, with [`REMOVED_ENV`] gone and the mark programs read to know they
/// run under Claude Code, as they do in the terminal.
fn environment(host: impl IntoIterator<Item = (OsString, OsString)>) -> Vec<(OsString, OsString)> {
    let mut env: Vec<_> = host
        .into_iter()
        .filter(|(key, _)| !REMOVED_ENV.iter().any(|gone| key == gone) && key != "CLAUDECODE")
        .collect();
    env.push(("CLAUDECODE".into(), "1".into()));
    env
}

/// What the reader has gathered so far.
#[derive(Default)]
struct Gathered {
    held: Vec<u8>,
    total: u64,
    spool: Option<(PathBuf, std::fs::File, u64)>,
}

impl Gathered {
    fn take(&mut self, bytes: &[u8], spool_dir: Option<&Path>) {
        self.total += bytes.len() as u64;
        if self.spool.is_none() && self.held.len() + bytes.len() > MEMORY_BYTES {
            self.open_spool(spool_dir);
        }
        if let Some((_, file, written)) = &mut self.spool {
            if *written < SPOOL_BYTES && file.write_all(bytes).is_ok() {
                *written += bytes.len() as u64;
            }
        }
        // The first part stays in memory either way: it is what the row and the preview show.
        let room = MEMORY_BYTES.saturating_sub(self.held.len());
        self.held.extend_from_slice(&bytes[..bytes.len().min(room)]);
    }

    fn open_spool(&mut self, spool_dir: Option<&Path>) {
        let Some(dir) = spool_dir else {
            return;
        };
        let path = dir.join(format!("{}.txt", uuid::Uuid::new_v4()));
        if let Ok(mut file) = agent::private_fs::open_private(&path) {
            if file.write_all(&self.held).is_ok() {
                self.spool = Some((path, file, self.held.len() as u64));
            }
        }
    }
}

fn spawn_reader(pipe: OwnedFd, gathered: Arc<Mutex<Gathered>>, spool_dir: Option<PathBuf>) -> mpsc::Receiver<()> {
    let (ended, read_end) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("shell-line-output".into())
        .spawn(move || {
            let mut pipe = std::fs::File::from(pipe);
            let mut buffer = [0u8; 64 * 1024];
            loop {
                match pipe.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => gathered.lock().unwrap().take(&buffer[..read], spool_dir.as_deref()),
                }
            }
            let _ = ended.send(());
        });
    if spawned.is_err() {
        eprintln!("[shell_line] no thread to read a line's output");
    }
    read_end
}

fn not_started(request: &Request, why: String) -> Finished {
    Finished {
        command: request.command.clone(),
        ending: Ending::NotStarted,
        output: why,
        spooled: None,
        cwd: None,
        left_project: false,
    }
}

/// Starts `request` in a process group of its own, with `shell` or the one [`choose_shell`] finds. `wake` is
/// called, from a worker, once [`Running::finished`] has its answer.
pub fn start(request: Request, shell: Option<PathBuf>, wake: impl Fn() + Send + 'static) -> Running {
    let control = Arc::new(Control {
        group: Mutex::new(None),
        interrupted: AtomicBool::new(false),
    });
    let (tx, done) = mpsc::channel();
    let worker = {
        let control = Arc::clone(&control);
        std::thread::Builder::new().name("shell-line".into()).spawn(move || {
            let finished =
                run_child(&request, shell.as_deref(), &control).unwrap_or_else(|why| not_started(&request, why));
            let _ = tx.send(finished);
            wake();
        })
    };
    if let Err(error) = worker {
        eprintln!("[shell_line] no thread to run a line: {error}");
    }
    Running { control, done }
}

fn run_child(request: &Request, shell: Option<&Path>, control: &Control) -> Result<Finished, String> {
    let chosen = shell.map(Path::to_path_buf).or_else(|| {
        let env_shell = std::env::var_os("SHELL");
        choose_shell(env_shell.as_deref(), Path::exists)
    });
    let shell = chosen.as_deref().ok_or("no bash, zsh or sh to run this with")?;
    let spool = request.spool_dir.clone().or_else(spool_dir);
    let (reader, writer) = std::io::pipe().map_err(|e| format!("no pipe for its output: {e}"))?;
    let writer_two = writer.try_clone().map_err(|e| format!("no pipe for its output: {e}"))?;
    let cwd_file = tempfile_near(spool.as_deref());
    let mut command = Command::new(shell);
    command
        .arg("-c")
        .arg(script(shell))
        .arg(shell)
        .arg(&request.command)
        .arg(cwd_file.as_deref().unwrap_or(Path::new("/dev/null")))
        .current_dir(&request.cwd)
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(writer_two)
        .env_clear()
        .envs(environment(std::env::vars_os()))
        .process_group(0);
    let mut child = command
        .spawn()
        .map_err(|e| format!("{} in {}: {e}", shell.display(), request.cwd.display()))?;
    // Its own end of the pipe is in `command`: the reader sees the end of the output only once that has gone.
    drop(command);
    let group = child.id() as i32;
    *control.group.lock().unwrap() = Some(group);
    let gathered = Arc::new(Mutex::new(Gathered::default()));
    let read_end = spawn_reader(reader.into(), Arc::clone(&gathered), spool.clone());

    let began = Instant::now();
    let mut asked_to_stop: Option<Instant> = None;
    let mut timed_out = false;
    let status = loop {
        let interrupted = control.interrupted.load(Ordering::SeqCst);
        if has_exited(group) {
            // A child that ignored the polite signal outlives the shell. The shell is unreaped here, so the
            // number is still this group's.
            if timed_out || interrupted {
                control.signal(libc::SIGKILL);
            }
            let mut held = control.group.lock().unwrap();
            *held = None;
            break child.wait().ok();
        }
        if !timed_out && !interrupted && began.elapsed() >= request.timeout {
            timed_out = true;
        }
        if timed_out || interrupted {
            // Asked here too: an interrupt may have come before the group was known.
            let since = *asked_to_stop.get_or_insert_with(|| {
                control.signal(libc::SIGTERM);
                Instant::now()
            });
            if since.elapsed() >= TERM_GRACE {
                control.signal(libc::SIGKILL);
            }
        }
        std::thread::sleep(POLL);
    };
    let _ = read_end.recv_timeout(DRAIN_GRACE);
    let ending = if control.interrupted.load(Ordering::SeqCst) {
        Ending::Interrupted
    } else if timed_out {
        Ending::TimedOut(request.timeout)
    } else {
        match status.and_then(|status| status.code()) {
            Some(code) => Ending::Exited(code),
            None => Ending::Signalled,
        }
    };
    let ended_in = cwd_file.as_deref().and_then(read_cwd);
    let root = std::fs::canonicalize(&request.root).unwrap_or_else(|_| request.root.clone());
    let left_project = ended_in.as_ref().is_some_and(|dir| !dir.starts_with(&root));
    let cwd = ended_in.filter(|_| !left_project);
    if let Some(file) = &cwd_file {
        let _ = std::fs::remove_file(file);
    }
    let mut gathered = gathered.lock().unwrap();
    let output = String::from_utf8_lossy(&gathered.held).into_owned();
    if gathered.spool.is_none() && output.chars().count() > wire::INLINE_CHARS {
        gathered.open_spool(spool.as_deref());
    }
    let total = gathered.total;
    let spooled = gathered.spool.take().map(|(path, _, _)| Spooled { path, bytes: total });
    Ok(Finished {
        command: request.command.clone(),
        ending,
        output,
        spooled,
        cwd,
        left_project,
    })
}

/// Whether `pid` has ended, leaving it unreaped.
fn has_exited(pid: i32) -> bool {
    // SAFETY: `waitid` fills `info`, which outlives the call; zeroed is a valid `siginfo_t` for it to read back.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let peeked = libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        );
        peeked != 0 || info.si_pid() != 0
    }
}

/// A private file for the shell to name its last directory in.
fn tempfile_near(dir: Option<&Path>) -> Option<PathBuf> {
    let path = dir?.join(format!("{}.cwd", uuid::Uuid::new_v4()));
    agent::private_fs::write_private(&path, b"").ok()?;
    Some(path)
}

fn read_cwd(path: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(path).ok()?;
    let line = text.strip_suffix('\n')?;
    (!line.is_empty()).then(|| PathBuf::from(line))
}

/// The directory a line's long output goes to, made private, and the outputs a week old gone from it.
/// `None` when there is nowhere to put it: the line still runs, and its output is cut. On a worker.
pub fn spool_dir() -> Option<PathBuf> {
    let root = crate::saved_tabs::state_dir(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )?;
    let dir = root.join("shell-output");
    agent::private_fs::create_private_dir_all(&dir, &root).ok()?;
    prune(&dir, Duration::from_secs(7 * 24 * 3600));
    Some(dir)
}

fn prune(dir: &Path, age: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|time| time.elapsed().is_ok_and(|elapsed| elapsed > age));
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests;
