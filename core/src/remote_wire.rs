//! JSON frames shared by remote observation and the phone's inbound transport.

use crate::agents::{wire, Change, View};
use crate::remote_rv::{PairMode, Secret};
use serde::Deserialize;
use serde_json::{json, Value};

pub const REMOTE_PROTOCOL: u32 = 1;
/// Maximum UTF-8 bytes in one inbound JSON line.
pub const MAX_PHONE_FRAME_BYTES: usize = 1024 * 1024;

/// The versioned view a plugin's agents watch answers with, wrapped for a phone.
pub fn agents_view_frame(view: &View) -> String {
    let (_, view) = wire::page_event(&Change::Reset { view: view.clone() });
    json!({ "v": REMOTE_PROTOCOL, "kind": "agents_view", "view": view }).to_string()
}

/// Wraps the same callback argument that plugin pages receive.
pub fn agents_change_frame(change: &Change) -> String {
    let (_, payload) = wire::page_event(change);
    let mut frame = json!({ "v": REMOTE_PROTOCOL });
    let (kind, field) = match change {
        Change::Row { .. } => ("agents_row", "row"),
        Change::Gone { .. } => ("agents_gone", ""),
        Change::Reset { .. } => ("agents_reset", "view"),
        Change::Selection { .. } => ("agents_selection", "selection"),
        Change::Round { .. } => ("agents_round", "round"),
    };
    frame["kind"] = kind.into();
    if matches!(change, Change::Gone { .. }) {
        frame
            .as_object_mut()
            .unwrap()
            .extend(payload.as_object().unwrap().clone());
    } else {
        frame[field] = payload;
    }
    frame.to_string()
}

/// What a phone may send `serve`. Any other kind, and any field not named here, fails to parse. A credential is a
/// [`Secret`], which `Debug` never shows.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PhoneInbound {
    /// The first frame of a connection. A phone that is enrolling has no token yet.
    Hello {
        device: String,
        token: Option<Secret>,
        layout: String,
        protocol: u32,
    },
    #[serde(deserialize_with = "empty_body")]
    SubscribeRows,
    Subscribe {
        key: String,
    },
    Unsubscribe {
        key: String,
    },
    /// The turns of `key` before `before_turn`, of the session `generation`.
    More {
        key: String,
        generation: u64,
        before_turn: u64,
    },
    Act {
        key: String,
        cmd: Value,
    },
    /// Keeps a quiet connection open.
    #[serde(deserialize_with = "empty_body")]
    Ping,
    /// Enrolls with the one-time `code` that the window `instance` showed, as `device_name`, by `recipe`: the pairing
    /// URI's `m`, the recipe the phone used to reach `serve`. A phone that reached it another way says so, and the
    /// window refuses the enrollment rather than store it under the code's recipe.
    Enroll {
        instance: String,
        code: Secret,
        device_name: String,
        recipe: PairMode,
    },
    /// The phone takes itself out of the store.
    #[serde(deserialize_with = "empty_body")]
    RevokeSelf,
    /// Where the phone's notifications go.
    SetPushEndpoint {
        url: Secret,
    },
    /// The newest audit records about this phone itself, at most `limit` (and never more than `serve` allows).
    AuditMine {
        limit: u32,
    },
}

// Serde's internally tagged unit visitor ignores fields; use an empty struct to reject them.
fn empty_body<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<(), D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Empty {}
    Empty::deserialize(deserializer).map(|_| ())
}

#[derive(Debug)]
pub struct PhoneFrame {
    pub v: u32,
    pub seq: u64,
    pub ack: u64,
    pub body: PhoneInbound,
}

/// Parses one bounded line. The transport enforces its read deadline before handing the line here.
pub fn parse_phone_frame(line: &str) -> Result<PhoneFrame, String> {
    if line.len() > MAX_PHONE_FRAME_BYTES {
        return Err("phone frame exceeds its size limit".into());
    }
    if line.contains(['\n', '\r']) {
        return Err("phone frame must be one line".into());
    }
    #[derive(Deserialize)]
    struct Envelope {
        v: u32,
        seq: u64,
        ack: u64,
        #[serde(flatten)]
        body: serde_json::Map<String, Value>,
    }
    let envelope: Envelope = serde_json::from_str(line).map_err(|e| e.to_string())?;
    if envelope.v != REMOTE_PROTOCOL {
        return Err(format!("unsupported remote protocol {}", envelope.v));
    }
    let body = serde_json::from_value(Value::Object(envelope.body)).map_err(|e| e.to_string())?;
    Ok(PhoneFrame {
        v: envelope.v,
        seq: envelope.seq,
        ack: envelope.ack,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{wire, Change, GoneWhy, Row, View, SCHEMA_V};
    use serde_json::{json, Value};

    #[test]
    fn view_frame_carries_the_watch_answer() {
        let view = View {
            rows: vec![Row {
                label: "hello".into(),
                ..Row::default()
            }],
            ..View::default()
        };
        let frame: Value = serde_json::from_str(&agents_view_frame(&view)).unwrap();
        assert_eq!(frame["v"], 1);
        assert_eq!(frame["kind"], "agents_view");
        assert_eq!(
            frame["view"],
            serde_json::from_str::<Value>(&wire::view_json(&view, SCHEMA_V)).unwrap()
        );
    }

    #[test]
    fn change_frames_carry_the_page_payloads() {
        let changes = [
            Change::Row { row: Row::default() },
            Change::Gone {
                id: "a.1".into(),
                key: "k-1".into(),
                why: GoneWhy::Moved,
            },
            Change::Reset { view: View::default() },
            Change::Selection {
                selection: Some(Default::default()),
            },
            Change::Round {
                round: Some(Default::default()),
            },
            Change::Selection { selection: None },
            Change::Round { round: None },
        ];
        for change in changes {
            let (event, payload) = wire::page_event(&change);
            let frame: Value = serde_json::from_str(&agents_change_frame(&change)).unwrap();
            assert_eq!(frame["v"], 1);
            assert_eq!(frame["kind"], event.replace('.', "_"));
            match change {
                Change::Gone { .. } => {
                    for field in ["id", "key", "why"] {
                        assert_eq!(frame[field], payload[field]);
                    }
                }
                _ => {
                    let field = match change {
                        Change::Row { .. } => "row",
                        Change::Reset { .. } => "view",
                        Change::Selection { .. } => "selection",
                        Change::Round { .. } => "round",
                        _ => unreachable!(),
                    };
                    assert_eq!(frame[field], payload);
                }
            }
        }
    }

    #[test]
    fn phone_frame_needs_its_envelope() {
        let line = r#"{"v":1,"seq":1,"ack":0,"kind":"subscribe_rows"}"#;
        let frame = parse_phone_frame(line).unwrap();
        assert_eq!((frame.v, frame.seq, frame.ack), (1, 1, 0));
        assert!(matches!(frame.body, PhoneInbound::SubscribeRows));
        for line in [
            r#"{"seq":1,"ack":0,"kind":"subscribe_rows"}"#,
            r#"{"v":2,"seq":1,"ack":0,"kind":"subscribe_rows"}"#,
            r#"{"v":1,"ack":0,"kind":"subscribe_rows"}"#,
            r#"{"v":1,"seq":1,"kind":"subscribe_rows"}"#,
        ] {
            assert!(parse_phone_frame(line).is_err());
        }
    }

    #[test]
    fn phone_inbound_has_no_desk_agents_calls() {
        for body in [
            json!({"kind":"select"}),
            json!({"kind":"round","op":"start"}),
            json!({"kind":"focus","id":"x"}),
            json!({"kind":"new","project":"p"}),
            json!({"kind":"stop","id":"x"}),
        ] {
            assert!(serde_json::from_value::<PhoneInbound>(body).is_err());
        }
    }

    #[test]
    fn phone_inbound_refuses_unknown_fields() {
        assert!(parse_phone_frame(r#"{"v":1,"seq":1,"ack":0,"kind":"subscribe_rows","x":1}"#).is_err());
        assert!(serde_json::from_value::<PhoneInbound>(json!({"kind":"subscribe_rows","x":1})).is_err());
    }

    #[test]
    fn phone_frame_is_bounded_to_one_json_line() {
        let line = r#"{"v":1,"seq":1,"ack":0,"kind":"subscribe_rows"}"#;
        assert!(parse_phone_frame(&format!("{line}\n{line}")).is_err());
        assert!(parse_phone_frame(&" ".repeat(MAX_PHONE_FRAME_BYTES + 1)).is_err());
        assert!(parse_phone_frame(&format!("{line} {line}")).is_err());
        assert!(parse_phone_frame(r#"{"v":1,"seq":-1,"ack":0,"kind":"subscribe_rows"}"#).is_err());
        assert!(parse_phone_frame(r#"{"v":1,"seq":1,"ack":"0","kind":"subscribe_rows"}"#).is_err());
    }

    #[test]
    fn every_new_phone_kind_parses_strictly() {
        for body in [
            json!({"kind":"ping"}),
            json!({"kind":"more", "key":"k-1", "generation":3, "before_turn":12}),
            json!({"kind":"enroll", "instance":"a3f09c1d2e4b5f60", "code":"c0de", "device_name":"Pixel", "recipe":"ssh"}),
            json!({"kind":"enroll", "instance":"a3f09c1d2e4b5f60", "code":"c0de", "device_name":"Pixel", "recipe":"direct"}),
            json!({"kind":"revoke_self"}),
            json!({"kind":"set_push_endpoint", "url":"https://ntfy.example/t"}),
            json!({"kind":"audit_mine", "limit":20}),
            json!({"kind":"hello", "device":"phone", "layout":"slab", "protocol":1}),
            json!({"kind":"hello", "device":"phone", "token":"secret", "layout":"slab", "protocol":1}),
        ] {
            let mut frame = body.clone();
            frame["v"] = json!(1);
            frame["seq"] = json!(1);
            frame["ack"] = json!(0);
            let parsed = parse_phone_frame(&frame.to_string()).unwrap_or_else(|e| panic!("{frame}: {e}"));
            let shown = format!("{:?}", parsed.body);
            assert!(
                !shown.contains("c0de") && !shown.contains("secret") && !shown.contains("ntfy.example"),
                "{shown}"
            );
            frame["unknown"] = json!(true);
            assert!(parse_phone_frame(&frame.to_string()).is_err(), "{frame}");
        }
        let line = r#"{"v":1,"seq":1,"ack":0,"kind":"hello","device":"phone","layout":"slab","protocol":1}"#;
        assert!(matches!(
            parse_phone_frame(line).unwrap().body,
            PhoneInbound::Hello { token: None, .. }
        ));
        for body in [
            json!({"kind":"more", "key":"k-1", "generation":3}),
            json!({"kind":"enroll", "code":"c0de", "device_name":"Pixel"}),
            // The recipe is required, and only the two the pairing URI names.
            json!({"kind":"enroll", "instance":"a3f09c1d2e4b5f60", "code":"c0de", "device_name":"Pixel"}),
            json!({"kind":"enroll", "instance":"a3f09c1d2e4b5f60", "code":"c0de", "device_name":"Pixel", "recipe":"tor"}),
            json!({"kind":"ping", "x":1}),
            json!({"kind":"revoke_self", "token":"t"}),
            json!({"kind":"audit_mine"}),
            json!({"kind":"audit_mine", "limit":-1}),
            json!({"kind":"audit_mine", "limit":20, "device":"d1"}),
        ] {
            assert!(serde_json::from_value::<PhoneInbound>(body.clone()).is_err(), "{body}");
        }
    }

    #[test]
    fn phone_frames_carry_only_the_declared_body_fields() {
        for body in [
            json!({"kind":"hello", "device":"phone", "token":"secret", "layout":"slab", "protocol":1}),
            json!({"kind":"subscribe", "key":"k-1"}),
            json!({"kind":"unsubscribe", "key":"k-1"}),
            json!({"kind":"act", "key":"k-1", "cmd":{"kind":"interrupt", "expect_work":{"turn":3}}}),
        ] {
            let mut frame = body;
            frame["v"] = json!(1);
            frame["seq"] = json!(2);
            frame["ack"] = json!(1);
            let parsed = parse_phone_frame(&frame.to_string()).unwrap();
            assert_eq!((parsed.v, parsed.seq, parsed.ack), (1, 2, 1));
            if let PhoneInbound::Act { cmd, .. } = parsed.body {
                assert_eq!(cmd, frame["cmd"]);
            }
            frame["unknown"] = json!(true);
            assert!(parse_phone_frame(&frame.to_string()).is_err());
        }
    }
}
