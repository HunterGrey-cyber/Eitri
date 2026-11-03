//! Tab numbering, the default mode, what the pump answers without a human, and the cases a tab set needs no pumping for.
#![cfg(test)]

use super::*;

/// The start owes this line before its backend exists, and a page may not have shown it by the time the
/// backend lands. What an earlier session owed or said ends with that session.
#[test]
fn a_line_the_start_owes_outlives_the_landing() {
    let landed = |note| {
        let session = SessionState {
            foreign_server_note: note,
            ..SessionState::default()
        };
        session.for_installed_backend().foreign_server_note
    };
    assert_eq!(
        landed(ForeignServerNote::NameTakenOwed(NAME_UNKNOWN_LINE)),
        ForeignServerNote::NameTakenOwed(NAME_UNKNOWN_LINE)
    );
    assert_eq!(
        landed(ForeignServerNote::NameTakenSaid(NAME_TAKEN_LINE)),
        ForeignServerNote::Unseen
    );
    assert_eq!(landed(ForeignServerNote::Owed), ForeignServerNote::Unseen);
    assert_eq!(landed(ForeignServerNote::Said), ForeignServerNote::Unseen);
}

#[test]
fn a_new_tab_takes_the_default() {
    let mut set = set();
    let first = set.active();
    assert!(!set.get(first).unwrap().following);
    set.set_following_default(true);
    let second = set.open(AgentId::Claude);
    assert!(set.get(second).unwrap().following);
    assert!(!set.get(first).unwrap().following);
    set.set_following_default(false);
    let third = set.open(AgentId::Codex);
    assert!(!set.get(third).unwrap().following);
    assert!(set.get(second).unwrap().following);
}

#[test]
fn the_toggle_is_per_tab() {
    let mut set = set();
    let first = set.active();
    let second = set.open(AgentId::Claude);
    assert!(set.toggle_following(first));
    assert!(set.get(first).unwrap().following);
    assert!(!set.get(second).unwrap().following);
    set.get_mut(first)
        .unwrap()
        .session
        .client_calls
        .edited
        .push(PathBuf::from("a.rs"));
    assert!(set.toggle_following(first));
    assert!(!set.get(first).unwrap().following);
    assert!(set.take_follow_edits().is_empty());
    assert!(!set.toggle_following(TabId(u64::MAX)));
    set.freeze_for_move(first).unwrap();
    assert!(!set.toggle_following(first));
    assert!(!set.get(first).unwrap().following);
}

/// An idle set needs no pumping; a live backend needs it only until it announces its folds, and then
/// each fold calls the wake, after which the pump finds it. A connect running on a worker needs pumping.
#[test]
fn a_set_needs_pumping_only_for_what_cannot_announce_itself() {
    let dir = workspace("tabs-needs-polling");
    let mut set = set();
    assert!(!set.needs_polling(), "an empty tab");
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    assert!(set.needs_polling(), "a live backend nobody is told about");

    let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let wakes = wakes.clone();
        set.set_ui_wake(Arc::new(move || {
            wakes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
    }
    assert!(!set.needs_polling(), "the backend announces its folds now");
    let installed = wakes.load(std::sync::atomic::Ordering::SeqCst);
    assert!(installed >= 1, "installing the wake takes in what was folded before");

    provider.queue(started("t1"));
    until("the wake after the fold", || {
        wakes.load(std::sync::atomic::Ordering::SeqCst) > installed
    });
    let out = set.pump(&dir, true);
    assert!(out.active_payload.is_some(), "the woken pump finds the fold");
    assert!(!set.needs_polling());

    let other = set.open(AgentId::Claude);
    let (_tx, result_rx) = mpsc::channel();
    set.get_mut(other).unwrap().backend = TabBackend::Starting(PendingStart {
        request_id: "req-1".into(),
        result_rx,
        first_turn: None,
        resume: None,
        resumed_title: None,
        resumed_name: None,
    });
    assert!(set.needs_polling(), "a connect on a worker reports through a channel");
    shut_down_all(&mut set);
}

/// A terminal handoff's close runs on a worker that reports through channels nobody is told about: the
/// set needs pumping until the handoff is collected, and not after.
#[test]
fn a_handoff_in_flight_needs_pumping() {
    let mut set = set();
    assert!(!set.needs_polling());
    let tab = set.active();
    let (_closed_tx, closed_rx) = mpsc::channel();
    let (_dropped_tx, dropped_rx) = mpsc::channel();
    set.get_mut(tab).unwrap().pending_handoff = Some(PendingHandoff {
        request_id: "req-handoff".into(),
        command: agent::handoff::ClaudeResumeCommand::for_session("/home/user/project", "1857dcd5-973b-46a2").unwrap(),
        closed_rx,
        dropped_rx,
    });
    assert!(set.needs_polling(), "a handoff's close is still running");
    set.get_mut(tab).unwrap().pending_handoff = None;
    assert!(!set.needs_polling(), "collected");
}

/// A tab resumed with its record's title takes the CLI's title once it is known; a tab titled by its
/// own first prompt keeps it.
#[test]
fn a_title_read_later_replaces_a_resumed_tabs_record_title() {
    let dir = workspace("tabs-retitle");
    let mut set = set();
    let resumed = set.active();
    let (provider, backend) = live(&dir);
    let (tx, result_rx) = mpsc::channel();
    set.get_mut(resumed).unwrap().backend = TabBackend::Starting(PendingStart {
        request_id: "req-resume".into(),
        result_rx,
        first_turn: None,
        resume: Some("s-resumed".into()),
        resumed_title: Some("first prompt".into()),
        resumed_name: None,
    });
    tx.send(Ok(backend)).unwrap();
    assert_eq!(set.collect_starts().len(), 1);
    provider.open_session("s-resumed", &dir);
    until("the session id", || {
        set.get(resumed)
            .unwrap()
            .live()
            .and_then(AgentBackend::provider_session_id)
            .is_some()
    });
    let fresh = set.open(AgentId::Claude);
    let (other_provider, other) = live(&dir);
    set.get_mut(fresh).unwrap().backend = TabBackend::Live(other);
    set.get_mut(fresh).unwrap().session.title = Some("its own prompt".into());
    other_provider.open_session("s-fresh", &dir);
    until("the other session id", || {
        set.get(fresh)
            .unwrap()
            .live()
            .and_then(AgentBackend::provider_session_id)
            .is_some()
    });

    let titles = |id: &str| Some(format!("CLI title of {id}"));
    assert!(!set.retitle_from(|_| None), "nothing known yet");
    assert!(set.retitle_from(titles));
    assert_eq!(
        set.get(resumed).unwrap().session.title.as_deref(),
        Some("CLI title of s-resumed")
    );
    assert_eq!(set.get(fresh).unwrap().session.title.as_deref(), Some("its own prompt"));
    assert!(!set.retitle_from(titles), "the same title again changes nothing");
    assert!(
        !set.retitle_from(|_| Some("retitled again".into())),
        "a later retitle is not followed, as the read at the resume was not"
    );
    assert_eq!(
        set.get(resumed).unwrap().session.title.as_deref(),
        Some("CLI title of s-resumed")
    );
    assert!(!set.get(resumed).unwrap().session.title_from_record);
    // A record whose title already was the CLI's (the cache was warm at the resume): kept, and done.
    set.get_mut(resumed).unwrap().session.title_from_record = true;
    assert!(!set.retitle_from(|_| Some("CLI title of s-resumed".into())));
    assert!(!set.get(resumed).unwrap().session.title_from_record);
    shut_down_all(&mut set);
}

#[test]
fn a_new_window_has_one_empty_tab_numbered_one() {
    let set = set();
    assert_eq!(set.tabs().len(), 1);
    let tab = set.active_tab();
    assert_eq!(tab.number, 1);
    assert!(matches!(tab.backend, TabBackend::NotStarted));
    assert_eq!(tab.label_name(), "new");
    assert_eq!(tab.mode, SessionModeChoice::Auto);
}

#[test]
fn tabs_open_at_the_lowest_free_number_and_the_last_close_opens_a_fresh_tab_one() {
    let mut set = set();
    let one = set.active();
    let two = set.open(AgentId::Claude);
    let three = set.open(AgentId::Claude);
    assert_eq!(set.active(), three, "tmux's new-window selects the new window");
    assert_eq!(set.get(three).unwrap().number, 3);
    set.remove(two).unwrap();
    let again = set.open(AgentId::Claude);
    assert_eq!(set.get(again).unwrap().number, 2, "the lowest free number, reused");
    assert_ne!(again, two, "the id is never reused");
    for id in [one, again, three] {
        set.remove(id).unwrap();
    }
    assert_eq!(set.tabs().len(), 1, "the panel is never tab-less");
    assert_eq!(set.active_tab().number, 1);
}

/// The fresh tab the last close opens runs the configured `agent.default`, as every other tab made
/// without naming an agent does.
#[test]
fn the_last_close_opens_a_tab_of_the_configured_agent() {
    let mut set = set();
    set.apply_configured_agent(AgentId::Codex);
    let one = set.active();
    assert_eq!(set.active_tab().agent(), &AgentId::Codex);
    set.remove(one).unwrap();
    assert_eq!(set.tabs().len(), 1);
    assert_eq!(
        set.active_tab().agent(),
        &AgentId::Codex,
        "the fresh tab runs the default agent"
    );
}

#[test]
fn tab_has_last_only_when_the_previous_tab_still_belongs_to_the_base_view() {
    let mut set = set();
    assert!(!set.has_last());
    assert_eq!(set.select_last(), None);

    let one = set.active();
    let two = set.open(AgentId::Claude);
    let active = set.active();
    assert!(set.has_last());
    assert_eq!(set.active(), active);
    assert_eq!(set.last_active, Some(one));
    assert_eq!(set.select_last(), Some(one));

    assert_eq!(set.break_out(view(2)), Ok(one));
    set.last_active = Some(one);
    assert!(!set.has_last());
    assert_eq!(set.select_last(), None);

    set.remove(one).unwrap();
    set.last_active = Some(one);
    assert!(!set.has_last());
    assert_eq!(set.select_last(), None);
    assert_eq!(set.active(), two);
}

#[test]
fn next_and_previous_wrap_last_goes_back_and_a_missing_number_selects_nothing() {
    let mut set = set();
    let one = set.active();
    let two = set.open(AgentId::Claude);
    let three = set.open(AgentId::Claude);
    assert_eq!(set.step(1), Some(one), "wraps from the last");
    assert_eq!(set.step(-1), Some(three));
    assert!(set.select(two));
    assert_eq!(set.select_last(), Some(three), "tmux last-window");
    assert_eq!(set.select_last(), Some(two));
    assert_eq!(set.select_number(1), Some(TabPlace::Base(one)));
    assert_eq!(set.select_number(9), None, "no tab 9: the caller flashes");
    assert_eq!(set.active(), one);
}

#[test]
fn a_command_without_a_tab_or_naming_an_unknown_one_is_refused() {
    let set = set();
    let active = set.active();
    assert_eq!(set.resolve(TabRef::Named(active)), Ok(Some(active)));
    assert_eq!(set.resolve(TabRef::WindowLevel), Ok(None));
    assert_eq!(
        set.resolve(TabRef::Missing),
        Err("protocol: this command names no tab".to_string()),
        "never the active one"
    );
    assert_eq!(
        set.resolve(TabRef::Named(TabId(99))),
        Err("protocol: no tab 99".to_string())
    );
}

/// Pumping every tab through `take_revised_ui_delivery` answers a background tab's Read
/// without dispatching that tab's events to the page.
#[test]
fn the_pump_answers_what_needs_no_human_in_a_background_tab() {
    let dir = workspace("tabs-background-pump");
    let mut set = set();
    let background = set.active();
    let (background_provider, backend) = live(&dir);
    set.get_mut(background).unwrap().backend = TabBackend::Live(backend);
    let foreground = set.open(AgentId::Claude);
    let (_foreground_provider, backend) = live(&dir);
    set.get_mut(foreground).unwrap().backend = TabBackend::Live(backend);

    background_provider.queue(read("perm-read"));
    background_provider.queue(write("perm-write"));
    until("the background tab's card", || {
        let out = set.pump(&dir, true);
        assert!(
            out.active_payload.is_none(),
            "nothing of a background tab is dispatched"
        );
        set.get(background).unwrap().attention.attention().pending == 1
    });
    assert_eq!(background_provider.resolutions(), vec![("perm-read".to_string(), true)]);
    assert!(set.get(background).unwrap().session.stale);
    assert_eq!(set.attention().pending, 1, "the tray sums every tab");
    let tabs: serde_json::Value = serde_json::from_str(&set.tabs_payload()).unwrap();
    assert_eq!(tabs["active"], foreground.0);
    assert_eq!(tabs["tabs"][0]["marker"], "needs_input");

    // The switch: the new tab's snapshot, and it is no longer stale.
    assert!(set.select(background));
    let payloads = set.active_state_payloads();
    let snapshot: serde_json::Value = serde_json::from_str(&payloads[0]).unwrap();
    assert_eq!(snapshot["kind"], "snapshot");
    assert_eq!(snapshot["tab"], background.0);
    // The projection retains both requests until the Read's resolution returns, but
    // `host_answered` hides the already answered Read from snapshots and attention.
    // Only the Write still waiting for a human is shown.
    assert_eq!(
        snapshot["state"]["pendingPermissions"].as_array().unwrap().len(),
        1,
        "the auto-answered Read is hidden; only the Write is a real card"
    );
    assert_eq!(snapshot["state"]["pendingPermissions"][0]["permissionId"], "perm-write");
    assert!(!set.get(background).unwrap().session.stale);
    shut_down_all(&mut set);
}
