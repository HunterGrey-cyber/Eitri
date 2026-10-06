//! `eitri split`'s GTK-free half: which Neovide to run and how, where the editor it starts listens,
//! and how long to wait for that editor. `shell` owns the window; nothing here knows about GTK.
//!
//! A split starts upstream Neovide with `--no-fork` (so the process it spawned is the Neovide, not a
//! launcher that exits at once) and tells the nvim inside it to listen on a socket the split chose, so
//! the panel can attach to exactly that editor.

use std::ffi::OsString;
use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::panel_control;

/// What the editor child must not inherit: the variable that makes Neovide fork, the two that carry
/// an enclosing nvim's RPC address (they would point this editor's nvim at some other one), and the
/// pair that says a tmux pane surrounds it.
///
/// `TMUX` and `TMUX_PANE` go because Neovide is never inside tmux, whatever shell started it: left
/// in, its nvim reports that it runs in tmux, so the navigator fallback leaves Ctrl+h/j/k/l to tmux
/// and smart-splits picks its tmux backend, and a move at nvim's edge then goes to the outer tmux
/// instead of to the panel. The split process itself keeps both.
///
/// `MYVIMRC`, `VIMRUNTIME` and `VIM` are not here: they describe an enclosing nvim only when the
/// split was started from one, and `eitri split` removes them from its own environment then (and
/// only then), so what reaches this command is the user's own export, which a Neovide nvim built
/// from source or Nix needs.
pub const NEOVIDE_ENV_REMOVED: [&str; 5] = ["NEOVIDE_FORK", "NVIM", "NVIM_LISTEN_ADDRESS", "TMUX", "TMUX_PANE"];

/// How often the socket is looked for.
const POLL: Duration = Duration::from_millis(50);

/// How many names are tried for the split socket before giving up.
const SOCKET_ATTEMPTS: u32 = 4;

/// The Neovide to run: `$EITRI_NEOVIDE` when set (it must name an existing file: a typo must not
/// silently run some other editor), else `neovide` on `PATH`. An empty `$EITRI_NEOVIDE` counts as
/// unset.
///
/// Always absolute, made so against this process's working directory: the editor is started in the
/// project root, where a relative name (`./bin/neovide`, or a relative `PATH` entry) would name
/// another file or none.
pub fn neovide_program(
    env: &dyn Fn(&str) -> Option<OsString>,
    which: &dyn Fn(&str) -> Option<PathBuf>,
) -> Result<PathBuf, String> {
    let found = if let Some(named) = env("EITRI_NEOVIDE").filter(|value| !value.is_empty()) {
        let path = PathBuf::from(named);
        if !path.is_file() {
            return Err(format!(
                "eitri split: EITRI_NEOVIDE names {}, which is not a file",
                path.display()
            ));
        }
        path
    } else {
        which("neovide").ok_or_else(|| {
            "eitri split: neovide is not on PATH; install Neovide or set EITRI_NEOVIDE to its path".to_string()
        })?
    };
    std::path::absolute(&found).map_err(|e| format!("eitri split: {}: {e}", found.display()))
}

/// The command that starts the editor: `<program> --no-fork -- --listen <sock>` in the project
/// root, with nothing on stdin and the inherited variables of [`NEOVIDE_ENV_REMOVED`] removed. stdout
/// and stderr are left to the caller's own.
pub fn neovide_command(program: &Path, sock: &Path, project_root: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .args(["--no-fork", "--", "--listen"])
        .arg(sock)
        .current_dir(project_root)
        .stdin(Stdio::null());
    for name in NEOVIDE_ENV_REMOVED {
        command.env_remove(name);
    }
    command
}

/// `dir/split-<project>-<pid>-<nonce>.<ext>` for the first of [`SOCKET_ATTEMPTS`] nonces whose name
/// is free. Anything at a name, of any kind, makes it taken: nothing is removed or followed, because
/// an earlier split's Neovide may still be listening there.
pub fn fresh_socket_path(
    dir: &Path,
    project_root: &Path,
    pid: u32,
    nonce: &mut dyn FnMut() -> u32,
) -> Result<PathBuf, String> {
    for _ in 0..SOCKET_ATTEMPTS {
        let path = panel_control::split_socket_path(dir, project_root, pid, nonce()).map_err(|e| e.to_string())?;
        match path.symlink_metadata() {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(path),
            Err(e) => return Err(format!("eitri split: could not look at {}: {e}", path.display())),
            Ok(_) => {}
        }
    }
    Err(format!(
        "eitri split: every socket name tried in {} was taken",
        dir.display()
    ))
}

/// Four random bytes. `/dev/urandom` is the source; where it cannot be read the clock and the pid
/// stand in, since the name is only made unique (a taken name is detected and another is drawn),
/// never secret.
pub fn random_nonce() -> u32 {
    let mut bytes = [0u8; 4];
    let read = std::fs::File::open("/dev/urandom").and_then(|mut file| file.read_exact(&mut bytes));
    if read.is_ok() {
        return u32::from_ne_bytes(bytes);
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.subsec_nanos())
        .unwrap_or(0);
    nanos ^ std::process::id().rotate_left(16)
}

fn shown(deadline: Duration) -> String {
    if deadline.subsec_millis() == 0 {
        format!("{} s", deadline.as_secs())
    } else {
        format!("{} ms", deadline.as_millis())
    }
}

/// Waits until the editor `child` started accepts connections on `sock`, for at most `deadline`.
/// Each poll first asks whether the child has already exited, so a Neovide that dies on start is
/// reported at once and not after the deadline. The socket file appears when nvim binds, a moment
/// before it listens, so a refused connect is just not yet; anything else at the path (a link, a
/// file that is not a socket, another user's socket) is refused by `validate_nvim_addr`.
pub fn wait_for_socket(sock: &Path, child: &mut Child, euid: u32, deadline: Duration) -> Result<(), String> {
    let end = Instant::now() + deadline;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!(
                    "eitri split: neovide exited ({status}) before nvim listened on {}",
                    sock.display()
                ));
            }
            Ok(None) => {}
            Err(e) => return Err(format!("eitri split: could not wait for neovide: {e}")),
        }
        if sock.symlink_metadata().is_ok() {
            // Only a socket of ours is worth connecting to; anything else will not become one.
            panel_control::validate_nvim_addr(sock, euid)?;
            match UnixStream::connect(sock) {
                Ok(_) => return Ok(()),
                Err(e) if matches!(e.kind(), io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound) => {}
                Err(e) => return Err(format!("eitri split: could not connect to {}: {e}", sock.display())),
            }
        }
        if Instant::now() >= end {
            return Err(format!(
                "eitri split: nvim did not listen on {} within {}; Neovide is left running",
                sock.display(),
                shown(deadline)
            ));
        }
        std::thread::sleep(POLL);
    }
}

/// Field 22 of `/proc/<pid>/stat`: when `pid` started, in clock ticks since boot. Together with the
/// pid it names one process for good, which a pid alone does not once it is reused. `None` for a
/// process that is gone, and off Linux.
pub fn proc_start_time(pid: u32) -> Option<u64> {
    crate::wm::proc_stat(pid)
        .as_deref()
        .and_then(panel_control::stat_parent_and_start)
        .map(|(_, start)| start)
}

#[cfg(test)]
mod tests {

    /// The editor starts in the project root, so a relative name must not reach it as one.
    #[test]
    fn a_relative_neovide_is_made_absolute_against_this_processs_cwd() {
        let cwd = std::env::current_dir().unwrap();
        // `Cargo.toml` is a file in this crate's directory, which is the cwd of its tests.
        let program = neovide_program(
            &|name| (name == "EITRI_NEOVIDE").then(|| OsString::from("./Cargo.toml")),
            &|_| None,
        )
        .unwrap();
        assert!(program.is_absolute(), "{}", program.display());
        assert_eq!(program, cwd.join("Cargo.toml"));
        let from_path = neovide_program(&|_| None, &|_| Some(PathBuf::from("rel/neovide"))).unwrap();
        assert_eq!(from_path, cwd.join("rel/neovide"));
    }
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::ffi::OsStr;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        }
    }

    fn scratch(case: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sp-{}-{case}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn eitri_neovide_wins_over_the_path() {
        let dir = scratch("prog1");
        let named = dir.join("my-neovide");
        std::fs::write(&named, "").unwrap();
        let text = named.to_str().unwrap().to_owned();
        let pairs = [("EITRI_NEOVIDE", text.as_str())];
        let env = env_of(&pairs);
        let on_path = |_: &str| Some(PathBuf::from("/usr/bin/neovide"));
        assert_eq!(neovide_program(&env, &on_path), Ok(named.clone()));
    }

    #[test]
    fn a_named_neovide_that_is_missing_is_an_error_not_a_fallback() {
        let env = env_of(&[("EITRI_NEOVIDE", "/no/such/eitri/neovide")]);
        let on_path = |_: &str| Some(PathBuf::from("/usr/bin/neovide"));
        let err = neovide_program(&env, &on_path).unwrap_err();
        assert!(
            err.contains("/no/such/eitri/neovide") && err.contains("not a file"),
            "{err}"
        );
        // A directory is not a Neovide either.
        let env = env_of(&[("EITRI_NEOVIDE", "/")]);
        assert!(neovide_program(&env, &on_path).is_err());
    }

    #[test]
    fn without_eitri_neovide_the_path_is_asked_and_an_empty_value_counts_as_unset() {
        let asked = RefCell::new(Vec::new());
        let on_path = |name: &str| {
            asked.borrow_mut().push(name.to_string());
            Some(PathBuf::from("/usr/bin/neovide"))
        };
        assert_eq!(
            neovide_program(&env_of(&[]), &on_path),
            Ok(PathBuf::from("/usr/bin/neovide"))
        );
        assert_eq!(
            neovide_program(&env_of(&[("EITRI_NEOVIDE", "")]), &on_path),
            Ok(PathBuf::from("/usr/bin/neovide"))
        );
        assert_eq!(asked.into_inner(), ["neovide", "neovide"]);
    }

    #[test]
    fn no_neovide_anywhere_says_how_to_name_one() {
        let err = neovide_program(&env_of(&[]), &|_| None).unwrap_err();
        assert!(err.starts_with("eitri split: "), "{err}");
        assert!(err.contains("PATH") && err.contains("EITRI_NEOVIDE"), "{err}");
    }

    #[test]
    fn the_command_is_exactly_the_documented_argv_cwd_and_environment() {
        let command = neovide_command(
            Path::new("/opt/neovide"),
            Path::new("/run/user/1/eitri/s"),
            Path::new("/work/proj"),
        );
        assert_eq!(command.get_program(), OsStr::new("/opt/neovide"));
        let args: Vec<&OsStr> = command.get_args().collect();
        assert_eq!(
            args,
            ["--no-fork", "--", "--listen", "/run/user/1/eitri/s"].map(OsStr::new)
        );
        assert_eq!(command.get_current_dir(), Some(Path::new("/work/proj")));
        // Removals show as a name with no value; nothing is set.
        let envs: HashMap<&OsStr, Option<&OsStr>> = command.get_envs().collect();
        assert_eq!(envs.len(), NEOVIDE_ENV_REMOVED.len());
        for name in ["NEOVIDE_FORK", "NVIM", "NVIM_LISTEN_ADDRESS", "TMUX", "TMUX_PANE"] {
            assert_eq!(envs.get(OsStr::new(name)), Some(&None), "{name} must be removed");
        }
        // The user's own exports describe no enclosing nvim and are left for Neovide's nvim.
        for name in ["MYVIMRC", "VIMRUNTIME", "VIM"] {
            assert!(!envs.contains_key(OsStr::new(name)), "{name} must be left alone");
        }
    }

    #[test]
    fn a_free_socket_name_is_returned_as_it_is() {
        let dir = scratch("free");
        let root = Path::new("/work/proj");
        let mut draws = vec![0x1234_abcd];
        let path = fresh_socket_path(&dir, root, 42, &mut || draws.remove(0)).unwrap();
        assert_eq!(
            path,
            panel_control::split_socket_path(&dir, root, 42, 0x1234_abcd).unwrap()
        );
        assert!(path.extension().is_some_and(|e| e == "sock"));
    }

    #[test]
    fn a_taken_name_of_any_kind_is_never_removed_and_another_nonce_is_drawn() {
        let dir = scratch("taken");
        let root = Path::new("/work/proj");
        let name = |nonce| panel_control::split_socket_path(&dir, root, 7, nonce).unwrap();
        // A regular file, a directory and a dangling link.
        std::fs::write(name(1), "x").unwrap();
        std::fs::create_dir(name(2)).unwrap();
        std::os::unix::fs::symlink("/no/such/target", name(3)).unwrap();
        let mut next = 0;
        let path = fresh_socket_path(&dir, root, 7, &mut || {
            next += 1;
            next
        })
        .unwrap();
        assert_eq!(path, name(4));
        assert!(name(1).is_file() && name(2).is_dir() && name(3).symlink_metadata().is_ok());
    }

    #[test]
    fn four_taken_names_fail_without_removing_any() {
        let dir = scratch("four");
        let root = Path::new("/work/proj");
        let name = |nonce| panel_control::split_socket_path(&dir, root, 7, nonce).unwrap();
        for nonce in 1..=4 {
            std::fs::write(name(nonce), "x").unwrap();
        }
        let mut next = 0;
        let mut drawn = 0;
        let err = fresh_socket_path(&dir, root, 7, &mut || {
            next += 1;
            drawn += 1;
            next
        })
        .unwrap_err();
        assert!(err.contains("taken"), "{err}");
        assert_eq!(drawn, 4);
        assert!((1..=4).all(|nonce| name(nonce).is_file()));
    }

    #[test]
    fn the_nonce_is_not_a_constant() {
        let draws: std::collections::HashSet<u32> = (0..8).map(|_| random_nonce()).collect();
        assert!(draws.len() > 1);
    }

    // `proc_start_time` reads `/proc`, which only Linux has; elsewhere it answers `None` by design.
    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_has_a_start_time_that_is_stable() {
        let pid = std::process::id();
        let first = proc_start_time(pid).expect("the test process has a /proc entry");
        assert_eq!(proc_start_time(pid), Some(first));
        assert_ne!(first, 0);
    }

    #[test]
    fn a_pid_that_is_gone_has_no_start_time() {
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert_eq!(proc_start_time(pid), None);
    }

    #[test]
    fn a_child_that_exits_early_is_reported_before_the_deadline() {
        let dir = scratch("early");
        let mut child = Command::new("false").spawn().unwrap();
        let started = Instant::now();
        let err = wait_for_socket(&dir.join("nothing"), &mut child, 0, Duration::from_secs(10)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(err.starts_with("eitri split: neovide exited ("), "{err}");
        assert!(err.contains("before nvim listened on"), "{err}");
    }

    #[test]
    fn a_child_that_never_listens_times_out_and_is_left_running() {
        let dir = scratch("never");
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let err = wait_for_socket(&dir.join("nothing"), &mut child, 0, Duration::from_millis(300)).unwrap_err();
        assert!(err.contains("did not listen") && err.contains("within 300 ms"), "{err}");
        assert!(err.ends_with("Neovide is left running"), "{err}");
        assert!(child.try_wait().unwrap().is_none(), "the child must still run");
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
