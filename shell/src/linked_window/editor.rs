//! The editor is window 1's: no other window has an nvim, so everything window 2 or a module window needs of the
//! editor goes over its link. Each such window is served alike through its own link: what that link was told, the
//! pages its window holds, its operations still out and its drafts are its own, and an answer goes back on the link
//! the question came on. Below, "window 2" stands for any of them.
//!
//! Window 1 forwards what its editor feeds already parsed (the context, the theme with the editor's cell height,
//! nvim's keys), once at each link and then on change only. Window 2 asks for editor operations as typed
//! [`EditorOp`]s through [`EditorOps`], and window 1 answers each one through `editor_ops::dispatch`, which picks
//! the Lua and builds every argument itself after its own checks. Editor requests (a path to open, a draft to edit,
//! the keys for the editor) are intents: window 1 checks the path as it checks its own, raises itself and carries
//! the request out, and a draft written back in nvim goes to whichever window holds its tab at that moment. Follow
//! lines for a plugin page go to the window that holds it; nvim's side of them stays in window 1.
//!
//! Nothing here waits: answers are polled by a repeating step the window drives (a GLib timer; a test steps it
//! by hand), and every message is queued on the link.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eitri_core::editor_context::{ContextSource, EditorContext};
use eitri_core::editor_rpc::EditorRpc;
use eitri_core::editor_tools::stamp::EDITOR_BUDGET_MAX;
use eitri_core::editor_tools::way::RemoteWay;
use eitri_core::nvim_keys::NvimReport;
use eitri_core::nvim_rpc::{Answer, Pending, RpcError};
use eitri_core::plugin::follow_feed::{self, FollowBatch, FollowCmd, Ordered, ViewLine};
use eitri_core::plugin::protocol::ViewCause;
use eitri_core::review_editor::OverlayCall;
use eitri_core::scratch::{EditDone, PendingEdit, ScratchDir, ScratchRequest};
use eitri_core::tabs::TabId;
use eitri_core::theme::payload::NvimThemePayload;
use eitri_core::window_link::editor_ops::{dispatch, Dispatch, LiveNote, OpContext};
use eitri_core::window_link::names::WindowName;
use eitri_core::window_link::{DraftEnd, EditorOp, EditorRequest, LinkError, Msg, ReplyResult};
use eitri_core::wm::PeerId;
use serde_json::{json, Value};

use super::driver::LinkDriver;
use super::UNLINKED_NOTE;

/// Runs a step every `every` for as long as it returns `true`. The window gives a GLib timer; a test steps by
/// hand.
pub type Repeat = Rc<dyn Fn(Duration, Box<dyn Fn() -> bool>)>;

/// How long window 2 waits for the answer to an editor operation: the plugin host's own deadline for an editor
/// call, so a page asks the editor across the link on the terms it asks it in one window.
pub const OP_DEADLINE: Duration = eitri_core::plugin::host::EDITOR_DEADLINE;

/// How long window 2 waits for the answer to an editor tool's operation: past any budget the broker gives a call,
/// so the broker gives up first and says the editor did not answer. Ended here, the wait would read as a link that
/// is down, and tell the agent the editor's window is closed.
const AGENT_OP_DEADLINE: Duration = EDITOR_BUDGET_MAX;

/// How often window 1 looks for answers to window 2's operations and for drafts written back in nvim while either
/// is out.
const POLL_EVERY: Duration = Duration::from_millis(20);

/// What window 2's operations hear when their answer does not come in time.
pub const NO_ANSWER: &str = "the editor window did not answer in time";

// ---------------------------------------------------------------------------------------------------------------
// The feeds' records.

/// The context as it travels: the record the editor-context feed parsed.
pub fn context_value(context: Option<&EditorContext>) -> Option<Value> {
    context.map(|context| serde_json::to_value(context).expect("a context serialises"))
}

/// A context read back; one that does not parse is no context.
pub fn context_of(value: Option<&Value>) -> Option<EditorContext> {
    let value = value?;
    match serde_json::from_value(value.clone()) {
        Ok(context) => Some(context),
        Err(err) => {
            eprintln!("[link] the editor context from window 1 does not parse ({err}); none is used");
            None
        }
    }
}

/// The theme as it travels: nvim's theme payload and the editor's cell height (in GTK logical px), which window 2
/// has no editor to measure. Either is absent until window 1's editor has reported it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThemeRecord {
    pub payload: Option<NvimThemePayload>,
    pub cell_height: Option<f32>,
}

impl ThemeRecord {
    pub fn to_value(&self) -> Value {
        json!({
            "payload": self.payload.as_ref().map(|p| serde_json::to_value(p).expect("a theme payload serialises")),
            "cell_height": self.cell_height,
        })
    }

    pub fn from_value(value: &Value) -> Result<ThemeRecord, String> {
        let map = value.as_object().ok_or("the theme record is not an object")?;
        let payload = match map.get("payload") {
            None | Some(Value::Null) => None,
            Some(payload) => Some(serde_json::from_value(payload.clone()).map_err(|e| e.to_string())?),
        };
        let cell_height = match map.get("cell_height") {
            None | Some(Value::Null) => None,
            Some(h) => Some(h.as_f64().ok_or("the cell height is not a number")? as f32),
        };
        Ok(ThemeRecord { payload, cell_height })
    }
}

pub fn keys_value(report: &NvimReport) -> Value {
    serde_json::to_value(report).expect("a keys report serialises")
}

pub fn keys_of(value: &Value) -> Result<NvimReport, String> {
    serde_json::from_value(value.clone()).map_err(|e| e.to_string())
}

/// What window 1's feeds last parsed: one value of each kind, however many windows hear it.
#[derive(Default)]
pub struct FeedForward {
    context: Option<Value>,
    theme: ThemeRecord,
    keys: Option<Value>,
}

/// What one link was told, kind by kind, so a feed that repeats itself sends nothing; `None` for a kind not told yet.
#[derive(Default)]
struct Told {
    context: Option<Option<Value>>,
    theme: Option<Value>,
    keys: Option<Value>,
}

impl FeedForward {
    /// What a new link hears at once: everything known.
    fn new_link(&self, told: &mut Told) -> Vec<Msg> {
        [self.context_msg(told), self.theme_msg(told), self.keys_msg(told)]
            .into_iter()
            .flatten()
            .collect()
    }

    pub fn context(&mut self, context: Option<&EditorContext>) {
        self.context = context_value(context);
    }

    pub fn theme(&mut self, payload: &NvimThemePayload) {
        self.theme.payload = Some(payload.clone());
    }

    pub fn cell_height(&mut self, height: f32) {
        self.theme.cell_height = Some(height);
    }

    pub fn keys(&mut self, report: &NvimReport) {
        self.keys = Some(keys_value(report));
    }

    fn context_msg(&self, told: &mut Told) -> Option<Msg> {
        // A linked window starts with no context, so a link that has heard nothing is told only a context there is.
        let unchanged = match &told.context {
            Some(said) => *said == self.context,
            None => self.context.is_none(),
        };
        if unchanged {
            return None;
        }
        told.context = Some(self.context.clone());
        Some(Msg::Context {
            payload: self.context.clone(),
        })
    }

    fn theme_msg(&self, told: &mut Told) -> Option<Msg> {
        if self.theme == ThemeRecord::default() {
            return None;
        }
        let value = self.theme.to_value();
        if told.theme.as_ref() == Some(&value) {
            return None;
        }
        told.theme = Some(value.clone());
        Some(Msg::Theme { payload: value })
    }

    fn keys_msg(&self, told: &mut Told) -> Option<Msg> {
        let value = self.keys.clone()?;
        if told.keys.as_ref() == Some(&value) {
            return None;
        }
        told.keys = Some(value.clone());
        Some(Msg::Keys { payload: value })
    }
}

// ---------------------------------------------------------------------------------------------------------------
// An editor's answer as JSON, and back.

/// One of nvim's answers as JSON, reversibly: a map, a string that is not UTF-8, bytes, a 32-bit float and an
/// extension value each become a JSON object of one member naming what it was (`map` keeps its pairs in order and
/// its keys of any type), so every JSON object in the result is one of these and the way back is never a guess.
pub fn msgpack_to_json(value: &rmpv::Value) -> Result<Value, String> {
    use rmpv::Value as M;
    let bytes = |b: &[u8]| Value::Array(b.iter().map(|&byte| Value::from(byte)).collect());
    Ok(match value {
        M::Nil => Value::Null,
        M::Boolean(b) => Value::Bool(*b),
        M::Integer(i) => match (i.as_u64(), i.as_i64()) {
            (Some(u), _) => Value::from(u),
            (None, Some(i)) => Value::from(i),
            (None, None) => return Err(format!("an integer out of range: {i}")),
        },
        M::F32(f) => json!({ "f32": finite(f64::from(*f))? }),
        M::F64(f) => Value::from(finite(*f)?),
        M::String(s) => match s.as_str() {
            Some(text) => Value::String(text.to_owned()),
            None => json!({ "str": bytes(s.as_bytes()) }),
        },
        M::Binary(b) => json!({ "bin": bytes(b) }),
        M::Array(items) => Value::Array(items.iter().map(msgpack_to_json).collect::<Result<_, _>>()?),
        M::Map(pairs) => json!({
            "map": pairs
                .iter()
                .map(|(k, v)| Ok(Value::Array(vec![msgpack_to_json(k)?, msgpack_to_json(v)?])))
                .collect::<Result<Vec<_>, String>>()?
        }),
        M::Ext(kind, data) => json!({ "ext": [kind, bytes(data)] }),
    })
}

/// A msgpack string holding `bytes` as they are, UTF-8 or not: `rmpv` makes one only by reading it.
fn raw_string(bytes: &[u8]) -> Result<rmpv::Value, String> {
    let len = u32::try_from(bytes.len()).map_err(|_| "a string too long for msgpack".to_string())?;
    let mut raw = Vec::with_capacity(bytes.len() + 5);
    match len {
        0..=31 => raw.push(0xa0 | len as u8),
        32..=0xff => raw.extend([0xd9, len as u8]),
        0x100..=0xffff => {
            raw.push(0xda);
            raw.extend((len as u16).to_be_bytes());
        }
        _ => {
            raw.push(0xdb);
            raw.extend(len.to_be_bytes());
        }
    }
    raw.extend_from_slice(bytes);
    rmpv::decode::read_value(&mut raw.as_slice()).map_err(|e| e.to_string())
}

fn finite(f: f64) -> Result<f64, String> {
    if f.is_finite() {
        Ok(f)
    } else {
        Err(format!("a float JSON cannot carry: {f}"))
    }
}

/// The way back of [`msgpack_to_json`].
pub fn json_to_msgpack(value: &Value) -> Result<rmpv::Value, String> {
    use rmpv::Value as M;
    let bytes = |v: &Value| -> Result<Vec<u8>, String> {
        v.as_array()
            .ok_or("bytes are not a list")?
            .iter()
            .map(|b| {
                b.as_u64()
                    .and_then(|b| u8::try_from(b).ok())
                    .ok_or_else(|| format!("not a byte: {b}"))
            })
            .collect()
    };
    Ok(match value {
        Value::Null => M::Nil,
        Value::Bool(b) => M::Boolean(*b),
        Value::Number(n) => match (n.as_u64(), n.as_i64(), n.as_f64()) {
            (Some(u), _, _) => M::from(u),
            (None, Some(i), _) => M::from(i),
            (None, None, Some(f)) => M::F64(f),
            _ => return Err(format!("a number with no value: {n}")),
        },
        Value::String(s) => M::from(s.as_str()),
        Value::Array(items) => M::Array(items.iter().map(json_to_msgpack).collect::<Result<_, _>>()?),
        Value::Object(map) => {
            let mut members = map.iter();
            let (Some((tag, inner)), None) = (members.next(), members.next()) else {
                return Err("an object that is not one encoded value".into());
            };
            match tag.as_str() {
                "f32" => M::F32(inner.as_f64().ok_or("f32 is not a number")? as f32),
                "str" => raw_string(&bytes(inner)?)?,
                "bin" => M::Binary(bytes(inner)?),
                "map" => M::Map(
                    inner
                        .as_array()
                        .ok_or("a map is not a list of pairs")?
                        .iter()
                        .map(|pair| match pair.as_array().map(Vec::as_slice) {
                            Some([k, v]) => Ok((json_to_msgpack(k)?, json_to_msgpack(v)?)),
                            _ => Err(format!("not a pair: {pair}")),
                        })
                        .collect::<Result<_, String>>()?,
                ),
                "ext" => match inner.as_array().map(Vec::as_slice) {
                    Some([kind, data]) => M::Ext(
                        kind.as_i64()
                            .and_then(|k| i8::try_from(k).ok())
                            .ok_or("an extension type out of range")?,
                        bytes(data)?,
                    ),
                    _ => return Err("an extension is not a type and its bytes".into()),
                },
                other => return Err(format!("{other:?} is not an encoded value")),
            }
        }
    })
}

// ---------------------------------------------------------------------------------------------------------------
// Follow lines, as the follow socket writes them.

/// `view` as the line nvim's follow snippet writes, so the window that receives it reads it with the socket's own
/// parser.
pub fn view_line(view: &ViewLine) -> String {
    let mut line = json!({
        "v": 1,
        "k": "view",
        "owner": view.owner,
        "seq": view.seq,
        "buf": view.buf,
        "name": view.name,
        "filetype": view.filetype,
        "tick": view.tick,
        "current": view.current,
        "line_count": view.line_count,
        "top": view.top,
        "top_fraction": view.top_fraction,
        "bottom": view.bottom,
        "cursor": { "line": view.cursor_line, "col": view.cursor_col },
        "cause": match view.cause {
            ViewCause::User => "user",
            ViewCause::Reveal => "reveal",
        },
    });
    // A view with no token carries none, as the snippet writes it: a `null` token is not a line it writes. The same
    // goes for missing 'scrolloff' and screen geometry.
    if let Some(token) = view.token {
        line["token"] = json!(token);
    }
    if let Some(scrolloff) = view.scrolloff {
        line["scrolloff"] = json!(scrolloff);
    }
    if let Some(rows) = view.rows {
        line["rows"] = json!(rows);
    }
    if let Some(cursor_row) = view.cursor_row {
        line["cursor_row"] = json!(cursor_row);
    }
    line.to_string()
}

/// A command or a buffer's end as the follow snippet writes it.
pub fn ordered_line(line: &Ordered) -> String {
    match line {
        Ordered::Cmd { owner, cmd, buf } => {
            let cmd = match cmd {
                FollowCmd::Show => "show",
                FollowCmd::Toggle => "toggle",
                FollowCmd::Hide => "hide",
                FollowCmd::Auto => "auto",
            };
            let mut line = json!({ "v": 1, "k": "cmd", "owner": owner, "cmd": cmd });
            if let Some(buf) = buf {
                line["buf"] = json!(buf);
            }
            line.to_string()
        }
        Ordered::Gone { owner, buf } => json!({ "v": 1, "k": "gone", "owner": owner, "buf": buf }).to_string(),
    }
}

fn owner_of(line: &Ordered) -> &str {
    match line {
        Ordered::Cmd { owner, .. } | Ordered::Gone { owner, .. } => owner,
    }
}

/// `batch` split by where each line's plugin page is: what stays in this window, and the lines for the pages
/// `there` names, in their order (views first, then the ordered lines, as a consumer applies one batch).
pub fn split_follow(batch: FollowBatch, there: &BTreeSet<String>) -> (FollowBatch, Vec<(String, String)>) {
    let mut out = Vec::new();
    let mut here = FollowBatch {
        views_received: batch.views_received,
        ordered_received: batch.ordered_received,
        ..FollowBatch::default()
    };
    for view in batch.views {
        if there.contains(&view.owner) {
            out.push((view.owner.clone(), view_line(&view)));
        } else {
            here.views.push(view);
        }
    }
    for line in batch.ordered {
        if there.contains(owner_of(&line)) {
            out.push((owner_of(&line).to_owned(), ordered_line(&line)));
        } else {
            here.ordered.push(line);
        }
    }
    (here, out)
}

/// A follow line from window 1 as a batch of its own; `None` for a line that does not parse or names another
/// owner than the message.
pub fn follow_batch(owner: &str, line: &str) -> Option<FollowBatch> {
    let (view, ordered) = follow_feed::parse_line(line.as_bytes())?;
    let mut batch = FollowBatch::default();
    if let Some(view) = view {
        if view.owner != owner {
            return None;
        }
        batch.views.push(view);
        batch.views_received = 1;
    }
    if let Some(line) = ordered {
        if owner_of(&line) != owner {
            return None;
        }
        batch.ordered.push(line);
        batch.ordered_received = 1;
    }
    Some(batch)
}

// ---------------------------------------------------------------------------------------------------------------
// Scratch requests, read back.

/// What a panel's request for nvim asks, read back from the request's own encoding.
#[derive(Debug, Clone, PartialEq)]
pub enum ScratchAsk {
    /// `gf` or a path click: the file at `path` (absolute), at `line`.
    Open { path: PathBuf, line: Option<u32> },
    /// `Ctrl+g` in INPUT: the draft in `body`, its end reported in `done`.
    Edit { body: PathBuf, done: PathBuf },
    /// A read-only view of the text in `body`; with `tab`, in a tab page of its own rather than the row-view split.
    View { body: PathBuf, tab: bool },
    /// A pending edit's proposal: the file as it is in `base`, as the edit would leave it in `proposed`; `name` is
    /// the real file's name.
    Diff {
        base: PathBuf,
        proposed: PathBuf,
        name: String,
    },
}

pub fn read_request(request: &ScratchRequest) -> Result<ScratchAsk, String> {
    let hex = request.hex();
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(hex.get(at..at + 2).unwrap_or(""), 16))
        .collect::<Result<Vec<u8>, _>>()
        .map_err(|e| format!("a request for nvim that is not hex: {e}"))?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("a request for nvim that is not JSON: {e}"))?;
    let text = |key: &str| value.get(key).and_then(Value::as_str).map(PathBuf::from);
    match value.get("op").and_then(Value::as_str) {
        Some("open") => Ok(ScratchAsk::Open {
            path: text("path").ok_or("an open with no path")?,
            line: value
                .get("line")
                .and_then(Value::as_u64)
                .and_then(|l| u32::try_from(l).ok()),
        }),
        Some("edit") => Ok(ScratchAsk::Edit {
            body: text("path").ok_or("an edit with no path")?,
            done: text("done").ok_or("an edit with no marker")?,
        }),
        Some("view") => Ok(ScratchAsk::View {
            body: text("path").ok_or("a view with no path")?,
            tab: value.get("tab").and_then(Value::as_bool).unwrap_or(false),
        }),
        Some("diff") => Ok(ScratchAsk::Diff {
            base: text("base").ok_or("a diff with no file as it is")?,
            proposed: text("proposed").ok_or("a diff with no proposed file")?,
            name: value
                .get("name")
                .and_then(Value::as_str)
                .ok_or("a diff with no file name")?
                .to_string(),
        }),
        other => Err(format!("a request for nvim of no known kind: {other:?}")),
    }
}

/// The edit number in a draft's file name (`<id>-draft.md`, as `ScratchDir::prepare_edit` names it).
fn edit_id(body: &Path) -> Option<u64> {
    body.file_name()?.to_str()?.strip_suffix("-draft.md")?.parse().ok()
}

/// A view's title read back from its file's name (`<id>-<slug>.md`, as `ScratchDir::prepare_view` names it).
fn view_title(body: &Path) -> String {
    let name = body.file_stem().and_then(|stem| stem.to_str()).unwrap_or("");
    let slug = name
        .trim_start_matches(|c: char| c.is_ascii_digit())
        .trim_start_matches('-');
    if slug.is_empty() {
        "view".to_string()
    } else {
        slug.to_string()
    }
}

/// A reviewed file to open: a project path in the form turn review keeps its own (plain names, no link or
/// non-directory among its parents), which must exist.
fn review_target(root: &Path, path: &str) -> Result<PathBuf, String> {
    if path.chars().any(|c| c.is_ascii_control()) {
        return Err(format!("refused to open a path holding a control character: {path:?}"));
    }
    let dir = eitri_core::turn_review::write::ProjectDir::open(root)
        .map_err(|e| format!("{} cannot be opened: {e}", root.display()))?;
    dir.target(Path::new(path), false).map_err(|e| e.to_string())?;
    let resolved = root.join(path);
    if !resolved.exists() {
        return Err(format!("no such file: {path}"));
    }
    Ok(resolved)
}

/// A path as window 2 names it to window 1: relative to the project root when it is under it.
fn named_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------------------------------------------
// Window 1.

/// What window 1 does in its editor for window 2: show it, give it the keys, and hand nvim a request.
pub enum EditorAct<'a> {
    Request(&'a ScratchRequest),
    /// Only the keys: the editor key pressed in window 2.
    Focus,
}

/// Carries an act out in window 1's own editor, after the window raised itself with the token window 2 sent.
pub type Drive = Rc<dyn Fn(EditorAct<'_>, Option<String>) -> Result<(), String>>;

/// Where a linked window's review calls go: window 1's own review driver, as that window's. Its answer is the op's
/// reply, or `None` when the driver answers the op itself once it has the call's outcome (an open).
pub type OverlayHook = Rc<dyn Fn(PeerId, u64, Vec<OverlayCall>) -> Option<ReplyResult>>;

/// The files window 1's review driver last heard were drawn for the window at this peer: the only ones it may clear.
pub type DrawnFor = Rc<dyn Fn(PeerId) -> Vec<PathBuf>>;

/// Where a linked window's live note goes: window 1's own review driver, as that window's. Whether a driver took it.
pub type LiveNoteHook = Rc<dyn Fn(PeerId, LiveNote) -> bool>;

/// A draft edit for a tab that is in this window now ended in nvim: its session, and how it ended. Called for every
/// end, a discarded or failed one too, since the tab is still marked as editing until it hears.
pub type DraftHere = Rc<dyn Fn(Option<String>, EditDone)>;

/// Whether a tab of this window holds an agent session.
pub type OwnSession = Rc<dyn Fn(&str) -> bool>;

/// Which tab, holding which session, a draft edit of window 2's panel is out for.
pub type DraftTab = Rc<dyn Fn(u64) -> Option<(TabId, Option<String>)>>;

/// An activation token from this window's display, for the other window to come forward with; `None` when the
/// compositor gives none.
pub type TokenSource = Rc<dyn Fn() -> Option<String>>;

/// Brings this window forward with the other window's activation token, if it sent one.
pub type Raise = Rc<dyn Fn(Option<String>)>;

/// A draft edit out in nvim for the tab `tab` of the window at `peer`: a tab's number means something only in its own
/// window.
struct OutDraft {
    edit: PendingEdit,
    peer: PeerId,
    tab: u64,
    session: Option<String>,
    /// The generation of `peer`'s link when the draft went out: the draft goes back only on that link, since a window
    /// opened again under the same peer has tabs of its own under the same numbers.
    asked_on: Option<u64>,
}

/// One linked window, as window 1's editor serves it.
struct LinkState {
    link: LinkDriver,
    name: WindowName,
    /// What this link was last told.
    told: Told,
    /// Which link to `peer` this is: a window that closes and is opened again is the same peer on a new link, and
    /// what was asked on the old one is no business of the new.
    generation: u64,
}

/// The roots of the plugin an id names, from this window's table; `None` when no plugin of that id is loaded here, as
/// for a Lua panel.
pub type PageRoots = Rc<dyn Fn(&str) -> Option<BTreeMap<String, PathBuf>>>;

/// The window a plugin's page is in, when that is not this one, and the plugin's roots from this window's table.
struct HeldPage {
    peer: PeerId,
    roots: BTreeMap<String, PathBuf>,
}

/// Window 1's side: the feeds told to each linked window, its operations answered and its requests carried out.
pub struct Window1Editor {
    me: Weak<Window1Editor>,
    root: PathBuf,
    repeat: Repeat,
    links: RefCell<BTreeMap<PeerId, LinkState>>,
    feeds: RefCell<FeedForward>,
    rpc: RefCell<Option<Rc<dyn EditorRpc>>>,
    own_session: RefCell<OwnSession>,
    /// By plugin, not by link: a page is in one window, and a page a restored layout left away is that window's
    /// before the window has linked.
    plugins: RefCell<BTreeMap<String, HeldPage>>,
    page_roots: RefCell<Option<PageRoots>>,
    plugin_follows: RefCell<BTreeMap<String, eitri_core::window_link::editor_ops::FollowConfig>>,
    drawn_for: RefCell<Option<DrawnFor>>,
    overlay: RefCell<Option<OverlayHook>>,
    live_note: RefCell<Option<LiveNoteHook>>,
    drive: RefCell<Option<Drive>>,
    draft_here: RefCell<Option<DraftHere>>,
    token: RefCell<Option<TokenSource>>,
    inflight: RefCell<Vec<(PeerId, u64, Pending)>>,
    /// Window 1's own scratch directory, under which window 2's drafts get one of their own: the editor-context
    /// feed already leaves out every buffer under it.
    scratch_parent: Option<PathBuf>,
    scratch: RefCell<Option<ScratchDir>>,
    drafts: RefCell<Vec<OutDraft>>,
    /// Views from another window being written on a worker, oldest first: each is handed to the editor once
    /// written, in the order they came.
    views: RefCell<Vec<WritingView>>,
    /// The generation the next link gets (`LinkState::generation`).
    link_generations: Cell<u64>,
    polling: Cell<bool>,
}

/// A view another window sent, its file being written on a worker thread: a view's text may be a megabyte, and
/// the GTK thread never waits for a disk. `token` is the asking window's activation token, kept for the drive.
struct WritingView {
    rx: mpsc::Receiver<std::io::Result<ScratchRequest>>,
    token: Option<String>,
    /// The window and the id to answer with [`Msg::EditorShown`] once the view is written and handed to the editor,
    /// when the request asked for an answer.
    answer: Option<(PeerId, u64)>,
    /// The generation of `answer`'s link when the view came; the answer goes only to that link.
    asked_on: Option<u64>,
}

impl Window1Editor {
    pub fn new(root: &Path, scratch_parent: Option<PathBuf>, repeat: Repeat) -> Rc<Window1Editor> {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        Rc::new_cyclic(|me| Window1Editor {
            me: me.clone(),
            root,
            repeat,
            links: RefCell::default(),
            feeds: RefCell::default(),
            rpc: RefCell::default(),
            own_session: RefCell::new(Rc::new(|_: &str| false)),
            plugins: RefCell::default(),
            page_roots: RefCell::default(),
            plugin_follows: RefCell::default(),
            drawn_for: RefCell::default(),
            overlay: RefCell::default(),
            live_note: RefCell::default(),
            drive: RefCell::default(),
            draft_here: RefCell::default(),
            token: RefCell::default(),
            inflight: RefCell::default(),
            scratch_parent,
            scratch: RefCell::default(),
            drafts: RefCell::default(),
            views: RefCell::default(),
            link_generations: Cell::new(0),
            polling: Cell::new(false),
        })
    }

    pub fn set_rpc(&self, rpc: Rc<dyn EditorRpc>) {
        *self.rpc.borrow_mut() = Some(rpc);
    }

    /// Whether a tab of this window holds a session: such a session is never window 2's.
    pub fn set_own_session(&self, own: OwnSession) {
        *self.own_session.borrow_mut() = own;
    }

    pub fn set_drive(&self, drive: Drive) {
        *self.drive.borrow_mut() = Some(drive);
    }

    pub fn set_draft_here(&self, here: DraftHere) {
        *self.draft_here.borrow_mut() = Some(here);
    }

    /// Where a draft's end gets the token it carries back: the keys are in this window's editor when the edit ends,
    /// and window 2 needs a token from here to take them back.
    pub fn set_token_source(&self, token: TokenSource) {
        *self.token.borrow_mut() = Some(token);
    }

    pub fn set_overlay(&self, hook: OverlayHook) {
        *self.overlay.borrow_mut() = Some(hook);
    }

    pub fn set_drawn_for(&self, drawn: DrawnFor) {
        *self.drawn_for.borrow_mut() = Some(drawn);
    }

    pub fn set_live_note(&self, hook: LiveNoteHook) {
        *self.live_note.borrow_mut() = Some(hook);
    }

    /// What the plugin `owner` follows in this window's editor (its socket, its filetypes and commands), kept for
    /// when its page is in window 2 and its follow lines go there.
    pub fn set_plugin_follow(&self, owner: &str, config: eitri_core::window_link::editor_ops::FollowConfig) {
        self.plugin_follows.borrow_mut().insert(owner.to_owned(), config);
    }

    /// The plugin `owner`'s page is in the window at `peer` (`roots` from this window's plugin table), or, with
    /// either absent, in no linked window. A page is in one window, so this replaces where it was.
    pub fn set_plugin_in_window(&self, owner: &str, peer: Option<PeerId>, roots: Option<BTreeMap<String, PathBuf>>) {
        let mut plugins = self.plugins.borrow_mut();
        match (peer, roots) {
            (Some(peer), Some(roots)) => plugins.insert(owner.to_owned(), HeldPage { peer, roots }),
            _ => plugins.remove(owner),
        };
    }

    /// Where the roots of a page window's plugin come from, when that window links.
    pub fn set_page_roots(&self, roots: PageRoots) {
        *self.page_roots.borrow_mut() = Some(roots);
    }

    /// The window at `peer`, as a log line names it.
    fn window_text(&self, peer: PeerId) -> String {
        match self.links.borrow().get(&peer) {
            Some(state) => format!("window {}", state.name.text()),
            None => format!("the window of {peer:?}"),
        }
    }

    /// Sends what the window holds for the link to `peer`, if it is up; a send refused for its size is logged.
    fn send(&self, peer: PeerId, msg: Msg) {
        let kind = msg.kind();
        if let Err(err) = self.try_send(peer, msg) {
            eprintln!("[link] {kind} could not be sent to {}: {err}", self.window_text(peer));
        }
    }

    /// [`send`](Self::send), with the refusal returned. Nothing is sent, and `Ok` comes back, while unlinked.
    fn try_send(&self, peer: PeerId, msg: Msg) -> Result<(), LinkError> {
        let link = self.links.borrow().get(&peer).map(|state| state.link.clone());
        match link.filter(LinkDriver::linked) {
            Some(link) => link.send(msg, Vec::new()),
            None => Ok(()),
        }
    }

    /// A draft edit of a tab of the window at `peer` ended: the tab hears how. A draft too large to hand back still
    /// ends the tab's edit, with the reason, so its next `Ctrl+g` works.
    fn draft_back(&self, peer: PeerId, tab: u64, session: Option<String>, text: String, end: DraftEnd) {
        let source = self.token.borrow().clone();
        let token = source.and_then(|token| token());
        let refused = match self.try_send(
            peer,
            Msg::DraftBack {
                tab,
                session: session.clone(),
                text,
                end,
                token: token.clone(),
            },
        ) {
            Ok(()) => return,
            Err(err) => err,
        };
        let window = self.window_text(peer);
        eprintln!("[scratch] the draft of {window}'s tab {tab} could not be handed back: {refused}");
        self.send(
            peer,
            Msg::DraftBack {
                tab,
                session,
                text: String::new(),
                end: DraftEnd::Failed {
                    why: format!("the edited draft could not be handed back ({refused}); the tab keeps its draft"),
                },
                token,
            },
        );
    }

    /// Tells each link what `told` finds it has not heard of one kind, which goes to none that has.
    fn tell(&self, told: fn(&FeedForward, &mut Told) -> Option<Msg>) {
        let out: Vec<(PeerId, Msg)> = {
            let feeds = self.feeds.borrow();
            let mut links = self.links.borrow_mut();
            links
                .iter_mut()
                .filter_map(|(peer, state)| Some((*peer, told(&feeds, &mut state.told)?)))
                .collect()
        };
        for (peer, msg) in out {
            self.send(peer, msg);
        }
    }

    pub fn context_changed(&self, context: Option<&EditorContext>) {
        self.feeds.borrow_mut().context(context);
        self.tell(FeedForward::context_msg);
    }

    pub fn theme_changed(&self, payload: &NvimThemePayload) {
        self.feeds.borrow_mut().theme(payload);
        self.tell(FeedForward::theme_msg);
    }

    pub fn cell_height_changed(&self, height: f32) {
        self.feeds.borrow_mut().cell_height(height);
        self.tell(FeedForward::theme_msg);
    }

    pub fn keys_changed(&self, report: &NvimReport) {
        self.feeds.borrow_mut().keys(report);
        self.tell(FeedForward::keys_msg);
    }

    /// A link to the window `name` is up: its operations and requests are taken from it, and it is told the feeds.
    /// Links already up hear nothing of it.
    pub fn linked(&self, peer: PeerId, name: WindowName, link: &LinkDriver) {
        // A page window holds its plugin's page from its first message on. A Lua panel's window has no plugin here.
        if let WindowName::Page(id) = &name {
            let roots = self.page_roots.borrow().as_ref().and_then(|roots| roots(id));
            if roots.is_some() {
                self.set_plugin_in_window(id, Some(peer), roots);
            }
        }
        let me = self.me.clone();
        link.on("editor_op", move |_, msg, _| {
            if let (Some(me), Msg::EditorOp { id, op }) = (me.upgrade(), msg) {
                me.op(peer, id, op);
            }
        });
        let me = self.me.clone();
        link.on("editor", move |_, msg, _| {
            if let (Some(me), Msg::Editor { request, token, answer }) = (me.upgrade(), msg) {
                // A view is answered once it is written and handed to the editor (`show_written_views`); every
                // other request, and a view refused before its write began, here.
                let view = matches!(request, EditorRequest::View { .. });
                let result = me.request(peer, request, token, answer);
                if let Err(why) = &result {
                    eprintln!("[link] {}'s editor request was refused: {why}", me.window_text(peer));
                }
                if let Some(id) = answer.filter(|_| !view || result.is_err()) {
                    me.send(peer, Msg::EditorShown { id, why: result.err() });
                }
            }
        });
        let mut told = Told::default();
        let heard = self.feeds.borrow().new_link(&mut told);
        let generation = self.link_generations.get() + 1;
        self.link_generations.set(generation);
        self.links.borrow_mut().insert(
            peer,
            LinkState {
                link: link.clone(),
                name,
                told,
                generation,
            },
        );
        for msg in heard {
            self.send(peer, msg);
        }
    }

    /// The link to `peer` ended: what its window held is forgotten, and its operations still out have nowhere to be
    /// answered, and so is a view of it still being written (it opens, with no answer). The other links carry on as
    /// they were. A draft still out in nvim stays: once written it goes to its tab if that tab is here by then, and
    /// never to a window opened in this one's place.
    pub fn unlinked(&self, peer: PeerId) {
        self.links.borrow_mut().remove(&peer);
        self.plugins.borrow_mut().retain(|_, page| page.peer != peer);
        self.inflight.borrow_mut().retain(|(asking, ..)| *asking != peer);
    }

    /// The owners whose follow lines go to another window.
    pub fn follow_owners(&self) -> BTreeSet<String> {
        self.plugins.borrow().keys().cloned().collect()
    }

    /// `batch` with the lines for the pages other windows hold sent to them; the rest is this window's.
    pub fn route_follow(&self, batch: FollowBatch) -> FollowBatch {
        let owners = self.follow_owners();
        if owners.is_empty() {
            return batch;
        }
        let (here, there) = split_follow(batch, &owners);
        for (owner, line) in there {
            let holder = self.plugins.borrow().get(&owner).map(|page| page.peer);
            if let Some(peer) = holder {
                self.send(peer, Msg::Follow { owner, line });
            }
        }
        here
    }

    fn reply(&self, peer: PeerId, id: u64, result: ReplyResult) {
        self.send(peer, Msg::EditorReply { id, result });
    }

    fn op(&self, peer: PeerId, id: u64, op: EditorOp) {
        let rpc: Rc<dyn EditorRpc> = self.rpc.borrow().clone().unwrap_or_else(|| Rc::new(super::NoEditor));
        let name = op.name();
        let dispatched = dispatch(id, op, &Ctx { one: self, peer }, &*rpc);
        match dispatched {
            Err(refused) => {
                println!("[link] {}'s {name} was refused: {refused}", self.window_text(peer));
                self.reply(
                    peer,
                    id,
                    ReplyResult::Err {
                        why: refused.to_string(),
                    },
                );
            }
            Ok(Dispatch::Rpc(pending)) => {
                self.inflight.borrow_mut().push((peer, id, pending));
                self.poll_soon();
            }
            Ok(Dispatch::Overlay(calls)) => {
                let hook = self.overlay.borrow().clone();
                let result = match hook {
                    Some(hook) => hook(peer, id, calls),
                    None => Some(ReplyResult::Err {
                        why: "this window's review driver takes no calls from the other window".into(),
                    }),
                };
                if let Some(result) = result {
                    self.reply(peer, id, result);
                }
            }
            // The note goes into the driver's table first; the editor's answer to which files are shown is
            // the reply, and an ended turn's note, which asks nothing, is answered at once.
            Ok(Dispatch::LiveNote { note, shown }) => {
                let hook = self.live_note.borrow().clone();
                let taken = hook.is_some_and(|hook| hook(peer, note));
                match (taken, shown) {
                    (false, _) => self.reply(
                        peer,
                        id,
                        ReplyResult::Err {
                            why: "this window's review driver takes no notes from the other window".into(),
                        },
                    ),
                    (true, Some(pending)) => {
                        self.inflight.borrow_mut().push((peer, id, pending));
                        self.poll_soon();
                    }
                    (true, None) => self.reply(peer, id, ReplyResult::Ok { value: Value::Null }),
                }
            }
        }
    }

    fn request(
        &self,
        peer: PeerId,
        request: EditorRequest,
        token: Option<String>,
        answer: Option<u64>,
    ) -> Result<(), String> {
        let drive = self
            .drive
            .borrow()
            .clone()
            .ok_or("this window has no editor to drive")?;
        match request {
            EditorRequest::OpenPath { path, line, col: _ } => {
                // The rule a window's own panel uses, so a path from window 2 opens exactly when it would open from
                // window 1's own chat.
                let resolved = eitri_panel::agent_panel::open_path_target(&self.root, &path)?;
                drive(
                    EditorAct::Request(&eitri_core::scratch::open_request(&resolved, line)),
                    token,
                )
            }
            EditorRequest::ReviewOpen { path, line } => {
                let resolved = review_target(&self.root, &path)?;
                drive(
                    EditorAct::Request(&eitri_core::scratch::open_request(&resolved, line)),
                    token,
                )
            }
            EditorRequest::Focus => drive(EditorAct::Focus, token),
            EditorRequest::View { title, text, tab } => {
                // Only the file's name is chosen here; the text is written on a worker and `poll` hands nvim the
                // view once it is.
                let file = self.with_scratch(|dir| Ok(dir.reserve_view(&title).with_tab(tab)))?;
                let (tx, rx) = mpsc::channel();
                std::thread::Builder::new()
                    .name("linked view write".into())
                    .spawn(move || {
                        // A window closed in the meantime has dropped the receiver; the answer has nowhere to go.
                        let _ = tx.send(file.write(&text));
                    })
                    .map_err(|e| format!("could not write the scratch file: {e}"))?;
                let asked_on = self.links.borrow().get(&peer).map(|state| state.generation);
                self.views.borrow_mut().push(WritingView {
                    rx,
                    token,
                    answer: answer.map(|id| (peer, id)),
                    asked_on,
                });
                self.poll_soon();
                Ok(())
            }
            EditorRequest::Diff { name, base, proposed } => {
                let request = self.with_scratch(|dir| dir.prepare_proposal(&name, &base, &proposed))?;
                drive(EditorAct::Request(&request), token)
            }
            EditorRequest::Scratch { tab, session, text } => {
                let prepared = self.prepare_draft(&text);
                let started = prepared.and_then(|(request, edit)| match drive(EditorAct::Request(&request), token) {
                    Ok(()) => Ok(edit),
                    Err(why) => {
                        edit.cleanup();
                        Err(why)
                    }
                });
                match started {
                    Ok(edit) => {
                        let asked_on = self.links.borrow().get(&peer).map(|state| state.generation);
                        self.drafts.borrow_mut().push(OutDraft {
                            edit,
                            peer,
                            tab,
                            session,
                            asked_on,
                        });
                        self.poll_soon();
                        Ok(())
                    }
                    Err(why) => {
                        // Window 2's tab is waiting for its edit to end: it hears why, keeps its draft, and its
                        // next `Ctrl+g` works.
                        let end = DraftEnd::Failed { why: why.clone() };
                        self.draft_back(peer, tab, session, String::new(), end);
                        Err(why)
                    }
                }
            }
        }
    }

    fn prepare_draft(&self, text: &str) -> Result<(ScratchRequest, PendingEdit), String> {
        self.with_scratch(|dir| dir.prepare_edit(text))
    }

    /// Writes window 2's text into the scratch directory kept for it under this window's own, made on first use.
    fn with_scratch<T>(&self, write: impl FnOnce(&mut ScratchDir) -> std::io::Result<T>) -> Result<T, String> {
        let mut scratch = self.scratch.borrow_mut();
        if scratch.is_none() {
            let parent = self
                .scratch_parent
                .as_deref()
                .ok_or("this window has no scratch directory for the other window's files")?;
            *scratch =
                Some(ScratchDir::in_dir(parent).ok_or("the other window's scratch directory could not be made")?);
        }
        let dir = scratch.as_mut().expect("made above");
        write(dir).map_err(|e| format!("could not write the scratch file: {e}"))
    }

    fn poll_soon(&self) {
        if self.polling.replace(true) {
            return;
        }
        let me = self.me.clone();
        (self.repeat)(
            POLL_EVERY,
            Box::new(move || {
                let Some(me) = me.upgrade() else { return false };
                let more = me.poll();
                if !more {
                    me.polling.set(false);
                }
                more
            }),
        );
    }

    /// Answers every operation whose editor call came back and routes every draft written back. Whether anything
    /// is still out.
    pub fn poll(&self) -> bool {
        let answered: Vec<(PeerId, u64, ReplyResult)> = {
            let mut inflight = self.inflight.borrow_mut();
            let mut answered = Vec::new();
            inflight.retain(|(peer, id, pending)| match pending.try_take() {
                None => true,
                Some(reply) => {
                    let result = match reply {
                        Ok(value) => match msgpack_to_json(&value) {
                            Ok(value) => ReplyResult::Ok { value },
                            Err(why) => ReplyResult::Err { why },
                        },
                        Err(err) => ReplyResult::Err { why: err.to_string() },
                    };
                    answered.push((*peer, *id, result));
                    false
                }
            });
            answered
        };
        for (peer, id, result) in answered {
            self.reply(peer, id, result);
        }
        let done: Vec<(OutDraft, EditDone)> = {
            let mut drafts = self.drafts.borrow_mut();
            let mut done = Vec::new();
            let mut index = 0;
            while index < drafts.len() {
                match drafts[index].edit.poll() {
                    Some(result) => done.push((drafts.remove(index), result)),
                    None => index += 1,
                }
            }
            done
        };
        for (draft, result) in done {
            draft.edit.cleanup();
            self.draft_written(draft, result);
        }
        self.show_written_views();
        !self.inflight.borrow().is_empty() || !self.drafts.borrow().is_empty() || !self.views.borrow().is_empty()
    }

    /// Hands the editor each view whose file is written, in the order the views came: one written early waits for
    /// the one before it. A failed write, or a worker that stopped, is logged, as a refused request is, and a view
    /// that asked for an answer is answered with how it went.
    fn show_written_views(&self) {
        loop {
            let written = {
                let views = self.views.borrow();
                let Some(view) = views.first() else { return };
                match view.rx.try_recv() {
                    Ok(written) => written.map_err(|e| format!("could not write the scratch file: {e}")),
                    Err(mpsc::TryRecvError::Empty) => return,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        Err("could not write the scratch file: its worker stopped".to_string())
                    }
                }
            };
            let view = self.views.borrow_mut().remove(0);
            let drive = self.drive.borrow().clone();
            let shown = written.and_then(|request| {
                let drive = drive.ok_or("this window has no editor to drive")?;
                drive(EditorAct::Request(&request), view.token)
            });
            if let Err(why) = &shown {
                eprintln!("[link] another window's view was not shown: {why}");
            }
            // The view opened either way, as a draft still out does; only its answer is tied to the link it came on:
            // a window opened again under the same peer has ids of its own.
            if let Some((peer, id)) = view.answer {
                let now = self.links.borrow().get(&peer).map(|state| state.generation);
                if now == view.asked_on {
                    self.send(peer, Msg::EditorShown { id, why: shown.err() });
                }
            }
        }
    }

    /// A draft edit ended: the tab's holder now hears how, found by its session. A tab that came to this window
    /// meanwhile takes it here; otherwise the window that asked does (a tab with no session yet has not moved).
    fn draft_written(&self, draft: OutDraft, result: EditDone) {
        let here = draft
            .session
            .as_deref()
            .is_some_and(|session| (self.own_session.borrow())(session));
        if here {
            let hook = self.draft_here.borrow().clone();
            match hook {
                Some(hook) => hook(draft.session, result),
                None => eprintln!("[scratch] a draft edit for a chat now in this window has nowhere to go"),
            }
            return;
        }
        // A window opened again under the same peer is not the one that asked: its tab of the same number holds
        // another chat's draft.
        let gone = self
            .links
            .borrow()
            .get(&draft.peer)
            .is_none_or(|state| !state.link.linked() || Some(state.generation) != draft.asked_on);
        if gone {
            let window = self.window_text(draft.peer);
            eprintln!(
                "[scratch] a draft written for {window}'s tab {} is lost: that window is gone",
                draft.tab
            );
            return;
        }
        let (text, end) = match result {
            EditDone::Written(text) => (text, DraftEnd::Written),
            EditDone::Discarded => (String::new(), DraftEnd::Discarded),
            EditDone::Failed(why) => (String::new(), DraftEnd::Failed { why }),
        };
        self.draft_back(draft.peer, draft.tab, draft.session, text, end);
    }
}

/// What `dispatch` asks window 1 about one operation, answered from window 1's own records for the window at `peer`,
/// whose operation it is.
struct Ctx<'a> {
    one: &'a Window1Editor,
    peer: PeerId,
}

impl Ctx<'_> {
    /// The window that holds `session`. A session one of this window's tabs holds is no other window's; any other
    /// that a window names is taken as one of its own (a chat started there): this window keeps no record of which
    /// sessions the others hold, so the one refusal it can make is for a session it holds itself.
    fn session_window(&self, session: &str) -> Option<PeerId> {
        (!(self.one.own_session.borrow())(session)).then_some(self.peer)
    }
}

impl OpContext for Ctx<'_> {
    fn project_root(&self) -> &Path {
        &self.one.root
    }

    fn peer(&self) -> PeerId {
        self.peer
    }

    fn session_in_window2(&self, session: &str) -> bool {
        self.session_window(session) == Some(self.peer)
    }

    fn plugin_follow(&self, plugin: &str) -> Option<eitri_core::window_link::editor_ops::FollowConfig> {
        self.one.plugin_follows.borrow().get(plugin).cloned()
    }

    /// The roots only for the window that holds the page: a page's calls come from the page.
    fn plugin_roots_in_window2(&self, plugin: &str) -> Option<BTreeMap<String, PathBuf>> {
        let plugins = self.one.plugins.borrow();
        let page = plugins.get(plugin)?;
        (page.peer == self.peer).then(|| page.roots.clone())
    }

    /// What the review driver drew for the asking window, and for no other.
    fn drawn_for_window2(&self) -> Vec<PathBuf> {
        let drawn = self.one.drawn_for.borrow().clone();
        drawn.map(|drawn| drawn(self.peer)).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Window 2.

/// Window 2's editor operations: each one an [`EditorOp`] sent to window 1, its answer a [`Pending`] the caller
/// reads as it reads an answer of its own editor (the same `rmpv::Value`), within [`OP_DEADLINE`]. With no link up
/// the answer is `Unavailable` at once and nothing is sent.
pub struct EditorOps {
    me: Weak<EditorOps>,
    repeat: Repeat,
    link: RefCell<Option<LinkDriver>>,
    next: Cell<u64>,
    waiting: RefCell<BTreeMap<u64, (Answer, Instant)>>,
    expiring: Cell<bool>,
}

impl EditorOps {
    pub fn new(repeat: Repeat) -> Rc<EditorOps> {
        Rc::new_cyclic(|me| EditorOps {
            me: me.clone(),
            repeat,
            link: RefCell::default(),
            next: Cell::new(1),
            waiting: RefCell::default(),
            expiring: Cell::new(false),
        })
    }

    pub fn send(&self, op: EditorOp) -> Pending {
        self.send_with_id(op).1
    }

    /// [`send`](Self::send), with the id the operation went under, which window 1's later reports about it
    /// (`review_drawn`) name. An id is given while unlinked too, though nothing is sent.
    pub fn send_with_id(&self, op: EditorOp) -> (u64, Pending) {
        self.send_within(op, OP_DEADLINE)
    }

    /// The way a window-2 tab's editor tools reach window 1 through these operations.
    pub fn editor_way(self: &Rc<Self>, root: &Path) -> RemoteWay {
        let ops = self.clone();
        RemoteWay::new(root, Box::new(move |op| ops.send_within(op, AGENT_OP_DEADLINE).1))
    }

    fn send_within(&self, op: EditorOp, deadline: Duration) -> (u64, Pending) {
        let id = self.next.get();
        self.next.set(id + 1);
        let link = self.link.borrow().clone();
        let Some(link) = link.filter(LinkDriver::linked) else {
            return (id, Pending::failed(RpcError::Unavailable(UNLINKED_NOTE.into())));
        };
        let (answer, pending) = Pending::pair();
        if let Err(err) = link.send(Msg::EditorOp { id, op }, Vec::new()) {
            return (id, Pending::failed(RpcError::Encode(err.to_string())));
        }
        self.waiting
            .borrow_mut()
            .insert(id, (answer, Instant::now() + deadline));
        self.expire_soon();
        (id, pending)
    }

    /// Whether an operation sent now reaches window 1.
    pub fn is_linked(&self) -> bool {
        self.link.borrow().as_ref().is_some_and(LinkDriver::linked)
    }

    fn expire_soon(&self) {
        if self.expiring.replace(true) {
            return;
        }
        let me = self.me.clone();
        (self.repeat)(
            Duration::from_millis(250),
            Box::new(move || {
                let Some(me) = me.upgrade() else { return false };
                let more = me.expire(Instant::now());
                if !more {
                    me.expiring.set(false);
                }
                more
            }),
        );
    }

    /// Fails every operation whose deadline is past at `now`; whether any is still waiting.
    pub fn expire(&self, now: Instant) -> bool {
        let late: Vec<Answer> = {
            let mut waiting = self.waiting.borrow_mut();
            let ids: Vec<u64> = waiting
                .iter()
                .filter(|(_, (_, by))| *by <= now)
                .map(|(id, _)| *id)
                .collect();
            ids.iter().filter_map(|id| waiting.remove(id)).map(|(a, _)| a).collect()
        };
        for answer in late {
            answer.send(Err(RpcError::Unavailable(NO_ANSWER.into())));
        }
        !self.waiting.borrow().is_empty()
    }

    fn answered(&self, id: u64, result: ReplyResult) {
        let Some((answer, _)) = self.waiting.borrow_mut().remove(&id) else {
            println!("[link] an answer to editor operation {id}, which is no longer waited for");
            return;
        };
        answer.send(match result {
            ReplyResult::Ok { value } => json_to_msgpack(&value).map_err(RpcError::Nvim),
            ReplyResult::Err { why } => Err(RpcError::Nvim(why)),
        });
    }

    fn linked(&self, link: &LinkDriver) {
        *self.link.borrow_mut() = Some(link.clone());
        let me = self.me.clone();
        link.on("editor_reply", move |_, msg, _| {
            if let (Some(me), Msg::EditorReply { id, result }) = (me.upgrade(), msg) {
                me.answered(id, result);
            }
        });
    }

    fn unlinked(&self) {
        self.link.borrow_mut().take();
        let waiting = std::mem::take(&mut *self.waiting.borrow_mut());
        for (_, (answer, _)) in waiting {
            answer.send(Err(RpcError::Unavailable(UNLINKED_NOTE.into())));
        }
    }
}

/// A draft edit window 2's panel has out in window 1's editor: its tab, and the files the panel polls.
struct WaitingDraft {
    tab: u64,
    session: Option<String>,
    body: PathBuf,
    done: PathBuf,
}

type Consumer<T> = RefCell<Option<Rc<dyn Fn(T)>>>;

/// A plugin host in window 2 translates its fixed local calls into typed requests; no Lua crosses the link.
pub(crate) struct PluginEditorRpc {
    owner: String,
    roots: eitri_core::plugin::roots::Roots,
    ops: Rc<EditorOps>,
}

impl PluginEditorRpc {
    pub(crate) fn new(owner: String, roots: eitri_core::plugin::roots::Roots, ops: Rc<EditorOps>) -> Self {
        Self { owner, roots, ops }
    }

    fn operation(&self, code: &'static str, args: &[rmpv::Value]) -> Result<EditorOp, String> {
        use eitri_core::plugin::editor::{PLUGIN_ASSIST_LUA, PLUGIN_FOLLOW_LUA, PLUGIN_OPEN_LUA};
        let plugin = self.owner.clone();
        let map = args
            .iter()
            .find_map(rmpv::Value::as_map)
            .ok_or("the plugin call has no fields")?;
        let get = |name: &str| {
            map.iter()
                .find(|(key, _)| key.as_str() == Some(name))
                .map(|(_, value)| value)
        };
        let number = |name: &str| {
            get(name)
                .and_then(rmpv::Value::as_u64)
                .ok_or_else(|| format!("missing {name}"))
        };
        let boolean = |name: &str| {
            get(name)
                .and_then(rmpv::Value::as_bool)
                .ok_or_else(|| format!("missing {name}"))
        };
        let line = || {
            get("line")
                .and_then(rmpv::Value::as_u64)
                .map(u32::try_from)
                .transpose()
                .map_err(|e| e.to_string())
        };
        if std::ptr::eq(code, PLUGIN_OPEN_LUA) {
            let value = get("path").ok_or("missing path")?;
            let bytes = value
                .as_slice()
                .or_else(|| value.as_str().map(str::as_bytes))
                .ok_or("invalid path")?;
            let path = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
            if let Some((name, root)) = self.roots.iter().find(|(_, root)| Path::new(path).starts_with(root)) {
                let path = Path::new(path)
                    .strip_prefix(root)
                    .map_err(|e| e.to_string())?
                    .to_str()
                    .ok_or("path is not UTF-8")?
                    .to_owned();
                return Ok(EditorOp::PluginOpen {
                    plugin,
                    root: name.clone(),
                    path,
                    line: line()?,
                    col: get("col")
                        .and_then(rmpv::Value::as_u64)
                        .map(u32::try_from)
                        .transpose()
                        .map_err(|e| e.to_string())?,
                });
            }
            return Ok(EditorOp::PluginOpenLink {
                plugin,
                path: path.into(),
                line: line()?,
            });
        }
        if std::ptr::eq(code, PLUGIN_ASSIST_LUA) {
            return match args.first().and_then(rmpv::Value::as_str) {
                Some("apply") => Ok(EditorOp::PluginAssistApply {
                    plugin,
                    diagnostics: boolean("diagnostics")?,
                    inlay_hints: boolean("inlay_hints")?,
                }),
                Some("clear") => Ok(EditorOp::PluginAssistClear { plugin }),
                _ => Err("unknown assist operation".into()),
            };
        }
        if std::ptr::eq(code, PLUGIN_FOLLOW_LUA) {
            let buf = || number("buf");
            return match args.first().and_then(rmpv::Value::as_str) {
                Some("install") => Ok(EditorOp::PluginInstall { plugin }),
                Some("follow") => Ok(EditorOp::PluginFollow {
                    plugin,
                    on: boolean("on")?,
                }),
                Some("text") => Ok(EditorOp::PluginText { plugin, buf: buf()? }),
                Some("reveal") => Ok(EditorOp::PluginReveal {
                    plugin,
                    buf: buf()?,
                    line: line()?.ok_or("missing line")?,
                    token: number("token")?,
                    cursor: get("cursor")
                        .and_then(rmpv::Value::as_u64)
                        .map(u32::try_from)
                        .transpose()
                        .map_err(|e| e.to_string())?,
                    row: get("row")
                        .and_then(rmpv::Value::as_u64)
                        .map(u32::try_from)
                        .transpose()
                        .map_err(|e| e.to_string())?,
                }),
                Some("jump") => Ok(EditorOp::PluginJump {
                    plugin,
                    buf: buf()?,
                    line: line()?.ok_or("missing line")?,
                    col: u32::try_from(number("col")?).map_err(|e| e.to_string())?,
                    select: match get("select_line") {
                        None => None,
                        Some(_) => Some(eitri_core::plugin::protocol::JumpSelect {
                            line: u32::try_from(number("select_line")?).map_err(|e| e.to_string())?,
                            col: u32::try_from(number("select_col")?).map_err(|e| e.to_string())?,
                            mode: match get("select_mode").and_then(rmpv::Value::as_str) {
                                Some("char") => eitri_core::plugin::protocol::SelectMode::Char,
                                Some("line") => eitri_core::plugin::protocol::SelectMode::Line,
                                _ => return Err("a jump's select_mode is char or line".into()),
                            },
                        }),
                    },
                }),
                // The snippet and its commands stay window 1's: a page ending here only stops following.
                Some("teardown") => Ok(EditorOp::PluginFollow { plugin, on: false }),
                _ => Err("unknown follow operation".into()),
            };
        }
        Err("the plugin call is outside the editor operation table".into())
    }
}

impl eitri_core::editor_rpc::EditorRpc for PluginEditorRpc {
    fn exec_lua(&self, code: &'static str, args: Vec<rmpv::Value>) -> Pending {
        match self.operation(code, &args) {
            Ok(op) => self.ops.send(op),
            Err(why) => Pending::failed(RpcError::Encode(why)),
        }
    }
    fn target(&self) -> Option<u64> {
        self.ops.is_linked().then_some(1)
    }
}

/// Window 2's side: the context, theme and keys window 1 tells it, its editor operations, and its editor requests.
pub struct Window2Editor {
    me: Weak<Window2Editor>,
    root: PathBuf,
    scratch: Option<PathBuf>,
    link: RefCell<Option<LinkDriver>>,
    context: Rc<RefCell<Option<EditorContext>>>,
    on_context: RefCell<Option<Rc<dyn Fn()>>>,
    theme: Consumer<NvimThemePayload>,
    cell_height: Consumer<f32>,
    keys: Consumer<NvimReport>,
    follow: Consumer<FollowBatch>,
    last_theme: RefCell<ThemeRecord>,
    draft_tab: RefCell<Option<DraftTab>>,
    drafts: RefCell<Vec<WaitingDraft>>,
    raise: RefCell<Option<Raise>>,
    ops: Rc<EditorOps>,
    repeat: Repeat,
    /// Views whose text is being read back on a worker, oldest first: each goes to window 1 once read, in the
    /// order they were asked for.
    views: RefCell<Vec<ReadingView>>,
    polling: Cell<bool>,
    /// Views sent to window 1 whose outcome the panel waits for, by the id window 1 answers with.
    answers: RefCell<BTreeMap<u64, mpsc::Sender<Result<(), String>>>>,
    next_answer: Cell<u64>,
}

/// A view on its way to window 1: its file in this window's scratch directory is read on a worker thread, since
/// the text may be a megabyte and the GTK thread never waits for a disk.
struct ReadingView {
    rx: mpsc::Receiver<std::io::Result<String>>,
    title: String,
    tab: bool,
    token: Option<String>,
    /// Where the outcome goes when the panel waits for it: window 1's answer, or why the view never reached it.
    answer: Option<mpsc::Sender<Result<(), String>>>,
}

impl Window2Editor {
    /// `scratch` is window 2's own scratch directory, where its panel writes a draft and polls for its end.
    pub fn new(root: &Path, scratch: Option<PathBuf>, repeat: Repeat) -> Rc<Window2Editor> {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        Rc::new_cyclic(|me| Window2Editor {
            me: me.clone(),
            root,
            scratch,
            link: RefCell::default(),
            context: Rc::default(),
            on_context: RefCell::default(),
            theme: RefCell::default(),
            cell_height: RefCell::default(),
            keys: RefCell::default(),
            follow: RefCell::default(),
            last_theme: RefCell::default(),
            draft_tab: RefCell::default(),
            drafts: RefCell::default(),
            raise: RefCell::default(),
            ops: EditorOps::new(repeat.clone()),
            repeat,
            views: RefCell::default(),
            polling: Cell::new(false),
            answers: RefCell::default(),
            next_answer: Cell::new(1),
        })
    }

    /// What the panel reads at send time: window 1's editor context while linked, `None` otherwise.
    pub fn context_source(&self) -> ContextSource {
        let context = self.context.clone();
        Rc::new(move || context.borrow().clone())
    }

    /// Called whenever the context source answers something new.
    pub fn on_context(&self, hook: Rc<dyn Fn()>) {
        *self.on_context.borrow_mut() = Some(hook);
    }

    pub fn on_theme(&self, hook: Rc<dyn Fn(NvimThemePayload)>) {
        *self.theme.borrow_mut() = Some(hook);
    }

    pub fn on_cell_height(&self, hook: Rc<dyn Fn(f32)>) {
        *self.cell_height.borrow_mut() = Some(hook);
    }

    pub fn on_keys(&self, hook: Rc<dyn Fn(NvimReport)>) {
        *self.keys.borrow_mut() = Some(hook);
    }

    pub fn on_follow(&self, hook: Rc<dyn Fn(FollowBatch)>) {
        *self.follow.borrow_mut() = Some(hook);
    }

    /// Which tab, holding which session, a draft edit of the panel is out for.
    pub fn set_draft_tab(&self, find: DraftTab) {
        *self.draft_tab.borrow_mut() = Some(find);
    }

    /// How this window comes forward when a draft's edit ended in window 1's editor, which had the keys meanwhile.
    pub fn set_raise(&self, raise: Raise) {
        *self.raise.borrow_mut() = Some(raise);
    }

    pub fn ops(&self) -> Rc<EditorOps> {
        self.ops.clone()
    }

    fn set_context(&self, context: Option<EditorContext>) {
        let changed = *self.context.borrow() != context;
        *self.context.borrow_mut() = context;
        if changed {
            let hook = self.on_context.borrow().clone();
            if let Some(hook) = hook {
                hook();
            }
        }
    }

    pub fn linked(&self, link: &LinkDriver) {
        *self.link.borrow_mut() = Some(link.clone());
        self.ops.linked(link);
        let me = self.me.clone();
        link.on("context", move |_, msg, _| {
            if let (Some(me), Msg::Context { payload }) = (me.upgrade(), msg) {
                me.set_context(context_of(payload.as_ref()));
            }
        });
        let me = self.me.clone();
        link.on("theme", move |_, msg, _| {
            if let (Some(me), Msg::Theme { payload }) = (me.upgrade(), msg) {
                me.take_theme(&payload);
            }
        });
        let me = self.me.clone();
        link.on("keys", move |_, msg, _| {
            let (Some(me), Msg::Keys { payload }) = (me.upgrade(), msg) else {
                return;
            };
            match keys_of(&payload) {
                Ok(report) => {
                    let hook = me.keys.borrow().clone();
                    if let Some(hook) = hook {
                        hook(report);
                    }
                }
                Err(err) => eprintln!("[link] nvim's keys from window 1 do not parse ({err}); the last ones stay"),
            }
        });
        let me = self.me.clone();
        link.on("draft_back", move |_, msg, _| {
            if let (
                Some(me),
                Msg::DraftBack {
                    tab,
                    session,
                    text,
                    end,
                    token,
                },
            ) = (me.upgrade(), msg)
            {
                me.draft_back(tab, session.as_deref(), &text, &end, token);
            }
        });
        let me = self.me.clone();
        link.on("editor_shown", move |_, msg, _| {
            if let (Some(me), Msg::EditorShown { id, why }) = (me.upgrade(), msg) {
                let waiting = me.answers.borrow_mut().remove(&id);
                if let Some(waiting) = waiting {
                    let _ = waiting.send(why.map_or(Ok(()), Err));
                }
            }
        });
        let me = self.me.clone();
        link.on("follow", move |_, msg, _| {
            let (Some(me), Msg::Follow { owner, line }) = (me.upgrade(), msg) else {
                return;
            };
            let Some(batch) = follow_batch(&owner, &line) else {
                eprintln!("[link] a follow line for {owner} from window 1 does not parse; dropped");
                return;
            };
            let hook = me.follow.borrow().clone();
            if let Some(hook) = hook {
                hook(batch);
            }
        });
    }

    /// The link ended: no context, operations answer `Unavailable`, and a draft still out ends with the reason, its
    /// tab keeping the text it had. The theme and keys keep their last values.
    pub fn unlinked(&self) {
        self.link.borrow_mut().take();
        self.ops.unlinked();
        // A view window 1 had not answered for has no answer coming: the panel hears that the link ended.
        for (_, waiting) in std::mem::take(&mut *self.answers.borrow_mut()) {
            let _ = waiting.send(Err(UNLINKED_NOTE.to_string()));
        }
        self.set_context(None);
        let drafts = std::mem::take(&mut *self.drafts.borrow_mut());
        for draft in drafts {
            if let Err(err) = write_marker(&draft.done, &format!("error: {UNLINKED_NOTE}")) {
                eprintln!("[scratch] the draft of tab {} could not be ended: {err}", draft.tab);
            }
        }
    }

    fn take_theme(&self, payload: &Value) {
        let record = match ThemeRecord::from_value(payload) {
            Ok(record) => record,
            Err(err) => {
                eprintln!("[link] the theme from window 1 does not parse ({err}); the last one stays");
                return;
            }
        };
        let last = std::mem::replace(&mut *self.last_theme.borrow_mut(), record.clone());
        if let Some(payload) = record.payload.filter(|p| last.payload.as_ref() != Some(p)) {
            let hook = self.theme.borrow().clone();
            if let Some(hook) = hook {
                hook(payload);
            }
        }
        if let Some(height) = record.cell_height.filter(|h| last.cell_height != Some(*h)) {
            let hook = self.cell_height.borrow().clone();
            if let Some(hook) = hook {
                hook(height);
            }
        }
    }

    /// The panel's request for nvim, sent to window 1 as an editor request with `token` (an activation token, so
    /// window 1 may come forward). `Err` says why it was not sent, for the panel to show.
    pub fn request(&self, request: &ScratchRequest, token: &dyn Fn() -> Option<String>) -> Result<(), String> {
        self.ask(request, token, false).map(|_| ())
    }

    /// [`request`](Self::request), with a view's outcome to come: `Ok(Some(later))` hears window 1's answer once it
    /// has written the view and handed it to its editor, or why the view never got there. Every other request is
    /// `Ok(None)` once sent.
    pub fn request_answered(
        &self,
        request: &ScratchRequest,
        token: &dyn Fn() -> Option<String>,
    ) -> Result<Option<mpsc::Receiver<Result<(), String>>>, String> {
        self.ask(request, token, true)
    }

    fn ask(
        &self,
        request: &ScratchRequest,
        token: &dyn Fn() -> Option<String>,
        answered: bool,
    ) -> Result<Option<mpsc::Receiver<Result<(), String>>>, String> {
        let link = self.link.borrow().clone();
        let Some(link) = link.filter(LinkDriver::linked) else {
            return Err(UNLINKED_NOTE.to_string());
        };
        let (request, waiting) = match read_request(request)? {
            ScratchAsk::Open { path, line } => (
                EditorRequest::OpenPath {
                    path: named_path(&self.root, &path),
                    line,
                    col: None,
                },
                None,
            ),
            ScratchAsk::Edit { body, done } => {
                let ours = self.scratch.as_deref().is_some_and(|dir| {
                    eitri_core::scratch::holds(dir, &body.to_string_lossy())
                        && eitri_core::scratch::holds(dir, &done.to_string_lossy())
                });
                if !ours {
                    return Err("the draft is not in this window's scratch directory".into());
                }
                let find = self.draft_tab.borrow().clone();
                let (tab, session) = edit_id(&body)
                    .and_then(|id| find.and_then(|find| find(id)))
                    .ok_or("no tab is waiting for this draft")?;
                let text = std::fs::read_to_string(&body).map_err(|e| format!("could not read the draft: {e}"))?;
                (
                    EditorRequest::Scratch {
                        tab: tab.0,
                        session: session.clone(),
                        text,
                    },
                    Some(WaitingDraft {
                        tab: tab.0,
                        session,
                        body,
                        done,
                    }),
                )
            }
            ScratchAsk::View { body, tab } => {
                // Checked here, read on a worker: `poll_views` sends the text once it is read.
                self.own_scratch_file(&body)?;
                let (tx, rx) = mpsc::channel();
                let path = body.clone();
                std::thread::Builder::new()
                    .name("linked view read".into())
                    .spawn(move || {
                        let _ = tx.send(std::fs::read_to_string(&path));
                    })
                    .map_err(|e| format!("could not read {}: {e}", body.display()))?;
                let (answer, later) = if answered {
                    let (tx, rx) = mpsc::channel();
                    (Some(tx), Some(rx))
                } else {
                    (None, None)
                };
                self.views.borrow_mut().push(ReadingView {
                    rx,
                    title: view_title(&body),
                    tab,
                    token: token(),
                    answer,
                });
                self.poll_views_soon();
                return Ok(later);
            }
            ScratchAsk::Diff { base, proposed, name } => (
                EditorRequest::Diff {
                    base: self.own_scratch_text(&base)?,
                    proposed: self.own_scratch_text(&proposed)?,
                    name,
                },
                None,
            ),
        };
        link.send(
            Msg::Editor {
                request,
                token: token(),
                answer: None,
            },
            Vec::new(),
        )
        .map_err(|e| format!("the editor window could not be asked: {e}"))?;
        if let Some(waiting) = waiting {
            self.drafts.borrow_mut().push(waiting);
        }
        Ok(None)
    }

    /// The text of a file the panel wrote for the editor, read only from this window's own scratch directory: what
    /// goes to window 1 is the text, never the file's name.
    fn own_scratch_text(&self, path: &Path) -> Result<String, String> {
        self.own_scratch_file(path)?;
        std::fs::read_to_string(path).map_err(|e| format!("could not read {}: {e}", path.display()))
    }

    /// Whether `path` is a file of this window's own scratch directory, reached by no way that climbs out of it.
    fn own_scratch_file(&self, path: &Path) -> Result<(), String> {
        let ours = self
            .scratch
            .as_deref()
            .is_some_and(|dir| eitri_core::scratch::holds(dir, &path.to_string_lossy()));
        let climbs = path.components().any(|part| part == std::path::Component::ParentDir);
        if !ours || climbs {
            return Err("the file is not in this window's scratch directory".into());
        }
        Ok(())
    }

    fn poll_views_soon(&self) {
        if self.polling.replace(true) {
            return;
        }
        let me = self.me.clone();
        (self.repeat)(
            POLL_EVERY,
            Box::new(move || {
                let Some(me) = me.upgrade() else { return false };
                let more = me.poll_views();
                if !more {
                    me.polling.set(false);
                }
                more
            }),
        );
    }

    /// Sends window 1 each view whose text is read, in the order they were asked for. A read that failed, or a link
    /// that ended meanwhile, is logged, and told to a panel waiting for the view's outcome; one sent asks window 1
    /// to answer how it went. Whether any is still being read.
    fn poll_views(&self) -> bool {
        loop {
            let read = {
                let views = self.views.borrow();
                let Some(view) = views.first() else { return false };
                match view.rx.try_recv() {
                    Ok(read) => read.map_err(|e| format!("could not read the view's file: {e}")),
                    Err(mpsc::TryRecvError::Empty) => return true,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        Err("could not read the view's file: its worker stopped".to_string())
                    }
                }
            };
            let view = self.views.borrow_mut().remove(0);
            let link = self.link.borrow().clone().filter(LinkDriver::linked);
            let id = view.answer.as_ref().map(|_| {
                let id = self.next_answer.get();
                self.next_answer.set(id + 1);
                id
            });
            let sent = read.and_then(|text| {
                let link = link.ok_or_else(|| UNLINKED_NOTE.to_string())?;
                link.send(
                    Msg::Editor {
                        request: EditorRequest::View {
                            title: view.title,
                            text,
                            tab: view.tab,
                        },
                        token: view.token,
                        answer: id,
                    },
                    Vec::new(),
                )
                .map_err(|e| format!("the editor window could not be asked: {e}"))
            });
            match (sent, view.answer, id) {
                (Ok(()), Some(waiting), Some(id)) => {
                    self.answers.borrow_mut().insert(id, waiting);
                }
                (Ok(()), _, _) => {}
                (Err(why), waiting, _) => {
                    eprintln!("[link] a view for the editor window was not sent: {why}");
                    if let Some(waiting) = waiting {
                        let _ = waiting.send(Err(why));
                    }
                }
            }
        }
    }

    /// Asks window 1 to come forward and give its editor the keys, with `token` (an activation token): what this
    /// window does once a file the review opened is in window 1's editor.
    pub fn focus_editor(&self, token: Option<String>) -> Result<(), String> {
        let link = self.link.borrow().clone();
        let Some(link) = link.filter(LinkDriver::linked) else {
            return Err(UNLINKED_NOTE.to_string());
        };
        link.send(
            Msg::Editor {
                request: EditorRequest::Focus,
                token,
                answer: None,
            },
            Vec::new(),
        )
        .map_err(|e| format!("the editor window could not be asked: {e}"))
    }

    /// A draft edit ended in window 1: its end written into the files the panel polls, so its own end of the edit
    /// takes it as it takes one from its own editor. Found by session; by the tab's number only when neither the
    /// answer nor the waiting draft has a session, since a draft for one chat must never land in another's.
    fn draft_back(&self, tab: u64, session: Option<&str>, text: &str, end: &DraftEnd, token: Option<String>) {
        let waiting = {
            let mut drafts = self.drafts.borrow_mut();
            let at = drafts.iter().position(|d| match (session, d.session.as_deref()) {
                (Some(back), Some(out)) => back == out,
                (None, None) => d.tab == tab,
                _ => false,
            });
            at.map(|at| drafts.remove(at))
        };
        let Some(waiting) = waiting else {
            eprintln!("[scratch] a draft came back for tab {tab}, which has none out");
            return;
        };
        let written = match end {
            DraftEnd::Written => {
                std::fs::write(&waiting.body, format!("{text}\n")).and_then(|()| write_marker(&waiting.done, "written"))
            }
            DraftEnd::Discarded => write_marker(&waiting.done, "discarded"),
            DraftEnd::Failed { why } => write_marker(&waiting.done, &format!("error: {}", one_line(why))),
        };
        if let Err(err) = written {
            eprintln!("[scratch] the draft for tab {tab} could not be handed back: {err}");
        }
        // The edit ended in window 1's editor, which has the keys: this window comes forward for its panel to give
        // them back to the chat.
        let raise = self.raise.borrow().clone();
        if let Some(raise) = raise {
            raise(token);
        }
    }
}

/// `why` as a marker's reason: on one line, and never empty, since an empty marker reads as one still being
/// written.
fn one_line(why: &str) -> String {
    let line = why.split(['\n', '\r']).collect::<Vec<_>>().join(" ");
    if line.trim().is_empty() {
        "the edit failed in the editor".to_string()
    } else {
        line
    }
}

/// The marker the panel polls, written whole: a marker read mid-write must never be taken as the end.
fn write_marker(done: &Path, text: &str) -> std::io::Result<()> {
    let tmp = done.with_extension("done.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linked_window::spawn::SECOND_PEER;
    use crate::linked_window::tests::Scratch;
    use agent::setting_sources::ProjectTrust;
    use agent::PermissionMode;
    use eitri_core::editor_context::Selection;
    use eitri_core::editor_tools::broker::{Broker, BrokerEnv, BrokerOut, Caller, TabFacts};
    use eitri_core::editor_tools::stamp::{Stamp, EDITOR_BUDGET};
    use eitri_core::editor_tools::step::ToolResult;
    use eitri_core::editor_tools::tools::baseline::{BASELINE_LIMIT, NO_BASELINE};
    use eitri_core::editor_tools::tools::diagnostics::DIAGNOSTICS_LUA;
    use eitri_core::editor_tools::tools::lsp::{LSP_ASK_LUA, LSP_COLLECT_LUA};
    use eitri_core::editor_tools::tools::open_file::OPEN_FILE_LUA;
    use eitri_core::editor_tools::way::{EditorCall, EditorWay};
    use eitri_core::editor_tools::words::WINDOW_CLOSED;
    use eitri_core::window_link::fdpass;
    use eitri_core::window_link::link::{LinkHandle, PresenceFlag};

    /// Steps the windows' repeating work by hand.
    #[derive(Clone, Default)]
    struct Steps(Rc<RefCell<Vec<Box<dyn Fn() -> bool>>>>);

    impl Steps {
        fn repeat(&self) -> Repeat {
            let steps = self.0.clone();
            Rc::new(move |_: Duration, step: Box<dyn Fn() -> bool>| steps.borrow_mut().push(step))
        }

        fn run(&self) {
            let steps = std::mem::take(&mut *self.0.borrow_mut());
            let kept: Vec<_> = steps.into_iter().filter(|step| step()).collect();
            self.0.borrow_mut().extend(kept);
        }
    }

    /// Two linked ends over a real socketpair: window 1's and window 2's.
    fn linked_pair() -> (LinkDriver, LinkDriver) {
        let (a, b) = fdpass::seqpacket_pair().unwrap();
        let (one, one_in) = LinkHandle::start(a, PresenceFlag::default()).unwrap();
        let (two, two_in) = LinkHandle::start(b, PresenceFlag::default()).unwrap();
        let (one, two) = (LinkDriver::new(one, one_in), LinkDriver::new(two, two_in));
        one.set_linked();
        two.set_linked();
        (one, two)
    }

    /// Pumps both ends and steps the windows until `done` holds, for at most five seconds.
    fn settle(one: &LinkDriver, two: &LinkDriver, steps: &Steps, done: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            one.pump();
            two.pump();
            steps.run();
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// Pumps both ends for a while, for checking that nothing arrives.
    fn pump_a_while(one: &LinkDriver, two: &LinkDriver, steps: &Steps) {
        for _ in 0..20 {
            one.pump();
            two.pump();
            steps.run();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn context(file: &str) -> EditorContext {
        EditorContext {
            file: file.into(),
            selection: Some(Selection {
                start_line: 2,
                end_line: 3,
                text: "two\nthree".into(),
            }),
            edit_request: None,
        }
    }

    fn theme(background: &str) -> NvimThemePayload {
        let mut payload = NvimThemePayload::empty();
        payload.options.background = background.into();
        payload.groups.insert(
            "Normal".into(),
            eitri_core::theme::payload::HlAttrs {
                fg: Some(1),
                bg: Some(2),
                reverse: false,
            },
        );
        payload
    }

    fn keys(timeoutlen: u32) -> NvimReport {
        eitri_core::nvim_keys::parse_report(
            format!(
                r#"{{"v":1,"mapleader":"<Space>","timeoutlen":{timeoutlen},"timeout":true,"maps":[{{"lhs":"<Space>f","rhs":":Files<CR>","desc":null}}]}}"#
            )
            .as_bytes(),
        )
        .unwrap()
    }

    struct Windows {
        _dir: Scratch,
        root: PathBuf,
        steps: Steps,
        one_link: LinkDriver,
        two_link: LinkDriver,
        one: Rc<Window1Editor>,
        two: Rc<Window2Editor>,
    }

    /// A project with `src/a.rs`, both windows' scratch directories, and the two windows linked.
    fn windows(label: &str) -> Windows {
        let dir = Scratch::new(label);
        let base = std::fs::canonicalize(&dir.1).unwrap();
        let root = base.join("project");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn a() {}\n").unwrap();
        for scratch in ["scratch-1", "scratch-2"] {
            std::fs::create_dir_all(base.join(scratch)).unwrap();
        }
        let steps = Steps::default();
        let one = Window1Editor::new(&root, Some(base.join("scratch-1")), steps.repeat());
        let two = Window2Editor::new(&root, Some(base.join("scratch-2")), steps.repeat());
        let (one_link, two_link) = linked_pair();
        one.linked(SECOND_PEER, WindowName::Second, &one_link);
        two.linked(&two_link);
        Windows {
            _dir: dir,
            root,
            steps,
            one_link,
            two_link,
            one,
            two,
        }
    }

    impl Windows {
        fn settle(&self, done: impl Fn() -> bool) -> bool {
            settle(&self.one_link, &self.two_link, &self.steps, done)
        }

        fn pump_a_while(&self) {
            pump_a_while(&self.one_link, &self.two_link, &self.steps);
        }
    }

    #[test]
    fn a_context_change_in_window_1_reaches_window_2s_context_source() {
        let w = windows("context");
        let source = w.two.context_source();
        let woken = Rc::new(Cell::new(0));
        {
            let woken = woken.clone();
            w.two.on_context(Rc::new(move || woken.set(woken.get() + 1)));
        }
        assert_eq!(source(), None, "window 2 starts with no context");

        w.one.context_changed(Some(&context("/p/a.rs")));
        assert!(w.settle(|| source().is_some()));
        assert_eq!(
            source(),
            Some(context("/p/a.rs")),
            "the selection travels with the file"
        );
        assert_eq!(woken.get(), 1, "window 2's panel is woken for it");

        w.one.context_changed(None);
        assert!(
            w.settle(|| source().is_none()),
            "a context gone in window 1 is gone in window 2"
        );

        w.one.context_changed(Some(&context("/p/b.rs")));
        assert!(w.settle(|| source().is_some()));
        w.two.unlinked();
        assert_eq!(source(), None, "an unlinked window 2 has no editor, so no context");
    }

    #[test]
    fn theme_and_keys_payloads_are_forwarded_on_change_only() {
        let w = windows("feeds");
        let themes: Rc<RefCell<Vec<String>>> = Rc::default();
        let cells: Rc<RefCell<Vec<f32>>> = Rc::default();
        let timeouts: Rc<RefCell<Vec<u32>>> = Rc::default();
        {
            let themes = themes.clone();
            w.two.on_theme(Rc::new(move |p: NvimThemePayload| {
                themes.borrow_mut().push(p.options.background)
            }));
            let cells = cells.clone();
            w.two.on_cell_height(Rc::new(move |h: f32| cells.borrow_mut().push(h)));
            let timeouts = timeouts.clone();
            w.two
                .on_keys(Rc::new(move |r: NvimReport| timeouts.borrow_mut().push(r.timeoutlen)));
        }
        // The feeds repeat themselves (VimEnter and ColorScheme back to back): each payload goes once.
        w.one.theme_changed(&theme("dark"));
        w.one.theme_changed(&theme("dark"));
        w.one.keys_changed(&keys(500));
        w.one.keys_changed(&keys(500));
        w.one.cell_height_changed(18.0);
        w.one.cell_height_changed(18.0);
        assert!(w.settle(|| !themes.borrow().is_empty() && !timeouts.borrow().is_empty() && !cells.borrow().is_empty()));
        w.pump_a_while();
        assert_eq!(*themes.borrow(), vec!["dark".to_string()]);
        assert_eq!(*timeouts.borrow(), vec![500]);
        assert_eq!(
            *cells.borrow(),
            vec![18.0],
            "the cell height rides with the theme, once"
        );

        w.one.theme_changed(&theme("light"));
        w.one.keys_changed(&keys(300));
        w.one.cell_height_changed(20.0);
        assert!(w.settle(|| themes.borrow().len() == 2 && timeouts.borrow().len() == 2 && cells.borrow().len() == 2));
        assert_eq!(*themes.borrow(), vec!["dark".to_string(), "light".to_string()]);
        assert_eq!(*timeouts.borrow(), vec![500, 300]);
        assert_eq!(
            *cells.borrow(),
            vec![18.0, 20.0],
            "a cell change alone does not repaint the theme"
        );

        // A later window 2 hears the last values at once, though they did not change for it.
        let (one_link, two_link) = linked_pair();
        let later = Window2Editor::new(&w.root, None, w.steps.repeat());
        let later_themes: Rc<RefCell<Vec<String>>> = Rc::default();
        {
            let later_themes = later_themes.clone();
            later.on_theme(Rc::new(move |p: NvimThemePayload| {
                later_themes.borrow_mut().push(p.options.background)
            }));
        }
        w.one.unlinked(SECOND_PEER);
        later.linked(&two_link);
        w.one.linked(SECOND_PEER, WindowName::Second, &one_link);
        assert!(settle(&one_link, &two_link, &w.steps, || !later_themes
            .borrow()
            .is_empty()));
        assert_eq!(*later_themes.borrow(), vec!["light".to_string()]);
        assert_eq!(
            themes.borrow().len(),
            2,
            "the window on the old link heard nothing more"
        );
    }

    /// Window 1's editor for a tab's tools: it records every call and keeps each answer for the test to give.
    #[derive(Default)]
    struct HeldNvim {
        calls: RefCell<Vec<(&'static str, Vec<rmpv::Value>)>>,
        answers: RefCell<Vec<Answer>>,
    }

    impl EditorRpc for HeldNvim {
        fn exec_lua(&self, code: &'static str, args: Vec<rmpv::Value>) -> Pending {
            self.calls.borrow_mut().push((code, args));
            let (answer, pending) = Pending::pair();
            self.answers.borrow_mut().push(answer);
            pending
        }
        fn target(&self) -> Option<u64> {
            Some(1)
        }
    }

    /// One tab of window 2, as its broker sees it: in Bypass, so a load needs no card, and reaching the editor over
    /// the link.
    struct Window2Tab {
        way: RemoteWay,
        root: PathBuf,
    }

    impl BrokerEnv for Window2Tab {
        fn way(&self) -> Option<&dyn EditorWay> {
            Some(&self.way)
        }
        fn facts(&self, _tab: TabId) -> Option<TabFacts> {
            Some(TabFacts {
                mode: PermissionMode::Bypass,
                trust: ProjectTrust::Trusted,
                session: "s-window-2".into(),
                process: "s-window-2".into(),
                revision: 0,
                declared: Rc::new(BTreeSet::from([
                    "open_file".to_owned(),
                    "usages".to_owned(),
                    "diagnostics".to_owned(),
                ])),
            })
        }
        fn root(&self) -> &Path {
            &self.root
        }
        fn home(&self) -> Option<&Path> {
            None
        }
        fn context(&self) -> Option<EditorContext> {
            None
        }
    }

    const TAB: TabId = TabId(3);

    fn window_2_tab(w: &Windows) -> Window2Tab {
        Window2Tab {
            way: w.two.ops().editor_way(&w.root),
            root: w.root.clone(),
        }
    }

    fn open_a_file(broker: &mut Broker, tab: &Window2Tab) -> Vec<BrokerOut> {
        broker.submit(
            TAB,
            Caller::Sidecar { call_id: "c-1".into() },
            Some("t-1".into()),
            "open_file",
            r#"{"path":"src/a.rs","line":1}"#,
            tab,
            Instant::now(),
        )
    }

    fn ask_for_usages(broker: &mut Broker, tab: &Window2Tab) -> Vec<BrokerOut> {
        broker.submit(
            TAB,
            Caller::Sidecar { call_id: "c-2".into() },
            Some("t-2".into()),
            "usages",
            r#"{"path":"src/a.rs","line":1,"symbol":"a"}"#,
            tab,
            Instant::now(),
        )
    }

    fn since_the_turn_began(broker: &mut Broker, tab: &Window2Tab) -> Vec<BrokerOut> {
        broker.submit(
            TAB,
            Caller::Sidecar { call_id: "c-3".into() },
            Some("t-3".into()),
            "diagnostics",
            r#"{"since":"turn"}"#,
            tab,
            Instant::now(),
        )
    }

    /// The `name` argument of the chunk call `at`, as window 1's editor was given it.
    fn chunk_field(nvim: &HeldNvim, at: usize, name: &str) -> Option<rmpv::Value> {
        nvim.calls.borrow()[at].1[1]
            .as_map()
            .unwrap()
            .iter()
            .find(|(key, _)| key.as_str() == Some(name))
            .map(|(_, value)| value.clone())
    }

    /// Ticks the broker while `w` settles, until window 1's editor has been called `count` times.
    fn until_called(w: &Windows, broker: &RefCell<Broker>, tab: &Window2Tab, nvim: &HeldNvim, count: usize) {
        let called = w.settle(|| {
            broker.borrow_mut().tick(tab, Instant::now());
            nvim.calls.borrow().len() >= count
        });
        assert!(
            called,
            "window 1's editor was called {} times",
            nvim.calls.borrow().len()
        );
    }

    /// One entry of the diagnostics chunk's answer, on `src/a.rs`.
    fn diagnostic(w: &Windows, message: &str) -> rmpv::Value {
        use rmpv::Value as M;
        let path = M::Binary(w.root.join("src/a.rs").as_os_str().as_encoded_bytes().to_vec());
        M::Map(vec![
            (M::from("path"), path.clone()),
            (M::from("read_from"), path),
            (M::from("lnum"), M::from(0)),
            (M::from("col"), M::from(0)),
            (M::from("end_lnum"), M::from(0)),
            (M::from("end_col"), M::from(1)),
            (M::from("severity"), M::from("error")),
            (M::from("message"), M::from(message)),
        ])
    }

    fn diagnostics_answer(entries: Vec<rmpv::Value>) -> rmpv::Value {
        use rmpv::Value as M;
        M::Map(vec![
            (M::from("entries"), M::Array(entries)),
            (M::from("modified"), M::Array(vec![])),
            (M::from("left_out"), M::from(0)),
        ])
    }

    /// Ticks the broker while `w` settles, until it answers the call; that answer.
    fn the_answer(w: &Windows, broker: &RefCell<Broker>, tab: &Window2Tab) -> ToolResult {
        let found = RefCell::new(None);
        let answered = w.settle(|| {
            for out in broker.borrow_mut().tick(tab, Instant::now()) {
                if let BrokerOut::Answer { result, .. } = out {
                    *found.borrow_mut() = Some(result);
                }
            }
            found.borrow().is_some()
        });
        assert!(answered, "the broker never answered the call");
        found.into_inner().unwrap()
    }

    #[test]
    fn a_window_2_tab_opens_a_file_through_the_link() {
        use rmpv::Value as M;
        let w = windows("agent-open");
        let nvim = Rc::new(HeldNvim::default());
        w.one.set_rpc(nvim.clone());
        let tab = window_2_tab(&w);
        let broker = RefCell::new(Broker::new());
        assert!(open_a_file(&mut broker.borrow_mut(), &tab).is_empty());

        // The stand-in for window 1's nvim answers what the chunk would, once the op has reached it.
        assert!(w.settle(|| {
            broker.borrow_mut().tick(&tab, Instant::now());
            !nvim.calls.borrow().is_empty()
        }));
        let (code, args) = nvim.calls.borrow()[0].clone();
        assert_eq!(code, OPEN_FILE_LUA, "window 1 picks the tool's own chunk");
        let field = |name: &str| {
            args[1]
                .as_map()
                .unwrap()
                .iter()
                .find(|(key, _)| key.as_str() == Some(name))
                .map(|(_, value)| value.clone())
        };
        let absolute = w.root.join("src/a.rs");
        assert_eq!(
            field("path"),
            Some(M::Binary(absolute.as_os_str().as_encoded_bytes().to_vec())),
            "the path window 1 gives its editor is its own absolute one"
        );
        assert_eq!(field("load"), Some(M::from(true)));
        assert_eq!(field("line"), Some(M::from(1)));

        nvim.answers.borrow_mut().remove(0).send(Ok(M::Map(vec![
            (M::from("shown"), M::from(true)),
            (M::from("where"), M::from("new_split")),
            (M::from("line"), M::from(1)),
        ])));
        let result = the_answer(&w, &broker, &tab);
        assert!(!result.is_error, "{}", result.text);
        let told: serde_json::Value = serde_json::from_str(&result.text).unwrap();
        assert_eq!(told["shown"], true, "{told}");
        assert_eq!(told["where"], "new_split", "{told}");
        assert_eq!(told["line"], 1, "{told}");
        assert_eq!(
            told["other_window"], true,
            "window 2 cannot see window 1's editor: {told}"
        );
        assert_eq!(nvim.calls.borrow().len(), 1, "one call, one chunk");
    }

    #[test]
    fn a_window_2_tab_asks_the_language_server_through_the_link() {
        use rmpv::Value as M;
        let w = windows("agent-lsp");
        let nvim = Rc::new(HeldNvim::default());
        w.one.set_rpc(nvim.clone());
        let tab = window_2_tab(&w);
        let broker = RefCell::new(Broker::new());
        assert!(ask_for_usages(&mut broker.borrow_mut(), &tab).is_empty());

        until_called(&w, &broker, &tab, &nvim, 1);
        assert_eq!(
            nvim.calls.borrow()[0].0,
            LSP_ASK_LUA,
            "window 1 picks the tool's own chunk"
        );
        let bytes = |path: &Path| M::Binary(path.as_os_str().as_encoded_bytes().to_vec());
        assert_eq!(
            chunk_field(&nvim, 0, "path"),
            Some(bytes(&w.root.join("src/a.rs"))),
            "the path window 1 gives its editor is its own absolute one"
        );
        assert_eq!(chunk_field(&nvim, 0, "root"), Some(bytes(&w.root)));
        assert_eq!(chunk_field(&nvim, 0, "kind"), Some(M::from("usages")));
        assert_eq!(chunk_field(&nvim, 0, "symbol"), Some(M::from("a")));
        assert_eq!(chunk_field(&nvim, 0, "line"), Some(M::from(1)));
        assert_eq!(chunk_field(&nvim, 0, "load"), Some(M::from(true)));

        nvim.answers.borrow_mut().remove(0).send(Ok(M::Map(vec![
            (M::from("started"), M::from(true)),
            (M::from("unsupported"), M::Array(vec![])),
        ])));
        until_called(&w, &broker, &tab, &nvim, 2);
        assert_eq!(nvim.calls.borrow()[1].0, LSP_COLLECT_LUA);
        assert_eq!(
            chunk_field(&nvim, 1, "id"),
            chunk_field(&nvim, 0, "id"),
            "the collect goes under the ask's id"
        );

        let item = M::Map(vec![
            (M::from("path"), M::from(w.root.join("src/a.rs").to_str().unwrap())),
            (M::from("line"), M::from(1)),
            (M::from("character"), M::from(3)),
        ]);
        let part = M::Map(vec![
            (M::from("name"), M::from("definitions")),
            (M::from("items"), M::Array(vec![item])),
            (M::from("total"), M::from(1)),
        ]);
        nvim.answers.borrow_mut().remove(0).send(Ok(M::Map(vec![
            (M::from("status"), M::from("done")),
            (M::from("parts"), M::Array(vec![part])),
            (M::from("missing"), M::Array(vec![])),
        ])));
        let result = the_answer(&w, &broker, &tab);
        assert!(!result.is_error, "{}", result.text);
        let told: serde_json::Value = serde_json::from_str(&result.text).unwrap();
        let definition = &told["definitions"][0];
        assert_eq!(definition["path"], "src/a.rs", "{told}");
        assert_eq!(definition["line"], 1, "{told}");
        assert_eq!(definition["column"], 4, "{told}");
        assert_eq!(definition["text"], "fn a() {}", "{told}");
        assert_eq!(nvim.calls.borrow().len(), 2, "one ask, one collect");
    }

    #[test]
    fn a_window_2_tab_takes_its_baseline_through_the_link() {
        use rmpv::Value as M;
        let w = windows("agent-baseline");
        let nvim = Rc::new(HeldNvim::default());
        w.one.set_rpc(nvim.clone());
        let tab = window_2_tab(&w);
        let broker = RefCell::new(Broker::new());
        assert!(broker
            .borrow_mut()
            .turn_began(TAB, "turn-1", &tab, Instant::now())
            .is_empty());

        until_called(&w, &broker, &tab, &nvim, 1);
        assert_eq!(nvim.calls.borrow()[0].0, DIAGNOSTICS_LUA);
        assert_eq!(
            chunk_field(&nvim, 0, "root"),
            Some(M::Binary(w.root.as_os_str().as_encoded_bytes().to_vec())),
            "the root is window 1's own"
        );
        assert_eq!(chunk_field(&nvim, 0, "path"), None);
        assert_eq!(chunk_field(&nvim, 0, "min_severity"), Some(M::from("hint")));
        assert_eq!(chunk_field(&nvim, 0, "limit"), Some(M::from(BASELINE_LIMIT)));

        let known = diagnostic(&w, "there before the turn");
        nvim.answers
            .borrow_mut()
            .remove(0)
            .send(Ok(diagnostics_answer(vec![known.clone()])));
        assert!(w.settle(|| {
            broker.borrow_mut().tick(&tab, Instant::now());
            broker.borrow().is_idle()
        }));

        // What the baseline is for: only what appeared since is told, and window 2 alone took the difference.
        since_the_turn_began(&mut broker.borrow_mut(), &tab);
        until_called(&w, &broker, &tab, &nvim, 2);
        assert_eq!(chunk_field(&nvim, 1, "limit"), Some(M::from(BASELINE_LIMIT)));
        nvim.answers
            .borrow_mut()
            .remove(0)
            .send(Ok(diagnostics_answer(vec![known, diagnostic(&w, "made by the turn")])));
        let result = the_answer(&w, &broker, &tab);
        assert!(!result.is_error, "{}", result.text);
        let told: serde_json::Value = serde_json::from_str(&result.text).unwrap();
        let entries = told["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1, "{told}");
        assert_eq!(entries[0]["message"], "made by the turn", "{told}");
    }

    #[test]
    fn window_1_refuses_a_session_it_holds_and_the_tool_says_so() {
        let w = windows("agent-own-session");
        let nvim = Rc::new(HeldNvim::default());
        w.one.set_rpc(nvim.clone());
        w.one.set_own_session(Rc::new(|session: &str| session == "s-window-2"));
        let tab = window_2_tab(&w);
        let broker = RefCell::new(Broker::new());
        open_a_file(&mut broker.borrow_mut(), &tab);
        let result = the_answer(&w, &broker, &tab);
        assert!(result.is_error);
        assert_eq!(result.text, "session s-window-2 is not held by window 2");
        assert!(nvim.calls.borrow().is_empty(), "a refused op reached the editor");
    }

    #[test]
    fn a_link_that_is_down_answers_the_editors_window_is_closed() {
        let w = windows("agent-down");
        let nvim = Rc::new(HeldNvim::default());
        w.one.set_rpc(nvim.clone());
        w.two.unlinked();
        let tab = window_2_tab(&w);
        let broker = RefCell::new(Broker::new());
        open_a_file(&mut broker.borrow_mut(), &tab);
        let result = the_answer(&w, &broker, &tab);
        assert!(result.is_error);
        assert_eq!(result.text, WINDOW_CLOSED);

        ask_for_usages(&mut broker.borrow_mut(), &tab);
        let result = the_answer(&w, &broker, &tab);
        assert!(result.is_error);
        assert_eq!(result.text, WINDOW_CLOSED);

        // A baseline that could not be taken leaves the turn without one, and the tool says that.
        broker.borrow_mut().turn_began(TAB, "turn-1", &tab, Instant::now());
        assert!(w.settle(|| {
            broker.borrow_mut().tick(&tab, Instant::now());
            broker.borrow().is_idle()
        }));
        since_the_turn_began(&mut broker.borrow_mut(), &tab);
        let result = the_answer(&w, &broker, &tab);
        assert!(result.is_error);
        assert_eq!(result.text, NO_BASELINE);

        assert!(nvim.calls.borrow().is_empty(), "nothing crossed a link that is down");
    }

    #[test]
    fn a_late_answer_after_the_link_ended_is_dropped() {
        let w = windows("agent-late");
        let nvim = Rc::new(HeldNvim::default());
        w.one.set_rpc(nvim.clone());
        let tab = window_2_tab(&w);
        let broker = RefCell::new(Broker::new());
        open_a_file(&mut broker.borrow_mut(), &tab);
        assert!(w.settle(|| {
            broker.borrow_mut().tick(&tab, Instant::now());
            !nvim.answers.borrow().is_empty()
        }));

        w.two.unlinked();
        let result = the_answer(&w, &broker, &tab);
        assert_eq!(result.text, WINDOW_CLOSED);

        // The chunk ends after all, and window 1 replies to an operation nobody waits for.
        nvim.answers.borrow_mut().remove(0).send(Ok(rmpv::Value::Nil));
        w.pump_a_while();
        assert!(w.two.ops().waiting.borrow().is_empty());
        assert!(
            broker.borrow_mut().tick(&tab, Instant::now()).is_empty(),
            "a call answered once is not answered again"
        );
        assert!(broker.borrow().is_idle());
    }

    #[test]
    fn an_editor_tools_op_waits_past_the_brokers_budget() {
        let w = windows("agent-deadline");
        let nvim = Rc::new(HeldNvim::default());
        w.one.set_rpc(nvim.clone());
        let ops = w.two.ops();
        let way = ops.editor_way(&w.root);
        let stamp = Stamp {
            deadline_ms: 1,
            scope: "abc-3-1".into(),
            seq: 1,
        };
        let root = w.root.clone();
        let asked = way.call("s-window-2", &EditorCall::EditorState { root }, &stamp);
        let sent = Instant::now();
        assert!(ops.expire(sent + EDITOR_BUDGET), "the broker's budget ends first");
        assert!(ops.expire(sent + OP_DEADLINE), "a page's deadline does not apply");
        assert!(asked.try_take().is_none());
        assert!(!ops.expire(sent + EDITOR_BUDGET_MAX + Duration::from_secs(1)));
        assert!(matches!(asked.try_take(), Some(Err(RpcError::Unavailable(_)))));
    }

    /// Records every Lua call and answers each with a value of its own.
    struct Answering {
        calls: RefCell<Vec<&'static str>>,
        answer: rmpv::Value,
    }

    impl EditorRpc for Answering {
        fn exec_lua(&self, code: &'static str, _args: Vec<rmpv::Value>) -> Pending {
            self.calls.borrow_mut().push(code);
            let (answer, pending) = Pending::pair();
            answer.send(Ok(self.answer.clone()));
            pending
        }
        fn target(&self) -> Option<u64> {
            Some(1)
        }
    }

    fn buffer_state_answer() -> rmpv::Value {
        use rmpv::Value as M;
        M::Map(vec![
            (M::from("found"), M::from(true)),
            (M::from("modified"), M::from(true)),
            (M::from("fileformat"), M::from("unix")),
            (M::from("line_count"), M::from(2)),
            (
                M::from("lines"),
                M::Array(vec![M::from("fn a() {}"), raw_string(b"\xffbad").unwrap()]),
            ),
        ])
    }

    #[test]
    fn a_page_ending_in_window_2_turns_its_following_off_and_leaves_window_1s_snippet() {
        use eitri_core::plugin::{editor, roots::Roots};
        let w = windows("plugin-teardown");
        let proxy = PluginEditorRpc::new("markdown".into(), Roots::new(BTreeMap::new()), w.two.ops());
        let args = vec![
            rmpv::Value::from("teardown"),
            rmpv::Value::Map(vec![(rmpv::Value::from("owner"), rmpv::Value::from("ignored"))]),
        ];
        assert_eq!(
            proxy.operation(editor::PLUGIN_FOLLOW_LUA, &args),
            Ok(EditorOp::PluginFollow {
                plugin: "markdown".into(),
                on: false
            })
        );
    }

    #[derive(Default)]
    struct BufferPage(RefCell<Vec<serde_json::Value>>);

    impl eitri_core::plugin::host::PagePort for BufferPage {
        fn send(&self, text: &str) {
            self.0.borrow_mut().push(serde_json::from_str(text).unwrap());
        }
        fn set_theme(&self, _: &eitri_core::plugin::theme::PageTheme) {}
    }

    fn window_two_plugin(w: &Windows) -> (eitri_core::plugin::host::PluginHost, Rc<BufferPage>) {
        use eitri_core::plugin::{host::PluginHost, load::load_plugin};
        let dir = w.root.join("plugin");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("eitri-plugin.toml"),
            r#"
            id = "demo"
            title = "Demo"
            protocol = 1
            site = "site"
            entry = "index.html"
            capabilities = ["editor.buffers", "editor.cwd"]
            [roots]
            work = "${plugin}"
            [window]
            root = "${plugin}"
            title = "Demo"
            layout = { module = "editor" }
        "#,
        )
        .unwrap();
        let plugin = Rc::new(load_plugin(dir.to_str().unwrap(), &BTreeMap::new(), None).unwrap());
        let page = Rc::new(BufferPage::default());
        let host = PluginHost::new(plugin.clone(), page.clone(), std::sync::Arc::new(|| {}), None, vec![]);
        host.set_editor(Rc::new(PluginEditorRpc::new(
            "demo".into(),
            plugin.roots.clone(),
            w.two.ops(),
        )));
        (host, page)
    }

    #[test]
    fn window_two_editor_buffers_verbs_answer_editor_unavailable_and_send_no_editor_op() {
        let w = windows("buffers-refused");
        let rpc = Rc::new(Answering {
            calls: RefCell::default(),
            answer: rmpv::Value::Nil,
        });
        w.one.set_rpc(rpc.clone());
        // Give window 1 a valid holding: a refusal must come from the missing operation itself.
        w.one.set_plugin_in_window(
            "demo",
            Some(SECOND_PEER),
            Some(BTreeMap::from([("work".into(), w.root.join("plugin"))])),
        );
        let (host, page) = window_two_plugin(&w);
        let root = w.root.join("plugin");
        let path = root.join("a.rs");
        std::fs::write(&path, "fn a() {}\n").unwrap();
        let text = serde_json::json!({"title": "Attempt", "text": "a\n", "filetype": "rust"});
        for (verb, args) in [
            (
                "editor.preload",
                serde_json::json!({"path": path, "diagnostics": "on", "inlay_hints": "off"}),
            ),
            ("editor.modified", serde_json::json!({"root": root})),
            ("editor.reload", serde_json::json!({"root": root})),
            (
                "editor.diff",
                serde_json::json!({"left": text, "right": {"path": path}}),
            ),
        ] {
            host.page_message(
                &serde_json::json!({"v": 1, "id": 1, "verb": verb, "args": args}).to_string(),
                Instant::now(),
            );
            assert!(w.settle(|| {
                host.pump(Instant::now());
                !page.0.borrow().is_empty()
            }));
            assert_eq!(
                page.0.borrow_mut().remove(0)["error"]["code"],
                "editor_unavailable",
                "{verb}"
            );
        }
        assert!(w.two.ops().waiting.borrow().is_empty(), "no editor op was queued");
        assert!(rpc.calls.borrow().is_empty());
    }

    #[test]
    fn window_two_editor_cd_answers_not_own_window() {
        let w = windows("cd-refused");
        let (host, page) = window_two_plugin(&w);
        host.page_message(
            &serde_json::json!({"v": 1, "id": 1, "verb": "editor.cd", "args": {"dir": w.root.join("plugin")}})
                .to_string(),
            Instant::now(),
        );
        assert_eq!(page.0.borrow()[0]["error"]["code"], "not_own_window");
        assert!(w.two.ops().waiting.borrow().is_empty());
    }

    #[test]
    fn window_two_hello_says_own_window_false() {
        let w = windows("hello-own-false");
        let (host, page) = window_two_plugin(&w);
        host.page_message(r#"{"v":1,"id":1,"verb":"host.hello","args":{}}"#, Instant::now());
        assert_eq!(page.0.borrow()[0]["result"]["own_window"], false);
    }

    #[test]
    fn a_plugin_open_carries_the_matched_root_name_and_relative_path() {
        use eitri_core::plugin::{editor::PLUGIN_OPEN_LUA, roots::Roots};
        use rmpv::Value;
        let roots = BTreeMap::from([
            ("a".into(), PathBuf::from("/work/one")),
            ("b".into(), PathBuf::from("/work/two")),
        ]);
        let proxy = PluginEditorRpc::new(
            "demo".into(),
            Roots::new(roots),
            EditorOps::new(Steps::default().repeat()),
        );
        let args = vec![Value::Map(vec![
            ("path".into(), Value::Binary(b"/work/two/src/lib.rs".to_vec())),
            ("line".into(), 3.into()),
            ("col".into(), 2.into()),
        ])];
        assert_eq!(
            proxy.operation(PLUGIN_OPEN_LUA, &args).unwrap(),
            EditorOp::PluginOpen {
                plugin: "demo".into(),
                root: "b".into(),
                path: "src/lib.rs".into(),
                line: Some(3),
                col: Some(2),
            }
        );
    }

    #[test]
    fn a_plugins_fixed_calls_in_window_2_are_typed_and_answered_by_window_1() {
        use eitri_core::plugin::{editor, roots::Roots};
        let w = windows("plugin-ops");
        w.one.set_plugin_in_window(
            "markdown",
            Some(SECOND_PEER),
            Some(BTreeMap::from([("work".into(), w.root.clone())])),
        );
        w.one.set_plugin_follow(
            "markdown",
            eitri_core::window_link::editor_ops::FollowConfig {
                socket: w.root.join("own-feed"),
                scratch: None,
                filetypes: vec!["markdown".into()],
                commands: vec![("MarkdownPreview".into(), "toggle")],
                auto: true,
            },
        );
        let rpc = Rc::new(Answering {
            calls: RefCell::default(),
            answer: rmpv::Value::from(true),
        });
        w.one.set_rpc(rpc.clone());
        let proxy = PluginEditorRpc::new(
            "markdown".into(),
            Roots::new(BTreeMap::from([("work".into(), w.root.clone())])),
            w.two.ops(),
        );
        let requests = vec![
            editor::open(&proxy, &w.root.join("src/a.rs"), Some(1), None),
            editor::assist_apply(&proxy, "ignored", Path::new("/ignored"), true, false),
            editor::assist_clear(&proxy, "ignored"),
            editor::follow_install(
                &proxy,
                &editor::FollowInstall {
                    owner: "ignored",
                    socket: Path::new("/ignored"),
                    filetypes: &[],
                    auto: false,
                    commands: &[],
                    scratch_dir: None,
                },
            ),
            editor::follow_on(&proxy, "ignored", true),
            editor::follow_text(&proxy, "ignored", 3, 1, 1),
            editor::follow_reveal(&proxy, "ignored", 3, 2, 8, Some(5), Some(9)),
            editor::follow_jump(&proxy, "ignored", 3, 2, 1, None),
            editor::follow_jump(
                &proxy,
                "ignored",
                3,
                2,
                1,
                Some(eitri_core::plugin::protocol::JumpSelect {
                    line: 4,
                    col: 2,
                    mode: eitri_core::plugin::protocol::SelectMode::Line,
                }),
            ),
            editor::follow_teardown(&proxy, "ignored"),
        ];
        for request in requests {
            let got = RefCell::new(None);
            assert!(w.settle(|| {
                if got.borrow().is_none() {
                    *got.borrow_mut() = request.try_take();
                }
                got.borrow().is_some()
            }));
            assert!(got.into_inner().unwrap().is_ok());
        }
        assert_eq!(rpc.calls.borrow().len(), 10);
        assert!(rpc.calls.borrow()[3..]
            .iter()
            .all(|code| std::ptr::eq(*code, editor::PLUGIN_FOLLOW_LUA)));
    }

    #[test]
    fn an_editor_op_from_window_2_is_answered_through_window_1s_dispatch() {
        let w = windows("ops");
        let rpc = Rc::new(Answering {
            calls: RefCell::default(),
            answer: buffer_state_answer(),
        });
        w.one.set_rpc(rpc.clone());
        let ops = w.two.ops();

        let asked = ops.send(EditorOp::BufferState {
            path: "src/a.rs".into(),
            lines: None,
        });
        let got = RefCell::new(None);
        assert!(w.settle(|| {
            if got.borrow().is_none() {
                *got.borrow_mut() = asked.try_take();
            }
            got.borrow().is_some()
        }));
        let value = got.into_inner().unwrap().expect("answered");
        assert_eq!(
            value,
            buffer_state_answer(),
            "window 2 reads the answer window 1's nvim gave, as it gave it"
        );
        let state = eitri_core::turn_review::revert::parse_buffer_state(&value).unwrap();
        assert!(state.found && state.modified);
        assert_eq!(
            state.lines.unwrap()[1],
            b"\xffbad".to_vec(),
            "bytes that are not UTF-8 survive"
        );
        assert_eq!(
            *rpc.calls.borrow(),
            vec![eitri_core::turn_review::revert::BUFFER_STATE_LUA]
        );

        // A path outside the project is refused by window 1 before anything reaches its editor.
        let refused = ops.send(EditorOp::BufferState {
            path: "../outside".into(),
            lines: None,
        });
        let got = RefCell::new(None);
        assert!(w.settle(|| {
            if got.borrow().is_none() {
                *got.borrow_mut() = refused.try_take();
            }
            got.borrow().is_some()
        }));
        assert!(matches!(got.into_inner(), Some(Err(RpcError::Nvim(_)))));
        assert_eq!(rpc.calls.borrow().len(), 1, "the refused op asked nothing");

        // A Lua nil is an answer, not its absence.
        let nil = Rc::new(Answering {
            calls: RefCell::default(),
            answer: rmpv::Value::Nil,
        });
        w.one.set_rpc(nil);
        let asked = ops.send(EditorOp::BufferState {
            path: "src/a.rs".into(),
            lines: Some((1, 1)),
        });
        let got = RefCell::new(None);
        assert!(w.settle(|| {
            if got.borrow().is_none() {
                *got.borrow_mut() = asked.try_take();
            }
            got.borrow().is_some()
        }));
        assert_eq!(got.into_inner(), Some(Ok(rmpv::Value::Nil)));

        // A review call goes to window 1's review driver, which answers it.
        w.one.set_overlay(Rc::new(|_: PeerId, _: u64, calls: Vec<OverlayCall>| {
            Some(ReplyResult::Ok {
                value: Value::from(calls.len()),
            })
        }));
        let cleared = ops.send(EditorOp::ReviewClearMine {});
        let got = RefCell::new(None);
        assert!(w.settle(|| {
            if got.borrow().is_none() {
                *got.borrow_mut() = cleared.try_take();
            }
            got.borrow().is_some()
        }));
        assert_eq!(got.into_inner(), Some(Ok(rmpv::Value::from(0u64))));
    }

    #[test]
    fn editor_ops_answer_unavailable_at_once_while_unlinked() {
        let steps = Steps::default();
        let ops = EditorOps::new(steps.repeat());
        let asked = ops.send(EditorOp::ReviewClearMine {});
        assert_eq!(
            asked.try_take(),
            Some(Err(RpcError::Unavailable(UNLINKED_NOTE.into()))),
            "no link: answered at once, with why"
        );
        assert!(
            steps.0.borrow().is_empty(),
            "nothing waits for an answer that cannot come"
        );

        let (one, two) = linked_pair();
        ops.linked(&two);
        let waiting = ops.send(EditorOp::ReviewClearMine {});
        let late = ops.send(EditorOp::ReviewClearMine {});
        assert_eq!(waiting.try_take(), None);
        // The deadline passes for both; the expiry step fails them.
        assert!(!ops.expire(Instant::now() + OP_DEADLINE));
        assert_eq!(waiting.try_take(), Some(Err(RpcError::Unavailable(NO_ANSWER.into()))));
        assert_eq!(late.try_take(), Some(Err(RpcError::Unavailable(NO_ANSWER.into()))));

        let cut = ops.send(EditorOp::ReviewClearMine {});
        ops.unlinked();
        assert_eq!(
            cut.try_take(),
            Some(Err(RpcError::Unavailable(UNLINKED_NOTE.into()))),
            "an op out when the link ends is answered then"
        );
        assert_eq!(
            ops.send(EditorOp::ReviewClearMine {}).try_take(),
            Some(Err(RpcError::Unavailable(UNLINKED_NOTE.into())))
        );
        drop(one);
    }

    /// Records what window 1's editor was asked to do, with the token that came with it.
    /// What window 1's editor was asked, in order: the request read back (none for the keys alone), and the token.
    type Driven = Rc<RefCell<Vec<(Option<ScratchAsk>, Option<String>)>>>;

    fn recording_drive(w: &Windows) -> Driven {
        drive_recorded(&w.one)
    }

    fn drive_recorded(one: &Window1Editor) -> Driven {
        let driven: Driven = Rc::default();
        let record = driven.clone();
        one.set_drive(Rc::new(move |act: EditorAct<'_>, token: Option<String>| {
            let ask = match act {
                EditorAct::Request(request) => Some(read_request(request).unwrap()),
                EditorAct::Focus => None,
            };
            record.borrow_mut().push((ask, token));
            Ok(())
        }));
        driven
    }

    #[test]
    fn a_path_click_from_window_2_is_checked_by_window_1_as_its_own() {
        let w = windows("paths");
        let driven = recording_drive(&w);
        // Window 2's panel resolved the click to an absolute path; it goes as a project path, with the token.
        let click = eitri_core::scratch::open_request(&w.root.join("src/a.rs"), Some(7));
        w.two.request(&click, &|| Some("token-1".into())).unwrap();
        assert!(w.settle(|| !driven.borrow().is_empty()));
        assert_eq!(
            driven.borrow()[0],
            (
                Some(ScratchAsk::Open {
                    path: w.root.join("src/a.rs"),
                    line: Some(7)
                }),
                Some("token-1".into())
            ),
            "window 1 opens its own absolute path at the line, raised by window 2's token"
        );

        // What window 2 sends is checked again in window 1, as a path from its own panel is.
        for path in ["src/a\n.rs", "src/missing.rs", "../outside.rs"] {
            w.two_link
                .send(
                    Msg::Editor {
                        request: EditorRequest::OpenPath {
                            path: path.into(),
                            line: None,
                            col: None,
                        },
                        token: None,
                        answer: None,
                    },
                    Vec::new(),
                )
                .unwrap();
        }
        // A reviewed file goes through turn review's own path rule: a project path, no way out of it.
        for path in ["../project/src/a.rs", "src/a.rs"] {
            w.two_link
                .send(
                    Msg::Editor {
                        request: EditorRequest::ReviewOpen {
                            path: path.into(),
                            line: Some(1),
                        },
                        token: None,
                        answer: None,
                    },
                    Vec::new(),
                )
                .unwrap();
        }
        assert!(w.settle(|| driven.borrow().len() >= 2));
        w.pump_a_while();
        assert_eq!(
            driven.borrow().len(),
            2,
            "only the reviewed file inside the project was opened: {:?}",
            driven.borrow()
        );
        assert_eq!(
            driven.borrow()[1].0,
            Some(ScratchAsk::Open {
                path: w.root.join("src/a.rs"),
                line: Some(1)
            })
        );

        // The editor key in window 2: the keys, no request.
        w.two_link
            .send(
                Msg::Editor {
                    request: EditorRequest::Focus,
                    token: Some("token-2".into()),
                    answer: None,
                },
                Vec::new(),
            )
            .unwrap();
        assert!(w.settle(|| driven.borrow().len() == 3));
        assert_eq!(driven.borrow()[2], (None, Some("token-2".into())));

        w.two.unlinked();
        assert_eq!(w.two.request(&click, &|| None), Err(UNLINKED_NOTE.to_string()));
    }

    /// Window 2's panel hears how a view went only once window 1 has written it and handed it to its editor: a
    /// drive that fails comes back as the failure, a shown view as shown, a link that ends first as the link's end.
    /// A view of window 2's still being written when it closes opens in window 1's editor, but its answer is not
    /// sent to the window opened in its place, whose own request may hold the same id.
    #[test]
    fn a_view_answer_for_a_window_that_went_does_not_reach_the_one_opened_in_its_place() {
        let w = windows("view-stale-answer");
        let drives = Rc::new(Cell::new(0));
        {
            let drives = drives.clone();
            w.one.set_drive(Rc::new(move |_: EditorAct<'_>, _: Option<String>| {
                drives.set(drives.get() + 1);
                // The first drive is the old window's view; the replacement's own view is the one that fails.
                if drives.get() == 1 {
                    Ok(())
                } else {
                    Err("the editor is not ready".to_string())
                }
            }));
        }
        let mut scratch = ScratchDir::in_dir(w.two.scratch.as_deref().unwrap()).unwrap();
        let old_view = scratch.prepare_view("Turn 1 notes", "old row").unwrap();
        let _old_later = w.two.request_answered(&old_view, &|| None).unwrap().unwrap();
        // Window 2 reads the view and sends it; window 1 takes it and has not yet polled, so the view waits in its list
        // (the steps are only run while window 1 has registered none).
        let deadline = Instant::now() + Duration::from_secs(5);
        while w.two.answers.borrow().is_empty() {
            assert!(Instant::now() < deadline, "window 2 never sent the view");
            w.two_link.pump();
            w.steps.run();
            std::thread::sleep(Duration::from_millis(5));
        }
        let old_ids: Vec<u64> = w.two.answers.borrow().keys().copied().collect();
        while w.one.views.borrow().is_empty() {
            assert!(Instant::now() < deadline, "window 1 never took the view");
            w.one_link.pump();
            std::thread::sleep(Duration::from_millis(5));
        }

        // Window 2 closes and is opened again under the same peer, and sends a view of its own before window 1 polls.
        w.two.unlinked();
        w.one.unlinked(SECOND_PEER);
        let (one_link, two_link) = linked_pair();
        let again_steps = Steps::default();
        let again = Window2Editor::new(&w.root, w.two.scratch.clone(), again_steps.repeat());
        again.linked(&two_link);
        w.one.linked(SECOND_PEER, WindowName::Second, &one_link);
        let new_view = scratch.prepare_view("Turn 2 notes", "new row").unwrap();
        let later = again.request_answered(&new_view, &|| None).unwrap().unwrap();
        while again.answers.borrow().is_empty() {
            assert!(Instant::now() < deadline, "the new window never sent its view");
            two_link.pump();
            again_steps.run();
            std::thread::sleep(Duration::from_millis(5));
        }
        let new_ids: Vec<u64> = again.answers.borrow().keys().copied().collect();
        assert_eq!(
            new_ids, old_ids,
            "the case: the new window's request holds the old one's id"
        );
        one_link.pump();

        let got: RefCell<Option<Result<(), String>>> = RefCell::default();
        assert!(settle(&one_link, &two_link, &w.steps, || {
            if got.borrow().is_none() {
                *got.borrow_mut() = later.try_recv().ok();
            }
            got.borrow().is_some()
        }));
        assert_eq!(
            got.into_inner(),
            Some(Err("the editor is not ready".to_string())),
            "the new window hears how its own view went"
        );
        assert_eq!(
            drives.get(),
            2,
            "the old window's view still opened in window 1's editor"
        );
    }

    #[test]
    fn a_view_from_window_2_is_answered_once_window_1_has_shown_it() {
        let w = windows("view-answers");
        let mut scratch = ScratchDir::in_dir(w.two.scratch.as_deref().unwrap()).unwrap();
        let fail = Rc::new(Cell::new(true));
        let drives = Rc::new(Cell::new(0));
        {
            let (fail, drives) = (fail.clone(), drives.clone());
            w.one.set_drive(Rc::new(move |_: EditorAct<'_>, _: Option<String>| {
                drives.set(drives.get() + 1);
                if fail.get() {
                    Err("the editor is not ready".to_string())
                } else {
                    Ok(())
                }
            }));
        }
        let outcome = |later: &mpsc::Receiver<Result<(), String>>| {
            let got: RefCell<Option<Result<(), String>>> = RefCell::default();
            assert!(w.settle(|| {
                if got.borrow().is_none() {
                    *got.borrow_mut() = later.try_recv().ok();
                }
                got.borrow().is_some()
            }));
            got.into_inner().unwrap()
        };

        let view = scratch.prepare_view("Turn 3 notes", "a row").unwrap();
        let later = w
            .two
            .request_answered(&view, &|| None)
            .unwrap()
            .expect("a view's outcome comes later");
        assert_eq!(
            later.try_recv(),
            Err(mpsc::TryRecvError::Empty),
            "nothing is known when it is sent"
        );
        assert_eq!(outcome(&later), Err("the editor is not ready".to_string()));
        assert_eq!(drives.get(), 1, "the answer came after window 1 drove its editor");

        fail.set(false);
        let view = scratch.prepare_view("Turn 4 notes", "another row").unwrap();
        let later = w.two.request_answered(&view, &|| None).unwrap().unwrap();
        assert_eq!(outcome(&later), Ok(()));
        assert_eq!(drives.get(), 2);

        // Any other request is answered when it is sent, and the plain `request` asks no answer.
        let open = eitri_core::scratch::open_request(&w.root.join("src/a.rs"), Some(1));
        assert!(w.two.request_answered(&open, &|| None).unwrap().is_none());
        let view = scratch.prepare_view("Turn 5 notes", "a third row").unwrap();
        w.two.request(&view, &|| None).unwrap();
        assert!(w.settle(|| drives.get() == 4));

        // Sent, then the link ends before window 1 answers: the panel hears that the link ended.
        w.one.set_drive(Rc::new(|_: EditorAct<'_>, _: Option<String>| Ok(())));
        let view = scratch.prepare_view("Turn 6 notes", "a last row").unwrap();
        let later = w.two.request_answered(&view, &|| None).unwrap().unwrap();
        assert!(w.settle(|| !w.two.answers.borrow().is_empty()), "the view went out");
        w.two.unlinked();
        assert_eq!(later.try_recv(), Ok(Err(UNLINKED_NOTE.to_string())));
    }

    #[test]
    fn a_view_or_a_proposal_from_window_2_opens_read_only_in_window_1s_editor() {
        let w = windows("views");
        let driven = recording_drive(&w);
        let ones = w.one.scratch_parent.clone().unwrap();
        let mut scratch = ScratchDir::in_dir(w.two.scratch.as_deref().unwrap()).unwrap();

        // A chat row's read-only view: its text goes, and window 1 opens a copy of its own.
        let view = scratch.prepare_view("Turn 3 notes", "a row\nof text").unwrap();
        w.two.request(&view, &|| Some("token-v".into())).unwrap();
        assert!(w.settle(|| !driven.borrow().is_empty()));
        let (ask, token) = driven.borrow()[0].clone();
        let Some(ScratchAsk::View { body, tab }) = ask else {
            panic!("a view opens as a view: {ask:?}")
        };
        assert!(!tab, "a row's view opens in the row-view split");
        assert_eq!(
            token,
            Some("token-v".into()),
            "window 1 comes forward with window 2's token"
        );
        assert!(
            body.starts_with(&ones),
            "window 1 opens its own copy, not window 2's file: {body:?}"
        );
        assert_eq!(std::fs::read_to_string(&body).unwrap(), "a row\nof text");
        assert!(
            body.to_string_lossy().contains("turn-3-notes"),
            "the view keeps its title: {body:?}"
        );

        // A pending edit's proposal: both sides go as text, the diff named after the real file.
        let proposal = scratch
            .prepare_proposal("src/a.rs", "fn a() {}\n", "fn a() { b() }\n")
            .unwrap();
        w.two.request(&proposal, &|| None).unwrap();
        assert!(w.settle(|| driven.borrow().len() == 2));
        let ask = driven.borrow()[1].0.clone();
        let Some(ScratchAsk::Diff { base, proposed, name }) = ask else {
            panic!("a proposal opens as a diff: {ask:?}")
        };
        assert_eq!(name, "src/a.rs");
        for (file, text) in [(&base, "fn a() {}\n"), (&proposed, "fn a() { b() }\n")] {
            assert!(file.starts_with(&ones), "window 1's own copy: {file:?}");
            assert_eq!(std::fs::read_to_string(file).unwrap(), text);
        }

        // Only window 2's own scratch files are read for window 1, by no way that climbs out of them.
        let mut elsewhere = ScratchDir::in_dir(&w.root).unwrap();
        let outside = elsewhere.prepare_view("notes", "text").unwrap();
        assert!(w.two.request(&outside, &|| None).is_err());
        let climbing = w.two.scratch.as_deref().unwrap().join("../project/src/a.rs");
        assert!(climbing.exists());
        assert!(w.two.own_scratch_text(&climbing).is_err());
        w.pump_a_while();
        assert_eq!(driven.borrow().len(), 2, "nothing more reached window 1's editor");
        elsewhere.cleanup();
        scratch.cleanup();
    }

    /// `:%scratch` from window 2: the tab page it asked for is the one window 1 opens, and views asked for one after
    /// another open in that order, each read and written again off the GTK thread.
    #[test]
    fn a_tab_view_from_window_2_keeps_its_tab_and_its_order() {
        let w = windows("tab-views");
        let driven = recording_drive(&w);
        let ones = w.one.scratch_parent.clone().unwrap();
        let mut scratch = ScratchDir::in_dir(w.two.scratch.as_deref().unwrap()).unwrap();
        let big = "## ▍You · prompt 1/1\n\n".to_string() + &"x".repeat(2_000_000);
        let whole = scratch.reserve_view("Conversation").with_tab(true).write(&big).unwrap();
        let row = scratch.prepare_view("reply", "a row").unwrap();
        w.two.request(&whole, &|| Some("token-t".into())).unwrap();
        w.two.request(&row, &|| None).unwrap();
        assert!(w.settle(|| driven.borrow().len() == 2));
        let asks: Vec<_> = driven
            .borrow()
            .iter()
            .map(|(ask, token)| (ask.clone(), token.clone()))
            .collect();
        let (Some(ScratchAsk::View { body, tab }), token) = &asks[0] else {
            panic!("the conversation opens as a view: {asks:?}")
        };
        assert!(*tab, "the conversation keeps its tab page");
        assert_eq!(token.as_deref(), Some("token-t"));
        assert!(body.starts_with(&ones), "window 1's own copy: {body:?}");
        assert_eq!(std::fs::read_to_string(body).unwrap(), big);
        let (Some(ScratchAsk::View { body, tab }), _) = &asks[1] else {
            panic!("the row opens as a view: {asks:?}")
        };
        assert!(!*tab, "the row after it keeps the split");
        assert_eq!(std::fs::read_to_string(body).unwrap(), "a row");
        scratch.cleanup();
    }

    #[test]
    fn a_draft_ended_in_window_1s_editor_brings_window_2_forward_with_window_1s_token() {
        let w = windows("draft-raise");
        let driven = recording_drive(&w);
        w.one.set_token_source(Rc::new(|| Some("from-window-1".into())));
        let raised: Rc<RefCell<Vec<Option<String>>>> = Rc::default();
        {
            let raised = raised.clone();
            w.two
                .set_raise(Rc::new(move |token: Option<String>| raised.borrow_mut().push(token)));
        }
        let mut scratch = ScratchDir::in_dir(w.two.scratch.as_deref().unwrap()).unwrap();
        let (ask, edit) = out_draft(&w, &driven, &mut scratch, "a draft", 4);
        assert!(
            raised.borrow().is_empty(),
            "window 2 stays behind while the draft is out"
        );
        nvim_writes(&ask, "edited");
        assert!(w.settle(|| edit.poll().is_some()));
        assert_eq!(edit.poll(), Some(EditDone::Written("edited".into())));
        assert_eq!(
            *raised.borrow(),
            vec![Some("from-window-1".to_string())],
            "the window whose tab it is takes the keys back, with a token from the window that had them"
        );

        // A quit in nvim brings it forward too: the keys were in window 1's editor either way.
        let (ask, quit) = out_draft(&w, &driven, &mut scratch, "kept", 5);
        let ScratchAsk::Edit { done, .. } = &ask else {
            panic!("{ask:?}")
        };
        std::fs::write(done, "discarded").unwrap();
        assert!(w.settle(|| quit.poll().is_some()));
        assert_eq!(raised.borrow().len(), 2);
        scratch.cleanup();
    }

    #[test]
    fn a_failed_editor_show_reports_to_the_requesting_window_2() {
        use eitri_core::layout::{Layout, LayoutError, ModuleId, Node, Workspaces};
        use eitri_core::wm::fake::{FakeHost, FakeScheduler, Logged};
        use eitri_core::wm::*;

        let w = windows("show-refused");
        let host = FakeHost::new();
        host.set_editor_running(true);
        let scheduler = FakeScheduler::new();
        let mut controller = Controller::new(scheduler.clone());
        controller.set_reads(host.clone());
        controller.open_window(
            WinId::MAIN,
            WindowSpec {
                role: WindowRole::Linked { side: LinkSide::First },
                presence: EditorPresence::Owned,
                title: "window 1".into(),
            },
            WindowParts::new(
                Layout::new(Node::Leaf(ModuleId::agent()), ModuleId::agent()).unwrap(),
                Workspaces::single(),
                None,
            ),
        );
        let driver = Driver::new(controller, host.clone(), scheduler);
        assert_eq!(driver.query(|c| c.editor_request_refusal(WinId::MAIN)), None);
        let drive = driver.clone();
        w.one.set_drive(Rc::new(move |act: EditorAct<'_>, _: Option<String>| {
            let keys = match act {
                EditorAct::Request(request) => request.input_keys(),
                EditorAct::Focus => String::new(),
            };
            drive.editor_request(WinId::MAIN, keys)
        }));
        w.two.set_draft_tab(Rc::new(|_| Some((TabId(4), None))));
        let mut scratch = ScratchDir::in_dir(w.two.scratch.as_deref().unwrap()).unwrap();
        let (request, edit) = scratch.prepare_edit("kept draft").unwrap();
        w.two.request(&request, &|| None).unwrap();
        assert!(w.settle(|| edit.poll().is_some()));
        assert_eq!(
            edit.poll(),
            Some(EditDone::Failed(eitri_core::wm::feedback::layout_refusal_text(
                &LayoutError::NotInTree(ModuleId::editor())
            )))
        );
        assert_eq!(driver.query(|c| c.chrome(WinId::MAIN).toast), None);
        assert!(!host.log().iter().any(|entry| matches!(
            entry,
            Logged::Op(HostOp::Editor(EditorCall::SendKeys { .. }) | HostOp::Focus { .. }, _)
        )));
        scratch.cleanup();
    }

    /// What nvim does with a draft edit window 1 handed it: writes the text and the marker.
    fn nvim_writes(ask: &ScratchAsk, text: &str) {
        let ScratchAsk::Edit { body, done } = ask else {
            panic!("not an edit: {ask:?}")
        };
        std::fs::write(body, format!("{text}\n")).unwrap();
        std::fs::write(done, "written").unwrap();
    }

    #[test]
    fn a_scratch_draft_goes_back_to_the_window_holding_the_tab_now() {
        let w = windows("drafts");
        let driven = recording_drive(&w);
        let moved_here: Rc<RefCell<BTreeSet<String>>> = Rc::default();
        {
            let moved_here = moved_here.clone();
            w.one
                .set_own_session(Rc::new(move |s: &str| moved_here.borrow().contains(s)));
        }
        let here: Rc<RefCell<Vec<(Option<String>, EditDone)>>> = Rc::default();
        {
            let here = here.clone();
            w.one.set_draft_here(Rc::new(move |s: Option<String>, d: EditDone| {
                here.borrow_mut().push((s, d))
            }));
        }
        let tabs: Rc<RefCell<BTreeMap<u64, (TabId, Option<String>)>>> = Rc::default();
        {
            let tabs = tabs.clone();
            w.two
                .set_draft_tab(Rc::new(move |edit: u64| tabs.borrow().get(&edit).cloned()));
        }
        let mut scratch = ScratchDir::in_dir(w.two.scratch.as_deref().unwrap()).unwrap();

        // Window 2's tab 4 edits its draft in window 1's nvim; the tab is still in window 2 when it is written.
        let (request, edit) = scratch.prepare_edit("first draft").unwrap();
        tabs.borrow_mut().insert(edit.id, (TabId(4), Some("s-stays".into())));
        w.two.request(&request, &|| None).unwrap();
        assert!(w.settle(|| !driven.borrow().is_empty()));
        let ask = driven.borrow()[0].0.clone().unwrap();
        let ScratchAsk::Edit { body, .. } = &ask else {
            panic!("{ask:?}")
        };
        assert_eq!(
            std::fs::read_to_string(body).unwrap(),
            "first draft",
            "the draft opens with the tab's text"
        );
        assert!(
            body.starts_with(w.one.scratch_parent.as_deref().unwrap()),
            "in window 1's own scratch directory, whose buffers the context feed leaves out"
        );
        assert_eq!(edit.poll(), None);
        nvim_writes(&ask, "edited in nvim");
        assert!(w.settle(|| edit.poll().is_some()));
        assert_eq!(edit.poll(), Some(EditDone::Written("edited in nvim".into())));
        assert!(here.borrow().is_empty());

        // Tab 5's draft goes out, and the tab moves to window 1 before it is written: window 1 keeps it.
        let (request, moved) = scratch.prepare_edit("second draft").unwrap();
        tabs.borrow_mut().insert(moved.id, (TabId(5), Some("s-moves".into())));
        w.two.request(&request, &|| None).unwrap();
        assert!(w.settle(|| driven.borrow().len() == 2));
        moved_here.borrow_mut().insert("s-moves".into());
        nvim_writes(&driven.borrow()[1].0.clone().unwrap(), "edited after the move");
        assert!(w.settle(|| !here.borrow().is_empty()));
        assert_eq!(
            *here.borrow(),
            vec![(
                Some("s-moves".into()),
                EditDone::Written("edited after the move".into())
            )],
            "the draft reaches the tab in the window that holds it now"
        );
        w.pump_a_while();
        assert_eq!(
            moved.poll(),
            None,
            "window 2, which no longer holds the tab, is not told"
        );

        // A draft window 1's editor cannot take ends with the reason, the tab keeping its text, so it can try
        // again.
        w.one.set_drive(Rc::new(|_: EditorAct<'_>, _: Option<String>| {
            Err("the editor is not ready yet".into())
        }));
        let (request, refused) = scratch.prepare_edit("third draft").unwrap();
        tabs.borrow_mut().insert(refused.id, (TabId(6), None));
        w.two.request(&request, &|| None).unwrap();
        assert!(w.settle(|| refused.poll().is_some()));
        assert_eq!(
            refused.poll(),
            Some(EditDone::Failed("the editor is not ready yet".into()))
        );

        // A draft still out when the link ends ends with the reason, and the tab keeps its text.
        w.one.set_drive(Rc::new(|_: EditorAct<'_>, _: Option<String>| Ok(())));
        let (request, cut) = scratch.prepare_edit("fourth draft").unwrap();
        tabs.borrow_mut().insert(cut.id, (TabId(7), None));
        w.two.request(&request, &|| None).unwrap();
        w.two.unlinked();
        assert_eq!(cut.poll(), Some(EditDone::Failed(UNLINKED_NOTE.into())));
        scratch.cleanup();
    }

    /// A draft edit window 2 sent out, as window 1's nvim was handed it, with the end window 2's panel polls.
    fn out_draft(
        w: &Windows,
        driven: &Driven,
        scratch: &mut ScratchDir,
        text: &str,
        tab: u64,
    ) -> (ScratchAsk, PendingEdit) {
        let tabs: Rc<RefCell<BTreeMap<u64, (TabId, Option<String>)>>> = Rc::default();
        let (request, edit) = scratch.prepare_edit(text).unwrap();
        tabs.borrow_mut()
            .insert(edit.id, (TabId(tab), Some(format!("s-{tab}"))));
        {
            let tabs = tabs.clone();
            w.two
                .set_draft_tab(Rc::new(move |edit: u64| tabs.borrow().get(&edit).cloned()));
        }
        let before = driven.borrow().len();
        w.two.request(&request, &|| None).unwrap();
        assert!(w.settle(|| driven.borrow().len() > before));
        let ask = driven.borrow()[before].0.clone().unwrap();
        (ask, edit)
    }

    /// A draft of window 2's still out in nvim when the window closes is not handed to the window opened in its
    /// place: that window's tab of the same number, editing another chat's draft, keeps it, and its own draft still
    /// comes back to it.
    #[test]
    fn a_draft_from_a_window_that_went_does_not_reach_the_one_opened_in_its_place() {
        let w = windows("draft-stale");
        let driven = recording_drive(&w);
        let mut scratch = ScratchDir::in_dir(w.two.scratch.as_deref().unwrap()).unwrap();
        let (old_ask, _old) = out_draft(&w, &driven, &mut scratch, "the old chat's draft", 4);

        // Window 2 closes and is opened again under the same peer; its tab 4 holds another chat, whose draft goes out
        // before the old one is written.
        w.two.unlinked();
        w.one.unlinked(SECOND_PEER);
        let (one_link, two_link) = linked_pair();
        let again = Window2Editor::new(&w.root, w.two.scratch.clone(), w.steps.repeat());
        again.linked(&two_link);
        w.one.linked(SECOND_PEER, WindowName::Second, &one_link);
        let (request, edit) = scratch.prepare_edit("the new chat's draft").unwrap();
        let id = edit.id;
        again.set_draft_tab(Rc::new(move |edit: u64| {
            (edit == id).then(|| (TabId(4), Some("s-new".to_string())))
        }));
        again.request(&request, &|| None).unwrap();
        assert!(settle(&one_link, &two_link, &w.steps, || driven.borrow().len() == 2));
        let new_ask = driven.borrow()[1].0.clone().unwrap();

        nvim_writes(&old_ask, "edited for the old chat");
        assert!(settle(&one_link, &two_link, &w.steps, || w.one.drafts.borrow().len() == 1));
        pump_a_while(&one_link, &two_link, &w.steps);
        assert_eq!(
            edit.poll(),
            None,
            "the new window's draft is untouched by the old window's"
        );

        // An answer for another chat's session never takes this tab's draft by its number.
        one_link
            .send(
                Msg::DraftBack {
                    tab: 4,
                    session: Some("s-4".into()),
                    text: "a stray answer".into(),
                    end: DraftEnd::Written,
                    token: None,
                },
                Vec::new(),
            )
            .unwrap();
        pump_a_while(&one_link, &two_link, &w.steps);
        assert_eq!(edit.poll(), None, "a draft is found by its session");

        nvim_writes(&new_ask, "edited for the new chat");
        assert!(settle(&one_link, &two_link, &w.steps, || edit.poll().is_some()));
        assert_eq!(edit.poll(), Some(EditDone::Written("edited for the new chat".into())));
        scratch.cleanup();
    }

    #[test]
    fn a_draft_edited_past_a_packets_length_comes_back_whole() {
        let w = windows("long-draft");
        let driven = recording_drive(&w);
        let mut scratch = ScratchDir::in_dir(w.two.scratch.as_deref().unwrap()).unwrap();
        let (ask, edit) = out_draft(&w, &driven, &mut scratch, "a short draft", 4);
        let pasted = "a pasted log line, long enough to pass one packet\n".repeat(4_000);
        nvim_writes(&ask, &pasted);
        assert!(w.settle(|| edit.poll().is_some()));
        assert_eq!(
            edit.poll(),
            Some(EditDone::Written(pasted.clone())),
            "a draft of {} bytes reaches the tab",
            pasted.len()
        );

        // Past even a body's bound, the edit still ends, with the reason, and the tab keeps its draft.
        let (ask, edit) = out_draft(&w, &driven, &mut scratch, "another short draft", 5);
        nvim_writes(&ask, &"x".repeat(17 * 1024 * 1024));
        assert!(w.settle(|| edit.poll().is_some()));
        match edit.poll() {
            Some(EditDone::Failed(why)) => assert!(why.contains("could not be handed back"), "{why}"),
            other => panic!("an edit too large to hand back ends as failed, not {other:?}"),
        }
        scratch.cleanup();
    }

    #[test]
    fn a_draft_quit_or_failed_in_nvim_ends_as_it_did_in_window_2() {
        let w = windows("draft-ends");
        let driven = recording_drive(&w);
        let mut scratch = ScratchDir::in_dir(w.two.scratch.as_deref().unwrap()).unwrap();

        let (ask, quit) = out_draft(&w, &driven, &mut scratch, "kept", 4);
        let ScratchAsk::Edit { done, .. } = &ask else {
            panic!("{ask:?}")
        };
        std::fs::write(done, "discarded").unwrap();
        assert!(w.settle(|| quit.poll().is_some()));
        assert_eq!(
            quit.poll(),
            Some(EditDone::Discarded),
            "a quit leaves the tab's draft alone"
        );

        let (ask, failed) = out_draft(&w, &driven, &mut scratch, "kept too", 5);
        let ScratchAsk::Edit { done, .. } = &ask else {
            panic!("{ask:?}")
        };
        std::fs::write(done, "error: E212: cannot open file for writing").unwrap();
        assert!(w.settle(|| failed.poll().is_some()));
        assert_eq!(
            failed.poll(),
            Some(EditDone::Failed("E212: cannot open file for writing".into())),
            "window 2's user is shown the reason the edit failed"
        );
        scratch.cleanup();
    }

    fn view(owner: &str, seq: u64, cause: ViewCause, token: Option<u64>) -> ViewLine {
        ViewLine {
            owner: owner.into(),
            seq,
            buf: 3,
            name: "/p/README.md".into(),
            filetype: "markdown".into(),
            tick: 9,
            current: true,
            line_count: 40,
            top: 5,
            top_fraction: 0.25,
            bottom: 30,
            cursor_line: 6,
            cursor_col: 2,
            cause,
            token,
            scrolloff: Some(4),
            rows: Some(46),
            cursor_row: Some(7),
        }
    }

    #[test]
    fn follow_lines_read_back_as_the_socket_reads_them() {
        let mut older = view("markdown", 3, ViewCause::User, None);
        older.rows = None;
        older.cursor_row = None;
        let old_wire: serde_json::Value = serde_json::from_str(&view_line(&older)).unwrap();
        assert!(old_wire.get("rows").is_none());
        assert!(old_wire.get("cursor_row").is_none());
        for line in [
            older,
            view("markdown", 1, ViewCause::User, None),
            view("markdown", 2, ViewCause::Reveal, Some(4)),
        ] {
            assert_eq!(
                follow_feed::parse_line(view_line(&line).as_bytes()),
                Some((Some(line), None))
            );
        }
        for line in [
            Ordered::Cmd {
                owner: "markdown".into(),
                cmd: FollowCmd::Auto,
                buf: Some(3),
            },
            Ordered::Cmd {
                owner: "markdown".into(),
                cmd: FollowCmd::Toggle,
                buf: None,
            },
            Ordered::Gone {
                owner: "markdown".into(),
                buf: 3,
            },
        ] {
            assert_eq!(
                follow_feed::parse_line(ordered_line(&line).as_bytes()),
                Some((None, Some(line)))
            );
        }
    }

    #[test]
    fn a_follow_line_for_a_page_the_other_window_holds_goes_there() {
        let w = windows("follow");
        let got: Rc<RefCell<Vec<FollowBatch>>> = Rc::default();
        {
            let got = got.clone();
            w.two
                .on_follow(Rc::new(move |batch: FollowBatch| got.borrow_mut().push(batch)));
        }
        let batch = |owner: &str| FollowBatch {
            views: vec![view(owner, 1, ViewCause::User, None)],
            ordered: vec![
                Ordered::Cmd {
                    owner: owner.into(),
                    cmd: FollowCmd::Show,
                    buf: None,
                },
                Ordered::Gone {
                    owner: owner.into(),
                    buf: 3,
                },
            ],
            views_received: 1,
            ordered_received: 2,
        };
        let mut both = batch("markdown");
        let other = batch("snotra");
        both.views.extend(other.views.clone());
        both.ordered.extend(other.ordered.clone());

        // No page in window 2: every line is window 1's.
        assert_eq!(w.one.route_follow(both.clone()), both);

        w.one.set_plugin_in_window(
            "markdown",
            Some(SECOND_PEER),
            Some(BTreeMap::from([("work".into(), w.root.join("work/markdown"))])),
        );
        let here = w.one.route_follow(both);
        assert_eq!(
            here.views, other.views,
            "window 1 keeps the lines of the pages it holds"
        );
        assert_eq!(here.ordered, other.ordered);
        assert!(w.settle(|| got.borrow().len() == 3));
        let lines = got.borrow();
        assert_eq!(
            lines[0].views,
            batch("markdown").views,
            "the page in window 2 gets its view"
        );
        assert_eq!(
            lines[1..].iter().flat_map(|b| b.ordered.clone()).collect::<Vec<_>>(),
            batch("markdown").ordered,
            "and its commands, in their order"
        );
        drop(lines);
        w.one.set_plugin_in_window("markdown", None, None);
        let back = batch("markdown");
        assert_eq!(
            w.one.route_follow(back.clone()),
            back,
            "a returned page keeps its next view and command here"
        );
        assert!(
            follow_batch("snotra", &view_line(&view("markdown", 1, ViewCause::User, None))).is_none(),
            "a line naming another owner than its message is dropped"
        );
    }

    const A: PeerId = PeerId(16);
    const B: PeerId = PeerId(17);

    /// One window at the far end of a link of window 1's, keeping everything window 1 tells it.
    struct Linked {
        ours: LinkDriver,
        far: LinkDriver,
        heard: Rc<RefCell<Vec<Msg>>>,
    }

    impl Linked {
        fn heard(&self) -> Vec<Msg> {
            self.heard.borrow().clone()
        }

        fn kinds(&self) -> Vec<&'static str> {
            self.heard.borrow().iter().map(Msg::kind).collect()
        }

        fn count(&self) -> usize {
            self.heard.borrow().len()
        }

        fn send(&self, msg: Msg) {
            self.far.send(msg, Vec::new()).unwrap();
        }
    }

    /// Window 1 in a project with `src/a.rs`, with no window linked yet.
    struct Group {
        _dir: Scratch,
        root: PathBuf,
        steps: Steps,
        one: Rc<Window1Editor>,
    }

    fn group(label: &str) -> Group {
        let dir = Scratch::new(label);
        let base = std::fs::canonicalize(&dir.1).unwrap();
        let root = base.join("project");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn a() {}\n").unwrap();
        std::fs::create_dir_all(base.join("scratch-1")).unwrap();
        let steps = Steps::default();
        let one = Window1Editor::new(&root, Some(base.join("scratch-1")), steps.repeat());
        Group {
            _dir: dir,
            root,
            steps,
            one,
        }
    }

    impl Group {
        fn link(&self, peer: PeerId, name: WindowName) -> Linked {
            let (ours, far) = linked_pair();
            let heard: Rc<RefCell<Vec<Msg>>> = Rc::default();
            for kind in ["context", "theme", "keys", "follow", "draft_back", "editor_reply"] {
                let heard = heard.clone();
                far.on(kind, move |_, msg, _| heard.borrow_mut().push(msg));
            }
            self.one.linked(peer, name, &ours);
            Linked { ours, far, heard }
        }

        fn step(&self, windows: &[&Linked]) {
            for window in windows {
                window.ours.pump();
                window.far.pump();
            }
            self.steps.run();
        }

        fn settle(&self, windows: &[&Linked], done: impl Fn() -> bool) -> bool {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                self.step(windows);
                if done() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            false
        }

        /// Runs everything a while, for checking that nothing more arrives.
        fn pump_a_while(&self, windows: &[&Linked]) {
            for _ in 0..20 {
                self.step(windows);
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn roots(&self) -> BTreeMap<String, PathBuf> {
            BTreeMap::from([("work".into(), self.root.clone())])
        }
    }

    /// Holds every editor call's answer for the test to give.
    #[derive(Default)]
    struct Holding(RefCell<Vec<Answer>>);

    impl EditorRpc for Holding {
        fn exec_lua(&self, _code: &'static str, _args: Vec<rmpv::Value>) -> Pending {
            let (answer, pending) = Pending::pair();
            self.0.borrow_mut().push(answer);
            pending
        }
        fn target(&self) -> Option<u64> {
            Some(1)
        }
    }

    fn buffer_state(id: u64, path: &str) -> Msg {
        Msg::EditorOp {
            id,
            op: EditorOp::BufferState {
                path: path.into(),
                lines: None,
            },
        }
    }

    #[test]
    fn a_context_change_reaches_every_linked_window_once() {
        let g = group("every-window");
        let (a, b) = (g.link(A, WindowName::Chat(1)), g.link(B, WindowName::Chat(2)));
        g.one.context_changed(Some(&context("/p/a.rs")));
        assert!(g.settle(&[&a, &b], || a.count() == 1 && b.count() == 1));

        g.one.context_changed(Some(&context("/p/a.rs")));
        g.pump_a_while(&[&a, &b]);
        assert_eq!(a.kinds(), ["context"], "an unchanged context is not told again");
        assert_eq!(b.kinds(), ["context"], "an unchanged context is not told again");
    }

    #[test]
    fn a_new_link_is_told_everything_and_an_old_one_only_changes() {
        let g = group("late-link");
        let a = g.link(A, WindowName::Chat(1));
        g.one.context_changed(Some(&context("/p/a.rs")));
        g.one.theme_changed(&theme("dark"));
        g.one.keys_changed(&keys(500));
        assert!(g.settle(&[&a], || a.count() == 3));

        let b = g.link(B, WindowName::Page("markdown".into()));
        assert!(g.settle(&[&a, &b], || b.count() == 3));
        g.pump_a_while(&[&a, &b]);
        assert_eq!(b.kinds(), ["context", "theme", "keys"]);
        assert_eq!(a.count(), 3, "a window that heard it all hears nothing of the new link");

        g.one.theme_changed(&theme("light"));
        assert!(g.settle(&[&a, &b], || a.count() == 4 && b.count() == 4));
        assert_eq!(a.heard()[3], b.heard()[3], "a change reaches both alike");
    }

    #[test]
    fn an_editor_reply_goes_back_on_the_link_the_op_came_from() {
        let g = group("replies");
        let rpc = Rc::new(Holding::default());
        g.one.set_rpc(rpc.clone());
        let (a, b) = (g.link(A, WindowName::Chat(1)), g.link(B, WindowName::Chat(2)));
        // Each window numbers its own operations, so the same id comes from both, and both are out at once.
        a.send(buffer_state(7, "src/a.rs"));
        assert!(g.settle(&[&a, &b], || rpc.0.borrow().len() == 1));
        b.send(buffer_state(7, "src/a.rs"));
        assert!(g.settle(&[&a, &b], || rpc.0.borrow().len() == 2));

        let answer = |value: &str| rpc.0.borrow_mut().pop().unwrap().send(Ok(rmpv::Value::from(value)));
        answer("for b");
        assert!(g.settle(&[&a, &b], || b.count() == 1));
        answer("for a");
        assert!(g.settle(&[&a, &b], || a.count() == 1));
        g.pump_a_while(&[&a, &b]);
        let reply = |value: &str| Msg::EditorReply {
            id: 7,
            result: ReplyResult::Ok { value: json!(value) },
        };
        assert_eq!(a.heard(), [reply("for a")]);
        assert_eq!(b.heard(), [reply("for b")]);

        // A refusal, which no editor call stands behind, goes the same way.
        b.send(buffer_state(8, "../outside"));
        assert!(g.settle(&[&a, &b], || b.count() == 2));
        g.pump_a_while(&[&a, &b]);
        assert!(matches!(
            &b.heard()[1],
            Msg::EditorReply {
                id: 8,
                result: ReplyResult::Err { .. }
            }
        ));
        assert_eq!(a.count(), 1, "the other window hears nothing of it");
    }

    #[test]
    fn an_operation_out_when_its_link_ends_leaves_the_other_windows_to_be_answered() {
        let g = group("reply-after-end");
        let rpc = Rc::new(Holding::default());
        g.one.set_rpc(rpc.clone());
        let (a, b) = (g.link(A, WindowName::Chat(1)), g.link(B, WindowName::Chat(2)));
        a.send(buffer_state(1, "src/a.rs"));
        b.send(buffer_state(1, "src/a.rs"));
        assert!(g.settle(&[&a, &b], || rpc.0.borrow().len() == 2));

        g.one.unlinked(A);
        for answer in rpc.0.borrow_mut().drain(..) {
            answer.send(Ok(rmpv::Value::Nil));
        }
        assert!(g.settle(&[&a, &b], || b.count() == 1));
        g.pump_a_while(&[&a, &b]);
        assert_eq!(a.count(), 0, "the ended link has nowhere to be answered");
    }

    #[test]
    fn a_draft_ends_in_the_window_that_holds_its_tab() {
        let g = group("draft-window");
        let driven = drive_recorded(&g.one);
        let (a, b) = (g.link(A, WindowName::Chat(1)), g.link(B, WindowName::Chat(2)));
        // A tab's number is its own window's, so both windows can have a tab 4 with a draft out.
        for (at, window) in [&a, &b].into_iter().enumerate() {
            window.send(Msg::Editor {
                request: EditorRequest::Scratch {
                    tab: 4,
                    session: None,
                    text: "a draft".into(),
                },
                token: None,
                answer: None,
            });
            assert!(g.settle(&[&a, &b], || driven.borrow().len() == at + 1));
        }
        let ask = |at: usize| driven.borrow()[at].0.clone().unwrap();
        let back = |text: &str| Msg::DraftBack {
            tab: 4,
            session: None,
            text: text.into(),
            end: DraftEnd::Written,
            token: None,
        };

        nvim_writes(&ask(1), "edited for b");
        assert!(g.settle(&[&a, &b], || b.count() == 1));
        g.pump_a_while(&[&a, &b]);
        assert_eq!(b.heard(), [back("edited for b")]);
        assert_eq!(a.count(), 0, "the window that asked hears, and no other");

        nvim_writes(&ask(0), "edited for a");
        assert!(g.settle(&[&a, &b], || a.count() == 1));
        g.pump_a_while(&[&a, &b]);
        assert_eq!(a.heard(), [back("edited for a")]);
        assert_eq!(b.count(), 1);
    }

    #[test]
    fn a_follow_line_goes_to_the_window_holding_the_page() {
        let g = group("follow-window");
        let (a, b) = (
            g.link(A, WindowName::Chat(1)),
            g.link(B, WindowName::Page("markdown".into())),
        );
        let batch = |seq: u64| FollowBatch {
            views: vec![view("markdown", seq, ViewCause::User, None)],
            views_received: 1,
            ..FollowBatch::default()
        };
        let line = |seq: u64| Msg::Follow {
            owner: "markdown".into(),
            line: view_line(&view("markdown", seq, ViewCause::User, None)),
        };

        g.one.set_plugin_in_window("markdown", Some(B), Some(g.roots()));
        assert!(g.one.route_follow(batch(1)).views.is_empty());
        assert!(g.settle(&[&a, &b], || b.count() == 1));
        g.pump_a_while(&[&a, &b]);
        assert_eq!(b.heard(), [line(1)]);
        assert_eq!(a.count(), 0, "only the page's window hears its lines");

        // The page moves: the next line follows it.
        g.one.set_plugin_in_window("markdown", Some(A), Some(g.roots()));
        assert!(g.one.route_follow(batch(2)).views.is_empty());
        assert!(g.settle(&[&a, &b], || a.count() == 1));
        g.pump_a_while(&[&a, &b]);
        assert_eq!(a.heard(), [line(2)]);
        assert_eq!(b.count(), 1);

        g.one.set_plugin_in_window("markdown", None, None);
        assert_eq!(
            g.one.route_follow(batch(3)),
            batch(3),
            "back in this window, its lines stay here"
        );
    }

    #[test]
    fn a_page_window_holds_its_plugin_once_it_links_and_a_lua_panels_holds_none() {
        let g = group("page-window-links");
        let rpc = Rc::new(Answering {
            calls: RefCell::default(),
            answer: rmpv::Value::Nil,
        });
        g.one.set_rpc(rpc.clone());
        let roots = g.roots();
        g.one
            .set_page_roots(Rc::new(move |id| (id == "markdown").then(|| roots.clone())));
        let line = || FollowBatch {
            views: vec![view("markdown", 1, ViewCause::User, None)],
            views_received: 1,
            ..FollowBatch::default()
        };
        assert_eq!(g.one.route_follow(line()), line(), "no window holds it yet");

        let (page, panel) = (
            g.link(A, WindowName::Page("markdown".into())),
            g.link(B, WindowName::Page("preview_panel".into())),
        );
        assert_eq!(g.one.follow_owners(), BTreeSet::from(["markdown".to_string()]));
        assert!(g.one.route_follow(line()).views.is_empty());
        assert!(g.settle(&[&page, &panel], || page.count() == 1));
        assert!(matches!(&page.heard()[0], Msg::Follow { owner, .. } if owner == "markdown"));

        page.send(Msg::EditorOp {
            id: 1,
            op: EditorOp::PluginAssistClear {
                plugin: "markdown".into(),
            },
        });
        assert!(g.settle(&[&page, &panel], || page.count() == 2));
        assert!(
            matches!(
                &page.heard()[1],
                Msg::EditorReply {
                    result: ReplyResult::Ok { .. },
                    ..
                }
            ),
            "{:?}",
            page.heard()
        );

        g.one.unlinked(A);
        assert_eq!(g.one.route_follow(line()), line(), "the page ended with its window");
        assert_eq!(panel.count(), 0);
    }

    #[test]
    fn a_page_a_window_will_hold_is_its_before_the_link_and_ends_with_it() {
        let g = group("held-before-link");
        let line = || FollowBatch {
            views: vec![view("markdown", 1, ViewCause::User, None)],
            views_received: 1,
            ..FollowBatch::default()
        };
        g.one.set_plugin_in_window("markdown", Some(A), Some(g.roots()));
        g.one.set_plugin_in_window("snotra", Some(B), Some(g.roots()));
        assert!(
            g.one.route_follow(line()).views.is_empty(),
            "a page restored away is not this window's, though its window has not linked yet"
        );

        let (a, b) = (g.link(A, WindowName::Chat(1)), g.link(B, WindowName::Chat(2)));
        assert!(g.one.route_follow(line()).views.is_empty());
        assert!(g.settle(&[&a, &b], || a.count() == 1));

        g.one.unlinked(A);
        assert_eq!(
            g.one.route_follow(line()),
            line(),
            "a window that ended holds no page any more"
        );
        assert_eq!(
            g.one.follow_owners(),
            BTreeSet::from(["snotra".to_string()]),
            "and the other window keeps the page it holds"
        );
    }

    #[test]
    fn a_window_reaches_the_editor_only_for_the_page_it_holds() {
        let g = group("page-window");
        let rpc = Rc::new(Answering {
            calls: RefCell::default(),
            answer: rmpv::Value::Nil,
        });
        g.one.set_rpc(rpc.clone());
        let (a, b) = (g.link(A, WindowName::Chat(1)), g.link(B, WindowName::Chat(2)));
        g.one.set_plugin_in_window("markdown", Some(B), Some(g.roots()));
        let clear = |id: u64| Msg::EditorOp {
            id,
            op: EditorOp::PluginAssistClear {
                plugin: "markdown".into(),
            },
        };

        a.send(clear(1));
        assert!(g.settle(&[&a, &b], || a.count() == 1));
        assert!(
            matches!(
                &a.heard()[0],
                Msg::EditorReply {
                    result: ReplyResult::Err { .. },
                    ..
                }
            ),
            "a window is refused a page another window holds: {:?}",
            a.heard()
        );
        assert!(rpc.calls.borrow().is_empty());

        b.send(clear(1));
        assert!(g.settle(&[&a, &b], || b.count() == 1));
        assert_eq!(rpc.calls.borrow().len(), 1, "the holder's own call reaches the editor");
    }

    #[test]
    fn a_review_clear_from_a_window_the_driver_draws_nothing_for_clears_nothing() {
        let g = group("drawn-window");
        let path = g.root.join("src/a.rs");
        g.one.set_drawn_for(Rc::new({
            let path = path.clone();
            move |peer: PeerId| {
                if peer == SECOND_PEER {
                    vec![path.clone()]
                } else {
                    Vec::new()
                }
            }
        }));
        let cleared: Rc<RefCell<Vec<usize>>> = Rc::default();
        {
            let cleared = cleared.clone();
            g.one
                .set_overlay(Rc::new(move |_: PeerId, _: u64, calls: Vec<OverlayCall>| {
                    cleared.borrow_mut().push(calls.len());
                    Some(ReplyResult::Ok { value: Value::Null })
                }));
        }
        let (two, a) = (g.link(SECOND_PEER, WindowName::Second), g.link(A, WindowName::Chat(1)));
        let clear = || Msg::EditorOp {
            id: 1,
            op: EditorOp::ReviewClearMine {},
        };
        two.send(clear());
        assert!(g.settle(&[&two, &a], || two.count() == 1));
        a.send(clear());
        assert!(g.settle(&[&two, &a], || a.count() == 1));
        assert_eq!(*cleared.borrow(), [1, 0], "window 2 clears what was drawn for it");
    }

    #[test]
    fn unlinking_one_window_keeps_the_others_feeds() {
        let g = group("one-link-ends");
        let (a, b) = (g.link(A, WindowName::Chat(1)), g.link(B, WindowName::Chat(2)));
        g.one.theme_changed(&theme("dark"));
        assert!(g.settle(&[&a, &b], || a.count() == 1 && b.count() == 1));

        g.one.unlinked(A);
        g.one.theme_changed(&theme("light"));
        assert!(g.settle(&[&a, &b], || b.count() == 2));
        g.pump_a_while(&[&a, &b]);
        assert_eq!(a.count(), 1, "the ended link hears nothing more");
    }

    #[test]
    fn an_editor_answer_survives_json_in_its_shape() {
        use rmpv::Value as M;
        let value = M::Array(vec![
            M::Nil,
            M::from(true),
            M::from(-3),
            M::from(u64::MAX),
            M::F64(1.5),
            M::F64(2.0),
            M::F32(0.5),
            M::from("text"),
            raw_string(b"\xc3\x28").unwrap(),
            raw_string(&[b'x'; 300]).unwrap(),
            M::Binary(vec![0, 255]),
            M::Map(vec![(M::from(1), M::from("one")), (M::from("k"), M::Map(Vec::new()))]),
            M::Ext(3, vec![1, 2]),
            M::Array(Vec::new()),
        ]);
        let json = msgpack_to_json(&value).unwrap();
        let travelled: Value = serde_json::from_str(&json.to_string()).unwrap();
        assert_eq!(json_to_msgpack(&travelled).unwrap(), value);
        assert!(msgpack_to_json(&M::F64(f64::NAN)).is_err());
        assert!(
            json_to_msgpack(&json!({"a": 1})).is_err(),
            "a plain object is not one of the encodings"
        );
    }
}
