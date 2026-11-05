//! One plugin's side of the page bridge: every verb a page sends, checked against the capabilities its
//! manifest declares and answered through one funnel to the page. Nothing here touches a toolkit or waits:
//! the page, the layout and the editor are reached through small traits the window implements, and task
//! output, editor answers and store answers are taken by `pump`, which the window runs when the host's
//! wake is raised or when `pump` asked for a timer.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::doc_route::{self, DocSlot};
use super::editor::{self, decode_apply, decode_clear, decode_open, read_applied, FollowInstall, PulledText};
use super::follow::{Done, FollowOp, FollowState, Next, Parked, MAX_TEXT_BYTES, MAX_TEXT_LINES};
use super::follow_feed::ViewLine;
use super::keys::{self, EffectiveRow, PluginKeyOp};
use super::kit_settings::KeysSettings;
use super::load::{LoadedPlugin, ResolvedTask, TaskRefusal};
use super::manifest::Capability;
use super::page_policy;
use super::protocol::{
    self, ClosedReason, Cursor, DocPrefixes, EditorClosed, EditorJump, EditorOpenLink, EditorReveal, EditorText,
    EditorView, ErrorCode, ExitStatus, FocusEvent, FocusTarget, Inbound, KeyRowArg, KeysConfig, NoArgs, OnOff,
    PageSaved, Reply, StreamName, TaskExit, TaskOutput, TooLarge, Verb, EVENT_EDITOR_CLOSED, EVENT_EDITOR_TEXT,
    EVENT_EDITOR_VIEW,
};
use super::roots::{self, PathRefusal};
use super::state::{PluginStore, StoreError, StoreErrorKind, StoreOp, StoreWorker, MAX_STORE_IN_FLIGHT};
use super::tasks::{ExitKind, Launch, RunInfo, RunsWaiter, StartError, Stream, TaskEvent, TaskRunner};
use super::theme::PageTheme;
use super::trace;
use crate::editor_rpc::EditorRpc;
use crate::keymap::{Chord, KeySpec};
use crate::nvim_rpc;

mod agents;

#[cfg(target_os = "linux")]
pub use agents::DirSource;
pub use agents::{
    something_waits, write_shared_on_thread, AgentsDone, AgentsFeed, AgentsPort, AgentsRefusal, AgentsServices,
    AgentsSource, FeedRead, GestureClock, ReaderSink, SharedWrite, WaitShow, AGENTS_DEADLINE, WATCH_DEADLINE,
};

/// How long an editor call may take before the page is told the editor is busy.
pub const EDITOR_DEADLINE: Duration = Duration::from_secs(5);
/// How often a pending editor answer is looked at, by a one-shot timer, while one is outstanding.
pub const EDITOR_POLL: Duration = Duration::from_millis(20);
/// How often a follow call past its deadline is looked at. The follow sends one call at a time, so it waits
/// for that answer before its next call, but no page waits on it any more.
pub const FOLLOW_LATE_POLL: Duration = Duration::from_millis(250);
/// How long a page-supplied path (or a task's program and working directory) may take to check before the
/// page is told the filesystem did not answer.
pub const CHECK_DEADLINE: Duration = Duration::from_secs(5);
/// How long a store operation (`state.*`, `log.*`) may take before the page is told the store did not answer.
pub const STORE_DEADLINE: Duration = Duration::from_secs(5);
/// How long a key sequence waits for its next key until the editor reports its own `timeoutlen`: vim's
/// default.
pub const DEFAULT_TIMEOUTLEN: u32 = 1000;

/// How long the window takes at most to answer a `terminal.at`, from the request: its read of the shell and a
/// restart share it, and it answers whatever still waits at its end itself.
pub const TERMINAL_AT_WAIT: Duration = Duration::from_secs(5);
/// How long a `terminal.at` is owed its answer here: past the window's own deadline, so the window's answer, which
/// says what it did, always comes first, and this one only stands for an answer that was lost.
pub const TERMINAL_AT_DEADLINE: Duration = Duration::from_secs(6);

const CHECK_TIMEOUT: &str = "the filesystem did not answer within 5 s";
const TERMINAL_LATE: &str = "the terminal did not answer within 6 s";
const _: () = assert!(
    TERMINAL_AT_DEADLINE.as_secs() == 6 && TERMINAL_AT_WAIT.as_secs() < TERMINAL_AT_DEADLINE.as_secs(),
    "the window answers before this deadline, and the late text names it"
);
/// The refusal of a verb that reshapes the window, asked by a page outside the plugin's own window.
pub const NOT_OWN_WINDOW_TEXT: &str = "This verb acts only in the plugin's own window";
const EDITOR_BUSY: &str = "the editor did not answer within 5 s; it may be waiting for a key (press Esc and try again)";

/// The page. `send` is asynchronous and never calls back into the host before it returns.
pub trait PagePort {
    fn send(&self, message: &str);
    /// The view's background and its user style sheet: what the next document starts with, and the
    /// background a reload shows before it paints.
    fn set_theme(&self, theme: &PageTheme);
}

/// The window's layout: give the keys to the editor (showing it first if hidden) or to the plugin's module, and put
/// the window's base terminal in a directory.
pub trait LayoutPort {
    fn focus(&self, target: FocusTarget) -> Result<(), String>;
    /// Asks for the base terminal in `dir` (checked already), its answer to come back through
    /// [`PluginHost::terminal_at_answered`] under `ticket`, possibly before this returns. `Err` is a refusal now.
    fn terminal_at(&self, dir: PathBuf, ticket: u64) -> Result<(), String>;
}

/// What a page's `terminal.at` came to: the directory kept for the terminal's next shell (none runs), a fresh shell
/// started there in place of an idle one, the shell already there or below it, or left alone with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalAtAnswer {
    StartDir,
    Restarted,
    Already,
    Busy(String),
}

/// A request the page is still owed an answer for, in whichever line it waits: the editor's calls, the assist
/// line, the path checks, the store, the follow queue. Each has a deadline, so no page waits on the host for
/// good.
struct Pending<T> {
    /// The page's id: `None` for a request the host made on its own (the clear when the plugin stops), and
    /// once the page has had its answer.
    re: Option<u64>,
    /// The document that asked: an answer goes to it only, never to one that replaced it.
    document: u64,
    /// When the page is told the request took too long, if nothing has answered it by then.
    deadline: Instant,
    on: T,
}

impl<T> Pending<T> {
    fn new(re: Option<u64>, document: u64, deadline: Instant, on: T) -> Pending<T> {
        Pending {
            re,
            document,
            deadline,
            on,
        }
    }
}

/// Who an answer may still reach: the current document, unless the window is closing.
#[derive(Debug, Clone, Copy)]
struct Audience {
    document: u64,
    closed: bool,
}

/// The one place a request's answer is sent or dropped. A request whose document has been replaced, or one
/// still owed when the window is closing, loses its id here with one log line (the work it asked for is not
/// undone). Otherwise `answer` gives the reply: `Some` sends it and spends the id, so no request is answered
/// twice; `None` leaves it owed.
fn answer_or_drop<T>(
    plugin: &str,
    audience: Audience,
    pending: &mut Pending<T>,
    answer: impl FnOnce(u64) -> Option<Reply>,
    replies: &mut Vec<Reply>,
) {
    let Some(re) = pending.re else { return };
    if audience.closed || pending.document != audience.document {
        pending.re = None;
        let reason = if audience.closed {
            "the window is closing"
        } else {
            "the requesting document was replaced"
        };
        log_dropped_reply(plugin, re, reason);
        return;
    }
    if let Some(reply) = answer(re) {
        pending.re = None;
        replies.push(reply);
    }
}

fn log_dropped_reply(plugin: &str, re: u64, reason: &str) {
    eprintln!("[plugin {plugin}] dropped reply {re}: {reason}");
}

/// The owed request with page id `re`, taken out of `owed`.
fn take_owed(owed: &mut Vec<Pending<()>>, re: u64) -> Option<Pending<()>> {
    let at = owed.iter().position(|p| p.re == Some(re))?;
    Some(owed.remove(at))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    Diff,
    CloseDiffs,
    Preload,
    WipePreload,
    Modified,
    Reload,
    Open,
    Cwd,
    CwdClear,
    Assist,
    Clear,
}

struct EditorCall {
    call: Call,
    answer: nvim_rpc::Pending,
}

/// An editor operation waiting for its path check. Clears stay behind checks so a stop undoes
/// every change already requested. The calls also carry their creation order to nvim, which can
/// otherwise receive a clear before the change it should undo.
enum ScopedOp {
    CheckingDiff { ticket: u64 },
    Diff(protocol::EditorDiff),
    CloseDiffs,
    Checking { ticket: u64, raw: String, op: DirOp },
    Apply { root: PathBuf, op: DirOp },
    Clear,
    CwdClear,
    WipePreload,
}

#[derive(Clone, Copy)]
enum DirOp {
    Preload { diagnostics: bool, inlay_hints: bool },
    Modified,
    Reload,
    Assist { diagnostics: bool, inlay_hints: bool },
    Cwd,
}

/// What the path checker is asked. `canonicalize` and `metadata` can stall on a slow or hung filesystem, so
/// they never run on the thread that calls the host.
enum CheckJob {
    Diff(protocol::EditorDiff),
    File(String),
    Dir(String),
    Task {
        task: String,
        vars: BTreeMap<String, String>,
    },
    /// The project of the followed document `doc` (absolute, as nvim names it).
    Project {
        doc: PathBuf,
        window_root: Option<PathBuf>,
        home: Option<PathBuf>,
    },
    /// A link under the document's approved directory, or its approved project with `from_root`.
    Link {
        project: PathBuf,
        doc_dir: PathBuf,
        path: String,
        from_root: bool,
    },
    /// A link whose document has no approved project yet: look it up on the worker before checking the link.
    LinkWithProjectLookup {
        doc: PathBuf,
        window_root: Option<PathBuf>,
        home: Option<PathBuf>,
        path: String,
        from_root: bool,
    },
}

enum Checked {
    Diff(Result<protocol::EditorDiff, (PathRefusal, String)>),
    Path(Result<PathBuf, PathRefusal>),
    Task(Result<ResolvedTask, TaskRefusal>),
    /// The canonical project directory and the canonical directory of the document, or `None` with no project.
    Project(Option<(PathBuf, PathBuf)>),
}

/// A document's project and its own directory, both canonical. Reads the filesystem: the path worker only.
fn project_of(doc: &Path, window_root: Option<&Path>, home: Option<&Path>) -> Option<(PathBuf, PathBuf)> {
    let project = doc_route::project_dir(doc, window_root, home)?;
    let doc_dir = std::fs::canonicalize(doc.parent()?).ok()?;
    Some((project, doc_dir))
}

/// Where a document's link leads: a regular file inside the document's project, canonical. A document is
/// untrusted text, so its links may name anything; only its own project is in reach, by canonical path, so
/// neither `..` nor a symlink leads out. Reads the filesystem: the path worker only.
fn link_inside(project: &Path, doc_dir: &Path, path: &str, from_root: bool) -> Result<PathBuf, PathRefusal> {
    if std::fs::canonicalize(project).ok().as_deref() != Some(project)
        || std::fs::canonicalize(doc_dir).ok().as_deref() != Some(doc_dir)
    {
        return Err(PathRefusal::OutsideRoots);
    }
    let base = if from_root { project } else { doc_dir };
    let canonical = std::fs::canonicalize(base.join(path)).map_err(|_| PathRefusal::Missing)?;
    // A link inside the project can lead to a file whose own name holds a control character.
    if roots::has_control(&canonical.to_string_lossy()) {
        return Err(PathRefusal::ControlCharacter);
    }
    if !canonical.starts_with(project) {
        return Err(PathRefusal::OutsideRoots);
    }
    let meta = std::fs::metadata(&canonical).map_err(|_| PathRefusal::Missing)?;
    if meta.is_file() {
        Ok(canonical)
    } else {
        Err(PathRefusal::NotRegularFile)
    }
}

/// The followed buffer's file, when it names one: nvim names a file's buffer by its absolute path, and anything
/// else (an unnamed buffer, a `term://` or other plugin buffer) has no file and so no project.
fn doc_path(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    (!name.is_empty() && path.is_absolute()).then(|| path.to_path_buf())
}

/// The line a link's fragment names: `L<n>` or `L<n>-L<m>`, `n` at least 1. Any other fragment is the page's to
/// act on.
fn line_fragment(fragment: &str) -> Option<u32> {
    let number = |text: &str| -> Option<u32> {
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        text.parse().ok()
    };
    let rest = fragment.strip_prefix('L')?;
    let (first, end) = match rest.split_once('-') {
        Some((first, end)) => (first, Some(end)),
        None => (rest, None),
    };
    if let Some(end) = end {
        number(end.strip_prefix('L')?)?;
    }
    number(first).filter(|n| *n >= 1)
}

/// One worker thread per plugin, started at the first check. Checks run in order; one stalled on the
/// filesystem holds the ones behind it, which then run out their deadlines rather than piling up threads.
struct PathChecker {
    tx: mpsc::Sender<(u64, CheckJob)>,
    rx: mpsc::Receiver<(u64, Checked)>,
}

impl PathChecker {
    fn spawn(plugin: LoadedPlugin, wake: agent::UiWake, gate: Option<mpsc::Receiver<()>>) -> std::io::Result<Self> {
        let (tx, jobs) = mpsc::channel::<(u64, CheckJob)>();
        let (done, rx) = mpsc::channel();
        std::thread::Builder::new().name("plugin-paths".into()).spawn(move || {
            for (ticket, job) in jobs {
                if let Some(gate) = &gate {
                    // A closed gate lets everything through.
                    let _ = gate.recv();
                }
                let checked = match job {
                    CheckJob::Diff(mut args) => {
                        let result = (|| {
                            for side in [&mut args.left, &mut args.right] {
                                if let protocol::DiffSide::Path { path } = side {
                                    let raw = path.to_string_lossy().into_owned();
                                    *path = roots::file_inside(&plugin.roots, &raw).map_err(|e| (e, raw))?;
                                }
                            }
                            Ok(args)
                        })();
                        Checked::Diff(result)
                    }
                    CheckJob::File(raw) => Checked::Path(roots::file_inside(&plugin.roots, &raw)),
                    CheckJob::Dir(raw) => Checked::Path(roots::dir_inside(&plugin.roots, &raw)),
                    CheckJob::Task { task, vars } => Checked::Task(plugin.resolve_task(&task, &vars)),
                    CheckJob::Project { doc, window_root, home } => {
                        Checked::Project(project_of(&doc, window_root.as_deref(), home.as_deref()))
                    }
                    CheckJob::Link {
                        project,
                        doc_dir,
                        path,
                        from_root,
                    } => Checked::Path(link_inside(&project, &doc_dir, &path, from_root)),
                    CheckJob::LinkWithProjectLookup {
                        doc,
                        window_root,
                        home,
                        path,
                        from_root,
                    } => Checked::Path(
                        project_of(&doc, window_root.as_deref(), home.as_deref())
                            .ok_or(PathRefusal::OutsideRoots)
                            .and_then(|(project, doc_dir)| link_inside(&project, &doc_dir, &path, from_root)),
                    ),
                };
                if done.send((ticket, checked)).is_err() {
                    return;
                }
                wake();
            }
        })?;
        Ok(PathChecker { tx, rx })
    }
}

/// The one follow call out to the editor: the host's own (install, follow, text, teardown) or a page's. Its
/// document is the one that was current when it was sent; a text or a reply for another one is dropped.
struct FollowCall {
    op: FollowOp,
    answer: nvim_rpc::Pending,
    /// The editor session it was sent to; a call to an earlier session is given up, not waited for.
    session: Option<u64>,
}

/// An `editor.open`, `editor.open_link` or `task.start` waiting for its check.
struct Checking {
    ticket: u64,
    then: Then,
}

enum Then {
    Open {
        raw: String,
        line: Option<u32>,
        col: Option<u32>,
    },
    Task {
        task: String,
        vars: BTreeMap<String, String>,
    },
    /// An `editor.open_link`: the target as the page sent it, the line its fragment names, and the editor
    /// session it was asked in.
    Link {
        target: String,
        line: Option<u32>,
        session: Option<u64>,
    },
    /// A `terminal.at`: the directory as the page sent it.
    TerminalAt { raw: String },
}

/// What [`PluginHost::take_saved`] found.
#[derive(Debug, Clone, PartialEq)]
pub enum SaveOutcome {
    /// The page's state, as it answered `host.save`.
    Saved(Value),
    /// The deadline passed with no answer, or the page could not answer (it had not said hello, or its document
    /// was replaced): the move goes on without state.
    TimedOut,
    /// Asked, not answered, and the deadline is still ahead.
    Waiting,
    /// Nothing was asked since the last outcome was taken.
    NotAsked,
    /// The plugin's manifest has no `[page] handoff = true`: its page is reopened, not handed over.
    NotHandoff,
}

/// How long a page has to answer `host.save` before a move goes on without its state.
pub const SAVE_DEADLINE: Duration = Duration::from_secs(1);

/// A `host.save` sent to the page, until its outcome is taken.
struct SaveAsk {
    deadline: Instant,
    answer: Option<Value>,
    /// No answer can come: the page was not listening when asked, or the asked document is gone.
    unanswerable: bool,
}

struct PluginHostState {
    /// The page said `host.hello` since it last started loading: events reach it from then on.
    hello: bool,
    /// The window is closing: the page may still be alive for a moment, but nothing it sends is acted on, so
    /// no run can start after the shutdown's list of runs to wait for was taken.
    closed: bool,
    moving: bool,
    moved_away: bool,
    /// Counts documents: raised by every reload and by a stop, so an answer still out for an older one
    /// is never sent into the document that replaced it.
    document: u64,
    editor: Option<Rc<dyn EditorRpc>>,
    /// Where the editor beside the page draws nvim's grid: the share of its height above the first row, read as
    /// each view goes out. Only a window that draws nvim's grid itself has it.
    grid_top: Option<Rc<dyn Fn() -> Option<f64>>>,
    layout: Option<Rc<dyn LayoutPort>>,
    tasks: TaskRunner,
    store: Option<StoreWorker>,
    /// The store worker carries a ticket rather than the page's id: ticket -> the request.
    store_waiting: BTreeMap<u64, Pending<()>>,
    /// How long a store operation may take; `STORE_DEADLINE` unless a test shortened it.
    store_deadline: Duration,
    /// Handed to the store worker when it starts: each operation then waits for one message (tests only).
    store_gate: Option<mpsc::Receiver<()>>,
    next_ticket: u64,
    calls: Vec<Pending<EditorCall>>,
    /// An assist was sent and not cleared since.
    assisted: bool,
    cwd_set: bool,
    preloaded: bool,
    diffed: bool,
    /// Editor changes and clears in request order, held only while a path check is outstanding.
    parked: VecDeque<Pending<ScopedOp>>,
    checker: Option<PathChecker>,
    checking: Vec<Pending<Checking>>,
    /// Handed to the checker when it starts: each check then waits for one message (tests only).
    check_gate: Option<mpsc::Receiver<()>>,
    /// `terminal.at` requests handed to the window, each under its ticket, until the window answers.
    terminal_at: Vec<Pending<u64>>,
    /// Whether the page's module holds the keys of an active window, as the window last said.
    has_keys: bool,
    /// The editor's `timeoutlen`, in milliseconds, as last reported.
    timeoutlen: u32,
    /// The window's prefix key: a page may not bind it.
    prefix_key: Chord,
    /// The same prefix as a person reads it, which the page is told for its own key help.
    prefix_label: String,
    /// The key that shows this plugin's module after the prefix, if the registration gave one.
    module_key: Option<String>,
    keys_settings: KeysSettings,
    /// The window's appearance as this page last received it.
    theme: PageTheme,
    /// The current document's declared key table, with the user's overrides applied.
    declared: Option<Vec<EffectiveRow>>,
    /// What to ask nvim next for a page that follows the editor's buffer.
    follow: FollowState,
    follow_call: Option<Pending<FollowCall>>,
    /// The page's reveals, jumps and links waiting in the follow queue: the queue sends one call at a time and
    /// waits for its answer, so they are owed their answer here, by their own deadline, however long the call
    /// ahead of them takes.
    follow_owed: Vec<Pending<()>>,
    /// The window's follow socket: until it is given, nothing of the follow state reaches nvim (an install
    /// needs somewhere to report to).
    follow_socket: Option<PathBuf>,
    /// Eitri's own scratch buffers live here; none is ever followed.
    scratch_dir: Option<PathBuf>,
    scratch_prefix: Option<PathBuf>,
    follow_refused: bool,
    follow_refused_hook: Option<Rc<dyn Fn()>>,
    /// The document that called `editor.follow`.
    page_follows_document: Option<u64>,
    /// Whether the plugin's module is drawn, as the window last said.
    on_screen: bool,
    /// The newest view of the followed buffer.
    view: Option<ViewLine>,
    /// The text (or notice) the page was last sent, for a new document that follows.
    last_text: Option<EditorText>,
    /// The followed buffer's end, for a document that was not on screen to hear of it; a new view clears it.
    closed_notice: Option<EditorClosed>,
    /// Command names the user already had, each logged once.
    skipped: Vec<String>,
    /// The last failure of a follow call, logged once until a call succeeds.
    follow_error: Option<String>,
    /// Whether the last `pump` asked for a timer.
    timer_armed: bool,
    /// The module was stopped: the snippet is taken down and stays down until the window restarts.
    follow_stopped: bool,
    /// What the document route serves, shared with the page's scheme handler; without one no project is looked
    /// up and views carry no prefixes.
    doc_slot: Option<Rc<RefCell<DocSlot>>>,
    /// The window's project root, which a document inside it belongs to.
    window_root: Option<PathBuf>,
    /// The user's home, which is never a document's project.
    home: Option<PathBuf>,
    /// Each followed buffer name's project and document directory (`None`: no project), as the path worker
    /// found them.
    projects: BTreeMap<String, Option<(PathBuf, PathBuf)>>,
    /// Project lookups out on the path worker: ticket -> buffer name.
    project_asked: BTreeMap<u64, String>,
    save: Option<SaveAsk>,
    /// The state a page in another window handed over, for the next `host.hello` reply only.
    restored: Option<Value>,
    /// The current document said `page.ready` and nobody has taken it since.
    ready: bool,
    /// How many documents the view has committed: the number a trace line gives the document it is about, so a
    /// reader can tell the document a page built ahead of its state from the one that reloaded with it.
    committed: u64,
    /// What the page watches of the agents layer, and its verbs still out.
    agents: agents::HostAgents,
}

pub struct PluginHost {
    own_window: Cell<bool>,
    plugin: Rc<LoadedPlugin>,
    page: Rc<dyn PagePort>,
    wake: agent::UiWake,
    store: Option<PluginStore>,
    /// `init.lua`'s `plugin:<id>` table, applied to every declaration the page makes.
    overrides: Vec<PluginKeyOp>,
    state: RefCell<PluginHostState>,
}

fn on_off(on: bool) -> &'static str {
    if on {
        "on"
    } else {
        "off"
    }
}

/// Whether follow events reach the page now: it said hello, called `editor.follow`, and is on screen.
fn deliverable(st: &PluginHostState) -> bool {
    st.hello && st.page_follows_document == Some(st.document) && st.on_screen
}

/// The follow snippet's `{ok = true}`.
fn answer_ok(value: &rmpv::Value) -> Result<(), String> {
    let ok = value
        .as_map()
        .and_then(|m| m.iter().find(|(k, _)| k.as_str() == Some("ok")))
        .and_then(|(_, v)| v.as_bool());
    match ok {
        Some(true) => Ok(()),
        Some(false) => Err("the editor has no follow snippet installed for this plugin".to_string()),
        None => Err("the editor's answer has no ok".to_string()),
    }
}

/// A view line as the page receives it; the document route's prefixes are not known here.
fn editor_view(v: &ViewLine) -> EditorView {
    EditorView {
        buf: v.buf,
        name: v.name.clone(),
        filetype: v.filetype.clone(),
        tick: v.tick,
        current: v.current,
        line_count: v.line_count,
        top: v.top,
        top_fraction: v.top_fraction,
        bottom: v.bottom,
        cursor: Cursor {
            line: v.cursor_line,
            col: v.cursor_col,
        },
        cause: v.cause,
        token: v.token,
        doc: None,
        scrolloff: v.scrolloff,
        rows: v.rows,
        cursor_row: v.cursor_row,
        grid_top: None,
    }
}

/// A link's refusal: as a path's, except that a link's bounds are its document's project.
fn link_refusal(re: u64, refusal: PathRefusal, target: &str) -> Reply {
    match refusal {
        PathRefusal::OutsideRoots => Reply::err(
            re,
            ErrorCode::OutsideRoots,
            &format!("\"{}\" is outside the document's project", target.escape_debug()),
        ),
        other => path_refusal(re, other, target),
    }
}

fn path_refusal(re: u64, refusal: PathRefusal, raw: &str) -> Reply {
    let code = match refusal {
        PathRefusal::NotAbsolute | PathRefusal::ControlCharacter => ErrorCode::BadRequest,
        PathRefusal::OutsideRoots => ErrorCode::OutsideRoots,
        PathRefusal::Missing | PathRefusal::NotRegularFile | PathRefusal::NotDirectory => ErrorCode::Io,
    };
    Reply::err(re, code, &refusal.message(raw))
}

fn store_reply(re: u64, result: Result<Value, StoreError>) -> Reply {
    match result {
        Ok(value) => Reply::ok(re, value),
        Err(e) => {
            let code = match e.kind {
                StoreErrorKind::BadRequest => ErrorCode::BadRequest,
                StoreErrorKind::TooLarge => ErrorCode::TooLarge,
                StoreErrorKind::Io => ErrorCode::Io,
            };
            Reply::err(re, code, &e.message)
        }
    }
}

fn event_message(event: &TaskEvent) -> String {
    match event {
        TaskEvent::Output {
            run,
            stream,
            lines,
            dropped_lines,
            dropped_bytes,
        } => protocol::event_text(
            "task.output",
            &TaskOutput {
                run: *run,
                stream: match stream {
                    Stream::Stdout => StreamName::Stdout,
                    Stream::Stderr => StreamName::Stderr,
                },
                lines: lines.clone(),
                dropped_lines: *dropped_lines,
                dropped_bytes: *dropped_bytes,
            },
        ),
        TaskEvent::Exit {
            run,
            kind,
            dropped_lines,
            dropped_bytes,
        } => {
            let (status, code) = match kind {
                ExitKind::Exited(code) => (ExitStatus::Exited, Some(*code)),
                ExitKind::Killed(_) => (ExitStatus::Killed, None),
                ExitKind::Timeout => (ExitStatus::Timeout, None),
                ExitKind::Cancelled => (ExitStatus::Cancelled, None),
                ExitKind::SpawnFailed(_) => (ExitStatus::SpawnFailed, None),
            };
            protocol::event_text(
                "task.exit",
                &TaskExit {
                    run: *run,
                    status,
                    code,
                    dropped_lines: *dropped_lines,
                    dropped_bytes: *dropped_bytes,
                },
            )
        }
    }
}

impl PluginHost {
    pub fn new(
        plugin: Rc<LoadedPlugin>,
        page: Rc<dyn PagePort>,
        wake: agent::UiWake,
        store: Option<PluginStore>,
        overrides: Vec<PluginKeyOp>,
    ) -> PluginHost {
        let stock_prefix = KeySpec::parse(crate::keymap::stock::STOCK_PREFIX).expect("the stock prefix parses");
        let state = PluginHostState {
            hello: false,
            closed: false,
            moving: false,
            moved_away: false,
            document: 0,
            editor: None,
            grid_top: None,
            layout: None,
            tasks: TaskRunner::new(wake.clone()),
            store: None,
            store_waiting: BTreeMap::new(),
            store_deadline: STORE_DEADLINE,
            store_gate: None,
            next_ticket: 1,
            calls: Vec::new(),
            assisted: false,
            cwd_set: false,
            preloaded: false,
            diffed: false,
            parked: VecDeque::new(),
            checker: None,
            checking: Vec::new(),
            check_gate: None,
            terminal_at: Vec::new(),
            has_keys: false,
            timeoutlen: DEFAULT_TIMEOUTLEN,
            prefix_key: Chord::of(&stock_prefix),
            prefix_label: stock_prefix.human(),
            module_key: None,
            keys_settings: KeysSettings::default(),
            theme: PageTheme::fallback(),
            declared: None,
            follow: FollowState::new(),
            follow_call: None,
            follow_owed: Vec::new(),
            follow_socket: None,
            scratch_dir: None,
            scratch_prefix: None,
            follow_refused: false,
            follow_refused_hook: None,
            page_follows_document: None,
            on_screen: false,
            view: None,
            last_text: None,
            closed_notice: None,
            skipped: Vec::new(),
            follow_error: None,
            timer_armed: false,
            follow_stopped: false,
            doc_slot: None,
            window_root: None,
            home: std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from),
            projects: BTreeMap::new(),
            project_asked: BTreeMap::new(),
            save: None,
            restored: None,
            ready: false,
            committed: 0,
            agents: agents::HostAgents::default(),
        };
        PluginHost {
            own_window: Cell::new(false),
            plugin,
            page,
            wake,
            store,
            overrides,
            state: RefCell::new(state),
        }
    }

    /// Whether this host serves the plugin's own window, as set by the window controller.
    pub fn own_window(&self) -> bool {
        self.own_window.get()
    }

    pub fn set_own_window(&self, own: bool) {
        self.own_window.set(own);
    }

    pub fn plugin(&self) -> &LoadedPlugin {
        &self.plugin
    }

    pub fn set_editor(&self, editor: Rc<dyn EditorRpc>) {
        self.state.borrow_mut().editor = Some(editor);
    }

    /// The editor's grid top (`EditorView::grid_top`), read as each view goes to the page.
    pub fn set_grid_top(&self, source: Rc<dyn Fn() -> Option<f64>>) {
        self.state.borrow_mut().grid_top = Some(source);
    }

    pub fn set_layout(&self, layout: Rc<dyn LayoutPort>) {
        self.state.borrow_mut().layout = Some(layout);
    }

    /// The document route's slot, the same one the page's scheme handler reads: from here on the followed
    /// document's project is looked up, the slot holds it, and views carry the route's prefixes.
    pub fn set_doc_slot(&self, slot: Rc<RefCell<DocSlot>>) {
        self.state.borrow_mut().doc_slot = Some(slot);
    }

    /// The window's project root: a followed document inside it has it as its project.
    pub fn set_window_root(&self, root: PathBuf) {
        self.state.borrow_mut().window_root = Some(root);
    }

    /// The window's prefix key, which a page's declaration may not hold. Until this is called it is the
    /// stock prefix. A page that has said hello is told its new spelling.
    pub fn set_prefix_key(&self, prefix: &KeySpec) {
        let label = prefix.human();
        let hello = {
            let mut st = self.state.borrow_mut();
            st.prefix_key = Chord::of(prefix);
            if st.prefix_label == label {
                return;
            }
            st.prefix_label = label;
            st.hello
        };
        if hello {
            self.send_keys_config();
        }
    }

    /// The key that shows this plugin's module after the prefix, as the registration wrote it (a single
    /// character, which is its own spelling), or `None`. A page that has said hello is told when it changes.
    pub fn set_module_key(&self, key: Option<&str>) {
        let hello = {
            let mut st = self.state.borrow_mut();
            if st.module_key.as_deref() == key {
                return;
            }
            st.module_key = key.map(str::to_owned);
            st.hello
        };
        if hello {
            self.send_keys_config();
        }
    }

    /// The window's theme for this page. Same set as before: nothing. Otherwise it is recorded, the page
    /// port takes it (background and user sheet), and the current document gets a `theme` event, whether
    /// or not it has said hello.
    pub fn set_theme(&self, theme: &PageTheme) {
        {
            let mut st = self.state.borrow_mut();
            if st.theme == *theme {
                return;
            }
            st.theme = theme.clone();
        }
        // The port first: a reload that starts before the event already gets the new sheet.
        self.page.set_theme(theme);
        self.send_theme();
    }

    pub fn theme(&self) -> PageTheme {
        self.state.borrow().theme.clone()
    }

    /// A document was committed in the view (a load, a reload, a crash's recovery, a back/forward-cache
    /// restore): it is a new document, which must say hello again, and nothing still out for the old one is
    /// answered. It gets the current `theme` event at once. A load that only started is not a new document:
    /// one that never commits (a download, an ignored response, a cancelled navigation) leaves the running
    /// document its hello, its table and its events.
    pub fn page_committed(&self) {
        self.state.borrow_mut().committed += 1;
        self.new_document();
        self.send_theme();
    }

    fn send_theme(&self) {
        let data = self.state.borrow().theme.to_json();
        self.page.send(&protocol::event_text("theme", &data));
    }

    /// Read at startup, so these settings reach the page in hello and never as an event.
    pub fn set_keys_settings(&self, settings: KeysSettings) {
        self.state.borrow_mut().keys_settings = settings;
    }

    /// The page's keyboard settings, all three each time, so a page never merges a partial update.
    fn send_keys_config(&self) {
        let config = {
            let st = self.state.borrow();
            KeysConfig {
                timeoutlen: st.timeoutlen,
                prefix: st.prefix_label.clone(),
                module_key: st.module_key.clone(),
            }
        };
        self.page.send(&protocol::event_text("keys_config", &config));
    }

    /// Whether the page's module holds the keys; the page is told once it has said hello.
    pub fn set_has_keys(&self, has_keys: bool) {
        let hello = {
            let mut st = self.state.borrow_mut();
            st.has_keys = has_keys;
            st.hello
        };
        if hello {
            self.send_focus();
        }
    }

    /// The editor's `timeoutlen`; a page that has said hello is told when it changes.
    pub fn set_timeoutlen(&self, ms: u32) {
        let hello = {
            let mut st = self.state.borrow_mut();
            if st.timeoutlen == ms {
                return;
            }
            st.timeoutlen = ms;
            st.hello
        };
        if hello {
            self.send_keys_config();
        }
    }

    /// The current document's key table with the user's overrides applied, or `None` before it declared
    /// one.
    pub fn declared_rows(&self) -> Option<Vec<EffectiveRow>> {
        self.state.borrow().declared.clone()
    }

    fn send_focus(&self) {
        let has_keys = self.state.borrow().has_keys;
        self.page.send(&protocol::event_text("focus", &FocusEvent { has_keys }));
    }

    /// Holds every path check until the returned sender sends one message for it, so a test can stand in for
    /// a filesystem that does not answer. Dropping the sender lets every check through. Must be called before
    /// the first check.
    #[cfg(any(test, feature = "test-support"))]
    pub fn gate_path_checks(&self) -> mpsc::Sender<()> {
        let (tx, rx) = mpsc::channel();
        self.state.borrow_mut().check_gate = Some(rx);
        tx
    }

    /// Holds every store operation until the returned sender sends one message for it, so a test can stand in
    /// for a lock another window holds or a disk that does not answer. Dropping the sender lets every operation
    /// through. Must be called before the first store operation.
    #[cfg(any(test, feature = "test-support"))]
    pub fn gate_store(&self) -> mpsc::Sender<()> {
        let (tx, rx) = mpsc::channel();
        self.state.borrow_mut().store_gate = Some(rx);
        tx
    }

    /// How long a store operation may take before the page is told it did not answer.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_store_deadline(&self, deadline: Duration) {
        self.state.borrow_mut().store_deadline = deadline;
    }

    /// Who an answer may reach now.
    fn audience(&self) -> Audience {
        let st = self.state.borrow();
        Audience {
            document: st.document,
            closed: st.closed,
        }
    }

    /// Sends what a step owes the page, in order.
    fn send_replies(&self, replies: Vec<Reply>) {
        for reply in replies {
            self.reply(reply);
        }
    }

    /// Puts a page's reveal, jump or link in the follow queue, owed its answer by `deadline`. Returns what the
    /// page is still owed for the reveal or jump it replaced, or answers a link refused by the queue's cap.
    fn park_page_op(&self, op: FollowOp, re: u64, deadline: Instant, replies: &mut Vec<Reply>) -> Option<Pending<()>> {
        let audience = self.audience();
        let mut st = self.state.borrow_mut();
        let parked = st.follow.park(op);
        let document = st.document;
        let mut pending = Pending::new(Some(re), document, deadline, ());
        if parked == Parked::Full {
            answer_or_drop(
                self.id(),
                audience,
                &mut pending,
                |re| Some(Reply::err(re, ErrorCode::EditorTimeout, EDITOR_BUSY)),
                replies,
            );
            return None;
        }
        st.follow_owed.push(pending);
        match parked {
            Parked::Replaced(old) => take_owed(&mut st.follow_owed, old),
            _ => None,
        }
    }

    /// Tells the page about each reveal, jump or link still in the follow queue past its deadline. The op keeps
    /// its place and is still sent; its answer then goes to nobody.
    fn expire_follow_owed(&self, now: Instant, replies: &mut Vec<Reply>) {
        let audience = self.audience();
        let mut st = self.state.borrow_mut();
        st.follow_owed.retain_mut(|owed| {
            if now < owed.deadline {
                return true;
            }
            answer_or_drop(
                self.id(),
                audience,
                owed,
                |re| Some(Reply::err(re, ErrorCode::EditorTimeout, EDITOR_BUSY)),
                replies,
            );
            false
        });
    }

    /// Hands a check to the worker; the ticket names its answer.
    fn submit_check(&self, job: CheckJob) -> Result<u64, String> {
        let mut st = self.state.borrow_mut();
        if st.checker.is_none() {
            let gate = st.check_gate.take();
            let spawned = PathChecker::spawn((*self.plugin).clone(), self.wake.clone(), gate);
            st.checker = Some(spawned.map_err(|e| format!("the path checker's thread could not start: {e}"))?);
        }
        let ticket = st.next_ticket;
        st.next_ticket += 1;
        if let Some(checker) = st.checker.as_ref() {
            let _ = checker.tx.send((ticket, job));
        }
        Ok(ticket)
    }

    fn check_then(&self, re: u64, job: CheckJob, then: Then, now: Instant) -> Option<Reply> {
        let ticket = match self.submit_check(job) {
            Ok(ticket) => ticket,
            Err(why) => return Some(Reply::err(re, ErrorCode::Io, &why)),
        };
        let mut st = self.state.borrow_mut();
        let document = st.document;
        st.checking.push(Pending::new(
            Some(re),
            document,
            now + CHECK_DEADLINE,
            Checking { ticket, then },
        ));
        None
    }

    /// Whether the window should hand this plugin its follow socket: the manifest lets the page follow the
    /// editor's buffer, or defines nvim commands (which report through the same socket).
    pub fn wants_follow_feed(&self) -> bool {
        let manifest = &self.plugin.manifest;
        manifest.has(Capability::EditorFollow) || manifest.has(Capability::EditorCommands)
    }

    /// The window's follow socket, which nvim reports views and commands to, and Eitri's scratch directory,
    /// whose buffers are never followed. The snippet is installed from the next `pump` on.
    pub fn set_follow_feed(&self, socket: PathBuf, scratch_dir: Option<PathBuf>) {
        {
            let mut st = self.state.borrow_mut();
            st.follow_socket = Some(socket);
            st.scratch_dir = scratch_dir;
        }
        (self.wake)();
    }

    /// Excludes scratch buffers belonging to every window sharing this editor.
    pub fn set_scratch_prefix(&self, prefix: Option<PathBuf>) {
        self.state.borrow_mut().scratch_prefix = prefix;
    }

    /// Called once when the editor refuses this connection's follow glue. No follow call is retried after it.
    pub fn on_follow_refused(&self, hook: Rc<dyn Fn()>) {
        self.state.borrow_mut().follow_refused_hook = Some(hook);
    }

    /// Whether the plugin's module is drawn. nvim follows only while it is and the document follows; a
    /// document that comes back on screen is given the newest view, and the text if it moved meanwhile.
    pub fn set_on_screen(&self, on: bool, _now: Instant) {
        let back = {
            let mut st = self.state.borrow_mut();
            let back = on && !st.on_screen;
            st.on_screen = on;
            st.follow.on_screen(on);
            back
        };
        if back {
            self.follow_catch_up();
            // A watching document starts again from the whole view.
            self.agents_back_on_screen();
            (self.wake)();
        }
        self.wake_follow();
    }

    /// A view line from nvim for this plugin. A scratch buffer's, or one of a filetype the manifest does not
    /// follow, is dropped.
    pub fn follow_view(&self, view: ViewLine, _now: Instant) {
        if view.owner != self.id() || !self.follows_filetype(&view.filetype) {
            return;
        }
        let scratch = {
            let st = self.state.borrow();
            st.scratch_dir
                .as_deref()
                .is_some_and(|dir| crate::scratch::holds(dir, &view.name))
        };
        if scratch {
            return;
        }
        self.track_project(&view.name);
        let event = self.view_event(&view);
        let deliver = {
            let mut st = self.state.borrow_mut();
            st.follow.view(view.buf, view.tick);
            st.closed_notice = None;
            st.view = Some(view);
            deliverable(&st)
        };
        if deliver {
            self.page.send(&protocol::event_text(EVENT_EDITOR_VIEW, &event));
        }
        self.wake_follow();
    }

    /// A view line as the page receives it, with the document route's prefixes once the document's project is
    /// known and is what the route serves.
    fn view_event(&self, view: &ViewLine) -> EditorView {
        let mut event = editor_view(view);
        event.doc = self.doc_prefixes(&view.name);
        let source = self.state.borrow().grid_top.clone();
        event.grid_top = source
            .and_then(|read| read())
            .filter(|g| g.is_finite() && (0.0..1.0).contains(g));
        event
    }

    fn doc_prefixes(&self, name: &str) -> Option<DocPrefixes> {
        let st = self.state.borrow();
        let slot = st.doc_slot.as_ref()?.borrow();
        let (project, doc_dir) = st.projects.get(name)?.as_ref()?;
        // The slot follows the newest view only; prefixes for any other project would not resolve.
        if slot.dir.as_ref() != Some(project) {
            return None;
        }
        Some(doc_route::doc_prefixes(self.id(), slot.generation, project, doc_dir))
    }

    /// The newest view names this buffer: the route serves its project when it is known (a change starts a
    /// new generation, so addresses made for another project stop resolving), and it is looked up on the path
    /// worker when it is not; only a found project is cached by name.
    fn track_project(&self, name: &str) {
        let job = {
            let st = self.state.borrow();
            let Some(slot) = st.doc_slot.clone() else { return };
            let Some(doc) = doc_path(name) else {
                slot.borrow_mut().set_dir(None);
                return;
            };
            if let Some(found) = st.projects.get(name) {
                slot.borrow_mut()
                    .set_dir(found.as_ref().map(|(project, _)| project.clone()));
                return;
            }
            if st.project_asked.values().any(|asked| asked == name) {
                return;
            }
            CheckJob::Project {
                doc,
                window_root: st.window_root.clone(),
                home: st.home.clone(),
            }
        };
        match self.submit_check(job) {
            Ok(ticket) => {
                self.state.borrow_mut().project_asked.insert(ticket, name.to_owned());
            }
            Err(why) => eprintln!(
                "[plugin {}] the document's project could not be looked up: {why}",
                self.id()
            ),
        }
    }

    /// The path worker found a buffer name's project. For the newest view's name the route moves to it and a
    /// page that hears views gets the view again, now with the route's prefixes.
    fn project_found(&self, name: String, found: Option<(PathBuf, PathBuf)>) {
        let (slot, view, deliver) = {
            let mut st = self.state.borrow_mut();
            if found.is_some() {
                st.projects.insert(name.clone(), found.clone());
            }
            let view = st.view.clone().filter(|v| v.name == name);
            (st.doc_slot.clone(), view, deliverable(&st))
        };
        let (Some(slot), Some(view)) = (slot, view) else { return };
        slot.borrow_mut().set_dir(found.map(|(project, _)| project));
        let event = self.view_event(&view);
        if deliver && event.doc.is_some() {
            self.page.send(&protocol::event_text(EVENT_EDITOR_VIEW, &event));
        }
    }

    /// nvim says a buffer was wiped: when it is the followed one, the page is told and it is not pulled again.
    pub fn follow_gone(&self, buf: u64, _now: Instant) {
        if self.follows_buffer(buf) {
            self.buffer_closed(buf, ClosedReason::Wiped);
        }
        self.wake_follow();
    }

    /// Whether the page is viewing this buffer or holds its text: its end is news to the page only then,
    /// and only once.
    fn follows_buffer(&self, buf: u64) -> bool {
        let st = self.state.borrow();
        st.view.as_ref().is_some_and(|v| v.buf == buf) || st.last_text.as_ref().is_some_and(|t| t.buf == buf)
    }

    /// The editor's nvim ended: a following page is told (`editor.closed {reason: "editor"}`). An nvim the pane
    /// starts again later is a new session, which [`editor_session`](Self::editor_session) installs into.
    pub fn editor_closed(&self, _now: Instant) {
        let buf = {
            let st = self.state.borrow();
            st.view
                .as_ref()
                .map(|v| v.buf)
                .or_else(|| st.last_text.as_ref().map(|t| t.buf))
        };
        // A page that followed no buffer still learns that there is no editor to follow; 0 names no buffer.
        self.buffer_closed(buf.unwrap_or(0), ClosedReason::Editor);
    }

    /// The `[follow]` table's `auto_show` setting: whether nvim asks to show the module when a buffer of a
    /// followed filetype is entered. False without one.
    pub fn auto_show(&self) -> bool {
        self.plugin
            .manifest
            .follow
            .as_ref()
            .and_then(|f| f.auto_show.as_deref())
            .and_then(|name| self.plugin.bool_setting(name))
            .unwrap_or(false)
    }

    /// Looks at the editor's session now: a new one is installed (and followed again when wanted), and an
    /// install nvim answered early (before VimEnter) is due at once. The window calls it from its theme and
    /// keys feeds' reports, which come at VimEnter; `pump` makes the same session check without the prompt.
    pub fn editor_session(&self, now: Instant) {
        if self.state.borrow().follow_socket.is_none() {
            return;
        }
        self.follow_session(now);
        self.state.borrow_mut().follow.entered();
        (self.wake)();
    }

    /// Command names the user already had when the snippet was installed.
    #[cfg(any(test, feature = "test-support"))]
    pub fn skipped_commands(&self) -> Vec<String> {
        self.state.borrow().skipped.clone()
    }

    fn follows_filetype(&self, filetype: &str) -> bool {
        self.plugin
            .manifest
            .follow
            .as_ref()
            .is_some_and(|f| f.filetypes.iter().any(|t| t == filetype))
    }

    /// Asks the window to run `pump`, which sends the follow state's next call.
    fn wake_follow(&self) {
        if self.state.borrow().follow_socket.is_some() {
            (self.wake)();
        }
    }

    /// The followed buffer ended: forgotten, and the page told when it can hear it (else when it next can).
    fn buffer_closed(&self, buf: u64, reason: ClosedReason) {
        let closed = EditorClosed { buf, reason };
        let deliver = {
            let mut st = self.state.borrow_mut();
            st.follow.gone(buf);
            if st.view.as_ref().is_some_and(|v| v.buf == buf) || reason == ClosedReason::Editor {
                st.view = None;
            }
            if st.last_text.as_ref().is_some_and(|t| t.buf == buf) || reason == ClosedReason::Editor {
                st.last_text = None;
            }
            st.closed_notice = Some(closed);
            deliverable(&st)
        };
        if deliver {
            self.page.send(&protocol::event_text(EVENT_EDITOR_CLOSED, &closed));
        }
    }

    /// What a document that just started hearing the follow events lacks: the newest view, the text sent
    /// last when the page does not hold it, or the end of the buffer it followed.
    fn follow_catch_up(&self) {
        let (view, text, closed) = {
            let st = self.state.borrow();
            if !deliverable(&st) {
                return;
            }
            let text = match (&st.view, &st.last_text) {
                (Some(v), Some(t)) if t.buf == v.buf && st.follow.page_text() != Some((t.buf, t.tick)) => {
                    Some(t.clone())
                }
                _ => None,
            };
            let closed = if st.view.is_none() { st.closed_notice } else { None };
            (st.view.clone(), text, closed)
        };
        if let Some(view) = view {
            self.page
                .send(&protocol::event_text(EVENT_EDITOR_VIEW, &self.view_event(&view)));
        }
        if let Some(text) = text {
            self.page.send(&protocol::event_text(EVENT_EDITOR_TEXT, &text));
            self.state.borrow_mut().follow.page_has(text.buf, text.tick);
        }
        if let Some(closed) = closed {
            self.page.send(&protocol::event_text(EVENT_EDITOR_CLOSED, &closed));
        }
    }

    /// The answer to a follow request once follow is stopped: the editor's refusal of the slot is a different reason
    /// from the module having been stopped, and the page is told which.
    fn stopped_follow_reply(&self, re: u64) -> Reply {
        let message = if self.state.borrow().follow_refused {
            "the editor refused the follow slot"
        } else {
            "stopped with prefix x; the plugin comes back when the window restarts"
        };
        Reply::err(re, ErrorCode::EditorUnavailable, message)
    }

    fn editor_follow(&self, re: u64) -> Option<Reply> {
        let (hello, stopped) = {
            let st = self.state.borrow();
            (st.hello, st.follow_stopped)
        };
        if !hello {
            return Some(Reply::err(
                re,
                ErrorCode::BadRequest,
                "editor.follow comes after host.hello",
            ));
        }
        if stopped {
            return Some(self.stopped_follow_reply(re));
        }
        {
            let mut st = self.state.borrow_mut();
            st.page_follows_document = Some(st.document);
            st.follow.page_follows(true);
        }
        let filetypes = self
            .plugin
            .manifest
            .follow
            .as_ref()
            .map(|f| f.filetypes.clone())
            .unwrap_or_default();
        // The reply first, so the page knows it follows before the first event arrives.
        self.reply(Reply::ok(re, json!({ "filetypes": filetypes })));
        self.follow_catch_up();
        self.wake_follow();
        None
    }

    /// Notes the editor's session; a call still out to an earlier one is given up rather than waited for,
    /// since the process it went to is gone.
    fn follow_session(&self, now: Instant) {
        let target = {
            let st = self.state.borrow();
            st.editor.as_ref().and_then(|e| e.target())
        };
        let changed = self.state.borrow_mut().follow.session(target);
        if !changed {
            return;
        }
        let audience = self.audience();
        let (stale, waiting) = {
            let mut st = self.state.borrow_mut();
            // Buffer numbers and ticks belong to one nvim.
            st.view = None;
            st.last_text = None;
            let stale = st.follow_call.take_if(|c| c.on.session != target);
            // A reveal, jump or link still in line names the old editor's buffers (or was asked of it): sent to
            // the next one, it could move another buffer's cursor or open a file the user has moved on from.
            let waiting: Vec<Pending<()>> = st
                .follow
                .drop_page_ops()
                .into_iter()
                .filter_map(|re| take_owed(&mut st.follow_owed, re))
                .collect();
            (stale, waiting)
        };
        let restarted = |re| Some(Reply::err(re, ErrorCode::EditorUnavailable, "the editor restarted"));
        let mut replies = Vec::new();
        if let Some(mut call) = stale {
            answer_or_drop(self.id(), audience, &mut call, restarted, &mut replies);
            self.state.borrow_mut().follow.done(Done::Failed, now);
        }
        for mut owed in waiting {
            answer_or_drop(self.id(), audience, &mut owed, restarted, &mut replies);
        }
        self.send_replies(replies);
    }

    fn follow_failed(&self, what: &str, why: &str) {
        let message = format!("{what}: {why}");
        let fresh = {
            let mut st = self.state.borrow_mut();
            let fresh = st.follow_error.as_deref() != Some(message.as_str());
            st.follow_error = Some(message.clone());
            fresh
        };
        if fresh {
            eprintln!("[plugin {}] {message}", self.id());
        }
    }

    /// Takes the follow call's answer if it has come, tells a page past the deadline that the editor is busy,
    /// and keeps a late call tracked: the follow sends its next call only once this one is answered, so an
    /// nvim held in a prompt is not sent a call every few seconds.
    fn take_follow_answer(&self, now: Instant, replies: &mut Vec<Reply>) {
        let audience = self.audience();
        let taken = {
            let mut st = self.state.borrow_mut();
            let Some(call) = st.follow_call.as_mut() else { return };
            // A replaced document's id is let go at once; the call itself stays tracked until nvim answers.
            answer_or_drop(self.id(), audience, call, |_| None, replies);
            match call.on.answer.try_take() {
                Some(answer) => st.follow_call.take().map(|call| (call, answer)),
                None => {
                    if now >= call.deadline {
                        answer_or_drop(
                            self.id(),
                            audience,
                            call,
                            |re| Some(Reply::err(re, ErrorCode::EditorTimeout, EDITOR_BUSY)),
                            replies,
                        );
                    }
                    None
                }
            }
        };
        let Some((mut call, answer)) = taken else { return };
        let done = match answer {
            Ok(value) => {
                let in_time = now < call.deadline;
                let current = call.document == audience.document;
                self.follow_answered(&mut call, audience, &value, current, in_time, replies)
            }
            Err(e) => {
                let unavailable = |re| {
                    Some(Reply::err(
                        re,
                        ErrorCode::EditorUnavailable,
                        &format!("the editor could not be asked: {e}"),
                    ))
                };
                answer_or_drop(self.id(), audience, &mut call, unavailable, replies);
                self.follow_failed("a follow call failed", &e.to_string());
                Done::Failed
            }
        };
        if done != Done::Failed {
            self.state.borrow_mut().follow_error = None;
        }
        self.state.borrow_mut().follow.done(done, now);
    }

    /// Acts on one follow answer; `current` is whether the document that was current when it was asked
    /// still is, `in_time` whether the answer was taken before the call's deadline.
    fn follow_answered(
        &self,
        call: &mut Pending<FollowCall>,
        audience: Audience,
        value: &rmpv::Value,
        current: bool,
        in_time: bool,
        replies: &mut Vec<Reply>,
    ) -> Done {
        let flag = |key: &str| {
            value.as_map().and_then(|fields| {
                fields
                    .iter()
                    .find(|(k, _)| k.as_str() == Some(key))
                    .and_then(|(_, v)| v.as_bool())
            })
        };
        if flag("ok") == Some(false) && flag("slot") == Some(false) {
            let hook = {
                let mut st = self.state.borrow_mut();
                if st.follow_refused {
                    None
                } else {
                    st.follow_refused = true;
                    st.follow_stopped = true;
                    st.follow_refused_hook.clone()
                }
            };
            answer_or_drop(
                self.id(),
                audience,
                call,
                |re| {
                    Some(Reply::err(
                        re,
                        ErrorCode::EditorUnavailable,
                        "the editor refused the follow slot",
                    ))
                },
                replies,
            );
            if let Some(hook) = hook {
                hook();
            }
            return Done::Failed;
        }
        // A copy, so the page's answer below can be given through `answer_or_drop`, which takes the call.
        let op = call.on.op.clone();
        match &op {
            FollowOp::Install => match editor::read_installed(value) {
                Ok(installed) if installed.early => Done::Early,
                Ok(installed) => {
                    for name in installed.skipped {
                        let fresh = {
                            let mut st = self.state.borrow_mut();
                            let fresh = !st.skipped.contains(&name);
                            if fresh {
                                st.skipped.push(name.clone());
                            }
                            fresh
                        };
                        if fresh {
                            eprintln!("[plugin {}] :{name} is already defined; not replacing it", self.id());
                        }
                    }
                    Done::Installed
                }
                Err(why) => {
                    self.follow_failed("the follow snippet could not be installed", &why);
                    Done::Failed
                }
            },
            FollowOp::Follow(on) => match answer_ok(value) {
                Ok(()) => Done::Followed(*on),
                Err(why) => {
                    self.follow_failed("the editor could not follow", &why);
                    Done::Failed
                }
            },
            FollowOp::Text { .. } => match editor::read_text(value) {
                Ok(PulledText::Gone { buf }) => {
                    // A wiped line for this buffer may have told the page already.
                    if self.follows_buffer(buf) {
                        self.buffer_closed(buf, ClosedReason::Wiped);
                    }
                    Done::Gone { buf }
                }
                // Pulled for a document that has since been replaced: the new one asks again when it follows.
                Ok(_) if !current => Done::Other,
                // The view moved to another buffer, or this one closed, while the pull was out: its text is
                // no longer what the page shows, and the newest view's buffer is pulled next.
                Ok(PulledText::Lines { buf, .. } | PulledText::TooLarge { buf, .. })
                    if self.view_name(buf).is_none() =>
                {
                    Done::Other
                }
                Ok(PulledText::Lines { buf, tick, lines }) => {
                    let text = lines.join("\n");
                    let bytes = text.len() as u64;
                    trace::line(self.id(), "pull", &format!("buf={buf} tick={tick} bytes={bytes}"));
                    self.pulled(EditorText {
                        buf,
                        tick,
                        name: self.view_name(buf).unwrap_or_default(),
                        text: Some(text),
                        too_large: None,
                    });
                    Done::Pulled { buf, tick, bytes }
                }
                Ok(PulledText::TooLarge {
                    buf,
                    tick,
                    bytes,
                    lines,
                }) => {
                    trace::line(self.id(), "pull", &format!("buf={buf} tick={tick} bytes={bytes}"));
                    self.pulled(EditorText {
                        buf,
                        tick,
                        name: self.view_name(buf).unwrap_or_default(),
                        text: None,
                        too_large: Some(TooLarge {
                            bytes,
                            lines,
                            max_bytes: MAX_TEXT_BYTES,
                            max_lines: MAX_TEXT_LINES,
                        }),
                    });
                    // Too many lines makes a buffer as slow to render as too many bytes, whatever its size.
                    Done::Pulled {
                        buf,
                        tick,
                        bytes: bytes.max(super::follow::LARGE_TEXT_BYTES + 1),
                    }
                }
                Err(why) => {
                    self.follow_failed("the buffer's text could not be read", &why);
                    Done::Failed
                }
            },
            FollowOp::Reveal { line, .. } | FollowOp::Jump { line, .. } => {
                let applied = read_applied(value);
                let (event, focus) = match &op {
                    FollowOp::Jump { focus, .. } => ("jump", *focus),
                    _ => ("reveal", false),
                };
                // A reveal's cursor, when the page sent one, is named too: what the page asked nvim's cursor to hold.
                let cursor = match &op {
                    FollowOp::Reveal {
                        cursor: Some(cursor), ..
                    } => format!(" cursor={cursor}"),
                    _ => String::new(),
                };
                let row = match &op {
                    FollowOp::Reveal { row: Some(row), .. } => format!(" row={row}"),
                    // A jump's selection: its mode and the line its cursor ends on.
                    FollowOp::Jump {
                        select: Some(select), ..
                    } => format!(" select={}:{}", select.mode.as_str(), select.line),
                    _ => String::new(),
                };
                if let Ok(applied) = applied {
                    trace::line(self.id(), event, &format!("line={line}{cursor}{row} applied={applied}"));
                }
                // The keys move only for a page still waiting on this answer, and only before the deadline: one
                // told the editor was busy, or replaced since, has moved on, and so may the user have while nvim
                // was slow; the keys jumping under them then would surprise them.
                if focus && applied == Ok(true) && current && in_time && call.re.is_some() {
                    let layout = self.state.borrow().layout.clone();
                    if let Some(layout) = layout {
                        if let Err(why) = layout.focus(FocusTarget::Editor) {
                            eprintln!("[plugin {}] the editor could not take the keys: {why}", self.id());
                        }
                    }
                }
                let answer = |re| {
                    Some(match applied {
                        Ok(applied) => Reply::ok(re, json!({ "applied": applied })),
                        Err(why) => Reply::err(re, ErrorCode::Io, &why),
                    })
                };
                answer_or_drop(self.id(), audience, call, answer, replies);
                Done::Other
            }
            FollowOp::OpenLink { .. } => {
                let answer = |re| {
                    Some(match decode_open(value) {
                        Ok(opened) => Reply::ok(re, json!({ "how": opened.how, "line": opened.line })),
                        Err(why) => Reply::err(re, ErrorCode::Io, &why),
                    })
                };
                answer_or_drop(self.id(), audience, call, answer, replies);
                Done::Other
            }
            FollowOp::Teardown => Done::Other,
        }
    }

    /// The newest view's name for this buffer; `None` when the view has moved to another or the buffer closed.
    fn view_name(&self, buf: u64) -> Option<String> {
        let st = self.state.borrow();
        st.view.as_ref().filter(|v| v.buf == buf).map(|v| v.name.clone())
    }

    /// A pulled text (or its notice): sent to a document that can hear it, and kept for one that follows later.
    fn pulled(&self, text: EditorText) {
        let deliver = {
            let st = self.state.borrow();
            deliverable(&st)
        };
        if !deliver {
            return;
        }
        self.page.send(&protocol::event_text(EVENT_EDITOR_TEXT, &text));
        let mut st = self.state.borrow_mut();
        st.follow.page_has(text.buf, text.tick);
        st.last_text = Some(text);
    }

    /// Sends the follow state's next call when none is out; returns when to look again, if it is waiting
    /// for a time.
    fn send_follow(&self, now: Instant) -> Option<Instant> {
        let next = {
            let mut st = self.state.borrow_mut();
            if st.follow_refused || st.follow_call.is_some() {
                return None;
            }
            st.follow.next(now)
        };
        let op = match next {
            Next::Idle => return None,
            Next::At(at) => return Some(at),
            Next::Send(op) => op,
        };
        let Some(editor) = self.editor() else {
            self.state.borrow_mut().follow.done(Done::Failed, now);
            return None;
        };
        let session = editor.target();
        let id = self.id();
        let (answer, re) = match &op {
            FollowOp::Install => {
                let (socket, scratch_dir, scratch_prefix) = {
                    let st = self.state.borrow();
                    (
                        st.follow_socket.clone(),
                        st.scratch_dir.clone(),
                        st.scratch_prefix.clone(),
                    )
                };
                let Some(socket) = socket else {
                    self.state.borrow_mut().follow.done(Done::Failed, now);
                    return None;
                };
                let manifest = &self.plugin.manifest;
                let filetypes = manifest
                    .follow
                    .as_ref()
                    .map(|f| f.filetypes.clone())
                    .unwrap_or_default();
                let commands: Vec<(String, &'static str)> = manifest
                    .commands
                    .iter()
                    .map(|c| (c.name.clone(), c.does.as_str()))
                    .collect();
                let install = FollowInstall {
                    owner: id,
                    socket: &socket,
                    filetypes: &filetypes,
                    auto: self.auto_show(),
                    commands: &commands,
                    scratch_dir: scratch_dir.as_deref(),
                };
                (
                    editor::follow_install_with_scratch_prefix(&*editor, &install, scratch_prefix.as_deref()),
                    None,
                )
            }
            FollowOp::Follow(on) => {
                trace::line(id, "follow", on_off(*on));
                (editor::follow_on(&*editor, id, *on), None)
            }
            FollowOp::Text { buf } => (
                editor::follow_text(&*editor, id, *buf, MAX_TEXT_BYTES, MAX_TEXT_LINES),
                None,
            ),
            FollowOp::Reveal {
                buf,
                line,
                token,
                cursor,
                row,
                re,
            } => (
                editor::follow_reveal(&*editor, id, *buf, *line, *token, *cursor, *row),
                Some(*re),
            ),
            FollowOp::Jump {
                buf,
                line,
                col,
                select,
                re,
                ..
            } => (editor::follow_jump(&*editor, id, *buf, *line, *col, *select), Some(*re)),
            FollowOp::OpenLink { path, line, re } => (editor::open(&*editor, path, *line, None), Some(*re)),
            FollowOp::Teardown => (editor::follow_teardown(&*editor, id), None),
        };
        let mut st = self.state.borrow_mut();
        let document = st.document;
        // A page's op keeps what is left of its own deadline; one whose page was already told the editor is busy
        // goes out owing nobody.
        let owed = re.and_then(|re| take_owed(&mut st.follow_owed, re));
        let (re, deadline) = match owed {
            Some(owed) => (owed.re, owed.deadline),
            None => (None, now + EDITOR_DEADLINE),
        };
        st.follow_call = Some(Pending::new(re, document, deadline, FollowCall { op, answer, session }));
        None
    }

    /// The follow state's part of `pump`: the session check, the answer, the next call. Returns when to run
    /// again. Nothing reaches nvim before the window has given the follow socket.
    fn pump_follow(&self, now: Instant, replies: &mut Vec<Reply>) -> Option<Instant> {
        self.expire_follow_owed(now, replies);
        let fed = self.state.borrow().follow_socket.is_some();
        if !fed {
            return None;
        }
        self.follow_session(now);
        self.take_follow_answer(now, replies);
        let at = self.send_follow(now);
        let st = self.state.borrow();
        match &st.follow_call {
            Some(call) => Some(
                now + if now >= call.deadline {
                    FOLLOW_LATE_POLL
                } else {
                    EDITOR_POLL
                },
            ),
            None => at,
        }
    }

    fn new_document(&self) {
        let (follows, dropped, preloaded) = {
            let mut st = self.state.borrow_mut();
            st.hello = false;
            st.document += 1;
            // A new document has its own keys, and declares them once it has loaded.
            st.declared = None;
            // It has not asked to follow, and holds no text.
            st.page_follows_document = None;
            st.follow.page_follows(false);
            st.follow.page_forgot();
            // What the old one asked of the editor and is still waiting in line is not sent: its answer would
            // carry an id the new document may be using for something else.
            st.follow.drop_page_ops();
            // The new document was not asked for its state and has not drawn one.
            st.ready = false;
            if let Some(ask) = st.save.as_mut() {
                ask.unanswerable |= ask.answer.is_none();
            }
            (
                st.follow_socket.is_some(),
                std::mem::take(&mut st.follow_owed),
                std::mem::replace(&mut st.preloaded, false),
            )
        };
        if preloaded {
            self.scoped_call(None, ScopedOp::WipePreload, Instant::now());
            (self.wake)();
        }
        self.agents_new_document();
        let audience = self.audience();
        let mut replies = Vec::new();
        for mut owed in dropped {
            answer_or_drop(self.id(), audience, &mut owed, |_| None, &mut replies);
        }
        self.send_replies(replies);
        if follows {
            (self.wake)();
        }
    }

    /// Takes task ownership to a worker, since pausing readers waits for their acknowledgement.
    pub fn take_move_runner(&self) -> Result<TaskRunner, String> {
        let mut st = self.state.borrow_mut();
        if st.moving {
            return Err("the page already has a move in flight".into());
        }
        st.moving = true;
        Ok(std::mem::replace(&mut st.tasks, TaskRunner::new(self.wake.clone())))
    }

    /// Puts the worker's runner back; a frozen source keeps refusing page calls until it resumes or leaves.
    pub fn return_move_runner(&self, runner: TaskRunner, frozen: bool) {
        let mut st = self.state.borrow_mut();
        st.tasks = runner;
        st.moving = frozen;
        if !frozen {
            st.closed = false;
        }
        drop(st);
        (self.wake)();
    }

    /// Reopens the registered host for a document arriving from the other window.
    pub fn accept_moved(&self) {
        self.new_document();
        let mut st = self.state.borrow_mut();
        st.closed = false;
        st.moved_away = false;
        st.follow_stopped = false;
        st.moving = true;
    }

    /// A resumed page can send requests again, including a page with no running tasks.
    pub fn resume_moved(&self) {
        self.state.borrow_mut().moving = false;
        (self.wake)();
    }

    /// Forgets a transferred document without stopping its successor's tasks or writing page state to disk.
    pub fn forget_moved(&self) {
        self.new_document();
        let mut st = self.state.borrow_mut();
        st.closed = true;
        st.moving = false;
        st.moved_away = true;
        let wipe_preload = st.parked.iter().any(|p| matches!(p.on, ScopedOp::WipePreload));
        st.calls.clear();
        st.parked.clear();
        st.checking.clear();
        st.follow_call = None;
        st.follow = FollowState::new();
        // The follow state starts off screen; so does the host's record of it, until the window tells it again.
        st.on_screen = false;
        st.view = None;
        st.last_text = None;
        drop(st);
        if wipe_preload {
            // The pending checks are gone; calls already sent reach the editor ahead of this wipe.
            let _ = self.send_scoped(None, ScopedOp::WipePreload, Instant::now() + EDITOR_DEADLINE);
        }
    }

    pub fn runs(&self) -> Vec<RunInfo> {
        self.state.borrow().tasks.runs()
    }

    fn id(&self) -> &str {
        &self.plugin.manifest.id
    }

    fn reply(&self, reply: Reply) {
        self.page.send(&reply.to_text());
    }

    pub fn page_message(&self, raw: &str, now: Instant) {
        if self.state.borrow().closed {
            return;
        }
        let request = match protocol::parse_request(raw) {
            Inbound::Request(request) => request,
            Inbound::Refused {
                re: Some(re),
                code,
                message,
            } => return self.reply(Reply::err(re, code, &message)),
            Inbound::Refused { re: None, message, .. } => {
                eprintln!(
                    "[plugin {}] dropped a page message with no id to answer: {message}",
                    self.id()
                );
                return;
            }
        };
        let re = request.id;
        if self.state.borrow().moving && !matches!(&request.verb, Verb::PageSaved(_)) {
            return self.reply(Reply::err(re, ErrorCode::Busy, "the page is moving to another window"));
        }
        if let Some(capability) = request.verb.capability() {
            if !self.plugin.manifest.has(capability) {
                let message = format!(
                    "{} needs the {} capability, which the plugin's manifest does not declare",
                    request.verb.name(),
                    capability.as_str()
                );
                return self.reply(Reply::err(re, ErrorCode::NotDeclared, &message));
            }
        }
        if let Some(reply) = self.handle(re, request.verb, now) {
            self.reply(reply);
        }
    }

    fn handle(&self, re: u64, verb: Verb, now: Instant) -> Option<Reply> {
        match verb {
            Verb::HostHello(_) => {
                // The page learns where the keys are only after the reply, so it has its configuration
                // before the first event.
                self.reply(self.hello(re));
                self.send_focus();
                // A run may have ended while the document was loading, with no later output to wake us.
                (self.wake)();
                None
            }
            Verb::KeysDeclare(args) => Some(self.keys_declare(re, args.rows)),
            Verb::TaskStart(args) => self.check_then(
                re,
                CheckJob::Task {
                    task: args.task.clone(),
                    vars: args.vars.clone(),
                },
                Then::Task {
                    task: args.task,
                    vars: args.vars,
                },
                now,
            ),
            Verb::TaskCancel(args) => {
                let known = self.state.borrow_mut().tasks.cancel(args.run);
                Some(if known {
                    Reply::ok(re, json!({}))
                } else {
                    Reply::err(re, ErrorCode::NoSuchTask, &format!("no run {}", args.run))
                })
            }
            Verb::EditorOpen(args) => self.editor_open(re, &args.path, args.line, args.col, now),
            Verb::EditorDiff(args) => self.editor_diff(re, args, now),
            Verb::EditorCd(args) => self.editor_cd(re, &args.dir, now),
            Verb::EditorPreload(args) => self.directory_call(
                re,
                &args.path,
                DirOp::Preload {
                    diagnostics: args.diagnostics == OnOff::On,
                    inlay_hints: args.inlay_hints == OnOff::On,
                },
                now,
            ),
            Verb::EditorModified(args) => self.directory_call(re, &args.root, DirOp::Modified, now),
            Verb::EditorReload(args) => self.directory_call(re, &args.root, DirOp::Reload, now),
            Verb::EditorAssist(args) => self.editor_assist(re, &args.root, args.diagnostics, args.inlay_hints, now),
            Verb::EditorAssistClear(_) => self.editor_clear(Some(re), now),
            Verb::LayoutFocus(args) => Some(self.focus(re, args.target)),
            Verb::StateRead(args) => self.store_op(re, StoreOp::Read { key: args.key }, now),
            Verb::StateWrite(args) => self.store_op(
                re,
                StoreOp::Write {
                    key: args.key,
                    value: args.value,
                },
                now,
            ),
            Verb::LogAppend(args) => self.store_op(
                re,
                StoreOp::Append {
                    stream: args.stream,
                    items: args.items,
                },
                now,
            ),
            Verb::LogRead(args) => self.store_op(
                re,
                StoreOp::ReadLog {
                    stream: args.stream,
                    from: args.from,
                },
                now,
            ),
            Verb::EditorFollow(_) => self.editor_follow(re),
            Verb::EditorReveal(args) => self.editor_reveal(re, args, now),
            Verb::EditorJump(args) => self.editor_jump(re, args, now),
            Verb::EditorOpenLink(args) => self.editor_open_link(re, args, now),
            Verb::PageSaved(args) => Some(self.page_saved(re, args, now)),
            Verb::PageReady(_) => Some(self.page_ready(re)),
            Verb::TerminalAt(args) => self.terminal_at(re, &args.dir, now),
            verb @ (Verb::AgentsWatch(_)
            | Verb::AgentsFocus(_)
            | Verb::AgentsNew(_)
            | Verb::AgentsSelect(_)
            | Verb::AgentsRound(_)
            | Verb::AgentsStop(_)) => self.agents_verb(re, verb, now),
        }
    }

    /// `terminal.at {dir}`: in the plugin's own window only, `dir` a directory inside a root, checked on the path
    /// worker; then handed to the window.
    fn terminal_at(&self, re: u64, raw: &str, now: Instant) -> Option<Reply> {
        if !self.own_window() {
            return Some(Reply::err(re, ErrorCode::NotOwnWindow, NOT_OWN_WINDOW_TEXT));
        }
        if let Err(refusal) = roots::lexical(raw) {
            return Some(path_refusal(re, refusal, raw));
        }
        let then = Then::TerminalAt { raw: raw.to_owned() };
        self.check_then(re, CheckJob::Dir(raw.to_owned()), then, now)
    }

    /// The directory passed its check: the window is asked under a fresh ticket, owed its answer for
    /// [`TERMINAL_AT_DEADLINE`]. A refusal now is the page's `busy`, with the window's words.
    fn terminal_at_checked(&self, re: u64, dir: PathBuf, now: Instant, replies: &mut Vec<Reply>) {
        let layout = self.state.borrow().layout.clone();
        let Some(layout) = layout else {
            replies.push(Reply::err(re, ErrorCode::Busy, "this window has no terminal to move"));
            return;
        };
        let ticket = {
            let mut st = self.state.borrow_mut();
            let ticket = st.next_ticket;
            st.next_ticket += 1;
            let document = st.document;
            st.terminal_at
                .push(Pending::new(Some(re), document, now + TERMINAL_AT_DEADLINE, ticket));
            ticket
        };
        // No borrow is held here: the window may answer before the call returns.
        let asked = layout.terminal_at(dir, ticket);
        if let Err(why) = asked {
            let pending = {
                let mut st = self.state.borrow_mut();
                let at = st.terminal_at.iter().position(|p| p.on == ticket);
                at.map(|i| st.terminal_at.remove(i))
            };
            if let Some(mut pending) = pending {
                let audience = self.audience();
                let refused = |re| Some(Reply::err(re, ErrorCode::Busy, &why));
                answer_or_drop(self.id(), audience, &mut pending, refused, replies);
            }
        }
    }

    /// The window's answer to the `terminal.at` asked under `ticket`. The page that asked hears it once, unless its
    /// document was replaced (dropped with one log line) or it was already told the terminal did not answer in
    /// time (dropped).
    pub fn terminal_at_answered(&self, ticket: u64, answer: TerminalAtAnswer, now: Instant) {
        let pending = {
            let mut st = self.state.borrow_mut();
            let at = st.terminal_at.iter().position(|p| p.on == ticket);
            at.map(|i| st.terminal_at.remove(i))
        };
        let Some(mut pending) = pending else {
            eprintln!(
                "[plugin {}] terminal.at {ticket}: answered after the page was told it took too long; dropped",
                self.id()
            );
            return;
        };
        let audience = self.audience();
        let mut replies = Vec::new();
        if now >= pending.deadline {
            let late = |re| Some(Reply::err(re, ErrorCode::Busy, TERMINAL_LATE));
            answer_or_drop(self.id(), audience, &mut pending, late, &mut replies);
        } else {
            let answered = |re| {
                Some(match answer {
                    TerminalAtAnswer::StartDir => Reply::ok(re, json!({"how": "start_dir"})),
                    TerminalAtAnswer::Restarted => Reply::ok(re, json!({"how": "restarted"})),
                    TerminalAtAnswer::Already => Reply::ok(re, json!({"how": "already"})),
                    TerminalAtAnswer::Busy(why) => Reply::err(re, ErrorCode::Busy, &why),
                })
            };
            answer_or_drop(self.id(), audience, &mut pending, answered, &mut replies);
        }
        self.send_replies(replies);
    }

    /// Tells the page whose `terminal.at` is past its deadline that the terminal did not answer; the window's
    /// answer, when it comes, is then dropped.
    fn expire_terminal_at(&self, now: Instant, replies: &mut Vec<Reply>) {
        let audience = self.audience();
        let mut st = self.state.borrow_mut();
        st.terminal_at.retain_mut(|pending| {
            if now < pending.deadline {
                return true;
            }
            let late = |re| Some(Reply::err(re, ErrorCode::Busy, TERMINAL_LATE));
            answer_or_drop(self.id(), audience, pending, late, replies);
            false
        });
    }

    fn not_handoff(re: u64, verb: &str) -> Reply {
        Reply::err(
            re,
            ErrorCode::NotDeclared,
            &format!("{verb} needs [page] handoff = true in the plugin's manifest"),
        )
    }

    /// Asks the page for its state, to hand to its successor in another window; the answer, or why there is
    /// none, is [`Self::take_saved`]'s. A plugin without `[page] handoff` is not asked.
    pub fn request_save(&self, deadline: Instant) {
        if !self.plugin.manifest.handoff {
            return;
        }
        let listening = {
            let mut st = self.state.borrow_mut();
            let listening = st.hello && !st.closed;
            st.save = Some(SaveAsk {
                deadline,
                answer: None,
                unanswerable: !listening,
            });
            listening
        };
        if listening {
            self.page
                .send(&protocol::event_text(protocol::EVENT_HOST_SAVE, &NoArgs {}));
        }
    }

    /// The outcome of the last [`Self::request_save`]. Anything but `Waiting` ends the question: a later answer
    /// is refused.
    pub fn take_saved(&self, now: Instant) -> SaveOutcome {
        if !self.plugin.manifest.handoff {
            return SaveOutcome::NotHandoff;
        }
        let mut st = self.state.borrow_mut();
        match st.save.take() {
            None => SaveOutcome::NotAsked,
            Some(SaveAsk {
                answer: Some(state), ..
            }) => SaveOutcome::Saved(state),
            Some(ask) if ask.unanswerable || now >= ask.deadline => SaveOutcome::TimedOut,
            Some(ask) => {
                st.save = Some(ask);
                SaveOutcome::Waiting
            }
        }
    }

    /// The state the page's predecessor in another window handed over; the next `host.hello` reply carries it,
    /// and no later one. Ignored for a plugin without `[page] handoff`, whose page never applies one.
    pub fn set_restored(&self, state: Value) {
        if self.plugin.manifest.handoff {
            self.state.borrow_mut().restored = Some(state);
        }
    }

    /// Whether the page has said `page.ready` (it applied its restored state and drew it) since this was last
    /// taken.
    pub fn take_ready(&self) -> bool {
        std::mem::take(&mut self.state.borrow_mut().ready)
    }

    /// The running document, once it has said hello: `None` before then and from each new document on until it
    /// does. A `page.ready` taken from a document is about that document alone, so a window that posts it later
    /// checks first that this still names the same one.
    pub fn hello_document(&self) -> Option<u64> {
        let st = self.state.borrow();
        st.hello.then_some(st.document)
    }

    fn page_saved(&self, re: u64, args: PageSaved, now: Instant) -> Reply {
        if !self.plugin.manifest.handoff {
            return Self::not_handoff(re, "page.saved");
        }
        if let Err(why) = args.check() {
            return Reply::err(re, ErrorCode::TooLarge, &why);
        }
        {
            let mut st = self.state.borrow_mut();
            match st.save.as_mut() {
                Some(ask) if ask.answer.is_none() && !ask.unanswerable && now < ask.deadline => {
                    ask.answer = Some(args.state);
                }
                _ => {
                    return Reply::err(
                        re,
                        ErrorCode::BadRequest,
                        "page.saved answers host.save, and no host.save is waiting for an answer",
                    );
                }
            }
        }
        // Whoever waits for the state learns of it on the next pump rather than at its deadline.
        (self.wake)();
        Reply::ok(re, json!({}))
    }

    fn page_ready(&self, re: u64) -> Reply {
        if !self.plugin.manifest.handoff {
            trace::line(self.id(), "page.ready refused", "no [page] handoff");
            return Self::not_handoff(re, "page.ready");
        }
        let committed = {
            let mut st = self.state.borrow_mut();
            if !st.hello {
                drop(st);
                trace::line(self.id(), "page.ready refused", "before host.hello");
                return Reply::err(re, ErrorCode::BadRequest, "page.ready before host.hello");
            }
            st.ready = true;
            st.committed
        };
        trace::line(self.id(), "page.ready", &format!("document={committed}"));
        (self.wake)();
        Reply::ok(re, json!({}))
    }

    /// What every follow verb needs first: the page said hello, its arguments pass their check, and there is an
    /// editor to ask.
    fn follow_verb_refusal(&self, re: u64, verb: &str, check: Result<(), String>) -> Option<Reply> {
        let (hello, stopped) = {
            let st = self.state.borrow();
            (st.hello, st.follow_stopped)
        };
        if !hello {
            return Some(Reply::err(
                re,
                ErrorCode::BadRequest,
                &format!("{verb} comes after host.hello"),
            ));
        }
        if let Err(why) = check {
            return Some(Reply::err(re, ErrorCode::BadRequest, &why));
        }
        if stopped {
            return Some(self.stopped_follow_reply(re));
        }
        if self.editor().is_none() {
            return Some(Reply::err(
                re,
                ErrorCode::EditorUnavailable,
                "this window has no editor running",
            ));
        }
        None
    }

    /// Whether `buf` is the newest view's buffer: a page acts only on the buffer it is shown.
    fn is_followed(&self, buf: u64) -> bool {
        self.state.borrow().view.as_ref().is_some_and(|v| v.buf == buf)
    }

    fn editor_reveal(&self, re: u64, args: EditorReveal, now: Instant) -> Option<Reply> {
        if let Some(refused) = self.follow_verb_refusal(re, "editor.reveal", args.check()) {
            return Some(refused);
        }
        if !self.is_followed(args.buf) {
            return Some(Reply::ok(re, json!({ "applied": false })));
        }
        let op = FollowOp::Reveal {
            buf: args.buf,
            line: args.line,
            token: args.token,
            cursor: args.cursor,
            row: args.row,
            re,
        };
        let mut replies = Vec::new();
        if let Some(mut old) = self.park_page_op(op, re, now + EDITOR_DEADLINE, &mut replies) {
            let audience = self.audience();
            answer_or_drop(
                self.id(),
                audience,
                &mut old,
                |re| Some(Reply::ok(re, json!({ "applied": false }))),
                &mut replies,
            );
        }
        self.send_replies(replies);
        self.wake_follow();
        None
    }

    fn editor_jump(&self, re: u64, args: EditorJump, now: Instant) -> Option<Reply> {
        if let Some(refused) = self.follow_verb_refusal(re, "editor.jump", args.check()) {
            return Some(refused);
        }
        if !self.is_followed(args.buf) {
            return Some(Reply::ok(re, json!({ "applied": false })));
        }
        let op = FollowOp::Jump {
            buf: args.buf,
            line: args.line,
            col: args.col,
            focus: args.focus,
            select: args.select,
            re,
        };
        let mut replies = Vec::new();
        if let Some(mut old) = self.park_page_op(op, re, now + EDITOR_DEADLINE, &mut replies) {
            let audience = self.audience();
            answer_or_drop(
                self.id(),
                audience,
                &mut old,
                |re| Some(Reply::ok(re, json!({ "applied": false }))),
                &mut replies,
            );
        }
        self.send_replies(replies);
        self.wake_follow();
        None
    }

    /// A link of the followed document: resolved against the document (a leading `/` against its project) and
    /// checked on the path worker to be a regular file of its project before nvim is asked to open it.
    fn editor_open_link(&self, re: u64, args: EditorOpenLink, now: Instant) -> Option<Reply> {
        if let Some(refused) = self.follow_verb_refusal(re, "editor.open_link", args.check()) {
            return Some(refused);
        }
        let name = {
            let st = self.state.borrow();
            st.view.as_ref().filter(|v| v.buf == args.buf).map(|v| v.name.clone())
        };
        let Some(name) = name else {
            return Some(Reply::err(re, ErrorCode::BadRequest, "buf is not the followed buffer"));
        };
        let Some(doc) = doc_path(&name) else {
            return Some(Reply::err(re, ErrorCode::OutsideRoots, "the document has no file"));
        };
        let (path, fragment) = match args.target.split_once('#') {
            Some((path, fragment)) => (path, Some(fragment)),
            None => (args.target.as_str(), None),
        };
        let line = fragment.and_then(line_fragment);
        let Some(decoded) = page_policy::decoded(path) else {
            return Some(Reply::err(
                re,
                ErrorCode::BadRequest,
                &format!("\"{}\" is not a well-formed path", path.escape_debug()),
            ));
        };
        let (rel, from_root) = match decoded.strip_prefix('/') {
            // Every leading `/`: joined to the project, an absolute rest would replace it.
            Some(rest) => (rest.trim_start_matches('/'), true),
            None => (decoded.as_str(), false),
        };
        if rel.is_empty() {
            return Some(Reply::err(re, ErrorCode::BadRequest, "nothing to open"));
        }
        let job = {
            let st = self.state.borrow();
            match st.projects.get(&name).cloned().flatten() {
                Some((project, doc_dir)) => CheckJob::Link {
                    project,
                    doc_dir,
                    path: rel.to_owned(),
                    from_root,
                },
                None => CheckJob::LinkWithProjectLookup {
                    doc,
                    window_root: st.window_root.clone(),
                    home: st.home.clone(),
                    path: rel.to_owned(),
                    from_root,
                },
            }
        };
        let then = Then::Link {
            target: args.target.clone(),
            line,
            session: self.state.borrow().follow.current_session(),
        };
        self.check_then(re, job, then, now)
    }

    fn hello(&self, re: u64) -> Reply {
        // One step: the tail handed over and the lines it already holds taken out of the queue, so a line
        // is delivered once (in the tail) and not again as output.
        let runs = self.state.borrow_mut().tasks.attach();
        self.state.borrow_mut().hello = true;
        let (timeoutlen, prefix, module_key, theme, keys_settings, restored) = {
            let mut st = self.state.borrow_mut();
            (
                st.timeoutlen,
                st.prefix_label.clone(),
                st.module_key.clone(),
                st.theme.to_json(),
                st.keys_settings.clone(),
                st.restored.take(),
            )
        };
        let committed = self.state.borrow().committed;
        trace::line(
            self.id(),
            "hello",
            &format!(
                "restored={} document={committed}",
                if restored.is_some() { "yes" } else { "no" }
            ),
        );
        let manifest = &self.plugin.manifest;
        let capabilities: Vec<&str> = manifest.capabilities.iter().map(|c| c.as_str()).collect();
        let roots: BTreeMap<&String, String> = self
            .plugin
            .roots
            .iter()
            .map(|(name, path)| (name, path.to_string_lossy().into_owned()))
            .collect();
        let runs: Vec<Value> = runs
            .into_iter()
            .map(|r| json!({ "run": r.run, "task": r.task, "vars": r.vars, "tail": r.tail }))
            .collect();
        Reply::ok(
            re,
            json!({
                "host": "eitri",
                "own_window": self.own_window(),
                "version": env!("CARGO_PKG_VERSION"),
                "protocol": protocol::VERSION,
                "capabilities": capabilities,
                "roots": roots,
                "runs": runs,
                "settings": self.plugin.page_settings(),
                "keys_kit": 1,
                "keys_settings": keys_settings,
                "timeoutlen": timeoutlen,
                "prefix": prefix,
                "module_key": module_key,
                "theme": theme,
                "restored": restored,
            }),
        )
    }

    fn keys_declare(&self, re: u64, rows: Vec<KeyRowArg>) -> Reply {
        // Hello opens a document's session with the host; a table declared before it would belong to a
        // document that has not said it is ready, and might never.
        let (hello, prefix_key) = {
            let st = self.state.borrow();
            (st.hello, st.prefix_key.clone())
        };
        if !hello {
            return Reply::err(re, ErrorCode::BadRequest, "keys.declare comes after host.hello");
        }
        let rows = match keys::validate(rows, &prefix_key) {
            Ok(rows) => rows,
            Err(why) => return Reply::err(re, ErrorCode::BadRequest, &why),
        };
        let (effective, log) = keys::effective(&rows, &self.overrides);
        for line in log {
            eprintln!("[plugin {}] {line}", self.id());
        }
        let result: Vec<Value> = effective
            .iter()
            .map(|row| {
                let keys: Vec<&str> = row.keys.iter().map(|(k, _)| k.as_str()).collect();
                let sources: Vec<&str> = row.keys.iter().map(|(_, s)| s.as_str()).collect();
                json!({
                    "action": row.action,
                    "keys": keys,
                    "sources": sources,
                    "context": row.context,
                    "desc": row.desc,
                })
            })
            .collect();
        self.state.borrow_mut().declared = Some(effective);
        Reply::ok(re, json!({ "rows": result }))
    }

    fn task_start(
        &self,
        re: u64,
        task: &str,
        vars: &BTreeMap<String, String>,
        resolved: Result<ResolvedTask, TaskRefusal>,
    ) -> Reply {
        if self.state.borrow().moving {
            return Reply::err(re, ErrorCode::Busy, "the page is moving to another window");
        }
        let started = {
            let mut st = self.state.borrow_mut();
            match resolved {
                Ok(t) => st.tasks.start(Launch {
                    task: task.to_owned(),
                    vars: vars.clone(),
                    program: t.program,
                    args: t.args,
                    cwd: t.cwd,
                    timeout: t.timeout,
                }),
                Err(TaskRefusal::NotBuilt(why)) => {
                    eprintln!("[plugin {}] task {task} cannot start: {why}", self.id());
                    st.tasks.start_failed(task, vars, why)
                }
                Err(TaskRefusal::NoSuchTask) => {
                    return Reply::err(
                        re,
                        ErrorCode::NoSuchTask,
                        &format!("the manifest declares no task {task:?}"),
                    )
                }
                Err(TaskRefusal::BadVar(why)) => return Reply::err(re, ErrorCode::BadVar, &why),
                Err(TaskRefusal::OutsideRoots(why)) => return Reply::err(re, ErrorCode::OutsideRoots, &why),
            }
        };
        match started {
            Ok(run) => Reply::ok(re, json!({ "run": run })),
            Err(StartError::TaskBusy(t)) => Reply::err(re, ErrorCode::Busy, &format!("task {t} is already running")),
            Err(StartError::TooManyRuns) => Reply::err(re, ErrorCode::Busy, "this plugin already has 3 runs"),
        }
    }

    /// The editor, if the window has one running now.
    fn editor(&self) -> Option<Rc<dyn EditorRpc>> {
        let editor = self.state.borrow().editor.clone();
        editor.filter(|e| e.target().is_some())
    }

    fn push_call(&self, re: Option<u64>, call: Call, answer: nvim_rpc::Pending, deadline: Instant) {
        let mut st = self.state.borrow_mut();
        let document = st.document;
        st.calls
            .push(Pending::new(re, document, deadline, EditorCall { call, answer }));
    }

    fn editor_open(&self, re: u64, raw: &str, line: Option<u32>, col: Option<u32>, now: Instant) -> Option<Reply> {
        if let Err(refusal) = roots::lexical(raw) {
            return Some(path_refusal(re, refusal, raw));
        }
        if self.editor().is_none() {
            return Some(Reply::err(
                re,
                ErrorCode::EditorUnavailable,
                "this window has no editor running",
            ));
        }
        let then = Then::Open {
            raw: raw.to_owned(),
            line,
            col,
        };
        self.check_then(re, CheckJob::File(raw.to_owned()), then, now)
    }

    /// The file passed its check: the editor is asked, within what is left of the request's deadline.
    fn open_checked(&self, re: u64, path: &std::path::Path, line: Option<u32>, col: Option<u32>, deadline: Instant) {
        let Some(editor) = self.editor() else {
            self.reply(Reply::err(
                re,
                ErrorCode::EditorUnavailable,
                "this window has no editor running",
            ));
            return;
        };
        let answer = editor::open_owned(&*editor, self.id(), path, line, col);
        self.push_call(Some(re), Call::Open, answer, deadline);
    }

    fn editor_diff(&self, re: u64, args: protocol::EditorDiff, now: Instant) -> Option<Reply> {
        if let Err((code, why)) = args.check() {
            return Some(Reply::err(re, code, &why));
        }
        for side in [&args.left, &args.right] {
            if let protocol::DiffSide::Path { path } = side {
                let raw = path.to_string_lossy();
                if let Err(refusal) = roots::lexical(&raw) {
                    return Some(path_refusal(re, refusal, &raw));
                }
            }
        }
        if self.editor().is_none() {
            return Some(Reply::err(
                re,
                ErrorCode::EditorUnavailable,
                "this window has no editor running",
            ));
        }
        let ticket = match self.submit_check(CheckJob::Diff(args)) {
            Ok(ticket) => ticket,
            Err(why) => return Some(Reply::err(re, ErrorCode::Io, &why)),
        };
        let mut st = self.state.borrow_mut();
        st.diffed = true;
        let document = st.document;
        st.parked.push_back(Pending::new(
            Some(re),
            document,
            now + EDITOR_DEADLINE,
            ScopedOp::CheckingDiff { ticket },
        ));
        None
    }

    fn editor_cd(&self, re: u64, raw: &str, now: Instant) -> Option<Reply> {
        if !self.own_window() {
            return Some(Reply::err(re, ErrorCode::NotOwnWindow, NOT_OWN_WINDOW_TEXT));
        }
        self.directory_call(re, raw, DirOp::Cwd, now)
    }

    fn editor_assist(&self, re: u64, raw: &str, diagnostics: OnOff, inlay_hints: OnOff, now: Instant) -> Option<Reply> {
        self.directory_call(
            re,
            raw,
            DirOp::Assist {
                diagnostics: diagnostics == OnOff::On,
                inlay_hints: inlay_hints == OnOff::On,
            },
            now,
        )
    }

    fn directory_call(&self, re: u64, raw: &str, op: DirOp, now: Instant) -> Option<Reply> {
        if let Err(refusal) = roots::lexical(raw) {
            return Some(path_refusal(re, refusal, raw));
        }
        if self.editor().is_none() {
            return Some(Reply::err(
                re,
                ErrorCode::EditorUnavailable,
                "this window has no editor running",
            ));
        }
        let job = if matches!(op, DirOp::Preload { .. }) {
            CheckJob::File(raw.to_owned())
        } else {
            CheckJob::Dir(raw.to_owned())
        };
        let ticket = match self.submit_check(job) {
            Ok(ticket) => ticket,
            Err(why) => return Some(Reply::err(re, ErrorCode::Io, &why)),
        };
        // Marked before the check finishes: a stop while it is pending still clears the change.
        let mut st = self.state.borrow_mut();
        match op {
            DirOp::Assist { .. } => st.assisted = true,
            DirOp::Cwd => st.cwd_set = true,
            DirOp::Preload { .. } => st.preloaded = true,
            DirOp::Modified | DirOp::Reload => {}
        }
        let document = st.document;
        let op = ScopedOp::Checking {
            ticket,
            raw: raw.to_owned(),
            op,
        };
        st.parked
            .push_back(Pending::new(Some(re), document, now + EDITOR_DEADLINE, op));
        None
    }

    fn editor_clear(&self, re: Option<u64>, now: Instant) -> Option<Reply> {
        self.state.borrow_mut().assisted = false;
        if self.editor().is_none() {
            // With no editor running, nothing the plugin set is in force any more.
            return re.map(|re| Reply::err(re, ErrorCode::EditorUnavailable, "this window has no editor running"));
        }
        self.scoped_call(re, ScopedOp::Clear, now);
        None
    }

    /// Sends a checked editor operation or clear unless a path ahead of it is still being checked.
    fn scoped_call(&self, re: Option<u64>, op: ScopedOp, now: Instant) {
        let busy = !self.state.borrow().parked.is_empty();
        if busy {
            let mut st = self.state.borrow_mut();
            let document = st.document;
            st.parked
                .push_back(Pending::new(re, document, now + EDITOR_DEADLINE, op));
            return;
        }
        let _ = self.send_scoped(re, op, now + EDITOR_DEADLINE);
    }

    /// The editor is asked now. Its absence is answered to the page, as nothing is then in force.
    fn send_scoped(&self, re: Option<u64>, op: ScopedOp, deadline: Instant) -> Option<Reply> {
        let Some(editor) = self.editor() else {
            return re.map(|re| Reply::err(re, ErrorCode::EditorUnavailable, "this window has no editor running"));
        };
        let (call, answer) = match &op {
            ScopedOp::Checking { .. } | ScopedOp::CheckingDiff { .. } => return None,
            ScopedOp::Diff(args) => (Call::Diff, editor::diff(&*editor, self.id(), args)),
            ScopedOp::CloseDiffs => (Call::CloseDiffs, editor::close_diffs(&*editor, self.id())),
            ScopedOp::Apply {
                root,
                op: DirOp::Preload {
                    diagnostics,
                    inlay_hints,
                },
            } => (
                Call::Preload,
                editor::preload(&*editor, self.id(), root, *diagnostics, *inlay_hints),
            ),
            ScopedOp::Apply {
                root,
                op: DirOp::Modified,
            } => (Call::Modified, editor::modified(&*editor, self.id(), root)),
            ScopedOp::Apply {
                root,
                op: DirOp::Reload,
            } => (Call::Reload, editor::reload(&*editor, self.id(), root)),
            ScopedOp::WipePreload => (Call::WipePreload, editor::wipe_preload(&*editor, self.id())),
            ScopedOp::Apply {
                root,
                op: DirOp::Assist {
                    diagnostics,
                    inlay_hints,
                },
            } => (
                Call::Assist,
                editor::assist_apply(&*editor, self.id(), root, *diagnostics, *inlay_hints),
            ),
            ScopedOp::Apply { root, op: DirOp::Cwd } => (Call::Cwd, editor::cwd(&*editor, self.id(), root)),
            ScopedOp::CwdClear => (Call::CwdClear, editor::cwd_clear(&*editor, self.id())),
            ScopedOp::Clear => (Call::Clear, editor::assist_clear(&*editor, self.id())),
        };
        self.push_call(re, call, answer, deadline);
        None
    }

    fn focus(&self, re: u64, target: FocusTarget) -> Reply {
        let layout = self.state.borrow().layout.clone();
        let Some(layout) = layout else {
            return Reply::err(re, ErrorCode::Io, "this window cannot move the keys");
        };
        let moved = layout.focus(target);
        match moved {
            Ok(()) => Reply::ok(re, json!({})),
            Err(why) => {
                let code = if target == FocusTarget::Editor {
                    ErrorCode::EditorUnavailable
                } else {
                    ErrorCode::Io
                };
                Reply::err(re, code, &why)
            }
        }
    }

    fn store_op(&self, re: u64, op: StoreOp, now: Instant) -> Option<Reply> {
        let Some(store) = self.store.clone() else {
            return Some(Reply::err(
                re,
                ErrorCode::Io,
                "no state directory: neither XDG_STATE_HOME nor HOME gives one",
            ));
        };
        let mut st = self.state.borrow_mut();
        if st.store.is_none() {
            let gate = st.store_gate.take();
            let spawned = StoreWorker::spawn(store, self.wake.clone(), gate);
            match spawned {
                Ok(worker) => st.store = Some(worker),
                Err(e) => {
                    return Some(Reply::err(
                        re,
                        ErrorCode::Io,
                        &format!("the store's thread could not start: {e}"),
                    ))
                }
            }
        }
        if st
            .store
            .as_ref()
            .is_some_and(|worker| worker.in_flight() >= MAX_STORE_IN_FLIGHT)
        {
            return Some(Reply::err(
                re,
                ErrorCode::Busy,
                "the plugin's store has too many operations in flight",
            ));
        }
        let ticket = st.next_ticket;
        st.next_ticket += 1;
        let document = st.document;
        let deadline = now + st.store_deadline;
        st.store_waiting
            .insert(ticket, Pending::new(Some(re), document, deadline, ()));
        if let Some(worker) = st.store.as_ref() {
            worker.submit(ticket, op, deadline);
        }
        None
    }

    /// The page's reply to an editor call nvim answered.
    fn answered(call: Call, re: u64, value: &rmpv::Value) -> Reply {
        match call {
            Call::Open | Call::WipePreload | Call::CloseDiffs => Reply::ok(re, json!({})),
            Call::Diff => match value
                .as_map()
                .and_then(|fields| fields.iter().find(|(k, _)| k.as_str() == Some("tab")))
                .and_then(|(_, v)| v.as_u64())
                .filter(|tab| *tab > 0)
            {
                Some(tab) => Reply::ok(re, json!({"tab": tab})),
                None => Reply::err(re, ErrorCode::Io, "the editor diff answer has no tab"),
            },
            Call::Preload => match editor::decode_preload(value) {
                Ok(p) => Reply::ok(
                    re,
                    json!({"buf": p.buf, "replaced": p.replaced, "stopped_clients": p.stopped_clients}),
                ),
                Err(why) => Reply::err(re, ErrorCode::Io, &why),
            },
            Call::Modified => match editor::decode_modified(value) {
                Ok(paths) => Reply::ok(re, json!({"paths": paths})),
                Err(why) => Reply::err(re, ErrorCode::Io, &why),
            },
            Call::Reload => match editor::decode_reload(value) {
                Ok((reloaded, kept_modified)) => {
                    Reply::ok(re, json!({"reloaded": reloaded, "kept_modified": kept_modified}))
                }
                Err(why) => Reply::err(re, ErrorCode::Io, &why),
            },
            Call::Cwd => match editor::decode_cwd(value) {
                Ok(c) => Reply::ok(re, json!({ "dir": c.dir, "tab": c.tab })),
                Err(why) => Reply::err(re, ErrorCode::Io, &why),
            },
            Call::CwdClear => match editor::decode_cwd_clear(value) {
                Ok(c) => Reply::ok(re, json!({ "restored": c.restored, "moved": c.moved })),
                Err(why) => Reply::err(re, ErrorCode::Io, &why),
            },
            Call::Assist => match decode_apply(value) {
                Ok(a) => Reply::ok(
                    re,
                    json!({
                        "applied": { "diagnostics": on_off(a.diagnostics), "inlay_hints": on_off(a.inlay_hints) },
                        "buffers": a.buffers,
                        "partial": a.partial,
                    }),
                ),
                Err(why) => Reply::err(re, ErrorCode::Io, &why),
            },
            Call::Clear => match decode_clear(value) {
                Ok(c) => Reply::ok(re, json!({ "restored": c.restored, "left_alone": c.left_alone })),
                Err(why) => Reply::err(re, ErrorCode::Io, &why),
            },
        }
    }

    /// Takes what is ready -- task output, editor answers, store answers -- and sends it; returns when to
    /// be run again if something is held for a time (a partial batch, an editor answer still out), else
    /// `None`: then only the wake runs it.
    pub fn pump(&self, now: Instant) -> Option<Instant> {
        if self.state.borrow().moved_away {
            return None;
        }
        let (events, mut next) = {
            let mut st = self.state.borrow_mut();
            // Runs belong to the plugin: their events stay queued until a document can receive them.
            if st.hello {
                st.tasks.poll(now)
            } else {
                (Vec::new(), None)
            }
        };
        for event in &events {
            if let TaskEvent::Exit {
                kind: ExitKind::SpawnFailed(why),
                ..
            } = event
            {
                eprintln!("[plugin {}] a task failed to start: {why}", self.id());
            }
            self.page.send(&event_message(event));
        }

        let mut replies = Vec::new();
        self.take_checks(now, &mut replies);

        let calls = std::mem::take(&mut self.state.borrow_mut().calls);
        let layout = self.state.borrow().layout.clone();
        let audience = self.audience();
        let mut keep = Vec::new();
        for mut call in calls {
            if call.document != audience.document {
                // Asked by a document that has been replaced: no answer, and the keys stay where they are. The
                // call itself still reaches nvim ahead of every later one.
                answer_or_drop(self.id(), audience, &mut call, |_| None, &mut replies);
                continue;
            }
            if now >= call.deadline {
                answer_or_drop(
                    self.id(),
                    audience,
                    &mut call,
                    |re| Some(Reply::err(re, ErrorCode::EditorTimeout, EDITOR_BUSY)),
                    &mut replies,
                );
                continue;
            }
            let kind = call.on.call;
            let answer = call.on.answer.try_take();
            match answer {
                Some(Ok(value)) => {
                    answer_or_drop(
                        self.id(),
                        audience,
                        &mut call,
                        |re| Some(Self::answered(kind, re, &value)),
                        &mut replies,
                    );
                    if kind == Call::Open {
                        if let Some(layout) = &layout {
                            if let Err(why) = layout.focus(FocusTarget::Editor) {
                                eprintln!("[plugin {}] the editor could not take the keys: {why}", self.id());
                            }
                        }
                    }
                }
                Some(Err(e)) => answer_or_drop(
                    self.id(),
                    audience,
                    &mut call,
                    |re| {
                        let message = format!("the editor could not be asked: {e}");
                        Some(Reply::err(re, ErrorCode::EditorUnavailable, &message))
                    },
                    &mut replies,
                ),
                None => keep.push(call),
            }
        }
        self.state.borrow_mut().calls.extend(keep);
        self.send_parked(now, &mut replies);
        if let Some(at) = self.pump_follow(now, &mut replies) {
            next = Some(next.map_or(at, |n| n.min(at)));
        }
        self.take_store_answers(now, &mut replies);
        self.expire_terminal_at(now, &mut replies);
        if let Some(at) = self.agents_pump(now, &mut replies) {
            next = Some(next.map_or(at, |n| n.min(at)));
        }
        {
            let st = self.state.borrow();
            if !st.calls.is_empty() {
                // Every call left is within its deadline: the ones past it were dropped above.
                let at = now + EDITOR_POLL;
                next = Some(next.map_or(at, |n| n.min(at)));
            }
            // A check's and a store operation's answer raise the wake, and the follow queue asks for its own
            // timer while a call is out: what is owed meanwhile needs a timer only for its deadline.
            let deadlines = st
                .checking
                .iter()
                .map(|c| c.deadline)
                .chain(
                    st.parked
                        .iter()
                        .filter(|p| matches!(p.on, ScopedOp::Checking { .. } | ScopedOp::CheckingDiff { .. }))
                        .map(|p| p.deadline),
                )
                .chain(st.store_waiting.values().map(|p| p.deadline))
                .chain(st.follow_owed.iter().map(|p| p.deadline))
                .chain(st.terminal_at.iter().map(|p| p.deadline));
            for at in deadlines {
                next = Some(next.map_or(at, |n| n.min(at)));
            }
        }
        self.send_replies(replies);
        let changed = {
            let mut st = self.state.borrow_mut();
            let armed = next.is_some();
            let changed = st.timer_armed != armed;
            st.timer_armed = armed;
            changed
        };
        if changed {
            trace::line(self.id(), "timer", if next.is_some() { "armed" } else { "idle" });
        }
        next
    }

    /// Acts on the checks that have finished, and gives up on the ones past their deadline.
    fn take_checks(&self, now: Instant, replies: &mut Vec<Reply>) {
        let results: Vec<(u64, Checked)> = {
            let st = self.state.borrow();
            st.checker
                .as_ref()
                .map(|c| c.rx.try_iter().collect())
                .unwrap_or_default()
        };
        let audience = self.audience();
        for (ticket, checked) in results {
            if let Checked::Project(found) = checked {
                let name = self.state.borrow_mut().project_asked.remove(&ticket);
                if let Some(name) = name {
                    self.project_found(name, found);
                }
                continue;
            }
            let waiting = {
                let mut st = self.state.borrow_mut();
                let at = st.checking.iter().position(|c| c.on.ticket == ticket);
                at.map(|i| st.checking.remove(i))
            };
            if let Some(mut c) = waiting {
                // The document that asked is gone (a reload, a stop) or the window is closing: nothing is
                // opened or started on its behalf.
                answer_or_drop(self.id(), audience, &mut c, |_| None, replies);
                // Finished after its deadline but before a pump gave it up: the page is told the filesystem
                // was too slow, as it would have been a moment earlier, and nothing is opened or started.
                if now >= c.deadline {
                    answer_or_drop(
                        self.id(),
                        audience,
                        &mut c,
                        |re| Some(Reply::err(re, ErrorCode::Io, CHECK_TIMEOUT)),
                        replies,
                    );
                }
                let Some(re) = c.re else { continue };
                match (c.on.then, checked) {
                    (Then::Open { raw, .. }, Checked::Path(Err(refusal))) => {
                        replies.push(path_refusal(re, refusal, &raw))
                    }
                    (Then::Open { line, col, .. }, Checked::Path(Ok(path))) => {
                        self.open_checked(re, &path, line, col, c.deadline)
                    }
                    (Then::Task { task, vars }, Checked::Task(resolved)) => {
                        replies.push(self.task_start(re, &task, &vars, resolved))
                    }
                    (Then::Link { target, .. }, Checked::Path(Err(refusal))) => {
                        replies.push(link_refusal(re, refusal, &target))
                    }
                    (Then::TerminalAt { raw }, Checked::Path(Err(refusal))) => {
                        replies.push(path_refusal(re, refusal, &raw))
                    }
                    (Then::TerminalAt { .. }, Checked::Path(Ok(dir))) => {
                        self.terminal_at_checked(re, dir, now, replies)
                    }
                    (Then::Link { line, session, .. }, Checked::Path(Ok(path))) => {
                        let current_session = self.state.borrow().follow.current_session();
                        if current_session != session {
                            // The editor the link was clicked against is gone; a new one is not asked to
                            // open files on its behalf.
                            replies.push(Reply::err(re, ErrorCode::EditorUnavailable, "the editor restarted"));
                        } else {
                            // Sent by the follow queue, after the call out and any parked before it, within what
                            // is left of the request's deadline. A link replaces nothing.
                            drop(self.park_page_op(FollowOp::OpenLink { path, line, re }, re, c.deadline, replies));
                        }
                    }
                    _ => {}
                }
                continue;
            }
            if let Checked::Diff(result) = checked {
                let mut st = self.state.borrow_mut();
                let at = st
                    .parked
                    .iter()
                    .position(|p| matches!(p.on, ScopedOp::CheckingDiff { ticket: t } if t == ticket));
                if let Some(i) = at {
                    if now >= st.parked[i].deadline {
                        if let Some(mut parked) = st.parked.remove(i) {
                            answer_or_drop(
                                self.id(),
                                audience,
                                &mut parked,
                                |re| Some(Reply::err(re, ErrorCode::Io, CHECK_TIMEOUT)),
                                replies,
                            );
                        }
                        continue;
                    }
                    match result {
                        Ok(args) => st.parked[i].on = ScopedOp::Diff(args),
                        Err((refusal, raw)) => {
                            if let Some(mut parked) = st.parked.remove(i) {
                                answer_or_drop(
                                    self.id(),
                                    audience,
                                    &mut parked,
                                    |re| Some(path_refusal(re, refusal, &raw)),
                                    replies,
                                );
                            }
                        }
                    }
                }
                continue;
            }
            let Checked::Path(result) = checked else { continue };
            let mut st = self.state.borrow_mut();
            let at = st
                .parked
                .iter()
                .position(|p| matches!(p.on, ScopedOp::Checking { ticket: t, .. } if t == ticket));
            let Some(i) = at else { continue };
            if now >= st.parked[i].deadline {
                let mut parked = st.parked.remove(i).expect("the parked check is present");
                answer_or_drop(
                    self.id(),
                    audience,
                    &mut parked,
                    |re| Some(Reply::err(re, ErrorCode::Io, CHECK_TIMEOUT)),
                    replies,
                );
                continue;
            }
            match result {
                Ok(root) => {
                    let parked = &mut st.parked[i];
                    if let ScopedOp::Checking { op, .. } = parked.on {
                        parked.on = ScopedOp::Apply { root, op };
                    }
                }
                Err(refusal) => {
                    let Some(mut parked) = st.parked.remove(i) else {
                        continue;
                    };
                    let raw = match &parked.on {
                        ScopedOp::Checking { raw, .. } => raw.clone(),
                        _ => continue,
                    };
                    let refused = |re| Some(path_refusal(re, refusal, &raw));
                    answer_or_drop(self.id(), audience, &mut parked, refused, replies);
                }
            }
        }

        // A check past its deadline is given up: whatever it finds later is dropped, and nothing is opened,
        // started or applied for it.
        let timed_out = |re| Some(Reply::err(re, ErrorCode::Io, CHECK_TIMEOUT));
        let mut st = self.state.borrow_mut();
        st.checking.retain_mut(|c| {
            if now < c.deadline {
                return true;
            }
            answer_or_drop(self.id(), audience, c, timed_out, replies);
            false
        });
        st.parked.retain_mut(|p| {
            if !matches!(p.on, ScopedOp::Checking { .. } | ScopedOp::CheckingDiff { .. }) || now < p.deadline {
                return true;
            }
            answer_or_drop(self.id(), audience, p, timed_out, replies);
            false
        });
    }

    /// Takes the store's answers and tells the page about each operation past its deadline. The work itself is
    /// never undone: an answer for a replaced document is dropped, a write the page was told timed out may
    /// still apply, and its late answer goes to nobody.
    fn take_store_answers(&self, now: Instant, replies: &mut Vec<Reply>) {
        let results = self
            .state
            .borrow()
            .store
            .as_ref()
            .map(StoreWorker::try_results)
            .unwrap_or_default();
        let audience = self.audience();
        for (ticket, result) in results {
            let waiting = self.state.borrow_mut().store_waiting.remove(&ticket);
            if let Some(mut waiting) = waiting {
                let answer = |re| Some(store_reply(re, result));
                answer_or_drop(self.id(), audience, &mut waiting, answer, replies);
            }
        }
        let mut st = self.state.borrow_mut();
        let late = format!(
            "the plugin's store did not answer within {:?}; a write may still be applied",
            st.store_deadline
        );
        let timed_out = |re| Some(Reply::err(re, ErrorCode::Io, &late));
        // A replaced document's operations are let go at once, so their deadlines arm no timer.
        st.store_waiting.retain(|_, waiting| {
            if now < waiting.deadline && waiting.document == audience.document && !audience.closed {
                return true;
            }
            answer_or_drop(self.id(), audience, waiting, timed_out, replies);
            false
        });
    }

    /// Sends the directory changes and clears at the head of the line, up to the first pending path check,
    /// and tells the page whose call has waited out its deadline that the editor is busy (the call itself still
    /// goes out, in its place).
    fn send_parked(&self, now: Instant, replies: &mut Vec<Reply>) {
        let audience = self.audience();
        {
            let mut st = self.state.borrow_mut();
            for parked in st.parked.iter_mut().filter(|p| now >= p.deadline) {
                let busy = |re| Some(Reply::err(re, ErrorCode::EditorTimeout, EDITOR_BUSY));
                answer_or_drop(self.id(), audience, parked, busy, replies);
            }
        }
        loop {
            let next = {
                let mut st = self.state.borrow_mut();
                if matches!(
                    st.parked.front().map(|p| &p.on),
                    Some(ScopedOp::Checking { .. } | ScopedOp::CheckingDiff { .. })
                ) {
                    return;
                }
                st.parked.pop_front()
            };
            let Some(mut parked) = next else { return };
            // A replaced document's call still goes out in its place, answering nobody.
            answer_or_drop(self.id(), audience, &mut parked, |_| None, replies);
            if let Some(reply) = self.send_scoped(parked.re, parked.on, parked.deadline) {
                replies.push(reply);
            }
        }
    }

    /// The plugin's module was closed (prefix `x`): every run is stopped, assist and cwd are restored and the follow
    /// snippet is taken down. The page is gone, so nothing is answered and nothing more is sent until a new page
    /// says hello. The stopped runs are not polled before then, so that page's hello lists them and their exits
    /// reach it; what they hold meanwhile is bounded by the runner's held-output cap.
    ///
    /// Each clear follows every change already requested, including those still waiting for a directory
    /// check. Call order stamps keep the clear in force even if the editor writes calls out of order. The
    /// returned time is when `pump` must run to take outstanding editor answers.
    pub fn stop(&self, now: Instant) -> Option<Instant> {
        self.new_document();
        let (assisted, cwd_set, diffed) = {
            let mut st = self.state.borrow_mut();
            st.tasks.stop_all();
            // The snippet's commands go with the module; the window's restart brings them back.
            st.follow_stopped = true;
            st.follow.park(FollowOp::Teardown);
            let cwd_set = std::mem::replace(&mut st.cwd_set, false);
            let diffed = st.diffed;
            (st.assisted, cwd_set, diffed)
        };
        if assisted {
            let _ = self.editor_clear(None, now);
        }
        if cwd_set {
            self.scoped_call(None, ScopedOp::CwdClear, now);
        }
        if diffed {
            self.scoped_call(None, ScopedOp::CloseDiffs, now);
        }
        let waiting = {
            let st = self.state.borrow();
            let follow =
                !st.follow_refused && st.follow_socket.is_some() && (st.follow_call.is_some() || !st.follow.stopped());
            follow || !st.calls.is_empty() || !st.parked.is_empty() || !st.checking.is_empty()
        };
        waiting.then(|| now + EDITOR_POLL)
    }

    /// The window is closing: as [`Self::stop`], and what to wait for before the process may exit. The page's
    /// requests are refused from here on, before the runs to wait for are listed, so a request still queued in
    /// the page (or sent by one of its timers) cannot start a run that nobody waits for.
    pub fn shutdown(&self, now: Instant) -> RunsWaiter {
        self.state.borrow_mut().closed = true;
        self.stop(now);
        self.state.borrow().tasks.waiter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::load::load_plugin;
    use crate::plugin::test_support::{demo_plugin, Scratch};
    use std::sync::Arc;

    #[derive(Default)]
    struct Page(RefCell<Vec<Value>>);

    impl PagePort for Page {
        fn send(&self, message: &str) {
            self.0
                .borrow_mut()
                .push(serde_json::from_str(message).expect("the host sends JSON"));
        }
        // Logged with the messages so a test sees which came first.
        fn set_theme(&self, theme: &PageTheme) {
            self.0.borrow_mut().push(json!({ "page.set_theme": theme.to_json() }));
        }
    }

    impl Page {
        fn take(&self) -> Vec<Value> {
            std::mem::take(&mut *self.0.borrow_mut())
        }
    }

    struct Rig {
        _s: Scratch,
        host: PluginHost,
        page: Rc<Page>,
    }

    /// A plugin whose manifest declares no capability at all, so every verb these tests send is one a
    /// page may send without asking for anything.
    fn rig(overrides: Vec<PluginKeyOp>) -> Rig {
        rig_with(overrides, "")
    }

    /// As [`rig`], for a plugin whose page hands its state over.
    fn handoff_rig() -> Rig {
        rig_with(vec![], "[page]\nhandoff = true\n")
    }

    fn rig_with(overrides: Vec<PluginKeyOp>, tables: &str) -> Rig {
        let s = Scratch::new("host-keys");
        let (dir, _) = demo_plugin(&s.0);
        std::fs::write(
            dir.join("eitri-plugin.toml"),
            format!(
                "id = \"demo\"\ntitle = \"Demo\"\nprotocol = 1\n\
                 site = \"build/site\"\nentry = \"index.html\"\ncapabilities = []\n{tables}"
            ),
        )
        .unwrap();
        let plugin = Rc::new(load_plugin(dir.to_str().unwrap(), &BTreeMap::new(), None).unwrap());
        let page = Rc::new(Page::default());
        let host = PluginHost::new(plugin, page.clone(), Arc::new(|| {}), None, overrides);
        Rig { _s: s, host, page }
    }

    impl Rig {
        fn send(&self, id: u64, verb: &str, args: Value) {
            self.send_at(Instant::now(), id, verb, args);
        }

        fn send_at(&self, now: Instant, id: u64, verb: &str, args: Value) {
            let text = json!({"v": 1, "id": id, "verb": verb, "args": args}).to_string();
            self.host.page_message(&text, now);
        }

        /// Sends one message and returns the one reply it gets.
        fn ask_at(&self, now: Instant, id: u64, verb: &str, args: Value) -> Value {
            self.send_at(now, id, verb, args);
            let mut sent = self.page.take();
            assert_eq!(sent.len(), 1, "{sent:?}");
            sent.remove(0)
        }

        fn hello(&self) -> Vec<Value> {
            self.send(1, "host.hello", json!({}));
            self.page.take()
        }

        fn declare(&self, rows: Value) -> Value {
            self.send(7, "keys.declare", json!({ "rows": rows }));
            let mut sent = self.page.take();
            assert_eq!(sent.len(), 1, "{sent:?}");
            sent.remove(0)
        }
    }

    #[derive(Default)]
    struct CwdEditor {
        glue: Option<crate::editor_rpc::Glue>,
        calls: RefCell<Vec<(&'static str, Vec<rmpv::Value>)>>,
        answers: RefCell<Vec<nvim_rpc::Answer>>,
    }

    impl EditorRpc for CwdEditor {
        fn exec_lua(&self, code: &'static str, args: Vec<rmpv::Value>) -> nvim_rpc::Pending {
            let (answer, pending) = nvim_rpc::Pending::pair();
            self.calls.borrow_mut().push((code, args));
            self.answers.borrow_mut().push(answer);
            pending
        }
        fn target(&self) -> Option<u64> {
            Some(1)
        }
        fn glue(&self) -> Option<crate::editor_rpc::Glue> {
            self.glue.clone()
        }
    }

    #[test]
    fn agents_verbs_answer_disabled_until_served() {
        let r = rig(vec![]);
        let mut plugin = (*r.host.plugin).clone();
        plugin.manifest.capabilities = vec![Capability::Agents];
        let host = PluginHost::new(Rc::new(plugin), r.page.clone(), Arc::new(|| {}), None, vec![]);
        for (index, (verb, args)) in [
            ("agents.watch", json!({})),
            ("agents.focus", json!({"id": "a.1", "card_first": true})),
            ("agents.new", json!({"project": "9b2c4e7a10f3d856"})),
            ("agents.select", json!({"key": "k-5c1e0b9a7d3f2e18"})),
            ("agents.round", json!({"op": "start"})),
            ("agents.stop", json!({"id": "a.1"})),
        ]
        .into_iter()
        .enumerate()
        {
            let id = index as u64 + 1;
            let raw = json!({"v": 1, "id": id, "verb": verb, "args": args}).to_string();
            r.host.page_message(&raw, Instant::now());
            let replies = r.page.take();
            assert_eq!(replies.len(), 1);
            assert_eq!(replies[0]["error"]["code"], "not_declared");
            host.page_message(&raw, Instant::now());
            let replies = r.page.take();
            assert_eq!(replies.len(), 1);
            assert_eq!(replies[0]["re"], id);
            assert_eq!(replies[0]["error"]["code"], "disabled");
        }
    }

    #[test]
    fn every_follow_op_carries_the_glue_when_the_editor_has_one() {
        use crate::editor_rpc::GlueSlot;
        for slot in [GlueSlot::Page("demo".into()), GlueSlot::Agent] {
            let r = rig(vec![]);
            let rpc = Rc::new(CwdEditor {
                glue: Some(crate::editor_rpc::Glue {
                    slot: slot.clone(),
                    chan: 17,
                }),
                ..CwdEditor::default()
            });
            r.host.set_editor(rpc.clone());
            let socket = agent::socket_path::in_dir(&std::env::temp_dir(), "follow.sock").unwrap();
            let prefix = std::env::temp_dir().join("nv-sc-");
            r.host.set_follow_feed(socket, None);
            r.host.set_scratch_prefix(Some(prefix.clone()));
            let now = Instant::now();
            r.host.follow_session(now);
            r.host.send_follow(now);
            let _ = editor::follow_on(&*rpc, "demo", true);
            let _ = editor::follow_text(&*rpc, "demo", 1, 100, 100);
            let _ = editor::follow_reveal(&*rpc, "demo", 1, 2, 3, None, None);
            let _ = editor::follow_jump(&*rpc, "demo", 1, 2, 0, None);
            let _ = editor::follow_teardown(&*rpc, "demo");
            let calls = rpc.calls.borrow();
            assert_eq!(calls.len(), 6);
            for ((code, args), op) in calls
                .iter()
                .zip(["install", "follow", "text", "reveal", "jump", "teardown"])
            {
                assert!(std::ptr::eq(*code, editor::PLUGIN_FOLLOW_LUA));
                assert_eq!(args[0].as_str(), Some(op));
                let field = |value: &rmpv::Value, key: &str| {
                    value
                        .as_map()
                        .unwrap()
                        .iter()
                        .find(|(k, _)| k.as_str() == Some(key))
                        .map(|(_, v)| v.clone())
                };
                let glue = field(&args[1], "slot").expect("every follow op identifies its glue");
                assert_eq!(field(&glue, "chan"), Some(17.into()));
                match &slot {
                    GlueSlot::Page(id) => assert_eq!(field(&glue, "id"), Some(id.as_str().into())),
                    GlueSlot::Agent => assert_eq!(field(&glue, "id"), None),
                }
                if op == "install" {
                    use std::os::unix::ffi::OsStrExt;
                    assert_eq!(
                        field(&args[1], "scratch_prefix"),
                        Some(rmpv::Value::Binary(prefix.as_os_str().as_bytes().to_vec()))
                    );
                }
            }
        }
    }

    #[test]
    fn a_refused_install_calls_on_follow_refused_once_and_does_not_retry() {
        let r = rig(vec![]);
        let host = Rc::new(r.host);
        let rpc = Rc::new(CwdEditor::default());
        host.set_editor(rpc.clone());
        let socket = agent::socket_path::in_dir(&std::env::temp_dir(), "follow.sock").unwrap();
        host.set_follow_feed(socket, None);
        let called = Rc::new(Cell::new(0));
        let count = called.clone();
        let weak_host = Rc::downgrade(&host);
        host.on_follow_refused(Rc::new(move || {
            count.set(count.get() + 1);
            // The hook may inspect the host while closing its window.
            let _ = weak_host.upgrade().unwrap().state.borrow();
        }));
        let now = Instant::now();
        host.pump(now);
        assert_eq!(rpc.calls.borrow().len(), 1);
        rpc.answers.borrow_mut().remove(0).send(Ok(rmpv::Value::Map(vec![
            ("ok".into(), false.into()),
            ("slot".into(), false.into()),
        ])));
        host.pump(now);
        assert_eq!(called.get(), 1);
        for seconds in [1, 10, 100] {
            host.editor_session(now + Duration::from_secs(seconds));
            host.pump(now + Duration::from_secs(seconds));
        }
        assert_eq!(called.get(), 1);
        assert_eq!(
            rpc.calls.borrow().len(),
            1,
            "a refusal cannot be repaired by another install"
        );
    }

    #[test]
    fn after_a_refusal_a_follow_request_is_told_the_editor_refused() {
        let r = rig(vec![]);
        r.hello();
        let host = Rc::new(r.host);
        let rpc = Rc::new(CwdEditor::default());
        host.set_editor(rpc.clone());
        let socket = agent::socket_path::in_dir(&std::env::temp_dir(), "follow.sock").unwrap();
        host.set_follow_feed(socket, None);
        let now = Instant::now();
        host.pump(now);
        rpc.answers.borrow_mut().remove(0).send(Ok(rmpv::Value::Map(vec![
            ("ok".into(), false.into()),
            ("slot".into(), false.into()),
        ])));
        host.pump(now);
        let reply = host.editor_follow(9).expect("follow is refused after a refusal");
        let error = reply.error.expect("an error answer");
        assert_eq!(error.code, ErrorCode::EditorUnavailable);
        assert_eq!(error.message, "the editor refused the follow slot");
    }

    fn cwd_rig() -> (Rig, Rc<CwdEditor>) {
        let s = Scratch::new("host-cwd");
        let (dir, settings) = demo_plugin(&s.0);
        let manifest = std::fs::read_to_string(dir.join("eitri-plugin.toml")).unwrap();
        let manifest = manifest.replace("\"editor.open\"", "\"editor.open\", \"editor.cwd\"");
        std::fs::write(
            dir.join("eitri-plugin.toml"),
            format!(
                "{manifest}\n[window]\nroot = \"${{work}}\"\ntitle = \"Demo\"\nlayout = {{ module = \"editor\" }}\n"
            ),
        )
        .unwrap();
        let plugin = Rc::new(load_plugin(dir.to_str().unwrap(), &settings, None).unwrap());
        let page = Rc::new(Page::default());
        let host = PluginHost::new(plugin, page.clone(), Arc::new(|| {}), None, vec![]);
        let editor = Rc::new(CwdEditor::default());
        host.set_editor(editor.clone());
        host.set_own_window(true);
        (Rig { _s: s, host, page }, editor)
    }

    fn buffers_rig() -> (Rig, Rc<CwdEditor>) {
        let (r, editor) = cwd_rig();
        // The manifest was loaded already; give this rig only the buffer capability it exercises.
        let mut plugin = (*r.host.plugin).clone();
        plugin.manifest.capabilities.push(Capability::EditorBuffers);
        let page = r.page.clone();
        let host = PluginHost::new(Rc::new(plugin), page.clone(), Arc::new(|| {}), None, vec![]);
        host.set_editor(editor.clone());
        (Rig { _s: r._s, host, page }, editor)
    }

    fn buffer_args(r: &Rig, verb: &str) -> Value {
        if verb == "editor.diff" {
            return text_diff();
        }
        let root = r._s.0.join("work");
        if verb == "editor.preload" {
            let path = root.join("file.rs");
            std::fs::write(&path, "fn main() {}\n").unwrap();
            json!({"path": path, "diagnostics": "on", "inlay_hints": "off"})
        } else {
            json!({"root": root})
        }
    }

    #[test]
    fn buffer_verbs_need_editor_buffers() {
        for verb in ["editor.preload", "editor.modified", "editor.reload", "editor.diff"] {
            let r = rig(vec![]);
            let args = buffer_args(&r, verb);
            assert_eq!(error_code(&r.ask_at(Instant::now(), 1, verb, args)), "not_declared");
        }
    }

    #[test]
    fn buffer_verbs_reject_outside_roots_and_bad_policy() {
        for verb in ["editor.preload", "editor.modified", "editor.reload"] {
            let (r, editor) = buffers_rig();
            let outside = r._s.0.join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            let args = if verb == "editor.preload" {
                let path = outside.join("a.rs");
                std::fs::write(&path, "a").unwrap();
                json!({"path": path, "diagnostics": "on", "inlay_hints": "off"})
            } else {
                json!({"root": outside})
            };
            r.send_at(Instant::now(), 1, verb, args);
            pump_until(&r, Instant::now(), || !r.page.0.borrow().is_empty());
            assert_eq!(error_code(&r.page.take()[0]), "outside_roots");
            assert!(editor.calls.borrow().is_empty());
        }
        let (r, _) = buffers_rig();
        assert_eq!(
            error_code(&r.ask_at(
                Instant::now(),
                2,
                "editor.preload",
                json!({"path": "/work/a.rs", "diagnostics": "yes", "inlay_hints": "off"})
            )),
            "bad_request"
        );
    }

    #[test]
    fn buffer_verbs_have_a_five_second_deadline() {
        for verb in ["editor.preload", "editor.modified", "editor.reload"] {
            let (r, editor) = buffers_rig();
            let now = Instant::now();
            r.send_at(now, 1, verb, buffer_args(&r, verb));
            pump_until(&r, now, || {
                !editor.calls.borrow().is_empty() || !r.page.0.borrow().is_empty()
            });
            assert_eq!(editor.calls.borrow().len(), 1);
            assert!(r.page.take().is_empty());
            r.host.pump(now + EDITOR_DEADLINE);
            assert_eq!(error_code(&r.page.take()[0]), "editor_timeout");
            r.host.pump(now + EDITOR_DEADLINE);
            assert!(r.page.take().is_empty());
        }
    }

    #[test]
    fn buffer_path_checks_expire_without_sending_the_late_operation() {
        for verb in ["editor.preload", "editor.modified", "editor.reload"] {
            let (r, editor) = buffers_rig();
            let gate = r.host.gate_path_checks();
            let now = Instant::now();
            r.send_at(now, 1, verb, buffer_args(&r, verb));
            r.host.pump(now + CHECK_DEADLINE);
            assert_eq!(error_code(&r.page.take()[0]), "io");
            gate.send(()).unwrap();
            r.host.stop(now + CHECK_DEADLINE);
            if verb == "editor.preload" {
                pump_until(&r, now + CHECK_DEADLINE, || !editor.calls.borrow().is_empty());
                assert_eq!(editor.calls.borrow().len(), 1);
                assert_eq!(editor.calls.borrow()[0].1[0], rmpv::Value::from("wipe_preload"));
            } else {
                assert!(editor.calls.borrow().is_empty());
            }
            assert!(r.page.take().is_empty());
        }
    }

    #[test]
    fn buffer_fixtures_gate_the_capability_and_decode_editor_answers() {
        let (r, editor) = buffers_rig();
        for (verb, request, response) in [
            (
                "editor.preload",
                include_str!("fixtures/req-editor-preload.json"),
                include_str!("fixtures/rep-editor-preload.json"),
            ),
            (
                "editor.modified",
                include_str!("fixtures/req-editor-modified.json"),
                include_str!("fixtures/rep-editor-modified.json"),
            ),
            (
                "editor.reload",
                include_str!("fixtures/req-editor-reload.json"),
                include_str!("fixtures/rep-editor-reload.json"),
            ),
            (
                "editor.diff",
                include_str!("fixtures/req-editor-diff.json"),
                include_str!("fixtures/rep-editor-diff.json"),
            ),
        ] {
            let Inbound::Request(req) = protocol::parse_request(request) else {
                panic!("{verb}")
            };
            assert_eq!(req.verb.capability(), Some(Capability::EditorBuffers));
            let fixture: Value = serde_json::from_str(response).unwrap();
            let now = Instant::now();
            r.send_at(now, req.id, verb, buffer_args(&r, verb));
            pump_until(&r, now, || !editor.answers.borrow().is_empty());
            let value = match verb {
                "editor.preload" => rmpv::Value::Map(vec![
                    ("buf".into(), 7.into()),
                    ("replaced".into(), true.into()),
                    ("stopped_clients".into(), 1.into()),
                ]),
                "editor.modified" => rmpv::Value::Map(vec![(
                    "paths".into(),
                    rmpv::Value::Array(vec![fixture["result"]["paths"][0].as_str().unwrap().into()]),
                )]),
                "editor.diff" => rmpv::Value::Map(vec![("tab".into(), 2.into())]),
                _ => rmpv::Value::Map(vec![("reloaded".into(), 1.into()), ("kept_modified".into(), 0.into())]),
            };
            editor.answers.borrow_mut().pop().unwrap().send(Ok(value));
            r.host.pump(now);
            assert_eq!(r.page.take(), vec![fixture]);
        }
        assert!(matches!(
            protocol::parse_request(include_str!("fixtures/bad-editor-preload-bad-policy.json")),
            Inbound::Refused {
                code: ErrorCode::BadRequest,
                ..
            }
        ));
    }

    #[test]
    fn a_new_document_wipes_the_previous_documents_only_preloaded_buffer() {
        for finish in ["document", "stop", "shutdown"] {
            let (r, editor) = buffers_rig();
            let gate = r.host.gate_path_checks();
            let now = Instant::now();
            r.send_at(now, 1, "editor.preload", buffer_args(&r, "editor.preload"));
            match finish {
                "document" => r.host.page_committed(),
                "stop" => {
                    r.host.stop(now);
                }
                _ => {
                    r.host.shutdown(now);
                }
            }
            gate.send(()).unwrap();
            pump_until(&r, now, || editor.calls.borrow().len() >= 2);
            let calls = editor.calls.borrow();
            assert_eq!(calls[0].1[0], rmpv::Value::from("preload"));
            assert_eq!(calls[1].1[0], rmpv::Value::from("wipe_preload"));
            assert!(r.page.take().iter().all(|reply| reply.get("re").is_none()));
        }
    }

    #[test]
    fn moving_a_page_away_sends_its_preload_wipe_past_discarded_path_checks() {
        let (r, editor) = buffers_rig();
        let gate = r.host.gate_path_checks();
        let now = Instant::now();
        r.send_at(now, 1, "editor.preload", buffer_args(&r, "editor.preload"));
        gate.send(()).unwrap();
        pump_until(&r, now, || !editor.calls.borrow().is_empty());
        assert_eq!(editor.calls.borrow()[0].1[0], rmpv::Value::from("preload"));

        r.send_at(now, 2, "editor.modified", buffer_args(&r, "editor.modified"));
        r.host.forget_moved();
        gate.send(()).unwrap();
        for _ in 0..5 {
            r.host.pump(now);
            std::thread::sleep(Duration::from_millis(1));
        }
        let calls = editor.calls.borrow();
        assert_eq!(calls.last().unwrap().1[0], rmpv::Value::from("wipe_preload"));
        assert_eq!(calls.len(), 2, "no buffer operation follows the wipe");
    }

    fn text_diff() -> Value {
        json!({"left": {"title": "Attempt", "text": "fn main() {}\n", "filetype": "rust"},
               "right": {"title": "Current", "text": "fn main() { }\n", "filetype": "rust"}})
    }

    #[test]
    fn diff_refuses_a_title_over_80_chars_or_with_a_control_or_hidden_character() {
        let (r, editor) = buffers_rig();
        for title in [
            "x".repeat(81),
            "a\nb".into(),
            "a\u{85}b".into(),
            "a\u{202e}b".into(),
            "a\u{3164}b".into(),
            "".into(),
        ] {
            let mut args = text_diff();
            args["left"]["title"] = title.into();
            assert_eq!(
                error_code(&r.ask_at(Instant::now(), 1, "editor.diff", args)),
                "bad_request"
            );
        }
        assert!(editor.calls.borrow().is_empty());
    }

    #[test]
    fn diff_refuses_a_text_over_96_kib() {
        let (r, editor) = buffers_rig();
        let mut args = text_diff();
        args["right"]["text"] = "x".repeat(96 * 1024 + 1).into();
        assert_eq!(
            error_code(&r.ask_at(Instant::now(), 1, "editor.diff", args)),
            "bad_request"
        );
        assert!(editor.calls.borrow().is_empty());
    }

    #[test]
    fn diff_refuses_a_filetype_outside_a_z0_9_underscore_1_to_32() {
        let (r, editor) = buffers_rig();
        for ft in ["", "Rust", "rust.lua", "rust|quit", "rust\n", &"x".repeat(33)] {
            let mut args = text_diff();
            args["left"]["filetype"] = ft.into();
            assert_eq!(
                error_code(&r.ask_at(Instant::now(), 1, "editor.diff", args)),
                "bad_request"
            );
        }
        assert!(editor.calls.borrow().is_empty());
    }

    #[test]
    fn diff_path_outside_roots_answers_outside_roots() {
        for side in ["left", "right"] {
            let (r, editor) = buffers_rig();
            let outside = r._s.0.join("outside.rs");
            std::fs::write(&outside, "a\n").unwrap();
            let mut args = text_diff();
            args[side] = json!({"path": outside});
            r.send_at(Instant::now(), 1, "editor.diff", args);
            pump_until(&r, Instant::now(), || !r.page.0.borrow().is_empty());
            assert_eq!(error_code(&r.page.take()[0]), "outside_roots");
            assert!(editor.calls.borrow().is_empty());
        }
    }

    #[test]
    fn diff_cleanup_follows_pending_checks_on_stop_and_window_close() {
        for shutdown in [false, true] {
            let (r, editor) = buffers_rig();
            let gate = r.host.gate_path_checks();
            let now = Instant::now();
            r.send_at(now, 1, "editor.diff", text_diff());
            if shutdown {
                r.host.shutdown(now);
            } else {
                r.host.stop(now);
            }
            gate.send(()).unwrap();
            pump_until(&r, now, || editor.calls.borrow().len() >= 2);
            let calls = editor.calls.borrow();
            assert_eq!(calls[0].1[0], rmpv::Value::from("diff"));
            assert_eq!(calls[1].1[0], rmpv::Value::from("close_diffs"));
            assert!(r.page.take().is_empty());
        }
    }

    #[test]
    fn diff_path_check_past_its_deadline_never_reaches_the_editor() {
        let (r, editor) = buffers_rig();
        let gate = r.host.gate_path_checks();
        let now = Instant::now();
        r.send_at(now, 1, "editor.diff", text_diff());
        r.host.pump(now + CHECK_DEADLINE);
        assert_eq!(error_code(&r.page.take()[0]), "io");
        gate.send(()).unwrap();
        r.host.stop(now + CHECK_DEADLINE);
        pump_until(&r, now + CHECK_DEADLINE, || !editor.calls.borrow().is_empty());
        assert_eq!(editor.calls.borrow().len(), 1);
        assert_eq!(editor.calls.borrow()[0].1[0], rmpv::Value::from("close_diffs"));
    }

    #[test]
    fn diff_text_limits_count_utf8_bytes_and_titles_count_characters() {
        let (r, editor) = buffers_rig();
        let now = Instant::now();
        let mut args = text_diff();
        args["left"]["title"] = "界".repeat(80).into();
        args["left"]["text"] = "界".repeat(32 * 1024).into();
        args["left"]["filetype"] = "x".repeat(32).into();
        r.send_at(now, 1, "editor.diff", args.clone());
        pump_until(&r, now, || !editor.calls.borrow().is_empty());
        args["left"]["text"] = format!("{}x", "界".repeat(32 * 1024)).into();
        assert_eq!(error_code(&r.ask_at(now, 2, "editor.diff", args)), "bad_request");
        for control in ["\u{0}", "\r", "\u{7f}", "\u{85}"] {
            let mut args = text_diff();
            args["right"]["text"] = control.into();
            assert_eq!(error_code(&r.ask_at(now, 3, "editor.diff", args)), "bad_request");
        }
    }

    #[test]
    fn diff_text_side_without_a_filetype_reaches_the_editor_without_one() {
        let (r, editor) = buffers_rig();
        let now = Instant::now();
        let mut args = text_diff();
        args["left"] = json!({"title": "notes.md", "text": "# a\n"});
        r.send_at(now, 1, "editor.diff", args);
        pump_until(&r, now, || !editor.calls.borrow().is_empty());
        let calls = editor.calls.borrow();
        assert_eq!(calls[0].1[0], rmpv::Value::from("diff"));
        let side = |name: &str| {
            calls[0].1[1]
                .as_map()
                .unwrap()
                .iter()
                .find(|(k, _)| k.as_str() == Some(name))
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        let keys = |side: rmpv::Value| {
            side.as_map()
                .unwrap()
                .iter()
                .map(|(k, _)| k.as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            keys(side("left")),
            ["title", "text"],
            "an absent filetype must stay absent, not nil"
        );
        assert_eq!(keys(side("right")), ["title", "text", "filetype"]);
    }

    #[test]
    fn diff_has_a_five_second_deadline_and_decodes_the_tab() {
        for timeout in [false, true] {
            let (r, editor) = buffers_rig();
            let now = Instant::now();
            r.send_at(now, 1, "editor.diff", text_diff());
            pump_until(&r, now, || !editor.calls.borrow().is_empty());
            if timeout {
                r.host.pump(now + EDITOR_DEADLINE);
                assert_eq!(error_code(&r.page.take()[0]), "editor_timeout");
            } else {
                editor
                    .answers
                    .borrow_mut()
                    .pop()
                    .unwrap()
                    .send(Ok(rmpv::Value::Map(vec![("tab".into(), 2.into())])));
                r.host.pump(now);
                assert_eq!(r.page.take()[0]["result"], json!({"tab": 2}));
            }
        }
    }

    fn pump_until(r: &Rig, now: Instant, ready: impl Fn() -> bool) {
        let guard = Instant::now() + Duration::from_secs(60);
        while !ready() {
            assert!(Instant::now() < guard, "the path worker did not answer");
            r.host.pump(now);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn editor_cd_needs_editor_cwd() {
        let r = rig(vec![]);
        let reply = r.ask_at(Instant::now(), 1, "editor.cd", json!({"dir": "/work"}));
        assert_eq!(error_code(&reply), "not_declared");
        assert!(reply["error"]["message"].as_str().unwrap().contains("editor.cwd"));
    }

    #[test]
    fn editor_cd_outside_the_own_window_answers_not_own_window() {
        let (r, editor) = cwd_rig();
        r.host.set_own_window(false);
        let fixture: Value = serde_json::from_str(include_str!("fixtures/rep-error-not-own-window.json")).unwrap();
        let reply = r.ask_at(
            Instant::now(),
            fixture["re"].as_u64().unwrap(),
            "editor.cd",
            json!({"dir": "/work"}),
        );
        assert_eq!(reply, fixture);
        assert_eq!(fixture["error"]["message"], NOT_OWN_WINDOW_TEXT);
        assert!(editor.calls.borrow().is_empty());
        assert!(r.host.state.borrow().checker.is_none());
    }

    #[test]
    fn editor_cd_refuses_a_relative_dir_and_a_control_character() {
        let (r, editor) = cwd_rig();
        for dir in ["work/ex", "/work/line\nbreak", "/work/\u{7f}"] {
            let reply = r.ask_at(Instant::now(), 1, "editor.cd", json!({"dir": dir}));
            assert_eq!(error_code(&reply), "bad_request", "{reply}");
        }
        assert!(editor.calls.borrow().is_empty());
        assert!(r.host.state.borrow().checker.is_none());
    }

    #[test]
    fn editor_cd_outside_roots_answers_outside_roots() {
        let (r, editor) = cwd_rig();
        let now = Instant::now();
        r.send_at(now, 1, "editor.cd", json!({"dir": r._s.0}));
        pump_until(&r, now, || !r.page.0.borrow().is_empty());
        assert_eq!(error_code(&r.page.take()[0]), "outside_roots");
        assert!(editor.calls.borrow().is_empty());
    }

    #[test]
    fn editor_cd_on_a_file_answers_io() {
        let (r, editor) = cwd_rig();
        let file = r._s.0.join("work/file");
        std::fs::write(&file, "").unwrap();
        let now = Instant::now();
        r.send_at(now, 1, "editor.cd", json!({"dir": file}));
        pump_until(&r, now, || !r.page.0.borrow().is_empty());
        assert_eq!(error_code(&r.page.take()[0]), "io");
        assert!(editor.calls.borrow().is_empty());
    }

    #[test]
    fn editor_cd_past_5_s_answers_editor_timeout_once() {
        let (r, editor) = cwd_rig();
        let now = Instant::now();
        r.send_at(now, 1, "editor.cd", json!({"dir": r._s.0.join("work")}));
        pump_until(&r, now, || {
            !editor.calls.borrow().is_empty() || !r.page.0.borrow().is_empty()
        });
        assert_eq!(editor.calls.borrow().len(), 1);
        assert!(r.page.take().is_empty());
        r.host.pump(now + EDITOR_DEADLINE);
        let replies = r.page.take();
        assert_eq!(replies.len(), 1);
        assert_eq!(error_code(&replies[0]), "editor_timeout");
        editor.answers.borrow_mut().pop().unwrap().send(Ok(rmpv::Value::Nil));
        assert_eq!(r.host.pump(now + EDITOR_DEADLINE), None);
        assert!(r.page.take().is_empty());
    }

    #[test]
    fn editor_cd_sends_canonical_bytes_and_decodes_the_reply_without_moving_keys() {
        #[derive(Default)]
        struct Layout(Cell<u64>);
        impl LayoutPort for Layout {
            fn focus(&self, _: FocusTarget) -> Result<(), String> {
                self.0.set(self.0.get() + 1);
                Ok(())
            }
            fn terminal_at(&self, _: PathBuf, _: u64) -> Result<(), String> {
                Err("not used by this test".to_string())
            }
        }
        let (r, editor) = cwd_rig();
        let layout = Rc::new(Layout::default());
        r.host.set_layout(layout.clone());
        let now = Instant::now();
        let work = r._s.0.join("work");
        let link = r._s.0.join("link");
        std::os::unix::fs::symlink(&work, &link).unwrap();
        r.send_at(now, 24, "editor.cd", json!({"dir": link}));
        pump_until(&r, now, || !editor.calls.borrow().is_empty());
        let calls = editor.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert!(std::ptr::eq(calls[0].0, editor::PLUGIN_CWD_LUA));
        assert_eq!(calls[0].1[0], rmpv::Value::from("cd"));
        let opts = calls[0].1[1].as_map().unwrap();
        let arg = |name| &opts.iter().find(|(k, _)| k.as_str() == Some(name)).unwrap().1;
        assert_eq!(arg("owner"), &rmpv::Value::from("demo"));
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(arg("dir"), &rmpv::Value::Binary(work.as_os_str().as_bytes().to_vec()));
        drop(calls);
        editor
            .answers
            .borrow_mut()
            .pop()
            .unwrap()
            .send(Ok(rmpv::Value::Map(vec![
                ("dir".into(), work.to_str().unwrap().into()),
                ("tab".into(), 3.into()),
            ])));
        r.host.pump(now);
        assert_eq!(layout.0.get(), 0, "cd leaves the keys where they were");
        assert_eq!(
            r.page.take(),
            vec![json!({"v": 1, "re": 24, "ok": true, "result": {"dir": work, "tab": 3}})]
        );
    }

    #[test]
    fn editor_cd_without_an_editor_answers_editor_unavailable() {
        let (r, editor) = cwd_rig();
        r.host.state.borrow_mut().editor = None;
        let reply = r.ask_at(Instant::now(), 1, "editor.cd", json!({"dir": r._s.0.join("work")}));
        assert_eq!(error_code(&reply), "editor_unavailable");
        assert!(editor.calls.borrow().is_empty());
        assert!(r.host.state.borrow().checker.is_none());
    }

    #[test]
    fn editor_cd_path_check_times_out_once_and_never_sends_the_late_change() {
        let (r, editor) = cwd_rig();
        let gate = r.host.gate_path_checks();
        let now = Instant::now();
        r.send_at(now, 1, "editor.cd", json!({"dir": r._s.0.join("work")}));
        r.host.pump(now + CHECK_DEADLINE);
        let reply = r.page.take();
        assert_eq!(reply.len(), 1);
        assert_eq!(error_code(&reply[0]), "io");
        gate.send(()).unwrap();
        r.host.stop(now + CHECK_DEADLINE);
        pump_until(&r, now + CHECK_DEADLINE, || !editor.calls.borrow().is_empty());
        assert_eq!(editor.calls.borrow().len(), 1, "only the clear may reach nvim");
        assert_eq!(editor.calls.borrow()[0].1[0], rmpv::Value::from("clear"));
        assert!(r.page.take().is_empty());
    }

    #[test]
    fn editor_cd_stop_and_shutdown_clear_after_a_pending_check_without_waiting_for_answers() {
        for closing in [false, true] {
            let (r, editor) = cwd_rig();
            let gate = r.host.gate_path_checks();
            let now = Instant::now();
            r.send_at(now, 1, "editor.cd", json!({"dir": r._s.0.join("work")}));
            if closing {
                r.host.shutdown(now);
            } else {
                assert!(r.host.stop(now).is_some());
            }
            assert!(editor.calls.borrow().is_empty());
            gate.send(()).unwrap();
            pump_until(&r, now, || editor.calls.borrow().len() == 2);
            let calls = editor.calls.borrow();
            assert!(calls
                .iter()
                .all(|(code, _)| std::ptr::eq(*code, editor::PLUGIN_CWD_LUA)));
            assert_eq!(calls[0].1[0], rmpv::Value::from("cd"));
            assert_eq!(calls[1].1[0], rmpv::Value::from("clear"));
            let field = |at: usize, name: &str| {
                calls[at].1[1]
                    .as_map()
                    .unwrap()
                    .iter()
                    .find(|(k, _)| k.as_str() == Some(name))
                    .unwrap()
                    .1
                    .clone()
            };
            assert_eq!(field(0, "maker"), field(1, "maker"));
            assert!(field(0, "order").as_u64() < field(1, "order").as_u64());
            drop(calls);
            r.host.stop(now);
            assert_eq!(editor.calls.borrow().len(), 2, "one clear per change");
            assert_eq!(r.host.pump(now + EDITOR_DEADLINE), None);
            assert!(r.page.take().is_empty(), "a stopped page gets no old answer");
        }
    }

    #[test]
    fn the_fixtures_parse() {
        let req = include_str!("fixtures/req-editor-cd.json");
        let Inbound::Request(req) = protocol::parse_request(req) else {
            panic!("editor.cd must parse")
        };
        assert_eq!(req.verb.name(), "editor.cd");
        assert_eq!(req.verb.capability(), Some(Capability::EditorCwd));
        for fixture in [
            include_str!("fixtures/rep-editor-cd.json"),
            include_str!("fixtures/rep-error-not-own-window.json"),
        ] {
            let reply: Reply = serde_json::from_str(fixture).unwrap();
            assert_eq!(
                serde_json::to_value(reply).unwrap(),
                serde_json::from_str::<Value>(fixture).unwrap()
            );
        }
    }

    fn row(action: &str, keys: &[&str]) -> Value {
        json!({"action": action, "keys": keys, "context": "题目", "desc": "下一题"})
    }

    fn focus_events(sent: &[Value]) -> Vec<Value> {
        sent.iter().filter(|m| m["event"] == "focus").cloned().collect()
    }

    fn error_code(reply: &Value) -> &str {
        reply["error"]["code"].as_str().unwrap_or("")
    }

    #[test]
    fn host_save_times_out_after_one_second_and_the_move_goes_on_without_state() {
        let r = handoff_rig();
        r.hello();
        let t0 = Instant::now();
        assert_eq!(r.host.take_saved(t0), SaveOutcome::NotAsked);
        r.host.request_save(t0 + SAVE_DEADLINE);
        assert_eq!(r.page.take(), vec![json!({"v": 1, "event": "host.save", "data": {}})]);
        assert_eq!(r.host.take_saved(t0 + Duration::from_millis(999)), SaveOutcome::Waiting);
        assert_eq!(r.host.take_saved(t0 + SAVE_DEADLINE), SaveOutcome::TimedOut);
        // An answer after the deadline is refused: the move has already gone on without it.
        let late = r.ask_at(t0 + SAVE_DEADLINE, 5, "page.saved", json!({"state": {"scroll": 3}}));
        assert_eq!(error_code(&late), "bad_request", "{late}");
        assert_eq!(r.host.take_saved(t0 + SAVE_DEADLINE), SaveOutcome::NotAsked);

        // In time, the state is kept until taken, even when it is taken after the deadline.
        let t1 = Instant::now();
        r.host.request_save(t1 + SAVE_DEADLINE);
        r.page.take();
        let reply = r.ask_at(t1, 6, "page.saved", json!({"state": {"scroll": 3}}));
        assert_eq!(
            (reply["re"].clone(), reply["ok"].clone()),
            (json!(6), json!(true)),
            "{reply}"
        );
        let again = r.ask_at(t1, 7, "page.saved", json!({"state": {"scroll": 4}}));
        assert_eq!(error_code(&again), "bad_request", "one answer per question: {again}");
        assert_eq!(
            r.host.take_saved(t1 + 2 * SAVE_DEADLINE),
            SaveOutcome::Saved(json!({"scroll": 3}))
        );
        assert_eq!(r.host.take_saved(t1), SaveOutcome::NotAsked);

        // A page that has not said hello cannot hear the question, so the move does not wait for it.
        let quiet = handoff_rig();
        quiet.host.request_save(t0 + SAVE_DEADLINE);
        assert!(quiet.page.take().is_empty());
        assert_eq!(quiet.host.take_saved(t0), SaveOutcome::TimedOut);

        // A document replaced while it was asked never answers either.
        let reloaded = handoff_rig();
        reloaded.hello();
        reloaded.host.request_save(t0 + SAVE_DEADLINE);
        reloaded.host.page_committed();
        assert_eq!(reloaded.host.take_saved(t0), SaveOutcome::TimedOut);

        // A plugin without the table is not asked at all, and may not answer.
        let plain = rig(vec![]);
        plain.hello();
        plain.host.request_save(t0 + SAVE_DEADLINE);
        assert!(plain.page.take().is_empty());
        assert_eq!(plain.host.take_saved(t0), SaveOutcome::NotHandoff);
        let refused = plain.ask_at(t0, 8, "page.saved", json!({"state": 1}));
        assert_eq!(error_code(&refused), "not_declared", "{refused}");
    }

    #[test]
    fn a_saved_state_over_256_kib_is_refused() {
        let r = handoff_rig();
        r.hello();
        let t0 = Instant::now();
        r.host.request_save(t0 + SAVE_DEADLINE);
        r.page.take();
        let big = json!({"state": "x".repeat(protocol::MAX_SAVED_STATE)});
        let reply = r.ask_at(t0, 9, "page.saved", big);
        assert_eq!(error_code(&reply), "too_large", "{reply}");
        assert!(
            reply["error"]["message"].as_str().unwrap().contains("256 KiB"),
            "{reply}"
        );
        // Nothing was kept, and the question is still open for a state that fits.
        assert_eq!(r.host.take_saved(t0), SaveOutcome::Waiting);
        let fits = r.ask_at(t0, 10, "page.saved", json!({"state": "x"}));
        assert_eq!(fits["ok"], true, "{fits}");
        assert_eq!(r.host.take_saved(t0), SaveOutcome::Saved(json!("x")));
    }

    #[test]
    fn restored_is_delivered_once_in_the_next_hello_reply() {
        let hello_restored = |r: &Rig| {
            let sent = r.hello();
            let reply = sent.iter().find(|m| m["re"] == 1).expect("a reply").clone();
            reply["result"]
                .get("restored")
                .cloned()
                .expect("hello always carries restored")
        };
        let r = handoff_rig();
        assert_eq!(hello_restored(&r), Value::Null);
        r.host.set_restored(json!({"scroll": 12, "draft": "let"}));
        assert!(r.page.take().is_empty(), "the state waits for the page's hello");
        r.host.page_committed();
        r.page.take();
        assert_eq!(hello_restored(&r), json!({"scroll": 12, "draft": "let"}));
        assert_eq!(hello_restored(&r), Value::Null, "given once");
        r.host.page_committed();
        r.page.take();
        assert_eq!(hello_restored(&r), Value::Null, "a later document starts afresh");

        let plain = rig(vec![]);
        plain.host.set_restored(json!({"scroll": 12}));
        assert_eq!(
            hello_restored(&plain),
            Value::Null,
            "a page without the table never asked for one"
        );
    }

    #[test]
    fn the_hello_and_ready_trace_lines_name_the_document() {
        let r = handoff_rig();
        let lines = trace::capture(|| {
            r.ask_at(Instant::now(), 3, "page.ready", json!({}));
            r.host.page_committed();
            r.page.take();
            r.hello();
            r.ask_at(Instant::now(), 4, "page.ready", json!({}));
            r.host.set_restored(json!({"scroll": 12}));
            r.host.page_committed();
            r.page.take();
            r.hello();
            r.ask_at(Instant::now(), 5, "page.ready", json!({}));
        });
        assert_eq!(
            lines,
            [
                "[plugin demo] trace: page.ready refused before host.hello",
                "[plugin demo] trace: hello restored=no document=1",
                "[plugin demo] trace: page.ready document=1",
                "[plugin demo] trace: hello restored=yes document=2",
                "[plugin demo] trace: page.ready document=2",
            ]
        );

        let plain = rig(vec![]);
        let lines = trace::capture(|| {
            plain.hello();
            plain.ask_at(Instant::now(), 6, "page.ready", json!({}));
        });
        assert_eq!(
            lines,
            [
                "[plugin demo] trace: hello restored=no document=0",
                "[plugin demo] trace: page.ready refused no [page] handoff",
            ]
        );
    }

    #[test]
    fn a_forgotten_document_is_off_screen_until_the_window_says_again() {
        let r = handoff_rig();
        r.host.set_on_screen(true, Instant::now());
        assert!(r.host.state.borrow().on_screen);
        r.host.forget_moved();
        assert!(!r.host.state.borrow().on_screen, "as its new follow state is");
        r.host.set_on_screen(true, Instant::now());
        assert!(r.host.state.borrow().on_screen);
    }

    #[test]
    fn a_ready_names_the_document_that_said_it_until_another_starts() {
        let r = handoff_rig();
        assert_eq!(r.host.hello_document(), None, "no document has said hello");
        r.host.page_committed();
        r.page.take();
        r.hello();
        r.ask_at(Instant::now(), 3, "page.ready", json!({}));
        assert!(r.host.take_ready());
        let ready = r.host.hello_document();
        assert!(ready.is_some());
        assert_eq!(
            r.host.hello_document(),
            ready,
            "the same document while nothing else happens"
        );
        r.host.page_committed();
        assert_eq!(r.host.hello_document(), None, "a new document has not said hello yet");
        r.page.take();
        r.hello();
        assert_ne!(r.host.hello_document(), ready, "a new document is another document");
        r.host.accept_moved();
        assert_eq!(r.host.hello_document(), None, "an arrival starts a new document");
        // Its document answers once the move is over.
        r.host.resume_moved();
        r.page.take();
        r.hello();
        assert!(r.host.hello_document().is_some());
        r.host.forget_moved();
        assert_eq!(r.host.hello_document(), None, "a forgotten document is gone");
    }

    #[test]
    fn page_ready_is_taken_once() {
        let r = handoff_rig();
        let early = r.ask_at(Instant::now(), 3, "page.ready", json!({}));
        assert_eq!(error_code(&early), "bad_request", "ready before hello: {early}");
        assert!(!r.host.take_ready());
        r.hello();
        assert!(!r.host.take_ready());
        let reply = r.ask_at(Instant::now(), 4, "page.ready", json!({}));
        assert_eq!(reply["ok"], true, "{reply}");
        assert!(r.host.take_ready());
        assert!(!r.host.take_ready(), "taken once");
        r.ask_at(Instant::now(), 5, "page.ready", json!({}));
        r.host.page_committed();
        assert!(!r.host.take_ready(), "the document that was ready is gone");

        let plain = rig(vec![]);
        plain.hello();
        let refused = plain.ask_at(Instant::now(), 6, "page.ready", json!({}));
        assert_eq!(error_code(&refused), "not_declared", "{refused}");
        assert!(!plain.host.take_ready());
    }

    #[test]
    fn a_state_read_pending_across_new_document_sends_no_reply() {
        let s = Scratch::new("host-stale-read");
        let (dir, settings) = demo_plugin(&s.0);
        let plugin = Rc::new(load_plugin(dir.to_str().unwrap(), &settings, None).unwrap());
        let store = PluginStore::new(s.0.join("state"), "demo");
        store.write("position", &json!("old page")).unwrap();
        let page = Rc::new(Page::default());
        let (done, ready) = mpsc::channel();
        let wake = Arc::new(move || {
            let _ = done.send(());
        });
        let host = PluginHost::new(plugin, page.clone(), wake, Some(store), Vec::new());
        host.page_message(
            &json!({"v": 1, "id": 42, "verb": "state.read", "args": {"key": "position"}}).to_string(),
            Instant::now(),
        );
        assert!(page.take().is_empty(), "the read waits for the store worker");
        host.new_document();
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        host.pump(Instant::now());
        assert!(
            page.take().is_empty(),
            "the old read must not answer into the new document"
        );
        assert!(host.state.borrow().store_waiting.is_empty(), "the answer was consumed");
        host.page_message(
            &json!({"v": 1, "id": 42, "verb": "host.hello", "args": {}}).to_string(),
            Instant::now(),
        );
        let sent = page.take();
        assert_eq!(sent[0]["result"]["host"], "eitri");
    }

    #[test]
    fn a_store_read_behind_a_held_lock_is_refused_at_its_deadline_and_the_write_still_lands() {
        let s = Scratch::new("host-store-deadline");
        let (dir, settings) = demo_plugin(&s.0);
        let plugin = Rc::new(load_plugin(dir.to_str().unwrap(), &settings, None).unwrap());
        let store = PluginStore::new(s.0.join("state"), "demo");
        store.write("position", &json!("before")).unwrap();
        let page = Rc::new(Page::default());
        let (done, woken) = mpsc::channel();
        let wake = Arc::new(move || {
            let _ = done.send(());
        });
        let host = PluginHost::new(plugin, page.clone(), wake, Some(store.clone()), Vec::new());
        // Each store operation waits for the test's word, as one behind a lock another window holds would.
        let gate = host.gate_store();
        let deadline = Duration::from_millis(50);
        host.set_store_deadline(deadline);
        let t0 = Instant::now();
        let ask = |id: u64, verb: &str, args: Value| {
            host.page_message(&json!({"v": 1, "id": id, "verb": verb, "args": args}).to_string(), t0);
        };
        ask(1, "state.write", json!({"key": "position", "value": "after"}));
        ask(2, "state.read", json!({"key": "position"}));
        let next = host.pump(t0);
        assert!(page.take().is_empty(), "nothing is answered before the deadline");
        assert!(
            next.is_some_and(|at| at <= t0 + deadline),
            "the host asks to run again by the deadline, as no answer will wake it: {next:?}"
        );

        assert_eq!(host.pump(t0 + deadline), None, "nothing is owed once both were refused");
        let sent = page.take();
        assert_eq!(sent.len(), 2, "{sent:?}");
        for (reply, re) in sent.iter().zip([1, 2]) {
            assert_eq!(reply["re"], re, "{reply}");
            assert_eq!(reply["ok"], false, "{reply}");
            assert_eq!(reply["error"]["code"], "io", "{reply}");
        }

        // The lock is let go: the write the page was told timed out still applies, and its late answer, like
        // the read's, goes to nobody.
        drop(gate);
        for _ in 0..2 {
            woken
                .recv_timeout(Duration::from_secs(60))
                .expect("the store worker answers once let through");
        }
        host.pump(Instant::now());
        assert!(
            page.take().is_empty(),
            "a request already refused is not answered again"
        );
        assert_eq!(store.read("position").unwrap(), Some(json!("after")));
    }

    #[test]
    fn a_store_answer_for_a_replaced_document_arms_no_timer() {
        let s = Scratch::new("host-store-replaced");
        let (dir, settings) = demo_plugin(&s.0);
        let plugin = Rc::new(load_plugin(dir.to_str().unwrap(), &settings, None).unwrap());
        let store = PluginStore::new(s.0.join("state"), "demo");
        let page = Rc::new(Page::default());
        let host = PluginHost::new(plugin, page.clone(), Arc::new(|| {}), Some(store), Vec::new());
        let _gate = host.gate_store();
        let t0 = Instant::now();
        host.page_message(
            &json!({"v": 1, "id": 3, "verb": "state.read", "args": {"key": "position"}}).to_string(),
            t0,
        );
        host.new_document();
        assert_eq!(host.pump(t0), None, "an answer nobody can receive needs no deadline");
        assert!(page.take().is_empty());
        assert!(host.state.borrow().store_waiting.is_empty());
    }

    #[test]
    fn hello_says_own_window_false_until_told() {
        let r = rig(vec![]);
        assert!(!r.host.own_window());
        let sent = r.hello();
        let reply = sent.iter().find(|m| m["re"] == 1).unwrap();
        assert_eq!(reply["result"]["own_window"], false);
    }

    #[test]
    fn hello_says_own_window_true_after_set_own_window() {
        let r = rig(vec![]);
        r.host.set_own_window(true);
        assert!(r.host.own_window());
        let sent = r.hello();
        let reply = sent.iter().find(|m| m["re"] == 1).unwrap();
        assert_eq!(reply["result"]["own_window"], true);
        r.host.new_document();
        assert!(r.host.own_window());
        r.host.set_own_window(false);
        let sent = r.hello();
        let reply = sent.iter().find(|m| m["re"] == 1).unwrap();
        assert_eq!(reply["result"]["own_window"], false);
    }

    #[test]
    fn hello_says_the_kit_version_and_the_default_timeoutlen() {
        let r = rig(vec![]);
        let sent = r.hello();
        let reply = sent.iter().find(|m| m["re"] == 1).expect("a reply");
        assert_eq!(reply["result"]["keys_kit"], 1);
        assert_eq!(reply["result"]["timeoutlen"], 1000);
    }

    #[test]
    fn hello_carries_the_keys_settings() {
        let r = rig(vec![]);
        let sent = r.hello();
        let reply = sent.iter().find(|m| m["re"] == 1).expect("a reply");
        assert_eq!(
            reply["result"]["keys_settings"],
            serde_json::to_value(KeysSettings::default()).unwrap()
        );
        let r = rig(vec![]);
        r.host.set_keys_settings(KeysSettings {
            scroll_step: 40,
            which_key_delay: None,
            ..KeysSettings::default()
        });
        assert!(r.page.take().is_empty(), "recording the settings sends nothing");
        let sent = r.hello();
        let reply = sent.iter().find(|m| m["re"] == 1).expect("a reply");
        assert_eq!(reply["result"]["keys_settings"]["scroll_step"], 40);
        assert_eq!(reply["result"]["keys_settings"]["which_key_delay"], Value::Null);
    }

    #[test]
    fn hello_reply_matches_the_shared_fixture() {
        let fixture: Value = serde_json::from_str(include_str!("fixtures/rep-hello.json")).unwrap();
        let r = rig(vec![]);
        r.send(fixture["re"].as_u64().unwrap(), "host.hello", json!({}));
        let sent = r.page.take();
        let reply = sent.iter().find(|m| m["re"] == fixture["re"]).expect("a reply");
        assert_eq!(
            reply.as_object().unwrap().keys().collect::<Vec<_>>(),
            fixture.as_object().unwrap().keys().collect::<Vec<_>>()
        );
        assert_eq!(
            reply["result"].as_object().unwrap().keys().collect::<Vec<_>>(),
            fixture["result"].as_object().unwrap().keys().collect::<Vec<_>>()
        );
        for key in [
            "keys_kit",
            "timeoutlen",
            "prefix",
            "module_key",
            "keys_settings",
            "own_window",
        ] {
            assert_eq!(reply["result"][key], fixture["result"][key], "{key}");
        }
        let fixture_vars = fixture["result"]["theme"]["vars"]
            .as_object()
            .expect("the fixture's theme.vars");
        let fallback = PageTheme::fallback();
        let mut want: Vec<&str> = fallback.vars().iter().map(|(n, _)| n.as_str()).collect();
        want.sort();
        assert_eq!(fixture_vars.keys().map(String::as_str).collect::<Vec<_>>(), want);
        for (name, value) in fixture_vars {
            let value = value.as_str().expect("a string value");
            assert!(crate::plugin::theme::page_var_ok(name, value), "{name}: {value}");
        }
    }

    #[test]
    fn keys_declare_reply_matches_the_shared_fixture() {
        let request: Value = serde_json::from_str(include_str!("fixtures/req-keys-declare.json")).unwrap();
        let fixture: Value = serde_json::from_str(include_str!("fixtures/rep-keys-declare.json")).unwrap();
        let r = rig(vec![]);
        r.hello();
        r.host.page_message(&request.to_string(), Instant::now());
        assert_eq!(r.page.take(), vec![fixture]);
    }

    #[test]
    fn the_focus_waits_for_hello_and_is_sent_once_after_its_reply() {
        let r = rig(vec![]);
        r.host.set_has_keys(true);
        assert!(r.page.take().is_empty(), "nothing before hello");
        let sent = r.hello();
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert_eq!(sent[0]["re"], 1, "the reply comes first");
        assert_eq!(sent[1], json!({"v": 1, "event": "focus", "data": {"has_keys": true}}));
        r.host.set_has_keys(false);
        assert_eq!(
            r.page.take(),
            vec![json!({"v": 1, "event": "focus", "data": {"has_keys": false}})]
        );
    }

    #[test]
    fn a_page_that_starts_without_the_keys_is_told_so() {
        let r = rig(vec![]);
        let sent = r.hello();
        assert_eq!(
            focus_events(&sent),
            vec![json!({"v": 1, "event": "focus", "data": {"has_keys": false}})]
        );
    }

    #[test]
    fn a_new_document_forgets_the_declaration_and_is_told_the_focus_again_after_its_hello() {
        let r = rig(vec![]);
        r.host.set_has_keys(true);
        r.hello();
        assert_eq!(r.declare(json!([row("item.next", &["]q"])]))["ok"], true);
        assert!(r.host.declared_rows().is_some());
        r.host.page_committed();
        assert!(r.host.declared_rows().is_none(), "a reloaded page declares again");
        r.page.take();
        r.host.set_has_keys(true);
        assert!(r.page.take().is_empty(), "the new document has not said hello");
        let sent = r.hello();
        assert_eq!(focus_events(&sent).len(), 1, "{sent:?}");
        assert_eq!(focus_events(&sent)[0]["data"]["has_keys"], true);
    }

    #[test]
    fn a_stop_forgets_the_declaration() {
        let r = rig(vec![]);
        r.hello();
        assert_eq!(r.declare(json!([row("item.next", &["]q"])]))["ok"], true);
        r.host.stop(Instant::now());
        assert!(r.host.declared_rows().is_none());
    }

    #[test]
    fn a_declaration_before_hello_is_refused_and_records_nothing() {
        let r = rig(vec![]);
        let reply = r.declare(json!([row("item.next", &["]q"])]));
        assert_eq!(reply["ok"], false, "{reply}");
        assert_eq!(reply["error"]["code"], "bad_request");
        assert!(r.host.declared_rows().is_none());
    }

    #[test]
    fn a_commit_starts_a_new_document() {
        let r = rig(vec![]);
        r.hello();
        assert_eq!(r.declare(json!([row("item.next", &["]q"])]))["ok"], true);
        r.host.page_committed();
        assert!(
            r.host.declared_rows().is_none(),
            "the committed document has declared nothing"
        );
        r.page.take();
        let reply = r.declare(json!([row("item.next", &["]q"])]));
        assert_eq!(reply["ok"], false, "{reply}");
        assert_eq!(
            reply["error"]["code"], "bad_request",
            "the new document has not said hello"
        );
    }

    #[test]
    fn the_old_document_declaring_while_the_new_one_loads_leaves_no_table() {
        let r = rig(vec![]);
        r.hello();
        assert_eq!(r.declare(json!([row("item.next", &["]q"])]))["ok"], true);
        // While a load is only provisional the old document is still the one shown, and may change its table;
        // a load that never commits (a download, a cancelled navigation) leaves it in place.
        let reply = r.declare(json!([row("old.row", &["x"])]));
        assert_eq!(reply["ok"], true, "{reply}");
        r.host.page_committed();
        r.page.take();
        r.hello();
        assert!(
            r.host.declared_rows().is_none(),
            "a new document that never declares has no table"
        );
    }

    #[test]
    fn set_timeoutlen_sends_all_three_fields() {
        let r = rig(vec![]);
        r.hello();
        r.host.set_timeoutlen(300);
        assert_eq!(
            r.page.take(),
            vec![json!({"v": 1, "event": "keys_config",
                        "data": {"timeoutlen": 300, "prefix": "Ctrl+b", "module_key": null}})]
        );
        r.host.set_timeoutlen(300);
        assert!(r.page.take().is_empty());
    }

    #[test]
    fn a_timeoutlen_reported_before_hello_is_in_the_hello() {
        let r = rig(vec![]);
        r.host.set_timeoutlen(300);
        assert!(r.page.take().is_empty());
        let sent = r.hello();
        assert_eq!(sent[0]["result"]["timeoutlen"], 300);
        assert!(sent.iter().all(|m| m["event"] != "keys_config"));
    }

    #[test]
    fn a_declaration_is_answered_with_the_effective_table() {
        let set = PluginKeyOp::Set {
            key: "<C-n>".into(),
            action: "item.next".into(),
        };
        let r = rig(vec![set, PluginKeyOp::Del { key: "j".into() }]);
        r.hello();
        let reply = r.declare(json!([row("item.next", &["]q"]), row("scroll.down", &["j", "<C-n>"])]));
        assert_eq!(reply["ok"], true, "{reply}");
        assert_eq!(
            reply["result"],
            json!({"rows": [
                {"action": "item.next", "keys": ["]q", "<C-n>"], "sources": ["default", "init.lua"],
                 "context": "题目", "desc": "下一题"},
                {"action": "scroll.down", "keys": [], "sources": [], "context": "题目", "desc": "下一题"},
            ]})
        );
        let rows = r.host.declared_rows().unwrap();
        assert_eq!(rows[0].keys[1], ("<C-n>".to_string(), keys::KeySource::InitLua));
        // A later declaration replaces the earlier one.
        r.declare(json!([row("only.this", &["x"])]));
        let rows = r.host.declared_rows().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].action, "only.this");
    }

    #[test]
    fn an_override_reaches_the_page_in_the_spelling_its_kit_binds() {
        let set = PluginKeyOp::Set {
            key: "<c-n>".into(),
            action: "item.next".into(),
        };
        let r = rig(vec![set]);
        r.hello();
        let reply = r.declare(json!([row("item.next", &["]q"]), row("scroll.down", &["j", "<C-n>"])]));
        assert_eq!(reply["ok"], true, "{reply}");
        assert_eq!(reply["result"]["rows"][0]["keys"], json!(["]q", "<C-n>"]));
        assert_eq!(reply["result"]["rows"][1]["keys"], json!(["j"]));
    }

    #[test]
    fn a_declaration_over_a_limit_or_with_a_kept_key_is_a_bad_request() {
        let r = rig(vec![]);
        r.hello();
        let many: Vec<Value> = (0..201).map(|i| row(&format!("a{i}"), &["j"])).collect();
        let mut long_desc = row("x", &["j"]);
        long_desc["desc"] = json!("d".repeat(81));
        let cases = vec![
            ("201 rows", json!(many)),
            (
                "11 keys",
                json!([row("x", &["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k"])]),
            ),
            ("a 41-character action", json!([row(&"a".repeat(41), &["j"])])),
            ("an 81-character desc", json!([long_desc])),
            ("a repeated action", json!([row("x", &["j"]), row("x", &["k"])])),
            ("<C-h>", json!([row("x", &["<C-h>"])])),
            ("<F11>", json!([row("x", &["<F11>"])])),
            ("<C-=>", json!([row("x", &["<C-=>"])])),
            ("<C-b>", json!([row("x", &["<C-b>"])])),
            ("<C-S-a>", json!([row("x", &["<C-S-a>"])])),
            ("<C-?>", json!([row("x", &["<C-?>"])])),
            (
                "an unknown row key",
                json!([{"action": "x", "keys": ["j"], "context": "c", "desc": "d", "x": 1}]),
            ),
        ];
        for (what, rows) in cases {
            let reply = r.declare(rows);
            assert_eq!(reply["ok"], false, "{what}");
            assert_eq!(reply["error"]["code"], "bad_request", "{what}");
            if what.starts_with('<') {
                let message = reply["error"]["message"].as_str().unwrap();
                assert!(message.contains(&format!("{what:?}")), "{what}: {message}");
            }
        }
        assert!(
            r.host.declared_rows().is_none(),
            "a refused declaration records nothing"
        );
    }

    #[test]
    fn the_prefix_the_window_uses_is_the_one_refused() {
        let r = rig(vec![]);
        r.hello();
        r.host.set_prefix_key(&KeySpec::parse("C-a").unwrap());
        r.page.take();
        assert_eq!(r.declare(json!([row("x", &["<C-a>"])]))["ok"], false);
        assert_eq!(r.declare(json!([row("x", &["<C-b>"])]))["ok"], true);
    }

    /// A theme other than the fallback, so setting it is a change.
    fn light_theme() -> PageTheme {
        let mut payload = crate::theme::payload::NvimThemePayload::empty();
        payload.options.background = "light".into();
        PageTheme::from_tokens(&crate::theme::ThemeTokens::derive(&payload))
    }

    fn theme_event(theme: &PageTheme) -> Value {
        json!({"v": 1, "event": "theme", "data": theme.to_json()})
    }

    fn port_entry(theme: &PageTheme) -> Value {
        json!({ "page.set_theme": theme.to_json() })
    }

    #[test]
    fn hello_carries_the_theme_prefix_and_module_key() {
        let r = rig(vec![]);
        let sent = r.hello();
        let result = &sent[0]["result"];
        assert_eq!(result["theme"], r.host.theme().to_json());
        assert_eq!(result["prefix"], "Ctrl+b");
        assert_eq!(result["module_key"], Value::Null);

        let r = rig(vec![]);
        r.host.set_prefix_key(&KeySpec::parse("C-a").unwrap());
        r.host.set_module_key(Some("y"));
        assert!(r.page.take().is_empty(), "nothing before hello");
        let sent = r.hello();
        assert_eq!(sent[0]["result"]["prefix"], "Ctrl+a");
        assert_eq!(sent[0]["result"]["module_key"], "y");
        assert!(sent.iter().all(|m| m["event"] != "keys_config"), "{sent:?}");
    }

    #[test]
    fn a_theme_before_hello_reaches_the_port_then_the_page() {
        let r = rig(vec![]);
        let theme = light_theme();
        r.host.set_theme(&theme);
        assert_eq!(r.page.take(), vec![port_entry(&theme), theme_event(&theme)]);
        assert_eq!(r.host.theme(), theme);
        let sent = r.hello();
        assert_eq!(sent[0]["result"]["theme"], theme.to_json());
    }

    #[test]
    fn a_changed_theme_after_hello_sends_one_event_after_the_port() {
        let r = rig(vec![]);
        r.hello();
        let theme = light_theme();
        r.host.set_theme(&theme);
        assert_eq!(r.page.take(), vec![port_entry(&theme), theme_event(&theme)]);
    }

    #[test]
    fn the_same_theme_again_sends_nothing() {
        let r = rig(vec![]);
        r.host.set_theme(&PageTheme::fallback());
        assert!(r.page.take().is_empty(), "the host starts from the fallback");
        r.hello();
        let theme = light_theme();
        r.host.set_theme(&theme);
        r.page.take();
        r.host.set_theme(&theme);
        assert!(r.page.take().is_empty());
    }

    #[test]
    fn after_a_new_load_a_change_still_sends_the_event_and_the_next_hello_has_the_newest() {
        let r = rig(vec![]);
        r.hello();
        r.host.page_committed();
        r.page.take();
        let theme = light_theme();
        r.host.set_theme(&theme);
        assert_eq!(r.page.take(), vec![port_entry(&theme), theme_event(&theme)]);
        let sent = r.hello();
        assert_eq!(sent[0]["result"]["theme"], theme.to_json());
    }

    #[test]
    fn a_commit_sends_the_current_theme_and_nothing_else() {
        let r = rig(vec![]);
        r.host.set_has_keys(true);
        r.host.page_committed();
        assert_eq!(r.page.take(), vec![theme_event(&PageTheme::fallback())]);
        r.hello();
        let theme = light_theme();
        r.host.set_theme(&theme);
        r.page.take();
        r.host.page_committed();
        assert_eq!(r.page.take(), vec![theme_event(&theme)]);
        r.host.page_committed();
        r.host.set_has_keys(false);
        r.host.set_timeoutlen(300);
        r.host.set_prefix_key(&KeySpec::parse("C-a").unwrap());
        r.host.set_module_key(Some("y"));
        assert_eq!(
            r.page.take(),
            vec![theme_event(&theme)],
            "focus and keys_config wait for the new document's hello"
        );
    }

    #[test]
    fn a_new_prefix_after_hello_sends_one_complete_keys_config() {
        let r = rig(vec![]);
        r.hello();
        r.host.set_prefix_key(&KeySpec::parse("C-a").unwrap());
        assert_eq!(
            r.page.take(),
            vec![json!({"v": 1, "event": "keys_config",
                        "data": {"timeoutlen": 1000, "prefix": "Ctrl+a", "module_key": null}})]
        );
        r.host.set_prefix_key(&KeySpec::parse("C-a").unwrap());
        assert!(r.page.take().is_empty(), "the same prefix again");
        r.host.set_module_key(Some("y"));
        assert_eq!(
            r.page.take(),
            vec![json!({"v": 1, "event": "keys_config",
                        "data": {"timeoutlen": 1000, "prefix": "Ctrl+a", "module_key": "y"}})]
        );
        r.host.set_module_key(Some("y"));
        assert!(r.page.take().is_empty(), "the same module key again");
        r.host.set_module_key(None);
        assert_eq!(
            r.page.take(),
            vec![json!({"v": 1, "event": "keys_config",
                        "data": {"timeoutlen": 1000, "prefix": "Ctrl+a", "module_key": null}})]
        );

        let r = rig(vec![]);
        r.host.set_prefix_key(&KeySpec::parse("C-a").unwrap());
        r.host.set_module_key(Some("y"));
        assert!(r.page.take().is_empty(), "nothing before hello");
        let sent = r.hello();
        assert_eq!(sent[0]["result"]["prefix"], "Ctrl+a");
        assert_eq!(sent[0]["result"]["module_key"], "y");
    }

    /// The window's layout as `terminal.at` reaches it: what it was asked, and a refusal when one is set.
    #[derive(Default)]
    struct TermLayout {
        asked: RefCell<Vec<(PathBuf, u64)>>,
        refuse: RefCell<Option<String>>,
    }

    impl LayoutPort for TermLayout {
        fn focus(&self, _target: FocusTarget) -> Result<(), String> {
            Ok(())
        }
        fn terminal_at(&self, dir: PathBuf, ticket: u64) -> Result<(), String> {
            self.asked.borrow_mut().push((dir, ticket));
            match self.refuse.borrow().clone() {
                Some(why) => Err(why),
                None => Ok(()),
            }
        }
    }

    struct TermRig {
        _s: Scratch,
        host: PluginHost,
        page: Rc<Page>,
        layout: Rc<TermLayout>,
        work: PathBuf,
    }

    /// A plugin with a window and `capabilities`, its host told whether this is its own window (`own`): a window 2
    /// of a pair is never told.
    fn term_rig(capabilities: &str, own: Option<bool>) -> TermRig {
        let s = Scratch::new("host-terminal-at");
        let (dir, settings) = demo_plugin(&s.0);
        std::fs::write(
            dir.join("eitri-plugin.toml"),
            format!(
                "id = \"demo\"\ntitle = \"Demo\"\nprotocol = 1\nsite = \"build/site\"\nentry = \"index.html\"\n\
                 capabilities = [{capabilities}]\n\
                 [settings]\nwork = {{ default = \"~/demo-work\", kind = \"dir\" }}\n\
                 [roots]\nwork = \"${{work}}\"\n\
                 [window]\nroot = \"${{work}}\"\ntitle = \"Study\"\n\
                 layout = {{ row = [{{ module = \"page\" }}, {{ module = \"terminal\" }}] }}\n"
            ),
        )
        .unwrap();
        let plugin = Rc::new(load_plugin(dir.to_str().unwrap(), &settings, None).unwrap());
        let page = Rc::new(Page::default());
        let host = PluginHost::new(plugin, page.clone(), Arc::new(|| {}), None, vec![]);
        let layout = Rc::new(TermLayout::default());
        host.set_layout(layout.clone());
        if let Some(own) = own {
            host.set_own_window(own);
        }
        let work = s.0.join("work");
        std::fs::create_dir_all(work.join("ex/src")).unwrap();
        std::fs::write(work.join("ex/src/lib.rs"), "").unwrap();
        TermRig {
            _s: s,
            host,
            page,
            layout,
            work,
        }
    }

    const TERMINAL_CWD: &str = "\"terminal.cwd\"";

    impl TermRig {
        fn ask(&self, id: u64, dir: &Path) {
            let text = json!({"v": 1, "id": id, "verb": "terminal.at", "args": {"dir": dir}}).to_string();
            self.host.page_message(&text, Instant::now());
        }

        /// Pumps until the path check is back: the window was asked (its ticket), or the page was answered.
        fn checked(&self) -> Option<u64> {
            let before = self.layout.asked.borrow().len();
            let guard = Instant::now() + Duration::from_secs(30);
            loop {
                self.host.pump(Instant::now());
                if self.layout.asked.borrow().len() > before {
                    return self.layout.asked.borrow().last().map(|(_, ticket)| *ticket);
                }
                if !self.page.0.borrow().is_empty() {
                    return None;
                }
                assert!(Instant::now() < guard, "the path check never came back");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    #[test]
    fn terminal_at_needs_terminal_cwd() {
        let r = term_rig("\"tasks\"", Some(true));
        r.ask(55, &r.work.join("ex"));
        let sent = r.page.take();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(error_code(&sent[0]), "not_declared", "{}", sent[0]);
        assert!(r.layout.asked.borrow().is_empty());
    }

    #[test]
    fn terminal_at_outside_the_own_window_answers_not_own_window() {
        let r = term_rig(TERMINAL_CWD, Some(true));
        r.host.set_own_window(false);
        r.ask(55, &r.work.join("ex"));
        let sent = r.page.take();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(
            sent[0],
            json!({"v": 1, "re": 55, "ok": false, "error": {"code": "not_own_window", "message": NOT_OWN_WINDOW_TEXT}})
        );
        assert_eq!(NOT_OWN_WINDOW_TEXT, "This verb acts only in the plugin's own window");
        assert!(r.layout.asked.borrow().is_empty(), "nothing reaches the window");
    }

    #[test]
    fn window_two_terminal_at_answers_not_own_window() {
        // The second window of a pair never tells its hosts they are in their own window.
        let r = term_rig(TERMINAL_CWD, None);
        r.ask(56, &r.work.join("ex"));
        let sent = r.page.take();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(error_code(&sent[0]), "not_own_window", "{}", sent[0]);
        assert!(r.layout.asked.borrow().is_empty());
    }

    #[test]
    fn terminal_at_checks_its_dir_like_any_page_path() {
        let r = term_rig(TERMINAL_CWD, Some(true));
        r.ask(57, Path::new("relative/dir"));
        assert_eq!(error_code(&r.page.take()[0]), "bad_request");
        r.ask(58, &r.work.join("../.."));
        assert_eq!(r.checked(), None);
        assert_eq!(error_code(&r.page.take()[0]), "outside_roots");
        r.ask(59, &r.work.join("ex/src/lib.rs"));
        assert_eq!(r.checked(), None);
        assert_eq!(error_code(&r.page.take()[0]), "io", "a file is not a directory");
        assert!(r.layout.asked.borrow().is_empty());
    }

    #[test]
    fn the_answer_reaches_the_page_once_through_terminal_at_answered() {
        let r = term_rig(TERMINAL_CWD, Some(true));
        let answers = [
            (
                TerminalAtAnswer::Restarted,
                json!({"v": 1, "re": 60, "ok": true, "result": {"how": "restarted"}}),
            ),
            (
                TerminalAtAnswer::StartDir,
                json!({"v": 1, "re": 61, "ok": true, "result": {"how": "start_dir"}}),
            ),
            (
                TerminalAtAnswer::Already,
                json!({"v": 1, "re": 62, "ok": true, "result": {"how": "already"}}),
            ),
            (
                TerminalAtAnswer::Busy("a program is running in the terminal".into()),
                json!({"v": 1, "re": 63, "ok": false,
                       "error": {"code": "busy", "message": "a program is running in the terminal"}}),
            ),
        ];
        for (id, (answer, reply)) in (60..).zip(answers) {
            r.ask(id, &r.work.join("ex"));
            let ticket = r.checked().expect("the window is asked");
            assert_eq!(
                r.layout.asked.borrow().last().unwrap().0,
                r.work.join("ex"),
                "the checked directory, canonical"
            );
            assert!(r.page.take().is_empty(), "nothing until the window answers");
            r.host.terminal_at_answered(ticket, answer.clone(), Instant::now());
            assert_eq!(r.page.take(), vec![reply]);
            r.host.terminal_at_answered(ticket, answer, Instant::now());
            r.host.pump(Instant::now() + EDITOR_DEADLINE * 2);
            assert!(r.page.take().is_empty(), "answered once");
        }
        let tickets: Vec<u64> = r.layout.asked.borrow().iter().map(|(_, t)| *t).collect();
        let mut unique = tickets.clone();
        unique.dedup();
        assert_eq!(tickets, unique, "each request has its own ticket");
    }

    #[test]
    fn an_answer_for_a_replaced_document_is_dropped_with_one_log_line() {
        let r = term_rig(TERMINAL_CWD, Some(true));
        r.ask(64, &r.work.join("ex"));
        let ticket = r.checked().unwrap();
        r.host.page_committed();
        r.page.take();
        r.host
            .terminal_at_answered(ticket, TerminalAtAnswer::Restarted, Instant::now());
        assert!(r.page.take().is_empty(), "the new document asked nothing");
        r.host.pump(Instant::now() + EDITOR_DEADLINE * 2);
        assert!(r.page.take().is_empty(), "and is not told of a deadline either");
    }

    #[test]
    fn past_the_deadline_the_page_has_busy_the_terminal_did_not_answer_within_6_s_once() {
        let r = term_rig(TERMINAL_CWD, Some(true));
        r.ask(65, &r.work.join("ex"));
        let ticket = r.checked().unwrap();
        let asked = Instant::now();
        r.host.pump(asked);
        assert!(r.page.take().is_empty());
        r.host.pump(asked + TERMINAL_AT_WAIT + Duration::from_millis(1));
        assert!(
            r.page.take().is_empty(),
            "the window answers by its own deadline; this one waits past it"
        );
        let late = asked + TERMINAL_AT_DEADLINE + Duration::from_millis(1);
        r.host.pump(late);
        assert_eq!(
            r.page.take(),
            vec![json!({"v": 1, "re": 65, "ok": false,
                        "error": {"code": "busy", "message": "the terminal did not answer within 6 s"}})]
        );
        r.host.pump(late);
        r.host.terminal_at_answered(ticket, TerminalAtAnswer::Restarted, late);
        assert!(r.page.take().is_empty(), "told once; the late answer is dropped");
    }

    #[test]
    fn a_port_err_is_busy_with_its_text() {
        let r = term_rig(TERMINAL_CWD, Some(true));
        *r.layout.refuse.borrow_mut() = Some("the layout is changing; try again".into());
        r.ask(66, &r.work.join("ex"));
        r.checked();
        assert_eq!(
            r.page.take(),
            vec![json!({"v": 1, "re": 66, "ok": false,
                        "error": {"code": "busy", "message": "the layout is changing; try again"}})]
        );
        r.host.pump(Instant::now() + EDITOR_DEADLINE * 2);
        assert!(r.page.take().is_empty(), "a refusal is the one answer");
    }
}
