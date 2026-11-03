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
