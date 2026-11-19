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
use std::path::{Path, PathBuf};
use std::process::Command;
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

/// A fake `git` whose behaviour is chosen by its last argument; it leaves marker files in the scratch directory.
const FAKE_GIT: &str = r#"#!/bin/sh
here="$(dirname "$0")/.."
for last; do :; done
case "$last" in
  env) pwd; printf '%s\n' "$@"; /usr/bin/env ;;
  hold) echo $$ > "$here/pid"; cat > "$here/got"; : > "$here/acted" ;;
  noread) echo $$ > "$here/pid"; sleep 30; : > "$here/acted" ;;
  mark) : > "$here/started" ;;
  cat) exec cat ;;
  quick) exit 3 ;;
  big) head -c 1048576 /dev/zero ;;
  slow) sleep 1; : > "$here/slow-done" ;;
  slower) echo $$ > "$here/slower.pid"; sleep 30; : > "$here/slower-done" ;;
  stamp-*) : > "$here/$last" ;;
esac
"#;

struct Scratch {
    base: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let base = std::env::temp_dir().join(format!(
            "exec-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&base);
        agent::private_fs::create_private_dir_all(&base, &base).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        for dir in ["home", "bin", "state", "repo"] {
            std::fs::create_dir(base.join(dir)).unwrap();
        }
        let git = base.join("bin/git");
        std::fs::write(&git, FAKE_GIT).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self { base }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.base.join(rel)
    }

    /// A login environment of the test's own: its home, the system's tools (the fake git first when asked), and what
    /// a login profile might export.
    fn login(&self, fake: bool) -> Environment {
        let mut path = OsString::new();
        if fake {
            path.push(self.path("bin"));
            path.push(":");
        }
        path.push("/usr/bin:/bin");
        Environment::from([
            ("HOME".into(), self.path("home").into_os_string()),
            ("PATH".into(), path),
            ("LANG".into(), "C.UTF-8".into()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("FROM_LOGIN_PROFILE".into(), "yes".into()),
            ("GIT_ASKPASS".into(), "/usr/bin/false".into()),
            ("SSH_ASKPASS".into(), "/usr/bin/false".into()),
            ("GIT_EDITOR".into(), "vi".into()),
            ("GIT_TERMINAL_PROMPT".into(), "1".into()),
        ])
    }

    fn audit(&self) -> String {
        std::fs::read_to_string(self.path("state/audit.log")).unwrap_or_default()
    }

    /// Runs the system's git in the test repository as the setup, never through the code under test.
    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(self.path("repo"))
            .env_clear()
            .envs(self.login(false))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn repository(&self) {
        self.git(&["init", "-q", "-b", "main"]);
        self.git(&["config", "user.name", "Example"]);
        self.git(&["config", "user.email", "example@example.invalid"]);
        std::fs::write(self.path("repo/file.txt"), "first\n").unwrap();
        self.git(&["add", "file.txt"]);
        self.git(&["commit", "-q", "-m", "first"]);
    }

    /// The pid the fake git wrote to `name`, once it has.
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
        purpose: ExecPurpose::Git as i32,
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

/// A run's input channel (the laptop's side of the stream) and its output and end.
struct Running {
    input: Option<mpsc::Sender<Result<ExecInput, Status>>>,
    output: Option<mpsc::Receiver<Result<ExecOutput, Status>>>,
    ended: tokio::task::JoinHandle<Ended>,
}

fn launch_with(
    scratch: &Scratch,
    argv: &[&str],
    fake: bool,
    runs: &Arc<Runs>,
    adjust: impl FnOnce(&mut exec::Prepared),
) -> Running {
    let mut prepared = exec::git::prepare(&start(&scratch.path("repo"), argv), &scratch.login(fake)).unwrap();
    adjust(&mut prepared);
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
    launch_with(scratch, argv, true, &Arc::default(), |_| {})
}

/// Everything a run sent: stdout, stderr, the exit, and the largest output chunk.
async fn collect(output: &mut mpsc::Receiver<Result<ExecOutput, Status>>) -> (Vec<u8>, Vec<u8>, ExecExit, usize) {
    let (mut out, mut err, mut largest) = (Vec::new(), Vec::new(), 0);
    let mut exit = None;
    while let Some(message) = output.recv().await {
        assert!(exit.is_none(), "nothing comes after the exit");
        match message.unwrap().output.unwrap() {
            exec_output::Output::Stdout(bytes) => {
                largest = largest.max(bytes.len());
                out.extend(bytes);
            }
            exec_output::Output::Stderr(bytes) => {
                largest = largest.max(bytes.len());
                err.extend(bytes);
            }
            exec_output::Output::Exit(status) => exit = Some(status),
        }
    }
    (out, err, exit.expect("an exit comes last"), largest)
}

#[tokio::test]
async fn exec_runs_git_in_the_login_environment_with_the_editor_and_prompt_off() {
    let scratch = Scratch::new("env");
    let mut running = launch(&scratch, &["git", "--no-pager", "env"]);
    running.input.take().unwrap().send(close()).await.unwrap();
    let (out, _, exit, _) = collect(running.output.as_mut().unwrap()).await;
    assert_eq!((exit.code, exit.signal, exit.refusal.as_str()), (0, 0, ""));
    let out = String::from_utf8(out).unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], scratch.path("repo").to_str().unwrap());
    assert_eq!(&lines[1..3], ["--no-pager", "env"]);
    for wanted in [
        "GIT_EDITOR=false",
        "GIT_SEQUENCE_EDITOR=false",
        "GIT_TERMINAL_PROMPT=0",
        "GIT_ASKPASS=",
        "SSH_ASKPASS_REQUIRE=never",
        "GCM_INTERACTIVE=never",
        "FROM_LOGIN_PROFILE=yes",
    ] {
        assert!(lines.contains(&wanted), "{wanted} missing from {out}");
    }
    assert!(!out.contains("GIT_ASKPASS=/"), "{out}");
    assert!(!lines.iter().any(|line| line.starts_with("SSH_ASKPASS=")), "{out}");
    assert!(!out.contains("GIT_EDITOR=vi"), "{out}");
    assert_eq!(running.ended.await.unwrap(), Ended::Exited(exit));
    let audit = scratch.audit();
    let line = format!("git env cwd={} started_ms=1234", scratch.path("repo").display());
    assert!(audit.contains(&format!("exec-start {line}\n")), "{audit}");
    assert!(audit.contains(&format!("exec {line} exit=0\n")), "{audit}");
}

#[test]
fn the_audited_subcommand_is_found_past_gits_global_options() {
    let args = |text: &str| text.split(' ').map(str::to_owned).collect::<Vec<_>>();
    for (line, wanted) in [
        ("commit -m x", Some("commit")),
        ("-C /srv/project -c user.name=x --no-pager commit -m x", Some("commit")),
        (
            "--git-dir=/srv/project/.git --work-tree /srv/project status",
            Some("status"),
        ),
        ("-c core.editor=vi -p log", Some("log")),
        ("--config-env name=VAR -- checkout", Some("checkout")),
        ("--version", None),
    ] {
        assert_eq!(exec::git::subcommand(&args(line)), wanted, "{line}");
    }
}

#[tokio::test]
async fn exec_refuses_a_command_that_is_not_git() {
    let scratch = Scratch::new("refuse");
    let login = scratch.login(false);
    let repo = scratch.path("repo");
    for (argv, why) in [
        (vec!["sh", "-c", "true"], "only git runs here"),
        (vec!["/usr/bin/git", "status"], "only git runs here"),
        (vec!["git ", "status"], "only git runs here"),
        (vec![], "no command was given"),
    ] {
        let refusal = exec::git::prepare(&start(&repo, &argv), &login).unwrap_err();
        assert!(refusal.contains(why), "{argv:?}: {refusal}");
    }
    let mut command = start(&repo, &["git", "status"]);
    command.purpose = ExecPurpose::Command as i32;
    assert!(exec::git::prepare(&command, &login).unwrap_err().contains("only git"));
    for cwd in ["relative/dir", "/srv/../etc", "/srv//project", "/srv/./project"] {
        let mut command = start(&repo, &["git", "status"]);
        command.cwd = cwd.into();
        assert!(
            exec::git::prepare(&command, &login)
                .unwrap_err()
                .contains("normal form"),
            "{cwd}"
        );
    }
    let mut no_git = login.clone();
    no_git.insert("PATH".into(), scratch.path("home").into_os_string());
    assert!(exec::git::prepare(&start(&repo, &["git", "status"]), &no_git)
        .unwrap_err()
        .contains("git is not on the login PATH"));
    let mut refused = exec::refused("only git runs here, not \"sh\"".into());
    let only = refused.recv().await.unwrap().unwrap();
    assert!(matches!(
        only.output,
        Some(exec_output::Output::Exit(ExecExit { ref refusal, .. })) if refusal.contains("only git")
    ));
    assert!(refused.recv().await.is_none());
}

#[tokio::test]
async fn exec_streams_stdin_and_large_stdout_without_deadlock() {
    let scratch = Scratch::new("stream");
    let mut running = launch(&scratch, &["git", "cat"]);
    let payload: Vec<u8> = (0..8 * 1024 * 1024u32).map(|i| (i % 253) as u8).collect();
    let sent = payload.clone();
    let input = running.input.take().unwrap();
    // The whole input goes in while the output comes back: a child that fills its stdout pipe before it has read all
    // of its stdin must not stall the run.
    let feeder = tokio::spawn(async move {
        for chunk in sent.chunks(64 * 1024) {
            input.send(stdin(chunk)).await.unwrap();
        }
        input.send(close()).await.unwrap();
    });
    let (out, err, exit, largest) =
        tokio::time::timeout(Duration::from_secs(120), collect(running.output.as_mut().unwrap()))
            .await
            .expect("the run finished");
    feeder.await.unwrap();
    assert_eq!(exit.code, 0);
    assert!(err.is_empty());
    assert!(largest <= exec::OUTPUT_CHUNK, "{largest}");
    assert_eq!(out.len(), payload.len());
    assert!(out == payload);
}

#[tokio::test]
async fn exec_drop_before_stdin_close_kills_the_child_and_applies_nothing() {
    let scratch = Scratch::new("drop-before");
    // The fake git reads its whole input and only then acts: an end of input, even a partial one, makes it act.
    let mut running = launch(&scratch, &["git", "hold"]);
    let input = running.input.take().unwrap();
    input.send(stdin(b"the first half of a patch\n")).await.unwrap();
    let pid = scratch.pid("pid").await;
    until(|| std::fs::read(scratch.path("got")).is_ok_and(|got| !got.is_empty())).await;
    drop(input);
    drop(running.output.take());
    assert_eq!(running.ended.await.unwrap(), Ended::KilledBeforeClose);
    until(|| ended(pid)).await;
    assert!(
        !scratch.path("acted").exists(),
        "the child acted on an input it never had whole"
    );
    assert_eq!(
        std::fs::read(scratch.path("got")).unwrap(),
        b"the first half of a patch\n"
    );
    assert!(scratch.audit().contains("exec git hold cwd="));
    assert!(scratch.audit().contains("killed=input-unfinished"));
}

#[tokio::test]
async fn a_lost_link_is_seen_while_a_write_waits_on_a_child_that_does_not_read() {
    let scratch = Scratch::new("lost-link");
    let mut running = launch(&scratch, &["git", "noread"]);
    let input = running.input.take().unwrap();
    // More than a pipe holds, so the write waits; the next message is then held back behind it.
    input.send(stdin(&vec![b'x'; 512 * 1024])).await.unwrap();
    input.send(stdin(b"more")).await.unwrap();
    let pid = scratch.pid("pid").await;
    // The transport drops the answer on a lost link; the input stream stays as it was.
    drop(running.output.take());
    let killed = tokio::time::timeout(Duration::from_secs(10), &mut running.ended)
        .await
        .expect("the lost link was seen while the write waited")
        .unwrap();
    assert_eq!(killed, Ended::KilledBeforeClose);
    until(|| ended(pid)).await;
    assert!(!scratch.path("acted").exists());
    drop(input);
}

#[tokio::test]
async fn a_run_dropped_before_close_kills_its_child_before_letting_go_of_its_input() {
    let scratch = Scratch::new("guard");
    let mut running = launch(&scratch, &["git", "hold"]);
    let input = running.input.take().unwrap();
    input.send(stdin(b"partial")).await.unwrap();
    let pid = scratch.pid("pid").await;
    // The daemon's runtime going away drops the run's task, as aborting it does here.
    running.ended.abort();
    assert!(running.ended.await.unwrap_err().is_cancelled());
    until(|| ended(pid)).await;
    assert!(
        !scratch.path("acted").exists(),
        "the child saw an end of input when its run was dropped"
    );
    drop(input);
}

#[tokio::test]
async fn a_stopping_daemon_kills_unclosed_runs_and_waits_a_while_for_closed_ones() {
    let scratch = Scratch::new("stopping");
    let runs: Arc<Runs> = Arc::default();
    let mut unclosed = launch_with(&scratch, &["git", "hold"], true, &runs, |_| {});
    let unclosed_input = unclosed.input.take().unwrap();
    unclosed_input.send(stdin(b"partial")).await.unwrap();
    let held = scratch.pid("pid").await;
    let mut quick = launch_with(&scratch, &["git", "slow"], true, &runs, |_| {});
    quick.input.take().unwrap().send(close()).await.unwrap();
    let mut long = launch_with(&scratch, &["git", "slower"], true, &runs, |_| {});
    long.input.take().unwrap().send(close()).await.unwrap();
    let slower = scratch.pid("slower.pid").await;
    runs.kill_unclosed();
    until(|| ended(held)).await;
    assert!(!scratch.path("acted").exists());
    // The stopping daemon's connections go with it.
    drop(unclosed_input);
    drop(unclosed.output.take());
    runs.settle(&scratch.path("state"), Duration::from_secs(5)).await;
    assert!(scratch.path("slow-done").exists(), "a closed run is waited for");
    let audit = scratch.audit();
    assert!(audit.contains("exec git slow cwd="), "{audit}");
    assert!(audit.contains("exec git slower cwd="), "{audit}");
    assert!(audit.contains("outcome-unknown"), "{audit}");
    assert!(audit.contains("exec git hold cwd="), "{audit}");
    assert!(audit.contains("killed=daemon-stop"), "{audit}");
    // SAFETY: the test's own child, still unreaped by its run.
    unsafe { libc::kill(-slower, libc::SIGKILL) };
    let _ = long.ended.await;
}

#[tokio::test]
async fn a_run_with_nothing_after_its_start_starts_nothing_and_a_close_first_gives_no_input() {
    let scratch = Scratch::new("lazy");
    let running = launch(&scratch, &["git", "mark"]);
    drop(running.input);
    assert_eq!(running.ended.await.unwrap(), Ended::NotStarted);
    assert!(!scratch.path("started").exists());
    assert!(!scratch.audit().contains("exec-start"));
    assert!(scratch.audit().contains("not-started"));
    // A command that reads no input is started by a close and runs with an empty stdin.
    let mut closed = launch(&scratch, &["git", "cat"]);
    closed.input.take().unwrap().send(close()).await.unwrap();
    let (out, _, exit, _) = collect(closed.output.as_mut().unwrap()).await;
    assert_eq!((out.len(), exit.code), (0, 0));
}

#[tokio::test]
async fn a_child_that_exits_before_close_is_reaped_and_audited_and_its_status_comes_after_close() {
    let scratch = Scratch::new("early-exit");
    let mut running = launch(&scratch, &["git", "quick"]);
    let input = running.input.take().unwrap();
    input.send(stdin(b"unread")).await.unwrap();
    until(|| scratch.audit().contains("exec-exited git quick")).await;
    assert!(scratch.audit().contains("exit=3"));
    input.send(close()).await.unwrap();
    let (_, _, exit, _) = collect(running.output.as_mut().unwrap()).await;
    assert_eq!(exit.code, 3);
    assert_eq!(running.ended.await.unwrap(), Ended::Exited(exit));
}

#[tokio::test]
async fn daemon_stop_preserves_the_recorded_exit_before_close() {
    let scratch = Scratch::new("reaped-stop");
    let runs = Arc::default();
    let running = launch_with(&scratch, &["git", "quick"], true, &runs, |_| {});
    running.input.as_ref().unwrap().send(stdin(b"unread")).await.unwrap();
    until(|| scratch.audit().contains("exec-exited git quick")).await;
    runs.kill_unclosed();
    assert!(matches!(running.ended.await.unwrap(), Ended::Exited(exit) if exit.code == 3));
    assert!(!scratch.audit().contains("killed=daemon-stop"));
}

#[test]
fn the_forwarded_argv_owns_the_git_pathspec_environment() {
    let scratch = Scratch::new("pathspec-env");
    let mut login = scratch.login(false);
    let keys = [
        "GIT_ICASE_PATHSPECS",
        "GIT_LITERAL_PATHSPECS",
        "GIT_GLOB_PATHSPECS",
        "GIT_NOGLOB_PATHSPECS",
    ];
    for key in keys {
        login.insert(key.into(), "true".into());
    }
    let prepared = exec::git::prepare(&start(&scratch.path("repo"), &["git", "add", "FILE"]), &login).unwrap();
    assert!(keys.iter().all(|key| !prepared.env.iter().any(|(k, _)| k == *key)));
}

#[tokio::test]
async fn output_is_never_cut_short_for_a_slow_reader() {
    let scratch = Scratch::new("slow-reader");
    let mut running = launch(&scratch, &["git", "big"]);
    running.input.take().unwrap().send(close()).await.unwrap();
    // Longer than a silent pipe is waited for after the child's exit.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (out, _, exit, _) = collect(running.output.as_mut().unwrap()).await;
    assert_eq!((out.len(), exit.code), (1024 * 1024, 0));
}

#[tokio::test]
async fn a_run_past_its_deadline_is_killed() {
    let scratch = Scratch::new("deadline");
    let mut running = launch_with(&scratch, &["git", "slower"], true, &Arc::default(), |prepared| {
        prepared.deadline = Duration::from_millis(500);
    });
    running.input.take().unwrap().send(close()).await.unwrap();
    let pid = scratch.pid("slower.pid").await;
    assert_eq!(running.ended.await.unwrap(), Ended::KilledAtDeadline);
    until(|| ended(pid)).await;
    assert!(scratch.audit().contains("killed=deadline"));
}

#[tokio::test]
async fn exec_child_survives_a_drop_after_close_and_its_status_is_audited() {
    let scratch = Scratch::new("drop-after");
    scratch.repository();
    let hook = scratch.path("repo/.git/hooks/pre-commit");
    let marker = scratch.path("hook-finished");
    std::fs::write(&hook, format!("#!/bin/sh\nsleep 1\n: > '{}'\n", marker.display())).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(scratch.path("repo/second.txt"), "second\n").unwrap();
    scratch.git(&["add", "second.txt"]);
    let mut running = launch_with(
        &scratch,
        &["git", "commit", "-q", "-m", "from the laptop"],
        false,
        &Arc::default(),
        |_| {},
    );
    let input = running.input.take().unwrap();
    input.send(close()).await.unwrap();
    // The laptop goes away right after its close, long before the hook is done.
    drop(input);
    drop(running.output.take());
    assert!(!marker.exists(), "the drop came before the commit finished");
    let ended = tokio::time::timeout(Duration::from_secs(60), running.ended)
        .await
        .expect("the commit ran to its end")
        .unwrap();
    assert_eq!(
        ended,
        Ended::Exited(ExecExit {
            code: 0,
            signal: 0,
            refusal: String::new()
        })
    );
    assert!(marker.exists());
    assert_eq!(scratch.git(&["log", "-1", "--format=%s"]), "from the laptop\n");
    let audit = scratch.audit();
    assert!(
        audit.contains(&format!(
            "exec git commit cwd={} started_ms=1234 exit=0",
            scratch.path("repo").display()
        )),
        "{audit}"
    );
}

#[tokio::test]
async fn close_then_immediate_disconnect_still_runs() {
    let scratch = Scratch::new("close-then-drop");
    // A command that reads nothing: the close starts it, and the link is gone at once.
    for round in 0..50 {
        let stamp = format!("stamp-{round}");
        let mut running = launch(&scratch, &["git", &stamp]);
        let input = running.input.take().unwrap();
        input.send(close()).await.unwrap();
        drop(input);
        drop(running.output.take());
        let ended = running.ended.await.unwrap();
        assert_eq!(
            ended,
            Ended::Exited(ExecExit {
                code: 0,
                signal: 0,
                refusal: String::new()
            }),
            "round {round}"
        );
        assert!(scratch.path(&stamp).exists(), "round {round}: the command never ran");
    }
    // The whole input and its close, then the link gone at once: the child gets all of it and acts.
    for round in 0..20 {
        for marker in ["pid", "got", "acted"] {
            let _ = std::fs::remove_file(scratch.path(marker));
        }
        let mut running = launch(&scratch, &["git", "hold"]);
        let input = running.input.take().unwrap();
        input.send(stdin(b"a whole patch\n")).await.unwrap();
        input.send(close()).await.unwrap();
        drop(input);
        drop(running.output.take());
        let ended = running.ended.await.unwrap();
        assert!(
            matches!(ended, Ended::Exited(ExecExit { code: 0, .. })),
            "round {round}: {ended:?}"
        );
        assert!(scratch.path("acted").exists(), "round {round}");
        assert_eq!(
            std::fs::read(scratch.path("got")).unwrap(),
            b"a whole patch\n",
            "round {round}"
        );
    }
}
