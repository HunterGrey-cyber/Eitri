//! The main-context half: what the worker asks of nvim and what nvim answers.
//!
//! Lives on the GTK thread, so nothing here waits: a call to nvim is a [`Pending`], polled from [`IdeHost::tick`]. The
//! window runs that tick only while [`IdeHost::next_tick`] says one is owed, so an idle window has no timer, and
//! `IDE_TAKE_EVENTS` goes only while a view stands.
//!
//! A view exists from the moment its open is sent. An open in flight is someone's to close, because a close made after
//! it lands after it: calls run in the order they were made, and the close chunk has no deadline. Giving a view up
//! therefore never withdraws the open; it sends the close, and keeps the open's `Pending` to close once more should
//! the two ever run swapped.
//!
//! Closing never approves. A view the person closed leaves its request open for the terminal prompt to decide, and an
//! ending the host sends answers an error or nothing.

use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use rmpv::Value;

use super::lines::LineShape;
use super::mention::{IDE_MENTION_DEFINE, IDE_MENTION_OUTSIDE, IDE_MENTION_REMOVE};
use super::types::{Command, ConnId, DiagnosticsQuery, DiffRequest, Event, RpcId, ToolAnswer};
use super::view::{parse_events, parse_open, send_close, send_open, send_take, Opened, ViewEvent};
use crate::editor_rpc::EditorRpc;
use crate::editor_tools::stamp::{Sequencer, Stamp, EDITOR_BUDGET, STAMP_MARGIN};
use crate::editor_tools::tools::diagnostics::{self, DiagnosticsArgs, Severity, DIAGNOSTICS_LIMIT};
use crate::nvim_rpc::{Pending, RpcError};

/// When the host gives up on one file's diagnostics, inside the CLI's own 500 ms: an error costs that client nothing,
/// while a timeout counts against it.
pub const DIAGNOSTICS_WITH_URI: Duration = Duration::from_millis(400);
/// When the host gives up on every file's diagnostics.
pub const DIAGNOSTICS_ALL: Duration = Duration::from_secs(2);

/// One file's chunk gives up sooner than the host, so nothing arrives after the client was told it did not. A
/// diagnostics chunk changes nothing, so this margin only buys the answer's way back; `STAMP_MARGIN` would leave almost
/// nothing of the budget.
const DIAGNOSTICS_WITH_URI_STAMP: Duration = Duration::from_millis(300);

/// The cadence while an answer is awaited.
const AWAITING_TICK: Duration = Duration::from_millis(10);
/// The cadence while only a person is awaited, and the least time between two takes.
const VIEW_TICK: Duration = Duration::from_millis(100);

const NO_EDITOR: &str = "the editor is not running";
const NOT_IN_TIME: &str = "the editor did not answer in time";
const NOT_READ: &str = "the editor's diagnostics could not be read";
const SHOWN_TOO_LATE: &str = "the editor was too busy to show the diff";
const ONE_VIEW: &str = "a diff of this connection is already open";
const VIEW_CLOSED: &str = "the view was closed";
const EDITOR_RESTARTED: &str = "the editor restarted";

/// `Open` holds no answer: the person's come through the take.
enum Stage {
    Opening { pending: Pending, give_up: Instant },
    Open,
}

struct View {
    id: u64,
    conn: ConnId,
    tab_name: String,
    request: RpcId,
    shape: LineShape,
    /// The editor the open went to, which is the one it landed in whatever the handle reaches by the time it answers.
    target: u64,
    stage: Stage,
}

/// A diagnostics call on its way.
struct Call {
    conn: ConnId,
    id: RpcId,
    named_uri: Option<String>,
    pending: Pending,
    give_up: Instant,
}

/// What nvim may still answer with nobody waiting: an open whose view was forgotten, or a call the client left.
struct Abandoned {
    /// The open's view, which its answer may have made.
    view: Option<u64>,
    /// The call a client left, whose place goes back when this ends.
    left: Option<(ConnId, RpcId)>,
    pending: Pending,
    until: Instant,
}

/// An `openDiff` the person closed the view of. The terminal prompt is deciding; a `close_tab` of its name ends it.
struct OpenRequest {
    conn: ConnId,
    id: RpcId,
    tab_name: String,
}

/// A take on its way, and the editor it went to.
struct Take {
    pending: Pending,
    target: u64,
}

struct Mention {
    socket: PathBuf,
    /// The editor that has `:EitriMention`, if one does yet.
    defined_on: Option<u64>,
}

pub struct IdeHost {
    root: PathBuf,
    sequencers: HashMap<ConnId, Sequencer>,
    calls: Vec<Call>,
    views: Vec<View>,
    next_view: u64,
    abandoned: Vec<Abandoned>,
    open_requests: Vec<OpenRequest>,
    take: Option<Take>,
    last_take: Option<Instant>,
    mention: Option<Mention>,
}

impl IdeHost {
    pub fn new(root: PathBuf) -> IdeHost {
        IdeHost {
            root,
            sequencers: HashMap::new(),
            calls: Vec::new(),
            views: Vec::new(),
            next_view: 1,
            abandoned: Vec::new(),
            open_requests: Vec::new(),
            take: None,
            last_take: None,
            mention: None,
        }
    }

    pub fn on_event(&mut self, event: Event, rpc: &dyn EditorRpc, now: Instant, out: &mut Vec<Command>) {
        match event {
            // The port and the refusals are the wiring's.
            Event::Started(_) => {}
            Event::Diagnostics { conn, id, query } => self.diagnostics(conn, id, query, rpc, now, out),
            Event::OpenDiff { conn, id, diff } => self.open_diff(conn, id, *diff, rpc, now, out),
            Event::CloseTab { conn, id, tab_name } => {
                self.close_and_refuse(rpc, now, out, |of, name| of == conn && name == tab_name);
                out.push(answer(conn, id, ToolAnswer::Empty));
            }
            Event::CloseAll { conn, id } => {
                self.close_and_refuse(rpc, now, out, |of, _| of == conn);
                out.push(answer(conn, id, ToolAnswer::Empty));
            }
            Event::Cancelled { conn, id } => {
                self.close_views(rpc, now, true, |view| view.conn == conn && view.request == id);
                self.leave_calls(|call| call.conn == conn && call.id == id);
                self.open_requests
                    .retain(|request| request.conn != conn || request.id != id);
                self.release_unless_owed(conn, id, out);
            }
            Event::ClientGone { conn } => {
                let mut left = self.close_views(rpc, now, true, |view| view.conn == conn);
                self.leave_calls(|call| call.conn == conn);
                let behind_views = self.open_requests.extract_if(.., |request| request.conn == conn);
                left.extend(behind_views.map(|request| (request.conn, request.id)));
                for (conn, id) in left {
                    self.release_unless_owed(conn, id, out);
                }
                self.sequencers.remove(&conn);
            }
            // No worker is left to take an answer.
            Event::Lost => {
                self.forget_all_requests(rpc, now);
                self.remove_mention(rpc);
            }
            Event::MentionUp { socket } => {
                self.mention = Some(Mention {
                    socket,
                    defined_on: None,
                });
                self.define_mention(rpc);
            }
            Event::MentionDown => self.remove_mention(rpc),
            Event::MentionOutside => {
                rpc.exec_lua(IDE_MENTION_OUTSIDE, vec![]);
            }
        }
    }

    /// Whatever has answered, and what the passing of time ends. Opens are read before the take, so an event of a view
    /// that has just opened finds it known.
    pub fn tick(&mut self, rpc: &dyn EditorRpc, now: Instant, out: &mut Vec<Command>) {
        self.poll_calls(now, out);
        self.poll_opening(rpc, now, out);
        self.poll_abandoned(rpc, now, out);
        self.check_target(rpc, out);
        self.poll_take(out);
        self.send_take_when_due(rpc, now);
    }

    /// A changed editor: views on the old one answer an error, `:EitriMention` is defined on the new one. `tick` calls
    /// it; so does a context update, since a restarted nvim's feed reports at load and an idle host has no timer.
    pub fn check_target(&mut self, rpc: &dyn EditorRpc, out: &mut Vec<Command>) {
        let target = rpc.target();
        // The view's Lua state went with its editor, so there is nothing to close.
        let gone = |view: &mut View| matches!(view.stage, Stage::Open) && Some(view.target) != target;
        for view in self.views.extract_if(.., gone) {
            out.push(refusal(view.conn, view.request, EDITOR_RESTARTED));
        }
        // An old editor may never answer this, and it would hold up every take of the new one.
        if self.take.as_ref().is_some_and(|take| Some(take.target) != target) {
            self.take = None;
        }
        self.define_mention(rpc);
    }

    /// The proposal views open in the editor, or on their way there: each ends with the window.
    pub fn open_views(&self) -> usize {
        self.views.len()
    }

    /// How soon `tick` is owed: `None` with nothing outstanding and no view open, so an idle window has no timer.
    pub fn next_tick(&self) -> Option<Duration> {
        let opening = self
            .views
            .iter()
            .any(|view| matches!(view.stage, Stage::Opening { .. }));
        if !self.calls.is_empty() || opening || !self.abandoned.is_empty() {
            Some(AWAITING_TICK)
        } else if !self.views.is_empty() {
            Some(VIEW_TICK)
        } else {
            None
        }
    }

    /// The window's end: every view closes, an opening one too, and nothing is answered.
    pub fn shutdown(&mut self, rpc: &dyn EditorRpc) {
        self.forget_all_requests(rpc, Instant::now());
    }

    fn leave_calls(&mut self, of: impl Fn(&Call) -> bool) {
        for call in self.calls.extract_if(.., |call| of(call)) {
            self.abandoned.push(Abandoned {
                view: None,
                left: Some((call.conn, call.id)),
                pending: call.pending,
                until: call.give_up,
            });
        }
    }

    /// A call the client left gives its place back now, unless nvim has yet to answer it: then the poll that sees the
    /// answer, or gives up on it, does.
    fn release_unless_owed(&self, conn: ConnId, id: RpcId, out: &mut Vec<Command>) {
        let owed = |open: &Abandoned| open.left.as_ref().is_some_and(|(of, call)| *of == conn && *call == id);
        if !self.abandoned.iter().any(owed) {
            out.push(Command::Released { conn, id });
        }
    }

    fn stamp(&mut self, conn: ConnId, budget: Duration) -> Stamp {
        self.sequencers
            .entry(conn)
            .or_insert_with(|| Sequencer::new(format!("ide-{conn}")))
            .stamp(budget)
    }

    fn diagnostics(
        &mut self,
        conn: ConnId,
        id: RpcId,
        query: DiagnosticsQuery,
        rpc: &dyn EditorRpc,
        now: Instant,
        out: &mut Vec<Command>,
    ) {
        if rpc.target().is_none() {
            out.push(refusal(conn, id, NO_EDITOR));
            return;
        }
        let (path, named_uri, stamped, give_up) = match query {
            DiagnosticsQuery::One { uri, path } => {
                (Some(path), Some(uri), DIAGNOSTICS_WITH_URI_STAMP, DIAGNOSTICS_WITH_URI)
            }
            DiagnosticsQuery::All => (None, None, DIAGNOSTICS_ALL - STAMP_MARGIN, DIAGNOSTICS_ALL),
        };
        let stamp = self.stamp(conn, stamped);
        let args = DiagnosticsArgs {
            root: self.root.clone(),
            path,
            min_severity: Severity::Hint,
            utf16: true,
            limit: DIAGNOSTICS_LIMIT,
        };
        self.calls.push(Call {
            conn,
            id,
            named_uri,
            pending: diagnostics::send(rpc, &stamp, &args),
            give_up: now + give_up,
        });
    }

    fn open_diff(
        &mut self,
        conn: ConnId,
        request: RpcId,
        diff: DiffRequest,
        rpc: &dyn EditorRpc,
        now: Instant,
        out: &mut Vec<Command>,
    ) {
        let Some(target) = rpc.target() else {
            out.push(refusal(conn, request, NO_EDITOR));
            return;
        };
        if self.views.iter().any(|view| view.conn == conn) {
            out.push(refusal(conn, request, ONE_VIEW));
            return;
        }
        let id = self.next_view;
        self.next_view += 1;
        let tab_name = diff.tab_name.clone();
        let shape = diff.shape;
        let stamp = self.stamp(conn, EDITOR_BUDGET - STAMP_MARGIN);
        let stage = Stage::Opening {
            pending: send_open(rpc, &stamp, id, diff),
            give_up: now + EDITOR_BUDGET,
        };
        self.views.push(View {
            id,
            conn,
            tab_name,
            request,
            shape,
            target,
            stage,
        });
    }

    /// Closes every view `matches` picks, whichever its state, and gives back the requests they held. `left`: the client
    /// left them, so an open still out gives its call's place back when it ends.
    fn close_views(
        &mut self,
        rpc: &dyn EditorRpc,
        now: Instant,
        left: bool,
        matches: impl Fn(&View) -> bool,
    ) -> Vec<(ConnId, RpcId)> {
        let mut requests = Vec::new();
        for view in self.views.extract_if(.., |view| matches(view)) {
            send_close(rpc, view.id);
            if let Stage::Opening { pending, .. } = view.stage {
                self.abandoned.push(Abandoned {
                    view: Some(view.id),
                    left: left.then(|| (view.conn, view.request.clone())),
                    pending,
                    until: now + EDITOR_BUDGET,
                });
            }
            requests.push((view.conn, view.request));
        }
        requests
    }

    /// `close_tab` and `closeAllDiffTabs`: the views they name close, and each request held by one of them, or still
    /// open behind one the person closed, answers an error. The client has settled its prompt by now.
    fn close_and_refuse(
        &mut self,
        rpc: &dyn EditorRpc,
        now: Instant,
        out: &mut Vec<Command>,
        matches: impl Fn(ConnId, &str) -> bool,
    ) {
        for (conn, id) in self.close_views(rpc, now, false, |view| matches(view.conn, &view.tab_name)) {
            out.push(refusal(conn, id, VIEW_CLOSED));
        }
        let behind = |request: &mut OpenRequest| matches(request.conn, &request.tab_name);
        for request in self.open_requests.extract_if(.., behind) {
            out.push(refusal(request.conn, request.id, VIEW_CLOSED));
        }
    }

    fn forget_all_requests(&mut self, rpc: &dyn EditorRpc, now: Instant) {
        self.close_views(rpc, now, false, |_| true);
        self.calls.clear();
        self.open_requests.clear();
    }

    fn define_mention(&mut self, rpc: &dyn EditorRpc) {
        let (Some(mention), Some(target)) = (&mut self.mention, rpc.target()) else {
            return;
        };
        if mention.defined_on != Some(target) {
            let socket = Value::Binary(mention.socket.as_os_str().as_bytes().to_vec());
            rpc.exec_lua(IDE_MENTION_DEFINE, vec![socket]);
            mention.defined_on = Some(target);
        }
    }

    fn remove_mention(&mut self, rpc: &dyn EditorRpc) {
        if self.mention.take().is_some() {
            rpc.exec_lua(IDE_MENTION_REMOVE, vec![]);
        }
    }

    fn poll_calls(&mut self, now: Instant, out: &mut Vec<Command>) {
        for call in std::mem::take(&mut self.calls) {
            let Call {
                conn,
                id,
                named_uri,
                pending,
                give_up,
            } = call;
            match pending.try_take() {
                Some(Ok(value)) => match diagnostics::parse(&value) {
                    Ok(answer) => out.push(Command::Entries {
                        conn,
                        id,
                        named_uri,
                        answer,
                    }),
                    Err(_) => out.push(refusal(conn, id, NOT_READ)),
                },
                Some(Err(error)) => out.push(refusal(conn, id, error.to_string())),
                None if now >= give_up => out.push(refusal(conn, id, NOT_IN_TIME)),
                None => self.calls.push(Call {
                    conn,
                    id,
                    named_uri,
                    pending,
                    give_up,
                }),
            }
        }
    }

    fn poll_opening(&mut self, rpc: &dyn EditorRpc, now: Instant, out: &mut Vec<Command>) {
        let mut failed = Vec::new();
        let mut given_up = Vec::new();
        for view in &mut self.views {
            let Stage::Opening { pending, give_up } = &view.stage else {
                continue;
            };
            match pending.try_take().map(open_outcome) {
                Some(Ok(())) => view.stage = Stage::Open,
                Some(Err(why)) => failed.push((view.id, why)),
                None if now >= *give_up => given_up.push(view.id),
                None => {}
            }
        }
        for (id, why) in failed {
            if let Some(view) = self.take_view(id) {
                out.push(refusal(view.conn, view.request, why));
            }
        }
        for (conn, id) in self.close_views(rpc, now, false, |view| given_up.contains(&view.id)) {
            out.push(refusal(conn, id, NOT_IN_TIME));
        }
    }

    /// The close sent when a view was forgotten lands after its open, except in the embedded editor's rare swap of two
    /// calls, which would leave a view nobody holds: an open that answers `Opened` here gets a second close. Past the
    /// budget its answer is not coming. Either way nvim owes it nothing now, so the place of a call the client left goes
    /// back.
    fn poll_abandoned(&mut self, rpc: &dyn EditorRpc, now: Instant, out: &mut Vec<Command>) {
        self.abandoned.retain(|open| {
            let reply = open.pending.try_take();
            if reply.is_none() && now < open.until {
                return true;
            }
            if let (Some(view), Some(Ok(()))) = (open.view, reply.map(open_outcome)) {
                send_close(rpc, view);
            }
            if let Some((conn, id)) = open.left.clone() {
                out.push(Command::Released { conn, id });
            }
            false
        });
    }

    fn take_view(&mut self, id: u64) -> Option<View> {
        let at = self.views.iter().position(|view| view.id == id)?;
        Some(self.views.remove(at))
    }

    fn poll_take(&mut self, out: &mut Vec<Command>) {
        let Some(reply) = self.take.as_ref().and_then(|take| take.pending.try_take()) else {
            return;
        };
        self.take = None;
        // A failed call asks again at the next poll. Answers that cannot be read never come from the constant chunk.
        let Ok(value) = reply else {
            return;
        };
        match parse_events(value) {
            Ok(events) => {
                for event in events {
                    self.on_view_event(event, out);
                }
            }
            Err(why) => eprintln!("[ide-server] a view's events were not read: {why}"),
        }
    }

    /// The view is forgotten before anything goes out, so the next edit's `openDiff` is not refused as a second view.
    /// An event of a view no longer known is dropped: its request has already been settled.
    fn on_view_event(&mut self, event: ViewEvent, out: &mut Vec<Command>) {
        match event {
            ViewEvent::Accept { view, lines } => {
                if let Some(view) = self.take_view(view) {
                    out.push(Command::Saved {
                        conn: view.conn,
                        id: view.request,
                        lines,
                        shape: view.shape,
                    });
                }
            }
            ViewEvent::Reject { view } => {
                if let Some(view) = self.take_view(view) {
                    out.push(answer(view.conn, view.request, ToolAnswer::DiffRejected));
                }
            }
            ViewEvent::Closed { view } => {
                if let Some(view) = self.take_view(view) {
                    self.open_requests.push(OpenRequest {
                        conn: view.conn,
                        id: view.request,
                        tab_name: view.tab_name,
                    });
                }
            }
        }
    }

    fn send_take_when_due(&mut self, rpc: &dyn EditorRpc, now: Instant) {
        let due = self.last_take.is_none_or(|last| now.duration_since(last) >= VIEW_TICK);
        let view_open = self.views.iter().any(|view| matches!(view.stage, Stage::Open));
        if !due || !view_open || self.take.is_some() {
            return;
        }
        if let Some(target) = rpc.target() {
            self.take = Some(Take {
                pending: send_take(rpc),
                target,
            });
            self.last_take = Some(now);
        }
    }
}

fn answer(conn: ConnId, id: RpcId, answer: ToolAnswer) -> Command {
    Command::Answer { conn, id, answer }
}

fn refusal(conn: ConnId, id: RpcId, why: impl Into<String>) -> Command {
    answer(conn, id, ToolAnswer::Error(why.into()))
}

/// `Ok` for a view that stands now, otherwise the sentence for the client.
fn open_outcome(reply: Result<Value, RpcError>) -> Result<(), String> {
    match parse_open(&reply.map_err(|error| error.to_string())?)? {
        Opened::Opened => Ok(()),
        Opened::Refused(why) => Err(why),
        Opened::Expired => Err(SHOWN_TOO_LATE.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use rmpv::Value;

    use super::*;
    use crate::editor_tools::stamp::{wall_ms, STAMP_MARGIN};
    use crate::editor_tools::tools::diagnostics::DIAGNOSTICS_LUA;
    use crate::ide_server::mention::{IDE_MENTION_DEFINE, IDE_MENTION_OUTSIDE, IDE_MENTION_REMOVE};
    use crate::ide_server::view::{IDE_DIFF_CLOSE, IDE_DIFF_OPEN, IDE_TAKE_EVENTS};
    use crate::nvim_rpc::{Answer, Pending};

    const TO_SHOW: &str = "/project/src/x.rs";

    /// Records every call and holds its answer until a test gives it.
    struct Editor {
        target: Cell<Option<u64>>,
        calls: RefCell<Vec<(&'static str, Vec<Value>)>>,
        answers: RefCell<Vec<Option<Answer>>>,
    }

    impl Editor {
        fn running(target: Option<u64>) -> Editor {
            Editor {
                target: Cell::new(target),
                calls: RefCell::new(Vec::new()),
                answers: RefCell::new(Vec::new()),
            }
        }

        fn names(&self) -> Vec<&'static str> {
            self.calls.borrow().iter().map(|(code, _)| name_of(code)).collect()
        }

        fn count(&self, name: &str) -> usize {
            self.names().iter().filter(|called| **called == name).count()
        }

        /// The first call of that name still unanswered.
        fn answer(&self, name: &str, reply: Result<Value, RpcError>) {
            let names = self.names();
            let mut answers = self.answers.borrow_mut();
            let at = (0..names.len())
                .find(|at| names[*at] == name && answers[*at].is_some())
                .unwrap_or_else(|| panic!("no {name} is out: {names:?}"));
            answers[at].take().expect("just found").send(reply);
        }

        fn args(&self, name: &str) -> Vec<Vec<Value>> {
            self.calls
                .borrow()
                .iter()
                .filter(|(code, _)| name_of(code) == name)
                .map(|(_, args)| args.clone())
                .collect()
        }
    }

    impl EditorRpc for Editor {
        fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending {
            let (answer, pending) = Pending::pair();
            self.calls.borrow_mut().push((code, args));
            self.answers.borrow_mut().push(Some(answer));
            pending
        }

        fn target(&self) -> Option<u64> {
            self.target.get()
        }
    }

    fn name_of(code: &str) -> &'static str {
        let chunks = [
            ("open", IDE_DIFF_OPEN),
            ("close", IDE_DIFF_CLOSE),
            ("take", IDE_TAKE_EVENTS),
            ("diagnostics", DIAGNOSTICS_LUA),
            ("define", IDE_MENTION_DEFINE),
            ("remove", IDE_MENTION_REMOVE),
            ("outside", IDE_MENTION_OUTSIDE),
        ];
        chunks
            .iter()
            .find(|(_, chunk)| *chunk == code)
            .map(|(name, _)| *name)
            .expect("a chunk the host sends")
    }

    fn map(fields: Vec<(&str, Value)>) -> Value {
        Value::Map(
            fields
                .into_iter()
                .map(|(name, value)| (Value::from(name), value))
                .collect(),
        )
    }

    fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
        value
            .as_map()
            .expect("a map")
            .iter()
            .find(|(key, _)| key.as_str() == Some(name))
            .map(|(_, value)| value)
            .unwrap_or_else(|| panic!("no field {name}"))
    }

    fn opened() -> Result<Value, RpcError> {
        Ok(map(vec![("opened", Value::from(true))]))
    }

    fn view_event(view: u64, kind: &str) -> Value {
        let mut fields = vec![("view", Value::from(view)), ("kind", Value::from(kind))];
        if kind == "accept" {
            fields.push(("lines", Value::Array(vec![Value::Binary(b"kept".to_vec())])));
        }
        map(fields)
    }

    fn events(events: Vec<Value>) -> Result<Value, RpcError> {
        Ok(Value::Array(events))
    }

    fn request(number: i64) -> RpcId {
        RpcId::Num(number)
    }

    fn open_diff(conn: ConnId, id: i64, tab: &str) -> Event {
        Event::OpenDiff {
            conn,
            id: request(id),
            diff: Box::new(DiffRequest {
                tab_name: tab.to_owned(),
                real_path: PathBuf::from(TO_SHOW),
                rel: "src/x.rs".to_owned(),
                file_text: Vec::new(),
                file_lines: Vec::new(),
                proposal_lines: vec![Value::Binary(b"kept".to_vec())],
                shape: LineShape { crlf: true, eol: false },
            }),
        }
    }

    fn all_diagnostics(conn: ConnId, id: i64) -> Event {
        Event::Diagnostics {
            conn,
            id: request(id),
            query: DiagnosticsQuery::All,
        }
    }

    fn one_file(conn: ConnId, id: i64) -> Event {
        Event::Diagnostics {
            conn,
            id: request(id),
            query: DiagnosticsQuery::One {
                uri: format!("file://{TO_SHOW}"),
                path: PathBuf::from(TO_SHOW),
            },
        }
    }

    /// A host, its editor and a clock the test moves.
    struct Bench {
        host: IdeHost,
        editor: Editor,
        now: Instant,
        out: Vec<Command>,
    }

    impl Bench {
        fn new() -> Bench {
            Bench::on(Some(1))
        }

        fn on(target: Option<u64>) -> Bench {
            Bench {
                host: IdeHost::new(PathBuf::from("/project")),
                editor: Editor::running(target),
                now: Instant::now(),
                out: Vec::new(),
            }
        }

        fn event(&mut self, event: Event) {
            self.host.on_event(event, &self.editor, self.now, &mut self.out);
        }

        fn tick(&mut self) {
            self.host.tick(&self.editor, self.now, &mut self.out);
        }

        fn after(&mut self, wait: Duration) {
            self.now += wait;
            self.tick();
        }

        /// A view on `conn`, the editor having answered.
        fn open(&mut self, conn: ConnId, id: i64, tab: &str) {
            self.event(open_diff(conn, id, tab));
            self.editor.answer("open", opened());
            self.tick();
            assert!(self.out.is_empty(), "{:?}", self.out);
        }

        /// What went out since the last look, every one an `Answer`.
        fn answers(&mut self) -> Vec<(ConnId, RpcId, ToolAnswer)> {
            self.out
                .drain(..)
                .map(|command| match command {
                    Command::Answer { conn, id, answer } => (conn, id, answer),
                    other => panic!("not an answer: {other:?}"),
                })
                .collect()
        }

        fn one_answer(&mut self) -> (ConnId, RpcId, ToolAnswer) {
            let mut answers = self.answers();
            assert_eq!(answers.len(), 1, "{answers:?}");
            answers.remove(0)
        }
    }

    fn the_error(answer: &ToolAnswer) -> &str {
        match answer {
            ToolAnswer::Error(why) => why,
            other => panic!("not an error: {other:?}"),
        }
    }

    #[test]
    fn get_diagnostics_stamps_inside_the_hosts_budget() {
        let stamp_of = |args: &[Vec<Value>]| {
            let stamp = &args.last().expect("a call")[0];
            field(stamp, "deadline_ms").as_u64().expect("a deadline")
        };
        // The deadline is read against the clock either side of the call, so a stalled test cannot flip the comparison.
        type Sends = fn(&mut Bench);
        let cases: [(Sends, &str, Duration, Duration); 3] = [
            (
                |bench| bench.event(one_file(1, 1)),
                "diagnostics",
                Duration::from_millis(300),
                DIAGNOSTICS_WITH_URI,
            ),
            (
                |bench| bench.event(all_diagnostics(1, 1)),
                "diagnostics",
                DIAGNOSTICS_ALL - STAMP_MARGIN,
                DIAGNOSTICS_ALL,
            ),
            (
                |bench| bench.event(open_diff(1, 1, "tab")),
                "open",
                EDITOR_BUDGET - STAMP_MARGIN,
                EDITOR_BUDGET,
            ),
        ];
        for (send, name, stamped, give_up) in cases {
            let mut bench = Bench::new();
            let before = wall_ms();
            send(&mut bench);
            let after = wall_ms();
            let deadline = stamp_of(&bench.editor.args(name));
            assert!(deadline >= before + stamped.as_millis() as u64, "{name}");
            assert!(deadline <= after + stamped.as_millis() as u64, "{name}");
            assert!(stamped < give_up, "{name}: the chunk gives up first");
            bench.after(give_up - Duration::from_millis(1));
            assert!(bench.out.is_empty(), "{name}: still waiting");
            bench.after(Duration::from_millis(1));
            the_error(&bench.one_answer().2);
        }
    }

    #[test]
    fn no_editor_answers_an_error_at_once() {
        let mut bench = Bench::on(None);
        bench.event(one_file(1, 1));
        assert_eq!(the_error(&bench.one_answer().2), "the editor is not running");
        bench.event(all_diagnostics(1, 2));
        assert_eq!(the_error(&bench.one_answer().2), "the editor is not running");
        bench.event(open_diff(1, 3, "tab"));
        assert_eq!(the_error(&bench.one_answer().2), "the editor is not running");
        assert!(bench.editor.names().is_empty());
        assert_eq!(bench.host.next_tick(), None);
    }

    #[test]
    fn silence_past_400_ms_answers_an_error() {
        let mut bench = Bench::new();
        bench.event(one_file(1, 7));
        bench.after(Duration::from_millis(399));
        assert!(bench.out.is_empty());
        bench.after(Duration::from_millis(1));
        let (conn, id, answer) = bench.one_answer();
        assert_eq!((conn, id), (1, request(7)));
        the_error(&answer);
        bench
            .editor
            .answer("diagnostics", Ok(map(vec![("entries", Value::Array(vec![]))])));
        bench.after(Duration::from_millis(1));
        assert!(bench.out.is_empty(), "a late answer goes nowhere");
        assert_eq!(bench.host.next_tick(), None);
    }

    #[test]
    fn an_expired_answer_is_an_error() {
        let mut bench = Bench::new();
        bench.event(one_file(1, 1));
        bench
            .editor
            .answer("diagnostics", Ok(map(vec![("expired", Value::from(true))])));
        bench.tick();
        the_error(&bench.one_answer().2);
    }

    #[test]
    fn a_failed_call_is_an_error() {
        let mut bench = Bench::new();
        bench.event(all_diagnostics(1, 1));
        bench.editor.answer("diagnostics", Err(RpcError::Closed));
        bench.tick();
        the_error(&bench.one_answer().2);
    }

    #[test]
    fn an_answer_goes_on_as_entries_with_its_uri() {
        let reply = || {
            Ok(map(vec![
                ("entries", Value::Array(vec![])),
                ("modified", Value::Array(vec![])),
                ("left_out", Value::from(3)),
            ]))
        };
        let mut bench = Bench::new();
        bench.event(one_file(4, 9));
        bench.editor.answer("diagnostics", reply());
        bench.tick();
        match bench.out.remove(0) {
            Command::Entries {
                conn,
                id,
                named_uri,
                answer,
            } => {
                assert_eq!((conn, id), (4, request(9)));
                assert_eq!(named_uri, Some(format!("file://{TO_SHOW}")));
                assert_eq!(answer.left_out, 3);
            }
            other => panic!("not entries: {other:?}"),
        }
        bench.event(all_diagnostics(4, 10));
        bench.editor.answer("diagnostics", reply());
        bench.tick();
        assert!(matches!(bench.out.remove(0), Command::Entries { named_uri: None, .. }));
        let args = &bench.editor.args("diagnostics");
        assert_eq!(field(&args[0][1], "limit").as_u64(), Some(u64::from(DIAGNOSTICS_LIMIT)));
        assert_eq!(field(&args[0][1], "utf16"), &Value::from(true));
        assert_eq!(field(&args[0][1], "min_severity").as_str(), Some("hint"));
        assert_eq!(
            field(&args[0][1], "path"),
            &Value::Binary(TO_SHOW.as_bytes().to_vec()),
            "the file asked about"
        );
    }

    #[test]
    fn a_second_open_diff_on_a_connection_answers_an_error() {
        let mut bench = Bench::new();
        bench.open(1, 1, "tab");
        bench.event(open_diff(1, 2, "other"));
        let (conn, id, answer) = bench.one_answer();
        assert_eq!((conn, id), (1, request(2)));
        the_error(&answer);
        assert_eq!(bench.editor.count("open"), 1);
        bench.event(open_diff(2, 3, "tab"));
        assert_eq!(
            bench.editor.count("open"),
            2,
            "another connection is the editor's to refuse"
        );
    }

    #[test]
    fn an_opening_view_counts_as_a_second_view() {
        let mut bench = Bench::new();
        bench.event(open_diff(1, 1, "tab"));
        bench.event(open_diff(1, 2, "other"));
        the_error(&bench.one_answer().2);
        assert_eq!(bench.editor.count("open"), 1);
    }

    #[test]
    fn a_refused_open_answers_with_the_editors_reason_and_leaves_no_view() {
        let mut bench = Bench::new();
        bench.event(open_diff(1, 1, "tab"));
        bench
            .editor
            .answer("open", Ok(map(vec![("refused", Value::from("a diff is open"))])));
        bench.tick();
        assert_eq!(the_error(&bench.one_answer().2), "a diff is open");
        assert_eq!(bench.host.next_tick(), None);
        bench.event(open_diff(1, 2, "tab"));
        assert_eq!(bench.editor.count("open"), 2, "the connection has no view");
        assert!(bench.out.is_empty());
        assert_eq!(bench.editor.count("close"), 0, "nothing stands to close");
    }

    #[test]
    fn an_expired_or_failed_open_answers_an_error_and_closes_nothing() {
        let replies: [Result<Value, RpcError>; 3] = [
            Ok(map(vec![("expired", Value::from(true))])),
            Ok(Value::Nil),
            Err(RpcError::Closed),
        ];
        for reply in replies {
            let mut bench = Bench::new();
            bench.event(open_diff(1, 1, "tab"));
            bench.editor.answer("open", reply);
            bench.tick();
            the_error(&bench.one_answer().2);
            assert_eq!(bench.editor.count("close"), 0);
            assert_eq!(bench.host.next_tick(), None);
        }
    }

    #[test]
    fn take_events_is_not_polled_while_no_view_is_open() {
        let mut bench = Bench::new();
        bench.after(Duration::from_secs(1));
        assert_eq!(bench.editor.count("take"), 0, "before anything");
        bench.event(all_diagnostics(1, 1));
        bench.after(Duration::from_millis(100));
        assert_eq!(bench.editor.count("take"), 0, "with only a call out");
        bench.event(open_diff(1, 2, "tab"));
        bench.after(Duration::from_millis(100));
        assert_eq!(bench.editor.count("take"), 0, "while opening");
        bench.editor.answer("open", opened());
        bench.tick();
        assert_eq!(bench.editor.count("take"), 1, "with a view open");
        bench.editor.answer("take", events(vec![view_event(1, "reject")]));
        bench.after(Duration::from_millis(100));
        bench.after(Duration::from_secs(1));
        assert_eq!(bench.editor.count("take"), 1, "after the view");
    }

    #[test]
    fn a_take_goes_at_most_every_100_ms_and_one_at_a_time() {
        let mut bench = Bench::new();
        bench.open(1, 1, "tab");
        assert_eq!(bench.editor.count("take"), 1);
        bench.after(Duration::from_millis(100));
        assert_eq!(bench.editor.count("take"), 1, "one is still out");
        bench.editor.answer("take", events(vec![]));
        bench.tick();
        assert_eq!(bench.editor.count("take"), 2, "due, and none is out");
        bench.editor.answer("take", events(vec![]));
        bench.after(Duration::from_millis(99));
        assert_eq!(bench.editor.count("take"), 2, "99 ms since the last");
        bench.after(Duration::from_millis(1));
        assert_eq!(bench.editor.count("take"), 3);
    }

    #[test]
    fn next_tick_is_none_when_idle() {
        let mut bench = Bench::new();
        assert_eq!(bench.host.next_tick(), None);
        bench.event(all_diagnostics(1, 1));
        assert_eq!(bench.host.next_tick(), Some(Duration::from_millis(10)), "a call");
        bench.editor.answer("diagnostics", Err(RpcError::Closed));
        bench.tick();
        assert_eq!(bench.host.next_tick(), None);

        bench.out.clear();
        bench.event(open_diff(1, 2, "tab"));
        assert_eq!(
            bench.host.next_tick(),
            Some(Duration::from_millis(10)),
            "an opening view"
        );
        bench.editor.answer("open", opened());
        bench.tick();
        assert_eq!(bench.host.next_tick(), Some(Duration::from_millis(100)), "only a view");
        bench.event(Event::Cancelled {
            conn: 1,
            id: request(2),
        });
        assert_eq!(bench.host.next_tick(), None);

        bench.event(open_diff(1, 3, "tab"));
        bench.event(Event::Cancelled {
            conn: 1,
            id: request(3),
        });
        assert_eq!(
            bench.host.next_tick(),
            Some(Duration::from_millis(10)),
            "an abandoned open"
        );
        bench.after(EDITOR_BUDGET);
        assert_eq!(bench.host.next_tick(), None, "kept for one more budget and no longer");
    }

    #[test]
    fn open_views_counts_a_view_from_its_open_to_its_end() {
        let mut bench = Bench::new();
        assert_eq!(bench.host.open_views(), 0);
        bench.event(open_diff(1, 1, "tab"));
        assert_eq!(bench.host.open_views(), 1, "on its way to the editor");
        bench.editor.answer("open", opened());
        bench.tick();
        assert_eq!(bench.host.open_views(), 1, "shown");
        bench.editor.answer("take", events(vec![view_event(1, "closed")]));
        bench.tick();
        assert_eq!(bench.host.open_views(), 0, "the person closed it");
    }

    #[test]
    fn a_request_behind_a_closed_view_keeps_no_timer() {
        let mut bench = Bench::new();
        bench.open(1, 1, "tab");
        bench.editor.answer("take", events(vec![view_event(1, "closed")]));
        bench.tick();
        assert!(bench.out.is_empty(), "the terminal prompt decides");
        assert_eq!(bench.host.next_tick(), None);
    }

    #[test]
    fn accept_forgets_the_view_before_saved_goes() {
        let mut bench = Bench::new();
        bench.open(3, 8, "tab");
        bench.editor.answer("take", events(vec![view_event(1, "accept")]));
        bench.tick();
        bench.event(open_diff(3, 9, "tab"));
        assert_eq!(
            bench.out.len(),
            1,
            "no refusal of the next edit's view: {:?}",
            bench.out
        );
        match bench.out.remove(0) {
            Command::Saved { conn, id, lines, shape } => {
                assert_eq!((conn, id), (3, request(8)));
                assert_eq!(lines, vec![Value::Binary(b"kept".to_vec())]);
                assert_eq!(shape, LineShape { crlf: true, eol: false });
            }
            other => panic!("not saved: {other:?}"),
        }
        assert_eq!(bench.editor.count("open"), 2);
    }

    #[test]
    fn reject_answers_diff_rejected() {
        let mut bench = Bench::new();
        bench.open(3, 8, "tab");
        bench.editor.answer("take", events(vec![view_event(1, "reject")]));
        bench.tick();
        assert_eq!(bench.one_answer(), (3, request(8), ToolAnswer::DiffRejected));
        assert_eq!(bench.host.next_tick(), None);
    }

    #[test]
    fn the_events_of_a_gone_view_are_dropped() {
        let mut bench = Bench::new();
        bench.open(1, 1, "tab");
        bench.event(Event::Cancelled {
            conn: 1,
            id: request(1),
        });
        only_released(&mut bench, 1, 1);
        bench.editor.answer(
            "take",
            events(vec![
                view_event(1, "accept"),
                view_event(1, "reject"),
                view_event(1, "closed"),
                view_event(99, "accept"),
            ]),
        );
        bench.tick();
        assert!(bench.out.is_empty(), "{:?}", bench.out);
        bench.event(Event::CloseTab {
            conn: 1,
            id: request(2),
            tab_name: "tab".to_owned(),
        });
        assert_eq!(
            bench.one_answer(),
            (1, request(2), ToolAnswer::Empty),
            "a closed event of a gone view leaves no open request behind"
        );
    }

    #[test]
    fn sequences_are_per_connection() {
        let mut bench = Bench::new();
        bench.event(all_diagnostics(1, 1));
        bench.event(all_diagnostics(1, 2));
        bench.event(open_diff(1, 3, "tab"));
        bench.event(all_diagnostics(2, 1));
        let mut stamps: Vec<(String, u64)> = bench
            .editor
            .calls
            .borrow()
            .iter()
            .map(|(_, args)| {
                let scope = field(&args[0], "scope").as_str().expect("a scope").to_owned();
                (scope, field(&args[0], "seq").as_u64().expect("a number"))
            })
            .collect();
        stamps.sort();
        let want: Vec<(String, u64)> = vec![
            ("ide-1".to_owned(), 1),
            ("ide-1".to_owned(), 2),
            ("ide-1".to_owned(), 3),
            ("ide-2".to_owned(), 1),
        ];
        assert_eq!(stamps, want);
    }

    /// `end` during an open, the open's answer held back: the close follows the open, and the late `Opened` is closed
    /// a second time, with nothing said.
    fn closes_a_late_open(end: fn(&mut Bench)) {
        let mut bench = Bench::new();
        bench.event(open_diff(1, 1, "tab"));
        end(&mut bench);
        bench.out.clear();
        assert_eq!(bench.editor.names(), ["open", "close"]);
        bench.after(Duration::from_millis(10));
        assert_eq!(bench.editor.names(), ["open", "close"]);
        bench.editor.answer("open", opened());
        bench.tick();
        assert_eq!(bench.editor.names(), ["open", "close", "close"]);
        let answered = |command: &Command| !matches!(command, Command::Released { .. });
        assert!(!bench.out.iter().any(answered), "{:?}", bench.out);
        assert_eq!(bench.host.next_tick(), None);
    }

    #[test]
    fn close_tab_during_an_open_closes_it() {
        closes_a_late_open(|bench| {
            bench.event(Event::CloseTab {
                conn: 1,
                id: request(2),
                tab_name: "tab".to_owned(),
            });
            let answers = bench.answers();
            assert_eq!(the_error(&answers[0].2), "the view was closed");
            assert_eq!(answers[0].1, request(1));
            assert_eq!(answers[1], (1, request(2), ToolAnswer::Empty));
        });
    }

    #[test]
    fn close_all_during_an_open_closes_it() {
        closes_a_late_open(|bench| {
            bench.event(Event::CloseAll {
                conn: 1,
                id: request(2),
            });
            let answers = bench.answers();
            assert_eq!(the_error(&answers[0].2), "the view was closed");
            assert_eq!(answers[1], (1, request(2), ToolAnswer::Empty));
        });
    }

    #[test]
    fn cancelled_during_an_open_closes_it_and_answers_nothing() {
        closes_a_late_open(|bench| {
            bench.event(Event::Cancelled {
                conn: 1,
                id: request(1),
            });
            assert!(bench.out.is_empty());
        });
    }

    #[test]
    fn a_connection_gone_during_an_open_closes_it() {
        closes_a_late_open(|bench| {
            bench.event(Event::ClientGone { conn: 1 });
            assert!(bench.out.is_empty());
        });
    }

    #[test]
    fn shutdown_during_an_open_closes_it() {
        closes_a_late_open(|bench| {
            bench.host.shutdown(&bench.editor);
            assert!(bench.out.is_empty());
        });
    }

    #[test]
    fn the_budget_passing_closes_a_late_open() {
        closes_a_late_open(|bench| {
            bench.after(EDITOR_BUDGET);
            the_error(&bench.one_answer().2);
        });
    }

    #[test]
    fn a_close_of_another_connections_name_touches_nothing() {
        let mut bench = Bench::new();
        bench.open(2, 1, "tab");
        bench.event(Event::CloseTab {
            conn: 1,
            id: request(2),
            tab_name: "tab".to_owned(),
        });
        assert_eq!(bench.one_answer(), (1, request(2), ToolAnswer::Empty));
        assert_eq!(bench.editor.count("close"), 0);
    }

    #[test]
    fn a_request_behind_a_closed_view_is_answered_by_close_tab_and_close_all() {
        let mut bench = Bench::new();
        bench.open(1, 1, "first");
        bench.editor.answer("take", events(vec![view_event(1, "closed")]));
        bench.tick();
        bench.event(Event::CloseTab {
            conn: 2,
            id: request(5),
            tab_name: "first".to_owned(),
        });
        assert_eq!(
            bench.one_answer(),
            (2, request(5), ToolAnswer::Empty),
            "a request behind a view is its own connection's"
        );
        bench.event(Event::CloseTab {
            conn: 1,
            id: request(6),
            tab_name: "first".to_owned(),
        });
        let answers = bench.answers();
        assert_eq!(answers.len(), 2, "{answers:?}");
        assert_eq!((answers[0].0, answers[0].1.clone()), (1, request(1)));
        assert_eq!(the_error(&answers[0].2), "the view was closed");
        assert_eq!(answers[1], (1, request(6), ToolAnswer::Empty));
        assert_eq!(bench.editor.count("close"), 0, "the view had closed itself");

        bench.now += VIEW_TICK;
        bench.open(1, 7, "second");
        bench.editor.answer("take", events(vec![view_event(2, "closed")]));
        bench.tick();
        bench.event(Event::CloseAll {
            conn: 1,
            id: request(8),
        });
        let answers = bench.answers();
        assert_eq!((answers[0].0, answers[0].1.clone()), (1, request(7)));
        assert_eq!(the_error(&answers[0].2), "the view was closed");
        assert_eq!(answers[1], (1, request(8), ToolAnswer::Empty));
    }

    #[test]
    fn a_withdrawn_request_behind_a_closed_view_answers_nothing_later() {
        for leave in [
            Event::Cancelled {
                conn: 1,
                id: request(1),
            },
            Event::ClientGone { conn: 1 },
        ] {
            let mut bench = Bench::new();
            bench.open(1, 1, "tab");
            bench.editor.answer("take", events(vec![view_event(1, "closed")]));
            bench.tick();
            bench.event(leave);
            only_released(&mut bench, 1, 1);
            bench.event(Event::CloseTab {
                conn: 1,
                id: request(2),
                tab_name: "tab".to_owned(),
            });
            assert_eq!(bench.one_answer(), (1, request(2), ToolAnswer::Empty));
        }
    }

    fn released(conn: ConnId, id: i64) -> Command {
        Command::Released { conn, id: request(id) }
    }

    /// What went out since the last look, as text: a `Command` holds no `PartialEq`.
    fn seen(bench: &mut Bench) -> Vec<String> {
        bench.out.drain(..).map(|command| format!("{command:?}")).collect()
    }

    fn only_released(bench: &mut Bench, conn: ConnId, id: i64) {
        assert_eq!(seen(bench), vec![format!("{:?}", released(conn, id))]);
    }

    fn cancelled(id: i64) -> Event {
        Event::Cancelled {
            conn: 1,
            id: request(id),
        }
    }

    #[test]
    fn a_call_left_with_nvim_is_released_when_nvim_answers() {
        for leave in [cancelled(1), Event::ClientGone { conn: 1 }] {
            let mut bench = Bench::new();
            bench.event(all_diagnostics(1, 1));
            bench.event(leave);
            bench.tick();
            assert!(bench.out.is_empty(), "nvim still owes it: {:?}", bench.out);
            assert_eq!(bench.host.next_tick(), Some(Duration::from_millis(10)));
            bench.editor.answer("diagnostics", Err(RpcError::Closed));
            bench.tick();
            only_released(&mut bench, 1, 1);
            assert_eq!(bench.host.next_tick(), None);
        }
    }

    #[test]
    fn a_call_left_with_nvim_is_released_when_the_host_gives_up() {
        for leave in [cancelled(1), Event::ClientGone { conn: 1 }] {
            let mut bench = Bench::new();
            bench.event(all_diagnostics(1, 1));
            bench.event(leave);
            bench.after(DIAGNOSTICS_ALL);
            only_released(&mut bench, 1, 1);
            assert_eq!(bench.host.next_tick(), None);
        }
    }

    #[test]
    fn an_open_left_with_nvim_is_released_when_it_answers_or_its_time_runs_out() {
        for leave in [cancelled(1), Event::ClientGone { conn: 1 }] {
            let mut bench = Bench::new();
            bench.event(open_diff(1, 1, "tab"));
            bench.event(leave);
            bench.tick();
            assert!(bench.out.is_empty(), "nvim still owes it: {:?}", bench.out);
            bench.editor.answer("open", opened());
            bench.tick();
            only_released(&mut bench, 1, 1);
        }
        for leave in [cancelled(1), Event::ClientGone { conn: 1 }] {
            let mut bench = Bench::new();
            bench.event(open_diff(1, 1, "tab"));
            bench.event(leave);
            bench.after(EDITOR_BUDGET);
            only_released(&mut bench, 1, 1);
        }
    }

    #[test]
    fn what_nvim_owes_nothing_for_is_released_at_once() {
        for leave in [cancelled(1), Event::ClientGone { conn: 1 }] {
            let mut bench = Bench::new();
            bench.open(1, 1, "tab");
            bench.event(leave);
            only_released(&mut bench, 1, 1);
        }
        for leave in [cancelled(1), Event::ClientGone { conn: 1 }] {
            let mut bench = Bench::new();
            bench.open(1, 1, "tab");
            bench.editor.answer("take", events(vec![view_event(1, "closed")]));
            bench.tick();
            bench.event(leave);
            only_released(&mut bench, 1, 1);
        }
        let mut bench = Bench::new();
        bench.event(cancelled(1));
        only_released(&mut bench, 1, 1);
    }

    #[test]
    fn a_cancelled_call_is_dropped() {
        let mut bench = Bench::new();
        bench.event(all_diagnostics(1, 1));
        bench.event(Event::Cancelled {
            conn: 1,
            id: request(1),
        });
        bench.after(DIAGNOSTICS_ALL);
        only_released(&mut bench, 1, 1);
        assert_eq!(bench.host.next_tick(), None);
    }

    #[test]
    fn a_gone_connections_calls_go_with_it() {
        let mut bench = Bench::new();
        bench.event(all_diagnostics(1, 1));
        bench.event(all_diagnostics(2, 2));
        bench.event(Event::ClientGone { conn: 1 });
        bench.after(DIAGNOSTICS_ALL);
        let [other, gone] = &seen(&mut bench)[..] else {
            panic!("one for each connection");
        };
        assert!(other.starts_with("Answer { conn: 2,"), "{other}");
        assert_eq!(*gone, format!("{:?}", released(1, 1)));
    }

    #[test]
    fn lost_closes_every_view_and_answers_nothing() {
        let mut bench = Bench::new();
        bench.event(Event::MentionUp {
            socket: PathBuf::from("/run/m.sock"),
        });
        bench.open(1, 1, "open one");
        bench.event(open_diff(2, 2, "opening one"));
        bench.event(all_diagnostics(3, 3));
        bench.event(Event::Lost);
        assert_eq!(bench.editor.count("close"), 2);
        assert_eq!(bench.editor.count("remove"), 1);
        bench.after(DIAGNOSTICS_ALL);
        bench.after(EDITOR_BUDGET);
        assert!(bench.out.is_empty(), "{:?}", bench.out);
        assert_eq!(bench.host.next_tick(), None);
        bench.editor.target.set(Some(2));
        bench.host.check_target(&bench.editor, &mut bench.out);
        assert_eq!(bench.editor.count("define"), 1, "the mention was forgotten");
    }

    #[test]
    fn a_restart_answers_an_open_view_with_an_error_and_stops_the_polling() {
        let mut bench = Bench::new();
        bench.open(1, 1, "tab");
        bench.editor.target.set(Some(2));
        bench.host.check_target(&bench.editor, &mut bench.out);
        let (conn, id, answer) = bench.one_answer();
        assert_eq!((conn, id), (1, request(1)));
        assert_eq!(the_error(&answer), "the editor restarted");
        assert_eq!(bench.host.next_tick(), None);
        assert_eq!(bench.editor.count("close"), 0, "its Lua state is gone with that editor");
    }

    #[test]
    fn an_editor_gone_altogether_ends_a_view_too() {
        let mut bench = Bench::new();
        bench.open(1, 1, "tab");
        bench.editor.target.set(None);
        bench.after(Duration::from_millis(100));
        the_error(&bench.one_answer().2);
    }

    #[test]
    fn a_view_keeps_the_editor_it_was_opened_on() {
        let mut bench = Bench::new();
        bench.event(open_diff(1, 1, "tab"));
        bench.editor.target.set(Some(2));
        bench.editor.answer("open", opened());
        bench.tick();
        assert_eq!(the_error(&bench.one_answer().2), "the editor restarted");
    }

    #[test]
    fn a_take_lost_with_its_editor_holds_up_nothing() {
        let mut bench = Bench::new();
        bench.open(1, 1, "tab");
        assert_eq!(bench.editor.count("take"), 1);
        bench.editor.target.set(Some(2));
        bench.tick();
        bench.out.clear();
        bench.now += Duration::from_millis(100);
        bench.open(1, 2, "tab");
        assert_eq!(bench.editor.count("take"), 2);
    }

    #[test]
    fn mention_outside_is_said_in_the_editor() {
        let mut bench = Bench::new();
        bench.event(Event::MentionOutside);
        assert_eq!(bench.editor.names(), ["outside"]);
    }

    #[test]
    fn started_is_the_wirings() {
        let mut bench = Bench::new();
        bench.event(Event::Started(crate::ide_server::Started::Attached { port: 1 }));
        assert!(bench.editor.names().is_empty());
        assert!(bench.out.is_empty());
    }

    #[test]
    fn a_restarted_editor_gets_the_command_on_its_first_context() {
        let socket = PathBuf::from("/run/m.sock");
        let mut bench = Bench::new();
        bench.event(Event::MentionUp { socket: socket.clone() });
        assert_eq!(bench.editor.count("define"), 1);
        bench.host.check_target(&bench.editor, &mut bench.out);
        assert_eq!(bench.editor.count("define"), 1, "the same editor has it");
        bench.editor.target.set(Some(2));
        bench.host.check_target(&bench.editor, &mut bench.out);
        assert_eq!(bench.editor.count("define"), 2);
        assert_eq!(
            bench.editor.args("define")[1],
            vec![Value::Binary(b"/run/m.sock".to_vec())],
            "the socket travels as an argument"
        );
    }

    #[test]
    fn a_mention_up_before_the_editor_runs_defines_later() {
        let mut bench = Bench::on(None);
        bench.event(Event::MentionUp {
            socket: PathBuf::from("/run/m.sock"),
        });
        bench.host.check_target(&bench.editor, &mut bench.out);
        assert_eq!(bench.editor.count("define"), 0);
        bench.editor.target.set(Some(1));
        bench.host.check_target(&bench.editor, &mut bench.out);
        assert_eq!(bench.editor.count("define"), 1);
        bench.event(Event::MentionDown);
        assert_eq!(bench.editor.count("remove"), 1);
        bench.editor.target.set(Some(2));
        bench.host.check_target(&bench.editor, &mut bench.out);
        assert_eq!(bench.editor.count("define"), 1, "forgotten with the last client");
    }
}
