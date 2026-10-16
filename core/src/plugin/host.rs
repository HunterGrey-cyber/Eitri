//! One plugin's side of the page bridge: every verb a page sends, checked against the capabilities its
//! manifest declares and answered through one funnel to the page. Nothing here touches a toolkit or waits:
//! the page, the layout and the editor are reached through small traits the window implements, and task
//! output, editor answers and store answers are taken by `pump`, which the window runs when the host's
//! wake is raised or when `pump` asked for a timer.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::editor::{self, decode_apply, decode_clear};
use super::load::{LoadedPlugin, TaskRefusal};
use super::protocol::{
    self, ErrorCode, ExitStatus, FocusTarget, Inbound, OnOff, Reply, StreamName, TaskExit, TaskOutput, Verb,
};
use super::roots::{self, PathRefusal};
use super::state::{PluginStore, StoreError, StoreErrorKind, StoreOp, StoreWorker};
use super::tasks::{ExitKind, Launch, RunInfo, RunsWaiter, StartError, Stream, TaskEvent, TaskRunner};
use crate::editor_rpc::EditorRpc;
use crate::nvim_rpc::Pending;

/// How long an editor call may take before the page is told the editor is busy.
pub const EDITOR_DEADLINE: Duration = Duration::from_secs(5);
/// How often a pending editor answer is looked at, by a one-shot timer, while one is outstanding.
pub const EDITOR_POLL: Duration = Duration::from_millis(20);

/// The page. `send` is asynchronous and never calls back into the host before it returns.
pub trait PagePort {
    fn send(&self, message: &str);
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
    Apply {
        root: PathBuf,
        diagnostics: bool,
        inlay_hints: bool,
    },
    Clear,
}

struct PluginHostState {
    /// The page said `host.hello` since it last started loading: events reach it from then on.
    hello: bool,
    /// The window is closing: the page may still be alive for a moment, but nothing it sends is acted on, so
    /// no run can start after the shutdown's list of runs to wait for was taken.
    closed: bool,
    /// Counts documents: raised by every reload and by a stop. A new document numbers its calls from 1 again,
    /// so an answer still out for an older one must never be sent under that id.
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
}

pub struct PluginHost {
    plugin: Rc<LoadedPlugin>,
    page: Rc<dyn PagePort>,
    wake: agent::UiWake,
    store: Option<PluginStore>,
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
    ) -> PluginHost {
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
        };
        PluginHost {
            plugin,
            page,
            wake,
            store,
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

    /// A new document started loading (a reload, a crash's recovery, a navigation): it must say hello again,
    /// and nothing still out for the old one is answered.
    pub fn page_loading(&self) {
        self.new_document();
    }

    fn new_document(&self) {
        let mut st = self.state.borrow_mut();
        st.hello = false;
        st.document += 1;
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
            Verb::HostHello(_) => Some(self.hello(re)),
            Verb::TaskStart(args) => Some(self.task_start(re, &args.task, &args.vars)),
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
            }),
        )
    }

    fn task_start(&self, re: u64, task: &str, vars: &BTreeMap<String, String>) -> Reply {
        let resolved = self.plugin.resolve_task(task, vars);
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
        let path = match roots::file_inside(&self.plugin.roots, raw) {
            Ok(path) => path,
            Err(refusal) => return Some(path_refusal(re, refusal, raw)),
        };
        let Some(editor) = self.editor() else {
            return Some(Reply::err(
                re,
                ErrorCode::EditorUnavailable,
                "this window has no editor running",
            ));
        };
        let pending = editor::open(&*editor, &path, line, col);
        self.push_call(Some(re), Call::Open, pending, now + EDITOR_DEADLINE);
        None
    }

    fn editor_assist(&self, re: u64, raw: &str, diagnostics: OnOff, inlay_hints: OnOff, now: Instant) -> Option<Reply> {
        let root = match roots::dir_inside(&self.plugin.roots, raw) {
            Ok(root) => root,
            Err(refusal) => return Some(path_refusal(re, refusal, raw)),
        };
        if self.editor().is_none() {
            return Some(Reply::err(
                re,
                ErrorCode::EditorUnavailable,
                "this window has no editor running",
            ));
        }
        // Marked now, not on the answer: a stop before the answer still clears it.
        self.state.borrow_mut().assisted = true;
        self.assist_call(
            Some(re),
            AssistOp::Apply {
                root,
                diagnostics: diagnostics == OnOff::On,
                inlay_hints: inlay_hints == OnOff::On,
            },
            now,
        );
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

        let calls = std::mem::take(&mut self.state.borrow_mut().calls);
        let layout = self.state.borrow().layout.clone();
        let current = self.state.borrow().document;
        let mut keep = Vec::new();
        let mut replies = Vec::new();
        for mut call in calls {
            if call.document != current {
                // Asked by a document that has been replaced: no answer, and the keys stay where they are.
                if call.call == Call::Open {
                    continue;
                }
                // An assist call stays tracked until the editor has answered it, silently: the calls behind
                // it are sent only then.
                call.re = None;
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
                    if let Some(re) = call.re {
                        replies.push(Reply::err(
                            re,
                            ErrorCode::EditorTimeout,
                            "the editor did not answer within 5 s; it may be waiting for a key (press Esc and try again)",
                        ));
                    }
                }
                None => {
                    let at = now + EDITOR_POLL;
                    next = Some(next.map_or(at, |n| n.min(at)));
                    keep.push(call);
                }
            }
        }
        self.state.borrow_mut().calls.extend(keep);
        self.send_parked(now, &mut replies);
        if !self.state.borrow().calls.is_empty() {
            let at = now + EDITOR_POLL;
            next = Some(next.map_or(at, |n| n.min(at)));
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
                }
            }
        }
        for reply in replies {
            self.reply(reply);
        }
        next
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
                st.parked.pop_front()
            };
            let Some(parked) = next else { return };
            let re = parked.re.filter(|_| parked.document == current);
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
            !st.calls.is_empty() || !st.parked.is_empty()
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
