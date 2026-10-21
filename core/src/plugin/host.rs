//! One plugin's side of the page bridge: every verb a page sends, checked against the capabilities its
//! manifest declares and answered through one funnel to the page. Nothing here touches a toolkit or waits:
//! the page, the layout and the editor are reached through small traits the window implements, and task
//! output, editor answers and store answers are taken by `pump`, which the window runs when the host's
//! wake is raised or when `pump` asked for a timer.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::editor::{self, decode_apply, decode_clear};
use super::keys::{self, EffectiveRow, PluginKeyOp};
use super::kit_settings::KeysSettings;
use super::load::{LoadedPlugin, ResolvedTask, TaskRefusal};
use super::protocol::{
    self, ErrorCode, ExitStatus, FocusEvent, FocusTarget, Inbound, KeyRowArg, KeysConfig, OnOff, Reply, StreamName,
    TaskExit, TaskOutput, Verb,
};
use super::roots::{self, PathRefusal};
use super::state::{PluginStore, StoreError, StoreErrorKind, StoreOp, StoreWorker};
use super::tasks::{ExitKind, Launch, RunInfo, RunsWaiter, StartError, Stream, TaskEvent, TaskRunner};
use super::theme::PageTheme;
use crate::editor_rpc::EditorRpc;
use crate::keymap::{Chord, KeySpec};
use crate::nvim_rpc::Pending;

/// How long an editor call may take before the page is told the editor is busy.
pub const EDITOR_DEADLINE: Duration = Duration::from_secs(5);
/// How often a pending editor answer is looked at, by a one-shot timer, while one is outstanding.
pub const EDITOR_POLL: Duration = Duration::from_millis(20);
/// How often an assist call the page has already been told about is looked at: it stays tracked until the
/// editor answers, so the calls behind it never overtake it, but nobody is waiting on it any more.
pub const EDITOR_LATE_POLL: Duration = Duration::from_millis(250);
/// How long a page-supplied path (or a task's program and working directory) may take to check before the
/// page is told the filesystem did not answer.
pub const CHECK_DEADLINE: Duration = Duration::from_secs(5);
/// How long a key sequence waits for its next key until the editor reports its own `timeoutlen`: vim's
/// default.
pub const DEFAULT_TIMEOUTLEN: u32 = 1000;

const CHECK_TIMEOUT: &str = "the filesystem did not answer within 5 s";

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    Open,
    Assist,
    Clear,
}

struct EditorCall {
    /// `None` for a call the host made on its own (the clear when the plugin stops): nobody is answered.
    re: Option<u64>,
    call: Call,
    pending: Pending,
    deadline: Instant,
    /// The document that asked; its answer is dropped once another document has replaced it.
    document: u64,
}

/// An assist call waiting for the one before it to be answered.
///
/// The editor runs every request on a thread of its own, so two requests sent back to back may reach nvim in
/// either order: a clear that overtakes the apply it was meant to undo clears nothing, and the apply then
/// installs settings no one will ever clear. Assist calls are therefore sent one at a time, each after the
/// previous one has been answered (or has run out its deadline).
struct ParkedAssist {
    /// `None` for a call the host made on its own, or once the page has been told the editor is busy.
    re: Option<u64>,
    op: AssistOp,
    deadline: Instant,
    document: u64,
}

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
}

enum Checked {
    Path(Result<PathBuf, PathRefusal>),
    Task(Result<ResolvedTask, TaskRefusal>),
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

/// An `editor.open` or `task.start` waiting for its check.
struct Checking {
    ticket: u64,
    re: u64,
    document: u64,
    deadline: Instant,
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
    /// The store worker carries a ticket rather than the page's id: ticket -> (document, the page's id).
    store_waiting: BTreeMap<u64, (u64, u64)>,
    next_ticket: u64,
    calls: Vec<EditorCall>,
    /// An assist was sent and not cleared since.
    assisted: bool,
    parked: VecDeque<ParkedAssist>,
    checker: Option<PathChecker>,
    checking: Vec<Checking>,
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

    /// A document was committed in the view (a load, a reload, a back/forward-cache restore): it gets the
    /// current `theme` event. No other state changes.
    pub fn page_committed(&self) {
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
        st.checking.push(Checking {
            ticket,
            re,
            document,
            deadline: now + CHECK_DEADLINE,
            then,
        });
        None
    }

    /// A new document started loading (a reload, a crash's recovery, a navigation): it must say hello again,
    /// and nothing still out for the old one is answered.
    pub fn page_loading(&self) {
        self.new_document();
    }

    fn new_document(&self) {
        let mut st = self.state.borrow_mut();
        st.hello = false;
        st.document += 1;
        // A new document has its own keys, and declares them once it has loaded.
        st.declared = None;
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

    fn log_dropped_reply(&self, re: u64, reason: &str) {
        eprintln!("[plugin {}] dropped reply {re}: {reason}", self.id());
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
            Verb::StateRead(args) => self.store_op(re, StoreOp::Read { key: args.key }),
            Verb::StateWrite(args) => self.store_op(
                re,
                StoreOp::Write {
                    key: args.key,
                    value: args.value,
                },
            ),
            Verb::LogAppend(args) => self.store_op(
                re,
                StoreOp::Append {
                    stream: args.stream,
                    items: args.items,
                },
            ),
            Verb::LogRead(args) => self.store_op(
                re,
                StoreOp::ReadLog {
                    stream: args.stream,
                    from: args.from,
                },
            ),
        }
    }

    fn hello(&self, re: u64) -> Reply {
        // One step: the tail handed over and the lines it already holds taken out of the queue, so a line
        // is delivered once (in the tail) and not again as output.
        let runs = self.state.borrow_mut().tasks.attach();
        self.state.borrow_mut().hello = true;
        let (timeoutlen, prefix, module_key, theme, keys_settings) = {
            let st = self.state.borrow();
            (
                st.timeoutlen,
                st.prefix_label.clone(),
                st.module_key.clone(),
                st.theme.to_json(),
                st.keys_settings.clone(),
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
                "keys_kit": 1,
                "keys_settings": keys_settings,
                "timeoutlen": timeoutlen,
                "prefix": prefix,
                "module_key": module_key,
                "theme": theme,
            }),
        )
    }

    fn keys_declare(&self, re: u64, rows: Vec<KeyRowArg>) -> Reply {
        // A document that has not said hello may be the previous one, still running script after a new
        // load started: its table would stand for a page that never declared one.
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

    fn push_call(&self, re: Option<u64>, call: Call, pending: Pending, deadline: Instant) {
        let mut st = self.state.borrow_mut();
        let document = st.document;
        st.calls.push(EditorCall {
            re,
            call,
            pending,
            deadline,
            document,
        });
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
        let pending = editor::open(&*editor, path, line, col);
        self.push_call(Some(re), Call::Open, pending, deadline);
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
        st.parked.push_back(ParkedAssist {
            re: Some(re),
            op: AssistOp::Checking {
                ticket,
                raw: raw.to_owned(),
                diagnostics: diagnostics == OnOff::On,
                inlay_hints: inlay_hints == OnOff::On,
            },
            deadline: now + EDITOR_DEADLINE,
            document,
        });
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

    /// Sends an assist call now when none is outstanding, else parks it behind the ones that are.
    fn assist_call(&self, re: Option<u64>, op: AssistOp, now: Instant) {
        let busy = {
            let st = self.state.borrow();
            !st.parked.is_empty() || st.calls.iter().any(|c| matches!(c.call, Call::Assist | Call::Clear))
        };
        if busy {
            let mut st = self.state.borrow_mut();
            let document = st.document;
            st.parked.push_back(ParkedAssist {
                re,
                op,
                deadline: now + EDITOR_DEADLINE,
                document,
            });
            return;
        }
        let _ = self.send_assist(re, op, now + EDITOR_DEADLINE);
    }

    /// The editor is asked now. Its absence is answered to the page, as nothing is then in force.
    fn send_assist(&self, re: Option<u64>, op: AssistOp, deadline: Instant) -> Option<Reply> {
        let Some(editor) = self.editor() else {
            return re.map(|re| Reply::err(re, ErrorCode::EditorUnavailable, "this window has no editor running"));
        };
        let (call, pending) = match &op {
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
        self.push_call(re, call, pending, deadline);
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

    fn store_op(&self, re: u64, op: StoreOp) -> Option<Reply> {
        let Some(store) = self.store.clone() else {
            return Some(Reply::err(
                re,
                ErrorCode::Io,
                "no state directory: neither XDG_STATE_HOME nor HOME gives one",
            ));
        };
        let mut st = self.state.borrow_mut();
        if st.store.is_none() {
            let spawned = StoreWorker::spawn(store, self.wake.clone());
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
        let ticket = st.next_ticket;
        st.next_ticket += 1;
        let document = st.document;
        st.store_waiting.insert(ticket, (document, re));
        if let Some(worker) = st.store.as_ref() {
            worker.submit(ticket, op);
        }
        None
    }

    fn answered(&self, call: &EditorCall, value: &rmpv::Value) -> Option<Reply> {
        let re = call.re?;
        Some(match call.call {
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
        })
    }

    /// Takes what is ready -- task output, editor answers, store answers -- and sends it; returns when to
    /// be run again if something is held for a time (a partial batch, an editor answer still out), else
    /// `None`: then only the wake runs it.
    pub fn pump(&self, now: Instant) -> Option<Instant> {
        let (events, mut next) = self.state.borrow_mut().tasks.poll(now);
        let hello = self.state.borrow().hello;
        for event in &events {
            if let TaskEvent::Exit {
                kind: ExitKind::SpawnFailed(why),
                ..
            } = event
            {
                eprintln!("[plugin {}] a task failed to start: {why}", self.id());
            }
            if hello {
                self.page.send(&event_message(event));
            }
        }

        let mut replies = Vec::new();
        self.take_checks(now, &mut replies);

        let calls = std::mem::take(&mut self.state.borrow_mut().calls);
        let layout = self.state.borrow().layout.clone();
        let current = self.state.borrow().document;
        let mut keep = Vec::new();
        for mut call in calls {
            if call.document != current {
                if let Some(re) = call.re.take() {
                    self.log_dropped_reply(re, "the requesting document was replaced");
                }
                // Asked by a document that has been replaced: no answer, and the keys stay where they are.
                if call.call == Call::Open {
                    continue;
                }
                // An assist call stays tracked until the editor has answered it, silently: the calls behind
                // it are sent only then.
            }
            let answer = call.pending.try_take();
            match answer {
                Some(Ok(value)) => {
                    replies.extend(self.answered(&call, &value));
                    if call.call == Call::Open {
                        if let Some(layout) = &layout {
                            if let Err(why) = layout.focus(FocusTarget::Editor) {
                                eprintln!("[plugin {}] the editor could not take the keys: {why}", self.id());
                            }
                        }
                    }
                }
                Some(Err(e)) => {
                    if let Some(re) = call.re {
                        replies.push(Reply::err(
                            re,
                            ErrorCode::EditorUnavailable,
                            &format!("the editor could not be asked: {e}"),
                        ));
                    }
                }
                None if now >= call.deadline => {
                    if let Some(re) = call.re.take() {
                        replies.push(Reply::err(
                            re,
                            ErrorCode::EditorTimeout,
                            "the editor did not answer within 5 s; it may be waiting for a key (press Esc and try again)",
                        ));
                    }
                    // The request is still on its way to nvim and may run later. An assist call is kept until
                    // it is answered, so the clear behind it can never reach nvim first and be undone by it.
                    if call.call != Call::Open {
                        keep.push(call);
                    }
                }
                None => keep.push(call),
            }
        }
        self.state.borrow_mut().calls.extend(keep);
        self.send_parked(now, &mut replies);
        {
            let st = self.state.borrow();
            for call in &st.calls {
                let at = now
                    + if now >= call.deadline {
                        EDITOR_LATE_POLL
                    } else {
                        EDITOR_POLL
                    };
                next = Some(next.map_or(at, |n| n.min(at)));
            }
            // A check's answer raises the wake; the timer is only for its deadline.
            let deadlines = st.checking.iter().map(|c| c.deadline).chain(
                st.parked
                    .iter()
                    .filter(|p| matches!(p.op, AssistOp::Checking { .. }))
                    .map(|p| p.deadline),
            );
            for at in deadlines {
                next = Some(next.map_or(at, |n| n.min(at)));
            }
        }

        let results = self
            .state
            .borrow()
            .store
            .as_ref()
            .map(StoreWorker::try_results)
            .unwrap_or_default();
        for (ticket, result) in results {
            let waiting = self.state.borrow_mut().store_waiting.remove(&ticket);
            // The work itself was done (a write stays written); only the answer to a replaced document is dropped.
            if let Some((document, re)) = waiting {
                if document == current {
                    replies.push(store_reply(re, result));
                } else {
                    self.log_dropped_reply(re, "the requesting document was replaced");
                }
            }
        }
        for reply in replies {
            self.reply(reply);
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
        let (current, closed) = {
            let st = self.state.borrow();
            (st.document, st.closed)
        };
        let drop_reason = if closed {
            "the window is closing"
        } else {
            "the requesting document was replaced"
        };
        for (ticket, checked) in results {
            let waiting = {
                let mut st = self.state.borrow_mut();
                let at = st.checking.iter().position(|c| c.ticket == ticket);
                at.map(|i| st.checking.remove(i))
            };
            if let Some(c) = waiting {
                // The document that asked is gone (a reload, a stop) or the window is closing: nothing is
                // opened or started on its behalf.
                if c.document != current || closed {
                    self.log_dropped_reply(c.re, drop_reason);
                    continue;
                }
                match (c.then, checked) {
                    (Then::Open { raw, .. }, Checked::Path(Err(refusal))) => {
                        replies.push(path_refusal(c.re, refusal, &raw))
                    }
                    (Then::Open { line, col, .. }, Checked::Path(Ok(path))) => {
                        self.open_checked(c.re, &path, line, col, c.deadline)
                    }
                    (Then::Task { task, vars }, Checked::Task(resolved)) => {
                        replies.push(self.task_start(c.re, &task, &vars, resolved))
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
                .position(|p| matches!(p.op, AssistOp::Checking { ticket: t, .. } if t == ticket));
            let Some(i) = at else { continue };
            match result {
                Ok(root) => {
                    let parked = &mut st.parked[i];
                    if let AssistOp::Checking {
                        diagnostics,
                        inlay_hints,
                        ..
                    } = parked.op
                    {
                        parked.op = AssistOp::Apply {
                            root,
                            diagnostics,
                            inlay_hints,
                        };
                    }
                }
                Err(refusal) => {
                    let Some(parked) = st.parked.remove(i) else { continue };
                    if let (Some(re), AssistOp::Checking { raw, .. }) = (parked.re, &parked.op) {
                        if parked.document == current {
                            replies.push(path_refusal(re, refusal, raw));
                        } else {
                            self.log_dropped_reply(re, drop_reason);
                        }
                    }
                }
            }
        }

        // A check past its deadline is given up: whatever it finds later is dropped, and nothing is opened,
        // started or applied for it.
        let mut st = self.state.borrow_mut();
        let mut expired = Vec::new();
        st.checking.retain(|c| {
            if now < c.deadline {
                return true;
            }
            if c.document == current && !closed {
                expired.push(c.re);
            } else {
                self.log_dropped_reply(c.re, drop_reason);
            }
            false
        });
        st.parked.retain(|p| {
            if !matches!(p.op, AssistOp::Checking { .. }) || now < p.deadline {
                return true;
            }
            if let Some(re) = p.re {
                if p.document == current {
                    expired.push(re);
                } else {
                    self.log_dropped_reply(re, drop_reason);
                }
            }
            false
        });
        replies.extend(
            expired
                .into_iter()
                .map(|re| Reply::err(re, ErrorCode::Io, CHECK_TIMEOUT)),
        );
    }

    /// Sends the assist call at the head of the line once none is outstanding, and tells the page whose call
    /// has waited out its deadline that the editor is busy (the call itself still goes out).
    fn send_parked(&self, now: Instant, replies: &mut Vec<Reply>) {
        let current = self.state.borrow().document;
        {
            let mut st = self.state.borrow_mut();
            for parked in st.parked.iter_mut() {
                if now < parked.deadline {
                    continue;
                }
                if let Some(re) = parked.re.take() {
                    if parked.document == current {
                        replies.push(Reply::err(
                            re,
                            ErrorCode::EditorTimeout,
                            "the editor did not answer within 5 s; it may be waiting for a key (press Esc and try again)",
                        ));
                    } else {
                        self.log_dropped_reply(re, "the requesting document was replaced");
                    }
                }
            }
        }
        loop {
            let next = {
                let mut st = self.state.borrow_mut();
                if st.calls.iter().any(|c| matches!(c.call, Call::Assist | Call::Clear)) {
                    return;
                }
                if matches!(st.parked.front().map(|p| &p.op), Some(AssistOp::Checking { .. })) {
                    return;
                }
                st.parked.pop_front()
            };
            let Some(parked) = next else { return };
            let re = parked.re.filter(|_| parked.document == current);
            if let Some(re) = parked.re.filter(|_| parked.document != current) {
                self.log_dropped_reply(re, "the requesting document was replaced");
            }
            if let Some(reply) = self.send_assist(re, parked.op, parked.deadline) {
                replies.push(reply);
            }
        }
    }

    /// The plugin's module was closed (prefix `x`): every run is stopped and the assist is put back. The page
    /// is gone, so nothing is answered and nothing more is sent until a new page says hello.
    ///
    /// The clear waits for an assist call still out, so it can never overtake it; the returned time is when
    /// `pump` must run for that, as it does after any other call that leaves an editor answer outstanding.
    pub fn stop(&self, now: Instant) -> Option<Instant> {
        self.new_document();
        let assisted = {
            let mut st = self.state.borrow_mut();
            st.tasks.stop_all();
            st.assisted
        };
        if assisted {
            let _ = self.editor_clear(None, now);
        }
        let waiting = {
            let st = self.state.borrow();
            !st.calls.is_empty() || !st.parked.is_empty() || !st.checking.is_empty()
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
        let s = Scratch::new("host-keys");
        let (dir, _) = demo_plugin(&s.0);
        std::fs::write(
            dir.join("eitri-plugin.toml"),
            "id = \"demo\"\ntitle = \"Demo\"\nprotocol = 1\n\
             site = \"build/site\"\nentry = \"index.html\"\ncapabilities = []\n",
        )
        .unwrap();
        let plugin = Rc::new(load_plugin(dir.to_str().unwrap(), &BTreeMap::new(), None).unwrap());
        let page = Rc::new(Page::default());
        let host = PluginHost::new(plugin, page.clone(), Arc::new(|| {}), None, overrides);
        Rig { _s: s, host, page }
    }

    impl Rig {
        fn send(&self, id: u64, verb: &str, args: Value) {
            let text = json!({"v": 1, "id": id, "verb": verb, "args": args}).to_string();
            self.host.page_message(&text, Instant::now());
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
        r.host.page_loading();
        assert!(r.host.declared_rows().is_none(), "a reloaded page declares again");
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
    fn the_old_document_declaring_while_the_new_one_loads_leaves_no_table() {
        let r = rig(vec![]);
        r.hello();
        assert_eq!(r.declare(json!([row("item.next", &["]q"])]))["ok"], true);
        r.host.page_loading();
        // The old document still runs script until the new one commits.
        assert_eq!(r.declare(json!([row("old.row", &["x"])]))["ok"], false);
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
        r.host.page_loading();
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
        r.host.page_loading();
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
