//! Tab commands and envelopes: task and setting commands, new tabs, following, the tabs list, the chooser, the phase-three messages and proposals, the context line and capabilities.
#![cfg(test)]

use super::*;

#[test]
fn task_commands_parse_and_the_output_reply_keeps_its_request_tab_and_task() {
    for kind in ["stop_task", "task_output"] {
        let raw = json!({"type": kind, "request_id": "q1", "tab": 4, "task_id": "task"});
        let message = parse_inbound_message(&raw.to_string()).unwrap();
        assert_eq!(message.request_id(), "q1");
        assert_eq!(message.tab_ref(), TabRef::Named(TabId(4)));
        assert!(
            matches!(message, InboundMessage::StopTask { task_id, .. } | InboundMessage::TaskOutput { task_id, .. } if task_id == "task")
        );
        let missing = json!({"type": kind, "request_id": "q1", "task_id": "task"});
        assert_eq!(
            parse_inbound_message(&missing.to_string()).unwrap().tab_ref(),
            TabRef::Missing
        );
        assert!(parse_inbound_message(&json!({"type": kind, "request_id": "q1", "tab": 4}).to_string()).is_none());
    }
    let output = agent::TaskOutput {
        output: "�<b>plain</b>".into(),
        total_bytes: 9000,
        truncated: true,
        offset: 1000,
    };
    let wire: Value = serde_json::from_str(&serialize_task_output_for_js("q1", TabId(4), "task", &output)).unwrap();
    assert_eq!(
        wire,
        json!({"kind": "task_output_result", "requestId": "q1", "tab": 4, "taskId": "task",
        "output": "�<b>plain</b>", "totalBytes": 9000, "truncated": true, "offset": 1000})
    );
    let ranged = json!({"type": "task_output", "request_id": "q1", "tab": 4, "task_id": "t", "offset": 12});
    assert!(matches!(
        parse_inbound_message(&ranged.to_string()).unwrap(),
        InboundMessage::TaskOutput { offset: Some(12), .. }
    ));
}

#[test]
fn the_direct_settings_parse_with_their_tab_and_their_replies_keep_it() {
    for raw in [
        json!({"type": "list_models", "request_id": "q1", "tab": 4}),
        json!({"type": "set_model", "request_id": "q1", "tab": 4, "model": "sonnet"}),
        json!({"type": "apply_effort", "request_id": "q1", "tab": 4, "effort": "low"}),
    ] {
        let message = parse_inbound_message(&raw.to_string()).unwrap();
        assert_eq!(message.request_id(), "q1");
        assert_eq!(message.tab_ref(), TabRef::Named(TabId(4)));
    }
    let applied = agent::AppliedSettings {
        model: "claude-sonnet-5-5".into(),
        effort: "low".into(),
    };
    let wire: Value = serde_json::from_str(&serialize_applied_for_js("q1", TabId(4), &applied)).unwrap();
    assert_eq!(
        wire,
        json!({"kind": "settings_result", "requestId": "q1", "tab": 4,
            "applied": {"model": "claude-sonnet-5-5", "effort": "low"}})
    );
    let list = agent::ModelList {
        models: vec![agent::ModelOption {
            value: "sonnet".into(),
            supports_auto_mode: true,
            supported_effort_levels: vec!["low".into()],
            ..Default::default()
        }],
        applied,
        efforts: Vec::new(),
    };
    let wire: Value = serde_json::from_str(&serialize_models_for_js("q1", TabId(4), &list)).unwrap();
    assert_eq!(wire["kind"], "models_result");
    assert_eq!(wire["models"][0]["supportsAutoMode"], true);
    assert_eq!(wire["models"][0]["supportedEffortLevels"][0], "low");
    assert_eq!(wire["applied"]["effort"], "low");
}

#[test]
fn a_tab_command_parses_and_says_which_tab_it_names() {
    let named = parse_inbound_message(r#"{"type":"send_message","request_id":"r1","tab":3,"text":"hi"}"#).unwrap();
    assert!(matches!(named.tab_ref(), TabRef::Named(TabId(3))));
    // Parsing preserves requestId so a request naming no tab can receive a refusal.
    let missing = parse_inbound_message(r#"{"type":"interrupt","request_id":"r2"}"#).unwrap();
    assert_eq!(missing.request_id(), "r2");
    assert!(matches!(missing.tab_ref(), TabRef::Missing));
    let window = parse_inbound_message(r#"{"type":"ready","request_id":"r3"}"#).unwrap();
    assert!(matches!(window.tab_ref(), TabRef::WindowLevel));
    let permission = parse_inbound_message(
        r#"{"type":"permission_response","request_id":"r4","tab":1,"permission_id":"p","decision":"allow"}"#,
    )
    .unwrap();
    assert!(matches!(permission.tab_ref(), TabRef::Named(TabId(1))));
}

/// The confirmed stop of a tab's background names its tab and the count its question showed; one
/// that names no tab is refused like every other tab command, and one without the count does not
/// parse, so no answer is taken for a question whose count is unknown.
#[test]
fn a_stop_of_the_background_names_its_tab_and_the_count_it_was_asked_about() {
    let named = parse_inbound_message(r#"{"type":"stop_background","request_id":"s1","tab":4,"running":2}"#).unwrap();
    assert!(matches!(named.tab_ref(), TabRef::Named(TabId(4))));
    assert_eq!(named.request_id(), "s1");
    assert!(matches!(named, InboundMessage::StopBackground { running: 2, .. }));
    let missing = parse_inbound_message(r#"{"type":"stop_background","request_id":"s2","running":1}"#).unwrap();
    assert!(matches!(missing.tab_ref(), TabRef::Missing));
    assert!(parse_inbound_message(r#"{"type":"stop_background","request_id":"s3","tab":4}"#).is_none());
}

#[test]
fn the_new_tab_messages_parse() {
    for (json, want) in [
        (r#"{"type":"select_tab","request_id":"a","tab":2}"#, "select_tab"),
        (
            r#"{"type":"rename_tab","request_id":"a","tab":2,"name":"docs"}"#,
            "rename_tab",
        ),
        (r#"{"type":"close_tab","request_id":"a","tab":2}"#, "close_tab"),
        (r#"{"type":"reset_tab","request_id":"a","tab":2}"#, "reset_tab"),
        (r#"{"type":"cycle_mode","request_id":"a","tab":2}"#, "cycle_mode"),
        (r#"{"type":"open_detail","request_id":"a","tab":2}"#, "open_detail"),
        (
            r#"{"type":"resume","request_id":"a","tab":2,"provider_session_id":"c-1"}"#,
            "resume",
        ),
    ] {
        let message = parse_inbound_message(json).unwrap_or_else(|| panic!("{want} did not parse"));
        assert!(matches!(message.tab_ref(), TabRef::Named(TabId(2))), "{want}");
    }
    assert!(
        parse_inbound_message(r#"{"type":"start_session","request_id":"a","mode":"auto"}"#).is_none(),
        "start_session is gone: the first message starts the session"
    );
}

#[test]
fn every_session_envelope_carries_its_tab() {
    let tab = TabId(7);
    let events: serde_json::Value = serde_json::from_str(&serialize_events_for_js(tab, 1, 2, &[])).unwrap();
    assert_eq!(events["tab"], 7);
    let error: serde_json::Value = serde_json::from_str(&serialize_error_for_js(tab, "gone")).unwrap();
    assert_eq!(
        error,
        serde_json::json!({ "kind": "error", "tab": 7, "message": "gone" })
    );
    let focus: serde_json::Value = serde_json::from_str(&serialize_focus_permission_for_js(tab)).unwrap();
    assert_eq!(focus, serde_json::json!({ "kind": "focus_permission", "tab": 7 }));
    let rename: serde_json::Value = serde_json::from_str(&serialize_begin_rename_for_js(tab, Some("docs"))).unwrap();
    assert_eq!(
        rename,
        serde_json::json!({ "kind": "begin_rename", "tab": 7, "current": "docs" })
    );
    let confirm: serde_json::Value = serde_json::from_str(&serialize_confirm_close_for_js(
        tab,
        &["close 1 \"new\"? (y/n)".to_string()],
    ))
    .unwrap();
    assert_eq!(confirm["lines"][0], "close 1 \"new\"? (y/n)");
    let command = agent::handoff::ClaudeResumeCommand::for_session("/home/user/project", "1857dcd5-973b-46a2").unwrap();
    let handoff: serde_json::Value = serde_json::from_str(&serialize_handoff_for_js(tab, &command)).unwrap();
    assert_eq!(handoff["tab"], 7);
    assert_eq!(handoff["providerSessionId"], "1857dcd5-973b-46a2");
}

#[test]
fn toggle_following_parses_and_names_its_tab() {
    let message: InboundMessage =
        serde_json::from_str(r#"{"type":"toggle_following","request_id":"follow-1","tab":3}"#).unwrap();
    assert!(matches!(message, InboundMessage::ToggleFollowing { tab: Some(3), .. }));
    assert_eq!(message.request_id(), "follow-1");
    assert!(matches!(message.tab_ref(), TabRef::Named(crate::tabs::TabId(3))));
}

#[test]
fn a_following_tab_says_so_and_another_says_nothing() {
    let mut tabs = crate::tab_set::TabSet::new(crate::agent_backend::BackendKind::Sidecar, SessionModeChoice::Auto);
    let first = tabs.active();
    tabs.get_mut(first).unwrap().following = true;
    tabs.open(crate::agent_backend::AgentId::Claude);
    let value: Value = serde_json::from_str(&tabs.tabs_payload()).unwrap();
    assert_eq!(value["tabs"][0]["following"], true);
    assert!(value["tabs"][1].get("following").is_none());
}

#[test]
fn the_tabs_envelope_lists_every_tab_with_its_marker_and_the_active_one() {
    let tabs = [
        TabView {
            following: false,
            question_draft: Default::default(),
            plan_feedback_draft: Default::default(),
            plan_on_start: false,
            id: TabId(1),
            number: 1,
            label: "1 fix-parser".into(),
            name: Some("fix-parser".into()),
            state: TabStateWire::Live,
            mode: SessionModeChoice::Auto,
            marker: Some(Marker::NeedsInput(2)),
            pending: 2,
            resumable: true,
            failure: None,
            failed_after_start: false,
            resumes_on_r: false,
            title: Some("fix the parser".into()),
            agent: "claude".into(),
        },
        TabView {
            following: false,
            question_draft: Default::default(),
            plan_feedback_draft: Default::default(),
            plan_on_start: false,
            id: TabId(4),
            number: 2,
            label: "2 new".into(),
            name: None,
            state: TabStateWire::Failed,
            mode: SessionModeChoice::Bypass,
            marker: Some(Marker::Ended),
            pending: 0,
            resumable: false,
            failure: Some("the gate refused".into()),
            failed_after_start: true,
            resumes_on_r: true,
            title: None,
            agent: "codex".into(),
        },
    ];
    let value: serde_json::Value =
        serde_json::from_str(&serialize_tabs_for_js(TabId(4), &tabs, SessionModeChoice::Bypass)).unwrap();
    assert_eq!(value["kind"], "tabs");
    assert_eq!(value["active"], 4);
    assert_eq!(value["defaultMode"], "bypass");
    assert_eq!(
        value["tabs"][0],
        serde_json::json!({
            "id": 1, "number": 1, "label": "1 fix-parser", "name": "fix-parser", "state": "live",
            "mode": "auto", "marker": "needs_input", "pending": 2, "resumable": true, "failure": null,
            "failedAfterStart": false, "resumesOnR": false, "title": "fix the parser", "agent": "claude"
        })
    );
    assert_eq!(value["tabs"][1]["agent"], "codex");
    assert_eq!(value["tabs"][1]["failedAfterStart"], true);
    assert_eq!(value["tabs"][1]["resumesOnR"], true);
    assert_eq!(value["tabs"][1]["state"], "failed");
    assert_eq!(value["tabs"][1]["marker"], "ended");
    assert_eq!(value["tabs"][1]["name"], serde_json::Value::Null);
    assert_eq!(value["tabs"][1]["title"], serde_json::Value::Null);
}

#[test]
fn a_manual_tab_and_a_manual_default_are_sent_as_manual() {
    let tabs = vec![TabView {
        following: false,
        question_draft: Default::default(),
        plan_feedback_draft: Default::default(),
        plan_on_start: false,
        id: TabId(1),
        number: 1,
        label: "1 new".into(),
        name: None,
        state: TabStateWire::NotStarted,
        mode: SessionModeChoice::Manual,
        marker: None,
        pending: 0,
        resumable: false,
        failure: None,
        failed_after_start: false,
        resumes_on_r: false,
        title: None,
        agent: "claude".into(),
    }];
    let value: Value =
        serde_json::from_str(&serialize_tabs_for_js(TabId(1), &tabs, SessionModeChoice::Manual)).unwrap();
    assert_eq!(value["defaultMode"], "manual");
    assert_eq!(value["tabs"][0]["mode"], "manual");
}

#[test]
fn the_chooser_lists_open_tabs_then_records_and_marks_those_held_elsewhere() {
    let open = [ChooserTab {
        tab: TabId(1),
        label: "1 docs".into(),
        marker: None,
        pending: 0,
        resumable: true,
    }];
    let records = [ChooserRecord {
        provider_session_id: "c-9".into(),
        name: None,
        title: Some("fix the picker".into()),
        created_at: "1".into(),
        updated_at: "2".into(),
        held_elsewhere: true,
        held_by_partner: false,
        partner_window: None,
    }];
    let value: serde_json::Value = serde_json::from_str(&serialize_chooser_for_js(&open, &records, None)).unwrap();
    assert_eq!(value["kind"], "chooser");
    assert!(
        value.get("limit").is_none(),
        "no limit is named while the project is under it"
    );
    let full: serde_json::Value = serde_json::from_str(&serialize_chooser_for_js(&open, &records, Some(200))).unwrap();
    assert_eq!(full["limit"], 200);
    // The rows of a list that arrived later carry the same rows under their own kind, so a page that closed its
    // chooser meanwhile does not take them for a request to open one.
    let update: serde_json::Value =
        serde_json::from_str(&serialize_chooser_update_for_js(&open, &records, None)).unwrap();
    assert_eq!(update["kind"], "chooser_update");
    assert_eq!(update["open"], value["open"]);
    assert_eq!(update["records"], value["records"]);
    assert_eq!(value["open"][0]["tab"], 1);
    assert_eq!(value["open"][0]["marker"], serde_json::Value::Null);
    assert_eq!(value["records"][0]["providerSessionId"], "c-9");
    assert_eq!(value["records"][0]["heldElsewhere"], true);
    let detail: serde_json::Value = serde_json::from_str(&serialize_tab_detail_for_js(
        TabId(1),
        &[DetailRow {
            label: "account".into(),
            value: "work".into(),
        }],
    ))
    .unwrap();
    assert_eq!(
        detail,
        serde_json::json!({ "kind": "tab_detail", "tab": 1, "rows": [{ "label": "account", "value": "work" }] })
    );
}

#[test]
fn hello_carries_each_sessions_name_or_null() {
    let mut greeting = crate::agent_backend::BackendGreeting::for_kind(
        crate::agent_backend::BackendKind::Legacy,
        std::path::PathBuf::from("/home/user/project"),
    );
    greeting.resumable = vec![agent::ResumableSession {
        provider: "claude".into(),
        provider_session_id: "c-1".into(),
        created_at: "1".into(),
        updated_at: "2".into(),
        title: None,
        name: Some("docs".into()),
    }];
    let value: serde_json::Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
    assert_eq!(value["resumableSessions"][0]["name"], "docs");
}

#[test]
fn the_phase_three_messages_parse_and_name_their_tab_or_none() {
    for (json, tab_scoped) in [
        (
            r#"{"type":"queue_message","request_id":"a","tab":2,"text":"later"}"#,
            true,
        ),
        (r#"{"type":"take_back_queue","request_id":"a","tab":2}"#, true),
        (r#"{"type":"send_now","request_id":"a","tab":2,"text":""}"#, true),
        (
            r#"{"type":"draft","request_id":"a","tab":2,"text":"half a thou"}"#,
            true,
        ),
        (r#"{"type":"edit_draft","request_id":"a","tab":2,"text":"draft"}"#, true),
        (r#"{"type":"history_push","request_id":"a","text":"cleared"}"#, false),
        (
            r#"{"type":"open_path","request_id":"a","path":"src/main.rs","line":42}"#,
            false,
        ),
        (r#"{"type":"open_path","request_id":"a","path":"src/main.rs"}"#, false),
        (
            r#"{"type":"view_in_editor","request_id":"a","title":"Bash","text":"out"}"#,
            false,
        ),
        (
            r#"{"type":"view_proposal","request_id":"a","path":"a.rs","change":{"kind":"content","content":""}}"#,
            false,
        ),
    ] {
        let message = parse_inbound_message(json).unwrap_or_else(|| panic!("{json} did not parse"));
        assert_eq!(message.request_id(), "a");
        if tab_scoped {
            assert!(matches!(message.tab_ref(), TabRef::Named(TabId(2))), "{json}");
        } else {
            assert!(matches!(message.tab_ref(), TabRef::WindowLevel), "{json}");
        }
    }
    let untabbed = parse_inbound_message(r#"{"type":"queue_message","request_id":"b","text":"x"}"#).unwrap();
    assert!(matches!(untabbed.tab_ref(), TabRef::Missing), "never the active one");
}

/// `Ctrl+x Enter` sends `after_turn: true`; every other queued message leaves it out and reads false, and
/// the queue envelope says of each item whether it waits for the turn's end.
#[test]
fn queue_message_carries_after_turn_and_the_envelope_names_it() {
    let after = |json: &str| match parse_inbound_message(json).unwrap_or_else(|| panic!("{json} did not parse")) {
        InboundMessage::QueueMessage { after_turn, .. } => after_turn,
        other => panic!("{json} parsed as {other:?}"),
    };
    assert!(!after(
        r#"{"type":"queue_message","request_id":"a","tab":2,"text":"x"}"#
    ));
    assert!(!after(
        r#"{"type":"queue_message","request_id":"a","tab":2,"text":"x","turn_id":"t1","after_turn":false}"#
    ));
    assert!(after(
        r#"{"type":"queue_message","request_id":"a","tab":2,"text":"x","turn_id":null,"after_turn":true}"#
    ));

    use crate::tab_set::Queued;
    let mut held = Queued::held("after it", "after it".into(), 3);
    held.after_turn = true;
    let value: Value = serde_json::from_str(&serialize_queue_for_js(
        TabId(2),
        &[held, Queued::held("b", "b".into(), 4)],
        None,
        None,
        None,
    ))
    .unwrap();
    assert_eq!(value["items"][0]["afterTurn"], true);
    assert_eq!(value["items"][1]["afterTurn"], false);
}

/// A proposal crosses the bridge in either shape, its path kept byte for byte so the panel's refusal sees
/// what the agent sent; a shape this build does not know does not parse, and a path holding a control
/// character is one the panel refuses before looking at the disk.
#[test]
fn view_proposal_parses_and_refuses_a_control_character_path() {
    let edits = parse_inbound_message(
        r#"{"type":"view_proposal","request_id":"v","path":"src/a.rs","change":{"kind":"edits","edits":[{"oldText":"a","newText":"b","replaceAll":true}]}}"#,
    )
    .unwrap();
    match edits {
        InboundMessage::ViewProposal { path, change, .. } => {
            assert_eq!(path, "src/a.rs");
            assert_eq!(
                change,
                ProposedChange::Edits {
                    edits: vec![ProposedEdit {
                        old_text: "a".into(),
                        new_text: "b".into(),
                        replace_all: true,
                    }],
                    crlf_as_lf: false,
                }
            );
        }
        other => panic!("{other:?}"),
    }
    let claude = parse_inbound_message(
        r#"{"type":"view_proposal","request_id":"v","path":"a.rs","change":{"kind":"edits","edits":[{"oldText":"a","newText":"b","replaceAll":false}],"crlfAsLf":true}}"#,
    )
    .unwrap();
    assert!(matches!(
        claude,
        InboundMessage::ViewProposal {
            change: ProposedChange::Edits { crlf_as_lf: true, .. },
            ..
        }
    ));
    let content = parse_inbound_message(
        r#"{"type":"view_proposal","request_id":"v","path":"new.rs","change":{"kind":"content","content":"fn main() {}\n"}}"#,
    )
    .unwrap();
    assert!(matches!(
        content,
        InboundMessage::ViewProposal { change: ProposedChange::Content { ref content }, .. } if content == "fn main() {}\n"
    ));
    for unknown in [
        r#"{"type":"view_proposal","request_id":"v","path":"a.rs","change":{"kind":"delete"}}"#,
        r#"{"type":"view_proposal","request_id":"v","path":"a.rs","change":{"kind":"edits"}}"#,
        r#"{"type":"view_proposal","request_id":"v","path":"a.rs"}"#,
    ] {
        assert!(parse_inbound_message(unknown).is_none(), "{unknown}");
    }
    let hostile = parse_inbound_message(
        r#"{"type":"view_proposal","request_id":"v","path":"a\nlua(io.open)('x','w')","change":{"kind":"content","content":""}}"#,
    )
    .unwrap();
    let InboundMessage::ViewProposal { path, .. } = hostile else {
        panic!("a control character is data to the parser")
    };
    assert_eq!(path, "a\nlua(io.open)('x','w')");
    assert!(path_holds_control(&path));
    for c in ['\0', '\r', '\t', '\x1b', '\x7f'] {
        assert!(path_holds_control(&format!("a{c}b.rs")), "{c:?}");
    }
    assert!(!path_holds_control("src/main.rs"));
    assert!(!path_holds_control("/abs/naïve file.rs"));
}

/// What a name can do to whatever shows it, past the ASCII range: a C1 control is read as one by a terminal, a bidi
/// control reorders what the person reads, and the two separators end a line.
#[test]
fn a_control_past_ascii_is_a_control() {
    for c in [
        '\u{80}', '\u{85}', '\u{9b}', '\u{9f}', '\u{61c}', '\u{200e}', '\u{200f}', '\u{2028}', '\u{2029}', '\u{202a}',
        '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
    ] {
        assert!(path_holds_control(&format!("a{c}b.rs")), "{:x}", c as u32);
    }
    for name in [
        "src/日本語.rs",
        "a\u{a0}b.rs",
        "a\u{200d}b.rs",
        "café 🙂.rs",
        "\u{fffd}.rs",
    ] {
        assert!(!path_holds_control(name), "{name:?}");
    }
}

#[test]
fn remember_defaults_to_false_and_is_read_when_sent() {
    let plain = parse_inbound_message(
        r#"{"type":"permission_response","request_id":"r","tab":1,"permission_id":"p","decision":"allow"}"#,
    )
    .unwrap();
    assert!(matches!(
        plain,
        InboundMessage::PermissionResponse { remember: false, .. }
    ));
    let remembered = parse_inbound_message(
        r#"{"type":"permission_response","request_id":"r","tab":1,"permission_id":"p","decision":"allow","remember":true}"#,
    )
    .unwrap();
    assert!(matches!(
        remembered,
        InboundMessage::PermissionResponse { remember: true, .. }
    ));
}

#[test]
fn the_phase_three_envelopes_have_their_exact_shapes() {
    use crate::tab_set::{Queued, QueuedState};
    let tab = TabId(4);
    let mut steered = Queued::held("look at the parser too", "ctx\n\nlook at the parser too".into(), 12);
    steered.state = QueuedState::Steered {
        client_id: "eitri-4-1".into(),
        sent_at_ms: 12,
    };
    let queue = [
        steered,
        Queued::held("and the tests", "ctx\n\nand the tests".into(), 17),
    ];
    let value = |s: String| -> Value { serde_json::from_str(&s).unwrap() };
    assert_eq!(
        value(serialize_queue_for_js(
            tab,
            &queue,
            Some("refused"),
            Some("held until the turn ends: why"),
            Some(true)
        )),
        json!({ "kind": "queue", "tab": 4, "items": [
            { "text": "look at the parser too", "queuedAt": 12, "steered": true, "afterTurn": false },
            { "text": "and the tests", "queuedAt": 17, "steered": false, "afterTurn": false },
        ], "error": "refused", "note": "held until the turn ends: why", "enterSteers": true }),
        "the wire text (with its context) and the client id never go to the panel"
    );
    assert_eq!(
        value(serialize_draft_for_js(tab, "half")),
        json!({ "kind": "draft", "tab": 4, "text": "half" })
    );
    assert_eq!(
        value(serialize_late_draft_for_js(tab, "half")),
        json!({ "kind": "draft", "tab": 4, "text": "half", "late": true })
    );
    assert_eq!(
        value(serialize_queue_taken_for_js(tab, &["a".into(), "b".into()])),
        json!({ "kind": "queue_taken", "tab": 4, "texts": ["a", "b"] })
    );
    assert_eq!(
        value(serialize_history_for_js(&["old".into(), "new".into()])),
        json!({ "kind": "history", "entries": ["old", "new"] })
    );
    let mut offers = std::collections::BTreeMap::new();
    offers.insert(
        "perm-1".to_string(),
        agent::PrefixRule::parse("Bash(git push *)").unwrap(),
    );
    assert_eq!(
        value(serialize_rule_offers_for_js(tab, &offers)),
        json!({ "kind": "rule_offers", "tab": 4, "offers": { "perm-1": "git push *" } })
    );
    assert_eq!(
        value(serialize_editor_context_for_js(Some(&ContextSummary {
            file: "src/a.rs".into(),
            lines: Some((10, 20))
        }))),
        json!({ "kind": "editor_context", "file": "src/a.rs", "lines": [10, 20] })
    );
    assert_eq!(
        value(serialize_editor_context_for_js(None)),
        json!({ "kind": "editor_context", "file": null, "lines": null })
    );
    assert_eq!(
        value(serialize_scratch_for_js(tab, true)),
        json!({ "kind": "scratch", "tab": 4, "editing": true })
    );
    assert_eq!(
        value(serialize_notice_for_js("no such file")),
        json!({ "kind": "notice", "text": "no such file" })
    );
}

#[test]
fn the_context_line_names_a_project_file_relatively_and_only_a_real_selection() {
    use crate::editor_context::{EditorContext, Selection};
    let root = std::path::Path::new("/home/user/project");
    let inside = EditorContext {
        file: "/home/user/project/src/parser.rs".into(),
        selection: None,
        edit_request: None,
    };
    assert_eq!(
        context_summary(Some(&inside), root),
        Some(ContextSummary {
            file: "src/parser.rs".into(),
            lines: None
        })
    );
    let selected = EditorContext {
        file: "/etc/hosts".into(),
        selection: Some(Selection {
            start_line: 3,
            end_line: 9,
            text: "x".into(),
        }),
        edit_request: None,
    };
    assert_eq!(
        context_summary(Some(&selected), root),
        Some(ContextSummary {
            file: "/etc/hosts".into(),
            lines: Some((3, 9))
        })
    );
    let unnamed = EditorContext {
        file: String::new(),
        selection: None,
        edit_request: None,
    };
    assert_eq!(
        context_summary(Some(&unnamed), root),
        None,
        "a buffer with no file says nothing"
    );
    assert_eq!(context_summary(None, root), None);
}

/// The snapshot omits `modeSwitch` because the CLI mode never switches. Absence keeps a panel
/// from mistaking a false capability value for a live response about mode switching.
#[test]
fn a_snapshot_carries_no_mode_switch_capability() {
    let projection = AgentSessionProjection::default();
    let view = SnapshotView {
        backend: "sidecar",
        conversation_id: None,
        session_id: None,
        provider_session_id: None,
        capabilities: agent::ProviderCapabilities {
            interactive_permission_mode: true,
            bypass_permission_mode: true,
            ..Default::default()
        },
        provider: None,
        projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
        hidden_pending: None,
    };
    let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
    let capabilities = parsed["state"]["capabilities"].as_object().unwrap();
    assert!(!capabilities.contains_key("modeSwitch"), "{capabilities:?}");
}

#[test]
fn a_snapshot_carries_each_offered_task_capability() {
    let projection = AgentSessionProjection::default();
    for offered in [false, true] {
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities {
                background_tasks: offered,
                task_control: offered,
                task_output: offered,
                workflow_progress: offered,
                ..Default::default()
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
            hidden_pending: None,
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(TabId(1), &view, None)).unwrap();
        for field in ["backgroundTasks", "taskControl", "taskOutput", "workflowProgress"] {
            assert_eq!(
                parsed["state"]["capabilities"][field].as_bool().unwrap_or(false),
                offered
            );
        }
    }
}

#[test]
fn hello_carries_what_the_launch_may_bring_back_or_null() {
    let greeting = greeting_with(Vec::new());
    let value: Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
    assert!(value["restore"].is_null(), "present, and null when nothing is on offer");
    let offer = crate::tab_restore::RestoreOffer {
        labels: vec!["api".to_string(), "docs".to_string()],
        bypass: 1,
        cut_note: Some("a turn was cut off".to_string()),
    };
    let value: Value = serde_json::from_str(&serialize_hello_with_restore_for_js(&greeting, Some(&offer))).unwrap();
    assert_eq!(
        value["restore"],
        json!({ "labels": ["api", "docs"], "bypass": 1, "cutNote": "a turn was cut off" })
    );
    assert_eq!(value["kind"], "hello", "the rest of the hello is the same");
    let none: Value = serde_json::from_str(&serialize_hello_with_restore_for_js(&greeting, None)).unwrap();
    assert!(none["restore"].is_null());
}
