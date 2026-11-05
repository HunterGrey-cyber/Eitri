use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::*;
use crate::agents::{Class, Needs, ProjectRef, WindowRef};
use crate::plugin::host::PagePort;
use crate::plugin::load::load_plugin;
use crate::plugin::test_support::{demo_plugin, Scratch};
use crate::plugin::theme::PageTheme;

#[derive(Default)]
struct Page(RefCell<Vec<Value>>);

impl PagePort for Page {
    fn send(&self, message: &str) {
        self.0
            .borrow_mut()
            .push(serde_json::from_str(message).expect("the host sends JSON"));
    }
    fn set_theme(&self, _theme: &PageTheme) {}
}

impl Page {
    fn take(&self) -> Vec<Value> {
        std::mem::take(&mut *self.0.borrow_mut())
    }
}

/// The names of the agents events in `sent`, in order.
fn agents_events(sent: &[Value]) -> Vec<String> {
    sent.iter()
        .filter_map(|m| m["event"].as_str())
        .filter(|e| e.starts_with("agents."))
        .map(str::to_owned)
        .collect()
}

/// The replies in `sent`, by request id.
fn replies(sent: &[Value]) -> Vec<&Value> {
    sent.iter().filter(|m| m.get("re").is_some()).collect()
}

#[derive(Default)]
struct SourceLog {
    sink: RefCell<Option<ReaderSink>>,
    starts: Cell<usize>,
    stops: Rc<Cell<usize>>,
}

struct Stops(Rc<Cell<usize>>);

impl Drop for Stops {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

struct FakeSource(Rc<SourceLog>);

impl AgentsSource for FakeSource {
    fn start(&self, sink: ReaderSink) -> Box<dyn Any> {
        *self.0.sink.borrow_mut() = Some(sink);
        self.0.starts.set(self.0.starts.get() + 1);
        Box::new(Stops(self.0.stops.clone()))
    }
}

impl SourceLog {
    fn emit(&self, change: Change) {
        let sink = self.sink.borrow();
        (sink.as_ref().expect("a reader runs"))(ReaderEvent::Change(change));
    }

    fn fail(&self, why: &str) {
        let sink = self.sink.borrow();
        (sink.as_ref().expect("a reader runs"))(ReaderEvent::Error(why.to_owned()));
    }
}

/// A window that records what it was asked and answers each request at once with `answer`.
#[derive(Default)]
struct FakePort {
    calls: RefCell<Vec<String>>,
    answer: RefCell<Option<AgentsRefusal>>,
    /// Where a shared write goes, when the test wants the real files.
    control_dir: RefCell<Option<std::path::PathBuf>>,
    /// A window that holds no tab and publishes no rows (a compositor page window).
    rowless: Cell<bool>,
    /// Every round written, in order.
    rounds: RefCell<Vec<Option<Round>>>,
}

/// The address a page window without rows gives a round started in it.
const PAGE_WINDOW: &str = "00000000000000e9";

impl FakePort {
    fn finish(&self, done: AgentsDone) {
        done(self.answer.borrow().clone().map_or(Ok(()), Err));
    }

    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.calls.borrow_mut())
    }
}

impl AgentsPort for FakePort {
    fn own_instance(&self) -> Option<String> {
        (!self.rowless.get()).then(|| A.to_owned())
    }

    fn origin(&self) -> String {
        self.own_instance().unwrap_or_else(|| PAGE_WINDOW.to_owned())
    }

    fn is_here(&self, instance: &str) -> bool {
        instance == self.origin()
    }

    fn focus(&self, instance: &str, tab: Option<u64>, card_first: bool, intent: Intent, done: AgentsDone) {
        self.calls
            .borrow_mut()
            .push(format!("focus {instance} {tab:?} card_first={card_first} {intent:?}"));
        self.finish(done);
    }

    fn focus_module(&self, module: &str, done: AgentsDone) {
        self.calls.borrow_mut().push(format!("module {module}"));
        self.finish(done);
    }

    fn new_in(&self, project_key: &str, done: AgentsDone) {
        self.calls.borrow_mut().push(format!("new {project_key}"));
        self.finish(done);
    }

    fn write_selection(&self, selection: Selection, done: AgentsDone) {
        self.calls
            .borrow_mut()
            .push(format!("select {} by {}", selection.key, selection.by));
        match self.control_dir.borrow().clone() {
            Some(dir) => write_shared_on_thread(dir, SharedWrite::Selection(selection), done),
            None => self.finish(done),
        }
    }

    fn write_round(&self, round: Option<Round>, done: AgentsDone) {
        let current = round.as_ref().map(|r| r.current.clone());
        self.calls.borrow_mut().push(format!("round {current:?}"));
        self.rounds.borrow_mut().push(round);
        self.finish(done);
    }
}

struct Rig {
    _s: Scratch,
    host: PluginHost,
    page: Rc<Page>,
    port: Rc<FakePort>,
    clock: Rc<GestureClock>,
}

fn agents_plugin(s: &Scratch, capabilities: &str) -> Rc<crate::plugin::load::LoadedPlugin> {
    let (dir, _) = demo_plugin(&s.0);
    std::fs::write(
        dir.join("eitri-plugin.toml"),
        format!(
            "id = \"face\"\ntitle = \"Face\"\nprotocol = 1\n\
             site = \"build/site\"\nentry = \"index.html\"\ncapabilities = [{capabilities}]\n"
        ),
    )
    .unwrap();
    Rc::new(load_plugin(dir.to_str().unwrap(), &BTreeMap::new(), None).unwrap())
}

fn rig_on(feed: Rc<AgentsFeed>, wake: agent::UiWake) -> Rig {
    let s = Scratch::new("host-agents");
    let plugin = agents_plugin(&s, "\"agents\"");
    let page = Rc::new(Page::default());
    let host = PluginHost::new(plugin, page.clone(), wake, None, vec![]);
    let port = Rc::new(FakePort::default());
    let clock = Rc::new(GestureClock::default());
    host.set_agents(Some(AgentsServices {
        feed,
        port: port.clone(),
        gesture: clock.clone(),
    }));
    host.set_on_screen(true, Instant::now());
    Rig {
        _s: s,
        host,
        page,
        port,
        clock,
    }
}

fn feed() -> (Rc<AgentsFeed>, Rc<SourceLog>) {
    let log = Rc::new(SourceLog::default());
    (AgentsFeed::new(Box::new(FakeSource(log.clone()))), log)
}

fn rig() -> (Rig, Rc<SourceLog>) {
    let (feed, log) = feed();
    (rig_on(feed, Arc::new(|| {})), log)
}

impl Rig {
    fn send_at(&self, now: Instant, id: u64, verb: &str, args: Value) {
        let text = json!({"v": 1, "id": id, "verb": verb, "args": args}).to_string();
        self.host.page_message(&text, now);
    }

    fn hello(&self) {
        self.send_at(Instant::now(), 1, "host.hello", json!({}));
        self.page.take();
    }

    /// Sends one verb, pumps, and returns everything the page got.
    fn ask_at(&self, now: Instant, id: u64, verb: &str, args: Value) -> Vec<Value> {
        self.send_at(now, id, verb, args);
        self.host.pump(now);
        self.page.take()
    }

    /// The one reply to one verb.
    fn answer_at(&self, now: Instant, id: u64, verb: &str, args: Value) -> Value {
        let sent = self.ask_at(now, id, verb, args);
        let replies = replies(&sent);
        assert_eq!(replies.len(), 1, "{sent:?}");
        replies[0].clone()
    }

    /// Hello, a watch answered with `view`, and a key or click just now with the module holding the keys.
    fn watching(&self, log: &SourceLog, view: View) -> Instant {
        self.hello();
        self.send_at(Instant::now(), 2, "agents.watch", json!({}));
        log.emit(Change::Reset { view });
        self.host.pump(Instant::now());
        let sent = self.page.take();
        assert_eq!(replies(&sent).len(), 1, "{sent:?}");
        self.host.set_has_keys(true);
        self.page.take();
        let now = Instant::now();
        self.clock.record(now);
        now
    }
}

fn row(instance: &str, tab: u64, key: &str, class: Class, waiting_since: Option<u64>) -> Row {
    Row {
        id: format!("{instance}.{tab}"),
        key: key.to_owned(),
        agent: "claude".to_owned(),
        project: ProjectRef {
            key: "9b2c4e7a10f3d856".to_owned(),
            name: "eitri".to_owned(),
        },
        window: WindowRef {
            instance: instance.to_owned(),
            name: "1".to_owned(),
            focused: false,
        },
        number: tab as u16,
        label: format!("tab {tab}"),
        state: if class == Class::Needs { "card" } else { "idle" }.to_owned(),
        class,
        since: 10,
        needs: Needs {
            cards: usize::from(class == Class::Needs),
            ..Needs::default()
        },
        waiting_since,
        ..Row::default()
    }
}

const A: &str = "00000000000000a1";
const B: &str = "00000000000000b2";

fn two_waiting() -> View {
    View {
        rows: vec![
            row(A, 1, "k-000000000000000a", Class::Needs, Some(200)),
            row(A, 2, "k-000000000000000b", Class::Quiet, None),
            row(B, 3, "k-000000000000000c", Class::Needs, Some(100)),
        ],
        ..View::default()
    }
}

#[test]
fn watch_answers_the_view_before_any_event() {
    let (r, log) = rig();
    r.hello();
    let now = Instant::now();
    r.send_at(now, 2, "agents.watch", json!({}));
    assert!(r.page.take().is_empty(), "no view is read yet: the answer waits");
    log.emit(Change::Reset { view: two_waiting() });
    let added = row(B, 4, "k-000000000000000d", Class::Quiet, None);
    log.emit(Change::Row { row: added.clone() });
    r.host.pump(now);
    let sent = r.page.take();
    assert_eq!(
        sent.len(),
        1,
        "the answer, and nothing it already holds as an event: {sent:?}"
    );
    assert_eq!(sent[0]["re"], 2);
    let ids: Vec<&str> = sent[0]["result"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&added.id.as_str()), "{ids:?}");
    assert_eq!(sent[0]["result"]["v"], SCHEMA_V);
    log.emit(Change::Gone {
        id: added.id.clone(),
        key: added.key.clone(),
        why: crate::agents::GoneWhy::Closed,
    });
    r.host.pump(now);
    assert_eq!(agents_events(&r.page.take()), ["agents.gone"]);
    // Asked again, the watch answers at once with the view as it is now.
    let again = r.answer_at(now, 3, "agents.watch", json!({}));
    assert_eq!(again["result"]["rows"].as_array().unwrap().len(), 3);
}

#[test]
fn no_event_before_hello_and_watch() {
    let (r, log) = rig();
    // Hello but no watch: the feed is not even read.
    r.hello();
    assert_eq!(log.starts.get(), 0);
    // A watch before hello in a new document: answered, but no event until hello.
    r.host.page_committed();
    r.page.take();
    let now = Instant::now();
    r.send_at(now, 2, "agents.watch", json!({}));
    log.emit(Change::Reset { view: two_waiting() });
    r.host.pump(now);
    let sent = r.page.take();
    assert_eq!(replies(&sent).len(), 1);
    log.emit(Change::Row {
        row: row(A, 5, "k-000000000000000e", Class::Quiet, None),
    });
    r.host.pump(now);
    assert!(agents_events(&r.page.take()).is_empty(), "no hello yet");
    r.hello();
    r.host.pump(now);
    assert_eq!(
        agents_events(&r.page.take()),
        ["agents.reset"],
        "what it missed before hello comes as one reset"
    );
    log.emit(Change::Row {
        row: row(A, 6, "k-000000000000000f", Class::Quiet, None),
    });
    r.host.pump(now);
    assert_eq!(agents_events(&r.page.take()), ["agents.row"]);
}

#[test]
fn no_event_while_off_screen_and_reset_on_return() {
    let (r, log) = rig();
    r.watching(&log, two_waiting());
    let now = Instant::now();
    r.host.set_on_screen(false, now);
    log.emit(Change::Row {
        row: row(A, 5, "k-000000000000000e", Class::Quiet, None),
    });
    log.emit(Change::Selection {
        selection: Some(Selection {
            key: "k-000000000000000e".into(),
            by: A.into(),
            at_ms: 1,
        }),
    });
    r.host.pump(now);
    assert!(agents_events(&r.page.take()).is_empty());
    r.host.set_on_screen(true, now);
    r.host.pump(now);
    let sent = r.page.take();
    assert_eq!(agents_events(&sent), ["agents.reset"]);
    let reset = sent.iter().find(|m| m["event"] == "agents.reset").unwrap();
    assert_eq!(reset["data"]["rows"].as_array().unwrap().len(), 4);
    assert_eq!(reset["data"]["selection"]["key"], "k-000000000000000e");
    // Back on screen with nothing missed, it still starts from a whole view.
    r.host.set_on_screen(false, now);
    r.host.set_on_screen(true, now);
    r.host.pump(now);
    assert_eq!(agents_events(&r.page.take()), ["agents.reset"]);
}

#[test]
fn focus_without_a_recent_gesture_is_refused() {
    let (r, log) = rig();
    let at = r.watching(&log, two_waiting());
    let args = json!({"id": format!("{A}.1"), "card_first": true});
    let late = r.answer_at(at + Duration::from_millis(GESTURE_MS), 10, "agents.focus", args.clone());
    assert_eq!(late["error"]["code"], "no_gesture");
    assert!(r.port.take().is_empty(), "nothing was asked of any window");
    let fresh = r.answer_at(at + Duration::from_millis(GESTURE_MS - 1), 11, "agents.focus", args);
    assert_eq!(fresh["ok"], true, "{fresh}");
    assert_eq!(r.port.take(), [format!("focus {A} Some(1) card_first=true Show")]);
    // No key or click ever: refused.
    let (other, log) = rig();
    other.watching(&log, two_waiting());
    other.clock.0.set(None);
    let none = other.answer_at(Instant::now(), 12, "agents.focus", json!({"id": format!("{A}.1")}));
    assert_eq!(none["error"]["code"], "no_gesture");
}

#[test]
fn focus_without_the_keys_is_refused() {
    let (r, log) = rig();
    let at = r.watching(&log, two_waiting());
    r.host.set_has_keys(false);
    r.page.take();
    let every = [
        ("agents.focus", json!({"id": format!("{A}.1")})),
        ("agents.new", json!({"project": "9b2c4e7a10f3d856"})),
        ("agents.select", json!({"key": "k-000000000000000a"})),
        ("agents.round", json!({"op": "start"})),
        ("agents.stop", json!({"id": format!("{A}.1")})),
    ];
    for (index, (verb, args)) in every.into_iter().enumerate() {
        let reply = r.answer_at(at, 20 + index as u64, verb, args);
        assert_eq!(reply["error"]["code"], "no_gesture", "{verb}: {reply}");
    }
    assert!(r.port.take().is_empty());
    // The watch needs no gesture.
    let watch = r.answer_at(at, 30, "agents.watch", json!({}));
    assert_eq!(watch["ok"], true);
}

#[test]
fn new_twice_in_a_second_is_refused() {
    let (r, log) = rig();
    let at = r.watching(&log, two_waiting());
    let args = json!({"project": "9b2c4e7a10f3d856"});
    let first = r.answer_at(at, 10, "agents.new", args.clone());
    assert_eq!(first["ok"], true, "{first}");
    assert_eq!(r.port.take(), ["new 9b2c4e7a10f3d856"]);
    let soon = at + Duration::from_millis(NEW_MIN_INTERVAL_MS - 1);
    r.clock.record(soon);
    let second = r.answer_at(soon, 11, "agents.new", args.clone());
    assert_eq!(second["error"]["code"], "refused", "{second}");
    assert!(r.port.take().is_empty());
    let later = at + Duration::from_millis(NEW_MIN_INTERVAL_MS);
    r.clock.record(later);
    let third = r.answer_at(later, 12, "agents.new", args);
    assert_eq!(third["ok"], true, "{third}");
    assert_eq!(r.port.take(), ["new 9b2c4e7a10f3d856"]);
    // A project with no running window.
    let later = later + Duration::from_millis(NEW_MIN_INTERVAL_MS);
    r.clock.record(later);
    let none = r.answer_at(later, 13, "agents.new", json!({"project": "0000000000000000"}));
    assert_eq!(none["error"]["code"], "no_window");
    assert!(r.port.take().is_empty());
    // A refusal is not a new agent: the next one in the same second is taken.
    let again = r.answer_at(later, 14, "agents.new", json!({"project": "9b2c4e7a10f3d856"}));
    assert_eq!(again["ok"], true, "{again}");
    assert_eq!(r.port.take(), ["new 9b2c4e7a10f3d856"]);
}

#[test]
fn new_reaches_a_project_whose_windows_hold_no_tab() {
    let (r, log) = rig();
    let hub_only = ProjectRef {
        key: "0123456789abcdef".to_owned(),
        name: "another".to_owned(),
    };
    let mut view = two_waiting();
    view.projects = vec![two_waiting().rows[0].project.clone(), hub_only.clone()];
    let at = r.watching(&log, view);
    let reply = r.answer_at(at, 15, "agents.new", json!({"project": hub_only.key}));
    assert_eq!(
        reply["ok"], true,
        "a project with a running window but no tab is a target: {reply}"
    );
    assert_eq!(r.port.take(), [format!("new {}", hub_only.key)]);
}

#[test]
fn disabled_layer_answers_disabled() {
    let (r, _log) = rig();
    r.host.set_agents(None);
    r.hello();
    r.host.set_has_keys(true);
    r.clock.record(Instant::now());
    for (index, (verb, args)) in [
        ("agents.watch", json!({})),
        ("agents.focus", json!({"id": format!("{A}.1")})),
        ("agents.new", json!({"project": "9b2c4e7a10f3d856"})),
        ("agents.select", json!({"key": "k-000000000000000a"})),
        ("agents.round", json!({"op": "leave"})),
        ("agents.stop", json!({"id": format!("{A}.1")})),
    ]
    .into_iter()
    .enumerate()
    {
        let reply = r.answer_at(Instant::now(), 40 + index as u64, verb, args);
        assert_eq!(reply["error"]["code"], "disabled", "{verb}: {reply}");
    }
    assert!(r.port.take().is_empty());
}

#[test]
fn a_gone_row_answers_gone() {
    let (r, log) = rig();
    let at = r.watching(&log, two_waiting());
    for (id, verb, args) in [
        (50, "agents.focus", json!({"id": format!("{A}.9")})),
        (51, "agents.stop", json!({"id": "not-an-id"})),
        (52, "agents.select", json!({"key": "k-ffffffffffffffff"})),
        (53, "agents.round", json!({"op": "start", "key": "k-fffffffffffffff0"})),
    ] {
        let reply = r.answer_at(at, id, verb, args);
        assert_eq!(reply["error"]["code"], "gone", "{verb}: {reply}");
    }
    assert!(r.port.take().is_empty());
    // The window itself says the tab went: the same code.
    *r.port.answer.borrow_mut() = Some(AgentsRefusal::Gone);
    let reply = r.answer_at(at, 54, "agents.focus", json!({"id": format!("{B}.3")}));
    assert_eq!(reply["error"]["code"], "gone");
    *r.port.answer.borrow_mut() = Some(AgentsRefusal::Refused("moving".into()));
    let reply = r.answer_at(at, 55, "agents.focus", json!({"id": format!("{B}.3")}));
    assert_eq!(reply["error"]["code"], "refused");
    assert_eq!(reply["error"]["message"], "moving");
}

#[test]
fn one_reader_per_process_for_two_watching_pages() {
    let (feed, log) = feed();
    let first = rig_on(feed.clone(), Arc::new(|| {}));
    let second = rig_on(feed.clone(), Arc::new(|| {}));
    first.watching(&log, two_waiting());
    second.hello();
    let reply = second.answer_at(Instant::now(), 2, "agents.watch", json!({}));
    assert_eq!(
        reply["ok"], true,
        "the second page is answered from the reader already running"
    );
    assert_eq!(log.starts.get(), 1);
    log.emit(Change::Row {
        row: row(A, 7, "k-0000000000000007", Class::Quiet, None),
    });
    first.host.pump(Instant::now());
    second.host.pump(Instant::now());
    assert_eq!(agents_events(&first.page.take()), ["agents.row"]);
    assert_eq!(agents_events(&second.page.take()), ["agents.row"]);
    // One document goes: the reader stays for the other; the last one stops it.
    first.host.page_committed();
    assert_eq!(log.stops.get(), 0);
    assert!(feed.reading());
    second.host.stop(Instant::now());
    assert_eq!(log.stops.get(), 1);
    assert!(!feed.reading());
    // A later watch starts a reader again, which reads the view afresh.
    second.hello();
    second.send_at(Instant::now(), 3, "agents.watch", json!({}));
    assert_eq!(log.starts.get(), 2);
    assert!(second.page.take().is_empty(), "the old view is not answered from");
}

#[test]
fn not_declared_without_the_capability() {
    let s = Scratch::new("host-agents-undeclared");
    let plugin = agents_plugin(&s, "");
    let page = Rc::new(Page::default());
    let host = PluginHost::new(plugin, page.clone(), Arc::new(|| {}), None, vec![]);
    let (feed, log) = feed();
    let clock = Rc::new(GestureClock::default());
    host.set_agents(Some(AgentsServices {
        feed,
        port: Rc::new(FakePort::default()),
        gesture: clock.clone(),
    }));
    host.set_has_keys(true);
    clock.record(Instant::now());
    for (index, (verb, args)) in [
        ("agents.watch", json!({})),
        ("agents.focus", json!({"id": format!("{A}.1")})),
        ("agents.round", json!({"op": "next"})),
    ]
    .into_iter()
    .enumerate()
    {
        let text = json!({"v": 1, "id": index, "verb": verb, "args": args}).to_string();
        host.page_message(&text, Instant::now());
        let sent = page.take();
        let reply = sent.iter().find(|m| m.get("re").is_some()).unwrap();
        assert_eq!(reply["error"]["code"], "not_declared", "{verb}");
    }
    assert_eq!(log.starts.get(), 0, "no reader for a page that may not watch");
}

#[test]
fn round_start_at_a_row_that_does_not_wait_is_refused() {
    let (r, log) = rig();
    let at = r.watching(&log, two_waiting());
    let reply = r.answer_at(
        at,
        56,
        "agents.round",
        json!({"op": "start", "key": "k-000000000000000b"}),
    );
    assert_eq!(
        reply["error"]["code"], "refused",
        "the row is there, it does not wait: {reply}"
    );
    assert_eq!(reply["error"]["message"], "that agent does not wait for you");
    assert!(r.port.take().is_empty());
}

#[test]
fn a_stale_shared_write_does_not_replace_a_newer_one() {
    let control = Scratch::new("host-agents-order");
    std::fs::set_permissions(&control.0, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let order = WriteOrder::default();
    let pick = |key: &str| {
        SharedWrite::Selection(Selection {
            key: key.to_owned(),
            by: A.to_owned(),
            at_ms: 1,
        })
    };
    let older = order.ticket();
    let newer = order.ticket();
    // The later request's thread happens to write first.
    order.write(newer, &control.0, &pick("k-000000000000000c")).unwrap();
    order.write(older, &control.0, &pick("k-000000000000000a")).unwrap();
    let stored = SharedStore::open(&control.0).unwrap().read_selection().unwrap();
    assert_eq!(
        stored.key, "k-000000000000000c",
        "the older request does not overwrite the newer"
    );
    // A round is its own file: a newer selection does not hold an older round back.
    let round = order.ticket();
    let selection = order.ticket();
    order.write(selection, &control.0, &pick("k-000000000000000a")).unwrap();
    order.write(round, &control.0, &SharedWrite::Round(None)).unwrap();
    let stored = SharedStore::open(&control.0).unwrap().read_selection().unwrap();
    assert_eq!(stored.key, "k-000000000000000a");
}

#[test]
fn select_writes_the_shared_selection() {
    let (feed, log) = feed();
    let (woke_tx, woke) = std::sync::mpsc::channel::<()>();
    let woke_tx = Mutex::new(woke_tx);
    let r = rig_on(
        feed,
        Arc::new(move || {
            let _ = woke_tx.lock().unwrap().send(());
        }),
    );
    let control = Scratch::new("host-agents-shared");
    std::fs::set_permissions(&control.0, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    *r.port.control_dir.borrow_mut() = Some(control.0.clone());
    let at = r.watching(&log, two_waiting());
    while woke.try_recv().is_ok() {}
    r.send_at(at, 60, "agents.select", json!({"key": "k-000000000000000c"}));
    woke.recv_timeout(Duration::from_secs(10)).expect("the write finished");
    r.host.pump(at);
    let sent = r.page.take();
    let reply = replies(&sent)[0];
    assert_eq!(reply["ok"], true, "{reply}");
    let stored = SharedStore::open(&control.0).unwrap().read_selection().unwrap();
    assert_eq!(stored.key, "k-000000000000000c");
    assert_eq!(stored.by, A);
}

#[test]
fn round_start_at_a_key_focuses_that_stop() {
    let (r, log) = rig();
    let at = r.watching(&log, two_waiting());
    let reply = r.answer_at(
        at,
        70,
        "agents.round",
        json!({"op": "start", "key": "k-000000000000000a"}),
    );
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(
        r.port.take(),
        [
            "round Some(Some(\"k-000000000000000a\"))".to_owned(),
            format!("focus {A} Some(1) card_first=true Show"),
        ]
    );
    // With no key, the oldest wait first.
    let reply = r.answer_at(at, 71, "agents.round", json!({"op": "start"}));
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(
        r.port.take(),
        [
            "round Some(Some(\"k-000000000000000c\"))".to_owned(),
            format!("focus {B} Some(3) card_first=true Show"),
        ]
    );
}

#[test]
fn round_moves_a_running_round_and_next_advances() {
    let (r, log) = rig();
    let running = Round {
        from: RoundPoint {
            instance: A.into(),
            module: "agent".into(),
            tab: Some(2),
        },
        current: Some("k-000000000000000c".into()),
        visited: vec!["k-000000000000000c".into()],
        started_at_ms: wall_clock_ms(),
        at_ms: wall_clock_ms(),
    };
    let at = r.watching(
        &log,
        View {
            round: Some(running.clone()),
            ..two_waiting()
        },
    );
    let reply = r.answer_at(at, 72, "agents.round", json!({"op": "next"}));
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(
        r.port.take(),
        [
            "round Some(Some(\"k-000000000000000a\"))".to_owned(),
            format!("focus {A} Some(1) card_first=true Show"),
        ]
    );
    log.emit(Change::Round {
        round: Some(Round {
            current: Some("k-000000000000000a".into()),
            visited: vec!["k-000000000000000c".into(), "k-000000000000000a".into()],
            ..running.clone()
        }),
    });
    r.host.pump(at);
    assert_eq!(agents_events(&r.page.take()), ["agents.round"]);
    // Every stop visited: next ends the round where it is, focusing nothing, and keeps where it started so the
    // round's back key still goes there.
    let reply = r.answer_at(at, 73, "agents.round", json!({"op": "next"}));
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(r.port.take(), ["round Some(None)"]);
    let ended = r
        .port
        .rounds
        .borrow()
        .last()
        .cloned()
        .flatten()
        .expect("the ended round is kept");
    assert_eq!(ended.from, running.from);
    assert_eq!(ended.current, None);
}

#[test]
fn a_round_that_ended_or_stood_idle_starts_fresh_from_here() {
    let ended = Round {
        from: RoundPoint {
            instance: B.into(),
            module: "agent".into(),
            tab: Some(4),
        },
        current: None,
        visited: vec!["k-000000000000000c".into(), "k-000000000000000a".into()],
        started_at_ms: wall_clock_ms(),
        at_ms: wall_clock_ms(),
    };
    let idle = Round {
        current: Some("k-000000000000000c".into()),
        started_at_ms: 1,
        at_ms: 1,
        ..ended.clone()
    };
    for (op, key) in [("start", None), ("start", Some("k-000000000000000a")), ("next", None)] {
        for old in [ended.clone(), idle.clone()] {
            let (r, log) = rig();
            let at = r.watching(
                &log,
                View {
                    round: Some(old.clone()),
                    ..two_waiting()
                },
            );
            let args = match key {
                Some(key) => json!({"op": op, "key": key}),
                None => json!({"op": op}),
            };
            let reply = r.answer_at(at, 74, "agents.round", args);
            assert_eq!(reply["ok"], true, "{op} {key:?} over {old:?}: {reply}");
            let started = r.port.rounds.borrow()[0].clone().expect("a round was written");
            let first = key.unwrap_or("k-000000000000000c");
            assert_eq!(started.current.as_deref(), Some(first), "{op} {key:?} over {old:?}");
            assert_eq!(
                started.visited,
                [first],
                "a fresh round has visited only its first stop"
            );
            assert_eq!(started.from.instance, A, "it goes back to where this one started");
            assert_eq!(started.from.module, ModuleId::lua("face").as_str());
        }
    }
}

#[test]
fn a_start_with_no_key_goes_on_when_the_current_stops_process_went() {
    let (r, log) = rig();
    let gone = Round {
        from: RoundPoint {
            instance: B.into(),
            module: "agent".into(),
            tab: Some(4),
        },
        current: Some("k-00000000000000ff".into()),
        visited: vec!["k-00000000000000ff".into()],
        started_at_ms: wall_clock_ms(),
        at_ms: wall_clock_ms(),
    };
    let at = r.watching(
        &log,
        View {
            round: Some(gone.clone()),
            ..two_waiting()
        },
    );
    let reply = r.answer_at(at, 75, "agents.round", json!({"op": "start"}));
    assert_eq!(reply["ok"], true, "{reply}");
    let moved = r.port.rounds.borrow()[0].clone().expect("a round was written");
    assert_eq!(moved.current.as_deref(), Some("k-000000000000000c"));
    assert_eq!(moved.from, gone.from, "the round keeps where it started");
}

#[test]
fn round_leave_focuses_from() {
    let (r, log) = rig();
    let running = Round {
        from: RoundPoint {
            instance: B.into(),
            module: "agent".into(),
            tab: Some(4),
        },
        current: Some("k-000000000000000a".into()),
        visited: vec!["k-000000000000000a".into()],
        started_at_ms: wall_clock_ms(),
        at_ms: wall_clock_ms(),
    };
    let at = r.watching(
        &log,
        View {
            round: Some(running),
            ..two_waiting()
        },
    );
    let reply = r.answer_at(at, 80, "agents.round", json!({"op": "leave"}));
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(
        r.port.take(),
        [
            "round None".to_owned(),
            format!("focus {B} Some(4) card_first=false Show")
        ]
    );
    // A start point that is gone says so.
    *r.port.answer.borrow_mut() = Some(AgentsRefusal::Gone);
    let reply = r.answer_at(at, 81, "agents.round", json!({"op": "leave"}));
    assert_eq!(reply["error"]["code"], "gone", "{reply}");
    // A round idle too long is only removed.
    let (r, log) = rig();
    let idle = Round {
        from: RoundPoint {
            instance: B.into(),
            module: "agent".into(),
            tab: Some(4),
        },
        current: Some("k-000000000000000a".into()),
        visited: vec!["k-000000000000000a".into()],
        started_at_ms: 1,
        at_ms: 1,
    };
    let at = r.watching(
        &log,
        View {
            round: Some(idle),
            ..two_waiting()
        },
    );
    let reply = r.answer_at(at, 84, "agents.round", json!({"op": "leave"}));
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(r.port.take(), ["round None"]);
}

#[test]
fn round_leave_in_its_own_window_focuses_the_module_it_started_from() {
    for rowless in [false, true] {
        let (r, log) = rig();
        r.port.rowless.set(rowless);
        let at = r.watching(&log, two_waiting());
        let reply = r.answer_at(at, 82, "agents.round", json!({"op": "start"}));
        assert_eq!(reply["ok"], true, "{reply}");
        let started = r.port.rounds.borrow()[0].clone().expect("a round was written");
        let here = if rowless { PAGE_WINDOW } else { A };
        assert_eq!(started.from.instance, here, "a round records the window it started in");
        assert_eq!(started.from.module, ModuleId::lua("face").as_str());
        assert_eq!(started.from.tab, None);
        r.port.take();
        log.emit(Change::Round { round: Some(started) });
        r.host.pump(at);
        r.page.take();
        let reply = r.answer_at(at, 83, "agents.round", json!({"op": "leave"}));
        assert_eq!(reply["ok"], true, "{reply}");
        assert_eq!(
            r.port.take(),
            [
                "round None".to_owned(),
                format!("module {}", ModuleId::lua("face").as_str())
            ],
            "the keys go back to the page, not to whatever this window focused last"
        );
    }
}

#[test]
fn a_failed_reader_leaves_no_frozen_view_and_a_watch_restarts_it() {
    let (feed, log) = feed();
    let first = rig_on(feed.clone(), Arc::new(|| {}));
    let second = rig_on(feed.clone(), Arc::new(|| {}));
    first.watching(&log, two_waiting());
    log.fail("the directory went");
    first.host.pump(Instant::now());
    assert!(agents_events(&first.page.take()).is_empty());
    assert_eq!(
        log.starts.get(),
        2,
        "a reader that failed after it gave a view is started again"
    );
    // A later watch is not answered from the view the dead reader left.
    second.hello();
    second.send_at(Instant::now(), 2, "agents.watch", json!({}));
    assert!(second.page.take().is_empty(), "no answer from a dead reader's view");
    assert_eq!(log.starts.get(), 2, "one new reader for both");
    let mut fresh = two_waiting();
    fresh.rows.truncate(1);
    log.emit(Change::Reset { view: fresh });
    second.host.pump(Instant::now());
    let sent = second.page.take();
    assert_eq!(replies(&sent).len(), 1, "{sent:?}");
    assert_eq!(sent[0]["result"]["rows"].as_array().unwrap().len(), 1);
    // The page that watched all along hears the new reader's whole view.
    first.host.pump(Instant::now());
    let heard = first.page.take();
    assert_eq!(agents_events(&heard), ["agents.reset"]);
    // A watch starts a failed reader again; one that fails before any view answers the watch `io`, and is not
    // started again until a page asks.
    log.fail("the directory went");
    let third = rig_on(feed.clone(), Arc::new(|| {}));
    third.hello();
    third.send_at(Instant::now(), 2, "agents.watch", json!({}));
    assert_eq!(log.starts.get(), 3);
    log.fail("could not watch");
    third.host.pump(Instant::now());
    first.host.pump(Instant::now());
    let sent = third.page.take();
    assert_eq!(replies(&sent)[0]["error"]["code"], "io", "{sent:?}");
    assert_eq!(log.starts.get(), 3, "no retry in a loop");
}

#[test]
fn the_watch_snapshot_and_its_place_move_together() {
    let (feed, log) = feed();
    let id = feed.watch(Arc::new(|| {}));
    log.emit(Change::Reset { view: two_waiting() });
    let view = feed.take_view(id).expect("a view");
    assert_eq!(view.rows.len(), 3);
    let added = row(B, 4, "k-000000000000000d", Class::Quiet, None);
    log.emit(Change::Row { row: added.clone() });
    assert_eq!(
        feed.read(id),
        FeedRead::Changes(vec![Change::Row { row: added }]),
        "a change after the snapshot is read as a change"
    );
}

#[test]
fn stop_focuses_with_the_stop_intent() {
    let (r, log) = rig();
    let at = r.watching(&log, two_waiting());
    let reply = r.answer_at(at, 90, "agents.stop", json!({"id": format!("{A}.2")}));
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(r.port.take(), [format!("focus {A} Some(2) card_first=false Stop")]);
    *r.port.answer.borrow_mut() = Some(AgentsRefusal::Refused("idle".into()));
    let reply = r.answer_at(at, 91, "agents.stop", json!({"id": format!("{A}.2")}));
    assert_eq!(reply["error"]["code"], "refused");
    assert_eq!(reply["error"]["message"], "idle");
}

#[test]
fn an_answer_for_a_replaced_document_is_dropped() {
    struct Later(RefCell<Vec<AgentsDone>>);
    impl AgentsPort for Later {
        fn own_instance(&self) -> Option<String> {
            None
        }
        fn origin(&self) -> String {
            PAGE_WINDOW.to_owned()
        }
        fn is_here(&self, instance: &str) -> bool {
            instance == PAGE_WINDOW
        }
        fn focus(&self, _: &str, _: Option<u64>, _: bool, _: Intent, done: AgentsDone) {
            self.0.borrow_mut().push(done);
        }
        fn focus_module(&self, _: &str, done: AgentsDone) {
            self.0.borrow_mut().push(done);
        }
        fn new_in(&self, _: &str, done: AgentsDone) {
            self.0.borrow_mut().push(done);
        }
        fn write_selection(&self, _: Selection, done: AgentsDone) {
            self.0.borrow_mut().push(done);
        }
        fn write_round(&self, _: Option<Round>, done: AgentsDone) {
            self.0.borrow_mut().push(done);
        }
    }
    let (r, log) = rig();
    let later = Rc::new(Later(RefCell::new(Vec::new())));
    let feed = AgentsFeed::new(Box::new(FakeSource(log.clone())));
    r.host.set_agents(Some(AgentsServices {
        feed,
        port: later.clone(),
        gesture: r.clock.clone(),
    }));
    let at = r.watching(&log, two_waiting());
    r.send_at(at, 95, "agents.focus", json!({"id": format!("{A}.1")}));
    assert!(r.page.take().is_empty(), "answered when the window answers");
    // Past the deadline with no answer: the page is told.
    r.host.pump(at + AGENTS_DEADLINE);
    let sent = r.page.take();
    assert_eq!(replies(&sent)[0]["error"]["code"], "busy");
    // An answer for a document that was replaced reaches nobody.
    r.send_at(at, 96, "agents.focus", json!({"id": format!("{A}.1")}));
    r.host.page_committed();
    r.page.take();
    for done in later.0.borrow_mut().drain(..) {
        done(Ok(()));
    }
    r.host.pump(at);
    assert!(replies(&r.page.take()).is_empty());
}

#[test]
fn the_guide_example_plugin_loads_and_counts_waiting() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugins/waiting");
    let plugin = Rc::new(load_plugin(dir.to_str().unwrap(), &BTreeMap::new(), None).expect("the example loads"));
    assert_eq!(
        plugin.manifest.capabilities,
        vec![crate::plugin::manifest::Capability::Agents]
    );
    // The script calls only what the host serves, and listens only for what it sends.
    let script = std::fs::read_to_string(dir.join("site/main.js")).unwrap();
    let named = |prefix: &str| -> Vec<String> {
        script
            .match_indices(prefix)
            .map(|(at, p)| {
                let rest = &script[at + p.len()..];
                rest[..rest.find('"').unwrap()].to_owned()
            })
            .collect()
    };
    assert_eq!(
        named("eitriPlugin.call(\""),
        ["agents.focus", "host.hello", "agents.watch"]
    );
    assert_eq!(
        named("eitriPlugin.on(\""),
        [EVENT_AGENTS_ROW, EVENT_AGENTS_GONE, EVENT_AGENTS_RESET]
    );
    // Served: hello, the watch's view (whose class `needs` rows the page counts), and the click's focus on the
    // oldest wait with its card first.
    let page = Rc::new(Page::default());
    let host = PluginHost::new(plugin, page.clone(), Arc::new(|| {}), None, vec![]);
    let (feed, log) = feed();
    let port = Rc::new(FakePort::default());
    let clock = Rc::new(GestureClock::default());
    host.set_agents(Some(AgentsServices {
        feed,
        port: port.clone(),
        gesture: clock.clone(),
    }));
    host.set_on_screen(true, Instant::now());
    let r = Rig {
        _s: Scratch::new("host-agents-example"),
        host,
        page,
        port,
        clock,
    };
    r.watching(&log, two_waiting());
    let watch = r.answer_at(Instant::now(), 3, "agents.watch", json!({}));
    let waiting: Vec<&Value> = watch["result"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["class"] == "needs")
        .collect();
    assert_eq!(waiting.len(), 2);
    let oldest = waiting
        .iter()
        .min_by_key(|row| row["waiting_since"].as_u64().unwrap())
        .unwrap();
    let at = Instant::now();
    r.clock.record(at);
    let reply = r.answer_at(at, 4, "agents.focus", json!({"id": oldest["id"], "card_first": true}));
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(r.port.take(), [format!("focus {B} Some(3) card_first=true Show")]);
}

#[test]
fn a_plugin_pages_view_never_satisfies_may_answer() {
    use crate::layout::ModuleId;
    use crate::tabs::ViewId;
    // An answer is allowed only from the chat view showing the tab, and a plugin page's module is no chat view:
    // there is no view a page could name.
    for page in ["face", "agents-strips", "agents-top", "agents-pass", "waiting"] {
        assert_eq!(ViewId::of_module(&ModuleId::lua(page)), None, "{page}");
    }
    assert!(ViewId::of_module(&ModuleId::agent()).is_some());
    // And nothing the host serves names a view: every agents verb takes only a row id, a key or a project.
    for fixture in [
        "req-agents-focus.json",
        "req-agents-new.json",
        "req-agents-select.json",
        "req-agents-round-start.json",
        "req-agents-stop.json",
    ] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/plugin/fixtures")
            .join(fixture);
        let request: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        for name in request["args"].as_object().unwrap().keys() {
            assert!(
                ["id", "card_first", "project", "key", "op"].contains(&name.as_str()),
                "{fixture}: {name}"
            );
        }
    }
}

fn a_round(current: Option<&str>) -> Round {
    Round {
        from: RoundPoint {
            instance: A.to_owned(),
            module: "agent".to_owned(),
            tab: None,
        },
        current: current.map(str::to_owned),
        visited: current.into_iter().map(str::to_owned).collect(),
        started_at_ms: 1,
        at_ms: 1,
    }
}

/// A clock reading a little after [`a_round`]'s last move.
const SOON_MS: u64 = 1_000;

#[test]
fn something_waits_while_a_row_needs_the_person_or_a_round_has_a_stop() {
    let something_waits = |view: &View| super::something_waits(view, SOON_MS);
    assert!(something_waits(&two_waiting()));
    let quiet = View {
        rows: vec![row(A, 2, "k-000000000000000b", Class::Quiet, None)],
        ..View::default()
    };
    assert!(!something_waits(&quiet));
    // A round at a stop runs even when its row was just answered and the next step has not been taken yet.
    let running = View {
        round: Some(a_round(Some("k-000000000000000b"))),
        ..quiet.clone()
    };
    assert!(something_waits(&running));
    // A round that ended (no current stop) is kept only so `prefix B` can go back; nothing runs.
    let ended = View {
        round: Some(a_round(None)),
        ..quiet
    };
    assert!(!something_waits(&ended));
}

#[test]
fn a_round_whose_stop_went_or_that_stood_idle_does_not_keep_the_strip_up() {
    let quiet = View {
        rows: vec![row(A, 2, "k-000000000000000b", Class::Quiet, None)],
        ..View::default()
    };
    // The current stop's process was killed: the reader dropped its row, the round file still names it.
    let orphaned = View {
        round: Some(a_round(Some("k-00000000000000ff"))),
        ..quiet.clone()
    };
    assert!(!something_waits(&orphaned, SOON_MS));
    let running = View {
        round: Some(a_round(Some("k-000000000000000b"))),
        ..quiet
    };
    let expires = 1 + ROUND_MAX_IDLE_MS;
    assert!(something_waits(&running, expires - 1));
    assert!(!something_waits(&running, expires), "idle too long: over");
}

#[test]
fn a_wait_show_rechecks_when_the_running_round_expires() {
    let (feed, log) = feed();
    let show = WaitShow::new(feed, Arc::new(|| {}));
    log.emit(Change::Reset {
        view: View {
            rows: vec![row(A, 2, "k-000000000000000b", Class::Quiet, None)],
            round: Some(a_round(Some("k-000000000000000b"))),
            ..View::default()
        },
    });
    assert_eq!(show.pump_at(SOON_MS), Some(true));
    let expires = 1 + ROUND_MAX_IDLE_MS;
    assert_eq!(show.recheck_in(SOON_MS), Some(Duration::from_millis(expires - SOON_MS)));
    assert_eq!(
        show.pump_at(expires),
        Some(false),
        "no file changed, and the round is over"
    );
    assert_eq!(show.recheck_in(expires), None);
}

#[test]
fn a_wait_show_says_show_and_hide_only_when_the_answer_changes() {
    let (feed, log) = feed();
    let woken = Arc::new(Mutex::new(0usize));
    let wake: agent::UiWake = {
        let woken = woken.clone();
        Arc::new(move || *woken.lock().unwrap() += 1)
    };
    let show = WaitShow::new(feed.clone(), wake);
    // It keeps the process's reader running, with no page watching.
    assert!(feed.reading());
    assert_eq!(show.pump_at(SOON_MS), None, "no view yet: nothing to say");

    log.emit(Change::Reset { view: two_waiting() });
    assert!(*woken.lock().unwrap() > 0, "a reader event wakes it");
    assert_eq!(show.pump_at(SOON_MS), Some(true));
    assert_eq!(show.pump_at(SOON_MS), None, "the same answer is not said twice");

    // The older card answered: one row still waits.
    log.emit(Change::Row {
        row: row(B, 3, "k-000000000000000c", Class::Quiet, None),
    });
    assert_eq!(show.pump_at(SOON_MS), None);
    log.emit(Change::Round {
        round: Some(a_round(Some("k-000000000000000a"))),
    });
    log.emit(Change::Row {
        row: row(A, 1, "k-000000000000000a", Class::Quiet, None),
    });
    assert_eq!(show.pump_at(SOON_MS), None, "a round at a stop still runs");
    log.emit(Change::Round {
        round: Some(a_round(None)),
    });
    assert_eq!(show.pump_at(SOON_MS), Some(false), "nothing waits and the round ended");

    drop(show);
    assert!(!feed.reading(), "the last watcher gone, the reader stops");
}

#[test]
fn a_quiet_first_view_says_hide() {
    let (feed, log) = feed();
    let show = WaitShow::new(feed, Arc::new(|| {}));
    log.emit(Change::Reset {
        view: View {
            rows: vec![row(A, 2, "k-000000000000000b", Class::Quiet, None)],
            ..View::default()
        },
    });
    assert_eq!(show.pump(), Some(false));
    assert_eq!(show.pump(), None);
}
