//! The turn lifecycle against real git: scripted events into a `TurnReview`, real snapshots in a
//! shadow under the target dir, and jobs held back on the worker to make every race happen on
//! purpose.

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use agent::{AgentDomainEvent, ResumeStatus, TurnOutcome};
use eitri_core::turn_review::{
    review_dir_for, JobEvent, NamedPaths, Origin, ReviewOptions, Scope, Snap, SnapshotKind, TurnRecord, TurnRef,
    TurnReview, TurnState,
};

/// A directory of the test's own under the target dir, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("turn_review_lifecycle")
            .join(format!(
                "{name}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let project = dir.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("main.rs"), "fn main() {}\n").unwrap();
        Scratch(dir)
    }

    fn project(&self) -> PathBuf {
        self.0.join("project")
    }

    fn state(&self) -> PathBuf {
        self.0.join("state")
    }

    fn review_dir(&self) -> PathBuf {
        review_dir_for(Some(self.state().as_os_str()), None, &self.project()).unwrap()
    }

    fn review(&self, jobs: &Jobs) -> TurnReview {
        TurnReview::with_options(Some(self.review_dir()), &self.project(), jobs.options())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(self.state(), std::fs::Permissions::from_mode(0o700));
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Watches the worker's jobs and holds back the ones a test names until it releases them.
#[derive(Clone, Default)]
struct Jobs {
    inner: Arc<(Mutex<JobsState>, Condvar)>,
    /// Slept on the worker before every end snapshot.
    slow_end: Option<Duration>,
}

#[derive(Default)]
struct JobsState {
    held: BTreeSet<(SnapshotKind, u32)>,
    seen: Vec<JobEvent>,
}

impl Jobs {
    fn options(&self) -> ReviewOptions {
        let jobs = self.clone();
        ReviewOptions {
            excludes: Some(None),
            on_job: Some(Arc::new(move |event: &JobEvent| jobs.on_job(event))),
            trim_every: Duration::MAX,
            ..ReviewOptions::default()
        }
    }

    fn on_job(&self, event: &JobEvent) {
        let (lock, cvar) = &*self.inner;
        let mut state = lock.lock().unwrap();
        state.seen.push(event.clone());
        cvar.notify_all();
        if let JobEvent::Starting { kind, position, .. } = event {
            while state.held.contains(&(*kind, *position)) {
                state = cvar.wait(state).unwrap();
            }
            drop(state);
            if *kind == SnapshotKind::End {
                if let Some(pause) = self.slow_end {
                    std::thread::sleep(pause);
                }
            }
        }
    }

    fn hold(&self, kind: SnapshotKind, position: u32) {
        self.inner.0.lock().unwrap().held.insert((kind, position));
    }

    fn release(&self, kind: SnapshotKind, position: u32) {
        let (lock, cvar) = &*self.inner;
        lock.lock().unwrap().held.remove(&(kind, position));
        cvar.notify_all();
    }

    /// Waits until the worker is parked on (or has started) the job.
    fn wait_started(&self, kind: SnapshotKind, position: u32) {
        let (lock, cvar) = &*self.inner;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut state = lock.lock().unwrap();
        while !state
            .seen
            .iter()
            .any(|e| matches!(e, JobEvent::Starting { kind: k, position: p, .. } if *k == kind && *p == position))
        {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "the worker never started {kind:?} {position}");
            state = cvar.wait_timeout(state, left).unwrap().0;
        }
    }

    fn seen(&self) -> Vec<JobEvent> {
        self.inner.0.lock().unwrap().seen.clone()
    }

    fn finished(&self, kind: SnapshotKind, position: u32) -> bool {
        self.seen()
            .iter()
            .any(|e| matches!(e, JobEvent::Finished { kind: k, position: p, .. } if *k == kind && *p == position))
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn opened(session: &str) -> AgentDomainEvent {
    AgentDomainEvent::SessionOpened {
        session_id: "local".into(),
        provider_session_id: session.into(),
        model: "m".into(),
        cwd: "/".into(),
    }
}

fn started(turn: &str) -> AgentDomainEvent {
    AgentDomainEvent::TurnStarted { turn_id: turn.into() }
}

fn tool(turn: &str) -> AgentDomainEvent {
    AgentDomainEvent::ToolCallStarted {
        turn_id: turn.into(),
        tool_use_id: format!("tu-{turn}"),
        name: "Write".into(),
        input: serde_json::json!({ "file_path": "main.rs", "content": "" }),
    }
}

fn completed(turn: &str) -> AgentDomainEvent {
    AgentDomainEvent::TurnCompleted {
        turn_id: turn.into(),
        outcome: TurnOutcome::Completed,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
        detail: Default::default(),
    }
}

/// Polls until `done` holds for the session's turns, or fails at 5 s.
fn poll_until(review: &mut TurnReview, session: &str, what: &str, done: impl Fn(&[TurnRecord]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        review.poll();
        if done(&review.turns(session)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {:#?}",
            review.turns(session)
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn turn<'a>(turns: &'a [TurnRecord], id: &str) -> &'a TurnRecord {
    turns
        .iter()
        .find(|t| t.turn_id == id)
        .unwrap_or_else(|| panic!("no turn {id}: {turns:#?}"))
}

fn has_base(turns: &[TurnRecord], id: &str) -> bool {
    turns
        .iter()
        .any(|t| t.turn_id == id && matches!(t.base, Snap::Taken { .. }))
}

fn is_ok(turns: &[TurnRecord], id: &str) -> bool {
    turns.iter().any(|t| t.turn_id == id && t.state() == TurnState::Ok)
}

#[test]
fn events_enqueue_and_never_block() {
    let scratch = Scratch::new("never-block");
    let jobs = Jobs::default();
    jobs.hold(SnapshotKind::Base, 1);
    let mut review = scratch.review(&jobs);
    review.observe(1, "s1", &opened("s1"), now());
    review.observe(1, "s1", &started("t1"), now());
    jobs.wait_started(SnapshotKind::Base, 1);

    // The observing runs on a thread of its own, so a blocked `observe` fails the test at the
    // deadline instead of hanging it.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut took = Vec::new();
        for i in 0..200 {
            let event = if i % 4 == 0 {
                AgentDomainEvent::ToolCallStarted {
                    turn_id: "t1".into(),
                    tool_use_id: format!("tu-{i}"),
                    name: "Bash".into(),
                    input: serde_json::json!({ "command": "true" }),
                }
            } else {
                AgentDomainEvent::ContentDelta {
                    turn_id: "t1".into(),
                    kind: agent::ContentKind::Text,
                    text: "x".into(),
                }
            };
            let at = Instant::now();
            review.observe(1, "s1", &event, now());
            took.push(at.elapsed());
        }
        let at = Instant::now();
        review.poll();
        review.turns("s1");
        took.push(at.elapsed());
        tx.send((review, took)).unwrap();
    });
    let (mut review, mut took) = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("observe blocked while a snapshot was held back");
    assert!(
        !jobs.finished(SnapshotKind::Base, 1),
        "every observe returned while the base was still parked"
    );
    took.sort();
    let median = took[took.len() / 2];
    let slowest = *took.last().unwrap();
    assert!(median < Duration::from_millis(1), "median observe {median:?}");
    assert!(slowest < Duration::from_millis(20), "slowest observe {slowest:?}");

    jobs.release(SnapshotKind::Base, 1);
    poll_until(&mut review, "s1", "the base", |t| has_base(t, "t1"));
}

#[test]
fn a_tool_call_before_the_base_marks_the_turn_late() {
    let scratch = Scratch::new("late");
    let jobs = Jobs::default();
    jobs.hold(SnapshotKind::Base, 1);
    let mut review = scratch.review(&jobs);
    review.observe(1, "s1", &started("t1"), now());
    jobs.wait_started(SnapshotKind::Base, 1);
    review.poll();
    review.observe(1, "s1", &tool("t1"), now());
    jobs.release(SnapshotKind::Base, 1);
    poll_until(&mut review, "s1", "the base", |t| has_base(t, "t1"));
    review.observe(1, "s1", &completed("t1"), now());
    poll_until(&mut review, "s1", "both snapshots", |t| is_ok(t, "t1"));
    let turns = review.turns("s1");
    let t1 = turn(&turns, "t1");
    assert!(t1.late, "{t1:#?}");
    assert_eq!(t1.n, 1);

    let overview = review
        .overview_job("s1", TurnRef::Latest, Scope::Turn, NamedPaths::default())
        .run()
        .unwrap();
    assert!(
        overview
            .notes
            .iter()
            .any(|n| n.starts_with("baseline late: changes made before")),
        "{:?}",
        overview.notes
    );
    assert_eq!(overview.notes[0], "changed on disk during this turn");
}

#[test]
fn a_tool_call_after_the_base_is_not_late() {
    let scratch = Scratch::new("not-late");
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    review.observe(1, "s1", &started("t1"), now());
    poll_until(&mut review, "s1", "the base", |t| has_base(t, "t1"));
    review.observe(1, "s1", &tool("t1"), now());
    review.observe(1, "s1", &completed("t1"), now());
    poll_until(&mut review, "s1", "both snapshots", |t| is_ok(t, "t1"));
    let turns = review.turns("s1");
    assert!(!turn(&turns, "t1").late);
}

#[test]
fn an_end_still_running_when_the_next_turn_asks_marks_both() {
    let scratch = Scratch::new("overlap-next");
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    review.observe(1, "s1", &started("t1"), now());
    poll_until(&mut review, "s1", "the first base", |t| has_base(t, "t1"));
    jobs.hold(SnapshotKind::End, 1);
    review.observe(1, "s1", &completed("t1"), now());
    jobs.wait_started(SnapshotKind::End, 1);
    review.observe(1, "s1", &started("t2"), now());
    review.poll();
    review.observe(1, "s1", &tool("t2"), now());
    jobs.release(SnapshotKind::End, 1);
    poll_until(&mut review, "s1", "the first end and the second base", |t| {
        is_ok(t, "t1") && has_base(t, "t2")
    });
    let turns = review.turns("s1");
    let (t1, t2) = (turn(&turns, "t1"), turn(&turns, "t2"));
    assert!(t1.overlapped_next, "{t1:#?}");
    assert!(t2.late, "{t2:#?}");
    assert!(!t1.late);
    assert_eq!((t1.n, t2.n), (1, 2));

    let overview = review
        .overview_job("s1", TurnRef::N(1), Scope::Turn, NamedPaths::default())
        .run()
        .unwrap();
    assert!(overview
        .notes
        .contains(&"may include the next turn's first changes".to_string()));
}

#[test]
fn a_turn_ended_by_session_closed_gets_an_end() {
    let endings = [
        AgentDomainEvent::SessionClosed { reason: "bye".into() },
        AgentDomainEvent::SessionUnavailable { reason: "gone".into() },
        AgentDomainEvent::ResumeOutcome {
            requested_provider_session_id: "s1".into(),
            status: ResumeStatus::Rejected,
            attached_provider_session_id: None,
            forked: false,
            detail: None,
        },
    ];
    for ending in endings {
        let scratch = Scratch::new("ended");
        let jobs = Jobs::default();
        let mut review = scratch.review(&jobs);
        review.observe(1, "s1", &started("t1"), now());
        poll_until(&mut review, "s1", "the base", |t| has_base(t, "t1"));
        review.observe(1, "s1", &ending, now());
        poll_until(&mut review, "s1", "the end", |t| is_ok(t, "t1"));
    }

    // A resume that did continue is no ending.
    let scratch = Scratch::new("attached");
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    review.observe(1, "s1", &started("t1"), now());
    review.observe(
        1,
        "s1",
        &AgentDomainEvent::ResumeOutcome {
            requested_provider_session_id: "s1".into(),
            status: ResumeStatus::Attached,
            attached_provider_session_id: Some("s1".into()),
            forked: false,
            detail: None,
        },
        now(),
    );
    poll_until(&mut review, "s1", "the base", |t| has_base(t, "t1"));
    std::thread::sleep(Duration::from_millis(100));
    review.poll();
    assert!(!jobs.seen().iter().any(|e| matches!(
        e,
        JobEvent::Starting {
            kind: SnapshotKind::End,
            ..
        }
    )));
    assert_eq!(turn(&review.turns("s1"), "t1").end, Snap::Pending);
}

#[test]
fn end_then_base_run_in_order_on_one_index() {
    let scratch = Scratch::new("order");
    let jobs = Jobs {
        slow_end: Some(Duration::from_millis(200)),
        ..Jobs::default()
    };
    let mut review = scratch.review(&jobs);
    review.observe(1, "s1", &started("t1"), now());
    review.observe(1, "s1", &completed("t1"), now());
    review.observe(1, "s1", &started("t2"), now());
    poll_until(&mut review, "s1", "the second base", |t| {
        is_ok(t, "t1") && has_base(t, "t2")
    });
    let seen = jobs.seen();
    let at = |want: &dyn Fn(&JobEvent) -> bool| seen.iter().position(want).unwrap();
    let end_finished = at(&|e| {
        matches!(
            e,
            JobEvent::Finished {
                kind: SnapshotKind::End,
                position: 1,
                ..
            }
        )
    });
    let base_started = at(&|e| {
        matches!(
            e,
            JobEvent::Starting {
                kind: SnapshotKind::Base,
                position: 2,
                ..
            }
        )
    });
    assert!(end_finished < base_started, "{seen:#?}");
    // One session, one index file.
    let indexes: Vec<_> = std::fs::read_dir(scratch.review_dir())
        .unwrap()
        .filter_map(|e| e.unwrap().file_name().into_string().ok())
        .filter(|n| n.starts_with("index-") && !n.ends_with(".guard") && !n.ends_with(".lock"))
        .collect();
    assert_eq!(indexes, vec!["index-s1".to_string()]);
}

#[test]
fn an_end_with_both_snapshots_gives_a_hint_even_for_no_files() {
    let scratch = Scratch::new("hints");
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    let mut hints = Vec::new();
    review.observe(3, "s1", &started("t1"), now());
    poll_until(&mut review, "s1", "the base", |t| has_base(t, "t1"));
    std::fs::write(scratch.project().join("new.txt"), "hello\n").unwrap();
    review.observe(3, "s1", &completed("t1"), now());
    let deadline = Instant::now() + Duration::from_secs(5);
    while hints.is_empty() {
        assert!(Instant::now() < deadline, "no hint");
        hints.extend(review.poll());
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(hints[0].tab, 3);
    assert_eq!(hints[0].turn, 1);
    assert_eq!(hints[0].files, 1);

    hints.clear();
    review.observe(3, "s1", &started("t2"), now());
    review.observe(3, "s1", &completed("t2"), now());
    let deadline = Instant::now() + Duration::from_secs(5);
    while hints.is_empty() {
        assert!(Instant::now() < deadline, "no hint");
        hints.extend(review.poll());
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        (hints[0].turn, hints[0].files),
        (2, 0),
        "a turn that changed nothing clears the band"
    );
}

#[test]
fn turns_of_an_earlier_run_come_back_from_refs() {
    let scratch = Scratch::new("earlier-run");
    let jobs = Jobs::default();
    let first_base;
    {
        let mut run1 = scratch.review(&jobs);
        run1.observe(1, "s1", &started("t1"), now());
        poll_until(&mut run1, "s1", "the base", |t| has_base(t, "t1"));
        std::fs::write(scratch.project().join("a.txt"), "one\n").unwrap();
        run1.observe(1, "s1", &completed("t1"), now());
        run1.observe(1, "s1", &started("t2"), now());
        poll_until(&mut run1, "s1", "turn 1 and the second base", |t| {
            is_ok(t, "t1") && has_base(t, "t2")
        });
        first_base = turn(&run1.turns("s1"), "t1").base.clone();
        // Eitri exits mid-turn: turn 2 never gets its end.
    }

    let mut run2 = scratch.review(&jobs);
    run2.observe(1, "s1", &started("t3"), now());
    run2.observe(1, "s1", &completed("t3"), now());
    poll_until(&mut run2, "s1", "the new turn", |t| is_ok(t, "t3"));
    assert_eq!(
        turn(&run2.turns("s1"), "t3").n,
        3,
        "numbers continue after the earlier run's"
    );

    let job = run2.overview_job("s1", TurnRef::N(1), Scope::Turn, NamedPaths::default());
    let overview = std::thread::spawn(move || job.run()).join().unwrap().unwrap();
    let listed: Vec<(u32, &str, bool)> = overview
        .turns
        .iter()
        .map(|t| (t.n, t.state().as_str(), t.earlier_run))
        .collect();
    assert_eq!(listed, vec![(1, "ok", true), (2, "unfinished", true), (3, "ok", false)]);
    assert_eq!(
        overview.turns[0].base, first_base,
        "run 2 did not overwrite turn 1's base"
    );
    assert_eq!(overview.turns[0].turn_id, "t1");
    assert_eq!(overview.current, 1);
    let files: Vec<(&Path, Origin)> = overview.files.iter().map(|f| (f.path.as_path(), f.origin)).collect();
    assert_eq!(files, vec![(Path::new("a.txt"), Origin::Workspace)]);

    let job = run2.overview_job("s1", TurnRef::N(2), Scope::Turn, NamedPaths::default());
    let overview = std::thread::spawn(move || job.run()).join().unwrap().unwrap();
    assert!(
        overview
            .notes
            .contains(&"this turn did not finish while Eitri was running".to_string()),
        "{:?}",
        overview.notes
    );
}

/// What a turn's review must say about its own precision survives a restart: a late baseline (and
/// when it was taken), an overlap with the next turn learnt only after the turn's end was asked
/// for, an overlap with another tab, and a file left out of the end snapshot for its size -- which,
/// forgotten, would read as a file the turn deleted.
#[test]
fn an_earlier_runs_turn_keeps_its_marks_and_its_left_out_files() {
    let scratch = Scratch::new("earlier-marks");
    let big = scratch.project().join("big.bin");
    std::fs::write(&big, "small\n").unwrap();
    let jobs = Jobs::default();
    let options = || {
        let mut options = jobs.options();
        options.limits.max_file_bytes = 64;
        options
    };
    {
        let mut run1 = TurnReview::with_options(Some(scratch.review_dir()), &scratch.project(), options());
        jobs.hold(SnapshotKind::Base, 1);
        run1.observe(1, "s1", &started("t1"), now());
        run1.observe(1, "s1", &tool("t1"), now());
        jobs.release(SnapshotKind::Base, 1);
        poll_until(&mut run1, "s1", "the first base", |t| has_base(t, "t1"));
        std::fs::write(&big, vec![b'x'; 200]).unwrap();
        jobs.hold(SnapshotKind::End, 1);
        run1.observe(1, "s1", &completed("t1"), now());
        run1.observe(1, "s1", &started("t2"), now());
        // Turn 1's end is still waiting when turn 2's first tool starts.
        run1.observe(1, "s1", &tool("t2"), now());
        run1.observe(2, "s2", &started("b1"), now());
        jobs.release(SnapshotKind::End, 1);
        run1.observe(1, "s1", &completed("t2"), now());
        run1.observe(2, "s2", &completed("b1"), now());
        poll_until(&mut run1, "s1", "both turns", |t| is_ok(t, "t1") && is_ok(t, "t2"));
        poll_until(&mut run1, "s2", "the other tab's turn", |t| is_ok(t, "b1"));
        let seen = run1.turns("s1");
        let (one, two) = (turn(&seen, "t1"), turn(&seen, "t2"));
        assert!(one.late && one.overlapped_next && !one.overlapped_tab, "{one:#?}");
        assert!(two.late && two.overlapped_tab, "{two:#?}");
        assert_eq!(one.skipped_large, vec![PathBuf::from("big.bin")]);
        // Everything handed to the worker is done once the session is no longer tracked.
        run1.close_tab(1, now());
        run1.close_tab(2, now());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !run1.turns("s1").is_empty() {
            assert!(Instant::now() < deadline, "the worker never finished");
            run1.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    let run2 = TurnReview::with_options(Some(scratch.review_dir()), &scratch.project(), options());
    let job = run2.overview_job("s1", TurnRef::N(1), Scope::Turn, NamedPaths::default());
    let overview = std::thread::spawn(move || job.run()).join().unwrap().unwrap();
    let flags: Vec<(u32, bool, bool, bool, bool)> = overview
        .turns
        .iter()
        .map(|t| (t.n, t.earlier_run, t.late, t.overlapped_next, t.overlapped_tab))
        .collect();
    assert_eq!(flags, vec![(1, true, true, true, false), (2, true, true, false, true)]);
    assert!(overview.turns[0].base_taken_ms.is_some(), "{:#?}", overview.turns[0]);
    assert!(
        overview
            .notes
            .iter()
            .any(|n| n.starts_with("baseline late: changes made before ")),
        "{:?}",
        overview.notes
    );
    assert!(
        overview
            .notes
            .contains(&"may include the next turn's first changes".to_string()),
        "{:?}",
        overview.notes
    );
    let big_row = overview
        .files
        .iter()
        .find(|f| f.path == Path::new("big.bin"))
        .unwrap_or_else(|| panic!("big.bin is not listed: {:#?}", overview.files));
    assert!(big_row.too_large, "listed as too large, not as deleted: {big_row:#?}");
    assert_eq!((big_row.added, big_row.removed), (0, 0));
    let diff = run2.diff_job("s1", 1, Scope::Turn, "big.bin");
    let refused = std::thread::spawn(move || diff.run()).join().unwrap();
    assert!(
        matches!(refused, Err(eitri_core::turn_review::ReviewError::TooLarge(_))),
        "{refused:?}"
    );

    let job = run2.overview_job("s1", TurnRef::N(2), Scope::Turn, NamedPaths::default());
    let overview = std::thread::spawn(move || job.run()).join().unwrap().unwrap();
    assert!(
        overview
            .notes
            .contains(&"this turn overlapped another tab's turn".to_string()),
        "{:?}",
        overview.notes
    );
}

#[test]
fn turns_never_runs_git() {
    let scratch = Scratch::new("no-git");
    std::fs::create_dir_all(scratch.state()).unwrap();
    std::fs::set_permissions(scratch.state(), std::fs::Permissions::from_mode(0o000)).unwrap();
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    review.observe(1, "s1", &started("t1"), now());
    let at = Instant::now();
    let _ = review.turns("s1");
    assert!(at.elapsed() < Duration::from_millis(20));
    poll_until(&mut review, "s1", "the failed base", |t| {
        t.iter().any(|t| matches!(t.state(), TurnState::NoBaseline(_)))
    });
    let turns = review.turns("s1");
    let t1 = turn(&turns, "t1");
    assert!(t1.state().reason().is_some_and(|r| !r.is_empty()));
}

#[test]
fn two_tabs_overlapping_are_marked() {
    let scratch = Scratch::new("two-tabs");
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    review.observe(1, "s1", &started("a1"), now());
    review.observe(2, "s2", &started("b1"), now());
    review.observe(1, "s1", &completed("a1"), now());
    review.observe(2, "s2", &completed("b1"), now());
    // Two pumps later: an end observed in the pump before may still have come after a start.
    review.poll();
    review.poll();
    review.observe(1, "s1", &started("a2"), now());
    review.observe(1, "s1", &completed("a2"), now());
    poll_until(&mut review, "s1", "tab 1's turns", |t| is_ok(t, "a1") && is_ok(t, "a2"));
    poll_until(&mut review, "s2", "tab 2's turn", |t| is_ok(t, "b1"));
    let one = review.turns("s1");
    let two = review.turns("s2");
    assert!(turn(&one, "a1").overlapped_tab);
    assert!(turn(&two, "b1").overlapped_tab);
    assert!(!turn(&one, "a2").overlapped_tab, "started after both had ended");
}

/// The pump drains tab after tab, so one tab's end and another's start that reach the same pump,
/// or neighbouring pumps, are in no known order: either may have come first.
#[test]
fn an_overlap_the_drain_order_hides_is_still_marked() {
    // Tab 1 drained first: its end is observed before tab 2's start in the same pump.
    let scratch = Scratch::new("drain-order");
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    review.poll();
    review.observe(1, "s1", &started("a1"), now());
    review.poll();
    review.observe(1, "s1", &completed("a1"), now());
    review.observe(2, "s2", &started("b1"), now());
    review.observe(2, "s2", &completed("b1"), now());
    poll_until(&mut review, "s1", "tab 1's turn", |t| is_ok(t, "a1"));
    poll_until(&mut review, "s2", "tab 2's turn", |t| is_ok(t, "b1"));
    assert!(turn(&review.turns("s1"), "a1").overlapped_tab);
    assert!(turn(&review.turns("s2"), "b1").overlapped_tab);

    // Tab 2 drained after tab 1: its end, observed in one pump, may have come after tab 1's start,
    // observed in the next.
    let scratch = Scratch::new("drain-order-next");
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    review.poll();
    review.observe(2, "s2", &started("b1"), now());
    review.poll();
    review.observe(2, "s2", &completed("b1"), now());
    review.poll();
    review.observe(1, "s1", &started("a1"), now());
    review.observe(1, "s1", &completed("a1"), now());
    poll_until(&mut review, "s1", "tab 1's turn", |t| is_ok(t, "a1"));
    poll_until(&mut review, "s2", "tab 2's turn", |t| is_ok(t, "b1"));
    assert!(turn(&review.turns("s1"), "a1").overlapped_tab);
    assert!(turn(&review.turns("s2"), "b1").overlapped_tab);
}

/// A tab whose session ended starts another (`r`, then a prompt). The new session reports its id
/// only after its first turn has started: that turn is filed under the new session, never under
/// the one that ended, however the tab learned of the end.
#[test]
fn a_tab_that_starts_a_new_session_files_its_first_turn_under_it() {
    // (the old session closed, the tab had no backend for a pump) / (it closed, and the next
    // backend came up within one pump) / (no end was seen at all)
    for (closed, no_backend) in [(true, true), (true, false), (false, false)] {
        let what = format!("closed: {closed}, a pump without a backend: {no_backend}");
        let scratch = Scratch::new("new-session");
        let jobs = Jobs::default();
        let mut review = scratch.review(&jobs);
        review.observe(1, "s1", &opened("s1"), now());
        review.observe(1, "s1", &started("t1"), now());
        review.observe(1, "s1", &completed("t1"), now());
        poll_until(&mut review, "s1", "the first session's turn", |t| is_ok(t, "t1"));
        if closed {
            review.observe(
                1,
                "s1",
                &AgentDomainEvent::SessionClosed { reason: "bye".into() },
                now(),
            );
        }
        if no_backend {
            review.tab_has_no_session(1, now());
        }

        // The new backend has no id yet.
        review.observe(1, "", &started("n1"), now());
        review.observe(1, "", &tool("n1"), now());
        review.observe(1, "s2", &opened("s2"), now());
        review.observe(1, "s2", &tool("n1"), now());
        review.observe(1, "s2", &completed("n1"), now());
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut hints = Vec::new();
        while !is_ok(&review.turns("s2"), "n1") {
            assert!(Instant::now() < deadline, "{what}: {:#?}", review.turns("s2"));
            hints.extend(review.poll());
            std::thread::sleep(Duration::from_millis(5));
        }
        hints.extend(review.poll());

        let turns = review.turns("s2");
        assert_eq!(turns.len(), 1, "{what}: {turns:#?}");
        let n1 = turn(&turns, "n1");
        assert_eq!((n1.n, n1.session.as_str()), (1, "s2"), "{what}");
        assert!(n1.late, "{what}: its tool call came before its base");
        assert!(
            hints.iter().any(|h| h.tab == 1 && h.turn == 1),
            "{what}: the new turn's end gives its hint: {hints:?}"
        );
        // The old session keeps only its own turn, in memory and in the shadow.
        assert!(
            review.turns("s1").iter().all(|t| t.turn_id == "t1"),
            "{what}: {:#?}",
            review.turns("s1")
        );
        let old = review
            .overview_job("s1", TurnRef::Latest, Scope::Session, NamedPaths::default())
            .run()
            .unwrap();
        let ids: Vec<&str> = old.turns.iter().map(|t| t.turn_id.as_str()).collect();
        assert_eq!(ids, vec!["t1"], "{what}");
    }
}

/// A resync can read a turn off the projection while that turn's start is still queued for the
/// next pump, which then delivers it again. It is one turn, with one base.
#[test]
fn a_turn_seen_by_a_resync_is_not_started_again_by_its_queued_start() {
    let scratch = Scratch::new("resync-then-start");
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    review.observe_resync(1, "s1", Some("t1"), now());
    review.poll();
    review.observe(1, "s1", &started("t1"), now());
    review.observe(1, "s1", &tool("t1"), now());
    review.observe(1, "s1", &completed("t1"), now());
    poll_until(&mut review, "s1", "the turn", |t| is_ok(t, "t1"));
    let turns = review.turns("s1");
    assert_eq!(turns.len(), 1, "{turns:#?}");
    assert!(!turns[0].overlapped_next, "{turns:#?}");
    let bases = jobs
        .seen()
        .iter()
        .filter(|e| {
            matches!(
                e,
                JobEvent::Starting {
                    kind: SnapshotKind::Base,
                    ..
                }
            )
        })
        .count();
    assert_eq!(bases, 1);
}

/// A tool names a file through a directory link inside the project; the snapshot lists the file
/// where it really is. The row is the real path's, credited to the agent.
#[test]
fn a_file_named_through_a_directory_link_is_attributed_where_it_is() {
    let scratch = Scratch::new("dir-link");
    let project = scratch.project();
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(project.join("src/a.rs"), "a\n").unwrap();
    std::os::unix::fs::symlink("src", project.join("link")).unwrap();
    let outside = scratch.0.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, project.join("out")).unwrap();
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    review.observe(1, "s1", &started("t1"), now());
    poll_until(&mut review, "s1", "the base", |t| has_base(t, "t1"));
    std::fs::write(project.join("link/a.rs"), "a\nb\n").unwrap();
    std::fs::write(outside.join("x.rs"), "x\n").unwrap();
    review.observe(1, "s1", &completed("t1"), now());
    poll_until(&mut review, "s1", "the end", |t| is_ok(t, "t1"));

    let named = NamedPaths {
        paths: [project.join("link/a.rs"), project.join("out/x.rs")]
            .into_iter()
            .collect(),
        pending_no_result: 0,
    };
    let overview = review
        .overview_job("s1", TurnRef::Latest, Scope::Turn, named)
        .run()
        .unwrap();
    let rows: Vec<(&Path, Origin)> = overview.files.iter().map(|f| (f.path.as_path(), f.origin)).collect();
    assert_eq!(
        rows,
        vec![(Path::new("src/a.rs"), Origin::Agent)],
        "a file outside the project, reached through a link, has no row"
    );
}

#[test]
fn the_first_turn_of_a_new_session_waits_for_its_session_id() {
    let scratch = Scratch::new("waits-for-id");
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    review.observe(1, "", &started("t1"), now());
    review.observe(1, "", &tool("t1"), now());
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        jobs.seen().is_empty(),
        "nothing can be snapshotted before the session has an id"
    );
    review.observe(1, "", &opened("s1"), now());
    review.observe(1, "s1", &opened("s1"), now());
    poll_until(&mut review, "s1", "the base", |t| has_base(t, "t1"));
    let starts: Vec<(SnapshotKind, u32)> = jobs
        .seen()
        .iter()
        .filter_map(|e| match e {
            JobEvent::Starting { kind, position, .. } => Some((*kind, *position)),
            _ => None,
        })
        .collect();
    assert_eq!(starts, vec![(SnapshotKind::Base, 1)], "one base and no warm-up");
    let turns = review.turns("s1");
    let t1 = turn(&turns, "t1");
    assert!(t1.late);
    assert_eq!(t1.session, "s1");
}

#[test]
fn a_session_opened_before_any_turn_warms_the_index() {
    let scratch = Scratch::new("warm");
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    review.observe(1, "", &opened("s1"), now());
    review.observe(1, "s1", &opened("s1"), now());
    let deadline = Instant::now() + Duration::from_secs(5);
    while !jobs.finished(SnapshotKind::Warm, 0) {
        assert!(Instant::now() < deadline, "no warm-up");
        std::thread::sleep(Duration::from_millis(5));
    }
    let warms = jobs
        .seen()
        .iter()
        .filter(|e| {
            matches!(
                e,
                JobEvent::Starting {
                    kind: SnapshotKind::Warm,
                    ..
                }
            )
        })
        .count();
    assert_eq!(warms, 1, "a session reports itself every turn; it is warmed once");
}

#[test]
fn a_resync_that_swallowed_the_end_still_ends_the_turn() {
    let scratch = Scratch::new("resync");
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    review.observe(1, "s1", &started("t1"), now());
    poll_until(&mut review, "s1", "the base", |t| has_base(t, "t1"));
    review.observe_resync(1, "s1", None, now());
    poll_until(&mut review, "s1", "the end", |t| is_ok(t, "t1"));

    // The dropped events held the next turn's start too: it is recorded, late, and the turn
    // before it may hold its first changes.
    review.observe(1, "s1", &started("t2"), now());
    poll_until(&mut review, "s1", "the second base", |t| has_base(t, "t2"));
    review.observe_resync(1, "s1", Some("t3"), now());
    poll_until(&mut review, "s1", "the third base", |t| {
        is_ok(t, "t2") && has_base(t, "t3")
    });
    let turns = review.turns("s1");
    assert!(turn(&turns, "t2").overlapped_next);
    assert!(turn(&turns, "t3").late);
    // A resync that changed nothing about the turn adds nothing.
    review.observe_resync(1, "s1", Some("t3"), now());
    assert_eq!(review.turns("s1").len(), 3);
}

#[test]
fn a_closed_tab_ends_its_turn() {
    let scratch = Scratch::new("closed-tab");
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    review.observe(1, "s1", &started("t1"), now());
    poll_until(&mut review, "s1", "the base", |t| has_base(t, "t1"));
    review.close_tab(1, now());
    let deadline = Instant::now() + Duration::from_secs(5);
    while !jobs.finished(SnapshotKind::End, 1) {
        assert!(Instant::now() < deadline, "the closed tab's turn got no end");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_turn_without_a_baseline_shows_no_diff() {
    let scratch = Scratch::new("no-baseline");
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    // An id the shadow cannot use as a file name gives the turn no snapshot at all.
    review.observe(1, "bad/id", &started("t1"), now());
    review.observe(1, "bad/id", &completed("t1"), now());
    poll_until(&mut review, "bad/id", "the failed base", |t| {
        t.iter().any(|t| matches!(t.state(), TurnState::NoBaseline(_)))
    });
    let overview = review
        .overview_job("bad/id", TurnRef::Latest, Scope::Turn, NamedPaths::default())
        .run()
        .unwrap();
    assert!(overview.files.is_empty());
    assert!(
        overview.notes.iter().any(|n| n.starts_with("no baseline: ")),
        "{:?}",
        overview.notes
    );
    let refused = review.diff_job("bad/id", 1, Scope::Turn, "main.rs").run();
    assert!(
        matches!(refused, Err(eitri_core::turn_review::ReviewError::NoBaseline(_))),
        "{refused:?}"
    );
}

#[test]
fn the_overview_attributes_and_the_diff_has_hunks() {
    let scratch = Scratch::new("attribute");
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    review.observe(1, "s1", &started("t1"), now());
    poll_until(&mut review, "s1", "the base", |t| has_base(t, "t1"));
    let project = scratch.project();
    std::fs::write(project.join("main.rs"), "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
    std::fs::write(project.join("by-bash.txt"), "x\n").unwrap();
    review.observe(1, "s1", &completed("t1"), now());
    poll_until(&mut review, "s1", "the end", |t| is_ok(t, "t1"));

    let named = NamedPaths {
        paths: [project.join("main.rs"), project.join("same.rs")].into_iter().collect(),
        pending_no_result: 1,
    };
    let overview = review
        .overview_job("s1", TurnRef::Latest, Scope::Turn, named)
        .run()
        .unwrap();
    let rows: Vec<(&Path, Origin)> = overview.files.iter().map(|f| (f.path.as_path(), f.origin)).collect();
    assert_eq!(
        rows,
        vec![
            (Path::new("by-bash.txt"), Origin::Workspace),
            (Path::new("main.rs"), Origin::Agent),
            (Path::new("same.rs"), Origin::AgentOnly),
        ]
    );
    assert_eq!(overview.pending_no_result, 1);

    let diff = review.diff_job("s1", 1, Scope::Turn, "main.rs").run().unwrap();
    let hunks = diff.hunks.expect("a short patch is shown");
    assert_eq!(hunks.len(), 1);
    assert_eq!((diff.added, diff.removed), (3, 1));

    // The session scope runs from the first base to the latest end.
    let session = review
        .overview_job("s1", TurnRef::Latest, Scope::Session, NamedPaths::default())
        .run()
        .unwrap();
    assert_eq!(session.files.len(), 2);
    assert_eq!(session.notes[0], "changed on disk during this session");
}

#[test]
fn a_long_patch_is_refused_with_its_counts() {
    let scratch = Scratch::new("long-patch");
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    review.observe(1, "s1", &started("t1"), now());
    poll_until(&mut review, "s1", "the base", |t| has_base(t, "t1"));
    let long: String = (0..2500).map(|i| format!("line {i}\n")).collect();
    std::fs::write(scratch.project().join("long.txt"), long).unwrap();
    review.observe(1, "s1", &completed("t1"), now());
    poll_until(&mut review, "s1", "the end", |t| is_ok(t, "t1"));
    let diff = review.diff_job("s1", 1, Scope::Turn, "long.txt").run().unwrap();
    assert_eq!(diff.hunks, None);
    assert_eq!((diff.added, diff.removed), (2500, 0));
}

/// Two files whose names read the same on the wire (one not valid UTF-8, one with the replacement
/// character itself) must not be answered with each other's patch: neither is served while both
/// are in the review, and a name that is unambiguous still is.
#[test]
fn a_path_that_reads_like_another_files_name_gets_no_patch() {
    use std::os::unix::ffi::OsStrExt;
    let scratch = Scratch::new("lossy-path");
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    review.observe(1, "s1", &started("t1"), now());
    poll_until(&mut review, "s1", "the base", |t| has_base(t, "t1"));
    let project = scratch.project();
    let raw = project.join(std::ffi::OsStr::from_bytes(b"file-\xFF.txt"));
    std::fs::write(&raw, "raw bytes name\n").unwrap();
    std::fs::write(project.join("file-\u{FFFD}.txt"), "real replacement character\n").unwrap();
    std::fs::write(project.join("plain.txt"), "plain\n").unwrap();
    review.observe(1, "s1", &completed("t1"), now());
    poll_until(&mut review, "s1", "the end", |t| is_ok(t, "t1"));

    let lossy = "file-\u{FFFD}.txt";
    let both = review.diff_job("s1", 1, Scope::Turn, lossy).run();
    assert!(
        matches!(both, Err(eitri_core::turn_review::ReviewError::AmbiguousPath(_))),
        "{both:?}"
    );
    let plain = review.diff_job("s1", 1, Scope::Turn, "plain.txt").run().unwrap();
    assert_eq!((plain.added, plain.removed), (1, 0));

    // A later turn that touches only the genuine replacement-character name serves it.
    review.observe(1, "s1", &started("t2"), now());
    poll_until(&mut review, "s1", "the second base", |t| has_base(t, "t2"));
    std::fs::write(project.join("file-\u{FFFD}.txt"), "changed again\n").unwrap();
    review.observe(1, "s1", &completed("t2"), now());
    poll_until(&mut review, "s1", "the second end", |t| is_ok(t, "t2"));
    let alone = review.diff_job("s1", 2, Scope::Turn, lossy).run().unwrap();
    assert_eq!(alone.path, Path::new(lossy));
    assert!(alone.hunks.is_some());
}

// ---- files too large to snapshot -------------------------------------------------------------

/// A review whose per-file limit is 64 bytes, so a 200-byte file is "too large".
fn small_limit_review(scratch: &Scratch, jobs: &Jobs) -> TurnReview {
    let mut options = jobs.options();
    options.limits.max_file_bytes = 64;
    TurnReview::with_options(Some(scratch.review_dir()), &scratch.project(), options)
}

const LARGE: usize = 200;

/// Runs one whole turn: `act` runs between its two snapshots.
fn run_turn(review: &mut TurnReview, id: &str, act: impl FnOnce()) {
    review.observe(1, "s1", &started(id), now());
    review.observe(1, "s1", &tool(id), now());
    poll_until(review, "s1", "the base", |t| has_base(t, id));
    act();
    review.observe(1, "s1", &completed(id), now());
    poll_until(review, "s1", "the end", |t| is_ok(t, id));
}

/// The rows of a review of `turn` as `(path, too_large)`.
fn rows(review: &TurnReview, turn: TurnRef, scope: Scope) -> Vec<(String, bool)> {
    let job = review.overview_job("s1", turn, scope, NamedPaths::default());
    let overview = std::thread::spawn(move || job.run()).join().unwrap().unwrap();
    overview
        .files
        .iter()
        .map(|f| (f.path.display().to_string(), f.too_large))
        .collect()
}

/// Lets the file system's clock move on, so a rewrite within a turn cannot carry the time of the
/// base snapshot's look at the file.
fn tick() {
    std::thread::sleep(Duration::from_millis(30));
}

fn row(path: &str) -> Vec<(String, bool)> {
    vec![(path.to_string(), true)]
}

#[test]
fn a_too_large_file_that_did_not_change_is_not_listed_as_changed() {
    let scratch = Scratch::new("large-unchanged");
    let big = scratch.project().join("huge.txt");
    let jobs = Jobs::default();
    let mut review = small_limit_review(&scratch, &jobs);

    // Turn 1 creates it: left out of the end snapshot only, a change.
    run_turn(&mut review, "t1", || std::fs::write(&big, vec![b'x'; LARGE]).unwrap());
    assert_eq!(rows(&review, TurnRef::N(1), Scope::Turn), row("huge.txt"));

    // Turns 2 and 3 do not touch it: left out of both snapshots, and the same file.
    run_turn(&mut review, "t2", || {});
    assert_eq!(rows(&review, TurnRef::N(2), Scope::Turn), vec![]);
    run_turn(&mut review, "t3", || {
        std::fs::write(scratch.project().join("a.txt"), "a\n").unwrap()
    });
    assert_eq!(
        rows(&review, TurnRef::N(3), Scope::Turn),
        vec![("a.txt".to_string(), false)]
    );
    // From the first base to the last end it did change: it is not in the base.
    assert_eq!(
        rows(&review, TurnRef::N(3), Scope::Session),
        vec![("a.txt".to_string(), false), ("huge.txt".to_string(), true)]
    );

    // A window that never saw those turns reads the same from the shadow's refs.
    let later = small_limit_review(&scratch, &jobs);
    assert_eq!(rows(&later, TurnRef::N(2), Scope::Turn), vec![]);
}

#[test]
fn a_too_large_file_that_changed_during_the_turn_is_listed() {
    let scratch = Scratch::new("large-changed");
    let big = scratch.project().join("huge.txt");
    std::fs::write(&big, vec![b'x'; LARGE]).unwrap();
    let jobs = Jobs::default();
    let mut review = small_limit_review(&scratch, &jobs);

    // Different size.
    run_turn(&mut review, "t1", || {
        tick();
        std::fs::write(&big, vec![b'y'; LARGE + 50]).unwrap();
    });
    assert_eq!(rows(&review, TurnRef::N(1), Scope::Turn), row("huge.txt"));
    // The same size, other bytes.
    run_turn(&mut review, "t2", || {
        tick();
        std::fs::write(&big, vec![b'z'; LARGE + 50]).unwrap();
    });
    assert_eq!(rows(&review, TurnRef::N(2), Scope::Turn), row("huge.txt"));
    // Replaced by a file of the same size and bytes: another inode.
    run_turn(&mut review, "t3", || {
        tick();
        let copy = scratch.project().join("copy.tmp");
        std::fs::write(&copy, vec![b'z'; LARGE + 50]).unwrap();
        std::fs::rename(&copy, &big).unwrap();
    });
    assert_eq!(rows(&review, TurnRef::N(3), Scope::Turn), row("huge.txt"));
    // Deleted.
    run_turn(&mut review, "t4", || std::fs::remove_file(&big).unwrap());
    assert_eq!(rows(&review, TurnRef::N(4), Scope::Turn), row("huge.txt"));
    // A turn after that, with the file gone from both snapshots, says nothing about it.
    run_turn(&mut review, "t5", || {});
    assert_eq!(rows(&review, TurnRef::N(5), Scope::Turn), vec![]);
}

#[test]
fn a_file_that_crosses_the_size_limit_either_way_is_listed() {
    let scratch = Scratch::new("large-crossing");
    let file = scratch.project().join("grows.txt");
    std::fs::write(&file, "small\n").unwrap();
    let jobs = Jobs::default();
    let mut review = small_limit_review(&scratch, &jobs);

    run_turn(&mut review, "t1", || std::fs::write(&file, vec![b'x'; LARGE]).unwrap());
    assert_eq!(rows(&review, TurnRef::N(1), Scope::Turn), row("grows.txt"));
    run_turn(&mut review, "t2", || std::fs::write(&file, "small again\n").unwrap());
    assert_eq!(rows(&review, TurnRef::N(2), Scope::Turn), row("grows.txt"));
}

/// A snapshot taken before prints were kept lists the file and no print: the review cannot tell
/// that it did not change, and says so as it always did.
#[test]
fn a_record_from_before_prints_were_kept_still_lists_the_file() {
    let scratch = Scratch::new("large-old-record");
    let big = scratch.project().join("huge.txt");
    std::fs::write(&big, vec![b'x'; LARGE]).unwrap();
    let jobs = Jobs::default();
    let mut review = small_limit_review(&scratch, &jobs);
    run_turn(&mut review, "t1", || {});
    assert_eq!(
        rows(&review, TurnRef::N(1), Scope::Turn),
        vec![],
        "prints known: unchanged"
    );

    // Rewrites both snapshots' messages the way an older build wrote them.
    let git_dir = scratch.review_dir().join("git");
    let git = |args: &[&str], stdin: Option<&str>| -> String {
        use std::io::Write;
        let mut cmd = std::process::Command::new("git");
        cmd.arg("--git-dir")
            .arg(&git_dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "eitri")
            .env("GIT_AUTHOR_EMAIL", "eitri@localhost")
            .env("GIT_COMMITTER_NAME", "eitri")
            .env("GIT_COMMITTER_EMAIL", "eitri@localhost")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.unwrap_or("").as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8(out.stdout).unwrap().trim_end().to_string()
    };
    let refs = git(&["for-each-ref", "--format=%(refname)", "refs/eitri/"], None);
    let mut rewritten = 0;
    for name in refs.lines().filter(|n| n.ends_with("/base") || n.ends_with("/end")) {
        let message = git(&["log", "-1", "--format=%B", name], None);
        assert!(message.contains("skipped-print: "), "{message}");
        let old: String = message
            .lines()
            .filter(|l| !l.starts_with("skipped-print: ") && !l.is_empty())
            .map(|l| format!("{l}\n"))
            .collect();
        assert!(old.contains("skipped-large: "), "{old}");
        let tree = git(&["rev-parse", &format!("{name}^{{tree}}")], None);
        let commit = git(&["commit-tree", &tree, "-F", "-"], Some(&old));
        git(&["update-ref", name, &commit], None);
        rewritten += 1;
    }
    assert!(rewritten >= 2, "{refs}");

    let later = small_limit_review(&scratch, &jobs);
    assert_eq!(rows(&later, TurnRef::N(1), Scope::Turn), row("huge.txt"));
}

/// A turn still running, or one whose end snapshot failed, is compared with the disk: a file left
/// out of its base is checked against the file as it is now.
#[test]
fn a_running_turn_lists_a_too_large_file_only_once_it_differs_from_its_base() {
    let scratch = Scratch::new("large-vs-disk");
    let big = scratch.project().join("huge.txt");
    std::fs::write(&big, vec![b'x'; LARGE]).unwrap();
    let jobs = Jobs::default();
    let mut review = small_limit_review(&scratch, &jobs);

    review.observe(1, "s1", &started("t1"), now());
    poll_until(&mut review, "s1", "the base", |t| has_base(t, "t1"));
    assert_eq!(rows(&review, TurnRef::Latest, Scope::Turn), vec![]);
    tick();
    std::fs::write(&big, vec![b'y'; LARGE]).unwrap();
    assert_eq!(rows(&review, TurnRef::Latest, Scope::Turn), row("huge.txt"));
}

/// A file the turn's own calls named that is too large keeps its size warning even when nothing
/// says it changed: its patch is refused, so the row must not offer one.
#[test]
fn a_named_too_large_file_that_did_not_change_keeps_its_size_flag() {
    let scratch = Scratch::new("large-named");
    std::fs::write(scratch.project().join("huge.txt"), vec![b'x'; LARGE]).unwrap();
    let jobs = Jobs::default();
    let mut review = small_limit_review(&scratch, &jobs);
    run_turn(&mut review, "t1", || {});
    let named = NamedPaths {
        paths: BTreeSet::from([PathBuf::from("huge.txt")]),
        pending_no_result: 0,
    };
    let job = review.overview_job("s1", TurnRef::N(1), Scope::Turn, named);
    let overview = std::thread::spawn(move || job.run()).join().unwrap().unwrap();
    let listed: Vec<(String, bool, Origin)> = overview
        .files
        .iter()
        .map(|f| (f.path.display().to_string(), f.too_large, f.origin))
        .collect();
    assert_eq!(listed, vec![("huge.txt".to_string(), true, Origin::AgentOnly)]);
}
