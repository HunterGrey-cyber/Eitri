#![allow(dead_code)]

#[path = "../src/audit.rs"]
mod audit;
#[path = "../src/daemon.rs"]
mod daemon;
#[path = "../src/peer.rs"]
mod peer;
#[path = "../src/run_dir.rs"]
mod run_dir;

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use daemon::exec::{self, Ended, Runs};
use daemon::login_env::Environment;
use eitri_server_proto::*;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Status;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REVISION: &str = env!("EITRI_BUILD_COMMIT");
fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}

/// Programs on the test's login `PATH`; each leaves marker files in the scratch directory.
const PROGRAMS: &[(&str, &str)] = &[
    ("envdump", "#!/bin/sh\npwd\nprintf '%s\\n' \"$@\"\n/usr/bin/env\n"),
    ("fmt", "#!/bin/sh\nexec cat\n"),
    (
        "hold",
        "#!/bin/sh\nhere=\"$(dirname \"$0\")/..\"\nsleep 30 &\necho $! > \"$here/grandchild\"\necho $$ > \"$here/pid\"\n\
         cat > \"$here/got\"\n: > \"$here/acted\"\n",
    ),
    ("mark", "#!/bin/sh\n: > \"$(dirname \"$0\")/../started\"\n"),
];

struct Scratch {
    base: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let base = std::env::temp_dir().join(format!(
            "exec-command-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&base);
        agent::private_fs::create_private_dir_all(&base, &base).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        for dir in ["home", "bin", "elsewhere", "state", "work"] {
            std::fs::create_dir(base.join(dir)).unwrap();
        }
        let scratch = Self { base };
        for (name, body) in PROGRAMS {
            scratch.program(&format!("bin/{name}"), body);
        }
        scratch
    }

    fn program(&self, rel: &str, body: &str) {
        let path = self.path(rel);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.base.join(rel)
    }

    fn work(&self) -> PathBuf {
        self.path("work")
    }

    /// The login environment the daemon captured: the scratch's programs first on `PATH`, a relative entry that must
    /// never be searched, and what a login profile might export.
    fn login(&self) -> Environment {
        let mut path = OsString::from("relative-bin:");
        path.push(self.path("bin"));
        path.push(":/usr/bin:/bin");
        Environment::from([
            ("HOME".into(), self.path("home").into_os_string()),
            ("PATH".into(), path),
            ("LANG".into(), "C.UTF-8".into()),
            ("FROM_LOGIN_PROFILE".into(), "yes".into()),
            ("TERM".into(), "xterm-256color".into()),
            ("GIT_EDITOR".into(), "vi".into()),
            ("GIT_TERMINAL_PROMPT".into(), "1".into()),
            ("SSH_ASKPASS".into(), "/usr/bin/false".into()),
        ])
    }

    fn audit(&self) -> String {
        std::fs::read_to_string(self.path("state/audit.log")).unwrap_or_default()
    }

    async fn pid(&self, name: &str) -> i32 {
        let path = self.path(name);
        until(|| std::fs::read_to_string(&path).is_ok_and(|text| text.ends_with('\n'))).await;
        std::fs::read_to_string(path).unwrap().trim().parse().unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Waits for `done`; the deadline only guards against a hang.
async fn until(mut done: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(tokio::time::Instant::now() < deadline, "hang guard");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Whether `pid` has ended (gone, or a zombie nobody has reaped yet).
fn ended(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat.rsplit(')').next().unwrap().split_whitespace().next() == Some("Z"),
        Err(_) => true,
    }
}

fn start(cwd: &Path, argv: &[&str]) -> ExecStart {
    ExecStart {
        cwd: cwd.to_str().unwrap().into(),
        argv: argv.iter().map(|arg| arg.to_string()).collect(),
        purpose: ExecPurpose::Command as i32,
    }
}

fn stdin(bytes: &[u8]) -> Result<ExecInput, Status> {
    Ok(ExecInput {
        input: Some(exec_input::Input::Stdin(bytes.to_vec())),
    })
}

fn close() -> Result<ExecInput, Status> {
    Ok(ExecInput {
        input: Some(exec_input::Input::Close(true)),
    })
}

struct Running {
    input: Option<mpsc::Sender<Result<ExecInput, Status>>>,
    output: Option<mpsc::Receiver<Result<ExecOutput, Status>>>,
    ended: tokio::task::JoinHandle<Ended>,
}

/// Starts `argv` the way the daemon does: the purpose decides the rules, then the run.
fn launch_with(scratch: &Scratch, argv: &[&str], runs: &Arc<Runs>) -> Running {
    let prepared = exec::prepare(&start(&scratch.work(), argv), &scratch.login()).unwrap();
    let (input, receiver) = mpsc::channel(4);
    let (output, ended) = exec::run(
        prepared,
        ReceiverStream::new(receiver),
        scratch.path("state"),
        1234,
        runs.clone(),
    );
    Running {
        input: Some(input),
        output: Some(output),
        ended,
    }
}

fn launch(scratch: &Scratch, argv: &[&str]) -> Running {
    launch_with(scratch, argv, &Arc::default())
}

async fn collect(output: &mut mpsc::Receiver<Result<ExecOutput, Status>>) -> (Vec<u8>, Vec<u8>, ExecExit) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut exit = None;
    while let Some(message) = output.recv().await {
        assert!(exit.is_none(), "nothing comes after the exit");
        match message.unwrap().output.unwrap() {
            exec_output::Output::Stdout(bytes) => out.extend(bytes),
            exec_output::Output::Stderr(bytes) => err.extend(bytes),
            exec_output::Output::Exit(status) => exit = Some(status),
        }
    }
    (out, err, exit.expect("an exit comes last"))
}

fn refusal(scratch: &Scratch, start: &ExecStart) -> String {
    exec::prepare(start, &scratch.login()).unwrap_err()
}

#[tokio::test]
async fn a_command_runs_in_its_directory_with_the_login_path() {
    let scratch = Scratch::new("env");
    let absolute = scratch.path("bin/envdump");
    for argv in [
        vec!["envdump", "one", "two"],
        vec![absolute.to_str().unwrap(), "one", "two"],
    ] {
        let mut running = launch(&scratch, &argv);
        running.input.take().unwrap().send(close()).await.unwrap();
        let (out, _, exit) = collect(running.output.as_mut().unwrap()).await;
        assert_eq!((exit.code, exit.signal, exit.refusal.as_str()), (0, 0, ""), "{argv:?}");
        let out = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], scratch.work().to_str().unwrap());
        assert_eq!(&lines[1..3], ["one", "two"]);
        let login_path = format!("PATH={}", scratch.login()[&OsString::from("PATH")].to_str().unwrap());
        for wanted in [
            login_path.as_str(),
            "TERM=dumb",
            "GIT_EDITOR=false",
            "GIT_TERMINAL_PROMPT=0",
            "SSH_ASKPASS_REQUIRE=never",
            "FROM_LOGIN_PROFILE=yes",
        ] {
            assert!(lines.contains(&wanted), "{wanted} missing from {out}");
        }
        assert!(!lines.iter().any(|line| line.starts_with("SSH_ASKPASS=")), "{out}");
        assert!(
            !out.contains("xterm-256color") && !out.contains("GIT_EDITOR=vi"),
            "{out}"
        );
        // Nothing of the process that started the run reaches the child: only the login environment.
        assert!(!out.contains("CARGO"), "{out}");
    }
}

#[tokio::test]
async fn a_name_not_on_the_login_path_refuses_and_runs_nothing() {
    let scratch = Scratch::new("missing");
    // Present, but not where the login PATH looks; not executable; a directory.
    scratch.program("elsewhere/hidden", "#!/bin/sh\n");
    std::fs::write(scratch.path("bin/plain"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(scratch.path("bin/plain"), std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::create_dir(scratch.path("bin/folder")).unwrap();
    let plain = scratch.path("bin/plain");
    for name in ["no-such-tool", "hidden", "plain", "folder", plain.to_str().unwrap()] {
        let refused = refusal(&scratch, &start(&scratch.work(), &[name, "--version"]));
        assert_eq!(
            refused,
            format!("`{name}` is not on the login PATH of this account"),
            "{name}"
        );
    }
    let folder = scratch.path("bin/folder");
    assert!(
        refusal(&scratch, &start(&scratch.work(), &[folder.to_str().unwrap()])).contains("is not on the login PATH")
    );
}

/// A relative name is refused whatever the working directory holds, and the login `PATH`'s relative entry is never
/// searched. That only shows when the process's own directory holds the files: this runs [`relative_names_child`] in a
/// child whose directory is the scratch's `work`.
#[test]
fn a_relative_name_is_refused_even_where_the_process_directory_holds_it() {
    let scratch = Scratch::new("relative");
    scratch.program("work/fmt", "#!/bin/sh\n");
    std::fs::create_dir(scratch.path("work/bin")).unwrap();
    scratch.program("work/bin/fmt", "#!/bin/sh\n");
    std::fs::create_dir(scratch.path("work/relative-bin")).unwrap();
    scratch.program("work/relative-bin/nearby", "#!/bin/sh\n");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "relative_names_child", "--nocapture", "--test-threads=1"])
        .env("EITRI_TEST_RELATIVE_BASE", &scratch.base)
        .current_dir(scratch.work())
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{text}");
    assert!(text.contains("1 passed"), "the child test ran: {text}");
}

/// The child half of [`a_relative_name_is_refused_even_where_the_process_directory_holds_it`].
#[test]
fn relative_names_child() {
    let Some(base) = std::env::var_os("EITRI_TEST_RELATIVE_BASE") else {
        return;
    };
    // The parent owns the directory: this handle must not remove it.
    let scratch = std::mem::ManuallyDrop::new(Scratch {
        base: PathBuf::from(base),
    });
    assert_eq!(std::env::current_dir().unwrap(), scratch.work());
    for name in [
        "nearby",
        "./fmt",
        "bin/fmt",
        "../bin/fmt",
        "relative-bin/nearby",
        ".",
        "..",
    ] {
        let refused = refusal(&scratch, &start(&scratch.work(), &[name]));
        assert_eq!(
            refused,
            format!("`{name}` is not on the login PATH of this account"),
            "{name}"
        );
    }
}

#[test]
fn a_relative_or_dotted_argv0_refuses() {
    let scratch = Scratch::new("dotted");
    // A program of that name in the working directory changes nothing.
    scratch.program("work/fmt", "#!/bin/sh\n");
    let not_normal = format!("{}/../bin/fmt", scratch.path("bin").display());
    for name in ["./fmt", "../bin/fmt", "bin/fmt", ".", "..", "", not_normal.as_str()] {
        let refused = refusal(&scratch, &start(&scratch.work(), &[name]));
        assert_eq!(
            refused,
            format!("`{name}` is not on the login PATH of this account"),
            "{name:?}"
        );
    }
    assert_eq!(refusal(&scratch, &start(&scratch.work(), &[])), "no command was given");
    let file = scratch.path("bin/fmt");
    for (cwd, why) in [
        ("relative/dir".to_owned(), "absolute and in normal form"),
        (
            format!("{}/../work", scratch.path("bin").display()),
            "absolute and in normal form",
        ),
        (
            format!("{}//work", scratch.base.display()),
            "absolute and in normal form",
        ),
        (
            scratch.path("missing").display().to_string(),
            "is not a directory on this server",
        ),
        (file.display().to_string(), "is not a directory on this server"),
    ] {
        let mut command = start(&scratch.work(), &["fmt"]);
        command.cwd = cwd.clone();
        let refused = refusal(&scratch, &command);
        assert!(refused.contains(why), "{cwd}: {refused}");
    }
    let mut nul = start(&scratch.work(), &["fmt", "a\0b"]);
    assert!(refusal(&scratch, &nul).contains("NUL"));
    nul.argv = vec!["fmt".into()];
    nul.purpose = ExecPurpose::Unspecified as i32;
    assert!(
        refusal(&scratch, &nul).contains("purpose"),
        "{}",
        refusal(&scratch, &nul)
    );
}

#[tokio::test]
async fn the_command_audit_line_names_only_the_program() {
    let scratch = Scratch::new("audit");
    let absolute = scratch.path("bin/fmt");
    let mut running = launch(
        &scratch,
        &[
            absolute.to_str().unwrap(),
            "--token=hunter2-secret",
            "src/private name.rs",
        ],
    );
    running.input.take().unwrap().send(close()).await.unwrap();
    let (_, _, exit) = collect(running.output.as_mut().unwrap()).await;
    assert_eq!(exit.code, 0);
    assert_eq!(running.ended.await.unwrap(), Ended::Exited(exit));
    let audit = scratch.audit();
    let line = format!("command fmt cwd={} started_ms=1234", scratch.work().display());
    assert!(audit.contains(&format!("exec-start {line}\n")), "{audit}");
    assert!(audit.contains(&format!("exec {line} exit=0\n")), "{audit}");
    for secret in [
        "hunter2",
        "--token",
        "private name",
        absolute.parent().unwrap().to_str().unwrap(),
    ] {
        assert!(!audit.contains(secret), "{secret} in {audit}");
    }
}

#[tokio::test]
async fn stdin_to_stdout_round_trips_a_formatter() {
    let scratch = Scratch::new("formatter");
    let mut running = launch(&scratch, &["cat"]);
    let payload: Vec<u8> = (0..512 * 1024u32).map(|i| (i % 251) as u8).collect();
    let sent = payload.clone();
    let input = running.input.take().unwrap();
    let feeder = tokio::spawn(async move {
        for chunk in sent.chunks(64 * 1024) {
            input.send(stdin(chunk)).await.unwrap();
        }
        input.send(close()).await.unwrap();
    });
    let (out, err, exit) = tokio::time::timeout(Duration::from_secs(120), collect(running.output.as_mut().unwrap()))
        .await
        .expect("the run finished");
    feeder.await.unwrap();
    assert_eq!((exit.code, exit.refusal.as_str()), (0, ""));
    assert!(err.is_empty());
    assert!(out == payload);
}

#[tokio::test]
async fn drop_before_close_kills_the_group_for_commands_too() {
    let scratch = Scratch::new("drop");
    let mut running = launch(&scratch, &["hold"]);
    let input = running.input.take().unwrap();
    input.send(stdin(b"the first half of the file\n")).await.unwrap();
    let pid = scratch.pid("pid").await;
    let grandchild = scratch.pid("grandchild").await;
    until(|| std::fs::read(scratch.path("got")).is_ok_and(|got| !got.is_empty())).await;
    drop(input);
    drop(running.output.take());
    assert_eq!(running.ended.await.unwrap(), Ended::KilledBeforeClose);
    until(|| ended(pid) && ended(grandchild)).await;
    assert!(
        !scratch.path("acted").exists(),
        "the command acted on an input it never had whole"
    );
    let audit = scratch.audit();
    assert!(audit.contains("exec command hold cwd="), "{audit}");
    assert!(audit.contains("killed=input-unfinished"), "{audit}");
}

#[tokio::test]
async fn a_stopping_daemon_refuses_a_command_with_nothing_run() {
    let scratch = Scratch::new("stopping");
    let runs: Arc<Runs> = Arc::default();
    runs.kill_unclosed();
    let mut running = launch_with(&scratch, &["mark"], &runs);
    running.input.take().unwrap().send(close()).await.unwrap();
    let (out, _, exit) = collect(running.output.as_mut().unwrap()).await;
    assert!(out.is_empty());
    assert!(exit.refusal.contains("nothing was run"), "{exit:?}");
    assert!(exit.refusal.contains("command mark"), "{exit:?}");
    assert_eq!(running.ended.await.unwrap(), Ended::NotStarted);
    assert!(!scratch.path("started").exists());
    assert!(scratch.audit().contains("exec command mark cwd="));
    assert!(scratch.audit().contains("not-started"));
}

/// The daemon `core/tests/remote_exec.rs` drives through `LocalExec`: this build's daemon, from a fixed login
/// environment, in the directories the environment names.
#[test]
fn fixture_daemon_entry() {
    if std::env::var_os("EITRI_TEST_DAEMON").is_none() {
        return;
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let home = std::env::var_os("HOME").unwrap();
        let mut paths = run_dir::Paths::from_env(VERSION).unwrap();
        paths.install = Some(paths.data.join(format!("{VERSION}-{}", run_dir::rev7(REVISION))));
        daemon::run_with_environment(
            paths,
            Duration::from_secs(3600),
            daemon::OwnedSidecars::default(),
            async move {
                Ok(daemon::login_env::Captured {
                    env: Environment::from([
                        ("HOME".into(), home),
                        ("PATH".into(), "/usr/bin:/bin".into()),
                        ("LANG".into(), "C.UTF-8".into()),
                    ]),
                    fallback: None,
                })
            },
        )
        .await
        .unwrap();
    });
}
