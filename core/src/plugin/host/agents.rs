//! The `agents` capability, served to a plugin page: the process's view of every agent tab, its changes as
//! events, and the verbs that move the keys (`focus`, `new`, `select`, `round`, `stop`).
//!
//! **One reader per process.** Every page of the window that watches shares one [`AgentsFeed`]: it starts the
//! process's [`crate::agents::Reader`] (on a thread of its own: it makes and opens the agents directory) when the
//! first document watches and stops it when the last one goes. The reader's events land in the feed's inbox on
//! the reader's thread and raise each watching host's wake; the hosts take them on the window's thread, in their
//! pump, from a log each reads at its own place, so one host off screen holds nobody else up.
//!
//! **Events.** A document gets `agents.*` events only after it said `host.hello` and its `agents.watch` was
//! answered, and only while its module is on screen. The watch answer is the view as the feed has it at that
//! moment, and the document reads the log from there on, so no event it gets is older than its view. A document
//! that missed changes (off screen, or before its hello) gets one `agents.reset` with the whole view when it can
//! hear again, before any other event.
//!
//! **The gesture rule.** Every verb but `agents.watch` moves the keys or the shared state the faces show, so it
//! is taken only from a page whose module has the keys in the active window and whose view had a key or button
//! press less than `GESTURE_MS` ago (the window records it, observing and consuming nothing). A page cannot pull
//! the keys from the person on a timer. `agents.new` is taken at most once in `NEW_MIN_INTERVAL_MS`.
//!
//! **What it never does.** A verb brings a tab forward, opens an empty one, or writes the shared selection or
//! round. None of them answers a card, a question or a trust prompt, sends text or changes a mode: the card that
//! comes forward is answered only by keys pressed in the chat that shows it.

use std::any::Any;
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{answer_or_drop, Pending, PluginHost, PluginHostState};
use crate::agents::shared::{self, Round, RoundPoint, Selection, SharedStore};
use crate::agents::{
    wire, Change, Class, Intent, ReaderEvent, Row, View, GESTURE_MS, NEW_MIN_INTERVAL_MS, ROUND_MAX_IDLE_MS, SCHEMA_V,
};
use crate::layout::ModuleId;
use crate::plugin::protocol::{
    self, AgentsRound, ErrorCode, Reply, RoundOp, Verb, EVENT_AGENTS_GONE, EVENT_AGENTS_RESET, EVENT_AGENTS_ROUND,
    EVENT_AGENTS_ROW, EVENT_AGENTS_SELECTION,
};

/// How long a verb waits for the window it asked (or for the shared file it writes) before the page is told.
/// The control socket's client gives up well before this, so the window's own refusal normally comes first.
pub const AGENTS_DEADLINE: Duration = Duration::from_secs(5);
/// How long a watch waits for the process's reader to read the view the first time.
pub const WATCH_DEADLINE: Duration = Duration::from_secs(5);
/// How many changes the feed keeps for a host that has not read them; one that falls further behind gets a reset.
const LOG_MAX: usize = 1024;

const DISABLED: &str = "the agents layer is off in this window (init.lua's agents.enabled)";
const NO_GESTURE: &str =
    "agents verbs need a key or click in this page in the last second, with its module holding the keys";

/// Why a request to a window, or a shared write, came to nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentsRefusal {
    /// The tab (or the window that held it) is gone.
    Gone,
    /// No window of the project runs.
    NoWindow,
    /// The window or the disk said no; the words say why.
    Refused(String),
}

impl AgentsRefusal {
    /// The refusal a control socket's reply stands for: `gone` is its own code, every other word is `refused`.
    pub fn of_words(words: &str) -> AgentsRefusal {
        if words == "gone" {
            AgentsRefusal::Gone
        } else {
            AgentsRefusal::Refused(words.to_owned())
        }
    }
}

/// Called once with what came of a request, on whichever thread finished it.
pub type AgentsDone = Box<dyn FnOnce(Result<(), AgentsRefusal>) + Send>;

/// What the window gives the host to act with. Every method returns at once: what blocks (a socket, a file) runs
/// on a worker, and `done` is called when it finished.
pub trait AgentsPort {
    /// The instance this process publishes its rows under, when it publishes any.
    fn own_instance(&self) -> Option<String>;
    /// Brings tab `tab` of process `instance` forward (`None`: the module that last had the keys there), with
    /// its waiting card first when `card_first`, and presents that window.
    fn focus(&self, instance: &str, tab: Option<u64>, card_first: bool, intent: Intent, done: AgentsDone);
    /// Where a round started in this window goes back to: its process's instance, or, in a window that publishes
    /// no rows, an address of its own that only this window answers to.
    fn origin(&self) -> String;
    /// Whether `instance` names this window ([`AgentsPort::origin`], or its process's instance).
    fn is_here(&self, instance: &str) -> bool;
    /// Gives the keys to module `module` of this window.
    fn focus_module(&self, module: &str, done: AgentsDone);
    /// Opens an empty chat for project `project_key` in the window the layer picks among every running window of
    /// that project, one that holds no tab (the compositor hub) included; none is `NoWindow`.
    fn new_in(&self, project_key: &str, done: AgentsDone);
    /// Replaces the shared selection.
    fn write_selection(&self, selection: Selection, done: AgentsDone);
    /// Replaces the shared round; `None` ends it.
    fn write_round(&self, round: Option<Round>, done: AgentsDone);
}

/// Writes the shared selection or round under `control_dir` on a new thread: what a window's [`AgentsPort`] runs.
/// The writes of a process land in the order they were asked for, whichever thread runs first.
pub fn write_shared_on_thread(control_dir: PathBuf, write: SharedWrite, done: AgentsDone) {
    static ORDER: std::sync::LazyLock<WriteOrder> = std::sync::LazyLock::new(WriteOrder::default);
    let ticket = ORDER.ticket();
    let spawned = std::thread::Builder::new()
        .name("eitri-agents-shared".into())
        .spawn(move || {
            let written = ORDER.write(ticket, &control_dir, &write);
            done(written.map_err(|e| AgentsRefusal::Refused(format!("the shared agents file was not written: {e}"))));
        });
    if let Err(e) = spawned {
        eprintln!("[agents] could not start a shared write: {e}");
    }
}

/// Keeps a process's shared writes in the order they were asked for. Each takes a ticket when it is asked (on the
/// window's thread); the writes run one at a time, and one whose file a later ticket already wrote is skipped: it
/// was replaced before it landed, so the newer selection or round stays.
#[derive(Default)]
pub struct WriteOrder {
    next: std::sync::atomic::AtomicU64,
    /// The newest ticket written to each file, by control directory and file.
    written: Mutex<BTreeMap<(PathBuf, bool), u64>>,
}

impl WriteOrder {
    pub fn ticket(&self) -> u64 {
        self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
    }

    pub fn write(&self, ticket: u64, control_dir: &std::path::Path, write: &SharedWrite) -> std::io::Result<()> {
        let mut written = self.written.lock().unwrap_or_else(|p| p.into_inner());
        let file = (control_dir.to_path_buf(), matches!(write, SharedWrite::Round(_)));
        if written.get(&file).is_some_and(|&newest| newest > ticket) {
            return Ok(());
        }
        let store = SharedStore::open(control_dir)?;
        match write {
            SharedWrite::Selection(s) => store.write_selection(s)?,
            SharedWrite::Round(r) => store.write_round(r.as_ref())?,
        }
        written.insert(file, ticket);
        Ok(())
    }
}

/// One write of a shared file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SharedWrite {
    Selection(Selection),
    Round(Option<Round>),
}

/// When the page's view last had a key or button press, as the window records it.
#[derive(Debug, Default)]
pub struct GestureClock(std::cell::Cell<Option<Instant>>);

impl GestureClock {
    pub fn record(&self, at: Instant) {
        self.0.set(Some(at));
    }

    pub fn last(&self) -> Option<Instant> {
        self.0.get()
    }
}

/// Starts the process's reader for a feed. What it returns keeps the reader running; dropping it stops it.
pub trait AgentsSource {
    fn start(&self, sink: ReaderSink) -> Box<dyn Any>;
}

/// Where a reader's events go, on the reader's thread.
pub type ReaderSink = Box<dyn Fn(ReaderEvent) + Send + Sync>;

/// The real source: a [`crate::agents::Reader`] on `control_dir`, started and stopped on threads of their own.
#[cfg(target_os = "linux")]
pub struct DirSource {
    pub control_dir: PathBuf,
}

#[cfg(target_os = "linux")]
struct ReaderGuard {
    slot: Arc<Mutex<Option<crate::agents::Reader>>>,
    stopped: Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(target_os = "linux")]
impl Drop for ReaderGuard {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        self.stopped.store(true, Ordering::SeqCst);
        let reader = self.slot.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(reader) = reader {
            // Stopping joins the reader's thread: not on the window's.
            let _ = std::thread::Builder::new()
                .name("eitri-agents-read-stop".into())
                .spawn(move || reader.stop());
        }
    }
}

#[cfg(target_os = "linux")]
impl AgentsSource for DirSource {
    fn start(&self, sink: ReaderSink) -> Box<dyn Any> {
        use std::sync::atomic::{AtomicBool, Ordering};
        let slot: Arc<Mutex<Option<crate::agents::Reader>>> = Arc::default();
        let stopped = Arc::new(AtomicBool::new(false));
        let (dir, into, gone) = (self.control_dir.clone(), slot.clone(), stopped.clone());
        let sink = Arc::new(sink);
        let failed = sink.clone();
        let spawned = std::thread::Builder::new()
            .name("eitri-agents-read-start".into())
            .spawn(
                move || match crate::agents::Reader::start(&dir, SCHEMA_V, move |e| sink(e)) {
                    Ok(reader) => {
                        let mut held = into.lock().unwrap_or_else(|p| p.into_inner());
                        if gone.load(Ordering::SeqCst) {
                            drop(held);
                            reader.stop();
                        } else {
                            *held = Some(reader);
                        }
                    }
                    Err(e) => failed(ReaderEvent::Error(format!(
                        "the agents directory could not be watched: {e}"
                    ))),
                },
            );
        if let Err(e) = spawned {
            eprintln!("[agents] could not start the reader: {e}");
        }
        Box::new(ReaderGuard { slot, stopped })
    }
}

/// What the reader's thread hands the window's thread.
#[derive(Default)]
struct Inbox {
    /// Each event with the start it came from: a stopped reader's late events are not the next one's.
    events: Vec<(u64, ReaderEvent)>,
    wakes: BTreeMap<u64, agent::UiWake>,
}

struct FeedState {
    /// The view as the reader's events so far make it; `None` until its first whole view.
    view: Option<View>,
    /// The changes since the oldest place any watcher reads from; `first` is the number of `log[0]`.
    log: VecDeque<Change>,
    first: u64,
    /// Each watcher's place: the number of the next change it reads.
    places: BTreeMap<u64, u64>,
    next_watcher: u64,
    reader: Option<Box<dyn Any>>,
    starts: usize,
    failed: Option<String>,
    /// The reader failed after it had given a view: the feed starts one again, once, on the next pump.
    restart_due: bool,
}

/// The process's one view of every agent, shared by the pages that watch it.
pub struct AgentsFeed {
    source: Box<dyn AgentsSource>,
    inbox: Arc<Mutex<Inbox>>,
    state: RefCell<FeedState>,
}

/// What a watcher reads next.
#[derive(Debug, Clone, PartialEq)]
pub enum FeedRead {
    /// No whole view yet.
    NotYet,
    /// The changes since its place.
    Changes(Vec<Change>),
    /// It fell behind what the feed keeps: here is the whole view.
    Behind(Box<View>),
}

fn row_order(r: &Row) -> (&str, &str, &str, &str, u16, &str) {
    (
        &r.project.name,
        &r.project.key,
        &r.window.name,
        &r.window.instance,
        r.number,
        &r.id,
    )
}

impl AgentsFeed {
    pub fn new(source: Box<dyn AgentsSource>) -> Rc<AgentsFeed> {
        Rc::new(AgentsFeed {
            source,
            inbox: Arc::default(),
            state: RefCell::new(FeedState {
                view: None,
                log: VecDeque::new(),
                first: 0,
                places: BTreeMap::new(),
                next_watcher: 1,
                reader: None,
                starts: 0,
                failed: None,
                restart_due: false,
            }),
        })
    }

    /// A feed of the reader on `control_dir`.
    #[cfg(target_os = "linux")]
    pub fn of_dir(control_dir: PathBuf) -> Rc<AgentsFeed> {
        AgentsFeed::new(Box::new(DirSource { control_dir }))
    }

    /// A new watcher, woken by `wake` on every reader event; the first one starts the reader.
    pub fn watch(&self, wake: agent::UiWake) -> u64 {
        let (id, start) = {
            let mut st = self.state.borrow_mut();
            let id = st.next_watcher;
            st.next_watcher += 1;
            let end = st.first + st.log.len() as u64;
            st.places.insert(id, end);
            (id, st.reader.is_none())
        };
        self.inbox
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .wakes
            .insert(id, wake);
        if start {
            self.start_reader();
        }
        id
    }

    /// Starts a reader of a new generation, in place of the one running (whose late events are then ignored).
    fn start_reader(&self) {
        let inbox = self.inbox.clone();
        let generation = self.state.borrow().starts as u64 + 1;
        let sink: ReaderSink = Box::new(move |event| {
            let wakes: Vec<agent::UiWake> = {
                let mut inbox = inbox.lock().unwrap_or_else(|p| p.into_inner());
                inbox.events.push((generation, event));
                inbox.wakes.values().cloned().collect()
            };
            for wake in wakes {
                wake();
            }
        });
        let reader = self.source.start(sink);
        let old = {
            let mut st = self.state.borrow_mut();
            st.starts += 1;
            st.failed = None;
            st.restart_due = false;
            st.view = None;
            st.reader.replace(reader)
        };
        drop(old);
    }

    /// A reader that failed is started again while documents watch: always when one asks to watch (`any`), and
    /// otherwise only when it failed after it had given a view, so a reader that cannot start is not retried in a
    /// loop. Returns whether it started one.
    pub fn restart_if_failed(&self, any: bool) -> bool {
        self.take_inbox();
        let due = {
            let st = self.state.borrow();
            st.reader.is_some() && !st.places.is_empty() && st.failed.is_some() && (any || st.restart_due)
        };
        if due {
            self.start_reader();
        }
        due
    }

    /// The watcher is gone; the last one stops the reader, and the next start reads the view afresh.
    pub fn unwatch(&self, id: u64) {
        self.inbox.lock().unwrap_or_else(|p| p.into_inner()).wakes.remove(&id);
        let reader = {
            let mut st = self.state.borrow_mut();
            st.places.remove(&id);
            if !st.places.is_empty() {
                Self::trim(&mut st);
                return;
            }
            st.view = None;
            st.first += st.log.len() as u64;
            st.log.clear();
            st.reader.take()
        };
        // Whatever the stopped reader still sent is of no use to the next one.
        self.inbox.lock().unwrap_or_else(|p| p.into_inner()).events.clear();
        drop(reader);
    }

    /// How many times a reader was started: one per stretch of time any page watched.
    pub fn starts(&self) -> usize {
        self.state.borrow().starts
    }

    /// Whether a reader runs now.
    pub fn reading(&self) -> bool {
        self.state.borrow().reader.is_some()
    }

    /// The view now, after the reader's latest events.
    pub fn view(&self) -> Option<View> {
        self.take_inbox();
        self.state.borrow().view.clone()
    }

    /// Why the reader stopped, if it failed.
    pub fn failed(&self) -> Option<String> {
        self.take_inbox();
        self.state.borrow().failed.clone()
    }

    /// The view now, after the reader's latest events, for watcher `id`, whose place moves to the end in the same
    /// step: nothing that arrives after it is in the view and also skipped. No view yet: why the reader failed, if
    /// it did.
    pub fn take_view(&self, id: u64) -> Result<View, Option<String>> {
        self.take_inbox();
        let mut st = self.state.borrow_mut();
        let Some(view) = st.view.clone() else {
            return Err(st.failed.clone());
        };
        let end = st.first + st.log.len() as u64;
        if let Some(place) = st.places.get_mut(&id) {
            *place = end;
        }
        Self::trim(&mut st);
        Ok(view)
    }

    /// What watcher `id` has not read yet; its place moves to the end.
    pub fn read(&self, id: u64) -> FeedRead {
        self.take_inbox();
        let mut st = self.state.borrow_mut();
        let Some(view) = st.view.clone() else {
            return FeedRead::NotYet;
        };
        let end = st.first + st.log.len() as u64;
        let Some(place) = st.places.insert(id, end) else {
            return FeedRead::NotYet;
        };
        let read = if place < st.first {
            FeedRead::Behind(Box::new(view))
        } else {
            let from = (place - st.first) as usize;
            FeedRead::Changes(st.log.iter().skip(from).cloned().collect())
        };
        Self::trim(&mut st);
        read
    }

    fn trim(st: &mut FeedState) {
        let oldest = st
            .places
            .values()
            .copied()
            .min()
            .unwrap_or(st.first + st.log.len() as u64);
        while st.first < oldest && !st.log.is_empty() {
            st.log.pop_front();
            st.first += 1;
        }
        while st.log.len() > LOG_MAX {
            st.log.pop_front();
            st.first += 1;
        }
    }

    fn take_inbox(&self) {
        let events = std::mem::take(&mut self.inbox.lock().unwrap_or_else(|p| p.into_inner()).events);
        if events.is_empty() {
            return;
        }
        let mut st = self.state.borrow_mut();
        if st.reader.is_none() {
            return;
        }
        let current = st.starts as u64;
        for (generation, event) in events {
            if generation != current {
                continue;
            }
            match event {
                ReaderEvent::Change(change) => {
                    Self::apply(&mut st, &change);
                    st.log.push_back(change);
                }
                ReaderEvent::Error(why) => {
                    // Nothing follows an error: the view it leaves would stay as it is, so it is dropped.
                    eprintln!("[agents] the pages' reader stopped: {why}");
                    st.restart_due = st.view.take().is_some();
                    st.failed = Some(why);
                }
            }
        }
        Self::trim(&mut st);
    }

    fn apply(st: &mut FeedState, change: &Change) {
        if let Change::Reset { view } = change {
            st.view = Some(view.clone());
            return;
        }
        let Some(view) = st.view.as_mut() else {
            return;
        };
        match change {
            Change::Row { row } => {
                view.rows.retain(|r| r.id != row.id);
                let at = view.rows.partition_point(|r| row_order(r) < row_order(row));
                view.rows.insert(at, row.clone());
            }
            Change::Gone { id, .. } => view.rows.retain(|r| r.id != *id),
            Change::Selection { selection } => view.selection = selection.clone(),
            Change::Round { round } => view.round = round.clone(),
            Change::Reset { .. } => {}
        }
    }
}

/// Whether anything is for the person to do at `now_ms`: a row waits for them, or the round runs
/// ([`shared::is_running`]). A round that ended with the person in it (no current stop) is kept only so `prefix B`
/// can go back; one whose stop's process went, or that stood idle too long, does not run either.
pub fn something_waits(view: &View, now_ms: u64) -> bool {
    view.round.as_ref().is_some_and(|r| shared::is_running(view, r, now_ms))
        || view.rows.iter().any(|r| r.class == Class::Needs)
}

/// The window's own watch over the feed for a module shown while agents wait (`[module] shown_while =
/// "agents-wait"`): it keeps the process's reader running with no page watching, since the module's page is not
/// built before its first show and hears nothing off screen, and says when the module should come or go. It answers
/// only, verbs and all: it shows a module, never gives it the keys, and never touches a card.
pub struct WaitShow {
    feed: Rc<AgentsFeed>,
    watcher: u64,
    said: std::cell::Cell<Option<bool>>,
}

impl WaitShow {
    /// Watches `feed`; `wake` is raised on every reader event, after which [`WaitShow::pump`] reads the answer.
    pub fn new(feed: Rc<AgentsFeed>, wake: agent::UiWake) -> WaitShow {
        let watcher = feed.watch(wake);
        WaitShow {
            feed,
            watcher,
            said: std::cell::Cell::new(None),
        }
    }

    /// `Some(true)` to show the module, `Some(false)` to hide it, each only when the answer changed since the
    /// last one said (the first view always says one); `None` with nothing new or no view yet.
    pub fn pump(&self) -> Option<bool> {
        self.pump_at(wall_clock_ms())
    }

    /// [`WaitShow::pump`] at `now_ms`.
    pub fn pump_at(&self, now_ms: u64) -> Option<bool> {
        self.feed.restart_if_failed(false);
        // Its place moves with every read, so the feed keeps no change for it.
        let _ = self.feed.read(self.watcher);
        let want = something_waits(&self.feed.view()?, now_ms);
        (self.said.replace(Some(want)) != Some(want)).then_some(want)
    }

    /// When to pump again with nothing new read: while a running round is on screen, at the moment it will have
    /// stood idle too long, since no file changes when a round merely expires. `None` while no round runs.
    pub fn recheck_in(&self, now_ms: u64) -> Option<Duration> {
        let view = self.feed.view()?;
        let round = view.round.as_ref().filter(|r| shared::is_running(&view, r, now_ms))?;
        Some(Duration::from_millis(
            round.at_ms.saturating_add(ROUND_MAX_IDLE_MS).saturating_sub(now_ms),
        ))
    }
}

impl Drop for WaitShow {
    fn drop(&mut self) {
        self.feed.unwatch(self.watcher);
    }
}

/// What the window hands a host that serves `agents`: the process's feed, the window's port and the page's
/// gesture clock. A host without them answers every agents verb `disabled`.
#[derive(Clone)]
pub struct AgentsServices {
    pub feed: Rc<AgentsFeed>,
    pub port: Rc<dyn AgentsPort>,
    pub gesture: Rc<GestureClock>,
}

/// A verb waiting for its parts (a focus, a shared write) to finish.
pub(super) struct Owed {
    ticket: u64,
    parts: u8,
    failed: Option<AgentsRefusal>,
}

/// What came of each ticket, put there on whichever thread finished it.
type Finished = Arc<Mutex<Vec<(u64, Result<(), AgentsRefusal>)>>>;

/// The host's agents state.
#[derive(Default)]
pub(super) struct HostAgents {
    services: Option<AgentsServices>,
    /// The host's place in the feed, held while a document watches.
    watcher: Option<u64>,
    /// The document that called `agents.watch`.
    watching: Option<u64>,
    /// Its watch was answered: events may follow.
    answered: bool,
    /// It did not hear some changes (off screen, or before its hello): a reset comes first when it can hear.
    missed: bool,
    owed_watch: Vec<Pending<()>>,
    owed: Vec<Pending<Owed>>,
    answers: Finished,
    next_ticket: u64,
    last_new: Option<Instant>,
}

fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The view as a page gets it: the versioned object `eitri agents --json` prints.
fn view_value(view: &View) -> Value {
    serde_json::from_str(&wire::view_json(view, SCHEMA_V)).expect("a view is JSON")
}

/// The event a change is to a page.
fn change_event(change: &Change) -> String {
    match change {
        Change::Row { row } => protocol::event_text(EVENT_AGENTS_ROW, row),
        Change::Gone { id, key, why } => {
            protocol::event_text(EVENT_AGENTS_GONE, &json!({ "id": id, "key": key, "why": why }))
        }
        Change::Reset { view } => protocol::event_text(EVENT_AGENTS_RESET, &view_value(view)),
        Change::Selection { selection } => protocol::event_text(EVENT_AGENTS_SELECTION, selection),
        Change::Round { round } => protocol::event_text(EVENT_AGENTS_ROUND, round),
    }
}

/// A row id's process and tab: `<instance>.<tab>`.
fn split_id(id: &str) -> Option<(&str, u64)> {
    let (instance, tab) = id.rsplit_once('.')?;
    (!instance.is_empty()).then_some(())?;
    Some((instance, tab.parse().ok()?))
}

fn refusal_reply(re: u64, refusal: &AgentsRefusal) -> Reply {
    match refusal {
        AgentsRefusal::Gone => Reply::err(re, ErrorCode::Gone, "that agent's tab is gone"),
        AgentsRefusal::NoWindow => Reply::err(re, ErrorCode::NoWindow, "no window of that project runs"),
        AgentsRefusal::Refused(why) => Reply::err(re, ErrorCode::Refused, why),
    }
}

/// Whether the host's document hears agents events now.
fn hears(st: &PluginHostState) -> bool {
    st.hello && st.on_screen && st.agents.answered && st.agents.watching == Some(st.document)
}

impl PluginHost {
    /// Gives this host the window's agents services; `None` (the layer off) answers every agents verb `disabled`.
    pub fn set_agents(&self, services: Option<AgentsServices>) {
        let old = {
            let mut st = self.state.borrow_mut();
            let old = st.agents.watcher.take().zip(st.agents.services.clone());
            st.agents.services = services;
            st.agents.watching = None;
            st.agents.answered = false;
            old
        };
        if let Some((watcher, services)) = old {
            services.feed.unwatch(watcher);
        }
    }

    /// One agents verb from the page.
    pub(super) fn agents_verb(&self, re: u64, verb: Verb, now: Instant) -> Option<Reply> {
        if self.state.borrow().agents.services.is_none() {
            return Some(Reply::err(re, ErrorCode::Disabled, DISABLED));
        }
        if let Verb::AgentsWatch(_) = verb {
            return self.agents_watch(re, now);
        }
        if let Err(reply) = self.agents_gesture(re, now) {
            return Some(reply);
        }
        match verb {
            Verb::AgentsFocus(args) => self.agents_focus(re, &args.id, args.card_first, Intent::Show, now),
            Verb::AgentsStop(args) => self.agents_focus(re, &args.id, false, Intent::Stop, now),
            Verb::AgentsNew(args) => self.agents_new(re, &args.project, now),
            Verb::AgentsSelect(args) => self.agents_select(re, args.key, now),
            Verb::AgentsRound(args) => self.agents_round(re, args, now),
            _ => None,
        }
    }

    /// The gesture rule: the page's module has the keys in the active window, and the page had a key or button
    /// press less than `GESTURE_MS` before `now`.
    fn agents_gesture(&self, re: u64, now: Instant) -> Result<(), Reply> {
        let st = self.state.borrow();
        let recent = st
            .agents
            .services
            .as_ref()
            .and_then(|s| s.gesture.last())
            .is_some_and(|at| now.saturating_duration_since(at) < Duration::from_millis(GESTURE_MS));
        if st.has_keys && recent {
            Ok(())
        } else {
            Err(Reply::err(re, ErrorCode::NoGesture, NO_GESTURE))
        }
    }

    fn agents_watch(&self, re: u64, now: Instant) -> Option<Reply> {
        let services = {
            let mut st = self.state.borrow_mut();
            let document = st.document;
            st.agents.watching = Some(document);
            st.agents.services.clone()?
        };
        let watcher = self.state.borrow().agents.watcher;
        let watcher = match watcher {
            Some(watcher) => watcher,
            None => {
                let watcher = services.feed.watch(self.wake.clone());
                self.state.borrow_mut().agents.watcher = Some(watcher);
                watcher
            }
        };
        // A reader that failed is started again: the watch is answered from its first view.
        services.feed.restart_if_failed(true);
        match services.feed.take_view(watcher) {
            Ok(view) => {
                let mut st = self.state.borrow_mut();
                st.agents.answered = true;
                st.agents.missed = false;
                return Some(Reply::ok(re, view_value(&view)));
            }
            Err(Some(why)) => return Some(Reply::err(re, ErrorCode::Io, &why)),
            Err(None) => {}
        }
        let mut st = self.state.borrow_mut();
        let document = st.document;
        st.agents
            .owed_watch
            .push(Pending::new(Some(re), document, now + WATCH_DEADLINE, ()));
        None
    }

    /// A new ticket whose done callbacks land in this host's inbox and raise its wake.
    fn agents_owe(&self, re: u64, parts: u8, now: Instant) -> u64 {
        let mut st = self.state.borrow_mut();
        st.agents.next_ticket += 1;
        let ticket = st.agents.next_ticket;
        let document = st.document;
        st.agents.owed.push(Pending::new(
            Some(re),
            document,
            now + AGENTS_DEADLINE,
            Owed {
                ticket,
                parts,
                failed: None,
            },
        ));
        ticket
    }

    fn agents_done(&self, ticket: u64) -> AgentsDone {
        let answers = self.state.borrow().agents.answers.clone();
        let wake = self.wake.clone();
        Box::new(move |result| {
            answers.lock().unwrap_or_else(|p| p.into_inner()).push((ticket, result));
            wake();
        })
    }

    fn agents_port(&self) -> Option<(Rc<dyn AgentsPort>, Rc<AgentsFeed>)> {
        let st = self.state.borrow();
        st.agents.services.as_ref().map(|s| (s.port.clone(), s.feed.clone()))
    }

    fn agents_focus(&self, re: u64, id: &str, card_first: bool, intent: Intent, now: Instant) -> Option<Reply> {
        let (port, feed) = self.agents_port()?;
        let Some((instance, tab)) = split_id(id) else {
            return Some(refusal_reply(re, &AgentsRefusal::Gone));
        };
        if let Some(view) = feed.view() {
            if !view.rows.iter().any(|r| r.id == id) {
                return Some(refusal_reply(re, &AgentsRefusal::Gone));
            }
        }
        let ticket = self.agents_owe(re, 1, now);
        port.focus(instance, Some(tab), card_first, intent, self.agents_done(ticket));
        None
    }

    fn agents_new(&self, re: u64, project: &str, now: Instant) -> Option<Reply> {
        let (port, feed) = self.agents_port()?;
        let soon = self
            .state
            .borrow()
            .agents
            .last_new
            .is_some_and(|at| now.saturating_duration_since(at) < Duration::from_millis(NEW_MIN_INTERVAL_MS));
        if soon {
            return Some(Reply::err(re, ErrorCode::Refused, "one new agent a second at most"));
        }
        // The view names every project with a running window, rows or not; the window itself is picked by the port.
        if let Some(view) = feed.view() {
            let runs =
                view.projects.iter().any(|p| p.key == project) || view.rows.iter().any(|r| r.project.key == project);
            if !runs {
                return Some(refusal_reply(re, &AgentsRefusal::NoWindow));
            }
        }
        self.state.borrow_mut().agents.last_new = Some(now);
        let ticket = self.agents_owe(re, 1, now);
        port.new_in(project, self.agents_done(ticket));
        None
    }

    fn agents_select(&self, re: u64, key: String, now: Instant) -> Option<Reply> {
        let (port, feed) = self.agents_port()?;
        if let Some(view) = feed.view() {
            if !view.rows.iter().any(|r| r.key == key) {
                return Some(refusal_reply(re, &AgentsRefusal::Gone));
            }
        }
        let selection = Selection {
            key,
            by: port.own_instance().unwrap_or_default(),
            at_ms: wall_clock_ms(),
        };
        let ticket = self.agents_owe(re, 1, now);
        port.write_selection(selection, self.agents_done(ticket));
        None
    }

    /// Writes `round` and focuses its current stop with its card first; the page is answered when both finished.
    fn agents_round_to(&self, re: u64, round: Round, view: &View, now: Instant) -> Option<Reply> {
        let (port, _) = self.agents_port()?;
        let stop = round
            .current
            .as_deref()
            .and_then(|key| view.rows.iter().find(|r| r.key == key))
            .and_then(|row| split_id(&row.id).map(|(i, t)| (i.to_owned(), t)));
        let Some((instance, tab)) = stop else {
            return Some(refusal_reply(re, &AgentsRefusal::Gone));
        };
        let ticket = self.agents_owe(re, 2, now);
        port.write_round(Some(round), self.agents_done(ticket));
        port.focus(&instance, Some(tab), true, Intent::Show, self.agents_done(ticket));
        None
    }

    fn agents_round(&self, re: u64, args: AgentsRound, now: Instant) -> Option<Reply> {
        let (port, feed) = self.agents_port()?;
        let Some(view) = feed.view() else {
            return Some(Reply::err(
                re,
                ErrorCode::Refused,
                "the agents view is not read yet: call agents.watch first",
            ));
        };
        let now_ms = wall_clock_ms();
        let from = RoundPoint {
            instance: port.origin(),
            module: ModuleId::lua(&self.plugin.manifest.id).as_str().to_owned(),
            tab: None,
        };
        // A round that ended with the person in it (no current stop) or stood idle too long does not run: a start or
        // a next begins a fresh one from here, and a leave only removes it.
        let ongoing = view
            .round
            .clone()
            .filter(|r| r.current.is_some() && !shared::is_idle(r, now_ms));
        match (args.op, view.round.clone()) {
            (RoundOp::Start, _) => {
                let started = match (args.key.as_deref(), ongoing) {
                    // A running round moves to the key, keeping where it started and what it visited.
                    (Some(key), Some(r)) => shared::start_at(&view, r.from.clone(), Some(key), now_ms).map(|moved| {
                        let mut visited = r.visited.clone();
                        visited.extend(moved.visited.into_iter().filter(|k| !r.visited.contains(k)));
                        Round {
                            visited,
                            started_at_ms: r.started_at_ms,
                            ..moved
                        }
                    }),
                    (key, None) => shared::start_at(&view, from, key, now_ms),
                    // Started again with no key: it goes back to its current stop, or on from it when that stop's
                    // process went.
                    (None, Some(r)) if shared::is_running(&view, &r, now_ms) => Some(r),
                    (None, Some(r)) => shared::advance(&view, &r, now_ms),
                };
                let listed = |key: &str| view.rows.iter().any(|r| r.key == key);
                match started {
                    Some(round) => self.agents_round_to(re, round, &view, now),
                    None if args.key.as_deref().is_some_and(listed) => {
                        Some(Reply::err(re, ErrorCode::Refused, "that agent does not wait for you"))
                    }
                    None if args.key.is_some() => Some(refusal_reply(re, &AgentsRefusal::Gone)),
                    None => Some(Reply::err(re, ErrorCode::Refused, "no agent waits for you")),
                }
            }
            (RoundOp::Next, _) => match ongoing {
                Some(r) => match shared::advance(&view, &r, now_ms) {
                    Some(next) => self.agents_round_to(re, next, &view, now),
                    // No stop left: the round ends where it is, keeping where it started for `prefix B`, as the
                    // round keys end it.
                    None => {
                        let ended = Round {
                            current: None,
                            at_ms: now_ms,
                            ..r
                        };
                        let ticket = self.agents_owe(re, 1, now);
                        port.write_round(Some(ended), self.agents_done(ticket));
                        None
                    }
                },
                None => match shared::start_at(&view, from, None, now_ms) {
                    Some(round) => self.agents_round_to(re, round, &view, now),
                    None => Some(Reply::err(re, ErrorCode::Refused, "no agent waits for you")),
                },
            },
            // Idle too long: the round is over, and leaving it only removes it.
            (RoundOp::Leave, Some(r)) if shared::is_idle(&r, now_ms) => {
                let ticket = self.agents_owe(re, 1, now);
                port.write_round(None, self.agents_done(ticket));
                None
            }
            (RoundOp::Leave, Some(r)) => {
                // Started in this window from a module that is not a tab (a page): the keys go back to that module.
                if r.from.tab.is_none() && !r.from.module.is_empty() && port.is_here(&r.from.instance) {
                    let ticket = self.agents_owe(re, 2, now);
                    port.write_round(None, self.agents_done(ticket));
                    port.focus_module(&r.from.module, self.agents_done(ticket));
                    return None;
                }
                if r.from.instance.is_empty() {
                    let ticket = self.agents_owe(re, 1, now);
                    port.write_round(None, self.agents_done(ticket));
                    return None;
                }
                let ticket = self.agents_owe(re, 2, now);
                port.write_round(None, self.agents_done(ticket));
                port.focus(
                    &r.from.instance,
                    r.from.tab,
                    false,
                    Intent::Show,
                    self.agents_done(ticket),
                );
                None
            }
            (RoundOp::Leave, None) => Some(Reply::ok(re, json!({}))),
        }
    }

    /// The module came back on screen: a watching document gets the whole view before any other event.
    pub(super) fn agents_back_on_screen(&self) {
        let mut st = self.state.borrow_mut();
        if st.agents.watcher.is_some() {
            st.agents.missed = true;
        }
    }

    /// A new document: it has not watched, and what the old one was owed is dropped where it would be answered.
    pub(super) fn agents_new_document(&self) {
        let (old, owed) = {
            let mut st = self.state.borrow_mut();
            st.agents.watching = None;
            st.agents.answered = false;
            st.agents.missed = false;
            let old = st.agents.watcher.take().zip(st.agents.services.clone());
            (old, std::mem::take(&mut st.agents.owed_watch))
        };
        if let Some((watcher, services)) = old {
            services.feed.unwatch(watcher);
        }
        let audience = self.audience();
        let mut replies = Vec::new();
        for mut pending in owed {
            answer_or_drop(self.id(), audience, &mut pending, |_| None, &mut replies);
        }
        self.send_replies(replies);
    }

    /// Answers what finished, gives up on what is past its deadline, and sends the document its events. Returns
    /// the earliest deadline still owed.
    pub(super) fn agents_pump(&self, now: Instant, replies: &mut Vec<Reply>) -> Option<Instant> {
        let audience = self.audience();
        let (finished, feed, watcher) = {
            let st = self.state.borrow();
            let finished = std::mem::take(&mut *st.agents.answers.lock().unwrap_or_else(|p| p.into_inner()));
            (
                finished,
                st.agents.services.as_ref().map(|s| s.feed.clone()),
                st.agents.watcher,
            )
        };
        {
            let mut st = self.state.borrow_mut();
            for (ticket, result) in finished {
                let Some(at) = st.agents.owed.iter().position(|p| p.on.ticket == ticket) else {
                    continue;
                };
                let owed = &mut st.agents.owed[at];
                owed.on.parts = owed.on.parts.saturating_sub(1);
                if let Err(refusal) = result {
                    owed.on.failed.get_or_insert(refusal);
                }
                if owed.on.parts == 0 {
                    let mut owed = st.agents.owed.remove(at);
                    let failed = owed.on.failed.take();
                    answer_or_drop(
                        &self.plugin.manifest.id,
                        audience,
                        &mut owed,
                        |re| {
                            Some(match &failed {
                                Some(refusal) => refusal_reply(re, refusal),
                                None => Reply::ok(re, json!({})),
                            })
                        },
                        replies,
                    );
                }
            }
            let (late, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut st.agents.owed)
                .into_iter()
                .partition(|p| now >= p.deadline);
            st.agents.owed = keep;
            for mut owed in late {
                answer_or_drop(
                    &self.plugin.manifest.id,
                    audience,
                    &mut owed,
                    |re| Some(Reply::err(re, ErrorCode::Busy, "the window did not answer within 5 s")),
                    replies,
                );
            }
        }
        if let (Some(feed), Some(watcher)) = (feed, watcher) {
            self.agents_feed_pump(&feed, watcher, now, replies);
        }
        let st = self.state.borrow();
        st.agents
            .owed
            .iter()
            .map(|p| p.deadline)
            .chain(st.agents.owed_watch.iter().map(|p| p.deadline))
            .min()
    }

    fn agents_feed_pump(&self, feed: &AgentsFeed, watcher: u64, now: Instant, replies: &mut Vec<Reply>) {
        let audience = self.audience();
        feed.restart_if_failed(false);
        let owed = std::mem::take(&mut self.state.borrow_mut().agents.owed_watch);
        let mut keep = Vec::new();
        for mut pending in owed {
            if pending.document != audience.document || audience.closed {
                answer_or_drop(self.id(), audience, &mut pending, |_| None, replies);
                continue;
            }
            let taken = feed.take_view(watcher);
            if let Ok(view) = &taken {
                answer_or_drop(
                    self.id(),
                    audience,
                    &mut pending,
                    |re| Some(Reply::ok(re, view_value(view))),
                    replies,
                );
                let mut st = self.state.borrow_mut();
                st.agents.answered = true;
                st.agents.missed = false;
            } else if let Err(Some(why)) = &taken {
                answer_or_drop(
                    self.id(),
                    audience,
                    &mut pending,
                    |re| Some(Reply::err(re, ErrorCode::Io, why)),
                    replies,
                );
            } else if now >= pending.deadline {
                answer_or_drop(
                    self.id(),
                    audience,
                    &mut pending,
                    |re| {
                        Some(Reply::err(
                            re,
                            ErrorCode::Busy,
                            "the agents directory was not read within 5 s",
                        ))
                    },
                    replies,
                );
            } else {
                keep.push(pending);
            }
        }
        self.state.borrow_mut().agents.owed_watch.extend(keep);
        // The replies go before any event, so a watch answered in this pump comes ahead of what follows it.
        self.send_replies(std::mem::take(replies));
        let (hears, missed, answered) = {
            let st = self.state.borrow();
            (hears(&st), st.agents.missed, st.agents.answered)
        };
        if !answered {
            return;
        }
        if !hears {
            // Read and dropped: the document gets the whole view when it can hear again.
            let dropped = match feed.read(watcher) {
                FeedRead::Changes(changes) => !changes.is_empty(),
                FeedRead::Behind(_) => true,
                FeedRead::NotYet => false,
            };
            if dropped {
                self.state.borrow_mut().agents.missed = true;
            }
            return;
        }
        let events: Vec<String> = if missed {
            // The whole view and the place after it in one step, so no change is both in it and skipped.
            match feed.take_view(watcher) {
                Ok(view) => {
                    self.state.borrow_mut().agents.missed = false;
                    vec![change_event(&Change::Reset { view })]
                }
                Err(_) => Vec::new(),
            }
        } else {
            match feed.read(watcher) {
                FeedRead::NotYet => Vec::new(),
                FeedRead::Behind(view) => vec![change_event(&Change::Reset { view: *view })],
                FeedRead::Changes(changes) => changes.iter().map(change_event).collect(),
            }
        };
        for event in events {
            self.page.send(&event);
        }
    }
}

#[cfg(test)]
mod tests;
