//! Every path of the tab set that answers cards by itself leaves Claude's questions waiting: a
//! question is never in `pending_permissions` nor drawn as a card, and only `answer_question` and
//! `decline_question` answer one.
#![cfg(test)]
use super::*;
use crate::tab_restore::{PlannedTab, RestorePlan};
use crate::test_providers::RecordingProvider;
use agent::{AgentConversation, AgentDomainEvent, QuestionOptionSpec, QuestionSpec, QuestionState};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn workspace(label: &str) -> PathBuf {
    agent::state_dirs::redirect_state_to_a_test_root();
    let dir = agent::state_dirs::test_workspace_dir(label);
    std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
    dir
}

fn live(dir: &Path) -> (Arc<RecordingProvider>, AgentBackend) {
    let provider = Arc::new(RecordingProvider::default());
    let conversation =
        AgentConversation::create(provider.clone(), dir, agent::setting_sources::ProjectTrust::Untrusted).unwrap();
    (
        provider,
        AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        },
    )
}

fn set() -> TabSet {
    TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto)
}

/// A `Write` into `.git`: a card in Manual and Auto, answered in Bypass.
fn card(id: &str) -> AgentDomainEvent {
    AgentDomainEvent::PermissionRequested {
        permission_id: id.into(),
        tool_use_id: None,
        tool_name: "Write".into(),
        input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }),
        provider_prompt: None,
    }
}

fn asked(id: &str) -> AgentDomainEvent {
    AgentDomainEvent::QuestionAsked {
        interaction_id: id.into(),
        tool_use_id: Some(format!("toolu_{id}")),
        questions: vec![QuestionSpec {
            question: "Which layout?".into(),
            header: "Layout".into(),
            multi_select: false,
            options: ["Grid", "List"]
                .into_iter()
                .map(|label| QuestionOptionSpec {
                    label: label.into(),
                    description: String::new(),
                    preview: None,
                })
                .collect(),
        }],
        unsupported_reason: None,
    }
}

fn question(set: &TabSet, tab: TabId, id: &str) -> Option<QuestionState> {
    let backend = set.get(tab)?.live()?;
    let state = backend.projection().question(id).map(|record| record.state.clone());
    state
}

/// Pumps the set as the panel's tick does until `done`.
fn pump_until(set: &mut TabSet, dir: &Path, what: &str, done: impl Fn(&TabSet) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done(set) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        set.pump(dir, true);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn enter_bypass(set: &mut TabSet, tab: TabId) -> ConfirmOutcome {
    let plan = match set.cycle_mode(tab, ViewId::BASE) {
        Ok(ModeCycle::Confirm(plan)) => plan,
        other => panic!("expected a bypass prompt, got {other:?}"),
    };
    set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap()
}

fn shut_down_all(set: &mut TabSet) {
    for tab in set.take_all() {
        if let Some(retired) = tab.retired {
            retired.into_backend().shutdown();
        }
    }
}

fn waiting(label: &str) -> (PathBuf, TabSet, TabId, Arc<RecordingProvider>) {
    let dir = workspace(label);
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(asked("q1"));
    pump_until(&mut set, &dir, "question attention", |s| {
        s.get(tab).unwrap().attention.attention().pending == 1
    });
    (dir, set, tab, provider)
}

fn settle(provider: &RecordingProvider) {
    provider.queue(AgentDomainEvent::QuestionSettled {
        interaction_id: "q1".into(),
        outcome: agent::QuestionOutcome::Declined,
        answers: Vec::new(),
        reason: None,
    });
}

#[test]
fn closing_a_tab_with_a_question_asks() {
    let (_dir, mut set, tab, _) = waiting("question-close");
    assert!(!set.get(tab).unwrap().turn_running());
    assert!(set.close_needs_confirm(tab));
    assert!(
        tabs::close_prompt(&set.close_facts(tab).unwrap()).contains(&"closing cancels Claude's question".to_string())
    );
    assert_eq!(
        tabs::window_close_prompt(set.running_count(), 0, 0).as_deref(),
        Some("close window? 1 running (y/n)")
    );
    set.open(AgentId::Claude);
    assert_eq!(set.close_others_plan().unwrap().1, "close 1 other tab? 1 running (y/n)");
    assert!(set.take_for_handoff(tab).is_none());
    shut_down_all(&mut set);
}

#[test]
fn close_others_names_background_tabs_beside_outstanding_questions() {
    let (dir, mut set, tab, provider) = waiting("question-background-others");
    provider.queue(AgentDomainEvent::BackgroundCalls { running: 2 });
    pump_until(&mut set, &dir, "background count", |s| s.background_count() == 1);
    set.open(AgentId::Claude);
    assert_eq!(
        set.close_others_plan(),
        Some((
            vec![tab],
            "close 1 other tab? 1 running, 1 with background tasks (y/n)".into()
        ))
    );
    shut_down_all(&mut set);
}

fn task(id: &str, kind: agent::TaskKind, ambient: bool) -> agent::BackgroundTask {
    agent::BackgroundTask {
        task_id: id.into(),
        run_id: None,
        kind,
        task_type: String::new(),
        description: "untrusted description".into(),
        subagent_type: None,
        parent_task_id: None,
        tool_use_id: None,
        ambient,
        workflow_name: None,
    }
}

fn background_tab(label: &str, tasks: Vec<agent::BackgroundTask>) -> (PathBuf, TabSet, TabId) {
    let dir = workspace(label);
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let count = tasks.iter().filter(|task| !task.ambient).count() as u32;
    provider.queue(AgentDomainEvent::BackgroundTasksChanged {
        tasks,
        occurred_at: None,
    });
    provider.queue(AgentDomainEvent::BackgroundCalls { running: count });
    pump_until(&mut set, &dir, "background roster", |s| {
        s.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .background_tasks
            .rows
            .len()
            > 0
            && s.get(tab).unwrap().background_calls() == count
    });
    (dir, set, tab)
}

#[test]
fn close_asks_with_live_background_tasks_and_names_them() {
    let (_dir, mut set, tab) = background_tab(
        "background-close",
        vec![
            task("s1", agent::TaskKind::Shell, false),
            task("s2", agent::TaskKind::Shell, false),
            task("wf", agent::TaskKind::Workflow, false),
            task("ambient", agent::TaskKind::Monitor, true),
        ],
    );
    assert!(!set.get(tab).unwrap().turn_running());
    assert!(set.close_needs_confirm(tab));
    assert_eq!(
        tabs::close_prompt(&set.close_facts(tab).unwrap())[1],
        "2 shells, 1 background dynamic workflow running in the background"
    );
    assert!(set.take_for_handoff(tab).is_none());
    shut_down_all(&mut set);
}

#[test]
fn ambient_tasks_do_not_ask() {
    let (_dir, mut set, tab) = background_tab("ambient-close", vec![task("monitor", agent::TaskKind::Monitor, true)]);
    assert!(!set.close_needs_confirm(tab));
    assert_eq!(set.background_count(), 0);
    assert!(set.close_facts(tab).unwrap().background.is_none());
    assert_eq!(
        tabs::window_close_prompt(set.running_count(), set.queued_count(), set.background_count()),
        None
    );
    let retired = set.take_for_handoff(tab).expect("ambient work does not block handoff");
    retired.into_backend().shutdown();
    shut_down_all(&mut set);
}

#[test]
fn window_close_counts_background_tabs() {
    let (_dir, mut set, _) = background_tab(
        "background-window",
        vec![
            task("s1", agent::TaskKind::Shell, false),
            task("s2", agent::TaskKind::Shell, false),
        ],
    );
    set.open(AgentId::Claude);
    assert_eq!(set.background_count(), 1);
    assert_eq!(
        tabs::window_close_prompt(set.running_count(), set.queued_count(), set.background_count()).as_deref(),
        Some("close window? 1 with background tasks (y/n)")
    );
    shut_down_all(&mut set);
}

#[test]
fn close_others_counts_background_tabs() {
    let (_dir, mut set, tab) = background_tab("background-others", vec![task("s1", agent::TaskKind::Shell, false)]);
    assert_eq!(set.close_others_plan(), None);
    set.open(AgentId::Claude);
    assert_eq!(
        set.close_others_plan(),
        Some((vec![tab], "close 1 other tab? 1 with background tasks (y/n)".into()))
    );
    set.select(tab);
    assert_eq!(set.break_out(ViewId::new(2).unwrap()).unwrap(), tab);
    assert_eq!(set.close_others_plan(), None);
    shut_down_all(&mut set);
}

#[test]
fn a_typed_message_waits_behind_a_waiting_question() {
    let (dir, mut set, tab, provider) = waiting("question-send");
    let turn = FirstTurn {
        typed: "next".into(),
        wire: "next with context".into(),
        typed_by_person: true,
        shell_line: false,
        attachments: Vec::new(),
    };
    assert!(matches!(
        set.send_prompt(tab, "r1", ViewId::BASE, turn),
        Some(Sent::Prompt { outcome: Ok(()), .. })
    ));
    assert!(provider.turns().is_empty());
    assert!(set.flush_queue(tab).is_none());
    assert!(set.release_after_card().is_empty());
    settle(&provider);
    pump_until(&mut set, &dir, "question settled", |s| {
        s.get(tab).unwrap().attention.attention().pending == 0
    });
    assert_eq!(set.release_after_card().len(), 1);
    assert_eq!(provider.turns(), vec!["next with context"]);
    assert!(provider.interactions().is_empty());
    shut_down_all(&mut set);
}

/// `Ctrl+x Enter` with no turn running goes out as Enter's message would: behind a waiting card or question,
/// as soon as it settles.
#[test]
fn an_after_turn_send_waits_behind_a_waiting_question_then_goes() {
    let (dir, mut set, tab, provider) = waiting("question-after-turn");
    set.queue_after_turn(tab, "later", "later with context".into(), 1, agent::PromptOrigin::Typed)
        .unwrap();
    assert!(set.get(tab).unwrap().session.queue[0].behind_card);
    assert!(set.flush_queue(tab).is_none());
    assert!(set.release_after_card().is_empty());
    settle(&provider);
    pump_until(&mut set, &dir, "question settled", |s| {
        s.get(tab).unwrap().attention.attention().pending == 0
    });
    assert_eq!(set.release_after_card().len(), 1);
    assert_eq!(provider.turns(), vec!["later with context"]);
    shut_down_all(&mut set);
}

#[test]
fn a_companion_send_waits_behind_a_waiting_question() {
    let (dir, mut set, tab, provider) = waiting("question-companion");
    set.queue_message(
        tab,
        "companion",
        "companion".into(),
        1,
        false,
        agent::PromptOrigin::Composed,
    )
    .unwrap();
    assert!(set.flush_queue(tab).is_none());
    assert!(provider.turns().is_empty());
    settle(&provider);
    pump_until(&mut set, &dir, "question settled", |s| {
        s.get(tab).unwrap().attention.attention().pending == 0
    });
    assert!(set.flush_queue(tab).is_some());
    assert_eq!(provider.turns(), vec!["companion"]);
    shut_down_all(&mut set);
}

#[test]
fn question_draft_survives_a_tab_switch() {
    let (_dir, mut set, tab, provider) = waiting("question-switch");
    let draft = serde_json::json!({"page": 0, "picks": [1], "text": "typed", "notes": "notes"});
    set.set_question_draft(tab, ViewId::BASE, "q1", draft.clone()).unwrap();
    set.open(AgentId::Claude);
    set.select(tab);
    assert_eq!(set.get(tab).unwrap().view(true).question_draft["q1"], draft);
    let payload: serde_json::Value = serde_json::from_str(&set.tabs_payload()).unwrap();
    assert_eq!(payload["tabs"][0]["questionDraft"]["q1"], draft);
    assert!(provider.interactions().is_empty());
    assert!(set
        .set_question_draft(tab, ViewId::BASE, "q1", serde_json::json!("x".repeat(65536)))
        .is_err());
    shut_down_all(&mut set);
}

#[test]
fn a_typed_answer_edited_in_nvim_is_stored_for_the_dock_to_restore() {
    let (_dir, mut set, tab, _) = waiting("question-other-stored");
    set.set_question_draft(
        tab,
        ViewId::BASE,
        "q1",
        serde_json::json!({"page": 0, "focus": 1, "answers": [{"options": [1], "other": false, "text": "old", "notes": "n", "showNotes": false}]}),
    )
    .unwrap();
    set.store_question_other(tab, "q1", 0, "edited in nvim").unwrap();
    let draft = &set.get(tab).unwrap().view(true).question_draft["q1"];
    assert_eq!(draft["focus"], 1, "the rest of the draft is kept");
    assert_eq!(draft["answers"][0]["text"], "edited in nvim");
    assert_eq!(draft["answers"][0]["other"], true);
    assert_eq!(
        draft["answers"][0]["options"],
        serde_json::json!([]),
        "a typed answer replaces the picked option of a single-select question"
    );
    assert_eq!(draft["answers"][0]["notes"], "n");
    assert!(set.store_question_other(tab, "q1", 1, "x").is_err(), "no such question");
    assert!(set.store_question_other(tab, "q9", 0, "x").is_err(), "not waiting");
    shut_down_all(&mut set);
}

#[test]
fn a_typed_answer_edited_in_nvim_is_stored_without_an_earlier_draft() {
    let (_dir, mut set, tab, _) = waiting("question-other-fresh");
    set.store_question_other(tab, "q1", 0, "typed").unwrap();
    let draft = &set.get(tab).unwrap().view(true).question_draft["q1"];
    assert_eq!(draft["answers"][0]["text"], "typed");
    assert_eq!(draft["answers"][0]["other"], true);
    shut_down_all(&mut set);
}

#[test]
fn the_draft_is_dropped_on_settle() {
    let (dir, mut set, tab, provider) = waiting("question-settle-draft");
    set.set_question_draft(tab, ViewId::BASE, "q1", serde_json::json!({"text":"typed"}))
        .unwrap();
    settle(&provider);
    pump_until(&mut set, &dir, "draft dropped", |s| {
        s.get(tab).unwrap().session.question_draft.is_empty()
    });
    assert!(set
        .set_question_draft(tab, ViewId::BASE, "q1", serde_json::json!({}))
        .is_err());
    shut_down_all(&mut set);
}

#[test]
fn a_moved_tab_keeps_its_question() {
    let (dir, mut source, tab, provider) = waiting("question-move");
    provider.queue(AgentDomainEvent::SessionOpened {
        session_id: "fake-session".into(),
        provider_session_id: "sess-move".into(),
        model: "m".into(),
        cwd: dir.to_string_lossy().into_owned(),
    });
    pump_until(&mut source, &dir, "session id", |s| {
        s.get(tab).unwrap().provider_session_id().is_some()
    });
    source
        .set_question_draft(tab, ViewId::BASE, "q1", serde_json::json!({"text":"kept"}))
        .unwrap();
    let frozen = loop {
        match source.freeze_for_move(tab) {
            Err(MoveRefusedWhy::Folding) => std::thread::sleep(Duration::from_millis(1)),
            result => break result.unwrap(),
        }
    };
    assert!(source
        .decline_question(tab, ViewId::BASE, "q1", agent::QuestionDeclineKind::Cancel, Vec::new())
        .is_err());
    let record = serde_json::from_str(&serde_json::to_string(&frozen.tab).unwrap()).unwrap();
    let chat = frozen.chat.unwrap();
    let backend = AgentBackend::adopt_moved(
        chat.core,
        chat.conversation,
        chat.backend,
        agent::MovedProvider {
            agent: provider.clone(),
            external: None,
            lease: None,
        },
    )
    .unwrap();
    let mut dest = set();
    let arrived = dest.adopt_moved(record, backend, ViewId::BASE);
    assert_eq!(question(&dest, arrived, "q1"), Some(QuestionState::Waiting));
    assert_eq!(
        dest.get(arrived).unwrap().view(true).question_draft["q1"]["text"],
        "kept"
    );
    assert_eq!(dest.get(arrived).unwrap().attention.attention().pending, 1);
    assert!(dest
        .decline_question(
            arrived,
            ViewId::new(2).unwrap(),
            "q1",
            agent::QuestionDeclineKind::Cancel,
            Vec::new()
        )
        .is_err());
    dest.decline_question(
        arrived,
        ViewId::BASE,
        "q1",
        agent::QuestionDeclineKind::Cancel,
        Vec::new(),
    )
    .unwrap();
    assert_eq!(provider.interactions().len(), 1);
    source.hand_over(tab).unwrap();
    shut_down_all(&mut source);
    shut_down_all(&mut dest);
}

#[test]
fn confirm_bypass_leaves_a_waiting_question() {
    let dir = workspace("question-confirm-bypass");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue_all(vec![card("c1"), asked("q1")]);
    pump_until(&mut set, &dir, "the card and the question", |set| {
        set.get(tab).unwrap().attention.attention().pending == 2 && question(set, tab, "q1").is_some()
    });

    assert_eq!(enter_bypass(&mut set, tab), ConfirmOutcome::Entered { approved: 1 });
    assert_eq!(provider.resolutions(), vec![("c1".to_string(), true)], "the card only");
    assert!(provider.interactions().is_empty());
    assert_eq!(question(&set, tab, "q1"), Some(QuestionState::Waiting));
    shut_down_all(&mut set);
}

/// `Ctrl+y` and BROWSE's `a`/`d` arrive as a card answer naming an id; a question's id is no card's,
/// so either is refused and the question waits.
#[test]
fn ctrl_y_never_answers_a_question() {
    let dir = workspace("question-ctrl-y");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(asked("q1"));
    pump_until(&mut set, &dir, "the question", |set| question(set, tab, "q1").is_some());

    set.pump(&dir, true);
    for decision in [
        agent::PermissionDecision::Allow,
        agent::PermissionDecision::Deny { reason: None },
    ] {
        assert!(set.answer_card(tab, ViewId::BASE, "q1", decision).is_err());
    }
    assert!(provider.resolutions().is_empty());
    assert!(provider.interactions().is_empty());
    assert_eq!(question(&set, tab, "q1"), Some(QuestionState::Waiting));
    assert_eq!(
        set.get(tab).unwrap().attention.attention().pending,
        1,
        "the question counts without becoming a card"
    );
    shut_down_all(&mut set);
}

#[test]
fn a_bypass_resync_and_a_restore_in_bypass_leave_a_question() {
    // The resync: a batch that overflows the page's queue, swept in bypass (`approve_pending`).
    let dir = workspace("question-bypass-resync");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    assert_eq!(enter_bypass(&mut set, tab), ConfirmOutcome::Entered { approved: 0 });
    let mut overflow = vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }];
    for i in 0..300 {
        overflow.push(AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: agent::ContentKind::Text,
            text: format!("chunk {i}"),
        });
    }
    overflow.extend([asked("q1"), card("lost1")]);
    provider.queue_all(overflow);
    let deadline = Instant::now() + Duration::from_secs(60);
    let snapshot = loop {
        assert!(Instant::now() < deadline, "timed out waiting for the resync");
        let out = set.pump(&dir, true);
        if let Some(payload) = out.active_payload {
            let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
            if value["kind"] == "snapshot" && value["state"]["questions"].is_array() {
                break value;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    pump_until(&mut set, &dir, "the swept card", |set| {
        set.get(tab).unwrap().session.host_answered.contains("lost1")
    });
    assert_eq!(provider.resolutions(), vec![("lost1".to_string(), true)]);
    assert!(provider.interactions().is_empty());
    assert_eq!(question(&set, tab, "q1"), Some(QuestionState::Waiting));
    assert_eq!(
        snapshot["state"]["questions"][0]["state"]["kind"], "waiting",
        "{snapshot}"
    );
    shut_down_all(&mut set);

    // A restore whose `y` kept a saved tab in bypass: its session's events meet bypass on arrival.
    let mut set = set_restored_in_bypass();
    let tab = set.active();
    assert_eq!(set.get(tab).unwrap().mode(), SessionModeChoice::Bypass, "the premise");
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue_all(vec![card("c1"), asked("q1")]);
    pump_until(&mut set, &dir, "the card answered and the question folded", |set| {
        set.get(tab).unwrap().session.host_answered.contains("c1") && question(set, tab, "q1").is_some()
    });
    assert_eq!(provider.resolutions(), vec![("c1".to_string(), true)]);
    assert!(provider.interactions().is_empty());
    assert_eq!(question(&set, tab, "q1"), Some(QuestionState::Waiting));
    shut_down_all(&mut set);
}

/// A set whose one saved tab, saved in bypass, came back in bypass on the restore question's `y`.
fn set_restored_in_bypass() -> TabSet {
    let mut set = set();
    let from = set.active();
    let plan = RestorePlan {
        items: vec![PlannedTab {
            agent: AgentId::Claude,
            provider_session_id: "r1".into(),
            name: None,
            title: Some("saved".into()),
            saved_mode: SessionModeChoice::Bypass,
            label: "saved".into(),
            view: None,
            turn_running: false,
            draft: None,
            row_key: None,
            plan_origin: None,
        }],
        skipped: Vec::new(),
        active_session: Some("r1".into()),
    };
    let RestoreStep::Confirm(prompt) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("a tab saved in bypass is asked about");
    };
    let go = set.answer_restore(prompt.nonce, true).unwrap();
    set.start_restore(from, go, |_, _| Ok(RestoreConnect::Started(mpsc::channel().1)));
    set
}

/// The ungated-mode tripwire stops the tab before anything in its batch is answered, a waiting
/// question included.
#[test]
fn the_ungated_mode_tripwire_answers_no_question() {
    let dir = workspace("question-tripwire");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    assert_eq!(enter_bypass(&mut set, tab), ConfirmOutcome::Entered { approved: 0 });
    provider.queue(asked("q1"));
    pump_until(&mut set, &dir, "the question", |set| question(set, tab, "q1").is_some());
    provider.queue_all(vec![
        AgentDomainEvent::UngatedCliMode {
            reported: "bypassPermissions".into(),
            detail: "SessionReady".into(),
        },
        card("behind-the-trip"),
    ]);
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut tripped = Vec::new();
    while tripped.is_empty() {
        assert!(Instant::now() < deadline, "timed out waiting for the trip");
        tripped.extend(set.pump(&dir, true).tripped);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(provider.interactions().is_empty(), "the question was not answered");
    assert!(provider.resolutions().is_empty(), "nor was anything else");
    for t in tripped {
        t.retired.into_backend().shutdown();
    }
    shut_down_all(&mut set);
}
