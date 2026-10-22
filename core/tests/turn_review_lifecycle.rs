//! The turn lifecycle against real git: scripted events into a `TurnReview`, real snapshots in a
//! shadow under the system's temporary directory, and jobs held back on the worker to make every
//! race happen on purpose.

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use agent::{AgentDomainEvent, ResumeStatus, TurnOutcome};
use eitri_core::turn_review::{
    review_dir_for, JobEvent, NamedPaths, Origin, ReviewOptions, Scope, Snap, SnapshotKind, TurnRecord, TurnRef,
    TurnReview, TurnState,
};

/// A directory of the test's own under the system's temporary directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        // The system's temporary directory, not the target dir: real snapshots must finish inside
        // the waits below, and a checkout on a network mount can take longer than that. A fresh
        // directory of this user's own holds it: a random name made here at 0700, never one already
        // in place in a shared directory, set exactly so whatever the umask, and removed on drop.
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let dir = std::env::temp_dir().join(format!(
            "eitri-turn_review_lifecycle-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
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
        // A snapshot or journal worker a test started can still write here for a moment after the
        // test's last line (they are not joined), and a removal racing it leaves part of the tree,
        // or the worker makes it again. So the directory must stay gone for a while, up to 5 s.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut absent_since = None;
        while std::time::Instant::now() < deadline {
            if self.0.exists() {
                let _ = std::fs::remove_dir_all(&self.0);
                absent_since = None;
            } else if absent_since.get_or_insert_with(std::time::Instant::now).elapsed()
                > std::time::Duration::from_millis(100)
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
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

/// The pump interval the overlap rule counts quiet gaps in.
fn turn_review_pump_interval() -> Duration {
    eitri_core::turn_review::lifecycle::PUMP_INTERVAL
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

fn background(running: u32) -> AgentDomainEvent {
    AgentDomainEvent::BackgroundCalls { running }
}

const NOTE_LEFT_RUNNING: &str =
    "a command was still running when this turn ended; later changes are not in this review";

/// A turn that ends while the agent still has a command running in the background says so, since
/// what that command writes later is in no review; the mark survives a restart. The count is the
/// tab's own, and a turn that ends with nothing left running is not marked.
#[test]
fn a_turn_that_left_a_command_running_says_so_and_keeps_saying_it() {
    let scratch = Scratch::new("left-running");
    let jobs = Jobs::default();
    {
        let mut run1 = scratch.review(&jobs);
        run1.observe(1, "s1", &started("t1"), now());
        // The provider counts what the turn left before it reports the turn's end.
        run1.observe(1, "s1", &background(1), now());
        run1.observe(1, "s1", &completed("t1"), now());
        run1.observe(1, "s1", &background(0), now());
        run1.observe(1, "s1", &started("t2"), now());
        run1.observe(1, "s1", &completed("t2"), now());
        // Another tab's background command says nothing about this tab's turns.
        run1.observe(2, "s2", &started("b1"), now());
        run1.observe(2, "s2", &background(2), now());
        run1.observe(2, "s2", &completed("b1"), now());
        poll_until(&mut run1, "s1", "both turns", |t| is_ok(t, "t1") && is_ok(t, "t2"));
        poll_until(&mut run1, "s2", "the other tab's turn", |t| is_ok(t, "b1"));
        let seen = run1.turns("s1");
        assert!(turn(&seen, "t1").left_running, "{seen:#?}");
        assert!(!turn(&seen, "t2").left_running, "{seen:#?}");
        assert!(turn(&run1.turns("s2"), "b1").left_running);

        let notes = |review: &TurnReview, n: u32| {
            review
                .overview_job("s1", TurnRef::N(n), Scope::Turn, NamedPaths::default())
                .run()
                .unwrap()
                .notes
        };
        assert!(
            notes(&run1, 1).contains(&NOTE_LEFT_RUNNING.to_string()),
            "{:?}",
            notes(&run1, 1)
        );
        assert!(
            !notes(&run1, 2).contains(&NOTE_LEFT_RUNNING.to_string()),
            "{:?}",
            notes(&run1, 2)
        );

        run1.close_tab(1, now());
        run1.close_tab(2, now());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !run1.turns("s1").is_empty() || !run1.turns("s2").is_empty() {
            assert!(Instant::now() < deadline, "the worker never finished");
            run1.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    let run2 = scratch.review(&jobs);
    let job = run2.overview_job("s1", TurnRef::N(1), Scope::Turn, NamedPaths::default());
    let overview = std::thread::spawn(move || job.run()).join().unwrap().unwrap();
    let flags: Vec<(u32, bool, bool)> = overview
        .turns
        .iter()
        .map(|t| (t.n, t.earlier_run, t.left_running))
        .collect();
    assert_eq!(flags, vec![(1, true, true), (2, true, false)]);
    assert!(
        overview.notes.contains(&NOTE_LEFT_RUNNING.to_string()),
        "{:?}",
        overview.notes
    );
    let job = run2.overview_job("s1", TurnRef::Latest, Scope::Session, NamedPaths::default());
    let session = std::thread::spawn(move || job.run()).join().unwrap().unwrap();
    assert!(
        session.notes.contains(&format!("turn 1: {NOTE_LEFT_RUNNING}")),
        "{:?}",
        session.notes
    );
}

/// The count is kept while it stays above zero: every turn that ends meanwhile is marked. A session
/// that ends takes its count with it, so a turn of the next session is not marked, and a resync
/// that swallowed a turn's end marks it from the count it was given.
#[test]
fn a_count_above_zero_marks_every_turn_until_it_drops() {
    let scratch = Scratch::new("left-running-more");
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    review.observe(1, "s1", &started("t1"), now());
    review.observe(1, "s1", &background(1), now());
    review.observe(1, "s1", &completed("t1"), now());
    review.observe(1, "s1", &started("t2"), now());
    review.observe(1, "s1", &completed("t2"), now());
    review.observe(1, "s1", &started("t3"), now());
    review.observe_resync(1, "s1", None, now());
    poll_until(&mut review, "s1", "three turns", |t| {
        is_ok(t, "t1") && is_ok(t, "t2") && is_ok(t, "t3")
    });
    let turns = review.turns("s1");
    assert!(turns.iter().all(|t| t.left_running), "{turns:#?}");

    review.observe(
        1,
        "s1",
        &AgentDomainEvent::SessionUnavailable { reason: "gone".into() },
        now(),
    );
    review.observe(1, "", &opened("s9"), now());
    review.observe(1, "s9", &started("n1"), now());
    review.observe(1, "s9", &completed("n1"), now());
    poll_until(&mut review, "s9", "the new session's turn", |t| is_ok(t, "n1"));
    assert!(!turn(&review.turns("s9"), "n1").left_running);
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
    // Pumps on a steady 33 ms tick, by the clock this test names: the wall clock between two calls
    // would decide which pumps count as neighbours.
    let at = Instant::now();
    let pump = |n: u32| at + turn_review_pump_interval() * n;
    let scratch = Scratch::new("two-tabs");
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    review.observe(1, "s1", &started("a1"), now());
    review.observe(2, "s2", &started("b1"), now());
    review.observe(1, "s1", &completed("a1"), now());
    review.observe(2, "s2", &completed("b1"), now());
    // Two pumps later: an end observed in the pump before may still have come after a start.
    review.poll_at(pump(0));
    review.poll_at(pump(1));
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
    // Pumps on a steady 33 ms tick, by the clock this test names: the wall clock between two calls
    // would decide which pumps count as neighbours.
    let at = Instant::now();
    let pump = |n: u32| at + turn_review_pump_interval() * n;
    // Tab 1 drained first: its end is observed before tab 2's start in the same pump.
    let scratch = Scratch::new("drain-order");
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    review.poll_at(pump(0));
    review.observe(1, "s1", &started("a1"), now());
    review.poll_at(pump(1));
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
    review.poll_at(pump(2));
    review.observe(2, "s2", &started("b1"), now());
    review.poll_at(pump(3));
    review.observe(2, "s2", &completed("b1"), now());
    review.poll_at(pump(4));
    review.observe(1, "s1", &started("a1"), now());
    review.observe(1, "s1", &completed("a1"), now());
    poll_until(&mut review, "s1", "tab 1's turn", |t| is_ok(t, "a1"));
    poll_until(&mut review, "s2", "tab 2's turn", |t| is_ok(t, "b1"));
    assert!(turn(&review.turns("s1"), "a1").overlapped_tab);
    assert!(turn(&review.turns("s2"), "b1").overlapped_tab);
}

/// A window pumps only when something happened, so a quiet stretch counts as the pumps a steady tick
/// would have made in it: an end observed a quiet second before another tab's start is not taken for one
/// that may have come after it, while an end in the pump just before still is.
#[test]
fn a_quiet_stretch_between_pumps_counts_as_the_pumps_it_would_have_held() {
    let at = std::time::Instant::now();
    let scratch = Scratch::new("quiet-gap");
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    review.poll_at(at);
    review.observe(2, "s2", &started("b1"), now());
    review.poll_at(at + std::time::Duration::from_millis(33));
    review.observe(2, "s2", &completed("b1"), now());
    // The next pump comes a second later, when tab 1's start woke the window.
    review.poll_at(at + std::time::Duration::from_millis(1033));
    review.observe(1, "s1", &started("a1"), now());
    review.observe(1, "s1", &completed("a1"), now());
    poll_until(&mut review, "s1", "tab 1's turn", |t| is_ok(t, "a1"));
    poll_until(&mut review, "s2", "tab 2's turn", |t| is_ok(t, "b1"));
    assert!(!turn(&review.turns("s1"), "a1").overlapped_tab, "a quiet second apart");
    assert!(!turn(&review.turns("s2"), "b1").overlapped_tab);

    // The same with the next pump one tick later: the end may still have come after the start.
    let scratch = Scratch::new("quiet-gap-none");
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    review.poll_at(at);
    review.observe(2, "s2", &started("b1"), now());
    review.poll_at(at + std::time::Duration::from_millis(33));
    review.observe(2, "s2", &completed("b1"), now());
    review.poll_at(at + std::time::Duration::from_millis(66));
    review.observe(1, "s1", &started("a1"), now());
    review.observe(1, "s1", &completed("a1"), now());
    poll_until(&mut review, "s1", "tab 1's turn", |t| is_ok(t, "a1"));
    poll_until(&mut review, "s2", "tab 2's turn", |t| is_ok(t, "b1"));
    assert!(turn(&review.turns("s1"), "a1").overlapped_tab, "neighbouring pumps");
    assert!(turn(&review.turns("s2"), "b1").overlapped_tab);
}

/// The window keeps pumping while a worker owes an answer, and may stop once none does: a turn's snapshots
/// are work in flight until their answers were polled.
#[test]
fn work_is_in_flight_until_every_snapshot_was_answered() {
    let scratch = Scratch::new("in-flight");
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    assert!(!review.has_work_in_flight(), "nothing was handed out");
    review.observe(1, "s1", &opened("s1"), now());
    review.observe(1, "s1", &started("a1"), now());
    review.observe(1, "s1", &completed("a1"), now());
    assert!(review.has_work_in_flight(), "the turn's snapshots are queued");
    poll_until(&mut review, "s1", "the turn's snapshots", |t| is_ok(t, "a1"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while review.has_work_in_flight() {
        assert!(Instant::now() < deadline, "work still in flight after 5 s");
        review.poll();
        std::thread::sleep(Duration::from_millis(5));
    }
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
// APFS refuses a file name that is not valid UTF-8 (`EILSEQ`), so the raw-bytes name this test is about
// cannot exist on macOS.
#[cfg(target_os = "linux")]
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

/// A resync that ends a turn whose end was among the dropped events hands review the count the turn ended
/// with before the count of now: the command may have finished in between, and the turn still left it
/// running.
#[test]
fn a_resync_marks_a_turn_by_the_count_it_ended_with() {
    let scratch = Scratch::new("left-running-resync");
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    review.observe(1, "s1", &started("t1"), now());
    // What the tab hands review on a resync: the count at the latest turn's end, the resync, the count now.
    review.observe(1, "s1", &background(1), now());
    review.observe_resync(1, "s1", None, now());
    review.observe(1, "s1", &background(0), now());
    poll_until(&mut review, "s1", "the turn", |t| is_ok(t, "t1"));
    assert!(turn(&review.turns("s1"), "t1").left_running);
}

// ---- a linked partner's turns, and a session's review moving between windows ---------------------

use eitri_core::turn_review::{HandedTurn, ReviewHandoff, ReviewHint, Unbound};
use eitri_core::window_link::{EdgeKind, TurnEdge};
use std::sync::atomic::{AtomicU64, Ordering};

/// A `CLOCK_MONOTONIC` stand-in the test sets, so turn times are the ones it names.
#[derive(Clone, Default)]
struct Clock(Arc<AtomicU64>);

impl Clock {
    fn set(&self, ns: u64) {
        self.0.store(ns, Ordering::SeqCst);
    }

    fn review(&self, scratch: &Scratch, jobs: &Jobs) -> TurnReview {
        let clock = self.0.clone();
        let options = ReviewOptions {
            monotonic: Some(Arc::new(move || clock.load(Ordering::SeqCst))),
            ..jobs.options()
        };
        TurnReview::with_options(Some(scratch.review_dir()), &scratch.project(), options)
    }
}

fn edge(session: &str, edge: EdgeKind, at_ns: u64) -> TurnEdge {
    TurnEdge {
        session: session.into(),
        edge,
        at_ns,
    }
}

/// Polls until no worker owes an answer, so what was handed to them (marks included) is written.
fn settle(review: &mut TurnReview) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while review.has_work_in_flight() {
        assert!(Instant::now() < deadline, "the workers never finished");
        review.poll();
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_late_started_edge_marks_a_turn_of_this_window_that_ended_pumps_before() {
    let at = Instant::now();
    let pump = |n: u32| at + turn_review_pump_interval() * n;
    let scratch = Scratch::new("late-edge");
    let jobs = Jobs::default();
    let clock = Clock::default();
    let mut review = clock.review(&scratch, &jobs);
    clock.set(1_000);
    review.observe(1, "s1", &started("a0"), now());
    clock.set(2_000);
    review.observe(1, "s1", &completed("a0"), now());
    clock.set(10_000);
    review.observe(1, "s1", &started("a1"), now());
    clock.set(20_000);
    review.observe(1, "s1", &completed("a1"), now());
    // Many pumps pass before the partner's start reaches this window: by pumps alone, a1 ended too
    // long ago to have met it.
    for n in 0..10 {
        review.poll_at(pump(n));
    }
    poll_until(&mut review, "s1", "both turns", |t| is_ok(t, "a0") && is_ok(t, "a1"));
    review.partner_turn(edge("p1", EdgeKind::Started, 15_000));
    let turns = review.turns("s1");
    assert!(
        turn(&turns, "a1").overlapped_tab,
        "it ran while the partner's turn had started"
    );
    assert!(
        !turn(&turns, "a0").overlapped_tab,
        "it ended before the partner's turn started"
    );

    // The mark reaches the store, so a later run says it too.
    settle(&mut review);
    let n = turn(&turns, "a1").n;
    let later = scratch.review(&Jobs::default());
    let overview = later
        .overview_job("s1", TurnRef::N(n), Scope::Turn, NamedPaths::default())
        .run()
        .unwrap();
    assert!(
        overview
            .notes
            .contains(&"this turn overlapped another tab's turn".to_string()),
        "{:?}",
        overview.notes
    );
}

#[test]
fn an_ended_edge_marks_only_the_turns_its_interval_met() {
    const S: u64 = 1_000_000_000;
    let scratch = Scratch::new("ended-edge");
    let jobs = Jobs::default();
    let clock = Clock::default();
    let mut review = clock.review(&scratch, &jobs);
    clock.set(S);
    review.observe(1, "s1", &started("a0"), now());
    clock.set(2 * S);
    review.observe(1, "s1", &completed("a0"), now());
    // A partner turn wholly between this window's two turns.
    review.partner_turn(edge("p1", EdgeKind::Started, 3 * S));
    review.partner_turn(edge("p1", EdgeKind::Ended, 5 * S));
    clock.set(10 * S);
    review.observe(1, "s1", &started("a1"), now());
    clock.set(20 * S);
    review.observe(1, "s1", &completed("a1"), now());
    poll_until(&mut review, "s1", "both turns", |t| is_ok(t, "a0") && is_ok(t, "a1"));
    // An end whose start was never heard of compares only its own instant.
    review.partner_turn(edge("p3", EdgeKind::Ended, 30 * S));
    let turns = review.turns("s1");
    assert!(!turn(&turns, "a0").overlapped_tab, "it ended before the partner's turn");
    assert!(
        !turn(&turns, "a1").overlapped_tab,
        "it started after the partner's turn ended"
    );
    // A partner turn heard of late that spanned both marks both, though both had ended.
    review.partner_turn(edge("p2", EdgeKind::Started, S + S / 2));
    review.partner_turn(edge("p2", EdgeKind::Ended, 12 * S));
    let turns = review.turns("s1");
    assert!(turn(&turns, "a0").overlapped_tab && turn(&turns, "a1").overlapped_tab);
}

#[test]
fn edges_carried_in_hello_mark_turns_already_running() {
    let (one, two) = (Scratch::new("hello-one"), Scratch::new("hello-two"));
    let (jobs_one, jobs_two) = (Jobs::default(), Jobs::default());
    let (clock_one, clock_two) = (Clock::default(), Clock::default());
    let mut window_one = clock_one.review(&one, &jobs_one);
    let mut window_two = clock_two.review(&two, &jobs_two);
    clock_one.set(5_000);
    window_one.observe(1, "s1", &started("a1"), now());
    clock_two.set(3_000);
    window_two.observe(1, "s2", &started("b1"), now());
    poll_until(&mut window_two, "s2", "b1's base", |t| has_base(t, "b1"));

    // The link comes up: each side's hello carries its running turns with their own times.
    let hello = window_one.running_turn_edges();
    assert_eq!(hello, vec![edge("s1", EdgeKind::Started, 5_000)]);
    for e in hello {
        window_two.partner_turn(e);
    }
    assert!(turn(&window_two.turns("s2"), "b1").overlapped_tab);

    // A turn starting while the partner's runs is marked as it starts.
    clock_two.set(6_000);
    window_two.observe(2, "s3", &started("c1"), now());
    poll_until(&mut window_two, "s3", "c1's base", |t| has_base(t, "c1"));
    assert!(turn(&window_two.turns("s3"), "c1").overlapped_tab);

    // Window 1's end, announced as an edge, ends it there; a turn long after is not marked.
    window_one.announce_turn_edges(true);
    clock_one.set(9_000);
    window_one.observe(1, "s1", &completed("a1"), now());
    let edges = window_one.take_turn_edges();
    assert_eq!(edges, vec![edge("s1", EdgeKind::Ended, 9_000)]);
    assert!(window_one.take_turn_edges().is_empty(), "taken once");
    for e in edges {
        window_two.partner_turn(e);
    }
    // Window 2's own turns end too, pumps before the next one starts, so only the partner could mark it.
    let at = Instant::now();
    let pump = |n: u32| at + turn_review_pump_interval() * n;
    window_two.observe(1, "s2", &completed("b1"), now());
    window_two.observe(2, "s3", &completed("c1"), now());
    for n in 0..5 {
        window_two.poll_at(pump(n));
    }
    clock_two.set(60_000_000_000);
    window_two.observe(3, "s4", &started("d1"), now());
    poll_until(&mut window_two, "s4", "d1's base", |t| has_base(t, "d1"));
    assert!(!turn(&window_two.turns("s4"), "d1").overlapped_tab);

    // A partner gone with a turn still running counts it as ended from then on.
    window_two.partner_turn(edge("s9", EdgeKind::Started, 60_000_000_001));
    clock_two.set(60_000_000_002);
    window_two.partner_gone();
    window_two.observe(3, "s4", &completed("d1"), now());
    for n in 5..10 {
        window_two.poll_at(pump(n));
    }
    clock_two.set(120_000_000_000);
    window_two.observe(4, "s5", &started("e1"), now());
    poll_until(&mut window_two, "s5", "e1's base", |t| has_base(t, "e1"));
    assert!(!turn(&window_two.turns("s5"), "e1").overlapped_tab);
}

#[test]
fn a_review_handoff_round_trips_with_ended_pump_reset_and_times_kept() {
    let at = Instant::now();
    let pump = |n: u32| at + turn_review_pump_interval() * n;
    let scratch = Scratch::new("handoff");
    let jobs = Jobs::default();
    let clock = Clock::default();
    let mut source = clock.review(&scratch, &jobs);
    source.poll_at(pump(0));
    clock.set(100);
    source.observe(1, "s1", &started("a1"), now());
    clock.set(200);
    source.observe(1, "s1", &completed("a1"), now());
    clock.set(300);
    source.observe(1, "s1", &started("a2"), now());
    clock.set(400);
    source.observe(1, "s1", &completed("a2"), now());
    poll_until(&mut source, "s1", "both turns", |t| is_ok(t, "a1") && is_ok(t, "a2"));
    let before = source.turns("s1");

    let handoff = source.hand_over_session("s1", 1).unwrap();
    let text = serde_json::to_string(&handoff).unwrap();
    let back: ReviewHandoff = serde_json::from_str(&text).unwrap();
    assert_eq!(back, handoff, "the handoff survives the link");
    let times: Vec<(u64, Option<u64>)> = handoff.records.iter().map(|r| (r.started_ns, r.ended_ns)).collect();
    assert_eq!(times, vec![(100, Some(200)), (300, Some(400))]);
    assert_eq!(handoff.running, None);
    assert_eq!(handoff.offset, Some(0));

    // The destination has pumped far more than the source.
    let dest_jobs = Jobs::default();
    let mut dest = Clock::default().review(&scratch, &dest_jobs);
    for n in 0..50 {
        dest.poll_at(pump(n));
    }
    dest.adopt_session(back, 7);
    assert_eq!(dest.turns("s1"), {
        let mut moved = before.clone();
        for t in &mut moved {
            t.tab = 7;
        }
        moved
    });

    // The adopted turn ended in the destination's current pump: a turn another tab starts in this
    // pump may have run beside it, as for an end observed here.
    dest.observe(8, "s2", &started("b1"), now());
    assert!(turn(&dest.turns("s1"), "a2").overlapped_tab, "{:#?}", dest.turns("s1"));

    // Its times moved with it.
    let again = dest.hand_over_session("s1", 7).unwrap();
    let times_again: Vec<(u64, Option<u64>)> = again.records.iter().map(|r| (r.started_ns, r.ended_ns)).collect();
    assert_eq!(times_again, times);
    assert!(again.records.iter().all(|r| r.tab == 7));
}

#[test]
fn a_job_finishing_in_the_source_reaches_the_holder() {
    let scratch = Scratch::new("holder-result");
    let jobs = Jobs::default();
    jobs.hold(SnapshotKind::Base, 1);
    let mut source = scratch.review(&jobs);
    // No session event first: the worker reads the numbering only when the base starts, which is
    // held, so the handoff goes without it.
    source.observe(1, "s1", &started("t1"), now());
    jobs.wait_started(SnapshotKind::Base, 1);
    source.poll();
    let handoff = source.hand_over_session("s1", 1).unwrap();
    assert_eq!(handoff.offset, None);
    assert_eq!(
        handoff.running,
        Some(HandedTurn {
            position: 1,
            base: None
        })
    );

    let dest_jobs = Jobs::default();
    let mut dest = scratch.review(&dest_jobs);
    dest.adopt_session(handoff, 9);
    source.drop_moved(1);
    // The turn ends in the destination; its end waits for the numbering the source is reading.
    dest.observe(9, "s1", &completed("t1"), now());
    dest.poll();
    assert!(
        !dest_jobs.finished(SnapshotKind::End, 1),
        "nothing is numbered before the offset is known"
    );

    jobs.release(SnapshotKind::Base, 1);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut forwarded = Vec::new();
    while forwarded.len() < 2 {
        assert!(Instant::now() < deadline, "the source never finished: {forwarded:?}");
        source.poll();
        while let Some(result) = source.result_for_holder("s1") {
            forwarded.push(result);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    for result in forwarded {
        dest.accept_result("s1", result).unwrap();
    }
    let mut hints: Vec<ReviewHint> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !is_ok(&dest.turns("s1"), "t1") {
        assert!(Instant::now() < deadline, "{:#?}", dest.turns("s1"));
        hints.extend(dest.poll());
        std::thread::sleep(Duration::from_millis(5));
    }
    hints.extend(dest.poll());
    let t1 = turn(&dest.turns("s1"), "t1").clone();
    assert_eq!(t1.n, 1, "numbered as the source numbered it");
    assert_eq!(t1.tab, 9);
    assert_eq!(
        hints,
        vec![ReviewHint {
            tab: 9,
            turn: 1,
            files: 0
        }]
    );
    settle(&mut source);
    assert!(source.turns("s1").is_empty());
    assert!(!source.has_work_in_flight());
}

#[test]
fn drop_moved_takes_no_end_snapshot_and_keeps_no_presence() {
    let scratch = Scratch::new("drop-moved");
    let jobs = Jobs::default();
    let clock = Clock::default();
    let mut source = clock.review(&scratch, &jobs);
    clock.set(1_000);
    source.observe(1, "s1", &started("t1"), now());
    poll_until(&mut source, "s1", "the base", |t| has_base(t, "t1"));
    source.hand_over_session("s1", 1).unwrap();
    source.drop_moved(1);
    settle(&mut source);
    assert!(
        !jobs.seen().iter().any(|e| matches!(
            e,
            JobEvent::Starting {
                kind: SnapshotKind::End,
                ..
            }
        )),
        "the moved turn was ended here: {:?}",
        jobs.seen()
    );
    assert!(source.turns("s1").is_empty());
    assert!(source.running_turn_edges().is_empty(), "no turn of this window runs");

    // The moved turn still runs, in the other window: a turn starting here meets it.
    clock.set(2_000);
    source.observe(2, "s2", &started("u1"), now());
    poll_until(&mut source, "s2", "u1's base", |t| has_base(t, "u1"));
    assert!(turn(&source.turns("s2"), "u1").overlapped_tab);

    // The presence: the tab's running file goes with the set that no longer has it.
    let presence_dir = scratch.review_dir();
    let holder = eitri_core::turn_review::PresenceHolder::start(presence_dir.clone(), 4242);
    let held = holder.sync_confirmed(&BTreeSet::from([1]));
    assert_eq!(
        held.recv_timeout(Duration::from_secs(5)),
        Ok(eitri_core::turn_review::Held::Yes)
    );
    let dropped = holder.sync_confirmed(&BTreeSet::new());
    assert_eq!(
        dropped.recv_timeout(Duration::from_secs(5)),
        Ok(eitri_core::turn_review::Held::Yes)
    );
    assert!(!presence_dir.join("running/4242.1").exists());
}

#[test]
fn an_unbound_turn_cannot_be_handed_over() {
    let scratch = Scratch::new("unbound");
    let jobs = Jobs::default();
    let mut review = scratch.review(&jobs);
    // The tab's backend has not reported its session yet.
    review.observe(1, "", &started("t1"), now());
    assert_eq!(review.hand_over_session("s1", 1), Err(Unbound));
    // Once it has, the turn is filed under it and can move.
    review.observe(1, "s1", &opened("s1"), now());
    assert!(review.hand_over_session("s1", 1).is_ok());
}

#[test]
fn a_session_back_before_its_first_worker_answered_is_numbered_and_its_turn_ends() {
    let scratch = Scratch::new("back-again");
    let jobs = Jobs::default();
    jobs.hold(SnapshotKind::Base, 1);
    let mut one = scratch.review(&jobs);
    // The worker here is parked before it reads the numbering, so neither handoff carries it.
    one.observe(1, "s1", &started("t1"), now());
    jobs.wait_started(SnapshotKind::Base, 1);
    one.poll();
    let there = one.hand_over_session("s1", 1).unwrap();
    assert_eq!(there.offset, None);
    let two_jobs = Jobs::default();
    let mut two = scratch.review(&two_jobs);
    two.adopt_session(there, 9);
    one.drop_moved(1);

    // The chat comes straight back, before the first worker here answered anything.
    let back = two.hand_over_session("s1", 9).unwrap();
    assert_eq!(back.offset, None);
    assert_eq!(back.running.as_ref().map(|t| t.position), Some(1));
    two.drop_moved(9);
    one.adopt_session(back, 2);
    one.observe(2, "s1", &completed("t1"), now());

    // The first worker's numbering and base are the session's here again: the end kept back for
    // the numbering goes out, and the turn is whole.
    jobs.release(SnapshotKind::Base, 1);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !is_ok(&one.turns("s1"), "t1") {
        assert!(
            Instant::now() < deadline,
            "the turn never ended: {:#?}",
            one.turns("s1")
        );
        one.poll();
        std::thread::sleep(Duration::from_millis(5));
    }
    let t1 = turn(&one.turns("s1"), "t1").clone();
    assert_eq!((t1.n, t1.tab), (1, 2));
    // Each worker's answers settled its own count: nothing is owed once both are done.
    settle(&mut one);
    assert!(!one.has_work_in_flight());
    assert_eq!(
        one.result_for_holder("s1"),
        None,
        "nothing is kept for a window that no longer holds the session"
    );
    assert_eq!(two.result_for_holder("s1"), None);
}

#[test]
fn a_command_left_running_moves_with_the_session() {
    let scratch = Scratch::new("background-moves");
    let jobs = Jobs::default();
    let mut source = scratch.review(&jobs);
    source.observe(1, "s1", &started("t1"), now());
    source.observe(1, "s1", &background(1), now());
    source.observe(1, "s1", &completed("t1"), now());
    poll_until(&mut source, "s1", "t1", |t| is_ok(t, "t1"));
    assert!(turn(&source.turns("s1"), "t1").left_running);

    let handoff = source.hand_over_session("s1", 1).unwrap();
    assert_eq!(handoff.background, 1);
    let back: ReviewHandoff = serde_json::from_str(&serde_json::to_string(&handoff).unwrap()).unwrap();
    assert_eq!(back, handoff);
    let dest_jobs = Jobs::default();
    let mut dest = scratch.review(&dest_jobs);
    dest.adopt_session(back, 9);
    source.drop_moved(1);

    // The agent says the count again only when it changes: the next turn here still ends with the
    // command running.
    dest.observe(9, "s1", &started("t2"), now());
    dest.observe(9, "s1", &completed("t2"), now());
    poll_until(&mut dest, "s1", "t2", |t| is_ok(t, "t2"));
    assert!(turn(&dest.turns("s1"), "t2").left_running);
}

#[test]
fn a_partner_turn_whose_edges_arrive_together_marks_only_what_it_met() {
    const S: u64 = 1_000_000_000;
    let scratch = Scratch::new("edges-together");
    let jobs = Jobs::default();
    let clock = Clock::default();
    let mut review = clock.review(&scratch, &jobs);
    clock.set(10 * S);
    review.observe(1, "s1", &started("a1"), now());
    clock.set(11 * S);
    review.observe(1, "s1", &completed("a1"), now());
    poll_until(&mut review, "s1", "a1", |t| is_ok(t, "a1"));

    // A partner turn long before, its two edges held up on the way and arriving together.
    review.partner_turns([edge("p1", EdgeKind::Started, S), edge("p1", EdgeKind::Ended, 2 * S)]);
    assert!(
        !turn(&review.turns("s1"), "a1").overlapped_tab,
        "it ended long before this window's turn started"
    );

    // One that did run beside it, and a start whose end is not among the edges, are still seen.
    review.partner_turns([
        edge("p1", EdgeKind::Started, 3 * S),
        edge("p1", EdgeKind::Ended, 4 * S),
        edge("p2", EdgeKind::Started, 10 * S + S / 2),
    ]);
    assert!(turn(&review.turns("s1"), "a1").overlapped_tab);
}
