//! What travels on the link: one JSON object per packet, with `"v"` (the protocol version) and `"k"` (the kind)
//! in every one. A message longer than [`INLINE_MAX`] travels as a body: its packet carries only `v`, `k` and
//! `body: {len}`, and exactly one sealed memfd holding the JSON. The link carries intents and each window's own
//! summary, never one window's focus, layout or selection mirrored into the other.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::editor_op::EditorOp;
use crate::agent_backend::AgentId;

pub const PROTOCOL_VERSION: u32 = 1;
/// The longest JSON sent in a packet of its own; anything longer goes as a body.
pub const INLINE_MAX: usize = 64 * 1024;
/// The most descriptors one `move` may carry, a body's memfd not counted.
pub const MAX_MOVE_FDS: usize = 16;

const MIB: usize = 1024 * 1024;

/// How a draft edit in the editor ended, as the window that waits for it is told: its own end of the edit takes
/// each as it takes the same end from an editor of its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "how", rename_all = "snake_case")]
pub enum DraftEnd {
    /// Saved: the message's text is the new draft.
    #[default]
    Written,
    /// Quit without saving: the tab keeps the draft it had.
    Discarded,
    /// The edit could not be finished; `why` is shown, and the tab keeps the draft it had.
    Failed { why: String },
}

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
    /// `text` is the draft as the tab holds it: the window with the editor has no copy of the other window's tabs.
    Scratch {
        tab: u64,
        session: Option<String>,
        text: String,
    },
    /// A path click or `gf`; `path` is relative to the project root, or absolute, as the window's own `gf` takes it.
    OpenPath {
        path: String,
        line: Option<u32>,
        col: Option<u32>,
    },
    /// Opening a reviewed file from the review list.
    ReviewOpen { path: String, line: Option<u32> },
    /// The editor key pressed in window 2.
    Focus,
    /// A read-only view (`Ctrl+g` on a chat row, `:%scratch`): `text` in a read-only buffer named after `title`, in
    /// the reusable row-view split, or in a tab page of its own with `tab`. The text travels, as a draft's does, so
    /// no file of the other window's scratch directory is ever named to the editor.
    View {
        title: String,
        text: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        tab: bool,
    },
    /// A pending edit's proposal as a read-only diff: the file as it is (`base`) and as the edit would leave it
    /// (`proposed`); `name` is the real file's name, which the buffers' filetype comes from.
    Diff {
        name: String,
        base: String,
        proposed: String,
    },
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
    /// Tabs with live non-ambient background work, which a close ends as it ends a turn.
    #[serde(default)]
    pub background: u32,
    /// Tabs whose unsent text would not come back after a close (`TabSet::unsaved_draft_count`).
    #[serde(default)]
    pub drafts: u32,
    pub workspaces: Vec<String>,
}

/// Where a moving module sits in the source: tiled, or floating at a rectangle relative to the grid (0 to 1).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "shape", rename_all = "snake_case")]
pub enum Shape {
    Tiled,
    Float { x: f64, y: f64, w: f64, h: f64 },
}

/// What a module window asks window 1 to do, because it needs another window: only window 1 starts and raises them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "a", rename_all = "snake_case")]
pub enum HubAsk {
    /// Show this module: the window that holds it comes forward, or one is started for it.
    Module { id: String },
    /// A new chat window.
    InstanceNew,
    /// Move the tab of `agent`'s session to the next free chat window, or with `back` into `c1`.
    TabBreak {
        agent: AgentId,
        session: String,
        back: bool,
    },
    /// Run this chat key, by its action's name and options, in the chat window that last held the keys: a page window
    /// has no chat of its own for it.
    ChatKey {
        name: String,
        opts: Vec<(String, serde_json::Value)>,
    },
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
    /// A draft edit window 1's nvim ended. `text` is the new draft when `end` is `Written`, and empty otherwise.
    DraftBack {
        tab: u64,
        session: Option<String>,
        text: String,
        #[serde(default)]
        end: DraftEnd,
        /// An activation token from the window that had the keys while the draft was edited, so the window whose
        /// tab it is may come forward to take them back.
        #[serde(default)]
        token: Option<String>,
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
    /// Asks the other window to offer a module it holds: the move a key in this window wants is the other window's
    /// to start, since only the holder can freeze what it holds. Either names the module, or a chat's session.
    MoveAsk {
        module: Option<String>,
        session: Option<String>,
        follow: bool,
    },
    /// A workspace's saved arrangement and the module moves that decide its arrival.
    WorkspaceMove {
        group: u64,
        name: String,
        tree: Value,
        moves: Vec<u64>,
        swap: Option<u64>,
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
    /// `token` is the asking window's activation token, for the window that comes forward.
    HubAsk {
        ask: HubAsk,
        token: Option<String>,
    },
    /// The sending window's toplevel gained the keyboard.
    WindowFocused {},
    /// A module window's revert job asks whether window 1 lets it write.
    RevertGuard {
        job: u64,
    },
    /// The answer to `revert_guard`; `why` says what blocks it.
    Guard {
        job: u64,
        ok: bool,
        why: Option<String>,
    },
    /// The sending window's saved tabs, for window 1 to keep with the group's.
    TabsReport {
        tabs: Value,
    },
    /// Tabs for the receiving window to open: at a link, or with `adopt` one tab handed over from another window.
    TabsGiven {
        tabs: Value,
        adopt: bool,
    },
    /// Window 1's editor listens on `port` for a `claude` in a terminal, or no longer listens: what window 2's next
    /// shells get. A shell that runs keeps what it started with.
    IdePort {
        port: Option<u16>,
    },
    /// Window 1 is closing its group: the chat window says its tabs as they stand now, then [`Msg::TabsSaid`].
    GroupClosing {},
    /// The last [`Msg::TabsReport`] window 1 asked for is out, or there was none to send.
    TabsSaid {},
    /// Window 1 asks the holder of a tab to give it up for window `to`. Session ids are each agent's own.
    TabRelease {
        agent: AgentId,
        session: String,
        to: String,
    },
    /// The tab is retired here, and `tab` is what its next holder opens.
    TabReleased {
        session: String,
        tab: Value,
    },
    /// The tab stays where it is, for the reason `why`: the holder's answer to `tab_release`, or what window 1 tells
    /// the holder when the hand-over it asked for cannot start.
    TabKept {
        session: String,
        why: String,
    },
    /// The tab `session` is open in the window it was handed to: that window tells window 1, and window 1 tells the
    /// window that gave it up, which until then stays open in case the tab has to come back.
    TabAdopted {
        session: String,
    },
    /// A keymap action by its name and the options that rebuild it; no Lua and no free text crosses.
    RunAction {
        name: String,
        opts: Vec<(String, Value)>,
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
    "move_ask",
    "workspace_move",
    "swap",
    "child_exited",
    "away",
    "hub_ask",
    "window_focused",
    "revert_guard",
    "guard",
    "tabs_report",
    "tabs_given",
    "ide_port",
    "group_closing",
    "tabs_said",
    "tab_release",
    "tab_released",
    "tab_kept",
    "tab_adopted",
    "run_action",
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
            Msg::MoveAsk { .. } => "move_ask",
            Msg::WorkspaceMove { .. } => "workspace_move",
            Msg::Swap { .. } => "swap",
            Msg::ChildExited { .. } => "child_exited",
            Msg::Away { .. } => "away",
            Msg::HubAsk { .. } => "hub_ask",
            Msg::WindowFocused {} => "window_focused",
            Msg::RevertGuard { .. } => "revert_guard",
            Msg::Guard { .. } => "guard",
            Msg::TabsReport { .. } => "tabs_report",
            Msg::TabsGiven { .. } => "tabs_given",
            Msg::IdePort { .. } => "ide_port",
            Msg::GroupClosing {} => "group_closing",
            Msg::TabsSaid {} => "tabs_said",
            Msg::TabRelease { .. } => "tab_release",
            Msg::TabReleased { .. } => "tab_released",
            Msg::TabKept { .. } => "tab_kept",
            Msg::TabAdopted { .. } => "tab_adopted",
            Msg::RunAction { .. } => "run_action",
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
        // A chat draft, out to the editor and back: any text a user pastes into a prompt, held to the bound of
        // an editor operation.
        "editor" | "draft_back" => 16 * MIB,
        "welcome" => 3 * MIB,
        // A whole window's saved tabs: a few hundred sessions pass the packet's size.
        "tabs_report" | "tabs_given" => 8 * MIB,
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

    fn the_hubs_kinds() -> Vec<Msg> {
        vec![
            Msg::HubAsk {
                ask: HubAsk::Module { id: "agent#3".into() },
                token: Some("t".into()),
            },
            Msg::WindowFocused {},
            Msg::RevertGuard { job: 4 },
            Msg::Guard {
                job: 4,
                ok: false,
                why: Some("window 1 is not linked".into()),
            },
            Msg::TabsReport {
                tabs: serde_json::json!({"tabs": [{"session": "s"}], "active": 0}),
            },
            Msg::TabsGiven {
                tabs: serde_json::json!([{"session": "s"}]),
                adopt: true,
            },
            Msg::GroupClosing {},
            Msg::TabsSaid {},
            Msg::TabRelease {
                agent: AgentId::Acp("two".into()),
                session: "s".into(),
                to: "c3".into(),
            },
            Msg::TabReleased {
                session: "s".into(),
                tab: serde_json::json!({"session": "s"}),
            },
            Msg::TabKept {
                session: "s".into(),
                why: "a turn is running".into(),
            },
            Msg::TabAdopted { session: "s".into() },
            Msg::RunAction {
                name: "tab_goto".into(),
                opts: vec![
                    ("index".into(), serde_json::json!(3)),
                    ("dir".into(), serde_json::json!("left")),
                ],
            },
        ]
    }

    fn one_of_each() -> Vec<Msg> {
        let edge = TurnEdge {
            session: "s".into(),
            edge: EdgeKind::Started,
            at_ns: 7,
        };
        let mut all = vec![
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
                    background: 5,
                    drafts: 6,
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
                end: DraftEnd::Written,
                token: Some("t".into()),
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
            Msg::MoveAsk {
                module: Some("terminal#2".into()),
                session: None,
                follow: true,
            },
            Msg::WorkspaceMove {
                group: 8,
                name: "study".into(),
                tree: serde_json::json!({"tiled": "agent"}),
                moves: vec![5, 7],
                swap: Some(9),
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
            Msg::IdePort { port: Some(23456) },
        ];
        all.extend(the_hubs_kinds());
        all
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
    fn every_editor_request_round_trips_with_its_token() {
        for request in [
            EditorRequest::Scratch {
                tab: 2,
                session: Some("s".into()),
                text: "draft".into(),
            },
            EditorRequest::OpenPath {
                path: "src/a.rs".into(),
                line: Some(3),
                col: Some(1),
            },
            EditorRequest::ReviewOpen {
                path: "src/a.rs".into(),
                line: None,
            },
            EditorRequest::Focus,
            EditorRequest::View {
                title: "notes".into(),
                text: "a row\nof text".into(),
                tab: false,
            },
            EditorRequest::View {
                title: "Conversation".into(),
                text: "## ▍You · prompt 1/1".into(),
                tab: true,
            },
            EditorRequest::Diff {
                name: "src/a.rs".into(),
                base: "old\n".into(),
                proposed: "new\n".into(),
            },
        ] {
            let msg = Msg::Editor {
                request,
                token: Some("t".into()),
            };
            assert_eq!(from_json(&to_json(&msg)).unwrap(), msg);
        }
    }

    /// A view for the row-view split goes as it always went, with no `tab`, and one without it reads as the split.
    #[test]
    fn a_view_without_tab_is_the_split_and_writes_no_tab() {
        let split = Msg::Editor {
            request: EditorRequest::View {
                title: "notes".into(),
                text: "a row".into(),
                tab: false,
            },
            token: None,
        };
        let json = to_json(&split);
        let text = String::from_utf8(json.clone()).unwrap();
        assert!(text.contains("\"view\""), "{text}");
        assert!(!text.contains("\"tab\""), "{text}");
        assert_eq!(from_json(&json).unwrap(), split);
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
    fn ide_port_round_trips_and_carries_nothing_else() {
        for port in [Some(1), Some(23456), Some(u16::MAX), None] {
            let msg = Msg::IdePort { port };
            assert_eq!(msg.kind(), "ide_port");
            assert!(KINDS.contains(&msg.kind()));
            let json = to_json(&msg);
            let value: Value = serde_json::from_slice(&json).unwrap();
            let mut keys: Vec<&str> = value.as_object().unwrap().keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(keys, ["k", "port", "v"], "{value}");
            assert_eq!(from_json(&json).unwrap(), msg);
        }
        assert!(check_fds("ide_port", false, 0).is_ok());
        assert!(check_fds("ide_port", false, 1).is_err());
        assert_eq!(body_cap("ide_port"), 0, "it fits one packet");
        assert!(
            from_json(br#"{"v":1,"k":"ide_port","port":65536}"#).is_err(),
            "no such port"
        );
    }

    #[test]
    fn every_new_kind_round_trips_and_carries_no_descriptor() {
        let hub = the_hubs_kinds();
        assert_eq!(hub.len(), 13);
        for msg in hub {
            let kind = msg.kind();
            assert!(KINDS.contains(&kind), "{kind} is listed");
            assert_eq!(from_json(&to_json(&msg)).unwrap(), msg);
            assert!(check_fds(kind, false, 0).is_ok(), "{kind} needs no descriptor");
            assert!(check_fds(kind, false, 1).is_err(), "{kind} carries no descriptor");
            let may_be_long = matches!(kind, "tabs_report" | "tabs_given");
            assert_eq!(
                body_cap(kind) > 0,
                may_be_long,
                "{kind}: only a set of tabs passes one packet"
            );
        }
        for ask in [
            HubAsk::Module {
                id: "lua:markdown".into(),
            },
            HubAsk::InstanceNew,
            HubAsk::TabBreak {
                agent: AgentId::Codex,
                session: "s".into(),
                back: true,
            },
            HubAsk::ChatKey {
                name: "tab.select".into(),
                opts: vec![("n".into(), serde_json::json!(3))],
            },
        ] {
            let msg = Msg::HubAsk { ask, token: None };
            assert_eq!(from_json(&to_json(&msg)).unwrap(), msg);
        }
        let msg = Msg::Guard {
            job: 1,
            ok: true,
            why: None,
        };
        assert_eq!(from_json(&to_json(&msg)).unwrap(), msg);
        assert_eq!(
            from_json(
                br#"{"v":1,"k":"hub_ask","ask":{"a":"tab_break","agent":"claude","session":"s","back":false},"token":null}"#
            )
            .unwrap(),
            Msg::HubAsk {
                ask: HubAsk::TabBreak {
                    agent: AgentId::Claude,
                    session: "s".into(),
                    back: false
                },
                token: None
            },
            "an ask is tagged by `a` on the wire"
        );
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
    fn a_draft_longer_than_a_packet_goes_out_and_back_as_a_body() {
        let long = "pasted log line\n".repeat(10_000);
        let out = Msg::Editor {
            request: EditorRequest::Scratch {
                tab: 1,
                session: Some("s".into()),
                text: long.clone(),
            },
            token: None,
        };
        let back = Msg::DraftBack {
            tab: 1,
            session: Some("s".into()),
            text: long,
            end: DraftEnd::Written,
            token: None,
        };
        for msg in [out, back] {
            let kind = msg.kind();
            let Frame::Body { header, json } = frame(&msg).unwrap() else {
                panic!("a {kind} of 160 KB travels as a body");
            };
            assert_eq!(
                read_header(&header).unwrap(),
                Header::Body {
                    kind: kind.into(),
                    len: json.len()
                }
            );
            assert_eq!(from_json(&json).unwrap(), msg);
        }
    }

    #[test]
    fn each_end_of_a_draft_edit_round_trips() {
        for end in [
            DraftEnd::Written,
            DraftEnd::Discarded,
            DraftEnd::Failed {
                why: "nvim went".into(),
            },
        ] {
            let msg = Msg::DraftBack {
                tab: 3,
                session: None,
                text: String::new(),
                end,
                token: None,
            };
            assert_eq!(from_json(&to_json(&msg)).unwrap(), msg);
        }
    }

    #[test]
    fn a_draft_back_from_an_older_window_reads_as_written() {
        let msg = from_json(br#"{"v":1,"k":"draft_back","tab":2,"session":null,"text":"x"}"#).unwrap();
        assert_eq!(
            msg,
            Msg::DraftBack {
                tab: 2,
                session: None,
                text: "x".into(),
                end: DraftEnd::Written,
                token: None,
            }
        );
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
