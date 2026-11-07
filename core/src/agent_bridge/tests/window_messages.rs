//! The window-level messages and envelopes: choosing a project, the copy kinds, the theme, pane focus, the keymap and its help, pane navigation and open_url.
#![cfg(test)]

use super::*;

#[test]
fn choose_project_carries_nothing_but_its_request_id() {
    let message = parse_inbound_message(r#"{"type":"choose_project","request_id":"p1"}"#).unwrap();
    let InboundMessage::ChooseProject { request_id } = &message else {
        panic!("the message requests the window's chooser");
    };
    assert_eq!(request_id, "p1");
    assert_eq!(message.request_id(), "p1");
    assert!(matches!(message.tab_ref(), TabRef::WindowLevel));
    assert!(parse_inbound_message(r#"{"type":"choose_project"}"#).is_none());
}

#[test]
fn hello_says_can_choose_project_when_a_hook_is_installed() {
    let greeting = greeting_with(Vec::new());
    for installed in [false, true] {
        let value: Value =
            serde_json::from_str(&serialize_hello_with_project_for_js(&greeting, None, installed)).unwrap();
        assert_eq!(value["canChooseProject"], installed);
    }
}

#[test]
fn primary_copy_reads_only_its_own_kind_and_caps_the_text() {
    assert_eq!(
        primary_copy(r#"{"type":"copy_primary","text":"Run this:\n\nls -la\n\nthen look."}"#).as_deref(),
        Some("Run this:\n\nls -la\n\nthen look.")
    );
    assert_eq!(
        primary_copy(r#"{"text":"x","type":"copy_primary"}"#).as_deref(),
        Some("x")
    );
    assert_eq!(
        primary_copy(r#"{"type":"copy_primary","text":""}"#).as_deref(),
        Some("")
    );
    for other in [
        // Another kind, even one whose text names this kind.
        r#"{"type":"send_message","request_id":"r1","text":"\"copy_primary\""}"#,
        r#"{"type":"view_in_editor","request_id":"r1","title":"copy_primary","text":"x"}"#,
        // This kind with anything more, less or other than its text.
        r#"{"type":"copy_primary","text":"x","request_id":"r1"}"#,
        r#"{"type":"copy_primary","text":"x","tab":1}"#,
        r#"{"type":"copy_primary"}"#,
        r#"{"type":"copy_primary","text":7}"#,
        r#"{"type":"copy_primary","text":null}"#,
        r#"{"type":"copy_primary","text":["x"]}"#,
        r#"{"type":"Copy_Primary","text":"x"}"#,
        r#"["copy_primary","x"]"#,
        r#"{"type":"copy_primary","text":"x""#,
        "",
    ] {
        assert_eq!(primary_copy(other), None, "{other}");
    }
    let message = |text: &str| serde_json::json!({ "type": "copy_primary", "text": text }).to_string();
    let at_cap = "x".repeat(PRIMARY_COPY_MAX_BYTES);
    assert_eq!(primary_copy(&message(&at_cap)), Some(at_cap.clone()));
    assert_eq!(primary_copy(&message(&format!("{at_cap}x"))), None);
    // The cap counts UTF-8 bytes, not characters.
    let two_byte = "\u{e9}".repeat(PRIMARY_COPY_MAX_BYTES / 2);
    assert_eq!(primary_copy(&message(&two_byte)), Some(two_byte.clone()));
    assert_eq!(primary_copy(&message(&format!("{two_byte}x"))), None);
    // The longest a capped text can be spelled in JSON still reads.
    let escaped = format!(
        r#"{{"type":"copy_primary","text":"{}"}}"#,
        "\\u0001".repeat(PRIMARY_COPY_MAX_BYTES)
    );
    assert_eq!(primary_copy(&escaped).map(|t| t.len()), Some(PRIMARY_COPY_MAX_BYTES));
    // The message is the shell's alone: the panel's parser never takes it.
    assert!(parse_inbound_message(&message("x")).is_none());
}

#[test]
fn first_paint_is_recognized_only_by_its_object_type() {
    for painted in [r#"{"type":"painted"}"#, r#"{"type":"painted","x":1}"#] {
        assert!(is_first_paint(painted), "{painted}");
    }
    for other in [
        r#"{"type":"ready","request_id":"r"}"#,
        r#"{"type":"copy_primary","text":"painted"}"#,
        r#""painted""#,
        r#"["painted"]"#,
        "[]",
        r#"{"type":"painted""#,
    ] {
        assert!(!is_first_paint(other), "{other}");
    }
}

#[test]
fn every_message_of_the_copy_kind_is_known_as_one_so_no_host_hands_it_to_the_panel() {
    let over_cap =
        serde_json::json!({ "type": "copy_primary", "text": "x".repeat(PRIMARY_COPY_MAX_BYTES + 1) }).to_string();
    for copy in [
        r#"{"type":"copy_primary","text":"secret"}"#,
        r#"{"text":"secret","type":"copy_primary"}"#,
        r#"{"type":"copy_primary","text":"secret","request_id":"r1"}"#,
        r#"{"type":"copy_primary"}"#,
        over_cap.as_str(),
    ] {
        assert!(is_primary_copy(copy), "{copy}");
    }
    for other in [
        r#"{"type":"send_message","request_id":"r1","tab":1,"text":"\"copy_primary\""}"#,
        r#"{"type":"view_in_editor","request_id":"r1","title":"copy_primary","text":"x"}"#,
        r#"["copy_primary","x"]"#,
        r#"{"type":"copy_primary","text":"x""#,
        r#"{"type":"Copy_Primary","text":"x"}"#,
        "",
    ] {
        assert!(!is_primary_copy(other), "{other}");
    }
}

#[test]
fn a_chooser_row_names_the_extra_view_that_shows_its_tab() {
    use crate::tabs::ViewId;
    assert_eq!(ChooserTab::label(3, "docs", ViewId::BASE), "3 docs");
    assert_eq!(
        ChooserTab::label(3, "docs", ViewId::new(2).unwrap()),
        "3 docs · agent 2"
    );
}

#[test]
fn a_theme_envelope_carries_every_css_variable_verbatim() {
    let tokens = crate::theme::ThemeTokens::fallback();
    let value: serde_json::Value = serde_json::from_str(&serialize_theme_for_js(&tokens)).unwrap();
    assert_eq!(value["kind"], "theme");
    let vars = value["vars"].as_object().unwrap();
    assert_eq!(vars.len(), tokens.css_vars().len());
    assert_eq!(vars["--nv-bg"], tokens.bg.hex());
    assert_eq!(vars["--nv-font-prose"], crate::theme::tokens::PROSE_FONT_STACK);
}

#[test]
fn pane_focus_is_a_kind_tagged_boolean() {
    for focused in [true, false] {
        let value: serde_json::Value = serde_json::from_str(&serialize_pane_focus_for_js(focused)).unwrap();
        assert_eq!(value, serde_json::json!({ "kind": "pane_focus", "focused": focused }));
    }
}

#[test]
fn the_root_note_is_a_kind_tagged_text_that_keeps_its_characters() {
    let text = "No project boundary here (it is open to others): a \"Claude\" tab asks.\nNext";
    let value: serde_json::Value = serde_json::from_str(&serialize_root_note_for_js(text)).unwrap();
    assert_eq!(value, serde_json::json!({ "kind": "root_note", "text": text }));
    assert_eq!(
        serialize_root_note_for_js("a"),
        r#"{"kind":"root_note","text":"a"}"#,
        "the exact envelope"
    );
}

#[test]
fn serializes_enter_input() {
    let value: serde_json::Value = serde_json::from_str(&serialize_enter_input_for_js()).unwrap();
    assert_eq!(value, serde_json::json!({ "kind": "enter_input" }));
}

#[test]
fn serializes_focus_permission() {
    let value: serde_json::Value = serde_json::from_str(&serialize_focus_permission_for_js(TabId(1))).unwrap();
    assert_eq!(value, serde_json::json!({ "kind": "focus_permission", "tab": 1 }));
}

#[test]
fn serializes_the_keymap_help() {
    let row = |k: &str, w: &str| crate::keymap::HelpRow {
        keys: k.into(),
        what: w.into(),
    };
    let (panel, _) = crate::keymap::panel::effective(&Default::default(), None);
    let value: serde_json::Value = serde_json::from_str(&serialize_keymap_for_js(
        "Ctrl+b",
        &[row("F11", "Fullscreen")],
        &[row("Ctrl+b f", "HINT")],
        &panel,
        "Ctrl+b c",
        &[row(
            "~/.tmux.conf:3",
            "bind P paste-buffer \u{2014} no Eitri equivalent: paste-buffer",
        )],
    ))
    .unwrap();
    assert_eq!(value["kind"], "keymap");
    assert_eq!(value["prefix"], "Ctrl+b");
    assert_eq!(
        value["window"],
        serde_json::json!([{ "keys": "F11", "what": "Fullscreen" }])
    );
    assert_eq!(
        value["prefixKeys"],
        serde_json::json!([{ "keys": "Ctrl+b f", "what": "HINT" }])
    );
    assert_eq!(value["newTabChord"], "Ctrl+b c");
    assert!(value["panel"]["bindings"].is_array(), "the panel table travels too");
    assert_eq!(
        value["tmuxSkipped"],
        serde_json::json!([{ "keys": "~/.tmux.conf:3", "what": "bind P paste-buffer \u{2014} no Eitri equivalent: paste-buffer" }])
    );
}

#[test]
fn the_keymap_envelope_carries_the_panel_table() {
    let (panel, _) = crate::keymap::panel::effective(&Default::default(), None);
    let v: serde_json::Value =
        serde_json::from_str(&serialize_keymap_for_js("Ctrl+b", &[], &[], &panel, "Ctrl+b c", &[])).unwrap();
    assert_eq!(v["newTabChord"], "Ctrl+b c");
    assert_eq!(v["panel"]["leader"], " ");
    assert_eq!(v["panel"]["leaderSource"], "default");
    assert_eq!(v["panel"]["timeoutlen"], 1000);
    let bd = v["panel"]["bindings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["action"] == "tab.close")
        .unwrap();
    assert_eq!(bd["keys"], serde_json::json!(["<leader>", "b", "d"]));
    assert_eq!(bd["source"], "default");
    assert_eq!(
        v["panel"]["groups"][0],
        serde_json::json!({"keys": ["<leader>", "b"], "label": "+tab"})
    );
}

fn intent_arrival(intent: crate::agents::Intent) -> serde_json::Value {
    serde_json::from_str(&serialize_arrive_with_intent_for_js(intent)).unwrap()
}

/// An unknown verb returns `None`, matching other unrecognized wire values and malformed
/// messages in the `Option<InboundMessage>` parser.
#[test]
fn arrive_tab_verb_and_cycle_default_mode() {
    assert_eq!(serialize_arrive_for_js(), r#"{"kind":"arrive"}"#);
    assert_eq!(
        intent_arrival(crate::agents::Intent::Stop),
        serde_json::json!({"kind": "arrive", "intent": "stop"})
    );
    assert_eq!(
        intent_arrival(crate::agents::Intent::Show),
        serde_json::json!({"kind": "arrive", "intent": "show"})
    );
    let m = parse_inbound_message(r#"{"type":"tab_verb","request_id":"a","verb":"close"}"#).unwrap();
    assert!(matches!(
        m,
        InboundMessage::TabVerb {
            verb: TabVerbWire::Close,
            ..
        }
    ));
    assert_eq!(m.tab_ref(), TabRef::WindowLevel);
    assert!(parse_inbound_message(r#"{"type":"tab_verb","request_id":"a","verb":"rename"}"#).is_none());
    let named = parse_inbound_message(r#"{"type":"tab_verb","request_id":"a","verb":"new","agent":"codex"}"#).unwrap();
    assert!(matches!(
        named,
        InboundMessage::TabVerb {
            verb: TabVerbWire::New,
            agent: Some(ref agent),
            ..
        } if agent == "codex"
    ));
    let plain = parse_inbound_message(r#"{"type":"tab_verb","request_id":"a","verb":"new"}"#).unwrap();
    assert!(matches!(plain, InboundMessage::TabVerb { agent: None, .. }));
    let c = parse_inbound_message(r#"{"type":"cycle_default_mode","request_id":"a"}"#).unwrap();
    assert_eq!(c.tab_ref(), TabRef::WindowLevel);
    let m = parse_inbound_message(r#"{"type":"tab_verb","request_id":"a","verb":"close_others"}"#).unwrap();
    assert!(matches!(
        m,
        InboundMessage::TabVerb {
            verb: TabVerbWire::CloseOthers,
            ..
        }
    ));
    let co = parse_inbound_message(r#"{"type":"close_others","request_id":"a"}"#).unwrap();
    assert_eq!(co.tab_ref(), TabRef::WindowLevel);
    assert_eq!(co.request_id(), "a");
}

#[test]
fn the_confirm_close_others_envelope_carries_the_ids_and_the_prompt() {
    let v: serde_json::Value = serde_json::from_str(&serialize_confirm_close_others_for_js(
        &[crate::tabs::TabId(2), crate::tabs::TabId(3)],
        &["close 2 other tabs? 1 running (y/n)".to_string()],
    ))
    .unwrap();
    assert_eq!(v["kind"], "confirm_close_others");
    assert_eq!(v["tabs"], serde_json::json!([2, 3]));
    assert_eq!(v["lines"], serde_json::json!(["close 2 other tabs? 1 running (y/n)"]));
}

#[test]
fn serializes_literal_key_and_open_keymap() {
    let value: serde_json::Value = serde_json::from_str(&serialize_literal_key_for_js("C-a")).unwrap();
    assert_eq!(value, serde_json::json!({ "kind": "literal_key", "key": "C-a" }));
    let value: serde_json::Value = serde_json::from_str(&serialize_open_keymap_for_js(None, 0)).unwrap();
    assert_eq!(value, serde_json::json!({ "kind": "open_keymap", "opening": 0 }));
}

#[test]
fn open_keymap_carries_the_opening() {
    let value: serde_json::Value = serde_json::from_str(&serialize_open_keymap_for_js(None, 7)).unwrap();
    assert_eq!(value, serde_json::json!({ "kind": "open_keymap", "opening": 7 }));
    let help = crate::plugin::keys::PluginKeysHelp {
        title: "Snotra".into(),
        declared: false,
        rows: Vec::new(),
    };
    let value: serde_json::Value = serde_json::from_str(&serialize_open_keymap_for_js(Some(&help), 8)).unwrap();
    assert_eq!(value["opening"], serde_json::json!(8));
    assert_eq!(value["kind"], serde_json::json!("open_keymap"));
}

/// `agent-ui/web/src/bridge.test.ts` pins `JSON.stringify` of the same message to these strings.
#[test]
fn keymap_closed_parses_with_both_return_keys_values() {
    for (json, want) in [
        (
            r#"{"type":"keymap_closed","request_id":"req-1","opening":7,"return_keys":true}"#,
            true,
        ),
        (
            r#"{"type":"keymap_closed","request_id":"req-2","opening":7,"return_keys":false}"#,
            false,
        ),
    ] {
        let msg = parse_inbound_message(json).unwrap();
        assert_eq!(msg.tab_ref(), TabRef::WindowLevel);
        match msg {
            InboundMessage::KeymapClosed {
                request_id,
                opening,
                return_keys,
            } => {
                assert!(request_id.starts_with("req-"), "{json}");
                assert_eq!(opening, 7, "{json}");
                assert_eq!(return_keys, want, "{json}");
            }
            other => panic!("{json}: {other:?}"),
        }
    }
}

#[test]
fn keymap_closed_without_opening_or_return_keys_is_malformed() {
    for json in [
        r#"{"type":"keymap_closed","request_id":"r","return_keys":true}"#,
        r#"{"type":"keymap_closed","request_id":"r","opening":7}"#,
        r#"{"type":"keymap_closed","opening":7,"return_keys":true}"#,
        r#"{"type":"keymap_closed","request_id":"r","opening":-1,"return_keys":true}"#,
        r#"{"type":"keymap_closed","request_id":"r","opening":7,"return_keys":null}"#,
    ] {
        assert!(parse_inbound_message(json).is_none(), "{json}");
    }
}

#[test]
fn open_keymap_carries_the_plugin_pages_section() {
    let help = crate::plugin::keys::PluginKeysHelp {
        title: "Snotra".into(),
        declared: true,
        rows: vec![crate::keymap::root::HelpRow {
            keys: "]q / <C-n>".into(),
            what: "<img src=x onerror=alert(1)>".into(),
        }],
    };
    let value: serde_json::Value = serde_json::from_str(&serialize_open_keymap_for_js(Some(&help), 3)).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "kind": "open_keymap",
            "opening": 3,
            "plugin": {
                "title": "Snotra",
                "declared": true,
                "rows": [{ "keys": "]q / <C-n>", "what": "<img src=x onerror=alert(1)>" }],
            },
        })
    );
    let empty = crate::plugin::keys::PluginKeysHelp {
        title: "Snotra".into(),
        declared: false,
        rows: Vec::new(),
    };
    let value: serde_json::Value = serde_json::from_str(&serialize_open_keymap_for_js(Some(&empty), 3)).unwrap();
    assert_eq!(value["plugin"]["rows"], serde_json::json!([]));
    assert_eq!(value["plugin"]["declared"], serde_json::json!(false));
}

#[test]
fn serializes_open_command_line() {
    let value: serde_json::Value = serde_json::from_str(&serialize_open_command_line_for_js()).unwrap();
    assert_eq!(value, serde_json::json!({ "kind": "open_command_line" }));
}

// Field order isn't pinned byte-for-byte (`serde_json`'s default map isn't insertion-ordered --
// the same reason `serializes_focus_permission` and the `confirm_close_others` test above parse
// rather than compare raw strings), so this checks the exact two wire spellings the Interfaces
// block gives ("down"/"up") through a parsed `Value` rather than a literal byte string.
#[test]
fn serializes_editor_typing_with_its_period() {
    let v: serde_json::Value = serde_json::from_str(&serialize_editor_typing_for_js(true, 500)).unwrap();
    assert_eq!(
        v,
        serde_json::json!({ "kind": "editor_typing", "typing": true, "periodMs": 500 })
    );
    let v: serde_json::Value = serde_json::from_str(&serialize_editor_typing_for_js(false, 1000)).unwrap();
    assert_eq!(
        v,
        serde_json::json!({ "kind": "editor_typing", "typing": false, "periodMs": 1000 })
    );
}

#[test]
fn serializes_nav_key_for_both_directions() {
    let v: serde_json::Value = serde_json::from_str(&serialize_nav_key_for_js(NavKeyDirection::Down)).unwrap();
    assert_eq!(v, serde_json::json!({ "kind": "nav_key", "direction": "down" }));
    let v: serde_json::Value = serde_json::from_str(&serialize_nav_key_for_js(NavKeyDirection::Up)).unwrap();
    assert_eq!(v, serde_json::json!({ "kind": "nav_key", "direction": "up" }));
}

/// The five page-to-Rust wire strings are pinned so `bridge.test.ts` can serialize the same
/// messages on the page side.
#[test]
fn parses_the_five_panel_keys_and_nav_fallthrough_wire_strings() {
    let msg = parse_inbound_message(r#"{"type":"panel_keys","request_id":"req-7","mode":"browse"}"#).unwrap();
    assert_eq!(msg.tab_ref(), TabRef::WindowLevel);
    assert_eq!(msg.request_id(), "req-7");
    assert!(matches!(
        msg,
        InboundMessage::PanelKeys {
            mode: PanelKeys::Browse,
            ..
        }
    ));

    let msg = parse_inbound_message(r#"{"type":"panel_keys","request_id":"req-8","mode":"input"}"#).unwrap();
    assert_eq!(msg.request_id(), "req-8");
    assert!(matches!(
        msg,
        InboundMessage::PanelKeys {
            mode: PanelKeys::Input,
            ..
        }
    ));

    let msg = parse_inbound_message(r#"{"type":"panel_keys","request_id":"req-9","mode":"other"}"#).unwrap();
    assert_eq!(msg.request_id(), "req-9");
    assert!(matches!(
        msg,
        InboundMessage::PanelKeys {
            mode: PanelKeys::Other,
            ..
        }
    ));

    let msg = parse_inbound_message(r#"{"type":"nav_fallthrough","request_id":"req-10","direction":"down"}"#).unwrap();
    assert_eq!(msg.tab_ref(), TabRef::WindowLevel);
    assert_eq!(msg.request_id(), "req-10");
    assert!(matches!(
        msg,
        InboundMessage::NavFallthrough {
            direction: NavKeyDirection::Down,
            ..
        }
    ));

    let msg = parse_inbound_message(r#"{"type":"nav_fallthrough","request_id":"req-11","direction":"up"}"#).unwrap();
    assert_eq!(msg.request_id(), "req-11");
    assert!(matches!(
        msg,
        InboundMessage::NavFallthrough {
            direction: NavKeyDirection::Up,
            ..
        }
    ));
}

/// The dangerous default here is obvious and one-directional (the same reasoning as
/// `DecisionChoice`'s own doc): an unrecognised `mode`/`direction` must be a parse failure, never
/// silently fall back to `Other`/some direction a later `match` picked for it.
#[test]
fn an_unknown_panel_keys_mode_or_nav_fallthrough_direction_fails_to_parse() {
    assert!(parse_inbound_message(r#"{"type":"panel_keys","request_id":"req-7","mode":"sideways"}"#).is_none());
    assert!(parse_inbound_message(r#"{"type":"nav_fallthrough","request_id":"req-10","direction":"left"}"#).is_none());
}

/// `Ctrl+w h/j/k/l` in BROWSE sends a window-level move without a tab id. All four directions
/// carry the request id through, whichever tab the panel shows.
#[test]
fn parses_pane_nav_in_all_four_directions_as_a_window_level_message() {
    let msg = parse_inbound_message(r#"{"type":"pane_nav","request_id":"r","direction":"left"}"#).unwrap();
    assert_eq!(msg.tab_ref(), TabRef::WindowLevel);
    assert_eq!(msg.request_id(), "r");
    assert!(matches!(
        msg,
        InboundMessage::PaneNav {
            direction: PaneNavDirection::Left,
            ..
        }
    ));
    // The four strings `agent-ui/web/src/bridge.test.ts` pins `JSON.stringify` to, byte for byte.
    for (json, id, want) in [
        (
            r#"{"type":"pane_nav","request_id":"req-1","direction":"left"}"#,
            "req-1",
            PaneNavDirection::Left,
        ),
        (
            r#"{"type":"pane_nav","request_id":"req-2","direction":"down"}"#,
            "req-2",
            PaneNavDirection::Down,
        ),
        (
            r#"{"type":"pane_nav","request_id":"req-3","direction":"up"}"#,
            "req-3",
            PaneNavDirection::Up,
        ),
        (
            r#"{"type":"pane_nav","request_id":"req-4","direction":"right"}"#,
            "req-4",
            PaneNavDirection::Right,
        ),
    ] {
        match parse_inbound_message(json) {
            Some(InboundMessage::PaneNav { request_id, direction }) => {
                assert_eq!(direction, want, "{json}");
                assert_eq!(request_id, id, "{json}");
            }
            other => panic!("{json}: {other:?}"),
        }
    }
}

/// Unknown directions fail parsing rather than becoming a default move in a later match.
/// `sideways`, `Left` and `north` check unsupported words and case; missing directions or
/// request ids are malformed too.
#[test]
fn an_unknown_missing_or_misspelled_pane_nav_direction_fails_to_parse() {
    for direction in ["sideways", "Left", "north", "", "h"] {
        let json = format!(r#"{{"type":"pane_nav","request_id":"r","direction":"{direction}"}}"#);
        assert!(parse_inbound_message(&json).is_none(), "{direction:?}");
    }
    assert!(parse_inbound_message(r#"{"type":"pane_nav","request_id":"r"}"#).is_none());
    assert!(parse_inbound_message(r#"{"type":"pane_nav","direction":"left"}"#).is_none());
    assert!(parse_inbound_message(r#"{"type":"pane_nav","request_id":"r","direction":null}"#).is_none());
}

/// `gx` opens a web link through window-level `open_url`, without a tab id. The request id and
/// chosen address match the exact JSON string pinned by `agent-ui/web/src/bridge.test.ts`.
#[test]
fn parses_open_url_as_a_window_level_message() {
    let json = r#"{"type":"open_url","request_id":"req-1","url":"https://example.com/a"}"#;
    let msg = parse_inbound_message(json).unwrap();
    assert_eq!(msg.tab_ref(), TabRef::WindowLevel);
    assert_eq!(msg.request_id(), "req-1");
    match msg {
        InboundMessage::OpenUrl { request_id, url } => {
            assert_eq!(request_id, "req-1");
            assert_eq!(url, "https://example.com/a");
        }
        other => panic!("{other:?}"),
    }
}

/// A message with no address, a `null` one, a non-string one or no request id is malformed like any
/// other: never "open nothing", and never an address some later `match` invents for it.
#[test]
fn an_open_url_with_no_url_or_request_id_fails_to_parse() {
    for bad in [
        r#"{"type":"open_url","request_id":"r"}"#,
        r#"{"type":"open_url","request_id":"r","url":null}"#,
        r#"{"type":"open_url","request_id":"r","url":5}"#,
        r#"{"type":"open_url","url":"https://example.com/"}"#,
    ] {
        assert!(parse_inbound_message(bad).is_none(), "{bad}");
    }
}

/// The two remote-control answers are window-level, and name the question, the answer and (for a phone) the grant,
/// none of them defaulted: an answer that does not say what it answers, or grants something unknown, is malformed.
#[test]
fn remote_answers_parse_as_window_level_and_need_every_field() {
    let enroll = parse_inbound_message(
        r#"{"type":"remote_enroll_answer","request_id":"r1","nonce":4,"yes":true,"grant":"watch+steer"}"#,
    )
    .unwrap();
    assert_eq!(enroll.tab_ref(), TabRef::WindowLevel);
    assert_eq!(enroll.request_id(), "r1");
    match enroll {
        InboundMessage::RemoteEnrollAnswer { nonce, yes, grant, .. } => {
            assert_eq!((nonce, yes, grant), (4, true, crate::remote_devices::Grant::WatchSteer))
        }
        other => panic!("{other:?}"),
    }
    let listen =
        parse_inbound_message(r#"{"type":"remote_listen_answer","request_id":"r2","nonce":5,"yes":false}"#).unwrap();
    assert_eq!(listen.tab_ref(), TabRef::WindowLevel);
    assert!(matches!(
        listen,
        InboundMessage::RemoteListenAnswer {
            nonce: 5,
            yes: false,
            ..
        }
    ));
    for bad in [
        r#"{"type":"remote_enroll_answer","request_id":"r","nonce":4,"yes":true}"#,
        r#"{"type":"remote_enroll_answer","request_id":"r","nonce":4,"yes":true,"grant":"root"}"#,
        r#"{"type":"remote_enroll_answer","request_id":"r","yes":true,"grant":"watch"}"#,
        r#"{"type":"remote_listen_answer","request_id":"r","nonce":5}"#,
        r#"{"type":"remote_listen_answer","request_id":"r","yes":true}"#,
    ] {
        assert!(parse_inbound_message(bad).is_none(), "{bad}");
    }
}

/// The device list's page kinds are window-level and name every field: the recipe a code is made by is one of three
/// words, and a revoke answers one question about one device.
#[test]
fn device_list_kinds_parse_as_window_level_and_need_every_field() {
    let open = parse_inbound_message(r#"{"type":"remote_devices_open","request_id":"o1"}"#).unwrap();
    assert_eq!(open.tab_ref(), TabRef::WindowLevel);
    assert_eq!(open.request_id(), "o1");
    for (word, mode) in [
        ("auto", None),
        ("ssh", Some(crate::remote_pairing::PairMode::Ssh)),
        ("direct", Some(crate::remote_pairing::PairMode::Direct)),
    ] {
        let start = parse_inbound_message(&format!(
            r#"{{"type":"remote_start_enroll","request_id":"s1","recipe":"{word}"}}"#
        ))
        .unwrap();
        assert_eq!(start.tab_ref(), TabRef::WindowLevel);
        match start {
            InboundMessage::RemoteStartEnroll { recipe, .. } => assert_eq!(recipe.mode(), mode, "{word}"),
            other => panic!("{other:?}"),
        }
    }
    let ask = parse_inbound_message(r#"{"type":"remote_revoke_ask","request_id":"a1","device":"d1"}"#).unwrap();
    assert_eq!(ask.tab_ref(), TabRef::WindowLevel);
    let revoke =
        parse_inbound_message(r#"{"type":"remote_revoke","request_id":"v1","device":"d1","nonce":7}"#).unwrap();
    assert_eq!(revoke.tab_ref(), TabRef::WindowLevel);
    assert_eq!(revoke.request_id(), "v1");
    match revoke {
        InboundMessage::RemoteRevoke { device, nonce, .. } => assert_eq!((device.as_str(), nonce), ("d1", 7)),
        other => panic!("{other:?}"),
    }
    for bad in [
        r#"{"type":"remote_start_enroll","request_id":"r"}"#,
        r#"{"type":"remote_start_enroll","request_id":"r","recipe":"tor"}"#,
        r#"{"type":"remote_revoke_ask","request_id":"r"}"#,
        r#"{"type":"remote_revoke","request_id":"r","device":"d1"}"#,
        r#"{"type":"remote_revoke","request_id":"r","nonce":7}"#,
    ] {
        assert!(parse_inbound_message(bad).is_none(), "{bad}");
    }
}
