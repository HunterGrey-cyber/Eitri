//! A background tab's trace, a tab's own draft across a switch, a scratch edit, and a CLI reporting an ungated mode.
#![cfg(test)]

use super::*;

/// Turn traces must be observed for background tabs as well as the active tab.
#[test]
fn a_background_tabs_trace_is_observed_marked_and_emitted() {
    let dir = workspace("tabs-bg-trace");
    let mut set = set();
    let background = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(background).unwrap().backend = TabBackend::Live(backend);
    set.get_mut(background).unwrap().session.turn_trace = Some(crate::turn_trace::TurnTrace::started_now());
    set.open(AgentId::Claude);
    provider.queue(started("t1"));
    provider.queue(AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: agent::ContentKind::Text,
        text: "hi".into(),
    });
    provider.queue(completed("t1", agent::TurnOutcome::Completed));
    until("the background turn to end", || {
        set.pump(&dir, true);
        !set.get(background).unwrap().turn_running()
            && set
                .get(background)
                .unwrap()
                .session
                .turn_trace
                .as_ref()
                .is_some_and(crate::turn_trace::TurnTrace::is_finished)
    });
    let trace = set.get(background).unwrap().session.turn_trace.as_ref().unwrap();
    assert!(trace.line().contains("first_paint_frame=bg"), "{}", trace.line());
    assert!(
        trace.is_emitted(),
        "the pump emits a background trace: nothing else will"
    );
    shut_down_all(&mut set);
}

/// Nothing previously pinned how `pump` feeds a drained batch into the turn trace. A turn that
/// folds entirely before the very first tick sees it (the double-delivery race's shape) is delivered to the
/// active tab wholly `in_snapshot`, via `active_state_payloads`, and never reaches the panel as
/// `events` at all -- so no paint report is ever coming for it, and `observe_in_snapshot` (not
/// plain `observe`) is what lets the trace complete anyway (`first_paint_frame=snapshot`).
/// `emit`'s own gate is `!dispatched`, not `!is_active`: this tab IS active and never dispatches
/// (the whole batch was already shown), so a gate keyed on `is_active` would never print it.
#[test]
fn a_turn_folded_entirely_inside_a_snapshot_still_completes_and_prints_its_trace() {
    let dir = workspace("tabs-trace-in-snapshot");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    set.get_mut(tab).unwrap().session.turn_trace = Some(crate::turn_trace::TurnTrace::started_now());

    // The whole turn folds with no tick draining it -- the ingestion thread folds on its own,
    // exactly as in the double-delivery races.
    provider.queue(started("t1"));
    provider.queue(text_delta("t1", "hi"));
    provider.queue(completed("t1", agent::TurnOutcome::Completed));
    wait_for_fold(&set, tab, "hi");

    // Reads the snapshot and notes its revision on the backend -- the watermark the next
    // pump's `covered` filter reads.
    let snapshot = the_snapshot(set.active_state_payloads());
    assert!(snapshot_text(&snapshot).contains("hi"), "{snapshot}");

    // The next pump drains the identical batch the snapshot already carried: `shown` covers
    // all of it, `fresh` is empty, nothing dispatches.
    let out = set.pump(&dir, true);
    assert!(
        out.active_payload.is_none(),
        "the whole batch was already in the snapshot"
    );

    let trace = set.get(tab).unwrap().session.turn_trace.as_ref().unwrap();
    assert!(
        trace.is_emitted(),
        "a turn folded entirely inside a snapshot must still be printed: nothing else will"
    );
    assert!(trace.line().contains("first_paint_frame=snapshot"), "{}", trace.line());
    shut_down_all(&mut set);
}

/// Every tab's own queue, draft and offers follow its snapshot on a switch, so the panel never
/// shows the tab it left.
#[test]
fn a_switch_sends_the_tabs_own_queue_draft_and_offers() {
    let dir = workspace("tabs-switch-payloads");
    let mut set = set();
    let first = set.active();
    let (_provider, backend) = live(&dir);
    set.get_mut(first).unwrap().backend = TabBackend::Live(backend);
    set.queue_message(first, "queued", "queued".into(), 5, true, agent::PromptOrigin::Typed)
        .unwrap();
    set.set_draft(first, "typing");
    let second = set.open(AgentId::Claude);
    let kinds = |payloads: Vec<String>| -> Vec<(String, serde_json::Value)> {
        payloads
            .into_iter()
            .map(|p| {
                let v: serde_json::Value = serde_json::from_str(&p).unwrap();
                (v["kind"].as_str().unwrap().to_string(), v)
            })
            .collect()
    };
    let empty = kinds(set.active_state_payloads());
    assert_eq!(
        empty.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        vec!["queue", "draft", "scratch"],
        "an empty tab still resets the panel's queue and draft"
    );
    assert!(empty.iter().all(|(_, v)| v["tab"] == second.0));
    set.select(first);
    let live_payloads = kinds(set.active_state_payloads());
    assert_eq!(
        live_payloads.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        vec!["snapshot", "queue", "draft", "rule_offers", "scratch"]
    );
    assert_eq!(live_payloads[1].1["items"][0]["text"], "queued");
    assert_eq!(live_payloads[2].1["text"], "typing");
    shut_down_all(&mut set);
}

/// Sending the first message from an empty tab clears its mirrored draft, so state delivered
/// after connection does not put the sent text back into the new conversation's composer.
#[test]
fn a_sent_prompt_is_no_longer_the_tabs_draft() {
    let mut set = set();
    let tab = set.active();
    set.set_draft(tab, "STREAM2 abc");
    set.note_sent(tab);
    let draft = set
        .active_state_payloads()
        .into_iter()
        .map(|p| serde_json::from_str::<serde_json::Value>(&p).unwrap())
        .find(|v| v["kind"] == "draft")
        .expect("a draft payload");
    assert_eq!(draft["text"], "");
}

/// A draft edited in nvim returns to its originating tab even after a tab switch.
#[test]
fn a_scratch_edit_returns_to_its_own_tab() {
    let mut set = set();
    let first = set.active();
    set.set_draft(first, "before");
    set.begin_scratch_edit(first, 7).unwrap();
    assert!(set.begin_scratch_edit(first, 8).is_err(), "one edit per tab at a time");
    let second = set.open(AgentId::Claude);
    set.set_draft(second, "second's own");
    let (tab, draft) = set
        .finish_scratch_edit(7, &crate::scratch::EditDone::Written("after".into()))
        .expect("edit 7 belongs to a tab");
    assert_eq!((tab, draft.as_deref()), (first, Some("after")));
    assert_eq!(set.get(first).unwrap().session.draft, "after");
    assert_eq!(
        set.get(second).unwrap().session.draft,
        "second's own",
        "never the active tab"
    );
    assert_eq!(set.get(first).unwrap().editing_draft, None);
    assert!(
        set.finish_scratch_edit(7, &crate::scratch::EditDone::Discarded)
            .is_none(),
        "already finished"
    );

    set.begin_scratch_edit(second, 9).unwrap();
    let (_, draft) = set
        .finish_scratch_edit(9, &crate::scratch::EditDone::Discarded)
        .unwrap();
    assert_eq!(draft, None, ":q! changes nothing");
    assert_eq!(set.get(second).unwrap().session.draft, "second's own");
}

#[test]
fn cancel_scratch_edits_takes_a_saved_body_and_keeps_the_draft_otherwise() {
    use crate::scratch::EditDone;
    let mut set = set();
    let changed = set.active();
    let identical = set.open(AgentId::Claude);
    let discarded = set.open(AgentId::Claude);
    let silent = set.open(AgentId::Claude);
    set.set_draft(changed, "old");
    set.set_draft(identical, "same\n");
    set.set_draft(discarded, "kept");
    set.set_draft(silent, "also kept");
    // Opened out of order: the answer is ordered by edit id.
    set.begin_scratch_edit(silent, 40).unwrap();
    set.begin_scratch_edit(changed, 10).unwrap();
    set.begin_scratch_edit(discarded, 30).unwrap();
    set.begin_scratch_edit(identical, 20).unwrap();
    let out = set.cancel_scratch_edits(&[
        (10, EditDone::Written("new".into())),
        // The shell strips one trailing newline off a body; the draft still has it.
        (20, EditDone::Written("same".into())),
        (30, EditDone::Discarded),
    ]);
    assert_eq!(
        out,
        vec![
            (changed, Some("new".to_string())),
            (identical, None),
            (discarded, None),
            (silent, None),
        ]
    );
    assert_eq!(set.get(changed).unwrap().session.draft, "new");
    assert_eq!(set.get(identical).unwrap().session.draft, "same\n");
    assert_eq!(set.get(discarded).unwrap().session.draft, "kept");
    assert_eq!(set.get(silent).unwrap().session.draft, "also kept");
    for tab in [changed, identical, discarded, silent] {
        assert_eq!(set.get(tab).unwrap().editing_draft, None);
    }
}

#[test]
fn after_cancel_ctrl_g_works_again() {
    let mut set = set();
    let tab = set.active();
    set.begin_scratch_edit(tab, 7).unwrap();
    assert!(set.begin_scratch_edit(tab, 12).is_err());
    set.cancel_scratch_edits(&[]);
    assert!(set.begin_scratch_edit(tab, 12).is_ok());
}

// A CLI reporting an ungated mode closes the session.

/// `UngatedCliMode` fails the tab with a reason naming the setting and mode, returns its backend
/// for shutdown, and delivers none of that batch. A queued Write cannot become an approvable
/// card; shutdown denies it instead.
#[test]
fn a_cli_reporting_an_ungated_mode_closes_the_session() {
    let dir = workspace("tabs-ungated-cli-mode");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

    provider.queue(AgentDomainEvent::UngatedCliMode {
        reported: "acceptEdits".into(),
        detail: "SessionReady".into(),
    });
    provider.queue(write("behind-the-trip"));

    let mut tripped = Vec::new();
    let mut payloads = Vec::new();
    until("the trip", || {
        let out = set.pump(&dir, true);
        payloads.extend(out.active_payload);
        tripped.extend(out.tripped);
        !tripped.is_empty()
    });
    let state = set.get(tab).unwrap();
    let TabBackend::Failed { reason } = &state.backend else {
        panic!("the tab must be failed");
    };
    assert!(reason.contains("permissions.defaultMode"), "{reason}");
    assert!(reason.contains("'acceptEdits'"), "{reason}");
    assert_eq!(state.wire_state(), TabStateWire::Failed);
    assert_eq!(state.attention.attention().pending, 0, "no card is counted as waiting");

    // Handed out, and exactly once, for the caller to shut down -- which closes the session.
    assert_eq!(tripped.len(), 1);
    assert_eq!(tripped[0].tab, tab);
    assert_eq!(&tripped[0].reason, reason);
    assert_eq!(
        provider.closes(),
        0,
        "the pump shuts nothing down on the GTK thread itself"
    );
    for t in tripped {
        t.retired.into_backend().shutdown();
    }
    assert_eq!(provider.closes(), 1);

    // Nothing more arrives for it, and nothing that did named the Write.
    for _ in 0..5 {
        let out = set.pump(&dir, true);
        assert!(out.tripped.is_empty());
        payloads.extend(out.active_payload);
    }
    for payload in &payloads {
        assert!(
            !payload.contains("behind-the-trip"),
            "a card could have been drawn: {payload}"
        );
    }
    assert!(
        provider.resolutions().is_empty(),
        "nothing was answered on the host's side"
    );
    shut_down_all(&mut set);
}

/// A `PolicyMismatch` from another agent closes the tab the way the ungated mode does: the
/// reason carries both sides, the backend is handed out for the shutdown, the batch it came in is
/// withheld (the `Write` behind it never becomes a card) and nothing is answered on the host's
/// side.
#[test]
fn policy_mismatch_withholds_the_batch_and_fails_the_tab() {
    let dir = workspace("tabs-policy-mismatch");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

    provider.queue(AgentDomainEvent::PolicyMismatch {
        requested: "sandbox workspaceWrite".into(),
        reported: "sandbox dangerFullAccess".into(),
    });
    provider.queue(write("behind-the-mismatch"));

    let mut tripped = Vec::new();
    let mut payloads = Vec::new();
    until("the trip", || {
        let out = set.pump(&dir, true);
        payloads.extend(out.active_payload);
        tripped.extend(out.tripped);
        !tripped.is_empty()
    });
    let state = set.get(tab).unwrap();
    let TabBackend::Failed { reason } = &state.backend else {
        panic!("the tab must be failed");
    };
    assert!(reason.contains("sandbox dangerFullAccess"), "{reason}");
    assert!(reason.contains("sandbox workspaceWrite"), "{reason}");
    assert_eq!(state.wire_state(), TabStateWire::Failed);
    assert_eq!(state.attention.attention().pending, 0, "no card is counted as waiting");

    assert_eq!(tripped.len(), 1);
    assert_eq!(tripped[0].tab, tab);
    assert_eq!(&tripped[0].reason, reason);
    assert_eq!(
        provider.closes(),
        0,
        "the pump shuts nothing down on the GTK thread itself"
    );
    for t in tripped {
        t.retired.into_backend().shutdown();
    }
    assert_eq!(provider.closes(), 1);

    for _ in 0..5 {
        let out = set.pump(&dir, true);
        assert!(out.tripped.is_empty());
        payloads.extend(out.active_payload);
    }
    for payload in &payloads {
        assert!(
            !payload.contains("behind-the-mismatch"),
            "a card could have been drawn: {payload}"
        );
    }
    assert!(
        provider.resolutions().is_empty(),
        "nothing was answered on the host's side"
    );
    shut_down_all(&mut set);
}

/// What the page is sent for a tab a tripwire failed, and for one whose start never happened:
/// the `error` envelope with the reason, and a `tabs` envelope that says whether the session
/// had been open (`failedAfterStart`), so the page can word the two differently and keep the
/// conversation of the first. `agent-ui/web/src/fixtures/failed-tabs.json` records both for the
/// page's tests. Compared on every run; `EITRI_WRITE_FIXTURES=1` rewrites it.
#[test]
fn a_failed_tab_says_whether_its_session_had_been_open_and_the_fixture_records_it() {
    let dir = workspace("tabs-failed-after-start");
    let mut set = set();
    let tripped_tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tripped_tab).unwrap().backend = TabBackend::Live(backend);
    let never_started_tab = set.open(AgentId::Claude);
    provider.queue(AgentDomainEvent::PolicyMismatch {
        requested: "sandbox workspaceWrite".into(),
        reported: "sandbox dangerFullAccess".into(),
    });
    let mut tripped = Vec::new();
    until("the trip", || {
        tripped.extend(set.pump(&dir, true).tripped);
        !tripped.is_empty()
    });
    let reason = tripped[0].reason.clone();
    for t in tripped {
        t.retired.into_backend().shutdown();
    }
    {
        let tab = set.get_mut(never_started_tab).unwrap();
        tab.failed_after_start = false;
        tab.backend = TabBackend::Failed {
            reason: "failed to start session: codex not found".into(),
        };
    }

    let tabs_of = |active: TabId, set: &TabSet| -> serde_json::Value {
        let mut value: serde_json::Value = serde_json::from_str(&set.tabs_payload()).unwrap();
        value["active"] = serde_json::json!(active.0);
        value
    };
    let tripped_tabs = tabs_of(tripped_tab, &set);
    let by_id = |id: TabId| {
        tripped_tabs["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == id.0)
            .unwrap()
            .clone()
    };
    assert_eq!(by_id(tripped_tab)["state"], "failed");
    assert_eq!(by_id(tripped_tab)["failure"], reason.as_str());
    assert_eq!(by_id(tripped_tab)["failedAfterStart"], true);
    assert_eq!(by_id(never_started_tab)["state"], "failed");
    assert_eq!(by_id(never_started_tab)["failedAfterStart"], false);

    let fixture = serde_json::json!({
        "writtenBy": "core/src/tab_set.rs, tests::a_failed_tab_says_whether_its_session_had_been_open_and_the_fixture_records_it \
                      -- compared on every run; EITRI_WRITE_FIXTURES=1 rewrites it",
        "trippedTab": tripped_tab.0,
        "neverStartedTab": never_started_tab.0,
        "error": serde_json::from_str::<serde_json::Value>(&crate::agent_bridge::serialize_error_for_js(tripped_tab, &reason)).unwrap(),
        "tabs": tripped_tabs,
    });
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/failed-tabs.json");
    let written = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
    let committed = std::fs::read_to_string(&path).ok();
    if committed.as_deref() != Some(written.as_str()) {
        if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
            std::fs::write(&path, &written).expect("write the failed tabs fixture");
        } else {
            assert_eq!(
                committed.as_deref(),
                Some(written.as_str()),
                "the committed failed tabs fixture is not what this test produces; if the change is \
                 intended, regenerate it with EITRI_WRITE_FIXTURES=1"
            );
        }
    }
    shut_down_all(&mut set);
}

/// The report survives a `Resync`: a UI queue that overflowed drops its events, the tripwire
/// among them, and the projection's record is what still closes the session.
#[test]
fn a_trip_lost_to_a_resync_still_closes_the_session() {
    let dir = workspace("tabs-ungated-cli-mode-resync");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    provider.queue(AgentDomainEvent::UngatedCliMode {
        reported: "bypassPermissions".into(),
        detail: "PermissionModeChanged".into(),
    });
    // More than the UI queue holds, so the first delivery is a `Resync`.
    for _ in 0..(agent::UI_EVENT_QUEUE_CAPACITY + 20) {
        provider.open_session("claude-1", &dir);
    }
    until("an overflowed UI queue", || {
        let AgentBackend::Conversation { conversation, .. } = &backend else {
            unreachable!("`live` builds a sidecar")
        };
        let stats = conversation.ingest_stats();
        stats.resyncs >= 1 && stats.events_ingested as usize > agent::UI_EVENT_QUEUE_CAPACITY + 20
    });
    assert!(
        backend.projection().ungated_cli_mode.is_some(),
        "the report was folded, and only the projection still has it"
    );
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

    let out = set.pump(&dir, true);
    assert_eq!(out.tripped.len(), 1, "the first pump already closes it");
    assert!(matches!(set.get(tab).unwrap().backend, TabBackend::Failed { .. }));
    for t in out.tripped {
        t.retired.into_backend().shutdown();
    }
    shut_down_all(&mut set);
}

/// `Tab::mode` is the only authority: a `PermissionModeChanged` reaching the pump, which only
/// a report naming `default` can do, must not change it.
#[test]
fn a_reported_mode_change_moves_nothing() {
    let dir = workspace("tabs-mode-report-moves-nothing");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    set.get_mut(tab).unwrap().mode = SessionModeChoice::Bypass;

    provider.queue(AgentDomainEvent::PermissionModeChanged {
        mode: agent::PermissionMode::Auto,
        provider_mode: "default".into(),
        floor_applied: false,
    });
    provider.queue(AgentDomainEvent::SessionClosed { reason: "x".into() });
    until("the session's end", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().wire_state() == TabStateWire::Ended
    });
    assert_eq!(set.get(tab).unwrap().mode, SessionModeChoice::Bypass);
    shut_down_all(&mut set);
}
