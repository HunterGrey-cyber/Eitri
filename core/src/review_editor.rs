//! Turn review in the user's own nvim: the Lua module that draws a turn's hunks over a buffer
//! ([`REVIEW_LUA`]), the constant calls into it, and the arguments those calls take.
//!
//! Every value -- a path, a line of a file, a session id -- goes to nvim as a msgpack argument of
//! a fixed chunk, never as Lua or Ex source. Line text goes as binary (a buffer line may be any
//! bytes) with its terminator named separately, so nothing between the file and the buffer
//! guesses an ending: [`ShowHunk`] is built from file lines by [`crate::editor_lines`] and turned
//! back into file bytes by the same module.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use rmpv::Value;
use serde::{Deserialize, Serialize};

use crate::editor_lines::{to_buffer_lines, to_file_bytes, BufferLine, Eol};
use crate::editor_rpc::EditorRpc;
use crate::nvim_rpc::{Pending, RpcError};
use crate::turn_review::flow::LiveAsk;
use crate::turn_review::live_contest::{LiveContest, TurnKey};
use crate::turn_review::{Scope, MAX_OVERVIEW_DIFF_LINES};
use crate::window_link::editor_op::WireLiveMeta;
use crate::window_link::{Drawn, EditorOp, WireHunk, WireMeta};
use crate::wm::PeerId;

/// The module itself; args `(owner, version)`. The same owner and version again change nothing
/// (`installed = false`); any other replaces the module there, tearing the old one down first.
/// Changing `find_buf.lua` or `review.lua` means bumping [`REVIEW_LUA_VERSION`], or an nvim that
/// holds the old one keeps it.
pub const REVIEW_LUA: &str =
    crate::editor_rpc::takeover_chunk!(concat!(include_str!("find_buf.lua"), include_str!("review.lua")), "");

/// Bumped whenever `find_buf.lua` or `review.lua` changes.
pub const REVIEW_LUA_VERSION: i64 = 16;

/// Every call into an installed module first checks that one is there, and answers
/// `{ missing = true }` when it is not (another panel's teardown, or nvim restarted) rather than
/// failing in Lua. `concat!` takes literals only, hence a macro.
macro_rules! guarded {
    ($body:literal) => {
        concat!(
            "local r = rawget(_G, '__eitri_review') if not r then return { missing = true } end ",
            $body
        )
    };
}

/// A standalone query of buffers shown in non-floating windows; args `([abs_path: bin])`.
pub const LIVE_SHOWN_LUA: &str = concat!(include_str!("find_buf.lua"), include_str!("live_shown.lua"));
/// Draws into an already shown, unmodified buffer; args `(abs_path: bin, meta: map, hunks: [map])`.
pub const LIVE_SHOW_LUA: &str = guarded!("return r.live_show(...)");
/// Removes only a live overlay; args `(abs_path: bin)`.
pub const LIVE_CLEAR_LUA: &str = guarded!("return r.live_clear(...)");
/// The largest batch of paths queried in one editor call.
pub const MAX_SHOWN_PATHS: usize = 256;

/// Args `(abs_path: bin, meta: map, hunks: [map])`: draws into the buffer showing the file, if one
/// is loaded. Answers `{drawn, kept, skipped, notice?, active, pending, live_only}`; `kept` also counts hunks reverted
/// in the buffer that the editor still follows. A hunk drawn over text that is its new side and
/// not its old one is also queued as an `unrevert` event, behind any earlier event, since it may
/// have been reverted while no overlay followed it.
pub const SHOW_LUA: &str = guarded!("return r.show(...)");
/// Args `(abs_path: bin, line: int|nil, meta: map, hunks: [map]|nil)`: opens the file (a split
/// when the current buffer cannot be left), puts the cursor on `line` and shows `hunks`. Answers
/// `{opened, error?, drawn?, skipped?, notice?, active, pending, live_only}`.
pub const OPEN_AND_SHOW_LUA: &str = guarded!("return r.open_and_show(...)");
/// Args `(abs_path: bin)`: takes the file's overlay off, with no event. Answers `{active, pending, live_only}`.
pub const CLEAR_LUA: &str = guarded!("return r.clear(...)");
/// No args: takes every overlay off, with no event. Answers `{active, pending, live_only}`.
pub const CLEAR_ALL_LUA: &str = guarded!("return r.clear_all()");
/// No args: the oldest events since the last call, in order, as many as fit in one answer (at
/// least one), and how many buffers have an overlay. Answers `{events, active, more, pending, live_only}`; `more`
/// says events are still waiting. An event is a `revert` or an `unrevert` (a hunk's lines, as
/// [`parse_revert_event`] and [`parse_unrevert_event`] read them) or an `off`.
pub const TAKE_EVENTS_LUA: &str = guarded!("return r.take_events()");

/// Who installed the module, which decides who tears it down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// A companion panel on that RPC channel: the module joins the panel's glue and goes with it.
    Companion { channel: u64 },
    /// The integrated window's own nvim, which goes with the window.
    Embedded,
}

impl Owner {
    fn arg(self) -> String {
        match self {
            Owner::Companion { channel } => format!("companion:{channel}"),
            Owner::Embedded => "embedded".to_owned(),
        }
    }
}

/// The arguments of [`REVIEW_LUA`].
pub fn install_args(owner: Owner) -> Vec<Value> {
    vec![Value::from(owner.arg()), Value::from(REVIEW_LUA_VERSION)]
}

/// Which review a drawn hunk belongs to; a revert event carries it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowMeta {
    pub tab: u64,
    pub session: String,
    pub turn: u32,
    pub scope: Scope,
}

/// The turn and disk line count a live drawing was computed from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveMeta {
    pub tab: u64,
    pub session: String,
    pub turn_id: String,
    pub turn: u32,
    pub running: bool,
    pub read_lines: u64,
}

/// A live drawing's result, including whether an existing overlay was left alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveShowAnswer {
    pub drawn: u32,
    pub shown: bool,
    pub left: bool,
    pub skipped: u32,
    pub active: u64,
    pub pending: u64,
    pub live_only: bool,
}

/// One hunk as nvim gets it: the counts of its `@@` header and both sides as buffer lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowHunk {
    pub id: u32,
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    pub old_lines: Vec<BufferLine>,
    pub new_lines: Vec<BufferLine>,
}

impl ShowHunk {
    /// From each side's file lines, terminators kept (`split_inclusive` on `\n`).
    pub fn from_file_lines(
        id: u32,
        old: (u32, u32),
        new: (u32, u32),
        old_lines: &[Vec<u8>],
        new_lines: &[Vec<u8>],
    ) -> ShowHunk {
        debug_assert_eq!(old_lines.len(), old.1 as usize, "the old side's lines and its count");
        debug_assert_eq!(new_lines.len(), new.1 as usize, "the new side's lines and its count");
        ShowHunk {
            id,
            old_start: old.0,
            old_len: old.1,
            new_start: new.0,
            new_len: new.1,
            old_lines: to_buffer_lines(old_lines),
            new_lines: to_buffer_lines(new_lines),
        }
    }

    /// The old side's file bytes.
    pub fn old_bytes(&self) -> Vec<u8> {
        to_file_bytes(&self.old_lines)
    }

    /// The new side's file bytes.
    pub fn new_bytes(&self) -> Vec<u8> {
        to_file_bytes(&self.new_lines)
    }
}

/// A hunk the user reverted in a buffer, as the module reported it: the buffer changed, the file
/// on disk did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorRevertEvent {
    pub meta: ShowMeta,
    pub path: String,
    /// The hunk as it was shown, its header included.
    pub hunk: ShowHunk,
    /// The line (from 1) where the reverted lines now start in the buffer: the hunk's
    /// `new_start`, unless an edit above it moved it before the revert.
    pub at_line: u32,
}

fn eol_name(eol: Eol) -> &'static str {
    match eol {
        Eol::Lf => "lf",
        Eol::CrLf => "crlf",
        Eol::Missing => "missing",
    }
}

fn eol_from(name: &str) -> Option<Eol> {
    match name {
        "lf" => Some(Eol::Lf),
        "crlf" => Some(Eol::CrLf),
        "missing" => Some(Eol::Missing),
        _ => None,
    }
}

fn scope_from(name: &str) -> Option<Scope> {
    match name {
        "turn" => Some(Scope::Turn),
        "session" => Some(Scope::Session),
        _ => None,
    }
}

/// A path as its OS bytes: nvim makes a Lua string of them, which names the same file whatever
/// its encoding.
fn path_arg(path: &Path) -> Value {
    Value::Binary(path.as_os_str().as_bytes().to_vec())
}

fn map(pairs: Vec<(&str, Value)>) -> Value {
    Value::Map(pairs.into_iter().map(|(k, v)| (Value::from(k), v)).collect())
}

fn meta_value(meta: &ShowMeta) -> Value {
    map(vec![
        ("tab", Value::from(meta.tab)),
        ("session", Value::from(meta.session.as_str())),
        ("turn", Value::from(meta.turn)),
        ("scope", Value::from(meta.scope.as_str())),
    ])
}

fn hunk_value(hunk: &ShowHunk) -> Value {
    let texts = |lines: &[BufferLine]| Value::Array(lines.iter().map(|l| Value::Binary(l.text.clone())).collect());
    let eols = |lines: &[BufferLine]| Value::Array(lines.iter().map(|l| Value::from(eol_name(l.eol))).collect());
    map(vec![
        ("id", Value::from(hunk.id)),
        ("old_start", Value::from(hunk.old_start)),
        ("old_len", Value::from(hunk.old_len)),
        ("new_start", Value::from(hunk.new_start)),
        ("new_len", Value::from(hunk.new_len)),
        ("old_lines", texts(&hunk.old_lines)),
        ("new_lines", texts(&hunk.new_lines)),
        ("old_eols", eols(&hunk.old_lines)),
        ("new_eols", eols(&hunk.new_lines)),
    ])
}

/// The arguments of [`SHOW_LUA`].
pub fn show_args(abs_path: &Path, meta: &ShowMeta, hunks: &[ShowHunk]) -> Vec<Value> {
    vec![
        path_arg(abs_path),
        meta_value(meta),
        Value::Array(hunks.iter().map(hunk_value).collect()),
    ]
}

/// The arguments of [`LIVE_SHOWN_LUA`], bounded to one batch.
pub fn live_shown_args(abs_paths: &[PathBuf]) -> Vec<Value> {
    vec![Value::Array(
        abs_paths.iter().take(MAX_SHOWN_PATHS).map(|p| path_arg(p)).collect(),
    )]
}

/// The arguments of [`LIVE_SHOW_LUA`]. Live drawings always belong to one turn.
pub fn live_show_args(abs_path: &Path, meta: &LiveMeta, hunks: &[ShowHunk]) -> Vec<Value> {
    vec![
        path_arg(abs_path),
        map(vec![
            ("tab", Value::from(meta.tab)),
            ("session", Value::from(meta.session.as_str())),
            ("turn_id", Value::from(meta.turn_id.as_str())),
            ("turn", Value::from(meta.turn)),
            ("scope", Value::from("turn")),
            ("running", Value::from(meta.running)),
            ("read_lines", Value::from(meta.read_lines)),
        ]),
        Value::Array(hunks.iter().map(hunk_value).collect()),
    ]
}

/// Reads exactly one boolean for every path sent in the bounded query.
pub fn parse_live_shown(answer: &Value, asked: usize) -> Result<Vec<bool>, String> {
    let shown = list(answer, "shown")?;
    if shown.len() != asked.min(MAX_SHOWN_PATHS) {
        return Err("the shown list differs in length from the query".to_owned());
    }
    shown
        .iter()
        .map(|v| v.as_bool().ok_or_else(|| "a shown entry is not a boolean".to_owned()))
        .collect()
}

/// Reads every field of a live drawing's answer without guessing missing values.
pub fn parse_live_show(answer: &Value) -> Result<LiveShowAnswer, String> {
    let boolean = |name| {
        field(answer, name)?
            .as_bool()
            .ok_or_else(|| format!("`{name}` is not a boolean"))
    };
    Ok(LiveShowAnswer {
        drawn: integer(answer, "drawn")?,
        shown: boolean("shown")?,
        left: boolean("left")?,
        skipped: integer(answer, "skipped")?,
        active: integer(answer, "active")?,
        pending: integer(answer, "pending")?,
        live_only: boolean("live_only")?,
    })
}

/// The arguments of [`OPEN_AND_SHOW_LUA`]; `hunks: None` only opens the file.
pub fn open_and_show_args(
    abs_path: &Path,
    line: Option<u32>,
    meta: &ShowMeta,
    hunks: Option<&[ShowHunk]>,
) -> Vec<Value> {
    vec![
        path_arg(abs_path),
        line.map_or(Value::Nil, Value::from),
        meta_value(meta),
        hunks.map_or(Value::Nil, |hunks| Value::Array(hunks.iter().map(hunk_value).collect())),
    ]
}

/// The arguments of [`CLEAR_LUA`].
pub fn clear_args(abs_path: &Path) -> Vec<Value> {
    vec![path_arg(abs_path)]
}

fn field<'a>(event: &'a Value, name: &str) -> Result<&'a Value, String> {
    event
        .as_map()
        .ok_or_else(|| "the event is not a map".to_owned())?
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
        .ok_or_else(|| format!("the event has no `{name}`"))
}

fn integer<T: TryFrom<u64>>(event: &Value, name: &str) -> Result<T, String> {
    field(event, name)?
        .as_u64()
        .and_then(|n| T::try_from(n).ok())
        .ok_or_else(|| format!("`{name}` is not an integer that fits"))
}

/// The bytes of a Lua string nvim sent: it sends every Lua string as msgpack str, valid UTF-8 or
/// not, so the bytes are taken as they are and never through a UTF-8 view.
fn bytes(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::String(s) => Some(s.as_bytes().to_vec()),
        Value::Binary(b) => Some(b.clone()),
        _ => None,
    }
}

fn text(event: &Value, name: &str) -> Result<String, String> {
    let raw = bytes(field(event, name)?).ok_or_else(|| format!("`{name}` is not a string"))?;
    String::from_utf8(raw).map_err(|_| format!("`{name}` is not UTF-8"))
}

/// An array, or the empty map an empty Lua table can arrive as.
fn list<'a>(event: &'a Value, name: &str) -> Result<&'a [Value], String> {
    match field(event, name)? {
        Value::Array(items) => Ok(items),
        Value::Map(entries) if entries.is_empty() => Ok(&[]),
        _ => Err(format!("`{name}` is not a list")),
    }
}

fn side(event: &Value, side: &str, len: u32) -> Result<Vec<BufferLine>, String> {
    let lines = list(event, &format!("{side}_lines"))?;
    let eols = list(event, &format!("{side}_eols"))?;
    if lines.len() != eols.len() {
        return Err(format!("`{side}_lines` and `{side}_eols` differ in length"));
    }
    if lines.len() != len as usize {
        return Err(format!("`{side}_lines` does not hold `{side}_len` lines"));
    }
    lines
        .iter()
        .zip(eols)
        .map(|(line, eol)| {
            let text = bytes(line).ok_or_else(|| format!("a line of `{side}_lines` is not a string"))?;
            let eol = eol
                .as_str()
                .and_then(eol_from)
                .ok_or_else(|| format!("an unknown line ending in `{side}_eols`: {eol}"))?;
            Ok(BufferLine { text, eol })
        })
        .collect()
}

/// A `revert` event from [`TAKE_EVENTS_LUA`]. Anything it cannot read exactly fails the event:
/// an ending it does not know or a side whose arrays disagree would give bytes the file never had.
pub fn parse_revert_event(event: &Value) -> Result<EditorRevertEvent, String> {
    parse_hunk_event(event, "revert")
}

/// An `unrevert` event: a hunk reverted in the buffer has its text back there (an undo, or the
/// user typing it again). It carries the hunk exactly as the `revert` event did.
pub fn parse_unrevert_event(event: &Value) -> Result<EditorRevertEvent, String> {
    parse_hunk_event(event, "unrevert")
}

fn parse_hunk_event(event: &Value, kind: &str) -> Result<EditorRevertEvent, String> {
    if field(event, "kind")?.as_str() != Some(kind) {
        return Err(format!("not a {kind} event"));
    }
    let scope = field(event, "scope")?
        .as_str()
        .and_then(scope_from)
        .ok_or_else(|| "`scope` is not turn or session".to_owned())?;
    let meta = ShowMeta {
        tab: integer(event, "tab")?,
        session: text(event, "session")?,
        turn: integer(event, "turn")?,
        scope,
    };
    let at_line: u32 = integer(event, "at_line")?;
    if at_line == 0 {
        return Err("`at_line` is not a line".to_owned());
    }
    let old_len: u32 = integer(event, "old_len")?;
    let new_len: u32 = integer(event, "new_len")?;
    let hunk = ShowHunk {
        id: integer(event, "hunk_id")?,
        old_start: integer(event, "old_start")?,
        old_len,
        new_start: integer(event, "new_start")?,
        new_len,
        old_lines: side(event, "old", old_len)?,
        new_lines: side(event, "new", new_len)?,
    };
    Ok(EditorRevertEvent {
        meta,
        path: text(event, "path")?,
        hunk,
        at_line,
    })
}

/// What the editor may draw of a file's change: nothing for a binary file, nor for one whose
/// patch the panel already refused (`None`) or whose hunks are longer than the panel would show.
/// The sum of each hunk's longer side is a lower bound on the patch's length, so this never
/// refuses what the panel showed.
pub fn overlay_hunks(binary: bool, hunks: Option<Vec<ShowHunk>>) -> Option<Vec<ShowHunk>> {
    let hunks = hunks?;
    if binary {
        return None;
    }
    let lines: usize = hunks.iter().map(|h| h.old_len.max(h.new_len) as usize).sum();
    (lines <= MAX_OVERVIEW_DIFF_LINES).then_some(hunks)
}

/// A buffer that stopped showing a review, as the module reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffEvent {
    pub path: String,
    /// `user`, `reload`, `closed`, `edited` (a live drawing whose hunks were all edited away) or
    /// `replaced` (the module's own, or another window drawing over the file), or `reported` when
    /// the editor's window passed it on without saying why.
    pub why: String,
}

/// One call into the module.
#[derive(Debug, Clone, PartialEq)]
pub enum OverlayCall {
    /// Opens the file (answering `request_id` when it did or could not) and shows `hunks` over it.
    OpenAndShow {
        request_id: String,
        path: PathBuf,
        line: Option<u32>,
        meta: ShowMeta,
        hunks: Option<Vec<ShowHunk>>,
    },
    /// Draws over the file if the editor already has it loaded.
    Show {
        path: PathBuf,
        meta: ShowMeta,
        hunks: Vec<ShowHunk>,
    },
    Clear {
        path: PathBuf,
    },
    ClearAll,
    /// Draws a turn's hunks over the file as the agent's edits land, if the editor shows the file and
    /// nothing there may not be replaced.
    LiveShow {
        path: PathBuf,
        meta: LiveMeta,
        hunks: Vec<ShowHunk>,
    },
    /// Takes the file's drawing off only if it is a live one.
    LiveClear {
        path: PathBuf,
    },
}

/// What the driver hands back from [`EditorOverlay::tick`].
#[derive(Debug, Clone, PartialEq)]
pub enum OverlayOutcome {
    /// The answer to an [`OverlayCall::OpenAndShow`]: what happened, or why nothing did.
    Answered {
        request_id: String,
        result: Result<String, String>,
    },
    /// Hunks the user reverted in a buffer since the last batch, in the order they did it.
    Reverts(Vec<EditorRevertEvent>),
    /// Hunks reverted in a buffer whose text is back there: the revert no longer stands.
    Unreverts(Vec<EditorRevertEvent>),
    /// Buffers that stopped showing a review.
    Off(Vec<OffEvent>),
    /// What a [`OverlayCall::LiveShow`] of this window's came to: `drawn` hunks are up, or none and
    /// `left` says a drawing the live one may not replace, or a modified buffer, kept the file.
    /// Not every live draw ends in one: a draw of a contested file is skipped or sent as a live clear,
    /// one over a path that is not UTF-8 is refused, and one the editor twice answered without the
    /// module fails, all without this outcome.
    LiveDrawn {
        path: PathBuf,
        meta: LiveMeta,
        drawn: u32,
        left: bool,
    },
}

/// How often the module's events are asked for while any buffer shows a review.
const DRAIN_EVERY: Duration = Duration::from_millis(500);

/// How often they are asked for while only live drawings are up and the module held no event at its
/// last answer: such drawings stay until dismissed, and nothing about them is urgent.
pub const LIVE_DRAIN_EVERY: Duration = Duration::from_secs(5);

/// Which window asked for a call, drew a file or holds a session: the window that runs the driver, or the
/// window linked to it as that peer. A window with no partner is window 1 throughout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Window {
    One,
    Linked(PeerId),
}

/// What window 1's driver owes a linked window, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkedEvent {
    /// The answer to one of that window's opens, under the id of the operation that asked for it.
    Answered { op_id: u64, result: Result<String, String> },
    /// What became of a file drawn for that window. `op_id` is the window's open or live call this is the
    /// outcome of; `None` for its other calls, and when another call or the editor took the file away.
    Drawn {
        op_id: Option<u64>,
        path: PathBuf,
        state: Drawn,
    },
    /// Hunks reverted in the editor, and reverts undone there, for a session that window holds, in the
    /// order they happened; each as [`editor_event_to_wire`] writes it.
    Events {
        session: String,
        events: Vec<serde_json::Value>,
    },
    /// None of that window's files is drawn any more: the module went away, or the editor reports
    /// nothing drawn at all.
    Idle,
}

/// What one tick of the driver found, for each window.
#[derive(Debug, Default, PartialEq)]
pub struct Routed {
    /// This window's own outcomes.
    pub mine: Vec<OverlayOutcome>,
    /// What goes to each linked window over its link.
    pub linked: BTreeMap<PeerId, Vec<LinkedEvent>>,
}

/// Window 1 names a linked window's opens `w<peer>:<operation id>`: the answer goes back under the id
/// that window asked with, and can never be taken for one of window 1's own or another window's.
pub fn linked_request_id(peer: PeerId, op_id: u64) -> String {
    format!("w{}:{op_id}", peer.0)
}

fn linked_op_id(peer: PeerId, request_id: &str) -> Option<u64> {
    request_id.strip_prefix(&format!("w{}:", peer.0))?.parse().ok()
}

/// Whether an editor event says a hunk was reverted in a buffer, or that its text came back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Revert,
    Unrevert,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireEditorEvent {
    kind: EventKind,
    meta: WireMeta,
    path: String,
    hunk: WireHunk,
    at_line: u32,
}

/// An editor event as it crosses the link: every field the module echoed, the hunk's bytes and line
/// endings exactly as they were.
pub fn editor_event_to_wire(kind: EventKind, event: &EditorRevertEvent) -> serde_json::Value {
    let wire = WireEditorEvent {
        kind,
        meta: WireMeta::from(&event.meta),
        path: event.path.clone(),
        hunk: WireHunk::from(&event.hunk),
        at_line: event.at_line,
    };
    // Strings, numbers and byte lists only: serialising cannot fail, and a `null` would be refused
    // on the other side rather than read as anything.
    serde_json::to_value(wire).unwrap_or(serde_json::Value::Null)
}

/// An editor event from the link, held to the same counts the module's own events are.
pub fn editor_event_from_wire(value: &serde_json::Value) -> Result<(EventKind, EditorRevertEvent), String> {
    let wire: WireEditorEvent =
        serde_json::from_value(value.clone()).map_err(|e| format!("not an editor event: {e}"))?;
    if wire.at_line == 0 {
        return Err("`at_line` is not a line".to_owned());
    }
    if wire.hunk.old_lines.len() != wire.hunk.old_len as usize
        || wire.hunk.new_lines.len() != wire.hunk.new_len as usize
    {
        return Err("a side of the hunk does not hold its count of lines".to_owned());
    }
    Ok((
        wire.kind,
        EditorRevertEvent {
            meta: wire.meta.into(),
            path: wire.path,
            hunk: wire.hunk.into(),
            at_line: wire.at_line,
        },
    ))
}

/// Adds an event to the outcomes, joining the last batch when it is of the same kind, so order is
/// kept across kinds.
fn push_event(done: &mut Vec<OverlayOutcome>, kind: EventKind, event: EditorRevertEvent) {
    if let Some(last) = done.last_mut() {
        match (kind, last) {
            (EventKind::Revert, OverlayOutcome::Reverts(events))
            | (EventKind::Unrevert, OverlayOutcome::Unreverts(events)) => {
                events.push(event);
                return;
            }
            _ => {}
        }
    }
    done.push(match kind {
        EventKind::Revert => OverlayOutcome::Reverts(vec![event]),
        EventKind::Unrevert => OverlayOutcome::Unreverts(vec![event]),
    });
}

fn push_off(done: &mut Vec<OverlayOutcome>, off: OffEvent) {
    if let Some(OverlayOutcome::Off(offs)) = done.last_mut() {
        offs.push(off);
        return;
    }
    done.push(OverlayOutcome::Off(vec![off]));
}

/// A call waiting to be sent, or sent and not yet answered.
struct Queued {
    /// Order of arrival: a call put back (its module was gone) goes back to its own place.
    seq: u64,
    call: OverlayCall,
    /// The module was missing once already; a second time fails the call.
    retried: bool,
    /// The window that asked: what the call comes to goes back to it.
    from: Window,
    /// The linked window's operation that asked for the call, when one did.
    op_id: Option<u64>,
}

struct InFlight {
    queued: Queued,
    pending: Pending,
    /// The install the call was sent after; a `missing` answer from an earlier one is stale.
    epoch: u64,
}

enum Install {
    /// Nothing is known to be in the editor.
    None,
    /// The install is sent and unanswered; the calls behind it wait.
    Installing {
        pending: Pending,
        target: Option<u64>,
    },
    Installed {
        target: Option<u64>,
    },
}

/// The driver of the review module in one editor: it installs the module before its first call,
/// sends calls in order through one [`EditorRpc`], re-installs once when a call finds the module
/// gone, and asks for the module's events while anything is drawn. It never waits: every answer
/// is a [`Pending`] polled from [`EditorOverlay::tick`].
///
/// One call is in flight at a time. A transport may put each request on a thread of its own (the
/// embedded editor does), so two calls sent together can reach nvim in either order, and an older
/// redraw executed last would leave the editor showing a turn the panel has left.
///
/// **Several windows, one driver.** nvim has one review module and one event queue, which a drain
/// empties, so a linked window never talks to it: its calls are queued here tagged
/// [`Window::Linked`] ([`EditorOverlay::call_for`]) behind the same install, the same one call in flight and the same
/// drain. Each outcome goes to exactly one window ([`Routed`]): an open's answer to the window that
/// asked, a revert or its undo to the window holding the event's session, a buffer going off to the
/// window that drew the file.
///
/// **Live drawings.** A [`OverlayCall::LiveShow`] draws an agent's edits as they land; nvim keeps it
/// off a file whose overlay is not live or follows reverted hunks, and off a modified buffer. The
/// driver also keeps, for every window, which paths two turns running at once both edited
/// ([`EditorOverlay::note_live`]): such a path is contested, its live drawing is cleared and no
/// other goes out, since each turn's diff would show the other agent's change as its own.
pub struct EditorOverlay {
    owner: Owner,
    install: Install,
    /// +1 each time an install answered.
    epoch: u64,
    next_seq: u64,
    queue: VecDeque<Queued>,
    in_flight: Vec<InFlight>,
    /// The unanswered `TAKE_EVENTS_LUA` and the install it was sent after.
    drain: Option<(Pending, u64)>,
    last_drain: Option<Instant>,
    /// How many buffers show a review, by the editor's last answer: every window's together.
    active: u64,
    /// The module's last `take_events` answer left events waiting.
    more_events: bool,
    /// A call's answer said nothing is drawn any more while events may still be waiting in the
    /// module (a revert made just before a clear): one more `take_events` is owed, sent after that
    /// answer so it reaches nvim after the call.
    drain_owed: bool,
    /// The files this window drew. A file is in this set or in one set of `linked_paths`, never two:
    /// the module keeps one overlay per buffer, so the last window to draw a file is the one showing it.
    active_paths: BTreeSet<PathBuf>,
    /// The files each linked window drew.
    linked_paths: BTreeMap<PeerId, BTreeSet<PathBuf>>,
    /// The window holding each session that drew here; a session never named is this window's.
    holders: BTreeMap<String, Window>,
    /// Sessions whose chat is moving between two windows: their events wait here, in order, until
    /// the move is decided.
    held: BTreeMap<String, Vec<(EventKind, EditorRevertEvent)>>,
    /// Linked windows that went away: nothing more goes to them, and what is drawn for them is taken
    /// off.
    gone: BTreeSet<PeerId>,
    /// Outcomes decided between two ticks.
    done: Vec<OverlayOutcome>,
    linked: BTreeMap<PeerId, Vec<LinkedEvent>>,
    /// The paths each turn edited while it ran, from every window's notes.
    contest: LiveContest,
    /// The drawn files whose drawing is a live one, with the session and turn it shows.
    live_drawings: BTreeMap<PathBuf, (String, u32)>,
    /// The window that drew each of `live_drawings`.
    live_windows: BTreeMap<PathBuf, Window>,
    /// The module's last answer said every overlay up is a live one.
    live_only: bool,
    /// How many events the module held at its last answer.
    pending: u64,
    /// For each file an answer drew or cleared, how many `off` events the module had queued by then:
    /// one numbered up to that is about a drawing that answer already replaced or took off.
    off_seq_at: BTreeMap<PathBuf, u64>,
    /// The `off` count in the call answer being handled.
    answer_off_seq: Option<u64>,
}

/// What becomes of a queued call when its turn to go out comes.
enum Due {
    Send,
    /// A live draw of a contested file whose live drawing is this window's: that drawing goes instead.
    ClearInstead,
    /// Nothing to send: a live draw of a contested file, or a live clear of a file this window has no
    /// live drawing over.
    Skip,
}

/// What a window that drew nothing has drawn.
static NOTHING_DRAWN: BTreeSet<PathBuf> = BTreeSet::new();

fn lookup<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    value
        .as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
}

fn is_missing(answer: &Value) -> bool {
    lookup(answer, "missing").and_then(Value::as_bool) == Some(true)
}

/// Sends `call` as the one constant chunk it stands for.
fn send(rpc: &dyn EditorRpc, call: &OverlayCall) -> Pending {
    match call {
        OverlayCall::OpenAndShow {
            path,
            line,
            meta,
            hunks,
            ..
        } => rpc.exec_lua(
            OPEN_AND_SHOW_LUA,
            open_and_show_args(path, *line, meta, hunks.as_deref()),
        ),
        OverlayCall::Show { path, meta, hunks } => rpc.exec_lua(SHOW_LUA, show_args(path, meta, hunks)),
        OverlayCall::Clear { path } => rpc.exec_lua(CLEAR_LUA, clear_args(path)),
        OverlayCall::ClearAll => rpc.exec_lua(CLEAR_ALL_LUA, Vec::new()),
        OverlayCall::LiveShow { path, meta, hunks } => rpc.exec_lua(LIVE_SHOW_LUA, live_show_args(path, meta, hunks)),
        OverlayCall::LiveClear { path } => rpc.exec_lua(LIVE_CLEAR_LUA, clear_args(path)),
    }
}

fn is_gone(error: &RpcError) -> bool {
    matches!(error, RpcError::Closed | RpcError::Unavailable(_))
}

fn off_for(path: &Path, why: &str) -> OffEvent {
    OffEvent {
        path: path.to_string_lossy().into_owned(),
        why: why.to_owned(),
    }
}

impl EditorOverlay {
    pub fn new(owner: Owner) -> EditorOverlay {
        EditorOverlay {
            owner,
            install: Install::None,
            epoch: 0,
            next_seq: 0,
            queue: VecDeque::new(),
            in_flight: Vec::new(),
            drain: None,
            last_drain: None,
            active: 0,
            more_events: false,
            drain_owed: false,
            active_paths: BTreeSet::new(),
            linked_paths: BTreeMap::new(),
            holders: BTreeMap::new(),
            held: BTreeMap::new(),
            gone: BTreeSet::new(),
            done: Vec::new(),
            linked: BTreeMap::new(),
            contest: LiveContest::new(),
            live_drawings: BTreeMap::new(),
            live_windows: BTreeMap::new(),
            live_only: false,
            pending: 0,
            off_seq_at: BTreeMap::new(),
            answer_off_seq: None,
        }
    }

    /// Notes that the turn `key` edited `paths` while it ran from `started_ns`, and until `ended_ns`
    /// once it ended. Every window's notes go into one table, whichever window `from` sent them: a
    /// file two turns running at once both edited is contested, and its live drawing, drawn or on its
    /// way, is cleared by the next tick. The table is decided from edits, so the editor going away
    /// leaves it as it was.
    pub fn note_live(
        &mut self,
        _from: Window,
        key: &TurnKey,
        started_ns: u64,
        ended_ns: Option<u64>,
        paths: &[PathBuf],
    ) {
        let mut touched: BTreeSet<PathBuf> = paths.iter().cloned().collect();
        // Running again after an end was noted: its older paths may now overlap a newer turn's.
        let reopens = ended_ns.is_none() && self.contest.has_ended(key);
        if reopens {
            touched.extend(self.contest.paths_of(key));
        }
        let before: BTreeSet<PathBuf> = touched
            .iter()
            .filter(|path| self.contest.is_contested(path))
            .cloned()
            .collect();
        if reopens {
            self.contest.reopen(key);
        }
        for path in paths {
            self.contest.note_edit(key, started_ns, path);
        }
        if let Some(ended_ns) = ended_ns {
            self.contest.note_end(key, ended_ns);
        }
        for path in &touched {
            if !before.contains(path) && self.contest.is_contested(path) {
                self.clear_contested(path);
            }
        }
    }

    /// Queues a live clear of `path` for each window that drew it live or has a live draw of it
    /// in flight: that answer may still put a drawing up. A live draw still queued is stopped when
    /// its turn comes ([`Due`]).
    fn clear_contested(&mut self, path: &Path) {
        let mut windows: BTreeSet<Window> = self.live_windows.get(path).copied().into_iter().collect();
        for flight in &self.in_flight {
            if let OverlayCall::LiveShow { path: drawn, .. } = &flight.queued.call {
                if drawn == path {
                    windows.insert(flight.queued.from);
                }
            }
        }
        for window in windows {
            self.enqueue(
                OverlayCall::LiveClear {
                    path: path.to_path_buf(),
                },
                window,
            );
        }
    }

    /// The files drawn live, each with the session and turn it shows, whichever window drew it.
    pub fn live_drawings(&self) -> &BTreeMap<PathBuf, (String, u32)> {
        &self.live_drawings
    }

    /// A driver for `owner` that keeps `contest`, the table an earlier driver of this window built:
    /// which paths are contested is decided from edits, not from the editor that showed them.
    pub fn with_contest(owner: Owner, contest: LiveContest) -> EditorOverlay {
        EditorOverlay {
            contest,
            ..EditorOverlay::new(owner)
        }
    }

    /// The contested-paths table, for the driver that replaces this one.
    pub fn into_contest(self) -> LiveContest {
        self.contest
    }

    /// Whether two turns running at once both edited `path`.
    pub fn is_contested(&self, path: &Path) -> bool {
        self.contest.is_contested(path)
    }

    /// Whether a call other than a live one, queued or waiting for its answer, draws over or clears
    /// `path`: what its answer leaves there replaces what any earlier answer drew.
    pub fn draws_pending(&self, path: &Path) -> bool {
        self.queue
            .iter()
            .chain(self.in_flight.iter().map(|flight| &flight.queued))
            .any(|queued| match &queued.call {
                OverlayCall::OpenAndShow { path: over, .. }
                | OverlayCall::Show { path: over, .. }
                | OverlayCall::Clear { path: over } => over == path,
                OverlayCall::ClearAll => true,
                OverlayCall::LiveShow { .. } | OverlayCall::LiveClear { .. } => false,
            })
    }

    fn forget_live(&mut self, path: &Path) {
        self.live_drawings.remove(path);
        self.live_windows.remove(path);
    }

    fn forget_live_drawings(&mut self) {
        self.live_drawings.clear();
        self.live_windows.clear();
    }

    /// Whether `queued` goes out as it is when its turn comes. Every call before it is answered by
    /// then, so which window's live drawing is over a file is known.
    fn due(&self, queued: &Queued) -> Due {
        match &queued.call {
            OverlayCall::LiveShow { path, .. } if self.contest.is_contested(path) => {
                if self.live_windows.get(path) == Some(&queued.from) {
                    Due::ClearInstead
                } else {
                    Due::Skip
                }
            }
            OverlayCall::LiveClear { path } if self.live_windows.get(path) != Some(&queued.from) => Due::Skip,
            _ => Due::Send,
        }
    }

    /// Queues this window's `call`, installing the module first when the editor has none from this
    /// driver. The call itself goes out only after the install answered.
    pub fn call(&mut self, rpc: &dyn EditorRpc, call: OverlayCall, now: Instant) {
        self.call_for(rpc, call, Window::One, now);
    }

    /// Queues `call` for the window `from`, behind every call already queued by any window. An open
    /// of a linked window's carries the request id [`linked_request_id`] gives it. A linked window's
    /// [`OverlayCall::ClearAll`] clears only the files that window drew, one [`OverlayCall::Clear`]
    /// each: no other window's overlays are its to take off. For the same reason a clear of a linked
    /// window's goes to the editor only if that window still holds the file when the clear's turn
    /// comes: a draw of another window's queued before it may have taken the file meanwhile.
    pub fn call_for(&mut self, rpc: &dyn EditorRpc, call: OverlayCall, from: Window, now: Instant) {
        self.call_for_op(rpc, call, from, None, now);
    }

    /// [`call_for`](Self::call_for), for the linked window's operation `op_id`: what a live call of that
    /// window's comes to is reported to it under that id, so it knows which of its live draws went up.
    pub fn call_for_op(
        &mut self,
        rpc: &dyn EditorRpc,
        call: OverlayCall,
        from: Window,
        op_id: Option<u64>,
        _now: Instant,
    ) {
        if let Window::Linked(peer) = from {
            self.gone.remove(&peer);
            if matches!(call, OverlayCall::ClearAll) {
                self.poll_in_flight();
                self.poll_install();
                self.sync_target(rpc);
                for path in self.linked_paths(peer).clone() {
                    self.enqueue(OverlayCall::Clear { path }, from);
                }
                self.advance(rpc);
                return;
            }
        }
        // A revert in the buffer is reported with its file's path as text, so over a file whose
        // path is not UTF-8 the buffer would change and the revert could not be recorded: such a
        // file is not drawn at all (opening it without hunks is harmless).
        let drawn_over = match &call {
            OverlayCall::OpenAndShow {
                path, hunks: Some(_), ..
            }
            | OverlayCall::Show { path, .. }
            | OverlayCall::LiveShow { path, .. } => Some(path),
            _ => None,
        };
        if let Some(path) = drawn_over.filter(|path| path.to_str().is_none()) {
            // A live draw is asked for at every edit of the file, unasked by the person: one line
            // per edit would say nothing new.
            if !matches!(call, OverlayCall::LiveShow { .. }) {
                eprintln!(
                    "eitri: the review is not drawn over {}: its path is not UTF-8",
                    path.display()
                );
            }
            self.fail(
                from,
                call,
                "the editor cannot show a review over a file whose path is not UTF-8",
            );
            return;
        }
        // The window that draws a session's turn holds that session; a move changes it only when
        // the move is decided (`release_session`).
        let session = match &call {
            OverlayCall::OpenAndShow { meta, .. } | OverlayCall::Show { meta, .. } => Some(&meta.session),
            OverlayCall::LiveShow { meta, .. } => Some(&meta.session),
            _ => None,
        };
        if let Some(session) = session {
            self.holders.insert(session.clone(), from);
        }
        self.poll_in_flight();
        self.poll_install();
        self.sync_target(rpc);
        self.enqueue_op(call, from, op_id);
        self.advance(rpc);
    }

    fn enqueue(&mut self, call: OverlayCall, from: Window) {
        self.enqueue_op(call, from, None);
    }

    fn enqueue_op(&mut self, call: OverlayCall, from: Window, op_id: Option<u64>) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.queue.push_back(Queued {
            seq,
            call,
            retried: false,
            from,
            op_id,
        });
    }

    /// Polls every answer, sends what can now go, and asks for the module's events when due.
    /// Returns what became known since the last tick, for each window.
    pub fn tick(&mut self, rpc: &dyn EditorRpc, now: Instant) -> Routed {
        self.poll_in_flight();
        self.poll_install();
        self.poll_drain();
        self.sync_target(rpc);
        self.advance(rpc);
        self.drain_if_due(rpc, now);
        Routed {
            mine: std::mem::take(&mut self.done),
            linked: std::mem::take(&mut self.linked),
        }
    }

    /// The editor is gone, or its panel's drafts were dropped: nothing in flight will be heard
    /// of, and nothing is known to be installed or drawn. Linked windows' opens still waiting are
    /// told so, since nothing else in window 1 answers them.
    pub fn editor_lost(&mut self) {
        let waiting: Vec<Queued> = self
            .queue
            .drain(..)
            .chain(self.in_flight.drain(..).map(|flight| flight.queued))
            .collect();
        for queued in waiting {
            if matches!(queued.from, Window::Linked(_)) {
                self.fail(
                    queued.from,
                    queued.call,
                    "the editor went away before it answered; nothing was opened",
                );
            }
        }
        self.install = Install::None;
        self.drain = None;
        self.last_drain = None;
        self.active = 0;
        self.more_events = false;
        self.drain_owed = false;
        self.live_only = false;
        self.pending = 0;
        self.active_paths.clear();
        self.forget_linked_paths();
        self.forget_live_drawings();
        self.done.clear();
    }

    /// The files this window last heard a review was drawn over.
    pub fn active_paths(&self) -> &BTreeSet<PathBuf> {
        &self.active_paths
    }

    /// The files the window at `peer` last heard a review was drawn over for it.
    pub fn linked_paths(&self, peer: PeerId) -> &BTreeSet<PathBuf> {
        self.linked_paths.get(&peer).unwrap_or(&NOTHING_DRAWN)
    }

    /// The linked window that drew `path`, if one did.
    fn path_holder(&self, path: &Path) -> Option<PeerId> {
        self.linked_paths
            .iter()
            .find(|(_, paths)| paths.contains(path))
            .map(|(peer, _)| *peer)
    }

    /// Takes `path` from the linked window that drew it, if one did.
    fn take_linked_path(&mut self, path: &Path) -> Option<PeerId> {
        let peer = self.path_holder(path)?;
        if let Some(paths) = self.linked_paths.get_mut(&peer) {
            paths.remove(path);
        }
        Some(peer)
    }

    /// The window the editor's events for `session` go to.
    pub fn holder(&self, session: &str) -> Window {
        self.holders.get(session).copied().unwrap_or(Window::One)
    }

    /// From a chat move's offer until its decision, the events of `session` wait here in order: one
    /// delivered to the source after it handed its records over would be lost from the review.
    pub fn hold_session(&mut self, session: &str) {
        self.held.entry(session.to_owned()).or_default();
    }

    /// The move of `session` is decided and `to` holds it now (the destination on a move, the
    /// source on a refusal or an abort): what waited for it goes there in order, then everything
    /// after it.
    pub fn release_session(&mut self, session: &str, to: Window) {
        self.holders.insert(session.to_owned(), to);
        let Some(waiting) = self.held.remove(session) else {
            return;
        };
        match to {
            Window::One => {
                for (kind, event) in waiting {
                    push_event(&mut self.done, kind, event);
                }
            }
            Window::Linked(peer) => self.events_push_linked(peer, session, waiting),
        }
    }

    /// The window at `peer` unlinked: every file drawn for it is cleared, its calls not yet sent are
    /// dropped, and nothing more goes to it; the other windows keep what they drew. The clears are
    /// that window's own, so a file another window draws over before they go out keeps its overlay.
    pub fn linked_gone(&mut self, peer: PeerId) {
        let window = Window::Linked(peer);
        self.gone.insert(peer);
        self.linked.remove(&peer);
        self.queue.retain(|queued| queued.from != window);
        for path in self.linked_paths(peer).clone() {
            self.enqueue(OverlayCall::Clear { path }, window);
        }
    }

    /// Whether [`EditorOverlay::tick`] has anything to do: an answer to wait for, a call to send,
    /// an outcome to hand out, or a drawn review whose events are due.
    pub fn wants_ticks(&self) -> bool {
        !self.queue.is_empty()
            || !self.in_flight.is_empty()
            || self.drain.is_some()
            || !self.done.is_empty()
            || !self.linked.is_empty()
            || matches!(self.install, Install::Installing { .. })
            || self.active > 0
            || self.more_events
            || self.drain_owed
    }

    /// The module is not in the editor (it never was, or it went away): nothing is drawn.
    fn forget_install(&mut self) {
        self.install = Install::None;
        self.drain = None;
        self.last_drain = None;
        self.active = 0;
        self.more_events = false;
        self.drain_owed = false;
        self.live_only = false;
        self.pending = 0;
        self.off_seq_at.clear();
        self.active_paths.clear();
        self.forget_linked_paths();
        self.forget_live_drawings();
    }

    fn forget_linked_paths(&mut self) {
        for (peer, paths) in std::mem::take(&mut self.linked_paths) {
            if !paths.is_empty() {
                self.push_linked(peer, LinkedEvent::Idle);
            }
        }
    }

    fn push_linked(&mut self, peer: PeerId, event: LinkedEvent) {
        if self.gone.contains(&peer) {
            return;
        }
        let waiting = self.linked.entry(peer).or_default();
        if let LinkedEvent::Events { session, events } = &event {
            if let Some(LinkedEvent::Events {
                session: last,
                events: into,
            }) = waiting.last_mut()
            {
                if last == session {
                    into.extend(events.iter().cloned());
                    return;
                }
            }
        }
        waiting.push(event);
    }

    fn events_push_linked(&mut self, peer: PeerId, session: &str, events: Vec<(EventKind, EditorRevertEvent)>) {
        if self.gone.contains(&peer) {
            eprintln!(
                "eitri: {} review events of session {session} were dropped: its window is gone",
                events.len()
            );
            return;
        }
        let events = events
            .iter()
            .map(|(kind, event)| editor_event_to_wire(*kind, event))
            .collect();
        self.push_linked(
            peer,
            LinkedEvent::Events {
                session: session.to_owned(),
                events,
            },
        );
    }

    fn fail(&mut self, from: Window, call: OverlayCall, why: &str) {
        if let OverlayCall::OpenAndShow { request_id, .. } = call {
            self.answer(from, request_id, Err(why.to_owned()));
        }
    }

    fn answer(&mut self, from: Window, request_id: String, result: Result<String, String>) {
        match from {
            Window::One => self.done.push(OverlayOutcome::Answered { request_id, result }),
            Window::Linked(peer) => match linked_op_id(peer, &request_id) {
                Some(op_id) => self.push_linked(peer, LinkedEvent::Answered { op_id, result }),
                None => eprintln!("eitri: an answer for {peer:?} was dropped: {request_id:?} names no operation"),
            },
        }
    }

    /// The window at `peer` no longer shows `path`: another window's call took it, or the editor did.
    fn push_off(&mut self, peer: PeerId, path: PathBuf) {
        self.push_linked(
            peer,
            LinkedEvent::Drawn {
                op_id: None,
                path,
                state: Drawn::Off,
            },
        );
    }

    /// `path` is drawn now by the window `by`; what another window drew there is gone.
    /// A live drawing over `path` is put back after this by the caller that drew it.
    fn draw(&mut self, path: PathBuf, by: Window, op_id: Option<u64>, state: Drawn) {
        self.forget_live(&path);
        self.settled(&path);
        let had_it = self.take_linked_path(&path);
        match by {
            Window::One => {
                if let Some(holder) = had_it {
                    self.push_off(holder, path.clone());
                }
                self.active_paths.insert(path);
            }
            Window::Linked(peer) => {
                if self.active_paths.remove(&path) {
                    push_off(&mut self.done, off_for(&path, "replaced"));
                }
                if let Some(holder) = had_it.filter(|holder| *holder != peer) {
                    self.push_off(holder, path.clone());
                }
                self.linked_paths.entry(peer).or_default().insert(path.clone());
                if self.gone.contains(&peer) {
                    // Drawn for a window that is no longer there to see it or clear it.
                    self.enqueue(OverlayCall::Clear { path }, by);
                    return;
                }
                self.push_linked(peer, LinkedEvent::Drawn { op_id, path, state });
            }
        }
    }

    /// Nothing is drawn over `path` after a call of the window `by`.
    fn undraw(&mut self, path: PathBuf, by: Window, op_id: Option<u64>) {
        self.forget_live(&path);
        self.settled(&path);
        let was_mine = self.active_paths.remove(&path);
        let had_it = self.take_linked_path(&path);
        match by {
            Window::One => {
                if let Some(holder) = had_it {
                    self.push_off(holder, path);
                }
            }
            Window::Linked(peer) => {
                if was_mine {
                    push_off(&mut self.done, off_for(&path, "replaced"));
                }
                if let Some(holder) = had_it.filter(|holder| *holder != peer) {
                    self.push_off(holder, path.clone());
                }
                self.push_linked(
                    peer,
                    LinkedEvent::Drawn {
                        op_id,
                        path,
                        state: Drawn::Cleared,
                    },
                );
            }
        }
    }

    /// The answer being handled said what is over `path`: an `off` queued before it is old news.
    fn settled(&mut self, path: &Path) {
        if let Some(seq) = self.answer_off_seq {
            self.off_seq_at.insert(path.to_path_buf(), seq);
        }
    }

    fn requeue(&mut self, queued: Queued) {
        let at = self
            .queue
            .iter()
            .position(|other| other.seq > queued.seq)
            .unwrap_or(self.queue.len());
        self.queue.insert(at, queued);
    }

    fn note_active(&mut self, answer: &Value) {
        if let Some(n) = lookup(answer, "active").and_then(Value::as_u64) {
            self.active = n;
            if n == 0 {
                self.active_paths.clear();
                self.forget_linked_paths();
                self.forget_live_drawings();
            }
        }
        // An answer that does not say counts as one that needs the fast drain.
        self.live_only = lookup(answer, "live_only").and_then(Value::as_bool) == Some(true);
        self.pending = lookup(answer, "pending").and_then(Value::as_u64).unwrap_or(0);
    }

    fn poll_in_flight(&mut self) {
        let mut waiting = Vec::new();
        for flight in std::mem::take(&mut self.in_flight) {
            match flight.pending.try_take() {
                None => waiting.push(flight),
                Some(Ok(answer)) if is_missing(&answer) => {
                    let InFlight { mut queued, epoch, .. } = flight;
                    if epoch == self.epoch {
                        // The module was gone when this call reached the editor.
                        if queued.retried {
                            self.fail(queued.from, queued.call, "the review module is not in the editor");
                            continue;
                        }
                        queued.retried = true;
                        if matches!(self.install, Install::Installed { .. }) {
                            self.forget_install();
                        }
                    }
                    self.requeue(queued);
                }
                Some(Ok(answer)) => {
                    let Queued { from, op_id, call, .. } = flight.queued;
                    self.answered(from, op_id, call, &answer);
                }
                Some(Err(error)) => {
                    if is_gone(&error) {
                        self.forget_install();
                    }
                    self.fail(flight.queued.from, flight.queued.call, &error.to_string());
                }
            }
        }
        self.in_flight = waiting;
    }

    /// `op_id` goes into what a live call of a linked window's is reported as; every other report stays
    /// as it was, an open's carrying the id of its own request.
    fn answered(&mut self, from: Window, op_id: Option<u64>, call: OverlayCall, answer: &Value) {
        self.answer_off_seq = lookup(answer, "off_seq").and_then(Value::as_u64);
        let was_drawn = self.active > 0;
        self.note_active(answer);
        if was_drawn && self.active == 0 {
            self.drain_owed = true;
        }
        // What the editor keeps over the file: the hunks drawn and those reverted in the buffer that
        // it still follows (`kept`; an older module answers `drawn` alone).
        let drawn_now = lookup(answer, "drawn").and_then(Value::as_u64);
        let drawn = lookup(answer, "kept").and_then(Value::as_u64).or(drawn_now);
        // Window 2 is told whether hunks are drawn, or only reverted ones are still followed.
        let state = if drawn_now.is_some_and(|n| n > 0) {
            Drawn::Drawn
        } else {
            Drawn::Kept
        };
        match call {
            OverlayCall::OpenAndShow { request_id, path, .. } => {
                let opened = lookup(answer, "opened").and_then(Value::as_bool);
                let op_id = match from {
                    Window::One => None,
                    Window::Linked(peer) => linked_op_id(peer, &request_id),
                };
                let result = match opened {
                    Some(true) => {
                        match drawn {
                            Some(0) => self.undraw(path, from, op_id),
                            Some(_) => self.draw(path, from, op_id, state),
                            None => {}
                        }
                        let mut message = "opened".to_owned();
                        if let Some(notice) = lookup(answer, "notice").and_then(bytes) {
                            message.push_str("; ");
                            message.push_str(&String::from_utf8_lossy(&notice));
                        }
                        Ok(message)
                    }
                    Some(false) => Err(lookup(answer, "error")
                        .and_then(bytes)
                        .map(|e| String::from_utf8_lossy(&e).into_owned())
                        .unwrap_or_else(|| "the editor could not open the file".to_owned())),
                    None => Err("the editor's answer was not understood".to_owned()),
                };
                self.answer(from, request_id, result);
            }
            OverlayCall::Show { path, .. } => {
                if drawn.is_some_and(|n| n > 0) {
                    self.draw(path, from, None, state);
                } else {
                    self.undraw(path, from, None);
                }
            }
            OverlayCall::Clear { path } => self.undraw(path, from, None),
            OverlayCall::ClearAll => {
                self.active_paths.clear();
                self.forget_linked_paths();
                self.forget_live_drawings();
            }
            OverlayCall::LiveShow { path, meta, .. } => self.live_answered(from, op_id, path, meta, answer),
            OverlayCall::LiveClear { path } => {
                // Not cleared: the overlay there is not live (any more), and the drawing it stands
                // for is someone else's to take off.
                if lookup(answer, "cleared").and_then(Value::as_bool) == Some(true) {
                    self.undraw(path, from, op_id);
                } else {
                    self.forget_live(&path);
                }
            }
        }
    }

    /// A live draw's answer. Left, or not shown, nothing in the editor changed and neither does who
    /// holds the file; otherwise the old drawing went and the live one, if any hunk was drawn, is up.
    fn live_answered(&mut self, from: Window, op_id: Option<u64>, path: PathBuf, meta: LiveMeta, answer: &Value) {
        let live = match parse_live_show(answer) {
            Ok(live) => live,
            Err(why) => {
                eprintln!("eitri: the editor's answer to a live drawing was not understood: {why}");
                return;
            }
        };
        if live.shown && !live.left {
            if live.drawn > 0 {
                self.draw(path.clone(), from, op_id, Drawn::Drawn);
                self.live_drawings
                    .insert(path.clone(), (meta.session.clone(), meta.turn));
                self.live_windows.insert(path.clone(), from);
            } else {
                self.undraw(path.clone(), from, op_id);
            }
        }
        if from == Window::One {
            self.done.push(OverlayOutcome::LiveDrawn {
                path,
                meta,
                drawn: live.drawn,
                left: live.left,
            });
        }
    }

    fn poll_install(&mut self) {
        let Install::Installing { pending, target } = &self.install else {
            return;
        };
        let target = *target;
        match pending.try_take() {
            None => {}
            Some(Ok(_)) => {
                self.epoch += 1;
                self.install = Install::Installed { target };
            }
            Some(Err(error)) => {
                self.forget_install();
                let why = error.to_string();
                for queued in std::mem::take(&mut self.queue) {
                    self.fail(queued.from, queued.call, &why);
                }
            }
        }
    }

    fn poll_drain(&mut self) {
        let Some((pending, epoch)) = &self.drain else {
            return;
        };
        let epoch = *epoch;
        match pending.try_take() {
            None => {}
            Some(Ok(answer)) => {
                self.drain = None;
                if is_missing(&answer) {
                    if epoch == self.epoch {
                        self.forget_install();
                    }
                    return;
                }
                self.note_active(&answer);
                self.more_events = lookup(&answer, "more").and_then(Value::as_bool) == Some(true);
                self.events(&answer);
            }
            Some(Err(error)) => {
                self.drain = None;
                if is_gone(&error) {
                    self.forget_install();
                }
            }
        }
    }

    /// The module's events, each to the window it belongs to, in the order they happened: a
    /// revert, its undo and a second revert of the same hunk must reach the draft in that order.
    fn events(&mut self, answer: &Value) {
        let items: &[Value] = match lookup(answer, "events") {
            Some(Value::Array(items)) => items,
            _ => &[],
        };
        for item in items {
            let kind = lookup(item, "kind").and_then(Value::as_str);
            match kind {
                Some("revert") => match parse_revert_event(item) {
                    Ok(event) => self.route_event(EventKind::Revert, event),
                    Err(why) => eprintln!("eitri: a review event from the editor was dropped: {why}"),
                },
                Some("unrevert") => match parse_unrevert_event(item) {
                    Ok(event) => self.route_event(EventKind::Unrevert, event),
                    Err(why) => eprintln!("eitri: a review event from the editor was dropped: {why}"),
                },
                Some("off") => match (text(item, "path"), text(item, "why")) {
                    (Ok(path), Ok(why)) => {
                        self.route_off(OffEvent { path, why }, lookup(item, "seq").and_then(Value::as_u64))
                    }
                    _ => eprintln!("eitri: a review event from the editor was dropped: unreadable off event"),
                },
                _ => {}
            }
        }
    }

    /// A revert or its undo goes to the window holding its session, or waits while that session
    /// moves.
    fn route_event(&mut self, kind: EventKind, event: EditorRevertEvent) {
        if let Some(waiting) = self.held.get_mut(&event.meta.session) {
            waiting.push((kind, event));
            return;
        }
        match self.holder(&event.meta.session) {
            Window::One => push_event(&mut self.done, kind, event),
            Window::Linked(peer) => {
                let session = event.meta.session.clone();
                self.events_push_linked(peer, &session, vec![(kind, event)]);
            }
        }
    }

    /// A buffer that stopped showing a review is news for the window that drew it, unless a call's
    /// answer drew or cleared the file after the event was queued: the drawing it is about is gone
    /// already, and the one up now, if any, stays.
    fn route_off(&mut self, off: OffEvent, seq: Option<u64>) {
        let path = PathBuf::from(OsStr::from_bytes(off.path.as_bytes()));
        if let (Some(seq), Some(at)) = (seq, self.off_seq_at.get(&path)) {
            if seq <= *at {
                return;
            }
        }
        self.off_seq_at.remove(&path);
        self.forget_live(&path);
        if let Some(holder) = self.take_linked_path(&path) {
            self.push_off(holder, path);
        } else {
            self.active_paths.remove(&path);
            push_off(&mut self.done, off);
        }
    }

    /// A different nvim behind the handle: what was installed and drawn was in the old one.
    fn sync_target(&mut self, rpc: &dyn EditorRpc) {
        let known = match &self.install {
            Install::None => return,
            Install::Installing { target, .. } | Install::Installed { target } => *target,
        };
        if rpc.target() == known {
            return;
        }
        self.forget_install();
        for flight in std::mem::take(&mut self.in_flight) {
            self.fail(
                flight.queued.from,
                flight.queued.call,
                "the editor changed before it answered",
            );
        }
    }

    /// Sends what is queued once the module is in, or starts the install it waits for.
    fn advance(&mut self, rpc: &dyn EditorRpc) {
        match self.install {
            Install::Installing { .. } => {}
            Install::None => {
                // Nothing is drawn while nothing is installed: a live clear has nothing to take off
                // and a contested live draw nothing to clear, so calls that are only these install
                // nothing. Behind any other call they wait: a live draw put back ahead of them may
                // still draw what they clear.
                if self.queue.iter().all(|queued| matches!(self.due(queued), Due::Skip)) {
                    self.queue.clear();
                }
                if !self.queue.is_empty() {
                    let target = rpc.target();
                    let pending = rpc.exec_lua(REVIEW_LUA, install_args(self.owner));
                    self.install = Install::Installing { pending, target };
                }
            }
            Install::Installed { .. } => {
                if !self.in_flight.is_empty() {
                    return;
                }
                while let Some(mut queued) = self.queue.pop_front() {
                    // Every call before this one is answered, so who holds the file is known: a
                    // file the asking window no longer holds has nothing of that window's to clear,
                    // and the editor's clear would take the holder's overlay off.
                    if let (Window::Linked(peer), OverlayCall::Clear { path }) = (queued.from, &queued.call) {
                        if !self.linked_paths(peer).contains(path) {
                            let path = path.clone();
                            self.push_linked(
                                peer,
                                LinkedEvent::Drawn {
                                    op_id: None,
                                    path,
                                    state: Drawn::Cleared,
                                },
                            );
                            continue;
                        }
                    }
                    match self.due(&queued) {
                        Due::Send => {}
                        Due::Skip => continue,
                        Due::ClearInstead => {
                            if let OverlayCall::LiveShow { path, .. } = queued.call {
                                queued.call = OverlayCall::LiveClear { path };
                            }
                        }
                    }
                    let pending = send(rpc, &queued.call);
                    self.in_flight.push(InFlight {
                        queued,
                        pending,
                        epoch: self.epoch,
                    });
                    break;
                }
            }
        }
    }

    fn drain_if_due(&mut self, rpc: &dyn EditorRpc, now: Instant) {
        if !matches!(self.install, Install::Installed { .. })
            || (self.active == 0 && !self.more_events && !self.drain_owed)
            || self.drain.is_some()
        {
            return;
        }
        // Only live drawings up, no event waiting at the last answer and no call on its way: the
        // drawings stay until dismissed, and a revert there can wait a few seconds.
        let every = if self.live_only && self.pending == 0 && self.queue.is_empty() && self.in_flight.is_empty() {
            LIVE_DRAIN_EVERY
        } else {
            DRAIN_EVERY
        };
        // Events left waiting by a full answer, or owed after the last overlay went, are asked for
        // at once, not half a second later.
        if !self.more_events
            && !self.drain_owed
            && self
                .last_drain
                .is_some_and(|at| now.saturating_duration_since(at) < every)
        {
            return;
        }
        self.last_drain = Some(now);
        self.drain_owed = false;
        self.drain = Some((rpc.exec_lua(TAKE_EVENTS_LUA, Vec::new()), self.epoch));
    }
}

/// The most live draws a linked window keeps waiting for window 1's report on. A draw nvim left
/// (an `o` drawing or a modified buffer kept the file) is never reported; a later report on its file
/// lets it go, and past this many the oldest goes.
const MAX_LIVE_SENT: usize = 256;

/// Window 2's driver: window 2 has no editor of its own, so its overlay calls go to window 1's
/// driver as editor operations, and what is drawn is known from what window 1 reports back
/// (`review_drawn`, `review_idle`), as the driver knows it from the editor's answers. Paths on the
/// link are relative to the project root; here they are absolute, as the flow uses them.
///
/// A live draw is known to be up when window 1 reports its file drawn under the id of the operation
/// that asked for it: window 1 answers this window's operations in the order they were sent, so a
/// report under a later operation's id, or under none, says the file holds something else.
pub struct RemoteOverlay {
    root: PathBuf,
    send: Box<dyn Fn(EditorOp) -> u64>,
    /// Opens sent and not answered yet: the operation's id to the panel's request id.
    opens: BTreeMap<u64, String>,
    active_paths: BTreeSet<PathBuf>,
    done: Vec<OverlayOutcome>,
    /// Live draws sent and not reported yet: the operation's id to the file, the drawing's meta and
    /// how many hunks went.
    live_sent: BTreeMap<u64, (PathBuf, LiveMeta, u32)>,
    /// The files this window drew live, each with the session and turn it shows.
    live_drawings: BTreeMap<PathBuf, (String, u32)>,
}

impl RemoteOverlay {
    /// `send` hands an operation to the link and returns the id it went under.
    pub fn new(root: &Path, send: Box<dyn Fn(EditorOp) -> u64>) -> RemoteOverlay {
        RemoteOverlay {
            root: root.to_path_buf(),
            send,
            opens: BTreeMap::new(),
            active_paths: BTreeSet::new(),
            done: Vec::new(),
            live_sent: BTreeMap::new(),
            live_drawings: BTreeMap::new(),
        }
    }

    fn relative(&self, path: &Path) -> Result<String, &'static str> {
        let rel = path
            .strip_prefix(&self.root)
            .map_err(|_| "the file is not inside the project")?;
        rel.to_str()
            .filter(|rel| !rel.is_empty())
            .map(str::to_owned)
            .ok_or("the editor window cannot be asked about a file whose path is not UTF-8")
    }

    /// Sends `call` to window 1 as the operation it stands for.
    pub fn call(&mut self, call: OverlayCall, _now: Instant) {
        let op = match &call {
            OverlayCall::OpenAndShow {
                path,
                line,
                meta,
                hunks,
                ..
            } => self.relative(path).map(|path| EditorOp::ReviewOpenAndShow {
                path,
                line: *line,
                meta: WireMeta::from(meta),
                hunks: hunks.as_ref().map(|hunks| hunks.iter().map(WireHunk::from).collect()),
            }),
            OverlayCall::Show { path, meta, hunks } => self.relative(path).map(|path| EditorOp::ReviewShow {
                path,
                meta: WireMeta::from(meta),
                hunks: hunks.iter().map(WireHunk::from).collect(),
            }),
            OverlayCall::Clear { path } => self.relative(path).map(|path| EditorOp::ReviewClear { path }),
            OverlayCall::ClearAll => Ok(EditorOp::ReviewClearMine {}),
            OverlayCall::LiveShow { path, meta, hunks } => self.relative(path).map(|path| EditorOp::ReviewLiveShow {
                path,
                meta: WireLiveMeta::from(meta),
                hunks: hunks.iter().map(WireHunk::from).collect(),
            }),
            OverlayCall::LiveClear { path } => {
                // Nothing of this window's is live there, drawn or on its way: there is nothing to take off.
                let live = self.live_drawings.contains_key(path) || self.live_sent.values().any(|(p, ..)| p == path);
                if !live {
                    return;
                }
                self.relative(path).map(|path| EditorOp::ReviewLiveClear { path })
            }
        };
        match op {
            Ok(op) => {
                let id = (self.send)(op);
                match call {
                    OverlayCall::OpenAndShow { request_id, .. } => {
                        self.opens.insert(id, request_id);
                    }
                    OverlayCall::LiveShow { path, meta, hunks } => {
                        self.live_sent.insert(id, (path, meta, hunks.len() as u32));
                        while self.live_sent.len() > MAX_LIVE_SENT {
                            self.live_sent.pop_first();
                        }
                    }
                    _ => {}
                }
            }
            Err(why) => {
                if let OverlayCall::OpenAndShow { request_id, .. } = call {
                    self.done.push(OverlayOutcome::Answered {
                        request_id,
                        result: Err(why.to_owned()),
                    });
                }
            }
        }
    }

    /// Window 1's answer to one of this window's operations. Only an open's is news: window 1
    /// answers the others when it queues them, and what they drew comes as `review_drawn`.
    pub fn answered(&mut self, op_id: u64, result: Result<String, String>) {
        if let Some(request_id) = self.opens.remove(&op_id) {
            self.done.push(OverlayOutcome::Answered { request_id, result });
        }
    }

    /// `review_drawn`: what window 1's driver now knows of a file this window drew; `path` is
    /// relative to the project root, as the link carries it.
    pub fn review_drawn(&mut self, path: &str, state: Drawn) {
        self.review_drawn_by(None, path, state);
    }

    /// [`review_drawn`](Self::review_drawn) with the id of the operation whose outcome it is, when
    /// window 1 named one: a live draw of this window's is up only when its own id comes back.
    pub fn review_drawn_by(&mut self, op_id: Option<u64>, path: &str, state: Drawn) {
        let inside = !path.is_empty() && Path::new(path).components().all(|c| matches!(c, Component::Normal(_)));
        if !inside {
            eprintln!("eitri: window 1 reported a review drawn over a path outside the project: {path:?}");
            return;
        }
        let abs = self.root.join(path);
        let live = op_id
            .and_then(|id| self.live_sent.remove(&id))
            .filter(|(sent, ..)| *sent == abs);
        if let Some(id) = op_id {
            // Window 1 answered every earlier operation of this window first: a live draw of the
            // file sent before this one that has not been reported never will be.
            self.live_sent
                .retain(|sent_id, (sent, ..)| *sent_id > id || *sent != abs);
        }
        match state {
            Drawn::Drawn | Drawn::Kept => {
                self.active_paths.insert(abs.clone());
                match live {
                    Some((_, meta, hunks)) => {
                        self.live_drawings
                            .insert(abs.clone(), (meta.session.clone(), meta.turn));
                        // Window 1 says that hunks went up, not how many: those sent, at least one.
                        self.done.push(OverlayOutcome::LiveDrawn {
                            path: abs,
                            meta,
                            drawn: hunks.max(1),
                            left: false,
                        });
                    }
                    None => {
                        self.live_drawings.remove(&abs);
                    }
                }
            }
            Drawn::Cleared => {
                self.active_paths.remove(&abs);
                self.live_drawings.remove(&abs);
                if let Some((_, meta, _)) = live {
                    self.done.push(OverlayOutcome::LiveDrawn {
                        path: abs,
                        meta,
                        drawn: 0,
                        left: false,
                    });
                }
            }
            Drawn::Off => {
                self.active_paths.remove(&abs);
                self.live_drawings.remove(&abs);
                push_off(&mut self.done, off_for(&abs, "reported"));
            }
        }
    }

    /// `review_idle`: window 1's driver reports none of this window's files drawn.
    pub fn review_idle(&mut self) {
        self.active_paths.clear();
        self.live_drawings.clear();
    }

    /// `review_events`: reverts made in the editor, and their undos, for a session this window
    /// holds, in order. An event naming another session is not this window's and is dropped.
    pub fn review_events(&mut self, session: &str, events: Vec<serde_json::Value>) {
        for value in events {
            match editor_event_from_wire(&value) {
                Ok((kind, event)) if event.meta.session == session => push_event(&mut self.done, kind, event),
                Ok(_) => eprintln!("eitri: a review event for another session than {session} was dropped"),
                Err(why) => eprintln!("eitri: a review event from window 1 was dropped: {why}"),
            }
        }
    }

    /// What became known since the last tick.
    pub fn tick(&mut self, _now: Instant) -> Vec<OverlayOutcome> {
        std::mem::take(&mut self.done)
    }

    /// Window 1 or its editor went away: no answer is coming and nothing is known to be drawn.
    pub fn editor_lost(&mut self) {
        self.opens.clear();
        self.active_paths.clear();
        self.done.clear();
        self.live_sent.clear();
        self.live_drawings.clear();
    }

    pub fn active_paths(&self) -> &BTreeSet<PathBuf> {
        &self.active_paths
    }

    /// The files this window drew live, each with the session and turn it shows.
    pub fn live_drawings(&self) -> &BTreeMap<PathBuf, (String, u32)> {
        &self.live_drawings
    }

    pub fn wants_ticks(&self) -> bool {
        !self.done.is_empty()
    }
}

/// The most notes a linked window keeps waiting for their shown answer. The flow takes each answer
/// or lets it go after a while; past this many the oldest is dropped unread.
const MAX_LIVE_QUESTIONS: usize = 64;

/// Window 2's way to window 1 for its live drawings: each note goes as an [`EditorOp::ReviewLiveNote`]
/// naming its files relative to the project root, at most [`MAX_SHOWN_PATHS`] an operation, and window
/// 1's editor's answer to which of them a window shows is the reply. A file that cannot be named to
/// window 1 (outside the project, not UTF-8) is not sent and counts as not shown.
pub struct RemoteLiveAsk {
    root: PathBuf,
    send: Box<dyn Fn(EditorOp) -> Option<Pending>>,
    next: Cell<u64>,
    asked: RefCell<BTreeMap<u64, ShownAsked>>,
}

/// One note's question: how many paths it named, and its operations' answers.
struct ShownAsked {
    len: usize,
    parts: Vec<ShownPart>,
}

/// One operation of a note: the places of its paths in the note, and its answer once in.
struct ShownPart {
    at: Vec<usize>,
    pending: Pending,
    answer: Option<Result<Vec<bool>, String>>,
}

impl RemoteLiveAsk {
    /// `send` hands an operation to window 1 and returns its answer to come, or `None` while the
    /// windows are not linked.
    pub fn new(root: &Path, send: Box<dyn Fn(EditorOp) -> Option<Pending>>) -> RemoteLiveAsk {
        RemoteLiveAsk {
            root: root.to_path_buf(),
            send,
            next: Cell::new(1),
            asked: RefCell::new(BTreeMap::new()),
        }
    }

    /// `abs` as window 1 takes it: relative to the project root, in UTF-8, plain names only.
    fn relative(&self, abs: &Path) -> Option<String> {
        let rel = abs.strip_prefix(&self.root).ok()?.to_str()?;
        let plain = !rel.is_empty()
            && !rel.contains('\0')
            && Path::new(rel).components().all(|c| matches!(c, Component::Normal(_)));
        plain.then(|| rel.to_owned())
    }
}

impl LiveAsk for RemoteLiveAsk {
    fn note(&self, key: &TurnKey, started_ns: u64, ended_ns: Option<u64>, abs_paths: &[PathBuf]) -> Option<u64> {
        let named: Vec<(usize, String)> = abs_paths
            .iter()
            .enumerate()
            .filter_map(|(at, abs)| Some((at, self.relative(abs)?)))
            .collect();
        let asking = ended_ns.is_none() && !named.is_empty();
        if !asking && ended_ns.is_none() {
            return None;
        }
        // An end is told even with no path to name: it ends the turn in window 1's table.
        let batches: Vec<&[(usize, String)]> = if named.is_empty() {
            vec![&[]]
        } else {
            named.chunks(MAX_SHOWN_PATHS).collect()
        };
        let mut parts = Vec::new();
        for batch in batches {
            let pending = (self.send)(EditorOp::ReviewLiveNote {
                session: key.session.clone(),
                turn_id: key.turn_id.clone(),
                started_ns,
                ended_ns,
                paths: batch.iter().map(|(_, rel)| rel.clone()).collect(),
            })?;
            parts.push(ShownPart {
                at: batch.iter().map(|(at, _)| *at).collect(),
                pending,
                answer: None,
            });
        }
        if !asking {
            return None;
        }
        let id = self.next.get();
        self.next.set(id + 1);
        let mut asked = self.asked.borrow_mut();
        asked.insert(
            id,
            ShownAsked {
                len: abs_paths.len(),
                parts,
            },
        );
        while asked.len() > MAX_LIVE_QUESTIONS {
            asked.pop_first();
        }
        Some(id)
    }

    fn take_shown(&self, op_id: u64) -> Option<Result<Vec<bool>, String>> {
        let mut asked = self.asked.borrow_mut();
        let question = asked.get_mut(&op_id)?;
        for part in question.parts.iter_mut().filter(|part| part.answer.is_none()) {
            part.answer = part.pending.try_take().map(|reply| {
                reply
                    .map_err(|e| e.to_string())
                    .and_then(|answer| parse_live_shown(&answer, part.at.len()))
            });
        }
        if question.parts.iter().any(|part| part.answer.is_none()) {
            return None;
        }
        let question = asked.remove(&op_id)?;
        let mut shown = vec![false; question.len];
        let mut answered = false;
        let mut failed = None;
        for part in question.parts {
            match part.answer {
                Some(Ok(list)) => {
                    answered = true;
                    for (at, is_shown) in part.at.into_iter().zip(list) {
                        shown[at] = is_shown;
                    }
                }
                Some(Err(why)) => {
                    failed.get_or_insert(why);
                }
                None => {}
            }
        }
        // A batch that failed counts as not shown; only when none was answered is the question failed.
        Some(match (answered, failed) {
            (false, Some(why)) => Err(why),
            _ => Ok(shown),
        })
    }
}

/// A window's overlay driver: its own over its own editor, or, in window 2, the one that reaches
/// window 1's. The flow's code is the same with either.
// One per window, made once and never copied around: the larger variant costs nothing worth a box.
#[allow(clippy::large_enum_variant)]
pub enum Overlay {
    Local(EditorOverlay),
    Remote(RemoteOverlay),
}

impl Overlay {
    /// Whether a call can reach an editor now: a local driver needs this window's editor, and a
    /// linked window's goes to window 1, which answers for itself.
    pub fn reachable(&self, rpc: Option<&dyn EditorRpc>) -> bool {
        match self {
            Overlay::Local(_) => rpc.is_some(),
            Overlay::Remote(_) => true,
        }
    }

    pub fn call(&mut self, rpc: Option<&dyn EditorRpc>, call: OverlayCall, now: Instant) {
        match (self, rpc) {
            (Overlay::Local(overlay), Some(rpc)) => overlay.call(rpc, call, now),
            (Overlay::Local(overlay), None) => overlay.fail(Window::One, call, "this window has no editor"),
            (Overlay::Remote(overlay), _) => overlay.call(call, now),
        }
    }

    pub fn tick(&mut self, rpc: Option<&dyn EditorRpc>, now: Instant) -> Routed {
        match (self, rpc) {
            (Overlay::Local(overlay), Some(rpc)) => overlay.tick(rpc, now),
            (Overlay::Local(_), None) => Routed::default(),
            (Overlay::Remote(overlay), _) => Routed {
                mine: overlay.tick(now),
                linked: BTreeMap::new(),
            },
        }
    }

    pub fn editor_lost(&mut self) {
        match self {
            Overlay::Local(overlay) => overlay.editor_lost(),
            Overlay::Remote(overlay) => overlay.editor_lost(),
        }
    }

    pub fn active_paths(&self) -> &BTreeSet<PathBuf> {
        match self {
            Overlay::Local(overlay) => overlay.active_paths(),
            Overlay::Remote(overlay) => overlay.active_paths(),
        }
    }

    pub fn live_drawings(&self) -> &BTreeMap<PathBuf, (String, u32)> {
        match self {
            Overlay::Local(overlay) => overlay.live_drawings(),
            Overlay::Remote(overlay) => overlay.live_drawings(),
        }
    }

    /// See [`EditorOverlay::draws_pending`]; a linked window's calls wait in window 1's driver.
    pub fn draws_pending(&self, path: &Path) -> bool {
        match self {
            Overlay::Local(overlay) => overlay.draws_pending(path),
            Overlay::Remote(_) => false,
        }
    }

    pub fn wants_ticks(&self) -> bool {
        match self {
            Overlay::Local(overlay) => overlay.wants_ticks(),
            Overlay::Remote(overlay) => overlay.wants_ticks(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(bytes: &[u8]) -> Vec<Vec<u8>> {
        bytes.split_inclusive(|b| *b == b'\n').map(<[u8]>::to_vec).collect()
    }

    fn meta() -> ShowMeta {
        ShowMeta {
            tab: 7,
            session: "s-1".to_owned(),
            turn: 3,
            scope: Scope::Session,
        }
    }

    fn hunk() -> ShowHunk {
        ShowHunk::from_file_lines(2, (4, 2), (4, 3), &split(b"a\r\nb\xff\n"), &split(b"a\r\nc\nend"))
    }

    fn get<'a>(map: &'a Value, name: &str) -> &'a Value {
        field(map, name).unwrap()
    }

    /// The event the module would send back for `hunk` shown with `meta`: every field it echoes,
    /// strings as nvim sends them (msgpack str, whatever the bytes).
    fn event_for(path: &str, meta: &ShowMeta, hunk: &ShowHunk) -> Value {
        let as_str = |b: &[u8]| -> Value {
            // A fixstr header: these lines are all shorter than 32 bytes.
            assert!(b.len() < 32);
            let mut encoded = vec![0xa0 | b.len() as u8];
            encoded.extend_from_slice(b);
            rmpv::decode::read_value(&mut encoded.as_slice()).unwrap()
        };
        let shown = hunk_value(hunk);
        let echo = |name: &str| -> Value {
            match get(&shown, name) {
                Value::Array(items) => Value::Array(
                    items
                        .iter()
                        .map(|v| match v {
                            Value::Binary(b) => as_str(b),
                            other => other.clone(),
                        })
                        .collect(),
                ),
                other => other.clone(),
            }
        };
        map(vec![
            ("kind", Value::from("revert")),
            ("tab", Value::from(meta.tab)),
            ("session", Value::from(meta.session.as_str())),
            ("turn", Value::from(meta.turn)),
            ("scope", Value::from(meta.scope.as_str())),
            ("path", Value::from(path)),
            ("hunk_id", Value::from(hunk.id)),
            ("old_start", Value::from(hunk.old_start)),
            ("old_len", Value::from(hunk.old_len)),
            ("new_start", Value::from(hunk.new_start)),
            ("new_len", Value::from(hunk.new_len)),
            ("at_line", Value::from(hunk.new_start + 2)),
            ("old_lines", echo("old_lines")),
            ("new_lines", echo("new_lines")),
            ("old_eols", echo("old_eols")),
            ("new_eols", echo("new_eols")),
        ])
    }

    fn with(event: &Value, name: &str, value: Value) -> Value {
        let mut entries = event.as_map().unwrap().clone();
        for (k, v) in entries.iter_mut() {
            if k.as_str() == Some(name) {
                *v = value.clone();
            }
        }
        Value::Map(entries)
    }

    #[test]
    fn show_args_carry_bytes_as_binary_and_eols_as_names() {
        let path = Path::new(std::ffi::OsStr::from_bytes(b"/p/it's 100% #1\xff.txt"));
        let args = show_args(path, &meta(), &[hunk()]);
        assert_eq!(args.len(), 3);
        assert_eq!(args[0], Value::Binary(b"/p/it's 100% #1\xff.txt".to_vec()));
        assert_eq!(get(&args[1], "tab"), &Value::from(7u64));
        assert_eq!(get(&args[1], "session"), &Value::from("s-1"));
        assert_eq!(get(&args[1], "turn"), &Value::from(3u32));
        assert_eq!(get(&args[1], "scope"), &Value::from("session"));
        let h = &args[2].as_array().unwrap()[0];
        assert_eq!(get(h, "id"), &Value::from(2u32));
        assert_eq!(get(h, "old_start"), &Value::from(4u32));
        assert_eq!(get(h, "new_len"), &Value::from(3u32));
        assert_eq!(
            get(h, "old_lines"),
            &Value::Array(vec![Value::Binary(b"a".to_vec()), Value::Binary(b"b\xff".to_vec())])
        );
        assert_eq!(
            get(h, "new_lines"),
            &Value::Array(vec![
                Value::Binary(b"a".to_vec()),
                Value::Binary(b"c".to_vec()),
                Value::Binary(b"end".to_vec())
            ])
        );
        assert_eq!(
            get(h, "old_eols"),
            &Value::Array(vec![Value::from("crlf"), Value::from("lf")])
        );
        assert_eq!(
            get(h, "new_eols"),
            &Value::Array(vec![Value::from("crlf"), Value::from("lf"), Value::from("missing")])
        );

        let open = open_and_show_args(Path::new("/p/f"), Some(9), &meta(), None);
        assert_eq!(open[0], Value::Binary(b"/p/f".to_vec()));
        assert_eq!(open[1], Value::from(9u32));
        assert_eq!(open[3], Value::Nil);
        let open = open_and_show_args(Path::new("/p/f"), None, &meta(), Some(&[hunk()]));
        assert_eq!(open[1], Value::Nil);
        assert_eq!(open[3], args[2]);
        assert_eq!(clear_args(Path::new("/p/f")), vec![Value::Binary(b"/p/f".to_vec())]);

        assert_eq!(
            install_args(Owner::Companion { channel: 12 }),
            vec![Value::from("companion:12"), Value::from(REVIEW_LUA_VERSION)]
        );
        assert_eq!(install_args(Owner::Embedded)[0], Value::from("embedded"));
    }

    #[test]
    fn a_revert_event_round_trips_through_show_args() {
        let h = hunk();
        assert_eq!(h.old_bytes(), b"a\r\nb\xff\n");
        assert_eq!(h.new_bytes(), b"a\r\nc\nend");
        let event = event_for("/p/f.txt", &meta(), &h);
        assert!(
            matches!(get(&event, "old_lines").as_array().unwrap()[1], Value::String(ref s) if s.as_str().is_none()),
            "the invalid line arrives as a msgpack str, as nvim sends it"
        );
        let parsed = parse_revert_event(&event).expect("a revert event");
        assert_eq!(parsed.meta, meta());
        assert_eq!(parsed.path, "/p/f.txt");
        assert_eq!(parsed.hunk, h);
        assert_eq!(
            parsed.at_line,
            h.new_start + 2,
            "apart from the header, which is echoed as shown"
        );
        assert_eq!(parsed.hunk.old_bytes(), b"a\r\nb\xff\n");

        // A str holding the bytes a2 a1 ff, as rmpv decodes it off the wire.
        let raw = rmpv::decode::read_value(&mut [0xa2u8, 0xa1, 0xff].as_slice()).unwrap();
        assert!(matches!(raw, Value::String(ref s) if s.as_str().is_none()));
        let one = ShowHunk::from_file_lines(1, (1, 1), (1, 1), &split(b"\xa1\xff\n"), &split(b"x\n"));
        let event = with(&event_for("/p/f", &meta(), &one), "old_lines", Value::Array(vec![raw]));
        assert_eq!(parse_revert_event(&event).unwrap().hunk.old_bytes(), b"\xa1\xff\n");

        // An empty side may arrive as an empty map.
        let made = ShowHunk::from_file_lines(1, (0, 0), (1, 1), &[], &split(b"x\n"));
        let event = event_for("/p/f", &meta(), &made);
        let event = with(&event, "old_lines", Value::Map(vec![]));
        let event = with(&event, "old_eols", Value::Map(vec![]));
        assert_eq!(parse_revert_event(&event).unwrap().hunk, made);
    }

    #[test]
    fn an_unknown_eol_fails_the_event() {
        let good = event_for("/p/f", &meta(), &hunk());
        let bad_eol = with(
            &good,
            "old_eols",
            Value::Array(vec![Value::from("crlf"), Value::from("cr")]),
        );
        assert!(parse_revert_event(&bad_eol).unwrap_err().contains("line ending"));
        let short = with(&good, "old_eols", Value::Array(vec![Value::from("crlf")]));
        assert!(parse_revert_event(&short).unwrap_err().contains("differ in length"));
        let both_short = with(
            &with(&good, "old_eols", Value::Array(vec![Value::from("crlf")])),
            "old_lines",
            Value::Array(vec![Value::from("a")]),
        );
        assert!(parse_revert_event(&both_short).unwrap_err().contains("old_len"));
        for (name, value) in [
            ("scope", Value::from("all")),
            ("tab", Value::from(-1)),
            ("turn", Value::from(u64::from(u32::MAX) + 1)),
            ("hunk_id", Value::from(1.5)),
            ("session", Value::Binary(vec![0xff])),
            ("path", Value::Binary(vec![b'/', 0xff])),
            ("kind", Value::from("off")),
            ("new_lines", Value::from("not a list")),
            ("at_line", Value::from(0)),
            ("at_line", Value::from(-3)),
        ] {
            assert!(parse_revert_event(&with(&good, name, value)).is_err(), "{name}");
        }
        let mut missing = good.as_map().unwrap().clone();
        missing.retain(|(k, _)| k.as_str() != Some("new_start"));
        assert!(parse_revert_event(&Value::Map(missing))
            .unwrap_err()
            .contains("new_start"));
        assert!(parse_revert_event(&Value::from(1)).is_err());
    }

    #[test]
    fn no_hunks_for_binary_or_over_the_cap() {
        let small = vec![hunk()];
        assert_eq!(overlay_hunks(false, Some(small.clone())), Some(small.clone()));
        assert_eq!(overlay_hunks(true, Some(small)), None);
        assert_eq!(overlay_hunks(false, None), None);
        let sized = |old: u32, new: u32| ShowHunk {
            id: 0,
            old_start: 1,
            old_len: old,
            new_start: 1,
            new_len: new,
            old_lines: Vec::new(),
            new_lines: Vec::new(),
        };
        let cap = MAX_OVERVIEW_DIFF_LINES as u32;
        let at_cap = vec![sized(cap - 10, 3), sized(4, 10)];
        assert_eq!(overlay_hunks(false, Some(at_cap.clone())), Some(at_cap));
        assert_eq!(overlay_hunks(false, Some(vec![sized(cap - 10, 3), sized(4, 11)])), None);
    }

    #[test]
    fn every_call_constant_starts_with_the_missing_guard() {
        // The install is the module itself and cannot check for itself; every other call can.
        let guard = "local r = rawget(_G, '__eitri_review') if not r then return { missing = true } end ";
        for (name, code) in [
            ("SHOW_LUA", SHOW_LUA),
            ("OPEN_AND_SHOW_LUA", OPEN_AND_SHOW_LUA),
            ("CLEAR_LUA", CLEAR_LUA),
            ("CLEAR_ALL_LUA", CLEAR_ALL_LUA),
            ("TAKE_EVENTS_LUA", TAKE_EVENTS_LUA),
        ] {
            assert!(code.starts_with(guard), "{name}: {code}");
            assert!(code.len() > guard.len(), "{name} calls something");
        }
    }

    /// The limit `review.lua` declares as `local <name> = <bytes>`.
    fn lua_limit(name: &str) -> u64 {
        let line = REVIEW_LUA
            .lines()
            .find(|l| l.starts_with(&format!("local {name} = ")))
            .unwrap_or_else(|| panic!("review.lua declares {name}"));
        line.rsplit(' ').next().unwrap().parse().unwrap()
    }

    #[test]
    fn the_events_answer_fits_well_inside_one_frame() {
        // One answer holds events up to the budget, or a single event, which is never larger than
        // the revert limit; the estimate is above the real encoding, so half a frame leaves room.
        let revert = lua_limit("MAX_REVERT_EVENT_BYTES");
        let take = lua_limit("TAKE_EVENTS_BYTES");
        assert!(revert <= take, "a single revert fits in one answer's budget");
        assert!(take <= crate::nvim_rpc::MAX_FRAME / 2, "{take}");
    }

    #[test]
    fn review_lua_builds_no_source() {
        for word in [
            "vim.cmd",
            "nvim_command",
            "nvim_exec",
            "loadstring",
            "load(",
            "dofile",
            "fnameescape",
            "execute(",
            "vim.fn.bufnr(",
        ] {
            assert!(!REVIEW_LUA.contains(word), "review.lua contains `{word}`");
        }
    }

    // ---- one driver for two windows --------------------------------------------------------------

    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use crate::nvim_rpc::Answer;

    /// An nvim that keeps what is drawn as the module would (one overlay per file, live or not),
    /// queues the events a test gives it, and answers each call when it is made or, while `hold` is
    /// set, when the test releases it. Every answer says what the module's status does: how many
    /// overlays are up, how many events wait, and whether every overlay is live.
    struct Nvim {
        serial: Cell<u64>,
        sent: RefCell<Vec<(&'static str, Vec<Value>)>>,
        held: RefCell<VecDeque<(Answer, Value)>>,
        hold: Cell<bool>,
        drawn: RefCell<BTreeSet<Vec<u8>>>,
        events: RefCell<Vec<Value>>,
        /// The drawn files whose overlay is a live one.
        live: RefCell<BTreeSet<Vec<u8>>>,
        /// Files whose buffer is modified: a live drawing leaves them.
        modified: RefCell<BTreeSet<Vec<u8>>>,
        /// Files loaded in no window.
        hidden: RefCell<BTreeSet<Vec<u8>>>,
        /// The module is in this nvim; without it every guarded call answers `missing`.
        module: Cell<bool>,
        /// How many `off` events were queued.
        off_seq: Cell<u64>,
    }

    impl Nvim {
        fn new() -> Nvim {
            Nvim {
                serial: Cell::new(1),
                sent: RefCell::new(Vec::new()),
                held: RefCell::new(VecDeque::new()),
                hold: Cell::new(false),
                drawn: RefCell::new(BTreeSet::new()),
                events: RefCell::new(Vec::new()),
                live: RefCell::new(BTreeSet::new()),
                modified: RefCell::new(BTreeSet::new()),
                hidden: RefCell::new(BTreeSet::new()),
                module: Cell::new(false),
                off_seq: Cell::new(0),
            }
        }

        fn status(&self, mut pairs: Vec<(&str, Value)>) -> Value {
            let drawn = self.drawn.borrow();
            let live = self.live.borrow();
            pairs.push(("active", Value::from(drawn.len() as u64)));
            pairs.push(("pending", Value::from(self.events.borrow().len() as u64)));
            pairs.push(("live_only", Value::from(drawn.iter().all(|p| live.contains(p)))));
            pairs.push(("off_seq", Value::from(self.off_seq.get())));
            map(pairs)
        }

        /// The person took the drawing over `path` off: an `off` event, numbered, waits.
        fn dismiss(&self, path: &str) {
            self.drawn.borrow_mut().remove(&bytes_of(path));
            self.live.borrow_mut().remove(&bytes_of(path));
            self.off_seq.set(self.off_seq.get() + 1);
            self.events.borrow_mut().push(map(vec![
                ("kind", Value::from("off")),
                ("path", Value::from(path)),
                ("why", Value::from("user")),
                ("seq", Value::from(self.off_seq.get())),
            ]));
        }

        /// The nvim restarted behind the same handle: no module, nothing drawn.
        fn restart(&self) {
            self.module.set(false);
            self.drawn.borrow_mut().clear();
            self.live.borrow_mut().clear();
            self.off_seq.set(0);
        }

        fn reply(&self, code: &'static str, args: &[Value]) -> Value {
            let path = || match &args[0] {
                Value::Binary(b) => b.clone(),
                other => panic!("a path travels as bytes: {other}"),
            };
            if code == REVIEW_LUA {
                self.module.set(true);
                return map(vec![("installed", Value::from(true))]);
            }
            if !self.module.get() {
                return map(vec![("missing", Value::from(true))]);
            }
            if code == SHOW_LUA {
                self.drawn.borrow_mut().insert(path());
                self.live.borrow_mut().remove(&path());
                self.status(vec![("drawn", Value::from(1)), ("kept", Value::from(1))])
            } else if code == OPEN_AND_SHOW_LUA {
                if args[3] != Value::Nil {
                    self.drawn.borrow_mut().insert(path());
                    self.live.borrow_mut().remove(&path());
                }
                self.status(vec![
                    ("opened", Value::from(true)),
                    ("drawn", Value::from(1)),
                    ("kept", Value::from(1)),
                ])
            } else if code == CLEAR_LUA {
                self.drawn.borrow_mut().remove(&path());
                self.live.borrow_mut().remove(&path());
                self.status(Vec::new())
            } else if code == CLEAR_ALL_LUA {
                self.drawn.borrow_mut().clear();
                self.live.borrow_mut().clear();
                self.status(Vec::new())
            } else if code == LIVE_SHOW_LUA {
                let p = path();
                let hunks = args[2].as_array().map_or(0, Vec::len) as u64;
                let shown = !self.hidden.borrow().contains(&p);
                let held = self.drawn.borrow().contains(&p) && !self.live.borrow().contains(&p);
                let left = shown && (held || self.modified.borrow().contains(&p));
                let mut drawn = 0;
                if shown && !left {
                    self.drawn.borrow_mut().remove(&p);
                    self.live.borrow_mut().remove(&p);
                    if hunks > 0 {
                        self.drawn.borrow_mut().insert(p.clone());
                        self.live.borrow_mut().insert(p);
                        drawn = hunks;
                    }
                }
                self.status(vec![
                    ("drawn", Value::from(drawn)),
                    ("shown", Value::from(shown)),
                    ("left", Value::from(left)),
                    ("skipped", Value::from(0)),
                ])
            } else if code == LIVE_CLEAR_LUA {
                let p = path();
                let cleared = self.live.borrow_mut().remove(&p);
                if cleared {
                    self.drawn.borrow_mut().remove(&p);
                }
                self.status(vec![("cleared", Value::from(cleared))])
            } else if code == TAKE_EVENTS_LUA {
                let events: Vec<Value> = self.events.borrow_mut().drain(..).collect();
                self.status(vec![("events", Value::Array(events))])
            } else {
                panic!("an unexpected call")
            }
        }

        /// Answers every call held so far, oldest first.
        fn release(&self) {
            let held: Vec<(Answer, Value)> = self.held.borrow_mut().drain(..).collect();
            for (answer, value) in held {
                answer.send(Ok(value));
            }
        }

        /// Calls held unanswered that are not the module's event drain.
        fn calls_in_flight(&self) -> usize {
            self.held
                .borrow()
                .iter()
                .filter(|(_, value)| lookup(value, "events").is_none())
                .count()
        }

        /// Each call's code and, when it names one, its path.
        fn calls(&self) -> Vec<(&'static str, Option<String>)> {
            self.sent
                .borrow()
                .iter()
                .filter(|(code, _)| *code != TAKE_EVENTS_LUA)
                .map(|(code, args)| {
                    let path = match args.first() {
                        Some(Value::Binary(b)) if *code != REVIEW_LUA => Some(String::from_utf8(b.clone()).unwrap()),
                        _ => None,
                    };
                    (*code, path)
                })
                .collect()
        }

        fn count(&self, code: &str) -> usize {
            self.sent.borrow().iter().filter(|(c, _)| *c == code).count()
        }
    }

    impl EditorRpc for Nvim {
        fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending {
            let value = self.reply(code, &args);
            self.sent.borrow_mut().push((code, args));
            let (answer, pending) = Pending::pair();
            if self.hold.get() {
                self.held.borrow_mut().push_back((answer, value));
            } else {
                answer.send(Ok(value));
            }
            pending
        }

        fn target(&self) -> Option<u64> {
            Some(self.serial.get())
        }
    }

    const PEER_2: PeerId = PeerId(2);
    const TWO: Window = Window::Linked(PEER_2);
    const C1: PeerId = PeerId(16);
    const C2: PeerId = PeerId(17);

    /// What a tick left for window 2.
    fn for_two(routed: &Routed) -> &[LinkedEvent] {
        routed.linked.get(&PEER_2).map_or(&[], Vec::as_slice)
    }

    fn meta_of(session: &str) -> ShowMeta {
        ShowMeta {
            session: session.to_owned(),
            ..meta()
        }
    }

    fn show(path: &str, session: &str) -> OverlayCall {
        OverlayCall::Show {
            path: PathBuf::from(path),
            meta: meta_of(session),
            hunks: vec![hunk()],
        }
    }

    fn revert_of(path: &str, session: &str) -> Value {
        event_for(path, &meta_of(session), &hunk())
    }

    fn unrevert_of(path: &str, session: &str) -> Value {
        with(&revert_of(path, session), "kind", Value::from("unrevert"))
    }

    /// Ticks with a clock a second apart, so a drain is due each time, and gathers what each tick
    /// found.
    fn ticks(overlay: &mut EditorOverlay, nvim: &Nvim, at: &mut Instant) -> Routed {
        let mut all = Routed::default();
        for _ in 0..8 {
            *at += Duration::from_secs(1);
            let routed = overlay.tick(nvim, *at);
            all.mine.extend(routed.mine);
            for (peer, events) in routed.linked {
                all.linked.entry(peer).or_default().extend(events);
            }
        }
        all
    }

    /// The events window 2 got for `session`, read back as window 2 reads them.
    fn window2_events(routed: &Routed, session: &str) -> Vec<(EventKind, EditorRevertEvent)> {
        for_two(routed)
            .iter()
            .filter_map(|event| match event {
                LinkedEvent::Events { session: s, events } if s == session => Some(events),
                _ => None,
            })
            .flatten()
            .map(|value| editor_event_from_wire(value).expect("an event window 2 can read"))
            .collect()
    }

    fn sessions_of(outcomes: &[OverlayOutcome]) -> Vec<(&'static str, String)> {
        outcomes
            .iter()
            .flat_map(|outcome| match outcome {
                OverlayOutcome::Reverts(events) => events.iter().map(|e| ("revert", e.meta.session.clone())).collect(),
                OverlayOutcome::Unreverts(events) => {
                    events.iter().map(|e| ("unrevert", e.meta.session.clone())).collect()
                }
                _ => Vec::new(),
            })
            .collect()
    }

    #[test]
    fn a_new_session_serial_reinstalls_the_review_module() {
        let nvim = Nvim::new();
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        overlay.call(&nvim, show("/p/a", "main:1"), at);
        ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(nvim.count(REVIEW_LUA), 1);
        let at_call = nvim.calls().len();
        nvim.serial.set(2);
        overlay.call(&nvim, show("/p/b", "main:2"), at);
        ticks(&mut overlay, &nvim, &mut at);
        let calls = nvim.calls();
        assert_eq!(calls[at_call], (REVIEW_LUA, None));
        assert_eq!(calls[at_call + 1], (SHOW_LUA, Some("/p/b".into())));
        assert_eq!(nvim.count(REVIEW_LUA), 2);
    }

    #[test]
    fn window_2s_show_on_an_nvim_where_window_1_drew_nothing_installs_once_and_its_reverts_are_drained() {
        let nvim = Nvim::new();
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        overlay.call_for(&nvim, show("/p/b", "s2"), TWO, at);
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(
            nvim.count(REVIEW_LUA),
            1,
            "the module is installed before window 2's first call"
        );
        assert_eq!(nvim.calls()[1], (SHOW_LUA, Some("/p/b".to_owned())));
        assert_eq!(
            for_two(&routed),
            vec![LinkedEvent::Drawn {
                op_id: None,
                path: PathBuf::from("/p/b"),
                state: Drawn::Drawn,
            }]
        );
        assert!(overlay.active_paths().is_empty(), "window 1 drew nothing");
        assert!(overlay.linked_paths(PEER_2).contains(Path::new("/p/b")));
        assert!(overlay.wants_ticks(), "window 2's overlay alone keeps the drain going");

        nvim.events.borrow_mut().push(revert_of("/p/b", "s2"));
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert!(
            routed.mine.is_empty(),
            "nothing of window 2's reaches window 1: {:?}",
            routed.mine
        );
        let events = window2_events(&routed, "s2");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, EventKind::Revert);
        assert_eq!(events[0].1, parse_revert_event(&revert_of("/p/b", "s2")).unwrap());
        assert_eq!(nvim.count(REVIEW_LUA), 1, "installed once");
    }

    #[test]
    fn an_event_for_a_window_2_session_never_reaches_window_1s_flow() {
        let nvim = Nvim::new();
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        overlay.call(&nvim, show("/p/a", "s1"), at);
        overlay.call_for(&nvim, show("/p/b", "s2"), TWO, at);
        ticks(&mut overlay, &nvim, &mut at);
        nvim.events.borrow_mut().extend([
            revert_of("/p/b", "s2"),
            revert_of("/p/a", "s1"),
            unrevert_of("/p/b", "s2"),
            unrevert_of("/p/a", "s1"),
        ]);
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(
            sessions_of(&routed.mine),
            vec![("revert", "s1".to_owned()), ("unrevert", "s1".to_owned())],
            "window 1 gets its own session's events only, in order"
        );
        let kinds: Vec<EventKind> = window2_events(&routed, "s2")
            .into_iter()
            .map(|(kind, _)| kind)
            .collect();
        assert_eq!(kinds, vec![EventKind::Revert, EventKind::Unrevert]);
        assert!(window2_events(&routed, "s1").is_empty());
    }

    #[test]
    fn window_2s_calls_and_window_1s_keep_one_call_in_flight_in_order() {
        let nvim = Nvim::new();
        nvim.hold.set(true);
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        overlay.call(
            &nvim,
            OverlayCall::Clear {
                path: PathBuf::from("/p/a"),
            },
            at,
        );
        overlay.call_for(&nvim, show("/p/b", "s2"), TWO, at);
        overlay.call(&nvim, show("/p/c", "s1"), at);
        overlay.call_for(
            &nvim,
            OverlayCall::OpenAndShow {
                request_id: linked_request_id(PEER_2, 7),
                path: PathBuf::from("/p/d"),
                line: Some(3),
                meta: meta_of("s2"),
                hunks: Some(vec![hunk()]),
            },
            TWO,
            at,
        );
        let mut all = Routed::default();
        for _ in 0..12 {
            assert!(nvim.calls_in_flight() <= 1, "one call in flight: {:?}", nvim.calls());
            nvim.release();
            at += Duration::from_secs(1);
            let routed = overlay.tick(&nvim, at);
            all.mine.extend(routed.mine);
            for (peer, events) in routed.linked {
                all.linked.entry(peer).or_default().extend(events);
            }
        }
        assert_eq!(
            nvim.calls(),
            vec![
                (REVIEW_LUA, None),
                (CLEAR_LUA, Some("/p/a".to_owned())),
                (SHOW_LUA, Some("/p/b".to_owned())),
                (SHOW_LUA, Some("/p/c".to_owned())),
                (OPEN_AND_SHOW_LUA, Some("/p/d".to_owned())),
            ],
            "both windows' calls, in the order they came"
        );
        assert!(
            for_two(&all).contains(&LinkedEvent::Answered {
                op_id: 7,
                result: Ok("opened".to_owned()),
            }),
            "window 2's open is answered under its own id: {:?}",
            for_two(&all)
        );
        assert!(for_two(&all).contains(&LinkedEvent::Drawn {
            op_id: Some(7),
            path: PathBuf::from("/p/d"),
            state: Drawn::Drawn,
        }));
        assert!(
            !all.mine.iter().any(|o| matches!(o, OverlayOutcome::Answered { .. })),
            "window 1 is answered nothing it did not ask: {:?}",
            all.mine
        );
        assert_eq!(
            overlay.active_paths().iter().collect::<Vec<_>>(),
            vec![Path::new("/p/c")]
        );
    }

    #[test]
    fn review_clear_mine_clears_only_window_2s_paths() {
        let nvim = Nvim::new();
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        overlay.call(&nvim, show("/p/a", "s1"), at);
        overlay.call_for(&nvim, show("/p/b", "s2"), TWO, at);
        overlay.call_for(&nvim, show("/p/c", "s2"), TWO, at);
        ticks(&mut overlay, &nvim, &mut at);
        overlay.call_for(&nvim, OverlayCall::ClearAll, TWO, at);
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(
            nvim.count(CLEAR_ALL_LUA),
            0,
            "never the clear that takes window 1's too"
        );
        let cleared: Vec<_> = nvim
            .calls()
            .into_iter()
            .filter(|(code, _)| *code == CLEAR_LUA)
            .collect();
        assert_eq!(
            cleared,
            vec![
                (CLEAR_LUA, Some("/p/b".to_owned())),
                (CLEAR_LUA, Some("/p/c".to_owned()))
            ]
        );
        assert!(overlay.active_paths().contains(Path::new("/p/a")));
        assert!(overlay.linked_paths(PEER_2).is_empty());
        assert!(
            nvim.drawn.borrow().contains(b"/p/a".as_slice()),
            "window 1's overlay stays"
        );
        for path in ["/p/b", "/p/c"] {
            assert!(for_two(&routed).contains(&LinkedEvent::Drawn {
                op_id: None,
                path: PathBuf::from(path),
                state: Drawn::Cleared,
            }));
        }
    }

    #[test]
    fn a_draw_by_one_window_over_the_others_path_moves_the_path() {
        let nvim = Nvim::new();
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        overlay.call_for(&nvim, show("/p/a", "s2"), TWO, at);
        ticks(&mut overlay, &nvim, &mut at);
        assert!(overlay.linked_paths(PEER_2).contains(Path::new("/p/a")));

        overlay.call(&nvim, show("/p/a", "s1"), at);
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert!(overlay.active_paths().contains(Path::new("/p/a")));
        assert!(overlay.linked_paths(PEER_2).is_empty());
        assert_eq!(
            for_two(&routed),
            vec![LinkedEvent::Drawn {
                op_id: None,
                path: PathBuf::from("/p/a"),
                state: Drawn::Off,
            }],
            "window 2 is told its overlay there is gone"
        );

        overlay.call_for(&nvim, show("/p/a", "s2"), TWO, at);
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert!(overlay.linked_paths(PEER_2).contains(Path::new("/p/a")));
        assert!(overlay.active_paths().is_empty());
        assert_eq!(
            routed.mine,
            vec![OverlayOutcome::Off(vec![OffEvent {
                path: "/p/a".to_owned(),
                why: "replaced".to_owned(),
            }])],
            "and window 1 the other way round"
        );
    }

    #[test]
    fn a_revert_drained_during_a_moves_freeze_reaches_the_destination_and_on_an_abort_the_source() {
        let nvim = Nvim::new();
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        overlay.call(&nvim, show("/p/a", "moving"), at);
        overlay.call(&nvim, show("/p/b", "staying"), at);
        ticks(&mut overlay, &nvim, &mut at);

        // Both sessions are offered for a move; their events wait while the moves are undecided.
        overlay.hold_session("moving");
        overlay.hold_session("staying");
        nvim.events.borrow_mut().extend([
            revert_of("/p/a", "moving"),
            revert_of("/p/b", "staying"),
            unrevert_of("/p/a", "moving"),
        ]);
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert!(nvim.events.borrow().is_empty(), "the events were drained");
        assert!(routed.mine.is_empty() && for_two(&routed).is_empty(), "{routed:?}");

        // One move is done: the destination gets its events, in order.
        overlay.release_session("moving", TWO);
        let routed = ticks(&mut overlay, &nvim, &mut at);
        let kinds: Vec<EventKind> = window2_events(&routed, "moving").into_iter().map(|(k, _)| k).collect();
        assert_eq!(kinds, vec![EventKind::Revert, EventKind::Unrevert]);
        assert!(routed.mine.is_empty());
        assert_eq!(overlay.holder("moving"), TWO);

        // The other aborts: the source gets its event back.
        overlay.release_session("staying", Window::One);
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(sessions_of(&routed.mine), vec![("revert", "staying".to_owned())]);
        assert!(for_two(&routed).is_empty());

        // After the move, the session's events follow it.
        nvim.events.borrow_mut().push(revert_of("/p/a", "moving"));
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(window2_events(&routed, "moving").len(), 1);
        assert!(routed.mine.is_empty());
    }

    #[test]
    fn window_2_gone_clears_its_paths() {
        let nvim = Nvim::new();
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        overlay.call(&nvim, show("/p/a", "s1"), at);
        overlay.call_for(&nvim, show("/p/b", "s2"), TWO, at);
        overlay.call_for(&nvim, show("/p/c", "s2"), TWO, at);
        ticks(&mut overlay, &nvim, &mut at);
        overlay.linked_gone(PEER_2);
        assert!(overlay.wants_ticks());
        let routed = ticks(&mut overlay, &nvim, &mut at);
        let cleared: Vec<_> = nvim
            .calls()
            .into_iter()
            .filter(|(code, _)| *code == CLEAR_LUA)
            .collect();
        assert_eq!(
            cleared,
            vec![
                (CLEAR_LUA, Some("/p/b".to_owned())),
                (CLEAR_LUA, Some("/p/c".to_owned()))
            ]
        );
        assert_eq!(nvim.drawn.borrow().iter().collect::<Vec<_>>(), vec![&b"/p/a".to_vec()]);
        assert!(overlay.active_paths().contains(Path::new("/p/a")));
        assert!(
            for_two(&routed).is_empty(),
            "nothing more goes to a window that is gone"
        );
    }

    #[test]
    fn a_clear_of_window_2s_after_window_1_drew_the_file_leaves_window_1s_overlay() {
        let nvim = Nvim::new();
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        overlay.call_for(&nvim, show("/p/a", "s2"), TWO, at);
        ticks(&mut overlay, &nvim, &mut at);
        assert!(overlay.linked_paths(PEER_2).contains(Path::new("/p/a")));

        // Window 1 draws the file; while its answer is out, window 2 clears the file, alone and as
        // part of clearing all of its own.
        nvim.hold.set(true);
        overlay.call(&nvim, show("/p/a", "s1"), at);
        overlay.call_for(
            &nvim,
            OverlayCall::Clear {
                path: PathBuf::from("/p/a"),
            },
            TWO,
            at,
        );
        overlay.call_for(&nvim, OverlayCall::ClearAll, TWO, at);
        nvim.hold.set(false);
        nvim.release();
        let routed = ticks(&mut overlay, &nvim, &mut at);

        assert_eq!(nvim.count(CLEAR_LUA), 0, "window 2 holds nothing to clear by then");
        assert!(
            nvim.drawn.borrow().contains(b"/p/a".as_slice()),
            "window 1's overlay stays"
        );
        assert!(overlay.active_paths().contains(Path::new("/p/a")));
        assert!(overlay.linked_paths(PEER_2).is_empty());
        assert!(
            !routed.mine.iter().any(|o| matches!(o, OverlayOutcome::Off(_))),
            "window 1 is told nothing went: {routed:?}"
        );
        assert!(for_two(&routed).contains(&LinkedEvent::Drawn {
            op_id: None,
            path: PathBuf::from("/p/a"),
            state: Drawn::Off,
        }));
    }

    #[test]
    fn window_2_gone_leaves_a_file_window_1_drew_meanwhile() {
        let nvim = Nvim::new();
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        overlay.call_for(&nvim, show("/p/a", "s2"), TWO, at);
        overlay.call_for(&nvim, show("/p/b", "s2"), TWO, at);
        ticks(&mut overlay, &nvim, &mut at);

        nvim.hold.set(true);
        overlay.call(&nvim, show("/p/a", "s1"), at);
        overlay.linked_gone(PEER_2);
        nvim.hold.set(false);
        nvim.release();
        let routed = ticks(&mut overlay, &nvim, &mut at);

        let cleared: Vec<_> = nvim
            .calls()
            .into_iter()
            .filter(|(code, _)| *code == CLEAR_LUA)
            .collect();
        assert_eq!(cleared, vec![(CLEAR_LUA, Some("/p/b".to_owned()))]);
        assert_eq!(nvim.drawn.borrow().iter().collect::<Vec<_>>(), vec![&b"/p/a".to_vec()]);
        assert!(overlay.active_paths().contains(Path::new("/p/a")));
        assert!(overlay.linked_paths(PEER_2).is_empty());
        assert!(
            !routed.mine.iter().any(|o| matches!(o, OverlayOutcome::Off(_))),
            "{routed:?}"
        );
        assert!(
            for_two(&routed).is_empty(),
            "nothing more goes to a window that is gone"
        );
    }

    fn for_peer(routed: &Routed, peer: PeerId) -> &[LinkedEvent] {
        routed.linked.get(&peer).map_or(&[], Vec::as_slice)
    }

    #[test]
    fn one_linked_window_going_away_clears_only_what_it_drew() {
        let nvim = Nvim::new();
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        overlay.call_for(&nvim, show("/p/a", "s1"), Window::Linked(C1), at);
        overlay.call_for(&nvim, show("/p/b", "s2"), Window::Linked(C2), at);
        ticks(&mut overlay, &nvim, &mut at);
        overlay.linked_gone(C1);
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(nvim.drawn.borrow().iter().collect::<Vec<_>>(), vec![&b"/p/b".to_vec()]);
        assert!(overlay.linked_paths(C1).is_empty());
        assert!(overlay.linked_paths(C2).contains(Path::new("/p/b")));
        assert!(routed.linked.is_empty(), "{routed:?}");
    }

    #[test]
    fn a_window_drawing_over_another_linked_windows_file_tells_that_window() {
        let nvim = Nvim::new();
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        overlay.call_for(&nvim, show("/p/a", "s1"), Window::Linked(C1), at);
        ticks(&mut overlay, &nvim, &mut at);
        overlay.call_for(&nvim, show("/p/a", "s2"), Window::Linked(C2), at);
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(
            for_peer(&routed, C1),
            [LinkedEvent::Drawn {
                op_id: None,
                path: PathBuf::from("/p/a"),
                state: Drawn::Off,
            }]
        );
        assert!(overlay.linked_paths(C1).is_empty());
        assert!(overlay.linked_paths(C2).contains(Path::new("/p/a")));
    }

    #[test]
    fn a_review_clear_of_one_window_leaves_another_windows_file_alone() {
        let nvim = Nvim::new();
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        overlay.call_for(&nvim, show("/p/a", "s1"), Window::Linked(C1), at);
        overlay.call_for(&nvim, show("/p/b", "s2"), Window::Linked(C2), at);
        ticks(&mut overlay, &nvim, &mut at);
        overlay.call_for(&nvim, OverlayCall::ClearAll, Window::Linked(C1), at);
        ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(nvim.drawn.borrow().iter().collect::<Vec<_>>(), vec![&b"/p/b".to_vec()]);
    }

    #[test]
    fn an_answer_goes_to_the_window_whose_request_id_it_carries() {
        let nvim = Nvim::new();
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        for (peer, op_id) in [(C1, 7), (C2, 7)] {
            overlay.call_for(
                &nvim,
                OverlayCall::OpenAndShow {
                    request_id: linked_request_id(peer, op_id),
                    path: PathBuf::from(format!("/p/{}", peer.0)),
                    line: None,
                    meta: meta_of("s"),
                    hunks: None,
                },
                Window::Linked(peer),
                at,
            );
        }
        let routed = ticks(&mut overlay, &nvim, &mut at);
        for peer in [C1, C2] {
            assert!(
                for_peer(&routed, peer)
                    .iter()
                    .any(|event| matches!(event, LinkedEvent::Answered { op_id: 7, .. })),
                "{routed:?}"
            );
        }
        assert_eq!(linked_request_id(PEER_2, 3), "w2:3", "window 2's opens keep their ids");
    }

    #[test]
    fn window_2s_opens_still_waiting_when_the_editor_goes_are_answered() {
        let nvim = Nvim::new();
        nvim.hold.set(true);
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let at = Instant::now();
        overlay.call_for(
            &nvim,
            OverlayCall::OpenAndShow {
                request_id: linked_request_id(PEER_2, 4),
                path: PathBuf::from("/p/d"),
                line: None,
                meta: meta_of("s2"),
                hunks: None,
            },
            TWO,
            at,
        );
        overlay.editor_lost();
        let routed = overlay.tick(&nvim, at);
        assert!(matches!(
            for_two(&routed),
            [LinkedEvent::Answered {
                op_id: 4,
                result: Err(_)
            }]
        ));
    }

    #[test]
    fn an_editor_event_crosses_the_link_unchanged() {
        let event = parse_revert_event(&revert_of("/p/it's a.rs", "s-9")).unwrap();
        for kind in [EventKind::Revert, EventKind::Unrevert] {
            let wire = editor_event_to_wire(kind, &event);
            assert_eq!(editor_event_from_wire(&wire).unwrap(), (kind, event.clone()));
        }
        let mut short = editor_event_to_wire(EventKind::Revert, &event);
        short["hunk"]["old_len"] = serde_json::json!(5);
        assert!(editor_event_from_wire(&short).is_err());
        let mut extra = editor_event_to_wire(EventKind::Revert, &event);
        extra["lua"] = serde_json::json!("os.exit()");
        assert!(editor_event_from_wire(&extra).is_err());
    }

    #[test]
    fn remote_overlay_reports_a_path_drawn_after_review_drawn() {
        let sent: Rc<RefCell<Vec<EditorOp>>> = Rc::default();
        let log = sent.clone();
        let mut remote = RemoteOverlay::new(
            Path::new("/p"),
            Box::new(move |op| {
                log.borrow_mut().push(op);
                log.borrow().len() as u64
            }),
        );
        let at = Instant::now();
        remote.call(show("/p/src/a.rs", "s2"), at);
        assert_eq!(
            sent.borrow()[0],
            EditorOp::ReviewShow {
                path: "src/a.rs".to_owned(),
                meta: WireMeta::from(&meta_of("s2")),
                hunks: vec![WireHunk::from(&hunk())],
            }
        );
        assert!(remote.active_paths().is_empty(), "queued is not drawn");
        remote.review_drawn("src/a.rs", Drawn::Drawn);
        assert!(remote.active_paths().contains(Path::new("/p/src/a.rs")));
        remote.review_drawn("src/a.rs", Drawn::Off);
        assert!(remote.active_paths().is_empty());
        assert!(matches!(remote.tick(at).as_slice(), [OverlayOutcome::Off(offs)] if offs[0].path == "/p/src/a.rs"));
        remote.review_drawn("src/b.rs", Drawn::Kept);
        remote.review_drawn("../outside", Drawn::Drawn);
        assert_eq!(remote.active_paths().len(), 1);
        remote.review_idle();
        assert!(remote.active_paths().is_empty());

        // An open is answered when window 1 answers its operation.
        remote.call(
            OverlayCall::OpenAndShow {
                request_id: "r1".to_owned(),
                path: PathBuf::from("/p/src/a.rs"),
                line: Some(2),
                meta: meta_of("s2"),
                hunks: None,
            },
            at,
        );
        assert!(
            matches!(&sent.borrow()[1], EditorOp::ReviewOpenAndShow { path, line: Some(2), hunks: None, .. } if path == "src/a.rs")
        );
        assert!(!remote.wants_ticks());
        remote.answered(2, Ok("opened".to_owned()));
        assert_eq!(
            remote.tick(at),
            vec![OverlayOutcome::Answered {
                request_id: "r1".to_owned(),
                result: Ok("opened".to_owned()),
            }]
        );

        // Clearing everything asks for window 2's own overlays only; a file outside the project is
        // never named.
        remote.call(OverlayCall::ClearAll, at);
        assert_eq!(sent.borrow()[2], EditorOp::ReviewClearMine {});
        remote.call(
            OverlayCall::OpenAndShow {
                request_id: "r2".to_owned(),
                path: PathBuf::from("/elsewhere/x"),
                line: None,
                meta: meta_of("s2"),
                hunks: None,
            },
            at,
        );
        assert_eq!(sent.borrow().len(), 3);
        assert!(matches!(
            remote.tick(at).as_slice(),
            [OverlayOutcome::Answered { result: Err(_), .. }]
        ));

        // Events window 1 routed here come back as the flow takes them.
        let wire = editor_event_to_wire(
            EventKind::Revert,
            &parse_revert_event(&revert_of("/p/src/a.rs", "s2")).unwrap(),
        );
        remote.review_events("s2", vec![wire.clone()]);
        remote.review_events("other", vec![wire]);
        assert!(matches!(remote.tick(at).as_slice(), [OverlayOutcome::Reverts(events)] if events.len() == 1));
    }

    // ---- live drawings -----------------------------------------------------------------------------

    fn live_meta(session: &str, turn: u32) -> LiveMeta {
        LiveMeta {
            tab: 7,
            session: session.to_owned(),
            turn_id: format!("t{turn}"),
            turn,
            running: true,
            read_lines: 10,
        }
    }

    fn live_show(path: &str, session: &str, turn: u32) -> OverlayCall {
        OverlayCall::LiveShow {
            path: PathBuf::from(path),
            meta: live_meta(session, turn),
            hunks: vec![hunk()],
        }
    }

    fn live_clear(path: &str) -> OverlayCall {
        OverlayCall::LiveClear {
            path: PathBuf::from(path),
        }
    }

    fn key(session: &str, turn_id: &str) -> TurnKey {
        TurnKey {
            session: session.to_owned(),
            turn_id: turn_id.to_owned(),
        }
    }

    fn paths(list: &[&str]) -> Vec<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    fn bytes_of(path: &str) -> Vec<u8> {
        path.as_bytes().to_vec()
    }

    /// The paths of every call with `code`, in the order they were sent.
    fn sent_paths(nvim: &Nvim, code: &str) -> Vec<String> {
        nvim.calls()
            .into_iter()
            .filter(|(c, _)| *c == code)
            .filter_map(|(_, path)| path)
            .collect()
    }

    fn live_drawn(outcomes: &[OverlayOutcome]) -> Vec<(PathBuf, u32, u32, bool)> {
        outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                OverlayOutcome::LiveDrawn {
                    path,
                    meta,
                    drawn,
                    left,
                } => Some((path.clone(), meta.turn, *drawn, *left)),
                _ => None,
            })
            .collect()
    }

    fn has_off(outcomes: &[OverlayOutcome]) -> bool {
        outcomes.iter().any(|o| matches!(o, OverlayOutcome::Off(_)))
    }

    fn drawings(overlay: &EditorOverlay) -> Vec<(&str, (&str, u32))> {
        overlay
            .live_drawings()
            .iter()
            .map(|(path, (session, turn))| (path.to_str().unwrap(), (session.as_str(), *turn)))
            .collect()
    }

    #[test]
    fn a_live_show_goes_out_behind_queued_calls() {
        let nvim = Nvim::new();
        nvim.hold.set(true);
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        overlay.call(
            &nvim,
            OverlayCall::Clear {
                path: PathBuf::from("/p/a"),
            },
            at,
        );
        overlay.call_for(&nvim, show("/p/b", "s2"), TWO, at);
        overlay.call(&nvim, live_show("/p/c", "s1", 3), at);
        overlay.call(&nvim, show("/p/d", "s1"), at);
        for _ in 0..12 {
            assert!(nvim.calls_in_flight() <= 1, "one call in flight: {:?}", nvim.calls());
            nvim.release();
            at += Duration::from_secs(1);
            overlay.tick(&nvim, at);
        }
        assert_eq!(
            nvim.calls(),
            vec![
                (REVIEW_LUA, None),
                (CLEAR_LUA, Some("/p/a".to_owned())),
                (SHOW_LUA, Some("/p/b".to_owned())),
                (LIVE_SHOW_LUA, Some("/p/c".to_owned())),
                (SHOW_LUA, Some("/p/d".to_owned())),
            ],
            "the live draw waits its turn like any other call"
        );
        assert_eq!(drawings(&overlay), vec![("/p/c", ("s1", 3))]);
    }

    #[test]
    fn a_contested_live_show_is_not_sent() {
        // Two turns running at once both edit /p/a; only /p/b is theirs alone.
        let nvim = Nvim::new();
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        overlay.note_live(Window::One, &key("s1", "t1"), 10, None, &paths(&["/p/a", "/p/b"]));
        overlay.note_live(TWO, &key("s2", "t9"), 20, None, &paths(&["/p/a"]));
        overlay.call(&nvim, live_show("/p/a", "s1", 1), at);
        overlay.call_for(&nvim, live_show("/p/a", "s2", 9), TWO, at);
        overlay.call(&nvim, live_show("/p/b", "s1", 1), at);
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(sent_paths(&nvim, LIVE_SHOW_LUA), vec!["/p/b".to_owned()]);
        assert_eq!(nvim.drawn.borrow().iter().collect::<Vec<_>>(), vec![&bytes_of("/p/b")]);
        assert_eq!(drawings(&overlay), vec![("/p/b", ("s1", 1))]);
        assert!(for_two(&routed).is_empty(), "{routed:?}");

        // A contested draw alone installs nothing.
        let quiet = Nvim::new();
        let mut alone = EditorOverlay::new(Owner::Embedded);
        alone.note_live(Window::One, &key("s1", "t1"), 10, None, &paths(&["/p/a"]));
        alone.note_live(Window::One, &key("s2", "t2"), 20, None, &paths(&["/p/a"]));
        alone.call(&quiet, live_show("/p/a", "s1", 1), at);
        ticks(&mut alone, &quiet, &mut at);
        assert!(quiet.sent.borrow().is_empty(), "{:?}", quiet.calls());

        // Both turns end; a later turn that edits the file alone draws it again.
        overlay.note_live(Window::One, &key("s1", "t1"), 10, Some(30), &paths(&["/p/a", "/p/b"]));
        overlay.note_live(TWO, &key("s2", "t9"), 20, Some(40), &paths(&["/p/a"]));
        overlay.call(&nvim, live_show("/p/a", "s1", 1), at);
        ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(
            sent_paths(&nvim, LIVE_SHOW_LUA).len(),
            1,
            "still contested after the ends"
        );
        overlay.note_live(Window::One, &key("s1", "t2"), 50, None, &paths(&["/p/a"]));
        overlay.call(&nvim, live_show("/p/a", "s1", 2), at);
        ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(
            sent_paths(&nvim, LIVE_SHOW_LUA),
            vec!["/p/b".to_owned(), "/p/a".to_owned()]
        );
        assert_eq!(drawings(&overlay), vec![("/p/a", ("s1", 2)), ("/p/b", ("s1", 1))]);
    }

    #[test]
    fn a_note_that_contests_a_drawn_path_clears_it() {
        let nvim = Nvim::new();
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        overlay.note_live(Window::One, &key("s1", "t1"), 10, None, &paths(&["/p/a", "/p/c"]));
        overlay.note_live(TWO, &key("s2", "t2"), 15, None, &paths(&["/p/b"]));
        overlay.call(&nvim, live_show("/p/a", "s1", 1), at);
        overlay.call_for(&nvim, live_show("/p/b", "s2", 2), TWO, at);
        overlay.call(&nvim, show("/p/o", "s1"), at);
        ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(drawings(&overlay), vec![("/p/a", ("s1", 1)), ("/p/b", ("s2", 2))]);

        // /p/c's live draw is still out when a third turn's note contests all three files.
        nvim.hold.set(true);
        overlay.call(&nvim, live_show("/p/c", "s1", 1), at);
        assert_eq!(nvim.calls_in_flight(), 1);
        overlay.note_live(
            Window::One,
            &key("s3", "t3"),
            20,
            None,
            &paths(&["/p/a", "/p/b", "/p/c", "/p/o"]),
        );
        nvim.hold.set(false);
        nvim.release();
        let routed = ticks(&mut overlay, &nvim, &mut at);

        let mut cleared = sent_paths(&nvim, LIVE_CLEAR_LUA);
        cleared.sort();
        assert_eq!(cleared, vec!["/p/a".to_owned(), "/p/b".to_owned(), "/p/c".to_owned()]);
        assert_eq!(
            nvim.drawn.borrow().iter().collect::<Vec<_>>(),
            vec![&bytes_of("/p/o")],
            "only the drawing that was not live stays"
        );
        assert!(overlay.live_drawings().is_empty(), "{:?}", overlay.live_drawings());
        assert_eq!(
            overlay.active_paths().iter().collect::<Vec<_>>(),
            vec![Path::new("/p/o")]
        );
        assert!(overlay.linked_paths(PEER_2).is_empty());
        assert!(for_two(&routed).contains(&LinkedEvent::Drawn {
            op_id: None,
            path: PathBuf::from("/p/b"),
            state: Drawn::Cleared,
        }));
        assert!(!has_off(&routed.mine), "a contested clear says nothing: {routed:?}");
    }

    #[test]
    fn a_turn_noted_running_after_its_end_contests_its_paths_again() {
        let nvim = Nvim::new();
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        // t1's tab went for a move, so its end was noted when it went.
        overlay.note_live(Window::One, &key("s1", "t1"), 10, None, &paths(&["/p/a"]));
        overlay.note_live(Window::One, &key("s1", "t1"), 10, Some(30), &[]);
        // A turn that started after that draws the same file.
        overlay.note_live(Window::One, &key("s2", "t2"), 40, None, &paths(&["/p/a"]));
        overlay.call(&nvim, live_show("/p/a", "s2", 2), at);
        ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(drawings(&overlay), vec![("/p/a", ("s2", 2))]);

        // The move was cancelled: t1 runs on and edits another file, and both turns now run.
        overlay.note_live(Window::One, &key("s1", "t1"), 10, None, &paths(&["/p/b"]));
        ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(sent_paths(&nvim, LIVE_CLEAR_LUA), vec!["/p/a".to_owned()]);
        assert!(overlay.live_drawings().is_empty(), "{:?}", overlay.live_drawings());
    }

    #[test]
    fn a_left_answer_keeps_the_owner_of_the_file() {
        let nvim = Nvim::new();
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        // Window 2 asked for a review over /p/a; window 1's live draw finds it there.
        overlay.call_for(&nvim, show("/p/a", "s2"), TWO, at);
        ticks(&mut overlay, &nvim, &mut at);
        overlay.call(&nvim, live_show("/p/a", "s1", 1), at);
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert!(overlay.linked_paths(PEER_2).contains(Path::new("/p/a")));
        assert!(overlay.active_paths().is_empty());
        assert!(overlay.live_drawings().is_empty());
        assert!(for_two(&routed).is_empty(), "window 2 keeps its drawing: {routed:?}");
        assert!(nvim.drawn.borrow().contains(&bytes_of("/p/a")) && nvim.live.borrow().is_empty());

        // Window 1's own live drawing over a buffer the person has since modified stays as it was.
        overlay.call(&nvim, live_show("/p/b", "s1", 1), at);
        ticks(&mut overlay, &nvim, &mut at);
        nvim.modified.borrow_mut().insert(bytes_of("/p/b"));
        overlay.call(&nvim, live_show("/p/b", "s1", 2), at);
        ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(drawings(&overlay), vec![("/p/b", ("s1", 1))]);
        assert!(overlay.active_paths().contains(Path::new("/p/b")));
    }

    #[test]
    fn live_clear_never_clears_an_o_drawing() {
        for clear_first in [false, true] {
            let nvim = Nvim::new();
            let mut overlay = EditorOverlay::new(Owner::Embedded);
            let mut at = Instant::now();
            overlay.call(&nvim, live_show("/p/a", "s1", 1), at);
            ticks(&mut overlay, &nvim, &mut at);
            nvim.hold.set(true);
            if clear_first {
                overlay.call(&nvim, live_clear("/p/a"), at);
                overlay.call(&nvim, show("/p/a", "s1"), at);
            } else {
                overlay.call(&nvim, show("/p/a", "s1"), at);
                overlay.call(&nvim, live_clear("/p/a"), at);
            }
            nvim.hold.set(false);
            nvim.release();
            let routed = ticks(&mut overlay, &nvim, &mut at);
            assert!(
                nvim.drawn.borrow().contains(&bytes_of("/p/a")),
                "clear first: {clear_first}"
            );
            assert!(nvim.live.borrow().is_empty(), "clear first: {clear_first}");
            assert!(
                overlay.active_paths().contains(Path::new("/p/a")),
                "clear first: {clear_first}"
            );
            assert!(overlay.live_drawings().is_empty(), "clear first: {clear_first}");
            assert!(!has_off(&routed.mine), "clear first: {clear_first}: {routed:?}");
        }
    }

    #[test]
    fn a_live_show_sets_the_sessions_holder() {
        let nvim = Nvim::new();
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        overlay.call_for(&nvim, live_show("/p/a", "s2", 1), TWO, at);
        ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(overlay.holder("s2"), TWO);
        nvim.events.borrow_mut().push(revert_of("/p/a", "s2"));
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(window2_events(&routed, "s2").len(), 1);
        assert!(sessions_of(&routed.mine).is_empty(), "{routed:?}");
    }

    #[test]
    fn live_drawn_is_reported_with_left() {
        let nvim = Nvim::new();
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        overlay.call(&nvim, live_show("/p/a", "s1", 1), at);
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(live_drawn(&routed.mine), vec![(PathBuf::from("/p/a"), 1, 1, false)]);
        assert!(routed.mine.contains(&OverlayOutcome::LiveDrawn {
            path: PathBuf::from("/p/a"),
            meta: live_meta("s1", 1),
            drawn: 1,
            left: false,
        }));

        nvim.modified.borrow_mut().insert(bytes_of("/p/a"));
        overlay.call(&nvim, live_show("/p/a", "s1", 2), at);
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(live_drawn(&routed.mine), vec![(PathBuf::from("/p/a"), 2, 0, true)]);

        nvim.hidden.borrow_mut().insert(bytes_of("/p/b"));
        overlay.call(&nvim, live_show("/p/b", "s1", 2), at);
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(live_drawn(&routed.mine), vec![(PathBuf::from("/p/b"), 2, 0, false)]);
        assert!(!overlay.live_drawings().contains_key(Path::new("/p/b")));

        // Another window's live draw is that window's news.
        overlay.call_for(&nvim, live_show("/p/c", "s2", 1), TWO, at);
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert!(live_drawn(&routed.mine).is_empty(), "{routed:?}");
    }

    /// Ticks every 100 ms for `span` and gives the offsets from the start at which a drain went out.
    fn drains_over(overlay: &mut EditorOverlay, nvim: &Nvim, at: &mut Instant, span: Duration) -> Vec<Duration> {
        let start = *at;
        let mut drains = Vec::new();
        while *at - start < span {
            *at += Duration::from_millis(100);
            let before = nvim.count(TAKE_EVENTS_LUA);
            overlay.tick(nvim, *at);
            if nvim.count(TAKE_EVENTS_LUA) > before {
                drains.push(*at - start);
            }
        }
        drains
    }

    #[test]
    fn the_drain_slows_with_only_idle_live_drawings_and_speeds_up_on_pending() {
        let nvim = Nvim::new();
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        overlay.call(&nvim, live_show("/p/a", "s1", 1), at);
        let drains = drains_over(&mut overlay, &nvim, &mut at, Duration::from_secs(12));
        assert_eq!(drains.len(), 3, "{drains:?}");
        assert!(drains.windows(2).all(|w| w[1] - w[0] >= LIVE_DRAIN_EVERY), "{drains:?}");

        // A revert waits in the module; the next answer says so and the drain comes at the fast rate.
        nvim.events.borrow_mut().push(revert_of("/p/a", "s1"));
        overlay.call(&nvim, live_show("/p/b", "s1", 1), at);
        let drains = drains_over(&mut overlay, &nvim, &mut at, Duration::from_secs(1));
        assert!(!drains.is_empty(), "a pending event is asked for within 500 ms");
        assert!(drains[0] <= DRAIN_EVERY, "{drains:?}");
        // That drain emptied the queue: back to the slow rate.
        let drains = drains_over(&mut overlay, &nvim, &mut at, Duration::from_millis(3500));
        assert!(drains.is_empty(), "{drains:?}");

        // A drawing that is not live (`o`) puts the drain back to 500 ms.
        overlay.call(&nvim, show("/p/c", "s1"), at);
        let drains = drains_over(&mut overlay, &nvim, &mut at, Duration::from_secs(3));
        assert!(drains.len() >= 5, "{drains:?}");
    }

    #[test]
    fn a_missing_module_drops_live_state_quietly() {
        let nvim = Nvim::new();
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        overlay.call(&nvim, live_show("/p/a", "s1", 1), at);
        overlay.call_for(&nvim, live_show("/p/b", "s2", 1), TWO, at);
        ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(overlay.live_drawings().len(), 2);

        nvim.restart();
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert!(overlay.live_drawings().is_empty());
        assert!(overlay.active_paths().is_empty());
        assert!(overlay.linked_paths(PEER_2).is_empty());
        assert!(routed.mine.is_empty(), "nothing is said: {routed:?}");
        assert_eq!(for_two(&routed), vec![LinkedEvent::Idle]);
        assert_eq!(nvim.count(REVIEW_LUA), 1, "nothing asked to put the module back");

        // A clear of what was drawn there neither installs nor goes out.
        overlay.call(&nvim, live_clear("/p/a"), at);
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(nvim.count(REVIEW_LUA), 1);
        assert_eq!(nvim.count(LIVE_CLEAR_LUA), 0);
        assert!(routed.mine.is_empty() && routed.linked.is_empty(), "{routed:?}");

        // A live draw found the module gone in flight: installed again, drawn, and nothing said.
        overlay.call(&nvim, live_show("/p/c", "s1", 2), at);
        ticks(&mut overlay, &nvim, &mut at);
        nvim.restart();
        overlay.call(&nvim, live_show("/p/c", "s1", 3), at);
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(drawings(&overlay), vec![("/p/c", ("s1", 3))]);
        assert!(!has_off(&routed.mine), "{routed:?}");
        assert!(!routed.mine.iter().any(|o| matches!(o, OverlayOutcome::Answered { .. })));

        // A clear queued behind a live draw that found the module gone still takes that drawing off.
        nvim.restart();
        nvim.hold.set(true);
        overlay.call(&nvim, live_show("/p/d", "s1", 3), at);
        overlay.call(&nvim, live_clear("/p/d"), at);
        nvim.hold.set(false);
        nvim.release();
        ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(sent_paths(&nvim, LIVE_CLEAR_LUA), vec!["/p/d".to_owned()]);
        assert!(!nvim.drawn.borrow().contains(&bytes_of("/p/d")));
        assert!(overlay.live_drawings().is_empty(), "{:?}", overlay.live_drawings());
    }

    #[test]
    fn a_non_utf8_live_path_is_skipped_quietly() {
        let nvim = Nvim::new();
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        let bad = PathBuf::from(OsStr::from_bytes(b"/p/\xff.rs"));
        overlay.call_for(
            &nvim,
            OverlayCall::LiveShow {
                path: bad,
                meta: live_meta("s2", 1),
                hunks: vec![hunk()],
            },
            TWO,
            at,
        );
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert!(nvim.sent.borrow().is_empty(), "{:?}", nvim.calls());
        assert_eq!(routed, Routed::default());
        assert_eq!(overlay.holder("s2"), Window::One, "a refused draw holds no session");
        assert!(overlay.live_drawings().is_empty());

        // Spaces and CJK are only bytes of a path.
        overlay.call(&nvim, live_show("/p/a b/文件.rs", "s1", 1), at);
        ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(drawings(&overlay), vec![("/p/a b/文件.rs", ("s1", 1))]);
    }

    #[test]
    fn live_drawings_are_kept_per_path_with_session_and_turn() {
        let nvim = Nvim::new();
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        overlay.call(&nvim, live_show("/p/a", "s1", 3), at);
        overlay.call_for(&nvim, live_show("/p/b", "s2", 5), TWO, at);
        overlay.call(&nvim, live_show("/p/c", "s1", 3), at);
        ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(
            drawings(&overlay),
            vec![("/p/a", ("s1", 3)), ("/p/b", ("s2", 5)), ("/p/c", ("s1", 3))]
        );

        // The same file drawn for a later turn replaces the earlier turn's drawing.
        overlay.call(&nvim, live_show("/p/a", "s1", 4), at);
        // A review asked for over a live drawing is not live.
        overlay.call(&nvim, show("/p/b", "s1"), at);
        ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(drawings(&overlay), vec![("/p/a", ("s1", 4)), ("/p/c", ("s1", 3))]);

        // The module took /p/c's drawing off.
        nvim.drawn.borrow_mut().remove(&bytes_of("/p/c"));
        nvim.live.borrow_mut().remove(&bytes_of("/p/c"));
        nvim.events.borrow_mut().push(map(vec![
            ("kind", Value::from("off")),
            ("path", Value::from("/p/c")),
            ("why", Value::from("user")),
        ]));
        ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(drawings(&overlay), vec![("/p/a", ("s1", 4))]);

        let expected = overlay.live_drawings().clone();
        let local = Overlay::Local(overlay);
        assert_eq!(local.live_drawings(), &expected);
        let remote = Overlay::Remote(RemoteOverlay::new(Path::new("/p"), Box::new(|_: EditorOp| 1)));
        assert!(remote.live_drawings().is_empty());
    }

    #[test]
    fn an_off_queued_before_a_redraw_leaves_the_new_drawing_to_the_contest() {
        let nvim = Nvim::new();
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        overlay.note_live(Window::One, &key("s1", "t1"), 10, None, &paths(&["/p/a"]));
        overlay.call(&nvim, live_show("/p/a", "s1", 1), at);
        ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(drawings(&overlay), vec![("/p/a", ("s1", 1))]);

        // Dismissed, and drawn again by the next edit before the drain took the dismissal.
        nvim.dismiss("/p/a");
        overlay.call(&nvim, live_show("/p/a", "s1", 1), at);
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert!(nvim.events.borrow().is_empty(), "the dismissal was drained");
        assert!(!has_off(&routed.mine), "{routed:?}");
        assert_eq!(drawings(&overlay), vec![("/p/a", ("s1", 1))]);
        assert_eq!(
            overlay.active_paths().iter().collect::<Vec<_>>(),
            vec![Path::new("/p/a")]
        );

        // A second turn contests the file: the drawing still up is cleared.
        overlay.note_live(TWO, &key("s2", "t2"), 15, None, &paths(&["/p/a"]));
        ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(sent_paths(&nvim, LIVE_CLEAR_LUA), vec!["/p/a".to_owned()]);
        assert!(nvim.drawn.borrow().is_empty());
        assert!(overlay.live_drawings().is_empty());

        // A dismissal after the last draw is news.
        overlay.call(&nvim, show("/p/b", "s1"), at);
        ticks(&mut overlay, &nvim, &mut at);
        nvim.dismiss("/p/b");
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert!(has_off(&routed.mine), "{routed:?}");
        assert!(overlay.active_paths().is_empty());
    }

    // ---- a linked window's live drawings ------------------------------------------------------------

    #[test]
    fn a_linked_live_draw_is_reported_under_its_operation() {
        let nvim = Nvim::new();
        let mut overlay = EditorOverlay::new(Owner::Embedded);
        let mut at = Instant::now();
        overlay.call_for_op(&nvim, show("/p/a", "s2"), TWO, Some(4), at);
        overlay.call_for_op(&nvim, live_show("/p/b", "s2", 1), TWO, Some(5), at);
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(
            for_two(&routed),
            [
                LinkedEvent::Drawn {
                    op_id: None,
                    path: PathBuf::from("/p/a"),
                    state: Drawn::Drawn,
                },
                LinkedEvent::Drawn {
                    op_id: Some(5),
                    path: PathBuf::from("/p/b"),
                    state: Drawn::Drawn,
                },
            ],
            "a show's report is as it was; a live draw's names its operation"
        );
        assert_eq!(drawings(&overlay), vec![("/p/b", ("s2", 1))]);
        overlay.call_for_op(&nvim, live_clear("/p/b"), TWO, Some(6), at);
        let routed = ticks(&mut overlay, &nvim, &mut at);
        assert_eq!(
            for_two(&routed),
            [LinkedEvent::Drawn {
                op_id: Some(6),
                path: PathBuf::from("/p/b"),
                state: Drawn::Cleared,
            }]
        );
        assert!(overlay.live_drawings().is_empty());
    }

    fn remote_at_p() -> (RemoteOverlay, Rc<RefCell<Vec<EditorOp>>>) {
        let sent: Rc<RefCell<Vec<EditorOp>>> = Rc::default();
        let log = sent.clone();
        let remote = RemoteOverlay::new(
            Path::new("/p"),
            Box::new(move |op| {
                log.borrow_mut().push(op);
                log.borrow().len() as u64
            }),
        );
        (remote, sent)
    }

    #[test]
    fn a_linked_windows_live_drawing_is_known_only_from_its_own_report() {
        let (mut remote, sent) = remote_at_p();
        let at = Instant::now();
        remote.call(show("/p/a", "s2"), at);
        remote.call(live_show("/p/a", "s2", 3), at);
        assert_eq!(
            sent.borrow()[1],
            EditorOp::ReviewLiveShow {
                path: "a".to_owned(),
                meta: WireLiveMeta::from(&live_meta("s2", 3)),
                hunks: vec![WireHunk::from(&hunk())],
            }
        );
        // The show's report: the live draw behind it was left (an `o` drawing holds the file).
        remote.review_drawn_by(None, "a", Drawn::Drawn);
        assert!(remote.active_paths().contains(Path::new("/p/a")));
        assert!(
            remote.live_drawings().is_empty(),
            "the show's drawing is not a live one"
        );
        assert!(remote.tick(at).is_empty());

        remote.call(live_show("/p/a", "s2", 4), at);
        remote.review_drawn_by(Some(3), "a", Drawn::Drawn);
        assert_eq!(
            remote.live_drawings(),
            &BTreeMap::from([(PathBuf::from("/p/a"), ("s2".to_owned(), 4))])
        );
        assert_eq!(
            remote.tick(at),
            vec![OverlayOutcome::LiveDrawn {
                path: PathBuf::from("/p/a"),
                meta: live_meta("s2", 4),
                drawn: 1,
                left: false,
            }]
        );

        // An `o` drawing over it: no live drawing of this window's is there, so a live clear has
        // nothing to take off.
        remote.call(show("/p/a", "s2"), at);
        remote.review_drawn_by(None, "a", Drawn::Drawn);
        assert!(remote.live_drawings().is_empty());
        let before = sent.borrow().len();
        remote.call(live_clear("/p/a"), at);
        assert_eq!(sent.borrow().len(), before, "nothing of this window's is live there");

        // Window 1 takes a live drawing off (a live clear of this window's, or a contest's).
        remote.call(live_show("/p/b", "s2", 4), at);
        let id = sent.borrow().len() as u64;
        remote.review_drawn_by(Some(id), "b", Drawn::Drawn);
        remote.call(live_clear("/p/b"), at);
        assert_eq!(
            sent.borrow().last(),
            Some(&EditorOp::ReviewLiveClear { path: "b".to_owned() })
        );
        remote.review_drawn_by(Some(id + 1), "b", Drawn::Cleared);
        assert!(remote.live_drawings().is_empty());
        assert!(!remote.active_paths().contains(Path::new("/p/b")));

        // The editor or another window took it, or nothing of this window's is drawn any more.
        remote.call(live_show("/p/c", "s2", 4), at);
        let id = sent.borrow().len() as u64;
        remote.review_drawn_by(Some(id), "c", Drawn::Drawn);
        remote.review_drawn_by(None, "c", Drawn::Off);
        assert!(remote.live_drawings().is_empty());
        remote.call(live_show("/p/c", "s2", 5), at);
        let id = sent.borrow().len() as u64;
        remote.review_drawn_by(Some(id), "c", Drawn::Drawn);
        assert_eq!(remote.live_drawings().len(), 1);
        remote.review_idle();
        assert!(remote.live_drawings().is_empty());
        remote.editor_lost();
        assert!(remote.tick(at).is_empty());

        // Through the window's overlay, as the flow reads it.
        let (mut remote, _) = remote_at_p();
        remote.call(live_show("/p/d", "s2", 6), at);
        remote.review_drawn_by(Some(1), "d", Drawn::Drawn);
        let overlay = Overlay::Remote(remote);
        assert_eq!(
            overlay.live_drawings(),
            &BTreeMap::from([(PathBuf::from("/p/d"), ("s2".to_owned(), 6))])
        );
    }

    #[test]
    fn a_report_under_a_later_operation_lets_an_unreported_live_draw_go() {
        let (mut remote, sent) = remote_at_p();
        let at = Instant::now();
        remote.call(live_show("/p/a", "s2", 1), at);
        remote.call(live_show("/p/a", "s2", 2), at);
        // The first was left and never reported; the second went up.
        remote.review_drawn_by(Some(2), "a", Drawn::Drawn);
        assert_eq!(remote.live_drawings()[Path::new("/p/a")], ("s2".to_owned(), 2));
        // The first's id coming back now would be a report of no draw of this window's.
        remote.review_drawn_by(Some(1), "a", Drawn::Drawn);
        assert!(
            remote.live_drawings().is_empty(),
            "a report under an id no live draw waits for is not a live drawing"
        );
        assert_eq!(sent.borrow().len(), 2);
    }

    type NoteSent = Rc<RefCell<Vec<(EditorOp, Option<crate::nvim_rpc::Answer>)>>>;

    fn live_ask_at_p() -> (RemoteLiveAsk, NoteSent) {
        let sent: NoteSent = Rc::default();
        let log = sent.clone();
        let ask = RemoteLiveAsk::new(
            Path::new("/p"),
            Box::new(move |op| {
                let (answer, pending) = Pending::pair();
                log.borrow_mut().push((op, Some(answer)));
                Some(pending)
            }),
        );
        (ask, sent)
    }

    fn shown_answer(shown: &[bool]) -> Value {
        Value::Map(vec![(
            Value::from("shown"),
            Value::Array(shown.iter().map(|s| Value::from(*s)).collect()),
        )])
    }

    fn note_paths(op: &EditorOp) -> (Option<u64>, Vec<String>) {
        match op {
            EditorOp::ReviewLiveNote {
                session,
                turn_id,
                started_ns,
                ended_ns,
                paths,
            } => {
                assert_eq!((session.as_str(), turn_id.as_str(), *started_ns), ("s2", "t1", 5));
                (*ended_ns, paths.clone())
            }
            other => panic!("not a note: {other:?}"),
        }
    }

    #[test]
    fn a_linked_windows_note_goes_in_batches_and_its_answer_comes_back_by_place() {
        let (ask, sent) = live_ask_at_p();
        let mut abs: Vec<PathBuf> = (0..300).map(|n| PathBuf::from(format!("/p/f{n}"))).collect();
        abs.insert(1, PathBuf::from("/elsewhere/x"));
        let id = ask
            .note(&key("s2", "t1"), 5, None, &abs)
            .expect("a running turn's note asks");
        {
            let sent = sent.borrow();
            assert_eq!(sent.len(), 2, "one batch an operation");
            let (end, first) = note_paths(&sent[0].0);
            assert_eq!(end, None);
            assert_eq!(first.len(), MAX_SHOWN_PATHS);
            assert_eq!(first[0], "f0");
            assert_eq!(first[1], "f1", "the path outside the project is not named");
            assert_eq!(note_paths(&sent[1].0).1.len(), 300 - MAX_SHOWN_PATHS);
        }
        assert_eq!(ask.take_shown(id), None, "nothing answered yet");
        let mut answers: Vec<_> = sent.borrow_mut().iter_mut().map(|(_, a)| a.take().unwrap()).collect();
        let second = answers.pop().unwrap();
        answers.pop().unwrap().send(Ok(shown_answer(&[true; MAX_SHOWN_PATHS])));
        assert_eq!(ask.take_shown(id), None, "one batch is still out");
        let rest: Vec<bool> = (0..300 - MAX_SHOWN_PATHS).map(|n| n % 2 == 0).collect();
        second.send(Ok(shown_answer(&rest)));
        let shown = ask.take_shown(id).expect("both answered").expect("answered");
        let mut expected = vec![true; MAX_SHOWN_PATHS + 1];
        expected[1] = false;
        expected.extend(rest);
        assert_eq!(shown, expected, "each answer in the place of its path");
        assert_eq!(ask.take_shown(id), None, "an answer is taken once");

        // An ended turn's note asks nothing, and is told even when it names no path.
        sent.borrow_mut().clear();
        assert_eq!(ask.note(&key("s2", "t1"), 5, Some(9), &abs[..3]), None);
        assert_eq!(ask.note(&key("s2", "t1"), 5, Some(9), &[]), None);
        let ends: Vec<_> = sent.borrow().iter().map(|(op, _)| note_paths(op)).collect();
        assert_eq!(
            ends,
            vec![(Some(9), vec!["f0".to_owned(), "f1".to_owned()]), (Some(9), Vec::new())]
        );
        // A running note with nothing window 1 could be asked about sends nothing.
        sent.borrow_mut().clear();
        assert_eq!(
            ask.note(&key("s2", "t1"), 5, None, &[PathBuf::from("/elsewhere/x")]),
            None
        );
        assert!(sent.borrow().is_empty());
    }

    #[test]
    fn a_linked_windows_note_with_a_failed_batch_counts_its_paths_not_shown() {
        let (ask, sent) = live_ask_at_p();
        let abs: Vec<PathBuf> = (0..MAX_SHOWN_PATHS + 1)
            .map(|n| PathBuf::from(format!("/p/f{n}")))
            .collect();
        let id = ask.note(&key("s2", "t1"), 5, None, &abs).unwrap();
        let mut answers: Vec<_> = sent.borrow_mut().iter_mut().map(|(_, a)| a.take().unwrap()).collect();
        answers.pop().unwrap().send(Err(RpcError::Nvim("no editor".into())));
        answers.pop().unwrap().send(Ok(shown_answer(&[true; MAX_SHOWN_PATHS])));
        let shown = ask.take_shown(id).unwrap().unwrap();
        assert_eq!(shown.iter().filter(|s| **s).count(), MAX_SHOWN_PATHS);
        assert!(!shown[MAX_SHOWN_PATHS]);

        // Every batch failed, or the answer was not a shown list: the question failed.
        let id = ask.note(&key("s2", "t1"), 5, None, &abs[..1]).unwrap();
        let answer = sent.borrow_mut().last_mut().unwrap().1.take().unwrap();
        answer.send(Ok(Value::Nil));
        assert!(ask.take_shown(id).unwrap().is_err());

        // Not linked: nothing is asked.
        let unlinked = RemoteLiveAsk::new(Path::new("/p"), Box::new(|_| None));
        assert_eq!(unlinked.note(&key("s2", "t1"), 5, None, &abs[..1]), None);
    }
}
