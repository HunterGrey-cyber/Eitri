//! What travels on the link: one JSON object per packet, with `"v"` (the protocol version) and `"k"` (the kind)
//! in every one. A message longer than [`INLINE_MAX`] travels as a body: its packet carries only `v`, `k` and
//! `body: {len}`, and exactly one sealed memfd holding the JSON. The link carries intents and each window's own
//! summary, never one window's focus, layout or selection mirrored into the other.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::editor_op::EditorOp;

pub const PROTOCOL_VERSION: u32 = 1;
/// The longest JSON sent in a packet of its own; anything longer goes as a body.
pub const INLINE_MAX: usize = 64 * 1024;
/// The most descriptors one `move` may carry, a body's memfd not counted.
pub const MAX_MOVE_FDS: usize = 16;

const MIB: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    Started,
    Ended,
}

/// A turn of the sending window that started or ended, at its `CLOCK_MONOTONIC` time in nanoseconds (one boot, so
/// the two windows' times compare).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnEdge {
    pub session: String,
    pub edge: EdgeKind,
    pub at_ns: u64,
}

/// What window 1's review driver did with one of window 2's overlay calls, or with a path window 2 drew.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Drawn {
    Drawn,
    Kept,
    Cleared,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "to", rename_all = "snake_case")]
pub enum FocusTarget {
    /// A module the receiving window holds, by its id.
    Module { id: String },
    /// Keys arriving across the grid's edge: the receiving window gives them to its module at the opposite edge.
    Edge { side: Side },
    /// The receiving window's last focused module.
    Window,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "what", rename_all = "snake_case")]
pub enum EditorRequest {
    /// `Ctrl+g` from a chat tab: the scratch draft opens in the editor and is written back to the tab's holder.
    Scratch { tab: u64, session: Option<String> },
    /// A path click or `gf`; `path` is relative to the project root.
    OpenPath {
        path: String,
        line: Option<u32>,
        col: Option<u32>,
    },
    /// Opening a reviewed file from the review list.
    ReviewOpen { path: String, line: Option<u32> },
    /// The editor key pressed in window 2.
    Focus,
}

/// The answer to one editor operation. A Lua call that returns nil is `Ok { value: Null }`: an `Option<Value>`
/// would read a JSON `null` back as "no answer".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "r", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReplyResult {
    Ok { value: Value },
    Err { why: String },
}

/// One window's own counts and workspace names, for the other window's top bar and chooser.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub waiting: u32,
    pub unseen: u32,
    pub running: u32,
    pub queued: u32,
    pub workspaces: Vec<String>,
}

/// Where a moving module sits in the source: tiled, or floating at a rectangle relative to the grid (0 to 1).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "shape", rename_all = "snake_case")]
pub enum Shape {
    Tiled,
    Float { x: f64, y: f64, w: f64, h: f64 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum Msg {
    Hello {
        build: String,
        window: String,
        pid: u32,
        start: u64,
        presence_ready: bool,
        running: Vec<TurnEdge>,
        summary: Summary,
    },
    Welcome {
        window: String,
        presence_ready: bool,
        running: Vec<TurnEdge>,
        summary: Summary,
        holders: BTreeMap<String, String>,
        context: Option<Value>,
        theme: Option<Value>,
        keys: Option<Value>,
    },
    Bye {
        why: String,
    },
    Context {
        payload: Option<Value>,
    },
    Theme {
        payload: Value,
    },
    Keys {
        payload: Value,
    },
    EditorOp {
        id: u64,
        op: EditorOp,
    },
    EditorReply {
        id: u64,
        result: ReplyResult,
    },
    ReviewEvents {
        session: String,
        events: Vec<Value>,
    },
    ReviewDrawn {
        id: Option<u64>,
        path: String,
        state: Drawn,
    },
    ReviewIdle,
    ReviewResult {
        session: String,
        job: u64,
        result: Value,
    },
    TurnEdge {
        session: String,
        edge: EdgeKind,
        at_ns: u64,
    },
    Presence {
        held: bool,
    },
    PresenceAck {
        held: bool,
    },
    Editor {
        request: EditorRequest,
        token: Option<String>,
    },
    DraftBack {
        tab: u64,
        session: Option<String>,
        text: String,
    },
    Follow {
        owner: String,
        line: String,
    },
    Focus {
        target: FocusTarget,
        token: Option<String>,
    },
    Summary(Summary),
    Monitor {
        connector: String,
        description: String,
    },
    Workspace {
        name: String,
        token: Option<String>,
    },
    MoveOffer {
        #[serde(rename = "move")]
        mv: u64,
        module: String,
        kind: String,
        shape: Shape,
        size: (u32, u32),
        session: Option<String>,
    },
    MoveReady {
        #[serde(rename = "move")]
        mv: u64,
    },
    MoveRefused {
        #[serde(rename = "move")]
        mv: u64,
        why: String,
    },
    Move {
        #[serde(rename = "move")]
        mv: u64,
        record: Value,
    },
    MoveDone {
        #[serde(rename = "move")]
        mv: u64,
    },
    MoveAbort {
        #[serde(rename = "move")]
        mv: u64,
        why: String,
    },
    Swap {
        move_a: u64,
        move_b: u64,
    },
    ChildExited {
        pid: u32,
        start: u64,
        status: Option<i32>,
    },
    Away {
        ids: Vec<String>,
    },
}

/// Every kind this build knows; any other is refused by name.
pub const KINDS: &[&str] = &[
    "hello",
    "welcome",
    "bye",
    "context",
    "theme",
    "keys",
    "editor_op",
    "editor_reply",
    "review_events",
    "review_drawn",
    "review_idle",
    "review_result",
    "turn_edge",
    "presence",
    "presence_ack",
    "editor",
    "draft_back",
    "follow",
    "focus",
    "summary",
    "monitor",
    "workspace",
    "move_offer",
    "move_ready",
    "move_refused",
    "move",
    "move_done",
    "move_abort",
    "swap",
    "child_exited",
    "away",
];

impl Msg {
    pub fn kind(&self) -> &'static str {
        match self {
            Msg::Hello { .. } => "hello",
            Msg::Welcome { .. } => "welcome",
            Msg::Bye { .. } => "bye",
            Msg::Context { .. } => "context",
            Msg::Theme { .. } => "theme",
            Msg::Keys { .. } => "keys",
            Msg::EditorOp { .. } => "editor_op",
            Msg::EditorReply { .. } => "editor_reply",
            Msg::ReviewEvents { .. } => "review_events",
            Msg::ReviewDrawn { .. } => "review_drawn",
            Msg::ReviewIdle => "review_idle",
            Msg::ReviewResult { .. } => "review_result",
            Msg::TurnEdge { .. } => "turn_edge",
            Msg::Presence { .. } => "presence",
            Msg::PresenceAck { .. } => "presence_ack",
            Msg::Editor { .. } => "editor",
            Msg::DraftBack { .. } => "draft_back",
            Msg::Follow { .. } => "follow",
            Msg::Focus { .. } => "focus",
            Msg::Summary(_) => "summary",
            Msg::Monitor { .. } => "monitor",
            Msg::Workspace { .. } => "workspace",
            Msg::MoveOffer { .. } => "move_offer",
            Msg::MoveReady { .. } => "move_ready",
            Msg::MoveRefused { .. } => "move_refused",
            Msg::Move { .. } => "move",
            Msg::MoveDone { .. } => "move_done",
            Msg::MoveAbort { .. } => "move_abort",
            Msg::Swap { .. } => "swap",
            Msg::ChildExited { .. } => "child_exited",
            Msg::Away { .. } => "away",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    Version(u64),
    UnknownKind(String),
    Malformed(String),
    TooLarge { kind: String, len: usize, cap: usize },
    NoBody(String),
    Fds { kind: String, count: usize },
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkError::Version(v) => write!(
                f,
                "the other window speaks link version {v}, this one {PROTOCOL_VERSION}"
            ),
            LinkError::UnknownKind(k) => write!(f, "{k:?} is not a kind this window knows"),
            LinkError::Malformed(why) => write!(f, "a malformed message: {why}"),
            LinkError::TooLarge { kind, len, cap } => write!(f, "{kind} of {len} bytes is over its {cap}-byte cap"),
            LinkError::NoBody(kind) => write!(f, "{kind} never travels as a body"),
            LinkError::Fds { kind, count } => write!(f, "{kind} cannot carry {count} descriptors"),
        }
    }
}

impl std::error::Error for LinkError {}

/// The largest body `kind` may have; 0 for a kind that must fit one packet.
pub fn body_cap(kind: &str) -> usize {
    match kind {
        "editor_op" | "editor_reply" => 16 * MIB,
        "review_events" => 8 * MIB,
        "move" => 128 * MIB,
        "context" | "theme" | "keys" => MIB,
        "welcome" => 3 * MIB,
        _ => 0,
    }
}

/// `msg` as the JSON of one packet or body, with the version added.
pub fn to_json(msg: &Msg) -> Vec<u8> {
    let mut value = serde_json::to_value(msg).expect("every message serialises");
    if let Value::Object(map) = &mut value {
        map.insert("v".into(), Value::from(PROTOCOL_VERSION));
    }
    serde_json::to_vec(&value).expect("a JSON value serialises")
}

/// One message read back from its JSON (a packet's, or a body's).
pub fn from_json(bytes: &[u8]) -> Result<Msg, LinkError> {
    let mut value: Value = serde_json::from_slice(bytes).map_err(|e| LinkError::Malformed(e.to_string()))?;
    let map = value
        .as_object_mut()
        .ok_or_else(|| LinkError::Malformed("not a JSON object".into()))?;
    let version = map
        .remove("v")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| LinkError::Malformed("no version".into()))?;
    if version != u64::from(PROTOCOL_VERSION) {
        return Err(LinkError::Version(version));
    }
    let kind = map
        .get("k")
        .and_then(Value::as_str)
        .ok_or_else(|| LinkError::Malformed("no kind".into()))?
        .to_owned();
    if !KINDS.contains(&kind.as_str()) {
        return Err(LinkError::UnknownKind(kind));
    }
    if map.contains_key("body") {
        return Err(LinkError::Malformed(format!("{kind}: a body header is not a message")));
    }
    serde_json::from_value(value).map_err(|e| LinkError::Malformed(format!("{kind}: {e}")))
}

/// What a received packet is: a whole message, or the header of a body that its memfd holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Header {
    Inline,
    Body { kind: String, len: usize },
}

/// Reads a packet's header fields only. A body header names a known kind whose cap allows `len`.
pub fn read_header(bytes: &[u8]) -> Result<Header, LinkError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|e| LinkError::Malformed(e.to_string()))?;
    let map = value
        .as_object()
        .ok_or_else(|| LinkError::Malformed("not a JSON object".into()))?;
    let Some(body) = map.get("body") else {
        return Ok(Header::Inline);
    };
    let version = map.get("v").and_then(Value::as_u64).unwrap_or(0);
    if version != u64::from(PROTOCOL_VERSION) {
        return Err(LinkError::Version(version));
    }
    let kind = map
        .get("k")
        .and_then(Value::as_str)
        .ok_or_else(|| LinkError::Malformed("no kind".into()))?
        .to_owned();
    if !KINDS.contains(&kind.as_str()) {
        return Err(LinkError::UnknownKind(kind));
    }
    if map.len() != 3 {
        return Err(LinkError::Malformed(format!(
            "{kind}: a body header carries v, k and body only"
        )));
    }
    let len = body
        .get("len")
        .and_then(Value::as_u64)
        .ok_or_else(|| LinkError::Malformed(format!("{kind}: a body without a length")))? as usize;
    let cap = body_cap(&kind);
    if cap == 0 {
        return Err(LinkError::NoBody(kind));
    }
    if len > cap {
        return Err(LinkError::TooLarge { kind, len, cap });
    }
    Ok(Header::Body { kind, len })
}

/// A message ready to send: its own packet, or a header packet and the JSON its memfd will hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Inline(Vec<u8>),
    Body { header: Vec<u8>, json: Vec<u8> },
}

pub fn frame(msg: &Msg) -> Result<Frame, LinkError> {
    let json = to_json(msg);
    if json.len() <= INLINE_MAX {
        return Ok(Frame::Inline(json));
    }
    let kind = msg.kind();
    let cap = body_cap(kind);
    if cap == 0 || json.len() > cap {
        return Err(LinkError::TooLarge {
            kind: kind.to_owned(),
            len: json.len(),
            cap: if cap == 0 { INLINE_MAX } else { cap },
        });
    }
    let header = serde_json::to_vec(&serde_json::json!({
        "v": PROTOCOL_VERSION,
        "k": kind,
        "body": { "len": json.len() },
    }))
    .expect("a JSON value serialises");
    Ok(Frame::Body { header, json })
}

/// Whether a packet of `kind` may carry `count` descriptors; `body` says the first of them is its body's memfd.
pub fn check_fds(kind: &str, body: bool, count: usize) -> Result<(), LinkError> {
    let refuse = || LinkError::Fds {
        kind: kind.to_owned(),
        count,
    };
    let own = if body {
        count.checked_sub(1).ok_or_else(refuse)?
    } else {
        count
    };
    let allowed = match kind {
        "welcome" => (1, 1),
        "move" => (0, MAX_MOVE_FDS),
        _ => (0, 0),
    };
    if own < allowed.0 || own > allowed.1 {
        return Err(refuse());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window_link::editor_op::EditorOp;

    fn one_of_each() -> Vec<Msg> {
        let edge = TurnEdge {
            session: "s".into(),
            edge: EdgeKind::Started,
            at_ns: 7,
        };
        vec![
            Msg::Hello {
                build: "b".into(),
                window: "2".into(),
                pid: 10,
                start: 11,
                presence_ready: true,
                running: vec![edge.clone()],
                summary: Summary::default(),
            },
            Msg::Welcome {
                window: "1".into(),
                presence_ready: false,
                running: vec![],
                summary: Summary {
                    waiting: 1,
                    unseen: 2,
                    running: 3,
                    queued: 4,
                    workspaces: vec!["main".into()],
                },
                holders: BTreeMap::from([("terminal#2".to_string(), "2".to_string())]),
                context: Some(serde_json::json!({"file": "a"})),
                theme: None,
                keys: None,
            },
            Msg::Bye { why: "closed".into() },
            Msg::Context { payload: None },
            Msg::Theme {
                payload: serde_json::json!({}),
            },
            Msg::Keys {
                payload: serde_json::json!([]),
            },
            Msg::EditorOp {
                id: 1,
                op: EditorOp::ReviewClearMine {},
            },
            Msg::EditorReply {
                id: 1,
                result: ReplyResult::Ok { value: Value::Null },
            },
            Msg::ReviewEvents {
                session: "s".into(),
                events: vec![serde_json::json!({"kind": "revert"})],
            },
            Msg::ReviewDrawn {
                id: Some(3),
                path: "a".into(),
                state: Drawn::Kept,
            },
            Msg::ReviewIdle,
            Msg::ReviewResult {
                session: "s".into(),
                job: 2,
                result: serde_json::json!({"ok": true}),
            },
            Msg::TurnEdge {
                session: "s".into(),
                edge: EdgeKind::Ended,
                at_ns: 9,
            },
            Msg::Presence { held: false },
            Msg::PresenceAck { held: false },
            Msg::Editor {
                request: EditorRequest::OpenPath {
                    path: "src/a.rs".into(),
                    line: Some(3),
                    col: None,
                },
                token: Some("t".into()),
            },
            Msg::DraftBack {
                tab: 3,
                session: Some("s".into()),
                text: "hi".into(),
            },
            Msg::Follow {
                owner: "markdown".into(),
                line: "{}".into(),
            },
            Msg::Focus {
                target: FocusTarget::Edge { side: Side::Left },
                token: None,
            },
            Msg::Summary(Summary::default()),
            Msg::Monitor {
                connector: "DP-1".into(),
                description: "d".into(),
            },
            Msg::Workspace {
                name: "study".into(),
                token: None,
            },
            Msg::MoveOffer {
                mv: 5,
                module: "terminal#2".into(),
                kind: "terminal".into(),
                shape: Shape::Float {
                    x: 0.1,
                    y: 0.2,
                    w: 0.5,
                    h: 0.5,
                },
                size: (800, 600),
                session: None,
            },
            Msg::MoveReady { mv: 5 },
            Msg::MoveRefused {
                mv: 5,
                why: "no".into(),
            },
            Msg::Move {
                mv: 5,
                record: serde_json::json!({"kind": "terminal"}),
            },
            Msg::MoveDone { mv: 5 },
            Msg::MoveAbort {
                mv: 5,
                why: "gone".into(),
            },
            Msg::Swap { move_a: 5, move_b: 6 },
            Msg::ChildExited {
                pid: 1,
                start: 2,
                status: Some(0),
            },
            Msg::Away {
                ids: vec!["lua:notes".into()],
            },
        ]
    }

    #[test]
    fn a_nil_answer_and_a_refusal_round_trip_as_themselves() {
        for result in [
            ReplyResult::Ok { value: Value::Null },
            ReplyResult::Ok {
                value: serde_json::json!({"modified": false}),
            },
            ReplyResult::Err {
                why: "no such buffer".into(),
            },
        ] {
            let msg = Msg::EditorReply { id: 7, result };
            assert_eq!(from_json(&to_json(&msg)).unwrap(), msg);
        }
        assert!(matches!(
            from_json(br#"{"v":1,"k":"editor_reply","id":7,"result":{"r":"ok"}}"#),
            Err(LinkError::Malformed(_))
        ));
        assert!(matches!(
            from_json(br#"{"v":1,"k":"editor_reply","id":7,"result":{"r":"ok","value":1,"x":2}}"#),
            Err(LinkError::Malformed(_))
        ));
    }

    #[test]
    fn every_kind_round_trips_and_carries_its_version_and_kind() {
        let all = one_of_each();
        assert_eq!(all.len(), KINDS.len());
        for msg in all {
            let json = to_json(&msg);
            let value: Value = serde_json::from_slice(&json).unwrap();
            assert_eq!(value["v"], 1, "{value}");
            assert_eq!(value["k"], msg.kind(), "{value}");
            assert!(KINDS.contains(&msg.kind()));
            assert_eq!(from_json(&json).unwrap(), msg);
        }
    }

    #[test]
    fn a_moves_number_is_named_move_on_the_wire() {
        let value: Value = serde_json::from_slice(&to_json(&Msg::MoveDone { mv: 7 })).unwrap();
        assert_eq!(value["move"], 7);
        assert!(value.get("mv").is_none());
    }

    #[test]
    fn an_unknown_kind_and_another_version_are_refused_by_name() {
        let err = from_json(br#"{"v":1,"k":"run_lua","code":"x"}"#).unwrap_err();
        assert_eq!(err, LinkError::UnknownKind("run_lua".into()));
        assert!(err.to_string().contains("run_lua"));
        assert_eq!(
            from_json(br#"{"v":2,"k":"bye","why":"x"}"#).unwrap_err(),
            LinkError::Version(2)
        );
        assert!(matches!(from_json(b"[1]"), Err(LinkError::Malformed(_))));
        assert!(matches!(
            from_json(br#"{"v":1,"k":"bye"}"#),
            Err(LinkError::Malformed(_))
        ));
    }

    #[test]
    fn a_body_header_names_a_kind_that_may_have_one_within_its_cap() {
        let Frame::Body { header, json } = frame(&Msg::EditorReply {
            id: 1,
            result: ReplyResult::Ok {
                value: Value::String("x".repeat(100_000)),
            },
        })
        .unwrap() else {
            panic!("a 100 KB reply travels as a body");
        };
        assert_eq!(
            read_header(&header).unwrap(),
            Header::Body {
                kind: "editor_reply".into(),
                len: json.len()
            }
        );
        assert_eq!(read_header(&to_json(&Msg::ReviewIdle)).unwrap(), Header::Inline);
        assert_eq!(
            read_header(br#"{"v":1,"k":"focus","body":{"len":10}}"#).unwrap_err(),
            LinkError::NoBody("focus".into())
        );
        assert!(matches!(
            read_header(br#"{"v":1,"k":"review_events","body":{"len":9000000}}"#),
            Err(LinkError::TooLarge { .. })
        ));
        assert!(matches!(
            read_header(br#"{"v":1,"k":"move","body":{"len":1},"x":1}"#),
            Err(LinkError::Malformed(_))
        ));
        assert!(matches!(
            read_header(br#"{"v":1,"k":"nope","body":{"len":1}}"#),
            Err(LinkError::UnknownKind(_))
        ));
    }

    #[test]
    fn a_long_message_of_a_kind_without_a_body_is_refused() {
        let long = Msg::Focus {
            target: FocusTarget::Module {
                id: "x".repeat(100_000),
            },
            token: None,
        };
        assert!(matches!(frame(&long), Err(LinkError::TooLarge { .. })));
        assert!(matches!(frame(&Msg::ReviewIdle), Ok(Frame::Inline(_))));
    }

    #[test]
    fn descriptors_ride_only_where_their_kind_declares_them() {
        assert!(check_fds("welcome", false, 1).is_ok());
        assert!(check_fds("welcome", true, 2).is_ok());
        assert!(check_fds("welcome", false, 0).is_err());
        assert!(check_fds("move", false, 0).is_ok());
        assert!(check_fds("move", true, 17).is_ok());
        assert!(check_fds("move", false, 17).is_err());
        assert!(check_fds("editor_op", true, 1).is_ok());
        assert!(check_fds("editor_op", true, 0).is_err());
        assert!(check_fds("focus", false, 1).is_err());
        assert!(check_fds("bye", false, 0).is_ok());
    }
}
