use super::*;
use std::sync::mpsc::Receiver;

/// A scratch directory holding a project and a home, removed at the end.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("eitri-shell-line-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir.canonicalize().unwrap())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn request(scratch: &Scratch, command: &str) -> Request {
    Request {
        command: command.to_string(),
        cwd: scratch.0.clone(),
        root: scratch.0.clone(),
        spool_dir: Some(scratch.0.clone()),
        timeout: Duration::from_secs(20),
    }
}

fn run_with(request: Request, shell: &str) -> (Running, Receiver<()>) {
    let (woke, wake) = mpsc::channel();
    let running = start(request, Some(PathBuf::from(shell)), move || {
        let _ = woke.send(());
    });
    (running, wake)
}

fn finish(running: &Running, wake: &Receiver<()>) -> Finished {
    wake.recv_timeout(Duration::from_secs(30)).expect("the line ends");
    running.finished().expect("the wake comes after the answer")
}

fn line(scratch: &Scratch, command: &str) -> Finished {
    let (running, wake) = run_with(request(scratch, command), "/bin/sh");
    finish(&running, &wake)
}

#[test]
fn stdout_and_stderr_are_one_stream_and_the_status_is_kept() {
    let scratch = Scratch::new("stream");
    let done = line(&scratch, "echo OUT; echo ERR >&2; exit 3");
    assert_eq!(done.output, "OUT\nERR\n");
    assert_eq!(done.ending, Ending::Exited(3));
    assert_eq!(done.command, "echo OUT; echo ERR >&2; exit 3");
}

#[test]
fn it_runs_in_its_directory_with_nothing_on_stdin_and_the_clis_mark() {
    let scratch = Scratch::new("where");
    let done = line(&scratch, "pwd; read x; echo \"[$x]\"; echo \"$CLAUDECODE\"; tty");
    assert_eq!(done.output, format!("{}\n[]\n1\nnot a tty\n", scratch.0.display()));
}

#[test]
fn the_words_of_a_line_are_data_to_the_script_around_it() {
    let scratch = Scratch::new("quoting");
    let done = line(&scratch, r#"echo "a'b" '"$1"' `echo c`; echo "$0" | wc -l"#);
    assert_eq!(done.output, "a'b \"$1\" c\n1\n");
}

#[test]
fn it_says_where_it_ended_unless_it_left_by_exit_or_left_the_project() {
    let scratch = Scratch::new("cwd");
    std::fs::create_dir(scratch.0.join("sub")).unwrap();
    assert_eq!(line(&scratch, "cd sub").cwd, Some(scratch.0.join("sub")));
    assert_eq!(line(&scratch, "exit 0").cwd, None);
    let outside = line(&scratch, "cd /");
    assert_eq!((outside.cwd, outside.left_project), (None, true));
    assert!(!line(&scratch, "cd sub").left_project);
}

#[test]
fn the_files_it_used_are_gone_after_it() {
    let scratch = Scratch::new("clean");
    let _ = line(&scratch, "true");
    assert_eq!(std::fs::read_dir(&scratch.0).unwrap().count(), 0);
}

#[test]
fn the_rc_file_gives_aliases_and_functions_quietly_and_does_not_move_it() {
    if !Path::new("/bin/bash").exists() {
        return;
    }
    let scratch = Scratch::new("rc");
    let home = scratch.0.join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::write(
        home.join(".bashrc"),
        "echo NOISE\ncd /\nalias bangalias='echo FROM-ALIAS'\nbangfn() { echo FROM-FUNCTION; }\n",
    )
    .unwrap();
    let script = script(Path::new("/bin/bash"));
    assert!(script.contains("shopt -s expand_aliases"));
    let inner = format!(
        "HOME={} /bin/bash -c {} /bin/bash 'bangalias; bangfn; pwd' /dev/null",
        home.display(),
        shell_quote(&script)
    );
    let done = line(&scratch, &inner);
    assert_eq!(
        done.output,
        format!("FROM-ALIAS\nFROM-FUNCTION\n{}\n", scratch.0.display())
    );
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[test]
fn zsh_takes_the_same_script() {
    if !Path::new("/usr/bin/zsh").exists() && !Path::new("/bin/zsh").exists() {
        return;
    }
    let shell = if Path::new("/usr/bin/zsh").exists() {
        "/usr/bin/zsh"
    } else {
        "/bin/zsh"
    };
    let scratch = Scratch::new("zsh");
    let home = scratch.0.join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::write(home.join(".zshrc"), "alias bangalias='echo FROM-ALIAS'\n").unwrap();
    let inner = format!(
        "ZDOTDIR={} {shell} -c {} {shell} 'bangalias; no-such-command-xyz' /dev/null",
        home.display(),
        shell_quote(&script(Path::new(shell)))
    );
    let done = line(&scratch, &inner);
    assert!(done.output.starts_with("FROM-ALIAS\n"), "{}", done.output);
    assert!(
        done.output.contains("(eval):1: command not found: no-such-command-xyz"),
        "{}",
        done.output
    );
}

#[test]
fn an_interrupt_stops_the_whole_group_and_says_so() {
    let scratch = Scratch::new("interrupt");
    let marker = scratch.0.join("late");
    let command = format!("echo PART; (sleep 5; touch {}) & sleep 5", marker.display());
    let (running, wake) = run_with(request(&scratch, &command), "/bin/sh");
    let began = Instant::now();
    while !std::fs::read_dir(&scratch.0)
        .unwrap()
        .flatten()
        .any(|e| e.path().extension().is_some_and(|x| x == "cwd"))
    {
        assert!(began.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(100));
    running.interrupt();
    let done = finish(&running, &wake);
    assert_eq!(done.ending, Ending::Interrupted);
    assert!(done.output.starts_with("PART\n"), "{}", done.output);
    assert!(began.elapsed() < Duration::from_secs(4));
}

#[test]
fn a_child_that_ignores_the_polite_signal_goes_with_the_shell() {
    let scratch = Scratch::new("descendant");
    let began_file = scratch.0.join("began");
    let marker = scratch.0.join("late");
    let command = format!(
        "(trap '' TERM; touch {}; sleep 1; touch {}) & wait",
        began_file.display(),
        marker.display()
    );
    let (running, wake) = run_with(request(&scratch, &command), "/bin/sh");
    let began = Instant::now();
    while !began_file.exists() {
        assert!(began.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(5));
    }
    running.interrupt();
    assert_eq!(finish(&running, &wake).ending, Ending::Interrupted);
    std::thread::sleep(Duration::from_millis(1300));
    assert!(!marker.exists(), "it outlived the stop");
}

#[test]
fn the_limit_kills_what_ignores_the_polite_signal() {
    let scratch = Scratch::new("limit");
    let mut asked = request(&scratch, "trap '' TERM; echo PART; sleep 30");
    asked.timeout = Duration::from_millis(100);
    let began = Instant::now();
    let (running, wake) = run_with(asked, "/bin/sh");
    let done = finish(&running, &wake);
    assert_eq!(done.ending, Ending::TimedOut(Duration::from_millis(100)));
    assert_eq!(done.output, "PART\n");
    assert!(began.elapsed() < Duration::from_secs(10));
}

#[test]
fn a_background_job_does_not_hold_the_line_open() {
    let scratch = Scratch::new("background");
    let began = Instant::now();
    let done = line(&scratch, "sleep 3 & echo DONE");
    assert_eq!(done.output, "DONE\n");
    assert_eq!(done.ending, Ending::Exited(0));
    assert!(began.elapsed() < Duration::from_millis(2500));
}

#[test]
fn a_long_output_goes_to_a_private_file_and_its_start_stays_in_memory() {
    let scratch = Scratch::new("spool");
    let done = line(&scratch, "seq 1 40000");
    let spooled = done.spooled.expect("it is past the limit");
    assert_eq!(std::fs::metadata(&spooled.path).unwrap().len(), spooled.bytes);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&spooled.path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(done.output.starts_with("1\n2\n"));
}

#[test]
fn an_output_a_little_over_the_inline_limit_is_spooled_at_the_end() {
    let scratch = Scratch::new("edge");
    assert!(line(&scratch, "seq 1 6200").spooled.is_none());
    let done = line(&scratch, "seq 1 8000");
    let spooled = done.spooled.expect("38 KB is past 30000 characters");
    assert_eq!(spooled.bytes, 38893);
    assert_eq!(std::fs::read_to_string(&spooled.path).unwrap().len(), 38893);
}

#[test]
fn a_shell_that_is_not_there_says_so() {
    let scratch = Scratch::new("noshell");
    let (running, wake) = run_with(request(&scratch, "true"), "/no/such/bash");
    let done = finish(&running, &wake);
    assert_eq!(done.ending, Ending::NotStarted);
    assert!(done.output.contains("/no/such/bash"), "{}", done.output);
}

#[test]
fn dropping_a_running_line_kills_it() {
    let scratch = Scratch::new("drop");
    let marker = scratch.0.join("late");
    let command = format!("sleep 1; touch {}", marker.display());
    let (running, _wake) = run_with(request(&scratch, &command), "/bin/sh");
    drop(running);
    std::thread::sleep(Duration::from_millis(1400));
    assert!(!marker.exists());
}

#[test]
fn a_shell_of_another_kind_gives_way_to_one_the_cli_runs_with() {
    let there = |path: &Path| path == Path::new("/bin/bash") || path == Path::new("/usr/bin/fish");
    assert_eq!(
        choose_shell(Some("/usr/bin/fish".as_ref()), there),
        Some(PathBuf::from("/bin/bash"))
    );
    assert_eq!(
        choose_shell(Some("/opt/bash".as_ref()), |p| p == Path::new("/opt/bash")),
        Some(PathBuf::from("/opt/bash"))
    );
    assert_eq!(
        choose_shell(Some("bash".as_ref()), there),
        Some(PathBuf::from("/bin/bash"))
    );
    assert_eq!(choose_shell(None, |_| false), None);
}

#[test]
fn the_environment_loses_the_host_shells_tmux_and_gains_the_mark() {
    let host = [
        ("TMUX", "x"),
        ("TMUX_PANE", "%1"),
        ("NEOVIM_BIN", "/n"),
        ("PATH", "/bin"),
        ("CLAUDECODE", "0"),
    ]
    .map(|(k, v)| (k.into(), v.into()));
    let env: Vec<(std::ffi::OsString, std::ffi::OsString)> = environment(host);
    assert_eq!(env, [("PATH".into(), "/bin".into()), ("CLAUDECODE".into(), "1".into())]);
}

#[test]
fn an_old_output_goes_and_a_new_one_stays() {
    let scratch = Scratch::new("prune");
    let file = scratch.0.join("kept.txt");
    std::fs::write(&file, "x").unwrap();
    prune(&scratch.0, Duration::from_secs(3600));
    assert!(file.exists());
    prune(&scratch.0, Duration::ZERO);
    assert!(!file.exists());
}
