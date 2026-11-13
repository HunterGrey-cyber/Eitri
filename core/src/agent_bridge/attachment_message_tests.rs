//! The page's image messages on the wire: each names its tab as a plain number, a send's images are a list of
//! tokens that defaults to none, and the host's answers carry metadata, never bytes.
#![cfg(test)]
use super::*;

#[test]
fn the_attach_messages_parse_and_name_their_tab() {
    let begin = parse_inbound_message(
        r#"{"type":"attach_begin","request_id":"b1","tab":3,"generation":2,"byte_length":10,"name":"shot.png"}"#,
    )
    .unwrap();
    assert_eq!(begin.request_id(), "b1");
    assert!(matches!(begin.tab_ref(), TabRef::Named(crate::tabs::TabId(3))));
    assert!(matches!(
        begin,
        InboundMessage::AttachBegin { generation: 2, byte_length: 10, ref name, .. } if name == "shot.png"
    ));
    for raw in [
        r#"{"type":"attach_chunk","request_id":"c","tab":3,"attachment_id":"x","offset":0,"data_b64":"AA=="}"#,
        r#"{"type":"attach_end","request_id":"e","tab":3,"attachment_id":"x","sha256":"00"}"#,
        r#"{"type":"attach_cancel","request_id":"x","tab":3,"attachment_id":"x"}"#,
        r#"{"type":"attach_remove","request_id":"r","tab":3,"attachment_id":"x"}"#,
    ] {
        let message = parse_inbound_message(raw).unwrap_or_else(|| panic!("{raw}"));
        assert!(
            matches!(message.tab_ref(), TabRef::Named(crate::tabs::TabId(3))),
            "{raw}"
        );
    }
    // The tab is required: an image message about no tab does not parse.
    assert!(parse_inbound_message(r#"{"type":"attach_remove","request_id":"r","attachment_id":"x"}"#).is_none());
    assert!(parse_inbound_message(
        r#"{"type":"attach_chunk","request_id":"c","tab":3,"attachment_id":"x","offset":-1,"data_b64":""}"#
    )
    .is_none());
}

#[test]
fn a_send_s_images_are_tokens_in_order_and_default_to_none() {
    let send =
        parse_inbound_message(r#"{"type":"send_message","request_id":"s","tab":1,"text":"t","attachments":["b","a"]}"#)
            .unwrap();
    assert!(matches!(send, InboundMessage::SendMessage { ref attachments, .. } if attachments == &["b", "a"]));
    let plain = parse_inbound_message(r#"{"type":"queue_message","request_id":"q","tab":1,"text":"t"}"#).unwrap();
    assert!(matches!(plain, InboundMessage::QueueMessage { ref attachments, .. } if attachments.is_empty()));
}

#[test]
fn a_take_back_without_images_reads_as_before() {
    let drafts = [crate::tab_set::QueuedDraft {
        text: "a".into(),
        attachments: Vec::new(),
    }];
    assert_eq!(
        serialize_queue_taken_drafts_for_js(crate::tabs::TabId(1), &drafts),
        serialize_queue_taken_for_js(crate::tabs::TabId(1), &["a".to_string()])
    );
    let begun: Value = serde_json::from_str(&serialize_attach_begun_for_js("b1", "tok")).unwrap();
    assert_eq!(
        begun,
        json!({"kind": "command_result", "requestId": "b1", "ok": true, "attachmentId": "tok"})
    );
}
