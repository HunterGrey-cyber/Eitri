//! Starting another project's window: a process of its own, which this window neither waits for nor takes down.

#[path = "project_start/remote.rs"]
pub(crate) mod remote;

use eitri_core::nvim_bin::{self, NEOVIM_BIN_ENV};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// How long after the spawn a bad exit counts as the start having failed. A window that lived longer failed some
/// other way, and it says so itself.
pub(crate) const FAILED_WITHIN: Duration = Duration::from_secs(5);

/// The kernel's name for this process's binary, which works even when no path reaches that binary any more.
const PROC_SELF_EXE: &str = "/proc/self/exe";
/// What the kernel adds to the path of a binary that was replaced or removed while it ran.
const DELETED: &[u8] = b" (deleted)";
/// The variable a Wayland launch reads its activation token from.
const TOKEN_ENV: &str = "XDG_ACTIVATION_TOKEN";

/// The binary a new project's window runs: this one's path, which after an update names the new build, and this
/// process's own image when no path reaches a binary. `exists` stats, so this runs off the GTK thread.
pub(crate) fn exe_to_start(current: io::Result<PathBuf>, exists: &dyn Fn(&Path) -> bool) -> PathBuf {
    let Ok(path) = current else {
        return PROC_SELF_EXE.into();
    };
    let bytes = path.as_os_str().as_bytes();
    let replaced = bytes
        .strip_suffix(DELETED)
        .map(|kept| PathBuf::from(OsStr::from_bytes(kept)));
    replaced
        .into_iter()
        .chain([path])
        .find(|candidate| exists(candidate))
        .unwrap_or_else(|| PROC_SELF_EXE.into())
}

/// A started child. `ended` carries one message, its exit: the code, or `None` for a death by signal. A sender gone
/// with no message means the exit could not be read, which says nothing about how the child is doing.
pub(crate) struct Started {
    pub pid: u32,
    pub ended: mpsc::Receiver<Option<i32>>,
}

impl Started {
    /// The exit to report when the child ended badly inside `window`, which blocks for as long as `window` lasts. A
    /// child still running at its end is left to the thread that reaps it.
    pub(crate) fn failed_within(&self, window: Duration) -> Option<Option<i32>> {
        self.ended.recv_timeout(window).ok().filter(|code| *code != Some(0))
    }
}

/// Whether `path` is not there for a start to open: nothing under that name, a link to nothing, or a parent that is a
/// file. Asked once a start has failed, so the reason the child had can be said; it stats, so never on the GTK thread.
pub(crate) fn not_there(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(_) => false,
        Err(error) => error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ENOTDIR),
    }
}

/// What the child is asked to do with its path.
pub(crate) enum How {
    /// Bring the path's running window forward, or be that window: `--raise -- <path>`.
    Raise(PathBuf),
    /// Be a window of the path, as any launch is: `-- <path>`.
    Plain(PathBuf),
    /// Open the remote project using its recorded host.
    Remote { host: String, path: PathBuf },
}

/// What a start comes to: as [`Started::failed_within`], and a spawn that failed has no exit code to report.
pub(crate) fn failure(start: io::Result<Started>, window: Duration) -> Option<Option<i32>> {
    match start {
        Ok(started) => started.failed_within(window),
        Err(_) => Some(None),
    }
}

fn command(exe: &Path, flags: &[OsString], how: How, token: Option<&str>) -> Command {
    let mut command = Command::new(exe);
    command.args(flags);
    let path = match how {
        How::Raise(path) => {
            command.arg("--raise");
            path
        }
        How::Plain(path) => path,
        How::Remote { host, path } => {
            command.arg("--ssh").arg(host);
            path
        }
    };
    command.arg("--").arg(path);
    // A token is good for one launch, so one this process was itself started with must not reach the child.
    command.env_remove("DESKTOP_STARTUP_ID").env_remove(TOKEN_ENV);
    if let Some(token) = token {
        command.env(TOKEN_ENV, token);
    }
    // A private copy's path is what an earlier window chose for its own editor; the new window chooses again, so it
    // takes a newer copy and does not die on a removed one. A path the user named stays theirs.
    let env = std::env::var_os;
    let private = env(NEOVIM_BIN_ENV)
        .is_some_and(|bin| nvim_bin::is_private_copy(&bin, env("XDG_DATA_HOME").as_deref(), env("HOME").as_deref()));
    if private {
        command.env_remove(NEOVIM_BIN_ENV);
    }
    // A group of its own keeps a signal to this process's group off the child; its own relay and log replace stdio.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    // A window started from a `setsid nohup` launch ignores SIGHUP, SIGINT and SIGQUIT, and an ignored signal stays
    // ignored across exec: the child starts with every disposition the default and no signal blocked.
    // SAFETY: between fork and exec the closure calls only `sigemptyset`, `sigprocmask` and `signal`, which are
    // async-signal-safe, and touches no memory of the parent's but its own stack.
    unsafe {
        command.pre_exec(|| {
            reset_signals();
            Ok(())
        });
    }
    command
}

/// Every signal's disposition back to the default and an empty signal mask, for a child about to exec. A number the
/// C library keeps for itself, or one that cannot be caught, is refused by `signal` and left as it is.
fn reset_signals() {
    // SAFETY: a zeroed `sigset_t` is a valid buffer for `sigemptyset` to fill; the calls take no other memory.
    unsafe {
        let mut none: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut none);
        libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
        for sig in 1..=64 {
            if sig != libc::SIGKILL && sig != libc::SIGSTOP {
                libc::signal(sig, libc::SIG_DFL);
            }
        }
    }
}

/// Starts `exe` detached from this process. It blocks on the spawn, so it runs off the GTK thread. The spawn happens
/// on the thread that reaps the child: with no such thread there is no child, so no start can leave a zombie.
pub(crate) fn start(exe: &Path, flags: &[OsString], how: How, token: Option<&str>) -> io::Result<Started> {
    let mut command = command(exe, flags, how, token);
    let (spawned, spawn_result) = mpsc::channel();
    let (exited, ended) = mpsc::channel();
    std::thread::Builder::new()
        .name("project-child".into())
        .spawn(move || {
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(error) => {
                    let _ = spawned.send(Err(error));
                    return;
                }
            };
            let _ = spawned.send(Ok(child.id()));
            if let Ok(status) = child.wait() {
                let _ = exited.send(status.code());
            }
        })?;
    let pid = spawn_result
        .recv()
        .map_err(|_| io::Error::other("the start's thread ended before it spawned"))??;
    Ok(Started { pid, ended })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_is_not_there_when_it_its_link_or_its_parent_is_missing() {
        let scratch = Scratch::new();
        let folder = scratch.0.join("app");
        std::fs::create_dir(&folder).unwrap();
        let file = scratch.0.join("file");
        std::fs::write(&file, b"").unwrap();
        let dangling = scratch.0.join("link");
        std::os::unix::fs::symlink(scratch.0.join("away"), &dangling).unwrap();
        assert!(!not_there(&folder));
        assert!(!not_there(&file), "a file is there; the child says what it makes of it");
        assert!(not_there(&scratch.0.join("away")));
        assert!(not_there(&dangling));
        assert!(not_there(&file.join("app")));
    }

    /// How long any one step may take on a loaded machine; a test that passes never waits it out.
    const PATIENCE: Duration = Duration::from_secs(30);
    /// Names the stub the helper process starts; unset, the helper does nothing.
    const HELPER_STUB: &str = "EITRI_TEST_PROJECT_START_STUB";

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Scratch {
            let name = format!("eitri-project-start-{}", uuid::Uuid::new_v4().simple());
            let dir = std::env::temp_dir().join(name);
            std::fs::create_dir(&dir).unwrap();
            Scratch(dir)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }

        /// A shell script, run as `/bin/sh <script> ...`: no executable of a test's is ever written and exec'd, which
        /// a concurrent test's fork can refuse with ETXTBSY.
        fn stub(&self, body: &str) -> OsString {
            let path = self.path("stub.sh");
            std::fs::write(&path, body).unwrap();
            path.into_os_string()
        }

        /// A line that waits, bounded, for `name` to appear: a stub left behind by a failed test still ends.
        fn wait_for(&self, name: &str) -> String {
            format!(
                "i=0; while [ ! -e '{}' ] && [ $i -lt 2000 ]; do sleep 0.02; i=$((i+1)); done\n",
                self.path(name).display()
            )
        }

        fn lines(&self, name: &str) -> Vec<String> {
            let text = std::fs::read_to_string(self.path(name)).unwrap();
            text.lines().map(str::to_owned).collect()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sh(stub: &OsString, how: How, token: Option<&str>) -> Started {
        start(Path::new("/bin/sh"), std::slice::from_ref(stub), how, token).expect("the shell starts")
    }

    fn plain() -> How {
        How::Plain("/work/app".into())
    }

    #[test]
    fn the_exe_drops_deleted_when_the_path_exists_and_falls_back_to_proc_self_exe() {
        let only = |real: &'static str| move |path: &Path| path == Path::new(real);
        let proc_exe = PathBuf::from("/proc/self/exe");
        assert_eq!(
            exe_to_start(Ok("/usr/bin/shell (deleted)".into()), &only("/usr/bin/shell")),
            PathBuf::from("/usr/bin/shell"),
            "after an update the path names the new build"
        );
        assert_eq!(
            exe_to_start(Ok("/usr/bin/shell".into()), &only("/usr/bin/shell")),
            PathBuf::from("/usr/bin/shell")
        );
        assert_eq!(
            exe_to_start(Ok("/opt/a (deleted)".into()), &only("/opt/a (deleted)")),
            PathBuf::from("/opt/a (deleted)"),
            "a binary really named so is that binary"
        );
        assert_eq!(
            exe_to_start(Ok("/usr/bin/shell (deleted)".into()), &|_| false),
            proc_exe
        );
        assert_eq!(exe_to_start(Ok("/usr/bin/shell".into()), &|_| false), proc_exe);
        assert_eq!(exe_to_start(Err(io::Error::other("no /proc")), &|_| true), proc_exe);
    }

    #[test]
    fn the_command_is_flags_raise_dashes_path_and_plain_has_no_raise() {
        let scratch = Scratch::new();
        let out = scratch.path("args");
        let stub = scratch.stub(&format!("printf '%s\\n' \"$@\" > '{}'\n", out.display()));
        let flags = [stub.clone(), "--clean".into(), "--legacy".into()];
        let name = "/work/a b\u{4e2d}<b>";

        let raised = start(Path::new("/bin/sh"), &flags, How::Raise(name.into()), None).unwrap();
        assert_eq!(raised.ended.recv_timeout(PATIENCE), Ok(Some(0)));
        assert_eq!(scratch.lines("args"), ["--clean", "--legacy", "--raise", "--", name]);

        let plain = start(Path::new("/bin/sh"), &flags, How::Plain(name.into()), None).unwrap();
        assert_eq!(plain.ended.recv_timeout(PATIENCE), Ok(Some(0)));
        assert_eq!(scratch.lines("args"), ["--clean", "--legacy", "--", name]);
    }

    #[test]
    fn the_child_gets_the_token_and_null_stdio_and_a_group_of_its_own() {
        let scratch = Scratch::new();
        let stub = scratch.stub(&format!(
            "fds=$(readlink /proc/$$/fd/0 /proc/$$/fd/1 /proc/$$/fd/2)\n\
             group=$(cut -d' ' -f5 /proc/$$/stat)\n\
             printf '%s\\n' \"$XDG_ACTIVATION_TOKEN\" $fds \"$group\" $$ > '{}'\n",
            scratch.path("seen").display()
        ));
        let started = sh(&stub, plain(), Some("the-token"));
        assert_eq!(started.ended.recv_timeout(PATIENCE), Ok(Some(0)));
        let seen = scratch.lines("seen");
        assert_eq!(
            seen[..4],
            ["the-token", "/dev/null", "/dev/null", "/dev/null"],
            "{seen:?}"
        );
        assert_eq!(seen[4], seen[5], "the child leads a process group of its own: {seen:?}");
    }

    #[test]
    fn a_token_only_ever_comes_from_this_start() {
        let env = |token| {
            let command = command(Path::new("/bin/sh"), &[], plain(), token);
            let env: Vec<_> = command
                .get_envs()
                .map(|(name, value)| (name.to_os_string(), value.map(OsStr::to_os_string)))
                .collect();
            env
        };
        let name = |text: &str| OsString::from(text);
        assert_eq!(
            env(None),
            [(name("DESKTOP_STARTUP_ID"), None), (name("XDG_ACTIVATION_TOKEN"), None)],
            "whatever the starter's own launch left behind is spent"
        );
        assert_eq!(
            env(Some("fresh")),
            [
                (name("DESKTOP_STARTUP_ID"), None),
                (name("XDG_ACTIVATION_TOKEN"), Some(name("fresh")))
            ]
        );
    }

    #[test]
    fn a_nonzero_exit_within_the_window_reports_and_a_later_one_does_not() {
        assert_eq!(FAILED_WITHIN, Duration::from_secs(5));
        let scratch = Scratch::new();
        let failing = scratch.stub("exit 7\n");
        assert_eq!(sh(&failing, plain(), None).failed_within(PATIENCE), Some(Some(7)));
        let fine = scratch.stub("exit 0\n");
        assert_eq!(
            sh(&fine, plain(), None).failed_within(PATIENCE),
            None,
            "a raise ends so"
        );
        let killed = scratch.stub("kill -KILL $$\n");
        assert_eq!(
            sh(&killed, plain(), None).failed_within(PATIENCE),
            Some(None),
            "a death by signal is no clean exit"
        );

        let late = scratch.stub(&format!("{}exit 7\n", scratch.wait_for("release")));
        let started = sh(&late, plain(), None);
        assert_eq!(started.failed_within(Duration::from_millis(20)), None, "still running");
        std::fs::write(scratch.path("release"), "").unwrap();
        assert_eq!(
            started.ended.recv_timeout(PATIENCE),
            Ok(Some(7)),
            "the same exit the window passed over, and it is still read, so the child leaves no zombie"
        );
    }

    #[test]
    fn a_failed_spawn_reports_at_once() {
        let scratch = Scratch::new();
        let missing = scratch.path("no-such-binary");
        let start = start(&missing, &[], How::Raise("/work/app".into()), None);
        assert!(start.is_err());
        // A window this wide would hang the test if the failure were waited for rather than known.
        assert_eq!(failure(start, Duration::from_secs(3600)), Some(None));
    }

    #[test]
    fn an_exit_nobody_could_read_is_not_a_failure() {
        let (sender, ended) = mpsc::channel();
        drop(sender);
        assert_eq!(Started { pid: 0, ended }.failed_within(PATIENCE), None);
    }

    /// Not a test of its own: [`the_child_outlives_the_starter`] runs this binary with only this test, as the process
    /// that does the starting and is then killed.
    #[test]
    fn a_helper_that_starts_a_stub_and_stays() {
        let Some(stub) = std::env::var_os(HELPER_STUB) else {
            return;
        };
        let started = sh(&stub, plain(), None);
        let _ = started.ended.recv_timeout(PATIENCE);
    }

    /// What a child started by a process with `neovim_bin` in its environment sees there, the starter being the helper
    /// so that the variable is set before anything in it runs.
    fn neovim_bin_the_child_sees(neovim_bin: &str, data_home: &Path) -> String {
        let scratch = Scratch::new();
        let seen = scratch.path("seen");
        let stub = scratch.stub(&format!(
            "printf '%s\\n' \"${{NEOVIM_BIN-unset}}\" > '{0}.part'\nmv '{0}.part' '{0}'\n",
            seen.display()
        ));
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "project_start::tests::a_helper_that_starts_a_stub_and_stays",
                "--exact",
                "--nocapture",
            ])
            .env(HELPER_STUB, &stub)
            .env("NEOVIM_BIN", neovim_bin)
            .env("XDG_DATA_HOME", data_home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        scratch.lines("seen").remove(0)
    }

    /// A starter launched as `setsid nohup` ignores `SIGHUP`, `SIGINT` and `SIGQUIT`, and a starter may block signals;
    /// a window it starts must not inherit either, or a later Ctrl+C or end signal would not reach it.
    #[test]
    fn a_started_child_has_default_signals() {
        let scratch = Scratch::new();
        let stub = scratch.stub(&format!(
            // A command's own status, not the shell's: bash ignores SIGQUIT in itself and gives its commands what
            // it was started with.
            "grep -E '^Sig(Ign|Blk):' /proc/self/status > '{0}.part'\nmv '{0}.part' '{0}'\n",
            scratch.path("seen").display()
        ));
        let mut helper = Command::new(std::env::current_exe().unwrap());
        helper
            .args([
                "project_start::tests::a_helper_that_starts_a_stub_and_stays",
                "--exact",
                "--nocapture",
            ])
            .env(HELPER_STUB, &stub)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: between fork and exec only async-signal-safe calls: signal, sigemptyset, sigaddset, sigprocmask.
        unsafe {
            helper.pre_exec(|| {
                for sig in [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT] {
                    libc::signal(sig, libc::SIG_IGN);
                }
                let mut blocked: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut blocked);
                libc::sigaddset(&mut blocked, libc::SIGUSR1);
                libc::sigprocmask(libc::SIG_BLOCK, &blocked, std::ptr::null_mut());
                Ok(())
            });
        }
        assert!(helper.status().unwrap().success());
        let mask = |name: &str| -> u64 {
            let line = scratch
                .lines("seen")
                .into_iter()
                .find(|line| line.starts_with(name))
                .unwrap_or_else(|| panic!("no {name} line"));
            u64::from_str_radix(line.split_whitespace().nth(1).unwrap(), 16).unwrap()
        };
        let bit = |sig: libc::c_int| 1u64 << (sig - 1);
        let ignored = mask("SigIgn:");
        for sig in [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT] {
            assert_eq!(
                ignored & bit(sig),
                0,
                "signal {sig} is ignored in the child: {ignored:#x}"
            );
        }
        assert_eq!(mask("SigBlk:"), 0, "the child starts with no signal blocked");
    }

    #[test]
    fn a_new_window_chooses_its_own_nvim_unless_the_user_named_one() {
        let data = Path::new("/data");
        assert_eq!(
            neovim_bin_the_child_sees("/data/eitri/nvim/0.11.4/bin/nvim", data),
            "unset",
            "a copy an earlier window settled on must not pin this one to it"
        );
        assert_eq!(
            neovim_bin_the_child_sees("/opt/nvim/bin/nvim", data),
            "/opt/nvim/bin/nvim",
            "the user's own choice carries over"
        );
    }

    #[test]
    fn the_child_outlives_the_starter() {
        let scratch = Scratch::new();
        let stub = scratch.stub(&format!(
            "echo $$ > '{}.part'\nmv '{0}.part' '{0}'\n{}echo done > '{}'\n",
            scratch.path("pid").display(),
            scratch.wait_for("release"),
            scratch.path("finished").display()
        ));
        let mut helper = Command::new(std::env::current_exe().unwrap())
            .args([
                "project_start::tests::a_helper_that_starts_a_stub_and_stays",
                "--exact",
                "--nocapture",
            ])
            .env(HELPER_STUB, &stub)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let group = helper.id() as libc::pid_t;

        let until = std::time::Instant::now() + PATIENCE;
        while !scratch.path("pid").exists() {
            assert!(std::time::Instant::now() < until, "the helper never started the stub");
            std::thread::sleep(Duration::from_millis(10));
        }
        let child: libc::pid_t = scratch.lines("pid")[0].parse().unwrap();

        // The starter's whole group goes, as a closed terminal or a crashed window would take it.
        assert_eq!(unsafe { libc::kill(-group, libc::SIGKILL) }, 0);
        helper.wait().unwrap();

        std::fs::write(scratch.path("release"), "").unwrap();
        let until = std::time::Instant::now() + PATIENCE;
        while !scratch.path("finished").exists() && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !scratch.path("finished").exists() {
            unsafe { libc::kill(child, libc::SIGKILL) };
            panic!("the child did not go on past its starter");
        }
    }
}
