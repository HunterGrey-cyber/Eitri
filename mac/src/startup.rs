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
pub fn neovide_vars_to_remove(vars: impl Iterator<Item = (OsString, OsString)>) -> Vec<OsString> {
    let names: Vec<OsString> = vars.map(|(name, _)| name).collect();
    let started_by_an_editor = names.iter().any(|name| name == "NVIM");
    let mut removed: Vec<OsString> = Vec::new();
    for name in names {
        let bytes = name.as_bytes();
        let listed = NEOVIDE_ENV_REMOVED
            .iter()
            .chain(&SET_ON_THE_NVIM_CHILD)
            .any(|listed| name == *listed);
        let runtime = started_by_an_editor && ["MYVIMRC", "VIMRUNTIME", "VIM"].iter().any(|n| name == *n);
        if (listed || bytes.starts_with(b"NEOVIDE_") || runtime) && !removed.contains(&name) {
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
/// the project. An empty one counts as unset. The configuration directory is not `$XDG_CONFIG_HOME/eitri`:
/// `init.lua` is read from `~/.config/eitri` on every host, and Neovide's file must sit beside it.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::mem::ManuallyDrop;
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

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
        let run = tempfile::tempdir().unwrap();
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
        let run = tempfile::tempdir().unwrap();
        let home = Path::new("/Users/u");
        assert!(paths(&env_of(&[("EITRI_CONFIG_DIR", "rel")]), home, run.path()).is_err());
        let p = paths(&env_of(&[("EITRI_CONFIG_DIR", "")]), home, run.path()).unwrap();
        assert_eq!(p.config_dir, PathBuf::from("/Users/u/.config/eitri"));
        let p = paths(&env_of(&[("EITRI_CONFIG_DIR", "/c")]), home, run.path()).unwrap();
        assert_eq!(p.neovide_config, PathBuf::from("/c/neovide.toml"));
    }

    #[test]
    fn a_relative_state_home_falls_back_to_home() {
        let run = tempfile::tempdir().unwrap();
        let p = paths(&env_of(&[("XDG_STATE_HOME", "rel")]), Path::new("/Users/u"), run.path()).unwrap();
        assert_eq!(p.neovide_data, PathBuf::from("/Users/u/.local/state/eitri/neovide"));
    }

    #[test]
    fn a_relative_home_is_refused() {
        let run = tempfile::tempdir().unwrap();
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
}
