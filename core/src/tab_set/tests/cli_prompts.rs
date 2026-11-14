//! The CLI's own permission prompts through the tab set.
#![cfg(test)]

use super::*;

#[test]
fn a_followup_waits_for_the_async_approval_and_is_reconsidered_once() {
    followup_before_reply(Ok(None));
}

#[test]
fn a_followup_is_a_card_when_the_async_approval_fails() {
    followup_before_reply(Err(agent::ProviderError::Timeout));
    followup_before_reply(Err(agent::ProviderError::Transport("lost".into())));
}

fn followup_before_reply(reply: agent::InteractionReply) {
    let succeeds = reply.is_ok();
    let dir = workspace("early-followup");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
    provider.queue(started("t1"));
    provider.queue(gate_write("hook", "call"));
    until("the gate card", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 1
    });
    provider.hold_permission_replies();
    let send = set
        .answer_card_async(tab, ViewId::BASE, "hook", agent::PermissionDecision::Allow)
        .unwrap();
    provider.queue(resolved("hook"));
    let prompt = || {
        cli_prompt(
            "followup",
            "call",
            "Write",
            serde_json::json!({ "file_path": ".git/probe2", "content": "o3" }),
            None,
        )
    };
    provider.queue(prompt());
    until("the follow-up folded before the acknowledgement", || {
        set.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .pending_permissions
            .contains_key("followup")
    });
    let key = set.get(tab).unwrap().row_key().clone();
    let observer = set.observe(&key).unwrap();
    let frames = set.take_observer_frames(observer, usize::MAX);
    assert!(matches!(&frames[0], ObserverFrame::Snapshot { json, .. } if !json.contains("followup")));
    let out = set.pump(&dir, true);
    assert!(!out.active_payload.as_deref().unwrap_or("").contains("followup"));
    assert_eq!(set.get(tab).unwrap().attention.attention().pending, 0);
    assert!(set.get(tab).unwrap().session.human_allowed.is_empty());
    assert!(
        !set.active_state_payloads()[0].contains("followup"),
        "reloads hide the held prompt too"
    );
    // A resync must keep the in-flight approval and hide the follow-up.
    for i in 0..300 {
        provider.queue(AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: agent::ContentKind::Text,
            text: format!("chunk {i}"),
        });
    }
    until("the overflow folded", || {
        set.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .transcript
            .last()
            .is_some_and(|m| m.text.ends_with("chunk 299"))
    });
    let resync: serde_json::Value = serde_json::from_str(&set.pump(&dir, true).active_payload.unwrap()).unwrap();
    assert_eq!(resync["kind"], "snapshot");
    assert!(!resync.to_string().contains("followup"));
    assert_eq!(set.get(tab).unwrap().attention.attention().pending, 0);
    let frames = set.take_observer_frames(observer, usize::MAX);
    assert!(matches!(&frames[0], ObserverFrame::Snapshot { json, .. } if !json.contains("followup")));
    provider.reply_to_held_permissions(reply);
    provider.resume_permission_replies();
    assert_eq!(set.finish_card_answer(tab, &send).unwrap().is_ok(), succeeds);
    until("the held follow-up settled", || {
        set.pump(&dir, true);
        if succeeds {
            provider.resolutions().len() == 1
        } else {
            set.get(tab).unwrap().attention.attention().pending == 1
        }
    });
    if succeeds {
        assert_eq!(provider.resolutions(), vec![("followup".into(), true)]);
        let revision = set.get(tab).unwrap().live().unwrap().projection().last_revision;
        provider.queue(prompt());
        until("the duplicate folded", || {
            set.get(tab).unwrap().live().unwrap().projection().last_revision > revision
        });
        set.pump(&dir, true);
        assert_eq!(provider.resolutions().len(), 1);
        assert_eq!(set.get(tab).unwrap().attention.attention().pending, 0);
    } else {
        assert!(set.active_state_payloads()[0].contains("followup"));
        assert!(set.get(tab).unwrap().card_is_waiting("followup"));
    }
    shut_down_all(&mut set);
}

fn probe_input() -> serde_json::Value {
    serde_json::json!({ "file_path": ".git/probe2", "content": "o3" })
}

/// The ids of the cards a snapshot payload lists, sorted.
fn cards_in(payload: &str) -> Vec<String> {
    let snapshot: serde_json::Value = serde_json::from_str(payload).unwrap();
    assert_eq!(snapshot["kind"], "snapshot", "{payload}");
    let mut cards: Vec<String> = snapshot["state"]["pendingPermissions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|card| card["permissionId"].as_str().unwrap().to_string())
        .collect();
    cards.sort();
    cards
}

fn pending(set: &TabSet, tab: TabId, permission: &str) -> bool {
    set.get(tab)
        .unwrap()
        .live()
        .unwrap()
        .projection()
        .pending_permissions
        .contains_key(permission)
}

/// A CLI prompt that is itself the card being approved is not held by its own send: it stays listed
/// (the page shows it as sending), and an acknowledgement read before its resolution folds leads to
/// no second answer.
#[test]
fn an_approved_cli_prompt_card_is_answered_once_and_stays_shown_while_sending() {
    let dir = workspace("cli-prompt-card-async");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
    provider.queue(started("t1"));
    provider.queue(cli_prompt("prompt", "call", "Write", probe_input(), None));
    until("the prompt's card", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 1
    });
    provider.hold_permission_replies();
    let send = set
        .answer_card_async(tab, ViewId::BASE, "prompt", agent::PermissionDecision::Allow)
        .unwrap();
    assert_eq!(cards_in(&set.active_state_payloads()[0]), vec!["prompt"]);
    set.pump(&dir, true);
    assert_eq!(
        cards_in(&set.active_state_payloads()[0]),
        vec!["prompt"],
        "its own send does not hide it"
    );
    provider.reply_to_held_permissions(Ok(None));
    provider.resume_permission_replies();
    assert!(set.finish_card_answer(tab, &send).unwrap().is_ok());
    for _ in 0..3 {
        set.pump(&dir, true);
    }
    assert!(pending(&set, tab, "prompt"), "its resolution has not folded yet");
    assert_eq!(
        provider.resolutions(),
        Vec::new(),
        "the answer is not sent a second time"
    );
    provider.queue(resolved("prompt"));
    until("the resolution folded", || {
        set.pump(&dir, true);
        !pending(&set, tab, "prompt")
    });
    set.pump(&dir, true);
    assert_eq!(provider.resolutions(), Vec::new());
    assert!(cards_in(&set.active_state_payloads()[0]).is_empty());
    assert_eq!(set.get(tab).unwrap().attention.attention().pending, 0);
    shut_down_all(&mut set);
}

/// A follow-up a snapshot hid before any drain read it is released like a drained one: when the
/// approval fails, the next pump shows it in a snapshot.
#[test]
fn a_followup_hidden_by_a_snapshot_before_any_drain_is_shown_when_the_approval_fails() {
    let dir = workspace("snapshot-hidden-followup");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
    provider.queue(started("t1"));
    provider.queue(gate_write("hook", "call"));
    until("the gate card", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 1
    });
    provider.hold_permission_replies();
    let send = set
        .answer_card_async(tab, ViewId::BASE, "hook", agent::PermissionDecision::Allow)
        .unwrap();
    provider.queue(cli_prompt("followup", "call", "Write", probe_input(), None));
    until("the follow-up folded", || pending(&set, tab, "followup"));
    // No pump in between: this snapshot hides the follow-up and covers its event.
    assert_eq!(cards_in(&set.active_state_payloads()[0]), vec!["hook"]);
    provider.reply_to_held_permissions(Err(agent::ProviderError::Timeout));
    provider.resume_permission_replies();
    assert!(set.finish_card_answer(tab, &send).unwrap().is_err());
    let out = set.pump(&dir, true);
    let payload = out.active_payload.expect("the released follow-up is sent");
    assert_eq!(cards_in(&payload), vec!["followup", "hook"]);
    assert_eq!(set.get(tab).unwrap().attention.attention().pending, 2);
    assert!(set.get(tab).unwrap().card_is_waiting("followup"));
    assert_eq!(provider.resolutions(), Vec::new());
    shut_down_all(&mut set);
}

/// Two approvals in flight at once: releasing the first one's follow-up, in a batch with other
/// events, keeps the second one's recorded approval, which then answers its own follow-up.
#[test]
fn releasing_one_followup_keeps_another_calls_recorded_approval() {
    let dir = workspace("parallel-approvals");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
    provider.queue(started("t1"));
    provider.queue(gate_write("gate-a", "call-a"));
    provider.queue(gate_write("gate-b", "call-b"));
    until("both gate cards", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 2
    });
    provider.hold_permission_replies();
    let send_a = set
        .answer_card_async(tab, ViewId::BASE, "gate-a", agent::PermissionDecision::Allow)
        .unwrap();
    let send_b = set
        .answer_card_async(tab, ViewId::BASE, "gate-b", agent::PermissionDecision::Allow)
        .unwrap();
    assert_eq!(provider.held_permissions(), 2);
    provider.queue(resolved("gate-a"));
    provider.queue(resolved("gate-b"));
    provider.queue(cli_prompt("followup-a", "call-a", "Write", probe_input(), None));
    until("the first follow-up folded", || pending(&set, tab, "followup-a"));
    set.pump(&dir, true);
    assert_eq!(set.get(tab).unwrap().attention.attention().pending, 0);
    provider.reply_to_held_permission(1, Ok(None));
    assert!(set.finish_card_answer(tab, &send_b).unwrap().is_ok());
    provider.queue(AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: agent::ContentKind::Text,
        text: "in the same batch".into(),
    });
    until("the text folded", || {
        set.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .transcript
            .last()
            .is_some_and(|m| m.text.ends_with("in the same batch"))
    });
    provider.reply_to_held_permission(0, Ok(None));
    provider.resume_permission_replies();
    assert!(set.finish_card_answer(tab, &send_a).unwrap().is_ok());
    set.pump(&dir, true);
    assert_eq!(provider.resolutions(), vec![("followup-a".to_string(), true)]);
    provider.queue(cli_prompt("followup-b", "call-b", "Write", probe_input(), None));
    until("the second follow-up settled", || {
        set.pump(&dir, true);
        provider.resolutions().len() == 2 || set.get(tab).unwrap().attention.attention().pending > 0
    });
    assert_eq!(
        provider.resolutions().get(1),
        Some(&("followup-b".to_string(), true)),
        "the second call's approval answered its follow-up"
    );
    assert_eq!(set.get(tab).unwrap().attention.attention().pending, 0);
    shut_down_all(&mut set);
}

/// A follow-up held for an approval in flight is no card, so a resync keeps the gate note of its call
/// that a pending card would drop.
#[test]
fn a_resync_keeps_a_gate_note_whose_only_pending_request_is_held() {
    let dir = workspace("held-keeps-note");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
    provider.queue(started("t1"));
    provider.queue(gate_write("hook", "call"));
    until("the gate card", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 1
    });
    provider.hold_permission_replies();
    let send = set
        .answer_card_async(tab, ViewId::BASE, "hook", agent::PermissionDecision::Allow)
        .unwrap();
    provider.queue(resolved("hook"));
    provider.queue(cli_prompt("followup", "call", "Write", probe_input(), None));
    until("the follow-up folded", || {
        pending(&set, tab, "followup") && !pending(&set, tab, "hook")
    });
    set.pump(&dir, true);
    set.get_mut(tab).unwrap().session.note_ledger.record_report(NoteReport {
        by_the_fast_path: vec![crate::agent_backend::FastPathAnsweredForYou {
            tool_use_id: "call".into(),
            tool_name: "Write".into(),
            input: probe_input(),
        }],
        ..NoteReport::default()
    });
    for i in 0..300 {
        provider.queue(AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: agent::ContentKind::Text,
            text: format!("chunk {i}"),
        });
    }
    until("the overflow folded", || {
        set.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .transcript
            .last()
            .is_some_and(|m| m.text.ends_with("chunk 299"))
    });
    let resync: serde_json::Value = serde_json::from_str(&set.pump(&dir, true).active_payload.unwrap()).unwrap();
    assert_eq!(resync["kind"], "snapshot");
    assert!(
        set.get(tab)
            .unwrap()
            .session
            .note_ledger
            .has("call", NoteKind::AutoEdit),
        "the held follow-up is no card"
    );
    provider.reply_to_held_permissions(Ok(None));
    provider.resume_permission_replies();
    assert!(set.finish_card_answer(tab, &send).unwrap().is_ok());
    shut_down_all(&mut set);
}

/// The gate card, human approval through `answer_card`, and gate resolution can be followed by
/// the CLI's prompt for the same call under a new permission id. That request is accepted and
/// answered `allow` using the prior human approval, producing exactly one card for the call.
/// In a remote tab the host's answer goes out without waiting, and the person's one approval of a call is used up
/// by the first prompt it answers, there and then: a second prompt for the same call in the same batch is a card.
#[test]
fn a_remote_tabs_one_approval_answers_one_prompt_while_its_answer_is_out() {
    let dir = workspace("remote-one-approval");
    let mut set = set();
    set.set_project_origin(crate::remote_project::TabOrigin::Remote {
        host: "example-host".into(),
        host_key_sha256: "SHA256:example".into(),
        root: "/srv/project".into(),
    })
    .unwrap();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    provider.set_link(Some(agent::RemoteLinkState::Live));
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(gate_write("perm-hook", "toolu_probe2"));
    until("the gate's card", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 1
    });
    let send = set
        .answer_card_async(tab, ViewId::BASE, "perm-hook", agent::PermissionDecision::Allow)
        .unwrap();
    let reply = loop {
        if let Some(reply) = set.finish_card_answer(tab, &send) {
            break reply;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(reply.is_ok());
    provider.hold_permission_replies();
    provider.queue_all(vec![
        resolved("perm-hook"),
        cli_prompt("perm-cli-1", "toolu_probe2", "Write", probe_input(), None),
        cli_prompt("perm-cli-2", "toolu_probe2", "Write", probe_input(), None),
    ]);
    until("one prompt answered", || {
        set.pump(&dir, true);
        provider.held_permissions() >= 1
    });
    for _ in 0..3 {
        set.pump(&dir, true);
    }
    assert_eq!(provider.held_permissions(), 1, "one approval, one answer");
    assert!(
        set.get(tab).unwrap().card_is_waiting("perm-cli-2"),
        "the second is the person's card"
    );
    provider.reply_to_held_permissions(Ok(None));
    shut_down_all(&mut set);
}

#[test]
fn approving_the_card_for_a_call_answers_the_clis_own_prompt_that_follows() {
    let dir = workspace("o3-auto-one-card");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

    provider.queue(gate_write("perm-hook", "toolu_probe2"));
    until("the gate's card", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 1
    });
    set.answer_card(tab, ViewId::BASE, "perm-hook", agent::PermissionDecision::Allow)
        .map_err(|e| e.message)
        .unwrap();
    assert_eq!(provider.resolutions(), vec![("perm-hook".to_string(), true)]);

    provider.queue(resolved("perm-hook"));
    provider.queue(cli_prompt(
        "perm-cli",
        "toolu_probe2",
        "Write",
        serde_json::json!({ "file_path": ".git/probe2", "content": "o3" }),
        None,
    ));
    until("the CLI's own prompt answered", || {
        if let Some(payload) = set.pump(&dir, true).active_payload {
            assert!(!payload.contains("perm-cli"), "never drawn: {payload}");
        }
        provider.resolutions().len() == 2
    });
    assert_eq!(provider.resolutions()[1], ("perm-cli".to_string(), true));
    let attention = set.get(tab).unwrap().attention.attention();
    assert_eq!(attention.pending, 0);
    assert_eq!(attention.arrived, 1, "exactly one card for the call");
    shut_down_all(&mut set);
}

#[test]
fn async_permission_refusals_keep_provider_words_and_transport_uncertainty() {
    for (error, expected) in [
        (
            agent::ProviderError::Provider {
                code: agent::ProviderErrorCode::PermissionAlreadyResolved,
                message: "This request was already resolved.".into(),
            },
            "This request was already resolved.".to_string(),
        ),
        (agent::ProviderError::Timeout, String::new()),
        (agent::ProviderError::Transport("connection lost".into()), String::new()),
    ] {
        let dir = workspace("async-permission-refusal");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
        provider.queue(write("permission"));
        until("the card", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().card_is_waiting("permission")
        });
        provider.hold_permission_replies();
        let send = set
            .answer_card_async(tab, ViewId::BASE, "permission", agent::PermissionDecision::Allow)
            .unwrap();
        assert!(!set.get(tab).unwrap().card_is_waiting("permission"));
        let expected = if expected.is_empty() {
            format!("the answer may not have reached the agent ({error}); the request is waiting again")
        } else {
            expected
        };
        provider.reply_to_held_permissions(Err(error));
        let refusal = set.finish_card_answer(tab, &send).unwrap().unwrap_err();
        assert_eq!(refusal.message, expected);
        assert!(refusal.benign);
        assert!(set.get(tab).unwrap().card_is_waiting("permission"));
        shut_down_all(&mut set);
    }
}

#[test]
fn an_async_approval_is_recorded_only_after_success_in_its_original_turn() {
    let dir = workspace("async-human-approval");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
    for (permission, turn, newer) in [("first", "t1", false), ("second", "t2", true)] {
        provider.queue(started(turn));
        provider.queue(gate_write(permission, "call"));
        until("the card", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().card_is_waiting(permission)
        });
        provider.hold_permission_replies();
        let send = set
            .answer_card_async(tab, ViewId::BASE, permission, agent::PermissionDecision::Allow)
            .unwrap();
        assert!(set.finish_card_answer(tab, &send).is_none());
        assert!(set.get(tab).unwrap().session.human_allowed.is_empty());
        if newer {
            provider.queue(started("t3"));
            until("the newer turn", || {
                set.pump(&dir, true);
                set.get(tab)
                    .unwrap()
                    .live()
                    .unwrap()
                    .projection()
                    .active_turn_id
                    .as_deref()
                    == Some("t3")
            });
        }
        provider.reply_to_held_permissions(Ok(None));
        set.finish_card_answer(tab, &send).unwrap().unwrap();
        assert_eq!(set.get(tab).unwrap().session.human_allowed.is_empty(), newer);
        set.get_mut(tab).unwrap().session.human_allowed.clear();
    }
    shut_down_all(&mut set);
}

/// Only an Approve counts: a denied card, and a card answered behind the tab set's back (not
/// through `answer_card`), leave the CLI's own prompt for that call a card.
#[test]
fn only_an_approve_through_the_tab_set_answers_the_clis_own_prompt() {
    let dir = workspace("o3-auto-deny");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

    provider.queue(gate_write("perm-denied", "toolu_a"));
    provider.queue(gate_write("perm-direct", "toolu_b"));
    until("both gate cards", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 2
    });
    set.answer_card(
        tab,
        ViewId::BASE,
        "perm-denied",
        agent::PermissionDecision::Deny {
            reason: Some("no".into()),
        },
    )
    .map_err(|e| e.message)
    .unwrap();
    set.get_mut(tab)
        .unwrap()
        .live_mut()
        .unwrap()
        .respond_permission("perm-direct", agent::PermissionDecision::Allow)
        .map_err(|e| e.message)
        .unwrap();
    assert!(set.get(tab).unwrap().session.human_allowed.is_empty());

    provider.queue(resolved("perm-denied"));
    provider.queue(resolved("perm-direct"));
    let input = serde_json::json!({ "file_path": ".git/probe2", "content": "o3" });
    provider.queue(cli_prompt("perm-cli-a", "toolu_a", "Write", input.clone(), None));
    provider.queue(cli_prompt("perm-cli-b", "toolu_b", "Write", input, None));
    until("both CLI prompts card", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 2
    });
    assert_eq!(provider.resolutions().len(), 2, "{:?}", provider.resolutions());
    shut_down_all(&mut set);
}

/// The set of approved calls lives for the turn: the CLI asks within the turn that made the call,
/// so it is emptied when the turn ends rather than growing for the life of the tab.
#[test]
fn approved_calls_are_forgotten_when_the_turn_ends() {
    let dir = workspace("o3-auto-turn-end");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    provider.queue(gate_write("perm-hook", "toolu_c"));
    until("the gate's card", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 1
    });
    set.answer_card(tab, ViewId::BASE, "perm-hook", agent::PermissionDecision::Allow)
        .map_err(|e| e.message)
        .unwrap();
    assert!(set.get(tab).unwrap().session.human_allowed.contains("toolu_c"));
    provider.queue(resolved("perm-hook"));
    provider.queue(AgentDomainEvent::TurnCompleted {
        turn_id: "t1".into(),
        outcome: agent::TurnOutcome::Completed,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
        detail: Default::default(),
    });
    until("the turn ends", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().session.human_allowed.is_empty()
    });
    shut_down_all(&mut set);
}

/// A completion that came with no turn of the backend's own open (Claude Code ran a turn of its own)
/// ends none of the panel's: the calls approved in the running turn stay approved, so the CLI's own
/// prompt for one of them still needs no second card.
#[test]
fn approved_calls_outlive_a_completion_of_no_open_turn() {
    let dir = workspace("o3-auto-foreign-end");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    provider.queue(gate_write("perm-hook", "toolu_c"));
    until("the gate's card", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 1
    });
    set.answer_card(tab, ViewId::BASE, "perm-hook", agent::PermissionDecision::Allow)
        .map_err(|e| e.message)
        .unwrap();
    provider.queue(resolved("perm-hook"));
    provider.queue(AgentDomainEvent::TurnCompleted {
        turn_id: agent::NO_OPEN_TURN_ID.into(),
        outcome: agent::TurnOutcome::Completed,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
        detail: Default::default(),
    });
    // A later card shows the completion before it was pumped.
    provider.queue(gate_write("perm-later", "toolu_d"));
    until("the later card", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.card_ids() == ["perm-later"]
    });
    let t = set.get(tab).unwrap();
    assert!(t.turn_running(), "the panel's turn runs on");
    assert!(t.session.human_allowed.contains("toolu_c"), "and so does its approval");
    shut_down_all(&mut set);
}

/// In bypass the CLI's prompt is answered without being drawn as a card.
#[test]
fn in_bypass_the_clis_own_prompt_is_answered_and_never_delivered() {
    let dir = workspace("o3-bypass");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let plan = plan(set.cycle_mode(tab, ViewId::BASE));
    assert_eq!(
        set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
        Ok(ConfirmOutcome::Entered { approved: 0 })
    );
    provider.queue(cli_prompt(
        "perm-cli",
        "toolu_d",
        "Write",
        serde_json::json!({ "file_path": ".claude/probe.json", "content": "{}" }),
        None,
    ));
    until("answered", || {
        if let Some(payload) = set.pump(&dir, true).active_payload {
            assert!(!payload.contains("perm-cli"), "{payload}");
        }
        !provider.resolutions().is_empty()
    });
    assert_eq!(provider.resolutions(), vec![("perm-cli".to_string(), true)]);
    assert_eq!(set.get(tab).unwrap().attention.attention().pending, 0);
    shut_down_all(&mut set);
}

/// A waiting card is not copied by a tick that finds it unchanged: the offers are worked out again
/// only when the set of cards changed. A 1 MiB card used to be copied on every tick (30 a second).
#[test]
fn an_unchanged_card_is_not_copied_by_a_tick() {
    let dir = workspace("offers-no-copy");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    // A pipe makes it a card whatever the command, and the card is the size of a large edit.
    let command = format!("echo {} | wc -c", "x".repeat(1 << 20));
    provider.queue(AgentDomainEvent::PermissionRequested {
        permission_id: "perm-big".into(),
        tool_use_id: None,
        tool_name: "Bash".into(),
        input: serde_json::json!({ "command": command }),
        provider_prompt: None,
    });
    until("the card", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 1
    });
    set.pump(&dir, true);
    let (_, bytes) = crate::alloc_count::bytes_allocated_by(|| set.pump(&dir, true));
    assert!(bytes < 64 * 1024, "one quiet tick allocated {bytes} bytes");
    shut_down_all(&mut set);
}

/// A saved rule cannot silence the CLI's prompt, so that card never offers "Always allow".
/// The gate card for the same command still offers a rule.
#[test]
fn the_clis_own_prompt_is_never_offered_a_rule() {
    let dir = workspace("o3-no-offer");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let npm = serde_json::json!({ "command": "npm ci" });
    provider.queue(cli_prompt("perm-cli", "toolu_e", "Bash", npm.clone(), None));
    provider.queue(AgentDomainEvent::PermissionRequested {
        permission_id: "perm-gate".into(),
        tool_use_id: Some("toolu_f".into()),
        tool_name: "Bash".into(),
        input: npm,
        provider_prompt: None,
    });
    until("both cards", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 2
    });
    let offers = &set.get(tab).unwrap().session.rule_offers;
    assert!(offers.contains_key("perm-gate"), "{offers:?}");
    assert!(!offers.contains_key("perm-cli"), "{offers:?}");
    shut_down_all(&mut set);
}

/// Entering bypass counts and approves every waiting card: the gate's request and CLI prompts
/// forced by an ask rule, of unknown kind, without a reason, describing only a subject or path,
/// or carrying a reason. They are listed in `seq` order and approved by `y`.
/// The prompt needs only one line because no card remains waiting. Completed calls whose CLI
/// prompt the entry approved carry the same bypass note as calls answered in a bypass tab.
#[test]
fn entering_bypass_approves_every_waiting_card_the_clis_own_prompts_included() {
    let dir = workspace("bypass-entry-every-prompt");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let changed = |id: &str, tool_use_id: &str, change: &dyn Fn(&mut agent::ProviderPrompt)| {
        let mut event = cli_prompt(
            id,
            tool_use_id,
            "Bash",
            serde_json::json!({ "command": format!("echo {id}") }),
            None,
        );
        if let AgentDomainEvent::PermissionRequested { provider_prompt, .. } = &mut event {
            change(provider_prompt.as_mut().unwrap());
        }
        event
    };
    provider.queue(started("t1"));
    provider.queue(call_started(
        "toolu_g",
        "Bash",
        serde_json::json!({ "command": "cat notes.txt" }),
    ));
    provider.queue(cli_prompt(
        "perm-ask",
        "toolu_g",
        "Bash",
        serde_json::json!({ "command": "cat notes.txt" }),
        Some("cat:*"),
    ));
    provider.queue(changed("perm-unknown", "toolu_u", &|p| p.unrecognized_origin = Some(7)));
    provider.queue(changed("perm-silent", "toolu_s", &|p| {
        p.reason = None;
        p.description = Some("Print the word zebra".into());
    }));
    provider.queue(changed("perm-path", "toolu_p", &|p| {
        p.reason = None;
        p.blocked_path = Some("/p/.git/x".into());
    }));
    provider.queue(changed("perm-reason", "toolu_r", &|_| {}));
    provider.queue(write("perm-write"));
    let all: Vec<String> = [
        "perm-ask",
        "perm-unknown",
        "perm-silent",
        "perm-path",
        "perm-reason",
        "perm-write",
    ]
    .map(str::to_string)
    .to_vec();
    until("six cards", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 6
    });
    let plan = plan(set.cycle_mode(tab, ViewId::BASE));
    assert_eq!(plan.approve, all);
    assert_eq!(
        plan.lines,
        vec!["Switch to bypass and approve the 6 waiting cards? (y/n)".to_string()]
    );
    assert_eq!(
        set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
        Ok(ConfirmOutcome::Entered { approved: 6 })
    );
    assert_eq!(
        provider.resolutions(),
        all.iter().map(|id| (id.clone(), true)).collect::<Vec<_>>()
    );
    assert_eq!(set.get(tab).unwrap().attention.attention().pending, 0);
    assert!(all
        .iter()
        .all(|id| set.get(tab).unwrap().session.host_answered.contains(id)));
    provider.queue(call_done("toolu_g", false));
    pump_until_done(&mut set, &dir, "toolu_g");
    assert_eq!(
        call_in_snapshot(&mut set, "toolu_g")["promptNote"],
        "your ask rule: Bash(cat:*) — allowed in bypass"
    );

    // In the bypass tab itself such a prompt is answered at once and never drawn.
    provider.queue(changed("perm-silent-2", "toolu_s2", &|p| p.reason = None));
    until("the second silent prompt answered", || {
        if let Some(payload) = set.pump(&dir, true).active_payload {
            assert!(!payload.contains("perm-silent-2"), "never drawn: {payload}");
        }
        provider.resolutions().iter().any(|(id, _)| id == "perm-silent-2")
    });
    assert_eq!(set.get(tab).unwrap().attention.attention().pending, 0);
    shut_down_all(&mut set);
}

/// A card arriving while the bypass prompt is open, even one forced by an ask rule, cannot be
/// approved by a `y` to a question that omitted it. Confirmation asks again with the new count;
/// only the second `y` approves it.
#[test]
fn a_cli_prompt_arriving_under_an_open_bypass_prompt_asks_again() {
    let dir = workspace("bypass-entry-late-ask");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(write("perm-write"));
    until("the first card", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 1
    });
    let first = plan(set.cycle_mode(tab, ViewId::BASE));
    assert_eq!(first.approve, vec!["perm-write".to_string()]);
    provider.queue(cli_prompt(
        "perm-ask",
        "toolu_late",
        "Bash",
        serde_json::json!({ "command": "cat notes.txt" }),
        Some("cat:*"),
    ));
    until("the late card", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 2
    });
    let again = match set.confirm_bypass(first.scope, first.nonce, ViewId::BASE) {
        Ok(ConfirmOutcome::Reprompt(again)) => again,
        other => panic!("a card the prompt did not count asks again, got {other:?}"),
    };
    assert!(provider.resolutions().is_empty(), "{:?}", provider.resolutions());
    assert_eq!(again.approve, vec!["perm-write".to_string(), "perm-ask".to_string()]);
    assert_eq!(
        set.confirm_bypass(again.scope, again.nonce, ViewId::BASE),
        Ok(ConfirmOutcome::Entered { approved: 2 })
    );
    assert_eq!(
        provider.resolutions(),
        vec![("perm-write".to_string(), true), ("perm-ask".to_string(), true)]
    );
    shut_down_all(&mut set);
}

/// `answer_card` names what it could not answer the way the panel's own path always did.
#[test]
fn answering_a_card_on_a_tab_with_no_session_is_refused() {
    let mut set = set();
    let tab = set.active();
    let error = set
        .answer_card(tab, ViewId::BASE, "perm-x", agent::PermissionDecision::Allow)
        .expect_err("no session");
    assert_eq!(error.message, "no active session");
    assert!(error.benign);
    assert!(set
        .answer_card(TabId(999), ViewId::BASE, "perm-x", agent::PermissionDecision::Allow)
        .is_err());
}

/// A `Resync` can swallow a turn boundary (T1's `TurnCompleted` and T2's
/// `TurnStarted` both dropped with the overflow), so the turn-end clear never runs. Every Resync
/// forgets the tab's approvals, so a T2 prompt reusing T1's id -- same tool, same input -- is a
/// card, not answered on T1's approval.
#[test]
fn a_resync_forgets_every_approval_even_across_a_turn_it_swallowed() {
    let dir = workspace("o3-resync-forgets");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

    provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    provider.queue(gate_write("perm-hook", "toolu_reused"));
    until("the gate's card", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 1
    });
    set.answer_card(tab, ViewId::BASE, "perm-hook", agent::PermissionDecision::Allow)
        .map_err(|e| e.message)
        .unwrap();
    assert!(set.get(tab).unwrap().session.human_allowed.contains("toolu_reused"));

    provider.queue(resolved("perm-hook"));
    provider.queue(AgentDomainEvent::TurnCompleted {
        turn_id: "t1".into(),
        outcome: agent::TurnOutcome::Completed,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
        detail: Default::default(),
    });
    provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t2".into() });
    for i in 0..300 {
        provider.queue(AgentDomainEvent::ContentDelta {
            turn_id: "t2".into(),
            kind: agent::ContentKind::Text,
            text: format!("chunk {i}"),
        });
    }
    until("t2 folded", || {
        let backend = set.get(tab).unwrap().live().unwrap();
        let projection = backend.projection();
        projection.active_turn_id.as_deref() == Some("t2")
            && projection
                .transcript
                .last()
                .is_some_and(|m| m.text.ends_with("chunk 299"))
    });
    let out = set.pump(&dir, true);
    let payload: serde_json::Value = serde_json::from_str(&out.active_payload.expect("a payload")).unwrap();
    assert_eq!(payload["kind"], "snapshot", "the overflow must have become a Resync");
    assert!(set.get(tab).unwrap().session.human_allowed.is_empty());

    provider.queue(cli_prompt(
        "perm-cli-t2",
        "toolu_reused",
        "Write",
        serde_json::json!({ "file_path": ".git/probe2", "content": "o3" }),
        None,
    ));
    until("the T2 prompt cards", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 1
    });
    assert_eq!(provider.resolutions(), vec![("perm-hook".to_string(), true)]);
    shut_down_all(&mut set);
}

/// `r` forgets the approvals at once, before any pump.
#[test]
fn reset_forgets_the_approvals() {
    let mut set = set();
    let tab = set.active();
    let t = set.get_mut(tab).unwrap();
    t.backend = TabBackend::Failed { reason: "gone".into() };
    t.session
        .human_allowed
        .record("toolu_x", "Write", &serde_json::json!({}));
    set.reset(tab).unwrap();
    assert!(set.get(tab).unwrap().session.human_allowed.is_empty());
}

/// A tab with no backend holds no approvals: the pump's non-live arm forgets them.
#[test]
fn a_tab_without_a_backend_holds_no_approvals() {
    let dir = workspace("o3-no-backend");
    let mut set = set();
    let tab = set.active();
    set.get_mut(tab)
        .unwrap()
        .session
        .human_allowed
        .record("toolu_x", "Write", &serde_json::json!({}));
    set.pump(&dir, true);
    assert!(set.get(tab).unwrap().session.human_allowed.is_empty());
}

/// An Approve the provider refused approved nothing, so nothing is recorded.
#[test]
fn a_refused_approve_records_no_approval() {
    let dir = workspace("o3-refused-approve");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(gate_write("perm-hook", "toolu_r"));
    until("the gate's card", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 1
    });
    provider.refuse_resolutions(true);
    assert!(set
        .answer_card(tab, ViewId::BASE, "perm-hook", agent::PermissionDecision::Allow)
        .is_err());
    assert!(set.get(tab).unwrap().session.human_allowed.is_empty());
    shut_down_all(&mut set);
}

/// A call whose CLI prompt was answered without a card says
/// so, muted, on its row -- in the events payload when the call completes, and in every later
/// snapshot -- as a call a saved rule answered does.
#[test]
fn a_call_answered_without_a_card_says_so_on_its_row() {
    let dir = workspace("o3-row-note");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let plan = plan(set.cycle_mode(tab, ViewId::BASE));
    assert_eq!(
        set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
        Ok(ConfirmOutcome::Entered { approved: 0 })
    );
    let input = serde_json::json!({ "file_path": ".git/probe2", "content": "o3" });
    provider.queue(AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_n".into(),
        name: "Write".into(),
        input: input.clone(),
    });
    provider.queue(cli_prompt("perm-cli", "toolu_n", "Write", input, None));
    until("answered", || {
        set.pump(&dir, true);
        !provider.resolutions().is_empty()
    });
    provider.queue(resolved("perm-cli"));
    provider.queue(AgentDomainEvent::ToolCallCompleted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_n".into(),
        content: serde_json::json!("File created successfully"),
        is_error: false,
    });
    let mut notes = serde_json::Value::Null;
    until("the note", || {
        if let Some(payload) = set.pump(&dir, true).active_payload {
            let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
            if value.get("promptNotes").is_some() {
                notes = value["promptNotes"].clone();
            }
        }
        !notes.is_null()
    });
    assert_eq!(
        notes,
        serde_json::json!([{ "toolUseId": "toolu_n", "note": "Claude Code asked — allowed in bypass" }])
    );
    let snapshot: serde_json::Value = serde_json::from_str(&set.active_state_payloads()[0]).unwrap();
    assert_eq!(
        snapshot["state"]["toolCalls"][0]["promptNote"],
        "Claude Code asked — allowed in bypass"
    );
    shut_down_all(&mut set);
}
