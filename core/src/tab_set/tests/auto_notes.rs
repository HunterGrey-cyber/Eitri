//! A call answered by the fast path or by a rule is named in the events and the snapshots, and when its note is dropped.
#![cfg(test)]

use super::*;

#[test]
fn remote_tab_write_cards_have_no_local_file_notes_or_rule_offers() {
    for remote in [true, false] {
        let dir = workspace("remote-card-notes");
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Manual);
        if remote {
            set.set_project_origin(crate::remote_project::TabOrigin::Remote {
                host: "example-host".into(),
                host_key_sha256: "SHA256:example".into(),
                root: "/srv/project".into(),
            })
            .unwrap();
        }
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
        assert!(!dir.join("missing.rs").exists());
        provider.queue_all(vec![
            AgentDomainEvent::PermissionRequested {
                permission_id: "write-missing".into(),
                tool_use_id: Some("tu-write".into()),
                tool_name: "Write".into(),
                input: serde_json::json!({"file_path": "missing.rs", "content": "new"}),
                provider_prompt: None,
            },
            AgentDomainEvent::PermissionRequested {
                permission_id: "bash-rule".into(),
                tool_use_id: Some("tu-bash".into()),
                tool_name: "Bash".into(),
                input: serde_json::json!({"command": "cargo test --lib"}),
                provider_prompt: None,
            },
        ]);
        until("both cards delivered", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 2
        });
        set.pump(&dir, true);
        let tab = set.get(tab).unwrap();
        assert_eq!(tab.notes.creates_file.contains_key("write-missing"), !remote);
        assert_eq!(tab.session.rule_offers.contains_key("bash-rule"), !remote);
        if remote {
            assert!(tab.notes.creates_file.is_empty());
            assert!(tab.session.rule_offers.is_empty());
        }
        shut_down_all(&mut set);
    }
}

/// Saved rules answer background tabs too, and a card eligible for a rule offers one.
#[test]
fn rules_answer_a_background_call_and_a_card_is_offered_the_rule_that_would() {
    let dir = workspace("tabs-rules");
    let mut set = set();
    set.set_rules(agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap()));
    let background = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(background).unwrap().backend = TabBackend::Live(backend);
    set.open(AgentId::Claude);
    let bash = |id: &str, command: &str| AgentDomainEvent::PermissionRequested {
        permission_id: id.into(),
        tool_use_id: None,
        tool_name: "Bash".into(),
        input: serde_json::json!({ "command": command }),
        provider_prompt: None,
    };
    provider.queue(bash("perm-npm", "npm ci"));
    provider.queue(bash("perm-cargo", "cargo test --lib"));
    let mut changed = Vec::new();
    until("the cargo card", || {
        changed.extend(set.pump(&dir, true).offers_changed);
        set.get(background).unwrap().attention.attention().pending == 1
            && set
                .get(background)
                .unwrap()
                .session
                .rule_offers
                .contains_key("perm-cargo")
    });
    assert_eq!(provider.resolutions(), vec![("perm-npm".to_string(), true)]);
    let offers = &set.get(background).unwrap().session.rule_offers;
    assert_eq!(offers["perm-cargo"].display(), "cargo test *");
    assert!(changed.contains(&background));
    shut_down_all(&mut set);
}

/// A call a saved rule answered is named in the events payload and in every later
/// snapshot; a call the user was asked about, and one no rule matched, are not.
#[test]
fn a_call_a_rule_answered_is_named_in_events_and_snapshots() {
    let dir = workspace("tabs-rule-notes");
    let mut set = set();
    set.set_rules(agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap()));
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let call = |id: &str, command: &str| AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: id.into(),
        name: "Bash".into(),
        input: serde_json::json!({ "command": command }),
    };
    let gate = |perm: &str, id: &str, command: &str| AgentDomainEvent::PermissionRequested {
        permission_id: perm.into(),
        tool_use_id: Some(id.into()),
        tool_name: "Bash".into(),
        input: serde_json::json!({ "command": command }),
        provider_prompt: None,
    };
    let done = |id: &str| AgentDomainEvent::ToolCallCompleted {
        turn_id: "t1".into(),
        tool_use_id: id.into(),
        content: serde_json::json!("ok"),
        is_error: false,
    };
    provider.queue(started("t1"));
    provider.queue(call("toolu_npm", "npm ci"));
    provider.queue(gate("perm-npm", "toolu_npm", "npm ci"));
    provider.queue(done("toolu_npm"));
    provider.queue(call("toolu_ls", "ls"));
    provider.queue(done("toolu_ls"));
    provider.queue(call("toolu_cargo", "cargo test"));
    provider.queue(gate("perm-cargo", "toolu_cargo", "cargo test"));
    provider.queue(done("toolu_cargo"));
    let mut payloads = Vec::new();
    until("the three calls to finish", || {
        payloads.extend(set.pump(&dir, true).active_payload);
        payloads
            .iter()
            .any(|p| p.contains("toolu_cargo") && p.contains("tool_call_completed"))
    });
    let notes: Vec<serde_json::Value> = payloads
        .iter()
        .map(|p| serde_json::from_str::<serde_json::Value>(p).unwrap())
        .filter_map(|p| p.get("ruleNotes").cloned())
        .collect();
    assert_eq!(
        notes,
        vec![serde_json::json!([{ "toolUseId": "toolu_npm", "rule": "Bash(npm ci *)" }])]
    );
    let snapshot: serde_json::Value = serde_json::from_str(&set.active_state_payloads()[0]).unwrap();
    let calls = snapshot["state"]["toolCalls"].as_array().unwrap();
    let rule_of = |id: &str| {
        calls
            .iter()
            .find(|c| c["toolUseId"] == id)
            .unwrap()
            .get("allowedByRule")
            .cloned()
    };
    assert_eq!(rule_of("toolu_npm"), Some(serde_json::json!("Bash(npm ci *)")));
    assert_eq!(rule_of("toolu_ls"), None, "the classifier allowed it, not a rule");
    assert_eq!(rule_of("toolu_cargo"), None, "a card was shown");
    shut_down_all(&mut set);
}

/// An in-project `Write` the acceptEdits fast path answers with no card is
/// named "allowed by auto" -- on the events envelope as it happens, and on the snapshot after --
/// while a `Write` into a protected path (`.git/`, still carded, `write()`'s own reason for
/// existing) carries no such note and stays a pending card.
#[test]
fn an_edit_the_fast_path_allowed_is_named_auto_in_events_and_snapshots() {
    let dir = workspace("tabs-auto-notes");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let started_call = |id: &str, name: &str, input: serde_json::Value| AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: id.into(),
        name: name.into(),
        input,
    };
    let gate = |perm: &str, id: &str, name: &str, input: serde_json::Value| AgentDomainEvent::PermissionRequested {
        permission_id: perm.into(),
        tool_use_id: Some(id.into()),
        tool_name: name.into(),
        input,
        provider_prompt: None,
    };
    let done = |id: &str| AgentDomainEvent::ToolCallCompleted {
        turn_id: "t1".into(),
        tool_use_id: id.into(),
        content: serde_json::json!("ok"),
        is_error: false,
    };
    provider.queue(started("t1"));
    provider.queue(started_call(
        "toolu_write",
        "Write",
        serde_json::json!({ "file_path": "new.rs", "content": "fn new() {}" }),
    ));
    provider.queue(gate(
        "perm-write",
        "toolu_write",
        "Write",
        serde_json::json!({ "file_path": "new.rs", "content": "fn new() {}" }),
    ));
    provider.queue(done("toolu_write"));
    provider.queue(started_call(
        "toolu_gitwrite",
        "Write",
        serde_json::json!({ "file_path": ".git/main.rs", "content": "" }),
    ));
    provider.queue(gate(
        "perm-gitwrite",
        "toolu_gitwrite",
        "Write",
        serde_json::json!({ "file_path": ".git/main.rs", "content": "" }),
    ));
    let mut payloads = Vec::new();
    until(
        "the fast path to answer the in-project write, and the protected one to card",
        || {
            payloads.extend(set.pump(&dir, true).active_payload);
            payloads
                .iter()
                .any(|p| p.contains("toolu_write") && p.contains("tool_call_completed"))
                && payloads.iter().any(|p| p.contains("perm-gitwrite"))
        },
    );
    let auto_notes: Vec<serde_json::Value> = payloads
        .iter()
        .map(|p| serde_json::from_str::<serde_json::Value>(p).unwrap())
        .filter_map(|p| p.get("autoNotes").cloned())
        .collect();
    assert_eq!(auto_notes, vec![serde_json::json!(["toolu_write"])]);
    let snapshot: serde_json::Value = serde_json::from_str(&set.active_state_payloads()[0]).unwrap();
    let calls = snapshot["state"]["toolCalls"].as_array().unwrap();
    let auto_of = |id: &str| {
        calls
            .iter()
            .find(|c| c["toolUseId"] == id)
            .unwrap()
            .get("allowedByAuto")
            .cloned()
    };
    assert_eq!(auto_of("toolu_write"), Some(serde_json::json!(true)));
    assert_eq!(auto_of("toolu_gitwrite"), None, "a card was shown, not the fast path");
    let pending = snapshot["state"]["pendingPermissions"].as_array().unwrap();
    assert!(
        pending.iter().any(|p| p["permissionId"] == "perm-gitwrite"),
        "the protected write is still a card, not silently allowed"
    );
    shut_down_all(&mut set);
}

/// A rule can never fire for `Write`/`Edit`/`NotebookEdit` at all
/// (`agent::permission_policy`'s module doc), so a fast-path allow can never carry a rule note
/// too -- pinned directly rather than left to follow from the two features never overlapping in
/// practice.
#[test]
fn an_auto_allowed_edit_never_also_carries_a_rule_note() {
    let dir = workspace("tabs-auto-notes-no-rule");
    let mut set = set();
    set.set_rules(agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap()));
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(started("t1"));
    provider.queue(AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_edit".into(),
        name: "Edit".into(),
        input: serde_json::json!({ "file_path": "main.rs", "old_string": "fn main() {}", "new_string": "fn main() { }" }),
    });
    provider.queue(AgentDomainEvent::PermissionRequested {
        permission_id: "perm-edit".into(),
        tool_use_id: Some("toolu_edit".into()),
        tool_name: "Edit".into(),
        input: serde_json::json!({ "file_path": "main.rs", "old_string": "fn main() {}", "new_string": "fn main() { }" }),
        provider_prompt: None,
    });
    provider.queue(AgentDomainEvent::ToolCallCompleted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_edit".into(),
        content: serde_json::json!("ok"),
        is_error: false,
    });
    until("the edit to finish", || {
        set.pump(&dir, true)
            .active_payload
            .is_some_and(|p| p.contains("toolu_edit") && p.contains("tool_call_completed"))
    });
    let snapshot: serde_json::Value = serde_json::from_str(&set.active_state_payloads()[0]).unwrap();
    let call = &snapshot["state"]["toolCalls"][0];
    assert_eq!(call["allowedByAuto"], serde_json::json!(true));
    assert!(call.get("allowedByRule").is_none());
    shut_down_all(&mut set);
}

/// A `Write` answered by the acceptEdits fast path still marks its completed row as creating a
/// file when the target did not exist. `note_new_files` runs from delivered permission requests,
/// which automatic answering suppresses. The existence check must therefore run where
/// `answer_what_needs_no_human` sends its allow, so a newly created file never gets an overwrite
/// warning. Inferring creation from `ToolCallStarted` in Auto would miss other answering modes.
#[test]
fn an_auto_allowed_write_over_no_file_is_marked_as_creating_one() {
    let dir = workspace("tabs-auto-new-file");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(started("t1"));
    provider.queue(AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_new".into(),
        name: "Write".into(),
        input: serde_json::json!({ "file_path": "brand_new.rs", "content": "fn x() {}" }),
    });
    // The fast-path note is recorded at the gate answer. Without a gate request this fixture
    // would model a call rejected before permission checking, leaving no answer to annotate.
    provider.queue(AgentDomainEvent::PermissionRequested {
        permission_id: "perm-new".into(),
        tool_use_id: Some("toolu_new".into()),
        tool_name: "Write".into(),
        input: serde_json::json!({ "file_path": "brand_new.rs", "content": "fn x() {}" }),
        provider_prompt: None,
    });
    provider.queue(AgentDomainEvent::ToolCallCompleted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_new".into(),
        content: serde_json::json!("ok"),
        is_error: false,
    });
    let mut payloads = Vec::new();
    until("the fast-path write to complete", || {
        payloads.extend(set.pump(&dir, true).active_payload);
        payloads
            .iter()
            .any(|p| p.contains("toolu_new") && p.contains("tool_call_completed"))
    });
    let auto_creates: Vec<serde_json::Value> = payloads
        .iter()
        .map(|p| serde_json::from_str::<serde_json::Value>(p).unwrap())
        .filter_map(|p| p.get("autoCreatesFile").cloned())
        .collect();
    assert_eq!(auto_creates, vec![serde_json::json!(["toolu_new"])]);
    let snapshot: serde_json::Value = serde_json::from_str(&set.active_state_payloads()[0]).unwrap();
    let call = snapshot["state"]["toolCalls"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["toolUseId"] == "toolu_new")
        .unwrap();
    assert_eq!(call["createsFile"], serde_json::json!(true));
    assert_eq!(call["allowedByAuto"], serde_json::json!(true));
    shut_down_all(&mut set);
}

/// A queue overflow may drop a follow-up card's `PermissionRequested`, which normally removes
/// a fast-path note candidate. The complete projection still has that permission pending,
/// so `Resync` must draw the card and sweep its candidate. Otherwise human approval could
/// complete the call with a false "allowed by auto" note.
///
/// The fixture must first edit an ordinary in-project file the fast path actually answers.
/// A protected `.git/probe` write cards immediately and creates no candidate, so it cannot
/// detect a broken sweep. Overflowing the CLI's follow-up prompt for the same allowed call
/// leaves a real candidate for the sweep to remove.
#[test]
fn a_resync_drops_a_fast_path_candidate_whose_further_card_survived_the_overflow() {
    let dir = workspace("tabs-resync-drops-candidate");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let input =
        serde_json::json!({ "file_path": "main.rs", "old_string": "fn main() {}", "new_string": "fn main() { }" });

    provider.queue(started("t1"));
    provider.queue(call_started("toolu_edit", "Edit", input.clone()));
    provider.queue(AgentDomainEvent::PermissionRequested {
        permission_id: "perm-gate".into(),
        tool_use_id: Some("toolu_edit".into()),
        tool_name: "Edit".into(),
        input: input.clone(),
        provider_prompt: None,
    });
    until("the fast path to answer the gate", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().session.host_answered.contains("perm-gate")
    });
    assert!(set
        .get(tab)
        .unwrap()
        .session
        .note_ledger
        .has("toolu_edit", NoteKind::AutoEdit));

    // The CLI's own follow-up prompt for the same call, plus filler enough to overflow the
    // queue so the whole batch -- the prompt's `PermissionRequested` included -- is dropped and
    // this tick's drain is a `Resync`. Queued as one batch (`queue_all`'s doc): a `queue` call
    // per event would let the ingestion thread fold the follow-up card alone, well before the
    // filler loop even finishes pushing the rest, so the `until` below could succeed before the
    // batch had actually overflowed.
    let mut batch = vec![
        resolved("perm-gate"),
        cli_prompt("perm-edit", "toolu_edit", "Edit", input, None),
    ];
    batch.extend((0..(agent::UI_EVENT_QUEUE_CAPACITY + 20)).map(|i| text_delta("t1", &format!("chunk {i}"))));
    provider.queue_all(batch);
    until("the projection to carry the follow-up card", || {
        let backend = set.get(tab).unwrap().live().unwrap();
        backend.projection().pending_permissions.contains_key("perm-edit")
    });
    let out = set.pump(&dir, true);
    let payload: serde_json::Value = serde_json::from_str(&out.active_payload.expect("a payload")).unwrap();
    assert_eq!(payload["kind"], "snapshot", "the overflow must have become a Resync");
    assert!(
        !set.get(tab)
            .unwrap()
            .session
            .note_ledger
            .has("toolu_edit", NoteKind::AutoEdit),
        "the still-pending sweep must drop a candidate whose own further card is genuinely pending"
    );

    set.answer_card(tab, ViewId::BASE, "perm-edit", agent::PermissionDecision::Allow)
        .map_err(|e| e.message)
        .unwrap();
    provider.queue(resolved("perm-edit"));
    provider.queue(AgentDomainEvent::ToolCallCompleted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_edit".into(),
        content: serde_json::json!("ok"),
        is_error: false,
    });
    until("the edit to complete", || {
        set.pump(&dir, true)
            .active_payload
            .is_some_and(|p| p.contains("toolu_edit") && p.contains("tool_call_completed"))
    });
    let snapshot: serde_json::Value = serde_json::from_str(&set.active_state_payloads()[0]).unwrap();
    let call = snapshot["state"]["toolCalls"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["toolUseId"] == "toolu_edit")
        .unwrap();
    assert!(
        call.get("allowedByAuto").is_none(),
        "a human approved this on screen; it must never read allowed by auto"
    );
    shut_down_all(&mut set);
}

fn auto_notes_in(payloads: &[String]) -> Vec<serde_json::Value> {
    payloads
        .iter()
        .map(|p| serde_json::from_str::<serde_json::Value>(p).unwrap())
        .filter_map(|p| p.get("autoNotes").cloned())
        .collect()
}

/// The CLI validates input before `PreToolUse`. An Edit of a file not read yet or lacking
/// `old_string` can finish with `is_error` without asking the gate. It must not claim
/// "allowed by auto": no card does not imply an automatic answer.
#[test]
fn an_edit_that_failed_validation_before_the_gate_is_never_named_auto() {
    let dir = workspace("tabs-auto-validation-failed");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(started("t1"));
    provider.queue(call_started(
        "toolu_hosts",
        "Edit",
        serde_json::json!({ "file_path": "/etc/hosts", "old_string": "a", "new_string": "b" }),
    ));
    provider.queue(call_done("toolu_hosts", true));
    provider.queue(call_started(
        "toolu_main",
        "Edit",
        serde_json::json!({ "file_path": "main.rs", "old_string": "not there", "new_string": "b" }),
    ));
    provider.queue(call_done("toolu_main", true));
    let payloads = pump_until_done(&mut set, &dir, "toolu_main");
    assert!(auto_notes_in(&payloads).is_empty(), "{payloads:?}");
    for id in ["toolu_hosts", "toolu_main"] {
        assert!(call_in_snapshot(&mut set, id).get("allowedByAuto").is_none(), "{id}");
    }
    shut_down_all(&mut set);
}

/// A card without a tool-use id maps to `None` and still removes a matching note candidate.
/// Human approval of an out-of-project Write must not complete it as "allowed by auto" merely
/// because id matching cannot connect the card to its call.
#[test]
fn a_carded_edit_whose_request_names_no_tool_use_id_is_never_named_auto() {
    let dir = workspace("tabs-auto-card-without-id");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let input = serde_json::json!({ "file_path": "/etc/eitri-out.txt", "content": "x" });
    provider.queue(started("t1"));
    provider.queue(call_started("toolu_out", "Write", input.clone()));
    provider.queue(AgentDomainEvent::PermissionRequested {
        permission_id: "perm-out".into(),
        tool_use_id: None,
        tool_name: "Write".into(),
        input,
        provider_prompt: None,
    });
    until("the card to be delivered", || {
        set.pump(&dir, true)
            .active_payload
            .is_some_and(|p| p.contains("perm-out"))
    });
    set.answer_card(tab, ViewId::BASE, "perm-out", agent::PermissionDecision::Allow)
        .map_err(|e| e.message)
        .unwrap();
    provider.queue(resolved("perm-out"));
    provider.queue(call_done("toolu_out", false));
    let payloads = pump_until_done(&mut set, &dir, "toolu_out");
    assert!(auto_notes_in(&payloads).is_empty(), "{payloads:?}");
    assert!(call_in_snapshot(&mut set, "toolu_out").get("allowedByAuto").is_none());
    shut_down_all(&mut set);
}

/// A switch to bypass between call start and gate makes bypass answer the call, never the
/// fast path. An Auto candidate must not survive into a note when the fast path answered nothing.
#[test]
fn an_edit_bypass_answered_after_a_switch_is_never_named_auto() {
    let dir = workspace("tabs-auto-then-bypass");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let input =
        serde_json::json!({ "file_path": "main.rs", "old_string": "fn main() {}", "new_string": "fn main() { }" });
    provider.queue(started("t1"));
    provider.queue(call_started("toolu_sw", "Edit", input.clone()));
    until("the call to be delivered in Auto", || {
        set.pump(&dir, true)
            .active_payload
            .is_some_and(|p| p.contains("toolu_sw"))
    });
    set.get_mut(tab).unwrap().mode = SessionModeChoice::Bypass;
    provider.queue(AgentDomainEvent::PermissionRequested {
        permission_id: "perm-sw".into(),
        tool_use_id: Some("toolu_sw".into()),
        tool_name: "Edit".into(),
        input,
        provider_prompt: None,
    });
    provider.queue(call_done("toolu_sw", false));
    let payloads = pump_until_done(&mut set, &dir, "toolu_sw");
    assert!(
        set.get(tab).unwrap().session.host_answered.contains("perm-sw"),
        "bypass answered it"
    );
    assert!(auto_notes_in(&payloads).is_empty(), "{payloads:?}");
    assert!(call_in_snapshot(&mut set, "toolu_sw").get("allowedByAuto").is_none());
    shut_down_all(&mut set);
}

/// Bypass answers a Write's gate and suppresses its permission request, so `note_new_files`
/// cannot rely on delivered requests. Checking file existence where the answer is sent in
/// both Auto and bypass marks newly created files correctly; an existing file gets no creation
/// note. Otherwise a new bypass-written file would keep a misleading overwrite warning.
#[test]
fn a_bypass_write_over_no_file_is_marked_as_creating_one() {
    let dir = workspace("tabs-bypass-new-file");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    set.get_mut(tab).unwrap().mode = SessionModeChoice::Bypass;
    provider.queue(started("t1"));
    for (id, perm, path) in [
        ("toolu_new", "perm-new", "fresh.rs"),
        ("toolu_old", "perm-old", "main.rs"),
    ] {
        let input = serde_json::json!({ "file_path": path, "content": "fn x() {}" });
        provider.queue(call_started(id, "Write", input.clone()));
        provider.queue(AgentDomainEvent::PermissionRequested {
            permission_id: perm.into(),
            tool_use_id: Some(id.into()),
            tool_name: "Write".into(),
            input,
            provider_prompt: None,
        });
        provider.queue(call_done(id, false));
    }
    pump_until_done(&mut set, &dir, "toolu_old");
    let fresh = call_in_snapshot(&mut set, "toolu_new");
    assert_eq!(fresh["createsFile"], serde_json::json!(true));
    assert!(fresh.get("allowedByAuto").is_none(), "bypass, not the fast path");
    assert!(call_in_snapshot(&mut set, "toolu_old").get("createsFile").is_none());
    shut_down_all(&mut set);
}

/// The created-file note must reach the panel on the events path too, not only in a later snapshot.
/// A gate usually arrives in a batch of its own, after its call's start; answered and dropped, it
/// leaves that batch empty (`RevisedDelivery::Nothing`), so a note made from the answer is held
/// for the next events payload rather than lost with the batch.
#[test]
fn a_created_file_note_answered_in_a_batch_of_its_own_reaches_the_next_events_payload() {
    let dir = workspace("tabs-creates-file-alone");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    set.get_mut(tab).unwrap().mode = SessionModeChoice::Bypass;
    let input = serde_json::json!({ "file_path": "alone.rs", "content": "fn x() {}" });
    provider.queue(started("t1"));
    provider.queue(call_started("toolu_alone", "Write", input.clone()));
    let mut payloads = Vec::new();
    until("the call to be delivered", || {
        payloads.extend(set.pump(&dir, true).active_payload);
        payloads.iter().any(|p| p.contains("toolu_alone"))
    });
    provider.queue(AgentDomainEvent::PermissionRequested {
        permission_id: "perm-alone".into(),
        tool_use_id: Some("toolu_alone".into()),
        tool_name: "Write".into(),
        input,
        provider_prompt: None,
    });
    until("bypass to answer the gate", || {
        payloads.extend(set.pump(&dir, true).active_payload);
        set.get(tab).unwrap().session.host_answered.contains("perm-alone")
    });
    provider.queue(call_done("toolu_alone", false));
    payloads.extend(pump_until_done(&mut set, &dir, "toolu_alone"));
    let creates: Vec<serde_json::Value> = payloads
        .iter()
        .map(|p| serde_json::from_str::<serde_json::Value>(p).unwrap())
        .filter_map(|p| p.get("autoCreatesFile").cloned())
        .collect();
    assert_eq!(creates, vec![serde_json::json!(["toolu_alone"])]);
    shut_down_all(&mut set);
}

/// A fast-path answer remains a candidate until its call completes. If queue overflow drops
/// the completion, `Resync` must still turn it into a note. Finishing before the resync makes
/// the finished sweep consume the candidate before the still-pending `host_answered` filter;
/// `a_resync_before_the_call_finishes_keeps_its_fast_path_answer` pins that filter separately.
#[test]
fn a_resync_turns_a_fast_path_answer_whose_call_finished_into_its_note() {
    let dir = workspace("tabs-resync-keeps-auto-answer");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let input =
        serde_json::json!({ "file_path": "main.rs", "old_string": "fn main() {}", "new_string": "fn main() { }" });
    provider.queue(started("t1"));
    provider.queue(call_started("toolu_fp", "Edit", input.clone()));
    provider.queue(AgentDomainEvent::PermissionRequested {
        permission_id: "perm-fp".into(),
        tool_use_id: Some("toolu_fp".into()),
        tool_name: "Edit".into(),
        input,
        provider_prompt: None,
    });
    until("the fast path to answer the edit", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().session.host_answered.contains("perm-fp")
    });
    assert!(set
        .get(tab)
        .unwrap()
        .session
        .note_ledger
        .has("toolu_fp", NoteKind::AutoEdit));
    // One batch, not one `queue` call per event (`queue_all`'s doc): otherwise the ingestion
    // thread can fold the completion alone, before the filler loop even finishes pushing the
    // rest of the batch, so the `until` below could succeed before the overflow actually did.
    let mut batch = vec![call_done("toolu_fp", false)];
    batch.extend((0..(agent::UI_EVENT_QUEUE_CAPACITY + 20)).map(|i| text_delta("t1", &format!("chunk {i}"))));
    provider.queue_all(batch);
    until("the projection to carry the completion", || {
        let backend = set.get(tab).unwrap().live().unwrap();
        backend
            .projection()
            .tool_calls
            .iter()
            .any(|c| c.tool_use_id == "toolu_fp" && c.result.is_some())
    });
    let out = set.pump(&dir, true);
    let payload: serde_json::Value = serde_json::from_str(&out.active_payload.expect("a payload")).unwrap();
    assert_eq!(payload["kind"], "snapshot", "the overflow must have become a Resync");
    assert_eq!(
        call_in_snapshot(&mut set, "toolu_fp")["allowedByAuto"],
        serde_json::json!(true)
    );
    assert!(!set
        .get(tab)
        .unwrap()
        .session
        .note_ledger
        .has("toolu_fp", NoteKind::AutoEdit));
    shut_down_all(&mut set);
}

/// A `Resync` after the fast path answers an edit's gate but before completion must keep its
/// note candidate. The projection may still list the unresolved request as pending;
/// `host_answered` must exclude it from the still-pending sweep, or an early resync would
/// remove the finished call's "allowed by auto" note.
#[test]
fn a_resync_before_the_call_finishes_keeps_its_fast_path_answer() {
    let dir = workspace("tabs-resync-before-completion");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let input =
        serde_json::json!({ "file_path": "main.rs", "old_string": "fn main() {}", "new_string": "fn main() { }" });
    provider.queue(started("t1"));
    provider.queue(call_started("toolu_pa", "Edit", input.clone()));
    provider.queue(AgentDomainEvent::PermissionRequested {
        permission_id: "perm-pa".into(),
        tool_use_id: Some("toolu_pa".into()),
        tool_name: "Edit".into(),
        input,
        provider_prompt: None,
    });
    until("the fast path to answer the edit", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().session.host_answered.contains("perm-pa")
    });
    // One batch (`queue_all`'s doc), for the same reason every other prefix+filler sequence in
    // this file uses it, even though this particular `until` loops on `pump` itself below and so
    // has no premature-success race of its own.
    provider.queue_all(
        (0..(agent::UI_EVENT_QUEUE_CAPACITY + 20))
            .map(|i| text_delta("t1", &format!("chunk {i}")))
            .collect(),
    );
    let mut saw_snapshot = false;
    until("a resync", || {
        if let Some(p) = set.pump(&dir, true).active_payload {
            let v: serde_json::Value = serde_json::from_str(&p).unwrap();
            saw_snapshot |= v["kind"] == "snapshot";
        }
        saw_snapshot
    });
    assert!(
        set.get(tab)
            .unwrap()
            .session
            .note_ledger
            .has("toolu_pa", NoteKind::AutoEdit),
        "the resync dropped a fast-path candidate whose call had not finished"
    );
    provider.queue(call_done("toolu_pa", false));
    pump_until_done(&mut set, &dir, "toolu_pa");
    assert_eq!(
        call_in_snapshot(&mut set, "toolu_pa")["allowedByAuto"],
        serde_json::json!(true)
    );
    shut_down_all(&mut set);
}

/// When the fast path allows the gate and a human then approves the CLI's follow-up prompt
/// for that call, the completed row must not say "allowed by auto".
/// `NoteLedger::fold` removes the candidate when `PermissionRequested` arrives.
#[test]
fn the_clis_own_prompt_after_the_fast_path_drops_the_auto_note() {
    let dir = workspace("tabs-cli-prompt-drops-auto");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let input =
        serde_json::json!({ "file_path": "main.rs", "old_string": "fn main() {}", "new_string": "fn main() { }" });
    provider.queue(started("t1"));
    provider.queue(call_started("toolu_pb", "Edit", input.clone()));
    provider.queue(AgentDomainEvent::PermissionRequested {
        permission_id: "perm-pb-gate".into(),
        tool_use_id: Some("toolu_pb".into()),
        tool_name: "Edit".into(),
        input: input.clone(),
        provider_prompt: None,
    });
    until("the fast path to answer the gate", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().session.host_answered.contains("perm-pb-gate")
    });
    assert!(set
        .get(tab)
        .unwrap()
        .session
        .note_ledger
        .has("toolu_pb", NoteKind::AutoEdit));
    provider.queue(resolved("perm-pb-gate"));
    provider.queue(cli_prompt("perm-pb-cli", "toolu_pb", "Edit", input, None));
    until("the CLI's own prompt as a card", || {
        set.pump(&dir, true)
            .active_payload
            .is_some_and(|p| p.contains("perm-pb-cli"))
    });
    set.answer_card(tab, ViewId::BASE, "perm-pb-cli", agent::PermissionDecision::Allow)
        .map_err(|e| e.message)
        .unwrap();
    provider.queue(resolved("perm-pb-cli"));
    provider.queue(call_done("toolu_pb", false));
    let payloads = pump_until_done(&mut set, &dir, "toolu_pb");
    assert!(auto_notes_in(&payloads).is_empty(), "{payloads:?}");
    assert!(call_in_snapshot(&mut set, "toolu_pb").get("allowedByAuto").is_none());
    shut_down_all(&mut set);
}

/// Only fast-path answers for `Write`/`Edit`/`NotebookEdit` (`ACCEPT_EDITS_TOOLS`) become auto-note
/// candidates. A `Read` allowed by `answer_what_needs_no_human` must not say "allowed by auto":
/// that note identifies the acceptEdits fast path rather than the read-only rules shared by modes.
#[test]
fn a_classifier_allowed_read_is_never_named_auto() {
    let dir = workspace("tabs-read-not-auto");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let input = serde_json::json!({ "file_path": "main.rs" });
    provider.queue(started("t1"));
    provider.queue(call_started("toolu_pc", "Read", input.clone()));
    provider.queue(AgentDomainEvent::PermissionRequested {
        permission_id: "perm-pc".into(),
        tool_use_id: Some("toolu_pc".into()),
        tool_name: "Read".into(),
        input,
        provider_prompt: None,
    });
    until("the classifier to answer the read", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().session.host_answered.contains("perm-pc")
    });
    assert!(!set
        .get(tab)
        .unwrap()
        .session
        .note_ledger
        .has("toolu_pc", NoteKind::AutoEdit));
    provider.queue(call_done("toolu_pc", false));
    let payloads = pump_until_done(&mut set, &dir, "toolu_pc");
    assert!(auto_notes_in(&payloads).is_empty(), "{payloads:?}");
    assert!(call_in_snapshot(&mut set, "toolu_pc").get("allowedByAuto").is_none());
    shut_down_all(&mut set);
}

/// A queue overflow may drop the CLI follow-up prompt's `PermissionRequested`, which would
/// normally remove a saved-rule note candidate. The complete projection still lists the prompt
/// pending, so `Resync` draws its card and must sweep the candidate. Otherwise the human's later
/// approval could produce a false "allowed by rule" note.
///
/// The fixture must answer an actual gate request through the rule before overflowing the
/// follow-up prompt for the same call. A bare `ToolCallStarted` cannot establish a candidate:
/// notes are recorded only from answers `AgentBackend::answer_what_needs_no_human` actually gave.
#[test]
fn a_resync_drops_a_rule_candidate_whose_further_card_survived_the_overflow() {
    let dir = workspace("tabs-resync-drops-rule-candidate");
    let mut set = set();
    set.set_rules(agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap()));
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let input = serde_json::json!({ "command": "npm ci" });

    provider.queue(started("t1"));
    provider.queue(call_started("toolu_npm", "Bash", input.clone()));
    provider.queue(AgentDomainEvent::PermissionRequested {
        permission_id: "perm-gate".into(),
        tool_use_id: Some("toolu_npm".into()),
        tool_name: "Bash".into(),
        input: input.clone(),
        provider_prompt: None,
    });
    until("the rule to answer the gate as a candidate", || {
        set.pump(&dir, true);
        set.get(tab)
            .unwrap()
            .session
            .note_ledger
            .has("toolu_npm", NoteKind::Rule)
    });

    // The CLI's own follow-up prompt for the same call, plus filler enough to overflow the
    // queue so the whole batch -- the prompt's `PermissionRequested` included -- is dropped and
    // this tick's drain is a `Resync`. Queued as one batch (`queue_all`'s doc): a `queue` call
    // per event would let the ingestion thread fold the follow-up card alone, well before the
    // filler loop even finishes pushing the rest, so the `until` below could succeed before the
    // batch had actually overflowed.
    let mut batch = vec![
        resolved("perm-gate"),
        cli_prompt("perm-npm", "toolu_npm", "Bash", input, None),
    ];
    batch.extend((0..(agent::UI_EVENT_QUEUE_CAPACITY + 20)).map(|i| text_delta("t1", &format!("chunk {i}"))));
    provider.queue_all(batch);
    until("the projection to carry the follow-up card", || {
        let backend = set.get(tab).unwrap().live().unwrap();
        backend.projection().pending_permissions.contains_key("perm-npm")
    });
    let out = set.pump(&dir, true);
    let payload: serde_json::Value = serde_json::from_str(&out.active_payload.expect("a payload")).unwrap();
    assert_eq!(payload["kind"], "snapshot", "the overflow must have become a Resync");
    assert!(
        !set.get(tab)
            .unwrap()
            .session
            .note_ledger
            .has("toolu_npm", NoteKind::Rule),
        "the still-pending sweep must drop a candidate whose own further card is genuinely pending"
    );

    set.answer_card(tab, ViewId::BASE, "perm-npm", agent::PermissionDecision::Allow)
        .map_err(|e| e.message)
        .unwrap();
    provider.queue(resolved("perm-npm"));
    provider.queue(AgentDomainEvent::ToolCallCompleted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_npm".into(),
        content: serde_json::json!("ok"),
        is_error: false,
    });
    until("the npm call to complete", || {
        set.pump(&dir, true)
            .active_payload
            .is_some_and(|p| p.contains("toolu_npm") && p.contains("tool_call_completed"))
    });
    let snapshot: serde_json::Value = serde_json::from_str(&set.active_state_payloads()[0]).unwrap();
    let call = snapshot["state"]["toolCalls"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["toolUseId"] == "toolu_npm")
        .unwrap();
    assert!(
        call.get("allowedByRule").is_none(),
        "a human approved this on screen; it must never read allowed by rule"
    );
    shut_down_all(&mut set);
}

/// When a snapshot already carried every event of the next drain, the payload
/// leaves them all out -- but a note those events produce was not in that snapshot (it was read
/// before this tab folded them), so it still goes to the panel, on an `events` envelope with no
/// events, which the panel applies to what it already holds (`applyCallNotes`). The completion
/// folds through the seam, right after a tick's drain and before the switch's snapshot read.
#[test]
fn a_note_from_events_a_snapshot_carried_still_reaches_the_panel() {
    let dir = workspace("tabs-p1a2-round2-notes");
    let mut set = set();
    set.set_rules(agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap()));
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(started("t1"));
    provider.queue(AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_npm".into(),
        name: "Bash".into(),
        input: serde_json::json!({ "command": "npm ci" }),
    });
    provider.queue(AgentDomainEvent::PermissionRequested {
        permission_id: "perm-npm".into(),
        tool_use_id: Some("toolu_npm".into()),
        tool_name: "Bash".into(),
        input: serde_json::json!({ "command": "npm ci" }),
        provider_prompt: None,
    });
    until("the rule to answer the call's gate", || {
        set.pump(&dir, true);
        provider.resolutions() == vec![("perm-npm".to_string(), true)]
    });

    // The call completes right after a tick's drain, before the switch's snapshot read.
    let completion = Arc::clone(&provider);
    set.after_drain = Some(Box::new(move |backend: &AgentBackend| {
        completion.queue(AgentDomainEvent::ToolCallCompleted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_npm".into(),
            content: serde_json::json!("ok"),
            is_error: false,
        });
        until("the completion to fold", || {
            backend
                .projection()
                .tool_calls
                .iter()
                .any(|call| call.tool_use_id == "toolu_npm" && call.result.is_some())
        });
    }));
    set.pump(&dir, true);
    assert!(set.after_drain.is_none(), "the seam ran");
    let snapshot: serde_json::Value = serde_json::from_str(&set.active_state_payloads()[0]).unwrap();
    let call = &snapshot["state"]["toolCalls"][0];
    assert_eq!(call["toolUseId"], "toolu_npm");
    assert!(!call["result"].is_null(), "the snapshot carries the completion");
    assert!(
        call.get("allowedByRule").is_none(),
        "but not its note: nothing had folded it yet"
    );

    let payload: serde_json::Value = serde_json::from_str(
        &set.pump(&dir, true)
            .active_payload
            .expect("the note is owed to the panel"),
    )
    .unwrap();
    assert_eq!(payload["kind"], "events");
    assert_eq!(
        payload["events"],
        serde_json::json!([]),
        "the completion is not delivered twice"
    );
    assert_eq!(
        payload["ruleNotes"],
        serde_json::json!([{ "toolUseId": "toolu_npm", "rule": "Bash(npm ci *)" }])
    );
    shut_down_all(&mut set);
}

/// A `Write` card whose file is not there when it is raised says it creates one,
/// in the events payload and in later snapshots (on the card and on its call); a card over an
/// existing file does not, nor does its file appearing later change what the card said.
#[test]
fn a_write_card_over_no_file_is_marked_as_creating_one() {
    // Both targets use `.git/` so they card regardless of whether the file exists. Ordinary
    // `new.txt` and `old.txt` would take the acceptEdits fast path; this test needs real cards
    // to pin their `createsFile` flags.
    let dir = workspace("tabs-new-file");
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::write(dir.join(".git/old.txt"), "there").unwrap();
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let write = |perm: &str, id: &str, path: std::path::PathBuf| AgentDomainEvent::PermissionRequested {
        permission_id: perm.into(),
        tool_use_id: Some(id.into()),
        tool_name: "Write".into(),
        input: serde_json::json!({ "file_path": path, "content": "x" }),
        provider_prompt: None,
    };
    provider.queue(AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_new".into(),
        name: "Write".into(),
        input: serde_json::json!({ "file_path": dir.join(".git/new.txt"), "content": "x" }),
    });
    provider.queue(write("perm-new", "toolu_new", dir.join(".git/new.txt")));
    provider.queue(write("perm-old", "toolu_old", dir.join(".git/old.txt")));
    let mut payloads = Vec::new();
    until("both cards", || {
        payloads.extend(set.pump(&dir, true).active_payload);
        set.get(tab).unwrap().attention.attention().pending == 2
    });
    let marked: Vec<serde_json::Value> = payloads
        .iter()
        .map(|p| serde_json::from_str::<serde_json::Value>(p).unwrap())
        .filter_map(|p| p.get("createsFile").cloned())
        .collect();
    assert_eq!(
        marked,
        vec![serde_json::json!([{ "permissionId": "perm-new", "toolUseId": "toolu_new" }])]
    );
    std::fs::write(dir.join(".git/new.txt"), "now there").unwrap();
    let snapshot: serde_json::Value = serde_json::from_str(&set.active_state_payloads()[0]).unwrap();
    let cards = snapshot["state"]["pendingPermissions"].as_array().unwrap();
    let creates = |id: &str| {
        cards
            .iter()
            .find(|c| c["permissionId"] == id)
            .unwrap()
            .get("createsFile")
            .cloned()
    };
    assert_eq!(creates("perm-new"), Some(serde_json::json!(true)));
    assert_eq!(creates("perm-old"), None);
    assert_eq!(
        snapshot["state"]["toolCalls"][0]["createsFile"],
        serde_json::json!(true)
    );
    shut_down_all(&mut set);
}

/// Only "not found" is no file. A path this process cannot
/// look at (a parent it may not search) or one through a regular file says nothing about what
/// the write replaces, so its card keeps the overwrite warning. A dangling symlink is something.
#[test]
fn only_a_path_that_is_not_found_counts_as_a_new_file() {
    use std::os::unix::fs::PermissionsExt;
    let dir = workspace("tabs-new-file-errors");
    std::fs::write(dir.join("plain.txt"), "a file").unwrap();
    std::os::unix::fs::symlink(dir.join("nowhere"), dir.join("dangling")).unwrap();
    let locked = dir.join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(locked.join("inside.txt"), "hidden").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let card = |perm: &str, path: &str| AgentDomainEvent::PermissionRequested {
        permission_id: perm.into(),
        tool_use_id: Some(format!("toolu_{perm}")),
        tool_name: "Write".into(),
        input: serde_json::json!({ "file_path": path, "content": "x" }),
        provider_prompt: None,
    };
    let events = vec![
        card("absent", "absent.txt"),
        card("locked", "locked/inside.txt"),
        card("through-a-file", "plain.txt/child"),
        card("dangling", "dangling"),
        card("existing", "plain.txt"),
    ];
    let mut notes = std::collections::BTreeMap::new();
    let fresh = note_new_files(&mut notes, &events, &dir);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Unmarked either way for `locked`: as a user its lookup is refused (EACCES), and root, who
    // may search a 000 directory, finds the file there.
    assert_eq!(fresh.keys().map(String::as_str).collect::<Vec<_>>(), vec!["absent"]);
    assert_eq!(notes, fresh);
}

fn npm_card(id: &str) -> AgentDomainEvent {
    AgentDomainEvent::PermissionRequested {
        permission_id: format!("perm-{id}"),
        tool_use_id: Some(id.into()),
        tool_name: "Bash".into(),
        input: serde_json::json!({ "command": "npm ci" }),
        provider_prompt: None,
    }
}

fn npm_rule_candidate() -> NoteCandidate {
    NoteCandidate::Rule {
        rule: "Bash(npm ci *)".to_string(),
        call: CandidateCall {
            tool_name: "Bash".to_string(),
            input: serde_json::json!({ "command": "npm ci" }),
        },
    }
}

/// A rule's answer is noted only once its call completes, and only if no card for the same call
/// came first: such a card means a human answered too (the rule's own answer failed, or the CLI
/// asked again after the gate).
#[test]
fn a_card_for_a_rule_answered_call_drops_its_note_and_a_surviving_one_is_noted_on_completion() {
    let mut ledger = NoteLedger::default();
    ledger.record("toolu_carded".to_string(), npm_rule_candidate());
    ledger.record("toolu_ruled".to_string(), npm_rule_candidate());
    let mut kept = CallNotes::default();
    // Spread over two batches, as a card and its answer are: the candidate outlives the first.
    let mut first = CallNotes::default();
    ledger.fold(&[npm_card("toolu_carded")], &mut kept, &mut first);
    assert!(first.is_empty());
    let mut second = CallNotes::default();
    ledger.fold(
        &[call_done("toolu_carded", false), call_done("toolu_ruled", false)],
        &mut kept,
        &mut second,
    );
    assert_eq!(
        second.allowed_by_rule.into_iter().collect::<Vec<_>>(),
        vec![("toolu_ruled".to_string(), "Bash(npm ci *)".to_string())]
    );
    assert!(ledger.is_empty());
    assert!(
        !kept.allowed_by_rule.contains_key("toolu_carded"),
        "a card was delivered for it"
    );
    assert_eq!(
        kept.allowed_by_rule.get("toolu_ruled"),
        Some(&"Bash(npm ci *)".to_string())
    );
}

/// A subagent's call is answered by the same gate as the parent's, and arrives inside a frame: its
/// completion still turns its candidate into the note, and nothing is left waiting in the ledger.
#[test]
fn a_subagent_call_answered_by_a_rule_is_noted_when_its_framed_completion_arrives() {
    let mut ledger = NoteLedger::default();
    ledger.record("toolu_child".to_string(), npm_rule_candidate());
    let mut kept = CallNotes::default();
    let mut fresh = CallNotes::default();
    ledger.fold(
        &[AgentDomainEvent::SubagentFrame {
            parent_tool_use_id: "toolu_agent".to_string(),
            frame: Box::new(call_done("toolu_child", false)),
        }],
        &mut kept,
        &mut fresh,
    );
    assert_eq!(
        fresh.allowed_by_rule.get("toolu_child"),
        Some(&"Bash(npm ci *)".to_string())
    );
    assert!(ledger.is_empty(), "the candidate is not kept once its call completed");
}

/// One call can wait for two notes at once: its gate answered by a rule, then the CLI's own prompt
/// for it answered without a card. A card for the call drops the gate's note (someone answered
/// after the rule), never the prompt's own, which the call's completion then shows.
#[test]
fn a_call_keeps_its_prompt_note_when_a_card_drops_its_rule_note() {
    let mut ledger = NoteLedger::default();
    ledger.record("toolu_both".to_string(), npm_rule_candidate());
    ledger.record(
        "toolu_both".to_string(),
        NoteCandidate::PromptNote("Claude Code asked — allowed in bypass".to_string()),
    );
    let mut kept = CallNotes::default();
    let mut fresh = CallNotes::default();
    ledger.fold(
        &[npm_card("toolu_both"), call_done("toolu_both", false)],
        &mut kept,
        &mut fresh,
    );
    assert!(fresh.allowed_by_rule.is_empty(), "the card dropped the rule's note");
    assert_eq!(
        fresh.prompt_notes.get("toolu_both").map(String::as_str),
        Some("Claude Code asked — allowed in bypass")
    );
    assert_eq!(kept, fresh);
    assert!(ledger.is_empty());
}

/// A resync reads the projection in place of the events it dropped: every candidate whose call
/// finished becomes its note; a gate's candidate whose call still has a card nobody answered for
/// the user is dropped, while one whose only pending request this tab answered itself, and a CLI
/// prompt's note, wait on.
#[test]
fn a_resync_notes_finished_calls_and_drops_gate_candidates_with_a_card_still_pending() {
    let edit = |path: &str| {
        NoteCandidate::AutoEdit(CandidateCall {
            tool_name: "Edit".to_string(),
            input: serde_json::json!({ "file_path": path }),
        })
    };
    let mut ledger = NoteLedger::default();
    ledger.record("toolu_done".to_string(), edit("done.rs"));
    ledger.record("toolu_done".to_string(), npm_rule_candidate());
    ledger.record("toolu_carded".to_string(), edit("carded.rs"));
    ledger.record("toolu_carded".to_string(), npm_rule_candidate());
    ledger.record(
        "toolu_carded".to_string(),
        NoteCandidate::PromptNote("Claude Code asked — allowed with your approval".to_string()),
    );
    ledger.record("toolu_ours".to_string(), edit("ours.rs"));
    let begun = |id: &str| AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: id.into(),
        name: "Edit".into(),
        input: serde_json::json!({}),
    };
    let mut projection = agent::AgentSessionProjection::default();
    for event in [
        started("t1"),
        begun("toolu_done"),
        call_done("toolu_done", false),
        begun("toolu_carded"),
        npm_card("toolu_carded"),
        begun("toolu_ours"),
        npm_card("toolu_ours"),
    ] {
        projection.apply(&event);
    }
    assert_eq!(projection.pending_permissions.len(), 2, "both cards are still pending");
    let host_answered: BTreeSet<String> = ["perm-toolu_ours".to_string()].into();
    let mut kept = CallNotes::default();
    ledger.resync(&projection, &host_answered, &mut kept);
    assert!(kept.allowed_by_auto.contains("toolu_done"));
    assert_eq!(
        kept.allowed_by_rule.get("toolu_done").map(String::as_str),
        Some("Bash(npm ci *)")
    );
    assert_eq!(kept.allowed_by_auto.len() + kept.allowed_by_rule.len(), 2);
    assert!(kept.prompt_notes.is_empty(), "no prompt's call finished");
    assert!(
        !ledger.has("toolu_carded", NoteKind::AutoEdit),
        "a human answers its card"
    );
    assert!(!ledger.has("toolu_carded", NoteKind::Rule), "a human answers its card");
    assert!(
        ledger.has("toolu_carded", NoteKind::PromptNote),
        "a card never drops a prompt's note"
    );
    assert!(
        ledger.has("toolu_ours", NoteKind::AutoEdit),
        "the only pending request is one this tab answered"
    );
}

/// A saved-rule note must come from an actual gate answer, never from reclassifying a started
/// call's arguments and seeing no card. The CLI validates input before `PreToolUse`, so a Bash
/// call can finish with `is_error` without ever requesting permission. Such a call must not
/// claim "allowed by rule npm ci *" when the rule answered nothing.
#[test]
fn a_bash_call_that_failed_validation_before_the_gate_is_never_named_by_rule() {
    let dir = workspace("tabs-rule-validation-failed");
    let mut set = set();
    set.set_rules(agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap()));
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(started("t1"));
    provider.queue(call_started(
        "toolu_ci",
        "Bash",
        serde_json::json!({ "command": "npm ci" }),
    ));
    provider.queue(call_done("toolu_ci", true));
    let payloads = pump_until_done(&mut set, &dir, "toolu_ci");
    let notes: Vec<serde_json::Value> = payloads
        .iter()
        .map(|p| serde_json::from_str::<serde_json::Value>(p).unwrap())
        .filter_map(|p| p.get("ruleNotes").cloned())
        .collect();
    assert!(notes.is_empty(), "{payloads:?}");
    assert!(call_in_snapshot(&mut set, "toolu_ci").get("allowedByRule").is_none());
    shut_down_all(&mut set);
}

/// Switching to bypass between call start and its gate makes bypass answer the call without
/// classification. A speculative rule candidate from the started call's arguments must not
/// survive as a rule note when no rule answered the gate.
#[test]
fn a_bash_call_bypass_answered_after_a_switch_is_never_named_by_rule() {
    let dir = workspace("tabs-rule-then-bypass");
    let mut set = set();
    set.set_rules(agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap()));
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(started("t1"));
    provider.queue(call_started(
        "toolu_sw",
        "Bash",
        serde_json::json!({ "command": "npm ci" }),
    ));
    until("the call to be delivered in Auto", || {
        set.pump(&dir, true)
            .active_payload
            .is_some_and(|p| p.contains("toolu_sw"))
    });
    set.get_mut(tab).unwrap().mode = SessionModeChoice::Bypass;
    provider.queue(AgentDomainEvent::PermissionRequested {
        permission_id: "perm-sw".into(),
        tool_use_id: Some("toolu_sw".into()),
        tool_name: "Bash".into(),
        input: serde_json::json!({ "command": "npm ci" }),
        provider_prompt: None,
    });
    provider.queue(call_done("toolu_sw", false));
    let payloads = pump_until_done(&mut set, &dir, "toolu_sw");
    assert!(
        set.get(tab).unwrap().session.host_answered.contains("perm-sw"),
        "bypass answered it"
    );
    let notes: Vec<serde_json::Value> = payloads
        .iter()
        .map(|p| serde_json::from_str::<serde_json::Value>(p).unwrap())
        .filter_map(|p| p.get("ruleNotes").cloned())
        .collect();
    assert!(notes.is_empty(), "{payloads:?}");
    assert!(call_in_snapshot(&mut set, "toolu_sw").get("allowedByRule").is_none());
    shut_down_all(&mut set);
}

/// The CLI's follow-up prompt for a rule-answered call can omit its tool-use id: Verdandi sends
/// `toolUseId: ''`, and `translate.rs` maps it to `None`. Id matching alone cannot remove that
/// call's note candidate. `NoteLedger::dropped_by_card` matches content instead, so human
/// approval of an id-less card cannot leave a false "allowed by rule" note on completion.
#[test]
fn an_id_less_follow_up_card_a_human_answers_drops_the_rule_candidate_by_content() {
    let dir = workspace("tabs-rule-id-less-follow-up");
    let mut set = set();
    set.set_rules(agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap()));
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let input = serde_json::json!({ "command": "npm ci" });

    provider.queue(started("t1"));
    provider.queue(call_started("toolu_idless", "Bash", input.clone()));
    provider.queue(AgentDomainEvent::PermissionRequested {
        permission_id: "perm-gate".into(),
        tool_use_id: Some("toolu_idless".into()),
        tool_name: "Bash".into(),
        input: input.clone(),
        provider_prompt: None,
    });
    until("the rule to answer the gate as a candidate", || {
        set.pump(&dir, true);
        set.get(tab)
            .unwrap()
            .session
            .note_ledger
            .has("toolu_idless", NoteKind::Rule)
    });

    // The CLI follow-up prompt omits its tool-use id. `needs_its_own_card()` is false because
    // it has a reason, no ask rule and a known origin. Even so, `HumanApprovals::matches` never
    // matches an id-less prompt, so Auto draws a card rather than re-answering it through a rule.
    provider.queue(resolved("perm-gate"));
    provider.queue(AgentDomainEvent::PermissionRequested {
        permission_id: "perm-follow-up".into(),
        tool_use_id: None,
        tool_name: "Bash".into(),
        input: input.clone(),
        provider_prompt: Some(agent::ProviderPrompt {
            reason: Some("Claude requested permissions to run this command.".into()),
            description: None,
            blocked_path: None,
            matched_ask_rule: None,
            unrecognized_origin: None,
        }),
    });
    until("the id-less follow-up card", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 1
    });
    assert!(
        !set.get(tab)
            .unwrap()
            .session
            .note_ledger
            .has("toolu_idless", NoteKind::Rule),
        "the id-less follow-up card must still drop the candidate, matched by content"
    );

    set.answer_card(tab, ViewId::BASE, "perm-follow-up", agent::PermissionDecision::Allow)
        .map_err(|e| e.message)
        .unwrap();
    provider.queue(resolved("perm-follow-up"));
    provider.queue(call_done("toolu_idless", false));
    pump_until_done(&mut set, &dir, "toolu_idless");
    assert!(
        call_in_snapshot(&mut set, "toolu_idless")
            .get("allowedByRule")
            .is_none(),
        "a human approved the follow-up card; it must never read allowed by rule"
    );
    shut_down_all(&mut set);
}

/// A rule-eligible call whose automatic answer the provider refuses stays a card. `by_rule`
/// records candidates only after a successful answer, so this refusal creates none. An empty
/// tool-use id also exercises content matching in `NoteLedger::dropped_by_card`: it must find
/// nothing to drop, and human approval must not yield an "allowed by rule" note.
#[test]
fn a_carded_rule_request_a_human_approves_is_never_named_by_rule() {
    let dir = workspace("tabs-rule-refused-answer");
    let mut set = set();
    set.set_rules(agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap()));
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.refuse_resolutions(true);
    provider.queue(started("t1"));
    provider.queue(call_started(
        "toolu_refused",
        "Bash",
        serde_json::json!({ "command": "npm ci" }),
    ));
    provider.queue(AgentDomainEvent::PermissionRequested {
        permission_id: "perm-refused".into(),
        tool_use_id: None,
        tool_name: "Bash".into(),
        input: serde_json::json!({ "command": "npm ci" }),
        provider_prompt: None,
    });
    until("the refused rule answer to fall back to a card", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().attention.attention().pending == 1
    });
    assert!(
        set.get(tab).unwrap().session.note_ledger.count(NoteKind::Rule) == 0,
        "the answer failed; no candidate was ever recorded"
    );
    provider.refuse_resolutions(false);
    set.answer_card(tab, ViewId::BASE, "perm-refused", agent::PermissionDecision::Allow)
        .map_err(|e| e.message)
        .unwrap();
    provider.queue(resolved("perm-refused"));
    provider.queue(call_done("toolu_refused", false));
    pump_until_done(&mut set, &dir, "toolu_refused");
    assert!(call_in_snapshot(&mut set, "toolu_refused")
        .get("allowedByRule")
        .is_none());
    shut_down_all(&mut set);
}

/// A rule-answered call whose completion was dropped by queue overflow must become a note
/// through `Resync`'s finished sweep, just like prompt and fast-path candidates.
/// Without sweeping rule candidates too, they remain live indefinitely and never display a note.
#[test]
fn a_resync_turns_a_rule_answer_whose_call_finished_into_its_note() {
    let dir = workspace("tabs-resync-keeps-rule-answer");
    let mut set = set();
    set.set_rules(agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap()));
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(started("t1"));
    provider.queue(call_started(
        "toolu_rp",
        "Bash",
        serde_json::json!({ "command": "npm ci" }),
    ));
    provider.queue(AgentDomainEvent::PermissionRequested {
        permission_id: "perm-rp".into(),
        tool_use_id: Some("toolu_rp".into()),
        tool_name: "Bash".into(),
        input: serde_json::json!({ "command": "npm ci" }),
        provider_prompt: None,
    });
    until("the rule to answer the call", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().session.host_answered.contains("perm-rp")
    });
    assert!(set
        .get(tab)
        .unwrap()
        .session
        .note_ledger
        .has("toolu_rp", NoteKind::Rule));
    // One batch, not one `queue` call per event (`queue_all`'s doc): otherwise the ingestion
    // thread can fold the completion alone, before the filler loop even finishes pushing the
    // rest of the batch, so the `until` below could succeed before the overflow actually did.
    let mut batch = vec![call_done("toolu_rp", false)];
    batch.extend((0..(agent::UI_EVENT_QUEUE_CAPACITY + 20)).map(|i| text_delta("t1", &format!("chunk {i}"))));
    provider.queue_all(batch);
    until("the projection to carry the completion", || {
        let backend = set.get(tab).unwrap().live().unwrap();
        backend
            .projection()
            .tool_calls
            .iter()
            .any(|c| c.tool_use_id == "toolu_rp" && c.result.is_some())
    });
    let out = set.pump(&dir, true);
    let payload: serde_json::Value = serde_json::from_str(&out.active_payload.expect("a payload")).unwrap();
    assert_eq!(payload["kind"], "snapshot", "the overflow must have become a Resync");
    assert_eq!(
        call_in_snapshot(&mut set, "toolu_rp")["allowedByRule"],
        serde_json::json!("Bash(npm ci *)")
    );
    assert!(!set
        .get(tab)
        .unwrap()
        .session
        .note_ledger
        .has("toolu_rp", NoteKind::Rule));
    shut_down_all(&mut set);
}

/// A resync after a rule answers the gate but before call completion must keep the rule
/// candidate even if the projection still lists the unresolved request as pending. The
/// `host_answered` filter excludes that request from the still-pending sweep, preserving the
/// finished call's "allowed by rule" note. This complements the corresponding fast-path test
/// and detects removal of that filter.
#[test]
fn a_resync_before_the_call_finishes_keeps_its_rule_answer() {
    let dir = workspace("tabs-resync-before-rule-completion");
    let mut set = set();
    set.set_rules(agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap()));
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(started("t1"));
    provider.queue(call_started(
        "toolu_rb",
        "Bash",
        serde_json::json!({ "command": "npm ci" }),
    ));
    provider.queue(AgentDomainEvent::PermissionRequested {
        permission_id: "perm-rb".into(),
        tool_use_id: Some("toolu_rb".into()),
        tool_name: "Bash".into(),
        input: serde_json::json!({ "command": "npm ci" }),
        provider_prompt: None,
    });
    until("the rule to answer the call", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().session.host_answered.contains("perm-rb")
    });
    // One batch (`queue_all`'s doc), for the same reason every other prefix+filler sequence in
    // this file uses it, even though this particular `until` loops on `pump` itself below and so
    // has no premature-success race of its own.
    provider.queue_all(
        (0..(agent::UI_EVENT_QUEUE_CAPACITY + 20))
            .map(|i| text_delta("t1", &format!("chunk {i}")))
            .collect(),
    );
    let mut saw_snapshot = false;
    until("a resync", || {
        if let Some(p) = set.pump(&dir, true).active_payload {
            let v: serde_json::Value = serde_json::from_str(&p).unwrap();
            saw_snapshot |= v["kind"] == "snapshot";
        }
        saw_snapshot
    });
    assert!(
        set.get(tab)
            .unwrap()
            .session
            .note_ledger
            .has("toolu_rb", NoteKind::Rule),
        "the resync dropped a rule candidate whose call had not finished"
    );
    provider.queue(call_done("toolu_rb", false));
    pump_until_done(&mut set, &dir, "toolu_rb");
    assert_eq!(
        call_in_snapshot(&mut set, "toolu_rb")["allowedByRule"],
        serde_json::json!("Bash(npm ci *)")
    );
    shut_down_all(&mut set);
}

#[test]
fn eviction_deliveries_and_overflow_resyncs_prune_notes_with_lifted_calls_kept() {
    for overflow in [false, true] {
        let dir = workspace("evicted-notes");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        for id in ["gone", "bg", "live"] {
            provider.queue(AgentDomainEvent::ToolCallStarted {
                turn_id: "turn".into(),
                tool_use_id: id.into(),
                name: "Read".into(),
                input: serde_json::json!({}),
            });
        }
        until("the three calls to fold", || {
            set.get(tab).unwrap().live().unwrap().projection().tool_calls.len() == 3
        });
        set.pump(&dir, true);
        let (boundary, lifted, revision) = {
            let p = set.get(tab).unwrap().live().unwrap().projection();
            (p.tool_calls[2].seq, p.tool_calls[1].seq, p.last_revision)
        };
        for id in ["gone", "bg", "live"] {
            set.get_mut(tab)
                .unwrap()
                .notes
                .prompt_notes
                .insert(id.into(), "answered".into());
        }
        provider.queue(AgentDomainEvent::HistoryEvicted {
            through_seq: boundary,
            turns: 2,
            lifted: vec![lifted],
        });
        let extra = if overflow { 300 } else { 0 };
        for _ in 0..extra {
            provider.queue(AgentDomainEvent::EffortReported { effort: None });
        }
        until("eviction and the overflow to fold", || {
            set.get(tab).unwrap().live().unwrap().projection().last_revision >= revision + 1 + extra
        });
        let out = set.pump(&dir, true);
        let notes = &set.get(tab).unwrap().notes;
        assert_eq!(
            notes.prompt_notes.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["bg", "live"]
        );
        if overflow {
            assert!(out
                .active_payload
                .iter()
                .any(|p| serde_json::from_str::<serde_json::Value>(p).unwrap()["kind"] == "snapshot"));
        }
        shut_down_all(&mut set);
    }
}

#[test]
fn eviction_delivery_keeps_new_notes_before_their_call_starts() {
    let dir = workspace("eviction-new-notes");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    set.get_mut(tab).unwrap().mode = SessionModeChoice::Bypass;
    set.get_mut(tab).unwrap().session.note_ledger.record(
        "late-rule".into(),
        NoteCandidate::Rule {
            rule: "Bash(npm ci *)".into(),
            call: CandidateCall {
                tool_name: "Bash".into(),
                input: serde_json::json!({"command":"npm ci"}),
            },
        },
    );
    set.get_mut(tab)
        .unwrap()
        .notes
        .prompt_notes
        .insert("gone".into(), "old".into());
    let revision = set.get(tab).unwrap().live().unwrap().projection().last_revision;
    provider.queue_all(vec![
        AgentDomainEvent::HistoryEvicted {
            through_seq: revision,
            turns: 1,
            lifted: vec![],
        },
        started("t1"),
        AgentDomainEvent::PermissionRequested {
            permission_id: "perm-new".into(),
            tool_use_id: Some("late-write".into()),
            tool_name: "Write".into(),
            input: serde_json::json!({"file_path":"fresh.rs","content":"new"}),
            provider_prompt: None,
        },
        call_done("late-rule", false),
    ]);
    // Wait for the whole queued batch before draining, so the cut and notes share one delivery.
    let deadline = Instant::now() + Duration::from_secs(60);
    while set.get(tab).unwrap().live().unwrap().projection().last_revision < revision + 4 {
        assert!(Instant::now() < deadline, "batch did not fold");
        std::thread::yield_now();
    }
    let out = set.pump(&dir, true);
    let notes = &set.get(tab).unwrap().notes;
    assert!(notes.auto_creates_file.contains("late-write"));
    assert_eq!(
        notes.allowed_by_rule.get("late-rule").map(String::as_str),
        Some("Bash(npm ci *)")
    );
    assert!(!notes.prompt_notes.contains_key("gone"));
    let payloads: Vec<serde_json::Value> = out
        .active_payload
        .iter()
        .map(|p| serde_json::from_str(p).unwrap())
        .collect();
    assert!(payloads
        .iter()
        .any(|p| p["autoCreatesFile"] == serde_json::json!(["late-write"])));
    assert!(payloads
        .iter()
        .any(|p| p["ruleNotes"] == serde_json::json!([{"toolUseId":"late-rule","rule":"Bash(npm ci *)"}])));
    provider.queue_all(vec![
        call_started(
            "late-write",
            "Write",
            serde_json::json!({"file_path":"fresh.rs","content":"new"}),
        ),
        call_started("late-rule", "Bash", serde_json::json!({"command":"npm ci"})),
    ]);
    while set.get(tab).unwrap().live().unwrap().projection().tool_calls.len() < 2 {
        assert!(Instant::now() < deadline, "call starts did not fold");
        std::thread::yield_now();
    }
    set.pump(&dir, true);
    assert_eq!(
        call_in_snapshot(&mut set, "late-write")["createsFile"],
        serde_json::json!(true)
    );
    assert_eq!(
        call_in_snapshot(&mut set, "late-rule")["allowedByRule"],
        serde_json::json!("Bash(npm ci *)")
    );
    shut_down_all(&mut set);
}
