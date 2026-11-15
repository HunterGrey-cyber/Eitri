//! Entering bypass asks first and then Eitri answers every request; the default mode; closing and what it involves.
#![cfg(test)]

use super::*;

#[test]
fn cycle_default_mode_moves_the_window_default_and_leaves_open_tabs_alone() {
    let mut set = set();
    let tab = set.active();
    assert_eq!(set.default_mode(), SessionModeChoice::Auto);
    assert_eq!(set.get(tab).unwrap().mode, SessionModeChoice::Auto);

    let plan = match set.cycle_default_mode(ViewId::BASE) {
        ModeCycle::Confirm(plan) => plan,
        other => panic!("auto -> bypass always asks first: {other:?}"),
    };
    assert_eq!(plan.scope, BypassScope::Default);
    assert_eq!(
        set.default_mode(),
        SessionModeChoice::Auto,
        "nothing moves until confirmed"
    );

    assert_eq!(
        set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
        Ok(ConfirmOutcome::Entered { approved: 0 })
    );
    assert_eq!(set.default_mode(), SessionModeChoice::Bypass);
    assert_eq!(
        set.get(tab).unwrap().mode,
        SessionModeChoice::Auto,
        "the open tab's own mode is untouched"
    );
    assert_eq!(
        set.cycle_default_mode(ViewId::BASE),
        ModeCycle::Changed(SessionModeChoice::Manual),
        "leaving bypass moves at once, to manual"
    );
    assert_eq!(set.default_mode(), SessionModeChoice::Manual);
    assert_eq!(
        set.cycle_default_mode(ViewId::BASE),
        ModeCycle::Changed(SessionModeChoice::Auto),
        "manual -> auto moves at once"
    );
    assert_eq!(set.default_mode(), SessionModeChoice::Auto);
    assert!(
        matches!(set.cycle_default_mode(ViewId::BASE), ModeCycle::Confirm(_)),
        "and auto -> bypass asks again"
    );
    assert_eq!(
        set.get(tab).unwrap().mode,
        SessionModeChoice::Auto,
        "the open tab's own mode is untouched throughout"
    );
    shut_down_all(&mut set);
}

// Entering bypass asks first, then Eitri answers every request in it.

#[test]
fn entering_bypass_on_a_live_tab_asks_and_changes_nothing_until_confirmed() {
    let dir = workspace("bypass-ask-first");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

    let _plan = plan(set.cycle_mode(tab, ViewId::BASE));
    assert_eq!(
        set.get(tab).unwrap().mode,
        SessionModeChoice::Auto,
        "nothing changes until confirmed"
    );
    assert_eq!(
        set.default_mode(),
        SessionModeChoice::Auto,
        "the window default is untouched too"
    );

    provider.queue(write("p1"));
    until("the card", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 1
    });
    assert!(
        provider.resolutions().is_empty(),
        "a queued write still cards in auto: {:?}",
        provider.resolutions()
    );
    shut_down_all(&mut set);
}

#[test]
fn confirming_approves_exactly_the_listed_cards_on_that_tab() {
    let dir = workspace("bypass-approves-exactly-listed");
    let mut set = set();
    let first = set.active();
    let (first_provider, backend) = live(&dir);
    set.get_mut(first).unwrap().backend = TabBackend::Live(backend);
    let second = set.open(AgentId::Claude);
    let (second_provider, backend) = live(&dir);
    set.get_mut(second).unwrap().backend = TabBackend::Live(backend);

    first_provider.queue(write("p1"));
    second_provider.queue(write("p2"));
    until("both cards", || {
        set.pump(&dir, true);
        set.get(first).unwrap().attention.attention().pending == 1
            && set.get(second).unwrap().attention.attention().pending == 1
    });

    // `open()` above left `second` active, and entering bypass on a tab that is not on screen
    // is refused -- select back to `first` first.
    set.select(first);
    let plan = plan(set.cycle_mode(first, ViewId::BASE));
    assert_eq!(plan.approve, vec!["p1".to_string()]);
    assert!(plan.lines[0].contains("1 waiting card"), "{:?}", plan.lines);

    assert_eq!(
        set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
        Ok(ConfirmOutcome::Entered { approved: 1 })
    );
    assert_eq!(first_provider.resolutions(), vec![("p1".to_string(), true)]);
    assert!(second_provider.resolutions().is_empty());
    assert_eq!(
        set.get(first).unwrap().attention.attention().pending,
        0,
        "cleared at once, no extra pump needed"
    );
    assert_eq!(set.attention().pending, 1, "only the second tab's own card remains");
    shut_down_all(&mut set);
}

#[test]
fn bypass_never_counts_or_approves_a_deny_in_flight() {
    let dir = workspace("bypass-deny-in-flight");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
    provider.queue(write("denied"));
    provider.queue(write("waiting"));
    until("both cards", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 2
    });
    provider.hold_permission_replies();
    let send = set
        .answer_card_async(
            tab,
            ViewId::BASE,
            "denied",
            agent::PermissionDecision::Deny { reason: None },
        )
        .unwrap();
    assert!(set.finish_card_answer(tab, &send).is_none());
    provider.resume_permission_replies();
    let plan = plan(set.cycle_mode(tab, ViewId::BASE));
    assert_eq!(plan.approve, ["waiting"]);
    assert!(plan.lines[0].contains("1 waiting card"), "{:?}", plan.lines);
    assert_eq!(
        set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
        Ok(ConfirmOutcome::Entered { approved: 1 })
    );
    assert_eq!(provider.resolutions(), [("waiting".to_string(), true)]);

    // Overflow the delivery queue so the bypass resync must sweep the pending projection.
    for i in 0..300 {
        provider.queue(AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: agent::ContentKind::Text,
            text: format!("chunk {i}"),
        });
    }
    provider.queue(write("overflow"));
    until("overflow folded", || {
        set.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .pending_permissions
            .contains_key("overflow")
    });
    let payload = set.pump(&dir, true).active_payload.unwrap();
    let snapshot: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(snapshot["kind"], "snapshot");
    assert_eq!(
        provider.resolutions(),
        [("waiting".to_string(), true), ("overflow".to_string(), true)]
    );
    assert!(set.finish_card_answer(tab, &send).is_none());
    provider.reply_to_held_permissions(Ok(None));
    set.finish_card_answer(tab, &send).unwrap().unwrap();
    shut_down_all(&mut set);
}

#[test]
fn a_card_arriving_while_the_prompt_is_up_reprompts() {
    let dir = workspace("bypass-reprompt-on-new-card");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

    provider.queue(write("p1"));
    until("p1's card", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 1
    });
    let old_plan = plan(set.cycle_mode(tab, ViewId::BASE));
    assert_eq!(old_plan.approve, vec!["p1".to_string()]);

    provider.queue(write("p2"));
    until("p2's card too", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 2
    });

    let reprompted = match set.confirm_bypass(old_plan.scope, old_plan.nonce, ViewId::BASE) {
        Ok(ConfirmOutcome::Reprompt(plan)) => plan,
        other => panic!("a card arrived while the prompt was up: {other:?}"),
    };
    assert_eq!(reprompted.approve, vec!["p1".to_string(), "p2".to_string()]);
    assert_ne!(reprompted.nonce, old_plan.nonce);
    assert!(
        reprompted.lines[0].contains("2 waiting cards"),
        "{:?}",
        reprompted.lines
    );
    assert_eq!(
        set.get(tab).unwrap().mode,
        SessionModeChoice::Auto,
        "nothing changed yet"
    );
    assert!(provider.resolutions().is_empty(), "no resolution sent by a reprompt");

    assert_eq!(
        set.confirm_bypass(old_plan.scope, old_plan.nonce, ViewId::BASE),
        Err("that prompt is no longer current".to_string()),
        "the OLD nonce is dead now"
    );
    assert_eq!(
        set.confirm_bypass(reprompted.scope, reprompted.nonce, ViewId::BASE),
        Ok(ConfirmOutcome::Entered { approved: 2 })
    );
    shut_down_all(&mut set);
}

#[test]
fn in_bypass_every_request_is_answered_and_never_delivered() {
    let dir = workspace("bypass-answers-everything");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

    let plan = plan(set.cycle_mode(tab, ViewId::BASE));
    assert_eq!(
        set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
        Ok(ConfirmOutcome::Entered { approved: 0 })
    );

    provider.queue(write("p3"));
    provider.queue(outside("p4"));
    until("both resolved", || {
        let out = set.pump(&dir, true);
        if let Some(payload) = &out.active_payload {
            assert!(!payload.contains("\"p3\"") && !payload.contains("\"p4\""), "{payload}");
        }
        let mut resolved: Vec<String> = provider.resolutions().into_iter().map(|(id, _)| id).collect();
        resolved.sort();
        resolved == vec!["p3".to_string(), "p4".to_string()]
    });
    assert_eq!(set.get(tab).unwrap().attention.attention().pending, 0);
    assert_eq!(
        set.get(tab).unwrap().session.host_answered,
        BTreeSet::from(["p3".to_string(), "p4".to_string()])
    );

    provider.queue(AgentDomainEvent::PermissionResolved {
        permission_id: "p3".into(),
        outcome: agent::PermissionOutcome::Allowed,
    });
    provider.queue(AgentDomainEvent::PermissionResolved {
        permission_id: "p4".into(),
        outcome: agent::PermissionOutcome::Allowed,
    });
    until("host_answered empties", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().session.host_answered.is_empty()
    });
    shut_down_all(&mut set);
}

#[test]
fn leaving_bypass_is_immediate_and_the_classifier_decides_again() {
    let dir = workspace("bypass-leave-immediate");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let plan = plan(set.cycle_mode(tab, ViewId::BASE));
    assert_eq!(
        set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
        Ok(ConfirmOutcome::Entered { approved: 0 })
    );

    // Leaving bypass lands on manual, where the policy answers again (and an edit cards).
    assert_eq!(
        set.cycle_mode(tab, ViewId::BASE),
        Ok(ModeCycle::Changed(SessionModeChoice::Manual))
    );

    provider.queue(write("p5"));
    provider.queue(read("p6"));
    // Both halves are waited for: the ingestion thread may deliver p5 and p6 in different polls,
    // so p5's card being up does not yet mean p6 was answered (seen once under a loaded host).
    until("p5 cards and p6 is answered", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 1 && !provider.resolutions().is_empty()
    });
    assert_eq!(provider.resolutions(), vec![("p6".to_string(), true)]);
    shut_down_all(&mut set);
}

#[test]
fn an_empty_tab_confirm_moves_the_window_default_and_new_tabs_inherit_it_unasked() {
    let mut set = set();
    let tab = set.active();
    let plan = plan(set.cycle_mode(tab, ViewId::BASE));
    assert_eq!(plan.scope, BypassScope::Tab(tab));
    assert!(
        plan.lines[0].contains("New sessions in this window start in bypass too"),
        "{:?}",
        plan.lines
    );

    assert_eq!(
        set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
        Ok(ConfirmOutcome::Entered { approved: 0 })
    );
    assert_eq!(set.default_mode(), SessionModeChoice::Bypass);

    let next = set.open(AgentId::Claude);
    assert_eq!(
        set.get(next).unwrap().mode,
        SessionModeChoice::Bypass,
        "inherited unasked"
    );
    assert_eq!(
        set.confirm_bypass(BypassScope::Tab(next), 0, ViewId::BASE),
        Err("that prompt is no longer current".to_string()),
        "open() left no stray prompt behind"
    );
    shut_down_all(&mut set);
}

#[test]
fn the_chooser_default_asks_before_bypass_and_not_before_auto() {
    let mut set = set();
    let tab = set.active();
    match set.cycle_default_mode(ViewId::BASE) {
        ModeCycle::Confirm(plan) => {
            assert_eq!(plan.scope, BypassScope::Default);
            assert_eq!(
                set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
                Ok(ConfirmOutcome::Entered { approved: 0 })
            );
        }
        other => panic!("auto -> bypass must ask first: {other:?}"),
    }
    assert_eq!(set.default_mode(), SessionModeChoice::Bypass);
    assert_eq!(
        set.get(tab).unwrap().mode,
        SessionModeChoice::Auto,
        "an open tab never changes"
    );

    assert_eq!(
        set.cycle_default_mode(ViewId::BASE),
        ModeCycle::Changed(SessionModeChoice::Manual),
        "bypass -> manual never asks"
    );
    assert_eq!(
        set.cycle_default_mode(ViewId::BASE),
        ModeCycle::Changed(SessionModeChoice::Auto),
        "manual -> auto never asks"
    );
    assert_eq!(set.get(tab).unwrap().mode, SessionModeChoice::Auto, "still untouched");
    shut_down_all(&mut set);
}

#[test]
fn entering_bypass_on_ended_and_failed_tabs_refuses_and_leaving_it_works() {
    let dir = workspace("bypass-ended-failed");

    // An ENDED AUTO tab refuses to enter bypass.
    {
        let mut set = set();
        let ended_auto = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(ended_auto).unwrap().backend = TabBackend::Live(backend);
        set.get_mut(ended_auto).unwrap().live_mut().unwrap().shutdown();
        provider.queue(AgentDomainEvent::SessionClosed {
            reason: "closed".into(),
        });
        until("ended", || {
            set.pump(&dir, true);
            set.get(ended_auto).unwrap().wire_state() == TabStateWire::Ended
        });
        assert_eq!(
            set.cycle_mode(ended_auto, ViewId::BASE),
            Err("the session has ended".to_string())
        );
        shut_down_all(&mut set);
    }

    // A STARTING tab asks like a live one: `Confirm`, LiveTab text, zero cards. If starting
    // fails, the FAILED bypass tab still leaves bypass immediately.
    {
        let mut set = set();
        let starting_tab = set.active();
        let tx = starting(&mut set, starting_tab, None, None);
        let plan = plan(set.cycle_mode(starting_tab, ViewId::BASE));
        assert_eq!(plan.approve, Vec::<String>::new());
        assert_eq!(
            plan.lines,
            vec!["Switch to bypass? (y/n)".to_string()],
            "a starting tab asks like a live one"
        );
        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
            Ok(ConfirmOutcome::Entered { approved: 0 })
        );
        tx.send(Err(BackendError {
            message: "boom".into(),
            benign: false,
        }))
        .unwrap();
        set.collect_starts();
        assert_eq!(set.get(starting_tab).unwrap().wire_state(), TabStateWire::Failed);
        assert_eq!(
            set.get(starting_tab).unwrap().mode,
            SessionModeChoice::Bypass,
            "the mode survived the failed start"
        );
        assert_eq!(
            set.cycle_mode(starting_tab, ViewId::BASE),
            Ok(ModeCycle::Changed(SessionModeChoice::Manual)),
            "a failed bypass tab still leaves at once"
        );
        assert_eq!(
            set.cycle_mode(starting_tab, ViewId::BASE),
            Err("the session has ended".to_string()),
            "a failed manual tab stays manual, as an ended auto tab stays auto"
        );
        shut_down_all(&mut set);
    }

    // An ENDED bypass tab also leaves at once...
    {
        let mut set = set();
        let ended_bypass = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(ended_bypass).unwrap().backend = TabBackend::Live(backend);
        let plan = plan(set.cycle_mode(ended_bypass, ViewId::BASE));
        set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();
        set.get_mut(ended_bypass).unwrap().live_mut().unwrap().shutdown();
        provider.queue(AgentDomainEvent::SessionClosed {
            reason: "closed".into(),
        });
        until("ended", || {
            set.pump(&dir, true);
            set.get(ended_bypass).unwrap().wire_state() == TabStateWire::Ended
        });
        assert_eq!(
            set.cycle_mode(ended_bypass, ViewId::BASE),
            Ok(ModeCycle::Changed(SessionModeChoice::Manual)),
            "an ended bypass tab still leaves at once"
        );
        shut_down_all(&mut set);
    }

    // `reset` on an ended bypass tab keeps its mode and leaves the window default untouched.
    {
        let mut set = set();
        let ended_bypass = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(ended_bypass).unwrap().backend = TabBackend::Live(backend);
        let plan = plan(set.cycle_mode(ended_bypass, ViewId::BASE));
        set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();
        let default_after_confirm = set.default_mode();
        set.get_mut(ended_bypass).unwrap().live_mut().unwrap().shutdown();
        provider.queue(AgentDomainEvent::SessionClosed {
            reason: "closed".into(),
        });
        until("ended", || {
            set.pump(&dir, true);
            set.get(ended_bypass).unwrap().wire_state() == TabStateWire::Ended
        });
        set.reset(ended_bypass).unwrap();
        assert_eq!(
            set.get(ended_bypass).unwrap().mode,
            SessionModeChoice::Bypass,
            "reset keeps bypass"
        );
        assert_eq!(
            set.default_mode(),
            default_after_confirm,
            "reset does not touch the window default"
        );
        shut_down_all(&mut set);
    }
}

#[test]
fn a_resync_in_bypass_sweeps_the_pending_cards_and_draws_none() {
    let dir = workspace("bypass-resync-sweep");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

    let plan = plan(set.cycle_mode(tab, ViewId::BASE));
    assert_eq!(
        set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
        Ok(ConfirmOutcome::Entered { approved: 0 })
    );

    provider.queue(write("pre"));
    until(
        "pre answered, still pending (the double no test provides its own resolution)",
        || {
            set.pump(&dir, true);
            set.get(tab).unwrap().session.host_answered.contains("pre")
                && set
                    .get(tab)
                    .unwrap()
                    .live()
                    .unwrap()
                    .projection()
                    .pending_permissions
                    .contains_key("pre")
        },
    );

    // Without pumping in between: enough to overflow the sidecar's own UI queue.
    provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    for i in 0..300 {
        provider.queue(AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: agent::ContentKind::Text,
            text: format!("chunk {i}"),
        });
    }
    provider.queue(write("lost1"));
    provider.queue(write("lost2"));
    // The CLI's own prompt, forced by the user's ask rule: bypass sweeps it like the gate's own.
    provider.queue(cli_prompt(
        "lost-ask",
        "toolu_lost_ask",
        "Bash",
        serde_json::json!({ "command": "cat notes.txt" }),
        Some("cat:*"),
    ));

    until("all four pending in the projection", || {
        let Some(backend) = set.get(tab).unwrap().live() else {
            return false;
        };
        backend.projection().pending_permissions.len() == 4
    });

    let out = set.pump(&dir, true);
    let payload = out.active_payload.expect("the active tab's own resync payload");
    let snapshot: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(snapshot["kind"], "snapshot");

    let mut resolved: Vec<String> = provider.resolutions().into_iter().map(|(id, _)| id).collect();
    resolved.sort();
    assert_eq!(
        resolved,
        vec![
            "lost-ask".to_string(),
            "lost1".to_string(),
            "lost2".to_string(),
            "pre".to_string()
        ]
    );
    assert_eq!(
        provider.resolutions().iter().filter(|(id, _)| id == "pre").count(),
        1,
        "pre was already host-answered before the resync -- never answered twice"
    );

    assert_eq!(set.get(tab).unwrap().attention.attention().pending, 0);
    let pending = snapshot["state"]["pendingPermissions"].as_array().unwrap();
    assert!(pending.is_empty(), "no swept id is drawn: {pending:?}");
    assert_eq!(
        set.get(tab).unwrap().session.note_ledger.prompt_note("toolu_lost_ask"),
        Some("your ask rule: Bash(cat:*) — allowed in bypass"),
        "the swept CLI prompt's call will say so on its row"
    );
    shut_down_all(&mut set);
}

/// The gap `attention`'s module doc used to record for `resync` is closed in `Auto` too: an id
/// the classifier already answered is swept from the snapshot and the tray exactly as bypass's
/// own sweep is, even though nothing here re-answers it (it was never in `Bypass`).
#[test]
fn a_resync_in_auto_hides_what_the_classifier_answered() {
    let dir = workspace("auto-resync-hides-classifier-answer");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

    provider.queue(read("seen"));
    until("the classifier answered it", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().session.host_answered.contains("seen")
    });

    provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    for i in 0..300 {
        provider.queue(AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: agent::ContentKind::Text,
            text: format!("chunk {i}"),
        });
    }
    provider.queue(write("lost"));

    until("both pending in the projection", || {
        let Some(backend) = set.get(tab).unwrap().live() else {
            return false;
        };
        backend.projection().pending_permissions.len() == 2
    });

    let out = set.pump(&dir, true);
    let payload = out.active_payload.expect("the active tab's own resync payload");
    let snapshot: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(snapshot["kind"], "snapshot");
    let pending = snapshot["state"]["pendingPermissions"].as_array().unwrap();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0]["permissionId"], "lost");
    assert_eq!(set.get(tab).unwrap().attention.attention().pending, 1);
    shut_down_all(&mut set);
}

/// `Tab::mode` remains the authority regardless of provider reports. A real provider turns an
/// unrestricted mode report into `UngatedCliMode` rather than a raw `PermissionModeChanged`;
/// even a raw event must not move the tab's mode.
#[test]
fn a_provider_mode_report_moves_nothing() {
    let dir = workspace("mode-report-is-inert");
    let mut set = set();
    let auto_tab = set.active();
    let (auto_provider, backend) = live(&dir);
    set.get_mut(auto_tab).unwrap().backend = TabBackend::Live(backend);
    auto_provider.queue(AgentDomainEvent::PermissionModeChanged {
        mode: agent::PermissionMode::Bypass,
        provider_mode: "default".into(),
        floor_applied: false,
    });
    auto_provider.queue(write("still-a-card"));
    until("the write still cards", || {
        set.pump(&dir, true);
        set.get(auto_tab).unwrap().attention.attention().pending == 1
    });
    assert_eq!(
        set.get(auto_tab).unwrap().mode,
        SessionModeChoice::Auto,
        "a provider report never moves Tab::mode"
    );

    let bypass_tab = set.open(AgentId::Claude);
    let (bypass_provider, backend) = live(&dir);
    set.get_mut(bypass_tab).unwrap().backend = TabBackend::Live(backend);
    let plan = plan(set.cycle_mode(bypass_tab, ViewId::BASE));
    set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();
    bypass_provider.queue(AgentDomainEvent::PermissionModeChanged {
        mode: agent::PermissionMode::Auto,
        provider_mode: "default".into(),
        floor_applied: false,
    });
    bypass_provider.queue(write("answered-anyway"));
    until("it is answered anyway", || {
        set.pump(&dir, true);
        bypass_provider
            .resolutions()
            .iter()
            .any(|(id, _)| id == "answered-anyway")
    });
    assert_eq!(
        set.get(bypass_tab).unwrap().mode,
        SessionModeChoice::Bypass,
        "still bypass"
    );
    shut_down_all(&mut set);
}

#[test]
fn a_prompt_dies_with_a_tab_switch() {
    let dir = workspace("bypass-prompt-dies-on-switch");

    fn two_tabs(dir: &Path) -> (TabSet, TabId, TabId) {
        let mut set = set();
        let first = set.active();
        let (_p, backend) = live(dir);
        set.get_mut(first).unwrap().backend = TabBackend::Live(backend);
        let second = set.open(AgentId::Claude);
        let (_p, backend) = live(dir);
        set.get_mut(second).unwrap().backend = TabBackend::Live(backend);
        set.select(first);
        (set, first, second)
    }

    for mutate in [
        "select_second",
        "open",
        "step",
        "select_number",
        "select_last",
        "remove_first",
    ] {
        let (mut set, first, second) = two_tabs(&dir);
        let plan = plan(set.cycle_mode(first, ViewId::BASE));
        match mutate {
            "select_second" => {
                set.select(second);
            }
            "open" => {
                set.open(AgentId::Claude);
            }
            "step" => {
                set.step(1);
            }
            "select_number" => {
                set.select_number(set.get(second).unwrap().number);
            }
            "select_last" => {
                set.select(second);
                set.select(first);
                set.select_last();
            }
            "remove_first" => {
                set.remove(first);
            }
            _ => unreachable!(),
        }
        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
            Err("that prompt is no longer current".to_string()),
            "{mutate}"
        );
        if mutate != "remove_first" {
            assert_eq!(set.get(first).unwrap().mode, SessionModeChoice::Auto, "{mutate}");
        }
        shut_down_all(&mut set);
    }

    // The strongest case: select(second) then back to select(first) -- the SAME tab ends up
    // active again -- still drops the prompt, since the active id genuinely changed in between.
    let (mut set, first, second) = two_tabs(&dir);
    let first_plan = plan(set.cycle_mode(first, ViewId::BASE));
    set.select(second);
    set.select(first);
    assert_eq!(
        set.confirm_bypass(first_plan.scope, first_plan.nonce, ViewId::BASE),
        Err("that prompt is no longer current".to_string())
    );

    let second_plan = plan(set.cycle_mode(first, ViewId::BASE));
    set.drop_bypass_prompt();
    assert_eq!(
        set.confirm_bypass(second_plan.scope, second_plan.nonce, ViewId::BASE),
        Err("that prompt is no longer current".to_string())
    );
    shut_down_all(&mut set);
}

#[test]
fn entering_bypass_on_a_tab_that_is_not_active_is_refused() {
    let dir = workspace("bypass-not-active-refused");
    let mut set = set();
    let first = set.active();
    let (_p, backend) = live(&dir);
    set.get_mut(first).unwrap().backend = TabBackend::Live(backend);
    let second = set.open(AgentId::Claude);
    let (_p, backend) = live(&dir);
    set.get_mut(second).unwrap().backend = TabBackend::Live(backend);
    set.select(first);
    assert_eq!(
        set.cycle_mode(second, ViewId::BASE),
        Err(format!(
            "tab {} is not the one on screen",
            set.get(second).unwrap().number
        ))
    );
    shut_down_all(&mut set);
}

#[test]
fn the_count_is_the_delivered_cards_not_the_projection() {
    let dir = workspace("bypass-count-is-delivered-cards");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

    // Stays pending in the projection until its resolution is queued (`RecordingProvider` never
    // emits one on its own) -- this holds the classifier's own answer back, so a prompt the
    // classifier answered sits pending in the projection yet never becomes a card.
    provider.queue(read("classifier-answered"));
    provider.queue(write("real-card"));
    until("only the real card is a card", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 1
    });
    assert!(
        set.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .pending_permissions
            .contains_key("classifier-answered"),
        "the premise: it has not resolved yet"
    );

    let plan = plan(set.cycle_mode(tab, ViewId::BASE));
    assert_eq!(plan.approve, vec!["real-card".to_string()]);
    assert!(plan.lines[0].contains("1 waiting card"), "{:?}", plan.lines);
    assert_eq!(
        set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
        Ok(ConfirmOutcome::Entered { approved: 1 }),
        "must not Reprompt for the classifier-answered id"
    );
    let mut resolutions = provider.resolutions();
    resolutions.sort();
    assert_eq!(
        resolutions,
        vec![
            ("classifier-answered".to_string(), true),
            ("real-card".to_string(), true)
        ],
        "each answered exactly once"
    );
    shut_down_all(&mut set);
}

/// The attention half of `waiting_cards`' filter, isolated from the `host_answered` half: a
/// request the background ingestion has already folded into the projection, but that no pump has
/// yet delivered as a card (`tab.attention.card_ids()`), must not be offered for bypass approval
/// either. Without this test, removing `cards.contains(&p.permission_id)` from `waiting_cards` and
/// keeping only the `host_answered` filter left the whole suite passing, because
/// `the_count_is_the_delivered_cards_not_the_projection`'s held-back id is also `host_answered`
/// (the classifier already allowed it), which masks the attention half entirely.
#[test]
fn waiting_cards_excludes_a_request_not_yet_delivered_as_a_card() {
    let dir = workspace("bypass-waiting-cards-excludes-undelivered");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

    provider.queue(write("real-card"));
    until(
        "the projection holds it, before any pump has delivered it as a card",
        || {
            set.get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .pending_permissions
                .contains_key("real-card")
        },
    );
    assert_eq!(
        set.get(tab).unwrap().attention.attention().pending,
        0,
        "the premise: nothing has been pumped, so nothing is a delivered card yet"
    );

    let plan = plan(set.cycle_mode(tab, ViewId::BASE));
    assert!(
        plan.approve.is_empty(),
        "a request only in the projection, never delivered as a card, must not be offered for \
         bypass approval: {:?}",
        plan.approve
    );
    shut_down_all(&mut set);
}

#[test]
fn leaving_bypass_on_a_live_tab_drops_the_window_bypass_default() {
    let dir = workspace("bypass-leaving-live-drops-default");
    let mut set = set();
    let tab = set.active();
    let plan = plan(set.cycle_mode(tab, ViewId::BASE));
    assert_eq!(
        set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
        Ok(ConfirmOutcome::Entered { approved: 0 })
    );
    assert_eq!(set.default_mode(), SessionModeChoice::Bypass);

    let (_p, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    assert_eq!(
        set.get(tab).unwrap().mode,
        SessionModeChoice::Bypass,
        "starting the session kept the mode"
    );

    assert_eq!(
        set.cycle_mode(tab, ViewId::BASE),
        Ok(ModeCycle::Changed(SessionModeChoice::Manual))
    );
    assert_eq!(
        set.default_mode(),
        SessionModeChoice::Auto,
        "leaving bypass on ANY tab drops an in-window default too, to auto"
    );

    let next = set.open(AgentId::Claude);
    assert_eq!(set.get(next).unwrap().mode, SessionModeChoice::Auto);
    shut_down_all(&mut set);
}

/// A live-tab bypass prompt says nothing about new sessions. If a terminal handoff changes the
/// tab to `NotStarted` while that prompt waits, accepting it must not silently change the window
/// default and put later new tabs into bypass. Confirmation remembers the question shown and
/// asks again when it no longer describes the tab.
#[test]
fn a_live_tab_prompt_answered_after_the_tab_became_empty_reprompts_and_moves_nothing() {
    let dir = workspace("bypass-prompt-kind-changed");
    let mut set = set();
    let tab = set.active();
    let (_provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

    let live_plan = plan(set.cycle_mode(tab, ViewId::BASE));
    assert_eq!(live_plan.prompt, tabs::PromptScope::LiveTab);
    assert_eq!(
        live_plan.lines,
        vec![tabs::bypass_prompt(tabs::PromptScope::LiveTab, 0)]
    );

    // What `agent_panel`'s `HandoffToTerminal` arm does to the tab (its own drop of the prompt
    // is the other half of the protection; this is the half that holds even without it).
    if let TabBackend::Live(mut backend) =
        std::mem::replace(&mut set.get_mut(tab).unwrap().backend, TabBackend::NotStarted)
    {
        backend.shutdown();
    }

    let fresh = match set.confirm_bypass(live_plan.scope, live_plan.nonce, ViewId::BASE) {
        Ok(ConfirmOutcome::Reprompt(fresh)) => fresh,
        other => panic!("the question changed, so the answer must be asked again: {other:?}"),
    };
    assert_eq!(fresh.prompt, tabs::PromptScope::EmptyTab);
    assert_eq!(fresh.lines, vec![tabs::bypass_prompt(tabs::PromptScope::EmptyTab, 0)]);
    assert_ne!(fresh.nonce, live_plan.nonce);
    assert_eq!(set.get(tab).unwrap().mode, SessionModeChoice::Auto, "nothing entered");
    assert_eq!(
        set.default_mode(),
        SessionModeChoice::Auto,
        "and the window default never moved"
    );
    assert_eq!(
        set.confirm_bypass(live_plan.scope, live_plan.nonce, ViewId::BASE),
        Err("that prompt is no longer current".to_string())
    );
    let next = set.open(AgentId::Claude);
    assert_eq!(
        set.get(next).unwrap().mode,
        SessionModeChoice::Auto,
        "a new tab is still auto"
    );
    shut_down_all(&mut set);
}

/// The same re-check the other way: an empty tab's prompt (which DOES say new sessions follow)
/// answered once the tab has started is asked again as a live tab's, which moves only the tab.
#[test]
fn an_empty_tab_prompt_answered_after_the_tab_started_reprompts_as_a_live_tab() {
    let dir = workspace("bypass-prompt-kind-changed-2");
    let mut set = set();
    let tab = set.active();
    let empty_plan = plan(set.cycle_mode(tab, ViewId::BASE));
    assert_eq!(empty_plan.prompt, tabs::PromptScope::EmptyTab);
    let (_provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

    let fresh = match set.confirm_bypass(empty_plan.scope, empty_plan.nonce, ViewId::BASE) {
        Ok(ConfirmOutcome::Reprompt(fresh)) => fresh,
        other => panic!("expected a reprompt: {other:?}"),
    };
    assert_eq!(fresh.prompt, tabs::PromptScope::LiveTab);
    assert_eq!(
        set.confirm_bypass(fresh.scope, fresh.nonce, ViewId::BASE),
        Ok(ConfirmOutcome::Entered { approved: 0 })
    );
    assert_eq!(set.get(tab).unwrap().mode, SessionModeChoice::Bypass);
    assert_eq!(
        set.default_mode(),
        SessionModeChoice::Auto,
        "a live tab's entry moves only that tab"
    );
    shut_down_all(&mut set);
}

#[test]
fn reset_drops_an_open_bypass_prompt() {
    let mut set = set();
    let tab = set.active();
    set.get_mut(tab).unwrap().backend = TabBackend::Failed { reason: "test".into() };
    let default_plan = match set.cycle_default_mode(ViewId::BASE) {
        ModeCycle::Confirm(plan) => plan,
        other => panic!("auto -> bypass asks: {other:?}"),
    };
    set.reset(tab).unwrap();
    assert_eq!(
        set.confirm_bypass(default_plan.scope, default_plan.nonce, ViewId::BASE),
        Err("that prompt is no longer current".to_string())
    );
    assert_eq!(set.default_mode(), SessionModeChoice::Auto);
}

/// A launch must never start in bypass, even if a caller bypasses `agent_prefs::startup_mode`.
/// The tab set enforces this independently of the caller's startup-mode normalization.
#[test]
fn a_window_never_starts_in_bypass_whatever_it_is_handed() {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Bypass);
    assert_eq!(set.default_mode(), SessionModeChoice::Auto);
    assert_eq!(set.active_tab().mode(), SessionModeChoice::Auto);
    let next = set.open(AgentId::Claude);
    assert_eq!(set.get(next).unwrap().mode(), SessionModeChoice::Auto);
}

#[test]
fn reset_is_refused_while_a_session_runs_and_keeps_the_number_and_the_name() {
    let dir = workspace("tabs-reset");
    let mut set = set();
    let tab = set.active();
    set.rename(tab, "docs");
    assert!(set.reset(tab).is_err(), "an empty tab has nothing to reset");
    let (_provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    assert!(set.reset(tab).is_err(), "r is only offered on an ended or failed tab");
    set.get_mut(tab).unwrap().live_mut().unwrap().shutdown();
    until("the ended state", || {
        set.get(tab).unwrap().wire_state() == TabStateWire::Ended
    });
    let old = set
        .reset(tab)
        .unwrap()
        .expect("the ended backend comes back for shutdown");
    drop(old);
    let reset = set.get(tab).unwrap();
    assert!(matches!(reset.backend, TabBackend::NotStarted));
    assert_eq!((reset.number, reset.name.as_deref()), (1, Some("docs")));
    set.get_mut(tab).unwrap().backend = TabBackend::Failed {
        reason: "refused".into(),
    };
    assert!(
        matches!(set.reset(tab), Ok(None)),
        "a failed tab resets with no backend to shut down"
    );
}

#[test]
fn the_oldest_card_across_tabs_names_the_tab_that_holds_it() {
    let mut set = set();
    let first = set.active();
    let second = set.open(AgentId::Claude);
    set.get_mut(second).unwrap().attention.observe(&[write("older")], false);
    set.get_mut(first).unwrap().attention.observe(&[write("newer")], false);
    assert_eq!(set.oldest_card_tab(), Some(second));
    assert_eq!(set.newest_card_tab(), Some(first));
    assert_eq!(set.attention().pending, 2);
    set.select(first);
    set.mark_seen(first);
    assert!(!set.get(first).unwrap().attention.attention().unread);
}

#[test]
fn the_close_facts_say_what_closing_a_tab_involves() {
    let mut set = set();
    let tab = set.active();
    set.rename(tab, "docs");
    let facts = set.close_facts(tab).unwrap();
    assert_eq!(facts.number, 1);
    assert_eq!(facts.label_name, "docs");
    assert!(!facts.turn_running && !facts.has_backend && !facts.legacy);
    let legacy = TabSet::new(BackendKind::Legacy, SessionModeChoice::Auto);
    assert!(legacy.close_facts(legacy.active()).unwrap().legacy);
}

#[test]
fn close_others_plan_names_every_tab_but_the_active_one_and_is_none_alone() {
    let mut set = set();
    let active = set.active();
    assert_eq!(set.close_others_plan(), None, "the active tab is the only one open");
    let second = set.open(AgentId::Claude);
    let third = set.open(AgentId::Claude);
    set.select(active);
    let (ids, prompt) = set.close_others_plan().unwrap();
    assert_eq!(ids, vec![second, third]);
    assert_eq!(prompt, "close 2 other tabs? (y/n)");
}

#[test]
fn close_others_plan_leaves_out_a_tab_shown_in_an_extra_view() {
    let mut set = set();
    let active = set.active();
    let second = set.open(AgentId::Claude);
    let third = set.open(AgentId::Claude);
    set.select(third);
    let view = ViewId::new(2).unwrap();
    assert_eq!(set.break_out(view), Ok(third));
    set.select(active);
    let (ids, prompt) = set.close_others_plan().unwrap();
    assert_eq!(ids, vec![second], "the bound tab is not one of the base strip's others");
    assert_eq!(prompt, "close 1 other tab? (y/n)");
    set.remove(second);
    assert_eq!(
        set.close_others_plan(),
        None,
        "the active tab is the base view's only one"
    );
}

fn remote_origin() -> crate::remote_project::TabOrigin {
    crate::remote_project::TabOrigin::Remote {
        host: "example-host".into(),
        host_key_sha256: "SHA256:example".into(),
        root: "/srv/project".into(),
    }
}

/// Whether any `pendingPermissions` a payload carries names `id`.
fn draws(payloads: &[String], id: &str) -> bool {
    payloads.iter().any(|payload| {
        serde_json::from_str::<serde_json::Value>(payload).is_ok_and(|value| {
            value["state"]["pendingPermissions"]
                .as_array()
                .is_some_and(|pending| pending.iter().any(|p| p.to_string().contains(id)))
        })
    })
}

/// A remote tab's host answer on its way is being answered: no snapshot draws it as a card, no card
/// answer is taken for it, the bypass resync sweep does not answer it a second time, and the tick keeps
/// polling until its reply is read.
#[test]
fn a_remote_host_answer_in_flight_is_drawn_nowhere_and_not_swept() {
    let dir = workspace("remote-host-answer-in-flight");
    let mut set = set();
    set.set_project_origin(remote_origin()).unwrap();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let plan = plan(set.cycle_mode(tab, ViewId::BASE));
    set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();

    provider.hold_permission_replies();
    provider.queue(write("inflight-1"));
    until("the host answer on its way", || {
        set.pump(&dir, true);
        provider.held_permissions() == 1
    });
    assert!(set.needs_polling(), "the reply is read without a wake");
    assert!(
        !draws(&set.active_state_payloads(), "inflight-1"),
        "no card drawn for it"
    );
    assert!(!set.get(tab).unwrap().card_is_waiting("inflight-1"));
    assert!(set
        .answer_card_async(
            tab,
            ViewId::BASE,
            "inflight-1",
            agent::PermissionDecision::Deny { reason: None }
        )
        .is_err());

    // A resync in bypass: the sweep answers what is pending and not being answered.
    provider.resume_permission_replies();
    provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    for i in 0..300 {
        provider.queue(AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: agent::ContentKind::Text,
            text: format!("chunk {i}"),
        });
    }
    provider.queue(write("swept"));
    until("both pending in the projection", || {
        set.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .pending_permissions
            .len()
            == 2
    });
    let out = set.pump(&dir, true);
    let payload = out.active_payload.expect("the resync's snapshot");
    assert!(!draws(&[payload], "inflight-1"));
    let decided: Vec<String> = provider.decisions().into_iter().map(|(id, _)| id).collect();
    assert!(
        !decided.contains(&"inflight-1".to_string()),
        "the sweep did not answer it again: {decided:?}"
    );

    provider.reply_to_held_permissions(Ok(None));
    until("the reply read", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().session.host_answered.contains("inflight-1")
    });
    assert!(!set.get(tab).unwrap().session.human_allowed.has_host_sends());
    shut_down_all(&mut set);
}

/// A remote tab records a host answer when its reply is read. When the call finished before that, its
/// row note still goes out, with the next batch.
#[test]
fn a_remote_answers_row_note_reaches_a_call_that_finished_first() {
    let dir = workspace("remote-late-row-note");
    let mut set = set();
    set.set_project_origin(remote_origin()).unwrap();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let plan = plan(set.cycle_mode(tab, ViewId::BASE));
    set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();

    provider.hold_permission_replies();
    provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    provider.queue(cli_prompt(
        "prompt-1",
        "toolu_late",
        "Bash",
        serde_json::json!({ "command": "cat notes.txt" }),
        Some("cat:*"),
    ));
    until("the host answer on its way", || {
        set.pump(&dir, true);
        provider.held_permissions() == 1
    });
    provider.queue(AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_late".into(),
        name: "Bash".into(),
        input: serde_json::json!({ "command": "cat notes.txt" }),
    });
    provider.queue(AgentDomainEvent::ToolCallCompleted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_late".into(),
        content: serde_json::json!("ok"),
        is_error: false,
    });
    until("the call finished in the projection", || {
        set.pump(&dir, true);
        set.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .tool_calls
            .iter()
            .any(|call| call.tool_use_id == "toolu_late" && call.result.is_some())
    });
    provider.reply_to_held_permissions(Ok(None));
    provider.queue(AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: agent::ContentKind::Text,
        text: "after".into(),
    });
    until("the note on the finished call", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().notes.prompt_notes.contains_key("toolu_late")
    });
    shut_down_all(&mut set);
}

/// Queues a batch long enough to overflow the page's queue, so the next pump is a resync, ending with
/// `last`.
fn overflow_then(provider: &RecordingProvider, turn: &str, last: AgentDomainEvent) {
    for i in 0..300 {
        provider.queue(AgentDomainEvent::ContentDelta {
            turn_id: turn.into(),
            kind: agent::ContentKind::Text,
            text: format!("chunk {i}"),
        });
    }
    provider.queue(last);
}

/// A remote tab's bypass resync sweep answers through the host answers on their way: what it sent is
/// drawn nowhere and taken from no card while its reply is out, a second resync does not send it again,
/// and once taken it is recorded with the row note the CLI's own prompt gets in bypass.
#[test]
fn a_remote_bypass_sweep_is_tracked_until_its_reply() {
    let dir = workspace("remote-sweep-tracked");
    let mut set = set();
    set.set_project_origin(remote_origin()).unwrap();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let plan = plan(set.cycle_mode(tab, ViewId::BASE));
    set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();

    provider.hold_permission_replies();
    provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    overflow_then(
        &provider,
        "t1",
        cli_prompt(
            "swept",
            "toolu_swept",
            "Bash",
            serde_json::json!({ "command": "cat notes.txt" }),
            Some("cat:*"),
        ),
    );
    until("the request in the projection", || {
        set.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .pending_permissions
            .len()
            == 1
    });
    let out = set.pump(&dir, true);
    let payload = out.active_payload.expect("the resync's snapshot");
    assert_eq!(provider.held_permissions(), 1, "the sweep sent its answer");
    assert!(!draws(&[payload], "swept"), "the resync's snapshot does not draw it");
    assert!(!draws(&set.active_state_payloads(), "swept"));
    assert!(!set.get(tab).unwrap().card_is_waiting("swept"));
    assert!(set.needs_polling(), "the reply is read without a wake");
    assert!(set
        .answer_card_async(
            tab,
            ViewId::BASE,
            "swept",
            agent::PermissionDecision::Deny { reason: None }
        )
        .is_err());

    // A second resync while the reply is still out sweeps only what is new.
    overflow_then(&provider, "t1", write("later"));
    until("both pending in the projection", || {
        set.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .pending_permissions
            .len()
            == 2
    });
    let out = set.pump(&dir, true);
    assert!(out.active_payload.is_some(), "a second resync");
    assert_eq!(provider.held_permissions(), 2, "only the new request was sent");

    provider.reply_to_held_permission(0, Ok(None));
    provider.reply_to_held_permission(0, Ok(None));
    until("both replies read", || {
        set.pump(&dir, true);
        let session = &set.get(tab).unwrap().session;
        session.host_answered.contains("swept") && session.host_answered.contains("later")
    });
    assert!(!set.get(tab).unwrap().session.human_allowed.has_host_sends());
    provider.queue(AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_swept".into(),
        name: "Bash".into(),
        input: serde_json::json!({ "command": "cat notes.txt" }),
    });
    provider.queue(AgentDomainEvent::ToolCallCompleted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_swept".into(),
        content: serde_json::json!("ok"),
        is_error: false,
    });
    until("the bypass note on the swept prompt's call", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().notes.prompt_notes.contains_key("toolu_swept")
    });
    shut_down_all(&mut set);
}

/// Entering bypass on a remote tab answers its waiting cards the same tracked way: the card is drawn
/// nowhere and taken by no card answer while the reply is out, and a refused answer brings it back as
/// the person's card, which no bypass sweep answers again.
#[test]
fn a_remote_bypass_confirmation_is_tracked_until_its_reply() {
    let dir = workspace("remote-confirm-tracked");
    let mut set = set();
    set.set_project_origin(remote_origin()).unwrap();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(write("waiting"));
    until("the card", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 1
    });

    provider.hold_permission_replies();
    let plan = plan(set.cycle_mode(tab, ViewId::BASE));
    assert_eq!(plan.approve, vec!["waiting".to_string()]);
    assert_eq!(
        set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
        Ok(ConfirmOutcome::Entered { approved: 1 })
    );
    assert_eq!(
        set.get(tab).unwrap().attention.attention().pending,
        0,
        "the tray counts it no more"
    );
    assert_eq!(provider.held_permissions(), 1, "the confirmation sent its answer");
    assert!(!draws(&set.active_state_payloads(), "waiting"));
    assert!(!set.get(tab).unwrap().card_is_waiting("waiting"));
    assert!(set.needs_polling());
    assert!(set
        .answer_card_async(
            tab,
            ViewId::BASE,
            "waiting",
            agent::PermissionDecision::Deny { reason: None }
        )
        .is_err());

    provider.reply_to_held_permissions(Err(agent::ProviderError::Provider {
        code: agent::ProviderErrorCode::PermissionAlreadyResolved,
        message: "refused by the test".into(),
    }));
    until("the card back", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().card_is_waiting("waiting")
    });
    assert!(!set.get(tab).unwrap().session.host_answered.contains("waiting"));
    // It is the person's now: the resync that showed it, and the ones after, do not answer it again.
    for _ in 0..3 {
        set.pump(&dir, true);
    }
    assert_eq!(provider.held_permissions(), 0, "nothing sent again");
    assert!(set.get(tab).unwrap().card_is_waiting("waiting"));
    assert_eq!(set.get(tab).unwrap().attention.attention().pending, 1);
    shut_down_all(&mut set);
}

/// A remote Bypass tab whose host answer is lost in an outage (the link broke before the reply) shows its
/// card and sends nothing again while the link is down; once the link reads live again the answer goes once
/// more, tracked, and is recorded when taken.
#[test]
fn a_remote_bypass_answer_lost_in_an_outage_is_sent_once_after_the_reconnect() {
    let dir = workspace("remote-lost-answer");
    let mut set = set();
    set.set_project_origin(remote_origin()).unwrap();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    provider.set_link(Some(agent::RemoteLinkState::Live));
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let plan = plan(set.cycle_mode(tab, ViewId::BASE));
    set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();

    provider.hold_permission_replies();
    provider.queue(write("lost-1"));
    until("the host answer on its way", || {
        set.pump(&dir, true);
        provider.held_permissions() == 1
    });
    provider.set_link(Some(agent::RemoteLinkState::Disconnected { since_unix_ms: 1 }));
    provider.reply_to_held_permissions(Err(agent::ProviderError::Transport("the link broke".into())));
    until("the card shown while the link is down", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().card_is_waiting("lost-1")
    });
    for _ in 0..3 {
        set.pump(&dir, true);
    }
    assert_eq!(provider.held_permissions(), 0, "nothing sent while the link is down");

    provider.set_link(Some(agent::RemoteLinkState::Reconnecting {
        attempt: 2,
        next_in_ms: 1000,
    }));
    set.pump(&dir, true);
    assert_eq!(provider.held_permissions(), 0);
    provider.set_link(Some(agent::RemoteLinkState::Live));
    until("the answer sent again", || {
        set.pump(&dir, true);
        provider.held_permissions() == 1
    });
    for _ in 0..3 {
        set.pump(&dir, true);
    }
    assert_eq!(provider.held_permissions(), 1, "sent once");
    provider.reply_to_held_permissions(Ok(None));
    until("the reply read", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().session.host_answered.contains("lost-1")
    });
    assert!(!set.get(tab).unwrap().session.human_allowed.fell_back("lost-1"));
    shut_down_all(&mut set);
}

/// A remote Bypass tab whose host answer to `id` is lost while the link reads disconnected: the card is shown and
/// the answer kept to be sent again. The provider still holds replies.
fn lose_a_bypass_answer(label: &str, id: &str) -> (std::path::PathBuf, TabSet, TabId, Arc<RecordingProvider>) {
    let dir = workspace(label);
    let mut set = set();
    set.set_project_origin(remote_origin()).unwrap();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    provider.set_link(Some(agent::RemoteLinkState::Live));
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let plan = plan(set.cycle_mode(tab, ViewId::BASE));
    set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();
    provider.hold_permission_replies();
    provider.queue(write(id));
    until("the host answer on its way", || {
        set.pump(&dir, true);
        provider.held_permissions() == 1
    });
    provider.set_link(Some(agent::RemoteLinkState::Disconnected { since_unix_ms: 1 }));
    provider.reply_to_held_permissions(Err(agent::ProviderError::Transport("the link broke".into())));
    until("the card shown while the link is down", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().card_is_waiting(id)
    });
    (dir, set, tab, provider)
}

/// A lost bypass answer the agent took after all (its resolution comes once the link is back) is recorded as the
/// host's answer, as one whose reply came is: in the gate's log, and never sent again. One the agent resolved another
/// way is not.
#[test]
fn a_lost_answer_the_agent_took_is_recorded() {
    let (dir, mut set, tab, provider) = lose_a_bypass_answer("remote-lost-taken", "lost-t");
    provider.queue(AgentDomainEvent::PermissionResolved {
        permission_id: "lost-t".into(),
        outcome: agent::PermissionOutcome::Allowed,
    });
    // The resolution in the same batch forgets the id from `host_answered` at once; the gate's log keeps it.
    until("the answer recorded", || {
        set.pump(&dir, true);
        set.get(tab)
            .unwrap()
            .gate_answers()
            .iter()
            .any(|answer| answer.permission_id == "lost-t")
    });
    assert!(!set.get(tab).unwrap().card_is_waiting("lost-t"));
    provider.set_link(Some(agent::RemoteLinkState::Live));
    for _ in 0..3 {
        set.pump(&dir, true);
    }
    assert_eq!(provider.held_permissions(), 0, "nothing sent again");
    shut_down_all(&mut set);

    let (dir, mut set, tab, provider) = lose_a_bypass_answer("remote-lost-expired", "lost-e");
    provider.queue(AgentDomainEvent::PermissionResolved {
        permission_id: "lost-e".into(),
        outcome: agent::PermissionOutcome::Expired,
    });
    until("the card gone", || {
        set.pump(&dir, true);
        !set.get(tab).unwrap().card_is_waiting("lost-e")
    });
    for _ in 0..3 {
        set.pump(&dir, true);
    }
    assert!(!set
        .get(tab)
        .unwrap()
        .gate_answers()
        .iter()
        .any(|answer| answer.permission_id == "lost-e"));
    shut_down_all(&mut set);
}

/// The resolution of a lost answer the agent took can be folded while a drain runs: after the drain took its
/// events and before it looks at the projection again, which then no longer lists the request. That drain must
/// not drop the lost answer with its request, or the next drain, which delivers the resolution, has nothing to
/// record it against. The hook folds the resolution in exactly that gap, as a sidecar's ingestion thread can.
/// With the link still down the drain only prunes; with it back the drain also looks at resending the answer
/// and finds its request gone.
#[test]
fn a_resolution_folded_while_a_drain_runs_still_records_the_lost_answer() {
    resolution_folded_mid_drain("remote-lost-mid-drain", "lost-m", false);
}

#[test]
fn a_resolution_folded_while_a_drain_runs_with_the_link_back_still_records_the_lost_answer() {
    resolution_folded_mid_drain("remote-lost-mid-drain-live", "lost-l", true);
}

fn resolution_folded_mid_drain(label: &str, id: &'static str, link_back: bool) {
    let (dir, mut set, tab, provider) = lose_a_bypass_answer(label, id);
    if link_back {
        provider.set_link(Some(agent::RemoteLinkState::Live));
    }
    let folder = provider.clone();
    let folded = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = folded.clone();
    crate::agent_backend::drain_test_hooks::set_after_take(move |backend| {
        folder.queue(AgentDomainEvent::PermissionResolved {
            permission_id: id.into(),
            outcome: agent::PermissionOutcome::Allowed,
        });
        let deadline = Instant::now() + Duration::from_secs(60);
        while backend.projection().pending_permissions.contains_key(id) {
            assert!(Instant::now() < deadline, "the resolution was never folded");
            std::thread::sleep(Duration::from_millis(1));
        }
        seen.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    set.pump(&dir, true);
    assert!(folded.load(std::sync::atomic::Ordering::SeqCst), "the hooked drain ran");
    for _ in 0..3 {
        set.pump(&dir, true);
    }
    assert!(
        set.get(tab)
            .unwrap()
            .gate_answers()
            .iter()
            .any(|answer| answer.permission_id == id),
        "the lost answer the agent took is recorded"
    );
    assert!(!set.get(tab).unwrap().card_is_waiting(id));
    assert_eq!(provider.held_permissions(), 0, "nothing sent again");
    shut_down_all(&mut set);
}

/// A backend that reports a link to a server answers without waiting on this thread whatever its tab's origin says,
/// as `approve_pending` does: its answer is tracked until its reply, never waited for here.
#[test]
fn a_backend_with_a_link_answers_off_this_thread_under_any_origin() {
    let dir = workspace("linked-local-origin");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    provider.set_link(Some(agent::RemoteLinkState::Live));
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let plan = plan(set.cycle_mode(tab, ViewId::BASE));
    set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();
    provider.hold_permission_replies();
    provider.queue(write("linked"));
    until("the host answer on its way", || {
        set.pump(&dir, true);
        provider.held_permissions() == 1
    });
    assert!(
        set.get(tab).unwrap().session.human_allowed.has_host_sends(),
        "tracked until its reply, not waited for"
    );
    provider.reply_to_held_permissions(Ok(None));
    until("the reply read", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().session.host_answered.contains("linked")
    });
    shut_down_all(&mut set);
}

/// The person denied a card whose bypass answer was lost, and the deny failed too (the link was down): once the
/// link is back the lost Allow is not sent over it; the card stays the person's.
#[test]
fn a_lost_answer_is_never_sent_over_the_persons_answer() {
    let (dir, mut set, tab, provider) = lose_a_bypass_answer("remote-lost-then-denied", "lost-d");
    let send = set
        .answer_card_async(
            tab,
            ViewId::BASE,
            "lost-d",
            agent::PermissionDecision::Deny { reason: None },
        )
        .unwrap();
    provider.reply_to_held_permissions(Err(agent::ProviderError::Transport("still down".into())));
    let reply = loop {
        if let Some(reply) = set.finish_card_answer(tab, &send) {
            break reply;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    assert!(reply.is_err());
    provider.set_link(Some(agent::RemoteLinkState::Live));
    for _ in 0..4 {
        set.pump(&dir, true);
    }
    assert_eq!(provider.held_permissions(), 0, "the lost allow is not sent");
    assert!(set.get(tab).unwrap().card_is_waiting("lost-d"));
    assert!(set.get(tab).unwrap().session.human_allowed.fell_back("lost-d"));
    shut_down_all(&mut set);
}

/// A lost bypass answer is not sent once the person took the tab out of bypass while the link was down: the card
/// stays theirs.
#[test]
fn a_lost_answer_is_not_sent_in_a_mode_it_was_not_planned_under() {
    let (dir, mut set, tab, provider) = lose_a_bypass_answer("remote-lost-then-manual", "lost-m");
    assert_eq!(
        set.cycle_mode(tab, ViewId::BASE),
        Ok(ModeCycle::Changed(SessionModeChoice::Manual))
    );
    provider.set_link(Some(agent::RemoteLinkState::Live));
    for _ in 0..4 {
        set.pump(&dir, true);
    }
    assert_eq!(provider.held_permissions(), 0, "nothing is sent in manual");
    assert!(set.get(tab).unwrap().card_is_waiting("lost-m"));
    shut_down_all(&mut set);
}

/// The answer's reply comes back lost only after the link already reads live again (its call outlasted the
/// outage): it is sent once more at the next drain.
#[test]
fn a_loss_read_after_the_reconnect_is_sent_again() {
    let dir = workspace("remote-lost-after-reconnect");
    let mut set = set();
    set.set_project_origin(remote_origin()).unwrap();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    provider.set_link(Some(agent::RemoteLinkState::Live));
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let plan = plan(set.cycle_mode(tab, ViewId::BASE));
    set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();
    provider.hold_permission_replies();
    provider.queue(write("late"));
    until("the host answer on its way", || {
        set.pump(&dir, true);
        provider.held_permissions() == 1
    });
    provider.set_link(Some(agent::RemoteLinkState::Disconnected { since_unix_ms: 1 }));
    set.pump(&dir, true);
    set.pump(&dir, true);
    provider.set_link(Some(agent::RemoteLinkState::Live));
    set.pump(&dir, true);
    provider.reply_to_held_permissions(Err(agent::ProviderError::Transport("the link broke".into())));
    until("the answer sent again", || {
        set.pump(&dir, true);
        provider.held_permissions() == 1
    });
    shut_down_all(&mut set);
}

/// No reply in time while the link never went down: the answer is sent once more at the next drain, and lost a
/// second time it is not sent a third time; its card stays.
#[test]
fn a_timeout_with_the_link_live_is_sent_once_more_and_then_left_to_the_person() {
    let dir = workspace("remote-timeout-live");
    let mut set = set();
    set.set_project_origin(remote_origin()).unwrap();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    provider.set_link(Some(agent::RemoteLinkState::Live));
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let plan = plan(set.cycle_mode(tab, ViewId::BASE));
    set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();
    provider.hold_permission_replies();
    provider.queue(write("slow"));
    until("the host answer on its way", || {
        set.pump(&dir, true);
        provider.held_permissions() == 1
    });
    provider.reply_to_held_permissions(Err(agent::ProviderError::Timeout));
    until("the answer sent again", || {
        set.pump(&dir, true);
        provider.held_permissions() == 1
    });
    provider.reply_to_held_permissions(Err(agent::ProviderError::Timeout));
    until("the card back", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().card_is_waiting("slow")
    });
    for _ in 0..4 {
        set.pump(&dir, true);
    }
    assert_eq!(provider.held_permissions(), 0, "not sent a third time");
    assert!(set.get(tab).unwrap().card_is_waiting("slow"));
    shut_down_all(&mut set);
}

/// Two answers lost in turn, the first during an outage the drains saw and the second timing out after the
/// reconnect: each is sent once more.
#[test]
fn a_second_loss_does_not_strand_the_first() {
    let dir = workspace("remote-two-losses");
    let mut set = set();
    set.set_project_origin(remote_origin()).unwrap();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    provider.set_link(Some(agent::RemoteLinkState::Live));
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let plan = plan(set.cycle_mode(tab, ViewId::BASE));
    set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();
    provider.hold_permission_replies();
    provider.queue(write("first"));
    provider.queue(write("second"));
    until("both host answers on their way", || {
        set.pump(&dir, true);
        provider.held_permissions() == 2
    });
    provider.set_link(Some(agent::RemoteLinkState::Disconnected { since_unix_ms: 1 }));
    provider.reply_to_held_permission(0, Err(agent::ProviderError::Transport("the link broke".into())));
    until("the first card", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().card_is_waiting("first")
    });
    provider.set_link(Some(agent::RemoteLinkState::Live));
    provider.reply_to_held_permission(0, Err(agent::ProviderError::Timeout));
    until("both sent again", || {
        set.pump(&dir, true);
        provider.held_permissions() == 2
    });
    shut_down_all(&mut set);
}

#[test]
fn a_lost_host_allow_is_recorded_when_the_person_denied_meanwhile() {
    for reply_read in [false, true] {
        let (dir, mut set, tab, provider) = lose_a_bypass_answer("lost-host-person-deny", "lost-person");
        let send = set
            .answer_card_async(
                tab,
                ViewId::BASE,
                "lost-person",
                agent::PermissionDecision::Deny { reason: None },
            )
            .unwrap();
        if reply_read {
            provider.reply_to_held_permissions(Err(agent::ProviderError::Timeout));
            until("the person's reply", || set.finish_card_answer(tab, &send).is_some());
        }
        provider.queue(AgentDomainEvent::PermissionResolved {
            permission_id: "lost-person".into(),
            outcome: agent::PermissionOutcome::Allowed,
        });
        until("the host answer recorded", || {
            set.pump(&dir, true);
            set.get(tab)
                .unwrap()
                .gate_answers()
                .iter()
                .any(|a| a.permission_id == "lost-person")
        });
        for _ in 0..3 {
            set.pump(&dir, true);
        }
        assert_eq!(
            set.get(tab)
                .unwrap()
                .gate_answers()
                .iter()
                .filter(|a| a.permission_id == "lost-person")
                .count(),
            1
        );
        if !reply_read {
            provider.reply_to_held_permissions(Err(agent::ProviderError::Timeout));
            until("the person's late reply", || {
                set.finish_card_answer(tab, &send).is_some()
            });
        }
        assert_eq!(provider.held_permissions(), 0);
        shut_down_all(&mut set);
    }
}

#[test]
fn a_taken_host_retry_with_a_lost_reply_is_recorded_once() {
    for (resolution_first, already_resolved) in [(false, false), (true, false), (false, true), (true, true)] {
        let (dir, mut set, tab, provider) = lose_a_bypass_answer("lost-retry-taken", "lost-retry");
        provider.set_link(Some(agent::RemoteLinkState::Live));
        until("the retry sent", || {
            set.pump(&dir, true);
            provider.held_permissions() == 1
        });
        let resolved = AgentDomainEvent::PermissionResolved {
            permission_id: "lost-retry".into(),
            outcome: agent::PermissionOutcome::Allowed,
        };
        if resolution_first {
            provider.queue(resolved.clone());
            set.pump(&dir, true);
        }
        let error = if already_resolved {
            agent::ProviderError::Provider {
                code: agent::ProviderErrorCode::PermissionAlreadyResolved,
                message: "the earlier answer was taken".into(),
            }
        } else {
            agent::ProviderError::Timeout
        };
        provider.reply_to_held_permissions(Err(error));
        until("the retry reply read", || {
            set.pump(&dir, true);
            !set.get(tab).unwrap().session.human_allowed.has_host_sends()
        });
        if !resolution_first {
            provider.queue(resolved);
        }
        until("the resolution folded", || {
            set.pump(&dir, true);
            !set.get(tab).unwrap().card_is_waiting("lost-retry")
        });
        for _ in 0..3 {
            set.pump(&dir, true);
        }
        assert_eq!(
            set.get(tab)
                .unwrap()
                .gate_answers()
                .iter()
                .filter(|a| a.permission_id == "lost-retry")
                .count(),
            1
        );
        assert_eq!(provider.held_permissions(), 0);
        shut_down_all(&mut set);
    }
}
