//! Tabs running another agent than Claude: the reviewer each turn carries, how a mode change reaches
//! the answers and the turns, what is saved and how it comes back, and the presence of a closed tab.
#![cfg(test)]

use super::*;
use crate::tab_restore::PlannedTab;
use crate::test_providers::{empty_trust_plan, ExternalRecordingProvider};
use agent::{Reviewer, TurnPolicy};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The other agents' tabs of a saved set, in order.
fn others(saved: &SavedTabs) -> Vec<&SavedTab> {
    saved.other_tabs().collect()
}

/// Where the tab that comes back on screen stands among the other agents' tabs, when it is one.
fn others_active(saved: &SavedTabs) -> Option<usize> {
    let index = saved.active.filter(|i| saved.tabs[*i].agent.is_external())?;
    Some(index - saved.claude_tabs().count())
}

fn workspace(label: &str) -> PathBuf {
    agent::state_dirs::redirect_state_to_a_test_root();
    let dir = agent::state_dirs::test_workspace_dir(label);
    std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
    dir
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A window with one Codex tab, live on a scripted provider, in `mode`.
fn codex_tab(dir: &Path, mode: SessionModeChoice) -> (TabSet, TabId, Arc<ExternalRecordingProvider>) {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let tab = set.open(AgentId::Codex);
    let provider = Arc::new(ExternalRecordingProvider::codex());
    let backend =
        AgentBackend::start_external_with(AgentId::Codex, provider.clone(), dir, empty_trust_plan(), mode, None)
            .unwrap();
    let t = set.get_mut(tab).unwrap();
    t.mode = mode;
    t.backend = TabBackend::Live(backend);
    (set, tab, provider)
}

fn turn_completed(turn: &str) -> AgentDomainEvent {
    AgentDomainEvent::TurnCompleted {
        turn_id: turn.into(),
        outcome: agent::TurnOutcome::Completed,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
        detail: Default::default(),
    }
}

fn command(id: &str) -> AgentDomainEvent {
    AgentDomainEvent::PermissionRequested {
        permission_id: id.into(),
        tool_use_id: Some(format!("item-{id}")),
        tool_name: "commandExecution".into(),
        input: serde_json::json!({ "command": "cargo publish" }),
        provider_prompt: None,
    }
}

fn send(set: &mut TabSet, tab: TabId, text: &str) {
    match set.send_now(tab, text, text.to_string(), 0).unwrap() {
        SendNow::Flushed(Some(Flush { outcome: Ok(_), .. })) => {}
        SendNow::Flushed(Some(Flush { outcome: Err(e), .. })) => panic!("refused: {}", e.message),
        _ => panic!("nothing was sent"),
    }
}

fn reviewer(r: Reviewer) -> TurnPolicy {
    TurnPolicy {
        reviewer: r,
        sandbox: agent::Sandbox::Project,
    }
}

fn background_count(set: &TabSet, tab: TabId) -> u32 {
    set.get(tab).unwrap().live().unwrap().projection().background_calls
}

/// The turn ids the tab set has reported as begun since the last call.
fn began(set: &mut TabSet) -> Vec<String> {
    set.take_client_news()
        .into_iter()
        .filter_map(|(_, news)| match news {
            ClientCallNews::TurnBegan { turn_id } => Some(turn_id),
            _ => None,
        })
        .collect()
}

#[test]
fn an_external_tab_reports_its_turns() {
    let dir = workspace("codex-turn-news");
    let (mut set, tab, provider) = codex_tab(&dir, SessionModeChoice::Auto);
    let mut told = Vec::new();
    for turn in ["turn-1", "turn-2"] {
        provider
            .inner
            .queue(AgentDomainEvent::TurnStarted { turn_id: turn.into() });
        until("the turn to be reported", || {
            set.pump(&dir, true);
            told.extend(began(&mut set));
            told.last().is_some_and(|last| last == turn)
        });
        provider.inner.queue(turn_completed(turn));
        until("the turn to end", || {
            set.pump(&dir, true);
            set.get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .active_turn_id
                .is_none()
        });
        told.extend(began(&mut set));
    }
    assert_eq!(
        told,
        ["turn-1", "turn-2"],
        "one report for each turn, none for an ending"
    );
    for taken in set.take_all() {
        if let Some(retired) = taken.retired {
            retired.into_backend().shutdown();
        }
    }
}

#[test]
fn a_codex_tab_with_commands_left_running_asks() {
    let dir = workspace("codex-background-close");
    let (mut set, tab, provider) = codex_tab(&dir, SessionModeChoice::Auto);
    provider.inner.queue(AgentDomainEvent::BackgroundCalls { running: 2 });
    until("background commands", || background_count(&set, tab) == 2);
    assert!(set.close_needs_confirm(tab));
    assert_eq!(set.background_count(), 1);
    assert_eq!(
        tabs::close_prompt(&set.close_facts(tab).unwrap())[1],
        "2 commands running in the background"
    );
    for taken in set.take_all() {
        if let Some(retired) = taken.retired {
            retired.into_backend().shutdown();
        }
    }
}

/// The confirmed stop reaches the conversation of the tab it names, and only when the count it was
/// asked about is the tab's count and no turn runs. Every refusal is benign and leaves the session
/// live; a tab with no session, and one that is gone, stop nothing.
#[test]
fn stopping_the_background_reaches_the_named_tab_and_a_refusal_leaves_it_alive() {
    let dir = workspace("external-stop-background");
    let (mut set, tab, provider) = codex_tab(&dir, SessionModeChoice::Auto);
    let idle = set.open(AgentId::Codex);
    let refused = set.stop_background(tab, 1).unwrap_err();
    assert!(refused.benign);
    assert_eq!(refused.message, NOTHING_IN_THE_BACKGROUND);

    provider.inner.queue(AgentDomainEvent::BackgroundCalls { running: 2 });
    until("the count is folded", || background_count(&set, tab) == 2);
    let refused = set.stop_background(tab, 1).unwrap_err();
    assert!(refused.benign);
    assert!(
        refused
            .message
            .starts_with("2 commands now running in the background, not 1"),
        "{}",
        refused.message
    );
    let refused = set.stop_background(idle, 2).unwrap_err();
    assert!(refused.benign);
    assert_eq!(refused.message, "no active session");
    assert!(set.stop_background(TabId(9_999), 2).unwrap_err().benign);

    provider.inner.queue(AgentDomainEvent::TurnStarted {
        turn_id: "turn-1".into(),
    });
    until("the turn is folded", || {
        set.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .active_turn_id
            .is_some()
    });
    let refused = set.stop_background(tab, 2).unwrap_err();
    assert!(refused.benign);
    assert!(refused.message.starts_with("a turn is running"), "{}", refused.message);
    provider.inner.queue(turn_completed("turn-1"));
    until("the turn ends", || {
        set.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .active_turn_id
            .is_none()
    });

    // The agent refused it after all (the commands ended a moment ago): said, and nothing else.
    provider.refuse_next_stop(agent::ProviderErrorCode::NoActiveTurn);
    let refused = set.stop_background(tab, 2).unwrap_err();
    assert!(refused.benign, "{}", refused.message);
    assert!(set.get(tab).unwrap().live().is_some());
    assert_eq!(provider.stops(), 0, "nothing reached the agent's stop yet");

    set.stop_background(tab, 2).unwrap();
    assert_eq!(provider.stops(), 1);
    until("the count drops", || background_count(&set, tab) == 0);
}

/// The window's root, as the gate is given it at every pump, is what a turn is judged by: past a start, a
/// conversation's own path is what a replaced folder resolved to.
#[test]
fn a_turn_is_judged_by_the_root_the_window_holds() {
    let target = workspace("external-window-root-target");
    let window_root = target.with_file_name(format!("{}-link", target.file_name().unwrap().to_string_lossy()));
    std::os::unix::fs::symlink(&target, &window_root).unwrap();
    let (mut set, tab, provider) = codex_tab(&window_root, SessionModeChoice::Auto);
    set.pump(&window_root, true);
    send(&mut set, tab, "first");
    assert_eq!(
        provider.policies(),
        vec![TurnPolicy {
            reviewer: Reviewer::User,
            sandbox: agent::Sandbox::ReadOnly,
        }]
    );
}

#[test]
fn send_turn_carries_reviewer_from_mode() {
    let dir = workspace("external-reviewer-from-mode");
    let (mut set, tab, provider) = codex_tab(&dir, SessionModeChoice::Auto);
    assert_eq!(set.get(tab).unwrap().agent(), &AgentId::Codex);
    send(&mut set, tab, "first");
    provider.inner.queue(AgentDomainEvent::TurnStarted {
        turn_id: "turn-1".into(),
    });
    provider.inner.queue(turn_completed("turn-1"));
    until("the first turn ends", || set.pump(&dir, true).turn_ended.contains(&tab));
    // Bypass is entered as always: asked first, then confirmed.
    let plan = match set.cycle_mode(tab, ViewId::BASE) {
        Ok(ModeCycle::Confirm(plan)) => plan,
        other => panic!("expected a question, got {other:?}"),
    };
    set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();
    send(&mut set, tab, "second");
    assert_eq!(
        provider.policies(),
        vec![reviewer(Reviewer::Agent), reviewer(Reviewer::User)],
        "Auto's turn is reviewed by the agent, Bypass's reaches Eitri"
    );
    for t in set.take_all() {
        if let Some(retired) = t.retired {
            retired.into_backend().shutdown();
        }
    }
}

#[test]
fn mode_switch_mid_turn_changes_answers_now_and_reviewer_next_turn() {
    let dir = workspace("external-mid-turn-switch");
    let (mut set, tab, provider) = codex_tab(&dir, SessionModeChoice::Auto);
    send(&mut set, tab, "first");
    provider.inner.queue(AgentDomainEvent::TurnStarted {
        turn_id: "turn-1".into(),
    });
    provider.inner.queue(command("p1"));
    until("the Auto card is waiting", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 1
    });
    assert!(
        provider.inner.decisions().is_empty(),
        "Auto drew a card and answered nothing"
    );

    // Entering bypass mid-turn: the waiting card is approved, and what arrives next is allowed
    // at once, while the running turn keeps the reviewer it started with.
    let plan = match set.cycle_mode(tab, ViewId::BASE) {
        Ok(ModeCycle::Confirm(plan)) => plan,
        other => panic!("expected a question, got {other:?}"),
    };
    set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();
    provider.inner.queue(command("p2"));
    until("both answered", || {
        set.pump(&dir, true);
        provider.inner.decisions().len() == 2
    });
    assert_eq!(
        provider.inner.decisions(),
        vec![
            ("p1".to_string(), agent::PermissionDecision::Allow),
            ("p2".to_string(), agent::PermissionDecision::Allow)
        ]
    );
    assert_eq!(provider.policies(), vec![reviewer(Reviewer::Agent)], "no new turn yet");

    // Out of bypass before the turn ends (to manual): the next request is a card again, at once.
    assert_eq!(
        set.cycle_mode(tab, ViewId::BASE),
        Ok(ModeCycle::Changed(SessionModeChoice::Manual))
    );
    provider.inner.queue(command("p3"));
    until("the card after leaving bypass", || {
        set.pump(&dir, true);
        set.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .pending_permissions
            .contains_key("p3")
    });
    set.pump(&dir, true);
    assert_eq!(provider.inner.decisions().len(), 2, "p3 waits for the user");
    // The provider reports each answer's resolution, and the user's interrupt ends p3.
    for (id, outcome) in [
        ("p1", agent::PermissionOutcome::Allowed),
        ("p2", agent::PermissionOutcome::Allowed),
        ("p3", agent::PermissionOutcome::CancelledByInterrupt),
    ] {
        provider.inner.queue(AgentDomainEvent::PermissionResolved {
            permission_id: id.into(),
            outcome,
        });
    }
    provider.inner.queue(turn_completed("turn-1"));
    until("the turn ends", || set.pump(&dir, true).turn_ended.contains(&tab));
    // The next turn carries the mode the tab is in now: manual hands every review to the user.
    send(&mut set, tab, "second");
    assert_eq!(
        provider.policies(),
        vec![reviewer(Reviewer::Agent), reviewer(Reviewer::User)]
    );
    for t in set.take_all() {
        if let Some(retired) = t.retired {
            retired.into_backend().shutdown();
        }
    }
}

/// A Codex tab in manual: its turns hand every review to the user (reviewer `user`), and every
/// request that reaches Eitri is a card, none answered by Eitri.
#[test]
fn a_manual_codex_tab_cards_every_request_and_reviews_nothing_itself() {
    let dir = workspace("external-manual");
    let (mut set, tab, provider) = codex_tab(&dir, SessionModeChoice::Manual);
    send(&mut set, tab, "first");
    provider.inner.queue(AgentDomainEvent::TurnStarted {
        turn_id: "turn-1".into(),
    });
    provider.inner.queue(command("p1"));
    provider.inner.queue(command("p2"));
    until("both cards are waiting", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 2
    });
    set.pump(&dir, true);
    assert!(provider.inner.decisions().is_empty(), "manual answers nothing itself");
    assert_eq!(provider.policies(), vec![reviewer(Reviewer::User)]);
    // Manual -> auto moves at once; the reviewer follows at the next turn.
    assert_eq!(
        set.cycle_mode(tab, ViewId::BASE),
        Ok(ModeCycle::Changed(SessionModeChoice::Auto))
    );
    for id in ["p1", "p2"] {
        provider.inner.queue(AgentDomainEvent::PermissionResolved {
            permission_id: id.into(),
            outcome: agent::PermissionOutcome::CancelledByInterrupt,
        });
    }
    provider.inner.queue(turn_completed("turn-1"));
    until("the turn ends", || set.pump(&dir, true).turn_ended.contains(&tab));
    send(&mut set, tab, "second");
    assert_eq!(
        provider.policies(),
        vec![reviewer(Reviewer::User), reviewer(Reviewer::Agent)]
    );
    for t in set.take_all() {
        if let Some(retired) = t.retired {
            retired.into_backend().shutdown();
        }
    }
}

/// An ACP tab saved in manual (a hand-edited file: no ACP tab is ever in manual) comes back in
/// auto, a mode its agent offers.
#[test]
fn an_acp_tab_saved_in_manual_comes_back_in_auto() {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let plan = RestorePlan {
        items: vec![planned(AgentId::Acp("gem".into()), "g", SessionModeChoice::Manual)],
        skipped: Vec::new(),
        active_session: None,
    };
    let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("nothing to ask")
    };
    let from = set.active();
    set.start_restore(from, go, |_, _| Ok(RestoreConnect::Started(mpsc::channel().1)));
    assert_eq!(set.get(from).unwrap().mode(), SessionModeChoice::Auto);
    assert_eq!(set.get(from).unwrap().agent(), &AgentId::Acp("gem".into()));
    let note = set
        .take_mode_note()
        .expect("the restore says the tab is not in its saved mode");
    let number = set.get(from).unwrap().number;
    assert!(note.starts_with("gem tabs have no manual mode"), "{note}");
    assert!(note.ends_with(&format!("tab {number} is in auto")), "{note}");
    assert_eq!(set.take_mode_note(), None, "once");
}

/// Under a manual window default, a restore whose first tab is an ACP agent's brings the next tabs back
/// into new tabs of their own agents, in their saved modes, and says nothing: no tab was kept from the
/// mode it was to have (the new tab is opened for the ACP agent of the tab before it, and only then
/// given its own agent and mode).
#[test]
fn a_restore_after_an_acp_tab_names_no_tab_it_did_not_clamp() {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Manual);
    assert_eq!(set.take_mode_note(), None);
    let plan = RestorePlan {
        items: vec![
            planned(AgentId::Acp("gem".into()), "g", SessionModeChoice::Auto),
            planned(AgentId::Claude, "c", SessionModeChoice::Manual),
            planned(AgentId::Codex, "x", SessionModeChoice::Manual),
        ],
        skipped: Vec::new(),
        active_session: None,
    };
    let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("nothing to ask")
    };
    let from = set.active();
    set.start_restore(from, go, |_, _| Ok(RestoreConnect::Started(mpsc::channel().1)));
    let of = |id: &str| {
        set.tabs()
            .iter()
            .find(|t| matches!(&t.backend, TabBackend::Starting(p) if p.resume.as_deref() == Some(id)))
            .map(|t| (t.agent().clone(), t.mode()))
            .expect("a tab for the session")
    };
    assert_eq!(of("g"), (AgentId::Acp("gem".into()), SessionModeChoice::Auto));
    assert_eq!(of("c"), (AgentId::Claude, SessionModeChoice::Manual));
    assert_eq!(of("x"), (AgentId::Codex, SessionModeChoice::Manual));
    assert_eq!(set.take_mode_note(), None, "no tab was kept from its mode");
}

/// A session that turns out to be an ACP agent's when its backend arrives, in a tab that was in manual,
/// takes auto, and the page is told which tab.
#[test]
fn a_backend_arriving_in_a_mode_its_agent_lacks_is_said() {
    let dir = workspace("arrival-clamp");
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Manual);
    let tab = set.active();
    assert_eq!(set.get(tab).unwrap().mode(), SessionModeChoice::Manual);
    let agent = AgentId::Acp("gem".into());
    let provider = Arc::new(ExternalRecordingProvider::named("acp:gem", false));
    let backend = AgentBackend::start_external_with(
        agent.clone(),
        provider.clone(),
        &dir,
        empty_trust_plan(),
        SessionModeChoice::Auto,
        None,
    )
    .unwrap();
    let (tx, rx) = mpsc::channel();
    tx.send(Ok(backend)).unwrap();
    set.get_mut(tab).unwrap().backend = TabBackend::Starting(PendingStart {
        request_id: "r1".into(),
        result_rx: rx,
        first_turn: None,
        resume: None,
        resumed_title: None,
        resumed_name: None,
    });
    set.collect_starts();
    assert_eq!(set.get(tab).unwrap().agent(), &agent);
    assert_eq!(set.get(tab).unwrap().mode(), SessionModeChoice::Auto);
    let note = set.take_mode_note().expect("the page is told");
    let number = set.get(tab).unwrap().number;
    assert!(note.starts_with("gem tabs have no manual mode"), "{note}");
    assert!(note.ends_with(&format!("tab {number} is in auto")), "{note}");
    for t in set.take_all() {
        if let Some(retired) = t.retired {
            retired.into_backend().shutdown();
        }
    }
}

fn planned(agent: AgentId, id: &str, mode: SessionModeChoice) -> PlannedTab {
    PlannedTab {
        agent,
        provider_session_id: id.to_string(),
        name: None,
        title: Some(format!("title of {id}")),
        saved_mode: mode,
        label: format!("title of {id}"),
        view: None,
        turn_running: false,
        draft: None,
        row_key: None,
    }
}

#[test]
fn restore_asks_bypass_then_trust_then_starts_for_external_tabs() {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let from = set.active();
    let plan = RestorePlan {
        items: vec![
            planned(AgentId::Claude, "claude-a", SessionModeChoice::Auto),
            planned(AgentId::Codex, "codex-b", SessionModeChoice::Bypass),
        ],
        skipped: Vec::new(),
        active_session: Some("codex-b".into()),
    };
    // 1. The bypass question, since a Codex tab was saved in bypass.
    let RestoreStep::Confirm(prompt) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("a saved bypass tab is asked about first")
    };
    let go = set.answer_restore(prompt.nonce, true).unwrap();
    assert_eq!(
        go.sessions(),
        vec![
            (AgentId::Claude, "claude-a".to_string()),
            (AgentId::Codex, "codex-b".to_string())
        ]
    );
    // 2. The trust question, before anything is started.
    set.defer_start(from, DeferredStart::Restore { go }).unwrap();
    let mut asked: Vec<(AgentId, String)> = Vec::new();
    assert!(set.tabs().iter().all(|t| !matches!(t.backend, TabBackend::Starting(_))));
    let nonce = set.ask_trust(from).unwrap();
    set.mark_recording(from, nonce, ViewId::BASE).unwrap();
    let Some(DeferredStart::Restore { go }) = set.take_recorded(from) else {
        panic!("the recorded answer hands the restore back")
    };
    // 3. Then the starts, each told its agent.
    let run = set.start_restore(from, go, |agent, id| {
        asked.push((agent.clone(), id.to_string()));
        Ok(RestoreConnect::Started(mpsc::channel().1))
    });
    assert_eq!(
        asked,
        vec![
            (AgentId::Claude, "claude-a".to_string()),
            (AgentId::Codex, "codex-b".to_string())
        ]
    );
    let codex = set
        .tab_with_session(&AgentId::Codex, "codex-b")
        .expect("the Codex tab is starting");
    let t = set.get(codex).unwrap();
    assert_eq!(t.agent(), &AgentId::Codex);
    assert_eq!(t.mode(), SessionModeChoice::Bypass, "answered y, so bypass comes back");
    assert_eq!(set.active(), codex, "the saved active tab is shown");
    assert_eq!(set.get(from).unwrap().agent(), &AgentId::Claude);
    // A failed start puts the first tab back as it was, its agent included.
    assert_eq!(run.back_to(from), Some((SessionModeChoice::Auto, AgentId::Claude)));
}

/// A saved tab of another agent than the one the restore's own trust answer was about comes back
/// holding its resume for its own agent's question: listed among the saved tabs while it waits,
/// in the mode the bypass question gave it, and counted as brought back.
#[test]
fn a_restored_external_tab_waits_for_its_own_trust_answer() {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let from = set.active();
    let plan = RestorePlan {
        items: vec![
            planned(AgentId::Claude, "claude-a", SessionModeChoice::Auto),
            planned(AgentId::Codex, "codex-b", SessionModeChoice::Bypass),
        ],
        skipped: Vec::new(),
        active_session: Some("codex-b".into()),
    };
    let RestoreStep::Confirm(prompt) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("a saved bypass tab is asked about first")
    };
    let go = set.answer_restore(prompt.nonce, true).unwrap();
    assert_eq!(
        go.trust_agent(),
        Some(AgentId::Claude),
        "the Claude tabs start on its answer"
    );
    let mut started: Vec<String> = Vec::new();
    let run = set.start_restore(from, go, |agent, id| {
        if agent.is_external() {
            return Ok(RestoreConnect::AwaitTrust);
        }
        started.push(id.to_string());
        Ok(RestoreConnect::Started(mpsc::channel().1))
    });
    assert_eq!(started, ["claude-a"]);
    let codex = set
        .tab_with_session(&AgentId::Codex, "codex-b")
        .expect("the Codex tab came back");
    assert_eq!(run.awaiting_trust(), &[codex]);
    let t = set.get(codex).unwrap();
    assert_eq!(t.agent(), &AgentId::Codex);
    assert_eq!(t.mode(), SessionModeChoice::Bypass);
    assert!(matches!(
        &t.backend,
        TabBackend::AwaitingTrust(AwaitingTrust {
            start: DeferredStart::Resume { provider_session_id, .. },
            asking: None,
            recording: false,
        }) if provider_session_id == "codex-b"
    ));
    assert_eq!(set.active(), codex, "the saved active tab is shown");
    let saved = set.saved_snapshot("conv").unwrap();
    assert_eq!(others(&saved).len(), 1, "kept in the saved file while it waits");
    assert_eq!(others(&saved)[0].provider_session_id, "codex-b");
    assert_eq!(others_active(&saved), Some(0));
    assert_eq!(run.outcome(), None, "claude-a is still connecting");
    assert_eq!(run.restored, 1, "the waiting tab is back");

    // A restore of another agent's tabs alone waits for that agent's answer.
    let only = RestorePlan {
        items: vec![planned(AgentId::Codex, "codex-c", SessionModeChoice::Auto)],
        skipped: Vec::new(),
        active_session: None,
    };
    let RestoreStep::Go(go) = set.begin_restore(only, BypassPolicy::Ask) else {
        panic!("nothing in bypass, nothing to ask")
    };
    assert_eq!(go.trust_agent(), Some(AgentId::Codex));
    drop(go);
}

#[test]
fn an_external_tab_is_saved_beside_the_claude_tabs_and_is_the_active_one() {
    let dir = workspace("external-saved");
    let (mut set, tab, provider) = codex_tab(&dir, SessionModeChoice::Bypass);
    until("the thread is named", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().provider_session_id().is_some()
    });
    set.select(tab);
    let saved = set.saved_snapshot("conv").unwrap();
    assert!(
        saved.claude_tabs().next().is_none(),
        "the empty Claude tab has nothing to save"
    );
    assert_eq!(
        saved.tabs,
        vec![SavedTab {
            agent: AgentId::Codex,
            conversation_id: "conv".into(),
            provider_session_id: "thread-1".into(),
            name: None,
            mode: SessionModeChoice::Bypass,
            view: None,
            window: None,
            turn_running: false,
            draft: None,
            row_key: Some(set.get(tab).unwrap().row_key().as_str().into()),
        }]
    );
    assert_eq!(others_active(&saved), Some(0));
    // A window on the legacy Claude backend still keeps its Codex tabs.
    set.kind = BackendKind::Legacy;
    assert_eq!(others(&set.saved_snapshot("conv").unwrap()).len(), 1);
    drop(provider);
    for t in set.take_all() {
        if let Some(retired) = t.retired {
            retired.into_backend().shutdown();
        }
    }
}

/// A restored tab whose start is refused (a relative `CODEX_HOME`, a missing binary) stays on screen
/// as a failed tab, and the saved-tabs file keeps its conversation, under its name and mode, until
/// the tab is closed; starting it over drops it, as the user chose a fresh session.
#[test]
fn a_restored_tab_whose_start_fails_stays_in_the_saved_tabs_until_it_is_closed() {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let from = set.active();
    let mut planned_codex = planned(AgentId::Codex, "codex-b", SessionModeChoice::Bypass);
    planned_codex.name = Some("parser work".into());
    let plan = RestorePlan {
        items: vec![planned_codex],
        skipped: Vec::new(),
        active_session: Some("codex-b".into()),
    };
    let RestoreStep::Confirm(prompt) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("a saved bypass tab is asked about first")
    };
    let go = set.answer_restore(prompt.nonce, true).unwrap();
    set.start_restore(from, go, |_, _| Ok(RestoreConnect::AwaitTrust));
    let codex = set
        .tab_with_session(&AgentId::Codex, "codex-b")
        .expect("the Codex tab came back");

    // Its own agent's trust answer starts it as a resume by hand, and the start is refused.
    let (tx, result_rx) = mpsc::channel();
    set.get_mut(codex).unwrap().backend = TabBackend::Starting(PendingStart {
        request_id: "restore-1".into(),
        result_rx,
        first_turn: None,
        resume: Some("codex-b".into()),
        resumed_title: None,
        resumed_name: Some("parser work".into()),
    });
    tx.send(Err(BackendError {
        message: "Codex could not start: CODEX_HOME is relative".into(),
        benign: false,
    }))
    .unwrap();
    let collected = set.collect_starts();
    assert!(matches!(collected.as_slice(), [StartCollected::Failed { tab, .. }] if *tab == codex));
    assert_eq!(set.get(codex).unwrap().wire_state(), TabStateWire::Failed);

    let saved = set.saved_snapshot("conv").unwrap();
    assert_eq!(others(&saved).len(), 1, "the conversation is still in the file");
    assert_eq!(others(&saved)[0].agent, AgentId::Codex);
    assert_eq!(others(&saved)[0].provider_session_id, "codex-b");
    assert_eq!(others(&saved)[0].name.as_deref(), Some("parser work"));
    assert_eq!(others(&saved)[0].mode, SessionModeChoice::Bypass);
    assert_eq!(others_active(&saved), Some(0));

    // The same session opened in another tab is that tab's entry, not listed twice.
    let other = set.open(AgentId::Codex);
    set.get_mut(other).unwrap().backend = TabBackend::AwaitingTrust(AwaitingTrust {
        start: DeferredStart::Resume {
            request_id: "r".into(),
            provider_session_id: "codex-b".into(),
            resumed_title: None,
            resumed_name: None,
        },
        asking: None,
        recording: false,
    });
    assert_eq!(others(&set.saved_snapshot("conv").unwrap()).len(), 1);
    set.remove(other).unwrap();

    // Starting it over is the user's choice of a fresh session; closing it is the user's choice to let go.
    let mut again = set.saved_snapshot("conv").unwrap();
    assert_eq!(others(&again).len(), 1);
    set.reset(codex).unwrap();
    again = set.saved_snapshot("conv").unwrap();
    assert!(others(&again).is_empty(), "a tab started over holds no session");

    set.get_mut(codex).unwrap().backend = TabBackend::Failed { reason: "x".into() };
    set.get_mut(codex).unwrap().failed_resume = Some("codex-b".into());
    assert_eq!(others(&set.saved_snapshot("conv").unwrap()).len(), 1);
    // A second failed tab of the same session (its resume by hand refused too) is not a second entry.
    let twin = set.open(AgentId::Codex);
    set.get_mut(twin).unwrap().backend = TabBackend::Failed { reason: "y".into() };
    set.get_mut(twin).unwrap().failed_resume = Some("codex-b".into());
    assert_eq!(others(&set.saved_snapshot("conv").unwrap()).len(), 1);
    set.remove(twin).unwrap();
    set.remove(codex).unwrap();
    assert!(others(&set.saved_snapshot("conv").unwrap()).is_empty());
}

/// An ACP tab is saved, and comes back, exactly when its agent said it can load a saved session.
#[test]
fn an_acp_tab_is_saved_only_when_its_agent_loads_sessions() {
    let dir = workspace("acp-saved");
    for (resumes, saved_count) in [(true, 1), (false, 0)] {
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let agent = AgentId::Acp("gem".into());
        let tab = set.open(agent.clone());
        assert!(
            !set.get(tab).unwrap().resumable(BackendKind::Sidecar),
            "nothing said yet"
        );
        let provider = Arc::new(ExternalRecordingProvider::named("acp:gem", resumes));
        let backend = AgentBackend::start_external_with(
            agent.clone(),
            provider.clone(),
            &dir,
            empty_trust_plan(),
            SessionModeChoice::Auto,
            None,
        )
        .unwrap();
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        until("the session is named", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().provider_session_id().is_some()
        });
        assert_eq!(set.get(tab).unwrap().resumable(BackendKind::Sidecar), resumes);
        set.select(tab);
        let saved = set.saved_snapshot("conv").unwrap();
        assert_eq!(others(&saved).len(), saved_count, "loadSession {resumes}");
        if resumes {
            assert_eq!(others(&saved)[0].agent, agent);
            assert_eq!(others(&saved)[0].provider_session_id, "thread-1");
            assert_eq!(others_active(&saved), Some(0));
        } else {
            assert_eq!(others_active(&saved), None);
        }
        drop(provider);
        for t in set.take_all() {
            if let Some(retired) = t.retired {
                retired.into_backend().shutdown();
            }
        }
    }
    // A tab brought back holding a saved session waits as resumable, so it stays in the file.
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let from = set.active();
    let plan = RestorePlan {
        items: vec![planned(AgentId::Acp("gem".into()), "gem-s", SessionModeChoice::Auto)],
        skipped: Vec::new(),
        active_session: None,
    };
    let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("nothing in bypass, nothing to ask")
    };
    assert_eq!(go.trust_agent(), Some(AgentId::Acp("gem".into())));
    set.start_restore(from, go, |_, _| Ok(RestoreConnect::AwaitTrust));
    let waiting = set
        .tab_with_session(&AgentId::Acp("gem".into()), "gem-s")
        .expect("the ACP tab came back");
    assert!(set.get(waiting).unwrap().resumable(BackendKind::Sidecar));
    let saved = set.saved_snapshot("conv").unwrap();
    assert_eq!(others(&saved).len(), 1, "kept in the saved file while it waits");
    assert_eq!(others(&saved)[0].provider_session_id, "gem-s");
}

#[test]
fn a_removed_live_tab_keeps_presence_until_retire_done() {
    let dir = workspace("external-retiring-count");
    let (mut set, tab, _provider) = codex_tab(&dir, SessionModeChoice::Auto);
    let review = dir.join(".review-state");
    set.hold_presence(crate::turn_review::PresenceHolder::start(review, std::process::id()));
    let guard = set.presence_guard().unwrap();
    until("the presence is held", || {
        !matches!(guard.check(), Err(crate::turn_review::Blocked::NotHeld(_)))
    });
    assert_eq!(guard.check(), Ok(()));
    let mut removed = set.remove(tab).unwrap();
    assert_eq!(
        guard.check(),
        Err(crate::turn_review::Blocked::TurnRunningHere),
        "the backend is still being shut down"
    );
    if let Some(retired) = removed.retired.take() {
        retired.into_backend().shutdown();
    }
    set.retire_done(tab);
    assert_eq!(guard.check(), Ok(()));
    // A second report for the same tab changes nothing.
    set.retire_done(tab);
    assert_eq!(guard.check(), Ok(()));
}

/// The terminal handoff takes the live session away like every other way: until its shutdown has
/// returned, the window's presence stays as if a turn ran, so no revert writes meanwhile.
#[test]
fn a_handed_off_session_counts_as_retiring_until_its_shutdown_returns() {
    let dir = workspace("external-handoff-retiring");
    let (mut set, tab, _provider) = codex_tab(&dir, SessionModeChoice::Auto);
    let review = dir.join(".review-state");
    set.hold_presence(crate::turn_review::PresenceHolder::start(review, std::process::id()));
    let guard = set.presence_guard().unwrap();
    until("the presence is held", || {
        !matches!(guard.check(), Err(crate::turn_review::Blocked::NotHeld(_)))
    });
    assert_eq!(guard.check(), Ok(()));
    let mut backend = set
        .take_for_handoff(tab)
        .expect("the live session is taken")
        .into_backend();
    assert!(matches!(set.get(tab).unwrap().backend, TabBackend::NotStarted));
    assert_eq!(
        guard.check(),
        Err(crate::turn_review::Blocked::TurnRunningHere),
        "the handed-off session is still being shut down"
    );
    assert!(set.take_for_handoff(tab).is_none(), "nothing live is left to take");
    backend.shutdown();
    set.retire_done(tab);
    assert_eq!(guard.check(), Ok(()));
}

/// A session handed to a terminal leaves its tab the way every other one does: as a counted
/// `Retired` naming the tab, whose presence stays until its shutdown is reported, not merely until
/// the shutdown returns.
#[test]
fn the_handoff_rule_is_the_retiring_rule() {
    let dir = workspace("external-handoff-retired");
    let (mut set, tab, _provider) = codex_tab(&dir, SessionModeChoice::Auto);
    let review = dir.join(".review-state");
    set.hold_presence(crate::turn_review::PresenceHolder::start(review, std::process::id()));
    let guard = set.presence_guard().unwrap();
    until("the presence is held", || {
        !matches!(guard.check(), Err(crate::turn_review::Blocked::NotHeld(_)))
    });
    assert_eq!(guard.check(), Ok(()));
    let retired = set.take_for_handoff(tab).expect("the live session is taken");
    assert_eq!(
        retired.tab(),
        tab,
        "the retired backend names the tab it was counted for"
    );
    assert_eq!(
        guard.check(),
        Err(crate::turn_review::Blocked::TurnRunningHere),
        "the handed-off session is still being shut down"
    );
    let mut backend = retired.into_backend();
    backend.shutdown();
    drop(backend);
    assert_eq!(
        guard.check(),
        Err(crate::turn_review::Blocked::TurnRunningHere),
        "the presence ends when the shutdown is reported, as for every retired backend"
    );
    set.retire_done(tab);
    assert_eq!(guard.check(), Ok(()));
}
