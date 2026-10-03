//! Turn review in the user's own nvim: the Lua module that draws a turn's hunks over a buffer
//! ([`REVIEW_LUA`]), the constant calls into it, and the arguments those calls take.
//!
//! Every value -- a path, a line of a file, a session id -- goes to nvim as a msgpack argument of
//! a fixed chunk, never as Lua or Ex source. Line text goes as binary (a buffer line may be any
//! bytes) with its terminator named separately, so nothing between the file and the buffer
//! guesses an ending: [`ShowHunk`] is built from file lines by [`crate::editor_lines`] and turned
//! back into file bytes by the same module.

use std::collections::{BTreeSet, VecDeque};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rmpv::Value;

use crate::editor_lines::{to_buffer_lines, to_file_bytes, BufferLine, Eol};
use crate::editor_rpc::EditorRpc;
use crate::nvim_rpc::{Pending, RpcError};
use crate::turn_review::{Scope, MAX_OVERVIEW_DIFF_LINES};

/// The module itself; args `(owner, version)`. The same owner and version again change nothing
/// (`installed = false`); any other replaces the module there, tearing the old one down first.
/// Changing `review.lua` means bumping [`REVIEW_LUA_VERSION`], or an nvim that holds the old one
/// keeps it.
pub const REVIEW_LUA: &str = include_str!("review.lua");

/// Bumped whenever `review.lua` changes.
pub const REVIEW_LUA_VERSION: i64 = 5;

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
    /// `user`, `reload`, `closed` or `replaced`.
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

/// A call waiting to be sent, or sent and not yet answered.
struct Queued {
    /// Order of arrival: a call put back (its module was gone) goes back to its own place.
    seq: u64,
    call: OverlayCall,
    /// The module was missing once already; a second time fails the call.
    retried: bool,
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
    /// How many buffers show a review, by the editor's last answer.
    active: u64,
    /// The module's last `take_events` answer left events waiting.
    more_events: bool,
    /// A call's answer said nothing is drawn any more while events may still be waiting in the
    /// module (a revert made just before a clear): one more `take_events` is owed, sent after that
    /// answer so it reaches nvim after the call.
    drain_owed: bool,
    active_paths: BTreeSet<PathBuf>,
    /// Outcomes decided between two ticks.
    done: Vec<OverlayOutcome>,
}

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
            done: Vec::new(),
        }
    }

    /// Queues `call`, installing the module first when the editor has none from this driver. The
    /// call itself goes out only after the install answered.
    pub fn call(&mut self, rpc: &dyn EditorRpc, call: OverlayCall, _now: Instant) {
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
                call,
                "the editor cannot show a review over a file whose path is not UTF-8",
            );
            return;
        }
        self.poll_in_flight();
        self.poll_install();
        self.sync_target(rpc);
        let seq = self.next_seq;
        self.next_seq += 1;
        self.queue.push_back(Queued {
            seq,
            call,
            retried: false,
        });
        self.advance(rpc);
    }

    /// Polls every answer, sends what can now go, and asks for the module's events when due.
    /// Returns what became known since the last tick.
    pub fn tick(&mut self, rpc: &dyn EditorRpc, now: Instant) -> Vec<OverlayOutcome> {
        self.poll_in_flight();
        self.poll_install();
        self.poll_drain();
        self.sync_target(rpc);
        self.advance(rpc);
        self.drain_if_due(rpc, now);
        std::mem::take(&mut self.done)
    }

    /// The editor is gone, or its panel's drafts were dropped: nothing in flight will be heard
    /// of, and nothing is known to be installed or drawn.
    pub fn editor_lost(&mut self) {
        self.install = Install::None;
        self.queue.clear();
        self.in_flight.clear();
        self.drain = None;
        self.last_drain = None;
        self.active = 0;
        self.more_events = false;
        self.drain_owed = false;
        self.active_paths.clear();
        self.done.clear();
    }

    /// The files this driver last heard a review was drawn over.
    pub fn active_paths(&self) -> &BTreeSet<PathBuf> {
        &self.active_paths
    }

    /// Whether [`EditorOverlay::tick`] has anything to do: an answer to wait for, a call to send,
    /// an outcome to hand out, or a drawn review whose events are due.
    pub fn wants_ticks(&self) -> bool {
        !self.queue.is_empty()
            || !self.in_flight.is_empty()
            || self.drain.is_some()
            || !self.done.is_empty()
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
    }

    fn fail(&mut self, call: OverlayCall, why: &str) {
        if let OverlayCall::OpenAndShow { request_id, .. } = call {
            self.done.push(OverlayOutcome::Answered {
                request_id,
                result: Err(why.to_owned()),
            });
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
                            self.fail(queued.call, "the review module is not in the editor");
                            continue;
                        }
                        queued.retried = true;
                        if matches!(self.install, Install::Installed { .. }) {
                            self.forget_install();
                        }
                    }
                    self.requeue(queued);
                }
                Some(Ok(answer)) => self.answered(flight.queued.call, &answer),
                Some(Err(error)) => {
                    if is_gone(&error) {
                        self.forget_install();
                    }
                    self.fail(flight.queued.call, &error.to_string());
                }
            }
        }
        self.in_flight = waiting;
    }

    fn answered(&mut self, call: OverlayCall, answer: &Value) {
        let was_drawn = self.active > 0;
        self.note_active(answer);
        if was_drawn && self.active == 0 {
            self.drain_owed = true;
        }
        // What the editor keeps over the file: the hunks drawn and those reverted in the buffer that
        // it still follows (`kept`; an older module answers `drawn` alone).
        let drawn = lookup(answer, "kept")
            .or_else(|| lookup(answer, "drawn"))
            .and_then(Value::as_u64);
        match call {
            OverlayCall::OpenAndShow { request_id, path, .. } => {
                let opened = lookup(answer, "opened").and_then(Value::as_bool);

                let result = match opened {
                    Some(true) => {
                        match drawn {
                            Some(0) => {
                                self.active_paths.remove(&path);
                            }
                            Some(_) => {
                                self.active_paths.insert(path);
                            }
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
                self.done.push(OverlayOutcome::Answered { request_id, result });
            }
            OverlayCall::Show { path, .. } => {
                if drawn.is_some_and(|n| n > 0) {
                    self.active_paths.insert(path);
                } else {
                    self.active_paths.remove(&path);
                }
            }
            OverlayCall::Clear { path } => {
                self.active_paths.remove(&path);
            }
            OverlayCall::ClearAll => self.active_paths.clear(),
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
                    self.fail(queued.call, &why);
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

    /// The module's events as batches of one kind each, in the order they happened: a revert, its
    /// undo and a second revert of the same hunk must reach the draft in that order.
    fn events(&mut self, answer: &Value) {
        enum Batch {
            None,
            Reverts(Vec<EditorRevertEvent>),
            Unreverts(Vec<EditorRevertEvent>),
            Offs(Vec<OffEvent>),
        }
        fn flush(batch: Batch, done: &mut Vec<OverlayOutcome>) {
            match batch {
                Batch::None => {}
                Batch::Reverts(events) => done.push(OverlayOutcome::Reverts(events)),
                Batch::Unreverts(events) => done.push(OverlayOutcome::Unreverts(events)),
                Batch::Offs(offs) => done.push(OverlayOutcome::Off(offs)),
            }
        }
        let items: &[Value] = match lookup(answer, "events") {
            Some(Value::Array(items)) => items,
            _ => &[],
        };
        let mut batch = Batch::None;
        for item in items {
            let kind = lookup(item, "kind").and_then(Value::as_str);
            match kind {
                Some("revert") => match parse_revert_event(item) {
                    Ok(event) => match &mut batch {
                        Batch::Reverts(events) => events.push(event),
                        other => flush(std::mem::replace(other, Batch::Reverts(vec![event])), &mut self.done),
                    },
                    Err(why) => eprintln!("eitri: a review event from the editor was dropped: {why}"),
                },
                Some("unrevert") => match parse_unrevert_event(item) {
                    Ok(event) => match &mut batch {
                        Batch::Unreverts(events) => events.push(event),
                        other => flush(std::mem::replace(other, Batch::Unreverts(vec![event])), &mut self.done),
                    },
                    Err(why) => eprintln!("eitri: a review event from the editor was dropped: {why}"),
                },
                Some("off") => match (text(item, "path"), text(item, "why")) {
                    (Ok(path), Ok(why)) => {
                        self.active_paths.remove(Path::new(OsStr::from_bytes(path.as_bytes())));
                        let off = OffEvent { path, why };
                        match &mut batch {
                            Batch::Offs(offs) => offs.push(off),
                            other => flush(std::mem::replace(other, Batch::Offs(vec![off])), &mut self.done),
                        }
                    }
                    _ => eprintln!("eitri: a review event from the editor was dropped: unreadable off event"),
                },
                _ => {}
            }
        }
        flush(batch, &mut self.done);
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
            self.fail(flight.queued.call, "the editor changed before it answered");
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
                if let Some(queued) = self.queue.pop_front() {
                    let pending = send(rpc, &queued.call);
                    self.in_flight.push(InFlight {
                        queued,
                        pending,
                        epoch: self.epoch,
                    });
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
}
