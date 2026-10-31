//! Turn review in the user's own nvim: the Lua module that draws a turn's hunks over a buffer
//! ([`REVIEW_LUA`]), the constant calls into it, and the arguments those calls take.
//!
//! Every value -- a path, a line of a file, a session id -- goes to nvim as a msgpack argument of
//! a fixed chunk, never as Lua or Ex source. Line text goes as binary (a buffer line may be any
//! bytes) with its terminator named separately, so nothing between the file and the buffer
//! guesses an ending: [`ShowHunk`] is built from file lines by [`crate::editor_lines`] and turned
//! back into file bytes by the same module.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rmpv::Value;
use serde::{Deserialize, Serialize};

use crate::editor_lines::{to_buffer_lines, to_file_bytes, BufferLine, Eol};
use crate::editor_rpc::EditorRpc;
use crate::nvim_rpc::{Pending, RpcError};
use crate::turn_review::{Scope, MAX_OVERVIEW_DIFF_LINES};
use crate::window_link::{Drawn, EditorOp, WireHunk, WireMeta};
use crate::wm::PeerId;

/// The module itself; args `(owner, version)`. The same owner and version again change nothing
/// (`installed = false`); any other replaces the module there, tearing the old one down first.
/// Changing `find_buf.lua` or `review.lua` means bumping [`REVIEW_LUA_VERSION`], or an nvim that
/// holds the old one keeps it.
pub const REVIEW_LUA: &str = concat!(include_str!("find_buf.lua"), include_str!("review.lua"));

/// Bumped whenever `find_buf.lua` or `review.lua` changes.
pub const REVIEW_LUA_VERSION: i64 = 6;

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

/// Args `(abs_path: bin, meta: map, hunks: [map])`: draws into the buffer showing the file, if one
/// is loaded. Answers `{drawn, kept, skipped, notice?, active}`; `kept` also counts hunks reverted
/// in the buffer that the editor still follows. A hunk drawn over text that is its new side and
/// not its old one is also queued as an `unrevert` event, behind any earlier event, since it may
/// have been reverted while no overlay followed it.
pub const SHOW_LUA: &str = guarded!("return r.show(...)");
/// Args `(abs_path: bin, line: int|nil, meta: map, hunks: [map]|nil)`: opens the file (a split
/// when the current buffer cannot be left), puts the cursor on `line` and shows `hunks`. Answers
/// `{opened, error?, drawn?, skipped?, notice?, active}`.
pub const OPEN_AND_SHOW_LUA: &str = guarded!("return r.open_and_show(...)");
/// Args `(abs_path: bin)`: takes the file's overlay off, with no event. Answers `{active}`.
pub const CLEAR_LUA: &str = guarded!("return r.clear(...)");
/// No args: takes every overlay off, with no event. Answers `{active}`.
pub const CLEAR_ALL_LUA: &str = guarded!("return r.clear_all()");
/// No args: the oldest events since the last call, in order, as many as fit in one answer (at
/// least one), and how many buffers have an overlay. Answers `{events, active, more}`; `more`
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
    /// `user`, `reload`, `closed` or `replaced` (the module's own, or another window drawing over
    /// the file), or `reported` when the editor's window passed it on without saying why.
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
}

/// How often the module's events are asked for while any buffer shows a review.
const DRAIN_EVERY: Duration = Duration::from_millis(500);

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
    /// What became of a file drawn for that window. `op_id` is the window's open this is the outcome of;
    /// `None` for its other calls, and when another call or the editor took the file away.
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
    pub fn call_for(&mut self, rpc: &dyn EditorRpc, call: OverlayCall, from: Window, _now: Instant) {
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
            | OverlayCall::Show { path, .. } => Some(path),
            _ => None,
        };
        if let Some(path) = drawn_over.filter(|path| path.to_str().is_none()) {
            eprintln!(
                "eitri: the review is not drawn over {}: its path is not UTF-8",
                path.display()
            );
            self.fail(
                from,
                call,
                "the editor cannot show a review over a file whose path is not UTF-8",
            );
            return;
        }
        // The window that draws a session's turn holds that session; a move changes it only when
        // the move is decided (`release_session`).
        if let OverlayCall::OpenAndShow { meta, .. } | OverlayCall::Show { meta, .. } = &call {
            self.holders.insert(meta.session.clone(), from);
        }
        self.poll_in_flight();
        self.poll_install();
        self.sync_target(rpc);
        self.enqueue(call, from);
        self.advance(rpc);
    }

    fn enqueue(&mut self, call: OverlayCall, from: Window) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.queue.push_back(Queued {
            seq,
            call,
            retried: false,
            from,
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
        self.active_paths.clear();
        self.forget_linked_paths();
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
        self.active_paths.clear();
        self.forget_linked_paths();
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
    fn draw(&mut self, path: PathBuf, by: Window, op_id: Option<u64>, state: Drawn) {
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
            }
        }
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
                Some(Ok(answer)) => self.answered(flight.queued.from, flight.queued.call, &answer),
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

    fn answered(&mut self, from: Window, call: OverlayCall, answer: &Value) {
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
            }
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
                    (Ok(path), Ok(why)) => self.route_off(OffEvent { path, why }),
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

    /// A buffer that stopped showing a review is news for the window that drew it.
    fn route_off(&mut self, off: OffEvent) {
        let path = PathBuf::from(OsStr::from_bytes(off.path.as_bytes()));
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
                while let Some(queued) = self.queue.pop_front() {
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
        // Events left waiting by a full answer, or owed after the last overlay went, are asked for
        // at once, not half a second later.
        if !self.more_events
            && !self.drain_owed
            && self
                .last_drain
                .is_some_and(|at| now.saturating_duration_since(at) < DRAIN_EVERY)
        {
            return;
        }
        self.last_drain = Some(now);
        self.drain_owed = false;
        self.drain = Some((rpc.exec_lua(TAKE_EVENTS_LUA, Vec::new()), self.epoch));
    }
}

/// Window 2's driver: window 2 has no editor of its own, so its overlay calls go to window 1's
/// driver as editor operations, and what is drawn is known from what window 1 reports back
/// (`review_drawn`, `review_idle`), as the driver knows it from the editor's answers. Paths on the
/// link are relative to the project root; here they are absolute, as the flow uses them.
pub struct RemoteOverlay {
    root: PathBuf,
    send: Box<dyn Fn(EditorOp) -> u64>,
    /// Opens sent and not answered yet: the operation's id to the panel's request id.
    opens: BTreeMap<u64, String>,
    active_paths: BTreeSet<PathBuf>,
    done: Vec<OverlayOutcome>,
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
        };
        match op {
            Ok(op) => {
                let id = (self.send)(op);
                if let OverlayCall::OpenAndShow { request_id, .. } = call {
                    self.opens.insert(id, request_id);
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
        let inside = !path.is_empty()
            && Path::new(path)
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)));
        if !inside {
            eprintln!("eitri: window 1 reported a review drawn over a path outside the project: {path:?}");
            return;
        }
        let abs = self.root.join(path);
        match state {
            Drawn::Drawn | Drawn::Kept => {
                self.active_paths.insert(abs);
            }
            Drawn::Cleared => {
                self.active_paths.remove(&abs);
            }
            Drawn::Off => {
                self.active_paths.remove(&abs);
                push_off(&mut self.done, off_for(&abs, "reported"));
            }
        }
    }

    /// `review_idle`: window 1's driver reports none of this window's files drawn.
    pub fn review_idle(&mut self) {
        self.active_paths.clear();
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
    }

    pub fn active_paths(&self) -> &BTreeSet<PathBuf> {
        &self.active_paths
    }

    pub fn wants_ticks(&self) -> bool {
        !self.done.is_empty()
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

    /// An nvim that keeps what is drawn as the module would (one overlay per file), queues the
    /// events a test gives it, and answers each call when it is made or, while `hold` is set, when
    /// the test releases it.
    struct Nvim {
        serial: Cell<u64>,
        sent: RefCell<Vec<(&'static str, Vec<Value>)>>,
        held: RefCell<VecDeque<(Answer, Value)>>,
        hold: Cell<bool>,
        drawn: RefCell<BTreeSet<Vec<u8>>>,
        events: RefCell<Vec<Value>>,
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
            }
        }

        fn active(&self) -> Value {
            Value::from(self.drawn.borrow().len() as u64)
        }

        fn reply(&self, code: &'static str, args: &[Value]) -> Value {
            let path = || match &args[0] {
                Value::Binary(b) => b.clone(),
                other => panic!("a path travels as bytes: {other}"),
            };
            if code == REVIEW_LUA {
                map(vec![("installed", Value::from(true))])
            } else if code == SHOW_LUA {
                self.drawn.borrow_mut().insert(path());
                map(vec![
                    ("drawn", Value::from(1)),
                    ("kept", Value::from(1)),
                    ("active", self.active()),
                ])
            } else if code == OPEN_AND_SHOW_LUA {
                if args[3] != Value::Nil {
                    self.drawn.borrow_mut().insert(path());
                }
                map(vec![
                    ("opened", Value::from(true)),
                    ("drawn", Value::from(1)),
                    ("kept", Value::from(1)),
                    ("active", self.active()),
                ])
            } else if code == CLEAR_LUA {
                self.drawn.borrow_mut().remove(&path());
                map(vec![("active", self.active())])
            } else if code == CLEAR_ALL_LUA {
                self.drawn.borrow_mut().clear();
                map(vec![("active", Value::from(0))])
            } else if code == TAKE_EVENTS_LUA {
                let events: Vec<Value> = self.events.borrow_mut().drain(..).collect();
                map(vec![("events", Value::Array(events)), ("active", self.active())])
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
}
