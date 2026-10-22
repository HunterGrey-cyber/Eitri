//! The task runner with real child processes: `sh`, `sleep` and `head`. Every wait is a 60 s hang guard;
//! nothing asserts how long anything took.
//! The runs go through the Linux-only run guard, so the file is built for Linux alone.

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eitri_core::plugin::tasks::{ExitKind, GuardCommand, Launch, StartError, Stream, TaskEvent, TaskRunner};

const HANG_GUARD: Duration = Duration::from_secs(60);

/// This binary's guard: it serves only when a runner started it as one.
#[test]
fn guard_entry() {
    if std::env::var_os(eitri_core::run_guard::HELPER_VAR).is_some() {
        eitri_core::run_guard::serve_inherited();
    }
}

/// Every run here goes through a guard, as a window's runs do.
fn runner() -> (TaskRunner, Arc<AtomicUsize>) {
    runner_with(Some(eitri_core::run_guard::test_binary_guard("guard_entry")))
}

fn runner_with(guard: Option<GuardCommand>) -> (TaskRunner, Arc<AtomicUsize>) {
    let wakes = Arc::new(AtomicUsize::new(0));
    let counter = wakes.clone();
    (
        TaskRunner::with_guard(
            Arc::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            }),
            guard,
        ),
        wakes,
    )
}

fn sh(task: &str, script: &str, timeout: Option<Duration>) -> Launch {
    Launch {
        task: task.into(),
        vars: BTreeMap::new(),
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: std::env::temp_dir(),
        timeout,
    }
}

/// Polls with a far-future clock, so a partial batch is taken at once, until `run` has ended.
fn until_exit(r: &mut TaskRunner, run: u64) -> (Vec<TaskEvent>, ExitKind, u64, u64) {
    let deadline = Instant::now() + HANG_GUARD;
    let mut seen = Vec::new();
    loop {
        assert!(Instant::now() < deadline, "run {run} never ended");
        let (events, _) = r.poll(Instant::now() + Duration::from_secs(3600));
        for e in events {
            if let TaskEvent::Exit {
                run: ended,
                kind,
                dropped_lines,
                dropped_bytes,
            } = &e
            {
                if *ended == run {
                    let (kind, l, b) = (kind.clone(), *dropped_lines, *dropped_bytes);
                    seen.push(e);
                    return (seen, kind, l, b);
                }
            }
            seen.push(e);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn stdout_lines(events: &[TaskEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            TaskEvent::Output {
                stream: Stream::Stdout,
                lines,
                ..
            } => Some(lines.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

/// Waits for the first stdout line of `run`.
fn first_line(r: &mut TaskRunner, run: u64) -> String {
    let deadline = Instant::now() + HANG_GUARD;
    loop {
        assert!(Instant::now() < deadline, "no output from run {run}");
        let (events, _) = r.poll(Instant::now() + Duration::from_secs(3600));
        if let Some(line) = stdout_lines(&events).into_iter().next() {
            return line;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Whether `pid` is gone (or only a zombie waiting for its new parent to reap it).
fn gone(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .is_some_and(|rest| rest.trim_start().starts_with('Z')),
    }
}

fn wait_gone(pid: i32) {
    let deadline = Instant::now() + HANG_GUARD;
    while !gone(pid) {
        assert!(Instant::now() < deadline, "pid {pid} outlived its run");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_run_reports_its_output_and_exit_code() {
    let (mut r, _) = runner();
    let run = r
        .start(sh("t", "echo one; echo two 1>&2; echo three; exit 3", None))
        .unwrap();
    let (events, kind, _, _) = until_exit(&mut r, run);
    assert_eq!(kind, ExitKind::Exited(3));
    assert_eq!(stdout_lines(&events), ["one", "three"]);
    assert!(events
        .iter()
        .any(|e| matches!(e, TaskEvent::Output { stream: Stream::Stderr, lines, .. } if lines == &["two"])));
    assert!(r.is_idle());
}

#[test]
fn a_runner_without_a_guard_starts_its_task_itself() {
    let (mut r, _) = runner_with(None);
    let run = r
        .start(sh("t", "echo one; echo two 1>&2; echo \"parent=$PPID\"; exit 3", None))
        .unwrap();
    let (events, kind, _, _) = until_exit(&mut r, run);
    assert_eq!(kind, ExitKind::Exited(3));
    assert_eq!(
        stdout_lines(&events),
        ["one".to_owned(), format!("parent={}", std::process::id())]
    );
}

#[test]
fn a_timeout_ends_the_whole_group() {
    let (mut r, _) = runner();
    let run = r
        .start(sh("t", "sleep 300 & echo $!; wait", Some(Duration::from_millis(300))))
        .unwrap();
    // Collected whole: the line and the end may arrive in one poll.
    let (events, kind, _, _) = until_exit(&mut r, run);
    assert_eq!(kind, ExitKind::Timeout);
    let pid: i32 = stdout_lines(&events)[0].parse().unwrap();
    wait_gone(pid);
}

#[test]
fn cancel_ends_the_group_and_a_child_ignoring_sigterm_is_killed_after_the_grace() {
    let (mut r, _) = runner();
    let run = r
        .start(sh(
            "t",
            "trap '' TERM; sleep 300 & echo $!; while :; do sleep 1; done",
            None,
        ))
        .unwrap();
    let pid: i32 = first_line(&mut r, run).parse().unwrap();
    assert!(r.cancel(run));
    let (_, kind, _, _) = until_exit(&mut r, run);
    assert_eq!(kind, ExitKind::Cancelled);
    wait_gone(pid);
    assert!(!r.cancel(run), "an ended run is no longer known");
}

#[test]
fn a_line_over_the_cap_is_dropped_and_counted_and_the_next_line_arrives() {
    let (mut r, _) = runner();
    let run = r
        .start(sh(
            "t",
            "head -c 1100000 /dev/zero | tr '\\0' x; printf '\\nafter\\n'",
            None,
        ))
        .unwrap();
    let (events, kind, lines, bytes) = until_exit(&mut r, run);
    assert_eq!(kind, ExitKind::Exited(0));
    assert_eq!(stdout_lines(&events), ["after"]);
    assert_eq!((lines, bytes), (1, 1_100_000));
}

#[test]
fn every_byte_is_either_delivered_or_counted_under_the_rate_cap() {
    let (mut r, _) = runner();
    // 6000 lines of 1023 characters: more than the 4 MiB the window admits.
    let script = "i=0; while [ $i -lt 6000 ]; do printf '%01023d\\n' 0; i=$((i+1)); done";
    let run = r.start(sh("t", script, None)).unwrap();
    let (events, kind, dropped_lines, dropped_bytes) = until_exit(&mut r, run);
    assert_eq!(kind, ExitKind::Exited(0));
    let delivered = stdout_lines(&events);
    assert_eq!(delivered.len() as u64 + dropped_lines, 6000);
    assert_eq!(
        delivered.iter().map(|l| l.len() as u64).sum::<u64>() + dropped_bytes,
        6000 * 1023
    );
    assert!(
        events.iter().all(|e| match e {
            TaskEvent::Output { lines, .. } =>
                lines.len() == 1 || lines.iter().map(String::len).sum::<usize>() <= 64 * 1024,
            _ => true,
        }),
        "a batch over 64 KiB holds more than one line"
    );
}

#[test]
fn a_program_that_cannot_start_is_a_run_that_failed_to_spawn() {
    let (mut r, _) = runner();
    let mut launch = sh("t", "", None);
    launch.program = "/nonexistent/learn".into();
    let run = r.start(launch).unwrap();
    let (_, kind, _, _) = until_exit(&mut r, run);
    assert!(matches!(kind, ExitKind::SpawnFailed(_)));
    let run = r.start_failed("t", &BTreeMap::new(), "not built".into()).unwrap();
    let (_, kind, _, _) = until_exit(&mut r, run);
    assert_eq!(kind, ExitKind::SpawnFailed("not built".into()));
}

#[test]
fn one_run_per_task_three_per_plugin_and_stop_all_ends_them() {
    let (mut r, _) = runner();
    let a = r.start(sh("a", "sleep 300", None)).unwrap();
    assert_eq!(
        r.start(sh("a", "sleep 300", None)),
        Err(StartError::TaskBusy("a".into()))
    );
    let b = r.start(sh("b", "sleep 300", None)).unwrap();
    let c = r.start(sh("c", "sleep 300", None)).unwrap();
    assert_eq!(r.start(sh("d", "sleep 300", None)), Err(StartError::TooManyRuns));
    assert_eq!(r.runs().iter().map(|i| i.run).collect::<Vec<_>>(), [a, b, c]);
    r.stop_all();
    // One poll may carry several runs' exits, so all three are collected together: `until_exit` stops at its
    // own run and would drop the others' events.
    let deadline = Instant::now() + HANG_GUARD;
    let mut ended = BTreeMap::new();
    while ended.len() < 3 {
        assert!(Instant::now() < deadline, "runs still alive: {:?}", r.runs());
        for e in r.poll(Instant::now() + Duration::from_secs(3600)).0 {
            if let TaskEvent::Exit { run, kind, .. } = e {
                ended.insert(run, kind);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!([a, b, c].iter().all(|run| ended[run] == ExitKind::Cancelled));
    assert!(r.waiter().finished());
}

#[test]
fn a_run_keeps_its_last_stdout_lines_for_a_returning_page() {
    let (mut r, _) = runner();
    let run = r
        .start(sh("watch", "echo first; echo second; exec sleep 300", None))
        .unwrap();
    let deadline = Instant::now() + HANG_GUARD;
    while r.runs()[0].tail.len() < 2 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    // Nobody polled: the tail holds the lines anyway.
    assert_eq!(r.runs()[0].tail, ["first", "second"]);
    assert!(r.cancel(run));
    until_exit(&mut r, run);
}

#[test]
fn an_idle_runner_is_never_woken_and_asks_for_no_timer() {
    let (mut r, wakes) = runner();
    let run = r.start(sh("t", "echo x", None)).unwrap();
    until_exit(&mut r, run);
    let after = wakes.load(Ordering::SeqCst);
    assert!(after >= 1, "output and exit raised the wake");
    for _ in 0..3 {
        let (events, next) = r.poll(Instant::now());
        assert!(events.is_empty());
        assert_eq!(next, None);
    }
    assert_eq!(wakes.load(Ordering::SeqCst), after);
}

#[test]
fn a_flood_of_empty_lines_is_bounded_like_any_other_output() {
    let (mut r, _) = runner();
    // Newlines only: two million lines, none with a byte of text.
    let script = "head -c 2000000 /dev/zero | tr '\\0' '\\n'";
    let run = r.start(sh("t", script, None)).unwrap();
    let (events, kind, dropped_lines, dropped_bytes) = until_exit(&mut r, run);
    assert_eq!(kind, ExitKind::Exited(0));
    let delivered = stdout_lines(&events).len() as u64;
    assert_eq!(delivered + dropped_lines, 2_000_000);
    assert_eq!(dropped_bytes, 0);
    assert!(dropped_lines > 0, "the window did not charge empty lines anything");
    assert!(delivered < 2_000_000);
}

#[test]
fn a_leader_that_exits_on_sigterm_does_not_cut_short_a_child_that_is_cleaning_up() {
    let (mut r, _) = runner();
    let dir = std::env::temp_dir().join(format!("eitri-tasks-grace-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("cleaned");
    let script = format!(
        "(trap 'sleep 0.5; echo ok > {m}; exit 0' TERM; echo ready; while :; do sleep 0.05; done) & \
         trap 'exit 0' TERM; wait",
        m = marker.display()
    );
    let run = r.start(sh("t", &script, None)).unwrap();
    assert_eq!(first_line(&mut r, run), "ready");
    assert!(r.cancel(run));
    let (_, kind, _, _) = until_exit(&mut r, run);
    assert_eq!(kind, ExitKind::Cancelled);
    let cleaned = marker.exists();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(cleaned, "the child was killed before its cleanup finished");
}

#[test]
fn a_writer_outside_the_group_finds_the_pipes_closed_once_the_run_has_ended() {
    let (mut r, _) = runner();
    // `setsid` puts the writer in a session of its own, so the group's end does not reach it; it keeps the
    // pipes open past the grace and writes after the run is over. With the readers released its write is a
    // broken pipe and it dies; with a reader left behind it would sleep on, filling a buffer nobody takes.
    let script = "setsid sh -c 'echo $$; sleep 3; echo late; sleep 300' & sleep 0.3";
    let run = r.start(sh("t", script, None)).unwrap();
    let (events, kind, _, _) = until_exit(&mut r, run);
    assert_eq!(kind, ExitKind::Exited(0));
    let pid: i32 = stdout_lines(&events)[0].parse().unwrap();
    assert!(r.is_idle());
    let deadline = Instant::now() + Duration::from_secs(20);
    while !gone(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let died_alone = gone(pid);
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    wait_gone(pid);
    assert!(died_alone, "the writer's output still had a reader after the run ended");
    let (events, next) = r.poll(Instant::now() + Duration::from_secs(3600));
    assert!(events.is_empty());
    assert_eq!(next, None);
}
