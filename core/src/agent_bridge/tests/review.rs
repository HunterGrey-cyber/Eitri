//! The review messages and envelopes: requests, diffs, drafts, the send preview, recovery and the review's own commands.
#![cfg(test)]

use super::*;

#[test]
fn review_request_deserializes() {
    use crate::turn_review::{Scope, TurnRef};
    let parse = |json: &str| parse_inbound_message(json).unwrap_or_else(|| panic!("{json} did not parse"));
    for (turn_json, turn, scope_json, scope) in [
        (r#""latest""#, TurnRef::Latest, "turn", Scope::Turn),
        ("3", TurnRef::N(3), "session", Scope::Session),
        ("0", TurnRef::N(0), "turn", Scope::Turn),
    ] {
        let message = parse(&format!(
            r#"{{"type":"review_request","request_id":"r1","tab":3,"turn":{turn_json},"scope":"{scope_json}"}}"#
        ));
        assert_eq!(message.request_id(), "r1");
        assert!(matches!(message.tab_ref(), TabRef::Named(TabId(3))));
        let InboundMessage::ReviewRequest {
            turn: got_turn,
            scope: got_scope,
            ..
        } = message
        else {
            panic!("not a review_request")
        };
        assert_eq!(TurnRef::from(got_turn), turn);
        assert_eq!(Scope::from(got_scope), scope);
    }
}

#[test]
fn a_review_request_with_an_unknown_turn_or_scope_fails_to_parse() {
    for bad in [
        r#"{"type":"review_request","request_id":"r","tab":1,"turn":"newest","scope":"turn"}"#,
        r#"{"type":"review_request","request_id":"r","tab":1,"turn":-1,"scope":"turn"}"#,
        r#"{"type":"review_request","request_id":"r","tab":1,"turn":1.5,"scope":"turn"}"#,
        r#"{"type":"review_request","request_id":"r","tab":1,"turn":null,"scope":"turn"}"#,
        r#"{"type":"review_request","request_id":"r","tab":1,"turn":"latest","scope":"all"}"#,
        r#"{"type":"review_request","request_id":"r","tab":1,"turn":"latest"}"#,
        r#"{"type":"review_request","request_id":"r","tab":1,"scope":"turn"}"#,
        r#"{"type":"review_diff_request","request_id":"r","tab":1,"turn":1,"path":"a"}"#,
        r#"{"type":"review_diff_request","request_id":"r","tab":1,"turn":"latest","scope":"turn","path":"a"}"#,
    ] {
        assert!(parse_inbound_message(bad).is_none(), "{bad}");
    }
}

#[test]
fn review_diff_request_deserializes() {
    use crate::turn_review::Scope;
    let message = parse_inbound_message(
        r#"{"type":"review_diff_request","request_id":"r2","tab":3,"turn":7,"scope":"session","path":"core/src/x.rs"}"#,
    )
    .unwrap();
    assert_eq!(message.request_id(), "r2");
    assert!(matches!(message.tab_ref(), TabRef::Named(TabId(3))));
    let InboundMessage::ReviewDiffRequest { turn, scope, path, .. } = message else {
        panic!("not a review_diff_request")
    };
    assert_eq!(turn, 7);
    assert_eq!(Scope::from(scope), Scope::Session);
    assert_eq!(path, "core/src/x.rs");
}

/// Tab routing: a review of "the active tab" would show the wrong session's files after a switch.
#[test]
fn a_review_request_naming_no_tab_is_refused() {
    for json in [
        r#"{"type":"review_request","request_id":"a","turn":"latest","scope":"turn"}"#,
        r#"{"type":"review_request","request_id":"a","tab":null,"turn":"latest","scope":"turn"}"#,
        r#"{"type":"review_diff_request","request_id":"a","turn":1,"scope":"turn","path":"x"}"#,
    ] {
        let message = parse_inbound_message(json).unwrap();
        assert!(matches!(message.tab_ref(), TabRef::Missing), "{json}");
    }
}

/// Every revert, undo, comment and send message for turn review, as the panel sends it.
const REVIEW_PHASE2_LINES: [&str; 9] = [
    r#"{"type":"review_revert","request_id":"r9","tab":3,"turn":7,"scope":"turn","path":"core/src/x.rs","target":{"hunk":2,"header":"@@ -830,6 +830,12 @@"}}"#,
    r#"{"type":"review_revert","request_id":"r9","tab":3,"turn":7,"scope":"turn","path":"core/src/x.rs","target":"file"}"#,
    r#"{"type":"review_undo","request_id":"r10","tab":3}"#,
    r#"{"type":"review_comment_add","request_id":"r11","tab":3,"turn":7,"scope":"turn","path":"core/src/x.rs","from":837,"to":842,"text":"Why by turn id?"}"#,
    r#"{"type":"review_comment_remove","request_id":"r12","tab":3,"id":1}"#,
    r#"{"type":"review_send","request_id":"r13","tab":3,"confirm":null}"#,
    r#"{"type":"review_send","request_id":"r14","tab":3,"confirm":"9f2c4e1a0b7d3c55"}"#,
    r#"{"type":"review_recover","request_id":"r15","entry":"<uuid>","answer":"restore"}"#,
    r#"{"type":"open_in_editor","request_id":"r16","tab":3,"turn":7,"scope":"turn","path":"core/src/x.rs","line":837}"#,
];

#[test]
fn review_phase2_messages_deserialize() {
    use crate::turn_review::Scope;
    let parsed: Vec<InboundMessage> = REVIEW_PHASE2_LINES
        .iter()
        .map(|line| parse_inbound_message(line).unwrap_or_else(|| panic!("{line}")))
        .collect();
    let InboundMessage::ReviewRevert {
        turn,
        scope,
        path,
        target,
        ..
    } = &parsed[0]
    else {
        panic!("{:?}", parsed[0])
    };
    assert_eq!(
        (*turn, Scope::from(*scope), path.as_str()),
        (7, Scope::Turn, "core/src/x.rs")
    );
    assert_eq!(
        *target,
        ReviewRevertTargetWire::Hunk {
            id: 2,
            header: "@@ -830,6 +830,12 @@".into()
        }
    );
    assert!(matches!(
        &parsed[1],
        InboundMessage::ReviewRevert {
            target: ReviewRevertTargetWire::File,
            ..
        }
    ));
    assert!(matches!(&parsed[2], InboundMessage::ReviewUndo { .. }));
    let InboundMessage::ReviewCommentAdd { from, to, text, .. } = &parsed[3] else {
        panic!("{:?}", parsed[3])
    };
    assert_eq!((*from, *to, text.as_str()), (837, 842, "Why by turn id?"));
    assert!(matches!(&parsed[4], InboundMessage::ReviewCommentRemove { id: 1, .. }));
    assert!(matches!(&parsed[5], InboundMessage::ReviewSend { confirm: None, .. }));
    let InboundMessage::ReviewSend { confirm, .. } = &parsed[6] else {
        panic!("{:?}", parsed[6])
    };
    assert_eq!(confirm.as_deref(), Some("9f2c4e1a0b7d3c55"));
    let InboundMessage::ReviewRecover { entry, answer, .. } = &parsed[7] else {
        panic!("{:?}", parsed[7])
    };
    assert_eq!((entry.as_str(), *answer), ("<uuid>", ReviewRecoverAnswerWire::Restore));
    let InboundMessage::OpenInEditor { line, path, .. } = &parsed[8] else {
        panic!("{:?}", parsed[8])
    };
    assert_eq!((*line, path.as_str()), (Some(837), "core/src/x.rs"));
    // An absent `confirm` and an absent `line` read as none; the other answer parses too.
    assert!(matches!(
        parse_inbound_message(r#"{"type":"review_send","request_id":"r","tab":3}"#),
        Some(InboundMessage::ReviewSend { confirm: None, .. })
    ));
    assert!(matches!(
        parse_inbound_message(
            r#"{"type":"open_in_editor","request_id":"r","tab":3,"turn":1,"scope":"session","path":"a"}"#
        ),
        Some(InboundMessage::OpenInEditor { line: None, .. })
    ));
    assert!(matches!(
        parse_inbound_message(r#"{"type":"review_recover","request_id":"r","entry":"e","answer":"dismiss"}"#),
        Some(InboundMessage::ReviewRecover {
            answer: ReviewRecoverAnswerWire::Dismiss,
            ..
        })
    ));
}

#[test]
fn a_review_message_that_does_not_fit_fails_to_parse() {
    for bad in [
        r#"{"type":"review_revert","request_id":"r","tab":1,"turn":1,"scope":"turn","path":"a","target":"hunk"}"#,
        r#"{"type":"review_revert","request_id":"r","tab":1,"turn":1,"scope":"turn","path":"a","target":{"hunk":1}}"#,
        r#"{"type":"review_revert","request_id":"r","tab":1,"turn":1,"scope":"turn","path":"a"}"#,
        r#"{"type":"review_comment_add","request_id":"r","tab":1,"turn":1,"scope":"turn","path":"a","from":1,"text":"t"}"#,
        r#"{"type":"review_comment_remove","request_id":"r","tab":1}"#,
        r#"{"type":"review_comment_remove","request_id":"r","tab":1,"id":-1}"#,
        r#"{"type":"review_recover","request_id":"r","entry":"e","answer":"later"}"#,
        r#"{"type":"review_recover","request_id":"r","entry":"e"}"#,
    ] {
        assert!(parse_inbound_message(bad).is_none(), "{bad}");
    }
}

/// Tab routing: a revert, an undo, a comment or a send for "the active tab" would act on the
/// wrong session after a switch. Only the recovery, which belongs to the project, is window-level.
#[test]
fn a_review_command_naming_no_tab_is_refused() {
    for line in REVIEW_PHASE2_LINES {
        let named = parse_inbound_message(line).unwrap();
        if matches!(named, InboundMessage::ReviewRecover { .. }) {
            assert!(matches!(named.tab_ref(), TabRef::WindowLevel), "{line}");
            continue;
        }
        assert!(matches!(named.tab_ref(), TabRef::Named(TabId(3))), "{line}");
        let dropped = if line.contains(r#""tab":3,"#) {
            line.replace(r#""tab":3,"#, "")
        } else {
            line.replace(r#","tab":3"#, "")
        };
        for without in [dropped, line.replace(r#""tab":3"#, r#""tab":null"#)] {
            assert_ne!(without, line, "the line names a tab: {line}");
            let message = parse_inbound_message(&without).unwrap_or_else(|| panic!("{without}"));
            assert!(matches!(message.tab_ref(), TabRef::Missing), "{without}");
        }
    }
}

#[test]
fn request_ids_are_read_for_every_new_message() {
    let ids: Vec<String> = REVIEW_PHASE2_LINES
        .iter()
        .map(|line| parse_inbound_message(line).unwrap().request_id().to_owned())
        .collect();
    assert_eq!(ids, ["r9", "r9", "r10", "r11", "r12", "r13", "r14", "r15", "r16"]);
}

fn draft_with_everything() -> crate::turn_review::ReviewDraft {
    use crate::turn_review::{NewRevert, RevertShape, RevertSource, ReviewDraft};
    let mut draft = ReviewDraft::default();
    draft
        .add_comment(
            7,
            "core/src/x.rs",
            837,
            842,
            vec!["let base = …;".into()],
            "Why by turn id?",
        )
        .unwrap();
    let revert = |path: &str, hunk: Option<(u32, String)>, shape, source| NewRevert {
        turn: 7,
        path: path.into(),
        hunk,
        shape,
        source,
        at_line: 120,
        reverted_to: b"a\n".to_vec(),
        replaced: b"b\n".to_vec(),
        undo: None,
    };
    draft.record_revert(revert(
        "core/src/foo.rs",
        Some((2, "@@ -120,8 +120,12 @@".into())),
        RevertShape::Lines { from: 120, to: 127 },
        RevertSource::Panel,
    ));
    draft.record_revert(revert("new.rs", None, RevertShape::Deleted, RevertSource::Panel));
    draft.record_revert(revert("old.rs", None, RevertShape::Restored, RevertSource::Editor));
    let whole = draft.record_revert(revert("all.rs", None, RevertShape::WholeFile, RevertSource::Panel));
    draft.mark_undone(whole);
    draft
}

#[test]
fn review_draft_send_preview_and_recovery_serialize_exactly() {
    use crate::turn_review::{compose, JournalEntry, RevertStatus};
    let v = |s: String| serde_json::from_str::<serde_json::Value>(&s).unwrap();
    let draft = draft_with_everything();
    let expected_draft = serde_json::json!({
        "comments": [{"id": 1, "turn": 7, "path": "core/src/x.rs", "from": 837, "to": 842,
                      "anchor": ["let base = …;"], "text": "Why by turn id?"}],
        "reverts": [
            {"id": 1, "turn": 7, "path": "core/src/foo.rs", "hunk": 2, "header": "@@ -120,8 +120,12 @@",
             "what": "hunk", "lines": [120, 127], "source": "panel", "undone": false},
            {"id": 2, "turn": 7, "path": "new.rs", "hunk": null, "header": null,
             "what": "deleted", "lines": null, "source": "panel", "undone": false},
            {"id": 3, "turn": 7, "path": "old.rs", "hunk": null, "header": null,
             "what": "restored", "lines": null, "source": "editor", "undone": false},
            {"id": 4, "turn": 7, "path": "all.rs", "hunk": null, "header": null,
             "what": "file", "lines": null, "source": "panel", "undone": true},
        ],
        "canUndo": true,
    });
    assert_eq!(
        v(serialize_review_draft_for_js(Some("r11"), TabId(3), &draft)),
        serde_json::json!({"kind": "review_draft", "requestId": "r11", "tab": 3, "draft": expected_draft})
    );
    assert_eq!(
        v(serialize_review_draft_for_js(None, TabId(3), &draft))["requestId"],
        serde_json::Value::Null,
        "a change the panel did not ask for carries no request id"
    );
    let empty = v(serialize_review_draft_for_js(
        None,
        TabId(3),
        &crate::turn_review::ReviewDraft::default(),
    ));
    assert_eq!(
        empty["draft"],
        serde_json::json!({"comments": [], "reverts": [], "canUndo": false})
    );

    let statuses = [
        (1, RevertStatus::OnDisk),
        (2, RevertStatus::ChangedSince),
        (3, RevertStatus::OnlyInEditor),
        (4, RevertStatus::Undone),
    ];
    let preview = compose(&draft, &statuses, 7);
    assert_eq!(
        v(serialize_review_send_preview_for_js(
            "r13",
            TabId(3),
            &draft,
            &preview,
            false
        )),
        serde_json::json!({
            "kind": "review_send_preview", "requestId": "r13", "tab": 3,
            "digest": preview.digest, "text": preview.text,
            "notOnDisk": [
                {"id": 2, "path": "new.rs", "what": "deleted", "lines": null, "why": "changed since"},
                {"id": 3, "path": "old.rs", "what": "restored", "lines": null,
                 "why": "only in the editor, not saved"},
                {"id": 4, "path": "all.rs", "what": "file", "lines": null, "why": "undone"},
            ],
            "queued": false,
        })
    );
    assert_eq!(
        v(serialize_review_send_preview_for_js(
            "r13",
            TabId(3),
            &draft,
            &preview,
            true
        ))["queued"],
        true
    );

    let entry = JournalEntry {
        id: "e1".into(),
        path: "core/src/x.rs".into(),
        pre: None,
        intended: None,
        intended_mode: Some(0o644),
        mode: 0o644,
        at_ms: 1_790_000_000_000,
        pid: 1,
        session: "s".into(),
    };
    assert_eq!(
        v(serialize_review_recovery_for_js(&[entry])),
        serde_json::json!({"kind": "review_recovery", "entries": [
            {"id": "e1", "path": "core/src/x.rs", "at": 1_790_000_000_000u64}]})
    );
    assert_eq!(
        v(serialize_review_recovery_for_js(&[])),
        serde_json::json!({"kind": "review_recovery", "entries": []})
    );
}

/// `agent-ui/web/src/fixtures/review-send-preview.json`: a send preview leaving out one revert of
/// every kind -- a hunk, a deleted file, a restored file and a whole file -- as this side sends it,
/// which the overlay's tests render, so an entry without a line range is drawn by the page's own
/// code and not only by a hand-written test value. Compared on every run;
/// `EITRI_WRITE_FIXTURES=1` rewrites it.
#[test]
fn the_review_send_preview_fixture_is_what_the_serializer_sends() {
    use crate::turn_review::{compose, RevertStatus};
    let draft = draft_with_everything();
    let statuses = [
        (1, RevertStatus::ChangedSince),
        (2, RevertStatus::ChangedSince),
        (3, RevertStatus::OnlyInEditor),
        (4, RevertStatus::Undone),
    ];
    let preview = compose(&draft, &statuses, 7);
    let envelope = serialize_review_send_preview_for_js("send-1", TabId(1), &draft, &preview, false);
    let fixture = json!({
        "writtenBy": "core/src/agent_bridge.rs, tests::the_review_send_preview_fixture_is_what_the_serializer_sends \
                      -- compared on every run; EITRI_WRITE_FIXTURES=1 rewrites it",
        "envelope": serde_json::from_str::<Value>(&envelope).unwrap(),
    });
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/review-send-preview.json");
    let written = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
    let committed = std::fs::read_to_string(&path).ok();
    if committed.as_deref() == Some(written.as_str()) {
        return;
    }
    if std::env::var_os("EITRI_WRITE_FIXTURES").is_some_and(|v| v == "1") {
        std::fs::write(&path, &written).expect("write the review send preview fixture");
    } else {
        assert_eq!(
            committed.as_deref(),
            Some(written.as_str()),
            "the committed review send preview fixture is not what this test produces; if the change \
             is intended, regenerate it with EITRI_WRITE_FIXTURES=1"
        );
    }
}

#[test]
fn review_carries_the_draft_and_review_diff_its_file_kind() {
    use crate::turn_review::{Overview, ReviewDiff, Scope};
    let v = |s: String| serde_json::from_str::<serde_json::Value>(&s).unwrap();
    let overview = Overview {
        session: "s".into(),
        scope: Scope::Turn,
        current: 7,
        turns: Vec::new(),
        files: Vec::new(),
        compared: true,
        pending_no_result: 0,
        notes: Vec::new(),
    };
    let draft = draft_with_everything();
    let sent = v(serialize_review_for_js("r1", TabId(3), &overview, &draft));
    assert_eq!(
        sent["draft"],
        v(serialize_review_draft_for_js(None, TabId(3), &draft))["draft"]
    );

    for (binary, new_file, deleted_file) in [(true, false, false), (false, true, false), (false, false, true)] {
        let diff = ReviewDiff {
            turn: 7,
            path: "a".into(),
            added: 0,
            removed: 0,
            binary,
            new_file,
            deleted_file,
            mode_change: None,
            hunks: None,
        };
        let sent = v(serialize_review_diff_for_js("r2", TabId(3), &diff));
        assert_eq!(
            (
                sent["binary"].as_bool(),
                sent["newFile"].as_bool(),
                sent["deletedFile"].as_bool()
            ),
            (Some(binary), Some(new_file), Some(deleted_file))
        );
    }
}

fn review_turn(n: u32, state_end: crate::turn_review::Snap) -> crate::turn_review::TurnRecord {
    use crate::turn_review::{Snap, TurnRecord};
    TurnRecord {
        n,
        turn_id: format!("t{n}"),
        tab: 3,
        session: "sess".into(),
        started_ms: 1_790_000_000_000,
        ended_ms: Some(1_790_000_020_000),
        base: Snap::Taken { commit: "b".into() },
        base_taken_ms: Some(1_790_000_000_100),
        end: state_end,
        late: false,
        overlapped_next: false,
        overlapped_tab: false,
        left_running: false,
        skipped_large: Vec::new(),
        left_out: Default::default(),
        earlier_run: false,
    }
}

#[test]
fn review_envelopes_serialize_exactly() {
    use crate::turn_review::{
        DiffLine, Hunk, LineKind, Origin, Overview, OverviewFile, ReviewDiff, ReviewHint, Scope, Snap,
    };
    let v = |s: String| serde_json::from_str::<serde_json::Value>(&s).unwrap();
    let mut unfinished = review_turn(8, Snap::Unavailable("it never ended".into()));
    unfinished.ended_ms = None;
    unfinished.late = true;
    unfinished.overlapped_next = true;
    unfinished.overlapped_tab = true;
    let overview = Overview {
        session: "sess".into(),
        scope: Scope::Turn,
        current: 7,
        turns: vec![review_turn(7, Snap::Taken { commit: "e".into() }), unfinished],
        files: vec![
            OverviewFile {
                path: "core/src/x.rs".into(),
                added: 41,
                removed: 6,
                origin: Origin::Agent,
                binary: false,
                too_large: false,
                nested: false,
            },
            OverviewFile {
                path: "a.png".into(),
                added: 0,
                removed: 0,
                origin: Origin::AgentOnly,
                binary: true,
                too_large: true,
                nested: true,
            },
            OverviewFile {
                path: "Cargo.lock".into(),
                added: 2,
                removed: 1,
                origin: Origin::Workspace,
                binary: false,
                too_large: false,
                nested: false,
            },
        ],
        compared: true,
        pending_no_result: 2,
        notes: vec!["changed on disk during this turn".into()],
    };
    assert_eq!(
        v(serialize_review_for_js(
            "r1",
            TabId(3),
            &overview,
            &crate::turn_review::ReviewDraft::default()
        )),
        serde_json::json!({
            "kind": "review", "requestId": "r1", "tab": 3, "scope": "turn", "current": 7,
            "turns": [
                {"n": 7, "turnId": "t7", "startedAt": 1_790_000_000_000u64, "endedAt": 1_790_000_020_000u64,
                 "state": "ok", "late": false, "overlappedNext": false, "overlappedTab": false, "reason": null},
                {"n": 8, "turnId": "t8", "startedAt": 1_790_000_000_000u64, "endedAt": null,
                 "state": "unfinished", "late": true, "overlappedNext": true, "overlappedTab": true,
                 "reason": "it never ended"},
            ],
            "files": [
                {"path": "core/src/x.rs", "added": 41, "removed": 6, "origin": "agent", "binary": false,
                 "tooLarge": false, "nested": false},
                {"path": "a.png", "added": 0, "removed": 0, "origin": "agent_only", "binary": true,
                 "tooLarge": true, "nested": true},
                {"path": "Cargo.lock", "added": 2, "removed": 1, "origin": "workspace", "binary": false,
                 "tooLarge": false, "nested": false},
            ],
            "compared": true,
            "pendingNoResult": 2,
            "notes": ["changed on disk during this turn"],
            "draft": {"comments": [], "reverts": [], "canUndo": false},
        })
    );

    let diff = ReviewDiff {
        turn: 7,
        path: "core/src/x.rs".into(),
        added: 41,
        removed: 6,
        binary: false,
        new_file: false,
        deleted_file: false,
        mode_change: None,
        hunks: Some(vec![Hunk {
            id: 0,
            header: "@@ -830,6 +830,12 @@".into(),
            old_start: 830,
            old_len: 6,
            new_start: 830,
            new_len: 12,
            lines: vec![
                DiffLine {
                    kind: LineKind::Context,
                    text: "keep".into(),
                    old_no: Some(830),
                    new_no: Some(830),
                },
                DiffLine {
                    kind: LineKind::Removed,
                    text: "old".into(),
                    old_no: Some(831),
                    new_no: None,
                },
                DiffLine {
                    kind: LineKind::Added,
                    text: "new".into(),
                    old_no: None,
                    new_no: Some(837),
                },
                DiffLine {
                    kind: LineKind::NoNewline,
                    text: "\\ No newline at end of file".into(),
                    old_no: None,
                    new_no: None,
                },
            ],
        }]),
    };
    assert_eq!(
        v(serialize_review_diff_for_js("r2", TabId(3), &diff)),
        serde_json::json!({
            "kind": "review_diff", "requestId": "r2", "tab": 3, "turn": 7, "path": "core/src/x.rs",
            "added": 41, "removed": 6, "binary": false, "newFile": false, "deletedFile": false,
            "hunks": [{"id": 0, "header": "@@ -830,6 +830,12 @@", "lines": [
                {"kind": "context", "text": "keep", "oldNo": 830, "newNo": 830},
                {"kind": "removed", "text": "old", "oldNo": 831, "newNo": null},
                {"kind": "added", "text": "new", "oldNo": null, "newNo": 837},
                {"kind": "no_newline", "text": "\\ No newline at end of file", "oldNo": null, "newNo": null},
            ]}],
        })
    );

    assert_eq!(
        v(serialize_review_hint_for_js(&ReviewHint {
            tab: 3,
            turn: 7,
            files: 3
        })),
        serde_json::json!({ "kind": "review_hint", "tab": 3, "turn": 7, "files": 3 })
    );
    assert_eq!(
        v(serialize_review_hint_for_js(&ReviewHint {
            tab: 3,
            turn: 7,
            files: 0
        }))["files"],
        0,
        "zero clears the hint"
    );
}

#[test]
fn hunks_null_over_the_cap() {
    let diff = crate::turn_review::ReviewDiff {
        turn: 2,
        path: "big.rs".into(),
        added: 1500,
        removed: 900,
        binary: false,
        new_file: false,
        deleted_file: false,
        mode_change: None,
        hunks: None,
    };
    let value: serde_json::Value = serde_json::from_str(&serialize_review_diff_for_js("r", TabId(1), &diff)).unwrap();
    assert!(value["hunks"].is_null(), "refused, not cut");
    assert!(value.as_object().unwrap().contains_key("hunks"), "the key stays");
    assert_eq!(value["added"], 1500);
    assert_eq!(value["removed"], 900);
}
