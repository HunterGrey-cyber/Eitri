//! A backend starting for a tab: late, resumed, handed off or refused, and the mode a tab can still be switched to.
#![cfg(test)]

use super::*;

/// A connect finishing after its tab closes is never installed elsewhere; the closed tab's
/// receiver returns the backend for shutdown.
#[test]
fn a_late_start_for_a_closed_tab_is_returned_for_shutdown() {
    let dir = workspace("tabs-late-start");
    let mut set = set();
    let keep = set.active();
    let doomed = set.open(AgentId::Claude);
    let (tx, result_rx) = mpsc::channel();
    set.get_mut(doomed).unwrap().backend = TabBackend::Starting(PendingStart {
        request_id: "req-1".into(),
        result_rx,
        first_turn: None,
        resume: None,
        resumed_title: None,
        resumed_name: None,
    });
    assert_eq!(set.running_count(), 1, "a connect counts as running");
    let closed = set.remove(doomed).unwrap();
    let (_provider, backend) = live(&dir);
    tx.send(Ok(backend)).unwrap();
    assert!(set.collect_starts().is_empty(), "no tab in the set was waiting for it");
    assert!(set.tabs().iter().all(|t| t.live().is_none()));
    assert_eq!(set.active(), keep);
    let TabBackend::Starting(pending) = closed.tab.backend else {
        panic!("the closed tab kept its pending start")
    };
    let mut orphan = pending.result_rx.try_recv().unwrap().map_err(|e| e.message).unwrap();
    orphan.shutdown();
}

/// A resumed record's rename wins over its title and becomes the tab's own `name`, also used
/// in the `prefix ,` detail popover. A rename made while connecting is newer and wins over both.
#[test]
fn a_resume_installs_the_records_rename_as_the_tabs_name_unless_renamed_meanwhile() {
    let dir = workspace("tabs-resume-name");
    let mut set = set();
    let first = set.active();
    let tx = starting(&mut set, first, Some("fix the parser"), Some("parser work"));
    tx.send(Ok(live(&dir).1)).unwrap();
    set.collect_starts();
    let tab = set.get(first).unwrap();
    assert_eq!(tab.name.as_deref(), Some("parser work"));
    assert_eq!(tab.session.title.as_deref(), Some("fix the parser"));
    assert_eq!(tab.label_name(), "parser work");

    let second = set.open(AgentId::Claude);
    let tx = starting(&mut set, second, Some("fix the parser"), Some("parser work"));
    set.rename(second, "mine");
    tx.send(Ok(live(&dir).1)).unwrap();
    set.collect_starts();
    assert_eq!(set.get(second).unwrap().name.as_deref(), Some("mine"));
    shut_down_all(&mut set);
}

/// `react` acts only when `arrived` grows. The window total must not decrease on session install,
/// reset or tab close, or a simultaneous card in another tab could fail to raise it.
#[test]
fn the_windows_arrived_count_never_goes_down() {
    let dir = workspace("tabs-arrived");
    let mut set = set();
    let first = set.active();
    set.get_mut(first).unwrap().attention.observe(&[write("p1")], false);
    assert_eq!(set.attention().arrived, 1);

    let tx = starting(&mut set, first, None, None);
    tx.send(Ok(live(&dir).1)).unwrap();
    set.collect_starts();
    assert_eq!(set.attention().pending, 0, "a new session holds no card");
    assert_eq!(set.attention().arrived, 1, "installing a session kept the count");

    let second = set.open(AgentId::Claude);
    set.get_mut(second).unwrap().attention.observe(&[write("p2")], false);
    set.get_mut(second).unwrap().backend = TabBackend::Failed { reason: "x".into() };
    set.reset(second).unwrap();
    assert_eq!(set.attention().arrived, 2, "a reset kept the count");
    set.remove(second).unwrap();
    assert_eq!(set.attention().arrived, 2, "a close kept the count");
    shut_down_all(&mut set);
}

#[test]
fn a_new_session_starts_with_no_per_session_state() {
    let dir = workspace("tabs-fresh-session");
    let mut set = set();
    let id = set.active();
    let (_, backend) = live(&dir);
    let tab = set.get_mut(id).unwrap();
    tab.backend = TabBackend::Live(backend);
    tab.session.title = Some("old title".into());
    tab.session.title_from_record = true;
    tab.attention.observe(&[write("old-card")], false);
    tab.session.turn_trace = Some(crate::turn_trace::TurnTrace::started_now());
    tab.session.reported_start_failure = true;
    tab.session
        .rule_offers
        .insert("old-card".into(), agent::PrefixRule::parse("Bash(ls *)").unwrap());
    tab.session.rule_offers_seen.push("old-card".into());
    tab.session.was_running = true;
    let call = CandidateCall {
        tool_name: "Write".into(),
        input: serde_json::json!({"file_path": "old.rs"}),
    };
    tab.session.note_ledger.record(
        "old-call".into(),
        NoteCandidate::Rule {
            rule: "Bash(ls *)".into(),
            call: call.clone(),
        },
    );
    tab.session
        .note_ledger
        .record("old-call".into(), NoteCandidate::AutoEdit(call));
    tab.session.creates_file_unsent.insert("old-call".into());
    tab.session.turn_clock = Some(("old-turn".into(), 1));
    tab.session.turn_times_unsent = 1;
    tab.session.host_answered.insert("old-card".into());
    tab.session
        .human_allowed
        .record("old-call", "Write", &serde_json::json!({}));
    tab.session
        .note_ledger
        .record("old-call".into(), NoteCandidate::PromptNote("old note".into()));
    tab.session.user_answered.insert("old-card".into());
    let TabBackend::Live(mut backend) = std::mem::replace(&mut tab.backend, TabBackend::NotStarted) else {
        unreachable!()
    };
    backend.shutdown();
    let tx = starting(&mut set, id, None, None);
    tx.send(Ok(live(&dir).1)).unwrap();
    assert!(matches!(
        set.collect_starts().as_slice(),
        [StartCollected::Installed { .. }]
    ));
    let tab = set.get(id).unwrap();
    assert!(tab.session.human_allowed.is_empty());
    assert!(tab.session.title.is_none());
    assert!(!tab.session.title_from_record);
    assert_eq!(tab.attention.attention().pending, 0);
    assert!(!tab.attention.attention().unread);
    assert!(tab.session.turn_trace.is_none());
    assert!(!tab.session.reported_start_failure);
    assert!(tab.session.rule_offers.is_empty());
    assert!(tab.session.rule_offers_seen.is_empty());
    assert!(!tab.session.was_running);
    assert!(tab.session.note_ledger.is_empty());
    assert!(tab.session.creates_file_unsent.is_empty());
    assert!(tab.session.turn_clock.is_none());
    assert_eq!(tab.session.turn_times_unsent, 0);
    assert!(tab.session.host_answered.is_empty());
    assert!(tab.session.user_answered.is_empty());
    shut_down_all(&mut set);
}

/// The tiers are named as the start begins, and the user may type and queue while the backend
/// connects; installing the backend must not throw any of that away.
#[test]
fn a_tab_keeps_what_it_gathered_while_its_backend_started() {
    let dir = workspace("tabs-kept-while-starting");
    let mut set = set();
    let id = set.active();
    let tx = starting(&mut set, id, None, None);
    set.queue_message(
        id,
        "queued while starting",
        "queued while starting".into(),
        1,
        true,
        PromptOrigin::Typed,
    )
    .unwrap();
    let tab = set.get_mut(id).unwrap();
    tab.session.project_tiers = Some(agent::setting_sources::ProjectTrust::Untrusted);
    tab.session.stale = true;
    tab.session.queue_error = Some("refused flush".into());
    tab.session.queue_note = Some("held note".into());
    tab.replace_draft("typed while starting".into());
    tab.session.left_view = Some(ViewId::BASE);
    tx.send(Ok(live(&dir).1)).unwrap();
    assert!(matches!(
        set.collect_starts().as_slice(),
        [StartCollected::Installed { .. }]
    ));
    let tab = set.get(id).unwrap();
    assert!(matches!(tab.backend(), TabBackend::Live(_)));
    assert_eq!(
        tab.session.project_tiers,
        Some(agent::setting_sources::ProjectTrust::Untrusted)
    );
    assert_eq!(tab.session.left_view, Some(ViewId::BASE));
    assert!(tab.session.stale);
    assert_eq!(
        tab.session.queue.iter().map(|q| q.text.as_str()).collect::<Vec<_>>(),
        ["queued while starting"]
    );
    assert_eq!(tab.session.queue_error.as_deref(), Some("refused flush"));
    assert_eq!(tab.session.queue_note.as_deref(), Some("held note"));
    assert_eq!(tab.session.draft, "typed while starting");
    shut_down_all(&mut set);
}

#[test]
fn a_handed_off_tab_keeps_its_resume_command_and_title_across_a_pump() {
    let dir = workspace("tabs-handoff-pump");
    let mut set = set();
    let id = set.active();
    let tab = set.get_mut(id).unwrap();
    tab.backend = TabBackend::Live(live(&dir).1);
    tab.session.title = Some("continue this conversation".into());
    let command =
        agent::handoff::ClaudeResumeCommand::for_session(dir.to_str().unwrap(), "1857dcd5-973b-46a2").unwrap();
    let mut backend = set.take_for_handoff(id).unwrap().into_backend();
    backend.shutdown();
    set.retire_done(id);
    set.get_mut(id).unwrap().last_handoff = Some(command);
    let before = set.active_state_payloads();
    for _ in 0..2 {
        set.pump_views(&dir, &|_| true);
    }
    let tab = set.get(id).unwrap();
    assert!(matches!(tab.backend, TabBackend::NotStarted));
    assert_eq!(tab.session.title.as_deref(), Some("continue this conversation"));
    assert_eq!(
        tab.last_handoff.as_ref().unwrap().provider_session_id(),
        "1857dcd5-973b-46a2"
    );
    assert_eq!(set.active_state_payloads(), before);
}

#[test]
fn a_finished_start_installs_into_its_own_tab_with_its_first_turn() {
    let dir = workspace("tabs-install");
    let mut set = set();
    let first = set.active();
    let second = set.open(AgentId::Claude);
    set.rename(first, "docs");
    let (tx, result_rx) = mpsc::channel();
    set.get_mut(first).unwrap().backend = TabBackend::Starting(PendingStart {
        request_id: "req-7".into(),
        result_rx,
        first_turn: Some(FirstTurn {
            typed_by_person: true,
            wire: "ctx\n\nhi".into(),
            typed: "hi".into(),
            shell_line: false,
            attachments: Vec::new(),
        }),
        resume: None,
        resumed_title: None,
        resumed_name: None,
    });
    let (_provider, backend) = live(&dir);
    tx.send(Ok(backend)).unwrap();
    let collected = set.collect_starts();
    assert!(matches!(
        collected.as_slice(),
        [StartCollected::Installed { tab, request_id, first_turn: Some(turn) }]
            if *tab == first && request_id == "req-7" && turn.typed == "hi"
    ));
    assert!(set.get(first).unwrap().live().is_some());
    assert_eq!(set.active(), second, "installing does not switch tabs");

    let (tx, result_rx) = mpsc::channel();
    set.get_mut(second).unwrap().backend = TabBackend::Starting(PendingStart {
        request_id: "req-8".into(),
        result_rx,
        first_turn: None,
        resume: None,
        resumed_title: None,
        resumed_name: None,
    });
    drop(tx);
    let collected = set.collect_starts();
    assert!(matches!(collected.as_slice(), [StartCollected::Failed { tab, .. }] if *tab == second));
    assert!(
        matches!(set.get(second).unwrap().backend, TabBackend::Failed { .. }),
        "failures stay in their tab"
    );
    assert_eq!(set.get(second).unwrap().wire_state(), TabStateWire::Failed);
    assert!(
        set.get(first).unwrap().live().is_some(),
        "and the other tab is untouched"
    );
    shut_down_all(&mut set);
}

#[test]
fn a_resume_of_a_session_open_in_a_tab_switches_to_it_and_one_held_elsewhere_is_refused() {
    let dir = workspace("tabs-resume-route");
    let mut set = set();
    let holder = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(holder).unwrap().backend = TabBackend::Live(backend);
    provider.open_session("claude-open", &dir);
    until("the session id", || {
        set.get(holder).unwrap().provider_session_id().is_some()
    });
    assert_eq!(set.open_sessions(), vec![(AgentId::Claude, "claude-open".to_string())]);
    let other = set.open(AgentId::Claude);

    assert_eq!(
        set.route_resume(other, &AgentId::Claude, "claude-open", true),
        ResumeRoute::SwitchTo(holder)
    );
    assert_eq!(set.active(), holder, "switched, nothing started");
    // Session ids are each agent's own: another agent's session of the same id is not this one.
    assert_eq!(
        set.route_resume(other, &AgentId::Codex, "claude-open", false),
        ResumeRoute::StartIn(other),
        "another agent's session of the same id is resumed, not switched to"
    );
    set.select(holder);
    assert_eq!(
        set.route_resume(holder, &AgentId::Claude, "claude-elsewhere", true),
        ResumeRoute::Refuse("open in another window".to_string())
    );
    assert_eq!(
        set.route_resume(other, &AgentId::Claude, "claude-free", false),
        ResumeRoute::StartIn(other),
        "an empty tab takes it"
    );
    let before = set.tabs().len();
    let ResumeRoute::StartIn(new_tab) = set.route_resume(holder, &AgentId::Claude, "claude-free", false) else {
        panic!("a busy tab resumes into a new one")
    };
    assert_ne!(new_tab, holder);
    assert_eq!(set.tabs().len(), before + 1);
    shut_down_all(&mut set);
}

/// Entering bypass asks for confirmation both before and after a session starts. A live active
/// Auto tab receives `Confirm`, just like an empty tab; leaving bypass is immediate in either state.
#[test]
fn a_rename_is_normalized_and_the_mode_can_still_be_switched_after_the_session_starts() {
    let dir = workspace("tabs-rename-mode");
    let mut set = set();
    let tab = set.active();
    assert!(set.rename(tab, "  docs  "));
    assert_eq!(set.get(tab).unwrap().label_name(), "docs");
    assert!(set.rename(tab, ""));
    assert_eq!(set.get(tab).unwrap().name, None);
    assert!(!set.rename(TabId(99), "x"));

    // Before the session starts (NotStarted, auto), entering bypass asks and confirming also
    // moves the window default, so a fresh tab inherits bypass without another question.
    let first_plan = plan(set.cycle_mode(tab, ViewId::BASE));
    assert_eq!(first_plan.scope, BypassScope::Tab(tab));
    assert_eq!(
        set.confirm_bypass(first_plan.scope, first_plan.nonce, ViewId::BASE),
        Ok(ConfirmOutcome::Entered { approved: 0 })
    );
    assert_eq!(
        set.default_mode(),
        SessionModeChoice::Bypass,
        "new tabs take the remembered mode"
    );
    let next = set.open(AgentId::Claude);
    assert_eq!(set.get(next).unwrap().mode, SessionModeChoice::Bypass);

    let (_provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    // Leaving bypass works even when `next`, rather than `tab`, is on screen.
    // Only entering bypass requires the tab to be active.
    assert_eq!(
        set.cycle_mode(tab, ViewId::BASE),
        Ok(ModeCycle::Changed(SessionModeChoice::Manual)),
        "leaving bypass on a live tab moves at once, to manual"
    );
    // Manual -> auto moves at once too, and needs no tab on screen.
    assert_eq!(
        set.cycle_mode(tab, ViewId::BASE),
        Ok(ModeCycle::Changed(SessionModeChoice::Auto))
    );
    // Entering bypass again asks just as before the session existed, once `tab` is active again.
    set.select(tab);
    let second_plan = plan(set.cycle_mode(tab, ViewId::BASE));
    assert_eq!(second_plan.scope, BypassScope::Tab(tab));
    shut_down_all(&mut set);
}
