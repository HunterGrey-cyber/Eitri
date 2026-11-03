//! Chat views: a tab bound to an extra view.
#![cfg(test)]

use super::*;

#[test]
fn breaking_out_binds_the_active_tab_and_the_base_view_keeps_a_base_tab() {
    let mut set = set();
    let one = set.active();
    let two = set.open(AgentId::Claude);
    let three = set.open(AgentId::Claude);
    assert_eq!(set.active(), three);

    assert_eq!(set.break_out(view(2)), Ok(three));
    assert_eq!(set.view_of(three), view(2));
    assert_eq!(set.get(three).unwrap().view_id(), view(2));
    assert_eq!(set.tab_in(view(2)), Some(three));
    assert_eq!(set.active(), two, "the base view shows its last active tab");
    assert_eq!(set.views(), vec![view(2)]);
    assert_eq!(set.base_tabs(), vec![one, two]);
    assert_eq!(set.shown(), vec![(ViewId::BASE, two), (view(2), three)]);
    assert!(set.is_shown(three) && set.is_shown(two) && !set.is_shown(one));
    assert_eq!(set.place_of(three), Some(TabPlace::View(view(2), three)));
    assert_eq!(set.place_of(one), Some(TabPlace::Base(one)));
    assert_eq!(set.select_last(), None, "the last active tab is cleared by a break");

    assert_eq!(set.break_out(ViewId::BASE), Err(ViewError::NotAnExtraView));
    assert_eq!(set.break_out(view(2)), Err(ViewError::Taken(view(2))));
    assert_eq!(set.break_out(view(3)), Ok(two));
    assert_eq!(set.active(), one, "the neighbour among the base view's tabs");
    assert_eq!(set.break_out(view(4)), Err(ViewError::OneTabLeft));
    assert_eq!(set.active(), one);
    assert_eq!(set.views(), vec![view(2), view(3)]);
    assert_eq!(set.tab_in(ViewId::BASE), None, "the base view's tab is `active`");

    assert_eq!(
        ViewError::NotAnExtraView.to_string(),
        "the base view is not an extra chat view"
    );
    assert_eq!(ViewError::Taken(view(2)).to_string(), "agent 2 already shows a tab");
    assert_eq!(ViewError::OneTabLeft.to_string(), "can't break with only one tab");
}

#[test]
fn joining_back_makes_the_tab_the_base_views_active_one() {
    let mut set = set();
    let one = set.active();
    let two = set.open(AgentId::Claude);
    assert_eq!(set.break_out(view(2)), Ok(two));
    let three = set.open(AgentId::Claude);
    assert_eq!(set.active(), three);

    assert_eq!(set.join_back(view(2)), Some(two));
    assert_eq!(set.active(), two);
    assert_eq!(set.view_of(two), ViewId::BASE);
    assert_eq!(set.tab_in(view(2)), None);
    assert!(set.views().is_empty());
    assert_eq!(set.base_tabs(), vec![one, two, three]);
    assert_eq!(
        set.select_last(),
        Some(three),
        "the tab that was active is the last active one"
    );

    assert_eq!(set.join_back(view(2)), None, "the view shows no tab");
    assert_eq!(set.join_back(ViewId::BASE), None);
    assert_eq!(set.active(), three);
}

#[test]
fn may_answer_compares_views_never_the_active_tab() {
    let mut set = set();
    let one = set.active();
    let two = set.open(AgentId::Claude);
    let three = set.open(AgentId::Claude);
    let four = set.open(AgentId::Claude);
    assert_eq!(set.break_out(view(2)), Ok(four));
    assert_eq!(set.break_out(view(3)), Ok(three));
    assert_eq!(set.active(), two);

    let expected = [
        (one, ViewId::BASE),
        (two, ViewId::BASE),
        (three, view(3)),
        (four, view(2)),
    ];
    for (tab, its_view) in expected {
        assert_eq!(set.view_of(tab), its_view);
        for page in [ViewId::BASE, view(2), view(3), view(4)] {
            assert_eq!(
                set.may_answer(tab, page),
                page == its_view,
                "tab {tab:?} from page {page:?}"
            );
        }
    }
    assert!(
        set.may_answer(one, ViewId::BASE),
        "a base tab in the background is the base page's"
    );
    assert_ne!(set.active(), one);
    for page in [ViewId::BASE, view(2)] {
        assert!(!set.may_answer(TabId(999), page), "no such tab");
    }
    assert_eq!(set.view_of(TabId(999)), ViewId::BASE);
}

#[test]
fn an_answer_from_the_page_a_tab_just_left_is_refused_and_the_card_waits() {
    let dir = workspace("views-answer-from-the-left-page");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let _other = set.open(AgentId::Claude);
    set.select(tab);

    provider.queue(gate_write("perm-hook", "toolu_views"));
    until("the gate's card", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 1
    });
    assert_eq!(set.break_out(view(2)), Ok(tab));

    let number = set.get(tab).unwrap().number;
    let error = set
        .answer_card(tab, ViewId::BASE, "perm-hook", agent::PermissionDecision::Allow)
        .expect_err("the base page no longer shows the tab");
    assert_eq!(error.message, format!("tab {number} is not shown in this page"));
    assert!(!error.benign);
    assert!(provider.resolutions().is_empty(), "nothing reached the agent");
    let t = set.get(tab).unwrap();
    assert!(t
        .live()
        .unwrap()
        .projection()
        .pending_permissions
        .contains_key("perm-hook"));
    assert!(t.card_is_waiting("perm-hook"), "the card still waits");

    set.answer_card(tab, view(2), "perm-hook", agent::PermissionDecision::Allow)
        .map_err(|e| e.message)
        .unwrap();
    assert_eq!(provider.resolutions(), vec![("perm-hook".to_string(), true)]);
    shut_down_all(&mut set);
}

#[test]
fn the_base_view_keeps_the_active_tab_through_every_path_that_moves_it() {
    // Every base-view tab closed while an extra view is live: a fresh base tab opens.
    let mut set = set();
    let one = set.active();
    let two = set.open(AgentId::Claude);
    let three = set.open(AgentId::Claude);
    assert_eq!(set.break_out(view(2)), Ok(three));
    set.remove(two).unwrap();
    assert_eq!(set.active(), one);
    set.remove(one).unwrap();
    let fresh = set.active();
    assert!(fresh != one && fresh != two && fresh != three);
    assert_eq!(set.view_of(fresh), ViewId::BASE);
    assert_eq!(set.get(fresh).unwrap().number, 1);
    assert_eq!(set.tab_in(view(2)), Some(three), "the view keeps its tab");
    assert!(set.v1_holds());

    // A break-out whose last active tab is bound: the fallback skips it.
    let mut set = super::set();
    let one = set.active();
    let two = set.open(AgentId::Claude);
    let three = set.open(AgentId::Claude);
    assert_eq!(set.break_out(view(2)), Ok(three));
    assert_eq!(set.active(), two);
    set.last_active = Some(three);
    let four = set.open(AgentId::Claude);
    set.last_active = Some(three);
    assert_eq!(set.break_out(view(3)), Ok(four));
    assert!(set.view_of(set.active()).is_base());
    assert_eq!(
        set.active(),
        two,
        "the neighbour among the base view's tabs, never the bound tab"
    );
    assert_eq!(set.select_last(), None);
    assert_eq!(set.base_tabs(), vec![one, two]);
    assert!(set.v1_holds());

    // A resume of the session a bound tab holds: switched to, and `active` stays.
    let mut set = super::set();
    let one = set.active();
    let two = set.open(AgentId::Claude);
    set.defer_start(two, resume_of("r1", "sess-bound")).unwrap();
    assert_eq!(set.break_out(view(2)), Ok(two));
    assert_eq!(set.active(), one);
    assert_eq!(
        set.route_resume(one, &AgentId::Claude, "sess-bound", false),
        ResumeRoute::SwitchTo(two)
    );
    assert_eq!(set.active(), one);
    assert_eq!(set.view_of(two), view(2));
    assert!(set.v1_holds());

    // The active tab closed when its neighbour in tab order is bound: the fallback skips it.
    let mut set = super::set();
    let one = set.active();
    let two = set.open(AgentId::Claude);
    let three = set.open(AgentId::Claude);
    set.select(two);
    assert_eq!(set.break_out(view(2)), Ok(two));
    set.select(one);
    set.last_active = None;
    set.remove(one).unwrap();
    assert_eq!(set.active(), three, "not tab 2, which is bound");
    assert!(set.v1_holds());
    let four = set.open(AgentId::Claude);
    set.last_active = Some(two);
    set.remove(four).unwrap();
    assert_eq!(set.active(), three, "a bound last active tab is not the fallback");
    assert!(set.v1_holds());
}

#[test]
fn select_number_on_a_bound_tab_reports_its_view_and_leaves_active_alone() {
    let mut set = set();
    let one = set.active();
    let two = set.open(AgentId::Claude);
    assert_eq!(set.break_out(view(2)), Ok(two));
    assert_eq!(set.active(), one);
    let n_two = set.get(two).unwrap().number;
    assert_eq!(set.select_number(n_two), Some(TabPlace::View(view(2), two)));
    assert_eq!(set.active(), one);
    assert!(!set.select(two), "a bound tab is never selected");
    assert_eq!(set.active(), one);
    assert_eq!(set.select_place(two), Some(TabPlace::View(view(2), two)));
    assert_eq!(set.active(), one);
    assert_eq!(set.select_place(TabId(999)), None);
    assert_eq!(set.select_number(9), None);

    let three = set.open(AgentId::Claude);
    assert_eq!(set.select_number(1), Some(TabPlace::Base(one)));
    assert_eq!(set.active(), one);
    assert_eq!(set.select_place(three), Some(TabPlace::Base(three)));
    assert_eq!(set.active(), three);
    assert_eq!(set.step(1), Some(one), "`n` skips the bound tab 2 and wraps");
    assert_eq!(set.step(1), Some(three));
    assert_eq!(set.step(-1), Some(one));
}

#[test]
fn a_base_view_switch_drops_only_base_view_trust_questions() {
    let mut set = set();
    let one = set.active();
    let two = set.open(AgentId::Claude);
    let bound = set.open_in(AgentId::Claude, view(2)).unwrap();
    set.defer_start(bound, first_send("r-bound", "hi")).unwrap();
    let bound_nonce = set.ask_trust(bound).unwrap();
    set.select(one);
    set.defer_start(one, first_send("r-base", "hello")).unwrap();
    let base_nonce = set.ask_trust(one).unwrap();
    assert!(set.check_trust_answer(one, base_nonce, ViewId::BASE).is_ok());

    assert!(set.select(two));
    assert_eq!(
        awaiting(&set, one).asking,
        None,
        "the base view's question went with its tab"
    );
    assert_eq!(
        awaiting(&set, bound).asking,
        Some(bound_nonce),
        "the extra view still shows its own"
    );
    assert!(set.check_trust_answer(bound, bound_nonce, view(2)).is_ok());
    let number = set.get(bound).unwrap().number;
    assert_eq!(
        set.check_trust_answer(bound, bound_nonce, ViewId::BASE),
        Err(format!("tab {number} is not shown in this page"))
    );
    set.mark_recording(bound, bound_nonce, view(2)).unwrap();
    assert!(awaiting(&set, bound).recording);
}

#[test]
fn the_bypass_prompt_belongs_to_its_page() {
    let mut set = set();
    let base = set.active();
    let other = set.open(AgentId::Claude);
    set.select(base);
    let bound = set.open_in(AgentId::Claude, view(2)).unwrap();

    let built = plan(set.cycle_mode(bound, view(2)));
    assert_eq!(built.page, view(2));
    assert_eq!(
        set.confirm_bypass(built.scope, built.nonce, ViewId::BASE),
        Err("that prompt is no longer current".to_string())
    );
    set.drop_bypass_prompt_in(ViewId::BASE);
    assert!(set.select(other), "a base-view switch");
    assert!(
        set.pending_bypass.is_some(),
        "the base page's drops leave view 2's prompt"
    );
    set.drop_bypass_prompt_in(view(2));
    assert!(set.pending_bypass.is_none());
    assert_eq!(
        set.confirm_bypass(built.scope, built.nonce, view(2)),
        Err("that prompt is no longer current".to_string())
    );

    let again = plan(set.cycle_mode(bound, view(2)));
    assert!(matches!(
        set.confirm_bypass(again.scope, again.nonce, view(2)),
        Ok(ConfirmOutcome::Entered { .. })
    ));
    assert_eq!(set.get(bound).unwrap().mode(), SessionModeChoice::Bypass);

    // The window default's prompt is the page's that asked.
    set.default_mode = SessionModeChoice::Auto;
    let ModeCycle::Confirm(default_plan) = set.cycle_default_mode(view(2)) else {
        panic!("auto -> bypass asks first");
    };
    assert_eq!(default_plan.page, view(2));
    assert_eq!(
        set.confirm_bypass(default_plan.scope, default_plan.nonce, ViewId::BASE),
        Err("that prompt is no longer current".to_string())
    );
    assert!(matches!(
        set.confirm_bypass(default_plan.scope, default_plan.nonce, view(2)),
        Ok(ConfirmOutcome::Entered { .. })
    ));

    // The trust questions go only with the page that shows them.
    let waiting = set.open_in(AgentId::Claude, view(3)).unwrap();
    set.defer_start(waiting, first_send("r-view", "hi")).unwrap();
    let view_nonce = set.ask_trust(waiting).unwrap();
    let shown = set.active();
    set.defer_start(shown, first_send("r-base", "hello")).unwrap();
    let _ = set.ask_trust(shown).unwrap();
    set.drop_bypass_prompt_in(ViewId::BASE);
    assert_eq!(awaiting(&set, shown).asking, None);
    assert_eq!(awaiting(&set, waiting).asking, Some(view_nonce));
    assert!(set.check_trust_answer(waiting, view_nonce, view(3)).is_ok());
    let base_nonce = set.ask_trust(shown).unwrap();
    set.drop_bypass_prompt_in(view(3));
    assert_eq!(awaiting(&set, waiting).asking, None);
    assert_eq!(awaiting(&set, shown).asking, Some(base_nonce));
}

#[test]
fn open_in_never_makes_its_tab_active() {
    let mut set = set();
    let one = set.active();
    let two = set.open(AgentId::Claude);
    assert_eq!(set.active(), two);
    let opened = set.open_in(AgentId::Codex, view(2)).unwrap();
    assert_eq!(set.active(), two, "the base view's active tab is unchanged");
    assert_eq!(set.view_of(opened), view(2));
    assert_eq!(set.tab_in(view(2)), Some(opened));
    assert_eq!(set.get(opened).unwrap().number, 3, "the lowest free number");
    assert_eq!(*set.get(opened).unwrap().agent(), AgentId::Codex);
    assert!(matches!(set.get(opened).unwrap().backend, TabBackend::NotStarted));
    assert!(!set.recency.contains(&opened), "never active, not even for a moment");
    assert_eq!(set.select_last(), Some(one), "the last active tab is unchanged too");

    assert_eq!(
        set.open_in(AgentId::Claude, ViewId::BASE),
        Err(ViewError::NotAnExtraView)
    );
    assert_eq!(set.open_in(AgentId::Claude, view(2)), Err(ViewError::Taken(view(2))));
    assert_eq!(set.tabs().len(), 3, "a refused open makes no tab");
}

#[test]
fn cycle_mode_and_confirm_bypass_accept_a_shown_tab_from_its_own_page() {
    let mut set = set();
    let base = set.active();
    let background = set.open(AgentId::Claude);
    set.select(base);
    let bound = set.open_in(AgentId::Claude, view(2)).unwrap();
    let n = |set: &TabSet, tab: TabId| set.get(tab).unwrap().number;

    assert_eq!(
        set.cycle_mode(bound, ViewId::BASE),
        Err(format!("tab {} is not shown in this page", n(&set, bound)))
    );
    assert_eq!(
        set.cycle_mode(base, view(2)),
        Err(format!("tab {} is not shown in this page", n(&set, base)))
    );
    assert_eq!(
        set.cycle_mode(background, ViewId::BASE),
        Err(format!("tab {} is not the one on screen", n(&set, background)))
    );

    let bound_plan = plan(set.cycle_mode(bound, view(2)));
    assert_eq!(bound_plan.page, view(2));
    assert!(matches!(
        set.confirm_bypass(bound_plan.scope, bound_plan.nonce, view(2)),
        Ok(ConfirmOutcome::Entered { .. })
    ));
    let base_plan = plan(set.cycle_mode(base, ViewId::BASE));
    assert_eq!(base_plan.page, ViewId::BASE);
    assert!(matches!(
        set.confirm_bypass(base_plan.scope, base_plan.nonce, ViewId::BASE),
        Ok(ConfirmOutcome::Entered { .. })
    ));
    assert_eq!(set.get(bound).unwrap().mode(), SessionModeChoice::Bypass);
    assert_eq!(set.get(base).unwrap().mode(), SessionModeChoice::Bypass);
    assert_eq!(set.get(background).unwrap().mode(), SessionModeChoice::Auto);
}

#[cfg(feature = "legacy-backend")]
#[test]
fn legacy_events_reach_the_page_on_the_next_pump() {
    let dir = workspace("legacy-next-pump");
    let mut set = set();
    let tab = set.active();
    let mut backend = AgentBackend::Legacy(agent::AgentSession::for_tests());
    let outcome: Result<(), BackendError> = backend
        .send_turn("wire text", "typed text", PromptOrigin::Composed)
        .map(|_| ());
    assert!(
        outcome.is_ok(),
        "the send succeeds while event delivery waits for the pump"
    );
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let out = set.pump_views(&dir, &|_| true);
    let payload: serde_json::Value = serde_json::from_str(out.active_payload.as_deref().unwrap()).unwrap();
    assert_eq!(payload["events"][0]["type"], "user_prompt_submitted");
    assert_eq!(payload["events"][0]["text"], "typed text");
    assert_eq!(payload["events"][1]["type"], "turn_started");
    assert!(set.pump_views(&dir, &|_| true).active_payload.is_none());
    shut_down_all(&mut set);
}

#[cfg(feature = "legacy-backend")]
#[test]
fn a_rejected_legacy_send_keeps_its_prompt_in_the_pump() {
    let dir = workspace("legacy-refused-pump");
    let mut set = set();
    let tab = set.active();
    let mut backend = AgentBackend::Legacy(agent::AgentSession::for_tests());
    backend.send_turn("first", "first", PromptOrigin::Composed).unwrap();
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    set.pump_views(&dir, &|_| true);
    let error = set
        .get_mut(tab)
        .unwrap()
        .live_mut()
        .unwrap()
        .send_turn("second", "second", PromptOrigin::Composed)
        .unwrap_err();
    assert!(
        error.benign,
        "a running turn refuses another send without ending the session"
    );
    let out = set.pump_views(&dir, &|_| true);
    let payload: serde_json::Value = serde_json::from_str(out.active_payload.as_deref().unwrap()).unwrap();
    assert_eq!(payload["events"].as_array().unwrap().len(), 1);
    assert_eq!(payload["events"][0]["text"], "second");
    shut_down_all(&mut set);
}

#[cfg(feature = "legacy-backend")]
#[test]
fn a_legacy_snapshot_covers_queued_local_events() {
    let dir = workspace("legacy-snapshot-pump");
    let mut set = set();
    let tab = set.active();
    let mut backend = AgentBackend::Legacy(agent::AgentSession::for_tests());
    backend.send_turn("first", "first", PromptOrigin::Composed).unwrap();
    let revision = backend.projection().last_revision;
    backend.note_ui_snapshot(revision);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    assert!(
        set.pump_views(&dir, &|_| true).active_payload.is_none(),
        "a snapshot already drew the queued prompt and turn start"
    );
    shut_down_all(&mut set);
}

#[test]
fn a_bound_tabs_events_go_to_its_view_and_it_is_never_stale() {
    let dir = workspace("views-bound-tab-events");
    let mut set = set();
    let base = set.active();
    let (base_provider, backend) = live(&dir);
    set.get_mut(base).unwrap().backend = TabBackend::Live(backend);
    let bound = set.open(AgentId::Claude);
    let (bound_provider, backend) = live(&dir);
    set.get_mut(bound).unwrap().backend = TabBackend::Live(backend);
    assert_eq!(set.break_out(view(2)), Ok(bound));
    assert_eq!(set.active(), base);

    bound_provider.queue(started("v1"));
    bound_provider.queue(text_delta("v1", "from view two"));
    base_provider.queue(started("b1"));
    base_provider.queue(text_delta("b1", "from the base view"));
    let (mut base_text, mut bound_text) = (String::new(), String::new());
    until("both tabs' text", || {
        let out = set.pump(&dir, true);
        base_text += &events_text(out.active_payload.as_deref());
        for shown in &out.view_payloads {
            assert_eq!(
                (shown.view, shown.tab),
                (view(2), bound),
                "only the bound tab, in its view"
            );
            bound_text += &events_text(Some(&shown.payload));
        }
        assert!(out.view_payloads.len() <= 1);
        assert!(!set.get(bound).unwrap().session.stale, "a bound tab is never stale");
        base_text.contains("from the base view") && bound_text.contains("from view two")
    });
    assert!(
        !base_text.contains("from view two"),
        "the base page gets only its own tab"
    );
    assert!(!bound_text.contains("from the base view"));

    // Its view mapped: the turn's end is seen as it happens.
    bound_provider.queue(completed("v1", agent::TurnOutcome::Completed));
    until("the bound tab's turn end", || {
        set.pump_views(&dir, &|_| true);
        !set.get(bound).unwrap().session.was_running
    });
    assert!(!set.get(bound).unwrap().attention.attention().unread);

    // Its view unmapped: the next turn's end is left unread, and the base page is still told
    // nothing of it; the tab stays current for its own page.
    bound_provider.queue(started("v2"));
    bound_provider.queue(text_delta("v2", "while hidden"));
    bound_provider.queue(completed("v2", agent::TurnOutcome::Completed));
    let mut hidden_text = String::new();
    until("the hidden turn's end", || {
        let out = set.pump_views(&dir, &|v| v.is_base());
        assert!(!events_text(out.active_payload.as_deref()).contains("while hidden"));
        for shown in &out.view_payloads {
            assert_eq!((shown.view, shown.tab), (view(2), bound));
            hidden_text += &events_text(Some(&shown.payload));
        }
        assert!(!set.get(bound).unwrap().session.stale, "a bound tab is never stale");
        set.get(bound).unwrap().attention.attention().unread
    });
    assert!(hidden_text.contains("while hidden"), "{hidden_text}");
    assert!(set.attention_of(view(2)).unread);
    assert!(!set.attention_of(ViewId::BASE).unread);
    assert!(set.attention().unread, "the window's sum carries it");

    // The bound tab's own state, for its page; nothing for a background base-view tab.
    let snapshot = the_snapshot(set.state_payloads(bound));
    assert!(snapshot_text(&snapshot).contains("while hidden"));
    let third = set.open(AgentId::Claude);
    set.select(base);
    assert!(set.state_payloads(third).is_empty(), "no page shows a background tab");
    assert!(set.state_payloads(TabId(999)).is_empty());
    shut_down_all(&mut set);
}

/// A bound tab's finished turn carries its duration on the envelope to its own page, and a resync
/// sends that page the snapshot, never paced and never to the base page.
#[test]
fn a_bound_tabs_turn_times_and_resync_snapshot_go_to_its_view() {
    use crate::panel_cadence::EnvelopeClass::Immediate;
    let dir = workspace("views-bound-tab-resync");
    let mut set = set();
    let base = set.active();
    let bound = set.open(AgentId::Claude);
    let (provider, backend) = live(&dir);
    set.get_mut(bound).unwrap().backend = TabBackend::Live(backend);
    assert_eq!(set.break_out(view(2)), Ok(bound));
    assert_eq!(set.active(), base);

    provider.queue(started("t1"));
    until("the bound tab's turn to start", || {
        set.pump_views(&dir, &|_| true);
        set.get(bound).unwrap().session.was_running
    });
    provider.queue(text_delta("t1", "done"));
    provider.queue(completed("t1", agent::TurnOutcome::Completed));
    let mut times = None;
    until("an events payload for view 2 carrying turnTimes", || {
        let out = set.pump_views(&dir, &|_| true);
        assert!(out.active_payload.is_none(), "the base page is told nothing of it");
        for shown in out.view_payloads {
            assert_eq!((shown.view, shown.tab), (view(2), bound));
            let value: serde_json::Value = serde_json::from_str(&shown.payload).unwrap();
            if value["kind"] == "events" && value.get("turnTimes").is_some() {
                times = Some(value["turnTimes"].clone());
            }
        }
        times.is_some()
    });
    assert_eq!(times.unwrap()[0]["turnId"], "t1");
    assert_eq!(
        set.get(bound).unwrap().session.turn_times_unsent,
        0,
        "the envelope carried it"
    );

    // Without pumping in between: enough to overflow the sidecar's own UI queue.
    provider.queue(started("t2"));
    for i in 0..300 {
        provider.queue(text_delta("t2", &format!("chunk {i}")));
    }
    until("the whole stream folded", || {
        let backend = set.get(bound).unwrap().live().unwrap();
        let projection = backend.projection();
        projection.transcript.iter().any(|m| m.text.contains("chunk 299"))
    });
    let mut snapshot = None;
    until("a payload for view 2", || {
        let out = set.pump_views(&dir, &|_| true);
        assert!(out.active_payload.is_none(), "the base page is told nothing of it");
        if let Some(shown) = out.view_payloads.into_iter().next() {
            assert_eq!((shown.view, shown.tab), (view(2), bound));
            snapshot = Some((shown.payload, shown.class));
        }
        snapshot.is_some()
    });
    let (payload, class) = snapshot.unwrap();
    let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(value["kind"], "snapshot", "an overflow resyncs: {payload:.120}");
    assert_eq!(class, Immediate);
    assert_eq!(
        value["turnTimes"][0]["turnId"], "t1",
        "the snapshot carries the durations"
    );
    assert!(snapshot_text(&payload).contains("chunk 299"));
    assert!(!set.get(bound).unwrap().session.stale, "a bound tab is never stale");
    shut_down_all(&mut set);
}

#[test]
fn the_base_page_lists_base_tabs_and_an_extra_page_its_own() {
    let mut set = set();
    let one = set.active();
    let two = set.open(AgentId::Claude);
    let three = set.open(AgentId::Claude);
    set.select(two);
    assert_eq!(set.break_out(view(2)), Ok(two));
    assert_eq!(set.active(), three);
    let parse = |payload: String| -> (u64, Vec<u64>) {
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["kind"], "tabs");
        let numbers = value["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["number"].as_u64().unwrap())
            .collect();
        (value["active"].as_u64().unwrap(), numbers)
    };
    assert_eq!(
        parse(set.tabs_payload_for(ViewId::BASE)),
        (three.0, vec![1, 3]),
        "the bound tab's number is a gap"
    );
    assert_eq!(set.tabs_payload(), set.tabs_payload_for(ViewId::BASE));
    assert_eq!(
        parse(set.tabs_payload_for(view(2))),
        (two.0, vec![2]),
        "its one tab, as active"
    );
    assert_eq!(
        parse(set.tabs_payload_for(view(3))),
        (0, vec![]),
        "a view with no tab names none"
    );
    assert_eq!(set.get(one).unwrap().number, 1);
}

#[test]
fn mark_seen_marks_the_tab_it_names() {
    let mut set = set();
    let one = set.active();
    let two = set.open(AgentId::Claude);
    assert_eq!(set.break_out(view(2)), Ok(two));
    let ended = [completed("t", agent::TurnOutcome::Completed)];
    set.get_mut(one).unwrap().attention.observe(&ended, false);
    set.get_mut(two).unwrap().attention.observe(&ended, false);
    set.mark_seen(two);
    assert!(!set.get(two).unwrap().attention.attention().unread, "the tab it names");
    assert!(
        set.get(one).unwrap().attention.attention().unread,
        "the active tab is not it"
    );
    set.mark_seen(TabId(999));
    set.mark_seen(one);
    assert!(!set.attention().unread);
}

#[test]
fn attention_of_a_view_is_its_own_tabs() {
    let mut set = set();
    let one = set.active();
    let two = set.open(AgentId::Claude);
    let three = set.open(AgentId::Claude);
    let four = set.open(AgentId::Claude);
    assert_eq!(set.break_out(view(3)), Ok(four));
    set.select(two);
    assert_eq!(set.break_out(view(2)), Ok(two));
    assert_eq!(set.base_tabs(), vec![one, three]);
    set.get_mut(two).unwrap().attention.observe(&[write("oldest")], true);
    set.get_mut(one).unwrap().attention.observe(&[write("a")], false);
    set.get_mut(three).unwrap().attention.observe(&[write("b")], false);
    set.get_mut(three)
        .unwrap()
        .attention
        .observe(&[completed("t", agent::TurnOutcome::Completed)], false);

    let base = set.attention_of(ViewId::BASE);
    assert_eq!((base.pending, base.unread, base.arrived), (2, true, 2));
    let bound = set.attention_of(view(2));
    assert_eq!((bound.pending, bound.unread, bound.arrived), (1, false, 1));
    assert_eq!(set.attention_of(view(3)).pending, 0);
    assert_eq!(set.attention_of(view(4)), Attention::default(), "a view with no tab");
    assert_eq!(set.attention().pending, 3, "the window sums every view");
    assert_eq!(set.oldest_card_tab(), Some(two));
    assert_eq!(set.oldest_card_place(), Some(TabPlace::View(view(2), two)));
    assert_eq!(set.newest_card_tab_in(ViewId::BASE), Some(three));
    assert_eq!(set.newest_card_tab_in(view(2)), Some(two));
    assert_eq!(set.newest_card_tab_in(view(3)), None);
    let each = set.tab_attentions();
    let of = |tab: TabId| each.iter().find(|t| t.tab == tab).copied().unwrap();
    assert_eq!((of(two).view, of(two).attention.pending), (view(2), 1));
    assert_eq!(
        (of(four).view, of(four).attention.pending, of(four).newest_card),
        (view(3), 0, None)
    );
    assert!(of(three).attention.unread);

    // A closed tab's cards stay in the window's `arrived` only.
    set.remove(one).unwrap();
    assert_eq!(set.attention().arrived, 3);
    assert_eq!(set.attention_of(ViewId::BASE).arrived, 1);
    set.remove(two).unwrap();
    assert_eq!(set.oldest_card_place(), Some(TabPlace::Base(three)));
}

/// A restore waiting for its trust answer is written as it stands, and a crash meanwhile leaves that file to the next
/// start, which still has the cut turn to tell of.
#[test]
fn a_restore_waiting_for_its_answer_goes_on_saying_a_turn_never_finished() {
    let mut set = set();
    let from = set.active();
    let mut cut = planned("a", SessionModeChoice::Auto);
    cut.turn_running = true;
    let plan = plan_of(vec![cut, planned("b", SessionModeChoice::Auto)], Some("a"));
    let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("nothing to ask")
    };
    set.defer_start(from, DeferredStart::Restore { go }).unwrap();

    let saved = set.saved_snapshot("conv").unwrap();
    let running: Vec<(&str, bool)> = saved
        .tabs
        .iter()
        .map(|t| (t.provider_session_id.as_str(), t.turn_running))
        .collect();
    assert_eq!(running, [("a", true), ("b", false)]);
}

/// Claude and Codex tabs mixed in the tab bar are saved in the bar's order, though `tabs` lists the Claude ones first.
#[test]
fn a_saved_snapshot_keeps_the_tab_bars_order_across_agents() {
    let mut set = set();
    let failed_with = |set: &mut TabSet, tab: TabId, session: &str| {
        let t = set.get_mut(tab).unwrap();
        t.backend = TabBackend::Failed {
            reason: "refused".to_string(),
        };
        t.failed_resume = Some(session.to_string());
    };
    let first = set.active();
    set.get_mut(first).unwrap().agent = AgentId::Codex;
    failed_with(&mut set, first, "thread-1");
    let second = set.open(AgentId::Claude);
    failed_with(&mut set, second, "s-1");
    let third = set.open(AgentId::Codex);
    failed_with(&mut set, third, "thread-2");
    let fourth = set.open(AgentId::Claude);
    failed_with(&mut set, fourth, "s-2");

    let saved = set.saved_snapshot("conv").unwrap();
    let listed: Vec<&str> = saved.tabs.iter().map(|t| t.provider_session_id.as_str()).collect();
    assert_eq!(
        listed,
        ["s-1", "s-2", "thread-1", "thread-2"],
        "the file's two lists keep their own order"
    );
    let in_bar: Vec<&str> = saved
        .in_bar_order()
        .into_iter()
        .map(|t| t.provider_session_id.as_str())
        .collect();
    assert_eq!(in_bar, ["thread-1", "s-1", "thread-2", "s-2"]);
}

/// A skipped tab has no tab to wait in. While the restore waits for its trust answer the file keeps its entry,
/// or a window ending before the answer would lose what was typed for it; the answer puts the draft in a composer.
#[test]
fn a_skipped_tabs_draft_stays_in_the_file_while_the_restore_waits() {
    let mut set = set();
    let from = set.active();
    let mut plan = plan_of(vec![planned("a", SessionModeChoice::Auto)], Some("a"));
    plan.skipped.push(Skipped {
        label: "gone".to_string(),
        reason: "its record is gone".to_string(),
        kept: Some(SavedTab {
            draft: Some("for the gone tab".to_string()),
            ..saved_tab_named("gone")
        }),
    });
    let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("nothing to ask")
    };
    set.defer_start(from, DeferredStart::Restore { go }).unwrap();
    let saved = set.saved_snapshot("conv").unwrap();
    let kept = saved.tabs.iter().find(|t| t.provider_session_id == "gone").unwrap();
    assert_eq!(kept.draft.as_deref(), Some("for the gone tab"));
    assert_eq!(kept.conversation_id, "conv");
}

/// A waiting restore's tabs are in the file by way of its plan: each keeps its draft in the bar's order, and
/// keeps it again when the answer is put off and the tabs are kept as failed ones.
#[test]
fn a_waiting_or_put_off_restore_keeps_each_tabs_draft_in_the_bars_order() {
    let mut set = set();
    let from = set.active();
    let typed = |mut tab: PlannedTab| {
        tab.draft = Some(format!("typed for {}", tab.provider_session_id));
        tab
    };
    let codex = |id: &str| PlannedTab {
        agent: AgentId::Codex,
        ..planned(id, SessionModeChoice::Auto)
    };
    let plan = plan_of(
        vec![
            typed(codex("x")),
            typed(planned("a", SessionModeChoice::Auto)),
            typed(codex("y")),
        ],
        None,
    );
    let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("nothing to ask")
    };
    let drafts_in_bar_order = |set: &TabSet| -> Vec<(String, Option<String>)> {
        let saved = set.saved_snapshot("conv").unwrap();
        saved
            .in_bar_order()
            .into_iter()
            .map(|t| (t.provider_session_id.clone(), t.draft.clone()))
            .collect()
    };
    let want: Vec<(String, Option<String>)> = ["x", "a", "y"]
        .map(|id| (id.to_string(), Some(format!("typed for {id}"))))
        .into();
    set.defer_start(from, DeferredStart::Restore { go }).unwrap();
    assert_eq!(drafts_in_bar_order(&set), want, "waiting for the trust answer");

    let TabBackend::AwaitingTrust(AwaitingTrust {
        start: DeferredStart::Restore { go },
        ..
    }) = std::mem::replace(&mut set.get_mut(from).unwrap().backend, TabBackend::NotStarted)
    else {
        panic!("the restore waits in its tab")
    };
    set.keep_put_off_restore(from, go, "put off");
    assert_eq!(drafts_in_bar_order(&set), want, "put off");
}

/// A restore waiting for its trust answer keeps the order it was saved in, whichever agents its tabs run.
#[test]
fn a_waiting_restore_is_saved_in_the_order_it_was_planned() {
    let mut set = set();
    let from = set.active();
    let codex = |id: &str| PlannedTab {
        agent: AgentId::Codex,
        ..planned(id, SessionModeChoice::Auto)
    };
    let plan = plan_of(
        vec![codex("x"), planned("a", SessionModeChoice::Auto), codex("y")],
        None,
    );
    let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("nothing to ask")
    };
    set.defer_start(from, DeferredStart::Restore { go }).unwrap();
    let saved = set.saved_snapshot("conv").unwrap();
    let in_bar: Vec<&str> = saved
        .in_bar_order()
        .into_iter()
        .map(|t| t.provider_session_id.as_str())
        .collect();
    assert_eq!(in_bar, ["x", "a", "y"]);
}

#[test]
fn a_saved_snapshot_writes_each_bound_tabs_view() {
    let mut set = set();
    let failed_with = |set: &mut TabSet, tab: TabId, session: &str| {
        let t = set.get_mut(tab).unwrap();
        t.backend = TabBackend::Failed {
            reason: "refused".to_string(),
        };
        t.failed_resume = Some(session.to_string());
    };
    let one = set.active();
    failed_with(&mut set, one, "s-1");
    let two = set.open(AgentId::Claude);
    failed_with(&mut set, two, "s-2");
    let three = set.open(AgentId::Claude);
    failed_with(&mut set, three, "s-3");
    let codex = set.open(AgentId::Codex);
    failed_with(&mut set, codex, "thread-1");
    let other_codex = set.open(AgentId::Codex);
    failed_with(&mut set, other_codex, "thread-2");
    assert_eq!(set.break_out(view(4)), Ok(other_codex));
    set.select(two);
    assert_eq!(set.break_out(view(3)), Ok(two));
    set.select(three);

    let saved = set.saved_snapshot("conv").unwrap();
    let claude: Vec<(&str, Option<u8>)> = saved
        .claude_tabs()
        .map(|t| (t.provider_session_id.as_str(), t.view))
        .collect();
    assert_eq!(claude, [("s-1", None), ("s-2", Some(3)), ("s-3", None)]);
    assert_eq!(saved.active, Some(2), "the base view's active tab, as before");
    let external: Vec<(&str, Option<u8>)> = saved
        .other_tabs()
        .map(|t| (t.provider_session_id.as_str(), t.view))
        .collect();
    assert_eq!(external, [("thread-1", None), ("thread-2", Some(4))]);

    // A restore waiting for its trust answer keeps the views it was saved with meanwhile.
    let mut set = super::set();
    let from = set.active();
    let mut a = planned("a", SessionModeChoice::Auto);
    a.view = ViewId::new(2);
    let plan = plan_of(vec![a, planned("b", SessionModeChoice::Auto)], None);
    let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("nothing to ask")
    };
    set.defer_start(from, DeferredStart::Restore { go }).unwrap();
    let saved = set.saved_snapshot("conv").unwrap();
    let waiting: Vec<(&str, Option<u8>)> = saved
        .tabs
        .iter()
        .map(|t| (t.provider_session_id.as_str(), t.view))
        .collect();
    assert_eq!(waiting, [("a", Some(2)), ("b", None)]);
}

fn planned_in(id: &str, view: u8) -> PlannedTab {
    PlannedTab {
        view: ViewId::new(view),
        turn_running: false,
        ..planned(id, SessionModeChoice::Auto)
    }
}

fn restore_into(set: &mut TabSet, items: Vec<PlannedTab>, active: Option<&str>) -> (TabId, RestoreRun) {
    let from = set.active();
    let RestoreStep::Go(go) = set.begin_restore(plan_of(items, active), BypassPolicy::Ask) else {
        panic!("nothing to ask")
    };
    let run = set.start_restore(from, go, |_, _| Ok(RestoreConnect::Started(mpsc::channel().1)));
    assert!(set.v1_holds());
    (from, run)
}

fn tab_of(set: &TabSet, session: &str) -> TabId {
    set.tabs()
        .iter()
        .find(|t| t.provider_session_id().as_deref() == Some(session))
        .map(|t| t.id)
        .expect("the session has a tab")
}

#[test]
fn restored_tabs_go_to_their_views_once_and_otherwise_to_the_base_view() {
    let mut set = set();
    set.set_layout_views(vec![view(3), ViewId::BASE, view(2), view(3)]);
    let (from, run) = restore_into(
        &mut set,
        vec![planned_in("a", 2), planned_in("b", 2), planned_in("c", 4)],
        Some("a"),
    );
    let (a, b, c) = (tab_of(&set, "a"), tab_of(&set, "b"), tab_of(&set, "c"));
    assert_eq!(set.view_of(a), view(2), "the first tab naming agent#2 gets it");
    assert_ne!(
        a, from,
        "a tab bound to a view is a new tab, never the one the restore began in"
    );
    assert_eq!(set.view_of(b), ViewId::BASE, "agent#2 is taken by then");
    assert_eq!(b, from, "the first base-view tab goes into the empty tab");
    assert_eq!(set.view_of(c), ViewId::BASE, "the layout holds no agent#4");
    assert_eq!(set.tab_in(view(3)), None, "nothing was saved in agent#3");
    assert_eq!(
        set.active(),
        b,
        "the saved active tab is bound, so the base view shows its first restored tab"
    );
    assert!(set.view_of(set.active()).is_base());
    assert_eq!(set.base_tabs(), vec![b, c]);
    assert!(run.owns(a) && run.owns(b) && run.owns(c));
    assert!(
        run.began_in(from) && run.back_to(a).is_none(),
        "a failed bound tab goes away"
    );

    // Not told which views the layout holds: every tab comes back in the base view.
    let mut set = super::set();
    let (from, _run) = restore_into(&mut set, vec![planned_in("a", 2), planned_in("b", 3)], Some("b"));
    assert!(set.views().is_empty());
    assert_eq!(tab_of(&set, "a"), from);
    assert_eq!(
        set.active(),
        tab_of(&set, "b"),
        "the saved active tab, in the base view"
    );

    // A view already showing a tab is not given a second one.
    let mut set = super::set();
    let _one = set.active();
    let held = set.open(AgentId::Claude);
    assert_eq!(set.break_out(view(2)), Ok(held));
    set.set_layout_views(vec![view(2)]);
    let (_from, _run) = restore_into(&mut set, vec![planned_in("a", 2)], Some("a"));
    assert_eq!(set.tab_in(view(2)), Some(held));
    assert_eq!(set.view_of(tab_of(&set, "a")), ViewId::BASE);
    assert_eq!(set.active(), tab_of(&set, "a"));
}

/// The tab the screen falls back to when the restored active tab fails is a base-view tab: one that
/// came back bound to an extra view cannot be the base view's active tab.
#[test]
fn a_failed_restored_active_tab_falls_back_to_a_base_view_tab() {
    let mut set = set();
    set.set_layout_views(vec![view(2)]);
    let (from, mut run) = restore_into(
        &mut set,
        vec![
            planned_in("a", 2),
            planned("b", SessionModeChoice::Auto),
            planned("c", SessionModeChoice::Auto),
        ],
        Some("b"),
    );
    let (a, b, c) = (tab_of(&set, "a"), tab_of(&set, "b"), tab_of(&set, "c"));
    assert_eq!(set.view_of(a), view(2));
    assert_eq!((b, set.active()), (from, from));
    assert_eq!(run.first_survivor(), Some(b), "the bound tab is no place to go back to");

    let back_to = run.back_to(b);
    run.note_failed(b, "refused");
    set.get_mut(b).unwrap().backend = TabBackend::Failed {
        reason: "refused".to_string(),
    };
    set.discard_failed_restore(b, back_to);
    assert_eq!(run.first_survivor(), Some(c));
    assert!(set.select(c), "the fallback is a tab the base view can show");
    assert_eq!(set.active(), c);
    assert_eq!(set.view_of(a), view(2));
}

/// A put-off restore keeps its sessions as failed tabs, each in the view it was saved in under the
/// rule a started restore follows, so the next write of the file still names those views.
#[test]
fn a_put_off_restore_keeps_each_tabs_saved_view() {
    let mut set = set();
    set.set_layout_views(vec![view(2)]);
    let from = set.active();
    let RestoreStep::Go(go) = set.begin_restore(
        plan_of(
            vec![
                planned_in("a", 2),
                planned("b", SessionModeChoice::Auto),
                planned_in("c", 2),
            ],
            Some("a"),
        ),
        BypassPolicy::Ask,
    ) else {
        panic!("nothing to ask")
    };
    assert!(!set.keep_put_off_restore(from, go, "put off"));
    assert!(set.v1_holds());
    let kept = |set: &TabSet, session: &str| {
        set.tabs()
            .iter()
            .find(|t| t.failed_resume.as_deref() == Some(session))
            .map(|t| t.id)
            .expect("the session is kept in a failed tab")
    };
    let (a, b, c) = (kept(&set, "a"), kept(&set, "b"), kept(&set, "c"));
    assert_eq!(set.view_of(a), view(2));
    assert_ne!(a, from);
    assert_eq!(
        b, from,
        "the first base-view tab goes into the tab the restore began in"
    );
    assert_eq!(set.view_of(c), ViewId::BASE, "agent#2 is taken by then");
    assert_eq!(
        set.active(),
        b,
        "the saved active tab is bound, so the base view shows its first kept tab"
    );
    assert!(matches!(set.get(a).unwrap().backend, TabBackend::Failed { .. }));
    let saved = set.saved_snapshot("conv").unwrap();
    let mut views: Vec<(&str, Option<u8>)> = saved
        .tabs
        .iter()
        .map(|t| (t.provider_session_id.as_str(), t.view))
        .collect();
    views.sort();
    assert_eq!(views, [("a", Some(2)), ("b", None), ("c", None)]);

    // Not told which views the layout holds: every kept tab is in the base view.
    let mut set = super::set();
    let from = set.active();
    let RestoreStep::Go(go) = set.begin_restore(plan_of(vec![planned_in("a", 2)], Some("a")), BypassPolicy::Ask) else {
        panic!("nothing to ask")
    };
    set.keep_put_off_restore(from, go, "put off");
    assert_eq!(set.get(from).unwrap().failed_resume.as_deref(), Some("a"));
    assert!(set.views().is_empty());
}

/// The tab half of the layout's invariant run: random opens, breaks, joins, opens in a view,
/// closes and selections over empty tabs, checking after every one that the base view's active
/// tab is a base-view tab, the base view holds a tab, an extra view holds at most one, and
/// `shown()` lists each shown tab once.
#[test]
fn views_invariant_run() {
    struct XorShift(u64);
    impl XorShift {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    fn check(set: &TabSet, step: usize, op: &str) {
        let active = set.active();
        let at = |what: &str| format!("step {step} ({op}): {what}");
        assert!(
            set.get(active).is_some_and(|t| t.view_id().is_base()),
            "{}",
            at("active is a base-view tab")
        );
        assert!(!set.base_tabs().is_empty(), "{}", at("the base view holds a tab"));
        let mut per_view = std::collections::BTreeMap::<ViewId, usize>::new();
        for t in set.tabs().iter().filter(|t| !t.view_id().is_base()) {
            *per_view.entry(t.view_id()).or_default() += 1;
        }
        assert!(per_view.values().all(|n| *n == 1), "{}", at("one tab per extra view"));
        let shown = set.shown();
        assert_eq!(shown.first(), Some(&(ViewId::BASE, active)), "{}", at("active first"));
        assert_eq!(
            shown.iter().filter(|(_, t)| *t == active).count(),
            1,
            "{}",
            at("active once")
        );
        for t in set.tabs().iter().filter(|t| !t.view_id().is_base()) {
            assert_eq!(
                shown.iter().filter(|s| **s == (t.view_id(), t.id)).count(),
                1,
                "{}",
                at("every bound tab once")
            );
        }
        assert_eq!(shown.len(), 1 + per_view.len(), "{}", at("nothing else shown"));
        assert!(set.v1_holds(), "{}", at("v1"));
    }

    let mut rng = XorShift(0x2f2f_2f2f);
    let mut set = set();
    let pick_tab = |set: &TabSet, rng: &mut XorShift| set.tabs()[rng.below(set.tabs().len())].id;
    let pick_view = |rng: &mut XorShift| ViewId::new(2 + rng.below(8) as u8).unwrap();
    let mut base_kills = 0;
    for step in 0..5_000 {
        let op = if rng.below(20) == 0 { 10 } else { rng.below(10) };
        let name = match op {
            0 => {
                if set.tabs().len() < 14 {
                    set.open(AgentId::Claude);
                }
                "open"
            }
            1 => {
                let free: Vec<ViewId> = (2..=9u8)
                    .filter_map(ViewId::new)
                    .filter(|v| set.tab_in(*v).is_none())
                    .collect();
                if !free.is_empty() {
                    let v = free[rng.below(free.len())];
                    let before = set.active();
                    match set.break_out(v) {
                        Ok(moved) => assert_eq!(moved, before),
                        Err(e) => assert_eq!(e, ViewError::OneTabLeft),
                    }
                }
                "break_out"
            }
            2 => {
                let v = pick_view(&mut rng);
                let held = set.tab_in(v);
                assert_eq!(set.join_back(v), held);
                if let Some(tab) = held {
                    assert_eq!(set.active(), tab);
                }
                "join_back"
            }
            3 => {
                let v = pick_view(&mut rng);
                let before = set.active();
                if set.tabs().len() < 14 {
                    let taken = set.tab_in(v).is_some();
                    match set.open_in(AgentId::Claude, v) {
                        Ok(tab) => {
                            assert!(!taken);
                            assert_eq!(set.tab_in(v), Some(tab));
                        }
                        Err(e) => assert_eq!(e, ViewError::Taken(v)),
                    }
                }
                assert_eq!(set.active(), before, "open_in never moves active");
                "open_in"
            }
            4 => {
                let tab = pick_tab(&set, &mut rng);
                let its_view = set.view_of(tab);
                let views: Vec<ViewId> = set.views();
                set.remove(tab).unwrap();
                if !its_view.is_base() {
                    assert_eq!(set.tab_in(its_view), None, "its view is left empty");
                } else {
                    assert_eq!(set.views(), views, "closing a base-view tab leaves the views");
                }
                "remove"
            }
            5 => {
                let tab = pick_tab(&set, &mut rng);
                let base = set.view_of(tab).is_base();
                let before = set.active();
                assert_eq!(set.select(tab), base);
                assert_eq!(set.active(), if base { tab } else { before });
                "select"
            }
            6 => {
                let n = 1 + rng.below(16) as u16;
                let before = set.active();
                match set.select_number(n) {
                    Some(TabPlace::Base(tab)) => assert_eq!(set.active(), tab),
                    Some(TabPlace::View(v, tab)) => {
                        assert_eq!(set.tab_in(v), Some(tab));
                        assert_eq!(set.active(), before);
                    }
                    None => assert_eq!(set.active(), before),
                }
                "select_number"
            }
            7 => {
                set.step(if rng.below(2) == 0 { 1 } else { -1 });
                "step"
            }
            8 => {
                set.select_last();
                "select_last"
            }
            9 => {
                let tab = pick_tab(&set, &mut rng);
                let before = set.active();
                match set.select_place(tab) {
                    Some(TabPlace::Base(id)) => assert_eq!(set.active(), id),
                    Some(TabPlace::View(_, _)) => assert_eq!(set.active(), before),
                    None => panic!("the tab exists"),
                }
                "select_place"
            }
            _ => {
                // Every base-view tab closed in turn while the views are live: a fresh base
                // tab opens, and the views keep theirs.
                let views: Vec<(ViewId, TabId)> = set.shown().into_iter().skip(1).collect();
                if !views.is_empty() {
                    base_kills += 1;
                }
                for tab in set.base_tabs() {
                    set.remove(tab).unwrap();
                    check(&set, step, "a base-view kill");
                }
                assert_eq!(set.shown().into_iter().skip(1).collect::<Vec<_>>(), views);
                assert_eq!(set.base_tabs().len(), 1, "one fresh base tab");
                "kill the base view"
            }
        };
        check(&set, step, name);
    }
    assert!(base_kills > 0, "the run killed the base view while views were live");
}
