//! Saving the tabs, and bringing them back.
#![cfg(test)]

use super::*;

/// After `/clear` the tab is saved under the new conversation: in the moment before its session
/// opens (the session holds no id then) and after, so a restore resumes what the tab shows.
#[test]
fn a_cleared_tab_is_saved_under_the_new_conversation() {
    let dir = workspace("clear-saved-id");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let opened = |id: &str| AgentDomainEvent::SessionOpened {
        session_id: "verdandi-1".into(),
        provider_session_id: id.into(),
        model: "m".into(),
        cwd: dir.to_string_lossy().into_owned(),
    };
    let saved = |set: &TabSet| {
        ids(&set.saved_snapshot(PROJECT).unwrap())
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    provider.queue(opened("sess-old"));
    until("the old session", || {
        set.pump(&dir, true);
        saved(&set) == ["sess-old"]
    });
    provider.queue_all(vec![
        AgentDomainEvent::TurnStarted { turn_id: "t2".into() },
        AgentDomainEvent::ConversationReset {
            new_conversation_id: "sess-new".into(),
            trigger: Some("clear".into()),
        },
    ]);
    until("the reset", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().provider_session_id().is_none()
    });
    assert_eq!(
        saved(&set),
        ["sess-new"],
        "saved under the new conversation before it opens"
    );
    provider.queue(opened("sess-new"));
    until("the new session", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().provider_session_id().as_deref() == Some("sess-new")
    });
    assert_eq!(saved(&set), ["sess-new"]);
    shut_down_all(&mut set);
}

#[test]
fn only_tabs_with_a_session_are_listed_in_tab_bar_order_with_their_name_and_mode() {
    let dir = workspace("tabs-snapshot");
    let mut set = set();
    assert_eq!(
        set.saved_snapshot(PROJECT),
        Some(SavedTabs::default()),
        "a fresh window has nothing to save"
    );
    let first = set.active();
    give_session(&mut set, first, &dir, "s-one");
    set.rename(first, "api");
    let empty = set.open(AgentId::Claude);
    let third = set.open(AgentId::Claude);
    give_session(&mut set, third, &dir, "s-three");
    set.get_mut(third).unwrap().mode = SessionModeChoice::Bypass;

    let saved = set.saved_snapshot(PROJECT).unwrap();
    assert_eq!(ids(&saved), ["s-one", "s-three"], "the empty tab is not listed");
    assert_eq!(saved.tabs[0].name.as_deref(), Some("api"));
    assert_eq!(saved.tabs[0].mode, SessionModeChoice::Auto);
    assert_eq!(saved.tabs[1].mode, SessionModeChoice::Bypass);
    assert!(saved.tabs.iter().all(|t| t.conversation_id == PROJECT));
    assert_eq!(saved.active, Some(1), "the tab on screen, among the listed ones");

    set.select(empty);
    assert_eq!(
        set.saved_snapshot(PROJECT).unwrap().active,
        Some(1),
        "an empty tab on screen falls back to the last listed one"
    );
    shut_down_all(&mut set);
}

#[test]
fn the_saved_active_tab_falls_back_to_the_listed_tab_that_was_active_most_recently() {
    let dir = workspace("tabs-snapshot-recent");
    let mut set = set();
    let a = set.active();
    give_session(&mut set, a, &dir, "s-a");
    let b = set.open(AgentId::Claude);
    give_session(&mut set, b, &dir, "s-b");
    let c = set.open(AgentId::Claude); // empty
    let d = set.open(AgentId::Claude); // empty
                                       // a, b, c, d in that order; now b, then d: the empty d is on screen.
    set.select(b);
    set.select(d);
    assert_eq!(
        set.saved_snapshot(PROJECT).unwrap().active,
        Some(1),
        "b was the last listed tab on screen"
    );
    set.select(a);
    set.select(c);
    assert_eq!(set.saved_snapshot(PROJECT).unwrap().active, Some(0), "now it was a");
    // Closing the tab it fell back to hands the fallback to the one before it.
    set.remove(a);
    assert_eq!(set.saved_snapshot(PROJECT).unwrap().active, Some(0));
    assert_eq!(ids(&set.saved_snapshot(PROJECT).unwrap()), ["s-b"]);
    shut_down_all(&mut set);
}

#[test]
fn a_tab_still_resuming_is_listed_and_a_failed_one_is_not() {
    let mut set = set();
    let resuming = set.active();
    let (_tx, result_rx) = mpsc::channel();
    set.get_mut(resuming).unwrap().backend = TabBackend::Starting(PendingStart {
        request_id: "r".into(),
        result_rx,
        first_turn: None,
        resume: Some("s-coming".into()),
        resumed_title: None,
        resumed_name: None,
    });
    let failed = set.open(AgentId::Claude);
    set.get_mut(failed).unwrap().backend = TabBackend::Failed { reason: "no".into() };
    assert_eq!(ids(&set.saved_snapshot(PROJECT).unwrap()), ["s-coming"]);
}

#[test]
fn the_legacy_backend_resumes_nothing_so_it_lists_nothing() {
    let dir = workspace("tabs-snapshot-legacy");
    let mut legacy = TabSet::new(BackendKind::Legacy, SessionModeChoice::Auto);
    let tab = legacy.active();
    give_session(&mut legacy, tab, &dir, "s-legacy");
    assert_eq!(legacy.saved_snapshot(PROJECT), Some(SavedTabs::default()));
    shut_down_all(&mut legacy);
}

/// Closing the window takes every tab away to tear it down; that is not the user closing them.
#[test]
fn a_window_that_has_taken_its_tabs_to_close_them_has_no_snapshot() {
    let dir = workspace("tabs-snapshot-closing");
    let mut set = set();
    let tab = set.active();
    give_session(&mut set, tab, &dir, "s-one");
    assert!(set.saved_snapshot(PROJECT).is_some());
    let taken = set.take_all();
    assert_eq!(set.saved_snapshot(PROJECT), None);
    for tab in taken {
        if let Some(retired) = tab.retired {
            retired.into_backend().shutdown();
        }
    }
}

#[test]
fn a_pristine_window_has_no_tab_with_a_session_in_any_state() {
    let mut set = set();
    assert!(set.is_pristine());
    set.open(AgentId::Claude);
    assert!(set.is_pristine(), "empty tabs do not count");
    let tab = set.active();
    set.get_mut(tab).unwrap().backend = TabBackend::Failed { reason: "x".into() };
    assert!(!set.is_pristine(), "a start that was attempted does");
    let (_tx, result_rx) = mpsc::channel();
    set.get_mut(tab).unwrap().backend = TabBackend::Starting(PendingStart {
        request_id: "r".into(),
        result_rx,
        first_turn: None,
        resume: None,
        resumed_title: None,
        resumed_name: None,
    });
    assert!(!set.is_pristine());
}

#[test]
fn the_configured_default_mode_is_the_one_way_a_window_starts_in_bypass() {
    let mut set = set();
    set.apply_configured_default(SessionModeChoice::Bypass);
    assert_eq!(set.default_mode(), SessionModeChoice::Bypass);
    assert_eq!(
        set.active_tab().mode(),
        SessionModeChoice::Bypass,
        "the empty tab it already has"
    );
    let next = set.open(AgentId::Claude);
    assert_eq!(
        set.get(next).unwrap().mode(),
        SessionModeChoice::Bypass,
        "and every new one"
    );
    // Leaving bypass is as it always was: at once, and an empty tab's default goes with it (to
    // manual, where leaving bypass lands).
    let tab = set.active();
    assert_eq!(
        set.cycle_mode(tab, ViewId::BASE),
        Ok(ModeCycle::Changed(SessionModeChoice::Manual)),
    );
    assert_eq!(set.default_mode(), SessionModeChoice::Manual);
}
