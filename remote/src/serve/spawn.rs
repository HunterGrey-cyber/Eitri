//! A window's side of `serve`: whether one runs, what the desk said yes to, and the process started.
//!
//! [`plan`] reads the user's lock and the confirmed listen address and hands them to
//! `eitri_core::remote_runtime::decide`; [`ServeSpawner`] starts `<exe> remote serve <args>` in a process group of its
//! own (a terminal's Ctrl+C to the window does not reach it), with no stdin or stdout and the window's stderr, and
//! reaps it on a thread of its own, saying why it ended when that was not a stop.
//!
//! `serve` faces the network, so it is given only what it needs: of the window's environment the directories it works
//! in, `PATH` and the locale ([`serve_env`]), never the API keys and tokens a desk session holds; and no descriptor
//! but its stdin, stdout and stderr (every other one is close-on-exec in the child before `exec`), since its sandbox
//! governs the files it opens, not the ones it was handed.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use eitri_core::lua::config::{RemoteListen, RemoteSettings};
use eitri_core::remote_runtime::{decide, LockState, RemotePlan, ServeConfig, Spawner};

use super::config::Listen;
use super::{is_running, read_record, Dirs, EXIT_RUNNING_DIFFERENT, EXIT_RUNNING_SAME};

/// Whether a `serve` holds the user's lock now. A lock that cannot be looked at reads as free: the `serve` started then
/// takes the lock or, finding it held, exits at once.
pub fn lock_state(dirs: &Dirs) -> LockState {
    match is_running(dirs) {
        Ok(true) => LockState::Held,
        Ok(false) => LockState::Free,
        Err(e) => {
            eprintln!("[remote] could not look at serve's lock: {e}");
            LockState::Free
        }
    }
}

/// What a window with `settings` does now, read from `dirs`. Reads files: never on the GTK thread.
pub fn plan(settings: &RemoteSettings, dirs: &Dirs) -> RemotePlan {
    let confirmed = eitri_core::remote_pairing::confirmed_listen_now(&dirs.state)
        .ok()
        .flatten();
    decide(settings, &lock_state(dirs), confirmed.as_deref())
}

/// Whether `serve` is given the variable `key` from the window's environment: the directories it works in (`HOME`, the
/// XDG ones, `TMPDIR`), `PATH` and the locale.
fn passed_to_serve(key: &OsStr) -> bool {
    let Some(key) = key.to_str() else {
        return false;
    };
    let hooks = cfg!(feature = "remote-test-hooks") && matches!(key, super::IDLE_TEST_VAR | JOURNAL_SOCKET_VAR);
    hooks
        || matches!(key, "HOME" | "PATH" | "TMPDIR" | "LANG" | "LANGUAGE")
        || key.starts_with("XDG_")
        || key.starts_with("LC_")
}

/// A test build's journal socket, handed on to the `serve` a window starts in a test.
const JOURNAL_SOCKET_VAR: &str = eitri_core::remote_audit::JOURNAL_SOCKET_VAR;

/// The part of `vars` (a window's environment) `serve` is started with.
pub fn serve_env(vars: impl IntoIterator<Item = (OsString, OsString)>) -> Vec<(OsString, OsString)> {
    vars.into_iter().filter(|(key, _)| passed_to_serve(key)).collect()
}

/// The configuration the running `serve` holds, from its record, in the window's terms (its port settled): what a
/// window that finds `serve` running pairs phones with, whatever its own `remote.*` keys say. `None` when no record is
/// whole (no `serve`, or one still starting) or the record is another build's (`serve::other_build` says so).
pub fn running_config(dirs: &Dirs) -> Option<ServeConfig> {
    let (_, running) = read_record(dirs)?;
    Some(ServeConfig {
        listen: match running.listen {
            Listen::Loopback => RemoteListen::Loopback,
            Listen::Socket => RemoteListen::Socket,
            Listen::Address { addr, .. } => RemoteListen::Address(addr),
        },
        port: Some(running.port),
        origins: running.origins,
        audit_keep_days: running.audit_keep_days,
        notify: running.notify,
    })
}

/// How a `serve` a window started ended, as its reaper saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServeEnded {
    /// Exit 0: no window had remote control on.
    Idle,
    /// A `serve` with the same configuration already ran.
    RunningSame,
    /// A `serve` with another configuration already runs; this one's stderr named each difference.
    RunningDifferent,
    /// Any other exit code: it could not start, its configuration was refused, or it failed later, and said why on
    /// the window's stderr.
    Failed(i32),
    /// Ended by a signal: `eitri remote stop`'s `SIGTERM`, or one sent by hand.
    Signalled,
    /// Waiting for it failed, so how it ended is unknown.
    Unknown,
}

impl ServeEnded {
    fn of(status: std::process::ExitStatus) -> ServeEnded {
        match status.code() {
            Some(0) => ServeEnded::Idle,
            Some(EXIT_RUNNING_SAME) => ServeEnded::RunningSame,
            Some(EXIT_RUNNING_DIFFERENT) => ServeEnded::RunningDifferent,
            Some(code) => ServeEnded::Failed(code),
            None => ServeEnded::Signalled,
        }
    }
}

/// What a window is told when a `serve` it started ends; called on the reaper's thread.
pub type OnServeEnd = std::sync::Arc<dyn Fn(ServeEnded) + Send + Sync>;

/// Starts `serve` as `<exe> remote serve`.
pub struct ServeSpawner {
    exe: PathBuf,
    envs: Vec<(OsString, OsString)>,
    on_end: Option<OnServeEnd>,
}

impl ServeSpawner {
    /// `exe` is the product binary: the running one, for a window.
    pub fn new(exe: PathBuf) -> ServeSpawner {
        ServeSpawner {
            exe,
            envs: Vec::new(),
            on_end: None,
        }
    }

    /// Tells `hook` how each `serve` started through this ended, after the line on stderr.
    pub fn on_end(mut self, hook: OnServeEnd) -> ServeSpawner {
        self.on_end = Some(hook);
        self
    }

    /// Sets `key` in the started process's environment, beside what [`serve_env`] passes on from this process's.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> ServeSpawner {
        self.envs.push((key.into(), value.into()));
        self
    }
}

impl Spawner for ServeSpawner {
    fn spawn_serve(&self, cfg: &ServeConfig) -> io::Result<()> {
        let mut command = Command::new(&self.exe);
        command
            .arg("remote")
            .arg("serve")
            .args(cfg.args())
            .env_clear()
            .envs(serve_env(std::env::vars_os()))
            .envs(self.envs.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .process_group(0);
        // SAFETY: the closure runs in the child between `fork` and `exec` and makes one system call, which allocates
        // nothing and takes no lock.
        unsafe {
            command.pre_exec(|| {
                // Close-on-exec rather than closed: std reports a failed `exec` through its own pipe among these
                // (close-on-exec already), and closing it would turn the failure into a false start. A kernel before
                // 5.11 refuses the flag; the descriptors std itself opens are close-on-exec anyway.
                libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, libc::CLOSE_RANGE_CLOEXEC);
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let pid = child.id();
        let on_end = self.on_end.clone();
        let reaped = std::thread::Builder::new()
            .name("eitri-remote-serve".into())
            .spawn(move || {
                let ended = match child.wait() {
                    Ok(status) => {
                        let ended = ServeEnded::of(status);
                        match ended {
                            ServeEnded::Idle => {
                                eprintln!("[remote] serve (pid {pid}) ended: no window had remote control on")
                            }
                            ServeEnded::RunningSame => {}
                            ServeEnded::RunningDifferent => eprintln!(
                                "[remote] a serve with another configuration already runs (the line above names each \
                                 difference): `eitri remote stop`, then start a window, for this window's"
                            ),
                            // A stop's SIGTERM, or a failure `serve` printed itself.
                            _ => eprintln!("[remote] serve (pid {pid}) ended: {status}"),
                        }
                        ended
                    }
                    Err(e) => {
                        eprintln!("[remote] could not wait for serve (pid {pid}): {e}");
                        ServeEnded::Unknown
                    }
                };
                if let Some(on_end) = on_end {
                    on_end(ended);
                }
            });
        if let Err(e) = reaped {
            eprintln!("[remote] serve (pid {pid}) runs unwatched: {e}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::lua::config::RemoteListen;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::time::{Duration, Instant};

    fn scratch(name: &str) -> PathBuf {
        let dir =
            PathBuf::from(option_env!("CARGO_TARGET_DIR").unwrap_or(concat!(env!("CARGO_MANIFEST_DIR"), "/../target")))
                .join("tmp/remote-spawn-tests")
                .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_running_serve_is_paired_with_as_it_runs() {
        let dir = scratch("running");
        let dirs = Dirs {
            control: dir.join("control"),
            state: dir.join("state"),
        };
        eitri_core::panel_control::ensure_dir(&dirs.control).unwrap();
        assert_eq!(running_config(&dirs), None, "no record");
        let record = super::super::RECORD_FILE;
        for (args, listen) in [
            (r#"["--listen","socket","--port","7400"]"#, RemoteListen::Socket),
            (r#"["--listen","loopback","--port","7500"]"#, RemoteListen::Loopback),
            (
                r#"["--listen","198.51.100.2","--port","7600"]"#,
                RemoteListen::Address("198.51.100.2".parse().unwrap()),
            ),
        ] {
            let path = dirs.control.join(record);
            let _ = std::fs::remove_file(&path);
            let mut file = agent::private_fs::open_private(&path).unwrap();
            std::io::Write::write_all(
                &mut file,
                format!(
                    r#"{{"v":1,"pid":1,"args":{args},"sandbox":"full","build":{}}}"#,
                    serde_json::json!(super::super::config::this_build())
                )
                .as_bytes(),
            )
            .unwrap();
            let cfg = running_config(&dirs).unwrap_or_else(|| panic!("{args} was not read"));
            assert_eq!(cfg.listen, listen, "{args}");
            let port: u16 = args.rsplit('"').nth(1).unwrap().parse().unwrap();
            assert_eq!(cfg.port, Some(port), "{args}");
        }
    }

    #[test]
    fn the_window_is_told_how_a_serve_it_started_ended() {
        let dir = scratch("ended");
        let cfg = eitri_core::remote_runtime::ServeConfig {
            listen: RemoteListen::Loopback,
            port: None,
            origins: Vec::new(),
            audit_keep_days: eitri_core::remote_audit::AUDIT_KEEP_DAYS_DEFAULT,
            notify: true,
        };
        for (body, want) in [
            ("exit 4", ServeEnded::RunningDifferent),
            ("exit 3", ServeEnded::RunningSame),
            ("exit 1", ServeEnded::Failed(1)),
            ("exit 0", ServeEnded::Idle),
            ("kill -TERM $$", ServeEnded::Signalled),
        ] {
            let exe = dir.join("serve-stand-in");
            let _ = std::fs::remove_file(&exe);
            script_fixture::write_executable_script(&exe, format!("#!/bin/sh\n{body}\n"), 0o700).unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            let tx = std::sync::Mutex::new(tx);
            ServeSpawner::new(exe)
                .env("XDG_RUNTIME_DIR", &dir)
                .on_end(std::sync::Arc::new(move |ended| {
                    let _ = tx.lock().unwrap().send(ended);
                }))
                .spawn_serve(&cfg)
                .unwrap();
            assert_eq!(rx.recv_timeout(Duration::from_secs(30)), Ok(want), "{body}");
        }
    }

    #[test]
    fn serve_is_given_its_directories_path_and_locale_only() {
        let vars = [
            ("HOME", "/home/u"),
            ("PATH", "/usr/bin"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("XDG_STATE_HOME", "/home/u/.local/state"),
            ("TMPDIR", "/var/tmp"),
            ("LANG", "en_AU.UTF-8"),
            ("LC_TIME", "C"),
            ("ANTHROPIC_API_KEY", "sk-test"),
            ("GITHUB_TOKEN", "ghp-test"),
            ("SSH_AUTH_SOCK", "/run/user/1000/ssh"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
            ("WAYLAND_DISPLAY", "wayland-0"),
            ("XDGX", "x"),
        ];
        let kept: Vec<String> = serve_env(vars.iter().map(|(k, v)| (OsString::from(k), OsString::from(v))))
            .into_iter()
            .map(|(k, _)| k.into_string().unwrap())
            .collect();
        assert_eq!(
            kept,
            [
                "HOME",
                "PATH",
                "XDG_RUNTIME_DIR",
                "XDG_STATE_HOME",
                "TMPDIR",
                "LANG",
                "LC_TIME"
            ]
        );
    }

    #[test]
    fn serve_starts_without_the_windows_secrets_or_descriptors() {
        let dir = scratch("env-fds");
        let out = dir.join("out");
        let exe = dir.join("serve-stand-in");
        script_fixture::write_executable_script(
            &exe,
            format!(
                "#!/bin/sh\nenv > '{0}.tmp'\nls /proc/self/fd >> '{0}.tmp'\nmv '{0}.tmp' '{0}'\n",
                out.display()
            ),
            0o700,
        )
        .unwrap();
        // A descriptor this process holds without close-on-exec, as a library might leave one. Numbered from 100: the
        // lowest free number is whatever other tests in this process just closed, often 3, which is also the number
        // `ls` below takes for the directory it lists, so a low one would read as inherited when it was not.
        let file = std::fs::File::open(&exe).unwrap();
        // SAFETY: `F_DUPFD` takes a valid descriptor and returns a new one (or -1, checked), owned below.
        let leaked = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 100) };
        assert!(leaked >= 100);
        // SAFETY: `leaked` is a descriptor this test just made and owns alone.
        let leaked = unsafe { OwnedFd::from_raw_fd(leaked) };
        // Cargo gives every test process variables of its own: one of them stands for a secret the window holds.
        let inherited = "CARGO_MANIFEST_DIR";
        assert!(std::env::var_os(inherited).is_some());

        let cfg = eitri_core::remote_runtime::ServeConfig {
            listen: RemoteListen::Loopback,
            port: None,
            origins: Vec::new(),
            audit_keep_days: eitri_core::remote_audit::AUDIT_KEEP_DAYS_DEFAULT,
            notify: true,
        };
        ServeSpawner::new(exe.clone())
            .env("XDG_RUNTIME_DIR", &dir)
            .spawn_serve(&cfg)
            .unwrap();
        let until = Instant::now() + Duration::from_secs(30);
        while !out.exists() {
            assert!(Instant::now() < until, "the stand-in never wrote what it was given");
            std::thread::sleep(Duration::from_millis(20));
        }
        let seen = std::fs::read_to_string(&out).unwrap();
        let (env, fds): (Vec<&str>, Vec<&str>) = seen.lines().partition(|line| line.contains('='));
        assert!(
            !env.iter().any(|line| line.starts_with(&format!("{inherited}="))),
            "serve was handed {inherited}: {env:?}"
        );
        assert!(
            env.contains(&format!("XDG_RUNTIME_DIR={}", dir.display()).as_str()),
            "{env:?}"
        );
        // `ls` holds one descriptor of its own for the directory it lists.
        let fds: Vec<u32> = fds.iter().filter_map(|fd| fd.parse().ok()).collect();
        assert!(
            !fds.contains(&(leaked.as_raw_fd() as u32)),
            "serve inherited descriptor {}: {fds:?}",
            leaked.as_raw_fd()
        );
        assert!(fds.iter().filter(|fd| **fd > 2).count() <= 1, "{fds:?}");
        drop(leaked);
    }
}

#[cfg(test)]
mod script_fixture {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/executable.rs"));
}
