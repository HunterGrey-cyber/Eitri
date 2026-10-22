//! What the Mac host decides before any window exists: which inherited variables it must not pass on,
//! which project it opens, where Neovide keeps its files, the command line it hands Neovide, and what
//! stands in for a redirected standard input.
//!
//! No `cfg(target_os)` and no AppKit type: every decision here is a function of its arguments, so it is
//! tested on any OS.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use eitri_core::companion::env::SET_ON_THE_NVIM_CHILD;
use eitri_core::project_root::{resolve_in, select_root_source, RootSource};
use eitri_core::split::NEOVIDE_ENV_REMOVED;

/// The prefix of the directory that holds the editor's listening socket.
const NVIM_DIR_PREFIX: &str = "nv-nl-";

/// The editor's socket inside that directory.
const NVIM_SOCKET: &str = "n.sock";

/// The variables this process must not hand on to Neovide, to its nvim, or to anything either starts, among
/// `vars` (the process's environment): only names that are present in it are returned, each once.
///
/// - Everything Neovide's own start reads (`NEOVIDE_*`, byte-wise, so a name that is not UTF-8 counts) and
///   what `eitri split` removes from its Neovide: an outer editor's address, and `TMUX`/`TMUX_PANE`, since
///   Neovide is never inside tmux and an nvim that believes it is leaves `Ctrl+h/j/k/l` to tmux.
/// - What a window of Eitri sets on its own nvim child alone: when present here it came from another
///   window's `:terminal`, and would reach this one's sidecar and agent tools.
/// - `MYVIMRC`, `VIMRUNTIME` and `VIM` only when `NVIM` is set, that is, when an nvim started us. Without
///   `NVIM` they are the user's own exports, which an nvim built from source or Nix needs.
/// - A relative or empty `TMPDIR`: the working directory becomes the project, and every socket and scratch
///   directory would then be made inside it (an empty one makes the temporary directory `""`, the working
///   directory itself). Without the variable the system's own temporary directory is used.
pub fn neovide_vars_to_remove(vars: impl Iterator<Item = (OsString, OsString)>) -> Vec<OsString> {
    let vars: Vec<(OsString, OsString)> = vars.collect();
    let relative_tmpdir = vars
        .iter()
        .any(|(name, value)| name == "TMPDIR" && !Path::new(value).is_absolute());
    let names: Vec<OsString> = vars.into_iter().map(|(name, _)| name).collect();
    let started_by_an_editor = names.iter().any(|name| name == "NVIM");
    let mut removed: Vec<OsString> = Vec::new();
    for name in names {
        let bytes = name.as_bytes();
        let listed = NEOVIDE_ENV_REMOVED
            .iter()
            .chain(&SET_ON_THE_NVIM_CHILD)
            .any(|listed| name == *listed);
        let runtime = started_by_an_editor && ["MYVIMRC", "VIMRUNTIME", "VIM"].iter().any(|n| name == *n);
        let tmpdir = relative_tmpdir && name == "TMPDIR";
        if (listed || bytes.starts_with(b"NEOVIDE_") || runtime || tmpdir) && !removed.contains(&name) {
            removed.push(name);
        }
    }
    removed
}

/// Removes [`neovide_vars_to_remove`]'s names from this process's own environment. Call it first in `main`.
pub fn scrub_environment() {
    for name in neovide_vars_to_remove(std::env::vars_os()) {
        // SAFETY: called first in `main`, before the window system, the panel or any other thread exists,
        // so nothing else can be reading the environment.
        unsafe { std::env::remove_var(name) }
    }
}

/// Which project the window opens.
#[derive(Debug, PartialEq, Eq)]
pub enum ProjectStart {
    /// A project directory, canonical.
    Root(PathBuf),
    /// Nothing named one and the working directory is `/` (the Dock and Finder start an app there): ask.
    Choose,
}

/// The project to open from the command line, `EITRI_PROJECT_DIR` and the working directory, as the Linux
/// window resolves them, with the environment variable and the working directory passed in.
///
/// A launch from the Dock or Finder names no project and runs in `/`, which is never a project: that is
/// `Choose`. `/` named on purpose, by an argument or the variable, is refused.
///
/// This host has no options at all. The Linux window's own (`--clean`, `--version`, `--legacy`), which the
/// shared resolver steps over, are refused here: skipped, `eitri-mac --legacy` would open the working
/// directory on the sidecar with nothing said.
pub fn project_start(args: &[OsString], env_project_dir: Option<OsString>, cwd: &Path) -> Result<ProjectStart, String> {
    // LaunchServices may add a process serial number (`-psn_<n>_<n>`) to a GUI start's arguments; left in,
    // it would be an unrecognised option and the app would not start. Only before `--`: after it every
    // argument is a path, and a directory may be named `-psn_dir`.
    let mut options_ended = false;
    let args: Vec<OsString> = args
        .iter()
        .filter(|arg| {
            options_ended |= arg.as_bytes() == b"--";
            options_ended || !arg.as_bytes().starts_with(b"-psn_")
        })
        .cloned()
        .collect();
    let mut options = args.iter().take_while(|arg| arg.as_bytes() != b"--");
    if let Some(option) = options.find(|arg| arg.as_bytes().first() == Some(&b'-')) {
        return Err(format!(
            "eitri-mac takes no options, and {option:?} is one; if it is a project directory, pass it after \
             `--` (eitri-mac -- {option:?})"
        ));
    }
    let source = select_root_source(args.iter().map(OsString::as_os_str), env_project_dir.as_deref())?;
    let root = resolve_in(&args, env_project_dir, Ok(cwd.to_path_buf()))?;
    if root == Path::new("/") {
        return match source {
            RootSource::Cwd => Ok(ProjectStart::Choose),
            _ => Err(String::from(
                "/ is never a project directory: name the directory to open (an argument or EITRI_PROJECT_DIR)",
            )),
        };
    }
    Ok(ProjectStart::Root(root))
}

/// Where Neovide keeps what it would keep for itself, and the editor's socket.
#[derive(Debug, PartialEq, Eq)]
pub struct Paths {
    /// Neovide's data directory (its window state), under Eitri's state home and never Neovide's own.
    pub neovide_data: PathBuf,
    /// The directory holding `init.lua` and `neovide.toml`.
    pub config_dir: PathBuf,
    /// Neovide's settings file, under Eitri's configuration directory and never `~/.config/neovide`.
    pub neovide_config: PathBuf,
    /// The socket the editor's nvim listens on, in a fresh private directory.
    pub nvim_listen: PathBuf,
}

/// Resolves [`Paths`] from `env` (the process's environment, passed in), the user's `home` and the directory
/// the runtime files go in (`run_dir`, the per-user temporary directory). Creates the socket's directory.
///
/// A `home` that is not absolute is refused. A relative `XDG_STATE_HOME` counts as unset, as it does for the
/// rest of Eitri's state. A relative `EITRI_CONFIG_DIR` is refused: the project becomes the working
/// directory, and a relative path would then read Neovide's settings (which can name the nvim to run) from
/// the project. An empty one counts as unset. Linux reads `$XDG_CONFIG_HOME/eitri` when that variable
/// is set and absolute; the Mac always uses `~/.config/eitri` (there is no XDG convention there).
/// Neovide's file must sit beside `init.lua`.
pub fn paths(env: &dyn Fn(&str) -> Option<OsString>, home: &Path, run_dir: &Path) -> Result<Paths, String> {
    if !home.is_absolute() {
        return Err(format!("the home directory {home:?} is not an absolute path"));
    }
    let neovide_data =
        eitri_core::layout::persist::state_subdir(env("XDG_STATE_HOME").as_deref(), Some(home.as_os_str()), "neovide")
            .ok_or_else(|| "no state directory could be derived".to_string())?;
    let config_dir = match env("EITRI_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) if Path::new(&dir).is_absolute() => PathBuf::from(dir),
        Some(dir) => return Err(format!("EITRI_CONFIG_DIR={dir:?} is not an absolute path")),
        None => home.join(".config/eitri"),
    };
    let neovide_config = config_dir.join("neovide.toml");
    let dir = eitri_core::instance_dir::create_instance_dir(run_dir, NVIM_DIR_PREFIX, NVIM_SOCKET, "eitri-mac")
        .map_err(|e| format!("cannot create the editor's socket directory under {run_dir:?}: {e}"))?;
    let nvim_listen = match agent::socket_path::in_dir(&dir, NVIM_SOCKET) {
        Ok(path) if path.to_str().is_some() => path,
        Ok(path) => {
            let _ = std::fs::remove_dir(&dir);
            return Err(format!("the editor's socket path {path:?} is not valid UTF-8"));
        }
        Err(e) => {
            let _ = std::fs::remove_dir(&dir);
            return Err(e.to_string());
        }
    };
    Ok(Paths {
        neovide_data,
        config_dir,
        neovide_config,
        nvim_listen,
    })
}

/// The command line handed to Neovide: its own name first (the embedder's arguments carry argv[0]), no
/// fork, and the socket nvim listens on after `--`, so it reaches nvim and not Neovide.
pub fn neovide_args(nvim_listen: &Path) -> Vec<String> {
    let socket = nvim_listen
        .to_str()
        .expect("`paths` refuses a socket path that is not UTF-8")
        .to_string();
    ["eitri", "--no-fork", "--", "--listen"]
        .into_iter()
        .map(String::from)
        .chain([socket])
        .collect()
}

/// Where a descriptor's output goes, as far as a start failure's message is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputGoes {
    /// A terminal: someone sees the message.
    Terminal,
    /// `/dev/null`: nobody does. A start by LaunchServices (the Dock, Finder, `open`) gets this.
    Nowhere,
    /// A file, a pipe or a socket: something keeps the message (a log, a harness).
    Kept,
    /// The descriptor is closed.
    Closed,
}

/// Where `fd`'s output goes, from `fstat` on the descriptor itself and `/dev/null`'s device number.
pub fn output_goes(fd: RawFd) -> io::Result<OutputGoes> {
    // SAFETY: `isatty` only reads the descriptor's type.
    if unsafe { libc::isatty(fd) } == 1 {
        return Ok(OutputGoes::Terminal);
    }
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: as in `is_char_device`: any integer is safe to pass, a closed one reports `EBADF`.
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } == -1 {
        let e = io::Error::last_os_error();
        return if e.raw_os_error() == Some(libc::EBADF) {
            Ok(OutputGoes::Closed)
        } else {
            Err(e)
        };
    }
    // SAFETY: `fstat` returned 0, so it filled in the whole structure.
    let stat = unsafe { stat.assume_init() };
    let null = std::fs::metadata("/dev/null")?;
    use std::os::unix::fs::MetadataExt;
    let is_null = stat.st_mode & libc::S_IFMT == libc::S_IFCHR && stat.st_rdev as u64 == null.rdev();
    Ok(if is_null { OutputGoes::Nowhere } else { OutputGoes::Kept })
}

/// Whether a start that failed must also say so in an alert: only when LaunchServices started the app (its
/// parent is launchd, pid 1) and the message on stderr would go nowhere. A terminal shows the message, and a
/// file or pipe keeps it for whoever redirected it; an alert there would leave a harness waiting on a modal
/// dialog instead of a failed exit.
pub fn start_failure_needs_an_alert(parent_pid: i32, stderr: OutputGoes) -> bool {
    parent_pid == 1 && matches!(stderr, OutputGoes::Nowhere | OutputGoes::Closed)
}

/// Makes file descriptor 0 a character device (`/dev/null`) when it is a pipe, a socket, a file or closed:
/// nvim reads an inherited standard input, and a pipe nobody writes to stalls its start. A terminal and
/// `/dev/null` are left alone.
pub fn detach_stdin_if_redirected() -> io::Result<()> {
    detach_if_redirected(0)
}

/// Whether `fd` is a character device, asked of the descriptor itself with `fstat`: no `File` is built
/// from a number that may not be open. `Ok(None)` is a closed descriptor (`EBADF`).
fn is_char_device(fd: RawFd) -> io::Result<Option<bool>> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: `stat` is a valid, writable `struct stat`; `fstat` only reads `fd` as a number and reports
    // `EBADF` for one that is not open, so any integer is safe to pass.
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } == -1 {
        let e = io::Error::last_os_error();
        return if e.raw_os_error() == Some(libc::EBADF) {
            Ok(None)
        } else {
            Err(e)
        };
    }
    // SAFETY: `fstat` returned 0, so it filled in the whole structure.
    let mode = unsafe { stat.assume_init() }.st_mode;
    Ok(Some(mode & libc::S_IFMT == libc::S_IFCHR))
}

fn detach_if_redirected(fd: RawFd) -> io::Result<()> {
    match is_char_device(fd)? {
        Some(true) => return Ok(()),
        // A pipe, a socket or a file: replaced below.
        Some(false) => {}
        // A closed descriptor: the next `open` would take its number and become some child's input.
        None => {}
    }
    let null = File::open("/dev/null")?;
    if null.as_raw_fd() == fd {
        // The descriptor was closed, so `open` took its number; it is already the right file, but std opened
        // it close-on-exec, which would close it again at the first child.
        // SAFETY: `fd` is the descriptor `null` owns, which stays open (`into_raw_fd` below gives it up).
        if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } == -1 {
            return Err(io::Error::last_os_error());
        }
        let _ = null.into_raw_fd();
        return Ok(());
    }
    // SAFETY: both descriptors are open; `dup2` closes `fd`'s old file and leaves the copy not close-on-exec.
    if unsafe { libc::dup2(null.as_raw_fd(), fd) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Whether output on a descriptor goes to the app's log instead: a pipe or a socket (its reader may go away, and
/// every write after that fails), `/dev/null` (a start by LaunchServices: nobody would ever see a line) or a
/// closed descriptor. A terminal and a file someone redirected to are left alone.
pub fn output_moves_to_log(goes: OutputGoes, pipe: bool) -> bool {
    pipe || matches!(goes, OutputGoes::Nowhere | OutputGoes::Closed)
}

/// `<state home>/eitri/mac`, where the app's log is kept, by the rule every state directory follows.
pub fn log_dir(xdg_state_home: Option<&std::ffi::OsStr>, home: &Path) -> Option<PathBuf> {
    eitri_core::layout::persist::state_subdir(xdg_state_home, Some(home.as_os_str()), "mac")
}

/// What [`move_output_to_log`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct OutputMoved {
    pub stdout: bool,
    pub stderr: bool,
    /// The log both now write to; `None` when nothing moved or the file could not be made (`/dev/null` then).
    pub log: Option<PathBuf>,
    /// The log was opened but this process could not take its shared lock in time, so a later start that finds
    /// itself alone may empty it while this one still writes.
    pub unlocked: bool,
}

/// An open project log ([`open_project_log`]).
#[derive(Debug)]
pub struct ProjectLog {
    /// Opened to append; what stdout and stderr become, and what children inherit.
    pub file: File,
    /// A second open of the same file, close-on-exec, that holds the shared lock. The lock belongs to an open
    /// file description, so holding it on the description stdout and stderr share would hand it to every child
    /// that inherits them, and a child that outlived the app (an orphaned `claude`) would keep every later start
    /// from ever being alone: the log would grow without end. On its own description, the lock goes when this
    /// process does. Kept open for the process's life (dropping it releases the lock).
    pub lock: File,
    pub path: PathBuf,
    /// Whether `lock` holds the shared lock ([`hold_shared`]).
    pub locked: bool,
}

/// How many times, and how far apart, [`open_project_log`] asks for the shared lock before writing without it.
const SHARED_LOCK_TRIES: u32 = 200;
const SHARED_LOCK_PAUSE: std::time::Duration = std::time::Duration::from_millis(10);

/// Takes the shared `flock` on `fd`, asking up to `tries` times `pause` apart while another start holds the
/// exclusive lock. On macOS turning an exclusive lock into a shared one is not atomic (flock(2): the old lock is
/// released, then the new one asked for), so a start that had the exclusive lock can lose the race to another
/// start's exclusive lock in between; that one holds it only while it empties the file, so waiting a moment
/// gets the shared lock. False once the tries run out or on any other error.
pub fn hold_shared(fd: RawFd, tries: u32, pause: std::time::Duration) -> bool {
    for n in 0..tries {
        // SAFETY: `flock` on a descriptor the caller keeps open for the call.
        if unsafe { libc::flock(fd, libc::LOCK_SH | libc::LOCK_NB) } == 0 {
            return true;
        }
        if io::Error::last_os_error().raw_os_error() != Some(libc::EWOULDBLOCK) {
            return false;
        }
        if n + 1 < tries {
            std::thread::sleep(pause);
        }
    }
    false
}

/// The project's log, `<dir>/<16 hex of the project>.log` (0600 in a 0700 directory), opened to append and
/// shared by every `eitri-mac` of the project.
///
/// Two windows of one project can run at once (nothing stops a second start), and each used to truncate the file
/// and write from its own offset, so one overwrote the other's lines. Now every write goes to the end
/// (`O_APPEND`), and the file is emptied at a start only when no other `eitri-mac` holds it: each process keeps a
/// shared `flock` on it for its life, on a description of its own that no child inherits ([`ProjectLog::lock`]),
/// and a start that gets the exclusive lock first is alone and truncates. A child left running by an earlier app
/// still appends to the file it inherited; a lone start empties it under that child, which then writes at the
/// new end. One file per project rather than one per process, so the line that names the log names the file a
/// user looks for, and logs do not pile up with nothing to remove them.
///
/// As `agent::private_fs::open_private`: a link in its place is refused, and what was opened must be a regular
/// file of this user (checked on the handle, opened without blocking) before anything is written or cut; the
/// lock's open must be that same file.
pub fn open_project_log(dir: &Path, project_root: &Path) -> Option<ProjectLog> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    eitri_core::layout::persist::create_state_dir(dir).ok()?;
    let path = dir.join(eitri_core::companion::stdio::log_file_name(project_root));
    let file = File::options()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&path)
        .ok()?;
    let meta = file.metadata().ok()?;
    // SAFETY: `geteuid` takes nothing and cannot fail.
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } {
        return None;
    }
    // The mode is put right before the lock's read-only open, which a log left at 0200 (or made under a umask
    // such as 0477) would otherwise fail, and every start would write to `/dev/null` instead.
    // SAFETY: `fchmod` on the descriptor `file` owns, open for the length of the call.
    if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } == -1 {
        return None;
    }
    let lock = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&path)
        .ok()?;
    let same = lock.metadata().ok()?;
    if (same.dev(), same.ino()) != (meta.dev(), meta.ino()) {
        return None;
    }
    // SAFETY: `flock` on the descriptor `lock` owns, open for the length of the call.
    unsafe {
        if libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 {
            // Alone: last time's lines go.
            file.set_len(0).ok()?;
        }
    }
    // Held for the process's life. Taking it fails only while another start holds the exclusive lock for the
    // moment its truncation takes (or, after the truncation above, in the gap macOS leaves when it turns the
    // exclusive lock into a shared one), so it is asked for again for a while.
    let locked = hold_shared(lock.as_raw_fd(), SHARED_LOCK_TRIES, SHARED_LOCK_PAUSE);
    Some(ProjectLog {
        file,
        lock,
        path,
        locked,
    })
}

/// Moves `out` and `err` (stdout and stderr; descriptors passed in so a test can use its own) to the project's
/// log when [`output_moves_to_log`] says so ([`open_project_log`]). When it cannot be made, `/dev/null`: a write
/// there never fails. `before` is called once, with the log's path, just before the first
/// descriptor moves, while the old stream still reaches whoever reads it. The log's lock is kept open for the
/// process's life.
pub fn move_output_to_log(
    out: RawFd,
    err: RawFd,
    log_dir: Option<&Path>,
    project_root: &Path,
    before: &dyn Fn(Option<&Path>),
) -> OutputMoved {
    use eitri_core::companion::stdio::is_pipe;
    let moves = |fd: RawFd| output_goes(fd).is_ok_and(|goes| output_moves_to_log(goes, is_pipe(fd)));
    let (move_out, move_err) = (moves(out), moves(err));
    let mut moved = OutputMoved::default();
    if !move_out && !move_err {
        return moved;
    }
    let (file, path) = match log_dir.and_then(|dir| open_project_log(dir, project_root)) {
        Some(log) => {
            moved.unlocked = !log.locked;
            // Held until the process ends; close-on-exec, so no child keeps it.
            let _ = log.lock.into_raw_fd();
            (log.file, Some(log.path))
        }
        None => match File::options().write(true).open("/dev/null") {
            Ok(null) => (null, None),
            Err(_) => return moved,
        },
    };
    before(path.as_deref());
    let point = |fd: RawFd| {
        // SAFETY: `file` is open; `dup2` replaces `fd` (open or not) atomically, and the copy is not
        // close-on-exec, so the editor and the sidecar write to the log too.
        unsafe { libc::dup2(file.as_raw_fd(), fd) != -1 }
    };
    moved.stdout = move_out && point(out);
    moved.stderr = move_err && point(err);
    moved.log = path.filter(|_| moved.stdout || moved.stderr);
    moved.unlocked &= moved.log.is_some();
    moved
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::mem::ManuallyDrop;
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    /// A stand-in for the run directory, under `/tmp`. The app makes its socket directory straight inside
    /// `TMPDIR`, which fits the socket cap with bytes to spare; a temporary directory under macOS's own
    /// `TMPDIR` (`/var/folders/../T/`) adds a level of its own, and the socket path below it no longer fits, so
    /// the tests would fail on the cap instead of testing the paths.
    fn short_run_dir() -> tempfile::TempDir {
        tempfile::Builder::new().tempdir_in("/tmp").unwrap()
    }

    fn vars<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Iterator<Item = (OsString, OsString)> + 'a {
        pairs.iter().map(|(k, v)| (OsString::from(k), OsString::from(v)))
    }

    #[test]
    fn every_neovide_variable_and_every_editor_address_is_removed() {
        let all = [
            ("NEOVIDE_SERVER", "x"),
            ("NEOVIDE_FRAME", "none"),
            ("TMUX", "/tmp/t,1,0"),
            ("TMUX_PANE", "%1"),
            ("NVIM", "/tmp/n"),
            ("NVIM_LISTEN_ADDRESS", "/tmp/n"),
            ("EITRI_PANE_SWITCH_SOCKET", "/tmp/p"),
            ("EITRI_THEME_LUA", "/tmp/t.lua"),
            ("VIMRUNTIME", "/opt/nvim/share"),
            ("HOME", "/Users/u"),
            ("NEOVIDEX", "kept"),
        ];
        let removed = neovide_vars_to_remove(vars(&all));
        for name in [
            "NEOVIDE_SERVER",
            "NEOVIDE_FRAME",
            "TMUX",
            "TMUX_PANE",
            "NVIM",
            "NVIM_LISTEN_ADDRESS",
            "EITRI_PANE_SWITCH_SOCKET",
            "EITRI_THEME_LUA",
            "VIMRUNTIME",
        ] {
            assert!(removed.contains(&OsString::from(name)), "{name}");
        }
        assert!(!removed.contains(&OsString::from("HOME")));
        assert!(!removed.contains(&OsString::from("NEOVIDEX"))); // only the NEOVIDE_ prefix
    }

    #[test]
    fn every_variable_a_window_sets_on_its_own_nvim_is_removed() {
        let all: Vec<(&str, &str)> = SET_ON_THE_NVIM_CHILD.iter().map(|name| (*name, "/x")).collect();
        let removed = neovide_vars_to_remove(vars(&all));
        assert_eq!(removed.len(), SET_ON_THE_NVIM_CHILD.len());
    }

    #[test]
    fn the_users_own_runtime_exports_stay_when_no_editor_started_us() {
        let all = [
            ("VIMRUNTIME", "/opt/nvim/share"),
            ("VIM", "/opt/nvim"),
            ("MYVIMRC", "/u/init.lua"),
        ];
        let removed = neovide_vars_to_remove(vars(&all));
        assert!(removed.is_empty(), "{removed:?}");
    }

    #[test]
    fn the_runtime_exports_go_when_an_editor_started_us_and_each_name_is_listed_once() {
        let all = [
            ("NVIM", "/n"),
            ("VIM", "/v"),
            ("MYVIMRC", "/m"),
            ("VIMRUNTIME", "/r"),
            ("NVIM", "/n"),
        ];
        let removed = neovide_vars_to_remove(vars(&all));
        assert_eq!(removed.len(), 4, "{removed:?}");
    }

    #[test]
    fn vars_named_neovide_are_removed_even_when_not_utf8() {
        let name = OsString::from_vec(b"NEOVIDE_\xff".to_vec());
        let removed = neovide_vars_to_remove([(name.clone(), OsString::from("x"))].into_iter());
        assert_eq!(removed, [name]);
    }

    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn a_launchservices_start_with_no_project_asks_for_one_and_root_is_never_a_project() {
        assert_eq!(project_start(&[], None, Path::new("/")), Ok(ProjectStart::Choose));
    }

    #[test]
    fn an_argument_or_the_variable_is_resolved_as_on_linux() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let arg = [dir.path().as_os_str().to_owned()];
        assert_eq!(
            project_start(&arg, None, Path::new("/")),
            Ok(ProjectStart::Root(canonical))
        );
    }

    #[test]
    fn the_variable_resolves_when_no_argument_is_given() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        assert_eq!(
            project_start(&[], Some(dir.path().as_os_str().to_owned()), Path::new("/")),
            Ok(ProjectStart::Root(canonical))
        );
    }

    #[test]
    fn the_working_directory_is_the_project_when_it_is_not_root() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            project_start(&[], None, dir.path()),
            Ok(ProjectStart::Root(dir.path().canonicalize().unwrap()))
        );
    }

    #[test]
    fn root_given_explicitly_is_refused() {
        assert!(project_start(&[OsString::from("/")], None, Path::new("/tmp")).is_err());
        assert!(project_start(&[], Some(OsString::from("/")), Path::new("/tmp")).is_err());
    }

    #[test]
    fn a_process_serial_number_argument_is_ignored() {
        assert_eq!(
            project_start(&[OsString::from("-psn_0_12345")], None, Path::new("/")),
            Ok(ProjectStart::Choose)
        );
    }

    #[test]
    fn a_directory_named_like_a_serial_number_opens_after_the_separator() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("-psn_project");
        std::fs::create_dir(&project).unwrap();
        let args = [OsString::from("--"), project.as_os_str().to_owned()];
        assert_eq!(
            project_start(&args, None, Path::new("/")),
            Ok(ProjectStart::Root(project.canonicalize().unwrap()))
        );
        // Before the separator it is still LaunchServices' serial number and is dropped.
        let before = [
            OsString::from("-psn_0_1"),
            OsString::from("--"),
            project.as_os_str().to_owned(),
        ];
        assert_eq!(
            project_start(&before, None, Path::new("/")),
            Ok(ProjectStart::Root(project.canonicalize().unwrap()))
        );
    }

    #[test]
    fn an_unknown_option_is_still_refused() {
        assert!(project_start(&[OsString::from("--bogus")], None, Path::new("/tmp")).is_err());
    }

    #[test]
    fn the_linux_windows_options_are_refused_not_skipped() {
        let dir = tempfile::tempdir().unwrap();
        for flag in ["--legacy", "--version", "--clean"] {
            let refused = project_start(&[OsString::from(flag)], None, dir.path());
            assert!(
                refused.as_ref().is_err_and(|why| why.contains(flag)),
                "{flag}: {refused:?}"
            );
            // Before a project directory too, and from `/`, where skipping it would have asked for one.
            let before = [OsString::from(flag), dir.path().as_os_str().to_owned()];
            assert!(project_start(&before, None, Path::new("/")).is_err(), "{flag}");
        }
        // After `--` it is a project directory, as on Linux.
        assert_eq!(
            project_start(
                &[OsString::from("--"), dir.path().as_os_str().to_owned()],
                None,
                Path::new("/")
            ),
            Ok(ProjectStart::Root(dir.path().canonicalize().unwrap()))
        );
    }

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |key| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| OsString::from(v))
    }

    #[test]
    fn neovide_keeps_its_state_and_config_under_eitri_never_its_own() {
        let run = short_run_dir();
        let env = env_of(&[("XDG_STATE_HOME", "/s")]);
        let p = paths(&env, Path::new("/Users/u"), run.path()).unwrap();
        assert_eq!(p.neovide_data, PathBuf::from("/s/eitri/neovide"));
        assert_eq!(p.config_dir, PathBuf::from("/Users/u/.config/eitri"));
        assert_eq!(p.neovide_config, PathBuf::from("/Users/u/.config/eitri/neovide.toml"));
        assert!(p.nvim_listen.as_os_str().len() <= 103); // agent::socket_path's cap
        let dir = p.nvim_listen.parent().unwrap();
        assert_eq!(dir.parent(), Some(run.path()));
        let meta = std::fs::symlink_metadata(dir).unwrap();
        assert!(meta.is_dir());
        assert_eq!(meta.mode() & 0o777, 0o700);
    }

    #[test]
    fn a_relative_config_dir_is_refused_and_an_empty_one_is_unset() {
        let run = short_run_dir();
        let home = Path::new("/Users/u");
        assert!(paths(&env_of(&[("EITRI_CONFIG_DIR", "rel")]), home, run.path()).is_err());
        let p = paths(&env_of(&[("EITRI_CONFIG_DIR", "")]), home, run.path()).unwrap();
        assert_eq!(p.config_dir, PathBuf::from("/Users/u/.config/eitri"));
        let p = paths(&env_of(&[("EITRI_CONFIG_DIR", "/c")]), home, run.path()).unwrap();
        assert_eq!(p.neovide_config, PathBuf::from("/c/neovide.toml"));
    }

    #[test]
    fn a_relative_state_home_falls_back_to_home() {
        let run = short_run_dir();
        let p = paths(&env_of(&[("XDG_STATE_HOME", "rel")]), Path::new("/Users/u"), run.path()).unwrap();
        assert_eq!(p.neovide_data, PathBuf::from("/Users/u/.local/state/eitri/neovide"));
    }

    #[test]
    fn a_relative_home_is_refused() {
        let run = short_run_dir();
        assert!(paths(&env_of(&[]), Path::new("u"), run.path()).is_err());
    }

    /// The socket path at macOS's own worst case, asserted exactly rather than against the limit: an
    /// inequality lets the margin be eaten in silence, and spending a byte should cost a red test.
    #[test]
    fn the_nvim_socket_is_exactly_100_bytes_at_the_macos_worst_case() {
        // `/var/folders/<2>/<28>/T/`, the shape of every macOS user's temporary directory.
        let macos_tmp = Path::new("/var/folders/33/0tqfpnyn4z3c049gljzppdv00000gn/T/");
        assert_eq!(macos_tmp.as_os_str().len(), 49);
        // Five digits is the longest pid there.
        let dir = macos_tmp.join(format!("{NVIM_DIR_PREFIX}99999-0123456789abcdef0123456789abcdef"));
        let path = agent::socket_path::in_dir(&dir, NVIM_SOCKET).expect("must fit");
        assert_eq!(path.as_os_str().len(), 100, "{path:?}");
    }

    #[test]
    fn neovide_gets_a_command_line_of_ours() {
        let sock = Path::new("/r/n.sock");
        assert_eq!(
            neovide_args(sock),
            ["eitri", "--no-fork", "--", "--listen", sock.to_str().unwrap()]
        );
    }

    #[test]
    fn a_relative_or_empty_tmpdir_is_removed_and_an_absolute_one_kept() {
        assert_eq!(neovide_vars_to_remove(vars(&[("TMPDIR", "tmp")])), ["TMPDIR"]);
        assert_eq!(neovide_vars_to_remove(vars(&[("TMPDIR", "./x/")])), ["TMPDIR"]);
        assert!(neovide_vars_to_remove(vars(&[("TMPDIR", "/var/folders/x/T/")])).is_empty());
        assert_eq!(
            neovide_vars_to_remove(vars(&[("TMPDIR", "")])),
            ["TMPDIR"],
            "an empty one too"
        );
    }

    #[test]
    fn a_failed_start_alerts_only_when_launchservices_started_it_and_stderr_goes_nowhere() {
        for stderr in [OutputGoes::Nowhere, OutputGoes::Closed] {
            assert!(start_failure_needs_an_alert(1, stderr), "{stderr:?}");
            assert!(
                !start_failure_needs_an_alert(4242, stderr),
                "a shell or a harness started it: {stderr:?}"
            );
        }
        for stderr in [OutputGoes::Terminal, OutputGoes::Kept] {
            assert!(!start_failure_needs_an_alert(1, stderr), "{stderr:?}");
        }
    }

    #[test]
    fn where_output_goes_is_read_from_the_descriptor() {
        use std::os::fd::AsRawFd;
        let null = File::options().write(true).open("/dev/null").unwrap();
        assert_eq!(output_goes(null.as_raw_fd()).unwrap(), OutputGoes::Nowhere);
        let dir = tempfile::tempdir().unwrap();
        let log = File::create(dir.path().join("log")).unwrap();
        assert_eq!(
            output_goes(log.as_raw_fd()).unwrap(),
            OutputGoes::Kept,
            "a redirect to a log file"
        );
        let (read, write) = std::os::unix::net::UnixStream::pair().unwrap();
        assert_eq!(output_goes(write.as_raw_fd()).unwrap(), OutputGoes::Kept);
        drop(read);
        // A high number no test opens (a just-closed one could be reused by a test running beside this one).
        let closed: RawFd = 987;
        // SAFETY: `fcntl(F_GETFD)` only reads the descriptor's flags; any integer is safe.
        if unsafe { libc::fcntl(closed, libc::F_GETFD) } == -1 {
            assert_eq!(output_goes(closed).unwrap(), OutputGoes::Closed);
        }
    }

    #[test]
    fn a_neovide_configuration_of_the_users_is_never_the_one_read() {
        // Neovide reads `NEOVIDE_CONFIG`, then `$XDG_CONFIG_HOME/neovide/config.toml` or `~/.config/neovide`,
        // unless the embedder names its file. The file named here never depends on those: the variable is
        // removed at start, and neither `XDG_CONFIG_HOME` nor Neovide's own directory is read.
        let run = short_run_dir();
        let env = env_of(&[
            ("XDG_CONFIG_HOME", "/evil/config"),
            ("NEOVIDE_CONFIG", "/evil/neovide.toml"),
        ]);
        let p = paths(&env, Path::new("/Users/u"), run.path()).unwrap();
        assert_eq!(p.neovide_config, PathBuf::from("/Users/u/.config/eitri/neovide.toml"));
        for path in [&p.neovide_config, &p.neovide_data, &p.config_dir] {
            assert!(!path.starts_with("/evil"), "{path:?}");
            assert!(!path.to_string_lossy().contains("/neovide/"), "{path:?}");
        }
        let removed = neovide_vars_to_remove(vars(&[("NEOVIDE_CONFIG", "/evil/neovide.toml"), ("NEOVIDE_FORK", "1")]));
        assert_eq!(removed, ["NEOVIDE_CONFIG", "NEOVIDE_FORK"]);
        // A `fork = true` in whatever file is read cannot detach the app: Neovide's command line says
        // `--no-fork`, and its embedded start never disowns.
        assert!(neovide_args(&p.nvim_listen).contains(&"--no-fork".to_string()));
    }

    fn is_dev_null_like(fd: RawFd) -> bool {
        // SAFETY: every caller passes a descriptor that is open (the tests make sure of it), and the
        // `ManuallyDrop` keeps this handle from closing it: it only borrows the number to `fstat` and read.
        let borrowed = ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
        let char_device = borrowed.metadata().unwrap().file_type().is_char_device();
        let mut buf = [0u8; 8];
        let mut reader: &File = &borrowed;
        char_device && reader.read(&mut buf).unwrap() == 0
    }

    #[test]
    fn a_pipe_and_a_regular_file_on_the_fd_are_replaced_by_dev_null() {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `fds` is two writable `c_int`s, which is all `pipe` writes.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (read_end, write_end) = (fds[0], fds[1]);
        detach_if_redirected(read_end).unwrap();
        assert!(is_dev_null_like(read_end));
        // SAFETY: both ends were opened above and nothing else owns or closes them.
        unsafe {
            libc::close(read_end);
            libc::close(write_end);
        }

        let dir = tempfile::tempdir().unwrap();
        let file = File::create(dir.path().join("f")).unwrap();
        detach_if_redirected(file.as_raw_fd()).unwrap();
        assert!(is_dev_null_like(file.as_raw_fd()));
    }

    #[test]
    fn a_closed_descriptor_is_replaced_by_dev_null() {
        // A high number no other thread of the test process is likely to take.
        let fd: RawFd = 777;
        let null = File::open("/dev/null").unwrap();
        // SAFETY: `null` is open; `dup2` makes `fd` another descriptor for it.
        assert_ne!(unsafe { libc::dup2(null.as_raw_fd(), fd) }, -1);
        // SAFETY: `fd` was opened by the `dup2` above and is closed exactly once here.
        assert_eq!(unsafe { libc::close(fd) }, 0);
        detach_if_redirected(fd).unwrap();
        assert!(is_dev_null_like(fd));
        // SAFETY: `detach_if_redirected` opened `fd` again; this closes that descriptor once.
        unsafe { libc::close(fd) };
    }

    #[test]
    fn a_character_device_is_left_alone() {
        let null = File::open("/dev/null").unwrap();
        let before = null.metadata().unwrap().ino();
        detach_if_redirected(null.as_raw_fd()).unwrap();
        assert_eq!(null.metadata().unwrap().ino(), before);
        assert!(is_dev_null_like(null.as_raw_fd()));
    }

    #[test]
    fn output_moves_off_pipes_and_nowhere_and_stays_on_a_terminal_or_a_file() {
        assert!(output_moves_to_log(OutputGoes::Kept, true), "a pipe");
        assert!(
            output_moves_to_log(OutputGoes::Nowhere, false),
            "LaunchServices' /dev/null"
        );
        assert!(output_moves_to_log(OutputGoes::Closed, false));
        assert!(!output_moves_to_log(OutputGoes::Terminal, false));
        assert!(
            !output_moves_to_log(OutputGoes::Kept, false),
            "a file someone redirected to"
        );
    }

    /// `eitri-mac proj | head` once `head` has exited: the write end's reader is gone. After the move the
    /// descriptors write to the log, and a write succeeds.
    #[test]
    fn a_dead_pipe_is_moved_to_the_log_and_writes_succeed() {
        use std::io::Write;
        use std::os::fd::{FromRawFd, OwnedFd};
        let (read, write) = std::io::pipe().unwrap();
        drop(read);
        let write = OwnedFd::from(write);
        // SAFETY: `dup` of an open descriptor; the copy is owned below.
        let write2 = unsafe { OwnedFd::from_raw_fd(libc::dup(write.as_raw_fd())) };
        let mut before_dead = File::from(write.try_clone().unwrap());
        assert!(before_dead.write_all(b"x").is_err(), "the pipe is dead");

        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("state/eitri/mac");
        let named = std::cell::RefCell::new(None);
        let moved = move_output_to_log(
            write.as_raw_fd(),
            write2.as_raw_fd(),
            Some(&logs),
            Path::new("/a/project"),
            &|log| *named.borrow_mut() = Some(log.map(Path::to_path_buf)),
        );
        let log = moved.log.clone().expect("a log file");
        assert!(moved.stdout && moved.stderr, "{moved:?}");
        assert!(
            log.starts_with(&logs) && log.extension().is_some_and(|e| e == "log"),
            "{log:?}"
        );
        assert_eq!(*named.borrow(), Some(Some(log.clone())), "told before the move");

        let mut out = File::from(write);
        let mut err = File::from(write2);
        out.write_all(b"[mac] after the move\n").expect("stdout writes");
        err.write_all(b"[mac] stderr too\n").expect("stderr writes");
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "[mac] after the move\n[mac] stderr too\n"
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&log).unwrap().permissions().mode() & 0o777, 0o600);
    }

    /// A child forked and not yet exec'd, as another test's `Command::spawn` leaves one for a moment: between the
    /// fork and the exec it holds a copy of every descriptor of the process, close-on-exec or not.
    struct ForkInFlight {
        go: std::io::PipeWriter,
        spawned: Option<std::thread::JoinHandle<()>>,
    }

    impl ForkInFlight {
        /// Returns once the child is between its fork and its exec.
        fn start() -> ForkInFlight {
            use std::io::Read;
            use std::os::unix::process::CommandExt;
            let (mut ready_read, ready_write) = std::io::pipe().unwrap();
            let (go_read, go_write) = std::io::pipe().unwrap();
            let (ready_fd, go_fd) = (ready_write.as_raw_fd(), go_read.as_raw_fd());
            let spawned = std::thread::spawn(move || {
                let mut command = std::process::Command::new("/usr/bin/true");
                // SAFETY: only `write` and `read` on descriptors the closure's thread keeps open, which are
                // async-signal-safe.
                unsafe {
                    command.pre_exec(move || {
                        let mut byte = 0u8;
                        libc::write(ready_fd, std::ptr::addr_of!(byte).cast(), 1);
                        libc::read(go_fd, std::ptr::addr_of_mut!(byte).cast(), 1);
                        Ok(())
                    });
                }
                command.status().unwrap();
                drop((ready_write, go_read));
            });
            let mut byte = [0u8; 1];
            ready_read.read_exact(&mut byte).unwrap();
            ForkInFlight {
                go: go_write,
                spawned: Some(spawned),
            }
        }

        /// Lets the child exec, which closes its copies of the close-on-exec descriptors.
        fn exec(self) {}
    }

    impl Drop for ForkInFlight {
        fn drop(&mut self) {
            use std::io::Write;
            let _ = self.go.write_all(&[0]);
            if let Some(spawned) = self.spawned.take() {
                let _ = spawned.join();
            }
        }
    }

    /// Two windows of one project write to one log: neither empties it while the other holds it, and each
    /// line lands after the last, whoever wrote it. A start with nobody else holding the log empties it.
    #[test]
    fn two_windows_of_one_project_append_to_its_log_and_only_a_lone_start_empties_it() {
        use std::io::Write;
        let _quiet = crate::spawn_gate::quiet();
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("state/eitri/mac");
        let root = Path::new("/a/project");
        let mut first = open_project_log(&logs, root).expect("the first window's log");
        assert!(first.locked);
        first.file.write_all(b"first: 1\n").unwrap();
        let mut second = open_project_log(&logs, root).expect("the second window's log");
        assert!(second.locked);
        assert_eq!(second.path, first.path, "one log per project");
        let path = first.path.clone();
        second.file.write_all(b"second: 1\n").unwrap();
        first.file.write_all(b"first: 2\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "first: 1\nsecond: 1\nfirst: 2\n",
            "the second start cut nothing and nobody wrote over anybody"
        );
        drop(first);
        let third = open_project_log(&logs, root).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().lines().count(),
            3,
            "a start while another window holds the log cuts nothing"
        );
        drop(third);
        drop(second);
        let _alone = open_project_log(&logs, root).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "", "a lone start empties it");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    /// Until a forked child execs it holds a copy of the lock's descriptor, and a start that asks for the exclusive
    /// lock meanwhile is not alone; the exec closes the copy (the lock's open is close-on-exec), so no child that
    /// is running a program keeps a later start from being alone.
    #[test]
    fn a_forked_child_holds_the_lock_only_until_it_execs() {
        use std::io::Write;
        let _spawning = crate::spawn_gate::spawning();
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        let root = Path::new("/a/project");
        let mut app = open_project_log(&logs, root).unwrap();
        app.file.write_all(b"a line\n").unwrap();
        let path = app.path.clone();
        let in_flight = ForkInFlight::start();
        drop(app);
        let during = open_project_log(&logs, root).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "a line\n",
            "a start while a fork is in flight is not alone"
        );
        drop(during);
        in_flight.exec();
        let _after = open_project_log(&logs, root).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "",
            "after the exec nothing holds the lock"
        );
    }

    /// A child that inherited the log as its output (an orphaned `claude`, a sidecar that outlived the app) does
    /// not hold the lock, so the next lone start still empties the log, and the child goes on appending.
    #[test]
    fn a_child_left_writing_to_the_log_does_not_keep_the_next_start_from_emptying_it() {
        use std::io::Write;
        let _spawning = crate::spawn_gate::spawning();
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("state/eitri/mac");
        let root = Path::new("/a/project");
        let mut app = open_project_log(&logs, root).unwrap();
        app.file.write_all(b"the app's line\n").unwrap();
        let path = app.path.clone();
        // What `move_output_to_log` does: the child's stdout is the log's appending description, not
        // close-on-exec.
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .stdout(std::process::Stdio::from(app.file.try_clone().unwrap()))
            .spawn()
            .unwrap();
        drop(app);
        let next = open_project_log(&logs, root).unwrap();
        let emptied = std::fs::read_to_string(&path).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(next.locked);
        assert_eq!(emptied, "", "the child kept the next start from being alone");
    }

    /// The downgrade from the exclusive lock to the shared one is not atomic on macOS; a start that finds another
    /// holding the exclusive lock for its truncation asks again until it is let go.
    #[test]
    fn the_shared_lock_is_asked_for_again_while_another_start_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, "").unwrap();
        let ours = File::open(&path).unwrap();
        let theirs = File::open(&path).unwrap();
        // SAFETY: `flock` on a descriptor `theirs` owns.
        assert_eq!(
            unsafe { libc::flock(theirs.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let pause = std::time::Duration::from_millis(5);
        assert!(
            !hold_shared(ours.as_raw_fd(), 1, pause),
            "one ask is refused while it truncates"
        );
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            drop(theirs);
        });
        assert!(hold_shared(ours.as_raw_fd(), 400, pause), "asking again gets it");
        release.join().unwrap();
    }

    #[test]
    fn the_shared_lock_is_given_up_after_its_tries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, "").unwrap();
        let ours = File::open(&path).unwrap();
        let theirs = File::open(&path).unwrap();
        // SAFETY: `flock` on a descriptor `theirs` owns.
        assert_eq!(
            unsafe { libc::flock(theirs.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let started = std::time::Instant::now();
        assert!(!hold_shared(ours.as_raw_fd(), 3, std::time::Duration::from_millis(1)));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    /// A log left at a mode nobody can read (a umask of 0477 makes one) is repaired at the start, not given up
    /// on: the lock's open comes after the mode is put right.
    #[test]
    fn a_log_without_read_permission_is_repaired_and_opened() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        let root = Path::new("/a/project");
        std::fs::create_dir_all(&logs).unwrap();
        let path = logs.join(eitri_core::companion::stdio::log_file_name(root));
        std::fs::write(&path, "old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o200)).unwrap();
        let log = open_project_log(&logs, root).expect("a real log, not /dev/null");
        assert!(log.locked);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "", "a lone start empties it");
    }

    #[test]
    fn a_link_in_the_logs_place_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, "kept").unwrap();
        let name = eitri_core::companion::stdio::log_file_name(Path::new("/a/project"));
        std::os::unix::fs::symlink(&target, logs.join(name)).unwrap();
        assert!(open_project_log(&logs, Path::new("/a/project")).is_none());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "kept");
    }

    #[test]
    fn a_terminal_or_a_file_is_not_moved_and_no_log_is_made() {
        let dir = tempfile::tempdir().unwrap();
        let kept = File::create(dir.path().join("redirected")).unwrap();
        let logs = dir.path().join("logs");
        let moved = move_output_to_log(
            kept.as_raw_fd(),
            kept.as_raw_fd(),
            Some(&logs),
            Path::new("/a/project"),
            &|_| panic!("nothing moves"),
        );
        assert_eq!(moved, OutputMoved::default());
        assert!(!logs.exists());
    }

    #[test]
    fn the_log_lives_under_the_state_home() {
        let home = Path::new("/Users/someone");
        assert_eq!(
            log_dir(None, home),
            Some(PathBuf::from("/Users/someone/.local/state/eitri/mac"))
        );
        assert_eq!(
            log_dir(Some(std::ffi::OsStr::new("/s")), home),
            Some(PathBuf::from("/s/eitri/mac"))
        );
    }
}
