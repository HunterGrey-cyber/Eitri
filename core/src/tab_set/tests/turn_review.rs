//! Turn review through the pump.
#![cfg(test)]

use super::*;

/// A directory of a test's own inside the target dir (beside the test binary), never under the
/// user's state directory.
fn target_scratch(label: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let dir = exe
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .join("turn-review-tests")
        .join(format!(
            "{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
    std::fs::create_dir_all(dir.join("project")).unwrap();
    std::fs::write(dir.join("project/main.rs"), "fn main() {}\n").unwrap();
    dir.canonicalize().unwrap()
}

/// Holds the worker on its first base snapshot until released.
#[derive(Clone, Default)]
struct HeldBase(Arc<(std::sync::Mutex<(bool, bool)>, std::sync::Condvar)>);

impl HeldBase {
    fn hook(&self) -> crate::turn_review::JobHook {
        let held = self.clone();
        Arc::new(move |event: &crate::turn_review::JobEvent| {
            if let crate::turn_review::JobEvent::Starting {
                kind: crate::turn_review::SnapshotKind::Base,
                ..
            } = event
            {
                let (lock, cvar) = &*held.0;
                let mut state = lock.lock().unwrap();
                state.1 = true;
                cvar.notify_all();
                while state.0 {
                    state = cvar.wait(state).unwrap();
                }
            }
        })
    }
    fn hold(&self) {
        self.0 .0.lock().unwrap().0 = true;
    }
    fn release(&self) {
        self.0 .0.lock().unwrap().0 = false;
        self.0 .1.notify_all();
    }
    fn parked(&self) -> bool {
        self.0 .0.lock().unwrap().1
    }
}

/// One auto tab on a session whose CLI runs auto: a turn whose in-project `Write` is deferred to
/// the CLI, which removes the request from the batch. Returns the answers the provider got and,
/// with a review installed, the turn's record once its base landed.
fn auto_turn_through_the_pump(
    scratch: &Path,
    review: Option<crate::turn_review::TurnReview>,
    held: &HeldBase,
) -> (
    Vec<(String, agent::PermissionDecision)>,
    Option<crate::turn_review::TurnRecord>,
) {
    let dir = scratch.join("project");
    let mut set = set();
    let reviewing = review.is_some();
    if let Some(review) = review {
        set.install_turn_review(review).unwrap();
    }
    let tab = set.active();
    let provider = Arc::new(RecordingProvider::cli_auto());
    let conversation =
        AgentConversation::create(provider.clone(), &dir, agent::setting_sources::ProjectTrust::Untrusted).unwrap();
    set.get_mut(tab).unwrap().backend = TabBackend::Live(AgentBackend::Conversation {
        agent: AgentId::Claude,
        conversation: Box::new(conversation),
    });
    let input = serde_json::json!({ "file_path": dir.join("main.rs"), "content": "fn main() { }\n" });
    provider.queue_all(vec![
        AgentDomainEvent::CliPermissionMode {
            reported: "auto".into(),
        },
        AgentDomainEvent::SessionOpened {
            session_id: "fake-session".into(),
            provider_session_id: "sess-auto".into(),
            model: "m".into(),
            cwd: dir.to_string_lossy().into_owned(),
        },
        AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
        AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "tu-w".into(),
            name: "Write".into(),
            input: input.clone(),
        },
        AgentDomainEvent::PermissionRequested {
            permission_id: "perm-w".into(),
            tool_use_id: Some("tu-w".into()),
            tool_name: "Write".into(),
            input,
            provider_prompt: None,
        },
    ]);
    until("the deferral", || {
        set.pump(&dir, true);
        !provider.decisions().is_empty()
    });
    let mut record = None;
    if reviewing {
        until("the base held on the worker", || held.parked());
        held.release();
        until("the base", || {
            set.pump(&dir, true);
            record = set.turn_review().unwrap().turns("sess-auto").into_iter().next();
            record
                .as_ref()
                .is_some_and(|r| r.base != crate::turn_review::Snap::Pending)
        });
    }
    let decisions = provider.decisions();
    shut_down_all(&mut set);
    (decisions, record)
}

#[test]
fn an_auto_answered_write_with_a_slow_base_is_marked_late_through_the_pump() {
    let scratch = target_scratch("late-through-pump");
    let held = HeldBase::default();
    held.hold();
    let review = crate::turn_review::TurnReview::with_options(
        Some(scratch.join("state/review")),
        &scratch.join("project"),
        crate::turn_review::ReviewOptions {
            excludes: Some(None),
            on_job: Some(held.hook()),
            trim_every: Duration::MAX,
            ..Default::default()
        },
    );
    let (with_review, record) = auto_turn_through_the_pump(&scratch, Some(review), &held);
    let record = record.unwrap();
    assert!(
        matches!(record.base, crate::turn_review::Snap::Taken { .. }),
        "{record:#?}"
    );
    assert!(
        record.late,
        "the deferred Write could have run before the base: {record:#?}"
    );
    assert_eq!(
        with_review,
        vec![("perm-w".to_string(), agent::PermissionDecision::Defer)],
        "the CLI decided, no card was drawn"
    );

    // The review changes no answer: the same turn with none installed is answered the same.
    let (without_review, _) = auto_turn_through_the_pump(&scratch, None, &HeldBase::default());
    assert_eq!(with_review, without_review);
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn a_review_with_review_off_says_so() {
    use crate::turn_review::{Scope, TurnRef};
    let mut set = set();
    let tab = set.active();
    set.turn_review_off();
    assert_eq!(
        set.review_overview_job(tab, TurnRef::Latest, Scope::Turn)
            .err()
            .as_deref(),
        Some(TURN_REVIEW_OFF)
    );
    assert_eq!(
        set.review_diff_job(tab, 1, Scope::Turn, "main.rs").err().as_deref(),
        Some(TURN_REVIEW_OFF)
    );
    assert!(set.turn_review().is_none());
}

#[test]
fn review_jobs_are_refused_without_a_review_a_tab_or_a_session() {
    use crate::turn_review::{Scope, TurnRef};
    let scratch = target_scratch("review-refused");
    let dir = scratch.join("project");
    let mut set = set();
    let tab = set.active();
    let refused = |set: &TabSet, tab: TabId| {
        (
            set.review_overview_job(tab, TurnRef::Latest, Scope::Turn).err(),
            set.review_diff_job(tab, 1, Scope::Turn, "main.rs").err(),
        )
    };
    let (overview, diff) = refused(&set, tab);
    assert_eq!(overview.as_deref(), Some(TURN_REVIEW_NEEDS_SIDECAR));
    assert_eq!(diff.as_deref(), Some(TURN_REVIEW_NEEDS_SIDECAR));

    set.install_turn_review(crate::turn_review::TurnReview::with_options(
        Some(scratch.join("state/review")),
        &dir,
        crate::turn_review::ReviewOptions {
            excludes: Some(None),
            ..Default::default()
        },
    ))
    .unwrap();
    let (overview, diff) = refused(&set, tab);
    assert!(overview.unwrap().contains("no session yet"));
    assert!(diff.unwrap().contains("no session yet"));
    let (overview, _) = refused(&set, TabId(9999));
    assert!(overview.unwrap().contains("no tab 9999"));
    let _ = std::fs::remove_dir_all(&scratch);
}

/// The whole route a review request takes in the shell, minus the GTK thread: the pump observes a
/// turn that edited a file, the tab builds the job from its own tool calls, and a worker runs it.
#[test]
fn a_review_job_attributes_with_the_tabs_own_edit_calls() {
    use crate::turn_review::{Origin, Scope, TurnRef};
    let scratch = target_scratch("review-job");
    let dir = scratch.join("project");
    let mut set = set();
    set.install_turn_review(crate::turn_review::TurnReview::with_options(
        Some(scratch.join("state/review")),
        &dir,
        crate::turn_review::ReviewOptions {
            excludes: Some(None),
            trim_every: Duration::MAX,
            ..Default::default()
        },
    ))
    .unwrap();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.open_session("sess-job", &dir);
    provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    until("the base", || {
        set.pump(&dir, true);
        set.turn_review()
            .unwrap()
            .turns("sess-job")
            .first()
            .is_some_and(|r| matches!(r.base, crate::turn_review::Snap::Taken { .. }))
    });
    // Two files change on disk during the turn; only one is named by a finished edit call.
    std::fs::write(dir.join("main.rs"), "fn main() { run(); }\n").unwrap();
    std::fs::write(dir.join("other.txt"), "from a shell command\n").unwrap();
    let input = serde_json::json!({ "file_path": dir.join("main.rs"), "content": "x" });
    provider.queue_all(vec![
        AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "tu-w".into(),
            name: "Write".into(),
            input,
        },
        AgentDomainEvent::ToolCallCompleted {
            turn_id: "t1".into(),
            tool_use_id: "tu-w".into(),
            content: serde_json::json!("ok"),
            is_error: false,
        },
        completed("t1", agent::TurnOutcome::Completed),
    ]);
    until("the end", || {
        set.pump(&dir, true);
        set.turn_review()
            .unwrap()
            .turns("sess-job")
            .first()
            .is_some_and(|r| matches!(r.end, crate::turn_review::Snap::Taken { .. }))
    });
    let overview = set
        .review_overview_job(tab, TurnRef::Latest, Scope::Turn)
        .unwrap()
        .run()
        .unwrap();
    let origin = |name: &str| {
        overview
            .files
            .iter()
            .find(|f| f.path == Path::new(name))
            .unwrap_or_else(|| panic!("{name} not in {:#?}", overview.files))
            .origin
    };
    assert_eq!(origin("main.rs"), Origin::Agent);
    assert_eq!(origin("other.txt"), Origin::Workspace);
    let diff = set
        .review_diff_job(tab, overview.current, Scope::Turn, "main.rs")
        .unwrap()
        .run()
        .unwrap();
    assert!(diff.hunks.is_some_and(|h| !h.is_empty()));
    shut_down_all(&mut set);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// With a turn review installed, the set needs pumping while the review's worker owes a snapshot: from
/// the turn's end until the pump has collected its end snapshot (held here on the worker), and not
/// after both snapshots are in.
#[test]
fn a_turn_reviews_snapshots_in_flight_need_pumping() {
    let scratch = target_scratch("review-in-flight");
    let dir = scratch.join("project");
    let gate: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)> = Arc::default();
    let hook: crate::turn_review::JobHook = {
        let gate = gate.clone();
        Arc::new(move |event: &crate::turn_review::JobEvent| {
            if let crate::turn_review::JobEvent::Starting {
                kind: crate::turn_review::SnapshotKind::End,
                ..
            } = event
            {
                let (held, cvar) = &*gate;
                let mut held = held.lock().unwrap();
                while *held {
                    held = cvar.wait(held).unwrap();
                }
            }
        })
    };
    *gate.0.lock().unwrap() = true;
    let mut set = set();
    set.install_turn_review(crate::turn_review::TurnReview::with_options(
        Some(scratch.join("state/review")),
        &dir,
        crate::turn_review::ReviewOptions {
            excludes: Some(None),
            trim_every: Duration::MAX,
            on_job: Some(hook),
            ..Default::default()
        },
    ))
    .unwrap();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    set.set_ui_wake(Arc::new(|| {}));
    provider.open_session("sess-in-flight", &dir);
    provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    let record = |set: &TabSet| set.turn_review().unwrap().turns("sess-in-flight").first().cloned();
    until("the base, and nothing left in flight", || {
        set.pump(&dir, true);
        record(&set).is_some_and(|r| matches!(r.base, crate::turn_review::Snap::Taken { .. })) && !set.needs_polling()
    });
    provider.queue(completed("t1", agent::TurnOutcome::Completed));
    until("the turn's end observed", || {
        set.pump(&dir, true);
        record(&set).is_some_and(|r| r.ended_ms.is_some())
    });
    for _ in 0..5 {
        assert!(set.needs_polling(), "the end snapshot is still with the worker");
        set.pump(&dir, true);
        std::thread::sleep(Duration::from_millis(10));
    }
    {
        let (held, cvar) = &*gate;
        *held.lock().unwrap() = false;
        cvar.notify_all();
    }
    until("both snapshots collected and nothing left in flight", || {
        set.pump(&dir, true);
        !set.needs_polling()
    });
    assert!(record(&set).is_some_and(|r| matches!(r.end, crate::turn_review::Snap::Taken { .. })));
    shut_down_all(&mut set);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A review directory for a presence test, in its own scratch directory.
fn presence_dir(label: &str) -> (PathBuf, PathBuf) {
    let scratch = target_scratch(label);
    let review = scratch.join("state/eitri/review/0123456789abcdef");
    (scratch, review)
}

/// What another window (pid 100) finds when it looks at the directory.
fn seen_from_elsewhere(review: &Path) -> Option<crate::turn_review::Busy> {
    crate::turn_review::busy_elsewhere(review, 100).unwrap()
}

#[test]
fn pump_feeds_presence_off_the_gtk_thread() {
    use crate::turn_review::{Blocked, Busy, PresenceHolder};
    let (scratch, review) = presence_dir("presence-pump");
    let dir = scratch.join("project");
    let mut set = set();
    let (holder, release) = PresenceHolder::start_parked(review.clone(), 4242);
    set.hold_presence(holder);
    let guard = set.presence_guard().expect("held");
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

    // The presence thread has not opened anything, and the pump that folds the turn start
    // neither waits for it nor reads the disk.
    provider.queue(started("t1"));
    pump_until_running(&mut set, &dir, tab, true);
    assert!(!review.join("windows").exists(), "nothing was opened yet");
    assert!(
        matches!(guard.check(), Err(Blocked::NotHeld(_))),
        "a window that holds nothing refuses"
    );

    release.send(()).unwrap();
    until("the turn is seen elsewhere", || {
        seen_from_elsewhere(&review) == Some(Busy::TurnRunning { pid: 4242 })
    });
    assert_eq!(guard.check(), Err(Blocked::TurnRunningHere));

    provider.queue(completed("t1", agent::TurnOutcome::Completed));
    pump_until_running(&mut set, &dir, tab, false);
    until("the turn is over", || {
        seen_from_elsewhere(&review) == Some(Busy::OtherWindow { pid: 4242 })
    });
    assert_eq!(guard.check(), Ok(()), "the count went back to nothing");
    shut_down_all(&mut set);
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn a_legacy_window_holds_presence_and_its_running_turn() {
    use crate::turn_review::{Busy, PresenceHolder};
    let (scratch, review) = presence_dir("presence-legacy");
    let dir = scratch.join("project");
    let mut set = TabSet::new(BackendKind::Legacy, SessionModeChoice::Auto);
    let off = crate::turn_review::TurnReview::with_options(
        None,
        Path::new("/nonexistent"),
        crate::turn_review::ReviewOptions::default(),
    );
    assert_eq!(set.install_turn_review(off), Err(TURN_REVIEW_NEEDS_SIDECAR));
    set.hold_presence(PresenceHolder::start(review.clone(), 4242));
    until("the window is seen elsewhere", || {
        seen_from_elsewhere(&review) == Some(Busy::OtherWindow { pid: 4242 })
    });

    // A send can fold a legacy turn before the pump sees it; presence must observe the
    // projection even when no review is installed.
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(started("t1"));
    until("the projection has the turn", || set.get(tab).unwrap().turn_running());
    set.pump(&dir, true);
    until("the turn is seen elsewhere", || {
        seen_from_elsewhere(&review) == Some(Busy::TurnRunning { pid: 4242 })
    });
    shut_down_all(&mut set);
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn presence_needs_no_turn_review() {
    use crate::turn_review::{Busy, PresenceHolder};
    let (scratch, review) = presence_dir("presence-review-off");
    let dir = scratch.join("project");
    let mut set = set();
    set.turn_review_off();
    set.hold_presence(PresenceHolder::start(review.clone(), 4242));
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(started("t1"));
    pump_until_running(&mut set, &dir, tab, true);
    until("the turn is seen elsewhere", || {
        seen_from_elsewhere(&review) == Some(Busy::TurnRunning { pid: 4242 })
    });
    assert!(
        !review.join("git").exists(),
        "presence does not make a shadow repository"
    );
    shut_down_all(&mut set);
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn a_set_without_presence_has_no_guard() {
    assert!(set().presence_guard().is_none());
}

#[test]
fn removing_a_running_tab_drops_its_running_file() {
    use crate::turn_review::{Busy, PresenceHolder};
    let (scratch, review) = presence_dir("presence-remove");
    let dir = scratch.join("project");
    let mut set = set();
    set.hold_presence(PresenceHolder::start(review.clone(), 4242));
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(started("t1"));
    pump_until_running(&mut set, &dir, tab, true);
    until("the turn is seen elsewhere", || {
        seen_from_elsewhere(&review) == Some(Busy::TurnRunning { pid: 4242 })
    });
    set.open(AgentId::Claude);
    let mut closed = set.remove(tab).unwrap();
    // Its backend may still be writing until its shutdown returns: the file stays until then.
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(seen_from_elsewhere(&review), Some(Busy::TurnRunning { pid: 4242 }));
    if let Some(retired) = closed.retired.take() {
        retired.into_backend().shutdown();
    }
    set.retire_done(tab);
    until("the turn's file is gone", || {
        seen_from_elsewhere(&review) == Some(Busy::OtherWindow { pid: 4242 })
    });
    shut_down_all(&mut set);
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn the_legacy_backend_refuses_a_turn_review() {
    let mut set = TabSet::new(BackendKind::Legacy, SessionModeChoice::Auto);
    let review = crate::turn_review::TurnReview::with_options(
        None,
        Path::new("/nonexistent"),
        crate::turn_review::ReviewOptions::default(),
    );
    assert_eq!(set.install_turn_review(review), Err(TURN_REVIEW_NEEDS_SIDECAR));
    assert!(set.turn_review().is_none());
}

#[test]
fn closing_a_tab_ends_its_turn_in_the_review() {
    let scratch = target_scratch("close-ends");
    let dir = scratch.join("project");
    let mut set = set();
    let options = crate::turn_review::ReviewOptions {
        excludes: Some(None),
        trim_every: Duration::MAX,
        ..Default::default()
    };
    set.install_turn_review(crate::turn_review::TurnReview::with_options(
        Some(scratch.join("state/review")),
        &dir,
        options,
    ))
    .unwrap();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.open_session("sess-close", &dir);
    provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    until("the base", || {
        set.pump(&dir, true);
        set.turn_review()
            .unwrap()
            .turns("sess-close")
            .first()
            .is_some_and(|r| matches!(r.base, crate::turn_review::Snap::Taken { .. }))
    });
    let other = set.open(AgentId::Claude);
    let mut closed = set.remove(tab).unwrap();
    if let Some(retired) = closed.retired.take() {
        retired.into_backend().shutdown();
    }
    assert_eq!(set.active(), other);
    // The tab's session stays tracked until its end snapshot is reported.
    until("the end", || {
        set.pump(&dir, true);
        set.turn_review().unwrap().turns("sess-close").is_empty()
    });
    let refs = std::fs::read_dir(scratch.join("state/review/git/refs/eitri/sess-close/1")).unwrap();
    let mut kinds: Vec<String> = refs.map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    kinds.sort();
    assert_eq!(kinds, vec!["base".to_string(), "end".to_string()]);
    shut_down_all(&mut set);
    let _ = std::fs::remove_dir_all(&scratch);
}

fn review_draft_revert(path: &str, undo: Option<crate::turn_review::UndoData>) -> crate::turn_review::NewRevert {
    use crate::turn_review::{NewRevert, RevertShape, RevertSource};
    NewRevert {
        turn: 1,
        path: path.into(),
        hunk: None,
        shape: RevertShape::Deleted,
        source: RevertSource::Panel,
        at_line: 1,
        reverted_to: Vec::new(),
        replaced: b"x\n".to_vec(),
        undo,
    }
}

#[test]
fn the_draft_survives_a_panel_reload() {
    use crate::turn_review::{Overview, ReviewDraft, Scope};
    let mut set = set();
    let tab = set.active();
    set.review_draft_mut(tab)
        .unwrap()
        .add_comment(1, "a.rs", 3, 4, vec!["let x = 1;".into()], "why?")
        .unwrap();
    let overview = Overview {
        session: "s".into(),
        scope: Scope::Turn,
        current: 1,
        turns: Vec::new(),
        files: Vec::new(),
        compared: false,
        pending_no_result: 0,
        notes: Vec::new(),
    };
    // A reload asks again: the answer carries what the tab holds, not what the WebView kept.
    let sent = crate::agent_bridge::serialize_review_for_js("r1", tab, &overview, set.review_draft(tab).unwrap());
    let sent: serde_json::Value = serde_json::from_str(&sent).unwrap();
    assert_eq!(sent["draft"]["comments"][0]["text"], "why?");
    assert_eq!(sent["draft"]["comments"][0]["anchor"][0], "let x = 1;");
    let empty = crate::agent_bridge::serialize_review_for_js("r2", tab, &overview, &ReviewDraft::default());
    let empty: serde_json::Value = serde_json::from_str(&empty).unwrap();
    assert_eq!(empty["draft"]["comments"].as_array().unwrap().len(), 0);
}

#[test]
fn undo_data_keeps_kind_and_mode() {
    use crate::turn_review::{UndoData, UndoState};
    let mut set = set();
    let tab = set.active();
    let undo = UndoData {
        pre: UndoState::Regular {
            blob: "pre".into(),
            mode: 0o755,
        },
        post: UndoState::Absent,
    };
    let id = set
        .review_draft_mut(tab)
        .unwrap()
        .record_revert(review_draft_revert("run.sh", Some(undo.clone())));
    let draft = set.review_draft(tab).unwrap();
    let last = draft.last_undoable().unwrap();
    assert_eq!(last.id, id);
    assert_eq!(last.new.undo, Some(undo));
}

#[test]
fn the_draft_is_dropped_with_its_tab() {
    let mut set = set();
    let first = set.active();
    set.review_draft_mut(first)
        .unwrap()
        .record_revert(review_draft_revert("a", None));
    let second = set.open(AgentId::Claude);
    assert!(set.review_draft(second).unwrap().is_empty(), "a new tab starts empty");
    set.remove(first).unwrap();
    assert!(set.review_draft(first).is_err(), "the draft went with its tab");
    assert!(set.review_draft_mut(first).is_err());
    assert!(set.review_draft(second).unwrap().is_empty());
}

#[test]
fn send_review_folds_the_message_as_typed() {
    let dir = workspace("tabs-send-review");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let text = "Review of your last turn: 1 comment, 0 reverts.\n\nComments:\n1. a.rs:3-4\n   > x\n   why?";
    let flush = set
        .send_review(tab, text, 5)
        .unwrap()
        .expect("an idle tab sends at once");
    assert!(flush.outcome.is_ok());
    assert_eq!(flush.typed, text);
    assert_eq!(provider.turns(), vec![text.to_string()], "wire == typed == text");
    until("the prompt in the transcript", || {
        let backend = set.get(tab).unwrap().live().unwrap();
        let projection = backend.projection();
        projection.user_prompts.last().is_some_and(|p| p.text == text)
    });
    assert!(set.get(tab).unwrap().session.queue.is_empty());
    shut_down_all(&mut set);
}

#[test]
fn send_review_queues_behind_a_running_turn() {
    let dir = workspace("tabs-send-review-queued");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(started("t1"));
    pump_until_running(&mut set, &dir, tab, true);

    assert!(
        set.send_review(tab, "the review", 5).unwrap().is_none(),
        "queued, not sent"
    );
    assert!(provider.turns().is_empty());
    assert_eq!(set.queue_texts(tab), vec!["the review".to_string()]);

    provider.queue(completed("t1", agent::TurnOutcome::Completed));
    pump_until_running(&mut set, &dir, tab, false);
    let flush = set.flush_queue(tab).expect("it goes out when the turn ends");
    assert_eq!(flush.typed, "the review");
    assert_eq!(provider.turns(), vec!["the review".to_string()]);
    shut_down_all(&mut set);
}

#[test]
fn send_review_on_a_tab_with_no_session_is_refused() {
    let mut set = set();
    let tab = set.active();
    assert!(
        set.send_review(tab, "the review", 5).is_err(),
        "an empty tab has no session"
    );
    assert_eq!(set.queued_count(), 0, "nothing was left behind");
    assert!(set.send_review(TabId(999), "the review", 5).is_err(), "no such tab");
}

#[test]
fn a_ticket_needs_a_tab_with_a_session_and_is_current_until_it_ends() {
    let dir = target_scratch("ticket-current").join("project");
    let mut set = set();
    let tab = set.active();
    assert!(set.review_ticket(tab).is_err(), "an empty tab has no session");
    assert_eq!(
        set.review_ticket(TabId(999)).unwrap_err(),
        "protocol: no tab 999",
        "the text a missing tab always gets"
    );
    give_session(&mut set, tab, &dir, "sess-ticket");
    let ticket = set.review_ticket(tab).unwrap();
    assert_eq!(ticket.session, "sess-ticket");
    assert!(ticket.is_current(&set));
    ticket.cancel();
    assert!(!ticket.is_current(&set), "a cancelled ticket is never current");
    assert!(!ticket.wanted.load(Ordering::SeqCst));
    shut_down_all(&mut set);
}

#[test]
fn a_ticket_is_not_current_for_another_session_of_the_same_tab() {
    let dir = target_scratch("ticket-session").join("project");
    let mut set = set();
    let tab = set.active();
    give_session(&mut set, tab, &dir, "sess-one");
    let ticket = set.review_ticket(tab).unwrap();
    let old = std::mem::replace(&mut set.get_mut(tab).unwrap().backend, TabBackend::NotStarted);
    if let TabBackend::Live(mut backend) = old {
        backend.shutdown();
    }
    assert!(!ticket.is_current(&set), "the tab holds no session now");
    give_session(&mut set, tab, &dir, "sess-two");
    assert!(!ticket.is_current(&set), "another session is not the one asked about");
    assert!(set.review_ticket(tab).unwrap().is_current(&set));
    shut_down_all(&mut set);
}

#[test]
fn removing_a_tab_cancels_its_tickets() {
    let dir = target_scratch("ticket-remove").join("project");
    let mut set = set();
    let tab = set.active();
    give_session(&mut set, tab, &dir, "sess-remove");
    let other = set.open(AgentId::Claude);
    let ticket = set.review_ticket(tab).unwrap();
    let kept = set.review_ticket(tab).unwrap();
    assert!(ticket.wanted.load(Ordering::SeqCst));
    let mut closed = set.remove(tab).unwrap();
    // Right after the removal, before any pump: the flag is what a worker's last check reads.
    assert!(!ticket.wanted.load(Ordering::SeqCst));
    assert!(
        !kept.wanted.load(Ordering::SeqCst),
        "every ticket of the tab, not the first"
    );
    assert!(!ticket.is_current(&set));
    assert_eq!(set.active(), other);
    if let Some(retired) = closed.retired.take() {
        retired.into_backend().shutdown();
    }
    shut_down_all(&mut set);
}

#[test]
fn removing_one_tab_leaves_another_tabs_tickets_alone() {
    let dir = target_scratch("ticket-other").join("project");
    let mut set = set();
    let first = set.active();
    give_session(&mut set, first, &dir, "sess-first");
    let second = set.open(AgentId::Claude);
    give_session(&mut set, second, &dir, "sess-second");
    let kept = set.review_ticket(second).unwrap();
    let mut closed = set.remove(first).unwrap();
    assert!(kept.is_current(&set));
    if let Some(retired) = closed.retired.take() {
        retired.into_backend().shutdown();
    }
    shut_down_all(&mut set);
}

#[test]
fn take_all_cancels_every_ticket() {
    let dir = target_scratch("ticket-take-all").join("project");
    let mut set = set();
    let first = set.active();
    give_session(&mut set, first, &dir, "sess-a");
    let second = set.open(AgentId::Claude);
    give_session(&mut set, second, &dir, "sess-b");
    let a = set.review_ticket(first).unwrap();
    let b = set.review_ticket(second).unwrap();
    let tabs = set.take_all();
    assert!(!a.wanted.load(Ordering::SeqCst));
    assert!(!b.wanted.load(Ordering::SeqCst));
    for tab in tabs {
        if let Some(retired) = tab.retired {
            retired.into_backend().shutdown();
        }
    }
}

#[test]
fn a_finished_ticket_leaves_no_watcher_behind() {
    let dir = target_scratch("ticket-drop").join("project");
    let mut set = set();
    let tab = set.active();
    give_session(&mut set, tab, &dir, "sess-drop");
    for _ in 0..50 {
        drop(set.review_ticket(tab).unwrap());
    }
    let _kept = set.review_ticket(tab).unwrap();
    assert_eq!(
        set.get(tab).unwrap().review_watchers.len(),
        1,
        "dropped tickets are swept the next time one is made"
    );
    shut_down_all(&mut set);
}

#[test]
fn the_availability_question_names_the_reason_without_a_tab() {
    let mut set = set();
    assert_eq!(set.review_unavailable(), Some(TURN_REVIEW_NEEDS_SIDECAR));
    set.turn_review_off();
    assert_eq!(set.review_unavailable(), Some(TURN_REVIEW_OFF));
    let tab = set.active();
    assert_eq!(set.review_session(tab).err().as_deref(), Some(TURN_REVIEW_OFF));
}
