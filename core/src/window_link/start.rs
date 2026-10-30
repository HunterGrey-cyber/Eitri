//! How window 2 comes to exist: window 1 starts it with `eitri linked-window [--legacy] [--clean] [--restore] <fd> 2
//! <root> [token]`, the link's other
//! end placed at that descriptor number, and window 2's first acts check that end before anything else runs.
//!
//! The command line carries what window 2 needs because `main()`'s flags take no values and an environment variable
//! would be inherited by every terminal window 2 starts; the activation token travels as an argument for the same
//! reason. Window 2 refuses to start unless the inherited descriptor is a seqpacket socket whose peer is its parent
//! with the same effective uid: the pair is made by window 1 just before the spawn, so any other peer means the
//! command was run by someone else. Window 2 has no terminal: its stdin is `/dev/null`, it leads its own process
//! group, and its stdout and stderr go to its own log, so neither a `Ctrl+C` in nor the closing of the terminal that
//! launched window 1 reaches it, and a write to a closed terminal cannot fail inside it.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use super::fdpass;
use super::pair::log_file_name;

/// The subcommand `main()` dispatches before the project resolver.
pub const SUBCOMMAND: &str = "linked-window";

/// The descriptor number window 2 finds its link end at (0-2 are its stdio).
pub const LINK_FD: RawFd = 3;

/// The only window a linked window can be.
pub const SECOND_WINDOW: &str = "2";

/// How many earlier logs are kept beside the current one: the previous run's, as `<name>.1`.
pub const LOG_KEEP: usize = 1;

/// Window 2's command line after the subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedArgs {
    pub fd: RawFd,
    pub window: String,
    pub root: PathBuf,
    /// An xdg-activation token from window 1, so window 2 opens focused.
    pub token: Option<String>,
    /// Window 1 started this window again as the pair last ended it ([`RESTORE_FLAG`]).
    pub restore: bool,
}

/// Window 1's `--legacy`, passed on to window 2 ahead of its other arguments: the flag picks a launch's chat
/// backend, and window 2's chats are to run the backend window 1's run. `main()` reads it as on any launch.
pub const LEGACY_FLAG: &str = "--legacy";

/// Window 1's `--clean`, passed on to window 2 after `--legacy`: a clean launch neither reads nor writes the user's
/// remembered state, and a window 2 it starts is part of that launch. `main()` reads it as on any launch.
pub const CLEAN_FLAG: &str = "--clean";

/// Window 1 starting window 2 again at its own launch, because window 2 was open when the pair last ended: window 2
/// then builds what the pair file says it held, offers its own saved tabs and goes back to its monitor. Written
/// after `--clean`.
pub const RESTORE_FLAG: &str = "--restore";

/// Parses `[--legacy] [--clean] [--restore] <fd> <window> <root> [token]`, the arguments after `linked-window`; each
/// flag at most once and in that order, as window 1 writes them.
pub fn parse_args(args: &[OsString]) -> Result<LinkedArgs, String> {
    let usage = format!(
        "usage: eitri {SUBCOMMAND} [{LEGACY_FLAG}] [{CLEAN_FLAG}] [{RESTORE_FLAG}] <fd> {SECOND_WINDOW} <root> [token]"
    );
    let mut args = args;
    let mut restore = false;
    for flag in [LEGACY_FLAG, CLEAN_FLAG, RESTORE_FLAG] {
        if let Some((first, rest)) = args.split_first() {
            if first == flag {
                args = rest;
                restore |= flag == RESTORE_FLAG;
            }
        }
    }
    let (fd, window, root, token) = match args {
        [fd, window, root] => (fd, window, root, None),
        [fd, window, root, token] => (fd, window, root, Some(token)),
        _ => return Err(format!("{SUBCOMMAND}: {} arguments; {usage}", args.len())),
    };
    let fd = fd
        .to_str()
        .and_then(|s| s.parse::<RawFd>().ok())
        .ok_or_else(|| format!("{SUBCOMMAND}: the descriptor {fd:?} is not a number; {usage}"))?;
    if fd < LINK_FD {
        return Err(format!(
            "{SUBCOMMAND}: the descriptor {fd} is below {LINK_FD} (0-2 are stdio)"
        ));
    }
    if window != SECOND_WINDOW {
        return Err(format!(
            "{SUBCOMMAND}: the window {window:?} is not {SECOND_WINDOW:?}, the only one started this way"
        ));
    }
    let root = PathBuf::from(root);
    if !root.is_absolute() {
        return Err(format!(
            "{SUBCOMMAND}: the project root {} is not an absolute path",
            root.display()
        ));
    }
    let token = match token {
        None => None,
        Some(t) => match t.to_str() {
            Some("") => return Err(format!("{SUBCOMMAND}: an empty activation token")),
            Some(t) => Some(t.to_owned()),
            None => return Err(format!("{SUBCOMMAND}: the activation token is not UTF-8")),
        },
    };
    Ok(LinkedArgs {
        fd,
        window: window.to_string_lossy().into_owned(),
        root,
        token,
        restore,
    })
}

/// Window 2's first acts on its link end, before any thread or child exists: the descriptor is made close-on-exec
/// first, so nothing started later can inherit it whatever the checks find; then it must be a seqpacket socket whose
/// peer is this process's parent with this process's effective uid. Only then is it owned. An error names what
/// failed; the caller exits 2 without opening a window.
pub fn first_acts(args: &LinkedArgs) -> Result<OwnedFd, String> {
    let fd = args.fd;
    fdpass::set_cloexec_raw(fd).map_err(|e| format!("the link descriptor {fd}: {e}"))?;
    // SAFETY: the descriptor is open (making it close-on-exec just succeeded) and stays open for these calls; nothing
    // else in this process uses it yet.
    let link = unsafe { BorrowedFd::borrow_raw(fd) };
    let kind = fdpass::socket_type(link).map_err(|e| format!("the link descriptor {fd} is not a socket: {e}"))?;
    if kind != libc::SOCK_SEQPACKET {
        return Err(format!(
            "the link descriptor {fd} is not a seqpacket socket (type {kind})"
        ));
    }
    let (pid, uid) =
        fdpass::peer_cred(link).map_err(|e| format!("the link descriptor {fd} has no peer credentials: {e}"))?;
    // SAFETY: neither call has preconditions or can fail.
    let (parent, euid) = unsafe { (libc::getppid(), libc::geteuid()) };
    if i64::from(pid) != i64::from(parent) {
        return Err(format!(
            "the link's peer is process {pid}, not this window's parent {parent}"
        ));
    }
    if uid != euid {
        return Err(format!("the link's peer runs as uid {uid}, not this window's {euid}"));
    }
    // SAFETY: the checks above found an open socket that nothing else in this process owns; from here this is its
    // one owner.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Opens window 2's log, `<state_home>/eitri/log/window-2-<16 hex>.log`, where `state_home` is `$XDG_STATE_HOME` (or
/// `$HOME/.local/state`). The directory is made private; the previous run's log is renamed `<name>.1` (replacing an
/// older one) and a new one is opened 0600, refusing a link in its place. The caller `dup2`s it onto 1 and 2.
pub fn open_log(state_home: &Path, root: &Path) -> io::Result<File> {
    let dir = state_home.join("eitri").join("log");
    crate::state_paths::create_state_dir(&dir)?;
    let name = log_file_name(root);
    let path = dir.join(&name);
    match std::fs::rename(&path, dir.join(format!("{name}.{LOG_KEEP}"))) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    agent::private_fs::open_private(&path)
}

/// Starts window 2 from `exe` (window 1's `/proc/self/exe`, so both windows are one build even after an upgrade
/// replaced the installed file) as `linked-window [--legacy] [--clean] [--restore] 3 2 <root> [token]` (`--legacy`
/// when `legacy`, `--clean` when `clean`, `--restore` when `restore`), with `child_end` at descriptor 3 and not
/// close-on-exec there, stdio on `/dev/null`, in its own process group. `child_end` itself stays close-on-exec in
/// this process and is closed when the spawn returns, so window 1 keeps no copy of window 2's end.
pub fn spawn_second(
    exe: &Path,
    child_end: OwnedFd,
    root: &Path,
    token: Option<&str>,
    legacy: bool,
    clean: bool,
    restore: bool,
) -> io::Result<Child> {
    let mut command = second_command(exe, root, token, legacy, clean, restore);
    let raw = child_end.as_raw_fd();
    // SAFETY: the closure runs in the forked child before `exec` and calls only `fcntl` and `dup2`, both
    // async-signal-safe; `raw` stays open in this process until the spawn has returned.
    unsafe {
        command.pre_exec(move || place_link_end(raw));
    }
    let child = command.spawn();
    drop(child_end);
    child
}

/// Window 2's command without its link end: the arguments, the stdio, the process group. Nothing is added to the
/// environment.
fn second_command(exe: &Path, root: &Path, token: Option<&str>, legacy: bool, clean: bool, restore: bool) -> Command {
    let mut command = Command::new(exe);
    command.arg(SUBCOMMAND);
    if legacy {
        command.arg(LEGACY_FLAG);
    }
    if clean {
        command.arg(CLEAN_FLAG);
    }
    if restore {
        command.arg(RESTORE_FLAG);
    }
    command.arg(LINK_FD.to_string()).arg(SECOND_WINDOW).arg(root);
    if let Some(token) = token.filter(|t| !t.is_empty()) {
        command.arg(token);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    command
}

/// In the forked child: puts the link end at [`LINK_FD`], not close-on-exec there. `dup2` gives a copy without the
/// flag; when the end already sits at that number, `dup2` would leave the flag as it is, so it is cleared instead.
fn place_link_end(raw: RawFd) -> io::Result<()> {
    if raw == LINK_FD {
        // SAFETY: `fcntl` on an open descriptor; async-signal-safe.
        let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
        // SAFETY: as above.
        if flags < 0 || unsafe { libc::fcntl(raw, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(());
    }
    // SAFETY: `dup2` onto a fixed number; async-signal-safe.
    if unsafe { libc::dup2(raw, LINK_FD) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_scratch_dir::ScratchDir;
    use std::io::{Read, Write};
    use std::os::fd::AsFd;
    use std::os::unix::fs::PermissionsExt;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn arguments_name_the_fd_the_window_the_root_and_an_optional_token() {
        assert_eq!(
            parse_args(&os(&["3", "2", "/home/someone/project"])).unwrap(),
            LinkedArgs {
                fd: 3,
                window: "2".into(),
                root: "/home/someone/project".into(),
                token: None,
                restore: false,
            }
        );
        assert_eq!(
            parse_args(&os(&["7", "2", "/p", "tok-1"])).unwrap().token.as_deref(),
            Some("tok-1")
        );
        let refused = [
            (vec!["3", "3", "/p"], "a window other than 2"),
            (vec!["3", "1", "/p"], "window 1 is never started this way"),
            (vec!["2", "2", "/p"], "a descriptor among stdio"),
            (vec!["-1", "2", "/p"], "a negative descriptor"),
            (vec!["three", "2", "/p"], "a descriptor that is not a number"),
            (vec!["3", "2", "relative/project"], "a relative root"),
            (vec!["3", "2", ""], "an empty root"),
            (vec!["3", "2"], "a missing root"),
            (vec![], "nothing at all"),
            (vec!["3", "2", "/p", ""], "an empty token"),
            (vec!["3", "2", "/p", "tok", "extra"], "an argument too many"),
        ];
        for (args, why) in refused {
            assert!(parse_args(&os(&args)).is_err(), "{why} is refused: {args:?}");
        }
    }

    #[test]
    fn first_acts_sets_close_on_exec_before_checking() {
        let (a, _b) = fdpass::seqpacket_pair().unwrap();
        // SAFETY: `dup` makes a new descriptor without close-on-exec, as an inherited one would be.
        let raw = unsafe { libc::dup(a.as_raw_fd()) };
        assert!(raw >= 3);
        // SAFETY: `raw` was just made and nothing else owns it.
        let copy = unsafe { OwnedFd::from_raw_fd(raw) };
        assert!(!fdpass::is_cloexec(copy.as_fd()).unwrap());
        let args = LinkedArgs {
            fd: raw,
            window: "2".into(),
            root: "/p".into(),
            token: None,
            restore: false,
        };
        let err = first_acts(&args).expect_err("this process's own pair has no parent at its other end");
        assert!(err.contains("parent"), "the refusal names the peer check: {err}");
        assert!(
            fdpass::is_cloexec(copy.as_fd()).unwrap(),
            "a refused descriptor is close-on-exec all the same"
        );
    }

    #[test]
    fn first_acts_refuse_what_is_not_a_seqpacket_socket() {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `fds` has room for two descriptors.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), 0) }, 0);
        // SAFETY: both are new and owned only here.
        let (r, _w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let args = |fd| LinkedArgs {
            fd,
            window: "2".into(),
            root: "/p".into(),
            token: None,
            restore: false,
        };
        let err = first_acts(&args(r.as_raw_fd())).unwrap_err();
        assert!(err.contains("socket"), "a pipe is refused as not a socket: {err}");
        assert!(fdpass::is_cloexec(r.as_fd()).unwrap());

        // SAFETY: `fds` has room for two descriptors.
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) },
            0
        );
        // SAFETY: both are new and owned only here.
        let (s, _t) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let err = first_acts(&args(s.as_raw_fd())).unwrap_err();
        assert!(err.contains("seqpacket"), "a stream socket is refused: {err}");

        // Past every descriptor this process holds.
        let err = first_acts(&args(1_000_000)).unwrap_err();
        assert!(!err.is_empty(), "a descriptor that is not open is refused");
    }

    // Spawning tests live in `core/tests/window_link_start.rs`: a fork from this binary would hold copies of the
    // descriptors other tests here watch (the pair lock's among them) until it execs.

    #[test]
    fn the_command_carries_the_token_as_its_last_argument_and_sets_no_environment() {
        let token = "activation-token-5f3a91";
        let command = second_command(
            Path::new("/usr/bin/eitri"),
            Path::new("/p"),
            Some(token),
            false,
            false,
            false,
        );
        assert_eq!(
            command.get_envs().count(),
            0,
            "the command sets no environment variable"
        );
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [SUBCOMMAND, "3", "2", "/p", token],
            "the token is the last argument"
        );
        let without = second_command(Path::new("/usr/bin/eitri"), Path::new("/p"), None, false, false, false);
        assert_eq!(without.get_args().collect::<Vec<_>>(), [SUBCOMMAND, "3", "2", "/p"]);
    }

    #[test]
    fn a_legacy_window_1_starts_window_2_with_the_same_flag_and_it_parses() {
        let command = second_command(
            Path::new("/usr/bin/eitri"),
            Path::new("/p"),
            Some("tok"),
            true,
            false,
            false,
        );
        let args: Vec<OsString> = command.get_args().map(OsString::from).collect();
        assert_eq!(args, os(&[SUBCOMMAND, LEGACY_FLAG, "3", "2", "/p", "tok"]));
        let parsed = parse_args(&args[1..]).unwrap();
        assert_eq!(
            (parsed.fd, parsed.token.as_deref()),
            (3, Some("tok")),
            "the flag is not read as an argument"
        );
        assert!(parse_args(&os(&[LEGACY_FLAG, "3", "2", "/p", "tok", "more"])).is_err());
    }

    #[test]
    fn a_clean_window_1_starts_window_2_with_the_same_flag_and_it_parses() {
        let exe = Path::new("/usr/bin/eitri");
        let args_of = |command: Command| -> Vec<OsString> { command.get_args().map(OsString::from).collect() };
        let clean = args_of(second_command(exe, Path::new("/p"), Some("tok"), false, true, false));
        assert_eq!(clean, os(&[SUBCOMMAND, "--clean", "3", "2", "/p", "tok"]));
        let both = args_of(second_command(exe, Path::new("/p"), Some("tok"), true, true, false));
        assert_eq!(
            both,
            os(&[SUBCOMMAND, "--legacy", "--clean", "3", "2", "/p", "tok"]),
            "--clean follows --legacy"
        );
        let expected = LinkedArgs {
            fd: 3,
            window: "2".into(),
            root: "/p".into(),
            token: Some("tok".into()),
            restore: false,
        };
        assert_eq!(
            parse_args(&clean[1..]).unwrap(),
            expected,
            "the flag is not read as an argument"
        );
        assert_eq!(
            parse_args(&both[1..]).unwrap(),
            expected,
            "neither flag is read as an argument"
        );
        let refused = [
            (
                vec!["--clean", "--legacy", "3", "2", "/p"],
                "the flags in the other order",
            ),
            (vec!["--clean", "--clean", "3", "2", "/p"], "--clean twice"),
            (vec!["--legacy", "--legacy", "3", "2", "/p"], "--legacy twice"),
            (
                vec!["--legacy", "--clean", "--clean", "3", "2", "/p"],
                "--clean twice after --legacy",
            ),
            (
                vec!["--legacy", "--clean", "3", "2", "/p", "tok", "more"],
                "an argument too many",
            ),
        ];
        for (args, why) in refused {
            assert!(parse_args(&os(&args)).is_err(), "{why} is refused: {args:?}");
        }
    }

    #[test]
    fn the_restore_flag_parses_beside_legacy_and_clean() {
        let exe = Path::new("/usr/bin/eitri");
        let args_of = |command: Command| -> Vec<OsString> { command.get_args().map(OsString::from).collect() };
        let restore = args_of(second_command(exe, Path::new("/p"), None, false, false, true));
        assert_eq!(restore, os(&[SUBCOMMAND, RESTORE_FLAG, "3", "2", "/p"]));
        let all = args_of(second_command(exe, Path::new("/p"), Some("tok"), true, true, true));
        assert_eq!(
            all,
            os(&[SUBCOMMAND, LEGACY_FLAG, CLEAN_FLAG, RESTORE_FLAG, "3", "2", "/p", "tok"]),
            "--restore follows --legacy and --clean"
        );
        let parsed = parse_args(&restore[1..]).unwrap();
        assert!(parsed.restore, "the flag is read");
        assert_eq!((parsed.fd, parsed.token), (3, None), "and not as an argument");
        let parsed = parse_args(&all[1..]).unwrap();
        assert!(parsed.restore);
        assert_eq!(parsed.token.as_deref(), Some("tok"));
        assert!(
            !parse_args(&os(&[LEGACY_FLAG, CLEAN_FLAG, "3", "2", "/p"]))
                .unwrap()
                .restore,
            "no flag, no restore"
        );
        let refused = [
            (vec!["--restore", "--clean", "3", "2", "/p"], "--restore before --clean"),
            (
                vec!["--restore", "--legacy", "3", "2", "/p"],
                "--restore before --legacy",
            ),
            (vec!["--restore", "--restore", "3", "2", "/p"], "--restore twice"),
            (
                vec!["3", "2", "/p", "--restore"],
                "--restore after the root, where the token goes",
            ),
        ];
        for (args, why) in refused {
            let parsed = parse_args(&os(&args));
            assert!(
                parsed
                    .as_ref()
                    .map_or(true, |a| a.token.as_deref() == Some("--restore") && !a.restore),
                "{why} is refused or read as a token: {args:?} gave {parsed:?}"
            );
        }
    }

    #[test]
    fn the_log_is_private_and_the_last_one_is_kept_as_dot_one() {
        let state = ScratchDir::new("mw-start", "log");
        let root = Path::new("/home/someone/project");
        let dir = state.join("eitri").join("log");
        let name = log_file_name(root);

        let mut first = open_log(&state, root).unwrap();
        first.write_all(b"first run\n").unwrap();
        drop(first);
        let mut second = open_log(&state, root).unwrap();
        second.write_all(b"second run\n").unwrap();
        drop(second);
        let mut third = open_log(&state, root).unwrap();
        third.write_all(b"third run\n").unwrap();
        assert!(fdpass::is_cloexec(third.as_fd()).unwrap());
        drop(third);

        let read = |p: &Path| {
            let mut s = String::new();
            File::open(p).unwrap().read_to_string(&mut s).unwrap();
            s
        };
        assert_eq!(read(&dir.join(&name)), "third run\n");
        assert_eq!(
            read(&dir.join(format!("{name}.1"))),
            "second run\n",
            "only the run before is kept"
        );
        assert!(!dir.join(format!("{name}.2")).exists());
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.join(&name)), 0o600);
        assert_eq!(mode(&dir.join(format!("{name}.1"))), 0o600);
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&state.join("eitri")), 0o700);
    }

    #[test]
    fn a_link_in_the_logs_place_is_kept_aside_and_never_written_through() {
        let state = ScratchDir::new("mw-start", "log-link");
        let root = Path::new("/home/someone/project");
        let dir = state.join("eitri").join("log");
        std::fs::create_dir_all(&dir).unwrap();
        let target = state.join("elsewhere");
        std::fs::write(&target, "untouched").unwrap();
        let name = log_file_name(root);
        std::os::unix::fs::symlink(&target, dir.join(&name)).unwrap();

        let mut log = open_log(&state, root).unwrap();
        log.write_all(b"run\n").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
        assert!(!std::fs::symlink_metadata(dir.join(&name))
            .unwrap()
            .file_type()
            .is_symlink());
    }
}
