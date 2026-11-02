//! Starting another project's window: a process of its own, which this window neither waits for nor takes down.

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
    pub ended: mpsc::Receiver<Option<i32>>,
}

impl Started {
    /// The exit to report when the child ended badly inside `window`, which blocks for as long as `window` lasts. A
    /// child still running at its end is left to the thread that reaps it.
    pub(crate) fn failed_within(&self, window: Duration) -> Option<Option<i32>> {
        self.ended.recv_timeout(window).ok().filter(|code| *code != Some(0))
    }
}

/// What the child is asked to do with its path.
pub(crate) enum How {
    /// Bring the path's running window forward, or be that window: `--raise -- <path>`.
    Raise(PathBuf),
    /// Be a window of the path, as any launch is: `-- <path>`.
    #[cfg_attr(not(test), allow(dead_code))]
    Plain(PathBuf),
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
    command
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
            let _ = spawned.send(Ok(()));
            if let Ok(status) = child.wait() {
                let _ = exited.send(status.code());
            }
        })?;
    spawn_result
        .recv()
        .map_err(|_| io::Error::other("the start's thread ended before it spawned"))??;
    Ok(Started { ended })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(Started { ended }.failed_within(PATIENCE), None);
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
