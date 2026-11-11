use super::*;
use std::sync::mpsc::Receiver;

thread_local! {
    static CLOCK: std::cell::RefCell<Option<(Instant, Arc<std::sync::atomic::AtomicBool>, std::cell::Cell<u64>)>> = const { std::cell::RefCell::new(None) };
}

pub(super) fn clock_now() -> Option<Instant> {
    CLOCK.with(|clock| {
        let clock = clock.borrow();
        let (base, advance, step) = clock.as_ref()?;
        if advance.load(Ordering::SeqCst) {
            assert!(step.get() < 10000, "the shell worker exceeded 10000 clock reads");
            step.set(step.get() + 1);
        }
        Some(*base + Duration::from_millis(step.get() * 50))
    })
}

fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(60);
    while !ready() {
        assert!(Instant::now() < until, "never happened: {what}");
        std::thread::yield_now();
    }
}

pub(super) struct ReaderCount(Arc<std::sync::atomic::AtomicUsize>);

impl ReaderCount {
    pub(super) fn new(count: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count)
    }
}

impl Drop for ReaderCount {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(crate) struct GroupCleanup(Arc<Control>);

impl GroupCleanup {
    pub(crate) fn new(running: &Running) -> Self {
        Self(running.control.clone())
    }
}

impl Drop for GroupCleanup {
    fn drop(&mut self) {
        let group = self.0.group.lock().unwrap();
        if let Some(pid) = *group {
            if pid > 1 {
                // The worker holds this lock until it reaps the leader, preventing PID reuse here.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
        }
    }
}

#[test]
fn the_cleanup_guard_kills_the_group_during_unwinding() {
    let scratch = Scratch::new("panic-cleanup");
    let (running, wake) = run_with(
        request(
            &scratch,
            "trap '' TERM; touch ready; i=0; while [ $i -lt 100 ]; do i=$((i+1)); sleep 0.02; done; touch survived",
        ),
        "/bin/sh",
    );
    wait_for_file(&scratch.0.join("ready"));
    let cleanup = GroupCleanup::new(&running);
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _cleanup = cleanup;
        panic!("exercise unwinding with a live group");
    }));
    assert!(unwound.is_err());
    finish(&running, &wake);
    assert!(
        !scratch.0.join("survived").exists(),
        "unwinding must kill the group before its natural end"
    );
}

fn complete_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()
        .filter(|s| s.ends_with('\n'))
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|p| *p > 1)
}

#[test]
fn only_a_complete_pid_line_is_a_record() {
    let scratch = Scratch::new("pid-record");
    let path = scratch.0.join("pid");
    for bytes in ["", "12", "1\n", "garbled\n"] {
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(complete_pid(&path), None, "incomplete or invalid PID record {bytes:?}");
    }
    std::fs::write(&path, "123\n").unwrap();
    assert_eq!(pid_record(&path), 123);
}

fn pid_record(path: &Path) -> u32 {
    let mut pid = None;
    until("a complete PID record", || {
        pid = complete_pid(path);
        pid.is_some()
    });
    pid.unwrap()
}

fn process_runs(pid: u32, start: u64) -> bool {
    if crate::split::proc_start_time(pid) != Some(start) {
        return false;
    }
    let result = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output()
        .expect("ps runs on Unix");
    result.status.success() && !String::from_utf8_lossy(&result.stdout).trim_start().starts_with('Z')
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !path.exists() {
        assert!(Instant::now() < deadline, "missing {}", path.display());
        std::thread::sleep(POLL);
    }
}

#[test]
fn a_background_writer_cannot_outlive_its_reader_or_create_a_late_spool() {
    let scratch = Scratch::new("late-output");
    // The background writer cannot print until the report has been taken.
    let command = format!(
        "(while [ ! -f release ] && [ -d '{}' ]; do sleep 0.02; done; seq 1 40000; touch attempted) & echo DONE",
        scratch.0.display()
    );
    let (running, wake) = run_with(request(&scratch, &command), "/bin/sh");
    let done = finish(&running, &wake);
    let readers = running.control.readers.load(Ordering::SeqCst);
    std::fs::write(scratch.0.join("release"), b"").unwrap();
    wait_for_file(&scratch.0.join("attempted"));
    assert_eq!(readers, 0, "the reader must be joined before reporting");
    assert_eq!(done.output, "DONE\n");
    assert_eq!(done.ending, Ending::Exited(0));
    assert!(done.output_loss.as_deref().unwrap().contains("drain deadline"));
    assert!(wire::wire_text(&done, None).contains("later output discarded"));
    assert!(wire::row_text(&done).contains("later output discarded"));
    assert!(!std::fs::read_dir(&scratch.0)
        .unwrap()
        .flatten()
        .any(|e| e.path().extension().is_some_and(|x| x == "txt")));
}

#[test]
fn a_worker_that_cannot_spawn_finishes_with_its_error() {
    let scratch = Scratch::new("spawn-failure");
    let running = start_with_spawn(
        request(&scratch, "touch never"),
        None,
        || {},
        |_job| Err(std::io::Error::other("forced worker spawn failure")),
    );
    let done = running.finished().expect("spawn failure is terminal");
    assert_eq!(done.ending, Ending::NotStarted);
    assert!(done.output.contains("forced worker spawn failure"));
    assert!(running.finished().is_none(), "a result is delivered only once");
    assert!(!scratch.0.join("never").exists());
}

#[test]
fn a_disconnected_worker_finishes_instead_of_waiting_forever() {
    let scratch = Scratch::new("disconnected");
    let running = start_with_spawn(
        request(&scratch, "true"),
        None,
        || {},
        |job| {
            drop(job);
            Ok(())
        },
    );
    let done = running.finished().expect("a lost worker is terminal");
    assert_eq!(done.ending, Ending::NotStarted);
    assert_eq!(done.command, "true");
    assert!(done.output.contains("disconnected"));
}

#[test]
fn output_past_the_spool_cap_is_reported_as_partial() {
    let scratch = Scratch::new("spool-cap");
    let done = line(&scratch, &format!("head -c {} /dev/zero", SPOOL_BYTES + 7));
    let text = wire::wire_text(&done, None);
    assert!(text.contains("first 67108864 bytes of 67108871"), "{}", &text[..200]);
    assert!(!text.contains("Full output"));
    assert!(wire::row_text(&done).contains("cap"));
    assert_eq!(
        std::fs::metadata(&done.spooled.unwrap().path).unwrap().len(),
        SPOOL_BYTES
    );
}

#[test]
fn a_spool_write_error_is_reported_as_partial() {
    let scratch = Scratch::new("spool-error");
    let path = scratch.0.join("output.txt");
    std::fs::write(&path, b"abc").unwrap();
    let mut gathered = Gathered {
        held: b"abc".to_vec(),
        total: 3,
        spool: Some((path, std::fs::File::open(scratch.0.join("output.txt")).unwrap(), 3)),
        ..Gathered::default()
    };
    gathered.take(b"def", Some(&scratch.0));
    // A later successful write must not leave a gap in the saved prefix.
    gathered.spool.as_mut().unwrap().1 = std::fs::OpenOptions::new()
        .append(true)
        .open(scratch.0.join("output.txt"))
        .unwrap();
    gathered.take(b"ghi", Some(&scratch.0));
    assert_eq!(std::fs::read(scratch.0.join("output.txt")).unwrap(), b"abc");
    let total = gathered.total;
    let done = Finished {
        command: "cmd".into(),
        ending: Ending::Exited(0),
        output: String::from_utf8_lossy(&gathered.held).into_owned(),
        spooled: gathered.spool.take().map(|(path, _, stored_bytes)| Spooled {
            path,
            bytes: total,
            stored_bytes,
        }),
        output_loss: gathered.output_loss.map(str::to_string),
        cwd: None,
        left_project: false,
    };
    for text in [wire::wire_text(&done, None), wire::row_text(&done)] {
        assert!(text.contains("first 3 bytes of 9"), "{text}");
        assert!(text.contains("write error"));
        assert!(!text.contains("Full output"));
    }
}

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
    let done = running.finished().expect("the wake comes after the answer");
    assert!(running.finished().is_none(), "a result is delivered only once");
    done
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
    // BSD `wc` pads its count with spaces; `tr` takes them off so the count reads the same everywhere.
    let done = line(&scratch, r#"echo "a'b" '"$1"' `echo c`; echo "$0" | wc -l | tr -d ' '"#);
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
    let command = "echo PART; touch ready; sleep 300 & wait";
    let (running, wake) = run_with(request(&scratch, &command), "/bin/sh");
    let _cleanup = GroupCleanup::new(&running);
    wait_for_file(&scratch.0.join("ready"));
    running.interrupt();
    let done = finish(&running, &wake);
    assert_eq!(done.ending, Ending::Interrupted);
    assert!(done.output.starts_with("PART\n"), "{}", done.output);
}

#[test]
fn a_child_that_ignores_the_polite_signal_goes_with_the_shell() {
    let scratch = Scratch::new("descendant");
    let began_file = scratch.0.join("began");
    let marker = scratch.0.join("late");
    let command = format!(
        "(trap '' TERM; touch {}; i=0; while [ $i -lt 500 ]; do i=$((i+1)); sleep 0.02; done; touch survived) & echo $! > {}; wait",
        began_file.display(),
        marker.display()
    );
    let (running, wake) = run_with(request(&scratch, &command), "/bin/sh");
    let _cleanup = GroupCleanup::new(&running);
    let pid = pid_record(&marker);
    wait_for_file(&began_file);
    let start = crate::split::proc_start_time(pid).expect("the descendant has started");
    assert!(
        process_runs(pid, start),
        "the portable liveness probe sees the live descendant"
    );
    running.interrupt();
    assert_eq!(finish(&running, &wake).ending, Ending::Interrupted);
    until("the descendant ends", || !process_runs(pid, start));
    assert!(
        !scratch.0.join("survived").exists(),
        "the descendant ran to its natural end after interrupt"
    );
}

#[test]
fn the_limit_kills_what_ignores_the_polite_signal() {
    let scratch = Scratch::new("limit");
    let mut asked = request(
        &scratch,
        "trap '' TERM; echo PART; touch ready; i=0; while [ $i -lt 500 ]; do i=$((i+1)); sleep 0.02; done; touch survived",
    );
    asked.timeout = Duration::from_millis(100);
    let advance = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker_advance = advance.clone();
    let (woke, wake) = mpsc::channel();
    let running = start_with_spawn(
        asked,
        Some("/bin/sh".into()),
        move || {
            let _ = woke.send(());
        },
        move |job| {
            std::thread::spawn(move || {
                CLOCK.with(|clock| {
                    *clock.borrow_mut() = Some((Instant::now(), worker_advance, std::cell::Cell::new(0)))
                });
                job();
            });
            Ok(())
        },
    );
    let _cleanup = GroupCleanup::new(&running);
    wait_for_file(&scratch.0.join("ready"));
    advance.store(true, Ordering::SeqCst);
    let done = finish(&running, &wake);
    assert_eq!(done.ending, Ending::TimedOut(Duration::from_millis(100)));
    assert_eq!(done.output, "PART\n");
    assert!(
        !scratch.0.join("survived").exists(),
        "SIGKILL must end the TERM-ignoring shell before its natural end"
    );
}

#[test]
fn a_background_job_does_not_hold_the_line_open() {
    let scratch = Scratch::new("background");
    let done = line(
        &scratch,
        "(i=0; while [ ! -e release ] && [ $i -lt 500 ]; do i=$((i+1)); sleep 0.02; done; if [ ! -e release ]; then touch survived; fi; touch attempted) & echo DONE",
    );
    assert_eq!(done.output, "DONE\n");
    assert_eq!(done.ending, Ending::Exited(0));
    assert!(done.output_loss.as_deref().unwrap().contains("drain deadline"));
    std::fs::write(scratch.0.join("release"), b"").unwrap();
    wait_for_file(&scratch.0.join("attempted"));
    assert!(
        !scratch.0.join("survived").exists(),
        "the background job must end through its release gate"
    );
}

#[test]
fn a_long_output_goes_to_a_private_file_and_its_start_stays_in_memory() {
    let scratch = Scratch::new("spool");
    let done = line(&scratch, "seq 1 40000");
    assert!(done.output_loss.is_none());
    let spooled = done.spooled.expect("it is past the limit");
    assert_eq!(spooled.stored_bytes, spooled.bytes);
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
    let (running, wake) = run_with(
        request(
            &scratch,
            "touch ready; i=0; while [ $i -lt 500 ]; do i=$((i+1)); sleep 0.02; done; touch survived",
        ),
        "/bin/sh",
    );
    let _cleanup = GroupCleanup::new(&running);
    wait_for_file(&scratch.0.join("ready"));
    drop(running);
    wake.recv_timeout(Duration::from_secs(60))
        .expect("the dropped line's worker reaped its child");
    assert!(
        !scratch.0.join("survived").exists(),
        "Drop kills the line before its bounded natural end"
    );
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
