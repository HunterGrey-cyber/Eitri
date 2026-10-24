//! One plugin's side of the page bridge: every verb a page sends, checked against the capabilities its
//! manifest declares and answered through one funnel to the page. Nothing here touches a toolkit or waits:
//! the page, the layout and the editor are reached through small traits the window implements, and task
//! output, editor answers and store answers are taken by `pump`, which the window runs when the host's
//! wake is raised or when `pump` asked for a timer.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::doc_route::{self, DocSlot};
use super::editor::{self, decode_apply, decode_clear, decode_open, read_applied, FollowInstall, PulledText};
use super::follow::{Done, FollowOp, FollowState, Next, MAX_TEXT_BYTES, MAX_TEXT_LINES};
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

const CHECK_TIMEOUT: &str = "the filesystem did not answer within 5 s";
const EDITOR_BUSY: &str = "the editor did not answer within 5 s; it may be waiting for a key (press Esc and try again)";

/// The page. `send` is asynchronous and never calls back into the host before it returns.
pub trait PagePort {
    fn send(&self, message: &str);
    /// The view's background and its user style sheet: what the next document starts with, and the
    /// background a reload shows before it paints.
    fn set_theme(&self, theme: &PageTheme);
}

/// The window's layout: give the keys to the editor (showing it first if hidden) or to the plugin's module.
pub trait LayoutPort {
    fn focus(&self, target: FocusTarget) -> Result<(), String>;
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
    Open,
    Assist,
    Clear,
}

struct EditorCall {
    call: Call,
    answer: nvim_rpc::Pending,
}

/// An assist call waiting behind an apply whose root is still being checked.
///
/// The editor writes calls in the order they were made (`EditorRpc`'s doc), so an assist call goes out as
/// soon as nothing ahead of it is still being checked, whether or not the calls before it have been
/// answered: a clear made after an apply reaches nvim after it and undoes it. In the rare case the embedded
/// editor writes two of them swapped, each carries the order it was made in and the snippet skips one older
/// than a call it has already run (`editor::assist_apply`), so the call made last is still the one in force.
/// Only the check, which runs on the path checker's thread, can take longer than the calls made after it;
/// the assist line keeps their order across it.
enum AssistOp {
    /// An apply whose root is still being checked: it holds its place in line, and nothing behind it is sent
    /// until it is either an `Apply` or gone.
    Checking {
        ticket: u64,
        raw: String,
        diagnostics: bool,
        inlay_hints: bool,
    },
    Apply {
        root: PathBuf,
        diagnostics: bool,
        inlay_hints: bool,
    },
    Clear,
}

/// What the path checker is asked. `canonicalize` and `metadata` can stall on a slow or hung filesystem, so
/// they never run on the thread that calls the host.
enum CheckJob {
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
    /// Counts documents: raised by every reload and by a stop, so an answer still out for an older one
    /// is never sent into the document that replaced it.
    document: u64,
    editor: Option<Rc<dyn EditorRpc>>,
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
    /// Assist calls waiting behind an apply whose root is still being checked, in the order they were asked.
    parked: VecDeque<Pending<AssistOp>>,
    checker: Option<PathChecker>,
    checking: Vec<Pending<Checking>>,
    /// Handed to the checker when it starts: each check then waits for one message (tests only).
    check_gate: Option<mpsc::Receiver<()>>,
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
}

pub struct PluginHost {
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
            document: 0,
            editor: None,
            layout: None,
            tasks: TaskRunner::new(wake.clone()),
            store: None,
            store_waiting: BTreeMap::new(),
            store_deadline: STORE_DEADLINE,
            store_gate: None,
            next_ticket: 1,
            calls: Vec::new(),
            assisted: false,
            parked: VecDeque::new(),
            checker: None,
            checking: Vec::new(),
            check_gate: None,
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
        };
        PluginHost {
            plugin,
            page,
            wake,
            store,
            overrides,
            state: RefCell::new(state),
        }
    }

    pub fn plugin(&self) -> &LoadedPlugin {
        &self.plugin
    }

    pub fn set_editor(&self, editor: Rc<dyn EditorRpc>) {
        self.state.borrow_mut().editor = Some(editor);
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
    /// page is still owed for the reveal it replaced, if any.
    fn park_page_op(&self, op: FollowOp, re: u64, deadline: Instant) -> Option<Pending<()>> {
        let mut st = self.state.borrow_mut();
        let replaced = st.follow.park(op);
        let document = st.document;
        st.follow_owed.push(Pending::new(Some(re), document, deadline, ()));
        replaced.and_then(|old| take_owed(&mut st.follow_owed, old))
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

    /// The editor quit for good: a following page is told (`editor.closed {reason: "editor"}`).
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
            return Some(Reply::err(
                re,
                ErrorCode::EditorUnavailable,
                "stopped with prefix x; the plugin comes back when the window restarts",
            ));
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
                if let Ok(applied) = applied {
                    trace::line(self.id(), event, &format!("line={line} applied={applied}"));
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
            if st.follow_call.is_some() {
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
                let (socket, scratch_dir) = {
                    let st = self.state.borrow();
                    (st.follow_socket.clone(), st.scratch_dir.clone())
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
                (editor::follow_install(&*editor, &install), None)
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
                buf, line, token, re, ..
            } => (editor::follow_reveal(&*editor, id, *buf, *line, *token), Some(*re)),
            FollowOp::Jump { buf, line, col, re, .. } => {
                (editor::follow_jump(&*editor, id, *buf, *line, *col), Some(*re))
            }
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
        let (follows, dropped) = {
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
            (st.follow_socket.is_some(), std::mem::take(&mut st.follow_owed))
        };
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
        }
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
            return Self::not_handoff(re, "page.ready");
        }
        {
            let mut st = self.state.borrow_mut();
            if !st.hello {
                return Reply::err(re, ErrorCode::BadRequest, "page.ready before host.hello");
            }
            st.ready = true;
        }
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
            return Some(Reply::err(
                re,
                ErrorCode::EditorUnavailable,
                "stopped with prefix x; the plugin comes back when the window restarts",
            ));
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
            re,
        };
        if let Some(mut old) = self.park_page_op(op, re, now + EDITOR_DEADLINE) {
            let mut replies = Vec::new();
            let audience = self.audience();
            answer_or_drop(
                self.id(),
                audience,
                &mut old,
                |re| Some(Reply::ok(re, json!({ "applied": false }))),
                &mut replies,
            );
            self.send_replies(replies);
        }
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
            re,
        };
        // A jump replaces nothing.
        let _ = self.park_page_op(op, re, now + EDITOR_DEADLINE);
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
        let answer = editor::open(&*editor, path, line, col);
        self.push_call(Some(re), Call::Open, answer, deadline);
    }

    fn editor_assist(&self, re: u64, raw: &str, diagnostics: OnOff, inlay_hints: OnOff, now: Instant) -> Option<Reply> {
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
        let ticket = match self.submit_check(CheckJob::Dir(raw.to_owned())) {
            Ok(ticket) => ticket,
            Err(why) => return Some(Reply::err(re, ErrorCode::Io, &why)),
        };
        // Marked now, not on the answer: a stop before the answer still clears it.
        let mut st = self.state.borrow_mut();
        st.assisted = true;
        // Parked even when nothing is outstanding, so a clear sent while the root is being checked waits
        // behind it.
        let document = st.document;
        let op = AssistOp::Checking {
            ticket,
            raw: raw.to_owned(),
            diagnostics: diagnostics == OnOff::On,
            inlay_hints: inlay_hints == OnOff::On,
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
        self.assist_call(re, AssistOp::Clear, now);
        None
    }

    /// Sends an assist call now when no apply ahead of it is still being checked, else parks it behind that.
    fn assist_call(&self, re: Option<u64>, op: AssistOp, now: Instant) {
        let busy = !self.state.borrow().parked.is_empty();
        if busy {
            let mut st = self.state.borrow_mut();
            let document = st.document;
            st.parked
                .push_back(Pending::new(re, document, now + EDITOR_DEADLINE, op));
            return;
        }
        let _ = self.send_assist(re, op, now + EDITOR_DEADLINE);
    }

    /// The editor is asked now. Its absence is answered to the page, as nothing is then in force.
    fn send_assist(&self, re: Option<u64>, op: AssistOp, deadline: Instant) -> Option<Reply> {
        let Some(editor) = self.editor() else {
            return re.map(|re| Reply::err(re, ErrorCode::EditorUnavailable, "this window has no editor running"));
        };
        let (call, answer) = match &op {
            AssistOp::Checking { .. } => return None,
            AssistOp::Apply {
                root,
                diagnostics,
                inlay_hints,
            } => (
                Call::Assist,
                editor::assist_apply(&*editor, self.id(), root, *diagnostics, *inlay_hints),
            ),
            AssistOp::Clear => (Call::Clear, editor::assist_clear(&*editor, self.id())),
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
            Call::Open => Reply::ok(re, json!({})),
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
                        .filter(|p| matches!(p.on, AssistOp::Checking { .. }))
                        .map(|p| p.deadline),
                )
                .chain(st.store_waiting.values().map(|p| p.deadline))
                .chain(st.follow_owed.iter().map(|p| p.deadline));
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
                    (Then::Link { line, session, .. }, Checked::Path(Ok(path))) => {
                        let current_session = self.state.borrow().follow.current_session();
                        if current_session != session {
                            // The editor the link was clicked against is gone; a new one is not asked to
                            // open files on its behalf.
                            replies.push(Reply::err(re, ErrorCode::EditorUnavailable, "the editor restarted"));
                        } else {
                            // Sent by the follow queue, after the call out and any parked before it, within what
                            // is left of the request's deadline. A link replaces nothing.
                            let _ = self.park_page_op(FollowOp::OpenLink { path, line, re }, re, c.deadline);
                        }
                    }
                    _ => {}
                }
                continue;
            }
            let Checked::Path(result) = checked else { continue };
            let mut st = self.state.borrow_mut();
            let at = st
                .parked
                .iter()
                .position(|p| matches!(p.on, AssistOp::Checking { ticket: t, .. } if t == ticket));
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
                    if let AssistOp::Checking {
                        diagnostics,
                        inlay_hints,
                        ..
                    } = parked.on
                    {
                        parked.on = AssistOp::Apply {
                            root,
                            diagnostics,
                            inlay_hints,
                        };
                    }
                }
                Err(refusal) => {
                    let Some(mut parked) = st.parked.remove(i) else {
                        continue;
                    };
                    let raw = match &parked.on {
                        AssistOp::Checking { raw, .. } => raw.clone(),
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
            if !matches!(p.on, AssistOp::Checking { .. }) || now < p.deadline {
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

    /// Sends the assist calls at the head of the line, up to the first apply whose root is still being checked,
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
                if matches!(st.parked.front().map(|p| &p.on), Some(AssistOp::Checking { .. })) {
                    return;
                }
                st.parked.pop_front()
            };
            let Some(mut parked) = next else { return };
            // A replaced document's call still goes out in its place, answering nobody.
            answer_or_drop(self.id(), audience, &mut parked, |_| None, replies);
            if let Some(reply) = self.send_assist(parked.re, parked.on, parked.deadline) {
                replies.push(reply);
            }
        }
    }

    /// The plugin's module was closed (prefix `x`): every run is stopped, the assist is put back and the follow
    /// snippet is taken down. The page is gone, so nothing is answered and nothing more is sent until a new page
    /// says hello. The stopped runs are not polled before then, so that page's hello lists them and their exits
    /// reach it; what they hold meanwhile is bounded by the runner's held-output cap.
    ///
    /// The clear is made after every assist call already sent, so it undoes them whether or not they have been
    /// answered, and stays in force even if the editor writes it ahead of one of them (`AssistOp`); the returned time is when `pump` must run to take its answer, as it does after any other
    /// call that leaves an editor answer outstanding.
    pub fn stop(&self, now: Instant) -> Option<Instant> {
        self.new_document();
        let assisted = {
            let mut st = self.state.borrow_mut();
            st.tasks.stop_all();
            // The snippet's commands go with the module; the window's restart brings them back.
            st.follow_stopped = true;
            st.follow.park(FollowOp::Teardown);
            st.assisted
        };
        if assisted {
            let _ = self.editor_clear(None, now);
        }
        let waiting = {
            let st = self.state.borrow();
            let follow = st.follow_socket.is_some() && (st.follow_call.is_some() || !st.follow.stopped());
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
    fn hello_reply_keys_and_keyboard_values_match_the_shared_fixture() {
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
        for key in ["keys_kit", "timeoutlen", "prefix", "module_key", "keys_settings"] {
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
}
