//! The page's two question answers on the wire: every field named, the decline's kind a closed
//! set, and anything else a parse failure rather than a default.
#![cfg(test)]
use super::*;

#[test]
fn question_draft_has_a_tab_and_an_interaction_without_an_answer() {
    let message = parse_inbound_message(r#"{"type":"question_draft","request_id":"d1","tab":3,"interaction_id":"q1","draft":{"page":1,"text":"typed"}}"#).unwrap();
    assert_eq!(message.request_id(), "d1");
    assert!(matches!(message.tab_ref(), TabRef::Named(crate::tabs::TabId(3))));
    assert!(
        matches!(message, InboundMessage::QuestionDraft { interaction_id, draft, .. } if interaction_id == "q1" && draft["text"] == "typed")
    );
}

#[test]
fn edit_question_other_names_the_question_and_text() {
    let message = parse_inbound_message(r#"{"type":"edit_question_other","request_id":"e1","tab":3,"interaction_id":"q1","question_index":2,"text":"typed"}"#).unwrap();
    assert_eq!(message.request_id(), "e1");
    assert!(matches!(message.tab_ref(), TabRef::Named(crate::tabs::TabId(3))));
    assert!(
        matches!(message, InboundMessage::EditQuestionOther { interaction_id, question_index: 2, text, .. } if interaction_id == "q1" && text == "typed")
    );
    assert!(parse_inbound_message(
        r#"{"type":"edit_question_other","request_id":"e1","interaction_id":"q1","question_index":-1,"text":"typed"}"#
    )
    .is_none());
}

#[test]
fn a_question_answer_parses_into_indexes_and_typed_text() {
    let message = parse_inbound_message(
        r#"{"type":"question_answer","request_id":"r1","tab":3,"interaction_id":"i1","answers":[{"question_index":0,"option_indexes":[1]},{"question_index":1,"option_indexes":[0,2],"other_text":"bench","notes":null}]}"#,
    )
    .expect("parses");
    assert_eq!(message.request_id(), "r1");
    assert!(matches!(message.tab_ref(), TabRef::Named(crate::tabs::TabId(3))));
    let InboundMessage::QuestionAnswer {
        interaction_id,
        answers,
        ..
    } = message
    else {
        panic!("a question_answer");
    };
    assert_eq!(interaction_id, "i1");
    let answers: Vec<agent::QuestionAnswer> = answers.into_iter().map(QuestionAnswerWire::into_answer).collect();
    assert_eq!(
        answers,
        vec![
            agent::QuestionAnswer {
                question_index: 0,
                option_indexes: vec![1],
                other_text: None,
                notes: None,
            },
            agent::QuestionAnswer {
                question_index: 1,
                option_indexes: vec![0, 2],
                other_text: Some("bench".into()),
                notes: None,
            },
        ]
    );
}

#[test]
fn a_question_decline_parses_its_kind_and_what_was_answered() {
    let cancel = parse_inbound_message(
        r#"{"type":"question_decline","request_id":"r1","tab":3,"interaction_id":"i1","kind":"cancel"}"#,
    )
    .expect("parses");
    assert!(matches!(
        cancel,
        InboundMessage::QuestionDecline { kind: QuestionDeclineChoice::Cancel, ref so_far, .. } if so_far.is_empty()
    ));
    let chat = parse_inbound_message(
        r#"{"type":"question_decline","request_id":"r2","tab":3,"interaction_id":"i1","kind":"chat","so_far":[{"question_index":0,"option_indexes":[0]}]}"#,
    )
    .expect("parses");
    let InboundMessage::QuestionDecline { kind, so_far, .. } = chat else {
        panic!("a question_decline");
    };
    assert_eq!(kind.kind(), agent::QuestionDeclineKind::Chat);
    assert_eq!(so_far.len(), 1);
    assert_eq!(QuestionDeclineChoice::Cancel.kind(), agent::QuestionDeclineKind::Cancel);
}

#[test]
fn an_editor_card_answer_parses_its_decision_and_origin() {
    let parsed = parse_inbound_message(
        r#"{"type":"editor_card_answer","request_id":"r1","tab":3,"card_id":"ec-0-1","decision":"allow","origin":"key"}"#,
    )
    .expect("parses");
    assert_eq!(parsed.request_id(), "r1");
    assert!(matches!(parsed.tab_ref(), TabRef::Named(crate::tabs::TabId(3))));
    let InboundMessage::EditorCardAnswer {
        card_id,
        decision,
        origin,
        ..
    } = parsed
    else {
        panic!("an editor_card_answer");
    };
    assert_eq!(card_id, "ec-0-1");
    assert_eq!(
        decision.card_decision(),
        crate::editor_tools::cards::CardDecision::Allow
    );
    assert_eq!(origin.origin(), crate::editor_tools::cards::CardOrigin::Key);
    assert_eq!(
        CardOriginChoice::Click.origin(),
        crate::editor_tools::cards::CardOrigin::Click
    );
    assert_eq!(
        DecisionChoice::Deny.card_decision(),
        crate::editor_tools::cards::CardDecision::Deny
    );
}

#[test]
fn editor_card_view_parses() {
    let parsed =
        parse_inbound_message(r#"{"type":"editor_card_view","request_id":"r1","tab":3,"proposal":"p-2","file":1}"#)
            .expect("parses");
    assert_eq!(parsed.request_id(), "r1");
    assert!(matches!(parsed.tab_ref(), TabRef::Named(crate::tabs::TabId(3))));
    let InboundMessage::EditorCardView { proposal, file, .. } = parsed else {
        panic!("an editor_card_view");
    };
    assert_eq!((proposal.as_str(), file), ("p-2", 1));
}

#[test]
fn a_missing_file_index_does_not_parse() {
    for raw in [
        r#"{"type":"editor_card_view","request_id":"r","tab":1,"proposal":"p-1"}"#,
        r#"{"type":"editor_card_view","request_id":"r","tab":1,"proposal":"p-1","file":-1}"#,
        r#"{"type":"editor_card_view","request_id":"r","tab":1,"file":0}"#,
    ] {
        assert!(parse_inbound_message(raw).is_none(), "{raw}");
    }
}

/// `origin` says how the person answered, so nothing but the two words the page sends may stand for one.
#[test]
fn an_editor_card_answer_without_a_known_origin_does_not_parse() {
    for raw in [
        r#"{"type":"editor_card_answer","request_id":"r","tab":1,"card_id":"c","decision":"allow"}"#,
        r#"{"type":"editor_card_answer","request_id":"r","tab":1,"card_id":"c","decision":"allow","origin":"agent"}"#,
        r#"{"type":"editor_card_answer","request_id":"r","tab":1,"card_id":"c","decision":"always","origin":"key"}"#,
        r#"{"type":"editor_card_answer","request_id":"r","tab":1,"decision":"allow","origin":"key"}"#,
    ] {
        assert!(parse_inbound_message(raw).is_none(), "{raw}");
    }
}

#[test]
fn an_editor_card_answer_with_any_other_origin_does_not_parse() {
    let answer = |origin: &str| {
        format!(
            r#"{{"type":"editor_card_answer","request_id":"r","tab":1,"card_id":"c","decision":"allow","origin":"{origin}"}}"#
        )
    };
    for origin in ["agent", "host", "sweep", "bypass", "ctrl_y", "Key", "KEY", "click ", ""] {
        assert!(parse_inbound_message(&answer(origin)).is_none(), "{origin:?}");
    }
    for origin in ["key", "click"] {
        assert!(parse_inbound_message(&answer(origin)).is_some(), "{origin:?}");
    }
}

#[test]
fn the_editor_cards_envelope_holds_the_shared_wire_shape() {
    let fixture: Value =
        serde_json::from_str(include_str!("../../../agent-ui/web/src/fixtures/editor-cards.json")).unwrap();
    let cards: Vec<crate::editor_tools::cards::EditorCardWire> = serde_json::from_value(fixture.clone()).unwrap();
    let envelope: Value = serde_json::from_str(&serialize_editor_cards_for_js(crate::tabs::TabId(7), &cards)).unwrap();
    assert_eq!(envelope["kind"], "editor_cards");
    assert_eq!(envelope["tab"], 7);
    assert_eq!(envelope["cards"], fixture);
    let empty: Value = serde_json::from_str(&serialize_editor_cards_for_js(crate::tabs::TabId(7), &[])).unwrap();
    assert_eq!(
        empty["cards"],
        json!([]),
        "an empty list is what takes a card off the page"
    );
}

#[test]
fn an_unknown_kind_or_a_malformed_answer_does_not_parse() {
    for raw in [
        r#"{"type":"question_decline","request_id":"r","tab":1,"interaction_id":"i1","kind":"later"}"#,
        r#"{"type":"question_decline","request_id":"r","tab":1,"interaction_id":"i1"}"#,
        r#"{"type":"question_answer","request_id":"r","tab":1,"interaction_id":"i1"}"#,
        r#"{"type":"question_answer","request_id":"r","tab":1,"interaction_id":"i1","answers":[{"question_index":0}]}"#,
        r#"{"type":"question_answer","request_id":"r","tab":1,"interaction_id":"i1","answers":[{"question_index":-1,"option_indexes":[]}]}"#,
        r#"{"type":"question_answer","request_id":"r","tab":1,"interaction_id":"i1","answers":[{"question_index":0,"option_indexes":["0"]}]}"#,
        r#"{"type":"question_answer","request_id":"r","tab":1,"interaction_id":"i1","answers":[{"question_index":0,"option_indexes":[0],"answer":"Grid"}]}"#,
        r#"{"type":"question_decline","request_id":"r","tab":1,"interaction_id":"i1","kind":"chat","so_far":[{"question_index":0,"option_indexes":[0],"updatedInput":{}}]}"#,
    ] {
        assert!(parse_inbound_message(raw).is_none(), "{raw}");
    }
}
